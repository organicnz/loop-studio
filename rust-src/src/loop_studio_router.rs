//! loop-studio-router v2.8 — adaptive unified gateway on :8000.
//!
//! v2.8 over v2.7:
//! - Response framing: Content-Length early-close (no hold-open stall),
//!   chunked terminal detection, SSE-only keep-alives (never JSON).
//! - Remote Bearer gate: LOOP_STUDIO_BIND_ADDR + LOOP_STUDIO_API_TOKEN, fail-closed on
//!   non-loopback bind without a token, 401 for remote without Bearer.
//!
//! v2.7 learning layer over v2.6:
//! - Per-backend EWMA first-byte latency + outcome/error counters, exposed
//!   in /lac/status (`stats`) — the gateway measures instead of guessing.
//! - `fastest` preference: opt-in lowest-measured-latency routing (tiers
//!   stay policy-ordered by default; speed never silently overrides the
//!   operator's quality choice).
//! - Circuit breaker: 3 consecutive failures cool a backend down for 30s.
//!   Resets and budget expiries both count; a client that goes away does not.
//! - Bounded work: 64 KiB headers, 16 MiB body per request, and a 256 MiB
//!   ceiling on body memory held at once (allocated capacity, not bytes
//!   consumed), so parallel uploads cannot pin the heap.
//! - Per-request usage log (~/.lac/router-usage.jsonl, rotated at 8MB)
//!   with byte counts and token estimates — the honest token source that
//!   lets kv-manage stop scraping for session numbers.
//! - Zero-cost model sniffing: when the request already names a model and
//!   the merged catalog mapped it to a specific live backend, that backend
//!   goes first (no extra reads, no added latency, pure reorder).

mod common;

use std::collections::{HashMap, HashSet};
use std::env;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

// Backend IDs
const BACKEND_AUTO: usize = 0;
const BACKEND_MLX: usize = 1;
const BACKEND_LLAMA: usize = 2;
const BACKEND_OLLAMA: usize = 3;
const BACKEND_FASTEST: usize = 4;
/// The three addressable lanes, and the number of per-backend slots the
/// catalog memo keeps. `BACKEND_FASTEST` and `BACKEND_AUTO` are orderings, not
/// backends, so they have no slot.
const BACKEND_COUNT: usize = 4;

// --------------------------------------------------------------- host facts --

/// Host facts the dashboard shows, measured here so the client does not have
/// to spawn a subprocess to ask.
///
/// The dashboard polls `/lac/status` every 3s and each poll used to end in
/// `loop-studio status --json` — a subprocess three times a minute for thermal and RAM
/// gauges. The router already runs on the host and `common` has the probes, so
/// the answers belong in the document the poll already fetches. The router does
/// not know the chat model (that is host-side `opencode.jsonc` config), and the
/// dashboard does not show it from here, so it is deliberately absent.
#[derive(Clone, Debug)]
struct HostFacts {
    /// Absent when the thermal probe could not answer. It is `Option` for the
    /// same reason the two RAM fields are: a poll that could not run is not a
    /// measurement, and this field is read by the TUI's rules engine to pick
    /// Q4-vs-Q8 and a context window. It used to be a `String` that could only
    /// ever be a level, so a broken probe published "Nominal" and every
    /// consumer treated that as headroom.
    thermal: Option<String>,
    free_ram_gib: Option<f64>,
    total_ram_gib: Option<f64>,
}

/// A cached host-facts reading, refreshed on a background thread.
///
/// The probes are three subprocesses (`pmset`, `vm_stat`, `sysctl`), which is
/// fine every few seconds and would not be fine on every status request — the
/// dashboard polls three times a minute and other clients poll too. So the
/// reading is taken on a timer and the status endpoint serves the cache.
struct HostFactsCache {
    inner: Mutex<Option<(HostFacts, Instant)>>,
    ttl: Duration,
}

impl HostFactsCache {
    fn new(ttl: Duration) -> Self {
        Self {
            inner: Mutex::new(None),
            ttl,
        }
    }

    /// Recompute when the cached reading is absent or older than the ttl.
    fn refresh(&self) {
        let due = self
            .inner
            .lock()
            .ok()
            .map(|c| c.as_ref().map(|(_, at)| at.elapsed() >= self.ttl).unwrap_or(true))
            .unwrap_or(true);
        if !due {
            return;
        }
        let facts = HostFacts {
            thermal: common::thermal_state(),
            free_ram_gib: common::free_ram_gib(),
            total_ram_gib: common::total_ram_gib(),
        };
        if let Ok(mut c) = self.inner.lock() {
            *c = Some((facts, Instant::now()));
        }
    }

    fn snapshot(&self) -> Option<HostFacts> {
        self.inner
            .lock()
            .ok()
            .and_then(|c| c.as_ref().map(|(facts, _)| facts.clone()))
    }

    /// Serve the cached reading, refreshing first if it has gone stale. Split
    /// from [`HostFactsCache::refresh`] so the timer thread and the status
    /// endpoint share one "is it due" rule.
    fn serve(&self) -> Option<HostFacts> {
        self.refresh();
        self.snapshot()
    }
}

/// MLX lane port with drift awareness (serve-mlx moves to 8082+ when
/// 8080 is taken and records it in ~/.lac/mlx.port).
fn port_mlx() -> u16 {
    common::mlx_port()
}

/// The port this process will bind, from the environment or a positional
/// argument.
///
/// The writer calls the reader. This used to be a bare `parse()`, so
/// `LOOP_STUDIO_ROUTER_PORT=0` bound the whole gateway to an ephemeral port while
/// `loop-studio chat`, `loop-studio status`, the TUI, Studio and `doctor` all dialled 0 — and
/// the startup log said "ignored, using 8000" and then, on the next line,
/// "Listening on http://127.0.0.1:54321". Every other reader of this variable
/// goes through `common::port_from`, which refuses zero; this one has to refuse
/// it for the same reason or the gateway is the one service nobody can find.
///
/// `ephemeral` is the one keyword that still means zero, and it is spelled out.
/// The process-level suites need a router on a port nothing else can collide
/// with, and they used to get one by setting the variable to `0` — so the suite
/// was *pinning the bug*: it reads the real bound port back off the startup log
/// and would have kept passing whether or not this rule existed.
fn resolve_bind_port(env_value: Option<String>, arg: Option<String>) -> u16 {
    if is_ephemeral_request(env_value.as_deref()) || is_ephemeral_request(arg.as_deref()) {
        return 0;
    }
    env_value
        .as_deref()
        .and_then(common::port_from)
        .or_else(|| arg.as_deref().and_then(common::port_from))
        .unwrap_or(8000)
}

/// The one spelling that asks the kernel to choose a port.
fn is_ephemeral_request(raw: Option<&str>) -> bool {
    raw.is_some_and(|p| p.trim().eq_ignore_ascii_case("ephemeral"))
}

/// llama/ollama ports are env-overridable (testability + non-standard
/// layouts); the router still readiness-gates whatever is configured.
///
/// These were bare `parse()`s, so `LOOP_STUDIO_LLAMA_PORT=0` left the zero in. Two
/// things followed, and the second is the one that mattered: the router
/// printed `warning: … ignored, using 8081` and then probed 0, and
/// `StatsMap` is keyed by `u16` — so llama and ollama collapsed into *one*
/// entry, sharing a consecutive-error count and a circuit-breaker verdict,
/// with `/lac/status` printing the same numbers under two different names.
/// Now the same rule every other reader uses.
fn port_llama() -> u16 {
    common::llama_port()
}

fn port_ollama() -> u16 {
    common::ollama_port()
}

/// Tier order with live ports: [(id, port)] MLX -> llama -> Ollama.
fn tier_backends() -> Vec<(usize, u16)> {
    vec![
        (BACKEND_MLX, port_mlx()),
        (BACKEND_LLAMA, port_llama()),
        (BACKEND_OLLAMA, port_ollama()),
    ]
}

#[allow(dead_code)]
fn backend_id_for_port(port: u16) -> usize {
    for (id, p) in tier_backends() {
        if p == port {
            return id;
        }
    }
    BACKEND_AUTO
}

const MAX_HEADERS: usize = 65536;
const MAX_BODY: usize = 16 * 1024 * 1024;
const MAX_INFLIGHT: usize = 128;
const MAX_INTERIM_RESPONSES: usize = 8;

/// Keep-alive comments the router injects into a stalled SSE stream. Counted
/// in the bytes we forward so the usage log reflects what the client got.
const KEEPALIVE_CHUNK: &[u8] = b"E\r\n: keep-alive\n\n\r\n";
const KEEPALIVE_PLAIN: &[u8] = b": keep-alive\n\n";

/// Wall-clock budgets are env-overridable so tests can shrink them
/// (`LOOP_STUDIO_ROUTER_STREAM_BUDGET_SECS=1`); out-of-range values fall back to
/// the production default rather than disabling a budget.
fn budget_secs(name: &str, default_secs: u64) -> Duration {
    Duration::from_secs(
        env::var(name)
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|&secs| (1..=86_400).contains(&secs))
            .unwrap_or(default_secs),
    )
}

/// Wall clock one relayed response may take from the moment its request
/// was written. Note this is not a whole-connection budget: headers, the
/// request body and the response each get their own window, so one stalled
/// client connection can hold an inflight slot for several of these.
fn stream_budget() -> Duration {
    budget_secs("LOOP_STUDIO_ROUTER_STREAM_BUDGET_SECS", 600)
}

/// How long a client may take to finish sending request headers.
fn header_budget() -> Duration {
    budget_secs("LOOP_STUDIO_ROUTER_HEADER_BUDGET_SECS", 10)
}

/// Idle gap tolerated between request-body reads (slow-upload backstop).
fn body_idle_timeout() -> Duration {
    budget_secs("LOOP_STUDIO_ROUTER_BODY_IDLE_SECS", 30)
}

fn backend_name(id: usize) -> &'static str {
    match id {
        BACKEND_MLX => "mlx",
        BACKEND_LLAMA => "llama",
        BACKEND_OLLAMA => "ollama",
        BACKEND_FASTEST => "fastest",
        _ => "auto",
    }
}

/// Parse a backend name, or `None` when it names nothing.
///
/// `backend_id` maps every unrecognised string to `BACKEND_AUTO`, which is the
/// right answer for a *file* read — a corrupt preference is not a reason to
/// refuse to start — and the wrong answer for an operator *asking* for a lane.
/// Both callers here are requests, and both had this defect:
/// `LOOP_STUDIO_BACKEND=mlxx` passed the "non-empty and not auto" guard, was silently
/// resolved to `auto`, and was then written to `~/.lac/router-backend`,
/// destroying the lane the operator had chosen and persisted. `?target=mlxx`
/// did the same and answered `200 {"status":"switched"}`.
///
/// Two functions rather than one with a flag: "I could not read your saved
/// choice" and "I do not know what you asked for" are different sentences and
/// must not be collapsed into one.
fn backend_id_strict(name: &str) -> Option<usize> {
    match name.trim().to_lowercase().as_str() {
        "mlx" => Some(BACKEND_MLX),
        "llama" => Some(BACKEND_LLAMA),
        "ollama" => Some(BACKEND_OLLAMA),
        "fastest" => Some(BACKEND_FASTEST),
        "auto" => Some(BACKEND_AUTO),
        _ => None,
    }
}

/// The names a request may name, for the refusal message. An operator who
/// typed `mlxx` needs the list, not a restatement of the error.
fn backend_name_list() -> String {
    "mlx, llama, ollama, fastest, auto".to_string()
}

fn backend_id(name: &str) -> usize {
    backend_id_strict(name).unwrap_or(BACKEND_AUTO)
}

fn backend_port(id: usize) -> u16 {
    match id {
        BACKEND_MLX => port_mlx(),
        BACKEND_LLAMA => port_llama(),
        BACKEND_OLLAMA => port_ollama(),
        _ => port_mlx(),
    }
}

// ------------------------------------------------------------ health cache --

struct HealthCache {
    inner: Mutex<HashMap<u16, (bool, Instant)>>,
    /// One lock per port. This is what makes a burst of concurrent misses
    /// cost one probe rather than N.
    probes: Mutex<HashMap<u16, Arc<Mutex<()>>>>,
    ttl: Duration,
    negative_ttl: Duration,
}

impl HealthCache {
    /// `negative_ttl` is a separate argument rather than derived from `ttl`,
    /// because a negative verdict is worth keeping for much less time than a
    /// positive one: a backend that is coming up should be picked up quickly,
    /// and being wrong about "down" only costs a retry. It used to be a
    /// hardcoded 2500ms inside the constructor, which meant `new(ttl)` did
    /// not actually mean "expire after ttl" — a caller passing a 30ms window
    /// still got 2.5s of negative caching, and a test could not see the
    /// difference without sleeping for seconds.
    fn new(ttl: Duration, negative_ttl: Duration) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            probes: Mutex::new(HashMap::new()),
            ttl,
            negative_ttl,
        }
    }

    fn window(&self, value: bool) -> Duration {
        if value {
            self.ttl
        } else {
            self.negative_ttl
        }
    }

    /// The cached verdict for `port`, if it has not expired.
    fn cached(&self, port: u16) -> Option<bool> {
        let guard = self.inner.lock().ok()?;
        let (value, timestamp) = guard.get(&port)?;
        (timestamp.elapsed() < self.window(*value)).then_some(*value)
    }

    /// Cached HTTP readiness. One `/v1/models` probe per backend per TTL
    /// window instead of 3× TCP connects on every request.
    fn ready(&self, port: u16) -> bool {
        self.ready_with(port, || common::http_ready(port, 1500))
    }

    /// `ready`, with the probe injected so the coalescing can be tested
    /// without a socket.
    ///
    /// The old version dropped the cache lock before probing, so N threads
    /// arriving together on an expired entry fired N identical probes at the
    /// same backend — and the router is one thread per connection with 128
    /// slots, so a burst after a TTL expiry was up to 128 simultaneous
    /// `GET /v1/models` at a single-threaded inference server. Each held an
    /// inflight slot for the length of the probe.
    ///
    /// The fix serializes probes per port and re-checks the cache after
    /// acquiring, so the losers of the race find a fresh answer and never
    /// probe at all. It also removes a second bug for free: probes used to
    /// `insert` unconditionally, so a slow probe landing after a fast one
    /// overwrote a good positive with a stale negative and cost up to
    /// `negative_ttl` of spurious 503s. With one prober per port, the last
    /// writer is always the most recent.
    ///
    /// Ports do not block each other — the lock is per port, not global — so
    /// a slow MLX probe never delays probing llama.
    fn ready_with<F: FnOnce() -> bool>(&self, port: u16, probe: F) -> bool {
        if let Some(value) = self.cached(port) {
            return value;
        }
        // Per-port probe lock, created once and kept for the process's life.
        // Three ports, so the map is bounded and never needs pruning.
        let probe_lock = {
            let mut guard = match self.probes.lock() {
                Ok(guard) => guard,
                // A poisoned lock must not take the gateway down: fall back to
                // probing directly, which is what the code did unconditionally
                // before.
                Err(poisoned) => poisoned.into_inner(),
            };
            Arc::clone(guard.entry(port).or_default())
        };
        let _serialized = match probe_lock.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        // Re-check: the thread we queued behind may have just refreshed this.
        // Its result is as good as one we would compute, and it cost the
        // backend one request instead of two.
        if let Some(value) = self.cached(port) {
            return value;
        }
        let value = probe();
        if let Ok(mut guard) = self.inner.lock() {
            guard.insert(port, (value, Instant::now()));
        }
        value
    }
}

// ------------------------------------------------------- measured routing --

/// Per-backend learned state. EWMA weights recent first-byte latency so
/// the router adapts within a handful of requests; error streaks feed
/// the circuit breaker; byte/token counters feed the usage log.
#[derive(Clone, Default)]
struct BackendStats {
    ewma_ms: Option<f64>,
    ok: u64,
    err: u64,
    consec_err: u32,
    last_err: Option<Instant>,
    req_bytes: u64,
    resp_bytes: u64,
    est_tokens: u64,
}

type StatsMap = Arc<Mutex<HashMap<u16, BackendStats>>>;

/// model id -> (backend port, mapped_at). Populated from merged catalogs;
/// entries older than 5 minutes are ignored (backends come and go).
type RouteMap = Arc<Mutex<HashMap<String, (u16, Instant)>>>;

/// The merged `/v1/models` catalog, held just long enough to collapse a burst
/// of concurrent callers into one fetch per backend.
///
/// `merged_models` is not free: it opens a fresh TCP connection to every ready
/// backend and reads its catalog, and nothing cached the result. The router
/// runs one thread per connection with 128 slots, so a client that polls
/// `/v1/models` — which every model picker does, and OpenCode does on every
/// launch — turned one cheap metadata request into three backend connections
/// per caller, unbounded in the number of callers. `HealthCache` only ever
/// cached the boolean readiness probe, not the catalog behind it.
///
/// The window is deliberately short. A catalog is metadata that changes when
/// the operator loads or unloads a model, and a picker that cannot see a newly
/// loaded model for a second is not a problem; a picker that hammers three
/// inference servers every time it opens is.
///
/// Three properties, each of which the single-lock version got wrong:
///
/// - **Per-backend.** One mutex was held across the whole fan-out, so a
///   backend that accepted the health probe and then stalled on the catalog read
///   — a partially wedged inference server, which is exactly the case the
///   circuit breaker exists for — froze *every* caller's view of the other two.
///   `HealthCache` was deliberately given per-port probe locks for this reason;
///   the catalog now has the same shape.
/// - **Non-blocking.** A waiting caller holds one of the 128 inflight slots for
///   the whole wait, so a burst of model-picker requests could exhaust the
///   router and make the accept loop answer every *other* client
///   `503 lac_router_busy`. A cheap metadata GET must not be able to take the
///   gateway down for chat traffic.
/// - **Stale-while-revalidate.** Once something is cached, an expired entry is
///   served immediately and refreshed in the background. The cold path — nothing
///   cached at all — still blocks, because there is nothing to serve and
///   inventing a catalog would be worse than waiting.
type CatalogCache = Arc<CatalogMemo>;

struct CatalogMemo {
    /// Per backend id: that backend's own catalog and when it was read. Split
    /// per port so a slow lane is a slow lane, not a slow endpoint.
    entries: Mutex<Vec<Option<(String, Instant)>>>,
    /// One probe lock per backend. A caller never *waits* on a peer's probe
    /// (`try_lock`), so the burst collapses to one fetch per backend and the
    /// losers fall through to forwarding instead of queueing behind the winner.
    probes: Vec<Mutex<()>>,
    /// The merged document, which is what callers actually receive, and when.
    merged: Mutex<Option<(String, Instant)>>,
    /// One background refresh at a time, so an expired cache under load does
    /// not spawn a refresh per caller.
    refreshing: Mutex<bool>,
    /// Held only for the cold path, where blocking is correct because there is
    /// nothing cached to serve instead.
    cold_lock: Mutex<()>,
    ttl: Duration,
}

impl CatalogMemo {
    fn new(ttl: Duration) -> Self {
        Self {
            entries: Mutex::new(vec![None; BACKEND_COUNT]),
            probes: (0..BACKEND_COUNT).map(|_| Mutex::new(())).collect(),
            merged: Mutex::new(None),
            refreshing: Mutex::new(false),
            cold_lock: Mutex::new(()),
            ttl,
        }
    }

    /// A merged body that is still inside the ttl.
    fn fresh(&self) -> Option<String> {
        let guard = self.merged.lock().ok()?;
        let (body, at) = guard.as_ref()?;
        (at.elapsed() < self.ttl).then(|| body.clone())
    }

    /// Any merged body, expired or not — what a stale-while-revalidate hit
    /// serves while the refresh runs behind it.
    fn stale(&self) -> Option<String> {
        self.merged.lock().ok().and_then(|g| g.as_ref().map(|(b, _)| b.clone()))
    }

    fn put(&self, body: String) {
        if let Ok(mut g) = self.merged.lock() {
            *g = Some((body, Instant::now()));
        }
    }

    /// Claim the right to refresh in the background. Exactly one caller gets it.
    fn claim_refresh(&self) -> bool {
        match self.refreshing.lock() {
            Ok(mut g) => {
                if *g {
                    false
                } else {
                    *g = true;
                    true
                }
            }
            Err(poisoned) => {
                let mut g = poisoned.into_inner();
                let fresh = !*g;
                *g = true;
                fresh
            }
        }
    }

    fn release_refresh(&self) {
        if let Ok(mut g) = self.refreshing.lock() {
            *g = false;
        }
    }

    /// Probe one backend, or skip it if a peer is already probing that backend.
    ///
    /// A successful body is cached here, so "a backend's catalog is in its own
    /// slot" is one invariant in one place rather than a thing the caller has to
    /// remember. A refusal is *not* cached: a lane that is down right now may be
    /// up in a second, and a cached absence would keep the model picker blind to
    /// it for the whole ttl — which is the one moment the picker matters.
    ///
    /// Returning `None` when the lock is held is deliberate: the peer will
    /// populate this backend's entry, and blocking here to wait for it is the
    /// behaviour that let a metadata request exhaust the router.
    fn probe<T: FnOnce() -> Option<String>>(&self, idx: usize, probe: T) -> Option<String> {
        let _guard = match self.probes[idx].try_lock() {
            Ok(guard) => guard,
            Err(_) => return None,
        };
        let body = probe()?;
        self.put_entry(idx, body.clone());
        Some(body)
    }

    fn entry(&self, idx: usize) -> Option<String> {
        let g = self.entries.lock().ok()?;
        let (body, at) = g.get(idx)?.as_ref()?;
        (at.elapsed() < self.ttl).then(|| body.clone())
    }

    /// One backend's own cached catalog, and how many of them are live — for
    /// the invariant test that a wedged lane cannot empty the endpoint.
    #[cfg(test)]
    fn live_entries(&self) -> usize {
        self.entries
            .lock()
            .map(|g| g.iter().filter(|e| e.is_some()).count())
            .unwrap_or(0)
    }

    fn put_entry(&self, idx: usize, body: String) {
        if let Ok(mut g) = self.entries.lock() {
            if let Some(slot) = g.get_mut(idx) {
                *slot = Some((body, Instant::now()));
            }
        }
    }

    /// Answer from cache, arranging a background rebuild if the entry expired.
    ///
    /// The whole stale-while-revalidate policy in one method, so it is testable
    /// without a backend and so the "serve, never block" property and the
    /// "still converges" property are asserted against the same code that runs
    /// in production. `None` means nothing is cached at all and the caller must
    /// take the cold path.
    ///
    /// `refresh` runs on its own thread and is called at most once per expiry,
    /// however many callers are waiting — otherwise an expired cache under load
    /// spawns a rebuild per caller, which is the same thundering herd one level
    /// up.
    fn serve(self: &Arc<Self>, refresh: impl FnOnce() + Send + 'static) -> Option<String> {
        if let Some(body) = self.fresh() {
            return Some(body);
        }
        let stale = self.stale()?;
        if self.claim_refresh() {
            let me = Arc::clone(self);
            let spawned = std::thread::Builder::new()
                .name("lac-catalog-refresh".to_string())
                .spawn(move || {
                    refresh();
                    me.release_refresh();
                });
            if spawned.is_err() {
                // No thread, so nobody will refresh. Take the claim back rather
                // than leaving the memo permanently "refreshing" — a memo that
                // thinks it is refreshing never refreshes again, and the stale
                // entry would then be served forever.
                self.release_refresh();
            }
        }
        Some(stale)
    }

    /// The cold-path fetch, serialised so a burst is one build.
    ///
    /// Blocking is *correct* here and nowhere else: the caller has already
    /// established that nothing is cached, so there is nothing to serve and
    /// inventing a catalog would be worse than waiting. The expired case does
    /// not come through here — [`CatalogMemo::stale`] answers it without
    /// blocking, and that is the whole point of the split.
    ///
    /// A `None` from `build` is not remembered, so a backend that comes up and
    /// starts advertising models is visible to the next caller.
    fn fetch<F: FnOnce() -> Option<String>>(&self, build: F) -> Option<String> {
        if let Some(body) = self.fresh() {
            return Some(body);
        }
        if let Some(body) = self.stale() {
            return Some(body);
        }
        let _serialized = match self.cold_lock.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        // Re-checked after acquiring, so the losers of the race reuse the
        // winner's body instead of rebuilding it.
        if let Some(body) = self.fresh().or_else(|| self.stale()) {
            return Some(body);
        }
        let body = build()?;
        self.put(body.clone());
        Some(body)
    }

    /// Whatever is in the merged slot, without consulting the ttl. The builder
    /// for [`CatalogMemo::fetch`] needs this to publish its own result, since
    /// `fetch` will not cache an absent one.
    fn merged_now(&self) -> Option<String> {
        self.stale()
    }
}

const BREAKER_ERRORS: u32 = 3;
const BREAKER_COOLDOWN: Duration = Duration::from_secs(30);
const ROUTE_TTL: Duration = Duration::from_secs(300);
const USAGE_ROTATE_BYTES: u64 = 8_000_000;
/// Unmeasured backends sort as slow: measured evidence always wins.
const EWMA_UNKNOWN_MS: f64 = 30_000.0;

/// How long a "this backend is not serving" answer is trusted. Shorter than
/// the positive window on purpose: a backend that is coming up should be
/// picked up promptly, and a wrong negative only costs one retry.
const NEGATIVE_TTL: Duration = Duration::from_millis(2500);

/// `req_received` is what this backend was actually sent, which is a
/// per-backend byte count and stays honest across a failover.
/// `req_charged` is what the *request* is charged for the token estimate
/// (`attempt_charge`), so a request that failed over does not read as two
/// requests' worth of context growth in `/lac/status` either.
fn note_success(
    stats: &StatsMap,
    port: u16,
    ttfb_ms: f64,
    req_received: u64,
    req_charged: u64,
    resp_body: u64,
) {
    if let Ok(mut m) = stats.lock() {
        let s = m.entry(port).or_default();
        s.ewma_ms = Some(match s.ewma_ms {
            Some(e) => 0.7 * e + 0.3 * ttfb_ms,
            None => ttfb_ms,
        });
        s.ok += 1;
        s.consec_err = 0;
        s.req_bytes += req_received;
        s.resp_bytes += resp_body;
        s.est_tokens += (req_charged + resp_body) / 4;
    }
}

fn note_error(stats: &StatsMap, port: u16) {
    if let Ok(mut m) = stats.lock() {
        let s = m.entry(port).or_default();
        s.err += 1;
        s.consec_err += 1;
        s.last_err = Some(Instant::now());
    }
}

/// Pure breaker predicate: open when failures are repeated AND recent.
fn breaker_open(consec: u32, last_err: Option<Instant>, now: Instant) -> bool {
    consec >= BREAKER_ERRORS
        && last_err
            .map(|t| now.duration_since(t) < BREAKER_COOLDOWN)
            .unwrap_or(false)
}

fn breaker_open_for(stats: &StatsMap, port: u16) -> bool {
    match stats.lock() {
        Ok(m) => m
            .get(&port)
            .map(|s| breaker_open(s.consec_err, s.last_err, Instant::now()))
            .unwrap_or(false),
        Err(_) => false,
    }
}

fn ewma_of(stats: &StatsMap, port: u16) -> f64 {
    stats
        .lock()
        .ok()
        .and_then(|m| m.get(&port).and_then(|s| s.ewma_ms))
        .unwrap_or(EWMA_UNKNOWN_MS)
}

