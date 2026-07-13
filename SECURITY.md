# Security Policy

UniversalLink links a user's own devices and transfers their files, so we take
security reports seriously. Thank you for helping keep it safe.

## Reporting a vulnerability

**Please do not report security vulnerabilities through public GitHub issues,
discussions, or pull requests.**

Instead, use one of the following private channels:

- **GitHub private vulnerability reporting** (preferred): open the repository's
  **Security** tab and click **Report a vulnerability**.
- **Email**: `iwan.burel@gmail.com`.

Please include enough detail to reproduce the issue: affected component
(server, Core daemon, GUI, data plane), version or commit, environment, and a
proof of concept if you have one.

We will acknowledge your report as soon as we can, keep you informed while we
investigate, and credit you in the release notes once a fix ships — unless you
prefer to remain anonymous.

## Scope

UniversalLink's security model rests on a few load-bearing properties; reports
that undermine any of them are especially valuable:

- **End-to-end encryption of the data plane.** The server relays control and
  signaling only and must never be able to read transferred data.
- **The server is not trusted to decide account membership.** Devices are
  attested by an account key derived from a recovery code that the server never
  learns; a peer refuses any device whose attestation does not verify
  (*fail-closed*).
- **Local IPC trust.** The Core exposes a local JSON-RPC API guarded by a
  per-startup token; only authorized local components should reach it.

## Supported versions

UniversalLink is pre-1.0 and under active development. Only the latest release
and `main` receive security fixes.
