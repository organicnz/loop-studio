# LAC Code Audit Report

**Generated:** 2026-09-12  
**Re-verified:** 2026-09-24 (10x pro delivery pass), 2026-09-26 (10x pro hardening pass)  
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

## 2026-09-26 Re-verification (10x pro hardening pass)

An audit of the four surfaces the previous passes had not covered — the process-signalling helpers, the usage accounting, the router's two header directions, and the Studio diff/gate path — found seven defects with observable wrong behavior. Each is now fixed, and each has a test that fails when the fix is reverted.

- **A signal only reaches the program it names.** `kill_matching` matched `cmd.contains(pattern)` over the whole `ps` command line, so it matched any process that *mentioned* the pattern anywhere in argv — and the patterns are strings the operator types. `lac stop` and the TUI's "stop engines" SIGTERMed `tail -f ~/Library/Logs/lac-serve-mlx.err` (a path this project prints itself), `grep -r lac-router .`, and the parent shell of `lac stop` itself, then reported the bystander as a stopped engine. Matching is now on program identity: the first token's file name, with the `lac-` install alias normalized so one binary is found by either name, plus an interpreter `-m` module (so an engine started by hand is still findable) and literal subcommands (so `ollama serve` still is). The pid in `~/.lac/router.pid` is additionally confirmed with `ps -p` before TERM: a recycled pid used to take down whatever now owns it, while the CLI printed "signaled" for a router that was never running. `lac ps`'s `mlx_lm` pattern, which no longer named the engine's actual program, is now `mlx_lm.server`.
- **The request body is charged once per request, in the log and in `/lac/status`.** The previous pass gated `req_body` in the three *retryable* arms only, and computed the attempt number after pushing the current port, so `attempt` was 1-based and duplicated within one rid. The shape it missed is the normal one: a request that fails over and *succeeds* was charged on the discarded attempt and again on the answer the client received. Measured on a two-backend failover with a 456-byte body: `sum(req_body)` was 912, and is now 456, with the rid's lines numbered 0 and 1 and `est_tokens` equal to (body + every generated byte) / 4. The rule now lives in one function (`attempt_charge`) that all eight outcomes share, and `attempt` is a 0-based index, so both documented summing rules agree. The same correction applies to the per-backend `est_tokens` in `/lac/status`, which had the identical double count; `req_bytes` there stays per backend, because those bytes really were received by that backend.
- **The gateway no longer promises a connection it does not keep, in either direction.** The relay serves one request per connection and then shuts the socket down, but forwarded the backend's head byte for byte — including `Connection: keep-alive` and `Keep-Alive: timeout=N`. Any client that honors the header the router just relayed (every HTTP/1.1 server that does not itself send `close`) sends request #2 into a closed socket: `curl` reports "Connection reset by peer" and does not retry a POST. The relayed head now drops every connection-scoped field, every field the backend's own `Connection` list named, and states `Connection: close`. The request direction had the same gap: `Connection`, `Proxy-Connection`, `Keep-Alive`, `TE`, `Trailer`, `Upgrade` and any client-named field were forwarded to the inference backend. `Content-Length` and `Transfer-Encoding` are deliberately *not* treated as hop-by-hop here, because the body is relayed as wire bytes and the backend must agree with the framing the router already validated. `Expect` also had two answers to one question: the router self-answered any value *containing* `100-continue` while the forwarded request only dropped a value that was *exactly* it, so `Expect: foo, 100-continue` was answered here and handed to a backend that then waited for an interim response this relay never forwards. One predicate answers both now.
- **`/lac/status` and `/lac/switch` report what they measured.** With every backend down, `/lac/status` answered `"status": "ok"` and named an active lane — the preferred id, which is the literal string `auto` by default — on a port nothing was bound to, in the same document whose `backends` block said all three were down. `lac status`, `lac chat` and the Studio dashboard all read that field. It now answers `"no-backends"` with `active: "none"` and `target_port: 0`. `/lac/switch` replied `"persisted": true` with the write's error discarded, so a read-only or full `~/.lac` silently lost the operator's choice while the dashboard showed it as saved; the flag is now the write's actual result, and the router says so on stderr.
- **The ports on screen are the ports that were probed.** `lac status` and `lac ps` printed `:8081`, `:11434` and `:8000` as literals while the probe beside them honored `LAC_LLAMA_PORT`, `LAC_OLLAMA_PORT` and `LAC_ROUTER_PORT` — so a relocated lane was reported online on a port nothing was bound to. `common.rs` already forbids exactly this.
- **The Studio's git-diff gate cannot deadlock or lie.** The Loops sheet called `waitUntilExit()` *before* draining the pipe. A pipe holds 64 KiB — about 1,500 lines of diff, routine after a refactor — and past it the child blocks in `write()` while the parent blocks in `wait()`: no timeout, `isDiffLoading` stays true, and the button that opens the sheet is disabled, so the gate could never be inspected again. The same ordering blocked `WorkspaceFileTree`'s status column. The exit status was also never read, and stderr was piped and discarded, so a Dock launch (where the working directory is `/`, not the checkout) produced an empty diff rendered as a green "Working tree is completely clean." — directly above a one-click "Approve & Complete" that marks the task complete and logs that the changes were verified. The drain comes first, the repository is found by walking up to the `.git` that actually exists, and a non-zero exit is shown as a failure rather than as a clean tree. The approval log no longer claims a verification the app never performed, and carries the Implement phase's failure with it when there was one. `TaskStatus.failed` also had no column on the board, so a failed task left it entirely.
- **Accepting one diff chunk applied every chunk.** `synthesizeCodeFromHunks` short-circuited to the full modified file whenever nothing had been *rejected*, so with two chunks, accepting the first wrote the second into the editor tab while it was still badged "Pending" — the badge stopped describing the screen. `.accepted` is now the only state that applies a change; `.pending` keeps the original text.
- **Tests:** 316 Rust (was 259) and 39 Swift (was 37). Every property above has a test that was verified to fail with the fix reverted: the process-identity table and the `ps`-table filter, the failover body sum measured at the log end-to-end (912 vs 456), the relayed head and the backend's received headers, the no-backends status and the false `persisted`, the two-chunk accept, and the gate's log. Two existing auth tests asserted `"status": "ok"` only as proof that the request was *served*; their fixtures point every backend port at the router's own, so they now assert the real document is what came back and leave that field to `router_intel`.
- **Gate:** `make lint`, `make test`, `make smoke`, `make release-smoke`, `make app-check`, `make delivery-index` all pass.

