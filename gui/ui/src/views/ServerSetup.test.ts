// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

// The server-setup screen: light client-side validation gating "Save", and the
// trimmed fields handed to the store (which is stubbed here — the write/reload
// path is covered in store.test.ts and gui/tests/api/).

import { afterEach, beforeEach, expect, test, vi } from "vitest";

import {
  byLabel,
  byText,
  cleanup,
  click,
  render,
  type as typeInto,
} from "../lib/harness";
import { CoreStore } from "../lib/store.svelte";
import ServerSetup from "./ServerSetup.svelte";

let store: CoreStore;

beforeEach(() => {
  store = new CoreStore();
  store.connection = { status: "connected", granted_scopes: [], api_version: 1 };
  store.primed = true;
  store.session = { logged_in: false, server_connected: false, configured: false };
  // No IPC in a view test: the pre-fill is stubbed (fresh install = blank).
  vi.spyOn(store, "loadServerConfig").mockResolvedValue({
    server_url: "",
    oidc_issuer: "",
    oidc_client_id: "",
  });
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

function fill(view: HTMLElement, url: string, issuer: string, id: string): void {
  typeInto(byLabel(view, "Server address"), url);
  typeInto(byLabel(view, "OpenID Connect issuer"), issuer);
  typeInto(byLabel(view, "OpenID Connect client ID"), id);
}

const saveButton = (view: HTMLElement) =>
  byText(view, "button", "Save and continue") as HTMLButtonElement;

test("Save stays disabled until the required fields are valid", () => {
  const view = render(ServerSetup, { store, firstRun: true });
  expect(saveButton(view).disabled).toBe(true);

  // A non-ws server URL is refused client-side (spares a round-trip).
  fill(view, "https://relay.example/ws", "https://idp.example", "id");
  expect(saveButton(view).disabled).toBe(true);

  // ws:// URL + http(s) issuer + a client id: valid.
  fill(view, "wss://relay.example/ws", "https://idp.example", "id");
  expect(saveButton(view).disabled).toBe(false);
});

test("saving hands the trimmed fields to the store", () => {
  const save = vi.spyOn(store, "saveServerConfig").mockResolvedValue(true);
  const view = render(ServerSetup, { store, firstRun: true });

  fill(view, "  wss://relay.example/ws  ", " https://idp.example ", " public-id ");
  typeInto(byLabel(view, "OpenID Connect client secret"), " GOCSPX-xyz ");
  click(saveButton(view));

  expect(save).toHaveBeenCalledWith({
    server_url: "wss://relay.example/ws",
    oidc_issuer: "https://idp.example",
    oidc_client_id: "public-id",
    oidc_client_secret: "GOCSPX-xyz",
  });
});

test("a blank secret is sent as null (the PKCE default)", () => {
  const save = vi.spyOn(store, "saveServerConfig").mockResolvedValue(true);
  const view = render(ServerSetup, { store, firstRun: true });

  fill(view, "wss://relay.example/ws", "https://idp.example", "id");
  // client secret left blank
  click(saveButton(view));

  expect(save).toHaveBeenCalledWith(
    expect.objectContaining({ oidc_client_secret: null }),
  );
});
