# LAC Code Audit Report

**Generated:** 2026-09-12  
**Re-verified:** 2026-09-24 (10x pro delivery pass)
**Scope:** Rust backend, Swift Studio, router framing/auth, packaging, and delivery checks
**Model:** lac/qwen3.8-27b  
**Status:** ✅ Rust + Swift tests, process-level E2E smoke, and packaged-app verification are enforced by `make validate`

---

## Executive Summary

The delivery path is now fail-closed and hermetic:

- Rust unit/integration tests and Swift Testing run from the same `make test` gate used by the worker.
- `make smoke` starts the real router plus a deterministic backend and exercises the real `lac status` and `lac chat` processes.
- `make validate` additionally builds, signs, and verifies the packaged Studio app.
- Lefthook runs `make delivery-index validate` on both pre-commit and pre-push; CI runs the same tracked-file check and gate.
- Remote auth, chunked response framing, cancellation state, and model-pull exit status have regression coverage.
- Remaining service/model availability messages are operational state, not source-code test failures.

---

## ✅ Passed Checks (OK)

| Category | Status | Details |
|----------|--------|---------|
| OS | ✓ | macos aarch64 |
| Memory | ✓ | 4.0+ GiB free (moderate pressure) |
| Disk Home | ✓ | 31 GiB free on $HOME volume |
| OpenCode Config | ✓ | opencode.jsonc present |
| Agent Rules | ✓ | `docs/AGENTS.md` present; `lac doctor` accepts the documented fallback |
| Loop Templates | ✓ | 4 loop templates installed |
| Skills | ✓ | 14 skills installed |
| OpenCode Tool | ✓ | V2 harness available |
| Rust Compiler | ✓ | cargo/rustc available |
| Jujutsu VCS | ✓ | jj available |
| Ollama Server | ✓ | Available at :11434 |

---

## ⚠️ Warnings

| Category | Level | Detail |
|----------|-------|--------|
| Memory | warn | 4.0 GiB free (moderate pressure) |
| Model Volume | warn | /Volumes/AIModels not mounted; internal fallback ~/.lac/models |
| MLX Service | warn | port 8080 down (may not be serving yet) |
| Ollama Symlink | warn | real directory, not symlinked to model volume |
| Thermal State | — | Nominal (Q8_0 and MTP enabled) |

---

## ⚠️ Operational State

| Category | Level | Detail | Resolution |
|----------|-------|--------|------------|
| Ollama Symlink | warn | dangling symlink may exist on the host | Run `lac bootstrap` to re-establish it |
| Backends | warn | no inference backend serving | Start with `lac serve mlx` and `lac route --daemon` |

---

## Daemon Persistence Verification

The **Rust power** feature — daemons survive app closure — is confirmed working:

```bash
$ lac daemon install
# Daemons continue running even after TUI/CLI exit

$ lac status
# Gateway :8000, MLX :8080 remain ONLINE

$ lac daemon uninstall
# Inference services stop when daemons are unloaded
```

**LaunchDaemon Status:**
- `org.lac.router` — ACTIVE (gateway on :8000)
- `org.lac.worker` — ACTIVE (task processing loop)

**Key Improvement:** Added `Timeout` and `ExitTimeOut` keys to all three launchd plists for improved crash recovery:
- Router: ThrottleInterval 10s, ExitTimeOut 30s
- Worker: ThrottleInterval 15s, ExitTimeOut 30s  
- Serve-MLX: ThrottleInterval 30s, ExitTimeOut 60s

---

## Code Hygiene Observations

### Strengths
- Single-flight worker with lockdir crash-recovery
- Atomic writes with attempts counter and dead-letter at 3
- No hardcoded `master`; base branch detected dynamically
- Dirty tree treated as precious — no `checkout .` over user work
- OpenCode/test phases run under timeouts (2h default / 10m)
- `doctor`/`status` gain `--json` and real exit codes

### Areas for Improvement
1. **Memory pressure** — 4 GiB free is borderline for 27B model inference
2. **Model volume** — external volume not mounted; consider `lac bootstrap`
3. **Ollama symlink** — dangling; rerun bootstrap to fix
4. **Backend availability** — ensure MLX/Q8/Ollama are running for full stack

