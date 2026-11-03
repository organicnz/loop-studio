//! loop-studio v2.6 — unified Loop Studio command suite.
//!
//! Binary entry point. Declares all sub-modules and dispatches commands.

#[path = "../common/mod.rs"]
mod common;

mod bench;
mod chat;
mod daemon;
mod loop_runner;
mod ops;
mod orchestrator;
mod queue;
mod review;
mod worker;

use std::env;
use std::process::Command;
use std::process::Stdio;

pub const VERSION: &str = "2.8.0";
pub const MAX_TASK_ATTEMPTS: u32 = 3;

// ------------------------------------------------------------------ help ---

fn print_help() {
    println!(
        "Loop Studio — Local Agentic Coding Command Suite v{}",
        VERSION
    );
    println!("Usage: loop-studio [command] [arguments]");
    println!(
        "  * Default mode: 'loop-studio' with no arguments runs the 24/7 autonomous worker.\n"
    );
    println!("Commands:");
    println!("  (default)           Run continuous 24/7 autonomous worker loop");
    println!(
        "  worker [--drain] [--parallel N]  Run autonomous worker loop (or batch drain and exit)"
    );
    println!("  daemon [install|uninstall|status] Manage macOS launchd background 24/7 services");
    println!("  status [--json]     Real-time stack health, ports, RAM, and thermals");
    println!("  serve [mlx|llama|ollama|auto] Start inference engine");
    println!(
        "  route [--daemon]    Launch unified intelligent router on :{}",
        common::gateway_port()
    );
    println!("  stop                Stop router + inference servers started by loop-studio");
    println!("  ps                  List loop-studio-related processes and port table");
    println!("  logs [router|worker|mlx|llama] Tail daemon log files");
    println!("  config              Print effective configuration (ports, models, paths)");
    println!("  tui                 Launch interactive Loop Studio Terminal Dashboard");
    println!("  doctor [--json]     End-to-end diagnostics (exit 1 when issues found)");
    println!("  bench [port] [--tokens N] [--temp F]  Measure latency, TTFT, tok/s");
    println!("  tune [--apply] [--tokens N] [--temp F] Rank lanes; --apply pins winner");
    println!("  thermal             Inspect thermals, power state, and throttling risk");
    println!("  cap [--set N]       Check or enforce session context window cap (16K hygiene)");
    println!("  hermes [run|status] Native Hermes orchestrator (judgment-augmented worker)");
    println!("                      run accepts worker flags: --drain, --once, --parallel N");
    println!("  kv [check|truncate] Context hygiene and memory leak prevention");
    println!("  loop [init|list|validate|run] Autonomous Kanban loop management");
    println!("  visualize           Launch SwiftUI dashboard visualizer");
    println!(
        "  chat \"prompt...\"      Single-shot chat through the :{} gateway",
        common::gateway_port()
    );
    println!("  code [-f file] \"...\"  Agentic Code Assistant (refactoring, tests, audits)");
    println!("  pull [model]        Pull model weights (MLX from Hugging Face or Ollama)");
    println!("  dashboard [--install] Launch dashboard, or install it to ~/.local/bin");
    println!("  bootstrap           Run idempotent machine bootstrap");
    println!("  version             Print version information\n");
}

// -------------------------------------------------------------------- main -

