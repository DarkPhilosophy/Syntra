# Syntra daemon frontend API

This document specifies the control protocol for a non-Rust frontend. A web dashboard, CLI, or native client can implement it with JSON and a byte-stream socket; no workspace crate is required. The authoritative Rust definitions are in `crates/syntra-api/src/lib.rs` (types at lines 95–635; events at 637–764; requests at 766–903).

## 1. Transport

### Endpoint discovery

On Unix, resolve the per-user control socket as follows:

1. If `SYNTRA_DAEMON_SOCKET` is set, use its value verbatim (`crates/syntra-api/src/paths.rs:27-29,94-99`).
2. Otherwise use the runtime directory selected by the platform and append `syntra-daemon.sock` (`paths.rs:42,62-85`). On Linux this is the user runtime directory (normally `$XDG_RUNTIME_DIR`); on macOS it is the per-user temporary/application runtime location. The resolver returns an absolute path (`crates/syntra-api/tests/contract.rs:136-147`).

The override is useful for tests, sandboxes, and side-by-side installations. Do not invent a second default: both daemon and clients use `paths::daemon_socket()`.

On Windows there is no Unix-domain socket. Connect to TCP `127.0.0.1:5252`; this is `DEFAULT_IPC_PORT` in `paths.rs:36-37` and the connector uses that literal in `connect.rs:84-87`. The port is not configurable through `SYNTRA_DAEMON_SOCKET`.

`DEFAULT_PORT` is a different constant: UDP peer traffic defaults to `4242` (`lib.rs:95-96`). Do not use `4242` for the frontend control connection.

### Framing and direction

The connection is a bidirectional newline-delimited JSON (NDJSON) stream. Each request is one JSON value followed by `\n`; each event is one JSON value followed by `\n` (`connect.rs:31-50`). Read complete lines, decode each line independently as UTF-8 JSON, and preserve ordering. Never pretty-print a frame or put a literal newline inside a JSON string: the contract test `serialised_frames_never_contain_a_newline` exists because a newline would split one frame (`tests/contract.rs:12-34`).

The daemon may emit events at any time after connection. Events are authoritative snapshots or results, not acknowledgements that can safely be inferred from a request. Keep reading until EOF; reconnect if the service restarts. The Rust connector retries service appearance with exponential backoff (`connect.rs:77-120`; async equivalent `connect_async.rs:61-115`).

### Errors and shutdown

A malformed JSON line is a protocol error. Unknown enum variants are rejected during deserialisation (see §8), so a client should report the error and choose whether to close rather than silently discard it. `StopService` deliberately terminates the daemon; expect the stream to close after sending it.

## 2. JSON conventions

Serde uses externally tagged enums. Unit variants are bare strings (`"Sync"`). Tuple variants are an object whose value is an array (`{"Activate":[1,true]}`). Struct variants are an object whose value is an object with field names (`{"SetPluginEnabled":{"id":"clipboard","enabled":true}}`). Struct fields use their Rust names and ordinary JSON types. `Option<T>` is `null` when absent; vectors and maps are JSON arrays and objects. `IpAddr`, `SocketAddr`, and `PathBuf` use their standard string representation. `Vec<u8>` and `[u8; 8]` use serde_json's integer-array representation.

## 3. Requests: `FrontendRequest`

Send exactly one of the following 38 variants. Declaration: `crates/syntra-api/src/lib.rs:766-903`.