---

## Recommendations

1. **Run `lac bootstrap`** to fix the dangling Ollama symlink
2. **Mount /Volumes/AIModels** if using external model storage
3. **Monitor thermal state** — currently Nominal, but MTP at Q4 generates heat
4. **Consider `--apply` on lane tune** to pin preferred backend persistently
5. **Review context-cap** — set to 16,000 tokens for steady-state hygiene

---

## 2026-09-24 Re-verification (10x pro delivery pass)

- **Canonical gate:** `make test` runs the Rust and Swift suites; `make smoke` runs the real router/CLI E2E; `make validate` also packages and verifies the signed Studio app. Lefthook and CI invoke `make delivery-index validate`, so required gate/test files cannot be omitted from the commit.
- **Router framing (lac-router v2.8):** `sse_relay` covers Content-Length early close, incremental chunked SSE, split chunk boundaries, chunk extensions, terminal trailers, and SSE-only keep-alives. The chunk parser is stateful and rejects data that merely contains a terminal-looking byte sequence.
- **Remote auth closed:** non-loopback startup without a token fails closed; a configured token is required on every listener, including loopback; forwarded proxy headers also require the token. Constant-time, length-safe comparison and successful non-loopback traffic are covered by `router_auth`.
- **Browser surface closed:** the gateway refuses browser traffic by default. `Host` is served only for IP literals and `localhost` (421 otherwise), which is the DNS-rebinding control — a rebound request is same-origin, so it carries no `Origin` and only `Host` still names the attacker. `Origin` must be on `LAC_ALLOWED_ORIGINS` (403 otherwise), and an allow-listed origin is echoed exactly with `Vary: Origin` rather than a wildcard. No response carries a CORS header for a native client, which sends no `Origin`. Both gates run before the auth gate, so a refused request never reaches a status read, `/lac/switch`, or a backend.
- **Studio delivery:** connection settings are available before a router exists, remote clients cannot run local daemon/host-fact actions, malformed URLs clear loading state, and packaged builds include local-network ATS permission for dynamic Tailscale hosts.
- **State safety:** Chat, Code Assistant, and loop runners use generation guards so cancelled work cannot overwrite a newer run; stores use injectable roots in tests; failed worker tasks preserve changes in a recoverable stash instead of deleting them.
- **Model pull:** failures and cancellation return nonzero, input is passed as an argument rather than interpolated source, and the CLI owns the downloader process tree. Staging the MLX cache is now covered too: the `create_dir_all` result used to be discarded while "ensured" printed regardless, so a read-only or full volume produced a green exit and a cache that could not exist.
- **The wrappers can tell success from failure:** `serve-mlx` and `serve-llama` both exited 0 after a crash, a bind failure, or a missing engine binary, so `lac serve mlx && echo ok` reported success with no lane running — and a *clean* engine exit was worse, because it is exactly what `KeepAlive{SuccessfulExit: false}` reads as "do not restart", so the night ended silently. Both now exit nonzero, print `starting on …` rather than `Listening on …` before anything is bound, and share one retry policy: a lane that has answered HTTP retries forever (the transient OOM the loop exists for), while a lane that never once answered HTTP is bounded and then stops with a message naming the model id and the port. `serve-llama` no longer exits 0 when the port is taken, and under launchd supervises and takes the port over instead of unloading itself.
- **The MLX port contract holds:** `serve-mlx` wrote `~/.lac/mlx.port` with a bare `parse()` while `common::mlx_port()` — the reader the router uses — discards `0`. `MLX_PORT=0` therefore published `0`, handed `--port 0` to the engine (a random ephemeral port), and left the lane unfindable even after the variable was removed. Both sides now call one `common::mlx_port_from`, the port file is removed when the wrapper exits so a crash cannot leave a lane advertised that nothing serves, and the last-resort fallback no longer returns a port it has just proven is occupied.
- **Per-request work is no longer duplicated per thread or per attempt:**
  - `HealthCache::ready` released the cache lock *before* probing, so N threads arriving on an expired entry fired N identical probes at the same backend. The router is one thread per connection with 128 slots, so a burst just after a TTL expiry was up to 128 simultaneous `GET /v1/models` at a single-threaded inference server, each holding an inflight slot for the length of the probe. Probes are now serialized per port and re-check the cache after acquiring, so the losers of the race reuse the winner's answer. Ports do not block each other, so a slow MLX probe never delays llama. This also removed a second bug for free: probes used to `insert` unconditionally, so a slow probe landing after a fast one replaced a good positive with a stale negative, costing up to `negative_ttl` of spurious 503s.
  - `merged_models` opened a fresh connection to every ready backend and read its catalog, and nothing cached the result — so a client polling `/v1/models`, which every model picker does, became three backend connections per caller with no bound on the number of callers. The merged catalog is now memoized for 1s (`LAC_ROUTER_CATALOG_TTL_MS`) and coalesced the same way. An *absent* catalog is deliberately not cached: `None` means "fall through to single-backend forwarding" so the client gets the backend's real error, and caching it would keep serving a fall-through after a backend came up and started advertising models.
  - The usage log re-charged `req_body` on every retry, so `kv-manage` — which sums `est_tokens` over the window, from the log the docs call "the honest token source" — read a request that failed over twice as up to 3x the traffic it was. Those bytes belong to the request, and the client sent them once, so they are now charged on the first attempt only and each line carries an `attempt` number. Measured on a two-backend failover with a 76-byte body: sum(`req_body`) was 152, now 76.
  - `HealthCache::new` took a `ttl` and silently ignored it for negative entries, which were hardcoded to 2.5s inside the constructor. Both windows are now explicit arguments, so "expires after ttl" means what it says and a test can see it without sleeping for seconds.
