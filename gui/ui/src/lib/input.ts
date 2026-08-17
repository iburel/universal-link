// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

/**
 * The words and the geometry of keyboard and mouse sharing: pure functions over
 * the `input.*` state (doc/input-sharing.md, sections 6, 7, 12 and 13).
 *
 * Two rules hold this file together.
 *
 * **Nothing here invents state.** Every sentence is a translation of a code the
 * engine emitted, and every rectangle comes from `plane.spots`. What is computed
 * here and nowhere else is the arrangement a human is DRAGGING, which is not
 * state until they let go of it.
 *
 * **A refusal is never softened.** The engine detects rather than guesses (that
 * is the whole point of the feature), so a sentence says what did not happen and
 * why, never "may not have". A code with no sentence would be a keystroke that
 * went nowhere in silence, which is the one thing this feature must not do:
 * hence {@link refusalSentence}'s fallback, which carries an unknown code rather
 * than dropping it.
 */

import type {
  InputGuards,
  InputHere,
  InputSpot,
  InputState,
  PlacedSpot,
} from "./api";
import { isCoreError } from "./errors";

// --- The pointer thresholds (doc/input-sharing.md, section 14) ---------------

/** Under this round trip, the pointer goes over without a word. */
export const POINTER_SILENT_MS = 10;
/**
 * Past this, the pointer is worth a question before it is handed over. The
 * decided bands are silent under 10 and refused above 60, and the epic's own
 * reading of the middle is that "typing over a relay is genuinely usable (it
 * feels like SSH), a pointer past roughly 40 ms is not": so 10 to 40 announces
 * the number and goes, and 40 to 60 asks first. Announcing a measured 12 ms and
 * then stopping to ask about it would be a warning nobody needs, on the ordinary
 * wide area path (#123 measured 4 ms direct, 32 relayed).
 */
export const POINTER_WARN_MS = 40;
/** Above this, the pointer is refused and the keyboard alone is offered. */
export const POINTER_MAX_MS = 60;

export type PointerVerdict =
  | "unknown"
  | "silent"
  | "announce"
  | "warn"
  | "refuse";

/**
 * What the measured round trip means for the pointer. The number comes from the
 * engine's own probe on the live channel (D2: the Core exposes no round trip),
 * so `null` means "nothing measured yet", never "fast".
 */
export function pointerVerdict(rtt: number | null | undefined): PointerVerdict {
  if (typeof rtt !== "number" || !Number.isFinite(rtt)) return "unknown";
  if (rtt <= POINTER_SILENT_MS) return "silent";
  if (rtt <= POINTER_WARN_MS) return "announce";
  if (rtt <= POINTER_MAX_MS) return "warn";
  return "refuse";
}

/**
 * The path and its latency, for a pair. Said whenever there is a number: the
 * relay is allowed here but never silently, and "on this network" is the one
 * thing the Core itself says about the route (`lan`).
 *
 * With nothing measured it says only that, and does NOT name a route: `lan` is
 * first-hand (this machine hears that one on the local network) but its absence
 * says nothing about how a session would travel, and "over the internet" about a
 * computer that has never answered would be an invention.
 */
export function pathLine(peer: {
  rtt_ms: number | null;
  lan: boolean;
}): string | null {
  if (typeof peer.rtt_ms !== "number") {
    return peer.lan
      ? "On this network. Nothing measured yet."
      : "Nothing measured yet.";
  }
  const number = `${peer.rtt_ms} ms away, ${peer.lan ? "on this network" : "over the internet"}`;
  switch (pointerVerdict(peer.rtt_ms)) {
    case "refuse":
      return `${number}. Too far for the pointer to feel right; its keyboard alone would work.`;
    case "warn":
      return `${number}. ${SLOW_POINTER}`;
    case "announce":
      return `${number}. The pointer will lag a little.`;
    default:
      return `${number}.`;
  }
}

/** One wording for the same fact, wherever it is said. */
const SLOW_POINTER =
  "The pointer will lag noticeably at that distance; its keyboard alone would feel normal.";

/** The warning shown BEFORE the pointer is handed across a slow path. */
export function slowPathWarning(name: string, rtt: number): string {
  return `${name} is ${rtt} ms away. ${SLOW_POINTER}`;
}

// --- The refusals (doc/input-sharing.md, section 13) ------------------------

/**
 * Every code that can reach this interface, and the sentence for it. The keys
 * are the engine's vocabulary verbatim: the dialect's own refusals, the `oops`
 * codes a target reports about an injection that did not happen, the words a
 * session ends with, and the codes the Core mints when a live channel cannot be
 * opened at all.
 *
 * `<name>` is replaced by the other computer's name, or by "That computer" when
 * the directory does not name it. Nothing else is substituted: a sentence a peer
 * could shape is a sentence a peer could write.
 */
