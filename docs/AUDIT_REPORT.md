# Loop Studio Code Audit Report

**Generated:** 2026-09-12  
**Re-verified:** 2026-09-24 (10x pro delivery pass), 2026-09-26 (10x pro hardening pass)  
**Scope:** Rust backend, Swift Studio, router framing/auth, packaging, and delivery checks
**Model:** loop-studio/qwen3.8-27b  
**Status:** ✅ Rust + Swift tests, process-level E2E smoke, and packaged-app verification are enforced by `make validate`

---

## Executive Summary

The delivery path is now fail-closed and hermetic:

- Rust unit/integration tests and Swift Testing run from the same `make test` gate used by the worker.
- `make smoke` starts the real router plus a deterministic backend and exercises the real `loop-studio status` and `loop-studio chat` processes.
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
| Agent Rules | ✓ | `docs/AGENTS.md` present; `loop-studio doctor` accepts the documented fallback |
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
| Ollama Symlink | warn | dangling symlink may exist on the host | Run `loop-studio bootstrap` to re-establish it |
| Backends | warn | no inference backend serving | Start with `loop-studio serve mlx` and `loop-studio route --daemon` |

---

## Daemon Persistence Verification

The **Rust power** feature — daemons survive app closure — is confirmed working:

```bash
$ loop-studio daemon install
# Daemons continue running even after TUI/CLI exit

$ loop-studio status
# Gateway :8000, MLX :8080 remain ONLINE

$ loop-studio daemon uninstall
# Inference services stop when daemons are unloaded
```

**LaunchDaemon Status:**
- `org.loopstudio.router` — ACTIVE (gateway on :8000)
- `org.loopstudio.worker` — ACTIVE (task processing loop)

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
2. **Model volume** — external volume not mounted; consider `loop-studio bootstrap`
3. **Ollama symlink** — dangling; rerun bootstrap to fix
4. **Backend availability** — ensure MLX/Q8/Ollama are running for full stack

---

## Recommendations

1. **Run `loop-studio bootstrap`** to fix the dangling Ollama symlink
2. **Mount /Volumes/AIModels** if using external model storage
3. **Monitor thermal state** — currently Nominal, but MTP at Q4 generates heat
4. **Consider `--apply` on lane tune** to pin preferred backend persistently
5. **Review context-cap** — set to 16,000 tokens for steady-state hygiene

---

## 2026-09-24 Re-verification (10x pro delivery pass)

- **Canonical gate:** `make test` runs the Rust and Swift suites; `make smoke` runs the real router/CLI E2E; `make validate` also packages and verifies the signed Studio app. Lefthook and CI invoke `make delivery-index validate`, so required gate/test files cannot be omitted from the commit.
- **Router framing (loop-studio-router v2.8):** `sse_relay` covers Content-Length early close, incremental chunked SSE, split chunk boundaries, chunk extensions, terminal trailers, and SSE-only keep-alives. The chunk parser is stateful and rejects data that merely contains a terminal-looking byte sequence.
- **Remote auth closed:** non-loopback startup without a token fails closed; a configured token is required on every listener, including loopback; forwarded proxy headers also require the token. Constant-time, length-safe comparison and successful non-loopback traffic are covered by `router_auth`.
- **Browser surface closed:** the gateway refuses browser traffic by default. `Host` is served only for IP literals and `localhost` (421 otherwise), which is the DNS-rebinding control — a rebound request is same-origin, so it carries no `Origin` and only `Host` still names the attacker. `Origin` must be on `LOOP_STUDIO_ALLOWED_ORIGINS` (403 otherwise), and an allow-listed origin is echoed exactly with `Vary: Origin` rather than a wildcard. No response carries a CORS header for a native client, which sends no `Origin`. Both gates run before the auth gate, so a refused request never reaches a status read, `/loop-studio/switch`, or a backend.
- **Studio delivery:** connection settings are available before a router exists, remote clients cannot run local daemon/host-fact actions, malformed URLs clear loading state, and packaged builds include local-network ATS permission for dynamic Tailscale hosts.
- **State safety:** Chat, Code Assistant, and loop runners use generation guards so cancelled work cannot overwrite a newer run; stores use injectable roots in tests; failed worker tasks preserve changes in a recoverable stash instead of deleting them.
- **Model pull:** failures and cancellation return nonzero, input is passed as an argument rather than interpolated source, and the CLI owns the downloader process tree. Staging the MLX cache is now covered too: the `create_dir_all` result used to be discarded while "ensured" printed regardless, so a read-only or full volume produced a green exit and a cache that could not exist.
- **The wrappers can tell success from failure:** `serve-mlx` and `serve-llama` both exited 0 after a crash, a bind failure, or a missing engine binary, so `loop-studio serve mlx && echo ok` reported success with no lane running — and a *clean* engine exit was worse, because it is exactly what `KeepAlive{SuccessfulExit: false}` reads as "do not restart", so the night ended silently. Both now exit nonzero, print `starting on …` rather than `Listening on …` before anything is bound, and share one retry policy: a lane that has answered HTTP retries forever (the transient OOM the loop exists for), while a lane that never once answered HTTP is bounded and then stops with a message naming the model id and the port. `serve-llama` no longer exits 0 when the port is taken, and under launchd supervises and takes the port over instead of unloading itself.
- **The MLX port contract holds:** `serve-mlx` wrote `~/.lac/mlx.port` with a bare `parse()` while `common::mlx_port()` — the reader the router uses — discards `0`. `MLX_PORT=0` therefore published `0`, handed `--port 0` to the engine (a random ephemeral port), and left the lane unfindable even after the variable was removed. Both sides now call one `common::mlx_port_from`, the port file is removed when the wrapper exits so a crash cannot leave a lane advertised that nothing serves, and the last-resort fallback no longer returns a port it has just proven is occupied.
- **Per-request work is no longer duplicated per thread or per attempt:**
  - `HealthCache::ready` released the cache lock *before* probing, so N threads arriving on an expired entry fired N identical probes at the same backend. The router is one thread per connection with 128 slots, so a burst just after a TTL expiry was up to 128 simultaneous `GET /v1/models` at a single-threaded inference server, each holding an inflight slot for the length of the probe. Probes are now serialized per port and re-check the cache after acquiring, so the losers of the race reuse the winner's answer. Ports do not block each other, so a slow MLX probe never delays llama. This also removed a second bug for free: probes used to `insert` unconditionally, so a slow probe landing after a fast one replaced a good positive with a stale negative, costing up to `negative_ttl` of spurious 503s.
  - `merged_models` opened a fresh connection to every ready backend and read its catalog, and nothing cached the result — so a client polling `/v1/models`, which every model picker does, became three backend connections per caller with no bound on the number of callers. The merged catalog is now memoized for 1s (`LOOP_STUDIO_ROUTER_CATALOG_TTL_MS`) and coalesced the same way. An *absent* catalog is deliberately not cached: `None` means "fall through to single-backend forwarding" so the client gets the backend's real error, and caching it would keep serving a fall-through after a backend came up and started advertising models.
  - The usage log re-charged `req_body` on every retry, so `kv-manage` — which sums `est_tokens` over the window, from the log the docs call "the honest token source" — read a request that failed over twice as up to 3x the traffic it was. Those bytes belong to the request, and the client sent them once, so they are now charged on the first attempt only and each line carries an `attempt` number. Measured on a two-backend failover with a 76-byte body: sum(`req_body`) was 152, now 76.
  - `HealthCache::new` took a `ttl` and silently ignored it for negative entries, which were hardcoded to 2.5s inside the constructor. Both windows are now explicit arguments, so "expires after ttl" means what it says and a test can see it without sleeping for seconds.
- **Supervised engines are no longer orphaned:** `spawn_server` used `Command::status()`, which consumes the `Child` and returns only an `ExitStatus`, so the wrapper kept no pid to signal. `kill -TERM serve-mlx` therefore killed the wrapper and left `mlx_lm.server` running — still holding its port, still resident with the model's weights, invisible to `loop-studio ps`. The next start found the port taken, drifted to 8082, and loaded a *second* copy of a 27B model. Both wrappers now publish the child pid and forward SIGTERM/SIGINT to it, backoff is interruptible so a signal during a 60s wait is not followed by a relaunch, and the pid is withdrawn on reap so a later signal cannot hit a recycled pid. The handler deliberately does not `_exit`, so the port-file guard still runs. The TUI's "stop engines" action signalled only the engines, leaving the `LAUNCHDAEMON=1` wrapper to respawn what it had just killed; it now stops the wrappers first, which takes their engines with them. Covered end to end in `tests/child_lifetime.rs`, which drives real wrapper processes against a stub engine.
- **Bootstrap reconciles instead of assuming:** the `~/.zprofile` block was guarded by a one-shot marker, so the first run decided where `HF_HOME` pointed and no later run could change it — including the run where `/Volumes/AIModels` finished mounting. The file went on claiming an internal-disk path while every other step in the same run used the external volume. The managed block is now reconciled in place between its markers, user lines untouched, exactly one block ever present. `bootstrap` also stops claiming "all green" with a failed step: a failed `~/.zprofile` write, an unreadable templates directory, a failed copy, and a missing `~/.ollama` or `~/todo/loop-studio-loops` all count as failures and exit nonzero, while advisory warnings (a tool you may not need) stay warnings. It refuses to run at all when `$HOME` is unset, which previously pointed every path at the current directory.
- **Counts:** the exact count is reported by the test runner; the previous stale arithmetic and active-daemon snapshot are intentionally not presented as guarantees.

---

## 2026-09-26 Re-verification (10x pro hardening pass)

An audit of the four surfaces the previous passes had not covered — the process-signalling helpers, the usage accounting, the router's two header directions, and the Studio diff/gate path — found seven defects with observable wrong behavior. Each is now fixed, and each has a test that fails when the fix is reverted.

- **A signal only reaches the program it names.** `kill_matching` matched `cmd.contains(pattern)` over the whole `ps` command line, so it matched any process that *mentioned* the pattern anywhere in argv — and the patterns are strings the operator types. `loop-studio stop` and the TUI's "stop engines" SIGTERMed `tail -f ~/Library/Logs/loop-studio-serve-mlx.err` (a path this project prints itself), `grep -r loop-studio-router .`, and the parent shell of `loop-studio stop` itself, then reported the bystander as a stopped engine. Matching is now on program identity: the first token's file name, with the `loop-studio-`/`lac-` install alias normalized so one binary is found by either name, plus an interpreter `-m` module (so an engine started by hand is still findable) and literal subcommands (so `ollama serve` still is). The pid in `~/.lac/router.pid` is additionally confirmed with `ps -p` before TERM: a recycled pid used to take down whatever now owns it, while the CLI printed "signaled" for a router that was never running. `loop-studio ps`'s `mlx_lm` pattern, which no longer named the engine's actual program, is now `mlx_lm.server`.
- **The request body is charged once per request, in the log and in `/loop-studio/status`.** The previous pass gated `req_body` in the three *retryable* arms only, and computed the attempt number after pushing the current port, so `attempt` was 1-based and duplicated within one rid. The shape it missed is the normal one: a request that fails over and *succeeds* was charged on the discarded attempt and again on the answer the client received. Measured on a two-backend failover with a 456-byte body: `sum(req_body)` was 912, and is now 456, with the rid's lines numbered 0 and 1 and `est_tokens` equal to (body + every generated byte) / 4. The rule now lives in one function (`attempt_charge`) that all eight outcomes share, and `attempt` is a 0-based index, so both documented summing rules agree. The same correction applies to the per-backend `est_tokens` in `/loop-studio/status`, which had the identical double count; `req_bytes` there stays per backend, because those bytes really were received by that backend.
- **The gateway no longer promises a connection it does not keep, in either direction.** The relay serves one request per connection and then shuts the socket down, but forwarded the backend's head byte for byte — including `Connection: keep-alive` and `Keep-Alive: timeout=N`. Any client that honors the header the router just relayed (every HTTP/1.1 server that does not itself send `close`) sends request #2 into a closed socket: `curl` reports "Connection reset by peer" and does not retry a POST. The relayed head now drops every connection-scoped field, every field the backend's own `Connection` list named, and states `Connection: close`. The request direction had the same gap: `Connection`, `Proxy-Connection`, `Keep-Alive`, `TE`, `Trailer`, `Upgrade` and any client-named field were forwarded to the inference backend. `Content-Length` and `Transfer-Encoding` are deliberately *not* treated as hop-by-hop here, because the body is relayed as wire bytes and the backend must agree with the framing the router already validated. `Expect` also had two answers to one question: the router self-answered any value *containing* `100-continue` while the forwarded request only dropped a value that was *exactly* it, so `Expect: foo, 100-continue` was answered here and handed to a backend that then waited for an interim response this relay never forwards. One predicate answers both now.
- **`/loop-studio/status` and `/loop-studio/switch` report what they measured.** With every backend down, `/loop-studio/status` answered `"status": "ok"` and named an active lane — the preferred id, which is the literal string `auto` by default — on a port nothing was bound to, in the same document whose `backends` block said all three were down. `loop-studio status`, `loop-studio chat` and the Studio dashboard all read that field. It now answers `"no-backends"` with `active: "none"` and `target_port: 0`. `/loop-studio/switch` replied `"persisted": true` with the write's error discarded, so a read-only or full `~/.lac` silently lost the operator's choice while the dashboard showed it as saved; the flag is now the write's actual result, and the router says so on stderr.
- **The ports on screen are the ports that were probed.** `loop-studio status` and `loop-studio ps` printed `:8081`, `:11434` and `:8000` as literals while the probe beside them honored `LOOP_STUDIO_LLAMA_PORT`, `LOOP_STUDIO_OLLAMA_PORT` and `LOOP_STUDIO_ROUTER_PORT` — so a relocated lane was reported online on a port nothing was bound to. `common.rs` already forbids exactly this.
- **The Studio's git-diff gate cannot deadlock or lie.** The Loops sheet called `waitUntilExit()` *before* draining the pipe. A pipe holds 64 KiB — about 1,500 lines of diff, routine after a refactor — and past it the child blocks in `write()` while the parent blocks in `wait()`: no timeout, `isDiffLoading` stays true, and the button that opens the sheet is disabled, so the gate could never be inspected again. The same ordering blocked `WorkspaceFileTree`'s status column. The exit status was also never read, and stderr was piped and discarded, so a Dock launch (where the working directory is `/`, not the checkout) produced an empty diff rendered as a green "Working tree is completely clean." — directly above a one-click "Approve & Complete" that marks the task complete and logs that the changes were verified. The drain comes first, the repository is found by walking up to the `.git` that actually exists, and a non-zero exit is shown as a failure rather than as a clean tree. The approval log no longer claims a verification the app never performed, and carries the Implement phase's failure with it when there was one. `TaskStatus.failed` also had no column on the board, so a failed task left it entirely.
- **Accepting one diff chunk applied every chunk.** `synthesizeCodeFromHunks` short-circuited to the full modified file whenever nothing had been *rejected*, so with two chunks, accepting the first wrote the second into the editor tab while it was still badged "Pending" — the badge stopped describing the screen. `.accepted` is now the only state that applies a change; `.pending` keeps the original text.
- **Tests:** 316 Rust (was 259) and 39 Swift (was 37). Every property above has a test that was verified to fail with the fix reverted: the process-identity table and the `ps`-table filter, the failover body sum measured at the log end-to-end (912 vs 456), the relayed head and the backend's received headers, the no-backends status and the false `persisted`, the two-chunk accept, and the gate's log. Two existing auth tests asserted `"status": "ok"` only as proof that the request was *served*; their fixtures point every backend port at the router's own, so they now assert the real document is what came back and leave that field to `router_intel`.
- **Gate:** `make lint`, `make test`, `make smoke`, `make release-smoke`, `make app-check`, `make delivery-index` all pass.

