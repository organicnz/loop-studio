//! Remote-auth regression tests for `loop-studio-router` v2.8.
//!
//! Covers the fail-closed contract from docs/ARCHITECTURE.md:
//!   1. non-loopback bind (`LOOP_STUDIO_BIND_ADDR=0.0.0.0`) without `LOOP_STUDIO_API_TOKEN`
//!      refuses to start (exit 1, "Refusing" on stderr);
//!   2. loopback bind without a token stays open (`GET /loop-studio/status` -> 200).
//! Plus the browser-facing gates, which are a second fail-closed contract:
//!   3. an un-allow-listed `Origin` is refused (403) and a rebound `Host` is
//!      refused (421), while native clients and allow-listed ones still work.
//! One sequential #[test] per router: child environments are explicit and
//! isolated, and the port lock keeps them from overlapping.

mod support;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

/// A full response as (status, headers, body).
fn raw_request(port: u16, request: &str) -> (u16, String, String) {
    let addr = format!("127.0.0.1:{}", port).parse().expect("addr");
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(500)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_millis(2500)))
        .expect("read timeout");
    stream.write_all(request.as_bytes()).expect("write");
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    let text = String::from_utf8_lossy(&response).into_owned();
    let (head, body) = text
        .split_once("\r\n\r\n")
        .map(|(h, b)| (h.to_string(), b.to_string()))
        .unwrap_or((text.clone(), String::new()));
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .unwrap_or(0);
    (status, head, body)
}

fn status_of(port: u16, request: &str) -> u16 {
    raw_request(port, request).0
}

/// A port nothing is listening on, reserved and then released, so a connect
/// attempt gets an immediate ECONNREFUSED instead of succeeding against the
/// kernel backlog and then timing out on the read.
fn dead_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("reserve a port");
    let port = listener.local_addr().expect("reserved address").port();
    drop(listener);
    port
}