const REFUSALS: Record<string, string> = {
  // The dialect's five: the far side said no, and this is its word.
  NOT_ALLOWED:
    "<name> has not been told to accept your keyboard. Allow it there.",
  BUSY: "Another of your computers is using <name>'s keyboard right now.",
  PLANE_STALE:
    "The two computers do not agree on where the screens are yet. Trying again.",
  NO_BACKEND: "<name> cannot be driven: nothing there can type.",
  // One cause, and not the one it looks like: `refused::LOCKED` and
  // `ended::LOCKED` are both sent from `input.lock` alone (verified in
  // input/src/session.rs), never by a locked screen, which reports itself as
  // `SCREEN_LOCKED`. So the sentence names the pin and the switch that undoes it.
  LOCKED:
    "<name>'s pointer is pinned to its own screen, so it cannot be driven. That switch is on <name>.",
  // The channel could not be opened. `NO_DIRECT_PATH` is the deployment whose
  // relays introduce devices without carrying a session (#88), and it is a
  // property of the PAIR rather than of either device.
  NO_DIRECT_PATH:
    "This account's relays do not carry a keyboard session. <name> and this computer need a network they share, or a VPN between them.",
  COMPONENT_ABSENT:
    "<name> is not running the keyboard and mouse engine right now.",
  // The everyday one, and it was the one with no sentence: a computer whose
  // record still carries a route (a relay, or a signed address) reads as
  // reachable while it is asleep, and the channel to it times out.
  DEVICE_OFFLINE:
    "<name> is not answering right now. It may be asleep, off, or on another network.",
  DEVICE_UNKNOWN: "<name> is no longer one of your account's computers.",
  // What every code outside a frame's closed set becomes on arrival, so a later
  // engine's vocabulary reaches a person as a sentence rather than as prose a
  // peer chose.
  UNKNOWN: "<name> refused, and this version does not know the reason it gave.",
  OPEN_REFUSED: "<name> did not accept a live channel.",
  OPEN_FAILED: "The live channel to <name> could not be opened.",
  // What the target says about an injection that did not happen. Each of these
  // is DETECTED, which is why it can be said at all.
  //
  // These five name NO machine, and that is not laziness: the engine emits them
  // on BOTH ends of a session (the target says them about itself, and forwards
  // them to the source in an `oops` frame), while the notification carries a
  // device_id and no direction. A sentence naming the device would name the wrong
  // machine on one of the two sides every time. "The computer being driven" is
  // true wherever it is read, and the epic's own wording is machine-free for the
  // same reason.
  ELEVATED_WINDOW: "Nothing was typed: this window runs as administrator.",
  SECURE_INPUT:
    "Nothing was typed: password fields block synthetic keystrokes on macOS.",
  // Windows reports its secure desktop the same way it reports a locked screen
  // (`refusal_for` in input/src/windows.rs), and a UAC prompt is the common one of
  // the two, so the sentence names both rather than sending somebody to unlock a
  // machine that is not locked.
  SCREEN_LOCKED:
    "Nothing was typed: a computer cannot be driven while it is locked, and Windows reports a security prompt the same way.",
  NO_PERMISSION:
    "Nothing was typed: 1Device is not allowed to type on the computer being driven.",
  UNRESOLVED:
    "That key does not exist on the keyboard of the computer being driven.",
  // How a session ended, when nobody asked it to.
  IDLE: "Your keyboard went quiet, so <name> released it.",
  TAKEN: "Someone is using <name> directly.",
  REVOKED: "<name> took its permission back, so your keyboard came back.",
  SLOW: "The connection to <name> slowed down, so your keyboard came back.",
  // Three causes under one code, two of them local (this computer's capture died,
  // or its own permission to capture went away) and one remote (the channel
  // ended). So it blames nobody: the local half, when there is one, is said by
  // this computer's own problem line right above.
  GONE: "Your keyboard came back: the session with <name> could not go on.",
  // Not a refusal: the channel is not warm yet. Said because a crossing that
  // did not happen needs a reason, and "wait a moment" is the true one.
  NOT_WARM: "<name> is not ready yet. Try again in a moment.",
};

/**
 * The sentence for a refusal code. A code this version does not know still gets
 * one, and carries the code: a later engine's vocabulary must degrade to
 * something a person can repeat, not to silence. The code is shown as it
 * arrived, never interpreted, which is also why no peer-authored prose is ever
 * rendered here.
 */
