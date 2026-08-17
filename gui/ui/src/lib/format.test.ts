// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

import { expect, test } from "vitest";

import type { Device } from "./api";
import {
  formatSize,
  platformLabel,
  relativeTime,
  roleLabel,
  scopeLabel,
  selfLabel,
  sortDevices,
  validFor,
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
  expect(platformLabel("android")).toBe("Android");
  expect(platformLabel("haiku")).toBe("haiku");
  expect(roleLabel("clipboard-backend")).toBe("clipboard");
  expect(roleLabel("future-role")).toBe("future-role");
  expect(scopeLabel("files.send")).toBe("Send files");
  expect(scopeLabel("future.scope")).toBe("future.scope");
});

// The approval prompt is the only place a user is told what a component may do,
// so a scope that grew must not keep its old label. `session.manage` now carries
// pairing, which hands the account key to another device.
test("the scope that can give the account away says so", () => {
  expect(scopeLabel("session.manage")).toMatch(/link new devices/);
});

// Every scope the Core can grant needs a sentence here, or the prompt shows the
// bare identifier and the user is told nothing. The two cross-device primitives
// are the ones that carry data off this machine.
test("the cross-device primitives are spelled out, one message and one flow", () => {
  expect(scopeLabel("peers.message")).toMatch(/messages/);
  expect(scopeLabel("peers.channel")).toMatch(/live channel/);
});

// Reading every keystroke on this machine is the strongest right in the whole
// list, and letting another computer type here is the second: the prompt must say
// both in words, not name the identifier. "Act as the keyboard and mouse engine"
// would be a keylogger worded as a job title, so what is asserted here is the
// verb, not the label.
test("the input scopes say what they really allow", () => {
  expect(roleLabel("input-backend")).toBe("keyboard and mouse sharing");
  expect(scopeLabel("input.serve")).toMatch(/Read this computer's keyboard/);
  expect(scopeLabel("input.serve")).toMatch(/type on it/);
  expect(scopeLabel("input.read")).toMatch(/who may drive this computer/);
  expect(scopeLabel("input.manage")).toMatch(/drive this one/);
  // The other half of `input.manage`, which the first wording left out.
  expect(scopeLabel("input.manage")).toMatch(/take control of another/);
});

// The phone runs the SAME view as the desktop, so the self label has to follow
// the platform rather than the build it is displayed on.
test("a device names itself after what it is", () => {
  expect(selfLabel("android")).toBe("this phone");
  expect(selfLabel("windows")).toBe("this PC");
  expect(selfLabel("haiku")).toBe("this PC");
});

test("sortDevices: this PC, then the reachable ones, then by name", () => {
  const device = (over: Partial<Device>): Device => ({
    device_id: over.name ?? "d",
    name: "x",
    platform: "linux",
    online: false,
    lan: false,
    reachable: false,
    is_self: false,
    ...over,
  });
  const devices = [
    device({ name: "Zephyr", online: true, reachable: true }),
    device({ name: "Alpha" }),
    device({ name: "Me", is_self: true }),
    // Reachable through the LAN alone (no server presence): rises just the
    // same — the Core's verdict is the sort key, not the server's flag.
    device({ name: "Beta", lan: true, reachable: true }),
  ];

  expect(sortDevices(devices).map((d) => d.name)).toEqual([
    "Me",
    "Beta",
    "Zephyr",
    "Alpha",
  ]);
});

// Binary units, like the limits the Core states (doc/core-api.md). A size is
// shown next to a file the user is about to send: precision past the tenth is
// noise, and "512.0 B" reads like a machine.
test.each([
  [0, "0 B"],
  [512, "512 B"],
  [1024, "1.0 KiB"],
  [1536, "1.5 KiB"],
  [2516582, "2.4 MiB"],
  [104857600, "100 MiB"],
  [3 * 1024 ** 3, "3.0 GiB"],
  [-1, ""],
  [Number.NaN, ""],
])("formatSize(%i) === %s", (bytes, text) => {
  expect(formatSize(bytes)).toBe(text);
});

// How long a pairing code is good for: a hint, deliberately rounded — the Core
// is what counts the deadline, and it says when the code has expired.
test.each([
  [120, "2 minutes"],
  [119, "2 minutes"],
  [90, "2 minutes"],
  [89, "89 seconds"],
  [30, "30 seconds"],
  [0, ""],
  [-1, ""],
  [Number.NaN, ""],
])("validFor(%s) is %s", (seconds, expected) => {
  expect(validFor(seconds)).toBe(expected);
});
