# Local deployment of the Core

This document describes the Core process as it installs and runs on a user's
machine: where it listens, what it reads, what it writes, and what it expects from
the components it launches.

The binary is called `1device-core` and lives in the `daemon/` crate.

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
| listening endpoint | `$XDG_RUNTIME_DIR/1device/core.sock` | `~/Library/Application Support/1Device/core.sock` | `\\.\pipe\1device-core-<USERDOMAIN>-<USERNAME>` |
| config folder | `$XDG_CONFIG_HOME` (default `~/.config`) `/1device` | `~/Library/Application Support/1Device` | `%APPDATA%\1Device` |
| log | `$XDG_STATE_HOME` (default `~/.local/state`) `/1device/logs` | `~/Library/Logs/1Device` | `%LOCALAPPDATA%\1Device\logs` |

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
  + this device's attestation. **Not secret** — no private key in this file.
  Absent until the device has joined the account (`account.setup`/`join`);
- `session.json` — present ⟺ a session is open;
- `directory.json` — the account's device records, so a Core that starts with no
  reachable server still recognizes its siblings. A cache where a server could
  refresh it (7-day staleness bound); with no server configured it IS the
  directory. What the devices teach each other over the LAN lands here too, and
  does **not** reset that bound — only the server it is a bound against can.
  Removed at logout;
- `revoked.json` — the devices the account has struck off, each with the account
  key's signature over that revocation. **Permanent**, and NOT removed at logout:
  a struck-off device keeps a valid attestation for good, so this file is the only
  thing that keeps it out. Deleting it takes those devices back into the account;
- `secrets.json` (0600) — fallback when no keyring is reachable. What the keyring
  holds: the OIDC refresh token, and the account's **private** key
  (`account-key-seed`) — kept at rest so this device can vouch for a joining one
  ([architecture.md](architecture.md), principle 3).

## Configuration

`config.json`, in the config folder:

```json
{
  "server_url": "wss://relais.example/ws",
  "oidc_issuer": "https://accounts.google.com",
  "oidc_client_id": "…apps.googleusercontent.com",
  "oidc_client_secret": "only-if-your-idp-requires-one",
  "device_name": "Living-room laptop",
  "relay": "https://relais-iroh.example",
  "receive_dir": "/home/iwan/Received"
}
```

