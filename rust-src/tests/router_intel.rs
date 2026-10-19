//! Router intelligence integration tests.
//!
//! Spins up the real `loop-studio-router` binary against in-process fake backends
//! on localhost and asserts merge, failover, model-hint routing, fastest
//! mode, and the usage log. One sequential #[test]: process env is global,
//! Child environments are explicit; scenarios run in order inside a single test.

mod support;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

fn read_headers(stream: &mut TcpStream) -> Vec<u8> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 65536 {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    buf
}

fn read_body(stream: &mut TcpStream, already: &[u8], len: usize) -> Vec<u8> {
    let head_end = already
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
        .unwrap_or(already.len());
    let mut body = already[head_end.min(already.len())..].to_vec();
    let mut chunk = [0u8; 4096];
    while body.len() < len {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => body.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    body.truncate(len);
    body
}

fn content_length(head: &[u8]) -> usize {
    let s = String::from_utf8_lossy(head).to_lowercase();
    for line in s.lines() {
        if let Some(v) = line.strip_prefix("content-length:") {
            if let Ok(n) = v.trim().parse() {
                return n;
            }
        }
    }
    0
}

struct Fake {
    hits_models: Arc<AtomicUsize>,
    hits_chat: Arc<AtomicUsize>,
    down: Arc<AtomicBool>,
    http_fail: Arc<AtomicBool>,
}

/// Serve a canned backend until the process exits (detached threads).
fn spawn_fake(ids: Vec<&'static str>, chat_delay_ms: u64) -> (Fake, u16) {
    let hits_models = Arc::new(AtomicUsize::new(0));
    let hits_chat = Arc::new(AtomicUsize::new(0));
    let down = Arc::new(AtomicBool::new(false));
    let hm = Arc::clone(&hits_models);
    let hc = Arc::clone(&hits_chat);
    let dn = Arc::clone(&down);
    let http_fail = Arc::new(AtomicBool::new(false));
    let hf = Arc::clone(&http_fail);
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("fake backend bind");
    let port = listener.local_addr().expect("fake backend address").port();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let hm = Arc::clone(&hm);
            let hc = Arc::clone(&hc);
            let dn = Arc::clone(&dn);
            let hf = Arc::clone(&hf);
            let ids = ids.clone();
            thread::spawn(move || {
                let mut s = stream;
                if dn.load(Ordering::SeqCst) {
                    return; // flap down: accept then hang up
                }
                let req = read_headers(&mut s);
                let head = String::from_utf8_lossy(&req);
                let path = head
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_string();
                let is_models = path == "/v1/models" || path.starts_with("/v1/models?");
                if !is_models {
                    let _ = read_body(&mut s, &req, content_length(&req));
                }
                if dn.load(Ordering::SeqCst) {
                    return;
                }
                let (body, code) = if is_models {
                    hm.fetch_add(1, Ordering::SeqCst);
                    let items: Vec<String> =
                        ids.iter().map(|id| format!("{{\"id\":\"{}\"}}", id)).collect();
                    (
                        format!("{{\"object\":\"list\",\"data\":[{}]}}", items.join(",")),
                        "200 OK",
                    )
                } else if path.starts_with("/v1/chat/completions") {
                    hc.fetch_add(1, Ordering::SeqCst);
                    if chat_delay_ms > 0 {
                        thread::sleep(Duration::from_millis(chat_delay_ms));
                    }
                    if hf.load(Ordering::SeqCst) {
                        ("{\"error\":\"backend failure\"}".to_string(), "500 Internal Server Error")
                    } else {
                        (
                            "{\"id\":\"t1\",\"object\":\"chat.completion\",\"choices\":[{\"message\":{\"content\":\"hello test\"}}]}".to_string(),
                            "200 OK",
                        )
                    }
                } else {
                    ("{\"error\":\"not found\"}".to_string(), "404 Not Found")
                };
                let resp = format!(
                    "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    code,
                    body.len(),
                    body
                );
                let _ = s.write_all(resp.as_bytes());
            });
        }
    });
    (
        Fake {
            hits_models,
            hits_chat,
            down,
            http_fail,
        },
        port,
    )
}

