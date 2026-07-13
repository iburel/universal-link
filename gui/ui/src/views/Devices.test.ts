// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

import { afterEach, beforeEach, expect, test, vi } from "vitest";

import type { Device } from "../lib/api";
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
import { CoreStore, type Transfer } from "../lib/store.svelte";
import Devices from "./Devices.svelte";

const NOW = new Date("2026-07-10T12:00:00Z");
const CONNECTED: ConnectionStatus = {
  status: "connected",
  granted_scopes: [],
  api_version: 1,
};

const SELF: Device = {
  device_id: "d_self",
  name: "Office PC",
  platform: "linux",
  online: true,
  last_seen: null,
  is_self: true,
};
const MAC: Device = {
  device_id: "d_mac",
  name: "MacBook",
  platform: "macos",
  online: false,
  last_seen: "2026-07-10T09:00:00Z",
  is_self: false,
};
const WIN: Device = {
  device_id: "d_win",
  name: "Living Room PC",
  platform: "windows",
  online: true,
  last_seen: null,
  is_self: false,
};

function transferTo(device_id: string, over: Partial<Transfer> = {}): Transfer {
  return {
    transfer_id: "t_1",
    device_id,
    files: [{ name: "a.pdf", size: 100 }],
    total: 100,
    done: 40,
    status: "active",
    ...over,
  };
}

let store: CoreStore;

