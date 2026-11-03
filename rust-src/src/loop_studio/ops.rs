//! Operational commands: status, doctor, stop, ps, logs, config, cap, thermal, visualize, dashboard.

use std::env;
use std::fs;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;

use super::common;
use super::VERSION;

// ------------------------------------------------------------------ helpers -

/// Best-effort model id from opencode.jsonc.
pub fn opencode_model(root: &str) -> String {
    fs::read_to_string(format!("{}/opencode.jsonc", root))
        .ok()
        .and_then(|t| common::jsonc_model_id(&t))
        .unwrap_or_else(|| "loop-studio/qwen3.8-27b".to_string())
}

/// Active backend according to the router (best-effort, empty when down).
fn router_active() -> String {
    common::http_get(common::gateway_port(), "/loop-studio/status", 1200)
        .filter(|(code, _)| *code == 200)
        .and_then(|(_, body)| {
            let p = body.find("\"active\"")?;
            let rest = &body[p + 8..];
            let colon = rest.find(':')?;
            let after = rest[colon + 1..].trim_start();
            let q1 = after.find('"')?;
            let tail = &after[q1 + 1..];
            tail.find('"').map(|q2| tail[..q2].to_string())
        })
        .filter(|active| !active.is_empty() && active != "none")
        .unwrap_or_default()
}

pub fn parse_router_host_facts(body: &str) -> Option<(Option<f64>, Option<f64>, Option<String>)> {
    let p = body.find("\"host\"")?;
    let rest = &body[p + "\"host".len()..];
    let brace = rest.find('{')?;
    let after = &rest[brace + 1..];
    let end = after.find('}')?;
    let block = &after[..end];
    let thermal = json_string(block, "\"thermal\"");
    let free = json_f64(block, "\"free_ram_gib\"");
    let total = json_f64(block, "\"total_ram_gib\"");
    Some((free, total, thermal))
}

