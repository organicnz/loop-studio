//! LAC shared helpers — std-only, zero external crates.
//!
//! Every binary in this suite includes this file with a top-level
//! `mod common;` declaration (same directory, no Cargo.toml change).
//! Centralizes: memory probing, thermals, port/health checks,
//! atomic file writes, inter-process locks, and repo introspection.

#![allow(dead_code)]

use std::env;
use std::fs;
use std::io;
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------- paths ---

/// The pure rule behind [`home_dir`]: which `$HOME` values name a real home.
///
/// Split out so the rule is assertable without exiting the test process.
pub fn home_dir_from(value: Option<&str>) -> Result<String, &'static str> {
    match value {
        Some(h) if !h.trim().is_empty() && Path::new(h).is_absolute() => Ok(h.to_string()),
        _ => Err("no absolute $HOME"),
    }
}

/// Home directory for every state path in this suite.
///
/// The fallback used to be `"."`, and a relative path is not a home
/// directory — it is "wherever this process happened to be launched".
/// With `HOME` unset, `kv-manage truncate` created a `.lac/` tree in the
/// working directory (launchd pins that to the checkout, which
/// `.gitignore` does not cover), and every later command read *that*
/// tree, because the same relative string resolves the same way from any
/// cwd. `bootstrap` already refused to run in that state while quietly
/// relying on this function everywhere else.
///
/// There is no correct path to offer when home is unknown, so this says so
/// and exits nonzero (EX_CONFIG) rather than writing into the cwd.
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

/// Instructs the OS kernel to ignore SIGPIPE (signal 13).
/// In network servers and CLI tools, writing to a socket closed by the remote
/// peer returns EPIPE instead of abruptly aborting the process with a signal.
/// Signal handler type. Typed as a function pointer rather than `usize`
/// because that is what the C ABI actually is, and a mismatched declaration
/// is how a handler ends up jumping to an address it should not.
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
    // SIG_IGN = 1 on Darwin/Linux.
    unsafe extern "C" fn ignore(_: std::ffi::c_int) {}
    unsafe {
        signal(SIGPIPE, ignore);
    }
}

#[cfg(not(unix))]
pub fn ignore_sigpipe() {}

// -------------------------------------------------------- child lifetime --

/// The engine child's pid, published so a signal handler can reach it.
///
/// `spawn(...).status()` hands the child to `wait()` and gives back only an
/// `ExitStatus`, so the parent has no handle left to signal. That is what let
/// `kill -TERM <serve-mlx>` orphan `mlx_lm.server`: the wrapper died, the
/// engine did not, it kept the port bound and ~16 GiB of weights resident,
/// and the next start found the port taken, moved to 8082 and loaded a
/// *second* copy of a 27B model. On the host this suite warns about at 32 GiB,
/// that is the difference between serving and swapping — and nothing in the
/// output said a duplicate had been created.
static CHILD_PID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// Set by the signal handler so a supervising loop can stop retrying.
static TERMINATING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether a termination signal has arrived. Loops check it and unwind
/// normally, so destructors still run.
pub fn terminating() -> bool {
    TERMINATING.load(std::sync::atomic::Ordering::SeqCst)
}

const SIGINT: std::ffi::c_int = 2;
const SIGTERM: std::ffi::c_int = 15;

#[cfg(unix)]
extern "C" fn forward_terminate_to_child(sig: std::ffi::c_int) {
    // Async-signal-safe only, and deliberately does not exit: `kill` is safe,
    // `_exit` would skip every Drop and leave the port file advertising a lane
    // that is now gone. Instead the child dies, the blocked `wait()` returns,
    // and the loop unwinds through normal code where the guard runs.
    let pid = CHILD_PID.load(std::sync::atomic::Ordering::SeqCst);
    if pid > 0 {
        unsafe { kill(pid, SIGTERM) };
    }
    TERMINATING.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = sig;
}

/// Route SIGTERM/SIGINT to the engine child, so stopping the wrapper stops
/// the engine. Idempotent, and safe to call when there is no child yet.
pub fn forward_terminate_to_child_process() {
    #[cfg(unix)]
    unsafe {
        signal(SIGTERM, forward_terminate_to_child);
        signal(SIGINT, forward_terminate_to_child);
    }
}

/// Record the child so the signal handler can signal it, and clear it once
/// the child is reaped so a later signal cannot hit a recycled pid.
pub fn set_child_pid(pid: i32) {
    CHILD_PID.store(pid, std::sync::atomic::Ordering::SeqCst);
}

pub fn clear_child_pid() {
    CHILD_PID.store(0, std::sync::atomic::Ordering::SeqCst);
}

/// Sleep, but wake early if a termination signal arrives. The retry loops
/// sleep for up to 60s between launches; without this a `loop-studio stop` during
/// backoff would be ignored for the rest of the wait, and the engine would be
/// relaunched after the operator asked for the lane to be down.
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
/// from the executable path (target/release/<bin> -> up 3 = rust-src's
/// parent), else current dir.
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

/// Canonical Kanban queue + loops paths (single copy — loop_studio.rs had 4).
pub fn tasks_file() -> String {
    format!("{}/todo/loop-studio-tasks.yaml", home_dir())
}

pub fn loops_dir() -> String {
    format!("{}/todo/loop-studio-loops", home_dir())
}

/// Persist-or-warn: queue/state writes must never silently drop.
/// Returns true on success; logs to stderr + event log on failure.
pub fn persist_or_warn(path: &str, contents: &str, ctx: &str) -> bool {
    match atomic_write(path, contents) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("warning: {} write to {} failed: {}", ctx, path, e);
            append_jsonl(
                &format!("{}/.lac/event.log", home_dir()),
                &format!("{{\"ts\":{},\"level\":\"warn\",\"ctx\":\"{}\",\"err\":\"{}\"}}", now_unix(), ctx, e),
            );
            false
        }
    }
}

/// Retarget a launchd plist template at THIS machine: baked-in prior-home
/// paths become `$HOME`, `ProgramArguments[0]` becomes `want_bin`, the
/// working directory becomes this checkout, and log paths re-root into
/// this user's `~/Library/Logs` (filenames preserved). Keyed on plist
/// structure, not on one exact string, so templates stay portable
/// across users and checkouts.
pub fn retarget_launchd_plist(content: &str, home: &str, root: &str, want_bin: &str) -> String {
    let mut out = content.replace("/Users/organic", home);
    // Replace the first <string> entry (ProgramArguments[0]).
    if let Some(arr) = out.find("<array>") {
        if let Some(s0) = out[arr..].find("<string>") {
            let abs = arr + s0 + "<string>".len();
            if let Some(e0) = out[abs..].find("</string>") {
                out.replace_range(abs..abs + e0, want_bin);
            }
        }
    }
    // Pin WorkingDirectory to this checkout.
    if let Some(k) = out.find("<key>WorkingDirectory</key>") {
        if let Some(s0) = out[k..].find("<string>") {
            let abs = k + s0 + "<string>".len();
            if let Some(e0) = out[abs..].find("</string>") {
                out.replace_range(abs..abs + e0, root);
            }
        }
    }
    // Re-root log files into this user's Logs dir,
    // preserving each plist's filename.
    for key in ["<key>StandardOutPath</key>", "<key>StandardErrorPath</key>"] {
        if let Some(k) = out.find(key) {
            if let Some(s0) = out[k..].find("<string>") {
                let abs = k + s0 + "<string>".len();
                if let Some(e0) = out[abs..].find("</string>") {
                    let cur = out[abs..abs + e0].to_string();
                    let base = cur.rsplit('/').next().unwrap_or("loop-studio.log");
                    out.replace_range(abs..abs + e0, &format!("{}/Library/Logs/{}", home, base));
                }
            }
        }
    }
    out
}

/// Set the `<string>` value that follows `<key>KEY</key>` in a plist, whatever
/// the indentation. Returns `None` when the key is not there, so a caller can
/// tell "retargeted" from "silently did nothing".
///
/// This exists because the router port was being rewritten with a literal
/// `"<key>LOOP_STUDIO_ROUTER_PORT</key>\n    <string>8000</string>"`, and the shipped
/// template indents its `<string>` by eight spaces. The replace never matched,
/// `LOOP_STUDIO_ROUTER_PORT=9000 loop-studio daemon install` printed `[ok] Placed:`, and the
/// launchd router bound 8000 while every `loop-studio` command addressed 9000 — so the
/// whole CLI reported the gateway down. A plist is XML with no significant
/// whitespace; matching on it is how that stayed invisible.
pub fn set_plist_string(content: &str, key: &str, value: &str) -> Option<String> {
    let needle = format!("<key>{key}</key>");
    let k = content.find(&needle)?;
    let after = k + needle.len();
    // The value must belong to *this* key, so the search stops at the next
    // one. A key whose value is not a string (`<true/>`, a nested `<dict>`) is
    // reported rather than answered with whatever `<string>` comes later in the
    // file.
    let end = content[after..]
        .find("<key>")
        .map(|i| after + i)
        .unwrap_or(content.len());
    let region = &content[after..end];
    let s0 = region.find("<string>")? + after;
    let open = s0 + "<string>".len();
    let close = content[open..end].find("</string>")? + open;
    let mut out = content.to_string();
    out.replace_range(open..close, value);
    Some(out)
}

// ---------------------------------------------------------------- memory --

/// OS page size via `sysctl hw.pagesize`; Apple Silicon fallback 16384.
/// Never hardcode: Rosetta/Intel pages are 4096 and silently skew RAM math.
pub fn page_size() -> u64 {
    if let Ok(out) = Command::new("sysctl")
        .args(["-n", "hw.pagesize"])
        .output()
    {
        if let Ok(n) = String::from_utf8_lossy(&out.stdout).trim().parse::<u64>() {
            if n > 0 {
                return n;
            }
        }
    }
    16384
}

/// Free unified memory in GiB.
///
/// Counts free + inactive + speculative pages. Speculative pages are
/// reclaimable on memory pressure; ignoring them (as before) understated
/// free RAM by several GiB and caused false OOM alarms.
pub fn free_ram_gib() -> Option<f64> {
    let out = Command::new("vm_stat").output().ok()?;
    let txt = String::from_utf8_lossy(&out.stdout);
    let ps = page_size() as f64;
    let mut pages: f64 = 0.0;
    for line in txt.lines() {
        let l = line.trim();
        if l.starts_with("Pages free:")
            || l.starts_with("Pages inactive:")
            || l.starts_with("Pages speculative:")
        {
            let n: String = l.chars().filter(|c| c.is_ascii_digit()).collect();
            pages += n.parse::<f64>().unwrap_or(0.0);
        }
    }
    if pages <= 0.0 {
        return None;
    }
    Some(pages * ps / 1073741824.0)
}

/// Total physical RAM in GiB via `sysctl hw.memsize`.
pub fn total_ram_gib() -> Option<f64> {
    let out = Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()?;
    let n: f64 = String::from_utf8(out.stdout).ok()?.trim().parse().ok()?;
    if n <= 0.0 {
        return None;
    }
    Some(n / 1073741824.0)
}

// ---------------------------------------------------------------- thermals -

/// The pmset invocation that reports thermal conditions affecting CPU speed.
///
/// `pmset -g thermals` does not exist. pmset answers it with
/// `Error: unhandled argument thermals` and exits 1 — on every host, always.
/// The probe used to run exactly that, discarded the exit status, and handed
/// every caller an empty string, which `thermal_state` then reported as
/// "Nominal". So the reading was invented everywhere, including on the
/// throttling machines the quality/context decisions exist to protect, and
/// the detail pane an operator checks to confirm the reading printed nothing.
const THERMAL_PROBE: [&str; 2] = ["-g", "therm"];

/// Raw `pmset -g therm` output. Empty when pmset could not be run or failed —
/// which is a host that cannot answer, not a machine reading Nominal.
pub fn thermal_detail() -> String {
    Command::new("pmset")
        .args(THERMAL_PROBE)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default()
}

/// One of Nominal | Fair | Serious | Critical — or `None` when the probe could
/// not answer.
///
/// `None` is a real answer and every caller has to handle it. `pmset` is not
/// present on every platform, it exits nonzero where it is unusable, and a
/// broken probe says nothing whatsoever about how hot the machine is. It used
/// to be flattened to "Nominal", and two decision paths read that as
/// headroom: `decide_model_and_context` (`lac_tui.rs`) picks Q4-vs-Q8 and the
/// context window from it, and `thermal_default_context` (`serve_llama.rs`)
/// sets the server's window. Later code collapsed "Serious" into "Nominal"
/// too, hiding the throttle warning that precedes Critical by minutes; an
/// absent reading is the same failure one level up.
pub fn thermal_state() -> Option<String> {
    let out = Command::new("pmset").args(THERMAL_PROBE).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    parse_thermal(&text)
}

/// A reading as a word, for lines that print it. An absent probe is named
/// here rather than defaulted at each call site, so no reason line, banner or
/// prompt can quietly claim a level nobody measured.
pub fn thermal_label(state: &Option<String>) -> &str {
    state.as_deref().unwrap_or("unknown")
}

/// Classify a *successful* `pmset -g therm` capture.
///
/// Split out so the mapping is testable without pmset, and so the few lines
/// that decide what a throttling machine is told can be read in one place.
///
/// The speed limit is the real signal: pmset reports `CPU_Speed_Limit = <n>`
/// only while a thermal condition is limiting the CPU, and omits the line
/// entirely when none is in effect. The bucket edges below are this project's
/// policy, not Apple's — what matters is that a reported limit is read as a
/// limit instead of being discarded, which is exactly what the invalid flag
/// did.
fn parse_thermal(stdout: &str) -> Option<String> {
    let out = stdout.trim();
    // pmset succeeded and said nothing at all. Silence is not Nominal.
    if out.is_empty() {
        return None;
    }
    // A named level, where the platform reports one.
    for (needle, level) in [
        ("Critical", "Critical"),
        ("Overheat", "Critical"),
        ("Serious", "Serious"),
        ("Fair", "Fair"),
    ] {
        if out.contains(needle) {
            return Some(level.to_string());
        }
    }
    if let Some(limit) = cpu_speed_limit(out) {
        return Some(
            match limit as u32 {
                0..=74 => "Critical",
                75..=89 => "Serious",
                90..=99 => "Fair",
                _ => "Nominal",
            }
            .to_string(),
        );
    }
    // pmset ran, succeeded, and reported no limit in effect. That is a
    // reading, not an absence: "No thermal warning level has been recorded" is
    // what a machine that is not currently throttling says.
    Some("Nominal".to_string())
}

