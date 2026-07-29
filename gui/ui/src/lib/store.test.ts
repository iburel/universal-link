// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

// The store against a Core mocked at the IPC level: the same shapes as the
// shell's contract (gui/tests/api/support.rs) and the Core's (doc/core-api.md).
// Nothing is stubbed above `core.ts` — a renamed command or event turns red
// here.

import { emit } from "@tauri-apps/api/event";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import { afterEach, beforeEach, expect, test, vi } from "vitest";

import type {
  AccountKey,
  Component,
  Device,
  PendingRequest,
  SessionState,
} from "./api";
import type { ConnectionStatus, CoreError } from "./core";
import { CoreStore, shareNotice, shareSummary } from "./store.svelte";

// -- Fixtures ---------------------------------------------------------------

const SELF: Device = {
  device_id: "d_self",
  name: "PC-Core",
  platform: "linux",
  online: true,
  last_seen: null,
  is_self: true,
};
const MAC: Device = {
  device_id: "d_mac",
  name: "MacBook",
  platform: "macos",
  online: false,
  last_seen: "2026-07-09T10:00:00Z",
  is_self: false,
};
const WIN: Device = {
  device_id: "d_win",
  name: "Living Room PC",
  platform: "windows",
  online: true,
  last_seen: null,
  is_self: false,
};
const SESSION: SessionState = {
  logged_in: true,
  server_connected: true,
  account: { email: "account@example.test" },
};
const ATTESTED: AccountKey = {
  attested: true,
  fingerprint: "AB12 CD34",
  holds_key: true,
};
const REQUEST: PendingRequest = {
  request_id: "r_1",
  name: "clipnet",
  role: "clipboard-backend",
  scopes: ["clipboard.read", "clipboard.write"],
  peer_info: { pid: 42, exe: "/usr/bin/clipnet" },
};
const COMPONENT: Component = {
  component_id: "c_1",
  name: "clipnet",
  role: "clipboard-backend",
  scopes: ["clipboard.read"],
  connected: true,
};

const CONNECTED: ConnectionStatus = {
  status: "connected",
  granted_scopes: ["session.read", "devices.read", "components.approve"],
  api_version: 1,
};
const CONNECTING: ConnectionStatus = { status: "connecting" };

/** An application error from the Core, as the shell relays it. */
function appError(data_code: string, message = "error"): CoreError {
  return { kind: "rpc", message, code: -32000, data_code };
}

function invalidParams(what: string): CoreError {
  return { kind: "rpc", message: `invalid params: ${what}`, code: -32602 };
}

// -- Harness ----------------------------------------------------------------

type Method = (params: Record<string, unknown>) => unknown;

interface Fake {
  /** The `core_request`s seen, in order. */
  calls: { method: string; params: unknown }[];
  /** The URLs passed to `openUrl` (plugin-opener). */
  opened: string[];
  /** The configs passed to `set_server_config` (the shell's write command). */
  configWrites: unknown[];
  /** All the raw IPC commands, event subscriptions included. */
  ipc: string[];
  /** The share ids released through `discard_share` (mobile only), in order. */
  discards: string[];
  /** The share ids answered through `share_taken` (mobile only), in order. */
  taken: string[];
  /** The keep-alive declarations (mobile only): `kind:on`, in order. */
  holds: string[];
  /** How many scans were asked for (mobile only). */
  scans: number;
  methods: Record<string, Method>;
}

interface MockOptions {
  status?: ConnectionStatus;
  methods?: Record<string, Method>;
  /** Run when the shell is queried for its snapshot. */
  onStatusRead?: () => void;
  /** What the mobile `share_status` snapshot returns (mobile only). */
  retainedShare?: unknown;
  /**
   * What the mobile shell answers about its camera. Undefined means the command
   * is not registered at all — the desktop, where a rejection IS the answer.
   */
  scanner?: boolean;
  /** What the mobile `scan_code` answers, once per call. */
  scanned?: () => unknown;
}

type Internals = {
  __TAURI_INTERNALS__: {
    invoke: (
      cmd: string,
      args?: Record<string, unknown>,
      options?: unknown,
    ) => Promise<unknown>;
  };
};

