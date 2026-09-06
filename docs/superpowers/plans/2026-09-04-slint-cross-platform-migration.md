# Lan Mouse Slint Cross-Platform Migration Plan

> Approved design: `docs/superpowers/specs/2026-09-04-slint-cross-platform-redesign.md`

## Goal

Replace the GTK/libadwaita user interface with a professional Slint frontend while preserving the current service, IPC, input, clipboard, file-transfer, and network behavior. The final frontend must share one Rust/Slint architecture across Linux, Windows, macOS, and Android; adapt its layout by capability and viewport; switch language live; and remove the old GTK frontend after feature parity.

## Non-goals and invariants

- Do not change the clipboard, FUSE, transfer protocol, input capture/emulation, peer protocol, or multi-layered control semantics during the UI migration.
- `lan-mouse-ipc` remains the authoritative frontend/service boundary.
- `Service` remains the sole source of domain truth; the Slint frontend never invents optimistic domain state.
- Do not permanently maintain two frontends. GTK exists only as a temporary parity reference.
- The Linux `adapters/gtk-clipboard` helper is not the GUI. Its replacement is a separate backend migration and must not be silently conflated with the frontend cutover.
- No WebView, HTML, JavaScript, Tauri, egui, or iced runtime is introduced.

## Target crate structure

Create these focused modules rather than putting platform logic in `.slint` files:

```text
lan-mouse-ui/
  Cargo.toml
  build.rs
  src/
    lib.rs
    app.rs
    bridge.rs
    models.rs
    localization.rs
    settings.rs
    platform/
      mod.rs
      linux.rs
      macos.rs
      windows.rs
      android.rs
  ui/
    app-window.slint
    theme.slint
    shell/
      navigation.slint
      title-bar.slint
      responsive-shell.slint
    components/
      status-pill.slint
      setting-row.slint
      empty-state.slint
      toast.slint
      confirmation-dialog.slint
    pages/
      overview.slint
      devices.slint
      input.slint
      clipboard.slint
      transfers.slint
      diagnostics.slint
      settings.slint
  locales/
    en-US.ftl
```

Names may be adjusted to match Slint compiler constraints, but responsibilities must remain separated.

## Phase 1 — Freeze and characterize the frontend contract

### Task 1.1: Add frontend contract characterization tests

**Files**
- `lan-mouse-ipc/src/lib.rs`
- Existing IPC test module/files, or a focused new test module beside the IPC implementation

**Work**
- Characterize serialization and round-trip behavior for every current `FrontendEvent` and `FrontendRequest` variant used by the UI.
- Include clipboard settings/status, client state with `peer_commit`, authorization, device connection/entry, capture/emulation status, port changes, and transfer cancellation.
- Test observable serialized contracts and round trips, not Rust field-copy plumbing.

**Verification**
- Run the affected `lan-mouse-ipc` tests.
- Prove old GTK and a minimal test consumer can both decode the same event stream.

### Task 1.2: Inventory dynamic GTK behavior as parity fixtures

**Files**
- `lan-mouse-gtk/src/lib.rs`
- `lan-mouse-gtk/src/window.rs`
- `lan-mouse-gtk/src/window/imp.rs`
- `lan-mouse-gtk/src/client_row.rs`
- `lan-mouse-gtk/src/client_row/imp.rs`
- `lan-mouse-gtk/src/authorization_window.rs`
- `lan-mouse-gtk/src/fingerprint_window.rs`

**Work**
- Convert the existing event-to-view behavior into a written parity table inside the migration branch/PR description or test fixtures, not duplicate runtime abstractions.
- Record each `FrontendEvent`, resulting visible state, relevant action, empty/error/loading state, and platform gate.
- Record all hardcoded dynamic text currently produced in Rust so it enters localization dictionaries.

**Acceptance**
- Every event/request in `lan-mouse-ipc` has an explicit Slint destination or is documented as service-only.

## Phase 2 — Introduce frontend-neutral presentation state

### Task 2.1: Create stable Rust view models

**Files**
- New `lan-mouse-ui/src/models.rs`
- New `lan-mouse-ui/src/bridge.rs`
- `lan-mouse-ipc/src/lib.rs` only if a genuinely missing domain datum is proven

