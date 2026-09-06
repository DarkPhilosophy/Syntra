# Configuration

Syntra’s daemon and clients use a per-user TOML configuration. The authoritative implementation is `crates/syntra-core/src/config.rs`; path and socket resolution is centralised in `crates/syntra-api/src/paths.rs`. A missing configuration directory is created automatically, and a missing `config.toml` is populated from the default serialisation (`config.rs:383-417`).

## Configuration file locations

The default directory is `dirs::config_dir()/syntra` (`crates/syntra-api/src/paths.rs:109-119`). In practice this is:

| Platform | Directory | File |
|---|---|---|
| Linux/BSD | `$XDG_CONFIG_HOME/syntra`, or `$HOME/.config/syntra` when the `dirs` crate falls back | `config.toml` |
| macOS | `$HOME/Library/Application Support/syntra` | `config.toml` |
| Windows | `%APPDATA%\\syntra` | `config.toml` |
| Android | App-private configuration supplied by the Android environment; the normal desktop path resolver is not used | `config.toml` |

Use `SYNTRA_CONFIG_DIR` to replace the directory, or `--config <file>` to replace the file for one invocation. `--config` takes precedence over the default directory and creates the selected parent directory (`crates/syntra-core/src/config.rs:387-407`).

Configuration is watched non-recursively. Changes to `config.toml` are picked up for create, data-modify, and remove events (`config.rs:446-470`), but command-line overrides remain in force for the running process.

## TOML format

All top-level fields are optional. The empty/default file is valid TOML and means “use platform/backend defaults”. The complete serde shape is:

```toml
capture_backend = "dummy"
emulation_backend = "dummy"
port = 4242
release_bind = ["KEY_LEFTCTRL", "KEY_LEFTSHIFT", "KEY_LEFTMETA", "KEY_LEFTALT"]
cert_path = "/home/alex/.config/syntra/syntra.pem"
clipboard_text = true
clipboard_image = true
clipboard_files = true
authorized_fingerprints = { "laptop" = "BASE64_OR_HEX_FINGERPRINT" }

[[clients]]
hostname = "desktop"
ips = ["192.0.2.10"]
port = 4242
position = "right"
activate_on_startup = true
enter_hook = "echo entered"
peer_fingerprint = "PEER_FINGERPRINT"

[file_receive]
auto_accept = false
download_directory = "/home/alex/Downloads"
```

The example shows every key, but values must match the actual enum and platform build. `host_name` is accepted as a legacy input spelling; newly serialised configuration writes `hostname` and omits `host_name` (`config.rs:127-138,351-364`).

### Top-level keys

| Key | Type | Default | Meaning |
|---|---|---|---|
| `capture_backend` | enum string, optional | platform-selected; unset means automatic backend | Selects input capture. Possible compiled values: `input-capture-portal`, `layer-shell`, `x11`, `windows`, `macos`, `dummy`; availability depends on target and enabled features (`config.rs:187-206`). |
| `emulation_backend` | enum string, optional | platform-selected; unset means automatic backend | Selects input emulation. The available names are compile-time/platform dependent; `dummy` is always represented by the enum (`config.rs:244-286`). |
| `port` | unsigned 16-bit integer | `4242` (`DEFAULT_PORT`) | Initial peer listen port and the value used when no CLI/config value exists (`config.rs:516-522`). |
| `release_bind` | array of Linux scancode values | Left Ctrl, Left Shift, Left Meta, Left Alt | Keys released when control is relinquished; the built-in array is `DEFAULT_RELEASE_KEYS` (`config.rs:380-381`). The exact TOML scancode spelling is provided by the scancode serde implementation. |
| `cert_path` | filesystem path | `<config-dir>/syntra.pem` | DTLS certificate/private identity file. CLI wins over TOML, which wins over the default (`config.rs:419-424`). |
| `clients` | array of client tables | empty | Configured peers. Each item is described below. |
| `authorized_fingerprints` | string-to-string map | empty map | Fingerprints authorised for incoming connections, keyed by the stored display/name value (`config.rs:483-490`). Treat these values as security-sensitive. |
| `clipboard_text` | boolean | `true` | Enables text clipboard synchronisation. |
| `clipboard_image` | boolean | `true` | Enables image clipboard synchronisation. |
| `clipboard_files` | boolean | `true` | Enables file-list/file clipboard synchronisation (`config.rs:530-536`). |
| `file_receive` | table, optional | `auto_accept=false`; download directory from platform Downloads, then `$HOME/Downloads`, then `Downloads` | Controls incoming file acceptance and destination. |