| Variant | JSON example | Meaning |
|---|---|---|
| `Activate` | `{"Activate":[1,true]}` | Set client handle 1 active (`false` deactivates). |
| `Create` | `"Create"` | Create a configured client. |
| `ChangePort` | `{"ChangePort":4242}` | Recreate the peer UDP listener on port 4242. |
| `Delete` | `{"Delete":1}` | Delete client handle 1. |
| `Enumerate` | `"Enumerate"` | Request all configured clients and state. |
| `ResolveDns` | `{"ResolveDns":1}` | Resolve DNS for client 1. |
| `DiscoverPeers` | `"DiscoverPeers"` | Refresh local mDNS discovery. |
| `StopService` | `"StopService"` | Gracefully stop the local daemon. |
| `UpdateHostname` | `{"UpdateHostname":[1,"desk.example"]}` | Set client 1 hostname; use `null` to clear. |
| `UpdatePort` | `{"UpdatePort":[1,4242]}` | Set client 1 peer UDP port. |
| `UpdatePosition` | `{"UpdatePosition":[1,"left"]}` | Put client 1 at `left`, `right`, `top`, or `bottom`. |
| `UpdateFixIps` | `{"UpdateFixIps":[1,["192.0.2.10"]]}` | Replace manually fixed IP addresses. |
| `EnableCapture` | `"EnableCapture"` | Request re-enabling local input capture. |
| `EnableEmulation` | `"EnableEmulation"` | Request re-enabling local input emulation. |
| `Sync` | `"Sync"` | Request synchronisation of all authoritative state. |
| `SetLocalDeviceProfile` | `{"SetLocalDeviceProfile":{"display_name":"Desk","avatar":null}}` | Replace the published device identity. |
| `AuthorizeKey` | `{"AuthorizeKey":["Office laptop","sha256:abc"]}` | Authorise a fingerprint with a description. |
| `RemoveAuthorizedKey` | `{"RemoveAuthorizedKey":"sha256:abc"}` | Remove an authorised fingerprint. |
| `UpdateEnterHook` | `{"UpdateEnterHook":[1,"/usr/local/bin/on-enter"]}` | Set client 1's enter hook; `null` clears it. |
| `SaveConfiguration` | `"SaveConfiguration"` | Persist current daemon configuration. |
| `SetClipboardText` | `{"SetClipboardText":true}` | Enable or disable text clipboard synchronisation. |
| `SetClipboardImage` | `{"SetClipboardImage":false}` | Enable or disable image clipboard synchronisation. |
| `SetClipboardFiles` | `{"SetClipboardFiles":true}` | Enable or disable file clipboard synchronisation. |
| `CancelClipboardTransfer` | `{"CancelClipboardTransfer":7}` | Cancel clipboard transfer ID 7. |
| `QueryHistory` | `{"QueryHistory":{"query":"report","offset":0,"limit":50}}` | Query a bounded history page; limit is clamped to 50. |
| `SetHistoryPinned` | `{"SetHistoryPinned":{"event_id":{"origin_device_id":"device-a","origin_sequence":9},"pinned":true}}` | Change one record's pin state. |
| `GetHistoryImage` | `{"GetHistoryImage":{"origin_device_id":"device-a","origin_sequence":9}}` | Fetch complete image bytes for one event. |
| `ClearGlobalHistory` | `"ClearGlobalHistory"` | Request coordinated deletion across peers. |
| `SetInputSharing` | `{"SetInputSharing":true}` | Enable or disable incoming and outgoing input sharing. |
| `RegenerateIdentity` | `"RegenerateIdentity"` | Generate replacement certificate for the next restart. |
| `SendFiles` | `{"SendFiles":{"peer_fingerprint":"sha256:abc","paths":["/tmp/a.txt"]}}` | Offer paths to a peer. |
| `AcceptFileTransfer` | `{"AcceptFileTransfer":{"peer_fingerprint":"sha256:abc","transfer_id":3,"destination_directory":"/tmp/incoming"}}` | Accept an incoming offer into a directory. |
| `DeclineFileTransfer` | `{"DeclineFileTransfer":{"peer_fingerprint":"sha256:abc","transfer_id":3}}` | Decline an incoming offer. |
| `CancelManualTransfer` | `{"CancelManualTransfer":{"peer_fingerprint":"sha256:abc","transfer_id":3}}` | Cancel a manual transfer. |
| `SetFileReceiveSettings` | `{"SetFileReceiveSettings":{"auto_accept":false,"download_directory":"/tmp/incoming"}}` | Replace automatic receive policy. |
| `SetLogSpec` | `{"SetLogSpec":"info,clipboard=trace"}` | Replace runtime logging filter. |
| `QueryLogSpec` | `"QueryLogSpec"` | Request the active logging filter. |
| `QueryPlugins` | `"QueryPlugins"` | Request all known plugin statuses. |
| `SetPluginEnabled` | `{"SetPluginEnabled":{"id":"clipboard","enabled":true}}` | Start/stop a plugin by manifest ID. |
| `RestartPlugin` | `{"RestartPlugin":{"id":"clipboard"}}` | Restart one supervised plugin process. |

