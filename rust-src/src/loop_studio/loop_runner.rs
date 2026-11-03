//! Loop runner: cmd_loop, cmd_loop_run, test gate, git helpers.

use std::fs;
use std::io::{self, Write};
use std::process::Command;

use super::common;
use super::ops::cap_path;
use super::queue::parse_tasks;

// ------------------------------------------------------------------ git helpers -

/// What a `git` invocation produced.
pub enum GitOutput {
    Text(String),
    Failed(String),
}

impl GitOutput {
    pub fn succeeded(&self) -> bool {
        matches!(self, GitOutput::Text(_))
    }
}

/// git bound to the project root — never the ambient cwd.
pub fn git_in(root: &str, args: &[&str]) -> GitOutput {
    match Command::new("git").current_dir(root).args(args).output() {
        Ok(o) if o.status.success() => {
            GitOutput::Text(String::from_utf8_lossy(&o.stdout).into_owned())
        }
        Ok(o) => GitOutput::Failed(format!(
            "git {} exited {}: {}",
            args.join(" "),
            o.status,
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Err(e) => GitOutput::Failed(format!("git {} could not run: {e}", args.join(" "))),
    }
}

// ------------------------------------------------------------------ test gate -

pub struct TestGate {
    pub passed: bool,
    pub detail: String,
}

pub fn run_test_gate(root: &str, dry_run: bool) -> TestGate {
    let manifest = format!("{}/rust-src/Cargo.toml", root);
    if dry_run {
        return TestGate {
            passed: false,
            detail: "not run — --dry-run performs no test run".to_string(),
        };
    }
    if fs::metadata(&manifest).is_err() {
        return TestGate {
            passed: false,
            detail: format!("not run — {manifest} is not here, so there is no suite to gate on"),
        };
    }
    print!("  Running cargo test... ");
    let _ = io::stdout().flush();
    match Command::new("cargo")
        .args(["test", "--manifest-path", &manifest])
        .output()
    {
        Ok(s) if s.status.success() => TestGate {
            passed: true,
            detail: "cargo test: passed".to_string(),
        },
        Ok(s) => TestGate {
            passed: false,
            detail: format!("cargo test: failed ({})", s.status),
        },
        Err(e) => TestGate {
            passed: false,
            detail: format!("cargo test: could not run ({e})"),
        },
    }
}

// ------------------------------------------------------------------ cmd_loop -

pub fn cmd_loop(root: &str, args: &[String]) {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("list");
    let loops_dir = common::loops_dir();

    match sub {
        "init" => {
            println!("Initializing ~/todo/loop-studio-loops and ~/todo/loop-studio-tasks.yaml ...");
            let mut failures: Vec<String> = Vec::new();
            if let Err(e) = fs::create_dir_all(&loops_dir) {
                eprintln!("  [x] Cannot create {}: {}", loops_dir, e);
                std::process::exit(1);
            }
            let templates_dir = format!("{}/templates/loops", root);
            match fs::read_dir(&templates_dir) {
                Ok(entries) => {
                    for entry in entries.flatten() {
                        let dest = format!("{}/{}", loops_dir, entry.file_name().to_string_lossy());
                        if fs::metadata(&dest).is_ok() {
                            println!("  Keeping existing loop: {}", dest);
                        } else {
                            match fs::copy(entry.path(), &dest) {
                                Ok(n) if n > 0 => println!("  Installed loop template: {}", dest),
                                Ok(_) => failures.push(format!("{}: copy wrote nothing", dest)),
                                Err(e) => failures.push(format!("{}: {}", dest, e)),
                            }
                        }
                    }
                }
                Err(e) => failures.push(format!("cannot read {}: {}", templates_dir, e)),
            }
            let task_template = format!("{}/templates/tasks/loop-studio-tasks.yaml", root);
            let task_dest = common::tasks_file();
            if fs::metadata(&task_dest).is_ok() {
                println!("  Keeping existing task queue: {}", task_dest);
            } else if fs::metadata(&task_template).is_err() {
                failures.push(format!("no task template at {}", task_template));
            } else {
                match fs::copy(&task_template, &task_dest) {
                    Ok(n) if n > 0 => println!("  Installed task queue: {}", task_dest),
                    Ok(_) => failures.push(format!("{}: copy wrote nothing", task_dest)),
                    Err(e) => failures.push(format!("{}: {}", task_dest, e)),
                }
            }
            if !failures.is_empty() {
                for failure in &failures {
                    eprintln!("  [x] {}", failure);
                }
                eprintln!("Loop system NOT initialized.");
                std::process::exit(1);
            }
            println!("Loop system initialized successfully.");
        }
        "list" => {
            println!("Available loops in {}:", loops_dir);
            if let Ok(entries) = fs::read_dir(&loops_dir) {
                for entry in entries.flatten() {
                    println!("  - {}", entry.file_name().to_string_lossy());
                }
            } else {
                println!("  (No loops found. Run `loop-studio loop init` to install defaults)");
            }
        }
        "validate" => {
            let file = match args.get(1) {
                Some(f) => f,
                None => {
                    eprintln!("Usage: loop-studio loop validate <path/to/loop.yaml>");
                    std::process::exit(2);
                }
            };
            println!("Validating loop: {}", file);
            match fs::read_to_string(file) {
                Ok(content) => {
                    let mut fails = 0;
                    let mut warns = 0;
                    let need = ["name:", "phases:"];
                    for k in need {
                        if !content.contains(k) {
                            println!("  [x] Missing '{}' field", k);
                            fails += 1;
                        }
                    }
                    for phase in ["implement", "review", "apply", "gate"] {
                        if !content.contains(phase) {
                            println!("  [!] Notice: '{}' phase not mentioned", phase);
                            warns += 1;
                        }
                    }
                    match content.find("max_rounds:") {
                        Some(i) => {
                            let tail = &content[i..];
                            let n: Option<u32> =
                                tail.split_whitespace().nth(1).and_then(|s| s.parse().ok());
                            match n {
                                Some(v) if v > 2 => {
                                    println!(
                                        "  [!] max_rounds={} exceeds 2-round policy (AGENTS.md)",
                                        v
                                    );
                                    warns += 1;
                                }
                                None => {
                                    println!("  [!] max_rounds unparsable; default 2 applies");
                                    warns += 1;
                                }
                                _ => {}
                            }
                        }
                        None => println!(
                            "  [!] Notice: 'max_rounds:' not specified (default 2 applies)"
                        ),
                    }
                    if !content.contains("require_human_gate: true") {
                        println!(
                            "  [!] Notice: human gate not enforced (require_human_gate: true)"
                        );
                        warns += 1;
                    }
                    if fails == 0 {
                        println!(
                            "  [ok] Loop schema valid ({} warning(s)), ready for loop-orchestrator.",
                            warns
                        );
                    } else {
                        eprintln!("  [x] {} error(s) — fix before running.", fails);
                        std::process::exit(1);
                    }
                }
                Err(e) => {
                    eprintln!("  Failed to read file {}: {}", file, e);
                    std::process::exit(1);
                }
            }
        }
        "run" => {
            let dry_run = args.iter().any(|a| a == "--dry-run");
            let target = args
                .iter()
                .skip(1)
                .find(|a| !a.starts_with("--"))
                .map(|s| s.as_str());
            let code = cmd_loop_run(root, target, dry_run);
            if code != 0 {
                std::process::exit(code);
            }
        }
        _ => {
            eprintln!("Unknown loop subcommand. Use: init, list, validate, run");
            std::process::exit(2);
        }
    }
}

pub fn cmd_loop_run(root: &str, target: Option<&str>, dry_run: bool) -> i32 {
    let tasks_file = common::tasks_file();

    println!("\x1b[1;36m=== Loop Studio Autonomous Loop Runner ===\x1b[0m");

    let tasks_content = fs::read_to_string(&tasks_file).unwrap_or_default();
    let tasks = parse_tasks(&tasks_content);

    let (task_id, task_desc, loop_name, from_queue) = if let Some(t) = target {
        if t.ends_with(".yaml") {
            let name = t.trim_end_matches(".yaml");
            (
                format!("loop-{}", name),
                format!("Execute standalone loop {}", t),
                t.to_string(),
                false,
            )
        } else if let Some(matched) = tasks.iter().find(|item| item.id == t) {
            (
                matched.id.clone(),
                matched.desc.clone(),
                "daily-coding.yaml".to_string(),
                true,
            )
        } else {
            (
                "loop-studio-adhoc".to_string(),
                t.to_string(),
                "daily-coding.yaml".to_string(),
                false,
            )
        }
    } else if let Some(first_pending) = tasks.iter().find(|t| t.status == "pending") {
        (
            first_pending.id.clone(),
            first_pending.desc.clone(),
            "daily-coding.yaml".to_string(),
            true,
        )
    } else {
        (
            "loop-studio-001".to_string(),
            "Verify local model stack and run smoke tests via gateway :8000".to_string(),
            "daily-coding.yaml".to_string(),
            false,
        )
    };

    println!("  Task ID: \x1b[1m{}\x1b[0m", task_id);
    println!("  Task:    {}", task_desc);
    println!(
        "  Loop:    {} (enforcing AGENTS.md single-resident model & human gate)",
        loop_name
    );
    if dry_run {
        println!("  Mode:    \x1b[33m--dry-run (simulation)\x1b[0m");
    }
    println!();

    println!(
        "\x1b[1;34m[1/5 PREFLIGHT]\x1b[0m Verifying resident model, context cap & thermals..."
    );
    let gateway_up = common::port_up(common::gateway_port());
    if gateway_up {
        println!("  ✓ Gateway :{} online", common::gateway_port());
    } else {
        println!(
            "  ! Gateway :{} inactive. Running offline simulation.",
            common::gateway_port()
        );
    }
    let cap = fs::read_to_string(cap_path())
        .ok()
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| "unset (policy: 16,000)".to_string());
    println!("  · Context cap: {} ({})", cap, cap_path());
    let thermal = common::thermal_state();
    let measured = thermal.is_some();
    println!(
        "  {} Thermals: {}",
        if thermal.as_deref() == Some("Nominal") {
            "✓"
        } else if measured {
            "!"
        } else {
            "?"
        },
        common::thermal_label(&thermal)
    );
    if matches!(thermal.as_deref(), Some("Critical" | "Serious")) {
        eprintln!(
            "[GATE REJECTED] Thermals are {}. AGENTS.md requires a pause before further work.",
            common::thermal_label(&thermal)
        );
        return 1;
    }

    println!("\n\x1b[1;34m[2/5 IMPLEMENT]\x1b[0m Agent: coder | Task scope locked");
    println!("  Task instruction: \"{}\"", task_desc);
    println!("  · This command dispatches no agent and writes no files.");

    println!("\n\x1b[1;34m[3/5 REVIEW]\x1b[0m Agent: @reviewer (read-only, edit denied)");
    println!("  Auditing uncommitted diff against invariants & policy (max 2 rounds)...");
    let mod_count = match git_in(root, &["status", "-s"]) {
        GitOutput::Text(t) => {
            let n = t.lines().filter(|l| !l.trim().is_empty()).count();
            println!("  Working tree: {} modified/untracked file(s)", n);
            n
        }
        GitOutput::Failed(why) => {
            eprintln!("  [x] Working tree could not be read — {why}");
            0
        }
    };

    println!("\n\x1b[1;34m[4/5 TEST GATE]\x1b[0m Executing test reliability gates...");
    let gate = run_test_gate(root, dry_run);
    if gate.passed {
        println!("  {}", gate.detail);
        println!("\x1b[32m  PASSED\x1b[0m");
    } else {
        eprintln!(
            "\x1b[1;31m[GATE REJECTED]\x1b[0m Test gate {} — halting before the human gate per AGENTS.md.",
            gate.detail
        );
        return 1;
    }

    println!("\n\x1b[1;35m[5/5 HUMAN APPROVAL GATE]\x1b[0m Mandatory per AGENTS.md");
    println!("  -------------------------------------------------------------");
    println!("  Test gate passed. This run produced no code, so the diff below is");
    println!("  your working tree as it was found — not a review candidate.");

    match git_in(root, &["diff", "--stat"]) {
        GitOutput::Text(t) if !t.trim().is_empty() => {
            println!(
                "  Working tree as found ({} file(s) uncommitted):\n{}",
                mod_count,
                t.trim_end()
            );
        }
        GitOutput::Text(_) => {
            println!(
                "  Working tree clean as found (no pending diffs, {} file(s) uncommitted).",
                mod_count
            );
        }
        GitOutput::Failed(why) => {
            eprintln!("  [x] Working tree unknown — {why}");
        }
    }

    println!("  -------------------------------------------------------------");
    println!(
        "\x1b[1;33m[REHEARSAL COMPLETE]\x1b[0m Phases 1-4 were checked. Nothing was implemented,"
    );
    println!("  nothing was reviewed, and no agent was dispatched. To do the work, run");
    println!(
        "  `loop-studio worker --drain` (or the loop-orchestrator skill), which dispatches the"
    );
    println!("  agent inside a task/<id> worktree and is what may retire this task.");

    if from_queue {
        println!(
            "  · Queue untouched: [{}] is still pending in {}",
            task_id, tasks_file
        );
    }
    0
}