export function refusalSentence(code: string, name?: string | null): string {
  const who = name && name.length > 0 ? name : "That computer";
  const known = REFUSALS[code];
  if (known) return known.replaceAll("<name>", who);
  // `ATTACH_FAILED: <reason>` is the one code with a payload; its head is what
  // identifies it.
  if (code.startsWith("ATTACH_FAILED")) {
    return `The live channel to ${who} could not be attached.`;
  }
  return `${who} refused, and this version does not know the reason it gave (${code}).`;
}

/** Every code this build has a sentence for, so a test can prove the coverage. */
export const REFUSAL_CODES: readonly string[] = Object.keys(REFUSALS);

/**
 * A pair's standing problem, from the snapshot: what the interface says next to
 * a device even when no refusal has just arrived. These are the collapsed set
 * (section 12), which is why `busy` names both directions.
 */
const PEER_PROBLEMS: Record<string, string> = {
  not_allowed:
    "<name> has not been told to accept your keyboard. Allow it there, on <name> itself.",
  busy: "<name> is already being driven, or is driving another computer.",
  locked:
    "<name>'s pointer is pinned to its own screen, so it cannot be driven. That switch is on <name>.",
  no_backend: "Nothing on <name> can type, so it cannot be driven.",
  no_path:
    "This account's relays do not carry a keyboard session, and no direct path to <name> was found. The same network, or a VPN between them, would give it one.",
  too_slow:
    "The path to <name> is too slow for the pointer. Its keyboard alone would work.",
  plane_stale:
    "This computer and <name> do not agree on where the screens are yet. They are already sorting it out.",
};

export function peerProblemSentence(
  problem: string | null | undefined,
  name?: string | null,
): string | null {
  if (!problem) return null;
  const who = name && name.length > 0 ? name : "That computer";
  const known = PEER_PROBLEMS[problem];
  if (known) return known.replaceAll("<name>", who);
  return `${who} reported a problem this version does not know (${problem}).`;
}

/**
 * What THIS computer cannot do, and what to do about it. A permission that is
 * missing is explained rather than hidden, and the two halves are named
 * separately because on a Mac they are two separate grants: a machine can be
 * drivable and not a driver, or the other way round, and telling someone "check
 * your permissions" when only one of two is missing sends them looking in the
 * wrong pane.
 *
 * `platform` is the self device's own record (`devices.list`), so the macOS
 * wording only ever appears on a Mac.
 */
export function hereProblemSentence(
  here: Pick<InputHere, "problem" | "can_drive" | "can_be_driven">,
  platform?: string,
): string | null {
  const mac = platform === "macos";
  switch (here.problem) {
    case null:
    case undefined:
      return null;
    case "no_permission": {
      // The normal state of a fresh Mac: Input Monitoring granted at the first
      // dialog, Accessibility not. Each half names its own pane, because they
      // are two different lists in System Settings.
      const cannotType = !here.can_be_driven;
      const cannotRead = !here.can_drive;
      const settings = mac
        ? " Open System Settings, Privacy and Security, then"
        : "";
      if (cannotType && cannotRead) {
        return `1Device has neither permission it needs on this computer, so it can neither drive another nor be driven.${settings}${mac ? " switch 1Device on under both Input Monitoring and Accessibility." : ""} It starts working within a second of the switch, with no restart.`;
      }
      if (cannotType) {
        return `1Device may not type on this computer, so no other computer can drive it. Your keyboard can still drive them.${settings}${mac ? " switch 1Device on under Accessibility." : ""} It starts working within a second of the switch, with no restart.`;
      }
      if (cannotRead) {
        return `1Device may not read this computer's keyboard and mouse, so it cannot drive another computer. It can still be driven.${settings}${mac ? " switch 1Device on under Input Monitoring." : ""} It starts working within a second of the switch, with no restart.`;
      }
      // Reported, and yet both halves work: say so plainly rather than dress it
      // up. It happens while a grant is being noticed.
      return "A permission this computer needs was refused, and both halves are working for now.";
    }
    case "no_backend":
      return "Nothing on this computer can read the keyboard or type on it, so it can neither drive another computer nor be driven.";
    case "wayland":
      return "This is a Wayland session, and 1Device has no way into this one. An X11 session on the same computer works today; everything else about this computer keeps working either way.";
    // The Wayland family below. Each says what is missing and what would fix it,
    // because "this is a Wayland session" on its own is advice nobody can act on:
    // it names no remedy, and on a desktop that has everything it is simply false.
    case "xwayland":
      return "This is a Wayland session and 1Device is going through its X11 half. Windows that are X11 programs can be read and typed into; windows that speak Wayland directly, which on most desktops is nearly all of them, cannot. A real X11 session is what works completely today.";
    case "wayland_no_bus":
      return "This Wayland session has no D-Bus session bus, so 1Device cannot ask the desktop for permission to share the keyboard and mouse. A session started the way your desktop normally starts one has that bus.";
    case "wayland_no_portal": {
      // Which half is missing is in the capability bits, not in the code: one
      // desktop can offer the capture portal and not the injection one (Hyprland
      // does exactly that today), and sending somebody to look for the wrong
      // package is worse than saying nothing.
      const half = !here.can_drive && !here.can_be_driven ? "neither half" : !here.can_drive ? "the half that reads your keyboard" : "the half that types on this computer";
      return `This desktop does not offer ${half} of what 1Device needs to share a keyboard and mouse on Wayland (the input portals). GNOME 45 and later, and KDE Plasma 6.1 and later, have them. An X11 session works today on any desktop.`;
    }
    case "wayland_portal_old":
      return "This desktop's keyboard and mouse portals are older than 1Device can talk to. A newer xdg-desktop-portal, together with a newer portal for your own desktop, is what fixes it.";
    case "wayland_portal_refused":
      return "Permission to share this computer's keyboard and mouse was not granted, so nothing was captured and nothing was typed. Switching the feature on again asks your desktop for it again.";
    case "wayland_untested":
      return "This desktop has everything 1Device needs to share a keyboard and mouse on Wayland, and that path has never been run against a real desktop yet, so it stays switched off rather than claiming to work. An X11 session is the proven one. Setting ONEDEVICE_INPUT_WAYLAND=1 before 1Device starts turns the Wayland path on if you want to try it.";
    case "monitors_unstable":
      return "This computer's screens cannot be told apart for certain, so they may swap places on the plane after one is unplugged. Dragging them back is what fixes it.";
    default:
      return `This computer reported a problem this version does not know (${here.problem}).`;
  }
}

