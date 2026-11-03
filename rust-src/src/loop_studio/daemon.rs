//! Daemon management: launchd install/uninstall/status, pull command.

use std::fs;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::common;
use super::worker::kill_tree;

// ------------------------------------------------------------------ pull signal -

static PULL_CANCELLED: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
pub fn install_pull_signal_handler() {
    type Sighandler = unsafe extern "C" fn(std::ffi::c_int);
    unsafe extern "C" {
        fn signal(sig: std::ffi::c_int, handler: Sighandler) -> Sighandler;
    }
    unsafe extern "C" fn cancel(_: std::ffi::c_int) {
        PULL_CANCELLED.store(true, Ordering::SeqCst);
    }
    unsafe {
        signal(15, cancel);
        signal(2, cancel);
    }
}

#[cfg(not(unix))]
pub fn install_pull_signal_handler() {}

pub fn run_pull_command(command: &mut Command) -> i32 {
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return 1,
    };
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.code().unwrap_or(1),
            Ok(None) => {
                if PULL_CANCELLED.load(Ordering::SeqCst) {
                    kill_tree(child.id());
                    let _ = child.kill();
                    let _ = child.wait();
                    return 130;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => {
                kill_tree(child.id());
                let _ = child.kill();
                let _ = child.wait();
                return 1;
            }
        }
    }
}

// --------------------------------------------------------------------- pull -

pub fn cmd_pull(_root: &str, args: &[String]) -> i32 {
    install_pull_signal_handler();
    let model = args
        .first()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or("qwen3.8-27b");
    println!("=== Loop Studio Model Pull: {} ===", model);

    let is_hf = model.contains('/')
        || model.starts_with("mlx-")
        || model.contains("MLX")
        || model.ends_with(".gguf");

    if is_hf {
        println!("Target is a Hugging Face / MLX repository: {}", model);
        let hf_hub = format!("{}/hf/hub", common::model_base());
        let _ = fs::create_dir_all(&hf_hub);

        if let Some(cli) = common::which("hf").or_else(|| common::which("huggingface-cli")) {
            println!("Downloading via Hugging Face CLI into {}...", hf_hub);
            let mut command = Command::new(cli);
            command
                .args(["download", model])
                .env("HF_HOME", format!("{}/hf", common::model_base()))
                .env("HF_HUB_CACHE", hf_hub.clone());
            let code = run_pull_command(&mut command);
            if code == 0 {
                println!("✓ Successfully downloaded {} to local cache.", model);
            } else if code == 130 {
                eprintln!("Download cancelled.");
            } else {
                eprintln!("huggingface-cli exited with status {}", code);
            }
            return code;
        }
        eprintln!(
            "Hugging Face CLI not found in PATH; install `brew install hf` before pulling models."
        );
        return 1;
    } else if let Some(ollama) = common::which("ollama") {
        println!("Pulling Ollama model: {}", model);
        let mut command = Command::new(ollama);
        command.arg("pull").arg(model);
        let code = run_pull_command(&mut command);
        if code == 0 {
            println!("✓ Successfully pulled Ollama model: {}", model);
        } else if code == 130 {
            eprintln!("Download cancelled.");
        } else {
            eprintln!("ollama pull exited with status {}", code);
        }
        return code;
    } else {
        eprintln!("ollama not found in PATH — install: brew install ollama");
        1
    }
}

// ------------------------------------------------------------------ launchd helpers -

pub fn launchd_src(root: &str, name: &str) -> String {
    format!("{}/launchd/{}.plist", root, name)
}

pub fn launchctl_bootstrap(dst: &str) -> bool {
    let uid = Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "501".to_string());
    let domain = format!("gui/{}", uid);
    let ok = Command::new("launchctl")
        .args(["bootstrap", &domain, dst])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        return true;
    }
    Command::new("launchctl")
        .args(["load", "-w", dst])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn launchctl_bootout(dst: &str, label: &str) -> bool {
    let uid = Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "501".to_string());
    let service_target = format!("gui/{}/{}", uid, label);
    let domain = format!("gui/{}", uid);
    let ok = Command::new("launchctl")
        .args(["bootout", &service_target])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        for attempt in [
            vec!["bootout", &domain, dst],
            vec!["unload", "-w", dst],
            vec!["remove", label],
        ] {
            if Command::new("launchctl")
                .args(&attempt)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
            {
                return true;
            }
        }
        return false;
    }
    true
}

// ------------------------------------------------------------------ cmd_daemon -

