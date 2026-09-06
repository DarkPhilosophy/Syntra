# Lan Mouse Slint Cross-Platform Redesign

## Status

Approved visual direction and architectural requirements. Desktop Slint integration and
frontend-neutral lifecycle/capability foundations are implemented incrementally; final written
spec review, runtime state projection, and default-feature cutover remain pending.
Android packaging/lifecycle scaffolding exists, but Android is not a verified supported target
until its Gradle/SDK pipeline and end-to-end runtime are validated.

## Goal

Replace the GTK/libadwaita frontend with a maintainable Slint frontend that preserves the existing Rust service and IPC behavior, supports Linux, Windows, and macOS desktop targets, and provides an Android path whose capabilities remain explicitly unavailable until verified. The frontend adapts cleanly to desktop and mobile layouts. Live language switching is a required target, but visible-string binding remains incomplete in the current implementation.

## Product principles

1. **Cross-platform is a core constraint, not a later port.** Shared behavior and UI models must compile for Linux, Windows, macOS, and Android. Platform-specific behavior lives behind narrow adapters.
2. **One source of truth.** The Lan Mouse service owns authoritative state. The UI renders immutable snapshots and sends typed user intents; it must not duplicate networking, capture, clipboard, transfer, or authorization state machines.
3. **DRY without premature abstraction.** Shared components and models are reused where behavior is genuinely identical. Platform adapters exist only where operating-system APIs differ.
4. **Configuration-driven UI.** Navigation, capabilities, permissions, clipboard options, and platform actions are described by typed data rather than scattered hard-coded visibility checks.
5. **No web runtime for the primary application.** The production frontend uses Slint and Rust, without Tauri, HTML, JavaScript, Tailwind, WebView, or a local HTTP server.
6. **Web/full-stack compatibility remains possible.** The frontend contract must not depend on Slint types. A future web client may consume the same serializable view models and commands through an additional transport without changing the core service.
7. **Localization is structural.** User-visible strings are translation keys with parameters; business logic never assembles translated sentences.

## Supported targets

### Required final targets

- Linux desktop: Wayland and X11 environments supported by the existing Lan Mouse backends.
- Windows desktop.
- macOS desktop.
- Android mobile/tablet (pipeline and runtime validation pending; not currently a supported release target).

Slint is selected because it supports native compiled desktop and mobile applications, including Android, while keeping application logic in Rust and avoiding a WebView frontend. Platform capability reporting must prevent unavailable Android input and transfer features from being presented as supported.

### Platform capability model

The frontend receives a `PlatformCapabilities` value rather than checking operating-system names throughout the UI:

```rust
pub struct PlatformCapabilities {
    pub input_capture: CapabilityState,
    pub input_emulation: CapabilityState,
    pub clipboard_text: CapabilityState,
    pub clipboard_images: CapabilityState,
    pub clipboard_files: CapabilityState,
    pub system_tray: CapabilityState,
    pub autostart: CapabilityState,
    pub background_service: CapabilityState,
    pub flatpak_permissions: CapabilityState,
}

pub enum CapabilityState {
    Available,
    PermissionRequired { action: PlatformActionId },
    Unavailable { reason_key: String },
}
```

The same page definitions therefore work on every target. Unsupported controls are explained or omitted according to the capability descriptor, never by ad-hoc `cfg` checks inside view code.

## Architecture

```text
Platform integrations
  Linux | Windows | macOS | Android
              │
              ▼
       Lan Mouse core/service
    networking, control, clipboard,
      transfers, configuration
              │
       frontend contract
 snapshots ◀──┴──▶ typed commands
              │
              ▼
        Slint presentation
 desktop shell | compact shell
```

### Crate boundaries

- `lan-mouse-core` or the existing service modules remain authoritative for domain behavior.
- `lan-mouse-ipc` continues to define transport-safe requests and frontend events.
- A new frontend-neutral presentation module maps domain state to serializable view models.
- A new `lan-mouse-slint` crate owns Slint files, presentation controllers, desktop/mobile shells, and UI-only state.
- Existing OS-specific code that currently lives in `lan-mouse-gtk` must move either into a platform adapter or into the Slint frontend when it is strictly window-related.
- `lan-mouse-gtk` remains only during migration and is removed after behavioral parity is verified. There is no permanent dual frontend.

### Frontend contract

The UI reads snapshots such as:

