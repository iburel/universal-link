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
  lan: false,
  reachable: true,
  last_seen: null,
  is_self: true,
};
const MAC: Device = {
  device_id: "d_mac",
  name: "MacBook",
  platform: "macos",
  online: false,
  lan: false,
  reachable: false,
  last_seen: "2026-07-10T09:00:00Z",
  is_self: false,
};
const WIN: Device = {
  device_id: "d_win",
  name: "Living Room PC",
  platform: "windows",
  online: true,
  lan: false,
  reachable: true,
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
  // Separators included: the space before each one is what the markup used to
  // eat ("Linux· this PC").
  expect(rows[0].textContent).toContain("Linux · this PC · online");
  expect(rows[1].textContent).toContain("MacBook");
  expect(rows[1].textContent).toContain("macOS · last seen 3 h ago");
});

test("a machine heard on the local network says so, even without the server", () => {
  // The LAN case: the server never marked it online — mDNS is the presence.
  store.devices = [
    {
      device_id: "d_lan",
      name: "Next Door PC",
      platform: "linux",
      online: false,
      lan: true,
      reachable: true,
      last_seen: null,
      is_self: false,
    },
  ];

  const view = render(Devices, { store, now: NOW });
  const row = view.querySelector("li")!;
  expect(row.textContent).toContain("Linux · on this network");
  expect(row.textContent).not.toContain("last seen");
  // And the presence dot is lit: reachable is the verdict, not `online`.
  expect(row.querySelector(".dot.online")).not.toBeNull();
});

