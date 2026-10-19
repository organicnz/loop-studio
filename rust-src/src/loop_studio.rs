//! loop-studio v2.6 — unified Loop Studio command suite.
//!
//! v2.6 hardening over v2.5:
//! - Shared `common` module (memory/thermal/ports/atomic writes/locks).
//! - Worker is single-flight (lockdir), crash-recoverable
//!   (~/.lac/worker-current preserves abandoned worktrees for review).
//! - Queue updates target the task id (old `replacen("status: pending")`
//!   could complete the WRONG task) via atomic writes, with an attempts
//!   counter and dead-letter at 3.
//! - No hardcoded `master`: base branch is detected; repos with zero
//!   commits skip branch flows instead of failing.
//! - A dirty tree is treated as precious: no `checkout .` over user work.
//! - OpenCode/test phases run under timeouts (default 2h / 10m).
//! - `doctor`/`status` gain --json and real exit codes; doctor also
//!   probes gateway, backends, disk, symlinks, templates, and skills.
//! - New ops commands: stop, ps, logs, config.

mod common;

use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const VERSION: &str = "2.8.0";
const MAX_TASK_ATTEMPTS: u32 = 3;

// ------------------------------------------------------------------ help ---

fn print_help() {
    println!("Loop Studio — Local Agentic Coding Command Suite v{}", VERSION);
    println!("Usage: loop-studio [command] [arguments]");
    println!("  * Default mode: 'loop-studio' with no arguments runs the 24/7 autonomous worker.\n");
    println!("Commands:");
    println!("  (default)           Run continuous 24/7 autonomous worker loop");
    println!("  worker [--drain]    Run autonomous worker loop (or batch drain and exit)");
    println!("  daemon [install|uninstall|status] Manage macOS launchd background 24/7 services");
    println!("  status [--json]     Real-time stack health, ports, RAM, and thermals");
    println!("  serve [mlx|llama|ollama|auto] Start inference engine");
    println!("  route [--daemon]    Launch unified intelligent router on :{}", common::gateway_port());
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
    println!("  kv [check|truncate] Context hygiene and memory leak prevention");
    println!("  loop [init|list|validate|run] Autonomous Kanban loop management");
    println!("  visualize           Launch SwiftUI dashboard visualizer");
    println!("  chat \"prompt...\"      Single-shot chat through the :{} gateway", common::gateway_port());
    println!("  code [-f file] \"...\"  Agentic Code Assistant (refactoring, tests, audits)");
    println!("  pull [model]        Pull model weights (MLX from Hugging Face or Ollama)");
    println!("  dashboard [--install] Launch dashboard, or install it to ~/.local/bin");
    println!("  bootstrap           Run idempotent machine bootstrap");
    println!("  version             Print version information\n");
}

// -------------------------------------------------------------- visualize ---

/// Launch the native macOS Loop Studio app when built, else print build guidance.
/// Checks `.build/LAC Studio.app`, `LACStudio` binary, then `LOOP_STUDIO_APP` / `LOOP_STUDIO_DASHBOARD`.
fn cmd_visualize(root: &str) {
    if !common::port_up(common::gateway_port()) {
        let port = common::gateway_port();
        println!("Auto-starting loop-studio-router on :{} in background...", port);
        let router_bin = common::bin(root, "loop-studio-router");
        let _ = Command::new(&router_bin)
            .env("LOOP_STUDIO_ROUTER_PORT", port.to_string())
            .arg(port.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        std::thread::sleep(Duration::from_millis(300));
    }
    // 1. Check for the bundled app (or the pre-rename bundles, for older builds)
    let app_bundles = [
        format!("{}/SwiftUI/.build/Loop Studio.app", root),
        format!("{}/SwiftUI/.build/Loop LAC Studio.app", root),
        format!("{}/SwiftUI/.build/LAC Studio.app", root),
        format!("{}/SwiftUI/.build/LACDashboard.app", root),
    ];
    for app in &app_bundles {
        if fs::metadata(app).is_ok() {
            // `status()` is `Ok` for *any* exit code, so this said "opened" for
            // an app bundle `open` could not launch. `open` itself exits
            // nonzero for a bundle that is corrupt or unsigned, and reporting
            // that as success is how a broken Studio looks like a working one.
            match Command::new("open").arg(app).status() {
                Ok(status) if status.success() => return,
                Ok(status) => eprintln!("Loop Studio launch failed ({}): {}", app, status),
                Err(e) => eprintln!("Loop Studio launch failed ({}): {}", app, e),
            }
        }
    }
    // 2. Check for LoopStudio built binary (plus legacy LACStudio/LoopLACStudio/LACDashboard for older builds)
    let candidates = [
        env::var("LOOP_STUDIO_APP").ok(),
        env::var("LOOP_STUDIO_DASHBOARD").ok(),
        Some(format!("{}/SwiftUI/.build/debug/LoopStudio", root)),
        Some(format!("{}/SwiftUI/.build/debug/LoopLACStudio", root)),
        Some(format!("{}/SwiftUI/.build/debug/LACStudio", root)),
        Some(format!("{}/SwiftUI/.build/debug/LACDashboard", root)),
    ];
    for cand in candidates.into_iter().flatten() {
        if fs::metadata(&cand).is_ok() {
            // Same defect as the bundle path above, one level down: a Studio
            // binary that crashes at launch used to return from here as though
            // the dashboard had opened.
            match Command::new(&cand).status() {
                Ok(status) if status.success() => return,
                Ok(status) => eprintln!("Loop Studio launch failed ({}): {}", cand, status),
                Err(e) => eprintln!("Loop Studio launch failed ({}): {}", cand, e),
            }
        }
    }
    eprintln!("Loop Studio not built yet.");
    eprintln!("Build & package: cd {}/SwiftUI && ./package-app.sh --open", root);
    eprintln!("Or build debug:  cd {}/SwiftUI && swift run", root);
}

// --------------------------------------------------------------------- chat ---

const NO_BACKEND_HINT: &str =
    "No Loop Studio inference backend is serving. Start one with `loop-studio serve mlx`.";

/// Escape-aware extraction of a top-level string field `"key":"..."`.
/// Returns None when the key is absent or unterminated. Handles the
/// standard escapes (\\ \" \n \t \r); \uXXXX stays literal (best-effort).
fn extract_json_string(body: &str, key: &str) -> Option<String> {
    let pat = format!("\"{}\":\"", key);
    let p = body.find(&pat)?;
    let mut out = String::new();
    let mut esc = false;
    for c in body[p + pat.len()..].chars() {
        if esc {
            out.push(match c {
                'n' => '\n',
                't' => '\t',
                'r' => '\r',
                _ => c,
            });
            esc = false;
        } else if c == '\\' {
            esc = true;
        } else if c == '"' {
            return Some(out);
        } else {
            out.push(c);
        }
    }
    None
}

/// Blocking non-streaming chat POST through the :8000 gateway.
/// Returns (status_code, response_body). std-only, reuses common::http_post.
fn post_chat(body: &str) -> Option<(u16, String)> {
    let secs = env::var("LOOP_STUDIO_CHAT_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(300);
    common::http_post(common::gateway_port(), "/v1/chat/completions", body, secs * 1000)
}

fn cmd_chat(root: &str, args: &[String]) {
    let prompt = args.join(" ");
    if prompt.trim().is_empty() {
        eprintln!("usage: loop-studio chat \"your prompt...\"");
        eprintln!("Model: LOOP_STUDIO_CHAT_MODEL env, else opencode.jsonc model.");
        std::process::exit(2);
    }
    if !common::port_up(common::gateway_port()) {
        eprintln!("{} (gateway :{} down).", NO_BACKEND_HINT, common::gateway_port());
        std::process::exit(1);
    }
    let model = env::var("LOOP_STUDIO_CHAT_MODEL")
        .ok()
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| opencode_model(root));
    let body = format!(
        "{{\"model\":\"{}\",\"messages\":[{{\"role\":\"user\",\"content\":\"{}\"}}],\"temperature\":0.0}}",
        common::json_escape(&model),
        common::json_escape(prompt.trim())
    );
    match post_chat(&body) {
        Some((200, reply)) => match extract_json_string(&reply, "content") {
            Some(text) => println!("{}", text),
            None => {
                match extract_json_string(&reply, "message") {
                    Some(err) => eprintln!("Backend error: {}", err),
                    None => eprintln!("Backend returned 200 without chat content."),
                }
                std::process::exit(1);
            }
        },
        Some((code, reply)) => {
            match extract_json_string(&reply, "message") {
                Some(err) => eprintln!("Backend error (HTTP {}): {}", code, err),
                None => eprintln!("Gateway HTTP {} — {}.", code, NO_BACKEND_HINT),
            }
            std::process::exit(1);
        }
        None => {
            eprintln!("{} (gateway :{} unreachable mid-request).", NO_BACKEND_HINT, common::gateway_port());
            std::process::exit(1);
        }
    }
}

fn extract_fenced_code(text: &str) -> Option<String> {
    let mut inside = false;
    let mut lines = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            if inside {
                return Some(lines.join("\n"));
            } else {
                inside = true;
                continue;
            }
        }
        if inside {
            lines.push(line);
        }
    }
    None
}

fn print_terminal_diff(original: &str, modified: &str, file_name: &str) {
    let orig_lines: Vec<&str> = original.lines().collect();
    let mod_lines: Vec<&str> = modified.lines().collect();
    println!("\x1B[1m--- a/{}\x1B[0m", file_name);
    println!("\x1B[1m+++ b/{}\x1B[0m", file_name);

    // O(n·m) LCS guard: large files fall back to simple +/- to avoid OOM/stall.
    const DIFF_LINE_CAP: usize = 2000;
    if orig_lines.len() + mod_lines.len() > DIFF_LINE_CAP {
        println!("\x1B[33m(diff truncated: {} lines exceed {} cap — simple +/- fallback)\x1B[0m", orig_lines.len() + mod_lines.len(), DIFF_LINE_CAP);
        for l in &orig_lines { println!("\x1B[31m- {}\x1B[0m", l); }
        for l in &mod_lines { println!("\x1B[32m+ {}\x1B[0m", l); }
        return;
    }

    let n = orig_lines.len();
    let m = mod_lines.len();

    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for i in 0..n {
        for j in 0..m {
            if orig_lines[i] == mod_lines[j] {
                dp[i + 1][j + 1] = dp[i][j] + 1;
            } else {
                dp[i + 1][j + 1] = dp[i][j + 1].max(dp[i + 1][j]);
            }
        }
    }

    let mut diff = Vec::new();
    let mut i = n;
    let mut j = m;
    while i > 0 || j > 0 {
        if i > 0 && j > 0 && orig_lines[i - 1] == mod_lines[j - 1] {
            diff.push((' ', orig_lines[i - 1]));
            i -= 1;
            j -= 1;
        } else if j > 0 && (i == 0 || dp[i][j - 1] >= dp[i - 1][j]) {
            diff.push(('+', mod_lines[j - 1]));
            j -= 1;
        } else if i > 0 && (j == 0 || dp[i][j - 1] < dp[i - 1][j]) {
            diff.push(('-', orig_lines[i - 1]));
            i -= 1;
        }
    }
    diff.reverse();

    for (prefix, line) in diff {
        match prefix {
            '+' => println!("\x1B[32m+ {}\x1B[0m", line),
            '-' => println!("\x1B[31m- {}\x1B[0m", line),
            _ => println!("  {}", line),
        }
    }
}

// --------------------------------------------------------------------- code ---

fn cmd_code(root: &str, args: &[String]) {
    let mut file_path: Option<String> = None;
    let mut diff_mode = false;
    let mut write_mode = false;
    let mut model_override: Option<String> = None;
    let mut temp = 0.2f64;
    let mut prompt_parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if (args[i] == "-f" || args[i] == "--file") && i + 1 < args.len() {
            file_path = Some(args[i + 1].clone());
            i += 2;
        } else if args[i] == "--diff" {
            diff_mode = true;
            i += 1;
        } else if args[i] == "--write" || args[i] == "--apply" {
            write_mode = true;
            i += 1;
        } else if (args[i] == "-m" || args[i] == "--model") && i + 1 < args.len() {
            model_override = Some(args[i + 1].clone());
            i += 2;
        } else if args[i] == "--temp" && i + 1 < args.len() {
            if let Ok(v) = args[i + 1].parse::<f64>() { temp = v; }
            i += 2;
        } else {
            prompt_parts.push(args[i].clone());
            i += 1;
        }
    }

    let user_input = prompt_parts.join(" ");

    let file_content = if let Some(ref path) = file_path {
        match fs::read_to_string(path) {
            Ok(content) => Some(content),
            Err(e) => {
                eprintln!("Error reading file '{}': {}", path, e);
                std::process::exit(1);
            }
        }
    } else {
        None
    };

    if user_input.trim().is_empty() && file_content.is_none() {
        eprintln!("usage: loop-studio code [-f file] [--diff] [--write] \"your instruction...\"");
        eprintln!("Examples:");
        eprintln!("  loop-studio code -f src/main.rs --diff \"Refactor this function to be thread-safe\"");
        eprintln!("  loop-studio code -f src/lib.rs --write \"Generate comprehensive unit tests\"");
        eprintln!("  loop-studio code \"Write a zero-allocation circular buffer in Rust\"");
        std::process::exit(2);
    }

    if !common::port_up(common::gateway_port()) {
        eprintln!("{} (gateway :{} down). Start with: loop-studio route --daemon", NO_BACKEND_HINT, common::gateway_port());
        std::process::exit(1);
    }

    let model = model_override
        .or_else(|| env::var("LOOP_STUDIO_CODE_MODEL").ok())
        .or_else(|| env::var("LOOP_STUDIO_CHAT_MODEL").ok())
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| opencode_model(root));

    let system_prompt = "You are Loop Studio Code Assistant, a world-class systems and software engineering agent running locally on Apple Silicon. You write clean, idiomatic, robust, memory-safe code with zero unnecessary dependencies. Provide exact code, concise explanations, and diffs where appropriate.";

    let full_user_content = if let Some(ref code) = file_content {
        let name = file_path.as_deref().unwrap_or("snippet");
        if user_input.trim().is_empty() {
            format!("Review and improve the following file ({}):\n\n```\n{}\n```", name, code)
        } else {
            format!("File: {}\n\n```\n{}\n```\n\nTask: {}", name, code, user_input.trim())
        }
    } else {
        user_input.trim().to_string()
    };

    let body = format!(
        "{{\"model\":\"{}\",\"messages\":[{{\"role\":\"system\",\"content\":\"{}\"}},{{\"role\":\"user\",\"content\":\"{}\"}}],\"temperature\":{}}}",
        common::json_escape(&model),
        common::json_escape(system_prompt),
        common::json_escape(&full_user_content),
        temp
    );

    match post_chat(&body) {
        Some((200, reply)) => match extract_json_string(&reply, "content") {
            Some(text) => {
                if diff_mode || write_mode {
                    let extracted = extract_fenced_code(&text).unwrap_or_else(|| text.clone());
                    if let (Some(orig), Some(path)) = (&file_content, &file_path) {
                        if diff_mode {
                            print_terminal_diff(orig, &extracted, path);
                        }
                        if write_mode {
                            let bak = format!("{}.bak", path);
                            // The backup is not decoration: it is the only copy
                            // of the original once the write lands. It used to
                            // be `let _ =`, so an unwritable backup path (a
                            // directory, a read-only mount) was swallowed and
                            // the CLI went on to print "(backup saved to …)"
                            // pointing at something that was not a backup. If
                            // it cannot be written, the original is not
                            // touched.
                            if let Err(e) = fs::write(&bak, orig) {
                                eprintln!(
                                    "Refusing to overwrite {}: no backup could be written ({}): {}",
                                    path, bak, e
                                );
                                std::process::exit(1);
                            }
                            match fs::write(path, &extracted) {
                                Ok(_) => println!("✓ Refactored code written to {} (backup saved to {})", path, bak),
                                Err(e) => {
                                    // Nonzero, so `loop-studio code --write … && next`
                                    // does not proceed as if the refactor landed.
                                    eprintln!("Failed writing to {}: {}", path, e);
                                    std::process::exit(1);
                                }
                            }
                        }
                    } else {
                        println!("{}", text);
                    }
                } else {
                    println!("{}", text);
                }
            }
            None => {
                match extract_json_string(&reply, "message") {
                    Some(err) => eprintln!("Backend error: {}", err),
                    None => eprintln!("Backend returned 200 without code content."),
                }
                std::process::exit(1);
            }
        },
        Some((code, reply)) => {
            match extract_json_string(&reply, "message") {
                Some(err) => eprintln!("Backend error (HTTP {}): {}", code, err),
                None => eprintln!("Gateway HTTP {} — {}.", code, NO_BACKEND_HINT),
            }
            std::process::exit(1);
        }
        None => {
            eprintln!("{} (gateway :{} unreachable mid-request).", NO_BACKEND_HINT, common::gateway_port());
            std::process::exit(1);
        }
    }
}

// ---------------------------------------------------------------- dashboard ---

/// Whether two paths are the same file, following symlinks. Used to refuse a
/// copy onto its own source: `fs::copy` truncates the destination before it
/// reads, so a self-copy is a delete, and it reports success.
fn same_file(a: &str, b: &str) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        // A destination that does not exist yet cannot be the source; neither
        // can the source be the destination if the source itself is unreadable.
        (Ok(_), Err(_)) | (Err(_), Ok(_)) => false,
        (Err(_), Err(_)) => a == b,
    }
}

fn cmd_dashboard(root: &str, args: &[String]) {
    if args.iter().any(|a| a == "--install") {
        let built_loop_studio = format!("{}/SwiftUI/.build/debug/LoopStudio", root);
        let built_lac_studio = format!("{}/SwiftUI/.build/debug/LACStudio", root);
        let built_loop_lac_studio = format!("{}/SwiftUI/.build/debug/LoopLACStudio", root);
        let built_dash = format!("{}/SwiftUI/.build/debug/LACDashboard", root);
        let src = env::var("LOOP_STUDIO_APP")
            .ok()
            .filter(|p| fs::metadata(p).is_ok())
            .or_else(|| env::var("LOOP_STUDIO_DASHBOARD").ok().filter(|p| fs::metadata(p).is_ok()))
            .or_else(|| if fs::metadata(&built_loop_studio).is_ok() { Some(built_loop_studio) } else { None })
            .or_else(|| if fs::metadata(&built_lac_studio).is_ok() { Some(built_lac_studio) } else { None })
            .or_else(|| if fs::metadata(&built_loop_lac_studio).is_ok() { Some(built_loop_lac_studio) } else { None })
            .unwrap_or(built_dash);
        if fs::metadata(&src).is_err() {
            eprintln!("Loop Studio not built yet. Build first: cd {}/SwiftUI && swift build", root);
            std::process::exit(1);
        }
        let dir = format!("{}/.local/bin", common::home_dir());
        if let Err(e) = fs::create_dir_all(&dir) {
            eprintln!("Install failed: cannot create {}: {}", dir, e);
            std::process::exit(1);
        }
        let dst_loop = format!("{}/LoopStudio", dir);
        let dst_lac = format!("{}/LACStudio", dir);
        let dst_loop_lac = format!("{}/LoopLACStudio", dir);
        let dst_dash = format!("{}/LACDashboard", dir);
        // `fs::copy(x, x)` is not a no-op: it opens the destination O_TRUNC and
        // returns `Ok(0)`, so pointing LOOP_STUDIO_APP at one of the three
        // destinations — the documented "here is my installed copy" use of the
        // variable — destroyed all three binaries and printed "Installed".
        // Reproduced: 40 bytes in, 0 bytes out, three green checkmarks.
        //
        // Every copy is now checked, and the size is compared, because a copy
        // that reports success and writes nothing is the same failure wearing a
        // success message.
        let source_len = fs::metadata(&src).map(|m| m.len()).unwrap_or(0);
        let mut failures: Vec<String> = Vec::new();
        for dst in [&dst_loop, &dst_lac, &dst_loop_lac, &dst_dash] {
            if same_file(&src, dst) {
                failures.push(format!(
                    "{} is the source (LOOP_STUDIO_APP/LOOP_STUDIO_DASHBOARD names a destination) — refusing to copy it onto itself",
                    dst
                ));
                continue;
            }
            match fs::copy(&src, dst) {
                Ok(n) if n == source_len => {}
                Ok(n) => failures.push(format!("{}: wrote {n} of {source_len} bytes", dst)),
                Err(e) => failures.push(format!("{}: {}", dst, e)),
            }
        }
        if !failures.is_empty() {
            for failure in &failures {
                eprintln!("Install failed: {}", failure);
            }
            std::process::exit(1);
        }
        println!("Installed Loop Studio → {} (launch with `loop-studio studio`, `loop-studio visualize`, or `loop-studio dashboard`)", dst_loop);
        return;
    }
    cmd_visualize(root);
}

static PULL_CANCELLED: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
fn install_pull_signal_handler() {
    type Sighandler = unsafe extern "C" fn(std::ffi::c_int);
    unsafe extern "C" {
        fn signal(sig: std::ffi::c_int, handler: Sighandler) -> Sighandler;
    }
    unsafe extern "C" fn cancel(_: std::ffi::c_int) {
        PULL_CANCELLED.store(true, Ordering::SeqCst);
    }
    unsafe {
        signal(15, cancel);
        signal(2, cancel);
    }
}

#[cfg(not(unix))]
fn install_pull_signal_handler() {}

fn run_pull_command(command: &mut Command) -> i32 {
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return 1,
    };
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.code().unwrap_or(1),
            Ok(None) => {
                if PULL_CANCELLED.load(Ordering::SeqCst) {
                    kill_tree(child.id());
                    let _ = child.kill();
                    let _ = child.wait();
                    return 130;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => {
                kill_tree(child.id());
                let _ = child.kill();
                let _ = child.wait();
                return 1;
            }
        }
    }
}

// --------------------------------------------------------------------- pull -