pub fn cmd_daemon(root: &str, args: &[String]) {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("status");
    let home = common::home_dir();
    let agent_dir = format!("{}/Library/LaunchAgents", home);
    let log_dir = format!("{}/Library/Logs", home);

    match sub {
        "install" => {
            // Platform-aware: use launchd on macOS, systemd on Linux.
            #[cfg(target_os = "linux")]
            {
                install_systemd_units(root, &home, &log_dir);
                return;
            }
            #[cfg(not(target_os = "linux"))]
            println!("Installing Loop Studio 24/7 Daemons as macOS LaunchAgents...");
            if let Err(e) = fs::create_dir_all(&agent_dir) {
                eprintln!("  [x] Cannot create {}: {}", agent_dir, e);
                std::process::exit(1);
            }
            let _ = fs::create_dir_all(&log_dir);
            let mut failures = 0usize;
            for label in ["org.loopstudio.router", "org.loopstudio.serve-mlx", "org.loopstudio.worker"] {
                let src = launchd_src(root, label);
                let dst = format!("{}/{}.plist", agent_dir, label);
                match fs::read_to_string(&src) {
                    Ok(content) => {
                        let installed = format!("{}/.local/bin/loop-studio", home);
                        let installed_router = format!("{}/.local/bin/loop-studio-router", home);
                        let installed_mlx = format!("{}/.local/bin/serve-mlx", home);
                        let release_lac = common::bin(root, "loop-studio");
                        let release_router = common::bin(root, "loop-studio-router");
                        let release_mlx = common::bin(root, "serve-mlx");
                        let program = if label == "org.loopstudio.router" {
                            common::choose_program_binary(&installed_router, &release_router)
                        } else if label == "org.loopstudio.serve-mlx" {
                            common::choose_program_binary(&installed_mlx, &release_mlx)
                        } else {
                            common::choose_program_binary(&installed, &release_lac)
                        };
                        if let Some(warning) = &program.warning {
                            eprintln!("  [!] {}", warning);
                        }
                        if program.kind == common::ProgramKind::Missing {
                            eprintln!("  [x] {} has no runnable binary — not placing it", dst);
                            failures += 1;
                            continue;
                        }
                        let want_bin = program.path;
                        let mut out =
                            common::retarget_launchd_plist(&content, &home, root, &want_bin);
                        if label == "org.loopstudio.router" {
                            let port = common::gateway_port().to_string();
                            match common::set_plist_string(&out, "LOOP_STUDIO_ROUTER_PORT", &port) {
                                Some(retargeted) => out = retargeted,
                                None => {
                                    eprintln!(
                                        "  [x] {} has no LOOP_STUDIO_ROUTER_PORT to retarget to :{} — not placing it",
                                        src, port
                                    );
                                    failures += 1;
                                    continue;
                                }
                            }
                        }
                        if label == "org.loopstudio.serve-mlx" {
                            // Retarget HF_HOME to the actual model library path.
                            // Without this the serve-mlx daemon uses a stale
                            // /Volumes/AIModels path on machines without the external volume.
                            let model_base = common::model_base();
                            let hf_home = format!("{}/hf", model_base);
                            let hf_hub = format!("{}/hf/hub", model_base);
                            if let Some(r) = common::set_plist_string(&out, "HF_HOME", &hf_home) {
                                out = r;
                            } else {
                                eprintln!("  [!] {} has no HF_HOME key — skipping retarget", src);
                            }
                            if let Some(r) = common::set_plist_string(&out, "HF_HUB_CACHE", &hf_hub) {
                                out = r;
                            } else {
                                eprintln!("  [!] {} has no HF_HUB_CACHE key — skipping retarget", src);
                            }
                        }
                        match common::atomic_write(&dst, &out) {
                            Ok(()) => println!("  [ok] Placed: {}", dst),
                            Err(e) => {
                                eprintln!("  [x] Write {} failed: {}", dst, e);
                                failures += 1;
                                continue;
                            }
                        }
                        launchctl_bootout(&dst, label);
                        if !launchctl_bootstrap(&dst) {
                            eprintln!(
                                "  [x] launchctl could not load {} — check `loop-studio logs`",
                                dst
                            );
                            failures += 1;
                        }
                    }
                    Err(e) => {
                        eprintln!("  [!] Source plist missing: {} ({})", src, e);
                        failures += 1;
                    }
                }
            }
            println!("Worker log: ~/Library/Logs/loop-studio-worker.log");
            println!("Router log: ~/Library/Logs/loop-studio-router.log");
            println!("MLX log:    ~/Library/Logs/loop-studio-serve-mlx.log");
            if failures > 0 {
                eprintln!("  [x] {} agent(s) did not install cleanly", failures);
                std::process::exit(1);
            }
        }
        "uninstall" => {
            println!("Unloading Loop Studio 24/7 Daemons...");
            let mut failures = 0;
            for label in ["org.loopstudio.worker", "org.loopstudio.serve-mlx", "org.loopstudio.router"] {
                let dst = format!("{}/{}.plist", agent_dir, label);
                if !launchctl_bootout(&dst, label) {
                    eprintln!("  [x] launchctl could not unload {label} — it may still be running");
                    failures += 1;
                }
                match fs::remove_file(&dst) {
                    Ok(()) => println!("  [ok] Removed {}", dst),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        eprintln!("  [x] Could not remove {}: {}", dst, e);
                        failures += 1;
                    }
                }
            }
            if failures > 0 {
                eprintln!("  [x] {} agent(s) did not uninstall cleanly", failures);
                std::process::exit(1);
            }
            println!("Loop Studio daemons uninstalled.");
        }
        "status" => {
            println!("================================================================");
            println!("  Loop Studio LAUNCHD DAEMON STATUS");
            println!("================================================================");
            let out = Command::new("launchctl")
                .arg("list")
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
                .unwrap_or_default();
            for label in ["org.loopstudio.router", "org.loopstudio.serve-mlx", "org.loopstudio.worker"] {
                let running = out.lines().any(|l| l.contains(label));
                let plist = format!("{}/{}.plist", agent_dir, label);
                let placed = fs::metadata(&plist).is_ok();
                println!(
                    "  - {} : {} ({})",
                    label,
                    if running {
                        "ACTIVE (running)"
                    } else {
                        "NOT LOADED"
                    },
                    if placed {
                        "plist placed"
                    } else {
                        "plist absent"
                    }
                );
            }
            println!("----------------------------------------------------------------");
            println!("  Worker: ~/Library/Logs/loop-studio-worker.log");
            println!("  Router: ~/Library/Logs/loop-studio-router.log");
            println!("================================================================");
        }
        _ => println!(
            "Unknown daemon command: {}. Use: install, uninstall, status",
            sub
        ),
    }
}

