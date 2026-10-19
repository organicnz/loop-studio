# Contributing to Loop Studio

Thanks for your interest in improving Loop Studio. This document covers how to
get set up, what the project expects from a change, and how to get it reviewed.

## What this project is

Loop Studio is a **fully local agentic coding stack for Apple Silicon Macs**:
a pure-Rust inference gateway, a native macOS desktop app, and an autonomous
task-queue runner — all pointed at a model running on your own hardware.

Two properties are load-bearing and should not be traded away:

1. **Everything runs on your machine.** No telemetry, no hosted model calls
   in the default path. The gateway binds to loopback and refuses browser
   traffic by default.
2. **The backend is 100% pure Rust** — standard library only, zero external
   crates, zero Python, and zero shell scripts in the backend path. Please do
   not add a dependency to `rust-src/Cargo.toml` without discussion.

## Requirements

- macOS on Apple Silicon (13.0+)
- Rust toolchain (stable) — `cargo`
- Swift toolchain (Xcode 26+, macOS 26 SDK) — `swift`
- `mise` and `just`/`make` for the task runner
- `lefthook` for the git hooks

## Getting set up

```bash
git clone https://github.com/organicnz/loop-studio.git
cd loop-studio

# Build the Rust backend and the Swift app
make build

# Run everything: lint, unit tests, E2E smoke, signed-app check
make validate
```

`make install` copies the binaries into `~/.local/bin` under both their plain
and prefixed names (`loop-studio` and `loop-studio-serve-mlx`, etc.).

## The delivery gate

Every change goes through the same gate, locally and in CI:

```
make delivery-index validate
```

That runs, in order:

| Target          | What it checks                                                     |
| --------------- | ------------------------------------------------------------------ |
| `delivery-index`| Required gate files are actually tracked in git                      |
| `lint`          | `cargo clippy -D warnings` + `swift build -Xswiftc -warnings-as-errors` |
| `test`          | Full Rust and Swift unit suites                                    |
| `smoke`         | Process-level gateway/CLI end-to-end smoke test                     |
| `release-smoke` | The same E2E against **release** binaries                           |
| `app-check`     | Packages, signs, and verifies the `.app` bundle                    |

`lefthook` runs the same gate on pre-commit and pre-push, and GitHub Actions
runs it on every push and pull request.

**Do not weaken a test to make it pass.** If a test encodes the wrong
expectation, say so in the PR and let a human decide. The test suite is the
project's specification of its own honesty guarantees — several of the tests
exist specifically to catch a change that would report success for work that
did not happen.

## How to make a change

1. **Open an issue first** for anything non-trivial, so the approach can be
   agreed before the work is done.
2. **Branch from `main`**: `git switch -c feature/<short-description>`.
3. **Keep the change scoped.** No drive-by reformatting, no unrelated files in
   a diff, no dependency bumps in a bug fix.
4. **Match the surrounding style** — the same naming, the same comment
   density, the same error-handling conventions. Read the file before editing
   it.
5. **Add or update a test** for the behaviour you changed. New behaviour
   without a test is a behaviour nobody can rely on.
6. **Run the gate**: `make validate`.
7. **Open a PR** describing what changed and why. Reference the issue.

## Code conventions

### Rust backend (`rust-src/`)

- Standard library only. No crates, no `unsafe` without a written justification.
- Every network path is a plain `TcpStream` with `SIGPIPE` ignored, and every
  thread is panic-isolated so one bad request cannot take down the gateway.
- Never report success for work that did not happen. A command that failed
  must exit nonzero and say so.
- User-facing output goes through the `common` helpers rather than being
  formatted inline, so it stays consistent.

### Swift app (`SwiftUI/`)

- Swift 6 language mode, `async`/`await` throughout, no blocking calls on the
  main actor.
- `LiquidGlass` holds the shared visual vocabulary; reuse it rather than
  re-deriving a gradient or a spring.
- The app and the CLI are two implementations of one contract. When you change
  a file format, a path, or a lock name, change **both** sides and update the
  parity fixture (`docs/lock-parity.tsv`) if it covers the change.

### Naming and identifiers

The project was renamed from `LAC` to **Loop Studio**. The current names are:

| Thing                | Name                                        |
| -------------------- | ------------------------------------------- |
| Repository           | `loop-studio`                               |
| CLI                  | `loop-studio`                               |
| Gateway              | `loop-studio-router`                        |
| TUI                  | `loop-studio-tui`                           |
| macOS app            | `Loop Studio.app`                           |
| Swift module         | `LoopStudio`                                |
| Environment prefix   | `LOOP_STUDIO_`                              |
| Task queue           | `~/todo/loop-studio-tasks.yaml`             |
| Loop definitions     | `~/todo/loop-studio-loops/`                 |
| LaunchAgent labels   | `org.loopstudio.*`                          |
| HTTP control routes  | `/loop-studio/status`, `/loop-studio/switch` |
| Log files            | `~/Library/Logs/loop-studio-*.log`          |

Pre-rename names are still accepted where accepting them is free — the CLI
reads the old environment variables as a fallback, the router still answers
the old `/lac/*` routes, and the app still reads the old `UserDefaults` keys.
**Do not add new pre-rename aliases**; that is a compatibility decision, not
a naming one.

## Reporting bugs

Please include:

- What you ran and what you expected to happen.
- What happened instead, with the exact output.
- Your macOS version, chip, and RAM (they materially change the behaviour —
  the stack is RAM-pressure aware).
- `loop-studio doctor` output, which is the project's own diagnostic.

Security issues: please do not open a public issue first. Use GitHub's
private vulnerability reporting for the repository.

## Code of conduct

Be decent to each other. Assume good faith, critique the code rather than the
person, and remember that this project is meant to be pleasant to work on.