/// The `CPU_Speed_Limit = n` value from a pmset thermal capture, if present.
fn cpu_speed_limit(out: &str) -> Option<f64> {
    let p = out.find("CPU_Speed_Limit")?;
    let tail = &out[p + "CPU_Speed_Limit".len()..];
    let eq = tail.find('=')?;
    tail[eq + 1..]
        .split_whitespace()
        .next()?
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.0)
}

// ----------------------------------------------------------------- network -

/// TCP connect probe. Default budget 150ms keeps router hot-path fast.
pub fn port_up(port: u16) -> bool {
    port_up_ms(port, 150)
}

pub fn port_up_ms(port: u16, ms: u64) -> bool {
    let addr = format!("127.0.0.1:{}", port);
    match addr.parse() {
        Ok(sa) => TcpStream::connect_timeout(&sa, Duration::from_millis(ms)).is_ok(),
        Err(_) => false,
    }
}

/// Block until a TCP port accepts (or timeout). For supervising daemons.
pub fn wait_for_port(port: u16, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if port_up_ms(port, 200) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    false
}

/// HTTP readiness: `GET /v1/models` must return 200. A bound TCP port only
/// proves the socket exists; this proves the model is loaded and serving.
/// Used by the router health cache and server readiness threads.
pub fn http_ready(port: u16, timeout_ms: u64) -> bool {
    http_get(port, "/v1/models", timeout_ms)
        .map(|(code, _)| code == 200)
        .unwrap_or(false)
}

/// Minimal blocking HTTP/1.0-style GET over 127.0.0.1. Returns
/// (status_code, body). Cap 256 KiB — enough for model lists, bounded
/// against pathological responses.
pub fn http_get(port: u16, path: &str, timeout_ms: u64) -> Option<(u16, String)> {
    http_get_auth(port, path, timeout_ms, port == gateway_port())
}

fn http_get_auth(port: u16, path: &str, timeout_ms: u64, authorized: bool) -> Option<(u16, String)> {
    use std::io::{Read, Write};
    let addr = format!("127.0.0.1:{}", port);
    let sa: std::net::SocketAddr = addr.parse().ok()?;
    let mut s =
        TcpStream::connect_timeout(&sa, Duration::from_millis(timeout_ms.min(2000))).ok()?;
    let _ = s.set_read_timeout(Some(Duration::from_millis(timeout_ms.min(5000))));
    let _ = s.set_write_timeout(Some(Duration::from_millis(2000)));
    let auth = if authorized {
        env::var("LOOP_STUDIO_API_TOKEN")
            .ok()
            .map(|token| token.trim().to_string())
            .filter(|token| !token.is_empty() && !token.chars().any(|c| c.is_control()))
            .map(|token| format!("Authorization: Bearer {}\r\n", token))
            .unwrap_or_default()
    } else {
        String::new()
    };
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\n{}Connection: close\r\n\r\n",
        path, addr, auth
    );
    s.write_all(req.as_bytes()).ok()?;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match s.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > 262_144 {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&buf).to_string();
    let head_end = text.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0);
    let status = text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .unwrap_or(0);
    Some((status, text[head_end.min(text.len())..].to_string()))
}

/// Blocking HTTP POST over 127.0.0.1. Returns (status_code, body).
/// std-only, bounded read buffer up to 4MB.
pub fn http_post(port: u16, path: &str, body: &str, timeout_ms: u64) -> Option<(u16, String)> {
    http_post_addr_auth(
        &format!("127.0.0.1:{}", port),
        path,
        body,
        Duration::from_millis(timeout_ms),
        true,
    )
}

/// Blocking HTTP POST to a specific socket address with total deadline budget.
pub fn http_post_addr(addr: &str, path: &str, body: &str, budget: Duration) -> Option<(u16, String)> {
    http_post_addr_auth(addr, path, body, budget, false)
}

fn http_post_addr_auth(
    addr: &str,
    path: &str,
    body: &str,
    budget: Duration,
    authorized: bool,
) -> Option<(u16, String)> {
    use std::io::{Read, Write};
    if budget.is_zero() {
        return None;
    }
    let start = Instant::now();
    let sa: std::net::SocketAddr = addr.parse().ok()?;
    let auth = if authorized {
        env::var("LOOP_STUDIO_API_TOKEN")
            .ok()
            .map(|token| token.trim().to_string())
            .filter(|token| !token.is_empty() && !token.chars().any(|c| c.is_control()))
            .map(|token| format!("Authorization: Bearer {}\r\n", token))
            .unwrap_or_default()
    } else {
        String::new()
    };
    let req = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        path,
        addr,
        auth,
        body.len(),
        body
    );
    // Connect budget never exceeds the total budget: a 2s caller budget
    // must not block 5s in connect (hung-backend test would flake).
    let connect_budget = Duration::from_secs(5).min(budget.max(Duration::from_millis(50)));
    let mut s = TcpStream::connect_timeout(&sa, connect_budget).ok()?;
    let _ = s.set_write_timeout(Some(Duration::from_secs(5).min(budget)));
    if s.write_all(req.as_bytes()).is_err() {
        return None;
    }
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 16384];
    loop {
        let remaining = budget.checked_sub(start.elapsed()).unwrap_or(Duration::ZERO);
        if remaining.is_zero() {
            return None;
        }
        let _ = s.set_read_timeout(Some(remaining.min(Duration::from_secs(30))));
        match s.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > 4_194_304 {
                    break;
                }
            }
            Err(_) => {
                if start.elapsed() > budget {
                    return None;
                }
            }
        }
    }
    if buf.is_empty() {
        return None;
    }
    let text = String::from_utf8_lossy(&buf).to_string();
    let code: u16 = text.lines().next()?.split_whitespace().nth(1)?.parse().ok()?;
    let start = text.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0);
    Some((code, text[start.min(text.len())..].to_string()))
}

/// Block until `GET /v1/models` is 200 (or timeout). Servers call this
/// from a prober thread so `loop-studio status` stops racing server boot.
pub fn wait_for_http(port: u16, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if http_ready(port, 1500) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    false
}

/// Whether a supervising engine wrapper should launch again after an exit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DaemonDecision {
    Retry,
    Stop,
}

/// Consecutive launches that failed without the engine ever answering HTTP.
pub const GIVE_UP_AFTER: u32 = 5;

/// The retry decision, as a pure function of what has been observed.
///
/// Both `serve-mlx` and `serve-llama` are supervised by the same promise —
/// "it comes back by itself at 3am" — so the policy lives here rather than
/// being written twice and drifting.
///
/// The loop is for one failure: an engine that worked and then went away, a
/// transient OOM at 3am. That retries forever, because it does recover on its
/// own. The opposite case is a launch that never once answered HTTP: a typo'd
/// model id, missing weights, a port nothing can bind. That does not recover,
/// and retrying it every 60s left a daemon that looked alive to `launchctl`
/// and served nothing until somebody read the log. So a run that has never
/// been ready is bounded, and a run that has been ready is not.
///
/// `ever_ready` is the whole distinction, and it comes from each wrapper's
/// readiness prober rather than a guess at the child's exit status.
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

// ------------------------------------------------------------------ volumes -

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

/// True only when `mount` shows the path as a real mount point.
/// NEVER `create_dir_all` under /Volumes on a false here: that shadows
/// the future mount with an internal-disk directory of the same name.
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

/// Canonical model library: TB5 external when mounted, else internal
/// fallback. Callers must not create /Volumes paths without this gate.
pub fn model_base() -> String {
    if volume_mounted("/Volumes/AIModels") {
        "/Volumes/AIModels".to_string()
    } else {
        format!("{}/.lac/models", home_dir())
    }
}

// -------------------------------------------------------------------- files -

/// Write-temp-then-rename. A crash mid-write never leaves a half file,
/// which is what corrupted the Kanban queue under the old direct writes.
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

pub fn append_jsonl(path: &str, line: &str) {
    if let Some(parent) = Path::new(path).parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(path) {
        use std::io::Write as _;
        // One write syscall for the record and its newline. `writeln!` is
        // two, and under O_APPEND with a thread per connection two writers
        // can land their bodies adjacent before either newline, welding two
        // JSON objects into one unparseable line.
        let mut record = String::with_capacity(line.len() + 1);
        record.push_str(line);
        record.push('\n');
        let _ = f.write_all(record.as_bytes());
    }
}

// ---------------------------------------------------------------- log bounds --

/// The directory every launchd plist in this project writes its logs to.
///
/// `retarget_launchd_plist` re-roots `StandardOutPath`/`StandardErrorPath` here
/// while preserving the filename, so a daemon can find the file its own plist
/// opened without the plist having to tell it.
pub fn logs_dir() -> String {
    format!("{}/Library/Logs", home_dir())
}

// ------------------------------------------------------------ token reading --

/// A u64 field in a hand-rolled JSON line, or None. Shared by every reader of
/// the usage log so a field extraction is one implementation.
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

/// `est_tokens` summed over the usage log's lines within `window_secs` of `now`,
/// across the paths given. None when no line was readable at all, which is the
/// case that means "no data" rather than "an idle window".
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

/// `est_tokens` from the router's usage log over a window — a one-hour
/// extrapolation of what the gateway recorded, so an *estimate*, never a
/// measurement.
pub fn router_est_tokens(window_secs: u64) -> Option<u64> {
    let (live, rotated) = usage_log_paths();
    sum_est_tokens(&[live, rotated], now_unix(), window_secs)
}

/// A token count, and what kind of number it is.
///
/// The three sources are not the same kind of quantity, and two commands read
/// them: `kv-manage` reports the risk level, and the TUI *decides a context
/// window* from it. They used to read it two ways — `kv-manage` preferred
/// `LOOP_STUDIO_TOKENS` and the router's usage log, the TUI scraped `opencode2` and
/// nothing else — so the number the TUI decided with could disagree with the
/// number the operator was shown, by orders of magnitude, with both looking
/// authoritative. One implementation, and the provenance travels with it so a
/// caller can say which kind of number it is holding.
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

/// How a token source is described to an operator, and whether it is a
/// measurement. An estimate must never be printed or acted on as a reading.
pub fn token_source_is_measured(source: &str) -> bool {
    source == "explicit" || source == "env"
}

/// Legacy last-resort probe: the opencode2 health endpoint, scraped for a
/// number sitting next to the words "token" or "context".
///
/// Only integers attached to a token-like key count; bare-number scraping
/// produced false positives, so a value has to follow one of those words.
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

/// How large a daemon's own log may grow before it is rotated.
pub const LOG_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// How many rotated generations are kept beside the live log.
pub const LOG_KEEP: usize = 3;

/// Rotate `path` once it grows past `max_bytes`, keeping `keep` generations.
///
/// launchd opens `StandardOutPath`/`StandardErrorPath` and never rotates them,
/// so a daemon's log grows for as long as the machine is up — a router writes a
/// line per request and per attempt, and nothing in this project or in macOS
/// trims it. The daemon is the only party that can, and doing it here is safe
/// because launchd holds the file open with `O_APPEND`: the rename moves the
/// old bytes out of the way and a fresh file takes the name, so the writer's
/// next write lands at offset 0 rather than past the old end.
///
/// Returns the bytes rotated away, or 0 when nothing needed doing — including
/// for a file that does not exist, which is the case for a daemon whose logs an
/// operator has pointed somewhere else.
pub fn rotate_log_if_large(path: &str, max_bytes: u64, keep: usize) -> u64 {
    let size = match fs::metadata(path) {
        Ok(m) if m.is_file() => m.len(),
        _ => return 0,
    };
    if size <= max_bytes {
        return 0;
    }
    if keep == 0 {
        let _ = fs::remove_file(path);
        return size;
    }
    // Shift the generations up, dropping the oldest off the end.
    for i in (1..=keep).rev() {
        let from = format!("{}.{}", path, i);
        if i == keep {
            let _ = fs::remove_file(&from);
        } else {
            let to = format!("{}.{}", path, i + 1);
            if fs::rename(&from, &to).is_err() {
                // A generation that will not move must not stop the rotation.
                let _ = fs::remove_file(&from);
            }
        }
    }
    // Move the old bytes aside, then leave an empty file under the original
    // name for the writer to append to. If the rename could not happen,
    // truncating in place is still better than growing without bound, and
    // `O_APPEND` makes either safe.
    let _ = fs::rename(path, format!("{}.1", path));
    let _ = fs::write(path, b"");
    size
}

/// Keep this daemon's own launchd logs bounded for as long as it runs.
///
/// One thread per daemon, waking every `interval`. The names are the ones
/// `retarget_launchd_plist` writes into `~/Library/Logs`, so this follows the
/// file the plist actually opened rather than a copy of the path.
pub fn spawn_log_rotator(names: &[&str], max_bytes: u64, keep: usize, interval: Duration) {
    let owned: Vec<String> = names.iter().map(|s| s.to_string()).collect();
    std::thread::spawn(move || {
        loop {
            for name in &owned {
                let path = format!("{}/{}", logs_dir(), name);
                let rotated = rotate_log_if_large(&path, max_bytes, keep);
                if rotated > 0 {
                    eprintln!(
                        "[loop-studio] rotated {} ({} MiB, keeping {} generations)",
                        path,
                        rotated / (1024 * 1024),
                        keep
                    );
                }
            }
            std::thread::sleep(interval);
        }
    });
}

/// How much space this project's logs hold, and how many files they are.
///
/// Every log in `~/Library/Logs` this project writes is named
/// `loop-studio-*`, and the daemon rotates them but nothing trims the
/// generations: a machine up for months holds 8 MiB x 4 per daemon x 4
/// daemons. Nothing deletes an operator's diagnostics on a timer — that
/// decision is theirs — so the cost is reported instead, and
/// `purge_rotated_generations` reclaims it on their word.
///
/// The pre-rename `lac-*` names are still counted, so a machine that has not
/// reinstalled since the rename does not report zero for logs that are
/// actually there.
pub fn lac_logs_footprint() -> (u64, usize) {
    let mut bytes = 0;
    let mut files = 0;
    if let Ok(entries) = fs::read_dir(logs_dir()) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if !is_project_log_name(&name.to_string_lossy()) {
                continue;
            }
            // A size, not a recursive total: a stray directory is counted as
            // nothing rather than walked, because nothing should create one.
            if let Ok(meta) = entry.metadata() {
                if meta.is_file() {
                    bytes += meta.len();
                    files += 1;
                }
            }
        }
    }
    (bytes, files)
}

/// Is this file one of this project's logs in `~/Library/Logs`?
///
/// The current prefix is `loop-studio-`; the pre-rename `lac-` is still
/// recognised so logs written before the rename are counted and reclaimable
/// rather than invisible.
pub fn is_project_log_name(name: &str) -> bool {
    name.starts_with("loop-studio-") || name.starts_with("lac-")
}