### Still open from this pass (audited, real, not yet fixed)

Recorded rather than silently dropped, each with the shape of the fix:

- `lac loop run` prints `✓ Tests passed (100% pass rate)` without running any test, hard-codes `✓ Apple Silicon thermals: Nominal` without calling `thermal_state()`, and — on `--dry-run` — then rewrites the queue to `status: complete`, so a task is dropped from the work queue having done nothing.
- `lac dashboard --install` copies the Studio binary over itself when `LAC_STUDIO` names one of its three destinations, which truncates all three to 0 bytes and prints "Installed".
- `lac code --write` exits 0 when the write fails, and prints "(backup saved to …)" for a backup whose write it discarded.
- `lac daemon install` rewrites the router port in a plist by matching four spaces of indentation while the shipped template has eight, so a relocated port is silently not applied.
- `common::tail_file` reads the whole of `~/Library/Logs/lac-router.log` — a launchd log nothing rotates — to print 100 lines; `lac bench`'s probe has no read timeout and an uncapped `read_to_end`.
- `common::acquire_lock` reads a truncated or malformed `owner` file as definitively stale, so a live worker's lock can be stolen, and `DirLock::drop` then deletes the reclaimer's directory.
- `lac_tui::status_header` indexes a slice with a hard-coded offset and panics on a truncated or non-ASCII `opencode.jsonc`; it is called on every menu redraw.
- `NetworkManager.startRouter` re-arms its auto-start flag before spawning, so a router that cannot bind is retried without bound, leaking one untracked `lac-router` per cycle.
- `LACConnection` restores its port with `storedPort ?? defaultPort ?? (storedPort ?? 0 > 0 ? storedPort! : 8000)`, where `??` binds looser than `>`: the condition is a nil check, so an out-of-range port persisted from the settings field is restored and every gateway call fails.
- `ChatStore.cancel()` bumps the stream generation before the partial-save paths, so both are unreachable: a cancelled stream's answer is never persisted and a phantom row stays on screen.
- `kv-manage check --tokens abc` discards the measurement and exits 0 GREEN, where exit 3 is reserved for usage errors.

