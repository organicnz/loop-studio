# Loop Studio Architecture Blueprint — Apple Silicon Macs

## 1. System Overview

The Local Agentic Coding (Loop Studio) stack targets Apple Silicon **Macs** (8GB+ supported; reference configuration below measured on a high-RAM Mac Studio). It runs **Qwen 3.8 27B** as the primary model across all coding, analysis, and auditing tasks. RAM-pressure thresholds and smart-serve tiers scale with detected total RAM (`common::mem_thresholds_gib`, `common::serve_ram_tiers_gib`), so smaller Macs get sane gates from the same code.

```
┌────────────────────────────────────────────────────────────────────────┐
│                        4. Orchestrator Layer                           │
│     Hermes Orchestrator      │  Kanban Loop Engine (`~/todo/loop-studio-loops`) │
└───────────────────────────────────┬────────────────────────────────────┘
                                    │ Drives & monitors
┌───────────────────────────────────▼────────────────────────────────────┐
│                      3. Client & Harness Layer                         │
│ Loop Studio (macOS Liquid Glass)      │ OpenCode V2 (Agents & 14 Skills)    │
└───────────────────────────────────┬────────────────────────────────────┘
                                    │ OpenAI API calls (:8000)
┌───────────────────────────────────▼────────────────────────────────────┐
│           2. Loop Studio Unified Gateway & Engine (100% Pure Rust Native)      │
│   `loop-studio` CLI  │  `loop-studio-router` :8000 (Hot-swap, Auto-routing, Telemetry) │
│   `loop-studio-tui`  │  `serve-mlx` :8080  │ `serve-llama` :8081               │
│   `kv-manage`│  `pull-models`      │ `bootstrap`                       │
└──────────┬────────────────────────┬────────────────────────┬───────────┘
           │ :8080/8082             │ :8081                  │ :11434
┌──────────▼───────────────┐ ┌──────▼───────────────┐ ┌──────▼───────────┐
│     MLX / MTPLX          │ │    llama-server      │ │      Ollama      │
│  Qwen3.8-27B 4-bit       │ │  Qwen3.8-27B Q8_0    │ │ Qwen3.8-27B GGUF │
│  (Native MTP ~45 tok/s)  │ │ (Quality / Spec Dec) │ │   (Fallback)     │
└──────────────────────────┴─┴──────────────────────┴─┴──────────────────┘
                                1. Inference Layer (Apple Silicon Macs)
```

---

## 2. Port Allocation & Gateway Map

| Component | Port | Interface | Purpose |
|---|---|---|---|
| **Loop Studio Router** | `:8000` | HTTP / OpenAI API | Unified reverse proxy, load balancer, hot-swap endpoint |
| **MLX Server** | `:8080` | HTTP / OpenAI API | Fast Q4 inference with native Multi-Token Prediction (MTP) |
| **llama-server** | `:8081` | HTTP / OpenAI API | High-fidelity Q8_0 inference, grammar constraints, fallback |
| **Ollama** | `:11434` | HTTP / OpenAI API | Local tool use & fallback runner |

OpenCode connects to `http://127.0.0.1:8000/v1`. The Loop Studio router transparently forwards requests to whichever engine is online, prioritizing MLX for speed, then llama-server for quality, then Ollama.

---

## 3. Unified Memory Budget (reference: 96GB Studio)

On Apple Silicon, unified memory is shared dynamically between CPU and GPU Metal cores. Worked example below is measured on a 96GB Studio; on other Macs the same rows apply with their own totals, and the code scales its gates automatically.

```
Reference Total Physical Memory: 96.0 GiB
┌───────────────────────────────┬─────────────────┬──────────────────────┐
│ Model Weights                 │ KV Cache        │ OS & Working Set     │
│ Q4_K_M: ~16.1 GiB             │ 32K ctx: ~8 GiB │ macOS 26: ~8-12 GiB  │
│ Q8_0  : ~29.5 GiB             │ 64K ctx: ~16 GiB│ Workspace: ~6 GiB    │
├───────────────────────────────┴─────────────────┴──────────────────────┤
│ Free Headroom: ~42.0 to 65.0 GiB (Wide open for compilation & tools)  │
└────────────────────────────────────────────────────────────────────────┘
```

- **Qwen3.8-27B (Q4 default)**: 16.1GB weights + 8GB KV (32K) + 12GB system = **~36GB total**. Over **60GB free** for compilation, IDE, and worktrees.
- **Qwen3.8-27B (Q8 quality)**: 29.5GB weights + 16GB KV (64K) + 12GB system = **~57.5GB total**. Over **38GB free**.
- **Working Context Cap**: 16K steady-state cap via `context-cap` skill; emergency truncation at 24K via `kv-manage`.

---

## 4. Process Supervision & Recovery

Inference servers are kept alive via Apple `launchd` daemons in `~/Library/LaunchAgents/`:
- `org.loopstudio.router.plist`: Supervises `loop-studio-router` on port `:8000`.
- `org.loopstudio.serve-mlx.plist`: Supervises `serve-mlx` with crash backoff and restart throttle.

In case of Metal OOM (`Insufficient Memory`), `loop-studio-router` returns HTTP 502 with error details, while `launchd` auto-restarts the inference backend within 10 seconds.

---

