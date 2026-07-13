# Deploying the UniversalLink server

The **server** is the *control plane*: it authenticates accounts (OIDC), holds the
directory of devices, presence, and relays the signaling information that lets two
Cores find each other. It **never** sees the transferred data (end-to-end encrypted
via iroh, directly between peers) and does not decide account membership on its own
(see the threat model below).

This document describes how **you** host it for your own devices. The binary
(`universallink-server`, crate `server-daemon`) is configured through the
environment — see [`server-daemon/src/config.rs`](../server-daemon/src/config.rs)
for the source of truth and [`server-api.md`](server-api.md) for the protocol.

> **State of this building block.** The artifacts below (Docker image, Caddy stack,
> systemd unit) are written and the image builds and starts. What has **not yet**
> been validated end-to-end: a real Google login against a deployed server, on two
> real machines. That is the next building block. There is also no image published
> to a registry: you build it yourself.

## What the server sees (threat model)

To decide knowingly before exposing it:

- **It sees**: which devices belong to which account (the OIDC `sub`), the name and
  platform of each device, its iroh `node_id`, its account attestation (public),
  and its presence (online / last seen at such a time).
- **It does not see**: the content of the transfers (E2E, never relayed by it) nor
  the **account key** (derived from the recovery code, never transmitted).
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
- The means to build the image (the cloned repository) — see also the build
  prerequisites in the [README](../README.md).

## Step 1 — Register a Google OIDC client

The server delegates authentication to an OIDC IdP; the reference issuer is
**Google**. The Core does an **authorization code + PKCE, public client** flow (it
sends **no `client_secret`** — see [`core/src/login.rs`](../core/src/login.rs)).
Hence the **critical** point:

> **The OAuth client must be of type "Desktop app", never "Web application".** A
> Google "Web application" client **requires** the `client_secret` at the code
> exchange, **even with PKCE** — the exchange would then fail with
> `client_secret is missing`. A "Desktop app" client, on the other hand, treats the
> secret as **optional**: the PKCE-without-secret flow goes through.

In the [Google Cloud console](https://console.cloud.google.com/):

1. **Create or select a project.**
2. **OAuth consent screen**:
   - user type: **External**;
   - fill in the app name and the support email;
   - **scopes**: `openid` and `email` are enough (the server only reads the `sub`;
     the Core displays the email). Nothing more;
   - **users**: as long as the app stays in **"Testing"** status, add each
     authorized Google account as a *test user*. ⚠️ In Testing mode, Google **expires
     refresh tokens after 7 days**: you will have to re-connect every week. To avoid
     this, **publish** the app ("In production" status).
3. **Credentials → Create credentials → OAuth client ID**:
   - application type: **Desktop app**;
   - give it a name.
4. Retrieve the **`client_id`** (`…apps.googleusercontent.com`). The `client_secret`
   shown **is not used** by UniversalLink.

The **loopback** (`http://127.0.0.1:<port>/callback`) is handled automatically for
"Desktop app" clients: the port is dynamic, you have no redirect URL to register.

*(Another OIDC IdP — Auth0, Keycloak, Entra… — is fine if it exposes a **public**
client (PKCE, no secret) and the discovery endpoint
`/.well-known/openid-configuration`. Then fill in its issuer instead.)*

## Step 2 — Deploy with Docker Compose + Caddy (recommended)

Caddy terminates TLS and obtains the certificate **on its own**; it natively relays
the `/ws` WebSocket. The server, for its part, stays in cleartext on the internal
network.

```sh
cd deploy
cp .env.example .env
# Edit .env: UNIVERSALLINK_DOMAIN, UNIVERSALLINK_OIDC_ISSUER,
# UNIVERSALLINK_OIDC_CLIENT_ID.
docker compose up -d --build
```

What this starts ([`deploy/docker-compose.yml`](../deploy/docker-compose.yml)):

- **`server`** — the image built from [`docker/server/Dockerfile`](../docker/server/Dockerfile),
  directory persisted in the `directory` volume (`/data`), reachable only by Caddy
  (no published port).
- **`caddy`** — the official `caddy:2` image, ports 80/443, config
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

# The WebSocket handshake must answer 101 (Switching Protocols):
curl -sSi https://your-server.example.com/ws \
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
   cargo build --release --locked -p universallink-server-daemon --bin universallink-server
   ```
2. Install the binary, the system user, and the unit — see the header of
   [`deploy/universallink-server.service`](../deploy/universallink-server.service).
   The unit makes the server listen on loopback; fill in
   `/etc/universallink/server.env`:
   ```sh
   UNIVERSALLINK_SERVER_BIND=127.0.0.1:8080
   UNIVERSALLINK_OIDC_ISSUER=https://accounts.google.com
   UNIVERSALLINK_OIDC_CLIENT_ID=…apps.googleusercontent.com
   ```
   (Do not put `UNIVERSALLINK_SERVER_STATE` there: the unit already sets it via
   `StateDirectory`. An `EnvironmentFile` would take precedence over that setting.)
3. Put your reverse proxy in front. With **nginx**, the WebSocket upgrade must be
   relayed explicitly — template in
   [`deploy/reverse-proxy-nginx.conf.example`](../deploy/reverse-proxy-nginx.conf.example).
   (Caddy, for its part, needs nothing more than the Caddyfile's `reverse_proxy`.)

`systemctl stop` sends `SIGTERM`: the server exits cleanly (code 0).

## Backup and loss of the directory

The directory is a JSON file (`UNIVERSALLINK_SERVER_STATE`), in the `directory`
volume under Docker or `/var/lib/universallink/` under systemd. Back it up with the
rest of the machine.

**Losing it is not catastrophic**: each device still holds its account key locally.
After restoring from empty, each one simply has to re-connect (OIDC re-login →
re-enrollment) and re-publish its attestation. You lose the presence history and the
names, not the ability to link up again.

## Settings

Required: `UNIVERSALLINK_SERVER_BIND`, `UNIVERSALLINK_OIDC_ISSUER`,
`UNIVERSALLINK_OIDC_CLIENT_ID`. Optional (defaults in parentheses):
`UNIVERSALLINK_SERVER_STATE` (`universallink-directory.json`),
`UNIVERSALLINK_HEARTBEAT_SECS` (30), `UNIVERSALLINK_HEARTBEAT_MAX_MISSED` (2),
`UNIVERSALLINK_NONCE_TTL_SECS` (60), `UNIVERSALLINK_FRESH_TOKEN_MAX_AGE_SECS`
(300), `UNIVERSALLINK_MAX_REQUESTS_PER_MINUTE` (120; `0` = unlimited),
`UNIVERSALLINK_LOG` (log level). Detail and semantics:
[`server-daemon/src/config.rs`](../server-daemon/src/config.rs) and
[`server-api.md`](server-api.md).

## What is not (yet) there

- **No published image** on a registry: you build it yourself.
- **Real bring-up not validated**: the nominal path (Google login, two machines) is
  tested in memory, but not yet proven against a real deployment — that is the next
  building block.
- **No graceful shutdown of axum**: `docker stop` / `SIGTERM` cuts the in-flight
  connections dead (the clients reconnect) — acceptable for a control plane.
- **A single node**: the full-snapshot JSON persistence targets a single server.
  Several concurrent replicas would require a real DBMS (lead noted in
  [`server-daemon/src/store.rs`](../server-daemon/src/store.rs)).
