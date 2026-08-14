# 1Device — Core public API (local IPC)

> Specification of the API between the Core and the components (official and
> third-party). Complements [architecture.md](architecture.md) and
> [server-api.md](server-api.md).
> Status: implemented. The four official components (GUI, tray, clipboard
> backend, context-menu manager) all speak it, and their test suites exercise
> these shapes against a real Core — that is what freezes them. This API is
> **the project's extension point**: a component is any artifact capable of
> speaking this protocol.

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
| `session.manage` | `session.login`, `session.logout`, `session.reload`, `account.setup`, `account.join`, `pairing.*` and the `pairing` topic |
| `devices.read` | `devices.list`, the `devices` topic |
| `devices.manage` | `devices.rename`, `devices.revoke` |
| `files.send` | `files.send`, `files.cancel` (any transfer, outgoing or incoming — components are the user's trusted agents; the `transfer_id` is random, non-enumerable) |
| `transfers.read` | the `transfers` topic |
| `clipboard.write` | `clipboard.updated`, answering `clipboard.get_data` — both additionally require the `clipboard-backend` role (announcing is the exclusive backend's privilege) |
| `clipboard.read` | the `clipboard` topic, `clipboard.current`, `transactions.open`, `transactions.fill` |
| `components.approve` | `components.*` — never grantable via the prompt |
| `system.shutdown` | `system.shutdown` — stops the whole Core (the tray's Quit) |

Verification: per method and per topic. Example profiles — menu manager:
`session.read + devices.read + files.send` (`session.read` is what tells it the
Core is actually connected to the server: the directory cache is served offline
too, so without it the menu would offer targets that cannot be reached); tray:
`session.read + devices.read + transfers.read`; clipboard manager:
`devices.read + clipboard.read + clipboard.write`.

## Subscribing to events

```
events.subscribe { topics: ["session", "devices", "transfers", "clipboard", "pairing"] }
```

Topics filtered by scopes. Notifications are named (below, by namespace). After a
(re)connection, a component resynchronizes its state through the snapshot methods
(`devices.list`, `session.status`…) then subscribes.

**All or nothing**: a topic that is unknown or out of scope refuses the whole call
(`-32602` / `SCOPE_DENIED`) — never a partial subscription nobody was told about.
A consequence for anything that has grown a topic since: asking a Core that
predates it fails the subscription, and with it the connection. The client crate
takes topics that may be refused separately (`ClientConfig::optional_topics`) and
falls back to the required set alone, which is how an interface keeps working
against an older Core while simply not seeing that topic's events.

## `session.*`

| Method | Description |
|---|---|
| `session.status {}` | → `{ logged_in, server_connected, account?, configured }`. `configured`: whether a server + OIDC is set — distinguishes "never configured" (→ first-run setup) from "configured but the server is down" |
| `session.login {}` | starts the OIDC flow (PKCE + loopback) → `{ auth_url }`. **The caller** opens the browser — the Core does not touch the UI. Completion signaled by `session.changed` |
| `session.logout {}` | closes the server session |
| `session.reload {}` | re-reads `config.json` (which the GUI's setup screen has just written) and swaps the server config in place — no restart. → the fresh `session.status`. `INVALID_CONFIG` if the file is malformed / half-filled. The Core only READS the file; the GUI is its sole writer |
| `session.discover { url }` | reads the **deployment descriptor** at an address the user typed and returns what to write into `config.json`: → `{ server_url, oidc_issuer, oidc_client_id, oidc_client_secret }` (the last one `null` when the IdP wants none). Writes nothing — the caller does that, then `session.reload`. Meaningful with nothing configured, which is the case it exists for. See below |

Notifications (topic `session`):

| Notification | Meaning |
|---|---|
| `session.changed { logged_in, server_connected, account? }` | every transition — note it carries NO `configured` (a caller that needs it re-reads `session.status`, which a session change prompts anyway) |
| `account.left { reason }` | this device is out of the ACCOUNT — the membership ended, not just a session. The only `reason` today is `struck_off`: the account's own signature named this device, and the Core obeyed it (trust root, account key, session, directory and `device.key` all erased — the next startup is a first startup). Preceded by `device.removed` for every device served and by a `session.changed`, which is what a logout would also emit: this event is what tells the two apart, and the sentence the interface owes the human hangs on it |

### `session.discover`: one address instead of three fields

The IdP and the OIDC client are properties of the deployment, identical on every
device of one server, and the server publishes them
([server-api.md](server-api.md#deployment-descriptor)). So `url` is whatever the
user typed and the Core derives the rest of it:

- **No scheme means TLS.** `host` → `https://host/.well-known/1device.json`,
  and the returned `server_url` is `wss://host/ws`. `http://` and `ws://` are
  honored when written out — a cleartext deployment is accepted everywhere else in
  the Core — but they are never a default: this answer decides where the login
  goes and under which client id, so a MITM able to substitute the IdP would own
  the sign-in.
- **Only the authority is kept.** A pasted `wss://host/ws` (what a configured
  device has in its `config.json`) or a trailing slash works; the descriptor and
  `/ws` both live at the root, fixed by the server API. A deployment mounted under
  a path prefix is therefore not discoverable.
- Errors are distinct because the answer for the user differs: `-32602` the
  address is not one, `SERVER_UNREACHABLE` nothing answered (or an HTTP status
  that leaves nothing to read), `NO_DESCRIPTOR` something answered but publishes
  none — a server older than this endpoint, or another site altogether, and the
  interface falls back to asking for the fields — and `INVALID_DESCRIPTOR` a
  descriptor missing what a login needs. The message names the field at fault and
  never quotes the response: the caller picks the address, so an echoed body would
  make the Core a reader of whatever else answers on its network.

`api_version` from the descriptor is ignored: nothing in the Core acts on the
server's version today, and discovery is not where that policy gets invented.

## `account.*` (account key, C7)

The account's root of trust: an account key (derived from a **recovery code**)
attests that each `node_id` is indeed one of the user's devices, independently of
the server (see [server-api.md](server-api.md), "Account attestation", and
[architecture.md](architecture.md)). Each device derives the key from the code,
attests ITS `node_id`, persists `ak_pub` + its attestation in
`account-key.json` — and **keeps the private key at rest** in its keyring, which
is what lets it later vouch for a device joining the account. What that costs is
in [architecture.md](architecture.md) (principle 3): a compromised device now
means a compromised account key.

| Method | Description |
|---|---|
| `account.status {}` | → `{ attested: bool, fingerprint: string?, holds_key: bool }`. `fingerprint` = fingerprint (safety number) of the account key, to compare across devices (out-of-band verification); `holds_key` = this device also holds the account's PRIVATE key, so it can vouch for a joining one |
| `account.setup {}` | **first device**: generates the code, derives the key, stows it, attests and publishes → `{ recovery_code, fingerprint }`. Display `recovery_code` once and hand it to the user — it is their way back if every device is lost. `ACCOUNT_KEY_SET` if a key already exists |
| `account.join { recovery_code }` | **subsequent device**: re-derives the key from the entered code, stows it, attests and publishes → `{ fingerprint }`. `INVALID_CODE` if the code is malformed or wrong (checksum); `ACCOUNT_KEY_SET` if the code derives a key OTHER than the one this device is already attested under |

The same key ⇒ the same `fingerprint` on every device: a divergence betrays a
wrong code (the device would then remain outside the account, *fail-closed*) or a
substitution. Replacing an existing key (rotation) is a follow-up building block —
v1 refuses it (`ACCOUNT_KEY_SET`). Entering the code of the account this device is
already in, on the other hand, is accepted: the attestation is byte-for-byte the
same, and the one thing it changes is the keyring — which is what makes it the way
back in for a device that has the account without its key (`holds_key: false`).

There are two ways to install that key on a device: the recovery code typed in
(here) and a **pairing** confirmed on a device that already has it (below). Both
end in the same `account-key.json` + keyring entry, and both go through the same
rules (`account_key::install`) — a key other than the one already installed is
refused either way.

`account.setup`/`account.join` assume the server is reachable **when there is
one** (`SERVER_UNREACHABLE` otherwise): joining publishes an attestation the
account's other devices read from the server, and a device that could not publish
it would be in the account for itself alone. With **no server at all** there is
nothing to publish to and nothing to be unreachable for — the key, the root and
this device's own record are all local, and that is how an account is created with
no server at all. "No server at all" means nothing configured **and** no session:
a session carries its own server URL, so a Core whose `config.json` went missing
still answers to a server, and so does the rest of the account. They return `ACCOUNT_KEY_SAVE_FAILED` if the key
or the root cannot be persisted — nothing is installed in that case. `holds_key`
is answered by reading the keyring back, not by remembering the write: a keyring
write can be queued, and a stored key that does not derive `ak_pub` is ignored
(*fail-closed*) rather than used to sign — so `attested: true` with
`holds_key: false` is a legitimate state, not a corrupt one.

## `pairing.*` (joining without typing the code)

A device joins the account by being **confirmed on a device that is already in
it**: one displays a code, the other scans it (or the code is pasted, which is the
same thing), a human confirms on the side that gives, and the account key crosses
sealed. The recovery code stays what its name says — the way back when every
device is gone.

The server is a rendezvous and a relay of ciphertext
([server-api.md](server-api.md), "Pairing"); the channel is keyed by an X25519
exchange **plus** the code's own 128-bit secret, which travels by a screen and a
camera. So a server that records everything cannot read the bundle, and a
photograph of the screen is not enough either — but being faster than the
legitimate scanner *is*, which is what the confirmation screen exists to catch
(`doc/architecture.md`).

Where there is **no server at all** the same four methods pair over the local
network instead, and the difference is in the code: a `1D2` code names the device to
dial ("Pairing with no server", below). Everything in this section holds for both.

| Method | Description |
|---|---|
| `pairing.offer {}` | display a code → `{ pairing_id, code, role, expires_in }`. `code` is the string to render as a QR **and** to offer as copyable text; `expires_in` is in seconds |
| `pairing.accept { code }` | a code was scanned or pasted → `{ pairing_id, role, verification, device? }`. `verification` is the confirmation number (below); `device` (`{ name, platform, node_id }`) is present when this device turns out to be the **sponsor**: it is what must be put in front of the human before confirming. `-32602` if `code` is not one |
| `pairing.confirm { pairing_id }` | the human said yes (sponsor only) → `{ status: "done" }`, or `{ status: "reauth_required", auth_url }` when the server wants a fresher OIDC token than the keyring can mint — the caller opens the URL and reads the outcome from the events, exactly as for `devices.revoke` |
| `pairing.cancel { pairing_id }` | the human declined, or the dialog closed → `{}`. Idempotent: an id we no longer hold is not an error |

Notifications (topic `pairing`):

| Notification | Emitted when |
|---|---|
| `pairing.claimed { pairing_id, verification, device? }` | the other side scanned. `verification` is the confirmation number (below), known only from here — before the claim there is no channel to derive it from. `device` present when we are the sponsor — same record as `pairing.accept`'s |
| `pairing.completed { pairing_id }` | this pairing is done: the account is installed (joiner) or handed over (sponsor). The joiner's `session.changed` and `account.status` carry what changed |
| `pairing.failed { pairing_id, reason }` | `declined` (the other side gave up), `abandoned` (its connection died), `expired` (the deadline, counted here — the server says nothing), `channel` (the other side's channel material is unusable), `bundle` (what arrived does not open), `other_account` (it opened, and held a key other than this device's), `install` (the key could not be persisted), `enroll` (the server refused the enrollment), `server` (the rendezvous was lost), or a server code such as `PAIRING_UNKNOWN` |

**The confirmation number** (`verification`, six digits in two groups) is derived
from the channel key, which only the two ends of that one exchange can compute.
Both sides must show it — the joining side while it waits, the sponsoring side on
its confirmation screen — and the human is asked to check they match. That is what
turns the screen into a check: whoever photographed the code and claimed the
session ahead of the legitimate device gets a channel of its own, and so a
different number, while the name and platform it declares are its own to choose.

**The role is not the caller's to choose.** This device sponsors when it can
actually vouch — it holds the account key AND is in the account — and joins
otherwise; `pairing.offer` answers which. That covers the case a parameter would
get wrong: a device that holds the key but was revoked needs to *join* again, not
to sponsor. A scanner is told its role by the **server**, since the session is what
decides who is joining; being told to sponsor with no key answers
`NO_ACCOUNT_KEY` and gives the session back rather than leaving the other side
waiting.

**One pairing at a time**, per Core and not per connection: a new `offer`/`accept`
replaces the previous one. A pairing is not tied to the connection that opened it
(a GUI that restarts mid-dialog does not cancel it), so what bounds it is the
deadline — both sides count it themselves.

**A Core that HAS a server pairs through it**, and cannot pair while it cannot
reach it (`SERVER_UNREACHABLE`, the same answer `session.login` gives). A device
with no session opens a connection of its own for the pairing and enrolls on it;
one already logged in pairs over its session connection, which is what tells the
server its account (so a sponsor from another one is turned away). A device that
enrolls this way has no OIDC refresh token: its first sensitive operation
(`devices.revoke`, sponsoring in its turn) opens a browser once.

### Pairing with no server

A device with **no server in its life at all** — nothing configured and no session,
the same condition `account.setup` and `devices.revoke` turn on — pairs over the
local network. `pairing.offer` then mints a `1D2` code instead of answering
`SERVER_UNREACHABLE`:

| Code | Shape | Who is dialled |
|---|---|---|
| `1D1` | `1D1:<psk>:<epk>:<pairing_id>` | nobody — the server relays between the two |
| `1D2` | `1D2:<psk>:<epk>:<node_id>` | the device that displays it, on the data plane |

The tag is what tells them apart. A `1D1` code with no server is a rendezvous this
device has no way to go to → `SERVER_UNREACHABLE`. A `1D2` code, since the
continuum, is accepted by ANY device that can play its part in it — including one
that answers to a server, which may scan it and sponsor over the local network.
The joiner then joins the **account**, not the deployment: it is not enrolled on
the server, which simply never lists it (the server-relayed `1D1` pairing is what
enrolls a device on the deployment too). Both tags are exported from the Core
crate (`PAIRING_CODE_TAG`, `PAIRING_LAN_CODE_TAG`) for a camera that has to know
which QR code in its view is a pairing code — the mobile scanner looks for
either, and which is which stays the Core's business.

The methods and the notifications are the ones above, unchanged. What differs:

- **`expires_in` is 180 seconds**, counted by both ends, and the deadline is also
  how long this device will read a frame from a device outside its directory at
  all — outside a window it reads none.
- **`pairing.confirm` never answers `reauth_required`**: there is no OIDC to be
  fresher with. `{ status: "done" }` means the yes was delivered to the exchange
  already under way; `pairing.completed` / `pairing.failed` is still the outcome.
- **Who sponsors** is whoever holds the account's private key (`account.status`'s
  `holds_key`). Neither of them → `NO_ACCOUNT_KEY` for the caller and
  `pairing.failed { reason: "no_account" }` on the other side. Two *different*
  accounts → `ACCOUNT_KEY_SET` / `other_account`, refused before the key crosses.
  Both holding the **same** key is not an error: it is what an account looks like
  when the recovery code was typed into each machine in turn, the device that
  displayed the code sponsors, and the exchange leaves the two of them knowing each
  other — which is the point of it.
- **`pairing.failed` gains one reason**, `no_account`; the others are the ones
  above (`channel`, `bundle`, `other_account`, `install`, `declined`, `abandoned`,
  `expired`, and `state` for an answer that is not the one the protocol expects at
  that point). A dialer that never read the code, and one that arrives at a window
  already taken, are refused on the wire (`proof`, `busy`) and end nobody's pairing:
  they are what a stranger gets, not what the human waiting is told.
- **`pairing.accept` can answer `DEVICE_OFFLINE`**: the device whose code was read
  is not on this network, or not reachable on it.
- **`pairing.accept` answers `DEVICE_REVOKED`** for a code shown by a `node_id`
  the account struck off — before anything is dialled. A tombstone is permanent,
  so there is no pairing to attempt: the refusal is the account's own decision
  said as such, not a failure discovered after two humans compared a number. The
  mirror needs no code of its own: a struck-off *dialer* is answered its
  tombstone by the displayer's data plane and never reaches the window (see
  `devices.*`).
- **Both devices come out holding each other's record**, so `devices.list` on each
  shows the other and the directory exchange runs from then on. With a server that
  is the server's business; here it is what the pairing is for. Each side declares
  its own SIGNED description and the other checks it against the `node_id` the
  transport authenticated — a device does not get to describe another one, and a
  description nobody signed could never travel any further anyway. A device that
  answers otherwise is refused (`PAIRING_STATE` for the caller).

## `devices.*`

The device record is the one from [server-api.md](server-api.md), enriched by the
Core with three fields:

- `is_self` — this very device.
- `lan` — this machine currently hears the device on the local network (mDNS).
  First-hand presence: alive with or without the server.
- `reachable` — the Core's ONE presence verdict: what a send could reach right
  now. True on the LAN, or — only while the server link that feeds the `online`
  flags is up — for an online device with a published relay. **Consumers gate
  on this**, never on `online` alone: it is derived once, Core-side, so nobody
  re-assembles presence from parts that go stale at different times.

| Method | Description |
|---|---|
| `devices.list {}` | → `[ device, … ]` (snapshot, includes the local device). `SERVER_UNREACHABLE` only for a Core that knows of no device at all — see below |
| `devices.rename { device_id, name }` | proxy to the server — and the renamed device **countersigns** the new name once it hears it (the continuum; see below). With **no server at all**: renames THIS device (it re-signs its own record); any other `device_id` → `SERVER_UNREACHABLE` |
| `devices.revoke { device_id }` | → `{ status: "done" }` or `{ status: "reauth_required", auth_url }` (fresh ID token required by the server; the caller opens the URL, completion arrives via `device.removed`). Where this device holds the account key, the account's own **tombstone** is minted besides the server's strike (the continuum — see below). A device the server never named (its label IS its `node_id`) is struck by the account key alone, the server not asked. With **no server at all**: the tombstone, and **for good** — see below |

Notifications: `device.added / removed / online / offline / updated { … }` — same
payloads as on the server side, with two Core-side additions. `device.offline`
carries the re-enriched `device` record alongside its `device_id` (a device that
left the *server* may still be on the LAN — patching `online` alone would get
`reachable` wrong either way). And `device.updated` also fires, unprompted, when
a device's LAN visibility flips — server connected or not: this is how the menu
and the GUI follow the room without polling.

**What the directory is made of.** The server's snapshot while there is a session
(kept across an outage, freshness read from `session.changed`) — plus, for a
device that has joined the account, **its own record**, which owes nothing to a
server: this Core knows its `node_id`, its name and its attestation first-hand,
and it keeps them across a logout (a session ends, a membership does not). So
`SERVER_UNREACHABLE` is left for a Core that knows of no device at all — one that
has never logged in *and* never joined an account. A record a Core minted for
itself carries `device_id` = its own `node_id` (no server has named it),
`online: true` (its own liveness needs nobody) and `null` in the fields only a
server fills: `relay_url`, `last_seen`, `status`.

**A record a device minted for itself signs itself.** It carries two more fields:
`seq` (u64) and `self_sig` — the device's own signature, under the key its
`node_id` IS, over `{node_id, name, platform, seq}`. So a description can travel
from device to device without the one relaying it being trusted with it, and `seq`
— which only its owner can raise — is what makes one description supersede
another. Deliberately NOT signed: `relay_url`, `online`, `last_seen`, `status`,
`device_id`. Those are said in the present tense, and a signature over them would
be a stale fact wearing a proof.

**Where a server names the device, the device countersigns** (the continuum). The
server keeps the name — `devices.rename` may come from another device — and the
named device signs the server's word as its own description, republishing the
signature to the server the way it publishes its attestation (opaque, carried
blind, rebroadcast in the records). So a record of the server's half proves
itself too, and can be relayed to siblings the server never met. A record whose
device has not countersigned yet (an old client, or one renamed and not yet
heard back) travels nowhere — the honest gap, and it closes itself the moment
that device reconnects.

The signed description is stable — a restart hands back the very record, `seq`
included. `devices.rename` is the one thing that re-signs it, and with no server
it only ever renames THIS device: another device's record is signed by that
device, so renaming it means asking it, and the only thing that carries such an
ask is the server. (One exception, at startup: a record that CLAIMS this device's
signature without carrying it — `device.key` changed, or the store was edited — is
minted again rather than republished, since a peer would refuse it and nothing
local would notice.)

**Revocation with no server: a tombstone.** `devices.revoke` normally strikes the
device from the server's directory. With no server at all, the **account key**
signs the withdrawal instead — a signature over the target's `node_id`, kept in
`revoked.json`. It bars that `node_id` at every door into the directory (the store
at startup, a server snapshot, a server `device.*` event), so it **outlives what a
server says**: a deployment that was never told keeps listing the device, and the
Core keeps refusing it. Three consequences worth stating plainly:

- It is **permanent**. Nothing un-revokes a `node_id` — an "undo" would need a
  total order the account cannot establish offline. A device struck off by mistake
  comes back only as a new one: a fresh `device.key`, attested again.
- It needs the account's **private** key: `NO_ACCOUNT_KEY` for a device that holds
  the account without its key (`holds_key: false`). And it refuses this very
  device (`CANNOT_REVOKE_SELF`) — barring your own installation from its own
  account has no way back; leaving the account is a logout plus erasing the trust
  root.
- There is **no fresh-login gate**, unlike the server path: with no server there
  is no OIDC to be fresh with. What stands between a component and this call is
  the `devices.manage` scope, nothing more.
- The struck device itself **obeys, once it hears**. The tombstone travels with
  the rosters (`dir_sync`) like any other — and to the device it names, whose
  streams every informed sibling now refuses, it travels as the answer to its own
  refused dial. On hearing it, that device leaves the account whole
  (`account.left { reason: "struck_off" }` — see `session.*`), erases its
  `device.key`, and restarts as a first startup. Best-effort by construction: a
  device that never dials again — stolen, dead — simply never learns, and loses
  nothing by it, since the account already refuses it everywhere.

**And with a server, the tombstone is minted too** (the continuum): a
`devices.revoke` that the server accepted also signs the account's own tombstone,
whenever this device holds the account key — a server-side strike stops at the
server's reach, and the account's serverless half would otherwise keep trusting
the struck device until its store expired (or forever, where nothing expires). A
device that holds the account *without* its key cannot sign the account's word:
the server's strike then stands alone, exactly as before the continuum, and the
7-day staleness bound is what limits the damage. Never against this very device —
a self-revocation through the server stays what it always was (`DEVICE_REVOKED`
at the next exchange), for the same reason `CANNOT_REVOKE_SELF` exists.

`revoked.json` survives what `directory.json` does not: a logout, and a revocation
of this device. The struck-off device keeps a valid attestation for good, so the
tombstone is the only thing that keeps it out.

**The devices tell each other whom they know.** Two devices of the account that
already hold each other's record exchange rosters over the data plane — on a
change of LAN membership, right after a local rename or revocation, and on a slow
tick. So `device.added`, `device.updated` and `device.removed` may now arrive
**with no server involved at all**: a device learns of a sibling it has never met
from a third one that has, a rename catches up, and a tombstone reaches the whole
account. Nothing else about those notifications changes — a subscriber cannot tell
which side of the account taught the Core, and should not care.

What a roster is allowed to teach is bounded, and the bound is what makes relaying
safe:

- A record must be **signed by the device it describes** and **attested under the
  account key**. A peer is a courier, never a witness: it cannot invent a sibling,
  rename one, or bring a struck-off one back. A tombstone must likewise be signed
  by the account key.
- The **highest `seq` wins**, and only among signed descriptions. A record this
  Core holds *without* a `seq` was minted by a server, and there the server keeps
  the name.
- A device this Core has never heard of arrives **known but not reachable**: keyed
  and labelled by its `node_id` (the one label a relayer cannot rewrite), with
  `relay_url`, `last_seen` and `status` null and `online: false`. Those are
  present-tense facts about a device the relayer is not — the transport hearing it
  on the LAN, or a server that owns them, is what fills them in.
- A record describing **this** device is never taken in, whatever its `seq`: what a
  peer holds about us is at best what we told it.
- An exchange that teaches nothing costs nothing — no notification, no disk write.
  And one that DOES teach something still does not move `directory.json`'s
  freshness stamp: a sibling's roster carries the account's tombstones, but not a
  server-side `devices.revoke`, which mints none. Counting the exchange as a
  refresh would let two devices left talking to each other in a room hold the
  7-day staleness bound open between themselves — and keep vouching for a device
  the server revoked.

With a server in the picture this exchange carries the server's half too (the
continuum): its devices countersign their descriptions, so their records prove
themselves like any other and are relayed the same way — the account is the union
the account key defines, of which a deployment lists the subset that enrolled
with it. The server's snapshot is **merged**, never swapped in: what the account
can prove about devices the server never met survives every reconnection, and a
logout keeps that half on disk (re-keyed by `node_id`, its present-tense fields
dropped — nothing vouches for a route anymore). A record only the server ever
asserted still travels nowhere and leaves with the session, exactly as before.

One case ends the exchange instead of feeding it: a tombstone naming *this*
device. It is signed by the account key, so it is not hearsay — and once that
signature verifies, it is **obeyed**: the device leaves the account. Everything
that made it a member goes — the trust root, the account key and the refresh
token in the keyring, the session, `directory.json`, `revoked.json`, and
`device.key` itself, so the next startup is a first startup under a fresh
identity (a revocation is permanent: the struck `node_id` never returns, and only
a new key can be attested again). Nothing of the human's is touched. The
components hear, in order, `device.removed` for every device served,
`session.changed`, then `account.left { reason: "struck_off" }` (topic
`session`) — the one event that says it was the account's decision, not a logout.
A pairing in flight fails with `no_account`; clipboard grants die as at a logout.
A signature that does not verify wipes nothing, like any tombstone the account
never signed.

How it *reaches* the device is its own mechanism, because gossip cannot carry it:
absorbing a tombstone is what evicts the struck device from a sibling's
directory, so the siblings refuse the very streams that could have delivered it —
enforcing the revocation is exactly what blocks its delivery. Instead, the data
plane answers the struck-off device's dial (any dial, pairing window open or not,
no byte of it read) with a one-entry `dir_roster` holding its own tombstone; the
struck device's next sync round reads it through the same absorb that obeys it. A
mere stranger still gets silence — the answer exists only for a `node_id` the
account's signature names, and it tells that device nothing it does not already
own.

## `files.*`

| Method | Description |
|---|---|
| `files.send { device_id, paths[] }` | → `{ transfer_id }`. Fire-and-forget: the Core reads the disk and streams via iroh, tracking goes through the events. A path may be a regular file **or a folder**: a folder is walked into a tree manifest (the same walk the clipboard uses, empty directories included) and arrives as a folder. A missing path or a name that cannot be represented on the wire → `-32602`; a manifest over the cap → `MANIFEST_TOO_LARGE` |
| `files.cancel { transfer_id }` | cancels an outgoing OR incoming transfer |

`device_id` is resolved by the directory, **C7 attestation verified before any
opening**: a target that is absent or attested under a foreign key →
`DEVICE_UNKNOWN` (fail-closed, indistinguishable so as to disclose nothing); known
but with no route to it — no published relay, and not currently visible on the
local network (mDNS) → `DEVICE_OFFLINE`. Once the `transfer_id` has been
returned, failures (connection, disk, a target that has shrunk) go through
`transfer.failed`.

Reception: **auto-accepted in v1** (these are the user's devices, authenticated by
the account key). The bytes land in the configured receive folder (see
[deployment.md](deployment.md), `ONEDEVICE_RECEIVE_DIR`), each file via a
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

Because the source may vanish the instant the push completes, it has to know
*when* that is. The announce therefore answers `pushed_to` — how many of the
account's other devices the fan-out targets — and, when that is non-zero, the
Core sends exactly one `clipboard.pushed { tx_id, delivered, failed }` on the
announcing connection once every push has settled. That is the completion
signal an ephemeral source waits on before exiting, and it is deliberately
*reporting*, not a guarantee: `pushed_to: 0` means nothing was shared at all,
`delivered: 0` means no device could be reached, and a device that was offline
at copy time never learns the clip. A source that does not care may ignore both
— the local transaction is unaffected.

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
| component → Core | `clipboard.updated { formats: [{format, size?}], paths?, sensitive?, materialize?, blobs? }` → `{ tx_id, pushed_to? }` | announces the local copy: opens the transaction that supersedes the previous one. `paths` mandatory if `files` (the manifest is frozen from them). `formats` may be empty — the clipboard was cleared; it supersedes like any announce (a contentless transaction), and destinations withdraw their promise. Inline `size` is an advisory hint (the content is re-serialized at paste time; the stream up to `EOF` is authoritative, a mismatch is not an error) and is omitted when `sensitive`. `sensitive`: set if the OS confidentiality markers are detected. `materialize: true` makes it a **materialized** transaction (push-at-copy): the caller supplies the inline bytes now as `blobs: { <format>: <base64> }` (one entry per inline format offered, capped), the Core pushes them to the account's online devices, and it also serves the source's own pastes from them — so the caller may exit right after the copy. It excludes `sensitive` and `files` (rejected). `pushed_to` is returned **only for a materialized announce**: the number of the account's other devices the push was launched to (`0` = no other device known, so nothing was shared), and it promises exactly one `clipboard.pushed` when non-zero. The backend keeps the returned `tx_id` mapped to that clipboard generation |
| Core → component | notification `clipboard.pushed { tx_id, delivered, failed }` | the outcome of a materialized announce's fan-out, sent to the **announcing connection only** (no topic, no subscription): `delivered + failed == pushed_to`, `delivered` counting the devices that acknowledged the bytes. Exactly one per announce whose `pushed_to` was non-zero, once every push has settled or timed out — a source that must not outlive its share by more than it has to (a phone holding a foreground service up) waits for this and then stops. Nothing to do on receipt: the copy is already the account's current clip, `failed` devices simply never learned it, exactly as a missed announce |
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
| `NO_DESCRIPTOR` | `session.discover`: the address answered but publishes no deployment descriptor (server older than that endpoint, or not one of ours) |
| `INVALID_DESCRIPTOR` | `session.discover`: a descriptor without what a login needs (the message names the field, never the response) |
| `ACCOUNT_KEY_SET` | `account.setup` / `account.join` for an account key OTHER than the one already installed (rotation is a follow-up); `pairing.accept` on the local network when the two devices are in two different accounts |
| `INVALID_CODE` | `account.join`: malformed or wrong recovery code (checksum) |
| `ACCOUNT_KEY_SAVE_FAILED` | the account key or its root cannot be persisted (keyring refused, folder not writable) — nothing is installed |
| `NO_ACCOUNT_KEY` | this device cannot sign for the account: it holds no account key (`pairing.accept` told to sponsor, `pairing.confirm`, `devices.revoke` with no server) — and, pairing on the local network, when NEITHER of the two devices holds one |
| `CANNOT_REVOKE_SELF` | `devices.revoke` aimed at this very device, with no server: a tombstone cannot be withdrawn, so this would bar the installation from its own account for good |
| `PAIRING_UNKNOWN` / `PAIRING_STATE` / `PAIRING_LIMIT` | relayed from the server as-is: unknown/expired/spent session, wrong moment (confirming before anyone scanned, or from the joining side), too many sessions at once. `PAIRING_STATE` is also the local answer for a pairing that is out of step: a code whose window is no longer the one on screen, a device that answers a dial with something other than the protocol's next frame, and confirming a pairing whose stream is gone |
| `DEVICE_UNKNOWN` / `DEVICE_OFFLINE` | target unknown / unreachable (`pairing.accept` of a `1D2` code: the device that displayed it is not on this network) |
| `DEVICE_REVOKED` | `pairing.accept` of a `1D2` code shown by a `node_id` the account struck off: a tombstone is permanent, and that device can only come back under a fresh identity |
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
