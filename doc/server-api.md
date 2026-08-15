# 1Device — Server public API

> Specification of the API between the Core and the Server. Complements
> [architecture.md](architecture.md). Status: implemented — the Core and the server
> both speak it, and the shapes below are exercised end to end (in memory) by the
> test suite, which is what freezes them.

## Scope

The server has six jobs, and only six:

1. **Authenticate** accounts (OIDC) and devices (device key).
2. **Hold the directory** of an account's devices (enrollment, revocation,
   metadata).
3. **Broadcast presence** (online/offline, status).
4. **Provide the composition info**: what is needed to reach a device via iroh
   (`node_id`, `relay_url`).
5. **Describe the deployment**: the IdP and the OIDC client that every device of
   this server must use — read before any login, since it is what makes one
   possible.
6. **Bring two devices together to pair**: a rendezvous for a device joining the
   account, relaying between them a sealed bundle it cannot read.

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
- Outside the WebSocket: `GET /health` (monitoring) and
  `GET /.well-known/1device.json` (the deployment descriptor, below). TLS
  mandatory everywhere — terminated by the server or by an upstream reverse proxy
  (the server can then listen in cleartext on its internal network).

## Deployment descriptor

`GET /.well-known/1device.json` — **unauthenticated**, necessarily: a
client reads it before it is able to log in.

```json
{
  "api_version": 1,
  "oidc_issuer": "https://accounts.google.com",
  "oidc_client_id": "1234.apps.googleusercontent.com",
  "oidc_client_secret": "GOCSPX-…",
  "relays": ["https://relay-eu.example", "https://relay-us.example"]
}
```

The IdP and the OIDC client belong to the **deployment**, not to the user: they
are identical on every device of one server. So a client is told **one** thing,
the server's address, and reads the rest here — instead of having three fields
typed into it per machine and per phone. Field names are the ones the Core writes
into its `config.json`.

- `oidc_client_secret` is `null` for an IdP that wants none (RFC 7636 conformant);
  Google asks for one even under PKCE. **Serving it in the clear is deliberate**:
  for an *installed application* client it is not confidential. Google's own OAuth
  2.0 documentation, under "Installed applications", says to embed it in the
  source code of the app and that "in this context, the client secret is obviously
  not treated as a secret" — its Android and iOS client types are issued none at
  all, PKCE plus the loopback redirect ([RFC 8252]) being what protects the
  exchange. The exposure is that of shipping it inside every installer, which is
  what published clients do; the most a reader of this endpoint gains is the
  ability to pose as this deployment's OAuth client on its IdP's consent screen,
  which grants nothing on an account, the directory, or a device.
- **It carries no server URL.** The client arrived by that address and derives
  `wss://<host>/ws` itself (the path is fixed by this API). A server behind a
  reverse proxy has no reliable knowledge of its public origin, and a URL it
  dictated would be a redirect it controls.
- `relays` is the deployment's relay announcement: the iroh relays the
  operator runs for the fleet, a list because a public deployment wants
  regional relays the endpoint elects from (one entry is the common
  self-hosted case), and possibly empty because a server without a relay is a
  valid deployment. The one deliberate exception to "no addresses": these
  name infrastructure run FOR the fleet, never the server itself, and a
  device's own explicit relay (a URL or n0) always wins over the
  announcement; the off default, written out or not, is what the
  announcement fills. The
  Core re-reads this at every session establishment and keeps its own copy
  (`announced-relays.json`), so a boot with the server down still binds with
  the operator's relays.
- `api_version` is the number `auth.enroll` also answers: a client can tell,
  before enrolling, whether this server speaks its protocol.
- `Cache-Control: no-store`: read when a device is set up, and re-read at each
  session establishment for `relays`; the Core keeps its own copy of those. An
  HTTP-cached copy is how a client keeps being configured with an OIDC client
  (or a relay list) already replaced.
