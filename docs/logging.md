# Logging

Syntra logging is process-wide, runtime-reconfigurable, and deliberately non-blocking. The implementation is in `crates/syntra-log`; the daemon installs it during startup (`crates/syntra-daemon/src/main.rs:53-68`). This page is an operator reference, not a promise that every module emits records at every level.

## Output model

Each enabled record is formatted as one line:

```text
[2026-09-06T12:34:56.789Z][INFO ][network] peer connected
```

The fields are timestamp (RFC 3339 with milliseconds), padded level, classified subsystem, and message (`crates/syntra-log/src/logger.rs:51-63`). Records are written to stderr and, on Unix when configured by the daemon, mirrored to a Unix datagram socket. Windows currently uses stderr only; Android does not provide the Unix-socket diagnostics transport.

The logger owns a bounded synchronous queue of 1,024 entries. Producers use `try_send`, never waiting for the writer (`crates/syntra-log/src/logger.rs:11-15,64-67`). A full queue drops a line rather than stalling input capture, emulation, or IPC. This is an intentional loss policy for overload: a wedged stderr consumer must not wedge the service.

A thread named `syntra-log` drains the queue. It acquires the stderr lock for each write, not for the thread lifetime, because other components can write stderr directly (`crates/syntra-log/src/logger.rs:99-123`). The mirror send is non-blocking and failures are ignored, so no dashboard is required for daemon operation (`crates/syntra-log/src/logger.rs:104-141`).

Before process termination the daemon calls `log::logger().flush()` (`crates/syntra-daemon/src/main.rs:41-51`). Flush enqueues a barrier and waits for the writer to acknowledge after flushing stderr (`crates/syntra-log/src/logger.rs:69-79,124-133`). Without this rule, a fatal error could be formatted successfully but remain in the queue while the process exits, losing the message that explains the exit.

## The nine subsystems

The stable identifiers are the values used by specifications and API messages (`crates/syntra-log/src/lib.rs:77-103`).

| Identifier | Covers | Typical targets |
|---|---|---|
| `input` | Pointer/keyboard capture and input emulation | `syntra_input_capture`, `syntra_input_emulation`, `syntra_core::capture`, `syntra_core::emulation` |
| `network` | Peer discovery, DNS, connection setup, DTLS, and machine-to-machine protocol | `syntra_core::discovery`, `syntra_proto`, `syntra_core::connect` |
| `clipboard` | Clipboard observation and synchronisation | modules containing `clipboard` |
| `transfer` | File offers, chunks, and transfer state machines | modules containing `transfer` |
| `history` | Clipboard history storage and peer reconciliation | modules containing `history` |
| `ipc` | Daemon/client control socket and request handling | modules containing `ipc` |
| `ui` | Dashboard presentation code | modules containing `ui` |
| `other` | Syntra code that matches none of the named rules | remaining `syntra…` targets |
| `external` | Dependencies and all non-Syntra targets | `tokio`, `zbus`, Slint, and other third-party targets |

### Classification rules

Classification is applied to the `log` target, lower-cased (`crates/syntra-log/src/lib.rs:106-145`). The crate-prefix rule is first: a target must start with `syntra` to be considered application code. Anything else is `external`, even if its spelling contains a Syntra keyword. This prevents dependency chatter such as `zbus::connection` being misclassified as `network` merely because it contains `connect`.

For targets beginning with `syntra`, substring rules are tested in this order:

1. `history` → `history`.
2. `transfer` → `transfer`.
3. `clipboard` → `clipboard`.
4. `capture`, `emulation`, or `input` → `input`.
5. `proto`, `discovery`, `dns`, or `connect` → `network`.
6. `ipc` → `ipc`.
7. `ui` → `ui`.
8. No match → `other`.

The order matters. A `history_sync` target containing a transfer-related word must remain history, and capture/emulation paths must remain input. Matching is intentionally substring-based so `syntra_core::clipboard` and `syntra_plugin_clipboard` can share an operator-facing category.

`external` defaults to `warn`, while the global default is `info` (`crates/syntra-log/src/lib.rs:210-241`). Dependencies often emit frequent informational status messages (D-Bus, windowing, and runtime libraries); allowing all of them at `info` obscures Syntra’s own records. Raise it explicitly when investigating dependency behaviour, for example `external=info` or `external=debug`.

## Specification syntax

A specification is a comma-separated list. Whitespace around entries, names, and levels is ignored. A bare level changes the global level; `name=level` sets one subsystem override (`crates/syntra-log/src/lib.rs:243-268`). The accepted levels are `off`, `error`, `warn`, `info`, `debug`, and `trace`.

Informal grammar:

