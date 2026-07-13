// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

/**
 * A fake Core, to look at the screens in a browser (`npm run dev`) without a
 * daemon or webview. No verification value: the contracts are held by
 * gui/tests/api/ (shell) and src/lib/*.test.ts (frontend).
 *
 * This module is loaded only by main.ts's `import.meta.env.DEV` branch; it does
 * not follow the production bundle.
 */

import { emit } from "@tauri-apps/api/event";
import { mockIPC } from "@tauri-apps/api/mocks";

import type {
  Component,
  Device,
  PendingRequest,
  SessionState,
} from "../lib/api";

const LOGGED_OUT: SessionState = { logged_in: false, server_connected: false };

const DEVICES: Device[] = [
  {
    device_id: "d_self",
    name: "Office PC",
    platform: "linux",
    online: true,
    last_seen: null,
    is_self: true,
  },
  {
    device_id: "d_mac",
    name: "MacBook",
    platform: "macos",
    online: false,
    last_seen: new Date(Date.now() - 3 * 3600_000).toISOString(),
    is_self: false,
  },
  {
    device_id: "d_win",
    name: "Living Room PC",
    platform: "windows",
    online: true,
    last_seen: null,
    is_self: false,
  },
];

const REQUEST: PendingRequest = {
  request_id: "r_clipnet",
  name: "clipnet",
  role: "clipboard-backend",
  scopes: ["devices.read", "clipboard.read", "clipboard.write"],
  peer_info: { pid: 4242, exe: "/usr/local/bin/clipnet" },
};

function rpc(code: string) {
  return { kind: "rpc", message: code.toLowerCase(), code: -32000, data_code: code };
}

export function installFakeCore(): void {
  let session: SessionState = LOGGED_OUT;
  // Account key (C7): not attested at first, to show the onboarding portal.
  // Persists after logout, like the real root on disk.
  let attested = false;
  let fingerprint: string | null = null;
  let devices: Device[] = [];
  let pending: PendingRequest[] = [];
  let components: Component[] = [
    {
      component_id: "c_gui",
      name: "universallink-gui",
      role: "gui",
      scopes: ["session.read", "devices.read", "components.approve"],
      connected: true,
      enrolled: false,
    },
  ];

  const changed = () => void emit("core:notification", {
    method: "session.changed",
    params: session,
  });
  const notify = (method: string, params: unknown) =>
    void emit("core:notification", { method, params });

  const methods: Record<string, (p: Record<string, string>) => unknown> = {
    "session.status": () => session,
    "session.login": () => {
      if (session.logged_in) throw rpc("ALREADY_LOGGED_IN");
      setTimeout(() => {
        session = {
          logged_in: true,
          server_connected: true,
          account: { email: "account@example.test" },
        };
        devices = DEVICES.map((d) => ({ ...d }));
        changed();
      }, 1200);
      return { auth_url: "https://example.test/oauth/authorize?demo=1" };
    },
    "session.logout": () => {
      session = LOGGED_OUT;
      devices = [];
      changed();
      return {};
    },
    "account.status": () => ({ attested, fingerprint }),
    "account.setup": () => {
      if (attested) throw rpc("ACCOUNT_KEY_SET");
      if (!session.server_connected) throw rpc("SERVER_UNREACHABLE");
      attested = true;
      fingerprint = "AB12 CD34 EF56 7890";
      return { recovery_code: "riverbed-lantern-harbor-92", fingerprint };
    },
    "account.join": ({ recovery_code }) => {
      if (!session.server_connected) throw rpc("SERVER_UNREACHABLE");
      if (!recovery_code) throw rpc("INVALID_CODE");
      attested = true;
      fingerprint = "AB12 CD34 EF56 7890";
      return { fingerprint };
    },
    "devices.list": () => {
      if (!session.logged_in) throw rpc("SERVER_UNREACHABLE");
      return devices;
    },
    "devices.rename": ({ device_id, name }) => {
      const device = devices.find((d) => d.device_id === device_id);
      if (!device) throw rpc("DEVICE_UNKNOWN");
      device.name = name;
      notify("device.updated", { device: { ...device } });
      return {};
    },
    "devices.revoke": ({ device_id }) => {
      const device = devices.find((d) => d.device_id === device_id);
      if (!device) throw rpc("DEVICE_UNKNOWN");
      // The real Core requires a fresh ID token to revoke: we replay the
      // browser detour for one's own device, the trickiest path.
      if (device.is_self) {
        setTimeout(() => {
          devices = devices.filter((d) => d.device_id !== device_id);
          notify("device.removed", { device_id });
          session = LOGGED_OUT;
          changed();
        }, 2000);
        return {
          status: "reauth_required",
          auth_url: "https://example.test/oauth/authorize?reauth=1",
        };
      }
      devices = devices.filter((d) => d.device_id !== device_id);
      notify("device.removed", { device_id });
      return { status: "done" };
    },
    "components.pending": () => pending,
    "components.list": () => components,
    "components.approve": ({ request_id }) => {
      const request = pending.find((r) => r.request_id === request_id);
      if (!request) throw { kind: "rpc", message: "invalid params", code: -32602 };
      pending = pending.filter((r) => r.request_id !== request_id);
      components = [
        ...components,
        {
          component_id: `c_${request.name}`,
          name: request.name,
          role: request.role,
          scopes: request.scopes,
          connected: true,
          enrolled: true,
        },
      ];
      return {};
    },
    "components.deny": ({ request_id }) => {
      pending = pending.filter((r) => r.request_id !== request_id);
      return {};
    },
    "components.revoke": ({ component_id }) => {
      components = components.filter((c) => c.component_id !== component_id);
      return {};
    },
  };

  mockIPC(
    (cmd, payload) => {
      const args = (payload ?? {}) as Record<string, string>;
      if (cmd === "connection_status") return { status: "connecting" };
      if (cmd === "plugin:opener|open_url") {
        window.open(args.url, "_blank", "noopener");
        return null;
      }
      if (cmd === "core_request") {
        const { method, params } = payload as {
          method: string;
          params?: Record<string, string>;
        };
        const handler = methods[method];
        if (!handler) throw { kind: "rpc", message: "method not found", code: -32601 };
        return handler(params ?? {});
      }
      throw new Error(`unexpected command: ${cmd}`);
    },
    { shouldMockEvents: true },
  );

  setTimeout(
    () =>
      void emit("core:connection", {
        status: "connected",
        granted_scopes: [
          "session.read",
          "session.manage",
          "devices.read",
          "devices.manage",
          "files.send",
          "transfers.read",
          "components.approve",
        ],
        api_version: 1,
      }),
    400,
  );
  // An enrollment request arrives on its own: this is the scenario from the prompt.
  setTimeout(() => {
    pending = [REQUEST];
    notify("component.pending", REQUEST);
  }, 5000);
}