/** A ghost's own sentence: the epic's words, and it keeps its place. */
export const GHOST_SENTENCE =
  "This screen is not connected right now. Its place is kept.";

/**
 * A gesture's refusal, as a sentence. These are the engine's own codes, and they
 * are all about THIS machine (D21: a gesture answers only with what this
 * computer knows, so nothing here can be a cached word about the far side).
 */
const GESTURES: Record<string, string> = {
  INPUT_NOT_READY:
    "1Device is still working out which devices are on your account. Try again in a moment.",
  INPUT_DEVICE_UNKNOWN: "That device is not on your account.",
  INPUT_BUSY:
    "This computer is already driving another one, or is being driven. There is one keyboard, and it is not taken from anybody by surprise.",
  INPUT_LOCKED:
    "This computer's pointer is pinned to its own screen. Switch that off first.",
  INPUT_NO_BACKEND:
    "This computer cannot read your keyboard and mouse, so it has none to send.",
  INPUT_TOO_SLOW:
    "That computer is too far away for the pointer to feel right. Its keyboard alone would work.",
  INPUT_UNKNOWN_MONITOR:
    "The screens have moved since this was on screen: there is no crossing there any more.",
  // Both reachable causes are permanent (a counter at its ceiling, a stored table
  // that is full), so "try again" would be advice that cannot work.
  INPUT_INTERNAL:
    "1Device could not record that. Its own limit for this was reached, and repeating the gesture will not change it.",
  COMPONENT_ABSENT:
    "The keyboard and mouse engine is not running on this computer right now.",
};

/**
 * The sentence for a failed `input.*` gesture, or `null` when the caller should
 * fall back to the shared translation (`humanize`). A `-32602` is deliberately
 * not here: a malformed request is this interface's own bug, and dressing it as
 * advice to the user would hide it.
 */
export function gestureFailure(e: unknown): string | null {
  if (!isCoreError(e) || e.kind !== "rpc") return null;
  const code = e.data_code;
  if (code && GESTURES[code]) return GESTURES[code];
  return null;
}

// --- The live state ---------------------------------------------------------

/**
 * What is happening right now, in the two sentences of the epic. The source says
 * where its keyboard went and how to get it back (the hotkey is enforced by THIS
 * machine, so naming it is a promise this machine can keep); the target says who
 * is using it.
 */
export function sessionSentence(
  session: InputState["session"],
  name: string | null,
  hotkey: string[],
): string | null {
  if (!session) return null;
  const who = name && name.length > 0 ? name : "another computer";
  if (session.direction === "out") {
    const keys = hotkeyLabel(hotkey);
    const what =
      session.mode === "keys"
        ? `Your keyboard is on ${who}`
        : `Your keyboard and mouse are on ${who}`;
    const far =
      pointerVerdict(session.rtt_ms) === "announce" ||
      pointerVerdict(session.rtt_ms) === "refuse"
        ? `, ${session.rtt_ms} ms away`
        : "";
    return `${what}${far}. Press ${keys} to bring them back.`;
  }
  return session.mode === "keys"
    ? `${who} is using your keyboard right now.`
    : `${who} is using your keyboard and mouse right now.`;
}