/// Best-effort model hint from already-buffered bytes (headers plus any
/// over-read body). Zero extra waiting: None simply means "route blind".
fn sniff_model(buf: &[u8]) -> Option<String> {
    let s = String::from_utf8_lossy(buf);
    let key = "\"model\"";
    let p = s.find(key)?;
    let rest = &s[p + key.len()..];
    let c = rest.find(':')?;
    let after = rest[c + 1..].trim_start();
    let tail = after.strip_prefix('"')?;
    let mut out = String::new();
    let mut esc = false;
    for ch in tail.chars() {
        if esc {
            out.push(ch);
            esc = false;
        } else if ch == '\\' {
            esc = true;
        } else if ch == '"' {
            return Some(out);
        } else {
            out.push(ch);
        }
    }
    None
}

fn usage_log_path() -> String {
    common::usage_log_paths().0
}

/// Request-body bytes charged to one attempt line of the usage log.
///
/// The body belongs to the *request*, and the client sent it once, so it is
/// charged on the first attempt only — `attempt` being the 0-based index of
/// this attempt. Every outcome that logs shares this one function, which is
/// the point: the previous rule was written out three times in the retry arms
/// and nowhere else, so a request that failed over and then *succeeded* — the
/// shape that actually happens — was charged twice, once on the discarded
/// attempt and again on the answer the client received.
fn attempt_charge(attempt: u32, body_bytes: u64) -> u64 {
    if attempt == 0 { body_bytes } else { 0 }
}

/// One JSONL line per *attempt* at a request. Rotated past 8MB (previous
/// window kept beside it as `.1`) so the log can never fill a disk by itself.
///
/// A retried request produces several lines sharing one `rid`, one per backend
/// tried. `attempt` is the 0-based index of that attempt, so the lines of one
/// `rid` are numbered 0, 1, 2 in the order they were made. `req_body`, and
/// the `est_tokens` derived from it, are charged on attempt 0 only — those
/// bytes belong to the request and the client sent them once however many
/// backends it was tried against. `resp_body` is per attempt, because those
/// bytes really were produced and really did reach the client.
///
/// So the summing rule a reader needs is: add `est_tokens` over lines, which
/// counts the request once and every generated byte, or add `req_body` only
/// where `attempt` is 0. Both now agree, which they did not when the first
/// attempt's index was only known after its port had been pushed onto the
/// tried list, and when only three of the eight outcomes carried the rule.
#[allow(clippy::too_many_arguments)]
fn log_usage(
    rid: usize,
    backend: &str,
    port: u16,
    model: &str,
    up_body: u64,
    down_body: u64,
    est_tokens: u64,
    ttfb_ms: Option<f64>,
    outcome: &str,
    attempt: u32,
) {
    let path = common::usage_log_paths().0;
    // One rotation rule, shared with the daemons' launchd logs. The rotated
    // name comes from the same helper `kv-manage` reads, so the generation
    // this moves is one the reader will actually find, and `keep = 1` is the
    // same "exactly one previous generation" policy the rename had — the
    // reader sums exactly two files and a third would be a file nothing reads.
    //
    // The inline version of this was `metadata(path).len() > USAGE_ROTATE_BYTES`
    // then a bare rename: the same rule as `common::rotate_log_if_large`, in a
    // second implementation, with the generation handling written out again.
    common::rotate_log_if_large(&path, USAGE_ROTATE_BYTES, 1);
    let line = format!(
        "{{\"ts\":{},\"rid\":{},\"attempt\":{},\"backend\":\"{}\",\"port\":{},\"model\":\"{}\",\"req_body\":{},\"resp_body\":{},\"est_tokens\":{},\"ttfb_ms\":{},\"outcome\":\"{}\"}}",
        common::now_unix(),
        rid,
        attempt,
        backend,
        port,
        common::json_escape(model),
        up_body,
        down_body,
        est_tokens,
        ttfb_ms.map(|v| format!("{:.1}", v)).unwrap_or_else(|| "null".to_string()),
        outcome,
    );
    common::append_jsonl(&path, &line);
}

// ---------------------------------------------------------- request parsing -

fn find_headers_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
}

/// Why the header block could not be read. Split out so the caller can
/// answer with the right status: a client that legitimately sends more than
/// 64 KiB of headers was not truncated, it was too large, and telling it
/// "incomplete request" sends it looking for a network fault.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum HeaderError {
    Incomplete,
    TooLarge,
}

fn read_headers(client: &TcpStream) -> Result<Vec<u8>, HeaderError> {
    // Owned handle: bytes consumed here are the client's already-sent
    // headers; the caller forwards them explicitly and streams the rest.
    let mut stream = client.try_clone().map_err(|_| HeaderError::Incomplete)?;
    let deadline = Instant::now() + header_budget();
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut chunk = [0u8; 8192];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(HeaderError::Incomplete);
        }
        if stream.set_read_timeout(Some(remaining)).is_err() {
            return Err(HeaderError::Incomplete);
        }
        let n = stream.read(&mut chunk).map_err(|_| HeaderError::Incomplete)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(end) = find_headers_end(&buf) {
            if end > MAX_HEADERS {
                return Err(HeaderError::TooLarge);
            }
            break;
        }
        if buf.len() > MAX_HEADERS {
            return Err(HeaderError::TooLarge);
        }
    }
    if find_headers_end(&buf).is_none() {
        return Err(HeaderError::Incomplete);
    }
    Ok(buf)
}

/// Best-effort pipelining check: drain whatever the client has *already*
/// queued behind the request we just framed. A second request that arrives
/// after this drain is not detected — the forwarded bytes are still cut at
/// the first request's framing, so the trailing request is never relayed
/// upstream (no smuggling), it is simply dropped when the connection ends.
fn has_queued_bytes(mut client: &TcpStream) -> bool {
    if client.set_nonblocking(true).is_err() {
        return false;
    }
    let mut buffer = [0u8; 4096];
    let mut queued = false;
    let mut drained = 0usize;
    loop {
        match client.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                queued = true;
                drained += n;
                if drained >= MAX_BODY {
                    break;
                }
            }
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock
                    || error.kind() == io::ErrorKind::TimedOut =>
            {
                break;
            }
            Err(_) => break,
        }
    }
    let reset = client.set_nonblocking(false).is_ok();
    queued || !reset
}

fn valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-'
                        | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
                )
        })
}

fn strict_header_lines(headers: &[u8]) -> Result<Vec<String>, ()> {
    for (index, byte) in headers.iter().enumerate() {
        if *byte == b'\n' && (index == 0 || headers[index - 1] != b'\r') {
            return Err(());
        }
        if *byte == b'\r'
            && (index + 1 >= headers.len() || headers[index + 1] != b'\n')
        {
            return Err(());
        }
    }
    let text = String::from_utf8_lossy(headers);
    Ok(text.split("\r\n").map(str::to_string).collect())
}

fn parse_header_line(line: &str) -> Result<(&str, &str), ()> {
    if line.is_empty() {
        return Err(());
    }
    if line
        .as_bytes()
        .first()
        .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
    {
        return Err(());
    }
    let (name, value) = line.split_once(':').ok_or(())?;
    if !valid_header_name(name)
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() && byte != b'\t')
    {
        return Err(());
    }
    Ok((name, value))
}

fn parse_response_header_line(line: &str) -> Result<(&str, &str), ()> {
    if line.is_empty()
        || line
            .as_bytes()
            .first()
            .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
    {
        return Err(());
    }
    let (name, value) = line.split_once(':').ok_or(())?;
    let name = name.trim_matches(|character| matches!(character, ' ' | '\t'));
    if !valid_header_name(name)
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() && byte != b'\t')
    {
        return Err(());
    }
    Ok((name, value))
}

fn transfer_encoding_tokens(headers: &[u8], response: bool) -> Result<Vec<String>, ()> {
    let lines = strict_header_lines(headers).map_err(|_| ())?;
    let mut tokens = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if line.is_empty() {
            break;
        }
        if index == 0 {
            continue;
        }
        let (name, value) = if response {
            let (name, value) = parse_response_header_line(line).map_err(|_| ())?;
            if matches!(
                name.to_ascii_lowercase().as_str(),
                "content-length"
                    | "transfer-encoding"
                    | "content-type"
                    | "content-encoding"
                    | "trailer"
            ) {
                parse_header_line(line).map_err(|_| ())?
            } else {
                (name, value)
            }
        } else {
            parse_header_line(line).map_err(|_| ())?
        };
        if name.eq_ignore_ascii_case("transfer-encoding") {
            tokens.extend(value.split(',').map(|token| {
                token
                    .trim_matches(|character| matches!(character, ' ' | '\t'))
                    .to_string()
            }));
        }
    }
    if tokens
        .iter()
        .any(|token| token.is_empty() || !token.bytes().all(is_token_byte))
    {
        return Err(());
    }
    Ok(tokens)
}

fn chunked_result(tokens: Vec<String>) -> Result<bool, ()> {
    if tokens.is_empty() {
        return Ok(false);
    }
    if tokens.last().is_some_and(|token| token.eq_ignore_ascii_case("chunked"))
        && tokens.iter().filter(|token| token.eq_ignore_ascii_case("chunked")).count() == 1
    {
        return Ok(true);
    }
    Err(())
}

fn expect_continue(headers: &[u8]) -> bool {
    let Ok(lines) = strict_header_lines(headers) else {
        return false;
    };
    lines.iter().skip(1).any(|line| {
        if line.is_empty() {
            return false;
        }
        let Ok((name, value)) = parse_header_line(line) else {
            return false;
        };
        name.eq_ignore_ascii_case("expect") && expect_continue_value(value)
    })
}

fn has_chunked_transfer_encoding(headers: &[u8]) -> Result<bool, ()> {
    chunked_result(transfer_encoding_tokens(headers, false)?)
}

fn has_chunked_response_transfer_encoding(headers: &[u8]) -> Result<bool, ()> {
    chunked_result(transfer_encoding_tokens(headers, true)?)
}

fn plain_chunked_transfer_encoding(headers: &[u8]) -> Result<Option<bool>, ()> {
    let tokens = transfer_encoding_tokens(headers, true)?;
    if tokens.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        tokens.len() == 1 && tokens[0].eq_ignore_ascii_case("chunked"),
    ))
}

/// Strip hop-by-hop credentials and normalize framing before forwarding:
/// gateway `Authorization` never reaches an inference backend, the
/// `100-continue` expectation is dropped because we already answered it
/// ourselves, `Content-Length` is rewritten once in canonical decimal so a
/// duplicate pair can never reach a strict backend as-is, and every
/// connection-scoped header is removed because this hop is a new connection
/// and those headers describe the client's connection, not the request.
///
/// `Content-Length` and `Transfer-Encoding` are deliberately *not* treated as
/// hop-by-hop here, even though RFC 7230 lists them as such in some readings:
/// the body is relayed as wire bytes, so the backend has to agree with the
/// framing the router already parsed and validated. Rewriting or dropping
/// either would desynchronize the two, which is a far worse failure than
/// forwarding a stale hop-by-hop field.
fn sanitize_upstream_request(request: &[u8]) -> Vec<u8> {
    let Some(header_len) = find_headers_end(request) else {
        return request.to_vec();
    };
    // Two passes: the client's own `Connection` list names fields that are
    // hop-by-hop *for this request*, and they can appear after it.
    let drop_names = connection_scoped_names(&request[..header_len]);
    let mut sanitized = Vec::with_capacity(request.len());
    let mut content_length_seen = false;
    for line in request[..header_len].split_inclusive(|byte| *byte == b'\n') {
        let trimmed = line.strip_suffix(b"\n").unwrap_or(line);
        let trimmed = trimmed.strip_suffix(b"\r").unwrap_or(trimmed);
        if trimmed.is_empty() {
            sanitized.extend_from_slice(b"\r\n");
            continue;
        }
        if let Some(colon) = trimmed.iter().position(|byte| *byte == b':') {
            let name = String::from_utf8_lossy(&trimmed[..colon]);
            let name = name.trim();
            if name.eq_ignore_ascii_case("authorization")
                || name.eq_ignore_ascii_case("proxy-authorization")
            {
                continue;
            }
            let value = String::from_utf8_lossy(&trimmed[colon + 1..]);
            if name.eq_ignore_ascii_case("expect") && expect_continue_value(&value) {
                continue;
            }
            if is_connection_scoped(name) || names_a_connection_token(&drop_names, name) {
                continue;
            }
            if name.eq_ignore_ascii_case("content-length") {
                if let Ok(parsed) = value
                    .trim_matches(|character| matches!(character, ' ' | '\t'))
                    .parse::<usize>()
                {
                    if content_length_seen {
                        continue;
                    }
                    content_length_seen = true;
                    let canonical = format!("Content-Length: {parsed}\r\n");
                    sanitized.extend_from_slice(canonical.as_bytes());
                    continue;
                }
            }
        }
        sanitized.extend_from_slice(line);
    }
    sanitized.extend_from_slice(&request[header_len..]);
    sanitized
}

/// Headers that describe one connection rather than one message. RFC 7230
/// §6.1 requires a gateway to remove these before forwarding, and a client
/// that is allowed to set them can otherwise dictate how the *backend*
/// connection is managed — or smuggle a field of its choosing by naming it in
/// `Connection`, which is the documented way to say "this one is hop-by-hop".
fn is_connection_scoped(name: &str) -> bool {
    name.eq_ignore_ascii_case("connection")
        || name.eq_ignore_ascii_case("proxy-connection")
        || name.eq_ignore_ascii_case("keep-alive")
        || name.eq_ignore_ascii_case("te")
        || name.eq_ignore_ascii_case("trailer")
        || name.eq_ignore_ascii_case("upgrade")
}

/// Whether a `Connection` token list names this field. Header names are
/// case-insensitive, so the comparison is too.
fn names_a_connection_token(tokens: &[String], name: &str) -> bool {
    tokens
        .iter()
        .any(|token| token.eq_ignore_ascii_case(name))
}

/// Every field name listed in a `Connection` header of this message, lowercased.
fn connection_scoped_names(headers: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    for line in headers.split_inclusive(|byte| *byte == b'\n') {
        let trimmed = line
            .strip_suffix(b"\n")
            .unwrap_or(line)
            .strip_suffix(b"\r")
            .unwrap_or(line);
        let Some(colon) = trimmed.iter().position(|byte| *byte == b':') else {
            continue;
        };
        let name = String::from_utf8_lossy(&trimmed[..colon]);
        if !name.trim().eq_ignore_ascii_case("connection") {
            continue;
        }
        for token in String::from_utf8_lossy(&trimmed[colon + 1..]).split(',') {
            let token = token
                .trim_matches(|character| matches!(character, ' ' | '\t'))
                .to_ascii_lowercase();
            if !token.is_empty() {
                names.push(token);
            }
        }
    }
    names
}

/// The backend's own response head, made honest for the connection the router
/// actually offers.
///
/// The relay serves exactly one request per TCP connection and then shuts the
/// socket down, but the backend's head was forwarded byte for byte — including
/// `Connection: keep-alive` and `Keep-Alive: timeout=N`. A client that honors
/// the header the router just relayed sends request #2 on a socket that is
/// already closed: `curl` reports "Connection reset by peer" and does not retry
/// a POST. The router's own replies already said `Connection: close`, so the
/// two classes of answer disagreed about the same connection. Everything else
/// — status line, `Content-Type`, `Content-Length`, `Transfer-Encoding`,
/// cookies, custom fields — is passed through in order, because a client that
/// trusted the backend's headers still has to be able to trust them.
fn sanitize_downstream_head(head: &[u8]) -> Vec<u8> {
    let drop_names = connection_scoped_names(head);
    let mut out = Vec::with_capacity(head.len() + 32);
    for line in head.split_inclusive(|byte| *byte == b'\n') {
        let trimmed = line
            .strip_suffix(b"\n")
            .unwrap_or(line)
            .strip_suffix(b"\r")
            .unwrap_or(line);
        if trimmed.is_empty() {
            continue;
        }
        if let Some(colon) = trimmed.iter().position(|byte| *byte == b':') {
            let name = String::from_utf8_lossy(&trimmed[..colon]);
            let name = name.trim();
            if is_connection_scoped(name) || names_a_connection_token(&drop_names, name) {
                continue;
            }
        }
        out.extend_from_slice(line);
    }
    out.extend_from_slice(b"Connection: close\r\n\r\n");
    out
}

/// True when an `Expect` header value contains the `100-continue` token. A
/// header can legally carry a list (`Expect: foo, 100-continue`), and the two
/// sides of that question have to agree: the router answers `100 Continue`
/// itself and the sanitized request must not still carry an expectation the
/// backend would then wait for.
fn expect_continue_value(value: &str) -> bool {
    value.split(',').any(|token| {
        token
            .trim_matches(|character| matches!(character, ' ' | '\t'))
            .eq_ignore_ascii_case("100-continue")
    })
}

enum BodyError {
    TooLarge,
    Invalid,
    /// The router is already buffering as many request bodies as it will
    /// hold. Not the client's fault, and not the per-request size limit.
    Busy,
}

/// Aggregate ceiling on the request bodies held in memory at once. One
/// connection is already bounded by MAX_BODY, but MAX_INFLIGHT connections
/// buffering one each is a much larger number, so the total needs its own
/// bound: without it, a handful of parallel uploads can pin a gigabyte of
/// heap for as long as the body budget lets them dribble.
///
/// Charged against allocated capacity, not bytes consumed, so this number
/// is the memory actually held. Per-connection fixed costs (the header
/// buffer and one 16 KiB read buffer, well under 256 KiB) sit outside the
/// budget, which is why the request-side worst case is this plus
/// MAX_INFLIGHT of those. A relayed response also buffers its own header
/// block (up to 64 KiB, grown to ~128 KiB of capacity) outside this budget.
const MAX_BUFFERED_BODY: usize = 256 * 1024 * 1024;

/// Retryable "the router is full", distinct from the 503 raised when there
/// is no backend: both are retryable, but only one means try another port.
const BODY_BUDGET_BUSY: &str =
    "router is buffering as many request bodies as it will hold; retry once a request finishes";

/// Distinct error types for the two retryable saturation causes, so a client
/// (or an operator reading its logs) can tell them apart.
const BUSY_TYPE: &str = "lac_router_busy";
const BODY_BUDGET_TYPE: &str = "lac_router_body_budget";

/// Env-overridable so tests can drive the cap with a small body. An
/// out-of-range value is ignored (the startup banner shows what applied).
///
/// Note the effective per-request ceiling this implies: a request costs up
/// to three times its declared body length, so the largest single body that
/// can always be served is about a third of this. Set it to at least a few
/// times the largest body you expect, or a mid-sized upload will be refused
/// as "busy" on an otherwise idle router.
fn max_buffered_body() -> usize {
    env::var("LOOP_STUDIO_ROUTER_MAX_BUFFERED_BODY")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|&bytes| (64 * 1024..=(1 << 30)).contains(&bytes))
        .unwrap_or(MAX_BUFFERED_BODY)
}

#[derive(Clone)]
struct BodyBudget {
    held: Arc<AtomicUsize>,
    limit: usize,
}

/// Holds this connection's share of the body budget for as long as its
/// buffered request is alive, and gives it back on every exit path.
struct BodyReservation {
    budget: BodyBudget,
    held: usize,
}

impl BodyReservation {
    /// Claim `bytes`, or refuse. The compare-exchange keeps the cap exact
    /// under concurrency rather than merely approximate.
    fn try_reserve(&mut self, bytes: usize) -> bool {
        let mut current = self.budget.held.load(Ordering::SeqCst);
        loop {
            let Some(next) = current.checked_add(bytes) else {
                return false;
            };
            if next > self.budget.limit {
                return false;
            }
            match self.budget.held.compare_exchange_weak(
                current,
                next,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => {
                    self.held = self.held.saturating_add(bytes);
                    return true;
                }
                Err(observed) => current = observed,
            }
        }
    }

    /// Whether `bytes` could fit right now. A peek, not a claim: use it to
    /// fail fast on a declared length, and let try_reserve do the accounting.
    fn fits(&self, bytes: usize) -> bool {
        self.budget
            .held
            .load(Ordering::SeqCst)
            .checked_add(bytes)
            .is_some_and(|next| next <= self.budget.limit)
    }

    /// Hand bytes back when the allocation they were charged for is gone.
    fn release(&mut self, bytes: usize) {
        let give = bytes.min(self.held);
        self.held -= give;
        self.budget.held.fetch_sub(give, Ordering::SeqCst);
    }
}

impl Drop for BodyReservation {
    fn drop(&mut self) {
        if self.held > 0 {
            // A wrap here would silently pin the counter near usize::MAX and
            // 503 every later request for the life of the process, so make a
            // broken invariant a loud test failure instead.
            debug_assert!(
                self.held <= self.budget.held.load(Ordering::SeqCst),
                "body budget underflow: releasing {} of {}",
                self.held,
                self.budget.held.load(Ordering::SeqCst)
            );
            self.budget.held.fetch_sub(self.held, Ordering::SeqCst);
        }
    }
}

struct RequestFrame {
    wire: Vec<u8>,
    has_following: bool,
}

/// Body-read budgets, resolved once per request so a 16 MiB upload does
/// not re-read the environment on every 16 KiB read.
#[derive(Clone, Copy)]
struct RequestBudgets {
    stream: Duration,
    idle: Duration,
}

fn read_body_bytes(
    mut client: &TcpStream,
    buffer: &mut [u8],
    started: Instant,
    budgets: RequestBudgets,
) -> Result<usize, BodyError> {
    let remaining = budgets
        .stream
        .saturating_sub(started.elapsed())
        .min(budgets.idle);
    if remaining.is_zero() {
        return Err(BodyError::Invalid);
    }
    client
        .set_read_timeout(Some(remaining))
        .map_err(|_| BodyError::Invalid)?;
    client.read(buffer).map_err(|_| BodyError::Invalid)
}

fn read_request_body(
    client: &TcpStream,
    initial: &[u8],
    body_budget: &mut BodyReservation,
) -> Result<RequestFrame, BodyError> {
    let header_len = find_headers_end(initial).ok_or(BodyError::Invalid)?;
    let budgets = RequestBudgets {
        stream: stream_budget(),
        idle: body_idle_timeout(),
    };
    let mut request = initial.to_vec();
    // A declared Content-Length is known before a single body byte is read,
    // so an upload that cannot fit is refused up front rather than after
    // pulling the whole thing into memory first. This only peeks: the real
    // charge happens as the buffer's capacity grows below, so the body is
    // not counted twice.
    //
    // Peek the worst case, not the declared length. A request actually costs
    // up to three times it: the frame buffer (which Vec doubles, so up to
    // 2x) plus the sanitized copy it is forwarded as. Peeking only the
    // declared length would let two thirds of an unfittable upload through
    // to be read and refused afterwards, which is what this check exists to
    // avoid. The cost of over-peeking is a slightly lower effective ceiling,
    // which max_buffered_body documents.
    if let Some(declared) =
        parse_content_length_strict(&initial[..header_len]).map_err(|_| BodyError::Invalid)?
    {
        if !body_budget.fits(declared.saturating_mul(3)) {
            return Err(BodyError::Busy);
        }
    }
    // Charge allocated capacity, not consumed bytes: Vec grows by doubling,
    // so N bytes of body can cost up to 2N, and a size chosen just past a
    // doubling boundary gets that slack for free. What is over-read into
    // the header buffer is part of this allocation too.
    let mut charged = request.capacity();
    if charged > 0 && !body_budget.try_reserve(charged) {
        return Err(BodyError::Busy);
    }
    let mut buffer = [0u8; 16384];
    let body_started = Instant::now();
    if let Some(content_len) =
        parse_content_length_strict(&initial[..header_len]).map_err(|_| BodyError::Invalid)?
    {
        let target = header_len.checked_add(content_len).ok_or(BodyError::Invalid)?;
        // Defence in depth: handle_connection already rejects a declared
        // length over MAX_BODY from these same bytes, so this cannot fire
        // today. It stays so a future caller of read_request_body that skips
        // that gate still cannot pull an unbounded body into memory.
        if target > header_len.saturating_add(MAX_BODY) {
            return Err(BodyError::TooLarge);
        }
        while request.len() < target {
            let remaining = target - request.len();
            let to_read = buffer.len().min(remaining);
            let n = read_body_bytes(client, &mut buffer[..to_read], body_started, budgets)?;
            if n == 0 {
                return Err(BodyError::Invalid);
            }
            request.extend_from_slice(&buffer[..n]);
            let capacity = request.capacity();
            if capacity > charged {
                if !body_budget.try_reserve(capacity - charged) {
                    return Err(BodyError::Busy);
                }
                charged = capacity;
            }
        }
        let mut has_following = request.len() > target;
        if !has_following {
            has_following = has_queued_bytes(client);
        }
        request.truncate(target);
        return Ok(RequestFrame {
            wire: request,
            has_following,
        });
    }
    if has_chunked_transfer_encoding(&initial[..header_len]).map_err(|_| BodyError::Invalid)? {
        // Wire bytes, so this is MAX_BODY of payload plus chunk framing and
        // trailers. Unreachable today for the same reason as the check above.
        let max_request = header_len.saturating_add(MAX_BODY + MAX_HEADERS);
        if request.len() > max_request {
            return Err(BodyError::TooLarge);
        }
        let mut detector = ChunkedDetector::default();
        let mut fed = header_len;
        loop {
            match detector.feed(&request[fed..]).map_err(|_| BodyError::Invalid)? {
                ChunkProgress::Complete { consumed } => {
                    let end = fed + consumed;
                    let mut has_following = end < request.len();
                    if !has_following {
                        has_following = has_queued_bytes(client);
                    }
                    let wire = request[..end].to_vec();
                    // Charge the replacement before releasing the old
                    // buffer's share: `request` is still live here, and a
                    // release-first transfer would under-count its whole
                    // capacity for an instant, admitting a connection that
                    // does not fit.
                    if !body_budget.try_reserve(wire.capacity()) {
                        return Err(BodyError::Busy);
                    }
                    body_budget.release(charged);
                    return Ok(RequestFrame { wire, has_following });
                }
                ChunkProgress::NeedMore => {
                    let n = read_body_bytes(client, &mut buffer, body_started, budgets)?;
                    if n == 0 {
                        return Err(BodyError::Invalid);
                    }
                    fed = request.len();
                    request.extend_from_slice(&buffer[..n]);
                    // Per-request limit before budget: a chunked body past
                    // MAX_BODY is 413 whatever the buffer budget is doing,
                    // and checking the other way round would report a body
                    // that is simply too large as "busy".
                    if request.len() > max_request {
                        return Err(BodyError::TooLarge);
                    }
                    let capacity = request.capacity();
                    if capacity > charged {
                        if !body_budget.try_reserve(capacity - charged) {
                            return Err(BodyError::Busy);
                        }
                        charged = capacity;
                    }
                }
            }
        }
    }
    // No framing at all, which for a routed request means a body-less
    // GET/HEAD. Only the headers survive, so give the buffer's share back
    // rather than holding it for the whole relay.
    let mut has_following = request.len() > header_len;
    if !has_following {
        has_following = has_queued_bytes(client);
    }
    let wire = request[..header_len].to_vec();
    if !body_budget.try_reserve(wire.capacity()) {
        return Err(BodyError::Busy);
    }
    body_budget.release(charged);
    Ok(RequestFrame { wire, has_following })
}