pub fn json_string(block: &str, key: &str) -> Option<String> {
    let p = block.find(key)?;
    let rest = &block[p + key.len()..];
    let colon = rest.find(':')?;
    let after = rest[colon + 1..].trim_start();
    if after.starts_with("null") {
        return None;
    }
    let q1 = after.find('"')?;
    let tail = &after[q1 + 1..];
    let q2 = tail.find('"')?;
    let value = &tail[..q2];
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

pub fn json_f64(block: &str, key: &str) -> Option<f64> {
    let p = block.find(key)?;
    let rest = &block[p + key.len()..];
    let colon = rest.find(':')?;
    let after = rest[colon + 1..].trim_start();
    if after.starts_with("null") {
        return None;
    }
    let end = after
        .char_indices()
        .find(|(_, c)| !(c.is_ascii_digit() || *c == '.' || *c == '-'))
        .map(|(i, _)| i)
        .unwrap_or(after.len());
    after[..end].parse().ok()
}

fn router_host_facts() -> Option<(Option<f64>, Option<f64>, Option<String>)> {
    common::http_get(common::gateway_port(), "/loop-studio/status", 1200)
        .filter(|(code, _)| *code == 200)
        .and_then(|(_, body): (u16, String)| parse_router_host_facts(&body))
}

// ------------------------------------------------------------------ status -

pub fn cmd_status(root: &str, args: &[String]) {
    let json = args.iter().any(|a| a == "--json");
    let gw = common::port_up(common::gateway_port());
    let mlx_port = common::mlx_port();
    let mlx = common::port_up(mlx_port);
    let llama = common::port_up(common::llama_port());
    let ollama = common::port_up(common::ollama_port());
    let (ram, total, thermal) = if gw {
        match router_host_facts() {
            Some((free, tot, therm)) => (free, tot, therm),
            None => (
                common::free_ram_gib(),
                common::total_ram_gib(),
                common::thermal_state(),
            ),
        }
    } else {
        (
            common::free_ram_gib(),
            common::total_ram_gib(),
            common::thermal_state(),
        )
    };
    let model = opencode_model(root);
    let active = if gw { router_active() } else { String::new() };

    if json {
        println!(
            "{{\"model\":\"{}\",\"thermal\":{},\"free_ram_gib\":{},\"total_ram_gib\":{},\"gateway\":{},\"mlx\":{},\"mlx_port\":{},\"llama\":{},\"ollama\":{},\"active\":\"{}\"}}",
            common::json_escape(&model),
            thermal
                .as_deref()
                .map(|t| format!("\"{}\"", common::json_escape(t)))
                .unwrap_or_else(|| "null".to_string()),
            ram.map(|f| format!("{:.1}", f)).unwrap_or_else(|| "null".to_string()),
            total.map(|f| format!("{:.1}", f)).unwrap_or_else(|| "null".to_string()),
            gw, mlx, mlx_port, llama, ollama,
            common::json_escape(&active),
        );
        return;
    }

    let ram_s = ram
        .map(|f| format!("{:.1} GiB", f))
        .unwrap_or_else(|| "Unknown".to_string());
    println!("================================================================");
    println!(
        "  Loop Studio STACK TELEMETRY — {} (v{})",
        common::arch_label(),
        VERSION
    );
    println!("================================================================");
    println!("  Primary Model  : {}", model);
    println!(
        "  Free RAM       : {}",
        total
            .map(|t| format!("{} (of {:.0} GiB)", ram_s, t))
            .unwrap_or_else(|| ram_s.clone())
    );
    println!("  Thermal State  : {}", common::thermal_label(&thermal));
    if !active.is_empty() {
        println!("  Router Active  : {}", active);
    }
    println!("----------------------------------------------------------------");
    println!("  INFERENCE SERVICES (TCP probe):");
    println!(
        "  - Loop Studio Gateway  : {}",
        if gw {
            format!("ONLINE (:{})", common::gateway_port())
        } else {
            "OFFLINE".to_string()
        }
    );
    if mlx {
        println!("  - MLX (Q4 MTP) : ONLINE (:{})", mlx_port);
    } else if !common::mlx_supported() {
        println!("  - MLX (Q4 MTP) : N/A (Apple Silicon only)");
    } else {
        println!("  - MLX (Q4 MTP) : OFFLINE");
    }
    println!(
        "  - llama (Q8)   : {}",
        if llama {
            format!("ONLINE (:{})", common::llama_port())
        } else {
            "OFFLINE".to_string()
        }
    );
    println!(
        "  - Ollama       : {}",
        if ollama {
            format!("ONLINE (:{})", common::ollama_port())
        } else {
            "OFFLINE".to_string()
        }
    );
    println!("================================================================");

    if !gw && !mlx && !llama && !ollama {
        println!("\nTip: No servers are running. Start one with:");
        if common::mlx_supported() {
            println!("   loop-studio serve mlx    (Speed Q4, native MTP ~45 tok/s)");
        }
        println!("   loop-studio serve llama  (Quality Q8_0)");
        println!(
            "   loop-studio route        (Start gateway on :{})",
            common::gateway_port()
        );
    }
}

// ------------------------------------------------------------------ doctor -

pub struct Check {
    pub name: &'static str,
    pub level: &'static str, // ok | warn | fail
    pub detail: String,
}

/// Pure memory check scaled by total physical RAM.
pub fn doctor_memory_check_with(
    total_gib: Option<f64>,
    free_gib: Option<f64>,
) -> (&'static str, String) {
    let free = match free_gib {
        Some(f) => f,
        None => return ("warn", "vm_stat unavailable".to_string()),
    };
    let (red, yellow) = match total_gib {
        Some(total) => {
            let r = (total * 0.04).clamp(1.5, 4.0);
            let y = (total * 0.12).clamp(3.0, 12.0);
            (r, y)
        }
        None => (4.0, 16.0),
    };
    if free >= yellow {
        ("ok", format!("{:.1} GiB free (healthy)", free))
    } else if free >= red {
        ("warn", format!("{:.1} GiB free (moderate pressure)", free))
    } else {
        ("fail", format!("{:.1} GiB free (high OOM risk)", free))
    }
}

pub fn disk_free_gib(path: &str) -> Option<f64> {
    let out = Command::new("df").arg("-k").arg(path).output().ok()?;
    let txt = String::from_utf8_lossy(&out.stdout);
    let line = txt.lines().nth(1)?;
    let avail: f64 = line.split_whitespace().nth(3)?.parse().ok()?;
    Some(avail / 1048576.0)
}

pub fn launchd_loaded() -> (bool, bool) {
    let out = Command::new("launchctl")
        .arg("list")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    (
        out.lines().any(|l| l.contains("org.loopstudio.router")),
        out.lines().any(|l| l.contains("org.loopstudio.worker")),
    )
}

pub fn count_files(dir: &str, ext: &str) -> usize {
    fs::read_dir(dir)
        .map(|it| {
            it.flatten()
                .filter(|e| e.path().extension().map(|x| x == ext).unwrap_or(false))
                .count()
        })
        .unwrap_or(0)
}

pub fn doctor_logs_check(bytes: u64, files: usize, task_bytes: u64, task_files: usize) -> Check {
    let mib = bytes as f64 / (1024.0 * 1024.0);
    let over = bytes > common::LOG_MAX_BYTES * 8;
    let task_mib = task_bytes as f64 / (1024.0 * 1024.0);
    let task_over = task_bytes > (common::TASK_LOG_MAX_BYTES * common::TASK_LOG_KEEP as u64);
    let task_note = if task_over {
        format!(
            "; agent transcripts {task_mib:.1} MiB across {task_files} file(s) in ~/.loop-studio/task-logs — reclaim with `loop-studio logs tasks --purge`"
        )
    } else if task_files > 0 {
        format!(
            "; agent transcripts {task_mib:.1} MiB across {task_files} file(s) in ~/.loop-studio/task-logs"
        )
    } else {
        String::new()
    };
    Check {
        name: "logs",
        level: if over || task_over { "warn" } else { "ok" },
        detail: if over {
            format!(
                "{mib:.1} MiB across {files} file(s) in ~/Library/Logs — reclaim with `loop-studio logs <router|mlx|llama|worker> --purge`{task_note}"
            )
        } else {
            format!("{mib:.1} MiB across {files} file(s) in ~/Library/Logs{task_note}")
        },
    }
}

pub fn cmd_doctor(root: &str, args: &[String]) -> i32 {
    let json = args.iter().any(|a| a == "--json");
    let mut checks: Vec<Check> = Vec::new();

    checks.push(Check {
        name: "os",
        level: "ok",
        detail: common::os_descr(),
    });

    {
        let (bytes, files) = common::lac_logs_footprint();
        let (task_bytes, task_files) = common::task_logs_footprint();
        checks.push(doctor_logs_check(bytes, files, task_bytes, task_files));
    }

    let (mem_level, mem_detail) =
        doctor_memory_check_with(common::total_ram_gib(), common::free_ram_gib());
    checks.push(Check {
        name: "memory",
        level: mem_level,
        detail: mem_detail,
    });

    if common::volume_mounted("/Volumes/AIModels") {
        let df = disk_free_gib("/Volumes/AIModels")
            .map(|f| format!("{:.0} GiB free", f))
            .unwrap_or_else(|| "df unavailable".to_string());
        checks.push(Check {
            name: "model_volume",
            level: "ok",
            detail: format!("/Volumes/AIModels mounted ({})", df),
        });
    } else {
        checks.push(Check {
            name: "model_volume",
            level: "warn",
            detail: "/Volumes/AIModels not mounted; internal fallback ~/.loop-studio/models"
                .to_string(),
        });
    }
    if let Some(df) = disk_free_gib(&env::var("HOME").unwrap_or_else(|_| ".".to_string())) {
        checks.push(Check {
            name: "disk_home",
            level: if df > 20.0 { "ok" } else { "warn" },
            detail: format!("{:.0} GiB free on $HOME volume", df),
        });
    }

    let ollama_models = format!("{}/.ollama/models", common::home_dir());
    match fs::symlink_metadata(&ollama_models) {
        Ok(m) if m.file_type().is_symlink() => {
            let target_ok = fs::read_link(&ollama_models)
                .ok()
                .map(|t| t.exists())
                .unwrap_or(false);
            checks.push(Check {
                name: "ollama_symlink",
                level: if target_ok { "ok" } else { "fail" },
                detail: if target_ok {
                    "points at live model library".to_string()
                } else {
                    "dangling symlink — rerun loop-studio bootstrap".to_string()
                },
            });
        }
        Ok(_) => checks.push(Check {
            name: "ollama_symlink",
            level: "warn",
            detail: "real directory, not symlinked to model volume".to_string(),
        }),
        Err(_) => checks.push(Check {
            name: "ollama_symlink",
            level: "warn",
            detail: "absent (Ollama not set up yet)".to_string(),
        }),
    }

    for (cmd, desc, check) in [
        ("opencode2", "OpenCode V2 harness", "tool_opencode2"),
        ("ollama", "Ollama server", "tool_ollama"),
        ("uv", "Python package manager", "tool_uv"),
        ("rustc", "Rust compiler", "tool_rustc"),
        ("jj", "Jujutsu VCS", "tool_jj"),
    ] {
        let found = common::which(cmd);
        checks.push(Check {
            name: check,
            level: if found.is_some() { "ok" } else { "warn" },
            detail: format!(
                "{} ({})",
                found.unwrap_or_else(|| "missing".to_string()),
                desc
            ),
        });
    }

    for (name, rel) in [
        ("opencode_config", "opencode.jsonc"),
        ("agent_rules", "docs/AGENTS.md"),
    ] {
        let p = format!("{}/{}", root, rel);
        let fallback = format!("{}/AGENTS.md", root);
        let ok = fs::metadata(&p).is_ok() || fs::metadata(&fallback).is_ok();
        checks.push(Check {
            name,
            level: if ok { "ok" } else { "fail" },
            detail: p,
        });
    }

    let loops = count_files(&format!("{}/templates/loops", root), "yaml");
    checks.push(Check {
        name: "loop_templates",
        level: if loops >= 4 { "ok" } else { "warn" },
        detail: format!("{} loop templates", loops),
    });
    let skills = fs::read_dir(format!("{}/.opencode/skills", root))
        .map(|it| it.count())
        .unwrap_or(0);
    checks.push(Check {
        name: "skills",
        level: if skills >= 10 { "ok" } else { "warn" },
        detail: format!("{} skills installed", skills),
    });

    let gw_up = common::port_up(common::gateway_port());
    let mut backends: Vec<(&'static str, u16)> = Vec::new();
    if common::mlx_supported() {
        backends.push(("mlx", common::mlx_port()));
    }
    backends.push(("llama", common::llama_port()));
    backends.push(("ollama", common::ollama_port()));
    let mut any_backend = false;
    for (name, port) in backends {
        let up = common::http_ready(port, 1200);
        any_backend |= up;
        let lvl = if up { "ok" } else { "warn" };
        checks.push(Check {
            name: match name {
                "mlx" => "svc_mlx",
                "llama" => "svc_llama",
                _ => "svc_ollama",
            },
            level: lvl,
            detail: format!("port {} {}", port, if up { "serving" } else { "down" }),
        });
    }
    checks.push(Check {
        name: "svc_gateway",
        level: if gw_up { "ok" } else { "warn" },
        detail: if gw_up {
            format!("gateway :{} accepting", common::gateway_port())
        } else {
            format!(
                "gateway :{} down (start with loop-studio route --daemon)",
                common::gateway_port()
            )
        },
    });
    if !any_backend {
        let hint = if common::mlx_supported() {
            "no inference backend serving; run loop-studio serve mlx"
        } else {
            "no inference backend serving; run loop-studio serve llama"
        };
        checks.push(Check {
            name: "backends",
            level: "fail",
            detail: hint.to_string(),
        });
    }

    let (router_ld, worker_ld) = launchd_loaded();
    checks.push(Check {
        name: "launchd",
        level: "ok",
        detail: format!(
            "router={} worker={}",
            if router_ld { "loaded" } else { "not loaded" },
            if worker_ld { "loaded" } else { "not loaded" }
        ),
    });

    {
        let bind = env::var("LOOP_STUDIO_BIND_ADDR")
            .or_else(|_| env::var("LOOP_STUDIO_ROUTER_HOST"))
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "127.0.0.1".to_string());
        let loopback = common::is_loopback_host(&bind);
        let token_set = env::var("LOOP_STUDIO_API_TOKEN")
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        if !loopback && !token_set {
            checks.push(Check {
                name: "remote_auth",
                level: "fail",
                detail: format!(
                    "LOOP_STUDIO_BIND_ADDR={} without LOOP_STUDIO_API_TOKEN — router will refuse to start; set a token",
                    bind
                ),
            });
        } else {
            checks.push(Check {
                name: "remote_auth",
                level: "ok",
                detail: if loopback {
                    format!("bind {bind} is loopback (no token required)")
                } else {
                    format!("bind {bind} with Bearer token (redacted)")
                },
            });
        }
    }

    let fails = checks.iter().filter(|c| c.level == "fail").count();
    let warns = checks.iter().filter(|c| c.level == "warn").count();

    let hist_path = format!("{}/.loop-studio/doctor-history.jsonl", common::home_dir());
    let prev_free: Option<f64> = fs::read_to_string(&hist_path).ok().and_then(|c| {
        let last = c.lines().last()?;
        let p = last.find("\"free\":")? + 7;
        last[p..]
            .trim_start()
            .split(',')
            .next()?
            .trim()
            .parse()
            .ok()
    });
    let free_now = common::free_ram_gib();
    common::append_jsonl(
        &hist_path,
        &format!(
            "{{\"ts\":{},\"free\":{},\"issues\":{},\"warnings\":{}}}",
            common::now_unix(),
            free_now
                .map(|f| format!("{:.1}", f))
                .unwrap_or_else(|| "null".to_string()),
            fails,
            warns
        ),
    );

    if json {
        let mut s = String::from("{\"issues\":");
        s.push_str(&fails.to_string());
        s.push_str(",\"warnings\":");
        s.push_str(&warns.to_string());
        s.push_str(",\"checks\":[");
        for (i, c) in checks.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&format!(
                "{{\"name\":\"{}\",\"level\":\"{}\",\"detail\":\"{}\"}}",
                common::json_escape(c.name),
                common::json_escape(c.level),
                common::json_escape(&c.detail)
            ));
        }
        s.push_str("]}");
        println!("{}", s);
    } else {
        println!("================================================================");
        println!(
            "  Loop Studio DOCTOR — SYSTEM INTEGRITY DIAGNOSTIC (v{})",
            VERSION
        );
        println!("================================================================");
        for c in &checks {
            let mark = match c.level {
                "ok" => "✓",
                "warn" => "!",
                _ => "✗",
            };
            println!("  [{}] {:<16} {}", mark, c.name, c.detail);
        }
        if let (Some(pf), Some(cf)) = (prev_free, free_now) {
            let arrow = if cf < pf {
                "↓"
            } else if cf > pf {
                "↑"
            } else {
                "→"
            };
            println!(
                "  [..] ram_trend      {:.1} -> {:.1} GiB free {} (since last doctor)",
                pf, cf, arrow
            );
        }
        println!("================================================================");
        if fails == 0 {
            println!("  RESULT: stack healthy ({} warning(s)). Ready.", warns);
        } else {
            println!(
                "  RESULT: {} failure(s), {} warning(s). Run `loop-studio bootstrap` to repair.",
                fails, warns
            );
        }
        println!("================================================================");
    }
    fails as i32
}

