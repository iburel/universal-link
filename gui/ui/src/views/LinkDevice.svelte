<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com> -->

<script lang="ts">
  import type { CoreStore } from "../lib/store.svelte";

  // Where a pairing STARTS. Once one is under way, the screen that shows it takes
  // over the window (views/Pairing.svelte) — so this only ever offers the two
  // gestures, never the flow itself.
  //
  // `mode` changes the wording and nothing else: which end of the exchange this
  // device is on is the Core's to decide, from what this device can actually do.
  // Neither side can therefore be asked for the wrong gesture, and both need both
  // — only one of the two machines may have a camera.
  let { store, mode }: { store: CoreStore; mode: "join" | "sponsor" } = $props();

  let entering = $state(false);
  let code = $state("");

  // Only the SHOWING gesture is bound to the server, and only for a device with
  // a session: it pairs over that session, so showing a code while the server is
  // lost is a dead end. READING a code is not — a `1D2` code names the device to
  // dial on the local network, which is exactly the situation a lost server
  // leaves the room in. And a device with no session opens a connection of its
  // own on demand (or shows a `1D2` itself), so `server_connected` — false for
  // it at all times — gates nothing there: it would disarm the very screen a
  // brand-new device needs.
  const stalled = $derived(
    store.session?.logged_in === true && store.session.server_connected !== true,
  );
  const disabled = $derived(
    store.busy || store.scanning || store.connection.status !== "connected",
  );

  async function link() {
    await store.enterPairingCode(code);
    // Cleared only on success — the store sets a banner otherwise, and what was
    // typed is still there to be fixed.
    if (store.pairing) {
      entering = false;
      code = "";
    }
  }
</script>

{#if store.pairingSupported}
  <div class="pair">
    <p class="muted">
      {mode === "sponsor"
        ? "Add a device to your account: show it a code, or read the one it shows."
        : "Link this device from one already on your account: show it a code, or read the one it shows."}
    </p>

    {#if entering}
      <input
        bind:value={code}
        aria-label="Pairing code"
        placeholder="1D1:… or 1D2:…"
        autocapitalize="off"
        autocorrect="off"
        spellcheck="false"
        onkeydown={(e) => {
          // The keyboard does not get past what disarms the button: neither the
          // dead connection nor the empty field.
          if (e.key === "Enter" && !disabled && code.trim()) void link();
        }}
      />
      <div class="actions">
        <button
          class="primary"
          disabled={disabled || !code.trim()}
          onclick={link}>Link</button
        >
        <button disabled={store.busy} onclick={() => (entering = false)}>
          Cancel
        </button>
      </div>
    {:else}
      <div class="actions">
        <!-- The good gesture where it exists (a phone), and the primary one
             there: pointing a camera at a screen beats reading 105 characters
             out. Absent everywhere else, camera and all — the shell answers for
             the device (store.canScan). -->
        {#if store.canScan}
          <button
            class="primary"
            {disabled}
            onclick={() => store.scanPairingCode()}
          >
            Scan a code
          </button>
        {/if}
        <button
          disabled={disabled || stalled}
          onclick={() => store.showPairingCode()}
        >
          Show a code
        </button>
        <button {disabled} onclick={() => (entering = true)}>
          Enter a code…
        </button>
      </div>
    {/if}

    {#if stalled}
      <p class="muted">
        The server is unreachable — showing a code will wait for it. Reading a
        code from a device on this network still works.
      </p>
    {/if}
  </div>
{/if}

<style>
  .pair {
    display: grid;
    gap: 0.5rem;
    justify-items: start;
    width: 100%;
  }

  p {
    margin: 0;
  }

  .muted {
    color: var(--muted);
    max-width: 34rem;
  }

  input {
    width: 100%;
    max-width: 26rem;
    font-family: ui-monospace, monospace;
    font-size: 0.8rem;
  }

  /* Three buttons on a phone's width: they wrap rather than overflow. */
  .actions {
    display: flex;
    flex-wrap: wrap;
    gap: 0.4rem;
  }
</style>