/** "Ctrl + Alt + Escape". The engine's canonical names, spelled for a human. */
const KEY_NAMES: Record<string, string> = {
  ctrl: "Ctrl",
  alt: "Alt",
  altgr: "AltGr",
  shift: "Shift",
  meta: "Meta",
};

export function hotkeyLabel(keys: readonly string[]): string {
  if (keys.length === 0) return "the return hotkey";
  return keys.map((key) => KEY_NAMES[key] ?? key).join(" + ");
}

// --- The guards, in plain words (section 7) ---------------------------------

/** The engine's canonical modifier bits (`mods::` in input/src/keys.rs). */
export const MODS = {
  shift: 1 << 0,
  ctrl: 1 << 1,
  alt: 1 << 2,
  altgr: 1 << 3,
  meta: 1 << 4,
} as const;

export interface GuardValues {
  dwell_ms: number;
  double_tap_ms: number;
  dead_corner: number;
  require_mods: number;
  wall: boolean;
}

/** The defaults the engine uses for a pair nobody has set anything on. */
export const GUARD_DEFAULTS: GuardValues = {
  dwell_ms: 250,
  double_tap_ms: 0,
  dead_corner: 16,
  require_mods: 0,
  wall: false,
};

/**
 * How long the pointer must rest against an edge, offered in words. A
 * millisecond count is not what a person is choosing here, so the choice is
 * three intentions; the number behind each is the engine's (250 ms is where the
 * prior art converged).
 */
export const DWELL_CHOICES: readonly { ms: number; label: string }[] = [
  { ms: 0, label: "As soon as the pointer touches the edge" },
  { ms: 250, label: "After a short pause at the edge" },
  { ms: 600, label: "After a deliberate pause at the edge" },
];

/** The stored dwell, snapped to the offered choice at or below it. */
export function dwellChoice(ms: number): number {
  let chosen = DWELL_CHOICES[0].ms;
  for (const choice of DWELL_CHOICES) if (ms >= choice.ms) chosen = choice.ms;
  return chosen;
}

/** The modifier a crossing requires, in words. `null` when it requires none. */
export function modifierWords(bits: number): string | null {
  const held = Object.entries(MODS)
    .filter(([, bit]) => (bits & bit) !== 0)
    .map(([name]) => KEY_NAMES[name] ?? name);
  return held.length === 0 ? null : held.join(" + ");
}

/**
 * What a crossing's guards do, as sentences, in the order the engine asks them
 * (section 7: wall, dead corner, required modifier, double tap, dwell), because
 * that is the order a refusal is reported in.
 */
export function guardWords(
  guards: Partial<GuardValues>,
  ghost = false,
): string[] {
  // A crossing into a screen that is away is a wall whatever the guards say, so
  // saying "after a short pause at the edge" would be describing something that
  // cannot happen. The place is kept and the pointer stops there.
  if (ghost) {
    return [
      "The pointer stops at that edge: the screen across it is not connected right now.",
    ];
  }
  const g = { ...GUARD_DEFAULTS, ...guards };
  if (g.wall) return ["The pointer never crosses here."];
  const words: string[] = [];
  if (g.dead_corner > 0) {
    words.push("The corners of the edge are left alone, for menus and hot corners.");
  }
  const mods = modifierWords(g.require_mods);
  if (mods) words.push(`Only while ${mods} is held.`);
  if (g.double_tap_ms > 0) {
    words.push("Only after the pointer leaves the edge and comes straight back.");
  }
  const offered = DWELL_CHOICES.find((c) => c.ms === g.dwell_ms);
  // A dwell set elsewhere (a third-party interface, a hand-edited file) is shown
  // as the number it is rather than rounded into one of the three intentions.
  words.push(offered ? `${offered.label}.` : `After ${g.dwell_ms} ms at the edge.`);
  return words;
}

// --- The plane (section 6) --------------------------------------------------

/** The tolerance that makes an imported arrangement abut (section 7). */
export const SNAP = 16;
/** The engine refuses a spot beyond this (`MAX_EXTENT` in input/src/plane.rs). */
export const PLANE_EXTENT = 1_000_000;

export type Side = "left" | "right" | "top" | "bottom";

