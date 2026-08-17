// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The keyboard: the canonical vocabulary that travels, the resolution table,
//! the injection sequence, and modifier hygiene (doc/input-sharing.md,
//! section 8).
//!
//! Nothing here touches an OS. The backend answers one question ("what key and
//! what modifiers produce this on this machine's active layout right now") and
//! this module does everything else: choosing which level of a key frame to
//! resolve, building the press-and-restore sequence, remembering what is held,
//! and releasing it. That split is what makes the hard half of the keyboard
//! testable without a desk (doc/input-sharing.md, D18).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use crate::backend::{Action, InputBackend, KeyEvent, PlatformKey, Resolved, Want};
use crate::wire::KeyMode;

/// The canonical modifier bits. Platform modifiers never travel: each end maps
/// its own to these and back, which is what makes a per machine remapping a
/// table with two canonical sides.
///
/// `META` is one bit for the Windows key, the Command key and Super, because
/// they are one position under three names and the machine that receives decides
/// what it means locally.
pub mod mods {
    pub const SHIFT: u16 = 1 << 0;
    pub const CTRL: u16 = 1 << 1;
    pub const ALT: u16 = 1 << 2;
    pub const ALTGR: u16 = 1 << 3;
    pub const META: u16 = 1 << 4;
    /// Lock STATES, not held keys: they say what the source's locks were set to,
    /// and a target never presses or releases one to match (a lock is toggled by
    /// its own half-duplex frame, never by a modifier set).
    pub const CAPS: u16 = 1 << 5;
    pub const NUM: u16 = 1 << 6;
    pub const SCROLL: u16 = 1 << 7;

    /// The bits a resolution may ask to be HELD. The locks are excluded on
    /// purpose: pressing Caps Lock to satisfy a modifier set would toggle the
    /// machine's lock and leave it that way, which is the opposite of hygiene.
    pub const HOLDABLE: u16 = SHIFT | CTRL | ALT | ALTGR | META;

    /// Every bit this dialect defines, which is exactly the eight of section 3's
    /// table and nothing else.
    ///
    /// The mask [`crate::wire::decode`] applies to an incoming `m`. A peer can put
    /// any of sixteen bits in that field, and the eight undefined ones would reach
    /// the remapping, the hotkey comparison and the modifier dance, where there is
    /// nothing any of them could correctly do with one. Masking makes a peer from a
    /// later version look like a peer holding the modifiers this build understands,
    /// which is the right degradation and the same rule as an unknown key name
    /// falling through the resolution table.
    pub const DEFINED: u16 = HOLDABLE | CAPS | NUM | SCROLL;

    /// The modifiers a LAYOUT uses to choose WHICH SYMBOL a key produces, and
    /// therefore the only ones a symbol resolution is allowed to take away.
    ///
    /// Shift and AltGr select the level of a key: `2` and `@` are one key on
    /// QWERTY and Shift is what picks between them. So "@ is AltGr plus the 0 key
    /// on this machine" is an EXACT statement about these two bits, and honouring
    /// it means releasing the one it does not name (the Shift the source was
    /// holding) as well as pressing the one it does.
    pub const LAYOUT: u16 = SHIFT | ALTGR;

    /// The modifiers that change what a stroke MEANS rather than which character
    /// it produces, and which a symbol resolution therefore never takes away.
    ///
    /// This is the one place the injection sequence departs from the letter of
    /// doc/input-sharing.md section 8, and it is what makes both halves of that
    /// section true at once. The sequence there says "release the modifiers the
    /// target is holding that the combination does not want"; the same section
    /// also says symbol-first resolution is what makes a shortcut work ("Ctrl plus
    /// the key labelled C is what a person means by copy, on any layout"). Both
    /// cannot hold if Control is released in order to produce a "c": the copy
    /// would silently become a letter, on the most used shortcut there is.
    ///
    /// Control, Alt and Meta do not select which character a key produces, so a
    /// resolution that does not name them is not saying they are unwanted, it is
    /// saying nothing about them. They are left exactly as they are, and they
    /// travel to the target as their own key frames (a Control press is a key
    /// press, usage 0xE0) which the held set carries across frames.
    ///
    /// The known cost, recorded rather than hidden: on macOS the Option key IS a
    /// layout modifier (Option plus e is a dead key for an acute accent), so a
    /// Mac target holding Option while a symbol resolves without it produces the
    /// wrong character. That is the lesser failure of the two, it only arises
    /// while a human on the SOURCE holds Option, and accents reach a Mac through
    /// [`Resolved::prefix`] or the Unicode fallback anyway.
    pub const COMMAND: u16 = CTRL | ALT | META;

    /// The canonical name of a single bit, for a snapshot and for a hotkey the
    /// interface renders. `None` for anything that is not exactly one known bit.
    pub fn name(bit: u16) -> Option<&'static str> {
        match bit {
            SHIFT => Some("shift"),
            CTRL => Some("ctrl"),
            ALT => Some("alt"),
            ALTGR => Some("altgr"),
            META => Some("meta"),
            CAPS => Some("caps"),
            NUM => Some("num"),
            SCROLL => Some("scroll"),
            _ => None,
        }
    }

    /// The bit a canonical name means. `None` for a name this build does not
    /// know, so a stored hotkey from a later version degrades to "unknown key"
    /// rather than to a wrong one.
    pub fn bit(name: &str) -> Option<u16> {
        match name {
            "shift" => Some(SHIFT),
            "ctrl" => Some(CTRL),
            "alt" => Some(ALT),
            "altgr" => Some(ALTGR),
            "meta" => Some(META),
            "caps" => Some(CAPS),
            "num" => Some(NUM),
            "scroll" => Some(SCROLL),
            _ => None,
        }
    }

    /// Every holdable bit set in `m`, low bit first: the order a sequence
    /// presses them in, and the reverse of the order it releases them in.
    pub fn holdable_bits(m: u16) -> Vec<u16> {
        [SHIFT, CTRL, ALT, ALTGR, META]
            .into_iter()
            .filter(|bit| m & bit != 0)
            .collect()
    }
}

/// The HID usage page a plain keyboard key lives on. A usage on the wire is
/// `(page << 16) | id`, so the media keys (consumer page 0x0C) can travel in the
/// same field as Enter (keyboard page 0x07).
pub const PAGE_KEYBOARD: u32 = 0x0007;
/// The HID usage page the media keys live on.
pub const PAGE_CONSUMER: u32 = 0x000C;

/// Builds a wire usage from a page and an id.
pub const fn usage(page: u32, id: u32) -> u32 {
    (page << 16) | id
}

/// The frozen table of canonical names for keys that produce no character,
/// paired with their HID usage where one exists.
///
/// Two levels for the same key, deliberately. The name is what makes a frame
/// self-describing in a debug dump and lets a target whose usage table is
/// incomplete still do the right thing; the usage is what a positional
/// resolution wants. Both are cheap, and "virtual key" as a third level does not
/// travel because it is not portable (a Windows virtual key, an X11 keysym and a
/// macOS virtual keycode are three different things).
///
/// An unknown name is never an error: resolution falls through to the next level
/// and, failing everything, reports `UNRESOLVED`. That is what lets a later
/// version add a key without this build mistyping it.
pub const NAMED: &[(&str, u32)] = &[
    ("Enter", usage(PAGE_KEYBOARD, 0x28)),
    ("Escape", usage(PAGE_KEYBOARD, 0x29)),
    ("Backspace", usage(PAGE_KEYBOARD, 0x2A)),
    ("Tab", usage(PAGE_KEYBOARD, 0x2B)),
    ("Space", usage(PAGE_KEYBOARD, 0x2C)),
    ("CapsLock", usage(PAGE_KEYBOARD, 0x39)),
    ("F1", usage(PAGE_KEYBOARD, 0x3A)),
    ("F2", usage(PAGE_KEYBOARD, 0x3B)),
    ("F3", usage(PAGE_KEYBOARD, 0x3C)),
    ("F4", usage(PAGE_KEYBOARD, 0x3D)),
    ("F5", usage(PAGE_KEYBOARD, 0x3E)),
    ("F6", usage(PAGE_KEYBOARD, 0x3F)),
    ("F7", usage(PAGE_KEYBOARD, 0x40)),
    ("F8", usage(PAGE_KEYBOARD, 0x41)),
    ("F9", usage(PAGE_KEYBOARD, 0x42)),
    ("F10", usage(PAGE_KEYBOARD, 0x43)),
    ("F11", usage(PAGE_KEYBOARD, 0x44)),
    ("F12", usage(PAGE_KEYBOARD, 0x45)),
    ("PrintScreen", usage(PAGE_KEYBOARD, 0x46)),
    ("ScrollLock", usage(PAGE_KEYBOARD, 0x47)),
    ("Pause", usage(PAGE_KEYBOARD, 0x48)),
    ("Insert", usage(PAGE_KEYBOARD, 0x49)),
    ("Home", usage(PAGE_KEYBOARD, 0x4A)),
    ("PageUp", usage(PAGE_KEYBOARD, 0x4B)),
    ("Delete", usage(PAGE_KEYBOARD, 0x4C)),
    ("End", usage(PAGE_KEYBOARD, 0x4D)),
    ("PageDown", usage(PAGE_KEYBOARD, 0x4E)),
    ("Right", usage(PAGE_KEYBOARD, 0x4F)),
    ("Left", usage(PAGE_KEYBOARD, 0x50)),
    ("Down", usage(PAGE_KEYBOARD, 0x51)),
    ("Up", usage(PAGE_KEYBOARD, 0x52)),
    ("NumLock", usage(PAGE_KEYBOARD, 0x53)),
    ("NumpadEnter", usage(PAGE_KEYBOARD, 0x58)),
    ("Menu", usage(PAGE_KEYBOARD, 0x65)),
    ("F13", usage(PAGE_KEYBOARD, 0x68)),
    ("F14", usage(PAGE_KEYBOARD, 0x69)),
    ("F15", usage(PAGE_KEYBOARD, 0x6A)),
    ("F16", usage(PAGE_KEYBOARD, 0x6B)),
    ("F17", usage(PAGE_KEYBOARD, 0x6C)),
    ("F18", usage(PAGE_KEYBOARD, 0x6D)),
    ("F19", usage(PAGE_KEYBOARD, 0x6E)),
    ("F20", usage(PAGE_KEYBOARD, 0x6F)),
    ("F21", usage(PAGE_KEYBOARD, 0x70)),
    ("F22", usage(PAGE_KEYBOARD, 0x71)),
    ("F23", usage(PAGE_KEYBOARD, 0x72)),
    ("F24", usage(PAGE_KEYBOARD, 0x73)),
    ("MediaPlayPause", usage(PAGE_CONSUMER, 0xCD)),
    ("MediaStop", usage(PAGE_CONSUMER, 0xB7)),
    ("MediaNext", usage(PAGE_CONSUMER, 0xB5)),
    ("MediaPrevious", usage(PAGE_CONSUMER, 0xB6)),
    ("VolumeUp", usage(PAGE_CONSUMER, 0xE9)),
    ("VolumeDown", usage(PAGE_CONSUMER, 0xEA)),
    ("VolumeMute", usage(PAGE_CONSUMER, 0xE2)),
];

/// The HID usage a canonical name means, if this build knows the name.
pub fn usage_of(name: &str) -> Option<u32> {
    NAMED
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(_, usage)| *usage)
}

/// The canonical name of a HID usage, if this build has one.
pub fn name_of(usage: u32) -> Option<&'static str> {
    NAMED
        .iter()
        .find(|(_, known)| *known == usage)
        .map(|(name, _)| *name)
}

/// Is this key a half-duplex lock, whose press has no release to come?
///
/// Caps Lock, Num Lock and Scroll Lock report a press only on some keyboards,
/// and a target that waited for the release would hold the lock down for ever.
///
/// Three constants and not three lookups in the named table: this is called from inside a
/// platform backend's capture callback, once per key event, and the table walk was three linear
/// scans (plus three `expect`s that can panic where a panic must not happen) for an answer that
/// is three fixed numbers. The three numbers are asserted against the named table by
/// [`tests::the_locks_are_the_three_half_duplex_keys_and_nothing_else`], so they cannot drift
/// away from it.
pub fn is_lock(usage: u32) -> bool {
    matches!(
        usage,
        // Caps Lock, Num Lock, Scroll Lock.
        0x0007_0039 | 0x0007_0053 | 0x0007_0047
    )
}

/// The HID usage of the key that MEANS one canonical modifier bit. `None` for a
/// lock bit, for a combination of bits, and for zero: only a single holdable
/// modifier has a key to press.
///
/// **The LEFT-hand key, deliberately**, except AltGr which is the right Alt
/// because that is what AltGr physically IS (usage 0xE6, Right Alt). A target
/// only ever needs ONE key per modifier to produce a modified stroke, both keys
/// of a pair are the same modifier as far as any layout is concerned, and
/// choosing the left one deterministically means the held set can never end up
/// holding two different keys that mean the same bit. Two keys for one bit would
/// make [`Held::mod_bits`] ambiguous about which one to let go of, and would let
/// a restoration press a key the source never pressed.
pub fn mod_usage(bit: u16) -> Option<u32> {
    match bit {
        mods::SHIFT => Some(usage(PAGE_KEYBOARD, 0xE1)),
        mods::CTRL => Some(usage(PAGE_KEYBOARD, 0xE0)),
        mods::ALT => Some(usage(PAGE_KEYBOARD, 0xE2)),
        mods::ALTGR => Some(usage(PAGE_KEYBOARD, 0xE6)),
        mods::META => Some(usage(PAGE_KEYBOARD, 0xE3)),
        _ => None,
    }
}

/// The canonical modifier bit a HID usage means, if it is a modifier key at all.
///
/// The inverse of [`mod_usage`], widened to both hands: a source that holds Right
/// Shift sends usage 0xE5, and the target must know that the key it is about to
/// press and hold means SHIFT, or [`Held::mod_bits`] would report nothing held
/// and the next symbol resolution would inject a stroke with a Shift the engine
/// cannot see.
///
/// Right Alt maps to ALTGR rather than ALT, matching [`mod_usage`]'s choice, so
/// the two functions agree. On a layout with no AltGr level the right Alt is a
/// plain Alt and this mislabels it by one bit; the cost is confined to
/// [`mods::LAYOUT`] bookkeeping (the engine may release a right Alt it pressed
/// itself, which is hygiene rather than damage) and the alternative, disagreeing
/// with [`mod_usage`], would make a round trip through the two lose the key.
pub fn mod_of_usage(usage: u32) -> Option<u16> {
    match usage {
        u if u == self::usage(PAGE_KEYBOARD, 0xE0) => Some(mods::CTRL),
        u if u == self::usage(PAGE_KEYBOARD, 0xE1) => Some(mods::SHIFT),
        u if u == self::usage(PAGE_KEYBOARD, 0xE2) => Some(mods::ALT),
        u if u == self::usage(PAGE_KEYBOARD, 0xE3) => Some(mods::META),
        u if u == self::usage(PAGE_KEYBOARD, 0xE4) => Some(mods::CTRL),
        u if u == self::usage(PAGE_KEYBOARD, 0xE5) => Some(mods::SHIFT),
        u if u == self::usage(PAGE_KEYBOARD, 0xE6) => Some(mods::ALTGR),
        u if u == self::usage(PAGE_KEYBOARD, 0xE7) => Some(mods::META),
        _ => None,
    }
}

