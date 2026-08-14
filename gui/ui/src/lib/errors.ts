// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

/** Translation of the shell's errors (CoreError) into displayable sentences. */

import type { CoreError } from "./core";

export function isCoreError(e: unknown): e is CoreError {
  return (
    typeof e === "object" &&
    e !== null &&
    "kind" in e &&
    typeof (e as CoreError).message === "string"
  );
}

/** Application codes from the Core (`error.data.code`) that the GUI can trigger. */
const APP_MESSAGES: Record<string, string> = {
  ALREADY_LOGGED_IN: "A session is already open on this device.",
  SERVER_UNREACHABLE: "Server unreachable.",
  INVALID_CODE: "Invalid recovery code.",
  ACCOUNT_KEY_SET: "An account is already set up on this device.",
  ACCOUNT_KEY_SAVE_FAILED: "Could not save the account key.",
  DEVICE_UNKNOWN: "This device no longer exists.",
  DEVICE_OFFLINE: "This device is offline.",
  INVALID_TOKEN: "Invalid or revoked token.",
  SCOPE_DENIED: "The Core refused this operation (missing permission).",
  ROLE_CONFLICT: "This role is already held by another component.",
  NOT_ENROLLED: "This interface is not enrolled with the Core.",
  NO_ACCOUNT_KEY: "This device cannot vouch for another: it holds no account key.",
  PAIRING_UNKNOWN: "This link has expired or was already answered.",
  // A pairing joins exactly two devices, so the case a user actually meets here
  // is a code somebody else got to first — and that is the one signal telling
  // them so. "Not at that step" was true and said nothing; the other cases this
  // code covers (confirming before anyone scanned) are unreachable from the UI.
  PAIRING_STATE:
    "That code was already answered — show a new one. If it was not you, someone else read it.",
  PAIRING_LIMIT: "The server is handling too many links at once — try again.",
  // `pairing.accept` of a code shown by a device the account struck off. The
  // sentence says the two things the human needs: it is not a glitch to retry,
  // and the machine in front of them is not lost — it comes back as a new device.
  DEVICE_REVOKED:
    "That device was struck from the account for good — it can only come back as a new device.",
  // `devices.revoke` aimed at this very device, with no server: a tombstone
  // cannot be withdrawn, so the Core refuses to let a device bar itself.
  CANNOT_REVOKE_SELF:
    "This device cannot revoke itself — do it from another device on the account.",
  // Deliberately absent: `session.discover`'s NO_DESCRIPTOR, which the setup
  // screen acts on rather than reports (it reveals the fields to fill in), and
  // INVALID_DESCRIPTOR, whose own message names the field at fault — more use to
  // whoever runs the server than a sentence written here.
};

const KIND_MESSAGES: Record<Exclude<CoreError["kind"], "rpc">, string> = {
  not_connected: "The Core is not reachable.",
  timeout: "The Core did not respond in time.",
  disconnected: "The connection to the Core was lost.",
};

export function humanize(e: unknown): string {
  if (!isCoreError(e)) {
    return e instanceof Error ? e.message : String(e);
  }
  if (e.kind !== "rpc") return KIND_MESSAGES[e.kind];
  if (e.data_code && APP_MESSAGES[e.data_code]) {
    return APP_MESSAGES[e.data_code];
  }
  return e.message;
}

/**
 * `-32602 invalid params`. On `components.approve` / `components.deny`, this is
 * the only possible response for a vanished `request_id` — the Core removes a
 * request whose component has disconnected, and a decision made by another GUI
 * notifies no one. The other causes (`scopes` outside the requested set,
 * `components.approve` granted) are out of this interface's reach: it only ever
 * offers checkboxes drawn from the request, minus that one scope.
 */
export function isInvalidParams(e: unknown): boolean {
  return isCoreError(e) && e.kind === "rpc" && e.code === -32602;
}

/** Whether this is the Core answering with a given application code. */
export function isAppCode(e: unknown, code: string): boolean {
  return isCoreError(e) && e.kind === "rpc" && e.data_code === code;
}

/**
 * `-32601 method not found`. On `pairing.*` this means one of the two halves is
 * older than pairing: the Core, or the server it relays to (the Core passes the
 * server's refusal on as it stands, and the code cannot tell them apart). Either
 * way there is nothing the user can do here, so the interface stops offering it.
 */
export function isUnknownMethod(e: unknown): boolean {
  return isCoreError(e) && e.kind === "rpc" && e.code === -32601;
}

/**
 * Why a pairing ended, as `pairing.failed` words it (doc/core-api.md). The
 * vocabulary is the Core's; anything outside it — a server application code, a
 * word from a later version — falls back to a sentence that carries it
 * verbatim, which is of more use to whoever has to explain it than a shrug.
 */
const PAIRING_REASONS: Record<string, string> = {
  declined: "The other device declined.",
  abandoned: "The other device dropped out.",
  expired: "The code expired before the link was confirmed.",
  channel: "That code could not be used — start again with a fresh one.",
  bundle: "What the other device sent could not be read.",
  other_account: "That device belongs to a different account.",
  install: "The account key could not be saved on this device.",
  enroll: "The server refused to add this device to the account.",
  server: "The link was lost — the server dropped it.",
  state: "The link went out of step and was abandoned.",
  // Local-network pairing: the exchange found no account key on either end —
  // or the account left this device while the link was under way.
  no_account: "Neither device holds the account key — the link needs one that does.",
  PAIRING_UNKNOWN: "The link expired before it was confirmed.",
};

export function pairingFailure(reason: string): string {
  return PAIRING_REASONS[reason] ?? `The link failed (${reason}).`;
}

/**
 * Why a scan produced no code (gui-mobile/src/scan.rs). `null` is a real answer
 * and the most common one: a user who backed out of the camera knows they did,
 * and a banner explaining it to them would be noise.
 *
 * Every sentence names the way out, because there always is one — the code can be
 * pasted, and on the other device the recovery code still works.
 */
const SCAN_REASONS: Record<string, string | null> = {
  cancelled: null,
  busy: null,
  denied:
    "1Device needs the camera to read a code. Allow it in Android settings, or paste the code instead.",
  no_camera: "This device has no camera — paste the code instead.",
  failed: "The camera could not be opened. Paste the code instead.",
};

export function scanFailure(reason: string): string | null {
  return reason in SCAN_REASONS
    ? SCAN_REASONS[reason]
    : "The code could not be read. Paste it instead.";
}
