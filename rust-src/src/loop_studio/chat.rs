//! Chat and code assistant commands.

use std::env;
use std::fs;

use super::common;

pub const NO_BACKEND_HINT: &str =
    "No Loop Studio inference backend is serving. Start one with `loop-studio serve mlx`.";

/// Escape-aware extraction of a top-level string field `"key":"..."`.
pub fn extract_json_string(body: &str, key: &str) -> Option<String> {
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

/// Blocking non-streaming chat POST through the gateway.
pub fn post_chat(body: &str) -> Option<(u16, String)> {
    let secs = env::var("LOOP_STUDIO_CHAT_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(300);
    common::http_post(common::gateway_port(), "/v1/chat/completions", body, secs * 1000)
}

pub fn cmd_chat(root: &str, args: &[String]) {
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
        .unwrap_or_else(|| super::ops::opencode_model(root));
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

pub fn extract_fenced_code(text: &str) -> Option<String> {
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

pub fn print_terminal_diff(original: &str, modified: &str, file_name: &str) {
    let orig_lines: Vec<&str> = original.lines().collect();
    let mod_lines: Vec<&str> = modified.lines().collect();
    println!("\x1B[1m--- a/{}\x1B[0m", file_name);
    println!("\x1B[1m+++ b/{}\x1B[0m", file_name);

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

pub fn cmd_code(root: &str, args: &[String]) {
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
        .unwrap_or_else(|| super::ops::opencode_model(root));

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

// end of module
