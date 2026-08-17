// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

import { expect, test } from "vitest";

import type { InputGuards, InputSpot } from "./api";
import {
  DWELL_CHOICES,
  GHOST_SENTENCE,
  MODS,
  POINTER_MAX_MS,
  REFUSAL_CODES,
  SNAP,
  blockKeys,
  crossings,
  dropRefusal,
  dropSpots,
  dwellChoice,
  gestureFailure,
  guardWords,
  guardsFor,
  hereProblemSentence,
  hotkeyLabel,
  modifierWords,
  monitorOfSpot,
  nodeOfSpot,
  pathLine,
  peerProblemSentence,
  planeBounds,
  pointerVerdict,
  refusalSentence,
  reimportBlock,
  sessionSentence,
  slowPathWarning,
} from "./input";

const A = "a".repeat(64);
const B = "b".repeat(64);

function spot(key: string, x: number, y: number, w = 1920, h = 1080): InputSpot {
  return {
    monitor: key,
    device_id: key.startsWith(A) ? "d_a" : "d_b",
    name: key.split("/")[1],
    x,
    y,
    w,
    h,
    present: true,
    primary: false,
  };
}

// --- The sentences ----------------------------------------------------------

// The three the epic wrote down, word for word: they are the differentiator, and
// a rewording is a regression.
test("the epic's three sentences are said exactly", () => {
  expect(refusalSentence("ELEVATED_WINDOW")).toBe(
    "Nothing was typed: this window runs as administrator.",
  );
  expect(refusalSentence("SECURE_INPUT")).toBe(
    "Nothing was typed: password fields block synthetic keystrokes on macOS.",
  );
  // The third one names the machine instead of saying "this computer", because
  // it is read on the machine that is DRIVING, and it carries the second cause
  // the one code covers (a pointer pinned there on purpose).
  expect(refusalSentence("LOCKED", "Desk")).toContain(
    "cannot be driven while it is locked",
  );
  expect(refusalSentence("LOCKED", "Desk")).toContain("pinned to its own screen");
  // And the same fact about an injection that did not happen, from the `oops`
  // half of the vocabulary.
  expect(refusalSentence("SCREEN_LOCKED", "Desk")).toBe(
    "Nothing was typed: Desk is locked.",
  );
});

test("the deployment whose relays will not carry a session says so, and how to fix it", () => {
  const said = refusalSentence("NO_DIRECT_PATH", "Desk");
  expect(said).toContain("do not carry a keyboard session");
  expect(said).toContain("Desk");
  expect(said).toContain("VPN");
});

// Every code in section 13 of the design document, plus the ones the engine
// really emits around a live channel. A code with no sentence is a keystroke
// that went nowhere in silence.
test("every code the engine emits has a sentence, and none of them softens it", () => {
  const emitted = [
    // The dialect's refusals.
    "NOT_ALLOWED",
    "BUSY",
    "PLANE_STALE",
    "NO_BACKEND",
    "LOCKED",
    // The `oops` codes, from the target.
    "ELEVATED_WINDOW",
    "SECURE_INPUT",
    "SCREEN_LOCKED",
    "NO_PERMISSION",
    "UNRESOLVED",
    // How a session ended.
    "IDLE",
    "TAKEN",
    "REVOKED",
    "SLOW",
    "GONE",
    // Not ready, and the channel that could not be opened at all.
    "NOT_WARM",
    "NO_DIRECT_PATH",
    "COMPONENT_ABSENT",
    "OPEN_REFUSED",
    "OPEN_FAILED",
  ];
  for (const code of emitted) {
    expect(REFUSAL_CODES, `${code} has a sentence`).toContain(code);
    const said = refusalSentence(code, "Desk");
    expect(said.length).toBeGreaterThan(20);
    expect(said, `${code} leaves no placeholder`).not.toContain("<name>");
    // No hedging: a refusal is detected, not guessed.
    expect(said).not.toMatch(/might|may not have|probably|perhaps/);
  }
});

// `ATTACH_FAILED` is the one code that arrives with a payload of its own.
test("a code with a payload is recognised by its head", () => {
  expect(refusalSentence("ATTACH_FAILED: socket closed", "Desk")).toBe(
    "The live channel to Desk could not be attached.",
  );
});

