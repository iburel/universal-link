// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

/**
 * TS mirror of the Core's surface (doc/core-api.md), on top of the shell's
 * `core_request` proxy. The Rust shell has only ONE command — the Core is the
 * sole authority; this file adds no logic, it just gives names and types to
 * the methods we call.
 *
 * Only the methods covered by the GUI's scopes (`GUI_SCOPES` in gui/src/lib.rs)
 * appear here: session, account, devices, files, components.
 */

import { coreRequest } from "./core";

export type Platform = "windows" | "macos" | "linux";

/** The device record from server-api.md, enriched with `is_self` by the Core. */
export interface Device {
  device_id: string;
  name: string;
  platform: Platform;
  node_id?: string;
  relay_url?: string | null;
  online: boolean;
  /** Free-form field reserved for extensibility; v1 defines no value for it. */
  status?: string | null;
  last_seen?: string | null;
  is_self: boolean;
}

/** `account` is opaque JSON replayed as-is by the Core; the email may be missing. */
export interface Account {
  email?: string;
}

export interface SessionState {
  logged_in: boolean;
  server_connected: boolean;
  account?: Account;
}

/**
 * The state of the account key (C7). `attested`: has this device joined the
 * account vault (root of trust installed) — without which `files.send` fails
 * closed, for lack of an attestation the peer can verify. `fingerprint` is the
 * safety number to compare across devices; `null` until attested.
 */
export interface AccountKey {
  attested: boolean;
  fingerprint: string | null;
}

/** Derived from the peer credentials; empty on macOS in v1. */
export interface PeerInfo {
  pid?: number;
  exe?: string;
}

export interface PendingRequest {
  request_id: string;
  name: string;
  role: string;
  scopes: string[];
  peer_info: PeerInfo;
}

export interface Component {
  component_id: string;
  name: string;
  role: string;
  scopes: string[];
  connected: boolean;
  /**
   * False for a bootstrap connection (the GUI's file token, an official
   * component's spawn token): there is no persistent token to revoke from it.
   * Absent if the Core predates this field — we fail closed.
   */
  enrolled?: boolean;
}

export type RevokeResult =
  | { status: "done" }
  | { status: "reauth_required"; auth_url: string };

export const api = {
  sessionStatus: () => coreRequest<SessionState>("session.status"),
  /** The caller opens `auth_url`; completion arrives via `session.changed`. */
  sessionLogin: () => coreRequest<{ auth_url: string }>("session.login"),
  sessionLogout: () => coreRequest<unknown>("session.logout"),

  /** Whether this device has joined the account vault, and under which fingerprint. */
  accountStatus: () => coreRequest<AccountKey>("account.status"),
  /**
   * Creates the vault (first device). Returns the `recovery_code` — the ONLY
   * copy of the private key, to be handed to the user, never replayed — and
   * the fingerprint. Requires the server to be reachable (`SERVER_UNREACHABLE`);
   * `ACCOUNT_KEY_SET` if a root already exists on this device.
   */
  accountSetup: () =>
    coreRequest<{ recovery_code: string; fingerprint: string | null }>(
      "account.setup",
    ),
  /**
   * Joins an existing vault with the code from another device. Returns the
   * fingerprint, to compare with the other devices'. `INVALID_CODE` if the code
   * is wrong (this device would then stay outside the account).
   */
  accountJoin: (recovery_code: string) =>
    coreRequest<{ fingerprint: string | null }>("account.join", {
      recovery_code,
    }),

  devicesList: () => coreRequest<Device[]>("devices.list"),
  /** The response is ignored: the state comes from the synthesized `device.updated`. */
  devicesRename: (device_id: string, name: string) =>
    coreRequest<unknown>("devices.rename", { device_id, name }),
  devicesRevoke: (device_id: string) =>
    coreRequest<RevokeResult>("devices.revoke", { device_id }),

  /**
   * Send flat files to a device (fire-and-forget). Returns a `transfer_id`, but
   * tracking goes through the `transfer.*` notifications — the caller need not
   * remember it. A folder or a missing path → `-32602`; a target outside the
   * directory → `DEVICE_UNKNOWN`; no relay → `DEVICE_OFFLINE`.
   */
  filesSend: (device_id: string, paths: string[]) =>
    coreRequest<{ transfer_id: string }>("files.send", { device_id, paths }),
  /** Cancels an outgoing transfer. `TRANSFER_UNKNOWN` if it has already finished. */
  filesCancel: (transfer_id: string) =>
    coreRequest<unknown>("files.cancel", { transfer_id }),

  componentsList: () => coreRequest<Component[]>("components.list"),
  componentsPending: () => coreRequest<PendingRequest[]>("components.pending"),
  componentsApprove: (request_id: string, scopes: string[]) =>
    coreRequest<unknown>("components.approve", { request_id, scopes }),
  componentsDeny: (request_id: string) =>
    coreRequest<unknown>("components.deny", { request_id }),
  componentsRevoke: (component_id: string) =>
    coreRequest<unknown>("components.revoke", { component_id }),
};

export type Api = typeof api;
