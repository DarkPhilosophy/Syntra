# Syntra documentation

The maintained documentation starts at [`docs/README.md`](docs/README.md). That index routes readers to the architecture, client API, plugin, peer protocol, logging and configuration references.

## Choose the right contract

```mermaid
flowchart TD
    D["syntra-daemon<br/>Tier 1"]
    C["syntra-app / syntra-cli<br/>Tier 2 clients"]
    P["plugins/*<br/>Tier 3 processes"]
    M["Other Syntra machine<br/>peer"]
    D <-->|"syntra-api<br/>newline JSON socket"| C
    D <-->|"syntra-plugin-api<br/>newline JSON stdio"| P
    D <-->|"syntra-proto<br/>UDP/TCP under DTLS"| M
```

Do not conflate these boundaries. `syntra-api` is local daemon-to-client IPC; `syntra-plugin-api` is daemon-to-plugin process communication; `syntra-proto` is machine-to-machine networking. Each guide records its framing, ownership and limitations.

## Guides

- [Documentation index](docs/README.md) — audience routing and contract map.
- [Architecture](docs/architecture.md) — tiers, process boundaries and data flow.
- [Client API](docs/api.md) — socket transport, requests, events and compatibility.
- [Plugins](docs/plugins.md) — manifests, supervision and stdio protocol.
- [Peer protocol](docs/protocol.md) — DTLS setup and machine-to-machine messages.
- [Logging](docs/logging.md) — subsystem filters and diagnostics.
- [Configuration](docs/configuration.md) — TOML settings, identity and overrides.

## Desktop file drops

The Slint dashboard accepts native Wayland file drops through a data-device adapter on its existing window connection. Drop positions use the same device-card routing as other desktop backends; the receiver still chooses the destination and accepts the transfer. This does not switch the dashboard to XWayland or start an input-capture or portal session.

The adapter negotiates Copy only, accepts local `text/uri-list` entries, and bounds selection reads to 1 MiB and five seconds. It does not read or replace the ordinary clipboard. Unsupported offers can still be sent through the existing file picker.

## Repository facts

The workspace is Rust 2021, version `0.11.0`, licensed GPL-3.0-or-later and maintained at [github.com/DarkPhilosophy/syntra](https://github.com/DarkPhilosophy/syntra). Linux, macOS, Windows and Android are represented in the workspace; iOS is planned. Build and feature-selection details belong in the canonical README and the relevant guide, not in this signpost.

## Build note

Builds are performed in the project container. Consult the canonical README for the exact command and Linux feature list before compiling. This file intentionally remains a navigation page so that operational instructions have one maintained home.
