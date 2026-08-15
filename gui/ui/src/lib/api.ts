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

export type Platform = "windows" | "macos" | "linux" | "android";

/** The device record from server-api.md, enriched with `is_self` by the Core. */
export interface Device {
  device_id: string;
  name: string;
  platform: Platform;
  node_id?: string;
  relay_url?: string | null;
  online: boolean;
  /** This machine hears the device on the local network (mDNS) — first-hand
   * presence, alive with or without the server. */
  lan: boolean;
  /** The Core's one presence verdict: what a send could reach right now —
   * the LAN, or (while the server link is up) an online device with a
   * published relay. Gate sends on THIS, never on `online` alone. */
  reachable: boolean;
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
  /**
   * Whether a server + OIDC is configured. Distinguishes "never configured"
   * (→ first-run setup screen) from "configured but the server is down" (both
   * otherwise read as `server_connected: false`). Present in `session.status`;
   * ABSENT from `session.changed` deltas (the store resyncs after those, which
   * re-reads it) — hence optional. See {@link Api.sessionReload}.
   */
  configured?: boolean;
}

/**
 * The state of the account key (C7). `attested`: has this device joined the
 * account vault (root of trust installed) — without which `files.send` fails
 * closed, for lack of an attestation the peer can verify. `fingerprint` is the
 * safety number to compare across devices; `null` until attested.
 *
 * `holds_key` says whether this device also holds the account's PRIVATE key, at
 * rest in its keyring — which is what lets it vouch for a device joining the
 * account. It can be `false` while `attested` is `true`: a device whose keyring no
 * longer holds the seed still verifies peers, it just cannot vouch. The way back
 * in is a pairing, or the recovery code (`joinAccount`).
 */
export interface AccountKey {
  attested: boolean;
  fingerprint: string | null;
  holds_key: boolean;
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

/**
 * The answer of an operation the server gates on a FRESH OIDC token: either the
 * Core minted one from its keyring and it is done, or the browser has to settle
 * it. `devices.revoke` and `pairing.confirm` share the shape because they share
 * the gate.
 */
export type Settled =
  | { status: "done" }
  | { status: "reauth_required"; auth_url: string };

export type PairingRole = "joiner" | "sponsor";

/**
 * What the device on the other end declares about itself — the very record the
 * account's directory will hold. This is what the human is shown before saying
 * yes, so it has to be the whole of it: the name and platform to recognize the
 * thing in front of them, and `node_id`, the cryptographic identity an intruder
 * cannot make match.
 */
export interface PairingDevice {
  name: string;
  platform: Platform;
  node_id?: string;
}

/**
 * `pairing.offer`: a code to show, and which end of the exchange we are on. No
 * confirmation number yet — it comes out of the channel, and there is no channel
 * until someone has read the code.
 */
export interface PairingOffer {
  pairing_id: string;
  code: string;
  role: PairingRole;
  /** Seconds. The Core counts it and says so (`pairing.failed`, `expired`). */
  expires_in: number;
}

/** `pairing.accept`: `device` is present when we turn out to be the sponsor. */
export interface PairingClaim {
  pairing_id: string;
  role: PairingRole;
  /** The confirmation number: both ends show it, the human compares. */
  verification?: string;
  device?: PairingDevice;
}

/**
 * What a deployment publishes about itself — the settings to write into
 * `config.json`, read from the server rather than typed in. `oidc_client_secret`
 * is `null` for an IdP that wants none.
 */
export interface DiscoveredDeployment {
  server_url: string;
  oidc_issuer: string;
  oidc_client_id: string;
  oidc_client_secret: string | null;
}

export const api = {
  sessionStatus: () => coreRequest<SessionState>("session.status"),
  /** The caller opens `auth_url`; completion arrives via `session.changed`. */
  sessionLogin: () => coreRequest<{ auth_url: string }>("session.login"),
  sessionLogout: () => coreRequest<unknown>("session.logout"),
  /**
   * Re-reads `config.json` (which the shell has just written via
   * `setServerConfig`) and applies it in place — no restart. Returns the fresh
   * `session.status`. `INVALID_CONFIG` if the file is malformed/half-filled.
   */
  sessionReload: () => coreRequest<SessionState>("session.reload"),
  /**
   * Reads the deployment descriptor at `url` — anything a user might type, a bare
   * host included — and returns the settings to write. The Core writes nothing:
   * the shell does, then {@link Api.sessionReload}.
   *
   * `NO_DESCRIPTOR`: something answered but publishes none (a server older than
   * that endpoint, or another site) — the setup screen falls back to asking.
   * `INVALID_DESCRIPTOR`: one missing what a login needs, the message naming the
   * field. `-32602`: the address is not one, and nothing was attempted.
   */
  sessionDiscover: (url: string) =>
    coreRequest<DiscoveredDeployment>("session.discover", { url }),

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
    coreRequest<Settled>("devices.revoke", { device_id }),

  /**
   * Shows a code for another device to scan or paste. The role comes back with
   * it: this device sponsors when it can vouch (it holds the account key and is
   * in the account), and joins otherwise — it is not the caller's to choose.
   * `SERVER_UNREACHABLE` when no server is configured, or when a signed-in Core
   * has lost it; `-32601` from a Core or a server too old to pair.
   */
  pairingOffer: () => coreRequest<PairingOffer>("pairing.offer"),
  /** A code was scanned or pasted. `-32602` if it is not one. */
  pairingAccept: (code: string) =>
    coreRequest<PairingClaim>("pairing.accept", { code }),
  /**
   * The human said yes (sponsor only): the account key crosses, sealed. Same
   * fresh-token gate as `devices.revoke`, so the same possible browser detour —
   * whose outcome then arrives as a `pairing.*` notification.
   */
  pairingConfirm: (pairing_id: string) =>
    coreRequest<Settled>("pairing.confirm", { pairing_id }),
  /** Declined, or the screen was left. Idempotent on the Core's side. */
  pairingCancel: (pairing_id: string) =>
    coreRequest<unknown>("pairing.cancel", { pairing_id }),

  /**
   * Send flat files to a device (fire-and-forget). Returns a `transfer_id`, but
   * tracking goes through the `transfer.*` notifications — the caller need not
   * remember it. A folder or a missing path → `-32602`; a target outside the
   * directory → `DEVICE_UNKNOWN`; no route → `DEVICE_OFFLINE`.
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