**Work**
- Define frontend-facing Rust models for navigation, clients, authorization keys, input health, clipboard settings, transfer rows, diagnostics, and application status.
- Keep wire types at the bridge boundary; map them once into UI models.
- Use stable identifiers (`ClientHandle`, transfer IDs, fingerprints), never list indexes, for callbacks.
- Represent loading, ready, empty, warning, error, disabled-by-capability, and reconnecting states explicitly.
- Centralize reducer-style application of `FrontendEvent` to view state so out-of-order or repeated authoritative events are deterministic.

**Acceptance**
- Reducer tests demonstrate create/update/delete/enumerate ordering, resync replacement, clipboard setting updates, transfer lifecycle updates, and disconnect/reconnect behavior.

### Task 2.2: Define typed user intents and a transport-neutral bridge

**Files**
- New `lan-mouse-ui/src/bridge.rs`
- New `lan-mouse-ui/src/app.rs`
- `lan-mouse-ipc/src/lib.rs`
- `lan-mouse-ipc/src/connect.rs`
- `lan-mouse-ipc/src/connect_async.rs`

**Work**
- Define a narrow `UiIntent` enum or equivalent typed command boundary.
- Map each UI action exactly once to the existing `FrontendRequest`: activate/create/change port/delete/enumerate/resolve/update client fields/enable capture or emulation/sync/authorize/remove key/save/set clipboard toggles/cancel transfer.
- Define the bridge against a transport-neutral asynchronous contract: an ordered `Stream<FrontendEvent>` plus a `Sink<FrontendRequest>` (or equivalent traits), with connection lifecycle/error semantics made explicit.
- Implement the desktop transport with the existing Unix-socket/Windows-TCP IPC client, but keep `FrontendEventReader`, `FrontendRequestWriter`, Unix sockets, and TCP types out of Slint components and reducers.
- Provide an in-memory transport for reducer/intent tests and as the architectural basis for the Android in-process service host.
- Add an explicit `target_os = \"android\"` IPC/config branch. Android must not fall through to the non-macOS Unix `$XDG_RUNTIME_DIR` socket path.
- Establish the Android process model now: the service core runs in-process under an Android foreground-service/lifecycle host; the UI bridge communicates through bounded in-process channels. Capture/emulation capabilities that have no Android backend are reported authoritatively as unavailable, not emulated or silently enabled.
- Provide explicit result handling for local-only operations such as opening settings or granting platform permission.

**Verification**
- Unit-test intent-to-request mapping for meaningful actions and validation boundaries.
- Run reducer and request/event ordering tests through the in-memory transport without creating a socket.
- Prove the desktop adapter preserves synthetic `Sync`, ordered initial snapshot delivery, disconnect, and reconnect semantics.

## Phase 3 — Add Slint build and theme foundation

### Task 3.1: Add the Slint frontend crate

**Files**
- Root `Cargo.toml`
- New `lan-mouse-ui/Cargo.toml`
- New `lan-mouse-ui/build.rs`
- `lan-mouse-ipc/src/lib.rs`
- `Cargo.lock`

**Work**
- Add `lan-mouse-ui` to the workspace.
- Add pinned compatible `slint` and `slint-build` versions using the minimum features needed per platform.
- Depend on `lan-mouse-ipc`, logging, serialization/localization crates, and only narrowly justified platform crates.
- Compile the root `.slint` entry in `build.rs` and rerun when `.slint`, locale, or asset files change.
- Introduce a temporary root feature such as `slint-ui` without making it default until parity is proven.

**Verification**
- Build the new crate alone on Linux.
- Compile-check target-gated Rust for Windows and macOS.
- Make `cargo check -p lan-mouse-ipc -p lan-mouse-ui --target aarch64-linux-android` pass in this phase, before Android UI pages or packaging are built.
- Establish the Android toolchain job before claiming platform support.

### Task 3.2: Implement design tokens and reusable primitives

**Files**
- `lan-mouse-ui/ui/theme.slint`
- `lan-mouse-ui/ui/components/*.slint`

