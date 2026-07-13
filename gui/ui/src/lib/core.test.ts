// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

// The wrapper against the mocked IPC of @tauri-apps/api: the same shapes as the
// shell's contract (gui/tests/api/support.rs).

import { emit } from "@tauri-apps/api/event";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import { afterEach, expect, test } from "vitest";

import {
  connectionStatus,
  coreRequest,
  onConnectionChanged,
  onCoreNotification,
  type ConnectionStatus,
  type CoreError,
  type CoreNotification,
} from "./core";

afterEach(() => {
  clearMocks();
});

test("coreRequest proxies method and params to core_request", async () => {
  const calls: Array<[string, unknown]> = [];
  mockIPC((cmd, payload) => {
    calls.push([cmd, payload]);
    return [{ device_id: "d_1", is_self: true }];
  });

  const devices = await coreRequest("devices.list", {});
  expect(devices).toEqual([{ device_id: "d_1", is_self: true }]);
  expect(calls).toEqual([
    ["core_request", { method: "devices.list", params: {} }],
  ]);
});

test("params is optional", async () => {
  let seen: unknown;
  mockIPC((_cmd, payload) => {
    seen = payload;
    return {};
  });

  await coreRequest("session.status");
  expect(seen).toEqual({ method: "session.status", params: undefined });
});

test("a structured error from the shell rejects as-is", async () => {
  mockIPC(() => {
    throw {
      kind: "not_connected",
      message: "no connection to the Core",
    } satisfies CoreError;
  });

  await expect(coreRequest("session.status")).rejects.toMatchObject({
    kind: "not_connected",
  });
});

test("connectionStatus reads the snapshot", async () => {
  mockIPC((cmd) => {
    expect(cmd).toBe("connection_status");
    return { status: "connected", granted_scopes: ["session.read"], api_version: 1 };
  });

  const status = await connectionStatus();
  expect(status.status).toBe("connected");
});

// The event names are the contract with the shell: a typo here would be caught
// by no Rust test (which only sees the emitter side).
test("onConnectionChanged listens to core:connection", async () => {
  mockIPC(() => null, { shouldMockEvents: true });
  const seen: ConnectionStatus[] = [];
  await onConnectionChanged((s) => seen.push(s));

  await emit("core:connection", { status: "connecting" });
  await emit("core:other", { status: "connected" });
  expect(seen).toEqual([{ status: "connecting" }]);
});

test("onCoreNotification listens to core:notification", async () => {
  mockIPC(() => null, { shouldMockEvents: true });
  const seen: CoreNotification[] = [];
  await onCoreNotification((n) => seen.push(n));

  await emit("core:notification", {
    method: "session.changed",
    params: { logged_in: true },
  });
  expect(seen).toEqual([
    { method: "session.changed", params: { logged_in: true } },
  ]);
});
