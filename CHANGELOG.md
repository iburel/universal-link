# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Your real keyboard and mouse, on all three desktops.** The half of the
  keyboard and mouse engine that touches the operating system: reading the
  keyboard and mouse of the computer you are looking at, and typing and
  clicking on the one you are driving. Windows uses the two low level input
  hooks and `SendInput`, Linux uses XInput2 and XTEST on an X11 session, macOS
  uses an event tap and `CGEventPost`. Three things it does that the tools in
  this category have historically not:

  - **What it cannot do, it says.** A window running as administrator, a
    locked screen, a password field on a Mac, a permission that has not been
    granted: each is DETECTED and each has a sentence, rather than a keystroke
    that goes nowhere in silence. One of those detections is worth naming: a
    Windows session nobody is attached to looks almost exactly like a normal
    one from the inside, and a first version of this code would have typed a
    whole session into a desktop nobody was looking at while reporting
    success. It now asks whether anybody can see the screen before it types.
  - **Your own keystrokes stop acting on your own machine while you drive
    another**, and they start again afterwards, on every path out including a
    crash. Each platform gives that away differently and each is proved rather
    than asserted: on Linux against a second X client whose window stops
    receiving keys, on Windows against the operating system's own key state.
  - **A permission granted late is believed.** On a Mac the two permissions
    this needs are given at a dialog, and nothing tells a program when that
    happens; it now notices within a second and stops saying it cannot type.

  A machine with no platform half still does everything else, exactly as
  before. What is NOT here: Wayland (its portals are their own piece of work,
  and a Wayland session says so rather than half working), and the interface
  that turns all of this into something you can see and switch on.

- **The engine that will share one keyboard and mouse across your
  computers.** A new official component, `1device-input`, and the frozen
  `input.*` vocabulary the interfaces will speak: one plane holding every
  monitor of every computer, dragged into the arrangement that matches the
  room; who may drive this computer, decided here and nowhere else; who this
  computer is willing to drive, which decides nothing on the far side; the
  guards that keep a pointer from crossing an edge by accident; the return
  hotkey; and one authoritative snapshot carrying the plane, the state of
  every computer with the round trip measured to it, the live session and any
  problem, so a window opening late renders everything from that snapshot
  alone. Permission to drive is stored on the machine that will be driven and
  is never copied anywhere: the other computer learns by asking and gets a
  clean refusal with a sentence, and the permission dies the instant the
  device leaves the account. The arrangement is a small signed document the
  computers pass between themselves, converging on the same plane in any
  order with no coordination, and a screen that is unplugged keeps its place
  and says so instead of letting the pointer fall through where it used to
  be. What is NOT here yet is the half that touches the real keyboard: this
  release ships the whole computer-independent engine, proven against a test
  double and against two real Cores talking to each other. Until the
  per-platform capture and injection land, every computer says plainly that
  nothing on it can type yet, and it says it while doing everything else: you
  can already arrange your screens, and decide who may drive this computer,
  and both will still be true the day it can. Design and protocol:
  [doc/input-sharing.md](doc/input-sharing.md).

- **The Core grew a live channel between two of your computers**, the
  primitive the coming keyboard and mouse sharing rides on. `peers.send`
  already carried a message between the components of one role across the
  account's devices; it opens a stream and waits for an acknowledgment per
  message, which is right for a gesture and hopeless for a flow of pointer
  positions. `peers.channel` is the other half: a component asks for a
  channel to another device, opens a second connection to the local socket
  the way a paste already does, and the two devices' components have a live
  duplex pipe. The Core carries the frames and reads none of them, so it
  learns nothing about keyboards, screens or permissions: it enforces who may
  talk to whom, the caps, and the moment the channel must die. It dies
  properly, which is the whole point: revoke the far device, log out, leave
  the account, stop the Core, let the component crash or the peer vanish, and
  both ends see a clean end with a reason the interface can say, never a pipe
  that goes quiet. One channel between a given kind of component and a given
  device, so a retry replaces its predecessor instead of leaking pipes. And a
  deployment whose relays are
  rendezvous-only (the setting below) refuses such a channel by its own code
  rather than quietly carrying a live flow it said it would not: a live
  session names no size, so it asks for a direct path. Nothing of this shows
  in the interfaces yet, and no component uses it yet: it is the contract the
  input component, and any third-party replacement, will speak
  ([doc/core-api.md](doc/core-api.md), `peers.*`).

