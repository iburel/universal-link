<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com> -->

<script lang="ts">
  import type { CoreStore } from "../lib/store.svelte";
  import LinkDevice from "./LinkDevice.svelte";
  import RecoveryCode from "./RecoveryCode.svelte";

  let { store }: { store: CoreStore } = $props();

  const connected = $derived(store.connection.status === "connected");
  const disabled = $derived(!connected || store.busy);

  // In the account, but without the account's private key. The device works —
  // `ak_pub` is what verifies peers — it just cannot vouch for a new one. The way
  // back in is the same one a device with no account at all takes: a pairing from
  // a device that holds the key, or the recovery code typed here. This screen is
  // where it is offered, because the onboarding portal only shows for a device
  // that is not attested, and this one is.
  const keyless = $derived(
    store.account?.attested === true && store.account.holds_key === false,
  );
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

    <!-- Or no browser at all: a device already on the account can hand this one
         everything it needs, sign-in included. -->
    <h2>Already have a device on this account?</h2>
    <LinkDevice {store} mode="join" />
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

    {#if keyless}
      <h2>This device has your account, but not its key</h2>
      <p class="muted">
        It can still exchange files with your other devices; it just cannot link a
        new one. Give it the key and it will — the same two ways in as a brand-new
        device.
      </p>
      <LinkDevice {store} mode="join" />
      <RecoveryCode {store} label="Or enter your recovery code:" />
    {/if}

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

  h2 {
    margin: 0.5rem 0 0;
    font-size: 1rem;
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