function mockCore(options: MockOptions): Fake {
  const fake: Fake = {
    calls: [],
    opened: [],
    configWrites: [],
    ipc: [],
    discards: [],
    taken: [],
    holds: [],
    scans: 0,
    methods: {
      "session.status": () => SESSION,
      "account.status": () => ATTESTED,
      "devices.list": () => [SELF, MAC],
      "components.pending": () => [REQUEST],
      "components.list": () => [COMPONENT],
      ...options.methods,
    },
  };
  mockIPC(
    (cmd, payload) => {
      const args = (payload ?? {}) as Record<string, never>;
      if (cmd === "connection_status") {
        options.onStatusRead?.();
        return options.status ?? CONNECTING;
      }
      if (cmd === "plugin:opener|open_url") {
        fake.opened.push(args.url);
        return null;
      }
      if (cmd === "set_server_config") {
        fake.configWrites.push((payload as { config: unknown }).config);
        return null;
      }
      if (cmd === "get_server_config") {
        return { server_url: "", oidc_issuer: "", oidc_client_id: "" };
      }
      // Mobile-only snapshot: no retained share unless a test says otherwise.
      if (cmd === "share_status") {
        return options.retainedShare ?? null;
      }
      // Mobile-only: the release of a shared file's cache copy, and the
      // retirement of a pick that has been answered.
      if (cmd === "discard_share") {
        fake.discards.push(args.id);
        return null;
      }
      if (cmd === "share_taken") {
        fake.taken.push(args.id);
        return null;
      }
      // Mobile-only: the process must outlive work the shell cannot see.
      if (cmd === "keep_alive") {
        fake.holds.push(`${args.kind}:${args.on}`);
        return null;
      }
      // Mobile-only: the camera. Both commands are ABSENT on the desktop shell,
      // which is why the default here is to fall through to the rejection below.
      if (cmd === "scan_supported" && options.scanner !== undefined) {
        return options.scanner;
      }
      if (cmd === "scan_code" && options.scanned) {
        fake.scans += 1;
        return options.scanned();
      }
      if (cmd === "core_request") {
        const { method, params } = payload as {
          method: string;
          params?: Record<string, unknown>;
        };
        fake.calls.push({ method, params });
        const handler = fake.methods[method];
        if (!handler) {
          throw { kind: "rpc", message: `method not found`, code: -32601 };
        }
        return handler(params ?? {});
      }
      throw new Error(`unexpected command: ${cmd}`);
    },
    { shouldMockEvents: true },
  );
  // `shouldMockEvents` intercepts `plugin:event|*` before the callback above:
  // to observe the ORDER of the subscriptions, we must sit lower down.
  const internals = window as unknown as Internals;
  const inner = internals.__TAURI_INTERNALS__.invoke;
  internals.__TAURI_INTERNALS__.invoke = (cmd, args, options) => {
    fake.ipc.push(
      cmd === "plugin:event|listen" ? `listen:${String(args?.event)}` : cmd,
    );
    return inner(cmd, args, options);
  };
  return fake;
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

/** Lets the in-flight microtasks run (the resnapshots are not awaited). */
function flush() {
  return new Promise((resolve) => setTimeout(resolve, 0));
}

const ids = (devices: readonly Device[]) => devices.map((d) => d.device_id);

let store: CoreStore;

beforeEach(() => {
  store = new CoreStore();
});

afterEach(() => {
  store.stop();
  clearMocks();
});

// -- Startup ----------------------------------------------------------------

// The shell's invariant (snapshot updated before the emit) leaves a blind spot
// on the frontend side: an event received DURING the snapshot read is more
// recent than it. The snapshot must then be dropped, not applied.
test("an event received during the snapshot read wins over it", async () => {
  mockCore({
    status: CONNECTING,
    onStatusRead: () => void emit("core:connection", CONNECTED),
  });

  await store.start();

  expect(store.connection).toEqual(CONNECTED);
  await vi.waitFor(() => expect(store.primed).toBe(true));
});

// The order is a race invariant: a `connected` received during the second
// subscription would launch a resnapshot whose concurrent notifications would
// not yet be listened to by anyone.
test("we subscribe to notifications, then to the connection, then read the snapshot", async () => {
  const fake = mockCore({ status: CONNECTING });

  await store.start();

  // Every subscription precedes the snapshot read; `core:share` (mobile only)
  // comes last because nothing about it feeds a resnapshot.
  expect(fake.ipc.slice(0, 4)).toEqual([
    "listen:core:notification",
    "listen:core:connection",
    "listen:core:share",
    "connection_status",
  ]);
});

test("when disconnected, the store does not talk to the Core", async () => {
  const fake = mockCore({ status: CONNECTING });

  await store.start();
  await flush();

  expect(fake.calls).toEqual([]);
  expect(store.primed).toBe(false);
  expect(store.session).toBeNull();
});

test("an incompatible status is terminal: no resnapshot", async () => {
  const fake = mockCore({ status: { status: "incompatible", api_version: 9 } });

  await store.start();
  await flush();

  expect(store.connection).toEqual({ status: "incompatible", api_version: 9 });
  expect(fake.calls).toEqual([]);
});

test("the connection triggers a total resnapshot", async () => {
  const fake = mockCore({ status: CONNECTED });

  await store.start();
  await vi.waitFor(() => expect(store.primed).toBe(true));

  expect(fake.calls.map((c) => c.method).sort()).toEqual([
    "account.status",
    "components.list",
    "components.pending",
    "devices.list",
    "session.status",
  ]);
  expect(store.session).toEqual(SESSION);
  expect(store.account).toEqual(ATTESTED);
  expect(ids(store.devices)).toEqual(["d_self", "d_mac"]);
  expect(store.pending.map((r) => r.request_id)).toEqual(["r_1"]);
  expect(store.components.map((c) => c.component_id)).toEqual(["c_1"]);
  expect(store.devicesError).toBeNull();
});

// `devices.list` responds SERVER_UNREACHABLE as long as the directory has never
// been snapshotted: session closed, or server never reached. The rest of the
// snapshot must survive — without this, a logged-out GUI would show nothing.
test("an unavailable directory does not take away the rest of the snapshot", async () => {
  mockCore({
    status: CONNECTED,
    methods: {
      "session.status": () => ({ logged_in: false, server_connected: false }),
      "devices.list": () => {
        throw appError("SERVER_UNREACHABLE");
      },
    },
  });

  await store.start();
  await vi.waitFor(() => expect(store.primed).toBe(true));

  expect(store.session).toEqual({ logged_in: false, server_connected: false });
  expect(store.devices).toEqual([]);
  expect(store.devicesError).toBe("Server unreachable.");
  expect(store.pending).toHaveLength(1);
  expect(store.notice).toBeNull();
});

// `RequestError::Timeout` leaves the IPC connection alive: no `connected` event
// will come to restart the resnapshot. Without a retry, the screen would stay
// on "Connecting to Core…" with a green status dot.
test("a missed resnapshot is retried as long as the Core stays connected", async () => {
  const retried = deferred<SessionState>();
  let attempts = 0;
  mockCore({
    status: CONNECTED,
    methods: {
      "session.status": () => {
        if (++attempts === 1) throw { kind: "timeout", message: "timeout" };
        return retried.promise; // the 2nd attempt stays in flight: observable state
      },
    },
  });
  store.retryDelayMs = 5;

  await store.start();
  await vi.waitFor(() => expect(attempts).toBe(2));
  expect(store.notice).toEqual({
    kind: "error",
    text: "The Core did not respond in time.",
  });
  expect(store.primed).toBe(false);

  retried.resolve(SESSION);
  await vi.waitFor(() => expect(store.primed).toBe(true));
  // A fresh snapshot expires the error banner from the previous resnapshot.
  expect(store.notice).toBeNull();
});

test("losing the Core during a resnapshot leaves the store blank", async () => {
  mockCore({
    status: CONNECTED,
    methods: {
      "session.status": () => {
        throw { kind: "disconnected", message: "connection lost" };
      },
    },
  });

  await store.start();
  await vi.waitFor(() =>
    expect(store.notice).toEqual({
      kind: "error",
      text: "The connection to the Core was lost.",
    }),
  );
  expect(store.primed).toBe(false);
});

// -- Resnapshot and notifications -------------------------------------------

test("a notification received during a resnapshot is replayed on top of it", async () => {
  const gate = deferred<Device[]>();
  mockCore({ status: CONNECTED, methods: { "devices.list": () => gate.promise } });

  await store.start(); // the resnapshot is in flight, blocked on devices.list
  await emit("core:notification", {
    method: "device.added",
    params: { device: MAC },
  });
  expect(store.devices).toEqual([]); // set aside, not applied

  gate.resolve([SELF]);
  await vi.waitFor(() => expect(store.primed).toBe(true));
  expect(ids(store.devices)).toEqual(["d_self", "d_mac"]);
});

// Two replay invariants in one test: the buffered session.changed restarts a
// resnapshot (`again`), and it does not swallow the notifications that follow
// it in the buffer (accumulation, not short-circuit).
test("a buffered session.changed restarts a resnapshot without swallowing what follows", async () => {
  const first = deferred<Device[]>();
  const second = deferred<Device[]>(); // never resolved: freezes resnapshot #2
  let listCalls = 0;
  mockCore({
    status: CONNECTED,
    methods: {
      "devices.list": () => (++listCalls === 1 ? first.promise : second.promise),
    },
  });

  await store.start(); // resnapshot #1, in flight
  await emit("core:notification", {
    method: "session.changed",
    params: { logged_in: false, server_connected: false },
  });
  await emit("core:notification", {
    method: "device.added",
    params: { device: MAC },
  });

  first.resolve([SELF]);
  await vi.waitFor(() => expect(listCalls).toBe(2)); // the replay restarted it

  expect(ids(store.devices)).toEqual(["d_self", "d_mac"]);
  expect(store.session).toEqual({ logged_in: false, server_connected: false });
});

test("the most recent resnapshot wins, even if it responds first", async () => {
  const first = deferred<Device[]>();
  const second = deferred<Device[]>();
  let call = 0;
  mockCore({
    status: CONNECTED,
    methods: { "devices.list": () => (++call === 1 ? first.promise : second.promise) },
  });

  await store.start(); // resnapshot #1, in flight
  void store.resync(); // resnapshot #2, in flight

  second.resolve([MAC]);
  await vi.waitFor(() => expect(ids(store.devices)).toEqual(["d_mac"]));

  first.resolve([SELF]); // the straggler rewrites nothing
  await flush();
  expect(ids(store.devices)).toEqual(["d_mac"]);
});

test("before the first snapshot, a notification is not applied", async () => {
  mockCore({ status: CONNECTING });
  await store.start();

  await emit("core:notification", {
    method: "device.added",
    params: { device: MAC },
  });

  expect(store.devices).toEqual([]);
});

test("device notifications apply as an idempotent upsert", async () => {
  mockCore({ status: CONNECTED });
  await store.start();
  await vi.waitFor(() => expect(store.primed).toBe(true));

  const renamed = { ...MAC, name: "MacBook Pro" };
  await emit("core:notification", {
    method: "device.updated",
    params: { device: renamed },
  });
  expect(store.devices.find((d) => d.device_id === "d_mac")?.name).toBe(
    "MacBook Pro",
  );

  await emit("core:notification", {
    method: "device.online",
    params: { device: { ...renamed, online: true } },
  });
  expect(ids(store.devices)).toEqual(["d_self", "d_mac"]); // no duplicate

  await emit("core:notification", {
    method: "device.offline",
    params: { device_id: "d_mac", last_seen: "2026-07-10T08:00:00Z" },
  });
  const mac = store.devices.find((d) => d.device_id === "d_mac");
  expect(mac?.online).toBe(false);
  expect(mac?.last_seen).toBe("2026-07-10T08:00:00Z");

  await emit("core:notification", {
    method: "device.removed",
    params: { device_id: "d_mac" },
  });
  expect(ids(store.devices)).toEqual(["d_self"]);
});

// The Core only overwrites `last_seen` if it provides it (core/src/session.rs):
// the store mirrors this guard, otherwise the screen's "last seen 3 h ago" is
// erased.
test("a device.offline without last_seen preserves the one we know", async () => {
  mockCore({ status: CONNECTED });
  await store.start();
  await vi.waitFor(() => expect(store.primed).toBe(true));
  await emit("core:notification", {
    method: "device.online",
    params: { device: { ...MAC, online: true } },
  });

  await emit("core:notification", {
    method: "device.offline",
    params: { device_id: "d_mac" },
  });

  const mac = store.devices.find((d) => d.device_id === "d_mac");
  expect(mac?.online).toBe(false);
  expect(mac?.last_seen).toBe("2026-07-09T10:00:00Z");
});

// The scopes come from a third party's `hello`: the Core validates their
// membership in the known list, never their uniqueness. A duplicate would make
// the view raise `each_key_duplicate` and would be sent back as-is to `approve`.
test("a request's duplicate scopes are deduplicated", async () => {
  const duplicate = {
    ...REQUEST,
    scopes: ["clipboard.read", "clipboard.read", "clipboard.write"],
  };
  mockCore({ status: CONNECTED, methods: { "components.pending": () => [duplicate] } });
  await store.start();
  await vi.waitFor(() => expect(store.primed).toBe(true));

  expect(store.pending[0].scopes).toEqual(["clipboard.read", "clipboard.write"]);

  await emit("core:notification", {
    method: "component.pending",
    params: { ...duplicate, request_id: "r_2" },
  });
  expect(store.pending[1].scopes).toEqual(["clipboard.read", "clipboard.write"]);
});

test("an unknown or malformed notification is ignored without breakage", async () => {
  mockCore({ status: CONNECTED });
  await store.start();
  await vi.waitFor(() => expect(store.primed).toBe(true));

  await emit("core:notification", {
    method: "transfer.progress", // unknown id: no matching `started`
    params: { transfer_id: "t_unknown", done: 1, total: 2 },
  });
  await emit("core:notification", { method: "device.added", params: {} });
  await emit("core:notification", {
    method: "device.offline",
    params: { device_id: "d_unknown" },
  });
  await emit("core:notification", { method: "session.changed", params: null });

  expect(ids(store.devices)).toEqual(["d_self", "d_mac"]);
  expect(store.session).toEqual(SESSION);
});

// The Core does not replay the events missed during a server outage:
// `session.changed` means "the directory must be re-read".
test("session.changed triggers a resnapshot", async () => {
  const fake = mockCore({ status: CONNECTED });
  await store.start();
  await vi.waitFor(() => expect(store.primed).toBe(true));
  const before = fake.calls.length;

  await emit("core:notification", {
    method: "session.changed",
    params: { logged_in: false, server_connected: false },
  });

  expect(store.session).toEqual({ logged_in: false, server_connected: false });
  await vi.waitFor(() => expect(fake.calls.length).toBe(before + 5));
});

test("component.pending adds the request without a duplicate", async () => {
  mockCore({ status: CONNECTED, methods: { "components.pending": () => [] } });
  await store.start();
  await vi.waitFor(() => expect(store.primed).toBe(true));

  await emit("core:notification", {
    method: "component.pending",
    params: REQUEST,
  });
  await emit("core:notification", {
    method: "component.pending",
    params: REQUEST,
  });

  expect(store.pending.map((r) => r.request_id)).toEqual(["r_1"]);
});

test("a connection loss freezes the data without erasing it", async () => {
  mockCore({ status: CONNECTED });
  await store.start();
  await vi.waitFor(() => expect(store.primed).toBe(true));

  await emit("core:connection", CONNECTING);

  expect(store.connection).toEqual(CONNECTING);
  expect(ids(store.devices)).toEqual(["d_self", "d_mac"]);
  expect(store.session).toEqual(SESSION);
});

test("stop() cuts the subscriptions", async () => {
  mockCore({ status: CONNECTED });
  await store.start();
  await vi.waitFor(() => expect(store.primed).toBe(true));

  store.stop();
  await emit("core:notification", {
    method: "device.removed",
    params: { device_id: "d_mac" },
  });

  expect(ids(store.devices)).toEqual(["d_self", "d_mac"]);
});

// -- Actions ----------------------------------------------------------------

test("login opens the URL returned by the Core", async () => {
  const fake = mockCore({
    status: CONNECTED,
    methods: { "session.login": () => ({ auth_url: "https://idp.test/auth" }) },
  });
  await store.start();
  await vi.waitFor(() => expect(store.primed).toBe(true));

  await store.login();

  expect(fake.opened).toEqual(["https://idp.test/auth"]);
  expect(store.notice?.kind).toBe("info");
  expect(store.busy).toBe(false);
  // Mobile: the browser takes the foreground, and the token exchange runs in
  // this process. The hold is declared before the round-trip and left standing —
  // the shell releases it when the session lands (or when it times out).
  expect(fake.holds).toEqual(["auth:true"]);
});

// A login that never reached the browser must not leave the phone holding itself
// awake for the whole timeout.
test("a login that fails on the spot gives the process back", async () => {
  const fake = mockCore({
    status: CONNECTED,
    methods: {
      "session.login": () => {
        throw appError("NOT_CONFIGURED", "no server configured");
      },
    },
  });
  await store.start();
  await vi.waitFor(() => expect(store.primed).toBe(true));

  await store.login();

  expect(fake.opened).toEqual([]);
  expect(fake.holds).toEqual(["auth:true", "auth:false"]);
  expect(store.notice?.kind).toBe("error");
});

test("each action starts from a clean banner", async () => {
  mockCore({ status: CONNECTED, methods: { "session.logout": () => ({}) } });
  await store.start();
  await vi.waitFor(() => expect(store.primed).toBe(true));
  store.notice = { kind: "error", text: "old error" };

  await store.logout();

  expect(store.notice).toBeNull();
});

test("an action started during another is ignored", async () => {
  const gate = deferred<unknown>();
  const fake = mockCore({
    status: CONNECTED,
    methods: {
      "session.logout": () => gate.promise,
      "session.login": () => ({ auth_url: "https://idp.test/auth" }),
    },
  });
  await store.start();
  await vi.waitFor(() => expect(store.primed).toBe(true));

  const inFlight = store.logout(); // busy = true
  await store.login(); // must be ignored

  expect(fake.calls.some((c) => c.method === "session.login")).toBe(false);
  expect(fake.opened).toEqual([]);
  gate.resolve({});
  await inFlight;
  expect(store.busy).toBe(false);
});

test("an application error becomes a readable message", async () => {
  mockCore({
    status: CONNECTED,
    methods: {
      "session.login": () => {
        throw appError("ALREADY_LOGGED_IN", "already logged in");
      },
    },
  });
  await store.start();
  await vi.waitFor(() => expect(store.primed).toBe(true));

  await store.login();

  expect(store.notice).toEqual({
    kind: "error",
    text: "A session is already open on this device.",
  });
});

test("logout and rename write nothing: the state will come from the notification", async () => {
  const fake = mockCore({
    status: CONNECTED,
    methods: { "session.logout": () => ({}), "devices.rename": () => ({}) },
  });
  await store.start();
  await vi.waitFor(() => expect(store.primed).toBe(true));

  await store.logout();
  await store.renameDevice("d_mac", "Living Room Mac");

  expect(store.session).toEqual(SESSION);
  expect(store.devices.find((d) => d.device_id === "d_mac")?.name).toBe(
    "MacBook",
  );
  expect(fake.calls).toContainEqual({
    method: "devices.rename",
    params: { device_id: "d_mac", name: "Living Room Mac" },
  });
});

test("a revocation that requires re-auth opens the browser", async () => {
  const fake = mockCore({
    status: CONNECTED,
    methods: {
      "devices.revoke": () => ({
        status: "reauth_required",
        auth_url: "https://idp.test/reauth",
      }),
    },
  });
  await store.start();
  await vi.waitFor(() => expect(store.primed).toBe(true));

  await store.revokeDevice("d_mac");

  expect(fake.opened).toEqual(["https://idp.test/reauth"]);
  // Same round-trip as a login, so the same hold: the revocation is carried out
  // by a callback that comes back into THIS process, while the browser has the
  // foreground (mobile only — see keepAlive).
  expect(fake.holds).toEqual(["auth:true"]);
  expect(store.notice?.kind).toBe("info");
  expect(ids(store.devices)).toEqual(["d_self", "d_mac"]); // device.removed will be authoritative
});

test("a successful revocation opens nothing and writes nothing", async () => {
  const fake = mockCore({
    status: CONNECTED,
    methods: { "devices.revoke": () => ({ status: "done" }) },
  });
  await store.start();
  await vi.waitFor(() => expect(store.primed).toBe(true));

  await store.revokeDevice("d_mac");

  expect(fake.opened).toEqual([]);
  expect(store.notice).toBeNull();
  expect(ids(store.devices)).toEqual(["d_self", "d_mac"]);
});

// `components.*` notifies no queue exit: without a resnapshot, the approved
// request would stay displayed forever.
test("approving resnapshots, for lack of a queue-exit notification", async () => {
  let pending: PendingRequest[] = [REQUEST];
  const fake = mockCore({
    status: CONNECTED,
    methods: {
      "components.pending": () => pending,
      "components.approve": () => {
        pending = [];
        return {};
      },
    },
  });
  await store.start();
  await vi.waitFor(() => expect(store.pending).toHaveLength(1));

  await store.approve("r_1", ["clipboard.read"]);

  expect(fake.calls).toContainEqual({
    method: "components.approve",
    params: { request_id: "r_1", scopes: ["clipboard.read"] },
  });
  expect(store.pending).toEqual([]);
});

// The request is displayed BEFORE the decision and the Core says it's gone:
// without a resnapshot, it would stay on screen forever.
test("approving a vanished request says so and resnapshots", async () => {
  let pending: PendingRequest[] = [REQUEST];
  mockCore({
    status: CONNECTED,
    methods: {
      "components.pending": () => pending,
      "components.approve": () => {
        pending = []; // the Core had already removed it from its queue
        throw invalidParams("request_id");
      },
    },
  });
  await store.start();
  await vi.waitFor(() => expect(store.pending).toHaveLength(1));

  await store.approve("r_1", ["clipboard.read"]);

  expect(store.notice).toEqual({
    kind: "error",
    text: "This request no longer exists.",
  });
  expect(store.pending).toEqual([]);
});

test("denying and revoking a component also resnapshot", async () => {
  const fake = mockCore({
    status: CONNECTED,
    methods: { "components.deny": () => ({}), "components.revoke": () => ({}) },
  });
  await store.start();
  await vi.waitFor(() => expect(store.primed).toBe(true));
  const before = fake.calls.length;

  await store.deny("r_1");
  await store.revokeComponent("c_1");

  const methods = fake.calls.slice(before).map((c) => c.method);
  expect(methods.filter((m) => m === "components.list")).toHaveLength(2);
});

test("an action while disconnected surfaces the shell's message", async () => {
  mockCore({
    status: CONNECTED,
    methods: {
      "session.logout": () => {
        throw { kind: "not_connected", message: "no connection to the Core" };
      },
    },
  });
  await store.start();
  await vi.waitFor(() => expect(store.primed).toBe(true));

  await store.logout();

  expect(store.notice).toEqual({
    kind: "error",
    text: "The Core is not reachable.",
  });
  expect(store.busy).toBe(false);
});

// -- Transfers --------------------------------------------------------------

/** A primed, connected store, ready to receive notifications. */
async function primed(
  methods?: Record<string, Method>,
  over: Omit<MockOptions, "status" | "methods"> = {},
) {
  const fake = mockCore({ status: CONNECTED, methods, ...over });
  await store.start();
  await vi.waitFor(() => expect(store.primed).toBe(true));
  return fake;
}

test("an outgoing send runs its course through the transfer.* notifications", async () => {
  await primed();

  await emit("core:notification", {
    method: "transfer.started",
    params: {
      transfer_id: "t_1",
      device_id: "d_win",
      files: [{ name: "a.pdf", size: 100 }],
      total: 100,
    },
  });
  let t = store.transfers[0];
  expect(t).toMatchObject({ transfer_id: "t_1", device_id: "d_win", status: "active", done: 0 });

  await emit("core:notification", {
    method: "transfer.progress",
    params: { transfer_id: "t_1", done: 40, total: 100 },
  });
  expect(store.transfers[0].done).toBe(40);

  await emit("core:notification", {
    method: "transfer.finished",
    params: { transfer_id: "t_1" },
  });
  t = store.transfers[0];
  expect(t.status).toBe("finished");
  expect(t.done).toBe(t.total); // the bar fills up at the end
});

test("a send that fails carries the error, a cancellation is 'cancelled'", async () => {
  await primed();
  const start = (id: string) =>
    emit("core:notification", {
      method: "transfer.started",
      params: { transfer_id: id, device_id: "d_win", files: [], total: 0 },
    });

  await start("t_1");
  await emit("core:notification", {
    method: "transfer.failed",
    params: { transfer_id: "t_1", error: "connection lost" },
  });
  await start("t_2");
  await emit("core:notification", {
    method: "transfer.failed",
    params: { transfer_id: "t_2", error: "cancelled" },
  });

  expect(store.transfers.map((t) => [t.status, t.error])).toEqual([
    ["failed", "connection lost"],
    ["failed", "cancelled"],
  ]);
});

// The GUI only shows the outgoing direction; incoming receipts land silently in
// the download folder (T2 contract).
test("incoming transfers are not shown", async () => {
  await primed();

  await emit("core:notification", {
    method: "transfer.incoming",
    params: {
      transfer_id: "t_in",
      device_id: "d_win",
      files: [{ name: "received.txt", size: 5 }],
    },
  });

  expect(store.transfers).toEqual([]);
});

test("a progress with no matching send creates no transfer", async () => {
  await primed();

  await emit("core:notification", {
    method: "transfer.progress",
    params: { transfer_id: "t_ghost", done: 1, total: 2 },
  });
  await emit("core:notification", {
    method: "transfer.finished",
    params: { transfer_id: "t_ghost" },
  });

  expect(store.transfers).toEqual([]);
});

test("sendFiles passes target and paths to the Core, without writing state", async () => {
  const fake = await primed({ "files.send": () => ({ transfer_id: "t_1" }) });

  await store.sendFiles("d_win", ["/home/u/a.pdf", "/home/u/b.png"]);

  expect(fake.calls).toContainEqual({
    method: "files.send",
    params: { device_id: "d_win", paths: ["/home/u/a.pdf", "/home/u/b.png"] },
  });
  // The tracking state comes only from transfer.started, not from the response.
  expect(store.transfers).toEqual([]);
  // Mobile: the phone must survive being pocketed between the tap and the Core's
  // answer, after which the transfer itself is what the shell follows.
  expect(fake.holds).toEqual(["send:true", "send:false"]);
});

test("sendFiles does not call the Core for an empty list", async () => {
  const fake = await primed();

  await store.sendFiles("d_win", []);

  expect(fake.calls.some((c) => c.method === "files.send")).toBe(false);
  // Nothing was asked of the Core, so nothing had to be held up either.
  expect(fake.holds).toEqual([]);
});

test("a send to an offline device is explained", async () => {
  await primed({
    "files.send": () => {
      throw appError("DEVICE_OFFLINE");
    },
  });

  await store.sendFiles("d_mac", ["/home/u/a.pdf"]);

  expect(store.notice).toEqual({
    kind: "error",
    text: "This device is offline.",
  });
});

// A dropped folder: the Core responds -32602 with a meaningful message, which
// we relay as-is (humanize falls back to the message for a -32602 with no code).
test("a folder rejected by the Core surfaces its message", async () => {
  await primed({
    "files.send": () => {
      throw invalidParams("folders are not supported");
    },
  });

  await store.sendFiles("d_win", ["/home/u/folder"]);

  expect(store.notice?.kind).toBe("error");
  expect(store.notice?.text).toContain("folders are not supported");
});

test("cancelling a transfer calls the Core", async () => {
  const fake = await primed({ "files.cancel": () => ({}) });

  await store.cancelTransfer("t_1");

  expect(fake.calls).toContainEqual({
    method: "files.cancel",
    params: { transfer_id: "t_1" },
  });
});

// The target may have finished between the button's display and the click: the
// Core responds TRANSFER_UNKNOWN, and that's not an error to show.
test("cancelling an already-finished transfer is silent", async () => {
  await primed({
    "files.cancel": () => {
      throw appError("TRANSFER_UNKNOWN");
    },
  });
  store.notice = null;

  await store.cancelTransfer("t_1");

  expect(store.notice).toBeNull();
});

test("cancelling surfaces any other error", async () => {
  await primed({
    "files.cancel": () => {
      throw { kind: "not_connected", message: "no connection to the Core" };
    },
  });

  await store.cancelTransfer("t_1");

  expect(store.notice).toEqual({
    kind: "error",
    text: "The Core is not reachable.",
  });
});

test("dismissing removes a finished transfer from the list", async () => {
  await primed();
  await emit("core:notification", {
    method: "transfer.started",
    params: { transfer_id: "t_1", device_id: "d_win", files: [], total: 0 },
  });
  await emit("core:notification", {
    method: "transfer.finished",
    params: { transfer_id: "t_1" },
  });

  store.dismissTransfer("t_1");

  expect(store.transfers).toEqual([]);
});

// A resnapshot only rewrites session/directory/components: a send in progress
// must not disappear from the screen because the server moved.
test("transfers survive a resnapshot", async () => {
  await primed();
  await emit("core:notification", {
    method: "transfer.started",
    params: { transfer_id: "t_1", device_id: "d_win", files: [], total: 0 },
  });

  await emit("core:notification", {
    method: "session.changed",
    params: { logged_in: false, server_connected: false },
  });
  await vi.waitFor(() => expect(store.session?.logged_in).toBe(false));

  expect(store.transfers.map((t) => t.transfer_id)).toEqual(["t_1"]);
});

test("the history of finished transfers is bounded", async () => {
  await primed();
  store.transferHistory = 2;

  for (const id of ["t_1", "t_2", "t_3"]) {
    await emit("core:notification", {
      method: "transfer.started",
      params: { transfer_id: id, device_id: "d_win", files: [], total: 0 },
    });
    await emit("core:notification", {
      method: "transfer.finished",
      params: { transfer_id: id },
    });
  }

  // The oldest terminal one is evicted; the last two remain.
  expect(store.transfers.map((t) => t.transfer_id)).toEqual(["t_2", "t_3"]);
});

test("targetFor only accepts an online device other than this PC", () => {
  store.devices = [SELF, MAC, WIN];

  expect(store.targetFor("d_win")).toBe("d_win"); // online, not self
  expect(store.targetFor("d_self")).toBeNull(); // this PC
  expect(store.targetFor("d_mac")).toBeNull(); // offline
  expect(store.targetFor("d_absent")).toBeNull(); // unknown
  expect(store.targetFor(null)).toBeNull();
});

// Sends and cancellations are OUTSIDE the `busy` lock: legitimately concurrent,
// and they must not disarm the management buttons (whose `disabled` derives from it).
test("a send is not subject to the busy lock", async () => {
  const fake = await primed({ "files.send": () => ({ transfer_id: "t_1" }) });
  store.busy = true; // a management action is "in flight"

  await store.sendFiles("d_win", ["/a"]);

  // A sendFiles wrapped in #act would early-return here: no call.
  expect(fake.calls.some((c) => c.method === "files.send")).toBe(true);
  expect(store.busy).toBe(true); // sendFiles never touches busy
});

test("a cancellation is not subject to the busy lock", async () => {
  const fake = await primed({ "files.cancel": () => ({}) });
  store.busy = true;

  await store.cancelTransfer("t_1");

  expect(fake.calls.some((c) => c.method === "files.cancel")).toBe(true);
  expect(store.busy).toBe(true);
});

test("two concurrent sends both reach the Core", async () => {
  const fake = await primed({ "files.send": () => ({ transfer_id: "t" }) });

  await Promise.all([
    store.sendFiles("d_win", ["/a"]),
    store.sendFiles("d_mac", ["/b"]),
  ]);

  expect(fake.calls.filter((c) => c.method === "files.send")).toHaveLength(2);
});

test("a successful send clears an earlier error", async () => {
  await primed({ "files.send": () => ({ transfer_id: "t_1" }) });
  store.notice = { kind: "error", text: "old faulty drop" };

  await store.sendFiles("d_win", ["/a"]);

  expect(store.notice).toBeNull();
});

// Transfers are a stream WITHOUT a recovery snapshot: buffered then dropped (a
// missed resync), they would be lost. They must be applied right away.
test("a transfer received during a resync is applied right away, not buffered", async () => {
  const gate = deferred<Device[]>();
  mockCore({ status: CONNECTED, methods: { "devices.list": () => gate.promise } });

  await store.start(); // resync in flight, blocked on devices.list (buffer active)

  await emit("core:notification", {
    method: "transfer.started",
    params: { transfer_id: "t_1", device_id: "d_win", files: [], total: 10 },
  });
  await emit("core:notification", {
    method: "transfer.progress",
    params: { transfer_id: "t_1", done: 10, total: 10 },
  });
  // A concurrent device.* is, itself, buffered (not yet applied)…
  await emit("core:notification", { method: "device.added", params: { device: WIN } });
  expect(store.devices).toEqual([]);
  // …but the transfer is already there: no snapshot to catch it up.
  expect(store.transfers.map((t) => [t.transfer_id, t.done])).toEqual([["t_1", 10]]);

  gate.resolve([SELF]);
  await vi.waitFor(() => expect(store.primed).toBe(true));
  expect(store.transfers[0].done).toBe(10); // survives the resync
});

test("a transfer finished during a failing resync is not lost", async () => {
  const gate = deferred<SessionState>();
  let attempts = 0;
  mockCore({
    status: CONNECTED,
    methods: { "session.status": () => (++attempts === 1 ? SESSION : gate.promise) },
  });
  store.retryDelayMs = 1000; // no retry during the test
  await store.start();
  await vi.waitFor(() => expect(store.primed).toBe(true));

  await emit("core:notification", {
    method: "transfer.started",
    params: { transfer_id: "t_1", device_id: "d_win", files: [], total: 0 },
  });
  await emit("core:notification", { method: "session.changed", params: SESSION }); // resync 2, will fail
  await emit("core:notification", {
    method: "transfer.finished",
    params: { transfer_id: "t_1" },
  });
  expect(store.transfers[0].status).toBe("finished"); // applied, not buffered

  gate.reject({ kind: "timeout", message: "timeout" });
  await vi.waitFor(() => expect(store.notice?.kind).toBe("error"));
  expect(store.transfers[0].status).toBe("finished"); // survives the dropped buffer
});

test("a replayed transfer.started downgrades neither progress nor status", async () => {
  await primed();
  const started = {
    method: "transfer.started",
    params: {
      transfer_id: "t_1",
      device_id: "d_win",
      files: [{ name: "a", size: 10 }],
      total: 10,
    },
  };
  await emit("core:notification", started);
  await emit("core:notification", {
    method: "transfer.progress",
    params: { transfer_id: "t_1", done: 6, total: 10 },
  });
  await emit("core:notification", started); // replay of the same started

  expect(store.transfers).toHaveLength(1); // no duplicate
  expect(store.transfers[0].done).toBe(6); // progress preserved
  expect(store.transfers[0].status).toBe("active");
});

// -- Account (C7 account key / onboarding) ----------------------------------

const UNATTESTED: AccountKey = { attested: false, fingerprint: null, holds_key: false };

test("resync reads the account key state", async () => {
  await primed({ "account.status": () => UNATTESTED });
  expect(store.account).toEqual(UNATTESTED);
});

// A Core older than C7 does not know account.status: we fail OPEN (account null
// → no portal) rather than block on a missing capability.
test("a Core without account.status leaves the account null", async () => {
  await primed({
    "account.status": () => {
      throw { kind: "rpc", message: "method not found", code: -32601 };
    },
  });
  expect(store.account).toBeNull();
});

test("createAccount returns the code, holds the portal, without a resnapshot or state write", async () => {
  const fake = await primed({
    "account.status": () => UNATTESTED,
    "account.setup": () => ({ recovery_code: "riverbed-92", fingerprint: "AB12" }),
  });
  const before = fake.calls.length;

  const code = await store.createAccount();

  expect(code).toBe("riverbed-92");
  expect(store.onboardingPending).toBe(true);
  // No resnapshot: the attestation stays the snapshot's, otherwise the portal
  // would lift, taking away the displayed code.
  expect(store.account).toEqual(UNATTESTED);
  expect(fake.calls.slice(before).map((c) => c.method)).toEqual([
    "account.setup",
  ]);
});

// The flag must be armed AS SOON AS the call happens, not on account.setup's
// return: during the round-trip, a background resync reading the attestation
// already set on the Core side would otherwise lift the portal before the code
// is even returned.
test("onboardingPending is armed before account.setup returns", async () => {
  const gate = deferred<void>();
  let attested = false;
  await primed({
    "account.status": () => (attested ? ATTESTED : UNATTESTED),
    "account.setup": async () => {
      await gate.promise; // setup "in flight"
      return { recovery_code: "riverbed-92", fingerprint: "AB12" };
    },
  });

  const createPromise = store.createAccount();
  await Promise.resolve();
  expect(store.onboardingPending).toBe(true); // armed during the flight

  // The Core has attested in the meantime; a background resync reads it…
  attested = true;
  await emit("core:notification", {
    method: "session.changed",
    params: { logged_in: true, server_connected: true },
  });
  await vi.waitFor(() => expect(store.account).toEqual(ATTESTED));
  expect(store.onboardingPending).toBe(true); // …the portal held

  gate.resolve();
  expect(await createPromise).toBe("riverbed-92");
  expect(store.onboardingPending).toBe(true);
});

test("createAccount on failure sets a banner and does not hold the portal", async () => {
  await primed({
    "account.status": () => UNATTESTED,
    "account.setup": () => {
      throw appError("SERVER_UNREACHABLE");
    },
  });

  const code = await store.createAccount();

  expect(code).toBeNull();
  expect(store.onboardingPending).toBe(false);
  expect(store.notice).toEqual({ kind: "error", text: "Server unreachable." });
});

test("createAccount is ignored if an action is already in flight", async () => {
  const fake = await primed({ "account.status": () => UNATTESTED });
  store.busy = true;

  const code = await store.createAccount();

  expect(code).toBeNull();
  expect(fake.calls.some((c) => c.method === "account.setup")).toBe(false);
});

test("a successful joinAccount resnapshots and returns true", async () => {
  let attested = false;
  const fake = await primed({
    "account.status": () => (attested ? ATTESTED : UNATTESTED),
    "account.join": () => {
      // The Core has installed the root: the next snapshot will see it.
      attested = true;
      return { fingerprint: "AB12 CD34" };
    },
  });

  const ok = await store.joinAccount("riverbed-92");

  expect(ok).toBe(true);
  expect(fake.calls).toContainEqual({
    method: "account.join",
    params: { recovery_code: "riverbed-92" },
  });
  // The resnapshot re-read the attestation: the portal can lift.
  await vi.waitFor(() => expect(store.account).toEqual(ATTESTED));
});

test("joinAccount with an invalid code explains without resnapshotting", async () => {
  const fake = await primed({
    "account.status": () => UNATTESTED,
    "account.join": () => {
      throw appError("INVALID_CODE");
    },
  });
  const before = fake.calls.length;

  const ok = await store.joinAccount("wrong");

  expect(ok).toBe(false);
  expect(store.notice).toEqual({
    kind: "error",
    text: "Invalid recovery code.",
  });
  // No resnapshot after a failed join.
  expect(fake.calls.slice(before).map((c) => c.method)).toEqual([
    "account.join",
  ]);
});

test("finishOnboarding lifts the flag and resnapshots", async () => {
  const fake = await primed({ "account.status": () => ATTESTED });
  store.onboardingPending = true;
  const before = fake.calls.length;

  store.finishOnboarding();

  expect(store.onboardingPending).toBe(false);
  await vi.waitFor(() =>
    expect(
      fake.calls.slice(before).some((c) => c.method === "account.status"),
    ).toBe(true),
  );
});

// The central trap: a BACKGROUND resnapshot (triggered by a notification) while
// the code is displayed must NOT disarm the portal, even when it re-reads an
// attestation that is now true — otherwise the portal would lift, taking away
// the irrecoverable code.
test("a background resnapshot while the code is displayed does not disarm the portal", async () => {
  let attested = false;
  await primed({
    "account.status": () => (attested ? ATTESTED : UNATTESTED),
    "account.setup": () => {
      attested = true; // the Core attests; a background resync will read it
      return { recovery_code: "riverbed-92", fingerprint: "AB12" };
    },
  });

  const code = await store.createAccount();
  expect(code).toBe("riverbed-92");
  expect(store.onboardingPending).toBe(true);

  await emit("core:notification", {
    method: "session.changed",
    params: { logged_in: true, server_connected: true },
  });
  await vi.waitFor(() => expect(store.account).toEqual(ATTESTED));

  expect(store.onboardingPending).toBe(true); // it HELD
});

// The other edge of the race: a closed session (expiry, remote revocation)
// while the code is displayed must DISARM the flag — otherwise it would stay
// stuck and reopen the portal on reconnection, on a device that is nonetheless
// already attested, with no way out.
test("a closed session during onboarding disarms the portal", async () => {
  let loggedIn = true;
  let attested = false;
  await primed({
    "session.status": () =>
      loggedIn ? SESSION : { logged_in: false, server_connected: false },
    "account.status": () => (attested ? ATTESTED : UNATTESTED),
    "account.setup": () => {
      attested = true;
      return { recovery_code: "riverbed-92", fingerprint: "AB12" };
    },
  });

  await store.createAccount();
  expect(store.onboardingPending).toBe(true);

  loggedIn = false;
  await emit("core:notification", {
    method: "session.changed",
    params: { logged_in: false, server_connected: false },
  });
  await vi.waitFor(() => expect(store.session?.logged_in).toBe(false));

  expect(store.onboardingPending).toBe(false); // disarmed, no re-locking
});

// The recovery code is shown once and never again, and it is the way back into
// an account whose every device is lost: it must live only in the view's LOCAL
// state, never in the store.
test("createAccount keeps the code in no store state", async () => {
  await primed({
    "account.status": () => UNATTESTED,
    "account.setup": () => ({
      recovery_code: "riverbed-secret-92",
      fingerprint: "AB12",
    }),
  });

  const code = await store.createAccount();
  expect(code).toBe("riverbed-secret-92");

  const dump = JSON.stringify({
    account: store.account,
    session: store.session,
    notice: store.notice,
    devices: store.devices,
    components: store.components,
    pending: store.pending,
    transfers: store.transfers,
  });
  expect(dump).not.toContain("riverbed-secret-92");
});

// -- Server configuration ---------------------------------------------------

test("saveServerConfig writes the config, reloads the Core, then resyncs", async () => {
  const CONFIGURED: SessionState = {
    logged_in: false,
    server_connected: false,
    configured: true,
  };
  const fake = mockCore({
    status: CONNECTED,
    methods: {
      "session.status": () => CONFIGURED,
      "session.reload": () => CONFIGURED,
    },
  });
  await store.start();
  await flush();

  const ok = await store.saveServerConfig({
    server_url: "wss://relay.example/ws",
    oidc_issuer: "https://idp.example",
    oidc_client_id: "public-id",
    oidc_client_secret: null,
  });

  expect(ok).toBe(true);
  // The shell wrote config.json (the GUI is the sole writer)...
  expect(fake.configWrites).toEqual([
    {
      server_url: "wss://relay.example/ws",
      oidc_issuer: "https://idp.example",
      oidc_client_id: "public-id",
      oidc_client_secret: null,
    },
  ]);
  // ...the Core re-read it via session.reload...
  expect(fake.calls.some((c) => c.method === "session.reload")).toBe(true);
  // ...and the resync picked up the now-configured state.
  expect(store.session?.configured).toBe(true);
});

test("saveServerConfig surfaces a config the Core rejects", async () => {
  const fake = mockCore({
    status: CONNECTED,
    methods: {
      "session.reload": () => {
        throw appError(
          "INVALID_CONFIG",
          "server_url must start with ws:// or wss://",
        );
      },
    },
  });
  await store.start();
  await flush();

  const ok = await store.saveServerConfig({
    server_url: "https://relay.example/ws",
    oidc_issuer: "https://idp.example",
    oidc_client_id: "id",
    oidc_client_secret: null,
  });

  expect(ok).toBe(false);
  // The write still happened; it is the Core that refused on reload.
  expect(fake.configWrites).toHaveLength(1);
  expect(store.notice?.kind).toBe("error");
  // The Core's specific reason is shown (INVALID_CONFIG is not remapped).
  expect(store.notice?.text).toContain("ws://");
});

// -- Android share sheet (`core:share`, mobile only) -------------------------

// The user has already left the app that shared the text: the banner is the one
// place they can learn what became of it, so every outcome has to be legible —
// and "it reached nobody" must not read like a success.
test.each([
  [{ phase: "sending" } as const, "info", "Sharing…"],
  [
    { phase: "sending", targets: 2 } as const,
    "info",
    "Sharing with 2 devices…",
  ],
  [
    { phase: "done", delivered: 1, failed: 0 } as const,
    "info",
    "Shared with 1 device.",
  ],
  [
    { phase: "done", delivered: 2, failed: 1 } as const,
    "info",
    "Shared with 2 of 3 devices.",
  ],
  [
    { phase: "done", delivered: 0, failed: 1 } as const,
    "error",
    "Could not reach your other device.",
  ],
  [
    { phase: "done", delivered: 0, failed: 3 } as const,
    "error",
    "Could not reach any of your 3 devices.",
  ],
  [
    { phase: "error", reason: "no_devices" } as const,
    "error",
    "No other device on your account — nothing was shared.",
  ],
  [
    { phase: "error", reason: "not_signed_in" } as const,
    "error",
    "Sign in to UniversalLink first — nothing was shared.",
  ],
  [
    { phase: "error", reason: "offline" } as const,
    "error",
    "Could not reach your account — nothing was shared.",
  ],
  [
    { phase: "error", reason: "too_large" } as const,
    "error",
    "That text is too large to share (8 MiB maximum).",
  ],
  [
    { phase: "error", reason: "unconfirmed" } as const,
    "error",
    "Shared, but your devices did not confirm receiving it.",
  ],
  [
    { phase: "error", reason: "refused", detail: "no connection" } as const,
    "error",
    "Sharing failed: no connection",
  ],
  [
    { phase: "error", reason: "refused" } as const,
    "error",
    "Sharing failed.",
  ],
  [
    { phase: "preparing", files: 1 } as const,
    "info",
    "Preparing the shared file…",
  ],
  [
    { phase: "preparing", files: 3 } as const,
    "info",
    "Preparing 3 shared files…",
  ],
  [
    { phase: "error", reason: "unreadable" } as const,
    "error",
    "Could not read what was shared — nothing was sent.",
  ],
  [
    { phase: "error", reason: "no_space" } as const,
    "error",
    "Not enough free space on this phone to prepare that share.",
  ],
])("a share status reads as a banner: %o", (status, kind, text) => {
  // Sticky: a share reports itself while the user may not have touched the app,
  // so a resnapshot in between must not expire its feedback.
  expect(shareNotice(status)).toEqual({ kind, text, sticky: true });
});

test("a share reports itself in the banner", async () => {
  mockCore({ status: CONNECTED });
  await store.start();
  await vi.waitFor(() => expect(store.primed).toBe(true));

  await emit("core:share", { phase: "sending", targets: 1 });
  await vi.waitFor(() =>
    expect(store.notice).toEqual({
      kind: "info",
      text: "Sharing with 1 device…",
      sticky: true,
    }),
  );

  await emit("core:share", { phase: "done", delivered: 1, failed: 0 });
  await vi.waitFor(() =>
    expect(store.notice).toEqual({
      kind: "info",
      text: "Shared with 1 device.",
      sticky: true,
    }),
  );
});

// A share is usually what COLD-STARTED the app: its status is published before
// the webview exists, and a Tauri event only reaches listeners already
// registered. Without the snapshot the user would never learn the outcome of the
// very share they made.
test("a share that finished before the UI existed is read from the snapshot", async () => {
  mockCore({
    status: CONNECTED,
    retainedShare: { phase: "done", delivered: 2, failed: 0 },
  });

  await store.start();

  expect(store.notice).toEqual({
    kind: "info",
    text: "Shared with 2 devices.",
    sticky: true,
  });
  // And the priming resnapshot, which runs right after, does not erase it.
  await vi.waitFor(() => expect(store.primed).toBe(true));
  expect(store.notice?.text).toBe("Shared with 2 devices.");
});

// The live event is more recent than the snapshot by construction.
test("a share event received during the snapshot read wins over the snapshot", async () => {
  mockCore({
    status: CONNECTED,
    retainedShare: { phase: "sending" },
    onStatusRead: () =>
      void emit("core:share", { phase: "done", delivered: 1, failed: 0 }),
  });

  await store.start();

  await vi.waitFor(() =>
    expect(store.notice?.text).toBe("Shared with 1 device."),
  );
});

test("a non-sticky notice is still expired by a resnapshot", async () => {
  mockCore({ status: CONNECTED });
  await store.start();
  await vi.waitFor(() => expect(store.primed).toBe(true));

  store.notice = { kind: "error", text: "an action failed" };
  await store.resync();

  expect(store.notice).toBeNull();
});

// -- Android share sheet: files ---------------------------------------------
//
// A shared FILE goes to ONE device, so the share stops half-way and waits for
// the user. What is at stake beyond the send: the copy Kotlin made in the app's
// cache (ShareFiles.kt) must be released on EVERY exit — sent, cancelled or
// superseded — because nothing else will ever come back for it.

const PICK = {
  phase: "pick",
  id: "s_1",
  files: [
    { path: "/data/cache/shares/s_1/holiday.jpg", name: "holiday.jpg", size: 2516582 },
  ],
} as const;

test("shared files wait for a destination instead of a banner", async () => {
  await primed();
  store.notice = { kind: "info", text: "Preparing the shared file…", sticky: true };

  await emit("core:share", PICK);

  await vi.waitFor(() => expect(store.pendingShare).toEqual(PICK));
  // The pick IS the message: the "Preparing…" banner it follows steps aside.
  expect(store.notice).toBeNull();
});

// The usual case, not a corner one: the share is what started the app, so the
// pick was published before this webview existed.
test("a pick made before the UI existed is read from the snapshot", async () => {
  mockCore({ status: CONNECTED, retainedShare: PICK });

  await store.start();

  expect(store.pendingShare).toEqual(PICK);
  // And the priming resnapshot does not take the question away.
  await vi.waitFor(() => expect(store.primed).toBe(true));
  expect(store.pendingShare).toEqual(PICK);
});

test("a new share supersedes the one still waiting, releasing its copy", async () => {
  const fake = await primed();
  await emit("core:share", PICK);
  await vi.waitFor(() => expect(store.pendingShare).not.toBeNull());

  // A second share, from the moment its bytes start being copied.
  await emit("core:share", { phase: "preparing", files: 1 });

  await vi.waitFor(() => expect(store.pendingShare).toBeNull());
  expect(fake.discards).toEqual(["s_1"]);
  expect(store.notice?.text).toBe("Preparing the shared file…");
});

test("a second pick replaces the first and releases only the first", async () => {
  const fake = await primed();
  await emit("core:share", PICK);
  await vi.waitFor(() => expect(store.pendingShare).not.toBeNull());

  const second = { ...PICK, id: "s_2" };
  await emit("core:share", second);

  await vi.waitFor(() => expect(store.pendingShare).toEqual(second));
  expect(fake.discards).toEqual(["s_1"]);
});

test("sending the share sends its paths to the chosen device, once", async () => {
  const fake = await primed({ "files.send": () => ({ transfer_id: "t_1" }) });
  await emit("core:share", PICK);
  await vi.waitFor(() => expect(store.pendingShare).not.toBeNull());

  await store.sendShare("d_win");
  // A second tap on another device: the share is already spent.
  await store.sendShare("d_mac");

  expect(fake.calls.filter((c) => c.method === "files.send")).toEqual([
    {
      method: "files.send",
      params: { device_id: "d_win", paths: [PICK.files[0].path] },
    },
  ]);
  expect(store.pendingShare).toBeNull();
  // Answered, so a later share cannot mistake it for one still waiting — but not
  // released: the Core is reading those files as it sends them.
  expect(fake.taken).toEqual(["s_1"]);
  expect(fake.discards).toEqual([]);
});

// The bytes of a transfer in flight are not the previous share's leftovers. The
// two look alike from the Rust side — a retained `pick` — which is why answering
// one retires it (`share_taken`).
test("a share arriving during a transfer does not take its files away", async () => {
  const fake = await primed({ "files.send": () => ({ transfer_id: "t_1" }) });
  await emit("core:share", PICK);
  await vi.waitFor(() => expect(store.pendingShare).not.toBeNull());
  await store.sendShare("d_win");

  // Another share, while t_1 is still reading /…/shares/s_1/holiday.jpg.
  await emit("core:share", { phase: "preparing", files: 1 });
  await flush();

  expect(fake.discards).toEqual([]);
});

test.each([
  ["transfer.finished", { transfer_id: "t_1" }],
  ["transfer.failed", { transfer_id: "t_1", error: "cancelled" }],
])("a share's copy is released when its transfer ends (%s)", async (method, params) => {
  const fake = await primed({ "files.send": () => ({ transfer_id: "t_1" }) });
  await emit("core:share", PICK);
  await vi.waitFor(() => expect(store.pendingShare).not.toBeNull());
  await store.sendShare("d_win");

  await emit("core:notification", { method, params });

  await vi.waitFor(() => expect(fake.discards).toEqual(["s_1"]));
});

// The transfer's notifications ride the same queue as the response that named
// it: it can be over BEFORE `files.send` returns, in which case the
// `finished` that would have released the copy arrived while the transfer had
// no share attached to it yet — and no second one will come.
test("a transfer that ended before its response still releases the copy", async () => {
  const fake = await primed({
    "files.send": async () => {
      await emit("core:notification", {
        method: "transfer.started",
        params: { transfer_id: "t_1", device_id: "d_win", files: [], total: 1 },
      });
      await emit("core:notification", {
        method: "transfer.finished",
        params: { transfer_id: "t_1" },
      });
      return { transfer_id: "t_1" };
    },
  });
  await emit("core:share", PICK);
  await vi.waitFor(() => expect(store.pendingShare).not.toBeNull());

  await store.sendShare("d_win");

  // Released without waiting for anything else: the notification is long gone.
  expect(store.transfers[0].status).toBe("finished");
  expect(fake.discards).toEqual(["s_1"]);
});

test("a refused send releases the copy right away and says why", async () => {
  const fake = await primed({
    "files.send": () => {
      throw appError("DEVICE_OFFLINE");
    },
  });
  await emit("core:share", PICK);
  await vi.waitFor(() => expect(store.pendingShare).not.toBeNull());

  await store.sendShare("d_win");

  expect(fake.discards).toEqual(["s_1"]);
  expect(store.notice).toEqual({ kind: "error", text: "This device is offline." });
});

test("cancelling the share releases the copy and sends nothing", async () => {
  const fake = await primed({ "files.send": () => ({ transfer_id: "t_1" }) });
  await emit("core:share", PICK);
  await vi.waitFor(() => expect(store.pendingShare).not.toBeNull());

  store.cancelShare();

  expect(store.pendingShare).toBeNull();
  expect(fake.discards).toEqual(["s_1"]);
  expect(fake.calls.some((c) => c.method === "files.send")).toBe(false);
});

test.each([
  [[{ name: "holiday.jpg", path: "/c/holiday.jpg", size: 2516582 }], "holiday.jpg · 2.4 MiB"],
  [
    [
      { name: "a.pdf", path: "/c/a.pdf", size: 1024 },
      { name: "b.png", path: "/c/b.png", size: 512 },
    ],
    "2 files · 1.5 KiB",
  ],
])("a pending share reads as %o → %s", (files, summary) => {
  expect(shareSummary(files)).toBe(summary);
});

test("setUpFromAddress asks the server for its settings, then writes them", async () => {
  const CONFIGURED: SessionState = {
    logged_in: false,
    server_connected: false,
    configured: true,
  };
  const fake = mockCore({
    status: CONNECTED,
    methods: {
      "session.status": () => CONFIGURED,
      "session.reload": () => CONFIGURED,
      "session.discover": () => ({
        server_url: "wss://universallink.example.com/ws",
        oidc_issuer: "https://accounts.google.com",
        oidc_client_id: "abc.apps.googleusercontent.com",
        oidc_client_secret: "GOCSPX-secret",
      }),
    },
  });
  await store.start();
  await flush();

  const outcome = await store.setUpFromAddress("universallink.example.com");

  expect(outcome).toBe("saved");
  // The address goes as typed — deriving the URLs is the Core's job, in one
  // place, shared with the phone.
  expect(
    fake.calls.find((c) => c.method === "session.discover")?.params,
  ).toEqual({ url: "universallink.example.com" });
  // What came back is what got written, secret included: the field that used to
  // be carried to every device by hand.
  expect(fake.configWrites).toEqual([
    {
      server_url: "wss://universallink.example.com/ws",
      oidc_issuer: "https://accounts.google.com",
      oidc_client_id: "abc.apps.googleusercontent.com",
      oidc_client_secret: "GOCSPX-secret",
    },
  ]);
  expect(store.session?.configured).toBe(true);
});

test("setUpFromAddress reports a server that publishes nothing, without a banner", async () => {
  const fake = mockCore({
    status: CONNECTED,
    methods: {
      "session.discover": () => {
        throw appError("NO_DESCRIPTOR", "this server does not publish its settings");
      },
    },
  });
  await store.start();
  await flush();

  const outcome = await store.setUpFromAddress("old-server.example.com");

  // The caller acts on this one (it reveals the fields to fill in) instead of
  // reading a banner it would have to explain around.
  expect(outcome).toBe("unpublished");
  expect(store.notice).toBeNull();
  // Nothing was written: a failed discovery leaves the config as it was.
  expect(fake.configWrites).toEqual([]);
});

test("setUpFromAddress gives an incomplete descriptor its context", async () => {
  mockCore({
    status: CONNECTED,
    methods: {
      "session.discover": () => {
        throw appError("INVALID_DESCRIPTOR", "oidc_client_id is missing");
      },
    },
  });
  await store.start();
  await flush();

  const outcome = await store.setUpFromAddress("host.example");

  expect(outcome).toBe("failed");
  // The Core names the field; on its own that reads as noise, so the banner says
  // what it is about too.
  expect(store.notice?.text).toContain("oidc_client_id");
  expect(store.notice?.text).toContain("incomplete");
});

test("setUpFromAddress surfaces an unreachable server as any other failure", async () => {
  const fake = mockCore({
    status: CONNECTED,
    methods: {
      "session.discover": () => {
        throw appError("SERVER_UNREACHABLE", "connecting to host.example");
      },
    },
  });
  await store.start();
  await flush();

  expect(await store.setUpFromAddress("host.example")).toBe("failed");
  expect(store.notice?.text).toBe("Server unreachable.");
  expect(fake.configWrites).toEqual([]);
});

// -- Pairing ----------------------------------------------------------------

const OFFER = {
  pairing_id: "p_1",
  code: "UL1:secret:key:p_1",
  role: "joiner",
  expires_in: 120,
};

/** The methods a pairing walks through, with the answers a Core would give. */
function pairingMethods(over: Record<string, Method> = {}) {
  return {
    "pairing.offer": () => OFFER,
    "pairing.accept": () => ({
      pairing_id: "p_1",
      role: "joiner",
      verification: "428 913",
    }),
    "pairing.confirm": () => ({ status: "done" }),
    "pairing.cancel": () => ({}),
    ...over,
  };
}

test("showing a code puts the Core's answer on screen, role included", async () => {
  const fake = await primed(pairingMethods());

  await store.showPairingCode();

  expect(fake.calls.map((c) => c.method)).toContain("pairing.offer");
  expect(store.pairing).toEqual({
    pairing_id: "p_1",
    role: "joiner",
    phase: "showing",
    code: "UL1:secret:key:p_1",
    expires_in: 120,
  });
});

// The role is the Core's answer, not this screen's assumption: the SAME gesture
// on a device that can vouch makes it the sponsor, and the confirmation is then
// this device's to make.
test("a code read on a device that can vouch goes straight to the confirmation", async () => {
  await primed(
    pairingMethods({
      "pairing.accept": () => ({
        pairing_id: "p_2",
        role: "sponsor",
        verification: "111 222",
        device: { name: "New laptop", platform: "linux", node_id: "ab" },
      }),
    }),
  );

  await store.enterPairingCode("  UL1:secret:key:p_2  ");

  expect(store.pairing).toMatchObject({
    pairing_id: "p_2",
    role: "sponsor",
    phase: "confirm",
    verification: "111 222",
    device: { name: "New laptop" },
  });
});

test("the code is trimmed, and an empty one never reaches the Core", async () => {
  const fake = await primed(pairingMethods());

  await store.enterPairingCode("   ");
  expect(fake.calls.map((c) => c.method)).not.toContain("pairing.accept");

  await store.enterPairingCode(" UL1:x:y:p_1 \n");
  expect(fake.calls.find((c) => c.method === "pairing.accept")?.params).toEqual({
    code: "UL1:x:y:p_1",
  });
});

test("being claimed carries the number both ends must show", async () => {
  await primed(pairingMethods());
  await store.showPairingCode();

  await emit("core:notification", {
    method: "pairing.claimed",
    params: { pairing_id: "p_1", verification: "428 913" },
  });

  // A joiner waits: the human confirms on the OTHER device, and this screen's
  // job is to show the number they will compare it against.
  expect(store.pairing).toMatchObject({
    phase: "waiting",
    verification: "428 913",
  });
});

// The role decides who confirms — never the presence of a device record. A
// server that sent one to the joining side must not turn it into the one that
// vouches.
test("a device record does not make the joining side the one that confirms", async () => {
  await primed(pairingMethods());
  await store.showPairingCode(); // role: joiner

  await emit("core:notification", {
    method: "pairing.claimed",
    params: {
      pairing_id: "p_1",
      verification: "428 913",
      device: { name: "Somebody else", platform: "linux" },
    },
  });

  expect(store.pairing?.phase).toBe("waiting");
});

test("an event that names another pairing is not ours to act on", async () => {
  await primed(pairingMethods());
  await store.showPairingCode();

  for (const method of ["pairing.claimed", "pairing.completed", "pairing.failed"]) {
    await emit("core:notification", {
      method,
      params: { pairing_id: "p_other", reason: "declined" },
    });
  }

  expect(store.pairing).toMatchObject({ pairing_id: "p_1", phase: "showing" });
  expect(store.notice).toBeNull();
});

// The completion is the ONLY word the joining device gets that it now has the
// account: `account.status` has no notification, so this is what re-reads it —
// and the message has to survive that very resnapshot.
test("a completed pairing resnapshots, and says so in a message that survives it", async () => {
  const fake = await primed(pairingMethods());
  await store.showPairingCode();
  const before = fake.calls.length;

  await emit("core:notification", {
    method: "pairing.completed",
    params: { pairing_id: "p_1" },
  });

  expect(store.pairing).toBeNull();
  await vi.waitFor(() =>
    expect(
      fake.calls.slice(before).some((c) => c.method === "account.status"),
    ).toBe(true),
  );
  expect(store.notice).toEqual({
    kind: "info",
    sticky: true,
    text: "This device is now linked to your account.",
  });
});

test("a sponsor's completion names the device it added", async () => {
  await primed(
    pairingMethods({
      "pairing.accept": () => ({
        pairing_id: "p_1",
        role: "sponsor",
        verification: "1",
        device: { name: "New laptop", platform: "linux" },
      }),
    }),
  );
  await store.enterPairingCode("UL1:x:y:p_1");

  await emit("core:notification", {
    method: "pairing.completed",
    params: { pairing_id: "p_1" },
  });

  expect(store.notice?.text).toBe("New laptop is now linked to your account.");
});

test.each([
  ["declined", "The other device declined."],
  ["expired", "The code expired before the link was confirmed."],
  ["other_account", "That device belongs to a different account."],
  // Outside the vocabulary (a server code, a later version): carried verbatim
  // rather than swallowed.
  ["WHATEVER", "The link failed (WHATEVER)."],
])("a failed pairing ends the screen and says why: %s", async (reason, text) => {
  await primed(pairingMethods());
  await store.showPairingCode();

  await emit("core:notification", {
    method: "pairing.failed",
    params: { pairing_id: "p_1", reason },
  });

  expect(store.pairing).toBeNull();
  expect(store.notice).toEqual({ kind: "error", sticky: true, text });
});

test("confirming is the sponsor's gesture alone", async () => {
  const fake = await primed(pairingMethods());
  await store.showPairingCode(); // joiner, phase "showing"

  await store.confirmPairing();
  await emit("core:notification", {
    method: "pairing.claimed",
    params: { pairing_id: "p_1", verification: "1" },
  });
  await store.confirmPairing(); // phase "waiting" now, still not ours

  expect(fake.calls.map((c) => c.method)).not.toContain("pairing.confirm");
});

test("a confirmation the server wants re-authorized goes through the browser", async () => {
  const fake = await primed(
    pairingMethods({
      "pairing.accept": () => ({
        pairing_id: "p_1",
        role: "sponsor",
        verification: "1",
        device: { name: "New laptop", platform: "linux" },
      }),
      "pairing.confirm": () => ({
        status: "reauth_required",
        auth_url: "https://idp.test/reauth",
      }),
    }),
  );
  await store.enterPairingCode("UL1:x:y:p_1");

  await store.confirmPairing();

  expect(fake.opened).toEqual(["https://idp.test/reauth"]);
  // Same round-trip as a login, so the same hold on the process (mobile only).
  expect(fake.holds).toEqual(["auth:true"]);
  // The screen stays up: the outcome will arrive as an event.
  expect(store.pairing).toMatchObject({ phase: "confirming" });
  expect(store.notice?.kind).toBe("info");
});

test("a confirmation the keyring could settle opens no browser", async () => {
  const fake = await primed(
    pairingMethods({
      "pairing.accept": () => ({
        pairing_id: "p_1",
        role: "sponsor",
        verification: "1",
        device: { name: "New laptop", platform: "linux" },
      }),
    }),
  );
  await store.enterPairingCode("UL1:x:y:p_1");

  await store.confirmPairing();

  expect(fake.opened).toEqual([]);
  // Nothing is concluded here either: `pairing.completed` closes the screen.
  expect(store.pairing).toMatchObject({ phase: "confirm" });
});

test("declining tells the Core and closes the screen at once", async () => {
  const fake = await primed(pairingMethods());
  await store.showPairingCode();

  await store.cancelPairing();

  expect(store.pairing).toBeNull();
  expect(fake.calls.find((c) => c.method === "pairing.cancel")?.params).toEqual({
    pairing_id: "p_1",
  });
});

// A Core or a server older than pairing answers `method not found`. There is
// nothing to retry, so the screens stop offering it — and a RECONNECTION (which
// may be to a Core that was just updated) offers it again.
test("a Core that does not know pairing takes the offer off the screen", async () => {
  await primed({
    "pairing.offer": () => {
      throw { kind: "rpc", message: "method not found", code: -32601 };
    },
  });

  await store.showPairingCode();

  expect(store.pairing).toBeNull();
  expect(store.pairingSupported).toBe(false);
  expect(store.notice?.text).toContain("newer UniversalLink");

  await emit("core:connection", CONNECTED);
  await vi.waitFor(() => expect(store.pairingSupported).toBe(true));
});

test("any other refusal is reported without taking the offer away", async () => {
  await primed({
    "pairing.offer": () => {
      throw appError("SERVER_UNREACHABLE");
    },
  });

  await store.showPairingCode();

  expect(store.pairing).toBeNull();
  expect(store.pairingSupported).toBe(true);
  expect(store.notice).toEqual({ kind: "error", text: "Server unreachable." });
});

// The Core says nothing about a code it cannot parse beyond "invalid params",
// which is not a sentence: the likely cause is a line copied short of its end.
test("a code the Core cannot read is explained, not relayed", async () => {
  await primed({
    "pairing.accept": () => {
      throw invalidParams("code");
    },
  });

  await store.enterPairingCode("UL1:truncated");

  expect(store.pairing).toBeNull();
  expect(store.pairingSupported).toBe(true);
  expect(store.notice?.text).toContain("not a complete pairing code");
});

// -- Pairing: reading a code with the camera (mobile) ------------------------

test("the shell is asked once whether this device can read a code", async () => {
  await primed(undefined, { scanner: true });

  expect(store.canScan).toBe(true);
});

// The desktop shell registers no scanner at all, and that rejection IS the
// answer: no camera gesture is ever offered there.
test("a shell with no scanner leaves the gesture off", async () => {
  await primed();

  expect(store.canScan).toBe(false);
});

// The point of the whole brick: what the camera reads takes exactly the path a
// pasted code takes, and nothing else.
test("a scanned code is joined with, like any other", async () => {
  const fake = await primed(pairingMethods(), {
    scanner: true,
    scanned: () => ({ code: "UL1:scanned:key:p_1" }),
  });

  await store.scanPairingCode();

  expect(fake.scans).toBe(1);
  expect(fake.calls.find((c) => c.method === "pairing.accept")?.params).toEqual({
    code: "UL1:scanned:key:p_1",
  });
  expect(store.pairing).toMatchObject({ pairing_id: "p_1", phase: "waiting" });
  expect(store.notice).toBeNull();
  expect(store.scanning).toBe(false);
});

// A scan is a human framing a camera, so it must not hold `busy`: that would
// disarm the rest of the app for a minute, and would make the very
// `pairing.accept` this gesture exists for return early.
test("the scan holds its own flag, not the one that guards the Core", async () => {
  const scan = deferred<unknown>();
  const fake = await primed(pairingMethods(), {
    scanner: true,
    scanned: () => scan.promise,
  });

  const done = store.scanPairingCode();
  await flush();
  expect(store.scanning).toBe(true);
  expect(store.busy).toBe(false);

  // A second gesture while the camera is up asks for nothing.
  await store.scanPairingCode();
  expect(fake.scans).toBe(1);

  scan.resolve({ code: "UL1:scanned:key:p_1" });
  await done;
  expect(store.scanning).toBe(false);
  expect(fake.calls.map((c) => c.method)).toContain("pairing.accept");
});

test("backing out of the camera says nothing at all", async () => {
  const fake = await primed(pairingMethods(), {
    scanner: true,
    scanned: () => ({ reason: "cancelled" }),
  });

  await store.scanPairingCode();

  expect(store.notice).toBeNull();
  expect(store.pairing).toBeNull();
  expect(fake.calls.map((c) => c.method)).not.toContain("pairing.accept");
});

test("a refused camera explains itself and names the way round it", async () => {
  await primed(pairingMethods(), {
    scanner: true,
    scanned: () => ({ reason: "denied" }),
  });

  await store.scanPairingCode();

  expect(store.notice?.text).toContain("paste the code instead");
  // Refused once is not refused for good: the gesture stays on offer.
  expect(store.canScan).toBe(true);
});

// ...whereas a camera that does not exist will not appear: the button goes.
test("a device with no camera stops being offered one", async () => {
  await primed(pairingMethods(), {
    scanner: true,
    scanned: () => ({ reason: "no_camera" }),
  });

  await store.scanPairingCode();

  expect(store.canScan).toBe(false);
  expect(store.notice?.text).toContain("no camera");
});

// The shell holds a scanner this page knows nothing about — a webview reloaded
// while the camera was up. Nothing to say: the scanner the user is looking at is
// the one that will answer.
test("a scanner the shell already has open is not reported", async () => {
  await primed(pairingMethods(), {
    scanner: true,
    scanned: () => ({ reason: "busy" }),
  });

  await store.scanPairingCode();

  expect(store.notice).toBeNull();
  expect(store.canScan).toBe(true);
});

// A word from a newer shell than this page: still an answer, still a sentence.
test("an outcome this page has no word for is still reported", async () => {
  await primed(pairingMethods(), {
    scanner: true,
    scanned: () => ({ reason: "lens_cap" }),
  });

  await store.scanPairingCode();

  expect(store.notice?.text).toContain("could not be read");
});
