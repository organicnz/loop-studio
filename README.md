# Loop Studio — Local Agentic Coding (Apple Silicon Macs)

**A fully local AI coding stack for Apple Silicon Macs.** Loop Studio runs a
27B-parameter model on your own hardware and wires it to a native macOS app,
an unbreakable local inference gateway, and an autonomous task-queue runner —
with no hosted model calls and no telemetry in the default path.

Everything is designed around two properties that are not negotiable:

- **Your code and your prompts never leave the machine.** The gateway binds to
  loopback, refuses browser traffic by default, and authenticates anything
  exposed beyond it.
- **The backend is 100% pure Rust** — standard library only, zero external
  crates, zero Python, and zero shell scripts in the backend path.

Target: **Apple Silicon Macs** (8GB+ supported; best on high-RAM Studio + TB5 external).
Primary Model: **Qwen 3.8 27B** dense hybrid VLM with native MTP speculative decoding (~45 tok/s).
Harness: **OpenCode V2** (@coder, @reviewer, @orchestrator).
Orchestrator: **Hermes** + Kanban Loops (`~/todo/loop-studio-loops/`).
Gateway: **`loop-studio-router` on `:8000`** (auto-routes MLX `:8080`, llama-server `:8081`, Ollama `:11434`).

---

## What's in the box

| Component              | What it does                                                                                     |
| ---------------------- | ------------------------------------------------------------------------------------------------ |
| **Loop Studio.app**    | Native macOS app: chat, a split-pane Code Assistant, an HF model browser, and a live ops dashboard   |
| **`loop-studio`**      | The CLI: `status`, `serve`, `route`, `code`, `doctor`, `loop`, `worker`, `bench`, `kv`              |
| **`loop-studio-router`**| The gateway on `:8000` — hot-swappable, auto-routing, panic-isolated, and honest about what it measured |
| **`loop-studio-tui`**  | A full terminal control centre for the whole stack                                                   |
| **Hermes / worker**    | A judgment-augmented queue drain that runs a task on an isolated branch, gates on tests, and reviews the diff |
| **LaunchAgents**       | `org.loopstudio.*` — the router, the MLX engine, and the worker, kept alive by `launchd`            |

### The gateway is the interesting part

`loop-studio-router` is an OpenAI-compatible reverse proxy that fronts several
local inference engines and picks the best one per request:

- **EWMA latency learning** — it measures each backend and reorders by observed speed.
- **A circuit breaker** — 3 strikes in 30s trips a lane off, so a dead backend costs you one failover, not a hung request.
- **Per-request accounting** — tokens and bytes are charged once per request, to the request that actually happened.
- **Host and Origin gates** — DNS-rebinding and drive-by browser reads are refused by default.
- **Honest answers** — with every backend down it says `"no-backends"`, not `"ok"`.

---

## 10x Pro Quickstart

```bash
# 1. System diagnostics & status
./rust-src/target/release/loop-studio doctor
./rust-src/target/release/loop-studio status

# 2. Start the unified gateway on :8000
./rust-src/target/release/loop-studio route --daemon

# 3. Start high-speed inference (MLX with native MTP)
./rust-src/target/release/loop-studio serve mlx

# 4. Or launch the interactive Terminal UI (TUI)
./rust-src/target/release/loop-studio tui

# 5. Run OpenCode against the unified local gateway
opencode2 run "Verify stack: list files and check git status"
```

### Remote via Tailscale (no public ports)
```bash
# On the Mac: join tailnet, set token, keep router on loopback, front it with TLS
brew install tailscale && sudo brew services start tailscale && tailscale up
export LOOP_STUDIO_API_TOKEN="$(openssl rand -hex 32)"
launchctl setenv LOOP_STUDIO_API_TOKEN "$LOOP_STUDIO_API_TOKEN"
tailscale serve --https 443 http://127.0.0.1:8000
# On any laptop: same tailnet, Studio → Ops → Gateway Connection →
# host mac.tailXXX.ts.net:443 + enable TLS + paste token, Test.
./rust-src/target/release/loop-studio config   # bind + auth (redacted)
./rust-src/target/release/loop-studio doctor   # remote_auth check
```

