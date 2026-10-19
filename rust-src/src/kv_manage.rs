//! kv-manage v2.7 — context cache risk analyzer & truncation engine.
//!
//! v2.7: the router's usage log (~/.lac/router-usage.jsonl) is now the
//! fallback token source, replacing the fragile opencode2-endpoint
//! scrape. Estimates can only escalate GREEN→YELLOW: only measurements
//! (explicit --tokens) or real RAM pressure may call RED.

mod common;

use std::env;
use std::fs;
use std::path::Path;

// qwen3.8-27B full-attention KV footprint, FP16 bytes per token.
// 16 full-attn layers x 4 KV heads x 256 dim x 2 (K+V) x 2 bytes = 32 KiB.
// Gated DeltaNet layers hold recurrent state, not full KV — excluded.
const FP16_BYTES_PER_TOKEN: f64 = 32768.0;

// RAM pressure coupling, scaled by total RAM via
// `common::mem_thresholds_gib()` (sane from 8GB Air to 96GB Studio).

fn usage() -> ! {
    eprintln!("usage: kv-manage check [--tokens N] [--threshold N] [--kv-q4|--kv-q8|--kv-fp16] [--json]");
    eprintln!("       kv-manage truncate --tokens N [--keep N]");
    eprintln!("exit codes: check 0 GREEN / 1 YELLOW / 2 RED; usage errors 3; truncate failures 1; no home 78 (EX_CONFIG, from common::home_dir)");
    std::process::exit(3);
}

/// A value the operator typed, as a number — or the reason it is not one.
///
/// The flags used to be `args.get(i).and_then(|s| s.parse().ok())`, so
/// `--tokens abc` and `--threshold 1e4` silently became "not provided" and
/// the run continued on a different input than the one that was asked for:
/// `check --tokens abc` fell through to the router estimate and printed
/// `tokens: 41022 (router-1h-est)` with `risk: GREEN` and exit 0. A
/// measurement that was requested, mangled, and then replaced by a guess
/// reads exactly like a measurement. Exit 3 is the usage-error code this
/// binary already documents, so that is where a bad number goes.
fn parse_count(flag: &str, raw: Option<&String>) -> u64 {
    parse_count_or(flag, raw, parse_count_result)
}

/// [`parse_count`] for a flag that must name a positive size.
fn parse_size(flag: &str, raw: Option<&String>) -> u64 {
    parse_count_or(flag, raw, parse_size_result)
}

fn parse_count_or(flag: &str, raw: Option<&String>, f: fn(&str, &str) -> Result<u64, String>) -> u64 {
    let raw = match raw {
        Some(r) => r.trim(),
        None => {
            eprintln!("{flag} requires a value");
            usage()
        }
    };
    match f(flag, raw) {
        Ok(n) => n,
        Err(why) => {
            eprintln!("{why}");
            usage()
        }
    }
}

/// The pure part of [`parse_count`], assertable without exiting.
fn parse_count_result(flag: &str, raw: &str) -> Result<u64, String> {
    raw.trim()
        .parse::<u64>()
        .map_err(|_| format!("{flag} expects a whole number, got {raw:?}"))
}

/// [`parse_count`] for a flag that describes a *size*, where zero is not a
/// smaller size but an unusable one.
///
/// `--tokens 0` is a legitimate reading — "this session holds no tokens yet" —
/// and stays valid. `--threshold 0` and `--keep 0` are not: `--threshold 0`
/// reached `measured >= threshold`, which is true for every measurement, so the
/// risk was RED with `truncate now` on a session holding four tokens — a RED
/// carrying no information, from the one command whose job is to measure.
/// `--keep 0` was already rejected a few lines away, so the two halves of one
/// flag family disagreed inside one file.
fn parse_size_result(flag: &str, raw: &str) -> Result<u64, String> {
    let n = parse_count_result(flag, raw)?;
    if n == 0 {
        return Err(format!(
            "{flag} expects a positive number, got 0 — zero is not a {n}-sized window"
        ));
    }
    Ok(n)
}

/// GiB of KV cache for `tokens` at the given quantization factor.
fn kv_gib(tokens: u64, factor: f64) -> f64 {
    tokens as f64 * FP16_BYTES_PER_TOKEN * factor / 1073741824.0
}