/// Pull model weights from Hugging Face (MLX/GGUF) or Ollama into the local library.
fn cmd_pull(_root: &str, args: &[String]) -> i32 {
    install_pull_signal_handler();
    let model = args.first().map(|s| s.trim()).filter(|s| !s.is_empty()).unwrap_or("qwen3.8-27b");
    println!("=== Loop Studio Model Pull: {} ===", model);

    let is_hf = model.contains('/') || model.starts_with("mlx-") || model.contains("MLX") || model.ends_with(".gguf");

    if is_hf {
        println!("Target is a Hugging Face / MLX repository: {}", model);
        let hf_hub = format!("{}/hf/hub", common::model_base());
        let _ = fs::create_dir_all(&hf_hub);

        if let Some(cli) = common::which("hf").or_else(|| common::which("huggingface-cli")) {
            println!("Downloading via Hugging Face CLI into {}...", hf_hub);
            let mut command = Command::new(cli);
            command
                .args(["download", model])
                .env("HF_HOME", format!("{}/hf", common::model_base()))
                .env("HF_HUB_CACHE", hf_hub.clone());
            let code = run_pull_command(&mut command);
            if code == 0 {
                println!("✓ Successfully downloaded {} to local cache.", model);
            } else if code == 130 {
                eprintln!("Download cancelled.");
            } else {
                eprintln!("huggingface-cli exited with status {}", code);
            }
            return code;
        }
        eprintln!("Hugging Face CLI not found in PATH; install `brew install hf` before pulling models.");
        return 1;
    } else if let Some(ollama) = common::which("ollama") {
        println!("Pulling Ollama model: {}", model);
        let mut command = Command::new(ollama);
        command.arg("pull").arg(model);
        let code = run_pull_command(&mut command);
        if code == 0 {
            println!("✓ Successfully pulled Ollama model: {}", model);
        } else if code == 130 {
            eprintln!("Download cancelled.");
        } else {
            eprintln!("ollama pull exited with status {}", code);
        }
        return code;
    } else {
        eprintln!("ollama not found in PATH — install: brew install ollama");
        1
    }
}

// ------------------------------------------------------------------ status -

/// Best-effort model id from opencode.jsonc (tolerates // and /* */ comments).
fn opencode_model(root: &str) -> String {
    // One parser, shared with the TUI (see `common::jsonc_model_id`): the copy
    // that lived here was correct and the one in the TUI was not, which is the
    // worst way for a parser to be duplicated.
    fs::read_to_string(format!("{}/opencode.jsonc", root))
        .ok()
        .and_then(|t| common::jsonc_model_id(&t))
        .unwrap_or_else(|| "loop-studio/qwen3.8-27b".to_string())
}

/// Active backend according to the router (best-effort, empty when down).
/// `"none"` is the router saying it has no ready backend, which is not the
/// same as knowing nothing: it measured and found nothing serving, so it is
/// reported as a state rather than dropped.
fn router_active() -> String {
    common::http_get(common::gateway_port(), "/loop-studio/status", 1200)
        .filter(|(code, _)| *code == 200)
        .and_then(|(_, body)| {
            let p = body.find("\"active\"")?;
            let rest = &body[p + 8..];
            let colon = rest.find(':')?;
            let after = rest[colon + 1..].trim_start();
            let q1 = after.find('"')?;
            let tail = &after[q1 + 1..];
            tail.find('"').map(|q2| tail[..q2].to_string())
        })
        .filter(|active| !active.is_empty() && active != "none")
        .unwrap_or_default()
}

/// The router's own host-facts reading, when the gateway is up.
///
/// The router measures thermals and RAM on a timer and serves them in
/// `/loop-studio/status` (see `HostFactsCache` in `loop_studio_router.rs`), so the CLI does not
/// have to spawn `pmset`, `vm_stat` and `sysctl` itself to answer the same
/// question the dashboard was polling. The probes remain the fallback for a
/// gateway that is down — `loop-studio status` is also the standalone stack report.
///
/// Split into a pure parse so the extraction is assertable without a router:
/// the document is hand-rolled JSON, and a `"thermal"` appearing anywhere
/// earlier in the document (a backend named "thermal", say) must not be read as
/// the host fact.
fn parse_router_host_facts(body: &str) -> Option<(Option<f64>, Option<f64>, Option<String>)> {
    // The key, then the next `{`, then the block up to the next `}` — tolerant
    // of the whitespace the router's pretty-printed document does or does not
    // have, and of a `}` inside a string ending the block early only if the
    // router emitted one (it does not: the values are a string and numbers).
    let p = body.find("\"host\"")?;
    let rest = &body[p + "\"host".len()..];
    let brace = rest.find('{')?;
    let after = &rest[brace + 1..];
    let end = after.find('}')?;
    let block = &after[..end];
    // Absent, because the router publishes `null` when the thermal probe could
    // not answer — the same shape as the two RAM fields.
    let thermal = json_string(block, "\"thermal\"");
    let free = json_f64(block, "\"free_ram_gib\"");
    let total = json_f64(block, "\"total_ram_gib\"");
    Some((free, total, thermal))
}

/// A string value for `key` in `block`, or None when absent, `null`, or not a
/// string.
///
/// `null` must be detected before the opening quote is looked for. It was not:
/// with `"thermal": null` the next `"` in the block belongs to the *following*
/// key, so the probe that could not answer was read as the string
/// `"free_ram_gib"` — a thermal level no host has ever reported, invented by
/// the parser, and then printed as a measurement.
fn json_string(block: &str, key: &str) -> Option<String> {
    let p = block.find(key)?;
    let rest = &block[p + key.len()..];
    let colon = rest.find(':')?;
    let after = rest[colon + 1..].trim_start();
    if after.starts_with("null") {
        return None;
    }
    let q1 = after.find('"')?;
    let tail = &after[q1 + 1..];
    let q2 = tail.find('"')?;
    let value = &tail[..q2];
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// A number value for `key` in `block`, or None when absent, `null`, or not a
/// number. A probe that could not answer is `null` in the router's document,
/// and `null` must stay `None` here rather than becoming a confident zero.
fn json_f64(block: &str, key: &str) -> Option<f64> {
    let p = block.find(key)?;
    let rest = &block[p + key.len()..];
    let colon = rest.find(':')?;
    let after = rest[colon + 1..].trim_start();
    if after.starts_with("null") {
        return None;
    }
    let end = after
        .char_indices()
        .find(|(_, c)| !(c.is_ascii_digit() || *c == '.' || *c == '-'))
        .map(|(i, _)| i)
        .unwrap_or(after.len());
    after[..end].parse().ok()
}

fn router_host_facts() -> Option<(Option<f64>, Option<f64>, Option<String>)> {
    common::http_get(common::gateway_port(), "/loop-studio/status", 1200)
        .filter(|(code, _)| *code == 200)
        .and_then(|(_, body)| parse_router_host_facts(&body))
}

fn cmd_status(root: &str, args: &[String]) {
    let json = args.iter().any(|a| a == "--json");
    let gw = common::port_up(common::gateway_port());
    let mlx_port = common::mlx_port();
    let mlx = common::port_up(mlx_port);
    let llama = common::port_up(common::llama_port());
    let ollama = common::port_up(common::ollama_port());
    // The router measures these on a timer and serves them in the status
    // document, so with the gateway up the CLI reads them there instead of
    // spawning three probes of its own. `loop-studio status` is also the standalone
    // stack report, so the local probes remain the fallback when the gateway
    // is down.
    let (ram, total, thermal) = if gw {
        match router_host_facts() {
            Some((free, tot, therm)) => (free, tot, therm),
            None => (
                common::free_ram_gib(),
                common::total_ram_gib(),
                common::thermal_state(),
            ),
        }
    } else {
        (
            common::free_ram_gib(),
            common::total_ram_gib(),
            common::thermal_state(),
        )
    };
    let model = opencode_model(root);
    let active = if gw { router_active() } else { String::new() };

    if json {
        println!(
            "{{\"model\":\"{}\",\"thermal\":{},\"free_ram_gib\":{},\"total_ram_gib\":{},\"gateway\":{},\"mlx\":{},\"mlx_port\":{},\"llama\":{},\"ollama\":{},\"active\":\"{}\"}}",
            common::json_escape(&model),
            // `null` when the probe could not answer, like the two RAM fields.
            // A script that reads this document must not be handed a level
            // nobody measured.
            thermal
                .as_deref()
                .map(|t| format!("\"{}\"", common::json_escape(t)))
                .unwrap_or_else(|| "null".to_string()),
            ram.map(|f| format!("{:.1}", f)).unwrap_or_else(|| "null".to_string()),
            total.map(|f| format!("{:.1}", f)).unwrap_or_else(|| "null".to_string()),
            gw, mlx, mlx_port, llama, ollama,
            common::json_escape(&active),
        );
        return;
    }

    let ram_s = ram
        .map(|f| format!("{:.1} GiB", f))
        .unwrap_or_else(|| "Unknown".to_string());
    println!("================================================================");
    println!("  Loop Studio STACK TELEMETRY — {} (v{})", common::arch_label(), VERSION);
    println!("================================================================");
    println!("  Primary Model  : {}", model);
    println!(
        "  Free RAM       : {}",
        total
            .map(|t| format!("{} (of {:.0} GiB)", ram_s, t))
            .unwrap_or_else(|| ram_s.clone())
    );
    println!("  Thermal State  : {}", common::thermal_label(&thermal));
    if !active.is_empty() {
        println!("  Router Active  : {}", active);
    }
    println!("----------------------------------------------------------------");
    println!("  INFERENCE SERVICES (TCP probe):");
    println!(
        "  - Loop Studio Gateway  : {}",
        if gw { format!("ONLINE (:{})", common::gateway_port()) } else { "OFFLINE".to_string() }
    );
    if mlx {
        println!("  - MLX (Q4 MTP) : ONLINE (:{})", mlx_port);
    } else if !common::mlx_supported() {
        println!("  - MLX (Q4 MTP) : N/A (Apple Silicon only)");
    } else {
        println!("  - MLX (Q4 MTP) : OFFLINE");
    }
    // The port printed is the port that was probed. These three labels used
    // to be literals (8081, 11434, 8000) while the probe beside them honored
    // LOOP_STUDIO_LLAMA_PORT / LOOP_STUDIO_OLLAMA_PORT / LOOP_STUDIO_ROUTER_PORT, so an operator
    // reading the screen was told a lane was up on a port nothing was bound
    // to. `common.rs` already forbids exactly this.
    println!(
        "  - llama (Q8)   : {}",
        if llama {
            format!("ONLINE (:{})", common::llama_port())
        } else {
            "OFFLINE".to_string()
        }
    );
    println!(
        "  - Ollama       : {}",
        if ollama {
            format!("ONLINE (:{})", common::ollama_port())
        } else {
            "OFFLINE".to_string()
        }
    );
    println!("================================================================");

    if !gw && !mlx && !llama && !ollama {
        println!("\nTip: No servers are running. Start one with:");
        if common::mlx_supported() {
            println!("   loop-studio serve mlx    (Speed Q4, native MTP ~45 tok/s)");
        }
        println!("   loop-studio serve llama  (Quality Q8_0)");
        println!("   loop-studio route        (Start gateway on :{})", common::gateway_port());
    }
}

// ------------------------------------------------------------------ doctor ---

struct Check {
    name: &'static str,
    level: &'static str, // ok | warn | fail
    detail: String,
}

/// Pure memory check scaled by total physical RAM (host-independent — use in tests).
/// 8GB Air, 16GB Mac, and 96GB Studio all evaluate explainable healthy/pressure/OOM gates.
fn doctor_memory_check_with(
    total_gib: Option<f64>,
    free_gib: Option<f64>,
) -> (&'static str, String) {
    let free = match free_gib {
        Some(f) => f,
        None => return ("warn", "vm_stat unavailable".to_string()),
    };
    let (red, yellow) = match total_gib {
        Some(total) => {
            let r = (total * 0.04).clamp(1.5, 4.0);
            let y = (total * 0.12).clamp(3.0, 12.0);
            (r, y)
        }
        None => (4.0, 16.0),
    };
    if free >= yellow {
        ("ok", format!("{:.1} GiB free (healthy)", free))
    } else if free >= red {
        ("warn", format!("{:.1} GiB free (moderate pressure)", free))
    } else {
        ("fail", format!("{:.1} GiB free (high OOM risk)", free))
    }
}

fn disk_free_gib(path: &str) -> Option<f64> {
    let out = Command::new("df").arg("-k").arg(path).output().ok()?;
    let txt = String::from_utf8_lossy(&out.stdout);
    let line = txt.lines().nth(1)?;
    let avail: f64 = line.split_whitespace().nth(3)?.parse().ok()?;
    Some(avail / 1048576.0)
}

fn launchd_loaded() -> (bool, bool) {
    let out = Command::new("launchctl")
        .arg("list")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    (
        out.lines().any(|l| l.contains("org.loopstudio.router")),
        out.lines().any(|l| l.contains("org.loopstudio.worker")),
    )
}

fn count_files(dir: &str, ext: &str) -> usize {
    fs::read_dir(dir)
        .map(|it| {
            it.flatten()
                .filter(|e| e.path().extension().map(|x| x == ext).unwrap_or(false))
                .count()
        })
        .unwrap_or(0)
}

/// The `logs` line of `loop-studio doctor`: how much disk this project's logs hold, and
/// the one command that reclaims it.
///
/// `task_bytes` is the per-task agent transcript tree, reported alongside the
/// daemon logs because it is a *different directory* — `~/.lac/task-logs` — and
/// the daemon footprint scan only ever looked at `~/Library/Logs`. So the tree
/// that actually grows fastest on a busy worker was the one number the
/// diagnostic could not see. It is passed in rather than read here so the
/// escalation rule stays testable without a filesystem.
fn doctor_logs_check(bytes: u64, files: usize, task_bytes: u64, task_files: usize) -> Check {
    let mib = bytes as f64 / (1024.0 * 1024.0);
    // Eight generations' worth: bounded per file and three kept, so this is
    // well past "working as designed" and into "reclaimable".
    let over = bytes > common::LOG_MAX_BYTES * 8;
    let task_mib = task_bytes as f64 / (1024.0 * 1024.0);
    // The same rule the daemons get, and the same bound: anything past the
    // retention ceiling is worth naming. It can no longer be past it, because
    // the writer trims — so this is the check for a machine that upgraded, or
    // for a tree that arrived from somewhere else.
    let task_over = task_bytes > (common::TASK_LOG_MAX_BYTES * common::TASK_LOG_KEEP as u64);
    let task_note = if task_over {
        format!(
            "; agent transcripts {task_mib:.1} MiB across {task_files} file(s) in ~/.lac/task-logs — reclaim with `loop-studio logs tasks --purge`"
        )
    } else if task_files > 0 {
        format!(
            "; agent transcripts {task_mib:.1} MiB across {task_files} file(s) in ~/.lac/task-logs"
        )
    } else {
        String::new()
    };
    Check {
        name: "logs",
        level: if over || task_over { "warn" } else { "ok" },
        detail: if over {
            format!(
                "{mib:.1} MiB across {files} file(s) in ~/Library/Logs — reclaim with `loop-studio logs <router|mlx|llama|worker> --purge`{task_note}"
            )
        } else {
            format!("{mib:.1} MiB across {files} file(s) in ~/Library/Logs{task_note}")
        },
    }
}

fn cmd_doctor(root: &str, args: &[String]) -> i32 {
    let json = args.iter().any(|a| a == "--json");
    let mut checks: Vec<Check> = Vec::new();

    checks.push(Check { name: "os", level: "ok", detail: common::os_descr() });

    // What this project's logs hold, unprompted. The daemons bound growth and
    // keep three generations each, so the disk cost is real and grows with
    // uptime — and an operator who never runs `loop-studio logs` would never see it.
    // Advisory: a large log is not a broken stack, and doctor's exit code is
    // what scripts branch on, so this must never be a `fail`.
    {
        let (bytes, files) = common::lac_logs_footprint();
        let (task_bytes, task_files) = common::task_logs_footprint();
        checks.push(doctor_logs_check(bytes, files, task_bytes, task_files));
    }

    let (mem_level, mem_detail) = doctor_memory_check_with(common::total_ram_gib(), common::free_ram_gib());
    checks.push(Check {
        name: "memory",
        level: mem_level,
        detail: mem_detail,
    });

    if common::volume_mounted("/Volumes/AIModels") {
        let df = disk_free_gib("/Volumes/AIModels")
            .map(|f| format!("{:.0} GiB free", f))
            .unwrap_or_else(|| "df unavailable".to_string());
        checks.push(Check {
            name: "model_volume",
            level: "ok",
            detail: format!("/Volumes/AIModels mounted ({})", df),
        });
    } else {
        checks.push(Check {
            name: "model_volume",
            level: "warn",
            detail: "/Volumes/AIModels not mounted; internal fallback ~/.lac/models".to_string(),
        });
    }
    if let Some(df) = disk_free_gib(&env::var("HOME").unwrap_or_else(|_| ".".to_string())) {
        checks.push(Check {
            name: "disk_home",
            level: if df > 20.0 { "ok" } else { "warn" },
            detail: format!("{:.0} GiB free on $HOME volume", df),
        });
    }

    let ollama_models = format!("{}/.ollama/models", common::home_dir());
    match fs::symlink_metadata(&ollama_models) {
        Ok(m) if m.file_type().is_symlink() => {
            let target_ok = fs::read_link(&ollama_models)
                .ok()
                .map(|t| t.exists())
                .unwrap_or(false);
            checks.push(Check {
                name: "ollama_symlink",
                level: if target_ok { "ok" } else { "fail" },
                detail: if target_ok {
                    "points at live model library".to_string()
                } else {
                    "dangling symlink — rerun loop-studio bootstrap".to_string()
                },
            });
        }
        Ok(_) => checks.push(Check {
            name: "ollama_symlink",
            level: "warn",
            detail: "real directory, not symlinked to model volume".to_string(),
        }),
        Err(_) => checks.push(Check {
            name: "ollama_symlink",
            level: "warn",
            detail: "absent (Ollama not set up yet)".to_string(),
        }),
    }

    for (cmd, desc, check) in [
        ("opencode2", "OpenCode V2 harness", "tool_opencode2"),
        ("ollama", "Ollama server", "tool_ollama"),
        ("uv", "Python package manager", "tool_uv"),
        ("rustc", "Rust compiler", "tool_rustc"),
        ("jj", "Jujutsu VCS", "tool_jj"),
    ] {
        let found = common::which(cmd);
        checks.push(Check {
            name: check,
            level: if found.is_some() { "ok" } else { "warn" },
            detail: format!(
                "{} ({})",
                found.unwrap_or_else(|| "missing".to_string()),
                desc
            ),
        });
    }

    for (name, rel) in [("opencode_config", "opencode.jsonc"), ("agent_rules", "docs/AGENTS.md")] {
        let p = format!("{}/{}", root, rel);
        // Back-compat: pre-move checkouts kept AGENTS.md at root.
        let fallback = format!("{}/AGENTS.md", root);
        let ok = fs::metadata(&p).is_ok() || fs::metadata(&fallback).is_ok();
        checks.push(Check {
            name,
            level: if ok { "ok" } else { "fail" },
            detail: p,
        });
    }

    let loops = count_files(&format!("{}/templates/loops", root), "yaml");
    checks.push(Check {
        name: "loop_templates",
        level: if loops >= 4 { "ok" } else { "warn" },
        detail: format!("{} loop templates", loops),
    });
    let skills = fs::read_dir(format!("{}/.opencode/skills", root))
        .map(|it| it.count())
        .unwrap_or(0);
    checks.push(Check {
        name: "skills",
        level: if skills >= 10 { "ok" } else { "warn" },
        detail: format!("{} skills installed", skills),
    });

    // Live service probes (informational unless everything is down).
    // Intel Macs do not support MLX: omit svc_mlx to never advertise it.
    let gw_up = common::port_up(common::gateway_port());
    let mut backends: Vec<(&'static str, u16)> = Vec::new();
    if common::mlx_supported() {
        backends.push(("mlx", common::mlx_port()));
    }
    backends.push(("llama", common::llama_port()));
    backends.push(("ollama", common::ollama_port()));
    let mut any_backend = false;
    for (name, port) in backends {
        let up = common::http_ready(port, 1200);
        any_backend |= up;
        let lvl = if up { "ok" } else { "warn" };
        checks.push(Check {
            name: match name {
                "mlx" => "svc_mlx",
                "llama" => "svc_llama",
                _ => "svc_ollama",
            },
            level: lvl,
            detail: format!("port {} {}", port, if up { "serving" } else { "down" }),
        });
    }
    checks.push(Check {
        name: "svc_gateway",
        level: if gw_up { "ok" } else { "warn" },
        detail: if gw_up {
            format!("gateway :{} accepting", common::gateway_port())
        } else {
            format!("gateway :{} down (start with loop-studio route --daemon)", common::gateway_port())
        },
    });
    if !any_backend {
        let hint = if common::mlx_supported() {
            "no inference backend serving; run loop-studio serve mlx"
        } else {
            "no inference backend serving; run loop-studio serve llama"
        };
        checks.push(Check {
            name: "backends",
            level: "fail",
            detail: hint.to_string(),
        });
    }

    let (router_ld, worker_ld) = launchd_loaded();
    checks.push(Check {
        name: "launchd",
        level: "ok",
        detail: format!(
            "router={} worker={}",
            if router_ld { "loaded" } else { "not loaded" },
            if worker_ld { "loaded" } else { "not loaded" }
        ),
    });

    // Remote-bind fail-closed mirror: non-loopback bind without a token
    // refuses to start at the router; surface it here before launchd does.
    //
    // The loopback test is `common::is_loopback_host`, the same function the
    // router uses. This was a second, case-sensitive definition of the same
    // fact — three literals, no trim, no lowercase — and a diagnostic that
    // disagrees with the thing it diagnoses is worse than no diagnostic:
    // `LOOP_STUDIO_BIND_ADDR=LocalHost` printed `remote_auth  FAIL` and exited 1 while
    // the router started happily, and `::ffff:127.0.0.1` made the router
    // `exit(1)` with *no* `remote_auth` row here at all, because the check only
    // emits a row on the non-loopback arms.
    {
        let bind = env::var("LOOP_STUDIO_BIND_ADDR")
            .or_else(|_| env::var("LOOP_STUDIO_ROUTER_HOST"))
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "127.0.0.1".to_string());
        let loopback = common::is_loopback_host(&bind);
        let token_set = env::var("LOOP_STUDIO_API_TOKEN")
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        if !loopback && !token_set {
            checks.push(Check {
                name: "remote_auth",
                level: "fail",
                detail: format!(
                    "LOOP_STUDIO_BIND_ADDR={} without LOOP_STUDIO_API_TOKEN — router will refuse to start; set a token",
                    bind
                ),
            });
        } else {
            // Reported either way. A check that only appears when it has
            // already failed is a check whose absence means nothing.
            checks.push(Check {
                name: "remote_auth",
                level: "ok",
                detail: if loopback {
                    format!("bind {bind} is loopback (no token required)")
                } else {
                    format!("bind {bind} with Bearer token (redacted)")
                },
            });
        }
    }

    let fails = checks.iter().filter(|c| c.level == "fail").count();
    let warns = checks.iter().filter(|c| c.level == "warn").count();

    // History: one JSONL line per run so RAM/issues trends are visible.
    let hist_path = format!("{}/.lac/doctor-history.jsonl", common::home_dir());
    let prev_free: Option<f64> = fs::read_to_string(&hist_path).ok().and_then(|c| {
        let last = c.lines().last()?;
        let p = last.find("\"free\":")? + 7;
        last[p..].trim_start().split(',').next()?.trim().parse().ok()
    });
    let free_now = common::free_ram_gib();
    common::append_jsonl(
        &hist_path,
        &format!(
            "{{\"ts\":{},\"free\":{},\"issues\":{},\"warnings\":{}}}",
            common::now_unix(),
            free_now.map(|f| format!("{:.1}", f)).unwrap_or_else(|| "null".to_string()),
            fails,
            warns
        ),
    );

    if json {
        let mut s = String::from("{\"issues\":");
        s.push_str(&fails.to_string());
        s.push_str(",\"warnings\":");
        s.push_str(&warns.to_string());
        s.push_str(",\"checks\":[");
        for (i, c) in checks.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&format!(
                "{{\"name\":\"{}\",\"level\":\"{}\",\"detail\":\"{}\"}}",
                c.name,
                c.level,
                common::json_escape(&c.detail)
            ));
        }
        s.push_str("]}");
        println!("{}", s);
    } else {
        println!("================================================================");
        println!("  Loop Studio DOCTOR — SYSTEM INTEGRITY DIAGNOSTIC (v{})", VERSION);
        println!("================================================================");
        for c in &checks {
            let mark = match c.level {
                "ok" => "✓",
                "warn" => "!",
                _ => "✗",
            };
            println!("  [{}] {:<16} {}", mark, c.name, c.detail);
        }
        if let (Some(pf), Some(cf)) = (prev_free, free_now) {
            let arrow = if cf < pf { "↓" } else if cf > pf { "↑" } else { "→" };
            println!("  [..] ram_trend      {:.1} -> {:.1} GiB free {} (since last doctor)", pf, cf, arrow);
        }
        println!("================================================================");
        if fails == 0 {
            println!("  RESULT: stack healthy ({} warning(s)). Ready.", warns);
        } else {
            println!(
                "  RESULT: {} failure(s), {} warning(s). Run `loop-studio bootstrap` to repair.",
                fails, warns
            );
        }
        println!("================================================================");
    }
    fails as i32
}

