//! LAC shared helpers — std-only, zero external crates.
//!
//! Every binary in this suite includes this file with a top-level
//! `mod common;` declaration (same directory, no Cargo.toml change).
//! Centralizes: memory probing, thermals, port/health checks,
//! atomic file writes, inter-process locks, and repo introspection.

#![allow(dead_code)]

pub mod git;
pub mod http;
pub mod lock;
pub mod logs;
pub mod plist;
pub mod ports;
pub mod process;
pub mod ram;
pub mod thermal;
pub mod tokens;

// ---------------------------------------------------------------- re-exports for backward compat ---
// Every public item from every sub-module is re-exported here so that
// `use common::foo` at existing call sites continues to work unchanged.
// Each binary uses a different subset, so the lint is suppressed per-use.
#[allow(unused_imports)]
pub use git::{
    default_branch, default_branch_in, repo_dirty, repo_dirty_in, repo_has_commits,
    repo_has_commits_in,
};
#[allow(unused_imports)]
pub use http::{
    http_get, http_get_auth, http_post, http_post_addr, http_post_addr_auth, http_ready,
    port_up, port_up_ms, wait_for_http, wait_for_port,
};
#[allow(unused_imports)]
pub use lock::{acquire_lock, DirLock};
#[allow(unused_imports)]
pub use logs::{
    append_jsonl, event_log_path, is_project_log_name, lac_logs_footprint, log_event,
    log_file_name, log_file_names, log_names_for, log_which_names, logs_dir,
    oldest_task_logs, oldest_task_logs_in, purge_rotated_generations, rotate_log_if_large,
    spawn_log_rotator, task_logs_dir, task_logs_dir_in, task_logs_footprint,
    task_logs_footprint_in, trim_task_logs, trim_task_logs_in,
    LOG_KEEP, LOG_MAX_BYTES, TASK_LOG_KEEP, TASK_LOG_MAX_BYTES,
};
#[allow(unused_imports)]
pub use plist::{retarget_launchd_plist, set_plist_string};
#[allow(unused_imports)]
pub use ports::{
    gateway_port, llama_port, mlx_port, mlx_port_from, ollama_port, port_env_notes, port_from,
};
#[allow(unused_imports)]
pub use process::{
    children_of, choose_program_binary, command_of, is_executable, kill_matching, kill_tree,
    parse_ps_line, process_snapshot, processes_matching, processes_matching_in, program_matches,
    signal_pid, which, ProcInfo, ProgramChoice, ProgramKind,
};
#[allow(unused_imports)]
pub use ram::{free_ram_gib, mem_thresholds_gib, page_size, serve_ram_tiers_gib, total_ram_gib};
#[allow(unused_imports)]
pub use thermal::{cpu_speed_limit, parse_thermal, thermal_detail, thermal_label, thermal_state};
#[allow(unused_imports)]
pub use tokens::{
    json_u64, opencode_tokens, router_est_tokens, session_tokens, sum_est_tokens,
    token_source_is_measured, usage_log_paths,
};

// ---------------------------------------------------------------- std imports for mod-level code ---
use std::env;
use std::fs;
use std::io;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------- paths ---

/// The pure rule behind [`home_dir`]: which `$HOME` values name a real home.
pub fn home_dir_from(value: Option<&str>) -> Result<String, &'static str> {
    match value {
        Some(h) if !h.trim().is_empty() && Path::new(h).is_absolute() => Ok(h.to_string()),
        _ => Err("no absolute $HOME"),
    }
}

/// Home directory for every state path in this suite.
pub fn home_dir() -> String {
    match home_dir_from(env::var("HOME").ok().as_deref()) {
        Ok(home) => home,
        Err(why) => {
            eprintln!("Loop Studio cannot locate a home directory ({why}), so it has nowhere to read or write state.");
            eprintln!("Run this from a login shell, or set HOME to an absolute path first.");
            std::process::exit(78);
        }
    }
}

/// The Loop Studio state directory (~/.loop-studio).
pub fn state_dir() -> String {
    let dir = format!("{}/.loop-studio", home_dir());
    if !Path::new(&dir).exists() {
        let old = format!("{}/.loop-studio", home_dir());
        if Path::new(&old).exists() {
            eprintln!("Notice: ~/.loop-studio exists but ~/.loop-studio does not. Run: mv ~/.loop-studio ~/.loop-studio");
        }
    }
    dir
}

/// Signal handler type.
#[cfg(unix)]
type Sighandler = unsafe extern "C" fn(std::ffi::c_int);