// ------------------------------------------------------------------ thermal -

pub fn cmd_thermal() {
    println!("================================================================");
    println!("  THERMAL & POWER GOVERNOR");
    println!("================================================================");
    let state = common::thermal_state();
    println!(
        "  Current Thermal Level : {}",
        common::thermal_label(&state)
    );
    println!("\n  Telemetry Detail:\n{}", common::thermal_detail().trim());
    println!("----------------------------------------------------------------");
    match state.as_deref() {
        Some("Nominal") => {
            println!("  Recommendation: Optimal headroom. Full Q8_0 and MTP enabled.")
        }
        Some("Fair") => {
            println!("  Recommendation: Moderate heat. Monitor TTFT; keep vents clear.")
        }
        Some("Serious") => {
            println!("  Recommendation: Throttling near. Swap Q8->Q4; defer long loops.")
        }
        Some("Critical") => {
            println!("  Recommendation: Overheat risk! Throttle Q8->Q4 now or pause 5 min.")
        }
        None => println!(
            "  Recommendation: UNKNOWN — pmset did not answer, so no level below is a \
             measurement. Check the raw detail above before trusting a wide context."
        ),
        Some(_) => {}
    }
    println!("================================================================");
}

// --------------------------------------------------------------------- cap -

pub fn cap_path() -> String {
    format!("{}/.loop-studio/context-cap", common::home_dir())
}

