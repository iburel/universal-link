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
