<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com> -->

<script lang="ts">
  import type { CoreStore } from "../lib/store.svelte";

  let { store }: { store: CoreStore } = $props();

  const connected = $derived(store.connection.status === "connected");
  const disabled = $derived(!connected || store.busy);
</script>

<section>
  <h1>Account</h1>

  {#if !store.primed}
    <p class="muted">Connecting to Core…</p>
  {:else if !store.session?.logged_in}
    <p>No account is connected on this device.</p>
    <p class="muted">
      Sign-in opens in your browser; come back here once you have granted
      access.
    </p>
    <button class="primary" {disabled} onclick={() => store.login()}>
      Sign in
    </button>
  {:else}
    <dl>
      <dt>Account</dt>
      <dd>{store.session.account?.email ?? "unknown address"}</dd>
      <dt>Server</dt>
      <dd class:warn={!store.session.server_connected}>
        {store.session.server_connected ? "connected" : "unreachable"}
      </dd>
      {#if store.account?.fingerprint}
        <dt>Fingerprint</dt>
        <dd>
          <code>{store.account.fingerprint}</code>
          <span class="hint">compare this across your devices</span>
        </dd>
      {/if}
    </dl>
    <button {disabled} onclick={() => store.logout()}>Sign out</button>
  {/if}
</section>

<style>
  section {
    display: grid;
    gap: 0.75rem;
    justify-items: start;
  }

  .muted {
    color: var(--muted);
    margin: 0;
    max-width: 34rem;
  }

  p {
    margin: 0;
  }

  dl {
    display: grid;
    grid-template-columns: auto 1fr;
    gap: 0.25rem 1.5rem;
    margin: 0;
  }

  dt {
    color: var(--muted);
  }

  dd {
    margin: 0;
  }

  dd.warn {
    color: var(--warn);
  }

  dd code {
    font-family: ui-monospace, monospace;
    user-select: all;
  }

  .hint {
    color: var(--muted);
    font-size: 0.85rem;
    margin-left: 0.5rem;
  }
</style>
