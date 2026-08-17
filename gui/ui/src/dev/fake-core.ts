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
  InputState,
  PendingRequest,
  SessionState,
} from "../lib/api";

// `configured: true` so the dev demo lands on the normal screens; flip it to
// `false` to preview the first-run server-setup gate.
const LOGGED_OUT: SessionState = {
  logged_in: false,
  server_connected: false,
  configured: true,
};

const DEVICES: Device[] = [
  {
    device_id: "d_self",
    name: "Office PC",
    platform: "linux",
    online: true,
    lan: false,
    reachable: true,
    last_seen: null,
    is_self: true,
  },
  {
    device_id: "d_mac",
    name: "MacBook",
    platform: "macos",
    online: false,
    lan: false,
    reachable: false,
    last_seen: new Date(Date.now() - 3 * 3600_000).toISOString(),
    is_self: false,
  },
  {
    // Heard over mDNS but not marked online by the server: the LAN case —
    // reachable, badged "on this network".
    device_id: "d_win",
    name: "Living Room PC",
    platform: "windows",
    online: false,
    lan: true,
    reachable: true,
    last_seen: null,
    is_self: false,
  },
];

const NODE_SELF = "1a".repeat(32);
const NODE_MAC = "2b".repeat(32);
const NODE_WIN = "3c".repeat(32);

/**
 * A desk worth looking at: this computer with two screens side by side, the
 * Living Room PC to their right (so there is a real crossing to set guards on),
 * and the MacBook's screen kept as a ghost, which is the case a plane exists for.
 * Its own arrangement is what the engine would have derived.
 */
