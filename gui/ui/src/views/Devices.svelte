<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com> -->

<script lang="ts">
  import type { Device } from "../lib/api";
  import { platformLabel, relativeTime, sortDevices } from "../lib/format";
  import type { CoreStore, Transfer } from "../lib/store.svelte";

  // `now` is a parameter: tests don't have to freeze the clock.
  let { store, now = new Date() }: { store: CoreStore; now?: Date } = $props();

  const devices = $derived(sortDevices(store.devices));
  const disabled = $derived(store.connection.status !== "connected" || store.busy);

  // One transfer per device on the card: an ACTIVE transfer takes priority (its
  // progress and its cancellation must stay accessible), otherwise the most
  // recent terminal one. Two concurrent sends to the same device are therefore
  // summarized by the one still in progress, not by the last one started.
  const latestTransfer = $derived.by(() => {
    const map = new Map<string, Transfer>();
    for (const transfer of store.transfers) {
      const shown = map.get(transfer.device_id);
      if (transfer.status === "active" || !shown || shown.status !== "active") {
        map.set(transfer.device_id, transfer);
      }
    }
    return map;
  });

  function percent(transfer: Transfer): string {
    if (transfer.total <= 0) return "0%";
    return `${Math.min(100, Math.round((transfer.done / transfer.total) * 100))}%`;
  }

  function sent(transfer: Transfer): string {
    const n = transfer.files.length;
    return n > 1 ? `Sent · ${n} files` : "Sent";
  }

  let editing = $state<string | null>(null);
  let draft = $state("");
  let confirming = $state<string | null>(null);

  function startRename(device: Device) {
    confirming = null;
    editing = device.device_id;
    draft = device.name;
  }

  async function commitRename(device: Device) {
    const name = draft.trim();
    editing = null;
    // An empty or unchanged name has nothing to tell the Core.
    if (!name || name === device.name) return;
    await store.renameDevice(device.device_id, name);
  }

  function seen(device: Device): string | null {
    return device.online ? null : relativeTime(device.last_seen, now);
  }
</script>

<section>
  <h1>Devices</h1>

  {#if !store.primed}
    <p class="muted">Connecting to Core…</p>
  {:else if !store.session?.logged_in}
    <p class="muted">Sign in to see the devices on your account.</p>
  {:else if store.devicesError}
    <p class="muted">Directory unavailable: {store.devicesError}</p>
  {:else}
    <ul>
      {#each devices as device (device.device_id)}
        {@const transfer = latestTransfer.get(device.device_id)}
        <li
          data-device-id={device.device_id}
          class:drop-target={store.dropTarget === device.device_id}
        >
          <div class="row">
            <span
              class="dot"
              class:online={device.online}
              aria-hidden="true"
            ></span>

            <div class="identity">
              {#if editing === device.device_id}
                <input
                  bind:value={draft}
                  aria-label="New name for {device.name}"
                  onkeydown={(e) => {
                    if (e.key === "Enter") void commitRename(device);
                    if (e.key === "Escape") editing = null;
                  }}
                />
              {:else}
                <span class="name">{device.name}</span>
              {/if}
              <span class="meta">
                {platformLabel(device.platform)}{#if device.is_self}
                  &middot; this PC{/if}{#if device.online}
                  &middot; online{:else if seen(device)}
                  &middot; last seen {seen(device)}{/if}
              </span>
            </div>

            <div class="actions">
              {#if editing === device.device_id}
                <button {disabled} onclick={() => commitRename(device)}>
                  Save
                </button>
                <button onclick={() => (editing = null)}>Cancel</button>
              {:else if confirming === device.device_id}
                <span class="confirm">
                  {#if device.is_self}
                    Revoking this PC will disconnect it from your account.
                  {:else}
                    Revoke {device.name}?
                  {/if}
                </span>
                <button
                  class="danger"
                  {disabled}
                  onclick={() => {
                    confirming = null;
                    void store.revokeDevice(device.device_id);
                  }}>Confirm</button
                >
                <button onclick={() => (confirming = null)}>Cancel</button>
              {:else}
                <button
                  {disabled}
                  aria-label="Rename {device.name}"
                  onclick={() => startRename(device)}>Rename</button
                >
                <button
                  {disabled}
                  aria-label="Revoke {device.name}"
                  onclick={() => {
                    editing = null;
                    confirming = device.device_id;
                  }}>Revoke</button
                >
              {/if}
            </div>
          </div>

          {#if transfer}
            <div class="transfer {transfer.status}" role="status">
              {#if transfer.status === "active"}
                <progress max={transfer.total || 1} value={transfer.done}
                ></progress>
                <span class="label">Sending… {percent(transfer)}</span>
                <button
                  class="link"
                  aria-label="Cancel send to {device.name}"
                  onclick={() => store.cancelTransfer(transfer.transfer_id)}
                  >Cancel</button
                >
              {:else}
                <span class="label">
                  {#if transfer.status === "finished"}
                    {sent(transfer)}
                  {:else if transfer.error === "cancelled"}
                    Send cancelled
                  {:else}
                    Send failed: {transfer.error}
                  {/if}
                </span>
                <button
                  class="close"
                  aria-label="Dismiss the transfer to {device.name}"
                  onclick={() => store.dismissTransfer(transfer.transfer_id)}
                  >×</button
                >
              {/if}
            </div>
          {/if}
        </li>
      {/each}
    </ul>
  {/if}
</section>

<style>
  section {
    display: grid;
    gap: 0.75rem;
  }

  .muted {
    color: var(--muted);
    margin: 0;
  }

  ul {
    list-style: none;
    margin: 0;
    padding: 0;
    display: grid;
    gap: 0.5rem;
  }

  li {
    display: grid;
    gap: 0.5rem;
    padding: 0.6rem 0.75rem;
    background: var(--panel);
    border: 1px solid var(--line);
    border-radius: var(--radius);
  }

  /* Target of an in-progress file drag: the only drop affordance. */
  li.drop-target {
    border-color: var(--accent);
    box-shadow: 0 0 0 1px var(--accent);
  }

  .row {
    display: flex;
    align-items: center;
    gap: 0.75rem;
  }

  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--muted);
    flex: none;
  }

  .dot.online {
    background: var(--ok);
  }

  .identity {
    display: grid;
    flex: 1;
    min-width: 0;
  }

  .name {
    font-weight: 500;
  }

  .meta,
  .confirm {
    color: var(--muted);
    font-size: 0.85rem;
  }

  .actions {
    display: flex;
    align-items: center;
    gap: 0.4rem;
  }

  .transfer {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    font-size: 0.85rem;
    color: var(--muted);
  }

  .transfer progress {
    flex: 1;
    height: 4px;
  }

  .transfer.failed .label {
    color: var(--danger);
  }

  .transfer .label {
    flex: 1;
    min-width: 0;
  }

  /* While sending, the bar stretches; the label keeps its width. */
  .transfer.active .label {
    flex: none;
  }

  .transfer .link {
    border: none;
    background: none;
    padding: 0;
    color: var(--accent);
    text-decoration: underline;
  }

  .transfer .close {
    border: none;
    background: none;
    padding: 0 0.25rem;
    line-height: 1;
  }
</style>