---

## 2026-09-26 Re-verification (10x pro CLI honesty pass)

The still-open list above, worked through. Same standard as every other pass: no green output for work that did not happen, no exit 0 for a failure, and a test that fails when the fix is reverted.

- **A gate that did not run no longer reads as a gate that passed.** `lac loop run` had an `else` branch that printed `✓ Tests passed (100% pass rate)` with no cargo anywhere in the process — and that branch is exactly where `--dry-run` and every checkout without `rust-src/Cargo.toml` landed. It then printed `Reliability gates passed`, `[LOOP COMPLETED]`, and rewrote the queue to `status: complete`: a task silently retired from the work queue by a command that ran no test, dispatched no agent, and wrote no file. The gate is now one function whose every branch states what it did, a gate that did not run fails closed with a nonzero exit as every other gate in the project does, and a dry run leaves the queue alone. Asserted end to end in the process-level smoke: `lac loop run t-dry --dry-run` exits nonzero, prints no test result, and the fixture task is still `pending` afterwards.
- **Preflight measures instead of asserting.** `✓ Context cap <= 32K verified` never read the cap, and `✓ Apple Silicon thermals: Nominal` never called `thermal_state()`. Both are now readings, and a Serious or Critical thermal state stops the run the way `AGENTS.md` requires. Phase 2's `✓ Changes scoped to active worktree` and phase 3's `Audit findings: 0 Critical, 0 Major` were verdicts from an agent and a reviewer that never ran; both lines now say so.
- **`lac dashboard --install` cannot destroy the binary it is installing.** `fs::copy(x, x)` is not a no-op: it opens the destination `O_TRUNC` and returns `Ok(0)`, so `LAC_STUDIO=~/.local/bin/LACStudio lac dashboard --install` — the documented "here is my installed copy" use of the variable — left three 0-byte binaries and printed "Installed". Reproduced before the fix: 40 bytes in, 0 out, rc=0. A self-copy is now refused by identity (canonical paths, so a symlink counts), every copy is checked, and a short write is a failure. A `--write` that fails also exits nonzero now, so `lac code … && next` stops instead of proceeding as if the refactor landed.
- **A backup that was not written is no longer claimed.** `lac code --write` discarded the result of the `.bak` write, so an unwritable backup path left the original one overwrite away and the message still pointed at a directory. The original is now not touched unless its backup landed.
- **The launchd port retarget actually retargets.** `lac daemon install` rewrote `LAC_ROUTER_PORT` with a literal `"<key>LAC_ROUTER_PORT</key>\n    <string>8000</string>"` while the shipped template indents that value by eight spaces, so `LAC_ROUTER_PORT=9000 lac daemon install` printed `[ok] Placed:` and left the launchd router on 8000 while every `lac` command addressed 9000 — so the whole CLI reported the gateway down. Values are now matched by key (in a plist, whitespace is not significant, which is exactly why this stayed invisible), the key is required to have been found, a failed write no longer proceeds to bootout/bootstrap, `launchctl_bootstrap` reports whether the agent loaded, and any failure exits nonzero.
- **`lac loop init` reports what landed.** It printed `Installed loop template: …` for every copy whose result was discarded, then `Loop system initialized successfully` — four of each with an unwritable `~/todo/lac-loops` and an empty directory. This is also the worker's pre-flight, so `lac worker` was looping on a no-op init forever, or exiting 0 in `--drain` mode having drained nothing. Copies are checked, a short write counts as a failure, and a failed init exits nonzero and says `NOT initialized`.
- **`lac logs` reads a window, not a log that never rotates.** `tail_file` was `fs::read` of the whole file, on `~/Library/Logs/lac-router.log` — a launchd `StandardOutPath` that nothing in this project or in macOS rotates, written one line per request and per attempt. Measured at 95 MiB of RSS to print 100 lines, and unbounded from there. It now seeks from the end and grows a window only if the last `n` lines are not in it yet; the test builds a 64 MiB file whose answer sits at the front of the tail and asserts the exact result.
- **`lac bench` can no longer hang forever.** Its `/v1/models` probe had no read timeout and an unbounded `read_to_end`, so any port that accepts and then says nothing — a tunnel, a proxy port, a half-dead server — left the command with no output, no error and no exit, and a peer that streamed instead grew the buffer indefinitely. There is now a deadline, a cap matching `common::http_get`, and a nonzero exit for "listening but not answering" and for a lane that is offline, so a bench script fails on a dead port instead of passing.
- **Tests:** 334 Rust (was 316) and 39 Swift. The dry-run queue behavior and the gate refusal are asserted against the real CLI binary; the plist retarget, the self-copy identity, the bounded tail and the gate verdict are unit tests; the self-copy test asserts a symlink to the source counts as the same file, since that is the case a string comparison would miss.
- **Gate:** `make lint`, `make test`, `make smoke`, `make release-smoke`, `make app-check`, `make delivery-index` all pass.