**Work**
- Encode colors, spacing, typography, radius, elevation, motion durations, hit areas, and density as named tokens.
- Build accessible reusable setting rows, state badges, buttons, toggles, empty/error states, confirmation dialogs, toasts, and progress rows.
- Ensure focus visuals, keyboard traversal, reduced-motion behavior, readable contrast, and minimum interaction targets.
- Avoid unstructured per-page color/spacing literals.

**Verification**
- Use Slint live preview for desktop widths and narrow/mobile widths.
- Capture screenshots for light/dark themes and high-content states.

## Phase 4 — Implement responsive shell and live localization

### Task 4.1: Build collapsible navigation shell

**Files**
- `lan-mouse-ui/ui/app-window.slint`
- `lan-mouse-ui/ui/shell/navigation.slint`
- `lan-mouse-ui/ui/shell/responsive-shell.slint`
- `lan-mouse-ui/src/settings.rs`

**Work**
- Implement left navigation with icon + localized label in expanded mode.
- `<` collapses it to icons; `>` expands it. Preserve the explicit user preference across desktop launches.
- At narrow widths, automatically use compact navigation without corrupting the stored desktop preference.
- Provide tooltip/accessibility labels for icon-only entries.
- Main content changes without recreating the entire application state.
- Model desktop and mobile shells with shared page components, not two copied UIs.

**Behavioral checks**
- Resize across the breakpoint while a page, search query, console scroll position, and modal are active.
- Verify keyboard navigation and focus restoration after collapse/expand.

### Task 4.2: Add live dictionary localization

**Files**
- `lan-mouse-ui/src/localization.rs`
- `lan-mouse-ui/locales/en-US.ftl`
- `lan-mouse-ui/src/settings.rs`
- `lan-mouse-ui/ui/pages/settings.slint`
- `src/config.rs` if locale is persisted in the shared config

**Work**
- Use Fluent-style locale resources with stable semantic message IDs.
- Put all visible strings in dictionaries: navigation, settings, status text, toasts, dialogs, transfer state, errors, empty states, diagnostics labels, accessibility labels, and platform permission copy.
- Expose a reactive translation model/property to Slint.
- Switching language rebuilds the translated property set live without restarting and without losing navigation/page state.
- Fall back per message to bundled `en-US`, log missing IDs once, and never display raw keys to users.
- Persist locale selection backward-compatibly; unknown or removed locales fall back safely.
- Support plural/select arguments and locale-aware numbers/sizes/timestamps from the start.

**Verification**
- Add a small pseudo-locale or test dictionary to exercise text expansion and missing-key fallback.
- Switch language with dialogs and transfer rows visible; verify every visible string updates.

## Phase 5 — Implement pages against authoritative models

### Task 5.1: Overview and input health

**Files**
- `lan-mouse-ui/ui/pages/overview.slint`
- `lan-mouse-ui/ui/pages/input.slint`
- `lan-mouse-ui/src/models.rs`

**Work**
- Present current capture/emulation health, connected devices, clipboard state, and recent actionable warnings.
- Preserve Enable Capture/Enable Emulation recovery flows.
- Distinguish unavailable capability, disabled setting, pending authorization, and actual runtime failure.

### Task 5.2: Devices and client editing

**Files**
- `lan-mouse-ui/ui/pages/devices.slint`
- Supporting components and models

**Work**
- Port create/delete/activate, hostname, port, position, fixed IP, DNS resolution, peer commit mismatch, and enter hook controls.
- Preserve stable-handle routing when rows reorder or disappear.
- Use inline validation and retain edits after recoverable failures.
- Preserve multi-layered control behavior; do not introduce unified control in this migration.

**Verification**
- Exercise two clients with concurrent state changes and deletion/reordering.

### Task 5.3: Clipboard and transfer pages

**Files**
- `lan-mouse-ui/ui/pages/clipboard.slint`
- `lan-mouse-ui/ui/pages/transfers.slint`

**Work**
- Port independent text/image/file switches.
- Port transfer progress, direction, completed/total bytes, terminal state, error, and cancel action.
- Ensure a transfer uses one stable row through its lifecycle and cancellation disappears after terminal state.
- Keep Flatpak file-manager permission actions capability-gated to Linux; do not hardcode them into shared page layout.