export interface Crossing {
  /** Our screen the pointer leaves by. */
  from: string;
  /** The neighbour's screen it arrives on, and what `input.guards` names. */
  to: string;
  device_id: string | null;
  side: Side;
  /** Logical pixels of edge the two screens actually share. */
  length: number;
  /** The neighbour is a ghost: this crossing is a wall until it comes back. */
  ghost: boolean;
}

/** The node_id half of a spot key, which is what groups a machine's screens. */
export function nodeOfSpot(key: string): string {
  const cut = key.indexOf("/");
  return cut === -1 ? key : key.slice(0, cut);
}

/**
 * The monitor's own id, the other half of a spot key. Split at the FIRST slash,
 * exactly as the engine splits it (`split_spot_key`), which also REFUSES an id
 * containing one: the tail is the whole id, and a key with two slashes is a key
 * the engine would never have minted and would refuse on the way back.
 */
export function monitorOfSpot(key: string): string {
  const cut = key.indexOf("/");
  return cut === -1 ? "" : key.slice(cut + 1);
}

/**
 * A machine's screens put back the way that machine itself has them: its own
 * desktop arrangement (which is in the snapshot, as that machine's own word
 * about its own screens), translated so the block keeps the corner it already
 * occupies on the plane. Nothing else moves, and the result is the whole set of
 * spots, because `input.place` replaces the placement.
 *
 * This is the way back from having scattered a block, and the reason it exists:
 * an arrangement is imported rather than re-invented, so re-inventing it by hand
 * afterwards should never be the only option.
 */
export function reimportBlock(
  spots: readonly InputSpot[],
  device_id: string,
  own: readonly { id: string; x: number; y: number }[],
): DropOutcome {
  const mine = spots.filter((s) => s.device_id === device_id);
  if (mine.length === 0 || own.length === 0) {
    return { ok: true, spots: spots.map((s) => ({ monitor: s.monitor, x: s.x, y: s.y })) };
  }
  const corner = {
    x: Math.min(...mine.map((s) => s.x)),
    y: Math.min(...mine.map((s) => s.y)),
  };
  const origin = {
    x: Math.min(...own.map((m) => m.x)),
    y: Math.min(...own.map((m) => m.y)),
  };
  const at = new Map(
    own.map((m) => [
      m.id,
      { x: corner.x + (m.x - origin.x), y: corner.y + (m.y - origin.y) },
    ]),
  );
  // Through the same door as a drag, deliberately: a block put back can land on
  // top of a screen that moved in while it was scattered, and an overlap is an
  // overlap however it was made.
  return settle(
    spots,
    new Map(
      spots.flatMap((s) => {
        const back = s.device_id === device_id ? at.get(monitorOfSpot(s.monitor)) : undefined;
        return back ? [[s.monitor, back] as const] : [];
      }),
    ),
  );
}

/**
 * Every spot of the machine that owns `key`: its own arrangement, which moves as
 * one block. The node_id is the grouping and not `device_id`, because a spot of a
 * device this directory does not name still belongs to exactly one machine.
 */
export function blockKeys(spots: readonly InputSpot[], key: string): string[] {
  const node = nodeOfSpot(key);
  return spots.filter((s) => nodeOfSpot(s.monitor) === node).map((s) => s.monitor);
}

export interface Bounds {
  x: number;
  y: number;
  w: number;
  h: number;
}

/** The plane's bounding box, for drawing it. Empty is 0 by 0. */
export function planeBounds(spots: readonly InputSpot[]): Bounds {
  if (spots.length === 0) return { x: 0, y: 0, w: 0, h: 0 };
  const left = Math.min(...spots.map((s) => s.x));
  const top = Math.min(...spots.map((s) => s.y));
  const right = Math.max(...spots.map((s) => s.x + s.w));
  const bottom = Math.max(...spots.map((s) => s.y + s.h));
  return { x: left, y: top, w: right - left, h: bottom - top };
}

function overlaps(a: Bounds, b: Bounds): boolean {
  return (
    a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
  );
}

/**
 * The crossings the plane implies, derived exactly as the engine derives them
 * (section 7): a neighbour whose facing edge is within {@link SNAP} of ours, and
 * a shared stretch of edge with a positive length. A stretch with nothing across
 * it is a wall and appears here as nothing at all.
 *
 * Computed here as well as there because an interface that offered guards for a
 * crossing the engine does not have would be offering a setting that can never
 * apply. When the two disagree the engine refuses the write
 * (`INPUT_UNKNOWN_MONITOR`), which is said out loud rather than swallowed.
 *
 * One deliberate difference: the engine's SEGMENT set has nothing into a ghost at
 * all, while this returns it marked `ghost`. That is the same fact from the other
 * side (the engine calls it a wall whose reason is that the screen is away), and
 * an interface needs the entry in order to say so. What it must not do is offer
 * guards on one, which the engine would refuse.
 */