- **Folders now stay identical across your computers, by themselves** (the
  sync engine). A new official component, `1device-sync`, keeps chosen
  folders in step over the generic primitives the Core already had: control
  messages ride `peers.send`, bytes ride published transactions, and the
  Core reads and writes both disks - nothing crosses the IPC. It is
  replaceable by construction (the exclusive `sync-backend` role, one
  dialect marker on every message) and owns all of its own state.
  What the interfaces get is the frozen `sync.*` vocabulary: create a set
  from a folder (or a single file) and invite your other computers, accept
  with a local folder of your choosing, pause, resume, leave, and resolve a
  conflict; plus one authoritative snapshot, `sync.status`, carrying per
  device whether it is up to date, behind (by how many files) or offline,
  the open conflicts with both versions, and the names the engine refused
  with the reason - so a window opening late can render everything and act
  from that snapshot alone. Membership is each device's own signed word (no
  central list): an invitation is the door both ways, `left` and `declined`
  bind until someone re-invites, and a device the account revoked is gone
  here too. Concurrent edits keep BOTH versions, deterministically: the same
  winner and the same
  "name (DeviceName, 2026-08-16 14h02 UTC).ext" copy on every computer, one
  gesture to keep the one you want. A deletion loses to a concurrent edit,
  everywhere and up the tree, so nothing is lost to a race. Design and
  protocol: [doc/sync-engine.md](doc/sync-engine.md).

