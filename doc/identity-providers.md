# The identity provider

The 1Device server never sees a password: it delegates "who are you" to an
**OIDC identity provider** and only ever reads the stable subject (`sub`) of the
ID tokens it validates. Which provider is the deployment's choice, made once, in
the server's `.env` ([step 2 of the deployment](server-deployment.md#step-2--deploy-with-docker-compose--caddy-recommended)).

This page is the identity half of that deployment guide:

- [the contract](#the-contract) any issuer must meet, derived from the code;
- [Google, screen by screen](#google-screen-by-screen): the reference path,
  with its traps called out before they bite;
- [recipes for issuers you host yourself](#your-own-issuer): Keycloak,
  Authentik, Zitadel, Pocket ID, Kanidm, Dex.

Every recipe was verified against the provider's current official
documentation (or its source, where the documentation is silent); versions and
caveats are noted inline.

## The contract

What the login flow actually does, and therefore what any issuer must offer
([`core/src/login.rs`](../core/src/login.rs) is the source of truth):

- **Authorization code + PKCE (S256)**, as a **native/desktop application**:
  in most providers' vocabulary, a *public* client.
- **Loopback redirect with a runtime port**: the Core starts a listener on
  `http://127.0.0.1:<port>/callback` where the **port is random at every
  login** (RFC 8252 §7.3). The issuer must accept that redirect without
  knowing the port in advance; each recipe below shows that provider's way.
  The `/callback` path is fixed, and matters wherever paths are matched.
- **Discovery**: the Core reads
  `<issuer>/.well-known/openid-configuration`; the issuer URL you configure
  must serve that document.
- **ID tokens signed with RS256.** The server validates RS256 and nothing
  else ([`server/src/oidc.rs`](../server/src/oidc.rs)). Every provider below
  signs RS256 out of the box except Kanidm (ES256 by default) and imported-key
  edge cases; the recipes say when a nudge is needed.
- **Scopes `openid email`.** Only `openid` is load-bearing: the server reads
  the `sub` claim, nothing more. The `email` claim is used by the app purely
  for display, and a login without it still succeeds; an issuer that cannot
  emit it costs you cosmetics, not access.
- **`client_secret` is optional.** The Core sends one at the token exchange
  only if the server's deployment configures it
  (`ONEDEVICE_OIDC_CLIENT_SECRET`); leave it empty for an issuer that wants
  none. The server itself never uses the secret: it advertises it to the
  clients through the deployment descriptor, and why that is safe for a
  native-app client is argued in
  [server-api.md](server-api.md#deployment-descriptor).
- The server checks `iss` against the configured issuer and `aud` against the
  configured `client_id`, and refuses tokens that omit either.

A one-line sanity check for any issuer, before touching a single client:

```sh
curl -fsS https://<issuer>/.well-known/openid-configuration | head -c 300
```

## Google, screen by screen

Google is the reference issuer: no server of yours to run, and accounts your
users already have. The price is a one-time walk through the Google Cloud
console, which is the longest step of the whole deployment. The console
reorganized in 2024: what tutorials call the "OAuth consent screen" menu is
now a dedicated section named **"Google Auth Platform"**
(<https://console.cloud.google.com/auth>). This walkthrough uses the current
labels.

1. **A project.** Any Google account works. In the console's top bar, open the
   project picker and create a project (or reuse one); the name is only ever
   seen by you.

2. **Consent configuration.** Go to **Google Auth Platform** (left navigation,
   or `console.cloud.google.com/auth`). A fresh project shows an **Overview**
   page with a **"Get started"** button that opens a short wizard:
   - **App Information**: the app name your users will see on Google's consent
     page (e.g. `1Device`) and a support email (yours);
   - **Audience**: choose **External** (Internal exists only for Google
     Workspace organizations and limits logins to their members);
   - **Contact Information**: an email for Google to reach you.

   After the wizard, the section shows the pages **Branding**, **Audience**,
   **Clients** and **Data Access**; you will only need the first three.

3. **The client.** On the **Clients** page, click **"Create client"**:
   - **Application type: Desktop app.** This is the critical choice. A "Web
     application" client must have all its redirect URIs registered in
     advance, and this flow redirects to `http://127.0.0.1:<port chosen at
     runtime>`: Google turns that down with `redirect_uri_mismatch` **in the
     browser**, before the code exchange is even attempted. A "Desktop app"
     client accepts loopback redirects on any port, which is also why the
     form asks for no redirect URI at all.
   - Give it a name (internal, users never see it).

4. **Copy both values now.** The dialog shows the **`client_id`**
   (`…apps.googleusercontent.com`) and the **`client_secret`**. Since
   mid-2025 the console shows a client secret **only once, at creation**, and
   masks it forever after; if you lose it, you reset it, and every configured
   device learns the new one from the server. Both values go into the
   server's `.env` (they are deployment configuration, not private keys: the
   descriptor hands them to your devices, and for a desktop client Google's
   own documentation says the secret "is obviously not treated as a secret").
   Google's current documentation lists the secret as optional at the token
   exchange, but deployments have seen the exchange refused without it:
   configure it, the flow simply sends it when present.

5. **Test users, then production.** On the **Audience** page, a new app is in
   **Testing** publishing status: only Google accounts listed there as **test
   users** (100 at most) can log in. Two ways to live with it:
   - stay in Testing and list each authorized account: fine for a personal
     fleet. Google's blanket rule that Testing-status consents expire after
     **7 days** has a documented carve-out when the app only asks for
     `openid`/`email`/`profile` scopes, which is our case; but the carve-out
     is one documentation page against another, so treat weekly forced
     re-logins as possible rather than impossible;
   - or click **"Publish app"** (status becomes **In production**): anyone
     with a Google account can then log in to *your server*, which still
     admits only accounts it has enrolled. With only non-sensitive scopes
     (`openid`, `email`), publishing requires **no Google verification
     review**; the console may still offer an optional "brand verification"
     that only affects how your name and logo appear on the consent screen.

6. **Fill the `.env`** ([deployment, step 2](server-deployment.md#step-2--deploy-with-docker-compose--caddy-recommended)):

   ```sh
   ONEDEVICE_OIDC_ISSUER=https://accounts.google.com
   ONEDEVICE_OIDC_CLIENT_ID=<the client_id>
   ONEDEVICE_OIDC_CLIENT_SECRET=<the client_secret>
   ```

One Google-specific trap for the long run: **an OAuth client unused for six
months is deleted automatically** (restorable for 30 days from the Clients
page's deleted-credentials view). A deployment that idles past that, say a
seasonal machine, will see logins fail until the client is restored or
recreated; the server keeps working for already-enrolled devices, which only
need the IdP again at their next login.

## Your own issuer

Any OIDC provider meeting [the contract](#the-contract) works, and the server
treats them all identically: issuer + `client_id` (+ secret if the provider
issues one) in the `.env`, done. Two things genuinely vary between providers,
and the recipes focus on them: **how to allow a loopback redirect whose port
changes at every login**, and **whether RS256 needs a nudge**.

| Provider | Client type to pick | Dynamic-port loopback | RS256 |
|---|---|---|---|
| Keycloak | OIDC client, "Client authentication" off | register `http://127.0.0.1/callback`, the port is ignored for loopback hosts | default |
| Authentik | Provider with "Client Type: Public" | redirect URI in **Regex** mode: `http://127\.0\.0\.1:\d+/callback` | default (RSA signing key) |
| Zitadel | Application type **Native** | register `http://127.0.0.1/callback`, the port is ignored on loopback | default |
| Pocket ID | OIDC client, "Public Client" checked | callback URL `http://127.0.0.1:*/callback` | default on fresh installs |
| Kanidm | `create-public` client | `enable-localhost-redirects` | **no: enable per client** |
| Dex | `staticClients` entry, `public: true` | automatic, but **only with no `redirectURIs` listed** | default |

### Keycloak

Realm of your choice; the issuer is per realm:
`https://<host>/realms/<realm>`.

- Create an OpenID Connect client; in **Capability config**, leave **"Client
  authentication" off** (that is Keycloak's public client) and keep
  **"Standard flow"** enabled.
- **Valid redirect URIs**: `http://127.0.0.1/callback`, with no port.
  Keycloak special-cases `http` on loopback hosts (`127.0.0.1`, `localhost`,
  `[::1]`) and ignores the port when matching, exactly for RFC 8252 native
  apps; the path still matches exactly, hence the `/callback`. (The full
  loopback list is Keycloak 26+; older versions already accepted the
  `127.0.0.1` form.) Do not reach for wildcards: port wildcards combined
  with a path are unsupported.
- In the client's **Advanced settings**, set **"Proof Key for Code Exchange
  Code Challenge Method"** to **S256** so PKCE is enforced rather than merely
  accepted.
- No secret exists for this client type. If your deployment wants one, turn
  "Client authentication" on instead and copy the secret from the
  **Credentials** tab; the flow works either way.

### Authentik

Authentik pairs an *application* with a *provider*; the application's slug
determines the issuer:
`https://<host>/application/o/<application-slug>/`.

- Create an **OAuth2/OpenID Provider** with **"Client Type: Public"**
  (PKCE is always supported, nothing to enable), then an application bound
  to it.
- **Redirect URIs**: add one entry, switch its matching mode from **Strict**
  to **Regex**, and use `http://127\.0\.0\.1:\d+/callback`. The regex must
  match the *entire* redirect URI (fullmatch) and dots must be escaped; both
  are per Authentik's own documentation. Per-URI matching modes exist since
  2024.8.5/2024.10.3; earlier versions are best upgraded anyway (the change
  came with a security fix).
- **Signing Key**: keep an **RSA** certificate selected (the default
  self-signed one is RSA, giving RS256). Picking an EC key would switch the
  tokens to ES256, which the server refuses.
- A Public provider exposes no usable secret; choose "Confidential" instead
  if your deployment wants one.

### Zitadel

The issuer is your instance domain; discovery lives at
`https://<instance-domain>/.well-known/openid-configuration`.

- In your project, create an application of type **Native**. That type does
  authorization code + PKCE and has **no client secret**, matching the flow
  exactly; leave `ONEDEVICE_OIDC_CLIENT_SECRET` empty.
- **Redirect URIs**: register `http://127.0.0.1/callback`. For Native
  applications Zitadel implements RFC 8252 loopback matching: on
  `localhost`/`127.0.0.1`/`[::1]` the **port is ignored** while path and
  query still must match. No "Development mode" toggle is needed for
  loopback `http`. (Some Console versions wrongly flag `http://localhost`
  URIs as invalid in the form while the server accepts them at login: a
  known UI bug; `http://127.0.0.1` is also simply the better form here.)
- RS256 is Zitadel's default signing algorithm; nothing to do unless an
  administrator deliberately rotated the instance's web keys to a non-RSA
  type.

### Pocket ID

The issuer is the instance's `APP_URL`. Pocket ID is OIDC-only and
passkey-first, which makes it one of the lightest options to operate.

- Create an OIDC client; check **"Public Client"** and **"PKCE"**.
- **Callback URLs**: `http://127.0.0.1:*/callback`. Pocket ID wildcards
  stay within one URL segment, and the port is such a segment, so `*` there
  means "any port" (this is also the pattern its own documentation uses for
  desktop clients). Wildcard semantics changed at v2; on v1, verify against
  its documentation of the time.
- A Public client has no secret (uncheck "Public Client" if your deployment
  wants one; keep PKCE checked either way).
- Fresh installs generate an RSA key, so RS256 is the default. One trap: an
  instance that **imported** a non-RSA key (Ed25519, EC) signs every token
  with it, and the server will refuse those logins; rotate the instance to
  an RS256 key in that case.

### Kanidm

The issuer is **per client**, not per instance:
`https://<idm-host>/oauth2/openid/<client-name>`, with discovery under it at
`/.well-known/openid-configuration`.

```sh
kanidm system oauth2 create-public 1device "1Device" https://<your-1device-server>
kanidm system oauth2 enable-localhost-redirects 1device
kanidm system oauth2 warning-enable-legacy-crypto 1device
```

- `create-public`: a public client; PKCE S256 is mandatory on it by design,
  and no secret exists. The trailing origin URL is a required argument;
  your 1Device server's address is a sensible value, and it is not used by
  the loopback flow.
- `enable-localhost-redirects` is Kanidm's RFC 8252 switch: loopback
  redirects are then accepted on any port (only allowed on public clients,
  where PKCE is enforced).
- `warning-enable-legacy-crypto` is **required**: Kanidm signs ES256 by
  default, which the server refuses; this per-client flag switches the
  client to RS256. The alarming name is Kanidm's judgement of RSA, not a
  deprecation of the flag.

### Dex

Dex is a lightweight federating issuer (it typically fronts LDAP, GitHub,
or another upstream); the issuer is the `issuer` value of its config, with
standard discovery under it.

```yaml
staticClients:
  - id: 1device
    name: 1Device
    public: true
```

- `public: true` with **no `redirectURIs` key at all**: exactly then, Dex
  accepts `http` redirects to loopback (`localhost`, `127.0.0.1`, `[::1]`)
  on **any port and any path**. Listing even one explicit redirect URI
  switches that client back to exact matching and breaks the dynamic port,
  so resist the urge to "document" the URI in the config.
- PKCE is supported since v2.26.0 (2020); there is no per-client switch to
  require it, the flow simply uses it.
- A public static client may also carry a `secret`; if the deployment sets
  one, put the same value in `ONEDEVICE_OIDC_CLIENT_SECRET`, otherwise
  leave both sides empty.
- Dex signs RS256 by default (its code calls RS256 mandatory-to-support);
  nothing to configure.

### Anything else

For a provider not listed here, walk [the contract](#the-contract) top to
bottom; the two questions that actually filter providers are the
dynamic-port loopback redirect and RS256. The discovery one-liner above
answers half of the rest: `id_token_signing_alg_values_supported` must
contain `RS256`, and `code_challenge_methods_supported` should contain
`S256`. Then configure the three `.env` values and watch
`docker compose logs server` during a first login; the server logs token
validation failures with their reason.