```rust
pub struct AppViewState {
    pub navigation: NavigationViewState,
    pub health: HealthViewState,
    pub devices: Vec<DeviceViewState>,
    pub clipboard: ClipboardViewState,
    pub transfers: Vec<TransferViewState>,
    pub diagnostics: DiagnosticViewState,
    pub permissions: Vec<PermissionViewState>,
    pub capabilities: PlatformCapabilities,
    pub locale: LocaleViewState,
}
```

It sends typed commands such as:

```rust
pub enum FrontendCommand {
    Navigate(PageId),
    SetClipboardOption { kind: ClipboardKind, enabled: bool },
    CancelTransfer(ClipboardTransferId),
    AddPeer(PeerDraft),
    UpdatePeer { id: PeerId, changes: PeerChanges },
    RemovePeer(PeerId),
    PerformPlatformAction(PlatformActionId),
    SetLocale(LocaleId),
}
```

Slint-generated types never cross into the service, IPC, persistence, or protocol crates.

## Information architecture

The approved left-navigation/right-content structure contains:

- Overview
- Devices
- Clipboard
- Transfers
- Diagnostics
- Settings

Security and permissions are grouped under Settings unless their state requires an actionable warning on Overview. Navigation items are data-driven and may be hidden when irrelevant to the current platform.

## Responsive and collapsible navigation

### Desktop expanded state

- Sidebar shows icon and translated label.
- A `<` collapse control appears at the sidebar edge.
- Sidebar width is user-resizable within defined minimum and maximum bounds.
- The chosen width and collapsed state are persisted as presentation preferences.

### Desktop collapsed state

- Sidebar shows icons only.
- The control changes to `>` and expands the sidebar.
- Every icon exposes a translated tooltip and accessible name.
- Active-page indication remains visible without relying only on color.
- Content expands into the reclaimed width without recreating page state.

### Compact/mobile state

- Layout switches by available width, not by operating-system name.
- On narrow Android windows, the sidebar becomes an overlay navigation drawer or compact navigation rail.
- Content remains the same component tree and view model; only the shell changes.
- Touch targets are at least 44 logical pixels.
- Back navigation closes an open drawer before leaving a page.

### Breakpoints

Breakpoints are centralized in one theme/layout configuration. Individual pages must not invent independent sidebar breakpoints.

## Localization

### Requirements

- Language changes must apply live without restarting the application.
- All visible labels, descriptions, errors, tooltips, accessibility names, dialog text, notifications, and formatted status messages must be localized.
- Translation must support parameters, plural forms, and locale-aware number/date/byte formatting.
- Missing keys fail visibly in development and fall back predictably in release builds.
- The selected locale persists in configuration.
- English is the source/fallback locale.

### Dictionary structure

Locale data is stored in one file per locale using a structured message system such as Fluent rather than a Rust `HashMap` of manually formatted sentences:

```text
locales/
  en-US.ftl
  ro-RO.ftl
  de-DE.ftl
```

Example:

```text
nav-overview = Overview
nav-devices = Devices
sidebar-collapse = Collapse navigation
sidebar-expand = Expand navigation
transfer-progress = { $completed } of { $total }
connected-peer-count =
    { $count ->
        [one] { $count } connected peer
       *[other] { $count } connected peers
    }
```

### Runtime localization service

A frontend-neutral `LocalizationService` owns the active locale and loaded bundles. It resolves message keys and parameters into a `LocalizedViewState`. On `SetLocale` it:

1. validates and persists the locale;
2. swaps the active dictionary atomically;
3. rebuilds localized presentation strings;
4. publishes a new `AppViewState` revision;
5. lets Slint bindings update immediately without rebuilding the process or losing page state.

The service logs missing keys with locale and key. Release fallback order is selected locale → language base → `en-US` → visible key identifier.

Business logic returns structured errors with `message_key` and parameters. It never stores already translated error sentences.

## Visual system

The approved mockup is the baseline rather than a disposable HTML prototype. Its tokens are mapped into Slint globals:

```slint
export global Theme {
    in-out property <color> background;
    in-out property <color> surface;
    in-out property <color> elevated-surface;
    in-out property <color> accent;
    in-out property <color> success;
    in-out property <color> warning;
    in-out property <color> danger;
    in-out property <length> sidebar-expanded-width;
    in-out property <length> sidebar-collapsed-width;
    in-out property <length> content-gap;
    in-out property <length> corner-radius;
}
```

Reusable components include navigation items, status badges, setting rows, device cards, transfer rows, permission rows, empty/error states, filter controls, and the diagnostics console. Pages compose these components; they do not fork their styling.

