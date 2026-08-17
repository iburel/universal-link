// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

import { afterEach, beforeEach, expect, test, vi } from "vitest";

import type { Device, InputSpot, InputState } from "../lib/api";
import type { ConnectionStatus } from "../lib/core";
import {
  byLabel,
  byText,
  cleanup,
  click,
  render,
  settle,
  textOf,
} from "../lib/harness";
import { CoreStore } from "../lib/store.svelte";
import Input from "./Input.svelte";

const CONNECTED: ConnectionStatus = {
  status: "connected",
  granted_scopes: ["input.read", "input.manage"],
  api_version: 1,
};

const NODE_A = "a".repeat(64);
const NODE_B = "b".repeat(64);

const SELF: Device = {
  device_id: "d_self",
  name: "Laptop",
  platform: "linux",
  online: true,
  lan: true,
  reachable: true,
  is_self: true,
};

function spot(over: Partial<InputSpot> & { monitor: string }): InputSpot {
  return {
    device_id: "d_self",
    name: "Built in",
    x: 0,
    y: 0,
    w: 1920,
    h: 1080,
    present: true,
    primary: true,
    ...over,
  };
}

function state(over: Partial<InputState> = {}): InputState {
  return {
    here: {
      device_id: "d_self",
      name: "Laptop",
      monitors: [
        {
          id: "A1",
          name: "Built in",
          w: 1920,
          h: 1080,
          x: 0,
          y: 0,
          scale: 1000,
          primary: true,
          present: true,
        },
      ],
      problem: null,
      can_drive: true,
      can_be_driven: true,
    },
    plane: {
      id: "f".repeat(32),
      by: null,
      spots: [
        spot({ monitor: `${NODE_A}/A1` }),
        spot({
          monitor: `${NODE_B}/B1`,
          device_id: "d_desk",
          name: "Main",
          x: 1920,
          y: 0,
        }),
      ],
    },
    devices: [
      {
        device_id: "d_desk",
        name: "Desk",
        state: "ready",
        monitors: [
          {
            id: "B1",
            name: "Main",
            w: 1920,
            h: 1080,
            x: 0,
            y: 0,
            scale: 1000,
            primary: true,
            present: true,
          },
        ],
        rtt_ms: 4,
        lan: true,
        allowed: false,
        drive: false,
        mode: "typing",
        problem: null,
      },
    ],
    session: null,
    guards: [],
    lock: false,
    hotkey: ["ctrl", "alt", "Escape"],
    ...over,
  };
}

/** The fixture's peer, ticked in "who this computer may drive" and ready. */
function drivable(over: Partial<InputState["devices"][number]> = {}) {
  return { devices: [{ ...state().devices[0], drive: true, ...over }] };
}

let store: CoreStore;

function ready(over: Partial<InputState> = {}) {
  store.connection = CONNECTED;
  store.primed = true;
  store.devices = [SELF];
  store.input = state(over);
  store.inputSeen = true;
}