fn main() {
    common::ignore_sigpipe();
    let root = common::project_root();
    let args: Vec<String> = env::args().skip(1).collect();

    if args.is_empty() {
        println!("================================================================");
        println!(
            "  Loop Studio: Default Mode -> 24/7 Autonomous Worker Daemon (v{})",
            VERSION
        );
        println!("  (Run 'loop-studio help' or 'loop-studio status' for other subcommands)");
        println!("================================================================\n");
        worker::cmd_worker(&root, &[]);
        return;
    }

    match args[0].as_str() {
        "worker" | "start" | "run" | "default" => worker::cmd_worker(&root, &args[1..]),
        "daemon" => daemon::cmd_daemon(&root, &args[1..]),
        "status" => ops::cmd_status(&root, &args[1..]),
        "doctor" => {
            let issues = ops::cmd_doctor(&root, &args[1..]);
            if issues > 0 {
                std::process::exit(1);
            }
        }
        "thermal" => ops::cmd_thermal(),
        "cap" => ops::cmd_cap(&args[1..]),
        "hermes" => orchestrator::cmd_hermes(&root, &args[1..]),
        "stop" => ops::cmd_stop(),
        "ps" => ops::cmd_ps(),
        "logs" => ops::cmd_logs(&args[1..]),
        "config" => ops::cmd_config(&root),
        "bench" => bench::cmd_bench(bench::bench_port(&args[1..], 8000), &args[1..]),
        "tune" => bench::cmd_tune(&args[1..]),
        "serve" => {
            let mode = args.get(1).map(|s| s.as_str()).unwrap_or("auto");
            match mode {
                "mlx" => {
                    if !common::mlx_supported() {
                        eprintln!(
                            "MLX requires Apple Silicon (arm64 macOS); this host is {}.",
                            common::arch_label()
                        );
                        eprintln!("Use the llama-server lane instead: `loop-studio serve llama`.");
                        std::process::exit(1);
                    }
                    daemon::exec_sibling(
                        common::bin(&root, "serve-mlx"),
                        &["mlx-community/Qwen3.8-27B-4bit".to_string()],
                    );
                }
                "llama" => daemon::exec_sibling(
                    common::bin(&root, "serve-llama"),
                    &[
                        "unsloth/Qwen3.6-27B-MTP-GGUF:Q8_0".to_string(),
                        "16384".to_string(),
                    ],
                ),
                "ollama" => {
                    println!("Starting Ollama server...");
                    daemon::exec_sibling(
                        "brew".to_string(),
                        &[
                            "services".to_string(),
                            "start".to_string(),
                            "ollama".to_string(),
                        ],
                    );
                }
                _ => {
                    println!("Smart serve is in the TUI (option 3). Launching...");
                    daemon::exec_sibling(common::bin(&root, "loop-studio-tui"), &[]);
                }
            }
        }
        "route" | "router" => {
            let is_daemon = args.iter().any(|a| a == "--daemon" || a == "-d");
            if is_daemon {
                let port = common::gateway_port();
                println!("Starting loop-studio-router on :{} in background...", port);
                match Command::new(common::bin(&root, "loop-studio-router"))
                    .env("LOOP_STUDIO_ROUTER_PORT", port.to_string())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                {
                    Ok(child) => {
                        let _ = common::atomic_write(
                            &format!("{}/.loop-studio/router.pid", common::home_dir()),
                            &child.id().to_string(),
                        );
                        println!("loop-studio-router started (pid {}).", child.id());
                    }
                    Err(e) => {
                        eprintln!("Router spawn failed: {}", e);
                        std::process::exit(1);
                    }
                }
            } else {
                match Command::new(common::bin(&root, "loop-studio-router"))
                    .env(
                        "LOOP_STUDIO_ROUTER_PORT",
                        common::gateway_port().to_string(),
                    )
                    .status()
                {
                    Ok(status) => std::process::exit(status.code().unwrap_or(1)),
                    Err(e) => {
                        eprintln!("loop-studio: could not run loop-studio-router: {}", e);
                        std::process::exit(127);
                    }
                }
            }
        }
        "tui" => daemon::exec_sibling(common::bin(&root, "loop-studio-tui"), &[]),
        "kv" => daemon::exec_sibling(common::bin(&root, "kv-manage"), &args[1..]),
        "loop" => loop_runner::cmd_loop(&root, &args[1..]),
        "visualize" | "studio" | "app" | "gui" => ops::cmd_visualize(&root),
        "chat" => chat::cmd_chat(&root, &args[1..]),
        "code" => chat::cmd_code(&root, &args[1..]),
        "dashboard" => ops::cmd_dashboard(&root, &args[1..]),
        "bootstrap" => daemon::exec_sibling(common::bin(&root, "bootstrap"), &[]),
        "models" => daemon::exec_sibling(common::bin(&root, "pull-models"), &[]),
        "pull" => std::process::exit(daemon::cmd_pull(&root, &args[1..])),
        "version" | "-v" | "--version" => {
            println!("Loop Studio (Local Agentic Coding) v{}", VERSION);
            println!("Engine: Qwen 3.8 27B Dense Hybrid VLM (MTP enabled)");
            println!("Target: {}", common::arch_label());
        }
        _ => print_help(),
    }
}

// ------------------------------------------------------------------- tests -
#[cfg(test)]
mod tests {
    #![deny(dead_code)]

    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn the_log_footprint_line_escalates_but_never_fails() {
        let cap = common::LOG_MAX_BYTES;
        let quiet = ops::doctor_logs_check(0, 0, 0, 0);
        assert_eq!(quiet.level, "ok");
        assert!(quiet.detail.contains("0.0 MiB"), "{}", quiet.detail);
        assert!(!quiet.detail.contains("--purge"), "{}", quiet.detail);

        let working = ops::doctor_logs_check(cap, 1, 0, 0);
        assert_eq!(working.level, "ok", "{}", working.detail);

        let loud = ops::doctor_logs_check(cap * 9, 12, 0, 0);
        assert_eq!(loud.level, "warn", "{}", loud.detail);
        assert!(
            loud.detail.contains("--purge"),
            "a warning must say what to do: {}",
            loud.detail
        );

        for bytes in [0, cap, cap * 9, cap * 1000] {
            assert_ne!(
                ops::doctor_logs_check(bytes, 3, 0, 0).level,
                "fail",
                "{bytes} bytes"
            );
            assert_ne!(
                ops::doctor_logs_check(0, 0, bytes, 3).level,
                "fail",
                "{bytes} transcript bytes"
            );
        }
    }

