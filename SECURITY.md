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
(server, Core daemon, data plane, GUI, tray, clipboard backend, contextual-menu
manager, Android app), version or commit, environment, and a proof of concept if
you have one.

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
- **A shell integration carries no credential.** The contextual menu's entries are
  files and registry values the user can rewrite, and the shell starts their
  command line with an influenceable `argv`. What it starts is a courier that
  holds no Core token: it only reaches its own manager over a private local
  channel. A path from a writable menu artifact to a Core capability is a finding.
- **Confidentiality markers are honored end to end.** A copy the OS marks
  confidential (a password manager's) is announced without a size hint, is never
  pushed to the account's devices ahead of a paste, and is re-marked on the
  machine it is pasted on so that clipboard history and cloud sync skip it.

## Supported versions

UniversalLink is pre-1.0 and under active development. Only the latest release
and `main` receive security fixes.