beforeEach(() => {
  store = new CoreStore();
  ready();
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

// --- The live state ---------------------------------------------------------

test("with the keyboard here, the tab says so and offers no way back", () => {
  const view = render(Input, { store });

  expect(textOf(view)).toContain("Your keyboard and mouse are on this computer.");
  expect(() => byText(view, "button", "Bring them back")).toThrow();
});

test("while the pointer is away, the source says where it is and how to get it back", () => {
  const release = vi.spyOn(store, "releaseInput").mockResolvedValue(true);
  ready({
    session: {
      device_id: "d_desk",
      direction: "out",
      mode: "full",
      since: 1,
      rtt_ms: 4,
    },
  });

  const view = render(Input, { store });

  expect(textOf(view)).toContain(
    "Your keyboard and mouse are on Desk. Press Ctrl + Alt + Escape to bring them back.",
  );
  click(byText(view, "button", "Bring them back"));
  expect(release).toHaveBeenCalledOnce();
});

test("the target says who is using it, and offers nothing to press", () => {
  ready({
    session: {
      device_id: "d_desk",
      direction: "in",
      mode: "full",
      since: 1,
      rtt_ms: null,
    },
  });

  const view = render(Input, { store });

  expect(textOf(view)).toContain("Desk is using your keyboard and mouse right now.");
  expect(() => byText(view, "button", "Bring them back")).toThrow();
});

// --- A permission that is missing ------------------------------------------

// The normal state of a fresh Mac. It must say what does not work rather than
// pretend, and name the pane the grant lives in.
test("a Mac without the Accessibility grant says what does not work", () => {
  store.devices = [{ ...SELF, platform: "macos" }];
  ready({
    here: { ...state().here, problem: "no_permission", can_be_driven: false },
  });
  store.devices = [{ ...SELF, platform: "macos" }];

  const view = render(Input, { store });

  const said = textOf(view);
  expect(said).toContain("may not type on this computer");
  expect(said).toContain("Accessibility");
  expect(view.querySelector('[role="alert"]')).toBeTruthy();
});

test("a Wayland session is told, and the rest of the tab still works", () => {
  ready({ here: { ...state().here, problem: "wayland", can_drive: false } });

  const view = render(Input, { store });

  expect(textOf(view)).toContain("Wayland");
  expect(textOf(view)).toContain("Who may drive this computer");
});

// --- The plane --------------------------------------------------------------

test("every screen of every computer is drawn, and this one is marked", () => {
  const view = render(Input, { store });

  const screens = view.querySelectorAll(".screen");
  expect(screens).toHaveLength(2);
  expect(textOf(view)).toContain("Laptop");
  expect(textOf(view)).toContain("Desk");
  expect(textOf(view)).toContain("you are here");
  expect(view.querySelectorAll(".screen.mine")).toHaveLength(1);
});

// A screen that is away keeps its place and says so, in the epic's words.
// Two identical monitors report the same name, which is the model's: both boxes
// would read the same and nobody could tell which one they were dragging.
test("two screens of one machine with the same name are numbered", () => {
  ready({
    plane: {
      id: "f".repeat(32),
      by: null,
      spots: [
        spot({ monitor: `${NODE_A}/A1`, name: "U2720Q" }),
        spot({ monitor: `${NODE_A}/A2`, name: "U2720Q", x: 1920, primary: false }),
        spot({ monitor: `${NODE_B}/B1`, device_id: "d_desk", name: "U2720Q", x: 3840 }),
      ],
    },
  });

  const view = render(Input, { store });

  expect(byLabel(view, "Laptop U2720Q (1)")).toBeTruthy();
  expect(byLabel(view, "Laptop U2720Q (2)")).toBeTruthy();
  // The other machine's screen shares the name but not the machine: no number.
  expect(byLabel(view, "Desk U2720Q")).toBeTruthy();
});

test("a screen that is away keeps its place and says so", () => {
  ready({
    plane: {
      id: "f".repeat(32),
      by: null,
      spots: [
        spot({ monitor: `${NODE_A}/A1` }),
        spot({
          monitor: `${NODE_B}/B1`,
          device_id: "d_desk",
          name: "",
          x: 1920,
          y: 0,
          present: false,
        }),
      ],
    },
  });

  const view = render(Input, { store });

  expect(view.querySelectorAll(".screen.ghost")).toHaveLength(1);
  expect(textOf(view)).toContain(
    "This screen is not connected right now. Its place is kept.",
  );
  // Its place: still drawn where it was, not collapsed away.
  const ghost = view.querySelector(".screen.ghost") as HTMLElement;
  expect(ghost.style.left).not.toBe("0px");
});

// A box of empty space below the screens reads as a plane with somewhere to drag
// to, and there is not: it is as tall as the arrangement.
test("the plane is as tall as the arrangement, not as tall as its budget", () => {
  const view = render(Input, { store });

  const plane = view.querySelector(".plane") as HTMLElement;
  // Two 1920x1080 screens side by side: 3840 wide, 1080 tall, so the height is
  // the width's ratio away from 620 and nowhere near the 300 budget.
  expect(plane.style.height).toBe("174px");
  expect(plane.style.width).toBe("620px");
});

test("the plane invites the drag, and says how to detach one screen", () => {
  const view = render(Input, { store });

  expect(textOf(view)).toContain("Drag a screen to say where it really is.");
  expect(byLabel(view, "Laptop Built in")).toBeTruthy();
  expect(textOf(view)).toContain("Move one screen at a time");
});

/** A drag of `screen` by `dx`, `dy` CSS pixels, mouse down to mouse up. */
function dragBy(screen: Element, dx: number, dy: number) {
  screen.dispatchEvent(
    new MouseEvent("mousedown", { bubbles: true, clientX: 0, clientY: 0 }),
  );
  window.dispatchEvent(
    new MouseEvent("mousemove", { bubbles: true, clientX: dx, clientY: dy }),
  );
  window.dispatchEvent(new MouseEvent("mouseup", { bubbles: true }));
}

test("dragging a screen sends the whole arrangement, every spot included", async () => {
  const place = vi.spyOn(store, "placeScreens").mockResolvedValue(true);
  const view = render(Input, { store });

  dragBy(byLabel(view, "Desk Main"), 40, 0);
  await Promise.resolve();

  expect(place).toHaveBeenCalledOnce();
  const sent = place.mock.calls[0][0];
  expect(sent.map((s) => s.monitor)).toEqual([
    `${NODE_A}/A1`,
    `${NODE_B}/B1`,
  ]);
  // Moved to the right, and the screen that was not dragged did not move.
  expect(sent[1].x).toBeGreaterThan(1920);
  expect(sent[0]).toEqual({ monitor: `${NODE_A}/A1`, x: 0, y: 0 });
});

test("a drag that would put two screens in the same place is refused, and nothing is sent", async () => {
  const place = vi.spyOn(store, "placeScreens").mockResolvedValue(true);
  const view = render(Input, { store });

  // Far to the left: right on top of this computer's own screen.
  dragBy(byLabel(view, "Desk Main"), -200, 0);
  await Promise.resolve();

  expect(place).not.toHaveBeenCalled();
  expect(textOf(view)).toContain("Two screens cannot be in the same place");
});

// Leaving the section with the mouse still down: the two window listeners a drag
// holds must go with it, or a mouseup afterwards writes a placement from a
// component nobody is looking at.
test("leaving the tab in the middle of a drag takes the drag with it", async () => {
  const place = vi.spyOn(store, "placeScreens").mockResolvedValue(true);
  const view = render(Input, { store });

  const screen = byLabel(view, "Desk Main");
  screen.dispatchEvent(
    new MouseEvent("mousedown", { bubbles: true, clientX: 0, clientY: 0 }),
  );
  window.dispatchEvent(
    new MouseEvent("mousemove", { bubbles: true, clientX: 40, clientY: 0 }),
  );
  cleanup();
  window.dispatchEvent(new MouseEvent("mouseup", { bubbles: true }));
  await Promise.resolve();

  expect(place).not.toHaveBeenCalled();
});

test("a click that moves nothing sends nothing", async () => {
  const place = vi.spyOn(store, "placeScreens").mockResolvedValue(true);
  const view = render(Input, { store });

  dragBy(byLabel(view, "Desk Main"), 0, 0);
  await Promise.resolve();

  expect(place).not.toHaveBeenCalled();
});

// A machine's screens move together, unless the person says otherwise: a laptop's
// external screen may genuinely sit next to another computer.
test("a whole machine's screens move together, and one can be detached", async () => {
  const place = vi.spyOn(store, "placeScreens").mockResolvedValue(true);
  ready({
    plane: {
      id: "f".repeat(32),
      by: null,
      spots: [
        spot({ monitor: `${NODE_A}/A1` }),
        spot({ monitor: `${NODE_A}/A2`, name: "Second", x: 1920, y: 0 }),
        spot({
          monitor: `${NODE_B}/B1`,
          device_id: "d_desk",
          name: "Main",
          x: 6000,
          y: 0,
        }),
      ],
    },
  });
  const view = render(Input, { store });

  dragBy(byLabel(view, "Laptop Built in"), 0, 40);
  await Promise.resolve();
  let sent = place.mock.calls[0][0];
  expect(sent[0].y).toBeGreaterThan(0);
  expect(sent[1].y, "the whole block moved").toBe(sent[0].y);

  // Now one screen at a time: the block's other screen stays put.
  click(byLabel(view, "Move one screen at a time"));
  dragBy(byLabel(view, "Laptop Second"), 0, 40);
  await Promise.resolve();
  sent = place.mock.calls[1][0];
  expect(sent[1].y).toBeGreaterThan(0);
  expect(sent[0].y).toBe(0);
});

test("arrow keys move a screen for whoever is not holding a mouse", async () => {
  const place = vi.spyOn(store, "placeScreens").mockResolvedValue(true);
  const view = render(Input, { store });

  byLabel(view, "Desk Main").dispatchEvent(
    new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }),
  );
  await Promise.resolve();

  const sent = place.mock.calls[0][0];
  expect(sent[1]).toEqual({ monitor: `${NODE_B}/B1`, x: 1920, y: 1080 });
});

