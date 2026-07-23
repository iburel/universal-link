# UniversalLink — Server public API

> Specification of the API between the Core and the Server. Complements
> [architecture.md](architecture.md). Status: phase-1 design, pre-implementation —
> the exact schemas will be frozen with the code.

## Scope

The server has four jobs, and only four:

1. **Authenticate** accounts (OIDC) and devices (device key).
2. **Hold the directory** of an account's devices (enrollment, revocation,
   metadata).
3. **Broadcast presence** (online/offline, status).
4. **Provide the composition info**: what is needed to reach a device via iroh
   (`node_id`, `relay_url`).

What is **deliberately not** in this API: file data, clipboard metadata, transfer
offer/negotiation, any device-to-device message. All of that goes through the
end-to-end encrypted iroh streams. The server sees neither the content nor the
activity — only connections, heartbeats, and the directory.

## Transport

- **One persistent WSS connection per device**, carrying **JSON-RPC 2.0** in both
  directions: client → server requests, server → client notifications. Same
  conventions as the client's local IPC (a single protocol grammar in the
  project).
- **The connection is the presence**: a device authenticated on an open socket =
  online; a closed socket = offline. Heartbeat via WebSocket ping/pong
  (indicative: every 30 s, offline after 2 failures).
- If a device opens a second connection, **the new one replaces the old** (the
  old one is closed) — one device = at most one connection. A replaced connection
  is no longer the device's presence: a late `presence.update` it might emit
  (racing with its own closure) is silently ignored, it does not overwrite the
  state published by the current connection.
- Outside the WebSocket: `GET /health` (monitoring). TLS mandatory everywhere —
  terminated by the server or by an upstream reverse proxy (the server can then
  listen in cleartext on its internal network).

## Authentication

### Identities

- **Account** = the pair `(OIDC issuer, sub)`. First supported issuer:
  **Google** (`accounts.google.com`). The issuer is a server configuration, not a
  protocol assumption — other IdPs can be added without changing the API.
- **Device** = an Ed25519 key pair: the same as the iroh identity
  (`node_id` = public key). A single identity per device, used both for the
  server and for the peers.

### Enrollment (once per device)

1. The Core obtains an **ID token** via OIDC (authorization code + PKCE, system
   browser).
2. On the WSS connection: `auth.challenge` → nonce, then `auth.enroll` with the
   ID token, the device metadata, and the **signature of the nonce** by the
   device key (proof of possession — prevents registering someone else's
   `node_id`).
3. The server validates the ID token (signature via the issuer's JWKS, `aud`,
   `exp`), verifies the proof, creates the device under the account `(iss, sub)`.
   The JWKS is cached and re-fetched when a token carries an unknown key id, so
   an issuer key rotation is picked up without a restart (rate-limited by
   `UNIVERSALLINK_JWKS_REFRESH_MIN_SECS`).

### Nominal connection (at every startup)

`auth.challenge` → nonce, then `auth.authenticate` signed by the device key. **No
OIDC in nominal operation**: a PC boots and connects even if the user has not
opened a browser in months.

### Sensitive operations

`auth.enroll` and `devices.revoke` require a **fresh** OIDC ID token (user
re-auth). The device key alone is not enough: a compromised device must not be
able to enroll accomplices or revoke the others.

## The device record

The central object, carried by `devices.list` and every notification:

```json
{
  "device_id": "d_7f3a…",
  "name": "Office-PC",
  "platform": "windows | macos | linux",
  "node_id": "<iroh public key>",
  "relay_url": "https://relay.example/…",
  "attestation": "<hex signature, or null>",
  "online": true,
  "status": null,
  "last_seen": "2026-07-09T15:04:05Z"
}
```

- `node_id` + `relay_url` = everything a peer must know to compose via iroh. The
  directory **is** the discovery mechanism (no iroh DNS/pkarr discovery).
- `relay_url` dies with the connection: when the device goes offline, it is
  cleared (`null`) — a relay from the previous session must not be re-served as
  current. The device re-publishes a fresh one (`auth.authenticate` or
  `presence.update`) at every reconnection.
- `attestation`: an **opaque blob** for the server — see "Account attestation"
  below. Unlike `relay_url`, it SURVIVES going offline (it is bound to the
  `node_id`, which is stable).
