// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

/**
 * The store: the sole holder of the Core's state on the frontend side.
 *
 * Three rules, drawn from the Core's contract (doc/core-api.md) and the shell's
 * (gui/tests/api/support.rs):
 *
 * 1. **State only comes from a snapshot or a notification**, never from a
 *    command's response. The Core guarantees this: `devices.rename` gives rise
 *    to a `device.updated`, `devices.revoke` to a `device.removed`,
 *    `session.login` and `session.logout` to a `session.changed`. No optimistic
 *    update, hence no possible divergence from the Core.
 *
 *    One exception, imposed by the Core: `components.*` has **no** queue-exit
 *    notification (a request approved from another GUI, or whose requester
 *    disconnects, vanishes silently) nor any `connected` change. After each
 *    decision, we resnapshot.
 *
 * 2. **Every resynchronization is total**: one path, one invariant. The
 *    notifications received during a resnapshot are set aside then replayed on
 *    top of it. Like the Core (`session.rs`), we accept the reverse window: a
 *    notification emitted BEFORE the snapshot is built will be replayed on top
 *    of it and may age it by one step. It is the shorter of the two windows,
 *    the handlers are idempotent, and the next notification corrects it.
 *
 * 3. **`session.changed` means "resnapshot"**. The Core does not replay the
 *    events missed during a server outage: on every session transition, the
 *    directory must be re-read.
 *
 * And a liveness invariant: **connected ⇒ sooner or later primed**. A
 * `RequestError::Timeout` leaves the IPC connection alive (client/src/lib.rs):
 * no new `connected` event will come to catch up on a missed resnapshot. Hence
 * the deferred retry — without it, a momentarily slow Core freezes the screen
 * on "Connecting to Core…" while the status dot is green.
 */

import { openUrl } from "@tauri-apps/plugin-opener";
import type { UnlistenFn } from "@tauri-apps/api/event";

import {
  api,
  type AccountKey,
  type Component,
  type Device,
  type PendingRequest,
  type SessionState,
} from "./api";
import {
  connectionStatus,
  discardShare,
  getServerConfig,
  keepAlive,
  onConnectionChanged,
  onCoreNotification,
  onShareStatus,
  setServerConfig,
  shareStatus,
  shareTaken,
  type ConnectionStatus,
  type CoreNotification,
  type ServerConfigInput,
  type ShareStatus,
  type SharedFile,
} from "./core";
import { humanize, isCoreError, isInvalidParams } from "./errors";
import { formatSize } from "./format";

export interface Notice {
  kind: "info" | "error";
  text: string;
  /**
   * Survives a resnapshot. For a message that is NOT the last action's feedback:
   * a share reports itself while the user may not have touched the app at all,
   * so a snapshot arriving in between says nothing about it and must not expire
   * it. Cleared like any other by the next action or by dismissing it.
   */
  sticky?: boolean;
}

const devices = (n: number) => `${n} device${n === 1 ? "" : "s"}`;

/** A file share waiting for a destination — the `pick` phase of a share. */
export type PendingShare = Extract<ShareStatus, { phase: "pick" }>;

/**
 * Every share status EXCEPT the one that is not a message: a `pick` is a
 * question, and it is answered by the device picker, not by a banner.
 */
export type ShareBanner = Exclude<ShareStatus, { phase: "pick" }>;

/**
 * What a share's progress reads like in the banner (Android share sheet; see
 * {@link ShareStatus}). Kept honest on purpose: a share that reached none of
 * the account's devices says so, because the user has already walked away from
 * the app that shared it and will not find out any other way.
 */
export function shareNotice(status: ShareBanner): Notice {
  return { ...shareText(status), sticky: true };
}

/** "holiday.jpg · 2.4 MiB" / "3 files · 12.1 MiB" — what is about to be sent. */
export function shareSummary(files: readonly SharedFile[]): string {
  const total = files.reduce((bytes, file) => bytes + file.size, 0);
  const what = files.length === 1 ? files[0].name : `${files.length} files`;
  return `${what} · ${formatSize(total)}`;
}