`ClientHandle`, transfer IDs, and sequence numbers are JSON numbers (unsigned Rust integers). A `PathBuf` example is a JSON string, not an array. `HistoryPreview` is internally tagged (`kind`), unlike the outer request/event enums.

## 4. Events: `FrontendEvent`

The daemon can emit these 34 variants. Declaration: `crates/syntra-api/src/lib.rs:637-764`. Handle every variant explicitly.
The count includes both unit events (`HistoryChanged`) and payload-bearing events; it does not count supporting-type enum variants.

| Variant | JSON example | Meaning |
|---|---|---|
| `Created` | `{"Created":[1,{"hostname":null,"fix_ips":[],"port":4242,"pos":"left","cmd":null},{"active":false,"active_addr":null,"alive":false,"remote_ready":false,"dns_ips":[],"ips":[],"has_pressed_keys":false,"resolving":false,"peer_commit":null}]}` | New client plus config and runtime state. |
| `NoSuchClient` | `{"NoSuchClient":1}` | Requested handle does not exist. |
| `State` | `{"State":[1,{"hostname":null,"fix_ips":[],"port":4242,"pos":"left","cmd":null},{"active":true,"active_addr":null,"alive":false,"remote_ready":false,"dns_ips":[],"ips":[],"has_pressed_keys":false,"resolving":false,"peer_commit":null}]}` | Authoritative config/state update. |
| `Deleted` | `{"Deleted":1}` | Client was deleted. |
| `PortChanged` | `{"PortChanged":[4242,null]}` | Peer UDP port changed; second value is an optional failure. |
| `Enumerate` | `{"Enumerate":[]}` | Complete list of `(handle, config, state)` tuples. |
| `DiscoveredPeers` | `{"DiscoveredPeers":[{"id":"syntra-1","display_name":"Laptop","addresses":["192.0.2.4"],"port":4242}]}` | Current unauthenticated mDNS advertisements. |
| `CaptureStatus` | `{"CaptureStatus":"Enabled"}` | Input capture status. |
| `EmulationStatus` | `{"EmulationStatus":"Disabled"}` | Input emulation status. |
| `AuthorizedUpdated` | `{"AuthorizedUpdated":{"Office laptop":"sha256:abc"}}` | Authorised-key description/fingerprint map changed. |
| `PublicKeyFingerprint` | `{"PublicKeyFingerprint":"sha256:self"}` | This device's certificate fingerprint. |
| `PeerDeviceProfile` | `{"PeerDeviceProfile":{"fingerprint":"sha256:peer","profile":{"display_name":"Laptop","avatar":null}}}` | Profile for an authenticated peer. |
| `ClientFingerprint` | `{"ClientFingerprint":{"handle":1,"fingerprint":"sha256:peer"}}` | Fingerprint associated with client route 1. |
| `DeviceConnected` | `{"DeviceConnected":{"addr":"192.0.2.4:4242","fingerprint":"sha256:peer"}}` | Authenticated device connection established. |
| `DeviceEntered` | `{"DeviceEntered":{"fingerprint":"sha256:peer","addr":"192.0.2.4:4242","pos":"left"}}` | Peer entered at an edge. |
| `IncomingDisconnected` | `{"IncomingDisconnected":"192.0.2.4:4242"}` | Incoming peer disconnected. |
| `ConnectionAttempt` | `{"ConnectionAttempt":{"fingerprint":"sha256:peer"}}` | Connection needs fingerprint approval. |
| `ClipboardSettings` | `{"ClipboardSettings":{"text":true,"image":true,"files":false}}` | Authoritative clipboard toggles. |
| `ClipboardTransferStatus` | `{"ClipboardTransferStatus":{"transfer_id":7,"file_id":1,"name":"a.txt","direction":"receiving","transferred_bytes":10,"total_bytes":20,"bytes_per_second":100,"state":"transferring"}}` | Progress/status for one clipboard file. |
| `HistoryChanged` | `"HistoryChanged"` | Visible history mutation invalidated cached pages. |
| `HistoryPage` | `{"HistoryPage":{"query":"report","offset":0,"next_offset":null,"records":[]}}` | Bounded ordered history result. |
| `HistoryPinResult` | `{"HistoryPinResult":{"event_id":{"origin_device_id":"device-a","origin_sequence":9},"pinned":true,"updated":true}}` | Pin operation result. |
| `HistoryImageResult` | `{"HistoryImageResult":{"event_id":{"origin_device_id":"device-a","origin_sequence":9},"image":null,"error":"not an image"}}` | Image fetch result. |
| `HistoryError` | `{"HistoryError":"history unavailable"}` | History operation failed. |
| `HistoryClearResult` | `{"HistoryClearResult":{"operation_id":"clear-1","affected":4,"peers_acknowledged":2,"error":null}}` | Coordinated clear terminal result. |
| `FileReceiveSettingsChanged` | `{"FileReceiveSettingsChanged":[{"auto_accept":false,"download_directory":"/tmp/incoming"},null]}` | Settings changed; optional error explains failure. |
| `IncomingFileOffer` | `{"IncomingFileOffer":{"peer_fingerprint":"sha256:peer","transfer_id":3,"file_name":"photo.jpg","size":1024,"suggested_directory":"/tmp"}}` | File awaits accept/decline. |
| `ManualTransferStatus` | `{"ManualTransferStatus":{"peer_fingerprint":"sha256:peer","transfer_id":3,"file_name":"photo.jpg","size":1024,"transferred":512,"direction":"receiving","state":"transferring","destination":"/tmp/photo.jpg","error":null}}` | Manual transfer progress/outcome. |
| `ManualTransferError` | `{"ManualTransferError":"peer refused transfer"}` | Manual transfer error. |
| `InputSharing` | `{"InputSharing":true}` | Authoritative global input-sharing state. |
| `IdentityRegenerated` | `{"IdentityRegenerated":{"fingerprint":"sha256:new"}}` | Replacement identity saved; active sessions change on restart. |
| `LogSpec` | `{"LogSpec":"info,clipboard=trace"}` | Active runtime logging specification. |
| `Plugins` | `{"Plugins":[]}` | Complete authoritative plugin status list. |

