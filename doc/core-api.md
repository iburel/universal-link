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
  (`transactions.fill`), or streamed via the data channel when the OS surface
  demands it. The clipboard's inline contents (text, image) move over the data
  channel too: the control plane carries control, never payloads.
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
| `clipboard.write` | `clipboard.updated`, answering `clipboard.get_data` — both additionally require the `clipboard-backend` role (announcing is the exclusive backend's privilege) |
| `clipboard.read` | the `clipboard` topic, `clipboard.current`, `transactions.open`, `transactions.fill` |
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
| `transfer.started { transfer_id, device_id, files, total }` | the actual start of a send (will include `transactions.fill` fills) |
| `transfer.progress { transfer_id, done, total }` | throttled by the Core (~2/s; the first and last point are always emitted) |
| `transfer.finished { transfer_id, paths? }` / `transfer.failed { transfer_id, error }` | end (`paths` = files written, on the receiving side; `error: "cancelled"` on cancellation) |

## Transactions

The object at the heart of everything that serves bytes across devices: a
**transaction** is a capability minted by the source Core that grants the right
to read a frozen set of resources. The clipboard is its first producer (one
copy = one transaction); a shared folder will simply be a long-lived
transaction with an explicit revocation instead of an automatic expiry. The
`tx_id` is unguessable and never reused: holding it (plus being an
authenticated device of the account, on the network side) is the
authorization, and the source verifies that every requested `format` /
`file_id` belongs to the transaction before serving a byte.

Two kinds of resources, split by who holds the bytes:

- **Inline formats** (`text`, `image/png`): the bytes live in the OS clipboard
  and only the source backend can read them — the Core pulls them from it at
  paste time (`clipboard.get_data`). If the OS clipboard changed since the
  announce, they no longer exist anywhere: `CLIP_STALE`. Deliberate limit of
  pull-at-paste (nothing is snapshotted — a copied password never sits in the
  Core's memory), with a negligible window: an inline paste is a single fetch.
  A **materialized** transaction lifts exactly this limit for a source that is
  only briefly alive (a phone) — see *Materialized transactions* below.
- **Files**: the backend hands over paths; the Core canonicalizes them and
  freezes the **manifest** at announce time (`stat` only — canonical paths,
  sizes, and each file's identity: device + inode where the OS gives one, plus
  mtime; no byte is read). From then on the Core serves the bytes from the disk
  itself: what the OS clipboard has since become no longer matters. Reads are
  strictly bounded to the manifest: at open time the Core re-verifies that the
  canonical path still resolves to the frozen identity — a swapped symlink, a
  replaced file, or a same-size rewrite fails with `FILE_CHANGED`, never a
  silent truncation and never silently different bytes.

### Lifecycle

1. **Born** at the announce (`clipboard.updated` → `tx_id`).
2. **Consumed** through sessions: an open consumer channel, or an in-flight
   `transactions.fill`. Closing the channel (or the fill ending) ends the
   session — there is no explicit "paste done" call: a crashed consumer is just
   a dead connection swept by the stall timeout. A live session's rights last
   exactly as long as its activity — deliberate: consumers are the account's
   own devices, and cutting a half-done folder paste would be worse than
   letting it finish.
3. **Superseded** by the next announce — its own device's, or a newer one
   learned from another device (last copier wins **globally**: Cores converge
   on the most recent announce, ordered by announce timestamp then `device_id`;
   best-effort clocks are fine — what matters is that every device elects the
   same winner). A superseded transaction refuses NEW sessions (`TX_STALE`) but
   the active ones run to completion — copying something else never cancels an
   in-flight paste, which keeps reading its frozen manifest, exactly as a local
   paste survives the next copy.
4. **Deleted** once superseded with zero active sessions. Until superseded, it
   serves any number of pastes (copy once, paste N times, from several
   devices).

Supersession is the graceful exit; the source Core stopping or logging out is
not — both **cut** active sessions (`ERROR { TX_STALE }` on open channels) and
drop every transaction. The shared folders' future explicit revocation will
take the same cutting path: revoking must mean *now*. Consuming a transaction
requires the scope of its producer — `clipboard.read` for a clipboard
transaction.

Very large trees: v1 freezes the full manifest at the announce and **caps it**
(65,536 entries; beyond, the announce fails with `MANIFEST_TOO_LARGE` — a
runaway copy is refused up front instead of killing connections with an
oversized frame). Lazy enumeration (which shared folders will need) is an
additive extension: `file_id` is opaque and the manifest can become pageable
without breaking consumers.

### Materialized transactions (push-at-copy)

Pull-at-paste assumes the source is still alive when a peer pastes: the source
Core re-reads its OS clipboard (inline) or its disk (files) on demand. A source
that is only briefly alive around the copy — a phone sharing a snippet from an
app the OS then kills — cannot answer that later pull: the announcing
connection, and the source Core with it, is gone (`CLIP_STALE`).

A **materialized** transaction inverts the inline path for exactly that case:
the source pushes the inline bytes to the account's online devices *at copy
time*, and each destination Core caches them. A later paste is served from that
cache, entirely locally — the source is never contacted, so it may vanish the
instant the push completes. The share gesture is explicit, so spilling the
bytes eagerly is the intent, not a leak.

Constraints keep it a narrow, safe extension:

- **Inline formats only** (`text`, `image/png`), never `files` — a file clip is
  already a push when it needs to be (`files.send`), and a manifest is not
  bytes. Bounded: the materialized payload is capped (a few MiB); a runaway is
  refused at the announce.
- **Never `sensitive`** — a concealed clip stays pull-at-paste, so its bytes
  move only to the device that actually pastes and never sit in the memory of
  devices that do not. A `materialize` request that also sets `sensitive` is
  refused.
- **Additive** — a non-materialized copy is unchanged (pull-at-paste). A
  destination holding the cached bytes serves them locally (no `DEVICE_OFFLINE`
  at `transactions.open`, no `PEER_GONE` at paste, even if the source has since
  gone offline); a destination that was offline at copy time simply never
  learned the clip, exactly as a missed announce today.

Supersession and the Core-stop/logout cut drop the cached bytes with the
transaction, like any other: a materialized clip is deleted (and its bytes
freed) the moment it is superseded with no active session.

## `clipboard.*`

**Pull-at-paste** model: on copy, only the metadata circulates (as a
transaction); the bytes move only at paste time. v1 normalized formats: `text`,
`image/png`, `files` — the conversion from/to the OS formats is the backend's
responsibility, the Core only transports normalized content. Last copier wins,
across all machines. The anti-echo (not re-announcing one's own writes) is a
contract of the backend.

### Source side (the PC where you copy)

| Direction | Call | Description |
|---|---|---|
| component → Core | `clipboard.updated { formats: [{format, size?}], paths?, sensitive?, materialize?, blobs? }` → `{ tx_id }` | announces the local copy: opens the transaction that supersedes the previous one. `paths` mandatory if `files` (the manifest is frozen from them). `formats` may be empty — the clipboard was cleared; it supersedes like any announce (a contentless transaction), and destinations withdraw their promise. Inline `size` is an advisory hint (the content is re-serialized at paste time; the stream up to `EOF` is authoritative, a mismatch is not an error) and is omitted when `sensitive`. `sensitive`: set if the OS confidentiality markers are detected. `materialize: true` makes it a **materialized** transaction (push-at-copy): the caller supplies the inline bytes now as `blobs: { <format>: <base64> }` (one entry per inline format offered, capped), the Core pushes them to the account's online devices, and it also serves the source's own pastes from them — so the caller may exit right after the copy. It excludes `sensitive` and `files` (rejected). The backend keeps the returned `tx_id` mapped to that clipboard generation |
| Core → component | `clipboard.get_data { tx_id, format, channel_token }` → `{}` | **request** from the Core when a device pastes an inline format: the backend re-reads the OS clipboard, streams the blob over the provider channel opened with `channel_token`, and replies `{}` only after `EOF` — the reply is the completion signal. It replies `CLIP_STALE` *without opening the channel* if it cannot vouch for the `tx_id` generation (the OS clipboard moved on — or this backend instance never knew it); a failure detected mid-stream surfaces as `ERROR` on the channel and mirrors in the reply |

The files never pass through the backend: the Core serves their bytes from the
disk (manifest paths). `clipboard.get_data` is only ever addressed to the
connection that announced the transaction; if it is gone, the Core fails
inline pulls with `CLIP_STALE` itself — a fresh backend cannot vouch for a
generation it never saw. After a (re)start the backend resynchronizes with
`clipboard.current` and announces only on the next observed change: blindly
re-announcing at startup would wrongly supersede a newer copy from another
device (the anti-echo contract, extended).

### Destination side (the PC where you paste)

| Direction | Call | Description |
|---|---|---|
| Core → component | notification `clipboard.remote_updated { device_id, tx_id, formats, files?: [{file_id, path, size, dir?}], sensitive? }` | a device has copied; `files` is the manifest (`path`: relative, `/`-separated, unique — the announcing Core suffixes collisions with "(n)", as in reception). Empty `formats`: the source cleared its clipboard — the backend withdraws its promise (touching the OS clipboard only if it still owns it). Otherwise the backend takes ownership of the OS clipboard with promised data |
| component → Core | `clipboard.current {}` → the current global clip (`{ device_id, tx_id, formats, files?, sensitive? }`, or `{}` if none) | the `clipboard` topic's **snapshot method**, per the resync rule: a (re)connecting backend re-learns the live promise before subscribing |
| component → Core | `transactions.open { tx_id }` → `{ channel_token }` | opens a **consumer channel** — a paste session. One request at a time per channel: open as many channels as the paste needs concurrency |
| component → Core | `transactions.fill { tx_id, entries: [{file_id, dest_path}] }` → `{ transfer_id }` | the backend designates target files (NSFilePresenter skeletons, spool…), **the Core fills them**. Fire-and-forget like `files.send`: progress and completion arrive via `transfer.*`, cancellation via `files.cancel`. `dest_path` comes from the enrolled backend — the user's agent, the `files.send` trust model; the remote manifest never chooses where bytes land |

The receiving Core **re-validates every manifest before delivering it** —
relative `/`-separated paths only, no `..`, no rooted or absolute segment, no
`:` or control character, no duplicate — and drops the announce otherwise
(fail-closed, exactly like reception): a naive backend joining `path` onto its
paste target must not be a confused deputy for a compromised peer.

`transactions.fill` details: `entries` reference non-`dir` entries only (the
backend creates the directories — it has the manifest); the Core creates each
`dest_path`'s missing parents. On `transfer.failed` (error or cancellation)
the backend discards whatever the `transfer.*` events did not confirm — the
paste surface is its promise, and temp-plus-atomic-rename is not possible on
OS-watched skeleton paths. A backend that disconnects mid-fill cancels it.

On a consumer channel the backend pulls what the OS asks for, in the order the
OS asks for it — a whole inline blob, or arbitrary ranges of a manifest file,
as if the file were local. Every pull can fail — `TX_STALE`, `CLIP_STALE`
(inline only), `FILE_CHANGED`, `DEVICE_OFFLINE` at `transactions.open`,
`PEER_GONE` mid-stream — and the backend must release its promise cleanly
(paste refused, never silently truncated content). `sensitive` is not
advisory: the destination backend re-applies the OS confidentiality markers
when it takes ownership, and no component may persist a sensitive clip's
contents (history, logs).

## `components.*`

Reserved for the `components.approve` scope.

| Method | Description |
|---|---|
| `components.list {}` | `[{ component_id, name, role, scopes, connected, enrolled }, …]` — the enrolled third parties (even disconnected) and the bootstrap connections. `enrolled: false` = spawn token or file token: no persistent token to revoke, `components.revoke` would only close the connection |
| `components.pending {}` | pending requests |
| `components.approve { request_id, scopes }` / `components.deny { request_id }` | decide a request (granted scopes ⊆ requested scopes) |
| `components.revoke { component_id }` | invalidates the token; any existing connection is closed |

Notification: `component.pending { request_id, name, role, scopes, peer_info }`
(binary, pid — derived from the peer credentials). It has no topic and needs
no subscription: the Core pushes it to every connected `gui`-role component
holding `components.approve`.

## `system.*`

| Method | Description |
|---|---|
| `system.shutdown {}` | → `{}`. Stops the whole Core — the tray's Quit. The Core replies, then tears down in order (components, then the IPC, then the data plane). Receiving a file with the window closed stops until the GUI is reopened, which respawns the Core. Guarded by the `system.shutdown` scope, strictly stronger than `session.read`: killing the daemon is not something a status reader may do |

## The data channel

Payloads never ride the control plane: file ranges AND inline blobs move over a
**data channel** — a second connection to the same socket — so a heavy paste
never delays a `session.status`. Built for consumers that drive the read
themselves (Explorer via IStream, FUSE, NFS, FSKit — and later the GUI's
drag & drop, which will consume the same primitive).

A `channel_token` is unguessable (CSPRNG — like `tx_id`, possession is the
authorization), single-use, short-lived, and bound to one transaction, one
component and one direction: the Core accepts it only from a connection whose
peer credentials match the component it was minted for, and closes anything
else. The bearer opens a **second connection** to the socket, presents the
token, and the connection becomes a binary protocol (exact framing frozen at
implementation time):

- **Consumer channel** (destination side, token minted by `transactions.open`)
  — the component drives, one request in flight per channel:
  - component → Core: `FETCH { format }` (a whole inline blob) · `READ {
    file_id, offset, len }` (a file range) · `ABORT` (cancels the in-flight
    request; the channel stays usable)
  - Core → component: `DATA { offset, bytes }`, `EOF`, `ERROR { code }`
    (`TX_STALE`, `CLIP_STALE`, `FILE_CHANGED`, `FILE_UNKNOWN`,
    `FORMAT_UNKNOWN`, `PEER_GONE`, `TIMEOUT`)
  - Every request is answered by `DATA*` then `EOF` — `EOF` terminates the
    *response*, not the file: a `READ` crossing the end of the file returns
    the intersection (possibly zero bytes) then `EOF`. `DATA` arrives in
    order; `offset` is absolute (file-relative for `READ`, 0-based for
    `FETCH`). An `ERROR` ends only the request — the channel stays usable —
    except `TX_STALE` and `PEER_GONE`, which end the session: the Core closes
    the channel. `READ` on a `dir` entry → `FILE_UNKNOWN` (a directory conveys
    the tree; it has no bytes).
- **Provider channel** (source side, token carried by `clipboard.get_data`) —
  the backend writes the requested blob: `DATA*` then `EOF`, or `ERROR { code }`
  (`CLIP_STALE`); the `clipboard.get_data` reply follows `EOF` — the RPC
  response is the completion signal.

Contractual properties: optimized sequential reads (read-ahead on the Core
side), `seek` supported (an arbitrary range is valid, at the cost of reopening
the network stream), **error propagable mid-read** (never a silent truncation),
**cancellation in both directions** (closing the connection = reset of the
network stream; paste abandoned on the OS side = `ABORT`), stall timeout on the
Core side. Closing the channel ends the paste session it materialized.

Network mapping (informative): between Cores, one iroh connection per device
pair and **at least one stream per transaction** — one transaction's traffic
never queues behind another's (a small copy pastes instantly while a big one is
still pouring), and a consumer channel's requests relay 1:1 onto such a stream.
A materialized transaction instead pushes its inline bytes source → destination
at copy time (one stream per online device, the metadata frame then the blobs);
the destination caches them and serves its pastes with no stream at all. The
exact wire protocol is out of scope for this document.

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
| `ACCOUNT_KEY_SET` | `account.setup` / `account.join` while an account key already exists (rotation is a follow-up) |
| `INVALID_CODE` | `account.join`: malformed or wrong recovery code (checksum) |
| `ACCOUNT_KEY_SAVE_FAILED` | the account root cannot be persisted (folder not writable) — nothing is installed |
| `DEVICE_UNKNOWN` / `DEVICE_OFFLINE` | target unknown / unreachable |
| `TRANSFER_UNKNOWN` | unknown `transfer_id` |
| `FORMAT_UNKNOWN` | format not present in the transaction |
| `FILE_UNKNOWN` | `file_id` absent from the manifest — or a `dir` entry, which has no bytes to read |
| `TX_STALE` | `tx_id` unknown or superseded: no new session. Supersession lets active sessions finish; a Core stop, logout, or (future) explicit revocation cuts them |
| `CLIP_STALE` | inline formats only: the source backend can no longer vouch for the announce's clipboard generation (the OS clipboard changed, the backend restarted or is gone) |
| `FILE_CHANGED` | the file behind a manifest entry is no longer the frozen one (size, identity, or mtime): the read is refused rather than serving different bytes |
| `MANIFEST_TOO_LARGE` | announce refused: the copy exceeds the v1 manifest cap |
| `PEER_GONE` | data channel: the source device vanished mid-stream (`DEVICE_OFFLINE` is its control-plane twin, at `transactions.open`) |
| `TIMEOUT` | data channel: stall timeout on the Core side |

## Versioning

- `api_version` is returned by `hello`.
- Tolerant JSON: unknown fields ignored, additive extensions (methods,
  notifications, topics, optional fields, new normalized formats).
- Incompatible change = major increment; the Core announces the supported range
  and the component refuses cleanly if incompatible.