/// Most entries [`Held::from_value`] will read out of a `held.json` this build
/// did not write. A real keyboard cannot hold sixty-four keys, so anything past
/// this is a corrupt or hostile file, and the file is read at START to un-stick a
/// machine: an unbounded read would let it make the engine act on a thousand keys
/// before it has done anything at all.
pub const HELD_MAX: usize = 64;

/// Most keys the PLAN path will add to. Past it a press is refused (and logged),
/// which the caller turns into the Unicode fallback or into `UNRESOLVED`.
///
/// A belt, and the doc on [`Held`] says why it is only a belt: a press requires a
/// successful [`Resolver::resolve`], so the set is bounded by the machine's own
/// keymap and cannot grow past a few hundred by any legitimate path. That reasoning
/// holds exactly as long as the backend refuses to resolve things it has no key
/// for, which is a rule this side of the seam cannot enforce, so a number four
/// times the size of a corrupt-file's worth of keys is where "already a bug" turns
/// into "and stop". It also caps the two O(n) linear scans ([`Held::press`] and
/// [`Held::holds`]) that sit on the 125 Hz path.
///
/// The refusal is in [`plan`] and NOT in [`Held::press`], and the difference is the
/// whole point: nothing has been handed to the OS when a plan gives up, so no key
/// is stranded, where a `press` that silently declined to record would strand the
/// key its own action had already pressed. Releases are never refused, whatever the
/// size of the set, or the keys could never be let go of at all.
pub const HELD_PRESS_MAX: usize = HELD_MAX * 4;

/// One key of the held set, and the bit it means if it is a modifier.
///
/// The bit is stored rather than derived, and that is forced rather than chosen:
/// [`PlatformKey`] is OPAQUE to the engine (a Windows virtual key, an X11 keycode
/// plus group, a macOS virtual keycode), so there is no way back from a code to
/// "this is Shift". The only moment the engine knows is the moment it presses the
/// key, because it knows which [`Want`] it resolved to get there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Entry {
    code: PlatformKey,
    /// `Some(bits)` marks a modifier key, `None` an ordinary one. `Some(0)` is a
    /// modifier whose bit this build cannot name, which is what a `held.json`
    /// written by a later version can produce: still released, claims no bit.
    bit: Option<u16>,
}

/// What the engine pressed on THIS machine and has not released, in press order
/// (doc/input-sharing.md, section 8 and D17).
///
/// The authoritative set, and it holds exactly what WE pressed: never the OS's
/// keyboard state, and never the modifiers a frame announced in its `m` field. A
/// key the machine's own user is holding is none of our business and must never
/// be released by us, and a frame's `m` is the SOURCE's keyboard, which says
/// nothing about what this machine has down. [`Held::mod_bits`] therefore derives
/// the modifier state from the presses this engine made, which is the only set it
/// has the right to change.
///
/// [`Held::press`] itself is not bounded, deliberately. A peer that holds five
/// hundred keys down gets five hundred releases at teardown, which is the correct
/// outcome; refusing to RECORD a press whose action has already been handed to the
/// OS would strand that key on the machine for ever, and that is the one failure
/// this whole structure exists to prevent. The bound is on the LOAD path
/// ([`HELD_MAX`]), where the input is a file rather than a frame and where nothing
/// has been pressed yet, and on the PLAN path ([`HELD_PRESS_MAX`]), where refusing
/// costs nothing because no action has been emitted yet.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Held {
    keys: Vec<Entry>,
}

impl Held {
    pub fn new() -> Held {
        Held::default()
    }

    /// Records a press. `modifier` carries the canonical bit when the key IS a
    /// modifier (see [`Entry::bit`] for why the bit must be handed in rather than
    /// derived), and `None` for an ordinary key.
    ///
    /// A key already held is not recorded twice: auto-repeat sends a press per
    /// repeat and the set must not grow with them. The press order of the first
    /// press is what survives, because that is the order the machine actually
    /// received them in.
    pub fn press(&mut self, code: PlatformKey, modifier: Option<u16>) {
        if let Some(entry) = self.keys.iter_mut().find(|entry| entry.code == code) {
            if modifier.is_some() {
                entry.bit = modifier;
            }
            return;
        }
        self.keys.push(Entry {
            code,
            bit: modifier,
        });
    }

    /// Takes on everything ANOTHER set holds, this set's own press order first.
    ///
    /// For the crash guard, and for one case in it: a release that could not have reached
    /// the OS moves the set it failed to release into the record of keys that may still be
    /// down, and a second failure has to ADD to that record rather than replace it.
    pub fn absorb(&mut self, other: &Held) {
        for entry in &other.keys {
            self.press(entry.code, entry.bit);
        }
    }

    /// Forgets a key. Releasing one that is not held is a no-op rather than an
    /// error: every teardown path calls this on its way out, and a path that
    /// cannot be called twice is a path that will be.
    pub fn release(&mut self, code: PlatformKey) {
        self.keys.retain(|entry| entry.code != code);
    }

    /// Is this key held by us? The question every incoming release asks, and the
    /// whole of "never release what we did not press".
    ///
    /// The WHOLE [`PlatformKey`] is compared, `detail` included, and that is
    /// deliberate rather than incidental. Matching on `code` alone was considered
    /// during review, to make a release impossible to miss after a layout change,
    /// and it is worse: on Windows the ordinary Enter and the numpad Enter are both
    /// `VK_RETURN` and differ only by the extended-key flag that lives in `detail`,
    /// so a code-only match would report "we hold that" about the wrong twin and
    /// [`Held::release`] would then forget BOTH entries, leaving one key physically
    /// down with nothing left to release it. That is the same failure it was meant
    /// to fix, made unrecoverable. The layout-change hole is closed where it belongs
    /// instead: `detail` must carry nothing a layout change can move (documented on
    /// the field), and the caller releases everything on a
    /// [`crate::backend::BackendEvent::LayoutChanged`] (documented there and on
    /// [`Resolver::layout_changed`]).
    pub fn holds(&self, code: PlatformKey) -> bool {
        self.keys.iter().any(|entry| entry.code == code)
    }

    /// The canonical modifier bits we are holding, derived from the modifier keys
    /// we pressed, so the engine never has to trust a frame's `m`.
    pub fn mod_bits(&self) -> u16 {
        self.keys
            .iter()
            .filter_map(|entry| entry.bit)
            .fold(0, |bits, bit| bits | bit)
    }

    /// Everything held, in REVERSE press order: the release-all plan of section
    /// 4's teardown, the drain of `held.json` at the next start, and the remedy a
    /// layout change owes (see [`Resolver::layout_changed`]).
    ///
    /// Reverse, because that is how a hand lets go of a chord: releasing Shift
    /// before the letter it modified can make the letter arrive unmodified on a
    /// platform that reads the modifier state at the release.
    pub fn release_plan(&self) -> Vec<PlatformKey> {
        self.keys.iter().rev().map(|entry| entry.code).collect()
    }

    /// Only the modifiers, in press order: what `held.json` carries.
    ///
    /// Press order and not reverse, because this is the document's order; a
    /// release uses [`Held::release_plan`], which reverses it.
    pub fn modifiers(&self) -> Vec<PlatformKey> {
        self.keys
            .iter()
            .filter(|entry| entry.bit.is_some())
            .map(|entry| entry.code)
            .collect()
    }

    /// The keys we hold that mean any of `bits`, in press order.
    pub fn keys_for(&self, bits: u16) -> Vec<PlatformKey> {
        self.keys
            .iter()
            .filter(|entry| entry.bit.is_some_and(|bit| bit & bits != 0))
            .map(|entry| entry.code)
            .collect()
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn clear(&mut self) {
        self.keys.clear();
    }

    /// `held.json`'s document: the MODIFIERS only, which is the crash guard's
    /// deliberate limit (doc/input-sharing.md, section 8 and D17). An ordinary
    /// character key is down for milliseconds and the damage of stranding one is
    /// a repeated character; a modifier is down for seconds and the damage is a
    /// machine with a dead keyboard, which is the classic failure of every tool
    /// in this category. Writing the file per character would put a file write on
    /// the typing path to buy nothing.
    pub fn to_value(&self) -> Value {
        let keys: Vec<Value> = self
            .keys
            .iter()
            .filter_map(|entry| {
                entry.bit.map(|bit| {
                    json!({ "code": entry.code.code, "detail": entry.code.detail, "mod": bit })
                })
            })
            .collect();
        json!({ "keys": keys })
    }

    /// Reads a `held.json` back, TOLERATING anything.
    ///
    /// An absent field, a wrong type, an entry that is not an object: each is
    /// skipped and the rest is loaded. This file is read at start to un-stick a
    /// machine whose engine died with keys down, so refusing to start because it
    /// is malformed would be the worse failure by a distance: the remedy would be
    /// withheld exactly when it is needed. That is the opposite of the store's
    /// rule for `settings.json` (loud on corruption) and deliberately so: a
    /// garbled grant file may have said something about who can type on this
    /// machine, where a garbled held file can only ever cost a spurious release
    /// of a key that is already up, which every platform treats as a no-op.
    ///
    /// Bounded at [`HELD_MAX`] entries, so a corrupt file cannot make the engine
    /// act on a thousand keys.
    pub fn from_value(v: &Value) -> Held {
        let mut held = Held::new();
        let Some(list) = v.get("keys").and_then(Value::as_array) else {
            return held;
        };
        for entry in list {
            if held.keys.len() >= HELD_MAX {
                break;
            }
            let Some(code) = u32_field(entry, "code") else {
                continue;
            };
            // `detail` defaults rather than skipping the entry: 0 is the plain
            // case on every platform, and the code alone is enough to ask for a
            // release.
            let detail = u32_field(entry, "detail").unwrap_or(0);
            let bit = entry
                .get("mod")
                .and_then(Value::as_u64)
                .and_then(|bit| u16::try_from(bit).ok())
                .map_or(0, |bit| bit & mods::HOLDABLE);
            // Every entry of this file is a modifier by construction, so it is
            // loaded as one even when its bit is unreadable: it must be released,
            // and that does not depend on knowing which bit it was.
            held.press(PlatformKey { code, detail }, Some(bit));
        }
        held
    }
}

fn u32_field(v: &Value, key: &str) -> Option<u32> {
    u32::try_from(v.get(key)?.as_u64()?).ok()
}

/// Most POSITIVE answers [`Resolver`] keeps for one layout.
///
/// A keyboard has about a hundred keys and four levels each, so a few hundred
/// entries covers everything a person can type on one layout, cache hit for cache
/// hit. The bound is not about the keyboard though: a peer chooses what a key
/// frame's `sym` says, and 32 bytes of UTF-8 is a lot of distinct symbols, so
/// without a bound a stream of them is a memory leak with a friendly name.
///
/// At the bound the cache STOPS INSERTING and keeps answering correctly by asking
/// the backend every time. Slower for a workload that could not be helped anyway,
/// and never wrong, which is the right way round: evicting instead would spend
/// bookkeeping to keep a thrashing cache thrashing.
pub const RESOLVE_CACHE_MAX: usize = 512;

/// Most NEGATIVE answers [`Resolver`] keeps for one layout, in their own map.
///
/// Kept apart from the positive ones, and much smaller, because of what a peer can
/// do with the difference. Every entry of the positive map is a key somebody
/// actually typed and will type again, so it pays for itself; every entry of the
/// negative map is a symbol this layout cannot produce, and WHICH symbols those are
/// is entirely the source's choice. A source that sends 512 distinct junk symbols
/// in one burst (well inside the rate cap: that is four seconds of typing at the
/// pointer's own ceiling) used to fill the ONE shared map with negatives, after
/// which the cache never inserted again and every real keystroke of every session
/// on that layout paid a round trip to the OS thread for ever. Splitting the maps
/// makes that impossible: junk can only ever displace junk.
///
/// When this one fills it is CLEARED rather than frozen, which is the opposite
/// choice from the positive map and for a reason. A frozen negative map would stop
/// caching the negatives that matter (a Chinese source typing into a Latin-only
/// target is a legitimate stream of hundreds of distinct unproducible symbols, and
/// section 8's whole point is that it costs one round trip per symbol rather than
/// one per keystroke). Clearing costs at most one round trip per distinct symbol
/// per refill, keeps a working set warm, and cannot touch a positive answer.
pub const NEGATIVE_CACHE_MAX: usize = 128;

/// The cache over the backend's one answering downcall
/// (doc/input-sharing.md, section 10 and D18).
///
/// "What key and what modifiers produce this on the active layout right now" is a
/// round trip to the OS thread, and a character must not pay it twice. Answers
/// are kept per layout identity and the whole cache is dropped when the layout
/// changes, because every answer in it was about a keymap that is gone.
///
/// NEGATIVE answers are cached too, and that is the half that matters on a hot
/// path: a symbol this layout cannot produce is asked about once, not once per
/// keystroke, so a source typing into a target that cannot serve it costs one
/// round trip rather than one per key. They live in their OWN map with its own,
/// smaller bound ([`NEGATIVE_CACHE_MAX`]), because a peer chooses which symbols go
/// in there and one shared map let it push every real answer out.
#[derive(Clone, Debug, Default)]
pub struct Resolver {
    layout: String,
    /// What this layout CAN produce. Stops inserting at [`RESOLVE_CACHE_MAX`].
    answers: BTreeMap<String, Resolved>,
    /// What it cannot. Cleared at [`NEGATIVE_CACHE_MAX`]; see that constant for why
    /// the two maps handle their bound differently.
    misses: BTreeSet<String>,
}

impl Resolver {
    pub fn new() -> Resolver {
        Resolver::default()
    }

    /// The layout identity every cached answer belongs to.
    pub fn layout(&self) -> &str {
        &self.layout
    }