/// Raw HTTP over one connection; headers+body sent in a single write so
/// the router's zero-copy model sniff sees the full picture.
/// What one HTTP round-trip actually did, so a failure can name itself.
///
/// This harness used to return a `String` and let `split_response` reduce a
/// missing status line to code 0. That single number cannot tell three
/// different failures apart, and one of them is not the test's fault at all:
/// a connect error panics here, so a `0` means the router *accepted* the
/// connection and answered nothing — a per-connection failure, not a refused
/// connect and not a bad request. Observed once at
/// `/loop-studio/switch?target=fastest` inside a full `make validate`, not
/// reproducible in isolation or across repeated full runs, and the only
/// artefact of it was `0`. A test that can report "0" is a test that cannot be
/// debugged, so the distinction is made here rather than left to the next
/// person to rediscover.
/// What one HTTP round-trip actually did, so a failure can name itself.
#[derive(Debug)]
struct RoundTrip {
    /// The HTTP status, or 0 when there was no parseable status line.
    status: u16,
    /// The response body, or — when there was no response — the diagnosis.
    /// Every assertion in this file formats `{body}`, so this is the channel
    /// through which a failure explains itself.
    body: String,
}

impl RoundTrip {
    fn answered(status: u16, body: String, bytes: usize) -> Self {
        let _ = bytes;
        Self { status, body }
    }

    /// The connection was accepted and closed without a status line.
    ///
    /// This is the case that was unnameable before. The harness used to
    /// `.unwrap()` its connect, so a refused connect panicked; a `0` could
    /// therefore only mean *accepted, then silent* — a per-connection failure
    /// in the router, not a busy listener and not a bad request. Observed once
    /// at `/loop-studio/switch?target=fastest` inside a full `make validate`, not
    /// reproducible in isolation or across repeated full runs. It is never
    /// retried: if the router can accept and then answer nothing, that is the
    /// thing worth catching, and a retry would hide exactly it.
    fn accepted_then_silent(bytes: usize) -> Self {
        Self {
            status: 0,
            body: format!(
                "the router accepted the connection and answered nothing ({bytes} bytes, \
                 no status line) — a per-connection failure, not a refused connect"
            ),
        }
    }

    fn unreachable(attempts: usize, last: &str) -> Self {
        Self {
            status: 0,
            body: format!("connect failed {attempts} time(s): {last}"),
        }
    }
}

/// One request/response, retrying only a *connect* that was refused.
///
/// A refused connect is a machine event — the listener was momentarily busy, or
/// a fixture had not finished binding — and asserting on a single attempt makes
/// the test report the scheduler's mood. A connection that was *accepted* and
/// then answered nothing is a different thing entirely and is never retried:
/// that would be the router's behaviour, and hiding it is the one outcome worth
/// ruling out.
fn raw_request(port: u16, text: &str) -> RoundTrip {
    const CONNECT_ATTEMPTS: usize = 5;
    let addr = format!("127.0.0.1:{port}").parse().expect("loopback address");
    let mut last_connect_err = String::new();
    for attempt in 1..=CONNECT_ATTEMPTS {
        let mut s = match TcpStream::connect_timeout(&addr, Duration::from_secs(5)) {
            Ok(s) => s,
            Err(e) => {
                last_connect_err = e.to_string();
                if attempt < CONNECT_ATTEMPTS {
                    std::thread::sleep(Duration::from_millis(200 * attempt as u64));
                }
                continue;
            }
        };
        let _ = s.set_read_timeout(Some(Duration::from_secs(15)));
        if let Err(e) = s.write_all(text.as_bytes()) {
            return RoundTrip::unreachable(attempt, &format!("write failed after connect: {e}"));
        }
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out);
        let raw = String::from_utf8_lossy(&out).into_owned();
        let (code, body) = split_response(&raw);
        return if code == 0 {
            RoundTrip::accepted_then_silent(out.len())
        } else {
            RoundTrip::answered(code, body, out.len())
        };
    }
    RoundTrip::unreachable(CONNECT_ATTEMPTS, &last_connect_err)
}

fn get(port: u16, path: &str) -> (u16, String) {
    let rt = raw_request(
        port,
        &format!("GET {} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n", path),
    );
    (rt.status, rt.body)
}

fn post_chat(port: u16, model: &str) -> (u16, String) {
    let body = format!("{{\"model\":\"{}\",\"messages\":[{{\"role\":\"user\",\"content\":\"hi\"}}]}}", model);
    let req = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let rt = raw_request(port, &req);
    (rt.status, rt.body)
}

fn post_chat_split_body(port: u16, model: &str) -> (u16, String) {
    let body = format!(
        "{{\"model\":\"{}\",\"messages\":[{{\"role\":\"user\",\"content\":\"{}\"}}]}}",
        model,
        "x".repeat(32_000)
    );
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("gateway connection");
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .expect("gateway timeout");
    let request_head = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(request_head.as_bytes())
        .expect("request head");
    thread::sleep(Duration::from_millis(100));
    let split = body.len() / 2;
    stream
        .write_all(&body.as_bytes()[..split])
        .expect("request body prefix");
    thread::sleep(Duration::from_millis(100));
    stream
        .write_all(&body.as_bytes()[split..])
        .expect("request body suffix");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("gateway response");
    split_response(&String::from_utf8_lossy(&response))
}

