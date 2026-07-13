// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

// Drag-and-drop: the pure logic (`handleFileDrag`) without DOM or Tauri, and
// the hit-test (`deviceAtPoint`) with `elementFromPoint` mocked — happy-dom
// computes no geometry, so the real position is out of a test's reach.

import { afterEach, beforeEach, expect, test, vi } from "vitest";

import {
  deviceAtPoint,
  handleFileDrag,
  installFileDrop,
  toFileDrag,
  type DragSink,
  type FileDrag,
} from "./dragdrop";

// The Tauri webview, mocked: installFileDrop imports it dynamically.
const { onDragDropEvent } = vi.hoisted(() => ({ onDragDropEvent: vi.fn() }));
vi.mock("@tauri-apps/api/webview", () => ({
  getCurrentWebview: () => ({ onDragDropEvent }),
}));

// Mocked window: Tauri's innerSize = the WHOLE frame (title bar included), in
// physical px; scale 2. With window.innerHeight = 600 (viewport), the bar is
// 1256/2 − 600 = 28 CSS px; width 1800/2 − 900 = 0 (no border). Used only when
// the UA is "Mac".
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({
    innerSize: async () => ({ width: 1800, height: 1256 }),
    scaleFactor: async () => 2,
  }),
}));

// -- handleFileDrag ---------------------------------------------------------

/** An observable sink: `targetFor` returns its input if it is "valid". */
function sink(valid: string[]): DragSink & { sends: [string, string[]][] } {
  const sends: [string, string[]][] = [];
  return {
    dropTarget: null,
    targetFor: (id) => (id && valid.includes(id) ? id : null),
    sendFiles: (id, paths) => {
      sends.push([id, paths]);
    },
    sends,
  };
}

const at = (x: number, y: number): string | null => (x === 10 && y === 20 ? "d_win" : null);

test("hovering an eligible target highlights it", () => {
  const s = sink(["d_win"]);
  handleFileDrag({ type: "over", position: { x: 10, y: 20 } }, at, s);
  expect(s.dropTarget).toBe("d_win");
});

test("hovering outside a card lights up no target", () => {
  const s = sink(["d_win"]);
  s.dropTarget = "d_win";
  handleFileDrag({ type: "over", position: { x: 0, y: 0 } }, at, s);
  expect(s.dropTarget).toBeNull();
});

test("hovering an ineligible card does not highlight it", () => {
  const s = sink([]); // no valid target (offline, this PC…)
  handleFileDrag({ type: "over", position: { x: 10, y: 20 } }, at, s);
  expect(s.dropTarget).toBeNull();
});

test("leaving the window turns off the highlight", () => {
  const s = sink(["d_win"]);
  s.dropTarget = "d_win";
  handleFileDrag({ type: "leave" }, at, s);
  expect(s.dropTarget).toBeNull();
});

test("dropping on an eligible target sends and turns off the highlight", () => {
  const s = sink(["d_win"]);
  s.dropTarget = "d_win";
  handleFileDrag(
    { type: "drop", position: { x: 10, y: 20 }, paths: ["/a", "/b"] },
    at,
    s,
  );
  expect(s.sends).toEqual([["d_win", ["/a", "/b"]]]);
  expect(s.dropTarget).toBeNull();
});

test("dropping outside an eligible target sends nothing", () => {
  const s = sink([]);
  handleFileDrag(
    { type: "drop", position: { x: 10, y: 20 }, paths: ["/a"] },
    at,
    s,
  );
  expect(s.sends).toEqual([]);
});

test("a drop with no file sends nothing", () => {
  const s = sink(["d_win"]);
  const empty: FileDrag = { type: "drop", position: { x: 10, y: 20 }, paths: [] };
  handleFileDrag(empty, at, s);
  handleFileDrag({ ...empty, paths: undefined }, at, s);
  expect(s.sends).toEqual([]);
});

test("an event with no position highlights nothing", () => {
  const s = sink(["d_win"]);
  handleFileDrag({ type: "over" }, at, s);
  expect(s.dropTarget).toBeNull();
});

// -- toFileDrag -------------------------------------------------------------

test("toFileDrag normalizes each type of Tauri event", () => {
  // x ≠ y: an x/y swap would be caught.
  expect(toFileDrag({ type: "drop", paths: ["/a"], position: { x: 3, y: 7 } })).toEqual({
    type: "drop",
    paths: ["/a"],
    position: { x: 3, y: 7 },
  });
  expect(toFileDrag({ type: "enter", paths: ["/a"], position: { x: 1, y: 2 } })).toEqual({
    type: "enter",
    paths: ["/a"],
    position: { x: 1, y: 2 },
  });
  expect(toFileDrag({ type: "over", position: { x: 1, y: 2 } })).toEqual({
    type: "over",
    paths: undefined,
    position: { x: 1, y: 2 },
  });
  expect(toFileDrag({ type: "leave" })).toEqual({
    type: "leave",
    paths: undefined,
    position: undefined,
  });
});

// -- deviceAtPoint ----------------------------------------------------------

let card: HTMLElement;
let inner: HTMLElement;
const originalElementFromPoint = document.elementFromPoint;
const originalRatio = window.devicePixelRatio;

