// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

import { afterEach, beforeEach, expect, test, vi } from "vitest";

import type { Component, PendingRequest } from "../lib/api";
import type { ConnectionStatus } from "../lib/core";
import { byLabel, byText, cleanup, click, render, textOf } from "../lib/harness";
import { CoreStore } from "../lib/store.svelte";
import Approvals from "./Approvals.svelte";

const CONNECTED: ConnectionStatus = {
  status: "connected",
  granted_scopes: [],
  api_version: 1,
};

const REQUEST: PendingRequest = {
  request_id: "r_1",
  name: "clipnet",
  role: "clipboard-backend",
  // A request may ask for `components.approve`; the Core never grants it through
  // this path.
  scopes: ["clipboard.read", "clipboard.write", "components.approve"],
  peer_info: { pid: 42, exe: "/usr/bin/clipnet" },
};

const GUI: Component = {
  component_id: "c_gui",
  name: "universallink-gui",
  role: "gui",
  scopes: ["components.approve"],
  connected: true,
  enrolled: false,
};
const CLIPNET: Component = {
  component_id: "c_clip",
  name: "clipnet",
  role: "clipboard-backend",
  scopes: ["clipboard.read"],
  connected: false,
  enrolled: true,
};

let store: CoreStore;

beforeEach(() => {
  store = new CoreStore();
  store.connection = CONNECTED;
  store.primed = true;
  store.pending = [REQUEST];
  store.components = [GUI, CLIPNET];
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

test("a request shows its origin and its scopes", () => {
  const view = render(Approvals, { store });

  expect(textOf(view)).toContain("clipnet");
  expect(textOf(view)).toContain("clipboard");
  expect(textOf(view)).toContain("/usr/bin/clipnet (pid 42)");
  expect(textOf(view)).toContain("Read the shared clipboard");
});

// The Core responds -32602 if we ask it to grant this scope: the checkbox must
// be inert, and the approval must never send it.
test("components.approve is shown struck through, never granted", () => {
  const approve = vi.spyOn(store, "approve").mockResolvedValue();

  const view = render(Approvals, { store });
  const refused = view.querySelector("li.refused input") as HTMLInputElement;
  expect(refused.disabled).toBe(true);
  expect(refused.checked).toBe(false);

  click(byLabel(view, "Approve clipnet"));
  expect(approve).toHaveBeenCalledWith("r_1", [
    "clipboard.read",
    "clipboard.write",
  ]);
});

test("unchecking a scope removes it from the approval", () => {
  const approve = vi.spyOn(store, "approve").mockResolvedValue();

  const view = render(Approvals, { store });
  const scope = byText(view, ".scopes li", "Write to the shared clipboard");
  click(scope.querySelector("input"));

  click(byLabel(view, "Approve clipnet"));
  expect(approve).toHaveBeenCalledWith("r_1", ["clipboard.read"]);
});

test("denying a request passes it to the Core", () => {
  const deny = vi.spyOn(store, "deny").mockResolvedValue();

  const view = render(Approvals, { store });
  click(byLabel(view, "Deny clipnet"));

  expect(deny).toHaveBeenCalledWith("r_1");
});

test("no request: the screen says so", () => {
  store.pending = [];

  const view = render(Approvals, { store });

  expect(textOf(view)).toContain("No pending requests.");
});

// A bootstrap connection (file token, spawn token) has no persistent token:
// `components.revoke` would only close its connection. The role does not let us
// recognize it — an approved third party can hold any role.
test("only an enrolled component is revocable", () => {
  const revoke = vi.spyOn(store, "revokeComponent").mockResolvedValue();

  const view = render(Approvals, { store });
  expect(() => byLabel(view, "Revoke universallink-gui")).toThrow();
  expect(textOf(view)).toContain("local connection");

  click(byLabel(view, "Revoke clipnet"));
  expect(revoke).toHaveBeenCalledWith("c_clip");
});

// A Core older than the `enrolled` field: we fail closed rather than offer a
// revocation whose meaning we don't know.
test("without the enrolled field, no revocation is offered", () => {
  store.components = [{ ...CLIPNET, enrolled: undefined }];

  const view = render(Approvals, { store });

  expect(() => byLabel(view, "Revoke clipnet")).toThrow();
});

// The connected state used to be carried only by the color of an aria-hidden dot.
test("a component's state is written, not just colored", () => {
  const view = render(Approvals, { store });

  const rows = [...view.querySelectorAll(".components > li")];
  expect(rows[0].textContent).toContain("connected");
  expect(rows[1].textContent).toContain("disconnected");
});

test("origin() covers partial peer_info", () => {
  store.pending = [
    { ...REQUEST, request_id: "r_exe", peer_info: { exe: "/bin/a" } },
    { ...REQUEST, request_id: "r_pid", peer_info: { pid: 7 } },
    // macOS v1: no peer information.
    { ...REQUEST, request_id: "r_empty", peer_info: {} },
  ];

  const view = render(Approvals, { store });

  const text = textOf(view);
  expect(text).toContain("/bin/a");
  expect(text).toContain("pid 7");
  expect(text).toContain("unknown origin");
});

test("refresh resnapshots", () => {
  const resync = vi.spyOn(store, "resync").mockResolvedValue();

  const view = render(Approvals, { store });
  click(byText(view, "button", "Refresh"));

  expect(resync).toHaveBeenCalledOnce();
});

test("without the Core, no decision is possible", () => {
  store.connection = { status: "connecting" };

  const view = render(Approvals, { store });

  expect(byLabel(view, "Approve clipnet")).toHaveProperty("disabled", true);
  expect(byLabel(view, "Deny clipnet")).toHaveProperty("disabled", true);
  expect(byLabel(view, "Revoke clipnet")).toHaveProperty("disabled", true);
});