    #[test]
    fn the_agent_transcript_tree_is_reported_and_reclaimable() {
        let cap = common::TASK_LOG_MAX_BYTES * common::TASK_LOG_KEEP as u64;
        let small = ops::doctor_logs_check(0, 0, cap / 2, 3);
        assert_eq!(small.level, "ok", "{}", small.detail);
        assert!(
            small.detail.contains("agent transcripts"),
            "the transcript tree must be reported even when it is within budget: {}",
            small.detail
        );
        assert!(
            small.detail.contains("task-logs"),
            "and the operator must be told where it lives: {}",
            small.detail
        );
        assert!(
            !small.detail.contains("--purge"),
            "within budget, nothing to reclaim: {}",
            small.detail
        );

        let big = ops::doctor_logs_check(0, 0, cap * 2, 90);
        assert_eq!(big.level, "warn", "{}", big.detail);
        assert!(
            big.detail.contains("loop-studio logs tasks --purge"),
            "a warning must name the reclaim: {}",
            big.detail
        );

        assert!(common::log_file_name("tasks").is_none());
        assert!(common::task_logs_dir().ends_with("task-logs"));

        for bytes in [0, cap, cap * 1000] {
            assert_ne!(
                ops::doctor_logs_check(0, 0, bytes, 3).level,
                "fail",
                "{bytes}"
            );
        }
    }

    const SAMPLE: &str = "\
- id: \"a-1\"\n  task: \"do first\"\n  status: pending\n\n- id: \"b-2\"\n  task: \"do second: with colon\"\n  status: pending\n  attempts: 2\n";