The examples use empty collections where that keeps the shape readable; field types and nested variants are defined below.

## 5. Supporting types

### Client and display state

| Type | JSON form / values | Fields and meaning |
|---|---|---|
| `Position` | lowercase string: `left`, `right`, `top`, `bottom` | Display edge. |
| `ClientConfig` | object | `hostname: string|null` DNS name; `fix_ips: string[]` manual IPs; `port: u16` peer UDP port; `pos: Position`; `cmd: string|null` enter hook. Default: inactive client config uses port 4242, empty IPs, `left`, and null fields (`lib.rs:175-200`). |
| `ClientState` | object | `active: bool`; `active_addr: SocketAddr|null`; `alive: bool`; `remote_ready: bool`; `dns_ips: IpAddr[]`; `ips: IpAddr[]` (Rust `HashSet`); `has_pressed_keys: bool`; `resolving: bool`; `peer_commit: number[]|null`, exactly eight bytes. |
| `Status` | `"Enabled"` or `"Disabled"` | Boolean-like capability status; unlike lowercase transfer enums, it has no rename rule (`lib.rs:905-920`). |
| `DeviceProfile` | object | `display_name: string`; `avatar: PeerAvatar|null`; published identity. |
| `PeerAvatar` | object | `width: u32`; `height: u32`; `rgba: number[]`, exactly `width*height*4` bytes. |

`DeviceProfile` validation rejects names over 128 UTF-8 bytes, zero or over-128 dimensions, and an RGBA array whose length does not equal the dimensions (`lib.rs:604-634`).

### Clipboard and files

