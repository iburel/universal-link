<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com> -->

<!--
  The Input tab: where your screens are, who may drive this computer, and where
  your keyboard is right now.

  Everything shown here comes from the `input.*` snapshot and its topic. The one
  thing computed in this file is the arrangement a human is DRAGGING, which is
  not state until they let go of it (and even then the engine's answer is what
  comes back). Every sentence lives in lib/input.ts, next to the code it
  translates.
-->

<script lang="ts">
  import type { InputPeer, InputSpot } from "../lib/api";
  import {
    GHOST_SENTENCE,
    SNAP,
    blockKeys,
    crossings,
    dropRefusal,
    dropSpots,
    guardWords,
    guardsFor,
    hereProblemSentence,
    hotkeyLabel,
    nodeOfSpot,
    pathLine,
    peerProblemSentence,
    planeBounds,
    pointerVerdict,
    reimportBlock,
    sessionSentence,
    slowPathWarning,
    type Crossing,
  } from "../lib/input";
  import type { CoreStore } from "../lib/store.svelte";

  let { store }: { store: CoreStore } = $props();

  // The nominal size of the plane, in CSS pixels. The plane is in logical
  // pixels of a real desk, so it is drawn scaled to fit this box; a drag is
  // converted back through the same factor. A fixed nominal size (rather than a
  // measured one) keeps the arithmetic honest with no layout to read, and the
  // one measurement taken is the box's real width, to survive the CSS shrinking
  // it on a narrow window.
  const PLANE_W = 620;
  const PLANE_H = 300;

  const state = $derived(store.input);
  const spots = $derived<InputSpot[]>(state?.plane.spots ?? []);
  const bounds = $derived(planeBounds(spots));
  // Fit the whole plane in the box, with a little air, and never magnify: a
  // single 1920 wide screen drawn 3 times its size would be a lie about the desk.
  const scale = $derived(
    bounds.w > 0 && bounds.h > 0
      ? Math.min(PLANE_W / bounds.w, PLANE_H / bounds.h, 1)
      : 1,
  );

  const peers = $derived<InputPeer[]>(state?.devices ?? []);
  const hereId = $derived(state?.here.device_id ?? null);
  const ourKeys = $derived(
    spots.filter((s) => hereId !== null && s.device_id === hereId).map((s) => s.monitor),
  );
  const ourCrossings = $derived(crossings(spots, ourKeys));
  const ghosts = $derived(spots.filter((s) => !s.present));

  const hereSentence = $derived(
    state ? hereProblemSentence(state.here, store.selfPlatform) : null,
  );
  const live = $derived(
    state
      ? sessionSentence(
          state.session,
          store.inputName(state.session?.device_id),
          state.hotkey,
        )
      : null,
  );
  const disabled = $derived(
    store.connection.status !== "connected" || store.busy || state === null,
  );

  // What a drag is moving: a whole machine's block, or one screen when the
  // person has said so. A laptop's external screen may genuinely sit next to
  // another computer, which is what detaching one is for.
  let detach = $state(false);
  let selected = $state<string | null>(null);
  let drag = $state<{ keys: string[]; dx: number; dy: number } | null>(null);
  let refused = $state<string | null>(null);

  function keysFor(key: string): string[] {
    return detach ? [key] : blockKeys(spots, key);
  }

  /** Where a spot is drawn, in CSS pixels of the box. */
  function box(spot: InputSpot): string {
    const moving = drag?.keys.includes(spot.monitor) ? drag : null;
    const x = (spot.x - bounds.x + (moving?.dx ?? 0)) * scale;
    const y = (spot.y - bounds.y + (moving?.dy ?? 0)) * scale;
    return `left:${x}px;top:${y}px;width:${spot.w * scale}px;height:${spot.h * scale}px`;
  }

  /**
   * A drag, in CSS pixels of the mouse, converted to the plane's own pixels. The
   * box's real width is read once per drag: CSS may have shrunk the nominal size
   * on a narrow window, and a factor of 1 (a box with no layout, as in a test)
   * is the honest fallback.
   */
  function startDrag(event: MouseEvent, spot: InputSpot) {
    if (disabled) return;
    selected = spot.monitor;
    const keys = keysFor(spot.monitor);
    const element = event.currentTarget as HTMLElement | null;
    const plane = element?.parentElement?.getBoundingClientRect();
    const factor = plane && plane.width > 0 ? PLANE_W / plane.width : 1;
    const from = { x: event.clientX, y: event.clientY };
    drag = { keys, dx: 0, dy: 0 };

    const move = (e: MouseEvent) => {
      drag = {
        keys,
        dx: ((e.clientX - from.x) * factor) / scale,
        dy: ((e.clientY - from.y) * factor) / scale,
      };
    };
    const up = () => {
      window.removeEventListener("mousemove", move);
      window.removeEventListener("mouseup", up);
      const moved = drag;
      drag = null;
      if (!moved || (moved.dx === 0 && moved.dy === 0)) return;
      void commit(moved.keys, moved.dx, moved.dy);
    };
    window.addEventListener("mousemove", move);
    window.addEventListener("mouseup", up);
  }

  /** Arrow keys, for whoever is not holding a mouse: one screen, or a nudge. */
  function nudge(event: KeyboardEvent, spot: InputSpot) {
    const step = event.shiftKey ? SNAP : 0;
    const dx = step || spot.w;
    const dy = step || spot.h;
    const by: Record<string, [number, number]> = {
      ArrowLeft: [-dx, 0],
      ArrowRight: [dx, 0],
      ArrowUp: [0, -dy],
      ArrowDown: [0, dy],
    };
    const delta = by[event.key];
    if (!delta || disabled) return;
    event.preventDefault();
    selected = spot.monitor;
    void commit(keysFor(spot.monitor), delta[0], delta[1]);
  }

  /**
   * The drop. Refused here rather than sent to be refused when it would put two
   * screens in the same place (the engine derives no crossing from overlapping
   * rectangles, so the drop would quietly take away the crossing the person was
   * making) or off the plane.
   */
  async function commit(keys: string[], dx: number, dy: number) {
    const outcome = dropSpots(spots, keys, dx, dy);
    if (!outcome.ok) {
      refused = dropRefusal(outcome.reason);
      return;
    }
    refused = null;
    await store.placeScreens(outcome.spots);
  }

  /** Put a machine's screens back the way that machine itself has them. */
  async function reimport(peer: InputPeer) {
    refused = null;
    await store.placeScreens(reimportBlock(spots, peer.device_id, peer.monitors));
  }

  /** The machines that have a spot on the plane, in the order they are drawn. */
  const blocks = $derived.by(() => {
    const seen = new Map<string, { device_id: string | null; name: string }>();
    for (const spot of spots) {
      const node = nodeOfSpot(spot.monitor);
      if (seen.has(node)) continue;
      seen.set(node, {
        device_id: spot.device_id,
        name:
          (spot.device_id === hereId ? state?.here.name : null) ??
          store.inputName(spot.device_id) ??
          "Another computer",
      });
    }
    return [...seen.entries()].map(([node, who]) => ({ node, ...who }));
  });

  function nameOfSpot(spot: InputSpot): string {
    return blocks.find((b) => b.node === nodeOfSpot(spot.monitor))?.name ?? "";
  }

  const STATES: Record<string, string> = {
    off: "Not connected",
    warming: "Getting ready",
    ready: "Ready",
    driving: "Your keyboard and mouse are there",
    driven: "It is using your keyboard and mouse",
    refused: "Refused",
  };

  /** The crossings toward one machine: the guards are per crossing. */
  function toward(device_id: string): Crossing[] {
    return ourCrossings.filter((c) => c.device_id === device_id);
  }

  /** One guard change, applied to every crossing toward that machine. */
  async function guard(peer: InputPeer, change: Record<string, number | boolean>) {
    for (const crossing of toward(peer.device_id)) {
      const stored = guardsFor(state?.guards ?? [], crossing);
      await store.setGuards(peer.device_id, crossing.to, crossing.side, {
        dwell_ms: stored?.dwell_ms ?? 250,
        double_tap_ms: stored?.double_tap_ms ?? 0,
        dead_corner: stored?.dead_corner ?? 16,
        require_mods: stored?.require_mods ?? 0,
        wall: stored?.wall ?? false,
        ...change,
      });
    }
  }

  /** What the guards toward one machine do, said once for the whole pair. */
  function guardSummary(peer: InputPeer): string[] {
    const crossing = toward(peer.device_id)[0];
    if (!crossing) return [];
    return guardWords(guardsFor(state?.guards ?? [], crossing) ?? {});
  }

  function firstGuards(peer: InputPeer) {
    const crossing = toward(peer.device_id)[0];
    return crossing ? guardsFor(state?.guards ?? [], crossing) : undefined;
  }

  /**
   * Taking the pointer across a slow path warns first, and the offer alongside
   * the warning is the keyboard alone. Above the threshold the engine refuses
   * the pointer outright (`INPUT_TOO_SLOW`), so the warning is the honest step
   * before a refusal the person would otherwise walk into.
   */
  let confirming = $state<string | null>(null);

  function askOrTake(peer: InputPeer) {
    const verdict = pointerVerdict(peer.rtt_ms);
    if (verdict === "announce" || verdict === "refuse") {
      confirming = peer.device_id;
      return;
    }
    void store.takeInput(peer.device_id);
  }

  const HOTKEYS: readonly { keys: string[]; label: string }[] = [
    { keys: ["ctrl", "alt", "Escape"], label: "Ctrl + Alt + Escape" },
    { keys: ["ctrl", "shift", "Home"], label: "Ctrl + Shift + Home" },
    { keys: ["ctrl", "alt", "F12"], label: "Ctrl + Alt + F12" },
  ];