// The arrangement is imported rather than re-invented, so there is a way back.
test("a machine's own arrangement can be put back", async () => {
  const place = vi.spyOn(store, "placeScreens").mockResolvedValue(true);
  const desk = state().devices[0];
  ready({
    devices: [
      {
        ...desk,
        monitors: [
          { ...desk.monitors[0], id: "B1", x: 0, y: 0 },
          { ...desk.monitors[0], id: "B2", name: "Second", x: 1920, y: 0, primary: false },
        ],
      },
    ],
    plane: {
      id: "f".repeat(32),
      by: null,
      spots: [
        spot({ monitor: `${NODE_A}/A1` }),
        spot({ monitor: `${NODE_B}/B1`, device_id: "d_desk", name: "Main", x: 4000, y: 0 }),
        spot({
          monitor: `${NODE_B}/B2`,
          device_id: "d_desk",
          name: "Second",
          x: 4000,
          y: 2000,
        }),
      ],
    },
  });
  const view = render(Input, { store });

  click(byText(view, "button", "Put Desk's screens back"));
  await Promise.resolve();

  const sent = place.mock.calls[0][0];
  expect(sent[1]).toEqual({ monitor: `${NODE_B}/B1`, x: 4000, y: 0 });
  expect(sent[2]).toEqual({ monitor: `${NODE_B}/B2`, x: 5920, y: 0 });
});

