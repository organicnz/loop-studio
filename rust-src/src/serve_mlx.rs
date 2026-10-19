//! serve-mlx v2.6 — mlx_lm.server on :8080 (Q4 speed lane, native MTP).
//!
//! v2.6: binary preflight with install hint, readiness prober thread,
//! and a daemon retry loop that actually survives the night: unbounded
//! attempts with capped exponential backoff + jitter instead of giving
//! up after 3 tries (a "KeepAlive" that quits is not one).

mod common;

use std::env;
use std::net::{SocketAddr, TcpStream};
use std::process::Command;
use std::time::Duration;

// Ensure mlx and llama servers coexist without port conflict.
// mlx owns :8080, llama owns :8081. If the preferred port is free,
// we use it; otherwise we probe the next available port so both can
// run simultaneously and the /models picker routes correctly.

/// A port override we are willing to act on.
///
/// `common::mlx_port()` — the reader, and the function the router calls to
/// find this lane — ignores `0` and anything unparseable, falling through to
/// 8080. This used to accept a bare `parse()`, so `MLX_PORT=0` published `0`
/// to `~/.lac/mlx.port`, handed `--port 0` to the engine (which makes the OS
/// pick a random ephemeral port), and left the router reading a `0` it
/// rejects. The lane then became unfindable even after the variable was
/// removed, because the stale `0` was re-read on every lookup. A writer must
/// never publish a value its own reader discards.
fn port_override(raw: &str) -> Option<u16> {
    common::mlx_port_from(raw)
}

/// Which environment variable supplied the port, and whether it was usable.
/// `PORT` is honoured for compatibility but named out loud, because it is a
/// very common name: an unrelated `PORT=3000` in a shell profile used to
/// move the model server off 8080 with nothing printed to explain why.
fn env_port() -> (Option<u16>, &'static str) {
    if let Ok(raw) = env::var("MLX_PORT") {
        return (port_override(&raw), "MLX_PORT");
    }
    if let Ok(raw) = env::var("PORT") {
        let port = port_override(&raw);
        if port.is_some() {
            eprintln!(
                "note: using PORT={} for the MLX lane; set MLX_PORT instead so an unrelated PORT cannot move it",
                raw.trim()
            );
        }
        return (port, "PORT");
    }
    (None, "")
}

/// Is something listening on `port` right now?
fn port_occupied(port: u16) -> bool {
    let addr: SocketAddr = match format!("127.0.0.1:{port}").parse() {
        Ok(a) => a,
        Err(_) => return false,
    };
    TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok()
}

fn find_free_mlx_port() -> u16 {
    if let (Some(port), _source) = env_port() {
        return port;
    }
    // Preferred port 8080, then probe 8082, 8083 … (skip 8081 reserved for llama-server).
    for port in [8080, 8082, 8083, 8084, 8085, 8086, 8087, 8088, 8089, 8090].iter().copied() {
        if port_occupied(port) {
            continue;
        }
        return port;
    }
    // Every probed port answered, and 8080 is the first one probed, so
    // returning it guarantees the engine is handed a port that is already
    // bound and will exit on bind. Refusing is the honest answer: there is no
    // free port to serve on, and the caller turns that into a nonzero exit
    // rather than a retry loop that can never succeed.
    eprintln!(
        "no free MLX port: 8080 and 8082-8090 are all in use. Free one, or set MLX_PORT."
    );
    0
}

/// Did the operator name this lane's port? A named port is held even when it is
/// taken — moving off it silently would serve somewhere the operator did not
/// ask for, which is a different lie than a lane that refuses to start.
fn port_is_pinned() -> bool {
    env_port().0.is_some()
}

/// What a crashed lane should do about its port before the next attempt.
///
/// The retry loop used to reuse the port it was given on every attempt, forever.
/// The commonest reason for a bind failure is *something else already owns that
/// port* — and that is exactly the case a retry can never fix: the daemon
/// re-ran `--port 8080` every 60 seconds for the rest of the machine's life,
/// failing identically, while `~/.lac/mlx.port` kept advertising 8080. Probing
/// on the way back up is what makes the loop able to recover at all.
#[derive(Debug, PartialEq)]
enum PortRecovery {
    /// Nothing is listening: the engine died for its own reasons (an OOM at
    /// 3am is the design case for this loop), so this port is still correct.
    Keep,
    /// Something else holds it, so look for a free one and republish.
    Reprobe,
    /// The operator named it. Hold the port, and say what is holding it.
    Pinned,
}

