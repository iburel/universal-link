// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

import { expect, test } from "vitest";

import type { Device } from "./api";
import {
  platformLabel,
  relativeTime,
  roleLabel,
  scopeLabel,
  sortDevices,
} from "./format";

const NOW = new Date("2026-07-10T12:00:00Z");

test("relativeTime holds the scale of durations", () => {
  expect(relativeTime("2026-07-10T11:59:30Z", NOW)).toBe("just now");
  expect(relativeTime("2026-07-10T11:45:00Z", NOW)).toBe("15 min ago");
  expect(relativeTime("2026-07-10T09:00:00Z", NOW)).toBe("3 h ago");
  expect(relativeTime("2026-07-08T12:00:00Z", NOW)).toBe("2 d ago");
  expect(relativeTime("2026-01-02T12:00:00Z", NOW)).toBe("Jan 2, 2026");
});

// The clocks of two PCs are not synchronized: a future date is normal, not an
// anomaly to display.
test("relativeTime absorbs the future, the missing, and the unreadable", () => {
  expect(relativeTime("2026-07-10T12:05:00Z", NOW)).toBe("just now");
  expect(relativeTime(null, NOW)).toBeNull();
  expect(relativeTime(undefined, NOW)).toBeNull();
  expect(relativeTime("yesterday", NOW)).toBeNull();
});

test("labels fall back to the raw value when it is unknown", () => {
  expect(platformLabel("macos")).toBe("macOS");
  expect(platformLabel("haiku")).toBe("haiku");
  expect(roleLabel("clipboard-backend")).toBe("clipboard");
  expect(roleLabel("future-role")).toBe("future-role");
  expect(scopeLabel("files.send")).toBe("Send files");
  expect(scopeLabel("future.scope")).toBe("future.scope");
});

test("sortDevices: this PC, then the connected ones, then by name", () => {
  const device = (over: Partial<Device>): Device => ({
    device_id: over.name ?? "d",
    name: "x",
    platform: "linux",
    online: false,
    is_self: false,
    ...over,
  });
  const devices = [
    device({ name: "Zephyr", online: true }),
    device({ name: "Alpha" }),
    device({ name: "Me", is_self: true }),
    device({ name: "Beta", online: true }),
  ];

  expect(sortDevices(devices).map((d) => d.name)).toEqual([
    "Me",
    "Beta",
    "Zephyr",
    "Alpha",
  ]);
});
