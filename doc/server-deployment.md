# Deploying the 1Device server

The **server** is the *control plane*: it authenticates accounts (OIDC), holds the
directory of devices, presence, and relays the signaling information that lets two
Cores find each other. It **never** sees the transferred data (end-to-end encrypted
via iroh, directly between peers) and does not decide account membership on its own
(see the threat model below).

This document describes how **you** host it for your own devices. The binary
(`1device-server`, crate `server-daemon`) is configured through the
environment — see [`server-daemon/src/config.rs`](../server-daemon/src/config.rs)
for the source of truth and [`server-api.md`](server-api.md) for the protocol.

> **State of this building block.** The artifacts below (Docker image, Caddy stack,
> systemd unit) are not merely written: this is how the project's own deployment
> runs, and the whole path has been validated against it: a real Google login,
> enrollment, devices attesting one another, transfers in both directions. The
> image is **published to a registry** (`ghcr.io/iburel/1device-server`, amd64 and
> arm64), and the compose below pulls it: nothing gets compiled on your machine.

## What the server sees (threat model)

To decide knowingly before exposing it:

- **It sees**: which devices belong to which account (the OIDC `sub`), the name and
  platform of each device, its iroh `node_id`, its account attestation (public),
  and its presence (online / last seen at such a time).
- **It does not see**: the content of the transfers (E2E, never relayed by it),
  nor the **account key** — including while it relays a pairing between two of
  your devices: the QR code's secret travels by a screen and a camera, and the
  bundle keyed by it is ciphertext the server carries blind
  ([server-api.md](server-api.md#pairing)).
- **It publishes, to anyone**: your OIDC issuer, `client_id` and — if you set one
  — `client_secret`, in its deployment descriptor. That is deliberate and is what
  lets a device be set up from the server's address alone; for an installed
  application that secret is not a confidential value (the reasoning is in
  [server-api.md](server-api.md#deployment-descriptor)). If your IdP hands you a
  genuinely confidential secret, it is the wrong client type for this flow — see
  step 1.
- **If it is compromised**, an attacker can **deny service**, **revoke** devices, or
  lie about presence — but **can neither decrypt the transfers nor get a rogue
  device accepted**: a peer verifies the attestation against the account key that
  the server does not have, and refuses *fail-closed* otherwise.

In other words: host it like a sensitive metadata directory, not like a data store.

## Prerequisites

- A machine (VPS, home server…) with **Docker** and the **Docker Compose** plugin
  (`docker compose version`). The Docker-free path is described further down.
- A **domain name** whose **A/AAAA** record points to this machine (e.g.
  `your-server.example.com`).
- **Ports 80 and 443** open and reachable from the Internet (Caddy needs them to
  obtain and then renew the Let's Encrypt certificate).
- The [`deploy/`](../deploy/) directory of this repository. That is the whole
  requirement: the image comes prebuilt from the registry, so there is no Rust
  toolchain to install and nothing to compile. (Cloning the full repository is
  fine too, and becomes necessary only for the build-from-source fallback.)

## Step 1 — Register a Google OIDC client

The server delegates authentication to an OIDC IdP; the reference issuer is
**Google**. The Core does an **authorization code + PKCE** flow with a **loopback
redirect**, and sends a `client_secret` at the token exchange only if one is
configured ([`core/src/login.rs`](../core/src/login.rs)). Hence the **critical**
point:

> **The OAuth client must be of type "Desktop app", never "Web application".** A
> web client's redirect URIs must all be registered in advance, and this flow
> redirects to `http://127.0.0.1:<port chosen at runtime>`: Google turns that down
> with `redirect_uri_mismatch` **in the browser**, before the code exchange is even
> attempted. A "Desktop app" client is the one that accepts a dynamic loopback
> port.

The full console walkthrough, with the current screen names and every trap
called out (secret shown only once, Testing status, the six-month inactivity
deletion), is in **[identity-providers.md](identity-providers.md#google-screen-by-screen)**.
The short version:

1. In the console's **Google Auth Platform** section, run the **"Get
   started"** wizard (app name, **External** audience, contact email).
2. On the **Clients** page, create a client of type **Desktop app** and
   copy the **`client_id`** and **`client_secret`** immediately (the secret
   is shown only at creation).
3. On the **Audience** page, add your Google accounts as **test users**, or
   publish the app to skip the list (no verification review is needed for
   the `openid`/`email` scopes this flow uses).

Both values go into **this server's** configuration (step 2), which hands them
to the clients through its deployment descriptor: that is what spares you
configuring each machine and phone by hand. An IdP that wants no secret simply
gets none. For an installed application the secret is not confidential, since it
ships inside every copy of the app, and PKCE is what actually protects the
exchange; the reasoning is spelled out in
[server-api.md](server-api.md#deployment-descriptor).

The **loopback** (`http://127.0.0.1:<port>/callback`) is handled automatically for
"Desktop app" clients: the port is dynamic, you have no redirect URL to register.

*(Another OIDC IdP is fine if it meets the
[contract](identity-providers.md#the-contract): PKCE on a loopback redirect
with a runtime port, discovery, RS256 tokens, secret optional. Verified
recipes for **Keycloak, Authentik, Zitadel, Pocket ID, Kanidm and Dex** are in
[identity-providers.md](identity-providers.md#your-own-issuer); then fill in
that issuer instead.)*

## Step 2 — Deploy with Docker Compose + Caddy (recommended)

Caddy terminates TLS and obtains the certificate **on its own**; it natively relays
the `/ws` WebSocket. The server, for its part, stays in cleartext on the internal
network.

```sh
cd deploy
cp .env.example .env
# Edit .env: ONEDEVICE_DOMAIN, ONEDEVICE_OIDC_ISSUER,
# ONEDEVICE_OIDC_CLIENT_ID (+ ONEDEVICE_OIDC_CLIENT_SECRET with Google).
docker compose up -d
```

What this starts ([`deploy/docker-compose.yml`](../deploy/docker-compose.yml)):

- **`server`**: the published image `ghcr.io/iburel/1device-server:latest`
  (multi-arch, amd64 + arm64, built from
  [`docker/server/Dockerfile`](../docker/server/Dockerfile) and smoke-tested by
  [`.github/workflows/server-image.yml`](../.github/workflows/server-image.yml)),
  directory persisted in the `directory` volume (`/data`), reachable only by Caddy
  (no published port). To pin a release instead of `latest`, put its version in
  the `image:` tag. To build from source instead, `docker compose up -d --build`
  from a full clone of the repository: same Dockerfile, same result, just slower.
- **`caddy`**: the official `caddy:2` image, ports 80/443, config
  [`deploy/Caddyfile`](../deploy/Caddyfile), certificates persisted in the
  `caddy_data` volume.

Follow the startup:

```sh
docker compose logs -f server   # "server listening" = OK
docker compose logs -f caddy    # the certificate acquisition shows up here
```

An incomplete configuration makes the server **refuse to start**, and it logs all
the errors at once — look at `docker compose logs server`.

## Verify the deployment

```sh
# Health, through Caddy's TLS:
curl https://your-server.example.com/health         # -> ok

# The deployment descriptor: what a client reads to configure itself. Check the
# issuer and the client here — a device is then set up with this address alone.
curl https://your-server.example.com/.well-known/1device.json

# The WebSocket handshake must answer 101 (Switching Protocols). `--http1.1` is
# not optional: this is an HTTP/1.1 Upgrade, and those headers are meaningless
# over HTTP/2 — which is what you get as soon as anything in front (a CDN, Caddy
# itself) negotiates h2. Without it the answer is a 400 that looks exactly like a
# broken deployment. Real clients speak HTTP/1.1 here.
curl -sSi --http1.1 https://your-server.example.com/ws \
     -H "Connection: Upgrade" -H "Upgrade: websocket" \
     -H "Sec-WebSocket-Version: 13" \
     -H "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==" | head -1
```

Each client's `config.json` will then point to `wss://your-server.example.com/ws` (see
[README, Part 3](../README.md#piece-3--configjson-on-each-pc)).

## Docker-free alternative (binary + systemd)

If you prefer the bare binary behind a reverse proxy you already manage:

1. Compile the server:
   ```sh
   cargo build --release --locked -p onedevice-server-daemon --bin 1device-server
   ```
2. Install the binary, the system user, and the unit — see the header of
   [`deploy/1device-server.service`](../deploy/1device-server.service).
   The unit makes the server listen on loopback; fill in
   `/etc/1device/server.env`:
   ```sh
   ONEDEVICE_SERVER_BIND=127.0.0.1:8080
   ONEDEVICE_OIDC_ISSUER=https://accounts.google.com
   ONEDEVICE_OIDC_CLIENT_ID=…apps.googleusercontent.com
   ```
   (Do not put `ONEDEVICE_SERVER_STATE` there: the unit already sets it via
   `StateDirectory`. An `EnvironmentFile` would take precedence over that setting.)
3. Put your reverse proxy in front. With **nginx**, the WebSocket upgrade must be
   relayed explicitly — template in
   [`deploy/reverse-proxy-nginx.conf.example`](../deploy/reverse-proxy-nginx.conf.example).
   (Caddy, for its part, needs nothing more than the Caddyfile's `reverse_proxy`.)

`systemctl stop` sends `SIGTERM`: the server exits cleanly (code 0).

## The companion relay (recommended)

The account server is a small control plane: it never carries your devices'
bytes. Off the LAN, with no VPN between two devices, the data plane needs a
relay for its rendezvous, and the clients' relay setting is **off by
default**: an operator who runs one restores the off-LAN path for the whole
fleet without touching a single client. The pair (account server + relay) is
the recommended shape of a full deployment.

The compose file carries an optional [`iroh-relay`] service for it, off
unless asked for:

1. Give the relay its own domain (A/AAAA to this machine), for example
   `relay.your-server.example.com`, and set `ONEDEVICE_RELAY_DOMAIN` in
   `.env`.
2. Uncomment the relay block in the `Caddyfile` (Caddy terminates its TLS
   like the server's) and copy `iroh-relay.toml.example` to
   `iroh-relay.toml`.
3. Announce it to the fleet: `ONEDEVICE_RELAYS=https://relay.your-server.example.com`
   in `.env`. The server serves that list in its deployment descriptor, and
   every device of the fleet re-reads it at each session; a device with an
   explicit local relay setting keeps its own.
4. `docker compose --profile relay up -d`.

`ONEDEVICE_RELAYS` is a comma-separated list: a larger deployment runs
regional relays (each entry its own machine and domain: the relay is
stateless and scales horizontally, unlike this server, which does not need
to), and each device elects the nearest. The relay and the account server
never talk to each other: announcing a relay hosted anywhere else works the
same, and a server that announces none is a valid deployment whose devices
simply keep their own relay setting.

Relays carry user bytes (end to end encrypted: a relay operator sees node
ids, client IPs, timings and volumes, never content). The rendezvous-only
policy planned in #88 will let a deployment cap or refuse that carriage;
until then, size the relay for your fleet's traffic.

[`iroh-relay`]: https://github.com/n0-computer/iroh

## Backup and loss of the directory

The directory is a JSON file (`ONEDEVICE_SERVER_STATE`), in the `directory`
volume under Docker or `/var/lib/1device/` under systemd. Back it up with the
rest of the machine.

**Losing it is not catastrophic**: each device still holds its account key locally.
After restoring from empty, each one simply has to re-connect (OIDC re-login →
re-enrollment) and re-publish its attestation. You lose the presence history and the
names, not the ability to link up again.

## Settings

Required: `ONEDEVICE_SERVER_BIND`, `ONEDEVICE_OIDC_ISSUER`,
`ONEDEVICE_OIDC_CLIENT_ID`. Optional (defaults in parentheses):
`ONEDEVICE_OIDC_CLIENT_SECRET` (none; the server never uses it itself — it
advertises it in the deployment descriptor, and Google's clients need it at the
token exchange), `ONEDEVICE_SERVER_STATE` (`1device-directory.json`),
`ONEDEVICE_HEARTBEAT_SECS` (30), `ONEDEVICE_HEARTBEAT_MAX_MISSED` (2),
`ONEDEVICE_NONCE_TTL_SECS` (60), `ONEDEVICE_PAIRING_TTL_SECS` (120; how
long a QR code stays claimable — paced by a human walking to another device,
scanning and confirming), `ONEDEVICE_FRESH_TOKEN_MAX_AGE_SECS` (300), `ONEDEVICE_JWKS_REFRESH_MIN_SECS` (60; shortest delay between two
JWKS fetches — the issuer's signing keys are re-fetched on a key-id miss, i.e. a
key rotation, but no more often than this),
`ONEDEVICE_MAX_REQUESTS_PER_MINUTE` (120; `0` = unlimited),
`ONEDEVICE_RELAYS` (none; comma-separated relay URLs announced to the fleet
in the deployment descriptor, see "The companion relay" above),
`ONEDEVICE_LOG` (log level). Detail and semantics:
[`server-daemon/src/config.rs`](../server-daemon/src/config.rs) and
[`server-api.md`](server-api.md).

## What is not (yet) there

- **No graceful shutdown of axum**: `docker stop` / `SIGTERM` cuts the in-flight
  connections dead (the clients reconnect) — acceptable for a control plane.
- **A single node**: the full-snapshot JSON persistence targets a single server.
  Several concurrent replicas would require a real DBMS (lead noted in
  [`server-daemon/src/store.rs`](../server-daemon/src/store.rs)).