**Verification**
- Feed simulated offered/transferring/completed/failed/cancelled states, including multiple simultaneous files and unknown totals.

### Task 5.4: Diagnostics console

**Files**
- `lan-mouse-ui/ui/pages/diagnostics.slint`
- `lan-mouse-ui/src/models.rs`
- `lan-mouse-ui/src/platform/linux.rs` or a frontend-neutral log source module

**Work**
- Preserve timestamp, level, source, stage, direction, text search, pause/resume, clear, bounded history, and bottom-follow behavior.
- Do not poll and reread a file. Retain the live event source and move platform transport details out of UI components.
- New events auto-scroll only when already following the bottom; manual upward scrolling remains stable.
- Bound memory and update batches to avoid rendering work per log line during bursts.

**Verification**
- Run a burst scenario and visually verify filtering, pause/resume, clear, and scroll anchoring.

### Task 5.5: Authorization and settings flows

**Files**
- `lan-mouse-ui/ui/pages/settings.slint`
- `lan-mouse-ui/ui/components/confirmation-dialog.slint`
- Relevant platform modules

**Work**
- Port fingerprint authorization/removal with explicit confirmation and complete fingerprint/description context.
- Port port editing/save configuration and application preferences.
- Add language, theme, density, and navigation collapse settings without mixing them into service-owned state.
- Show actionable errors and preserve user input.

## Phase 6 — Isolate platform integrations

### Task 6.1: Define capability/action adapter

**Files**
- `lan-mouse-ui/src/platform/mod.rs`
- Platform-specific modules

**Work**
- Define narrow capabilities such as tray availability, background close behavior, accessibility/input permission, Flatpak grants, native menu/status item, notifications, and mobile lifecycle.
- Shared UI renders from capabilities and sends actions; it never imports platform APIs.
- Unsupported actions are absent/disabled with a localized explanation, never silent no-ops.

### Task 6.2: Port Linux integration

**Source reference**
- `lan-mouse-gtk/src/linux_tray.rs`
- Linux-only Flatpak grant code in `lan-mouse-gtk/src/window/imp.rs`

**Target**
- `lan-mouse-ui/src/platform/linux.rs`

**Work**
- Preserve tray open/quit and close-to-background behavior.
- Preserve detected Flatpak application permission actions with exact command result/error reporting.
- Localize tray labels.

### Task 6.3: Port macOS integration

**Source reference**
- `lan-mouse-gtk/src/macos_privacy.rs`
- `lan-mouse-gtk/src/macos_status_item.rs`
- `build-aux/macos-lsui-element.plist`

**Target**
- `lan-mouse-ui/src/platform/macos.rs`
- A shared platform crate if input backends also need the permission helper

**Work**
- Move permission querying/open-settings/relaunch logic out of GTK/GLib.
- Preserve status item/menu and close-to-background behavior without GTK.
- Remove stale comments in `input-capture/src/macos.rs` and `input-emulation/src/macos.rs` that point to GTK ownership.
- Keep privacy usage descriptions in bundle metadata and localize runtime explanations.

### Task 6.4: Implement Windows and Android lifecycle adapters

**Files**
- `lan-mouse-ui/src/platform/windows.rs`
- `lan-mouse-ui/src/platform/android.rs`
- Android foreground-service/lifecycle host files selected from current supported Slint guidance
- New Android packaging/project files selected from current supported Slint guidance

**Work**
- Windows: native window lifecycle, tray/background semantics where supported, platform permission/capability presentation.
- Android: connect the already-defined in-process bridge to the foreground-service/lifecycle host; do not introduce a second transport or retrofit desktop sockets.
- Implement activity lifecycle, touch-first navigation, safe areas, back handling, suspend/resume/reconnect, and capability-limited pages.
- Keep the service alive according to Android foreground-service rules while network functionality is active, with an honest persistent notification and explicit stop path.
- Do not pretend unavailable desktop input features exist on Android; expose accurate capability states while reusing networking/configuration pages where viable.