- **404** = a server older than this endpoint; the client falls back to asking the
  user for the fields.
- The `/.well-known/` name is not IANA-registered. What registration guards
  against is a collision on a shared domain, and a deployment's domain serves this
  control plane alone.

[RFC 8252]: https://www.rfc-editor.org/rfc/rfc8252

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
   `ONEDEVICE_JWKS_REFRESH_MIN_SECS`).

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
  "platform": "windows | macos | linux | android",
  "node_id": "<iroh public key>",
  "relay_url": "https://relay.example/…",
  "attestation": "<hex signature, or null>",
  "seq": 1753791245,
  "self_sig": "<hex signature, or null>",
  "addrs": ["192.0.2.7:41641"],
  "relay_hint": "https://relay.example/… or null",
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
- `seq` + `self_sig`: the device's **signed description** — its own signature
  over `{node_id, name, platform, seq, addrs, relay_hint}`, published alongside
  the attestation (`presence.update`) and carried just as blind. It is what lets
  a *peer* relay this record to devices this server never met (the serverless
  half of the account: `doc/architecture.md`, the continuum). The pair comes
  and survives together: one without the other is refused. A `devices.rename`
  that changes the name **drops both** (the signature covered the old name),
  and the device republishes over its own connection once it hears the rename.
- `addrs` + `relay_hint`: the **reach half** of that same description: where
  the device says it can be dialed (socket addresses as text, at most 16 of at
  most 64 bytes each) and the relay somebody chose for it (a configured URL,
  or the home relay an explicit n0 opt-in elected). Opaque
  like the rest; every `presence.update` replaces them together with
  `seq`/`self_sig` (one signature covers the whole description), and they are
  refused without `self_sig`. Durable like the attestation: unlike
  `relay_url`, the hints are the device's signed word, not this connection's
  presence. A rename is the one asymmetry: it drops `seq`/`self_sig` (they
  covered the old name) but KEEPS the hints: the addresses did not move with
  the name, peers only ever trust them through a signature anyway, and the
  record goes honestly unsigned until its device re-countersigns.
- `status`: an optional free field, reserved for extensibility (idle, busy…). v1
  defines no value for it.
- `platform` is a **closed set**: `auth.enroll` refuses anything else with a
  plain JSON-RPC `invalid params`. It is therefore a compatibility surface — a
  server older than a client's platform rejects that client outright (a 0.3.0
  server, which predates `android`, refuses a phone).

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

## Pairing

How a device joins the account without the user typing anything into it: a device
already in the account confirms it, and hands it the account's key material over
a channel the server cannot read.

**The server is a rendezvous and a relay of two opaque strings.** The QR code
carries a pre-shared secret that travels by a screen and a camera — a channel the
server has no access to — and `bundle` is ciphertext keyed by it. So what protects
an account here is not the state machine below; it is the optical channel. The
state machine is answerable for four things:

1. bringing two parties together under an unguessable `pairing_id`, one-shot,
   short-lived, and **never written to disk** (a restart cancels what is in
   flight);
2. making sure the **sponsor** — the side that gives the account away — is an
   authenticated device of that account with a **fresh** ID token;
3. **pinning** the joining device's `name`/`platform`/`node_id` between the moment
   a human confirms them and the moment they enter the directory, so what enrolls
   is what was shown;
4. refusing to bridge two different accounts.

### One state machine, two directions

Whoever **displays** the QR code creates the session (the *offerer*); whoever
**scans** it claims the session (the *claimer*). Orthogonally, `role` says which
of the two is the **joiner** (it receives the bundle) and which is the **sponsor**
(it gives it). The two directions the UI offers are the two ways to fill those
slots:

| Direction | Offerer | Claimer |
|---|---|---|
| a brand-new PC, confirmed from a phone | the PC, `role="joiner"` | the phone, sponsor |
| a brand-new phone, confirmed from a PC | the PC, `role="sponsor"` | the phone, joiner |