export function crossings(
  spots: readonly InputSpot[],
  ours: readonly string[],
): Crossing[] {
  const out: Crossing[] = [];
  const mine = new Set(ours);
  // The stretch two intervals share: the crossing segment on one axis.
  const shared = (a: number, b: number, c: number, d: number) =>
    Math.min(a + b, c + d) - Math.max(a, c);
  for (const from of spots) {
    if (!mine.has(from.monitor)) continue;
    for (const to of spots) {
      if (nodeOfSpot(to.monitor) === nodeOfSpot(from.monitor)) continue;
      const rows = shared(from.y, from.h, to.y, to.h);
      const cols = shared(from.x, from.w, to.x, to.w);
      const abut = (edge: number, facing: number) =>
        Math.abs(facing - edge) <= SNAP;
      const found: Side | null = abut(from.x + from.w, to.x) && rows > 0
        ? "right"
        : abut(from.x, to.x + to.w) && rows > 0
          ? "left"
          : abut(from.y + from.h, to.y) && cols > 0
            ? "bottom"
            : abut(from.y, to.y + to.h) && cols > 0
              ? "top"
              : null;
      if (!found) continue;
      out.push({
        from: from.monitor,
        to: to.monitor,
        device_id: to.device_id,
        side: found,
        length: found === "right" || found === "left" ? rows : cols,
        ghost: !to.present,
      });
    }
  }
  return out;
}

export type DropOutcome =
  | { ok: true; spots: PlacedSpot[] }
  | { ok: false; reason: "overlap" | "off_plane" };

/**
 * The arrangement a drag would produce: `keys` moved by `dx`, `dy` in plane
 * pixels, the block kept rigid, and each moved edge that lands within
 * {@link SNAP} of a stationary one snapped exactly onto it, so a crossing forms
 * instead of a gap nobody can see.
 *
 * It refuses two things rather than letting them through:
 *
 * - an **overlap**, because the engine derives no crossing from overlapping
 *   rectangles (section 7): the drop would silently take away the crossing the
 *   person was trying to make, and nothing on screen would say why;
 * - a spot **off the plane**, which the engine answers with `-32602`.
 *
 * Returns every spot the SNAPSHOT carries, which is what `input.place` needs
 * (it replaces the placement, so a spot left out loses its place, and a ghost's
 * place is exactly what must not be lost). One caveat that is not this file's to
 * fix: the snapshot itself omits the spots of a device whose own signed word
 * about its screens this computer cannot verify yet, so a placement written from
 * here cannot carry those. See the note in the PR for #127.
 */
export function dropSpots(
  spots: readonly InputSpot[],
  keys: readonly string[],
  dx: number,
  dy: number,
): DropOutcome {
  // A delta that is not a number cannot be reasoned about, and `Math.abs(NaN)`
  // passes every bound below: it would reach the engine as a JSON `null` and come
  // back as a shape error, which is a lie about what happened.
  if (!Number.isFinite(dx) || !Number.isFinite(dy)) {
    return { ok: false, reason: "off_plane" };
  }
  const moving = new Set(keys);
  const moved = spots.filter((s) => moving.has(s.monitor));
  const still = spots.filter((s) => !moving.has(s.monitor));
  const shifted = moved.map((s) => ({
    ...s,
    x: Math.round(s.x + dx),
    y: Math.round(s.y + dy),
  }));

  // One snap for the whole block, the smallest on each axis: a block is rigid,
  // so an offset found on one of its screens moves all of them.
  const snap = (axis: "x" | "y", size: "w" | "h", cross: "y" | "x", crossSize: "h" | "w") => {
    let best: number | null = null;
    for (const a of shifted) {
      for (const b of still) {
        // Two arrangements, and they want different candidates. Side by side
        // (the other axis overlaps): the two FACING edges, which is what makes a
        // crossing, plus lining the near edges up. Stacked (the other axis abuts
        // within the tolerance): lining the near edges up is the only thing left,
        // and it is what tidies a block dropped under another one. A screen
        // nowhere near it on either axis is not a snap at all.
        const overlapping =
          a[cross] < b[cross] + b[crossSize] && b[cross] < a[cross] + a[crossSize];
        const abutting =
          Math.abs(b[cross] - (a[cross] + a[crossSize])) <= SNAP ||
          Math.abs(a[cross] - (b[cross] + b[crossSize])) <= SNAP;
        if (!overlapping && !abutting) continue;
        const candidates = overlapping
          ? [
              b[axis] + b[size] - a[axis],
              b[axis] - (a[axis] + a[size]),
              b[axis] - a[axis],
            ]
          : [b[axis] - a[axis]];
        for (const delta of candidates) {
          // `null` and not `0` as the sentinel: a drop that landed exactly right
          // has a delta of zero, and a sentinel of zero let a 10 pixel candidate
          // overrule it and open a gap where the human had aimed true.
          if (
            Math.abs(delta) <= SNAP &&
            (best === null || Math.abs(delta) < Math.abs(best))
          ) {
            best = delta;
          }
        }
      }
    }
    return best ?? 0;
  };
  const ax = snap("x", "w", "y", "h");
  const ay = snap("y", "h", "x", "w");
  return settle(
    spots,
    new Map(shifted.map((s) => [s.monitor, { x: s.x + ax, y: s.y + ay }])),
  );
}

