//! Worker loop: task execution, timeout, logging, and checkpointing.

use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::chat::extract_json_string;
use super::common;
use super::loop_runner::git_in;
use super::orchestrator::{count_pending, judge_next_excluding};
use super::queue::{parse_tasks, update_task_file, Task};
use super::review::{review_diff, ReviewOutcome};
use super::MAX_TASK_ATTEMPTS;

/// Default cap injected into OpenCode runs, overridable per operator.
pub const DEFAULT_CONTEXT_LIMIT: u64 = 32768;
/// Refuse absurd values at the env boundary rather than writing them into a
/// model config the engine will then reject.
const CONTEXT_LIMIT_MIN: u64 = 1024;
const CONTEXT_LIMIT_MAX: u64 = 1_048_576;
const PARALLEL_MAX: usize = 32;

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
fn run_with_timeout_env(
    cmd: &str,
    args: &[&str],
    envs: &[(&str, &str)],
    dir: &str,
    timeout: Duration,
) -> io::Result<bool> {
    let mut child = Command::new(cmd)
        .args(args)
        .envs(envs.iter().copied())
        .current_dir(dir)
        .spawn()?;
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

/// Transcripts live under ~/.loop-studio/task-logs/<id>-<unix>.log.
pub fn task_log_path(id: &str) -> String {
    format!(
        "{}/.loop-studio/task-logs/{}-{}.log",
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
    run_logged_env(cmd, args, &[], dir, timeout, log, header)
}

/// [`run_logged`] with extra child environment variables. The OpenCode
/// dispatch needs a context limit on every invocation, and the child env is
/// the only per-run channel that does not mutate the worktree's config.
#[allow(clippy::too_many_arguments)]
pub fn run_logged_env(
    cmd: &str,
    args: &[&str],
    envs: &[(&str, &str)],
    dir: &str,
    timeout: Duration,
    log: &str,
    header: &str,
) -> io::Result<bool> {
    let file = match fs::OpenOptions::new().create(true).append(true).open(log) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("transcript {} unwritable ({}); running without log", log, e);
            return run_with_timeout_env(cmd, args, envs, dir, timeout);
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
        .envs(envs.iter().copied())
        .current_dir(dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let pump =
        |mut pipe: Box<dyn std::io::Read + Send>, to_stderr: bool, file: Arc<Mutex<fs::File>>| {
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

fn worker_current_path_for(suffix: &str) -> String {
    format!(
        "{}/.loop-studio/worker-current{}",
        common::home_dir(),
        suffix
    )
}

/// Clear stale per-thread markers left by a crash. Returns true when at least
/// one was found, so the caller can print the preservation notice once.
fn recover_worker_markers() -> bool {
    let dir = format!("{}/.loop-studio", common::home_dir());
    let Ok(entries) = fs::read_dir(&dir) else {
        return false;
    };
    let mut found = false;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("worker-current") {
            continue;
        }
        found = true;
        let _ = fs::remove_file(entry.path());
    }
    if found {
        common::log_event(
            "worker_recover_preserved",
            "abandoned worktree retained for human review",
        );
    }
    found
}

// ------------------------------------------------------------- checkpoints --

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerCheckpoint {
    pub task_id: String,
    pub branch: String,
    pub phase: String,
}

fn checkpoint_dir() -> String {
    format!("{}/.local/state/loop-studio", common::home_dir())
}

/// Legacy single-file checkpoint path (also the latest checkpoint in
/// single-threaded mode).
pub fn worker_checkpoint_path() -> String {
    format!("{}/worker-checkpoint.json", checkpoint_dir())
}

fn checkpoint_path_for(task_id: &str) -> String {
    format!("{}/worker-checkpoints/{}.json", checkpoint_dir(), task_id)
}

fn parse_checkpoint(text: &str) -> Option<WorkerCheckpoint> {
    Some(WorkerCheckpoint {
        task_id: extract_json_string(text, "task_id")?,
        branch: extract_json_string(text, "branch")?,
        phase: extract_json_string(text, "phase")?,
    })
}

fn write_worker_checkpoint(task_id: &str, branch: &str, phase: &str, worktree_path: &str) {
    let git_head = git_head_sha(worktree_path);
    let json = format!(
        "{{\"task_id\":\"{}\",\"branch\":\"{}\",\"phase\":\"{}\",\"git_head\":\"{}\",\"timestamp_secs\":{}}}",
        common::json_escape(task_id),
        common::json_escape(branch),
        common::json_escape(phase),
        common::json_escape(&git_head),
        common::now_unix()
    );
    let dir = format!("{}/worker-checkpoints", checkpoint_dir());
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let _ = common::atomic_write(&checkpoint_path_for(task_id), &json);
    write_checkpoint_diff(task_id, phase, worktree_path);
}

/// Current HEAD of the worktree, captured into the checkpoint so a resume or
/// human review can pin the exact commit a phase ran against. Empty when the
/// worktree has no commits yet or git is unavailable.
fn git_head_sha(worktree: &str) -> String {
    Command::new("git")
        .current_dir(worktree)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Snapshot of the working-tree diff at a checkpoint phase, written beside
/// the JSON so an interrupted task leaves a human-reviewable record of what
/// was in flight.
fn write_checkpoint_diff(task_id: &str, phase: &str, worktree: &str) {
    let dir = format!("{}/checkpoints", common::state_dir());
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = format!("{}/{}-{}.diff", dir, task_id, phase);
    let diff = match Command::new("git")
        .current_dir(worktree)
        .args(["diff", "HEAD"])
        .output()
    {
        Ok(o) => String::from_utf8_lossy(&o.stdout).into_owned(),
        Err(_) => return,
    };
    let _ = common::atomic_write(&path, &diff);
}

/// Swap the router backend file from the Q8 llama lane to the lighter Q4 MLX
/// lane when thermals turn Serious. Returns true only when a swap happened.
fn try_throttle_router_backend() -> bool {
    let path = format!("{}/router-backend", common::state_dir());
    let current = fs::read_to_string(&path).unwrap_or_default();
    if current.contains("llama") && common::atomic_write(&path, "mlx").is_ok() {
        common::log_event("thermal_throttle", "swapped llama→mlx for thermal safety");
        return true;
    }
    false
}

/// One line per thermal decision, for an operator watching pressure over time.
fn log_thermal(label: &str, action: &str) {
    let dir = format!("{}/thermal-log", common::state_dir());
    let _ = fs::create_dir_all(&dir);
    let path = format!("{}/thermal.csv", dir);
    if let Ok(mut f) = fs::OpenOptions::new().append(true).create(true).open(&path) {
        let _ = writeln!(f, "{},{},{}", common::now_unix(), label, action);
    }
}

/// Estimated context tokens from `kv-manage check`, or None when the probe or
/// the parse came up empty.
fn kv_check_tokens(kv_bin: &str) -> Option<u64> {
    let out = Command::new(kv_bin).arg("check").output().ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let at = stdout.find("tokens:")? + "tokens:".len();
    let rest = stdout[at..].trim_start();
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    rest[..end].parse().ok()
}

fn clear_worker_checkpoint(task_id: &str) {
    let _ = fs::remove_file(checkpoint_path_for(task_id));
    if let Ok(text) = fs::read_to_string(worker_checkpoint_path()) {
        if extract_json_string(&text, "task_id").as_deref() == Some(task_id) {
            let _ = fs::remove_file(worker_checkpoint_path());
        }
    }
}

/// Every per-task checkpoint, newest first by file mtime where available.
/// Falls back to the legacy single file when no per-task files exist.
fn read_worker_checkpoints() -> Vec<WorkerCheckpoint> {
    let dir = format!("{}/worker-checkpoints", checkpoint_dir());
    let mut found: Vec<(u64, WorkerCheckpoint)> = Vec::new();
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let stamp = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if let Ok(text) = fs::read_to_string(&path) {
                if let Some(cp) = parse_checkpoint(&text) {
                    found.push((stamp, cp));
                }
            }
        }
    }
    found.sort_by_key(|(stamp, _)| std::cmp::Reverse(*stamp));
    let mut out: Vec<WorkerCheckpoint> = found.into_iter().map(|(_, cp)| cp).collect();
    if out.is_empty() {
        if let Ok(text) = fs::read_to_string(worker_checkpoint_path()) {
            if let Some(cp) = parse_checkpoint(&text) {
                out.push(cp);
            }
        }
    }
    out
}

/// A checkpoint only resumes a task that is still retryable and whose branch
/// still exists. Anything else is cleared rather than reported as resumable.
fn resume_candidate(root: &str, tasks_file: &str, checkpoint: &WorkerCheckpoint) -> Option<Task> {
    let content = fs::read_to_string(tasks_file).ok()?;
    let task = parse_tasks(&content).into_iter().find(|t| {
        t.id == checkpoint.task_id && t.status == "pending" && t.attempts < MAX_TASK_ATTEMPTS
    })?;
    let branch_ref = format!("refs/heads/{}", checkpoint.branch);
    if !git_in(root, &["show-ref", "--verify", "--quiet", &branch_ref]).succeeded() {
        return None;
    }
    Some(task)
}

/// Existing worktree for a branch, if git still has one registered. Reusing it
/// is what turns a checkpoint into a resume instead of a "branch already
/// checked out" failure on the next `worktree add`.
fn find_worktree_for_branch(root: &str, branch: &str) -> Option<String> {
    let super::loop_runner::GitOutput::Text(out) =
        git_in(root, &["worktree", "list", "--porcelain"])
    else {
        return None;
    };
    let want = format!("branch refs/heads/{}", branch);
    let mut current: Option<String> = None;
    for line in out.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            current = Some(path.to_string());
        } else if line.trim() == want {
            return current.filter(|path| Path::new(path).is_dir());
        }
    }
    None
}

// ------------------------------------------------------------ context limit --

fn context_limit() -> u64 {
    env::var("LOOP_STUDIO_CONTEXT_LIMIT")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_CONTEXT_LIMIT)
        .clamp(CONTEXT_LIMIT_MIN, CONTEXT_LIMIT_MAX)
}