#[allow(dead_code)]
fn risk_level_with(
    measured: Option<u64>,
    estimate: Option<u64>,
    threshold: u64,
    free_gib: Option<f64>,
    red_gib: f64,
    yellow_gib: f64,
) -> (&'static str, &'static str) {
    let pct = |t: u64| {
        if threshold > 0 {
            t.saturating_mul(100) / threshold.max(1)
        } else {
            0
        }
    };
    let m_red = measured.map(|t| t >= threshold).unwrap_or(false);
    let m_yel = measured.map(|t| pct(t) >= 66).unwrap_or(false);
    let e_yel = estimate.map(|t| pct(t) >= 66).unwrap_or(false);
    let mem_red = free_gib.map(|f| f < red_gib).unwrap_or(false);
    let mem_yellow = free_gib.map(|f| f < yellow_gib).unwrap_or(false);
    if m_red || mem_red {
        ("RED", "truncate now: kv-manage truncate --tokens N")
    } else if m_yel || e_yel || mem_yellow {
        ("YELLOW", "plan truncation within next few turns")
    } else {
        ("GREEN", "continue")
    }
}

/// RAM pressure at the thresholds this machine actually has.
fn risk_level_full(
    measured: Option<u64>,
    estimate: Option<u64>,
    threshold: u64,
    free_gib: Option<f64>,
) -> (&'static str, &'static str) {
    let (red_gib, yellow_gib) = common::mem_thresholds_gib();
    risk_level_with(measured, estimate, threshold, free_gib, red_gib, yellow_gib)
}

#[allow(dead_code)]
fn risk_level(tokens: u64, threshold: u64, free_gib: Option<f64>) -> (&'static str, &'static str) {
    risk_level_full(Some(tokens), None, threshold, free_gib)
}

#[cfg(test)]
fn risk_level_fixed(tokens: u64, threshold: u64, free_gib: Option<f64>) -> (&'static str, &'static str) {
    // Fixed 2.0/8.0 gates: preserves the original 96GB-Studio expectations
    // independent of the test host's total RAM.
    risk_level_with(Some(tokens), None, threshold, free_gib, 2.0, 8.0)
}

fn exit_for(level: &str) -> i32 {
    match level {
        "RED" => 2,
        "YELLOW" => 1,
        _ => 0,
    }
}

fn cmd_check(args: &[String]) {
    let mut tokens: Option<u64> = None;
    let mut threshold: u64 = 24000;
    let mut factor = 0.5; // q8 default
    let mut kv_label = "q8";
    let mut json = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--tokens" => {
                i += 1;
                tokens = Some(parse_count("--tokens", args.get(i)));
            }
            "--threshold" => {
                i += 1;
                threshold = parse_size("--threshold", args.get(i));
            }
            "--kv-q4" => {
                factor = 0.25;
                kv_label = "q4";
            }
            "--kv-q8" => {
                factor = 0.5;
                kv_label = "q8";
            }
            "--kv-fp16" => {
                factor = 1.0;
                kv_label = "fp16";
            }
            "--json" => json = true,
            _ => usage(),
        }
        i += 1;
    }
    // Token provenance: explicit flag > LOOP_STUDIO_TOKENS env (both measured) >
    // router 1h estimate / legacy probe (estimates, YELLOW-capped).
    let flag_tokens = tokens;
    let env_tokens = if flag_tokens.is_none() {
        // An operator who exports LOOP_STUDIO_TOKENS has asked for a measurement, so
        // a value that will not parse is a usage error rather than a reason to
        // quietly answer with the router estimate instead.
        match env::var("LOOP_STUDIO_TOKENS") {
            Ok(raw) => Some(parse_count("LOOP_STUDIO_TOKENS", Some(&raw))),
            Err(_) => None,
        }
    } else {
        None
    };
    let measured = flag_tokens.or(env_tokens);
    let (estimate, est_source) = if measured.is_none() {
        match common::router_est_tokens(3600) {
            Some(n) => (Some(n), "router-1h-est"),
            None => match common::opencode_tokens() {
                Some(n) => (Some(n), "opencode-probe"),
                None => (None, "unknown"),
            },
        }
    } else {
        (None, "")
    };
    let source = if flag_tokens.is_some() {
        "explicit"
    } else if env_tokens.is_some() {
        "env"
    } else {
        est_source
    };
    let tokens = measured.or(estimate).unwrap_or(0);
    let kv = kv_gib(tokens, factor);
    let free = common::free_ram_gib();
    let (level, action) = risk_level_full(measured, estimate, threshold, free);

    if json {
        println!(
            "{{\"tokens\":{},\"tokens_source\":\"{}\",\"tokens_measured\":{},\"kv_est_gib\":{:.2},\"kv_kind\":\"{}\",\"free_ram_gib\":{},\"threshold\":{},\"risk\":\"{}\",\"action\":\"{}\"}}",
            tokens,
            source,
            measured.is_some(),
            kv,
            kv_label,
            free.map(|f| format!("{:.1}", f)).unwrap_or_else(|| "null".to_string()),
            threshold,
            level,
            common::json_escape(action),
        );
    } else {
        println!("tokens: {} ({})", tokens, source);
        if measured.is_none() && estimate.is_some() {
            println!("note: estimate only — pass --tokens N for a measured reading");
        }
        println!("kv_est_gib ({}): {:.2}", kv_label, kv);
        match free {
            Some(f) => println!("free_ram_gib: {:.1}", f),
            None => println!("free_ram_gib: unknown (vm_stat failed)"),
        }
        println!("threshold: {}", threshold);
        println!("risk: {}", level);
        println!("action: {}", action);
    }
    std::process::exit(exit_for(level));
}