/**
 * The one door every arrangement this interface produces goes through: the moved
 * screens at their new corners, checked, and the WHOLE set returned in the
 * snapshot's own order.
 *
 * Two refusals, and they are the same two whether a human dragged a screen or
 * asked for a block to be put back:
 *
 * - an **overlap**, because the engine derives no crossing from overlapping
 *   rectangles (section 7), so the result would silently take away the crossing
 *   the person was making and nothing on screen would say why;
 * - a spot **off the plane**, which the engine answers with `-32602`.
 *
 * The whole set, because `input.place` REPLACES the placement: a spot left out
 * would lose its place, and a ghost's place is exactly what must not be lost.
 */
function settle(
  spots: readonly InputSpot[],
  moved: ReadonlyMap<string, { x: number; y: number }>,
): DropOutcome {
  const placed = spots.map((s) => {
    const at = moved.get(s.monitor);
    return { ...s, x: at?.x ?? s.x, y: at?.y ?? s.y };
  });
  const before = new Map(spots.map((s) => [s.monitor, s]));
  // Only what this gesture MOVED is judged. A plane that arrived already
  // overlapping (dragged on another computer, or written by another interface)
  // must stay draggable, or the one gesture that could repair it would be the one
  // gesture refused.
  for (const a of placed) {
    if (!moved.has(a.monitor)) continue;
    if (
      !Number.isFinite(a.x) ||
      !Number.isFinite(a.y) ||
      // The corner, and only the corner: that is what the engine bounds
      // (`MAX_EXTENT` in input/src/plane.rs), and refusing a plane it would
      // accept is not this file's job.
      Math.abs(a.x) > PLANE_EXTENT ||
      Math.abs(a.y) > PLANE_EXTENT
    ) {
      return { ok: false, reason: "off_plane" };
    }
    for (const b of placed) {
      if (a.monitor === b.monitor || !overlaps(a, b)) continue;
      // An overlap that was already there is not this gesture's doing: a rigid
      // block keeps its own shape, so its internal pairs are exactly as they
      // arrived, while a block being put back really does rearrange itself and is
      // judged on the result.
      const was = { a: before.get(a.monitor), b: before.get(b.monitor) };
      if (was.a && was.b && overlaps(was.a, was.b)) continue;
      return { ok: false, reason: "overlap" };
    }
  }
  return {
    ok: true,
    spots: placed.map((s) => ({ monitor: s.monitor, x: s.x, y: s.y })),
  };
}

/**
 * A crossing whose shared stretch is not longer than the dead corners at both
 * ends of it admits nothing at all: the engine shrinks the segment by
 * `dead_corner` on each side and a pointer never lands inside what is left. Said
 * out loud, because the remedy is one of the toggles right beside it.
 */
export function tooShortToCross(
  length: number,
  guards: Partial<GuardValues>,
): string | null {
  const corner = { ...GUARD_DEFAULTS, ...guards }.dead_corner;
  if (corner <= 0 || length > 2 * corner) return null;
  return `Only about ${length} pixels of that edge are shared, which the corners left alone use up: the pointer cannot cross there. Untick the corners, or line the two screens up more.`;
}

/** Why a drop was refused, as a sentence. */
export function dropRefusal(reason: "overlap" | "off_plane"): string {
  return reason === "overlap"
    ? "Two screens cannot be in the same place: the pointer would have no edge to cross there. Nothing was moved."
    : "That is too far out to be a place on the plane. Nothing was moved.";
}

/** The guards stored for one crossing, or `undefined` for the defaults. */
export function guardsFor(
  guards: readonly InputGuards[],
  crossing: Pick<Crossing, "to" | "side">,
): InputGuards | undefined {
  return guards.find((g) => g.monitor === crossing.to && g.side === crossing.side);
}
