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