fn valid_config_component(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// JSON overlay that lowers the model's declared context window. OpenCode V2
/// has no `--context-limit` flag; `providers.<p>.models.<m>.limit.context` is
/// the supported knob, and a project-local `.opencode/opencode.json` overrides
/// the repository's `opencode.jsonc` without touching a tracked file.
fn model_override_json(model: &str, limit: u64) -> Option<String> {
    let model = model.split('#').next().unwrap_or(model);
    let (provider, model_id) = model.split_once('/')?;
    if !valid_config_component(provider) || !valid_config_component(model_id) {
        return None;
    }
    let mut json = String::from("{\"providers\":{\"");
    json.push_str(provider);
    json.push_str("\":{\"models\":{\"");
    json.push_str(model_id);
    json.push_str("\":{\"limit\":{\"context\":");
    json.push_str(&limit.to_string());
    json.push_str("}}}}}}\n");
    Some(json)
}

/// Only override a provider the project already declares; inventing a provider
/// block with no package/baseURL would break model resolution instead of
/// lowering its context window.
fn provider_declared(root: &str, provider: &str) -> bool {
    let Ok(text) = fs::read_to_string(format!("{}/opencode.jsonc", root)) else {
        return false;
    };
    let text = common::strip_jsonc_comments(&text);
    text.contains(&format!("\"{}\"", provider))
}

struct ContextOverride {
    path: String,
}

impl Drop for ContextOverride {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Whether an existing overlay is exactly one this worker wrote. A crashed
/// run leaves the file behind; adopting it (and removing it on drop) keeps it
/// out of the next task's `git add -A` commit. A richer file is someone
/// else's config and is left alone.
fn is_worker_overlay(text: &str, model: &str) -> bool {
    let model = model.split('#').next().unwrap_or(model);
    let Some((provider, model_id)) = model.split_once('/') else {
        return false;
    };
    if !valid_config_component(provider) || !valid_config_component(model_id) {
        return false;
    }
    let mut prefix = String::from("{\"providers\":{\"");
    prefix.push_str(provider);
    prefix.push_str("\":{\"models\":{\"");
    prefix.push_str(model_id);
    prefix.push_str("\":{\"limit\":{\"context\":");
    let Some(rest) = text.strip_prefix(&prefix) else {
        return false;
    };
    let Some(digits) = rest.strip_suffix("}}}}}}\n") else {
        return false;
    };
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

fn apply_context_override(
    root: &str,
    work_root: &str,
    model: &str,
    limit: u64,
) -> Option<ContextOverride> {
    let json = model_override_json(model, limit)?;
    let provider = model
        .split('#')
        .next()
        .unwrap_or(model)
        .split_once('/')
        .map(|(provider, _)| provider)?;
    if !provider_declared(root, provider) {
        return None;
    }
    let dir = format!("{}/.opencode", work_root);
    let path = format!("{}/opencode.json", dir);
    // Never clobber a config the task owns; only replace an overlay we wrote
    // ourselves on a previous, crashed attempt.
    if Path::new(&path).exists()
        && !fs::read_to_string(&path)
            .map(|text| is_worker_overlay(&text, model))
            .unwrap_or(false)
    {
        return None;
    }
    if fs::create_dir_all(&dir).is_err() || fs::write(&path, json).is_err() {
        return None;
    }
    Some(ContextOverride { path })
}

// ------------------------------------------------------ thermal / claims ----

/// One pause deadline shared by every worker thread: a thread that sees
/// Critical sets the deadline, and all threads wait out the same window rather
/// than each sleeping and resetting its own flag.
struct ThermalGate {
    until: Mutex<Instant>,
}

impl ThermalGate {
    fn new() -> Self {
        Self {
            until: Mutex::new(Instant::now()),
        }
    }

    fn pause_for(&self, duration: Duration) {
        let mut until = self.until.lock().unwrap_or_else(|e| e.into_inner());
        let target = Instant::now() + duration;
        if target > *until {
            *until = target;
        }
    }

    fn wait(&self) {
        loop {
            let left = {
                let until = self.until.lock().unwrap_or_else(|e| e.into_inner());
                (*until).saturating_duration_since(Instant::now())
            };
            if left.is_zero() {
                break;
            }
            std::thread::sleep(left.min(Duration::from_secs(5)));
        }
    }
}

struct WorkerShared {
    thermal: ThermalGate,
    claims: Mutex<HashSet<String>>,
    resume: Mutex<Vec<Task>>,
    /// Serializes queue writes between threads. `update_task_file` takes an
    /// inter-process directory lock keyed on the pid, and every thread here
    /// shares that pid — two threads contending would look to the second one
    /// like a foreign live owner and silently drop its status update.
    queue: Mutex<()>,
}

struct ClaimGuard<'a> {
    claims: &'a Mutex<HashSet<String>>,
    id: String,
}

impl Drop for ClaimGuard<'_> {
    fn drop(&mut self) {
        let mut claims = self.claims.lock().unwrap_or_else(|e| e.into_inner());
        claims.remove(&self.id);
    }
}

struct WorkerConfig<'a> {
    root: &'a str,
    tasks_file: &'a str,
    home: &'a str,
    base_branch: &'a str,
    kv_bin: &'a str,
    model: &'a str,
    marker_suffix: &'a str,
    kv_ok: bool,
    has_commits: bool,
    continuous: bool,
    #[allow(dead_code)] // reserved for future parallel-worker coordination
    parallel: usize,
    ctx_limit: u64,
}