```text
spec       = entry *( "," entry )
entry      = whitespace* ( level | name whitespace* "=" whitespace* level ) whitespace*
name       = "input" | "network" | "clipboard" | "transfer" | "history"
           | "ipc" | "ui" | "other" | "external"
level      = "off" | "error" | "warn" | "info" | "debug" | "trace"
whitespace = ASCII whitespace accepted by trim()
```

The parser is forgiving by design. Empty entries are skipped; malformed entries and unknown subsystem names are ignored rather than preventing startup (`crates/syntra-log/src/lib.rs:249-267`). A later bare level replaces the earlier global level. Repeated subsystem assignments use the last valid assignment. An omitted subsystem override inherits the global level.

Examples:

```text
info
```
Global `info`; `external` remains `warn`.

```text
info,clipboard=trace,input=warn
```
Normal application verbosity, detailed clipboard records, and no input debug noise.

```text
warn,network=debug,external=error
```
Quiet baseline, verbose peer diagnosis, and only dependency errors.

```text
trace,input=off,external=warn
```
Trace everything except input, with dependencies capped at warnings.

```text
clipboard=debug
```
Global remains the default `info`; clipboard becomes `debug`.

```text
info, clipboard = trace, ,bad=debug
```
Whitespace and the empty entry are harmless; `bad=debug` is ignored.

```text
info,external=debug,external=trace
```
The final valid assignment wins: external is `trace`.

To clear an override through the runtime API, set that subsystem to inherit (the API representation is described below); writing a new environment specification only takes effect at the next process start.

## `SYNTRA_LOG`

`SYNTRA_LOG` is the environment override for the process log filter (`crates/syntra-api/src/paths.rs:27-34`). The daemon reads it through `LogConfig::from_env` with a default specification of `info` (`crates/syntra-daemon/src/main.rs:58-68`).

```sh
SYNTRA_LOG='info,network=debug,external=warn' syntra-daemon
```

The dashboard also reads the same environment convention at startup where its process logger is initialised. Environment parsing does not fail closed: a typo is ignored entry-by-entry. `SYNTRA_LOG` is not a live control channel; restart the relevant process, or use the daemon API for changes without restarting.

## Runtime API control

The daemon exposes logging through the daemon/client API. `SetLogSpec(String)` applies a specification; `QueryLogSpec` requests the current value; the daemon reports the authoritative serialised specification in `LogSpec(String)` (`crates/syntra-api/src/lib.rs:756-761,873-880`). The shared `LogConfig` stores levels in atomics, so a change is visible to the next record and needs no restart (`crates/syntra-log/src/lib.rs:195-205,276-300`).

The JSON is newline-delimited and uses the API’s externally tagged enum representation. A request body is therefore shaped like:

```json
{"SetLogSpec":"info,clipboard=trace"}
```

A query is:

```json
{"QueryLogSpec":null}
```

The response/event is:

```json
{"LogSpec":"info,clipboard=trace,external=warn"}
```

The returned string is authoritative and suitable for display or round-tripping. It starts with the global level and then lists explicit overrides in stable subsystem order (`crates/syntra-log/src/lib.rs:302-325`). Runtime changes also refresh the `log` crate’s global ceiling so raising one subsystem to `trace` is not short-circuited before Syntra’s logger sees the record (`crates/syntra-log/src/lib.rs:327-342`).

## Diagnostics mirror socket

The daemon’s mirror is a Unix datagram sender. `syntra-api::paths::diagnostics_socket()` resolves the endpoint; `SYNTRA_DIAGNOSTICS_SOCKET` can override it (`crates/syntra-api/src/paths.rs:101-107`). On ordinary Unix the default is `$XDG_RUNTIME_DIR/syntra-diagnostics.sock`; on macOS it is `$HOME/Library/Caches/syntra-diagnostics.sock` because that is the runtime base selected by `paths.rs:62-75`. The daemon does not bind or require a listener: its writer thread sends each formatted line to the path if a mirror socket was created (`crates/syntra-log/src/logger.rs:104-141`).

The dashboard binds the socket and receives datagrams through `LiveDiagnosticReceiver` (`crates/syntra-ui/src/diagnostics.rs:198-366`). It validates ownership/metadata before accepting an existing socket, removes stale sockets where safe, and marks the stream unavailable when binding or receiving fails. A second live listener is rejected rather than silently stealing the stream.

A datagram contains exactly one formatted log line, including its trailing newline. The UI parser extracts timestamp, level, target/subsystem, and message from the bracketed prefix (`crates/syntra-ui/src/diagnostics.rs:127-153`). It normalises `warn`/`warning` to `warn`, derives a stage from message metadata, and retains the original message for display.

On Windows and Android this Unix datagram path is unavailable in the current implementation. The logging path still writes stderr; the UI must treat the live stream as unavailable rather than assuming that no records exist.