// --- The switches -----------------------------------------------------------

// The machine being driven is the only authority, and the interface must not
// imply that the other list grants anything on the far side.
test("the two lists are told apart: one decides, the other only offers", () => {
  const view = render(Input, { store });

  const said = textOf(view);
  expect(said).toContain("Who may drive this computer");
  expect(said).toContain("This is the list that decides");
  expect(said).toContain("only if it is");
  expect(said).toContain("ticked here, on this computer");
  expect(said).toContain("Who this computer may drive");
  expect(said).toContain("It grants");
  expect(said).toContain("nothing over there");
  expect(said).toContain("finds out by trying");
});

test("the authority's switch is the one stored here", () => {
  const allow = vi.spyOn(store, "allowInput").mockResolvedValue(true);
  const drive = vi.spyOn(store, "driveInput").mockResolvedValue(true);
  const view = render(Input, { store });

  click(byLabel(view, "Let Desk drive this computer"));
  expect(allow).toHaveBeenCalledWith("d_desk", true);

  click(byLabel(view, "Let this computer drive Desk"));
  expect(drive).toHaveBeenCalledWith("d_desk", true);
});

test("a switch already on turns off", () => {
  const allow = vi.spyOn(store, "allowInput").mockResolvedValue(true);
  ready({ devices: [{ ...state().devices[0], allowed: true }] });

  const view = render(Input, { store });
  click(byLabel(view, "Let Desk drive this computer"));

  expect(allow).toHaveBeenCalledWith("d_desk", false);
});

test("with no other computer, the lists say so instead of standing empty", () => {
  ready({ devices: [] });

  const view = render(Input, { store });

  expect(textOf(view)).toContain("No other computer on your account yet.");
});

// --- Each computer ----------------------------------------------------------

test("a pair shows its path and its measured latency", () => {
  const view = render(Input, { store });

  expect(textOf(view)).toContain("4 ms away, on this network");
});

test("a pair's standing problem is said, with the remedy on the right machine", () => {
  ready({ devices: [{ ...state().devices[0], problem: "not_allowed" }] });

  const view = render(Input, { store });

  expect(textOf(view)).toContain("has not been told to accept your keyboard");
  expect(textOf(view)).toContain("on Desk itself");
});

