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
    DWELL_CHOICES,
    GHOST_SENTENCE,
    GUARD_DEFAULTS,
    MODS,
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
    tooShortToCross,
    type Crossing,
  } from "../lib/input";
  import type { CoreStore } from "../lib/store.svelte";

  let { store }: { store: CoreStore } = $props();

  // The size of the plane's box, in CSS pixels. The plane is in logical pixels of
  // a real desk, so it is drawn scaled to fit this box, and a drag is converted
  // back through the same scale.
  //
  // The box keeps this width whatever the window does (a scroller around it is
  // what gives way on a narrow one), and that is what makes the arithmetic
  // honest: a box CSS had shrunk would draw the screens over 620 pixels of a
  // 400 pixel pane and move the dragged one faster than the cursor.
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

  // Drawn as tall as the arrangement really is (never taller than the budget,
  // never so short that a single screen has no room): a box of empty space below
  // the screens reads as a plane with somewhere to drag to, and there is not.
  const planeHeight = $derived(
    Math.min(PLANE_H, Math.max(80, Math.round(bounds.h * scale))),
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
  /**
   * How to take a live drag's two window listeners back. Leaving the section
   * mid-drag has to take them with it: without this, the mouse being released
   * afterwards would send a placement from a component nobody is looking at, and
   * every visit would leave another pair of listeners on the window.
   */
  let stopDrag: (() => void) | null = null;
  $effect(() => () => stopDrag?.());

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
   * A drag, in CSS pixels of the mouse, converted to the plane's own pixels
   * through the scale the plane HAD when the mouse went down. Reading the live
   * scale here would have committed a distance the box never showed: a peer
   * coming online mid-drag widens the plane, the scale halves, and the same
   * cursor offset would suddenly mean twice as far.
   */
  function startDrag(event: MouseEvent, spot: InputSpot) {
    if (disabled) return;
    // A second drag cannot start while one is live (a mouse has one button down
    // at a time, but a lost mouseup would otherwise strand the first one's
    // listeners).
    stopDrag?.();
    selected = spot.monitor;
    const keys = keysFor(spot.monitor);
    const at = scale;
    const from = { x: event.clientX, y: event.clientY };
    drag = { keys, dx: 0, dy: 0 };

    const handlers = {
      move: (e: MouseEvent) => {
        drag = {
          keys,
          dx: (e.clientX - from.x) / at,
          dy: (e.clientY - from.y) / at,
        };
      },
      up: () => {
        const moved = drag;
        finish();
        if (!moved || (moved.dx === 0 && moved.dy === 0)) return;
        void commit(moved.keys, moved.dx, moved.dy);
      },
    };
    const finish = () => {
      window.removeEventListener("mousemove", handlers.move);
      window.removeEventListener("mouseup", handlers.up);
      stopDrag = null;
      drag = null;
    };
    stopDrag = finish;
    window.addEventListener("mousemove", handlers.move);
    window.addEventListener("mouseup", handlers.up);
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
    // The plane may have gone while the mouse was down (the engine restarting
    // takes the snapshot away, and the drag's listeners live on the window rather
    // than in this section's markup). An arrangement of nothing REPLACES the
    // placement of every computer on the account, so it is not a thing to send.
    if (state === null || spots.length === 0 || keys.length === 0) return;
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
    const outcome = reimportBlock(spots, peer.device_id, peer.monitors);
    if (!outcome.ok) {
      refused = dropRefusal(outcome.reason);
      return;
    }
    refused = null;
    await store.placeScreens(outcome.spots);
  }

  /**
   * Who dragged the arrangement this plane is showing. Worth saying because the
   * plane is adopted from a signature this computer may not be able to verify
   * (D11): the worst a forged one can do is misplace a screen, and a human
   * noticing is the whole repair, so they are told whose it is.
   */
  const arrangedBy = $derived(
    state?.plane.by ? store.inputName(state.plane.by) : null,
  );

  // A drop refused is about the plane it was refused against: once the plane has
  // moved on (a drag that took, or another computer's), the sentence is a stale
  // account of something nobody can act on any more.
  $effect(() => {
    void state?.plane.id;
    refused = null;
  });

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

  /**
   * What a screen is called on the plane. Two identical monitors on one desk
   * really do report the same name (it is the model's, from the EDID), so they are
   * numbered: without that, both boxes read "Dell U2720Q" and nobody can tell
   * which of the two they are about to drag. The order is the snapshot's own.
   */
  function screenLabel(spot: InputSpot): string {
    const named = spot.name || "screen";
    const siblings = spots.filter(
      (s) =>
        nodeOfSpot(s.monitor) === nodeOfSpot(spot.monitor) &&
        (s.name || "screen") === named,
    );
    if (siblings.length < 2) return named;
    return `${named} (${siblings.findIndex((s) => s.monitor === spot.monitor) + 1})`;
  }

  /**
   * Where a pair has got to, in words. The two live ones name what really
   * crossed: a keyboard-only session (the offer this tab makes on every slow
   * path) never moved the mouse, and saying it did would be the interface
   * inventing half a session.
   */
  function stateWords(peer: InputPeer): string {
    const keys = state?.session?.mode === "keys";
    switch (peer.state) {
      case "off":
        return "Not connected";
      case "warming":
        return "Getting ready";
      case "ready":
        return "Ready";
      case "driving":
        return keys
          ? "Your keyboard is there"
          : "Your keyboard and mouse are there";
      case "driven":
        return keys
          ? "It is using your keyboard"
          : "It is using your keyboard and mouse";
      case "refused":
        return "Refused";
      default:
        return peer.state;
    }
  }

  /** The crossings toward one machine: the guards are per crossing. */
  function toward(device_id: string): Crossing[] {
    return ourCrossings.filter((c) => c.device_id === device_id);
  }

  /**
   * The ones a pointer could really use. A crossing into a screen that is away is
   * a wall whatever any guard says, so it is not offered as a setting: it is
   * reported as the wall it is.
   */
  function crossable(device_id: string): Crossing[] {
    return toward(device_id).filter((c) => !c.ghost);
  }

  /**
   * One guard change, applied to every crossing toward that machine, and every
   * field sent every time: the engine fills a field a write leaves out with its
   * DEFAULT rather than with what was stored, so a partial write would quietly
   * reset the other four.
   *
   * One write per distinct (their screen, side): that is the key the engine
   * stores under, and it already fans a write out to every segment matching it,
   * so writing once per segment would be several identical persists and several
   * snapshots for nothing. It stops at the first refusal, because each gesture
   * clears the banner as it starts and carrying on would wipe the sentence
   * explaining the failure.
   */
  async function guard(
    peer: InputPeer,
    change: Record<string, number | boolean>,
  ): Promise<boolean> {
    const seen = new Set<string>();
    for (const crossing of crossable(peer.device_id)) {
      const key = `${crossing.to}|${crossing.side}`;
      if (seen.has(key)) continue;
      seen.add(key);
      const stored = guardsFor(state?.guards ?? [], crossing);
      const ok = await store.setGuards(peer.device_id, crossing.to, crossing.side, {
        ...GUARD_DEFAULTS,
        ...(stored ?? {}),
        ...change,
      });
      if (!ok) return false;
    }
    return true;
  }

  /** What the guards toward one machine do, said once for the whole pair. */
  function guardSummary(peer: InputPeer): string[] {
    const crossing = crossable(peer.device_id)[0] ?? toward(peer.device_id)[0];
    if (!crossing) return [];
    return guardWords(
      guardsFor(state?.guards ?? [], crossing) ?? {},
      crossing.ghost,
    );
  }

  function firstGuards(peer: InputPeer) {
    const crossing = crossable(peer.device_id)[0];
    return crossing ? guardsFor(state?.guards ?? [], crossing) : undefined;
  }

  /**
   * Taking the pointer across a slow path warns first, and the offer alongside
   * the warning is the keyboard alone. Above the threshold the engine refuses
   * the pointer outright (`INPUT_TOO_SLOW`), so the warning is the honest step
   * before a refusal the person would otherwise walk into.
   */
  let confirming = $state<{ device_id: string; rtt: number } | null>(null);

  function askOrTake(peer: InputPeer) {
    const verdict = pointerVerdict(peer.rtt_ms);
    if ((verdict === "warn" || verdict === "refuse") && peer.rtt_ms !== null) {
      // The number is captured, not re-read: the engine republishes up to ten
      // times a second, and a warning that read a live `rtt_ms` could end up
      // saying "0 ms away" about a path that had just gone quiet.
      confirming = { device_id: peer.device_id, rtt: peer.rtt_ms };
      return;
    }
    void store.takeInput(peer.device_id);
  }

  /**
   * Whether taking the keyboard there could work at all, and what to say when it
   * could not. Both facts are this machine's own: the engine only warms a channel
   * to a computer this one has been told it may drive AND that its directory calls
   * reachable, so a take without both is accepted, parked, and never spoken of
   * again. Offering a button that answers nothing is the exact failure this
   * feature exists to correct.
   */
  function cannotTake(peer: InputPeer): string | null {
    if (!peer.drive) {
      return `Tick ${peer.name} under "Who this computer may drive" first: this computer keeps a live channel only to the computers on that list.`;
    }
    if (peer.state === "off") {
      return `${peer.name} is not answering right now, so there is nothing to hand a keyboard to.`;
    }
    return null;
  }

  /** A checkbox the browser has already moved, put back if the engine said no. */
  async function toggle(
    event: Event,
    act: (wanted: boolean) => Promise<boolean>,
  ) {
    const box = event.currentTarget as HTMLInputElement;
    const wanted = box.checked;
    if (!(await act(wanted))) box.checked = !wanted;
  }

  /** The same for a menu of choices: `was` is what the engine had. */
  async function choose(
    event: Event,
    was: string,
    act: (wanted: string) => Promise<boolean>,
  ) {
    const menu = event.currentTarget as HTMLSelectElement;
    if (!(await act(menu.value))) menu.value = was;
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
    <!-- The scroller is what gives way on a narrow window, so the box below keeps
         the width the drag's arithmetic assumes. -->
    <div class="scroller">
    <div
      class="plane"
      style="width:{PLANE_W}px;height:{planeHeight}px"
      aria-label="The screens of your computers"
    >
      {#each spots as spot (spot.monitor)}
        <button
          class="screen"
          class:ghost={!spot.present}
          class:mine={spot.device_id === hereId}
          class:selected={selected === spot.monitor}
          style={box(spot)}
          aria-label="{nameOfSpot(spot)} {screenLabel(spot)}"
          onmousedown={(e) => startDrag(e, spot)}
          onkeydown={(e) => nudge(e, spot)}
        >
          <span class="who">{nameOfSpot(spot)}</span>
          <span class="what">
            {screenLabel(spot)}{spot.primary ? " (main)" : ""}
          </span>
          {#if spot.device_id === hereId && spot.primary}
            <span class="here">you are here</span>
          {/if}
          {#if !spot.present}<span class="what">away</span>{/if}
        </button>
      {/each}
    </div>
    </div>

    {#if blocks.length < 2}
      <p class="hint">
        There is only this computer's screens here. Another computer of your
        account shows up on this plane as soon as it says where its own screens
        are, and then dragging says which is next to which.
      </p>
    {:else}
      <p class="hint">
        Drag a screen to say where it really is. A computer's screens move
        together; tick the box below to move one on its own. Arrow keys move the
        selection by one screen, and hold Shift for a nudge.
      </p>
    {/if}
    {#if arrangedBy}
      <p class="muted">Arranged by {arrangedBy}.</p>
    {/if}
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
                toggle(e, (wanted) => store.allowInput(peer.device_id, wanted))}
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
      A shortlist for this computer's own use: which computers its pointer may
      cross to, and which ones it keeps a live channel ready for. It grants
      nothing over there. Each of those computers keeps its own list, and this one
      finds out by trying.
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
                toggle(e, (wanted) => store.driveInput(peer.device_id, wanted))}
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
          {@const blocked = cannotTake(peer)}
          <li>
            <div class="row">
              <span class="name">{peer.name}</span>
              <span class="meta">{stateWords(peer)}</span>
            </div>
            {#if path}<p class="meta">{path}</p>{/if}
            {#if problem}<p class="problem">{problem}</p>{/if}

            {#if confirming?.device_id === peer.device_id}
              <p class="confirm">
                {slowPathWarning(peer.name, confirming.rtt)}
              </p>
            {/if}
            <div class="row">
              {#if peer.state === "driving"}
                <button {disabled} onclick={() => store.releaseInput()}>
                  Bring my keyboard back
                </button>
              {:else if peer.state === "driven"}
                <!-- It is using OUR keyboard: the only gesture that can succeed
                     here is ending that, and the two takes would both be refused
                     `INPUT_BUSY` every time. -->
                <button
                  {disabled}
                  aria-label="Take my keyboard back from {peer.name}"
                  onclick={() => store.releaseInput()}
                  >Take my keyboard back</button
                >
              {:else if confirming?.device_id === peer.device_id}
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
              {:else if blocked === null}
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
            </div>
            {#if blocked && peer.state !== "driving" && peer.state !== "driven"}
              <!-- No button at all, and the reason: a take this machine cannot
                   even attempt is accepted by the engine, parked, and never spoken
                   of again, which is the one thing this feature must not do. -->
              <p class="meta">{blocked}</p>
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
                {#if crossable(peer.device_id).length > 0}
                {@const edge = crossable(peer.device_id)[0]}
                {#if tooShortToCross(edge.length, stored ?? {})}
                  <p class="problem">{tooShortToCross(edge.length, stored ?? {})}</p>
                {/if}
                <label>
                  Cross
                  <select
                    {disabled}
                    aria-label="When the pointer crosses to {peer.name}"
                    value={String(stored?.dwell_ms ?? GUARD_DEFAULTS.dwell_ms)}
                    onchange={(e) =>
                      choose(
                        e,
                        String(stored?.dwell_ms ?? GUARD_DEFAULTS.dwell_ms),
                        (wanted) => guard(peer, { dwell_ms: Number(wanted) }),
                      )}
                  >
                    {#if !DWELL_CHOICES.some((c) => c.ms === (stored?.dwell_ms ?? GUARD_DEFAULTS.dwell_ms))}
                      <option value={String(stored?.dwell_ms)}>
                        after {stored?.dwell_ms} ms at the edge
                      </option>
                    {/if}
                    {#each DWELL_CHOICES as choice (choice.ms)}
                      <option value={String(choice.ms)}>
                        {choice.label.toLowerCase()}
                      </option>
                    {/each}
                  </select>
                </label>
                <label>
                  Only while holding
                  <select
                    {disabled}
                    aria-label="The key to hold to cross to {peer.name}"
                    value={String(stored?.require_mods ?? 0)}
                    onchange={(e) =>
                      choose(e, String(stored?.require_mods ?? 0), (wanted) =>
                        guard(peer, { require_mods: Number(wanted) }),
                      )}
                  >
                    <option value="0">nothing</option>
                    <option value={String(MODS.ctrl)}>Ctrl</option>
                    <option value={String(MODS.alt)}>Alt</option>
                    <option value={String(MODS.shift)}>Shift</option>
                    <option value={String(MODS.meta)}>Meta</option>
                  </select>
                </label>
                <label>
                  <input
                    type="checkbox"
                    checked={stored?.wall ?? false}
                    {disabled}
                    aria-label="Never cross to {peer.name}"
                    onchange={(e) =>
                      toggle(e, (wanted) => guard(peer, { wall: wanted }))}
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
                      toggle(e, (wanted) =>
                        guard(peer, { double_tap_ms: wanted ? 300 : 0 }),
                      )}
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
                      toggle(e, (wanted) =>
                        guard(peer, {
                          dead_corner: wanted ? GUARD_DEFAULTS.dead_corner : 0,
                        }),
                      )}
                  />
                  Leave the corners alone, for menus and hot corners
                </label>
                {/if}
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
        onchange={(e) => toggle(e, (wanted) => store.lockPointer(wanted))}
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
        onchange={(e) =>
          choose(e, hotkeyLabel(state.hotkey), async (label) => {
            const chosen = HOTKEYS.find((chord) => chord.label === label);
            // A chord this build does not offer is not a refusal: it is the one
            // already in force, listed so it can be seen.
            return chosen ? await store.setHotkey(chosen.keys) : true;
          })}
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
  /* The box keeps its width; this is what gives way on a narrow window. */
  .scroller {
    max-width: 100%;
    overflow-x: auto;
  }

  .plane {
    position: relative;
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
