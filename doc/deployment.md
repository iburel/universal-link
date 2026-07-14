# Local deployment of the Core

This document describes the Core process as it installs and runs on a user's
machine: where it listens, what it reads, what it writes, and what it expects from
the components it launches.

The binary is called `universallink-core` and lives in the `daemon/` crate.

## One process per user

The Core is a **session** daemon, not a system service: it runs under the user's
account, with their rights, and has no business in a session that is not its own.

Only one can run at a time. On unix, the first takes a non-blocking `flock` on
`<socket>.lock`; on Windows, it creates the first instance of the named pipe
(`FILE_FLAG_FIRST_PIPE_INSTANCE`). The second **exits with code 0** after saying
so in its log: an autostart that races after a manual launch is not a failure. It
gives up **before** having touched `ipc-token` — otherwise it would revoke, out
from under it, the secret of the live Core.

After a `kill -9`, there is nothing to clean up: the kernel releases the lock, and
the next Core removes the stale socket itself.

## Paths

| | Linux | macOS | Windows |
|---|---|---|---|
| listening endpoint | `$XDG_RUNTIME_DIR/universallink/core.sock` | `~/Library/Application Support/UniversalLink/core.sock` | `\\.\pipe\universallink-core-<USERDOMAIN>-<USERNAME>` |
| config folder | `$XDG_CONFIG_HOME` (default `~/.config`) `/universallink` | `~/Library/Application Support/UniversalLink` | `%APPDATA%\UniversalLink` |
| log | `$XDG_STATE_HOME` (default `~/.local/state`) `/universallink/logs` | `~/Library/Logs/UniversalLink` | `%LOCALAPPDATA%\UniversalLink\logs` |

The Windows pipe name carries the domain **and** the user name: a local account
`john` and a domain account `CORP\john` are two distinct users with the same
`USERNAME`.

The config folder houses:

- `config.json` — written by the GUI's setup screen (or by hand), never by the
  Core (see below);
- `ipc-token` (0600) — the GUI's root of trust, **regenerated at every startup**;
- `device.key` (0600) — the device's Ed25519 seed, generated at first startup.
  This is the iroh identity, and it precedes the login;
- `account-key.json` — the account's root of trust (C7): the account's PUBLIC key
  + this device's attestation. **Not secret** (no private key: the account key is
  reconstituted from the recovery code, with the user). Absent until the device
  has joined the account (`account.setup`/`join`);
- `session.json` — present ⟺ a session is open;
- `secrets.json` (0600) — fallback when no keyring is reachable.

## Configuration

`config.json`, in the config folder:

```json
{
  "server_url": "wss://relais.example/ws",
  "oidc_issuer": "https://accounts.google.com",
  "oidc_client_id": "…apps.googleusercontent.com",
  "oidc_client_secret": "only-if-your-idp-requires-one",
  "device_name": "Living-room laptop",
  "relay_url": "https://relais-iroh.example",
  "receive_dir": "/home/iwan/Received"
}
```

**Nothing is baked into the binary**: a fresh install carries no server. The GUI's
first-run setup screen collects `server_url` / `oidc_issuer` / `oidc_client_id`
(+ the optional secret), writes this file, and calls `session.reload` so the Core
applies it live — no restart. `config.json` can also be written by hand.

`oidc_client_secret` is optional: a conformant PKCE IdP has none, but Google
requires it at the token exchange even under PKCE (it is not confidential for a
"Desktop app" OAuth client — it ships with the app). `device_name` is optional —
without it, the hostname. It is only a display label: the device's identity is
its public key.

`relay_url` is optional: the deployment's iroh relay, for whoever also self-hosts
their relay ([`iroh-relay`]) — without it, the n0 public relays. A sovereign
deployment's data plane then depends on no third-party infrastructure. Checked at
startup like the rest: a typo is a `problem`, not a silently mute data plane.

[`iroh-relay`]: https://github.com/n0-computer/iroh

`receive_dir` is optional: where received files land (`files.send` from another
device). Without it, `<Downloads>/UniversalLink` (`$XDG_DOWNLOAD_DIR` or
`~/Downloads` on Linux, `~/Downloads` on macOS, `%USERPROFILE%\Downloads` on
Windows); and if the environment does not even allow determining it,
`<config folder>/received` — the Core always receives. Each file is written via a
temporary renamed at the end; a name collision is suffixed "(n)", never an
overwrite.

