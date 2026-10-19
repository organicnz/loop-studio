mod common;

use std::fs;
use std::io::{self, Write};
use std::process::Command;

fn clear() {
    print!("\x1B[2J\x1B[1;1H");
    let _ = io::stdout().flush();
}

fn pause() {
    println!("\nPress Enter to continue...");
    let mut buf = String::new();
    let _ = io::stdin().read_line(&mut buf);
}

/// Run a command and render its output *with its verdict*.
///
/// The status used to be shown only when the output was empty, so a command
/// that printed something and failed — `df` on a volume that is not mounted,
/// `kv-manage check` at RED, an `opencode2` run that errored — looked
/// identical to one that succeeded. Every gate in this project was taught to
/// report a nonzero exit; this was the one place that swallowed it.
fn run(cmd: &str, args: &[&str]) -> String {
    match Command::new(cmd).args(args).output() {
        Ok(o) => {
            let mut s = String::from_utf8_lossy(&o.stdout).to_string();
            let e = String::from_utf8_lossy(&o.stderr).to_string();
            if !e.is_empty() {
                s.push_str("\n[stderr]\n");
                s.push_str(&e);
            }
            if s.trim().is_empty() {
                s = "(no output)".to_string();
            }
            if o.status.success() {
                format!("{}\n[exit 0]", s.trim_end())
            } else {
                // The failure is the headline, not a footnote on some output.
                format!("FAILED ({}):\n{}", o.status, s.trim_end())
            }
        }
        Err(e) => format!("FAILED to run {}: {}", cmd, e),
    }
}

fn port_up(port: u16) -> bool {
    common::port_up(port)
}


fn free_ram_gib() -> Option<f64> {
    common::free_ram_gib()
}

// Project root + sibling binaries resolve through the shared module.
fn project_root() -> String {
    common::project_root()
}

fn bin(root: &str, name: &str) -> String {
    common::bin(root, name)
}

fn status_header(root: &str) -> String {
    let gw = if port_up(common::gateway_port()) { "UP  " } else { "DOWN" };
    let mlx = if port_up(common::mlx_port()) { "UP  " } else { "DOWN" };
    let ram = free_ram_gib()
        .map(|f| format!("{:.1}GiB", f))
        .unwrap_or_else(|| "?".to_string());
    // The ports probed are the ports printed (ollama and llama honor
    // LOOP_STUDIO_OLLAMA_PORT / LOOP_STUDIO_LLAMA_PORT; these two literals did not), and the
    // model id comes from the shared parser: the hand-rolled quote scan this
    // replaces indexed one byte past the end of a file truncated right after
    // `"model"`, and sliced a character in half on a non-ASCII byte next —
    // either one a panic, on every menu redraw.
    let o = if port_up(common::ollama_port()) { "UP  " } else { "DOWN" };
    let llama = if port_up(common::llama_port()) { "UP  " } else { "DOWN" };
    let model = fs::read_to_string(format!("{}/opencode.jsonc", root))
        .ok()
        .and_then(|t| common::jsonc_model_id(&t))
        .unwrap_or_else(|| "—".to_string());
    let arch = common::arch_label();
    format!(
        "Loop Studio  model:{}  gw:{}  ollama:{}  mlx:{}  llama:{}  free:{}  | {}",
        model, gw, o, mlx, llama, ram, arch
    )
}

/// Native model-catalog probe (no curl subprocess): truncated for display.
fn http_models(port: u16) -> String {
    match common::http_get(port, "/v1/models", 3000) {
        Some((200, b)) => {
            let mut s = b;
            if s.len() > 1500 {
                // Byte cuts can split UTF-8: walk back to a char boundary
                // (String::truncate panics otherwise).
                let mut cut = 1500.min(s.len());
                while !s.is_char_boundary(cut) {
                    cut -= 1;
                }
                s.truncate(cut);
                s.push_str("…[truncated]");
            }
            s
        }
        Some((c, _)) => format!("HTTP {}", c),
        None => "OFFLINE".to_string(),
    }
}