pub fn cmd_cap(args: &[String]) {
    println!("=== Loop Studio Context Cap Hygiene ===");
    if args.len() >= 2 && args[0] == "--set" {
        let cap = &args[1];
        match cap.parse::<u64>() {
            Ok(n) => {
                if common::persist_or_warn(&cap_path(), &n.to_string(), "context_cap") {
                    println!(
                        "Persisted active context cap: {} tokens ({}).",
                        n,
                        cap_path()
                    );
                }
            }
            Err(_) => println!("Invalid cap '{}': expected integer tokens.", cap),
        }
        println!("Steady-state recommendation: 16,000 tokens (emergency truncate at 24,000).");
    } else {
        let saved = fs::read_to_string(cap_path())
            .ok()
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "(unset)".to_string());
        println!("Persisted cap ({}): {}", cap_path(), saved);
        println!("  - Native Served Context : 65,536 tokens");
        println!("  - Steady-state Session Cap : 16,000 tokens (`context-cap`)");
        println!("  - Emergency Truncation Trigger : 24,000 tokens (`kv-manage`)");
    }
}

// ----------------------------------------------------------------- visualize -

pub fn cmd_visualize(root: &str) {
    if !common::port_up(common::gateway_port()) {
        let port = common::gateway_port();
        println!(
            "Auto-starting loop-studio-router on :{} in background...",
            port
        );
        let router_bin = common::bin(root, "loop-studio-router");
        let _ = Command::new(&router_bin)
            .env("LOOP_STUDIO_ROUTER_PORT", port.to_string())
            .arg(port.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        std::thread::sleep(Duration::from_millis(300));
    }
    let app_bundles = [
        format!("{}/SwiftUI/.build/Loop Studio.app", root),
        format!("{}/SwiftUI/.build/Loop LAC Studio.app", root),
        format!("{}/SwiftUI/.build/LAC Studio.app", root),
        format!("{}/SwiftUI/.build/LACDashboard.app", root),
    ];
    for app in &app_bundles {
        if fs::metadata(app).is_ok() {
            match Command::new("open").arg(app).status() {
                Ok(status) if status.success() => return,
                Ok(status) => eprintln!("Loop Studio launch failed ({}): {}", app, status),
                Err(e) => eprintln!("Loop Studio launch failed ({}): {}", app, e),
            }
        }
    }
    let candidates = [
        env::var("LOOP_STUDIO_APP").ok(),
        env::var("LOOP_STUDIO_DASHBOARD").ok(),
        Some(format!("{}/SwiftUI/.build/debug/LoopStudio", root)),
        Some(format!("{}/SwiftUI/.build/debug/LoopLACStudio", root)),
        Some(format!("{}/SwiftUI/.build/debug/LACStudio", root)),
        Some(format!("{}/SwiftUI/.build/debug/LACDashboard", root)),
    ];
    for cand in candidates.into_iter().flatten() {
        if fs::metadata(&cand).is_ok() {
            match Command::new(&cand).status() {
                Ok(status) if status.success() => return,
                Ok(status) => eprintln!("Loop Studio launch failed ({}): {}", cand, status),
                Err(e) => eprintln!("Loop Studio launch failed ({}): {}", cand, e),
            }
        }
    }
    eprintln!("Loop Studio not built yet.");
    eprintln!(
        "Build & package: cd {}/SwiftUI && ./package-app.sh --open",
        root
    );
    eprintln!("Or build debug:  cd {}/SwiftUI && swift run", root);
}

// ----------------------------------------------------------------- dashboard -

/// Whether two paths are the same file, following symlinks.
pub fn same_file(a: &str, b: &str) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        (Ok(_), Err(_)) | (Err(_), Ok(_)) => false,
        (Err(_), Err(_)) => a == b,
    }
}

