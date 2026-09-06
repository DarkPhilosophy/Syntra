# Syntra documentation

This index is the starting point for the maintained, repository-local reference. Choose a guide by the work you need to do; the guides describe the current three-tier workspace.

## Where do I start?

- **New to Syntra:** read [Using Syntra](#using-syntra), then [Configuration](configuration.md), then the platform notes in the [canonical README](../.github/README.md).
- **Frontend or client author:** read [Client API](api.md), followed by [Architecture](architecture.md). Do not use the machine protocol for dashboard IPC.
- **Plugin author:** read [Plugins](plugins.md). A plugin is a separate process and uses stdio, not the client socket.
- **Daemon operator:** read [Configuration](configuration.md), [Logging](logging.md), and [Operating and troubleshooting](#operating-and-troubleshooting).
- **Protocol implementer:** read [Peer protocol](protocol.md), then [Architecture](architecture.md) for lifecycle ownership.

## Using Syntra

### [Canonical README](../.github/README.md)
The user-facing overview covers supported platforms, installation, desktop usage, daemon mode, file transfer, and current caveats. Read it before building or packaging.

### [Configuration](configuration.md)
Reference for daemon configuration, environment overrides, identity, peer definitions and bind settings. Check field names and defaults before copying an example.

## Building a client

### [Client API](api.md)
Documents `syntra-api`, newline-delimited JSON between daemon and clients over a Unix socket (loopback TCP on Windows). It lists transport discovery, request and event shapes, errors, and compatibility rules.

### [Architecture](architecture.md)
Explains Tier 1 daemon, Tier 2 application/UI/CLI clients, Tier 3 plugin processes, and ownership boundaries. Use its data-flow diagrams when deciding where a feature belongs.

## Writing a plugin

### [Plugins](plugins.md)
Describes discovery, JSON manifests, supervision and the newline-delimited stdio protocol exposed by `syntra-plugin-api`. It includes lifecycle and handshake examples derived from Rust types.

A plugin must not connect to the daemon socket with `syntra-api`; that contract is for clients. It must also not emit peer packets: machine-to-machine traffic belongs to `syntra-proto`.

## Understanding internals

### [Peer protocol](protocol.md)
Reference for `syntra-proto`: UDP event traffic, TCP setup and DTLS protection. Use it for cross-device connectivity, not local dashboard IPC.

### [Logging](logging.md)
Explains `syntra-log` filters, subsystem names, runtime tuning and diagnostics. Begin with a narrow subsystem filter rather than enabling trace globally.

### [Configuration](configuration.md) (identity and migration)
Use this guide after upgrades or identity changes. It records defaults and limitations so a syntactically valid file is not mistaken for a supported deployment.

## Operating and troubleshooting

1. Confirm the daemon is running and that the client is connecting to the expected local endpoint.
2. Check configuration paths, identity and peer authorisation in [Configuration](configuration.md).
3. Check discovery and machine-to-machine reachability using [Peer protocol](protocol.md).
4. Raise only the relevant subsystem in [Logging](logging.md), reproduce once, and capture the resulting lines.
5. Check capture and emulation backend support in [Architecture](architecture.md) and the platform matrix in the [canonical README](../.github/README.md).
6. For file transfer, verify the peer is connected and authenticated before investigating destination permissions.
7. For a plugin failure, inspect its manifest and stdio handshake using [Plugins](plugins.md); do not diagnose it as a client socket failure.

The canonical README has practical checks for firewall access, mDNS discovery, service installation, Flatpak permissions, file transfer and input backends. Follow those checks before changing protocol settings.

## Contract map

| Contract | Counterpart | Transport | Guide |
| --- | --- | --- | --- |
| `syntra-api` | daemon ↔ clients | newline-delimited JSON; Unix socket, loopback TCP on Windows | [Client API](api.md) |
| `syntra-plugin-api` | daemon ↔ plugins | newline-delimited JSON over stdio | [Plugins](plugins.md) |
| `syntra-proto` | machine ↔ machine | UDP events, TCP setup, DTLS | [Peer protocol](protocol.md) |

These contracts are deliberately distinct. A client should not implement plugin framing, a plugin should not depend on peer packets, and machine traffic should not be routed through the local client socket.

## Workspace map

| Area | Crates or paths | Responsibility |
| --- | --- | --- |
| Tier 1 | `crates/syntra-core`, `crates/syntra-daemon` | Logic, capture, emulation, discovery, transfer and service process |
| Tier 2 | `crates/syntra-app`, `crates/syntra-ui`, `crates/syntra-cli` | Dashboard, presentation and command-line clients |
| Tier 3 | `plugins/clipboard`, `plugins/fuse` | Separate-process integrations discovered from manifests |
| Contracts | `crates/syntra-api`, `crates/syntra-plugin-api`, `crates/syntra-proto` | Independent wire boundaries |
| Subsystems | `syntra-input-*`, `syntra-store`, `syntra-log` | Input events, history and diagnostics |

## Reference selection table

| If you need to… | Read first | Then read |
| --- | --- | --- |
| Add a dashboard control | [Client API](api.md) | [Architecture](architecture.md) |
| Add a CLI command | [Client API](api.md) | [canonical README](../.github/README.md) |
| Package the daemon | [Configuration](configuration.md) | [Architecture](architecture.md) |
| Add clipboard or filesystem integration | [Plugins](plugins.md) | [Logging](logging.md) |
| Diagnose a peer disconnect | [Peer protocol](protocol.md) | [Logging](logging.md) |
| Diagnose missing input | [Architecture](architecture.md) | [canonical README](../.github/README.md) |
| Change log verbosity | [Logging](logging.md) | [Configuration](configuration.md) |

## Build and source conventions

The workspace uses Rust 2021 and GPL-3.0-or-later. Builds happen in the project container; consult the canonical README for the exact `distrobox` invocation and Linux feature list. Feature flags determine which platform capture and emulation backends are compiled.

Documentation examples are repository-relative and must be checked against the current serde types before being copied into a client or plugin. Newline-delimited JSON means one complete JSON value per line; framing errors are protocol errors, not harmless formatting differences.

## Document conventions

Each guide separates current behaviour from limitations. Platform support is not implied merely because a crate compiles on that target. Where a backend, setting or lifecycle is incomplete, the relevant guide says so plainly.

The diagrams show ownership, not deployment topology. A single machine may run daemon, dashboard and plugins together; peer protocol traffic still crosses the machine boundary. Likewise, a temporary daemon started by a client does not change the API contract.

## Contribution route

Start with [Contributing](../.github/CONTRIBUTING.md) for repository workflow and prerequisites. Before proposing a change, identify its contract in the map above, read the owning guide, and check whether the architecture tests enforce the intended tier boundary.

Keep public examples aligned with serde representation and avoid introducing a second description of defaults. If a behaviour changes, update the guide that owns the contract and link it from this index.

## Limitations

This index is a routing document, not a substitute for the detailed references. It does not promise feature support on every platform, automatic peer authorisation, or successful plugin startup. The detailed guides and canonical README are authoritative for those conditions.

The repository currently represents Linux, macOS, Windows and Android; iOS is planned. Availability of individual capture, emulation and packaging backends depends on platform libraries and selected Cargo features.

## Quick links

- [Architecture](architecture.md)
- [Client API](api.md)
- [Plugins](plugins.md)
- [Peer protocol](protocol.md)
- [Logging](logging.md)
- [Configuration](configuration.md)
- [Canonical README](../.github/README.md)
- [Contributing](../.github/CONTRIBUTING.md)