fn claim_next(
    cfg: &WorkerConfig,
    shared: &WorkerShared,
    content: &str,
    thermal: Option<&str>,
) -> Option<Task> {
    loop {
        let excluded = {
            let claims = shared.claims.lock().unwrap_or_else(|e| e.into_inner());
            claims.clone()
        };

        // A resumed checkpoint wins over fresh judgment, but only while the
        // task is still pending in the queue re-read above.
        let from_resume = {
            let mut resume = shared.resume.lock().unwrap_or_else(|e| e.into_inner());
            if resume.is_empty() {
                None
            } else {
                let candidate = resume.remove(0);
                let live = parse_tasks(content).into_iter().find(|t| {
                    t.id == candidate.id && t.status == "pending" && t.attempts < MAX_TASK_ATTEMPTS
                });
                if live.is_none() {
                    clear_worker_checkpoint(&candidate.id);
                }
                live
            }
        };

        let candidate = match from_resume {
            Some(task) => Some(task),
            None => judge_next_excluding(content, thermal, cfg.root, &excluded),
        };
        let task = candidate?;

        let mut claims = shared.claims.lock().unwrap_or_else(|e| e.into_inner());
        if claims.insert(task.id.clone()) {
            return Some(task);
        }
        // Another thread won the race between judgment and the claim. Retry.
    }
}

