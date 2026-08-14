# 1Device — Overall architecture (Phase 1)

> Summary document from the initial design phase, kept in step with what has been
> built since: where it says **built**, the code is in the tree and under test.
> Describes the major building blocks and the decisions that have been settled.

## Goal

Link a single user's machines — macOS, Windows, Linux, and an Android phone that
shares to them — to transfer files and content in several ways:

- Shared clipboard (copy on one machine, paste on another) — **built**
- Contextual menu ("right click → send to PC X") — **built**
- Drag and drop, via the GUI, onto a device's card — **built** (inbound only:
  dragging *out* of the app is not implemented)
- Shared folder, in the sense of a folder kept in sync between machines — not
  built. *Sending* a folder is, and is part of transfers.
- Remote filesystem exposure — not built

Written in **Rust**.

## Overview

```
                      ┌────────────┐
                      │   Server   │  OIDC · device directory · presence · signaling
                      └─────┬──────┘
                            │  (authenticated session, control plane)
          ┌─────────────────┼─────────────────┐
          │                 │                 │
     ┌────┴────┐       ┌────┴────┐       ┌────┴────┐
     │  PC A   │◄─────►│  PC B   │       │  PC C   │
     │ (Core)  │ iroh  │ (Core)  │       │ (Core)  │
     └─────────┘ P2P   └─────────┘       └─────────┘
              (direct, else relay — E2E-encrypted data)
```

On each PC:

```
 Server ◄────┐   ┌────────┐    local IPC (JSON-RPC 2.0 / UDS · named pipes)
 iroh   ◄────┼───┤  Core  │◄────────┬──────────────┬───────────────┬───────────┐
             │   └───┬────┘         │              │               │           │
             │       │ spawns   ┌───┴────┐   ┌─────┴─────┐   ┌─────┴─────┐ ┌───┴───┐
             │       └─────────►│  Tray  │   │ Clipboard │   │ Ctx menu  │ │  GUI  │
             │                  │notifier│   │  manager  │   │  manager  │ │(run by│
             │                  └────────┘   └───────────┘   └─────┬─────┘ │ the   │
             │                                                     │       │ user) │
             │                                              ┌──────┴─────┐ └───────┘
             │                                              │  OS shims  │
             │                                              │ DLL, appex,│
             │                                              │ .desktop…  │
             └──────────────────────────────────────────────┴────────────┘
```

## Guiding principles

1. **The extension point is the Core's IPC API — not the "executable" form.**
   A component is any artifact (executable, DLL, app extension, file-manager
   plugin) that speaks the Core's protocol. This is necessary because some OS
   integrations impose their own form (in-process COM DLL for the Windows 11
   menu, signed appex for Finder, in-process plugin for Nautilus).