test("a deployment whose relays will not carry a session says so", () => {
  ready({ devices: [{ ...state().devices[0], problem: "no_path" }] });

  const view = render(Input, { store });

  expect(textOf(view)).toContain("do not carry a keyboard session");
});

test("taking control on a fast path just takes it", () => {
  const take = vi.spyOn(store, "takeInput").mockResolvedValue(true);
  ready(drivable());
  const view = render(Input, { store });

  click(byLabel(view, "Take control of Desk"));

  expect(take).toHaveBeenCalledWith("d_desk");
});

// The engine only warms a channel to a computer this one may drive and that its
// directory calls reachable, and a take without both is accepted, parked and
// never spoken of again. So the button is not offered, and the reason is.
test("a computer this one may not drive is not offered a take, and says why", () => {
  const take = vi.spyOn(store, "takeInput").mockResolvedValue(true);
  const view = render(Input, { store });

  expect(() => byLabel(view, "Take control of Desk")).toThrow();
  expect(() => byLabel(view, "Send the keyboard alone to Desk")).toThrow();
  expect(textOf(view)).toContain('Tick Desk under "Who this computer may drive"');
  expect(take).not.toHaveBeenCalled();
});

test("a computer that is not answering is not offered a take either", () => {
  ready(drivable({ state: "off" }));
  const view = render(Input, { store });

  expect(() => byLabel(view, "Take control of Desk")).toThrow();
  expect(textOf(view)).toContain("is not answering right now");
});

// The relay is allowed, never silently: a slow path warns BEFORE the pointer
// goes, and the offer alongside the warning is the keyboard alone.
test("a slow path warns before the pointer goes, and offers the keyboard alone", () => {
  const take = vi.spyOn(store, "takeInput").mockResolvedValue(true);
  ready(drivable({ rtt_ms: 50, lan: false }));
  const view = render(Input, { store });

  click(byLabel(view, "Take control of Desk"));
  expect(take).not.toHaveBeenCalled();
  expect(textOf(view)).toContain("50 ms away");
  expect(textOf(view)).toContain("lag noticeably");

  click(byLabel(view, "Send the keyboard alone to Desk"));
  expect(take).toHaveBeenCalledWith("d_desk", "keys");
});

test("a slow path can still be taken deliberately", () => {
  const take = vi.spyOn(store, "takeInput").mockResolvedValue(true);
  ready(drivable({ rtt_ms: 50, lan: false }));
  const view = render(Input, { store });

  click(byLabel(view, "Take control of Desk"));
  click(byLabel(view, "Take the pointer to Desk anyway"));

  expect(take).toHaveBeenCalledWith("d_desk", "full");
});

// Above the threshold the engine refuses the pointer outright, so the interface
// does not offer a button that cannot succeed.
// A measured number inside the band that "passes but announces it" is not worth
// a question: #123 measured 32 ms over a relay, and the epic's own line is that a
// pointer stops feeling like one at roughly 40.
test("an ordinary relay is announced rather than questioned", () => {
  const take = vi.spyOn(store, "takeInput").mockResolvedValue(true);
  ready(drivable({ rtt_ms: 32, lan: false }));
  const view = render(Input, { store });

  expect(textOf(view)).toContain("32 ms away, over the internet");
  click(byLabel(view, "Take control of Desk"));
  expect(take).toHaveBeenCalledWith("d_desk");
});

test("past the threshold the pointer is not offered at all", () => {
  ready(drivable({ rtt_ms: 120, lan: false }));
  const view = render(Input, { store });

  click(byLabel(view, "Take control of Desk"));

  expect(textOf(view)).toContain("120 ms away");
  expect(() => byLabel(view, "Take the pointer to Desk anyway")).toThrow();
  expect(byLabel(view, "Send the keyboard alone to Desk")).toBeTruthy();
});

test("the computer being driven offers the way back rather than a second take", () => {
  const release = vi.spyOn(store, "releaseInput").mockResolvedValue(true);
  ready({
    devices: [{ ...state().devices[0], drive: true, state: "driving" }],
    session: {
      device_id: "d_desk",
      direction: "out",
      mode: "full",
      since: 1,
      rtt_ms: 4,
    },
  });
  const view = render(Input, { store });

  expect(textOf(view)).toContain("Your keyboard and mouse are there");
  click(byText(view, "button", "Bring my keyboard back"));
  expect(release).toHaveBeenCalled();
});

