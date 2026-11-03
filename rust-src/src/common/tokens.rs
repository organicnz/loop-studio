//! Token counting — session estimate, router usage log, opencode probe.

use std::env;
use std::fs;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use super::home_dir;

/// The router's per-request usage log, and the rotated generation it keeps
/// beside it, as (live, rotated).
pub fn usage_log_paths() -> (String, String) {
    let live = format!("{}/.loop-studio/router-usage.jsonl", home_dir());
    let rotated = format!("{live}.1");
    (live, rotated)
}

/// A u64 field in a hand-rolled JSON line, or None.
pub fn json_u64(line: &str, key: &str) -> Option<u64> {
    let p = line.find(key)?;
    let rest = line[p + key.len()..].trim_start();
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    rest[..end].parse().ok()
}

/// `est_tokens` summed over the usage log's lines within `window_secs` of `now`.
pub fn sum_est_tokens(paths: &[String], now: u64, window_secs: u64) -> Option<u64> {
    let mut sum = 0u64;
    let mut lines = 0u64;
    for path in paths {
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        for line in content.lines() {
            let (Some(ts), Some(est)) = (
                json_u64(line, "\"ts\":"),
                json_u64(line, "\"est_tokens\":"),
            ) else {
                continue;
            };
            lines += 1;
            if ts.saturating_add(window_secs) >= now {
                sum = sum.saturating_add(est);
            }
        }
    }
    if lines == 0 {
        None
    } else {
        Some(sum)
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `est_tokens` from the router's usage log over a window.
pub fn router_est_tokens(window_secs: u64) -> Option<u64> {
    let (live, rotated) = usage_log_paths();
    sum_est_tokens(&[live, rotated], now_unix(), window_secs)
}

/// A token count, and what kind of number it is.
pub fn session_tokens() -> (Option<u64>, &'static str) {
    if let Ok(v) = env::var("LOOP_STUDIO_TOKENS") {
        if let Ok(n) = v.trim().parse::<u64>() {
            return (Some(n), "env");
        }
    }
    if let Some(n) = router_est_tokens(3600) {
        return (Some(n), "router-1h-est");
    }
    (opencode_tokens(), "opencode-probe")
}

/// How a token source is described to an operator, and whether it is a measurement.
pub fn token_source_is_measured(source: &str) -> bool {
    source == "explicit" || source == "env"
}

/// Legacy last-resort probe: the opencode2 health endpoint.
pub fn opencode_tokens() -> Option<u64> {
    let out = Command::new("opencode2")
        .args(["api", "get", "/api/health"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let txt = String::from_utf8_lossy(&out.stdout).to_lowercase();
    let mut best: Option<u64> = None;
    let mut search = txt.as_str();
    loop {
        let pos = match (search.find("token"), search.find("context")) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };
        let p = match pos {
            Some(p) => p,
            None => break,
        };
        let after = &search[p..];
        let mut started: Option<usize> = None;
        let mut end = 0;
        for (i, b) in after.bytes().enumerate() {
            if b.is_ascii_digit() {
                if started.is_none() {
                    started = Some(i);
                }
                end = i + 1;
            } else if started.is_some() {
                break;
            }
        }
        if let Some(s) = started {
            if let Ok(n) = after[s..end].parse::<u64>() {
                if n > 100 && n < 10_000_000 {
                    best = Some(n);
                }
            }
            search = &after[end.min(after.len())..];
        } else if after.len() > 1 {
            search = &after[1..];
        } else {
            break;
        }
        if search.is_empty() {
            break;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    #![deny(dead_code)]
    use super::*;

    #[test]
    fn usage_log_rotated_name_is_the_one_the_reader_asks_for() {
        let (live, rotated) = usage_log_paths();
        assert!(live.ends_with("/.loop-studio/router-usage.jsonl"), "{live}");
        assert_eq!(rotated, format!("{live}.1"), "{rotated}");
    }
}