---

## Architecture & Layout

- `SwiftUI/` — **Loop Studio** native macOS desktop app (Apple Silicon native):
  - **Apple Liquid Glass**: Optically accurate specular refraction borders, ambient light sheen, dual-layer visionOS depth shadows, 120Hz ProMotion interpolating springs, and trackpad tactile haptics.
  - **Claude & Codex Ergonomics**: 780pt centered golden-ratio reading column, sparkle monogram avatar, syntax-highlighted code containers with animated copy buttons, collapsible sidebar (`⌘B`), `+ New Chat` (`⌘N`), and floating elevated bottom composer.
  - **Code Assistant (`⌘2`)**: Split coding canvas with multi-language editor on the left and agentic assistant on the right. Quick actions for `Explain`, `Refactor`, `Generate Tests`, `Audit Bugs`, and `Optimize`, with 1-click apply and undo.
  - **LM Studio Hugging Face Hub**: Live debounced search across 100,000+ HF models, Apple Silicon / MLX / GGUF filter capsules, unified memory RAM fit estimation, and 1-click model switching.
  - **Live Ops Dashboard**: Real-time daemon monitoring, one-click backend switches, port health telemetry, and self-healing error recovery.
- `rust-src/` — **100% Pure Native Rust Backend** (std library only, zero external crates, zero Python/shell scripts in the backend path):
  - `loop-studio` — Swiss-army-knife CLI (`status`, `serve`, `route`, `code`, `doctor`, `loop`, `bench`, `kv`)
  - `loop-studio-router` — Unbreakable intelligent reverse proxy on `:8000` (SIGPIPE-immune, thread panic-isolated, auto-routes MLX `:8080`, llama `:8081`, Ollama `:11434`)
  - `loop-studio-tui` — Full-featured interactive ANSI terminal control center
  - `bootstrap` — Idempotent machine setup and volume provisioning
  - `serve-mlx` — MLX-LM server launcher with port drift detection and MTP on `:8080`
  - `serve-llama` — llama-server with Flash Attention & Q8_0 on `:8081`
  - `pull-models` — Automated Hugging Face model puller with RAM fit gating
  - `kv-manage` — Context cache risk analyzer & truncation engine
- `opencode.jsonc` — OpenCode V2 project config (gateway `:8000`, 64K context, 14 skills, qwen3.8-27b primary)
- `templates/` — Production Kanban queue (`loop-studio-tasks.yaml`) & loops (`daily-coding`, `bug-fix`, `feature-branch`, `security-audit`)
- `.opencode/skills/` — 14 skills including `hermes-orchestrator`, `loop-orchestrator`, `kv-cache-manage`, `thermal-monitor`
- `launchd/` — KeepAlive daemons for `loop-studio-router` (`org.loopstudio.router.plist`) and `serve-mlx` (`org.loopstudio.serve-mlx.plist`)
- `docs/` — Technical blueprints (`ARCHITECTURE.md`, `MTP_TUNING.md`, `LOOPS_AND_KANBAN.md`)

---

## Contributing

Contributions are welcome — see [CONTRIBUTING.md](CONTRIBUTING.md) for the
setup, the delivery gate (`make validate`), and the project's conventions.
There is an [open-issues list](https://github.com/organicnz/loop-studio/issues)
for anyone looking for a starting point.

## License

[MIT](LICENSE) — see the file for the full text.

## Documentation

- [Operating Rules](docs/AGENTS.md)
- [Quickstart Guide](docs/QUICKSTART.md)
- [Architecture Blueprint](docs/ARCHITECTURE.md)
- [Speculative Decoding & MTP](docs/MTP_TUNING.md)
- [Kanban & Autonomous Loops](docs/LOOPS_AND_KANBAN.md)
- [Terminal Selection Guide](docs/TERMINALS.md)
- [Contributing](CONTRIBUTING.md)