/// Remove `path`'s rotated generations, leaving the live log in place.
///
/// The deliberate counterpart to `rotate_log_if_large`: that one bounds growth
/// automatically and keeps a few generations so a crash is still diagnosable;
/// this one is what an operator runs when they decide the disk is worth more
/// than the history. It never touches the live log, which is the file a
/// running daemon is appending to.
///
/// Returns the bytes reclaimed.
pub fn purge_rotated_generations(path: &str) -> (u64, usize) {
    let mut reclaimed = 0;
    let mut failed = 0;
    for i in 1..=64 {
        let generation = format!("{}.{}", path, i);
        match fs::metadata(&generation) {
            Ok(m) if m.is_file() => {
                // Count only what was actually removed. The bytes used to be
                // added before the unlink and the unlink's error discarded, so
                // a read-only volume or a file the operator had protected
                // produced "8.0 MiB reclaimed" for a directory that was
                // untouched — and the same bytes were still counted by
                // `loop-studio doctor` on the next run.
                match fs::remove_file(&generation) {
                    Ok(()) => reclaimed += m.len(),
                    Err(e) => {
                        failed += 1;
                        eprintln!("could not remove {generation}: {e}");
                    }
                }
            }
            // No `.i` means no `.i+1` either: the generations are contiguous.
            // A `.3` left behind by an interrupted rotation must not make this
            // spin through the rest of the range.
            _ => break,
        }
    }
    (reclaimed, failed)
}

/// Is this bind address the loopback interface?
///
/// One definition, used by the router that refuses to start on a non-loopback
/// bind without a token and by `loop-studio doctor`, which reports that refusal before
/// launchd gets the chance. There were two, and they disagreed: the doctor's
/// was three case-sensitive literals, so `LOOP_STUDIO_BIND_ADDR=LocalHost` failed the
/// diagnostic while the router started fine.
///
/// `::ffff:127.0.0.1` is the IPv4-mapped form of loopback. Rust's
/// `Ipv4Addr::is_loopback` does not accept it and the string form has to be
/// matched explicitly, so the two are named separately here rather than being
/// one more thing for a caller to remember.
pub fn is_loopback_host(h: &str) -> bool {
    let t = h.trim().to_lowercase();
    t == "127.0.0.1"
        || t == "localhost"
        || t == "::1"
        || t == "[::1]"
        || t == "::ffff:127.0.0.1"
        || t == "0:0:0:0:0:0:0:1"
}

/// Every name `loop-studio logs <which>` accepts, for the refusal message.
///
/// `tasks` is included even though it is not a daemon log: it is the transcript
/// tree, handled by its own arm. Leaving it out would produce a refusal that
/// does not mention the name the operator was one keystroke from typing.
pub fn log_which_names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = log_file_names();
    names.push("tasks");
    names
}

/// The launchd log names this project writes, for a daemon to rotate.
pub fn log_names_for(kind: &str) -> &'static [&'static str] {
    match kind {
        "mlx" => &["loop-studio-serve-mlx.log", "loop-studio-serve-mlx.err"],
        "llama" => &["loop-studio-serve-llama.log", "loop-studio-serve-llama.err"],
        "worker" => &["loop-studio-worker.log", "loop-studio-worker.err"],
        _ => &["loop-studio-router.log", "loop-studio-router.err"],
    }
}

/// Every log this project writes, derived from [`log_names_for`] rather than
/// written out again.
///
/// The count matters: `lac_logs_footprint` counts all of these, and the three
/// `*.err` files used to have no `loop-studio logs <which>` name that could reach them,
/// so the "reclaim with `loop-studio logs <which> --purge`" hint was naming a reclaim
/// that could not be typed. A reader that derives its list from the writer's
/// list cannot fall behind it.
pub fn log_file_names() -> Vec<&'static str> {
    ["mlx", "llama", "worker", "router"]
        .into_iter()
        .flat_map(log_names_for)
        .copied()
        .collect()
}

/// The per-task agent transcripts, in a directory of their own.
///
/// `~/.lac/task-logs/` gets a new timestamped file per task per attempt, with
/// no size cap, no rotation and no reader: `run_logged` tees the full
/// stdout+stderr of every `opencode2 run` and `make test` into one. A 24/7
/// worker on a repo with a slow test suite therefore grows this without bound,
/// and it was invisible to the one command that exists to report log cost —
/// `lac_logs_footprint` scans `~/Library/Logs` for `loop-studio-*`, a different
/// directory, so `loop-studio doctor` reported the daemons alone and said nothing about
/// a tree that is usually the larger of the two.
pub fn task_logs_dir() -> String {
    task_logs_dir_in(&home_dir())
}

/// [`task_logs_dir`] for an explicit home.
///
/// The `*_in` forms exist so the retention policy is testable against a
/// fixture directory. The first version of these tests set `HOME` instead,
/// which is process-global: two of them ran in parallel, each pointed the other
/// at its own tree, and both failed. A filesystem policy that can only be
/// tested by mutating global state is a policy that will be tested wrong.
pub fn task_logs_dir_in(home: &str) -> String {
    format!("{home}/.lac/task-logs")
}

/// Bytes and file count under [`task_logs_dir`], the same shape as
/// [`lac_logs_footprint`] so the two can be reported side by side.
///
/// Counts the tree rather than one directory level, because the transcripts are
/// the reason this exists: they are the files that actually accumulate.
pub fn task_logs_footprint() -> (u64, usize) {
    task_logs_footprint_in(&task_logs_dir())
}

/// [`task_logs_footprint`] for an explicit directory.
pub fn task_logs_footprint_in(dir: &str) -> (u64, usize) {
    let mut bytes = 0;
    let mut files = 0;
    walk_dir(dir, 0, &mut bytes, &mut files);
    (bytes, files)
}

/// Recursive size, with a depth cap so a symlink loop or a mis-set path cannot
/// spin. Depth 6 is far past anything a transcript tree should have.
fn walk_dir(dir: &str, depth: u32, bytes: &mut u64, files: &mut usize) {
    if depth > 6 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        // `metadata` follows symlinks; a link out of the tree would be counted
        // as if it lived here, so it is deliberately not followed. The
        // transcript writer only ever creates regular files.
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_dir() {
            walk_dir(&entry.path().to_string_lossy(), depth + 1, bytes, files);
        } else if meta.is_file() {
            *bytes += meta.len();
            *files += 1;
        }
    }
}

/// The oldest `count` transcripts by mtime, for the retention policy.
///
/// Returned oldest-first so the caller can delete from the front. A symlink is
/// reported by its link metadata rather than its target's, so a planted link
/// cannot make a young file look old and get the operator's real transcripts
/// deleted in its place.
pub fn oldest_task_logs(count: usize) -> Vec<(String, u64)> {
    oldest_task_logs_in(&task_logs_dir(), count)
}

/// [`oldest_task_logs`] for an explicit directory.
pub fn oldest_task_logs_in(dir: &str, count: usize) -> Vec<(String, u64)> {
    let mut dated: Vec<(std::time::SystemTime, String, u64)> = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let Ok(meta) = fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let Ok(modified) = meta.modified() else {
            continue;
        };
        dated.push((
            modified,
            entry.path().to_string_lossy().into_owned(),
            meta.len(),
        ));
    }
    dated.sort_by_key(|entry| entry.0);
    dated
        .into_iter()
        .take(count)
        .map(|(_, path, len)| (path, len))
        .collect()
}

/// How many per-task transcripts to keep, and the ceiling one may reach.
///
/// The transcripts are what makes a failed 3am task debuggable, so this is a
/// retention policy rather than a cap in the daemon-log sense: the newest
/// `TASK_LOG_KEEP` survive, and a single file over `TASK_LOG_MAX_BYTES` is
/// truncated at that boundary so one runaway build cannot hold the whole budget
/// on its own. Both numbers are named constants because the point at which an
/// operator needs to be told the tree is large is a policy, not an accident.
pub const TASK_LOG_KEEP: usize = 40;
pub const TASK_LOG_MAX_BYTES: u64 = 32 * 1024 * 1024;

/// Trim the transcript tree to [`TASK_LOG_KEEP`], and cap any file over
/// [`TASK_LOG_MAX_BYTES`]. Returns (files removed, bytes reclaimed).
///
/// Called after a task's transcript is closed, not while it is being written:
/// the newest transcript is by definition not a candidate, so a run in progress
/// is never touched. Rotation is the *daemon* policy in this project — bounded
/// automatically, generations kept so a crash stays diagnosable — so this does
/// the same thing rather than inventing a second rule.
pub fn trim_task_logs() -> (usize, u64) {
    trim_task_logs_in(&task_logs_dir())
}

/// [`trim_task_logs`] for an explicit directory.
pub fn trim_task_logs_in(dir: &str) -> (usize, u64) {
    let mut removed = 0;
    let mut bytes = 0;
    // Per-file cap first, so the size of what is about to be deleted is known.
    for (path, len) in oldest_task_logs_in(dir, usize::MAX) {
        if len <= TASK_LOG_MAX_BYTES {
            continue;
        }
        match fs::OpenOptions::new().write(true).open(&path) {
            Ok(f) => {
                // `set_len` truncates; no seek or write is needed, and
                // `make lint` denies an unused import.
                if f.set_len(TASK_LOG_MAX_BYTES).is_ok() {
                    bytes += len - TASK_LOG_MAX_BYTES;
                }
            }
            Err(_) => continue,
        }
    }
    // Then the count, oldest first: delete everything past the newest
    // `TASK_LOG_KEEP`. This was the other way round first time — it took the
    // *oldest* TASK_LOG_KEEP and deleted them, which is the one order that
    // throws away the recent failure and keeps the stale diagnostics. The
    // test caught it, which is the argument for asserting order and not just
    // the resulting count.
    let all = oldest_task_logs_in(dir, usize::MAX);
    let doomed = all.len().saturating_sub(TASK_LOG_KEEP);
    for (path, len) in all.into_iter().take(doomed) {
        if fs::remove_file(&path).is_ok() {
            removed += 1;
            bytes += len;
        }
    }
    (removed, bytes)
}

/// The file `loop-studio logs <which>` should show, for any spelling of `<which>` that
/// names one. `None` for anything else — an unrecognised name is a mistake the
/// operator should be told about, not silently answered with the router's log.
pub fn log_file_name(which: &str) -> Option<&'static str> {
    let key = which.trim().to_ascii_lowercase();
    let wanted = match key.as_str() {
        "mlx" | "serve-mlx" => "loop-studio-serve-mlx.log",
        "mlx-err" | "serve-mlx-err" => "loop-studio-serve-mlx.err",
        "llama" | "serve-llama" => "loop-studio-serve-llama.log",
        "llama-err" | "serve-llama-err" => "loop-studio-serve-llama.err",
        "worker" => "loop-studio-worker.log",
        "worker-err" => "loop-studio-worker.err",
        "router" => "loop-studio-router.log",
        "router-err" => "loop-studio-router.err",
        // The literal file names are accepted too, so an operator can paste
        // exactly the string `loop-studio doctor` and `ls` showed them.
        other if log_file_names().contains(&other) => other,
        _ => return None,
    };
    // Resolved inside the writer's own list, so an alias can never name a file
    // the rotator does not know about. The three `*.err` files used to be
    // counted by `loop-studio doctor` and by the footprint total but had no reachable
    // `loop-studio logs <which>` name, so the hint "reclaim with `loop-studio logs <which>
    // --purge`" named a reclaim nobody could type.
    log_file_names().into_iter().find(|n| *n == wanted)
}

/// The router's per-request usage log, and the rotated generation it keeps
/// beside it, as (live, rotated).
///
/// One function, because the writer and the reader had drifted: the writer
/// rotated to `router-usage.jsonl.1` while `kv-manage` looked for
/// `router-usage.1.jsonl`. The rotated window was therefore never read, the
/// documented "current + rotated window" sum was really just the current
/// file, and the token estimate fell to near zero at every 8MB rotation and
/// rebuilt from scratch. It also meant a reader landing in the window between
/// the rename and the next append saw an empty live file and no `.1` under
/// the name it knew, so it reported "no data" instead of the previous
/// window's total.
pub fn usage_log_paths() -> (String, String) {
    let live = format!("{}/.lac/router-usage.jsonl", home_dir());
    // `.jsonl.1`, the ordinary rotated-log suffix, and the name already on
    // disk — so this adopts the existing file rather than orphaning it.
    let rotated = format!("{live}.1");
    (live, rotated)
}

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

// -------------------------------------------------------------------- locks -

/// Crash-safe inter-process lock via atomic `create_dir` + owner record.
/// Prevents two `loop-studio worker` daemons (launchd + terminal) draining the
/// same queue twice. Stale locks (dead owner or age) are reclaimed.
pub struct DirLock {
    dir: String,
}