// A later engine's vocabulary must degrade to something a person can repeat.
test("an unknown code is carried, never swallowed", () => {
  const said = refusalSentence("SOMETHING_NEW", "Desk");
  expect(said).toContain("Desk");
  expect(said).toContain("SOMETHING_NEW");
  expect(said).toContain("this version does not know");
});

test("without a name, a sentence still reads", () => {
  expect(refusalSentence("BUSY", null)).toBe(
    "Another of your computers is using That computer's keyboard right now.",
  );
  expect(refusalSentence("BUSY", "")).toContain("That computer");
});

test("every standing problem of a pair has a sentence", () => {
  for (const problem of [
    "not_allowed",
    "busy",
    "locked",
    "no_backend",
    "no_path",
    "too_slow",
    "plane_stale",
  ]) {
    const said = peerProblemSentence(problem, "Desk");
    expect(said, problem).toBeTruthy();
    expect(said).toContain("Desk");
  }
  expect(peerProblemSentence(null)).toBeNull();
  expect(peerProblemSentence("from_the_future", "Desk")).toContain(
    "from_the_future",
  );
});

// The grant is the far side's, and the sentence has to send the person THERE.
test("a refused grant names the machine to go and change it on", () => {
  expect(peerProblemSentence("not_allowed", "Desk")).toContain("on Desk itself");
});

// The normal state of a fresh Mac: Input Monitoring granted at the first dialog,
// Accessibility not. It must name the half that is missing and its own pane.
test("a Mac missing only Accessibility is told which half and which pane", () => {
  const said = hereProblemSentence(
    { problem: "no_permission", can_drive: true, can_be_driven: false },
    "macos",
  );
  expect(said).toContain("may not type on this computer");
  expect(said).toContain("Accessibility");
  expect(said).not.toContain("Input Monitoring");
  expect(said).toContain("can still drive them");
  expect(said).toContain("within a second");
});

test("a Mac missing only Input Monitoring is told the other half", () => {
  const said = hereProblemSentence(
    { problem: "no_permission", can_drive: false, can_be_driven: true },
    "macos",
  );
  expect(said).toContain("Input Monitoring");
  expect(said).not.toContain("under Accessibility");
  expect(said).toContain("can still be driven");
});

test("a Mac missing both names both, and no pane is named off a Mac", () => {
  expect(
    hereProblemSentence(
      { problem: "no_permission", can_drive: false, can_be_driven: false },
      "macos",
    ),
  ).toContain("both Input Monitoring and Accessibility");
  const elsewhere = hereProblemSentence(
    { problem: "no_permission", can_drive: false, can_be_driven: false },
    "linux",
  );
  expect(elsewhere).not.toContain("System Settings");
  expect(elsewhere).toContain("neither permission");
});

test("a Wayland session is told what does not work, and what still does", () => {
  const said = hereProblemSentence(
    { problem: "wayland", can_drive: false, can_be_driven: false },
    "linux",
  );
  expect(said).toContain("Wayland");
  expect(said).toContain("X11");
  expect(said).toContain("keeps working");
});

test("this computer's other problems have sentences, and none is invented", () => {
  expect(
    hereProblemSentence({
      problem: null,
      can_drive: true,
      can_be_driven: true,
    }),
  ).toBeNull();
  for (const problem of ["no_backend", "monitors_unstable"] as const) {
    expect(
      hereProblemSentence({ problem, can_drive: false, can_be_driven: false }),
    ).toBeTruthy();
  }
});

test("a gesture's own refusals are sentences, and a malformed call is not", () => {
  const rpc = (data_code: string, code = -32000) => ({
    kind: "rpc" as const,
    message: "no",
    code,
    data_code,
  });
  for (const code of [
    "INPUT_NOT_READY",
    "INPUT_DEVICE_UNKNOWN",
    "INPUT_BUSY",
    "INPUT_LOCKED",
    "INPUT_NO_BACKEND",
    "INPUT_TOO_SLOW",
    "INPUT_UNKNOWN_MONITOR",
    "INPUT_INTERNAL",
    "COMPONENT_ABSENT",
  ]) {
    expect(gestureFailure(rpc(code)), code).toBeTruthy();
  }
  // -32602 has no data code: this interface's own bug, not advice for the user.
  expect(
    gestureFailure({ kind: "rpc", message: "invalid params: spots", code: -32602 }),
  ).toBeNull();
  expect(gestureFailure({ kind: "timeout", message: "" })).toBeNull();
  expect(gestureFailure(new Error("boom"))).toBeNull();
});