// --- The guards -------------------------------------------------------------

test("the guards of a crossing are shown in words, not in milliseconds", () => {
  const view = render(Input, { store });

  const said = textOf(view);
  expect(said).toContain("When the pointer crosses to Desk");
  expect(said).toContain("short pause at the edge");
  expect(said).toContain("corners");
  expect(said).not.toContain("250 ms");
});

test("a wall is set per crossing, and says the whole truth on its own", async () => {
  const guards = vi.spyOn(store, "setGuards").mockResolvedValue(true);
  const view = render(Input, { store });

  click(byLabel(view, "Never cross to Desk"));
  await Promise.resolve();

  expect(guards).toHaveBeenCalledWith(
    "d_desk",
    `${NODE_B}/B1`,
    "right",
    expect.objectContaining({ wall: true, dwell_ms: 250 }),
  );
});

test("the double tap and the dead corners are toggles", async () => {
  const guards = vi.spyOn(store, "setGuards").mockResolvedValue(true);
  const view = render(Input, { store });

  click(byLabel(view, "Ask for a double tap toward Desk"));
  await Promise.resolve();
  expect(guards).toHaveBeenLastCalledWith(
    "d_desk",
    `${NODE_B}/B1`,
    "right",
    expect.objectContaining({ double_tap_ms: 300 }),
  );

  click(byLabel(view, "Keep the corners free toward Desk"));
  await Promise.resolve();
  expect(guards).toHaveBeenLastCalledWith(
    "d_desk",
    `${NODE_B}/B1`,
    "right",
    expect.objectContaining({ dead_corner: 0 }),
  );
});

test("a stored wall is shown as set, and its words replace the rest", () => {
  ready({
    guards: [
      {
        device_id: "d_desk",
        monitor: `${NODE_B}/B1`,
        side: "right",
        dwell_ms: 250,
        double_tap_ms: 0,
        dead_corner: 16,
        require_mods: 0,
        wall: true,
      },
    ],
  });

  const view = render(Input, { store });

  expect(textOf(view)).toContain("The pointer never crosses here.");
  expect(byLabel(view, "Never cross to Desk")).toHaveProperty("checked", true);
});

// A crossing into a screen that is away is a wall, so the guards are not offered
// as settings there: they are reported as the wall.
test("a neighbour whose only screen is away is a wall, not a set of guards", () => {
  ready({
    plane: {
      id: "f".repeat(32),
      by: null,
      spots: [
        spot({ monitor: `${NODE_A}/A1` }),
        spot({
          monitor: `${NODE_B}/B1`,
          device_id: "d_desk",
          name: "",
          x: 1920,
          y: 0,
          present: false,
        }),
      ],
    },
  });

  const view = render(Input, { store });

  expect(textOf(view)).toContain("The pointer stops at that edge");
  expect(() => byLabel(view, "Never cross to Desk")).toThrow();
  expect(() => byLabel(view, "Ask for a double tap toward Desk")).toThrow();
});

// A pair whose screens do not touch cannot be crossed to, and saying nothing
// would leave someone waiting at an edge for ever.
test("a pair with no shared edge says why the pointer cannot cross", () => {
  ready({
    plane: {
      id: "f".repeat(32),
      by: null,
      spots: [
        spot({ monitor: `${NODE_A}/A1` }),
        spot({
          monitor: `${NODE_B}/B1`,
          device_id: "d_desk",
          name: "Main",
          x: 9000,
          y: 0,
        }),
      ],
    },
  });

  const view = render(Input, { store });

  expect(textOf(view)).toContain("No edge of your screens touches");
  expect(() => byLabel(view, "Never cross to Desk")).toThrow();
});

// --- This computer ----------------------------------------------------------

test("the pointer can be pinned to this computer, and the reason is given", () => {
  const lock = vi.spyOn(store, "lockPointer").mockResolvedValue(true);
  const view = render(Input, { store });

  expect(textOf(view)).toContain("game or a virtual machine");
  click(byLabel(view, "Keep the pointer on this screen"));

  expect(lock).toHaveBeenCalledWith(true);
});