/// Queue mutations are serialized in-process as well as on disk; see the
/// `WorkerShared::queue` comment for why the on-disk lock alone is not enough.
fn update_task_locked(
    cfg: &WorkerConfig,
    shared: &WorkerShared,
    id: &str,
    status: &str,
    bump_attempts: bool,
) -> bool {
    let _guard = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
    update_task_file(cfg.tasks_file, id, status, bump_attempts)
}

// ------------------------------------------------------------------ cmd_worker -

fn parse_parallel(args: &[String]) -> usize {
    let mut parallel = 1usize;
    let mut i = 0;
    while i < args.len() {
        if let Some(value) = args[i].strip_prefix("--parallel=") {
            parallel = value.parse().unwrap_or(1);
            i += 1;
        } else if (args[i] == "--parallel" || args[i] == "-p") && i + 1 < args.len() {
            parallel = args[i + 1].parse().unwrap_or(1);
            i += 2;
        } else {
            i += 1;
        }
    }
    parallel.clamp(1, PARALLEL_MAX)
}

pub fn cmd_worker(root: &str, args: &[String]) {
    let continuous = !args.iter().any(|a| a == "--drain" || a == "--once");
    let ctx_limit = context_limit();
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
            eprintln!(
                "Another loop-studio worker holds the lock (~/.loop-studio/locks/loop-studio-worker). Exiting."
            );
            std::process::exit(1);
        }
    };

    if recover_worker_markers() {
        println!("Previous worker state found; preserving its worktree for human review.");
    }

    if !common::port_up(common::gateway_port()) {
        let router_bin = common::bin(root, "loop-studio-router");
        if fs::metadata(&router_bin).is_ok() {
            println!(
                "Starting Loop Studio Unified Gateway on :{} in background...",
                common::gateway_port()
            );
            match Command::new(&router_bin)
                .env(
                    "LOOP_STUDIO_ROUTER_PORT",
                    common::gateway_port().to_string(),
                )
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(child) => {
                    common::persist_or_warn(
                        &format!("{}/.loop-studio/router.pid", home),
                        &child.id().to_string(),
                        "router_pid",
                    );
                }
                Err(e) => println!("  [!] Router spawn failed: {}", e),
            }
            if common::wait_for_port(common::gateway_port(), Duration::from_secs(10)) {
                println!(
                    "  [ok] Loop Studio Gateway online on http://127.0.0.1:{}/v1",
                    common::gateway_port()
                );
            } else {
                println!(
                    "  [!] Gateway did not come up; worker continues, backends probed directly."
                );
            }
        } else {
            println!(
                "  [!] Router binary missing ({}); run `make build` first.",
                router_bin
            );
        }
    }

    if !common::port_up(common::mlx_port())
        && !common::port_up(common::llama_port())
        && !common::port_up(common::ollama_port())
    {
        println!("Notice: no inference engine on MLX/llama/Ollama ports.");
        let hint = if common::mlx_supported() {
            "loop-studio serve mlx"
        } else {
            "loop-studio serve llama"
        };
        println!("The worker will idle until one appears ('{}').", hint);
    }

    if fs::metadata(&tasks_file).is_err() {
        println!("Initializing task queue at {}...", tasks_file);
        super::loop_runner::cmd_loop(root, &["init".to_string()]);
    }

    let base_branch = common::default_branch_in(root);
    let has_commits = common::repo_has_commits_in(root);
    if !has_commits {
        println!(
            "Notice: repo has no commits yet — branch/commit steps are skipped until the first commit."
        );
    }
    // Without commits there is no branch to isolate a worktree on, so every
    // thread would mutate the operator tree. Parallelism is only safe once a
    // task can own its own checkout.
    let requested_parallel = parse_parallel(args);
    let parallel = if has_commits { requested_parallel } else { 1 };
    if requested_parallel > 1 && parallel == 1 {
        println!(
            "Notice: --parallel {} requested, but parallel workers need commits for isolated worktrees; running single-threaded.",
            requested_parallel
        );
    }

    // Resolve every checkpoint whose branch and queue entry still exist. The
    // rest are cleared: a stale checkpoint must not look like a resume.
    let mut resume: Vec<Task> = Vec::new();
    for checkpoint in read_worker_checkpoints() {
        match resume_candidate(root, &tasks_file, &checkpoint) {
            Some(task) => {
                println!(
                    "Resuming from checkpoint: task={} branch={} phase={}",
                    checkpoint.task_id, checkpoint.branch, checkpoint.phase
                );
                common::log_event(
                    "worker_resume",
                    &format!(
                        "task={} branch={} phase={}",
                        checkpoint.task_id, checkpoint.branch, checkpoint.phase
                    ),
                );
                resume.push(task);
            }
            None => clear_worker_checkpoint(&checkpoint.task_id),
        }
    }

    println!("================================================================");
    println!(
        "  Loop Studio 24/7 AUTONOMOUS WORKER DAEMON (v{})",
        super::VERSION
    );
    println!("  Target: {} | Qwen 3.8 27B", common::arch_label());
    println!("  Queue : {}", tasks_file);
    println!(
        "  Base  : {} | Mode: {}",
        base_branch,
        if continuous {
            "Continuous 24/7 Watch"
        } else {
            "Batch Drain & Stop"
        }
    );
    println!("  Context limit: {} tokens", ctx_limit);
    if parallel > 1 {
        println!("  Threads: {} parallel workers", parallel);
    }
    println!("================================================================\n");
    common::log_event(
        "worker_start",
        &format!("mode={}", if continuous { "watch" } else { "drain" }),
    );

    let kv_bin = common::bin(root, "kv-manage");
    let kv_ok = fs::metadata(&kv_bin).is_ok();
    let model = super::ops::opencode_model(root);

    let shared = WorkerShared {
        thermal: ThermalGate::new(),
        claims: Mutex::new(HashSet::new()),
        resume: Mutex::new(resume),
        queue: Mutex::new(()),
    };

    if parallel > 1 {
        println!("Starting {} parallel worker threads...", parallel);
        // The suffixes live for the whole scope; each thread borrows one.
        let marker_suffixes: Vec<String> = (0..parallel).map(|idx| format!("-p{}", idx)).collect();
        std::thread::scope(|scope| {
            for idx in 0..parallel {
                let cfg = WorkerConfig {
                    root,
                    tasks_file: &tasks_file,
                    home: &home,
                    base_branch: &base_branch,
                    kv_bin: &kv_bin,
                    model: &model,
                    marker_suffix: &marker_suffixes[idx],
                    kv_ok,
                    has_commits,
                    continuous,
                    parallel,
                    ctx_limit,
                };
                let shared = &shared;
                let spawned = std::thread::Builder::new()
                    .name(format!("worker-{}", idx))
                    .spawn_scoped(scope, move || worker_thread_loop(&cfg, shared));
                if let Err(e) = spawned {
                    eprintln!("worker thread {} failed to start: {}", idx, e);
                }
            }
        });
        common::log_event("worker_stop", "parallel workers exiting");
        return;
    }

    let cfg = WorkerConfig {
        root,
        tasks_file: &tasks_file,
        home: &home,
        base_branch: &base_branch,
        kv_bin: &kv_bin,
        model: &model,
        marker_suffix: "",
        kv_ok,
        has_commits,
        continuous,
        parallel,
        ctx_limit,
    };
    worker_thread_loop(&cfg, &shared);
    common::log_event("worker_stop", "exiting");
}