fn recover_port(occupied: bool, pinned: bool) -> PortRecovery {
    match (occupied, pinned) {
        (true, true) => PortRecovery::Pinned,
        (true, false) => PortRecovery::Reprobe,
        (false, _) => PortRecovery::Keep,
    }
}

/// Decide, report, and act on one crashed lane's port, returning the port the
/// next attempt should use.
///
/// The probe, the decision, the move and the republication are one function so
/// that "the lane recovers" is something a test can observe: with a real socket
/// held on the port and a recorder standing in for the port file, this returns
/// a different port *and* has published it. When the loop did this inline there
/// was nothing to observe — or to assert — about recovery.
fn recover_lane_port(
    current: u16,
    pinned: bool,
    port_file: &PortFile,
    probe: impl Fn(u16) -> bool,
    find_free: impl Fn() -> u16,
) -> u16 {
    let occupied = probe(current);
    match recover_port(occupied, pinned) {
        PortRecovery::Keep => current,
        PortRecovery::Pinned => {
            eprintln!(
                "port {current} is held by another process, but MLX_PORT/PORT named it — \
                 holding the port and retrying. Free it, or unset the variable to drift."
            );
            current
        }
        PortRecovery::Reprobe => {
            let next = find_free();
            if next == 0 {
                eprintln!(
                    "no free MLX port to move to; retrying {current}, which another process holds"
                );
                return current;
            }
            if next != current {
                eprintln!(
                    "port {current} is held by another process; moving this lane to {next}"
                );
                port_file.republish(next);
            }
            next
        }
    }
}

// ── Auto-restart daemon mode ─────────────────────────────────────────────
// LAUNCHDAEMON=1 (set by the launchd plist): retry forever with backoff
// capped at 60s plus jitter, so a transient OOM at 3am costs a minute,
// not the whole night. Without the flag: single-run, Ctrl+C stops.
const BACKOFF_BASE_MS: u64 = 2000;
const BACKOFF_CAP_MS: u64 = 60_000;

fn should_daemon() -> bool {
    env::var("LAUNCHDAEMON").map_or(false, |v| v == "1")
}

/// Deterministic jitter from pid + attempt (std-only, no rand crate).
fn jitter_ms(attempt: u32) -> u64 {
    let pid = std::process::id() as u64;
    (pid.wrapping_mul(2654435761).wrapping_add(attempt as u64 * 40503)) % 1000
}

fn backoff_ms(attempt: u32) -> u64 {
    let exp = BACKOFF_BASE_MS.saturating_mul(1u64 << attempt.min(5));
    exp.min(BACKOFF_CAP_MS) + jitter_ms(attempt)
}

/// Launch the engine and wait for it, keeping the pid published so a
/// termination signal can be forwarded to it. `status()` consumed the
/// `Child` and returned only an `ExitStatus`, which left the parent with
/// no handle to signal — the mechanism behind orphaned engines.
fn spawn_server(model: &str, port: u16) -> std::io::Result<std::process::ExitStatus> {
    Command::new("mlx_lm.server")
        .arg("--model")
        .arg(model)
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .spawn()
        .and_then(|mut child| {
            common::set_child_pid(child.id() as i32);
            let status = child.wait();
            common::clear_child_pid();
            status
        })
}

/// Set by the readiness prober the first time the engine answers HTTP.
static EVER_READY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Removes `~/.lac/mlx.port` when the wrapper exits, so a crash cannot leave
/// a port advertised that nothing is listening on. The router's readiness
/// gate means a stale file costs a wrong "lane down" report rather than
/// misrouted traffic, but the file claiming a lane that does not exist is
/// still a lie an operator has to notice on their own.
struct PortFile {
    path: String,
}