function shareText(status: ShareBanner): Notice {
  switch (status.phase) {
    case "preparing":
      return {
        kind: "info",
        text:
          status.files === 1
            ? "Preparing the shared file…"
            : `Preparing ${status.files} shared files…`,
      };
    case "sending":
      return {
        kind: "info",
        text: status.targets
          ? `Sharing with ${devices(status.targets)}…`
          : "Sharing…",
      };
    case "done": {
      const { delivered, failed } = status;
      if (delivered === 0) {
        return {
          kind: "error",
          text: `Could not reach ${failed === 1 ? "your other device" : `any of your ${devices(failed)}`}.`,
        };
      }
      return {
        kind: "info",
        text:
          failed === 0
            ? `Shared with ${devices(delivered)}.`
            : `Shared with ${delivered} of ${devices(delivered + failed)}.`,
      };
    }
    case "error":
      switch (status.reason) {
        // The Core reached its device directory and this account has no OTHER
        // device — the only case where "no other device" is a fact, because the
        // phone waits for the directory before announcing.
        case "no_devices":
          return {
            kind: "error",
            text: "No other device on your account — nothing was shared.",
          };
        case "not_signed_in":
          return {
            kind: "error",
            text: "Sign in to UniversalLink first — nothing was shared.",
          };
        case "offline":
          return {
            kind: "error",
            text: "Could not reach your account — nothing was shared.",
          };
        case "too_large":
          return {
            kind: "error",
            text: "That text is too large to share (8 MiB maximum).",
          };
        case "unconfirmed":
          return {
            kind: "error",
            text: "Shared, but your devices did not confirm receiving it.",
          };
        case "refused":
          return {
            kind: "error",
            text: status.detail
              ? `Sharing failed: ${status.detail}`
              : "Sharing failed.",
          };
        // The copy out of the share failed. Whatever the app that shared it
        // meant to hand over, we never got it — so there is nothing to send.
        case "unreadable":
          return {
            kind: "error",
            text: "Could not read what was shared — nothing was sent.",
          };
        case "no_space":
          return {
            kind: "error",
            text: "Not enough free space on this phone to prepare that share.",
          };
      }
  }
}

/** A file within a transfer (the `transfer.started` manifest). */
export interface TransferFile {
  name: string;
  size: number;
}

/**
 * An OUTGOING transfer, in progress or finished, as tracked by the `transfer.*`
 * notifications. The GUI only shows the outgoing direction: `transfer.incoming`
 * (another device sending us files) is ignored — the receipt lands silently in
 * the download folder (the Core's T2 contract). `done`/`total` are in bytes.
 */
export interface Transfer {
  transfer_id: string;
  device_id: string;
  files: TransferFile[];
  total: number;
  done: number;
  status: "active" | "finished" | "failed";
  /** Set if `failed`; `"cancelled"` on cancellation. */
  error?: string;
}

/** A request's scopes come from a third party's `hello`: the Core validates
 *  their membership in the known list, not their uniqueness. A duplicate would
 *  make the view raise `each_key_duplicate`, and would be sent back as-is to
 *  `components.approve`. */
function normalizePending(request: PendingRequest): PendingRequest {
  return { ...request, scopes: [...new Set(request.scopes)] };
}