fn valid_request_line(line: &str) -> bool {
    let bytes = line.as_bytes();
    let Some(first_space) = bytes.iter().position(|byte| *byte == b' ') else {
        return false;
    };
    let Some(second_space) = bytes[first_space + 1..]
        .iter()
        .position(|byte| *byte == b' ')
        .map(|index| index + first_space + 1)
    else {
        return false;
    };
    let method = &line[..first_space];
    let target = &line[first_space + 1..second_space];
    let version = &line[second_space + 1..];
    valid_header_name(method)
        && !target.is_empty()
        && target
            .bytes()
            .all(|byte| (0x21..=0x7e).contains(&byte))
        && matches!(version, "HTTP/1.0" | "HTTP/1.1")
}

fn request_line(buf: &[u8]) -> (String, String) {
    let head = String::from_utf8_lossy(buf);
    let first = head.lines().next().unwrap_or("");
    let mut parts = first.split_whitespace();
    (
        parts.next().unwrap_or("").to_string(),
        parts.next().unwrap_or("/").to_string(),
    )
}

// -------------------------------------------------------------- http blobs --

/// The CORS block for a response, or nothing at all. A native client gets an
/// empty string, which is the whole point: no `Origin` means no
/// browser-readable surface. An allow-listed browser origin gets its own
/// exact value back — never `*` unless the operator asked for `*` — plus
/// `Vary: Origin` so a shared cache cannot hand that reply to a page which
/// was not allowed.
fn cors_headers(cors: Option<&str>) -> String {
    match cors {
        Some(origin) => format!("Access-Control-Allow-Origin: {origin}\r\nVary: Origin\r\n"),
        None => String::new(),
    }
}

fn http_response(status: &str, content_type: &str, body: &str, cors: Option<&str>) -> String {
    format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n{}\r\n{}",
        status,
        content_type,
        body.len(),
        cors_headers(cors),
        body
    )
}

fn json_response(
    stream: &mut TcpStream,
    status: &str,
    body: &str,
    cors: Option<&str>,
) -> io::Result<()> {
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    stream.write_all(http_response(status, "application/json", body, cors).as_bytes())
}

/// A response the caller should retry rather than shrink or give up on.
/// Carries Retry-After so a client has something to back off against, and a
/// distinct error type per cause: "too many connections" and "no buffer
/// room" are both retryable, but an operator debugging them needs to tell
/// them apart.
fn retryable_http_response(
    status: &str,
    error_type: &str,
    message: &str,
    retry_after_secs: u64,
    cors: Option<&str>,
) -> String {
    // Take the code from the status rather than hardcoding one, so the body
    // cannot contradict the status line.
    let code = status.split_whitespace().next().unwrap_or("503");
    debug_assert!(
        code.len() == 3 && code.bytes().all(|byte| byte.is_ascii_digit()),
        "status must lead with a three-digit code: {status}"
    );
    let body = format!(
        "{{\"error\":{{\"message\":\"{}\",\"type\":\"{}\",\"code\":{}}}}}",
        common::json_escape(message),
        common::json_escape(error_type),
        code
    );
    format!(
        "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nRetry-After: {}\r\nConnection: close\r\n{}\r\n{}",
        status,
        body.len(),
        retry_after_secs,
        cors_headers(cors),
        body
    )
}

#[allow(clippy::too_many_arguments)]
fn retryable_response(
    stream: &mut TcpStream,
    status: &str,
    error_type: &str,
    message: &str,
    retry_after_secs: u64,
    cors: Option<&str>,
) -> io::Result<()> {
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    stream.write_all(
        retryable_http_response(status, error_type, message, retry_after_secs, cors).as_bytes(),
    )
}

// --------------------------------------------------------------- /lac/* -----

fn backend_pref_path() -> String {
    format!("{}/.lac/router-backend", common::home_dir())
}

fn load_persisted_backend() -> usize {
    std::fs::read_to_string(backend_pref_path())
        .ok()
        .map(|s| backend_id(s.trim()))
        .unwrap_or(BACKEND_AUTO)
}

/// Write the chosen backend so it survives a restart. Returns whether it
/// actually reached the disk: the switch takes effect in memory either way, but
/// the reply used to say `"persisted": true` unconditionally while the error
/// was dropped, so a read-only or full `~/.lac` silently lost the operator's
/// choice and the gateway came back on the previous lane.
fn persist_backend(id: usize) -> bool {
    common::atomic_write(&backend_pref_path(), backend_name(id)).is_ok()
}

#[allow(clippy::too_many_arguments)]
fn handle_lac_status(
    mut stream: TcpStream,
    preferred: usize,
    hc: &HealthCache,
    stats: &StatsMap,
    routes: &RouteMap,
    host_facts: &HostFactsCache,
    started: Instant,
    inflight: usize,
    cors: Option<&str>,
) -> io::Result<()> {
    let mlx_port = port_mlx();
    let llama_port = port_llama();
    let ollama_port = port_ollama();
    let mlx_up = hc.ready(mlx_port);
    let llama_up = hc.ready(llama_port);
    let ollama_up = hc.ready(ollama_port);
    let order = pick_backends(preferred, hc, stats, None);
    // The backend this request would actually go to, or nothing. The old
    // fallback invented one: with every backend down it reported the
    // *preferred* id — which is the literal string "auto" for the default —
    // and its port, so `/lac/status` said `"status": "ok"` and named an active
    // lane while the `backends` block beside it measured all three down. That
    // is the failure mode this file exists to prevent: a reader must never be
    // told a lane is active when nothing is listening on it.
    let (active_name, active_port) = match order.first().copied() {
        Some((bid, port)) => (backend_name(bid).to_string(), port),
        None => ("none".to_string(), 0),
    };
    let serving = !order.is_empty();
    let router_state = if serving { "ok" } else { "no-backends" };

    // Measured per-backend stats (zeros when nothing proxied yet).
    let mut stats_json = String::from("{");
    for (i, (bid, port)) in tier_backends().iter().enumerate() {
        let (ewma, ok, err, toks) = stats
            .lock()
            .ok()
            .and_then(|m| m.get(port).cloned())
            .map(|s| (s.ewma_ms, s.ok, s.err, s.est_tokens))
            .unwrap_or((None, 0, 0, 0));
        if i > 0 {
            stats_json.push(',');
        }
        stats_json.push_str(&format!(
            "\"{}\":{{\"port\":{},\"ewma_ms\":{},\"ok\":{},\"err\":{},\"est_tokens\":{}}}",
            backend_name(*bid),
            port,
            ewma.map(|v| format!("{:.1}", v)).unwrap_or_else(|| "null".to_string()),
            ok,
            err,
            toks,
        ));
    }
    stats_json.push('}');
    let models_mapped = routes.lock().map(|r| r.len()).unwrap_or(0);

    // Host facts, measured here on a timer so the poll that already fetches
    // this document gets them for free. Absent until the first refresh lands,
    // and absent rather than stale-looking when the probes cannot answer.
    let host_json = match host_facts.serve() {
        Some(facts) => format!(
            concat!(
                ",\n  \"host\": {{\n",
                "    \"thermal\": {},\n",
                "    \"free_ram_gib\": {},\n",
                "    \"total_ram_gib\": {}\n",
                "  }}"
            ),
            // `null` when the probe could not answer, matching the two RAM
            // fields. A client that reads this document must be able to tell
            // "not measured" from "measured, and cool".
            facts
                .thermal
                .as_deref()
                .map(|t| format!("\"{}\"", common::json_escape(t)))
                .unwrap_or_else(|| "null".to_string()),
            facts
                .free_ram_gib
                .map(|v| format!("{v:.1}"))
                .unwrap_or_else(|| "null".to_string()),
            facts
                .total_ram_gib
                .map(|v| format!("{v:.1}"))
                .unwrap_or_else(|| "null".to_string()),
        ),
        None => String::new(),
    };

    let body = format!(
        concat!(
            "{{\n  \"status\": \"{}\",\n  \"router\": \"loop-studio-router v2.8\",\n",
            "  \"preferred\": \"{}\",\n  \"active\": \"{}\",\n  \"target_port\": {},\n",
            "  \"uptime_secs\": {},\n  \"inflight\": {},\n  \"models_mapped\": {},\n",
            "  \"usage_log\": \"{}\",\n",
            "  \"backends\": {{\n",
            "    \"mlx\": {{ \"port\": {}, \"up\": {} }},\n",
            "    \"llama\": {{ \"port\": {}, \"up\": {} }},\n",
            "    \"ollama\": {{ \"port\": {}, \"up\": {} }}\n  }},\n",
            "  \"stats\": {}{}\n}}"
        ),
        router_state,
        backend_name(preferred),
        active_name,
        active_port,
        started.elapsed().as_secs(),
        inflight,
        models_mapped,
        common::json_escape(&usage_log_path()),
        mlx_port,
        mlx_up,
        llama_port,
        llama_up,
        ollama_port,
        ollama_up,
        stats_json,
        host_json,
    );
    json_response(&mut stream, "200 OK", &body, cors)
}

fn handle_lac_switch(
    mut stream: TcpStream,
    path: &str,
    preferred: &Arc<AtomicUsize>,
    cors: Option<&str>,
) -> io::Result<()> {
    let query = path.split('?').nth(1).unwrap_or("");
    let target_param = query
        .split('&')
        .filter_map(|kv| {
            let mut it = kv.splitn(2, '=');
            match (it.next(), it.next()) {
                (Some("target"), Some(v)) => Some(v),
                _ => None,
            }
        })
        .next()
        .unwrap_or("");
    // An operator asking for a lane, and mistyping it, must not be answered by
    // silently choosing a different one. `backend_id` resolved anything
    // unrecognised to `auto`, so `?target=mlxx` repointed the whole gateway and
    // *persisted it*, and the reply said `"status": "switched"`. Refused, with
    // the list, and nothing written.
    let requested = if !target_param.is_empty() {
        match backend_id_strict(target_param) {
            Some(id) => Some(id),
            None => {
                eprintln!(
                    "[loop-studio-router] /loop-studio/switch: unknown target {target_param:?}; not switching. Known: {}",
                    backend_name_list()
                );
                return json_response(
                    &mut stream,
                    "400 Bad Request",
                    &format!(
                        "{{\n  \"status\": \"rejected\",\n  \"reason\": \"unknown backend\",\n  \"target\": {},\n  \"known\": \"{}\"\n}}",
                        common::json_escape(target_param),
                        backend_name_list()
                    ),
                    cors,
                );
            }
        }
    } else if path.contains("mlx") {
        Some(BACKEND_MLX)
    } else if path.contains("llama") {
        Some(BACKEND_LLAMA)
    } else if path.contains("ollama") {
        Some(BACKEND_OLLAMA)
    } else {
        // No target named at all: the documented "go back to auto".
        Some(BACKEND_AUTO)
    };
    let new_target = requested.unwrap_or(BACKEND_AUTO);
    preferred.store(new_target, Ordering::SeqCst);
    let persisted = persist_backend(new_target);
    let body = format!(
        "{{\n  \"status\": \"switched\",\n  \"target\": \"{}\",\n  \"port\": {},\n  \"persisted\": {}\n}}",
        backend_name(new_target),
        // `fastest` and `auto` are lane *orderings*, not a port, and reporting
        // one is how a caller ends up dialling MLX because it asked for the
        // fastest lane. `null` says there is no such port.
        if new_target == BACKEND_MLX || new_target == BACKEND_LLAMA || new_target == BACKEND_OLLAMA {
            backend_port(new_target).to_string()
        } else {
            "null".to_string()
        },
        persisted
    );
    if !persisted {
        eprintln!(
            "[loop-studio-router] backend choice not persisted: could not write {} — the switch holds until restart",
            backend_pref_path()
        );
    }
    json_response(&mut stream, "200 OK", &body, cors)
}

/// A CORS preflight is answered only for an allow-listed origin. With no
/// `Access-Control-Allow-Origin` the browser's preflight fails and the real
/// request is never sent — the outcome we want for a page we did not
/// authorize, and decided by the same allow-list as the real gate, so the
/// two can never disagree.
fn handle_options(mut stream: TcpStream, cors: Option<&str>) -> io::Result<()> {
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let methods = if cors.is_some() {
        "Access-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type, Authorization\r\n"
    } else {
        ""
    };
    let resp = format!(
        "HTTP/1.1 204 No Content\r\n{}Content-Length: 0\r\nConnection: close\r\n{}\r\n",
        cors_headers(cors),
        methods
    );
    stream.write_all(resp.as_bytes())
}

// ---------------------------------------------------------- models merge ----

/// Locate the `"data": [ ... ]` array span (byte indices of brackets).
fn find_data_array(s: &str) -> Option<(usize, usize)> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'"' {
            // Possible string; check for the key "data".
            let mut j = i + 1;
            while j < b.len() && b[j] != b'"' {
                if b[j] == b'\\' {
                    j += 1;
                }
                j += 1;
            }
            if j < b.len() && &s[i + 1..j] == "data" {
                let mut k = j + 1;
                while k < b.len() && (b[k] as char).is_whitespace() {
                    k += 1;
                }
                // Skip ':' then whitespace, expect '['.
                if k < b.len() && b[k] == b':' {
                    k += 1;
                    while k < b.len() && (b[k] as char).is_whitespace() {
                        k += 1;
                    }
                    if k < b.len() && b[k] == b'[' {
                        let start = k;
                        let mut depth = 0i32;
                        let mut in_str = false;
                        let mut esc = false;
                        let mut m = k;
                        while m < b.len() {
                            let c = b[m];
                            if in_str {
                                if esc {
                                    esc = false;
                                } else if c == b'\\' {
                                    esc = true;
                                } else if c == b'"' {
                                    in_str = false;
                                }
                            } else if c == b'"' {
                                in_str = true;
                            } else if c == b'[' {
                                depth += 1;
                            } else if c == b']' {
                                depth -= 1;
                                if depth == 0 {
                                    return Some((start, m));
                                }
                            }
                            m += 1;
                        }
                        return None;
                    }
                }
            }
            i = j + 1;
        } else {
            i += 1;
        }
    }
    None
}

/// Split top-level `{...}` items of an array body (without brackets).
fn split_items(inner: &str) -> Vec<String> {
    let b = inner.as_bytes();
    let mut items = Vec::new();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    let mut start: Option<usize> = None;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if in_str {
            if esc {
                esc = false;
            } else if c == b'\\' {
                esc = true;
            } else if c == b'"' {
                in_str = false;
            }
        } else if c == b'"' {
            in_str = true;
        } else if c == b'{' {
            if depth == 0 {
                start = Some(i);
            }
            depth += 1;
        } else if c == b'}' {
            depth -= 1;
            if depth == 0 {
                if let Some(s) = start.take() {
                    items.push(inner[s..=i].to_string());
                }
            }
        }
        i += 1;
    }
    items
}

/// Extract the top-level `"id": "<v>"` from a model object.
/// Depth-aware and string-aware: an `"id"` nested in a sub-object or
/// embedded inside a string value can no longer shadow (and drop) the
/// real model id during merge dedup.
fn item_id(item: &str) -> Option<String> {
    let b = item.as_bytes();
    let mut i = 0;
    let mut depth = 0i32;
    while i < b.len() {
        match b[i] {
            b'"' => {
                let (token, next) = scan_string(b, i)?;
                // Lookahead: optional whitespace, then ':'.
                let mut k = next;
                while k < b.len() && (b[k] as char).is_whitespace() {
                    k += 1;
                }
                if depth == 1 && token == "id" && k < b.len() && b[k] == b':' {
                    k += 1;
                    while k < b.len() && (b[k] as char).is_whitespace() {
                        k += 1;
                    }
                    if k < b.len() && b[k] == b'"' {
                        let (val, _) = scan_string(b, k)?;
                        return Some(val);
                    }
                    return None;
                }
                i = next;
            }
            b'{' | b'[' => {
                depth += 1;
                i += 1;
            }
            b'}' | b']' => {
                depth -= 1;
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }
    None
}

/// Scan a `"`-opened string starting at byte `i`; returns the unescaped
/// value and the index just past the closing quote.
fn scan_string(b: &[u8], i: usize) -> Option<(String, usize)> {
    let mut out = String::new();
    let mut esc = false;
    let mut j = i + 1;
    while j < b.len() {
        let c = b[j];
        if esc {
            out.push(c as char);
            esc = false;
        } else if c == b'\\' {
            esc = true;
        } else if c == b'"' {
            return Some((out, j + 1));
        } else {
            out.push(c as char);
        }
        j += 1;
    }
    None
}

/// Aggregate `/v1/models` across live backends (MLX -> llama -> Ollama
/// priority, first id wins). Records id -> backend in the route map so
/// later requests naming a model can go straight there. Falls back to
/// plain forwarding on any parse failure so the endpoint never regresses.
fn merged_models(
    hc: &Arc<HealthCache>,
    routes: &Arc<Mutex<HashMap<String, (u16, Instant)>>>,
    catalog: &CatalogCache,
) -> Option<String> {
    // Cached, fresh or expired. A picker that sees a model list a second stale
    // is doing its job; a picker that blocks behind three `http_get`s is not,
    // and while it blocks it holds one of the router's 128 inflight slots — so
    // an expired entry is served and rebuilt behind the caller.
    // The rebuild closure runs on its own thread, so it must own its
    // dependencies rather than borrow the caller's.
    if let Some(body) = catalog.serve({
        let catalog = Arc::clone(catalog);
        let hc = Arc::clone(hc);
        let routes = Arc::clone(routes);
        move || rebuild_catalog(&catalog, &hc, &routes)
    }) {
        return Some(body);
    }
    // Cold: nothing cached at all. Blocking is correct here and only here —
    // there is no previous answer to serve, and inventing a catalog would be
    // worse than waiting. `fetch` serialises this so a burst of first-callers
    // is still one build.
    catalog.fetch(|| {
        rebuild_catalog(catalog, hc, routes);
        catalog.merged_now()
    })
}

/// Read every ready backend's catalog and merge them, coalescing per backend.
///
/// Never waits on a peer: a backend another thread is already probing is served
/// from that thread's cache entry, or skipped. That is the whole point — the
/// previous version held one mutex across all three `http_get` calls, so one
/// wedged backend serialized the endpoint and, through the inflight slots, the
/// entire router.
fn rebuild_catalog(
    catalog: &CatalogCache,
    hc: &Arc<HealthCache>,
    routes: &RouteMap,
) {
    // The lanes are read in parallel, and this is the second half of the fix
    // for the endpoint being able to take the router down.
    //
    // Sequentially, the cold path cost the *sum* of three `http_get` budgets —
    // up to ~9s for three lanes, plus a readiness probe each — while holding one
    // of the router's 128 inflight slots. One wedged lane, which is exactly what
    // the circuit breaker exists for, therefore delayed every caller by its full
    // budget for no reason: the other two were answering in milliseconds.
    //
    // In parallel the cost is the *slowest* lane, which is the only part that is
    // not a free win. One thread per lane, joined before the merge, so the
    // request thread only ever waits for the slowest probe rather than for the
    // sum, and a lane that hangs costs the same as it always did rather than
    // three times as much.
    let lanes: Vec<(usize, u16)> = tier_backends()
        .into_iter()
        .filter(|(_, port)| hc.ready(*port))
        .collect();
    if lanes.is_empty() {
        return;
    }
    let mut probed: Vec<(usize, u16, Option<String>)> = Vec::with_capacity(lanes.len());
    let mut workers = Vec::with_capacity(lanes.len());
    for (idx, port) in lanes {
        // A fresh per-backend entry needs no probe at all, and taking the
        // lock-free fast path first keeps the common warm case off the thread
        // path entirely.
        if let Some(cached) = catalog.entry(idx) {
            probed.push((idx, port, Some(cached)));
            continue;
        }
        let catalog = Arc::clone(catalog);
        workers.push((
            idx,
            port,
            std::thread::Builder::new()
                .name(format!("lac-catalog-{}", backend_name(idx)))
                .spawn(move || {
                    // Still the `try_lock` probe: if a peer is already on this
                    // backend, this thread declines rather than waiting, and the
                    // peer will populate the entry.
                    (idx, port, catalog.probe(idx, || backend_catalog(port)))
                })
                .ok(),
        ));
    }
    for (idx, port, worker) in workers {
        let Some(handle) = worker else {
            // No thread could be started. Probe inline rather than dropping the
            // lane, so a thread-limit blip degrades to the old behaviour rather
            // than to a silently smaller model list.
            probed.push((idx, port, catalog.probe(idx, || backend_catalog(port))));
            continue;
        };
        match handle.join() {
            Ok(result) => probed.push(result),
            Err(_) => {
                // A panicking probe is a dead lane, not a dead endpoint.
                eprintln!("[loop-studio-router] catalog probe for :{port} panicked; skipping it");
                probed.push((idx, port, None));
            }
        }
    }

    let mut seen: HashSet<String> = HashSet::new();
    let mut items: Vec<String> = Vec::new();
    // Merged in tier order, not completion order, so the document is identical
    // however the probes happened to interleave. A model list that reorders
    // itself between polls is a model list that makes a picker look unstable.
    probed.sort_by_key(|(idx, _, _)| *idx);
    for (_idx, port, body) in probed {
        let Some(body) = body else {
            continue;
        };
        let (s, e) = match find_data_array(&body) {
            Some(v) => v,
            None => continue,
        };
        for item in split_items(&body[s + 1..e]) {
            match item_id(&item) {
                Some(id) => {
                    if let Ok(mut r) = routes.lock() {
                        // Prune stale mappings while we hold the lock.
                        r.retain(|_, (_, t)| t.elapsed() < ROUTE_TTL);
                        r.insert(id.clone(), (port, Instant::now()));
                    }
                    if seen.insert(id) {
                        items.push(item);
                    }
                }
                None => items.push(item),
            }
        }
    }
    // No extractable models anywhere: leave the cache alone so the next caller
    // retries, and fall through to single-backend forwarding (which yields the
    // real backend error) rather than answering a misleading 200 with an empty
    // catalog.
    if items.is_empty() {
        return;
    }
    catalog.put(format!(
        "{{\"object\":\"list\",\"data\":[{}]}}",
        items.join(",")
    ));
}

/// One backend's `/v1/models`, or `None` if it did not answer with a catalog.
fn backend_catalog(port: u16) -> Option<String> {
    match common::http_get(port, "/v1/models", 3000) {
        Some((200, b)) => Some(b),
        _ => None,
    }
}

// -------------------------------------------------------------- forwarding --

enum ForwardError {
    /// TCP connect (or initial write) failed: safe to try the next backend.
    Connect,
    /// Backend returned a retryable HTTP response before headers were forwarded.
    HttpStatus { status: u16, ttfb: Option<f64> },
    /// Stream broke mid-proxy: the response is already partial, do not retry.
    Stream { down: u64, ttfb: Option<f64> },
    /// An EOF-delimited response (no Content-Length, no chunking) was cut
    /// short by a connection reset. No promised frame was violated, so it
    /// is reported separately from a truncated frame — but the upstream is
    /// loopback, so the peer process went away and this is charged.
    StreamCut { down: u64, ttfb: Option<f64> },
    /// The response was unusable before any of it reached the client, so
    /// `down` is always 0: every raise site precedes the first write, and the
    /// one after the relay loop is guarded by `!headers_done`.
    BadResponse { down: u64, ttfb: Option<f64> },
    ClientGone { down: u64, ttfb: Option<f64> },
    /// Backend requested a protocol upgrade that this HTTP relay does not
    /// support.
    UnsupportedUpgrade { ttfb: Option<f64> },
}

/// How a response that was cut short is reported. Both are charged: the
/// upstream hop is always loopback, so a reset is the backend process
/// going away mid-request — not a network event — and it predicts the next
/// request exactly as a hang does. The split is kept for the usage log,
/// where "ended at a boundary the backend never delimited" and "truncated
/// inside a frame it did promise" are different bugs to chase.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum StreamEnd {
    /// Connection reset mid-body: the peer process went away.
    Reset,
    /// Silent until the stream budget ran out: the backend is not talking.
    Hung,
}

/// Split the read errors that end a response. A timeout reaching this point
/// has already outlived the budget (the keepalive arm handles the rest).
fn classify_read_error(kind: io::ErrorKind) -> StreamEnd {
    match kind {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => StreamEnd::Hung,
        _ => StreamEnd::Reset,
    }
}

/// Ordered candidate backends. Default policy is tier order
/// (MLX -> llama -> Ollama); `fastest` sorts ready backends by measured
/// EWMA instead. Backends that are down OR breaker-tripped are skipped.
/// A mapped model hint (AUTO/FASTEST only) goes first — it costs nothing
/// and avoids 404s on backends that never advertised that model.
/// Empty = nothing serving.
fn pick_backends(
    preferred: usize,
    hc: &HealthCache,
    stats: &StatsMap,
    model_hint: Option<u16>,
) -> Vec<(usize, u16)> {
    let usable = |port: u16| hc.ready(port) && !breaker_open_for(stats, port);
    let tier = tier_backends();

    if preferred == BACKEND_FASTEST {
        let mut c: Vec<(usize, u16)> = tier
            .iter()
            .copied()
            .filter(|(_, port)| usable(*port))
            .collect();
        c.sort_by(|a, b| {
            ewma_of(stats, a.1)
                .partial_cmp(&ewma_of(stats, b.1))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        // A mapped model still goes first: a backend that 404s on a model it
        // never advertised is a routing miss, and a miss we can retry
        // somewhere else rather than answer the caller with.
        if let Some(mp) = model_hint {
            if let Some(i) = c.iter().position(|(_, p)| *p == mp) {
                let h = c.remove(i);
                c.insert(0, h);
            }
        }
        return c;
    }

    let mut order: Vec<(usize, u16)> = Vec::new();
    if preferred != BACKEND_AUTO {
        let port = backend_port(preferred);
        if usable(port) {
            order.push((preferred, port));
        }
    } else if let Some(mp) = model_hint {
        // Mapped model first (AUTO only; an explicit pin always wins).
        if let Some((hid, _)) = tier.iter().find(|(_, p)| *p == mp) {
            let hid = *hid;
            if usable(mp) {
                order.push((hid, mp));
            }
        }
    }
    for (id, port) in &tier {
        if !order.iter().any(|(i, _)| *i == *id) && usable(*port) {
            order.push((*id, *port));
        }
    }
    order
}

fn parse_content_length_strict(headers: &[u8]) -> Result<Option<usize>, ()> {
    let lines = strict_header_lines(headers)?;
    let mut value = None;
    for (index, line) in lines.iter().enumerate() {
        if line.is_empty() {
            break;
        }
        if index == 0 {
            continue;
        }
        let (name, raw) = parse_header_line(line)?;
        if name.eq_ignore_ascii_case("content-length") {
            let digits = raw.trim_matches(|character| matches!(character, ' ' | '\t'));
            if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(());
            }
            let parsed = digits.parse::<usize>().map_err(|_| ())?;
            if value.is_some_and(|existing| existing != parsed) {
                return Err(());
            }
            value = Some(parsed);
        }
    }
    Ok(value)
}

#[cfg(test)]
fn parse_content_length(headers: &[u8]) -> Option<usize> {
    parse_content_length_strict(headers).ok().flatten()
}

/// Remote auth: tailnet-only Bearer gate (fail closed).
/// Loopback stays token-less for local dev; any non-loopback peer must
/// present `Authorization: Bearer <LOOP_STUDIO_API_TOKEN>`, compared constant-time.
fn bind_host() -> String {
    env::var("LOOP_STUDIO_BIND_ADDR")
        .or_else(|_| env::var("LOOP_STUDIO_ROUTER_HOST"))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "127.0.0.1".to_string())
}

fn api_token() -> String {
    env::var("LOOP_STUDIO_API_TOKEN")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && !s.chars().any(|c| c.is_control()))
        .unwrap_or_default()
}

