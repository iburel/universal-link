# First link — end-to-end bring-up (macOS + Windows)

This document describes the **first real bring-up**: linking a Mac and a Windows
PC to *your* deployed server, with a real Google login, all the way to transferring
a file in both directions. It is the project's "building block 4".

> **What "first" honestly means.** Every piece of the path (server, config,
> login/enrollment, account key, P2P transfer) is wired and tested *in isolation* —
> but the whole thing has **never** run against a real Google on two real machines:
> the tests use a fake OIDC issuer in cleartext (see
> [`doc/server-deployment.md`](server-deployment.md), section "State"). This
> bring-up **is** that first validation. There is **no known blocker in the code**;
> expect, on the other hand, to flush out real-world things that no test covers:
> Google token responses, the server's retrieval of the JWKS keys, TLS, clock
> drift, iroh relay reachability. When something breaks, it is not necessarily a
> bug — it is the point of this step.

> **No installer for now.** On each machine you compile the Core and the GUI **from
> source**, write `config.json` **by hand**, and launch **two foreground
> processes** (the Core then the GUI). Packaging (installers, autostart) is a later
> building block.

## The two pitfalls that sink a first attempt

Check them *before* starting — their symptoms are misleading:

1. **`oidc_client_id` and `oidc_issuer` must be identical on both sides.** The
   value in each client's `config.json` **must** be exactly the one passed to the
   server (`UNIVERSALLINK_OIDC_CLIENT_ID` / `UNIVERSALLINK_OIDC_ISSUER` in
   `deploy/.env`). The server checks the token's `aud` and `iss` at enrollment
   ([`server/src/oidc.rs`](../server/src/oidc.rs)). A mismatch is only visible
   **after** going through the browser, in the form of an opaque `OIDC_INVALID`.

2. **The Google OAuth client must be of type "Desktop app".** The Core uses a
   **loopback** redirect on a dynamic port and sends **no** `client_secret` (public
   client + PKCE, [`core/src/login.rs`](../core/src/login.rs)). A "Web application"
   client is incompatible with both: it rejects the dynamic loopback redirect
   (`redirect_uri_mismatch` error in the browser, *before* the code exchange even)
   and/or requires a `client_secret` at the exchange. Only "Desktop app" accepts
   this flow. Detail in
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
  `UNIVERSALLINK_FRESH_TOKEN_MAX_AGE_SECS`), with no margin.

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
git clone https://github.com/iburel/UniversalLink.git
cd UniversalLink

# a) the web UI (produces gui/ui/dist, embedded in the binary)
cd gui/ui && npm ci && npm run build && cd ../..

# b) the Core and the rest of the binaries
cargo build --workspace --lib --bins --locked

# c) the actual GUI binary (system webview)
cargo build -p universallink-gui --features webview --locked
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
| macOS | `~/Library/Application Support/UniversalLink/` |
| Windows | `%APPDATA%\UniversalLink\` |

Contents of `config.json` (the first three keys are **mandatory together**):

```json
{
  "server_url": "wss://your-server.example.com/ws",
  "oidc_issuer": "https://accounts.google.com",
  "oidc_client_id": "xxxxxxxx.apps.googleusercontent.com",
  "device_name": "Living-room Mac"
}
```

- `server_url` must start with `wss://` (or `ws://`) and point to `/ws`.
- `oidc_issuer` / `oidc_client_id`: **exactly** the server's values (pitfall #1).
- `device_name` is optional (default: the hostname) — it is a display label, not an
  identity.
- Also optional: `relay_url` (self-hosted iroh relay; otherwise the n0 public
  relays) and `receive_dir` (otherwise `<Downloads>/UniversalLink`).

An incomplete trio, a scheme typo (`https://…/ws` instead of `wss://`), or broken
JSON: the Core **still starts** but *not configured*, and any login will answer
`SERVER_UNREACHABLE`. It logs the problem precisely. Detail of the keys:
[`daemon/src/config.rs`](../daemon/src/config.rs).

### 1.3 Launch the Core (in the foreground, let it run)

macOS (Terminal):

```sh
UNIVERSALLINK_LOG=debug cargo run --bin universallink-core --locked
```

Windows (PowerShell):

```powershell
$env:UNIVERSALLINK_LOG = "debug"; cargo run --bin universallink-core --locked
```

**Expected**: the lines `keyring chosen` then `Core listening` with the IPC path.
**No** `Core not configured` if `config.json` is complete. The process stays in the
foreground until `Ctrl-C`. (`UNIVERSALLINK_LOG` — **not** `RUST_LOG`.)

If you relaunch while a Core is already running for this user: it logs "a Core is
already running" and exits cleanly (single-instance lock).

### 1.4 Launch the GUI (another terminal; it does not start the Core)