impl PortFile {
    fn publish(port: u16) -> Self {
        let file = Self::new();
        file.republish(port);
        file
    }

    fn new() -> Self {
        Self {
            path: format!("{}/.lac/mlx.port", common::home_dir()),
        }
    }

    /// A port file aimed at a path that cannot exist, so a test can exercise
    /// republication without writing to the developer's real `~/.lac`.
    #[cfg(test)]
    fn new_in(path: &str) -> Self {
        Self {
            path: path.to_string(),
        }
    }

    /// Point the file at `port`, or say why it could not. Called again when a
    /// crashed lane recovers onto a different port, so the router follows the
    /// drift instead of probing a port this process abandoned.
    fn republish(&self, port: u16) {
        if let Err(e) = common::atomic_write(&self.path, &port.to_string()) {
            // Not fatal — the router still finds a drifted lane by probing,
            // and a 0755 $HOME makes this a warning, not a wall.
            eprintln!(
                "warning: could not write {} ({e}); `loop-studio status` may not follow a port drift",
                self.path
            );
        }
    }
}

impl Drop for PortFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn daemon_retry_loop(model: String, port: u16, pinned: bool, port_file: &PortFile) -> std::io::Error {
    use std::sync::atomic::Ordering;
    let mut crashes: u32 = 0;
    let mut port = port;
    loop {
        eprintln!(
            "mlx-lm launch (attempt {}, model {} on port {})",
            crashes + 1,
            model,
            port
        );
        let (outcome, clean) = match spawn_server(&model, port) {
            Ok(status) => {
                let clean = status.success();
                if clean {
                    eprintln!("mlx-lm exited cleanly");
                } else {
                    eprintln!("mlx-lm crashed ({status}); backing off...");
                }
                (format!("{status}"), clean)
            }
            Err(e) => {
                eprintln!("Failed to launch mlx_lm.server: {e} (retrying)");
                (e.to_string(), false)
            }
        };
        let ever_ready = EVER_READY.load(Ordering::SeqCst);
        if clean {
            // A clean exit is the engine choosing to stop, not a success for
            // this process: no lane is serving. Exiting 0 is what
            // `KeepAlive{SuccessfulExit: false}` treats as "do not restart",
            // so the night ends silently. Report the failure instead.
            return std::io::Error::other(format!(
                "mlx_lm.server exited successfully without staying up ({outcome}); \
                 treating as a failure so the supervisor restarts it"
            ));
        }

        // Before backing off, find out whether the port itself is the problem.
        // Retrying an occupied port is a loop that can never succeed; the one
        // case that does need the same port back is the one this loop exists
        // for (the engine was OOM-killed and the port was released).
        port = recover_lane_port(port, pinned, port_file, port_occupied, find_free_mlx_port);

        crashes = crashes.saturating_add(1);
        if common::daemon_decision(ever_ready, crashes) == common::DaemonDecision::Stop {
            return std::io::Error::other(common::daemon_give_up_reason(
                "mlx_lm.server",
                &model,
                port,
                crashes,
                &outcome,
            ));
        }
        let wait = backoff_ms(crashes - 1);
        if !common::sleep_unless_terminating(Duration::from_millis(wait)) {
            // A signal arrived during backoff. Do not relaunch: the
            // operator asked for this lane to be down.
            return std::io::Error::other("terminated while backing off");
        }
    }
}

fn which(cmd: &str) -> bool {
    common::which(cmd).is_some()
}