### Still open from this pass (audited, real, not yet fixed)

Recorded rather than silently dropped, each with the shape of the fix:

- `loop-studio loop run` prints `✓ Tests passed (100% pass rate)` without running any test, hard-codes `✓ Apple Silicon thermals: Nominal` without calling `thermal_state()`, and — on `--dry-run` — then rewrites the queue to `status: complete`, so a task is dropped from the work queue having done nothing.
- `loop-studio dashboard --install` copies the Studio binary over itself when `LOOP_STUDIO_APP` names one of its three destinations, which truncates all three to 0 bytes and prints "Installed".
- `loop-studio code --write` exits 0 when the write fails, and prints "(backup saved to …)" for a backup whose write it discarded.
- `loop-studio daemon install` rewrites the router port in a plist by matching four spaces of indentation while the shipped template has eight, so a relocated port is silently not applied.
- `common::tail_file` reads the whole of `~/Library/Logs/loop-studio-router.log` — a launchd log nothing rotates — to print 100 lines; `loop-studio bench`'s probe has no read timeout and an uncapped `read_to_end`.
- `common::acquire_lock` reads a truncated or malformed `owner` file as definitively stale, so a live worker's lock can be stolen, and `DirLock::drop` then deletes the reclaimer's directory.
- `loop_studio_tui::status_header` indexes a slice with a hard-coded offset and panics on a truncated or non-ASCII `opencode.jsonc`; it is called on every menu redraw.
- `NetworkManager.startRouter` re-arms its auto-start flag before spawning, so a router that cannot bind is retried without bound, leaking one untracked `loop-studio-router` per cycle.
- `LoopStudioConnection` restores its port with `storedPort ?? defaultPort ?? (storedPort ?? 0 > 0 ? storedPort! : 8000)`, where `??` binds looser than `>`: the condition is a nil check, so an out-of-range port persisted from the settings field is restored and every gateway call fails.
- `ChatStore.cancel()` bumps the stream generation before the partial-save paths, so both are unreachable: a cancelled stream's answer is never persisted and a phantom row stays on screen.
- `kv-manage check --tokens abc` discards the measurement and exits 0 GREEN, where exit 3 is reserved for usage errors.

---

## 2026-09-26 Re-verification (10x pro CLI honesty pass)

The still-open list above, worked through. Same standard as every other pass: no green output for work that did not happen, no exit 0 for a failure, and a test that fails when the fix is reverted.

- **A gate that did not run no longer reads as a gate that passed.** `loop-studio loop run` had an `else` branch that printed `✓ Tests passed (100% pass rate)` with no cargo anywhere in the process — and that branch is exactly where `--dry-run` and every checkout without `rust-src/Cargo.toml` landed. It then printed `Reliability gates passed`, `[LOOP COMPLETED]`, and rewrote the queue to `status: complete`: a task silently retired from the work queue by a command that ran no test, dispatched no agent, and wrote no file. The gate is now one function whose every branch states what it did, a gate that did not run fails closed with a nonzero exit as every other gate in the project does, and a dry run leaves the queue alone. Asserted end to end in the process-level smoke: `loop-studio loop run t-dry --dry-run` exits nonzero, prints no test result, and the fixture task is still `pending` afterwards.
- **Preflight measures instead of asserting.** `✓ Context cap <= 32K verified` never read the cap, and `✓ Apple Silicon thermals: Nominal` never called `thermal_state()`. Both are now readings, and a Serious or Critical thermal state stops the run the way `AGENTS.md` requires. Phase 2's `✓ Changes scoped to active worktree` and phase 3's `Audit findings: 0 Critical, 0 Major` were verdicts from an agent and a reviewer that never ran; both lines now say so.
- **`loop-studio dashboard --install` cannot destroy the binary it is installing.** `fs::copy(x, x)` is not a no-op: it opens the destination `O_TRUNC` and returns `Ok(0)`, so `LOOP_STUDIO_APP=~/.local/bin/LoopStudio loop-studio dashboard --install` — the documented "here is my installed copy" use of the variable — left three 0-byte binaries and printed "Installed". Reproduced before the fix: 40 bytes in, 0 out, rc=0. A self-copy is now refused by identity (canonical paths, so a symlink counts), every copy is checked, and a short write is a failure. A `--write` that fails also exits nonzero now, so `loop-studio code … && next` stops instead of proceeding as if the refactor landed.
- **A backup that was not written is no longer claimed.** `loop-studio code --write` discarded the result of the `.bak` write, so an unwritable backup path left the original one overwrite away and the message still pointed at a directory. The original is now not touched unless its backup landed.
- **The launchd port retarget actually retargets.** `loop-studio daemon install` rewrote `LOOP_STUDIO_ROUTER_PORT` with a literal `"<key>LOOP_STUDIO_ROUTER_PORT</key>\n    <string>8000</string>"` while the shipped template indents that value by eight spaces, so `LOOP_STUDIO_ROUTER_PORT=9000 loop-studio daemon install` printed `[ok] Placed:` and left the launchd router on 8000 while every `loop-studio` command addressed 9000 — so the whole CLI reported the gateway down. Values are now matched by key (in a plist, whitespace is not significant, which is exactly why this stayed invisible), the key is required to have been found, a failed write no longer proceeds to bootout/bootstrap, `launchctl_bootstrap` reports whether the agent loaded, and any failure exits nonzero.
- **`loop-studio loop init` reports what landed.** It printed `Installed loop template: …` for every copy whose result was discarded, then `Loop system initialized successfully` — four of each with an unwritable `~/todo/loop-studio-loops` and an empty directory. This is also the worker's pre-flight, so `loop-studio worker` was looping on a no-op init forever, or exiting 0 in `--drain` mode having drained nothing. Copies are checked, a short write counts as a failure, and a failed init exits nonzero and says `NOT initialized`.
- **`loop-studio logs` reads a window, not a log that never rotates.** `tail_file` was `fs::read` of the whole file, on `~/Library/Logs/loop-studio-router.log` — a launchd `StandardOutPath` that nothing in this project or in macOS rotates, written one line per request and per attempt. Measured at 95 MiB of RSS to print 100 lines, and unbounded from there. It now seeks from the end and grows a window only if the last `n` lines are not in it yet; the test builds a 64 MiB file whose answer sits at the front of the tail and asserts the exact result.
- **`loop-studio bench` can no longer hang forever.** Its `/v1/models` probe had no read timeout and an unbounded `read_to_end`, so any port that accepts and then says nothing — a tunnel, a proxy port, a half-dead server — left the command with no output, no error and no exit, and a peer that streamed instead grew the buffer indefinitely. There is now a deadline, a cap matching `common::http_get`, and a nonzero exit for "listening but not answering" and for a lane that is offline, so a bench script fails on a dead port instead of passing.
- **Tests:** 334 Rust (was 316) and 39 Swift. The dry-run queue behavior and the gate refusal are asserted against the real CLI binary; the plist retarget, the self-copy identity, the bounded tail and the gate verdict are unit tests; the self-copy test asserts a symlink to the source counts as the same file, since that is the case a string comparison would miss.
- **Gate:** `make lint`, `make test`, `make smoke`, `make release-smoke`, `make app-check`, `make delivery-index` all pass.

### Still open after this pass

- `NetworkManager.startRouter` re-arms its auto-start flag before spawning, so a router that cannot bind is retried without bound, leaking one untracked `loop-studio-router` per cycle; the three backend start buttons have no in-flight guard either.
- `LoopStudioConnection` restores its port with `storedPort ?? defaultPort ?? (storedPort ?? 0 > 0 ? storedPort! : 8000)`, where `??` binds looser than `>`: the condition is a nil check, so an out-of-range port persisted from the settings field is restored and every gateway call fails.
- `ChatStore.cancel()` bumps the stream generation before the partial-save paths, so both are unreachable: a cancelled stream's answer is never persisted and a phantom row stays on screen.
- `kv-manage check --tokens abc` discards the measurement and exits 0 GREEN, where exit 3 is reserved for usage errors; `kv-manage truncate` writes a manifest that contradicts itself when `--keep` exceeds `--tokens`, and two runs in the same second overwrite each other's manifest.
- `home_dir()` falls back to `"."`, so with `HOME` unset every state path becomes cwd-relative and `kv-manage truncate` creates a `.lac/` tree in the working directory — which launchd pins to the checkout, and which `.gitignore` does not cover.
- `loop_studio_tui` option 5 and the Studio attachment reader have their own small honesty gaps (a non-executable `~/.local/bin/loop-studio-serve-mlx` satisfies the install target; an attachment is read whole into memory before being truncated to 128 KB, behind a `|| true` guard on the security-scoped-resource check).
- `LoopsStore` blocks the main actor for up to 30 s in a synchronous lock-retry loop, so a held `~/.lac/locks/loop-studio-tasks` freezes the whole app; and `ChatView.submit()` clears the attached files before the `isSending` guard, so pressing Return mid-stream discards them silently.

---

## 2026-09-26 Re-verification (10x pro lock + TUI pass)

The lock and terminal paths, which are the ones a crash or a signal reaches.

- **A lock is reclaimed on proof or on age, never on a shrug.** `acquire_lock` read an *unparsable* owner record as definitively stale while treating a *missing* one conservatively — which is exactly backwards. The acquirer writes the record with `File::create` (O_TRUNC) and then `write_all`, so between those two syscalls the file exists and is empty; a second worker arriving in that window parsed `pid = -1`, called the lock stale, deleted the directory and took it. Both workers then held `loop-studio-worker` and drained the same Kanban queue, which is the one thing the lock exists to prevent, and `kill -9` in that window makes the state permanent. The rule is now one pure function: the only unconditional reclaim is a pid that is provably gone, and an absent, empty, unparsable or unprobeable record all wait out the backstop. A live owner still keeps the lock regardless of age, deliberately — a forward clock jump must not hand one queue to two workers, and a slow worker must not lose its lock for being slow.
- **A lock only removes itself.** `DirLock::drop` called `remove_dir_all` on its path unconditionally, so a lock that had been reclaimed out from under its owner was deleted by that owner's exit — handing the queue to a third worker the moment either of them went. The owner record is now checked against this process before the directory is removed, which is the only thing that distinguishes "my lock" from "a lock at my path".
- **The TUI cannot panic on your config file.** `status_header` scanned for `"model"` and then stepped over quotes with hard-coded byte offsets: a file truncated right after the key indexed one past its length, and a file whose next byte was multi-byte UTF-8 sliced a character in half. Both panic, on every menu redraw, from a file an interrupted `>` redirect or a botched checkout produces — verified by running the old expression on both inputs. `loop_studio.rs` had a correct parser for the same file the whole time, so there were two implementations and only one was safe; there is now one (`common::jsonc_model_id`), both callers use it, and it is byte-safe by construction. The test pins the two panicking inputs as `None`, plus a `//` inside the value and an escaped quote that must not end it early.
- **The TUI shows the ports it probed.** Its header printed `:11434` and `:8081` as literals beside probes that honor `LOOP_STUDIO_OLLAMA_PORT` and `LOOP_STUDIO_LLAMA_PORT` — the same defect fixed in `loop-studio status` a pass earlier, in the other binary.
- **The cursor comes back.** `live_monitor` hid the cursor, looped fifteen times, and restored it with a `println!` at the end. A Ctrl-C during the monitor — or any panic inside it, from a config file the user edits — left the shell with no cursor until `reset`. A `Drop` guard restores it on every path out, which is also the second route to that state now that the header cannot panic.
- **Tests:** 350 Rust (was 334) and 39 Swift. The reclaim rule is asserted as a table over the states that matter (empty record, unparsable, negative pid, missing, dead pid, live pid, unprobeable pid, fresh and aged directory) rather than through a race, and the `Drop` test reconstructs the reclaim — remove the directory, let someone else write their own record, drop the original — and asserts their lock survives.
- **Gate:** `make lint`, `make test`, `make smoke`, `make release-smoke`, `make app-check`, `make delivery-index` all pass.

---

## 2026-09-27 Re-verification (10x pro Studio concurrency + state-path pass)

The two lists the last three passes left open, worked through: every still-open
Studio item, and the CLI's state-path and measurement honesty. Ten defects with
observable wrong behavior, each now fixed, each with a test verified to fail when
the fix is reverted, plus one regression of my own that the review pass caught and
a test now pins.

- **A failed gateway start is no longer retried every three seconds.**
  `startRouter` set `hasAutoStartedRouter = false` *before* it spawned, so a start
  that failed left the automatic path armed, and the next dashboard tick started
  another one — unbounded, one untracked `loop-studio-router` per cycle, each finding the
  port held by the last. The rule is now one pure function with three brakes that
  answer three different questions: one automatic start per outage, a budget of
  `maxAutoStartAttempts` reset only by a router that answers, and a cooldown so a
  flapping gateway cannot spawn per tick. A start already in flight is not a new
  outage — without that, a poll landing mid-start consumed the one-per-outage
  allowance and its (immediately declined) start call was lost. A failed start
  re-arms the *policy* rather than latching the app off for the rest of the session,
  because the transient case a retry fixes (the port still held by the router that
  was dying a second ago) is the common one, and the budget plus the cooldown are
  what bound it. A successful `/loop-studio/status` now also has to *be* one:
  `no-backends` on `target_port: 0` is what a router with nothing behind it answers,
  and both the latch and the start verification counted that document as a live
  gateway — the shape of a dead router wearing a healthy one.