## 5. Native Orchestrator (Hermes)

Normative spec: `.opencode/skills/hermes-orchestrator/SKILL.md`. Summary:

There is no external Hermes process. `loop-studio hermes` and `loop-studio worker` are entry points into one native Rust loop (`cmd_worker` in `rust-src/src/loop_studio.rs`):

- **Selection**: `judge_next()` asks the `:8000` gateway to pick the most urgent pending task from `~/todo/loop-studio-tasks.yaml`, given thermal state. Single-candidate queues, a down gateway, `LOOP_STUDIO_JUDGE=off`, or any model/gateway failure all resolve to deterministic file order — judgment augments selection, never reliability.
- **Execution per task**: thermal gate → KV checkpoint (`kv-manage truncate`) → isolated `task/<id>` worktree → `opencode2 run` (@coder) → `make test` fail-closed gate → native reviewer pass (`review_diff`: `git diff HEAD` plus capped untracked contents, verdict derived from `[critical]`/`[major]` markers; one @coder fix round, max 2 review rounds) → commit on pass, stash/remove failed worktree + attempt bump on failure, dead-letter (`failed`) after 3 attempts. Reviewer infra failure never blocks a test-passing commit (event-logged); round-2 leftovers commit flagged `review_flagged` for human merge review. Panic-isolated via `catch_unwind`; single-flight locked; the operator tree is never checked out, reset, or cleaned during recovery.
- **Status**: `loop-studio hermes status` reports queue counts, gateway, thermals, and model from the live machine. `loop-studio hermes run [--drain]` delegates to the worker loop. `LOOP_STUDIO_JUDGE=off` / `LOOP_STUDIO_REVIEW=off` force deterministic skip of each LLM gate.

---

## 6. Remote Access via Tailscale (tailnet-only, no public ports)

`loop-studio-router` binds loopback by default. Remote Studio clients connect over the
tailnet (WireGuard-encrypted), never the public internet. No `:8000` ingress
rule on Oracle/VPS firewalls.

- Router: `LOOP_STUDIO_BIND_ADDR` (default `127.0.0.1`) + `LOOP_STUDIO_API_TOKEN`. A
  non-loopback bind without a token refuses to start (fail closed, in one
  check). When a token is configured it is required on every listener,
  including loopback, so a local reverse proxy cannot bypass the gate;
  forwarded proxy headers also require it. Requests without a valid
  `Authorization: Bearer` get 401 and are never forwarded. Token comparison
  is constant-time *and* length-safe, so a wrong-length guess costs the same
  as a right-length one. See `loop-studio config` (redacted) and `loop-studio doctor`
  (`remote_auth` check).
- Browser gates: a loopback gateway is reachable by every web page the user
  has open, so it refuses browser traffic by default rather than inheriting
  the web's attacker model.
  - `Host` is served only for IP literals and `localhost`; any other DNS name
    gets **421 Misdirected Request**. This is the DNS-rebinding control: an
    attacker who rebinds their domain to `127.0.0.1` gets a request the
    browser considers *same-origin*, so no `Origin` is sent and CORS never
    applies — only `Host` still carries their name. A valid Bearer token is
    the one exception, and it is proof rather than heuristic (a rebound
    request is same-origin to a page that has never seen the token), which is
    what keeps MagicDNS working on a token-protected gateway. Add names
    explicitly with `LOOP_STUDIO_ALLOWED_HOSTS`.
  - `Origin` must be on `LOOP_STUDIO_ALLOWED_ORIGINS` or the request gets **403
    Forbidden**; an allow-listed origin is echoed back exactly (never a
    wildcard the router picked) with `Vary: Origin`. A native client sends no
    `Origin` at all and therefore gets no CORS header on any response, so
    Studio, OpenCode, curl and the local SDKs are unaffected. `*` in
    `LOOP_STUDIO_ALLOWED_ORIGINS` re-opens every page and is called out in the startup
    banner.
  - Both gates run before the auth gate, so a refused request never reaches a
    status read, `/loop-studio/switch`, or an inference backend. Regression coverage:
    `router_auth` (`browser_origins_and_rebound_hosts_are_refused`,
    `operator_allowlists_and_a_token_reopen_exactly_what_they_name`).
- Studio: `LoopStudioConnectionStore` (`SwiftUI/.../LACConnection.swift`) — host
  (MagicDNS `mac.tailXXX.ts.net`), port, TLS toggle (required for remote
  profiles; use `tailscale serve --https`), Bearer token in Keychain
  (`org.loopstudio.studio`). All clients (chat, code, loops, dashboard) route through
  `connection.url(path:)` + `authorize(&req)`. The store rejects arbitrary
  public hosts and plaintext remote profiles before creating a request, and
  clears the token whenever the origin changes. Local serve/daemon/host-fact
  actions self-disable while remote.
- Ops: one tailnet, device approval + key expiry + Tailnet Lock on,
  ACL `laptops => mac:8000`. `launchctl setenv LOOP_STUDIO_API_TOKEN "$(openssl rand -hex 32)"`
  then `loop-studio daemon install`. A `tailscale serve` reverse proxy must preserve the
  Bearer header; configure the token even when the router itself listens on
  loopback. Oracle box is a plain tailnet member (status/ping/logs), never a public relay.