fn archive_dir() -> Option<String> {
    // model_base() already gates on the real mount; this can never
    // shadow /Volumes with an internal directory.
    let dir = format!("{}/hf/kv_cache_history", common::model_base());
    match fs::create_dir_all(&dir) {
        Ok(()) => Some(dir),
        Err(e) => {
            eprintln!("archive dir {} unavailable: {}", dir, e);
            None
        }
    }
}

/// What a truncation run will actually do, before it does it.
///
/// Three things used to be decided here and then written down wrongly:
/// `keep > tokens` warned on stderr and still recorded `keep_last` as if a
/// truncation were pending; an unknown token count was recorded as
/// `tokens_before: 0`, which is a measurement of nothing; and the manifest
/// name was `truncate-<seconds>.json`, so two runs in the same second
/// overwrote each other and the first record was simply gone.
struct TruncationPlan {
    /// Whatever is known about the window, measurement or estimate. The
    /// manifest carries `measured` next to it, so a number with an honest
    /// label is more useful than a null that hides a real reading.
    tokens_before: Option<u64>,
    tokens_source: &'static str,
    /// True only for a number that was measured rather than inferred.
    measured: bool,
    keep: u64,
    /// Nothing will be cut: a *measured* keep window already covers the
    /// window. An estimate cannot establish this — it could be wrong in
    /// either direction, and "nothing to do" is the one conclusion an
    /// estimate must not produce.
    noop: bool,
}

/// Decide the plan from what was actually measured.
///
/// `value` is the count, `source` where it came from, and `measured` whether it
/// is a measurement or an inference.
fn truncation_plan(
    value: Option<u64>,
    source: &'static str,
    measured: bool,
    keep: u64,
) -> TruncationPlan {
    TruncationPlan {
        noop: measured && value.is_some_and(|t| keep >= t),
        tokens_before: value,
        tokens_source: source,
        measured,
        keep,
    }
}

impl TruncationPlan {
    fn manifest(&self, ts: u64) -> String {
        let tokens = match self.tokens_before {
            Some(n) => n.to_string(),
            None => "null".to_string(),
        };
        format!(
            "{{\"tokens_before\":{},\"tokens_measured\":{},\"tokens_source\":\"{}\",\"keep_last\":{},\"preserve\":[\"system\",\"tools\"],\"noop\":{},\"ts\":{}}}",
            tokens,
            self.measured,
            self.tokens_source,
            self.keep,
            self.noop,
            ts
        )
    }
}

/// A manifest path no other run can silently take.
///
/// Seconds alone collide: `kv-manage truncate` is the command AGENTS.md
/// tells the agent to run between tasks, so two runs inside one second is
/// routine, and the second `atomic_write` replaced the first record.
fn unique_manifest_path(dir: &str, ts: u64, pid: u32) -> String {
    let base = format!("{dir}/truncate-{ts}-{pid}");
    if !Path::new(&base).with_extension("json").exists() {
        return format!("{base}.json");
    }
    for n in 1..=999u32 {
        let candidate = format!("{base}-{n}.json");
        if !Path::new(&candidate).exists() {
            return candidate;
        }
    }
    // 1000 same-pid, same-second runs is not a real scenario; say so rather
    // than overwrite the thousandth record.
    eprintln!("archive {} already holds 1000 manifests for this second", dir);
    std::process::exit(1);
}

