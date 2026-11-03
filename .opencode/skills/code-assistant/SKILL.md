---
name: Code Assistant
description: Agentic code reasoning, refactoring, test generation, and bug audits via Loop Studio & CLI
---

## What it does

The **Code Assistant** provides dedicated, autonomous systems engineering and programming capabilities:
- **Refactoring & Modernization**: Refactors functions and structs to be clean, idiomatic, memory-safe, and zero-allocation.
- **Test Generation**: Generates comprehensive unit tests with edge-case and boundary verification.
- **Bug & Vulnerability Auditing**: Finds race conditions, memory leaks, panics, deadlocks, and unsafe bounds operations.
- **Apple Silicon Optimization**: Analyzes hot paths for cache locality, SIMD vectorization, and unified memory alignment.

## Usage Modes

### 1. In Loop Studio (macOS Native App)

1. Open **Loop Studio**.
2. Click **Code Assistant** in the sidebar under `WORKSPACE` (or press `⌘2`).
3. Paste or type your code into the left **Code Canvas** (supports Rust, Swift, Python, TypeScript, Go, C++, Shell, SQL).
4. Click any quick action capsule at the top:
   - `[Explain Code]`
   - `[Refactor / Clean]`
   - `[Generate Tests]`
   - `[Audit Bugs & Safety]`
   - `[Optimize (SIMD / Cache)]`
5. Or type custom instructions in the bottom prompt bar.
6. Click **"Apply to Editor"** to immediately merge refactored code back into the source canvas with 1-click undo.

### 2. From Terminal via Pure Rust CLI (`loop-studio code`)

Run agentic coding prompts directly from your shell without opening an editor:

```bash
# Review and refactor an existing file
loop-studio code -f src/main.rs "Refactor to be thread-safe with zero allocations"

# Generate unit tests
loop-studio code -f src/lib.rs "Generate comprehensive tests covering empty inputs and boundary conditions"

# Pipe code from stdin
cat rust-src/src/loop_studio_router.rs | loop-studio code "Audit socket error handling and ensure SIGPIPE safety"

# Direct implementation instruction
loop-studio code "Write a lock-free multi-producer single-consumer ring buffer in Rust"
```

## Model Selection

- Default: `mlx-community/Qwen3.8-27B-4bit`
- Recommended for pure coding: `Qwen/Qwen2.5-Coder-32B-Instruct` or `mlx-community/Qwen2.5-Coder-32B-Instruct-4bit`
- Override via environment variable:
  ```bash
  export LOOP_STUDIO_CODE_MODEL="mlx-community/Qwen2.5-Coder-32B-Instruct-4bit"
  ```

## Project-specific conventions (Loop Studio)

The Code Assistant is pre-grounded in these rules when working on this repo:

### Backend (Rust)
- **Zero external crates.** `std` only. Never add a `Cargo.toml` dependency. No serde, tokio, anyhow, clap, reqwest, etc.
- **Every network path is a plain `TcpStream`** with `common::ignore_sigpipe()` at process start and `std::panic::catch_unwind` on every spawned thread. No async runtime.
- **State directory**: `~/.loop-studio` (via `common::state_dir()`). Never write `~/.lac` — that name is retired.
- **No bare `unwrap()`/`expect()` in non-test daemon paths.** Use `unwrap_or_default()`, `match`, or `if let`.
- **JSON string values** in `format!()` must use `common::json_escape()`. Timestamps via `common::now_unix()`.
- **Port resolution** goes through `common::gateway_port()`, `common::mlx_port()`, etc. — never hardcoded integers.

### Swift (macOS app)
- **Swift 6 language mode.** All types that cross actor boundaries must be `Sendable`. No blocking calls on `@MainActor`.
- **One gateway client.** `GatewayClient` owns all URL construction, auth, SSE decoding. Chat/Code/Loops import `GatewayEvent` and `GatewayReply`.
- **Lock parity**: queue lock path and log names are shared with Rust — see `docs/lock-parity.tsv`. If one side changes, both change.

### Delivery gate
```bash
make validate   # lint + test + smoke + Swift test + app-check — must be green before commit
```