    #[test]
    fn parses_ids_status_attempts() {
        let t = queue::parse_tasks(SAMPLE);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].id, "a-1");
        assert_eq!(t[0].attempts, 0);
        assert_eq!(t[1].desc, "do second: with colon");
        assert_eq!(t[1].attempts, 2);
    }

    #[test]
    fn task_id_sanitize_allows_sane_ids() {
        assert_eq!(queue::sanitize_task_id("a-1").as_deref(), Some("a-1"));
        assert_eq!(
            queue::sanitize_task_id("loop-studio-004").as_deref(),
            Some("loop-studio-004")
        );
        assert_eq!(queue::sanitize_task_id("A").as_deref(), Some("A"));
        assert_eq!(
            queue::sanitize_task_id("x.y_z-9").as_deref(),
            Some("x.y_z-9")
        );
        assert!(queue::sanitize_task_id(&"q".repeat(65)).is_some());
    }

    #[test]
    fn task_id_sanitize_rejects_traversal_and_junk() {
        for bad in [
            "",
            "../../evil",
            "/abs",
            "a/b",
            "a b",
            "-lead",
            ".lead",
            "semi;colon",
            "dq\"q",
            "sq'q",
            "back`tick",
            "dollar$",
            "tilde~",
            "ctrl\x01",
            "star*",
        ] {
            assert!(
                queue::sanitize_task_id(bad).is_none(),
                "must reject {:?}",
                bad
            );
        }
        assert!(
            queue::sanitize_task_id(&"q".repeat(66)).is_none(),
            "must reject 66 chars"
        );
    }

    #[test]
    fn parse_drops_tasks_with_unsafe_ids() {
        let yaml = "- id: \"../../evil\"\n  task: \"pwn\"\n  status: pending\n\n- id: \"ok-1\"\n  task: \"fine\"\n  status: pending\n";
        let t = queue::parse_tasks(yaml);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].id, "ok-1");
    }

    #[test]
    fn chat_field_extraction() {
        let body = r#"{"id":"x","choices":[{"message":{"role":"assistant","content":"Hello \"world\"\nline2"}}]}"#;
        assert_eq!(
            chat::extract_json_string(body, "content").as_deref(),
            Some("Hello \"world\"\nline2")
        );
        assert_eq!(
            chat::extract_json_string(r#"{"error":{"message":"no backend"}}"#, "message")
                .as_deref(),
            Some("no backend")
        );
        assert_eq!(chat::extract_json_string(r#"{"a":1}"#, "content"), None);
        assert_eq!(
            chat::extract_json_string(r#"{"content":"unterminated"#, "content"),
            None
        );
    }

    #[test]
    fn chat_gives_up_on_hung_backend() {
        use std::io::Read;
        let ln = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = ln.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            for stream in ln.incoming().take(1).flatten() {
                let mut s = stream;
                let mut tmp = [0u8; 4096];
                let _ = s.read(&mut tmp);
                std::thread::sleep(Duration::from_secs(30));
            }
        });
        let t0 = Instant::now();
        assert!(
            common::http_post_addr(&addr, "/v1/chat/completions", "{}", Duration::from_secs(2))
                .is_none()
        );
        assert!(t0.elapsed() < Duration::from_secs(20));
    }

    #[test]
    fn update_targets_only_matching_block() {
        let out = queue::update_task(SAMPLE, "b-2", "complete", false);
        assert!(out.contains("- id: \"a-1\"\n  task: \"do first\"\n  status: pending"));
        assert!(
            out.contains("- id: \"b-2\"\n  task: \"do second: with colon\"\n  status: complete")
        );
        assert_eq!(out.matches("status: complete").count(), 1);
    }

    #[test]
    fn update_inserts_missing_keys() {
        let bare = "- id: \"c-3\"\n  task: \"bare block\"\n";
        let out = queue::update_task(bare, "c-3", "pending", true);
        assert!(out.contains("status: pending"), "status inserted:\n{}", out);
        assert!(out.contains("attempts: 1"), "attempts inserted:\n{}", out);
        let t = queue::parse_tasks(&out);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].status, "pending");
        assert_eq!(t[0].attempts, 1);
    }

    #[test]
    fn nested_status_keys_are_ignored() {
        let tricky = "- id: \"d-4\"\n  task: \"migrate status: pending docs\"\n  status: pending\n  metadata:\n    status: archived\n    attempts: 99\n";
        let t = queue::parse_tasks(tricky);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].status, "pending");
        assert_eq!(t[0].attempts, 0);
        let out = queue::update_task(tricky, "d-4", "complete", true);
        assert!(out.contains("\n  status: complete\n"));
        assert!(out.contains("\n    status: archived\n"));
        assert!(out.contains("\n    attempts: 99\n"));
        assert!(out.contains("\n  attempts: 1\n"));
    }

    #[test]
    fn duplicate_ids_update_together() {
        let dup = "- id: \"e-5\"\n  task: \"first\"\n  status: pending\n- id: \"e-5\"\n  task: \"second\"\n  status: pending\n";
        let out = queue::update_task(dup, "e-5", "complete", false);
        assert_eq!(out.matches("status: complete").count(), 2);
    }

    #[test]
    fn update_bumps_attempts_and_inserts_key() {
        let out = queue::update_task(SAMPLE, "a-1", "pending", true);
        assert!(out.contains("attempts: 1"));
        let out2 = queue::update_task(&out, "a-1", "pending", true);
        assert!(out2.contains("attempts: 2"));
    }

    #[test]
    fn dead_letters_exhaust_attempts_gate() {
        assert!(queue::next_pending(SAMPLE).is_some());
        let out = queue::update_task(SAMPLE, "a-1", "pending", true);
        let out = queue::update_task(&out, "a-1", "pending", true);
        let out = queue::update_task(&out, "a-1", "pending", true);
        let next = queue::next_pending(&out).expect("b-2 still retryable");
        assert_eq!(next.id, "b-2");
        let (p, d) = orchestrator::count_pending(&out);
        assert_eq!((p, d), (1, 1));
    }

    #[test]
    fn judge_prompt_lists_candidates_and_thermal() {
        let cands = queue::pending_candidates(SAMPLE);
        assert_eq!(cands.len(), 2);
        let p = orchestrator::judge_prompt(&cands, Some("Nominal"));
        assert!(p.contains("Nominal"), "thermal in prompt:\n{}", p);
        assert!(p.contains("a-1"), "candidate a-1 in prompt:\n{}", p);
        assert!(p.contains("b-2"), "candidate b-2 in prompt:\n{}", p);
        assert!(
            p.contains("TASK_ID:"),
            "format instructions in prompt:\n{}",
            p
        );
    }

    #[test]
    fn judge_parses_non_first_pick() {
        const URGENT_LAST: &str = "\
- id: \"t-routine\"\n  task: \"routine: tidy comments in docs\"\n  status: pending\n\n- id: \"t-normal\"\n  task: \"normal: add a unit test for tail_file\"\n  status: pending\n\n- id: \"t-urgent\"\n  task: \"URGENT: production auth bypass — fix login check in lac.rs now\"\n  status: pending\n";
        let cands = queue::pending_candidates(URGENT_LAST);
        assert_eq!(cands.len(), 3);
        let first = queue::next_pending(URGENT_LAST).expect("pending").id;
        assert_eq!(first, "t-routine");
        let judged = orchestrator::parse_judge_decision(
            "TASK_ID: t-urgent\nREASON: production auth bypass outranks tidy-up\n",
            &cands,
        )
        .expect("valid id parses");
        assert_eq!(judged.id, "t-urgent");
        assert_ne!(judged.id, first, "judgment differs from file order");
        let picked = orchestrator::judge_select(cands, Some(judged)).expect("pick");
        assert_eq!(picked.id, "t-urgent");
    }

    #[test]
    fn judge_falls_back_to_file_order_on_failure() {
        const URGENT_LAST: &str = "\
- id: \"t-routine\"\n  task: \"routine: tidy comments in docs\"\n  status: pending\n\n- id: \"t-urgent\"\n  task: \"URGENT: production auth bypass\"\n  status: pending\n";
        let cands = queue::pending_candidates(URGENT_LAST);
        let expected = queue::next_pending(URGENT_LAST).expect("pending").id;
        assert!(
            orchestrator::parse_judge_decision("ACTION: dispatch_loop\nno task id here\n", &cands)
                .is_none()
        );
        let garbage = orchestrator::parse_judge_decision("hello world", &cands);
        assert_eq!(
            orchestrator::judge_select(cands.clone(), garbage)
                .expect("pick")
                .id,
            expected
        );
        let unknown = orchestrator::parse_judge_decision(
            "TASK_ID: nope-missing\nREASON: hallucinated\n",
            &cands,
        );
        assert!(unknown.is_none());
        assert_eq!(
            orchestrator::judge_select(cands.clone(), unknown)
                .expect("pick")
                .id,
            expected
        );
        assert_eq!(
            orchestrator::judge_select(cands, None).expect("pick").id,
            expected
        );
    }

    #[test]
    fn judge_next_single_candidate_skips_model() {
        const ONE: &str = "- id: \"solo-1\"\n  task: \"only task\"\n  status: pending\n";
        let picked = orchestrator::judge_next(ONE, Some("Nominal"), "/tmp").expect("solo");
        assert_eq!(picked.id, "solo-1");
        assert!(orchestrator::judge_next("nothing here", Some("Nominal"), "/tmp").is_none());
    }

    #[test]
    fn review_parses_critical_and_major_only() {
        let out = "- [critical] lac.rs:42 — unsanitized id reaches git branch\n- [major] lac.rs:90 — missing error handling on router spawn\n- [minor] lac.rs:12 — typo in comment\n- [suggestion] consider renaming\nNO_FINDINGS is absent\nVERDICT: fix";
        let f = review::parse_review_findings(out);
        assert_eq!(f.len(), 2, "only critical+major survive: {:?}", f);
        assert!(f[0].contains("lac.rs:42"));
        assert!(f[1].contains("lac.rs:90"));
        let out2 = "* [Critical] a.rs:1 — x\n[MAJOR] b.rs:2 — y";
        assert_eq!(review::parse_review_findings(out2).len(), 2);
    }

    #[test]
    fn review_approves_clean_output() {
        for clean in [
            "NO_FINDINGS\nVERDICT: approve",
            "- [minor] x.rs:1 — nit\nVERDICT: approve",
            "",
            "Looks fine, no issues.",
            "VERDICT: fix",
        ] {
            assert!(
                review::parse_review_findings(clean).is_empty(),
                "must approve: {:?}",
                clean
            );
        }
    }

    #[test]
    fn review_disposition_caps_rounds() {
        assert_eq!(review::review_disposition(1, false), "commit");
        assert_eq!(review::review_disposition(2, false), "commit");
        assert_eq!(review::review_disposition(1, true), "fix");
        assert_eq!(review::review_disposition(2, true), "commit-flagged");
        assert_eq!(review::review_disposition(99, true), "commit-flagged");
    }

    #[test]
    fn review_prompt_truncates_and_discloses() {
        let big = "x".repeat(review::REVIEW_DIFF_LIMIT + 100);
        let p = review::review_prompt("fix login check", &big);
        assert!(p.contains("fix login check"), "task in prompt");
        assert!(
            p.contains("truncated"),
            "truncation disclosed:\n{}",
            &p[..500]
        );
        assert!(p.len() < big.len() + 2000, "prompt bounded");
        let small = "diff --git a/x b/x";
        let p2 = review::review_prompt("t", small);
        assert!(!p2.contains("truncated"), "no false truncation note");
        assert!(p2.contains(small));
    }

    #[test]
    fn review_disabled_flag() {
        let saved = env::var("LOOP_STUDIO_REVIEW").ok();
        unsafe {
            env::set_var("LOOP_STUDIO_REVIEW", "off");
        }
        assert!(!review::review_enabled());
        unsafe {
            env::set_var("LOOP_STUDIO_REVIEW", "0");
        }
        assert!(!review::review_enabled());
        unsafe {
            env::set_var("LOOP_STUDIO_REVIEW", "1");
        }
        assert!(review::review_enabled());
        unsafe {
            match saved {
                Some(v) => env::set_var("LOOP_STUDIO_REVIEW", v),
                None => env::remove_var("LOOP_STUDIO_REVIEW"),
            }
        }
    }

    #[test]
    fn model_parsed_through_comments() {
        let jsonc = "{\n// comment\n\"model\": \"lac/qwen-test\", /* block */\n}";
        assert_eq!(
            common::jsonc_model_id(jsonc).as_deref(),
            Some("lac/qwen-test")
        );
        assert_eq!(
            common::jsonc_model_id("{\"model\": \"a//b\"}").as_deref(),
            Some("a//b")
        );
        assert_eq!(
            common::jsonc_model_id("{\"model\": \"a\\\"b\"}").as_deref(),
            Some("a\"b")
        );
        for bad in [
            "{}",
            "{\"model\": 42}",
            "{\"model\": \"unterminated",
            "{\n  \"model\"",
            "{\n  \"model\"\u{6a21}\u{578b}",
        ] {
            assert_eq!(
                common::jsonc_model_id(bad),
                None,
                "{bad:?} must not parse or panic"
            );
        }
    }

    #[test]
    fn content_len_handles_escapes() {
        let body = r#"{"choices":[{"message":{"content":"a\"b\\c"}}]}"#;
        assert_eq!(bench::content_len_approx(body), Some(5));
        assert_eq!(bench::content_len_approx("{}"), None);
    }

    #[test]
    fn doctor_memory_check_scales_across_macs() {
        assert_eq!(ops::doctor_memory_check_with(Some(8.0), Some(4.0)).0, "ok");
        assert_eq!(
            ops::doctor_memory_check_with(Some(8.0), Some(2.0)).0,
            "warn"
        );
        assert_eq!(
            ops::doctor_memory_check_with(Some(8.0), Some(1.0)).0,
            "fail"
        );

        assert_eq!(ops::doctor_memory_check_with(Some(16.0), Some(5.0)).0, "ok");
        assert_eq!(
            ops::doctor_memory_check_with(Some(16.0), Some(2.5)).0,
            "warn"
        );
        assert_eq!(
            ops::doctor_memory_check_with(Some(16.0), Some(1.0)).0,
            "fail"
        );

        assert_eq!(
            ops::doctor_memory_check_with(Some(96.0), Some(20.0)).0,
            "ok"
        );
        assert_eq!(
            ops::doctor_memory_check_with(Some(96.0), Some(8.0)).0,
            "warn"
        );
        assert_eq!(
            ops::doctor_memory_check_with(Some(96.0), Some(2.0)).0,
            "fail"
        );

        assert_eq!(ops::doctor_memory_check_with(Some(16.0), None).0, "warn");
    }

    #[test]
    fn intel_hint_never_mentions_mlx() {
        let intel_hint = |mlx_supported: bool| -> &'static str {
            if mlx_supported {
                "no inference backend serving; run loop-studio serve mlx"
            } else {
                "no inference backend serving; run loop-studio serve llama"
            }
        };
        assert!(!intel_hint(false).to_lowercase().contains("mlx"));
        assert!(intel_hint(false).contains("serve llama"));
    }

    #[test]
    fn router_down_hint_matches_standard() {
        assert!(chat::NO_BACKEND_HINT.contains("Start one with `loop-studio serve mlx`."));
    }

    #[test]
    fn line_indent_basic() {
        assert_eq!(queue::line_indent(""), 0);
        assert_eq!(queue::line_indent("   "), 3);
        assert_eq!(queue::line_indent("\t\t"), 2);
        assert_eq!(queue::line_indent("abc"), 0);
        assert_eq!(queue::line_indent("  abc"), 2);
    }

    #[test]
    fn block_key_indent_basic() {
        let block = "- id: task-1\n";
        assert_eq!(queue::block_key_indent(block), 2);
        let block2 = "- id:task-1\n";
        assert_eq!(queue::block_key_indent(block2), 2);
    }

    #[test]
    fn block_field_simple() {
        let block = "- id: my-task\n  task: do it\n  status: pending\n  attempts: 0";
        assert_eq!(queue::block_field(block, "task:").as_deref(), Some("do it"));
        assert_eq!(
            queue::block_field(block, "status:").as_deref(),
            Some("pending")
        );
        assert_eq!(queue::block_field(block, "attempts:").as_deref(), Some("0"));
        assert_eq!(queue::block_field(block, "nonexistent:").as_deref(), None);
    }

    #[test]
    fn pull_target_detection() {
        let is_hf = |m: &str| {
            m.contains('/') || m.starts_with("mlx-") || m.contains("MLX") || m.ends_with(".gguf")
        };
        assert!(is_hf("mlx-community/Qwen3.8-27B-4bit"));
        assert!(is_hf("lmstudio-community/Qwen3.8-27B-MLX-4bit"));
        assert!(is_hf("unsloth/Qwen3.6-27B-MTP-GGUF"));
        assert!(!is_hf("qwen3.8-27b"));
        assert!(!is_hf("llama3"));
    }

    #[test]
    fn probe_opts_parse_and_clamp() {
        let d: Vec<String> = vec![];
        let o = bench::parse_probe_opts(&d);
        assert_eq!((o.tokens, o.temp), (64, 0.0));
        let a = [
            "--tokens".to_string(),
            "128".to_string(),
            "--temp".to_string(),
            "0.7".to_string(),
        ];
        let o = bench::parse_probe_opts(&a);
        assert_eq!((o.tokens, o.temp), (128, 0.7));
        let b = ["--max-tokens=4099".to_string(), "--temp=9.0".to_string()];
        let o = bench::parse_probe_opts(&b);
        assert_eq!((o.tokens, o.temp), (4096, 2.0));
        let c = ["--tokens".to_string(), "1".to_string()];
        let o = bench::parse_probe_opts(&c);
        assert_eq!(o.tokens, 8);
    }

    #[test]
    fn a_gate_that_did_not_run_does_not_report_a_pass() {
        use std::fs;
        let empty = std::env::temp_dir().join(format!(
            "loop-studio-test-no-manifest-{}",
            std::process::id()
        ));
        let _ = fs::create_dir_all(&empty);
        let gate = loop_runner::run_test_gate(&empty.to_string_lossy(), false);
        assert!(
            !gate.passed,
            "a missing suite is not a pass: {}",
            gate.detail
        );
        assert!(gate.detail.contains("not run"), "{}", gate.detail);
        let dry = loop_runner::run_test_gate(&empty.to_string_lossy(), true);
        assert!(!dry.passed, "--dry-run runs nothing: {}", dry.detail);
        assert!(dry.detail.contains("--dry-run"), "{}", dry.detail);
        let _ = fs::remove_dir_all(&empty);
    }

    #[test]
    fn a_copy_onto_its_own_source_is_recognized() {
        use std::fs;
        let dir =
            std::env::temp_dir().join(format!("loop-studio-test-same-file-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let real = dir.join("LACStudio");
        fs::write(&real, b"binary").unwrap();
        let link = dir.join("LACStudio.link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let absent = dir.join("not-created-yet");
        assert!(ops::same_file(
            &real.to_string_lossy(),
            &real.to_string_lossy()
        ));
        assert!(
            ops::same_file(&real.to_string_lossy(), &link.to_string_lossy()),
            "a symlink to the source is the same file"
        );
        assert!(!ops::same_file(
            &real.to_string_lossy(),
            &absent.to_string_lossy()
        ));
        assert!(!ops::same_file(
            &absent.to_string_lossy(),
            &dir.join("other").to_string_lossy()
        ));
        let _ = fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod host_facts_tests {
    #![deny(dead_code)]

    use super::*;

    #[test]
    fn the_host_block_is_parsed_from_its_own_object() {
        let body = r#"{"status":"ok","router":"loop-studio-router v2.8","preferred":"auto",
            "active":"mlx","target_port":8080,"uptime_secs":1,"inflight":0,
            "models_mapped":1,"usage_log":"","backends":{},"stats":{},
            "host":{"thermal":"Serious","free_ram_gib":3.2,"total_ram_gib":16.0}}"#;
        let (free, total, thermal) = ops::parse_router_host_facts(body).expect("the block parses");
        assert_eq!(thermal.as_deref(), Some("Serious"));
        assert_eq!(free, Some(3.2));
        assert_eq!(total, Some(16.0));
    }

    #[test]
    fn a_document_without_the_block_is_none() {
        let body = r#"{"status":"ok","router":"loop-studio-router v2.7","active":"mlx","target_port":8080}"#;
        assert!(ops::parse_router_host_facts(body).is_none());
    }

    #[test]
    fn a_null_reading_stays_unknown() {
        let body = r#"{"host":{"thermal":"Nominal","free_ram_gib":null,"total_ram_gib":null}}"#;
        let (free, total, thermal) = ops::parse_router_host_facts(body).expect("the block parses");
        assert_eq!(thermal.as_deref(), Some("Nominal"));
        assert_eq!(free, None, "null is unknown, not zero");
        assert_eq!(total, None);
    }

    #[test]
    fn a_null_thermal_is_not_the_next_keys_name() {
        let body = r#"{"host":{"thermal":null,"free_ram_gib":12.0,"total_ram_gib":64.0}}"#;
        let (free, total, thermal) = ops::parse_router_host_facts(body).expect("the block parses");
        assert_eq!(
            thermal, None,
            "an absent probe must stay absent, not borrow the following key's name"
        );
        assert_eq!(free, Some(12.0), "the rest of the block still parses");
        assert_eq!(total, Some(64.0));
    }

    #[test]
    fn the_block_ends_at_its_own_brace() {
        let body = r#"{"host":{"thermal":"Nominal","free_ram_gib":1.5,"total_ram_gib":2.5}}"#;
        let (free, total, _) = ops::parse_router_host_facts(body).expect("the block parses");
        assert_eq!(free, Some(1.5));
        assert_eq!(total, Some(2.5));
    }

    #[test]
    fn the_value_helpers_read_what_they_name() {
        let block = r#"{"thermal":"Fair","free_ram_gib":12.0,"total_ram_gib":null}"#;
        assert_eq!(
            ops::json_string(block, "\"thermal\""),
            Some("Fair".to_string())
        );
        assert_eq!(ops::json_string(block, "\"missing\""), None);
        assert_eq!(ops::json_f64(block, "\"free_ram_gib\""), Some(12.0));
        assert_eq!(
            ops::json_f64(block, "\"total_ram_gib\""),
            None,
            "null is not a number"
        );
        assert_eq!(
            ops::json_f64(block, "\"thermal\""),
            None,
            "a string is not a number"
        );
    }
}

#[cfg(test)]
mod dispatch_honesty_tests {
    #![deny(dead_code)]

    use super::*;

    #[test]
    fn a_bench_flag_value_is_not_probed_as_a_port() {
        let s = |x: &str| vec![x.to_string()];
        assert_eq!(
            bench::bench_port(&[], 8000),
            8000,
            "no port argument means the documented default"
        );
        assert_eq!(
            bench::bench_port(&s("8080"), 8000),
            8080,
            "a bare port is a port"
        );
        for flag in [
            "--tokens",
            "--max-tokens",
            "--max_tokens",
            "--temp",
            "--temperature",
        ] {
            let args = vec![flag.to_string(), "128".to_string()];
            assert_eq!(
                bench::bench_port(&args, 8000),
                8000,
                "{flag} 128 must not become port 128"
            );
        }
        let mut mixed = s("--tokens");
        mixed.push("64".to_string());
        mixed.push("9000".to_string());
        assert_eq!(bench::bench_port(&mixed, 8000), 9000);
        assert_eq!(bench::bench_port(&s("0"), 8000), 8000);
        assert_eq!(bench::bench_port(&s("70000"), 8000), 8000);
    }

    #[test]
    fn no_dispatch_arm_discards_a_childs_exit_status() {
        // Read main.rs (this file) to check for the erasing pattern.
        let src = include_str!("main.rs");
        let offenders: Vec<&str> = src
            .lines()
            .map(str::trim_start)
            .filter(|l| !l.starts_with("//"))
            .filter(|l| l.contains("Command::new(") && l.contains(".status()"))
            .filter(|l| l.contains("let _ ="))
            .collect();
        assert!(
            offenders.is_empty(),
            "these arms erase a child's exit status (a missing binary and a failed \
             child both become exit 0): {offenders:?}"
        );
    }

    fn loop_runner_code() -> String {
        include_str!("loop_runner.rs")
            .split("fn cmd_loop_run(")
            .nth(1)
            .expect("cmd_loop_run is missing from its own source")
            .split("fn git_in(")
            .next()
            .expect("cmd_loop_run is unterminated")
            .lines()
            .map(str::trim_start)
            .filter(|l| !l.starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_loop_runner_never_marks_a_task_complete() {
        let body = loop_runner_code();
        assert!(
            !body.contains("update_task(") && !body.contains("update_task_file("),
            "the loop runner dispatches no agent and writes no files, so it must \
             never write a task status; a run that marks a task complete deletes \
             it from the queue while reporting [LOOP COMPLETED]"
        );
        for claim in ["[LOOP COMPLETED]", "Reliability gates passed"] {
            assert!(
                !body.contains(claim),
                "{claim:?} claims a loop completed; this command rehearses phases"
            );
        }
        assert!(
            body.contains("[REHEARSAL COMPLETE]"),
            "a rehearsal must be labelled as one"
        );
    }

    #[test]
    fn a_git_that_did_not_run_is_not_a_clean_tree() {
        let body = loop_runner_code();
        for line in body.lines().filter(|l| l.contains("unwrap_or_default()")) {
            assert!(
                !line.contains("git"),
                "a git result is being unwrapped to a default, which is what makes \
                 a failed git read as a clean tree: {line}"
            );
        }
        assert_eq!(
            body.matches("GitOutput::Failed(why)").count(),
            2,
            "both the review phase and the approval gate must handle a failed git"
        );
    }

    #[test]
    fn the_queue_reader_agrees_with_both_writers() {
        let app_written = "\
- id: 'a-1'
  task: 'Fix the \"task:\" key: colon inside'
  priority: high
  status: failed
  loop: 'daily-coding.yaml'
  created: '2026-09-01'
  attempts: 3
";
        let block = app_written;
        assert_eq!(queue::block_field(block, "- id:").as_deref(), Some("a-1"));
        assert_eq!(
            queue::block_field(block, "task:").as_deref(),
            Some("Fix the \"task:\" key: colon inside"),
            "a value containing its own key name, a colon and a quote"
        );
        assert_eq!(
            queue::block_field(block, "status:").as_deref(),
            Some("failed")
        );
        assert_eq!(
            queue::block_field(block, "attempts:").as_deref(),
            Some("3"),
            "the retry budget must be readable, or the dead-letter is a no-op"
        );

        let worker_written = "\
- id: \"a-1\"
  task: \"do the thing\"
  status: pending
  attempts: 0
";
        assert_eq!(
            queue::block_field(worker_written, "- id:").as_deref(),
            Some("a-1")
        );
        assert_eq!(
            queue::block_field(worker_written, "task:").as_deref(),
            Some("do the thing")
        );
        assert_eq!(
            queue::block_field(worker_written, "attempts:").as_deref(),
            Some("0")
        );

        assert_eq!(queue::unquote("  3  "), "3");
        assert_eq!(queue::unquote("\"x\""), "x");
        assert_eq!(queue::unquote("'x'"), "x");
        assert_eq!(queue::unquote("\"unterminated"), "\"unterminated");
        assert_eq!(queue::unquote("'"), "'");
    }

    #[test]
    fn a_dead_lettered_task_is_not_pending_in_either_quoting() {
        for quoting in ["'", "\""] {
            let block = format!(
                "- id: {q}a-1{q}\n  task: {q}do it{q}\n  status: failed\n  attempts: 3\n",
                q = quoting
            );
            let parsed = queue::parse_tasks(&block);
            let task = parsed.first().expect("one task");
            assert_eq!(task.id, "a-1", "quoting {quoting} leaked into the id");
            assert_eq!(task.status, "failed");
            assert_eq!(
                task.attempts, 3,
                "quoting {quoting} made a dead-lettered task look retryable"
            );
            assert!(
                !parsed
                    .iter()
                    .any(|t| t.status == "pending" && t.attempts < MAX_TASK_ATTEMPTS),
                "a task the worker poisoned must not be re-queued, quoting {quoting}"
            );
        }
    }
}