beforeEach(() => {
  card = document.createElement("div");
  card.setAttribute("data-device-id", "d_win");
  inner = document.createElement("span");
  card.appendChild(inner);
  document.body.appendChild(card);
  Object.defineProperty(window, "devicePixelRatio", {
    value: 2,
    configurable: true,
  });
});

afterEach(() => {
  document.body.innerHTML = "";
  document.elementFromPoint = originalElementFromPoint;
  Object.defineProperty(window, "devicePixelRatio", {
    value: originalRatio,
    configurable: true,
  });
  onDragDropEvent.mockReset();
  vi.restoreAllMocks();
});

test("deviceAtPoint hit-tests the CSS position as-is (no DPR division)", () => {
  const seen: [number, number][] = [];
  document.elementFromPoint = ((x: number, y: number) => {
    seen.push([x, y]);
    return inner; // the child that was hit
  }) as typeof document.elementFromPoint;

  // devicePixelRatio = 2 (Retina), but the Tauri position is ALREADY in CSS
  // pixels: it must be passed UNCHANGED to elementFromPoint. Dividing it (the
  // old bug) gave (10, 20) and shifted the target on Retina screens.
  expect(deviceAtPoint(20, 40)).toBe("d_win");
  expect(seen).toEqual([[20, 40]]);
});

test("deviceAtPoint returns null outside any card", () => {
  document.elementFromPoint = (() => document.body) as typeof document.elementFromPoint;
  expect(deviceAtPoint(20, 40)).toBeNull();
});

test("deviceAtPoint returns null when the point hits nothing", () => {
  document.elementFromPoint = (() => null) as typeof document.elementFromPoint;
  expect(deviceAtPoint(0, 0)).toBeNull();
});

// -- installFileDrop --------------------------------------------------------

test("installFileDrop registers the handler and returns the unsubscribe", async () => {
  const unlisten = () => {};
  onDragDropEvent.mockResolvedValue(unlisten);
  const s: DragSink = { dropTarget: null, targetFor: (id) => id, sendFiles: () => {} };

  const result = await installFileDrop(s);

  expect(onDragDropEvent).toHaveBeenCalledOnce();
  expect(result).toBe(unlisten); // to be called on unmount (App.svelte)
});

test("the registered handler routes a drop to sendFiles", async () => {
  onDragDropEvent.mockResolvedValue(() => {});
  const sends: [string, string[]][] = [];
  const s: DragSink = {
    dropTarget: null,
    targetFor: (id) => id,
    sendFiles: (id, paths) => void sends.push([id, paths]),
  };
  await installFileDrop(s);
  const handler = onDragDropEvent.mock.calls[0][0] as (e: { payload: unknown }) => void;

  // `card` (data-device-id="d_win") is planted by the beforeEach; elementFromPoint returns it.
  document.elementFromPoint = (() => inner) as typeof document.elementFromPoint;
  handler({ payload: { type: "drop", paths: ["/a"], position: { x: 4, y: 4 } } });

  expect(sends).toEqual([["d_win", ["/a"]]]);
});

test("on macOS, the title bar is subtracted from the drop position", async () => {
  // Tauri inconsistency: on macOS the position is relative to the FRAME (title
  // bar included). We measure it as the gap between innerSize (frame) and the
  // WebKit viewport, then subtract it (mock → 28 CSS px tall, 0 wide).
  const originalUA = navigator.userAgent;
  const originalInnerW = window.innerWidth;
  const originalInnerH = window.innerHeight;
  Object.defineProperty(navigator, "userAgent", {
    value: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)",
    configurable: true,
  });
  Object.defineProperty(window, "innerWidth", { value: 900, configurable: true });
  Object.defineProperty(window, "innerHeight", { value: 600, configurable: true });
  try {
    onDragDropEvent.mockResolvedValue(() => {});
    const seen: [number, number][] = [];
    document.elementFromPoint = ((x: number, y: number) => {
      seen.push([x, y]);
      return inner;
    }) as typeof document.elementFromPoint;
    const s: DragSink = { dropTarget: null, targetFor: (id) => id, sendFiles: () => {} };

    await installFileDrop(s);
    const handler = onDragDropEvent.mock.calls[0][0] as (e: { payload: unknown }) => void;

    // y: 48 − 28 (measured title bar) = 20; x unchanged (no left border).
    handler({ payload: { type: "over", position: { x: 10, y: 48 } } });
    expect(seen).toEqual([[10, 20]]);
  } finally {
    Object.defineProperty(navigator, "userAgent", {
      value: originalUA,
      configurable: true,
    });
    Object.defineProperty(window, "innerWidth", { value: originalInnerW, configurable: true });
    Object.defineProperty(window, "innerHeight", { value: originalInnerH, configurable: true });
  }
});

test("installFileDrop returns undefined outside a Tauri webview", async () => {
  onDragDropEvent.mockRejectedValue(new Error("no webview"));
  const s: DragSink = { dropTarget: null, targetFor: () => null, sendFiles: () => {} };

  expect(await installFileDrop(s)).toBeUndefined();
});
