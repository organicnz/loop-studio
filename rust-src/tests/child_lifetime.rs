//! Supervised engine wrappers must not orphan the engine they launch.
//!
//! `spawn_server` used to call `Command::status()`, which consumes the `Child`
//! and hands back only an `ExitStatus`. The parent was left with no pid to
//! signal, so `kill -TERM serve-mlx` killed the wrapper and left
//! `mlx_lm.server` running: still holding its port, still resident with the
//! model's weights, and invisible to `lac ps`. The next start found the port
//! taken, drifted to 8082, and loaded a *second* copy of the model — on the
//! host this suite warns about at 32 GiB.
//!
//! Both wrappers now publish the child pid and forward SIGTERM/SIGINT to it.
//! These tests drive that against a stub engine, because the property only
//! exists at the process level: it cannot be observed from inside the unit
//! tests.
//!
//! No port is bound and no PortLock is taken — the stub never listens, so
//! this suite cannot contend with the router suites for the global lock.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A pid the kernel no longer has. Used only to assert that the *stub's* pid
/// is gone, so it must be one we actually observed.
fn is_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn sleep_ms(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

/// Whether a pid went away *on its own* within `timeout`.
///
/// Deliberately does not clean up: this answers "did the engine exit by
/// itself", and an earlier version killed the orphan on the way out and
/// returned whether the kill had worked — which made the test unable to fail.
/// Cleanup is [`force_kill`], and it runs after the assertion.
fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !is_alive(pid) {
            return true;
        }
        sleep_ms(50);
    }
    false
}

/// Reap a stub left behind by a failed assertion, escalating so the next run
/// does not trip over it. The stub is `sleep 600`, so without this a single
/// failure would hold the suite open for ten minutes.
fn force_kill(pid: u32) {
    for sig in ["-TERM", "-KILL"] {
        if !is_alive(pid) {
            return;
        }
        let _ = Command::new("kill")
            .args([sig, &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let give_up = Instant::now() + Duration::from_secs(2);
        while Instant::now() < give_up && is_alive(pid) {
            sleep_ms(50);
        }
    }
}

/// Wait for `path` to hold a pid, then return it.
fn wait_for_pid(path: &std::path::Path, timeout: Duration) -> Option<u32> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(pid) = text.trim().parse::<u32>() {
                return Some(pid);
            }
        }
        sleep_ms(50);
    }
    None
}

/// A stub engine that records its own pid and then holds, like a real one
/// that has loaded a model and is serving.
fn stub_engine(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).expect("stub bin dir");
    // 30s, not 600. The test signals within a second or two, so a long-lived
    // stub buys nothing — but if an assertion ever fails to reap it, the suite
    // then waits out the stub's whole lifetime before it can report. This
    // bounds the worst case without weakening what is being checked.
    let script = "#!/bin/sh\necho $$ > \"$STUB_PIDFILE\"\nexec sleep 30\n";
    let path = bin.join(name);
    std::fs::write(&path, script).expect("write stub");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod stub");
    }
    path
}

struct Harness {
    dir: std::path::PathBuf,
    pidfile: std::path::PathBuf,
    log: std::path::PathBuf,
    child: Child,
}

impl Harness {
    fn new(binary: &str, engine: &str, extra_env: &[(&str, &str)]) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "lac-test-child-{}-{engine}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("home")).expect("home");
        let _ = stub_engine(&dir, engine);
        let pidfile = dir.join("engine.pid");
        let log = dir.join("wrapper.log");
        let log_file = std::fs::File::create(&log).expect("wrapper log");
        let bin_dir = dir.join("bin");

        let mut command = Command::new(binary);
        command
            .env_clear()
            .env("PATH", format!("{}:/usr/bin:/bin", bin_dir.display()))
            .env("HOME", dir.join("home"))
            .env("STUB_PIDFILE", &pidfile)
            .env("MLX_PORT", "8080")
            .stdout(Stdio::null())
            // A file, not a pipe: the engine inherits this descriptor, and an
            // orphaned stub holding a pipe open is what made a failing
            // assertion block for as long as the stub lived.
            .stderr(Stdio::from(log_file));
        for (key, value) in extra_env {
            command.env(key, value);
        }
        let child = command.spawn().expect("wrapper spawns");
        Self {
            dir,
            pidfile,
            log,
            child,
        }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn engine_pid(&self) -> Option<u32> {
        wait_for_pid(&self.pidfile, Duration::from_secs(20))
    }

}