- **The Core grew the generic primitives pluggable cross-device components
  build on** (the sync epic's foundation). Transactions gained a second
  producer: any component holding the new `transactions.publish` scope can
  freeze a manifest into a LONG-LIVED transaction - outside the clipboard's
  last-copier-wins election, revoked explicitly (`transactions.revoke` cuts
  open channels at once, the path a logout already took) or dying with its
  publisher's connection - and a peer device installs it with
  `transactions.adopt`, after the same fail-closed re-validation a remote
  clipboard manifest gets; the untouched consumer machinery (range reads,
  fills, `FILE_CHANGED`) then serves it as it always served a remote clip.
  Components of the same role can now talk to each other across the
  account's devices: `peers.send` carries an opaque, capped payload to an
  attested device over the encrypted data plane and lands as a
  `peer.message` push on the components holding the sender's role there -
  role-to-same-role, the role stamped by the Core, delivery best-effort but
  the refusals honest (`DEVICE_OFFLINE`, `COMPONENT_ABSENT`,
  `PAYLOAD_TOO_LARGE`). And the `sync.*` namespace is now a routed facade:
  forwarded whole to the exclusive `sync-backend` component (a second
  exclusive role beside the clipboard's), scope-checked by the Core
  (`sync.read` for the snapshot, `sync.manage` for gestures, `sync.serve`
  to answer), with the backend publishing the `sync` topic through
  `sync.emit`. None of it shows in the interfaces yet: it is the contract
  the official sync engine, and any third-party replacement, will speak.

- **A deployment can now make its relays rendezvous-only.** A relay plays
  two roles: the meeting point for hole punching (tiny, a few KB per device
  per day) and the fallback that carries the bytes when no direct path can
  be punched (the expensive one, on the operator's bill). One server-side
  setting (`ONEDEVICE_RELAY_MAX_PAYLOAD`, a size in bytes) now splits them:
  the announced relays keep introducing devices, payloads up to the cap may
  still ride them - clipboard-sized things keep working everywhere - and
  anything larger requires the punched direct connection. The role rides the
  same announcement as the relays themselves, so the fleet picks it up with
  no per-device configuration, keeps honoring it when the server is down,
  and sheds it with the relationship at logout. What the cap costs,
  honestly: the rare pair that genuinely cannot hole-punch (both ends behind
  symmetric NAT or CGNAT, for example two phones on mobile data) can no
  longer move large payloads through the operator's relay; the failure is a
  clean sentence naming the pair and its remedies - the same network, or a
  VPN between the two - never a silent stall. A device whose own `relay`
  setting names other infrastructure is outside the operator's word, and the
  deployment guide now pairs the setting with a relay-side rate limit for
  clients that predate it.
- **A serverless account now works beyond the local network.** Every device
  signs, in its own directory record, where it can be dialed: the addresses
  its endpoint stands behind (LAN, VPN and public IPv6 alike), re-signed on
  the spot when they move. Its already-paired siblings learn them through the
  directory exchanges they already run and try them at every dial, so two
  devices on the same WireGuard or Tailscale network, or with public IPv6,
  reach each other from anywhere that network routes: no server, no third
  party, nothing published outside the account. A stale or hostile hint costs
  one failed attempt and can never lead to a wrong machine (connections are
  authenticated by the device's key), and a relayed record cannot have its
  routes rewritten in transit (the hints ride the device's own signature).
  Pairing a NEW device stays a same-room gesture, deliberately. Recipes in
  [beyond the LAN](doc/beyond-the-lan.md).
- **A serverless account can rendezvous through a relay you host.** The
  `relay` setting serves the serverless half too: a device that is explicitly
  pointed at a self-hosted
  [`iroh-relay`](https://github.com/n0-computer/iroh) signs that relay into
  its record, and its siblings dial it through the relay across any NAT.
  Explicit, never a default: a fleet whose relay is off signs none, and
  no device of yours is ever dialed through a relay nobody chose. In the
  app, a device dialable only through its signed hints shows as "reachable":
  nobody vouches it is up, the dial is what answers.
- **A server now announces its relays to its fleet.** The deployment
  descriptor gained a `relays` list: the operator states where the relays
  are once, server-side (`ONEDEVICE_RELAYS`), and every device of the fleet
  picks them up at each session, no per-device configuration. The
  announcement fills the off default only (an explicit local relay setting
  always wins), may be empty (a server without a relay stays a valid
  deployment), and each device keeps its own copy on disk, so a fleet whose
  server is down still meets through the operator's relays. The compose file
  gained an optional `iroh-relay` service and the deployment guide a
  companion-relay section: the pair (account server + relay) is the
  recommended shape of a full deployment.
- **The relay is now off by default, and using one is a choice.** An
  unconfigured device used to ride n0's public relays silently: the bound
  endpoint kept a housekeeping connection to the nearest one even idle. The
  `relay` setting (formerly `relay_url`) is now three-valued: `"off"` (the
  default: no relay connection at all, and no ten-second wait for one at
  login), `"n0"` (the public relays, opted into explicitly; the elected home
  relay is then signed into the record like a configured one), or a relay
  URL as before. What off costs, honestly: two devices behind two distinct
  NATs, off the LAN, with no VPN between them, need a relay to meet. A
  fleet that ran unconfigured keeps the LAN, VPN routes and signed address
  hints, and regains a relay by choosing one; a `config.json` still carrying
  `relay_url` (or the `ONEDEVICE_RELAY_URL` variable) is refused with the
  cure in the message rather than silently downgraded. The "device is
  offline" message now names the remedies.
- **A VPN that swallows the local network is now called by its name.** A
  device-wide or per-app VPN can capture multicast routing: beacons loop
  back inside the kernel while the actual network never sees a byte, which
  the old probe filed as a healthy wire and stayed silent about. The probe
  now reads the source address of its looped-back beacon; when a tunnel
  interface owns it, the warning names that interface and the way out
  (exclude 1Device from the VPN, or allow LAN traffic in its settings).
  Devices that only ever met through the server while a VPN was up is the
  symptom this line explains.
- **The identity step of self-hosting is documented end to end.** A new
  [identity providers](doc/identity-providers.md) page carries the Google
  console walkthrough with today's screen names and every trap called out
  (Desktop-app client type, the secret shown only at creation, Testing
  status, the six-month inactivity deletion), plus verified recipes for
  issuers you host yourself: Keycloak, Authentik, Zitadel, Pocket ID,
  Kanidm and Dex, each with the exact settings for the dynamic-port
  loopback redirect and RS256 tokens this flow needs.
- **Self-hosting the server no longer requires building it.** Every release
  now publishes the server's Docker image to GitHub Container Registry
  (`ghcr.io/iburel/1device-server`, amd64 and arm64), each architecture
  smoke-tested as a running server before publication. The `deploy/` compose
  pulls that image by default: copy `.env.example` to `.env`, fill it in,
  `docker compose up -d`, and no Rust toolchain ever touches your machine.
  Building from source remains one flag away (`docker compose up -d --build`).
- **Machines on the same network now find each other directly.** Each desktop
  announces itself over mDNS (as `1device`, UDP 5353) and resolves its
  siblings the same way — and being visible on the local network now *counts as
  reachable*: sends and the shared clipboard take the local route even for a
  machine that has no relay at all, and no longer depend on the relay to get
  started when both ends share a network. The
  announcement carries the device's public key and addresses, nothing more,
  and changes nothing about trust: a machine that is not attested on the
  account is refused exactly as before, and an impostor answering to someone
  else's identity fails the handshake. `"lan_discovery": false` in
  `config.json` turns the whole thing off.
- **Two machines in the same room keep working when the server is
  unreachable.** Each device now remembers the account's device list
  (`directory.json` in the config directory), so a machine that starts with no
  internet still recognizes its siblings and reaches them over the local
  network — sends and the shared clipboard included. Remembering is not
  trusting: every use re-verifies each device's attestation against the
  account key, exactly as with the live list — what the memory can do is keep
  a *revoked* device recognized until the next server contact, so a snapshot
  older than **7 days** no longer counts and the machine then waits for the
  server, as before. Logging out forgets the list. What still needs the
  server: signing in, and pairing a device that is not in the same room.
  (Adding, revoking and reaching devices all work without one; see the
  serverless and beyond-the-LAN entries below.)
- **And you can see it.** A machine this one hears nearby is badged *"on this
  network"* in the app — presence the machine observes itself, no server
  involved — and stays a live drop target and a right-click destination when
  the internet is down, appearing and disappearing as it comes and goes. The
  context menu used to empty itself the moment the server link dropped;
  now what remains is exactly what is still in the room.
- **The phone is on the network too.** Android joins the same LAN discovery:
  the app now sees — and is seen by — the account's machines on the same
  Wi-Fi, badges them *"on this network"*, and shares to them directly even
  when the internet is down. Android hands an app no incoming multicast
  unless it asks, and asking keeps the Wi-Fi chip awake for every multicast
  frame on the network, so the app asks exactly while it has a window in
  front or a transfer in flight — and stops listening the rest of the time,
  which is the part where a phone is in a pocket.
- **An account can now exist with no server at all.** The first screen offers
  a second door: *"Or use 1Device without a server"*. Taking it creates
  the account right on the device, recovery code included, with no sign-in and
  nothing leaving the machine. Membership is proven the same way it always
  was: every device carries a record signed under the account key, so trust
  works exactly as it does with a server, minus the server.
- **Adding a device works without a server too, on the local network.** Any
  device already on the account can vouch for a new one, session or not: the
  same show-a-code gesture, a code of its own kind, and an introduction that
  happens entirely between the two machines on the LAN. The phone's scanner
  reads both kinds of code and tells them apart on its own.
- **The devices carry the account's directory between themselves.** Two
  attested devices exchange whom they know whenever they talk, so a device
  learns of a sibling it has never met from a third one that has. Each device
  signs what it says about itself (its name, its platform), which lets a
  description travel through a middleman without trusting it, and only the
  owner can publish a newer one.
- **Removing a device no longer needs a directory to strike it from.** The
  account key signs the withdrawal itself: a permanent tombstone that travels
  device to device and outlives whatever a server still lists. The struck
  device obeys its own tombstone: once it hears it, it erases the account from
  itself, and its next startup is a first startup.
- **One account, half on a server, half not.** Serverless is not a separate
  mode: devices enrolled through a server and devices paired in the room form
  a single account, and each half proves itself to the other the same way.
  Where a server names a device, the device countersigns, so the name survives
  the server's absence; the server's view is merged into the account's own,
  never swapped in; logging out keeps what the account can prove by itself;
  and revoking through the server also mints the account's own tombstone, so
  the strike reaches devices that server never met.
- **And the interface only offers what can succeed.** Gestures that need a
  session say so instead of failing; a server gone unreachable takes away
  exactly one gesture, *showing* a code (which needs it), while reading a code
  from a device on this network keeps working, and the screen says which is
  which; a device the account struck off explains itself in one clear
  sentence.

### Changed

- **The project is now called 1Device.** One name, one identity, everywhere:
  the app and its installers, the binaries (`1device-core`, `1device-server`),
  the config and state directories, the environment variables (`ONEDEVICE_*`),
  the server's descriptor path (`/.well-known/1device.json`), the pairing-code
  prefixes (`1D1:`, `1D2:`) and every wire identifier. This is a clean identity
  break, said plainly: a device on this release and a device on an earlier one
  cannot talk to each other, and no code path bridges them. What to do about
  it: update every machine, reinstall the app on Android (its application id
  changed), redeploy the server (new binary name, new variables), then enroll
  the devices again; macOS asks once more for the Local Network permission.
  Earlier releases and the git history keep the name they shipped under; this
  page describes those same releases under the product's one name.

### Fixed

- **A settings problem now reaches the screen.** When a saved setting is
  faulty (the retired `relay_url` spelling, a mistyped `relay` value), the
  faulty setting is simply not applied and the Core keeps running, but the
  reason used to reach the log file and nobody else: the app showed a
  perfectly healthy screen while, say, the relay somebody explicitly chose
  had been silently dropped. `session.status` now carries the reason (with
  its cure) as `problem` and the app shows it as a banner. The banner
  mirrors the file: fixing the named setting (in `config.json` for the ones
  the setup screen does not own) and reloading, a save on the Server view
  included, withdraws it. A cured `relay` takes effect at the next Core
  start, and the log says so. Found on a real client during live validation
  of the `relay_url` migration path.
- **Android now honors the configured relay.** The phone parsed the relay
  setting from its `config.json` like every desktop and then ignored it, so a
  fleet pointed at a self-hosted relay silently excluded its phone. The parsed
  value now reaches the transport, on the same path as everywhere else.
- **Launching the app twice now brings back the window you already have.**
  The Core always held a single-instance lock, but the app itself did not: a
  second launch (from the launcher, or the tray's "Open" while the window was
  already there) opened a second window on the same Core. The second launch
  now hands off to the running app, which surfaces and focuses its window,
  and exits. Desktop only; the phone's shell never had the problem.
- **Android: a repeating log line no longer buries the log.** Android's log is
  a small ring buffer shared with the whole system, and an app that repeats
  itself pushes out everything worth reading — a phone left in a pocket, whose
  network the system takes away, had its own diagnosis wiped by hundreds of
  identical "send refused" warnings. Each distinct line is now written once
  and its repeats are counted out loud instead of copied (measured on the
  device: 141 lines in 45 seconds became 9, with nothing lost — the counts are
  reported). Nothing is silenced: a line never seen before always goes
  through.
- **macOS: a blocked "Local Network" permission now reads as one clear line.**
  macOS asks each fresh build for that permission and quietly refuses every
  discovery packet until someone answers — LAN discovery deaf and mute, and
  the only trace hundreds of cryptic `error sending mDNS: No route to host`
  warnings. The Core now probes the wire when it starts with LAN discovery on
  and, when nothing comes back, says plainly what is wrong and where the
  switch is (System Settings → Privacy & Security → Local Network). The same
  line covers any machine whose network drops multicast; sends through the
  server and the relay were never affected.
- **macOS: clicking the app icon opened nothing.** Not the Dock, not the Finder,
  not the Launchpad, not the tray's own *Open* — no window, no error, nothing.
  The tray shipped as a plain executable inside the app bundle, and to macOS a
  process started from there *is* the application: it took the app's identity, so
  from the moment the background service started the tray, the system considered
  1Device to be already running and every launch just brought the tray to
  the front. It now ships as a helper application of its own inside the bundle,
  which is how the same problem is solved in Chrome and in every Electron app.
  Two things follow: the icon opens the window again, and the second
  "1Device" that sat in the Dock — the tray, wearing the app's name — is
  gone, as a menu-bar item should be. It also stops taking the focus from
  whatever you are doing when it starts at login. Windows and Linux were never
  affected.

## [0.6.0] - 2026-07-29

Adding a machine to the account is a code shown on one screen and read by the
other, six digits to compare, and one confirmation — no browser, nothing typed.
Setting a machine up is one field, the server's address. Both rest on the same
change underneath: every device now keeps the account key, so any device can vouch
for the next.

### Added

- **Link a device by showing it a code.** One machine displays a QR code, the
  other reads it — the phone with its camera, a PC by pasting the same line under
  it — and one confirmation on the machine already on the account hands the
  newcomer everything it needs to be trusted by the others: it signs in, enters the
  directory and gets the account key in one gesture, with no browser and nothing
  typed. Either machine may be the one that displays, so the phone can add a PC as
  readily as the reverse. The buttons are on the Devices screen (*Add a device*) and
  in the join step of the setup portal.
  - **Both screens show the same six digits**, and the confirmation asks you to
    check them: they come out of the channel the two devices share, so someone who
    read your code over your shoulder — or off a screen share — is showing
    different ones. Decline then; that person is the one you would be adding.
  - Your **recovery code is still accepted** and is unchanged. What changes is its
    job: it is the way back if you ever lose every device, not something to retype
    on each new one.
  - A code is good for two minutes, works once, and both screens count the deadline
    down themselves. Closing the dialog cancels it on the other device rather than
    leaving it waiting.
  - On the phone the scanner is the app's own (CameraX + ZXing, no Play Services,
    both Apache-2.0), so it works on a de-Googled phone; the camera is asked for
    the first time you scan and never otherwise. It keeps looking until it sees one
    of *our* codes, so another QR code in the frame is ignored.
- **Setting up a device takes one field: the server's address.** The issuer and
  the OpenID Connect client describe the *deployment*, not the user — identical on
  every device of one server, yet retyped on each machine and each phone. The
  server now publishes them at `GET /.well-known/1device.json`, the Core
  reads them from an address in any shape (a bare host, or the `wss://…/ws` you
  paste from another device), and the setup screen writes them for you. Set the
  secret on the server with `ONEDEVICE_OIDC_CLIENT_SECRET` — Google's clients
  need one, other IdPs may not — and it is served with the rest deliberately: for
  an installed application it identifies the app rather than authenticating it,
  and it already shipped inside every client's configuration.
  - A schemeless address is read over TLS. `http://` and `ws://` work when written
    out, but never by default: those settings decide where the sign-in goes.
  - A server that publishes none — one older than this release, or another site
    altogether — makes the screen say so and ask for the three fields, as before.
    They also stay available behind "Enter the OpenID Connect settings manually",
    for a deployment you want to override.
  - Saving in the Server settings tab asks the server again, so a deployment that
    rotated its OpenID Connect client is picked up without touching each device.
- **The app shows which version it is** — at the foot of its sidebar, and on the
  screen that asks you to update, the one place where the question always comes
  up. It is the interface's version: on Linux the background Core runs from a
  copy the app refreshes when it starts, so between an upgrade and the next
  launch an autostarted Core can still be the previous one.

### Changed

- **Every device now keeps the account key**, in its keyring — the OIDC refresh
  token's neighbour — where before each one derived it from the recovery code and
  discarded it. That is what makes the pairing above possible: a device can only
  vouch for another if it still holds the key. Stated plainly, it also means a
  device whose storage is read gives up the account key, where previously that took
  the user's recovery code, so key rotation stops being a nicety — it is the answer
  to a compromised device, and it is still upcoming.
  - A device upgrading from the previous release holds no key yet, and so is not
    offered "add a device" until it has one. Its Account screen says which state it
    is in and puts both ways back in right there: pair it with a device that holds
    the key, or type the recovery code once — which now does nothing but stow the
    key. The same screen answers for a keyring that loses the entry later, which is
    the other way to land in that state.
  - Installing the key is confirmed by reading it back before the device claims to
    have joined the account. A keyring write can be *accepted* and never land — on
    the desktop it is queued — which used to leave a device attested with no key,
    silently.
  - A key that does not match the account this device is attested under is refused,
    from a keyring as from a pairing: the account key cannot be swapped underneath a
    device.
  - The approval prompt for a third-party component now reads "open and close the
    session, **and link new devices to the account**" for the `session.manage`
    scope. Same scope, wider consequence, and the prompt is the only place you are
    told.
- **Server** — both features above need the deployment updated too. It gained the
  `pairing.*` methods that bring the two devices together (a server still on 0.5.0
  does not know them, so the dialog fails against it) and the descriptor at
  `/.well-known/1device.json` that makes the one-field setup possible. What
  the two devices exchange stays sealed to the channel their code establishes: the
  server relays that bundle without being able to read it, and so never learns the
  account key. It does learn that two devices paired, and when — as it already
  learns every enrollment.

### Fixed

- **Linux, AppImage: the tray icon could vanish for a whole session.** The Core
  the app launches runs from a copy that lives *outside* the AppImage, but it
  inherited the bundle's library paths — so on a distribution newer than the
  build host, the tray failed to load the system's app-indicator library and the
  supervisor restarted it about once a second until logout. The Core, and the
  components it spawns in turn, now get an environment cleared of the bundle's
  paths while keeping the host's. Measured on Debian 13, where the tray stays up
  again. Only the case where the app itself starts the Core was affected: a Core
  started by autostart at login never had the problem.

### Known limitations

- **Only the device that already holds the account confirms a pairing.** The one
  being added checks that the six digits match, but it has no button to decline
  with: it takes what the confirmation hands it. So someone who reads your code
  before your own machine does can put that device into *their* account instead —
  and the account name it then shows is a label their side chose. What settles the
  question is the account fingerprint on the Account screen, next to the one another
  of your devices shows. Deliberate for this release; what bounds it is that a code
  lives two minutes and answers exactly once.
- Account key rotation is still not implemented, and it matters more than it did:
  every device now keeps the key, so a device whose storage is read gives it up.
- Desktop installers remain unsigned (milestone 1); the OS shows a first-launch
  warning. That is also what keeps the two richer context-menu integrations out of
  reach — the Windows 11 **main** menu and a Finder extension both have to be
  signed. The Android APK *is* signed, with the project's own self-issued key,
  because Android installs nothing otherwise: a sideload, not a Play Store listing.
- The phone shares, it does not receive, and is never offered as a menu
  destination.
- Aggressive power management can still end the Android app: on the test device,
  swiping it out of Recents kills the process even with the foreground service
  running.
- A menu manager that *crashes* leaves its entries behind until the supervisor
  restarts it, and a click on a stale one fails silently. A clean shutdown removes
  them, and so does every startup.

## [0.5.0] - 2026-07-28

Right click → send to another machine, from the file manager's own menu, on all
three desktops. It needed no new Core API: one more component, shipped inside the
installers, riding the device list and `files.send` that were already there.

### Added

- **"Send to PC X" from the file manager's context menu** — right-click a
  selection of files or a folder, pick one of your PCs, and it is on its way: no
  window to open, no drag. The entries are the account's live device list, so one
  appears only for a device that is online, attested and reachable, and they all
  disappear while the Core has no server connection — the menu never offers a
  destination it cannot reach. Per desktop:
  - **Windows** — a `1Device ▸ PC` submenu in the classic shortcut menu, for
    a file selection and for a folder, plus one entry per device under "Send to".
  - **Linux** — an entry in Dolphin's menu (KDE ServiceMenu) and a submenu of
    Nautilus scripts.
  - **macOS** — one entry per device in Finder's **Services** submenu. Whether it
    also appears in the inline "Quick Actions" row is a system setting (General →
    Login Items & Extensions → Finder), not something the app can decide.

  A click sends the whole selection as a single transfer, tracked in the tray and
  the app like any other. Folders go as folders. A phone is never offered as a
  destination: a file dropped into its private storage is a file nothing on it can
  open.

### Changed

- All three installers carry a fourth sidecar, the contextual-menu component,
  next to the Core, the tray and the clipboard backend. On Linux it is staged out
  of the AppImage like the others, so a Core started by autostart still finds it
  once that ephemeral mount is gone.
- The README and `doc/` are back in step with what ships: how to install it and
  the real asset names instead of a "no packaging yet" status, the Android client
  and the contextual menu in the architecture, autostart described per OS, and the
  OIDC `client_secret` a Google client does need — three documents disagreed about
  that one.

### Known limitations

- Desktop installers remain unsigned (milestone 1), which is also what keeps the
  two richer menu integrations out of reach: the Windows 11 **main** context menu
  (an `IExplorerCommand` COM DLL) and a Finder extension both have to be signed
  and registered at install time. On Windows 11 the entries are therefore in the
  classic menu, behind **Show more options** (or Shift+F10).
- A menu manager that *crashes* leaves its entries behind until the supervisor
  restarts it, and a click on a stale one fails silently. A clean shutdown removes
  them, and so does every startup.
- The phone shares, it does not receive, and is never offered as a menu
  destination.
- Aggressive power management can still end the Android app: on the test device,
  swiping it out of Recents kills the process even with the foreground service
  running.
- Account key rotation is not implemented.

## [0.4.0] - 2026-07-27

An Android client: from the system share sheet, send text to the account's
clipboard or a file to one device you pick. It ships as a signed `.apk` next to
the three desktop installers.

### Added

- **Android client** (arm64, Android 7.0 or newer) — the desktop UI verbatim,
  driven by a Core embedded in the app's own process (Android has nowhere to
  supervise a separate daemon from), with the same OIDC login, device enrollment
  and account join as a PC. Two gestures, both from the share sheet:
  - **Share text** — it is copied to every other device of the account, ready to
    paste, and the app reports how many of them actually received it.
  - **Share a file** — the app asks which device, then the file lands there,
    tracked and cancellable like a desktop drag-and-drop.
  - A foreground service runs only while there is work to protect — a transfer, a
    share waiting for its destination, a round-trip through the browser — and not
    merely because the app is open.
- **Materialized clipboard transactions** — a clipboard source the OS may kill
  the moment it is done (a phone) cannot answer a pull at paste time. Such a copy
  pushes its bytes to the account's online devices instead, each caches them, and
  the paste is served locally, so the source may vanish immediately. Inline
  payloads only and capped at 8 MiB, never files, never content the OS marks
  sensitive — those stay pull-at-paste, as does every desktop copy, which is
  byte-identical to before.

### Changed

- **Server** — `android` is accepted as a device platform at enrollment. A
  deployment still on 0.3.0 refuses a phone.
- **Every device needs 0.4.0 to receive a phone's clipboard share**: a peer that
  does not know the push stream drops it, and the phone reports that device as
  failed rather than assuming success.
- The reconnection backoff is capped at 64x the base delay instead of a flat 60 s
  — a phone loses its network whenever the app stops running, and a share
  arriving after a while in a pocket used to wait out a 30 s tick and give up.
  It asks for a 200 ms base and comes back in about 13 s; the desktop Core's own
  cap moves from 60 s to 64 s, which changes nothing in practice.
- CI compiles the Android app on every push — clippy for its own target triple,
  then the release APK, the configuration R8 runs on. `gui-mobile` sits outside
  the workspace, so until now nothing built its Rust and nothing at all compiled
  its Kotlin.
- Dependencies: `jsonwebtoken` 11, `fuser` 0.18 (the FUSE tree that serves a
  received files clip on Linux), `png` 0.18, `windows` 0.62, `ed25519-dalek`
  3.0.0 final with its exact pin dropped, along with the routine Rust, npm,
  Gradle and Actions updates and a build-time `postcss` advisory.

### Known limitations

- Desktop installers remain unsigned (milestone 1); the OS shows a first-launch
  warning. The Android APK *is* signed, because Android will not install or
  upgrade an app otherwise — with the project's own self-issued key, so it is a
  sideload and not a Play Store listing.
- The phone shares, it does not receive: it never writes to the Android
  clipboard, and a file sent to it lands in the app's private storage, which
  nothing yet opens.
- Aggressive power management can still end the app: on the test device, swiping
  it out of Recents kills the process even with the foreground service running.
- No context-menu integration yet.
- Account key rotation is not implemented.

## [0.3.0] - 2026-07-23

Shared clipboard across your devices, and folder support for transfers. The
Core now supervises a per-OS clipboard backend and a system tray, both shipped
inside the installers.

### Added

- **Shared clipboard** — copy on one device and paste on another. A dedicated
  clipboard backend (a sidecar the Core supervises) bridges the real system
  clipboard for text, images, single files, and whole folders, on Linux (X11,
  with ICCCM INCR for large payloads), Windows, and macOS. It is pull-at-paste:
  the payload moves over the cross-Core data plane only when the receiver
  actually pastes, and content the OS marks sensitive (e.g. a password manager)
  is honored in both directions and carries no size hint.
- **Send and paste folders** — a whole directory tree, empty folders included,
  can now be sent by drag-and-drop or shared through the clipboard, not just
  single files.
- **System tray** — a tray icon showing status, with a Quit action that shuts
  the Core down cleanly. Bundled as a sidecar next to the Core.
- **Bidirectional local IPC** — the client gained incoming requests and a data
  channel, the transport the clipboard and transfers ride on.

### Changed

- All three installers now bundle and stage the clipboard backend as a sidecar,
  alongside the existing Core and tray.
- **Server** — the OIDC verifier refreshes its JWKS on an unknown key id, so it
  follows the identity provider's key rotation without a restart.
- CI runs the test suite under `cargo-nextest` — the cross-reactor data-channel
  tests are serialized and the residual contention flake is retried, ending a
  macOS flake — and `rustfmt` is pinned and enforced.

### Fixed

- Windows clipboard images: a 3-pixel horizontal shift on synthesized
  `CF_DIBV5` bitmaps (miscounted trailing channel masks) is corrected.

### Known limitations

- Installers remain unsigned (milestone 1); the OS shows a first-launch warning.
- No context-menu integration yet.
- Account key rotation is not implemented.

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
  runtime from `config.json` / `ONEDEVICE_*`. `session.status` reports a
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

[Unreleased]: https://github.com/iburel/universal-link/compare/v0.6.0...HEAD
[0.6.0]: https://github.com/iburel/universal-link/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/iburel/universal-link/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/iburel/universal-link/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/iburel/universal-link/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/iburel/universal-link/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/iburel/universal-link/releases/tag/v0.1.0
