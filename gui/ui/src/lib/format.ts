// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

/** Pure formatting: no dependency on the Core, testable without a clock. */

import type { Device, Platform } from "./api";

const PLATFORMS: Record<Platform, string> = {
  windows: "Windows",
  macos: "macOS",
  linux: "Linux",
};

export function platformLabel(platform: string): string {
  return PLATFORMS[platform as Platform] ?? platform;
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
  "session.manage": "Open and close the session",
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

/** This PC first, then online devices, then by name. */
export function sortDevices(devices: readonly Device[]): Device[] {
  return [...devices].sort((a, b) => {
    if (a.is_self !== b.is_self) return a.is_self ? -1 : 1;
    if (a.online !== b.online) return a.online ? -1 : 1;
    return a.name.localeCompare(b.name, "en");
  });
}
