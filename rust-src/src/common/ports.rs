//! Port resolution — env-overridable, single source of truth.

use std::env;
use std::fs;

// `home_dir` lives in `common` (the parent of this module in its canonical
// home) and is reached through `crate::common` rather than `super` so the file
// compiles unchanged when a binary pulls it in a second time with
// `#[path = "common/ports.rs"] mod shared_ports;` — there `super` is the
// including binary's root, not `common`, but `crate::common` is the same
// module in every binary (each one declares `mod common;` at its root).
use crate::common::home_dir;

/// Centralized ports — env-overridable, single source of truth.
/// loop_studio.rs / loop_studio_tui.rs must call these instead of hardcoding 8000/8081/11434.
pub fn gateway_port() -> u16 {
    env::var("LOOP_STUDIO_ROUTER_PORT")
        .ok()
        .and_then(|p| port_from(&p))
        .unwrap_or(8000)
}

/// The single rule for a port taken from the environment: a trimmed `u16`
/// that is not zero.
///
/// Zero is not a port anyone can dial. `TcpListener::bind` reads `127.0.0.1:0`
/// as "any free port", so a program that passes it straight through silently
/// relocates its service to an ephemeral port while every reader of the same
/// variable keeps looking at 0. `MLX_PORT=0` did exactly that, and that lane
/// stayed unfindable even after the variable was removed.
///
/// These three readers were each a bare `parse()` with the zero left in, so
/// `LOOP_STUDIO_ROUTER_PORT=0` bound the whole gateway to a random port — the router
/// logged its real port while `loop-studio chat`, the TUI and `doctor` all dialled 0 —
/// and `serve_llama` carried a fourth definition that disagreed with all of
/// them. One rule, used by every reader and every writer, so a port refused on
/// the way in is refused on the way out too.
pub fn port_from(raw: &str) -> Option<u16> {
    raw.trim().parse::<u16>().ok().filter(|&port| port > 0)
}

/// Any `LOOP_STUDIO_*` port in the environment that this process will ignore, so a
/// typo is reported in the log a human reads rather than discovered much later
/// as a service nothing can reach. Called once at startup; the readers
/// themselves stay silent so a long-running process does not repeat this.
pub fn port_env_notes() -> Vec<String> {
    let mut notes = Vec::new();
    for (var, default) in [
        ("LOOP_STUDIO_ROUTER_PORT", 8000u16),
        ("LOOP_STUDIO_LLAMA_PORT", 8081),
        ("LOOP_STUDIO_OLLAMA_PORT", 11434),
    ] {
        if let Ok(raw) = env::var(var) {
            if port_from(&raw).is_none() {
                notes.push(format!(
                    "{var}={:?} is not a usable port (need 1-65535) — ignored, using {default}",
                    raw.trim()
                ));
            }
        }
    }
    notes
}

pub fn llama_port() -> u16 {
    env::var("LOOP_STUDIO_LLAMA_PORT")
        .ok()
        .and_then(|p| port_from(&p))
        .unwrap_or(8081)
}

pub fn ollama_port() -> u16 {
    env::var("LOOP_STUDIO_OLLAMA_PORT")
        .ok()
        .and_then(|p| port_from(&p))
        .unwrap_or(11434)
}

/// The rule for turning a configured or recorded MLX port into a port the
/// router will actually use: a trimmed, in-range, non-zero `u16`, or nothing.
///
/// Named and shared because `serve-mlx` writes `~/.lac/mlx.port` and this
/// module reads it, and the two had drifted. The writer accepted a bare
/// `parse()`, so `MLX_PORT=0` published `0`, handed `--port 0` to the engine
/// (which makes the OS assign a random ephemeral port), and left the router
/// re-reading a `0` it discards — so the lane stayed unfindable even after
/// the variable was gone. A writer must never publish a value its own reader
/// throws away, so the writer now calls this.
pub fn mlx_port_from(raw: &str) -> Option<u16> {
    port_from(raw)
}

