<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com> -->

<script lang="ts">
  import { onMount } from "svelte";

  import type { CoreStore } from "../lib/store.svelte";

  // `firstRun`: shown full-screen as a gate on a fresh install (no server yet).
  // Otherwise it is the "Server" settings view, reachable from the nav.
  //
  // `withoutServer` is the other way in (first run only): an account with no
  // server at all, living on the devices themselves. The App owns what taking
  // the door means; this screen only offers it.
  let {
    store,
    firstRun = false,
    withoutServer,
  }: { store: CoreStore; firstRun?: boolean; withoutServer?: () => void } =
    $props();

  // Local form state — pre-filled from config.json. Nothing here touches the
  // store until "Save": the store writes the file then reloads the Core.
  let serverUrl = $state("");
  let oidcIssuer = $state("");
  let oidcClientId = $state("");
  let oidcClientSecret = $state("");
  let justSaved = $state(false);

  // The OIDC fields are the deployment's, not the user's: the server publishes
  // them and the Core reads them (`session.discover`). They are only asked for
  // when that fails — a server older than that endpoint — or when someone wants
  // to override them.
  let manual = $state(false);
  // Set when discovery found no descriptor: says why the fields just appeared,
  // which a generic error banner would not.
  let unpublished = $state(false);

  onMount(async () => {
    try {
      const c = await store.loadServerConfig();
      // Only fills what is still empty: this reads a file through the shell, so
      // it can land after the user has started typing — on first run the address
      // is the very first thing they touch — and a convenience must not take
      // away what they wrote.
      serverUrl ||= c.server_url ?? "";
      oidcIssuer ||= c.oidc_issuer ?? "";
      oidcClientId ||= c.oidc_client_id ?? "";
      oidcClientSecret ||= c.oidc_client_secret ?? "";
    } catch {
      // Fresh install (no config.json) or unreadable: start from blank fields.
    }
  });

  // Light client-side checks — the Core re-validates and stays authoritative,
  // but we spare the user a round-trip for the obvious mistakes. Discovery takes
  // an address in any shape (a bare host included) and derives the rest, so all
  // it needs is something to send; the manual path writes `server_url` straight
  // into config.json, where the daemon requires a ws(s) URL.
  const addressOk = $derived(
    manual
      ? /^wss?:\/\//.test(serverUrl.trim())
      : serverUrl.trim().length > 0,
  );
  const issuerOk = $derived(/^https?:\/\//.test(oidcIssuer.trim()));
  const idOk = $derived(oidcClientId.trim().length > 0);
  const valid = $derived(
    manual ? addressOk && issuerOk && idOk : addressOk,
  );

  // Changing the server invalidates a session enrolled on the old one.
  const warnLoggedIn = $derived(!firstRun && store.session?.logged_in === true);

  async function save() {
    justSaved = false;
    unpublished = false;
    const ok = manual
      ? await store.saveServerConfig({
          server_url: serverUrl.trim(),
          oidc_issuer: oidcIssuer.trim(),
          oidc_client_id: oidcClientId.trim(),
          // A blank secret means "none" (a conformant PKCE IdP has none): the
          // shell clears the key rather than writing an empty string.
          oidc_client_secret: oidcClientSecret.trim() || null,
        })
      : await discover();
    // On first run, success flips `configured` and this whole screen is
    // replaced — no message needed. In settings the view stays: confirm inline.
    if (ok) justSaved = true;
  }

  /** The one-field path: the server is asked for the rest. */
  async function discover(): Promise<boolean> {
    const outcome = await store.setUpFromAddress(serverUrl.trim());
    if (outcome === "unpublished") {
      // Nothing was written. The fields appear, carrying whatever was already
      // configured, and the address the user typed stays in place — they only
      // have to complete it (the manual path wants a full ws(s) URL).
      manual = true;
      unpublished = true;
    }
    return outcome === "saved";
  }
</script>

<section>
  <h1>{firstRun ? "Set up your server" : "Server"}</h1>
  <p class="muted">
    {#if firstRun}
      1Device connects to a server you choose. Enter its address — it tells
      this device how to sign you in.
    {:else}
      The server this device connects to. Saving asks it again how to sign you in,
      so a deployment that changed its OpenID Connect client is picked up.
    {/if}
  </p>

  {#if store.notice}
    <p class="banner {store.notice.kind}" role="status">
      <span>{store.notice.text}</span>
      <button
        class="close"
        aria-label="Close message"
        onclick={() => store.dismiss()}>×</button
      >
    </p>
  {/if}

  {#if warnLoggedIn}
    <p class="banner warn" role="status">
      You are signed in. Changing the server will require signing in again.
    </p>
  {/if}

  {#if unpublished}
    <p class="banner warn" role="status">
      This server does not publish its settings — an older version, or not a
      1Device server. Enter them below; whoever runs it has them.
    </p>
  {/if}

  <label>
    <span>Server address</span>
    <input
      bind:value={serverUrl}
      aria-label="Server address"
      placeholder={manual
        ? "wss://1device.example.com/ws"
        : "1device.example.com"}
      autocapitalize="off"
      autocorrect="off"
      spellcheck="false"
    />
  </label>

  {#if manual}
    <label>
      <span>OpenID Connect issuer</span>
      <input
        bind:value={oidcIssuer}
        aria-label="OpenID Connect issuer"
        placeholder="https://accounts.google.com"
        autocapitalize="off"
        autocorrect="off"
        spellcheck="false"
      />
    </label>

    <label>
      <span>OpenID Connect client ID</span>
      <input
        bind:value={oidcClientId}
        aria-label="OpenID Connect client ID"
        placeholder="xxxxx.apps.googleusercontent.com"
        autocapitalize="off"
        autocorrect="off"
        spellcheck="false"
      />
    </label>

    <label>
      <span>OpenID Connect client secret <em>(optional)</em></span>
      <input
        bind:value={oidcClientSecret}
        aria-label="OpenID Connect client secret"
        type="password"
        placeholder="only if your provider requires one (e.g. Google)"
        autocapitalize="off"
        autocorrect="off"
        spellcheck="false"
      />
    </label>
  {/if}

  <div class="actions">
    <button class="primary" disabled={store.busy || !valid} onclick={save}>
      {firstRun ? "Save and continue" : "Save"}
    </button>
    {#if justSaved}
      <span class="saved" role="status">Saved.</span>
    {/if}
  </div>

  <!-- Always available, both ways: someone may want to override what the server
       publishes, or go back to letting it answer. -->
  <button
    class="link"
    onclick={() => {
      manual = !manual;
      unpublished = false;
      justSaved = false;
    }}
  >
    {manual
      ? "Read the settings from the server"
      : "Enter the OpenID Connect settings manually"}
  </button>

  {#if firstRun && withoutServer}
    <!-- The other way in: the account lives on the devices, which meet on the
         local network — no server, no sign-in. A quiet link rather than a
         fork-in-the-road: the server is still the primary path. -->
    <button class="link" onclick={withoutServer}>
      Or use 1Device without a server
    </button>
  {/if}
</section>

<style>
  section {
    display: grid;
    gap: 0.75rem;
    justify-items: start;
    max-width: 34rem;
    margin: 0 auto;
    padding: 2rem;
  }

  h1 {
    margin: 0;
  }

  p {
    margin: 0;
  }

  .muted {
    color: var(--muted);
  }

  label {
    display: grid;
    gap: 0.25rem;
    width: 100%;
  }

  label span {
    font-size: 0.85rem;
    color: var(--muted);
  }

  label em {
    font-style: normal;
    opacity: 0.7;
  }

  input {
    width: 100%;
  }

  .actions {
    display: flex;
    align-items: center;
    gap: 0.75rem;
    margin-top: 0.25rem;
  }

  .saved {
    color: var(--ok);
    font-size: 0.85rem;
  }

  /* A button, not an anchor: it navigates nowhere. */
  .link {
    border: none;
    background: none;
    padding: 0;
    font-size: 0.85rem;
    color: var(--muted);
    text-decoration: underline;
  }

  .banner {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 1rem;
    width: 100%;
    padding: 0.5rem 0.75rem;
    border-radius: var(--radius);
    border: 1px solid var(--line);
    background: var(--panel);
  }

  .banner.error {
    border-color: var(--danger);
    color: var(--danger);
  }

  .banner.warn {
    border-color: var(--warn);
    color: var(--warn);
  }

  .close {
    border: none;
    background: none;
    padding: 0 0.25rem;
    line-height: 1;
  }
</style>