    /// How many POSITIVE answers are cached. The bound's witness.
    pub fn cached(&self) -> usize {
        self.answers.len()
    }

    /// How many negative answers are cached, in their own bounded map.
    pub fn negatives(&self) -> usize {
        self.misses.len()
    }

    /// The active layout changed, so every cached answer is stale.
    ///
    /// An identical identity keeps the cache: a backend may re-announce the
    /// layout it already had (X11 sends a keymap notify for a modifier map
    /// change), and throwing away a warm cache for that would put a round trip
    /// back on the next hundred keystrokes for nothing.
    ///
    /// **The caller owes a layout change more than this call**, and the rest is not
    /// optional: it must re-learn the modifier keys ([`ModKeys::learn`]), seed the
    /// group it tracks from [`crate::backend::BackendEvent::LayoutChanged`]'s
    /// `group` (see [`Stroke::group`]), and RELEASE EVERYTHING IT HOLDS
    /// ([`Held::release_plan`], then [`crate::backend::InputBackend::release_all`],
    /// then [`Held::clear`]). The last one is the one that is easy to miss and the
    /// one that costs a keyboard: every [`PlatformKey`] in the held set was named by
    /// a resolution against the keymap that has just been replaced, so a release
    /// frame arriving after the change can re-resolve to a key that does not compare
    /// equal to the one that was pressed, and [`Held::holds`] then says we are not
    /// holding it. The key stays physically down until teardown; if it is Control,
    /// the machine has a dead keyboard for the rest of the session.
    pub fn layout_changed(&mut self, layout: &str) {
        if self.layout == layout {
            return;
        }
        self.layout = layout.to_string();
        self.answers.clear();
        self.misses.clear();
    }

    /// Resolves through the backend, caching the answer including a negative one.
    pub async fn resolve<B: InputBackend>(&mut self, backend: &B, want: Want) -> Option<Resolved> {
        let key = cache_key(&want);
        if let Some(cached) = self.answers.get(&key) {
            return Some(cached.clone());
        }
        if self.misses.contains(&key) {
            return None;
        }
        let answer = backend.resolve(want).await;
        match &answer {
            Some(how) => {
                if self.answers.len() < RESOLVE_CACHE_MAX {
                    self.answers.insert(key, how.clone());
                }
            }
            None => {
                // Cleared rather than frozen, and only ever this map: see
                // [`NEGATIVE_CACHE_MAX`]. A peer choosing what fills this can make
                // it forget, and can no longer make the real answers unreachable.
                if self.misses.len() >= NEGATIVE_CACHE_MAX {
                    self.misses.clear();
                }
                self.misses.insert(key);
            }
        }
        answer
    }
}

/// The cache's key. A string rather than the [`Want`] itself so the map needs no
/// ordering on a type that will grow variants; nothing outside depends on the
/// shape.
fn cache_key(want: &Want) -> String {
    match want {
        Want::Usage(u) => format!("u:{u}"),
        Want::Named(n) => format!("n:{n}"),
        Want::Symbol(s) => format!("s:{s}"),
    }
}

/// The platform keys of the canonical modifier bits, learned once per layout.
///
/// [`plan`] is pure and synchronous, which is what makes the injection sequence
/// exhaustively testable, so it cannot resolve the modifier keys it may need to
/// press. They are resolved ahead of it, here, from [`mod_usage`] through the
/// same [`Resolver`] as everything else: a modifier key is a HID usage like any
/// other.
///
/// A bit whose key this machine cannot produce is simply absent, and a stroke
/// that would need it is abandoned whole (see [`plan`]) rather than injected with
/// half of its modifiers, which would type a different character.
#[derive(Clone, Debug, Default)]
pub struct ModKeys {
    keys: BTreeMap<u16, PlatformKey>,
}

impl ModKeys {
    pub fn new() -> ModKeys {
        ModKeys::default()
    }

    pub fn set(&mut self, bit: u16, code: PlatformKey) {
        self.keys.insert(bit, code);
    }

    pub fn get(&self, bit: u16) -> Option<PlatformKey> {
        self.keys.get(&bit).copied()
    }

    /// Asks the backend for all five modifier keys. Called when a session starts
    /// and after a [`crate::backend::BackendEvent::LayoutChanged`], next to the
    /// [`Resolver::layout_changed`] that empties the cache.
    ///
    /// Only the resolved CODE is kept: a modifier key needs no modifiers held to
    /// be itself, so a `mods` a backend reports for one is meaningless here, and
    /// a `prefix` or a `group` on one would be a backend bug this engine cannot
    /// act on.
    pub async fn learn<B: InputBackend>(&mut self, backend: &B, resolver: &mut Resolver) {
        for bit in mods::holdable_bits(mods::HOLDABLE) {
            let Some(usage) = mod_usage(bit) else {
                continue;
            };
            match resolver.resolve(backend, Want::Usage(usage)).await {
                Some(how) => {
                    self.keys.insert(bit, how.code);
                }
                // Removed rather than left stale: after a layout change the old
                // answer is about a keymap that no longer exists.
                None => {
                    self.keys.remove(&bit);
                }
            }
        }
    }
}

/// The levels to try, in order, for one key frame under one mode.
///
/// Exactly doc/input-sharing.md section 8's two tables. Direct Unicode injection
/// is not a [`Want`] (it asks the layout nothing) so it does not appear here; it
/// is the fallback [`plan`] adds, and its POSITION in the typing table is second,
/// which [`plan`] recovers from the mode. See D9 for why the symbol leads in
/// `typing` mode against the epic's stated order: the epic's own example (typing
/// `@` on an AZERTY target from a QWERTY source is AltGr plus 0) is unreachable
/// under a usage-first order, because the source pressed the position of the `2`
/// key and that position with Shift produces `2` on AZERTY.
///
/// A `usage` of 0 (the backend could not tell) contributes no level, and so does
/// an absent `key` or `sym`; an empty `sym` counts as absent, since no layout
/// produces nothing. The result therefore holds no duplicates: each level is a
/// different variant and each appears at most once.
pub fn levels(mode: KeyMode, usage: u32, key: Option<&str>, sym: Option<&str>) -> Vec<Want> {
    let positional = (usage != 0).then_some(Want::Usage(usage));
    let named = key.map(|key| Want::Named(key.to_string()));
    let symbol = sym
        .filter(|sym| !sym.is_empty())
        .map(|sym| Want::Symbol(sym.to_string()));
    let ordered = match (mode, symbol.is_some()) {
        // What a person means when they type: the symbol they saw appear.
        (KeyMode::Typing, true) => vec![symbol, positional, named],
        // A key with no symbol is a NAME first: a target whose usage table is
        // incomplete still knows what `Enter` is.
        (KeyMode::Typing, false) => vec![named, positional],
        // What a real HID keyboard would have sent, for games and positional
        // shortcuts.
        (KeyMode::Positional, _) => vec![positional, named, symbol],
    };
    ordered.into_iter().flatten().collect()
}

/// One resolved level: which question was answered, and the answer.
///
/// The pair travels together because WHICH level answered decides how the stroke
/// is sequenced, not just what it presses (see [`plan`]).
#[derive(Clone, Copy, Debug)]
pub struct Answer<'a> {
    pub level: &'a Want,
    pub how: &'a Resolved,
}

impl Answer<'_> {
    /// Did a SYMBOL answer? The one distinction the sequence turns on.
    pub fn from_symbol(&self) -> bool {
        matches!(self.level, Want::Symbol(_))
    }
}

/// One key frame, as much of it as the sequence needs, with the resolution
/// already done.
#[derive(Clone, Copy, Debug)]
pub struct Stroke<'a> {
    /// The session's key mode. The sequence needs it for exactly one reason: in
    /// `typing` mode the symbol level LEADS, so an answer from any other level
    /// proves the symbol could not be produced on this layout, and that is where
    /// section 8's table puts direct Unicode injection (second, ahead of the
    /// positional levels).
    pub mode: KeyMode,
    /// The level that answered, or `None` when none did.
    pub answer: Option<Answer<'a>>,
    /// The frame's `dn`.
    pub down: bool,
    /// The frame's `lk`: a half-duplex lock, a press with no release to come.
    pub lock: bool,
    /// The frame's `sym`, for the Unicode fallback.
    pub sym: Option<&'a str>,
    /// [`crate::backend::Capabilities::unicode`] of THIS machine: can it inject
    /// an arbitrary string at all.
    pub unicode: bool,
    /// The keyboard layout group this machine is in NOW, so a stroke that has to
    /// switch group can switch back to it.
    ///
    /// **The caller MUST seed this from
    /// [`crate::backend::BackendEvent::LayoutChanged`]'s `group`**, and a wrong
    /// value here silently changes the machine's keyboard layout, which is precisely
    /// the failure the switch-back exists to prevent. `plan` switches to
    /// [`Resolved::group`] and then switches back to THIS number, so a caller that
    /// leaves it at 0 while the user is working in group 1 emits
    /// `Group(2) ... Group(0)` and leaves them in group 0: the next thing they type
    /// comes out in the wrong alphabet, and nothing in the session says why.
    ///
    /// It is a plain `u32` and not an `Option` on purpose: 0 is a real group and
    /// there is no honest "unknown" a pure plan could act on. The belt is on the
    /// other side of the seam instead, where the knowledge is: the group travels
    /// with every layout change, so a caller that handles that event at all has the
    /// number in its hand.
    pub group: u32,
}

/// What to do to the OS for one keystroke, and what the held set becomes.
///
/// A keystroke is a SEQUENCE, not an event (doc/input-sharing.md, section 8):
/// producing `@` on an AZERTY target means letting go of the Shift the source is
/// holding, pressing AltGr, striking a key, and putting the machine back exactly
/// as it was.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// Handed to [`InputBackend::inject`] as one batch, applied in order.
    pub actions: Vec<Action>,
    /// The held set AFTER the batch. The caller adopts it once the batch is
    /// handed over.
    pub held: Held,
    /// The held set at its WIDEST point DURING the batch, which is what must be
    /// on disk before the batch is handed over.
    ///
    /// The crash guard needs the peak and not the end state, and the `@` stroke
    /// is exactly why: it holds AltGr in the middle and holds none of it at the
    /// end, so a process death between the two would strand AltGr on a machine
    /// whose `held.json` never mentioned it. Writing the peak first costs one
    /// unnecessary release at the next start in the ordinary case, which every
    /// platform treats as a no-op.
    pub peak: Held,
}

impl Plan {
    /// The plan that does nothing and changes nothing. Not an error: an incoming
    /// release of a key we never pressed, and a lock's absent release, are both
    /// silence by design.
    fn nothing(held: &Held) -> Plan {
        Plan {
            actions: Vec::new(),
            held: held.clone(),
            peak: held.clone(),
        }
    }
}

/// Builds the sequence for one key frame. `None` when nothing can produce it, and
/// the caller then reports `UNRESOLVED` (doc/input-sharing.md, section 13).
///
/// Pure and synchronous on purpose: the async part (asking the backend what
/// produces what) happens before it, in [`Resolver`], which is what makes every
/// rule below provable by a test with no desk in the room.
///
/// # The rules, and the one that is not obvious
///
/// A keystroke resolved from a SYMBOL is ATOMIC: pressed and released inside one
/// plan, and never entered in the held set. A keystroke resolved POSITIONALLY (a
/// usage or a name) follows the frame's own `dn`, so a game holding W keeps W
/// down across frames until the frame that releases it.
///
/// The reason is the modifier dance. A symbol stroke owns the modifiers around
/// it: it releases the source's Shift, presses AltGr, and restores both. That
/// dance MUST NOT LEAK past the stroke, so the stroke has to end inside the plan
/// that started it; leaving the key down and restoring the modifiers around it
/// would hold a key whose modifiers no longer match, and a later release frame
/// would arrive with the machine in a state this plan cannot describe. A
/// positional stroke has no dance to leak (see below), so it can span frames, and
/// it must: `dn` is the whole content of a held key.
///
/// A positional resolution's `mods` is NOT reconciled at all, and that is the
/// second thing to know. A HID usage IS a physical position and needs no
/// modifiers to be that position, while the modifiers a human is holding travel
/// to this machine as their own key frames (a Control press is usage 0xE0) and
/// live in the held set across frames. Reconciling to a positional answer's
/// `mods` would release that Control before pressing the C of a Ctrl plus C.
pub fn plan(stroke: &Stroke<'_>, held: &Held, keys: &ModKeys) -> Option<Plan> {
    // Section 8's rule about locks is FLAT: "a lock is never in the held set:
    // releasing it later would toggle it, which is the opposite of hygiene". So the
    // lock path is chosen by what the key IS and not only by what the frame CLAIMS.
    // The frame's `lk` is the source's manners, and a target must not depend on them
    // for a hygiene rule it owns: a third-party engine holding the same exclusive
    // role, or our own capture path on a backend that reports Caps Lock as a full
    // press and release pair, sends a `k` with the Caps Lock usage and no `lk`. On
    // the flag alone that took the positional path, entered the held set, and the
    // teardown's release-all then sent a Caps Lock release, which TOGGLES the lock
    // and leaves the machine in caps: the exact outcome the rule forbids, arrived at
    // by trusting the sender.
    if stroke.lock
        || stroke
            .answer
            .is_some_and(|answer| answers_a_lock(answer.level))
    {
        return lock_plan(stroke, held);
    }
    if !stroke.down {
        return Some(release_plan(stroke, held));
    }
    // Section 8's typing table puts direct Unicode injection SECOND, ahead of the
    // positional levels, and this is where that ordering lands: in `typing` mode
    // the symbol level leads, so an answer from another level is proof that the
    // symbol could not be produced here. Pressing the position instead would type
    // the wrong character (the `2` of a QWERTY `@`) where injecting the text types
    // the right one.
    if symbol_level_failed(stroke)
        && let Some(plan) = text_plan(stroke, held)
    {
        return Some(plan);
    }
    if let Some(answer) = stroke.answer
        && let Some(plan) = press_plan(stroke, answer, held, keys)
    {
        return Some(plan);
    }
    // Nothing resolved, or the resolution needed a modifier this machine has no
    // key for. Either way the last resort is the same.
    text_plan(stroke, held)
}

/// Does the level that answered name a lock key?
///
/// The engine's half of section 8's flat rule, asked of the RESOLUTION rather than
/// of the frame's `lk` flag (see [`plan`]). A symbol never names a lock: no layout
/// produces a character from Caps Lock, and a source that put one in `sym` is
/// describing something this table cannot recognise anyway.
fn answers_a_lock(level: &Want) -> bool {
    match level {
        Want::Usage(usage) => is_lock(*usage),
        Want::Named(name) => usage_of(name).is_some_and(is_lock),
        Want::Symbol(_) => false,
    }
}

