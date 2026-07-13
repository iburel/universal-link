# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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

[0.1.0]: https://github.com/iburel/UniversalLink/releases/tag/v0.1.0