fn split_response(resp: &str) -> (u16, String) {
    let code = resp
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let body = resp.find("\r\n\r\n").map(|i| resp[i + 4..].to_string()).unwrap_or_default();
    (code, body)
}

fn wait_router(port: u16) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(15) {
        if let Ok(mut s) = TcpStream::connect_timeout(
            &format!("127.0.0.1:{}", port).parse().unwrap(),
            Duration::from_millis(300),
        ) {
            let _ = s.set_read_timeout(Some(Duration::from_millis(800)));
            let _ = s.write_all(b"GET /loop-studio/status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
            let mut buf = Vec::new();
            let _ = s.read_to_end(&mut buf);
            if String::from_utf8_lossy(&buf).contains("200 OK") {
                return;
            }
        }
        thread::sleep(Duration::from_millis(200));
    }
    panic!("router on :{} never became ready", port);
}

#[test]
fn router_intelligence() {
    let _port_lock = support::PortLock::acquire();
    // Isolate HOME: the router persists its backend choice and usage log
    // under ~/.lac — the test must never rewrite the operator's files.
    // (All scenarios run sequentially in this one test; env is global.)
    let tmp_home = std::env::temp_dir().join(format!("loop-studio-test-home-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&tmp_home);
    let (mlx, mlx_port) = spawn_fake(vec!["t-m1", "t-dup"], 0);
    let (llama, llama_port) = spawn_fake(vec!["t-m2", "t-dup"], 0);
    let dead_listener = TcpListener::bind(("127.0.0.1", 0)).expect("ollama dead reservation");
    let ollama_port = dead_listener.local_addr().expect("ollama dead address").port();

    let router_bin = env!("CARGO_BIN_EXE_loop-studio-router");
    let home_env = tmp_home.to_string_lossy().into_owned();
    let mlx_env = mlx_port.to_string();
    let llama_env = llama_port.to_string();
    let ollama_env = ollama_port.to_string();
    let router = support::spawn_router(
        router_bin,
        &[
            ("HOME", &home_env),
            ("LOOP_STUDIO_BIND_ADDR", "127.0.0.1"),
            ("MLX_PORT", &mlx_env),
            ("LOOP_STUDIO_LLAMA_PORT", &llama_env),
            ("LOOP_STUDIO_OLLAMA_PORT", &ollama_env),
        ],
    );
    let router_port = router.port;
    wait_router(router_port);

    // 1. Merged catalog: union with first-wins dedupe.
    let (code, body) = get(router_port, "/v1/models");
    assert_eq!(code, 200, "models body: {}", body);
    assert!(body.contains("t-m1"), "mlx model present: {}", body);
    assert!(body.contains("t-m2"), "llama model present: {}", body);
    assert_eq!(body.matches("t-dup").count(), 1, "dup deduped: {}", body);

    // 2. Model hint: t-m2 was mapped to llama by the merge above.
    for _ in 0..3 {
        let (code, _) = post_chat(router_port, "t-m2");
        assert_eq!(code, 200);
        if llama.hits_chat.load(Ordering::SeqCst) >= 1 {
            break;
        }
    }
    assert!(
        llama.hits_chat.load(Ordering::SeqCst) >= 1,
        "hinted model reaches its backend"
    );

    // 3. Failover: flap mlx down, wait out the 1s health cache, re-probe.
    mlx.down.store(true, Ordering::SeqCst);
    thread::sleep(Duration::from_millis(2000));
    let before = llama.hits_chat.load(Ordering::SeqCst);
    let (code, _) = post_chat(router_port, "t-m1");
    assert_eq!(code, 200);
    assert!(
        llama.hits_chat.load(Ordering::SeqCst) > before,
        "traffic fails over to llama"
    );
    mlx.down.store(false, Ordering::SeqCst);

    mlx.http_fail.store(true, Ordering::SeqCst);
    let before_http_failover = llama.hits_chat.load(Ordering::SeqCst);
    let (code, body) = post_chat(router_port, "unmapped-model");
    assert_eq!(code, 200, "HTTP 500 backend must fail over: {body}");
    assert!(llama.hits_chat.load(Ordering::SeqCst) > before_http_failover);
    let before_split = llama.hits_chat.load(Ordering::SeqCst);
    let (code, body) = post_chat_split_body(router_port, "large-unmapped-model");
    assert_eq!(code, 200, "split request body must fail over intact: {body}");
    assert!(llama.hits_chat.load(Ordering::SeqCst) > before_split);
    mlx.http_fail.store(false, Ordering::SeqCst);

    // 4. Fastest mode + stats surface.
    let (code, _) = get(router_port, "/loop-studio/switch?target=fastest");
    assert_eq!(code, 200);
    thread::sleep(Duration::from_millis(2000));
    let (code, status) = get(router_port, "/loop-studio/status");
    assert_eq!(code, 200);
    assert!(status.contains("\"preferred\": \"fastest\""), "{}", status);
    assert!(status.contains("\"ewma_ms\""), "stats block: {}", status);
    assert!(status.contains("\"models_mapped\""), "{}", status);
    drop(mlx.hits_models);

    // The pre-rename paths still answer, so an operator's existing script or
    // bookmarked dashboard URL keeps working across the rename.
    let (code, legacy_status) = get(router_port, "/lac/status");
    assert_eq!(code, 200, "legacy status alias must still answer: {}", legacy_status);
    assert!(legacy_status.contains("\"preferred\": \"fastest\""), "{}", legacy_status);
    let (code, _) = get(router_port, "/lac/switch?target=auto");
    assert_eq!(code, 200, "legacy switch alias must still answer");

    let usage = std::fs::read_to_string(tmp_home.join(".lac/router-usage.jsonl"))
        .expect("usage log exists");
    assert!(usage.contains("\"outcome\":\"proxied\""), "{}", &usage[usage.len().saturating_sub(400)..]);

    drop(router);

    // 5. The two replies that used to state something the router had not
    // measured. Both are read by `loop-studio status`, the Studio dashboard and any
    // script watching this endpoint, so both are load-bearing.
    //
    // Every backend down: `/loop-studio/status` used to answer `"status": "ok"` and
    // name an active lane — the preferred id, which is the literal string
    // "auto" by default — on a port nothing was bound to, in the same document
    // whose `backends` block said all three were down.
    let honest_home = std::env::temp_dir().join(format!("loop-studio-test-honest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&honest_home);
    std::fs::create_dir_all(&honest_home).expect("honest home");
    // Reserve then release, so connect() is refused at once instead of
    // succeeding into the kernel backlog and stalling the health probe.
    let dead = || {
        let reservation = TcpListener::bind(("127.0.0.1", 0)).expect("dead reservation");
        reservation.local_addr().expect("dead address").port()
    };
    let (dead_mlx, dead_llama, dead_ollama) = (dead(), dead(), dead());
    let honest_home_env = honest_home.to_string_lossy().into_owned();
    let honest_mlx = dead_mlx.to_string();
    let honest_llama = dead_llama.to_string();
    let honest_ollama = dead_ollama.to_string();
    let honest = support::spawn_router(
        router_bin,
        &[
            ("HOME", &honest_home_env),
            ("LOOP_STUDIO_BIND_ADDR", "127.0.0.1"),
            ("MLX_PORT", &honest_mlx),
            ("LOOP_STUDIO_LLAMA_PORT", &honest_llama),
            ("LOOP_STUDIO_OLLAMA_PORT", &honest_ollama),
        ],
    );
    let honest_port = honest.port;
    wait_router(honest_port);
    let (code, status) = get(honest_port, "/loop-studio/status");
    assert_eq!(code, 200);
    assert!(
        status.contains("\"status\": \"no-backends\""),
        "a gateway with nothing to serve must not say ok: {status}"
    );
    assert!(
        status.contains("\"active\": \"none\"") && status.contains("\"target_port\": 0"),
        "and must not name a lane: {status}"
    );
    assert!(
        status.contains("\"up\": false"),
        "the measurement beside the claim: {status}"
    );

    // A switch that could not be written. The reply used to say
    // `"persisted": true` with the write's error discarded, so a read-only or
    // full `~/.lac` lost the operator's choice silently while the dashboard
    // showed it as saved. A directory where the file belongs makes the rename
    // fail for any user, root included.
    std::fs::create_dir_all(honest_home.join(".lac/router-backend")).expect("block the write");
    let (code, switched) = get(honest_port, "/loop-studio/switch?target=llama");
    assert_eq!(code, 200);
    assert!(
        switched.contains("\"target\": \"llama\""),
        "the switch itself still takes effect: {switched}"
    );
    assert!(
        switched.contains("\"persisted\": false"),
        "and the reply must admit it was not saved: {switched}"
    );
    let (_, after) = get(honest_port, "/loop-studio/status");
    assert!(
        after.contains("\"preferred\": \"llama\""),
        "the in-memory choice is real even when the file could not be written: {after}"
    );
    drop(honest);
    let _ = std::fs::remove_dir_all(&honest_home);
    let _ = std::fs::remove_dir_all(&tmp_home);
}