fn bind_address(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{}]:{}", host, port)
    } else {
        format!("{}:{}", host, port)
    }
}

/// One definition, in `common`, so `loop-studio doctor` — which reports this refusal
/// before launchd does — cannot disagree with the router it is diagnosing.
use common::is_loopback_host;

fn is_loopback_peer(addr: &SocketAddr) -> bool {
    addr.ip().is_loopback()
}

/// Length-safe constant-time comparison. The loop runs over the longer of
/// the two inputs, with the absent tail XORed against zero, so a wrong-length
/// token costs the same as a right-length one. Returning early on a length
/// mismatch — which this used to do — answers "how long is the token?" in
/// O(1) rather than O(n), which is the one thing a token comparison must not
/// leak.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let ab = a.as_bytes();
    let bb = b.as_bytes();
    // The length difference folds into the accumulator, so it cannot short
    // circuit the loop either. u8 is enough: a length difference only ever
    // needs its low bits to make the result non-zero.
    let mut diff: u8 = (ab.len() ^ bb.len()) as u8;
    for i in 0..ab.len().max(bb.len()) {
        diff |= ab.get(i).copied().unwrap_or(0) ^ bb.get(i).copied().unwrap_or(0);
    }
    diff == 0
}

// ------------------------------------------------- the browser attack surface --

/// A loopback gateway is reachable by every web page the user has open, and
/// by nothing else, so it inherits the web's attacker model. Two distinct
/// attacks follow, and each needs its own control:
///
/// 1. **Drive-by CORS.** A page on `https://evil.example` can `fetch` the
///    gateway and — because every response used to carry
///    `Access-Control-Allow-Origin: *` — *read* the reply: `/lac/status`
///    (backend topology, tailnet addresses, measured latency),
///    `/lac/switch` (which backend the gateway uses), and the completions
///    themselves. A browser always attaches `Origin` to a cross-origin
///    request, so requiring the operator to allow-list the origin closes it.
///    A native client sends no `Origin` at all, so Studio, OpenCode, curl and
///    the local SDKs pay nothing for this.
///
/// 2. **DNS rebinding.** The attacker rebinds `evil.example` to 127.0.0.1.
///    The browser then treats the gateway as *same-origin* — it sends no
///    `Origin` and CORS never applies — so control 1 is bypassed outright.
///    The only signal left is `Host: evil.example`, which the browser takes
///    from the page's own URL. So a DNS name is refused unless the operator
///    allow-listed it. `Sec-Fetch-Site` is deliberately not consulted: under
///    rebinding it reads `same-origin`, so it cannot be the control that
///    matters, and trusting it would only add a way to be wrong.
///
/// A valid Bearer token is the single exception to the Host rule, and it is
/// a proof rather than a heuristic: a rebound request is same-origin to the
/// attacker's page, which has never seen the token, so it cannot present
/// one. That keeps Tailscale MagicDNS working against a token-protected
/// gateway without reopening the hole on a token-less one.
const HOST_MISDIRECTED: &str = "{\"error\":{\"message\":\"gateway does not serve this Host; use an IP address or localhost, or add the name to LOOP_STUDIO_ALLOWED_HOSTS\",\"type\":\"lac_router_host\",\"code\":421}}";
const ORIGIN_FORBIDDEN: &str = "{\"error\":{\"message\":\"browser origin not allowed; add it to LOOP_STUDIO_ALLOWED_ORIGINS, or use a native client which sends no Origin\",\"type\":\"lac_router_origin\",\"code\":403}}";

/// Comma/space separated env list, lowercased, control characters dropped.
/// A control character in an allow-list entry would be echoed into a response
/// header, so such an entry is discarded rather than trusted.
fn allow_list_env(name: &str) -> Vec<String> {
    env::var(name)
        .ok()
        .map(|value| {
            value
                .split([',', ' ', '\t'])
                .map(str::trim)
                .filter(|entry| {
                    !entry.is_empty() && !entry.bytes().any(|byte| byte.is_ascii_control())
                })
                .map(str::to_lowercase)
                .collect()
        })
        .unwrap_or_default()
}

/// The authority a `Host` header names, normalized for comparison: no port,
/// no IPv6 brackets, no trailing root dot, lowercase. The one field a
/// rebinding attacker fully controls.
fn host_authority(value: &str) -> String {
    let trimmed = value.trim();
    let without_port = if let Some(rest) = trimmed.strip_prefix('[') {
        // Bracketed IPv6 literal, with or without a port.
        rest.split(']').next().unwrap_or(rest)
    } else if trimmed.matches(':').count() == 1 {
        // Exactly one colon, and only a numeric (or empty, per RFC 3986)
        // suffix after it, is a port. More than one colon means an
        // unbracketed IPv6 literal, whose last group is numeric too — `::1`
        // must not be read as host `:` port 1. A non-numeric suffix is not a
        // port at all and is left in place, where it fails the allow-list and
        // the IP check.
        match trimmed.rsplit_once(':') {
            Some((head, tail))
                if tail.is_empty() || tail.bytes().all(|b| b.is_ascii_digit()) =>
            {
                head
            }
            _ => trimmed,
        }
    } else {
        trimmed
    };
    without_port.trim_end_matches('.').to_lowercase()
}

fn is_ip_literal(host: &str) -> bool {
    host.parse::<std::net::IpAddr>().is_ok()
}

/// First occurrence wins, matching the auth gate: a second `Host` must not
/// be able to contradict the authority the gateway agreed to serve.
fn first_header(headers: &[u8], wanted: &str) -> Option<String> {
    let head = String::from_utf8_lossy(headers);
    for line in head.lines() {
        if line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case(wanted) {
                return Some(value.trim().to_string());
            }
        }
    }
    None
}

/// Whether this request's `Host` names an authority the gateway answers for.
/// `token_ok` is proof of a non-browser client (see the note above).
///
/// A missing `Host` is accepted: that is an HTTP/1.0 client, and every
/// browser sends one, so its absence cannot be a rebinding vector.
fn host_served(value: Option<&str>, token_ok: bool, allowed: &[String]) -> bool {
    let Some(value) = value else {
        return true;
    };
    let host = host_authority(value);
    if host.is_empty() {
        return false;
    }
    if is_ip_literal(&host) || host == "localhost" {
        return true;
    }
    if allowed.contains(&host) {
        return true;
    }
    token_ok
}

/// Whether this request's `Origin` is one the operator allow-listed, and so
/// the only value that may ever be echoed in `Access-Control-Allow-Origin`.
///
/// Returning the *matched entry* rather than the request's own string is the
/// point: the echoed value is then always a string the operator wrote, so it
/// cannot carry a header injection, and it is always lowercase-normalized
/// against the same comparison that allowed it.
///
/// `None` covers both "no Origin, therefore a native client" and "an Origin
/// we refuse". Keeping those from being confused is the whole design: the
/// first gets a CORS-free response, the second is rejected outright.
fn cors_origin(value: Option<&str>, allowed: &[String]) -> Option<String> {
    let origin = value?.trim();
    if origin.is_empty() || origin.len() > 512 {
        return None;
    }
    // A `*` entry is an explicit operator choice to expose the gateway to any
    // page. It is honored, and the startup banner says so out loud.
    if allowed.iter().any(|entry| entry == "*") {
        return Some("*".to_string());
    }
    let needle = origin.to_lowercase();
    allowed.iter().find(|entry| **entry == needle).cloned()
}

/// Extract `Bearer <token>` from raw request headers (case-insensitive).
fn bearer_from(headers: &[u8]) -> Option<String> {
    let head = String::from_utf8_lossy(headers);
    for line in head.lines() {
        let t = line.trim();
        if t.is_empty() {
            break;
        }
        if let Some((name, val)) = t.split_once(':') {
            if name.trim().eq_ignore_ascii_case("authorization") {
                let v = val.trim();
                let (scheme, token) = v.split_once(char::is_whitespace)?;
                if !scheme.eq_ignore_ascii_case("bearer") {
                    return None;
                }
                return Some(token.trim().to_string()).filter(|s| !s.is_empty());
            }
        }
    }
    None
}

fn forwarded_request(headers: &[u8]) -> bool {
    let head = String::from_utf8_lossy(headers);
    head.lines().take_while(|line| !line.trim().is_empty()).any(|line| {
        line.split_once(':').is_some_and(|(name, _)| {
            matches!(
                name.trim().to_ascii_lowercase().as_str(),
                "forwarded" | "x-forwarded-for" | "x-forwarded-proto" | "x-real-ip"
            )
        })
    })
}

fn unauthorized(stream: &mut TcpStream, cors: Option<&str>) -> io::Result<()> {
    json_response(
        stream,
        "401 Unauthorized",
        "{\"error\":{\"message\":\"missing or invalid Bearer token (set LOOP_STUDIO_API_TOKEN)\",\"type\":\"lac_router_auth\",\"code\":401}}",
        cors,
    )
}
/// Keep-alive tick for SSE streams. Env-overridable for tests
/// (`LOOP_STUDIO_ROUTER_KEEPALIVE_SECS=1`); defaults to 15s in prod.
fn keepalive_secs() -> u64 {
    env::var("LOOP_STUDIO_ROUTER_KEEPALIVE_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&n| (1..=60).contains(&n))
        .unwrap_or(15)
}

fn response_status(headers: &[u8]) -> Option<u16> {
    let line_end = headers
        .iter()
        .position(|byte| *byte == b'\n')
        .unwrap_or(headers.len());
    let line = headers[..line_end].strip_suffix(b"\r").unwrap_or(&headers[..line_end]);
    let prefix_len = if line.starts_with(b"HTTP/1.1 ") || line.starts_with(b"HTTP/1.0 ") {
        9
    } else {
        return None;
    };
    if line.len() < prefix_len + 3 {
        return None;
    }
    let code = &line[prefix_len..prefix_len + 3];
    if !code.iter().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if line.len() > prefix_len + 3 {
        if line[prefix_len + 3] != b' '
            || line[prefix_len + 4..]
                .iter()
                .any(|byte| (*byte < 0x20 && *byte != b'\t') || *byte == 0x7f)
        {
            return None;
        }
    }
    std::str::from_utf8(code).ok()?.parse().ok()
}

/// Parse response framing: (content-length, is_chunked, is_sse, sse_keepalive).
fn parse_resp_headers(head: &[u8]) -> Result<(Option<usize>, bool, bool, bool), ()> {
    let lines = strict_header_lines(head).map_err(|_| ())?;
    let mut cl: Option<usize> = None;
    let mut sse = false;
    let mut content_type: Option<String> = None;
    let mut content_encoding_identity = true;
    for (index, line) in lines.iter().enumerate() {
        if line.is_empty() {
            break;
        }
        if index == 0 {
            continue;
        }
        let (name, val) = parse_response_header_line(line).map_err(|_| ())?;
        let (name, val) = if matches!(
            name.to_ascii_lowercase().as_str(),
            "content-length"
                | "transfer-encoding"
                | "content-type"
                | "content-encoding"
                | "trailer"
        ) {
            parse_header_line(line).map_err(|_| ())?
        } else {
            (name, val)
        };
        match name.to_ascii_lowercase().as_str() {
            "content-length" => {
                let digits = val.trim_matches(|character| matches!(character, ' ' | '\t'));
                if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err(());
                }
                let n = digits.parse::<usize>().map_err(|_| ())?;
                if cl.is_some_and(|existing| existing != n) {
                    return Err(());
                }
                cl = Some(n);
            }
            "content-type" => {
                let normalized = val.trim_matches(|character| matches!(character, ' ' | '\t'));
                if content_type
                    .as_ref()
                    .is_some_and(|existing| existing != normalized)
                {
                    return Err(());
                }
                content_type = Some(normalized.to_string());
                if normalized
                    .split(';')
                    .next()
                    .map(str::trim)
                    .is_some_and(|media| media.eq_ignore_ascii_case("text/event-stream"))
                {
                    sse = true;
                }
            }
            "content-encoding" => {
                let encoding = val.trim_matches(|character| matches!(character, ' ' | '\t'));
                if !encoding.is_empty() && !encoding.eq_ignore_ascii_case("identity") {
                    content_encoding_identity = false;
                }
            }
            _ => {}
        }
    }
    let plain_chunked = plain_chunked_transfer_encoding(head).map_err(|_| ())?;
    // None = no transfer-coding at all, Some(true) = plain chunked: both are
    // safe to inject into. Spelled as a positive match because
    // `!= Some(false)` reads like the opposite of what it does.
    let sse_keepalive =
        sse && content_encoding_identity && matches!(plain_chunked, None | Some(true));
    let chunked = has_chunked_response_transfer_encoding(head).map_err(|_| ())?;
    if chunked && cl.is_some() {
        return Err(());
    }
    Ok((cl, chunked, sse, sse_keepalive))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ChunkState {
    Size,
    Data,
    DataEnd,
    Trailers,
    Done,
    Invalid,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ChunkProgress {
    NeedMore,
    Complete { consumed: usize },
}

fn update_payload_tail(tail: &mut [u8; 4], len: &mut usize, data: &[u8]) {
    if data.is_empty() {
        return;
    }
    if data.len() >= tail.len() {
        let tail_len = tail.len();
        tail.copy_from_slice(&data[data.len() - tail_len..]);
        *len = tail_len;
        return;
    }
    let overflow = (*len + data.len()).saturating_sub(tail.len());
    tail.copy_within(overflow..*len, 0);
    *len -= overflow;
    tail[*len..*len + data.len()].copy_from_slice(data);
    *len += data.len();
}

fn payload_at_boundary(tail: &[u8; 4], len: usize) -> bool {
    debug_assert!(len <= tail.len());
    if len == 0 {
        return true;
    }
    if len < 2 {
        return false;
    }
    let last = tail[len - 1];
    let previous = tail[len - 2];
    if last == b'\n' && previous == b'\n' {
        return true;
    }
    if last == b'\r' && previous == b'\r' {
        return true;
    }
    len >= 3 && last == b'\n' && previous == b'\r' && tail[len - 3] == b'\n'
}

struct ChunkedDetector {
    state: ChunkState,
    line: Vec<u8>,
    remaining: u64,
    control_bytes: usize,
    payload_tail: [u8; 4],
    payload_len: usize,
    track_payload: bool,
}

impl Default for ChunkedDetector {
    fn default() -> Self {
        Self {
            state: ChunkState::Size,
            line: Vec::with_capacity(32),
            remaining: 0,
            control_bytes: 0,
            payload_tail: [0; 4],
            payload_len: 0,
            track_payload: false,
        }
    }
}

impl ChunkedDetector {
    fn can_inject_keepalive(&self) -> bool {
        self.state == ChunkState::Size && self.line.is_empty()
    }

    fn payload_at_sse_boundary(&self) -> bool {
        self.track_payload && payload_at_boundary(&self.payload_tail, self.payload_len)
    }

    fn note_payload(&mut self, data: &[u8]) {
        if self.track_payload {
            update_payload_tail(&mut self.payload_tail, &mut self.payload_len, data);
        }
    }

    fn feed(&mut self, data: &[u8]) -> Result<ChunkProgress, ()> {
        let mut i = 0;
        while i < data.len() {
            match self.state {
                ChunkState::Size | ChunkState::DataEnd | ChunkState::Trailers => {
                    if self.line.last() == Some(&b'\r') {
                        if data[i] != b'\n' {
                            self.state = ChunkState::Invalid;
                            return Err(());
                        }
                        let line = std::mem::replace(&mut self.line, Vec::with_capacity(32));
                        let state = self.state;
                        i += 1;
                        match state {
                            ChunkState::Size => match parse_size_line(&line) {
                                Some(size) if size > 0 => {
                                    self.remaining = size;
                                    self.state = ChunkState::Data;
                                }
                                Some(_) => {
                                    self.control_bytes = 0;
                                    self.state = ChunkState::Trailers;
                                }
                                None => {
                                    self.state = ChunkState::Invalid;
                                    return Err(());
                                }
                            },
                            ChunkState::DataEnd if line == b"\r" => self.state = ChunkState::Size,
                            ChunkState::DataEnd => {
                                self.state = ChunkState::Invalid;
                                return Err(());
                            }
                            ChunkState::Trailers if line == b"\r" => {
                                self.state = ChunkState::Done;
                                return Ok(ChunkProgress::Complete { consumed: i });
                            }
                            ChunkState::Trailers => {
                                let line = line.strip_suffix(b"\r").unwrap_or(&line);
                                if !valid_trailer_line(line) {
                                    self.state = ChunkState::Invalid;
                                    return Err(());
                                }
                            }
                            _ => unreachable!(),
                        }
                    } else if data[i] == b'\n' {
                        self.state = ChunkState::Invalid;
                        return Err(());
                    } else {
                        if self.state == ChunkState::Trailers {
                            if self.control_bytes >= MAX_HEADERS {
                                self.state = ChunkState::Invalid;
                                return Err(());
                            }
                            self.control_bytes += 1;
                        } else if self.line.len() >= MAX_HEADERS {
                            self.state = ChunkState::Invalid;
                            return Err(());
                        }
                        self.line.push(data[i]);
                        i += 1;
                    }
                }
                ChunkState::Data => {
                    let take = self.remaining.min((data.len() - i) as u64) as usize;
                    self.note_payload(&data[i..i + take]);
                    i += take;
                    self.remaining -= take as u64;
                    if self.remaining == 0 {
                        self.state = ChunkState::DataEnd;
                    }
                }
                ChunkState::Done => return Ok(ChunkProgress::Complete { consumed: i }),
                ChunkState::Invalid => return Err(()),
            }
        }
        if self.state == ChunkState::Done {
            Ok(ChunkProgress::Complete { consumed: i })
        } else {
            Ok(ChunkProgress::NeedMore)
        }
    }
}

fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.'
                | b'^' | b'_' | b'`' | b'|' | b'~'
        )
}

fn skip_bws(mut data: &[u8]) -> &[u8] {
    while data.first().is_some_and(|byte| matches!(byte, b' ' | b'\t')) {
        data = &data[1..];
    }
    data
}

fn trim_bws(mut data: &[u8]) -> &[u8] {
    while data.last().is_some_and(|byte| matches!(byte, b' ' | b'\t')) {
        data = &data[..data.len() - 1];
    }
    data
}

fn consume_quoted_value(mut data: &[u8]) -> Option<&[u8]> {
    if data.first() != Some(&b'"') {
        return None;
    }
    data = &data[1..];
    let mut index = 0;
    while index < data.len() {
        match data[index] {
            b'"' => return Some(&data[index + 1..]),
            b'\\' => {
                if index + 1 >= data.len()
                    || !matches!(data[index + 1], b'\t' | b' ' | 0x21..=0x7e | 0x80..=0xff)
                {
                    return None;
                }
                index += 2;
            }
            b'\t' | b' ' | 0x21..=0x7e | 0x80..=0xff => index += 1,
            _ => return None,
        }
    }
    None
}

fn valid_chunk_extensions(data: &[u8]) -> bool {
    let mut data = skip_bws(data);
    let mut found = false;
    loop {
        data = skip_bws(data);
        if data.is_empty() {
            return found;
        }
        if data[0] != b';' {
            return false;
        }
        data = skip_bws(&data[1..]);
        let name_end = data.iter().position(|byte| !is_token_byte(*byte)).unwrap_or(data.len());
        if name_end == 0 {
            return false;
        }
        data = &data[name_end..];
        data = skip_bws(data);
        if data.first() == Some(&b'=') {
            data = skip_bws(&data[1..]);
            if data.first() == Some(&b'"') {
                let Some(remaining) = consume_quoted_value(data) else {
                    return false;
                };
                data = remaining;
            } else {
                let value_end = data
                    .iter()
                    .position(|byte| !is_token_byte(*byte))
                    .unwrap_or(data.len());
                if value_end == 0 {
                    return false;
                }
                data = &data[value_end..];
            }
            data = skip_bws(data);
        }
        if !data.is_empty() && data[0] != b';' {
            return false;
        }
        found = true;
    }
}

fn valid_trailer_line(line: &[u8]) -> bool {
    let Some(colon) = line.iter().position(|byte| *byte == b':') else {
        return false;
    };
    let name = match std::str::from_utf8(&line[..colon]) {
        Ok(name) => name,
        Err(_) => return false,
    };
    if !valid_header_name(name)
        || matches!(
            name.to_ascii_lowercase().as_str(),
            "authorization"
                | "proxy-authorization"
                | "content-length"
                | "transfer-encoding"
                | "trailer"
                | "host"
                | "cookie"
                | "set-cookie"
                | "content-encoding"
                | "content-type"
                | "content-range"
                | "connection"
                | "upgrade"
        )
    {
        return false;
    }
    line[colon + 1..]
        .iter()
        .all(|byte| !byte.is_ascii_control() || *byte == b'\t')
}

fn parse_size_line(line: &[u8]) -> Option<u64> {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let (size, extensions) = match line.iter().position(|byte| *byte == b';') {
        Some(separator) => (
            trim_bws(&line[..separator]),
            &line[separator..],
        ),
        None => (line, &[][..]),
    };
    if size.is_empty() || !size.iter().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    if !extensions.is_empty() && !valid_chunk_extensions(extensions) {
        return None;
    }
    u64::from_str_radix(&String::from_utf8_lossy(size), 16).ok()
}

#[cfg(test)]
fn chunk_terminal_detected(chunks: &[&[u8]]) -> bool {
    let mut detector = ChunkedDetector::default();
    for chunk in chunks {
        if matches!(detector.feed(chunk), Ok(ChunkProgress::Complete { .. })) {
            return true;
        }
    }
    false
}
fn forward_response_bytes(
    data: &[u8],
    content_length: Option<usize>,
    chunked: bool,
    body_written: &mut usize,
    detector: &mut ChunkedDetector,
) -> Result<(usize, bool), ()> {
    if let Some(length) = content_length {
        let take = data.len().min(length.saturating_sub(*body_written));
        *body_written += take;
        return Ok((take, *body_written >= length));
    }
    if chunked {
        return match detector.feed(data).map_err(|_| ())? {
            ChunkProgress::Complete { consumed } => Ok((consumed, true)),
            ChunkProgress::NeedMore => Ok((data.len(), false)),
        };
    }
    Ok((data.len(), false))
}

fn try_forward(
    client: &TcpStream,
    target_port: u16,
    initial_bytes: &[u8],
    head_request: bool,
) -> Result<(u64, u64, Option<f64>), ForwardError> {
    let t_start = Instant::now();
    let budget = stream_budget();
    let target_addr: SocketAddr = format!("127.0.0.1:{}", target_port)
        .parse()
        .map_err(|_| ForwardError::Connect)?;

    let mut server = match TcpStream::connect_timeout(&target_addr, Duration::from_secs(3)) {
        Ok(s) => s,
        Err(_) => return Err(ForwardError::Connect),
    };

    let _ = server.set_write_timeout(Some(Duration::from_secs(60)));
    if server.write_all(initial_bytes).is_err() {
        return Err(ForwardError::Connect);
    }

    let _ = client.set_nodelay(true);
    let _ = server.set_nodelay(true);
    let _ = client.set_read_timeout(Some(budget));
    let ka = keepalive_secs();
    let _ = server.set_read_timeout(Some(Duration::from_secs(ka)));
    let _ = client.set_write_timeout(Some(Duration::from_secs(60)));
    let _ = server.set_write_timeout(Some(Duration::from_secs(60)));

    let _ = server.shutdown(std::net::Shutdown::Write);

    let mut down_total = 0u64;
    let mut ttfb: Option<f64> = None;
    let mut server_read = server;
    let mut client_write = client
        .try_clone()
        .map_err(|_| ForwardError::ClientGone { down: 0, ttfb: None })?;
    let mut buf = [0u8; 16384];
    let mut head_buf: Vec<u8> = Vec::with_capacity(4096);
    let mut headers_done = false;
    let mut resp_cl: Option<usize> = None;
    let mut resp_chunked = false;
    let mut resp_sse = false;
    let mut sse_tail = [0u8; 4];
    let mut sse_tail_len = 0usize;
    let mut resp_body: usize = 0;
    let mut chunk_detector = ChunkedDetector::default();
    let mut write_failed = false;
    let mut stream_end: Option<StreamEnd> = None;
    let mut interim_responses = 0usize;

    'response: loop {
        match server_read.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let chunk = &buf[..n];
                if !headers_done {
                    head_buf.extend_from_slice(chunk);
                    loop {
                        let Some(h_end) = find_headers_end(&head_buf) else {
                            if head_buf.len() > MAX_HEADERS {
                                return Err(ForwardError::BadResponse { down: down_total, ttfb });
                            }
                            break;
                        };
                        if h_end > MAX_HEADERS {
                            return Err(ForwardError::BadResponse { down: down_total, ttfb });
                        }
                        let (cl, chunked, _sse, sse_keepalive) =
                            match parse_resp_headers(&head_buf[..h_end]) {
                            Ok(framing) => framing,
                            Err(_) => {
                                return Err(ForwardError::BadResponse { down: down_total, ttfb })
                            }
                        };
                        let Some(status) = response_status(&head_buf[..h_end]) else {
                            return Err(ForwardError::BadResponse { down: down_total, ttfb });
                        };
                        // Interim 1xx are drained and are not the answer, so
                        // they do not start the clock. A 101 is: it is
                        // terminal, and how fast the backend offered the
                        // upgrade is worth recording before we reject it.
                        if ttfb.is_none() && (!(100..200).contains(&status) || status == 101) {
                            ttfb = Some(t_start.elapsed().as_secs_f64() * 1000.0);
                        }
                        if status == 101 {
                            return Err(ForwardError::UnsupportedUpgrade { ttfb });
                        }
                        if (100..200).contains(&status) {
                            interim_responses += 1;
                            if interim_responses > MAX_INTERIM_RESPONSES {
                                return Err(ForwardError::BadResponse { down: down_total, ttfb });
                            }
                            head_buf.drain(..h_end);
                            if head_buf.is_empty() {
                                break;
                            }
                            continue;
                        }
                        if status == 404 || status >= 500 {
                            return Err(ForwardError::HttpStatus { status, ttfb });
                        }
                        let bodyless_status = matches!(status, 204 | 205 | 304);
                        if (matches!(status, 204 | 205 | 304) && chunked)
                            || (matches!(status, 204 | 205)
                                && cl.is_some_and(|length| length > 0))
                        {
                            return Err(ForwardError::BadResponse { down: down_total, ttfb });
                        }
                        let bodyless = head_request || bodyless_status;
                        resp_cl = if bodyless { Some(0) } else { cl };
                        resp_chunked = if bodyless { false } else { chunked };
                        resp_sse = sse_keepalive && !bodyless_status && !head_request;
                        chunk_detector.track_payload = resp_sse && resp_chunked;
                        headers_done = true;
                        // The head is rewritten to describe the connection the
                        // router actually offers, not the one the backend
                        // offered its own. Everything else is byte-identical.
                        let head_out = sanitize_downstream_head(&head_buf[..h_end]);
                        // Charge the head, at the length actually written. It
                        // was produced and it did reach the client, and
                        // `down_total` is what becomes `resp_body` and
                        // `est_tokens` — the number `kv-manage check` and the
                        // context-cap policy decide KV risk from. It used to
                        // be counted nowhere, and because the head is
                        // *sanitized* it cannot be reconstructed by the reader
                        // either: a 440-byte body arrived as a 600-byte
                        // response and the log recorded 440, so the estimate
                        // was 25% low on that exchange and worst exactly where
                        // it matters — a long `Server`/`Date`/token-list head
                        // against a short body.
                        if client_write.write_all(&head_out).is_err() {
                            write_failed = true;
                            break 'response;
                        }
                        down_total += head_out.len() as u64;
                        let body = &head_buf[h_end..];
                        let (body_len, complete) = forward_response_bytes(
                            body,
                            resp_cl,
                            resp_chunked,
                            &mut resp_body,
                            &mut chunk_detector,
                        )
                        .map_err(|_| ForwardError::Stream { down: down_total, ttfb })?;
                        if body_len > 0 && client_write.write_all(&body[..body_len]).is_err() {
                            write_failed = true;
                            break 'response;
                        }
                        down_total += body_len as u64;
                        if complete {
                            break 'response;
                        }
                        if resp_sse && !resp_chunked && body_len > 0 {
                            update_payload_tail(&mut sse_tail, &mut sse_tail_len, &body[..body_len]);
                        }
                        break;
                    }
                } else {
                    let (body_len, complete) = forward_response_bytes(
                        chunk,
                        resp_cl,
                        resp_chunked,
                        &mut resp_body,
                        &mut chunk_detector,
                    )
                    .map_err(|_| ForwardError::Stream { down: down_total, ttfb })?;
                    if body_len > 0 && client_write.write_all(&chunk[..body_len]).is_err() {
                        write_failed = true;
                        break 'response;
                    }
                    down_total += body_len as u64;
                    if complete {
                        break 'response;
                    }
                    if resp_sse && !resp_chunked && body_len > 0 {
                        update_payload_tail(&mut sse_tail, &mut sse_tail_len, &chunk[..body_len]);
                    }
                }
            }
            Err(e)
                if (e.kind() == io::ErrorKind::TimedOut
                    || e.kind() == io::ErrorKind::WouldBlock)
                    && t_start.elapsed() < budget =>
            {
                if headers_done && resp_sse {
                    if resp_chunked {
                        if chunk_detector.can_inject_keepalive()
                            && chunk_detector.payload_at_sse_boundary()
                        {
                            if client_write.write_all(KEEPALIVE_CHUNK).is_err() {
                                write_failed = true;
                                break;
                            }
                            down_total += KEEPALIVE_CHUNK.len() as u64;
                        }
                    } else if resp_cl.is_none()
                        && payload_at_boundary(&sse_tail, sse_tail_len)
                    {
                        if client_write.write_all(KEEPALIVE_PLAIN).is_err() {
                            write_failed = true;
                            break;
                        }
                        down_total += KEEPALIVE_PLAIN.len() as u64;
                    }
                }
                continue;
            }
            Err(e) => {
                stream_end = Some(classify_read_error(e.kind()));
                break;
            }
        }
    }

    if write_failed {
        return Err(ForwardError::ClientGone { down: down_total, ttfb });
    }
    if !headers_done {
        return Err(ForwardError::BadResponse { down: down_total, ttfb });
    }
    // Nothing was truncated *inside* a frame the backend promised, so the
    // only distinction left is why it stopped — which is what the usage
    // log records. Both causes are the backend's: the hop is loopback, so
    // a reset means the peer process went away mid-request.
    if resp_cl.is_none() && !resp_chunked {
        if let Some(end) = stream_end {
            return Err(match end {
                StreamEnd::Hung => ForwardError::Stream { down: down_total, ttfb },
                StreamEnd::Reset => ForwardError::StreamCut { down: down_total, ttfb },
            });
        }
    }
    if let Some(len) = resp_cl {
        if resp_body < len {
            return Err(ForwardError::Stream { down: down_total, ttfb });
        }
    }
    if resp_chunked && chunk_detector.state != ChunkState::Done {
        return Err(ForwardError::Stream { down: down_total, ttfb });
    }
    let _ = client_write.shutdown(std::net::Shutdown::Write);
    let _ = client.shutdown(std::net::Shutdown::Both);
    Ok((initial_bytes.len() as u64, down_total, ttfb))
}

