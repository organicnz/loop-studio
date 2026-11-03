//! Hermes orchestrator: judgment-aware task selection.

use std::collections::HashSet;
use std::env;

use super::MAX_TASK_ATTEMPTS;
use super::chat::{extract_json_string, post_chat};
use super::common;
use super::ops::opencode_model;
use super::queue::{Task, pending_candidates};

pub fn count_pending(content: &str) -> (usize, usize) {
    let mut p = 0;
    let mut d = 0;
    for t in super::queue::parse_tasks(content) {
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

pub fn judge_prompt(candidates: &[Task], thermal: Option<&str>) -> String {
    let mut s = String::from(
        "You are the Hermes task selector for a local autonomous coding worker. Pick the single most urgent task to run next.\n",
    );
    s.push_str(&format!(
        "Thermal state: {}. ",
        thermal.unwrap_or("unknown")
    ));
    s.push_str("Critical means prefer tiny safe tasks; otherwise pick by urgency described in each task.\nCandidates:\n");
    for t in candidates {
        let short: String = t.desc.chars().take(200).collect();
        s.push_str(&format!(
            "- ID: {} | attempts: {} | task: {}\n",
            t.id, t.attempts, short
        ));
    }
    s.push_str("Reply with exactly two lines, no extra text:\nTASK_ID: <one of the IDs above>\nREASON: <one short line>\n");
    s
}

pub fn parse_judge_decision(output: &str, candidates: &[Task]) -> Option<Task> {
    for line in output.lines() {
        let t = line.trim();
        let rest = match t.strip_prefix("TASK_ID:") {
            Some(r) => r,
            None => continue,
        };
        let mut id = rest.trim().trim_matches('"').trim_matches('\'').trim();
        if let Some(tok) = id.split_whitespace().next() {
            id = tok.trim_matches('"').trim_matches('\'');
        }
        if id.is_empty() {
            continue;
        }
        if let Some(hit) = candidates.iter().find(|c| c.id == id) {
            return Some(hit.clone());
        }
    }
    None
}

pub fn judge_select(candidates: Vec<Task>, judged: Option<Task>) -> Option<Task> {
    match judged {
        Some(t) if candidates.iter().any(|c| c.id == t.id) => Some(t),
        _ => candidates.into_iter().next(),
    }
}

/// Single-threaded selection entry point, kept for the existing tests and for
/// callers that do not share a claim set.
#[cfg_attr(not(test), allow(dead_code))]
pub fn judge_next(content: &str, thermal: Option<&str>, root: &str) -> Option<Task> {
    judge_candidates(pending_candidates(content), thermal, root)
}

/// As [`judge_next`], but ignores tasks another worker thread has already
/// claimed. Parallel workers share one queue file, so without the exclusion
/// every thread would select the same first pending task.
pub fn judge_next_excluding(
    content: &str,
    thermal: Option<&str>,
    root: &str,
    claimed: &HashSet<String>,
) -> Option<Task> {
    let candidates = pending_candidates(content)
        .into_iter()
        .filter(|task| !claimed.contains(&task.id))
        .collect();
    judge_candidates(candidates, thermal, root)
}

fn judge_candidates(candidates: Vec<Task>, thermal: Option<&str>, root: &str) -> Option<Task> {
    if candidates.len() <= 1 {
        return candidates.into_iter().next();
    }
    if env::var("LOOP_STUDIO_JUDGE")
        .map(|v| v == "0" || v.eq_ignore_ascii_case("off") || v.eq_ignore_ascii_case("false"))
        .unwrap_or(false)
    {
        return candidates.into_iter().next();
    }
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

pub fn cmd_hermes(root: &str, args: &[String]) {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("status");
    match sub {
        "status" => {
            println!("=== Hermes Orchestrator (native) ===");
            let home = common::home_dir();
            let tasks_file = common::tasks_file();
            let content = std::fs::read_to_string(&tasks_file).unwrap_or_default();
            let (pending, dead) = count_pending(&content);
            let total = super::queue::parse_tasks(&content).len();
            println!(
                "  Queue   : {} ({} total, {} retryable pending, {} dead-letter)",
                tasks_file, total, pending, dead
            );
            println!(
                "  Gateway : http://127.0.0.1:{}/v1 ({})",
                common::gateway_port(),
                if common::port_up(common::gateway_port()) {
                    "online"
                } else {
                    "offline"
                }
            );
            println!(
                "  Thermal : {}",
                common::thermal_label(&common::thermal_state())
            );
            println!("  Model   : {}", opencode_model(root));
            println!("  Loops   : {}/todo/loop-studio-loops", home);
            println!(
                "  Worker  : loop-studio worker [--drain] (judgment-augmented selection; deterministic fallback)"
            );
        }
        "run" | "drain" | "worker" => {
            let rest: Vec<String> = args.iter().skip(1).cloned().collect();
            super::worker::cmd_worker(root, &rest);
        }
        _ => println!("Usage: loop-studio hermes [status|run [--drain|--once]]"),
    }
}
