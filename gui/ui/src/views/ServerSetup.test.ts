// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

// The server-setup screen: one address, the deployment's own settings read from
// it, and the manual fields kept for a server that publishes none. The store is
// stubbed here — the discover/write/reload path is covered in store.test.ts and
// gui/tests/api/.

import { afterEach, beforeEach, expect, test, vi } from "vitest";

import {
  byLabel,
  byText,
  cleanup,
  click,
  render,
  textOf,
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

const saveButton = (view: HTMLElement) =>
  byText(view, "button", "Save and continue") as HTMLButtonElement;

const manualToggle = (view: HTMLElement) =>
  byText(view, "button", "Enter the OpenID Connect settings manually");

function fillManual(
  view: HTMLElement,
  url: string,
  issuer: string,
  id: string,
): void {
  typeInto(byLabel(view, "Server address"), url);
  typeInto(byLabel(view, "OpenID Connect issuer"), issuer);
  typeInto(byLabel(view, "OpenID Connect client ID"), id);
}

test("only the address is asked for", () => {
  const view = render(ServerSetup, { store, firstRun: true });

  expect(byLabel(view, "Server address")).not.toBeNull();
  // The issuer and the client belong to the deployment: the server is asked.
  expect(view.querySelector('[aria-label="OpenID Connect issuer"]')).toBeNull();
  expect(
    view.querySelector('[aria-label="OpenID Connect client ID"]'),
  ).toBeNull();
});

// The no-server door: offered on first run when the App hands us what taking it
// means, and only there — the settings view configures a server, full stop.
test("first run offers the no-server way in, the settings view does not", () => {
  const withoutServer = vi.fn();

  const first = render(ServerSetup, { store, firstRun: true, withoutServer });
  click(byText(first, "button", "without a server"));
  expect(withoutServer).toHaveBeenCalledOnce();

  cleanup();
  const settings = render(ServerSetup, { store });
  expect(textOf(settings)).not.toContain("without a server");
});

test("an address is all it takes to continue", () => {
  const discover = vi.spyOn(store, "setUpFromAddress").mockResolvedValue("saved");
  const view = render(ServerSetup, { store, firstRun: true });
  expect(saveButton(view).disabled).toBe(true);

  // A bare host is enough — the Core derives the rest of it.
  typeInto(byLabel(view, "Server address"), "  1device.example.com  ");
  expect(saveButton(view).disabled).toBe(false);
  click(saveButton(view));

  expect(discover).toHaveBeenCalledWith("1device.example.com");
});

test("a server that publishes nothing reveals the fields, and explains", async () => {
  vi.spyOn(store, "setUpFromAddress").mockResolvedValue("unpublished");
  const view = render(ServerSetup, { store, firstRun: true });

  typeInto(byLabel(view, "Server address"), "old-server.example.com");
  click(saveButton(view));
  await vi.waitFor(() =>
    expect(
      view.querySelector('[aria-label="OpenID Connect issuer"]'),
    ).not.toBeNull(),
  );

  // Why they appeared — a generic error banner would not say it.
  expect(textOf(view)).toContain("does not publish its settings");
  // The typed address stays put; the manual path wants it complete, so Save
  // waits for a ws(s) URL rather than writing a host into config.json.
  expect((byLabel(view, "Server address") as HTMLInputElement).value).toBe(
    "old-server.example.com",
  );
  expect(saveButton(view).disabled).toBe(true);
});

test("the manual fields can be asked for without a round-trip", () => {
  const discover = vi.spyOn(store, "setUpFromAddress");
  const view = render(ServerSetup, { store, firstRun: true });

  click(manualToggle(view));

  expect(byLabel(view, "OpenID Connect issuer")).not.toBeNull();
  expect(discover).not.toHaveBeenCalled();
  // Nothing failed: no explanation of a failure either.
  expect(textOf(view)).not.toContain("does not publish");
});

test("the manual path saves the four fields itself", () => {
  const save = vi.spyOn(store, "saveServerConfig").mockResolvedValue(true);
  const discover = vi.spyOn(store, "setUpFromAddress");
  const view = render(ServerSetup, { store, firstRun: true });
  click(manualToggle(view));

  // A non-ws address is refused client-side here: it goes straight into
  // config.json, where the daemon requires ws:// or wss://.
  fillManual(view, "https://relay.example/ws", "https://idp.example", "id");
  expect(saveButton(view).disabled).toBe(true);

  fillManual(view, "  wss://relay.example/ws  ", " https://idp.example ", " public-id ");
  typeInto(byLabel(view, "OpenID Connect client secret"), " GOCSPX-xyz ");
  expect(saveButton(view).disabled).toBe(false);
  click(saveButton(view));

  expect(save).toHaveBeenCalledWith({
    server_url: "wss://relay.example/ws",
    oidc_issuer: "https://idp.example",
    oidc_client_id: "public-id",
    oidc_client_secret: "GOCSPX-xyz",
  });
  // The server is not asked when the user has taken over.
  expect(discover).not.toHaveBeenCalled();
});

test("a blank secret is sent as null (the PKCE default)", () => {
  const save = vi.spyOn(store, "saveServerConfig").mockResolvedValue(true);
  const view = render(ServerSetup, { store, firstRun: true });
  click(manualToggle(view));

  fillManual(view, "wss://relay.example/ws", "https://idp.example", "id");
  // client secret left blank
  click(saveButton(view));

  expect(save).toHaveBeenCalledWith(
    expect.objectContaining({ oidc_client_secret: null }),
  );
});

test("the toggle goes back to letting the server answer", async () => {
  vi.spyOn(store, "setUpFromAddress").mockResolvedValue("unpublished");
  const view = render(ServerSetup, { store, firstRun: true });
  typeInto(byLabel(view, "Server address"), "host.example");
  click(saveButton(view));
  await vi.waitFor(() => expect(textOf(view)).toContain("does not publish"));

  click(byText(view, "button", "Read the settings from the server"));

  expect(view.querySelector('[aria-label="OpenID Connect issuer"]')).toBeNull();
  expect(textOf(view)).not.toContain("does not publish");
});
