# 1Device

[![License: AGPL v3](https://img.shields.io/badge/License-AGPL_v3-blue.svg)](LICENSE)

Link the machines of a single user — Windows, macOS, Linux, and an Android phone
— to move files and content between them, **end-to-end encrypted**: the server
never sees the data, and does not even decide on its own who belongs to the
account.

Written in **Rust** (a Cargo workspace) with a **Tauri / Svelte** interface.

## What it does

- **Right click → send to another machine.** Pick a selection of files or a folder
  in Explorer, Finder, Dolphin or Nautilus, choose one of your machines, and it is
  on its way — no window to open, no drag. Only machines that are online, attested
  and reachable are listed, and the entries disappear entirely while there is no
  server connection: the menu never offers a destination it cannot reach.
- **A clipboard shared across your machines.** Copy on one, paste on another:
  text, images, files, whole folders. The payload crosses the network only when
  you actually paste, straight between the two machines. A copy the OS marks as
  confidential — a password manager's — stays marked on the way: it is announced
  without even a size, and the machine you paste on flags it in turn, so its
  clipboard history and cloud sync leave it alone too.
- **Drag files onto a machine's card.** The app lists your machines and their
  state; drop files or a folder on one to send it. Receipt on the other side is
  automatic — these are your own devices.
- **Share from your phone.** From Android's share sheet: text goes to the
  account's clipboard, ready to paste anywhere, and a file goes to the one machine
  you pick.
- **Add a machine by showing it a code.** One of your devices displays a QR code,
  the new one reads it — the phone with its camera, a PC by pasting the same line —
  and one confirmation on the machine that is already on the account hands over
  everything the newcomer needs to be trusted by the others. Both screens show the
  same six digits, so a code someone read over your shoulder is caught before
  anything crosses. Nothing to retype: your recovery code goes back to being what
  its name says, the way back if you ever lose every device.

## Install

Installers are published on the [Releases](https://github.com/iburel/universal-link/releases)
page, built by CI from the tag:

| | Asset | |
|---|---|---|
| **Windows** | `1Device_<v>_x64-setup.exe` | NSIS, per-user — no admin rights |
| **macOS** | `1Device_<v>_aarch64.dmg` | Apple Silicon |
| **Linux** | `1Device_<v>_amd64.AppImage` | `chmod +x`, then run |
| **Android** | `1Device_<v>_arm64.apk` | arm64 only, Android 7.0 or newer |

**The desktop builds are not code-signed** (milestone 1), so the OS warns on
first launch: on macOS, "unverified developer" → System Settings → Privacy &
Security → *Open anyway*; on Windows, SmartScreen → *More info* → *Run anyway*.
The Linux AppImage needs FUSE 2 (`sudo apt install libfuse2`) and, for the tray
icon, `libayatana-appindicator3` plus a StatusNotifierItem host (on GNOME, the
AppIndicator extension). The **Android APK is signed**, with the project's own
key — Android installs nothing otherwise — which makes it a sideload, not a Play
Store listing. It asks for the **camera** the first time you scan a pairing code,
and not before: the permission is optional, and the scanner is the app's own
(CameraX + ZXing, no Play Services), so a de-Googled phone reads a code just as
well.

On first launch the app installs the background service (per-user, no admin),
registers it to start at login, and opens a setup screen. **Nothing is baked into
the build**: it asks for your server's address — the server itself supplies the
OpenID Connect client to sign you in with — which means the one thing you must
provide yourself is a server, see
[Set up a real link](#set-up-a-real-link-between-your-machines).

## Status

Milestone 1: it installs and works on all four platforms, and it is **not yet
signed**. CI is green on the three desktops and builds the Android app on every
push. The path everything rests on — a real OIDC login against a deployed server,
two machines attesting each other, transfers both ways — has been through a real
bring-up, not just an in-memory test.

> What is still missing:
>
> - **Nothing on the desktop is code-signed or notarized.** First launch shows an
>   OS warning (see above). This also keeps two richer OS integrations out of
>   reach: the Windows 11 main context menu and a Finder extension both require a
>   signed, install-time-registered artifact.
> - **You host the server yourself**, and the image is not published to a
>   registry: you build it from [`deploy/`](deploy/).
> - **Outbound drag** — from the app back onto the desktop — is not implemented.
>   Inbound is.
> - **The phone shares, it does not receive**: nothing writes to the Android
>   clipboard, and a file sent to the phone lands where nothing yet opens it.
> - **Account key rotation** is not implemented.

### What works today

| Capability | State |
|---|---|
| Windows / macOS / Linux client: installer, autostart, background Core | ✅ implemented |
| Android client: share text to the clipboard, a file to one machine | ✅ signed APK |
| OIDC login (authorization code + PKCE, browser → loopback) | ✅ implemented |
| Device enrollment and directory (`devices.list` / `rename` / `revoke`) | ✅ implemented |
| Account attachment (create / join via recovery code) + peer attestation | ✅ implemented |
| Add a device by pairing: show a code, scan or paste it, confirm | ✅ implemented |
| Send files **and folders**: drag onto a card, context menu, phone share | ✅ implemented |
| Shared clipboard: text, images, files, folders | ✅ implemented |
| Context menu "send to machine X" (Explorer, Finder, Dolphin, Nautilus) | ✅ implemented |
| Tray / notifier | ✅ implemented |
| iroh data plane (E2E-encrypted QUIC, NAT traversal, relays) | ✅ implemented |
| Server: Docker image + Caddy auto-TLS, persisted directory, real bring-up | ✅ deployable, validated |
| Explore the UI without installing anything (fake Core, browser) | ✅ `npm run dev` |
| **Code signing / notarization** (and the integrations that need it) | ❌ upcoming |
| **Published server image** | ❌ you build it |
| **Outbound drag-and-drop** | ❌ upcoming |
| **Receiving on the phone** | ❌ upcoming |
| **Account key rotation** | ❌ upcoming |

The design details (and what is deliberately deferred) live in
[`doc/`](doc/): [architecture](doc/architecture.md),
[Core API](doc/core-api.md), [server API](doc/server-api.md),
[server deployment](doc/server-deployment.md),
[identity providers](doc/identity-providers.md),
[first link](doc/first-link.md),
[Core deployment](doc/deployment.md),
[beyond the LAN](doc/beyond-the-lan.md). Release by release:
[CHANGELOG.md](CHANGELOG.md).

## Architecture at a glance

```
                      ┌────────────┐
                      │   Server   │  OIDC · directory · presence · signaling
                      └─────┬──────┘  (CONTROL plane — blind to the data)
          ┌─────────────────┼─────────────────┐
     ┌────┴────┐       ┌────┴────┐       ┌────┴────┐
     │  PC A   │◄─────►│  PC B   │       │  Phone  │
     │ (Core)  │ iroh  │ (Core)  │       │ (Core)  │
     └─────────┘ P2P   └─────────┘       └─────────┘
              (direct, else relayed — data end-to-end encrypted)
```

On each machine, a **Core** (session daemon) holds the server session, the device
identity (its iroh key) and transfers, and exposes a **local IPC API**
(JSON-RPC 2.0 over a Unix socket / named pipe) to components — the tray, the
clipboard backend, the context-menu manager, and the GUI. On a desktop the Core
is its own process, supervising those components; on Android it is embedded in
the app's process, since a phone has nowhere to supervise a daemon.

The server is removed from the trust decision about *who belongs to the account*:
an **account key** derived from a recovery code (never known to the server)
attests each device, and a peer refuses any device whose attestation does not
verify (*fail-closed*).

## Set up a real link between your machines

Two pieces have to exist before your machines can see each other, and neither is
turnkey: an **OIDC client** and a **server**. You register the OIDC client with
the server, once — every device then reads it from there, and the app's setup
screen asks each of them for one thing: the server's address.

### Piece 1 — an OIDC client

The server authenticates accounts via **OIDC**; the reference issuer is **Google**
(`accounts.google.com`). You need a client that does **authorization code + PKCE
with a loopback redirect**: its `client_id`, the issuer URL, and — if your IdP
demands one at the token exchange, as Google's installed-app clients do — its
`client_secret`. The Core sends the secret only if you configure one; for an
installed application that value is not confidential (it ships inside every copy
of the app), which is why it lives in a config file rather than a keyring.

> ⚠️ On Google, create a client of type **"Desktop app"**, **not "Web
> application"**. A web client's redirect URIs must all be registered in advance,
> and the Core redirects to `http://127.0.0.1:<port chosen at runtime>` — which
> such a client rejects (`redirect_uri_mismatch`) in the browser, before the code
> exchange is even reached. Step by step:
> [Deploy the server, step 1](doc/server-deployment.md#step-1--register-a-google-oidc-client).

### Piece 2 — a running server

The `1device-server` binary (crate `server-daemon`) is configured through
the environment and starts the control plane (WebSocket `/ws`, `GET /health`,
and `GET /.well-known/1device.json` — the deployment descriptor, from which
a client reads the OIDC settings below instead of having them typed in):

```sh
# Add ONEDEVICE_OIDC_CLIENT_SECRET=… if your IdP demands one (Google does).
ONEDEVICE_SERVER_BIND=0.0.0.0:8080 \
ONEDEVICE_OIDC_ISSUER=https://accounts.google.com \
ONEDEVICE_OIDC_CLIENT_ID=…apps.googleusercontent.com \
cargo run --bin 1device-server --locked
```

Optional settings (with their defaults): `ONEDEVICE_OIDC_CLIENT_SECRET`
(none; the server never uses it — it advertises it in the descriptor for the
clients, and Google's installed-app clients need it at the token exchange),
`ONEDEVICE_SERVER_STATE`
(`1device-directory.json` — the directory file, to point at a volume in a
deployment), `ONEDEVICE_HEARTBEAT_SECS` (30),
`ONEDEVICE_HEARTBEAT_MAX_MISSED` (2), `ONEDEVICE_NONCE_TTL_SECS` (60),
`ONEDEVICE_FRESH_TOKEN_MAX_AGE_SECS` (300),
`ONEDEVICE_JWKS_REFRESH_MIN_SECS` (60),
`ONEDEVICE_MAX_REQUESTS_PER_MINUTE` (120; `0` = unlimited); log level via
`ONEDEVICE_LOG`. On an incomplete or invalid config, the server **refuses to
start** and logs every error at once.

The directory (device identities, account attestations, revocations) is
**persisted to disk**: enrollments survive a restart.

For a **real deployment** — automatic TLS (the server listens in cleartext, a
reverse proxy terminates TLS; the Core requires `wss://`), Docker image and
Caddy stack ready to use — follow
**[Deploy the server](doc/server-deployment.md)**. In short:

```sh
cd deploy
cp .env.example .env      # domain + OIDC issuer + client (+ secret if needed)
docker compose up -d      # pulls the published image, nothing to compile
```

### Piece 3 — `config.json` on each PC

The installed app's **first-run screen writes this file for you**, asking only for
the address and reading the OIDC fields from the server (`GET
/.well-known/1device.json`) — this section is what it writes, and the path
to take on a machine you drive from a terminal (development, a headless box). The
Core reads it in its config directory (see
[Where the files live](#where-the-files-live)) and never writes it itself.

```json
{
  "server_url": "wss://your-server.example.com/ws",
  "oidc_issuer": "https://accounts.google.com",
  "oidc_client_id": "…apps.googleusercontent.com",
  "oidc_client_secret": "only-if-your-IdP-demands-one",
  "device_name": "Living-room laptop",
  "relay": "https://your-iroh-relay.example",
  "receive_dir": "/home/you/Downloads",
  "lan_discovery": true
}
```

- `server_url`, `oidc_issuer`, `oidc_client_id`: **required** together (a
  half-filled file is flagged as a problem). `server_url` must be `ws://` or
  `wss://`; `oidc_issuer`, `http(s)://`.
- `oidc_client_secret`: optional. A conformant PKCE IdP has none; Google's
  installed-app clients do, and the working reference setup sets it. Not
  confidential for an installed app — it ships with the client.
- `device_name`: optional (default: the hostname). A plain display label.
- `relay`: optional, **off by default**: no relay at all, unless the device
  belongs to a server whose deployment announces its own relays (the
  announcement fills exactly this default). A self-hosted iroh relay's URL,
  or `"n0"` to opt into the public n0 relays explicitly, wins over any
  announcement. Off the LAN, with no VPN or dialable address between two
  devices, a relay is what lets them meet.
- `receive_dir`: optional — where received files land; without it,
  `<Downloads>/1Device`.
- `lan_discovery`: optional, default `true` — announce this device and resolve
  its siblings over mDNS (UDP 5353) so machines on the same network reach each
  other directly. The broadcast carries the device's public key and addresses,
  nothing else, and trust is unaffected: an unknown machine is refused exactly
  as before. Set `false` on networks where even that is too chatty.

Each of the variables `ONEDEVICE_SERVER_URL`, `ONEDEVICE_OIDC_ISSUER`,
`ONEDEVICE_OIDC_CLIENT_ID`, `ONEDEVICE_OIDC_CLIENT_SECRET`,
`ONEDEVICE_DEVICE_NAME`, `ONEDEVICE_RELAY`,
`ONEDEVICE_RECEIVE_DIR` overrides the file (for development); one that is
defined but empty overrides nothing. **The Core always starts**, even with no config or a broken one:
it logs the issue, and the interface says what is wrong.

### Piece 4 — launch, connect, attach, send

On **each** machine. Steps 1 and 2 are what the installed app does by itself;
from source, run them by hand:

1. **Launch the Core**:
   ```sh
   cargo run --bin 1device-core --locked
   ```
   (or the built executable, `target/debug/1device-core`). It writes an
   `ipc-token` in its config directory, regenerated at every startup: this is
   the root of trust the interface will read. It also spawns the background
   components it finds next to itself — tray, clipboard, context menu.

2. **Launch the interface** (the real binary, not the browser mode):
   ```sh
   cargo run -p onedevice-gui --features webview --locked
   ```
   It connects to the Core via the local socket and the `ipc-token`.

3. **Connect**: the connect button starts the OIDC flow; the system browser
   opens, you authenticate, and the loopback redirect is captured by the Core.
   On first login the device enrolls in the directory.

4. **Attach the device to the account** (a blocking portal after login):
   - on the **first** machine: "This is my first device" → a **recovery code** is
     displayed. It is your way back if you ever lose every device: write it down
     offline. Then "Continue".
   - on the **others**: "I already have a device on this account", and then either
     way in. **Pairing**, which needs nothing typed: on one of the two machines
     press *Show a code*, on the other *Scan a code* (a phone, with its camera) or
     *Enter a code…* (a PC, pasting the line under the QR code) — then check both
     screens show the same six digits and confirm on the machine that is already on
     the account. Or the **recovery code**, typed in, which still works and is what
     a server older than pairing leaves you.
   - either way, the **safety number** shown on the Account screen must be
     **identical** everywhere — compare it visually (a mismatch betrays a wrong
     code, a substitution, or a pairing someone else answered).

   Without this attachment, every send fails *fail-closed*: it is the account
   attestation that authorizes a peer, not its mere presence in the directory.

5. **Send**: once two machines are connected, attested and online, any of the
   three gestures works — the file manager's context menu, a copy/paste through
   the shared clipboard, or the **Devices** screen, where you **drag files
   directly onto the target machine's card**. In the app the target is where you
   drop: dropping outside an eligible card (empty space, offline device, or your
   own machine) does nothing — there is no picker. Receipt is automatic; files
   land in `receive_dir`, and a folder arrives as a folder.

## Build from source

### Prerequisites

Identical to the toolchain pinned by CI
([`.github/workflows/ci.yml`](.github/workflows/ci.yml)):

- **Rust 1.97.0** (exact version; `rustup toolchain install 1.97.0` — or just let
  [`rust-toolchain.toml`](rust-toolchain.toml) do it).
- **Node.js 24** (to build the interface).
- **A C compiler** (native build chain: `gcc`/`clang` on Linux, Xcode Command
  Line Tools on macOS, MSVC Build Tools on Windows) — required by the native
  dependencies (iroh, rustls/`ring`). Present by default on most development
  machines.
- **Linux only** — the webview headers, which Tauri links even without running
  the rendering engine, and the X11 libraries the clipboard backend links:
  ```
  sudo apt-get install -y libwebkit2gtk-4.1-dev libgtk-3-dev \
                          libxcb1-dev libxcb-xfixes0-dev
  ```
  Without `sudo` (WSL, locked-down machine), a build Docker image is provided:
  ```
  docker build -t 1device-build docker/1device-build/
  docker run --rm -v "$PWD":/work -w /work 1device-build cargo build -p onedevice-gui --features webview --locked
  ```

The **Core** builds without the webview — only the GUI binary needs it. (Only
the `1device-core` *library*, the target of the multi-OS cross-check, is
pure Rust with no C compiler; the Core binary itself links iroh and rustls just
like the interface.)

The Android app lives outside the workspace, in [`gui-mobile/`](gui-mobile/), and
needs its own toolchain (Android SDK/NDK, `cargo-tauri`); the `android` job of
`ci.yml` is the reference.

### Build

```sh
git clone https://github.com/iburel/universal-link.git
cd universal-link

# 1. Web interface (produces gui/ui/dist, embedded into the GUI binary)
cd gui/ui
npm ci
npm run build
cd ../..

# 2. The Core, the background components, and the rest of the workspace
#    (without the GUI, which has its own features)
cargo build --workspace --lib --bins --locked

# 3. The real interface binary (system webview)
cargo build -p onedevice-gui --features webview --locked
```

`--locked` fails if `Cargo.lock` is stale instead of silently resolving other
versions — keep it.

### Run the test suite

This is what guarantees everything stays consistent, including the server ↔
Core and Core ↔ interface protocols (exercised end-to-end, in memory):

```sh
# Interface
cd gui/ui && npm run check && npm test && cd ../..

# Rust — exactly what CI runs
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo nextest run --workspace --locked --profile ci
cargo test --workspace --doc --locked   # nextest does not run doctests
```

Two things about that `nextest` line:

- **`--profile ci` is not decoration.** It carries the test groups that serialize
  the tests driving a per-session global — the X `CLIPBOARD` selection, the
  `NSPasteboard`, the Win32 clipboard, a FUSE mount — and the heavy
  cross-reactor IPC tests. Without it, those tests fight each other and fail for
  reasons that have nothing to do with your change. The why is written out in
  [`.config/nextest.toml`](.config/nextest.toml).
- **On Linux the clipboard tests need an X display.** Headless (CI, WSL without
  a desktop): `Xvfb :99 -screen 0 1280x1024x24 &` then `export DISPLAY=:99`.

`cargo test --workspace` still works, but it cannot express those groups.

### Try the interface without installing anything

The fastest way to see all the screens (login, account attachment, devices,
approvals): an in-memory **fake Core**, in a browser, with no daemon or webview.

```sh
cd gui/ui
npm ci        # if not already done
npm run dev   # http://localhost:1420
```

The fake Core ([`gui/ui/src/dev/fake-core.ts`](gui/ui/src/dev/fake-core.ts))
answers the same IPC calls as the real Core: you can "connect", "create an
account" and see the recovery code, "join", list fictitious devices — all
without a network. This branch is dropped from the production bundle.

## Where the files live

Placed by the Core, per user (see [`doc/deployment.md`](doc/deployment.md)):

| | Linux | macOS | Windows |
|---|---|---|---|
| IPC socket / pipe | `$XDG_RUNTIME_DIR/1device/core.sock` | `~/Library/Application Support/1Device/core.sock` | `\\.\pipe\1device-core-<DOMAIN>-<USER>` |
| config directory | `~/.config/1device` | `~/Library/Application Support/1Device` | `%APPDATA%\1Device` |
| logs | `~/.local/state/1device/logs` | `~/Library/Logs/1Device` | `%LOCALAPPDATA%\1Device\logs` |

The config directory holds `config.json` (written by the setup screen or by you),
`ipc-token` (0600, regenerated at every startup), `device.key` (0600, the
device's iroh identity), `account-key.json` (the account's public key + this
device's attestation, *not a secret*, absent until the device has joined the
account), `session.json` (present ⟺ a session is open), and `directory.json`
(the last known list of the account's devices, so a machine that starts without
reaching the server still recognizes its siblings on the local network; every
use re-verifies the attestations, a snapshot older than 7 days is ignored, and
it is deleted at logout). What the OS keyring
holds is the OIDC refresh token and the account's **private** key — kept at rest
so this device can vouch for the next one it links. `secrets.json` (0600,
cleartext secrets at rest) only appears as a **fallback**, on a machine where no
OS keyring is reachable.

On Android everything lives in the app's private storage, and goes with the app
when it is uninstalled.

Log level: `ONEDEVICE_LOG=debug` (not `RUST_LOG`).

## Accepted limitations (v1)

- **Unsigned desktop builds** (milestone 1): a first-launch OS warning, and no
  Windows 11 main-menu integration or Finder extension, both of which require a
  signed artifact registered at install time.
- **The server is yours to host**: nobody runs it for you. The published
  Docker image and the Caddy auto-TLS stack reduce hosting to a compose file
  and four variables (cf. [deployment](doc/server-deployment.md)), but a
  machine and a domain remain yours to bring.
- **Outbound drag-and-drop** is absent (only inbound works).
- **The phone shares, it does not receive**, and aggressive power management can
  still end the app — on the test device, swiping it out of Recents kills the
  process even with the foreground service running.
- **Windows session end**: a Core started by the graphical autostart has no
  console, so it gets none of the shutdown events and is terminated at logout
  instead of stopping cleanly. Its components die with it (they hang off a Job
  object), and anything they left in the shell is swept at the next startup — so
  the residual is a missing goodbye, not a leak. The fix is a message-only
  window (`WM_QUERYENDSESSION`) or a real service.
- **Account key rotation** is not implemented (v1 refuses to replace an existing
  key). It matters more than it used to: every device keeps the account key in its
  keyring, which is what lets it link the next one, so a device whose storage is
  read hands over the account key — and rotating it is the answer to that. Until it
  exists, the manual equivalent is to erase the key on **every** device and start
  over from a fresh recovery code.

## Documentation

- [`doc/architecture.md`](doc/architecture.md) — overview and decisions.
- [`doc/core-api.md`](doc/core-api.md) — the Core's local IPC API (the project's
  extension point).
- [`doc/server-api.md`](doc/server-api.md) — the server API.
- [`doc/identity-providers.md`](doc/identity-providers.md): the OIDC
  identity provider. The Google walkthrough, recipes for self-hosted issuers
  (Keycloak, Authentik, Zitadel, Pocket ID, Kanidm, Dex), and the contract
  any other must meet.
- [`doc/server-deployment.md`](doc/server-deployment.md) — hosting the server
  (Docker, Caddy, Google OIDC client).
- [`doc/first-link.md`](doc/first-link.md) — bringing up a link end to end (two
  machines, real Google login, transfer) and its troubleshooting.
- [`doc/deployment.md`](doc/deployment.md) — the Core running locally.
- [`doc/beyond-the-lan.md`](doc/beyond-the-lan.md): serverless accounts past
  the local network. The self-hosting ladder, and the recipes (WireGuard,
  Tailscale, a self-hosted relay).
- [`CHANGELOG.md`](CHANGELOG.md) — what each release added.

## License

1Device is licensed under the **GNU Affero General Public License v3.0
only** (AGPL-3.0-only). See [LICENSE](LICENSE) for the full text, and
[CONTRIBUTING.md](CONTRIBUTING.md) for how to contribute (including the DCO
sign-off).