| Type | Values / fields |
|---|---|
| `ClipboardSettings` | `{text: bool, image: bool, files: bool}`; each category is independent. |
| `ClipboardTransferDirection` | lowercase `sending` or `receiving`, relative to this daemon. |
| `ClipboardTransferState` | lowercase `pending`, `transferring`, `completed`, `cancelled`, or `{"failed":"reason"}`. |
| `ClipboardTransferStatus` | `transfer_id: u64`; `file_id: u64`; `name: string`; `direction`; `transferred_bytes: u64`; `total_bytes: u64`; `bytes_per_second: u64`; `state`. |
| `FileReceiveSettings` | `auto_accept: bool`; `download_directory: string`. Default is false and an empty path. |
| `ManualTransferState` | externally tagged string: `"Offering"`, `"AwaitingAcceptance"`, `"Transferring"`, `"Completed"`, `"Declined"`, `"Cancelled"`, or `"Failed"` (no `rename_all`). |
| `ManualTransferStatus` | `peer_fingerprint: string`; `transfer_id: u64`; `file_name: string`; `size: u64`; `transferred: u64`; `direction: ClipboardTransferDirection`; `state: ManualTransferState`; `destination: string|null`; `error: string|null`. |
| `IncomingFileOffer` | `peer_fingerprint: string`; `transfer_id: u64`; `file_name: string`; `size: u64`; `suggested_directory: string`. |

The distinction between clipboard files and manually offered files is contractual: use the corresponding status/event family rather than merging them in your model.

### History

| Type | Fields / JSON |
|---|---|
| `HistoryEventId` | `{origin_device_id: string, origin_sequence: u64}`; stable identity across pages and peers. |
| `HistoryKind` | lowercase `text`, `image`, `files`. |
| `HistoryPreview` | Internally tagged object with `kind`. Text: `{"kind":"text","preview":string,"truncated":bool}`. Image: `{"kind":"image","media_type":string|null,"width":u32|null,"height":u32|null,"size_bytes":u64}`. Files: `{"kind":"files","count":u32,"total_size_bytes":u64,"names":string[],"names_truncated":bool}`. |
| `HistoryRecordSummary` | `event_id`; `created_at_ms: i64` Unix milliseconds; `origin_label: string|null`; `pinned: bool`; `kind: HistoryKind`; `preview: HistoryPreview`. |
| `HistoryPage` | `query: string`; `offset: u64`; `next_offset: u64|null`; `records: HistoryRecordSummary[]`. |
| `HistoryImage` | `event_id`; `bytes: number[]`; `media_type: string|null`; `width: u32|null`; `height: u32|null`. |

`MAX_HISTORY_PAGE_SIZE` is 50 (`lib.rs:370-371`); requests with a larger limit are clamped, keeping responses bounded.

### Discovery and plugins

| Type | Fields / values |
|---|---|
| `DiscoveredPeer` | `id: string` mDNS service name; `display_name: string`; `addresses: IpAddr[]`; `port: u16`. Discovery is explicitly unauthenticated (`lib.rs:474-486`). |
| `PluginHealth` | lowercase `disabled`, `not_installed`, `stopped`, `starting`, `healthy`, `unresponsive`, `failed`. |
| `PluginStatus` | `id`, `name`, `description`, `version`, `author`, `manifest_path`, `executable`: strings; `protocol_version`, `supported_protocol_version`, `restarts`: numbers; `health: PluginHealth`; `homepage`, `source`, `update_url`, `license`, `error`: string|null; `mime_types: string[]`; `bundled`, `installed`, `enabled`, `running`: booleans. Provenance paths and author/source fields are intentional trust metadata (`lib.rs:522-582`). |

## 6. Bounds and constants

| Constant | Value | Applies to |
|---|---:|---|
| `DEFAULT_PORT` | `4242` | Peer UDP (and peer setup TCP) default, not frontend IPC. |
| `MAX_HISTORY_PAGE_SIZE` | `50` | Maximum returned history page size; larger query limits are clamped. |
| `MAX_DEVICE_PROFILE_NAME_BYTES` | `128` | Maximum UTF-8 encoded byte length of `display_name`; not character count. |
| `MAX_PEER_AVATAR_DIMENSION` | `128` | Maximum avatar width and height in pixels; both must be non-zero. |
| `DEFAULT_IPC_PORT` | `5252` | Windows-only loopback frontend control port (`paths.rs:36-37`). |

For example, 128 ASCII characters occupy 128 bytes and fit, while 128 two-byte characters occupy 256 bytes and are rejected. The contract tests `device_profile_enforces_its_documented_bound` and `profile_bound_counts_bytes_not_characters` pin this behaviour (`tests/contract.rs:93-134`).

## 7. Worked session

The following is a representative transcript; asynchronous events may be interleaved or repeated.

