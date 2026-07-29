// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

import { afterEach, beforeEach, expect, test, vi } from "vitest";

import type { ConnectionStatus } from "../lib/core";
import { byText, cleanup, click, render, textOf } from "../lib/harness";
import { CoreStore } from "../lib/store.svelte";
import Account from "./Account.svelte";

const CONNECTED: ConnectionStatus = {
  status: "connected",
  granted_scopes: [],
  api_version: 1,
};

let store: CoreStore;

beforeEach(() => {
  store = new CoreStore();
  store.connection = CONNECTED;
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

test("before the first snapshot, nothing is asserted", () => {
  const view = render(Account, { store });

  expect(textOf(view)).toContain("Connecting to Core…");
  expect(view.querySelector("button")).toBeNull();
});

test("when logged out, the button starts login", () => {
  const login = vi.spyOn(store, "login").mockResolvedValue();
  store.primed = true;
  store.session = { logged_in: false, server_connected: false };

  const view = render(Account, { store });
  click(byText(view, "button", "Sign in"));

  expect(login).toHaveBeenCalledOnce();
});

test("when connected, the address and the server state are shown", () => {
  const logout = vi.spyOn(store, "logout").mockResolvedValue();
  store.primed = true;
  store.session = {
    logged_in: true,
    server_connected: true,
    account: { email: "account@example.test" },
  };

  const view = render(Account, { store });
  expect(textOf(view)).toContain("account@example.test");
  expect(textOf(view)).toContain("connected");

  click(byText(view, "button", "Sign out"));
  expect(logout).toHaveBeenCalledOnce();
});

// The account key's fingerprint (safety number) is the anchor of out-of-band
// verification: we show it when it is known.
test("the account fingerprint is shown when it exists", () => {
  store.primed = true;
  store.session = {
    logged_in: true,
    server_connected: true,
    account: { email: "account@example.test" },
  };
  store.account = {
    attested: true,
    fingerprint: "AB12 CD34 EF56 7890",
    holds_key: true,
  };

  const view = render(Account, { store });

  expect(textOf(view)).toContain("AB12 CD34 EF56 7890");
  expect(textOf(view)).toContain("compare");
});

// The Core returns `account` as the IdP gave it: the email may be missing.
test("an account with no address does not break the display", () => {
  store.primed = true;
  store.session = { logged_in: true, server_connected: false, account: {} };

  const view = render(Account, { store });

  expect(textOf(view)).toContain("unknown address");
  expect(textOf(view)).toContain("unreachable");
});

test("without the Core, the actions are disarmed", () => {
  store.primed = true;
  store.connection = { status: "connecting" };
  store.session = { logged_in: false, server_connected: false };

  const view = render(Account, { store });

  expect(byText(view, "button", "Sign in")).toHaveProperty(
    "disabled",
    true,
  );
});

test("during an action, the buttons are disarmed", () => {
  store.primed = true;
  store.busy = true;
  store.session = { logged_in: true, server_connected: true };

  const view = render(Account, { store });

  expect(byText(view, "button", "Sign out")).toHaveProperty(
    "disabled",
    true,
  );
});

// A device with no session can be handed the account by one that has it — no
// browser at all. This is the other way in, next to "Sign in".
test("signing in is not the only way in", () => {
  store.primed = true;
  store.session = { logged_in: false, server_connected: false };

  const view = render(Account, { store });

  expect(textOf(view)).toContain("Already have a device on this account?");
  expect(byText(view, "button", "Show a code")).toBeTruthy();
});

// Attested but without the account's private key. Both ways back in have to be
// on this screen, and this screen only: the onboarding portal shows for a device
// that is NOT attested, so a keyless one would otherwise be told to type a
// recovery code with nowhere to type it.
test("a device without the key is offered both ways back in", () => {
  store.primed = true;
  store.session = { logged_in: true, server_connected: true };
  store.account = { attested: true, fingerprint: "AB12", holds_key: false };

  const view = render(Account, { store });

  expect(textOf(view)).toContain("has your account, but not its key");
  expect(byText(view, "button", "Show a code")).toBeTruthy();
  expect(view.querySelector('input[aria-label="Recovery code"]')).toBeTruthy();
  expect(byText(view, "button", "Join")).toBeTruthy();
});

test("a device that holds the key is offered nothing of the sort", () => {
  store.primed = true;
  store.session = { logged_in: true, server_connected: true };
  store.account = { attested: true, fingerprint: "AB12", holds_key: true };

  const view = render(Account, { store });
  expect(textOf(view)).not.toContain("not its key");
  expect(view.querySelector('input[aria-label="Recovery code"]')).toBeNull();
});
