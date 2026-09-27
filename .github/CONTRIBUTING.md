# Contributing to Syntra

Thanks for considering a contribution.

## Code of conduct

This project follows the [code of conduct](CODE_OF_CONDUCT.md).

## Before opening work

Read the architecture and contract guides in [`docs/`](../docs/). Keep the three IPC contracts distinct: `syntra-api` is client-to-daemon, `syntra-plugin-api` is plugin-to-daemon, and `syntra-proto` is machine-to-machine.

## Changes

- Keep daemon logic independent of UI code.
- Put presentation behaviour in `syntra-ui` and client orchestration in `syntra-app`.
- Treat plugins as separate processes discovered from manifests.
- Preserve platform feature gates and document unsupported targets plainly.
- Do not add references to projects from which Syntra was forked.

## Development

Build inside the project container:

```bash
distrobox enter "$SYNTRA_BUILD_CONTAINER" -- bash -lc 'cd "$PWD" && cargo build --workspace --features syntra-daemon/layer_shell_capture,syntra-daemon/x11_capture,syntra-daemon/libei_capture,syntra-daemon/wlroots_emulation,syntra-daemon/libei_emulation,syntra-daemon/rdp_emulation,syntra-daemon/uinput_emulation,syntra-daemon/x11_emulation'
```

Run formatting and checks through the workflows in [`.github/workflows/`](workflows/). Documentation changes must keep internal links valid and must keep links valid. Generated sections are refreshed automatically on `main`; to update them locally run `node .scripts/sync-readme.js` and `node .scripts/sync-licenses.js`.

## Pull requests

Describe the user-visible change, affected tier or contract, platform assumptions, and verification performed. Keep commits focused. Do not include generated build output or credentials. Contributions are accepted under GPL-3.0-or-later.

## Reporting bugs

Use the [bug report form](https://github.com/DarkPhilosophy/Syntra/issues/new/choose). Security problems follow the [security policy](SECURITY.md). Include platform, daemon/client versions, selected backend features, relevant diagnostics, and a minimal reproduction. Remove private network details before posting.