```text
# Unix client opens $XDG_RUNTIME_DIR/syntra-daemon.sock
-> "Sync"\n
<- {"PublicKeyFingerprint":"sha256:self"}\n
<- {"CaptureStatus":"Enabled"}\n
<- {"EmulationStatus":"Disabled"}\n<- {"ClipboardSettings":{"text":true,"image":true,"files":false}}\n<- {"InputSharing":true}\n<- {"Enumerate":[[1,{"hostname":"desk","fix_ips":[],"port":4242,"pos":"left","cmd":null},{"active":true,"active_addr":"192.0.2.4:4242","alive":true,"remote_ready":true,"dns_ips":["192.0.2.4"],"ips":["192.0.2.4"],"has_pressed_keys":false,"resolving":false,"peer_commit":[49,50,51,52,53,54,55,56]}]]}\n
-> "Enumerate"\n<- {"Enumerate":[[1,{"hostname":"desk","fix_ips":[],"port":4242,"pos":"left","cmd":null},{"active":true,"active_addr":"192.0.2.4:4242","alive":true,"remote_ready":true,"dns_ips":["192.0.2.4"],"ips":["192.0.2.4"],"has_pressed_keys":false,"resolving":false,"peer_commit":[49,50,51,52,53,54,55,56]}]]}\n
-> {"SetClipboardText":false}\n<- {"ClipboardSettings":{"text":false,"image":true,"files":false}}\n```

The last event is the authoritative echo. Do not optimistically render `text=false` before receiving it; an error may instead be emitted. A frontend should issue `Sync` on connect, use `Enumerate` when it needs a fresh complete client list, and continue consuming events for changes.

## 8. Minimal Python client

This standard-library client uses the same framing on Unix. It sends `Sync`, decodes every event, and prints the resulting Python value. Set `SYNTRA_DAEMON_SOCKET` to select a non-default endpoint.

```python
import json
import os
import socket

socket_path = os.environ.get(
    "SYNTRA_DAEMON_SOCKET",
    os.path.join(os.environ["XDG_RUNTIME_DIR"], "syntra-daemon.sock"),
)

with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
    stream.connect(socket_path)
    stream.sendall(b'"Sync"\n')
    reader = stream.makefile("r", encoding="utf-8", newline="\n")
    for line in reader:
        if not line:
            break
        try:
            event = json.loads(line)
        except json.JSONDecodeError as exc:
            raise RuntimeError(f"invalid daemon event: {exc}") from exc
        print(event)
```

For Windows replace the socket construction and connection with `socket.create_connection(("127.0.0.1", 5252))`; the JSON framing and request/event values are unchanged. A production client should handle reconnects, EOF, malformed frames, and unknown variants explicitly.

## 9. Compatibility rules

* Variant names are wire format. Renaming `Sync`, `ClipboardSettings`, or any other variant changes the JSON token and is a breaking change even if Rust callers still compile. `variant_names_are_part_of_the_wire_format` pins exact text (`tests/contract.rs:65-82`).
* Unknown variants are rejected, not ignored. A new daemon event can therefore make an old strict decoder fail; do not silently drop it, because that would present an incomplete dashboard. `unknown_variants_are_rejected_not_ignored` pins this (`tests/contract.rs:84-91`).
* Preserve externally tagged shapes, field names, casing, and tuple ordering. The request round-trip test covers representative serialisations (`tests/contract.rs:36-63`).
* Preserve newline-free serialisation. The framing test pins this (`tests/contract.rs:12-34`).
* Treat missing optional values as `null`, not omitted fields, unless a future protocol revision explicitly changes the serde contract.

## 10. Security limitations

The frontend control socket has **no authentication and no capability scoping**. Any local process that can reach the Unix socket can send every request, including `StopService` and `RemoveAuthorizedKey`, and can read the daemon's events. On Windows, any process able to connect to loopback `127.0.0.1:5252` has the same unauthenticated access. The API does not provide a browser-origin check, user identity, per-request authorisation, or read-only mode. A web dashboard must not expose this socket directly to untrusted network clients; place any network bridge behind its own authentication and authorisation boundary.

`DiscoveredPeer` advertisements are also explicitly unauthenticated. Do not treat discovery as proof of identity; authenticated peer fingerprints and profiles are separate events. Profile and avatar bounds limit malformed payload size but are not an access-control mechanism.

