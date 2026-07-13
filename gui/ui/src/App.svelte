<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com> -->

<script lang="ts">
  import { onMount } from "svelte";

  import { installFileDrop } from "./lib/dragdrop";
  import type { CoreStore } from "./lib/store.svelte";
  import Account from "./views/Account.svelte";
  import Approvals from "./views/Approvals.svelte";
  import Devices from "./views/Devices.svelte";
  import Onboarding from "./views/Onboarding.svelte";

  type View = "account" | "devices" | "approvals";

  let { store }: { store: CoreStore } = $props();
  let view = $state<View>("account");

  onMount(() => {
    void store.start();
    // File drop is a WINDOW event (see lib/dragdrop.ts): we listen for it here,
    // once, whatever view is shown.
    const drop = installFileDrop(store);
    return () => {
      store.stop();
      void drop.then((unlisten) => unlisten?.());
    };
  });

  const coreLabel = $derived(
    store.connection.status === "connected"
      ? `Core connected (API v${store.connection.api_version})`
      : "Connecting to Core…",
  );
  // The Core was reachable, then no longer is: what is shown is frozen.
  const stale = $derived(store.primed && store.connection.status !== "connected");

  // Blocking portal: once connected to the account, until this device has
  // joined the vault (C7 attestation), nothing else is accessible — a send
  // would fail closed without it. `onboardingPending` holds the portal while
  // the recovery code is displayed, before `attested` is re-read (see the
  // store). `account === null` (an old Core) does not open a portal: we don't
  // block on a missing capability.
  const needsOnboarding = $derived(
    store.primed &&
      store.session?.logged_in === true &&
      // `onboardingPending` holds the portal on its own: even if `account`
      // transiently goes to null (a background `account.status` that fails),
      // the displayed code must not be taken away. So it must be evaluated
      // BEFORE the `account !== null` guard, not after (otherwise short-circuit).
      (store.onboardingPending ||
        (store.account !== null && !store.account.attested)),
  );
</script>

{#if store.connection.status === "incompatible"}
  <div class="blocked" role="alert">
    <h1>Incompatible version</h1>
    <p>
      This Core speaks version {store.connection.api_version} of the local API;
      this interface speaks version 1. Please update UniversalLink.
    </p>
  </div>
{:else if needsOnboarding}
  <Onboarding {store} />
{:else}
  <div class="app">
    <nav aria-label="Sections">
      <p class="core" class:connected={store.connection.status === "connected"}>
        <span class="dot" aria-hidden="true"></span>{coreLabel}
      </p>
      <button
        class:active={view === "account"}
        aria-current={view === "account" ? "page" : undefined}
        onclick={() => (view = "account")}>Account</button
      >
      <button
        class:active={view === "devices"}
        aria-current={view === "devices" ? "page" : undefined}
        onclick={() => (view = "devices")}>Devices</button
      >
      <button
        class:active={view === "approvals"}
        aria-current={view === "approvals" ? "page" : undefined}
        onclick={() => (view = "approvals")}
      >
        Approvals
        {#if store.pending.length > 0}
          <span class="badge">{store.pending.length}</span>
        {/if}
      </button>
    </nav>

    <main>
      {#if stale}
        <p class="banner warn" role="status">
          Core unreachable — the information shown is frozen.
        </p>
      {/if}
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

      {#if view === "account"}
        <Account {store} />
      {:else if view === "devices"}
        <Devices {store} />
      {:else}
        <Approvals {store} />
      {/if}
    </main>
  </div>
{/if}

<style>
  .app {
    display: grid;
    grid-template-columns: 180px 1fr;
    height: 100%;
  }

  nav {
    display: flex;
    flex-direction: column;
    gap: 0.25rem;
    padding: 0.75rem;
    background: var(--nav);
    border-right: 1px solid var(--line);
  }

  nav button {
    text-align: left;
    border-color: transparent;
    background: transparent;
    display: flex;
    justify-content: space-between;
    align-items: center;
  }

  nav button.active {
    background: var(--panel);
    border-color: var(--line);
  }

  .badge {
    background: var(--accent);
    color: var(--accent-text);
    border-radius: 999px;
    padding: 0 0.4rem;
    font-size: 0.75rem;
  }

  .core {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    margin: 0 0 0.75rem;
    font-size: 0.8rem;
    color: var(--muted);
  }

  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--warn);
    flex: none;
  }

  .core.connected .dot {
    background: var(--ok);
  }

  main {
    overflow-y: auto;
    padding: 1.25rem;
  }

  .banner {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 1rem;
    margin: 0 0 1rem;
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

  .blocked {
    display: grid;
    place-content: center;
    gap: 0.5rem;
    height: 100%;
    padding: 2rem;
    text-align: center;
    max-width: 30rem;
    margin: 0 auto;
  }
</style>
