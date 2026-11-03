//! Benchmark and tuning commands.

use std::env;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use super::common;

pub fn content_len_approx(body: &str) -> Option<usize> {
    let key = "\"content\":\"";
    let p = body.find(key)?;
    let mut len = 0usize;
    let mut esc = false;
    for c in body[p + key.len()..].chars() {
        if esc {
            len += 1;
            esc = false;
        } else if c == '\\' {
            esc = true;
        } else if c == '"' {
            return Some(len);
        } else {
            len += c.len_utf8();
        }
    }
    None
}

/// One chat-completion probe against a backend.
pub struct Probe {
    pub ttft_s: Option<f64>,
    pub total_s: f64,
    pub toks: Option<f64>,
}

/// Tunable probe shape.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProbeOpts {
    pub tokens: u32,
    pub temp: f64,
}

/// A probe of a lane that answers HTTP must finish or fail inside this.
const BENCH_HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Ceiling on a probe's response accumulation.
const HTTP_PROBE_CAP: u64 = 4 * 1024 * 1024;

/// The port argument of `loop-studio bench` / `loop-studio tune`, or the documented default.
pub fn bench_port(args: &[String], default: u16) -> u16 {
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--tokens" | "--max-tokens" | "--max_tokens" | "--temp" | "--temperature" => i += 1,
            s if s.starts_with('-') => {}
            other => return common::port_from(other).unwrap_or(default),
        }
        i += 1;
    }
    default
}

pub fn parse_probe_opts(args: &[String]) -> ProbeOpts {
    let mut tokens = env::var("LOOP_STUDIO_BENCH_TOKENS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(64u32);
    let mut temp = env::var("LOOP_STUDIO_BENCH_TEMP")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0.0f64);
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--tokens" | "--max-tokens" | "--max_tokens" => {
                if let Some(v) = args.get(i + 1).and_then(|s| s.parse::<u32>().ok()) {
                    tokens = v;
                    i += 1;
                }
            }
            s if s.starts_with("--tokens=") || s.starts_with("--max-tokens=") => {
                if let Some(v) = s.split('=').nth(1).and_then(|x| x.parse::<u32>().ok()) {
                    tokens = v;
                }
            }
            "--temp" | "--temperature" => {
                if let Some(v) = args.get(i + 1).and_then(|s| s.parse::<f64>().ok()) {
                    temp = v;
                    i += 1;
                }
            }
            s if s.starts_with("--temp=") || s.starts_with("--temperature=") => {
                if let Some(v) = s.split('=').nth(1).and_then(|x| x.parse::<f64>().ok()) {
                    temp = v;
                }
            }
            _ => {}
        }
        i += 1;
    }
    ProbeOpts {
        tokens: tokens.clamp(8, 4096),
        temp: if temp.is_finite() { temp.clamp(0.0, 2.0) } else { 0.0 },
    }
}

pub fn probe_chat(port: u16, read_timeout: Duration, opts: ProbeOpts) -> Option<Probe> {
    let addr = format!("127.0.0.1:{}", port);
    let sa: std::net::SocketAddr = addr.parse().ok()?;
    let body = format!(
        "{{\"model\":\"qwen3.8-27b\",\"messages\":[{{\"role\":\"user\",\"content\":\"Respond with exactly three lines of text about high performance computing.\"}}],\"max_tokens\":{},\"temperature\":{}}}",
        opts.tokens, opts.temp
    );
    let post_req = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        addr, body.len(), body
    );
    let t_inf = Instant::now();
    let mut ttft: Option<Duration> = None;
    let mut inf_resp: Vec<u8> = Vec::new();
    let mut s = TcpStream::connect_timeout(&sa, Duration::from_secs(10)).ok()?;
    let _ = s.set_read_timeout(Some(read_timeout));
    s.write_all(post_req.as_bytes()).ok()?;
    let mut chunk = [0u8; 4096];
    loop {
        match s.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if ttft.is_none() {
                    ttft = Some(t_inf.elapsed());
                }
                if (inf_resp.len() as u64) < HTTP_PROBE_CAP {
                    let room = (HTTP_PROBE_CAP - inf_resp.len() as u64) as usize;
                    inf_resp.extend_from_slice(&chunk[..n.min(room)]);
                }
            }
            Err(_) => break,
        }
    }
    if ttft.is_none() && inf_resp.is_empty() {
        return None;
    }
    let total = t_inf.elapsed();
    let inf_str = String::from_utf8_lossy(&inf_resp);
    let toks = content_len_approx(&inf_str)
        .map(|clen| (clen as f64 / 4.0).max(1.0) / total.as_secs_f64().max(0.01));
    Some(Probe {
        ttft_s: ttft.map(|t| t.as_secs_f64()),
        total_s: total.as_secs_f64(),
        toks,
    })
}