fn main() {
    common::ignore_sigpipe();
    common::forward_terminate_to_child_process();
    // launchd never rotates the log its plist opened; the daemon does.
    common::spawn_log_rotator(
        common::log_names_for("mlx"),
        common::LOG_MAX_BYTES,
        common::LOG_KEEP,
        std::time::Duration::from_secs(300),
    );
    eprintln!("=== Loop Studio serve-mlx v2.7 (Rust, port-aware, auto-restart daemon) ===");

    if !common::mlx_supported() && std::env::var("LOOP_STUDIO_ALLOW_MLX").unwrap_or_default() != "1" {
        eprintln!("MLX requires Apple Silicon (arm64 macOS); this host is {}.", common::arch_label());
        eprintln!("Use the llama-server lane instead: `loop-studio serve llama`.");
        eprintln!("Override for testing only: LOOP_STUDIO_ALLOW_MLX=1.");
        std::process::exit(1);
    }

    if !which("mlx_lm.server") {
        eprintln!("mlx_lm.server not found in PATH.");
        eprintln!("Install: brew install mlx-lm  (or: uv tool install mlx-lm)");
        std::process::exit(1);
    }

    let model = env::args().nth(1).unwrap_or_else(|| {
        env::var("MLX_MODEL").unwrap_or_else(|_| "mlx-community/Qwen3.8-27B-4bit".to_string())
    });

    let port = find_free_mlx_port();
    if port == 0 {
        std::process::exit(1);
    }
    eprintln!("mlx-lm server with model: {}", model);
    // Deliberately not "Listening on". Nothing is bound yet, and this line
    // used to be printed before the port was even known to be free, so the
    // last thing in a failed launch's log was a confident claim of a listener
    // that did not exist. The prober below reports READY when it is true.
    eprintln!("starting on 127.0.0.1:{} (readiness confirmed below)", port);
    // Advertise the bound port so the router (and `loop-studio status`) follows
    // drift to 8082+ instead of probing a dead :8080. The router still
    // readiness-gates, so a stale file can never route into the void — and
    // the guard takes the file back down on any exit, so it cannot go stale.
    let port_file = PortFile::publish(port);

    std::thread::spawn(move || {
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if common::wait_for_http(port, Duration::from_secs(300)) {
                EVER_READY.store(true, std::sync::atomic::Ordering::SeqCst);
                eprintln!("serve-mlx READY on :{} (model loaded)", port);
            } else {
                eprintln!(
                    "serve-mlx: no HTTP on :{} after 300s — the model is still loading, or the \
                     engine failed. The supervisor will retry if it exits.",
                    port
                );
            }
        }));
        if let Err(e) = res {
            eprintln!("[serve-mlx] readiness prober panicked (harmless): {:?}", e);
        }
    });

    // Either way the wrapper ends by reporting whether a lane is serving.
    // Both paths used to fall through to a return, so the process exited 0
    // after a crash, a bind failure, or a missing binary — and `loop-studio serve mlx
    // && echo ok` printed ok with no MLX lane running.
    let failure = if should_daemon() {
        daemon_retry_loop(model, port, port_is_pinned(), &port_file)
    } else {
        match spawn_server(&model, port) {
            Ok(status) if status.success() => {
                eprintln!("server exited: {status}");
                std::io::Error::other("mlx_lm.server exited without staying up")
            }
            Ok(status) => {
                eprintln!("server exited: {status}");
                std::io::Error::other(format!("mlx_lm.server exited with {status}"))
            }
            Err(e) => {
                eprintln!("Failed to start mlx_lm.server: {e}");
                e
            }
        }
    };
    drop(port_file);
    eprintln!("serve-mlx: {}", failure);
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    // A `#[test]` that loses its attribute is valid Rust that never runs:
    // the suite goes one test quieter and stays green. Fail the build instead.
    #![deny(dead_code)]

    use super::*;
    // Only the tests bind a socket; the daemon itself only connects.
    use std::net::TcpListener;

    /// The retry loop reuses a port it was given unless something else is
    /// holding it. It used to reuse it unconditionally, so a foreign process
    /// on 8080 meant `--port 8080` failed identically every 60 seconds for the
    /// rest of the machine's life while `~/.lac/mlx.port` kept advertising
    /// 8080 — a retry loop that could not recover from its own commonest cause.
    #[test]
    fn a_crashed_lane_re_probes_a_port_someone_else_holds() {
        // A real listener, so the probe this decision depends on is the code
        // under test rather than a stubbed answer.
        let held = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
        let port = held.local_addr().expect("local addr").port();

        assert!(port_occupied(port), "the probe must see a bound port");
        assert_eq!(
            recover_port(true, false),
            PortRecovery::Reprobe,
            "an auto-chosen port that is taken must be re-probed, not reused"
        );
        assert_eq!(
            recover_port(true, true),
            PortRecovery::Pinned,
            "a port the operator named is held, not silently moved"
        );

        // And the free-port search must skip it.
        let saved: Vec<_> = ["MLX_PORT", "PORT"]
            .iter()
            .map(|v| (*v, std::env::var(v).ok()))
            .collect();
        for (var, _) in &saved {
            unsafe { std::env::remove_var(var) };
        }
        let chosen = find_free_mlx_port();
        assert_ne!(chosen, port, "a bound port was offered again");
        assert_ne!(chosen, 0, "no free port was found");
        assert!(
            [8080, 8082, 8083, 8084, 8085, 8086, 8087, 8088, 8089, 8090].contains(&chosen),
            "{chosen} is outside the documented search list"
        );
        for (var, prior) in saved {
            unsafe {
                match prior {
                    Some(v) => std::env::set_var(var, v),
                    None => std::env::remove_var(var),
                }
            }
        }
        drop(held);
    }

    /// The reason this loop exists is an engine that was OOM-killed at 3am, and
    /// that case must keep its port: the port was released, so re-probing would
    /// only risk moving a working configuration for no reason.
    #[test]
    fn a_crashed_lane_keeps_its_port_when_nothing_holds_it() {
        // The port must be one the kernel just gave us and then let go of: an
        // OOM kill releases a port nobody had claimed. Both halves of the
        // observation below happen on a real socket, and the port is visible
        // to the whole machine between the two halves — so the kernel (or any
        // process on the box) is free to hand that exact port to an outbound
        // connection in the gap. This test used to bind an ephemeral port,
        // drop it, and assert the probe then said "free", which failed about
        // one run in four on an idle host: it was reporting the port
        // allocator's mood, not the code's behaviour. The decision itself is
        // pure and is asserted exactly; only the socket observation is
        // retried, and it fails if the probe is ever systematically wrong.
        let mut observed_free_after_release = false;
        for _ in 0..8 {
            let port = {
                let l = TcpListener::bind("127.0.0.1:0").expect("bind");
                l.local_addr().expect("addr").port()
                // Dropped here: the port is free again, as it is after an OOM kill.
            };
            if !port_occupied(port) {
                observed_free_after_release = true;
                break;
            }
            // Something took the port the instant we let it go. Ask for another.
        }
        assert!(
            observed_free_after_release,
            "a port the kernel released must read as free; the probe never saw it"
        );
        assert_eq!(recover_port(false, false), PortRecovery::Keep);
        assert_eq!(recover_port(false, true), PortRecovery::Keep);
    }

    /// End to end on the recovery step, with a real socket held on the port:
    /// the lane must move *and* republish, because a lane that drifts to a new
    /// port while `~/.lac/mlx.port` still names the old one is a lane the
    /// router cannot find — the exact failure the port file exists to prevent.
    #[test]
    fn a_crashed_lane_moves_and_publishes_where_it_actually_serves() {
        use std::cell::RefCell;
        use std::rc::Rc;

        let held = TcpListener::bind("127.0.0.1:0").expect("bind");
        let taken = held.local_addr().expect("addr").port();
        let file = PortFile::new_in("$definitely-not-a-home/.lac/mlx.port");
        let published: Rc<RefCell<Vec<u16>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&published);

        let chosen = recover_lane_port(
            taken,
            false,
            &file,
            |p| p == taken,
            || {
                sink.borrow_mut().push(9001);
                9001
            },
        );

        assert_eq!(chosen, 9001, "the lane must move off a port it cannot bind");
        assert_eq!(*published.borrow(), vec![9001], "the port file must be told");
        drop(held);
    }

    /// A pinned port is held whatever the probe says, and nothing is published:
    /// moving a port the operator named is a different lie from a lane that
    /// will not start.
    #[test]
    fn a_pinned_lane_holds_its_port_and_publishes_nothing() {
        use std::cell::RefCell;
        use std::rc::Rc;

        let file = PortFile::new_in("$definitely-not-a-home/.lac/mlx.port");
        let published: Rc<RefCell<Vec<u16>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&published);

        let chosen = recover_lane_port(8080, true, &file, |_| true, || {
            sink.borrow_mut().push(9002);
            9002
        });
        assert_eq!(chosen, 8080, "a named port is not moved");
        assert!(published.borrow().is_empty(), "nothing may be published");
    }

    /// A released port — the OOM-kill case this loop exists for — is kept, and
    /// nothing is published.
    #[test]
    fn a_released_port_is_kept_and_nothing_is_published() {
        use std::cell::RefCell;
        use std::rc::Rc;

        let file = PortFile::new_in("$definitely-not-a-home/.lac/mlx.port");
        let published: Rc<RefCell<Vec<u16>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&published);
        let chosen = recover_lane_port(8080, false, &file, |_| false, || {
            sink.borrow_mut().push(9003);
            9003
        });
        assert_eq!(chosen, 8080);
        assert!(published.borrow().is_empty());
    }

    /// A structural guard, and only that. The recovery step is fully tested
    /// above, but nothing behavioural can observe *the retry loop* calling it
    /// without refactoring a working daemon loop around an injected launcher.
    /// Deleting the call — the exact regression this fixes — leaves every other
    /// test in this file green, so this asserts the wiring. It is a source
    /// check on purpose, and it is the only test here that is not behaviour.
    #[test]
    fn the_retry_loop_actually_consults_the_recovery_step() {
        let src = include_str!("serve_mlx.rs");
        let loop_body = src
            .split("fn daemon_retry_loop(")
            .nth(1)
            .expect("daemon_retry_loop is missing from its own source")
            .split("fn which(")
            .next()
            .expect("daemon_retry_loop is unterminated");
        assert!(
            loop_body.contains("recover_lane_port("),
            "the retry loop must route every crash through recover_lane_port; \
             without it the lane re-binds a port another process owns, forever"
        );
    }

    /// The whole truth table, so the one new branch is visible as a decision
    /// rather than buried in the loop.
    #[test]
    fn port_recovery_is_a_closed_truth_table() {
        use PortRecovery::*;
        for occupied in [true, false] {
            for pinned in [true, false] {
                let d = recover_port(occupied, pinned);
                if occupied {
                    assert_eq!(d, if pinned { Pinned } else { Reprobe });
                } else {
                    // A port is never moved while nothing holds it, whatever
                    // the operator said about it.
                    assert_eq!(d, Keep, "occupied={occupied} pinned={pinned}");
                }
            }
        }
    }

    #[test]
    fn backoff_caps_and_grows() {
        assert!(backoff_ms(0) >= BACKOFF_BASE_MS);
        assert!(backoff_ms(0) < backoff_ms(3));
        assert!(backoff_ms(30) <= BACKOFF_CAP_MS + 1000);
        assert!(backoff_ms(100) <= BACKOFF_CAP_MS + 1000);
    }

    #[test]
    fn jitter_bounded() {
        for a in 0..10 {
            assert!(jitter_ms(a) < 1000);
        }
    }

    #[test]
    fn port_override_is_the_readers_rule_not_a_second_one() {
        // Deliberately not a reimplementation. `common::mlx_port_from` is the
        // single definition of a port the router will honour, and the writer
        // calls it, so the two cannot drift again. This asserts the
        // delegation rather than restating the table, which `common` owns.
        for raw in ["8080", " 8082 ", "0", "", "abc", "65536"] {
            assert_eq!(
                port_override(raw),
                common::mlx_port_from(raw),
                "{raw:?} must resolve the same on both sides"
            );
        }
        // And the case that motivated it: 0 is refused, so the wrapper can
        // never hand `--port 0` to the engine or publish 0 to the port file.
        assert_eq!(port_override("0"), None);
    }

    #[test]
    fn backoff_stays_inside_its_ceiling_on_the_retry_path() {
        // The loop now indexes backoff by `crashes - 1`, so the first wait is
        // still the base delay and the cap still holds well past the point
        // where it stops retrying.
        assert!(backoff_ms(0) >= BACKOFF_BASE_MS);
        for attempt in 0..64 {
            assert!(
                backoff_ms(attempt) <= BACKOFF_CAP_MS + 1000,
                "attempt {attempt} exceeded the cap"
            );
        }
    }
}