impl DirLock {
    pub fn dir(&self) -> &str {
        &self.dir
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        // Only remove a lock that is still ours. A reclaim deletes the
        // directory and the new owner writes its own record into it; the old
        // owner's `remove_dir_all` would then delete the *new* owner's lock, and
        // the queue would be handed to a third worker the moment either of them
        // exited. Comparing the owner record is the only thing that
        // distinguishes "my lock" from "a lock at my path".
        let owner_path = format!("{}/owner", self.dir);
        let still_ours = fs::read_to_string(&owner_path)
            .ok()
            .and_then(|record| {
                record
                    .split_whitespace()
                    .next()
                    .and_then(|pid| pid.parse::<i32>().ok())
            })
            .is_some_and(|pid| pid == std::process::id() as i32);
        if still_ours {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}

/// Liveness probe: Some(true) alive, Some(false) dead,
/// None when the probe itself could not run (fail-open unknown).
fn process_alive(pid: i32) -> Option<bool> {
    let out = Command::new("kill").args(["-0", &pid.to_string()]).output().ok()?;
    Some(out.status.success())
}

/// Whether an existing lock directory may be taken over.
///
/// Split out from [`acquire_lock`] because the rule is the whole safety
/// argument and it is worth asserting without a filesystem race. `owner` is
/// the record read from the lock directory, `dir_age_secs` how long ago that
/// directory was last modified, `probe` the pid liveness check.
///
/// The only unconditional reclaim is "the recorded pid is provably gone".
/// Everything else — no record, a record that will not parse, a pid the
/// platform cannot answer for — has to wait out `stale_secs`.
///
/// The previous version read an *unparsable* record as definitively stale
/// while treating a *missing* one conservatively, which is backwards: the
/// acquirer writes the record with `File::create` (O_TRUNC) and then
/// `write_all`, so between those two syscalls the file exists and is empty, and
/// a second worker that arrives in that window parsed `pid = -1`, declared the
/// lock stale and deleted it. Both workers then believed they held
/// `loop-studio-worker` and drained the same Kanban queue, which is the one thing this
/// lock exists to prevent. `kill -9` in that window makes the state permanent.
fn lock_is_reclaimable(
    owner: Option<&str>,
    dir_age_secs: Option<u64>,
    stale_secs: u64,
    probe: impl Fn(i32) -> Option<bool>,
) -> bool {
    let aged_out = dir_age_secs.is_some_and(|age| age >= stale_secs);
    let pid = owner
        .and_then(|record| record.split_whitespace().next())
        .and_then(|pid| pid.parse::<i32>().ok())
        .filter(|pid| *pid > 0);
    match pid {
        Some(pid) => match probe(pid) {
            // The owner is gone. Reclaim now rather than waiting: this is the
            // crash case the lock exists to recover from.
            Some(false) => true,
            // A live owner keeps the lock regardless of age. A forward clock
            // jump must not hand one queue to two workers, and a long-running
            // worker must not lose its lock for being slow.
            Some(true) => false,
            // The pid could not be probed at all, so age is all there is.
            None => aged_out,
        },
        None => aged_out,
    }
}

/// Acquire `~/.lac/locks/<name>`. Returns None when a live owner holds it.
/// `stale_secs` is only a backstop for when the owner pid cannot be probed;
/// a live owner keeps the lock regardless of wall-clock age (a forward
/// clock jump must never hand one queue to two workers).
pub fn acquire_lock(name: &str, stale_secs: u64) -> Option<DirLock> {
    let parent = format!("{}/.lac/locks", home_dir());
    let _ = fs::create_dir_all(&parent);
    let dir = format!("{}/{}", parent, name);
    for _ in 0..2 {
        match fs::create_dir(&dir) {
            Ok(()) => {
                let owner = format!("{} {}", std::process::id(), now_unix());
                // Truncate-then-write is the window `lock_is_reclaimable` is
                // written to survive, but the record is still written before
                // the lock is handed back.
                let _ = fs::write(format!("{}/owner", dir), owner);
                return Some(DirLock { dir });
            }
            Err(_) => {
                let owner_path = format!("{}/owner", dir);
                let owner = fs::read_to_string(&owner_path).ok();
                let dir_age = fs::metadata(&dir)
                    .and_then(|meta| meta.modified())
                    .ok()
                    .and_then(|modified| modified.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map(|modified| now_unix().saturating_sub(modified.as_secs()));
                if lock_is_reclaimable(owner.as_deref(), dir_age, stale_secs, process_alive) {
                    let _ = fs::remove_dir_all(&dir);
                    continue;
                }
                return None;
            }
        }
    }
    None
}

// ---------------------------------------------------------------------- git -

/// Current branch in `dir`, working even on unborn repos (no commits yet).
/// Falls back to whichever of main/master exists, else "main".
/// Takes a directory so daemon flows never depend on process cwd.
pub fn default_branch_in(dir: &str) -> String {
    if let Ok(out) = Command::new("git")
        .current_dir(dir)
        .args(["symbolic-ref", "--short", "HEAD"])
        .output()
    {
        if out.status.success() {
            let b = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !b.is_empty() {
                return b;
            }
        }
    }
    for cand in ["main", "master"] {
        let ok = Command::new("git")
            .current_dir(dir)
            .args(["show-ref", "--verify", &format!("refs/heads/{}", cand)])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            return cand.to_string();
        }
    }
    "main".to_string()
}

/// Current branch in the process working directory.
pub fn default_branch() -> String {
    default_branch_in(".")
}

/// Commit existence in `dir` (explicit cwd for daemon flows).
pub fn repo_has_commits_in(dir: &str) -> bool {
    Command::new("git")
        .current_dir(dir)
        .args(["rev-parse", "--verify", "HEAD"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// False on fresh `git init` repos (no HEAD) — callers must skip
/// branch/commit flows there instead of failing on `master`.
pub fn repo_has_commits() -> bool {
    repo_has_commits_in(".")
}

/// Dirtiness in `dir` (explicit cwd for daemon flows).
pub fn repo_dirty_in(dir: &str) -> bool {
    Command::new("git")
        .current_dir(dir)
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .map(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty())
        .unwrap_or(false)
}

/// True when `git status --porcelain` is non-empty (tracked mods or
/// untracked files). The worker treats a dirty tree as precious: it
/// never runs `checkout .` over user work.
pub fn repo_dirty() -> bool {
    Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .map(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty())
        .unwrap_or(false)
}

// -------------------------------------------------------------------- events -

/// Worker / daemon event log. JSONL so `grep`/jq keep working at 3am.
pub fn event_log_path() -> String {
    format!("{}/.lac/worker-events.jsonl", home_dir())
}

pub fn log_event(kind: &str, msg: &str) {
    let line = format!(
        "{{\"ts\":{},\"event\":\"{}\",\"msg\":\"{}\"}}",
        now_unix(),
        json_escape(kind),
        json_escape(msg)
    );
    append_jsonl(&event_log_path(), &line);
}

// ------------------------------------------------- native shell replacements -
// Every helper below exists to delete a shell-out: `which`, `tail`,
// `uname`, `pgrep`/`pkill`, and `pgrep -P` tree walks are now parsed or
// probed in Rust. Only one `ps` snapshot and one `kill` per signal remain
// as subprocesses (macOS offers no std API for either), and both are
// wrapped here so no binary shells out ad hoc anymore.

/// PATH lookup without spawning `which`. Returns the full path only when
/// the candidate exists AND has any execute bit (a shadow non-executable
/// file must not satisfy the probe).
pub fn which(cmd: &str) -> Option<String> {
    if cmd.contains('/') {
        let p = Path::new(cmd);
        if is_executable(p) {
            return Some(cmd.to_string());
        }
        return None;
    }
    let path = env::var("PATH").unwrap_or_default();
    for dir in path.split(':') {
        if dir.is_empty() {
            continue;
        }
        let cand = format!("{}/{}", dir, cmd);
        if is_executable(Path::new(&cand)) {
            return Some(cand);
        }
    }
    None
}

fn is_executable(p: &Path) -> bool {
    fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Which binary a generated launchd plist should point `ProgramArguments[0]`
/// at.
#[derive(Debug, PartialEq, Eq)]
pub enum ProgramKind {
    /// The installed copy is there and can run.
    Installed,
    /// Something is at the installed path but cannot be executed (absent
    /// execute bit, a directory, a 0-byte truncated file). The release
    /// binary is used instead and the operator is told why.
    InstalledUnusable,
    /// Nothing installed; use this checkout's release build.
    Release,
    /// Neither the installed copy nor the release build can run. There is no
    /// program to write, and an installer that writes a plist pointing at
    /// nothing still prints success — `launchctl bootstrap` does not exec at
    /// load time, so the failure surfaces minutes later as a dead agent.
    Missing,
}

/// The resolved program for a plist: the path to write, and why.
#[derive(Debug, PartialEq, Eq)]
pub struct ProgramChoice {
    pub kind: ProgramKind,
    /// The path that goes into the plist.
    pub path: String,
    /// Set when the operator's installed copy was passed over, so the
    /// installer can say so instead of silently retargeting a daemon at a
    /// different binary than the one they installed.
    pub warning: Option<String>,
}

/// Resolve the program for a launchd plist: the user's installed copy when
/// it can actually run, else the release build.
///
/// The installers asked only `metadata(path).is_ok()`, so existence was
/// mistaken for capability: a `~/.local/bin/loop-studio-router` left non-executable
/// (or truncated to 0 bytes — which is exactly what `loop-studio dashboard
/// --install` used to produce when `LOOP_STUDIO_STUDIO` named one of its own
/// destinations) became the daemon's `ProgramArguments[0]`. launchd then
/// respawned a file that cannot execute, forever, while the installer
/// printed `[ok] Placed:` and `loop-studio status` reported the gateway down.
pub fn choose_program_binary(installed: &str, release: &str) -> ProgramChoice {
    let path = Path::new(installed);
    if is_executable(path) {
        return ProgramChoice {
            kind: ProgramKind::Installed,
            path: installed.to_string(),
            warning: None,
        };
    }
    match fs::metadata(path) {
        Ok(m) => {
            let why = if m.is_dir() {
                "is a directory".to_string()
            } else {
                format!("is not executable (mode {:o})", m.permissions().mode() & 0o7777)
            };
            ProgramChoice {
                kind: ProgramKind::InstalledUnusable,
                path: release.to_string(),
                warning: Some(format!(
                    "{installed} {why} — pointing the agent at {release} instead"
                )),
            }
        }
        Err(_) => {
            // The fallback is only a fallback if it exists. An unbuilt or
            // `cargo clean`ed checkout is a normal state, and it is exactly
            // the state in which "installed" would be a lie.
            if is_executable(Path::new(release)) {
                ProgramChoice {
                    kind: ProgramKind::Release,
                    path: release.to_string(),
                    warning: None,
                }
            } else {
                ProgramChoice {
                    kind: ProgramKind::Missing,
                    path: String::new(),
                    warning: Some(format!(
                        "no runnable Loop Studio binary: {installed} is not installed and {release} does not exist or is not executable"
                    )),
                }
            }
        }
    }
}

/// The `model` string value out of an `opencode.jsonc`, comments and all.
///
/// One implementation, because there were two and only one of them was safe.
/// The TUI scanned for `"model"` and then stepped over quotes with hard-coded
/// byte offsets, so a file that ended at `"model"` indexed one past its length
/// and a file whose next byte was multi-byte UTF-8 sliced a character in
/// half — both panics, on the TUI's every menu redraw, from a file an
/// interrupted `>` redirect or a botched checkout can produce.
///
/// Byte-safe by construction: every offset comes from `find` on a `char`
/// boundary, and the value is collected a character at a time so an escape
/// sequence cannot end it early.
pub fn jsonc_model_id(source: &str) -> Option<String> {
    let text = strip_jsonc_comments(source);
    let at = text.find("\"model\"")?;
    let rest = &text[at + "\"model\"".len()..];
    let colon = rest.find(':')?;
    let after = rest[colon + 1..].trim_start();
    // A model id is a bare string; anything else is not one.
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

/// OS/arch without spawning `uname`. (Kernel release omitted on purpose:
/// nothing in the stack consumes it.)
pub fn os_descr() -> String {
    format!("{} {}", std::env::consts::OS, std::env::consts::ARCH)
}

/// True on Apple Silicon Macs (arm64). MLX (`mlx_lm.server`) only runs
/// here — Intel Macs must use the llama/Ollama lanes instead.
pub fn is_apple_silicon() -> bool {
    std::env::consts::ARCH == "aarch64" && std::env::consts::OS == "macos"
}

/// Human label for headers: "Apple Silicon" on arm64 macOS, else the
/// compile-time arch (e.g. "x86_64"). Uses `std::env::consts::ARCH`
/// (always available) — never `CARGO_CFG_TARGET_ARCH`, which only
/// exists at compile time and reads back as "unknown" at runtime.
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

/// RAM-pressure thresholds scaled by total physical RAM, so an 8GB
/// MacBook Air and a 96GB Studio get sane gates from the same code.
/// Returns (red_gib, yellow_gib): free < red is RED, < yellow is YELLOW.
pub fn mem_thresholds_gib() -> (f64, f64) {
    let total = total_ram_gib().unwrap_or(16.0);
    let red = (total * 0.04).clamp(1.5, 4.0);
    let yellow = (total * 0.12).clamp(3.0, 12.0);
    (red, yellow)
}

/// Smart-serve tiers scaled by total RAM: (high_gib, medium_gib).
/// Above high → quality lane; above medium → balanced; below → conserve.
/// On 96GB this is ~(38, 14), matching the old 40/16 constants.
pub fn serve_ram_tiers_gib() -> (f64, f64) {
    let total = total_ram_gib().unwrap_or(16.0);
    (total * 0.4, total * 0.15)
}

/// Last `n` lines of a file (trailing newline normalized). Replaces
/// `tail -n` with zero subprocesses.
///
/// Seeks from the end and reads a bounded tail, growing the window only if the
/// last `n` lines have not been seen yet. The old version was `fs::read` of the
/// whole file: fine for a config file, not for the one caller, which is
/// `loop-studio logs` over `~/Library/Logs/loop-studio-router.log` — a launchd
/// `StandardOutPath` that launchd never rotates and that the router appends
/// one line per request and per attempt to. Measured at 95 MiB of RSS to print
/// 100 lines, and unbounded from there: a busy week is multiple GB, read into
/// memory by a foreground CLI.
pub fn tail_file(path: &str, n: usize) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let want = n.max(1);
    let mut file = fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    // Start with a window that comfortably holds `want` lines, then double
    // until the count is satisfied or the whole file has been read.
    let mut window = 64 * 1024u64;
    loop {
        let start = len.saturating_sub(window);
        let mut buf = Vec::with_capacity((len - start) as usize);
        file.seek(SeekFrom::Start(start)).ok()?;
        file.read_to_end(&mut buf).ok()?;
        let text = String::from_utf8_lossy(&buf);
        let lines: Vec<&str> = text.lines().collect();
        // A partial first line (the seek landed mid-line) is dropped unless we
        // read the file from the beginning.
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

/// First `n` lines of a string. Replaces `| head -N` in display paths.
pub fn head_lines(s: &str, n: usize) -> String {
    s.lines().take(n.max(1)).collect::<Vec<_>>().join("\n")
}

/// One row of `ps -eo pid=,ppid=,command=`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcInfo {
    pub pid: u32,
    pub ppid: u32,
    pub cmd: String,
}

/// Parse one `ps` row: "<pid> <ppid> <command...>". Byte-index math lands
/// on ASCII whitespace/token boundaries only (pids are digits), so this
/// is panic-free on hostile input; kernel threads without commands yield None.
pub fn parse_ps_line(line: &str) -> Option<ProcInfo> {
    let t = line.trim_start();
    let p1e = t.find(|c: char| c.is_whitespace())?;
    let pid: u32 = t[..p1e].parse().ok()?;
    let rest = t[p1e..].trim_start();
    let p2e = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
    let ppid: u32 = rest[..p2e].parse().ok()?;
    let cmd = rest[p2e..].trim_start().to_string();
    if cmd.is_empty() {
        return None;
    }
    Some(ProcInfo { pid, ppid, cmd })
}

/// Single `ps` snapshot, parsed in Rust. The only process-listing
/// subprocess in the suite; every `pgrep`/`pkill` call site funnels here.
pub fn process_snapshot() -> Vec<ProcInfo> {
    Command::new("ps")
        .args(["-eo", "pid=,ppid=,command="])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .map(|t| t.lines().filter_map(parse_ps_line).collect())
        .unwrap_or_default()
}

/// True when `pattern` names the *program* this process is running, not
/// something that merely appears in its arguments.
///
/// This is the predicate every signal in the suite goes through, so it is
/// deliberately strict. `cmd.contains(pattern)` over the whole `ps` line
/// matched any process that mentioned the word anywhere in argv, and the
/// words this suite matches on are everywhere: an operator reading the lane
/// runs `tail -f ~/Library/Logs/loop-studio-serve-mlx.err`, `grep -r loop-studio-router .`,
/// or types the name into a shell — and `loop-studio stop` / the TUI's "stop engines"
/// then SIGTERMed the reader, the grep, and the shell itself. A kill is not
/// recoverable by the process that did not ask for it, so a substring match is
/// the wrong tool for one.
///
/// What counts as the program:
///   * the first token's file name, with the `loop-studio-`/`lac-` install alias normalized
///     (`loop-studio-serve-mlx` is the same program as `serve-mlx`; `make install`
///     ships both names for one binary);
///   * a module argument, for an engine someone started by hand under an
///     interpreter (`python3 -m mlx_lm.server`), so the TUI's belt-and-braces
///     engine sweep still finds an unwrapped lane;
///   * a subcommand, so `ollama serve` is still recognizable.
///
/// A pattern containing whitespace is matched token-by-token against the front
/// of the command, which is what makes `ollama serve` name one process rather
/// than every `ollama` invocation.
pub fn program_matches(cmd: &str, pattern: &str) -> bool {
    let tokens: Vec<&str> = cmd.split_whitespace().collect();
    let wanted: Vec<&str> = pattern.split_whitespace().collect();
    if wanted.is_empty() || tokens.len() < wanted.len() {
        return false;
    }
    if wanted.len() > 1 {
        // A multi-token pattern is a program plus literal arguments, so it can
        // only ever be the front of the command line.
        return program_matches_token(&module_file_name(tokens[0]), wanted[0])
            && wanted
                .iter()
                .skip(1)
                .enumerate()
                .all(|(index, token)| tokens[index + 1] == *token);
    }
    if program_matches_token(&module_file_name(tokens[0]), wanted[0]) {
        return true;
    }
    // A single-token pattern also names an engine someone started by hand
    // under an interpreter (`python3 -m mlx_lm.server`), which is exactly the
    // case the TUI's second engine sweep exists for: no wrapper owns it. Only
    // an interpreter is consulted — `llama-server -m model.gguf` uses the same
    // flag to mean the model, and that is its own business.
    if is_interpreter(&module_file_name(tokens[0])) {
        if let Some(at) = tokens.iter().position(|token| *token == "-m") {
            if let Some(module) = tokens.get(at + 1) {
                return program_matches_token(&module_file_name(module), wanted[0]);
            }
        }
    }
    false
}

fn is_interpreter(program: &str) -> bool {
    program == "python"
        || program == "python2"
        || program == "python3"
        || program == "uv"
        || (program.starts_with("python") && program[6..].starts_with('.'))
}

/// Last path component, tolerating the `(deleted)` / `(defunct)` suffix the
/// kernel appends to the argv of a process that is exiting or whose binary was
/// replaced under it.
fn module_file_name(token: &str) -> String {
    let base = token
        .split(" (deleted)")
        .next()
        .unwrap_or(token)
        .split(" (defunct)")
        .next()
        .unwrap_or(token);
    base.rsplit('/').next().unwrap_or(base).to_string()
}

/// Collapse the install-alias prefixes so one binary is found by either name.
///
/// `make install` writes each tool under two names — the plain one and a
/// prefixed one (`serve-mlx` and `loop-studio-serve-mlx`) — and the old
/// prefix was `lac-`. A match that only strips one of them finds an engine
/// under the name it was installed with and misses it under the other, so
/// `loop-studio stop` reports "not running" for a process it just started.
fn program_matches_token(program: &str, pattern: &str) -> bool {
    if program.is_empty() || pattern.is_empty() {
        return false;
    }
    fn strip(name: &str) -> &str {
        name.strip_prefix("loop-studio-")
            .or_else(|| name.strip_prefix("lac-"))
            .unwrap_or(name)
    }
    program == pattern || strip(program) == strip(pattern)
}

/// Full command line of one pid, or None when it is gone (`ps` exits non-zero
/// for a pid it cannot see). Used to confirm a recorded pid is still the
/// process we wrote it for before signalling it: a pid file is a claim about
/// the past, and pids are recycled.
pub fn command_of(pid: u32) -> Option<String> {
    let out = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Processes running `pattern`, excluding self. (The old `pgrep -af` wrapper
/// used to match its own probe command; the substring version matched far
/// more than that — see [`program_matches`].)
pub fn processes_matching(substr: &str) -> Vec<ProcInfo> {
    processes_matching_in(&process_snapshot(), substr)
}

/// [`processes_matching`] over a supplied snapshot, so the filter can be
/// pinned on a `ps` table that is known rather than on whatever the host
/// happens to be running.
pub fn processes_matching_in(snapshot: &[ProcInfo], substr: &str) -> Vec<ProcInfo> {
    let me = std::process::id();
    snapshot
        .iter()
        .filter(|p| p.pid != me && program_matches(&p.cmd, substr))
        .cloned()
        .collect()
}

/// Direct children of `pid` (replaces `pgrep -P`).
pub fn children_of(pid: u32) -> Vec<u32> {
    process_snapshot()
        .iter()
        .filter(|p| p.ppid == pid)
        .map(|p| p.pid)
        .collect()
}

/// Signal a pid via `kill` (macOS has no std API for arbitrary pids).
/// Returns false when the signal could not be delivered.
pub fn signal_pid(pid: u32, sig: &str) -> bool {
    Command::new("kill")
        .args([format!("-{}", sig), pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// SIGTERM every process running `pattern`. Returns kill count.
/// Replaces `pkill -f` with an auditable Rust loop. Matching is by program
/// identity, not by substring — see [`program_matches`] — because the cost of
/// a false positive here is a process the operator never asked to lose.
pub fn kill_matching(substr: &str) -> usize {
    processes_matching(substr)
        .iter()
        .filter(|p| signal_pid(p.pid, "TERM"))
        .count()
}

/// SIGKILL a process tree, children first (recursive). Best-effort:
/// races with natural exit are harmless. Replaces the old
/// `pgrep -P`-snapshot shell pipeline with the same semantics.
pub fn kill_tree(pid: u32) {
    for child in children_of(pid) {
        kill_tree(child);
    }
    signal_pid(pid, "9");
}

// --------------------------------------------------------------------- tests -

#[cfg(test)]
mod tests {
    // A `#[test]` that loses its attribute is still valid Rust and simply never
    // runs: the suite goes one test quieter and stays green. The file-level
    // `allow(dead_code)` exists because this module is shared by eight binaries
    // and most of it is unused by any one of them; inside the tests it is undone,
    // so that mistake is a compile error instead of a silent omission.
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
    fn page_size_plausible() {
        let ps = page_size();
        assert!(ps == 4096 || ps == 16384 || ps == 65536, "ps={}", ps);
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
        // No stray tmp file left behind.
        assert!(!Path::new(&format!("{}.tmp.{}", p, std::process::id())).exists());
    }

    #[test]
    fn lock_excludes_second_owner_and_releases_on_drop() {
        let name = format!("test-{}", std::process::id());
        let a = acquire_lock(&name, 60);
        assert!(a.is_some());
        assert!(acquire_lock(&name, 60).is_none());
        drop(a);
        assert!(acquire_lock(&name, 60).is_some());
    }

    /// The reclaim rule, on states that are awkward to produce deliberately
    /// from another process but trivial to state here. The one that matters is
    /// the empty record: the acquirer creates it with `File::create` (O_TRUNC)
    /// and then writes into it, so a second worker arriving in that window used
    /// to read `pid = -1`, call the lock stale, delete it, and take the queue
    /// while the first worker still held it.
    #[test]
    fn a_lock_is_reclaimed_only_on_proof_or_on_age() {
        let alive = |_: i32| Some(true);
        let dead = |_: i32| Some(false);
        let unprobeable = |_: i32| None;

        // Provably gone: reclaimed at once, which is the crash case.
        assert!(lock_is_reclaimable(Some("999999 100"), Some(0), 900, dead));
        // Alive: held, however old the directory is. A forward clock jump must
        // not hand one queue to two workers.
        assert!(!lock_is_reclaimable(Some("4242 100"), Some(100_000), 900, alive));
        // Alive and unprobeable pid: still held.
        assert!(!lock_is_reclaimable(Some("4242 100"), Some(5), 900, unprobeable));
        // Alive-looking but the platform cannot answer, and it is old enough.
        assert!(lock_is_reclaimable(Some("4242 100"), Some(901), 900, unprobeable));
        // The two states that used to disagree. An empty or unparsable record
        // is exactly the truncate window, so it waits out the backstop like a
        // missing record does; neither is a proof of anything.
        for record in [Some(""), Some("   "), Some("not-a-pid 5"), Some("-1 5"), None] {
            assert!(
                !lock_is_reclaimable(record, Some(10), 900, dead),
                "record {record:?} must not be taken as proof"
            );
            assert!(
                lock_is_reclaimable(record, Some(901), 900, dead),
                "record {record:?} ages out like a missing one"
            );
        }
        // No record and no age to judge by: hold.
        assert!(!lock_is_reclaimable(None, None, 900, dead));
    }

    /// A lock that was reclaimed out from under its owner must not be deleted
    /// by that owner's `Drop`. Otherwise the reclaimer's lock disappears the
    /// moment the old worker exits, and a third worker walks in.
    #[test]
    fn a_lock_only_removes_itself() {
        let name = format!("test-drop-owner-{}", std::process::id());
        let lock = acquire_lock(&name, 60).expect("first owner");
        let dir = lock.dir().to_string();
        // Simulate the reclaim: the directory is replaced by someone else's.
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("reclaim creates the directory");
        fs::write(format!("{}/owner", dir), format!("{} {}", std::process::id() + 1, now_unix()))
            .expect("new owner record");
        drop(lock);
        assert!(
            Path::new(&dir).exists(),
            "the new owner's lock must survive the old owner's drop"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_branch_never_empty() {
        assert!(!default_branch().is_empty());
    }

    #[test]
    fn a_signal_only_ever_reaches_the_program_it_names() {
        // The processes an operator has open while working on this stack. Each
        // of these mentions a pattern the suite kills on, and every one of them
        // was SIGTERMed by `loop-studio stop` or the TUI's "stop engines" when the
        // match was a substring of the whole command line.
        let bystanders = [
            ("/usr/bin/tail -f /Users/x/Library/Logs/lac-serve-mlx.err", "serve-mlx"),
            ("/usr/bin/less /Users/x/Library/Logs/lac-serve-llama.log", "serve-llama"),
            ("/usr/bin/grep -r loop-studio-router /Users/x/dev", "loop-studio-router"),
            ("/usr/bin/git diff -- rust-src/target/release/serve-mlx", "serve-mlx"),
            // The killer: the parent shell of `loop-studio stop` itself, whose argv
            // contains the whole command line the operator just typed.
            ("/bin/zsh -c sleep 300; : loop-studio-router", "loop-studio-router"),
            ("/Applications/Codex.app/Contents/MacOS/Codex run restart serve-mlx", "serve-mlx"),
            ("/usr/bin/vim notes # loop-studio-router", "loop-studio-router"),
            ("/usr/bin/open -a TextEdit ~/notes/loop-studio-router.md", "loop-studio-router"),
        ];
        for (cmd, pattern) in bystanders {
            assert!(
                !program_matches(cmd, pattern),
                "`{cmd}` must not be matched as {pattern}"
            );
        }
    }

    #[test]
    fn the_programs_we_mean_are_still_matched() {
        let owned = [
            ("/Users/x/.local/bin/loop-studio-router", "loop-studio-router", true),
            ("./target/release/loop-studio-router", "loop-studio-router", true),
            ("/Users/x/.local/bin/lac-serve-mlx", "serve-mlx", true),
            // `make install` writes the prefixed alias as well, so an engine
            // installed under it is the same program and has to be found.
            ("/Users/x/.local/bin/loop-studio-serve-mlx", "serve-mlx", true),
            ("/Users/x/.local/bin/loop-studio-serve-llama", "serve-llama", true),
            ("/Users/x/.local/bin/serve-llama", "serve-llama", true),
            ("/opt/homebrew/bin/llama-server --port 8081 -m q8.gguf", "llama-server", true),
            ("/Users/x/.venv/bin/mlx_lm.server --model m", "mlx_lm.server", true),
            // An engine started by hand under an interpreter still has to be
            // findable: that is exactly the case the TUI's second sweep exists
            // for, and it is how a lane is stopped when no wrapper owns it.
            ("/usr/bin/python3 -m mlx_lm.server --model m", "mlx_lm.server", true),
            ("/usr/local/bin/ollama serve", "ollama serve", true),
            ("/usr/local/bin/ollama runner x", "ollama serve", false),
            // Exiting processes keep a decodable argv.
            ("/Users/x/.local/bin/loop-studio-router (deleted)", "loop-studio-router", true),
        ];
        for (cmd, pattern, expected) in owned {
            assert_eq!(
                program_matches(cmd, pattern),
                expected,
                "`{cmd}` vs {pattern}"
            );
        }
    }

    #[test]
    fn every_matched_process_is_actually_that_program() {
        // The invariant, asserted against a live snapshot rather than a
        // fixture: whatever the walk returns, each row's own program has to be
        // the pattern. The substring version could not satisfy this.
        for pattern in ["loop-studio-router", "serve-mlx", "llama-server", "mlx_lm.server"] {
            for p in processes_matching(pattern) {
                assert!(
                    program_matches(&p.cmd, pattern),
                    "matched a non-program: {} ({})",
                    p.cmd,
                    pattern
                );
            }
        }
    }

    #[test]
    fn the_kill_path_filters_on_a_ps_table_not_on_the_whole_command_line() {
        // The property the caller has to keep, pinned on a table that cannot
        // change under us. Reverting the filter to `cmd.contains` fails here.
        let snapshot: Vec<ProcInfo> = [
            "/Users/x/.local/bin/loop-studio-router",
            "/usr/bin/tail -f /Users/x/Library/Logs/loop-studio-router.log",
            "/bin/zsh -c loop-studio stop",
            "/Users/x/.local/bin/loop-studio-tui",
            "/usr/bin/grep -rn loop-studio-router rust-src",
        ]
        .iter()
        .enumerate()
        .map(|(index, cmd)| ProcInfo { pid: 1000 + index as u32, ppid: 1, cmd: cmd.to_string() })
        .collect();
        let hits: Vec<String> = processes_matching_in(&snapshot, "loop-studio-router")
            .iter()
            .map(|p| p.cmd.clone())
            .collect();
        assert_eq!(hits, vec!["/Users/x/.local/bin/loop-studio-router".to_string()]);
        // Self is never a candidate even when it is the program named.
        let me = std::process::id();
        let mine = vec![ProcInfo { pid: me, ppid: 1, cmd: "loop-studio-router".to_string() }];
        assert!(processes_matching_in(&mine, "loop-studio-router").is_empty());
    }

    #[test]
    fn a_live_pid_reports_its_own_command_line() {
        let me = std::process::id();
        let command = command_of(me).expect("own pid is visible to ps");
        assert!(!command.is_empty());
        // The test binary's own name, so this also pins that `ps -p` returns
        // the argv rather than an empty row.
        assert!(
            command.contains("lac_bootstrap") || command.contains("common") || command.contains("-"),
            "unexpected own command line: {command}"
        );
        // A pid that cannot exist is reported as absent, not as a blank string
        // that a caller might mistake for "running, empty command".
        assert!(command_of(u32::MAX).is_none());
    }

    #[test]
    fn arch_helpers_consistent() {
        // arch_label never empty; mlx implies Apple Silicon.
        assert!(!arch_label().is_empty());
        assert!(!super::arch_label().is_empty());
        if mlx_supported() {
            assert!(is_apple_silicon());
            assert_eq!(arch_label(), "Apple Silicon");
        }
        // Compile-time arch and runtime consts must agree (the old
        // CARGO_CFG_TARGET_ARCH runtime read always missed).
        assert_eq!(is_apple_silicon(), std::env::consts::ARCH == "aarch64");
    }

    #[test]
    fn mem_thresholds_ordered_and_clamped() {
        let (red, yellow) = mem_thresholds_gib();
        assert!(red > 0.0 && yellow > red, "red={} yellow={}", red, yellow);
        assert!((1.5..=4.0).contains(&red), "red={}", red);
        assert!((3.0..=12.0).contains(&yellow), "yellow={}", yellow);
    }

    #[test]
    fn serve_tiers_ordered() {
        let (high, medium) = serve_ram_tiers_gib();
        assert!(high > medium && medium > 0.0, "high={} medium={}", high, medium);
    }

    #[test]
    fn retry_is_bounded_only_while_the_engine_has_never_worked() {
        use DaemonDecision::*;
        // Never ready: the permanent fault — a typo'd model id, missing
        // weights, or a port nothing can bind. Bounded, so the daemon stops
        // pretending to supervise something.
        for crashes in 0..GIVE_UP_AFTER {
            assert_eq!(daemon_decision(false, crashes), Retry, "crashes={crashes}");
        }
        assert_eq!(daemon_decision(false, GIVE_UP_AFTER), Stop);
        assert_eq!(daemon_decision(false, GIVE_UP_AFTER + 50), Stop);
        assert_eq!(daemon_decision(false, u32::MAX), Stop);
        // Ever ready: a crash after a working lane is exactly the transient
        // case the loop exists for, and it retries indefinitely.
        for crashes in [0, 1, GIVE_UP_AFTER, 1000, u32::MAX] {
            assert_eq!(daemon_decision(true, crashes), Retry, "crashes={crashes}");
        }
    }

    #[test]
    fn give_up_names_the_model_and_the_port() {
        let reason = daemon_give_up_reason("llama-server", "org/typo:Q4_K_M", 8081, 5, "exit 1");
        // An operator reading only this line has to be able to act on it.
        assert!(reason.contains("llama-server"), "{reason}");
        assert!(reason.contains("org/typo:Q4_K_M"), "{reason}");
        assert!(reason.contains("8081"), "{reason}");
        assert!(reason.contains("5 times"), "{reason}");
        assert!(reason.contains("permanent"), "{reason}");
    }

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
        // env mutation is process-global; this is the only test in this binary
        // that writes these three, and it restores them.
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

    #[test]
    fn usage_log_rotated_name_is_the_one_the_reader_asks_for() {
        let (live, rotated) = usage_log_paths();
        assert!(live.ends_with("/.lac/router-usage.jsonl"), "{live}");
        assert_eq!(rotated, format!("{live}.1"), "{rotated}");
    }

    #[test]
    fn plist_retarget_ports_across_macs() {        let tpl = r#"<?xml version="1.0"?>
<dict>
    <key>ProgramArguments</key>
    <array>
        <string>/Users/organic/dev/work/AI/loop-studio/rust-src/target/release/serve-mlx</string>
        <string>mlx-community/Qwen3.8-27B-4bit</string>
    </array>
    <key>WorkingDirectory</key>
    <string>/Users/organic/dev/work/AI/loop-studio</string>
    <key>StandardOutPath</key>
    <string>/Users/organic/Library/Logs/loop-studio-serve-mlx.log</string>
    <key>StandardErrorPath</key>
    <string>/Users/organic/Library/Logs/loop-studio-serve-mlx.err</string>
</dict>"#;
        let out = retarget_launchd_plist(tpl, "/Users/alice", "/ws/loop-studio", "/Users/alice/.local/bin/loop-studio-serve-mlx");
        assert!(out.contains("/Users/alice/.local/bin/loop-studio-serve-mlx"), "binary retargeted");
        assert!(out.contains("<string>/ws/loop-studio</string>"), "workdir pinned");
        assert!(out.contains("/Users/alice/Library/Logs/loop-studio-serve-mlx.log"), "stdout re-rooted");
        assert!(out.contains("/Users/alice/Library/Logs/loop-studio-serve-mlx.err"), "stderr re-rooted");
        assert!(!out.contains("/Users/organic"), "no prior-home leakage");
        // Second model arg untouched (only ProgramArguments[0] replaced).
        assert!(out.contains("mlx-community/Qwen3.8-27B-4bit"), "model arg preserved");
    }

    #[test]
    fn a_plist_value_is_matched_by_key_not_by_its_whitespace() {
        // The shape of launchd/org.loopstudio.router.plist: the <string> under a
        // nested <dict> is indented by eight, not four.
        let router = "<key>EnvironmentVariables</key>\n    <dict>\n        <key>LOOP_STUDIO_ROUTER_PORT</key>\n        <string>8000</string>\n        <!-- note -->\n    </dict>";
        let out = set_plist_string(router, "LOOP_STUDIO_ROUTER_PORT", "9000")
            .expect("the key is present, so this must retarget");
        assert!(out.contains("<key>LOOP_STUDIO_ROUTER_PORT</key>"), "{out}");
        assert!(out.contains("<string>9000</string>"), "{out}");
        assert!(!out.contains("8000"), "{out}");
        // The literal-whitespace replace this replaced, for the record.
        assert!(router.replace(
            "<key>LOOP_STUDIO_ROUTER_PORT</key>\n    <string>8000</string>",
            "<key>LOOP_STUDIO_ROUTER_PORT</key>\n    <string>9000</string>"
        ) == router, "the old needle never matched the shipped template");
        // A key that is not there is reported, not silently ignored.
        assert!(set_plist_string(router, "LOOP_STUDIO_MISSING_KEY", "1").is_none());
        // A key with no string value is reported too, rather than rewriting
        // whatever `<string>` happens to come later in the file.
        let no_value = "<key>KeepAlive</key>\n    <true/>\n    <key>Other</key>\n    <string>keep</string>";
        assert!(set_plist_string(no_value, "KeepAlive", "false").is_none());
    }

    #[test]
    fn which_finds_shell_and_rejects_ghosts() {
        let sh = which("sh");
        assert!(sh.is_some(), "sh must resolve via PATH");
        assert!(which("definitely-not-a-binary-xyz").is_none());
        // Explicit path without exec bit (or missing) is rejected.
        assert!(which("/nonexistent/path/xyz").is_none());
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

    /// The tail has to come off the end of a file that grows without limit, so
    /// the read is bounded by the window rather than by the file. Asserted by
    /// content (a big file, an exact answer) and by construction: a 64 MiB file
    /// whose 100th-from-last line is the very first one, so the window has to
    /// grow, and the result must still be right.
    #[test]
    fn tail_of_a_huge_file_reads_a_window_not_the_file() {
        let p = format!(
            "{}/loop-studio-test-tail-big-{}.log",
            std::env::temp_dir().to_string_lossy(),
            std::process::id()
        );
        let filler = "x".repeat(1023);
        let mut file = fs::File::create(&p).unwrap();
        // 64 MiB of 1 KiB lines, then the two lines the answer is made of.
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

    #[test]
    fn ps_line_parsing() {
        let p = parse_ps_line("  123   456 /usr/bin/foo --bar baz").unwrap();
        assert_eq!(p, ProcInfo { pid: 123, ppid: 456, cmd: "/usr/bin/foo --bar baz".to_string() });
        assert!(parse_ps_line("not a row").is_none());
        assert!(parse_ps_line("12").is_none());
        assert!(parse_ps_line("  7   1 ").is_none()); // kernel thread, no command
        assert!(parse_ps_line("").is_none());
    }

    #[test]
    fn snapshot_contains_self() {
        let me = std::process::id();
        let snap = process_snapshot();
        assert!(!snap.is_empty());
        assert!(snap.iter().any(|p| p.pid == me));
    }

    #[test]
    fn kill_tree_reaps_own_child() {
        let mut child = Command::new("sleep")
            .arg("120")
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("sleep must exist");
        let pid = child.id();
        kill_tree(pid);
        let start = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(st)) => {
                    assert!(!st.success(), "SIGKILLed child must not exit 0");
                    break;
                }
                Ok(None) => {
                    assert!(start.elapsed() < Duration::from_secs(10), "child survived kill_tree");
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => panic!("try_wait: {}", e),
            }
        }
    }
}

#[cfg(test)]
mod child_tests {
    // See the note in `mod tests`: a dropped `#[test]` must not be able
    // to make this suite quieter and still green.
    #![deny(dead_code)]

    use super::*;
    use std::sync::atomic::Ordering;

    /// The invariant the whole child-lifetime block exists for: a published
    /// pid is the one a handler will signal, and it is withdrawn the moment
    /// the child is reaped so a later signal cannot land on a recycled pid.
    #[test]
    fn child_pid_is_published_and_withdrawn() {
        set_child_pid(4242);
        assert_eq!(CHILD_PID.load(Ordering::SeqCst), 4242);
        clear_child_pid();
        assert_eq!(
            CHILD_PID.load(Ordering::SeqCst),
            0,
            "a stale pid must never outlive its child"
        );
        // Idempotent, and safe to install with no child present.
        forward_terminate_to_child_process();
        forward_terminate_to_child_process();
    }

    /// Backoff must be interruptible, or `loop-studio stop` during a 60s wait is
    /// ignored and the lane comes back after the operator asked for it down.
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
        // The old fallback was ".", which is not a home directory: it is
        // "the directory this process was launched from". Every state path
        // became cwd-relative, so two runs in different cwds disagreed about
        // where the queue was and a `.lac/` tree appeared in the checkout.
        assert!(home_dir_from(None).is_err());
        assert!(home_dir_from(Some("")).is_err());
        assert!(home_dir_from(Some("   ")).is_err());
        assert!(home_dir_from(Some(".")).is_err());
        assert!(home_dir_from(Some("..")).is_err());
        assert!(home_dir_from(Some("home/u")).is_err());
        assert!(home_dir_from(Some("home/u/")).is_err());
        // A tilde is not expanded by the kernel, and neither is it here.
        assert!(home_dir_from(Some("~/loop-studio")).is_err());
        assert_eq!(
            home_dir_from(Some("/home/u")).as_deref(),
            Ok("/home/u"),
            "an absolute path is the only thing that names a home"
        );
        assert_eq!(home_dir_from(Some("/Volumes/AIModels")).as_deref(), Ok("/Volumes/AIModels"));
    }

    /// A launchd plist must not point at a file that cannot run.
    ///
    /// The installers asked only whether the path existed, so a truncated or
    /// non-executable `~/.local/bin/loop-studio-router` became
    /// `ProgramArguments[0]`: launchd respawned a file that exits
    /// immediately, forever, while the installer printed `[ok] Placed:`.
    #[test]
    fn launchd_program_requires_an_executable_file() {
        let dir = std::env::temp_dir().join(format!(
            "loop-studio-program-choice-{}-{}",
            std::process::id(),
            now_unix()
        ));
        fs::create_dir_all(&dir).expect("temp dir");
        let installed = dir.join("loop-studio-router");
        let release = "/checkout/rust-src/target/release/loop-studio-router";

        // A release build that exists and runs: the normal fallback, no complaint.
        let real_release = dir.join("loop-studio-router-release");
        fs::write(&real_release, b"#!/bin/sh\n").expect("release build");
        fs::set_permissions(&real_release, fs::Permissions::from_mode(0o755)).expect("chmod");
        let real_release = real_release.to_string_lossy().into_owned();
        let picked = choose_program_binary(&installed.to_string_lossy(), &real_release);
        assert_eq!(picked.kind, ProgramKind::Release);
        assert_eq!(picked.path, real_release);
        assert_eq!(picked.warning, None);

        // An unbuilt or `cargo clean`ed checkout is a normal state, and it is
        // exactly the state in which "installed" would be a lie: launchd loads
        // a plist whose program does not exist and then respawns a failure
        // forever, and only says so minutes later.
        let picked = choose_program_binary(&installed.to_string_lossy(), release);
        assert_eq!(picked.kind, ProgramKind::Missing);
        assert!(picked.path.is_empty(), "no program may be invented: {:?}", picked.path);
        let warning = picked.warning.expect("the operator is told why nothing was placed");
        assert!(warning.contains("no runnable Loop Studio binary"), "{warning}");
        assert!(warning.contains(release), "{warning}");

        // A binary that exists but cannot run: mode stripped, or truncated.
        fs::write(&installed, b"").expect("truncated binary");
        let picked = choose_program_binary(&installed.to_string_lossy(), &real_release);
        assert_eq!(picked.kind, ProgramKind::InstalledUnusable);
        assert_eq!(
            picked.path, real_release,
            "an unrunnable file must not become the daemon"
        );
        assert_ne!(picked.path, installed.to_string_lossy());
        let warning = picked.warning.expect("the operator is told");
        assert!(warning.contains("not executable"), "{warning}");
        assert!(warning.contains(&real_release), "{warning}");

        // Present and runnable: the installed copy wins.
        fs::set_permissions(&installed, fs::Permissions::from_mode(0o755)).expect("chmod");
        let picked = choose_program_binary(&installed.to_string_lossy(), &real_release);
        assert_eq!(picked.kind, ProgramKind::Installed);
        assert_eq!(picked.path, installed.to_string_lossy());
        assert_eq!(picked.warning, None);

        // Executable bit stripped again (a `git checkout` of a mode-644 file,
        // a `cp` without -p, a zip round-trip).
        fs::set_permissions(&installed, fs::Permissions::from_mode(0o644)).expect("chmod -x");
        let picked = choose_program_binary(&installed.to_string_lossy(), &real_release);
        assert_eq!(picked.kind, ProgramKind::InstalledUnusable);
        assert_eq!(picked.path, real_release);

        // A directory where a binary should be is not a binary.
        let as_dir = dir.join("loop-studio");
        fs::create_dir_all(&as_dir).expect("mkdir");
        let picked = choose_program_binary(&as_dir.to_string_lossy(), &real_release);
        assert_eq!(picked.kind, ProgramKind::InstalledUnusable);
        assert_eq!(picked.path, real_release);
        assert!(picked.warning.unwrap().contains("is a directory"));

        let _ = fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod log_rotation_tests {
    // See the note in `mod tests`: a dropped `#[test]` must not be able
    // to make this suite quieter and still green.
    #![deny(dead_code)]

    use super::*;

    /// launchd opens a daemon's log and never rotates it, so a file nobody
    /// trims is the one file in this project that grows for as long as the
    /// machine is up. The rule: rotate only past the limit, keep a bounded
    /// number of generations, and leave an empty file for the writer.
    #[test]
    fn a_log_is_rotated_past_the_limit_and_keeps_bounded_generations() {
        let dir = std::env::temp_dir().join(format!(
            "loop-studio-log-rotate-{}-{}",
            std::process::id(),
            now_unix()
        ));
        fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("loop-studio-router.log").to_string_lossy().into_owned();

        // Under the limit: untouched, contents intact.
        fs::write(&path, b"small").expect("write");
        assert_eq!(rotate_log_if_large(&path, 100, 3), 0);
        assert_eq!(fs::read_to_string(&path).unwrap(), "small");

        // Over the limit: rotated, and a fresh file left for the writer.
        fs::write(&path, vec![b'x'; 200]).expect("write");
        assert_eq!(rotate_log_if_large(&path, 100, 3), 200);
        assert_eq!(
            fs::metadata(&path).unwrap().len(),
            0,
            "the writer needs an empty file under the original name"
        );
        assert_eq!(fs::metadata(format!("{path}.1")).unwrap().len(), 200);

        // Generations shift up.
        fs::write(&path, vec![b'y'; 150]).expect("write");
        assert_eq!(rotate_log_if_large(&path, 100, 3), 150);
        assert_eq!(fs::metadata(format!("{path}.1")).unwrap().len(), 150);
        assert_eq!(fs::metadata(format!("{path}.2")).unwrap().len(), 200);

        fs::write(&path, vec![b'z'; 120]).expect("write");
        assert_eq!(rotate_log_if_large(&path, 100, 3), 120);
        assert_eq!(fs::metadata(format!("{path}.1")).unwrap().len(), 120);
        assert_eq!(fs::metadata(format!("{path}.2")).unwrap().len(), 150);
        assert_eq!(fs::metadata(format!("{path}.3")).unwrap().len(), 200);

        // The fourth rotation drops the oldest rather than growing a fourth
        // generation — this is the bound that makes the whole thing worth doing.
        fs::write(&path, vec![b'w'; 110]).expect("write");
        assert_eq!(rotate_log_if_large(&path, 100, 3), 110);
        assert!(
            fs::metadata(format!("{path}.4")).is_err(),
            "kept a fourth generation"
        );
        assert_eq!(fs::metadata(format!("{path}.3")).unwrap().len(), 150);

        let _ = fs::remove_dir_all(&dir);
    }

    /// A daemon whose logs an operator pointed elsewhere has no file here, and
    /// a missing file must never be an error or a reason to create one.
    #[test]
    fn an_absent_or_foreign_file_is_never_touched() {
        let dir = std::env::temp_dir().join(format!(
            "loop-studio-log-rotate-missing-{}-{}",
            std::process::id(),
            now_unix()
        ));
        fs::create_dir_all(&dir).expect("temp dir");
        let absent = dir.join("nothing-here.log").to_string_lossy().into_owned();
        assert_eq!(rotate_log_if_large(&absent, 1, 3), 0);
        assert!(
            !std::path::Path::new(&absent).exists(),
            "rotation created a file nobody was writing"
        );

        // A directory where a log should be is not rotated either.
        let as_dir = dir.join("loop-studio-router.log");
        fs::create_dir_all(&as_dir).expect("mkdir");
        assert_eq!(rotate_log_if_large(&as_dir.to_string_lossy(), 1, 3), 0);

        // `keep = 0` is the degenerate policy: keep nothing, truncate away.
        let flat = dir.join("flat.log").to_string_lossy().into_owned();
        fs::write(&flat, vec![b'q'; 50]).expect("write");
        assert_eq!(rotate_log_if_large(&flat, 10, 0), 50);
        assert!(!std::path::Path::new(&flat).exists());

        let _ = fs::remove_dir_all(&dir);
    }

    /// The usage log keeps exactly one previous generation, because that is the
    /// one `kv-manage` reads: a third would be a file nothing ever opens. The
    /// rotation is the same helper the launchd logs use, with the policy the
    /// reader's `(live, rotated)` pair implies.
    #[test]
    fn the_usage_log_keeps_exactly_the_generation_its_reader_reads() {
        let dir = std::env::temp_dir().join(format!(
            "loop-studio-usage-rotate-{}-{}",
            std::process::id(),
            now_unix()
        ));
        fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("router-usage.jsonl").to_string_lossy().into_owned();

        for round in 1..=3u8 {
            fs::write(&path, vec![b'x'; 2_000 * u64::from(round) as usize]).expect("write");
            assert!(rotate_log_if_large(&path, 1_000, 1) > 0, "round {round} must rotate");
            assert!(
                fs::metadata(format!("{path}.2")).is_err(),
                "kept a generation the reader never reads"
            );
        }
        // Exactly one previous generation, holding the round before last —
        // each rotation replaces it, which is the point: the reader sums two
        // files, so a third would be a file nothing opens.
        assert_eq!(fs::metadata(format!("{path}.1")).unwrap().len(), 6_000);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Both implementations of the reclaim rule — this one and the Studio's —
    /// are checked against one shared table, so "kept in sync by hand" becomes
    /// "provably the same rule". A divergence fails whichever suite runs next.
    #[test]
    fn this_rule_matches_the_shared_parity_table() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/lock-parity.tsv");
        let table = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{} unreadable: {e}", path.display()));
        let mut seen: Vec<&str> = Vec::new();
        for line in table.lines() {
            if line.trim().is_empty() || line.starts_with('#') {
                continue;
            }
            let cols: Vec<&str> = line.split('\t').collect();
            assert_eq!(cols.len(), 5, "malformed parity row: {line:?}");
            let (case, owner, age, probe, expect) = (cols[0], cols[1], cols[2], cols[3], cols[4]);
            let owner: Option<&str> = if owner == "-" { None } else { Some(owner) };
            let age: Option<u64> = age.parse().ok();
            let probe = |_pid: i32| match probe {
                "alive" => Some(true),
                "dead" => Some(false),
                "unprobeable" => None,
                other => panic!("unknown probe {other:?} in row {case}"),
            };
            let got = lock_is_reclaimable(owner, age, 30, probe);
            let want = match expect {
                "reclaim" => true,
                "keep" => false,
                other => panic!("unknown expectation {other:?} in row {case}"),
            };
            assert_eq!(got, want, "parity row {case} disagrees: reclaimable={got}");
            seen.push(case);
        }
        // The exact set, not a floor. A row-count floor is satisfied by any
        // table at least that long, so the rows it stops covering are exactly
        // the ones nobody would notice losing — which is the failure this
        // table exists to make impossible. Naming all nineteen means a deleted
        // row fails here and a renamed one fails too, and the Studio's copy of
        // this list is what keeps the two halves from drifting apart silently.
        const EXPECTED: [&str; 19] = [
            "no-record-fresh",
            "no-record-at-backstop",
            "no-record-aged",
            "no-record-unknown-age",
            "empty-record-fresh",
            "empty-record-at-backstop",
            "garbage-record-fresh",
            "garbage-record-at-backstop",
            "negative-pid-fresh",
            "negative-pid-aged",
            "live-owner-ancient",
            "live-owner-just-born",
            "dead-owner-fresh",
            "dead-owner-ancient",
            "unprobeable-fresh",
            "unprobeable-just-under",
            "unprobeable-at-backstop",
            "legacy-live-owner",
            "legacy-dead-owner",
        ];
        seen.sort_unstable();
        let mut expected = EXPECTED;
        expected.sort_unstable();
        assert_eq!(
            seen,
            expected,
            "the parity table is not the agreed set of cases: a row was removed, added or renamed"
        );
    }

    /// The names each daemon rotates are the ones the plists open, so this
    /// follows the file rather than a copy of the path.
    #[test]
    fn every_daemon_rotates_the_logs_its_plist_opens() {
        assert_eq!(log_names_for("router"), &["loop-studio-router.log", "loop-studio-router.err"]);
        assert_eq!(log_names_for("mlx"), &["loop-studio-serve-mlx.log", "loop-studio-serve-mlx.err"]);
        assert_eq!(log_names_for("worker"), &["loop-studio-worker.log", "loop-studio-worker.err"]);
        // An unknown kind rotates the router's, which is the suite's default.
        assert_eq!(log_names_for("nonsense"), &["loop-studio-router.log", "loop-studio-router.err"]);
    }

    /// The rotation is only real if the names match the files the launchd
    /// plists actually open. A rename on either side would leave the daemon
    /// rotating a file nobody writes — silently, and forever.
    #[test]
    fn the_names_are_the_ones_the_plists_open() {
        let launchd = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../launchd");
        let plist = |file: &str| -> String {
            fs::read_to_string(launchd.join(file))
                .unwrap_or_else(|e| panic!("{} unreadable: {e}", launchd.join(file).display()))
        };
        let opened = |xml: &str| -> Vec<String> {
            let mut names = Vec::new();
            for key in ["<key>StandardOutPath</key>", "<key>StandardErrorPath</key>"] {
                let at = xml.find(key).unwrap_or_else(|| panic!("{key} missing"));
                let start = xml[at..]
                    .find("<string>")
                    .map(|i| at + i + "<string>".len())
                    .expect("a string value");
                let end = xml[start..]
                    .find("</string>")
                    .map(|i| start + i)
                    .expect("a closed string");
                names.push(
                    xml[start..end]
                        .rsplit('/')
                        .next()
                        .unwrap_or_default()
                        .to_string(),
                );
            }
            names
        };
        for (plist_file, kind) in [
            ("org.loopstudio.router.plist", "router"),
            ("org.loopstudio.serve-mlx.plist", "mlx"),
            ("org.loopstudio.worker.plist", "worker"),
        ] {
            let from_plist = opened(&plist(plist_file));
            let rotating: Vec<String> = log_names_for(kind)
                .iter()
                .map(|s| s.to_string())
                .collect();
            assert_eq!(
                rotating, from_plist,
                "{plist_file} opens {from_plist:?} but the {kind} daemon rotates {rotating:?}"
            );
        }
    }
}

#[cfg(test)]
mod log_retention_tests {
    use super::*;

    /// The counterpart to rotation: what an operator runs when they decide the
    /// disk is worth more than the history. It reclaims every generation and
    /// never touches the live log, which is the file a running daemon appends
    /// to.
    #[test]
    fn purging_reclaims_the_generations_and_leaves_the_live_log() {
        let dir = std::env::temp_dir().join(format!(
            "loop-studio-log-purge-{}-{}",
            std::process::id(),
            now_unix()
        ));
        fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("loop-studio-router.log").to_string_lossy().into_owned();

        fs::write(&path, b"live").expect("write live");
        for i in 1..=3 {
            fs::write(format!("{path}.{i}"), vec![b'x'; i * 100]).expect("write generation");
        }
        // An unrelated file beside them is not ours to remove.
        fs::write(dir.join("something-else.log"), b"keep me").expect("write other");

        assert_eq!(purge_rotated_generations(&path), (100 + 200 + 300, 0));
        assert_eq!(fs::read_to_string(&path).unwrap(), "live", "the live log must survive");
        for i in 1..=3 {
            assert!(
                fs::metadata(format!("{path}.{i}")).is_err(),
                "generation {i} survived the purge"
            );
        }
        assert!(fs::metadata(dir.join("something-else.log")).is_ok());

        // A second purge with nothing left reclaims nothing and is not an error.
        assert_eq!(purge_rotated_generations(&path), (0, 0));
        let _ = fs::remove_dir_all(&dir);
    }

    /// Generations are contiguous, so the first gap ends the walk — a `.3`
    /// left behind by an interrupted rotation must not make the purge spin.
    #[test]
    fn a_gap_ends_the_walk() {
        let dir = std::env::temp_dir().join(format!(
            "loop-studio-log-purge-gap-{}-{}",
            std::process::id(),
            now_unix()
        ));
        fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("loop-studio-router.log").to_string_lossy().into_owned();
        fs::write(format!("{path}.1"), vec![b'x'; 10]).expect("write");
        fs::write(format!("{path}.3"), vec![b'x'; 10]).expect("write");
        assert_eq!(purge_rotated_generations(&path), (10, 0));
        assert!(fs::metadata(&path).is_err(), "no live log to protect");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Reclaimed means gone. The bytes were added to the total *before* the
    /// unlink and the unlink's error was thrown away, so a purge against a
    /// read-only volume or a file the operator had protected reported
    /// "8.0 MiB reclaimed" for a directory that was still exactly as large —
    /// and the same bytes went straight back into the `loop-studio doctor` total on
    /// the next run. A failure is counted, not swallowed, and it is a failure
    /// the caller can exit on.
    #[test]
    fn bytes_the_purge_could_not_reclaim_are_not_counted() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "loop-studio-log-purge-denied-{}-{}",
            std::process::id(),
            now_unix()
        ));
        fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("loop-studio-router.log").to_string_lossy().into_owned();
        fs::write(format!("{path}.1"), vec![b'x'; 4096]).expect("write generation");
        // A directory the process cannot unlink from. Root ignores the mode
        // bit, so the test says so rather than passing vacuously.
        let mode = fs::metadata(&dir).expect("dir meta").permissions().mode();
        let root = running_as_root();
        if root {
            eprintln!("skipping: root ignores directory permissions");
            let _ = fs::remove_dir_all(&dir);
            return;
        }
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).expect("chmod");
        let (reclaimed, failed) = purge_rotated_generations(&path);
        fs::set_permissions(&dir, fs::Permissions::from_mode(mode)).expect("restore");

        assert_eq!(failed, 1, "a denied unlink must be reported as a failure");
        assert_eq!(
            reclaimed, 0,
            "bytes that were not removed must not be reported as reclaimed"
        );
        assert!(
            fs::metadata(format!("{path}.1")).is_ok(),
            "the file is still there, so nothing was reclaimed"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    unsafe extern "C" {
        fn geteuid() -> u32;
    }

    /// True when the process could ignore a directory's write permission, which
    /// would make the denied-unlink test pass for the wrong reason.
    #[cfg(unix)]
    fn running_as_root() -> bool {
        // A genuine FFI call, so the `unsafe` block is required rather than
        // redundant — and `make lint` denies warnings, so it has to be here.
        unsafe { geteuid() == 0 }
    }

    /// The transcript tree is bounded, oldest-first, and the newest survive.
    ///
    /// `run_logged` writes one timestamped file per task per attempt. Nothing
    /// read that tree, nothing bounded it, and nothing reported it: a 24/7
    /// worker on a repo with a slow suite grew `~/.lac/task-logs` for as long
    /// as the machine was up while `loop-studio doctor` reported the daemon logs alone.
    /// The trim is what makes the growth an operator's choice rather than a
    /// consequence — so what is asserted here is the *order*, because
    /// "delete the wrong 40 transcripts" is the failure mode that matters: the
    /// newest ones hold the failure you would want to read.
    #[test]
    fn the_transcript_tree_is_trimmed_oldest_first() {
        let dir = std::env::temp_dir().join(format!(
            "loop-studio-task-logs-{}-{}",
            std::process::id(),
            now_unix()
        ));
        let logs = task_logs_dir_in(&dir.to_string_lossy());
        fs::create_dir_all(&logs).expect("transcript dir");

        // 50 files with distinct mtimes, oldest first.
        let count = TASK_LOG_KEEP + 10;
        let base = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        for i in 0..count {
            let path = std::path::Path::new(&logs).join(format!("task-{i:03}.log"));
            fs::write(&path, vec![b'x'; 16]).expect("write");
            // Space the mtimes so "oldest" is unambiguous.
            let when = base + std::time::Duration::from_secs(i as u64 * 60);
            let stamp = std::time::UNIX_EPOCH + std::time::Duration::from_secs(
                when.duration_since(std::time::UNIX_EPOCH).expect("epoch").as_secs(),
            );
            set_mtime(&path, stamp);
        }

        let (bytes, files) = task_logs_footprint_in(&logs);
        assert_eq!(files, count, "the footprint must see the whole tree");
        assert_eq!(bytes, count as u64 * 16);

        let (removed, reclaimed) = trim_task_logs_in(&logs);
        assert_eq!(removed, 10, "exactly the surplus is removed: {removed}");
        assert_eq!(reclaimed, 10 * 16);

        let (_, after) = task_logs_footprint_in(&logs);
        assert_eq!(after, TASK_LOG_KEEP, "the tree is bounded afterwards: {after}");

        // The newest survived; the oldest did not.
        assert!(
            fs::metadata(std::path::Path::new(&logs).join("task-000.log")).is_err(),
            "the oldest transcript must be the first to go"
        );
        assert!(
            fs::metadata(std::path::Path::new(&logs).join(format!("task-{:03}.log", count - 1)))
                .is_ok(),
            "the newest transcript must survive: it holds the failure worth reading"
        );

        // A second trim is a no-op, not a deletion of live diagnostics.
        let (removed_again, _) = trim_task_logs_in(&logs);
        assert_eq!(removed_again, 0, "a settled tree must be left alone");

        let _ = fs::remove_dir_all(&dir);
    }

    /// One file over the per-file ceiling is truncated, so a single runaway
    /// build cannot hold the entire budget on its own.
    #[test]
    fn one_runaway_transcript_is_capped() {
        let dir = std::env::temp_dir().join(format!(
            "loop-studio-task-cap-{}-{}",
            std::process::id(),
            now_unix()
        ));
        let logs = task_logs_dir_in(&dir.to_string_lossy());
        fs::create_dir_all(&logs).expect("transcript dir");
        let path = std::path::Path::new(&logs).join("one.log");
        // Small enough to be cheap, so the constant is scaled rather than
        // written as a 32 MiB fixture.
        fs::write(&path, vec![b'x'; 4096]).expect("write");

        let (removed, _reclaimed) = trim_task_logs_in(&logs);
        assert_eq!(removed, 0, "a single transcript is never removed by count");
        assert_eq!(
            fs::metadata(&path).expect("still there").len(),
            4096,
            "under the per-file ceiling, nothing is truncated"
        );

        // Over the ceiling: truncate to it, and keep the file.
        let ceiling = TASK_LOG_MAX_BYTES;
        fs::write(&path, vec![b'x'; 1]).expect("rewrite");
        {
            let f = fs::OpenOptions::new().write(true).open(&path).expect("open");
            f.set_len(ceiling + 1).expect("grow");
        }
        trim_task_logs_in(&logs);
        let len = fs::metadata(&path).expect("still there").len();
        assert_eq!(len, ceiling, "an oversized transcript is capped, not deleted");

        let _ = fs::remove_dir_all(&dir);
    }

    /// Set a file's mtime, so "oldest" is a real ordering rather than an
    /// accident of directory iteration order.
    ///
    /// `std` has no mtime setter, so this is the one FFI call in the test
    /// module. Two timestamps are supplied because `utimensat` takes an array
    /// of (atime, mtime).
    #[cfg(unix)]
    fn set_mtime(path: &std::path::Path, when: std::time::SystemTime) {
        let secs = when
            .duration_since(std::time::UNIX_EPOCH)
            .expect("epoch")
            .as_secs() as isize;
        let times = [Timespec { sec: secs, nsec: 0isize }; 2];
        let c = std::ffi::CString::new(path.to_string_lossy().as_bytes()).expect("path cstring");
        unsafe extern "C" {
            fn utimensat(dirfd: i32, path: *const std::ffi::c_char, times: *const Timespec, flags: i32) -> i32;
        }
        // AT_FDCWD, flags 0 (follow symlinks): the path is our own fixture.
        const AT_FDCWD: i32 = -2;
        let r = unsafe { utimensat(AT_FDCWD, c.as_ptr(), times.as_ptr(), 0) };
        assert_eq!(r, 0, "could not set mtime on {path:?}");
    }

    #[cfg(unix)]
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Timespec {
        sec: isize,
        nsec: isize,
    }

    /// The footprint is the number an operator needs: what all of this
    /// project's logs hold together, and how many files that is.
    #[test]
    fn the_footprint_counts_our_logs_and_nothing_else() {
        // The real directory, not a fixture: this is a report, and the rule
        // that matters is which names it counts.
        let (bytes, files) = lac_logs_footprint();
        assert!(
            files <= 64,
            "counting something that is not a log file: {files} files, {bytes} bytes"
        );
        let dir = logs_dir();
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if !is_project_log_name(&name) {
                    continue;
                }
                if let Ok(m) = entry.metadata() {
                    if m.is_file() {
                        assert!(bytes >= m.len(), "undercounted {name}");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod thermal_probe_tests {
    // A `#[test]` that loses its attribute is valid Rust that never runs:
    // the suite goes one test quieter and stays green. Fail the build instead.
    #![deny(dead_code)]

    use super::*;

    /// The pmset invocation itself. `pmset -g thermals` is not a pmset
    /// argument: it answers `Error: unhandled argument thermals` and exits 1
    /// on every host, always. The probe ran that, threw the exit status away,
    /// and every caller received an empty string — which `thermal_state`
    /// reported as "Nominal", the one value that means "the machine is cool
    /// and you may spend headroom". So the TUI's rules engine chose Q8 and a
    /// 16k window, `serve_llama` defaulted to 16k, and `/loop-studio/status` published
    /// a level, on a machine that was actively throttling. `thermal_detail()`
    /// printed blank, so the operator's own check of the reading showed
    /// nothing.
    #[test]
    fn the_probe_uses_an_argument_pmset_accepts() {
        assert_eq!(
            THERMAL_PROBE,
            ["-g", "therm"],
            "pmset spells it `-g therm`; `-g thermals` exits 1 with a usage error"
        );
        // And the fact, checked against the tool rather than against this
        // file's belief about it. `thermals` must be the one that fails, or
        // this test is asserting the wrong direction.
        let rejected = Command::new("pmset").args(["-g", "thermals"]).output();
        if let Ok(o) = rejected {
            assert!(
                !o.status.success(),
                "pmset accepted -g thermals; if that has changed, re-check \
                 which spelling this project should use"
            );
        }
    }

    /// A failed probe is absent, not Nominal. This is the single assertion the
    /// whole fix turns on: every consumer of the reading (the TUI's
    /// `decide_model_and_context`, `thermal_default_context` in serve_llama,
    /// the loop's preflight gate, the worker's thermal backoff) treats a
    /// level as a licence to spend, so an absent reading must be unable to
    /// look like a licence.
    #[test]
    fn a_probe_that_cannot_answer_yields_no_level() {
        assert_eq!(parse_thermal(""), None, "silence is not Nominal");
        assert_eq!(parse_thermal("   \n\t "), None);
        // A nonzero exit never reaches the parser — `thermal_state` checks the
        // status first — and that is where the old code lost the reading. It
        // captured the usage error, found no level word in it, and reported
        // "Nominal". The level-word scan is kept for platforms that *do* name a
        // level, and the usage text of a rejected invocation is not one.
        let rejected = Command::new("pmset").args(["-g", "thermals"]).output();
        if let Ok(o) = rejected {
            let captured = String::from_utf8_lossy(&o.stdout).into_owned();
            if !o.status.success() {
                // This is precisely the text the old probe fed to a function
                // whose every branch but the last said "Nominal".
                assert!(
                    parse_thermal(&captured)
                        .map(|l| l == "Nominal")
                        .unwrap_or(false),
                    "a rejected invocation has no level word, so the old code \
                     reported it as Nominal; this fixture is what it saw"
                );
            }
        }
    }

    /// What a *successful* pmset capture means, in the shapes it actually
    /// takes. On an idle Mac pmset reports no limit in effect, which is a
    /// reading — not an absence — so that case must be Nominal, or every
    /// healthy machine would print `unknown`.
    #[test]
    fn a_successful_capture_is_classified_not_guessed() {
        // Idle: no limit recorded. This is the real output of the probe.
        let idle = "Note: No thermal warning level has been recorded\n\
                    Note: No performance warning level has been recorded\n\
                    Note: No CPU power status has been recorded\n";
        assert_eq!(parse_thermal(idle).as_deref(), Some("Nominal"));

        // A throttling machine reports a CPU speed limit. The old code could
        // not see this at all: the flag was invalid, so the limit was never
        // read, and the answer was "Nominal" on exactly the machines that need
        // a smaller window.
        assert_eq!(
            parse_thermal("CPU_Speed_Limit = 71\n").as_deref(),
            Some("Critical")
        );
        assert_eq!(
            parse_thermal("CPU_Speed_Limit = 80\n").as_deref(),
            Some("Serious")
        );
        assert_eq!(
            parse_thermal("CPU_Speed_Limit = 95\n").as_deref(),
            Some("Fair")
        );
        assert_eq!(
            parse_thermal("CPU_Speed_Limit = 100\n").as_deref(),
            Some("Nominal")
        );

        // Named levels, where a platform reports one. `Overheat` maps to
        // Critical and `Serious` is *not* collapsed into Nominal — that
        // collapse hid the warning that precedes a throttle by minutes.
        assert_eq!(parse_thermal("Fair").as_deref(), Some("Fair"));
        assert_eq!(parse_thermal("Serious").as_deref(), Some("Serious"));
        assert_eq!(parse_thermal("Critical").as_deref(), Some("Critical"));
        assert_eq!(parse_thermal("Overheat").as_deref(), Some("Critical"));

        // A named level wins over a limit line, whichever order they appear.
        assert_eq!(
            parse_thermal("CPU_Speed_Limit = 100\nSerious\n").as_deref(),
            Some("Serious")
        );
    }

    /// The limit parser, on the values pmset can emit, so the classification
    /// above rests on a number rather than on a substring.
    #[test]
    fn a_speed_limit_is_read_as_a_number_not_a_word() {
        assert_eq!(cpu_speed_limit("CPU_Speed_Limit = 71"), Some(71.0));
        assert_eq!(cpu_speed_limit("CPU_Speed_Limit=71"), Some(71.0));
        assert_eq!(cpu_speed_limit("CPU_Speed_Limit = 71\n"), Some(71.0));
        assert_eq!(cpu_speed_limit("x CPU_Speed_Limit = 71.5 y"), Some(71.5));
        // Absent, or not a limit at all.
        assert_eq!(cpu_speed_limit("No thermal warning level has been recorded"), None);
        assert_eq!(cpu_speed_limit("CPU_Speed_Limit = "), None);
        assert_eq!(cpu_speed_limit("CPU_Speed_Limit = fast"), None);
        // A negative or infinite "limit" is not a measurement; it must not
        // become a number the rules engine buckets.
        assert_eq!(cpu_speed_limit("CPU_Speed_Limit = -1"), None);
        assert_eq!(cpu_speed_limit("CPU_Speed_Limit = inf"), None);
    }

    /// Every level a decision can be made on is a level the router can publish
    /// and a parser can read back. `thermal_label` is the one place an absent
    /// reading is turned into a word, so it is asserted there rather than left
    /// to each call site.
    #[test]
    fn an_absent_reading_is_named_and_never_filled_in() {
        assert_eq!(thermal_label(&None), "unknown");
        for level in ["Nominal", "Fair", "Serious", "Critical"] {
            let reading = Some(level.to_string());
            assert_eq!(thermal_label(&reading), level);
        }
        // "unknown" is not any of the levels, so a `== "Nominal"` style test
        // downstream cannot accidentally pass on it.
        assert_ne!(thermal_label(&None), "Nominal");
    }
}