// The offer that goes with the refusal: the keyboard alone.
test("INPUT_TOO_SLOW offers the keyboard alone", () => {
  const said = gestureFailure({
    kind: "rpc",
    message: "no",
    code: -32000,
    data_code: "INPUT_TOO_SLOW",
  });
  expect(said).toContain("keyboard alone");
});

// --- The pointer thresholds -------------------------------------------------

test("the pointer thresholds are the decided ones", () => {
  expect(pointerVerdict(null)).toBe("unknown");
  expect(pointerVerdict(undefined)).toBe("unknown");
  expect(pointerVerdict(4)).toBe("silent");
  expect(pointerVerdict(10)).toBe("silent");
  expect(pointerVerdict(11)).toBe("announce");
  expect(pointerVerdict(32)).toBe("announce");
  expect(pointerVerdict(POINTER_MAX_MS)).toBe("announce");
  expect(pointerVerdict(POINTER_MAX_MS + 1)).toBe("refuse");
});

// A relayed session is allowed, never silent: the path and the number are said.
test("the path and its number are said, and a slow one warns first", () => {
  expect(pathLine({ rtt_ms: 4, lan: true })).toBe("4 ms away, on this network.");
  const relayed = pathLine({ rtt_ms: 32, lan: false });
  expect(relayed).toContain("32 ms away, over the internet");
  expect(relayed).toContain("lag a little");
  const far = pathLine({ rtt_ms: 120, lan: false });
  expect(far).toContain("keyboard alone");
  expect(pathLine({ rtt_ms: null, lan: false })).toContain("Not measured yet");
  expect(slowPathWarning("Desk", 32)).toContain("32 ms");
  expect(slowPathWarning("Desk", 32)).toContain("keyboard alone");
});

// --- The live state ---------------------------------------------------------

test("the source says where its keyboard went and how to get it back", () => {
  const said = sessionSentence(
    {
      device_id: "d_b",
      direction: "out",
      mode: "full",
      since: 0,
      rtt_ms: 4,
    },
    "Desk",
    ["ctrl", "alt", "Escape"],
  );
  expect(said).toBe(
    "Your keyboard and mouse are on Desk. Press Ctrl + Alt + Escape to bring them back.",
  );
});

test("a slow session announces its number in the live sentence", () => {
  const said = sessionSentence(
    { device_id: "d_b", direction: "out", mode: "full", since: 0, rtt_ms: 32 },
    "Desk",
    ["ctrl", "alt", "Escape"],
  );
  expect(said).toContain("32 ms away");
});

test("a keyboard-only session does not claim the mouse went too", () => {
  const said = sessionSentence(
    { device_id: "d_b", direction: "out", mode: "keys", since: 0, rtt_ms: 4 },
    "Desk",
    ["ctrl", "alt", "Escape"],
  );
  expect(said).toContain("Your keyboard is on Desk");
  expect(said).not.toContain("mouse");
});

test("the target says who is using it", () => {
  expect(
    sessionSentence(
      { device_id: "d_a", direction: "in", mode: "full", since: 0, rtt_ms: null },
      "Laptop",
      [],
    ),
  ).toBe("Laptop is using your keyboard and mouse right now.");
  expect(
    sessionSentence(
      { device_id: "d_a", direction: "in", mode: "keys", since: 0, rtt_ms: null },
      "Laptop",
      [],
    ),
  ).toBe("Laptop is using your keyboard right now.");
  expect(sessionSentence(null, "Laptop", [])).toBeNull();
});

test("a session with a device the directory does not name still reads", () => {
  const said = sessionSentence(
    { device_id: null, direction: "in", mode: "full", since: 0, rtt_ms: null },
    null,
    [],
  );
  expect(said).toBe("another computer is using your keyboard and mouse right now.");
});

test("the hotkey is spelled for a human", () => {
  expect(hotkeyLabel(["ctrl", "alt", "Escape"])).toBe("Ctrl + Alt + Escape");
  expect(hotkeyLabel(["ctrl", "shift", "Home"])).toBe("Ctrl + Shift + Home");
  expect(hotkeyLabel([])).toBe("the return hotkey");
});

// --- The guards -------------------------------------------------------------