#[cfg(unix)]
unsafe extern "C" {
    fn signal(sig: std::ffi::c_int, handler: Sighandler) -> Sighandler;
    fn kill(pid: std::ffi::c_int, sig: std::ffi::c_int) -> std::ffi::c_int;
}

#[cfg(unix)]
pub fn ignore_sigpipe() {
    const SIGPIPE: std::ffi::c_int = 13;
    unsafe extern "C" fn ignore(_: std::ffi::c_int) {}
    unsafe {
        signal(SIGPIPE, ignore);
    }
}

#[cfg(not(unix))]
pub fn ignore_sigpipe() {}

// -------------------------------------------------------- child lifetime --

/// The engine child's pid, published so a signal handler can reach it.
pub static CHILD_PID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// Set by the signal handler so a supervising loop can stop retrying.
pub static TERMINATING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether a termination signal has arrived.
pub fn terminating() -> bool {
    TERMINATING.load(std::sync::atomic::Ordering::SeqCst)
}

const SIGINT: std::ffi::c_int = 2;
const SIGTERM: std::ffi::c_int = 15;

#[cfg(unix)]
extern "C" fn forward_terminate_to_child(sig: std::ffi::c_int) {
    let pid = CHILD_PID.load(std::sync::atomic::Ordering::SeqCst);
    if pid > 0 {
        unsafe { kill(pid, SIGTERM) };
    }
    TERMINATING.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = sig;
}

/// Route SIGTERM/SIGINT to the engine child.
pub fn forward_terminate_to_child_process() {
    #[cfg(unix)]
    unsafe {
        signal(SIGTERM, forward_terminate_to_child);
        signal(SIGINT, forward_terminate_to_child);
    }
}

/// Record the child so the signal handler can signal it.
pub fn set_child_pid(pid: i32) {
    CHILD_PID.store(pid, std::sync::atomic::Ordering::SeqCst);
}

pub fn clear_child_pid() {
    CHILD_PID.store(0, std::sync::atomic::Ordering::SeqCst);
}

/// Sleep, but wake early if a termination signal arrives.
pub fn sleep_unless_terminating(total: Duration) -> bool {
    let deadline = Instant::now() + total;
    while Instant::now() < deadline {
        if terminating() {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50).min(total));
    }
    !terminating()
}

/// Project root: current dir when it holds opencode.jsonc, else derived
/// from the executable path, else current dir.
pub fn project_root() -> String {
    let cwd = env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| ".".to_string());
    if fs::metadata(format!("{}/opencode.jsonc", cwd)).is_ok() {
        return cwd;
    }
    if let Ok(exe) = env::current_exe() {
        if let Some(root) = exe
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
        {
            let r = root.to_string_lossy().to_string();
            if fs::metadata(format!("{}/opencode.jsonc", r)).is_ok() {
                return r;
            }
        }
    }
    cwd
}

/// Absolute path to a sibling release binary.
pub fn bin(root: &str, name: &str) -> String {
    format!("{}/rust-src/target/release/{}", root, name)
}

/// Canonical Kanban queue path.
pub fn tasks_file() -> String {
    format!("{}/todo/loop-studio-tasks.yaml", home_dir())
}

pub fn loops_dir() -> String {
    format!("{}/todo/loop-studio-loops", home_dir())
}

/// Persist-or-warn: queue/state writes must never silently drop.
pub fn persist_or_warn(path: &str, contents: &str, ctx: &str) -> bool {
    match atomic_write(path, contents) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("warning: {} write to {} failed: {}", ctx, path, e);
            append_jsonl(
                &format!("{}/event.log", state_dir()),
                &format!("{{\"ts\":{},\"level\":\"warn\",\"ctx\":\"{}\",\"err\":\"{}\"}}", now_unix(), ctx, e),
            );
            false
        }
    }
}

// ------------------------------------------------------------------ volumes -

/// True only when `mount` shows the path as a real mount point.
pub fn volume_mounted(mount_point: &str) -> bool {
    if let Ok(out) = Command::new("mount").output() {
        let txt = String::from_utf8_lossy(&out.stdout);
        let needle = format!(" on {} (", mount_point);
        if txt.lines().any(|l| l.contains(&needle)) {
            return true;
        }
    }
    false
}

/// Canonical model library: TB5 external when mounted, else internal fallback.
pub fn model_base() -> String {
    if volume_mounted("/Volumes/AIModels") {
        "/Volumes/AIModels".to_string()
    } else {
        format!("{}/models", state_dir())
    }
}