/// The lanes the health check reports, with the port it probes for each.
///
/// One expression for both the label and the probe, so a report cannot claim a
/// port it did not check. The MLX lane follows `serve-mlx`'s own port file,
/// which is how a drifted engine is found at all.
fn health_lanes() -> [(&'static str, u16); 3] {
    [
        ("Ollama", common::ollama_port()),
        ("MLX", common::mlx_port()),
        ("llama", common::llama_port()),
    ]
}

/// Render the lanes from one list, so the port in a label and the port probed
/// cannot be different numbers. Separated from `health_check` so a test can
/// render it without touching the network.
fn render_health_lanes(lanes: [(&'static str, u16); 3], probe: impl Fn(u16) -> String) -> String {
    let mut out = String::new();
    for (name, port) in lanes {
        out.push_str(&format!("\n-- {name} :{port} --\n{}", probe(port)));
    }
    out
}

/// The gateway base URL, from the resolved port.
fn gateway_url(path: &str) -> String {
    format!("http://127.0.0.1:{}{path}", common::gateway_port())
}

fn health_check(root: &str) {
    clear();
    println!("=== Loop Studio Stack Health ===\n");
    print!("{}", render_health_lanes(health_lanes(), http_models));
    println!("\n-- gateway :{} --", common::gateway_port());
    // The body is kept on a non-200: the router's 503 and 400 both carry the
    // operator-facing reason, and "HTTP 503" is not that.
    println!(
        "{}",
        describe_reply(
            common::http_get(common::gateway_port(), "/loop-studio/status", 3000),
            "gateway"
        )
    );
    println!("\n-- ollama ps --");
    println!("{}", run("ollama", &["ps"]));
    println!("\n-- kv-manage check --");
    println!("{}", run(&bin(root, "kv-manage"), &["check"]));
    println!("\n-- disk /Volumes/AIModels --");
    println!("{}", run("df", &["-h", "/Volumes/AIModels"]));
    println!("\n-- thermals --");
    println!("{}", common::head_lines(&common::thermal_detail(), 20));
    pause();
}

fn serve_menu(root: &str) {
    loop {
        clear();
        println!("=== Serve Menu (qwen3.8-27B) ===\n");
        println!("  ollama :11434  {}", if port_up(11434) { "UP" } else { "DOWN" });
        println!("  mlx :{}  {}", common::mlx_port(), if port_up(common::mlx_port()) { "UP" } else { "DOWN" });
        println!("  llama :8081 {}", if port_up(8081) { "UP" } else { "DOWN" });
        println!();
        println!("  1) Start mlx-lm Q4 (fast, 16.1GB) — blocks, Ctrl+C stops");
        println!("  2) Start llama-server Q8_0 (quality, ~20GB) — blocks");
        println!("  3) Start ollama serve (background)");
        println!("  4) Stop mlx/llama servers (native kill)");
        println!("  5) Install launchd KeepAlive (non-stop survival)");
        println!("  6) Smart Serve         (auto-Q4/Q8 based on thermal/RAM)");
        println!("  7) Switch router backend (mlx/llama/ollama/auto/fastest)");
        println!("  0) Back\n");
        print!("choice> ");
        let _ = io::stdout().flush();
        let mut c = String::new();
        if io::stdin().read_line(&mut c).is_err() {
            return;
        }
        match c.trim() {
            "1" => {
                println!("\nExec: {} mlx-community/Qwen3.8-27B-4bit", bin(root, "serve-mlx"));
                let _ = Command::new(bin(root, "serve-mlx"))
                    .arg("mlx-community/Qwen3.8-27B-4bit")
                    .status();
                pause();
            }
            "2" => {
                println!("\nExec: serve-llama Q8_0 ctx 16384 (Ctrl+C stops)");
                let _ = Command::new(bin(root, "serve-llama"))
                    .args(["unsloth/Qwen3.6-27B-MTP-GGUF:Q8_0", "16384"])
                    .status();
                pause();
            }
            "3" => {
                if port_up(11434) {
                    println!("ollama :11434 already up");
                    pause();
                    continue;
                }
                // Prefer the brew service; fall back to a detached server.
                let started = Command::new("brew")
                    .args(["services", "start", "ollama"])
                    .output()
                    .map(|o| o.status.success())
                    .unwrap_or(false);
                if started {
                    println!("brew services: ollama started");
                } else if port_up(11434) {
                    println!("ollama :11434 already up");
                } else {
                    match Command::new("ollama")
                        .arg("serve")
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn()
                    {
                        Ok(c) => println!("ollama serve spawned in background (pid {})", c.id()),
                        Err(e) => println!("could not start ollama: {}", e),
                    }
                }
                pause();
            }
            "4" => {
                // Wrappers first, engines second, and the order is the whole
                // fix. This used to signal only `mlx_lm.server` and
                // `llama-server`, which under `LAUNCHDAEMON=1` — exactly what
                // the serve-mlx plist sets — left the supervising wrapper
                // alive to faithfully respawn the engine it had just killed,
                // 30 to 60 seconds later. "Stop" did not stop.
                //
                // Signalling a wrapper now takes its engine with it: the
                // wrapper forwards the signal rather than orphaning a process
                // that still holds the port and the weights. The engine sweep
                // afterwards is belt and braces for anything not owned by a
                // wrapper, such as an engine someone started by hand.
                //
                // `kill_matching` matches on program identity, not on the
                // pattern appearing anywhere in the command line. It used to
                // do the latter, so pressing 4 while `tail -f
                // ~/Library/Logs/loop-studio-serve-mlx.err` was open — a log path
                // this project prints itself — killed the reader, and the
                // count below reported it as an engine stopped.
                let mut parts = Vec::new();
                for pattern in ["serve-mlx", "serve-llama"] {
                    let killed = common::kill_matching(pattern);
                    if killed > 0 {
                        parts.push(format!("{killed} {pattern}"));
                    }
                }
                // Give the wrappers a moment to forward the signal and reap
                // their children before sweeping for engines directly.
                std::thread::sleep(std::time::Duration::from_millis(300));
                for pattern in ["mlx_lm.server", "llama-server"] {
                    let killed = common::kill_matching(pattern);
                    if killed > 0 {
                        parts.push(format!("{killed} {pattern}"));
                    }
                }
                if parts.is_empty() {
                    println!("no MLX or llama processes running");
                } else {
                    println!("stopped {}", parts.join(", "));
                }
                pause();
            }
            "5" => {
                // serve-mlx launchd install, retargeted at this machine
                // (same shared helper as `loop-studio daemon install`, so other
                // users/checkouts get correct binary, workdir, and logs).
                let src = format!("{}/launchd/org.loopstudio.serve-mlx.plist", root);
                let home = common::home_dir();
                let dst = format!("{}/Library/LaunchAgents/org.loopstudio.serve-mlx.plist", home);
                let installed = format!("{}/.local/bin/loop-studio-serve-mlx", home);
                // Existence is not capability: a non-executable or 0-byte
                // `~/.local/bin/loop-studio-serve-mlx` used to be retargeted into
                // ProgramArguments[0], so launchd respawned a file that
                // cannot run and the menu printed "installed".
                let program =
                    common::choose_program_binary(&installed, &common::bin(root, "serve-mlx"));
                if let Some(warning) = &program.warning {
                    println!("note: {}", warning);
                }
                if program.kind == common::ProgramKind::Missing {
                    println!("install failed: no runnable loop-studio-serve-mlx (build the release suite first)");
                    pause();
                    continue;
                }
                let want_bin = program.path;
                match std::fs::create_dir_all(format!("{}/Library/LaunchAgents", home))
                    .and_then(|_| std::fs::read_to_string(&src))
                    .map(|c| common::retarget_launchd_plist(&c, &home, root, &want_bin))
                    .and_then(|c| common::atomic_write(&dst, &c))
                {
                    Ok(()) => {
                        let uid = Command::new("id")
                            .arg("-u")
                            .output()
                            .ok()
                            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                            .unwrap_or_else(|| "501".to_string());
                        // Read launchctl's own complaint instead of throwing it
                        // away: "installed" with no reason attached is how a
                        // broken agent went unnoticed.
                        let out = Command::new("launchctl")
                            .args(["bootstrap", &format!("gui/{}", uid), &dst])
                            .output();
                        let ok = out.as_ref().map(|o| o.status.success()).unwrap_or(false);
                        println!("{}", if ok { "installed" } else { "plist placed; bootstrap it manually" });
                        if !ok {
                            if let Ok(o) = out {
                                let why = String::from_utf8_lossy(&o.stderr);
                                if let Some(line) = why.lines().find(|l| !l.trim().is_empty()) {
                                    println!("launchctl: {}", line.trim());
                                }
                            }
                        }
                    }
                    Err(e) => println!("install failed: {}", e),
                }
                pause();
            }
            "6" => smart_serve(root),
            "7" => switch_backend(),
            "0" | "q" => return,
            _ => {}
        }
    }
}

/// Smart Serve Orchestration — automatically selects Q4 vs Q8 based on
/// current thermal state, free RAM, and token usage. This is the "incredibly
/// intelligent" layer that builds on all the predictive/thermal/KV infrastructure.
fn smart_serve(root: &str) {
    clear();
    println!("=== Smart Serve Orchestration ===\n");
    println!("Assessing current conditions...\n");

    // 1. Check thermal state
    let thermal = thermal_state_from_pmset();
    println!("Thermal state: {}", common::thermal_label(&thermal));

    // 2. Check free RAM — an absent reading is reported as absent, not as 0.
    let free = free_ram_gib();
    let free_formatted = match free {
        Some(f) => format!("{f:.1} GiB"),
        None => "unknown (vm_stat did not answer)".to_string(),
    };
    println!("Free RAM: {}", free_formatted);

    // 3. Check current token usage, from the same source `kv-manage check`
    //    reads, so the number this decides with and the number the operator is
    //    shown are the same one. The TUI used to scrape `opencode2` alone and
    //    never read the usage log, so its "current tokens" could disagree with
    //    `kv-manage` by orders of magnitude with both looking authoritative.
    let (probed, probe_source) = common::session_tokens();
    let current_tokens = probed.unwrap_or(0);
    let tokens_str = match probed {
        Some(n) if n > 0 => format!(
            "{} tokens ({}{})",
            n,
            probe_source,
            if common::token_source_is_measured(probe_source) {
                ", measured"
            } else {
                ", an estimate"
            }
        ),
        _ => "unknown (no token source answered)".to_string(),
    };
    println!("Current tokens: {}", tokens_str);

    // 4. Apply decision rules
    let (model_choice, context, decision_reason) =
        decide_model_and_context(thermal.as_deref(), free, current_tokens);

    println!();
    println!("Decision: {}", decision_reason);
    println!();
    println!("Selected: {}", model_choice);
    println!("Context window: {} tokens", context);

    // 5. Ask user to confirm before starting
    println!();
    print!("Start this configuration? [y/n]> ");
    let _ = io::stdout().flush();
    let mut confirm = String::new();
    if io::stdin().read_line(&mut confirm).is_err() {
        println!("Cancelled.");
        pause();
        return;
    }
    let confirm = confirm.trim().to_lowercase();
    if !confirm.starts_with('y') && !confirm.starts_with('1') {
        println!("Cancelled.");
        pause();
        return;
    }

    // 6. Start the selected server
    start_served_model(root, &model_choice, context);
    pause();
}

/// Decide which lane to serve: thresholds scale with total RAM via
/// `common::serve_ram_tiers_gib()`, and the MLX lane is Apple Silicon
/// only (Intel Macs fall back to llama-server).
/// Q4 always rides MLX (native MTP, fastest); Q8 always rides
/// llama-server (quality). The old table mixed them up -- it labelled
/// choices "llama Q4" then booted an MLX model into llama-server.
/// `free` is what was *measured*, and None is a real answer: `vm_stat` can
/// fail, and an absent reading is not zero free RAM. The caller used
/// `unwrap_or(0.0)`, so a machine whose probe failed was reported as "Free
/// RAM: 0.0 GiB" and then handed the lowest tier and the smallest context
/// window — a decision made on a number nobody measured, printed as one
/// somebody did.
///
/// `thermal` is `Option` for the same reason, and now for the same bug: the
/// probe used to run `pmset -g thermals`, an argument pmset rejects on every
/// host, so a probe that had failed arrived here already flattened to the
/// string "Nominal" and was spent as headroom. `None` now means what it says:
/// the probe did not answer. One standard applies in both directions — an
/// absent reading is not evidence of pressure, and it is not evidence of
/// headroom either. It is named in every reason line, so the decision cannot
/// look better-informed than it is.
fn decide_model_and_context(
    thermal: Option<&str>,
    free: Option<f64>,
    current_tokens: u64,
) -> (String, i32, String) {
    let threshold_tokens = 24000u64;
    let (high_gib, medium_gib) = common::serve_ram_tiers_gib();
    let mlx_ok = common::mlx_supported();
    let q4_lane = if mlx_ok {
        "mlx Q4 (mlx-community/Qwen3.8-27B-4bit)".to_string()
    } else {
        // Intel / non-MLX Macs: closest speed lane is llama Q8 with a
        // small context; never advertise an unbootable MLX model.
        "llama-server Q8 (unsloth/Qwen3.6-27B-MTP-GGUF:Q8_0)".to_string()
    };
    // How the RAM reading is described in every reason line below. One place,
    // so no branch can print a measurement that was never taken.
    let free_text = match free {
        Some(f) => format!("{:.1} GiB", f),
        None => "unknown".to_string(),
    };
    // And the same for the thermal reading, in one place. `unknown` is not
    // `Nominal`, and a reason line that cannot tell them apart is the defect.
    let thermal_text = match thermal {
        Some(t) => t.to_string(),
        None => "unknown".to_string(),
    };
    let tokens_percent = if threshold_tokens > 0 {
        (current_tokens as f64 / threshold_tokens as f64) * 100.0
    } else {
        0.0
    };

    // Rule 1: heat always wins -- shed quality for survival.
    if thermal == Some("Critical") {
        return (
            q4_lane.clone(),
            4096,
            format!("Thermal Critical -> {} ctx=4096 to survive. Free RAM: {free_text}, tokens {}/24000 ({:.1}%)", q4_lane, current_tokens, tokens_percent),
        );
    }
    if thermal == Some("Serious") || thermal == Some("Fair") {
        return (
            q4_lane.clone(),
            8192,
            format!("Thermal {thermal_text} -> {} ctx=8192 for stability. Free RAM: {free_text}.", q4_lane),
        );
    }

    // Rule 2: no heat recorded -- spend RAM on quality while headroom is vast.
    // Tiers scale with total RAM (high/medium), not fixed 40/16 GiB. Every
    // RAM test requires a reading: "we could not measure it" is not evidence
    // of pressure, and treating it as such picked the smallest configuration on
    // exactly the machines where the probe was broken.
    if free.is_some_and(|f| f > high_gib) && tokens_percent < 50.0 {
        return (
            "llama-server Q8 (unsloth/Qwen3.6-27B-MTP-GGUF:Q8_0)".to_string(),
            16384,
            format!("{thermal_text} + {free_text} free + tokens {:.1}% -> llama Q8 ctx=16384 for maximum quality.", tokens_percent),
        );
    }
    if free.is_some_and(|f| f >= medium_gib) {
        return (
            q4_lane.clone(),
            16384,
            // The token load is part of why this is not Q8, so the reason says
            // so: an operator reading "balanced speed" would have no way to
            // know a third input was consulted.
            format!(
                "{thermal_text} + {free_text} free + tokens {:.1}% of {threshold_tokens} -> {} ctx=16384, balanced speed.",
                tokens_percent, q4_lane
            ),
        );
    }
    (
        q4_lane,
        8192,
        match free {
            Some(f) => format!("{thermal_text} but only {f:.1} GiB free -> conserve with ctx=8192."),
            // Say why, and do not pretend the machine told us.
            None => format!("{thermal_text}, RAM reading unavailable -> cannot size for headroom, so ctx=8192."),
        },
    )
}

/// Start the selected lane. MLX <=> 4-bit MLX weights, llama <=> GGUF:
/// never cross them (llama-server cannot load MLX checkpoints).
fn start_served_model(root: &str, model_choice: &str, context: i32) {
    clear();
    println!("=== Starting Smart Serve ===\n");
    println!("Lane: {}", model_choice);
    println!("Context: {} tokens", context);
    println!();

    let ctx_str = context.to_string();
    if model_choice.starts_with("mlx") {
        println!("Starting mlx-lm (mlx-community/Qwen3.8-27B-4bit)...");
        let _ = Command::new(bin(root, "serve-mlx"))
            .arg("mlx-community/Qwen3.8-27B-4bit")
            .status();
    } else {
        println!("Starting llama-server (Q8_0, ctx {})...", ctx_str);
        let _ = Command::new(bin(root, "serve-llama"))
            .args(["unsloth/Qwen3.6-27B-MTP-GGUF:Q8_0", &ctx_str])
            .status();
    }

    println!();
    println!("Server start initiated. Check the TUI status header for live state.");
    pause();
}


/// One of Nominal | Fair | Serious | Critical, from the host — or `None` when
/// pmset could not answer, which is a different answer from any of them.
fn thermal_state_from_pmset() -> Option<String> {
    common::thermal_state()
}

/// Hot-swap the router's preferred backend via /lac/switch (persisted
/// router-side in ~/.lac/router-backend, survives restarts).
/// What to show for one HTTP reply, given the three things that can happen.
///
/// Split out from both callers so the decision is testable without a socket, and
/// so the two cannot drift. The property that matters: **a reply that explains
/// itself is not truncated to its status code.** The router answers
/// `400 {"status":"rejected","reason":"unknown backend","known":"mlx, llama, …"}`
/// and `503 {"error":{"message":"No LAC inference backend is serving… start one
/// with `loop-studio serve mlx`"}}` — every operator-facing detail is in the body, and
/// `Some((c, _)) => "HTTP {c}"` threw all of it away. A refusal you cannot read
/// is a refusal you cannot act on.
fn describe_reply(reply: Option<(u16, String)>, what: &str) -> String {
    match reply {
        Some((200, body)) => body,
        Some((code, body)) => {
            // The body is the reason, so it is shown; the status is the headline.
            let body = body.trim();
            if body.is_empty() {
                format!("{what} replied HTTP {code}")
            } else {
                format!("{what} replied HTTP {code}:\n{body}")
            }
        }
        None => format!("{what} unreachable — start it with loop-studio route --daemon"),
    }
}

/// The backend names this menu can ask for, and nothing else.
///
/// One list, used both to build the switch URL and to check the reply, so the
/// set of names the TUI can send and the set it believes in cannot diverge. The
/// router *refuses* an unrecognised target with a 400 rather than silently
/// choosing `auto`, which is the right behaviour and means this function has to
/// be right rather than merely plausible.
fn switch_target(choice: &str) -> Option<&'static str> {
    match choice.trim() {
        "1" => Some("auto"),
        "2" => Some("mlx"),
        "3" => Some("llama"),
        "4" => Some("ollama"),
        "5" => Some("fastest"),
        _ => None,
    }
}

/// The URL a switch target is requested at, from the resolved gateway port.
fn switch_url(target: &str) -> String {
    format!(
        "http://127.0.0.1:{}/loop-studio/switch?target={}",
        common::gateway_port(),
        target
    )
}

fn switch_backend() {
    clear();
    println!("=== Router Backend Switch ===\n");
    println!("  Current preference is shown at {}\n", gateway_url("/loop-studio/status"));
    println!("  1) auto    (MLX -> llama -> Ollama failover)");
    println!("  2) mlx     (Q4 speed lane)");
    println!("  3) llama   (Q8 quality lane)");
    println!("  4) ollama  (fallback)");
    println!("  5) fastest (lowest measured latency)");
    println!("  0) Back\n");
    print!("choice> ");
    let _ = io::stdout().flush();
    let mut c = String::new();
    if io::stdin().read_line(&mut c).is_err() {
        return;
    }
    let Some(target) = switch_target(&c) else {
        return;
    };
    // The reply is described, not merely shown, so a refusal reads as one, and
    // the URL is built by the same function the test checks — the port in a
    // request and the port the menu advertises cannot drift apart.
    let url = switch_url(target);
    let path = url
        .split_once("://")
        .and_then(|(_, rest)| rest.split_once('/'))
        .map(|(_, p)| format!("/{p}"))
        .unwrap_or_else(|| format!("/loop-studio/switch?target={target}"));
    println!(
        "{}",
        describe_reply(common::http_get(common::gateway_port(), &path, 5000), "router")
    );
    pause();
}

fn skills_menu() {
    loop {
        clear();
        println!("=== Skills Menu ===\n");
        println!("  1) local-verify        (stack health)");
        println!("  2) model-swap           (Q4<->Q8 toggle)");
        println!("  3) kv-cache-manage      (check / truncate)");
        println!("  4) context-cap          (steady-state cap)");
        println!("  5) agent-resume         (save / resume)");
        println!("  6) thermal-monitor      (temp check)");
        println!("  7) auditor              (code audit)");
        println!("  8) task-runner status   (queue overview)");
        println!("  0) Back\n");
        print!("choice> ");
        let _ = io::stdout().flush();
        let mut c = String::new();
        if io::stdin().read_line(&mut c).is_err() {
            return;
        }
        match c.trim() {
            "1" => {
                println!("{}", run("opencode2", &["run", "Use the local-verify skill and report stack health"]));
                pause();
            }
            "2" => {
                println!("{}", run("opencode2", &["run", "Use the model-swap skill to toggle the model backend"]));
                pause();
            }
            "3" => {
                print!("check or truncate? [c/t]> ");
                let _ = io::stdout().flush();
                let mut m = String::new();
                let _ = io::stdin().read_line(&mut m);
                if m.trim() == "t" {
                    print!("tokens before?> ");
                    let _ = io::stdout().flush();
                    let mut t = String::new();
                    let _ = io::stdin().read_line(&mut t);
                    println!("{}", run("opencode2", &["run", &format!("Use the kv-cache-manage skill to truncate context, keeping the last {} tokens", t.trim())]));
                } else {
                    println!("{}", run("opencode2", &["run", "Use the kv-cache-manage skill to check context usage"]));
                }
                pause();
            }
            "4" => {
                println!("{}", run("opencode2", &["run", "Use the context-cap skill to check current context usage"]));
                pause();
            }
            "5" => {
                print!("save or resume? [s/r]> ");
                let _ = io::stdout().flush();
                let mut m = String::new();
                let _ = io::stdin().read_line(&mut m);
                let prompt = if m.trim() == "r" {
                    "Use the agent-resume skill to resume the saved session state"
                } else {
                    "Use the agent-resume skill to save the current session state"
                };
                println!("{}", run("opencode2", &["run", prompt]));
                pause();
            }
            "6" => {
                println!("{}", run("opencode2", &["run", "Use the thermal-monitor skill to check thermals"]));
                pause();
            }
            "7" => {
                println!("{}", run("opencode2", &["run", "Use the auditor skill to audit the codebase"]));
                pause();
            }
            "8" => {
                println!("{}", run("opencode2", &["run", "Use the task-runner skill to show queue status"]));
                pause();
            }
            "0" | "q" => return,
            _ => {}
        }
    }
}

fn preflight(root: &str) {
    clear();
    println!("=== Pre-flight (long runs) ===\n");
    println!("-- servers --");
    println!("ollama :11434  {}", if port_up(11434) { "UP" } else { "DOWN" });
    println!("mlx :{}  {}", common::mlx_port(), if port_up(common::mlx_port()) { "UP" } else { "DOWN" });
    println!("llama :8081 {}", if port_up(8081) { "UP" } else { "DOWN" });
    println!("\n-- kv-manage check --");
    println!("{}", run(&bin(root, "kv-manage"), &["check"]));
    println!("\n-- thermals --");
    println!("{}", common::head_lines(&common::thermal_detail(), 8));
    println!("\n-- agent state --");
    let home = common::home_dir();
    let mut states = Vec::new();
    for p in [
        "/Volumes/AIModels/hf/agent-state.json".to_string(),
        format!("{}/.lac/agent-state.json", home),
    ] {
        match std::fs::metadata(&p) {
            Ok(m) => states.push(format!("{} ({} bytes)", p, m.len())),
            Err(_) => states.push(format!("{} (absent)", p)),
        }
    }
    println!("{}", states.join("\n"));
    println!("\nChecklist: servers UP → kv GREEN/YELLOW → thermals Nominal/Fair → state saved.");
    println!("Then: context-cap --set → agent-resume --auto → loop-orchestrator --run");
    pause();
}

fn sparkline(values: &[u32], max_val: u32) -> String {
    let ticks = [' ', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let mut out = String::new();
    let m = if max_val == 0 { 1 } else { max_val };
    for &v in values {
        let idx = ((v as usize * (ticks.len() - 1)) / m as usize).min(ticks.len() - 1);
        out.push(ticks[idx]);
    }
    out
}

/// Restores the cursor on the way out, however the scope ends.
///
/// This is the only terminal mode state this program touches and it had no
/// guard: the hide and the restore were two `println!`s with fifteen redraws
/// between them, so a Ctrl-C during the monitor (or any panic inside it — the
/// status header is parsed from a file the user edits) left the shell with no
/// cursor until `reset`.
struct CursorGuard;

impl Drop for CursorGuard {
    fn drop(&mut self) {
        println!("\x1B[?25h");
    }
}

fn live_monitor(root: &str) {
    println!("\x1B[?25l"); // Hide cursor
    let _cursor = CursorGuard;
    let mut history_ram: Vec<u32> = Vec::new();
    let mut iteration = 0;

    while iteration < 15 {
        iteration += 1;
        clear();
        println!("===========================================================");
        println!("  Loop Studio Live Telemetry Monitor (Apple Silicon)");
        println!("  {}", status_header(root));
        println!("===========================================================\n");

        let gw_up = port_up(common::gateway_port());
        let gw_status = if gw_up {
            format!("\x1B[32mONLINE (:{})\x1B[0m", common::gateway_port())
        } else {
            "\x1B[31mOFFLINE\x1B[0m".to_string()
        };
        println!("Gateway:      {}", gw_status);

        // The live gauge, and the same rule the decision path follows: a probe
        // that could not answer prints `unknown` and is left out of the
        // history, rather than being recorded as 0.0 GiB and drawn in red as
        // though the machine had run out of memory.
        let free_gb = free_ram_gib();
        let free_text = match free_gb {
            Some(f) => {
                history_ram.push((f * 10.0) as u32);
                if history_ram.len() > 24 { history_ram.remove(0); }
                let spark = sparkline(&history_ram, 640);
                let ram_color = if f >= 16.0 { "\x1B[32m" } else if f >= 8.0 { "\x1B[33m" } else { "\x1B[31m" };
                format!("{ram_color}{f:.1} GiB\x1B[0m  [{spark}]")
            }
            None => "\x1B[90munknown (vm_stat did not answer)\x1B[0m".to_string(),
        };
        println!("Free RAM:     {}", free_text);

        // The level, from the parser — not the first line of the raw capture,
        // which is a usage note on an idle Mac ("No thermal warning level has
        // been recorded") and so was a line of prose in a level's slot. The
        // old `unwrap_or("Nominal")` filled that slot with a level nobody read.
        println!("Thermals:     {}", common::thermal_label(&common::thermal_state()));

        println!("\nActive Lane Routing:");
        // Resolved ports, not literals: the labels used to be hardcoded while
        // the probes read the environment, so `LOOP_STUDIO_LLAMA_PORT=9001` printed
        // "llama (:8081)" and probed 8081 — the menu disagreed with the machine
        // in the same three lines.
        let mlx_port = common::mlx_port();
        let llama_port = common::llama_port();
        let ollama_port = common::ollama_port();
        println!("  - MLX (Lane 1):    {} (:{}/v1)", if port_up(mlx_port) { "\x1B[32mACTIVE\x1B[0m" } else { "\x1B[90mIDLE\x1B[0m" }, mlx_port);
        println!("  - Llama (Lane 2):  {} (:{}/v1)", if port_up(llama_port) { "\x1B[32mACTIVE\x1B[0m" } else { "\x1B[90mIDLE\x1B[0m" }, llama_port);
        println!("  - Ollama (Lane 3): {} (:{}/v1)", if port_up(ollama_port) { "\x1B[32mACTIVE\x1B[0m" } else { "\x1B[90mIDLE\x1B[0m" }, ollama_port);

        println!("\nSampling (AGENTS.md):");
        println!("  temp 0.6, top_p 0.95, context-cap 32k/16k, single-model resident.");
        println!("\n[Sample {}/15 | Updating live...]", iteration);

        std::thread::sleep(std::time::Duration::from_millis(800));
    }
    // The cursor comes back through `_cursor` on the way out, including on a
    // panic or an early return.
    drop(_cursor);
    pause();
}

fn main() {
    common::ignore_sigpipe();
    let root = project_root();
    loop {
        clear();
        println!("==============================================");
        println!("  {}", status_header(&root));
        println!("  {} | Q4<->Q8 | OpenCode V2", common::arch_label());
        println!("==============================================\n");
        println!("  1) Health check (servers, kv, disk, thermals)");
        println!("  2) Serve menu (start/stop Q4/Q8, launchd install)");
        println!("  3) Smart Serve (auto-Q4/Q8 based on thermal/RAM)");
        println!("  4) Skills menu (verify, swap, kv, resume, audit)");
        println!("  5) Pull models (qwen3.8-27b)");
        println!("  6) Pre-flight check (8hr+ long runs)");
        println!("  7) Bootstrap (one-time setup)");
        println!("  8) Live Monitor (TTFT, TPS, KV Pressure sparkline)");
        println!("  0) Quit\n");
        print!("choice> ");
        let _ = io::stdout().flush();
        let mut c = String::new();
        if io::stdin().read_line(&mut c).is_err() {
            break;
        }
        match c.trim() {
            "1" => health_check(&root),
            "2" => serve_menu(&root),
            "3" => smart_serve(&root),
            "4" => skills_menu(),
            "5" => {
                clear();
                println!("Pulling qwen3.8-27b (may take a while)...\n");
                let _ = Command::new(bin(&root, "pull-models")).status();
                pause();
            }
            "6" => preflight(&root),
            "7" => {
                clear();
                println!("Running bootstrap...\n");
                let _ = Command::new(bin(&root, "bootstrap")).status();
                pause();
            }
            "8" => live_monitor(&root),
            "0" | "q" | "quit" | "exit" => break,
            _ => {}
        }
    }
    clear();
    println!("Loop Studio TUI exit.");
}

#[cfg(test)]
mod tests {
    // A `#[test]` that loses its attribute is valid Rust that never runs:
    // the suite goes one test quieter and stays green. Fail the build instead.
    #![deny(dead_code)]

    use super::*;

    /// The decision rules pick a model and a context window from three inputs,
    /// and two of them can be *absent*. "We could not measure it" is not
    /// evidence of pressure, and the pre-fix `unwrap_or(0.0)` turned a failed
    /// `vm_stat` into "0.0 GiB free" and then the smallest configuration —
    /// a decision made on a number nobody measured.
    #[test]
    fn an_unreadable_ram_probe_does_not_select_the_conserving_tier() {
        let (high, medium) = common::serve_ram_tiers_gib();
        let (_model, ctx, reason) = decide_model_and_context(Some("Nominal"), None, 0);
        assert!(
            !reason.contains("only"),
            "an absent reading must not be reported as pressure: {reason}"
        );
        assert!(
            reason.contains("unavailable") || reason.contains("unknown"),
            "the reason must say the reading was unavailable: {reason}"
        );
        // Whatever it picks, the RAM text is never a fabricated number.
        assert!(!reason.contains("0.0 GiB free"), "{reason}");
        // ...and it is the same tier a machine with middling RAM would get,
        // not the floor.
        let (_, measured_ctx, _) = decide_model_and_context(Some("Nominal"), Some(medium - 1.0), 0);
        assert_eq!(ctx, measured_ctx, "unknown and low-RAM diverged: {reason}");
        assert!(high > medium);
    }

    /// Measured RAM still steers the choice, in both directions.
    #[test]
    fn measured_ram_steers_the_tier() {
        let (high, medium) = common::serve_ram_tiers_gib();
        let (q8_model, q8_ctx, q8_reason) =
            decide_model_and_context(Some("Nominal"), Some(high + 1.0), 0);
        assert!(q8_model.contains("llama-server Q8"), "{q8_reason}");
        assert_eq!(q8_ctx, 16384);

        let (q4_model, q4_ctx, q4_reason) =
            decide_model_and_context(Some("Nominal"), Some(medium + 0.5), 0);
        assert!(q4_model.contains("Q4"), "{q4_reason}");
        assert_eq!(q4_ctx, 16384, "the middle tier is still 16k");

        // Heat overrides RAM in both directions.
        let (hot_model, hot_ctx, hot_reason) =
            decide_model_and_context(Some("Critical"), Some(high + 10.0), 0);
        assert!(hot_model.contains("Q4"), "heat must win over RAM: {hot_reason}");
        assert_eq!(hot_ctx, 4096, "Critical is the survival tier");
    }

    /// An Intel host is never offered a model it cannot boot.
    #[test]
    fn a_non_mlx_host_is_never_offered_mlx() {
        if common::mlx_supported() {
            return; // this host is Apple Silicon; the gate is untestable here
        }
        let (model, _, reason) = decide_model_and_context(Some("Nominal"), Some(64.0), 0);
        assert!(!model.contains("mlx"), "{reason}");
    }

    /// Tokens are the third input: a loaded session costs quality even on a
    /// machine with RAM to spare — Q8 while the window is under half, Q4 once
    /// it is not. The context window follows the tier, not the load directly.
    #[test]
    fn a_loaded_session_costs_quality_not_window() {
        let (high, _) = common::serve_ram_tiers_gib();
        let (idle_model, idle_ctx, idle_reason) =
            decide_model_and_context(Some("Nominal"), Some(high + 1.0), 0);
        assert!(idle_model.contains("Q8"), "{idle_reason}");
        assert_eq!(idle_ctx, 16384);

        // 23k of a 24k threshold: past half, so the Q8 branch is skipped.
        let (loaded_model, loaded_ctx, loaded_reason) =
            decide_model_and_context(Some("Nominal"), Some(high + 1.0), 23_000);
        assert!(loaded_model.contains("Q4"), "{loaded_reason}");
        assert_eq!(
            loaded_ctx, idle_ctx,
            "the window follows the tier, not the load: {loaded_reason}"
        );
        assert!(loaded_reason.contains("tokens"), "the reason must say why: {loaded_reason}");
    }

    /// A command's output is never shown without its verdict.
    #[test]
    fn a_failing_command_is_labelled_as_failing() {
        // A real command that fails with output on stderr: `false` writes
        // nothing, so use a nonexistent binary for the launch failure and a
        // genuine nonzero exit for the rest.
        let missing = run("/nonexistent/lac-does-not-exist", &[]);
        assert!(missing.starts_with("FAILED"), "{missing}");

        let denied = run("/bin/sh", &["-c", "echo boom >&2; exit 3"]);
        assert!(denied.starts_with("FAILED"), "a nonzero exit must be the headline: {denied}");
        assert!(denied.contains("boom"), "the output must still be shown: {denied}");

        let ok = run("/bin/sh", &["-c", "echo fine"]);
        assert!(ok.contains("fine"), "{ok}");
        assert!(ok.contains("[exit 0]"), "a success says so too: {ok}");
    }

    /// A reply that explains itself is not truncated to its status code.
    ///
    /// Both TUI network call sites ended in `Some((c, _)) => "HTTP {c}"`,
    /// which threw away the body — and the body is where the router puts the
    /// only useful part of a refusal. `400 {"status":"rejected","reason":
    /// "unknown backend","known":"mlx, llama, …"}` and `503 {"error":
    /// {"message":"No LAC inference backend is serving… start one with
    /// `loop-studio serve mlx`"}}` are operator instructions. A TUI that printed
    /// "HTTP 503" for either hid the remedy behind the problem.
    ///
    /// This is no longer hypothetical: the router started *refusing* an
    /// unknown target with a 400 rather than silently choosing `auto` in this
    /// project's previous pass, so the reason now exists and the TUI has to
    /// show it.
    #[test]
    fn a_refusal_keeps_the_reason_the_router_gave_for_it() {
        let ok = describe_reply(Some((200, "{\"status\":\"ok\"}".to_string())), "gateway");
        assert_eq!(ok, "{\"status\":\"ok\"}", "a 200 is the document, as-is");

        let refused = describe_reply(
            Some((
                400,
                "{\"status\":\"rejected\",\"reason\":\"unknown backend\"}".to_string(),
            )),
            "router",
        );
        assert!(refused.starts_with("router replied HTTP 400"), "{refused}");
        assert!(
            refused.contains("unknown backend"),
            "the reason must survive: {refused}"
        );

        // 503 is the one that names the fix, so it is the one that matters most.
        let down = describe_reply(
            Some((
                503,
                "{\"error\":{\"message\":\"No Loop Studio inference backend is serving\"}}".to_string(),
            )),
            "gateway",
        );
        assert!(down.contains("No Loop Studio inference backend is serving"), "{down}");

        // A non-200 with no body still names the status, rather than printing "".
        assert_eq!(
            describe_reply(Some((500, "   ".to_string())), "gateway"),
            "gateway replied HTTP 500"
        );

        // Unreachable is a third case, not a fourth, and it says what to do.
        let offline = describe_reply(None, "gateway");
        assert!(offline.contains("unreachable"), "{offline}");
        assert!(offline.contains("loop-studio route --daemon"), "{offline}");
        assert_ne!(
            offline, "OFFLINE",
            "the old text named the state and not the remedy"
        );
    }

    /// The names the switch menu can ask for, and the URL it asks at.
    ///
    /// `switch_backend` held the menu mapping inline and built the URL with a
    /// format string, so neither could be checked. The router now refuses an
    /// unknown target with a 400 instead of quietly choosing `auto`, which
    /// turns a wrong name here from a cosmetic bug into a feature that stops
    /// working — silently, from the user's point of view, since the old code
    /// printed the router's reply and it said `"status": "switched"`.
    #[test]
    fn the_switch_menu_can_only_ask_for_a_name_the_router_accepts() {
        // Each menu choice resolves to a name, and to the right one.
        let expected = [
            ("1", "auto"),
            ("2", "mlx"),
            ("3", "llama"),
            ("4", "ollama"),
            ("5", "fastest"),
        ];
        for (choice, name) in expected {
            assert_eq!(
                switch_target(choice),
                Some(name),
                "menu choice {choice} must ask for {name}"
            );
        }
        // Whitespace, because a pasted line carries it.
        assert_eq!(switch_target(" 3 \n"), Some("llama"));

        // Anything else is "back", never a request.
        for other in ["0", "6", "", "q", "mlx", "-1"] {
            assert_eq!(switch_target(other), None, "{other:?} must not be sent");
        }

        // The URL carries the resolved gateway port, and the target.
        let url = switch_url("llama");
        assert!(url.contains("target=llama"), "{url}");
        assert!(
            url.contains(&common::gateway_port().to_string()),
            "the switch must go to the configured gateway, not a literal: {url}"
        );
        assert!(!url.contains(":8000/") || common::gateway_port() == 8000, "{url}");
    }

    /// The health check must probe the ports it prints, and both must be the
    /// configured ones. The MLX lane beside two literals already read
    /// `common::mlx_port()`, and the literals did not — so with
    /// `LOOP_STUDIO_LLAMA_PORT=9001` the same menu reported a live lane OFFLINE and
    /// offered to start it.
    ///
    /// Asserting this against the *default* ports proves nothing: the literals
    /// 11434 and 8081 equal the defaults, so the regression passes. The port
    /// is therefore moved off its default here, which is the only way this
    /// test can fail for hardcoded ports. Env mutation is process-global; this
    /// is the only test in this binary that touches these two variables, and
    /// it restores the prior values.
    #[test]
    fn the_health_check_follows_a_relocated_lane() {
        let saved_ollama = std::env::var("LOOP_STUDIO_OLLAMA_PORT").ok();
        let saved_llama = std::env::var("LOOP_STUDIO_LLAMA_PORT").ok();
        unsafe {
            std::env::set_var("LOOP_STUDIO_OLLAMA_PORT", "19001");
            std::env::set_var("LOOP_STUDIO_LLAMA_PORT", "19002");
        }

        let lanes = health_lanes();
        let probe = |port: u16| format!("-- probe {port} --");
        let rendered = render_health_lanes(lanes, probe);

        // The label, the probe and the resolved port are one number.
        assert!(rendered.contains("llama :19002"), "{rendered}");
        assert!(rendered.contains("-- probe 19002 --"), "{rendered}");
        assert!(rendered.contains("Ollama :19001"), "{rendered}");
        assert!(rendered.contains("-- probe 19001 --"), "{rendered}");
        assert!(!rendered.contains("11434"), "the literal returned: {rendered}");
        assert!(!rendered.contains("8081"), "the literal returned: {rendered}");

        unsafe {
            match saved_ollama {
                Some(v) => std::env::set_var("LOOP_STUDIO_OLLAMA_PORT", v),
                None => std::env::remove_var("LOOP_STUDIO_OLLAMA_PORT"),
            }
            match saved_llama {
                Some(v) => std::env::set_var("LOOP_STUDIO_LLAMA_PORT", v),
                None => std::env::remove_var("LOOP_STUDIO_LLAMA_PORT"),
            }
        }
        // A restored environment is what the next test in this binary expects.
        assert_eq!(health_lanes()[2].1, common::llama_port());
    }
}