export class CoreStore {
  connection = $state<ConnectionStatus>({ status: "connecting" });
  session = $state<SessionState | null>(null);
  /**
   * The account key state (C7), read by `account.status` at snapshot time —
   * there is NO `account.*` notification at all. `null` before the first
   * snapshot, or if the Core does not know the method (an old Core: we fail
   * open, no portal). See {@link AccountKey}.
   */
  account = $state<AccountKey | null>(null);
  devices = $state<Device[]>([]);
  components = $state<Component[]>([]);
  pending = $state<PendingRequest[]>([]);
  /** Outgoing transfers, active and recent (see {@link Transfer}). */
  transfers = $state<Transfer[]>([]);
  /**
   * The device currently hovered by a file drag, or `null`. Set by the drop
   * controller (lib/dragdrop.ts) to highlight the target; it is never set on
   * an ineligible device (offline, this PC).
   */
  dropTarget = $state<string | null>(null);
  /**
   * Files shared from the Android share sheet, waiting for a destination
   * (mobile only — always `null` on the desktop, which has drag-and-drop
   * instead). The picker lives in the Devices view; consuming this is
   * {@link sendShare} or {@link cancelShare}, and either way the copy in the
   * app's cache is released.
   */
  pendingShare = $state<PendingShare | null>(null);
  /** Why the directory is empty, when the Core refuses to serve it. */
  devicesError = $state<string | null>(null);
  /** The last action's feedback, shown as a banner. */
  notice = $state<Notice | null>(null);
  /** A user action is in flight: the buttons disarm. */
  busy = $state(false);
  /** A first snapshot has been applied: before that, the screen knows nothing. */
  primed = $state(false);
  /**
   * Keeps the onboarding portal open between account creation and the user's
   * acknowledgment of the code. Without it, a resnapshot (triggered by any
   * event) would see `attested` flip and would lift the portal, taking away the
   * displayed code — which is irrecoverable.
   */
  onboardingPending = $state(false);

  /** Delay before retrying a missed resnapshot. Lowered by the tests. */
  retryDelayMs = 2000;

  /** How many FINISHED transfers we keep in memory. Lowered by the tests. */
  transferHistory = 40;

  #unlisten: UnlistenFn[] = [];
  #retry: ReturnType<typeof setTimeout> | null = null;
  /** Non-null ⇔ a resnapshot is in flight: notifications accumulate in it. */
  #buffer: CoreNotification[] | null = null;
  /** Resnapshot generations: the most recent wins, the others withdraw. */
  #generation = 0;
  #sawConnectionEvent = false;
  #sawShareStatus = false;
  /**
   * Which share a transfer is carrying (`transfer_id` → share id), so its cache
   * copy can be released when the transfer ends. The `transfer_id` is used ONLY
   * as a key here: no state is derived from a command's response (rule 1).
   */
  #shareOfTransfer = new Map<string, string>();