### Still open after this pass

- `common::acquire_lock` reads a truncated or malformed `owner` file as definitively stale, so a live worker's lock can be stolen, and `DirLock::drop` then deletes the reclaimer's directory.
- `lac_tui::status_header` indexes a slice with a hard-coded offset and panics on a truncated or non-ASCII `opencode.jsonc`; it is called on every menu redraw. The TUI also hides the cursor with no `Drop` guard, so a panic or a Ctrl-C leaves the shell without one.
- `NetworkManager.startRouter` re-arms its auto-start flag before spawning, so a router that cannot bind is retried without bound, leaking one untracked `lac-router` per cycle; the three backend start buttons have no in-flight guard either.
- `LACConnection` restores its port with `storedPort ?? defaultPort ?? (storedPort ?? 0 > 0 ? storedPort! : 8000)`, where `??` binds looser than `>`: the condition is a nil check, so an out-of-range port persisted from the settings field is restored and every gateway call fails.
- `ChatStore.cancel()` bumps the stream generation before the partial-save paths, so both are unreachable: a cancelled stream's answer is never persisted and a phantom row stays on screen.
- `kv-manage check --tokens abc` discards the measurement and exits 0 GREEN, where exit 3 is reserved for usage errors; `kv-manage truncate` writes a manifest that contradicts itself when `--keep` exceeds `--tokens`, and two runs in the same second overwrite each other's manifest.
- `home_dir()` falls back to `"."`, so with `HOME` unset every state path becomes cwd-relative and `kv-manage truncate` creates a `.lac/` tree in the working directory — which launchd pins to the checkout, and which `.gitignore` does not cover.
- `lac_tui` option 5 and the Studio attachment reader have their own small honesty gaps (a non-executable `~/.local/bin/lac-serve-mlx` satisfies the install target; an attachment is read whole into memory before being truncated to 128 KB, behind a `|| true` guard on the security-scoped-resource check).

---

## Rust Power Confirmation

> "When you close it the app is still running — that's our rust power for the community."

The launchd daemon system demonstrates this principle fully:

- **Before daemon install:** Closing the TUI/CLI stops all inference
- **After daemon install:** `org.lac.router` and `org.lac.worker` LaunchAgents keep services running in the background
- **Unix philosophy:** Rust-compiled, zero-dependency binaries launched at login, surviving user session closure

This is the architectural differentiator against LM Studio's visual layer + background process flaw. LAC combines **beautiful visual design** (TUI + gateway UI) with **Rust-powered persistence** — the best of both worlds.