/// Run a sibling binary in the foreground and exit with its status.
pub fn exec_sibling(program: String, args: &[String]) -> ! {
    match Command::new(&program).args(args).status() {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("loop-studio: could not run {}: {}", program, e);
            std::process::exit(127);
        }
    }
}

// ─────────────────────────────────────────── Linux systemd user units ───────

/// Install Loop Studio as systemd user services on Linux.
///
/// Creates `~/.config/systemd/user/loop-studio-{router,worker}.service`
/// with KeepAlive semantics (Restart=always, RestartSec=10), log routing
/// to `~/.local/share/loop-studio/logs/`, and the same binary resolution
/// as the macOS launchd path.
#[cfg(target_os = "linux")]
fn install_systemd_units(root: &str, home: &str, _log_dir: &str) {
    let unit_dir = format!("{}/.config/systemd/user", home);
    let log_dir = format!("{}/.local/share/loop-studio/logs", home);
    if let Err(e) = fs::create_dir_all(&unit_dir) {
        eprintln!("  [x] Cannot create {}: {}", unit_dir, e);
        std::process::exit(1);
    }
    let _ = fs::create_dir_all(&log_dir);

    let installed_router = format!("{}/.local/bin/loop-studio-router", home);
    let release_router = common::bin(root, "loop-studio-router");
    let installed = format!("{}/.local/bin/loop-studio", home);
    let release_lac = common::bin(root, "loop-studio");

    let router_bin = common::choose_program_binary(&installed_router, &release_router);
    let worker_bin = common::choose_program_binary(&installed, &release_lac);

    let units: &[(&str, &str, String)] = &[
        (
            "loop-studio-router",
            "Loop Studio gateway router on :8000",
            router_bin.path.clone(),
        ),
        (
            "loop-studio-worker",
            "Loop Studio autonomous task worker",
            format!("{} worker", worker_bin.path),
        ),
    ];

    let mut failures = 0usize;
    for (name, desc, exec) in units {
        let unit = format!(
            "[Unit]\nDescription={desc}\nAfter=network.target\n\n\
             [Service]\nType=simple\nExecStart={exec}\nRestart=always\nRestartSec=10\n\
             StandardOutput=append:{log_dir}/{name}.log\n\
             StandardError=append:{log_dir}/{name}.err\n\n\
             [Install]\nWantedBy=default.target\n",
            desc = desc,
            exec = exec,
            log_dir = log_dir,
            name = name,
        );
        let dst = format!("{}/{}.service", unit_dir, name);
        match common::atomic_write(&dst, &unit) {
            Ok(()) => println!("  [ok] Placed: {}", dst),
            Err(e) => {
                eprintln!("  [x] Write {} failed: {}", dst, e);
                failures += 1;
                continue;
            }
        }
        // Enable and start: `systemctl --user daemon-reload && enable --now`
        let _ = Command::new("systemctl").args(["--user", "daemon-reload"]).status();
        let ok = Command::new("systemctl")
            .args(["--user", "enable", "--now", &format!("{}.service", name)])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            eprintln!(
                "  [x] systemctl could not enable {} — run: systemctl --user daemon-reload",
                name
            );
            failures += 1;
        }
    }
    println!("Router log: {}/loop-studio-router.log", log_dir);
    println!("Worker log: {}/loop-studio-worker.log", log_dir);
    if failures > 0 {
        eprintln!("  [x] {} unit(s) did not install cleanly", failures);
        std::process::exit(1);
    }
}