// ------------------------------------------------------------------- bench -

fn content_len_approx(body: &str) -> Option<usize> {
    // First "content":"..." value, escape-aware length.
    let key = "\"content\":\"";
    let p = body.find(key)?;
    let mut len = 0usize;
    let mut esc = false;
    for c in body[p + key.len()..].chars() {
        if esc {
            len += 1;
            esc = false;
        } else if c == '\\' {
            esc = true;
        } else if c == '"' {
            return Some(len);
        } else {
            len += c.len_utf8();
        }
    }
    None
}

/// One chat-completion probe against a backend. Shared by `bench`
/// (single port, verbose) and `tune` (all lanes, comparative).
struct Probe {
    ttft_s: Option<f64>,
    total_s: f64,
    toks: Option<f64>,
}

/// Tunable probe shape: `loop-studio bench --tokens 128 --temp 0.2`,
/// `loop-studio tune --apply --tokens 64 --temp 0.0`. Env fallback
/// LOOP_STUDIO_BENCH_TOKENS / LOOP_STUDIO_BENCH_TEMP keeps scripts stable.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ProbeOpts {
    tokens: u32,
    temp: f64,
}

/// The port argument of `loop-studio bench` / `loop-studio tune`, or the documented default.
///
/// The old caller took the first token that did not begin with `--` and parsed
/// it as a port. That is the *value* of a flag: `loop-studio bench --tokens 128`
/// (a shape this file's own help line advertises, and the shape
/// `LOOP_STUDIO_BENCH_TOKENS` exists to mirror) probed `127.0.0.1:128`, printed
/// `Connection failed to port 128`, and exited 1 — a benchmark of nothing, on
/// a perfectly healthy stack. A flag's value has to be skipped, not probed.
///
/// Zero is refused for the same reason as everywhere else: `common::port_from`
/// is the one rule, and 0 is not a port a client can dial.
fn bench_port(args: &[String], default: u16) -> u16 {
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            // Flags that take the next token as their value.
            "--tokens" | "--max-tokens" | "--max_tokens" | "--temp" | "--temperature" => i += 1,
            s if s.starts_with('-') => {}
            other => return common::port_from(other).unwrap_or(default),
        }
        i += 1;
    }
    default
}

fn parse_probe_opts(args: &[String]) -> ProbeOpts {
    let mut tokens = env::var("LOOP_STUDIO_BENCH_TOKENS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(64u32);
    let mut temp = env::var("LOOP_STUDIO_BENCH_TEMP")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0.0f64);
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--tokens" | "--max-tokens" | "--max_tokens" => {
                if let Some(v) = args.get(i + 1).and_then(|s| s.parse::<u32>().ok()) {
                    tokens = v;
                    i += 1;
                }
            }
            s if s.starts_with("--tokens=") || s.starts_with("--max-tokens=") => {
                if let Some(v) = s.split('=').nth(1).and_then(|x| x.parse::<u32>().ok()) {
                    tokens = v;
                }
            }
            "--temp" | "--temperature" => {
                if let Some(v) = args.get(i + 1).and_then(|s| s.parse::<f64>().ok()) {
                    temp = v;
                    i += 1;
                }
            }
            s if s.starts_with("--temp=") || s.starts_with("--temperature=") => {
                if let Some(v) = s.split('=').nth(1).and_then(|x| x.parse::<f64>().ok()) {
                    temp = v;
                }
            }
            _ => {}
        }
        i += 1;
    }
    ProbeOpts {
        tokens: tokens.clamp(8, 4096),
        temp: if temp.is_finite() { temp.clamp(0.0, 2.0) } else { 0.0 },
    }
}

fn probe_chat(port: u16, read_timeout: Duration, opts: ProbeOpts) -> Option<Probe> {
    let addr = format!("127.0.0.1:{}", port);
    let sa: std::net::SocketAddr = addr.parse().ok()?;
    let body = format!(
        "{{\"model\":\"qwen3.8-27b\",\"messages\":[{{\"role\":\"user\",\"content\":\"Respond with exactly three lines of text about high performance computing.\"}}],\"max_tokens\":{},\"temperature\":{}}}",
        opts.tokens, opts.temp
    );
    let post_req = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        addr, body.len(), body
    );
    let t_inf = Instant::now();
    let mut ttft: Option<Duration> = None;
    let mut inf_resp: Vec<u8> = Vec::new();
    let mut s = TcpStream::connect_timeout(&sa, Duration::from_secs(10)).ok()?;
    let _ = s.set_read_timeout(Some(read_timeout));
    s.write_all(post_req.as_bytes()).ok()?;
    let mut chunk = [0u8; 4096];
    loop {
        match s.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if ttft.is_none() {
                    ttft = Some(t_inf.elapsed());
                }
                // Bounded: a peer that keeps talking must not grow this
                // without limit for the length of the 300 s window.
                if (inf_resp.len() as u64) < HTTP_PROBE_CAP {
                    let room = (HTTP_PROBE_CAP - inf_resp.len() as u64) as usize;
                    inf_resp.extend_from_slice(&chunk[..n.min(room)]);
                }
            }
            Err(_) => break,
        }
    }
    if ttft.is_none() && inf_resp.is_empty() {
        return None;
    }
    let total = t_inf.elapsed();
    let inf_str = String::from_utf8_lossy(&inf_resp);
    let toks = content_len_approx(&inf_str)
        .map(|clen| (clen as f64 / 4.0).max(1.0) / total.as_secs_f64().max(0.01));
    Some(Probe {
        ttft_s: ttft.map(|t| t.as_secs_f64()),
        total_s: total.as_secs_f64(),
        toks,
    })
}

/// A probe of a lane that answers HTTP must finish or fail inside this. The
/// point is that "listening but silent" is a state a CLI has to be able to
/// report, not one it can sit in.
const BENCH_HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Ceiling on a probe's response accumulation, matching `common::http_get`.
const HTTP_PROBE_CAP: u64 = 4 * 1024 * 1024;

fn cmd_bench(port: u16, args: &[String]) {
    let opts = parse_probe_opts(args);
    println!(
        "=== Loop Studio Benchmark Probe (http://127.0.0.1:{}) [tokens={} temp={}] ===",
        port, opts.tokens, opts.temp
    );

    let addr = format!("127.0.0.1:{}", port);
    let sa: Option<std::net::SocketAddr> = addr.parse().ok();
    let socket = sa.and_then(|sa| TcpStream::connect_timeout(&sa, Duration::from_millis(2000)).ok());

    match socket {
        Some(mut stream) => {
            let req = format!("GET /v1/models HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n", addr);
            let t0 = Instant::now();
            let _ = stream.write_all(req.as_bytes());
            // This socket had no read timeout and an unbounded `read_to_end`.
            // Against anything that accepts and then says nothing — an ssh
            // tunnel, a docker-proxy port, a half-dead server — `loop-studio bench`
            // never returned: no output, no error, no exit. And a peer that
            // streams instead of stalling grew `resp` for as long as it liked.
            let _ = stream.set_read_timeout(Some(BENCH_HTTP_TIMEOUT));
            let _ = stream.set_write_timeout(Some(BENCH_HTTP_TIMEOUT));
            let mut resp = Vec::new();
            let read = (&mut stream).take(HTTP_PROBE_CAP).read_to_end(&mut resp);
            println!(
                "  - Socket roundtrip (TTFB): {:.2} ms",
                t0.elapsed().as_secs_f64() * 1000.0
            );
            if read.is_err() && resp.is_empty() {
                eprintln!(
                    "  [x] No response from :{} within {:?} — the port is listening but not answering HTTP.",
                    port, BENCH_HTTP_TIMEOUT
                );
                std::process::exit(1);
            }
            let resp_str = String::from_utf8_lossy(&resp);
            let status = resp_str.lines().next().unwrap_or("Unknown");
            println!("  - Health Status: {}", status);

            if status.contains("200") {
                println!("\nRunning inference benchmark (chat completion test)...");
                match probe_chat(port, Duration::from_secs(300), opts) {
                    Some(p) => {
                        if let Some(t) = p.ttft_s {
                            println!("  - TTFT (first byte): {:.2} s", t);
                        }
                        println!("  - Inference Total Time: {:.2} s", p.total_s);
                        match p.toks {
                            Some(t) => println!(
                                "  - Estimated Generation Speed: {:.1} tokens/sec (rough: ~4 chars/token)",
                                t
                            ),
                            None => println!("  - No content field found in response (backend may have errored)."),
                        }
                    }
                    None => println!("  - Probe failed: no response bytes."),
                }
            }
        }
        None => {
            // A bench that could not reach the lane is a failure, not a
            // result: this used to print the failure and exit 0, so a bench
            // script passed on a dead port.
            eprintln!("  [x] Connection failed to port {}. Service is offline.", port);
            std::process::exit(1);
        }
    }
}

/// Benchmark every live lane and rank them. `--apply` pins the winner as
/// the router's preferred backend (persisted + hot-swapped when the
/// gateway is up). Throughput picks the winner; TTFT breaks ties.
/// Note: lanes differ in quality (Q4 speed vs Q8 fidelity) — tune ranks
/// speed, the operator keeps the quality vote.
fn cmd_tune(args: &[String]) {
    let apply = args.iter().any(|a| a == "--apply");
    let opts = parse_probe_opts(args);
    println!(
        "=== Loop Studio Lane Tune (all live backends, 60s probe budget each) [tokens={} temp={}] ===",
        opts.tokens, opts.temp
    );
    let lanes = [
        ("mlx", common::mlx_port()),
        ("llama", common::llama_port()),
        ("ollama", common::ollama_port()),
    ];
    let mut results: Vec<(&str, u16, Probe)> = Vec::new();
    for (name, port) in lanes {
        if !common::http_ready(port, 1500) {
            println!("  {:<7} :{:<5} down (skipped)", name, port);
            continue;
        }
        print!("  {:<7} :{:<5} probing... ", name, port);
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
        match probe_chat(port, Duration::from_secs(60), opts) {
            Some(p) => {
                println!(
                    "TTFT {:.2}s total {:.2}s tok/s {}",
                    p.ttft_s.unwrap_or(-1.0),
                    p.total_s,
                    p.toks.map(|t| format!("{:.1}", t)).unwrap_or_else(|| "-".to_string())
                );
                results.push((name, port, p));
            }
            None => println!("probe failed"),
        }
    }
    if results.is_empty() {
        let hint = if common::mlx_supported() { "loop-studio serve mlx" } else { "loop-studio serve llama" };
        println!("No live backends. Start one with `{}`.", hint);
        return;
    }
    results.sort_by(|a, b| {
        b.2.toks
            .partial_cmp(&a.2.toks)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(
                a.2.ttft_s
                    .partial_cmp(&b.2.ttft_s)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
    });
    println!("----------------------------------------------------------------");
    for (i, (name, port, p)) in results.iter().enumerate() {
        println!(
            "  #{:<2} {:<7} :{:<5} tok/s {} TTFT {:.2}s",
            i + 1,
            name,
            port,
            p.toks.map(|t| format!("{:.1}", t)).unwrap_or_else(|| "-".to_string()),
            p.ttft_s.unwrap_or(-1.0)
        );
    }
    let winner = results[0].0;
    println!("Winner (throughput): {}", winner);
    if apply {
        if !common::persist_or_warn(
            &format!("{}/.lac/router-backend", common::home_dir()),
            winner,
            "tune_persist",
        ) {
            eprintln!("tune winner '{}' not persisted; re-run with --apply.", winner);
        }
        // Hot-swap a live gateway; a down gateway picks the file up later.
        if common::port_up(common::gateway_port()) {
            let url = format!("/loop-studio/switch?target={}", winner);
            match common::http_get(common::gateway_port(), &url, 2000) {
                Some((200, _)) => println!("Router hot-swapped to {} (persisted).", winner),
                _ => println!("Persisted {}; gateway switch unconfirmed.", winner),
            }
        } else {
            println!("Persisted {}; gateway down — applies on next router start.", winner);
        }
    } else {
        println!("Re-run with --apply to pin the winner.");
    }
}

// ----------------------------------------------------------------- thermal -

fn cmd_thermal() {
    println!("================================================================");
    println!("  THERMAL & POWER GOVERNOR");
    println!("================================================================");
    let state = common::thermal_state();
    println!("  Current Thermal Level : {}", common::thermal_label(&state));
    println!("\n  Telemetry Detail:\n{}", common::thermal_detail().trim());
    println!("----------------------------------------------------------------");
    match state.as_deref() {
        Some("Nominal") => println!("  Recommendation: Optimal headroom. Full Q8_0 and MTP enabled."),
        Some("Fair") => println!("  Recommendation: Moderate heat. Monitor TTFT; keep vents clear."),
        Some("Serious") => println!("  Recommendation: Throttling near. Swap Q8->Q4; defer long loops."),
        Some("Critical") => println!("  Recommendation: Overheat risk! Throttle Q8->Q4 now or pause 5 min."),
        // The probe did not answer. Say so, and say what it costs: nothing
        // below can claim headroom, and the remedy is to read the raw detail
        // above, which is what this command prints for exactly this reason.
        None => println!(
            "  Recommendation: UNKNOWN — pmset did not answer, so no level below is a \
             measurement. Check the raw detail above before trusting a wide context."
        ),
        Some(_) => {}
    }
    println!("================================================================");
}

// --------------------------------------------------------------------- cap -

fn cap_path() -> String {
    format!("{}/.lac/context-cap", common::home_dir())
}

fn cmd_cap(args: &[String]) {
    println!("=== Loop Studio Context Cap Hygiene ===");
    if args.len() >= 2 && args[0] == "--set" {
        let cap = &args[1];
        match cap.parse::<u64>() {
            Ok(n) => {
                if common::persist_or_warn(&cap_path(), &n.to_string(), "context_cap") {
                    println!("Persisted active context cap: {} tokens ({}).", n, cap_path());
                }
            }
            Err(_) => println!("Invalid cap '{}': expected integer tokens.", cap),
        }
        println!("Steady-state recommendation: 16,000 tokens (emergency truncate at 24,000).");
    } else {
        let saved = fs::read_to_string(cap_path())
            .ok()
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "(unset)".to_string());
        println!("Persisted cap ({}): {}", cap_path(), saved);
        println!("  - Native Served Context : 65,536 tokens");
        println!("  - Steady-state Session Cap : 16,000 tokens (`context-cap`)");
        println!("  - Emergency Truncation Trigger : 24,000 tokens (`kv-manage`)");
    }
}

// ------------------------------------------------------------------ hermes -
// Native Hermes orchestrator: judgment-augmented task selection over the
// same queue the 24/7 worker drains. There is no external Python process —
// `loop-studio hermes status` reports native state and `loop-studio hermes run` delegates
// to cmd_worker, which picks tasks via judge_next() (LLM judgment with a
// deterministic fallback, never less reliable than file order).

fn cmd_hermes(root: &str, args: &[String]) {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("status");
    match sub {
        "status" => {
            println!("=== Hermes Orchestrator (native) ===");
            let home = common::home_dir();
            let tasks_file = common::tasks_file();
            let content = fs::read_to_string(&tasks_file).unwrap_or_default();
            let (pending, dead) = count_pending(&content);
            let total = parse_tasks(&content).len();
            println!("  Queue   : {} ({} total, {} retryable pending, {} dead-letter)", tasks_file, total, pending, dead);
            println!("  Gateway : http://127.0.0.1:{}/v1 ({})", common::gateway_port(), if common::port_up(common::gateway_port()) { "online" } else { "offline" });
            println!("  Thermal : {}", common::thermal_label(&common::thermal_state()));
            println!("  Model   : {}", opencode_model(root));
            println!("  Loops   : {}/todo/loop-studio-loops", home);
            println!("  Worker  : loop-studio worker [--drain] (judgment-augmented selection; deterministic fallback)");
        }
        "run" | "drain" | "worker" => {
            // Native dispatch: same loop as `loop-studio worker`. Pass through
            // --drain/--once when present; bare `loop-studio hermes run` runs the
            // continuous 24/7 watch, exactly like bare `loop-studio worker`.
            let rest: Vec<String> = args.iter().skip(1).cloned().collect();
            cmd_worker(root, &rest);
        }
        _ => println!("Usage: loop-studio hermes [status|run [--drain|--once]]"),
    }
}

// -------------------------------------------------------------------- loop -

