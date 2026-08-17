// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The dialect spoken over the live channel (doc/input-sharing.md, section 3),
//! frozen here. One JSON object per `peers.channel` frame, `t` naming the type.
//!
//! # Why JSON on a 125 Hz path
//!
//! Because it is free at this size, measured rather than assumed: #123 put a
//! 131 byte LSP-framed JSON-RPC notification against 24 raw bytes on both the
//! Unix socket and the Windows named pipe and measured the same number, and on
//! the wire a 256 byte frame costs exactly what a 24 byte frame costs on every
//! path (direct and relayed alike). A packed binary encoding would save about 30
//! bytes per pointer frame and buy nothing measurable, against a frame that is
//! readable in a debug dump and a dialect that can grow a field without a
//! version.
//!
//! # The two guards, and why they are ours and not the Core's
//!
//! The Core cuts a channel with `FRAME_TOO_LARGE` above 1 KiB and with
//! `RATE_EXCEEDED` above 4000 frames per second sustained. A component that can
//! reach either can kill its own channel, so this module keeps its own caps well
//! below: [`MAX_OUT_FRAME`] is half the Core's, every variable-length field is
//! bounded at the source ([`SYM_MAX`], [`LAYOUT_MAX`], [`KEY_NAME_MAX`],
//! [`DEVICE_ID_MAX`], [`PLANE_ID_MAX`], [`CAPS_MAX`]), and the largest frame this
//! dialect can build is under 300 bytes. The size check at the end of [`encode`]
//! is therefore a belt on a fastened belt, and it is kept because the day a field
//! is added is the day it matters. [`Frame::degrade`] is what a caller does with
//! the check when it ever fires: section 2's rule, one line at the call site.
//!
//! # Reading is defensive, and never fatal
//!
//! A peer is semi-trusted. A malformed frame is DROPPED with a debug log, never
//! a panic and never a channel cut: cutting on a bad frame would hand a
//! misbehaving peer a way to end a session, and there is nothing a torn frame
//! could mean that is worth that. Unknown fields are ignored and an unknown `t`
//! is dropped, which is what lets a later version add both.
//!
//! # Nothing a peer chooses reaches the engine unbounded or undefined
//!
//! The paragraph above is about SHAPE. This one is about VALUES, and it was
//! written after a review proved that a peer could put a 400 byte refusal code, a
//! 300 byte device id, a 500 byte plane id, eight modifier bits nobody defined
//! and mouse button 200 into the state the interface renders and the OS is handed.
//! None of those is a torn frame: each is a well-formed frame carrying a value the
//! dialect never defined. So [`decode`] does not merely check that a field is
//! present and of the right type, it checks that it is one of the things this
//! dialect can mean:
//!
//! - the codes of `no`, `stop`, `end` and `oops` are CLOSED sets (section 3 and
//!   section 13 give each one a sentence), and anything else becomes [`UNKNOWN`],
//!   so a later version's word degrades to one an interface can still say
//!   something about instead of arriving as peer-chosen prose;
//! - `by` is bounded at [`DEVICE_ID_MAX`] and `plane` at [`PLANE_ID_MAX`], which
//!   are the exact lengths of a node_id and of a plane id;
//! - `m` is masked to the eight defined bits;
//! - `i` must be one of the five buttons the dialect defines, because the number
//!   goes to `SendInput` and to XTEST, and a button nobody defined is a
//!   peer-chosen action;
//! - `caps` is the one field whose shape is open, so it is bounded and it is read
//!   through [`crate::backend::Capabilities::from_value`], which defaults every
//!   field it cannot read to `false`.
//!
//! Fail closed on peer input, and degrade rather than refuse where the frame
//! still means something without the field.

use serde_json::{Value, json};

/// The dialect marker every message of this engine opens with, on the channel
/// and on `peers.send` alike. A third-party engine may hold the same exclusive
/// role, so two devices running different engines must recognise each other as
/// mutually unintelligible rather than misread each other's bytes.
pub const DIALECT: &str = "1device-input/1";

/// The dialect version this build speaks. A session uses the lower of the two
/// ends' versions; v1 is the only one, so this is a hook and not yet a
/// negotiation.
pub const VERSION: u32 = 1;

/// Ceiling on a frame this engine will emit: half the Core's 1 KiB cap
/// (`core/src/peerchannel.rs`, `MAX_FRAME`). Half, and not a hair under, so that
/// adding a field later cannot creep over the real cap without this check
/// firing first in a test.
pub const MAX_OUT_FRAME: usize = 512;

/// Ceiling on frames per second this engine will emit, a quarter of the Core's
/// rate cap. The pointer's own coalescing ceiling is lower still (250 Hz on a
/// fast path, 125 Hz on a slow one); this is the backstop that covers every
/// frame kind at once.
pub const OUT_RATE_MAX: u32 = 1000;

/// Longest symbol a key frame carries, in bytes of UTF-8. Generous for a
/// grapheme cluster with combining marks, far short of anything that could grow
/// a frame.
pub const SYM_MAX: usize = 32;
/// Longest layout identity a key frame carries, in bytes.
pub const LAYOUT_MAX: usize = 64;
/// Longest canonical key name, in bytes. The frozen table's longest entry is
/// `PrintScreen`.
pub const KEY_NAME_MAX: usize = 32;
/// Longest device id a `no` frame's `by` carries, in bytes. Exactly a node_id: 64
/// hex characters (`core/src/identity.rs`), so this is the shape and not a guess.
pub const DEVICE_ID_MAX: usize = 64;
/// Longest plane id, in bytes. Exactly what [`crate::plane::plane_id`] produces:
/// 16 bytes of BLAKE3 in hex, 32 characters, which is also what section 3 froze.
pub const PLANE_ID_MAX: usize = 32;
/// Longest `caps` object a `hi` frame carries, in bytes of serialized JSON.
///
/// The one field on the wire whose shape is open rather than a scalar, so the one
/// field that needs a size of its own. [`crate::backend::Capabilities::to_value`]
/// builds eight booleans and an optional problem code, which is about 160 bytes;
/// this leaves room for a later version to add a field and still be carried by a
/// build that does not know it.
pub const CAPS_MAX: usize = 256;

/// The most a wheel can move in ONE event, in either unit.
///
/// Bounded on arrival like every other field a peer chooses (section 3), and this one
/// was not until three platform backends were written against it. A real device sends
/// one notch per event, three under acceleration, or a few dozen pixels from a
/// trackpad; four thousand and ninety six of either is already absurd. What the absence
/// of a bound cost, on each platform in turn: an `i32` multiplication by the Windows
/// wheel unit overflowed and PANICKED in a debug build, the same happened to the X11
/// backend's pixel accumulator, and macOS would have scrolled a document by two
/// billion. A clamp rather than a refusal, because a frame carrying an impossible
/// number is not a malformed frame: it is a number no device could mean, and the most a
/// device could mean is the honest reading of it.
pub const WHEEL_MAX: i32 = 4096;

