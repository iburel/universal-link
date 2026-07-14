// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

import { afterEach, beforeEach, expect, test, vi } from "vitest";

import App from "./App.svelte";
import {
  byLabel,
  byText,
  cleanup,
  click,
  render,
  textOf,
} from "./lib/harness";
import { CoreStore } from "./lib/store.svelte";

let store: CoreStore;

beforeEach(() => {
  store = new CoreStore();
  // The shell isn't here: App mounts the store, and we don't want its IPC.
  vi.spyOn(store, "start").mockResolvedValue();
  vi.spyOn(store, "stop").mockReturnValue();
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

test("an incompatible Core blocks the interface", () => {
  store.connection = { status: "incompatible", api_version: 9 };

  const app = render(App, { store });

  expect(textOf(app)).toContain("Incompatible version");
  expect(textOf(app)).toContain("version 9");
  expect(app.querySelector("nav")).toBeNull();
});

test("navigation changes the view", () => {
  store.primed = true;
  store.session = { logged_in: false, server_connected: false };

  const app = render(App, { store });

  expect(app.querySelector("h1")?.textContent).toBe("Account");
  click(byText(app, "nav button", "Devices"));
  expect(app.querySelector("h1")?.textContent).toBe("Devices");
  click(byText(app, "nav button", "Approvals"));
  expect(app.querySelector("h1")?.textContent).toBe("Approvals");
});

test("the number of pending requests is shown on the tab", () => {
  store.pending = [
    {
      request_id: "r_1",
      name: "clipnet",
      role: "custom",
      scopes: [],
      peer_info: {},
    },
  ];

  const app = render(App, { store });

  expect(byText(app, "nav button", "Approvals").textContent).toContain("1");
});

test("the message banner closes", () => {
  store.notice = { kind: "error", text: "Server unreachable." };

  const app = render(App, { store });
  expect(textOf(app)).toContain("Server unreachable.");

  click(byLabel(app, "Close message"));
  expect(app.querySelector(".banner")).toBeNull();
});

// As long as no snapshot has arrived, "Core unreachable" would be false: we
// haven't yet displayed anything that could be stale.
test("the frozen-data banner appears only after a first snapshot", () => {
  store.connection = { status: "connecting" };

  const app = render(App, { store });
  expect(textOf(app)).not.toContain("frozen");

  cleanup();
  store.primed = true;
  const primed = render(App, { store });
  expect(textOf(primed)).toContain("Core unreachable");
});

test("the Core status is shown with its API version", () => {
  store.connection = {
    status: "connected",
    granted_scopes: [],
    api_version: 1,
  };

  const app = render(App, { store });

  expect(textOf(app)).toContain("Core connected (API v1)");
});

// Blocking portal: connected to the account but device not linked to the vault.
test("when not linked to the account, the onboarding portal hides everything else", () => {
  store.primed = true;
  store.session = { logged_in: true, server_connected: true };
  store.account = { attested: false, fingerprint: null };

  const app = render(App, { store });

  expect(textOf(app)).toContain("Link this device");
  expect(app.querySelector("nav")).toBeNull();
});

test("when linked to the account, the normal app is shown", () => {
  store.primed = true;
  store.session = { logged_in: true, server_connected: true };
  store.account = { attested: true, fingerprint: "AB12" };

  const app = render(App, { store });

  expect(app.querySelector("nav")).not.toBeNull();
  expect(textOf(app)).not.toContain("Link this device");
});

// The code has just been created: even though attested has flipped, the flag
// holds the portal until "Continue" so as not to take away the displayed code.
test("onboardingPending holds the portal even once attested", () => {
  store.primed = true;
  store.session = { logged_in: true, server_connected: true };
  store.account = { attested: true, fingerprint: "AB12" };
  store.onboardingPending = true;

  const app = render(App, { store });

  expect(textOf(app)).toContain("Link this device");
  expect(app.querySelector("nav")).toBeNull();
});

// A Core older than C7 (account null) does not open a portal: we don't block on
// a capability the Core lacks.
test("with no known account state, no portal", () => {
  store.primed = true;
  store.session = { logged_in: true, server_connected: true };
  store.account = null;

  const app = render(App, { store });

  expect(app.querySelector("nav")).not.toBeNull();
  expect(textOf(app)).not.toContain("Link this device");
});

// The portal is blocking AFTER login, not before: account.status is always
// callable, so a brand-new device that has never connected has
// account={attested:false} BEFORE any session — it must see the sign-in
// screen, not the portal.
test("when logged out, an unattested device does not see the portal", () => {
  store.primed = true;
  store.session = { logged_in: false, server_connected: false };
  store.account = { attested: false, fingerprint: null };

  const app = render(App, { store });

  expect(app.querySelector("nav")).not.toBeNull();
  expect(textOf(app)).not.toContain("Link this device");
});

// onboardingPending holds the portal ON ITS OWN: even if the account state is
// momentarily unknown (a background account.status failed → null), the
// displayed code must not be taken away by a portal lift.
test("onboardingPending holds the portal even if the account state is null", () => {
  store.primed = true;
  store.session = { logged_in: true, server_connected: true };
  store.account = null;
  store.onboardingPending = true;

  const app = render(App, { store });

  expect(textOf(app)).toContain("Link this device");
  expect(app.querySelector("nav")).toBeNull();
});

// A fresh install: the Core reports configured:false → the setup screen gates
// everything, BEFORE any sign-in is possible.
test("an unconfigured Core shows the first-run server setup", () => {
  store.primed = true;
  store.session = { logged_in: false, server_connected: false, configured: false };
  vi.spyOn(store, "loadServerConfig").mockResolvedValue({
    server_url: "",
    oidc_issuer: "",
    oidc_client_id: "",
  });

  const app = render(App, { store });

  expect(textOf(app)).toContain("Set up your server");
  expect(app.querySelector("nav")).toBeNull();
});

// Once configured, the normal app is shown and the server is editable from a
// "Server" settings tab.
test("a configured Core shows the app with a Server settings tab", () => {
  store.primed = true;
  store.session = { logged_in: false, server_connected: false, configured: true };
  vi.spyOn(store, "loadServerConfig").mockResolvedValue({
    server_url: "wss://relay.example/ws",
    oidc_issuer: "https://idp.example",
    oidc_client_id: "id",
  });

  const app = render(App, { store });

  expect(app.querySelector("nav")).not.toBeNull();
  expect(textOf(app)).not.toContain("Set up your server");

  click(byText(app, "nav button", "Server"));
  expect(textOf(app)).toContain("this device connects to");
});