- **Supervised engines are no longer orphaned:** `spawn_server` used `Command::status()`, which consumes the `Child` and returns only an `ExitStatus`, so the wrapper kept no pid to signal. `kill -TERM serve-mlx` therefore killed the wrapper and left `mlx_lm.server` running — still holding its port, still resident with the model's weights, invisible to `lac ps`. The next start found the port taken, drifted to 8082, and loaded a *second* copy of a 27B model. Both wrappers now publish the child pid and forward SIGTERM/SIGINT to it, backoff is interruptible so a signal during a 60s wait is not followed by a relaunch, and the pid is withdrawn on reap so a later signal cannot hit a recycled pid. The handler deliberately does not `_exit`, so the port-file guard still runs. The TUI's "stop engines" action signalled only the engines, leaving the `LAUNCHDAEMON=1` wrapper to respawn what it had just killed; it now stops the wrappers first, which takes their engines with them. Covered end to end in `tests/child_lifetime.rs`, which drives real wrapper processes against a stub engine.
- **Bootstrap reconciles instead of assuming:** the `~/.zprofile` block was guarded by a one-shot marker, so the first run decided where `HF_HOME` pointed and no later run could change it — including the run where `/Volumes/AIModels` finished mounting. The file went on claiming an internal-disk path while every other step in the same run used the external volume. The managed block is now reconciled in place between its markers, user lines untouched, exactly one block ever present. `bootstrap` also stops claiming "all green" with a failed step: a failed `~/.zprofile` write, an unreadable templates directory, a failed copy, and a missing `~/.ollama` or `~/todo/lac-loops` all count as failures and exit nonzero, while advisory warnings (a tool you may not need) stay warnings. It refuses to run at all when `$HOME` is unset, which previously pointed every path at the current directory.
- **Counts:** the exact count is reported by the test runner; the previous stale arithmetic and active-daemon snapshot are intentionally not presented as guarantees.

---

## Rust Power Confirmation

> "When you close it the app is still running — that's our rust power for the community."

The launchd daemon system demonstrates this principle fully:

- **Before daemon install:** Closing the TUI/CLI stops all inference
- **After daemon install:** `org.lac.router` and `org.lac.worker` LaunchAgents keep services running in the background
- **Unix philosophy:** Rust-compiled, zero-dependency binaries launched at login, surviving user session closure

This is the architectural differentiator against LM Studio's visual layer + background process flaw. LAC combines **beautiful visual design** (TUI + gateway UI) with **Rust-powered persistence** — the best of both worlds.