/// A half-duplex lock (Caps Lock, Num Lock, Scroll Lock on some keyboards): a
/// press with no release, and the key is NEVER entered in the held set.
///
/// Releasing it later would toggle the machine's lock, which is the opposite of
/// hygiene: the teardown would leave the machine in caps.
fn lock_plan(stroke: &Stroke<'_>, held: &Held) -> Option<Plan> {
    // A lock frame carries a press. A release of one is a source that did not
    // read the dialect: nothing to do, and nothing to complain about.
    if !stroke.down {
        return Some(Plan::nothing(held));
    }
    // No Unicode fallback: no string injection can toggle a lock, so a lock this
    // machine cannot name is genuinely `UNRESOLVED`. No modifier dance either: a
    // lock key is a lock key on every level of every layout.
    let answer = stroke.answer?;
    Some(Plan {
        actions: vec![Action::Key {
            code: answer.how.code,
            down: true,
        }],
        held: held.clone(),
        peak: held.clone(),
    })
}

/// A release: let the key go if WE are holding it, and say nothing at all if we
/// are not.
///
/// Never release what we did not press. That covers three cases with one rule:
/// the machine's own user holding that key, a symbol stroke whose release already
/// happened inside its own plan, and a stroke that was served by the Unicode
/// fallback and therefore never pressed anything.
fn release_plan(stroke: &Stroke<'_>, held: &Held) -> Plan {
    // Nothing resolved, so there is no code to release. Silence rather than
    // `UNRESOLVED`: the press was already reported, and reporting the pair would
    // double every refusal the interface shows.
    let Some(answer) = stroke.answer else {
        return Plan::nothing(held);
    };
    let code = answer.how.code;
    if !held.holds(code) {
        return Plan::nothing(held);
    }
    let mut after = held.clone();
    after.release(code);
    Plan {
        actions: vec![Action::Key { code, down: false }],
        held: after,
        // The peak is the set BEFORE, not after: the key being released was down
        // when the batch started, and the peak's contract is what a crash in the
        // middle of the batch could strand.
        peak: held.clone(),
    }
}

/// Did the symbol level get its chance and fail? True only in `typing` mode,
/// where the symbol leads, and only when the frame carried one.
fn symbol_level_failed(stroke: &Stroke<'_>) -> bool {
    stroke.mode == KeyMode::Typing
        && stroke.sym.is_some_and(|sym| !sym.is_empty())
        && !stroke.answer.is_some_and(|answer| answer.from_symbol())
}

/// The Unicode fallback: one string injection, on the PRESS only.
///
/// A release of a text injection is meaningless (there is no key down to let go
/// of), and nothing enters the held set, so the teardown has nothing to undo.
fn text_plan(stroke: &Stroke<'_>, held: &Held) -> Option<Plan> {
    if !stroke.unicode || !stroke.down {
        return None;
    }
    let sym = stroke.sym.filter(|sym| !sym.is_empty())?;
    Some(Plan {
        actions: vec![Action::Text(sym.to_string())],
        held: held.clone(),
        peak: held.clone(),
    })
}

/// The press sequence. `None` when the resolution needs a modifier this machine
/// has no key for, which the caller turns into the Unicode fallback or into
/// `UNRESOLVED`: half a modifier set types a different character, so the stroke
/// is abandoned whole. Nothing has been handed to the OS at that point, because
/// everything here is built in locals.
fn press_plan(
    stroke: &Stroke<'_>,
    answer: Answer<'_>,
    held: &Held,
    keys: &ModKeys,
) -> Option<Plan> {
    // The belt of [`HELD_PRESS_MAX`], and it is here rather than in [`Held::press`]
    // because here nothing has been handed to the OS yet: returning `None` costs
    // this one keystroke (the caller falls back to Unicode or reports
    // `UNRESOLVED`), where a `press` that declined to record would strand a key its
    // own action had already pressed. A set this size is already a bug, and the
    // reasoning that says it cannot happen rests on the backend refusing to resolve
    // what it has no key for, which is not ours to enforce.
    if held.len() >= HELD_PRESS_MAX {
        eprintln!(
            "[1device-input] refusing to press a {}th key: something is holding far more than a \
             keyboard can",
            held.len() + 1
        );
        return None;
    }
    let how = answer.how;
    let owns_modifiers = answer.from_symbol();
    let mut actions = Vec::new();
    let mut work = held.clone();
    let mut peak = held.clone();
    // The modifier keys THIS plan pressed, in press order, so the restoration can
    // let go of exactly those and of nothing else. Not a diff taken at the end:
    // the key a positional stroke holds down is sometimes itself a modifier (a
    // Shift press travels as usage 0xE1), and a diff would release the very key
    // the frame asked to hold.
    let mut added: Vec<PlatformKey> = Vec::new();

    // The group first, so everything between the two switches happens on the
    // keymap the resolution was about.
    let group = how.group.filter(|group| *group != stroke.group);
    if let Some(group) = group {
        actions.push(Action::Group(group));
    }

    // A dead key: the same machinery with two strokes. Its own modifiers, pressed
    // and released before the base key, and always atomic (a dead key is a
    // stroke, never a state).
    if let Some(prefix) = &how.prefix {
        if owns_modifiers {
            dance(&work, prefix.mods, keys)?.apply(
                &mut actions,
                &mut work,
                &mut peak,
                &mut added,
                held,
            );
        }
        actions.push(Action::Key {
            code: prefix.code,
            down: true,
        });
        actions.push(Action::Key {
            code: prefix.code,
            down: false,
        });
        // In the PEAK, like the base key below and for the same reason: the peak's
        // contract is "everything a crash in the middle of this batch could strand",
        // and between those two actions the dead key is down. It was left out
        // originally and was harmless only by accident, because `Held::to_value`
        // filters the peak to modifiers and a dead key is not one; the two keys of
        // one stroke being treated differently for no stated reason is how the
        // prefix escapes the day the crash guard carries character keys.
        peak.press(prefix.code, None);
        // The prefix's modifiers are not restored here: the base key's own dance
        // follows immediately and reconciles from wherever the prefix left them,
        // which saves two strokes on every accented character.
    }

    if owns_modifiers {
        dance(&work, how.mods, keys)?.apply(&mut actions, &mut work, &mut peak, &mut added, held);
    }

    actions.push(Action::Key {
        code: how.code,
        down: true,
    });
    if owns_modifiers {
        actions.push(Action::Key {
            code: how.code,
            down: false,
        });
        // Down for the length of the batch and no longer, so it never enters the
        // held set. It IS in the peak, which is what the peak is for.
        peak.press(how.code, None);
    } else {
        let bit = positional_bit(answer.level);
        work.press(how.code, bit);
        peak.press(how.code, bit);
    }

    // The restoration, and it is only ever about OUR keys: the modifiers the
    // machine's own user is holding are not in the held set, so they are not in
    // any of these lists. Release what this plan added, newest first; press back
    // what it took away, in the order the source pressed them.
    for code in added.iter().rev() {
        actions.push(Action::Key {
            code: *code,
            down: false,
        });
        work.release(*code);
    }
    for entry in &held.keys {
        if entry.bit.is_some() && !work.holds(entry.code) {
            actions.push(Action::Key {
                code: entry.code,
                down: true,
            });
            work.press(entry.code, entry.bit);
        }
    }

    if group.is_some() {
        actions.push(Action::Group(stroke.group));
    }
    Some(Plan {
        actions,
        held: work,
        peak,
    })
}

/// The bit a positionally resolved key means, if it is a modifier key.
///
/// This is how a Control press travelling as usage 0xE0 ends up in the held set
/// as a modifier: without it [`Held::mod_bits`] would report nothing held and the
/// next symbol resolution would inject its stroke around a Shift the engine
/// cannot see.
fn positional_bit(level: &Want) -> Option<u16> {
    match level {
        Want::Usage(usage) => mod_of_usage(*usage),
        // The frozen named table holds no modifier, so this is `None` today. It
        // is written as the same question rather than as `None` because the table
        // is allowed to grow.
        Want::Named(name) => usage_of(name).and_then(mod_of_usage),
        Want::Symbol(_) => None,
    }
}

/// The modifier reconciliation of one stroke, computed before anything is
/// emitted.
struct Dance {
    /// Ours to let go of, in the order they were pressed (emitted in reverse).
    release: Vec<PlatformKey>,
    /// Ours to add, in canonical press order, each with the bit it means.
    press: Vec<(PlatformKey, u16)>,
}

/// What has to change for this machine to hold exactly what a symbol resolution
/// asks for. `None` when a wanted modifier has no key here.
///
/// Only [`mods::LAYOUT`] bits are ever taken away, and [`mods::COMMAND`] carries
/// the reason at length: releasing the Control of a Ctrl plus C in order to
/// produce a "c" would turn a copy into a letter.
fn dance(held: &Held, want: u16, keys: &ModKeys) -> Option<Dance> {
    let have = held.mod_bits();
    let release = held.keys_for(have & mods::LAYOUT & !want);
    let mut press = Vec::new();
    for bit in mods::holdable_bits(want & !have) {
        press.push((keys.get(bit)?, bit));
    }
    Some(Dance { release, press })
}

impl Dance {
    /// Emits the reconciliation and advances the working sets.
    ///
    /// `before` is the held set the whole plan started from, and it is what
    /// decides whether a press has to be undone later: a key that was already
    /// held before the plan is being put BACK where it was, not added.
    fn apply(
        &self,
        actions: &mut Vec<Action>,
        work: &mut Held,
        peak: &mut Held,
        added: &mut Vec<PlatformKey>,
        before: &Held,
    ) {
        for code in self.release.iter().rev() {
            actions.push(Action::Key {
                code: *code,
                down: false,
            });
            work.release(*code);
            // A key this plan pressed and now lets go of needs no restoration.
            added.retain(|held| held != code);
        }
        for (code, bit) in &self.press {
            actions.push(Action::Key {
                code: *code,
                down: true,
            });
            work.press(*code, Some(*bit));
            peak.press(*code, Some(*bit));
            if !before.holds(*code) && !added.contains(code) {
                added.push(*code);
            }
        }
    }
}

/// Applies this machine's incoming remapping to a frame's canonical modifier
/// bits, before resolution, so everything downstream sees one vocabulary
/// (doc/input-sharing.md, section 8, and D10).
///
/// The map has two canonical sides, name to name: `{"ctrl": "meta"}` is what a PC
/// driving a Mac wants. The default is the IDENTITY, and that is a decision:
/// guessing that a Mac target wants Control and Command swapped is an opinion,
/// and an opinion that is wrong for half of its users is worse than a switch.
///
/// A SIMULTANEOUS permutation, never a sequence of substitutions. Mapping ctrl to
/// meta and meta to ctrl must SWAP the two, where applying the entries one after
/// another would collapse both onto one bit and lose a modifier. Unknown names,
/// on either side, are ignored: a table written by a later version degrades to
/// the identity rather than to a wrong modifier.
///
/// Lock bits pass through untouched, and a map entry whose target names a lock is
/// ignored: no keyboard can hold Caps Lock the way it holds Shift, and pressing
/// it to satisfy a resolution would toggle the machine's lock and leave it that
/// way.
///
/// # The map MUST be injective on the holdable bits, and this function does not
/// # check it
///
/// A simultaneous permutation is only a permutation when no two names map to the
/// same bit. `{"ctrl": "meta"}` with Ctrl AND Meta both held yields META alone: a
/// modifier VANISHES, silently, on the very path a person takes when they are trying
/// to make a Mac and a PC share a desk. That is not a defect of the operation, it is
/// a map that is not a permutation, and there is nothing this function can do about
/// it once it has been asked: dropping the collision would lose a modifier, keeping
/// both would invent one the table did not ask for, and refusing here would mean
/// dropping a keystroke on the hot path for a configuration mistake made hours ago.
///
/// So the requirement is stated here and enforced where the map ARRIVES:
/// `Settings::set_remap` must refuse a map whose holdable targets collide, at the
/// moment `input.remap` is called, where a refusal is a sentence a person can read
/// and act on. This function assumes what that guard promises, and behaves
/// predictably rather than correctly if it is ever wrong.
pub fn remap(bits: u16, map: &BTreeMap<String, String>) -> u16 {
    let mut out = bits & !mods::HOLDABLE;
    for bit in mods::holdable_bits(bits) {
        let to = mods::name(bit)
            .and_then(|name| map.get(name))
            .and_then(|to| mods::bit(to))
            .filter(|to| to & mods::HOLDABLE != 0)
            .unwrap_or(bit);
        out |= to;
    }
    out
}

/// Most entries a stored hotkey may name: the five holdable modifiers and one
/// key.
pub const HOTKEY_MAX: usize = 6;

/// The order a chord is PRESENTED in, which is not the order its bits are in.
///
/// A hotkey is a set of modifiers plus one key, so it needs one spelling: an
/// interface that rendered whatever order a caller happened to type would show
/// two different chords for one binding, and a comparison would have to sort
/// first. That much argues for any fixed order. Which one it is, though, is a
/// question about people rather than about bits, and the bit order (shift first,
/// because it is bit 0) gets it wrong: nobody writes "Shift+Ctrl+Home".
///
/// So the order here is the one all three desktops present chords in. Windows
/// writes Ctrl+Alt+Shift+Win; macOS menus render Control, Option, Shift, Command;
/// the X11 world follows Windows. Control leads, the alternate keys come next,
/// Shift sits late, and the platform key is last, on all three. The bitfield on
/// the wire is unaffected: it is a set there and has no order at all.
const PRESENTED: [u16; 5] = [mods::CTRL, mods::ALT, mods::ALTGR, mods::SHIFT, mods::META];

/// The chord that brings the keyboard home (doc/input-sharing.md, section 8 and
/// D20).
///
/// Recognised in the CAPTURED stream by the machine that captures, swallowed
/// there, never forwarded and never negotiated. It works while a session is live,
/// which is exactly when the source sees every key, and it works when the channel
/// is dead, because nothing about it involves the channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hotkey {
    /// The holdable modifier bits it requires, and requires exactly.
    pub mods: u16,
    /// The HID usage of its key.
    pub usage: u32,
}