// -------------------------------------------------------------------- files -

/// Write-temp-then-rename. A crash mid-write never leaves a half file.
pub fn atomic_write(path: &str, contents: &str) -> io::Result<()> {
    let p = Path::new(path);
    if let Some(parent) = p.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let tmp = format!("{}.tmp.{}", path, std::process::id());
    fs::write(&tmp, contents)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Is this bind address the loopback interface?
pub fn is_loopback_host(h: &str) -> bool {
    let t = h.trim().to_lowercase();
    t == "127.0.0.1"
        || t == "localhost"
        || t == "::1"
        || t == "[::1]"
        || t == "::ffff:127.0.0.1"
        || t == "0:0:0:0:0:0:0:1"
}

// ---------------------------------------------------------------- time ----

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Minimal JSON string escaper for hand-built payloads (std-only).
pub fn json_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o
}

// ---------------------------------------------------------------- daemon supervision --

/// Whether a supervising engine wrapper should launch again after an exit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DaemonDecision {
    Retry,
    Stop,
}

/// Consecutive launches that failed without the engine ever answering HTTP.
pub const GIVE_UP_AFTER: u32 = 5;

/// The retry decision, as a pure function of what has been observed.
pub fn daemon_decision(ever_ready: bool, crashes: u32) -> DaemonDecision {
    if ever_ready || crashes < GIVE_UP_AFTER {
        DaemonDecision::Retry
    } else {
        DaemonDecision::Stop
    }
}

/// Explanation for giving up, in one voice so both lanes say the same thing.
pub fn daemon_give_up_reason(engine: &str, model: &str, port: u16, crashes: u32, last: &str) -> String {
    format!(
        "{engine} failed {crashes} times in a row and never answered HTTP on 127.0.0.1:{port} \
         ({last}). That is a permanent fault, not a transient one — check the model id \
         `{model}` and that nothing else holds the port. Not retrying; re-run once it is fixed."
    )
}

// ---------------------------------------------------------------- arch helpers --

/// OS/arch without spawning `uname`.
pub fn os_descr() -> String {
    format!("{} {}", std::env::consts::OS, std::env::consts::ARCH)
}

/// True on Apple Silicon Macs (arm64).
pub fn is_apple_silicon() -> bool {
    std::env::consts::ARCH == "aarch64" && std::env::consts::OS == "macos"
}

/// Human label for headers.
pub fn arch_label() -> &'static str {
    if is_apple_silicon() {
        "Apple Silicon"
    } else if std::env::consts::ARCH == "x86_64" {
        "Intel"
    } else {
        std::env::consts::ARCH
    }
}

/// MLX lane availability. Currently Apple Silicon only.
pub fn mlx_supported() -> bool {
    is_apple_silicon()
}

// ---------------------------------------------------------------- file tailing --

/// Last `n` lines of a file (trailing newline normalized).
pub fn tail_file(path: &str, n: usize) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let want = n.max(1);
    let mut file = fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let mut window = 64 * 1024u64;
    loop {
        let start = len.saturating_sub(window);
        let mut buf = Vec::with_capacity((len - start) as usize);
        file.seek(SeekFrom::Start(start)).ok()?;
        file.read_to_end(&mut buf).ok()?;
        let text = String::from_utf8_lossy(&buf);
        let lines: Vec<&str> = text.lines().collect();
        let skipped = if start > 0 && !lines.is_empty() { 1 } else { 0 };
        let available = lines.len().saturating_sub(skipped);
        if available >= want || start == 0 {
            let begin = lines.len().saturating_sub(want);
            let mut out = lines[begin..].join("\n");
            out.push('\n');
            return Some(out);
        }
        window = window.saturating_mul(2);
    }
}

/// First `n` lines of a string.
pub fn head_lines(s: &str, n: usize) -> String {
    s.lines().take(n.max(1)).collect::<Vec<_>>().join("\n")
}

// ---------------------------------------------------------------- jsonc helpers --

