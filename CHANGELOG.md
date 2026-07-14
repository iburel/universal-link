# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0] - 2026-07-14

Milestone 1 packaging: the client now installs and configures itself. A fresh
install ships blank and is set up from the app — nothing is baked into the
binary.

### Added

- **Installers** — unsigned, per-user, no admin rights: macOS `.dmg` (Apple
  Silicon), Windows NSIS `.exe`, and Linux `.AppImage`, built and published by
  CI on a `v*` tag.
- **First-run setup** — a screen that collects the server address and the
  OpenID Connect client, writes `config.json`, and applies it live through the
  new `session.reload` (no restart). A Server settings tab changes it later.
- **Autostart** — the GUI installs the background Core to start at each login,
  per user: macOS LaunchAgent, Windows `HKCU\…\Run`, Linux XDG autostart. On
  Linux the Core is copied to a stable path so autostart survives an AppImage's
  ephemeral mount.

### Changed

- **Nothing is baked into the released binaries** — no server URL, OIDC client,
  or secret. The deployment is entered on the first-run screen and read at
  runtime from `config.json` / `UNIVERSALLINK_*`. `session.status` reports a
  `configured` flag so the app tells "not set up yet" apart from "server
  unreachable"; an invalid configuration is rejected with `INVALID_CONFIG`.
- Updated dependencies (`sha2`, `tokio-tungstenite`, and CI actions).

### Fixed

- Flaky cross-Core file-transfer tests (a receiver-side attestation race).

## [0.1.0] - 2026-07-13

First public release. Milestone 1: the foundation is built and green in CI on
Linux, macOS, and Windows.

### Added

- **Core daemon** — session lifecycle, local IPC (JSON-RPC 2.0 over a Unix
  socket / Windows named pipe) guarded by a per-startup token, configuration,
  logging, OS keyring integration, and clean shutdown.
- **OIDC login** — authorization code + PKCE via the system browser, with a
  loopback redirect captured by the Core.
- **Device enrollment and directory** — `devices.list` / `rename` / `revoke`.
- **Account key** — create or join an account with a recovery code; devices are
  attested by a key the server never learns (fail-closed peer authorization).
- **File transfer** — drag a file onto a device card to send; automatic receipt
  on the peer.
- **iroh data plane** — end-to-end encrypted QUIC with NAT traversal and relay
  fallback.
- **Server** — directory / signaling service (OIDC auth, presence, persisted
  directory) deployable behind automatic TLS (Docker image + Caddy stack).
- **Tauri + Svelte GUI** — the first usable component; also runnable against an
  in-memory fake Core in the browser for development.

### Known limitations

- No packaging, autostart, or installers for the Core/GUI yet.
- No background components yet (no tray, shared clipboard, or context menu).
- Outbound drag-and-drop (from the app to the desktop) is not implemented.
- Flat transfers only (no directory trees).
- Account key rotation is not implemented.

[0.2.0]: https://github.com/iburel/universal-link/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/iburel/universal-link/releases/tag/v0.1.0