impl Hotkey {
    /// The default: Ctrl plus Alt plus Escape.
    ///
    /// Every candidate is taken somewhere, so the reasoning is recorded rather
    /// than left to look arbitrary. Ctrl plus Alt plus Delete is reserved by
    /// Windows and unreachable from a hook. Command plus Option plus Escape is
    /// Force Quit on macOS. A bare modifier double tap fires by accident.
    /// Control rather than Command is what makes ONE default work on all three
    /// desktops.
    pub fn default_hotkey() -> Hotkey {
        Hotkey {
            mods: mods::CTRL | mods::ALT,
            usage: usage_of("Escape").expect("Escape is in the frozen table"),
        }
    }

    /// Parses `["ctrl", "alt", "Escape"]`: canonical modifier names plus exactly
    /// one named key from the frozen table.
    ///
    /// `None` for anything else, and that includes a name this build does not
    /// know: a stored hotkey from a later version must degrade to "this build
    /// cannot honour that" and leave the default in place, never to a DIFFERENT
    /// chord. Silently dropping the unknown half would arm a hotkey the owner
    /// never chose, on a keyboard they cannot bring home.
    ///
    /// Two shapes are refused that the brief did not name, both for the same
    /// reason as the double tap: a chord with no modifier (a bare `Escape` would
    /// swallow every Escape a session ever sees) and a lock as a chord member (a
    /// lock is a STATE, not something a hand holds, and it cannot be compared
    /// against what is held).
    pub fn parse(keys: &[String]) -> Option<Hotkey> {
        if keys.is_empty() || keys.len() > HOTKEY_MAX {
            return None;
        }
        let mut bits = 0u16;
        let mut usage = None;
        for name in keys {
            if let Some(bit) = mods::bit(name) {
                if bit & mods::HOLDABLE == 0 {
                    return None;
                }
                bits |= bit;
                continue;
            }
            if usage.is_some() {
                return None;
            }
            usage = Some(usage_of(name)?);
        }
        let usage = usage?;
        if bits == 0 {
            return None;
        }
        Some(Hotkey { mods: bits, usage })
    }

    /// The same shape back, for the snapshot the interface renders. Modifiers in
    /// the canonical press order, then the key.
    pub fn to_names(&self) -> Vec<String> {
        let mut names: Vec<String> = PRESENTED
            .iter()
            .filter(|bit| self.mods & *bit != 0)
            .filter_map(|bit| mods::name(*bit))
            .map(str::to_string)
            .collect();
        if let Some(name) = name_of(self.usage) {
            names.push(name.to_string());
        }
        names
    }

    /// Does this captured event fire it?
    ///
    /// Only on a PRESS: firing on the release too would bring the keyboard home
    /// and then immediately act on the second half of the chord.
    ///
    /// The modifier bits must match EXACTLY among the holdable ones, so Ctrl plus
    /// Alt plus Shift plus Escape does not fire a Ctrl plus Alt plus Escape
    /// hotkey. An accidental extra modifier must not send the keyboard home in
    /// the middle of a shortcut, and the cost of being strict is that the chord
    /// simply does not fire, which the owner notices and repeats; the cost of
    /// being loose is a session ending under a hand that was doing something
    /// else. Lock bits are excluded from the comparison, because Caps Lock being
    /// on is not part of any chord.
    pub fn fires(&self, event: &KeyEvent) -> bool {
        if !event.down || event.mods & mods::HOLDABLE != self.mods {
            return false;
        }
        if event.usage == self.usage && self.usage != 0 {
            return true;
        }
        // A backend that cannot tell the usage still names the key, and the
        // return hotkey is the one chord that must work on a backend having a bad
        // day: it is the way out.
        self.usage != 0 && event.usage == 0 && event.key.as_deref() == name_of(self.usage)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::fake::FakeBackend;

    fn pk(code: u32) -> PlatformKey {
        PlatformKey { code, detail: 0 }
    }

    fn down(code: u32) -> Action {
        Action::Key {
            code: pk(code),
            down: true,
        }
    }

    fn up(code: u32) -> Action {
        Action::Key {
            code: pk(code),
            down: false,
        }
    }

    /// A frame's stroke with the ordinary flags: a press, not a lock, on a target
    /// that can inject Unicode, in group 0.
    fn stroke<'a>(mode: KeyMode, answer: Option<Answer<'a>>, sym: Option<&'a str>) -> Stroke<'a> {
        Stroke {
            mode,
            answer,
            down: true,
            lock: false,
            sym,
            unicode: true,
            group: 0,
        }
    }

    /// Resolves one key frame exactly as the session loop will: the levels in
    /// order, the first that answers wins.
    async fn first_answer(
        resolver: &mut Resolver,
        backend: &FakeBackend,
        wants: &[Want],
    ) -> Option<(usize, Resolved)> {
        for (at, want) in wants.iter().enumerate() {
            if let Some(how) = resolver.resolve(backend, want.clone()).await {
                return Some((at, how));
            }
        }
        None
    }

    /// A target that speaks a QWERTY-like layout for the plan tests: Shift,
    /// Control and AltGr exist as keys, so [`ModKeys`] can be learned.
    async fn armed() -> (FakeBackend, Resolver, ModKeys) {
        // The upcall receiver is dropped: nothing here emits one, and a plan is
        // built from downcalls alone.
        let (fake, _) = FakeBackend::new();
        fake.teach_usage(mod_usage(mods::SHIFT).expect("shift"), 16);
        fake.teach_usage(mod_usage(mods::CTRL).expect("ctrl"), 17);
        fake.teach_usage(mod_usage(mods::ALT).expect("alt"), 18);
        fake.teach_usage(mod_usage(mods::ALTGR).expect("altgr"), 165);
        fake.teach_usage(mod_usage(mods::META).expect("meta"), 91);
        let mut resolver = Resolver::new();
        resolver.layout_changed("test-layout");
        let mut keys = ModKeys::new();
        keys.learn(&fake, &mut resolver).await;
        (fake, resolver, keys)
    }

    /// The table is a contract: a name maps to exactly one usage and back, with
    /// no duplicate on either side. A duplicate would make one of the two
    /// unreachable, silently, and the wire would carry a key the target could
    /// never resolve.
    #[test]
    fn the_named_key_table_is_a_bijection() {
        for (name, usage) in NAMED {
            assert_eq!(usage_of(name), Some(*usage), "{name} maps to its usage");
            assert_eq!(name_of(*usage), Some(*name), "{usage:#x} maps back");
        }
        let mut names: Vec<&str> = NAMED.iter().map(|(n, _)| *n).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "a duplicate name hides one of the two");
        let mut usages: Vec<u32> = NAMED.iter().map(|(_, u)| *u).collect();
        usages.sort_unstable();
        usages.dedup();
        assert_eq!(
            usages.len(),
            count,
            "a duplicate usage hides one of the two"
        );
    }

    /// Every name fits the wire's bound, so a named key can never be the reason
    /// a frame is clipped.
    #[test]
    fn every_canonical_name_fits_the_wire() {
        for (name, _) in NAMED {
            assert!(
                name.len() <= crate::wire::KEY_NAME_MAX,
                "{name} is longer than the wire allows"
            );
        }
    }

    /// The modifier vocabulary round trips, the locks are not holdable, and the
    /// press order is stable. The last one matters: a sequence presses in this
    /// order and releases in the reverse, and a table that reordered itself
    /// between two builds would release keys it never pressed.
    #[test]
    fn the_modifier_vocabulary_round_trips_and_only_the_real_modifiers_are_held() {
        for bit in [
            mods::SHIFT,
            mods::CTRL,
            mods::ALT,
            mods::ALTGR,
            mods::META,
            mods::CAPS,
            mods::NUM,
            mods::SCROLL,
        ] {
            let name = mods::name(bit).expect("every bit has a name");
            assert_eq!(mods::bit(name), Some(bit));
        }
        assert_eq!(mods::name(0), None);
        assert_eq!(mods::name(mods::SHIFT | mods::CTRL), None);
        assert_eq!(mods::bit("command"), None);

        assert_eq!(mods::HOLDABLE & mods::CAPS, 0, "a lock is never held");
        assert_eq!(
            mods::holdable_bits(u16::MAX),
            vec![mods::SHIFT, mods::CTRL, mods::ALT, mods::ALTGR, mods::META],
            "the press order is fixed"
        );
        assert_eq!(
            mods::holdable_bits(mods::CAPS | mods::NUM),
            Vec::<u16>::new(),
            "locks contribute nothing to hold"
        );
    }

    #[test]
    fn the_locks_are_the_three_half_duplex_keys_and_nothing_else() {
        assert!(is_lock(usage_of("CapsLock").expect("in the table")));
        assert!(is_lock(usage_of("NumLock").expect("in the table")));
        assert!(is_lock(usage_of("ScrollLock").expect("in the table")));
        assert!(!is_lock(usage_of("Enter").expect("in the table")));
        assert!(!is_lock(usage(PAGE_KEYBOARD, 0x04)));
    }

    /// The page lives in the high half of the usage, so a media key and a
    /// keyboard key with the same id can never be confused.
    #[test]
    fn the_page_travels_with_the_id() {
        assert_eq!(usage(PAGE_KEYBOARD, 0x28), 0x0007_0028);
        assert_eq!(usage(PAGE_CONSUMER, 0xCD), 0x000C_00CD);
        assert_ne!(usage(PAGE_KEYBOARD, 0xCD), usage(PAGE_CONSUMER, 0xCD));
    }

    /// One key per modifier, the left-hand one, except AltGr which is physically
    /// the right Alt. Distinct, so the held set can never hold two keys meaning
    /// the same bit, and reversible, so a modifier arriving as its own key frame
    /// is recognised for what it is.
    #[test]
    fn each_modifier_has_one_left_hand_key_and_altgr_is_the_right_alt() {
        assert_eq!(mod_usage(mods::SHIFT), Some(usage(PAGE_KEYBOARD, 0xE1)));
        assert_eq!(mod_usage(mods::CTRL), Some(usage(PAGE_KEYBOARD, 0xE0)));
        assert_eq!(mod_usage(mods::ALT), Some(usage(PAGE_KEYBOARD, 0xE2)));
        assert_eq!(
            mod_usage(mods::ALTGR),
            Some(usage(PAGE_KEYBOARD, 0xE6)),
            "AltGr is the RIGHT Alt, because that is what AltGr physically is"
        );
        assert_eq!(mod_usage(mods::META), Some(usage(PAGE_KEYBOARD, 0xE3)));

        let mut usages: Vec<u32> = mods::holdable_bits(mods::HOLDABLE)
            .into_iter()
            .filter_map(mod_usage)
            .collect();
        assert_eq!(usages.len(), 5, "every holdable bit has a key");
        usages.sort_unstable();
        usages.dedup();
        assert_eq!(usages.len(), 5, "two bits sharing a key would be ambiguous");

        // A lock, a combination and zero have no key: pressing Caps Lock to
        // satisfy a modifier set would toggle the machine's lock.
        assert_eq!(mod_usage(mods::CAPS), None);
        assert_eq!(mod_usage(mods::NUM), None);
        assert_eq!(mod_usage(mods::SHIFT | mods::CTRL), None);
        assert_eq!(mod_usage(0), None);

        for bit in mods::holdable_bits(mods::HOLDABLE) {
            let usage = mod_usage(bit).expect("a key");
            assert_eq!(mod_of_usage(usage), Some(bit), "the pair round trips");
        }
        // Both hands are recognised: a source holding the right Shift sends
        // 0xE5, and a key the engine holds without knowing what it means would
        // make the whole modifier dance blind.
        assert_eq!(mod_of_usage(usage(PAGE_KEYBOARD, 0xE4)), Some(mods::CTRL));
        assert_eq!(mod_of_usage(usage(PAGE_KEYBOARD, 0xE5)), Some(mods::SHIFT));
        assert_eq!(mod_of_usage(usage(PAGE_KEYBOARD, 0xE7)), Some(mods::META));
        assert_eq!(mod_of_usage(usage(PAGE_KEYBOARD, 0x04)), None);
        assert_eq!(mod_of_usage(0), None);
    }

    /// The held set is press-ordered and releases in reverse, it derives its
    /// modifier bits from what was pressed rather than from any frame, and it
    /// never lets go of a key it did not press.
    #[test]
    fn the_held_set_is_what_we_pressed_and_it_releases_in_reverse_order() {
        let mut held = Held::new();
        assert!(held.is_empty());
        assert_eq!(held.mod_bits(), 0);

        held.press(pk(16), Some(mods::SHIFT));
        held.press(pk(17), Some(mods::CTRL));
        held.press(pk(65), None);
        assert_eq!(held.len(), 3);
        assert_eq!(
            held.release_plan(),
            vec![pk(65), pk(17), pk(16)],
            "reverse press order: a chord is let go of the way a hand lets go"
        );
        assert_eq!(
            held.mod_bits(),
            mods::SHIFT | mods::CTRL,
            "derived from the modifier keys we pressed, never from a frame's m"
        );
        assert_eq!(held.modifiers(), vec![pk(16), pk(17)]);
        assert_eq!(held.keys_for(mods::CTRL), vec![pk(17)]);
        assert_eq!(held.keys_for(mods::LAYOUT), vec![pk(16)]);
        assert!(held.holds(pk(65)) && !held.holds(pk(66)));

        // Auto-repeat presses the same key again and again; the set must not grow
        // with it, and the first press's position is the one that happened.
        held.press(pk(16), Some(mods::SHIFT));
        assert_eq!(held.len(), 3);
        assert_eq!(held.release_plan(), vec![pk(65), pk(17), pk(16)]);

        // Releasing a key we never pressed is a no-op, not an error: every
        // teardown path calls this, and a path that cannot be called twice is a
        // path that will be.
        held.release(pk(99));
        assert_eq!(held.len(), 3);
        held.release(pk(17));
        assert_eq!(held.mod_bits(), mods::SHIFT);
        held.clear();
        assert!(held.is_empty() && held.release_plan().is_empty());
    }

    /// `held.json` carries the MODIFIERS and nothing else (D17: a stranded
    /// character key costs a repeated letter, a stranded modifier costs a machine
    /// with a dead keyboard), and what it carries comes back with its bit.
    #[test]
    fn the_held_file_carries_only_the_modifiers_and_round_trips() {
        let mut held = Held::new();
        held.press(pk(16), Some(mods::SHIFT));
        held.press(pk(65), None);
        held.press(pk(165), Some(mods::ALTGR));

        let doc = held.to_value();
        assert_eq!(
            doc,
            json!({ "keys": [
                { "code": 16, "detail": 0, "mod": mods::SHIFT },
                { "code": 165, "detail": 0, "mod": mods::ALTGR },
            ] }),
            "the character key is not in the crash guard, on purpose"
        );

        let back = Held::from_value(&doc);
        assert_eq!(back.release_plan(), vec![pk(165), pk(16)]);
        assert_eq!(back.mod_bits(), mods::SHIFT | mods::ALTGR);
        assert_eq!(
            back.to_value(),
            doc,
            "the document is a fixed point of the pair"
        );
    }

