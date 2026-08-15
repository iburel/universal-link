# Beyond the LAN, without a server

A serverless account works wherever mDNS works: the same local network. This
page is about everything past that wall. It is the client half of the
self-hosting story ([deployment.md](deployment.md) holds the `config.json`
reference; [server-deployment.md](server-deployment.md) is the full-server
rung); the design lives in [architecture.md](architecture.md), principle 3.

The ladder, each rung strictly optional:

1. **Nothing.** Your devices meet on the local network. Zero setup, zero
   infrastructure, and the account ends at the walls. This is also the
   network's ground state: the relay is **off by default** (#104), so an
   unconfigured device contacts no relay anybody did not choose.
2. **Your own network** (WireGuard, Tailscale, public IPv6). Your devices reach
   each other anywhere that network routes. You run what you already ran;
   1Device adds nothing to host.
3. **Your own relay** (`iroh-relay`). One tiny stateless binary with a domain
   and a TLS certificate, and your devices rendezvous through it across any
   NAT. No accounts, no storage.
4. **The full server.** Enrollment by login, presence, remote pairing, and
   the operator's relays announced to the whole fleet (no per-device relay
   setting needed): [server-deployment.md](server-deployment.md).

## How a device is found off the LAN

Every device signs, in its own directory record, where it can be dialed: the
socket addresses its endpoint stands behind (`addrs`: LAN, VPN and public IPv6
addresses alike) and the relay somebody chose for it (`relay_hint`: the
configured URL, or the home relay elected under an explicit `"n0"` opt-in
or a server's announced relays). The
records travel between your devices in the directory exchange they already
run, and at dial time the hints are handed to the
transport as candidates, tried alongside whatever mDNS resolves. When a
device's addresses move (it joined a network, a VPN came up), it re-signs its
record on the spot and tells the account.

What makes this safe is what it refuses to claim:

- **Nothing is published anywhere.** The hints travel only inside the
  account's authenticated, encrypted exchanges, between devices that hold each
  other's attestation. There is no global directory to query, deliberately: a
  public lookup would announce every device's whereabouts to anyone holding
  its `node_id`.
- **A hint is not a promise.** Nobody vouches that the device is up; the claim
  is "worth trying", and the dial is what answers. A stale or even hostile
  hint costs one failed attempt: connections are authenticated by the node
  key, so no route can lead to a wrong peer.
- **A relayer cannot plant a route.** The hints are covered by the device's
  own signature, ordered by its `seq`: whoever carries a record cannot rewrite
  where it points.
- **Pairing stays in the room.** Introducing a NEW device is a physical,
  same-room gesture (the code, a screen and a camera). Off-LAN reach only ever
  concerns devices already in the account: introduced at home, connected
  anywhere.

## Recipe: WireGuard

Note first why the naive advice ("just put both machines on the VPN") did not
work before this existed: WireGuard does not route multicast, so two devices
that could perfectly well reach each other were unable to *find* each other.
The signed addresses are what closes that gap: the tunnel address is a local
interface address, so the endpoint stands behind it and signs it like any
other.

1. Set up the tunnel as you would for anything else: one interface per
   machine, an address each (say `10.8.0.1/24` and `10.8.0.2/24`), and
   `AllowedIPs` on each peer covering the other's tunnel address.
2. Pair the devices once, on a common network (the pairing gesture needs the
   room, not the tunnel).
3. Bring the tunnel up on both. Each device observes its `10.8.0.x` address,
   re-signs its record, and the other learns it at the next directory
   exchange: while they still share a network, immediately; otherwise at the
   moment any path between them exists (the tunnel itself, once one side
   heard the other's hint).

One nuance about split tunneling. 1Device warns when a VPN swallows its LAN
multicast (the probe names the tunnel and suggests excluding the app). That
warning is about *discovery on the local network*; it does not apply to this
recipe, where the tunnel is the route on purpose. If you rely on WireGuard for
off-LAN reach, do not exclude 1Device from it.

## Recipe: Tailscale

The same story with the addresses handed out for you. Every machine gets a
stable `100.x.y.z` address; the daemon's endpoint stands behind it like any
interface address and signs it into the record. Pair once in the same room,
install Tailscale on both machines, done. No subnet routing, no MagicDNS, no
Tailscale ACL beyond "these machines may talk" is required.

## Recipe: your own relay

For devices behind networks you do not control (mobile data, hotel Wi-Fi,
carrier NAT), a relay is the rendezvous: hole punching needs a meeting point,
and when no direct path can be punched the relay carries the bytes as a
fallback. The stock [`iroh-relay`](https://github.com/n0-computer/iroh) binary
is stateless: no accounts, no storage, one domain, one TLS certificate.

1. Run `iroh-relay` on a machine with a public address, behind your TLS
   (a container or the plain binary; its own docs cover flags and ports).
2. On EVERY device of the account, set `relay` in `config.json` (or the
   `ONEDEVICE_RELAY` environment variable) to `https://your-relay.example`,
   and restart the Core. The phone reads the same `config.json` field.
3. Each device now signs that relay into its record (`relay_hint`), and its
   siblings dial it through the relay from anywhere.

Two properties are deliberate and worth knowing:

- **Relaying your devices' bytes is explicit, never a default.** The relay
  setting is off unless somebody set it, and a device whose relay is off
  contacts none at all (no relay connection, no housekeeping traffic, nothing
  signed into its record), with one deliberate exception: a device that
  belongs to a server account uses the relays its operator announces
  ([server-deployment.md](server-deployment.md)), somebody's choice too. The
  n0 public relays still exist as one of the choices (`"n0"`), and that
  opt-in signs the home relay it elects, exactly as a configured URL signs
  itself and an announced relay signs its election: everything in
  `relay_hint` is a relay somebody chose.
- **What a relay operator sees.** Never content (everything is end-to-end
  encrypted), but a relay in use sees node ids, client IPs, timings and
  volumes. That operator is you, which is the point of this rung.

## The fine print

- The hints ride the directory. A device that changed networks while nothing
  could carry its new record (no common LAN, no relay, no tunnel already up)
  is unreachable until any path exists again; its record catches up at the
  first exchange. Rung 3 exists largely to make that window short.
- `devices.list` shows the consequence, honestly: a device dialable only
  through its hints is `reachable` without being `online` (the GUI says
  "reachable"): nobody vouches it is up, the dial is what answers.
- Revocation needs no route at all: tombstones travel with every directory
  exchange, and a struck-off device that dials anyone learns its fate from
  the answer.