```
 Joiner (new)                    Server                    Sponsor (in the account)
   │── pairing.create {joiner, channel, device} ─►│
   │◄─ { pairing_id, expires_in } ────────────────│
   │·············· QR code, read off the screen ·············►│  (the secret never
   │                                              │◄─ pairing.claim {pairing_id, channel} ─┤   reaches the server)
   │◄─ pairing.claimed { channel } ───────────────│─ { role: sponsor, expires_in, device } ►│
   │                                              │                          (a human confirms)
   │                                              │◄─ pairing.approve {id_token, bundle} ──┤
   │◄─ pairing.completed { bundle } ──────────────│
   │── auth.enroll { pairing_id, proof } ────────►│
   │◄─ { device_id, api_version, device } ────────│
```

The claimer is told its `role` by the **server**, not by the QR code: the session
is what decides who is joining.

### Rules

- `channel` and `bundle` are **opaque** to the server: relayed verbatim, bounded
  in size, never parsed. What goes in them is the two ends' business
  (`doc/core-api.md`).
- `device` — the joining device's declaration — is required from whichever side
  the joiner is on, and is what the sponsor is shown. **Any `node_id` may be
  declared**, and nothing here proves it: `auth.enroll` demands a signature by
  that key over the connection's nonce, so a declaration that does not belong to
  its declarer enrolls nothing.
- `auth.enroll { pairing_id, proof }` carries **no ID token**: the joiner has no
  account credential (that is the point), and the account was proven by the
  sponsor's fresh token at `pairing.approve`. The record comes from the session,
  not from the request — restating `name`/`node_id` there changes nothing.
- **One confirmation, one device.** The grant is spent on first use.
- Only the connection that took part answers: the account's other devices cannot
  confirm in the scanner's place, and only the joiner's connection can spend the
  grant.
- One offer per connection: a new `pairing.create` retires that connection's
  previous session (a dialog closed and reopened must work, and the code left on
  the abandoned screen must stop working).
- **Both sides are told the deadline**: `expires_in` at `pairing.create` for the
  offerer, and in `pairing.claim`'s answer for the claimer. Neither has to invent
  one, which is what makes the silent expiry below defensible.
- A device that is **re-joining** — enrolled already, but holding no account key —
  offers with `role="joiner"` on its authenticated connection. Its account is then
  known from the start, and a sponsor from another one is turned away with
  `PAIRING_UNKNOWN` rather than being told the session exists.

### What the fresh token proves, and what it does not

`pairing.approve` is gated like `devices.revoke`. That gate is narrower than it
looks and is worth stating plainly: it proves the sponsor's session at the IdP is
still alive — so cutting the account's access there stops new devices from being
taken in — but it is **not** evidence that a human is at the keyboard. The Core
mints such a token from the refresh token in its keyring, with no browser
involved (`core/src/login.rs::fresh_id_token`). Human presence rests on the
confirmation screen alone; the residual risk that follows is in
`doc/architecture.md`.

## Methods (client → server)