pub fn cmd_dashboard(root: &str, args: &[String]) {
    if args.iter().any(|a| a == "--install") {
        let built_loop_studio = format!("{}/SwiftUI/.build/debug/LoopStudio", root);
        let built_lac_studio = format!("{}/SwiftUI/.build/debug/LACStudio", root);
        let built_loop_lac_studio = format!("{}/SwiftUI/.build/debug/LoopLACStudio", root);
        let built_dash = format!("{}/SwiftUI/.build/debug/LACDashboard", root);
        let src = env::var("LOOP_STUDIO_APP")
            .ok()
            .filter(|p| fs::metadata(p).is_ok())
            .or_else(|| {
                env::var("LOOP_STUDIO_DASHBOARD")
                    .ok()
                    .filter(|p| fs::metadata(p).is_ok())
            })
            .or_else(|| {
                if fs::metadata(&built_loop_studio).is_ok() {
                    Some(built_loop_studio)
                } else {
                    None
                }
            })
            .or_else(|| {
                if fs::metadata(&built_lac_studio).is_ok() {
                    Some(built_lac_studio)
                } else {
                    None
                }
            })
            .or_else(|| {
                if fs::metadata(&built_loop_lac_studio).is_ok() {
                    Some(built_loop_lac_studio)
                } else {
                    None
                }
            })
            .unwrap_or(built_dash);
        if fs::metadata(&src).is_err() {
            eprintln!(
                "Loop Studio not built yet. Build first: cd {}/SwiftUI && swift build",
                root
            );
            std::process::exit(1);
        }
        let dir = format!("{}/.local/bin", common::home_dir());
        if let Err(e) = fs::create_dir_all(&dir) {
            eprintln!("Install failed: cannot create {}: {}", dir, e);
            std::process::exit(1);
        }
        let dst_loop = format!("{}/LoopStudio", dir);
        let dst_lac = format!("{}/LACStudio", dir);
        let dst_loop_lac = format!("{}/LoopLACStudio", dir);
        let dst_dash = format!("{}/LACDashboard", dir);
        let source_len = fs::metadata(&src).map(|m| m.len()).unwrap_or(0);
        let mut failures: Vec<String> = Vec::new();
        for dst in [&dst_loop, &dst_lac, &dst_loop_lac, &dst_dash] {
            if same_file(&src, dst) {
                failures.push(format!(
                    "{} is the source (LOOP_STUDIO_APP/LOOP_STUDIO_DASHBOARD names a destination) — refusing to copy it onto itself",
                    dst
                ));
                continue;
            }
            match fs::copy(&src, dst) {
                Ok(n) if n == source_len => {}
                Ok(n) => failures.push(format!("{}: wrote {n} of {source_len} bytes", dst)),
                Err(e) => failures.push(format!("{}: {}", dst, e)),
            }
        }
        if !failures.is_empty() {
            for failure in &failures {
                eprintln!("Install failed: {}", failure);
            }
            std::process::exit(1);
        }
        println!("Installed Loop Studio → {} (launch with `loop-studio studio`, `loop-studio visualize`, or `loop-studio dashboard`)", dst_loop);
        return;
    }
    cmd_visualize(root);
}