impl Drop for Harness {
    fn drop(&mut self) {
        // Never leave a `sleep 600` behind, whatever the assertions did.
        if let Ok(text) = std::fs::read_to_string(&self.pidfile) {
            if let Ok(pid) = text.trim().parse::<u32>() {
                force_kill(pid);
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Reap any stub recorded in `pidfile`. A wrapper that fails to forward leaves
/// the engine running with its own `sleep 600`, and nothing else in the suite
/// knows about it.
fn reap_strays(pidfile: &std::path::Path) {
    if let Ok(text) = std::fs::read_to_string(pidfile) {
        if let Ok(pid) = text.trim().parse::<u32>() {
            force_kill(pid);
        }
    }
}

/// Reap the wrapper, returning its exit code.
fn reap(mut child: Child) -> Option<i32> {
    child.wait().ok().and_then(|s| s.code())
}

#[test]
fn serve_mlx_termination_takes_the_engine_with_it() {
    let harness = Harness::new(
        env!("CARGO_BIN_EXE_serve-mlx"),
        "mlx_lm.server",
        &[("LAC_ALLOW_MLX", "1")],
    );
    let engine = harness
        .engine_pid()
        .expect("the stub engine must report a pid");
    assert!(
        is_alive(engine),
        "precondition: the engine must be running before we signal anything"
    );

    // SIGTERM to the wrapper only — not the process group, not the engine.
    let _ = Command::new("kill")
        .args(["-TERM", &harness.child.id().to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    if !wait_for_exit(engine, Duration::from_secs(20)) {
        force_kill(engine);
        panic!(
            "the engine outlived the wrapper: pid {engine} is still running, holding its port and \
             its weights with nothing supervising it\n--- wrapper output ---\n{}",
            harness.log()
        );
    }

    // And the port file must not outlive it either, or the router is told
    // about a lane that nothing is serving.
    let port_file = harness.dir.join("home/.lac/mlx.port");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && port_file.exists() {
        sleep_ms(50);
    }
    if port_file.exists() {
        let contents = std::fs::read_to_string(&port_file).ok();
        force_kill(engine);
        panic!(
            "~/.lac/mlx.port still advertises a lane that is gone: {contents:?}\n--- wrapper output ---\n{}",
            harness.log()
        );
    }
}

#[test]
fn serve_llama_termination_takes_the_engine_with_it() {
    let harness = Harness::new(env!("CARGO_BIN_EXE_serve-llama"), "llama-server", &[]);
    let engine = harness
        .engine_pid()
        .expect("the stub engine must report a pid");
    assert!(is_alive(engine), "precondition: the engine must be running");

    let _ = Command::new("kill")
        .args(["-TERM", &harness.child.id().to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    if !wait_for_exit(engine, Duration::from_secs(20)) {
        force_kill(engine);
        panic!(
            "the engine outlived the wrapper: pid {engine} is still running\n--- wrapper output ---\n{}",
            harness.log()
        );
    }
}

#[test]
fn a_signal_during_backoff_does_not_relaunch_the_engine() {
    // The retry loop sleeps up to 60s between launches. A `lac stop` landing
    // in that window used to be ignored, and the lane came back after the
    // operator had asked for it to be down.
    let dir = std::env::temp_dir().join(format!("lac-test-backoff-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("home")).expect("home");
    let _ = stub_engine(&dir, "mlx_lm.server");
    let log = dir.join("log");
    let log_file = std::fs::File::create(&log).expect("log file");
    // The stub needs to be told where to record its pid, or a wrapper that
    // fails to forward leaves a `sleep 600` nothing can find to reap — which is
    // how a failing run of this suite used to take ten minutes to report.
    let pidfile = dir.join("engine.pid");

    let child = Command::new(env!("CARGO_BIN_EXE_serve-mlx"))
        .env_clear()
        .env("PATH", format!("{}/bin:/usr/bin:/bin", dir.display()))
        .env("HOME", dir.join("home"))
        .env("STUB_PIDFILE", &pidfile)
        .env("LAC_ALLOW_MLX", "1")
        .env("LAUNCHDAEMON", "1")
        .env("MLX_PORT", "8080")
        .stdout(Stdio::null())
        .stderr(Stdio::from(log_file))
        .spawn()
        .expect("wrapper spawns");

    // Wait until it is at least one launch in, so we are inside the window.
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        if text.contains("mlx-lm launch") && text.contains("backing off") {
            break;
        }
        sleep_ms(50);
    }
    let before = std::fs::read_to_string(&log)
        .unwrap_or_default()
        .matches("mlx-lm launch")
        .count();
    assert!(before >= 1, "the wrapper never launched the engine: {before}");

    let _ = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let code = reap(child);
    let log_text = std::fs::read_to_string(&log).unwrap_or_default();

    // Long enough for a relaunch to have happened if one were coming.
    sleep_ms(1500);
    reap_strays(&pidfile);
    let text = std::fs::read_to_string(&log).unwrap_or_default();
    let after = text.matches("mlx-lm launch").count();
    let code_ok = code == Some(1);
    if after != before || !code_ok {
        let message = format!(
            "launches before={before} after={after} (a relaunch after a termination signal is \
             the bug), wrapper exit={code:?} (must be nonzero, because exit 0 is what \
             KeepAlive{{SuccessfulExit: false}} reads as 'do not restart')\n--- wrapper output ---\n{log_text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
        panic!("{message}");
    }

    let _ = std::fs::remove_dir_all(&dir);
}