2. **Files never travel over the local IPC's control plane.** Components
   exchange *paths* and control; the Core reads/writes the disk itself (same
   machine) and streams via iroh. Payloads that must cross the IPC (the
   clipboard's inline content, consumer-driven reads) go through a dedicated
   **data channel** on the same socket. The control plane stays control only:
   simple, reliable, secure — no need for high performance.

3. **End-to-end encryption, blind server.** Each device has its own key pair
   (the iroh identity is an Ed25519 key pair). The server and the relays can
   never read the data that flows through.

   The server is also removed from trust over *who belongs to the account*: an
   **account key** (Ed25519), derived by the user from a **recovery code** and
   never known to the server, signs an **attestation** binding each `node_id` to
   the account. A peer only authorizes another device if its attestation
   verifies under this key (which the peer derived itself) — mere presence in the
   directory is not enough (*fail-closed*). Thus a compromised server cannot
   inject a foreign device.

   - Each device derives the key from the code, attests ITS `node_id`, persists
     the account's public key + its attestation — and **keeps the private key at
     rest**, in its keyring (OS keyring, or a 0600 file: the same perimeter as
     the device key and the OIDC refresh token). The recovery code is therefore
     not the only copy of that key; it is the way back when every device is gone.
   - **Why it is kept, and what that costs.** A device that holds the key can
     vouch for a new one — which is what makes pairing possible (scan a QR code
     on an enrolled device instead of retyping the code). The price is stated
     plainly: whoever reads a device's storage reads the account key, where
     before they would have needed the user's recovery code. So **a compromised
     device means a compromised account key**, and rotating it stops being a
     nicety — it becomes the mandatory response. The key is never displayed and
     never leaves the device except sealed inside a pairing, over a channel keyed
     by a secret that traveled from a screen to a camera.
   - **Out-of-band verification**: a fingerprint (safety number) of the account
     key, identical on every device, is compared visually — it diverges as soon
     as one device has derived a different key or a substitution has taken place.
     A device that reads a key from its keyring checks it against the public key
     it persisted: the keyring does not get to choose what the device signs with.
   - The attestation binds the `node_id` alone (stable crypto identity), not the
     `device_id` (ephemeral server label): it survives a re-enrollment. The
     signed payload is versioned to allow a later key rotation.
   - **Revocation**: removing a specific device = striking it from the server
     directory (`devices.revoke`). That is enough to stop it vouching for
     anything — a device must be authenticated in the account to reach the
     pairing rendezvous — but it is not enough for a device whose storage was
     *read*: that case is an account-key rotation, a follow-up building block.
     Until it exists, the manual equivalent is to erase the trust root and the
     stored key on **every** device and start over from `account.setup` — a new
     recovery code, and every device attested again.
   - **Revocation with no server**: where there is no directory to strike a
     device from, the account key signs the withdrawal itself — a **tombstone**
     over the `node_id`, under a domain of its own so it can never be confused
     with an attestation. Every device verifies it against the key it derived
     itself, so it needs no authority to carry it, and it **outlives what a
     server says**: a deployment that was never told keeps listing the device,
     and the peers keep refusing it. It is permanent by design (un-revoking
     would need a total order the account cannot establish offline): a device
     struck off comes back only as a new one, with a fresh `node_id`.
   - **The struck device obeys, once it hears.** A tombstone naming *this*
     device, verified under the account key, is not refused — it is the account's
     own word, and the device leaves: trust root, account key, session, directory
     and `device.key`, all erased, so its next startup is a first startup under
     the fresh identity that is the only way back. Delivering it takes a
     mechanism of its own, because enforcement blocks gossip: absorbing a
     tombstone evicts the struck device from a sibling's directory, so every
     informed sibling refuses the very streams that could have carried it. The
     data plane therefore answers the struck-off device's dial with a one-entry
     roster — its own tombstone, nothing read in return — and the same absorb
     that reads rosters is the one that obeys. Best-effort by construction: a
     device that never dials again never learns, and loses nothing by it, since
     the account already refuses it everywhere.
   - **A device signs what it says about itself**: its directory record carries
     its own signature over `{node_id, name, platform, seq}`, so a description
     can pass from device to device without the one relaying it being trusted
     with it, and only its owner can raise the `seq` that makes a new
     description supersede the one already known. What a device says in the
     present tense — its relay, its liveness — is left unsigned, deliberately:
     a signature over it would be a stale fact wearing a proof.
   - **The continuum: one account, half on a server, half not.** The account is
     the union the ACCOUNT KEY defines; a deployment lists the subset that
     enrolled with it, and is the authority on that subset's names and routes —
     nothing more. Where a server names a device, the device **countersigns**:
     the name stays the server's word, the signature is nobody's but its own,
     published to the server as blind as the attestation and rebroadcast in its
     records. So the server's half proves itself like the serverless half, the
     rosters carry both, and a serverless-only sibling is a full member of an
     account the server never learns it belongs to. The server's snapshot is
     merged, never swapped in — what the account can prove outlives every
     reconnection and every logout — and a `devices.revoke` through the server
     also mints the account's own tombstone wherever the key is at hand, because
     a server-side strike stops at the server's reach. The two halves lag each
     other by one sync round, not a quarter of an hour: server news nudges the
     rosters.
   - **The devices carry the directory between themselves.** Two devices that
     already hold each other's record exchange rosters over the data plane (one
     round trip, a frame type of its own on the existing ALPN): a device thereby
     learns of a sibling it has never met, from a third one that has, and a
     tombstone reaches the whole account. A relayer is a courier, never a
     witness — every record must be signed by the device it describes AND
     attested under the account key, so a compromised member cannot invent a
     sibling, rename one, or bring a struck-off one back. What it can do is stay
     silent, and silence costs nothing: an account is not made of what a peer
     chooses to mention. What a courier could not witness (a route, a liveness)
     is dropped on arrival, so a device learns of a sibling **known but not
     reachable** until it hears it for itself.
   - **The first introduction, with no server to make it.** That exchange
     presupposes two devices that already hold each other's record, which the
     recovery code typed into each machine does not give: each then knows only
     itself. So a code displayed on one device and read on the other opens a
     pairing over the local network — the code names the device to dial — and it
     ends with each holding the other's signed record, the joiner's attested by
     the sponsor on the spot. Both `node_id`s are authenticated by the transport,
     so neither device can be posed as; what a photographed code buys is a race,
     and the confirmation number is what catches it ("Pairing a device", "On the
     local network").

4. **Push between long-lived processes, pull for ephemeral artifacts.**
   Server → Core → managers: subscriptions/events, in-memory caches always warm.
   Artifacts loaded/unloaded by the OS (contextual-menu DLL, plugins) do
   request/response against their manager's cache, with a short timeout. The
   latency budget of opening a menu crosses only a single local hop.

5. **Fail-closed.** If the Core or a manager is unreachable, the OS integrations
   hide (no dead menu entry, no misleading state).

6. **Headless-first.** The Core starts at login and runs without a GUI. Once the
   user is logged in, the clipboard and the contextual menu work without any
   window ever being opened.

7. **Core rule: no integration API or UI, but abstracted OS plumbing allowed.**
   The Core never touches the clipboard, menus, windows, the tray, or
   notifications — that is the components' job. It does, however, use ordinary OS
   facilities through cross-platform abstractions: process spawning, file
   permissions, secrets keyring (a `keyring`-type crate), autostart.

## The Server

Role strictly limited to the control plane:

- **Authentication**: accounts via OIDC.
- **Device directory**: each device registered under the account, with its
  public key. Enrollment and revocation. Account membership is **attested by the
  account key** (see principle 3): the server carries the attestation without
  being able to forge or verify it — the peers are the ones who verify it.
- **Presence**: the state of each device (connected / disconnected / states to
  be specified), broadcast to the account's other devices via events.
- **Signaling**: helps establish P2P connections between devices.

The server does not relay the data itself: that is delegated to the transport
layer (iroh relays), and in every case it cannot decrypt it (E2E).

The server's public API is specified in [server-api.md](server-api.md).

## Data transport: iroh

The networking building block is not developed in-house. The **iroh** crate
provides:

- Encrypted QUIC connections between devices, identified by key pair
  (NodeId = public key).
- NAT traversal / hole punching for direct connections.
- Automatic fallback to a relay when a direct connection is impossible — a relay
  that only sees encrypted traffic.
- **Local discovery** (mDNS, on by default, `lan_discovery` in `config.json`):
  devices announce themselves on the local network and resolve each other by
  NodeId alone, so two machines that share a network connect directly — without
  the relay, including when a device never published one. Discovery only ever
  provides an *address*: an impostor announcing someone else's NodeId fails the
  QUIC handshake, and the account attestation still gates every stream. Turning it
  off also turns off pairing over the local network ("On the local network"), which
  is the one route a device with no server has to reach another.

## The Client

### Core

Central daemon, launched automatically at session login.

- Holds the **server session**: OIDC login (authorization code + PKCE, the
  system browser redirects to a loopback listened to by the Core), refresh token
  stored in the OS keyring. If the session is cached, no user interaction at
  startup.
- Holds the **device identity** (iroh key pair).
- Establishes **transfers** via iroh (direct, else relay).
- Exposes the **local IPC server** to components.
- Is the **supervisor** of the official background components: it spawns (and
  restarts on crash) the clipboard manager, the contextual menu manager, and the
  tray/notifier.

### Official components

| Component | Launch | Role |
|---|---|---|
| **Tray / notifier** | spawned by the Core | Minimal always-present surface: status icon, native notifications (session expired, pending approval…), "open the GUI" / "open the browser" actions. It is the Core's doorbell. |
| **Clipboard manager** | spawned by the Core | Per-OS backends to read/write the clipboard and be notified of changes (X11 with ICCCM INCR on Linux, the Win32 clipboard and OLE `IDataObject` on Windows, `NSPasteboard` with an `NSFilePresenter` on macOS). Handles the "blocking paste" for the duration of the download, and honors the OS's confidentiality markers in both directions. Protocol specified in [core-api.md](core-api.md) (`clipboard.*`, transactions). |
| **Contextual menu manager** | spawned by the Core | Per-contextual-menu-surface backends. See the dedicated section. |
| **GUI** | launched by the user (or via the tray) | Displays the PCs and their states, drag and drop, list of transfers, settings, approval of third-party components. Never required for nominal operation. |

The Core finds them next to itself, and launches whichever ones the build shipped
(a missing one is a line in the log, not a failure). **The tray on macOS is the one
exception**: it ships in a nested application bundle of its own,
`1Device.app/Contents/Frameworks/1DeviceTray.app`, and the supervisor
looks for it there first.

That is not tidiness, it is the only way the app opens. To Launch Services, a
process started from `Contents/MacOS` *is* the application: it takes the enclosing
bundle's identifier. The tray owns a status item, so it checks in as a GUI
application — and from the moment the Core spawned it, `org.onedevice.gui` was
already running. Every `open` of the app then merely activated the tray, which has
no window, so the Dock icon, the Finder, the Launchpad and the tray's own *Open*
item all did nothing at all, silently and without an error anywhere. With an
identifier of its own the tray is no longer mistaken for the application, and
`LSUIElement` keeps it out of the Dock and the application switcher where a
menu-bar agent has no business being. Chromium and Electron place their helpers
the same way. Windows and Linux have no such notion and keep every component
beside the Core.

### The phone (Android)

A fourth client, and the one that bends the model — for a reason that is Android's,
not ours: **there is nowhere on a phone to supervise a separate daemon.** So the
Core is linked *into the app's own process*, and the app still reaches it the
normal way — a Unix socket, the same JSON-RPC, the same [core-api.md](core-api.md)
— rather than through a bespoke in-process API. The gain is that the desktop's
Svelte UI runs there verbatim: on Android it is a component of a local Core, like
everywhere else. What follows from that:

- **It shares, it does not receive.** Two gestures, both from the system share
  sheet: text to the account's clipboard, a file to one machine the user picks.
- **A phone cannot answer a pull.** The desktop clipboard is pull-at-paste: the
  source serves the bytes when a peer pastes. Android may kill the process the
  moment the share sheet is dismissed, so a source that must not be asked later
  pushes instead — a **materialized** transaction (inline payloads only, capped,
  never files, never content marked sensitive; detail in
  [core-api.md](core-api.md)). It is additive: every desktop copy is unchanged.
- A **foreground service** is held only while something must survive the app going
  to the background (a transfer, a share waiting for its destination, a round trip
  through the browser), not merely because the app is open. Its absence would cost
  the network itself: some OEM builds cut outbound sockets in the background.

The phone's TLS trust comes from `webpki-roots` rather than the platform verifier
the desktop uses — reaching Android's trust store means crossing into Kotlin, and
the deviation is confined to the mobile shell.

### Third-party components

An explicit goal of the project: anyone can implement their component (e.g. an
alternative clipboard backend) in any language and plug it in. The contract =
the IPC API spec (versioned). Access goes through enrollment with scopes (see
Security).

## Local IPC

### Transport

- **macOS / Linux**: Unix domain socket in a private user folder
  (`$XDG_RUNTIME_DIR/1device/core.sock` on Linux).
- **Windows**: named pipe `\\.\pipe\1device-core-<USERDOMAIN>-<USERNAME>`
  with a DACL restricted to the current user's SID. The name carries the domain as
  well as the user: a local `john` and a domain `CORP\john` are two different
  users with the same `USERNAME`.
- localhost TCP is excluded (accessible to every account on the machine, no peer
  identity, firewall prompts).
- The Core verifies the **peer credentials** of every connection: `SO_PEERCRED`
  (Linux), `LOCAL_PEERCRED` (macOS), `GetNamedPipeClientProcessId` (Windows).
  On macOS, `LOCAL_PEERTOKEN` additionally provides the peer's audit token on a
  Unix socket — the clean basis (no PID race) for the level-3 code-signature
  attestation.
- **Mach ports / XPC (macOS)**: considered and set aside as the Core's primary
  IPC — Apple discourages raw Mach ports, and XPC would break the uniformity of
  the protocol and the accessibility of non-Swift/ObjC third-party components,
  without bringing a decisive advantage (audit token and launchd activation also
  exist on UDS). XPC remains an *internal* option for the macOS backend of the
  contextual menu: the FinderSync appex is necessarily sandboxed and will reach
  its manager through an app group (an XPC/Mach service of the group, or a UDS in
  the group's container — to be settled at implementation time).

### Protocol

**JSON-RPC 2.0** over the socket, LSP-style framing (`Content-Length`).

- Request/response + Core → component notifications (subscription events), all on
  the same full-duplex connection.
- Implementable by hand in any language, without a toolchain — chosen to
  maximize the ease of writing third-party components.
- No payload on the control plane: inline clipboard blobs (text/image) and
  consumer-driven file reads (IStream, FUSE…) both go through a dedicated
  **data channel** (binary, range reads).
- The API is defined as a **versioned formal spec** — see
  [core-api.md](core-api.md): it is the project's extensibility product.

### Security and enrollment

Assumed threat model: against malware running with the user's rights, no local
IPC is watertight. Realistic goals: block the machine's other accounts, apply
least privilege between components, give the user visibility and control.

- **Level 1 (OS, mandatory)**: per-user file permissions / DACL + peer
  credentials verification.
- **Level 2 (enrollment, v1)**: on a component's first connection without a
  token, the Core notifies the GUI, which shows an approval prompt ("Component
  \"X\" requests the permissions [clipboard.read, …]"). If granted: a persistent
  token bound to **scopes**. Subsequent connections: the token is enough.
  Example scopes: `devices.read`, `files.send`, `clipboard.read`,
  `clipboard.write`, `components.approve`.
- **Guardrails**:
  - The `components.approve` scope is **never** grantable via the prompt — only
    via bootstrap trust. (Otherwise: self-escalation possible.)
  - If no GUI is connected, approval requests are queued and flagged via the
    tray.
- **Level 3 (later, best-effort)**: code-signature attestation of the connecting
  process (clean on macOS via the audit token, racy elsewhere).

### Trust bootstrap

The approval prompt cannot be the root of trust (it depends on the GUI, itself a
component). Two roots:

- **B — token at spawn**: the Core passes an ephemeral token (env var / stdin) to
  the components it launches itself (clipboard manager, menu manager, tray).
- **A — file token**: at first startup, the Core writes a secret in 0600 in its
  config folder. The GUI (launched by the user) reads it and presents it at the
  handshake. Rationale: a process able to read the Core's config is already
  within the trust perimeter (it could modify the Core itself). This is the X11
  magic-cookie / Syncthing API-key pattern. It also serves as a fallback for an
  official component launched by hand (dev, debug).

### Handshake

```
component → Core : hello { name, version, role, requested scopes, token? }
Core → component : { granted scopes, API version }
```

The roles (`gui`, `clipboard-backend`, `menu-backend`, `tray`, `custom`) also
serve for arbitration: a single active clipboard backend — the exclusive
`clipboard-backend` role, see [core-api.md](core-api.md), "Roles".

## Contextual menu manager

### Two families of backends

- **Family A — dynamic registration**: the surface is driven by files or keys
  that a normal process can rewrite on the fly. The backend subscribes to the
  list of targets from the Core and rewrites the entries on every change. The
  click launches a small helper that forwards `(target, paths[])`.
  Examples: Send to (`.lnk` in `shell:sendto`), the classic Windows menu
  (`HKCU\Software\Classes\*\shell`), KDE ServiceMenus (`.desktop` in
  `~/.local/share/kio/servicemenus/`), Nautilus scripts, Thunar actions, macOS
  Services (an Automator `.workflow` bundle in `~/Library/Services`, which Finder
  shows in its Services submenu).
- **Family B — static registration, dynamic content**: the surface requires an
  artifact loaded into a host process, registered once at install time; the OS
  queries it when the menu opens. The dynamism lives in the handler: hide/show
  and enumeration of subcommands at the moment of opening.
  Examples: the Windows 11 main menu (`IExplorerCommand` COM DLL packaged
  MSIX/sparse, signed), FinderSync (appex in the signed bundle), in-process
  Nautilus extension.

### What v1 implements

Family A only, on all three desktops (`menu/`, one binary in two modes):

| OS | Surfaces |
|---|---|
| Linux | a KDE ServiceMenu for Dolphin, plus one Nautilus script per device in a submenu of its own |
| Windows | the classic menu's cascade, twice (`*` for files, `Directory` for folders), plus one "Send to" shortcut per device |
| macOS | one Automator `.workflow` per device in `~/Library/Services` |

Family B is deferred for one reason: both an `IExplorerCommand` COM DLL and a
FinderSync appex must be **signed** and registered at install time, and milestone
1 ships unsigned installers. Nothing about family A blocks them — the local
channel already answers the `targets` pull they need.

Two rules the implementation adds to the contract below:

- **A click never carries a credential.** The entry's command line starts a small
  courier that talks only to the manager over a private local socket, and holds no
  Core token of its own: the IPC token is the GUI's whole root of trust, so giving
  it to a process the shell starts with an influenceable `argv` would turn every
  writable registry key or `.desktop` file into a Core capability.
- **The marker is the authority, the container is the scope.** Every artifact
  carries a marker, and a surface prunes by enumerating the container its reader
  reads (a directory, a `shell` key) and deleting what is marked — never by
  unlinking the names the current version writes. That way an artifact left by an
  older version is swept at the next startup instead of staying in the menu for
  ever, and nothing unmarked is ever deleted.

### A backend's contract (validity criteria)

1. **Hide/show mandatory**: the entry only appears if the system is functional
   and targets exist. Fail-closed if the manager does not respond. No permanent
   entry.
2. When the menu opens, the user sees the **current list of targets** (target UX:
   `1Device → PC A / PC B / …` submenu).
3. On click, the backend reports **`(target, paths[])`** to the manager, which
   calls `files.send` on the Core. Fire-and-forget: progress lives elsewhere
   (tray/GUI).

### Flow

- Push: Server → Core → manager (subscription); the manager keeps an **in-memory
  cache** of the list of targets.
- Pull: the family-B shims query the manager's cache when the menu opens (local
  request/response, short timeout). The manager never relays this pull to the
  Core or the Server synchronously.
- In-process shims (DLLs, plugins) talk **only to their manager**, never directly
  to the Core. The Core sees only one client: the manager. The shims are an
  internal detail of each backend (a backend can be multi-binary: DLL + a part in
  the manager).

## Session & login

- First login: tray/GUI → system browser → OIDC authorization code + PKCE →
  loopback redirect captured by the Core → refresh token in the OS keyring.
- Subsequent startups: session restored from the cache, zero interaction.
- Expired session: notification via the tray → click → browser → reconnected. The
  GUI is not required for re-login.

## Pairing a device

A device joins the account by being **confirmed on a device that is already in
it**: one displays a code, the other reads it (a camera, or the same string
pasted), a human confirms on the side that gives, and the account key crosses
sealed. It needs no browser on the joining device and nothing typed. The recovery
code stays what its name says — the way back when every device is gone
(principle 3).

Wire protocol: [server-api.md](server-api.md), "Pairing". Local API:
[core-api.md](core-api.md), `pairing.*`. This section is the threat model those
two point at.

Two ways the code travels, and the rest of this section holds for both: through the
**server**, which is a rendezvous and a relay of ciphertext, or — where there is no
server at all — **dialled directly on the local network**, the code naming the
device to dial ("On the local network", below).

### The channel

The code carries three things: the session's id, a **128-bit secret**, and the
public half of an X25519 keypair minted for this one pairing. The reader sends its
own public half back through the server, and both ends derive the same key —
HKDF-SHA256, the exchange as keying material, the code's secret as the salt, both
public keys and the session id bound into the info. What crosses under it (the
account key's seed) is XChaCha20-Poly1305.

Two halves, and either one alone is worthless:

- a server that records everything lacks the optical secret — it travelled from a
  screen to a camera, a channel the server is not on;
- someone who photographs the screen lacks a private key.

So the server relays two opaque strings and can read neither. It is not, however,
what protects the account here: the optical channel is.

### What the confirmation screen is for

Photographing the screen **and** claiming the session before the legitimate reader
does work — the server hands a session to whoever claims it first. That is the
attack this design accepts, and answers with a **confirmation number**: six digits
derived from the channel key, which only the two ends of one exchange can compute.
An intruder holds a channel of its own, hence different digits from the ones on the
device in the user's hand. The name a joining device declares is not that check; it
is its own to choose.

Both sides show the number, and what is asked of the human differs:

| Side | What it shows | What can stop the pairing |
|---|---|---|
| the one that gives (sponsor) | the joining device's name and platform, the number, and "decline if it differs" | its own button: nothing crosses until it is pressed |
| the one that joins | the number, while it waits | **nothing** — the check is passive |

That asymmetry is the residual risk, and it is worth naming rather than glossing:
a user who ignores the number on the joining device can be linked into **someone
else's** account by whoever read the displayed code first — the signal they missed
being their own device's refusal, which says the code was already answered and by
whom it might have been (`PAIRING_STATE`). What it costs is a
device that syncs with a stranger, not an account of theirs that leaks: nothing of
the user's own account crosses in that direction. And what catches it afterwards is
the safety number — the fingerprint a paired device displays is derived from the key
it actually installed, so comparing it against another device's says which account
this one really joined. The account label shown beside it comes from the sponsor's
bundle: a label for the interface, never a check.

Gating the joining side as well — a second button, on the number — is the obvious
hardening and is deliberately not in v1. It would not change the proof (a human who
clicks without comparing is in the same place), it would add a step to every
legitimate pairing, and it is the asymmetry Signal's device linking has too. It
becomes worth revisiting if the passive check turns out to be one nobody reads.

### On the local network

An account with no server has no rendezvous, and needs none: the code carries the
**`node_id` to dial** where the session id was, and the other device dials it on the
data plane (`1D2:<psk>:<epk>:<node_id>`, one bidirectional stream, no new ALPN). The
device that displays the code is the one that gets dialled — either of them can be
the one displaying, so a machine with a camera and a machine without both have a
gesture.

A device that answers to a server may *scan* one of these codes and sponsor
(the continuum, principle 3): the joiner then joins the **account** — key,
roster, mutual records — and not the deployment, which simply never lists it.
Showing a `1D1` code, through the server, remains that device's way of enrolling
a newcomer on the deployment too; its `pairing.offer` still goes through the
server whenever it has one.

What changes in the threat model, and it changes for the better: **both `node_id`s
are authenticated by the transport**. The dialer reached exactly the key the code
names (iroh authenticates the remote end); the displaying side is handed the
dialer's key by the transport rather than told it in a frame, and a device that
declares a description belonging to another key is refused. So neither side can be
*posed as*, where on the server path the joining device's name is its own to choose
and only the number tells an impostor from a sibling. What is left for an attacker
is the same race, answered by the same confirmation number.

Three things the local path adds:

- **The dialer proves it read the code off a screen** before the displaying side
  spends anything: a MAC over both public halves and the `node_id`, keyed by the
  code's 128-bit secret. Checked *before* the ephemeral secret is consumed — the
  other order would let anything on the network burn a pairing window for the
  device the human is actually holding.
- **A window, and only a window.** The data plane refuses every device outside the
  account's directory without reading a byte (C7). While a pairing is open on this
  device — a deliberate human gesture, 180 seconds — a stranger is let as far as
  its first frame, and that frame may only be the pairing offer; the window shuts
  to newcomers the instant it takes a dialer. This is the only hole in
  "the directory decides who is served", and it is the hole through which the
  directory gets its first entry. What it costs, stated rather than glossed: for
  the length of a window, someone on the network can make this device read (and
  discard) frames it would otherwise not have read, and so occupy its handler
  slots. It is not a new *class* of nuisance — anyone who can reach the endpoint
  can already fill the QUIC acceptor's handshake slots, directory or no directory —
  and it lasts as long as a human keeps a code on screen.
- **The introduction is mutual.** With no server there is no directory to join:
  each side ends up holding the other's *signed* record — the joiner's attested by
  the sponsor on the spot — which is what lets the two devices carry the directory
  between themselves afterwards (principle 3, "The devices carry the directory
  between themselves").

Who sponsors is whoever holds the account's private key. Neither holding it is
refused; two *different* accounts are refused before the seed crosses (and the
devices compare accounts as a MAC under the channel key, so the local network is
not told which account either of them is in). Both holding the same key is not an
error but the ordinary case of a serverless account whose recovery code was typed
into each machine in turn: the device that displayed the code sponsors, the key that
crosses is the one the other side already has, and what the two of them are really
doing is swapping directories.

And two refusals for a device the account struck off, one per direction. Its
*dial* never reaches the window: the data plane answers it with its own tombstone
instead (principle 3, "The struck device obeys") — the one good outcome left for
that device is learning it is out. Its *code*, read on a healthy device, is
refused before anything is dialled (`DEVICE_REVOKED`): a tombstone is permanent,
so there is no pairing to attempt, and the refusal should say whose decision it
was rather than surface later as a failed pairing.

Deliberately LAN-only, and not relayed — decided, not omitted. The code's secret
travels by a screen and a camera, so the window it opens should be as narrow as
the room; a code that carried a relay hint would work from anywhere. The same
decision bounds the serverless account as a whole: its records carry no relay,
so two of its devices see each other where mDNS does, and nowhere else. If that
reach is ever widened, the honest route is the directory itself (a device
declaring its relay in its signed record) — never a public discovery that would
announce every device's whereabouts to anyone holding its `node_id`.

### The fresh token, and what it proves

`pairing.approve` demands an ID token minted no longer than
`ONEDEVICE_FRESH_TOKEN_MAX_AGE_SECS` ago, exactly as `devices.revoke` does.
What that gate proves is narrower than it looks: the Core mints the token from the
refresh token in its keyring, browserless and with no human involved
(`core/src/login.rs::fresh_id_token`). It proves the sponsor's session at the IdP
is still alive — so cutting the account's access there stops devices from being
taken in, which is the point — and it says nothing about anyone being at the
keyboard. The human-presence gate is the confirmation screen; the token is the
IdP-side kill switch.

On the local path there is no such gate and none to be had: no IdP to be fresh
with, and nothing to cut access at. What stands between a component and the account
key there is the confirmation and the `session.manage` scope, exactly as with
`devices.revoke` on a device with no server — an account that answers to nobody
answers to the machine it is on.

### What it widens locally

`pairing.*` and the `pairing` topic ride the `session.manage` scope. A component
holding it can therefore offer a code, pass it to a remote accomplice and confirm
the pairing itself, with no human at any point — the token comes from the keyring.
That is a real widening of what that scope grants, which is why the approval prompt
now reads "open and close the session, **and link new devices to the account**": it
is the only place the user is ever told. A dedicated `pairing.manage` scope was
considered and left out — the only component that needs pairing is the interface,
which needs `session.manage` anyway, so the split would move the grant without
narrowing it. It becomes worth doing the day a component wants one without the
other. Against malware running with the user's rights it changes little either way:
the same process can read the keyring the key sits in (see the IPC threat model
above).

### A device that holds no key

Enrolled in the account but holding no account key (`account.status`
→ `holds_key: false`) is a legitimate state, not a corrupt one: it is where a
device whose keyring lost the seed lands, or one whose keyring never answered.
Such a device still works — `ak_pub` is what verifies peers — it simply cannot
**sponsor**, and is not offered the gesture.

The way back in is the same door a device with no account at all uses, and the
Account screen puts both of its halves there: be paired *to* by a device that does
hold the key, or type the recovery code (entering the code of the account it is
already in changes nothing but the keyring). It has to be on that screen, because
the onboarding portal only shows for a device that is not attested. Both halves go
through the same rules, and both refuse a key other than the one this device is
already attested under.

Installing the key is verified by a **read-back** before the device claims to
have joined: a keyring `set` that answered `Ok` may only have been queued
(`daemon::secrets`), so `attested: true` with the key silently absent was
reachable, and no longer is. What remains reachable is a keyring emptied
afterwards — which is exactly the state above, and has the door.