// ------------------------------------------------------------------ stop/ps/logs/config -

pub fn cmd_stop() {
    println!("Stopping Loop Studio services...");
    let pid_files = [(
        format!("{}/.loop-studio/router.pid", common::home_dir()),
        "loop-studio-router",
    )];
    for (pf, label) in pid_files {
        if let Ok(pid) = fs::read_to_string(&pf) {
            let pid = pid.trim().to_string();
            if !pid.is_empty() {
                let identity = pid
                    .parse::<u32>()
                    .ok()
                    .and_then(common::command_of)
                    .unwrap_or_default();
                if identity.is_empty() {
                    println!("  {} pid {}: not running", label, pid);
                } else if !common::program_matches(&identity, label) {
                    println!(
                        "  {} pid {}: left alone — that pid is `{}`, not a {}",
                        label,
                        pid,
                        common::head_lines(&identity, 1),
                        label
                    );
                    continue;
                } else {
                    let ok = common::signal_pid(pid.parse::<u32>().unwrap_or(0), "TERM");
                    println!(
                        "  {} pid {}: {}",
                        label,
                        pid,
                        if ok { "signaled" } else { "signal failed" }
                    );
                }
            }
            let _ = fs::remove_file(&pf);
        }
    }
    let n = common::kill_matching("loop-studio-router");
    println!("  loop-studio-router process sweep: {} signaled", n);
    println!("Note: inference engines (mlx/llama/ollama) left running; stop them via the TUI serve menu or brew services.");
}