**Verification**
- Real Windows/macOS builds and smoke runs.
- Android emulator/device build, launch, rotation/resizing, background/resume, service stop/restart, and in-process bridge reconnection.

## Phase 7 — Integrate startup while retaining rollback

### Task 7.1: Add temporary Slint startup feature

**Files**
- `src/main.rs`
- Root `Cargo.toml`
- `lan-mouse-ui/src/lib.rs`

**Work**
- On Linux, Windows, and macOS, add a Slint error variant and call `lan_mouse_ui::run(config::local_commit())` in the same daemon-child lifecycle currently used by GTK.
- On Android, use the in-process foreground-service host and channel transport established in Tasks 2.2 and 6.4; never spawn the desktop daemon child or require `$XDG_RUNTIME_DIR`.
- Preserve graceful Unix SIGINT/wait behavior and correct Windows/macOS shutdown.
- Fix the existing unconditional Unix-only `UnixDatagram` import/path code so the root binary is truly cross-platform.
- Keep GTK selectable only during parity validation; prevent both frontends being enabled simultaneously or define deterministic precedence with a compile error.

**Verification**
- Launch the actual desktop application, connect through IPC, close/reopen UI while daemon runs, and quit fully.
- Launch Android with the in-process host, background/restore the activity, and prove ordered state resynchronization without a socket.

### Task 7.2: Run parity matrix

**Platforms**
- Linux first, then Windows, macOS, Android capability subset

**Scenarios**
- Initial sync and daemon unavailable/reconnect.
- Add/edit/resolve/activate/delete clients.
- Capture/emulation health and recovery.
- Authorization add/remove and connection attempts.
- Clipboard toggles and real text/image/file behavior unchanged.
- Transfer progress/cancel/error terminal states.
- Diagnostics filters and scroll behavior.
- Locale change live, theme/density, responsive navigation.
- Platform tray/status/permission actions.

**Evidence**
- Use actual UI smoke tests and screenshots/accessibility evidence; do not substitute unit tests for application behavior.

## Phase 8 — Clean cutover and remove GTK frontend

### Task 8.1: Make Slint the only frontend

**Files**
- Root `Cargo.toml`
- `src/main.rs`
- Workspace membership
- `Cargo.lock`

**Work**
- Make Slint default and remove the GTK frontend feature/dependency/error/startup branch.
- Remove `lan-mouse-gtk` from the workspace and delete its Rust/UI/resource files only after parity proof.
- Remove obsolete aliases, compatibility paths, and GTK-specific frontend comments.

### Task 8.2: Decide and execute GTK clipboard-helper cutover separately

**Files**
- `adapters/gtk-clipboard/**`
- `src/adapter_manager.rs`
- `src/transfer_manager.rs`
- `src/service.rs`

**Work**
- Explicitly choose one of two truthful outcomes:
  1. Retain the Linux GTK clipboard adapter as a documented backend helper, meaning the frontend is Slint but the Linux build still has a GTK backend dependency; or
  2. Replace it with a platform-neutral/native clipboard adapter, migrate all hardcoded `Gtk`/`gtk-clipboard` IDs and paths, then remove GTK from the dependency graph.
- Because the approved final criterion says no GTK/libadwaita frontend dependency, outcome 1 is acceptable only if documentation clearly distinguishes backend helper from frontend. If the desired release criterion is zero GTK libraries anywhere, outcome 2 is mandatory and is a separate behavior-sensitive project.

**Verification**
- Run the real clipboard/file-transfer matrix before and after any helper replacement.

## Phase 9 — Packaging, CI, documentation, and release proof

### Task 9.1: Update assets and Linux packaging

**Files**
- `build-aux/de.feschber.LanMouse.yml`
- `nix/default.nix`
- `flake.nix`
- `de.feschber.LanMouse.desktop`
- New Slint asset paths

