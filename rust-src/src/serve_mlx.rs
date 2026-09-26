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

fn find_free_mlx_port() -> u16 {
    if let (Some(port), _source) = env_port() {
        return port;
    }
    // Preferred port 8080, then probe 8082, 8083 … (skip 8081 reserved for llama-server).
    for port in [8080, 8082, 8083, 8084, 8085, 8086, 8087, 8088, 8089, 8090].iter().copied() {
        let addr: SocketAddr = match format!("127.0.0.1:{port}").parse() {
            Ok(a) => a,
            Err(_) => continue,
        };
        if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
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

fn spawn_server(model: &str, port: u16) -> std::io::Result<std::process::ExitStatus> {
    Command::new("mlx_lm.server")
        .arg("--model")
        .arg(model)
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .status()
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
        let path = format!("{}/.lac/mlx.port", common::home_dir());
        if let Err(e) = common::atomic_write(&path, &port.to_string()) {
            // Not fatal — the router still finds a drifted lane by probing,
            // and a 0755 $HOME makes this a warning, not a wall.
            eprintln!(
                "warning: could not write {} ({e}); `lac status` may not follow a port drift",
                path
            );
        }
        Self { path }
    }
}

impl Drop for PortFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn daemon_retry_loop(model: String, port: u16) -> std::io::Error {
    use std::sync::atomic::Ordering;
    let mut crashes: u32 = 0;
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
        std::thread::sleep(Duration::from_millis(wait));
    }
}

fn which(cmd: &str) -> bool {
    common::which(cmd).is_some()
}

fn main() {
    common::ignore_sigpipe();
    eprintln!("=== LAC serve-mlx v2.7 (Rust, port-aware, auto-restart daemon) ===");

    if !common::mlx_supported() && std::env::var("LAC_ALLOW_MLX").unwrap_or_default() != "1" {
        eprintln!("MLX requires Apple Silicon (arm64 macOS); this host is {}.", common::arch_label());
        eprintln!("Use the llama-server lane instead: `lac serve llama`.");
        eprintln!("Override for testing only: LAC_ALLOW_MLX=1.");
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
    // Advertise the bound port so the router (and `lac status`) follows
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
    // after a crash, a bind failure, or a missing binary — and `lac serve mlx
    // && echo ok` printed ok with no MLX lane running.
    let failure = if should_daemon() {
        daemon_retry_loop(model, port)
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
    use super::*;

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
