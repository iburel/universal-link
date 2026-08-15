<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com> -->

<script lang="ts">
  import type { Device } from "../lib/api";
  import {
    platformLabel,
    relativeTime,
    selfLabel,
    sortDevices,
  } from "../lib/format";
  import { shareSummary, type CoreStore, type Transfer } from "../lib/store.svelte";
  import LinkDevice from "./LinkDevice.svelte";

  // `now` is a parameter: tests don't have to freeze the clock.
  let { store, now = new Date() }: { store: CoreStore; now?: Date } = $props();

  const devices = $derived(sortDevices(store.devices));
  const disabled = $derived(store.connection.status !== "connected" || store.busy);

  // Adding a device is offered where the devices are — but only by a device that
  // can actually vouch: it holds the account key AND is in the account. That is
  // the Core's own definition, session or none — a serverless sponsor shows a
  // `1D2` code, a signed-in one pairs through its server. A device without the
  // key would be offered the gesture and then be told by the Core that it is the
  // one JOINING, which is not what "add a device" means. The Account screen
  // offers it that, in those words.
  const canVouch = $derived(
    store.account?.attested === true && store.account.holds_key === true,
  );

  // No session means no server to carry a gesture: a sibling's name is its own
  // signed word (only a server may overrule it), and a device cannot strike
  // ITSELF from the account (a tombstone has no way back — the Core would
  // refuse with CANNOT_REVOKE_SELF). The buttons those leave are honest ones.
  const sessionless = $derived(store.session?.logged_in !== true);

  // Files shared from the Android share sheet, waiting for a destination: the
  // list becomes a picker (one tap = one send), and the management actions step
  // aside for it. Always null on the desktop.
  const share = $derived(store.pendingShare);

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

  // The picker replaces the management actions, Save and Cancel included: a
  // rename or a revocation left half-open when a share arrives would have no
  // button left to finish it.
  $effect(() => {
    if (share) {
      editing = null;
      confirming = null;
    }
  });

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
    return device.reachable ? null : relativeTime(device.last_seen, now);
  }

  /**
   * "Windows · this PC · online". Joined here rather than woven out of `{#if}`
   * blocks in the markup: Svelte trims the whitespace at a block's edges, so
   * that spelling lost the space BEFORE each separator ("Windows· this PC").
   *
   * Presence is the Core's `reachable` verdict; "on this network" names the
   * machines this one hears itself (mDNS) — the ones that keep working when
   * the internet does not. "reachable" names the third case: nobody vouches
   * that the device is up right now, but its record carries a route worth
   * trying (its signed addresses or relay); calling that one "online" would
   * claim a liveness nobody stated.
   */
  function meta(device: Device): string {
    const parts = [platformLabel(device.platform)];
    if (device.is_self) parts.push(selfLabel(device.platform));
    if (device.reachable)
      parts.push(device.lan ? "on this network" : device.online ? "online" : "reachable");
    else {
      const last = seen(device);
      if (last) parts.push(`last seen ${last}`);
    }
    return parts.join(" · ");
  }
</script>

<section>
  {#if share}
    <div class="share" role="status">
      <div class="what">
        <strong>Send to…</strong>
        <span class="meta">{shareSummary(share.files)}</span>
      </div>
      <button aria-label="Cancel the share" onclick={() => store.cancelShare()}>
        Cancel
      </button>
    </div>
  {/if}

  <h1>Devices</h1>

  {#if !store.primed}
    <p class="muted">Connecting to Core…</p>
  {:else if !store.session?.logged_in && store.account?.attested !== true}
    <!-- Not attested either: this Core knows of no device at all. A device IN
         the account has a directory whatever its session — its own record at
         least, plus what the account taught it — so it gets the list below. -->
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
              class:online={device.reachable}
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
              <span class="meta">{meta(device)}</span>
            </div>

            <div class="actions">
              {#if share}
                <!-- This phone cannot be a destination for its own share; an
                     offline one is shown but not offered (the Core would refuse
                     it with DEVICE_OFFLINE). -->
                {#if !device.is_self}
                  <button
                    disabled={disabled || !store.targetFor(device.device_id)}
                    aria-label="Send to {device.name}"
                    onclick={() => store.sendShare(device.device_id)}
                    >Send</button
                  >
                {/if}
              {:else if editing === device.device_id}
                <button {disabled} onclick={() => commitRename(device)}>
                  Save
                </button>
                <button onclick={() => (editing = null)}>Cancel</button>
              {:else if confirming === device.device_id}
                <span class="confirm">
                  {#if device.is_self}
                    Revoking {selfLabel(device.platform)} will disconnect it
                    from your account.
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
                {#if device.is_self || !sessionless}
                  <button
                    {disabled}
                    aria-label="Rename {device.name}"
                    onclick={() => startRename(device)}>Rename</button
                  >
                {/if}
                {#if !device.is_self || !sessionless}
                  <button
                    {disabled}
                    aria-label="Revoke {device.name}"
                    onclick={() => {
                      editing = null;
                      confirming = device.device_id;
                    }}>Revoke</button
                  >
                {/if}
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

    <!-- Not while a share is waiting for a destination: that question owns the
         list until it is answered (mobile). -->
    {#if canVouch && !share}
      <h2>Add a device</h2>
      <LinkDevice {store} mode="sponsor" />
    {/if}
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

  h2 {
    margin: 0.5rem 0 0;
    font-size: 1rem;
  }

  /* The pending share: what the taps below are about. */
  .share {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 1rem;
    padding: 0.6rem 0.75rem;
    background: var(--panel);
    border: 1px solid var(--accent);
    border-radius: var(--radius);
  }

  .share .what {
    display: grid;
    min-width: 0;
  }

  .share .meta {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
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

  /* A long device name must not push the row's buttons off a phone screen. The
     confirmation text is deliberately left out: it is a sentence, and it wraps. */
  .identity .name,
  .identity .meta {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
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

  /* Phone: a row cannot hold a name, a state AND two buttons side by side —
     measured on the device, where "Android · this phone · online" came out as
     "Android · this pho…". The actions take a line of their own, which gives the
     identity the full width and the buttons a proper tap size. */
  @media (max-width: 600px) {
    .row {
      flex-wrap: wrap;
    }

    .actions {
      flex: 1 0 100%;
      justify-content: flex-end;
    }
  }
</style>