    /// A garbled held file loads what it can and skips the rest. Refusing to
    /// start over it would withhold the remedy exactly when it is needed: this
    /// file is read to un-stick a machine whose engine died with keys down.
    #[test]
    fn a_garbled_held_file_loads_what_it_can_and_bounds_what_it_loads() {
        assert!(Held::from_value(&json!(null)).is_empty());
        assert!(Held::from_value(&json!({})).is_empty());
        assert!(Held::from_value(&json!({ "keys": "several" })).is_empty());
        assert!(Held::from_value(&json!({ "keys": [] })).is_empty());

        let garbled = json!({ "keys": [
            7,
            "shift",
            { "detail": 3 },
            { "code": "sixteen" },
            { "code": 16, "detail": 0, "mod": "shift" },
            { "code": 17, "mod": mods::CTRL },
        ] });
        let held = Held::from_value(&garbled);
        assert_eq!(held.release_plan(), vec![pk(17), pk(16)], "two survive");
        assert_eq!(
            held.mod_bits(),
            mods::CTRL,
            "an unreadable bit claims nothing and is still released"
        );

        let flood: Vec<serde_json::Value> = (0..1000)
            .map(|code| json!({ "code": code, "detail": 0, "mod": mods::SHIFT }))
            .collect();
        let held = Held::from_value(&json!({ "keys": flood }));
        assert_eq!(
            held.len(),
            HELD_MAX,
            "a corrupt file cannot make the engine act on a thousand keys"
        );
    }

    /// Section 8's two tables, level for level, with every combination of present
    /// and absent levels and no duplicate anywhere. D9 is why the symbol leads in
    /// `typing` mode: the epic's own `@` example is unreachable under a
    /// usage-first order.
    #[test]
    fn the_resolution_order_is_the_documents_two_tables() {
        let u = usage(PAGE_KEYBOARD, 0x1F);
        let named = Want::Named("Enter".into());
        let symbol = Want::Symbol("@".into());
        let positional = Want::Usage(u);

        /// One row of the tables: a mode, a frame's three levels, and the order
        /// they must be tried in.
        type Case<'a> = (KeyMode, u32, Option<&'a str>, Option<&'a str>, Vec<Want>);

        let cases: Vec<Case> = vec![
            // typing, with a symbol: the symbol leads, then the position, then
            // the name.
            (
                KeyMode::Typing,
                u,
                Some("Enter"),
                Some("@"),
                vec![symbol.clone(), positional.clone(), named.clone()],
            ),
            (
                KeyMode::Typing,
                0,
                Some("Enter"),
                Some("@"),
                vec![symbol.clone(), named.clone()],
            ),
            (
                KeyMode::Typing,
                u,
                None,
                Some("@"),
                vec![symbol.clone(), positional.clone()],
            ),
            (KeyMode::Typing, 0, None, Some("@"), vec![symbol.clone()]),
            // typing, no symbol: the NAME leads, so a target whose usage table is
            // incomplete still knows what Enter is.
            (
                KeyMode::Typing,
                u,
                Some("Enter"),
                None,
                vec![named.clone(), positional.clone()],
            ),
            (KeyMode::Typing, u, None, None, vec![positional.clone()]),
            (KeyMode::Typing, 0, Some("Enter"), None, vec![named.clone()]),
            (KeyMode::Typing, 0, None, None, vec![]),
            // positional: what a real HID keyboard would have sent.
            (
                KeyMode::Positional,
                u,
                Some("Enter"),
                Some("@"),
                vec![positional.clone(), named.clone(), symbol.clone()],
            ),
            (
                KeyMode::Positional,
                0,
                Some("Enter"),
                Some("@"),
                vec![named.clone(), symbol.clone()],
            ),
            (
                KeyMode::Positional,
                u,
                None,
                Some("@"),
                vec![positional.clone(), symbol.clone()],
            ),
            (
                KeyMode::Positional,
                u,
                Some("Enter"),
                None,
                vec![positional.clone(), named.clone()],
            ),
            (KeyMode::Positional, 0, None, None, vec![]),
            // An empty symbol is an absent symbol: no layout produces nothing.
            (
                KeyMode::Typing,
                u,
                Some("Enter"),
                Some(""),
                vec![named.clone(), positional.clone()],
            ),
        ];

        for (mode, usage, key, sym, expected) in cases {
            let got = levels(mode, usage, key, sym);
            assert_eq!(
                got, expected,
                "{mode:?} usage {usage:#x} key {key:?} sym {sym:?}"
            );
            for (at, level) in got.iter().enumerate() {
                for later in &got[at + 1..] {
                    assert_ne!(level, later, "a level must not be tried twice");
                }
            }
        }
    }

    /// An answer is asked for once per layout, INCLUDING a negative one: a symbol
    /// this layout cannot produce must cost one round trip to the OS thread, not
    /// one per keystroke. And a layout change drops the lot, because every answer
    /// in it was about a keymap that is gone.
    #[tokio::test]
    async fn a_resolution_is_cached_including_a_negative_one_and_dropped_on_a_layout_change() {
        let (fake, _) = FakeBackend::new();
        let mut resolver = Resolver::new();
        resolver.layout_changed("us");
        assert_eq!(resolver.layout(), "us");

        assert!(
            resolver
                .resolve(&fake, Want::Symbol("@".into()))
                .await
                .is_none()
        );
        fake.teach_symbol("@", 50, mods::SHIFT);
        assert!(
            resolver
                .resolve(&fake, Want::Symbol("@".into()))
                .await
                .is_none(),
            "the negative answer was cached, so the backend is not asked again"
        );

        // Re-announcing the same layout keeps the cache: X11 sends a keymap
        // notify for a modifier map change, and dropping a warm cache for that
        // would put a round trip back on the next hundred keystrokes.
        resolver.layout_changed("us");
        assert!(
            resolver
                .resolve(&fake, Want::Symbol("@".into()))
                .await
                .is_none()
        );

        resolver.layout_changed("fr(azerty)");
        assert_eq!(resolver.cached(), 0, "a new keymap invalidates everything");
        let how = resolver
            .resolve(&fake, Want::Symbol("@".into()))
            .await
            .expect("asked afresh");
        assert_eq!(how.code, pk(50));
    }

    /// The cache is bounded, because a peer chooses what a frame's `sym` says and
    /// 32 bytes of UTF-8 is a great many distinct symbols. At the bound it stops
    /// inserting and keeps answering correctly by asking every time.
    #[tokio::test]
    async fn the_resolution_cache_stops_growing_at_its_bound_and_still_answers() {
        let (fake, _) = FakeBackend::new();
        let mut resolver = Resolver::new();
        for i in 0..RESOLVE_CACHE_MAX {
            let sym = format!("{i}");
            fake.teach_symbol(&sym, 1000 + u32::try_from(i).expect("small"), 0);
            assert!(resolver.resolve(&fake, Want::Symbol(sym)).await.is_some());
        }
        assert_eq!(resolver.cached(), RESOLVE_CACHE_MAX);

        fake.teach_symbol("beyond", 1, 0);
        assert!(
            resolver
                .resolve(&fake, Want::Symbol("beyond".into()))
                .await
                .is_some(),
            "past the bound it is slower and never wrong"
        );
        assert_eq!(
            resolver.cached(),
            RESOLVE_CACHE_MAX,
            "the cache stopped growing"
        );
        // An answer taken before the bound stays taken, so the backend changing its
        // mind about it is not seen: that is what a cache IS, and the layout
        // identity is what makes it correct.
        fake.teach_symbol("0", 2, 0);
        let how = resolver
            .resolve(&fake, Want::Symbol("0".into()))
            .await
            .expect("cached");
        assert_eq!(how.code, pk(1000), "the cached answer, not the new one");
    }

    /// A source that sends junk symbols cannot make every real keystroke pay a
    /// round trip for ever.
    ///
    /// The negatives live in their own small map, so a burst of them (512 distinct
    /// unproducible symbols is four seconds of typing, well inside the rate cap)
    /// displaces only other negatives. With one shared map that burst filled the
    /// cache, after which it never inserted again and every subsequent keystroke of
    /// every session on that layout went back to the OS thread.
    #[tokio::test]
    async fn a_burst_of_junk_symbols_cannot_poison_the_answers_that_matter() {
        let (fake, _) = FakeBackend::new();
        let mut resolver = Resolver::new();
        fake.teach_symbol("a", 65, 0);
        assert!(
            resolver
                .resolve(&fake, Want::Symbol("a".into()))
                .await
                .is_some()
        );
        assert_eq!(resolver.cached(), 1);

        for i in 0..RESOLVE_CACHE_MAX * 4 {
            assert!(
                resolver
                    .resolve(&fake, Want::Symbol(format!("junk-{i}")))
                    .await
                    .is_none()
            );
        }
        assert_eq!(
            resolver.cached(),
            1,
            "the real answer is still there, and still the only one"
        );
        assert!(
            resolver.negatives() <= NEGATIVE_CACHE_MAX,
            "the negatives are bounded on their own: {} of them",
            resolver.negatives()
        );

        // And the negatives are still doing their job for a working set: a symbol
        // asked about twice in a row costs one round trip, which is what a source
        // typing into a target that cannot serve it depends on.
        let asked = Want::Symbol("字".into());
        assert!(resolver.resolve(&fake, asked.clone()).await.is_none());
        fake.teach_symbol("字", 1, 0);
        assert!(
            resolver.resolve(&fake, asked).await.is_none(),
            "the negative answer was cached, so the backend is not asked again"
        );

        // A layout change drops both maps: every answer in either was about a
        // keymap that is gone.
        resolver.layout_changed("something-else");
        assert_eq!(resolver.cached(), 0);
        assert_eq!(resolver.negatives(), 0);
    }

    /// The plain case: a symbol needing no modifier is one press and one release,
    /// and it leaves nothing held. A symbol stroke is ATOMIC because the modifier
    /// dance around it must not leak past it.
    #[tokio::test]
    async fn a_plain_symbol_is_one_press_and_one_release_and_holds_nothing() {
        let (fake, mut resolver, keys) = armed().await;
        fake.teach_symbol("a", 65, 0);
        let wants = levels(KeyMode::Typing, usage(PAGE_KEYBOARD, 0x04), None, Some("a"));
        let (at, how) = first_answer(&mut resolver, &fake, &wants)
            .await
            .expect("the symbol resolves");
        let answer = Answer {
            level: &wants[at],
            how: &how,
        };
        let pressed = plan(
            &stroke(KeyMode::Typing, Some(answer), Some("a")),
            &Held::new(),
            &keys,
        )
        .expect("a plan");
        assert_eq!(pressed.actions, vec![down(65), up(65)]);
        assert!(
            pressed.held.is_empty(),
            "a symbol stroke holds nothing after it"
        );

        // And the release frame that follows says nothing, because we are not
        // holding that key: the atomic press already let it go.
        let mut release = stroke(KeyMode::Typing, Some(answer), Some("a"));
        release.down = false;
        let released = plan(&release, &Held::new(), &keys).expect("a plan");
        assert!(
            released.actions.is_empty(),
            "never release what we did not press"
        );
    }

    /// **The epic's own example, end to end.** A source on QWERTY holds Shift and
    /// types `@`; the AZERTY target produces it with AltGr and the 0 key. The
    /// stroke must let go of Shift, take AltGr, strike the key, and put the
    /// machine back exactly as it found it.
    ///
    /// This is the test the whole of section 8 exists for.
    #[tokio::test]
    async fn typing_an_at_sign_from_qwerty_onto_azerty_swaps_shift_for_altgr_and_puts_shift_back() {
        let (fake, mut resolver, keys) = armed().await;
        // On this target, `@` is AltGr plus the key at the 0 position.
        fake.teach_symbol("@", 48, mods::ALTGR);

        // The source's Shift arrives first, as its own key frame: a modifier is a
        // key, and it is the held set that carries it across frames.
        let shift_wants = levels(
            KeyMode::Typing,
            mod_usage(mods::SHIFT).expect("shift"),
            None,
            None,
        );
        assert_eq!(shift_wants, vec![Want::Usage(usage(PAGE_KEYBOARD, 0xE1))]);
        let (at, how) = first_answer(&mut resolver, &fake, &shift_wants)
            .await
            .expect("the target has a Shift key");
        let plan_shift = plan(
            &stroke(
                KeyMode::Typing,
                Some(Answer {
                    level: &shift_wants[at],
                    how: &how,
                }),
                None,
            ),
            &Held::new(),
            &keys,
        )
        .expect("a plan");
        assert_eq!(
            plan_shift.actions,
            vec![down(16)],
            "Shift goes down and stays"
        );
        fake.inject(plan_shift.actions.clone());
        let before = plan_shift.held;
        assert_eq!(
            before.mod_bits(),
            mods::SHIFT,
            "the engine knows it is holding Shift without being told by a frame"
        );

        // Then the `@`, whose frame carries the position of the QWERTY 2 key.
        let at_wants = levels(KeyMode::Typing, usage(PAGE_KEYBOARD, 0x1F), None, Some("@"));
        let (at, how) = first_answer(&mut resolver, &fake, &at_wants)
            .await
            .expect("the symbol resolves");
        assert_eq!(
            at_wants[at],
            Want::Symbol("@".into()),
            "the symbol level is what answers, which is D9's whole point"
        );
        let plan_at = plan(
            &stroke(
                KeyMode::Typing,
                Some(Answer {
                    level: &at_wants[at],
                    how: &how,
                }),
                Some("@"),
            ),
            &before,
            &keys,
        )
        .expect("a plan");
        assert_eq!(
            plan_at.actions,
            vec![
                up(16),    // let go of the Shift WE are holding: AZERTY does not want it
                down(165), // take AltGr
                down(48),  // strike the 0 key
                up(48),
                up(165),  // let AltGr go again
                down(16), // and put Shift back exactly as it was
            ]
        );
        assert_eq!(
            plan_at.held, before,
            "the held set afterwards is exactly what it was before"
        );

        // Proven from the calls rather than from the engine's own bookkeeping,
        // which is what a hygiene assertion must never trust.
        fake.inject(plan_at.actions.clone());
        assert_eq!(
            fake.keys_down(),
            vec![pk(16)],
            "the machine is holding Shift and nothing else"
        );

        // The crash guard names both, because a death mid-stroke could strand
        // either: the peak, not the end state, is what goes on disk first.
        assert_eq!(
            plan_at.peak.to_value(),
            json!({ "keys": [
                { "code": 16, "detail": 0, "mod": mods::SHIFT },
                { "code": 165, "detail": 0, "mod": mods::ALTGR },
            ] })
        );
    }

