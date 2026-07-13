<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com> -->

<script lang="ts">
  import type { CoreStore } from "../lib/store.svelte";

  let { store }: { store: CoreStore } = $props();

  // Three LOCAL sub-states. The recovery code never enters the store: it's a
  // one-shot secret that the view displays then forgets.
  type Step = "choose" | "join" | "created";
  let step = $state<Step>("choose");
  let code = $state(""); // input for "Join"
  let recoveryCode = $state<string | null>(null); // code shown after "Create"

  // The Core refuses setup/join when the server is disconnected
  // (`SERVER_UNREACHABLE`): we disarm until it's ready rather than invite a failure.
  const serverReady = $derived(store.session?.server_connected === true);
  const disabled = $derived(store.busy || !serverReady);

  async function create() {
    const result = await store.createAccount();
    if (result !== null) {
      recoveryCode = result;
      step = "created";
    }
  }

  async function join() {
    const entered = code.trim();
    if (!entered) return;
    // A success lifts the portal (finishOnboarding internal to the store); on
    // failure we stay here, the banner explains.
    if (await store.joinAccount(entered)) code = "";
  }

  function done() {
    // We do NOT clear recoveryCode here: the portal only lifts (and this
    // component is only unmounted, taking the secret away) once the attestation
    // has been re-read by finishOnboarding's resync. Clearing it now would
    // leave — for the duration of the round-trip, or indefinitely if the Core
    // is unreachable — an EMPTY code box. Unmounting is the only erasure, and
    // it is confirmed.
    store.finishOnboarding();
  }
</script>

<section>
  <h1>Link this device</h1>

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

  {#if step === "created"}
    <p>
      Account created. Here is your <strong>recovery code</strong>:
    </p>
    <code class="recovery">{recoveryCode}</code>
    <p class="muted">
      This is the only copy. It will never be shown again. Write it down: you
      must enter it on your other devices to link them to this account —
      without it, no additional device can join.
    </p>
    <button class="primary" onclick={done}>I've saved the code, continue</button>
  {:else}
    <p class="muted">
      To exchange files in a verified way, this device must join your account.
    </p>

    {#if !serverReady}
      <p class="banner warn" role="status">
        Waiting for the server connection…
      </p>
    {/if}

    {#if step === "join"}
      <p>Enter the code shown on a device already linked to this account.</p>
      <input
        bind:value={code}
        aria-label="Recovery code"
        placeholder="recovery code"
        onkeydown={(e) => {
          if (e.key === "Enter" && !disabled) void join();
        }}
      />
      <div class="actions">
        <button
          class="primary"
          disabled={disabled || !code.trim()}
          onclick={join}>Join</button
        >
        <button disabled={store.busy} onclick={() => (step = "choose")}
          >Back</button
        >
      </div>
    {:else}
      <div class="choices">
        <button class="primary" {disabled} onclick={create}>
          This is my first device
        </button>
        <button {disabled} onclick={() => (step = "join")}>
          I already have a device on this account
        </button>
      </div>
    {/if}

    <button class="link logout" disabled={store.busy} onclick={() => store.logout()}>
      Sign out
    </button>
  {/if}
</section>

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

  h1 {
    margin: 0;
  }

  p {
    margin: 0;
  }

  .muted {
    color: var(--muted);
    max-width: 34rem;
  }

  .recovery {
    display: block;
    font-family: ui-monospace, monospace;
    font-size: 1.25rem;
    letter-spacing: 0.08em;
    padding: 0.6rem 0.9rem;
    background: var(--panel);
    border: 1px solid var(--accent);
    border-radius: var(--radius);
    user-select: all;
    word-break: break-all;
  }

  input {
    width: 100%;
    max-width: 24rem;
  }

  .choices {
    display: grid;
    gap: 0.5rem;
    justify-items: stretch;
  }

  .actions {
    display: flex;
    gap: 0.4rem;
  }

  .link {
    border: none;
    background: none;
    padding: 0;
    color: var(--muted);
    text-decoration: underline;
  }

  .logout {
    margin-top: 0.5rem;
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