fn cmd_loop(root: &str, args: &[String]) {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("list");
    let loops_dir = common::loops_dir();

    match sub {
        "init" => {
            println!("Initializing ~/todo/loop-studio-loops and ~/todo/loop-studio-tasks.yaml ...");
            // Every copy is checked and only the ones that landed are named.
            // This is the worker's pre-flight, so a "successful" init that
            // installed nothing left `loop-studio worker` looping on a no-op forever
            // (or, in --drain mode, exiting 0 having drained nothing).
            let mut failures: Vec<String> = Vec::new();
            if let Err(e) = fs::create_dir_all(&loops_dir) {
                eprintln!("  [x] Cannot create {}: {}", loops_dir, e);
                std::process::exit(1);
            }
            let templates_dir = format!("{}/templates/loops", root);
            // Never overwrite user-edited loops: only install missing files.
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
                            let n: Option<u32> = tail
                                .split_whitespace()
                                .nth(1)
                                .and_then(|s| s.parse().ok());
                            match n {
                                Some(v) if v > 2 => {
                                    println!("  [!] max_rounds={} exceeds 2-round policy (AGENTS.md)", v);
                                    warns += 1;
                                }
                                None => {
                                    println!("  [!] max_rounds unparsable; default 2 applies");
                                    warns += 1;
                                }
                                _ => {}
                            }
                        }
                        None => println!("  [!] Notice: 'max_rounds:' not specified (default 2 applies)"),
                    }
                    if !content.contains("require_human_gate: true") {
                        println!("  [!] Notice: human gate not enforced (require_human_gate: true)");
                        warns += 1;
                    }
                    // A validator a script cannot branch on is a printout.
                    // This is the one command whose whole job is to be a gate:
                    // `loop-studio loop validate bad.yaml && deploy` used to deploy,
                    // because the errors were printed and the exit was 0.
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
            let target = args.iter().skip(1).find(|a| !a.starts_with("--")).map(|s| s.as_str());
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

/// The gate verdict for one run, decided once and printed from.
struct TestGate {
    /// `cargo test` was actually run and passed.
    passed: bool,
    /// One line saying what did or did not happen, for the operator.
    detail: String,
}

/// Decide the test gate for a run. Every branch states what it did.
///
/// This used to have an `else` that printed `✓ Tests passed (100% pass rate)`
/// — the branch taken by `--dry-run` and by every checkout without
/// `rust-src/Cargo.toml`. So the one path that never ran a test was the path
/// that claimed a perfect score, and the gate then reported
/// `Reliability gates passed` on the strength of it.
fn run_test_gate(root: &str, dry_run: bool) -> TestGate {
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

/// What a `git` invocation produced, with "it did not run" kept distinct from
/// "it ran and the output was empty".
///
/// Three call sites in the loop runner unwrapped git output with
/// `.unwrap_or_default()`, so a `git` that could not be spawned — not on PATH,
/// or `root` not a repository — produced `""`, and `""` is exactly what a clean
/// working tree produces. The command then reported a clean tree and a zero
/// modified-file count as measurements of a repository it had never asked.
enum GitOutput {
    Text(String),
    Failed(String),
}

impl GitOutput {
    /// Did this git invocation do what it was asked to? For a mutating call
    /// (branch, commit, stash, worktree) the output is not interesting — the
    /// exit status is the whole answer.
    fn succeeded(&self) -> bool {
        matches!(self, GitOutput::Text(_))
    }
}

fn cmd_loop_run(root: &str, target: Option<&str>, dry_run: bool) -> i32 {
    let tasks_file = common::tasks_file();

    println!("\x1b[1;36m=== Loop Studio Autonomous Loop Runner ===\x1b[0m");

    let tasks_content = fs::read_to_string(&tasks_file).unwrap_or_default();
    let tasks = parse_tasks(&tasks_content);

    let (task_id, task_desc, loop_name, from_queue) = if let Some(t) = target {
        if t.ends_with(".yaml") {
            let name = t.trim_end_matches(".yaml");
            (format!("loop-{}", name), format!("Execute standalone loop {}", t), t.to_string(), false)
        } else if let Some(matched) = tasks.iter().find(|item| item.id == t) {
            (matched.id.clone(), matched.desc.clone(), "daily-coding.yaml".to_string(), true)
        } else {
            ("loop-studio-adhoc".to_string(), t.to_string(), "daily-coding.yaml".to_string(), false)
        }
    } else if let Some(first_pending) = tasks.iter().find(|t| t.status == "pending") {
        (first_pending.id.clone(), first_pending.desc.clone(), "daily-coding.yaml".to_string(), true)
    } else {
        ("loop-studio-001".to_string(), "Verify local model stack and run smoke tests via gateway :8000".to_string(), "daily-coding.yaml".to_string(), false)
    };

    println!("  Task ID: \x1b[1m{}\x1b[0m", task_id);
    println!("  Task:    {}", task_desc);
    println!("  Loop:    {} (enforcing AGENTS.md single-resident model & human gate)", loop_name);
    if dry_run {
        println!("  Mode:    \x1b[33m--dry-run (simulation)\x1b[0m");
    }
    println!();

    // -----------------------------------------------------------------
    // Phase 1: Preflight & Hygiene
    // -----------------------------------------------------------------
    println!("\x1b[1;34m[1/5 PREFLIGHT]\x1b[0m Verifying resident model, context cap & thermals...");
    let gateway_up = common::port_up(common::gateway_port());
    if gateway_up {
        println!("  ✓ Gateway :{} online", common::gateway_port());
    } else {
        println!("  ! Gateway :{} inactive. Running offline simulation.", common::gateway_port());
    }
    // These two were constants in a println: the cap was "verified" without
    // being read and the thermals were "Nominal" without a single call. Both
    // are measurements now, so a Critical reading shows up here instead of
    // being papered over by a green tick.
    let cap = fs::read_to_string(cap_path())
        .ok()
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| "unset (policy: 16,000)".to_string());
    println!("  · Context cap: {} ({})", cap, cap_path());
    let thermal = common::thermal_state();
    // `?` is a reading nobody could take, so the tick is withheld rather than
    // faked. The gate below still has to answer the question "is it too hot to
    // keep working", and an absent reading is not an answer to it — refusing
    // here would deadlock every run on a host where pmset is unavailable, and
    // proceeding is what the same rule already does for a missing RAM reading.
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
    // -----------------------------------------------------------------
    // Phase 2: Implementation Phase (Agent: coder)
    // -----------------------------------------------------------------
    println!("\n\x1b[1;34m[2/5 IMPLEMENT]\x1b[0m Agent: coder | Task scope locked");
    println!("  Task instruction: \"{}\"", task_desc);
    // Said plainly, because nothing in this command writes code: the
    // implementation phase belongs to `loop-studio worker` (or the loop-orchestrator
    // skill), which dispatches the agent inside a `task/<id>` worktree.
    println!("  · This command dispatches no agent and writes no files.");

    // -----------------------------------------------------------------
    // Phase 3: Review Phase (Agent: @reviewer)
    // -----------------------------------------------------------------
    println!("\n\x1b[1;34m[3/5 REVIEW]\x1b[0m Agent: @reviewer (read-only, edit denied)");
    println!("  Auditing uncommitted diff against invariants & policy (max 2 rounds)...");
    // A count of what is in the tree is a measurement; "0 Critical, 0 Major"
    // was a verdict from a reviewer that never ran.
    let mod_count = match git_in(root, &["status", "-s"]) {
        GitOutput::Text(t) => {
            let n = t.lines().filter(|l| !l.trim().is_empty()).count();
            println!("  Working tree: {} modified/untracked file(s)", n);
            n
        }
        GitOutput::Failed(why) => {
            // Reported, because "0 modified files" from a git that never ran is
            // a claim about the repository rather than an admission.
            eprintln!("  [x] Working tree could not be read — {why}");
            0
        }
    };

    // -----------------------------------------------------------------
    // Phase 4: Test Gate (Tests-as-Gate)
    // -----------------------------------------------------------------
    println!("\n\x1b[1;34m[4/5 TEST GATE]\x1b[0m Executing test reliability gates...");
    let gate = run_test_gate(root, dry_run);
    if gate.passed {
        println!("  {}", gate.detail);
        println!("\x1b[32m  PASSED\x1b[0m");
    } else {
        // Fail closed, the way every other gate in this project does: a gate
        // that did not run has not passed. This is the branch `--dry-run` and
        // every checkout without `rust-src/Cargo.toml` used to fall into,
        // where it printed ✓ Tests passed (100% pass rate) with no cargo
        // anywhere in the process.
        eprintln!(
            "\x1b[1;31m[GATE REJECTED]\x1b[0m Test gate {} — halting before the human gate per AGENTS.md.",
            gate.detail
        );
        return 1;
    }

    // -----------------------------------------------------------------
    // Phase 5: Human Approval Gate (Mandatory per AGENTS.md)
    // -----------------------------------------------------------------
    println!("\n\x1b[1;35m[5/5 HUMAN APPROVAL GATE]\x1b[0m Mandatory per AGENTS.md");
    println!("  -------------------------------------------------------------");
    // Said first, because it governs how the rest of the block reads. The gate
    // exists to approve work an agent did; this run did none, so the diff
    // below is whatever was already uncommitted when it started. "Code is
    // uncommitted in working tree" implied the tree had just been changed by
    // the run that is reporting on it.
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
        // A `git` that never ran used to be reported here as "Working tree
        // clean": the output was unwrapped to `""`, and an empty string and a
        // clean tree are the same string.
        GitOutput::Failed(why) => {
            eprintln!("  [x] Working tree unknown — {why}");
        }
    }

    println!("  -------------------------------------------------------------");
    println!("\x1b[1;33m[REHEARSAL COMPLETE]\x1b[0m Phases 1-4 were checked. Nothing was implemented,");
    println!("  nothing was reviewed, and no agent was dispatched. To do the work, run");
    println!("  `loop-studio worker --drain` (or the loop-orchestrator skill), which dispatches the");
    println!("  agent inside a task/<id> worktree and is what may retire this task.");

    // The queue. This used to write `status: complete`, which retired a pending
    // task by a command that stated two phases earlier that it dispatches no
    // agent and writes no files. `loop-studio worker` then could never pick the task up
    // again, so the task was not done — it was deleted, and the run that
    // deleted it printed `[LOOP COMPLETED]`.
    if from_queue {
        println!(
            "  · Queue untouched: [{}] is still pending in {}",
            task_id, tasks_file
        );
    }
    0
}

// ------------------------------------------------------------------ daemon -

fn launchd_src(root: &str, name: &str) -> String {
    format!("{}/launchd/{}.plist", root, name)
}

fn launchctl_bootstrap(dst: &str) -> bool {
    // Modern macOS prefers bootstrap; fall back to legacy load -w. The result
    // is reported: the old version returned `()`, so a plist that launchd
    // refused — a bad path, a malformed file, a domain that is not there —
    // left the command printing "installed" and exiting 0.
    let uid = Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "501".to_string());
    let domain = format!("gui/{}", uid);
    let ok = Command::new("launchctl")
        .args(["bootstrap", &domain, dst])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        return true;
    }
    Command::new("launchctl")
        .args(["load", "-w", dst])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Unload a launchd agent, trying the modern spelling and then the legacy
/// ones. Returns whether the agent is actually gone from launchd.
fn launchctl_bootout(dst: &str, label: &str) -> bool {
    let uid = Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "501".to_string());
    let service_target = format!("gui/{}/{}", uid, label);
    let domain = format!("gui/{}", uid);
    let ok = Command::new("launchctl")
        .args(["bootout", &service_target])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        // The legacy spellings, for a plist written by an older install. Each
        // is tried and each result used: the caller has to be able to tell
        // "unloaded" from "still running", which it could not when all three
        // results were dropped on the floor.
        for attempt in [
            vec!["bootout", &domain, dst],
            vec!["unload", "-w", dst],
            vec!["remove", label],
        ] {
            if Command::new("launchctl")
                .args(&attempt)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
            {
                return true;
            }
        }
        return false;
    }
    true
}

fn cmd_daemon(root: &str, args: &[String]) {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("status");
    let home = common::home_dir();
    let agent_dir = format!("{}/Library/LaunchAgents", home);
    let log_dir = format!("{}/Library/Logs", home);

    match sub {
        "install" => {
            println!("Installing Loop Studio 24/7 Daemons as macOS LaunchAgents...");
            if let Err(e) = fs::create_dir_all(&agent_dir) {
                eprintln!("  [x] Cannot create {}: {}", agent_dir, e);
                std::process::exit(1);
            }
            let _ = fs::create_dir_all(&log_dir);
            let mut failures = 0usize;
            for label in ["org.loopstudio.router", "org.loopstudio.worker"] {
                let src = launchd_src(root, label);
                let dst = format!("{}/{}.plist", agent_dir, label);
                match fs::read_to_string(&src) {
                    Ok(content) => {
                        // Retarget the plist at THIS machine (see
                        // `common::retarget_launchd_plist`): home, binary,
                        // working directory, and log paths.
                        let installed = format!("{}/.local/bin/loop-studio", home);
                        let installed_router = format!("{}/.local/bin/loop-studio-router", home);
                        let release_lac = common::bin(root, "loop-studio");
                        let release_router = common::bin(root, "loop-studio-router");
                        // A plist whose ProgramArguments[0] cannot execute is a
                        // daemon that respawns a failure forever, so "the file
                        // is there" is not the question — "it can run" is.
                        let program = if label == "org.loopstudio.router" {
                            common::choose_program_binary(&installed_router, &release_router)
                        } else {
                            common::choose_program_binary(&installed, &release_lac)
                        };
                        if let Some(warning) = &program.warning {
                            eprintln!("  [!] {}", warning);
                        }
                        if program.kind == common::ProgramKind::Missing {
                            // A plist whose ProgramArguments[0] is a path to
                            // nothing loads fine and then respawns a failure
                            // forever, and this installer is the only thing
                            // standing between that and `[ok] Placed:`.
                            eprintln!("  [x] {} has no runnable binary — not placing it", dst);
                            failures += 1;
                            continue;
                        }
                        let want_bin = program.path;
                        let mut out = common::retarget_launchd_plist(&content, &home, root, &want_bin);
                        if label == "org.loopstudio.router" {
                            let port = common::gateway_port().to_string();
                            // Matched by key, and required to have matched: the
                            // old literal-needle replace silently did nothing
                            // on the shipped template, so a relocated port left
                            // the launchd router on 8000 while the whole CLI
                            // addressed 9000 and reported the gateway down.
                            match common::set_plist_string(&out, "LOOP_STUDIO_ROUTER_PORT", &port) {
                                Some(retargeted) => out = retargeted,
                                None => {
                                    eprintln!(
                                        "  [x] {} has no LOOP_STUDIO_ROUTER_PORT to retarget to :{} — not placing it",
                                        src, port
                                    );
                                    failures += 1;
                                    continue;
                                }
                            }
                        }
                        match common::atomic_write(&dst, &out) {
                            Ok(()) => println!("  [ok] Placed: {}", dst),
                            Err(e) => {
                                // Loading a plist that is not on disk would
                                // leave whatever was there before in charge.
                                eprintln!("  [x] Write {} failed: {}", dst, e);
                                failures += 1;
                                continue;
                            }
                        }
                        launchctl_bootout(&dst, label); // idempotent reinstall
                        if !launchctl_bootstrap(&dst) {
                            eprintln!("  [x] launchctl could not load {} — check `loop-studio logs`", dst);
                            failures += 1;
                        }
                    }
                    Err(e) => {
                        eprintln!("  [!] Source plist missing: {} ({})", src, e);
                        failures += 1;
                    }
                }
            }
            println!("Worker log: ~/Library/Logs/loop-studio-worker.log");
            println!("Router log: ~/Library/Logs/loop-studio-router.log");
            if failures > 0 {
                eprintln!("  [x] {} agent(s) did not install cleanly", failures);
                std::process::exit(1);
            }
        }
        "uninstall" => {
            println!("Unloading Loop Studio 24/7 Daemons...");
            // Every step of this reports itself, and the command fails if any
            // of them did not happen. It used to print
            // "Loop Studio daemons uninstalled." and exit 0 unconditionally, after
            // discarding the result of all three `launchctl` fallbacks and of
            // the plist `remove_file`. From a session with no `gui/$UID` domain
            // — a non-GUI login, an ssh shell, a CI step — the agents keep
            // running under launchd while their plists are deleted, and
            // `loop-studio daemon status` then reports "NOT LOADED (plist absent)" for
            // processes that are very much alive.
            let mut failures = 0;
            for label in ["org.loopstudio.worker", "org.loopstudio.router"] {
                let dst = format!("{}/{}.plist", agent_dir, label);
                if !launchctl_bootout(&dst, label) {
                    eprintln!(
                        "  [x] launchctl could not unload {label} — it may still be running"
                    );
                    failures += 1;
                }
                match fs::remove_file(&dst) {
                    Ok(()) => println!("  [ok] Removed {}", dst),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        eprintln!("  [x] Could not remove {}: {}", dst, e);
                        failures += 1;
                    }
                }
            }
            if failures > 0 {
                eprintln!("  [x] {} agent(s) did not uninstall cleanly", failures);
                std::process::exit(1);
            }
            println!("Loop Studio daemons uninstalled.");
        }
        "status" => {
            println!("================================================================");
            println!("  Loop Studio LAUNCHD DAEMON STATUS");
            println!("================================================================");
            let out = Command::new("launchctl")
                .arg("list")
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
                .unwrap_or_default();
            for label in ["org.loopstudio.router", "org.loopstudio.worker"] {
                let running = out.lines().any(|l| l.contains(label));
                let plist = format!("{}/{}.plist", agent_dir, label);
                let placed = fs::metadata(&plist).is_ok();
                println!(
                    "  - {} : {} ({})",
                    label,
                    if running { "ACTIVE (running)" } else { "NOT LOADED" },
                    if placed { "plist placed" } else { "plist absent" }
                );
            }
            println!("----------------------------------------------------------------");
            println!("  Worker: ~/Library/Logs/loop-studio-worker.log");
            println!("  Router: ~/Library/Logs/loop-studio-router.log");
            println!("================================================================");
        }
        _ => println!("Unknown daemon command: {}. Use: install, uninstall, status", sub),
    }
}

// ------------------------------------------------------------- task queue --

#[derive(Debug, Clone)]
struct Task {
    id: String,
    desc: String,
    status: String,
    attempts: u32,
}

fn line_indent(s: &str) -> usize {
    s.chars().take_while(|c| *c == ' ' || *c == '\t').count()
}

fn block_key_indent(block: &str) -> usize {
    block
        .lines()
        .find(|l| l.trim_start().starts_with("- id:"))
        .and_then(|l| l.find("id:"))
        .unwrap_or(0)
}

/// Key lookup inside one `- id:` block. The `- ` list marker occupies
/// two columns, so sibling keys (`task:`, `status:`, `attempts:`) sit at
/// the `id` column while `metadata:` children sit deeper. Only matches
/// at that column count, so nested decoys can never hijack the match.
/// Strip the quoting a writer may have put around a scalar.
///
/// Both quote styles are accepted, and both now have to be. The queue file has
/// two writers: this worker, and Loop Studio. The worker emitted `"…"`, and the
/// Studio emitted `"…"` too — until it was fixed to emit `'…'`, because a
/// double-quoted YAML scalar may not contain an unescaped `"` or span lines, and
/// a task description routinely contains both. So the writer changed and this
/// reader did not, and the disagreement was immediate and total: `- id: 'a-1'`
/// read back as the id `'a-1'`, quotes included, which then flowed into the
/// transcript path, the branch name and `update_task`'s own matching, so the
/// worker could not find the task it had just written.
///
/// A writer is not allowed to publish a value its own reader discards, and the
/// reverse is just as true: a reader is not allowed to disagree with the only
/// two writers of the file it reads.
fn unquote(value: &str) -> String {
    let v = value.trim();
    if v.len() >= 2 {
        let bytes = v.as_bytes();
        let first = bytes[0] as char;
        let last = bytes[v.len() - 1] as char;
        if (first == '"' && last == '"') || (first == '\'' && last == '\'') {
            return v[1..v.len() - 1].to_string();
        }
    }
    v.to_string()
}

fn block_field(block: &str, key: &str) -> Option<String> {
    if key == "- id:" {
        for line in block.lines() {
            let l = line.trim();
            if l.starts_with(key) {
                let v = unquote(&l[key.len()..]);
                if !v.is_empty() {
                    return Some(v);
                }
            }
        }
        return None;
    }
    let base = block_key_indent(block);
    for line in block.lines() {
        if line_indent(line) != base {
            continue;
        }
        let l = line.trim();
        if l.starts_with(key) {
            let v = unquote(&l[key.len()..]);
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    None
}

/// Allowlist for task ids. Ids flow into a transcript path
/// (`~/.lac/task-logs/<id>-<ts>.log`), a git branch (`task/<id>`), and a
/// commit message — all from YAML the worker does not fully control. Only
/// `[A-Za-z0-9][A-Za-z0-9._-]{0,64}` passes, so `../../evil`, absolute
/// paths, spaces, and control chars can never reach the fs or git.
fn sanitize_task_id(id: &str) -> Option<String> {
    if id.is_empty() || id.len() > 65 {
        return None;
    }
    let mut chars = id.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return None,
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-') {
        return None;
    }
    Some(id.to_string())
}

/// Parse `- id:` blocks. Tolerates missing fields; blocks with an unsafe
/// id are dropped here so no caller can ever interpolate them into a
/// path, branch, or commit message. Unknown blocks are preserved verbatim
/// by `update_task` (which edits line ranges, so it never corrupts
/// neighbors).
fn parse_tasks(content: &str) -> Vec<Task> {
    let mut tasks = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    let flush = |cur: &mut Vec<&str>, tasks: &mut Vec<Task>| {
        if cur.is_empty() {
            return;
        }
        let block = cur.join("\n");
        if let (Some(id), Some(desc)) = (block_field(&block, "- id:"), block_field(&block, "task:"))
        {
            match sanitize_task_id(&id) {
                Some(id) => tasks.push(Task {
                    id,
                    desc,
                    status: block_field(&block, "status:").unwrap_or_else(|| "pending".to_string()),
                    attempts: block_field(&block, "attempts:")
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0),
                }),
                None => {
                    eprintln!("Skipping task block with unsafe id: {:?}", id);
                    common::log_event("task_rejected", &format!("unsafe id rejected: {:?}", id));
                }
            }
        }
        cur.clear();
    };
    for line in content.lines() {
        if line.trim_start().starts_with("- id:") && !cur.is_empty() {
            let mut tmp = std::mem::take(&mut cur);
            flush(&mut tmp, &mut tasks);
            cur = tmp;
        }
        cur.push(line);
    }
    flush(&mut cur, &mut tasks);
    tasks
}

