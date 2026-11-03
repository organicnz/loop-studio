//! Worker loop: task execution, timeout, logging, and checkpointing.

use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::common;
use super::loop_runner::git_in;
use super::orchestrator::{count_pending, judge_next};
use super::queue::update_task_file;
use super::review::{review_diff, ReviewOutcome};
use super::MAX_TASK_ATTEMPTS;

pub fn task_timeout() -> Duration {
    env::var("LOOP_STUDIO_TASK_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(7200))
}

/// Kill a process tree via the shared helper.
pub fn kill_tree(pid: u32) {
    common::kill_tree(pid);
}

/// Spawn and wait with a hard deadline; kills the whole tree on expiry.
pub fn run_with_timeout(cmd: &str, args: &[&str], dir: &str, timeout: Duration) -> io::Result<bool> {
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
pub fn task_log_path(id: &str) -> String {
    format!(
        "{}/.lac/task-logs/{}-{}.log",
        common::home_dir(),
        id,
        common::now_unix()
    )
}

/// run_with_timeout, but child stdout/stderr tee live to both the console
/// and a per-task transcript.
pub fn run_logged(
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
    let deadline = Instant::now() + Duration::from_secs(10);
    for t in [t_out, t_err].into_iter().flatten() {
        while !t.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    Ok(ok)
}

pub fn worker_current_path() -> String {
    format!("{}/.lac/worker-current", common::home_dir())
}

// ------------------------------------------------------------------ cmd_worker -

pub fn cmd_worker(root: &str, args: &[String]) {
    let continuous = !args.iter().any(|a| a == "--drain" || a == "--once");
    common::spawn_log_rotator(
        common::log_names_for("worker"),
        common::LOG_MAX_BYTES,
        common::LOG_KEEP,
        std::time::Duration::from_secs(300),
    );
    let home = common::home_dir();
    let tasks_file = common::tasks_file();

    let _lock = match common::acquire_lock("loop-studio-worker", 900) {
        Some(l) => l,
        None => {
            eprintln!("Another loop-studio worker holds the lock (~/.lac/locks/loop-studio-worker). Exiting.");
            std::process::exit(1);
        }
    };

    if fs::metadata(worker_current_path()).is_ok() {
        common::log_event("worker_recover_preserved", "abandoned worktree retained for human review");
        println!("Previous worker state found; preserving its worktree for human review.");
        let _ = fs::remove_file(worker_current_path());
    }

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

    if !common::port_up(common::mlx_port()) && !common::port_up(common::llama_port()) && !common::port_up(common::ollama_port()) {
        println!("Notice: no inference engine on MLX/llama/Ollama ports.");
        let hint = if common::mlx_supported() { "loop-studio serve mlx" } else { "loop-studio serve llama" };
        println!("The worker will idle until one appears ('{}').", hint);
    }

    if fs::metadata(&tasks_file).is_err() {
        println!("Initializing task queue at {}...", tasks_file);
        super::loop_runner::cmd_loop(root, &["init".to_string()]);
    }

    let base_branch = common::default_branch_in(root);
    let has_commits = common::repo_has_commits_in(root);
    if !has_commits {
        println!("Notice: repo has no commits yet — branch/commit steps are skipped until the first commit.");
    }

    println!("================================================================");
    println!("  Loop Studio 24/7 AUTONOMOUS WORKER DAEMON (v{})", super::VERSION);
    println!("  Target: {} | Qwen 3.8 27B", common::arch_label());
    println!("  Queue : {}", tasks_file);
    println!("  Base  : {} | Mode: {}", base_branch, if continuous { "Continuous 24/7 Watch" } else { "Batch Drain & Stop" });
    println!("================================================================\n");
    common::log_event("worker_start", &format!("mode={}", if continuous { "watch" } else { "drain" }));

    let kv_bin = common::bin(root, "kv-manage");
    let kv_ok = fs::metadata(&kv_bin).is_ok();

    use std::sync::atomic::{AtomicBool, Ordering};

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
                super::loop_runner::cmd_loop(root, &["init".to_string()]);
                if !continuous {
                    break;
                }
                std::thread::sleep(Duration::from_secs(15));
                continue;
            }
        };

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
            run_logged("make", &["test"], &work_root, Duration::from_secs(600), &logpath, "make test")
                .unwrap_or(false)
        } else {
            false
        };

        use super::review::review_disposition;
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
            continue;
        }
    }
    common::log_event("worker_stop", "exiting");
}
