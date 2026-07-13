# UniversalLink — Overall architecture (Phase 1)

> Summary document from the initial design phase. Describes the major building
> blocks and the decisions that have been settled.

## Goal

Link a single user's PCs (macOS, Windows, Linux) to transfer files and content
in several ways:

- Shared clipboard (copy on one PC, paste on another)
- Contextual menu ("right click → send to PC X")
- Drag and drop (via the GUI)
- Shared folder
- Remote filesystem exposure

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

2. **Files never travel over the local IPC.** Components exchange *paths* and
   control; the Core reads/writes the disk itself (same machine) and streams via
   iroh. Only the clipboard's content travels over the IPC (small payloads). The
   IPC is a control plane: simple, reliable, secure — no need for high
   performance.

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
     the account's public key + its attestation, then **discards** the private
     key: after onboarding, no device holds the account's private key at rest;
     the code (with the user) is its only copy.
   - **Out-of-band verification**: a fingerprint (safety number) of the account
     key, identical on every device, is compared visually — it diverges as soon
     as one device has derived a different key or a substitution has taken place.
   - The attestation binds the `node_id` alone (stable crypto identity), not the
     `device_id` (ephemeral server label): it survives a re-enrollment. The
     signed payload is versioned to allow a later key rotation.
   - **Revocation**: removing a specific device = striking it from the server
     directory (`devices.revoke`); the "compromised device that keeps the
     secret" case is handled by rotating the account key — a follow-up building
     block.

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
| **Clipboard manager** | spawned by the Core | Per-OS backends to read/write the clipboard and be notified of changes. Handles the "blocking paste" for the duration of the download. **Detailed design in phase 2.** |
| **Contextual menu manager** | spawned by the Core | Per-contextual-menu-surface backends. See the dedicated section. |
| **GUI** | launched by the user (or via the tray) | Displays the PCs and their states, drag and drop, list of transfers, settings, approval of third-party components. Never required for nominal operation. |

### Third-party components

An explicit goal of the project: anyone can implement their component (e.g. an
alternative clipboard backend) in any language and plug it in. The contract =
the IPC API spec (versioned). Access goes through enrollment with scopes (see
Security).

## Local IPC

### Transport

- **macOS / Linux**: Unix domain socket in a private user folder
  (`$XDG_RUNTIME_DIR/universallink/core.sock` on Linux).
- **Windows**: named pipe `\\.\pipe\universallink-<sid>` with a DACL restricted
  to the current user.
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
- Inline payloads (clipboard text/image) as base64 on the control plane;
  consumer-driven file reads (IStream, FUSE…) go through a dedicated **data
  channel** for range reads.
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

The roles (`gui`, `clipboard-backend`, `menu-backend`, `tray`, `custom`) will
also serve for arbitration (e.g. a single active clipboard backend — to be
settled in phase 2).

## Contextual menu manager

### Two families of backends

- **Family A — dynamic registration**: the surface is driven by files or keys
  that a normal process can rewrite on the fly. The backend subscribes to the
  list of targets from the Core and rewrites the entries on every change. The
  click launches a small helper that forwards `(target, paths[])`.
  Examples: Send to (`.lnk` in `shell:sendto`), the classic Windows menu
  (`HKCU\Software\Classes\*\shell`), KDE ServiceMenus (`.desktop` in
  `~/.local/share/kio/servicemenus/`), Nautilus scripts, Thunar actions.
- **Family B — static registration, dynamic content**: the surface requires an
  artifact loaded into a host process, registered once at install time; the OS
  queries it when the menu opens. The dynamism lives in the handler: hide/show
  and enumeration of subcommands at the moment of opening.
  Examples: the Windows 11 main menu (`IExplorerCommand` COM DLL packaged
  MSIX/sparse, signed), FinderSync (appex in the signed bundle), in-process
  Nautilus extension.

### A backend's contract (validity criteria)

1. **Hide/show mandatory**: the entry only appears if the system is functional
   and targets exist. Fail-closed if the manager does not respond. No permanent
   entry.
2. When the menu opens, the user sees the **current list of targets** (target UX:
   `UniversalLink → PC A / PC B / …` submenu).
3. On click, the backend reports **`(target, paths[])`** to the manager, which
   calls `send_files` on the Core. Fire-and-forget: progress lives elsewhere
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