test("the guards are words, and the wall says the whole truth on its own", () => {
  expect(guardWords({ wall: true })).toEqual([
    "The pointer never crosses here.",
  ]);
  const plain = guardWords({});
  expect(plain.join(" ")).toContain("short pause");
  expect(plain.join(" ")).toContain("corners");
  const strict = guardWords({
    require_mods: MODS.ctrl,
    double_tap_ms: 200,
    dwell_ms: 0,
    dead_corner: 0,
  });
  expect(strict[0]).toBe("Only while Ctrl is held.");
  expect(strict[1]).toContain("comes straight back");
  expect(strict[2]).toContain("As soon as");
  expect(strict.join(" ")).not.toContain("corners");
});

test("a dwell nobody offered is shown as the number it is", () => {
  expect(guardWords({ dwell_ms: 1234 }).join(" ")).toContain("1234 ms");
  expect(dwellChoice(0)).toBe(0);
  expect(dwellChoice(250)).toBe(250);
  expect(dwellChoice(400)).toBe(250);
  expect(dwellChoice(600)).toBe(600);
  expect(dwellChoice(5000)).toBe(600);
  expect(DWELL_CHOICES.map((c) => c.ms)).toEqual([0, 250, 600]);
});

test("the modifier bits are the engine's, and they are named", () => {
  expect(modifierWords(0)).toBeNull();
  expect(modifierWords(MODS.ctrl)).toBe("Ctrl");
  expect(modifierWords(MODS.ctrl | MODS.shift)).toBe("Shift + Ctrl");
  expect(MODS.shift).toBe(1);
  expect(MODS.ctrl).toBe(2);
  expect(MODS.alt).toBe(4);
  expect(MODS.altgr).toBe(8);
  expect(MODS.meta).toBe(16);
});

test("the guards of a crossing are found by the neighbour's screen and the side", () => {
  const stored: InputGuards[] = [
    {
      device_id: "d_b",
      monitor: `${B}/B1`,
      side: "right",
      dwell_ms: 500,
      double_tap_ms: 0,
      dead_corner: 16,
      require_mods: 0,
      wall: false,
    },
  ];
  expect(guardsFor(stored, { to: `${B}/B1`, side: "right" })?.dwell_ms).toBe(500);
  expect(guardsFor(stored, { to: `${B}/B1`, side: "left" })).toBeUndefined();
  expect(guardsFor(stored, { to: `${B}/B2`, side: "right" })).toBeUndefined();
});

// --- The plane --------------------------------------------------------------

test("a machine's screens move as one block, found by node and not by name", () => {
  const spots = [
    spot(`${A}/A1`, 0, 0),
    spot(`${A}/A2`, 1920, 0),
    spot(`${B}/B1`, 3840, 0),
  ];
  expect(blockKeys(spots, `${A}/A2`).sort()).toEqual(
    [`${A}/A1`, `${A}/A2`].sort(),
  );
  expect(blockKeys(spots, `${B}/B1`)).toEqual([`${B}/B1`]);
});

test("the plane's bounds cover every screen, ghosts included", () => {
  expect(planeBounds([])).toEqual({ x: 0, y: 0, w: 0, h: 0 });
  const bounds = planeBounds([
    spot(`${A}/A1`, 0, 0),
    spot(`${B}/B1`, -1920, 200),
  ]);
  expect(bounds).toEqual({ x: -1920, y: 0, w: 3840, h: 1280 });
});

// Section 7: a neighbour within SNAP of our edge, sharing a stretch of it.
test("a crossing is derived from the geometry, on the side it really is", () => {
  const spots = [spot(`${A}/A1`, 0, 0), spot(`${B}/B1`, 1920, 0)];
  const found = crossings(spots, [`${A}/A1`]);
  expect(found).toHaveLength(1);
  expect(found[0]).toMatchObject({
    from: `${A}/A1`,
    to: `${B}/B1`,
    side: "right",
    length: 1080,
    ghost: false,
    device_id: "d_b",
  });
});

test("a gap inside the tolerance still abuts, a bigger one does not", () => {
  const near = crossings(
    [spot(`${A}/A1`, 0, 0), spot(`${B}/B1`, 1920 + SNAP, 0)],
    [`${A}/A1`],
  );
  expect(near).toHaveLength(1);
  const far = crossings(
    [spot(`${A}/A1`, 0, 0), spot(`${B}/B1`, 1920 + SNAP + 1, 0)],
    [`${A}/A1`],
  );
  expect(far).toHaveLength(0);
});