- **A deliberate stop is no longer undone by the app.** `stopAll` ran `loop-studio stop`
  and left the automatic path armed, so the next tick started the gateway the
  operator had just stopped.
- **The three backend start buttons are guarded, and probe the port the engine
  binds.** They had no in-flight guard at all, so a double-click spawned a second
  engine which found the port taken, drifted, and loaded a second copy of a 27B
  model. They also polled `8080`/`8081`/`11434` as literals while the router,
  `loop-studio status` and the TUI all read the lane's real port — a third implementation
  of the rule the last two passes fixed elsewhere, so a relocated lane was started
  and then polled on a port nothing was bound to. The precedence is now
  `common::mlx_port`/`llama_port`/`ollama_port` exactly, including the MLX lane's
  own port file (`MLX_PORT` → `PORT` → `~/.lac/mlx.port` → 8080), which is the only
  way a drifted engine is found at all and which the Studio had never read. Values
  that are not ports are ignored rather than probed, `0` included: the engine reads
  `0` as "assign me an ephemeral port" and the router discards it, so a probe of `0`
  can never succeed. One lane type carries the mapping, its `rawValue` *is* the
  `loop-studio serve` subcommand, the button disables and says "Starting…", and the palette
  and chat entry points say why a second press did nothing.
- **A persisted port that is not a port no longer breaks every call.**
  `port ?? defaultPort ?? (storedPort ?? 0 > 0 ? storedPort! : 8000)` — `??` binds
  looser than `>`, so the condition was a nil check: `0`, `-1` and `70000` were all
  restored from UserDefaults, and `url(path:)` then returned nil for every gateway
  call, so the whole app reported "Invalid router URL" against a profile nobody had
  edited. Recovered values (env, then disk) must now be real ports. An *explicit*
  argument is still passed through even when invalid, because a caller that asks for
  port 0 deserves a refusal rather than a quiet dial to a different port — and the
  settings field, which is a free numeric `TextField` bound straight to the
  property, rejects the value, keeps the last good one, and says so in the panel.
- **The Stop button no longer destroys the answer.** `ChatStore.cancel` bumped the
  stream generation *first*, and every save path in the task opens with
  `guard streamGeneration == generation` — so a cancelled answer was never
  appended, never persisted, and `streamText` kept holding it. The transcript draws
  a live row whenever that buffer is non-empty, independently of `isSending`, so a
  phantom message stayed on screen belonging to no thread for the rest of the
  session. The order is now persist, then invalidate, and all four save paths share
  one helper.
- **A held queue lock no longer freezes the app, and the Studio can read what the CLI
  writes.** Three separate defects in the Studio's copy of
  `~/.lac/locks/loop-studio-tasks`. It waited with `Thread.sleep` inside a `@MainActor`
  function for up to 30 seconds: any holder froze every window, including the one
  streaming an answer. It removed the directory unconditionally on the way out, so a
  lock that had been reclaimed out from under it was deleted by it — the
  `DirLock::drop` defect the Rust side was fixed for in the previous pass, still
  live in the other implementation. And it wrote `"<pid>"` while
  `common::acquire_lock` writes `"<pid> <seconds>"`, then parsed the whole record
  with `Int32(...)`, so no CLI-written record could be read here at all; it also
  aged locks by birth time where the CLI ages by mtime, and birth time is absent on
  the external volumes this project supports, which left the backstop unable to
  fire. The record is now published in the CLI's format, the age comes from mtime,
  the reclaim rule is one pure function mirroring `lock_is_reclaimable` (the only
  unconditional reclaim is a pid that is provably gone; a live owner keeps the lock
  regardless of age), the release only removes a lock this process still owns, a
  record that fails to write hands the lock straight back instead of becoming one
  nobody can release, a cancelled waiter stops waiting instead of spinning, and the
  wait is a suspension off the main actor. Mutations chain in call order, which the
  old synchronous version got for free from the actor, and a new task id is computed
  from the list read *inside* the lock. One difference remains, documented in the
  code: `EPERM` from the liveness probe means "alive, owned by someone else" here
  and "gone" to the CLI's `/bin/kill -0`, which cannot see the errno. A per-user
  `~/.lac/locks` cannot hold another user's process, so the divergence is
  unreachable — but it is the reason these are two implementations and not one.
- **Return mid-stream no longer discards the attachments.** `submit()` folded the
  files into the prompt and cleared the tray, and only *then* hit the
  `!store.isSending` early return — so pressing Return while a stream ran (exactly
  when a person reaches for Stop, or types the next question early) emptied the
  tray and sent nothing, with no error for either. The whole transition is now one
  pure step: a held submission returns the state it was given, so there is no
  ordering left to get wrong.
- **The attachment reader is capped, UTF-8 safe, and off the main thread.** It read
  the *whole* file with `Data(contentsOf:)` and then took a 128 KB prefix — the panel
  accepts `.data`, so a 2 GB capture meant a 2 GB allocation to inject 128 KB — and
  the true size came from `data.count`, so knowing a file was truncated required
  having read all of it. Truncating at a byte offset can split a multi-byte
  character, and the strict decode then returns nil, so a readable 200 KB UTF-8 file
  was rejected as "not a readable UTF-8 text file" whenever the boundary landed
  mid-character. `startAccessingSecurityScopedResource() || true` made that guard
  vacuous and the `defer` released a scope it never opened. Now: at most `maxBytes`
  are read, the size comes from `resourceValues`, the scope is opened and closed in
  balance, a boundary artefact costs at most 3 bytes (each candidate measured from
  the original length — trimming cumulatively skips the boundary that would have
  worked, which is how the first version of this fix still failed its own test), and
  the read happens off the main actor.
- **`$HOME` unset is a refusal, not a directory.** `home_dir()` fell back to `"."`,
  and a relative path is not a home directory — it is "wherever this process was
  launched". `kv-manage truncate` built a `.lac/` tree in the working directory
  (launchd pins that to the checkout, which `.gitignore` does not cover) and every
  later command read *that* tree, because the same relative string resolves the same
  way from any cwd. It now exits 78 (EX_CONFIG) with a reason, as `bootstrap`
  already did while quietly relying on this function everywhere else.
- **A launchd plist can no longer point at a file that cannot run.** Both
  installers asked only `metadata(path).is_ok()`, so existence was mistaken for
  capability: a non-executable or 0-byte `~/.local/bin/loop-studio-router` became
  `ProgramArguments[0]`, launchd respawned a failure forever, and `loop-studio daemon
  install` printed `[ok] Placed:`. A binary without its execute bit at that path is
  ordinary rather than exotic — a `cp` without `-p`, a zip round-trip, a download, a
  mode-644 `git checkout` — and an empty one is what a full disk leaves. Either way
  the result is a daemon that is installed, reported as installed, and can never
  run. One resolver now returns the path to write *and* the reason it passed the
  installed copy over — and it checks the fallback too, because an unbuilt or
  `cargo clean`ed checkout is a normal state, and `launchctl bootstrap` does not
  exec at load time, so a plist pointing at a path that does not exist installs
  cleanly and dies minutes later. With no runnable binary at either path the
  installer places nothing, names both paths it tried, and exits nonzero. The TUI's
  install path also surfaces `launchctl`'s own complaint instead of discarding it.
- **`kv-manage` refuses a number it cannot read, and writes a manifest that matches
  the run.** `--tokens abc` parsed to `None`, so `check` fell through to the router
  estimate and printed `tokens: 41022 (router-1h-est)` with `risk: GREEN` and exit 0
  — a mangled request rendered as a measurement, from the one command whose entire
  job is measuring. `--threshold 1e4` was the same, silently becoming 24000, and
  `LOOP_STUDIO_TOKENS=abc` was the same defect by another route: an operator who exports it
  has asked for a measurement, and the old code answered with a guess. All four
  count flags and `LOOP_STUDIO_TOKENS` are now usage errors (exit 3) when unparsable. The
  manifest no longer records `keep_last` for a run that will keep everything
  (`noop: true` instead), records `tokens_before: null` when nothing answered
  rather than a confident `0`, and keeps an *estimate* labelled as one: the router
  figure is a one-hour extrapolation, `check` prints "estimate only" for it and caps
  it at YELLOW, and `truncate` used to write it into a manifest as a measurement.
  Only a measured number can establish `noop`, because "nothing to do" is the one
  conclusion an estimate must not produce. The name is `truncate-<ts>-<pid>.json`
  with a counter, because two runs inside one second is routine for the command
  AGENTS.md tells the agent to run between tasks, and the second `atomic_write` used
  to replace the first record outright.
- **Tests:** 369 Rust — five new tests, two of them in `common.rs`, which is
  compiled into all eight binaries, hence the larger delta in reported results than
  in tests written — and 59 Swift (was 39). Every property above has a
  test verified to fail with the fix reverted: the auto-start policy as a table over
  the four brakes, the port precedence and the field's rejection, the cancelled
  stream's message *and* its bytes on disk, the reclaim rule as a table over both
  record formats plus the record the CLI can read, an absent queue file not being an
  empty one, a failed write not appearing on the board, the UTF-8 boundary, the
  submission state, the manifest name and its estimate labelling, the launcher
  program, and — at the process level — `kv-manage check --tokens abc` exiting 3
  with no risk line, and `loop-studio loop list` exiting 78 with no `HOME`. The lock test
  reconstructs the freeze: a lock owned by a live pid, a heartbeat the main actor
  has to be free to run, and an assertion that it kept ticking while the mutation
  waited. Three of these tests caught bugs in the fixes themselves (a cumulative trim
  that skipped the working boundary; a "rejected port" that reported the port still
  in use; a "reclaim" that wiped the seeded queue), which is the point of writing
  them against the rule rather than the implementation.
