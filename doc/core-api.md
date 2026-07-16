# UniversalLink — Core public API (local IPC)

> Specification of the API between the Core and the components (official and
> third-party). Complements [architecture.md](architecture.md) and
> [server-api.md](server-api.md).
> Status: phase-1 design, pre-implementation — the exact schemas will be frozen
> with the code. This API is **the project's extension point**: a component is
> any artifact capable of speaking this protocol.

## Principles

- **Separate control plane + data channel.** The control plane is JSON-RPC 2.0
  (LSP-style framing, `Content-Length`) over the socket described in
  [architecture.md](architecture.md) (UDS / named pipe, peer credentials). The
  data channel (raw bytes, range reads) is a separate connection to the same
  socket — see [The data channel](#the-data-channel).
- **Requests in both directions.** The component calls the Core, and the Core
  calls the component (e.g. `clipboard.get_data`). Both ends are simultaneously
  JSON-RPC client and server on the same full-duplex connection.
- **A local file's bytes never travel over the IPC**: components exchange paths,
  the Core reads/writes the disk itself. The *remote* bytes being downloaded are
  either written directly by the Core at the designated locations
  (`clipboard.fill_files`), or streamed via the data channel when the OS surface
  demands it. Only the clipboard's inline contents (text, image) travel over the
  control plane (base64 in v1).
- **Subscription-based notifications**: a component subscribes to topics, the
  Core pushes named notifications. No polling.

## Handshake and enrollment

`hello` is the only method callable before enrollment. A single **accepted**
`hello` per connection; a refused `hello` leaves the connection pristine and can
be retried (fix your scopes without reconnecting):

```
component → Core : hello { name, version, role, scopes: [...], token? }
```

Possible responses:

- **Valid token** (or bootstrap token) → `{ status: "ok", granted_scopes,
  api_version }`. The connection is active.
- **No token (unknown third-party component)** → `{ status: "pending" }`. The
  Core queues the request and flags it (the GUI if connected, otherwise the
  tray). When the user decides, the Core notifies on the connection:
  `enrollment.decided { approved, token?, granted_scopes? }`. The token is
  persistent: subsequent connections go through the nominal path.
- **Invalid/revoked token** → `INVALID_TOKEN` error.

Trust roots (detail in [architecture.md](architecture.md)): ephemeral token
passed at spawn for the components launched by the Core; file token (0600, config
folder) for the GUI and for debugging. The `components.approve` scope is grantable
only through these two paths, never via the prompt.

### Roles

| Role | Particularity |
|---|---|
| `gui` | the only role that receives approval requests (with the `components.approve` scope) |
| `clipboard-backend` | **exclusive**: only one active at a time; a second `hello` with this role → `ROLE_CONFLICT` (replacing the official backend with a third-party one is a configuration choice) |
| `menu-backend` | — |
| `tray` | — |
| `custom` | generic third-party components |

## Scopes

| Scope | Grants access to |
|---|---|
| `session.read` | `session.status`, `account.status`, the `session` topic |
| `session.manage` | `session.login`, `session.logout`, `session.reload`, `account.setup`, `account.join` |
| `devices.read` | `devices.list`, the `devices` topic |
| `devices.manage` | `devices.rename`, `devices.revoke` |
| `files.send` | `files.send`, `files.cancel` (any transfer, outgoing or incoming — components are the user's trusted agents; the `transfer_id` is random, non-enumerable) |
| `transfers.read` | the `transfers` topic |
| `clipboard.write` | `clipboard.updated`, answering `clipboard.get_data` |
| `clipboard.read` | the `clipboard` topic, `clipboard.fetch`, `clipboard.open_file`, `clipboard.fill_files` |
| `components.approve` | `components.*` — never grantable via the prompt |
| `system.shutdown` | `system.shutdown` — stops the whole Core (the tray's Quit) |

Verification: per method and per topic. Example profiles — menu manager:
`devices.read + files.send`; tray: `session.read + devices.read +
transfers.read`; clipboard manager: `devices.read + clipboard.read +
clipboard.write`.

## Subscribing to events

```
events.subscribe { topics: ["session", "devices", "transfers", "clipboard"] }
```

Topics filtered by scopes. Notifications are named (below, by namespace). After a
(re)connection, a component resynchronizes its state through the snapshot methods
(`devices.list`, `session.status`…) then subscribes.

## `session.*`

| Method | Description |
|---|---|
| `session.status {}` | → `{ logged_in, server_connected, account?, configured }`. `configured`: whether a server + OIDC is set — distinguishes "never configured" (→ first-run setup) from "configured but the server is down" |
| `session.login {}` | starts the OIDC flow (PKCE + loopback) → `{ auth_url }`. **The caller** opens the browser — the Core does not touch the UI. Completion signaled by `session.changed` |
| `session.logout {}` | closes the server session |
| `session.reload {}` | re-reads `config.json` (which the GUI's setup screen has just written) and swaps the server config in place — no restart. → the fresh `session.status`. `INVALID_CONFIG` if the file is malformed / half-filled. The Core only READS the file; the GUI is its sole writer |

Notification: `session.changed { logged_in, server_connected, account? }` — note it carries NO `configured` (a caller that needs it re-reads `session.status`, which a session change prompts anyway).

## `account.*` (account key, C7)

The account's root of trust: an account key (derived from a **recovery code**)
attests that each `node_id` is indeed one of the user's devices, independently of
the server (see [server-api.md](server-api.md), "Account attestation", and
[architecture.md](architecture.md)). Each device derives the key from the code,
attests ITS `node_id`, persists `ak_pub` + its attestation, then **discards** the
private key: after onboarding, no device holds the account's private key at rest.

| Method | Description |
|---|---|
| `account.status {}` | → `{ attested: bool, fingerprint: string? }`. `fingerprint` = fingerprint (safety number) of the account key, to compare across devices (out-of-band verification) |
| `account.setup {}` | **first device**: generates the code, derives the key, attests and publishes → `{ recovery_code, fingerprint }`. `recovery_code` is the ONLY copy of the private key — display it once and hand it to the user. `ACCOUNT_KEY_SET` if a key already exists |
| `account.join { recovery_code }` | **subsequent device**: re-derives the key from the entered code, attests and publishes → `{ fingerprint }`. `INVALID_CODE` if the code is malformed or wrong (checksum); `ACCOUNT_KEY_SET` if a key already exists |

The same key ⇒ the same `fingerprint` on every device: a divergence betrays a
wrong code (the device would then remain outside the account, *fail-closed*) or a
substitution. Replacing an existing key (rotation) is a follow-up building block —
v1 refuses it (`ACCOUNT_KEY_SET`). `account.setup`/`account.join` assume the
server is reachable (`SERVER_UNREACHABLE` otherwise) and return
`ACCOUNT_KEY_SAVE_FAILED` if the root cannot be persisted (folder not writable) —
nothing is installed in that case.

## `devices.*`

The device record is the one from [server-api.md](server-api.md), enriched by the
Core with an `is_self` field.

| Method | Description |
|---|---|
| `devices.list {}` | → `[ device, … ]` (snapshot, includes the local device) |
| `devices.rename { device_id, name }` | proxy to the server |
| `devices.revoke { device_id }` | → `{ status: "done" }` or `{ status: "reauth_required", auth_url }` (fresh ID token required by the server; the caller opens the URL, completion arrives via `device.removed`) |

Notifications: `device.added / removed / online / offline / updated { … }` — same
payloads as on the server side.

## `files.*`

| Method | Description |
|---|---|
| `files.send { device_id, paths[] }` | → `{ transfer_id }`. Fire-and-forget: the Core reads the disk and streams via iroh, tracking goes through the events. **v1: flat files** — each path must be a regular file, a folder, or a missing path → `-32602` (directory trees are a follow-up building block) |
| `files.cancel { transfer_id }` | cancels an outgoing OR incoming transfer |

`device_id` is resolved by the directory, **C7 attestation verified before any
opening**: a target that is absent or attested under a foreign key →
`DEVICE_UNKNOWN` (fail-closed, indistinguishable so as to disclose nothing); known
but with no published relay → `DEVICE_OFFLINE`. Once the `transfer_id` has been
returned, failures (connection, disk, a target that has shrunk) go through
`transfer.failed`.

Reception: **auto-accepted in v1** (these are the user's devices, authenticated by
the account key). The bytes land in the configured receive folder (see
[deployment.md](deployment.md), `UNIVERSALLINK_RECEIVE_DIR`), each file via a
temporary renamed atomically **at the end** of the transfer — nothing partial is
ever exposed, and a cancellation/error leaves no trace of it. Name collision →
"(n)" suffix, never an overwrite. The received name must be a **simple basename**:
refused (the transfer fails) if it carries a separator (`/` or `\`), `..`, `:`, or
a control character — a legitimate sender only sends a basename, and a peer cannot
write outside the receive folder. (The refusal is identical on every OS: no
platform-dependent path splitting.)

The channel is the data-plane stream (one bidirectional iroh connection per
transfer): offer (manifest) + bodies concatenated in the outbound direction, a
single acknowledgment on the way back; the `transfer_id` is specific to each side
(no cross-device correlation in v1).

Notifications (topic `transfers`):

| Notification | Emitted when |
|---|---|
| `transfer.incoming { transfer_id, device_id, files }` | a device sends us files (`files` = manifest `[{name, size}]`) |
| `transfer.started { transfer_id, device_id, files, total }` | the actual start of a send (will include `clipboard.fill_files` fills) |
| `transfer.progress { transfer_id, done, total }` | throttled by the Core (~2/s; the first and last point are always emitted) |
| `transfer.finished { transfer_id, paths? }` / `transfer.failed { transfer_id, error }` | end (`paths` = files written, on the receiving side; `error: "cancelled"` on cancellation) |

## `clipboard.*`

**Pull-at-paste** model: on copy, only the metadata circulates; the bytes move
only at paste time. v1 normalized formats: `text`, `image/png`, `files` — the
conversion from/to the OS formats is the backend's responsibility, the Core only
transports normalized content. Last copier wins, across all machines. The
anti-echo (not re-announcing one's own writes) is a contract of the backend.

### Source side (the PC where you copy)

| Direction | Call | Description |
|---|---|---|
| component → Core | `clipboard.updated { clip_id, formats: [{format, size?}], paths?, sensitive? }` | announces the local copy. `clip_id`: generated by the backend, locally unique (the Core prefixes it with the device). `paths` mandatory if `files`: the Core builds the **manifest** (`stat` only, instant announcement — names, sizes, tree). `sensitive`: set if the OS confidentiality markers are detected |
| Core → component | `clipboard.get_data { clip_id, format }` → `{ data }` | **request** from the Core when a remote device fetches an inline format. The backend reads the OS clipboard. `CLIP_STALE` if the clipboard has changed since |

The files never pass back through the backend: the Core serves their bytes from
the disk (manifest paths), validating the size at read time (`SIZE_CHANGED`
otherwise).

### Destination side (the PC where you paste)

| Direction | Call | Description |
|---|---|---|
| Core → component | notification `clipboard.remote_updated { device_id, clip_id, formats, files?: [{file_id, name, size, dir}], sensitive? }` | a device has copied; `files` is the manifest. The backend takes ownership of the OS clipboard with promised data |
| component → Core | `clipboard.fetch { clip_id, format }` → `{ data }` | paste of an **inline** format (text, image) |
| component → Core | `clipboard.open_file { clip_id, file_id }` → `{ channel_token, size }` | opens a manifest entry for range reads on the data channel (consumer-driven surfaces: IStream, FUSE, NFS, FSKit…) |
| component → Core | `clipboard.fill_files { clip_id, entries: [{file_id, dest_path}] }` | the backend designates target files (NSFilePresenter skeletons, spool…), **the Core fills them** and answers at the end. Tracked via `transfer.*` |

All the fetch paths can fail with `CLIP_STALE` (the source no longer recognizes
this `clip_id`), `SIZE_CHANGED`, or `DEVICE_OFFLINE` — the backend must release
its promise cleanly (paste refused, never silently truncated content).

## `components.*`

Reserved for the `components.approve` scope.

| Method | Description |
|---|---|
| `components.list {}` | `[{ component_id, name, role, scopes, connected, enrolled }, …]` — the enrolled third parties (even disconnected) and the bootstrap connections. `enrolled: false` = spawn token or file token: no persistent token to revoke, `components.revoke` would only close the connection |
| `components.pending {}` | pending requests |
| `components.approve { request_id, scopes }` / `components.deny { request_id }` | decide a request (granted scopes ⊆ requested scopes) |
| `components.revoke { component_id }` | invalidates the token; any existing connection is closed |

Notification: `component.pending { request_id, name, role, scopes, peer_info }`
(binary, pid — derived from the peer credentials).

## `system.*`

| Method | Description |
|---|---|
| `system.shutdown {}` | → `{}`. Stops the whole Core — the tray's Quit. The Core replies, then tears down in order (components, then the IPC, then the data plane). Receiving a file with the window closed stops until the GUI is reopened, which respawns the Core. Guarded by the `system.shutdown` scope, strictly stronger than `session.read`: killing the daemon is not something a status reader may do |

## The data channel

For consumers that drive the read themselves (Explorer via IStream, FUSE, NFS,
FSKit — and later the GUI's drag & drop, which will consume the same primitive).

1. The component obtains a `channel_token` (`clipboard.open_file`) — single-use,
   short-lived, bound to the component and to the opened entry.
2. It opens a **second connection** to the socket and presents the token.
3. The connection becomes a binary range-read protocol (exact framing to be
   frozen at implementation time):
   - component → Core: `READ { offset, len }`, `ABORT`
   - Core → component: `DATA { offset, bytes }`, `EOF`, `ERROR { code }`
     (`CLIP_STALE`, `SIZE_CHANGED`, `PEER_GONE`, `TIMEOUT`)

Contractual properties: optimized sequential reads (read-ahead on the Core side),
`seek` supported (an arbitrary range is valid, at the cost of reopening the network
stream), **error propagable mid-read** (never a silent truncation), **cancellation
in both directions** (closing the connection = reset of the iroh stream; paste
abandoned on the OS side = `ABORT`), stall timeout on the Core side. One channel =
one opened entry.

## Errors

Standard JSON-RPC codes + application codes in `error.data.code`:

| Code | Meaning |
|---|---|
| `NOT_ENROLLED` | method called before an accepted `hello` |
| `PENDING_APPROVAL` | enrollment request still pending |
| `INVALID_TOKEN` | unknown or revoked token |
| `SCOPE_DENIED` | scope missing for the method or the topic |
| `ROLE_CONFLICT` | exclusive role already taken (`clipboard-backend`) |
| `ALREADY_LOGGED_IN` | `session.login` while a session is open (re-logging in starts with `session.logout`) |
| `INVALID_CONFIG` | `session.reload` on a malformed / half-filled `config.json` (the message carries the reason) |
| `SERVER_UNREACHABLE` | operation requiring the server, offline |
| `DEVICE_UNKNOWN` / `DEVICE_OFFLINE` | target unknown / unreachable |
| `TRANSFER_UNKNOWN` | unknown `transfer_id` |
| `FORMAT_UNKNOWN` | format not present in the clip |
| `CLIP_STALE` | the `clip_id` is no longer the source's current clipboard |
| `SIZE_CHANGED` | file modified between the copy and the read |

## Versioning

- `api_version` is returned by `hello`.
- Tolerant JSON: unknown fields ignored, additive extensions (methods,
  notifications, topics, optional fields, new normalized formats).
- Incompatible change = major increment; the Core announces the supported range
  and the component refuses cleanly if incompatible.