fn wait_status(port: u16, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Ok(mut s) = TcpStream::connect_timeout(
            &format!("127.0.0.1:{}", port).parse().unwrap(),
            Duration::from_millis(300),
        ) {
            s.set_read_timeout(Some(Duration::from_millis(2500))).ok();
            let _ = s.write_all(b"GET /loop-studio/status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
            let mut buf = Vec::new();
            let _ = s.read_to_end(&mut buf);
            if String::from_utf8_lossy(&buf).contains("200 OK") {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

fn request_status(port: u16, authorization: Option<&str>, forwarded: bool) -> Option<u16> {
    let addr = format!("127.0.0.1:{}", port).parse().ok()?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(500)).ok()?;
    stream.set_read_timeout(Some(Duration::from_millis(2500))).ok()?;
    let auth = authorization
        .map(|token| format!("Authorization: Bearer {}\r\n", token))
        .unwrap_or_default();
    let forwarded = if forwarded { "X-Forwarded-For: 100.64.0.5\r\n" } else { "" };
    let request = format!(
        "GET /loop-studio/status HTTP/1.1\r\nHost: localhost\r\n{}{}Connection: close\r\n\r\n",
        forwarded, auth
    );
    stream.write_all(request.as_bytes()).ok()?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).ok()?;
    String::from_utf8_lossy(&response)
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// Send a request whose auth-relevant headers are supplied verbatim, so the
/// odd shapes (wrong scheme, trailing junk, duplicates, other header names)
/// can be exercised end to end rather than only in unit tests.
fn request_with_headers(port: u16, extra: &str) -> Option<u16> {
    let addr = format!("127.0.0.1:{}", port).parse().ok()?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(500)).ok()?;
    stream.set_read_timeout(Some(Duration::from_millis(2500))).ok()?;
    let request = format!(
        "GET /loop-studio/status HTTP/1.1\r\nHost: localhost\r\n{}Connection: close\r\n\r\n",
        extra
    );
    stream.write_all(request.as_bytes()).ok()?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).ok()?;
    String::from_utf8_lossy(&response)
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

#[test]
fn remote_auth_fail_closed_and_loopback_open() {
    let _port_lock = support::PortLock::acquire();
    let tmp_home = std::env::temp_dir().join(format!("loop-studio-test-auth-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&tmp_home);
    let router_bin = env!("CARGO_BIN_EXE_loop-studio-router");
    let dead_listener = TcpListener::bind(("127.0.0.1", 0)).expect("dead backend reservation");
    let dead_port = dead_listener.local_addr().expect("dead backend address").port();
    let dead_env = dead_port.to_string();
    let home_env = tmp_home.to_string_lossy().into_owned();

    // 1. Fail closed: 0.0.0.0 without a token must exit 1 with "Refusing".
    let mut refusal = std::process::Command::new(router_bin);
    refusal
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &home_env)
        .env("LOOP_STUDIO_BIND_ADDR", "0.0.0.0")
        .env("LOOP_STUDIO_ROUTER_PORT", "ephemeral")
        .env("MLX_PORT", &dead_env)
        .env("LOOP_STUDIO_LLAMA_PORT", &dead_env)
        .env("LOOP_STUDIO_OLLAMA_PORT", &dead_env);
    let out = refusal.output().expect("router binary runs");
    assert_eq!(out.status.code(), Some(1), "non-loopback bind without token must exit 1");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("Refusing"),
        "stderr must explain refusal: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // 2. Loopback without a token stays open for local dev.
    let open_router = support::spawn_router(
        router_bin,
        &[
            ("HOME", &home_env),
            ("LOOP_STUDIO_BIND_ADDR", "127.0.0.1"),
            ("MLX_PORT", &dead_env),
            ("LOOP_STUDIO_LLAMA_PORT", &dead_env),
            ("LOOP_STUDIO_OLLAMA_PORT", &dead_env),
        ],
    );
    let ok = wait_status(open_router.port, Duration::from_secs(15));
    assert!(ok, "loopback without token must serve /loop-studio/status 200");

    let token_router = support::spawn_router(
        router_bin,
        &[
            ("HOME", &home_env),
            ("LOOP_STUDIO_BIND_ADDR", "127.0.0.1"),
            ("LOOP_STUDIO_API_TOKEN", "test-secret"),
            ("MLX_PORT", &dead_env),
            ("LOOP_STUDIO_LLAMA_PORT", &dead_env),
            ("LOOP_STUDIO_OLLAMA_PORT", &dead_env),
        ],
    );
    let token_port = token_router.port;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10)
        && request_status(token_port, Some("test-secret"), false) != Some(200)
    {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(request_status(token_port, None, false), Some(401));
    assert_eq!(request_status(token_port, Some("wrong"), false), Some(401));
    assert_eq!(request_status(token_port, Some("test-secret"), false), Some(200));
    assert_eq!(request_status(token_port, Some("test-secret"), true), Some(200));
    assert_eq!(request_status(token_port, None, true), Some(401));

    // Odd header shapes, end to end. A valid token presented the wrong way
    // must not open the gate, and must not be accepted as a prefix.
    assert_eq!(
        request_with_headers(token_port, "Authorization: Basic test-secret\r\n"),
        Some(401),
        "only Bearer may carry the token"
    );
    assert_eq!(
        request_with_headers(token_port, "Authorization: Bearer test-secret extra\r\n"),
        Some(401),
        "the token is taken whole, not as a prefix"
    );
    assert_eq!(
        request_with_headers(token_port, "Authorization: Bearer \r\n"),
        Some(401),
        "an empty bearer is not a token"
    );
    assert_eq!(
        request_with_headers(token_port, "Proxy-Authorization: Bearer test-secret\r\n"),
        Some(401),
        "Proxy-Authorization must not authenticate"
    );
    assert_eq!(
        request_with_headers(
            token_port,
            "Authorization: Bearer wrong\r\nAuthorization: Bearer test-secret\r\n"
        ),
        Some(401),
        "a wrong first credential must not fall through to a later right one"
    );
    assert_eq!(
        request_with_headers(token_port, "authorization: Bearer test-secret\r\n"),
        Some(200),
        "the field name is case-insensitive"
    );
    assert_eq!(
        request_with_headers(token_port, "AUTHORIZATION: BEARER test-secret\r\n"),
        Some(200),
        "so is the scheme"
    );
    let remote_router = support::spawn_router(
        router_bin,
        &[
            ("HOME", &home_env),
            ("LOOP_STUDIO_BIND_ADDR", "0.0.0.0"),
            ("LOOP_STUDIO_API_TOKEN", "test-secret"),
            ("MLX_PORT", &dead_env),
            ("LOOP_STUDIO_LLAMA_PORT", &dead_env),
            ("LOOP_STUDIO_OLLAMA_PORT", &dead_env),
        ],
    );
    let remote_port = remote_router.port;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10)
        && request_status(remote_port, Some("test-secret"), false) != Some(200)
    {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(request_status(remote_port, None, false), Some(401));
    assert_eq!(request_status(remote_port, Some("test-secret"), false), Some(200));
    drop(remote_router);
    drop(token_router);
    drop(open_router);
    let _ = std::fs::remove_dir_all(&tmp_home);
}

/// The two browser-facing gates, end to end against a real router process.
///
/// A loopback gateway is reachable by every web page the user has open. Two
/// attacks follow, and each needs its own control:
///
///   * **Drive-by CORS** — the responses used to carry
///     `Access-Control-Allow-Origin: *`, so a page on any origin could
///     `fetch` the gateway and *read* the reply: `/loop-studio/status` (backend
///     topology, tailnet addresses, measured latency), `/loop-studio/switch` (which
///     backend the gateway uses), and the completions themselves.
///   * **DNS rebinding** — the attacker rebinds their domain to 127.0.0.1, at
///     which point the browser considers the gateway *same-origin*: no
///     `Origin` is sent, CORS does not apply, and the first control is
///     bypassed outright. Only `Host` still carries the attacker's name.
///
/// So the tests assert both, and then that the legitimate clients — which
/// send no `Origin` and dial an IP — are untouched.
#[test]
fn browser_origins_and_rebound_hosts_are_refused() {
    let _port_lock = support::PortLock::acquire();
    let tmp_home = std::env::temp_dir().join(format!("loop-studio-test-browser-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&tmp_home);
    let router_bin = env!("CARGO_BIN_EXE_loop-studio-router");
    let dead = dead_port().to_string();
    let home_env = tmp_home.to_string_lossy().into_owned();

    let router = support::spawn_router(
        router_bin,
        &[
            ("HOME", &home_env),
            ("LOOP_STUDIO_BIND_ADDR", "127.0.0.1"),
            ("MLX_PORT", &dead),
            ("LOOP_STUDIO_LLAMA_PORT", &dead),
            ("LOOP_STUDIO_OLLAMA_PORT", &dead),
        ],
    );
    let port = router.port;
    assert!(
        wait_status(port, Duration::from_secs(15)),
        "router must serve /loop-studio/status to a native client"
    );

    // --- the native client path, which must not regress -------------------
    // No Origin, and a Host the router serves.
    for host in [
        format!("127.0.0.1:{port}"),
        "localhost".to_string(),
        format!("localhost:{port}"),
        "[::1]:8000".to_string(),
    ] {
        let request =
            format!("GET /loop-studio/status HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
        let (status, head, body) = raw_request(port, &request);
        assert_eq!(status, 200, "Host: {host} must be served: {head}");
        // The real status document, not a refusal. Its `status` field is not
        // asserted here: this fixture points every backend port at the router's
        // own port, so nothing is serving and the router now says
        // `no-backends` — the honesty this suite is not about, and which
        // `router_intel` pins directly.
        assert!(
            body.contains("\"router\": \"loop-studio-router") && body.contains("\"backends\""),
            "Host: {host} must get the real status body: {body}"
        );
        // The load-bearing assertion for every native client: no CORS header
        // at all, so there is no browser-readable surface to inherit.
        assert!(
            !head.to_ascii_lowercase().contains("access-control"),
            "a native client must not be handed a CORS header: {head}"
        );
    }
    // HTTP/1.0 sends no Host at all, and every browser sends one, so its
    // absence is not a rebinding vector.
    assert_eq!(
        status_of(port, "GET /loop-studio/status HTTP/1.0\r\nConnection: close\r\n\r\n"),
        200
    );

    // --- drive-by CORS: refused, and unreadable ---------------------------
    let (status, head, body) = raw_request(
        port,
        "GET /loop-studio/status HTTP/1.1\r\nHost: localhost\r\nOrigin: https://evil.example\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status, 403, "an un-allow-listed Origin must be refused: {head}");
    assert!(
        !head.to_ascii_lowercase().contains("access-control"),
        "the refusal must not be readable by the page that provoked it: {head}"
    );
    assert!(
        !body.contains("uptime_secs"),
        "the refused page must not learn anything about the gateway: {body}"
    );

    // A state-changing endpoint must be refused too — this one moves the
    // gateway's active backend, so a drive-by page could otherwise repoint
    // the whole cluster with a GET.
    assert_eq!(
        status_of(
            port,
            "GET /loop-studio/switch?target=ollama HTTP/1.1\r\nHost: localhost\r\nOrigin: https://evil.example\r\nConnection: close\r\n\r\n"
        ),
        403,
        "a browser must not be able to switch the backend"
    );
    // And it really did not switch: the preference is still auto.
    let (_, _, after) = raw_request(
        port,
        "GET /loop-studio/status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(
        after.contains("\"preferred\": \"auto\""),
        "the refused /loop-studio/switch must have changed nothing: {after}"
    );

    // The same drive-by without an `Origin`, which is the shape that actually
    // gets through: a `mode: "no-cors"` subresource load is not a CORS request,
    // so the browser appends no preflight and no `Origin`. Nothing above can
    // see it, and `Sec-Fetch-Site` is the only header left that says where the
    // request came from.
    assert_eq!(
        status_of(
            port,
            "GET /loop-studio/switch?target=ollama HTTP/1.1\r\nHost: localhost\r\nSec-Fetch-Site: cross-site\r\nConnection: close\r\n\r\n"
        ),
        403,
        "a cross-site subresource load must not be able to switch the backend"
    );
    let (_, _, after) = raw_request(
        port,
        "GET /loop-studio/status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(
        after.contains("\"preferred\": \"auto\""),
        "the originless cross-site /loop-studio/switch must have changed nothing: {after}"
    );

    // The gate must cost the native callers nothing, so the negative case above
    // is only meaningful if the positive one still works. curl, `loop-studio` and the
    // TUI send no `Sec-Fetch-Site` at all, and a same-origin fetch sends
    // `same-origin`; both must still be served. `auto` is used as the target so
    // this leaves the preference where the assertions above expect it.
    assert_eq!(
        status_of(
            port,
            "GET /loop-studio/switch?target=auto HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
        ),
        200,
        "a native client with no Sec-Fetch-Site must still be able to switch"
    );
    assert_eq!(
        status_of(
            port,
            "GET /loop-studio/switch?target=auto HTTP/1.1\r\nHost: localhost\r\nSec-Fetch-Site: same-origin\r\nConnection: close\r\n\r\n"
        ),
        200,
        "a same-origin fetch must still be able to switch"
    );

    // The preflight is refused by the same allow-list, so the browser never
    // gets to send the real request.
    let (status, head, _) = raw_request(
        port,
        "OPTIONS /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\nOrigin: https://evil.example\r\nAccess-Control-Request-Method: POST\r\n\r\n",
    );
    assert_eq!(status, 403, "preflight from a refused origin: {head}");
    assert!(!head.to_ascii_lowercase().contains("access-control"), "{head}");

    // A sandboxed iframe or a file:// page sends the literal `null` origin.
    // It is not on any allow-list, so it is refused like any other.
    assert_eq!(
        status_of(
            port,
            "GET /loop-studio/status HTTP/1.1\r\nHost: localhost\r\nOrigin: null\r\nConnection: close\r\n\r\n"
        ),
        403
    );

    // --- DNS rebinding: no Origin, attacker-chosen Host -------------------
    let (status, head, body) = raw_request(
        port,
        &format!(
            "POST /loop-studio/switch?target=ollama HTTP/1.1\r\nHost: evil.example:{port}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ),
    );
    assert_eq!(
        status, 421,
        "a rebound Host must be refused: {head}\n{body}"
    );
    assert!(!body.contains("switched"), "no state may change: {body}");
    let (_, _, after) = raw_request(
        port,
        "GET /loop-studio/status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(
        after.contains("\"preferred\": \"auto\""),
        "the rebound /loop-studio/switch must have changed nothing: {after}"
    );

    // A second Host must not be able to contradict the first, in either
    // direction: the refused one first is refused, the served one first is
    // served.
    assert_eq!(
        status_of(
            port,
            &format!(
                "GET /loop-studio/status HTTP/1.1\r\nHost: evil.example\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
            )
        ),
        421
    );
    assert_eq!(
        status_of(
            port,
            &format!("GET /loop-studio/status HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nHost: evil.example\r\nConnection: close\r\n\r\n")
        ),
        200
    );

    drop(router);
    let _ = std::fs::remove_dir_all(&tmp_home);
}

/// The escape hatches: an operator allow-list, and a valid Bearer token as
/// proof of a non-browser client. Both exist so that legitimate remote access
/// keeps working; both are asserted here so they cannot be removed as
/// "unused" later.
#[test]
fn operator_allowlists_and_a_token_reopen_exactly_what_they_name() {
    let _port_lock = support::PortLock::acquire();
    let tmp_home = std::env::temp_dir().join(format!("loop-studio-test-allow-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&tmp_home);
    let router_bin = env!("CARGO_BIN_EXE_loop-studio-router");
    let dead = dead_port().to_string();
    let home_env = tmp_home.to_string_lossy().into_owned();

    // --- Host allow-list -------------------------------------------------
    let hosts_router = support::spawn_router(
        router_bin,
        &[
            ("HOME", &home_env),
            ("LOOP_STUDIO_BIND_ADDR", "127.0.0.1"),
            ("LOOP_STUDIO_ALLOWED_HOSTS", "studio.local, box.tail1234.ts.net"),
            ("MLX_PORT", &dead),
            ("LOOP_STUDIO_LLAMA_PORT", &dead),
            ("LOOP_STUDIO_OLLAMA_PORT", &dead),
        ],
    );
    let hosts_port = hosts_router.port;
    assert!(
        wait_status(hosts_port, Duration::from_secs(15)),
        "router must start with a Host allow-list"
    );
    for host in [
        "studio.local",
        "studio.local:8000",
        "STUDIO.local",
        "box.tail1234.ts.net:8000",
    ] {
        let request =
            format!("GET /loop-studio/status HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
        assert_eq!(
            status_of(hosts_port, &request),
            200,
            "allow-listed Host {host} must be served"
        );
    }
    for host in [
        "evil.example",
        // An allow-list entry is a whole name, never a suffix of one.
        "studio.local.evil.example",
        "not-studio.local",
    ] {
        let request =
            format!("GET /loop-studio/status HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
        assert_eq!(
            status_of(hosts_port, &request),
            421,
            "Host {host} is not allow-listed and must be refused"
        );
    }
    drop(hosts_router);

    // --- Origin allow-list ------------------------------------------------
    let origin_router = support::spawn_router(
        router_bin,
        &[
            ("HOME", &home_env),
            ("LOOP_STUDIO_BIND_ADDR", "127.0.0.1"),
            (
                "LOOP_STUDIO_ALLOWED_ORIGINS",
                "https://studio.example,http://localhost:5173",
            ),
            ("MLX_PORT", &dead),
            ("LOOP_STUDIO_LLAMA_PORT", &dead),
            ("LOOP_STUDIO_OLLAMA_PORT", &dead),
        ],
    );
    let origin_port = origin_router.port;
    assert!(
        wait_status(origin_port, Duration::from_secs(15)),
        "router must start with an Origin allow-list"
    );
    for origin in ["https://studio.example", "http://localhost:5173"] {
        let request = format!(
            "GET /loop-studio/status HTTP/1.1\r\nHost: localhost\r\nOrigin: {origin}\r\nConnection: close\r\n\r\n"
        );
        let (status, head, body) = raw_request(origin_port, &request);
        assert_eq!(status, 200, "allow-listed origin {origin}: {head}");
        assert!(
            body.contains("\"router\": \"loop-studio-router") && body.contains("\"backends\""),
            "{origin}: {body}"
        );
        // Its own value back, never a wildcard the router picked, plus Vary
        // so a shared cache cannot hand this reply to another page.
        assert!(
            head.contains(&format!("Access-Control-Allow-Origin: {origin}\r\n")),
            "{origin} must be echoed exactly: {head}"
        );
        // `Vary: Origin` is emitted last, so it is the final header line and
        // carries no trailing CRLF inside the head block. Compare the line
        // rather than a substring, so a `Vary: Origin-Foo` cannot pass.
        assert!(
            head.lines().any(|line| line == "Vary: Origin"),
            "{origin}: {head}"
        );
        // The preflight agrees with the real gate, so a browser that passed
        // one is not refused by the other.
        let preflight = format!(
            "OPTIONS /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\nOrigin: {origin}\r\nAccess-Control-Request-Method: POST\r\n\r\n"
        );
        let (status, head, _) = raw_request(origin_port, &preflight);
        assert_eq!(status, 204, "{origin} preflight: {head}");
        assert!(
            head.contains(&format!("Access-Control-Allow-Origin: {origin}\r\n")),
            "{origin} preflight must be echoed: {head}"
        );
        assert!(
            head.contains("Access-Control-Allow-Methods:"),
            "{origin} preflight: {head}"
        );
    }
    for origin in [
        "https://evil.example",
        // An allow-listed origin is a whole origin, not a prefix of one.
        "https://studio.example.evil.test",
        "https://studio.example:8443",
        "http://studio.example",
    ] {
        let request = format!(
            "GET /loop-studio/status HTTP/1.1\r\nHost: localhost\r\nOrigin: {origin}\r\nConnection: close\r\n\r\n"
        );
        assert_eq!(
            status_of(origin_port, &request),
            403,
            "origin {origin} is not allow-listed and must be refused"
        );
    }
    drop(origin_router);

    // --- a valid token is proof of a non-browser client -------------------
    // This is what keeps Tailscale MagicDNS working against a token-protected
    // gateway: a rebound request is same-origin to the attacker's page, which
    // has never seen the token, so it cannot present one.
    let token_router = support::spawn_router(
        router_bin,
        &[
            ("HOME", &home_env),
            ("LOOP_STUDIO_BIND_ADDR", "127.0.0.1"),
            ("LOOP_STUDIO_API_TOKEN", "test-secret"),
            ("MLX_PORT", &dead),
            ("LOOP_STUDIO_LLAMA_PORT", &dead),
            ("LOOP_STUDIO_OLLAMA_PORT", &dead),
        ],
    );
    let token_port = token_router.port;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10)
        && status_of(
            token_port,
            &format!(
                "GET /loop-studio/status HTTP/1.1\r\nHost: mac.tail1234.ts.net:{token_port}\r\nAuthorization: Bearer test-secret\r\nConnection: close\r\n\r\n"
            ),
        ) != 200
    {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        status_of(
            token_port,
            &format!(
                "GET /loop-studio/status HTTP/1.1\r\nHost: mac.tail1234.ts.net:{token_port}\r\nAuthorization: Bearer test-secret\r\nConnection: close\r\n\r\n"
            )
        ),
        200,
        "a valid token must serve a DNS-named Host"
    );
    // The same Host without the token must not be served — the exception is
    // the token, not the name.
    assert_eq!(
        status_of(
            token_port,
            &format!("GET /loop-studio/status HTTP/1.1\r\nHost: mac.tail1234.ts.net:{token_port}\r\nConnection: close\r\n\r\n")
        ),
        421,
        "no token means no DNS-named Host"
    );
    // A wrong token proves nothing, so it must not open the gate either.
    assert_eq!(
        status_of(
            token_port,
            &format!(
                "GET /loop-studio/status HTTP/1.1\r\nHost: mac.tail1234.ts.net:{token_port}\r\nAuthorization: Bearer wrong\r\nConnection: close\r\n\r\n"
            )
        ),
        421,
        "a wrong token is not proof of anything"
    );
    // And a token still does not buy a browser origin: that is the
    // operator's explicit decision, made with LOOP_STUDIO_ALLOWED_ORIGINS.
    assert_eq!(
        status_of(
            token_port,
            "GET /loop-studio/status HTTP/1.1\r\nHost: localhost\r\nOrigin: https://evil.example\r\nAuthorization: Bearer test-secret\r\nConnection: close\r\n\r\n"
        ),
        403
    );

    drop(token_router);
    let _ = std::fs::remove_dir_all(&tmp_home);
}