pub fn cmd_ps() {
    println!("=== Loop Studio processes ===");
    let mut seen: Vec<(u32, String)> = Vec::new();
    for pat in [
        "loop-studio-router",
        "loop-studio-tui",
        "serve-mlx",
        "serve-llama",
        "llama-server",
        "mlx_lm.server",
        "ollama serve",
    ] {
        for p in common::processes_matching(pat) {
            if !seen.iter().any(|(pid, _)| *pid == p.pid) {
                seen.push((p.pid, p.cmd));
            }
        }
    }
    seen.sort();
    if seen.is_empty() {
        println!("  (no loop-studio-related processes found)");
    }
    for (pid, cmd) in seen {
        println!("  {:>6} {}", pid, cmd);
    }
    println!("\n=== Port table ===");
    println!(
        "  {:<8} :{}  {}",
        "gateway",
        common::gateway_port(),
        if common::port_up(common::gateway_port()) {
            "LISTENING"
        } else {
            "-"
        }
    );
    println!(
        "  {:<8} :{}  {}",
        "mlx",
        common::mlx_port(),
        if common::port_up(common::mlx_port()) {
            "LISTENING"
        } else {
            "-"
        }
    );
    for (name, port) in [
        ("llama", common::llama_port()),
        ("ollama", common::ollama_port()),
    ] {
        println!(
            "  {:<8} :{}  {}",
            name,
            port,
            if common::port_up(port) {
                "LISTENING"
            } else {
                "-"
            }
        );
    }
}