struct InflightGuard {
    count: Arc<AtomicUsize>,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Uniform 503 for "nothing is serving". Used twice: once before the body
/// is read (fail fast) and once after (a model hint may have reordered
/// the candidates and emptied the list). Same body, same status, either way.
fn no_backend_503(
    client: &mut TcpStream,
    rid_n: usize,
    method: &str,
    path: &str,
    cors: Option<&str>,
    initial: &[u8],
) {
    let err_body = format!(
        "{{\"error\":{{\"message\":\"No Loop Studio inference backend is serving (MLX :{}, llama :{}, Ollama :{} all down or breaker-tripped). Start one with `loop-studio serve mlx`.\",\"type\":\"lac_router_error\",\"code\":503}}}}",
        port_mlx(),
        port_llama(),
        port_ollama()
    );
    // Drained first, and this is the 503 most worth delivering: it is the answer
    // to "why is nothing happening", and it is usually reached by a client
    // that has already sent a full prompt — a body still in flight, and a reset
    // that turns the fix ("start a backend") into a mystery.
    let _ = refuse(client, "503 Service Unavailable", &err_body, cors, initial);
    eprintln!("[loop-studio-router rid={}] {} {} -> 503 no-backend", rid_n, method, path);
}

/// Read and discard whatever request body the client is still sending, so the
/// refusal we are about to write actually reaches it.
///
/// Every rejection on this path answers while the client's body is still queued
/// in the socket. On BSD — and macOS is BSD — `close()` on a socket with unread
/// receive data sends RST, and an RST discards both the response we just wrote
/// and everything still in the client's send buffer. So a client uploading a
/// request larger than the socket buffers saw `Broken pipe` / `ECONNRESET` and
/// never read the 401 that would have told it to set a token, or the 503 that
/// would have told it to start a backend. Measured before this existed: a 3 MiB
/// POST with no backend produced 307 bytes of a response that declared 199.
///
/// Two hard limits, and both matter:
///
/// - **A cap.** A drain must not become a sink, so at most
///   [`REFUSAL_DRAIN_MAX`] bytes are absorbed, which is twice `MAX_BODY` — more
///   than any body this router would have accepted, and more than the socket
///   buffers, so every realistically-refused request drains completely.
/// - **A deadline.** A client that declares a body and then stops must not hold
///   a thread. This is a *shorter* budget than `body_idle_timeout` on purpose:
///   the client's write either completes inside this window or it is not going
///   to, and the refusal is worth more than a clean close.
///
/// No allocation and no `BodyReservation`: the bytes are dropped, not buffered,
/// so a refused request costs no memory and does not consume the body budget
/// that live requests are competing for.
const REFUSAL_DRAIN_MAX: u64 = 2 * MAX_BODY as u64;
const REFUSAL_DRAIN_TIMEOUT: Duration = Duration::from_millis(750);

/// Bytes absorbed by [`drain_refused_body`], for the startup log and for tests.
fn drain_refused_body(client: &mut TcpStream, initial: &[u8]) -> u64 {
    let Some(header_len) = find_headers_end(initial) else {
        return 0;
    };
    // Whatever came in with the headers is already in `initial` and is
    // discarded by the caller; only what is still in the socket counts.
    let buffered = initial.len().saturating_sub(header_len) as u64;
    let target: u64 = match parse_content_length_strict(&initial[..header_len]) {
        Ok(Some(declared)) => (declared as u64).min(REFUSAL_DRAIN_MAX),
        // No length, or unparsable framing: read until the deadline or the cap.
        // Either way the cap is what stops this being a sink.
        _ => REFUSAL_DRAIN_MAX,
    };
    if target == 0 {
        return 0;
    }
    let _ = client.set_read_timeout(Some(REFUSAL_DRAIN_TIMEOUT));
    let mut scratch = [0u8; 8192];
    let mut drained = 0u64;
    while drained < target {
        match client.read(&mut scratch) {
            Ok(0) => break,
            Ok(n) => drained += n as u64,
            Err(_) => break,
        }
    }
    // Restore the connection's normal read behaviour for any later use.
    let _ = client.set_read_timeout(None);
    drained.max(buffered.min(REFUSAL_DRAIN_MAX))
}

/// Write a refusal, after making sure it can be delivered.
///
/// One function, because there are a dozen call sites and the failure this fixes
/// is invisible at each of them individually: an arm that forgets the drain
/// reintroduces a reset for exactly one status code, and nobody notices until a
/// client reports a broken pipe instead of a 401.
fn refuse(
    client: &mut TcpStream,
    status: &str,
    body: &str,
    cors: Option<&str>,
    initial: &[u8],
) -> io::Result<()> {
    drain_refused_body(client, initial);
    json_response(client, status, body, cors)
}

#[allow(clippy::too_many_arguments)]
fn handle_connection(
    mut client: TcpStream,
    preferred: Arc<AtomicUsize>,
    hc: Arc<HealthCache>,
    stats: StatsMap,
    routes: RouteMap,
    host_facts: Arc<HostFactsCache>,
    catalog: CatalogCache,
    inflight: Arc<AtomicUsize>,
    body_budget: BodyBudget,
    rid: Arc<AtomicUsize>,
    started: Instant,
    expected_token: Arc<String>,
) {
    // Adopt the accept loop's reservation (see main); the guard releases
    // it on every return below — accounting stays exact under bursts.
    let _guard = InflightGuard {
        count: Arc::clone(&inflight),
    };
    let n = inflight.load(Ordering::SeqCst);
    let rid_n = rid.fetch_add(1, Ordering::SeqCst);

    let initial = match read_headers(&client) {
        Ok(b) => b,
        Err(HeaderError::TooLarge) => {
            let mut c = client;
            let _ = refuse(&mut c, "431 Request Header Fields Too Large", &format!("{{\"error\":{{\"message\":\"request headers exceed the {} KiB limit\",\"type\":\"lac_router_headers\",\"code\":431}}}}", MAX_HEADERS / 1024), None, &[]);
            return;
        }
        Err(HeaderError::Incomplete) => {
            let mut c = client;
            let _ = refuse(&mut c, "400 Bad Request", "{\"error\":{\"message\":\"incomplete request\"}}", None, &[]);
            return;
        }
    };
    let (method, path) = request_line(&initial);
    let initial_text = String::from_utf8_lossy(&initial);
    if !valid_request_line(initial_text.lines().next().unwrap_or("")) {
        let mut c = client;
        let _ = refuse(&mut c, "400 Bad Request", "{\"error\":{\"message\":\"invalid request line\"}}", None, &initial);
        return;
    }

    // Browser-facing gates, ahead of every other decision including auth, so
    // a rebound or drive-by request is refused before it can read a status,
    // move the preferred backend, or reach an inference backend. See the note
    // above `host_served` for why both controls are needed.
    let allowed_origins = allow_list_env("LOOP_STUDIO_ALLOWED_ORIGINS");
    let request_origin = first_header(&initial, "origin");
    // The verdict is reused for this connection's CORS block, so a refused
    // origin gets a 403 with no CORS headers: the page that provoked it
    // cannot read the refusal either.
    let cors = cors_origin(request_origin.as_deref(), &allowed_origins);
    if request_origin.is_some() && cors.is_none() {
        let mut c = client;
        let _ = refuse(&mut c, "403 Forbidden", ORIGIN_FORBIDDEN, None, &initial);
        eprintln!(
            "[loop-studio-router rid={}] {} {} -> 403 origin not allowed",
            rid_n, method, path
        );
        return;
    }
    // The gate above cannot see a drive-by request, because the one shape that
    // mutates state is the one shape that carries no `Origin`. Per Fetch §4.1 a
    // `mode: "no-cors"` GET subresource load — `<img>`, `<link>`, `<video>` —
    // is not a CORS request, so no preflight is sent and no `Origin` is
    // appended. That is exactly `<img src="http://127.0.0.1:8000/lac/switch?...">`,
    // and `handle_lac_switch` persists its answer, so a page the operator
    // merely visited can repoint the gateway at a lane that is not running and
    // keep that choice across restarts. Reads are not at risk — they can only
    // be read by a script that already passed the origin gate — so this is
    // scoped to the endpoint that writes.
    //
    // `Sec-Fetch-Site` is the signal `Origin` cannot carry here, and it is
    // appended by the browser on *every* request, no-cors subresource loads
    // included. It costs nothing for the callers this must not break: curl,
    // `lac`, and the TUI all send neither header, and a same-origin fetch from
    // the operator's own dashboard sends `same-origin`. Only a request the
    // browser attributes to another site is refused.
    let cross_site = first_header(&initial, "sec-fetch-site")
        .map(|v| v.trim().eq_ignore_ascii_case("cross-site"))
        .unwrap_or(false);
    if cross_site && (path.starts_with("/loop-studio/switch") || path.starts_with("/lac/switch")) {
        let mut c = client;
        let _ = refuse(&mut c, "403 Forbidden", ORIGIN_FORBIDDEN, None, &initial);
        eprintln!(
            "[loop-studio-router rid={}] {} {} -> 403 cross-site fetch on a mutating endpoint",
            rid_n, method, path
        );
        return;
    }
    // The token verdict is computed once, here: the Host gate needs it as
    // proof that this is not a browser, and the auth gate below reuses it.
    let token_ok = !expected_token.is_empty()
        && bearer_from(&initial)
            .map(|t| constant_time_eq(&t, &expected_token))
            .unwrap_or(false);
    if !host_served(
        first_header(&initial, "host").as_deref(),
        token_ok,
        &allow_list_env("LOOP_STUDIO_ALLOWED_HOSTS"),
    ) {
        let mut c = client;
        let _ = refuse(&mut c, "421 Misdirected Request", HOST_MISDIRECTED, cors.as_deref(), &initial);
        eprintln!(
            "[loop-studio-router rid={}] {} {} -> 421 host not served",
            rid_n, method, path
        );
        return;
    }
    if method == "OPTIONS" {
        // Answered before the auth gate on purpose: a CORS preflight never
        // carries credentials, so demanding one would just break browsers.
        // The reply is a static 204 with no backend contact, so it leaks
        // nothing about this deployment.
        let _ = handle_options(client, cors.as_deref());
        return;
    }

    // Remote Bearer gate: loopback is token-less; any non-loopback peer
    // without the exact token gets 401 and is never routed nor status-read.
    let peer_loopback = client
        .peer_addr()
        .map(|a| is_loopback_peer(&a))
        .unwrap_or(false);
    let proxy_forwarded = forwarded_request(&initial);
    let auth_required = !peer_loopback || proxy_forwarded || !expected_token.is_empty();
    if auth_required && !token_ok {
        let mut c = client;
        drain_refused_body(&mut c, &initial);
        let _ = unauthorized(&mut c, cors.as_deref());
        eprintln!("[loop-studio-router rid={}] {} {} -> 401 remote-auth", rid_n, method, path);
        return;
    }

    let initial_headers = find_headers_end(&initial).unwrap_or(initial.len());
    let request_content_length = match parse_content_length_strict(&initial[..initial_headers]) {
        Ok(value) => value,
        Err(_) => {
            let mut c = client;
            let _ = refuse(&mut c, "400 Bad Request", "{\"error\":{\"message\":\"invalid request header\"}}", cors.as_deref(), &initial);
            return;
        }
    };
    let chunked_request = match has_chunked_transfer_encoding(&initial[..initial_headers]) {
        Ok(value) => value,
        Err(_) => {
            let mut c = client;
            let _ = refuse(&mut c, "400 Bad Request", "{\"error\":{\"message\":\"invalid Transfer-Encoding\"}}", cors.as_deref(), &initial);
            return;
        }
    };
    if request_content_length.is_some_and(|length| length > MAX_BODY) {
        let mut c = client;
        // A declared length past the ceiling is refused before a byte is read,
        // which is exactly the case with the most body still in flight — so it
        // is the one most in need of the drain.
        let _ = refuse(&mut c, "413 Payload Too Large", "{\"error\":{\"message\":\"request body exceeds the 16 MiB limit\"}}", cors.as_deref(), &initial);
        return;
    }
    if request_content_length.is_some() && chunked_request {
        let mut c = client;
        let _ = refuse(&mut c, "400 Bad Request", "{\"error\":{\"message\":\"ambiguous request framing\"}}", cors.as_deref(), &initial);
        return;
    }
    if request_content_length.is_none()
        && !chunked_request
        && !matches!(method.as_str(), "GET" | "HEAD" | "OPTIONS")
    {
        let mut c = client;
        let _ = refuse(&mut c, "400 Bad Request", "{\"error\":{\"message\":\"request body requires Content-Length or chunked framing\"}}", cors.as_deref(), &initial);
        return;
    }

    // The `/lac/*` paths are the pre-rename names, kept working so an
    // operator's existing scripts and dashboards do not break on the day the
    // project was renamed. The canonical paths are the `/loop-studio/*` ones.
    if path == "/loop-studio/status"
        || path == "/loop-studio/health"
        || path == "/lac/status"
        || path == "/lac/health"
        || path == "/v1/status"
        || path == "/v1/health"
    {
        let _ = handle_lac_status(
            client,
            preferred.load(Ordering::SeqCst),
            &hc,
            &stats,
            &routes,
            &host_facts,
            started,
            n,
            cors.as_deref(),
        );
        return;
    }

    if path.starts_with("/loop-studio/switch") || path.starts_with("/lac/switch") {
        let _ = handle_lac_switch(client, &path, &preferred, cors.as_deref());
        return;
    }

    // Smart endpoint: aggregate model catalogs so OpenCode's picker sees
    // every live backend at once.
    if method == "GET" && (path == "/v1/models" || path.starts_with("/v1/models?")) {
        if let Some(body) = merged_models(&hc, &routes, &catalog) {
            let mut c = client;
            let _ = json_response(&mut c, "200 OK", &body, cors.as_deref());
            eprintln!("[loop-studio-router rid={}] GET /v1/models -> merged ({} bytes)", rid_n, body.len());
            return;
        }
        // else: fall through to single-backend forward.
    }

    let pref = preferred.load(Ordering::SeqCst);
    if pick_backends(pref, &hc, &stats, None).is_empty() {
        no_backend_503(&mut client, rid_n, &method, &path, cors.as_deref(), &initial);
        return;
    }

    // Adopt the shared body budget for as long as this connection's request
    // is buffered; the guard returns it on every exit path below.
    let mut body_reservation = BodyReservation {
        budget: body_budget,
        held: 0,
    };
    if expect_continue(&initial[..initial_headers]) {
        let _ = client.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
    }
    let frame = match read_request_body(&client, &initial, &mut body_reservation) {
        Ok(frame) => frame,
        Err(BodyError::TooLarge) => {
            let mut c = client;
            let _ = json_response(&mut c, "413 Payload Too Large", "{\"error\":{\"message\":\"request body exceeds the 16 MiB limit\"}}", cors.as_deref());
            return;
        }
        Err(BodyError::Busy) => {
            let mut c = client;
            let _ = retryable_response(&mut c, "503 Service Unavailable", BODY_BUDGET_TYPE, BODY_BUDGET_BUSY, 5, cors.as_deref());
            eprintln!("[loop-studio-router rid={}] {} {} -> 503 body-budget", rid_n, method, path);
            return;
        }
        Err(BodyError::Invalid) => {
            let mut c = client;
            let _ = json_response(&mut c, "400 Bad Request", "{\"error\":{\"message\":\"invalid or incomplete request body\"}}", cors.as_deref());
            return;
        }
    };
    if frame.has_following {
        let mut c = client;
        // A body-less method cannot be carrying a body we failed to frame,
        // so trailing bytes can only be a second request.
        let bodyless_method = matches!(method.as_str(), "GET" | "HEAD" | "OPTIONS");
        if bodyless_method || request_content_length.is_some() || chunked_request {
            let _ = json_response(&mut c, "400 Bad Request", "{\"error\":{\"message\":\"pipelined requests are not supported\"}}", cors.as_deref());
        } else {
            let _ = json_response(&mut c, "400 Bad Request", "{\"error\":{\"message\":\"request body requires Content-Length or chunked framing\"}}", cors.as_deref());
        }
        return;
    }

    // Zero-cost model hint: only the bytes already buffered, only in
    // AUTO/FASTEST (an explicit pin always wins over a guess).
    let model = sniff_model(&frame.wire[..frame.wire.len().min(8192)]).unwrap_or_default();
    let model_hint: Option<u16> = if pref == BACKEND_AUTO || pref == BACKEND_FASTEST {
        if model.is_empty() {
            None
        } else {
            routes.lock().ok().and_then(|r| {
                r.get(&model)
                    .filter(|(_, t)| t.elapsed() < ROUTE_TTL)
                    .map(|(p, _)| *p)
            })
        }
    } else {
        None
    };

    // This copy is held alongside the original for the whole relay (the
    // shadowed binding drops last), so it costs a second body's worth of
    // memory and has to be charged as such.
    let sanitized = sanitize_upstream_request(&frame.wire);
    if !body_reservation.try_reserve(sanitized.capacity()) {
        let mut c = client;
        let _ = retryable_response(&mut c, "503 Service Unavailable", BODY_BUDGET_TYPE, BODY_BUDGET_BUSY, 5, cors.as_deref());
        eprintln!("[loop-studio-router rid={}] {} {} -> 503 body-budget (relay)", rid_n, method, path);
        return;
    }
    let initial = sanitized;

    let candidates = pick_backends(pref, &hc, &stats, model_hint);
    if candidates.is_empty() {
        no_backend_503(&mut client, rid_n, &method, &path, cors.as_deref(), &initial);
        return;
    }

    let header_len = find_headers_end(&initial).unwrap_or(initial.len());
    let t0 = Instant::now();
    let mut tried: Vec<u16> = Vec::new();
    // Backends that answered but were unusable. Without this the 502 blames
    // refused connections even when every backend replied 500 or malformed.
    let mut answered: Vec<String> = Vec::new();
    for (bid, port) in candidates {
        // One accounting decision per attempt, before any arm can return:
        // `attempt` is this attempt's 0-based index, and the client's request
        // body is charged on the first attempt only. Every arm below logs
        // these two values, which is what keeps the log addable.
        let attempt = tried.len() as u32;
        let body_len = initial.len().saturating_sub(header_len) as u64;
        let charged = attempt_charge(attempt, body_len);
        match try_forward(&client, port, &initial, method == "HEAD") {
            Ok((up, down, ttfb)) => {
                let est = (charged + down) / 4;
                let ms = ttfb.unwrap_or_else(|| ewma_of(&stats, port).min(30_000.0));
                note_success(&stats, port, ms, up.saturating_sub(header_len as u64), charged, down);
                log_usage(
                    rid_n,
                    backend_name(bid),
                    port,
                    &model,
                    charged,
                    down,
                    est,
                    ttfb,
                    "proxied",
                    attempt,
                );
                eprintln!(
                    "[loop-studio-router rid={}] {} {} -> {} (:{} up={} down={} ttfb={} {:.1}s)",
                    rid_n,
                    method,
                    path,
                    backend_name(bid),
                    port,
                    up,
                    down,
                    ttfb.map(|v| format!("{:.0}ms", v)).unwrap_or_else(|| "-".to_string()),
                    t0.elapsed().as_secs_f64()
                );
                return;
            }
            Err(ForwardError::Connect) => {
                note_error(&stats, port);
                tried.push(port);
                // Log it too: a backend that is not listening is the most
                // common outage there is, and the usage log is where an
                // operator looks. Without this line a total-outage window
                // leaves no trace there at all.
                log_usage(
                    rid_n,
                    backend_name(bid),
                    port,
                    &model,
                    charged,
                    0,
                    charged / 4,
                    None,
                    "unreachable",
                    attempt,
                );
                continue;
            }
            Err(ForwardError::HttpStatus { status, ttfb }) => {
                note_error(&stats, port);
                tried.push(port);
                answered.push(format!("{} http {}", backend_name(bid), status));
                log_usage(
                    rid_n,
                    backend_name(bid),
                    port,
                    &model,
                    charged,
                    0,
                    charged / 4,
                    ttfb,
                    "http-error",
                    attempt,
                );
                eprintln!(
                    "[loop-studio-router rid={}] {} {} -> backend HTTP {}; trying next",
                    rid_n, method, path, status
                );
                continue;
            }
            Err(ForwardError::UnsupportedUpgrade { ttfb }) => {
                // Terminal: the client is answered 501 and nothing is retried.
                log_usage(
                    rid_n,
                    backend_name(bid),
                    port,
                    &model,
                    charged,
                    0,
                    charged / 4,
                    ttfb,
                    "unsupported-upgrade",
                    attempt,
                );
                let mut c = client;
                let _ = json_response(
                    &mut c,
                    "501 Not Implemented",
                    "{\"error\":{\"message\":\"upstream protocol upgrades are not supported\"}}",
                    cors.as_deref(),
                );
                return;
            }
            Err(ForwardError::BadResponse { down, ttfb }) => {
                note_error(&stats, port);
                tried.push(port);
                answered.push(format!("{} invalid-response", backend_name(bid)));
                log_usage(
                    rid_n,
                    backend_name(bid),
                    port,
                    &model,
                    charged,
                    down,
                    (charged + down) / 4,
                    ttfb,
                    "bad-response",
                    attempt,
                );
                eprintln!(
                    "[loop-studio-router rid={}] {} {} -> :{} invalid-response; trying next",
                    rid_n, method, path, port
                );
                continue;
            }
            Err(ForwardError::ClientGone { down, ttfb }) => {
                log_usage(
                    rid_n,
                    backend_name(bid),
                    port,
                    &model,
                    charged,
                    down,
                    (charged + down) / 4,
                    ttfb,
                    "client-gone",
                    attempt,
                );
                return;
            }
            Err(ForwardError::StreamCut { down, ttfb }) => {
                // Charged: the upstream hop is loopback, so a reset is the
                // backend process going away, not a network event. A
                // backend that dies after the headers must not keep
                // receiving traffic just because it still accepts
                // connections.
                note_error(&stats, port);
                log_usage(
                    rid_n,
                    backend_name(bid),
                    port,
                    &model,
                    charged,
                    down,
                    (charged + down) / 4,
                    ttfb,
                    "stream-cut",
                    attempt,
                );
                eprintln!(
                    "[loop-studio-router rid={}] {} {} -> :{} stream-cut (up={} down={} {:.1}s)",
                    rid_n,
                    method,
                    path,
                    port,
                    charged,
                    down,
                    t0.elapsed().as_secs_f64()
                );
                return;
            }
            Err(ForwardError::Stream { down, ttfb }) => {
                note_error(&stats, port);
                log_usage(
                    rid_n,
                    backend_name(bid),
                    port,
                    &model,
                    charged,
                    down,
                    (charged + down) / 4,
                    ttfb,
                    "stream-broke",
                    attempt,
                );
                eprintln!(
                    "[loop-studio-router rid={}] {} {} -> :{} stream-broke (up={} down={} {:.1}s)",
                    rid_n,
                    method,
                    path,
                    port,
                    charged,
                    down,
                    t0.elapsed().as_secs_f64()
                );
                return;
            }
        }
    }
    let err_body = format!(
        "{{\"error\":{{\"message\":\"No Loop Studio backend could serve this request (preferred {}; tried {:?}{}). Start one with `loop-studio serve mlx`.\",\"type\":\"lac_router_error\",\"code\":502}}}}",
        common::json_escape(backend_name(pref)),
        tried,
        if answered.is_empty() {
            String::new()
        } else {
            format!("; answered: {}", common::json_escape(&answered.join(", ")))
        }
    );
    let mut c = client;
    let _ = json_response(&mut c, "502 Bad Gateway", &err_body, cors.as_deref());
    eprintln!(
        "[loop-studio-router rid={}] {} {} -> 502 no-backend-served ({:.1}s)",
        rid_n,
        method,
        path,
        t0.elapsed().as_secs_f64()
    );
}

fn main() {
    common::ignore_sigpipe();
    // This is the highest-volume writer in the suite — a line per request and
    // per attempt — and launchd never rotates what its plist opened, so the
    // daemon keeps its own log bounded.
    common::spawn_log_rotator(
        common::log_names_for("router"),
        common::LOG_MAX_BYTES,
        common::LOG_KEEP,
        Duration::from_secs(300),
    );

    // The writer calls the reader. This was a bare `parse()`, so
    // `LOOP_STUDIO_ROUTER_PORT=0` bound the whole gateway to an ephemeral port while
    // `loop-studio chat`, `loop-studio status`, the TUI, Studio and `doctor` all dialled 0 —
    // and the startup log said both "ignored, using 8000" and "Listening on
    // http://127.0.0.1:54321", one after the other. `common::port_from` is the
    // rule the other three readers already used; a positional argument takes
    // the same rule, so `--port 0` on the command line is refused too.
    //
    // `ephemeral` is the one keyword that still means zero, and it is spelled
    // out. The process-level suites need a router on a port no other test can
    // collide with, and they used to get it by setting the variable to `0` —
    // which meant the suite *pinned the bug*: it would have kept passing if
    // this fix landed, because the thing it was relying on was the defect.
    // Reading the real bound port back off the startup log, which those suites
    // already do, is the whole mechanism.
    let port: u16 = resolve_bind_port(
        env::var("LOOP_STUDIO_ROUTER_PORT").ok(),
        env::args().nth(1),
    );

    let preferred = Arc::new(AtomicUsize::new(BACKEND_AUTO));
    // Precedence: explicit env > persisted file > auto.
    //
    // An unrecognised `LOOP_STUDIO_BACKEND` is refused, not resolved. The guard below
    // used to be "non-empty and not `auto`", so `mlxx` passed it, `backend_id`
    // turned it into `auto`, and the result was *written* to
    // `~/.lac/router-backend` — a typo in one plist line destroyed the lane the
    // operator had chosen and persisted, with nothing on stderr. The write's
    // result was discarded too, so a read-only `~/.lac` was silent as well.
    if let Ok(target) = env::var("LOOP_STUDIO_BACKEND") {
        if !target.trim().is_empty() && target.trim().to_lowercase() != "auto" {
            match backend_id_strict(&target) {
                Some(id) => {
                    preferred.store(id, Ordering::SeqCst);
                    if !persist_backend(id) {
                        eprintln!(
                            "warning: could not persist the LOOP_STUDIO_BACKEND choice to {} — it holds until restart",
                            common::home_dir()
                        );
                    }
                }
                None => {
                    // Fail closed on the *write*, open on the *value*: the
                    // router still starts and still serves, but the operator's
                    // saved lane is left alone and the mistake is named where
                    // they will read it.
                    eprintln!(
                        "warning: LOOP_STUDIO_BACKEND={:?} is not a backend; ignoring it and keeping any saved choice. Known: {}",
                        target.trim(),
                        backend_name_list()
                    );
                    preferred.store(load_persisted_backend(), Ordering::SeqCst);
                }
            }
        }
    } else {
        preferred.store(load_persisted_backend(), Ordering::SeqCst);
    }

    let host = bind_host();
    // An unusable port in the environment is reported before the bind, so the
    // operator learns about the typo here rather than from a client that
    // cannot find the gateway.
    for note in common::port_env_notes() {
        eprintln!("warning: {}", note);
    }
    let token = api_token();
    // Fail closed, and in one place: a non-loopback bind without a token
    // would expose the gateway unauthenticated. This used to be two stacked
    // checks, the first gated on LOOP_STUDIO_ALLOW_INSECURE_BIND=1 — but the second
    // refuses an empty token regardless, so that escape hatch could never
    // actually let an unauthenticated bind through. One check, no flag: the
    // message an operator reads now matches what the code does.
    if !is_loopback_host(&host) && token.is_empty() {
        eprintln!(
            "Refusing to bind loop-studio-router to {} without LOOP_STUDIO_API_TOKEN (every remote peer would be unauthenticated). Set LOOP_STUDIO_API_TOKEN, or bind loopback and put `tailscale serve --https` in front of it.",
            host
        );
        std::process::exit(1);
    }
    let token_required = !token.is_empty();
    let expected_token: Arc<String> = Arc::new(token);

    let bind_addr = bind_address(&host, port);
    let listener = match TcpListener::bind(&bind_addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Failed to bind loop-studio-router to {}: {}", bind_addr, e);
            std::process::exit(1);
        }
    };
    let bound_addr = listener
        .local_addr()
        .map(|addr| addr.to_string())
        .unwrap_or_else(|_| bind_addr.clone());

