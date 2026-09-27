## Summary

<!-- What does this change, and why? Link the issue it resolves: "Fixes #123". -->

## Scope

- [ ] Service (`syntra-core`, `syntra-daemon`)
- [ ] Dashboard (`syntra-ui`, `syntra-app`)
- [ ] Contract (`syntra-api`, `syntra-plugin-api`, `syntra-proto`) — version bump considered
- [ ] Input backends (capture / emulation)
- [ ] Plugins
- [ ] Build, CI, packaging or documentation

## Verification

<!-- Commands run and what you tested by hand, on which OS / desktop. -->

- [ ] `cargo fmt --check`
- [ ] `cargo clippy` for the affected crates
- [ ] `cargo test` for the affected crates
- [ ] Tested by hand on: <!-- e.g. Fedora 44 GNOME Wayland ↔ Ubuntu 24.04 KDE -->

## Checklist

- [ ] No credentials, private addresses, host names or personal paths in code, logs or screenshots.
- [ ] User-visible behaviour is documented (README, docs, changelog) where it changed.
- [ ] Generated sections are current (`node .scripts/sync-readme.js`, `node .scripts/sync-licenses.js`).

## Licensing

By submitting this pull request I confirm that I have the right to submit
this work and that I license it under the project's
[GPL-3.0-or-later](../LICENSE), and I understand the project is provided
without warranty ([disclaimer](DISCLAIMER.md)).