test("a stretch of edge with nothing across it is a wall, and says nothing", () => {
  // B sits past A's right edge but entirely below it: no shared stretch.
  const found = crossings(
    [spot(`${A}/A1`, 0, 0), spot(`${B}/B1`, 1920, 1080)],
    [`${A}/A1`],
  );
  expect(found).toHaveLength(0);
});

test("the shared stretch is only what the two screens really share", () => {
  const found = crossings(
    [spot(`${A}/A1`, 0, 0, 1920, 1080), spot(`${B}/B1`, 1920, 600, 1920, 1080)],
    [`${A}/A1`],
  );
  expect(found[0].length).toBe(480);
});

test("a screen of the same machine is never a crossing", () => {
  const found = crossings(
    [spot(`${A}/A1`, 0, 0), spot(`${A}/A2`, 1920, 0)],
    [`${A}/A1`, `${A}/A2`],
  );
  expect(found).toHaveLength(0);
});

test("a crossing into a screen that is away is marked as the wall it is", () => {
  const ghost = { ...spot(`${B}/B1`, 1920, 0), present: false, name: "" };
  const found = crossings([spot(`${A}/A1`, 0, 0), ghost], [`${A}/A1`]);
  expect(found[0].ghost).toBe(true);
  expect(GHOST_SENTENCE).toContain("place is kept");
});

test("crossings are found on all four sides", () => {
  const mine = spot(`${A}/A1`, 0, 0);
  const sides = [
    { at: spot(`${B}/B1`, 1920, 0), side: "right" },
    { at: spot(`${B}/B1`, -1920, 0), side: "left" },
    { at: spot(`${B}/B1`, 0, 1080), side: "bottom" },
    { at: spot(`${B}/B1`, 0, -1080), side: "top" },
  ];
  for (const { at, side } of sides) {
    expect(crossings([mine, at], [`${A}/A1`])[0]?.side, side).toBe(side);
  }
});

// --- The drag ---------------------------------------------------------------

test("a drag moves the whole block and keeps its shape", () => {
  const spots = [
    spot(`${A}/A1`, 0, 0),
    spot(`${A}/A2`, 1920, 0),
    spot(`${B}/B1`, 5000, 0),
  ];
  const out = dropSpots(spots, [`${A}/A1`, `${A}/A2`], 100, 50);
  expect(out.ok).toBe(true);
  if (!out.ok) return;
  expect(out.spots).toEqual([
    { monitor: `${A}/A1`, x: 100, y: 50 },
    { monitor: `${A}/A2`, x: 2020, y: 50 },
    { monitor: `${B}/B1`, x: 5000, y: 0 },
  ]);
});

// A laptop's external screen may genuinely sit next to another computer.
test("one screen can be detached from its block", () => {
  const spots = [spot(`${A}/A1`, 0, 0), spot(`${A}/A2`, 1920, 0)];
  const out = dropSpots(spots, [`${A}/A2`], 0, 1200);
  expect(out.ok).toBe(true);
  if (!out.ok) return;
  expect(out.spots[0]).toEqual({ monitor: `${A}/A1`, x: 0, y: 0 });
  expect(out.spots[1]).toEqual({ monitor: `${A}/A2`, x: 1920, y: 1200 });
});

// Without the snap, an imported arrangement would need pixel-perfect dragging.
test("a drop within the tolerance snaps onto the edge it was aiming at", () => {
  const spots = [spot(`${A}/A1`, 0, 0), spot(`${B}/B1`, 5000, 0)];
  // Dropped 7 px short of A's right edge and 3 px low: it lands exactly on it.
  const out = dropSpots(spots, [`${B}/B1`], -5000 + 1920 - 7, 3);
  expect(out.ok).toBe(true);
  if (!out.ok) return;
  expect(out.spots[1]).toEqual({ monitor: `${B}/B1`, x: 1920, y: 0 });
  // And the crossing that snap was for really is there now.
  const moved = spots.map((s) => {
    const at = out.spots.find((p) => p.monitor === s.monitor);
    return { ...s, x: at?.x ?? s.x, y: at?.y ?? s.y };
  });
  expect(crossings(moved, [`${A}/A1`])).toHaveLength(1);
});