## Diagnostics view limits

The UI store is a ring buffer capped at `MAX_DIAGNOSTIC_ENTRIES = 2_000` (`crates/syntra-ui/src/diagnostics.rs:6-7,42-121`). New records evict the oldest. Filtering scans from the newest end and stops once the requested row window is full, rather than filtering an unbounded projection of the entire ring.

Incoming records are coalesced by a timer in the receiver/UI integration so bursts update the model in batches instead of triggering a redraw for every datagram. The view exposes a bounded row window: only the rows needed by the current page/viewport are projected. These bounds are correctness and responsiveness controls, not arbitrary data loss in the daemon’s stderr stream.

The previous unbounded projection attempted to derive every matching row on every update. Under a busy network or dependency stream, that work grew continuously and blocked the interface thread, making the dashboard appear frozen precisely when diagnostics were needed. The 2,000-entry ring, coalescing timer, and bounded newest-first window keep work proportional to what a user can see (`crates/syntra-ui/src/diagnostics.rs:83-121`).

## Troubleshooting and limitations

| Symptom | Likely cause | Resolution |
|---|---|---|
| No `debug` records after setting a subsystem | An earlier global `log` ceiling or malformed spec prevents them | Query `LogSpec`; use a valid `SetLogSpec` and ensure the override is at least `debug`. |
| Dependency messages swamp the view | `external` was raised too far | Set `external=warn` or `external=error`; raise only during a focused investigation. |
| Dashboard says diagnostics unavailable | Missing runtime directory, stale socket, permissions, or unsupported platform | Check `XDG_RUNTIME_DIR`/`HOME`, override `SYNTRA_DIAGNOSTICS_SOCKET`, and inspect stderr. Windows/Android have no Unix mirror. |
| A burst loses lines | The 1,024-entry writer queue filled | Reduce verbosity or inspect stderr/journal; dropping is preferable to blocking the service. |
| Fatal error is absent at exit | A non-daemon process exited without flushing | Ensure its shutdown path calls `log::logger().flush()`; the daemon already does. |
| `bad=trace` appears to do nothing | Unknown subsystem names are ignored | Use one of the nine exact identifiers. |

The logger intentionally does not guarantee lossless delivery under overload, and the diagnostics mirror is best-effort. A mirror listener’s absence never stops the daemon. `SYNTRA_LOG` configures startup only; API control is the no-restart mechanism.
## Operational checklist

When increasing verbosity, begin with one subsystem rather than enabling `trace` globally. Reproduce the fault, capture the relevant interval, then restore the previous specification. This limits queue pressure and keeps sensitive operational context out of unnecessarily broad logs.

Record the process, effective specification, platform, and whether the observation came from stderr or the diagnostics mirror. The mirror is a presentation feed, not an archival log; retain stderr or the service journal when evidence must survive dashboard restarts.

If records stop abruptly, distinguish three cases: the producer may be filtered, the bounded queue may be full, or the mirror listener may have disappeared. Querying `LogSpec` addresses only the first. Comparing stderr with the dashboard addresses the third. A busy service with no stderr consumer can explain the second.

## Security and privacy

Log messages can contain peer names, addresses, paths, protocol errors, and dependency diagnostics. Treat captured logs as potentially sensitive. The logger provides filtering, not redaction or encryption. The Unix diagnostics socket is a local IPC endpoint and must remain within the intended user runtime directory.

Do not grant another user access to the diagnostics socket merely to make a dashboard work. Fix ownership and runtime-directory setup instead. When sharing a report, remove certificate paths, fingerprints, local addresses, and file names unless each is necessary to reproduce the fault.

## Implementation reference

`LogConfig` is cloneable and internally shared; clones change the same atomic levels (`crates/syntra-log/src/lib.rs:198-214`). `level_for` resolves an explicit subsystem level or the global level, and `enabled` compares the record’s level against that result (`lib.rs:276-300`). The installed logger performs classification and filtering before queueing, so disabled records do not consume queue capacity.

Installation is process-global. Calling `install` twice returns `InstallError` because the `log` crate accepts one logger (`crates/syntra-log/src/logger.rs:87-99,145-153`). This is normal when embedding Syntra components: initialise logging once at the executable boundary and pass a `LogConfig` handle to code that needs runtime control.

For reproducible reports, include the exact specification as returned by `QueryLogSpec`, not merely the environment variable that was intended to set it.

The level ceiling is recalculated whenever a subsystem changes. Consequently, lowering the last verbose subsystem also lowers the process-wide `log` ceiling; subsequent records are rejected early, as intended.

The logger does not rotate files. Configure the service manager or shell redirection if persistent file retention is required, and ensure that such redirection cannot block the daemon indefinitely.