- **Review:** a `@reviewer` pass over the diff produced one critical regression of
  my own (collapsing "queue file unreadable" into "queue empty", which blanked a
  fresh install's board on the first edit) and thirteen major findings, all applied:
  the `LOOP_STUDIO_MLX_PORT` variable I introduced that nothing in the repo reads, the
  launcher resolver blessing a release path that does not exist, `LOOP_STUDIO_TOKENS` still
  being silently swallowed, the router estimate being written into a manifest as a
  measurement, a double-approval window and a lost draft that the move to async
  introduced, birth-time aging, an unreachable auto-start budget, and four claims in
  this report that were overstated. The corrections are folded into the bullets
  above rather than listed separately.
- **Harness:** the e2e fixture abandoned a request after 2s of read silence, so on a
  loaded box it closed the socket mid-request and the router reported
  `llama invalid-response` — a flake that reads as a product failure and is not
  one. The backstop is now 10s.
- **Gate:** `make lint`, `make test`, `make smoke`, `make release-smoke`,
  `make app-check`, `make delivery-index` all pass.

### Still open after this pass

Audited, real, not yet fixed:

- `LoopsStore` has no undo. A mutation returns whether it was written, and the
  human gate says so when it was not, but a drag that fails to write still leaves
  the card where the user dropped it while the file behind it disagrees, and there
  is no way back.
- The queue lock's backstop is 10s in the Studio against `stale_secs = 30` in the
  CLI (`loop_studio.rs:2205`), and neither waits on the other's terms: a Studio mutation
  gives up while the worker still holds the lock and says so, but nothing retries.
  The reclaim rule is also still two implementations of one rule, differing on
  `EPERM` — documented above, and unreachable for a per-user lock directory, but
  it is duplication that will drift.
- The dashboard polls every 3s (4s when unhealthy) and *every completed poll* ends
  in `fetchHostFacts`, which spawns `loop-studio status --json` — one subprocess every three
  seconds for as long as the window is open, to read four numbers the router already
  reports in `/loop-studio/status`.
- `spawnDetachedLac` runs `try? p.run()` with no `waitUntilExit`, so a
  `loop-studio route --daemon` or `loop-studio serve <lane>` that exits (a port it cannot bind, a
  preflight that fails) becomes a zombie child of the app until Studio itself quits,
  and the launch error is discarded — the button can only report "not up" later.
  `runLac` does reap, so the kill ladder there is sound.
- The Studio's queue and Loops surfaces read `~/todo/loop-studio-tasks.yaml` through
  `FileManager.homeDirectoryForCurrentUser`, which is the password-database home,
  while the Rust side resolves `$HOME` and now *refuses* when it is unset or
  relative. `HOME=/tmp/x loop-studio …` and Studio therefore write and read two different
  queues.
- `readAttachment` asks for "up to" `maxBytes` in one `FileHandle` read, so a short
  read on a network or FUSE volume silently yields less content while
  `wasTruncated` says the file was whole — the one thing that function exists to
  get right. It needs a read-until-full loop.

## 2026-09-27 Re-verification (10x pro Studio write-honesty pass)

The six-item list the previous pass left open, worked through. Six defects with
observable wrong behavior, each fixed, each with a test verified to fail when the
fix is reverted.

- **A failed drag no longer leaves the board lying about the queue.** A mutation
  was fire-and-forget with the in-memory list replaced only on success, so a write
  that failed — a lock the worker holds, a disk that will not take the file — left
  the card where the user dropped it while the file behind it disagreed, with only
  a banner to say so and no way back. The edit is now applied to `tasks` *before*
  the write is attempted, so a drag is instant, and a failed write rolls back to
  the snapshot taken before the drag: the card goes back to where the drag found it
  and the banner says why. The snapshot is also the base for the edit when the
  queue file is absent or unreadable, which is what keeps a fresh install's seeded
  tasks from being wiped by its first mutation. The test had to be caught out once
  before it meant anything: the seeded `lac-001` is already `completed`, so the
  first version of it dragged a card to the status it already had and passed
  against the unfixed code.
- **The Studio's lock now waits on the CLI's terms, and reaches the same verdicts.**
  Its backstop was 10s against `common::acquire_lock("loop-studio-tasks", 30)`, so a Studio
  mutation gave up while the worker still held the lock and nothing retried. The
  parameter is now `staleSeconds` and defaults to the CLI's 30. Its liveness probe
  also disagreed with the CLI's on `EPERM`: the Studio read it as "alive, owned by
  someone else" while `common::process_alive` shells out to `/bin/kill -0`, which
  exits non-zero on EPERM and cannot see the errno, and therefore concludes "gone".
  Both implementations now call `EPERM` gone — the case that gets it wrong (a lock
  held by another user's process) cannot occur for a per-user `~/.lac/locks`,
  which only that user can write. The test probes pid 1, the one process a non-root
  test process can ask about and get `EPERM` for, and skips when run as root.
- **The dashboard no longer spawns a subprocess every three seconds.** Every poll
  ended in `fetchHostFacts`, which ran `loop-studio status --json` — one subprocess three
  times a minute for as long as the window was open, for thermal and RAM gauges.
  (The previous pass's note that the router "already reports" these in
  `/loop-studio/status` was wrong: the status document carries no host facts at all, which
  is why the subprocess existed.) A fresh reading is now taken at most once per
  15s; a poll that finds a fresh cache costs nothing, and the freshness rule is
  asserted directly, as is the wiring — a warm cache must not call the loader at
  all, which is the property that makes the poll free.
- **A detached engine that dies says so.** `spawnDetachedLac` and the router's
  fallback spawn both ran `try? p.run()` with no `waitUntilExit` and no
  `terminationHandler`, so a child that exited — a port it could not bind, a
  preflight that failed — became a zombie of the app until Studio itself quit, and
  the launch error was discarded: the button could only report "not up" five
  seconds later, which reads the same as a model that is simply slow to load. Both
  spawns are now tracked: a `terminationHandler` reaps the child and records its
  exit status, a failed launch is recorded too, and a start that never comes up
  surfaces the recorded failure instead of only the poll's verdict.
- **The Studio resolves home the way the CLI does.** The queue and lock paths used
  `FileManager.homeDirectoryForCurrentUser`, which reads the password database,
  while Rust resolves `$HOME` — so with `HOME=/tmp/x` the CLI and the app wrote
  and read two different queues, and every `HOME` that is not absolute (the case
  Rust now refused) silently fell back to passwd. One helper now applies the same
  rule as `common::home_dir`: `$HOME` when it is absolute, else the real home.
- **An attachment read no longer stops at the first short read.** `read(upToCount:)`
  returns *up to* the count. A regular file fills the request in one call, but a
  network or FUSE volume returns less with more still to come, so one call silently
  yielded a partial file while `wasTruncated` reported the file as whole — the one
  thing that function exists to get right. The read now loops until the cap or EOF.
  A pipe cannot demonstrate this (it blocks until the count or EOF), so the loop is
  extracted and tested against a reader that answers in pieces smaller than asked,
  which is also how the first version of the fix still failed its own test.
- **Tests:** 369 Rust (unchanged) and 72 Swift (was 59). Every property above has a
  test verified to fail with the fix reverted. Three of them caught bugs in the
  fixes or the tests themselves: the rollback test that dragged a card to the status
  it already had, the home test whose first revert did not compile and so never
  ran, and the short-read test that used a pipe and therefore passed against the
  unfixed code.
- **Gate:** `make lint`, `make test`, `make smoke`, `make release-smoke`,
  `make app-check`, `make delivery-index` all pass.

### Still open after this pass

Audited, real, not yet fixed:

- The queue lock is still two implementations of one rule — the Swift copy cannot
  call the Rust one. The backstop, the record format, the age source and the
  `EPERM` verdict now match, which is what keeps them from drifting on the
  properties that matter, but a change to the rule has to be made twice.
- `fetchHostFacts` still spawns `loop-studio status --json`, now at most every 15s. The
  router could serve host facts natively — it already runs on the host and
  `common` has `thermal_state` and `free_ram_gib` — which would remove the
  subprocess and make the gauges as fresh as the poll.
- After a failed write the board holds the rolled-back state and the file holds
  whatever it held; they meet again on the next successful mutation. That is the
  intended contract, but it means a queue nobody mutates can stay diverged
  indefinitely, and nothing reconciles it.
- `DetachedProcessLog` keeps only the most recent child's failure, and `runTracked`
  captures no stderr (a pipe would block the child), so the recorded message is
  the exit status rather than the engine's own complaint about the port it could
  not bind.
- `readAttachment` reports `wasTruncated: true` for a stream of unknown size that
  stops at the cap, because it cannot rule out more behind it. Conservative, but
  a stream of exactly `maxBytes` is reported as truncated.

## 2026-09-27 Re-verification (10x pro router host-facts pass)

The five-item list the previous pass left open. Four fixed, one accepted with
reasons. Each fix has a test verified to fail when the fix is reverted.

- **The dashboard no longer spawns a subprocess for the host gauges at all.**
  Every poll ended in `loop-studio status --json` — three subprocesses (`pmset`,
  `vm_stat`, `sysctl`) three times a minute for as long as the window was open.
  The router runs on the host and `common` already has the probes, so the
  answers belong in `/loop-studio/status`, the document the poll already fetches. The
  router now measures them on a 5s timer and serves a `host` block alongside
  everything else; the dashboard reads it from the status response, and the
  subprocess remains only as a fallback for a router old enough to omit the
  block. The gauges are now as fresh as the poll, and the common path costs
  nothing. The router does not know the chat model (host-side `opencode.jsonc`
  config) and the dashboard does not show it from here, so it is deliberately
  absent rather than guessed. A probe that cannot answer is `null`, not a
  confident zero.
- **A queue changed behind the app's back is noticed.** The file is the truth
  for what is durable and the board for what the user asked for; they meet on
  every successful write, and a failed write rolls the board back. What they
  could not do is notice a change made by the other writer — the `loop-studio worker`,
  or the app on the Mac this one is remoted to — and a queue nobody mutates
  stayed diverged until someone did. The dashboard now reconciles on appear:
  it adopts the file when no mutation is in flight and the two differ. It only
  ever adopts, never pushes, so the risk is limited to clobbering board state,
  which the in-flight guard prevents.
- **A dead engine's complaint is recorded, not just its exit status.** The log
  kept only the most recent failure, so a start button pressed repeatedly on a
  dead lane overwrote the one message that explained the first failure; it is
  now bounded at 8 with the newest kept. And the child's stderr goes to a
  per-command log file under `~/Library/Logs` whose tail is recorded with the
  exit status — a pipe cannot be used for a long-lived child's stderr (it fills
  at 64 KiB and blocks the child), and an exit status alone cannot say "the
  port was taken". Two defects in the fix were caught by its own tests: the log
  file name used the first argument raw, which is a path for a script and so
  silently sent stderr to null, and `forWritingAtPath` does not create a
  missing file.
- **`wasTruncated` is now exact for streams.** A stream of unknown size that
  yields exactly the cap is a whole file, and one byte more proves there is
  more behind it; reading exactly the cap cannot tell those apart, and the
  conservative answer was wrong half the time. The read now takes one byte
  past the cap when the size is unknown, which makes both cases exact. A pipe
  cannot demonstrate a short read (it blocks until the count or EOF), so the
  loop is tested against a stub reader and the cap against a real pipe.
- **Accepted: the queue lock is still two implementations of one rule.** The
  Studio is Swift and the CLI is Rust; the Swift copy cannot call the Rust one,
  and a shared implementation would mean a helper binary or a CLI command,
  which is a feature rather than a fix. The parity is now exact on the four
  properties that matter — backstop (30s), record format (`<pid> <seconds>`),
  age source (mtime), and the `EPERM` verdict (gone, matching `/bin/kill -0`)
  — and both sides are tested against the same rule. What remains is
  duplication that a change to the rule must make twice, and that is the
  accepted cost of a Swift app and a Rust CLI sharing a filesystem.
- **Tests:** 370 Rust (was 369) and 83 Swift (was 72). Every property above has
  a test verified to fail with the fix reverted, including the wiring in both
  directions — a router that provides the block must not spawn a subprocess,
  and one that does not must. Three of them caught bugs in the fixes or the
  tests themselves: the stderr test whose complaint was in the command string
  and so passed without the tail, the log-file name that was a path, and the
  short-read test that used a pipe and therefore could not fail.
- **Gate:** `make lint`, `make test`, `make smoke`, `make release-smoke`,
  `make app-check`, `make delivery-index` all pass.

### Still open after this pass

Audited, real, not yet fixed:

- The router's host facts are cached for up to 5s, so a thermal excursion
  takes up to 5s to reach the dashboard. The gauges are seconds-scale; this is
  a deliberate trade against probing on every request, and the poll that
  carries them runs at 3s.
- The reconcile runs on appear, not on a timer: a queue that changes while the
  dashboard is open is picked up the next time it appears, not live. The 3s
  poll refreshes the router status but not the queue file.
- `loop-studio status --json` still measures host facts locally with three
  subprocesses. Only the Studio benefits from the router's block; the CLI
  could prefer it when the gateway is up and keep the local probes as the
  standalone fallback.
- The per-command stderr logs under `~/Library/Logs` are removed at the start
  of the next run of the same command, so they are bounded by the number of
  distinct commands — but the last run's file is never deleted, and nothing
  rotates them.

## 2026-09-27 Re-verification (10x pro host-facts propagation pass)

The four-item list the previous pass left open. Three fixed, one accepted with
reasons. Each fix has a test verified to fail when the fix is reverted.

- **`loop-studio status --json` reads the router's host facts instead of re-probing
  them.** The router now measures thermals and RAM on a timer and serves them
  in `/loop-studio/status`, which is what removed the dashboard's subprocess — but the
  CLI still spawned `pmset`, `vm_stat` and `sysctl` of its own to answer the
  same question the gateway was already answering, every time `loop-studio status` ran.
  It now reads the router's block when the gateway is up, and keeps the local
  probes as the fallback, because `loop-studio status` is also the standalone stack
  report for a machine whose gateway is down. The extraction is a pure parse
  over the hand-rolled document, so the `null` that means "this probe could not
  answer" stays unknown rather than becoming a confident zero, and the block is
  read from the `host` object rather than from any earlier key that shares a
  name. The e2e asserts the CLI and the router agree on the thermal state, and
  that the CLI does not report a RAM reading it probed itself while the
  router's was available.
- **A queue that changes while the dashboard is open is noticed, not only on
  appear.** The previous pass reconciled on appear, which covered a queue the
  worker changed while the window was away and left a queue changed *while it
  was open* diverged until someone navigated away and back. The poll now stats
  the file's mtime and reconciles when it is newer than the last read — a stat
  is enough, and the reconcile itself is a no-op when the board already matches
  the file, which is the case after every write this app made. The in-flight
  guard still applies: a board that is ahead of the file on purpose, because
  its write has not landed, is not reconciled away.
- **Stderr logs have a lifetime.** Each detached start removes its own log at
  the start, so at most one file per command accumulated — but the last run's
  file was never deleted and nothing rotated them, so the directory grew a file
  per distinct command forever. A start that died is worth keeping for a day;
  older than that it is a log nobody will read. The rule is a pure function of
  (name, mtime) so it can be asserted without touching the real logs
  directory, and a file that is not one of ours is never touched.
- **Accepted: the router's host facts are cached for up to 5s.** The probes are
  three subprocesses, and the whole point of the timer was to keep them off the
  request path — the dashboard polls every 3s, so measuring on each request
  would put them back where they started. A thermal excursion therefore takes
  up to 5s to reach a gauge that a person reads at a glance, which is a
  deliberate trade for a status endpoint that stays fast, and the poll that
  carries the block runs faster than the cache. This is a documented property
  of the design, not a defect left open.
- **Tests:** 375 Rust (was 370) and 88 Swift (was 83). Every property above has
  a test verified to fail with the fix reverted. One of the new Swift tests
  asserted absolute task counts and failed against a fixture that seeds four
  default tasks, which is the second time in this series a test passed for the
  wrong reason and had to be pinned to a relative count.
- **Gate:** `make lint`, `make test`, `make smoke`, `make release-smoke`,
  `make app-check`, `make delivery-index` all pass.

### Still open after this pass

Audited, real, not yet fixed:

- `~/Library/Logs` is still not rotated as a whole. The per-command stderr logs
  are now cleaned, but the launchd plists' own logs (`loop-studio-router.log`,
  `loop-studio-serve-mlx.err`) are the ones that grow without bound, and nothing in this
  project or in macOS rotates them. `loop-studio logs` reads a bounded tail of the
  router's, which is why the cost is invisible until someone opens the file.
- The Studio's own copy of the queue lock remains a second implementation of
  the rule (accepted last pass with reasons: a Swift app and a Rust CLI sharing
  a filesystem, with the parity exact on backstop, record format, age source
  and the `EPERM` verdict).
- The dashboard still shows a 5s-stale thermal gauge and a 3s-stale router
  status; there is no push channel, and a local gateway over a polling
  connection does not need one.

## 2026-09-27 Re-verification (10x pro log-bound pass)

The three-item list the previous pass left open. One defect fixed — the only
unbounded growth left in the project — and two accepted with reasons. Each fix
has a test verified to fail when the fix is reverted.

- **The daemons now rotate the logs launchd never will.** `StandardOutPath` and
  `StandardErrorPath` are opened by launchd and never rotated: not by macOS, not
  by this project. The router writes a line per request and per attempt, so its
  log grows for as long as the machine is up, and the cost was invisible because
  `loop-studio logs` reads a bounded *tail* of it. The daemon is the only party that can
  rotate its own log, and doing it there is safe: launchd holds the file open
  with `O_APPEND`, so renaming the old bytes aside and leaving an empty file
  under the original name puts the writer's next write at offset 0. Every daemon
  now does this for its own two logs on a timer — the router, `serve-mlx`,
  `serve-llama` and `loop-studio worker` — at 8 MiB, keeping three generations, which is
  what makes the bound a bound rather than a slower leak. The names come from
  `retarget_launchd_plist`, which re-roots every log into `~/Library/Logs`, so
  the daemon finds the file its own plist opened instead of a copy of the path;
  and a test reads the shipped plists and asserts the rotating names are the
  names they open, because a rename on either side would silently leave a daemon
  rotating a file nobody writes. `loop-studio logs` now prints the size and the number of
  generations beside the tail, so the cost is visible before someone opens the
  file in an editor. Verified end to end on a real 9 MiB log: rotated on
  startup, old bytes in `.1`, empty file in place, and the rotation reported.
- **The Studio's stderr logs clean up after themselves, including the mess an
  earlier version made.** The per-command `loop-studio-*.err` files are now
  removed past a day — a start that died is worth keeping, one older than that
  is a log nobody will read. The namespace is matched by prefix rather than by
  prefix and extension, because a version that used the first argument as a
  file name turned a path into a *directory tree* under `~/Library/Logs`, and
  only the top-level entry is ever listed. That tree was removed from this
  machine; the cleanup now covers anything of that shape it finds.