  /**
   * Subscribe THEN read the snapshot: the shell updates its snapshot before
   * emitting, so what we read here covers everything emitted before our
   * subscription. But an event received *during* this read is more recent than
   * the snapshot — applying it afterward would make it regress. Hence
   * `#sawConnectionEvent`: as soon as an event has arrived, the snapshot is
   * stale and we drop it.
   *
   * The notifications subscribe first: otherwise a `connected` received during
   * the next subscription would launch a resnapshot whose concurrent
   * notifications would not yet be listened to by anyone.
   */
  async start(): Promise<void> {
    this.#unlisten.push(
      await onCoreNotification((n) => this.#onNotification(n)),
    );
    this.#unlisten.push(
      await onConnectionChanged((s) => {
        this.#sawConnectionEvent = true;
        this.#setConnection(s);
      }),
    );
    // Android share sheet only — always silent on the desktop. It borrows the
    // action banner (as a sticky notice), so a share reports itself wherever the
    // user happens to be, including behind the onboarding portals.
    this.#unlisten.push(
      await onShareStatus((s) => {
        this.#sawShareStatus = true;
        this.#applyShare(s);
      }),
    );
    const snapshot = await connectionStatus();
    if (!this.#sawConnectionEvent) this.#setConnection(snapshot);
    // A share is usually what STARTED the app, so its status was published
    // before this webview existed: read the retained one, having subscribed
    // above so a newer status cannot be missed. A live event wins — it is more
    // recent than the snapshot by construction.
    const share = await shareStatus();
    if (share && !this.#sawShareStatus) this.#applyShare(share);
  }

  /**
   * Applies a share status: a `pick` becomes the question the Devices view asks,
   * everything else becomes the banner.
   */
  #applyShare(status: ShareStatus): void {
    if (status.phase === "pick") {
      // Supersedes any earlier pick, and takes down the "Preparing…" banner this
      // one follows.
      this.#dropPending(status.id);
      this.pendingShare = status;
      this.notice = null;
      return;
    }
    // A share being prepared supersedes one still waiting for a destination:
    // the Rust side has already dropped its copy (share.rs `discard_pending`),
    // and offering to send bytes that no longer exist would be a dead end.
    if (status.phase === "preparing") this.#dropPending();
    this.notice = shareNotice(status);
  }

  /** Forgets the pending pick and releases its copy — unless it IS `keep`. */
  #dropPending(keep?: string): void {
    const pending = this.pendingShare;
    this.pendingShare = null;
    if (pending && pending.id !== keep) void discardShare(pending.id);
  }

  stop(): void {
    this.#cancelRetry();
    for (const unlisten of this.#unlisten) unlisten();
    this.#unlisten = [];
  }

  #cancelRetry(): void {
    if (this.#retry === null) return;
    clearTimeout(this.#retry);
    this.#retry = null;
  }

  #scheduleRetry(): void {
    this.#cancelRetry();
    this.#retry = setTimeout(() => {
      this.#retry = null;
      // The connection may have dropped in the meantime: the next `connected`
      // will resnapshot on its own.
      if (this.connection.status === "connected") void this.resync();
    }, this.retryDelayMs);
  }

  dismiss(): void {
    this.notice = null;
  }

  #setConnection(status: ConnectionStatus): void {
    this.connection = status;
    // A connection loss clears nothing: the data stays displayed, flagged as
    // frozen by the status. `incompatible` is terminal.
    if (status.status === "connected") void this.resync();
  }

  /** Total resnapshot. The only path for a bulk write of the state. */
  async resync(): Promise<void> {
    if (this.connection.status !== "connected") return;
    this.#cancelRetry();
    const generation = ++this.#generation;
    this.#buffer = [];

    const [session, account, devices, pending, components] =
      await Promise.allSettled([
        api.sessionStatus(),
        api.accountStatus(),
        api.devicesList(),
        api.componentsPending(),
        api.componentsList(),
      ]);
    // A more recent resnapshot has taken over: it owns the buffer.
    if (generation !== this.#generation) return;

    if (session.status === "rejected") {
      // `session.status` is purely local to the Core: its failure is not a
      // session state, it's a mute Core. Since a timeout leaves the connection
      // alive, no one would restart the resnapshot: we do it ourselves.
      this.#buffer = null;
      this.notice = { kind: "error", text: humanize(session.reason) };
      this.#scheduleRetry();
      return;
    }
    this.session = session.value;

    // A closed session (logout, or this device revoked remotely) cancels an
    // onboarding in progress: without this disarm, the flag would stay stuck
    // and would wrongly reopen the portal on reconnection, on a device that is
    // nonetheless already attested — with no way out (the code is gone,
    // `finishOnboarding` is out of reach). The attestation itself survives
    // disconnection (the disk root).
    if (!this.session?.logged_in) this.onboardingPending = false;

    // `account.status` is local to the Core and should not fail; if it does (a
    // Core older than C7: method_not_found), we fail OPEN — no portal — rather
    // than block on a capability the Core lacks. But while an onboarding is in
    // progress (`onboardingPending`), a transient rejection must NOT clear the
    // state: that would lift the portal (and so take away the displayed code).
    // We then keep the last known value.
    if (account.status === "fulfilled") {
      this.account = account.value;
    } else if (!this.onboardingPending) {
      this.account = null;
    }

    // `devices.list` responds `SERVER_UNREACHABLE` as long as the directory has
    // never been snapshotted (session closed, or server never reached): this is
    // not an action's failure, it's a state the view can show.
    this.devices = devices.status === "fulfilled" ? devices.value : [];
    this.devicesError =
      devices.status === "rejected" ? humanize(devices.reason) : null;

    this.pending =
      pending.status === "fulfilled" ? pending.value.map(normalizePending) : [];
    this.components = components.status === "fulfilled" ? components.value : [];
    this.primed = true;
    // A fresh snapshot expires any earlier message: without this, the error
    // from a missed resnapshot survives the recovery and lies to the user. A
    // STICKY one survives: it reports something the snapshot says nothing about
    // (a share), and the priming resnapshot alone would otherwise erase the
    // feedback of the very share that started the app.
    if (!this.notice?.sticky) this.notice = null;

    const buffered = this.#buffer ?? [];
    this.#buffer = null;
    let again = false;
    for (const notification of buffered) {
      again = this.#apply(notification) || again;
    }
    if (again) void this.resync();
  }

  #onNotification(notification: CoreNotification): void {
    // Transfers are a stream independent of the snapshot: NO method resnapshots
    // them. Buffering them would risk losing them forever — a missed resync
    // drops its buffer (see resync()), and a lost transfer would stay "in
    // progress" forever, for lack of recovery. So we ALWAYS apply them, outside
    // the buffer and outside the `primed` barrier.
    if (notification.method.startsWith("transfer.")) {
      this.#apply(notification);
      return;
    }
    if (this.#buffer) {
      this.#buffer.push(notification);
      return;
    }
    // Before the first snapshot, there is nothing to apply a delta to.
    if (!this.primed) return;
    if (this.#apply(notification)) void this.resync();
  }

  /**
   * Idempotent handlers (upsert / deletion by identifier): replaying a
   * notification already included in the snapshot has no effect. An unknown
   * method is ignored — the API is additive (doc/core-api.md).
   *
   * Returns `true` if the event requires a resnapshot.
   */
  #apply({ method, params }: CoreNotification): boolean {
    switch (method) {
      case "session.changed": {
        const state = params as SessionState | null;
        if (!state || typeof state.logged_in !== "boolean") return false;
        this.session = state;
        return true;
      }
      case "device.added":
      case "device.online":
      case "device.updated": {
        const device = (params as { device?: Device } | null)?.device;
        if (device?.device_id) this.#upsertDevice(device);
        return false;
      }
      case "device.offline": {
        const p = params as {
          device_id?: string;
          last_seen?: string | null;
        } | null;
        // The Core relays the event even for a device missing from its cache.
        const device = this.devices.find((d) => d.device_id === p?.device_id);
        if (!device) return false;
        device.online = false;
        if (p?.last_seen !== undefined) device.last_seen = p.last_seen;
        return false;
      }
      case "device.removed": {
        const id = (params as { device_id?: string } | null)?.device_id;
        if (id) this.devices = this.devices.filter((d) => d.device_id !== id);
        return false;
      }
      case "component.pending": {
        const request = params as PendingRequest | null;
        if (request?.request_id && Array.isArray(request.scopes)) {
          this.#upsertPending(normalizePending(request));
        }
        return false;
      }
      // OUTGOING transfers. End-to-end ordering is guaranteed (a single mpsc
      // queue): `started` always precedes `progress`, `finished`/`failed`.
      case "transfer.started": {
        const p = params as {
          transfer_id?: string;
          device_id?: string;
          files?: TransferFile[];
          total?: number;
        } | null;
        if (typeof p?.transfer_id !== "string" || typeof p.device_id !== "string") {
          return false;
        }
        this.#upsertTransfer(p.transfer_id, {
          device_id: p.device_id,
          files: Array.isArray(p.files) ? p.files : [],
          total: typeof p.total === "number" ? p.total : 0,
          done: 0,
          status: "active",
        });
        return false;
      }
      case "transfer.progress": {
        const p = params as {
          transfer_id?: string;
          done?: number;
          total?: number;
        } | null;
        const t = this.transfers.find((x) => x.transfer_id === p?.transfer_id);
        // No matching `started` (incoming ignored, unknown id): nothing to do.
        if (!t || t.status !== "active") return false;
        if (typeof p?.done === "number") t.done = p.done;
        if (typeof p?.total === "number") t.total = p.total;
        return false;
      }
      case "transfer.finished": {
        const id = (params as { transfer_id?: string } | null)?.transfer_id;
        // Before the tracking lookup: a shared file's copy must be released even
        // if this transfer was never tracked here.
        this.#releaseShare(id);
        const t = this.transfers.find((x) => x.transfer_id === id);
        if (!t) return false;
        t.status = "finished";
        t.done = t.total;
        this.#capTransfers();
        return false;
      }
      case "transfer.failed": {
        const p = params as { transfer_id?: string; error?: string } | null;
        this.#releaseShare(p?.transfer_id);
        const t = this.transfers.find((x) => x.transfer_id === p?.transfer_id);
        if (!t) return false;
        t.status = "failed";
        t.error = typeof p?.error === "string" ? p.error : "";
        this.#capTransfers();
        return false;
      }
      // `transfer.incoming`: the GUI does not show incoming receipts (they land
      // in the download folder, T2 contract). Ignored.
      default:
        return false;
    }
  }

  #upsertTransfer(transfer_id: string, fields: Omit<Transfer, "transfer_id">): void {
    const index = this.transfers.findIndex((t) => t.transfer_id === transfer_id);
    if (index === -1) {
      this.transfers = [...this.transfers, { transfer_id, ...fields }];
      return;
    }
    // Replay of a `started` (rare, ordering is guaranteed): we refresh the
    // manifest, but WITHOUT downgrading the progress (`done`) or the status.
    const existing = this.transfers[index];
    existing.device_id = fields.device_id;
    existing.files = fields.files;
    existing.total = fields.total;
  }

  /** Bounds the history: keeps all the active ones + the N most recent terminal ones. */
  #capTransfers(): void {
    const terminal = this.transfers.filter((t) => t.status !== "active");
    const excess = terminal.length - this.transferHistory;
    if (excess <= 0) return;
    const drop = new Set(
      terminal.slice(0, excess).map((t) => t.transfer_id),
    );
    this.transfers = this.transfers.filter((t) => !drop.has(t.transfer_id));
  }

  #upsertDevice(device: Device): void {
    const index = this.devices.findIndex(
      (d) => d.device_id === device.device_id,
    );
    if (index === -1) this.devices = [...this.devices, device];
    else this.devices[index] = device;
  }

  #upsertPending(request: PendingRequest): void {
    const index = this.pending.findIndex(
      (r) => r.request_id === request.request_id,
    );
    if (index === -1) this.pending = [...this.pending, request];
    else this.pending[index] = request;
  }

  // -- Actions ------------------------------------------------------------
  //
  // None writes to the state: they call the Core, whose notification is
  // authoritative. Only the `components.*` decisions, mute by construction,
  // trigger a resnapshot.

  async #act(action: () => Promise<void>): Promise<void> {
    if (this.busy) return;
    this.busy = true;
    this.notice = null;
    try {
      await action();
    } catch (e) {
      this.notice = { kind: "error", text: humanize(e) };
    } finally {
      this.busy = false;
    }
  }

  login(): Promise<void> {
    return this.#act(async () => {
      // Mobile: the browser is about to take the foreground, and this app's
      // process has to survive long enough for the Core to finish the exchange.
      // Declared BEFORE the round-trip — awaited, so the service is asked for
      // while this app is still the foreground one (Android refuses a foreground
      // service started from the background) — and released if the round-trip
      // never gets under way. The shell expires the hold on its own if we never
      // come back at all.
      await keepAlive("auth", true);
      try {
        const { auth_url } = await api.sessionLogin();
        await openUrl(auth_url);
      } catch (e) {
        void keepAlive("auth", false);
        throw e;
      }
      this.notice = {
        kind: "info",
        text: "Finish signing in through your browser.",
      };
    });
  }

  logout(): Promise<void> {
    return this.#act(async () => {
      await api.sessionLogout();
    });
  }

  // -- Server configuration -----------------------------------------------
  //
  // The GUI writes config.json (shell command), then has the Core re-read it
  // (`session.reload`) — no baked defaults, no process restart. The Core stays
  // the validation authority: a bad config comes back as an error.

  /** The server fields currently written, to pre-fill the setup screen. */
  loadServerConfig(): Promise<ServerConfigInput> {
    return getServerConfig();
  }

  /**
   * Persists the setup screen's fields then applies them live (reload +
   * resync). Returns `true` on success; on failure a banner carries the reason
   * (e.g. the Core rejecting an invalid URL) and the config is left as it was.
   * `busy`-guarded by hand (it returns a value), like the account actions.
   */
  async saveServerConfig(input: ServerConfigInput): Promise<boolean> {
    if (this.busy) return false;
    this.busy = true;
    this.notice = null;
    try {
      await setServerConfig(input);
      await api.sessionReload();
      await this.resync();
      return true;
    } catch (e) {
      this.notice = { kind: "error", text: humanize(e) };
      return false;
    } finally {
      this.busy = false;
    }
  }

  // -- Account (C7 account key) -------------------------------------------
  //
  // No `account.*` notification: the attestation is only read at snapshot time.
  // `createAccount` therefore does NOT resnapshot — otherwise `attested` would
  // flip and the portal would lift, taking away the code (see
  // `onboardingPending`). These actions return a value (code / success), hence
  // `busy` is managed by hand rather than via `#act`.

  /**
   * Creates the account vault (first device). Returns the recovery code — the
   * ONLY copy of the private key, to display once — or `null` on failure (a
   * banner is set). Does not touch `this.account`: it is `finishOnboarding`
   * that, on "Continue", reads its fresh state.
   */
  async createAccount(): Promise<string | null> {
    if (this.busy) return null;
    this.busy = true;
    this.notice = null;
    // Hold the portal BEFORE the round-trip, not after: otherwise a
    // notification received while `account.setup` is in flight would trigger a
    // resync that reads the attestation already set on the Core side (attested
    // becomes true) and would lift the portal — the component would be
    // unmounted and the code, barely returned, lost. Released only if creation
    // fails.
    this.onboardingPending = true;
    try {
      const { recovery_code } = await api.accountSetup();
      return recovery_code;
    } catch (e) {
      this.onboardingPending = false;
      this.notice = { kind: "error", text: humanize(e) };
      return null;
    } finally {
      this.busy = false;
    }
  }

  /**
   * Joins an existing vault with the code from another device. Returns `true`
   * if the attestation took (resnapshot → `attested`, portal lifted), `false`
   * otherwise (banner). No code to remember: we resnapshot right away.
   */
  async joinAccount(recovery_code: string): Promise<boolean> {
    if (this.busy) return false;
    this.busy = true;
    this.notice = null;
    try {
      await api.accountJoin(recovery_code);
      // Resync UNDER the lock (like `#decide`): the button stays disarmed
      // during the confirmation, no double-submission before the portal lifts.
      await this.resync();
      return true;
    } catch (e) {
      this.notice = { kind: "error", text: humanize(e) };
      return false;
    } finally {
      this.busy = false;
    }
  }

  /** Acknowledges the displayed code: the portal can lift, we re-read the state. */
  finishOnboarding(): void {
    this.onboardingPending = false;
    void this.resync();
  }

  renameDevice(device_id: string, name: string): Promise<void> {
    return this.#act(async () => {
      await api.devicesRename(device_id, name);
    });
  }

  revokeDevice(device_id: string): Promise<void> {
    return this.#act(async () => {
      const result = await api.devicesRevoke(device_id);
      if (result.status === "reauth_required") {
        // The server requires a fresh ID token: the browser completion will
        // carry out the revocation, and `device.removed` will follow. Same
        // round-trip as a login, so the same hold on the process — this one comes
        // back to the Core's loopback listener too (mobile only).
        await keepAlive("auth", true);
        await openUrl(result.auth_url);
        this.notice = {
          kind: "info",
          text: "Confirm the revocation in your browser.",
        };
      }
    });
  }

  approve(request_id: string, scopes: string[]): Promise<void> {
    return this.#decide(
      () => api.componentsApprove(request_id, scopes),
      "This request no longer exists.",
    );
  }

  deny(request_id: string): Promise<void> {
    return this.#decide(
      () => api.componentsDeny(request_id),
      "This request no longer exists.",
    );
  }

  revokeComponent(component_id: string): Promise<void> {
    return this.#decide(
      () => api.componentsRevoke(component_id),
      "This component no longer exists.",
    );
  }

  // -- Transfers ----------------------------------------------------------
  //
  // Outside the `busy` lock: multiple concurrent sends (to different devices)
  // are legitimate, and a send must not disarm the management buttons. The
  // tracking state comes only from the `transfer.*` notifications.

  /**
   * `device_id` if it is an eligible send target — online and not this PC —
   * otherwise `null`. The drop hit-test finds some card; this is where we
   * decide whether it can receive.
   */
  targetFor(device_id: string | null): string | null {
    if (!device_id) return null;
    const device = this.devices.find((d) => d.device_id === device_id);
    return device && device.online && !device.is_self ? device_id : null;
  }

  /**
   * Sends `paths` to `device_id`. Tracking arises from `transfer.started`; the
   * returned `transfer_id` is not state, only a handle for a caller that has
   * something to release when the transfer ends ({@link sendShare}).
   */
  async sendFiles(device_id: string, paths: string[]): Promise<string | null> {
    if (paths.length === 0) return null;
    // Each drop starts from a clean banner: the error of a faulty drop (a
    // folder) must not survive a valid drop that follows it.
    this.notice = null;
    // Mobile: until the Core answers, no `transfer.started` has been emitted and
    // nothing holds this process up — while the user has every reason to put the
    // phone down the moment they tapped. Released as soon as the answer is in:
    // by then the transfer, if any, is the shell's to follow.
    await keepAlive("send", true);
    try {
      const { transfer_id } = await api.filesSend(device_id, paths);
      return transfer_id;
    } catch (e) {
      this.notice = { kind: "error", text: humanize(e) };
      return null;
    } finally {
      void keepAlive("send", false);
    }
  }

  /**
   * Sends the pending share to `device_id` — one tap, one destination, and the
   * share is spent. Its cache copy is released when the transfer ends, whichever
   * way it ends.
   */
  async sendShare(device_id: string): Promise<void> {
    const share = this.pendingShare;
    if (!share) return;
    // Consumed here, not on the way back: a second tap must not send twice.
    this.pendingShare = null;
    // Before anything can await: from now on this share is answered, and a share
    // arriving meanwhile must not mistake it for one still waiting and delete the
    // files the transfer is about to read.
    void shareTaken(share.id);
    const transfer_id = await this.sendFiles(
      device_id,
      share.files.map((file) => file.path),
    );
    if (transfer_id === null) {
      // Refused outright (offline device, revoked...): the bytes are of no
      // further use, and the banner already carries the reason.
      void discardShare(share.id);
      return;
    }
    this.#shareOfTransfer.set(transfer_id, share.id);
    // The transfer may have ended while we awaited the response — its
    // notifications ride the same queue as that response. No second
    // `finished`/`failed` will come, so release it now.
    const known = this.transfers.find((t) => t.transfer_id === transfer_id);
    if (known && known.status !== "active") this.#releaseShare(transfer_id);
  }

  /** Gives up on the pending share (the picker's "Cancel"). */
  cancelShare(): void {
    this.#dropPending();
  }

  /** Releases the share a finished transfer was carrying, if it carried one. */
  #releaseShare(transfer_id: string | undefined): void {
    if (transfer_id === undefined) return;
    const share = this.#shareOfTransfer.get(transfer_id);
    if (share === undefined) return;
    this.#shareOfTransfer.delete(transfer_id);
    void discardShare(share);
  }

  /** Cancels a transfer. The outcome (`failed`/`finished`) will come by notification. */
  async cancelTransfer(transfer_id: string): Promise<void> {
    try {
      await api.filesCancel(transfer_id);
    } catch (e) {
      // Already finished between display and click: nothing to report.
      if (isCoreError(e) && e.data_code === "TRANSFER_UNKNOWN") return;
      this.notice = { kind: "error", text: humanize(e) };
    }
  }

  /** Removes a finished transfer from the list (the card's "×" button). */
  dismissTransfer(transfer_id: string): void {
    this.transfers = this.transfers.filter(
      (t) => t.transfer_id !== transfer_id,
    );
  }

  #decide(call: () => Promise<unknown>, staleText: string): Promise<void> {
    return this.#act(async () => {
      try {
        await call();
      } catch (e) {
        if (!isInvalidParams(e)) throw e;
        // Target vanished: the Core does not notify these queue exits, only a
        // resnapshot sees them.
        await this.resync();
        this.notice = { kind: "error", text: staleText };
        return;
      }
      await this.resync();
    });
  }
}