/// The lowest and highest mouse button the dialect defines: 1 left, 2 middle,
/// 3 right, 4 back, 5 forward (section 3).
///
/// A closed set, and it is checked on the way in rather than left to the platform.
/// The number reaches `SendInput` as a `MOUSEEVENTF_X*` data value on Windows and
/// XTEST as a button number on X11, so a button nobody defined is a peer choosing
/// an action this design never sanctioned.
pub const BUTTON_MIN: u8 = 1;
pub const BUTTON_MAX: u8 = 5;

/// The code a frame carries when this build does not know the word the peer used.
///
/// The alternative was dropping the frame, and it is worse: a `no` this build
/// cannot name is still a refusal, and a session that hangs waiting for an answer
/// it already received is a worse failure than a session refused with a word the
/// interface has to render as "that computer refused, and this build does not know
/// why". A later version's code therefore degrades to something an interface can
/// still say something about, which is the same reasoning as an unknown key name
/// falling through the resolution table rather than being mistyped.
pub const UNKNOWN: &str = "UNKNOWN";

/// What a session drives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Pointer and keyboard.
    Full,
    /// Keyboard only: the offer for a path too slow for a pointer to feel right
    /// (doc/input-sharing.md, section 14), and for a target whose backend cannot
    /// move a pointer at all.
    Keys,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Full => "full",
            Mode::Keys => "keys",
        }
    }

    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "full" => Some(Mode::Full),
            "keys" => Some(Mode::Keys),
            _ => None,
        }
    }
}

/// Which level of a key frame the target resolves first
/// (doc/input-sharing.md, section 8, and D9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyMode {
    /// Symbol first: what a person means when they type, and what makes the
    /// epic's own example work (typing `@` on an AZERTY target from a QWERTY
    /// source is AltGr plus 0, which no positional order can reach).
    Typing,
    /// HID usage first: what a real keyboard would have sent, for games and
    /// positional shortcuts.
    Positional,
}

impl Default for KeyMode {
    /// Typing, because that is what a keyboard is for most of the time, and
    /// because it is the only mode in which the epic's own example of typing an
    /// at sign across two layouts can work at all.
    fn default() -> KeyMode {
        KeyMode::Typing
    }
}

impl KeyMode {
    pub fn as_str(self) -> &'static str {
        match self {
            KeyMode::Typing => "typing",
            KeyMode::Positional => "positional",
        }
    }

    pub fn parse(s: &str) -> Option<KeyMode> {
        match s {
            "typing" => Some(KeyMode::Typing),
            "positional" => Some(KeyMode::Positional),
            _ => None,
        }
    }
}

/// Why a target refuses to be driven. Codes, not sentences: the interface owns
/// the wording (doc/input-sharing.md, section 13).
pub mod refused {
    /// This device has not been allowed to drive that machine. Its authority,
    /// learned by trying, never hinted at in the handshake.
    pub const NOT_ALLOWED: &str = "NOT_ALLOWED";
    /// Another source holds that machine, or that machine is itself driving.
    pub const BUSY: &str = "BUSY";
    /// The two ends do not hold the same plane, so absolute coordinates would
    /// mean two different things. The one refusal that repairs itself.
    pub const PLANE_STALE: &str = "PLANE_STALE";
    /// Nothing there can type: no platform backend, or its permission refused.
    pub const NO_BACKEND: &str = "NO_BACKEND";
    /// That machine is locked, or its pointer is pinned to its own screen.
    pub const LOCKED: &str = "LOCKED";

    /// The closed set, for the decoder and for the test that pins it. A code that
    /// is not in here is not a refusal this dialect defines.
    pub const ALL: &[&str] = &[NOT_ALLOWED, BUSY, PLANE_STALE, NO_BACKEND, LOCKED];
}

/// Why a source ended a session it owned.
pub mod stopped {
    /// The hotkey, or the pointer crossed back home.
    pub const RETURNED: &str = "RETURNED";
    /// On to another screen: the pointer left this target for the next one.
    pub const MOVED: &str = "MOVED";
    /// The local capture backend died under us.
    pub const GONE: &str = "GONE";
    /// The measured path degraded past the pointer threshold.
    pub const SLOW: &str = "SLOW";

    /// The closed set, for the decoder and for the test that pins it.
    pub const ALL: &[&str] = &[RETURNED, MOVED, GONE, SLOW];
}

/// Why a target ended a session unilaterally.
pub mod ended {
    /// The grant was withdrawn while the session ran.
    pub const REVOKED: &str = "REVOKED";
    /// The injection backend went away, or its permission did.
    pub const NO_BACKEND: &str = "NO_BACKEND";
    /// The machine locked, or its pointer was pinned to its own screen.
    pub const LOCKED: &str = "LOCKED";
    /// No frame from the source inside the session's idle budget. A source that
    /// went quiet is a hung source, and a target holding Control is the failure
    /// this feature must not have.
    pub const IDLE: &str = "IDLE";
    /// Its own user asked for it back. Reserved: v1 does not detect local input
    /// on a target (doc/input-sharing.md, section 15), and the word exists so
    /// the day it does needs no dialect change.
    pub const TAKEN: &str = "TAKEN";

    /// The closed set, for the decoder and for the test that pins it.
    pub const ALL: &[&str] = &[REVOKED, NO_BACKEND, LOCKED, IDLE, TAKEN];
}

/// Why an injection did not happen, carried by the `oops` frame
/// (doc/input-sharing.md, section 3 and section 13).
///
/// Frozen here rather than only in [`crate::backend::Refusal`], because the set is
/// not the same set: four of the five are refusals a platform backend DETECTS and
/// reports upward, and `UNRESOLVED` is generated by the engine itself when no level
/// of a key frame could be produced on this machine. Before this module existed the
/// four lived as `Refusal::code`'s return values and the fifth as a string literal
/// in the code that emits it, which is one spelling short of a frozen dialect.
/// [`crate::backend::Refusal::code`] now returns these constants, so there is one
/// spelling of each.
pub mod oops {
    /// `SendInput` returned 0 and the foreground window's integrity level says
    /// why: UIPI blocks a lower integrity process from a higher one.
    pub const ELEVATED_WINDOW: &str = "ELEVATED_WINDOW";
    /// macOS has a password field focused (`IsSecureEventInputEnabled`).
    pub const SECURE_INPUT: &str = "SECURE_INPUT";
    /// The machine is locked, or on Windows the secure desktop is up.
    pub const SCREEN_LOCKED: &str = "SCREEN_LOCKED";
    /// The OS grant is missing: nothing can be typed until a human gives it.
    pub const NO_PERMISSION: &str = "NO_PERMISSION";
    /// No level of the key frame could be produced here, and this machine cannot
    /// inject an arbitrary string either. The one code of this set the ENGINE
    /// generates rather than a backend reporting it.
    pub const UNRESOLVED: &str = "UNRESOLVED";

    /// The closed set, for the decoder and for the test that pins it.
    pub const ALL: &[&str] = &[
        ELEVATED_WINDOW,
        SECURE_INPUT,
        SCREEN_LOCKED,
        NO_PERMISSION,
        UNRESOLVED,
    ];
}