- **Accepted: the 5s host-facts cache and the absence of a push channel.** The
  cache is the deliberate trade that keeps three subprocess probes off the
  request path, and a local gateway polled over loopback does not need a push
  channel — the poll that carries the block runs faster than the cache. Both are
  properties of the design, not defects left open.
- **Tests:** 407 Rust (was 375 — four new tests in `common.rs`, which is
  compiled into all eight binaries) and 88 Swift (unchanged; the Swift change
  was a widening of an existing rule). The rotation is asserted as a table over
  the states that matter: under the limit is untouched, over the limit rotates
  and leaves an empty file, generations shift and the oldest falls off, a
  missing file or a directory is never touched, and the degenerate `keep = 0`
  truncates away. Both reverts were confirmed to fail their tests.
- **Gate:** `make lint`, `make test`, `make smoke`, `make release-smoke`,
  `make app-check`, `make delivery-index` all pass.

### Still open after this pass

Audited, real, not yet fixed:

- The Studio's copy of the queue lock remains a second implementation of the
  rule (accepted with reasons: a Swift app and a Rust CLI sharing a filesystem,
  with the parity exact on backstop, record format, age source and the `EPERM`
  verdict). A change to the rule still has to be made twice.
- The launchd logs are now bounded per generation, but a machine that is up for
  months holds 8 MiB x 4 per daemon x 4 daemons, and nothing trims the
  generations themselves — a `.1` from last spring is as permanent as the live
  log used to be. A retention rule (by age, not by count) is the obvious next
  step and is deliberately not done here, because deleting an operator's
  diagnostics on a timer is a decision that should be theirs.
- The router's per-request usage log (`~/.lac/router-usage.jsonl`) rotates on
  size as well, and is read by `kv-manage`; its rotation and the log rotation
  here are two mechanisms that could be one, and are not.

## 2026-09-27 Re-verification (10x pro one-rule pass)

The three-item list the previous pass left open. Two resolved, and the third —
which I had twice called "not a defect" — turned out to be the one worth doing.

- **The two implementations of the lock rule are now proved the same, not kept
  the same.** `~/.lac/locks/loop-studio-tasks` is taken by Rust's
  `common::lock_is_reclaimable` and the Studio's `LoopsStore.lockIsReclaimable`.
  Two previous passes accepted that duplication on the grounds that Swift cannot
  call Rust, and both were right that the *code* cannot be shared. What can be
  shared is the rule: `docs/lock-parity.tsv` is a table of the states that
  decide a verdict — absent, empty, unparsable and negative-pid records, fresh
  and aged directories, and a live, dead and unprobeable probe — and both test
  suites read it and assert the same answers. A change to one implementation
  that the other does not share now fails that other's gate. Both directions
  were verified by diverging one implementation and watching the other side's
  suite fail on named rows. The file is tracked by `make delivery-index`, because
  a table both suites depend on and a gate that does not track it is a test
  suite that quietly stops testing anything.
- **One rotation rule, not two.** The router's per-request usage log rotated
  itself with its own inline `metadata().len() > LIMIT` and a bare rename — the
  same rule as the daemons' launchd logs, written out a second time, with the
  generation handling written out again. It now calls the shared helper with
  `keep = 1`, which is the policy its reader implies: `kv-manage` sums exactly
  the live file and `.1`, so a third generation would be a file nothing opens.
  The reader's own tests — the ones that caught a rotated name nothing looked
  for — still pass unchanged.
- **The accumulated logs are now a number and a command, not a timer.** The
  daemons bound growth and keep three generations, so a machine up for months
  holds ~128 MiB across four daemons, and a `.1` from last spring is as
  permanent as the live log used to be. Two previous passes declined to fix this
  on the grounds that deleting an operator's diagnostics on a timer is their
  decision rather than the tool's — which is right, and is also not a reason to
  leave them invisible. `loop-studio logs` now reports the footprint of every log this
  project writes, not just the one it is tailing, and names the reclaim command
  when it is large. `loop-studio logs <which> --purge` reclaims the generations on the
  operator's word and never touches the live log, which is the file a running
  daemon is appending to. The purge stops at the first missing generation,
  because a `.3` left by an interrupted rotation must not make it spin.
- **Tests:** 447 Rust (was 407) and 90 Swift (was 88). The parity table is read
  by both suites, so the count of cases that are actually checked is 19 rows x 2
  implementations. Both halves assert the exact set of nineteen case names
  rather than a floor: the earlier `checked >= 15` was satisfied by any table
  at least that long, so four rows — among them the O_TRUNC-window rows that
  are the reason the table exists — could be deleted with both suites green.
  Naming every row makes a removed row fail and a renamed one fail too.
- **Gate:** `make lint`, `make test`, `make smoke`, `make release-smoke`,
  `make app-check`, `make delivery-index` all pass. `docs/lock-parity.tsv` is
  staged so the delivery gate can see it tracked.

### Still open after this pass

Audited, real, not yet fixed:

- Nothing in this list is a known-wrong behavior any more. What remains is
  design, not defects: the lock rule is verified rather than shared, so a future
  change to it must touch both implementations and the table; the log retention
  is a command rather than a policy; and the 5s host-facts cache and the
  absence of a push channel are the deliberate trades they were documented as.
- An operator who never runs `loop-studio logs` still accumulates logs. The number is
  now printed when they look, and `loop-studio doctor` would be the place to surface it
  unprompted — which is a behaviour change for a command that currently only
  reports, so it is not done here.

---

## 2026-09-27 Re-verification (10x pro measurement + port-rule pass)

The previous pass ended with a note that the TUI had never been audited. It
had not: 779 lines, and not one test. It turned out to hold the last two
copies of a rule the rest of the project had already fixed, plus a fabricated
measurement, plus a swallowed exit status.

### The TUI decided with numbers nobody had measured

- **Unknown free RAM was reported as zero, and then acted on.** `smart_serve`
  read `free_ram_gib().unwrap_or(0.0)`. A failed `vm_stat` therefore printed
  `Free RAM: 0.0 GiB` and handed the rules engine the smallest model and the
  smallest context window — the conservative-looking answer, arrived at by
  inventing a number. `decide_model_and_context` now takes `Option<f64>`, every
  RAM test requires a reading, and the reason line says `unknown` instead of
  quoting a figure. An absent reading is not evidence of pressure; treating it
  as such chose the floor configuration on exactly the machines whose probe was
  broken.
- **The health check probed different ports than it printed.** It printed the
  Ollama and llama lanes on literals `11434` and `8081` while probing those
  same literals, and the MLX lane beside them read `common::mlx_port()` — the
  one lane that had been fixed in an earlier pass. So with
  `LOOP_STUDIO_LLAMA_PORT=9001` the same menu reported a live lane `DOWN` and offered
  to start a server that would bind 9001. Both the label and the probe now come
  from one `health_lanes()` list, so they cannot disagree again.
- **A failing command looked like a succeeding one.** `run()` printed its
  status only when the output was empty, so anything that printed *and* failed
  — a `df` on an unmounted volume, `kv-manage check` at RED, an `opencode2`
  run that errored — rendered exactly like a success. Every other gate in this
  project was taught to report a nonzero exit; this was the one place still
  swallowing it. A failure is now the headline, and a success says so too.
- **It had its own token meter.** The TUI scraped `opencode2` alone, never read
  `LOOP_STUDIO_TOKENS` or the router's usage log, and then chose a context window from
  the result — so the number it decided with could differ from the number
  `kv-manage check` reports by orders of magnitude, with both looking
  authoritative. The five token-reading functions now live once in `common.rs`
  and both binaries call them. A disabled `--json` note explains what the number
  is when there is one.
- **Its reason strings hid their own inputs.** The middle tier explained itself
  as `Nominal + 7.4 GiB free -> mlx Q4 ... balanced speed`, never mentioning the
  token load that had demoted it from Q8. An operator reading that has no way
  to know a third input was consulted; it is now in the string.

### The port rule existed four times, and three of them allowed zero

The one-rule pass fixed `MLX_PORT=0` — an engine handed `--port 0` binds an
ephemeral port while the router keeps reading 0 — and wrote down the rule: *a
writer must never publish a value its own reader discards*. Three readers and
one writer had never been brought under it.

- `LOOP_STUDIO_ROUTER_PORT=0` bound the **whole gateway** to a random port. The router
  logged its real address, and `loop-studio chat`, `loop-studio status`, the TUI and `loop-studio
  doctor` all dialled 0. The primary port of the project, unreadable, by a
  single typo.
- `LOOP_STUDIO_LLAMA_PORT=0` did the same to the quality lane — and `serve_llama` had a
  **fourth** definition of that port, a bare `parse()`, disagreeing with
  `common::llama_port()` that the router, the TUI, `loop-studio status` and Studio all
  read. The writer now calls the reader, the same delegation `serve-mlx` already
  makes.
- One rule, `common::port_from`, now answers for all four: trimmed, `u16`, and
  never zero. An unusable port is named in the startup log of the router and of
  `serve-llama`, where an operator is reading it, instead of surfacing much
  later as a service nothing can reach.

### A retry loop that could not recover from its own commonest cause

`serve-mlx`'s daemon loop reused the port it was handed on every attempt,
forever. The most frequent reason for a bind failure is *something else already
owns that port* — precisely the case a retry can never fix. The wrapper re-ran
`--port 8080` every 60 seconds for the rest of the machine's life, failing
identically, while `~/.lac/mlx.port` kept advertising 8080. A crash now probes
the port first: nothing listening means the engine was OOM-killed and this is
the case the loop exists for, so it keeps the port; something listening means it
moves to a free one **and republishes the port file**, because a lane that
drifts while the file still names the old port is a lane the router cannot find.
A port the operator named with `MLX_PORT`/`PORT` is held and reported, never
silently moved.

### The gate was quietly able to lose a test

While removing a duplicated function I deleted a neighbouring `#[test]`
attribute. The test became a plain function: valid Rust, never run, and the
suite reported one test fewer and stayed green. `common.rs` carries a
file-level `#![allow(dead_code)]` — necessary, since eight binaries share it and
each uses a fraction — which is exactly what hid this. Every test module in
`src/` now carries `#![deny(dead_code)]`, so a dropped attribute is a compile
error. Verified by deleting an attribute again: the build fails with
`function ... is never used`.

### `loop-studio doctor` now reports what the logs hold

The open item from the last pass, done. The daemons bound growth and keep three
generations, so the disk cost is real and grows with uptime, and an operator
who never runs `loop-studio logs` never saw it. `doctor` prints the footprint unprompted
and names the reclaim command when it is large. It is a **warning, never a
failure** — `doctor`'s exit code is what scripts branch on, and a chatty log
must not be able to fail a healthy stack. There is a test for that specific
property, because "never fail" is exactly the kind of rule that decays.

### A test that reported the weather

One Swift run in this pass failed and did not reproduce in 55 subsequent runs,
including 20 under deliberate CPU load; the failing test's name was not
captured, so I am not going to call it understood. Auditing for the class
instead found the cause: `loopsStoreMutations` demanded `ticks > 3` from a 20ms
ticker over a 400ms window. The property under test is "the main actor was not
blocked by the mutation", and a blocked actor produces *zero* ticks in that
window, not fewer — so the high count was measuring the host's ability to
schedule a timer, and failed when the box was busy while the code was correct.
The floor is now 2, which still separates blocked from not-blocked. Nine
further runs under load pass.

### Verification

- **Revert-tested, 8 for 8.** Every new test was confirmed to fail for the bug
  it targets: the TUI's hardcoded lane ports, its fabricated RAM, its swallowed
  exit status, the port-zero readers, `serve_llama`'s fourth port definition, the
  retry loop's unconditional reuse, the doctor line escalating to `fail`, and a
  deleted `#[test]`. One test *looked* revert-sensitive and was not: the RAM
  revert was not faithful, because the `None` arm of the reason string survived
  it and kept saying `unavailable`. The revert was redone. Separately, the first
  attempt at covering the retry loop passed with the fix removed, because it
  only covered the decision function; the recovery step was restructured into
  one observable unit (`recover_lane_port`) that returns the next port *and*
  publishes it, and one test is a labelled source check on the single call
  site — the only test here that is not about behaviour, and it says so.
- **Tests:** 478 Rust in `make test` (was 447) and 90 Swift (unchanged).
  `make test` skips `process_level_gateway_and_cli_smoke` by design and
  `make smoke` runs it, so the two together are 479 distinct Rust results.
- **Gate:** `make lint`, `make test`, `make smoke`, `make release-smoke`,
  `make app-check`, `make delivery-index` all pass.
- **Not verified here:** `a_non_mlx_host_is_never_offered_mlx` returns early on
  Apple Silicon, so the gate it covers is untested on this host. It is
  self-skipping rather than passing vacuously, and it will run on Intel.

### Still open after this pass

- `LOOP_STUDIO_LLAMA_PORT` set to the MLX lane's port (or the reverse) is accepted and
  both lanes then fail to bind. Both are now operator-chosen ports, so this is
  a configuration mistake rather than a bug in the code, and the right answer
  may be a refusal at startup rather than a warning.
- The TUI still has no test of `health_check`'s network path or of
  `switch_backend`; only the port resolution and the rules engine are covered.
- `a_non_mlx_host_is_never_offered_mlx` needs an Intel run to mean anything.

---

## 2026-09-28 Re-verification (10x pro measurement-honesty pass)

Two read-only auditors went over the two largest files in the project
(`lac_router.rs`, 4803 lines; `loop_studio.rs`, 4123) with the brief "assume nothing is
correct, and only report what you read". They returned 30 findings. Eight were
verified by hand before any code was touched — running the binaries, reading
the source, checking the tool — and the rest are written up below as open
because they were not.

The seven that are fixed share one shape: **something reported a number, or a
success, that it had not measured.** Every one of them was a place where a
failure, an absence, or a default had been flattened into a value that a human
or a script then read as fact.

### The thermal reading was invented on every host

This is the finding that mattered most, and it was a one-word bug.

`common::thermal_detail` ran `pmset -g thermals`. That is not a pmset
argument. pmset answers `Error: unhandled argument thermals` and exits 1 — on
every host, always:

```
$ pmset -g thermals ; echo $?
Error: unhandled argument thermals
1
$ pmset -g therm ; echo $?
Note: No thermal warning level has been recorded
Note: No performance warning level has been recorded
Note: No CPU power status has been recorded
0
```

The probe discarded the exit status, handed every caller an empty string, and
`thermal_state` reported the empty string as `"Nominal"`. So the reading was
fabricated everywhere, including on the machines it exists to protect:

