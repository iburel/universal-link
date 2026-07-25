// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

/**
 * The bridge to the Core, on the frontend side — a mirror of the shell's
 * contract (gui/src/bridge.rs, pinned by gui/tests/api/). All access to the
 * Core goes through here: one proxied JSON-RPC request, one connection
 * snapshot, two event streams.
 */

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type ConnectionStatus =
  | { status: "connecting" }
  | { status: "connected"; granted_scopes: string[]; api_version: number }
  | { status: "incompatible"; api_version: number };

/** A faithful relay of the shell's error (RequestError on the Rust side). */
export interface CoreError {
  kind: "not_connected" | "timeout" | "disconnected" | "rpc";
  message: string;
  code?: number;
  data_code?: string;
}

export interface CoreNotification {
  method: string;
  params: unknown;
}

/**
 * Full JSON-RPC proxy to the Core. Rejects with a {@link CoreError} —
 * fail-closed: when disconnected, failure is immediate.
 */
export function coreRequest<T = unknown>(
  method: string,
  params?: unknown,
): Promise<T> {
  return invoke<T>("core_request", { method, params });
}

/** Connection snapshot. Subscribe first, read second: no gap. */
export function connectionStatus(): Promise<ConnectionStatus> {
  return invoke<ConnectionStatus>("connection_status");
}

export function onConnectionChanged(
  handler: (status: ConnectionStatus) => void,
): Promise<UnlistenFn> {
  return listen<ConnectionStatus>("core:connection", (event) =>
    handler(event.payload),
  );
}

export function onCoreNotification(
  handler: (notification: CoreNotification) => void,
): Promise<UnlistenFn> {
  return listen<CoreNotification>("core:notification", (event) =>
    handler(event.payload),
  );
}

/** A file waiting for a destination: a copy in the app's cache (ShareFiles.kt). */
export interface SharedFile {
  /** Absolute path, readable by the Core — what `files.send` takes. */
  path: string;
  /** The name the receiving device will see (the path's basename). */
  name: string;
  size: number;
}

/**
 * Progress of a share from the Android share sheet (gui-mobile/src/share.rs).
 * MOBILE ONLY: the desktop shell never emits `core:share`, so this stream is
 * simply always silent there.
 *
 * Shared TEXT goes to the whole account through the clipboard, and reports
 * itself: `sending` → `done` | `error`. It never stops on `sending`, because the
 * banner it drives would otherwise stay up forever. `done` is the Core's
 * delivery report and may still say `delivered: 0`: reaching nobody is an
 * outcome, not an error to swallow.
 *
 * Shared FILES go to ONE device, so they stop half-way and wait for the user:
 * `preparing` (the bytes are being copied out of the share) → `pick` (choose a
 * destination) | `error`. From `pick` on, the frontend drives — the send is a
 * plain `files.send`, tracked like a desktop drag-and-drop.
 */
export type ShareStatus =
  | { phase: "sending"; targets?: number }
  | { phase: "done"; delivered: number; failed: number }
  | { phase: "preparing"; files: number }
  | { phase: "pick"; id: string; files: SharedFile[] }
  | {
      phase: "error";
      reason:
        | "no_devices"
        | "too_large"
        | "refused"
        | "unconfirmed"
        | "not_signed_in"
        | "offline"
        | "unreadable"
        | "no_space";
      detail?: string;
    };

export function onShareStatus(
  handler: (status: ShareStatus) => void,
): Promise<UnlistenFn> {
  return listen<ShareStatus>("core:share", (event) => handler(event.payload));
}

/**
 * The last share's retained status, or `null`. Subscribe first, read second —
 * same contract as {@link connectionStatus}, and for a sharper reason: a share
 * can COLD-START the app, so the worker publishes its status long before this
 * webview exists, and a Tauri event only reaches listeners already registered.
 *
 * The desktop shell does not implement the command — it can never have a share —
 * so a rejection there IS the answer "no share surface", not an error.
 */
export async function shareStatus(): Promise<ShareStatus | null> {
  try {
    return await invoke<ShareStatus | null>("share_status");
  } catch {
    return null;
  }
}

/**
 * Releases a shared file's cache copy: the destination was cancelled, or the
 * transfer that was reading it is over. Idempotent on the Rust side, and a
 * no-op on the desktop shell (which never hands out a share to release).
 */
export async function discardShare(id: string): Promise<void> {
  try {
    await invoke("discard_share", { id });
  } catch {
    // Nothing to recover: the copy lives in a cache the OS can reclaim, and the
    // next share sweeps whatever is left behind (see share.rs `register`).
  }
}

/**
 * Says a destination has been chosen for a share: the question is answered, but
 * its bytes must stay until the transfer has read them. Without this, a share
 * arriving during a transfer would look like an unanswered one and take the files
 * out from under it.
 */
export async function shareTaken(id: string): Promise<void> {
  try {
    await invoke("share_taken", { id });
  } catch {
    // Desktop shell, or a share the Rust side has already retired.
  }
}

/**
 * Work that only the frontend knows is under way (`true`) or over (`false`), so
 * the shell can keep the process alive through it.
 *
 * MOBILE ONLY, and only for what the Core's event stream cannot tell the shell by
 * itself:
 *
 * - `"auth"` — a round-trip through the browser (signing in, or re-authorizing a
 *   revocation). Both come back into the Core's own loopback listener, while
 *   Android has this app backgrounded behind the browser — where it suspends its
 *   network and may reclaim the process outright.
 * - `"send"` — a `files.send` waiting to be accepted. Until `transfer.started`
 *   there is no transfer for the shell to follow, and the user may well pocket
 *   the phone the moment they have tapped.
 *
 * The shell bounds every hold on its own side, so a webview that dies mid-flow
 * cannot leak one; `false` is an optimization, not the only way out.
 *
 * The desktop shell does not implement the command: a rejection there IS the
 * answer ("this process needs no help staying alive").
 */
export async function keepAlive(
  kind: "auth" | "send",
  on: boolean,
): Promise<void> {
  try {
    await invoke("keep_alive", { kind, on });
  } catch {
    // Desktop, or a shell too old to know: the flow proceeds either way.
  }
}

/**
 * The server + OIDC fields the setup screen collects, mirroring the shell's
 * `ServerConfigForm` (gui/src/bridge.rs). The secret is optional (a conformant
 * PKCE IdP has none); a blank one clears the key on write.
 */
export interface ServerConfigInput {
  server_url: string;
  oidc_issuer: string;
  oidc_client_id: string;
  oidc_client_secret?: string | null;
}

/**
 * Writes the fields into `config.json` (the GUI is the sole writer of that
 * file). The caller then triggers `session.reload` so the Core picks them up.
 */
export function setServerConfig(config: ServerConfigInput): Promise<void> {
  return invoke("set_server_config", { config });
}

/** The server fields currently in `config.json`, to pre-fill the setup screen. */
export function getServerConfig(): Promise<ServerConfigInput> {
  return invoke<ServerConfigInput>("get_server_config");
}