// An overlap silently removes the crossing the person was trying to make: the
// engine derives no crossing from overlapping rectangles.
test("a drop that would put two screens in the same place is refused", () => {
  const spots = [spot(`${A}/A1`, 0, 0), spot(`${B}/B1`, 5000, 0)];
  const out = dropSpots(spots, [`${B}/B1`], -4000, 0);
  expect(out).toEqual({ ok: false, reason: "overlap" });
  expect(dropRefusal("overlap")).toContain("same place");
  expect(dropRefusal("overlap")).toContain("Nothing was moved");
});

test("a drop off the plane is refused rather than sent to be refused", () => {
  const spots = [spot(`${A}/A1`, 0, 0), spot(`${B}/B1`, 5000, 0)];
  const out = dropSpots(spots, [`${B}/B1`], 2_000_000, 0);
  expect(out).toEqual({ ok: false, reason: "off_plane" });
  expect(dropRefusal("off_plane")).toContain("Nothing was moved");
});

// `input.place` REPLACES the placement, so a screen left out of the set would
// lose its place, and a ghost's place is what must not be lost.
test("a drop carries every screen, ghosts included, so none loses its place", () => {
  const ghost = { ...spot(`${B}/B2`, 1920, 1080), present: false };
  const spots = [spot(`${A}/A1`, 0, 0), spot(`${B}/B1`, 1920, 0), ghost];
  const out = dropSpots(spots, [`${A}/A1`], 0, -1080);
  expect(out.ok).toBe(true);
  if (!out.ok) return;
  expect(out.spots.map((s) => s.monitor)).toEqual(spots.map((s) => s.monitor));
  expect(out.spots[2]).toEqual({ monitor: `${B}/B2`, x: 1920, y: 1080 });
});

// The arrangement is imported rather than re-invented, so there has to be a way
// back from having scattered it by hand.
test("a machine's own arrangement can be put back, keeping the block's corner", () => {
  const spots = [
    spot(`${A}/A1`, 0, 0),
    // B's two screens, dragged apart by a human: B1 far right, B2 far below.
    { ...spot(`${B}/B1`, 4000, 0), device_id: "d_b" },
    { ...spot(`${B}/B2`, 4000, 2000), device_id: "d_b" },
  ];
  // B's own desktop has them side by side, B2 to the right of B1.
  const own = [
    { id: "B1", x: 0, y: 0 },
    { id: "B2", x: 1920, y: 0 },
  ];
  const back = reimportBlock(spots, "d_b", own);
  expect(back).toEqual([
    { monitor: `${A}/A1`, x: 0, y: 0 },
    { monitor: `${B}/B1`, x: 4000, y: 0 },
    { monitor: `${B}/B2`, x: 5920, y: 0 },
  ]);
});

test("putting a block back leaves every other screen exactly where it was", () => {
  const spots = [spot(`${A}/A1`, -100, -200), spot(`${B}/B1`, 4000, 0)];
  expect(reimportBlock(spots, "d_b", [])).toEqual([
    { monitor: `${A}/A1`, x: -100, y: -200 },
    { monitor: `${B}/B1`, x: 4000, y: 0 },
  ]);
  expect(reimportBlock(spots, "d_nobody", [{ id: "X", x: 0, y: 0 }])).toEqual([
    { monitor: `${A}/A1`, x: -100, y: -200 },
    { monitor: `${B}/B1`, x: 4000, y: 0 },
  ]);
});

// A monitor id may itself contain a slash, so the key splits at the first one.
test("a spot key splits into the machine and the screen", () => {
  expect(nodeOfSpot(`${A}/win:dev:DISPLAY/1`)).toBe(A);
  expect(monitorOfSpot(`${A}/win:dev:DISPLAY/1`)).toBe("win:dev:DISPLAY/1");
  expect(monitorOfSpot("nothing")).toBe("");
});

test("a drag of nothing changes nothing", () => {
  const spots = [spot(`${A}/A1`, 0, 0)];
  const out = dropSpots(spots, [], 100, 100);
  expect(out).toEqual({ ok: true, spots: [{ monitor: `${A}/A1`, x: 0, y: 0 }] });
});

// The plane is integers: the engine signs this document, and a float on a signed
// wire is a round-tripping trap.
test("a fractional drag lands on whole pixels", () => {
  const spots = [spot(`${A}/A1`, 0, 0)];
  const out = dropSpots(spots, [`${A}/A1`], 10.6, -3.2);
  expect(out.ok).toBe(true);
  if (!out.ok) return;
  expect(out.spots[0]).toEqual({ monitor: `${A}/A1`, x: 11, y: -3 });
});