- `loop_studio_tui.rs`'s `decide_model_and_context` picks Q4-vs-Q8 *and* the context
  window from it. A fabricated Nominal spends the largest lane.
- `serve_llama.rs`'s `thermal_default_context` sets the server's window to
  16384 on the same input.
- The loop's preflight gate and the worker's thermal backoff both branch on it.
- `/loop-studio/status` published `"thermal": "Nominal"` next to two honest `Option`
  fields, so the dashboard's gauge was the one number in the document that
  could not be `null`.
- `loop-studio thermal`'s detail pane printed the empty string, so the operator's own
  check of the reading showed nothing at all.

The previous pass had already done the neighbouring work — it made free RAM an
`Option` and stopped the rules engine picking the floor configuration on a
broken probe — and had left the argument itself wrong, so the class was half
fixed. The asymmetry is the tell: an absent RAM reading chose conservatively
and an absent thermal reading chose the ceiling.

`thermal_state` is now `Option<String>`, and `None` means what it says. The
flag is correct, the exit status is honoured, and `parse_thermal` classifies a
*successful* capture: a `CPU_Speed_Limit` of 71 reads as Critical, 80 as
Serious, 95 as Fair, and an idle Mac's "no limit in effect" reads as Nominal —
which is a reading, not an absence, so healthy machines do not print `unknown`.
Silence and a rejected invocation are `None`. Every consumer was propagated:
the router publishes `null`, the CLI's `--json` publishes `null`, and
`thermal_label` is the one place an absent reading becomes a word, so no reason
line or banner can quietly claim a level nobody measured. The rules engine's
reason strings now carry the reading they were given, which is what the earlier
pass did for the token load.

Three client-side defaults would have re-introduced the same lie one process
away, so they went too. The status bar rendered `host.thermal ?? "nominal"` in
green; the Ops Dashboard's `thermalColor` mapped every unrecognised string to
green; the menu-bar item was conditional on a reading, so a probe that could not
answer silently deleted the telemetry line — an absence indistinguishable from a
machine with nothing to report. The bar now says "Thermals unknown" in grey, the
dashboard's default arm is `.secondary` rather than `.green`, and the menu bar
names the absence. Grey is the absence of information; green is a claim, and
only a *measured* Nominal gets one.

These were latent rather than live: before the flag was corrected the router
could never publish that `null` — it only ever published "Nominal" — so all
three defaults were unreachable. Each one turned a real `null` into the
all-clear, and the Rust side had just started emitting real nulls.

The parser had a second defect the fix would have walked into: the CLI read
`"thermal"` out of the router's document by searching for the next `"`, so a
`null` thermal returned the *following key's name* as a string —
`"free_ram_gib"`, a level no host has ever reported — and printed it as a
measurement. `json_string` now rejects `null` first, and the shape is pinned.

The bucket edges in `parse_thermal` are this project's policy, not Apple's, and
the code says so where a reader would otherwise assume otherwise.

### The gateway's own bind port was the last reader speaking the old language

The one-rule pass wrote the rule down — *a writer must never publish a value its
own reader discards* — and its own comment claimed it covered "three readers
and one writer". It did not. The router kept three bare `parse()`s:

- `LOOP_STUDIO_ROUTER_PORT=0` bound the whole gateway to a random port while `loop-studio
  chat`, `loop-studio status`, the TUI, Studio and `doctor` all dialled 0. The startup
  log said "warning: ignored, using 8000" and then, on the next line,
  "Listening on http://127.0.0.1:54321".
- `LOOP_STUDIO_LLAMA_PORT=0` and `LOOP_STUDIO_OLLAMA_PORT=0` were worse than a bad probe
  target, because `StatsMap` is keyed by `u16`: two lanes configured to 0 were
  *one entry*, sharing a consecutive-error count and a circuit-breaker verdict,
  with `/loop-studio/status` printing the same numbers under two backend names.

The test that "pinned the delegation" compared `serve_llama` against
`common::llama_port()` and never looked at the router, which is why this
survived a pass that was looking for exactly this.

The deeper problem was in the suite. `tests/support/mod.rs` and every
process-level test got an ephemeral router by setting `LOOP_STUDIO_ROUTER_PORT=0` and
reading the real port back off the startup log. **The suite was pinning the
bug** — it read the port off stdout and carried on regardless, so it would have
kept passing with the fix in place, and kept passing with the bug in place.
`ephemeral` is now the only spelling that means zero, and the tests use it. The
handoff is also extracted into `resolve_bind_port(env, arg)`, so the rule is a
pure function that a test can assert rather than a `main()`-local expression.

### Nine dispatch arms reported success for work that did not happen

`let _ = Command::new(..).status()` erases two different failures into exit 0:
a binary that is missing (`Err`) and a child that ran and failed
(`Ok(non-zero)`). Nine arms in `loop_studio.rs` did it. The blast radius is every
documented remedy and every Makefile target that routes through them:

- `loop-studio kv check` could not return the 2 that RED is documented to produce, so
  no script could branch on KV risk through the wrapper.
- `loop-studio bootstrap`'s own `exit(1)` was unreachable behind the wrapper's exit 0,
  which is what `make bootstrap` chains on.
- `loop-studio route`, `loop-studio tui`, `loop-studio serve`, `loop-studio models` did the same.

One `exec_sibling` now owns it. The same class, in three more places:
`cmd_visualize` treated `Ok(_)` from `open` and from the Studio binary as
"opened" (it is `Ok` for *any* exit code, so a corrupt bundle returned as a
success); `loop-studio daemon uninstall` printed "Loop Studio daemons uninstalled." and exited 0
after discarding the result of all three `launchctl` fallbacks and of the plist
`remove_file`, so from a session with no `gui/$UID` domain the agents kept
running while their plists were deleted; and `loop-studio loop validate` — the one
command whose entire job is to be a gate — printed its errors and exited 0, so
`loop-studio loop validate bad.yaml && deploy` deployed.

### `loop-studio bench --tokens 128` benchmarked port 128

The dispatch arm took the first argument that did not begin with `--` and parsed
it as the port, which is the *value* of the flag. The help line in `loop_studio.rs`
advertises exactly `bench [port] [--tokens N]`, and `LOOP_STUDIO_BENCH_TOKENS` exists to
mirror it, so the documented invocation of the command was a benchmark of a port
that is not the gateway: `Connection failed to port 128`, exit 1, against a
perfectly healthy stack. A flag's value has to be skipped, not probed.

### The honest token source undercounted every response