    /// A layout modifier is taken away for a symbol that does not want it, and a
    /// COMMAND modifier never is: releasing the Control of a Ctrl plus C to
    /// produce a "c" would turn a copy into a letter. This is the recorded
    /// departure from the letter of section 8's step 1.
    #[tokio::test]
    async fn a_command_modifier_survives_a_symbol_stroke_so_ctrl_c_is_still_a_copy() {
        let (fake, mut resolver, keys) = armed().await;
        fake.teach_symbol("c", 67, 0);
        let wants = levels(KeyMode::Typing, usage(PAGE_KEYBOARD, 0x06), None, Some("c"));
        let (at, how) = first_answer(&mut resolver, &fake, &wants)
            .await
            .expect("the symbol resolves");
        let answer = Answer {
            level: &wants[at],
            how: &how,
        };

        let mut ctrl_held = Held::new();
        ctrl_held.press(pk(17), Some(mods::CTRL));
        let plan_c = plan(
            &stroke(KeyMode::Typing, Some(answer), Some("c")),
            &ctrl_held,
            &keys,
        )
        .expect("a plan");
        assert_eq!(
            plan_c.actions,
            vec![down(67), up(67)],
            "Control is not ours to take away: the copy stays a copy"
        );
        assert_eq!(plan_c.held, ctrl_held);

        // The same stroke with SHIFT held does release it, because Shift chooses
        // which symbol a key produces and the resolution said this one needs none.
        let mut shift_held = Held::new();
        shift_held.press(pk(16), Some(mods::SHIFT));
        let plan_c = plan(
            &stroke(KeyMode::Typing, Some(answer), Some("c")),
            &shift_held,
            &keys,
        )
        .expect("a plan");
        assert_eq!(plan_c.actions, vec![up(16), down(67), up(67), down(16)]);
        assert_eq!(plan_c.held, shift_held, "and it is put back");
    }

    /// A dead key is the same machinery with two strokes: the prefix with its own
    /// modifiers, pressed and released, then the base key. The prefix's modifiers
    /// are reconciled by the base key's own dance rather than restored in
    /// between, and the whole stroke still ends where it started.
    #[tokio::test]
    async fn a_dead_key_prefix_is_struck_before_the_base_key_and_restores_afterwards() {
        let (fake, mut resolver, keys) = armed().await;
        fake.teach(
            &Want::Symbol("é".into()),
            Resolved {
                code: pk(69),
                mods: 0,
                prefix: Some(Box::new(Resolved {
                    code: pk(222),
                    mods: mods::SHIFT,
                    prefix: None,
                    group: None,
                })),
                group: None,
            },
        );
        let wants = levels(KeyMode::Typing, usage(PAGE_KEYBOARD, 0x08), None, Some("é"));
        let (at, how) = first_answer(&mut resolver, &fake, &wants)
            .await
            .expect("the symbol resolves");
        let plan_e = plan(
            &stroke(
                KeyMode::Typing,
                Some(Answer {
                    level: &wants[at],
                    how: &how,
                }),
                Some("é"),
            ),
            &Held::new(),
            &keys,
        )
        .expect("a plan");
        assert_eq!(
            plan_e.actions,
            vec![
                down(16),  // the dead key wants Shift
                down(222), // strike the dead key
                up(222),
                up(16),   // the base key wants no Shift
                down(69), // strike the base key
                up(69),
            ]
        );
        assert!(
            plan_e.held.is_empty(),
            "nothing the stroke pressed outlives it"
        );
        // Both modifiers a crash could strand are named, the prefix's included.
        assert_eq!(
            plan_e.peak.modifiers(),
            vec![pk(16)],
            "the dead key's Shift is on disk before it is pressed"
        );
        // And BOTH keys of the stroke are in the peak, the dead key as well as the
        // base key: the peak's contract is everything a crash mid-batch could
        // strand, and the dead key is down between two of these actions. It is
        // filtered out of `to_value` because the crash guard carries modifiers only
        // (D17), which is why leaving it out was harmless rather than right.
        assert!(
            plan_e.peak.holds(pk(222)) && plan_e.peak.holds(pk(69)),
            "the dead key and the base key are treated the same way"
        );
        assert_eq!(
            plan_e.peak.to_value(),
            json!({ "keys": [{ "code": 16, "detail": 0, "mod": mods::SHIFT }] }),
            "and the file still carries the modifiers alone"
        );
    }

    /// A symbol that lives in another keyboard group switches the group and
    /// switches it BACK: a session that silently changes the machine's layout
    /// breaks the next thing its owner types.
    #[tokio::test]
    async fn a_symbol_in_another_group_switches_the_group_and_switches_it_back() {
        let (fake, mut resolver, keys) = armed().await;
        fake.teach(
            &Want::Symbol("Ω".into()),
            Resolved {
                code: pk(87),
                mods: 0,
                prefix: None,
                group: Some(2),
            },
        );
        let wants = levels(KeyMode::Typing, usage(PAGE_KEYBOARD, 0x1D), None, Some("Ω"));
        let (at, how) = first_answer(&mut resolver, &fake, &wants)
            .await
            .expect("the symbol resolves");
        let answer = Answer {
            level: &wants[at],
            how: &how,
        };

        let mut in_group_1 = stroke(KeyMode::Typing, Some(answer), Some("Ω"));
        in_group_1.group = 1;
        let plan_omega = plan(&in_group_1, &Held::new(), &keys).expect("a plan");
        assert_eq!(
            plan_omega.actions,
            vec![Action::Group(2), down(87), up(87), Action::Group(1)],
            "back to the group the machine was in, not to a guess"
        );

        // A resolution in the group the machine is already in switches nothing:
        // the cheapest correct plan.
        let mut in_group_2 = stroke(KeyMode::Typing, Some(answer), Some("Ω"));
        in_group_2.group = 2;
        let plan_omega = plan(&in_group_2, &Held::new(), &keys).expect("a plan");
        assert_eq!(plan_omega.actions, vec![down(87), up(87)]);
    }

    /// A half-duplex lock is one press with no release, and the key is NEVER in
    /// the held set: releasing it later would toggle the machine's lock, which is
    /// the opposite of hygiene.
    #[tokio::test]
    async fn a_half_duplex_lock_is_one_press_that_never_enters_the_held_set() {
        let (fake, mut resolver, keys) = armed().await;
        fake.teach_named("CapsLock", 20);
        let caps = usage_of("CapsLock").expect("in the table");
        assert!(is_lock(caps));
        let wants = levels(KeyMode::Typing, caps, Some("CapsLock"), None);
        let (at, how) = first_answer(&mut resolver, &fake, &wants)
            .await
            .expect("the name resolves");
        let answer = Answer {
            level: &wants[at],
            how: &how,
        };

        let mut lock = stroke(KeyMode::Typing, Some(answer), None);
        lock.lock = true;
        let plan_caps = plan(&lock, &Held::new(), &keys).expect("a plan");
        assert_eq!(plan_caps.actions, vec![down(20)], "a press, and no release");
        assert!(
            plan_caps.held.is_empty() && plan_caps.peak.is_empty(),
            "a lock is never held, so no teardown can toggle it"
        );

        // A source sending a release for a lock is a source that did not read the
        // dialect: nothing to do, nothing to complain about.
        lock.down = false;
        let plan_caps = plan(&lock, &Held::new(), &keys).expect("a plan");
        assert!(plan_caps.actions.is_empty());

        // A lock this machine cannot name is genuinely unresolved: no string
        // injection can toggle one.
        let mut unknown = stroke(KeyMode::Typing, None, Some("x"));
        unknown.lock = true;
        assert!(plan(&unknown, &Held::new(), &keys).is_none());
    }

    /// A lock key with NO `lk` flag is still a lock, because section 8's rule is
    /// flat and it is a rule the TARGET owns.
    ///
    /// A third-party engine holding the same exclusive role, or our own capture path
    /// on a backend that reports Caps Lock as a full press and release pair, sends a
    /// `k` with the Caps Lock usage and no `lk`. On the flag alone that took the
    /// positional path, entered the held set, and the teardown's release-all then
    /// sent a Caps Lock RELEASE, which toggles the lock and leaves the machine in
    /// caps: the exact outcome "a lock is never in the held set" forbids, reached by
    /// trusting the sender's manners.
    #[tokio::test]
    async fn a_lock_key_arriving_without_the_flag_is_still_never_held() {
        let (fake, mut resolver, keys) = armed().await;
        for name in ["CapsLock", "NumLock", "ScrollLock"] {
            fake.teach_named(name, 20);
            let usage = usage_of(name).expect("in the table");
            fake.teach_usage(usage, 20);

            // Positional mode, where the usage leads, and no `lk` anywhere.
            let wants = levels(KeyMode::Positional, usage, Some(name), None);
            let (at, how) = first_answer(&mut resolver, &fake, &wants)
                .await
                .expect("the usage resolves");
            let answer = Answer {
                level: &wants[at],
                how: &how,
            };
            let mut frame = stroke(KeyMode::Positional, Some(answer), None);
            assert!(!frame.lock, "the source said nothing about a lock");

            let pressed = plan(&frame, &Held::new(), &keys).expect("a plan");
            assert_eq!(
                pressed.actions,
                vec![down(20)],
                "{name} is one press, as the dialect's own lock frame would be"
            );
            assert!(
                pressed.held.is_empty() && pressed.peak.is_empty(),
                "{name} must not enter the held set: a teardown release would toggle it"
            );

            // And the release that follows a full press-and-release pair is silence,
            // because we are not holding it and never were.
            frame.down = false;
            let released = plan(&frame, &pressed.held, &keys).expect("a plan");
            assert!(
                released.actions.is_empty(),
                "{name}'s release must not reach the machine"
            );
        }

        // The named level answers to the same rule, since a target whose usage table
        // is incomplete resolves a lock by its name.
        let wants = vec![Want::Named("CapsLock".into())];
        let (at, how) = first_answer(&mut resolver, &fake, &wants)
            .await
            .expect("the name resolves");
        let by_name = stroke(
            KeyMode::Typing,
            Some(Answer {
                level: &wants[at],
                how: &how,
            }),
            None,
        );
        let pressed = plan(&by_name, &Held::new(), &keys).expect("a plan");
        assert_eq!(pressed.actions, vec![down(20)]);
        assert!(pressed.held.is_empty());
    }

    /// A held set far larger than a keyboard stops accepting presses, and still
    /// releases everything.
    ///
    /// The reasoning that says the set cannot grow past a keymap's worth of keys
    /// (a press requires a successful resolve) holds only as long as a backend
    /// refuses to resolve what it has no key for, which is not enforceable on this
    /// side of the seam. So the belt: no action has been emitted when the plan gives
    /// up, so nothing is stranded, and the caller degrades to Unicode or reports
    /// `UNRESOLVED`. The release path is never refused, or the keys could never be
    /// let go of at all.
    #[tokio::test]
    async fn a_held_set_far_larger_than_a_keyboard_refuses_a_press_and_still_releases() {
        let (fake, mut resolver, keys) = armed().await;
        fake.teach_symbol("a", 65, 0);
        let w = usage(PAGE_KEYBOARD, 0x1A);
        fake.teach_usage(w, 87);

        let mut crowded = Held::new();
        for code in 0..u32::try_from(HELD_PRESS_MAX).expect("small") {
            crowded.press(pk(1000 + code), None);
        }
        assert_eq!(crowded.len(), HELD_PRESS_MAX);

        // A symbol press: refused, and the Unicode fallback carries the character,
        // which is exactly the degradation a machine holding 256 keys deserves.
        let wants = levels(KeyMode::Typing, 0, None, Some("a"));
        let (at, how) = first_answer(&mut resolver, &fake, &wants)
            .await
            .expect("the symbol resolves");
        let pressed = plan(
            &stroke(
                KeyMode::Typing,
                Some(Answer {
                    level: &wants[at],
                    how: &how,
                }),
                Some("a"),
            ),
            &crowded,
            &keys,
        )
        .expect("a plan");
        assert_eq!(pressed.actions, vec![Action::Text("a".into())]);
        assert_eq!(pressed.held, crowded, "and nothing was added");

        // A positional press with no fallback: nothing at all, and the caller says
        // `UNRESOLVED`.
        let wants = levels(KeyMode::Positional, w, None, None);
        let (at, how) = first_answer(&mut resolver, &fake, &wants)
            .await
            .expect("the usage resolves");
        let answer = Answer {
            level: &wants[at],
            how: &how,
        };
        let mut frame = stroke(KeyMode::Positional, Some(answer), None);
        assert!(plan(&frame, &crowded, &keys).is_none());

        // But a release of something we ARE holding is never refused, whatever the
        // size of the set: that is the one path that must always work.
        crowded.press(pk(87), None);
        frame.down = false;
        let released = plan(&frame, &crowded, &keys).expect("a release is never refused");
        assert_eq!(released.actions, vec![up(87)]);
        assert!(!released.held.holds(pk(87)));
    }

