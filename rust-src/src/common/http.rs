//! Minimal blocking HTTP helpers over 127.0.0.1, std-only.

use std::env;
use std::net::TcpStream;
use std::time::{Duration, Instant};

use super::ports::gateway_port;

/// TCP connect probe. Default budget 150ms keeps router hot-path fast.
pub fn port_up(port: u16) -> bool {
    port_up_ms(port, 150)
}

pub fn port_up_ms(port: u16, ms: u64) -> bool {
    let addr = format!("127.0.0.1:{}", port);
    match addr.parse() {
        Ok(sa) => TcpStream::connect_timeout(&sa, Duration::from_millis(ms)).is_ok(),
        Err(_) => false,
    }
}

/// Block until a TCP port accepts (or timeout). For supervising daemons.
pub fn wait_for_port(port: u16, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if port_up_ms(port, 200) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    false
}

/// HTTP readiness: `GET /v1/models` must return 200. A bound TCP port only
/// proves the socket exists; this proves the model is loaded and serving.
/// Used by the router health cache and server readiness threads.
pub fn http_ready(port: u16, timeout_ms: u64) -> bool {
    http_get(port, "/v1/models", timeout_ms)
        .map(|(code, _)| code == 200)
        .unwrap_or(false)
}

/// Minimal blocking HTTP/1.0-style GET over 127.0.0.1. Returns
/// (status_code, body). Cap 256 KiB — enough for model lists, bounded
/// against pathological responses.
pub fn http_get(port: u16, path: &str, timeout_ms: u64) -> Option<(u16, String)> {
    http_get_auth(port, path, timeout_ms, port == gateway_port())
}

pub fn http_get_auth(
    port: u16,
    path: &str,
    timeout_ms: u64,
    authorized: bool,
) -> Option<(u16, String)> {
    use std::io::{Read, Write};
    let addr = format!("127.0.0.1:{}", port);
    let sa: std::net::SocketAddr = addr.parse().ok()?;
    let mut s =
        TcpStream::connect_timeout(&sa, Duration::from_millis(timeout_ms.min(2000))).ok()?;
    let _ = s.set_read_timeout(Some(Duration::from_millis(timeout_ms.min(5000))));
    let _ = s.set_write_timeout(Some(Duration::from_millis(2000)));
    let auth = if authorized {
        env::var("LOOP_STUDIO_API_TOKEN")
            .ok()
            .map(|token| token.trim().to_string())
            .filter(|token| !token.is_empty() && !token.chars().any(|c| c.is_control()))
            .map(|token| format!("Authorization: Bearer {}\r\n", token))
            .unwrap_or_default()
    } else {
        String::new()
    };
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\n{}Connection: close\r\n\r\n",
        path, addr, auth
    );
    s.write_all(req.as_bytes()).ok()?;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match s.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > 262_144 {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&buf).to_string();
    let head_end = text.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0);
    let status = text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .unwrap_or(0);
    Some((status, text[head_end.min(text.len())..].to_string()))
}

/// Blocking HTTP POST over 127.0.0.1. Returns (status_code, body).
/// std-only, bounded read buffer up to 4MB.
pub fn http_post(port: u16, path: &str, body: &str, timeout_ms: u64) -> Option<(u16, String)> {
    http_post_addr_auth(
        &format!("127.0.0.1:{}", port),
        path,
        body,
        Duration::from_millis(timeout_ms),
        true,
    )
}

/// Blocking HTTP POST to a specific socket address with total deadline budget.
pub fn http_post_addr(
    addr: &str,
    path: &str,
    body: &str,
    budget: Duration,
) -> Option<(u16, String)> {
    http_post_addr_auth(addr, path, body, budget, false)
}

pub fn http_post_addr_auth(
    addr: &str,
    path: &str,
    body: &str,
    budget: Duration,
    authorized: bool,
) -> Option<(u16, String)> {
    use std::io::{Read, Write};
    if budget.is_zero() {
        return None;
    }
    let start = Instant::now();
    let sa: std::net::SocketAddr = addr.parse().ok()?;
    let auth = if authorized {
        env::var("LOOP_STUDIO_API_TOKEN")
            .ok()
            .map(|token| token.trim().to_string())
            .filter(|token| !token.is_empty() && !token.chars().any(|c| c.is_control()))
            .map(|token| format!("Authorization: Bearer {}\r\n", token))
            .unwrap_or_default()
    } else {
        String::new()
    };
    let req = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        path,
        addr,
        auth,
        body.len(),
        body
    );
    // Connect budget never exceeds the total budget: a 2s caller budget
    // must not block 5s in connect (hung-backend test would flake).
    let connect_budget = Duration::from_secs(5).min(budget.max(Duration::from_millis(50)));
    let mut s = TcpStream::connect_timeout(&sa, connect_budget).ok()?;
    let _ = s.set_write_timeout(Some(Duration::from_secs(5).min(budget)));
    if s.write_all(req.as_bytes()).is_err() {
        return None;
    }
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 16384];
    loop {
        let remaining = budget
            .checked_sub(start.elapsed())
            .unwrap_or(Duration::ZERO);
        if remaining.is_zero() {
            return None;
        }
        let _ = s.set_read_timeout(Some(remaining.min(Duration::from_secs(30))));
        match s.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > 4_194_304 {
                    break;
                }
            }
            Err(_) => {
                if start.elapsed() > budget {
                    return None;
                }
            }
        }
    }
    if buf.is_empty() {
        return None;
    }
    let text = String::from_utf8_lossy(&buf).to_string();
    let code: u16 = text
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    let start = text.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0);
    Some((code, text[start.min(text.len())..].to_string()))
}

/// Block until `GET /v1/models` is 200 (or timeout). Servers call this
/// from a prober thread so `loop-studio status` stops racing server boot.
pub fn wait_for_http(port: u16, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if http_ready(port, 1500) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    false
}
