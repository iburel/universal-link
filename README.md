# UniversalLink

[![License: AGPL v3](https://img.shields.io/badge/License-AGPL_v3-blue.svg)](LICENSE)

Link the PCs (Windows, macOS, Linux) of a single user to transfer files and
content between them, **end-to-end encrypted** — the server never sees the data,
and does not even decide on its own who belongs to the account.

Written in **Rust** (a Cargo workspace) with a **Tauri / Svelte** interface.

> ### Status: milestone 1, pre-packaging
>
> The foundation is built and green in CI on all three OSes, but the project
> **is not yet software you install and that just works.** In particular:
>
> - **The server deploys behind automatic TLS.** A Docker image and a Caddy
>   stack are provided in [`deploy/`](deploy/); the image builds, starts,
>   **persists its directory to disk** (enrollments survive a restart), and the
>   certificate is obtained automatically. **What's left**: the first real
>   bring-up (Google login, two real machines) and a published image — see
>   [Deploy the server](doc/server-deployment.md).
> - **No packaging, no autostart, no installer** on the Core/GUI side. The Core
>   is launched from a terminal.
> - **No official background component exists yet** (no tray, no clipboard
>   manager, no context menu). The GUI is the only usable component today.
> - **Inbound** drag-and-drop works (drop a file onto a device's card → send);
>   **outbound** drag (from the app to the desktop) is not there yet.
>
> In other words: a **developer** can build everything, test everything,
> explore the interface, and make the pieces talk to each other. Actually
> linking two PCs additionally requires providing the three missing pieces
> below (OIDC client, server, TLS).

## What works today

| Capability | State |
|---|---|
| Build and test the whole workspace (3 OSes) | ✅ green in CI |
| Explore the UI without installing anything (fake Core, browser) | ✅ `npm run dev` |
| Core: startup, local IPC, config, logging, OS keyring, clean shutdown | ✅ implemented |
| OIDC login (authorization code + PKCE, browser → loopback) | ✅ implemented |
| Device enrollment and directory (`devices.list` / `rename` / `revoke`) | ✅ implemented |
| Account attachment (create / join via recovery code) | ✅ implemented |
| File send (drop onto a device's card) + automatic receipt | ✅ implemented |
| iroh data plane (E2E-encrypted QUIC, NAT traversal, relays) | ✅ implemented |
| Server deployment (Docker image + Caddy auto-TLS, env, persisted directory) | 🟡 deployable; real bring-up = next milestone |
| **Packaging / autostart / installers** | ❌ upcoming |
| **Tray, shared clipboard, context menu** | ❌ upcoming |
| **Outbound drag-and-drop** | ❌ upcoming |

The design details (and what is deliberately deferred) live in
[`doc/`](doc/): [architecture](doc/architecture.md),
[Core API](doc/core-api.md), [server API](doc/server-api.md),
[server deployment](doc/server-deployment.md),
[first link](doc/first-link.md),
[Core deployment](doc/deployment.md).

## Architecture at a glance

```
                      ┌────────────┐
                      │   Server   │  OIDC · directory · presence · signaling
                      └─────┬──────┘  (CONTROL plane — blind to the data)
          ┌─────────────────┼─────────────────┐
     ┌────┴────┐       ┌────┴────┐       ┌────┴────┐
     │  PC A   │◄─────►│  PC B   │       │  PC C   │
     │ (Core)  │ iroh  │ (Core)  │       │ (Core)  │
     └─────────┘ P2P   └─────────┘       └─────────┘
              (direct, else relayed — data end-to-end encrypted)
```

On each PC, a **Core** (session daemon) holds the server session, the device
identity (its iroh key) and transfers, and exposes a **local IPC API**
(JSON-RPC 2.0 over a Unix socket / named pipe) to components — including the
GUI. The server is removed from the trust decision about *who belongs to the
account*: an **account key** derived from a recovery code (never known to the
server) attests each device, and a peer refuses any device whose attestation
does not verify (*fail-closed*).

## Prerequisites

Identical to the toolchain pinned by CI
([`.github/workflows/ci.yml`](.github/workflows/ci.yml)):

- **Rust 1.97.0** (exact version; `rustup toolchain install 1.97.0`).
- **Node.js 24** (to build the interface).
- **A C compiler** (native build chain: `gcc`/`clang` on Linux, Xcode Command
  Line Tools on macOS, MSVC Build Tools on Windows) — required by the native
  dependencies (iroh, rustls/`ring`). Present by default on most development
  machines.
- **Linux only** — the webview headers, which Tauri links even without running
  the rendering engine:
  ```
  sudo apt-get install -y libwebkit2gtk-4.1-dev libgtk-3-dev
  ```
  Without `sudo` (WSL, locked-down machine), a build Docker image is provided:
  ```
  docker build -t ul-build docker/ul-build/
  docker run --rm -v "$PWD":/work -w /work ul-build cargo build -p universallink-gui --features webview --locked
  ```

The **Core** builds without the webview — only the GUI binary needs it. (Only
the `universallink-core` *library*, the target of the multi-OS cross-check, is
pure Rust with no C compiler; the Core binary itself links iroh and rustls just
like the interface.)

## Build from source

```sh
git clone https://github.com/iburel/UniversalLink.git
cd UniversalLink

# 1. Web interface (produces gui/ui/dist, embedded into the GUI binary)
cd gui/ui
npm ci
npm run build
cd ../..

# 2. The Core and the rest of the workspace (without the GUI, which has its
#    own features)
cargo build --workspace --lib --bins --locked

# 3. The real interface binary (system webview)
cargo build -p universallink-gui --features webview --locked
```

`--locked` fails if `Cargo.lock` is stale instead of silently resolving other
versions — keep it.

## Run the test suite

This is what guarantees everything stays consistent, including the server ↔
Core and Core ↔ interface protocols (exercised end-to-end, in memory):

```sh
# Interface
cd gui/ui && npm run check && npm test && cd ../..

# Rust (capped parallelism: some tests depend on timing windows)
cargo test --workspace --locked -- --test-threads=2
```

## Try the interface without installing anything

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

## Set up a real link between two PCs

This is where the "milestone 1" status shows. The happy-path code exists and is
tested, but **three pieces must be provided by you** before two PCs actually see
each other. None of them is turnkey yet.

### Piece 1 — an OIDC client

The server authenticates accounts via **OIDC**; the reference issuer is
**Google** (`accounts.google.com`). You need a **public** OIDC client (PKCE, no
*client secret*), its `client_id`, and the issuer URL.

> ⚠️ On Google, create a client of type **"Desktop app"**, **not "Web
> application"**: the latter requires a `client_secret` that the Core does not
> send (login would fail at the code exchange). Step by step:
> [Deploy the server, step 1](doc/server-deployment.md#step-1--register-a-google-oidc-client).

### Piece 2 — a running server

The `universallink-server` binary (crate `server-daemon`) is configured through
the environment and starts the control plane (WebSocket `/ws`, `GET /health`):

```sh
UNIVERSALLINK_SERVER_BIND=0.0.0.0:8080 \
UNIVERSALLINK_OIDC_ISSUER=https://accounts.google.com \
UNIVERSALLINK_OIDC_CLIENT_ID=…apps.googleusercontent.com \
cargo run --bin universallink-server --locked
```

Optional settings (with their defaults): `UNIVERSALLINK_SERVER_STATE`
(`universallink-directory.json` — the directory file, to point at a volume in a
deployment), `UNIVERSALLINK_HEARTBEAT_SECS` (30),
`UNIVERSALLINK_HEARTBEAT_MAX_MISSED` (2), `UNIVERSALLINK_NONCE_TTL_SECS` (60),
`UNIVERSALLINK_FRESH_TOKEN_MAX_AGE_SECS` (300),
`UNIVERSALLINK_MAX_REQUESTS_PER_MINUTE` (120; `0` = unlimited); log level via
`UNIVERSALLINK_LOG`. On an incomplete or invalid config, the server **refuses to
start** and logs every error at once.

The directory (device identities, account attestations, revocations) is
**persisted to disk**: enrollments survive a restart.

For a **real deployment** — automatic TLS (the server listens in cleartext, a
reverse proxy terminates TLS; the Core requires `wss://`), Docker image and
Caddy stack ready to use — follow
**[Deploy the server](doc/server-deployment.md)**. In short:

```sh
cd deploy
cp .env.example .env      # domain + OIDC issuer + client_id
docker compose up -d --build
```

### Piece 3 — `config.json` on each PC

The Core reads a `config.json` in its config directory (see
[Where the files live](#where-the-files-live)). It never writes it itself.

```json
{
  "server_url": "wss://your-server.example.com/ws",
  "oidc_issuer": "https://accounts.google.com",
  "oidc_client_id": "…apps.googleusercontent.com",
  "device_name": "Living-room laptop",
  "relay_url": "https://your-iroh-relay.example",
  "receive_dir": "/home/you/Downloads"
}
```

- `server_url`, `oidc_issuer`, `oidc_client_id`: **required** together (a
  half-filled file is flagged as a problem). `server_url` must be `ws://` or
  `wss://`; `oidc_issuer`, `http(s)://`.
- `device_name`: optional (default: the hostname). A plain display label.
- `relay_url`: optional — a self-hosted iroh relay; without it, the public n0
  relays are used.
- `receive_dir`: optional — where received files land; without it,
  `<Downloads>/UniversalLink`.

No secret in this file (the OIDC client is public). Each of the variables
`UNIVERSALLINK_SERVER_URL`, `UNIVERSALLINK_OIDC_ISSUER`,
`UNIVERSALLINK_OIDC_CLIENT_ID`, `UNIVERSALLINK_DEVICE_NAME`,
`UNIVERSALLINK_RELAY_URL`, `UNIVERSALLINK_RECEIVE_DIR` overrides the file (for
development). **The Core always starts**, even with no config or a broken one:
it logs the issue, and the interface says what is wrong.

### Piece 4 — launch, connect, attach, send

On **each** PC:

1. **Launch the Core** from a terminal:
   ```sh
   cargo run --bin universallink-core --locked
   ```
   (or the built executable, `target/debug/universallink-core`). It writes an
   `ipc-token` in its config directory, regenerated at every startup: this is
   the root of trust the interface will read.

2. **Launch the interface** (the real binary, not the browser mode):
   ```sh
   cargo run -p universallink-gui --features webview --locked
   ```
   It connects to the Core via the local socket and the `ipc-token`.

3. **Connect**: the connect button starts the OIDC flow; the system browser
   opens, you authenticate, and the loopback redirect is captured by the Core.
   On first login the device enrolls in the directory.

4. **Attach the device to the account** (a blocking portal after login):
   - on the **first** PC: "This is my first device" → a **recovery code** is
     displayed. This is the **only copy** of the account's private key: write it
     down offline. Then "Continue".
   - on the **others**: "I already have a device on this account" → enter that
     same code. The **safety number** shown on the Account screen must be
     **identical** on all your PCs — compare it visually (a mismatch betrays a
     wrong code or a substitution).

   Without this attachment, every send fails *fail-closed*: it is the account
   attestation that authorizes a peer, not its mere presence in the directory.

5. **Send**: once two PCs are connected, attested, and online, open the
   **Devices** screen and **drag files directly onto the target device's card**
   (which must be online). The target is determined by where you drop: dropping
   outside an eligible card (empty space, offline device, or your own PC) does
   nothing — there is no picker. Receipt is automatic (v1: these are your own
   devices); files land in `receive_dir`. **v1: flat files** (no directory
   trees).

## Where the files live

Placed by the Core, per user (see [`doc/deployment.md`](doc/deployment.md)):

| | Linux | macOS | Windows |
|---|---|---|---|
| IPC socket / pipe | `$XDG_RUNTIME_DIR/universallink/core.sock` | `~/Library/Application Support/UniversalLink/core.sock` | `\\.\pipe\universallink-core-<DOMAIN>-<USER>` |
| config directory | `~/.config/universallink` | `~/Library/Application Support/UniversalLink` | `%APPDATA%\UniversalLink` |
| logs | `~/.local/state/universallink/logs` | `~/Library/Logs/UniversalLink` | `%LOCALAPPDATA%\UniversalLink\logs` |

The config directory holds `config.json` (written by you), `ipc-token` (0600,
regenerated at every startup), `device.key` (0600, the device's iroh identity),
`account-key.json` (the account's public key + attestation, *not a secret*,
absent until the device has joined the account), and `session.json` (present ⟺
a session is open). `secrets.json` (0600, cleartext secret at rest) only appears
as a **fallback**, on a machine where no OS keyring is reachable.

Log level: `UNIVERSALLINK_LOG=debug` (not `RUST_LOG`).

## Accepted limitations (v1)

- **Server: deployable behind auto-TLS** (Docker image + Caddy, cf.
  [deployment](doc/server-deployment.md)), but the **image is not published**
  and the **real bring-up is not yet validated** (see the status at the top).
- **No background component**: no shared clipboard, no "send to PC X" context
  menu, no tray icon. The interface is the only component.
- **Outbound drag-and-drop** is absent (only inbound works).
- **Flat transfers**: every path must be a regular file (directory trees are a
  tracked follow-up).
- **Windows without a console**: a Core launched by a graphical autostart would
  not receive shutdown events — for now, it is launched from a terminal.
- **Account key rotation** is not implemented (v1 refuses to replace an existing
  key).

## Documentation

- [`doc/architecture.md`](doc/architecture.md) — overview and decisions.
- [`doc/core-api.md`](doc/core-api.md) — the Core's local IPC API (the project's
  extension point).
- [`doc/server-api.md`](doc/server-api.md) — the server API.
- [`doc/server-deployment.md`](doc/server-deployment.md) — hosting the server
  (Docker, Caddy, Google OIDC client).
- [`doc/first-link.md`](doc/first-link.md) — the first end-to-end bring-up (two
  machines, real Google login, transfer) and its troubleshooting.
- [`doc/deployment.md`](doc/deployment.md) — the Core running locally.

## License

UniversalLink is licensed under the **GNU Affero General Public License v3.0
only** (AGPL-3.0-only). See [LICENSE](LICENSE) for the full text, and
[CONTRIBUTING.md](CONTRIBUTING.md) for how to contribute (including the DCO
sign-off).
