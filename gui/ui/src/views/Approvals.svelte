<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com> -->

<script lang="ts">
  import type { PendingRequest } from "../lib/api";
  import { roleLabel, scopeLabel } from "../lib/format";
  import type { CoreStore } from "../lib/store.svelte";

  /**
   * The Core refuses to grant this scope via the prompt, whatever the request:
   * it can only be obtained through a spawn token or the file token. We show it
   * struck through rather than hiding it — the requester asked for it.
   */
  const NEVER_GRANTED = "components.approve";

  let { store }: { store: CoreStore } = $props();

  const disabled = $derived(store.connection.status !== "connected" || store.busy);

  /** Checked scopes, per request. Absent ⇒ all the ones we're allowed to grant. */
  let chosen = $state<Record<string, string[]>>({});

  function scopesFor(request: PendingRequest): string[] {
    return (
      chosen[request.request_id] ??
      request.scopes.filter((s) => s !== NEVER_GRANTED)
    );
  }

  function toggle(request: PendingRequest, scope: string) {
    const current = scopesFor(request);
    chosen[request.request_id] = current.includes(scope)
      ? current.filter((s) => s !== scope)
      : [...current, scope];
  }

  function origin(request: PendingRequest): string {
    const { exe, pid } = request.peer_info;
    if (exe && pid !== undefined) return `${exe} (pid ${pid})`;
    if (exe) return exe;
    if (pid !== undefined) return `pid ${pid}`;
    return "unknown origin";
  }
</script>

<section>
  <header>
    <h1>Approvals</h1>
    <button {disabled} onclick={() => store.resync()}>Refresh</button>
  </header>

  {#if !store.primed}
    <p class="muted">Connecting to Core…</p>
  {:else}
    <h2>Pending requests</h2>
    {#if store.pending.length === 0}
      <p class="muted">No pending requests.</p>
    {:else}
      <ul class="requests">
        {#each store.pending as request (request.request_id)}
          <li>
            <p class="title">
              <strong>{request.name}</strong> requests the
              “{roleLabel(request.role)}” role
            </p>
            <p class="muted">{origin(request)}</p>

            <ul class="scopes">
              {#each request.scopes as scope (scope)}
                <li class:refused={scope === NEVER_GRANTED}>
                  <label>
                    <input
                      type="checkbox"
                      disabled={scope === NEVER_GRANTED || disabled}
                      checked={scopesFor(request).includes(scope)}
                      onchange={() => toggle(request, scope)}
                    />
                    {scopeLabel(scope)}
                  </label>
                  {#if scope === NEVER_GRANTED}
                    <span class="muted">never granted from this window</span>
                  {/if}
                </li>
              {/each}
            </ul>

            <div class="actions">
              <button
                class="primary"
                {disabled}
                aria-label="Approve {request.name}"
                onclick={() => store.approve(request.request_id, scopesFor(request))}
                >Approve</button
              >
              <button
                {disabled}
                aria-label="Deny {request.name}"
                onclick={() => store.deny(request.request_id)}>Deny</button
              >
            </div>
          </li>
        {/each}
      </ul>
    {/if}

    <h2>Enrolled components</h2>
    {#if store.components.length === 0}
      <p class="muted">No components.</p>
    {:else}
      <ul class="components">
        {#each store.components as component (component.component_id)}
          <li>
            <span
              class="dot"
              class:online={component.connected}
              aria-hidden="true"
            ></span>
            <div class="identity">
              <span>{component.name}</span>
              <span class="muted">
                {roleLabel(component.role)} &middot;
                {component.connected ? "connected" : "disconnected"} &middot;
                {component.scopes.map(scopeLabel).join(", ") || "no scopes"}
              </span>
            </div>
            {#if component.enrolled === true}
              <button
                class="danger"
                {disabled}
                aria-label="Revoke {component.name}"
                onclick={() => store.revokeComponent(component.component_id)}
                >Revoke</button
              >
            {:else}
              <!-- Bootstrap (file token or spawn token): no persistent token to
                   revoke, `components.revoke` would only close its connection.
                   The role does not let us tell. -->
              <span class="muted">local connection</span>
            {/if}
          </li>
        {/each}
      </ul>
    {/if}
  {/if}
</section>

<style>
  section {
    display: grid;
    gap: 0.75rem;
  }

  header {
    display: flex;
    align-items: center;
    justify-content: space-between;
  }

  h2 {
    font-size: 0.8rem;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--muted);
    margin-top: 0.5rem;
  }

  p {
    margin: 0;
  }

  .muted {
    color: var(--muted);
    font-size: 0.85rem;
  }

  ul {
    list-style: none;
    margin: 0;
    padding: 0;
    display: grid;
    gap: 0.5rem;
  }

  .requests > li,
  .components > li {
    padding: 0.75rem;
    background: var(--panel);
    border: 1px solid var(--line);
    border-radius: var(--radius);
  }

  .requests > li {
    display: grid;
    gap: 0.4rem;
  }

  .scopes {
    gap: 0.15rem;
  }

  .scopes label {
    display: inline-flex;
    align-items: center;
    gap: 0.4rem;
  }

  .scopes li.refused {
    color: var(--muted);
    text-decoration: line-through;
  }

  .scopes li.refused .muted {
    text-decoration: none;
  }

  .actions {
    display: flex;
    gap: 0.4rem;
    margin-top: 0.25rem;
  }

  .components > li {
    display: flex;
    align-items: center;
    gap: 0.75rem;
  }

  .identity {
    display: grid;
    flex: 1;
    min-width: 0;
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
</style>
