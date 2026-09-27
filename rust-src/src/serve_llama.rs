//! serve-llama v2.6 — llama-server on :8081 (Q8 quality lane).
//!
//! v2.6: thermal probed once (old code spawned pmset 3x and double-
//! shrank Fair 8192->4096 by accident), port overridable via
//! LAC_LLAMA_PORT, binary/model preflight with actionable errors,
//! context clamped to a sane range, and a readiness prober thread so
//! `lac status` stops racing server boot.

mod common;

use std::env;
use std::net::{SocketAddr, TcpStream};
use std::process::Command;
use std::time::Duration;

fn port_in_use(port: u16) -> bool {
    let addr: SocketAddr = match format!("127.0.0.1:{port}").parse() {
        Ok(a) => a,
        Err(_) => return false,
    };
    TcpStream::connect_timeout(&addr, Duration::from_secs(1)).is_ok()
}

fn server_port() -> u16 {
    env::var("LAC_LLAMA_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8081)
}

/// Context default for one thermal reading. Fair/Critical shrink the
/// window so the server stays under the throttle threshold.
fn thermal_default_context(thermal: &str) -> i32 {
    match thermal {
        "Critical" => 4096,
        "Serious" | "Fair" => 8192,
        _ => 16384,
    }
}

fn which(cmd: &str) -> bool {
    common::which(cmd).is_some()
}

fn looks_like_path(model: &str) -> bool {
    model.starts_with('/') || model.starts_with('.') || model.ends_with(".gguf")
}

/// Batch/parallel sizing scaled by host: (batch, ubatch, parallel slots).
/// Pure in (total_gib, cpus) so tests never depend on the build host
/// (same pattern as kv-manage `risk_level_with`). 96GB Studio keeps the
/// original 2048/256/4; smaller Macs step down to conserve KV and slots.
fn llama_tune(total_gib: f64, cpus: usize) -> (i32, i32, i32) {
    let (b, u, p) = if total_gib >= 48.0 {
        (2048, 256, 4)
    } else if total_gib >= 24.0 {
        (1024, 128, 2)
    } else if total_gib >= 12.0 {
        (512, 64, 2)
    } else {
        (256, 32, 1)
    };
    let p = (p as usize).min(cpus.max(1)).max(1) as i32;
    (b, u, p)
}

/// Exact-int env override or the scaled default (rejects garbage/zero).
fn tune_or_env(key: &str, def: i32) -> i32 {
    env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<i32>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(def)
}

// ── Auto-restart daemon mode ─────────────────────────────────────────────
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
fn spawn_server(
    model: &str,
    port: u16,
    context: i32,
    batch: i32,
    ubatch: i32,
    parallel: i32,
) -> std::io::Result<std::process::ExitStatus> {
    Command::new("llama-server")
        .arg("-m")
        .arg(model)
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .arg("-c")
        .arg(context.to_string())
        .arg("-fa")
        .arg("on")
        .arg("-b")
        .arg(batch.to_string())
        .arg("-ub")
        .arg(ubatch.to_string())
        .arg("-np")
        .arg(parallel.to_string())
        .arg("-n-gpu-layers")
        .arg("999")
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

fn daemon_retry_loop(
    model: String,
    port: u16,
    context: i32,
    batch: i32,
    ubatch: i32,
    parallel: i32,
) -> std::io::Error {
    use std::sync::atomic::Ordering;
    let mut crashes: u32 = 0;
    loop {
        eprintln!(
            "llama-server launch (attempt {}, model {} on port {})",
            crashes + 1,
            model,
            port
        );
        let (outcome, clean) = match spawn_server(&model, port, context, batch, ubatch, parallel) {
            Ok(status) => {
                let clean = status.success();
                if clean {
                    eprintln!("llama-server exited cleanly");
                } else {
                    eprintln!("llama-server crashed ({status}); backing off...");
                }
                (format!("{status}"), clean)
            }
            Err(e) => {
                eprintln!("Failed to launch llama-server: {e} (retrying)");
                (e.to_string(), false)
            }
        };
        if clean {
            // A clean exit is the engine choosing to stop, not this process
            // succeeding: no lane is serving. Exiting 0 is exactly what
            // `KeepAlive{SuccessfulExit: false}` reads as "do not restart", so
            // the night would end silently. Report the failure instead.
            return std::io::Error::other(format!(
                "llama-server exited successfully without staying up ({outcome}); \
                 treating as a failure so the supervisor restarts it"
            ));
        }
        crashes = crashes.saturating_add(1);
        if common::daemon_decision(EVER_READY.load(Ordering::SeqCst), crashes)
            == common::DaemonDecision::Stop
        {
            return std::io::Error::other(common::daemon_give_up_reason(
                "llama-server",
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

fn main() {
    common::ignore_sigpipe();
    common::forward_terminate_to_child_process();
    eprintln!("=== LAC serve-llama v2.6 (Rust, port-coexistent, thermal-aware) ===");

    if !which("llama-server") {
        eprintln!("llama-server not found in PATH.");
        eprintln!("Install: brew install llama.cpp");
        std::process::exit(1);
    }

    let model = env::args().nth(1).unwrap_or_else(|| {
        env::var("LLAMA_MODEL").unwrap_or_else(|_| "unsloth/Qwen3.6-27B-MTP-GGUF:Q8_0".to_string())
    });
    if looks_like_path(&model) && std::fs::metadata(&model).is_err() {
        eprintln!("Model file not found: {}", model);
        eprintln!("Hint: pass an HF ref (e.g. unsloth/Qwen3.6-27B-MTP-GGUF:Q8_0) or a valid -m path.");
        std::process::exit(1);
    }

    let thermal = common::thermal_state();
    let context: i32 = env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| thermal_default_context(&thermal));
    let context = context.clamp(2048, 65536);

    let port = server_port();
    eprintln!("llama-server with model: {}", model);
    eprintln!("Context window: {} tokens (thermal={})", context, thermal);
    // Not "Listening on": nothing is bound yet, and this line used to be
    // printed before the port was even known to be free, so a failed launch
    // ended with a confident claim of a listener that did not exist. The
    // prober below reports READY when it is true.
    eprintln!("starting on 127.0.0.1:{} (readiness confirmed below)", port);

    let total_gib = common::total_ram_gib().unwrap_or(16.0);
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let (def_b, def_u, def_np) = llama_tune(total_gib, cpus);
    let batch = tune_or_env("LAC_LLAMA_BATCH", def_b);
    let ubatch = tune_or_env("LAC_LLAMA_UBATCH", def_u);
    let parallel = tune_or_env("LAC_LLAMA_NP", def_np);
    eprintln!(
        "Batch: {} / micro-batch: {} / parallel: {} (host {:.0} GiB, {} cpus; override via LAC_LLAMA_BATCH/_UBATCH/_NP)",
        batch, ubatch, parallel, total_gib, cpus
    );

    // A port already in use is not this process's success. It used to exit 0
    // here, which is the one outcome `KeepAlive{SuccessfulExit: false}` treats
    // as "do not restart" — so under launchd the job went unloaded and stayed
    // unloaded, and the last line in the log was a confident "Listening on
    // 127.0.0.1:8081" for a listener this process never owned.
    //
    // Interactive: refuse, nonzero, so `lac serve llama` is honest.
    // Supervised: do not give up. The holder may be a stale process, and the
    // retry loop is how this lane takes over when it goes away. If the holder
    // is healthy and stays, the loop stops itself after GIVE_UP_AFTER with a
    // message that says exactly that.
    if port_in_use(port) {
        if !should_daemon() {
            eprintln!(
                "port {port} is already bound by another process; not starting a second \
                 llama-server. Route to the existing one, or free the port."
            );
            std::process::exit(1);
        }
        eprintln!(
            "port {port} is already bound; supervising anyway and will take it over if that \
             process exits"
        );
    }

    // Readiness prober: the router's health gate needs /v1/models == 200,
    // which lags bind by the model-load time. Report it instead of racing.
    std::thread::spawn(move || {
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if common::wait_for_http(port, Duration::from_secs(300)) {
                EVER_READY.store(true, std::sync::atomic::Ordering::SeqCst);
                eprintln!("serve-llama READY on :{} (model loaded)", port);
            } else {
                eprintln!(
                    "serve-llama: no HTTP on :{port} after 300s — the model is still loading, or \
                     the engine failed. The supervisor will retry if it exits."
                );
            }
        }));
        if let Err(e) = res {
            eprintln!("[serve-llama] readiness prober panicked (harmless): {:?}", e);
        }
    });

    // Both paths used to fall through to a return, so the process exited 0
    // after a crash, a bind failure, or a missing binary.
    let failure = if should_daemon() {
        daemon_retry_loop(model, port, context, batch, ubatch, parallel)
    } else {
        match spawn_server(&model, port, context, batch, ubatch, parallel) {
            Ok(status) if status.success() => {
                eprintln!("server exited: {status}");
                std::io::Error::other("llama-server exited without staying up")
            }
            Ok(status) => {
                eprintln!("server exited: {status}");
                std::io::Error::other(format!("llama-server exited with {status}"))
            }
            Err(e) => {
                eprintln!("Failed to start llama-server: {e}");
                e
            }
        }
    };
    eprintln!("serve-llama: {failure}");
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thermal_context_steps_down() {
        assert_eq!(thermal_default_context("Nominal"), 16384);
        assert_eq!(thermal_default_context("Fair"), 8192);
        assert_eq!(thermal_default_context("Serious"), 8192);
        assert_eq!(thermal_default_context("Critical"), 4096);
    }

    #[test]
    fn tune_scales_down_on_small_macs() {
        assert_eq!(llama_tune(96.0, 24), (2048, 256, 4));
        assert_eq!(llama_tune(32.0, 12), (1024, 128, 2));
        assert_eq!(llama_tune(16.0, 8), (512, 64, 2));
        assert_eq!(llama_tune(8.0, 8), (256, 32, 1));
    }

    #[test]
    fn tune_parallel_never_exceeds_cpus() {
        assert_eq!(llama_tune(96.0, 2), (2048, 256, 2));
        assert_eq!(llama_tune(8.0, 1), (256, 32, 1));
    }

    #[test]
    fn path_detection() {        assert!(looks_like_path("/models/foo.gguf"));
        assert!(looks_like_path("./x.gguf"));
        assert!(looks_like_path("qwen.gguf"));
        assert!(!looks_like_path("unsloth/Qwen3.6-27B-MTP-GGUF:Q8_0"));
        assert!(!looks_like_path("qwen3.8-27b"));
    }

    #[test]
    fn backoff_caps_and_grows() {
        let b0 = backoff_ms(0);
        let b1 = backoff_ms(1);
        let b5 = backoff_ms(5);
        let b20 = backoff_ms(20);
        assert!(b0 >= 2000 && b0 < 4000);
        assert!(b1 >= 4000 && b1 < 6000);
        assert!(b5 >= 60_000 && b5 <= 61_000);
        assert!(b20 <= 61_000);
    }

    #[test]
    fn jitter_bounded() {
        for a in 0..50 {
            let j = jitter_ms(a);
            assert!(j < 1000);
        }
    }
}