### Client table keys

Each `[[clients]]` entry maps to `ConfigClient` (`config.rs:326-347`).

| Key | Type | Default | Meaning |
|---|---|---|---|
| `hostname` | string, optional | none | Peer DNS/host name. |
| `host_name` | string, optional | none | Accepted legacy spelling. If both are present, source-level conversion must be treated cautiously; prefer `hostname`. |
| `ips` | array of IP addresses, optional | empty set | Direct peer addresses. Duplicates are removed in memory and sorted on write (`config.rs:336-364`). |
| `port` | unsigned 16-bit integer, optional | `4242` | Peer port. |
| `position` | `Position`, optional | enum default | Relative placement of the peer for pointer movement. Use the enum’s serde spelling accepted by the current build. |
| `activate_on_startup` | boolean, optional | `false` | Whether this peer is active on startup (`config.rs:336-344`). |
| `enter_hook` | string, optional | none | Hook command/value associated with entering the peer. It is stored as supplied; this reference does not imply shell safety. |
| `peer_fingerprint` | string, optional | none | Fingerprint pin for this configured peer. |

### File receiving

`[file_receive]` contains exactly two optional keys (`config.rs:121-125`):

| Key | Type | Default | Meaning |
|---|---|---|---|
| `auto_accept` | boolean | `false` | Accept incoming files automatically. `false` requires the normal acceptance flow. |
| `download_directory` | path | platform Downloads directory | Destination. Only an absolute configured path is accepted; a relative value is ignored and fallback resolution continues (`config.rs:538-552`). |

## Precedence and persistence

For `port`, `capture_backend`, and `emulation_backend`, command-line values override TOML values. Otherwise TOML overrides the code default (`config.rs:502-522`). `--cert-path` similarly overrides TOML and the default certificate path. GUI changes such as selecting a port or file-receive settings are persisted back to TOML through the configuration writer; the writer replaces the serialised file and syncs it (`config.rs:555-568,659-687`).

A frontend-selected port is persisted as a configured value. A client’s default port is omitted when serialised because `4242` is implicit (`config.rs:351-364`). Do not hand-edit generated fields while another process is writing the file.

## Command-line arguments

The shared parser accepts these options (`crates/syntra-core/src/config.rs:147-172`):

| Option | Value | Effect |
|---|---|---|
| `-p, --port` | `u16` | Override the initial listen port. |
| `-c, --config` | path | Use a non-default TOML file. |
| `--capture-backend` | backend enum | Override capture backend for this run. |
| `--emulation-backend` | backend enum | Override emulation backend for this run. |
| `--cert-path` | path | Use a non-default certificate/identity file. |

Subcommands are:

| Subcommand | Purpose |
|---|---|
| `daemon` | Run the headless daemon explicitly. No subcommand also runs daemon mode (`crates/syntra-daemon/src/main.rs:71-87`). |
| `cli …` | Run the Syntra command-line client; its remaining arguments are defined by `syntra-cli`. |
| `test-capture …` | Exercise input capture using the test argument structure. |
| `test-emulation …` | Exercise input emulation using the test argument structure. |

The dashboard binary `syntra` has its own small argument parser. `-b, --background` starts without a window and leaves the dashboard reachable from the tray; `-h, --help` prints help (`crates/syntra-app/src/main.rs:68-77`). The dashboard is a client and can start without a daemon, retrying and reattaching; `syntra-daemon` is the service binary.