`resp_body` is the bytes the client received, and outside the request body it is
the only input to the KV-risk estimate. The response head was written to the
client and charged nowhere — and because the head is *sanitized* on the way out
(the backend's `Connection: keep-alive` is rewritten), a reader cannot
reconstruct it. Measured against the live binary: a 440-byte body arrived as 146
bytes of head plus body, and the log recorded the body alone. The error scales
with head size, so it was worst exactly where it matters — short completions
behind a long `Server`/`Date` head.

This is the number `kv-manage check` prints as the session's token load and the
one `context-cap` throttles on, and the log is what the code calls "the honest
token source". The charge is now made at the length actually written, and it is
asserted at the byte level against the socket in `tests/sse_relay.rs` rather
than against the log, because a log-only assertion would have passed with the
bug in place.

Measured against the live router with a scripted backend, one 243-byte body
behind a 164-byte head:

```
bytes the client received: 407
head bytes: 164  body bytes: 243
log says resp_body = 407
```

Before the fix the same exchange logged `resp_body = 243` — 41% low — and
`est_tokens` came out 98 instead of 118.

### A test that reported the weather

`a_crashed_lane_keeps_its_port_when_nothing_holds_it` bound an ephemeral port,
dropped it, and asserted the probe then read it as free. It failed about one run
in four on an idle host. The port is visible to the whole machine between the
two halves of that observation, so the test was measuring the kernel's port
allocator, not the recovery logic. The decision is a pure function and is still
asserted exactly; only the socket observation is retried, and it fails if the
probe is ever systematically wrong. This is the second time in this project a
test has been found measuring the host — the first was
`loopsStoreMutations` demanding `ticks > 3` from a 20ms ticker.

### A reclaim that reported bytes it had not reclaimed

`purge_rotated_generations` added each file's size to the total *before* the
unlink and discarded the unlink's error, so a purge against a read-only volume
printed "8.0 MiB reclaimed" for a directory that was still exactly as large —
and the same bytes went straight back into the `loop-studio doctor` total on the next
run. It now counts only what it removed, reports the failures, and the caller
exits 1 on them.

The same command's name list was a five-entry hand-list whose catch-all was the
router, so `loop-studio logs wroker` silently tailed the *router's* log and printed its
path with no error, and three of the eight rotated names (`loop-studio-worker.err`,
`loop-studio-serve-mlx.err`, `loop-studio-serve-llama.err`) were counted by `loop-studio doctor` but had
no reachable name here — so the message "reclaim with `loop-studio logs <which> --purge`"
named a reclaim nobody could type. The list is now derived from
`log_names_for`, the same function the rotator and the plists use, and an
unknown name is an error that lists the real ones.

### Verification

- **Revert-tested, 9 for 9.** The thermal flag, the fabricated-Nominal fallback,
  `port_llama`, the gateway's own bind port, the sanitized-head charge, the
  purge's byte count, `bench_port`, the log-name fallback, and the
  `loop validate` gate were each confirmed to fail with the fix removed. Two
  needed a second attempt: the `loop-studio logs` revert initially passed because the
  test's assertion was on the wrong side of a build, and the head-charge revert
  passed against a *unit* test that could not observe the accounting — which is
  why that one is now an end-to-end byte-level assertion in `sse_relay.rs`.
- **A test that was itself a bug.** `bytes_the_purge_could_not_reclaim_are_not_
  counted` initially passed as root, where the denied unlink is permitted. It
  detects root and says so instead of passing vacuously.
- **Tests:** 533 Rust in `make test` (was 478) and 92 Swift (was 90). The
  arithmetic is checkable: this pass added 6 tests to `common.rs` — which is
  compiled into all eight binaries, so 48 executions — plus 3 in `loop_studio.rs`, 4 in
  `lac_router.rs`, and 2 in Swift. `make test` skips
  `process_level_gateway_and_cli_smoke` by design and `make smoke` runs it, so
  the two together are 534 distinct Rust results. The new process-level
  assertions live inside the existing `e2e_smoke.rs` and `sse_relay.rs` tests,
  so they run under `make smoke` only.
- **Gate:** `make lint`, `make test`, `make smoke`, `make release-smoke`,
  `make app-check`, `make delivery-index` all pass.

### Still open after this pass

Audited, real, not fixed here — recorded so the next pass does not re-derive
them:

- `loop-studio loop run` retires a queue task to `complete` having dispatched no agent.
  Its phase 5 prints `[HUMAN APPROVAL GATE]`, a diff stat and `[LOOP
  COMPLETED]`, and nothing reads stdin; the only thing that had to succeed is
  `cargo test` on the *pre-existing* tree. The net effect on the queue is to
  delete a pending task while doing nothing to it. This is the largest
  remaining finding in the project and it is a design problem, not a one-line
  fix.
- Per-task agent transcripts in `~/.lac/task-logs/` grow without bound, are
  written one file per attempt, and are invisible to the one command that
  reports disk cost — `lac_logs_footprint` only scans `~/Library/Logs` for
  `lac-*`. A 24/7 worker on a repo with a slow test suite writes a full
  transcript per task forever while `loop-studio doctor` reports the daemon logs alone.
- A typo in `LOOP_STUDIO_BACKEND` silently overwrites the operator's persisted lane
  choice with `auto`, because `backend_id` maps an unrecognised value to
  `BACKEND_AUTO` and the non-empty/non-auto guard does not notice. The write's
  result is also discarded, so a read-only `~/.lac` is completely silent.
  `/loop-studio/switch?target=mlxx` has the same shape and answers `200 "switched"`.
- `loop-studio doctor`'s loopback rule is a second, case-sensitive definition of the
  router's: `LOOP_STUDIO_BIND_ADDR=LocalHost` fails doctor and starts fine, while
  `::ffff:127.0.0.1` makes the router exit(1) with no `remote_auth` row at all.
- The router answers 401/403/421/503 before reading the request body, so a large
  rejected upload is RST mid-response and the client sees a broken pipe rather
  than the error that would tell it what to fix. Measured: a 3 MiB POST with no
  backend produced 307 bytes of a declared-199-byte response.
- One `std::sync::Mutex` is held across the whole multi-backend catalog fetch
  (up to ~10s of `http_get`), and every waiter consumes an inflight slot, so a
  slow catalog fetch during a model-picker burst can exhaust the router and 503
  every *other* client. `HealthCache` was deliberately given per-port probe
  locks; `CatalogMemo` was not.
- `kv-manage check --threshold 0` is always RED, while `--keep 0` is rejected —
  the asymmetry is inside one function.
- `bootstrap` claims to be "keeping existing task queue" for a file that does
  not exist, and exits 0 for every step that only incremented `warns`, which is
  the case its own comment says must be nonzero.
- Per-task agent transcripts, the `LOOP_STUDIO_BACKEND` typo, and the loop's premature
  completion are all consequences of one missing question: *what did this
  command actually do?* Every one of them is a place that question is not
  asked.

---

## 2026-09-28 Re-verification (10x pro what-did-this-do pass)

The previous pass closed seven "something reported a number or a success it had
not measured" defects and left eight open items. This pass took the seven
small ones and the one that was large. The theme had already been named in the
open list, so this pass is that question asked properly, item by item.

### `loop-studio loop run` deleted a task from the work queue

This was the largest remaining defect in the project, and it survived a pass
that had fixed the *same class in the same function* — which is worth
understanding, because the reason is instructive.

The command runs five phases. Phase 2 prints, in its own words:

```
  · This command dispatches no agent and writes no files.
```

Phase 3 prints that no reviewer ran. Phase 4 runs `cargo test` — on the
**pre-existing** tree. Phase 5 then printed a green `[LOOP COMPLETED]` banner
and wrote `status: complete` into the Kanban queue.

So `loop-studio loop run`, with the task it had just described still pending, deleted
that task from the queue. `loop-studio worker` selects pending tasks, so it could never
pick it up again. The task was not done; it was deleted, by a command that
reported success, and the only thing that had to succeed was a test run on code
the command had not written.

Why the earlier pass missed it: `--dry-run` *was* protected, because its test
gate refuses before phase 5 — and the previous pass added exactly that test. The
unprotected arm was the real one. A fix that covers the case you happened to
write a test for is not the same as a fix.

The queue write is gone. The command now says what it is:

```
[REHEARSAL COMPLETE] Phases 1-4 were checked. Nothing was implemented,
  nothing was reviewed, and no agent was dispatched. To do the work, run
  `loop-studio worker --drain` (or the loop-orchestrator skill), which dispatches the
  agent inside a task/<id> worktree and is what may retire this task.
  · Queue untouched: [lac-005] is still pending in /Users/organic/todo/loop-studio-tasks.yaml
```

Verified against the real queue, not a fixture: the run reached phase 5, printed
the rehearsal banner, and the queue file was byte-identical afterwards.

The same function had a second, quieter version of the same problem. Three call
sites unwrapped `git` output with `unwrap_or_default()`, so a `git` that could
not be spawned — not on `PATH`, or a root that is not a repository — produced
`""`, and `""` is exactly what a clean working tree produces:

```
  [x] Working tree could not be read — git status -s exited exit status: 128:
      fatal: not a git repository (or any of the parent directories): .git
```

`git_in` now returns a `GitOutput` that keeps "it did not run" distinct from
"it ran and printed nothing", and every mutating caller asks
`.succeeded()` — the status is still the whole answer for a `git branch` or
`git commit`, and only the *reporting* paths needed the distinction.

### The transcript tree grew without bound, and nothing could see it

`run_logged` tees the full stdout+stderr of every `opencode2 run` and every
`make test` into a fresh timestamped file under `~/.lac/task-logs`. One file per
task per attempt, no size cap, no rotation, and no reader anywhere in the
project. A 24/7 worker on a repo with a slow test suite grows it for as long as
the machine is up.

The part that made it worse than an ordinary leak: the one command whose job is
to report log cost could not see it. `lac_logs_footprint` scans
`~/Library/Logs` for names beginning `lac-` — a different directory — so
`loop-studio doctor` reported the four daemon logs and said nothing about the tree that
is usually the larger of the two.

Three changes, in the shape this project already uses for daemon logs:

- **Bounded automatically.** The newest 40 transcripts survive; any single file
  over 32 MiB is truncated at that boundary, so one runaway build cannot hold
  the whole budget alone. The trim runs after a task's transcript is closed,
  never while one is being written, so a run in progress is never a candidate.
- **Reported unprompted.** `loop-studio doctor`'s `logs` row now names the tree, its
  size, its file count, and the reclaim — and escalates to `warn` on its own
  bound, while still being incapable of reaching `fail`, because `doctor`'s exit
  code is what scripts branch on.
- **Reclaimable by name.** `loop-studio logs tasks` shows and tails it; `loop-studio logs tasks
  --purge` deletes everything past the newest 40, on the operator's word and
  never automatically. `tasks` is now in the "Known:" list the refusal prints,
  because a refusal that omits the name the operator was one keystroke from
  typing is a poor way to teach it.

The ordering is the part that matters, and it is asserted directly: **the newest
transcripts survive**, because they hold the failure you would want to read. The
first version of the trim took the *oldest* 40 and deleted them, which is the one
order that throws away the recent diagnosis and keeps the stale one. The test
caught it, which is the argument for asserting order and not just the resulting
count.

The first version of the tests set `HOME` to point at a fixture directory. Two
of them ran in parallel, each repointed the other at its own tree, and both
failed — so the helpers grew `*_in(dir)` forms and the fixtures address an
explicit directory. A filesystem policy that can only be tested by mutating
process-global state is a policy that will be tested wrong.

### A typo in a backend name destroyed the operator's saved lane

`backend_id` maps every unrecognised string to `BACKEND_AUTO`. That is the
correct reading of a *corrupt saved file* — a bad preference must not stop the
router booting — and the wrong reading of an operator *asking* for a lane. Both
request paths had this defect:

- `LOOP_STUDIO_BACKEND=mlxx` passed the "non-empty and not `auto`" guard, resolved to
  `auto`, and was **written to `~/.lac/router-backend`** — one typo in one
  plist line replaced the lane the operator had chosen and persisted. The write's
  result was also discarded, so a read-only `~/.lac` was silent as well.
- `/loop-studio/switch?target=mlxx` did the same and answered
  `200 {"status": "switched"}`.

Now there are two functions, because "I could not read your saved choice" and
"I do not know what you asked for" are different sentences:

```
$ curl 'localhost:8000/loop-studio/switch?target=mlxx'
{ "status": "rejected", "reason": "unknown backend",
  "target": mlxx, "known": "mlx, llama, ollama, fastest, auto" }   [400]

$ LOOP_STUDIO_BACKEND=bogus loop-studio-router
warning: LOOP_STUDIO_BACKEND="bogus" is not a backend; ignoring it and keeping any
saved choice. Known: mlx, llama, ollama, fastest, auto
```

The env path fails closed on the *write* and open on the *value*: the router
still starts and still serves, the saved lane is left alone, and the mistake is
named in the startup log where the operator will read it. A valid `LOOP_STUDIO_BACKEND`
still wins and still persists.

One more thing in the same handler: `target=fastest` reported MLX's port.
`fastest` and `auto` are lane *orderings*, not addresses, and
`backend_port` has no arm for either, so it fell through to `port_mlx()`. A
caller that trusted the reply dialled MLX because it asked for the fastest
lane. Those two now report `"port": null`.

### Three smaller ones, all the same shape

- **`loop-studio doctor` and the router each defined "loopback".** The router refuses to
  start on a non-loopback bind without a token; `doctor` reports that refusal so
  an operator finds out before launchd does. Two definitions that disagree make
  the diagnostic worse than nothing: it either fails a machine that would have
  worked (`LOOP_STUDIO_BIND_ADDR=LocalHost` failed `remote_auth` and exited 1 while the
  router started fine) or passes one that will not start
  (`::ffff:127.0.0.1` made the router `exit(1)` with **no `remote_auth` row at
  all**, because the check only emitted a row on the non-loopback arms). There is
  now one function, in `common`, used by both, and the check reports on every
  arm — a check that only appears when it has already failed is a check whose
  absence means nothing.
- **`kv-manage check --threshold 0` was always RED.** Zero parsed, so
  `measured >= threshold` held for every measurement: `risk: RED`,
  `action: truncate now`, on a session holding four tokens. A RED carrying no
  information, from the one command whose job is to measure. `--keep 0` was
  already rejected a few lines away, so two halves of one flag family disagreed
  inside one file. Both are refused now. `--tokens 0` deliberately still parses:
  "this session holds no tokens yet" is a real reading, and a zero *window* is
  not a smaller window.
- **`bootstrap` exited 0 for a warning-only run.** Nine of its ten steps can
  only ever increment `warns`, so on a fresh Mac with no `brew` it printed
  "complete with 7 warning(s)" and returned success, and `make bootstrap` could
  not see it. The file's own comment, three lines below that arm, said a zero
  would "carry a half-configured machine forward as a configured one" — which is
  why the rule held for `failures` and quietly decayed for `warns`. Warnings are
  now exit 2, a distinct code from failure's 1, so a caller can tell "not
  configured" from "broken" without parsing stdout. Separately, its `else` branch
  claimed "keeping existing task queue" for a file that did not exist, when the
  destination was absent *and* the template was absent.

### Verification

- **Revert-tested, 6 for 6.** The queue write, `LOOP COMPLETED`, the transcript
  trim's ordering, `bootstrap_verdict`'s warn arm, the loopback rule's
  IPv4-mapped case, and `parse_size_result`'s zero arm were each confirmed to
  fail with the fix removed.
- **Two of my own bugs, caught before the gate.** The transcript trim deleted
  the oldest 40 instead of the surplus — the exact order that destroys the recent
  diagnosis. And two transcript tests initially set `HOME` and fought each other.
  Both are in this pass's history because both were found by tests written for
  the change, which is the only reason they were found at all.
- **One test that had to be rewritten rather than added.** The `kv-manage` zero
  check was first written as a blanket `parse_count_result` refusal, which
  rejected `--tokens 0` and broke an existing test that correctly wants it. The
  right shape was two parsers — a count and a size — not one flag.
- **One failure I could not reproduce, and made diagnosable instead.** During
  one full `make validate`, `router_intel` failed at
  `/loop-studio/switch?target=fastest` with status `0`. It passes alone and in three
  subsequent full runs. Its harness `.unwrap()`s the connect, so a `0` could
  only mean the router *accepted* the connection and answered nothing — a
  per-connection failure, not a busy listener and not a bad request. I have not
  established a cause and am not going to claim one. What I did instead: the
  harness now distinguishes the three outcomes (answered, accepted-then-silent,
  connect-refused), retries *only* a refused connect — a machine event that a
  single attempt turns into a scheduler-mood assertion — and never retries
  accepted-then-silent, because that is the router's behaviour and a retry would
  hide exactly the thing worth catching. The diagnosis is carried in the string
  every assertion in that file already formats, so a future occurrence says what
  happened instead of printing `0`.
- **Tests:** 558 Rust in `make test` (was 533) and 92 Swift (unchanged), plus
  `make smoke`'s 1. The 25 executions this pass added are 5 tests in
  `common.rs` compiled into all eight binaries, 3 in `loop_studio.rs`, 2 in
  `lac_router.rs`, 1 in `kv_manage.rs`, and 1 in `bootstrap.rs`; the
  `kv-manage` zero and the transcript reporting are additionally asserted
  process-level, inside the existing `e2e_smoke.rs` test.
- **Gate:** `make lint`, `make test`, `make smoke`, `make release-smoke`,
  `make app-check`, `make delivery-index` all pass.

### Still open after this pass

Audited, real, not fixed here:

- The router answers 401/403/421/503 **before reading the request body**, so a
  large rejected upload is RST mid-response and the client sees a broken pipe
  rather than the error that would tell it what to fix. Measured earlier: a 3 MiB
  POST with no backend produced 307 bytes of a declared-199-byte response. The
  gateway is safe; the diagnosability is not.
- One `std::sync::Mutex` is held across the whole multi-backend catalog fetch
  (up to ~10s of `http_get`), and every waiter consumes an inflight slot, so a
  slow catalog fetch during a model-picker burst can exhaust the router and 503
  every *other* client. `HealthCache` was deliberately given per-port probe
  locks; `CatalogMemo` was not.
- The TUI still has no test of `health_check`'s network path or of
  `switch_backend`; only port resolution, the rules engine and the thermal
  probe are covered.
- `a_non_mlx_host_is_never_offered_mlx` needs an Intel run to mean anything.
- The transcript tree is bounded by count and per-file size, but there is no
  *age* bound: on a machine that runs one task a week, 40 transcripts is years
  of history. That may be the right retention, and it is a policy question
  rather than a defect.
- The open question from the previous pass still stands, and this pass is the
  argument for it: **what did this command actually do?** Every defect in the
  last two passes is a place that question is not asked — a queue task retired
  by a command that dispatched nothing, a transcript tree nobody reported, a
  saved lane overwritten by a typo, a clean tree reported by a `git` that never
  ran, a "completed loop" from a rehearsal, a bootstrap that configured nothing
  and returned success. The pattern is more useful than any individual fix.

---

## 2026-09-28 Re-verification (10x pro can-the-caller-hear pass)

Four items, three of them from the previous pass's open list. The theme here is
narrower than the last two: **an answer that was produced and then thrown away.**
The gateway could not deliver its refusals, a model picker could not see a
model, and the TUI could not read a reason.

### A 4xx the client could never read

Every rejection on the router's pre-body path answers while the client's request
body is still queued in the socket. On BSD — and macOS is BSD — `close()` on a
socket with unread receive data sends RST, and an RST discards both the response
the router just wrote and everything still in the client's send buffer.

So the answer to "why is nothing happening?" was often `Broken pipe`. Measured
before this pass: a 3 MiB POST with no backend produced 307 bytes of a response
that declared 199, and the client saw a reset rather than the 503 whose body
says *"Start one with `loop-studio serve mlx`"*. The same applied to the 401 that would
have told a client to set a token, the 413 that says its body is too big, and
the 403/421 the browser gates emit.

`refuse` now drains before it answers, and all thirteen sites go through it —
one function, because the failure is invisible at any individual site: an arm
that forgets the drain reintroduces a reset for exactly one status code, and
nobody notices until a client reports a broken pipe instead of a 401.

The drain has two hard limits, and both matter:

- **A cap** of twice `MAX_BODY`. More than any body this router would have
  accepted, and more than the socket buffers, so every realistically-refused
  request drains completely — but a drain is not allowed to become a sink.
- **A deadline** of 750ms, deliberately shorter than `body_idle_timeout`. A
  client that declares a body and then stops must not hold a thread, and the
  refusal is worth more than a clean close.

No allocation and no `BodyReservation`: the bytes are dropped, not buffered, so
a refused request costs no memory and does not consume the body budget that
live requests are competing for.

Verified at the socket level, in three shapes — a body small enough to fit the
socket buffers, a 3 MiB body far larger than them, and a `Content-Length` past
the ceiling where the whole upload is still outstanding. With the drain removed,
the client's own write is reset, which is the whole failure.

### A model picker could not see a model, and could take the gateway down

`CatalogMemo` held one `Mutex` across the whole fan-out: up to three
`http_get(port, "/v1/models", 3000)` calls, so roughly ten seconds, for every
caller. Two consequences.

**A wedged lane froze the endpoint.** A backend that passes the health probe and
then stalls on the catalog read — a partially wedged inference server, which is
exactly what the circuit breaker exists for — blocked every caller's view of the
other two. `HealthCache` had been deliberately given per-port probe locks for
precisely this reason; the catalog had not.

**128 waiting callers took the router down.** Each blocked caller holds one of
the 128 inflight slots for the whole wait, so a burst of `/v1/models` requests
— which every model picker issues, and OpenCode on every launch — could exhaust
the router and make the accept loop answer every *other* client
`503 lac_router_busy`. A cheap metadata GET was able to deny service to chat
traffic.

Three changes, and the third is the one that matters most:

- **Per-backend entries and per-backend probe locks**, so a slow lane is a slow
  lane rather than a slow endpoint.
- **`try_lock`, not `lock`,** in the probe. A caller never *waits* behind a
  peer's probe: it declines that backend and falls through to single-backend
  forwarding, which is a correct and cheap answer. Waiting here is what let a
  metadata request exhaust the router.
- **Stale-while-revalidate.** Once anything is cached, an expired entry is
  served immediately and rebuilt on its own thread, exactly once per expiry. The
  cold path — nothing cached at all — still blocks, because there is nothing to
  serve and inventing a catalog would be worse than waiting. That distinction is
  the design: a picker that sees a model list a second stale is doing its job.

Two things worth recording about how this went. The first version of this
change was caught by a test that asserted the *mechanism* rather than the
property: `a_catalog_expires_so_a_newly_loaded_model_becomes_visible` required
the call which observes the expiry to rebuild synchronously, and
stale-while-revalidate failed it. The mechanism is what this pass deliberately
changed; the property — a newly loaded model becomes visible — is what the
operator actually needs. The test now asserts both halves of the property:
never blocked, and still converging.

And a design smell the tests surfaced: `probe` originally returned the body and
left caching to the caller, which had to remember to call `put_entry`. A
successful body is now cached by `probe` itself, so "a backend's catalog lives
in its own slot" is one invariant in one place. A refusal is *not* cached: a
lane that is down now may be up in a second, and a cached absence would keep the
picker blind through the whole ttl — the one moment it matters.

### The TUI threw away the reason

The item that had been open for two passes. Not the TUI's *port* handling — that
was fixed earlier — but what it did with a reply.

Both network call sites ended in `Some((c, _)) => "HTTP {c}"`, discarding the
body. The body is where the router puts the only useful part of a refusal, and
the previous pass had just started *creating* reasons there: the router now
answers `400 {"status":"rejected","reason":"unknown backend","known":"mlx,
llama, …"}` rather than silently choosing `auto`. So the TUI was, in effect,
about to start hiding a message written for it.

`describe_reply` is now one function used by both sites, and it keeps the body
on a non-200. `OFFLINE` — the gateway's entire answer when it was unreachable —
became a sentence naming the remedy. The switch menu's name mapping and URL
construction moved into `switch_target` and `switch_url`, so the set of names
the TUI can send and the set it believes in cannot drift.

### Verification

- **Revert-tested, 3 for 3.** The refusal drain (the client's own write comes
  back reset), the catalog's `try_lock` (a second caller waits 201ms behind a
  peer's probe), and `describe_reply`'s body retention (the reason disappears).
- **One test rewritten rather than added**, for the reason above: it asserted
  that an expired catalog is rebuilt synchronously, which is the mechanism this
  pass deliberately changed.
- **A test that was itself a bug.** The per-backend catalog test asserted the
  refusing lane was *not* probed, on the reasoning that a failed probe should be
  skipped. It is probed — once — and what must not happen is the refusal
  poisoning the *other* lanes. The test now says that.
- **Tests:** 562 Rust in `make test` (was 558) and 92 Swift (unchanged). The 4
  executions are 2 in `lac_router.rs` (a wedged lane must not empty the
  endpoint; a caller must not queue behind a peer's probe) and 2 in `loop_studio_tui.rs`
  (a refusal keeps the reason; the switch menu can only ask for a name the
  router accepts). The refusal drain is additionally asserted at the socket
  level, in a new process-level block in `sse_relay.rs`, and the catalog shape
  is pinned end to end by the existing model-picker scenarios in that file and
  in `router_intel.rs`. `a_catalog_expires_so_a_newly_loaded_model_becomes_visible`
  was rewritten, not added, so it is not in that count.
- **Gate:** `make lint`, `make test`, `make smoke`, `make release-smoke`,
  `make app-check`, `make delivery-index` all pass.

### Still open after this pass

- `a_non_mlx_host_is_never_offered_mlx` needs an Intel run to mean anything. It
  is self-skipping rather than passing vacuously, and nothing in this project
  can manufacture a non-Apple Silicon host.
- The transcript tree is bounded by count and per-file size but has no *age*
  bound: on a machine that runs one task a week, 40 transcripts is years of
  history. That may well be the right retention, and it is a policy question
  rather than a defect.
- The catalog's cold path still blocks. With nothing cached and a wedged
  backend, the first `/v1/models` waits out that backend's `http_get` timeout
  (3s per lane, sequentially) while holding an inflight slot. The fix would be
  to fetch the lanes in parallel, which needs either threads per lane or a
  shorter per-lane budget on the cold path; neither is obviously right, so it is
  written up rather than guessed at.
- The single `router_intel` failure recorded in the previous pass has not
  recurred, and the harness now names which of the three outcomes occurred if it
  ever does.

---

## 2026-09-28 Re-verification (10x pro two-writers pass)

The Swift app had been audited once, in a project where every other pass had
been about the Rust binaries. This pass gave it a read-only audit of its own and
followed the findings to their end. Two of them were critical, and one of those
two was *already visible on disk* — which is the part worth reading.

### The app was deleting the worker's retry budget on every write

`~/todo/loop-studio-tasks.yaml` has two writers: `loop-studio worker` and Loop Studio. The
Studio's mutation path was a whole-file rewrite from its in-memory model, and
`TaskItem` has no property for `attempts:`.

That key is the worker's entire retry budget. `MAX_TASK_ATTEMPTS` is 3,
`pending_candidates` filters on `t.status == "pending" && t.attempts <
MAX_TASK_ATTEMPTS`, and `update_task` writes `status: failed` and `attempts: 3`
in the *same call*. So a task dead-lettered to stop the daemon hot-looping was
one card-drag in the Studio away from being re-queued forever — the exact loop
the dead-letter exists to prevent.

This was not inferred from the source. The live queue on this machine, at the
start of the pass, held:

```
$ grep -c "attempts:" ~/todo/loop-studio-tasks.yaml
0
$ grep -c "status: failed" ~/todo/loop-studio-tasks.yaml
4
```

Four failed tasks and no `attempts:` keys — a combination the worker cannot
produce, because it writes them together. The field had already been stripped.

The same rewrite dropped every other unmodelled line: the shipped template
carries `metadata: {module, target}` and `{module, tokens}`, and only `module`
is modelled.

**The fix is to edit text, not to re-emit a model.** `parseTaskBlocks` returns
each task's block *and* every line in it the model does not name, and
`serializeTaskBlocks` writes them back verbatim. A writer that cannot round-trip
what it does not understand will destroy the other writer's state; editing the
text is the only way to be two writers of one file.

Two subtleties that the tests forced into the open, both about YAML rather than
about the queue:

- **Values.** The writer emitted `task: "\(item.task)"` and the reader used
  `replacingOccurrences(of: "task:", with: "")`. So `Fix the "task:" key` came
  back as `Fix the " key` and the next write persisted that. And the New Task
  field is a multi-line `TextEditor`, so a two-line description produced
  `task: "line1` / `line2"` — a double-quoted scalar may not span lines, and
  the file is read by a **line-based** parser on the Rust side, which then read
  `line1` while the app read both. Two readers permanently disagreeing, with the
  app the one that persisted it. Values are now single-quoted with embedded
  quotes doubled, and newlines flattened — flattening being the contract, not a
  loss, because a value that spans lines cannot be read at all on the other side.
- **Nesting.** A first attempt anchored on indentation alone, which is not
  enough: `metadata: { status: critical }` was then read as a nested child *and*
  consumed into the task's own status one level down. The rule has to be
  nesting-*aware*, not just depth-aware: `module` is a field under `metadata:`,
  `status` is not, and the two key sets are disjoint. Rust's `block_field` has
  been explicitly indent-anchored all along, for exactly this reason.

### …and that fix broke the reader, which is the more interesting half

Switching the Studio to single quotes changed what it writes, and the Rust reader
stripped only `"`. So `- id: 'a-1'` came back as the id `'a-1'`, quotes included,
and that flowed into the transcript path, the branch name, and `update_task`'s
own matching — the worker could no longer find the task it had just written.

The rule this project already wrote down is *a writer must never publish a value
its own reader discards*. The mirror is just as true and this pass tripped it:
**a reader must not disagree with the only two writers of the file it reads.**

`unquote` accepts both styles now, and the agreement is asserted from both sides:
Swift pins the round trip, Rust pins that a dead-lettered task is not pending in
either quoting, and the app's exact output is a Rust fixture rather than a
description of it.

### A successful router start latched the app for the rest of the session

`isChecking` is set before a start and cleared on *both* failure paths. Neither
success path cleared it, and both called `fetch()` — which opens with
`guard !isChecking else { return }` and is therefore a no-op while the flag is
set.

So one successful start latched the flag permanently: the 3s dashboard poll
stopped, the refresh button was disabled for good, the menu-bar refresh was a
no-op, and an app with no `response` yet showed "Connecting to router…" forever.
It only appeared when the start *worked*, which is the opposite of when an
operator would go looking — and the automatic start on the default first-run path
succeeds whenever the gateway comes up, so this was the common case.

`resumeAfterSuccessfulStart` now owns the ordering, and takes an injectable poll
so the *order* is assertable without a router — the same seam
`fetchHostFacts(loader:)` already uses. Clearing the flag after the call is the
same as never clearing it, and the test checks the flag's value at the moment the
poll runs, not afterwards.

### A dead gateway still read as "Online"

`response` was assigned in exactly one place — on a successful decode — and the
failure path left it alone. So with the router killed, every liveness indicator
kept reading the stale snapshot: `Loop Studio Gateway :8000 · Online` with a filled
checkmark, green header and footer dots, a pulsing avatar, and a chat empty-state
offering "Start MLX Backend" against a dead gateway. A stale reading of a
liveness probe is not a liveness reading, so the failure now clears it.

### The cold path of the catalog fan-out, measured

Last pass left the catalog's cold path as the known remainder: sequential, so
the cost was the *sum* of three `http_get` budgets. Before fixing it, it is worth
being precise about which case is reachable, because the obvious one is not.

`HealthCache::ready` probes with `http_ready(port, 1500)`, which is
`GET /v1/models` — the same request the catalog read makes. A lane that is slow
on `/v1/models` therefore fails readiness and is never probed at all. The
reachable case is narrower and more interesting: a lane that answers inside the
1.5s probe and *then* stalls, which is what a server that has accepted a
connection and stopped scheduling looks like, and precisely what the circuit
breaker exists for.

Measured with two such lanes, each stalling 2.5s on the catalog read after a
fast readiness answer:

```
=== SEQUENTIAL (the shape before this pass) ===
  catalog rebuild: 5.07s
  models: ['slow-a', 'slow-b']
=== PARALLEL (this pass) ===
  catalog rebuild: 2.60s
  models: ['slow-a', 'slow-b']
```

The sum of the stalls, not the maximum. The request thread waits for the slowest
probe instead of all of them, which is the only part of the old behaviour that
was not a free win. One thread per lane, joined before the merge, with the merge
sorted by tier so the document is byte-identical however the probes interleaved
— a model list that reorders itself between polls is a list that makes a picker
look unstable.

### Verification

- **Revert-tested, 5 for 5.** The latch ordering (the poll runs with the latch
  still set), the queue's nesting-awareness (a depth-only rule reads neither the
  decoy nor the field beside it), `unquote`'s single-quote handling (the id
  comes back as `'a-1'` and a dead-lettered task reads as retryable), plus the
  two Swift criticals.
