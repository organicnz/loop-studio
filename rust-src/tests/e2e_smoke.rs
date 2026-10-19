mod support;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

struct Fixture {
    port: u16,
    stop: Arc<AtomicBool>,
    requests: Arc<AtomicUsize>,
    saw_authorization: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Fixture {
    fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("fixture listener");
        listener.set_nonblocking(true).expect("fixture nonblocking");
        let port = listener.local_addr().expect("fixture address").port();
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(AtomicUsize::new(0));
        let saw_authorization = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = Arc::clone(&stop);
            let requests = Arc::clone(&requests);
            let saw_authorization = Arc::clone(&saw_authorization);
            thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let requests = Arc::clone(&requests);
                            let saw_authorization = Arc::clone(&saw_authorization);
                            thread::spawn(move || {
                                serve(stream, requests, saw_authorization);
                            });
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        Self {
            port,
            stop,
            requests,
            saw_authorization,
            thread: Some(thread),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve(mut stream: TcpStream, requests: Arc<AtomicUsize>, saw_authorization: Arc<AtomicBool>) {
    // 10s, not 2s. This read timeout is a backstop against a wedged
    // connection, and at 2s it fired on a loaded machine *before* the client
    // had finished sending — the fixture then closed the socket with no
    // response and the router reported `llama invalid-response`, which reads
    // as a product failure and is not one.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let mut request = Vec::new();
    let mut buffer = [0u8; 4096];
    let head_end = loop {
        match stream.read(&mut buffer) {
            Ok(0) => return,
            Ok(n) => {
                request.extend_from_slice(&buffer[..n]);
                if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    break end + 4;
                }
                if request.len() > 64 * 1024 {
                    return;
                }
            }
            Err(_) => return,
        }
    };
    let head = String::from_utf8_lossy(&request[..head_end]).to_lowercase();
    if head.lines().any(|line| {
        line.split_once(':')
            .map(|(name, _)| {
                let name = name.trim();
                name.eq_ignore_ascii_case("authorization")
                    || name.eq_ignore_ascii_case("proxy-authorization")
            })
            .unwrap_or(false)
    }) {
        saw_authorization.store(true, Ordering::Release);
    }
    let content_length = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while request.len() < head_end + content_length {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => request.extend_from_slice(&buffer[..n]),
            Err(_) => return,
        }
    }
    requests.fetch_add(1, Ordering::AcqRel);
    let first_line = head.lines().next().unwrap_or_default();
    let path = first_line.split_whitespace().nth(1).unwrap_or("/");
    let (status, content_type, body) = if path.starts_with("/v1/models") {
        (
            "200 OK",
            "application/json",
            r#"{"object":"list","data":[{"id":"e2e-model"}]}"#.to_string(),
        )
    } else if path.starts_with("/v1/chat/completions") {
        (
            "200 OK",
            "application/json",
            r#"{"choices":[{"message":{"role":"assistant","content":"E2E_SENTINEL"}}]}"#.to_string(),
        )
    } else {
        (
            "404 Not Found",
            "application/json",
            r#"{"error":"not found"}"#.to_string(),
        )
    };
    let response = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        content_type,
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes());
}

fn test_binary(name: &str) -> String {
    if let Ok(directory) = std::env::var("LOOP_STUDIO_E2E_BIN_DIR") {
        return PathBuf::from(directory).join(name).to_string_lossy().into_owned();
    }
    match name {
        "loop-studio" => env!("CARGO_BIN_EXE_loop-studio").to_string(),
        "loop-studio-router" => env!("CARGO_BIN_EXE_loop-studio-router").to_string(),
        "kv-manage" => env!("CARGO_BIN_EXE_kv-manage").to_string(),
        other => panic!("unknown test binary {other}"),
    }
}

