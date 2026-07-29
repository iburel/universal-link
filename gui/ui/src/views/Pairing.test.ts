// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

import { afterEach, beforeEach, expect, test, vi } from "vitest";

import type { ConnectionStatus } from "../lib/core";
import { byLabel, byText, cleanup, click, render, textOf } from "../lib/harness";
import { CoreStore, type PairingFlow } from "../lib/store.svelte";
import Pairing from "./Pairing.svelte";

const CONNECTED: ConnectionStatus = {
  status: "connected",
  granted_scopes: [],
  api_version: 1,
};

/** A code of the shape the Core really mints: version, secret, key, session. */
const CODE = `UL1:${"A".repeat(22)}:${"B".repeat(43)}:p_${"9".repeat(32)}`;

let store: CoreStore;

beforeEach(() => {
  store = new CoreStore();
  store.connection = CONNECTED;
  store.primed = true;
  store.session = { logged_in: true, server_connected: true };
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

function showing(over: Partial<PairingFlow> = {}) {
  store.pairing = {
    pairing_id: "p_1",
    role: "joiner",
    phase: "showing",
    code: CODE,
    expires_in: 120,
    ...over,
  };
}

test("the code is on screen twice: as a symbol, and as text to copy", () => {
  showing();

  const view = render(Pairing, { store });

  const svg = byLabel(view, "Pairing code as a QR code");
  // The quiet zone is part of the picture: four modules of margin on each side,
  // without which a camera cannot find the symbol's edges. A 105-character code
  // is a version-6 symbol, 41 modules across.
  expect(svg.getAttribute("viewBox")).toBe("0 0 49 49");
  expect(svg.querySelector("path")?.getAttribute("d")).toMatch(/^M\d+ \d+h1v1h-1z/);
  // Dark on light, whatever theme the app is in.
  expect(svg.querySelector("rect")?.getAttribute("fill")).toBe("#ffffff");
  expect(svg.querySelector("path")?.getAttribute("fill")).toBe("#000000");

  // And the same code as text, for a device with no camera.
  expect(textOf(view)).toContain(CODE);
  expect(textOf(view)).toContain("good for about 2 minutes");
});

test("who scans what depends on which end this device is on", () => {
  showing();
  expect(textOf(render(Pairing, { store }))).toContain(
    "Scan this on a device that is already on your account",
  );

  cleanup();
  showing({ role: "sponsor" });
  expect(textOf(render(Pairing, { store }))).toContain(
    "Scan this on the device you want to add",
  );
});

// The number is the check the whole screen exists for: the joining side must show
// it while it waits, so the human has something to compare on the other device.
test("while waiting, the number to compare is shown", () => {
  store.pairing = {
    pairing_id: "p_1",
    role: "joiner",
    phase: "waiting",
    verification: "428 913",
  };

  const view = render(Pairing, { store });

  expect(textOf(view)).toContain("428 913");
  expect(textOf(view)).toContain("Confirm on your other device");
});

test("the confirmation shows what is being confirmed, and how to check it", () => {
  store.pairing = {
    pairing_id: "p_1",
    role: "sponsor",
    phase: "confirm",
    verification: "428 913",
    device: { name: "New laptop", platform: "macos", node_id: "ab" },
  };
  const confirm = vi.spyOn(store, "confirmPairing").mockResolvedValue();

  const view = render(Pairing, { store });

  expect(textOf(view)).toContain("New laptop");
  expect(textOf(view)).toContain("macOS");
  expect(textOf(view)).toContain("428 913");
  expect(textOf(view)).toContain("must be showing the same number");

  click(byText(view, "button", "Add to my account"));
  expect(confirm).toHaveBeenCalledOnce();
});

test("declining cancels the pairing", () => {
  store.pairing = {
    pairing_id: "p_1",
    role: "sponsor",
    phase: "confirm",
    verification: "1",
    device: { name: "New laptop", platform: "linux" },
  };
  const cancel = vi.spyOn(store, "cancelPairing").mockResolvedValue();

  click(byText(render(Pairing, { store }), "button", "Decline"));

  expect(cancel).toHaveBeenCalledOnce();
});

test("a code on screen can be given up on", () => {
  showing();
  const cancel = vi.spyOn(store, "cancelPairing").mockResolvedValue();

  click(byText(render(Pairing, { store }), "button", "Cancel"));

  expect(cancel).toHaveBeenCalledOnce();
});

test("a confirmation gone through the browser says where to finish it", () => {
  store.pairing = { pairing_id: "p_1", role: "sponsor", phase: "confirming" };

  expect(textOf(render(Pairing, { store }))).toContain("in your browser");
});

// This screen covers the whole window, so it has to carry the banner: a refusal
// from the Core would otherwise happen in silence.
test("the store's message is shown here too, and closable", () => {
  showing();
  store.notice = { kind: "error", text: "Server unreachable." };

  const view = render(Pairing, { store });
  expect(textOf(view)).toContain("Server unreachable.");

  click(byLabel(view, "Close message"));
  expect(view.querySelector(".banner")).toBeNull();
});