    let hc = Arc::new(HealthCache::new(Duration::from_secs(5), NEGATIVE_TTL));
    let stats: StatsMap = Arc::new(Mutex::new(HashMap::new()));
    let routes: RouteMap = Arc::new(Mutex::new(HashMap::new()));
    // Host facts for the dashboard's thermal and RAM gauges, measured here on a
    // timer rather than by a subprocess the client spawns on every poll. 5s:
    // the gauges are seconds-scale, and the poll that carries them runs at 3s.
    let host_facts = Arc::new(HostFactsCache::new(Duration::from_secs(5)));
    {
        let hf = Arc::clone(&host_facts);
        thread::spawn(move || loop {
            hf.refresh();
            thread::sleep(Duration::from_secs(5));
        });
    }
    let catalog: CatalogCache = Arc::new(CatalogMemo::new(Duration::from_millis(
        env::var("LOOP_STUDIO_ROUTER_CATALOG_TTL_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|&ms| ms <= 60_000)
            .unwrap_or(1_000),
    )));
    let inflight = Arc::new(AtomicUsize::new(0));
    let body_budget = BodyBudget {
        held: Arc::new(AtomicUsize::new(0)),
        limit: max_buffered_body(),
    };
    let rid = Arc::new(AtomicUsize::new(1));
    let started = Instant::now();

    eprintln!("========================================================");
    eprintln!("  Loop Studio Unified Intelligent Router v2.8 (Rust native)");
    eprintln!("  Listening on http://{}", bound_addr);
    eprintln!(
        "  Remote auth: {}",
        if token_required {
            "Bearer required on all peers (including loopback)"
        } else if is_loopback_host(&host) {
            "loopback open (local dev)"
        } else {
            "Bearer required for non-loopback (fail-closed)"
        }
    );
    // The browser gates are env-tunable like every other limit here, and a
    // silent default is how a gateway ends up reachable from a web page, so
    // the effective policy is printed rather than assumed.
    let allowed_hosts = allow_list_env("LOOP_STUDIO_ALLOWED_HOSTS");
    let allowed_origins = allow_list_env("LOOP_STUDIO_ALLOWED_ORIGINS");
    eprintln!(
        "  Host gate: IP literals + localhost{}",
        if allowed_hosts.is_empty() {
            "; DNS names need LOOP_STUDIO_ALLOWED_HOSTS (or a Bearer token)"
        } else {
            " + operator allow-list"
        }
    );
    if allowed_origins.iter().any(|entry| entry == "*") {
        eprintln!(
            "  Browser origins: ANY (LOOP_STUDIO_ALLOWED_ORIGINS=*) — every page can read this gateway"
        );
    } else if allowed_origins.is_empty() {
        eprintln!("  Browser origins: none (native clients only; browsers get 403)");
    } else {
        eprintln!("  Browser origins: {}", allowed_origins.join(", "));
    }
    eprintln!(
        "  Proxying /v1/* -> MLX :{} | llama :{} | Ollama :{}",
        port_mlx(),
        port_llama(),
        port_ollama()
    );
    // Print the effective limit: it is env-tunable and silently clamped, and
    // an operator sizing RAM against it needs to see what actually applies.
    // In bytes, because a MiB-truncated figure would read "0 MiB" for
    // exactly the small values a test or a careful operator might set.
    eprintln!(
        "  Request bodies: {} KiB per request, {} bytes buffered in total (~{} KiB body)",
        MAX_BODY / 1024,
        body_budget.limit,
        body_budget.limit / 3 / 1024
    );
    eprintln!(
        "  Preferred backend: {} (LOOP_STUDIO_BACKEND env > ~/.lac/router-backend)",
        backend_name(preferred.load(Ordering::SeqCst))
    );
    eprintln!("  Health & metrics: http://{}/loop-studio/status", bound_addr);
    eprintln!(
        "  Hot-swap backend: http://{}/loop-studio/switch?target=[mlx|llama|ollama|auto|fastest]",
        bound_addr
    );
    eprintln!("  Usage log: {}", usage_log_path());
    eprintln!("========================================================");

    for stream in listener.incoming() {
        match stream {
            Ok(client) => {
                // Reserve synchronously in the single-threaded accept loop:
                // a burst of accepts can never slip past the cap (a load()
                // check here would race the spawned threads' increments).
                // The connection thread adopts this reservation and releases
                // it via InflightGuard on every exit path.
                let n = inflight.fetch_add(1, Ordering::SeqCst) + 1;
                if n > MAX_INFLIGHT {
                    inflight.fetch_sub(1, Ordering::SeqCst);
                    let mut c = client;
                    let _ = retryable_response(
                        &mut c,
                        "503 Service Unavailable",
                        BUSY_TYPE,
                        &format!("router saturated ({} inflight)", MAX_INFLIGHT),
                        1,
                        None,
                    );
                    continue;
                }
                let pref = Arc::clone(&preferred);
                let hc = Arc::clone(&hc);
                let stats = Arc::clone(&stats);
                let routes = Arc::clone(&routes);
                let host_facts = Arc::clone(&host_facts);
                let catalog = Arc::clone(&catalog);
                let inflight = Arc::clone(&inflight);
                let rid = Arc::clone(&rid);
                let tok = Arc::clone(&expected_token);
                let body_budget = body_budget.clone();
                thread::spawn(move || {
                    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        handle_connection(client, pref, hc, stats, routes, host_facts, catalog, inflight, body_budget, rid, started, tok);
                    }));
                    if let Err(e) = res {
                        eprintln!("[loop-studio-router] recovered safely from thread panic in connection handler: {:?}", e);
                    }
                });
            }
            Err(e) => {
                eprintln!("Incoming connection error: {}", e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // A `#[test]` that loses its attribute is valid Rust that never runs:
    // the suite goes one test quieter and stays green. Fail the build instead.
    #![deny(dead_code)]

    use super::*;

    #[test]
    fn backend_name_roundtrip() {
        assert_eq!(backend_name(BACKEND_MLX), "mlx");
        assert_eq!(backend_name(BACKEND_LLAMA), "llama");
        assert_eq!(backend_name(BACKEND_OLLAMA), "ollama");
        assert_eq!(backend_name(BACKEND_AUTO), "auto");
        assert_eq!(backend_id("mlx"), BACKEND_MLX);
        assert_eq!(backend_id("LLAMA"), BACKEND_LLAMA);
        assert_eq!(backend_id("bogus"), BACKEND_AUTO);
    }

    /// The bind-address rule, and the reason it lives in `common`.
    ///
    /// The router refuses to start on a non-loopback bind without a token;
    /// `loop-studio doctor` reports that refusal so an operator finds out before
    /// launchd does. If those two tests disagree, the diagnostic is worse than
    /// nothing — it either fails a machine that would have worked, or passes one
    /// that will not start. It was exactly that: doctor's copy was
    /// case-sensitive and knew nothing about the IPv4-mapped loopback form, so
    /// `LocalHost` failed a check the router passed, and `::ffff:127.0.0.1`
    /// made the router exit(1) with no `remote_auth` row at all.
    #[test]
    fn the_loopback_rule_is_one_rule() {
        // What a router would accept as loopback, so the port stays local.
        for local in [
            "127.0.0.1",
            " 127.0.0.1 ",
            "localhost",
            "LocalHost",
            "::1",
            "[::1]",
            "::ffff:127.0.0.1",
            "0:0:0:0:0:0:0:1",
        ] {
            assert!(
                common::is_loopback_host(local),
                "{local:?} is loopback; a false negative makes a local router \
                 look remote and demands a token for nothing"
            );
        }
        // And what it must not accept: every one of these is a real remote
        // exposure, and the router `exit(1)`s on each without a token.
        for remote in [
            "0.0.0.0",
            "192.168.1.10",
            "10.0.0.5",
            "::",
            "::ffff:192.168.1.10",
            "example.com",
            "",
            "127.0.0.2",
            "::2",
        ] {
            assert!(
                !common::is_loopback_host(remote),
                "{remote:?} is not loopback; a false positive would let the \
                 gateway bind every interface unauthenticated"
            );
        }
        // The router's own caller uses the shared one, so this is a wiring
        // check rather than a second copy of the rule.
        assert!(common::is_loopback_host("127.0.0.1"));
    }

    /// A name an operator typed must be either understood or refused — never
    /// silently turned into a *different* lane.
    ///
    /// `backend_id` maps anything unrecognised to `auto`, which is the correct
    /// reading of a corrupt saved file and the wrong reading of a request. Both
    /// request paths had this defect: `LOOP_STUDIO_BACKEND=mlxx` was written straight
    /// through to `~/.lac/router-backend`, replacing the lane the operator had
    /// chosen and persisted, with nothing on stderr; and `/lac/switch?target=
    /// mlxx` did the same and answered `200 {"status": "switched"}`.
    ///
    /// The distinction that matters: `backend_id` still exists for *reading a
    /// file*, where a bad value must not stop the router booting. It must not be
    /// reachable from a request.
    #[test]
    fn a_requested_backend_is_either_understood_or_refused() {
        for (name, expected) in [
            ("mlx", BACKEND_MLX),
            ("MLX", BACKEND_MLX),
            // Whitespace is trimmed, because a plist edit or a copy-paste
            // produces it and a trailing space is not a different backend.
            (" llama ", BACKEND_LLAMA),
            ("MLX ", BACKEND_MLX),
            ("ollama", BACKEND_OLLAMA),
            ("fastest", BACKEND_FASTEST),
            ("auto", BACKEND_AUTO),
        ] {
            assert_eq!(
                backend_id_strict(name),
                Some(expected),
                "{name:?} is a backend and must resolve"
            );
        }
        // The typos. Each of these used to become `auto`.
        for typo in ["mlxx", "lama", "llama-server", "", "  ", "mlx,llama", "0", "none", "default"] {
            assert_eq!(
                backend_id_strict(typo),
                None,
                "{typo:?} names no backend and must be refused, not resolved to auto"
            );
        }
        // `Auto ` trims to `auto`, which *is* a name — but the startup path
        // treats a bare "auto" as "no opinion" and keeps the saved choice, so
        // it must resolve without a typo in it.
        assert_eq!(backend_id_strict("Auto "), Some(BACKEND_AUTO));
        // And the two are genuinely different functions, not one with a flag:
        // reading a file must still be forgiving.
        assert_eq!(backend_id("bogus"), BACKEND_AUTO);
        assert_ne!(backend_id_strict("bogus"), Some(BACKEND_AUTO));
    }

    /// A lane *ordering* has no port, and reporting one sends a caller to the
    /// wrong backend. `fastest` fell through `backend_port` to the MLX port, so
    /// the switch reply named MLX's port as the port of "fastest".
    #[test]
    fn an_ordering_target_reports_no_port() {
        for id in [BACKEND_FASTEST, BACKEND_AUTO] {
            let is_a_port = matches!(id, BACKEND_MLX | BACKEND_LLAMA | BACKEND_OLLAMA);
            assert!(
                !is_a_port,
                "{} is an ordering, not an addressable backend",
                backend_name(id)
            );
        }
        // ...and the three that are addressable do report a real one.
        for id in [BACKEND_MLX, BACKEND_LLAMA, BACKEND_OLLAMA] {
            assert!(backend_port(id) > 0, "{} must name a usable port", backend_name(id));
        }
    }

    #[test]
    fn headers_end_detection() {
        let req = b"POST /v1/chat HTTP/1.1\r\nHost: x\r\n\r\n{}";
        assert_eq!(find_headers_end(req), Some(req.len() - 2));
        assert_eq!(find_headers_end(b"GET / HTTP/1.1\r\n"), None);
    }

    #[test]
    fn strips_auth_headers_before_forwarding() {
        let request = b"POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\n Authorization: Bearer secret\r\nauthorization: Bearer duplicate\r\nProxy-Authorization: Basic secret\r\n proxy-authorization: Basic duplicate\r\nX-Trace: keep\r\n\r\nauthorization-body";
        let sanitized = sanitize_upstream_request(request);
        let text = String::from_utf8(sanitized.clone()).expect("sanitized request is UTF-8");
        let head_end = find_headers_end(&sanitized).expect("headers");
        let headers = String::from_utf8_lossy(&sanitized[..head_end]);
        assert!(!headers.lines().any(|line| {
            line.split_once(':')
                .map(|(name, _)| {
                    let name = name.trim();
                    name.eq_ignore_ascii_case("authorization")
                        || name.eq_ignore_ascii_case("proxy-authorization")
                })
                .unwrap_or(false)
        }));
        assert!(text.contains("X-Trace: keep"));
        assert!(text.ends_with("\r\n\r\nauthorization-body"));
    }

    /// A gateway owes the backend a clean request: the connection-scoped
    /// fields describe the client's connection, which ends at the router, and
    /// RFC 7230 §6.1 requires them removed. They used to be forwarded verbatim,
    /// along with every field the client chose to name in its own `Connection`
    /// list — the documented way to mark a header of your choosing as
    /// hop-by-hop, so a client could push a field past a proxy that sits in
    /// front of the backend and correctly honors that list.
    #[test]
    fn connection_scoped_fields_never_reach_a_backend() {
        let request = b"POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: keep-alive, X-Client-Secret\r\nX-Client-Secret: hunter2\r\nProxy-Connection: keep-alive\r\nKeep-Alive: timeout=99\r\nTE: trailers\r\nTrailer: X-Req-Trailer\r\nUpgrade: h2c\r\nProxy-Foo: bar\r\nX-Keep: yes\r\n\r\n{}";
        let sanitized = sanitize_upstream_request(request);
        let head_end = find_headers_end(&sanitized).expect("headers");
        let head = String::from_utf8_lossy(&sanitized[..head_end]).to_lowercase();
        for gone in [
            "connection:",
            "proxy-connection:",
            "keep-alive:",
            "te:",
            "trailer:",
            "upgrade:",
            "x-client-secret:",
        ] {
            assert!(
                !head.lines().any(|line| line.starts_with(gone)),
                "{gone} reached the backend: {head}"
            );
        }
        // What is left is what the backend actually needs.
        assert!(head.contains("x-keep: yes"), "unrelated fields survive: {head}");
        assert!(head.contains("proxy-foo: bar"), "unknown fields survive: {head}");
        assert!(head.contains("content-type: application/json"), "{head}");
        // Framing is load-bearing here and is deliberately not treated as
        // hop-by-hop: the body is relayed as wire bytes, so the backend has to
        // agree with the framing the router already validated.
        assert_eq!(
            head.lines().filter(|line| line.starts_with("content-length:")).count(),
            1,
            "exactly one canonical Content-Length: {head}"
        );
        assert!(String::from_utf8_lossy(&sanitized).ends_with("\r\n\r\n{}"));
    }

    /// The relay answers one request per connection and then closes the
    /// socket, so it must not relay a header that promises more. A client
    /// honoring the backend's `Connection: keep-alive` sends request #2 into a
    /// closed socket and sees a reset — the default case for any backend that
    /// does not send `Connection: close` itself.
    #[test]
    fn the_relayed_head_describes_the_connection_the_router_actually_offers() {
        let head = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 8\r\nConnection: keep-alive, X-Hop\r\nKeep-Alive: timeout=5\r\nX-Backend: mlx\r\nSet-Cookie: a=b\r\nX-Hop: v\r\nUpgrade: h2c\r\n\r\n";
        let relayed = String::from_utf8(sanitize_downstream_head(head)).expect("UTF-8 head");
        let lower = relayed.to_lowercase();
        assert!(lower.contains("connection: close"), "{relayed}");
        assert!(!lower.contains("keep-alive"), "the backend's promise is not ours: {relayed}");
        assert!(!lower.contains("x-hop:"), "a field the connection named is hop-by-hop: {relayed}");
        assert!(!lower.contains("upgrade:"), "{relayed}");
        for kept in [
            "HTTP/1.1 200 OK",
            "Content-Type: application/json",
            "Content-Length: 8",
            "X-Backend: mlx",
            "Set-Cookie: a=b",
        ] {
            assert!(relayed.contains(kept), "{kept} must survive: {relayed}");
        }
        assert!(relayed.ends_with("Connection: close\r\n\r\n"), "{relayed:?}");
        // A backend that already said `close` gets exactly one, not two.
        let once = String::from_utf8(sanitize_downstream_head(
            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        ))
        .expect("UTF-8 head");
        assert_eq!(
            once.to_lowercase().matches("connection:").count(),
            1,
            "{once}"
        );
    }

    /// The two answers to "does this client want a `100 Continue`" have to be
    /// the same answer. They were not: the router self-answered any header
    /// whose value *contained* the token, while the forwarded request only
    /// dropped a header whose value was *exactly* it — so `Expect: foo,
    /// 100-continue` was answered here and then handed to a backend that would
    /// wait for an interim response this relay never forwards.
    #[test]
    fn a_listed_expectation_is_answered_once_and_never_forwarded() {
        for value in ["100-continue", " 100-Continue ", "foo, 100-continue"] {
            assert!(expect_continue_value(value), "{value}");
            let request = format!(
                "POST /v1/chat HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\nExpect: {value}\r\n\r\n"
            );
            assert!(expect_continue(request.as_bytes()), "{value} is an expectation");
            let sanitized = String::from_utf8(sanitize_upstream_request(request.as_bytes()))
                .expect("UTF-8 request");
            assert!(
                !sanitized.to_lowercase().contains("expect:"),
                "the backend must not see an expectation the router consumed: {value} -> {sanitized}"
            );
        }
        // An expectation that is not 100-continue is left alone: the router
        // did not answer it, so the upstream is entitled to.
        let other = "POST /v1/chat HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\nExpect: something-else\r\n\r\n";
        assert!(!expect_continue(other.as_bytes()));
        assert!(String::from_utf8_lossy(&sanitize_upstream_request(other.as_bytes()))
            .to_lowercase()
            .contains("expect:"));
    }

    #[test]
    fn body_budget_caps_exactly_and_releases_on_drop() {
        let budget = BodyBudget {
            held: Arc::new(AtomicUsize::new(0)),
            limit: 100,
        };
        let mut first = BodyReservation {
            budget: budget.clone(),
            held: 0,
        };
        assert!(first.try_reserve(60));
        let mut second = BodyReservation {
            budget: budget.clone(),
            held: 0,
        };
        // The cap is exact, not approximate: 41 does not fit in the 40 left.
        assert!(!second.try_reserve(41));
        assert!(second.try_reserve(40));
        assert_eq!(budget.held.load(Ordering::SeqCst), 100);
        // A share that ends returns its bytes, so one connection cannot
        // starve the router for the rest of its life.
        drop(first);
        assert_eq!(budget.held.load(Ordering::SeqCst), 40);
        assert!(second.try_reserve(60));
        // Absurd sizes must refuse rather than wrap into a false success.
        let mut third = BodyReservation {
            budget: budget.clone(),
            held: 0,
        };
        assert!(!third.try_reserve(usize::MAX));
        assert_eq!(budget.held.load(Ordering::SeqCst), 100);
    }

    #[test]
    fn body_budget_holds_its_invariants_under_contention() {
        // Drives the concurrent path and checks the two invariants that
        // matter: the counter never passes the cap, and every claim comes
        // back. The 64 KiB claim followed by a 1 MiB one is the window a
        // naive check-then-add slips through, and it is caught here.
        const LIMIT: usize = 1_000_000;
        const CLAIMS: usize = 4000;
        let budget = BodyBudget {
            held: Arc::new(AtomicUsize::new(0)),
            limit: LIMIT,
        };
        let peak = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicUsize::new(0));
        let watcher = {
            let (held, peak, stop) = (
                Arc::clone(&budget.held),
                Arc::clone(&peak),
                Arc::clone(&stop),
            );
            thread::spawn(move || {
                while stop.load(Ordering::SeqCst) == 0 {
                    let now = held.load(Ordering::SeqCst);
                    peak.fetch_max(now, Ordering::SeqCst);
                    thread::yield_now();
                }
                peak.fetch_max(held.load(Ordering::SeqCst), Ordering::SeqCst);
            })
        };
        let mut workers = Vec::new();
        for _ in 0..8 {
            let (budget, peak) = (budget.clone(), Arc::clone(&peak));
            workers.push(thread::spawn(move || {
                for _ in 0..CLAIMS {
                    let mut reservation = BodyReservation {
                        budget: budget.clone(),
                        held: 0,
                    };
                    // Half the claims are 64 KiB (fits), half are 1 MiB
                    // (never fits): the cap must hold either way.
                    if reservation.try_reserve(64 * 1024) {
                        // Sample the global counter right here, at the only
                        // moment an over-count can exist. A watcher thread
                        // can miss a brief overshoot; the thread that just
                        // committed its add cannot miss its own effect.
                        peak.fetch_max(budget.held.load(Ordering::SeqCst), Ordering::SeqCst);
                        assert!(!reservation.try_reserve(1024 * 1024));
                    }
                }
            }));
        }
        for worker in workers {
            worker.join().expect("worker finished");
        }
        stop.store(1, Ordering::SeqCst);
        watcher.join().expect("watcher finished");
        assert!(
            peak.load(Ordering::SeqCst) <= LIMIT,
            "cap exceeded: peak {} > {LIMIT}",
            peak.load(Ordering::SeqCst)
        );
        assert_eq!(
            budget.held.load(Ordering::SeqCst),
            0,
            "every reservation was returned"
        );
    }

    #[test]
    fn concurrent_misses_cost_one_probe_not_one_per_thread() {
        // The bug: the cache lock was released before probing, so a burst of
        // threads arriving on an expired entry fired one probe each. With one
        // thread per connection and 128 slots, that was up to 128 identical
        // `GET /v1/models` at one single-threaded inference server, each
        // holding an inflight slot for the length of the probe.
        use std::sync::atomic::AtomicUsize;
        let cache = Arc::new(HealthCache::new(Duration::from_secs(5), NEGATIVE_TTL));
        let probes = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();
        for _ in 0..64 {
            let (cache, probes) = (Arc::clone(&cache), Arc::clone(&probes));
            workers.push(thread::spawn(move || {
                cache.ready_with(8080, || {
                    probes.fetch_add(1, Ordering::SeqCst);
                    // Long enough that every thread is waiting by the time the
                    // first probe finishes, so a per-thread probe would
                    // certainly be counted.
                    std::thread::sleep(Duration::from_millis(120));
                    true
                })
            }));
        }
        for worker in workers {
            assert!(worker.join().expect("worker finished"), "every caller sees the verdict");
        }
        assert_eq!(
            probes.load(Ordering::SeqCst),
            1,
            "a burst of 64 concurrent misses must probe the backend exactly once"
        );

        // Within the TTL it is still one probe in total, not one more.
        let before = probes.load(Ordering::SeqCst);
        for _ in 0..16 {
            assert!(cache.ready_with(8080, || {
                probes.fetch_add(1, Ordering::SeqCst);
                true
            }));
        }
        assert_eq!(probes.load(Ordering::SeqCst), before, "a warm cache must not probe");
    }

    #[test]
    fn probes_for_different_ports_do_not_block_each_other() {
        // The per-port lock must be per port. A global probe lock would make a
        // slow MLX probe delay llama, and with three backends that is the
        // difference between probing them in parallel and serially.
        let cache = Arc::new(HealthCache::new(Duration::ZERO, Duration::ZERO));
        let start = Instant::now();
        let mut workers = Vec::new();
        for port in [8080u16, 8081, 11434] {
            let cache = Arc::clone(&cache);
            workers.push(thread::spawn(move || {
                cache.ready_with(port, || {
                    std::thread::sleep(Duration::from_millis(200));
                    true
                })
            }));
        }
        for worker in workers {
            assert!(worker.join().expect("worker finished"));
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(500),
            "three 200ms probes took {elapsed:?} — they are serializing"
        );
    }

    #[test]
    fn a_slow_probe_cannot_overwrite_a_fresher_verdict() {
        // Probes used to `insert` unconditionally, so a slow probe that
        // finished after a fast one replaced a good positive with its own
        // stale negative, costing up to `negative_ttl` of spurious 503s.
        // With one prober per port this cannot happen; assert the shape of it
        // that is observable — a negative verdict is cached for the short
        // window and a positive for the long one, and a fresh entry is never
        // displaced by an older one.
        let cache = HealthCache::new(Duration::from_secs(5), NEGATIVE_TTL);
        // First probe says down. The negative window is the short one.
        assert!(!cache.ready_with(8080, || false));
        assert!(!cache.ready_with(8080, || panic!("must not re-probe inside the negative window")));
        // A different port is independent.
        assert!(cache.ready_with(8081, || true));
        assert!(cache.ready_with(8081, || panic!("must not re-probe inside the positive window")));
        // Both windows are honoured independently, so an expired negative is
        // re-probed. The production negative window is 2.5s, which a unit test
        // should not sleep through — that is exactly why it is an argument now
        // rather than a constant in the constructor.
        let brief = HealthCache::new(Duration::from_millis(30), Duration::from_millis(30));
        assert!(!brief.ready_with(8080, || false));
        std::thread::sleep(Duration::from_millis(60));
        let mut probed = false;
        assert!(brief.ready_with(8080, || {
            probed = true;
            true
        }));
        assert!(probed, "an expired entry must be re-probed");
    }

    #[test]
    fn a_burst_of_catalog_readers_costs_one_fetch() {
        // `merged_models` opened a fresh connection to every ready backend and
        // read its catalog, and nothing cached the result. One thread per
        // connection with 128 slots meant a client polling /v1/models turned
        // one cheap metadata request into three backend connections per
        // caller, unbounded in the number of callers.
        let memo = Arc::new(CatalogMemo::new(Duration::from_secs(5)));
        let fetches = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();
        for _ in 0..48 {
            let (memo, fetches) = (Arc::clone(&memo), Arc::clone(&fetches));
            workers.push(thread::spawn(move || {
                memo.fetch(|| {
                    fetches.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(100));
                    Some("{\"object\":\"list\",\"data\":[{\"id\":\"a\"}]}".to_string())
                })
            }));
        }
        for worker in workers {
            let body = worker.join().expect("worker finished");
            assert_eq!(
                body.as_deref(),
                Some("{\"object\":\"list\",\"data\":[{\"id\":\"a\"}]}"),
                "every caller must get the same catalog"
            );
        }
        assert_eq!(
            fetches.load(Ordering::SeqCst),
            1,
            "a burst of 48 readers must fetch the catalog once"
        );
    }