- `status`: an optional free field, reserved for extensibility (idle, busy…). v1
  defines no value for it.

### Account attestation (C7)

The server asserts which `node_id`s belong to an account — but a compromised
server could inject a foreign `node_id` into the directory and pass it off as one
of the user's devices. To remove this trust from the server, each device
publishes an **attestation**: a signature by an **account key** (distinct from the
device keys, derived by the user from a recovery code, never known to the server)
binding its `node_id` to the account.

The server **merely carries** this blob (`presence.update`) and rebroadcasts it in
the record — it **never** decodes or verifies it. It is the **peer** that verifies
it under the account key it holds: a `node_id` without a valid attestation is not
authorized (*fail-closed*). The server thus stays blind, and can neither forge a
member nor substitute the key. (Detail of the signed schema: `doc/architecture.md`.)

## Methods (client → server)

| Method | Auth required | Description |
|---|---|---|
| `auth.challenge {}` | none | Returns `{ nonce }` (anti-replay, single-use, short-lived) |
| `auth.enroll { id_token, node_id, name, platform, proof }` | OIDC ID token + key proof | Creates the device under the account → `{ device_id, api_version, device }` |
| `auth.authenticate { device_id, proof, relay_url? }` | key proof | Binds the connection to the device (→ online) → `{ api_version, device }` (its own record) |
| `devices.list {}` | session | Snapshot of the account's directory → `[ device, … ]` |
| `devices.rename { device_id, name }` | session | Renames any device of the account (handy from the GUI of another PC) |
| `devices.revoke { device_id, id_token }` | session + fresh OIDC | Strikes the device from the directory; its existing connection is closed (`DEVICE_REVOKED`) |
| `presence.update { status?, relay_url?, attestation? }` | session | Updates its own record; broadcast to the others via `device.updated`. `attestation` = opaque account blob (C7), carried without being interpreted |

`proof` = Ed25519 signature of the current nonce by the device's private key.

## Notifications (server → client)

Broadcast to all the account's connected devices, **except the connection that
originated the change** (the requester has the response):

| Notification | Emitted when |
|---|---|
| `device.added { device }` | a device is enrolled |
| `device.removed { device_id }` | a device is revoked |
| `device.online { device }` | a device authenticates |
| `device.offline { device_id, last_seen }` | connection closed or heartbeat lost |
| `device.updated { device }` | rename, `presence.update`, change of composition info |

Two connection closures do **not** produce a `device.offline`: replacing a
connection with a new one (the others see a simple `device.online`, no
offline/online flap) and revocation (`device.removed` is authoritative, alone).

A revoked device is not notified by message: its connection is closed with the
reason `DEVICE_REVOKED`, and any re-authentication fails.

## Connection lifecycle

```
 Core                                        Server
  │── WSS connect ───────────────────────────►│
  │── auth.challenge ────────────────────────►│
  │◄─ { nonce } ──────────────────────────────│
  │── auth.authenticate { proof } ───────────►│
  │◄─ { api_version, device } ────────────────│  → online, `device.online` to the others
  │── devices.list ──────────────────────────►│
  │◄─ [ devices… ] ───────────────────────────│
  │◄─ device.* (as they come in) ─────────────│
  │◄── ping / pong ──────────────────────────►│
  │  (closed or heartbeat lost)               │  → offline, `device.offline` to the others
```

## Errors

Standard JSON-RPC error codes, plus application codes in `error.data.code` (list
to be fleshed out at implementation time):

| Code | Meaning |
|---|---|
| `NOT_AUTHENTICATED` | method called before `auth.authenticate` |
| `INVALID_PROOF` | invalid nonce signature or expired/replayed nonce |
| `OIDC_INVALID` | ID token invalid, expired, or not fresh enough for a sensitive operation |
| `DEVICE_UNKNOWN` | `device_id` unknown to the account |
| `DEVICE_REVOKED` | device struck from the directory (also used as a closure reason) |
| `RATE_LIMITED` | too many requests |

## Versioning

- `api_version` is returned by `auth.enroll` / `auth.authenticate`.
- Tolerant JSON: unknown fields are ignored, extensions are additive (new
  optional fields, new methods, new notifications).
- An incompatible change = major increment of `api_version`; the server announces
  the supported range and the Core refuses cleanly if incompatible.