pub fn cmd_bench(port: u16, args: &[String]) {
    let opts = parse_probe_opts(args);
    println!(
        "=== Loop Studio Benchmark Probe (http://127.0.0.1:{}) [tokens={} temp={}] ===",
        port, opts.tokens, opts.temp
    );

    let addr = format!("127.0.0.1:{}", port);
    let sa: Option<std::net::SocketAddr> = addr.parse().ok();
    let socket = sa.and_then(|sa| TcpStream::connect_timeout(&sa, Duration::from_millis(2000)).ok());

    match socket {
        Some(mut stream) => {
            let req = format!("GET /v1/models HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n", addr);
            let t0 = Instant::now();
            let _ = stream.write_all(req.as_bytes());
            let _ = stream.set_read_timeout(Some(BENCH_HTTP_TIMEOUT));
            let _ = stream.set_write_timeout(Some(BENCH_HTTP_TIMEOUT));
            let mut resp = Vec::new();
            let read = (&mut stream).take(HTTP_PROBE_CAP).read_to_end(&mut resp);
            println!(
                "  - Socket roundtrip (TTFB): {:.2} ms",
                t0.elapsed().as_secs_f64() * 1000.0
            );
            if read.is_err() && resp.is_empty() {
                eprintln!(
                    "  [x] No response from :{} within {:?} — the port is listening but not answering HTTP.",
                    port, BENCH_HTTP_TIMEOUT
                );
                std::process::exit(1);
            }
            let resp_str = String::from_utf8_lossy(&resp);
            let status = resp_str.lines().next().unwrap_or("Unknown");
            println!("  - Health Status: {}", status);

            if status.contains("200") {
                println!("\nRunning inference benchmark (chat completion test)...");
                match probe_chat(port, Duration::from_secs(300), opts) {
                    Some(p) => {
                        if let Some(t) = p.ttft_s {
                            println!("  - TTFT (first byte): {:.2} s", t);
                        }
                        println!("  - Inference Total Time: {:.2} s", p.total_s);
                        match p.toks {
                            Some(t) => println!(
                                "  - Estimated Generation Speed: {:.1} tokens/sec (rough: ~4 chars/token)",
                                t
                            ),
                            None => println!("  - No content field found in response (backend may have errored)."),
                        }
                    }
                    None => println!("  - Probe failed: no response bytes."),
                }
            }
        }
        None => {
            eprintln!("  [x] Connection failed to port {}. Service is offline.", port);
            std::process::exit(1);
        }
    }
}

pub fn cmd_tune(args: &[String]) {
    let apply = args.iter().any(|a| a == "--apply");
    let opts = parse_probe_opts(args);
    println!(
        "=== Loop Studio Lane Tune (all live backends, 60s probe budget each) [tokens={} temp={}] ===",
        opts.tokens, opts.temp
    );
    let lanes = [
        ("mlx", common::mlx_port()),
        ("llama", common::llama_port()),
        ("ollama", common::ollama_port()),
    ];
    let mut results: Vec<(&str, u16, Probe)> = Vec::new();
    for (name, port) in lanes {
        if !common::http_ready(port, 1500) {
            println!("  {:<7} :{:<5} down (skipped)", name, port);
            continue;
        }
        print!("  {:<7} :{:<5} probing... ", name, port);
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
        match probe_chat(port, Duration::from_secs(60), opts) {
            Some(p) => {
                println!(
                    "TTFT {:.2}s total {:.2}s tok/s {}",
                    p.ttft_s.unwrap_or(-1.0),
                    p.total_s,
                    p.toks.map(|t| format!("{:.1}", t)).unwrap_or_else(|| "-".to_string())
                );
                results.push((name, port, p));
            }
            None => println!("probe failed"),
        }
    }
    if results.is_empty() {
        let hint = if common::mlx_supported() { "loop-studio serve mlx" } else { "loop-studio serve llama" };
        println!("No live backends. Start one with `{}`.", hint);
        return;
    }
    results.sort_by(|a, b| {
        b.2.toks
            .partial_cmp(&a.2.toks)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(
                a.2.ttft_s
                    .partial_cmp(&b.2.ttft_s)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
    });
    println!("----------------------------------------------------------------");
    for (i, (name, port, p)) in results.iter().enumerate() {
        println!(
            "  #{:<2} {:<7} :{:<5} tok/s {} TTFT {:.2}s",
            i + 1,
            name,
            port,
            p.toks.map(|t| format!("{:.1}", t)).unwrap_or_else(|| "-".to_string()),
            p.ttft_s.unwrap_or(-1.0)
        );
    }
    let winner = results[0].0;
    println!("Winner (throughput): {}", winner);
    if apply {
        if !common::persist_or_warn(
            &format!("{}/.lac/router-backend", common::home_dir()),
            winner,
            "tune_persist",
        ) {
            eprintln!("tune winner '{}' not persisted; re-run with --apply.", winner);
        }
        if common::port_up(common::gateway_port()) {
            let url = format!("/loop-studio/switch?target={}", winner);
            match common::http_get(common::gateway_port(), &url, 2000) {
                Some((200, _)) => println!("Router hot-swapped to {} (persisted).", winner),
                _ => println!("Persisted {}; gateway switch unconfirmed.", winner),
            }
        } else {
            println!("Persisted {}; gateway down — applies on next router start.", winner);
        }
    } else {
        println!("Re-run with --apply to pin the winner.");
    }
}
