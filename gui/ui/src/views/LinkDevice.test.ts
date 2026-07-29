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
  typeInto(byLabel(view, "Pairing code"), "UL1:a:b:p_1");
  click(byText(view, "button", "Link"));
  expect(enter).toHaveBeenCalledWith("UL1:a:b:p_1");

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
// pairs OVER it, so a lost server is a dead end...
test("a signed-in device with no server says why it cannot", () => {
  store.session = { logged_in: true, server_connected: false };

  const view = render(LinkDevice, { store, mode: "sponsor" });

  expect(byText(view, "button", "Show a code")).toHaveProperty("disabled", true);
  expect(textOf(view)).toContain("Waiting for the server connection");
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
  expect(textOf(view)).not.toContain("Waiting for the server");
});

test("the words change with the side, the gestures do not", () => {
  const view = render(LinkDevice, { store, mode: "sponsor" });
  expect(textOf(view)).toContain("Add a device to your account");

  cleanup();
  const joining = render(LinkDevice, { store, mode: "join" });
  expect(textOf(joining)).toContain("Link this device from one already");
});