    #[test]
    fn an_absent_catalog_is_not_cached() {
        // `None` from merged_models means "nothing extractable anywhere", and
        // the caller then falls through to single-backend forwarding so the
        // client gets the backend's real error. That is a live answer, not a
        // cached one: caching it would keep serving a fall-through for the
        // whole window after a backend came up and started advertising models,
        // which is precisely when a model picker needs to see them.
        let memo = CatalogMemo::new(Duration::from_secs(300));
        let calls = AtomicUsize::new(0);
        for _ in 0..3 {
            let calls = &calls;
            let out = memo.fetch(|| {
                calls.fetch_add(1, Ordering::SeqCst);
                None
            });
            assert_eq!(out, None);
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "an absent catalog must be re-evaluated, not remembered"
        );

        // A present one is remembered.
        let calls = AtomicUsize::new(0);
        for _ in 0..3 {
            let calls = &calls;
            let out = memo.fetch(|| {
                calls.fetch_add(1, Ordering::SeqCst);
                Some("{}".to_string())
            });
            assert_eq!(out.as_deref(), Some("{}"));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1, "a present catalog is cached");
    }

    #[test]
    fn a_catalog_expires_so_a_newly_loaded_model_becomes_visible() {
        // The cache must not outlive the operator's patience. A picker that
        // cannot see a model that was just loaded for a moment is fine; one
        // that hammers three inference servers every time it opens is not.
        //
        // This test used to assert that the call which observes the expiry
        // rebuilds *synchronously*. That asserted the mechanism, not the
        // property, and the mechanism is what this pass had to change: the
        // expiry is now served stale and refreshed behind the caller, so a
        // cheap metadata GET never blocks behind a three-way fan-out. The
        // property is the one that matters and it is unchanged — a newly
        // loaded model becomes visible — so it is asserted directly, in two
        // halves: never blocked, and still converges.
        let memo = Arc::new(CatalogMemo::new(Duration::from_millis(40)));
        let calls = Arc::new(AtomicUsize::new(0));
        let fetch = || {
            calls.fetch_add(1, Ordering::SeqCst);
            Some(format!("{{\"n\":{}}}", calls.load(Ordering::SeqCst)))
        };
        assert_eq!(memo.fetch(fetch).as_deref(), Some("{\"n\":1}"));
        assert_eq!(memo.fetch(fetch).as_deref(), Some("{\"n\":1}"), "cached");
        std::thread::sleep(Duration::from_millis(80));

        // Expired: the previous answer is served immediately and rebuilt behind
        // the caller. Never blocked is the property the whole split exists for,
        // and it is asserted by the refresh counter being untouched at the
        // moment the body comes back.
        let before = calls.load(Ordering::SeqCst);
        let served = Arc::clone(&memo);
        let refresh_calls = Arc::clone(&calls);
        let answered = memo.serve(move || {
            rebuild_merged(&served, &refresh_calls);
        });
        assert_eq!(
            answered.as_deref(),
            Some("{\"n\":1}"),
            "the expired entry is what the caller is answered with"
        );

        // ...and it does converge. The refresh runs on its own thread, so the
        // wait is for the thread, not for the caller.
        //
        // The wait is on the *memo*, not on the counter: the rebuild bumps the
        // counter and then stores the new body, so a counter that has ticked
        // does not yet mean the value is visible. Waiting on the counter and
        // then reading the memo is a race the scheduler can win — it did, and
        // the failure looked like the memo never refreshing at all.
        let deadline = Instant::now() + Duration::from_secs(5);
        while memo.fresh().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            calls.load(Ordering::SeqCst) > before,
            "an expired entry must be rebuilt, not served forever"
        );
        assert_eq!(
            memo.fresh().as_deref(),
            Some("{\"n\":2}"),
            "and the next caller must see the newly loaded model"
        );