## Diagnostics console

The console remains part of the application and supports:

- stable chronological ordering with newest events at the bottom;
- timestamps, level, subsystem, stage, direction, peer/transfer correlation, and message;
- level, stage, direction, peer, and free-text filters;
- pause/resume without dropping retained events;
- clear-view and export actions;
- bounded storage and virtualized rendering;
- autoscroll only while the user is already at the bottom.

Diagnostics data is structured before it reaches the UI. The UI does not parse arbitrary log lines to discover level or stage.

## Configuration

Configuration distinguishes domain settings from presentation preferences:

```rust
pub struct UiPreferences {
    pub locale: LocaleId,
    pub theme: ThemePreference,
    pub sidebar: SidebarPreference,
    pub diagnostics: DiagnosticFilterPreference,
}
```

New fields use explicit defaults and backward-compatible deserialization. Platform-specific settings are grouped under typed platform sections rather than scattered optional booleans.

## Error handling and state transitions

- Every asynchronous action has idle, pending, success, and failed states.
- Failed operations preserve the user's input and expose a retry or corrective action.
- Permission-required states identify the exact platform action.
- UI commands carry stable IDs so delayed responses cannot mutate a replacement device or transfer row.
- Unsupported operations are disabled with a translated explanation, not silently ignored.
- Locale switching is atomic: the old language remains visible until the new bundle has loaded successfully.

## Accessibility and input

- Every icon-only control has an accessible translated name and tooltip.
- Navigation supports pointer, keyboard, touch, and platform back behavior.
- Focus order follows visual order.
- Focus remains on the equivalent element after sidebar collapse or locale change where possible.
- Status is never communicated by color alone.
- Motion respects reduced-motion settings where exposed by the platform.

## Migration strategy

1. Extract frontend-neutral view models and commands without changing GTK behavior.
2. Introduce localization infrastructure and migrate all frontend-visible strings to keys.
3. Add the Slint crate and shared theme/component primitives.
4. Implement desktop shell, responsive compact shell, and collapsible navigation.
5. Port pages one at a time against the same frontend contract.
6. Port platform window/tray/privacy integration behind typed platform adapters.
7. Verify feature parity and target builds.
8. Make Slint the default frontend and remove GTK/libadwaita code and dependencies completely.

The migration must not alter clipboard, FUSE, networking, capture, emulation, or protocol behavior merely to accommodate the new UI.

## Verification

### Contract tests

- Domain state maps deterministically into `AppViewState`.
- Commands preserve stable IDs and configuration semantics.
- Capability descriptors produce the expected visible actions for each platform fixture.

### Localization tests

- Every source-locale key exists and parses.
- Every referenced key exists in `en-US`.
- Parameter and plural messages format correctly.
- Switching locale updates visible view state without restarting or losing selected page/sidebar state.
- Missing-key fallback order is deterministic.

### UI behavior verification

- Expanded sidebar displays icons and labels.
- `<` collapses to icon-only navigation; `>` expands it.
- Tooltips/accessibility names remain available while collapsed.
- Compact width selects mobile navigation without changing page state.
- Filters, transfer cancellation, permission actions, dialogs, and console autoscroll behave as specified.

### Platform verification

- Linux, Windows, macOS, and Android targets compile in CI.
- Desktop smoke tests launch the actual application and exercise navigation, settings, diagnostics, and shutdown/tray behavior.
- Android smoke test launches the actual application, exercises compact navigation, changes locale live, and verifies supported capability states.
- Platform-specific permission and background behavior is manually verified on its target OS when automation is unavailable.

## Explicit non-goals

- No Tauri or WebView frontend in this migration.
- No permanent GTK compatibility frontend.
- No redesign of the network protocol, clipboard protocol, FUSE transfer protocol, or input pipeline.
- No cloud service or browser client in the initial Slint migration.
- No platform-specific fork of the complete UI.

## Acceptance criteria

- The approved mockup structure is implemented in Slint with left navigation and right content.
- The sidebar collapses to icons with `<` and expands with `>`; layout responds dynamically.
- Language changes apply live across all visible UI without restarting.
- User-visible text comes from structured locale dictionaries, including errors and accessibility labels.
- Shared frontend code and models support Linux, Windows, macOS, and Android.
- Platform differences are isolated behind capability/action adapters.
- The service remains the sole source of domain truth.
- The final application has no GTK/libadwaita frontend dependency and no WebView runtime.
- Existing Lan Mouse behavior remains intact while the new frontend reaches feature parity.