/// One frame of the dialect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frame {
    /// The handshake, sent by both ends immediately on attach, without waiting
    /// for the other's. Nothing else is legal before it.
    Hi {
        version: u32,
        /// What this end's platform backend can do, so an interface can say what
        /// a session cannot do BEFORE anyone tries.
        caps: Value,
        /// This end's plane id.
        plane: String,
    },
    /// The source asks to drive.
    Start {
        session: u32,
        mode: Mode,
        keys: KeyMode,
        /// The source's plane id. The whole reason absolute positions are safe:
        /// `x` and `y` are in the TARGET's own logical desktop coordinates, and
        /// if the two ends hold different planes those coordinates mean
        /// different things.
        plane: String,
        n: u32,
        x: i32,
        y: i32,
    },
    Accepted {
        session: u32,
    },
    Refused {
        session: u32,
        code: String,
        /// On [`refused::BUSY`] only: which device holds it, so the interface
        /// can name it.
        by: Option<String>,
    },
    Stop {
        session: u32,
        code: String,
    },
    Ended {
        session: u32,
        code: String,
    },
    /// Absolute pointer, in the target's own logical desktop coordinates.
    Pointer {
        session: u32,
        n: u32,
        x: i32,
        y: i32,
    },
    /// Relative pointer, logical pixels, never both zero (Windows discards a
    /// relative move of (0, 0) and it reaches no hook at all).
    Motion {
        session: u32,
        n: u32,
        dx: i32,
        dy: i32,
    },
    Button {
        session: u32,
        n: u32,
        button: u8,
        down: bool,
    },
    Wheel {
        session: u32,
        n: u32,
        dx: i32,
        dy: i32,
        pixels: bool,
    },
    Key {
        session: u32,
        n: u32,
        /// HID usage, `(page << 16) | id`, 0 when unknown.
        usage: u32,
        /// Canonical name of a key that produces no character.
        key: Option<String>,
        /// The text the source's own layout produced.
        sym: Option<String>,
        /// Canonical modifier bits ([`crate::keys::mods`]).
        mods: u16,
        /// The source's layout identity.
        layout: String,
        down: bool,
        /// A half-duplex lock: a press with no release to come.
        lock: bool,
    },
    /// Release everything you believe I hold. Belt and braces on a stop, and the
    /// remedy when a source notices its own capture desynchronised.
    ReleaseAll {
        session: u32,
    },
    /// Keepalive and latency probe in one. The Core sweeps a channel with no
    /// frame in either direction for 10 s, so a warm channel must say something;
    /// making that something a round trip buys the number the pointer thresholds
    /// need for one extra frame. `ms` is the sender's own monotonic clock, echoed
    /// back untouched, so the round trip is computed with no clock
    /// synchronisation between the machines.
    Ping {
        ms: u64,
    },
    Pong {
        ms: u64,
    },
    /// An injection was refused, coalesced to one code per second with a count.
    Oops {
        session: u32,
        code: String,
        count: u32,
    },
}

impl Frame {
    /// The session this frame belongs to, if any. `Ping` and `Pong` belong to
    /// the channel rather than to a session, which is what lets them keep a warm
    /// channel alive with no session on it.
    pub fn session(&self) -> Option<u32> {
        match self {
            Frame::Hi { .. } | Frame::Ping { .. } | Frame::Pong { .. } => None,
            Frame::Start { session, .. }
            | Frame::Accepted { session }
            | Frame::Refused { session, .. }
            | Frame::Stop { session, .. }
            | Frame::Ended { session, .. }
            | Frame::Pointer { session, .. }
            | Frame::Motion { session, .. }
            | Frame::Button { session, .. }
            | Frame::Wheel { session, .. }
            | Frame::Key { session, .. }
            | Frame::ReleaseAll { session }
            | Frame::Oops { session, .. } => Some(*session),
        }
    }

    /// Does this frame carry the flow counter? Exactly the frames the counter
    /// numbers, and exactly the frames a target applies to its OS.
    pub fn flow(&self) -> Option<u32> {
        match self {
            Frame::Pointer { n, .. }
            | Frame::Motion { n, .. }
            | Frame::Button { n, .. }
            | Frame::Wheel { n, .. }
            | Frame::Key { n, .. } => Some(*n),
            _ => None,
        }
    }

    /// Is this a pointer position? The read-side coalescing rule needs to know
    /// (doc/input-sharing.md, D6): a position whose immediate successor in the
    /// same batch is also a position is dropped, and nothing else ever is.
    pub fn is_position(&self) -> bool {
        matches!(self, Frame::Pointer { .. } | Frame::Motion { .. })
    }

    /// Section 2's whole-frame remedy for a frame this engine would emit above
    /// [`MAX_OUT_FRAME`]: "the pointer frame is dropped, the key frame is degraded
    /// (the symbol is dropped, the usage kept)".
    ///
    /// Returns true when something was given up and the frame is worth encoding
    /// again, false when there is nothing left to give up and the caller must drop
    /// the frame and log at warn. So the caller's rule is one line:
    /// `if frame.degrade() { retry } else { drop }`.
    ///
    /// It lives here rather than in the caller because WHICH field a frame can
    /// afford to lose is dialect knowledge, and because the answer for a key frame
    /// (`sym`, and only `sym`) is the same answer [`encode`] applies to a symbol
    /// over [`SYM_MAX`]. Unreachable today, which is the point of section 2's own
    /// note that the size check is a belt on a fastened belt: the largest frame
    /// this dialect can build is under 300 bytes, and the day a field is added is
    /// the day this matters.
    pub fn degrade(&mut self) -> bool {
        match self {
            Frame::Key { sym, .. } => sym.take().is_some(),
            _ => false,
        }
    }
}

/// A frame this engine refused to build.
#[derive(Debug, PartialEq, Eq)]
pub enum EncodeError {
    /// Above [`MAX_OUT_FRAME`]. Never reachable with the field bounds this
    /// module enforces, which is the point of checking.
    TooLarge(usize),
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EncodeError::TooLarge(n) => {
                write!(
                    f,
                    "an input frame of {n} bytes is above the engine's own cap"
                )
            }
        }
    }
}

impl std::error::Error for EncodeError {}