/// The `model` string value out of an `opencode.jsonc`, comments and all.
pub fn jsonc_model_id(source: &str) -> Option<String> {
    let text = strip_jsonc_comments(source);
    let at = text.find("\"model\"")?;
    let rest = &text[at + "\"model\"".len()..];
    let colon = rest.find(':')?;
    let after = rest[colon + 1..].trim_start();
    let mut chars = after.chars();
    if chars.next()? != '"' {
        return None;
    }
    let mut out = String::new();
    let mut escaped = false;
    for c in chars {
        if escaped {
            out.push(c);
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == '"' {
            return Some(out);
        } else {
            out.push(c);
        }
    }
    None
}

/// Drop `//` and `/* */` comments that are not inside a string literal.
pub fn strip_jsonc_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    let mut in_str = false;
    while i < b.len() {
        if in_str {
            out.push(b[i] as char);
            if b[i] == b'\\' && i + 1 < b.len() {
                out.push(b[i + 1] as char);
                i += 2;
                continue;
            }
            if b[i] == b'"' {
                in_str = false;
            }
            i += 1;
        } else if b[i] == b'"' {
            in_str = true;
            out.push('"');
            i += 1;
        } else if i + 1 < b.len() && b[i] == b'/' && b[i + 1] == b'/' {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if i + 1 < b.len() && b[i] == b'/' && b[i + 1] == b'*' {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(b.len());
        } else {
            out.push(b[i] as char);
            i += 1;
        }
    }
    out
}

// --------------------------------------------------------------------- tests -

/// Serializes tests that mutate the `LOOP_STUDIO_*` / `MLX_PORT` env vars.
///
/// `ports.rs` is compiled twice in the router binary — as `common::ports` and
/// as the `#[path]`-included `shared_ports` — and the router has its own port
/// test on top. Two writers racing the same process-global env vars is a flake,
/// not a test, so each of them takes this lock and runs one at a time. It is
/// test-only: production never holds it, and uncontended in binaries that
/// compile `ports.rs` only once.
#[cfg(test)]
pub(crate) static PORT_ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    #![deny(dead_code)]
    use super::*;

    #[test]
    fn json_escape_roundtrip_safe() {
        assert_eq!(json_escape("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
        assert_eq!(json_escape("tab\there"), "tab\\there");
        assert_eq!(json_escape("plain"), "plain");
        assert_eq!(json_escape("\u{1}"), "\\u0001");
    }

    #[test]
    fn json_escape_neutralizes_known_bad_chars() {
        // Every JSON-breaking character must be escaped so a value can never
        // close its string field early or break the enclosing object.
        assert_eq!(json_escape("\t"), "\\t", "tab");
        assert_eq!(json_escape("\n"), "\\n", "newline");
        assert_eq!(json_escape("\r"), "\\r", "carriage return");
        assert_eq!(json_escape("\\"), "\\\\", "backslash");
        assert_eq!(json_escape("\""), "\\\"", "double-quote");
        // Control characters below 0x20 become \uXXXX escapes.
        assert_eq!(json_escape("\u{0}"), "\\u0000", "NUL");
        assert_eq!(json_escape("\u{7}"), "\\u0007", "BEL");
        assert_eq!(json_escape("\u{1f}"), "\\u001f", "unit separator");
        // A value carrying every bad char at once round-trips to escaped JSON.
        assert_eq!(
            json_escape("a\tb\nc\rd\\e\"f\u{0}g\u{1f}"),
            "a\\tb\\nc\\rd\\\\e\\\"f\\u0000g\\u001f"
        );
    }

    #[test]
    fn volume_mounted_rejects_fantasy_paths() {
        assert!(!volume_mounted("/Volumes/Definitely-Not-A-Volume-xyz"));
    }

    #[test]
    fn atomic_write_roundtrip() {
        let p = format!(
            "{}/loop-studio-test-atomic-{}.txt",
            std::env::temp_dir().to_string_lossy(),
            std::process::id()
        );
        atomic_write(&p, "hello").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "hello");
        atomic_write(&p, "world").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "world");
        let _ = fs::remove_file(&p);
        assert!(!Path::new(&format!("{}.tmp.{}", p, std::process::id())).exists());
    }

    #[test]
    fn arch_helpers_consistent() {
        assert!(!arch_label().is_empty());
        assert!(!super::arch_label().is_empty());
        if mlx_supported() {
            assert!(is_apple_silicon());
            assert_eq!(arch_label(), "Apple Silicon");
        }
        assert_eq!(is_apple_silicon(), std::env::consts::ARCH == "aarch64");
    }

    #[test]
    fn retry_is_bounded_only_while_the_engine_has_never_worked() {
        use DaemonDecision::*;
        for crashes in 0..GIVE_UP_AFTER {
            assert_eq!(daemon_decision(false, crashes), Retry, "crashes={crashes}");
        }
        assert_eq!(daemon_decision(false, GIVE_UP_AFTER), Stop);
        assert_eq!(daemon_decision(false, GIVE_UP_AFTER + 50), Stop);
        assert_eq!(daemon_decision(false, u32::MAX), Stop);
        for crashes in [0, 1, GIVE_UP_AFTER, 1000, u32::MAX] {
            assert_eq!(daemon_decision(true, crashes), Retry, "crashes={crashes}");
        }
    }

    #[test]
    fn give_up_names_the_model_and_the_port() {
        let reason = daemon_give_up_reason("llama-server", "org/typo:Q4_K_M", 8081, 5, "exit 1");
        assert!(reason.contains("llama-server"), "{reason}");
        assert!(reason.contains("org/typo:Q4_K_M"), "{reason}");
        assert!(reason.contains("8081"), "{reason}");
        assert!(reason.contains("5 times"), "{reason}");
        assert!(reason.contains("permanent"), "{reason}");
    }

    #[test]
    fn tail_file_roundtrip() {
        let p = format!(
            "{}/loop-studio-test-tail-{}.log",
            std::env::temp_dir().to_string_lossy(),
            std::process::id()
        );
        fs::write(&p, "a\nb\nc\nd\n").unwrap();
        assert_eq!(tail_file(&p, 2).as_deref(), Some("c\nd\n"));
        assert_eq!(tail_file(&p, 99).as_deref(), Some("a\nb\nc\nd\n"));
        assert_eq!(tail_file(&p, 0).as_deref(), Some("d\n"));
        assert!(tail_file("/nonexistent/loop-studio-tail-xyz", 10).is_none());
        let _ = fs::remove_file(&p);
        assert_eq!(head_lines("a\nb\nc", 2), "a\nb");
    }

    #[test]
    fn tail_of_a_huge_file_reads_a_window_not_the_file() {
        let p = format!(
            "{}/loop-studio-test-tail-big-{}.log",
            std::env::temp_dir().to_string_lossy(),
            std::process::id()
        );
        let filler = "x".repeat(1023);
        let mut file = fs::File::create(&p).unwrap();
        for _ in 0..(64 * 1024) {
            use std::io::Write as _;
            writeln!(file, "{}", filler).unwrap();
        }
        use std::io::Write as _;
        writeln!(file, "the-answer-line").unwrap();
        writeln!(file, "the-last-line").unwrap();
        drop(file);
        let size = fs::metadata(&p).unwrap().len();
        assert!(size > 64 * 1024 * 1024, "fixture is {} bytes", size);
        let out = tail_file(&p, 2).expect("tail");
        assert_eq!(out, "the-answer-line\nthe-last-line\n");
        let _ = fs::remove_file(&p);
    }
}