/// Run a binary with a deliberately broken environment: no `HOME` at all,
/// which is the state a launchd agent or a `sudo`-launched tool can be in.
fn run_without_home(binary: &str, args: &[&str]) -> (bool, i32, String) {
    let output = Command::new(binary)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("process");
    (
        output.status.success(),
        output.status.code().unwrap_or(-1),
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}


fn request(port: u16, method: &str, path: &str, body: &str) -> (u16, String) {
    let address = format!("127.0.0.1:{}", port);
    let mut stream = TcpStream::connect(&address).expect("gateway connection");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("gateway read timeout");
    let request = format!(
        "{} {} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        method,
        path,
        body.len(),
        body
    );
    stream.write_all(request.as_bytes()).expect("gateway request");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("gateway response");
    let text = String::from_utf8_lossy(&response).to_string();
    let status = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let body = text
        .find("\r\n\r\n")
        .map(|index| text[index + 4..].to_string())
        .unwrap_or(text);
    (status, body)
}

fn request_without_length(port: u16, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("gateway connection");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("gateway timeout");
    stream
        .write_all(
            format!("GET {} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n", path)
                .as_bytes(),
        )
        .expect("gateway request");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("gateway response");
    let text = String::from_utf8_lossy(&response).to_string();
    let status = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let body = text
        .find("\r\n\r\n")
        .map(|index| text[index + 4..].to_string())
        .unwrap_or(text);
    (status, body)
}

fn wait_for_router(port: u16, token: Option<&str>) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
            let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
            let auth = token
                .map(|value| format!("Authorization: Bearer {}\r\n", value))
                .unwrap_or_default();
            let request = format!(
                "GET /loop-studio/status HTTP/1.1\r\nHost: localhost\r\n{}Connection: close\r\n\r\n",
                auth
            );
            if stream.write_all(request.as_bytes()).is_ok() {
                let mut response = Vec::new();
                if stream.read_to_end(&mut response).is_ok()
                    && String::from_utf8_lossy(&response).contains("200 OK")
                {
                    return;
                }
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!("router did not become ready");
}

fn wait_for_models(port: u16, token: Option<&str>) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
            let _ = stream.set_read_timeout(Some(Duration::from_millis(800)));
            let auth = token
                .map(|value| format!("Authorization: Bearer {}\r\n", value))
                .unwrap_or_default();
            let request = format!(
                "GET /v1/models HTTP/1.1\r\nHost: localhost\r\n{}Connection: close\r\n\r\n",
                auth
            );
            if stream.write_all(request.as_bytes()).is_ok() {
                let mut response = Vec::new();
                if stream.read_to_end(&mut response).is_ok()
                    && response.starts_with(b"HTTP/1.1 200")
                {
                    return;
                }
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!("gateway models endpoint on :{} never became ready", port);
}

fn run_cli(binary: &str, home: &PathBuf, args: &[&str], extra_env: &[(&str, &str)]) -> (bool, String) {
    let mut command = Command::new(binary);
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .args(args)
        .env("HOME", home)
        // Only when the caller is not naming the port itself: a port the
        // caller chose must reach the CLI, and the "ephemeral" default below is
        // for the calls that are not about a port at all. `0` is not a usable
        // value here either — the CLI resolves this variable through the one
        // rule that drops it.
        .env(
            "LOOP_STUDIO_ROUTER_PORT",
            extra_env.first().map(|(_, value)| *value).unwrap_or("ephemeral"),
        )
        .env("LOOP_STUDIO_BIND_ADDR", "127.0.0.1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in extra_env.iter().skip(1) {
        command.env(key, value);
    }
    let output = command.output().expect("CLI process");
    (
        output.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}

#[test]
fn process_level_gateway_and_cli_smoke() {
    let _port_lock = support::PortLock::acquire();
    let fixture = Fixture::start();
    let dead_listener = TcpListener::bind(("127.0.0.1", 0)).expect("dead backend reservation");
    let dead_port = dead_listener.local_addr().expect("dead backend address").port();
    let home = std::env::temp_dir().join(format!(
        "loop-studio-e2e-home-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    std::fs::create_dir_all(&home).expect("temporary home");
    let router_bin = test_binary("loop-studio-router");
    let home_env = home.to_string_lossy().into_owned();
    let dead_env = dead_port.to_string();
    let llama_env = fixture.port.to_string();
    let router = support::spawn_router(
        &router_bin,
        &[
            ("HOME", &home_env),
            ("LOOP_STUDIO_BIND_ADDR", "127.0.0.1"),
            ("MLX_PORT", &dead_env),
            ("LOOP_STUDIO_LLAMA_PORT", &llama_env),
            ("LOOP_STUDIO_OLLAMA_PORT", &dead_env),
            ("LOOP_STUDIO_BACKEND", "llama"),
            ("LOOP_STUDIO_ROUTER_KEEPALIVE_SECS", "1"),
        ],
    );
    let router_port = router.port;
    wait_for_router(router_port, None);
    wait_for_models(router_port, None);

    let (status, body) = request(router_port, "GET", "/loop-studio/status", "");
    assert_eq!(status, 200);
    assert!(body.contains(&format!("\"port\": {}", fixture.port)));
    assert!(body.contains("\"up\": true"));
    // Host facts ride along in the status document the dashboard already
    // polls, so the client no longer spawns `loop-studio status --json` on every poll
    // for the thermal and RAM gauges. The block is measured on a timer, so it
    // appears a moment after the router starts rather than instantly.
    let mut saw_host = false;
    for _ in 0..20 {
        let (_, body) = request(router_port, "GET", "/loop-studio/status", "");
        if body.contains("\"host\": {") && body.contains("\"thermal\":") {
            saw_host = true;
            break;
        }
        thread::sleep(Duration::from_millis(500));
    }
    assert!(
        saw_host,
        "/loop-studio/status must carry host facts so the dashboard stops spawning a subprocess: {body}"
    );

    let (status, body) = request(router_port, "GET", "/v1/models", "");
    assert_eq!(status, 200);
    assert!(body.contains("e2e-model"));
    let (status, body) = request(router_port, "HEAD", "/v1/models", "");
    assert_eq!(status, 200);
    assert!(body.is_empty());

    let (status, _) = request_without_length(router_port, "/v1/unknown");
    assert_eq!(status, 502);

    let (status, body) = request(
        router_port,
        "POST",
        "/v1/chat/completions",
        r#"{"model":"e2e-model","messages":[{"role":"user","content":"smoke"}]}"#,
    );
    assert_eq!(status, 200);
    assert!(body.contains("E2E_SENTINEL"));

    let cli = test_binary("loop-studio");

    let (ok, output) = run_cli(
        &cli,
        &home,
        &["status", "--json"],
        &[("LOOP_STUDIO_ROUTER_PORT", &router_port.to_string())],
    );
    assert!(ok, "loop-studio status failed: {output}");
    assert!(output.contains("\"gateway\":true"));
    assert!(output.contains("\"active\":\"llama\""));

    // The CLI reads the router's host block instead of spawning pmset, vm_stat
    // and sysctl of its own when the gateway is up — the same thing the
    // dashboard does. Thermal is a string, so the router's cached reading and
    // what the CLI reports must agree exactly.
    let (_, status_now) = request(router_port, "GET", "/loop-studio/status", "");
    let router_thermal = status_now
        .split("\"thermal\": \"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap_or_default()
        .to_string();
    assert!(!router_thermal.is_empty(), "the router reported no thermal state");
    let cli_thermal = output
        .split("\"thermal\":\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap_or_default();
    assert_eq!(
        cli_thermal, router_thermal,
        "the CLI must report the router's host facts, not its own probes: {output}"
    );
    assert!(
        !output.contains("\"free_ram_gib\":null"),
        "the router's reading is available, so the CLI must not report a null it probed itself: {output}"
    );

    let (ok, output) = run_cli(
        &cli,
        &home,
        &["chat", "smoke"],
        &[
            ("LOOP_STUDIO_ROUTER_PORT", &router_port.to_string()),
            ("LOOP_STUDIO_CHAT_MODEL", "e2e-model"),
        ],
    );
    assert!(ok, "loop-studio chat failed: {output}");
    assert!(output.contains("E2E_SENTINEL"));

    let marker = home.join("injected");
    let (ok, output) = run_cli(
        &cli,
        &home,
        &["pull", "repo/'; touch injected; echo '"],
        &[("LOOP_STUDIO_ROUTER_PORT", "ephemeral"), ("PATH", "")],
    );
    assert!(!ok, "missing downloader must fail: {output}");
    assert!(!marker.exists(), "model input must never become executable source");

    let fake_bin = home.join("fake-bin");
    std::fs::create_dir_all(&fake_bin).expect("fake downloader directory");
    let fake_hf = fake_bin.join("hf");
    std::fs::write(&fake_hf, "#!/bin/sh\nexit 0\n").expect("fake downloader");
    let mut permissions = std::fs::metadata(&fake_hf).expect("fake downloader metadata").permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&fake_hf, permissions).expect("fake downloader permissions");
    let (ok, output) = run_cli(
        &cli,
        &home,
        &["pull", "org/model"],
        &[("LOOP_STUDIO_ROUTER_PORT", "ephemeral"), ("PATH", fake_bin.to_str().expect("fake PATH"))],
    );
    assert!(ok, "Hugging Face CLI success path failed: {output}");
    assert!(output.contains("Successfully downloaded"));

    assert!(fixture.requests.load(Ordering::Acquire) >= 3);
    drop(router);

    let token_router = support::spawn_router(
        &router_bin,
        &[
            ("HOME", &home_env),
            ("LOOP_STUDIO_BIND_ADDR", "127.0.0.1"),
            ("MLX_PORT", &dead_env),
            ("LOOP_STUDIO_LLAMA_PORT", &llama_env),
            ("LOOP_STUDIO_OLLAMA_PORT", &dead_env),
            ("LOOP_STUDIO_BACKEND", "llama"),
            ("LOOP_STUDIO_API_TOKEN", "e2e-secret"),
        ],
    );
    let token_port = token_router.port;
    wait_for_router(token_port, Some("e2e-secret"));
    wait_for_models(token_port, Some("e2e-secret"));
    let (ok, output) = run_cli(
        &cli,
        &home,
        &["status", "--json"],
        &[
            ("LOOP_STUDIO_ROUTER_PORT", &token_port.to_string()),
            ("LOOP_STUDIO_API_TOKEN", "e2e-secret"),
        ],
    );
    assert!(ok, "authenticated loop-studio status failed: {output}");
    let (ok, output) = run_cli(
        &cli,
        &home,
        &["chat", "authenticated smoke"],
        &[
            ("LOOP_STUDIO_ROUTER_PORT", &token_port.to_string()),
            ("LOOP_STUDIO_API_TOKEN", "e2e-secret"),
            ("LOOP_STUDIO_CHAT_MODEL", "e2e-model"),
        ],
    );
    assert!(ok, "authenticated loop-studio chat failed: {output}");
    assert!(output.contains("E2E_SENTINEL"));
    assert!(
        !fixture.saw_authorization.load(Ordering::Acquire),
        "router authorization must not reach the backend"
    );
    drop(token_router);

    // A loop that simulates its phases must not retire a real task, and a gate
    // that did not run must not read as a pass. Both used to be the other way
    // round: `--dry-run` printed "✓ Tests passed (100% pass rate)" with no
    // cargo in the process, then rewrote the queue to `status: complete`, so
    // `loop-studio worker` would never pick the task up again.
    let queue = home.join("todo/loop-studio-tasks.yaml");
    std::fs::create_dir_all(queue.parent().expect("queue parent")).expect("queue directory");
    std::fs::write(
        &queue,
        "- id: \"t-dry\"\n  task: \"REAL WORK: keep this pending\"\n  status: pending\n",
    )
    .expect("queue fixture");
    let (ok, output) = run_cli(
        &cli,
        &home,
        &["loop", "run", "t-dry", "--dry-run"],
        &[("LOOP_STUDIO_ROUTER_PORT", "ephemeral")],
    );
    assert!(!ok, "a gate that never ran must not exit 0: {output}");
    assert!(
        !output.contains("Tests passed"),
        "a dry run must not report a test result: {output}"
    );
    assert!(output.contains("GATE REJECTED"), "{output}");
    let after = std::fs::read_to_string(&queue).expect("queue readable");
    assert!(
        after.contains("status: pending"),
        "a simulation must leave the task in the queue: {after}"
    );


    // `--threshold 0` is a number the operator typed and it was never refused,
    // so `measured >= 0` was true for every measurement and the risk was RED
    // with `truncate now` regardless of the actual token count — a RED that
    // means nothing, printed by the one command whose job is to measure. The
    // asymmetry is inside a single function: `--keep 0` *is* rejected, a few
    // lines away in the same file.
    let kv = test_binary("kv-manage");
    for args in [vec!["check", "--threshold", "0"], vec!["truncate", "--keep", "0"]] {
        let output = Command::new(&kv)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &home)
            .args(&args)
            .output()
            .expect("kv-manage process");
        let body = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !output.status.success(),
            "a zero that has no meaning must be refused, not measured against: \
             {args:?} -> {body}"
        );
    }

    // A number the operator typed is either used or refused. `--tokens abc`
    // parsed to nothing, so `check` fell through to the router estimate and
    // printed it as a reading with `risk: GREEN` and exit 0 — from the one
    // command whose entire job is to measure. Exit 3 is this binary's own
    // documented usage-error code.
    let kv = test_binary("kv-manage");
    for (args, flag) in [
        (vec!["check", "--tokens", "abc"], "--tokens"),
        (vec!["check", "--threshold", "1e4"], "--threshold"),
        (vec!["truncate", "--keep", "eight"], "--keep"),
        (vec!["truncate", "--tokens", "-5"], "--tokens"),
    ] {
        let mut command = Command::new(&kv);
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &home)
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = command.output().expect("kv-manage process");
        let body = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !output.status.success(),
            "{args:?} must not exit 0 on an unparsable number: {body}"
        );
        assert_eq!(
            output.status.code(),
            Some(3),
            "{args:?} must exit 3 (usage error): {body}"
        );
        assert!(body.contains(flag), "{args:?} must name the bad flag: {body}");
        // Nothing may be reported as a reading when the input was refused.
        assert!(
            !body.contains("risk:") && !body.contains("\"risk\""),
            "{args:?} must not print a risk level it did not measure: {body}"
        );
        assert!(
            !body.contains("manifest:"),
            "{args:?} must not write a manifest from a refused input: {body}"
        );
    }

    // No home, no state path. The fallback used to be ".", so every path
    // became cwd-relative: `kv-manage truncate` built a `.lac/` tree in
    // whatever directory it ran in (launchd pins that to the checkout, which
    // .gitignore does not cover) and a later run from a different cwd read a
    // different queue. Refuse with a nonzero exit and a reason instead.
    let (ok, code, body) = run_without_home(&kv, &["truncate", "--tokens", "100", "--keep", "50"]);
    assert!(!ok, "a state write with no home must fail: {body}");
    assert_eq!(code, 78, "EX_CONFIG, and a reason on stderr: {body}");
    assert!(body.contains("home"), "{body}");

    let (ok, code, body) = run_without_home(&cli, &["loop", "list"]);
    assert!(!ok, "reading the queue with no home must fail: {body}");
    assert_eq!(code, 78, "{body}");
    assert!(body.contains("home"), "{body}");

    // The wrapper's exit status *is* the child's. Nine dispatch arms read
    // `let _ = ... .status()`, which made a missing binary and a failed child
    // both exit 0 — so `loop-studio kv check` could not return the 2 that RED is
    // documented to produce, and `make bootstrap` could not fail. Asserted on
    // the real binaries, because a unit test cannot spawn them.
    let (ok, code, body) = run_without_home(&cli, &["kv", "check", "--json"]);
    assert!(
        !ok,
        "with no home there is nothing to measure, so this must fail: {body}"
    );
    assert_ne!(code, 0, "a refused measurement must not exit 0: {body}");

    // `loop-studio loop validate` is a gate: `validate bad.yaml && deploy` deployed,
    // because the errors were printed and the status was 0. Warnings stay 0 —
    // they are notices, not findings — and a missing file is a failure.
    let loops = home.join("todo/loop-studio-loops");
    let _ = std::fs::create_dir_all(&loops);
    let bad = loops.join("bad.yaml");
    std::fs::write(&bad, "name: broken\n").expect("write bad loop");
    let (ok, body) = run_cli(&cli, &home, &["loop", "validate", bad.to_str().expect("path")], &[]);
    assert!(!ok, "a loop missing `phases:` must fail validation: {body}");
    assert!(body.contains("error(s)"), "and say so: {body}");

    let good = loops.join("good.yaml");
    std::fs::write(
        &good,
        "name: fine\nphases: [implement, review, apply, gate]\nmax_rounds: 2\nrequire_human_gate: true\n",
    )
    .expect("write good loop");
    let (ok, body) = run_cli(&cli, &home, &["loop", "validate", good.to_str().expect("path")], &[]);
    assert!(ok, "a valid loop must pass: {body}");

    let (ok, body) = run_cli(
        &cli,
        &home,
        &["loop", "validate", "/definitely/not/here.yaml"],
        &[],
    );
    assert!(!ok, "an unreadable loop file must fail: {body}");

    // `loop-studio logs` used to map any unrecognised name to the router's log and
    // print its path with no error, so `loop-studio logs wroker` looked like it worked.
    let (ok, body) = run_cli(&cli, &home, &["logs", "wroker"], &[]);
    assert!(!ok, "a mistyped log name must not silently tail the router log: {body}");
    assert!(body.contains("Known:"), "and must list the real names: {body}");
    // A real name still works, and the three `*.err` files are now reachable —
    // they were counted by `loop-studio doctor` but not nameable here, so the "reclaim
    // with `loop-studio logs <which> --purge`" hint named a reclaim nobody could type.
    for (name, file) in [
        ("router", "loop-studio-router.log"),
        ("worker", "loop-studio-worker.log"),
        ("mlx", "loop-studio-serve-mlx.log"),
        ("llama", "loop-studio-serve-llama.log"),
        ("router-err", "loop-studio-router.err"),
        ("worker-err", "loop-studio-worker.err"),
        ("mlx-err", "loop-studio-serve-mlx.err"),
        ("llama-err", "loop-studio-serve-llama.err"),
        // The literal file name, so an operator can paste what `ls` showed.
        ("loop-studio-serve-llama.err", "loop-studio-serve-llama.err"),
    ] {
        let (ok, body) = run_cli(&cli, &home, &["logs", name], &[]);
        assert!(ok, "log {name} must be reachable: {body}");
        assert!(
            body.contains(file),
            "log {name} must resolve to {file}, not another daemon's file: {body}"
        );
    }

    // `loop-studio bench --tokens 128` probed port 128. The help line advertises that
    // exact invocation, so the documented form of the command was a benchmark
    // of a port that is not the gateway.
    let (_ok, body) = run_cli(&cli, &home, &["bench", "--tokens", "128"], &[]);
    assert!(
        !body.contains("127.0.0.1:128"),
        "a flag's value must not be read as the port: {body}"
    );

    drop(fixture);
    let _ = std::fs::remove_dir_all(home);
}