fn cmd_truncate(args: &[String]) {
    let mut tokens: Option<u64> = None;
    let mut keep: u64 = 8000;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--tokens" => {
                i += 1;
                tokens = Some(parse_count("--tokens", args.get(i)));
            }
            "--keep" => {
                i += 1;
                keep = parse_size("--keep", args.get(i));
            }
            _ => usage(),
        }
        i += 1;
    }
    if keep == 0 {
        eprintln!("--keep must be > 0");
        std::process::exit(3);
    }
    // Provenance travels into the manifest, because the candidates are not the
    // same kind of number: an explicit count and `LOOP_STUDIO_TOKENS` are measurements,
    // the router figure is a one-hour extrapolation, the opencode2 scrape is a
    // guess. The old code wrote all of them — and "nothing answered" — as the
    // same `tokens_before`, and `check`, three functions away, correctly
    // refuses to call the same router figure a measurement.
    let (value, source, measured) = if let Some(n) = tokens {
        (Some(n), "explicit", true)
    } else {
        match env::var("LOOP_STUDIO_TOKENS") {
            Ok(raw) => (Some(parse_count("LOOP_STUDIO_TOKENS", Some(&raw))), "env", true),
            Err(_) => {
                let (probed, probed_source) = common::session_tokens();
                (probed, probed_source, false)
            }
        }
    };
    let plan = truncation_plan(value, source, measured, keep);
    match (plan.tokens_before, plan.measured) {
        (Some(t), true) if keep >= t => eprintln!(
            "note: --keep {keep} covers the measured window of {t} tokens; nothing to cut"
        ),
        (Some(_), false) => eprintln!(
            "note: tokens_before is a {} estimate, not a measurement — this manifest records intent",
            plan.tokens_source
        ),
        (None, _) => eprintln!(
            "note: no token source answered, so this manifest records intent, not a measurement"
        ),
        _ => {}
    }
    let dir = match archive_dir() {
        Some(d) => d,
        None => std::process::exit(1),
    };
    let ts = common::now_unix();
    let path = unique_manifest_path(&dir, ts, std::process::id());
    if let Err(e) = common::atomic_write(&path, &plan.manifest(ts)) {
        eprintln!("archive write {} failed: {}", path, e);
        std::process::exit(1);
    }
    println!("manifest: {}", path);
    println!(
        "tokens_before: {}",
        plan.tokens_before
            .map(|n| n.to_string())
            .unwrap_or_else(|| "unknown".to_string())
    );
    println!("tokens_source: {}", plan.tokens_source);
    println!("keep_last: {}", plan.keep);
    if plan.noop {
        println!("noop: keep already covers the window — the history does not need trimming");
    } else {
        println!("next: in OpenCode, replace history with [system]+[tools]+[summary]+[last {} tokens], then verify with: kv-manage check", plan.keep);
    }
}

fn main() {
    common::ignore_sigpipe();
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        usage();
    }
    match args[0].as_str() {
        "check" => cmd_check(&args[1..]),
        "truncate" => cmd_truncate(&args[1..]),
        _ => usage(),
    }
}

#[cfg(test)]
mod tests {
    // A `#[test]` that loses its attribute is valid Rust that never runs:
    // the suite goes one test quieter and stays green. Fail the build instead.
    #![deny(dead_code)]

    use super::*;

    #[test]
    fn green_when_calm() {
        assert_eq!(risk_level_fixed(1000, 24000, Some(60.0)).0, "GREEN");
    }

    #[test]
    fn red_on_tokens() {
        assert_eq!(risk_level_fixed(24000, 24000, Some(60.0)).0, "RED");
    }

    #[test]
    fn yellow_on_token_pressure() {
        assert_eq!(risk_level_fixed(16000, 24000, Some(60.0)).0, "YELLOW");
    }

    #[test]
    fn red_on_ram_regardless_of_tokens() {
        assert_eq!(risk_level_fixed(500, 24000, Some(1.2)).0, "RED");
    }

    #[test]
    fn yellow_on_ram() {
        assert_eq!(risk_level_fixed(500, 24000, Some(6.0)).0, "YELLOW");
    }

    #[test]
    fn unknown_ram_falls_back_to_tokens() {
        assert_eq!(risk_level_fixed(500, 24000, None).0, "GREEN");
        assert_eq!(risk_level_fixed(30000, 24000, None).0, "RED");
    }

