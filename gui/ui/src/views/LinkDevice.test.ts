// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

import { afterEach, beforeEach, expect, test, vi } from "vitest";

import type { ConnectionStatus } from "../lib/core";
import {
  byLabel,
  byText,
  cleanup,
  click,
  press,
  render,
  textOf,
  type as typeInto,
} from "../lib/harness";
import { CoreStore } from "../lib/store.svelte";
import LinkDevice from "./LinkDevice.svelte";

const CONNECTED: ConnectionStatus = {
  status: "connected",
  granted_scopes: [],
  api_version: 1,
};

let store: CoreStore;

beforeEach(() => {
  store = new CoreStore();
  store.connection = CONNECTED;
  store.primed = true;
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

test("both gestures are offered, and each calls the Core", () => {
  const show = vi.spyOn(store, "showPairingCode").mockResolvedValue();
  const view = render(LinkDevice, { store, mode: "join" });

  click(byText(view, "button", "Show a code"));
  expect(show).toHaveBeenCalledOnce();

  // The other way round: read the code the other device is showing.
  click(byText(view, "button", "Enter a code…"));
  expect(byLabel(view, "Pairing code")).toBeTruthy();
});

test("a pasted code is passed on, and Enter commits it", () => {
  const enter = vi.spyOn(store, "enterPairingCode").mockResolvedValue();
  const view = render(LinkDevice, { store, mode: "join" });

  click(byText(view, "button", "Enter a code…"));
  typeInto(byLabel(view, "Pairing code"), "1D1:a:b:p_1");
  click(byText(view, "button", "Link"));
  expect(enter).toHaveBeenCalledWith("1D1:a:b:p_1");

  press(byLabel(view, "Pairing code"), "Enter");
  expect(enter).toHaveBeenCalledTimes(2);
});

test("nothing typed, nothing sent", () => {
  const enter = vi.spyOn(store, "enterPairingCode").mockResolvedValue();
  const view = render(LinkDevice, { store, mode: "join" });

  click(byText(view, "button", "Enter a code…"));
  typeInto(byLabel(view, "Pairing code"), "   ");

  expect(byText(view, "button", "Link")).toHaveProperty("disabled", true);
  press(byLabel(view, "Pairing code"), "Enter");
  expect(enter).not.toHaveBeenCalled();
});

// A Core (or a server) older than pairing: the screens stop offering it, rather
// than offering a button that can only fail.
test("a Core that does not know pairing is not offered it", () => {
  store.pairingSupported = false;

  const view = render(LinkDevice, { store, mode: "join" });

  expect(textOf(view)).toBe("");
});

// The two sides read "is the server there?" differently. A device with a session
// SHOWS its code over that session, so a lost server disarms showing — and only
// showing: reading a `1D2` code dials the device that displays it on the local
// network, which is exactly the situation a lost server leaves the room in.
test("a signed-in device with no server can still read a code, not show one", () => {
  store.session = { logged_in: true, server_connected: false };
  const enter = vi.spyOn(store, "enterPairingCode").mockResolvedValue();

  const view = render(LinkDevice, { store, mode: "sponsor" });

  expect(byText(view, "button", "Show a code")).toHaveProperty("disabled", true);
  expect(textOf(view)).toContain("The server is unreachable");

  click(byText(view, "button", "Enter a code…"));
  typeInto(byLabel(view, "Pairing code"), "1D2:a:b:node_1");
  click(byText(view, "button", "Link"));
  expect(enter).toHaveBeenCalledWith("1D2:a:b:node_1");
});

// ...whereas a device with NO session opens a connection of its own on demand,
// and `server_connected` is false for it at all times. Gating on it would disarm
// the very screen a brand-new device needs.
test("a device with no session is not disarmed by a server it has never talked to", () => {
  store.session = { logged_in: false, server_connected: false };
  const show = vi.spyOn(store, "showPairingCode").mockResolvedValue();

  const view = render(LinkDevice, { store, mode: "join" });

  expect(byText(view, "button", "Show a code")).toHaveProperty("disabled", false);
  click(byText(view, "button", "Show a code"));
  expect(show).toHaveBeenCalledOnce();
  expect(textOf(view)).not.toContain("The server is unreachable");
});

// The camera gesture exists only where there is a camera, and the shell is what
// answers that (mobile). It is the PRIMARY one there: pointing a phone at a
// screen beats reading 105 characters out.
test("a device with a camera is offered it first", () => {
  store.canScan = true;
  const scan = vi.spyOn(store, "scanPairingCode").mockResolvedValue();

  const view = render(LinkDevice, { store, mode: "join" });

  const buttons = [...view.querySelectorAll("button")].map((b) => b.textContent?.trim());
  expect(buttons).toEqual(["Scan a code", "Show a code", "Enter a code…"]);
  click(byText(view, "button", "Scan a code"));
  expect(scan).toHaveBeenCalledOnce();
});

test("a device with no camera is not offered one", () => {
  const view = render(LinkDevice, { store, mode: "join" });

  expect(textOf(view)).not.toContain("Scan a code");
});

// While the camera is up the store holds `scanning`, not `busy` (a scan lasts as
// long as a human takes). The buttons still have to disarm — a second scanner on
// top of the first would be two windows fighting over one camera.
test("a scan under way disarms the gestures", () => {
  store.canScan = true;
  store.scanning = true;

  const view = render(LinkDevice, { store, mode: "join" });

  for (const label of ["Scan a code", "Show a code", "Enter a code…"]) {
    expect(byText(view, "button", label)).toHaveProperty("disabled", true);
  }
});

test("the words change with the side, the gestures do not", () => {
  const view = render(LinkDevice, { store, mode: "sponsor" });
  expect(textOf(view)).toContain("Add a device to your account");

  cleanup();
  const joining = render(LinkDevice, { store, mode: "join" });
  expect(textOf(joining)).toContain("Link this device from one already");
});