fn worker_thread_loop(cfg: &WorkerConfig, shared: &WorkerShared) {
    loop {
        shared.thermal.wait();
        let thermal = common::thermal_state();
        let thermal_label = common::thermal_label(&thermal).to_string();
        if thermal.as_deref() == Some("Critical") {
            log_thermal(&thermal_label, "pause-300s");
            println!("Thermal Critical! Pausing all workers 300s...");
            common::log_event("worker_thermal", "critical: sleeping 300s");
            shared.thermal.pause_for(Duration::from_secs(300));
            continue;
        }
        if thermal.as_deref() == Some("Serious") {
            let action = if try_throttle_router_backend() {
                "throttle-q8-to-q4"
            } else {
                "throttle-60s"
            };
            log_thermal(&thermal_label, action);
            println!("Thermal Serious: throttling 60s before next task...");
            common::log_event("worker_thermal", "serious: sleeping 60s");
            shared.thermal.pause_for(Duration::from_secs(60));
            continue;
        }

        log_thermal(&thermal_label, "continue");

        let content = match fs::read_to_string(cfg.tasks_file) {
            Ok(c) => c,
            Err(_) => {
                println!("No tasks file at {}. Reinitializing...", cfg.tasks_file);
                super::loop_runner::cmd_loop(cfg.root, &["init".to_string()]);
                if !cfg.continuous {
                    break;
                }
                std::thread::sleep(Duration::from_secs(15));
                continue;
            }
        };

        let task = match claim_next(cfg, shared, &content, thermal.as_deref()) {
            Some(t) => t,
            None => {
                let (pending, dead) = count_pending(&content);
                println!(
                    "Queue idle ({} retryable pending, {} dead-letter). Thermals: {}.",
                    pending,
                    dead,
                    common::thermal_label(&common::thermal_state())
                );
                if cfg.kv_ok {
                    let _ = Command::new(cfg.kv_bin).arg("check").output();
                }
                if !cfg.continuous && pending == 0 {
                    println!("All tasks drained. Worker exiting.");
                    break;
                }
                println!("Sleeping 15s before next scan (Ctrl+C stops)...");
                std::thread::sleep(Duration::from_secs(15));
                continue;
            }
        };

        let _claim = ClaimGuard {
            claims: &shared.claims,
            id: task.id.clone(),
        };
        let task_id = task.id.clone();
        let task_branched = Arc::new(AtomicBool::new(false));

        let branch = format!("task/{}", task.id);
        let worktree_path = find_worktree_for_branch(cfg.root, &branch).unwrap_or_else(|| {
            format!(
                "{}/.loop-studio/worker-worktrees/{}-a{}-{}-{}",
                cfg.home,
                task.id,
                task.attempts + 1,
                std::process::id(),
                std::thread::current().name().unwrap_or("worker")
            )
        });

        let survived = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            println!(
                "\nProcessing queued task [{}]: \"{}\" (attempt {}/{})",
                task.id,
                task.desc,
                task.attempts + 1,
                MAX_TASK_ATTEMPTS
            );
            common::log_event("task_start", &format!("[{}] {}", task.id, task.desc));
            common::persist_or_warn(
                &worker_current_path_for(cfg.marker_suffix),
                &format!("id: {}\nbase: {}\n", task.id, cfg.base_branch),
                "worker_current",
            );

            if cfg.kv_ok {
                println!("  [1/4] KV hygiene checkpoint...");
                let _ = Command::new(cfg.kv_bin)
                    .args(["truncate", "--tokens", "16000", "--keep", "8000"])
                    .output();
            }

            let was_dirty = common::repo_dirty_in(cfg.root);
            let branched: Option<String> = if cfg.has_commits {
                let _ = fs::create_dir_all(
                    Path::new(&worktree_path)
                        .parent()
                        .unwrap_or_else(|| Path::new(cfg.home)),
                );
                if find_worktree_for_branch(cfg.root, &branch).is_some() {
                    println!("  [2/4] Reusing checkpointed worktree: {}", branch);
                    Some(branch.clone())
                } else {
                    println!("  [2/4] Isolated worktree: {}", branch);
                    let branch_ref = format!("refs/heads/{}", branch);
                    let branch_exists =
                        git_in(cfg.root, &["show-ref", "--verify", "--quiet", &branch_ref])
                            .succeeded();
                    if branch_exists {
                        // A crashed run can leave a registration whose
                        // directory is gone; prune it or the add below fails
                        // with "branch is already checked out".
                        let _ = git_in(cfg.root, &["worktree", "prune"]);
                    }
                    let added = if branch_exists {
                        git_in(cfg.root, &["worktree", "add", &worktree_path, &branch])
                    } else {
                        git_in(
                            cfg.root,
                            &["worktree", "add", "-b", &branch, &worktree_path],
                        )
                    }
                    .succeeded();
                    if added {
                        Some(branch.clone())
                    } else {
                        println!(
                            "  [!] Worktree creation failed; refusing to mutate the operator tree."
                        );
                        None
                    }
                }
            } else {
                println!("  [2/4] No commits yet — working tree used directly.");
                None
            };
            let work_root = branched
                .as_ref()
                .map(|_| worktree_path.clone())
                .unwrap_or_else(|| cfg.root.to_string());
            task_branched.store(branched.is_some(), Ordering::SeqCst);
            if cfg.has_commits && branched.is_none() {
                let bumped = task.attempts + 1;
                let status = if bumped >= MAX_TASK_ATTEMPTS {
                    println!(
                        "  Task [{}] reached {} attempts — dead-letter.",
                        task.id, MAX_TASK_ATTEMPTS
                    );
                    common::log_event("task_dead_letter", &format!("[{}]", task.id));
                    "failed"
                } else {
                    "pending"
                };
                if bumped >= MAX_TASK_ATTEMPTS {
                    clear_worker_checkpoint(&task.id);
                }
                if !update_task_locked(cfg, shared, &task.id, status, true) {
                    common::log_event("task_update_failed", &format!("task {}", task.id));
                }
                let _ = fs::remove_file(worker_current_path_for(cfg.marker_suffix));
                return;
            }

            write_worker_checkpoint(&task.id, &branch, "Dispatch", &work_root);

            let context_override = if branched.is_some() {
                apply_context_override(cfg.root, &work_root, cfg.model, cfg.ctx_limit)
            } else {
                None
            };
            let ctx_env = cfg.ctx_limit.to_string();
            let opencode_env = [("OPENCODE_CONTEXT_LIMIT", ctx_env.as_str())];

            println!(
                "  [3/4] Dispatching to OpenCode V2 (@coder), context limit {}...",
                cfg.ctx_limit
            );
            let prompt = format!(
                "Task: {}. Implement surgically, run project tests, and ensure zero failures.",
                task.desc
            );
            let timeout = task_timeout();
            let logpath = task_log_path(&task.id);
            // Auto-save a "Running" checkpoint during long dispatches so a
            // crash mid-task still leaves a recent diff snapshot to resume from.
            let autosave_stop = Arc::new(AtomicBool::new(false));
            let autosave_state = Arc::new(Mutex::new((task.id.clone(), branch.clone())));
            let autosave_handle = {
                let stop = Arc::clone(&autosave_stop);
                let state = Arc::clone(&autosave_state);
                let wt = work_root.clone();
                std::thread::spawn(move || {
                    let interval = env::var("LOOP_STUDIO_CHECKPOINT_INTERVAL")
                        .ok()
                        .and_then(|v| v.trim().parse::<u64>().ok())
                        .unwrap_or(300);
                    let tick = Duration::from_secs(1);
                    let mut since = 0u64;
                    loop {
                        std::thread::sleep(tick);
                        if stop.load(Ordering::SeqCst) {
                            return;
                        }
                        since += tick.as_secs();
                        if since >= interval {
                            since = 0;
                            let (tid, br) = state.lock().unwrap_or_else(|e| e.into_inner()).clone();
                            write_worker_checkpoint(&tid, &br, "Running", &wt);
                        }
                    }
                })
            };
            let mut impl_ok = run_logged_env(
                "opencode2",
                &["run", &prompt],
                &opencode_env,
                &work_root,
                timeout,
                &logpath,
                &format!("opencode2 task {}", task.id),
            )
            .unwrap_or(false);
            autosave_stop.store(true, Ordering::SeqCst);
            let _ = autosave_handle.join();

            write_worker_checkpoint(&task.id, &branch, "TestGate", &work_root);
            let mut test_ok = if impl_ok {
                println!("  [3b/4] Reliability gate: project tests...");
                run_logged(
                    "make",
                    &["test"],
                    &work_root,
                    Duration::from_secs(600),
                    &logpath,
                    "make test",
                )
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
                    if round > 1 && cfg.kv_ok {
                        if let Some(tokens) = kv_check_tokens(cfg.kv_bin) {
                            if tokens * 100 > cfg.ctx_limit * 90 {
                                let _ = Command::new(cfg.kv_bin)
                                    .args(["truncate", "--tokens", "16000", "--keep", "8000"])
                                    .output();
                                common::log_event("context_truncate", "between review rounds");
                            }
                        }
                    }
                    write_worker_checkpoint(&task.id, &branch, "Review", &work_root);
                    match review_diff(&work_root, &task.id, &task.desc, &logpath) {
                        ReviewOutcome::Approve => {
                            println!("  [3c/4] Reviewer round {}: approve.", round);
                            common::log_event(
                                "review_approve",
                                &format!("[{}] round {}", task.id, round),
                            );
                            break;
                        }
                        ReviewOutcome::Unavailable(reason) => {
                            println!(
                                "  [3c/4] Reviewer unavailable ({}); proceeding on test gate.",
                                reason
                            );
                            common::log_event(
                                "review_unavailable",
                                &format!("[{}] {}", task.id, reason),
                            );
                            break;
                        }
                        ReviewOutcome::Fix(findings) => match review_disposition(round, true) {
                            "fix" => {
                                println!(
                                    "  [3c/4] Reviewer round {}: {} critical/major finding(s) — back to @coder...",
                                    round,
                                    findings.len()
                                );
                                common::log_event(
                                    "review_fix",
                                    &format!(
                                        "[{}] round {} findings {}",
                                        task.id,
                                        round,
                                        findings.len()
                                    ),
                                );
                                let fix_prompt = format!(
                                    "Task: {}. Address these review findings (critical and major ONLY; leave minor/suggestions untouched):\n{}\nRe-run project tests and ensure zero failures.",
                                    task.desc,
                                    findings.join("\n")
                                );
                                impl_ok = run_logged_env(
                                    "opencode2",
                                    &["run", &fix_prompt],
                                    &opencode_env,
                                    &work_root,
                                    timeout,
                                    &logpath,
                                    &format!("opencode2 fix {} r{}", task.id, round),
                                )
                                .unwrap_or(false);
                                test_ok = if impl_ok {
                                    println!(
                                        "  [3b/4] Reliability gate (post-fix): project tests..."
                                    );
                                    run_logged(
                                        "make",
                                        &["test"],
                                        &work_root,
                                        Duration::from_secs(600),
                                        &logpath,
                                        "make test",
                                    )
                                    .unwrap_or(false)
                                } else {
                                    false
                                };
                                if !(impl_ok && test_ok) {
                                    break;
                                }
                            }
                            _ => {
                                println!(
                                    "  [3c/4] Reviewer round {}: still {} critical/major finding(s) after {} rounds — committing to task branch FLAGGED for human merge review.",
                                    round,
                                    findings.len(),
                                    round
                                );
                                common::log_event(
                                    "review_unresolved",
                                    &format!(
                                        "[{}] {} finding(s) after {} rounds",
                                        task.id,
                                        findings.len(),
                                        round
                                    ),
                                );
                                review_flagged = true;
                                break;
                            }
                        },
                    }
                }
            }

            // The overlay is untracked by design; remove it before `git add -A`
            // so a task commit can never contain the worker's own config.
            drop(context_override);

            if impl_ok && test_ok {
                println!(
                    "  [4/4] Tests passed{}.",
                    if review_flagged {
                        " (review findings unresolved — flagged)"
                    } else {
                        ""
                    }
                );
                if cfg.has_commits && branched.is_some() {
                    let _ = git_in(&work_root, &["add", "-A"]);
                    let msg = format!(
                        "feat({}): {}",
                        task.id,
                        task.desc.chars().take(120).collect::<String>()
                    );
                    let _ = git_in(&work_root, &["commit", "-m", &msg]);
                } else if was_dirty {
                    println!(
                        "  Operator tree was dirty; changes remain uncommitted for human review."
                    );
                    review_flagged = true;
                }
                println!("  Task [{}] complete.", task.id);
                common::log_event(
                    "task_complete",
                    &format!("[{}] review_flagged={}", task.id, review_flagged),
                );
                clear_worker_checkpoint(&task.id);
                if !update_task_locked(cfg, shared, &task.id, "complete", false) {
                    common::log_event("task_update_failed", &format!("task {}", task.id));
                }
            } else {
                println!("  Task [{}] failed reliability gate.", task.id);
                common::log_event(
                    "task_failed",
                    &format!("[{}] attempt {}", task.id, task.attempts + 1),
                );
                if branched.is_some() {
                    let stash_label = format!(
                        "loop-studio failed task {} attempt {}",
                        task.id,
                        task.attempts + 1
                    );
                    if git_in(&work_root, &["stash", "push", "-u", "-m", &stash_label]).succeeded()
                    {
                        println!(
                            "  Preserved failed-task changes in git stash '{}'.",
                            stash_label
                        );
                    } else {
                        println!(
                            "  Could not preserve failed-task changes; leaving the worktree for manual recovery."
                        );
                    }
                    if !git_in(cfg.root, &["worktree", "remove", "--force", &worktree_path])
                        .succeeded()
                    {
                        println!("  Failed worktree remains at {}.", worktree_path);
                    }
                } else if was_dirty {
                    println!("  Leaving working tree untouched (was dirty before task).");
                }
                let bumped = task.attempts + 1;
                let status = if bumped >= MAX_TASK_ATTEMPTS {
                    println!(
                        "  Task [{}] reached {} attempts — dead-letter.",
                        task.id, MAX_TASK_ATTEMPTS
                    );
                    common::log_event("task_dead_letter", &format!("[{}]", task.id));
                    "failed"
                } else {
                    "pending"
                };
                if bumped >= MAX_TASK_ATTEMPTS {
                    clear_worker_checkpoint(&task.id);
                }
                if !update_task_locked(cfg, shared, &task.id, status, true) {
                    common::log_event("task_update_failed", &format!("task {}", task.id));
                }
            }

            if branched.is_some() {
                if !git_in(cfg.root, &["worktree", "remove", "--force", &worktree_path]).succeeded()
                {
                    common::log_event(
                        "worker_cleanup_blocked",
                        &format!("task {} worktree remains at {}", task.id, worktree_path),
                    );
                } else {
                    let _ = fs::remove_file(worker_current_path_for(cfg.marker_suffix));
                }
            } else if !was_dirty {
                let _ = fs::remove_file(worker_current_path_for(cfg.marker_suffix));
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
            println!(
                "  Task [{}] panicked; daemon survives. Counting a strike.",
                task_id
            );
            common::log_event(
                "task_panic",
                &format!("[{}] worker caught panic; state reset", task_id),
            );
            if task_branched.load(Ordering::SeqCst) {
                let panic_stash = format!(
                    "loop-studio panicked task {} attempt {}",
                    task_id,
                    task.attempts + 1
                );
                let _ = git_in(&worktree_path, &["stash", "push", "-u", "-m", &panic_stash]);
                if git_in(cfg.root, &["worktree", "remove", "--force", &worktree_path]).succeeded()
                {
                    let _ = fs::remove_file(worker_current_path_for(cfg.marker_suffix));
                } else {
                    common::log_event(
                        "worker_recovery_blocked",
                        &format!("task {} worktree remains at {}", task_id, worktree_path),
                    );
                }
            } else {
                common::log_event(
                    "worker_recovery_blocked",
                    &format!("task {} preserved unbranched tree", task_id),
                );
            }
            let bumped = task.attempts + 1;
            let status = if bumped >= MAX_TASK_ATTEMPTS {
                println!(
                    "  Task [{}] reached {} attempts — dead-letter.",
                    task_id, MAX_TASK_ATTEMPTS
                );
                common::log_event("task_dead_letter", &format!("[{}]", task_id));
                "failed"
            } else {
                "pending"
            };
            if bumped >= MAX_TASK_ATTEMPTS {
                clear_worker_checkpoint(&task_id);
            }
            if !update_task_locked(cfg, shared, &task_id, status, true) {
                common::log_event("task_update_failed", &format!("task {}", task_id));
            }
        }

        if !cfg.continuous {
            continue;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUEUE: &str = "\
- id: a-1
  task: first
  status: pending
  attempts: 0
- id: b-2
  task: second
  status: pending
  attempts: 0
";

    #[test]
    fn checkpoint_parses_escaped_fields() {
        let cp = parse_checkpoint(
            "{\"task_id\":\"a-1\",\"branch\":\"task/a-1\",\"phase\":\"TestGate\",\"timestamp_secs\":7}",
        )
        .expect("checkpoint");
        assert_eq!(cp.task_id, "a-1");
        assert_eq!(cp.branch, "task/a-1");
        assert_eq!(cp.phase, "TestGate");
        assert!(parse_checkpoint("{}").is_none());
    }

    #[test]
    fn context_override_shapes_the_model_limit() {
        let json = model_override_json("loop-studio/qwen3.8-27b#fast", 32768).expect("json");
        assert_eq!(
            json,
            "{\"providers\":{\"loop-studio\":{\"models\":{\"qwen3.8-27b\":{\"limit\":{\"context\":32768}}}}}}\n"
        );
        assert!(model_override_json("no-slash", 1).is_none());
        assert!(model_override_json("bad/provider name", 1).is_none());
    }

    #[test]
    fn parallel_flag_parses_and_clamps() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(parse_parallel(&args(&[])), 1);
        assert_eq!(parse_parallel(&args(&["--parallel", "4"])), 4);
        assert_eq!(parse_parallel(&args(&["--parallel=9"])), 9);
        assert_eq!(parse_parallel(&args(&["-p", "0"])), 1);
        assert_eq!(parse_parallel(&args(&["--parallel", "999"])), PARALLEL_MAX);
    }

    #[test]
    fn claimed_candidates_are_not_reselected() {
        let claimed = HashSet::from(["a-1".to_string()]);
        let picked = super::super::orchestrator::judge_next_excluding(
            QUEUE,
            Some("Nominal"),
            "/tmp",
            &claimed,
        )
        .expect("second task");
        assert_eq!(picked.id, "b-2");
    }
}
