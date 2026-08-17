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
  settle,
  textOf,
} from "./lib/harness";
import { CoreStore } from "./lib/store.svelte";
import { appVersion } from "./lib/version";

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
  // Outside the <nav>, which the narrow layout turns into a bottom tab bar: put
  // it back inside and the status line would follow the tabs to the bottom.
  expect(app.querySelector("nav")?.textContent).not.toContain("Core connected");
});

test("the app shows its own version", () => {
  // Not compared against a written-down number, which would need editing at
  // every release: what can actually break is the build-time substitution (a
  // `define` that never fires leaves the string "undefined").
  expect(appVersion).toMatch(/^\d+\.\d+\.\d+/);

  store.primed = true;
  store.session = { logged_in: false, server_connected: false };
  const app = render(App, { store });

  expect(textOf(app)).toContain(`1Device ${appVersion}`);
  // Same reason as the connection line: inside the <nav> it would become a
  // fifth item of the narrow layout's bottom tab bar.
  expect(app.querySelector("nav")?.textContent).not.toContain("1Device");

  // And on the screen that asks the user to update, which no navigation reaches.
  cleanup();
  store.connection = { status: "incompatible", api_version: 9 };
  expect(textOf(render(App, { store }))).toContain(
    `1Device ${appVersion}`,
  );
});

// Blocking portal: connected to the account but device not linked to the vault.
test("when not linked to the account, the onboarding portal hides everything else", () => {
  store.primed = true;
  store.session = { logged_in: true, server_connected: true };
  store.account = { attested: false, fingerprint: null, holds_key: false };

  const app = render(App, { store });

  expect(app.querySelector("h1")?.textContent).toBe("Link this device");
  expect(app.querySelector("nav")).toBeNull();
});

test("when linked to the account, the normal app is shown", () => {
  store.primed = true;
  store.session = { logged_in: true, server_connected: true };
  store.account = { attested: true, fingerprint: "AB12", holds_key: true };

  const app = render(App, { store });

  expect(app.querySelector("nav")).not.toBeNull();
  expect(app.querySelector("h1")?.textContent).not.toBe("Link this device");
});

// The code has just been created: even though attested has flipped, the flag
// holds the portal until "Continue" so as not to take away the displayed code.
test("onboardingPending holds the portal even once attested", () => {
  store.primed = true;
  store.session = { logged_in: true, server_connected: true };
  store.account = { attested: true, fingerprint: "AB12", holds_key: true };
  store.onboardingPending = true;

  const app = render(App, { store });

  expect(app.querySelector("h1")?.textContent).toBe("Link this device");
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
  expect(app.querySelector("h1")?.textContent).not.toBe("Link this device");
});