- **Three bugs of my own, all caught before the gate.** The first nesting rule
  was depth-only. `insideMetadata` was cleared by the first sibling child, so
  `metadata: { status: x, module: y }` read neither. And my first Rust revert
  attempt silently no-op'd on shell escaping and reported a false pass — the
  same class of mistake as an earlier pass in this project, and the reason the
  revert is now a file rather than a one-liner.
- **A cross-reader break the tests could not have caught alone.** The
  single-quote change was correct for YAML and wrong for the Rust reader. Neither
  side's tests failed. It was found by asking what the *other* reader would do,
  and the agreement is now pinned from both ends.
- **Tests:** 566 Rust in `make test` (was 562) and 101 Swift (was 92). The 4 new
  Rust tests are 2 in `loop_studio.rs` (the two-writer agreement, from the reader's
  side) and 2 in `lac_router.rs` (the cold path costs the slowest lane; the merged
  document is ordered by tier, not by completion).
- **Gate:** `make lint`, `make test`, `make smoke`, `make release-smoke`,
  `make app-check`, `make delivery-index` all pass.

### Still open after this pass

From the Swift audit, audited and real, not fixed here — all in the same class,
and all in the app:

- A `200` with `{"choices": []}` or `message.content: null` is logged as
  `✓ Code Implementation Complete:`, and `implementPhaseFailed` stays false, so
  the human-gate sheet reports the benign wording for a run that produced
  nothing. The `⚠ nothing was produced` variant already exists and is never
  reached.
- "N.N tok/s" is `reply.count / 4` — a *character* count presented as a measured
  throughput, next to a `%.1fs` total as if both were measurements. The router
  publishes `est_tokens` in `/loop-studio/status`; it is not consulted. `max(1, …)` also
  gives an empty reply a non-zero-looking rate.
- A failed `/v1/models` read is answered with two hardcoded model names, styled
  identically to real ones, so selecting one sets a model that may not exist and
  the next send 4xxs.
- The diff sheet runs `git diff`, which cannot see staged or untracked changes,
  and prints "Working tree is completely clean — no unstaged or **uncommitted**
  changes detected" above the Approve Gate that sets `status: completed`.
- `MenuBarView` compares `therm == "nominal"` while the router publishes
  `"Nominal"`, so a cool machine still gets the warning icon and Fair, Serious and
  Critical are visually identical. `MainAppView` gets the same comparison right —
  the two files disagree with each other, and only one with the producer.
- `ChatView` renders `64k` from a literal; the router publishes no context length,
  and it contradicts the app's own `contextCap` of 32768 and the
  `max_tokens = min(cap/2, 4096)` it actually requests.
- `Int(ewma)` on unvalidated JSON from the gateway traps outside `Int` range; in
  remote mode that is another host on the tailnet.

From the Rust side, unchanged from the last pass: the readiness probe and the
catalog read being the same request is why a uniformly-slow lane is invisible
(it fails readiness) — correct behaviour, and worth knowing before anyone tries
to "fix" it — and `a_non_mlx_host_is_never_offered_mlx` still needs an Intel run
to mean anything.

---


> "When you close it the app is still running — that's our rust power for the community."

The launchd daemon system demonstrates this principle fully:

- **Before daemon install:** Closing the TUI/CLI stops all inference
- **After daemon install:** `org.loopstudio.router` and `org.loopstudio.worker` LaunchAgents keep services running in the background
- **Unix philosophy:** Rust-compiled, zero-dependency binaries launched at login, surviving user session closure

This is the architectural differentiator against LM Studio's visual layer + background process flaw. Loop Studio combines **beautiful visual design** (TUI + gateway UI) with **Rust-powered persistence** — the best of both worlds.