Examples:

```sh
syntra-daemon --port 4242 --capture-backend x11
syntra-daemon --config /tmp/syntra-test/config.toml --cert-path /tmp/syntra-test/syntra.pem
syntra daemon
syntra --background
```

Backend names are not universally available: Linux feature flags decide whether `layer-shell`, `x11`, or `input-capture-portal` are compiled.

For container builds, enter the project’s configured build container and run the required Cargo command there. The repository’s build workflow supplies the feature list; the configuration reference intentionally does not prescribe a host-side build.

## Safe editing procedure

1. Stop the daemon before making coordinated manual edits.
2. Copy the complete configuration directory as a backup.
3. Edit TOML with a syntax-aware editor; preserve quoted paths and enum spellings.
4. Start the daemon and inspect warnings for parse or permission failures.
5. Confirm the effective port, certificate path, and peer fingerprints before reconnecting clients.

Do not place private keys, passwords, or unredacted trust inventories in shared issue reports.

## Environment overrides

| Variable | Exact effect | Scope |
|---|---|---|
| `SYNTRA_CONFIG_DIR` | Replaces the per-user configuration directory, including `config.toml` and the default certificate path | daemon/config clients (`crates/syntra-api/src/paths.rs:109-119`) |
| `SYNTRA_DAEMON_SOCKET` | Replaces the daemon/client control socket path | clients and daemon endpoint resolution (`paths.rs:27-29,94-99`) |
| `SYNTRA_DIAGNOSTICS_SOCKET` | Replaces the diagnostics Unix datagram path | daemon mirror and dashboard receiver (`paths.rs:29-30,101-107`) |
| `SYNTRA_LOG` | Selects the startup log specification, e.g. `info,network=debug` | every process using `syntra-log`; daemon reads it in `init_logging` |
| `XDG_RUNTIME_DIR` | Supplies the Unix runtime directory used for default control and diagnostics sockets | Linux/BSD path resolution (`paths.rs:62-68`) |
| `HOME` | Supplies the macOS runtime cache base and is also used by platform directory helpers | macOS runtime and fallback directories (`paths.rs:70-75`) |

`SYNTRA_CONFIG_DIR` is a directory, whereas `--config` is a file. Socket variables are complete paths, not directory prefixes. Environment values are read at process startup; changing them does not relocate an already-created socket or reload a running daemon.

## On-disk layout

A normal configuration directory contains:

| Path | Contents | Safe to delete? |
|---|---|---|
| `config.toml` | Backends, port, clipboard policy, peers, authorisations, and file receiving | Yes only with intentional reset; it removes configured peers and preferences. A default file is recreated. |
| `syntra.pem` | Device certificate/private key and therefore device identity | No for a routine cleanup. Deleting generates a new identity and requires every peer to authorise again. |
| History database | Clipboard history managed by `syntra-store` in the application-data directory | Only if history loss is acceptable; stop Syntra first and expect a fresh database. |
| Device profiles | Persisted peer/client records and fingerprints, represented principally by `clients` and `authorized_fingerprints` | Only as a deliberate pairing reset; it removes trust/profile state. |
| Images | Clipboard image payloads associated with history | Only if old image history may be discarded. |

The exact history/profile/image filenames are owned by the store and daemon implementation and may vary with the current schema; do not assume that deleting one unknown file is equivalent to clearing history. Back up the entire directory before manual changes. Socket files are runtime artefacts, not configuration data, and normally live under the runtime directory rather than the config directory.

### Device identity and migration

The certificate **is** the device identity. Its public fingerprint is what peers authorise; changing unrelated TOML settings does not change it. Preserve `syntra.pem` when moving a profile or changing `SYNTRA_CONFIG_DIR`.