/// Where the MLX lane lives: MLX_PORT env > serve-mlx's port file >
/// default 8080. serve-mlx drifts to 8082+ when 8080 is taken; without
/// this the router would probe a dead port while the server is up.
/// The readiness gate still applies, so a stale port file is harmless.
pub fn mlx_port() -> u16 {
    if let Ok(p) = env::var("MLX_PORT").or_else(|_| env::var("PORT")) {
        if let Some(port) = mlx_port_from(&p) {
            return port;
        }
    }
    if let Ok(s) = fs::read_to_string(format!("{}/.lac/mlx.port", home_dir())) {
        if let Some(port) = mlx_port_from(&s) {
            return port;
        }
    }
    8080
}

#[cfg(test)]
mod tests {
    #![deny(dead_code)]
    use super::*;
    use std::env;

    /// Every port in the environment goes through one rule, and zero is not a
    /// port: `bind(":0")` relocates the service to a random port while every
    /// reader keeps looking at 0. This is the bug that was live in three
    /// readers at once (`LOOP_STUDIO_ROUTER_PORT=0` bound the whole gateway to an
    /// ephemeral port that `loop-studio chat`, the TUI and `doctor` could never dial).
    #[test]
    fn every_env_port_shares_one_rule_that_refuses_zero() {
        for good in ["1", " 8000 ", "65535", "11434"] {
            assert!(port_from(good).is_some(), "{good:?} must be accepted");
        }
        // The whole point: 0 is refused, and so is everything unparseable.
        for bad in ["0", " 0 ", "", "abc", "65536", "-1", "80.0", "0x50"] {
            assert_eq!(port_from(bad), None, "{bad:?} must be refused");
        }
        assert_eq!(port_from("0"), None);
    }

    /// The three readers cannot drift from the rule or from each other: with
    /// every variable set to zero, all three fall back to their documented
    /// defaults instead of returning a port nothing can dial.
    #[test]
    fn a_zero_port_never_reaches_a_reader() {
        // env mutation is process-global. This file is compiled twice in the
        // router binary — once as `common::ports` and once as the `#[path]`-
        // included `shared_ports` — and the router has its own port test that
        // writes these three too. A second writer racing the first on the same
        // process-global vars is a flake, not a test, so every one of them takes
        // one lock (`crate::common::PORT_ENV_TEST_LOCK`) and runs one at a time.
        let _env_lock = crate::common::PORT_ENV_TEST_LOCK.lock().unwrap();
        let saved: Vec<_> = ["LOOP_STUDIO_ROUTER_PORT", "LOOP_STUDIO_LLAMA_PORT", "LOOP_STUDIO_OLLAMA_PORT"]
            .iter()
            .map(|v| (*v, env::var(v).ok()))
            .collect();
        for (var, _) in &saved {
            unsafe { env::set_var(var, "0") };
        }
        assert_eq!(gateway_port(), 8000);
        assert_eq!(llama_port(), 8081);
        assert_eq!(ollama_port(), 11434);
        for note in port_env_notes() {
            assert!(note.contains("not a usable port"), "{note}");
        }
        for (var, prior) in saved {
            unsafe {
                match prior {
                    Some(v) => env::set_var(var, v),
                    None => env::remove_var(var),
                }
            }
        }
        // A usable value is not reported, and is used.
        assert!(port_env_notes().is_empty(), "{:?}", port_env_notes());
    }

    #[test]
    fn mlx_port_rule_is_shared_by_writer_and_reader() {
        // `serve-mlx` writes ~/.lac/mlx.port and this module reads it, so the
        // two must agree on what a usable port is. The writer used to accept
        // `0`, which the reader discards: the engine bound a random ephemeral
        // port while the file said 0, and the lane stayed unfindable.
        assert_eq!(mlx_port_from("8080"), Some(8080));
        assert_eq!(mlx_port_from("  8082\n"), Some(8082));
        assert_eq!(mlx_port_from("65535"), Some(65535));
        for bad in ["0", "", "  ", "abc", "-1", "65536", "8080.5"] {
            assert_eq!(mlx_port_from(bad), None, "{bad:?} must be refused");
        }
    }
}
