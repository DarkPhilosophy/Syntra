# Lan Mouse Agent Instructions

## Overview

Lan Mouse is an open-source Software KVM sharing mouse/keyboard input across local networks. The desktop presentation uses Slint by default. GTK remains only in the separately packaged Linux clipboard helper.

## Core principles

- **Scope discipline.** Only implement what was requested; describe follow-up work instead of absorbing it.
- **Clarify OS behavior.** Ask when requirements touch OS-specific capture/emulation (they differ significantly).
- **Docs stay current.** Update [README.md](README.md) or [DOC.md](DOC.md) when touching public APIs or platform support.
- **Rust idioms.** Use `Result`/`Option`, `thiserror` for errors, descriptive logs, and concise comments for non-obvious invariants.

## Terminology

- **Client:** A remote machine that can receive or send input events. Each client is either _active_ (receiving events) or _inactive_ (can send events back). This mutual exclusion prevents feedback loops.
- **Backend:** OS-specific implementation for capture or emulation (e.g., libei, layer-shell, wlroots, X11, Windows, macOS).
- **Handle:** A per-client identifier used to route events and track state (pressed keys, position).

## Architecture

**Frontend pipeline:** service state → `lan-mouse-ipc` → frontend-neutral view models → Slint presentation.

The Slint UI is responsible for rendering snapshots and sending typed intents; it does not own
networking, capture, emulation, clipboard, transfer, or authorization state machines.

**Input pipeline:** `input-capture` → `lan-mouse-ipc` → `input-emulation`

- **input-capture:** Reads OS events into a `Stream<CaptureEvent>`. Backends tried in priority order (libei → layer-shell → X11 → fallback). Tracks `pressed_keys` to avoid stuck modifiers. `position_map` queues events when multiple clients share a screen edge.
- **input-emulation:** Replays events via the `Emulation` trait (`consume`, `create`, `destroy`, `terminate`). Maintains `pressed_keys` and releases them on disconnect.
- **lan-mouse-ipc / lan-mouse-proto:** Protocol glue and serialization. Events are UDP; connection requests are TCP on the same port. Version bumps required when serialization changes.
- **input-event:** Shared scancode enums and abstract event types—extend here, don't duplicate translations.

The Linux `lan-mouse-adapter-gtk-clipboard` executable is a runtime helper, not a GUI. Keep it
packaged on Linux until a verified replacement exists; the FUSE helper remains separate as well.

## Feature & cfg discipline

- Feature flags live in root `Cargo.toml`. Gate OS-specific modules with the configs exported in build.rs (e.g., `cfg(layer_shell)`).
- Prefer module-level gating over per-function cfgs to avoid empty stubs.
- New backends: add feature in `Cargo.toml`, create gated module, log backend selection.

## Async patterns

- Tokio runtime with `futures` streams and `async_trait`. Model new flows as streams or async methods.
- Avoid blocking; use `spawn_blocking` if needed. Prefer existing single-threaded stream handling.
- `InputCapture` implements `Stream` and manually pumps backends—don't short-circuit this logic.

## Commands

```sh
cargo build -p lan-mouse --no-default-features --features slint-ui,layer_shell_capture,x11_capture,libei_capture,wlroots_emulation,libei_emulation,rdp_emulation,x11_emulation  # Slint Linux build during cutover
cargo build -p <crate>                                     # single crate
cargo test -p lan-mouse-ui                                      # Slint UI tests
cargo fmt && cargo clippy -p lan-mouse --all-targets --no-default-features --features slint-ui,layer_shell_capture,x11_capture,libei_capture,wlroots_emulation,libei_emulation,rdp_emulation,x11_emulation  # lint
RUST_LOG=lan_mouse=debug cargo run -p lan-mouse --no-default-features --features slint-ui,layer_shell_capture,x11_capture,libei_capture,wlroots_emulation,libei_emulation,rdp_emulation,x11_emulation  # Slint runtime
```

Run from repo root—no `cd` in scripts.

## Testing

- Unit tests for utilities; integration tests for protocol behavior.
- OS-specific backends: test via the Slint frontend or CLI on the target OS, or document manual verification.
- Dummy backend exercises pipeline without real dependencies.
- Verify `terminate()` releases keys on unexpected disconnect.

## Workflow

1. Clarify ambiguous requirements, especially OS-specific behavior.
2. Implement minimal change; flag follow-up work.
3. Add proportional tests; run `cargo test` on affected crates.
4. Run `cargo fmt` and package-scoped `cargo clippy` with the selected feature set; Linux builds also include the separate clipboard and FUSE helpers.