The predecessor layout is migrated on first use. If the new `syntra` directory does not exist, the implementation adopts the old per-user directory rather than starting empty. It prefers a directory rename; if that crosses filesystems and rename fails, the old directory is left intact rather than half-copied (`crates/syntra-core/src/config.rs:60-103`). The former certificate filename is recognised and renamed to `syntra.pem` during migration. An already-existing new directory is never overwritten. This preserves the certificate and therefore the existing fingerprint. After migration, verify that `syntra.pem`, `config.toml`, profiles, and history are present before removing the old directory.

Deleting the certificate, pointing `--cert-path` at a new file, or creating a fresh config directory creates a new fingerprint. Every peer must then re-authorise the device; this is expected security behaviour, not a connectivity bug.

## User service installation

The daemon is intended to run headlessly as a user service and start at boot/login. Install the `syntra-daemon` binary, then use the project’s service-install command or package integration for the target platform. The generated unit must execute `syntra-daemon` without a dashboard, run in the user context, and inherit the user’s configuration/environment so it can read the certificate and bind the control socket. In particular, an installed unit contains the daemon executable invocation, an automatic restart/start-at-boot policy, and the user service identity; it must not launch the Slint dashboard.

On systems using a generated systemd user unit, inspect it with `systemctl --user cat syntra-daemon` and control it with `systemctl --user enable --now syntra-daemon`. Uninstall by disabling/stopping that unit and removing the generated unit through the same installer; do not delete `syntra.pem` unless a new identity is intended. Exact installer packaging is platform-specific, so treat the generated unit on disk as authoritative rather than copying a unit from another machine.

## Troubleshooting

| Symptom | Likely cause | Resolution |
|---|---|---|
| Config changes are ignored | CLI override still wins, malformed TOML was rejected, or the watched path is not the file being edited | Remove the relevant CLI option, inspect daemon stderr, and confirm `config_path()`/`--config`. |
| New config keeps resetting | The directory is not writable or the process is pointed at a temporary `SYNTRA_CONFIG_DIR` | Check ownership/permissions and the service environment. |
| “No such file” for config | Parent directory was unavailable or a custom `--config` parent does not exist | Let `Config::new` create it, or create the parent and check permissions. |
| Peers all require authorisation again | Certificate was deleted/replaced or a new config directory was selected | Restore the original `syntra.pem`; otherwise re-authorise the new fingerprint on every peer. |
| Peer cannot connect on default port | Different configured ports or a firewall | Compare both `port` values; remember peer default is `4242`. |
| Selected backend is rejected | Backend was not compiled for this platform/features | Use a compiled enum value or rebuild with the required daemon feature set. |
| Incoming files go elsewhere | Configured download path is relative or invalid | Use an absolute `download_directory`; otherwise the platform Downloads fallback is used. |
| Dashboard starts but daemon is absent | Dashboard is a client and daemon service is stopped/uninstalled | Start `syntra-daemon`; dashboard retry/reattach is expected. |
| Control socket cannot be found | `SYNTRA_DAEMON_SOCKET` differs between processes or runtime directory is missing | Set the same override for both, or fix `XDG_RUNTIME_DIR`/platform runtime setup. |
| Diagnostics socket is unavailable | Platform does not support Unix sockets, stale ownership, or mismatched override | Windows/Android currently have no Unix mirror; otherwise align `SYNTRA_DIAGNOSTICS_SOCKET` and remove only a stale socket. |
| History disappears after cleanup | Store database or image payloads were deleted | Restore the application-data backup; deletion is destructive for history. |

When diagnosing a service, first record the effective config path, certificate path, selected port, and environment overrides. Do not expose certificate private-key contents or trust maps in bug reports.

## Limitations

The configuration structs are intentionally sparse: there is no general environment override for individual TOML keys, and backend availability is compile-time/platform-dependent. The file watcher reports parse/read failures through logs and continues without a usable in-memory config rather than inventing values (`config.rs:410-417`). Android does not use the desktop Unix socket path resolver. Service-unit generation and exact data filenames are packaging concerns; inspect the installed artefacts for the target platform.
