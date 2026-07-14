<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com> -->

<script lang="ts">
  import { onMount } from "svelte";

  import type { CoreStore } from "../lib/store.svelte";

  // `firstRun`: shown full-screen as a gate on a fresh install (no server yet).
  // Otherwise it is the "Server" settings view, reachable from the nav.
  let { store, firstRun = false }: { store: CoreStore; firstRun?: boolean } =
    $props();

  // Local form state — pre-filled from config.json. Nothing here touches the
  // store until "Save": the store writes the file then reloads the Core.
  let serverUrl = $state("");
  let oidcIssuer = $state("");
  let oidcClientId = $state("");
  let oidcClientSecret = $state("");
  let justSaved = $state(false);

  onMount(async () => {
    try {
      const c = await store.loadServerConfig();
      serverUrl = c.server_url ?? "";
      oidcIssuer = c.oidc_issuer ?? "";
      oidcClientId = c.oidc_client_id ?? "";
      oidcClientSecret = c.oidc_client_secret ?? "";
    } catch {
      // Fresh install (no config.json) or unreadable: start from blank fields.
    }
  });

  // Light client-side checks — the Core re-validates and stays authoritative,
  // but we spare the user a round-trip for the obvious mistakes.
  const urlOk = $derived(/^wss?:\/\//.test(serverUrl.trim()));
  const issuerOk = $derived(/^https?:\/\//.test(oidcIssuer.trim()));
  const idOk = $derived(oidcClientId.trim().length > 0);
  const valid = $derived(urlOk && issuerOk && idOk);

  // Changing the server invalidates a session enrolled on the old one.
  const warnLoggedIn = $derived(!firstRun && store.session?.logged_in === true);

  async function save() {
    justSaved = false;
    const ok = await store.saveServerConfig({
      server_url: serverUrl.trim(),
      oidc_issuer: oidcIssuer.trim(),
      oidc_client_id: oidcClientId.trim(),
      // A blank secret means "none" (a conformant PKCE IdP has none): the shell
      // clears the key rather than writing an empty string.
      oidc_client_secret: oidcClientSecret.trim() || null,
    });
    // On first run, success flips `configured` and this whole screen is
    // replaced — no message needed. In settings the view stays: confirm inline.
    if (ok) justSaved = true;
  }
</script>

<section>
  <h1>{firstRun ? "Set up your server" : "Server"}</h1>
  <p class="muted">
    {#if firstRun}
      UniversalLink connects to a server you choose. Enter its address and the
      OpenID Connect client it uses to sign you in.
    {:else}
      The server and OpenID Connect client this device connects to.
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

  <label>
    <span>Server address</span>
    <input
      bind:value={serverUrl}
      aria-label="Server address"
      placeholder="wss://universallink.example.com/ws"
      autocapitalize="off"
      autocorrect="off"
      spellcheck="false"
    />
  </label>

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

  <div class="actions">
    <button class="primary" disabled={store.busy || !valid} onclick={save}>
      {firstRun ? "Save and continue" : "Save"}
    </button>
    {#if justSaved}
      <span class="saved" role="status">Saved.</span>
    {/if}
  </div>
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
