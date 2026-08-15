# First link — end-to-end bring-up (macOS + Windows)

This document brings a link up end to end: two machines — a Mac and a Windows PC
here — against *your* deployed server, with a real Google login, all the way to
transferring a file in both directions. It was the project's "building block 4",
and it stays the runbook for the first link of a fresh deployment.

> **This bring-up has been done.** It was written before the first real trial and
> kept afterwards, because what it walks through is exactly what breaks: the test
> suite uses a fake OIDC issuer in cleartext, so nothing in it exercises Google's
> token responses, the server's JWKS retrieval, TLS, clock drift or iroh relay
> reachability. Those are what this document is for — as a first bring-up of *your*
> deployment, and as the troubleshooting map when a link will not come up.

> **This is the from-source path.** There are installers now
> ([README, Install](../README.md#install)), and on an installed machine the
> first-run screen writes `config.json` and the Core starts at login — most of the
> steps below then have nothing to do. Follow them when you build from source, on a
> machine you drive from a terminal, or when you want the two processes in the
> foreground with their logs in front of you.

## The two pitfalls that sink a first attempt

Check them *before* starting — their symptoms are misleading:

1. **`oidc_client_id` and `oidc_issuer` must be identical on both sides.** The
   value in each client's `config.json` **must** be exactly the one passed to the
   server (`ONEDEVICE_OIDC_CLIENT_ID` / `ONEDEVICE_OIDC_ISSUER` in
   `deploy/.env`). The server checks the token's `aud` and `iss` at enrollment
   ([`server/src/oidc.rs`](../server/src/oidc.rs)). A mismatch is only visible
   **after** going through the browser, in the form of an opaque `OIDC_INVALID`.

2. **The Google OAuth client must be of type "Desktop app".** The Core uses a
   **loopback** redirect on a dynamic port, with PKCE
   ([`core/src/login.rs`](../core/src/login.rs)). A "Web application" client
   registers its redirect URIs in advance and therefore rejects that dynamic
   loopback (`redirect_uri_mismatch` in the browser, *before* the code exchange
   even). Only "Desktop app" accepts this flow. Its `client_secret`, on the other
   hand, you keep: put it in `config.json` as `oidc_client_secret` — the Core sends
   it at the exchange when it is set. Detail in
   [step 1 of the server doc](server-deployment.md#step-1--register-a-google-oidc-client).

*(The Linux `XDG_RUNTIME_DIR` pitfall from the general doc does not concern you:
neither macOS nor Windows depends on it. A normal interactive desktop session
provides everything needed — `HOME` on Mac, `APPDATA`/`USERNAME`/`USERDOMAIN` on
Windows.)*

## Prerequisites

**On the server side** (a separate machine, a VPS for example):

- A server **already deployed behind TLS** and a **registered Google client** — it
  is all in [`doc/server-deployment.md`](server-deployment.md). Only continue here
  once `curl https://<domain>/health` → `ok`.
- The server must have an **HTTPS egress to Google** (it retrieves the OIDC
  discovery + the JWKS keys on the first token;
  [`server/src/oidc.rs`](../server/src/oidc.rs)). A VPS with a closed outbound
  firewall makes enrollment fail with `OIDC_INVALID` with no distinct signal.
- The **server clock** must be synchronized (NTP): tokens are refused beyond a
  freshness window (`iat`, 300 s by default,
  `ONEDEVICE_FRESH_TOKEN_MAX_AGE_SECS`), with no margin.

**On each client machine** (the Mac and the Windows PC):

- **Rust 1.97.0** exactly (`rustup toolchain install 1.97.0`) and **Node.js 24**. If
  1.97.0 is not your default toolchain, prefix the commands with `cargo +1.97.0`.
- A C compiler (required by iroh / rustls-`ring`):
  - **macOS** — Xcode Command Line Tools (`xcode-select --install`).
  - **Windows** — MSVC Build Tools. The **WebView2** engine (preinstalled on
    up-to-date Windows 10/11) is required by the Tauri GUI.
- The cloned repository. See [README, Prerequisites](../README.md#prerequisites).

> It does not matter which one is "A" or "B": the **first** machine on which you
> choose "This is my first device" *creates* the account; the other *joins* it.
> Below, A = the one that creates, B = the one that joins.

## Overview

```
deployed server (TLS) + Google "Desktop app" client
        │
        ├── Machine A : build → config.json → Core → GUI → Google login
        │               → "first device" → RECOVERY CODE
        │
        ├── Machine B : build → config.json → Core → GUI → login (SAME Google account)
        │               → "I already have a device" → IDENTICAL FINGERPRINTS
        │
        └── Transfer : drag a file A→B, then B→A
```

The **same Google account** must be used on A and B: the directory is partitioned
by the token's `sub` identifier ([`server/src/conn.rs`](../server/src/conn.rs)). Two
devices only see each other under **a single** account.

## Step 1 — Machine A (first device, *creates* the account)

### 1.1 Build

```sh
git clone https://github.com/iburel/universal-link.git
cd universal-link

# a) the web UI (produces gui/ui/dist, embedded in the binary)
cd gui/ui && npm ci && npm run build && cd ../..

# b) the Core and the rest of the binaries
cargo build --workspace --lib --bins --locked

# c) the actual GUI binary (system webview)
cargo build -p onedevice-gui --features webview --locked
```

Step (a) **must** precede (c): the GUI binary embeds `gui/ui/dist` at compile time.
A link error mentioning webkit/gtk on Linux flags missing headers — moot on
Mac/Windows.

### 1.2 Write `config.json`

The Core **never creates** this file; it reads it. The simplest way: launch the
Core once — it creates the config folder and logs "Core not configured" — then drop
the file there.

Location of the config folder:

| OS | Config folder |
|---|---|
| macOS | `~/Library/Application Support/1Device/` |
| Windows | `%APPDATA%\1Device\` |

Contents of `config.json` (`server_url`, `oidc_issuer` and `oidc_client_id` are
**mandatory together**):

```json
{
  "server_url": "wss://your-server.example.com/ws",
  "oidc_issuer": "https://accounts.google.com",
  "oidc_client_id": "xxxxxxxx.apps.googleusercontent.com",
  "oidc_client_secret": "GOCSPX-…",
  "device_name": "Living-room Mac"
}
```

- `server_url` must start with `wss://` (or `ws://`) and point to `/ws`.
- `oidc_issuer` / `oidc_client_id`: **exactly** the server's values (pitfall #1).
- `oidc_client_secret` is optional and belongs on the **client** only (the server
  never sees it): the Google Desktop-app secret, sent at the token exchange. Not a
  secret in the usual sense — it ships with any installed app.
- `device_name` is optional (default: the hostname) — it is a display label, not an
  identity.
- Also optional: `relay` (off by default; a self-hosted iroh relay's URL, or
  `"n0"` to opt into the public relays) and `receive_dir` (otherwise
  `<Downloads>/1Device`).

An incomplete trio, a scheme typo (`https://…/ws` instead of `wss://`), or broken
JSON: the Core **still starts** but *not configured*, and any login will answer
`SERVER_UNREACHABLE`. It logs the problem precisely. Detail of the keys:
[`daemon/src/config.rs`](../daemon/src/config.rs).

### 1.3 Launch the Core (in the foreground, let it run)

macOS (Terminal):

```sh
ONEDEVICE_LOG=debug cargo run --bin 1device-core --locked
```

Windows (PowerShell):

```powershell
$env:ONEDEVICE_LOG = "debug"; cargo run --bin 1device-core --locked
```

**Expected**: the lines `keyring chosen` then `Core listening` with the IPC path.
**No** `Core not configured` if `config.json` is complete. The process stays in the
foreground until `Ctrl-C`. (`ONEDEVICE_LOG` — **not** `RUST_LOG`.)

If you relaunch while a Core is already running for this user: it logs "a Core is
already running" and exits cleanly (single-instance lock).

### 1.4 Launch the GUI (another terminal; it does not start the Core)

```sh
cargo run -p onedevice-gui --features webview --locked
```

**Expected**: a window opens and the state switches to "connected". The GUI joins
the already-launched Core via the local socket and the `ipc-token` — it does not
*spawn* it. Stuck on "connecting…" → the Core is not listening (review 1.3).

### 1.5 Log in (Google login)

Click the connect button: the system browser opens the Google screen, you
authenticate, and the **loopback** redirect (`http://127.0.0.1:<port>/…`, dynamic
port, nothing to register) is captured by the Core. On the first login, the device
**enrolls** in the directory.

**Expected**: after consent, the Account screen displays your email; `session.json`
appears in the config folder. Common failures → see [Troubleshooting](#troubleshooting).

### 1.6 Create the account (blocking portal after login)

Choose **"This is my first device"**. A **recovery code** is displayed: it is your
way back if you ever lose every device — **write it down offline** (password
manager, paper). The Account screen then displays a **fingerprint** (safety number);
remember it for step 2.6.

Under the hood: `account.setup` publishes the account attestation (C7) to the server,
writes `account-key.json` in the config folder, and stows the account's private key
in the keyring (or `secrets.json` when no keyring answers).

## Step 2 — Machine B (second device, *joins* the account)

Repeat **2.1 → 2.5 identically** on the other machine (the other OS), with:

- a `config.json` with the **same** `server_url` / `oidc_issuer` / `oidc_client_id`,
  and a distinct `device_name` (e.g. `"Office PC"`);
- the **same Google account** at login.

**Expected after login (2.5)**: each GUI now sees the other device in the
**Devices** screen (A sees B, B sees A). If they do not see each other → different
Google account, or one Core has not yet received its first directory snapshot.

### 2.6 Join the account

At the portal, choose **"I already have a device on this account"**. Two ways in
from there, and they end in the same place — the same `account-key.json`, the same
key in the keyring.

**Either pair with machine A** (nothing typed). On B press **"Show a code"**: a QR
code appears with the same string spelled out underneath. On A, Devices screen →
*Add a device* → **"Enter a code…"**, and paste that line (a PC has no camera; a
phone would press *Scan a code* and point it at B's screen). A then shows what it is
about to add — B's name, its platform, and a **six-digit number**. That number must
be the one B is showing: check it, then **"Add to my account"**. The bundle crosses,
B installs the key and enrolls by itself.

- If A asks for the browser once more before confirming ("your account has to be
  confirmed once more"), that is the server wanting a fresh ID token, exactly as for
  a revocation. Complete the tab and the confirmation resumes on its own.
- A code lives two minutes and works once. A second attempt needs a new code.
- If the two numbers **differ**, decline: someone else answered the code — see the
  threat model in [architecture.md](architecture.md#pairing-a-device).

**Or enter the recovery code** from step 1.6, which still works and is the way in
when the server is older than pairing (every `pairing.*` then answers `-32601` and
the buttons say so).

**Expected**, whichever way: the fingerprint displayed on B must be **identical** to
the one seen on A (compare them visually). Identical fingerprints = same account key
on both sides. A **different** fingerprint betrays a wrong code, a substitution, or a
pairing someone else answered: B would remain *fail-closed* outside the account.

> Without this attachment, **every send fails**: it is the account attestation (C7),
> not mere presence in the directory, that authorizes a peer.

## Step 3 — Transfer a file

### 3.1 A → B

On A's **Devices** screen, **drag one or more files directly onto B's card** (which
must be **online**). There is **no picker**: dropping outside an eligible card (empty
space, an offline device, or your own PC) does nothing.

**Expected**: A shows `transfer.started` then `transfer.finished`; the file lands in
B's `receive_dir` (by default `<Downloads>/1Device` —
`~/Downloads/1Device` on Mac, `%USERPROFILE%\Downloads\1Device` on
Windows). A folder can be dropped too, and arrives as a folder.

### 3.2 B → A

Redo the operation the other way from B's Devices screen, onto A's card. Reception is
**automatic** (v1: these are your own devices); the names are sanitized and never
overwrite an existing file.

### 3.3 Verify

On the receiving machine, list the receive folder:

```sh
# macOS
ls -l ~/Downloads/1Device
```
```powershell
# Windows
dir $env:USERPROFILE\Downloads\1Device
```

The files must be present, at the right size. A leftover `.part` file = interrupted
transfer (deleted automatically on failure).

## Troubleshooting

### Decoder for the GUI's error codes

| Code | Probable cause |
|---|---|
| `SERVER_UNREACHABLE` | `config.json` absent/incomplete, or WS server unreachable (URL, TLS, DNS). |
| `OIDC_INVALID` | `client_id`/`issuer` diverging between client and server (**pitfall #1**), server unable to reach the Google JWKS, or clock drift > 300 s. |
| `redirect_uri_mismatch` (browser) | The Google client is **not** of type "Desktop app" (**pitfall #2**). |
| `access_denied` (browser) | Consent screen in "Testing" and the account not added as a test user. |
| `DEVICE_UNKNOWN` | C7 attestation absent/invalid (one side has not done *setup*/*join*), or no directory snapshot yet. |
| `DEVICE_OFFLINE` | Peer known but **with no route**: no relay (its iroh has not joined one, or the server has not registered its `relay_url`), not heard on the LAN, and its record carries no signed reach hints ([beyond-the-lan.md](beyond-the-lan.md)). |
| `NO_DIRECT_PATH` | The deployment's relays are **rendezvous-only** above a size cap ([server-deployment.md](server-deployment.md)) and hole punching found no direct path between the two devices. The pair is what fails, not one device; the same network or a VPN between them restores the path. |

Sources: [`core/src/login.rs`](../core/src/login.rs),
[`server/src/oidc.rs`](../server/src/oidc.rs),
[`core/src/dataplane.rs`](../core/src/dataplane.rs).

### "Both devices are online but the transfer stalls"

This is the most likely friction point, and the reason is worth knowing: the iroh
data plane is in a **minimal** preset — **automatic discovery is disabled** (no
DNS; mDNS covers the local network only), and the relay is **off by default**, so
off the LAN two peers only meet through a route somebody set up: a VPN between
them, their signed address hints, or a relay opted into with the `relay` setting
(a URL, or `"n0"` for the public relays). With a relay, the peers **meet through
it** (rendezvous); NAT traversal (hole-punching) stays active and a **direct
route** can form after the rendezvous, the relay remaining the fallback
channel, unless the deployment announced its relays rendezvous-only above a
size cap, in which case an over-cap operation with no punched direct path
fails with `NO_DIRECT_PATH` instead of riding the relay.
Practical consequence: to establish the initial connection through a relay,
**both machines must have a UDP egress to a common relay** (corporate firewall,
restricted network → failure). Lead: host your own relay and set it as `relay`
in both `config.json`. See
[`daemon/src/dataplane.rs`](../daemon/src/dataplane.rs).

### Where to look

Core logs (relaunch it with `ONEDEVICE_LOG=debug`):

| OS | Logs |
|---|---|
| macOS | `~/Library/Logs/1Device` |
| Windows | `%LOCALAPPDATA%\1Device\logs` |

State files, in the config folder (§1.2):

- `session.json` — present ⟺ a session is open (you are logged in).
- `account-key.json` — present ⟺ the device has joined the account.
- `directory.json` — the account's devices as last known (removed at logout).
- `revoked.json` — the devices struck off, signed by the account key. Permanent,
  and kept across a logout: deleting it takes those devices back in.
- `ipc-token` — regenerated at every startup (the GUI's root of trust).
- `secrets.json` — appears **only** on a machine with no system keyring (0600
  fallback). On Mac (Keychain) and Windows (Credential Manager), it should not exist.

On the server side: `docker compose logs -f server` (expected `server listening`, no
config error line) and `docker compose logs -f caddy` (certificate acquisition).
Verification reminders:
[`doc/server-deployment.md`](server-deployment.md#verify-the-deployment).

### Replaying cleanly

To start from scratch on a machine: stop the Core, delete `account-key.json`
(otherwise `account.setup` answers `ACCOUNT_KEY_SET`) and possibly `session.json`,
plus `directory.json` and `revoked.json` if you want no memory of the old account's
devices at all, then resume at login. The account key stays in the keyring; drop its
`account-key-seed` entry too (or `secrets.json` wholesale) to leave nothing
behind. Beware: erasing all of this **everywhere** without having the recovery
code cuts you off from the account.

## How this differs from an installed machine

- **Nothing starts by itself**: two foreground processes per machine, launched by
  hand. An installed machine gets autostart instead (see
  [`doc/deployment.md`](deployment.md)).
- The background components, on the other hand, you do get: the Core spawns the
  tray, the clipboard backend and the contextual menu when it finds them **next to
  its own binary**, and `cargo build --workspace --bins` puts all four in the same
  `target/` folder. Copy the Core somewhere on its own and it starts alone.
- On Windows, a Core launched this way *does* have a console, so it receives the
  shutdown/logoff signals — the one case where the manual path is better behaved
  than the installed one
  ([`daemon/src/main.rs`](../daemon/src/main.rs)).

See also: [README, Part 4](../README.md#piece-4--launch-connect-attach-send),
[`doc/deployment.md`](deployment.md) (Core reference),
[`doc/core-api.md`](core-api.md) (Core ↔ GUI protocol).
