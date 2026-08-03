<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com> -->

<script lang="ts">
  import type { Snippet } from "svelte";
  import type { CoreStore } from "../lib/store.svelte";

  // The recovery code, typed in. Shared by the two screens that ask for it,
  // because it is one gesture and not two: give THIS device the account key. A
  // device that has no account yet meets it in the portal; a device that has the
  // account but not its key meets it on the Account screen. Same call, same
  // outcome — `holds_key` becomes true and the pairing gestures light up.
  //
  // `extra` is for whatever else belongs on the button row (the portal's way
  // back to its first question), so the row stays one row.
  let {
    store,
    label,
    extra,
  }: { store: CoreStore; label: string; extra?: Snippet } = $props();

  let code = $state("");
  // The Core refuses `account.join` while a CONFIGURED server is unreachable
  // (`SERVER_UNREACHABLE`: the attestation has to be published). Disarmed rather
  // than inviting the failure — but with no server in this device's life there
  // is nothing to publish to, and the code is accepted as it stands.
  const disabled = $derived(
    store.connection.status !== "connected" ||
      store.busy ||
      !(store.session?.server_connected === true || store.serverless),
  );

  async function join() {
    const entered = code.trim();
    if (!entered) return;
    // Cleared only on success: a refused code stays on screen to be corrected,
    // and the store's banner says why.
    if (await store.joinAccount(entered)) code = "";
  }
</script>

<p class="muted">{label}</p>
<input
  bind:value={code}
  aria-label="Recovery code"
  placeholder="recovery code"
  autocapitalize="characters"
  autocorrect="off"
  spellcheck="false"
  onkeydown={(e) => {
    if (e.key === "Enter" && !disabled) void join();
  }}
/>
<div class="actions">
  <button class="primary" disabled={disabled || !code.trim()} onclick={join}>
    Join
  </button>
  {@render extra?.()}
</div>

<style>
  p {
    margin: 0;
  }

  .muted {
    color: var(--muted);
  }

  input {
    width: 100%;
    max-width: 26rem;
    font-family: ui-monospace, monospace;
  }

  .actions {
    display: flex;
    flex-wrap: wrap;
    gap: 0.4rem;
  }
</style>