// The portal is blocking AFTER login, not before: account.status is always
// callable, so a brand-new device that has never connected has
// account={attested:false} BEFORE any session — it must see the sign-in
// screen, not the portal.
test("when logged out, an unattested device does not see the portal", () => {
  store.primed = true;
  store.session = { logged_in: false, server_connected: false };
  store.account = { attested: false, fingerprint: null, holds_key: false };

  const app = render(App, { store });

  expect(app.querySelector("nav")).not.toBeNull();
  expect(app.querySelector("h1")?.textContent).not.toBe("Link this device");
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

  expect(app.querySelector("h1")?.textContent).toBe("Link this device");
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

// Mobile: a file share from the Android share sheet needs a destination, and the
// picker lives in the Devices view. Whatever the user was looking at, the app
// takes them there — the share, not the navigation, is what they just did.
test("a pending share opens the Devices view", async () => {
  store.primed = true;
  store.session = { logged_in: true, server_connected: true };
  store.account = { attested: true, fingerprint: "AB12", holds_key: true };

  const app = render(App, { store });
  expect(app.querySelector("h1")?.textContent).toBe("Account");

  store.pendingShare = {
    phase: "pick",
    id: "s_1",
    files: [{ path: "/c/shares/s_1/a.pdf", name: "a.pdf", size: 10 }],
  };
  await vi.waitFor(() =>
    expect(app.querySelector("h1")?.textContent).toBe("Devices"),
  );
  expect(textOf(app)).toContain("Send to…");
});

// A pairing under way owns the window, ABOVE the onboarding portal: the device
// being linked is inside that portal when it starts one, and a confirmation the
// user could navigate away from would leave the other side waiting.
test("a pairing under way takes over the window, onboarding portal included", () => {
  store.primed = true;
  store.session = { logged_in: true, server_connected: true };
  store.account = { attested: false, fingerprint: null, holds_key: false };
  store.pairing = {
    pairing_id: "p_1",
    role: "joiner",
    phase: "waiting",
    verification: "428 913",
  };

  const app = render(App, { store });

  expect(textOf(app)).toContain("428 913");
  expect(app.querySelector("nav")).toBeNull();
});

// The setup screen's quiet door: an account with no server at all. Taking it
// opens the account portal; nothing is remembered — once the device is in the
// account, the attestation is what holds the door open (test below).
test("the no-server door opens the account portal, and steps back through", () => {
  store.primed = true;
  store.session = { logged_in: false, server_connected: false, configured: false };
  store.account = { attested: false, fingerprint: null, holds_key: false };
  vi.spyOn(store, "loadServerConfig").mockResolvedValue({
    server_url: "",
    oidc_issuer: "",
    oidc_client_id: "",
  });

  const app = render(App, { store });
  expect(textOf(app)).toContain("Set up your server");

  click(byText(app, "button", "without a server"));
  expect(app.querySelector("h1")?.textContent).toBe("Link this device");
  // Serverless, so the server gate must NOT disarm the portal's gestures.
  expect(byText(app, "button", "This is my first device")).toHaveProperty(
    "disabled",
    false,
  );

  // No session to sign out of: the way out is back through the door.
  expect(textOf(app)).not.toContain("Sign out");
  click(byText(app, "button", "Back"));
  expect(textOf(app)).toContain("Set up your server");
});

// A serverless account IS a configuration: a device that joined one must not be
// pushed into the server setup at every start. The proof is the attestation —
// nothing else is remembered anywhere.
test("a serverless device already in its account is not pushed into setup", () => {
  store.primed = true;
  store.session = { logged_in: false, server_connected: false, configured: false };
  store.account = { attested: true, fingerprint: "AB12", holds_key: true };

  const app = render(App, { store });

  expect(app.querySelector("nav")).not.toBeNull();
  expect(textOf(app)).not.toContain("Set up your server");
});

// But not above the setup screen: a pairing on an unconfigured device runs
// behind the no-server door (which keeps the portal open while it is under
// way); with the door untaken, the setup screen still comes first.
test("an unconfigured Core still asks for a server first", () => {
  store.primed = true;
  store.session = { logged_in: false, server_connected: false, configured: false };
  store.pairing = { pairing_id: "p_1", role: "joiner", phase: "showing", code: "1D1:a:b:p_1" };
  vi.spyOn(store, "loadServerConfig").mockResolvedValue({
    server_url: "",
    oidc_issuer: "",
    oidc_client_id: "",
  });

  expect(textOf(render(App, { store }))).toContain("Set up your server");
});

// The Input section exists only where it can do something. On a phone it never
// does: the engine is not started on Android at all (a phone is neither a source
// nor a target in v1), so the facade never answers and the section never
// appears. A list only ever offers what can succeed.
test("no engine, no Input section anywhere in the navigation", () => {
  store.primed = true;
  store.session = { logged_in: false, server_connected: false };

  const app = render(App, { store });

  expect(store.inputSeen).toBe(false);
  expect(() => byText(app, "nav button", "Input")).toThrow();
  expect(textOf(app)).not.toContain("Input");
});

test("an engine that has answered puts the Input section in the navigation", () => {
  store.primed = true;
  store.session = { logged_in: false, server_connected: false };
  store.inputSeen = true;

  const app = render(App, { store });

  click(byText(app, "nav button", "Input"));
  expect(app.querySelector("h1")?.textContent).toBe("Keyboard and mouse");
});

// The section survives an engine that has gone quiet (it says so itself), but a
// Core that stops granting the scopes takes it away, and whoever was standing on
// it has to land somewhere.
test("the section going away carries the view off it", () => {
  store.primed = true;
  store.session = { logged_in: false, server_connected: false };
  store.inputSeen = true;

  const app = render(App, { store });
  click(byText(app, "nav button", "Input"));
  expect(app.querySelector("h1")?.textContent).toBe("Keyboard and mouse");

  store.inputSeen = false;
  settle();
  expect(app.querySelector("h1")?.textContent).toBe("Account");
});