</script>

<section class="input">
  <h1>Keyboard and mouse</h1>

  {#if !store.primed}
    <p class="muted">Connecting to Core…</p>
  {:else if state === null}
    <!-- The section survives an engine that has gone quiet, and says so. Taking
         it away under someone who is reading it would explain nothing. -->
    <p class="muted">
      The keyboard and mouse engine is not running on this computer right now.
      1Device starts it again by itself; nothing here can be shown or changed
      until it answers.
    </p>
  {:else}
    <p class="live" class:away={state.session !== null} role="status">
      {live ?? "Your keyboard and mouse are on this computer."}
    </p>
    {#if state.session?.direction === "out"}
      <button {disabled} onclick={() => store.releaseInput()}>
        Bring them back
      </button>
    {/if}

    {#if hereSentence}
      <p class="banner error" role="alert">{hereSentence}</p>
    {/if}

    <h2>Where your screens are</h2>
    <div
      class="plane"
      style="width:{PLANE_W}px;height:{PLANE_H}px"
      aria-label="The screens of your computers"
    >
      {#each spots as spot (spot.monitor)}
        <button
          class="screen"
          class:ghost={!spot.present}
          class:mine={spot.device_id === hereId}
          class:selected={selected === spot.monitor}
          style={box(spot)}
          aria-label="{nameOfSpot(spot)} {spot.name || 'screen'}"
          onmousedown={(e) => startDrag(e, spot)}
          onkeydown={(e) => nudge(e, spot)}
        >
          <span class="who">{nameOfSpot(spot)}</span>
          <span class="what">
            {spot.name || "screen"}{spot.primary ? " (main)" : ""}
          </span>
          {#if spot.device_id === hereId && spot.primary}
            <span class="here">you are here</span>
          {/if}
          {#if !spot.present}<span class="what">away</span>{/if}
        </button>
      {/each}
    </div>

    <p class="hint">
      Drag a screen to say where it really is. A computer's screens move together;
      tick the box below to move one on its own. Arrow keys move the selection by
      one screen, and hold Shift for a nudge.
    </p>
    <label class="row">
      <input
        type="checkbox"
        bind:checked={detach}
        aria-label="Move one screen at a time"
      />
      Move one screen at a time
    </label>
    {#if refused}
      <p class="banner error" role="alert">{refused}</p>
    {/if}
    {#each ghosts as ghost (ghost.monitor)}
      <p class="muted">
        {nameOfSpot(ghost)}{ghost.name ? `, ${ghost.name}` : ""}: {GHOST_SENTENCE}
      </p>
    {/each}
    {#each blocks.filter((b) => b.device_id !== null && b.device_id !== hereId) as block (block.node)}
      {@const peer = peers.find((p) => p.device_id === block.device_id)}
      {#if peer && peer.monitors.length > 1}
        <button class="link" {disabled} onclick={() => reimport(peer)}>
          Put {block.name}'s screens back the way that computer has them
        </button>
      {/if}
    {/each}

    <h2>Who may drive this computer</h2>
    <p class="muted">
      This is the list that decides. A computer can type here only if it is
      ticked here, on this computer, and this list is never sent anywhere.
    </p>
    <ul class="switches">
      {#each peers as peer (peer.device_id)}
        <li>
          <label>
            <input
              type="checkbox"
              checked={peer.allowed}
              {disabled}
              aria-label="Let {peer.name} drive this computer"
              onchange={(e) =>
                store.allowInput(
                  peer.device_id,
                  (e.currentTarget as HTMLInputElement).checked,
                )}
            />
            {peer.name}
          </label>
        </li>
      {/each}
      {#if peers.length === 0}
        <li class="muted">No other computer on your account yet.</li>
      {/if}
    </ul>

    <h2>Who this computer may drive</h2>
    <p class="muted">
      A shortlist for this computer's own use: it says which computers it will
      offer to drive. It grants nothing over there. Each of those computers keeps
      its own list, and this one finds out by trying.
    </p>
    <ul class="switches">
      {#each peers as peer (peer.device_id)}
        <li>
          <label>
            <input
              type="checkbox"
              checked={peer.drive}
              {disabled}
              aria-label="Let this computer drive {peer.name}"
              onchange={(e) =>
                store.driveInput(
                  peer.device_id,
                  (e.currentTarget as HTMLInputElement).checked,
                )}
            />
            {peer.name}
          </label>
        </li>
      {/each}
    </ul>

    {#if peers.length > 0}
      <h2>Each computer</h2>
      <ul class="peers">
        {#each peers as peer (peer.device_id)}
          {@const problem = peerProblemSentence(peer.problem, peer.name)}
          {@const path = pathLine(peer)}
          <li>
            <div class="row">
              <span class="name">{peer.name}</span>
              <span class="meta">{STATES[peer.state] ?? peer.state}</span>
            </div>
            {#if path}<p class="meta">{path}</p>{/if}
            {#if problem}<p class="problem">{problem}</p>{/if}

            {#if peer.state === "driving"}
              <button {disabled} onclick={() => store.releaseInput()}>
                Bring my keyboard back
              </button>
            {:else if confirming === peer.device_id}
              <p class="confirm">
                {slowPathWarning(peer.name, peer.rtt_ms ?? 0)}
              </p>
              <button
                {disabled}
                aria-label="Send the keyboard alone to {peer.name}"
                onclick={() => {
                  confirming = null;
                  void store.takeInput(peer.device_id, "keys");
                }}>Keyboard only</button
              >
              {#if pointerVerdict(peer.rtt_ms) !== "refuse"}
                <button
                  {disabled}
                  aria-label="Take the pointer to {peer.name} anyway"
                  onclick={() => {
                    confirming = null;
                    void store.takeInput(peer.device_id, "full");
                  }}>Take it anyway</button
                >
              {/if}
              <button onclick={() => (confirming = null)}>Cancel</button>
            {:else}
              <button
                {disabled}
                aria-label="Take control of {peer.name}"
                onclick={() => askOrTake(peer)}>Take control</button
              >
              <button
                {disabled}
                aria-label="Send the keyboard alone to {peer.name}"
                onclick={() => store.takeInput(peer.device_id, "keys")}
                >Keyboard only</button
              >
            {/if}

            {#if toward(peer.device_id).length > 0}
              {@const stored = firstGuards(peer)}
              <div class="guards">
                <p class="meta">When the pointer crosses to {peer.name}:</p>
                <ul>
                  {#each guardSummary(peer) as said}
                    <li>{said}</li>
                  {/each}
                </ul>
                <label>
                  <input
                    type="checkbox"
                    checked={stored?.wall ?? false}
                    {disabled}
                    aria-label="Never cross to {peer.name}"
                    onchange={(e) =>
                      guard(peer, {
                        wall: (e.currentTarget as HTMLInputElement).checked,
                      })}
                  />
                  Never cross to this computer
                </label>
                <label>
                  <input
                    type="checkbox"
                    checked={(stored?.double_tap_ms ?? 0) > 0}
                    {disabled}
                    aria-label="Ask for a double tap toward {peer.name}"
                    onchange={(e) =>
                      guard(peer, {
                        double_tap_ms: (e.currentTarget as HTMLInputElement)
                          .checked
                          ? 300
                          : 0,
                      })}
                  />
                  Only after the pointer leaves the edge and comes straight back
                </label>
                <label>
                  <input
                    type="checkbox"
                    checked={(stored?.dead_corner ?? 16) > 0}
                    {disabled}
                    aria-label="Keep the corners free toward {peer.name}"
                    onchange={(e) =>
                      guard(peer, {
                        dead_corner: (e.currentTarget as HTMLInputElement)
                          .checked
                          ? 16
                          : 0,
                      })}
                  />
                  Leave the corners alone, for menus and hot corners
                </label>
              </div>
            {:else}
              <p class="meta">
                No edge of your screens touches this computer's on the plane, so
                the pointer cannot cross to it. Drag them next to each other
                above.
              </p>
            {/if}
          </li>
        {/each}
      </ul>
    {/if}

    <h2>This computer</h2>
    <label class="row">
      <input
        type="checkbox"
        checked={state.lock}
        {disabled}
        aria-label="Keep the pointer on this screen"
        onchange={(e) =>
          store.lockPointer((e.currentTarget as HTMLInputElement).checked)}
      />
      Keep the pointer on this computer's screens, whatever the layout says
    </label>
    <p class="muted">
      For a game or a virtual machine. Nothing crosses in either direction while
      it is on.
    </p>
    <label class="row">
      Bring my keyboard back with
      <select
        {disabled}
        aria-label="The key that brings your keyboard back"
        value={hotkeyLabel(state.hotkey)}
        onchange={(e) => {
          const label = (e.currentTarget as HTMLSelectElement).value;
          const chosen = HOTKEYS.find((chord) => chord.label === label);
          if (chosen) void store.setHotkey(chosen.keys);
        }}
      >
        <!-- A chord set from somewhere else (a third-party interface) is listed
             as it is rather than silently replaced by one of these three. -->
        {#if !HOTKEYS.some((chord) => chord.label === hotkeyLabel(state.hotkey))}
          <option value={hotkeyLabel(state.hotkey)}>
            {hotkeyLabel(state.hotkey)}
          </option>
        {/if}
        {#each HOTKEYS as chord (chord.label)}
          <option value={chord.label}>{chord.label}</option>
        {/each}
      </select>
    </label>
    <p class="muted">
      This computer enforces it on its own: a computer you are driving is never
      asked, so one that hangs cannot keep your keyboard.
    </p>
  {/if}
</section>

<style>
  section {
    display: grid;
    gap: 0.75rem;
    justify-items: start;
  }

  h2 {
    margin: 0.5rem 0 0;
    font-size: 1rem;
  }

  p {
    margin: 0;
  }

  .muted,
  .meta,
  .hint {
    color: var(--muted);
    font-size: 0.85rem;
  }

  .live {
    font-weight: 500;
  }

  .live.away {
    color: var(--accent);
  }

  .problem,
  .confirm {
    font-size: 0.85rem;
    color: var(--warn);
  }

  .banner {
    padding: 0.5rem 0.75rem;
    border-radius: var(--radius);
    border: 1px solid var(--line);
    background: var(--panel);
  }

  .banner.error {
    border-color: var(--danger);
    color: var(--danger);
  }

  /* The plane: one box, the screens placed inside it in its own pixels. A fixed
     nominal size, shrunk by CSS on a narrow window (the drag reads the real
     width once, so the arithmetic follows). */
  .plane {
    position: relative;
    max-width: 100%;
    background: var(--nav);
    border: 1px solid var(--line);
    border-radius: var(--radius);
  }

  .screen {
    position: absolute;
    display: grid;
    align-content: center;
    gap: 0.1rem;
    overflow: hidden;
    padding: 0.2rem;
    font-size: 0.75rem;
    text-align: center;
    background: var(--panel);
    border: 1px solid var(--line);
    border-radius: 4px;
    cursor: grab;
  }

  .screen.mine {
    border-color: var(--accent);
  }

  .screen.selected {
    box-shadow: 0 0 0 2px var(--accent);
  }

  /* A screen that is away keeps its place, and says so rather than pretending. */
  .screen.ghost {
    border-style: dashed;
    color: var(--muted);
    background: transparent;
    cursor: grab;
  }

  .who {
    font-weight: 500;
  }

  .what,
  .here {
    color: var(--muted);
  }

  ul {
    list-style: none;
    margin: 0;
    padding: 0;
    display: grid;
    gap: 0.35rem;
  }

  .peers > li {
    display: grid;
    justify-items: start;
    gap: 0.35rem;
    padding: 0.6rem 0.75rem;
    background: var(--panel);
    border: 1px solid var(--line);
    border-radius: var(--radius);
  }

  .row {
    display: flex;
    align-items: center;
    gap: 0.5rem;
  }

  .name {
    font-weight: 500;
  }

  .guards {
    display: grid;
    justify-items: start;
    gap: 0.2rem;
    font-size: 0.85rem;
    color: var(--muted);
  }

  .guards ul {
    gap: 0.1rem;
  }

  .link {
    border: none;
    background: none;
    padding: 0;
    color: var(--accent);
    text-decoration: underline;
    font-size: 0.85rem;
  }
</style>