/// Serializes a frame, bounding every variable-length field on the way out and
/// then the whole frame.
///
/// # Which fields are clipped and which are dropped, and why the two differ
///
/// A field that is only ever COMPARED is clipped: a layout identity and a
/// canonical key name are tokens, a clipped one simply fails to match, and the
/// keystroke it travels with is still worth carrying. A field whose value is USED
/// is dropped whole when it does not fit, because a wrong value is worse than an
/// absent one. That distinction was learned the hard way and section 2 already
/// states the outcome for the one field it matters most on: over the cap "the key
/// frame is degraded (the symbol is dropped, the usage kept)".
///
/// `sym` was being clipped on a character boundary, and 33 bytes of decomposed
/// accents clip to a trailing BARE base letter: the target then typed an
/// unaccented letter where the human typed an accented one, and `typing` mode
/// PREFERS that wrong symbol over the usage that would have been right. So `sym`
/// is dropped, `by` (a device id, which names a device or names nothing) is
/// dropped, and only `l` and `key` are clipped.
pub fn encode(frame: &Frame) -> Result<Vec<u8>, EncodeError> {
    let value = match frame {
        Frame::Hi {
            version,
            caps,
            plane,
        } => {
            // The ONE frame that must always be sent: nothing else is legal before
            // it, so a `hi` this engine refused to build would make the session
            // impossible rather than degraded. `caps` is the only open-shaped field
            // on the wire, so it is the only one that could ever push this frame
            // over the cap, and it is replaced by the all-false object rather than
            // carried: a peer that cannot read what we can do must assume we can do
            // nothing, which is the same fail-closed rule the decoder applies.
            let caps = if json_len(caps) > CAPS_MAX {
                eprintln!(
                    "[1device-input] this machine's capabilities do not fit a handshake frame, \
                     announcing none"
                );
                json!({})
            } else {
                caps.clone()
            };
            json!({ "t": "hi", "d": DIALECT, "v": version, "caps": caps, "plane": plane })
        }
        Frame::Start {
            session,
            mode,
            keys,
            plane,
            n,
            x,
            y,
        } => json!({ "t": "start", "s": session, "mode": mode.as_str(),
                     "keys": keys.as_str(), "plane": plane, "n": n, "x": x, "y": y }),
        Frame::Accepted { session } => json!({ "t": "ok", "s": session }),
        Frame::Refused { session, code, by } => {
            let mut v = json!({ "t": "no", "s": session, "c": code });
            // Dropped rather than clipped: `by` is a device id the interface looks
            // up to name a machine, and half of a node_id names nothing at all.
            if let Some(by) = by.as_ref().filter(|by| by.len() <= DEVICE_ID_MAX) {
                v["by"] = json!(by);
            }
            v
        }
        Frame::Stop { session, code } => json!({ "t": "stop", "s": session, "c": code }),
        Frame::Ended { session, code } => json!({ "t": "end", "s": session, "c": code }),
        Frame::Pointer { session, n, x, y } => {
            json!({ "t": "p", "s": session, "n": n, "x": x, "y": y })
        }
        Frame::Motion { session, n, dx, dy } => {
            json!({ "t": "r", "s": session, "n": n, "dx": dx, "dy": dy })
        }
        Frame::Button {
            session,
            n,
            button,
            down,
        } => json!({ "t": "b", "s": session, "n": n, "i": button, "dn": down }),
        Frame::Wheel {
            session,
            n,
            dx,
            dy,
            pixels,
            // Clamped on the way OUT as well, so a local device that reported
            // something impossible never puts it on the wire: the same rule as
            // `clip(sym, SYM_MAX)` one field below.
        } => json!({ "t": "w", "s": session, "n": n,
                     "dx": dx.clamp(&-WHEEL_MAX, &WHEEL_MAX),
                     "dy": dy.clamp(&-WHEEL_MAX, &WHEEL_MAX),
                     "u": if *pixels { "px" } else { "line" } }),
        Frame::Key {
            session,
            n,
            usage,
            key,
            sym,
            mods,
            layout,
            down,
            lock,
        } => {
            let mut v = json!({ "t": "k", "s": session, "n": n, "u": usage, "m": mods,
                                "l": clip(layout, LAYOUT_MAX), "dn": down });
            if let Some(key) = key {
                v["key"] = json!(clip(key, KEY_NAME_MAX));
            }
            // DROPPED over the bound, never clipped, which is section 2's own rule
            // ("the symbol is dropped, the usage kept"). A clip on a character
            // boundary turns 33 bytes of decomposed accents into a bare base
            // letter, and `typing` mode then prefers that wrong character over the
            // usage that would have been right: the target types `e` where the
            // human typed `é`. A wrong character is worse than no character.
            if let Some(sym) = sym {
                if sym.len() <= SYM_MAX {
                    v["sym"] = json!(sym);
                } else {
                    eprintln!(
                        "[1device-input] a symbol of {} bytes is over the wire's bound, \
                         sending the keystroke without it",
                        sym.len()
                    );
                }
            }
            // Absent rather than false for the common case: a lock is rare, and
            // the flag is the one field on the hot path that can be left out.
            if *lock {
                v["lk"] = json!(true);
            }
            v
        }
        Frame::ReleaseAll { session } => json!({ "t": "rel", "s": session }),
        Frame::Ping { ms } => json!({ "t": "ping", "ms": ms }),
        Frame::Pong { ms } => json!({ "t": "pong", "ms": ms }),
        Frame::Oops {
            session,
            code,
            count,
        } => json!({ "t": "oops", "s": session, "c": code, "k": count }),
    };
    let bytes = serde_json::to_vec(&value).expect("serialize an input frame");
    if bytes.len() > MAX_OUT_FRAME {
        return Err(EncodeError::TooLarge(bytes.len()));
    }
    Ok(bytes)
}

/// How many bytes a value costs on the wire. Used to bound the one open-shaped
/// field there is, on both sides of the codec.
fn json_len(v: &Value) -> usize {
    serde_json::to_vec(v).map_or(usize::MAX, |bytes| bytes.len())
}