The variables `UNIVERSALLINK_SERVER_URL`, `UNIVERSALLINK_OIDC_ISSUER`,
`UNIVERSALLINK_OIDC_CLIENT_ID`, `UNIVERSALLINK_DEVICE_NAME`,
`UNIVERSALLINK_RELAY_URL`, and `UNIVERSALLINK_RECEIVE_DIR` override the file — a
variable that is defined but empty overrides nothing. Completeness is checked
**after** the merge: a partial file that the environment completes is valid.

**The Core always starts**, even without configuration or with a faulty
configuration: it logs it and runs logged out (`session.login` answers
`SERVER_UNREACHABLE`). Refusing to start would leave the GUI stuck on a "Connecting
to the Core…" forever, without ever being able to say why.

## Log

Daily rotation, seven files kept. The level is set by `UNIVERSALLINK_LOG` (and not
`RUST_LOG`, too widely shared): `UNIVERSALLINK_LOG=debug`. The error output is
mirrored only if it is attached to a terminal — a Core launched at login has no one
to talk to.

## Secrets

The OIDC refresh token goes to the OS keyring: Secret Service (Linux), Keychain
(macOS), Credential Manager (Windows). If none responds — SSH session, machine with
no agent, CI — the Core falls back to `secrets.json` in 0600, and says so in its
log.

Keyring accesses go through a dedicated thread: the Core writes its secrets while
holding the session lock, and a Keychain that opens a confirmation window would
otherwise freeze all the IPC commands. Writes are queued and return immediately;
reads wait, but no more than three seconds — beyond that, "secret absent", and the
flow that had produced it is redone.

## TLS

The Core speaks `wss://` to the server and `https://` to the IdP via rustls
(provider `ring`), with the system's trust roots: on Windows and macOS it is the
OS verifier, so an enterprise root or a root added by the user is honored; on
Linux, it is the certificates of the CA bundle (`SSL_CERT_FILE` / `SSL_CERT_DIR`
included).

`ws://` and `http://` remain possible — this is how you develop against a local
server. The URL scheme decides, and nothing else: a `wss://` URL is never served in
cleartext.

## Supervised components

The Core launches the official components installed next to its binary, restarts
them when they fall (capped exponential backoff, reset once the child has stood up),
and takes them with it when it stops. A missing component is ignored: a Core without
a tray is still a Core that works.

The contract of a supervised component:

1. It finds the Core at the path passed in `UNIVERSALLINK_IPC_PATH`.
2. It reads its **spawn token** on the first line of its standard input. Not
   `argv` (readable by all), nor the environment (inherited by all its
   descendants).
3. Its standard input stays open. **Its EOF means "stop".** It is the only graceful
   shutdown channel that exists on all three OSes — Windows has no SIGTERM.
4. **If it loses its IPC connection, it must exit.** The spawn token is single-use:
   it will not be able to reconnect with it. The supervisor will relaunch it with a
   fresh token. A component that looped on reconnections doomed to fail would be a
   live and useless process, which a process supervisor would not be able to detect.

A component's descendants die with it: process group on unix (the supervisor
signals `-pgid`), a `KILL_ON_JOB_CLOSE` Job Object on Windows. A contextual-menu
backend launches shims; leaving them behind would be a process leak, and an OS
integration answering into the void.

## Shutdown

`SIGINT`, `SIGTERM`, or `SIGHUP` on unix; `Ctrl-C`, console close, shutdown, or
logoff on Windows. `SIGHUP` means shutdown and not reload: there is nothing to
reload, and the default behavior (dying without warning) would abandon the
components behind us.

The order is imposed: we stop restarting, we stop and **reap** the children while
the tokio runtime is still alive, then the Core closes its IPC connections, then the
instance lock is released. A second signal during shutdown exits immediately.

## Accepted limitations (v1)

- **No autostart.** It will come with the packaging, which will give the binary a
  stable install path.
- **Windows without a console.** A Core started by a graphical autostart receives
  none of the events above: it will have to be given a message-only window
  (`WM_QUERYENDSESSION`) or turned into a real service. Today it is launched from a
  terminal.
- **No official component exists yet**, so the supervisor's table is empty in
  production. It is exercised end-to-end by the daemon's test suite, with a dummy
  component that genuinely speaks the protocol.
- **The keyring choice is frozen at startup**: if the secrets agent comes up after
  the Core, we stay on the file fallback until the next launch.