test("the return hotkey is shown, changeable, and said to be local", () => {
  const hotkey = vi.spyOn(store, "setHotkey").mockResolvedValue(true);
  const view = render(Input, { store });

  const select = byLabel(view, "The key that brings your keyboard back") as HTMLSelectElement;
  expect(select.value).toBe("Ctrl + Alt + Escape");
  expect(textOf(view)).toContain("This computer enforces it on its own");

  select.value = "Ctrl + Shift + Home";
  select.dispatchEvent(new Event("change", { bubbles: true }));
  expect(hotkey).toHaveBeenCalledWith(["ctrl", "shift", "Home"]);
});

test("a hotkey set from somewhere else is listed as it is", () => {
  ready({ hotkey: ["ctrl", "alt", "F7"] });

  const view = render(Input, { store });

  const select = byLabel(view, "The key that brings your keyboard back") as HTMLSelectElement;
  expect(select.value).toBe("Ctrl + Alt + F7");
});

// --- Nothing to show -------------------------------------------------------

test("an engine that is not running says so, and offers nothing", () => {
  store.input = null;
  const view = render(Input, { store });

  expect(textOf(view)).toContain("engine is not running on this computer");
  expect(view.querySelectorAll(".screen")).toHaveLength(0);
  expect(view.querySelectorAll("input")).toHaveLength(0);
});

test("without the Core, nothing can be changed", () => {
  ready(drivable());
  store.connection = { status: "connecting" };
  const view = render(Input, { store });

  expect(byLabel(view, "Let Desk drive this computer")).toHaveProperty(
    "disabled",
    true,
  );
  expect(byLabel(view, "Take control of Desk")).toHaveProperty("disabled", true);
});

test("a drag is inert while the Core is away", async () => {
  const place = vi.spyOn(store, "placeScreens").mockResolvedValue(true);
  store.connection = { status: "connecting" };
  const view = render(Input, { store });

  dragBy(byLabel(view, "Desk Main"), 40, 0);
  await Promise.resolve();

  expect(place).not.toHaveBeenCalled();
});

// --- What the engine says, and only that ------------------------------------

// A tick the engine refused must not be left standing: on the list that decides
// who may type here, that would be the interface asserting a grant nobody holds.
test("a refused switch goes back to what the engine says", async () => {
  vi.spyOn(store, "allowInput").mockResolvedValue(false);
  const view = render(Input, { store });

  const box = byLabel(view, "Let Desk drive this computer") as HTMLInputElement;
  expect(box.checked).toBe(false);
  box.click();
  expect(box.checked).toBe(true);
  await Promise.resolve();
  await Promise.resolve();

  expect(box.checked).toBe(false);
});

test("a switch the engine took goes on waiting for the engine's word", async () => {
  vi.spyOn(store, "allowInput").mockResolvedValue(true);
  const view = render(Input, { store });

  const box = byLabel(view, "Let Desk drive this computer") as HTMLInputElement;
  box.click();
  await Promise.resolve();
  await Promise.resolve();

  // Not put back: the engine accepted, and its notification is what will confirm
  // it. Putting it back here would make the happy path flicker.
  expect(box.checked).toBe(true);
});

// A keyboard-only session never moved the mouse, and the row must not say it did.
test("a keyboard-only session is not described as a pointer one", () => {
  ready({
    devices: [{ ...state().devices[0], drive: true, state: "driving" }],
    session: {
      device_id: "d_desk",
      direction: "out",
      mode: "keys",
      since: 1,
      rtt_ms: 4,
    },
  });

  const view = render(Input, { store });

  expect(textOf(view)).toContain("Your keyboard is there");
  expect(textOf(view)).not.toContain("Your keyboard and mouse are there");
});

// The machine being driven: the two takes would both be refused INPUT_BUSY every
// time, and the gesture that works was offered nowhere.
test("the machine being driven is offered the one gesture that works", () => {
  const release = vi.spyOn(store, "releaseInput").mockResolvedValue(true);
  ready({
    devices: [{ ...state().devices[0], state: "driven" }],
    session: {
      device_id: "d_desk",
      direction: "in",
      mode: "full",
      since: 1,
      rtt_ms: 4,
    },
  });

  const view = render(Input, { store });

  expect(() => byLabel(view, "Take control of Desk")).toThrow();
  click(byLabel(view, "Take my keyboard back from Desk"));
  expect(release).toHaveBeenCalledOnce();
});

