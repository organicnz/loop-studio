//! Git repository probes — branch, commit existence, dirtiness.

use std::process::Command;

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

#[cfg(test)]
mod tests {
    #![deny(dead_code)]
    use super::*;

    #[test]
    fn default_branch_never_empty() {
        assert!(!default_branch().is_empty());
    }
}