/// Truncates a string to at most `max` BYTES, on a character boundary, so the
/// result is always valid UTF-8 and always fits the wire's bound.
///
/// For the two fields that are only ever COMPARED, and no others: a layout
/// identity and a canonical key name are tokens, so a clipped one fails to match
/// and falls through to the next level of the resolution table, which is a level
/// the frame carried anyway. A clipped `sym` would be a DIFFERENT character, so
/// `sym` is dropped instead (see [`encode`]).
fn clip(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Parses a frame. `None` for anything this build cannot make sense of: an
/// unknown `t`, a missing required field, a field of the wrong type. The caller
/// drops it with a debug log and reads on, which is what keeps a peer from being
/// able to end a session with one bad frame.
pub fn decode(bytes: &[u8]) -> Option<Frame> {
    let v: Value = serde_json::from_slice(bytes).ok()?;
    let t = v.get("t")?.as_str()?;
    // Session and flow numbers are u32 on the wire; anything outside is a frame
    // this dialect did not write.
    let s = || u32_of(&v, "s");
    let n = || u32_of(&v, "n");
    match t {
        "hi" => {
            // The dialect marker is checked HERE and not by the caller, so no
            // caller can forget: a message in another dialect must never be read
            // as ours, whatever its shape happens to be.
            if v.get("d")?.as_str()? != DIALECT {
                return None;
            }
            Some(Frame::Hi {
                version: u32_of(&v, "v")?,
                // Bounded and shape-checked, then read by
                // `Capabilities::from_value`, which defaults every field it cannot
                // read to false. An absent, oversized or non-object `caps` becomes
                // the empty object, which that reader turns into "this peer can do
                // nothing": the fail-closed answer, and the one that makes an
                // interface say what a session cannot do instead of guessing.
                caps: match v.get("caps") {
                    Some(caps) if caps.is_object() && json_len(caps) <= CAPS_MAX => caps.clone(),
                    _ => json!({}),
                },
                plane: bounded(v.get("plane"), PLANE_ID_MAX)?,
            })
        }
        "start" => Some(Frame::Start {
            session: s()?,
            mode: Mode::parse(v.get("mode")?.as_str()?)?,
            keys: KeyMode::parse(v.get("keys")?.as_str()?)?,
            // Refused rather than degraded, unlike `by`: a plane id is what makes
            // the absolute coordinates of this session mean anything, so a `start`
            // whose plane id is not the shape of a plane id has nothing left to
            // agree about. The comparison would refuse it as `PLANE_STALE` in any
            // case; refusing to read it keeps a peer from choosing how much of our
            // memory the attempt costs.
            plane: bounded(v.get("plane"), PLANE_ID_MAX)?,
            n: n()?,
            x: i32_of(&v, "x")?,
            y: i32_of(&v, "y")?,
        }),
        "ok" => Some(Frame::Accepted { session: s()? }),
        "no" => Some(Frame::Refused {
            session: s()?,
            code: code_in(&v, refused::ALL)?,
            // Bounded, and DROPPED rather than refusing the frame: `by` decorates a
            // refusal (it lets the interface name who holds the keyboard) and a
            // refusal with no name is still a refusal the session must act on.
            by: bounded(v.get("by"), DEVICE_ID_MAX),
        }),
        "stop" => Some(Frame::Stop {
            session: s()?,
            code: code_in(&v, stopped::ALL)?,
        }),
        "end" => Some(Frame::Ended {
            session: s()?,
            code: code_in(&v, ended::ALL)?,
        }),
        "p" => Some(Frame::Pointer {
            session: s()?,
            n: n()?,
            x: i32_of(&v, "x")?,
            y: i32_of(&v, "y")?,
        }),
        "r" => {
            let (dx, dy) = (i32_of(&v, "dx")?, i32_of(&v, "dy")?);
            // A relative move of (0, 0) is not a move: Windows discards one and
            // it reaches no hook at all, so nothing legitimate sends it and
            // applying it would be a no-op with a cost.
            if dx == 0 && dy == 0 {
                return None;
            }
            Some(Frame::Motion {
                session: s()?,
                n: n()?,
                dx,
                dy,
            })
        }
        "b" => {
            // A button the dialect does not define is REFUSED, not clamped. The
            // number is handed to the platform as it stands (a `MOUSEEVENTF_X*`
            // data value on Windows, an XTEST button number on X11), so button 200
            // would be a peer choosing an action this design never sanctioned, and
            // a clamp would be us choosing a different one on its behalf.
            let button = u8::try_from(v.get("i")?.as_u64()?).ok()?;
            if !(BUTTON_MIN..=BUTTON_MAX).contains(&button) {
                return None;
            }
            Some(Frame::Button {
                session: s()?,
                n: n()?,
                button,
                down: v.get("dn")?.as_bool()?,
            })
        }
        "w" => Some(Frame::Wheel {
            session: s()?,
            n: n()?,
            dx: i32_of(&v, "dx")?.clamp(-WHEEL_MAX, WHEEL_MAX),
            dy: i32_of(&v, "dy")?.clamp(-WHEEL_MAX, WHEEL_MAX),
            pixels: match v.get("u")?.as_str()? {
                "px" => true,
                "line" => false,
                _ => return None,
            },
        }),
        "k" => Some(Frame::Key {
            session: s()?,
            n: n()?,
            usage: u32_of(&v, "u")?,
            key: bounded(v.get("key"), KEY_NAME_MAX),
            sym: bounded(v.get("sym"), SYM_MAX),
            // Masked to the eight bits section 3 defines. An undefined bit reaches
            // the remapping, the hotkey comparison and the modifier dance, and
            // there is nothing any of them could correctly do with it; masking
            // makes a peer from a later version look like a peer holding the
            // modifiers this build understands, which is exactly right.
            mods: u16::try_from(v.get("m")?.as_u64()?).ok()? & crate::keys::mods::DEFINED,
            // Bounded on arrival as well as on the way out: the bound protects
            // this engine's own memory and its logs from a peer that does not
            // share its manners.
            layout: bounded(v.get("l"), LAYOUT_MAX)?,
            down: v.get("dn")?.as_bool()?,
            lock: v.get("lk").and_then(Value::as_bool).unwrap_or(false),
        }),
        "rel" => Some(Frame::ReleaseAll { session: s()? }),
        "ping" => Some(Frame::Ping {
            ms: v.get("ms")?.as_u64()?,
        }),
        "pong" => Some(Frame::Pong {
            ms: v.get("ms")?.as_u64()?,
        }),
        "oops" => Some(Frame::Oops {
            session: s()?,
            code: code_in(&v, oops::ALL)?,
            count: u32_of(&v, "k")?,
        }),
        // An unknown type is a later version saying something this build does
        // not need. Dropped, never fatal.
        _ => None,
    }
}

fn u32_of(v: &Value, key: &str) -> Option<u32> {
    u32::try_from(v.get(key)?.as_u64()?).ok()
}

fn i32_of(v: &Value, key: &str) -> Option<i32> {
    i32::try_from(v.get(key)?.as_i64()?).ok()
}

/// A frame's `c`, read against the CLOSED set of codes that frame kind defines.
///
/// `None` only when `c` is missing or is not a string, which is a malformed frame.
/// A well-formed frame carrying a word this build does not know keeps its meaning
/// and loses only the detail: the code becomes [`UNKNOWN`]. The set is checked here
/// rather than by the caller for the same reason the dialect marker is: no caller
/// can forget, and the string that reaches `input.status` and the notification a
/// GUI renders is then always one of ours rather than 400 bytes a peer chose.
///
/// The matched constant is returned rather than the peer's own bytes, so the
/// spelling downstream is this build's spelling.
fn code_in(v: &Value, known: &[&'static str]) -> Option<String> {
    let c = v.get("c")?.as_str()?;
    Some(
        known
            .iter()
            .find(|code| **code == c)
            .map_or_else(|| UNKNOWN.to_string(), |code| (*code).to_string()),
    )
}

/// A string field, refused if it is over its bound. Refused and not truncated,
/// deliberately: on the way out a clip saves a keystroke, on the way in a field
/// over the bound is a peer not speaking this dialect, and reading it would mean
/// letting that peer choose how much of our memory a frame costs.
fn bounded(v: Option<&Value>, max: usize) -> Option<String> {
    let s = v?.as_str()?;
    if s.len() > max {
        return None;
    }
    Some(s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(frame: Frame) {
        let bytes = encode(&frame).expect("the frame fits");
        assert_eq!(
            decode(&bytes),
            Some(frame.clone()),
            "a frame must survive its own codec"
        );
        assert!(
            bytes.len() <= MAX_OUT_FRAME,
            "{} bytes for {frame:?}",
            bytes.len()
        );
    }

    /// Every frame of the dialect survives its own codec, and every one of them
    /// fits the engine's own cap with room to spare. The second half is the
    /// contract that keeps this engine from ever being cut by the Core's
    /// `FRAME_TOO_LARGE`.
    #[test]
    fn every_frame_survives_the_codec_and_fits_the_cap() {
        roundtrip(Frame::Hi {
            version: VERSION,
            caps: json!({ "inject_keys": true, "inject_pointer": true }),
            plane: "0".repeat(32),
        });
        roundtrip(Frame::Start {
            session: 1,
            mode: Mode::Full,
            keys: KeyMode::Typing,
            plane: "f".repeat(32),
            n: 1,
            x: -2560,
            y: 1439,
        });
        roundtrip(Frame::Accepted { session: 1 });
        roundtrip(Frame::Refused {
            session: 1,
            code: refused::BUSY.into(),
            by: Some("device-0123456789abcdef".into()),
        });
        roundtrip(Frame::Refused {
            session: 1,
            code: refused::NOT_ALLOWED.into(),
            by: None,
        });
        roundtrip(Frame::Stop {
            session: 1,
            code: stopped::RETURNED.into(),
        });
        roundtrip(Frame::Ended {
            session: 1,
            code: ended::IDLE.into(),
        });
        roundtrip(Frame::Pointer {
            session: 1,
            n: 4_000_000_000,
            x: i32::MIN,
            y: i32::MAX,
        });
        roundtrip(Frame::Motion {
            session: 1,
            n: 2,
            dx: -3,
            dy: 4,
        });
        roundtrip(Frame::Button {
            session: 1,
            n: 3,
            button: 5,
            down: true,
        });
        roundtrip(Frame::Wheel {
            session: 1,
            n: 4,
            dx: 0,
            dy: -120,
            pixels: true,
        });
        roundtrip(Frame::Wheel {
            session: 1,
            n: 5,
            dx: 1,
            dy: 0,
            pixels: false,
        });
        roundtrip(Frame::Key {
            session: 1,
            n: 6,
            usage: 0x0007_0028,
            key: Some("PrintScreen".into()),
            sym: Some("é".into()),
            mods: 0b0001_1111,
            layout: "fr(azerty)".into(),
            down: true,
            lock: false,
        });
        roundtrip(Frame::Key {
            session: 1,
            n: 7,
            usage: 0x0007_0039,
            key: Some("CapsLock".into()),
            sym: None,
            mods: 0,
            layout: "us".into(),
            down: true,
            lock: true,
        });
        roundtrip(Frame::ReleaseAll { session: 1 });
        roundtrip(Frame::Ping { ms: u64::MAX });
        roundtrip(Frame::Pong { ms: 0 });
        roundtrip(Frame::Oops {
            session: 1,
            code: oops::ELEVATED_WINDOW.into(),
            count: 42,
        });
    }

    /// The worst frame this dialect can legally build, with every field at its
    /// bound, still sits far below the Core's 1 KiB cap. This is the test that
    /// notices the day a field is added.
    #[test]
    fn the_largest_legal_frame_is_far_below_the_cores_cap() {
        let bytes = encode(&Frame::Key {
            session: u32::MAX,
            n: u32::MAX,
            usage: u32::MAX,
            key: Some("K".repeat(KEY_NAME_MAX)),
            sym: Some("s".repeat(SYM_MAX)),
            mods: u16::MAX,
            layout: "L".repeat(LAYOUT_MAX),
            down: true,
            lock: true,
        })
        .expect("the largest key frame fits");
        assert!(
            bytes.len() < 300,
            "the largest legal frame is {} bytes",
            bytes.len()
        );
    }

    /// An oversized SYMBOL is dropped and the keystroke keeps its usage, which is
    /// section 2's own rule ("the symbol is dropped, the usage kept"), while an
    /// identity that is only ever compared is clipped and a field over its bound is
    /// refused coming in.
    ///
    /// The symbol half is the one that matters and it was the other way round.
    /// Clipping on a character boundary turns 33 bytes of decomposed accents into a
    /// trailing BARE base letter, so the target typed `e` where the human typed `é`,
    /// and `typing` mode PREFERS that wrong symbol over the usage that would have
    /// been right. A wrong character is worse than no character.
    #[test]
    fn an_oversized_symbol_is_dropped_going_out_and_an_oversized_identity_is_refused_coming_in() {
        // 33 bytes: an `e` followed by sixteen combining acute accents. A clip on a
        // character boundary would leave the bare `e`, and 32 bytes is where the
        // bound falls, so this is the shape of the real failure rather than a
        // caricature of it.
        let decomposed = format!("e{}", "\u{0301}".repeat(16));
        assert!(decomposed.len() > SYM_MAX);
        let bytes = encode(&Frame::Key {
            session: 1,
            n: 1,
            usage: 0x0007_0008,
            key: None,
            sym: Some(decomposed),
            mods: 0,
            layout: "x".repeat(200),
            down: true,
            lock: false,
        })
        .expect("degraded, not refused");
        let Some(Frame::Key {
            sym, layout, usage, ..
        }) = decode(&bytes)
        else {
            panic!("a degraded key frame is still a key frame");
        };
        assert_eq!(
            sym, None,
            "the symbol is dropped whole: a bare base letter is the WRONG character"
        );
        assert_eq!(
            usage, 0x0007_0008,
            "and the usage is kept, so the key lands"
        );
        assert!(
            layout.len() <= LAYOUT_MAX,
            "an identity is only ever compared, so clipping it costs a fallthrough"
        );

        // A symbol exactly at the bound still travels: the rule is over the bound,
        // not near it.
        let bytes = encode(&Frame::Key {
            session: 1,
            n: 1,
            usage: 7,
            key: None,
            sym: Some("s".repeat(SYM_MAX)),
            mods: 0,
            layout: "us".into(),
            down: true,
            lock: false,
        })
        .expect("fits");
        let Some(Frame::Key { sym, .. }) = decode(&bytes) else {
            panic!("a key frame");
        };
        assert_eq!(sym, Some("s".repeat(SYM_MAX)));

        let hostile = json!({ "t": "k", "s": 1, "n": 1, "u": 7, "m": 0,
                              "l": "x".repeat(LAYOUT_MAX + 1), "dn": true });
        assert_eq!(
            decode(&serde_json::to_vec(&hostile).expect("json")),
            None,
            "a layout name over the bound is a peer not speaking this dialect"
        );
    }

    /// Section 2's whole-frame remedy, as a shape the caller can use: a key frame
    /// gives up its symbol and is worth encoding again, and anything else has
    /// nothing to give up and must be dropped.
    #[test]
    fn a_frame_over_the_cap_degrades_by_giving_up_its_symbol_and_nothing_else_can() {
        let mut key = Frame::Key {
            session: 1,
            n: 1,
            usage: 0x0007_0004,
            key: None,
            sym: Some("a".into()),
            mods: 0,
            layout: "us".into(),
            down: true,
            lock: false,
        };
        assert!(key.degrade(), "the symbol is what a key frame can afford");
        assert!(matches!(
            &key,
            Frame::Key {
                sym: None,
                usage: 0x0007_0004,
                ..
            }
        ));
        assert!(
            !key.degrade(),
            "and once it is gone there is nothing left to give up"
        );

        let mut pointer = Frame::Pointer {
            session: 1,
            n: 1,
            x: 0,
            y: 0,
        };
        assert!(
            !pointer.degrade(),
            "a pointer frame is dropped, not degraded: it is all coordinates"
        );
    }

    /// The `caps` object is bounded on both sides of the codec, and an unreadable
    /// one becomes the empty object rather than making the ONE frame that must
    /// always be sent impossible to send.
    ///
    /// Nothing else is legal before `hi`, so a refused `hi` is a session that cannot
    /// exist at all, where an empty `caps` is a peer that reads as "can do nothing"
    /// through `Capabilities::from_value` and therefore a session refused honestly.
    #[test]
    fn an_unreadable_caps_degrades_the_handshake_instead_of_making_it_impossible() {
        let real = crate::backend::Capabilities::none(Some(crate::backend::Problem::NoPermission));
        assert!(
            json_len(&real.to_value()) <= CAPS_MAX,
            "the real object is {} bytes, well inside the bound",
            json_len(&real.to_value())
        );

        let bloated = json!({ "capture": true, "junk": "j".repeat(CAPS_MAX) });
        let bytes = encode(&Frame::Hi {
            version: VERSION,
            caps: bloated,
            plane: "0".repeat(PLANE_ID_MAX),
        })
        .expect("a hi frame is always sendable");
        let Some(Frame::Hi { caps, .. }) = decode(&bytes) else {
            panic!("a hi frame");
        };
        assert_eq!(
            caps,
            json!({}),
            "announcing nothing rather than nothing at all"
        );
        assert_eq!(
            crate::backend::Capabilities::from_value(&caps),
            crate::backend::Capabilities::none(None),
            "which a reader turns into a peer that can do nothing"
        );

        // And coming in: an oversized, or non-object, `caps` from a peer reads as the
        // empty object rather than being cloned into our state.
        for hostile in [
            json!({ "t": "hi", "d": DIALECT, "v": 1, "plane": "0",
                    "caps": { "capture": true, "junk": "j".repeat(CAPS_MAX) } }),
            json!({ "t": "hi", "d": DIALECT, "v": 1, "plane": "0", "caps": "everything" }),
            json!({ "t": "hi", "d": DIALECT, "v": 1, "plane": "0", "caps": [true, true] }),
        ] {
            let Some(Frame::Hi { caps, .. }) = decode(&serde_json::to_vec(&hostile).expect("json"))
            else {
                panic!("the handshake still arrives: {hostile}");
            };
            assert_eq!(caps, json!({}), "fail closed on {hostile}");
        }
    }

    /// Every code of the four closed sets survives its own codec, and a word this
    /// build does not know becomes [`UNKNOWN`] rather than reaching the interface as
    /// 400 bytes a peer chose.
    ///
    /// The codes are what `input.status`'s `problem` and the notification a GUI shows
    /// are built from, and each one has a sentence in section 13. A code with no
    /// sentence has nothing an interface can say, so it degrades to the one word that
    /// does.
    #[test]
    fn a_refusal_code_this_build_does_not_know_degrades_to_one_word() {
        for code in refused::ALL {
            roundtrip(Frame::Refused {
                session: 1,
                code: (*code).into(),
                by: None,
            });
        }
        for code in stopped::ALL {
            roundtrip(Frame::Stop {
                session: 1,
                code: (*code).into(),
            });
        }
        for code in ended::ALL {
            roundtrip(Frame::Ended {
                session: 1,
                code: (*code).into(),
            });
        }
        for code in oops::ALL {
            roundtrip(Frame::Oops {
                session: 1,
                code: (*code).into(),
                count: 1,
            });
        }

        let cases = [
            (json!({ "t": "no", "s": 1, "c": "N".repeat(400) }), UNKNOWN),
            // A word from a later version of this dialect, and a word from another
            // set: both are "not one of ours" here, which is the point of the sets
            // being per frame kind.
            (json!({ "t": "no", "s": 1, "c": "PREEMPTED" }), UNKNOWN),
            (json!({ "t": "stop", "s": 1, "c": "REVOKED" }), UNKNOWN),
            (json!({ "t": "end", "s": 1, "c": "RETURNED" }), UNKNOWN),
            (
                json!({ "t": "oops", "s": 1, "c": "MOONPHASE", "k": 1 }),
                UNKNOWN,
            ),
            (json!({ "t": "stop", "s": 1, "c": "SLOW" }), stopped::SLOW),
        ];
        for (frame, expected) in cases {
            let decoded = decode(&serde_json::to_vec(&frame).expect("json"))
                .unwrap_or_else(|| panic!("{frame} must still be read"));
            let code = match &decoded {
                Frame::Refused { code, .. }
                | Frame::Stop { code, .. }
                | Frame::Ended { code, .. }
                | Frame::Oops { code, .. } => code.as_str(),
                other => panic!("{other:?} is not a coded frame"),
            };
            assert_eq!(code, expected, "for {frame}");
        }
    }

    /// Every remaining peer-chosen value is bounded or masked: `by`, `plane`, `m`
    /// and the button number.
    ///
    /// All four were proven to get through, and all four reach something that acts
    /// on them: `by` and the plane id reach the state an interface renders, `m`
    /// reaches the remapping and the modifier dance, and the button number reaches
    /// `SendInput` and XTEST as-is.
    /// A wheel delta a peer chose is clamped to what a device could mean, in both
    /// directions and both units.
    ///
    /// It is the field that was NOT bounded until three platform backends were written
    /// against it, and what the absence cost was arithmetic: a notch count times the
    /// Windows wheel unit, and the X11 pixel accumulator's own multiplication, both
    /// overflow an `i32` and both PANIC in a debug build. A peer chose the number, so
    /// that is a peer choosing to crash the component that is being driven.
    #[test]
    fn a_wheel_delta_is_clamped_to_what_a_device_could_mean() {
        for (sent, want) in [
            (i32::MAX, WHEEL_MAX),
            (i32::MIN, -WHEEL_MAX),
            (WHEEL_MAX + 1, WHEEL_MAX),
            (5, 5),
            (-3, -3),
            (0, 0),
        ] {
            let frame = json!({ "t": "w", "s": 1, "n": 2, "dx": sent, "dy": sent, "u": "line" });
            let Some(Frame::Wheel { dx, dy, .. }) =
                decode(&serde_json::to_vec(&frame).expect("json"))
            else {
                panic!("a wheel frame with {sent} still arrives");
            };
            assert_eq!((dx, dy), (want, want), "{sent} should clamp to {want}");
        }
        // Encoding follows the same rule, so this engine cannot emit one either.
        let out = encode(&Frame::Wheel {
            session: 1,
            n: 2,
            dx: i32::MAX,
            dy: i32::MIN,
            pixels: true,
        })
        .expect("encodable");
        let v: Value = serde_json::from_slice(&out).expect("json");
        assert_eq!(v["dx"], json!(WHEEL_MAX));
        assert_eq!(v["dy"], json!(-WHEEL_MAX));
    }

    #[test]
    fn nothing_a_peer_chooses_reaches_the_engine_unbounded_or_undefined() {
        // `by` decorates a refusal, so it is dropped over the bound rather than
        // taking the refusal with it: a refusal with no name is still a refusal the
        // session must act on.
        let long_by = json!({ "t": "no", "s": 1, "c": refused::BUSY, "by": "b".repeat(300) });
        let Some(Frame::Refused { by, code, .. }) =
            decode(&serde_json::to_vec(&long_by).expect("json"))
        else {
            panic!("the refusal still arrives");
        };
        assert_eq!(by, None, "an oversized device id names nobody");
        assert_eq!(code, refused::BUSY, "and the refusal itself is intact");
        // A node_id is exactly the bound, so the ordinary case is not the edge case.
        let real_by =
            json!({ "t": "no", "s": 1, "c": refused::BUSY, "by": "a".repeat(DEVICE_ID_MAX) });
        let Some(Frame::Refused { by, .. }) = decode(&serde_json::to_vec(&real_by).expect("json"))
        else {
            panic!("a refusal");
        };
        assert_eq!(by, Some("a".repeat(DEVICE_ID_MAX)));
        // Encoding follows the same rule, so this engine cannot emit one either.
        let bytes = encode(&Frame::Refused {
            session: 1,
            code: refused::BUSY.into(),
            by: Some("b".repeat(DEVICE_ID_MAX + 1)),
        })
        .expect("still sendable");
        assert!(matches!(
            decode(&bytes),
            Some(Frame::Refused { by: None, .. })
        ));

        // A plane id is what makes this session's absolute coordinates mean
        // anything, so an oversized one takes the frame with it.
        for oversized in [
            json!({ "t": "hi", "d": DIALECT, "v": 1, "plane": "p".repeat(500) }),
            json!({ "t": "start", "s": 1, "mode": "full", "keys": "typing",
                    "plane": "p".repeat(500), "n": 1, "x": 0, "y": 0 }),
        ] {
            assert_eq!(
                decode(&serde_json::to_vec(&oversized).expect("json")),
                None,
                "{oversized} must be dropped"
            );
        }

        // Eight bits are defined and eight are not. An undefined one would reach the
        // remapping, the hotkey comparison and the modifier dance, where there is
        // nothing any of them could correctly do with it.
        let all_bits = json!({ "t": "k", "s": 1, "n": 1, "u": 7, "m": 65535,
                               "l": "us", "dn": true });
        let Some(Frame::Key { mods, .. }) = decode(&serde_json::to_vec(&all_bits).expect("json"))
        else {
            panic!("a key frame");
        };
        assert_eq!(
            mods,
            crate::keys::mods::DEFINED,
            "the eight bits section 3 defines, and not one more"
        );

        // The five buttons the dialect defines, and nothing else: the number is
        // handed to the platform as it stands.
        for button in BUTTON_MIN..=BUTTON_MAX {
            let good = json!({ "t": "b", "s": 1, "n": 1, "i": button, "dn": true });
            assert_eq!(
                decode(&serde_json::to_vec(&good).expect("json")),
                Some(Frame::Button {
                    session: 1,
                    n: 1,
                    button,
                    down: true
                })
            );
        }
        for undefined in [0u64, 6, 200, 255] {
            let bad = json!({ "t": "b", "s": 1, "n": 1, "i": undefined, "dn": true });
            assert_eq!(
                decode(&serde_json::to_vec(&bad).expect("json")),
                None,
                "button {undefined} is an action this design never sanctioned"
            );
        }
    }

    /// Garbage, a foreign dialect, a missing field, a wrong type and an unknown
    /// frame type are all DROPPED and none of them panics: a peer is
    /// semi-trusted, and a channel that died on a bad frame would hand it a way
    /// to end a session.
    #[test]
    fn nothing_a_peer_can_send_is_fatal() {
        for bad in [
            &b""[..],
            &b"{"[..],
            &b"null"[..],
            &b"[]"[..],
            &b"{\"t\":\"nope\"}"[..],
            // A handshake in another engine's dialect.
            br#"{"t":"hi","d":"someone-else/1","v":1,"plane":"x"}"#,
            // A start with no plane: absolute coordinates with no agreement
            // about what they mean.
            br#"{"t":"start","s":1,"mode":"full","keys":"typing","n":1,"x":0,"y":0}"#,
            // A mode this build does not know.
            br#"{"t":"start","s":1,"mode":"telepathy","keys":"typing","plane":"x","n":1,"x":0,"y":0}"#,
            // A session id that does not fit u32.
            br#"{"t":"ok","s":99999999999}"#,
            // A coordinate that does not fit i32.
            br#"{"t":"p","s":1,"n":1,"x":99999999999,"y":0}"#,
            // A relative move of nothing.
            br#"{"t":"r","s":1,"n":1,"dx":0,"dy":0}"#,
            // A wheel unit nobody defined.
            br#"{"t":"w","s":1,"n":1,"dx":0,"dy":1,"u":"furlongs"}"#,
            // A button number that does not fit a byte.
            br#"{"t":"b","s":1,"n":1,"i":300,"dn":true}"#,
        ] {
            assert_eq!(decode(bad), None, "{:?} must be dropped", bad);
        }
    }

    /// Unknown fields are ignored, which is what lets a later version add one
    /// without breaking this build.
    #[test]
    fn a_field_this_build_does_not_know_is_ignored() {
        let ahead = br#"{"t":"p","s":1,"n":9,"x":10,"y":20,"pressure":128}"#;
        assert_eq!(
            decode(ahead),
            Some(Frame::Pointer {
                session: 1,
                n: 9,
                x: 10,
                y: 20
            })
        );
    }

    /// Which frames carry the flow counter, and which are positions: the two
    /// questions the coalescing rules ask, pinned so a new frame kind cannot
    /// quietly join or leave either set.
    #[test]
    fn only_the_flow_frames_are_numbered_and_only_two_are_positions() {
        let pointer = Frame::Pointer {
            session: 1,
            n: 5,
            x: 0,
            y: 0,
        };
        let motion = Frame::Motion {
            session: 1,
            n: 6,
            dx: 1,
            dy: 1,
        };
        let key = Frame::Key {
            session: 1,
            n: 7,
            usage: 0,
            key: None,
            sym: Some("a".into()),
            mods: 0,
            layout: "us".into(),
            down: true,
            lock: false,
        };
        assert_eq!(pointer.flow(), Some(5));
        assert_eq!(key.flow(), Some(7));
        assert!(pointer.is_position() && motion.is_position());
        assert!(!key.is_position());
        assert_eq!(Frame::Ping { ms: 1 }.flow(), None);
        // A ping belongs to the channel, not to a session: that is what lets it
        // keep a warm channel alive with no session on it.
        assert_eq!(Frame::Ping { ms: 1 }.session(), None);
        assert_eq!(pointer.session(), Some(1));
    }
}