```sh
cargo run -p universallink-gui --features webview --locked
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

Choose **"This is my first device"**. A **recovery code** is displayed: it is the
**only copy** of the account's private key — **write it down offline** (password
manager, paper). The Account screen then displays a **fingerprint** (safety number);
remember it for step 2.6.

Under the hood: `account.setup` publishes the account attestation (C7) to the server
and writes `account-key.json` in the config folder.

## Step 2 — Machine B (second device, *joins* the account)

Repeat **2.1 → 2.5 identically** on the other machine (the other OS), with:

- a `config.json` with the **same** `server_url` / `oidc_issuer` / `oidc_client_id`,
  and a distinct `device_name` (e.g. `"Office PC"`);
- the **same Google account** at login.

**Expected after login (2.5)**: each GUI now sees the other device in the
**Devices** screen (A sees B, B sees A). If they do not see each other → different
Google account, or one Core has not yet received its first directory snapshot.

### 2.6 Join the account

At the portal, choose **"I already have a device on this account"** and enter the
**recovery code** from step 1.6.

**Expected**: the fingerprint displayed on B must be **identical** to the one seen on
A (compare them visually). Identical fingerprints = same account key on both sides. A
**different** fingerprint betrays a wrong code or a substitution: B would remain
*fail-closed* outside the account — re-enter the correct code.

> Without this attachment, **every send fails**: it is the account attestation (C7),
> not mere presence in the directory, that authorizes a peer.

## Step 3 — Transfer a file

### 3.1 A → B

On A's **Devices** screen, **drag one or more files directly onto B's card** (which
must be **online**). There is **no picker**: dropping outside an eligible card (empty
space, an offline device, or your own PC) does nothing.

**Expected**: A shows `transfer.started` then `transfer.finished`; the file lands in
B's `receive_dir` (by default `<Downloads>/UniversalLink` —
`~/Downloads/UniversalLink` on Mac, `%USERPROFILE%\Downloads\UniversalLink` on
Windows). **v1: flat files**, no folder tree.

### 3.2 B → A

Redo the operation the other way from B's Devices screen, onto A's card. Reception is
**automatic** (v1: these are your own devices); the names are sanitized and never
overwrite an existing file.

### 3.3 Verify

On the receiving machine, list the receive folder:

```sh
# macOS
ls -l ~/Downloads/UniversalLink
```
```powershell
# Windows
dir $env:USERPROFILE\Downloads\UniversalLink
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
| `DEVICE_OFFLINE` | Peer known but **with no relay**: its iroh has not joined a relay, or the server has not registered its `relay_url`. |

Sources: [`core/src/login.rs`](../core/src/login.rs),
[`server/src/oidc.rs`](../server/src/oidc.rs),
[`core/src/dataplane.rs`](../core/src/dataplane.rs).

### "Both devices are online but the transfer stalls"

This is the most likely friction point, and it is **unproven in the real world**: the
iroh data plane is in a **minimal** preset — **automatic discovery is disabled** (no
LAN/DNS), so the two peers **meet via the relay**. The relay then remains the
fallback channel; NAT traversal (hole-punching) stays active and a **direct route**
can form after the rendezvous. Without a configured `relay_url`, it is the **n0**
public relays. Practical consequence: to establish the initial connection, **both
machines must have a UDP egress to a common relay** (corporate firewall, restricted
network → failure). Lead: host your own relay and set it as `relay_url` in both
`config.json`. See [`daemon/src/dataplane.rs`](../daemon/src/dataplane.rs).

### Where to look

Core logs (relaunch it with `UNIVERSALLINK_LOG=debug`):

| OS | Logs |
|---|---|
| macOS | `~/Library/Logs/UniversalLink` |
| Windows | `%LOCALAPPDATA%\UniversalLink\logs` |

State files, in the config folder (§1.2):

- `session.json` — present ⟺ a session is open (you are logged in).
- `account-key.json` — present ⟺ the device has joined the account.
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
then resume at login. Beware: deleting `account-key.json` **everywhere** without
having the recovery code cuts you off from the account.

## What is not there yet

- **No installer or autostart**: everything is manual, two foreground processes. On
  Windows, the Core only receives the shutdown/logoff signals if it has a **console
  attached** ([`daemon/src/main.rs`](../daemon/src/main.rs)) — irrelevant for a
  manual launch, to be handled at packaging time.
- **Real Google path not yet proven**: this document *is* the first trial. Record
  here any deviation observed.
- **Relay not proven in the real world** (see above).

See also: [README, Part 4](../README.md#piece-4--launch-connect-attach-send),
[`doc/deployment.md`](deployment.md) (Core reference),
[`doc/core-api.md`](core-api.md) (Core ↔ GUI protocol).