**Nothing is baked into the binary**: a fresh install carries no server. The GUI's
first-run setup screen asks for the **address** and reads the rest from the server
itself (`session.discover` → its deployment descriptor, see
[core-api.md](core-api.md#sessiondiscover-one-address-instead-of-three-fields)):
`oidc_issuer` / `oidc_client_id` (+ the optional secret) describe the deployment,
not the user. It writes this file, then calls `session.reload` so the Core applies
it live — no restart. A server that publishes no descriptor (older than that
endpoint) makes the screen fall back to asking for the three fields, and
`config.json` can always be written by hand.

`oidc_client_secret` is optional: a conformant PKCE IdP has none, but Google
requires it at the token exchange even under PKCE (it is not confidential for a
"Desktop app" OAuth client — it ships with the app). `device_name` is optional —
without it, the hostname. It is only a display label: the device's identity is
its public key.

`relay` is optional and three-valued, **off by default**: an unconfigured device
talks only to infrastructure somebody chose. Set it to the URL of the
deployment's iroh relay ([`iroh-relay`]) for whoever also self-hosts their
relay, or to `"n0"` to opt into the n0 public relays explicitly (exactly the
old unconfigured behavior, now a choice). A sovereign deployment's data plane
depends on no third-party infrastructure. Checked at startup like the rest: a
typo is a `problem`, not a silently mute data plane; so is the pre-rename
`relay_url` key, refused with its cure rather than silently downgrading an
explicit choice to off. For a **serverless** account the same setting is the
third rung of the self-hosting ladder: the device signs its chosen relay into
its directory record (`relay_hint`; under `"n0"`, the home relay that opt-in
elects), and its already-paired siblings dial it through that relay from
anywhere. A device that belongs to a server account also hears its
deployment's relay announcement ([server-api.md](server-api.md#deployment-descriptor)):
the announcement fills the off default, and an explicit local relay here (a
URL or `"n0"`) wins over it; `"off"` IS the default, written out or not, so
it is exactly what the announcement fills. Never a relay nobody chose: with
no announcement, a device whose relay is off signs none and none of your
devices is dialed through one; under an announcement, the elected relay is
one your operator chose and you accepted by joining their server. What off
costs, honestly: two
devices behind two distinct NATs, off the LAN, with no VPN between them, need
a relay to meet (hole punching needs the rendezvous). The whole off-LAN story,
VPN recipes included, is [beyond-the-lan.md](beyond-the-lan.md).

[`iroh-relay`]: https://github.com/n0-computer/iroh

`receive_dir` is optional: where received files land (`files.send` from another
device). Without it, `<Downloads>/1Device` (`$XDG_DOWNLOAD_DIR` or
`~/Downloads` on Linux, `~/Downloads` on macOS, `%USERPROFILE%\Downloads` on
Windows); and if the environment does not even allow determining it,
`<config folder>/received` — the Core always receives. Each file is written via a
temporary renamed at the end; a name collision is suffixed "(n)", never an
overwrite.

The variables `ONEDEVICE_SERVER_URL`, `ONEDEVICE_OIDC_ISSUER`,
`ONEDEVICE_OIDC_CLIENT_ID`, `ONEDEVICE_DEVICE_NAME`,
`ONEDEVICE_RELAY`, and `ONEDEVICE_RECEIVE_DIR` override the file; a
variable that is defined but empty overrides nothing. Completeness is checked
**after** the merge: a partial file that the environment completes is valid.

**The Core always starts**, even without configuration or with a faulty
configuration. An unreadable or half-filled file leaves it unconfigured
(`session.login` answers `SERVER_UNREACHABLE`); a faulty single setting is
simply not applied and the rest of the config runs. Either way the reason,
cure included, reaches the app as a banner (`session.status` `problem`), not
just the log. Refusing to start would leave the GUI stuck on a "Connecting
to the Core…" forever, without ever being able to say why.

## Log

Daily rotation, seven files kept. The level is set by `ONEDEVICE_LOG` (and not
`RUST_LOG`, too widely shared): `ONEDEVICE_LOG=debug`. The error output is
mirrored only if it is attached to a terminal — a Core launched at login has no one
to talk to.

## macOS's Local Network permission

macOS asks each build of the app for *Local Network* access the first time it
touches the LAN, and quietly refuses every multicast packet until someone
answers: each send fails with `No route to host`, so LAN discovery is deaf and
mute while the rest of the data plane (server, relay) works normally. The Core
probes the wire when it starts with LAN discovery on and, if nothing comes
back, writes one log line naming the cure — System Settings → Privacy &
Security → Local Network → 1Device.

The grant is tied to the binary's code-signing identity. Two consequences for
unsigned (ad-hoc) builds: every update is a fresh identity and asks again, and
a binary swapped inside the bundle by hand silently loses the grant —
reinstall the bundle properly instead. And never overwrite a Mach-O in place:
the kernel kills a binary whose file was rewritten under a cached signature
(`last exit reason: OS_REASON_CODESIGNING` in `launchctl print`).

## A VPN takes the app off its own network

A device-wide VPN routes a covered app's traffic — inbound and outbound —
through the tunnel, and multicast does not survive a tunnel: the app can
neither announce on the local network nor hear anyone announcing, even though
everything the server and the relay carry keeps working. On Android this
confinement is per-app and absolute (the system routes by UID; pinning a
socket to the Wi-Fi interface changes nothing, and with a non-bypassable VPN
the app cannot opt out programmatically). Measured on a real device under
WireGuard: the system's own mDNS still reaches the wire, the app's never does,
in either direction.

The Core's dark-wire probe reads the source address of its looped-back
beacon: stamped with a tunnel interface's address, the beacon proves the
kernel routed multicast into the VPN, and the warning line names that
interface instead of reporting a healthy wire. The recognition is a
heuristic (interface flags, OS-reported type, and well-known names), so a
tunnel it cannot identify still slips through silently: if devices on the
same network do not see each other and one of them runs a VPN, this remains
the first thing to check even without the line. The cure is the VPN app's
own per-app exemption (WireGuard calls it *Excluded applications*), a
setting allowing LAN traffic outside the tunnel, or turning the tunnel off.

## Secrets

Two go to the OS keyring — Secret Service (Linux), Keychain (macOS), Credential
Manager (Windows): the **OIDC refresh token**, and the **account key's seed**
(`account-key-seed`), which is what lets this device vouch for one joining the
account. If none responds — SSH session, machine with no agent, CI — the Core falls
back to `secrets.json` in 0600, and says so in its log.

Which keyring answered therefore decides more than a re-login: a device whose
keyring has lost the seed still works in every respect but one — it can be linked
*from* another device and cannot link one itself (`account.status` →
`holds_key: false`). That is a legitimate state, not a corrupt one, and the
interface offers the two ways out of it
([architecture.md](architecture.md#pairing-a-device)).

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

## Autostart

The Core starts at session login, per user, with no privileges. It does not
install that itself: the **GUI** registers it at launch (and re-registers it,
idempotently, at every launch), pointing at a durable path:

| | Mechanism |
|---|---|
| Linux | an XDG autostart `.desktop` in `~/.config/autostart` |
| macOS | a LaunchAgent in `~/Library/LaunchAgents` (`RunAtLoad`; `KeepAlive` only on a *crash*, so the redundant instance that exits 0 starts no loop) |
| Windows | a value under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` |

The current session is already covered by the direct spawn — autostart takes over
from the next login on.

On **Linux** the installed form is an AppImage, whose mount is ephemeral: a path
inside it is dead the moment the app closes. The GUI therefore copies the Core
**and its sidecars** into `$XDG_DATA_HOME/1device/` and registers *that*
copy. The list of sidecars to copy is in `gui/src/supervise.rs`
(`STAGED_SIDECARS`) — one absent from it is simply never launched on a real Linux
install.

## Supervised components

The Core launches the official components installed next to its binary, restarts
them when they fall (capped exponential backoff, reset once the child has stood up),
and takes them with it when it stops. A missing component is ignored: a Core without
a tray is still a Core that works.

Three of them ship (`daemon/src/supervisor.rs`, `official_components`), each with
its own scopes: `1device-tray`, `1device-clipboard` (a per-OS
backend, on all three desktops) and `1device-menu` (the contextual menu).
The GUI is not in that list — the user launches it.

One exception to "next to its binary": on macOS the tray lives in a nested
application bundle, `1Device.app/Contents/Frameworks/1DeviceTray.app`,
and the supervisor looks there first. It has to — a process started from
`Contents/MacOS` is *the application* to Launch Services, and a tray holding the
app's identity made `open` activate the tray instead of opening the window. See
[architecture.md](architecture.md#official-components).

The contract of a supervised component:

1. It finds the Core at the path passed in `ONEDEVICE_IPC_PATH`.
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

- **Windows without a console.** The graphical autostart is how the Core starts on
  a real install, and such a process receives **none** of the events above: at
  logout it is terminated instead of stopping cleanly. The children still die with
  it (the Job object closes with the process), and a component's stale OS
  artifacts are swept at the next startup, so what is lost is the orderly
  goodbye, not the cleanup. The fix is a message-only window
  (`WM_QUERYENDSESSION`) or a real service.
- **The keyring choice is frozen at startup**: if the secrets agent comes up after
  the Core, we stay on the file fallback until the next launch.