/// Rewrite every block whose `- id:` matches: set status and optionally
/// bump attempts, inserting whichever key is absent (a status-less block
/// otherwise becomes an unfinishable infinite retry). Key matches are
/// indent-anchored to the block level (see `block_field`); all other
/// bytes are untouched — the old global `replacen` could flip a
/// different task that happened to sort first.
fn update_task(content: &str, id: &str, new_status: &str, bump_attempts: bool) -> String {
    let lines: Vec<&str> = content.lines().collect();
    // Locate block boundaries.
    let mut starts: Vec<usize> = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        if l.trim_start().starts_with("- id:") {
            starts.push(i);
        }
    }
    // All blocks sharing the id (duplicate ids update together so the
    // second copy can never re-execute after the first completes).
    let targets: Vec<usize> = starts
        .iter()
        .enumerate()
        .filter(|(_, s)| {
            lines[**s]
                .trim_start()
                .trim_start_matches("- id:")
                .trim()
                .trim_matches('"')
                == id
        })
        .map(|(t, _)| t)
        .collect();
    if targets.is_empty() {
        return content.to_string();
    }
    let mut out: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
    // Record insertions as (index, text) applied after all rewrites so
    // earlier inserts cannot shift later block offsets: apply descending.
    let mut inserts: Vec<(usize, String)> = Vec::new();
    for t in targets {
        let begin = starts[t];
        let end = if t + 1 < starts.len() {
            starts[t + 1]
        } else {
            lines.len()
        };
        let base = lines[begin].find("id:").unwrap_or(0);
        let mut status_idx: Option<usize> = None;
        let mut attempts_idx: Option<usize> = None;
        for i in begin..end {
            if line_indent(lines[i]) != base {
                continue;
            }
            let lt = lines[i].trim();
            if lt.starts_with("status:") && status_idx.is_none() {
                status_idx = Some(i);
            } else if lt.starts_with("attempts:") && attempts_idx.is_none() {
                attempts_idx = Some(i);
            }
        }
        let indent = " ".repeat(base);
        match status_idx {
            Some(i) => {
                out[i] = format!("{}status: {}", indent, new_status);
            }
            None => {
                inserts.push((begin + 1, format!("{}status: {}", indent, new_status)));
            }
        }
        if bump_attempts {
            let cur: u32 = attempts_idx
                .and_then(|i| {
                    lines[i]
                        .trim()
                        .trim_start_matches("attempts:")
                        .trim()
                        .trim_matches('"')
                        .parse()
                        .ok()
                })
                .unwrap_or(0);
            match attempts_idx {
                Some(i) => {
                    out[i] = format!("{}attempts: {}", indent, cur + 1);
                }
                None => {
                    // Land after the status line (original or just-inserted).
                    let at = status_idx.map(|i| i + 1).unwrap_or(begin + 2);
                    inserts.push((at, format!("{}attempts: {}", indent, cur + 1)));
                }
            }
        }
    }
    inserts.sort_by_key(|a| std::cmp::Reverse(a.0));
    for (at, text) in inserts {
        out.insert(at.min(out.len()), text);
    }
    let mut s = out.join("\n");
    if content.ends_with('\n') {
        s.push('\n');
    }
    s
}

fn update_task_file(path: &str, id: &str, status: &str, bump_attempts: bool) -> bool {
    let Some(_lock) = common::acquire_lock("loop-studio-tasks", 30) else {
        eprintln!("warning: queue lock busy; update deferred for {}", id);
        return false;
    };
    let Ok(content) = fs::read_to_string(path) else {
        return false;
    };
    common::atomic_write(path, &update_task(&content, id, status, bump_attempts)).is_ok()
}


/// Deterministic fallback order: first retryable pending task in file
/// order. Judgment (`judge_next`) falls back to exactly this on any
/// model/gateway failure, so the daemon is never less reliable with
/// judgment enabled than without it.
#[allow(dead_code)]
fn next_pending(content: &str) -> Option<Task> {
    parse_tasks(content)
        .into_iter()
        .find(|t| t.status == "pending" && t.attempts < MAX_TASK_ATTEMPTS)
}

/// Retryable candidates in file order. Split out so judgment and the
/// deterministic fallback share one source of truth.
fn pending_candidates(content: &str) -> Vec<Task> {
    parse_tasks(content)
        .into_iter()
        .filter(|t| t.status == "pending" && t.attempts < MAX_TASK_ATTEMPTS)
        .collect()
}

/// Compact judgment prompt: candidate ids + short descriptions + thermal
/// state. Descriptions are truncated so a long queue cannot blow the
/// context window of the judging call itself.
fn judge_prompt(candidates: &[Task], thermal: Option<&str>) -> String {
    let mut s = String::from(
        "You are the Hermes task selector for a local autonomous coding worker. Pick the single most urgent task to run next.\n",
    );
    s.push_str(&format!("Thermal state: {}. ", thermal.unwrap_or("unknown")));
    s.push_str("Critical means prefer tiny safe tasks; otherwise pick by urgency described in each task.\nCandidates:\n");
    for t in candidates {
        let short: String = t.desc.chars().take(200).collect();
        s.push_str(&format!("- ID: {} | attempts: {} | task: {}\n", t.id, t.attempts, short));
    }
    s.push_str("Reply with exactly two lines, no extra text:\nTASK_ID: <one of the IDs above>\nREASON: <one short line>\n");
    s
}

/// Parse the line-based judgment format. Returns the matching candidate
/// on an exact id hit, else None (caller falls back to file order).
/// Line-based on purpose: no JSON crate in this std-only binary.
fn parse_judge_decision(output: &str, candidates: &[Task]) -> Option<Task> {
    for line in output.lines() {
        let t = line.trim();
        let rest = match t.strip_prefix("TASK_ID:") {
            Some(r) => r,
            None => continue,
        };
        let mut id = rest.trim().trim_matches('"').trim_matches('\'').trim();
        // Take the first whitespace-separated token so trailing
        // commentary ("b-2 because ...") cannot corrupt the match.
        if let Some(tok) = id.split_whitespace().next() {
            id = tok.trim_matches('"').trim_matches('\'');
        }
        if id.is_empty() {
            continue;
        }
        if let Some(hit) = candidates.iter().find(|c| c.id == id) {
            return Some(hit.clone());
        }
        // Unknown id: keep scanning further TASK_ID lines, if any.
    }
    None
}

/// Fold a judgment result onto the candidate list. Any failure —
/// gateway down, timeout, garbage output, unknown id — resolves to the
/// deterministic first-pending task, so judgment can never make the
/// daemon less reliable than file order.
fn judge_select(candidates: Vec<Task>, judged: Option<Task>) -> Option<Task> {
    match judged {
        Some(t) if candidates.iter().any(|c| c.id == t.id) => Some(t),
        _ => candidates.into_iter().next(),
    }
}

/// Judgment-aware task selection for the worker loop. Single-candidate
/// (or empty) queues skip the model call entirely; multi-candidate
/// queues ask the gateway via post_chat() and fall back to
/// next_pending()'s file order on any failure. Set LOOP_STUDIO_JUDGE=0/off to
/// force deterministic file order without a model call.
fn judge_next(content: &str, thermal: Option<&str>, root: &str) -> Option<Task> {
    let candidates = pending_candidates(content);
    if candidates.len() <= 1 {
        return candidates.into_iter().next();
    }
    if env::var("LOOP_STUDIO_JUDGE")
        .map(|v| v == "0" || v.eq_ignore_ascii_case("off") || v.eq_ignore_ascii_case("false"))
        .unwrap_or(false)
    {
        return candidates.into_iter().next();
    }
    // Fast path: no gateway means post_chat() can only fail. Skip the
    // multi-second connect timeout and stay on deterministic order.
    if !common::port_up(common::gateway_port()) {
        return candidates.into_iter().next();
    }
    let prompt = judge_prompt(&candidates, thermal);
    let model = env::var("LOOP_STUDIO_CHAT_MODEL")
        .ok()
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| opencode_model(root));
    let body = format!(
        "{{\"model\":\"{}\",\"messages\":[{{\"role\":\"user\",\"content\":\"{}\"}}],\"temperature\":0.0}}",
        common::json_escape(&model),
        common::json_escape(&prompt)
    );
    let judged = match post_chat(&body) {
        Some((200, reply)) => match extract_json_string(&reply, "content") {
            Some(text) => parse_judge_decision(&text, &candidates),
            None => None,
        },
        _ => None,
    };
    judge_select(candidates, judged)
}

// ------------------------------------------------- reviewer gate ----------
// Worker review pass (Finding 4 fix): implement → review → apply, max 2
// rounds per AGENTS.md. Pure std Rust: the diff is collected with git,
// judged through the existing post_chat() gateway call, parsed
// line-based (no JSON crate). Tests stay the fail-closed gate; review is
// fail-open on infra failure (gateway down, timeout, garbage output) so
// the daemon is never less reliable with review enabled than without.

/// Max review rounds per task. Round 3+ means the requirement is
/// ambiguous — the worker commits the task branch flagged for the human
/// instead of hot-looping (AGENTS.md round-3 rule, adapted: work is
/// preserved on the branch, the queue keeps moving).
const MAX_REVIEW_ROUNDS: u32 = 2;
/// Diff bytes fed to the reviewer prompt. Truncation is disclosed
/// in-prompt so the model cannot mistake a cut diff for a clean one.
const REVIEW_DIFF_LIMIT: usize = 12_000;
/// Per-file cap for untracked-file contents included in the review.
const REVIEW_FILE_LIMIT: usize = 2_000;
/// Max untracked files swept into the review prompt.
const REVIEW_FILES_MAX: usize = 10;

/// LOOP_STUDIO_REVIEW=0/off/false forces deterministic skip (tests, ops).
fn review_enabled() -> bool {
    !env::var("LOOP_STUDIO_REVIEW")
        .map(|v| v == "0" || v.eq_ignore_ascii_case("off") || v.eq_ignore_ascii_case("false"))
        .unwrap_or(false)
}

/// Working-tree diff vs HEAD (tracked mods, staged or not) plus capped
/// contents of small text untracked files. Read-only: never stages.
/// Empty string when the tree is clean vs HEAD.
fn worker_diff(root: &str) -> String {
    let mut out = String::new();
    if let Ok(o) = Command::new("git")
        .current_dir(root)
        .args(["diff", "HEAD", "--", "."])
        .output()
    {
        out.push_str(&String::from_utf8_lossy(&o.stdout));
    }
    if let Ok(o) = Command::new("git")
        .current_dir(root)
        .args(["status", "-s"])
        .output()
    {
        let mut n = 0;
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            let path = match line.strip_prefix("??") {
                Some(p) => p.trim(),
                None => continue,
            };
            if path.is_empty() || n >= REVIEW_FILES_MAX {
                continue;
            }
            n += 1;
            let full = format!("{}/{}", root, path);
            match fs::read(&full) {
                Ok(bytes) if bytes.len() > 100_000 || bytes.contains(&0) => {
                    out.push_str(&format!("\n--- untracked (binary/large, name only): {} ---\n", path));
                }
                Ok(bytes) => {
                    let text = String::from_utf8_lossy(&bytes);
                    let capped: String = text.chars().take(REVIEW_FILE_LIMIT).collect();
                    out.push_str(&format!("\n--- untracked file: {} ---\n{}\n", path, capped));
                    if text.chars().count() > REVIEW_FILE_LIMIT {
                        out.push_str("[truncated]\n");
                    }
                }
                Err(_) => {
                    out.push_str(&format!("\n--- untracked (unreadable, name only): {} ---\n", path));
                }
            }
        }
        if n >= REVIEW_FILES_MAX {
            out.push_str("[untracked file list truncated]\n");
        }
    }
    out
}

/// Reviewer prompt. The verdict is DERIVED from finding markers by
/// parse_review_findings (robust against verdict/findings mismatch), but
/// the model is still asked for VERDICT so its output stays structured.
fn review_prompt(task_desc: &str, diff: &str) -> String {
    let short_task: String = task_desc.chars().take(300).collect();
    let (shown, truncated) = if diff.chars().count() > REVIEW_DIFF_LIMIT {
        (diff.chars().take(REVIEW_DIFF_LIMIT).collect::<String>(), true)
    } else {
        (diff.to_string(), false)
    };
    let mut s = String::from(
        "You are Loop Studio Reviewer, a read-only code auditor. Audit ONLY the diff below against the task. Report one finding per line:\n",
    );
    s.push_str("- [critical] file:line — description  (security hole, data loss, broken invariant, test bypass)\n");
    s.push_str("- [major] file:line — description  (likely bug, wrong behavior, missing error handling, scope creep)\n");
    s.push_str("- [minor] file:line — description  (style, nits — informational only)\n");
    s.push_str("Rules: no refactors beyond the task; minor findings never block. If clean, reply exactly: NO_FINDINGS\n");
    s.push_str("End with exactly one line — VERDICT: approve  (zero critical/major)  or  VERDICT: fix  (any critical/major).\n");
    s.push_str(&format!("Task: {}\n", short_task));
    if truncated {
        s.push_str(&format!("(Diff truncated to {} chars — audit what is shown; do not assume the hidden tail is clean.)\n", REVIEW_DIFF_LIMIT));
    }
    s.push_str("Diff:\n");
    s.push_str(&shown);
    if truncated {
        s.push_str("\n[truncated]");
    }
    s.push('\n');
    s
}

/// Critical/major finding lines. Verdict is derived from these markers —
/// never from the model's VERDICT line — so a mismatched verdict cannot
/// smuggle findings past the gate or block on prose. Minor/suggestion
/// lines are dropped here (report-only, never applied).
fn parse_review_findings(output: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in output.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let low = t.to_lowercase();
        if low.contains("[critical]") || low.contains("[major]") {
            out.push(t.to_string());
        }
    }
    out
}

/// Disposition after review round `round` (1-based) with `has_findings`.
/// Pure policy, unit-tested: the loop below can never exceed
/// MAX_REVIEW_ROUNDS reviews no matter what the model returns.
fn review_disposition(round: u32, has_findings: bool) -> &'static str {
    if !has_findings {
        "commit"
    } else if round < MAX_REVIEW_ROUNDS {
        "fix"
    } else {
        "commit-flagged"
    }
}

enum ReviewOutcome {
    Approve,
    Fix(Vec<String>),
    Unavailable(String),
}

/// One review pass over the current working tree. Fail-open on infra
/// failure: gateway down, timeout, non-200, missing content, or
/// LOOP_STUDIO_REVIEW=off all yield Unavailable (commit proceeds on the test
/// gate, event logged). A clean diff is Approve without a model call.
fn review_diff(root: &str, task_desc: &str, logpath: &str) -> ReviewOutcome {
    if !review_enabled() {
        return ReviewOutcome::Unavailable("LOOP_STUDIO_REVIEW=off".to_string());
    }
    if !common::port_up(common::gateway_port()) {
        return ReviewOutcome::Unavailable(format!("gateway :{} down", common::gateway_port()));
    }
    let diff = worker_diff(root);
    if diff.trim().is_empty() {
        return ReviewOutcome::Approve;
    }
    let prompt = review_prompt(task_desc, &diff);
    let model = env::var("LOOP_STUDIO_CHAT_MODEL")
        .ok()
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| opencode_model(root));
    let body = format!(
        "{{\"model\":\"{}\",\"messages\":[{{\"role\":\"system\",\"content\":\"{}\"}},{{\"role\":\"user\",\"content\":\"{}\"}}],\"temperature\":0.0}}",
        common::json_escape(&model),
        common::json_escape("You are Loop Studio Reviewer, a read-only code auditor. Reply only in the requested finding-line format."),
        common::json_escape(&prompt)
    );
    let text = match post_chat(&body) {
        Some((200, reply)) => match extract_json_string(&reply, "content") {
            Some(t) => t,
            None => return ReviewOutcome::Unavailable("200 without chat content".to_string()),
        },
        Some((code, _)) => return ReviewOutcome::Unavailable(format!("gateway HTTP {}", code)),
        None => return ReviewOutcome::Unavailable("gateway unreachable/timeout".to_string()),
    };
    // Transcript the raw review for 3am debugging (best-effort).
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(logpath) {
        let _ = writeln!(f, "\n===== reviewer @ {} =====\n{}", common::now_unix(), text);
    }
    let findings = parse_review_findings(&text);
    if findings.is_empty() {
        ReviewOutcome::Approve
    } else {
        ReviewOutcome::Fix(findings)
    }
}

fn count_pending(content: &str) -> (usize, usize) {
    // (pending_and_retryable, dead_letter)
    let mut p = 0;
    let mut d = 0;
    for t in parse_tasks(content) {
        if t.status == "pending" {
            if t.attempts < MAX_TASK_ATTEMPTS {
                p += 1;
            } else {
                d += 1;
            }
        }
    }
    (p, d)
}

fn task_timeout() -> Duration {
    env::var("LOOP_STUDIO_TASK_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(7200))
}

/// Kill a process tree via the shared helper (children first, SIGKILL).
/// `child.kill()` alone only signals the direct child; `opencode2` and
/// `make` both spawn grandchildren that would otherwise pile up past
/// the timeout. Best-effort: races with natural exit are harmless.
fn kill_tree(pid: u32) {
    common::kill_tree(pid);
}

/// Spawn and wait with a hard deadline; kills the whole tree on expiry.
/// Returns Ok(true) only on exit status 0 within budget.
fn run_with_timeout(cmd: &str, args: &[&str], dir: &str, timeout: Duration) -> io::Result<bool> {
    let mut child = Command::new(cmd).args(args).current_dir(dir).spawn()?;
    let start = Instant::now();
    loop {
        match child.try_wait()? {
            Some(st) => return Ok(st.success()),
            None => {
                if start.elapsed() >= timeout {
                    kill_tree(child.id());
                    let _ = child.kill();
                    let _ = child.wait();
                    return Ok(false);
                }
                std::thread::sleep(Duration::from_secs(5));
            }
        }
    }
}

/// Transcripts live under ~/.lac/task-logs/<id>-<unix>.log.
fn task_log_path(id: &str) -> String {
    format!(
        "{}/.lac/task-logs/{}-{}.log",
        common::home_dir(),
        id,
        common::now_unix()
    )
}

/// run_with_timeout, but child stdout/stderr tee live to both the console
/// and a per-task transcript — failed tasks stay debuggable afterwards,
/// and the knowledge-recall skill gains real learning material.
fn run_logged(
    cmd: &str,
    args: &[&str],
    dir: &str,
    timeout: Duration,
    log: &str,
    header: &str,
) -> io::Result<bool> {
    let file = match fs::OpenOptions::new().create(true).append(true).open(log) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("transcript {} unwritable ({}); running without log", log, e);
            return run_with_timeout(cmd, args, dir, timeout);
        }
    };
    let file = Arc::new(Mutex::new(file));
    {
        let mut f = file.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writeln!(f, "\n===== {} @ {} =====", header, common::now_unix());
    }
    println!("--- {} (transcript: {})", header, log);
    let mut child = Command::new(cmd)
        .args(args)
        .current_dir(dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let pump = |mut pipe: Box<dyn std::io::Read + Send>, to_stderr: bool, file: Arc<Mutex<fs::File>>| {
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match pipe.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if to_stderr {
                            let _ = std::io::stderr().write_all(&buf[..n]);
                        } else {
                            let _ = std::io::stdout().write_all(&buf[..n]);
                        }
                        let mut f = file.lock().unwrap_or_else(|e| e.into_inner());
                        let _ = f.write_all(&buf[..n]);
                    }
                    Err(_) => break,
                }
            }
        })
    };
    let t_out = child
        .stdout
        .take()
        .map(|p| pump(Box::new(p), false, Arc::clone(&file)));
    let t_err = child
        .stderr
        .take()
        .map(|p| pump(Box::new(p), true, Arc::clone(&file)));

    let start = Instant::now();
    let ok = loop {
        match child.try_wait()? {
            Some(st) => break st.success(),
            None => {
                if start.elapsed() >= timeout {
                    kill_tree(child.id());
                    let _ = child.kill();
                    let _ = child.wait();
                    break false;
                }
                std::thread::sleep(Duration::from_secs(5));
            }
        }
    };
    // Bounded join: a detached grandchild can inherit the pipes and hold
    // them open past the kill, in which case the pumps never see EOF.
    // The worker must survive that (detached pumps die with the process;
    // a stalled queue does not come back). 10s grace, then detach.
    let deadline = Instant::now() + Duration::from_secs(10);
    for t in [t_out, t_err].into_iter().flatten() {
        while !t.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    Ok(ok)
}