    /// The game case: a positional key follows the frame's own `dn`, so W stays
    /// down across frames and the character keeps walking. Its release frame is
    /// what lets it go, and a release of a key we never pressed says nothing.
    #[tokio::test]
    async fn a_positional_key_stays_down_across_frames_and_its_own_frame_releases_it() {
        let (fake, mut resolver, keys) = armed().await;
        let w = usage(PAGE_KEYBOARD, 0x1A);
        fake.teach_usage(w, 87);
        fake.teach_symbol("w", 87, 0);
        let wants = levels(KeyMode::Positional, w, None, Some("w"));
        let (at, how) = first_answer(&mut resolver, &fake, &wants)
            .await
            .expect("the usage resolves");
        assert_eq!(
            wants[at],
            Want::Usage(w),
            "the usage leads in positional mode"
        );
        let answer = Answer {
            level: &wants[at],
            how: &how,
        };

        let mut frame = stroke(KeyMode::Positional, Some(answer), Some("w"));
        let pressed = plan(&frame, &Held::new(), &keys).expect("a plan");
        assert_eq!(
            pressed.actions,
            vec![down(87)],
            "no release: the engine must not take the finger off the key"
        );
        assert!(pressed.held.holds(pk(87)));
        assert_eq!(pressed.held.mod_bits(), 0, "W is not a modifier");

        frame.down = false;
        let released = plan(&frame, &pressed.held, &keys).expect("a plan");
        assert_eq!(released.actions, vec![up(87)]);
        assert!(released.held.is_empty());

        let stranger = plan(&frame, &Held::new(), &keys).expect("a plan");
        assert!(
            stranger.actions.is_empty(),
            "a key the machine's own user is holding is none of our business"
        );

        // The other half of the rule: the SAME key resolved from a symbol is
        // atomic, because the modifier dance around a symbol must not leak.
        let typed = levels(KeyMode::Typing, w, None, Some("w"));
        let (at, how) = first_answer(&mut resolver, &fake, &typed)
            .await
            .expect("the symbol resolves");
        assert_eq!(typed[at], Want::Symbol("w".into()));
        let plan_w = plan(
            &stroke(
                KeyMode::Typing,
                Some(Answer {
                    level: &typed[at],
                    how: &how,
                }),
                Some("w"),
            ),
            &Held::new(),
            &keys,
        )
        .expect("a plan");
        assert_eq!(plan_w.actions, vec![down(87), up(87)]);
        assert!(plan_w.held.is_empty());
    }

    /// A modifier arriving positionally is held WITH its bit, which is the whole
    /// reason [`Held::mod_bits`] can be derived from our own presses instead of
    /// from a frame's `m`.
    #[tokio::test]
    async fn a_modifier_pressed_positionally_is_held_with_the_bit_it_means() {
        let (fake, mut resolver, keys) = armed().await;
        for (bit, code) in [
            (mods::SHIFT, 16),
            (mods::CTRL, 17),
            (mods::ALTGR, 165),
            (mods::META, 91),
        ] {
            let wants = levels(KeyMode::Typing, mod_usage(bit).expect("a key"), None, None);
            let (at, how) = first_answer(&mut resolver, &fake, &wants)
                .await
                .expect("the modifier resolves");
            let plan_mod = plan(
                &stroke(
                    KeyMode::Typing,
                    Some(Answer {
                        level: &wants[at],
                        how: &how,
                    }),
                    None,
                ),
                &Held::new(),
                &keys,
            )
            .expect("a plan");
            assert_eq!(plan_mod.actions, vec![down(code)]);
            assert_eq!(
                plan_mod.held.mod_bits(),
                bit,
                "{:?} must be recognised for what it is",
                mods::name(bit)
            );
            assert_eq!(
                plan_mod.peak.modifiers(),
                vec![pk(code)],
                "on disk before it is pressed"
            );
        }
    }

    /// Nothing resolves, and the target can inject Unicode: one string injection
    /// on the press, nothing held, and nothing at all on the release.
    #[tokio::test]
    async fn nothing_resolves_so_the_symbol_is_injected_as_text_on_the_press_alone() {
        let (_fake, _resolver, keys) = armed().await;
        let mut frame = stroke(KeyMode::Typing, None, Some("字"));
        let plan_text = plan(&frame, &Held::new(), &keys).expect("a plan");
        assert_eq!(plan_text.actions, vec![Action::Text("字".into())]);
        assert!(
            plan_text.held.is_empty() && plan_text.peak.is_empty(),
            "a text injection presses nothing, so the teardown has nothing to undo"
        );

        frame.down = false;
        let plan_text = plan(&frame, &Held::new(), &keys).expect("a plan");
        assert!(
            plan_text.actions.is_empty(),
            "a release of a text injection is meaningless"
        );
    }

    /// Nothing resolves and the target cannot inject Unicode: `UNRESOLVED`, which
    /// the caller says out loud ("That key does not exist on the other computer's
    /// keyboard") rather than swallowing.
    #[tokio::test]
    async fn nothing_resolves_and_no_unicode_is_unresolved() {
        let (_fake, _resolver, keys) = armed().await;
        let mut frame = stroke(KeyMode::Typing, None, Some("字"));
        frame.unicode = false;
        assert!(plan(&frame, &Held::new(), &keys).is_none());

        // And a key with no symbol at all cannot fall back either, whatever the
        // target can do.
        let bare = stroke(KeyMode::Typing, None, None);
        assert!(plan(&bare, &Held::new(), &keys).is_none());
        let empty = stroke(KeyMode::Typing, None, Some(""));
        assert!(plan(&empty, &Held::new(), &keys).is_none());
    }

    /// Section 8's typing table puts direct Unicode injection SECOND, ahead of
    /// the positional levels, and this is why it matters: pressing the position
    /// of a QWERTY `@` on a target that cannot produce `@` types the wrong
    /// character, where injecting the text types the right one.
    #[tokio::test]
    async fn typing_mode_injects_text_rather_than_a_position_when_the_symbol_cannot_be_produced() {
        let (fake, mut resolver, keys) = armed().await;
        // The position resolves, the symbol does not.
        let two = usage(PAGE_KEYBOARD, 0x1F);
        fake.teach_usage(two, 50);
        let wants = levels(KeyMode::Typing, two, None, Some("@"));
        let (at, how) = first_answer(&mut resolver, &fake, &wants)
            .await
            .expect("the position resolves");
        assert_eq!(wants[at], Want::Usage(two), "the symbol level failed first");
        let answer = Answer {
            level: &wants[at],
            how: &how,
        };

        let mut frame = stroke(KeyMode::Typing, Some(answer), Some("@"));
        let plan_at = plan(&frame, &Held::new(), &keys).expect("a plan");
        assert_eq!(plan_at.actions, vec![Action::Text("@".into())]);

        // With a target that cannot inject a string, the position is better than
        // nothing and is what the table's next line says.
        frame.unicode = false;
        let plan_at = plan(&frame, &Held::new(), &keys).expect("a plan");
        assert_eq!(plan_at.actions, vec![down(50)]);

        // In `positional` mode the usage LEADS, so the same answer is used as
        // given and no fallback is considered.
        let wants = levels(KeyMode::Positional, two, None, Some("@"));
        let (at, how) = first_answer(&mut resolver, &fake, &wants)
            .await
            .expect("the position resolves");
        let plan_at = plan(
            &stroke(
                KeyMode::Positional,
                Some(Answer {
                    level: &wants[at],
                    how: &how,
                }),
                Some("@"),
            ),
            &Held::new(),
            &keys,
        )
        .expect("a plan");
        assert_eq!(plan_at.actions, vec![down(50)]);
    }

    /// A resolution needing a modifier this machine has no key for is abandoned
    /// WHOLE. Half a modifier set types a different character, and nothing has
    /// been handed to the OS at the point the plan gives up.
    #[tokio::test]
    async fn a_stroke_needing_a_modifier_this_machine_cannot_press_is_abandoned_whole() {
        let (fake, mut resolver, _keys) = armed().await;
        fake.teach_symbol("@", 48, mods::ALTGR);
        let wants = levels(KeyMode::Typing, usage(PAGE_KEYBOARD, 0x1F), None, Some("@"));
        let (at, how) = first_answer(&mut resolver, &fake, &wants)
            .await
            .expect("the symbol resolves");
        let answer = Answer {
            level: &wants[at],
            how: &how,
        };
        // A machine whose AltGr key could not be resolved.
        let mut keys = ModKeys::new();
        keys.set(mods::SHIFT, pk(16));
        assert_eq!(keys.get(mods::ALTGR), None);

        let mut frame = stroke(KeyMode::Typing, Some(answer), Some("@"));
        let plan_at = plan(&frame, &Held::new(), &keys).expect("a plan");
        assert_eq!(
            plan_at.actions,
            vec![Action::Text("@".into())],
            "the last resort, rather than a stroke with half its modifiers"
        );
        frame.unicode = false;
        assert!(plan(&frame, &Held::new(), &keys).is_none());
    }

    /// The remapping is a SIMULTANEOUS permutation. Mapping ctrl to meta and meta
    /// to ctrl must swap the two; applying the entries one after the other would
    /// collapse both onto one bit and lose a modifier.
    #[test]
    fn the_remapping_swaps_two_modifiers_rather_than_collapsing_them() {
        let swap = BTreeMap::from([
            ("ctrl".to_string(), "meta".to_string()),
            ("meta".to_string(), "ctrl".to_string()),
        ]);
        assert_eq!(remap(mods::CTRL, &swap), mods::META);
        assert_eq!(remap(mods::META, &swap), mods::CTRL);
        assert_eq!(
            remap(mods::CTRL | mods::META, &swap),
            mods::CTRL | mods::META,
            "a swap, not a collapse: both modifiers survive"
        );
        assert_eq!(
            remap(mods::CTRL | mods::SHIFT, &swap),
            mods::META | mods::SHIFT,
            "what the table does not name it does not touch"
        );

        // The default is the identity, because guessing that a Mac target wants
        // Control and Command swapped is an opinion.
        let identity = BTreeMap::new();
        assert_eq!(remap(0b1111_1111, &identity), 0b1111_1111);

        // An unknown name on either side is ignored, so a table from a later
        // version degrades to the identity rather than to a wrong modifier.
        let unknown = BTreeMap::from([
            ("command".to_string(), "ctrl".to_string()),
            ("ctrl".to_string(), "hyper".to_string()),
        ]);
        assert_eq!(remap(mods::CTRL, &unknown), mods::CTRL);

        // Lock bits pass through, and a table that tries to map a modifier ONTO a
        // lock is ignored: no keyboard holds Caps Lock the way it holds Shift.
        let onto_lock = BTreeMap::from([("ctrl".to_string(), "caps".to_string())]);
        assert_eq!(
            remap(mods::CTRL | mods::CAPS, &onto_lock),
            mods::CTRL | mods::CAPS
        );
        assert_eq!(remap(mods::CTRL | mods::NUM, &swap), mods::META | mods::NUM);
    }

    /// The return hotkey fires on a press, only with exactly its modifiers, and
    /// its stored shape survives a round trip. An extra modifier must NOT fire it:
    /// sending the keyboard home in the middle of someone's shortcut is worse
    /// than a chord that has to be repeated.
    #[test]
    fn the_return_hotkey_fires_on_a_press_with_exactly_its_modifiers() {
        let hotkey = Hotkey::default_hotkey();
        assert_eq!(hotkey.mods, mods::CTRL | mods::ALT);
        assert_eq!(hotkey.usage, usage_of("Escape").expect("in the table"));

        let event = |mods: u16, down: bool| KeyEvent {
            usage: usage_of("Escape").expect("in the table"),
            key: Some("Escape".into()),
            sym: None,
            mods,
            down,
            lock: false,
        };
        assert!(hotkey.fires(&event(mods::CTRL | mods::ALT, true)));
        assert!(
            !hotkey.fires(&event(mods::CTRL | mods::ALT, false)),
            "on the press only, or the release would act on the far side"
        );
        assert!(
            !hotkey.fires(&event(mods::CTRL | mods::ALT | mods::SHIFT, true)),
            "an accidental extra modifier must not send the keyboard home"
        );
        assert!(!hotkey.fires(&event(mods::CTRL, true)));
        assert!(!hotkey.fires(&event(0, true)));
        assert!(
            hotkey.fires(&event(mods::CTRL | mods::ALT | mods::CAPS, true)),
            "Caps Lock being on is not part of any chord"
        );

        let mut other_key = event(mods::CTRL | mods::ALT, true);
        other_key.usage = usage_of("Enter").expect("in the table");
        other_key.key = Some("Enter".into());
        assert!(!hotkey.fires(&other_key));

        // A backend that cannot tell the usage still names the key, and the way
        // out is the one chord that must work on a backend having a bad day.
        let mut nameless = event(mods::CTRL | mods::ALT, true);
        nameless.usage = 0;
        assert!(hotkey.fires(&nameless));
        nameless.key = None;
        assert!(!hotkey.fires(&nameless));
        // A hotkey with no usage fires on nothing, rather than on everything a
        // backend could not name.
        let broken = Hotkey {
            mods: mods::CTRL,
            usage: 0,
        };
        let mut anything = event(mods::CTRL, true);
        anything.usage = 0;
        anything.key = None;
        assert!(!broken.fires(&anything));
    }

    /// A stored hotkey round trips through its names, and an unknown name is
    /// REFUSED rather than silently becoming a wrong key: arming a chord the owner
    /// never chose, on a keyboard they cannot bring home, is the failure to avoid.
    #[test]
    fn a_hotkey_round_trips_through_its_names_and_an_unknown_one_is_refused() {
        let names = |list: &[&str]| -> Vec<String> {
            list.iter().map(|name| (*name).to_string()).collect()
        };
        assert_eq!(
            Hotkey::default_hotkey().to_names(),
            names(&["ctrl", "alt", "Escape"])
        );
        // Written in PRESENTATION order, which is the order they come back in:
        // Control, the alternate keys, Shift, then the platform key, because that
        // is how all three desktops write a chord. Nobody writes
        // "Shift+Ctrl+Home", and a first version of `to_names` did, because it
        // walked the bits and Shift is bit 0.
        for shape in [
            names(&["ctrl", "alt", "Escape"]),
            names(&["shift", "meta", "F12"]),
            names(&["altgr", "Pause"]),
            names(&["ctrl", "alt", "altgr", "shift", "meta", "Home"]),
        ] {
            let parsed = Hotkey::parse(&shape).expect("a legal chord");
            assert_eq!(parsed.to_names(), shape, "the shape survives a round trip");
            assert_eq!(Hotkey::parse(&parsed.to_names()), Some(parsed));
        }
        // And parsing does not care what order it is GIVEN: a chord is a set.
        assert_eq!(
            Hotkey::parse(&names(&["shift", "ctrl", "Home"]))
                .expect("a legal chord")
                .to_names(),
            names(&["ctrl", "shift", "Home"]),
            "however a caller spells a chord, one binding has one spelling"
        );

        for bad in [
            // A name this build does not know, on either side of the chord.
            names(&["ctrl", "alt", "Compose"]),
            names(&["hyper", "Escape"]),
            // Two keys, or none.
            names(&["ctrl", "Escape", "Enter"]),
            names(&["ctrl", "alt"]),
            // A bare key would swallow every Escape a session ever sees.
            names(&["Escape"]),
            // A lock is a state, not something a hand holds.
            names(&["caps", "Escape"]),
            // Nothing at all, and more entries than the vocabulary has.
            names(&[]),
            names(&["ctrl", "ctrl", "ctrl", "ctrl", "ctrl", "ctrl", "Escape"]),
        ] {
            assert_eq!(Hotkey::parse(&bad), None, "{bad:?} must be refused");
        }
    }
}