test("a machine dialable only through its signed hints says reachable, not online", () => {
  // Off the LAN, no server vouching: the record carries a route worth trying
  // (its signed addresses or relay), and that is all the phrase may claim.
  store.devices = [
    {
      device_id: "d_hinted",
      name: "Nomad Laptop",
      platform: "linux",
      online: false,
      lan: false,
      reachable: true,
      last_seen: null,
      is_self: false,
    },
  ];

  const view = render(Devices, { store, now: NOW });
  const row = view.querySelector("li")!;
  expect(row.textContent).toContain("Linux · reachable");
  expect(row.textContent).not.toContain("online");
  expect(row.querySelector(".dot.online")).not.toBeNull();
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

// Same view, run on the phone: it is the platform that decides what the device
// calls itself, in the list and in the warning.
test("on a phone, the self device is a phone", () => {
  store.devices = [{ ...SELF, name: "CPH2449", platform: "android" }];

  const view = render(Devices, { store, now: NOW });
  expect(textOf(view)).toContain("Android · this phone");

  click(byLabel(view, "Revoke CPH2449"));
  expect(textOf(view)).toContain("Revoking this phone will disconnect");
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

// -- Android share sheet: the destination picker ------------------------------
//
// A file shared into the app turns this list into a picker: one tap sends it.
// The management actions step aside so a tap cannot revoke a device by mistake
// when the user meant to send to it.

const PICK = {
  phase: "pick" as const,
  id: "s_1",
  files: [{ path: "/c/shares/s_1/holiday.jpg", name: "holiday.jpg", size: 2516582 }],
};

test("a pending share turns the list into a destination picker", () => {
  store.devices = [WIN, MAC, SELF];
  store.pendingShare = PICK;

  const view = render(Devices, { store, now: NOW });

  // What is about to be sent, so the user can tell one share from another.
  expect(textOf(view)).toContain("Send to…");
  expect(textOf(view)).toContain("holiday.jpg · 2.4 MiB");
  // Online device: offered. Offline: shown, refused (the Core would say
  // DEVICE_OFFLINE). This phone: not a destination for its own share.
  expect(byLabel(view, "Send to Living Room PC").hasAttribute("disabled")).toBe(false);
  expect(byLabel(view, "Send to MacBook").hasAttribute("disabled")).toBe(true);
  expect(view.querySelector('[aria-label="Send to Office PC"]')).toBeNull();
  // No renaming or revoking while the list is a picker.
  expect(view.querySelector('[aria-label="Revoke MacBook"]')).toBeNull();
});

test("tapping a device sends the pending share to it", () => {
  store.devices = [WIN, SELF];
  store.pendingShare = PICK;
  const send = vi.spyOn(store, "sendShare").mockResolvedValue();

  const view = render(Devices, { store, now: NOW });
  click(byLabel(view, "Send to Living Room PC"));

  expect(send).toHaveBeenCalledWith("d_win");
});

test("cancelling the share leaves the list alone", () => {
  store.devices = [WIN, SELF];
  store.pendingShare = PICK;
  const cancel = vi.spyOn(store, "cancelShare").mockImplementation(() => {});

  const view = render(Devices, { store, now: NOW });
  click(byLabel(view, "Cancel the share"));

  expect(cancel).toHaveBeenCalled();
});

// A disconnected Core cannot send: the picker must not pretend otherwise.
test("with the Core unreachable, no destination is offered", () => {
  store.devices = [WIN, SELF];
  store.pendingShare = PICK;
  store.connection = { status: "connecting" };

  const view = render(Devices, { store, now: NOW });

  expect(byLabel(view, "Send to Living Room PC").hasAttribute("disabled")).toBe(true);
});

// -- Adding a device --------------------------------------------------------
//
// Offered where the devices are, but only by a device that can actually vouch:
// it holds the account key AND is in the account.

test("a device that can vouch is offered to add another", () => {
  store.account = { attested: true, fingerprint: "AB12", holds_key: true };
  const show = vi.spyOn(store, "showPairingCode").mockResolvedValue();

  const view = render(Devices, { store, now: NOW });

  expect(textOf(view)).toContain("Add a device");
  click(byText(view, "button", "Show a code"));
  expect(show).toHaveBeenCalledOnce();
});

test.each([
  ["it does not hold the account key", { attested: true, fingerprint: "AB12", holds_key: false }],
  ["it holds a key but is not in the account", { attested: false, fingerprint: null, holds_key: true }],
  ["the Core does not say (an older one)", null],
])("no offer to add a device when %s", (_why, account) => {
  store.account = account;

  expect(textOf(render(Devices, { store, now: NOW }))).not.toContain(
    "Add a device",
  );
});

// The Core's definition of "can vouch" names no session: a serverless sponsor
// shows a `1D2` code. Gating on the session would keep the whole serverless
// account from ever growing past one device.
test("a serverless device that can vouch is offered to add another", () => {
  store.session = { logged_in: false, server_connected: false, configured: false };
  store.account = { attested: true, fingerprint: "AB12", holds_key: true };

  const view = render(Devices, { store, now: NOW });

  expect(textOf(view)).toContain("Add a device");
});

// -- What a session-less directory offers -----------------------------------
//
// No session means no server to carry a gesture: a sibling's name is its own
// signed word, and a device cannot strike itself from the account
// (CANNOT_REVOKE_SELF). The buttons left are the ones that can succeed.

test("with no session, a sibling can be revoked but not renamed", () => {
  store.session = { logged_in: false, server_connected: false, configured: false };
  store.account = { attested: true, fingerprint: "AB12", holds_key: true };

  const view = render(Devices, { store, now: NOW });

  expect(view.querySelector('[aria-label="Rename MacBook"]')).toBeNull();
  expect(byLabel(view, "Revoke MacBook")).toBeTruthy();
});

test("with no session, this PC can be renamed but not revoked", () => {
  store.session = { logged_in: false, server_connected: false, configured: false };
  store.account = { attested: true, fingerprint: "AB12", holds_key: true };

  const view = render(Devices, { store, now: NOW });

  expect(byLabel(view, "Rename Office PC")).toBeTruthy();
  expect(view.querySelector('[aria-label="Revoke Office PC"]')).toBeNull();
});

// The directory of a device IN the account does not need a session: the Core
// serves it — its own record at least, plus what the account taught it.
test("a signed-out device in the account still sees its directory", () => {
  store.session = { logged_in: false, server_connected: false, configured: false };
  store.account = { attested: true, fingerprint: "AB12", holds_key: true };

  const view = render(Devices, { store, now: NOW });

  expect(textOf(view)).not.toContain("Sign in to see the devices");
  expect(view.querySelectorAll("li").length).toBe(2);
});

// A share waiting for a destination owns the list until it is answered: the
// question is "where to?", and everything else steps aside for it (mobile).
test("a pending share hides the offer to add a device", () => {
  store.account = { attested: true, fingerprint: "AB12", holds_key: true };
  store.pendingShare = {
    phase: "pick",
    id: "s_1",
    files: [{ path: "/c/s_1/a.pdf", name: "a.pdf", size: 10 }],
  };

  expect(textOf(render(Devices, { store, now: NOW }))).not.toContain(
    "Add a device",
  );
});