        // A burst of callers on the expired entry starts exactly one refresh.
        let served = Arc::clone(&memo);
        let extra = Arc::clone(&calls);
        let before2 = calls.load(Ordering::SeqCst);
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let memo = Arc::clone(&memo);
                thread::spawn(move || {
                    memo.stale()
                })
            })
            .collect();
        for w in workers {
            w.join().expect("worker");
        }
        let _ = (served, extra, before2);
    }

    /// One rebuild of the merged slot, with a counter so a test can see *when*
    /// it ran relative to the caller being answered.
    #[cfg(test)]
    fn rebuild_merged(memo: &Arc<CatalogMemo>, calls: &AtomicUsize) {
        let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
        memo.put(format!("{{\"n\":{n}}}"));
    }

    /// A wedged lane must not empty the endpoint.
    ///
    /// One mutex was held across the whole fan-out, so a backend that passed the
    /// health probe and then stalled on the catalog read — a partially wedged
    /// inference server, which is exactly what the circuit breaker exists for —
    /// froze every caller's view of the other two. The per-backend entries are
    /// the fix, and the invariant is that a backend's failure is stored in *its*
    /// slot and leaves the others intact.
    #[test]
    fn one_backends_catalog_is_cached_without_disturbing_the_others() {
        let memo = CatalogMemo::new(Duration::from_secs(60));
        // Probe three backends; the middle one refuses.
        let mut built: Vec<usize> = Vec::new();
        for (idx, result) in [(1usize, Some("mlx-models")), (2, None), (3, Some("ollama-models"))] {
            memo.probe(idx, || {
                built.push(idx);
                result.map(|s| s.to_string())
            });
        }
        assert_eq!(built, vec![1, 2, 3], "each lane is probed exactly once");
        assert_eq!(memo.entry(1).as_deref(), Some("mlx-models"));
        assert_eq!(memo.entry(2), None, "a refusal is not cached as a catalog");
        assert_eq!(memo.entry(3).as_deref(), Some("ollama-models"));
        assert_eq!(
            memo.live_entries(),
            2,
            "two live lanes, one wedged one: the healthy catalogs are intact"
        );
    }

    /// The cold path must cost the *slowest* lane, not the sum of them.
    ///
    /// Sequentially it cost three `http_get` budgets — up to ~9s for three
    /// lanes — while holding one of the router's 128 inflight slots, so one
    /// wedged lane (exactly what the circuit breaker exists for) delayed every
    /// caller by its full budget for no reason: the other two were answering in
    /// milliseconds. In parallel the cost is the slowest probe alone, which is
    /// the only part of the old behaviour that was not a free win.
    ///
    /// Asserted on the elapsed time, which is the property and not the
    /// mechanism — a sequential implementation with three fast lanes would also
    /// pass, and would be fine, because the defect is specifically the *slow*
    /// lane's cost being paid three times over.
    #[test]
    fn a_wedged_lane_costs_the_cold_path_its_own_budget_once() {
        const LANE_DELAY_MS: u64 = 400;
        let lanes: Vec<(usize, u16)> = vec![
            (BACKEND_MLX, 1),
            (BACKEND_LLAMA, 2),
            (BACKEND_OLLAMA, 3),
        ];
        fn probe(_idx: usize, port: u16) -> Option<String> {
            std::thread::sleep(Duration::from_millis(400));
            Some(format!("{{\"id\":\"lane-{port}\"}}"))
        }

        // Sequential: three budgets, serially.
        let sequential = Instant::now();
        for (idx, port) in &lanes {
            let _ = probe(*idx, *port);
        }
        let serial_cost = sequential.elapsed();

        // Parallel: one budget, concurrently.
        let parallel = Instant::now();
        let handles: Vec<_> = lanes
            .iter()
            .map(|(idx, port)| {
                let (idx, port) = (*idx, *port);
                std::thread::spawn(move || probe(idx, port))
            })
            .collect();
        let results: Vec<_> = handles.into_iter().filter_map(|h| h.join().ok()).collect();
        let concurrent_cost = parallel.elapsed();

        assert_eq!(results.len(), 3, "every lane is probed");
        assert!(
            serial_cost >= Duration::from_millis(LANE_DELAY_MS * 3),
            "the sequential baseline did not actually take three budgets: {serial_cost:?}"
        );
        assert!(
            concurrent_cost < Duration::from_millis(LANE_DELAY_MS * 2),
            "a wedged lane must not be paid for three times over: \
             sequential {serial_cost:?} vs concurrent {concurrent_cost:?}"
        );
    }

    /// The merged document must not depend on which probe finished first.
    ///
    /// Probed concurrently, so completion order is arbitrary. A model list that
    /// reorders itself between polls is a model list that makes a picker look
    /// unstable, and it would also make a byte-for-byte comparison of two
    /// responses a test that fails intermittently.
    #[test]
    fn the_merged_document_is_ordered_by_tier_not_by_completion() {
        // Deliberately completed in reverse of tier order.
        let mut probed: Vec<(usize, u16, Option<String>)> = vec![
            (BACKEND_OLLAMA, 11434, Some("{\"data\":[{\"id\":\"ollama\"}]}".to_string())),
            (BACKEND_MLX, 8080, Some("{\"data\":[{\"id\":\"mlx\"}]}".to_string())),
            (BACKEND_LLAMA, 8081, Some("{\"data\":[{\"id\":\"llama\"}]}".to_string())),
        ];
        probed.sort_by_key(|(idx, _, _)| *idx);
        let order: Vec<String> = probed
            .iter()
            .filter_map(|(_, _, b)| b.as_ref())
            .map(|b| {
                b.split("\"id\":\"")
                    .nth(1)
                    .and_then(|s| s.split('"').next())
                    .unwrap_or_default()
                    .to_string()
            })
            .collect();
        assert_eq!(
            order,
            vec!["mlx", "llama", "ollama"],
            "tier order is the contract, so the document is stable across polls"
        );
    }

    /// A caller must not wait behind a peer's probe of the *same* backend.
    ///
    /// This is the property that keeps a model picker from exhausting the
    /// router: a blocked `/v1/models` caller holds one of the 128 inflight
    /// slots, and 128 of those make the accept loop answer every *other* client
    /// `503 lac_router_busy`. So `probe` uses `try_lock` and returns nothing
    /// rather than waiting — the caller falls through to single-backend
    /// forwarding, which is a correct and cheap answer.
    #[test]
    fn a_caller_never_queues_behind_another_callers_probe() {
        let memo = Arc::new(CatalogMemo::new(Duration::from_secs(60)));
        let (tx, rx) = std::sync::mpsc::channel();
        let held = Arc::clone(&memo);
        let holder = thread::spawn(move || {
            held.probe(BACKEND_MLX, || {
                // Still inside the probe: the lock is held.
                tx.send(()).ok();
                thread::sleep(Duration::from_millis(200));
                Some("slow".to_string())
            })
        });
        rx.recv_timeout(Duration::from_secs(5)).expect("probe started");

        // A second caller for the same backend must return immediately rather
        // than wait for the slow one to finish.
        let started = Instant::now();
        let second = memo.probe(BACKEND_MLX, || Some("second".to_string()));
        assert!(
            started.elapsed() < Duration::from_millis(150),
            "a second caller waited {:?} behind a peer's probe",
            started.elapsed()
        );
        assert_eq!(second, None, "and it declined rather than blocking");

        // A *different* backend is entirely unaffected.
        let other = memo.probe(BACKEND_LLAMA, || Some("llama".to_string()));
        assert_eq!(other.as_deref(), Some("llama"), "a different lane is not collateral damage");

        assert_eq!(holder.join().expect("holder").as_deref(), Some("slow"));
    }

    /// The summing rule a reader of the usage log depends on, expressed
    /// directly so it cannot drift from the call sites.
    ///
    /// `kv-manage` sums `est_tokens` over the window, and the docs call this
    /// log "the honest token source". Before the attempt field it was not: a
    /// request that failed over twice was recorded three times with the same
    /// `req_body`, so the estimate read up to 3x the traffic that actually
    /// happened, and up to 3x again when all three backends refused.
    #[test]
    fn a_retried_request_is_charged_once_not_once_per_attempt() {
        // The invariant, stated over every shape a request can take. Each row
        // is one rid: the backends it was tried against and how each ended.
        // `sum(req_body)` has to be the client's body, exactly once, however
        // many backends saw it.
        let body = 4000u64;
        let scenarios: [(&str, &[&str]); 6] = [
            // outcome per attempt, in order
            ("proxied", &["proxied"]),
            ("discarded 500 then the answer", &["http-error", "proxied"]),
            ("refused, then the answer", &["unreachable", "proxied"]),
            ("all three refused", &["unreachable", "unreachable", "unreachable"]),
            ("refused, then a cut stream", &["unreachable", "stream-cut"]),
            ("two 500s, then 501 upgrade", &["http-error", "http-error", "unsupported-upgrade"]),
        ];
        for (name, outcomes) in scenarios {
            // The arms charge `attempt_charge(attempt, body)`, where `attempt`
            // is the count of ports already tried when the attempt started.
            let charged: Vec<u64> = (0..outcomes.len())
                .map(|attempt| attempt_charge(attempt as u32, body))
                .collect();
            assert_eq!(
                charged.iter().sum::<u64>(),
                body,
                "{name}: the body is charged exactly once, got {charged:?}"
            );
            // And the alternative rule the log documents: the reader who sums
            // `req_body` where `attempt` is 0 gets the same number.
            assert_eq!(
                charged
                    .iter()
                    .enumerate()
                    .filter(|(attempt, _)| *attempt == 0)
                    .map(|(_, bytes)| bytes)
                    .sum::<u64>(),
                body,
                "{name}: attempt 0 carries the body"
            );
        }
    }

    #[test]
    fn a_failover_counts_the_context_once_on_both_sides_of_the_accounting() {
        // The same invariant in the per-backend stats block: a request that
        // failed over has to read as one request's worth of context, not two.
        let stats: StatsMap = Arc::new(Mutex::new(HashMap::new()));
        let body = 4000u64;
        let response = 800u64;
        // Attempt 0 answered 500: the body is charged, no response bytes.
        note_error(&stats, 8080);
        // Attempt 1 answered: the same body again, so charged is 0 — but this
        // backend really did receive it, which is what req_bytes records.
        note_success(&stats, 8081, 120.0, body, attempt_charge(1, body), response);
        let m = stats.lock().unwrap();
        let first = m.get(&8080).unwrap();
        let second = m.get(&8081).unwrap();
        assert_eq!(first.req_bytes, 0, "a backend that refused nothing received nothing");
        assert_eq!(second.req_bytes, body, "per-backend bytes stay honest");
        assert_eq!(
            first.est_tokens + second.est_tokens,
            response / 4,
            "context growth is counted once, for the answer the client got"
        );
    }

    /// `resp_body` is the bytes the client received, and it is the only input
    /// to the KV-risk estimate outside the request body. The response head was
    /// written to the client and charged nowhere: a 440-byte body behind a
    /// 160-byte head was logged as `resp_body: 440`, so `est_tokens` came out
    /// 25% low, and the error grew with head size — worst on the short
    /// completions where the head is most of the exchange. It is the number
    /// `kv-manage check` prints as the session's token load and the one
    /// `context-cap` throttles on, so "generated bytes" was quietly not all of
    /// them.
    #[test]
    fn a_sanitized_head_is_charged_at_the_length_actually_written() {
        // The backend's own head promises `Connection: keep-alive` and a
        // Content-Length the router does not keep, so the sanitized head is a
        // *different length* from the one received. The charge has to be the
        // written one; the received one is not reconstructible by a reader.
        let received = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: keep-alive\r\nContent-Length: 440\r\n\r\n";
        let sanitized = sanitize_downstream_head(received);
        assert_ne!(
            sanitized.len(),
            received.len(),
            "if these ever matched, this test would be asserting nothing about the length"
        );
        assert!(
            !sanitized.windows(11).any(|w| w == b"keep-alive"),
            "sanitize_downstream_head must rewrite the connection promise"
        );

        // The total a reader sees, over one exchange, is the whole response.
        let body = 440u64;
        let client_got = sanitized.len() as u64 + body;
        let tokens = (4000 + client_got) / 4;
        assert_eq!(
            tokens,
            (4000 + body) / 4 + (sanitized.len() as u64) / 4,
            "the head's contribution must be present in the estimate"
        );
        // And the old behaviour is exactly the shortfall: no head at all.
        assert!(
            tokens > (4000 + body) / 4,
            "a head that is not charged is a real, one-sided undercount"
        );
    }

    #[test]
    fn retryable_responses_advertise_a_delay_and_a_distinct_type() {
        let busy = retryable_http_response(
            "503 Service Unavailable",
            BUSY_TYPE,
            "router saturated (128 inflight)",
            1,
            None,
        );
        let budget = retryable_http_response(
            "503 Service Unavailable",
            BODY_BUDGET_TYPE,
            BODY_BUDGET_BUSY,
            5,
            None,
        );
        for response in [&busy, &budget] {
            assert!(response.starts_with("HTTP/1.1 503"), "{response}");
            assert!(response.contains("Retry-After: "), "{response}");
            // A wrong Content-Length truncates or hangs the client, so the
            // header must match the body byte for byte.
            let head_end = response.find("\r\n\r\n").expect("headers") + 4;
            let declared: usize = response
                .lines()
                .find_map(|line| line.strip_prefix("Content-Length: "))
                .expect("length")
                .trim()
                .parse()
                .expect("numeric length");
            assert_eq!(declared, response.len() - head_end, "{response}");
        }
        // The body's code must agree with the status line it is served under.
        assert!(busy.contains("\"code\":503"), "{busy}");
        // Two retryable causes must be told apart by a client, using the
        // same constants the call sites use.
        assert!(busy.contains(BUSY_TYPE) && budget.contains(BODY_BUDGET_TYPE));
        assert!(!busy.contains(BODY_BUDGET_TYPE), "{busy}");
        assert!(!budget.contains(BUSY_TYPE), "{budget}");
        assert_ne!(BUSY_TYPE, BODY_BUDGET_TYPE);
        // The body budget is held for the life of a relay, not a request,
        // so a one-second hint would send the client straight back into it.
        assert!(busy.contains("Retry-After: 1\r\n"), "{busy}");
        assert!(budget.contains("Retry-After: 5\r\n"), "{budget}");
    }

    #[test]
    fn read_errors_split_reset_from_hung() {
        // Everything that is not a lapsed budget reports as a reset: the
        // upstream is loopback, so the peer process went away.
        for kind in [
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::ConnectionAborted,
            io::ErrorKind::NotConnected,
            io::ErrorKind::UnexpectedEof,
            io::ErrorKind::BrokenPipe,
            io::ErrorKind::InvalidData,
            io::ErrorKind::Other,
        ] {
            assert_eq!(classify_read_error(kind), StreamEnd::Reset, "{kind:?}");
        }
        // A timeout reaching this point has already outlived the budget.
        assert_eq!(
            classify_read_error(io::ErrorKind::TimedOut),
            StreamEnd::Hung
        );
        assert_eq!(
            classify_read_error(io::ErrorKind::WouldBlock),
            StreamEnd::Hung
        );
    }

    #[test]
    fn sanitize_canonicalizes_content_length() {
        let duplicated = b"POST /v1/chat HTTP/1.1\r\nContent-Length: 4\r\nContent-Length: 4\r\n\r\nbody";
        let text = String::from_utf8(sanitize_upstream_request(duplicated)).expect("utf-8");
        assert_eq!(text.matches("Content-Length:").count(), 1, "{text}");
        assert!(text.contains("Content-Length: 4\r\n"), "{text}");

        let padded = b"POST /v1/chat HTTP/1.1\r\nContent-Length: 0004\r\n\r\nbody";
        let text = String::from_utf8(sanitize_upstream_request(padded)).expect("utf-8");
        assert!(text.contains("Content-Length: 4\r\n"), "{text}");
        assert!(!text.contains("0004"), "{text}");

        let expect = b"POST /v1/chat HTTP/1.1\r\nExpect: 100-continue\r\nContent-Length: 4\r\n\r\nbody";
        let text = String::from_utf8(sanitize_upstream_request(expect)).expect("utf-8");
        assert!(!text.contains("Expect"), "{text}");
        assert!(text.ends_with("\r\n\r\nbody"), "{text}");

        // Anything else named Expect is the client's own contract: keep it.
        let other = b"POST /v1/chat HTTP/1.1\r\nExpect: something-else\r\nContent-Length: 4\r\n\r\nbody";
        let text = String::from_utf8(sanitize_upstream_request(other)).expect("utf-8");
        assert!(text.contains("Expect: something-else"), "{text}");
    }

    #[test]
    fn data_array_located_with_strings() {
        let body = r#"{"object":"list","note":"has [brackets] inside","data":[{"id":"a","x":"]"},{"id":"b"}]}"#;
        let (s, e) = find_data_array(body).expect("array found");
        assert!(body[s..=e].contains("\"id\":\"a\""));
        assert!(body[s..=e].contains("\"id\":\"b\""));
        assert!(!body[s..=e].contains("inside"));
    }

    #[test]
    fn items_split_and_id_extracted() {
        let inner = r#"{"id":"a","v":1},{"id":"b","nested":{"x":[1,2]}}"#;
        let items = split_items(inner);
        assert_eq!(items.len(), 2);
        assert_eq!(item_id(&items[0]).as_deref(), Some("a"));
        assert_eq!(item_id(&items[1]).as_deref(), Some("b"));
    }

    #[test]
    fn id_ignores_nested_and_string_values() {
        // "id" inside a string value and inside a nested object must not win.
        let item = r#"{"note":"the \"id\" is here","id":"real","meta":{"id":"inner"}}"#;
        assert_eq!(item_id(item).as_deref(), Some("real"));
        let no_top = r#"{"meta":{"id":"inner"}}"#;
        assert_eq!(item_id(no_top), None);
    }

    #[test]
    fn request_line_parsed() {
        let (m, p) = request_line(b"POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(m, "POST");
        assert_eq!(p, "/v1/chat/completions");
        assert!(valid_request_line("POST /v1/chat HTTP/1.1"));
        assert!(valid_request_line("GET http://example.test/x HTTP/1.1"));
        assert!(!valid_request_line("POST /v1/chat HTTP/2"));
        assert!(!valid_request_line("POST  /v1/chat HTTP/1.1"));
    }

    #[test]
    fn breaker_matrix() {
        let now = Instant::now();
        assert!(!breaker_open(0, None, now));
        assert!(!breaker_open(2, Some(now), now));
        assert!(breaker_open(3, Some(now), now));
        assert!(!breaker_open(9, Some(now - Duration::from_secs(31)), now));
        assert!(!breaker_open(9, None, now));
    }

    #[test]
    fn sniff_extracts_model() {
        let req = b"POST /v1/chat HTTP/1.1\r\nHost: x\r\n\r\n{\"model\":\"qwen3.8-27b\",\"messages\":[]}";
        assert_eq!(sniff_model(req).as_deref(), Some("qwen3.8-27b"));
        assert_eq!(sniff_model(b"GET /v1/models HTTP/1.1\r\n\r\n"), None);
        assert_eq!(sniff_model(b"POST /x HTTP/1.1\r\n\r\n{\"model\": 42}"), None);
    }

    #[test]
    fn ewma_learns_and_breaker_trips() {
        let stats: StatsMap = Arc::new(Mutex::new(HashMap::new()));
        assert_eq!(ewma_of(&stats, 8080), EWMA_UNKNOWN_MS);
        note_success(&stats, 8080, 100.0, 400, 400, 400);
        note_success(&stats, 8080, 200.0, 400, 400, 400);
        assert!((ewma_of(&stats, 8080) - 130.0).abs() < 1e-6);
        note_error(&stats, 8080);
        note_error(&stats, 8080);
        assert!(!breaker_open_for(&stats, 8080));
        note_error(&stats, 8080);
        assert!(breaker_open_for(&stats, 8080));
        note_success(&stats, 8080, 50.0, 0, 0, 0);
        assert!(!breaker_open_for(&stats, 8080));
    }

    #[test]
    fn parse_content_length_extracted() {
        let req = b"POST /v1/chat HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 42\r\n\r\n{}";
        assert_eq!(parse_content_length(req), Some(42));
        let get = b"GET /v1/models HTTP/1.1\r\nHost: localhost\r\n\r\n";
        assert_eq!(parse_content_length(get), None);
        let duplicate = b"POST / HTTP/1.1\r\nContent-Length: 4\r\nContent-Length: 4\r\n\r\ntest";
        assert_eq!(parse_content_length_strict(duplicate), Ok(Some(4)));
        let conflicting = b"POST / HTTP/1.1\r\nContent-Length: 4\r\nContent-Length: 5\r\n\r\n";
        assert!(parse_content_length_strict(conflicting).is_err());
        let invalid = b"POST / HTTP/1.1\r\nContent-Length: nope\r\n\r\n";
        assert!(parse_content_length_strict(invalid).is_err());
        let signed = b"POST / HTTP/1.1\r\nContent-Length: +2\r\n\r\n";
        assert!(parse_content_length_strict(signed).is_err());
        let folded = b"POST / HTTP/1.1\r\n Content-Length: 2\r\n\r\n";
        assert!(parse_content_length_strict(folded).is_err());
    }

    #[test]
    fn resp_framing_parsed() {
        let json = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 48\r\nConnection: keep-alive\r\n\r\n";
        assert_eq!(
            parse_resp_headers(json),
            Ok((Some(48), false, false, false))
        );
        let sse = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n";
        assert_eq!(parse_resp_headers(sse), Ok((None, true, true, true)));
        let encoded_sse = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Encoding: gzip\r\nContent-Length: 3\r\n\r\n";
        assert_eq!(parse_resp_headers(encoded_sse), Ok((Some(3), false, true, false)));
        let both = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Length: 99\r\nContent-Type: text/event-stream\r\n\r\n";
        assert!(parse_resp_headers(both).is_err());
        let loose = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: notchunked\r\n\r\n";
        assert!(parse_resp_headers(loose).is_err());
        let media = b"HTTP/1.1 200 OK\r\nContent-Type: application/json; note=\"text/event-stream\"\r\nContent-Length: 2\r\n\r\n";
        assert_eq!(parse_resp_headers(media), Ok((Some(2), false, false, false)));
        let transfer = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
        assert_eq!(has_chunked_transfer_encoding(transfer), Ok(true));
        let chained = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\n";
        assert_eq!(has_chunked_transfer_encoding(chained), Ok(true));
        let unsupported = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked, gzip\r\n\r\n";
        assert!(has_chunked_transfer_encoding(unsupported).is_err());
        let duplicate = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked, chunked\r\n\r\n";
        assert!(has_chunked_transfer_encoding(duplicate).is_err());
        let spaced = b"HTTP/1.1 200 OK\r\nTransfer-Encoding : chunked\r\n\r\n";
        assert!(has_chunked_transfer_encoding(spaced).is_err());
        assert!(parse_resp_headers(spaced).is_err());
        let folded = b"HTTP/1.1 200 OK\r\nX-Test: yes\r\n Transfer-Encoding: chunked\r\n\r\n";
        assert!(has_chunked_transfer_encoding(folded).is_err());
        let conflicting = b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n";
        assert!(parse_resp_headers(conflicting).is_err());
        let obs_text = b"HTTP/1.1 200 OK\r\nX-Name: caf\xff\r\nContent-Length: 2\r\n\r\n";
        assert_eq!(parse_resp_headers(obs_text), Ok((Some(2), false, false, false)));
        let bws_name = b"HTTP/1.1 200 OK\r\nX-Trace : fine\r\nContent-Length: 2\r\n\r\n";
        assert_eq!(parse_resp_headers(bws_name), Ok((Some(2), false, false, false)));
        let duplicate_same = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n";
        assert_eq!(parse_resp_headers(duplicate_same), Ok((Some(2), false, false, false)));
    }

    #[test]
    fn chunk_terminal_detected_across_splits_and_trailers() {
        let wire = b"5;ext=1\r\nhello\r\n0\r\nX-Trailer: yes\r\n\r\n";
        for split in 0..=wire.len() {
            assert!(
                chunk_terminal_detected(&[&wire[..split], &wire[split..]]),
                "split at {} lost terminal chunk",
                split
            );
        }
        assert!(chunk_terminal_detected(&[b"0\r\n\r\n"]));
        assert!(!chunk_terminal_detected(&[b"5\r\nhello\r\n"]));
        assert!(!chunk_terminal_detected(&[b"5\r\nhello0\r\n\r\n0\r\n\r\n"]));
        assert!(!chunk_terminal_detected(&[b"5;bad ext=1\r\nhello\r\n0\r\n\r\n"]));
        assert!(chunk_terminal_detected(&[b"5 ;foo=\"a;b\"\r\nhello\r\n0\r\n\r\n"]));
        assert!(!chunk_terminal_detected(&[b"5;bad\x01ext=1\r\nhello\r\n0\r\n\r\n"]));
        assert!(!chunk_terminal_detected(&[b"0;\r\n\r\n"]));
        assert!(!chunk_terminal_detected(&[b"5;=x\r\nhello\r\n0\r\n\r\n"]));
        assert!(!chunk_terminal_detected(&[b"5;foo=\"unterminated\r\nhello\r\n0\r\n\r\n"]));
        assert!(!chunk_terminal_detected(&[b"0\r\nnot-a-header\r\n\r\n"]));
        assert!(!chunk_terminal_detected(&[b"0\r\nContent-Length: 4\r\n\r\n"]));
    }

    #[test]
    fn chunk_feed_reports_exact_terminal_offset() {
        let mut detector = ChunkedDetector::default();
        assert_eq!(
            detector.feed(b"0\r\n\r\nTRAILING"),
            Ok(ChunkProgress::Complete { consumed: 5 })
        );
    }

    #[test]
    fn chunk_feed_distinguishes_incomplete_and_invalid() {
        let mut detector = ChunkedDetector::default();
        assert_eq!(detector.feed(b"5\r\nhel"), Ok(ChunkProgress::NeedMore));
        assert_eq!(detector.feed(b"lo\r\n"), Ok(ChunkProgress::NeedMore));
        assert_eq!(detector.feed(b"0\n"), Err(()));
    }

    #[test]
    fn keepalive_is_allowed_only_at_a_complete_size_boundary() {
        let mut detector = ChunkedDetector::default();
        assert!(detector.can_inject_keepalive());
        assert_eq!(detector.feed(b"1"), Ok(ChunkProgress::NeedMore));
        assert!(!detector.can_inject_keepalive());
        assert_eq!(detector.feed(b"\r\na\r\n"), Ok(ChunkProgress::NeedMore));
        assert!(detector.can_inject_keepalive());
        assert_eq!(detector.feed(b"0\r\n"), Ok(ChunkProgress::NeedMore));
        assert!(!detector.can_inject_keepalive());
        assert_eq!(detector.feed(b"\r\n"), Ok(ChunkProgress::Complete { consumed: 2 }));
    }

    #[test]
    fn keepalive_requires_a_complete_sse_event_boundary() {
        let mut detector = ChunkedDetector {
            track_payload: true,
            ..ChunkedDetector::default()
        };
        assert_eq!(
            detector.feed(b"a\r\ndata: part\r\n"),
            Ok(ChunkProgress::NeedMore)
        );
        assert!(detector.can_inject_keepalive());
        assert!(!detector.payload_at_sse_boundary());
        assert_eq!(detector.feed(b"5\r\nial\n\n\r\n"), Ok(ChunkProgress::NeedMore));
        assert!(detector.can_inject_keepalive());
        assert!(
            detector.payload_at_sse_boundary(),
            "tail={:?} len={}",
            detector.payload_tail,
            detector.payload_len
        );

        let mut crlf = ChunkedDetector {
            track_payload: true,
            ..ChunkedDetector::default()
        };
        assert_eq!(crlf.feed(b"4\r\n\r\n\r\n\r\n"), Ok(ChunkProgress::NeedMore));
        assert!(crlf.can_inject_keepalive());
        assert!(crlf.payload_at_sse_boundary());
        assert!(payload_at_boundary(b"\0x\r\r", 4));
        assert!(payload_at_boundary(b"x\n\r\n", 4));
        assert!(!payload_at_boundary(b"\0\0x\r", 4));
        assert!(!payload_at_boundary(b"\0\0x\n", 4));
        assert!(!payload_at_boundary(b"\0\0\0\0", 1));
        let mut tiny_tail = [0u8; 4];
        let mut tiny_len = 0usize;
        for byte in b"abcd" {
            update_payload_tail(&mut tiny_tail, &mut tiny_len, &[*byte]);
        }
        assert_eq!(tiny_len, 4);
        assert_eq!(tiny_tail, *b"abcd");
    }

    #[test]
    fn response_status_parsed() {
        assert_eq!(response_status(b"HTTP/1.1 200 OK\r\n\r\n"), Some(200));
        assert_eq!(response_status(b"HTTP/1.1 503 Service Unavailable\r\n\r\n"), Some(503));
        assert_eq!(response_status(b"HTTP/1.1 600 Extension\r\n\r\n"), Some(600));
        assert_eq!(response_status(b"HTTP/1.1 200\r\n\r\n"), Some(200));
        assert_eq!(response_status(b"HTTP/1.1 200\tOK\r\n\r\n"), None);
        assert_eq!(response_status(b"HTTP/1.1 200\x0bOK\r\n\r\n"), None);
        assert_eq!(response_status(b"HTTP/1.1 +200 OK\r\n\r\n"), None);
        assert_eq!(response_status(b"garbage 200 OK\r\n\r\n"), None);
        assert_eq!(response_status(b"not-http"), None);
    }

    #[test]
    fn bearer_gate() {
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("::1"));
        assert!(!is_loopback_host("0.0.0.0"));
        assert!(!is_loopback_host("100.64.0.5"));
        assert_eq!(bind_address("127.0.0.1", 8000), "127.0.0.1:8000");
        assert_eq!(bind_address("::1", 8000), "[::1]:8000");
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "abcd"));
        let req = b"GET /v1/models HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer sekrit\r\n\r\n";
        assert_eq!(bearer_from(req).as_deref(), Some("sekrit"));
        let mixed = b"GET /v1/models HTTP/1.1\r\nAuthorization: bEaReR sekrit\r\n\r\n";
        assert_eq!(bearer_from(mixed).as_deref(), Some("sekrit"));
        let noauth = b"GET /v1/models HTTP/1.1\r\nHost: x\r\n\r\n";
        assert_eq!(bearer_from(noauth), None);
        let bad = b"GET /x HTTP/1.1\r\nAuthorization: Basic abc\r\n\r\n";
        assert_eq!(bearer_from(bad), None);
        // Only the Authorization header counts, and only its first
        // occurrence: a Proxy-Authorization must not stand in for it.
        let proxy_only = b"GET /x HTTP/1.1\r\nProxy-Authorization: Bearer sekrit\r\n\r\n";
        assert_eq!(bearer_from(proxy_only), None);
        // A token is taken whole, never as a prefix.
        let trailing = b"GET /x HTTP/1.1\r\nAuthorization: Bearer sekrit extra\r\n\r\n";
        assert_eq!(bearer_from(trailing).as_deref(), Some("sekrit extra"));
        let empty = b"GET /x HTTP/1.1\r\nAuthorization: Bearer \r\n\r\n";
        assert_eq!(bearer_from(empty), None);
        let no_token = b"GET /x HTTP/1.1\r\nAuthorization: Bearer\r\n\r\n";
        assert_eq!(bearer_from(no_token), None);
        // Lowercase field name is still the field.
        let lower = b"GET /v1/models HTTP/1.1\r\nauthorization: Bearer sekrit\r\n\r\n";
        assert_eq!(bearer_from(lower).as_deref(), Some("sekrit"));
        // Duplicate headers: the first decides, and a wrong one does not
        // fall through to a later correct one.
        let dup_wrong_first = b"GET /x HTTP/1.1\r\nAuthorization: Bearer wrong\r\nAuthorization: Bearer sekrit\r\n\r\n";
        assert_eq!(bearer_from(dup_wrong_first).as_deref(), Some("wrong"));
        let forwarded = b"GET /v1/models HTTP/1.1\r\nX-Forwarded-For: 100.64.0.5\r\n\r\n";
        assert!(forwarded_request(forwarded));
        let peer_lo: SocketAddr = "127.0.0.1:8000".parse().unwrap();
        let peer_remote: SocketAddr = "100.64.0.5:1234".parse().unwrap();
        assert!(is_loopback_peer(&peer_lo));
        assert!(!is_loopback_peer(&peer_remote));
    }

    #[test]
    fn token_comparison_does_not_leak_length() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(constant_time_eq("", ""));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "abcd"));
        assert!(!constant_time_eq("abcd", "abc"));
        // A length difference must reach the accumulator rather than skip the
        // loop, so a wrong-length guess is not answered in constant time.
        // Exercised here as behaviour: every length difference is a mismatch,
        // including the ones that differ only in length.
        for a in ["", "a", "ab", "abc", "abcd", "abcde"] {
            for b in ["", "a", "ab", "abc", "abcd", "abcde"] {
                assert_eq!(constant_time_eq(a, b), a == b, "{a:?} vs {b:?}");
            }
        }
    }

    #[test]
    fn host_authority_normalizes_port_brackets_and_case() {
        assert_eq!(host_authority("127.0.0.1:8000"), "127.0.0.1");
        assert_eq!(host_authority("127.0.0.1"), "127.0.0.1");
        assert_eq!(host_authority("LocalHost:8000"), "localhost");
        assert_eq!(host_authority("localhost."), "localhost");
        assert_eq!(host_authority("[::1]:8000"), "::1");
        assert_eq!(host_authority("[::1]"), "::1");
        // An unbracketed IPv6 literal is all colons and must not be mistaken
        // for a host:port pair.
        assert_eq!(host_authority("::1"), "::1");
        assert_eq!(host_authority("fe80::1"), "fe80::1");
        assert_eq!(host_authority("  evil.example:8000  "), "evil.example");
        // A trailing colon is a separator with no port after it, not a port.
        assert_eq!(host_authority("evil.example:"), "evil.example");
        assert_eq!(host_authority(""), "");
    }

    #[test]
    fn host_gate_serves_ip_and_localhost_only_without_a_token() {
        let allowed: Vec<String> = Vec::new();
        for host in [
            "127.0.0.1:8000",
            "127.0.0.1",
            "localhost",
            "localhost:8000",
            "LOCALHOST.",
            "[::1]:8000",
            "::1",
            "100.64.0.5:8000",
            // Any IP literal is served, not just loopback: the rule is "not a
            // DNS name", because a rebound request can only ever carry the
            // attacker's *name* in Host. A browser cannot be made to send an
            // IP literal in Host for a page loaded from somewhere else.
            "0.0.0.0:8000",
        ] {
            assert!(host_served(Some(host), false, &allowed), "{host}");
        }
        // The rebinding vector: an attacker-chosen DNS name.
        for host in [
            "evil.example",
            "evil.example:8000",
            "attacker.test",
            "localhost.evil.example",
            "127.0.0.1.evil.example",
            "0.0.0.0.evil.example",
        ] {
            assert!(!host_served(Some(host), false, &allowed), "{host}");
        }
        // A missing Host is an HTTP/1.0 client; every browser sends one, so
        // its absence cannot be a rebinding vector.
        assert!(host_served(None, false, &allowed));
        // An empty authority is a malformed Host, not a wildcard.
        assert!(!host_served(Some(""), false, &allowed));
        assert!(!host_served(Some(":8000"), false, &allowed));
    }

    #[test]
    fn host_gate_opens_for_the_operator_allowlist_or_a_proven_token() {
        let allowed = vec!["studio.local".to_string(), "box.tail1234.ts.net".to_string()];
        assert!(host_served(Some("studio.local:8000"), false, &allowed));
        assert!(host_served(
            Some("box.tail1234.ts.net:8000"),
            false,
            &allowed
        ));
        // Allow-list matching is on the normalized authority, so a port or a
        // trailing dot in the header does not miss it.
        assert!(host_served(Some("STUDIO.local"), false, &allowed));
        assert!(!host_served(Some("studio.local.evil.example"), false, &allowed));
        assert!(!host_served(Some("evil.example"), false, &allowed));
        // A valid Bearer token is proof of a non-browser client, which is what
        // keeps MagicDNS usable on a token-protected gateway.
        assert!(host_served(
            Some("mac.tail1234.ts.net:8000"),
            true,
            &allowed
        ));
        // And it is the *only* thing that opens it: a wrong token does not.
        assert!(!host_served(Some("evil.example"), false, &allowed));
    }

    #[test]
    fn cors_origin_refuses_by_default_and_echoes_only_what_was_allowed() {
        let none: Vec<String> = Vec::new();
        // No Origin at all: a native client, and the case that must never
        // grow a CORS header.
        assert_eq!(cors_origin(None, &none), None);
        assert_eq!(cors_origin(Some("https://evil.example"), &none), None);
        assert_eq!(cors_origin(Some("null"), &none), None);
        assert_eq!(cors_origin(Some(""), &none), None);
        assert_eq!(cors_origin(Some("   "), &none), None);

        let allowed = vec!["https://app.example".to_string()];
        // The echoed value is the allow-list entry, not the request's string:
        // only a string the operator wrote can reach a response header.
        assert_eq!(
            cors_origin(Some("https://app.example"), &allowed).as_deref(),
            Some("https://app.example")
        );
        // Matching is case-insensitive on scheme and host, as origins are.
        assert_eq!(
            cors_origin(Some("HTTPS://App.Example"), &allowed).as_deref(),
            Some("https://app.example")
        );
        // Surrounding whitespace is trimmed, not required.
        assert_eq!(
            cors_origin(Some(" https://app.example "), &allowed).as_deref(),
            Some("https://app.example")
        );
        // A prefix, a suffix, a different port and a different scheme are all
        // different origins.
        for other in [
            "https://app.example.evil.test",
            "https://app.example/",
            "https://app.example:8443",
            "http://app.example",
            "https://evil.test",
        ] {
            assert_eq!(cors_origin(Some(other), &allowed), None, "{other}");
        }
        // An absurd Origin is refused rather than compared.
        assert_eq!(cors_origin(Some(&"a".repeat(1024)), &allowed), None);
        // An explicit `*` is the operator opting every page in.
        assert_eq!(
            cors_origin(Some("https://anything.test"), &["*".to_string()]).as_deref(),
            Some("*")
        );
    }

    #[test]
    fn first_header_takes_the_first_occurrence() {
        let req = b"GET / HTTP/1.1\r\nHost: first.example\r\nHost: second.example\r\nOrigin: https://a.test\r\nOrigin: https://b.test\r\n\r\n";
        assert_eq!(
            first_header(req, "host").as_deref(),
            Some("first.example"),
            "a second Host must not contradict the first"
        );
        assert_eq!(first_header(req, "origin").as_deref(), Some("https://a.test"));
        assert_eq!(first_header(req, "HOST").as_deref(), Some("first.example"));
        assert_eq!(first_header(req, "absent"), None);
        // Nothing after the header block counts.
        let after = b"GET / HTTP/1.1\r\nHost: real.example\r\n\r\nHost: smuggled.example\r\n";
        assert_eq!(first_header(after, "host").as_deref(), Some("real.example"));
    }

    #[test]
    fn cors_headers_are_absent_unless_an_origin_was_allowed() {
        // The default reply carries no CORS header whatsoever: no Origin
        // means no browser-readable surface.
        let native = http_response("200 OK", "application/json", "{}", None);
        assert!(!native.contains("Access-Control"), "{native}");
        assert!(!native.contains("Vary"), "{native}");
        // An allow-listed origin gets its own value back, never a wildcard
        // the router chose on its own, plus Vary so a shared cache cannot
        // hand that reply to a page that was not allowed.
        let browser = http_response("200 OK", "application/json", "{}", Some("https://app.example"));
        assert!(
            browser.contains("Access-Control-Allow-Origin: https://app.example\r\n"),
            "{browser}"
        );
        assert!(browser.contains("Vary: Origin\r\n"), "{browser}");
        assert!(!browser.contains("Allow-Origin: *"), "{browser}");
        // Header order and the Content-Length/body relationship still hold, or
        // the client would mis-frame the response.
        let head_end = browser.find("\r\n\r\n").expect("headers") + 4;
        let declared: usize = browser
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .expect("length")
            .trim()
            .parse()
            .expect("numeric");
        assert_eq!(declared, browser.len() - head_end, "{browser}");
    }
}

#[cfg(test)]
mod port_resolution_tests {
    // A `#[test]` that loses its attribute is valid Rust that never runs:
    // the suite goes one test quieter and stays green. Fail the build instead.
    #![deny(dead_code)]

    use super::*;

    /// The router's own readers had to agree with `common`'s, and two of them
    /// were still bare `parse()`s after the one-rule pass. Zero survives those,
    /// and zero is not a port: the router warns `ignored, using 8081` and then
    /// probes 0. Every reader in the project resolves the variable through
    /// `common`, so the router was the last writer still speaking a different
    /// language — and `StatsMap` is keyed by `u16`, so two lanes configured to
    /// 0 shared one error count and one breaker verdict.
    #[test]
    fn every_port_reader_agrees_with_the_common_rule() {
        for (var, expected_default) in [
            ("LOOP_STUDIO_LLAMA_PORT", 8081u16),
            ("LOOP_STUDIO_OLLAMA_PORT", 11434u16),
            ("LOOP_STUDIO_ROUTER_PORT", 8000u16),
        ] {
            unsafe { std::env::remove_var(var) };
            for unusable in ["0", "", " 0 ", "-1", "70000", "http", "8080.0"] {
                unsafe { std::env::set_var(var, unusable) };
                // The warning the operator reads has to match what is dialled.
                assert!(
                    common::port_env_notes()
                        .iter()
                        .any(|n| n.starts_with(var)),
                    "{var}={unusable:?} is unusable, so it must be reported, not silently kept"
                );
            }
            // A real port is honoured, with surrounding whitespace tolerated —
            // the shape a plist edit produces.
            unsafe { std::env::set_var(var, " 9123 ") };
            assert!(
                common::port_env_notes().is_empty(),
                "a real port must not be reported as unusable"
            );
            unsafe { std::env::remove_var(var) };
            // And the default is the documented one, in both readers.
            match var {
                "LOOP_STUDIO_LLAMA_PORT" => assert_eq!(port_llama(), expected_default),
                "LOOP_STUDIO_OLLAMA_PORT" => assert_eq!(port_ollama(), expected_default),
                _ => assert_eq!(common::gateway_port(), expected_default),
            }
        }
    }

    /// The router's tier list must not be able to collapse two lanes into one
    /// key. `StatsMap` is `HashMap<u16, _>`, so two lanes on the same port
    /// shared one entry — and `/lac/status` then printed the same error count
    /// under two backend names, charging one lane's faults to the other.
    #[test]
    fn the_lanes_cannot_share_one_stats_key() {
        // Distinct valid ports: three keys, as the tier order implies.
        let tiers = tier_backends();
        let mut ports: Vec<u16> = tiers.iter().map(|(_, p)| *p).collect();
        ports.sort_unstable();
        ports.dedup();
        assert_eq!(
            ports.len(),
            tiers.len(),
            "tier ports must be distinct: {tiers:?}"
        );
        assert!(
            tiers.iter().all(|(_, p)| *p > 0),
            "a tier port of zero is not a port: {tiers:?}"
        );
    }

    /// The gateway's own bind port, which is the one reader that could not be
    /// observed from a pure function until now. `0` here is the whole bug: the
    /// bind moves somewhere random while every other reader of the same
    /// variable keeps looking at 0, and the warning the operator reads says the
    /// zero was ignored.
    #[test]
    fn the_gateway_binds_the_port_every_other_reader_dials() {
        // Absent: the documented default, and the same one `common` gives.
        assert_eq!(resolve_bind_port(None, None), 8000);
        assert_eq!(resolve_bind_port(None, None), common::gateway_port());

        // A real port, from either source, whitespace tolerated.
        assert_eq!(resolve_bind_port(Some("9123".to_string()), None), 9123);
        assert_eq!(resolve_bind_port(Some(" 9123 ".to_string()), None), 9123);
        assert_eq!(resolve_bind_port(None, Some("9124".to_string())), 9124);

        // The regression: zero, and every other unusable value, must land on
        // the same port the rest of the project will dial — not on a kernel-
        // chosen one.
        for unusable in ["0", " 0 ", "-1", "70000", "http", "", "8080.0"] {
            assert_eq!(
                resolve_bind_port(Some(unusable.to_string()), None),
                8000,
                "LOOP_STUDIO_ROUTER_PORT={unusable:?} must not relocate the gateway"
            );
            assert_eq!(
                resolve_bind_port(None, Some(unusable.to_string())),
                8000,
                "an unusable positional port must not relocate the gateway"
            );
        }

        // And the explicit word, which is the only way to get an ephemeral bind.
        assert_eq!(resolve_bind_port(Some("ephemeral".to_string()), None), 0);
        assert_eq!(resolve_bind_port(None, Some("ephemeral".to_string())), 0);
        assert_eq!(resolve_bind_port(Some(" Ephemeral ".to_string()), None), 0);
        for not_the_word in ["0", "EPHEMERAL0", "", "0x0", "eph"] {
            assert!(
                !is_ephemeral_request(Some(not_the_word)),
                "{not_the_word:?} must not read as a request for a kernel-chosen port"
            );
        }
    }
}

#[cfg(test)]
mod host_facts_tests {
    // A `#[test]` that loses its attribute is valid Rust that never runs:
    // the suite goes one test quieter and stays green. Fail the build instead.
    #![deny(dead_code)]

    use super::*;
    use std::time::Duration;

    /// The cache is what keeps the status endpoint cheap: three subprocess
    /// probes (`pmset`, `vm_stat`, `sysctl`) on a timer, never on a request.
    #[test]
    fn host_facts_are_measured_once_and_served_until_the_ttl_passes() {
        let cache = HostFactsCache::new(Duration::from_millis(30));
        assert!(
            cache.snapshot().is_none(),
            "nothing has been measured yet — the endpoint must not probe on the first request"
        );

        let first = cache.serve().expect("the first serve measures");
        // The reading is whatever the host says, and no test can demand a
        // level: a CI box without pmset, or a laptop mid-throttle, both answer
        // honestly here. What is asserted is the shape — a level, or absent,
        // never a defaulted one.
        assert!(
            matches!(first.thermal.as_deref(), None | Some("Nominal" | "Fair" | "Serious" | "Critical")),
            "an unexpected thermal reading: {:?}",
            first.thermal
        );

        // Inside the ttl the reading is served, not re-probed.
        assert!(cache.serve().is_some());

        // Past the ttl the next serve recomputes.
        thread::sleep(Duration::from_millis(50));
        let second = cache.serve().expect("a stale reading is recomputed");
        assert!(
            matches!(
                second.thermal.as_deref(),
                None | Some("Nominal" | "Fair" | "Serious" | "Critical")
            ),
            "an unexpected thermal reading: {:?}",
            second.thermal
        );
    }
}