beforeEach(() => {
  store = new CoreStore();
  store.connection = CONNECTED;
  store.primed = true;
  store.session = { logged_in: true, server_connected: true };
  store.devices = [MAC, SELF];
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

test("when logged out, the directory is not shown", () => {
  store.session = { logged_in: false, server_connected: false };

  const view = render(Devices, { store, now: NOW });

  expect(textOf(view)).toContain("Sign in");
  expect(view.querySelectorAll("li")).toHaveLength(0);
});

test("a directory refused by the Core is explained", () => {
  store.devices = [];
  store.devicesError = "Server unreachable.";

  const view = render(Devices, { store, now: NOW });

  expect(textOf(view)).toContain("Directory unavailable: Server unreachable.");
});

test("this PC comes first, inactivity is dated", () => {
  const view = render(Devices, { store, now: NOW });

  const rows = [...view.querySelectorAll("li")];
  expect(rows[0].textContent).toContain("Office PC");
  expect(rows[0].textContent).toContain("this PC");
  expect(rows[0].textContent).toContain("online");
  expect(rows[1].textContent).toContain("MacBook");
  expect(rows[1].textContent).toContain("last seen 3 h ago");
});

test("renaming sends the cleaned name to the Core", async () => {
  const rename = vi.spyOn(store, "renameDevice").mockResolvedValue();

  const view = render(Devices, { store, now: NOW });
  click(byLabel(view, "Rename MacBook"));
  typeInto(byLabel(view, "New name for MacBook"), "  Living Room Mac  ");
  click(byText(view, "button", "Save"));
  await Promise.resolve();

  expect(rename).toHaveBeenCalledWith("d_mac", "Living Room Mac");
});

test("an unchanged or empty name does not bother the Core", async () => {
  const rename = vi.spyOn(store, "renameDevice").mockResolvedValue();

  const view = render(Devices, { store, now: NOW });
  click(byLabel(view, "Rename MacBook"));
  click(byText(view, "button", "Save")); // unchanged name
  await Promise.resolve();

  click(byLabel(view, "Rename MacBook"));
  typeInto(byLabel(view, "New name for MacBook"), "   ");
  click(byText(view, "button", "Save"));
  await Promise.resolve();

  expect(rename).not.toHaveBeenCalled();
});

test("Enter commits the rename, Escape cancels it", async () => {
  const rename = vi.spyOn(store, "renameDevice").mockResolvedValue();

  const view = render(Devices, { store, now: NOW });
  click(byLabel(view, "Rename MacBook"));
  typeInto(byLabel(view, "New name for MacBook"), "Living Room Mac");
  press(byLabel(view, "New name for MacBook"), "Enter");
  await Promise.resolve();
  expect(rename).toHaveBeenCalledWith("d_mac", "Living Room Mac");

  click(byLabel(view, "Rename MacBook"));
  typeInto(byLabel(view, "New name for MacBook"), "Another name");
  press(byLabel(view, "New name for MacBook"), "Escape");
  await Promise.resolve();
  expect(rename).toHaveBeenCalledOnce();
  expect(textOf(view)).toContain("MacBook");
});

test("a revocation asks for confirmation", () => {
  const revoke = vi.spyOn(store, "revokeDevice").mockResolvedValue();

  const view = render(Devices, { store, now: NOW });
  click(byLabel(view, "Revoke MacBook"));
  expect(textOf(view)).toContain("Revoke MacBook?");
  expect(revoke).not.toHaveBeenCalled();

  click(byText(view, "button", "Confirm"));
  expect(revoke).toHaveBeenCalledWith("d_mac");
});

// Revoking one's own device disconnects this PC from the account: say so beforehand.
test("revoking this PC is announced as such", () => {
  const view = render(Devices, { store, now: NOW });

  click(byLabel(view, "Revoke Office PC"));

  expect(textOf(view)).toContain("Revoking this PC will disconnect");
});

test("cancelling a revocation revokes nothing", () => {
  const revoke = vi.spyOn(store, "revokeDevice").mockResolvedValue();

  const view = render(Devices, { store, now: NOW });
  click(byLabel(view, "Revoke MacBook"));
  click(byText(view, "button", "Cancel"));

  expect(revoke).not.toHaveBeenCalled();
  expect(textOf(view)).not.toContain("Confirm");
});

test("without the Core, the actions are disarmed", () => {
  store.connection = { status: "connecting" };

  const view = render(Devices, { store, now: NOW });

  expect(byLabel(view, "Rename MacBook")).toHaveProperty("disabled", true);
  expect(byLabel(view, "Revoke MacBook")).toHaveProperty("disabled", true);
});

// -- Drag-and-drop and transfers --------------------------------------------

// Each card carries its device_id: it's the anchor of the drop hit-test
// (lib/dragdrop.ts), the only way to find the target from a position.
test("each card exposes its device_id for the hit-test", () => {
  const view = render(Devices, { store, now: NOW });

  const ids = [...view.querySelectorAll("li")].map((li) =>
    li.getAttribute("data-device-id"),
  );
  expect(ids).toEqual(["d_self", "d_mac"]); // this PC first (sort)
});

test("the target of an in-progress drag is highlighted", () => {
  store.devices = [WIN, SELF];
  store.dropTarget = "d_win";

  const view = render(Devices, { store, now: NOW });

  const win = view.querySelector('[data-device-id="d_win"]');
  const self = view.querySelector('[data-device-id="d_self"]');
  expect(win?.classList.contains("drop-target")).toBe(true);
  expect(self?.classList.contains("drop-target")).toBe(false);
});

test("a send in progress shows its progress and a cancel button", () => {
  const cancel = vi.spyOn(store, "cancelTransfer").mockResolvedValue();
  store.devices = [WIN, SELF];
  store.transfers = [transferTo("d_win", { done: 40, total: 100 })];

  const view = render(Devices, { store, now: NOW });
  const card = view.querySelector('[data-device-id="d_win"]')!;
  expect(card.textContent).toContain("Sending… 40%");
  expect(card.querySelector("progress")).toHaveProperty("value", 40);

  click(byLabel(view, "Cancel send to Living Room PC"));
  expect(cancel).toHaveBeenCalledWith("t_1");
});

test("a completed send is confirmed and can be dismissed", () => {
  const dismiss = vi.spyOn(store, "dismissTransfer");
  store.devices = [WIN, SELF];
  store.transfers = [
    transferTo("d_win", {
      status: "finished",
      done: 100,
      files: [{ name: "a.pdf", size: 60 }, { name: "b.png", size: 40 }],
    }),
  ];

  const view = render(Devices, { store, now: NOW });
  const card = view.querySelector('[data-device-id="d_win"]')!;
  expect(card.textContent).toContain("Sent · 2 files");

  click(byLabel(view, "Dismiss the transfer to Living Room PC"));
  expect(dismiss).toHaveBeenCalledWith("t_1");
});

// Two sends to the same device, one active, the other terminal and more recent:
// it's the active one that is summarized (its progress and its cancellation
// stay accessible), not the terminal one.
test("an active send takes priority over a more recent finished send to the same device", () => {
  store.devices = [WIN, SELF];
  store.transfers = [
    {
      transfer_id: "t_active",
      device_id: "d_win",
      files: [{ name: "big", size: 1000 }],
      total: 1000,
      done: 200,
      status: "active",
    },
    {
      transfer_id: "t_done",
      device_id: "d_win",
      files: [{ name: "small", size: 10 }],
      total: 10,
      done: 10,
      status: "finished",
    },
  ];

  const view = render(Devices, { store, now: NOW });
  const card = view.querySelector('[data-device-id="d_win"]')!;

  expect(card.textContent).toContain("Sending… 20%");
  expect(card.textContent).not.toContain("Sent");
  expect(byLabel(view, "Cancel send to Living Room PC")).toBeTruthy();
});

test("a failed or cancelled send says so on the card", () => {
  store.devices = [WIN, SELF];

  store.transfers = [transferTo("d_win", { status: "failed", error: "disk full" })];
  let view = render(Devices, { store, now: NOW });
  expect(view.querySelector('[data-device-id="d_win"]')?.textContent).toContain(
    "Send failed: disk full",
  );
  cleanup();

  store.transfers = [transferTo("d_win", { status: "failed", error: "cancelled" })];
  view = render(Devices, { store, now: NOW });
  expect(view.querySelector('[data-device-id="d_win"]')?.textContent).toContain(
    "Send cancelled",
  );
});
