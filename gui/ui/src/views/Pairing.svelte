<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com> -->

<script lang="ts">
  import { platformLabel, validFor } from "../lib/format";
  import { qrMatrix, qrPath } from "../lib/qr";
  import type { CoreStore } from "../lib/store.svelte";

  // A pairing under way takes the whole window (App mounts this as a portal), for
  // two reasons: it is a gesture with a deadline, and the confirmation MUST be
  // seen — a screen the user could navigate away from would leave the other
  // device waiting out the deadline for nothing.
  let { store }: { store: CoreStore } = $props();

  const flow = $derived(store.pairing);

  // The code as a symbol. Recomputed only when the code changes; a code too long
  // for any symbol (it never is — see qr.test.ts) leaves the text code alone on
  // screen rather than taking the screen down.
  const symbol = $derived.by(() => {
    const code = flow?.code;
    if (!code) return null;
    try {
      const matrix = qrMatrix(code);
      return { size: matrix.length, path: qrPath(matrix) };
    } catch {
      return null;
    }
  });

  const validity = $derived(validFor(flow?.expires_in ?? 0));

  const title = $derived.by(() => {
    switch (flow?.phase) {
      case "confirm":
        return "Add this device?";
      case "confirming":
        return "Almost done";
      default:
        return flow?.role === "sponsor" ? "Add a device" : "Link this device";
    }
  });
</script>

{#if flow}
  <section>
    <h1>{title}</h1>

    <!-- This screen covers the whole window, so it carries the banner too: a
         refusal from the Core (the server gone, the pairing already settled)
         would otherwise happen in silence. -->
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

    {#if flow.phase === "showing"}
      <p>
        {flow.role === "sponsor"
          ? "Scan this on the device you want to add."
          : "Scan this on a device that is already on your account."}
      </p>

      {#if symbol}
        <!-- Dark on light whatever the app's theme: a scanner is not asked to
             cope with an inverted symbol. The four modules of margin are the
             quiet zone the standard requires — without them a camera cannot
             find the edges. -->
        <svg
          class="qr"
          viewBox="0 0 {symbol.size + 8} {symbol.size + 8}"
          role="img"
          aria-label="Pairing code as a QR code"
        >
          <rect width="100%" height="100%" fill="#ffffff" />
          <path d={symbol.path} transform="translate(4 4)" fill="#000000" />
        </svg>
      {/if}

      <code class="code">{flow.code}</code>
      <p class="muted">
        No camera on the other device? Copy this code into it instead.
        {#if validity}
          The code is good for about {validity}.
        {/if}
      </p>
    {:else if flow.phase === "waiting"}
      <p>Confirm on your other device to finish linking this one.</p>
      {#if flow.verification}
        <p class="muted">Check it is showing this number:</p>
        <p class="number">{flow.verification}</p>
      {/if}
      <p class="muted">
        Nothing reaches this device until someone confirms it over there.
      </p>
    {:else if flow.phase === "confirm"}
      <dl>
        <dt>Device</dt>
        <dd>{flow.device?.name ?? "an unnamed device"}</dd>
        {#if flow.device?.platform}
          <dt>Platform</dt>
          <dd>{platformLabel(flow.device.platform)}</dd>
        {/if}
      </dl>

      {#if flow.verification}
        <!-- The check that a name cannot make: the number comes out of the
             channel the two ends share, so whoever read the code over the user's
             shoulder is showing a different one — and the name they declare is
             their own to choose. -->
        <p class="number">{flow.verification}</p>
        <p class="banner warn" role="status">
          That device must be showing the same number. If it is not, decline —
          someone else read your code.
        </p>
      {/if}

      <p class="muted">
        Confirming gives it your account key, so it can exchange files with your
        devices — and vouch for the next one in its turn.
      </p>
    {:else}
      <p>
        Finish the confirmation in your browser — your account has to be
        confirmed once more before the key crosses.
      </p>
    {/if}

    <div class="actions">
      {#if flow.phase === "confirm"}
        <button
          class="primary"
          disabled={store.busy}
          onclick={() => store.confirmPairing()}
        >
          Add to my account
        </button>
        <button disabled={store.busy} onclick={() => store.cancelPairing()}>
          Decline
        </button>
      {:else}
        <button disabled={store.busy} onclick={() => store.cancelPairing()}>
          Cancel
        </button>
      {/if}
    </div>
  </section>
{/if}

<style>
  section {
    display: grid;
    gap: 0.75rem;
    place-content: center;
    justify-items: start;
    height: 100%;
    max-width: 32rem;
    margin: 0 auto;
    padding: 2rem;
  }

  h1,
  p {
    margin: 0;
  }

  .muted {
    color: var(--muted);
  }

  /* Sized in rem so the modules stay square and crisp: 41 modules across ~15rem
     is about 6 px each at the default text size, which a phone camera reads
     from a hand's width away. */
  .qr {
    width: 15rem;
    height: 15rem;
    border-radius: var(--radius);
    /* The white tile is part of the symbol (the quiet zone), so it gets the
       border rather than a padded container. */
    border: 1px solid var(--line);
    align-self: center;
    justify-self: center;
  }

  .code {
    display: block;
    width: 100%;
    font-family: ui-monospace, monospace;
    font-size: 0.8rem;
    padding: 0.5rem 0.75rem;
    background: var(--panel);
    border: 1px solid var(--line);
    border-radius: var(--radius);
    user-select: all;
    word-break: break-all;
  }

  /* Read aloud from one screen to another: the number is the point of the
     screen, so it is the biggest thing on it. */
  .number {
    font-family: ui-monospace, monospace;
    font-size: 1.75rem;
    letter-spacing: 0.15em;
    user-select: all;
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

  .actions {
    display: flex;
    gap: 0.4rem;
    margin-top: 0.25rem;
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