## 11. Source map

* Contract types and serde derives: `crates/syntra-api/src/lib.rs:95-635`.
* Event enum: `crates/syntra-api/src/lib.rs:637-764`.
* Request enum: `crates/syntra-api/src/lib.rs:766-903`.
* Blocking framing and Windows endpoint: `crates/syntra-api/src/connect.rs:31-120`.
* Async framing and reconnect backoff: `crates/syntra-api/src/connect_async.rs:18-115`.
* Listener and per-connection line streams: `crates/syntra-api/src/listen.rs:23-150`.
* Socket/path resolution and overrides: `crates/syntra-api/src/paths.rs:17-119`.
* Compatibility and bounds tests: `crates/syntra-api/tests/contract.rs:12-171`.
## 12. Implementation checklist

Before presenting a connected dashboard, a client should:

1. Resolve the endpoint using the environment override and platform rule in §1.
2. Open a byte stream and treat each newline as the only frame delimiter.
3. Serialise requests with the exact outer variant name and exact tuple order.
4. Decode every line as one complete JSON value; reject trailing non-whitespace data.
5. Apply `Sync` and render only the events returned by the daemon.
6. Keep an in-memory map keyed by `ClientHandle`; replace entries on `Created` and `State`, and remove them on `Deleted`.
7. Treat `Enumerate` as a replacement snapshot, not as an incremental append.
8. Correlate transfer progress by `(transfer_id, file_id)` for clipboard transfers and by `(peer_fingerprint, transfer_id)` for manual transfers.
9. Treat `HistoryChanged` as cache invalidation and query again when the visible page is stale.
10. Display `Error`, `HistoryError`, `ManualTransferError`, and optional status error fields rather than hiding them.
11. Keep consuming events while controls are displayed; a setting can change because another frontend issued a request.
12. On EOF, mark the daemon unavailable, close the old stream, and reconnect with backoff.

## 13. Failure modes and defensive decoding

| Observation | Correct client response |
|---|---|
| Socket path cannot be resolved | Show a local configuration/runtime-directory error; do not substitute the peer port. |
| Unix socket is absent | Retry while the service may be starting, then show offline state. |
| Windows connection refused | Retry `127.0.0.1:5252`; this is the control endpoint, not UDP 4242. |
| A line is not valid JSON | Log the offending frame safely, report a protocol error, and stop or reconnect. |
| A decoded value is not an object/string matching the outer enum | Reject it; it is not a valid frontend event/request. |
| An unknown outer variant is received | Fail loudly or enter an explicitly incompatible state. Never ignore it. |
| `State` refers to an unknown handle | Create the map entry only after obtaining the corresponding configuration, or request `Enumerate`. |
| `NoSuchClient` follows a request | Remove any stale local representation and display the daemon's result. |
| `PortChanged` contains an error | Keep the previous confirmed port and expose the failure. |
| An optional error is non-null | Treat the operation as unsuccessful even if a settings-shaped payload is present. |
| `HistoryPage.next_offset` is null | There is no next page; do not manufacture one. |
| A profile exceeds name/avatar bounds | Reject it before publishing; never truncate the display name or pixel buffer. |

Do not use the examples' values as sentinels. Empty vectors, `null` optionals, zero counters, and `false` booleans are all legitimate wire values. In particular, an empty `Enumerate` means the daemon has no configured clients, while an absent `Enumerate` event means the snapshot has not arrived yet.

## 14. Versioning strategy

There is no negotiated frontend schema version in this contract. A client therefore needs a tested compatibility policy: pin the known variant set, surface an incompatibility when deserialisation rejects a new variant, and upgrade the client alongside the daemon when the contract changes. Do not attempt to recover by guessing whether a tuple is a struct or by lowercasing names. The derives in `lib.rs` are the wire specification, and the contract tests deliberately make accidental changes visible during review.

For a browser implementation, a small local native bridge is still required: browsers cannot normally open Unix-domain sockets or arbitrary loopback streams under the web security model. The bridge must preserve the same one-line JSON contract and enforce its own authentication before accepting network requests.

This reference intentionally documents only the daemon frontend boundary; peer UDP/DTLS messages and plugin stdio messages are separate contracts.
