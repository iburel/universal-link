// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

/** Pure formatting: no dependency on the Core, testable without a clock. */

import type { Device, Platform } from "./api";

const PLATFORMS: Record<Platform, string> = {
  windows: "Windows",
  macos: "macOS",
  linux: "Linux",
  android: "Android",
};

export function platformLabel(platform: string): string {
  return PLATFORMS[platform as Platform] ?? platform;
}

/**
 * How a device names ITSELF in the list ("… · this PC"). A phone is not a PC,
 * and the phrase is also what the revocation warning is built on — a sentence
 * the user is asked to act on, so it has to name the thing they are holding.
 */
export function selfLabel(platform: string): string {
  return platform === "android" ? "this phone" : "this PC";
}

const ROLES: Record<string, string> = {
  gui: "interface",
  tray: "notification area",
  "clipboard-backend": "clipboard",
  "menu-backend": "context menu",
  custom: "third-party component",
};

export function roleLabel(role: string): string {
  return ROLES[role] ?? role;
}

const SCOPES: Record<string, string> = {
  "session.read": "Read the session state",
  // Pairing rides this scope (`pairing.*` and the `pairing` topic), and pairing
  // hands the account key to another device. A label that stopped at "open and
  // close the session" would understate what the user is granting — the prompt
  // is the only place they are ever told.
  "session.manage": "Open and close the session, and link new devices to the account",
  "devices.read": "Read the device list",
  "devices.manage": "Rename and revoke devices",
  "files.send": "Send files",
  "transfers.read": "Track transfers",
  "clipboard.read": "Read the shared clipboard",
  "clipboard.write": "Write to the shared clipboard",
  "components.approve": "Approve other components",
};

export function scopeLabel(scope: string): string {
  return SCOPES[scope] ?? scope;
}

/**
 * "3 h ago". `now` is injected: time is a parameter, not a side effect. Returns
 * `null` if the date is missing or unreadable — the caller then shows nothing,
 * rather than an "Invalid Date".
 */
export function relativeTime(
  iso: string | null | undefined,
  now: Date,
): string | null {
  if (!iso) return null;
  const then = Date.parse(iso);
  if (Number.isNaN(then)) return null;

  // A future date = clocks out of sync, not an error to display.
  const seconds = Math.round((now.getTime() - then) / 1000);
  if (seconds < 60) return "just now";
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes} min ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours} h ago`;
  const days = Math.floor(hours / 24);
  if (days < 30) return `${days} d ago`;
  return new Intl.DateTimeFormat("en", { dateStyle: "medium" }).format(
    new Date(then),
  );
}

const UNITS = ["B", "KiB", "MiB", "GiB", "TiB"];

/**
 * "2.4 MiB". Binary units, to match the limits the Core states in the same terms
 * (doc/core-api.md). Returns "" for anything that is not a size, so a caller can
 * print it without guarding.
 */
export function formatSize(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return "";
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < UNITS.length - 1) {
    value /= 1024;
    unit++;
  }
  // A decimal on raw bytes reads oddly ("512.0 B"), and past a hundred of any
  // unit the tenth is noise.
  const digits = unit === 0 || value >= 100 ? 0 : 1;
  return `${value.toFixed(digits)} ${UNITS[unit]}`;
}

/**
 * "2 minutes" / "45 seconds" — how long a pairing code is good for, as the tail
 * of a sentence. Rounded on purpose: this is a hint, not a countdown. The Core is
 * the one that counts, and it says when the code has expired.
 */
export function validFor(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds <= 0) return "";
  if (seconds < 90) return `${Math.round(seconds)} seconds`;
  return `${Math.round(seconds / 60)} minutes`;
}

/** This PC first, then online devices, then by name. */
export function sortDevices(devices: readonly Device[]): Device[] {
  return [...devices].sort((a, b) => {
    if (a.is_self !== b.is_self) return a.is_self ? -1 : 1;
    if (a.online !== b.online) return a.online ? -1 : 1;
    return a.name.localeCompare(b.name, "en");
  });
}