const INPUT: InputState = {
  here: {
    device_id: "d_self",
    name: "Office PC",
    monitors: [
      {
        id: "DP-1",
        name: "Dell U2720Q",
        w: 2560,
        h: 1440,
        x: 0,
        y: 0,
        scale: 1000,
        primary: true,
        present: true,
      },
      {
        id: "HDMI-1",
        name: "Second screen",
        w: 1920,
        h: 1080,
        x: 2560,
        y: 0,
        scale: 1000,
        primary: false,
        present: true,
      },
    ],
    problem: null,
    can_drive: true,
    can_be_driven: true,
  },
  plane: {
    id: "7f3a91cc20b45de6a1740b9e3f28cd51",
    by: "d_self",
    spots: [
      {
        monitor: `${NODE_SELF}/DP-1`,
        device_id: "d_self",
        name: "Dell U2720Q",
        x: 0,
        y: 0,
        w: 2560,
        h: 1440,
        present: true,
        primary: true,
      },
      {
        monitor: `${NODE_SELF}/HDMI-1`,
        device_id: "d_self",
        name: "Second screen",
        x: 2560,
        y: 0,
        w: 1920,
        h: 1080,
        present: true,
        primary: false,
      },
      {
        monitor: `${NODE_WIN}/win:dev:0`,
        device_id: "d_win",
        name: "Living room TV",
        x: 4480,
        y: 0,
        w: 1920,
        h: 1080,
        present: true,
        primary: true,
      },
      // A ghost, exactly as the engine renders one: no name, not primary, and a
      // nominal size, because the only thing anybody still knows about it is
      // where it was.
      {
        monitor: `${NODE_MAC}/mac:1`,
        device_id: "d_mac",
        name: "",
        x: -1920,
        y: 200,
        w: 1920,
        h: 1080,
        present: false,
        primary: false,
      },
    ],
  },
  devices: [
    {
      device_id: "d_mac",
      name: "MacBook",
      state: "off",
      monitors: [],
      rtt_ms: null,
      lan: false,
      allowed: false,
      drive: false,
      mode: "typing",
      problem: null,
    },
    {
      device_id: "d_win",
      name: "Living Room PC",
      state: "ready",
      monitors: [
        {
          id: "win:dev:0",
          name: "Living room TV",
          w: 1920,
          h: 1080,
          x: 0,
          y: 0,
          scale: 1000,
          primary: true,
          present: true,
        },
      ],
      rtt_ms: 6,
      lan: true,
      allowed: true,
      drive: true,
      mode: "typing",
      problem: null,
    },
  ],
  session: null,
  guards: [],
  lock: false,
  hotkey: ["ctrl", "alt", "Escape"],
};

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
  // The account's private key at rest: acquired along with the attestation.
  let holdsKey = false;
  let devices: Device[] = [];
  let pending: PendingRequest[] = [];
  let components: Component[] = [
    {
      component_id: "c_gui",
      name: "1device-gui",
      role: "gui",
      scopes: ["session.read", "devices.read", "components.approve"],
      connected: true,
      enrolled: false,
    },
  ];

  // The server config the setup screen reads/writes (dev only).
  let serverConfig = {
    server_url: "wss://demo.example/ws",
    oidc_issuer: "https://accounts.google.com",
    oidc_client_id: "demo.apps.googleusercontent.com",
    oidc_client_secret: null as string | null,
  };

  let input: InputState = INPUT;

  const changed = () => void emit("core:notification", {
    method: "session.changed",
    params: session,
  });
  const notify = (method: string, params: unknown) =>
    void emit("core:notification", { method, params });
  /** The engine publishes its whole state after every change, so this does too. */
  const inputChanged = () => notify("input.updated", { state: input });
  const withPeer = (
    device_id: string,
    change: (peer: InputState["devices"][number]) => InputState["devices"][number],
  ): InputState => ({
    ...input,
    devices: input.devices.map((peer) =>
      peer.device_id === device_id ? change(peer) : peer,
    ),
  });

  /** What both `pairing.accept` and `pairing.claimed` carry. */
  const claimed = (pairing_id: string, role: string) => ({
    pairing_id,
    role,
    verification: "428 913",
    ...(role === "sponsor"
      ? {
          device: {
            name: "New laptop",
            platform: "linux",
            node_id: "9f".repeat(32),
          },
        }
      : {}),
  });

  /** This device received the account: what the real Core's completion leaves. */
  const joined = (pairing_id: string) => {
    attested = true;
    holdsKey = true;
    fingerprint = "AB12 CD34 EF56 7890";
    session = {
      logged_in: true,
      server_connected: true,
      configured: true,
      account: { email: "account@example.test" },
    };
    devices = DEVICES.map((d) => ({ ...d }));
    notify("pairing.completed", { pairing_id });
    changed();
  };

  const methods: Record<string, (p: Record<string, string>) => unknown> = {
    "session.status": () => session,
    "session.login": () => {
      if (session.logged_in) throw rpc("ALREADY_LOGGED_IN");
      setTimeout(() => {
        session = {
          logged_in: true,
          server_connected: true,
          configured: true,
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
    "session.reload": () => session,
    "account.status": () => ({ attested, fingerprint, holds_key: holdsKey }),
    "account.setup": () => {
      if (attested) throw rpc("ACCOUNT_KEY_SET");
      if (!session.server_connected) throw rpc("SERVER_UNREACHABLE");
      attested = true;
      holdsKey = true;
      fingerprint = "AB12 CD34 EF56 7890";
      return { recovery_code: "riverbed-lantern-harbor-92", fingerprint };
    },
    // A code already known to this account goes through and only stows the key
    // (the real Core refuses a code of ANOTHER account: nothing here models a
    // second account, so every accepted code is "the same one").
    "account.join": ({ recovery_code }) => {
      if (!session.server_connected) throw rpc("SERVER_UNREACHABLE");
      if (!recovery_code) throw rpc("INVALID_CODE");
      attested = true;
      holdsKey = true;
      fingerprint = "AB12 CD34 EF56 7890";
      return { fingerprint };
    },
    // Pairing, both roles, on timers: enough to walk the screens in a browser.
    // The code is the shape of a real one (~105 characters) so the QR comes out at
    // the size it will really be.
    "pairing.offer": () => {
      const pairing_id = `p_${"ab12".repeat(8)}`;
      const role = attested && holdsKey ? "sponsor" : "joiner";
      // The other device reads the code a moment later.
      setTimeout(() => notify("pairing.claimed", claimed(pairing_id, role)), 1500);
      if (role === "joiner") setTimeout(() => joined(pairing_id), 4000);
      return {
        pairing_id,
        role,
        expires_in: 120,
        code: `1D1:${"A".repeat(22)}:${"B".repeat(43)}:${pairing_id}`,
      };
    },
    // Either tag: `1D2` names a device on the local network instead of a server
    // rendezvous, and this fake plays the flow the same either way.
    "pairing.accept": ({ code }) => {
      if (!code?.startsWith("1D1:") && !code?.startsWith("1D2:")) {
        throw { kind: "rpc", message: "invalid params: code", code: -32602 };
      }
      const pairing_id = code.slice(code.lastIndexOf(":") + 1);
      const role = attested && holdsKey ? "sponsor" : "joiner";
      if (role === "joiner") setTimeout(() => joined(pairing_id), 2500);
      return claimed(pairing_id, role);
    },
    "pairing.confirm": ({ pairing_id }) => {
      setTimeout(() => notify("pairing.completed", { pairing_id }), 400);
      return { status: "done" };
    },
    "pairing.cancel": () => ({}),
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
    // Keyboard and mouse. Enough of the engine to walk the Input tab in a
    // browser: the gestures mutate this fake state and publish the whole
    // snapshot, which is exactly the contract the real engine has.
    "input.status": () => input,
    "input.place": ({ spots }) => {
      const placed = spots as unknown as { monitor: string; x: number; y: number }[];
      input = {
        ...input,
        plane: {
          ...input.plane,
          by: "d_self",
          spots: input.plane.spots.map((spot) => {
            const at = placed.find((p) => p.monitor === spot.monitor);
            return at ? { ...spot, x: at.x, y: at.y } : spot;
          }),
        },
      };
      inputChanged();
      return {};
    },
    "input.allow": ({ device_id, allowed }) => {
      input = withPeer(device_id, (peer) => ({
        ...peer,
        allowed: allowed as unknown as boolean,
      }));
      inputChanged();
      return {};
    },
    "input.drive": ({ device_id, allowed }) => {
      input = withPeer(device_id, (peer) => ({
        ...peer,
        drive: allowed as unknown as boolean,
      }));
      inputChanged();
      return {};
    },
    "input.take": ({ device_id, mode }) => {
      const peer = input.devices.find((d) => d.device_id === device_id);
      if (!peer) throw rpc("INPUT_DEVICE_UNKNOWN");
      if (!peer.drive) {
        // The far side's word, learned by trying: a refusal, then the standing
        // problem in the snapshot. The engine does exactly this.
        setTimeout(() => {
          notify("input.refused", {
            device_id,
            code: "NOT_ALLOWED",
            count: 1,
          });
          input = withPeer(device_id, (p) => ({
            ...p,
            state: "refused",
            problem: "not_allowed",
          }));
          inputChanged();
        }, 300);
        return {};
      }
      input = {
        ...input,
        session: {
          device_id,
          direction: "out",
          mode: (mode as unknown as "full" | "keys") ?? "full",
          since: Date.now(),
          rtt_ms: peer.rtt_ms,
        },
      };
      input = withPeer(device_id, (p) => ({ ...p, state: "driving" }));
      inputChanged();
      return {};
    },
    "input.release": () => {
      const device_id = input.session?.device_id;
      input = { ...input, session: null };
      if (device_id) {
        input = withPeer(device_id, (p) => ({ ...p, state: "ready" }));
      }
      inputChanged();
      return {};
    },
    "input.guards": ({ device_id, monitor, side, guards }) => {
      const set = guards as unknown as Record<string, number | boolean>;
      const rest = input.guards.filter(
        (g) => !(g.monitor === monitor && g.side === side),
      );
      input = {
        ...input,
        guards: [
          ...rest,
          {
            device_id,
            monitor,
            side: side as unknown as "left" | "right" | "top" | "bottom",
            dwell_ms: 250,
            double_tap_ms: 0,
            dead_corner: 16,
            require_mods: 0,
            wall: false,
            ...set,
          },
        ],
      };
      inputChanged();
      return {};
    },
    "input.lock": ({ locked }) => {
      input = { ...input, lock: locked as unknown as boolean };
      inputChanged();
      return {};
    },
    "input.hotkey": ({ keys }) => {
      input = { ...input, hotkey: keys as unknown as string[] };
      inputChanged();
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
      if (cmd === "set_server_config") {
        serverConfig = { ...serverConfig, ...(payload as { config: typeof serverConfig }).config };
        return null;
      }
      if (cmd === "get_server_config") return serverConfig;
      // Mobile-only in production (the desktop shell registers neither), faked
      // here so the scanning gesture can be walked in a browser: a camera the
      // page cannot open answers with the code another device would be showing.
      if (cmd === "scan_supported") return true;
      if (cmd === "scan_code") {
        const pairing_id = `p_${"cd34".repeat(8)}`;
        return new Promise((resolve) =>
          setTimeout(
            () =>
              resolve({
                code: `1D1:${"A".repeat(22)}:${"B".repeat(43)}:${pairing_id}`,
              }),
            1200,
          ),
        );
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
          // Asked for optionally by the real shell, and granted by a Core of
          // this version: without them the Input section does not exist, which
          // is also worth being able to preview (drop them here).
          "input.read",
          "input.manage",
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
