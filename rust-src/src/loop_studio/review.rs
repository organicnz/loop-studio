//! Review gate: implement → review → apply, max 2 rounds per AGENTS.md.

use std::env;
use std::fs;
use std::io::Write;
use std::process::Command;

use super::chat::{extract_json_string, post_chat};
use super::common;
use super::ops::opencode_model;

/// Max review rounds per task.
pub const MAX_REVIEW_ROUNDS: u32 = 2;
/// Diff bytes fed to the reviewer prompt.
pub const REVIEW_DIFF_LIMIT: usize = 12_000;
/// Per-file cap for untracked-file contents included in the review.
const REVIEW_FILE_LIMIT: usize = 2_000;
/// Max untracked files swept into the review prompt.
const REVIEW_FILES_MAX: usize = 10;

pub fn review_enabled() -> bool {
    !env::var("LOOP_STUDIO_REVIEW")
        .map(|v| v == "0" || v.eq_ignore_ascii_case("off") || v.eq_ignore_ascii_case("false"))
        .unwrap_or(false)
}

/// Working-tree diff vs HEAD plus capped contents of small text untracked files.
pub fn worker_diff(root: &str) -> String {
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

pub fn review_prompt(task_desc: &str, diff: &str) -> String {
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

/// Critical/major finding lines.
pub fn parse_review_findings(output: &str) -> Vec<String> {
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

pub fn review_disposition(round: u32, has_findings: bool) -> &'static str {
    if !has_findings {
        "commit"
    } else if round < MAX_REVIEW_ROUNDS {
        "fix"
    } else {
        "commit-flagged"
    }
}

pub enum ReviewOutcome {
    Approve,
    Fix(Vec<String>),
    Unavailable(String),
}

pub fn review_diff(root: &str, task_desc: &str, logpath: &str) -> ReviewOutcome {
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
