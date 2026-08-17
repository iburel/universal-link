// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

import { expect, test } from "vitest";

import type {
  InputGuards,
  InputProblem,
  InputSpot,
  PeerProblem,
} from "./api";
import {
  DWELL_CHOICES,
  GHOST_SENTENCE,
  MODS,
  POINTER_MAX_MS,
  POINTER_WARN_MS,
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
  tooShortToCross,
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
  // The third one belongs to the code a locked screen really produces, which is
  // the `oops` one (`refused::LOCKED` is the pointer PIN, and nothing else). It
  // names no machine, deliberately: the same code is emitted on both ends of a
  // session and a name would be the wrong machine on one of them.
  const locked = refusalSentence("SCREEN_LOCKED", "Desk");
  expect(locked).toContain("cannot be driven while it is locked");
  expect(locked).not.toContain("Desk");
  // And the pin, which is a switch on the other machine rather than a lock.
  expect(refusalSentence("LOCKED", "Desk")).toContain("pinned to its own screen");
  expect(refusalSentence("LOCKED", "Desk")).toContain("switch is on Desk");
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
    // The Core's own words for a channel that could not be opened, relayed
    // verbatim by the engine. `DEVICE_OFFLINE` is the everyday one: a computer
    // whose record still carries a route while it is asleep.
    "DEVICE_OFFLINE",
    "DEVICE_UNKNOWN",
    // And what a code from a newer engine becomes on the way in.
    "UNKNOWN",
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

// A Record keyed by the union, for the reason EVERY_PROBLEM below is one: **tsc
// refuses to compile this file** when a member joins `PeerProblem` and is not
// listed here, so a walk over it is real coverage rather than the appearance of it.
// The engine's own half of the bridge is `PeerProblem::position`, an exhaustive
// match, plus the test that reads this file's sentences from disk.
const EVERY_PEER_PROBLEM: Record<PeerProblem, true> = {
  not_allowed: true,
  busy: true,
  locked: true,
  no_backend: true,
  no_path: true,
  too_slow: true,
  plane_stale: true,
  xwayland: true,
};

test("every standing problem of a pair has a sentence", () => {
  for (const problem of Object.keys(EVERY_PEER_PROBLEM) as PeerProblem[]) {
    const said = peerProblemSentence(problem, "Desk");
    expect(said, problem).toBeTruthy();
    expect(said, problem).toContain("Desk");
    // The fallback would carry the code, so a member with no sentence of its own
    // cannot pass by looking plausible.
    expect(said, problem).not.toContain("does not know");
    expect(said, problem).not.toContain(problem);
  }
  expect(peerProblemSentence(null)).toBeNull();
  expect(peerProblemSentence("from_the_future", "Desk")).toContain(
    "from_the_future",
  );
});

// The one this change exists for: a peer reached through its XWayland is a pair that
// half works, and the row has to say WHICH half before somebody types into the other
// one. A sentence that only named the session type would pass a truthiness test and
// leave the silence in place.
test("an XWayland target says which windows will receive the keyboard", () => {
  const said = peerProblemSentence("xwayland", "Desk") ?? "";
  expect(said).toContain("only part of Desk");
  expect(said).toContain("X11");
  expect(said).toContain("Wayland");
  expect(said).toContain("receive nothing");
  // It is not a refusal and must not read as one: nothing here says the pair
  // cannot be driven.
  expect(said).not.toContain("cannot be driven");
  // And the screens warning this code swallows in the engine's single slot, said
  // here, to the side that drags the plane. Its twin is asserted for
  // `hereProblemSentence` for the same reason.
  expect(said).toContain("swap places");
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

// Every problem the engine can report, so a new reason code cannot arrive without
// a sentence. The engine has the mirror of this test: it walks its own `Problem`
// enum and reads this file, which closes the loop from the other side. Neither the
// Rust compiler nor tsc can see across the boundary, so this pair is the bridge.
// A Record keyed by the union, so **tsc refuses to compile this file** when a member
// is added to `InputProblem` and not listed here. A review caught the first version,
// which was a plain array: a member added to the union and to `input.ts` but not to
// the array was never exercised, so the test looked like coverage and was not. The
// engine's own half of the bridge is `Problem::position`, an exhaustive match that
// makes the same guarantee on the Rust side.
const EVERY_PROBLEM: Record<InputProblem, true> = {
  no_backend: true,
  no_permission: true,
  monitors_unstable: true,
  wayland: true,
  xwayland: true,
  wayland_no_bus: true,
  wayland_no_portal: true,
  wayland_portal_old: true,
  wayland_portal_refused: true,
  wayland_untested: true,
};

const PROBLEMS = Object.keys(EVERY_PROBLEM) as InputProblem[];

test("every problem the engine can report has a real sentence, on every platform", () => {
  expect(
    hereProblemSentence({
      problem: null,
      can_drive: true,
      can_be_driven: true,
    }),
  ).toBeNull();
  // Both platform wordings and all four capability combinations, because two of
  // these sentences branch on them and a branch with no test is a branch that says
  // the wrong thing to somebody.
  for (const problem of PROBLEMS) {
    for (const platform of ["linux", "macos", "windows", undefined]) {
      for (const can_drive of [true, false]) {
        for (const can_be_driven of [true, false]) {
          const said = hereProblemSentence(
            { problem, can_drive, can_be_driven },
            platform,
          );
          expect(said, `${problem} on ${platform}`).toBeTruthy();
          expect(said, `${problem} on ${platform}`).not.toContain(
            "does not know",
          );
          // The code itself never reaches a person: a sentence that leaked its
          // own reason code would mean the `default` arm had been taken.
          expect(said, `${problem} on ${platform}`).not.toContain(problem);
        }
      }
    }
  }
});

test("a code the engine invents later still says something a person can repeat", () => {
  const said = hereProblemSentence({
    problem: "wayland_something_new" as InputProblem,
    can_drive: false,
    can_be_driven: false,
  });
  expect(said).toContain("does not know");
  expect(said).toContain("wayland_something_new");
});

// The one this ticket exists for: a Wayland desktop is told which piece is missing
// and what would fix it, never just "this is Wayland".
test("each Wayland reason names its own remedy", () => {
  const say = (problem: InputProblem, can_drive = false, can_be_driven = false) =>
    hereProblemSentence({ problem, can_drive, can_be_driven }, "linux") ?? "";

  expect(say("xwayland")).toContain("X11");
  expect(say("xwayland")).toContain("Wayland directly");
  expect(say("wayland_no_bus")).toContain("D-Bus");
  expect(say("wayland_portal_old")).toContain("xdg-desktop-portal");
  expect(say("wayland_portal_refused")).toContain("again");
  expect(say("wayland_untested")).toContain("ONEDEVICE_INPUT_WAYLAND");
  expect(say("wayland_untested")).toContain("never been run");

  // A missing portal says that a desktop may have one half without the other, and
  // deliberately does not claim WHICH: the capability bits it would have to read are
  // all false while the Wayland path is switched off, so every combination said the
  // same wrong thing. It must therefore read identically whatever they are.
  const said = new Set([
    say("wayland_no_portal", false, false),
    say("wayland_no_portal", false, true),
    say("wayland_no_portal", true, false),
    say("wayland_no_portal", true, true),
  ]);
  expect(said.size).toBe(1);
  expect(say("wayland_no_portal")).toContain("GNOME 45");
  expect(say("wayland_no_portal")).toContain("or the other way round");
  expect(say("wayland_no_portal")).not.toContain("neither half");

  // And the screens clause the `xwayland` code has to carry, because it outranks
  // `monitors_unstable` in the engine's one problem slot and nothing else in this
  // interface reads `monitors_stable`.
  expect(say("xwayland")).toContain("swap places");
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
  // 10 to 60 "passes but announces the number"; the question is only worth
  // asking past the point where a pointer stops feeling like one (the epic's
  // "roughly 40 ms"), which #123's measured 32 ms relay is still inside.
  expect(pointerVerdict(11)).toBe("announce");
  expect(pointerVerdict(32)).toBe("announce");
  expect(pointerVerdict(POINTER_WARN_MS)).toBe("announce");
  expect(pointerVerdict(POINTER_WARN_MS + 1)).toBe("warn");
  expect(pointerVerdict(POINTER_MAX_MS)).toBe("warn");
  expect(pointerVerdict(POINTER_MAX_MS + 1)).toBe("refuse");
});

// A relayed session is allowed, never silent: the path and the number are said.
test("the path and its number are said, and a slow one warns first", () => {
  expect(pathLine({ rtt_ms: 4, lan: true })).toBe("4 ms away, on this network.");
  const relayed = pathLine({ rtt_ms: 32, lan: false });
  expect(relayed).toContain("32 ms away, over the internet");
  expect(relayed).toContain("lag a little");
  // The same wording in the line and in the warning, for the same number.
  const slow = pathLine({ rtt_ms: 50, lan: false });
  expect(slow).toContain("lag noticeably");
  expect(slowPathWarning("Desk", 50)).toContain("lag noticeably");
  expect(slow).toContain("keyboard alone");
  const far = pathLine({ rtt_ms: 120, lan: false });
  expect(far).toContain("keyboard alone");
  expect(slowPathWarning("Desk", 50)).toContain("50 ms");
});

// Nothing measured is not a route: a computer that has never answered gets no
// claim about how a session to it would travel.
test("an unmeasured path claims no route", () => {
  expect(pathLine({ rtt_ms: null, lan: false })).toBe("Nothing measured yet.");
  expect(pathLine({ rtt_ms: null, lan: true })).toBe(
    "On this network. Nothing measured yet.",
  );
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
  // The chain's own order (section 7): corners, modifier, double tap, dwell.
  expect(strict[0]).toBe("Only while Ctrl is held.");
  expect(strict[1]).toContain("comes straight back");
  expect(strict[2]).toContain("As soon as");
  expect(strict.join(" ")).not.toContain("corners");
  const all = guardWords({ require_mods: MODS.alt, double_tap_ms: 200 });
  expect(all[0]).toContain("corners");
  expect(all[1]).toContain("Alt");
  expect(all[2]).toContain("comes straight back");
  expect(all[3]).toContain("short pause");
});

// A crossing into a screen that is away is a wall whatever the guards say, so
// describing a dwell there would describe something that cannot happen.
// A shared stretch no longer than the corners left alone at both ends of it
// admits nothing, and the remedy is one of the toggles beside it.
test("a crossing too short for its own corners says so", () => {
  expect(tooShortToCross(20, {})).toContain("cannot cross there");
  expect(tooShortToCross(20, {})).toContain("Untick the corners");
  expect(tooShortToCross(20, { dead_corner: 0 })).toBeNull();
  expect(tooShortToCross(400, {})).toBeNull();
  // Exactly twice the corner is still nothing left in the middle.
  expect(tooShortToCross(32, {})).not.toBeNull();
  expect(tooShortToCross(33, {})).toBeNull();
});

test("a crossing into a screen that is away is reported as the wall it is", () => {
  const said = guardWords({ dwell_ms: 250 }, true);
  expect(said).toHaveLength(1);
  expect(said[0]).toContain("stops at that edge");
  expect(said[0]).toContain("not connected right now");
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

// Two blocks stacked one under the other: the facing edges are on the y axis, so
// the x snap has nothing to face and lines the near edges up instead.
test("a block dropped under another lines its edge up", () => {
  const spots = [spot(`${A}/A1`, 0, 0), spot(`${B}/B1`, 4000, 4000)];
  const out = dropSpots(spots, [`${B}/B1`], -4000 + 9, -4000 + 1080 + 4);
  expect(out.ok).toBe(true);
  if (!out.ok) return;
  expect(out.spots[1]).toEqual({ monitor: `${B}/B1`, x: 0, y: 1080 });
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
  const outcome = reimportBlock(spots, "d_b", own);
  expect(outcome.ok).toBe(true);
  const back = outcome.ok ? outcome.spots : [];
  expect(back).toEqual([
    { monitor: `${A}/A1`, x: 0, y: 0 },
    { monitor: `${B}/B1`, x: 4000, y: 0 },
    { monitor: `${B}/B2`, x: 5920, y: 0 },
  ]);
});

test("putting a block back leaves every other screen exactly where it was", () => {
  const spots = [spot(`${A}/A1`, -100, -200), spot(`${B}/B1`, 4000, 0)];
  const untouched = [
    { monitor: `${A}/A1`, x: -100, y: -200 },
    { monitor: `${B}/B1`, x: 4000, y: 0 },
  ];
  expect(reimportBlock(spots, "d_b", [])).toEqual({ ok: true, spots: untouched });
  expect(reimportBlock(spots, "d_nobody", [{ id: "X", x: 0, y: 0 }])).toEqual({
    ok: true,
    spots: untouched,
  });
});

// A block put back can land on a screen that moved in while it was scattered, and
// an overlap is an overlap however it was made: the same door as a drag.
test("putting a block back is refused when it would land on another screen", () => {
  const spots = [
    // A's screen sits exactly where B's second screen wants to come back to.
    spot(`${A}/A1`, 5920, 0),
    { ...spot(`${B}/B1`, 4000, 0), device_id: "d_b" },
    { ...spot(`${B}/B2`, 4000, 2000), device_id: "d_b" },
  ];
  const own = [
    { id: "B1", x: 0, y: 0 },
    { id: "B2", x: 1920, y: 0 },
  ];
  expect(reimportBlock(spots, "d_b", own)).toEqual({
    ok: false,
    reason: "overlap",
  });
});

// A plane that arrived already overlapping (dragged on another computer, or
// written by another interface) must stay draggable: refusing the one gesture
// that could repair it would leave nothing to do at all.
test("an overlap that was already there does not block the drag that repairs it", () => {
  const spots = [spot(`${A}/A1`, 0, 0), spot(`${B}/B1`, 500, 0)];
  // Nudged a little, still overlapping: not this drag's doing.
  const nudged = dropSpots(spots, [`${B}/B1`], 40, 0);
  expect(nudged.ok).toBe(true);
  // And dragged clear of it, which is the repair.
  const clear = dropSpots(spots, [`${B}/B1`], 1420, 0);
  expect(clear.ok).toBe(true);
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

// A delta that is not a number passes every bound (`Math.abs(NaN) > n` is false)
// and would reach the engine as a JSON null.
test("a drag of something that is not a number moves nothing", () => {
  const spots = [spot(`${A}/A1`, 0, 0)];
  expect(dropSpots(spots, [`${A}/A1`], Number.NaN, 0).ok).toBe(false);
  expect(dropSpots(spots, [`${A}/A1`], 0, Number.POSITIVE_INFINITY).ok).toBe(false);
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