/// The name of the most recent transcript.
pub fn newest_task_log_name() -> String {
    common::oldest_task_logs(usize::MAX)
        .pop()
        .map(|(path, _)| {
            std::path::Path::new(&path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
        .unwrap_or_default()
}

pub fn cmd_logs(args: &[String]) {
    let purge = args.iter().any(|a| a == "--purge");
    let which = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .map(|s| s.as_str())
        .unwrap_or("router");
    if which == "tasks" || which == "task-logs" {
        if purge {
            let victims = common::oldest_task_logs(usize::MAX);
            let (total, count) = common::task_logs_footprint();
            let doomed = victims.len().saturating_sub(common::TASK_LOG_KEEP);
            let mut reclaimed = 0u64;
            for (path, len) in victims.into_iter().take(doomed) {
                if fs::remove_file(&path).is_ok() {
                    reclaimed += len;
                }
            }
            println!(
                "purged {doomed} agent transcript(s): {:.1} MiB reclaimed \
                 (the {} most recent are kept)",
                reclaimed as f64 / (1024.0 * 1024.0),
                common::TASK_LOG_KEEP
            );
            if count == 0 {
                println!("  (there were no transcripts to purge)");
            }
            let _ = total;
        }
        let (task_bytes, task_files) = common::task_logs_footprint();
        println!("=== {} ===", common::task_logs_dir());
        println!(
            "    {:.1} MiB across {task_files} transcript(s) (newest {} kept, each capped at {} MiB)",
            task_bytes as f64 / (1024.0 * 1024.0),
            common::TASK_LOG_KEEP,
            common::TASK_LOG_MAX_BYTES / (1024 * 1024),
        );
        let newest = newest_task_log_name();
        if newest.is_empty() {
            println!("(no task transcripts yet)");
            return;
        }
        match common::tail_file(&format!("{}/{}", common::task_logs_dir(), newest), 100) {
            Some(t) if !t.is_empty() => print!("{}", t),
            _ => println!("(transcript unreadable — {} may be mid-write)", newest),
        }
        return;
    }
    let file = match common::log_file_name(which) {
        Some(name) => name,
        None => {
            eprintln!(
                "loop-studio logs: unknown log {which:?}. Known: {}",
                common::log_which_names().join(", ")
            );
            std::process::exit(2);
        }
    };
    let path = format!("{}/{}", common::logs_dir(), file);
    if purge {
        let (reclaimed, failed) = common::purge_rotated_generations(&path);
        println!(
            "purged rotated generations of {file}: {:.1} MiB reclaimed (the live log is untouched)",
            reclaimed as f64 / (1024.0 * 1024.0)
        );
        if failed > 0 {
            eprintln!("  [x] {failed} generation(s) could not be removed and are still on disk");
            std::process::exit(1);
        }
    }
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let generations = std::fs::read_dir(common::logs_dir())
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .starts_with(&format!("{file}."))
                })
                .count()
        })
        .unwrap_or(0);
    println!("=== tail {} ===", path);
    println!(
        "    {:.1} MiB{}{}",
        size as f64 / (1024.0 * 1024.0),
        if generations > 0 {
            format!(", {generations} rotated generation(s) beside it")
        } else {
            String::new()
        },
        if size > common::LOG_MAX_BYTES {
            format!(
                " — over the {} MiB the daemon rotates at",
                common::LOG_MAX_BYTES / (1024 * 1024)
            )
        } else {
            String::new()
        }
    );
    let (bytes, files) = common::lac_logs_footprint();
    println!(
        "--- all Loop Studio logs: {:.1} MiB across {files} file(s){} ---",
        bytes as f64 / (1024.0 * 1024.0),
        if bytes > common::LOG_MAX_BYTES * 8 {
            " — reclaim with `loop-studio logs <which> --purge`"
        } else {
            ""
        }
    );
    if which == "tasks" {
        let (task_bytes, task_files) = common::task_logs_footprint();
        println!(
            "{}: {:.1} MiB across {task_files} transcript(s) (newest {} kept, each capped at {} MiB)",
            common::task_logs_dir(),
            task_bytes as f64 / (1024.0 * 1024.0),
            common::TASK_LOG_KEEP,
            common::TASK_LOG_MAX_BYTES / (1024 * 1024),
        );
        match common::tail_file(
            &format!("{}/{}", common::task_logs_dir(), newest_task_log_name()),
            100,
        ) {
            Some(t) if !t.is_empty() => print!("{}", t),
            _ => println!("(no task transcripts yet)"),
        }
        return;
    }
    match common::tail_file(&path, 100) {
        Some(t) if !t.is_empty() => print!("{}", t),
        _ => println!("(log file absent or empty — daemon may never have run)"),
    }
}

pub fn cmd_config(root: &str) {
    let e = |k: &str, d: &str| env::var(k).unwrap_or_else(|_| d.to_string());
    println!("=== Loop Studio effective configuration ===");
    println!("  version      : {}", VERSION);
    println!("  root         : {}", root);
    println!(
        "  gateway      : {}",
        e(
            "LOOP_STUDIO_GATEWAY_URL",
            &format!("http://127.0.0.1:{}/v1", common::gateway_port())
        )
    );
    println!("  router_port  : {}", common::gateway_port());
    println!(
        "  bind_addr    : {}",
        e(
            "LOOP_STUDIO_BIND_ADDR",
            &e("LOOP_STUDIO_ROUTER_HOST", "127.0.0.1")
        )
    );
    println!(
        "  auth         : {}",
        if env::var("LOOP_STUDIO_API_TOKEN")
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
        {
            "Bearer required on every listener (token set, redacted)"
        } else {
            "none (loopback only)"
        }
    );
    println!("  backend_pref : {}", e("LOOP_STUDIO_BACKEND", "auto"));
    println!("  primary      : {}", e("PRIMARY_MODEL", "qwen3.8-27b"));
    println!(
        "  mlx_model    : {}",
        e("MLX_MODEL", "mlx-community/Qwen3.8-27B-4bit")
    );
    println!(
        "  llama_model  : {}",
        e("LLAMA_MODEL", "unsloth/Qwen3.6-27B-MTP-GGUF:Q8_0")
    );
    println!(
        "  model_base   : {} (mounted={})",
        common::model_base(),
        common::volume_mounted("/Volumes/AIModels")
    );
    println!(
        "  branch       : {} (commits={})",
        common::default_branch_in(root),
        common::repo_has_commits_in(root)
    );
    println!(
        "  task_timeout : {}s",
        super::worker::task_timeout().as_secs()
    );
}
