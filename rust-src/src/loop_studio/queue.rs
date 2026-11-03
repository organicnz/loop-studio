//! Task queue: parse, update, and sanitize the YAML task queue file.

use std::cmp::Reverse;
use std::fs;

use super::common;
use super::MAX_TASK_ATTEMPTS;

#[derive(Debug, Clone)]
pub struct Task {
    pub id: String,
    pub desc: String,
    pub status: String,
    pub attempts: u32,
}

pub fn line_indent(s: &str) -> usize {
    s.chars().take_while(|c| *c == ' ' || *c == '\t').count()
}

pub fn block_key_indent(block: &str) -> usize {
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
pub fn unquote(value: &str) -> String {
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

pub fn block_field(block: &str, key: &str) -> Option<String> {
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

/// Allowlist for task ids. Ids flow into a transcript path, a git branch,
/// and a commit message — all from YAML the worker does not fully control.
/// Only `[A-Za-z0-9][A-Za-z0-9._-]{0,64}` passes.
pub fn sanitize_task_id(id: &str) -> Option<String> {
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
/// path, branch, or commit message.
pub fn parse_tasks(content: &str) -> Vec<Task> {
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
/// bump attempts, inserting whichever key is absent.
pub fn update_task(content: &str, id: &str, new_status: &str, bump_attempts: bool) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let mut starts: Vec<usize> = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        if l.trim_start().starts_with("- id:") {
            starts.push(i);
        }
    }
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
                    let at = status_idx.map(|i| i + 1).unwrap_or(begin + 2);
                    inserts.push((at, format!("{}attempts: {}", indent, cur + 1)));
                }
            }
        }
    }
    inserts.sort_by_key(|a| Reverse(a.0));
    for (at, text) in inserts {
        out.insert(at.min(out.len()), text);
    }
    let mut s = out.join("\n");
    if content.ends_with('\n') {
        s.push('\n');
    }
    s
}

pub fn update_task_file(path: &str, id: &str, status: &str, bump_attempts: bool) -> bool {
    let Some(_lock) = common::acquire_lock("loop-studio-tasks", 30) else {
        eprintln!("warning: queue lock busy; update deferred for {}", id);
        return false;
    };
    let Ok(content) = fs::read_to_string(path) else {
        return false;
    };
    common::atomic_write(path, &update_task(&content, id, status, bump_attempts)).is_ok()
}

/// Deterministic fallback order: first retryable pending task in file order.
#[allow(dead_code)]
pub fn next_pending(content: &str) -> Option<Task> {
    parse_tasks(content)
        .into_iter()
        .find(|t| t.status == "pending" && t.attempts < MAX_TASK_ATTEMPTS)
}

/// Retryable candidates in file order.
pub fn pending_candidates(content: &str) -> Vec<Task> {
    parse_tasks(content)
        .into_iter()
        .filter(|t| t.status == "pending" && t.attempts < MAX_TASK_ATTEMPTS)
        .collect()
}