**Work**
- Replace GTK frontend runtime/SDK/finish/build dependencies and icon paths without accidentally removing runtime libraries still required by a deliberately retained clipboard helper.
- Explicitly build and install every helper required by `AdapterPaths::new`, including `lan-mouse-adapter-fuse` and, while retained, `lan-mouse-adapter-gtk-clipboard`.
- Install helper executables adjacent to `lan-mouse`, or change `AdapterPaths` and packaging together to a documented platform-appropriate libexec location. Never rely on binaries left in `target/release`.
- Package Slint runtime/assets correctly.
- Preserve daemon/systemd/Home Manager integration.
- Localize desktop metadata with supported platform conventions.
- Launch the installed Flatpak and Nix artifacts and prove the service resolves both helper paths before running clipboard/file-transfer smoke tests.

### Task 9.2: Update Windows/macOS packaging

**Files**
- `.github/workflows/release.yml`
- `.github/workflows/rust.yml`
- `scripts/makeicns.sh`
- `scripts/copy-macos-dylib.sh`
- Root bundle metadata

**Work**
- Remove GTK gvsbuild cache, GTK DLL copies, Homebrew GTK/libadwaita dependencies, GSettings bundling, and obsolete dylib collection only after confirming no retained target helper consumes them.
- Build and package all helper executables required on each target; exclude Linux-only helpers explicitly rather than letting their absence surface at runtime.
- Point icon generation at frontend-neutral assets.
- Preserve signing/bundle/privacy metadata.
- Produce and smoke-test native artifacts on both targets from their installed/bundled locations, not from the workspace target directory.

### Task 9.3: Add Android build/release pipeline

**Files**
- New Android manifest/project or cargo integration files
- `.github/workflows/rust.yml`
- `.github/workflows/release.yml`

**Work**
- Pin JDK/Android SDK/NDK/Rust target inputs.
- Build a reproducible debug artifact in CI first, then signed release artifacts through repository-secret-backed signing.
- Document capability differences rather than hiding unsupported desktop functions.

### Task 9.4: Update public documentation

**Files**
- `README.md`
- `DOC.md` if present and relevant
- Release/changelog material used by the project

**Work**
- Replace GTK frontend/build/install instructions with Slint equivalents.
- Document supported platforms and honest Android capability scope.
- Document live language switching, responsive/collapsible navigation, configuration paths, and platform permissions.
- Remove screenshots/instructions belonging to deleted GTK UI.

### Task 9.5: Final verification and cleanup

**Commands/scenarios**
- Run formatting once.
- Run affected crate tests, then workspace tests.
- Run clippy with repository-required targets/features where supported.
- Build release artifacts for Linux, Windows, macOS, and Android through their actual packaging paths.
- Inspect each desktop artifact for the main executable and every required target-specific helper, then launch it from an unpacked/installed release location.
- Launch and exercise the actual UI on each available platform/emulator.
- Confirm dependency graph and packaged artifacts contain no GTK/libadwaita frontend runtime.
- Remove throwaway scripts, obsolete resources, stale generated files, and temporary dual-frontend feature flags.

## Delivery checkpoints

1. **Foundation checkpoint:** new Slint crate builds and renders tokens/components; no default behavior changed.
2. **Functional checkpoint:** IPC bridge, shell, localization, and every page reach Linux parity behind a temporary feature.
3. **Cross-platform checkpoint:** Windows/macOS smoke-tested; Android capability subset launches and survives lifecycle transitions.
4. **Cutover checkpoint:** Slint becomes default; GTK frontend is deleted cleanly.
5. **Release checkpoint:** packaging/CI/docs are current and real artifacts are smoke-tested.

## Critical risks to manage

- **False GTK removal:** deleting `lan-mouse-gtk` does not remove `adapters/gtk-clipboard`; keep this distinction explicit.
- **State divergence:** UI-local optimistic state must never override authoritative IPC events.
- **Index routing bugs:** callbacks must carry stable IDs, not current list positions.
- **Localization leakage:** Rust-generated errors/status/accessibility text must use message IDs or structured values, not embedded English.
- **Responsive duplication:** desktop/mobile share page components; only shell/interaction presentation adapts.
- **Platform overclaim:** Android cannot be called supported until a real artifact launches and the supported capability matrix is verified.
- **Packaging regressions:** GTK removal affects Flatpak, Nix, Windows DLL packaging, macOS dylib scripts, icon paths, and CI—not only Cargo manifests.