/// git bound to the project root — never the ambient cwd. A terminal
/// `loop-studio worker` launched from another directory must not branch,
/// commit, or reset that directory.
///
/// Also keeps "it did not run" distinct from "it ran and printed nothing",
/// which is what the reporting paths need. The mutating callers use
/// [`GitOutput::succeeded`], because for a `git branch` or a `git commit` the
/// only question is whether it worked.
fn git_in(root: &str, args: &[&str]) -> GitOutput {
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

fn worker_current_path() -> String {
    format!("{}/.lac/worker-current", common::home_dir())
}

// ------------------------------------------------------------------ worker -

fn cmd_worker(root: &str, args: &[String]) {
    let continuous = !args.iter().any(|a| a == "--drain" || a == "--once");
    // launchd never rotates the log its plist opened; the worker does. Started
    // for every invocation so a terminal `loop-studio worker` is bounded too.
    common::spawn_log_rotator(
        common::log_names_for("worker"),
        common::LOG_MAX_BYTES,
        common::LOG_KEEP,
        std::time::Duration::from_secs(300),
    );
    let home = common::home_dir();
    let tasks_file = common::tasks_file();

    // Single-flight: two workers (launchd + terminal) must never drain
    // the same queue concurrently.
    let _lock = match common::acquire_lock("loop-studio-worker", 900) {
        Some(l) => l,
        None => {
            eprintln!("Another loop-studio worker holds the lock (~/.lac/locks/loop-studio-worker). Exiting.");
            std::process::exit(1);
        }
    };

    // Crash recovery: worktrees are isolated, so never checkout or reset the
    // operator tree while recovering. Preserve any abandoned worktree for
    // human review and clear only the marker.
    if fs::metadata(worker_current_path()).is_ok() {
        common::log_event("worker_recover_preserved", "abandoned worktree retained for human review");
        println!("Previous worker state found; preserving its worktree for human review.");
        let _ = fs::remove_file(worker_current_path());
    }

    // Pre-flight 1: router on :8000.
    if !common::port_up(common::gateway_port()) {
        let router_bin = common::bin(root, "loop-studio-router");
        if fs::metadata(&router_bin).is_ok() {
            println!("Starting Loop Studio Unified Gateway on :{} in background...", common::gateway_port());
            match Command::new(&router_bin)
                .env("LOOP_STUDIO_ROUTER_PORT", common::gateway_port().to_string())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(child) => {
                    common::persist_or_warn(
                        &format!("{}/.lac/router.pid", home),
                        &child.id().to_string(),
                        "router_pid",
                    );
                }
                Err(e) => println!("  [!] Router spawn failed: {}", e),
            }
            if common::wait_for_port(common::gateway_port(), Duration::from_secs(10)) {
                println!("  [ok] Loop Studio Gateway online on http://127.0.0.1:{}/v1", common::gateway_port());
            } else {
                println!("  [!] Gateway did not come up; worker continues, backends probed directly.");
            }
        } else {
            println!("  [!] Router binary missing ({}); run `make build` first.", router_bin);
        }
    }

    // Pre-flight 2: backends (MLX port follows serve-mlx drift).
    if !common::port_up(common::mlx_port()) && !common::port_up(common::llama_port()) && !common::port_up(common::ollama_port()) {
        println!("Notice: no inference engine on MLX/llama/Ollama ports.");
        let hint = if common::mlx_supported() { "loop-studio serve mlx" } else { "loop-studio serve llama" };
        println!("The worker will idle until one appears ('{}').", hint);
    }

    // Pre-flight 3: queue exists.
    if fs::metadata(&tasks_file).is_err() {
        println!("Initializing task queue at {}...", tasks_file);
        cmd_loop(root, &["init".to_string()]);
    }

    let base_branch = common::default_branch_in(root);
    let has_commits = common::repo_has_commits_in(root);
    if !has_commits {
        println!("Notice: repo has no commits yet — branch/commit steps are skipped until the first commit.");
    }

    println!("================================================================");
    println!("  Loop Studio 24/7 AUTONOMOUS WORKER DAEMON (v{})", VERSION);
    println!("  Target: {} | Qwen 3.8 27B", common::arch_label());
    println!("  Queue : {}", tasks_file);
    println!("  Base  : {} | Mode: {}", base_branch, if continuous { "Continuous 24/7 Watch" } else { "Batch Drain & Stop" });
    println!("================================================================\n");
    common::log_event("worker_start", &format!("mode={}", if continuous { "watch" } else { "drain" }));

    let kv_bin = common::bin(root, "kv-manage");
    let kv_ok = fs::metadata(&kv_bin).is_ok();

    loop {
        let thermal = common::thermal_state();
        if thermal.as_deref() == Some("Critical") {
            println!("Thermal Critical! Pausing worker 300s...");
            common::log_event("worker_thermal", "critical: sleeping 300s");
            std::thread::sleep(Duration::from_secs(300));
            continue;
        }
        if thermal.as_deref() == Some("Serious") {
            println!("Thermal Serious: throttling 60s before next task...");
            common::log_event("worker_thermal", "serious: sleeping 60s");
            std::thread::sleep(Duration::from_secs(60));
        }

        let content = match fs::read_to_string(&tasks_file) {
            Ok(c) => c,
            Err(_) => {
                println!("No tasks file at {}. Reinitializing...", tasks_file);
                cmd_loop(root, &["init".to_string()]);
                if !continuous {
                    break;
                }
                std::thread::sleep(Duration::from_secs(15));
                continue;
            }
        };

        // Judgment-aware selection: LLM picks among pending tasks, with a
        // fail-closed fallback to deterministic file order (next_pending).
        // Hygiene gates above (thermal) and below (KV checkpoint, tests)
        // are unchanged — judgment only affects *which* task runs next.
        let task = match judge_next(&content, thermal.as_deref(), root) {
            Some(t) => t,
            None => {
                let (p, d) = count_pending(&content);
                println!(
                    "Queue idle ({} retryable pending, {} dead-letter). Thermals: {}.",
                    p,
                    d,
                    common::thermal_label(&common::thermal_state())
                );
                if kv_ok {
                    let _ = Command::new(&kv_bin).arg("check").output();
                }
                if !continuous {
                    println!("All tasks drained. Worker exiting.");
                    break;
                }
                println!("Sleeping 15s before next scan (Ctrl+C stops)...");
                std::thread::sleep(Duration::from_secs(15));
                continue;
            }
        };

        // Panic isolation: the 24/7 loop must survive a poisoned task.
        // `task` is only borrowed below so its strike can still be
        // counted afterwards if the closure unwinds.
        let task_id = task.id.clone();
        let task_branched = Arc::new(AtomicBool::new(false));
        let worktree_path = format!(
            "{}/.lac/worker-worktrees/{}-a{}-{}",
            home,
            task.id,
            task.attempts + 1,
            std::process::id()
        );
        let survived = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        println!("\nProcessing queued task [{}]: \"{}\" (attempt {}/{})", task.id, task.desc, task.attempts + 1, MAX_TASK_ATTEMPTS);
        common::log_event("task_start", &format!("[{}] {}", task.id, task.desc));
        common::persist_or_warn(&worker_current_path(), &format!("id: {}\nbase: {}\n", task.id, base_branch), "worker_current");

        if kv_ok {
            println!("  [1/4] KV hygiene checkpoint...");
            let _ = Command::new(&kv_bin)
                .args(["truncate", "--tokens", "16000", "--keep", "8000"])
                .output();
        }

        let was_dirty = common::repo_dirty_in(root);
        let branched: Option<String> = if has_commits {
            let branch = format!("task/{}", task.id);
            let _ = fs::create_dir_all(
                Path::new(&worktree_path)
                    .parent()
                    .unwrap_or_else(|| Path::new(&home)),
            );
            println!("  [2/4] Isolated worktree: {}", branch);
            let branch_ref = format!("refs/heads/{}", branch);
            let branch_exists =
                git_in(root, &["show-ref", "--verify", "--quiet", &branch_ref]).succeeded();
            // An existing branch is checked out as-is; a new one is created by
            // the add. Both are the worker's own branch namespace.
            let added = if branch_exists {
                git_in(root, &["worktree", "add", &worktree_path, &branch])
            } else {
                git_in(root, &["worktree", "add", "-b", &branch, &worktree_path])
            }
            .succeeded();
            if added {
                Some(branch)
            } else {
                println!("  [!] Worktree creation failed; refusing to mutate the operator tree.");
                None
            }
        } else {
            println!("  [2/4] No commits yet — working tree used directly.");
            None
        };
        let work_root = branched
            .as_ref()
            .map(|_| worktree_path.clone())
            .unwrap_or_else(|| root.to_string());
        task_branched.store(branched.is_some(), Ordering::SeqCst);
        if has_commits && branched.is_none() {
            let bumped = task.attempts + 1;
            let status = if bumped >= MAX_TASK_ATTEMPTS {
                println!("  Task [{}] reached {} attempts — dead-letter.", task.id, MAX_TASK_ATTEMPTS);
                common::log_event("task_dead_letter", &format!("[{}]", task.id));
                "failed"
            } else {
                "pending"
            };
            if !update_task_file(&tasks_file, &task.id, status, true) {
                common::log_event("task_update_failed", &format!("task {}", task.id));
            }
            let _ = fs::remove_file(worker_current_path());
            return;
        }

        println!("  [3/4] Dispatching to OpenCode V2 (@coder)...");
        let prompt = format!(
            "Task: {}. Implement surgically, run project tests, and ensure zero failures.",
            task.desc
        );
        let timeout = task_timeout();
        let logpath = task_log_path(&task.id);
        let mut impl_ok = run_logged(
            "opencode2",
            &["run", &prompt],
            &work_root,
            timeout,
            &logpath,
            &format!("opencode2 task {}", task.id),
        )
        .unwrap_or(false);

        let mut test_ok = if impl_ok {
            println!("  [3b/4] Reliability gate: project tests...");
            // Fail-closed: a test runner that cannot even spawn must never
            // count as "tests passed" (that would commit untested code).
            run_logged("make", &["test"], &work_root, Duration::from_secs(600), &logpath, "make test")
                .unwrap_or(false)
        } else {
            false
        };

        // Reviewer gate: implement → review → apply, max 2 rounds.
        // Tests stay fail-closed; review is fail-open on infra failure.
        // A fix round that breaks tests falls back into the failure path
        // below (reset + attempt bump), exactly like a fresh failure.
        let mut review_flagged = false;
        if impl_ok && test_ok {
            let mut round: u32 = 0;
            loop {
                round += 1;
                match review_diff(&work_root, &task.desc, &logpath) {
                    ReviewOutcome::Approve => {
                        println!("  [3c/4] Reviewer round {}: approve.", round);
                        common::log_event("review_approve", &format!("[{}] round {}", task.id, round));
                        break;
                    }
                    ReviewOutcome::Unavailable(reason) => {
                        println!("  [3c/4] Reviewer unavailable ({}); proceeding on test gate.", reason);
                        common::log_event("review_unavailable", &format!("[{}] {}", task.id, reason));
                        break;
                    }
                    ReviewOutcome::Fix(findings) => {
                        match review_disposition(round, true) {
                            "fix" => {
                                println!("  [3c/4] Reviewer round {}: {} critical/major finding(s) — back to @coder...", round, findings.len());
                                common::log_event("review_fix", &format!("[{}] round {} findings {}", task.id, round, findings.len()));
                                let fix_prompt = format!(
                                    "Task: {}. Address these review findings (critical and major ONLY; leave minor/suggestions untouched):\n{}\nRe-run project tests and ensure zero failures.",
                                    task.desc,
                                    findings.join("\n")
                                );
                                impl_ok = run_logged(
                                    "opencode2",
                                    &["run", &fix_prompt],
                                    &work_root,
                                    timeout,
                                    &logpath,
                                    &format!("opencode2 fix {} r{}", task.id, round),
                                )
                                .unwrap_or(false);
                                test_ok = if impl_ok {
                                    println!("  [3b/4] Reliability gate (post-fix): project tests...");
                                    run_logged("make", &["test"], &work_root, Duration::from_secs(600), &logpath, "make test")
                                        .unwrap_or(false)
                                } else {
                                    false
                                };
                                if !(impl_ok && test_ok) {
                                    break;
                                }
                                // Loop re-reviews (round 2). review_disposition
                                // caps this: round 2 findings commit flagged.
                            }
                            _ => {
                                println!("  [3c/4] Reviewer round {}: still {} critical/major finding(s) after {} rounds — committing to task branch FLAGGED for human merge review.", round, findings.len(), round);
                                common::log_event("review_unresolved", &format!("[{}] {} finding(s) after {} rounds", task.id, findings.len(), round));
                                review_flagged = true;
                                break;
                            }
                        }
                    }
                }
            }
        }

        if impl_ok && test_ok {
            println!("  [4/4] Tests passed{}.", if review_flagged { " (review findings unresolved — flagged)" } else { "" });
            if has_commits && branched.is_some() {
                let _ = git_in(&work_root, &["add", "-A"]);
                let msg = format!("feat({}): {}", task.id, task.desc.chars().take(120).collect::<String>());
                let _ = git_in(&work_root, &["commit", "-m", &msg]);
            } else if was_dirty {
                println!("  Operator tree was dirty; changes remain uncommitted for human review.");
                review_flagged = true;
            }
            println!("  Task [{}] complete.", task.id);
            common::log_event("task_complete", &format!("[{}] review_flagged={}", task.id, review_flagged));
            if !update_task_file(&tasks_file, &task.id, "complete", false) {
                common::log_event("task_update_failed", &format!("task {}", task.id));
            }
        } else {
            println!("  Task [{}] failed reliability gate.", task.id);
            common::log_event("task_failed", &format!("[{}] attempt {}", task.id, task.attempts + 1));
            if branched.is_some() {
                let stash_label = format!("loop-studio failed task {} attempt {}", task.id, task.attempts + 1);
                if git_in(&work_root, &["stash", "push", "-u", "-m", &stash_label]).succeeded() {
                    println!("  Preserved failed-task changes in git stash '{}'.", stash_label);
                } else {
                    println!("  Could not preserve failed-task changes; leaving the worktree for manual recovery.");
                }
                if !git_in(root, &["worktree", "remove", "--force", &worktree_path]).succeeded() {
                    println!("  Failed worktree remains at {}.", worktree_path);
                }
            } else if was_dirty {
                println!("  Leaving working tree untouched (was dirty before task).");
            }
            let bumped = task.attempts + 1;
            let status = if bumped >= MAX_TASK_ATTEMPTS {
                println!("  Task [{}] reached {} attempts — dead-letter.", task.id, MAX_TASK_ATTEMPTS);
                common::log_event("task_dead_letter", &format!("[{}]", task.id));
                "failed"
            } else {
                "pending"
            };
            if !update_task_file(&tasks_file, &task.id, status, true) {
                common::log_event("task_update_failed", &format!("task {}", task.id));
            }
        }

        if branched.is_some() {
            if !git_in(root, &["worktree", "remove", "--force", &worktree_path]).succeeded() {
                common::log_event("worker_cleanup_blocked", &format!("task {} worktree remains at {}", task.id, worktree_path));
            } else {
                let _ = fs::remove_file(worker_current_path());
            }
        } else if !was_dirty {
            let _ = fs::remove_file(worker_current_path());
        }

        // The task's transcript is closed by now, so the retention trim is
        // safe to run — and it has to run *somewhere*, because this is the only
        // code path that writes one. The tree used to have no bound at all: a
        // new timestamped file per task per attempt, forever, and nothing
        // reported it, so the 24/7 worker's cost grew without either an
        // operator's consent or their knowledge.
        {
            let (removed, reclaimed) = common::trim_task_logs();
            if removed > 0 {
                common::log_event(
                    "task_logs_trimmed",
                    &format!(
                        "removed {removed} transcript(s), {:.1} MiB reclaimed",
                        reclaimed as f64 / (1024.0 * 1024.0)
                    ),
                );
            }
        }
        }));
        if survived.is_err() {
            println!("  Task [{}] panicked; daemon survives. Counting a strike.", task_id);
            common::log_event("task_panic", &format!("[{}] worker caught panic; state reset", task_id));
            if task_branched.load(Ordering::SeqCst) {
                let panic_stash = format!("loop-studio panicked task {} attempt {}", task_id, task.attempts + 1);
                let _ = git_in(&worktree_path, &["stash", "push", "-u", "-m", &panic_stash]);
                if git_in(root, &["worktree", "remove", "--force", &worktree_path]).succeeded() {
                    let _ = fs::remove_file(worker_current_path());
                } else {
                    common::log_event("worker_recovery_blocked", &format!("task {} worktree remains at {}", task_id, worktree_path));
                }
            } else {
                common::log_event("worker_recovery_blocked", &format!("task {} preserved unbranched tree", task_id));
            }
            // A deterministically-poisoned task must dead-letter after 3
            // strikes, not hot-loop the daemon forever.
            let bumped = task.attempts + 1;
            let status = if bumped >= MAX_TASK_ATTEMPTS {
                println!("  Task [{}] reached {} attempts — dead-letter.", task_id, MAX_TASK_ATTEMPTS);
                common::log_event("task_dead_letter", &format!("[{}]", task_id));
                "failed"
            } else {
                "pending"
            };
            if !update_task_file(&tasks_file, &task_id, status, true) {
                common::log_event("task_update_failed", &format!("task {}", task_id));
            }
        }

        if !continuous {
            // Drain mode re-reads the queue on the next iteration.
            continue;
        }
    }
    common::log_event("worker_stop", "exiting");
}

// --------------------------------------------------------------- ops cmds --

fn cmd_stop() {
    println!("Stopping Loop Studio services...");
    let pid_files = [
        (format!("{}/.lac/router.pid", common::home_dir()), "loop-studio-router"),
    ];
    for (pf, label) in pid_files {
        if let Ok(pid) = fs::read_to_string(&pf) {
            let pid = pid.trim().to_string();
            if !pid.is_empty() {
                // A pid file is a claim about the past. Pids are recycled and
                // the file survives a crash, a reboot, or a `loop-studio route` that
                // exited and left it behind — so before TERM goes out, confirm
                // the pid is still the program we recorded. Signalling a
                // stranger's pid reports "signaled" for a router that was
                // never there, and takes down whatever is actually running.
                let identity = pid
                    .parse::<u32>()
                    .ok()
                    .and_then(common::command_of)
                    .unwrap_or_default();
                if identity.is_empty() {
                    println!("  {} pid {}: not running", label, pid);
                } else if !common::program_matches(&identity, label) {
                    println!(
                        "  {} pid {}: left alone — that pid is `{}`, not a {}",
                        label,
                        pid,
                        common::head_lines(&identity, 1),
                        label
                    );
                    // The file stays: it is the evidence, and the pid it names
                    // may be the router again after a reboot reuses the slot.
                    continue;
                } else {
                    let ok = common::signal_pid(pid.parse::<u32>().unwrap_or(0), "TERM");
                    println!("  {} pid {}: {}", label, pid, if ok { "signaled" } else { "signal failed" });
                }
            }
            // Removed only once the recorded pid is gone or was ours to signal.
            // A concurrent `loop-studio route --daemon` may have just written this
            // file, and deleting its pid out from under it would strand the
            // router it is supervising.
            let _ = fs::remove_file(&pf);
        }
    }
    // Fallback: program-identity kill for a router started without a pid file.
    let n = common::kill_matching("loop-studio-router");
    println!("  loop-studio-router process sweep: {} signaled", n);
    println!("Note: inference engines (mlx/llama/ollama) left running; stop them via the TUI serve menu or brew services.");
}

fn cmd_ps() {
    println!("=== Loop Studio processes ===");
    let mut seen: Vec<(u32, String)> = Vec::new();
    for pat in [
        "loop-studio-router",
        "loop-studio-tui",
        "serve-mlx",
        "serve-llama",
        "llama-server",
        "mlx_lm.server",
        "ollama serve",
    ] {
        for p in common::processes_matching(pat) {
            if !seen.iter().any(|(pid, _)| *pid == p.pid) {
                seen.push((p.pid, p.cmd));
            }
        }
    }
    seen.sort();
    if seen.is_empty() {
        println!("  (no loop-studio-related processes found)");
    }
    for (pid, cmd) in seen {
        println!("  {:>6} {}", pid, cmd);
    }
    println!("\n=== Port table ===");
    println!(
        "  {:<8} :{}  {}",
        "gateway",
        common::gateway_port(),
        if common::port_up(common::gateway_port()) { "LISTENING" } else { "-" }
    );
    println!(
        "  {:<8} :{}  {}",
        "mlx",
        common::mlx_port(),
        if common::port_up(common::mlx_port()) {
            "LISTENING"
        } else {
            "-"
        }
    );
    for (name, port) in [("llama", common::llama_port()), ("ollama", common::ollama_port())] {
        println!(
            "  {:<8} :{}  {}",
            name,
            port,
            if common::port_up(port) { "LISTENING" } else { "-" }
        );
    }
}

/// Run a sibling binary in the foreground and exit with *its* status.
///
/// Nine dispatch arms read `let _ = Command::new(..).status();`, which erases
/// two different failures into exit 0: the binary is missing (`Err`), and the
/// child ran and failed (`Ok(non-zero)`). The blast radius is every documented
/// remedy and every Makefile target that goes through them — `loop-studio kv check`
/// returns 2 at RED by contract and could not return anything else, `loop-studio
/// bootstrap`'s own `exit(1)` was unreachable behind the wrapper's exit 0, and
/// `make bootstrap` chained on the wrapper.
///
/// The child's stdout/stderr are inherited, so a long-running lane (`serve`,
/// `route`, `tui`) still behaves exactly as it did. Only the exit status
/// changes, which is the point: a wrapper that returns 0 for work that did not
/// happen is worse than no wrapper, because `make` trusts it.
fn exec_sibling(program: String, args: &[String]) -> ! {
    match Command::new(&program).args(args).status() {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("loop-studio: could not run {}: {}", program, e);
            std::process::exit(127);
        }
    }
}