    #[test]
    fn kv_math_sane() {
        // 32K tokens fp16 = 1 GiB exactly (32768 * 32768 bytes).
        assert!((kv_gib(32768, 1.0) - 1.0).abs() < 1e-9);
        assert!((kv_gib(32768, 0.5) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn estimates_cap_at_yellow() {
        // A huge estimate alone must never call RED.
        assert_eq!(risk_level_with(None, Some(1_000_000), 24000, Some(60.0), 2.0, 8.0).0, "YELLOW");
        // ...but a measured value at threshold still goes RED.
        assert_eq!(risk_level_with(Some(24000), Some(1_000_000), 24000, Some(60.0), 2.0, 8.0).0, "RED");
        // Small estimate stays GREEN.
        assert_eq!(risk_level_with(None, Some(500), 24000, Some(60.0), 2.0, 8.0).0, "GREEN");
        // No data at all is GREEN (RAM calm).
        assert_eq!(risk_level_with(None, None, 24000, Some(60.0), 2.0, 8.0).0, "GREEN");
    }

    #[test]
    fn json_u64_parses_tolerantly() {
        assert_eq!(common::json_u64(r#"{"ts":123,"est_tokens":456}"#, "\"ts\":"), Some(123));
        assert_eq!(common::json_u64(r#"{"ts": 123 }"#, "\"ts\":"), Some(123));
        assert_eq!(common::json_u64(r#"{"nope":1}"#, "\"ts\":"), None);
        assert_eq!(common::json_u64(r#"{"ts":"abc"}"#, "\"ts\":"), None);
    }

    /// Write `lines` into a temp file and return its path.
    fn usage_file(tag: &str, lines: &[String]) -> String {
        let path = std::env::temp_dir().join(format!(
            "loop-studio-test-usage-{}-{}-{tag}.jsonl",
            std::process::id(),
            lines.len()
        ));
        fs::write(&path, format!("{}\n", lines.join("\n"))).expect("write usage log");
        path.to_string_lossy().into_owned()
    }

    fn record(ts: u64, est: u64) -> String {
        format!(r#"{{"ts":{ts},"rid":1,"est_tokens":{est},"outcome":"proxied"}}"#)
    }

    #[test]
    fn usage_window_reads_the_rotated_generation() {
        // The bug this pins: the router rotates to `router-usage.jsonl.1` and
        // the reader used to ask for `router-usage.1.jsonl`, so the previous
        // window was invisible and the estimate collapsed to whatever the new
        // file happened to hold. Both names now come from one helper, and this
        // asserts the two halves still agree on the *sum*, not just the
        // spelling.
        let now = 1_000_000u64;
        let live = usage_file("live", &[record(now - 10, 100), record(now - 20, 200)]);
        let rotated = usage_file("rotated", &[record(now - 30, 400), record(now - 40, 800)]);
        let both = vec![live.clone(), rotated.clone()];
        assert_eq!(common::sum_est_tokens(&both, now, 3600), Some(1500));

        // Only the live file: what the broken reader would have reported.
        assert_eq!(common::sum_est_tokens(std::slice::from_ref(&live), now, 3600), Some(300));
        // Only the rotated file: what the broken reader would have missed
        // entirely.
        assert_eq!(common::sum_est_tokens(std::slice::from_ref(&rotated), now, 3600), Some(1200));

        for path in both {
            let _ = fs::remove_file(path);
        }
    }

    #[test]
    fn usage_window_excludes_old_entries_but_still_counts_them() {
        let now = 1_000_000u64;
        // An entry from two hours ago, well outside a 1h window.
        let path = usage_file("stale", &[record(now - 7200, 9999), record(now - 5, 42)]);
        // Out of window, so it contributes nothing to the sum...
        assert_eq!(common::sum_est_tokens(std::slice::from_ref(&path), now, 3600), Some(42));
        // ...but it was still a logged line, so the answer is Some(0) rather
        // than None. None means "the router never logged", and a caller that
        // cannot tell that apart from a quiet hour silently drops to its
        // scrape.
        let quiet = usage_file("quiet", &[record(now - 7200, 9999)]);
        assert_eq!(common::sum_est_tokens(std::slice::from_ref(&quiet), now, 3600), Some(0));
        for path in [path, quiet] {
            let _ = fs::remove_file(path);
        }
        // Nothing at all is None, which is the case that means "no data".
        assert_eq!(common::sum_est_tokens(&["/nonexistent/loop-studio-usage-xyz".to_string()], now, 3600), None);
    }

    #[test]
    fn rotated_usage_log_keeps_the_name_the_router_writes() {
        let (live, rotated) = common::usage_log_paths();
        assert!(live.ends_with("/.lac/router-usage.jsonl"), "{live}");
        // The ordinary rotated-log suffix, and the name already on disk.
        assert_eq!(rotated, format!("{live}.1"), "{rotated}");
        assert!(
            !rotated.contains(".1.jsonl"),
            "the reader used to ask for this name and never found it: {rotated}"
        );
    }

    /// A number the operator typed must not be silently replaced by a guess.
    ///
    /// `--tokens abc` parsed to `None`, so `check` fell through to the
    /// router estimate and printed `tokens: 41022 (router-1h-est)` with
    /// `risk: GREEN` and exit 0 — a mangled request rendered as a
    /// measurement, from the one command whose whole job is measuring.
    #[test]
    fn unparsable_numbers_are_refused_not_ignored() {
        assert_eq!(parse_count_result("--tokens", "24000"), Ok(24000));
        assert_eq!(parse_count_result("--tokens", " 24000 "), Ok(24000));
        assert_eq!(parse_count_result("--tokens", "0"), Ok(0));
        for bad in ["abc", "", "  ", "-5", "1e4", "24_000", "24000abc", "NaN", "0x10"] {
            let err = parse_count_result("--tokens", bad).unwrap_err();
            assert!(err.starts_with("--tokens expects"), "{bad}: {err}");
        }
        // Beyond u64 is a typo, not a request for 2^64 tokens.
        assert!(parse_count_result("--tokens", "18446744073709551616").is_err());
        assert!(parse_count_result("--keep", "eight thousand").is_err());
    }

    /// Zero is a size only where it is a reading.
    ///
    /// `--tokens 0` is legitimate — "this session holds no tokens yet" — and
    /// must keep parsing. `--threshold 0` and `--keep 0` are not: a zero
    /// window is not a smaller window. `--threshold 0` made
    /// `measured >= threshold` true for *every* measurement, so `check` printed
    /// `risk: RED` and `action: truncate now` on a session holding four tokens
    /// — a RED carrying no information, from the one command whose entire job
    /// is to measure. `--keep 0` was already rejected a few lines away, so two
    /// halves of one flag family disagreed inside one file.
    #[test]
    fn a_zero_window_is_not_a_smaller_window() {
        // The reading is fine, and goes through the same parser as any other.
        assert_eq!(parse_count_result("--tokens", "0"), Ok(0));
        assert_eq!(parse_count_result("--tokens", "1"), Ok(1));
        // The sizes are not.
        for flag in ["--threshold", "--keep"] {
            let err = parse_size_result(flag, "0").unwrap_err();
            assert!(err.contains("positive"), "{flag}: {err}");
            // ...while a positive value is accepted, so this is not a flag that
            // refuses everything.
            assert_eq!(parse_size_result(flag, "1"), Ok(1));
            assert_eq!(parse_size_result(flag, "24000"), Ok(24000));
        }
    }

    /// The consequence, stated through the function that made it. A zero
    /// threshold made `measured >= threshold` true for *every* measurement, so
    /// the risk was RED with `truncate now` on a session holding four tokens —
    /// a RED carrying no information, printed by the one command whose job is
    /// to measure. The RAM terms are supplied as healthy so only the token
    /// terms can decide the verdict.
    #[test]
    fn a_zero_threshold_made_every_session_red() {
        let (level, action) = risk_level_with(Some(4), None, 0, Some(64.0), 2.0, 8.0);
        assert_eq!(
            (level, action),
            ("RED", "truncate now: kv-manage truncate --tokens N"),
            "a four-token session is not a RED; this is the verdict a zero \
             threshold produced for every measurement"
        );
        // The same session against the real threshold, for contrast.
        let (level, _) = risk_level_with(Some(4), None, 24000, Some(64.0), 2.0, 8.0);
        assert_eq!(level, "GREEN");
    }

    /// The manifest must not contradict the run that wrote it.
    ///
    /// `--keep` larger than the window warned on stderr and still recorded
    /// `keep_last` as a pending cut; an unknown count was recorded as
    /// `tokens_before: 0`, which reads as "the window was empty" instead of
    /// "nothing answered"; and a router *estimate* was recorded as a
    /// measurement, which is the same mistake `check` does not make three
    /// functions away.
    #[test]
    fn manifest_states_what_will_happen() {
        let plan = truncation_plan(Some(30_000), "explicit", true, 8_000);
        assert!(!plan.noop);
        let m = plan.manifest(1_700_000_000);
        assert!(m.contains("\"tokens_before\":30000"), "{m}");
        assert!(m.contains("\"tokens_measured\":true"), "{m}");
        assert!(m.contains("\"tokens_source\":\"explicit\""), "{m}");
        assert!(m.contains("\"keep_last\":8000"), "{m}");
        assert!(m.contains("\"noop\":false"), "{m}");

        // keep == tokens, and keep > tokens: both keep everything, and both
        // have to say so in the document, not only on stderr.
        for keep in [30_000u64, 40_000] {
            let plan = truncation_plan(Some(30_000), "explicit", true, keep);
            assert!(plan.noop, "keep {keep} against 30000 truncates nothing");
            let m = plan.manifest(1_700_000_000);
            assert!(m.contains("\"noop\":true"), "{m}");
            assert!(m.contains(&format!("\"keep_last\":{keep}")), "{m}");
        }

        // An estimate keeps its number and loses its claim to be measured.
        // "Nothing to do" is the one conclusion an estimate may not produce.
        let plan = truncation_plan(Some(30_000), "router-1h-est", false, 40_000);
        assert!(!plan.noop, "an estimate cannot establish that nothing needs cutting");
        let m = plan.manifest(1_700_000_000);
        assert!(m.contains("\"tokens_before\":30000"), "{m}");
        assert!(m.contains("\"tokens_measured\":false"), "{m}");
        assert!(m.contains("\"tokens_source\":\"router-1h-est\""), "{m}");
        assert!(m.contains("\"noop\":false"), "{m}");

        // Nothing known: null, not zero, and not marked as measured.
        let plan = truncation_plan(None, "unknown", false, 8_000);
        assert!(!plan.noop);
        let m = plan.manifest(1_700_000_000);
        assert!(m.contains("\"tokens_before\":null"), "{m}");
        assert!(m.contains("\"tokens_measured\":false"), "{m}");
        assert!(!m.contains("\"tokens_before\":0"), "{m}");

        // A measurement that says the window is idle is a real reading.
        let plan = truncation_plan(Some(0), "explicit", true, 8_000);
        let m = plan.manifest(1_700_000_000);
        assert!(m.contains("\"tokens_before\":0"), "{m}");
        assert!(m.contains("\"tokens_measured\":true"), "{m}");
        assert!(plan.noop);
    }

    /// Two runs in the same second must not overwrite each other.
    ///
    /// `truncate` is the command AGENTS.md tells the agent to run between
    /// tasks, so two runs inside one second is routine — and the manifest
    /// name was `truncate-<seconds>.json`, so the second `atomic_write`
    /// replaced the first record outright.
    #[test]
    fn manifest_names_do_not_collide() {
        let dir = std::env::temp_dir().join(format!(
            "loop-studio-truncate-manifest-{}-{}",
            std::process::id(),
            common::now_unix()
        ));
        fs::create_dir_all(&dir).expect("archive dir");
        let dir = dir.to_string_lossy().into_owned();
        let ts = 1_700_000_000;

        // The race the existence check cannot see: two processes in the same
        // second both look before either has written, so both would be handed
        // the free name and the second `atomic_write` would replace the first
        // record. The pid is what makes the name unique up front.
        let offered = unique_manifest_path(&dir, ts, 111);
        let other_process = unique_manifest_path(&dir, ts, 222);
        assert_ne!(
            offered, other_process,
            "two processes in one second must not be offered one name"
        );

        // Within one process the counter handles the repeats.
        let first = unique_manifest_path(&dir, ts, 4242);
        fs::write(&first, "{\"first\":true}").expect("first manifest");
        let second = unique_manifest_path(&dir, ts, 4242);
        assert_ne!(first, second, "the second run took the first run's name");
        fs::write(&second, "{\"second\":true}").expect("second manifest");
        let third = unique_manifest_path(&dir, ts, 4242);
        assert_ne!(third, first);
        assert_ne!(third, second);
        // And the first record survived both.
        assert_eq!(fs::read_to_string(&first).unwrap(), "{\"first\":true}");
        let _ = fs::remove_dir_all(&dir);
    }
}
