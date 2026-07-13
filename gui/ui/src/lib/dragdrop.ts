// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

/**
 * Drag-and-drop of files onto a device card → send.
 *
 * Structuring constraint: only Tauri delivers the ABSOLUTE PATHS of a dropped
 * file, and it delivers them at the WINDOW level (`onDragDropEvent`), not per
 * element — the webview's HTML5 drag-and-drop exposes no disk path. Targeting a
 * specific card therefore requires hit-testing the drop position (which Tauri
 * delivers in CSS pixels) against the DOM.
 *
 * This fragility (geometry, DPI) is confined to `deviceAtPoint`. Everything
 * else — when to highlight, when to send — is pure code tested without DOM or
 * Tauri (`handleFileDrag`).
 */

/** A drag event, normalized from Tauri's `DragDropEvent`. */
export interface FileDrag {
  type: "enter" | "over" | "drop" | "leave";
  /** Present on `drop` (and `enter`); absolute OS-side paths. */
  paths?: string[];
  /** Present except on `leave`; in CSS pixels relative to the webview. */
  position?: { x: number; y: number };
}

/** What the controller drives — satisfied as-is by the CoreStore. */
export interface DragSink {
  /** The hovered target to highlight, or `null`. */
  dropTarget: string | null;
  /** `device_id` if it is an eligible send target, otherwise `null`. */
  targetFor(device_id: string | null): string | null;
  /** Triggers the send (any return value is ignored). */
  sendFiles(device_id: string, paths: string[]): unknown;
}

/**
 * Applies a drag event: highlights the eligible target on hover, sends on drop,
 * clears on leave. Pure — `resolve` does the hit-test.
 */
export function handleFileDrag(
  event: FileDrag,
  resolve: (x: number, y: number) => string | null,
  sink: DragSink,
): void {
  if (event.type === "leave") {
    sink.dropTarget = null;
    return;
  }

  const under = event.position
    ? resolve(event.position.x, event.position.y)
    : null;
  const target = sink.targetFor(under);

  if (event.type === "drop") {
    sink.dropTarget = null;
    // Drop outside an eligible card, or a drag with no file: nothing.
    if (target && event.paths && event.paths.length > 0) {
      sink.sendFiles(target, event.paths);
    }
    return;
  }

  // enter / over: only highlight what can receive.
  sink.dropTarget = target;
}

/**
 * Resolves the device card under a point in **CSS pixels relative to the
 * viewport** — that's what `document.elementFromPoint` expects. The origin of
 * Tauri's raw position is corrected upstream (see `dropOriginOffset`), and we
 * do NOT divide by `devicePixelRatio`: the position is already logical (despite
 * the Rust type's `PhysicalPosition` name). Returns the `device_id` of the card
 * (`[data-device-id]`) that was hit, or `null`.
 */
export function deviceAtPoint(x: number, y: number): string | null {
  const element = document.elementFromPoint(x, y);
  const card = element?.closest?.("[data-device-id]") ?? null;
  return card?.getAttribute("data-device-id") ?? null;
}

/** The payload of Tauri's `onDragDropEvent` (position in CSS pixels). */
type TauriDragPayload =
  | { type: "enter"; paths: string[]; position: { x: number; y: number } }
  | { type: "over"; position: { x: number; y: number } }
  | { type: "drop"; paths: string[]; position: { x: number; y: number } }
  | { type: "leave" };

/** Normalizes the Tauri payload into a {@link FileDrag} — pure, testable without Tauri. */
export function toFileDrag(payload: TauriDragPayload): FileDrag {
  const position =
    "position" in payload
      ? { x: payload.position.x, y: payload.position.y }
      : undefined;
  const paths = "paths" in payload ? payload.paths : undefined;
  return { type: payload.type, paths, position };
}

/**
 * Offset (CSS px) between the origin of Tauri's drop position and the origin of
 * the viewport that `elementFromPoint` expects.
 *
 * Tauri v2 inconsistency: on **macOS**, the position is relative to the window
 * FRAME (title bar included); on Windows/Linux, to the CLIENT area (viewport,
 * zero offset). Hence a vertical offset = the title-bar height on macOS only
 * (X stays correct: no left border).
 *
 * We MEASURE it rather than hard-coding a fragile constant. The trap: all of
 * Tauri's *window* APIs (inner/outer Position/Size) report the same geometry
 * here — that of the FRAME — so their differences are 0. The only witness to
 * the real viewport is on the WebKit side: `window.innerHeight` (608) is
 * shorter than Tauri's `innerSize` (640, the whole frame). Their gap IS the
 * title bar: `innerSize/scale − window.inner*`. Zero off macOS (Tauri reports
 * the client area there). Degrades to `{0,0}` if the API fails — never a broken
 * drop.
 */
async function dropOriginOffset(): Promise<{ x: number; y: number }> {
  const none = { x: 0, y: 0 };
  if (!/Mac/i.test(navigator.userAgent)) return none;
  try {
    const { getCurrentWindow } = await import("@tauri-apps/api/window");
    const win = getCurrentWindow();
    const [size, scale] = await Promise.all([win.innerSize(), win.scaleFactor()]);
    return {
      x: size.width / scale - window.innerWidth,
      y: size.height / scale - window.innerHeight,
    };
  } catch {
    return none;
  }
}

/**
 * Wires the webview's native drag-and-drop to `sink`. Returns an unsubscribe
 * thunk, or `undefined` outside a Tauri webview (tests, dev browser): there,
 * there is no native drop — it's an absence, not a failure.
 */
export async function installFileDrop(
  sink: DragSink,
): Promise<(() => void) | undefined> {
  try {
    const { getCurrentWebview } = await import("@tauri-apps/api/webview");
    let offset = await dropOriginOffset();

    return await getCurrentWebview().onDragDropEvent(({ payload }) => {
      // The geometry can change between two drags (screen change, DPI change,
      // resize): we refresh the offset on each enter.
      if (payload.type === "enter") void dropOriginOffset().then((o) => (offset = o));
      const resolve = (x: number, y: number) => deviceAtPoint(x - offset.x, y - offset.y);
      handleFileDrag(toFileDrag(payload), resolve, sink);
    });
  } catch {
    return undefined;
  }
}