fn cmd_logs(args: &[String]) {
    // Explicit, never automatic: the daemons bound growth and keep a few
    // generations so a crash stays diagnosable, and this is the operator saying
    // they want the disk back. The live log is never touched — it is the file a
    // running daemon is appending to.
    let purge = args.iter().any(|a| a == "--purge");
    let which = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .map(|s| s.as_str())
        .unwrap_or("router");
    // The per-task agent transcripts, which are not a daemon log and live in
    // their own directory. Handled first because the rest of this function is
    // about one file under ~/Library/Logs, which is not what these are.
    if which == "tasks" || which == "task-logs" {
        if purge {
            // The operator's word, and only the operator's word: the newest
            // `TASK_LOG_KEEP` survive so a recent failure stays debuggable.
            let victims = common::oldest_task_logs(usize::MAX);
            let (total, count) = common::task_logs_footprint();
            let doomed = victims.len().saturating_sub(common::TASK_LOG_KEEP);
            let mut reclaimed = 0u64;
            for (path, len) in victims.into_iter().take(doomed) {
                if fs::remove_file(&path).is_ok() {
                    reclaimed += len;
                }
            }
            println!(
                "purged {doomed} agent transcript(s): {:.1} MiB reclaimed \
                 (the {} most recent are kept)",
                reclaimed as f64 / (1024.0 * 1024.0),
                common::TASK_LOG_KEEP
            );
            if count == 0 {
                println!("  (there were no transcripts to purge)");
            }
            let _ = total;
        }
        let (task_bytes, task_files) = common::task_logs_footprint();
        println!(
            "=== {} ===",
            common::task_logs_dir()
        );
        println!(
            "    {:.1} MiB across {task_files} transcript(s) (newest {} kept, each capped at {} MiB)",
            task_bytes as f64 / (1024.0 * 1024.0),
            common::TASK_LOG_KEEP,
            common::TASK_LOG_MAX_BYTES / (1024 * 1024),
        );
        let newest = newest_task_log_name();
        if newest.is_empty() {
            println!("(no task transcripts yet)");
            return;
        }
        match common::tail_file(
            &format!("{}/{}", common::task_logs_dir(), newest),
            100,
        ) {
            Some(t) if !t.is_empty() => print!("{}", t),
            _ => println!("(transcript unreadable — {} may be mid-write)", newest),
        }
        return;
    }
    // Every name the daemons write, and only those. This used to be a
    // five-entry hand-list whose catch-all was the router, so a typo —
    // `loop-studio logs wroker` — silently tailed the *router's* log and printed its
    // path with no error at all, and three of the eight rotated names
    // (`loop-studio-worker.err`, `loop-studio-serve-mlx.err`, `loop-studio-serve-llama.err`) were
    // counted by `loop-studio doctor` and by the footprint total but could not be
    // named here, so the message "reclaim with `loop-studio logs <which> --purge`"
    // named a reclaim that could not be performed. The name list now comes
    // from the one function the rotator and the plists use.
    let file = match common::log_file_name(which) {
        Some(name) => name,
        None => {
            eprintln!(
                "loop-studio logs: unknown log {which:?}. Known: {}",
                common::log_which_names().join(", ")
            );
            std::process::exit(2);
        }
    };
    let path = format!("{}/{}", common::logs_dir(), file);
    // The size, because a daemon's log is the one file in this project that
    // grows for as long as the machine is up: launchd opens it, nothing
    // rotates it, and `tail_file` reads a bounded window of it — so the cost of
    // a large log was invisible until someone opened the file in an editor.
    if purge {
        let (reclaimed, failed) = common::purge_rotated_generations(&path);
        println!(
            "purged rotated generations of {file}: {:.1} MiB reclaimed (the live log is untouched)",
            reclaimed as f64 / (1024.0 * 1024.0)
        );
        if failed > 0 {
            eprintln!(
                "  [x] {failed} generation(s) could not be removed and are still on disk"
            );
            std::process::exit(1);
        }
    }
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let generations = std::fs::read_dir(common::logs_dir())
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .starts_with(&format!("{file}."))
                })
                .count()
        })
        .unwrap_or(0);
    println!("=== tail {} ===", path);
    println!(
        "    {:.1} MiB{}{}",
        size as f64 / (1024.0 * 1024.0),
        if generations > 0 {
            format!(", {generations} rotated generation(s) beside it")
        } else {
            String::new()
        },
        if size > common::LOG_MAX_BYTES {
            format!(
                " — over the {} MiB the daemon rotates at",
                common::LOG_MAX_BYTES / (1024 * 1024)
            )
        } else {
            String::new()
        }
    );
    // The whole picture, not one file: the cost of unbounded logs is the sum
    // across every daemon, and reporting one file at a time is how a directory
    // of them stays invisible.
    let (bytes, files) = common::lac_logs_footprint();
    println!(
        "--- all Loop Studio logs: {:.1} MiB across {files} file(s){} ---",
        bytes as f64 / (1024.0 * 1024.0),
        if bytes > common::LOG_MAX_BYTES * 8 {
            " — reclaim with `loop-studio logs <which> --purge`"
        } else {
            ""
        }
    );
    if which == "tasks" {
        // `tasks` is the transcript tree, not a daemon log, so it has its own
        // path, its own size and its own reclaim. It used to be unreachable
        // from here at all: an unrecognised name fell through to the router's
        // log, so `loop-studio logs tasks` printed the router's path and said nothing
        // about the one directory on this machine that grows without a bound.
        let (task_bytes, task_files) = common::task_logs_footprint();
        println!(
            "{}: {:.1} MiB across {task_files} transcript(s) (newest {} kept, each capped at {} MiB)",
            common::task_logs_dir(),
            task_bytes as f64 / (1024.0 * 1024.0),
            common::TASK_LOG_KEEP,
            common::TASK_LOG_MAX_BYTES / (1024 * 1024),
        );
        match common::tail_file(
            &format!("{}/{}", common::task_logs_dir(), newest_task_log_name()),
            100,
        ) {
            Some(t) if !t.is_empty() => print!("{}", t),
            _ => println!("(no task transcripts yet)"),
        }
        return;
    }
    match common::tail_file(&path, 100) {
        Some(t) if !t.is_empty() => print!("{}", t),
        _ => println!("(log file absent or empty — daemon may never have run)"),
    }
}