// The dwell is the primary guard of the chain, and a millisecond count is not
// what anybody is choosing: three intentions, and the engine's own numbers.
test("the dwell is chosen in words", async () => {
  const guards = vi.spyOn(store, "setGuards").mockResolvedValue(true);
  const view = render(Input, { store });

  const select = byLabel(
    view,
    "When the pointer crosses to Desk",
  ) as HTMLSelectElement;
  expect(select.value).toBe("250");
  select.value = "600";
  select.dispatchEvent(new Event("change", { bubbles: true }));
  await Promise.resolve();

  expect(guards).toHaveBeenCalledWith(
    "d_desk",
    `${NODE_B}/B1`,
    "right",
    expect.objectContaining({ dwell_ms: 600 }),
  );
});

test("a held modifier is chosen by name", async () => {
  const guards = vi.spyOn(store, "setGuards").mockResolvedValue(true);
  const view = render(Input, { store });

  const select = byLabel(
    view,
    "The key to hold to cross to Desk",
  ) as HTMLSelectElement;
  expect(select.value).toBe("0");
  select.value = "2";
  select.dispatchEvent(new Event("change", { bubbles: true }));
  await Promise.resolve();

  expect(guards).toHaveBeenCalledWith(
    "d_desk",
    `${NODE_B}/B1`,
    "right",
    expect.objectContaining({ require_mods: 2 }),
  );
});

// A crossing whose shared stretch is used up by the corners left alone at both
// ends admits nothing, and the remedy is the toggle right beside it.
test("a crossing too short for its corners says so", () => {
  ready({
    plane: {
      id: "f".repeat(32),
      by: null,
      spots: [
        spot({ monitor: `${NODE_A}/A1` }),
        // Twenty pixels of shared edge, which the 16 pixel corners eat.
        spot({
          monitor: `${NODE_B}/B1`,
          device_id: "d_desk",
          name: "Main",
          x: 1920,
          y: 1060,
        }),
      ],
    },
  });

  const view = render(Input, { store });

  expect(textOf(view)).toContain("pixels of that edge are shared");
  expect(textOf(view)).toContain("Untick the corners");
});

// Who arranged the plane, because a placement is adopted from a signature this
// computer may not be able to verify, and a human noticing is the whole repair.
test("the plane says who arranged it", () => {
  ready({
    plane: { ...state().plane, by: "d_desk" },
  });

  const view = render(Input, { store });

  expect(textOf(view)).toContain("Arranged by Desk.");
});

// One computer on the plane: dragging its own screens says nothing to anybody,
// and inviting a drag would be inviting a gesture with no meaning.
test("with only this computer on the plane, the invitation is a different one", () => {
  ready({
    plane: {
      id: "f".repeat(32),
      by: null,
      spots: [spot({ monitor: `${NODE_A}/A1` })],
    },
    devices: [],
  });

  const view = render(Input, { store });

  expect(textOf(view)).toContain("only this computer's screens here");
  expect(textOf(view)).not.toContain("Drag a screen to say where it really is");
});

// The plane can go while the mouse is down (the engine restarting takes the
// snapshot away), and an arrangement of NOTHING replaces the placement of every
// computer on the account.
test("a drag that outlives the plane sends nothing at all", async () => {
  const place = vi.spyOn(store, "placeScreens").mockResolvedValue(true);
  const view = render(Input, { store });

  const screen = byLabel(view, "Desk Main");
  screen.dispatchEvent(
    new MouseEvent("mousedown", { bubbles: true, clientX: 0, clientY: 0 }),
  );
  window.dispatchEvent(
    new MouseEvent("mousemove", { bubbles: true, clientX: 40, clientY: 0 }),
  );
  store.input = null;
  settle();
  window.dispatchEvent(new MouseEvent("mouseup", { bubbles: true }));
  await Promise.resolve();

  expect(place).not.toHaveBeenCalled();
});
