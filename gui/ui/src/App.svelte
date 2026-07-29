<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com> -->

<script lang="ts">
  import { onMount } from "svelte";

  import { installFileDrop } from "./lib/dragdrop";
  import type { CoreStore } from "./lib/store.svelte";
  import { appVersion } from "./lib/version";
  import Account from "./views/Account.svelte";
  import Approvals from "./views/Approvals.svelte";
  import Devices from "./views/Devices.svelte";
  import Onboarding from "./views/Onboarding.svelte";
  import Pairing from "./views/Pairing.svelte";
  import ServerSetup from "./views/ServerSetup.svelte";

  type View = "account" | "devices" | "approvals" | "settings";

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

  // A file share from the Android share sheet is waiting for a destination, and
  // the picker lives in the Devices view. Mobile only: `pendingShare` is always
  // null on the desktop, where a send starts from a drag-and-drop instead.
  $effect(() => {
    if (store.pendingShare) view = "devices";
  });

  const coreLabel = $derived(
    store.connection.status === "connected"
      ? `Core connected (API v${store.connection.api_version})`
      : "Connecting to Core…",
  );
  // The Core was reachable, then no longer is: what is shown is frozen.
  const stale = $derived(store.primed && store.connection.status !== "connected");

  // Fresh install: the Core has no server configured. Block on the setup screen
  // BEFORE login is possible (you cannot sign in nowhere). `configured === false`
  // and not merely falsy: an old Core (no such field) must not be pushed into
  // setup, and a session.changed delta (no field) must not flicker it on.
  const needsServerSetup = $derived(
    store.primed && store.session?.configured === false,
  );

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
    <!-- This screen asks the user to update, and no navigation reaches it: the
         version has to be here too, it is the first thing anyone will ask for. -->
    <p class="version">UniversalLink {appVersion}</p>
  </div>
{:else if needsServerSetup}
  <ServerSetup {store} firstRun />
{:else if store.pairing}
  <!-- A pairing under way owns the window, above the onboarding portal: the
       device being linked is IN that portal when it starts one, and the
       confirmation is not a screen the user may wander away from. It cannot come
       before the setup screen, though — pairing needs a server to go through. -->
  <Pairing {store} />
{:else if needsOnboarding}
  <Onboarding {store} />
{:else}
  <div class="app">
    <!-- Outside the <nav> on purpose: on a narrow screen the sections become a
         bottom tab bar, and the connection line stays at the top. -->
    <p class="core" class:connected={store.connection.status === "connected"}>
      <span class="dot" aria-hidden="true"></span>{coreLabel}
    </p>

    <nav aria-label="Sections">
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
      <button
        class:active={view === "settings"}
        aria-current={view === "settings" ? "page" : undefined}
        onclick={() => (view = "settings")}>Server</button
      >
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
      {:else if view === "approvals"}
        <Approvals {store} />
      {:else}
        <ServerSetup {store} />
      {/if}
    </main>

    <!-- Last in the reading order, at the foot of the sidebar on screen: it is
         chrome, not content. Outside the <nav> for the same reason as the
         connection line above. -->
    <p class="version">UniversalLink {appVersion}</p>
  </div>
{/if}

<style>
  .app {
    display: grid;
    grid-template-columns: 180px 1fr;
    grid-template-rows: auto 1fr auto;
    grid-template-areas:
      "core main"
      "nav main"
      "version main";
    height: 100%;
  }

  .core {
    grid-area: core;
  }

  nav {
    grid-area: nav;
  }

  main {
    grid-area: main;
  }

  /* Three cells, one sidebar: they share its skin. */
  .core,
  nav,
  .app > .version {
    background: var(--nav);
    border-right: 1px solid var(--line);
  }

  nav {
    display: flex;
    flex-direction: column;
    gap: 0.25rem;
    padding: 0.75rem;
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
    /* No bottom padding: the nav cell's own top padding is the gap. */
    margin: 0;
    padding: 0.75rem 0.75rem 0;
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

  /* Sidebar footer: the nav above it grows, this stays at the bottom of the
     window. Its own line, and not a suffix to the connection status, because the
     two numbers would read as one — the product's version is not the local
     API's. */
  .app > .version {
    grid-area: version;
    /* No top padding: the nav cell's own bottom padding is the gap. */
    margin: 0;
    padding: 0 0.75rem 0.75rem;
  }

  .version {
    font-size: 0.75rem;
    color: var(--muted);
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

  /* Phone (360 dp here), and a desktop window squeezed that narrow: a fixed
     180px column would eat half of it, so the sections become a bottom tab bar
     — thumb-reachable, and out of the way of the content. The webview is
     already inset from the gesture bar by the activity (MainActivity's
     onWebViewCreate), so the bar needs no extra room of its own. */
  @media (max-width: 600px) {
    .app {
      grid-template-columns: 1fr;
      grid-template-rows: auto 1fr auto;
      grid-template-areas:
        "core"
        "main"
        "nav";
    }

    .core,
    nav,
    .app > .version {
      border-right: none;
    }

    .core {
      padding: 0.35rem 0.75rem;
      border-bottom: 1px solid var(--line);
    }

    /* The tab bar is at the bottom and a thumb is on it: a footer underneath
       would be wasted height in the one place where height is scarce. The version
       joins the top strip instead, right-aligned in the SAME cell as the
       connection status — nothing overlaps, the longest status ("Core connected
       (API v1)") and the version together stay well inside the 360 dp this
       layout targets. */
    .app > .version {
      grid-area: core;
      justify-self: end;
      align-self: center;
      padding: 0.35rem 0.75rem;
    }

    nav {
      flex-direction: row;
      gap: 0.25rem;
      padding: 0.35rem 0.5rem;
      border-top: 1px solid var(--line);
    }

    nav button {
      flex: 1;
      /* Centred, and tall enough to be a tap target rather than a click one.
         The gap replaces the sidebar's space-between, which centring drops —
         without it the Approvals badge would touch its label. */
      justify-content: center;
      gap: 0.3rem;
      min-height: 44px;
      padding: 0.35rem 0.2rem;
      font-size: 0.9rem;
    }

    main {
      padding: 0.85rem;
    }
  }
</style>
