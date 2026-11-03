//! Process inspection and signalling — ps snapshot, program matching, kill tree.

use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

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

pub fn is_executable(p: &Path) -> bool {
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
                format!(
                    "is not executable (mode {:o})",
                    m.permissions().mode() & 0o7777
                )
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
    // case the TUI's second engine sweep exists for: no wrapper owns it.
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

/// Full command line of one pid, or None when it is gone.
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

/// Processes running `pattern`, excluding self.
pub fn processes_matching(substr: &str) -> Vec<ProcInfo> {
    processes_matching_in(&process_snapshot(), substr)
}

/// [`processes_matching`] over a supplied snapshot.
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

/// Signal a pid via `kill`.
pub fn signal_pid(pid: u32, sig: &str) -> bool {
    Command::new("kill")
        .args([format!("-{}", sig), pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// SIGTERM every process running `pattern`. Returns kill count.
pub fn kill_matching(substr: &str) -> usize {
    processes_matching(substr)
        .iter()
        .filter(|p| signal_pid(p.pid, "TERM"))
        .count()
}

/// SIGKILL a process tree, children first (recursive).
pub fn kill_tree(pid: u32) {
    for child in children_of(pid) {
        kill_tree(child);
    }
    signal_pid(pid, "9");
}

#[cfg(test)]
mod tests {
    #![deny(dead_code)]
    use super::super::now_unix;
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn which_finds_shell_and_rejects_ghosts() {
        let sh = which("sh");
        assert!(sh.is_some(), "sh must resolve via PATH");
        assert!(which("definitely-not-a-binary-xyz").is_none());
        assert!(which("/nonexistent/path/xyz").is_none());
    }

    #[test]
    fn ps_line_parsing() {
        let p = parse_ps_line("  123   456 /usr/bin/foo --bar baz").unwrap();
        assert_eq!(
            p,
            ProcInfo {
                pid: 123,
                ppid: 456,
                cmd: "/usr/bin/foo --bar baz".to_string()
            }
        );
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
                    assert!(
                        start.elapsed() < Duration::from_secs(10),
                        "child survived kill_tree"
                    );
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => panic!("try_wait: {}", e),
            }
        }
    }

    #[test]
    fn a_signal_only_ever_reaches_the_program_it_names() {
        let bystanders = [
            (
                "/usr/bin/tail -f /Users/x/Library/Logs/lac-serve-mlx.err",
                "serve-mlx",
            ),
            (
                "/usr/bin/less /Users/x/Library/Logs/lac-serve-llama.log",
                "serve-llama",
            ),
            (
                "/usr/bin/grep -r loop-studio-router /Users/x/dev",
                "loop-studio-router",
            ),
            (
                "/usr/bin/git diff -- rust-src/target/release/serve-mlx",
                "serve-mlx",
            ),
            (
                "/bin/zsh -c sleep 300; : loop-studio-router",
                "loop-studio-router",
            ),
            (
                "/Applications/Codex.app/Contents/MacOS/Codex run restart serve-mlx",
                "serve-mlx",
            ),
            (
                "/usr/bin/vim notes # loop-studio-router",
                "loop-studio-router",
            ),
            (
                "/usr/bin/open -a TextEdit ~/notes/loop-studio-router.md",
                "loop-studio-router",
            ),
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
            (
                "/Users/x/.local/bin/loop-studio-router",
                "loop-studio-router",
                true,
            ),
            (
                "./target/release/loop-studio-router",
                "loop-studio-router",
                true,
            ),
            ("/Users/x/.local/bin/lac-serve-mlx", "serve-mlx", true),
            (
                "/Users/x/.local/bin/loop-studio-serve-mlx",
                "serve-mlx",
                true,
            ),
            (
                "/Users/x/.local/bin/loop-studio-serve-llama",
                "serve-llama",
                true,
            ),
            ("/Users/x/.local/bin/serve-llama", "serve-llama", true),
            (
                "/opt/homebrew/bin/llama-server --port 8081 -m q8.gguf",
                "llama-server",
                true,
            ),
            (
                "/Users/x/.venv/bin/mlx_lm.server --model m",
                "mlx_lm.server",
                true,
            ),
            (
                "/usr/bin/python3 -m mlx_lm.server --model m",
                "mlx_lm.server",
                true,
            ),
            ("/usr/local/bin/ollama serve", "ollama serve", true),
            ("/usr/local/bin/ollama runner x", "ollama serve", false),
            (
                "/Users/x/.local/bin/loop-studio-router (deleted)",
                "loop-studio-router",
                true,
            ),
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
        for pattern in [
            "loop-studio-router",
            "serve-mlx",
            "llama-server",
            "mlx_lm.server",
        ] {
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
        let snapshot: Vec<ProcInfo> = [
            "/Users/x/.local/bin/loop-studio-router",
            "/usr/bin/tail -f /Users/x/Library/Logs/loop-studio-router.log",
            "/bin/zsh -c loop-studio stop",
            "/Users/x/.local/bin/loop-studio-tui",
            "/usr/bin/grep -rn loop-studio-router rust-src",
        ]
        .iter()
        .enumerate()
        .map(|(index, cmd)| ProcInfo {
            pid: 1000 + index as u32,
            ppid: 1,
            cmd: cmd.to_string(),
        })
        .collect();
        let hits: Vec<String> = processes_matching_in(&snapshot, "loop-studio-router")
            .iter()
            .map(|p| p.cmd.clone())
            .collect();
        assert_eq!(
            hits,
            vec!["/Users/x/.local/bin/loop-studio-router".to_string()]
        );
        let me = std::process::id();
        let mine = vec![ProcInfo {
            pid: me,
            ppid: 1,
            cmd: "loop-studio-router".to_string(),
        }];
        assert!(processes_matching_in(&mine, "loop-studio-router").is_empty());
    }

    #[test]
    fn a_live_pid_reports_its_own_command_line() {
        let me = std::process::id();
        let command = command_of(me).expect("own pid is visible to ps");
        assert!(!command.is_empty());
        assert!(
            command.contains("lac_bootstrap")
                || command.contains("common")
                || command.contains("-"),
            "unexpected own command line: {command}"
        );
        assert!(command_of(u32::MAX).is_none());
    }

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

        let real_release = dir.join("loop-studio-router-release");
        fs::write(&real_release, b"#!/bin/sh\n").expect("release build");
        fs::set_permissions(&real_release, fs::Permissions::from_mode(0o755)).expect("chmod");
        let real_release = real_release.to_string_lossy().into_owned();
        let picked = choose_program_binary(&installed.to_string_lossy(), &real_release);
        assert_eq!(picked.kind, ProgramKind::Release);
        assert_eq!(picked.path, real_release);
        assert_eq!(picked.warning, None);

        let picked = choose_program_binary(&installed.to_string_lossy(), release);
        assert_eq!(picked.kind, ProgramKind::Missing);
        assert!(
            picked.path.is_empty(),
            "no program may be invented: {:?}",
            picked.path
        );
        let warning = picked
            .warning
            .expect("the operator is told why nothing was placed");
        assert!(
            warning.contains("no runnable Loop Studio binary"),
            "{warning}"
        );
        assert!(warning.contains(release), "{warning}");

        fs::write(&installed, b"").expect("truncated binary");
        let picked = choose_program_binary(&installed.to_string_lossy(), &real_release);
        assert_eq!(picked.kind, ProgramKind::InstalledUnusable);
        assert_eq!(picked.path, real_release);
        assert_ne!(picked.path, installed.to_string_lossy());
        let warning = picked.warning.expect("the operator is told");
        assert!(warning.contains("not executable"), "{warning}");
        assert!(warning.contains(&real_release), "{warning}");

        fs::set_permissions(&installed, fs::Permissions::from_mode(0o755)).expect("chmod");
        let picked = choose_program_binary(&installed.to_string_lossy(), &real_release);
        assert_eq!(picked.kind, ProgramKind::Installed);
        assert_eq!(picked.path, installed.to_string_lossy());
        assert_eq!(picked.warning, None);

        fs::set_permissions(&installed, fs::Permissions::from_mode(0o644)).expect("chmod -x");
        let picked = choose_program_binary(&installed.to_string_lossy(), &real_release);
        assert_eq!(picked.kind, ProgramKind::InstalledUnusable);
        assert_eq!(picked.path, real_release);

        let as_dir = dir.join("loop-studio");
        fs::create_dir_all(&as_dir).expect("mkdir");
        let picked = choose_program_binary(&as_dir.to_string_lossy(), &real_release);
        assert_eq!(picked.kind, ProgramKind::InstalledUnusable);
        assert_eq!(picked.path, real_release);
        assert!(picked.warning.unwrap().contains("is a directory"));

        let _ = fs::remove_dir_all(&dir);
    }
}
