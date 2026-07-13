// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

import { flushSync } from "svelte";
import { afterEach, beforeEach, expect, test, vi } from "vitest";

import type { ConnectionStatus } from "../lib/core";
import {
  byLabel,
  byText,
  cleanup,
  click,
  press,
  render,
  textOf,
  type as typeInto,
} from "../lib/harness";
import { CoreStore } from "../lib/store.svelte";
import Onboarding from "./Onboarding.svelte";

const CONNECTED: ConnectionStatus = {
  status: "connected",
  granted_scopes: [],
  api_version: 1,
};

let store: CoreStore;

beforeEach(() => {
  store = new CoreStore();
  store.connection = CONNECTED;
  store.primed = true;
  // Connected to the account, server ready: onboarding can act.
  store.session = { logged_in: true, server_connected: true };
  store.account = { attested: false, fingerprint: null };
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

/** Lets an async action resolve, then propagates reactivity. */
async function settle(view: HTMLElement, text: string) {
  await vi.waitFor(() => {
    flushSync();
    expect(textOf(view)).toContain(text);
  });
}

test("the portal offers to create or join, by intent", () => {
  const view = render(Onboarding, { store });

  expect(textOf(view)).toContain("This is my first device");
  expect(textOf(view)).toContain("I already have a device on this account");
});

test("creating shows the recovery code only once", async () => {
  const create = vi
    .spyOn(store, "createAccount")
    .mockResolvedValue("riverbed-lantern-92");

  const view = render(Onboarding, { store });
  click(byText(view, "button", "This is my first device"));

  expect(create).toHaveBeenCalledOnce();
  await settle(view, "riverbed-lantern-92");
  expect(textOf(view)).toContain("only copy");
});

test('"Continue" acknowledges the displayed code', async () => {
  vi.spyOn(store, "createAccount").mockResolvedValue("riverbed-lantern-92");
  const finish = vi.spyOn(store, "finishOnboarding").mockReturnValue();

  const view = render(Onboarding, { store });
  click(byText(view, "button", "This is my first device"));
  await settle(view, "riverbed-lantern-92");

  click(byText(view, "button", "I've saved the code, continue"));
  expect(finish).toHaveBeenCalledOnce();
});

test("a creation failure shows no code (the store set the banner)", async () => {
  // createAccount returns null on failure: the view stays on the choice.
  vi.spyOn(store, "createAccount").mockResolvedValue(null);

  const view = render(Onboarding, { store });
  click(byText(view, "button", "This is my first device"));
  await Promise.resolve();
  flushSync();

  expect(textOf(view)).not.toContain("recovery code");
  expect(textOf(view)).toContain("This is my first device"); // still on the choice
});

test("joining passes the entered code, cleaned", async () => {
  const join = vi.spyOn(store, "joinAccount").mockResolvedValue(true);

  const view = render(Onboarding, { store });
  click(byText(view, "button", "I already have a device on this account"));
  typeInto(byLabel(view, "Recovery code"), "  riverbed-92  ");
  click(byText(view, "button", "Join"));
  await Promise.resolve();

  expect(join).toHaveBeenCalledWith("riverbed-92");
});

test("an empty code does not call the Core", async () => {
  const join = vi.spyOn(store, "joinAccount").mockResolvedValue(true);

  const view = render(Onboarding, { store });
  click(byText(view, "button", "I already have a device on this account"));
  typeInto(byLabel(view, "Recovery code"), "   ");
  click(byText(view, "button", "Join"));
  await Promise.resolve();

  expect(join).not.toHaveBeenCalled();
});

test("Enter commits the input when the server is ready", async () => {
  const join = vi.spyOn(store, "joinAccount").mockResolvedValue(true);

  const view = render(Onboarding, { store });
  click(byText(view, "button", "I already have a device on this account"));
  typeInto(byLabel(view, "Recovery code"), "riverbed-92");
  press(byLabel(view, "Recovery code"), "Enter");
  await Promise.resolve();

  expect(join).toHaveBeenCalledWith("riverbed-92");
});

// The keyboard must not bypass the button's disarm: if the server drops while
// we're on the input screen, Enter does not call the Core (otherwise a spurious
// error banner over "waiting for the server").
test("Enter respects the server disarm", async () => {
  const join = vi.spyOn(store, "joinAccount").mockResolvedValue(true);

  const view = render(Onboarding, { store });
  // We reach the input screen with the server ready, then the connection drops.
  click(byText(view, "button", "I already have a device on this account"));
  typeInto(byLabel(view, "Recovery code"), "riverbed-92");
  store.session = { logged_in: true, server_connected: false };
  flushSync();

  press(byLabel(view, "Recovery code"), "Enter");
  await Promise.resolve();

  expect(join).not.toHaveBeenCalled();
});

test("server unreachable: the actions are disarmed and signaled", () => {
  store.session = { logged_in: true, server_connected: false };

  const view = render(Onboarding, { store });

  expect(textOf(view)).toContain("Waiting for the server connection");
  expect(byText(view, "button", "This is my first device")).toHaveProperty(
    "disabled",
    true,
  );
});

test("the portal offers a way out through sign-out", () => {
  const logout = vi.spyOn(store, "logout").mockResolvedValue();

  const view = render(Onboarding, { store });
  click(byText(view, "button", "Sign out"));

  expect(logout).toHaveBeenCalledOnce();
});

test("the store's error banner is shown and closable", () => {
  store.notice = { kind: "error", text: "Invalid recovery code." };

  const view = render(Onboarding, { store });
  expect(textOf(view)).toContain("Invalid recovery code.");

  click(byLabel(view, "Close message"));
  expect(view.querySelector(".banner")).toBeNull();
});