#[cfg(test)]
mod child_tests {
    #![deny(dead_code)]

    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn child_pid_is_published_and_withdrawn() {
        set_child_pid(4242);
        assert_eq!(CHILD_PID.load(Ordering::SeqCst), 4242);
        clear_child_pid();
        assert_eq!(CHILD_PID.load(Ordering::SeqCst), 0, "a stale pid must never outlive its child");
        forward_terminate_to_child_process();
        forward_terminate_to_child_process();
    }

    #[test]
    fn sleep_returns_early_when_terminating() {
        TERMINATING.store(false, Ordering::SeqCst);
        let start = Instant::now();
        assert!(sleep_unless_terminating(Duration::from_millis(200)));
        assert!(start.elapsed() < Duration::from_secs(2), "overran the sleep");

        TERMINATING.store(true, Ordering::SeqCst);
        let start = Instant::now();
        assert!(
            !sleep_unless_terminating(Duration::from_secs(30)),
            "must report that it was cut short"
        );
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "a 30s backoff must not survive a termination signal: {:?}",
            start.elapsed()
        );
        TERMINATING.store(false, Ordering::SeqCst);
    }

    #[test]
    fn home_must_be_absolute_and_present() {
        assert!(home_dir_from(None).is_err());
        assert!(home_dir_from(Some("")).is_err());
        assert!(home_dir_from(Some("   ")).is_err());
        assert!(home_dir_from(Some(".")).is_err());
        assert!(home_dir_from(Some("..")).is_err());
        assert!(home_dir_from(Some("home/u")).is_err());
        assert!(home_dir_from(Some("home/u/")).is_err());
        assert!(home_dir_from(Some("~/loop-studio")).is_err());
        assert_eq!(
            home_dir_from(Some("/home/u")).as_deref(),
            Ok("/home/u"),
            "an absolute path is the only thing that names a home"
        );
        assert_eq!(home_dir_from(Some("/Volumes/AIModels")).as_deref(), Ok("/Volumes/AIModels"));
    }
}