| Method | Auth required | Description |
|---|---|---|
| `auth.challenge {}` | none | Returns `{ nonce }` (anti-replay, single-use, short-lived) |
| `auth.enroll { id_token, node_id, name, platform, proof }` | OIDC ID token + key proof | Creates the device under the account → `{ device_id, api_version, device }` |
| `auth.enroll { pairing_id, proof }` | approved pairing + key proof | Same, on the strength of a confirmation instead of a token (below) |
| `pairing.create { role, channel, device? }` | none, or session if `role="sponsor"` | Opens a pairing session and displays it → `{ pairing_id, expires_in }` |
| `pairing.claim { pairing_id, channel, device? }` | none, or session if the claimer turns out to be the sponsor | Joins a scanned session → `{ role, expires_in, device? }` |
| `pairing.approve { pairing_id, id_token, bundle }` | session + fresh OIDC | Confirms: relays the sealed `bundle` and turns the session into an enrollment grant |
| `pairing.cancel { pairing_id }` | party to the session | Gives up; the other side is told at once |
| `auth.authenticate { device_id, proof, relay_url? }` | key proof | Binds the connection to the device (→ online) → `{ api_version, device }` (its own record) |
| `devices.list {}` | session | Snapshot of the account's directory → `[ device, … ]` |
| `devices.rename { device_id, name }` | session | Renames any device of the account (handy from the GUI of another PC) |
| `devices.revoke { device_id, id_token }` | session + fresh OIDC | Strikes the device from the directory; its existing connection is closed (`DEVICE_REVOKED`) |
| `presence.update { status?, relay_url?, attestation?, seq?, self_sig?, addrs?, relay_hint? }` | session | Updates its own record; broadcast to the others via `device.updated`. `attestation` = opaque account blob (C7), carried without being interpreted; `seq`/`self_sig` = the signed description, equally opaque, both-or-neither (the continuum); `addrs`/`relay_hint` = the description's reach half, refused without `self_sig` (one signature covers it all) |

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

The `pairing.*` notifications are the exception to the rule above: they go to
**one connection**, the other party of a pairing session, and not to the account
— a joining device is in no account's device list yet, so its open connection is
the only way to reach it.

| Notification | Emitted to | When |
|---|---|---|
| `pairing.claimed { channel, device? }` | the offerer | the other side scanned. `device` is present when the offerer is the sponsor: it is what it must show the human |
| `pairing.completed { bundle }` | the joiner | the human confirmed. Carries the sealed bundle verbatim |
| `pairing.failed { reason }` | the other party | `declined` (`pairing.cancel`) or `abandoned` (a party's connection died). **No `expired`**: both sides were told `expires_in` and time out on their own clocks, which is exact — a notification sent from a lazy sweep would not be |

Two connection closures do **not** produce a `device.offline`: replacing a
connection with a new one (the others see a simple `device.online`, no
offline/online flap) and revocation (`device.removed` is authoritative, alone).

A revoked device is not notified by message: its connection is closed with the
reason `DEVICE_REVOKED`, and any re-authentication fails. The server closes a
connection with one of three reasons: `DEVICE_REVOKED`, `REPLACED` (one device =
at most one connection, so authenticating a new one closes the previous) and
`HEARTBEAT_LOST`.

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

Standard JSON-RPC error codes, plus the application codes in `error.data.code`
— the implemented set:

| Code | Meaning |
|---|---|
| `NOT_AUTHENTICATED` | method called before `auth.authenticate` |
| `INVALID_PROOF` | invalid nonce signature or expired/replayed nonce |
| `OIDC_INVALID` | ID token invalid, expired, or not fresh enough for a sensitive operation |
| `DEVICE_UNKNOWN` | `device_id` unknown to the account |
| `DEVICE_REVOKED` | device struck from the directory (also used as a closure reason) |
| `RATE_LIMITED` | too many requests |
| `PAIRING_UNKNOWN` | pairing id unknown, expired, spent — or none of the caller's business. The cases are deliberately not told apart: an id is 128 random bits, so a caller that holds none learns nothing, and a caller from another account is not told the session exists |
| `PAIRING_STATE` | the session exists, but not in a state where this call means anything (claiming a claimed session, confirming an unclaimed one, confirming from a device that did not scan) |
| `PAIRING_LIMIT` | too many pairing sessions held at once (a memory backstop; the real bound is one per connection) |

## Versioning

- `api_version` is returned by `auth.enroll` / `auth.authenticate`, and by the
  deployment descriptor — which is readable before any connection.
- Tolerant JSON: unknown fields are ignored, extensions are additive (new
  optional fields, new methods, new notifications).
- An incompatible change = major increment of `api_version`; the server announces
  the supported range and the Core refuses cleanly if incompatible.