/// The name of the most recent transcript, for `loop-studio logs tasks` to tail.
fn newest_task_log_name() -> String {
    common::oldest_task_logs(usize::MAX)
        .pop()
        .map(|(path, _)| {
            std::path::Path::new(&path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
        .unwrap_or_default()
}

fn cmd_config(root: &str) {
    let e = |k: &str, d: &str| env::var(k).unwrap_or_else(|_| d.to_string());
    println!("=== Loop Studio effective configuration ===");
    println!("  version      : {}", VERSION);
    println!("  root         : {}", root);
    println!("  gateway      : {}", e("LOOP_STUDIO_GATEWAY_URL", &format!("http://127.0.0.1:{}/v1", common::gateway_port())));
    println!("  router_port  : {}", common::gateway_port());
    println!("  bind_addr    : {}", e("LOOP_STUDIO_BIND_ADDR", &e("LOOP_STUDIO_ROUTER_HOST", "127.0.0.1")));
    println!(
        "  auth         : {}",
        if env::var("LOOP_STUDIO_API_TOKEN").map(|s| !s.trim().is_empty()).unwrap_or(false) {
            "Bearer required on every listener (token set, redacted)"
        } else {
            "none (loopback only)"
        }
    );
    println!("  backend_pref : {}", e("LOOP_STUDIO_BACKEND", "auto"));
    println!("  primary      : {}", e("PRIMARY_MODEL", "qwen3.8-27b"));
    println!("  mlx_model    : {}", e("MLX_MODEL", "mlx-community/Qwen3.8-27B-4bit"));
    println!("  llama_model  : {}", e("LLAMA_MODEL", "unsloth/Qwen3.6-27B-MTP-GGUF:Q8_0"));
    println!("  model_base   : {} (mounted={})", common::model_base(), common::volume_mounted("/Volumes/AIModels"));
    println!("  branch       : {} (commits={})", common::default_branch_in(root), common::repo_has_commits_in(root));
    println!("  task_timeout : {}s", task_timeout().as_secs());
}

// -------------------------------------------------------------------- main -

fn main() {
    common::ignore_sigpipe();
    let root = common::project_root();
    let args: Vec<String> = env::args().skip(1).collect();

    if args.is_empty() {
        println!("================================================================");
        println!("  Loop Studio: Default Mode -> 24/7 Autonomous Worker Daemon (v{})", VERSION);
        println!("  (Run 'loop-studio help' or 'loop-studio status' for other subcommands)");
        println!("================================================================\n");
        cmd_worker(&root, &[]);
        return;
    }

    match args[0].as_str() {
        "worker" | "start" | "run" | "default" => cmd_worker(&root, &args[1..]),
        "daemon" => cmd_daemon(&root, &args[1..]),
        "status" => cmd_status(&root, &args[1..]),
        "doctor" => {
            let issues = cmd_doctor(&root, &args[1..]);
            if issues > 0 {
                std::process::exit(1);
            }
        }
        "thermal" => cmd_thermal(),
        "cap" => cmd_cap(&args[1..]),
        "hermes" => cmd_hermes(&root, &args[1..]),
        "stop" => cmd_stop(),
        "ps" => cmd_ps(),
        "logs" => cmd_logs(&args[1..]),
        "config" => cmd_config(&root),
        "bench" => cmd_bench(bench_port(&args[1..], 8000), &args[1..]),
        "tune" => cmd_tune(&args[1..]),
        "serve" => {
            let mode = args.get(1).map(|s| s.as_str()).unwrap_or("auto");
            match mode {
                "mlx" => {
                    if !common::mlx_supported() {
                        eprintln!("MLX requires Apple Silicon (arm64 macOS); this host is {}.", common::arch_label());
                        eprintln!("Use the llama-server lane instead: `loop-studio serve llama`.");
                        std::process::exit(1);
                    }
                    exec_sibling(
                        common::bin(&root, "serve-mlx"),
                        &["mlx-community/Qwen3.8-27B-4bit".to_string()],
                    );
                }
                "llama" => exec_sibling(
                    common::bin(&root, "serve-llama"),
                    &[
                        "unsloth/Qwen3.6-27B-MTP-GGUF:Q8_0".to_string(),
                        "16384".to_string(),
                    ],
                ),
                "ollama" => {
                    println!("Starting Ollama server...");
                    exec_sibling(
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
                    exec_sibling(common::bin(&root, "loop-studio-tui"), &[]);
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
                            &format!("{}/.lac/router.pid", common::home_dir()),
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
                // `--daemon` above backgrounds the router and records its pid;
                // this is the foreground case, and its exit status is the
                // router's. `make route` chains on it.
                match Command::new(common::bin(&root, "loop-studio-router"))
                    .env("LOOP_STUDIO_ROUTER_PORT", common::gateway_port().to_string())
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
        "tui" => exec_sibling(common::bin(&root, "loop-studio-tui"), &[]),
        // `kv-manage check` is documented as 0 GREEN / 1 YELLOW / 2 RED, and no
        // script could observe that through this wrapper before.
        "kv" => exec_sibling(common::bin(&root, "kv-manage"), &args[1..]),
        "loop" => cmd_loop(&root, &args[1..]),
        "visualize" | "studio" | "app" | "gui" => cmd_visualize(&root),
        "chat" => cmd_chat(&root, &args[1..]),
        "code" => cmd_code(&root, &args[1..]),
        "dashboard" => cmd_dashboard(&root, &args[1..]),
        "bootstrap" => exec_sibling(common::bin(&root, "bootstrap"), &[]),
        "models" => exec_sibling(common::bin(&root, "pull-models"), &[]),
        "pull" => std::process::exit(cmd_pull(&root, &args[1..])),
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
    // A `#[test]` that loses its attribute is valid Rust that never runs:
    // the suite goes one test quieter and stays green. Fail the build instead.
    #![deny(dead_code)]

    use super::*;

    /// The new `logs` line: it escalates when the daemons' own bound is well
    /// exceeded, names the command that reclaims it, and never reaches `fail`
    /// — a large log is not a broken stack, and `doctor`'s exit code is what
    /// automation branches on.
    #[test]
    fn the_log_footprint_line_escalates_but_never_fails() {
        let cap = common::LOG_MAX_BYTES;
        let quiet = doctor_logs_check(0, 0, 0, 0);
        assert_eq!(quiet.level, "ok");
        assert!(quiet.detail.contains("0.0 MiB"), "{}", quiet.detail);
        assert!(!quiet.detail.contains("--purge"), "{}", quiet.detail);

        // One generation is normal and says nothing.
        let working = doctor_logs_check(cap, 1, 0, 0);
        assert_eq!(working.level, "ok", "{}", working.detail);

        let loud = doctor_logs_check(cap * 9, 12, 0, 0);
        assert_eq!(loud.level, "warn", "{}", loud.detail);
        assert!(loud.detail.contains("--purge"), "a warning must say what to do: {}", loud.detail);

        // The escalation must never be able to fail the stack.
        for bytes in [0, cap, cap * 9, cap * 1000] {
            assert_ne!(doctor_logs_check(bytes, 3, 0, 0).level, "fail", "{bytes} bytes");
            assert_ne!(
                doctor_logs_check(0, 0, bytes, 3).level,
                "fail",
                "{bytes} transcript bytes"
            );
        }
    }

    /// The transcript tree has to be visible to the one command that exists to
    /// report log cost, and it has to be reclaimable by name.
    ///
    /// `run_logged` writes a new timestamped file per task per attempt, under
    /// `~/.lac/task-logs`, with no size cap, no rotation and no reader. The
    /// footprint scan only ever looked at `~/Library/Logs` for `loop-studio-*`, so this
    /// tree — usually the larger one on a busy worker — was invisible to
    /// `loop-studio doctor` while it grew for as long as the machine was up.
    #[test]
    fn the_agent_transcript_tree_is_reported_and_reclaimable() {
        let cap = common::TASK_LOG_MAX_BYTES * common::TASK_LOG_KEEP as u64;
        // Under the retention ceiling: reported quietly, and still reported —
        // the size is the number an operator needs either way.
        let small = doctor_logs_check(0, 0, cap / 2, 3);
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

        // Past it: escalates, and names a command that actually works.
        let big = doctor_logs_check(0, 0, cap * 2, 90);
        assert_eq!(big.level, "warn", "{}", big.detail);
        assert!(
            big.detail.contains("loop-studio logs tasks --purge"),
            "a warning must name the reclaim: {}",
            big.detail
        );

        // And the command name is one `loop-studio logs` accepts, derived from the
        // same place the reclaim reads from.
        assert!(common::log_file_name("tasks").is_none());
        assert!(common::task_logs_dir().ends_with("task-logs"));

        // Never able to fail the stack, in either tree.
        for bytes in [0, cap, cap * 1000] {
            assert_ne!(doctor_logs_check(0, 0, bytes, 3).level, "fail", "{bytes}");
        }
    }

    const SAMPLE: &str = "\
- id: \"a-1\"\n  task: \"do first\"\n  status: pending\n\n- id: \"b-2\"\n  task: \"do second: with colon\"\n  status: pending\n  attempts: 2\n";

    #[test]
    fn parses_ids_status_attempts() {
        let t = parse_tasks(SAMPLE);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].id, "a-1");
        assert_eq!(t[0].attempts, 0);
        assert_eq!(t[1].desc, "do second: with colon");
        assert_eq!(t[1].attempts, 2);
    }

    #[test]
    fn task_id_sanitize_allows_sane_ids() {
        assert_eq!(sanitize_task_id("a-1").as_deref(), Some("a-1"));
        assert_eq!(sanitize_task_id("loop-studio-004").as_deref(), Some("loop-studio-004"));
        assert_eq!(sanitize_task_id("A").as_deref(), Some("A"));
        assert_eq!(sanitize_task_id("x.y_z-9").as_deref(), Some("x.y_z-9"));
        assert!(sanitize_task_id(&"q".repeat(65)).is_some());
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
            assert!(sanitize_task_id(bad).is_none(), "must reject {:?}", bad);
        }
        assert!(sanitize_task_id(&"q".repeat(66)).is_none(), "must reject 66 chars");
    }

    #[test]
    fn parse_drops_tasks_with_unsafe_ids() {
        let yaml = "- id: \"../../evil\"\n  task: \"pwn\"\n  status: pending\n\n- id: \"ok-1\"\n  task: \"fine\"\n  status: pending\n";
        let t = parse_tasks(yaml);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].id, "ok-1");
    }

    #[test]
    fn chat_field_extraction() {
        let body = r#"{"id":"x","choices":[{"message":{"role":"assistant","content":"Hello \"world\"\nline2"}}]}"#;
        assert_eq!(
            extract_json_string(body, "content").as_deref(),
            Some("Hello \"world\"\nline2")
        );
        assert_eq!(
            extract_json_string(r#"{"error":{"message":"no backend"}}"#, "message").as_deref(),
            Some("no backend")
        );
        assert_eq!(extract_json_string(r#"{"a":1}"#, "content"), None);
        assert_eq!(extract_json_string(r#"{"content":"unterminated"#, "content"), None);
    }

    #[test]
    fn chat_gives_up_on_hung_backend() {
        use std::io::Read;
        // Listener that accepts and then holds the connection silently.
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
        assert!(common::http_post_addr(&addr, "/v1/chat/completions", "{}", Duration::from_secs(2)).is_none());
        assert!(t0.elapsed() < Duration::from_secs(20));
    }

    #[test]
    fn update_targets_only_matching_block() {
        let out = update_task(SAMPLE, "b-2", "complete", false);
        // First block untouched.
        assert!(out.contains("- id: \"a-1\"\n  task: \"do first\"\n  status: pending"));
        assert!(out.contains("- id: \"b-2\"\n  task: \"do second: with colon\"\n  status: complete"));
        assert_eq!(out.matches("status: complete").count(), 1);
    }

    #[test]
    fn update_inserts_missing_keys() {
        // No status:/attempts: anywhere: both must be created, or the
        // block would retry forever without ever dead-lettering.
        let bare = "- id: \"c-3\"\n  task: \"bare block\"\n";
        let out = update_task(bare, "c-3", "pending", true);
        assert!(out.contains("status: pending"), "status inserted:\n{}", out);
        assert!(out.contains("attempts: 1"), "attempts inserted:\n{}", out);
        let t = parse_tasks(&out);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].status, "pending");
        assert_eq!(t[0].attempts, 1);
    }

    #[test]
    fn nested_status_keys_are_ignored() {
        let tricky = "- id: \"d-4\"\n  task: \"migrate status: pending docs\"\n  status: pending\n  metadata:\n    status: archived\n    attempts: 99\n";
        let t = parse_tasks(tricky);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].status, "pending");
        assert_eq!(t[0].attempts, 0);
        let out = update_task(tricky, "d-4", "complete", true);
        // Real keys rewritten...
        assert!(out.contains("\n  status: complete\n"));
        // ...nested decoys untouched.
        assert!(out.contains("\n    status: archived\n"));
        assert!(out.contains("\n    attempts: 99\n"));
        assert!(out.contains("\n  attempts: 1\n"));
    }

    #[test]
    fn duplicate_ids_update_together() {
        let dup = "- id: \"e-5\"\n  task: \"first\"\n  status: pending\n- id: \"e-5\"\n  task: \"second\"\n  status: pending\n";
        let out = update_task(dup, "e-5", "complete", false);
        assert_eq!(out.matches("status: complete").count(), 2);
    }

    #[test]
    fn update_bumps_attempts_and_inserts_key() {
        let out = update_task(SAMPLE, "a-1", "pending", true);
        assert!(out.contains("attempts: 1"));
        let out2 = update_task(&out, "a-1", "pending", true);
        assert!(out2.contains("attempts: 2"));
    }

    #[test]
    fn dead_letters_exhaust_attempts_gate() {
        assert!(next_pending(SAMPLE).is_some());
        let out = update_task(SAMPLE, "a-1", "pending", true);
        let out = update_task(&out, "a-1", "pending", true);
        let out = update_task(&out, "a-1", "pending", true);
        // a-1 now at 3 attempts: skipped.
        let next = next_pending(&out).expect("b-2 still retryable");
        assert_eq!(next.id, "b-2");
        let (p, d) = count_pending(&out);
        assert_eq!((p, d), (1, 1));
    }

    #[test]
    fn judge_prompt_lists_candidates_and_thermal() {
        let cands = pending_candidates(SAMPLE);
        assert_eq!(cands.len(), 2);
        let p = judge_prompt(&cands, Some("Nominal"));
        assert!(p.contains("Nominal"), "thermal in prompt:\n{}", p);
        assert!(p.contains("a-1"), "candidate a-1 in prompt:\n{}", p);
        assert!(p.contains("b-2"), "candidate b-2 in prompt:\n{}", p);
        assert!(p.contains("TASK_ID:"), "format instructions in prompt:\n{}", p);
    }

    #[test]
    fn judge_parses_non_first_pick() {
        // Fixture queue: file order is routine first, urgent last. A
        // judgment for the urgent task must differ from next_pending().
        const URGENT_LAST: &str = "\
- id: \"t-routine\"\n  task: \"routine: tidy comments in docs\"\n  status: pending\n\n- id: \"t-normal\"\n  task: \"normal: add a unit test for tail_file\"\n  status: pending\n\n- id: \"t-urgent\"\n  task: \"URGENT: production auth bypass — fix login check in lac.rs now\"\n  status: pending\n";
        let cands = pending_candidates(URGENT_LAST);
        assert_eq!(cands.len(), 3);
        let first = next_pending(URGENT_LAST).expect("pending").id;
        assert_eq!(first, "t-routine");
        let judged = parse_judge_decision("TASK_ID: t-urgent\nREASON: production auth bypass outranks tidy-up\n", &cands)
            .expect("valid id parses");
        assert_eq!(judged.id, "t-urgent");
        assert_ne!(judged.id, first, "judgment differs from file order");
        // Folded through judge_select, the judged task is what runs.
        let picked = judge_select(cands, Some(judged)).expect("pick");
        assert_eq!(picked.id, "t-urgent");
    }

    #[test]
    fn judge_falls_back_to_file_order_on_failure() {
        const URGENT_LAST: &str = "\
- id: \"t-routine\"\n  task: \"routine: tidy comments in docs\"\n  status: pending\n\n- id: \"t-urgent\"\n  task: \"URGENT: production auth bypass\"\n  status: pending\n";
        let cands = pending_candidates(URGENT_LAST);
        let expected = next_pending(URGENT_LAST).expect("pending").id;
        // Garbage output parses to None...
        assert!(parse_judge_decision("ACTION: dispatch_loop\nno task id here\n", &cands).is_none());
        // ...and both garbage and unknown ids fold back to file order.
        let garbage = parse_judge_decision("hello world", &cands);
        assert_eq!(judge_select(cands.clone(), garbage).expect("pick").id, expected);
        let unknown = parse_judge_decision("TASK_ID: nope-missing\nREASON: hallucinated\n", &cands);
        assert!(unknown.is_none());
        assert_eq!(judge_select(cands.clone(), unknown).expect("pick").id, expected);
        // Total failure (gateway down / timeout / non-200) is None too.
        assert_eq!(judge_select(cands, None).expect("pick").id, expected);
    }

    #[test]
    fn judge_next_single_candidate_skips_model() {
        const ONE: &str = "- id: \"solo-1\"\n  task: \"only task\"\n  status: pending\n";
        // No gateway needed: single-candidate queues never call post_chat.
        let picked = judge_next(ONE, Some("Nominal"), "/tmp").expect("solo");
        assert_eq!(picked.id, "solo-1");
        assert!(judge_next("nothing here", Some("Nominal"), "/tmp").is_none());
    }

    #[test]
    fn review_parses_critical_and_major_only() {
        let out = "- [critical] lac.rs:42 — unsanitized id reaches git branch\n- [major] lac.rs:90 — missing error handling on router spawn\n- [minor] lac.rs:12 — typo in comment\n- [suggestion] consider renaming\nNO_FINDINGS is absent\nVERDICT: fix";
        let f = parse_review_findings(out);
        assert_eq!(f.len(), 2, "only critical+major survive: {:?}", f);
        assert!(f[0].contains("lac.rs:42"));
        assert!(f[1].contains("lac.rs:90"));
        // Case-insensitive markers, different bullets.
        let out2 = "* [Critical] a.rs:1 — x\n[MAJOR] b.rs:2 — y";
        assert_eq!(parse_review_findings(out2).len(), 2);
    }

    #[test]
    fn review_approves_clean_output() {
        for clean in [
            "NO_FINDINGS\nVERDICT: approve",
            "- [minor] x.rs:1 — nit\nVERDICT: approve",
            "",
            "Looks fine, no issues.",
            "VERDICT: fix", // verdict line alone is NOT a finding marker
        ] {
            assert!(parse_review_findings(clean).is_empty(), "must approve: {:?}", clean);
        }
    }

    #[test]
    fn review_disposition_caps_rounds() {
        // Clean at any round commits.
        assert_eq!(review_disposition(1, false), "commit");
        assert_eq!(review_disposition(2, false), "commit");
        // Findings on round 1 go back to the coder...
        assert_eq!(review_disposition(1, true), "fix");
        // ...but round 2 findings commit flagged — the loop can never
        // reach round 3 no matter what the model returns.
        assert_eq!(review_disposition(2, true), "commit-flagged");
        assert_eq!(review_disposition(99, true), "commit-flagged");
    }

    #[test]
    fn review_prompt_truncates_and_discloses() {
        let big = "x".repeat(REVIEW_DIFF_LIMIT + 100);
        let p = review_prompt("fix login check", &big);
        assert!(p.contains("fix login check"), "task in prompt");
        assert!(p.contains("truncated"), "truncation disclosed:\n{}", &p[..500]);
        assert!(p.len() < big.len() + 2000, "prompt bounded");
        let small = "diff --git a/x b/x";
        let p2 = review_prompt("t", small);
        assert!(!p2.contains("truncated"), "no false truncation note");
        assert!(p2.contains(small));
    }

    #[test]
    fn review_disabled_flag() {
        let saved = env::var("LOOP_STUDIO_REVIEW").ok();
        // env mutation is process-global; this is the only test that
        // touches LOOP_STUDIO_REVIEW, and it restores the prior value.
        unsafe {
            env::set_var("LOOP_STUDIO_REVIEW", "off");
        }
        assert!(!review_enabled());
        unsafe {
            env::set_var("LOOP_STUDIO_REVIEW", "0");
        }
        assert!(!review_enabled());
        unsafe {
            env::set_var("LOOP_STUDIO_REVIEW", "1");
        }
        assert!(review_enabled());
        unsafe {
            match saved {
                Some(v) => env::set_var("LOOP_STUDIO_REVIEW", v),
                None => env::remove_var("LOOP_STUDIO_REVIEW"),
            }
        }
    }

    #[test]
    fn model_parsed_through_comments() {
        // Comments, block and line, around and inside the assignment.
        let jsonc = "{\n// comment\n\"model\": \"lac/qwen-test\", /* block */\n}";
        assert_eq!(common::jsonc_model_id(jsonc).as_deref(), Some("lac/qwen-test"));
        // A `//` inside the value is part of the value, not a comment.
        assert_eq!(
            common::jsonc_model_id("{\"model\": \"a//b\"}").as_deref(),
            Some("a//b")
        );
        // An escaped quote does not end the value early.
        assert_eq!(
            common::jsonc_model_id("{\"model\": \"a\\\"b\"}").as_deref(),
            Some("a\"b")
        );
        // Absent, non-string, and truncated: all None, none of them a panic.
        for bad in [
            "{}",
            "{\"model\": 42}",
            "{\"model\": \"unterminated",
            // The two that used to take the TUI down: a file truncated right
            // after the key, and a multi-byte character where the old scan
            // assumed one byte.
            "{\n  \"model\"",
            "{\n  \"model\"\u{6a21}\u{578b}",
        ] {
            assert_eq!(common::jsonc_model_id(bad), None, "{bad:?} must not parse or panic");
        }
    }

    #[test]
    fn content_len_handles_escapes() {
        let body = r#"{"choices":[{"message":{"content":"a\"b\\c"}}]}"#;
        // a " b \ c -> 5 chars.
        assert_eq!(content_len_approx(body), Some(5));
        assert_eq!(content_len_approx("{}"), None);
    }

    #[test]
    fn doctor_memory_check_scales_across_macs() {
        // 8GB MacBook Air
        assert_eq!(doctor_memory_check_with(Some(8.0), Some(4.0)).0, "ok");
        assert_eq!(doctor_memory_check_with(Some(8.0), Some(2.0)).0, "warn");
        assert_eq!(doctor_memory_check_with(Some(8.0), Some(1.0)).0, "fail");

        // 16GB MacBook Pro
        assert_eq!(doctor_memory_check_with(Some(16.0), Some(5.0)).0, "ok");
        assert_eq!(doctor_memory_check_with(Some(16.0), Some(2.5)).0, "warn");
        assert_eq!(doctor_memory_check_with(Some(16.0), Some(1.0)).0, "fail");

        // 96GB Mac Studio
        assert_eq!(doctor_memory_check_with(Some(96.0), Some(20.0)).0, "ok");
        assert_eq!(doctor_memory_check_with(Some(96.0), Some(8.0)).0, "warn");
        assert_eq!(doctor_memory_check_with(Some(96.0), Some(2.0)).0, "fail");

        // Missing telemetry
        assert_eq!(doctor_memory_check_with(Some(16.0), None).0, "warn");
    }

    #[test]
    fn intel_hint_never_mentions_mlx() {
        // On Intel hosts, fallback hints must direct to llama or ollama, never MLX.
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
        assert!(NO_BACKEND_HINT.contains("Start one with `loop-studio serve mlx`."));
    }

    #[test]
    fn line_indent_basic() {
        assert_eq!(line_indent(""), 0);
        assert_eq!(line_indent("   "), 3);
        assert_eq!(line_indent("\t\t"), 2);
        assert_eq!(line_indent("abc"), 0);
        assert_eq!(line_indent("  abc"), 2);
    }

    #[test]
    fn block_key_indent_basic() {
        let block = "- id: task-1\n";
        assert_eq!(block_key_indent(block), 2);
        let block2 = "- id:task-1\n";
        assert_eq!(block_key_indent(block2), 2);
    }

    #[test]
    fn block_field_simple() {
        let block = "- id: my-task\n  task: do it\n  status: pending\n  attempts: 0";
        assert_eq!(block_field(block, "task:").as_deref(), Some("do it"));
        assert_eq!(block_field(block, "status:").as_deref(), Some("pending"));
        assert_eq!(block_field(block, "attempts:").as_deref(), Some("0"));
        assert_eq!(block_field(block, "nonexistent:").as_deref(), None);
    }

    #[test]
    fn pull_target_detection() {
        let is_hf = |m: &str| m.contains('/') || m.starts_with("mlx-") || m.contains("MLX") || m.ends_with(".gguf");
        assert!(is_hf("mlx-community/Qwen3.8-27B-4bit"));
        assert!(is_hf("lmstudio-community/Qwen3.8-27B-MLX-4bit"));
        assert!(is_hf("unsloth/Qwen3.6-27B-MTP-GGUF"));
        assert!(!is_hf("qwen3.8-27b"));
        assert!(!is_hf("llama3"));
    }

    #[test]
    fn probe_opts_parse_and_clamp() {
        let d: Vec<String> = vec![];
        let o = parse_probe_opts(&d);
        assert_eq!((o.tokens, o.temp), (64, 0.0));
        let a = ["--tokens".to_string(), "128".to_string(), "--temp".to_string(), "0.7".to_string()];
        let o = parse_probe_opts(&a);
        assert_eq!((o.tokens, o.temp), (128, 0.7));
        let b = ["--max-tokens=4099".to_string(), "--temp=9.0".to_string()];
        let o = parse_probe_opts(&b);
        assert_eq!((o.tokens, o.temp), (4096, 2.0));
        let c = ["--tokens".to_string(), "1".to_string()];
        let o = parse_probe_opts(&c);
        assert_eq!(o.tokens, 8);
    }

    /// The gate has to be able to say "not run" as well as "passed". It used to
    /// have a branch that printed `✓ Tests passed (100% pass rate)` with no
    /// cargo in the process, and that branch is where `--dry-run` and every
    /// checkout without a manifest landed.
    #[test]
    fn a_gate_that_did_not_run_does_not_report_a_pass() {
        let empty = std::env::temp_dir().join(format!("loop-studio-test-no-manifest-{}", std::process::id()));
        let _ = fs::create_dir_all(&empty);
        // No rust-src/Cargo.toml under it: nothing to gate on.
        let gate = run_test_gate(&empty.to_string_lossy(), false);
        assert!(!gate.passed, "a missing suite is not a pass: {}", gate.detail);
        assert!(gate.detail.contains("not run"), "{}", gate.detail);
        let dry = run_test_gate(&empty.to_string_lossy(), true);
        assert!(!dry.passed, "--dry-run runs nothing: {}", dry.detail);
        assert!(dry.detail.contains("--dry-run"), "{}", dry.detail);
        let _ = fs::remove_dir_all(&empty);
    }

    /// `fs::copy(x, x)` truncates x and returns `Ok(0)`, so the self-copy guard
    /// is what stands between an operator's installed binary and a 0-byte one.
    #[test]
    fn a_copy_onto_its_own_source_is_recognized() {
        let dir = std::env::temp_dir().join(format!("loop-studio-test-same-file-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let real = dir.join("LACStudio");
        fs::write(&real, b"binary").unwrap();
        let link = dir.join("LACStudio.link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let absent = dir.join("not-created-yet");
        assert!(same_file(&real.to_string_lossy(), &real.to_string_lossy()));
        assert!(
            same_file(&real.to_string_lossy(), &link.to_string_lossy()),
            "a symlink to the source is the same file"
        );
        assert!(!same_file(&real.to_string_lossy(), &absent.to_string_lossy()));
        assert!(!same_file(&absent.to_string_lossy(), &dir.join("other").to_string_lossy()));
        let _ = fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod host_facts_tests {
    // A `#[test]` that loses its attribute is valid Rust that never runs:
    // the suite goes one test quieter and stays green. Fail the build instead.
    #![deny(dead_code)]

    use super::*;

    /// The router's host block is read from the `host` object, not from any
    /// earlier key that happens to share a name — a backend or a model called
    /// "thermal" must not be read as the thermal state.
    #[test]
    fn the_host_block_is_parsed_from_its_own_object() {
        let body = r#"{"status":"ok","router":"loop-studio-router v2.8","preferred":"auto",
            "active":"mlx","target_port":8080,"uptime_secs":1,"inflight":0,
            "models_mapped":1,"usage_log":"","backends":{},"stats":{},
            "host":{"thermal":"Serious","free_ram_gib":3.2,"total_ram_gib":16.0}}"#;
        let (free, total, thermal) = parse_router_host_facts(body).expect("the block parses");
        assert_eq!(thermal.as_deref(), Some("Serious"));
        assert_eq!(free, Some(3.2));
        assert_eq!(total, Some(16.0));
    }

    /// A document without the block — an older router — is None, and the CLI
    /// falls back to its own probes rather than reporting nothing.
    #[test]
    fn a_document_without_the_block_is_none() {
        let body = r#"{"status":"ok","router":"loop-studio-router v2.7","active":"mlx","target_port":8080}"#;
        assert!(parse_router_host_facts(body).is_none());
    }

    /// A probe that could not answer is `null` in the router's document, and
    /// `null` must stay None here rather than becoming a confident zero.
    #[test]
    fn a_null_reading_stays_unknown() {
        let body = r#"{"host":{"thermal":"Nominal","free_ram_gib":null,"total_ram_gib":null}}"#;
        let (free, total, thermal) = parse_router_host_facts(body).expect("the block parses");
        assert_eq!(thermal.as_deref(), Some("Nominal"));
        assert_eq!(free, None, "null is unknown, not zero");
        assert_eq!(total, None);
    }

    /// A null *thermal* must not be read as the next key's name. The parser
    /// searched for the opening quote, and with `"thermal": null` the next
    /// quote in the block belongs to `"free_ram_gib"` — so the reading was the
    /// literal string `free_ram_gib`, a thermal level no host has ever
    /// reported, and it went into a `Thermal State :` line looking measured.
    #[test]
    fn a_null_thermal_is_not_the_next_keys_name() {
        let body = r#"{"host":{"thermal":null,"free_ram_gib":12.0,"total_ram_gib":64.0}}"#;
        let (free, total, thermal) = parse_router_host_facts(body).expect("the block parses");
        assert_eq!(
            thermal, None,
            "an absent probe must stay absent, not borrow the following key's name"
        );
        assert_eq!(free, Some(12.0), "the rest of the block still parses");
        assert_eq!(total, Some(64.0));
    }

    /// The block's own keys are found even when another key shares the prefix —
    /// `"free_ram_gib"` must not be matched by searching for `"ram"`-suffixed
    /// text elsewhere, and a nested `}` inside a string must not end the block
    /// early.
    #[test]
    fn the_block_ends_at_its_own_brace() {
        let body = r#"{"host":{"thermal":"Nominal","free_ram_gib":1.5,"total_ram_gib":2.5}}"#;
        let (free, total, _) = parse_router_host_facts(body).expect("the block parses");
        assert_eq!(free, Some(1.5));
        assert_eq!(total, Some(2.5));
    }

    #[test]
    fn the_value_helpers_read_what_they_name() {
        let block = r#"{"thermal":"Fair","free_ram_gib":12.0,"total_ram_gib":null}"#;
        assert_eq!(json_string(block, "\"thermal\""), Some("Fair".to_string()));
        assert_eq!(json_string(block, "\"missing\""), None);
        assert_eq!(json_f64(block, "\"free_ram_gib\""), Some(12.0));
        assert_eq!(json_f64(block, "\"total_ram_gib\""), None, "null is not a number");
        assert_eq!(json_f64(block, "\"thermal\""), None, "a string is not a number");
    }
}

#[cfg(test)]
mod dispatch_honesty_tests {
    // A `#[test]` that loses its attribute is valid Rust that never runs:
    // the suite goes one test quieter and stays green. Fail the build instead.
    #![deny(dead_code)]

    use super::*;

    /// `loop-studio bench --tokens 128` used to probe `127.0.0.1:128`.
    ///
    /// The dispatch arm took the first argument that did not begin with `--`
    /// and parsed it as the port, which is the *value* of the flag. The help
    /// line in this file advertises exactly that invocation, and
    /// `LOOP_STUDIO_BENCH_TOKENS` exists to mirror it, so the documented form of the
    /// command was a benchmark of port 128: `Connection failed to port 128`,
    /// exit 1, on a perfectly healthy stack.
    #[test]
    fn a_bench_flag_value_is_not_probed_as_a_port() {
        let s = |x: &str| vec![x.to_string()];
        assert_eq!(
            bench_port(&[], 8000),
            8000,
            "no port argument means the documented default"
        );
        assert_eq!(bench_port(&s("8080"), 8000), 8080, "a bare port is a port");
        // The regression, in every spelling the flag parser accepts.
        for flag in ["--tokens", "--max-tokens", "--max_tokens", "--temp", "--temperature"] {
            let args = vec![flag.to_string(), "128".to_string()];
            assert_eq!(
                bench_port(&args, 8000),
                8000,
                "{flag} 128 must not become port 128"
            );
        }
        // A flag after a real port, and a port after a flag's value.
        let mut mixed = s("--tokens");
        mixed.push("64".to_string());
        mixed.push("9000".to_string());
        assert_eq!(bench_port(&mixed, 8000), 9000);
        // Zero is not a port a client can dial, so it falls back like every
        // other unusable value in this project.
        assert_eq!(bench_port(&s("0"), 8000), 8000);
        assert_eq!(bench_port(&s("70000"), 8000), 8000);
    }

    /// A structural check, and the honest kind: the nine dispatch arms this
    /// replaces cannot be observed from a unit test without running the
    /// binary, so what is asserted is that the erasing pattern is gone. The
    /// behavioural half — that the child's status becomes the process's — is
    /// covered end to end by `tests/e2e_smoke.rs`.
    #[test]
    fn no_dispatch_arm_discards_a_childs_exit_status() {
        let src = include_str!("loop_studio.rs");
        // Only `.status()` erases a status. `.spawn()` and `.output()` are
        // backgrounding and best-effort probes, and are named in the failures
        // when they matter, so the test looks for the one call whose entire
        // purpose is the exit code.
        let offenders: Vec<&str> = src
            .lines()
            // Code, not prose: the doc comment on `exec_sibling` quotes the
            // pattern verbatim to explain what it replaced.
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

    /// The code of `cmd_loop_run`, with comments removed.
    ///
    /// Comments are stripped because the prose inside this function *describes*
    /// the very patterns being asserted against — it says the run used to print
    /// `[LOOP COMPLETED]`. Scanning the raw text would match the explanation
    /// instead of the code, which is how a source check passes for the wrong
    /// reason.
    fn loop_runner_code() -> String {
        include_str!("loop_studio.rs")
            .split("fn cmd_loop_run(")
            .nth(1)
            .expect("cmd_loop_run is missing from its own source")
            .split("fn launchd_src(")
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
        // The only write to the queue's status, from the loop runner.
        assert!(
            !body.contains("update_task(") && !body.contains("update_task_file("),
            "the loop runner dispatches no agent and writes no files, so it must \
             never write a task status; a run that marks a task complete deletes \
             it from the queue while reporting [LOOP COMPLETED]"
        );
        // And it must not claim a loop ran.
        for claim in ["[LOOP COMPLETED]", "Reliability gates passed"] {
            assert!(
                !body.contains(claim),
                "{claim:?} claims a loop completed; this command rehearses phases"
            );
        }
        // The word that is true, instead.
        assert!(
            body.contains("[REHEARSAL COMPLETE]"),
            "a rehearsal must be labelled as one"
        );
    }

    #[test]
    fn a_git_that_did_not_run_is_not_a_clean_tree() {
        let body = loop_runner_code();
        // Only the git sites: a *file* read may legitimately default to empty,
        // and `tasks_content` does. What must not happen is a git result being
        // unwrapped into a string that means "clean".
        for line in body.lines().filter(|l| l.contains("unwrap_or_default()")) {
            assert!(
                !line.contains("git"),
                "a git result is being unwrapped to a default, which is what makes \\
                 a failed git read as a clean tree: {line}"
            );
        }
        // And the two outcomes are separate arms at both reporting sites.
        assert_eq!(
            body.matches("GitOutput::Failed(why)").count(),
            2,
            "both the review phase and the approval gate must handle a failed git"
        );
    }

    /// The two writers of one file must agree, and the disagreement is checked
    /// from *this* side because this is the side the worker's correctness
    /// depends on.
    ///
    /// The queue file has two writers: this worker, and Loop Studio. The Studio
    /// was fixed to emit `'…'` rather than `"…"` — a double-quoted YAML scalar
    /// may not contain an unescaped `"` nor span lines, and a task description
    /// routinely contains both, so the two readers had started disagreeing
    /// about the same task. That fix changed the writer; this reader stripped
    /// only `"`, so `- id: 'a-1'` came back as the id `'a-1'`, quotes included,
    /// and that flowed into the transcript path, the branch name and
    /// `update_task`'s own matching.
    #[test]
    fn the_queue_reader_agrees_with_both_writers() {
        // Exactly what the Studio writes, including the single quotes, the
        // embedded double quotes, a colon in the value, and the flattened
        // second line of a two-line description.
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
        assert_eq!(block_field(block, "- id:").as_deref(), Some("a-1"));
        assert_eq!(
            block_field(block, "task:").as_deref(),
            Some("Fix the \"task:\" key: colon inside"),
            "a value containing its own key name, a colon and a quote"
        );
        assert_eq!(block_field(block, "status:").as_deref(), Some("failed"));
        assert_eq!(
            block_field(block, "attempts:").as_deref(),
            Some("3"),
            "the retry budget must be readable, or the dead-letter is a no-op"
        );

        // The worker's own form still reads, unchanged. A reader that only
        // accepts the newest writer is its own bug.
        let worker_written = "\
- id: \"a-1\"
  task: \"do the thing\"
  status: pending
  attempts: 0
";
        assert_eq!(block_field(worker_written, "- id:").as_deref(), Some("a-1"));
        assert_eq!(block_field(worker_written, "task:").as_deref(), Some("do the thing"));
        assert_eq!(block_field(worker_written, "attempts:").as_deref(), Some("0"));

        // And an unquoted value is left alone, so a hand-edited file still
        // works.
        assert_eq!(unquote("  3  "), "3");
        assert_eq!(unquote("\"x\""), "x");
        assert_eq!(unquote("'x'"), "x");
        // A lone quote is not quoting.
        assert_eq!(unquote("\"unterminated"), "\"unterminated");
        assert_eq!(unquote("'"), "'");
    }

    /// A dead-lettered task must be unreadable as pending, from either writer's
    /// quoting — the whole point of the round trip the Studio now does.
    #[test]
    fn a_dead_lettered_task_is_not_pending_in_either_quoting() {
        for quoting in ["'", "\""] {
            let block = format!(
                "- id: {q}a-1{q}\n  task: {q}do it{q}\n  status: failed\n  attempts: 3\n",
                q = quoting
            );
            let parsed = parse_tasks(&block);
            let task = parsed.first().expect("one task");
            assert_eq!(task.id, "a-1", "quoting {quoting} leaked into the id");
            assert_eq!(task.status, "failed");
            assert_eq!(
                task.attempts, 3,
                "quoting {quoting} made a dead-lettered task look retryable"
            );
            // The predicate the worker actually selects on.
            assert!(
                !parsed
                    .iter()
                    .any(|t| t.status == "pending" && t.attempts < MAX_TASK_ATTEMPTS),
                "a task the worker poisoned must not be re-queued, quoting {quoting}"
            );
        }
    }
}
