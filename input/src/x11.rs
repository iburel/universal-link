// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The X11 keyboard and mouse backend: XTEST for injection, XInput2 raw events for
//! the relative source, an XI2 device grab for swallowing, XKB for the keymap and the
//! active group, RandR for the monitors, XFixes to hide the pointer while this
//! machine is driving another.
//!
//! The shape is `clipboard/src/x11.rs`'s and the reason is the same: everything that
//! touches the connection happens on ONE thread, the main one, pumped by a
//! `mio::Poll` over the X socket fd and a `Waker`-backed command queue, while the
//! async engine drives it through the cheap, `Clone` [`X11Backend`] handle and
//! observes it through a bounded channel of upcalls.
//!
//! Unlike the Windows backend, which runs most downcalls inline because no Win32 call
//! in it has any thread affinity, this one marshals ALL of them, answering ones
//! included. `xcb::Connection` is `Send + Sync` and libxcb is thread safe, so the
//! alternative was available and was rejected on purpose: a `wait_for_reply` on the
//! engine's thread reads the socket, which pulls X events libxcb then holds in its own
//! queue, and the loop thread's `poll` on the raw fd does not fire for data that has
//! already been read. Every motion event queued behind such a read would wait for the
//! loop's timeout cap. One thread owning the connection has no such state to get
//! wrong, and the price is a `Waker` write per downcall.
//!
//! # Nothing here is mandatory, and every absence is a sentence
//!
//! All five extensions are OPTIONAL at connect time and each one's absence narrows
//! [`Capabilities`] instead of failing the build. A server with XTEST and no XInput2
//! is a machine that can be TYPED ON but cannot drive anything, which is a real and
//! useful state the interface can describe; one with XInput2 and no XTEST is the
//! opposite. Only the connection itself is required, and its absence is the
//! `Unsupported` that becomes [`crate::os::Absent`].
//!
//! # Eight X11 truths this backend is built on
//!
//! 1. **Raw XI2 events are the only observation channel that works.** A core event
//!    mask on the root window is subject to propagation, so an application window that
//!    selected `PointerMotion` or has the keyboard focus takes the event and the root
//!    never sees it. `XI_RawMotion` and friends are delivered to every client that
//!    selected them ON THE ROOT and are not propagated or intercepted, which is why
//!    #123 could measure XTEST to raw capture at 81 microseconds with nothing missed.
//! 2. **The delta comes from a different place in each mode, and the platform decides
//!    which.** While this backend only WATCHES, it reads the raw XInput2 valuators,
//!    which are UNACCELERATED device deltas. While it SWALLOWS, it cannot read them at
//!    all (see truth 8), so it differences the position the grab reports, which is the
//!    ACCELERATED movement, the same thing the Windows backend gets for free from its
//!    hook. That is truth 4 of the seam, which asked #125 to choose and to name its
//!    choice, and on this platform the choice turned out to be made by what a grab will
//!    and will not deliver: the pointer feels the way this machine's own pointer feels
//!    while driving. The cost is stated where it is paid (`on_core_motion`): a
//!    difference against the pin's anchor carries at most half a screen of movement per
//!    event before the server's own clamp eats the rest, which at 125 events a second is
//!    a hand nobody has. Reading the device's acceleration profile out of its XI2
//!    properties and applying it to the raw valuators is a third option and a ticket of
//!    its own rather than a line of this one.
//! 3. **A grab redirects events, it does not stop the pointer moving.** So swallowing
//!    and pinning are two separate mechanisms: the core `GrabKeyboard` and `GrabPointer`
//!    with `owner_events: false` are what keep the source's own input from acting
//!    locally, and a `WarpPointer` back to an anchor on every motion is what keeps the
//!    pointer on this desktop. A warp generates no raw event (no device moved) and the
//!    core motion it does generate is measured against the anchor it warped TO, so the
//!    two do not feed each other.
//! 4. **The core keyboard mapping cannot be read unambiguously, and XKB can.** The
//!    core protocol says the first four keysyms of a keycode are "group 1 levels 1 and
//!    2, group 2 levels 1 and 2", and an XKB server synthesises that list as
//!    `width * groups` entries in group-major order. One group of four levels and two
//!    groups of two levels are therefore the SAME four bytes with different meanings,
//!    and telling them apart matters for the one example the epic leads with (typing
//!    `@` on an AZERTY target is AltGr plus 0, which is group 1 level 3). `xkb::GetMap`
//!    answers with the per-key group count and width, so this backend asks XKB and
//!    falls back to the ambiguous core reading only when the extension is missing.
//! 5. **X11 has no Unicode injection**, so the last resort of key resolution is a
//!    keycode nothing is bound to, remapped for one stroke and restored. It is what
//!    every tool in this category does, it needs a round trip between the remap and the
//!    stroke (the server translates with the keymap it has, not the one in flight), and
//!    it changes the keymap for every client for those few microseconds. The spare
//!    keycode is EXCLUDED from the keymap fingerprint, so this backend's own churn
//!    never looks like the layout change it would otherwise report (which would have
//!    the engine release everything it holds in the middle of typing a word).
//! 6. **X11 has no per-monitor scale.** RandR reports pixels and millimetres and
//!    nothing about what the desktop environment is scaling by, so `scale` is reported
//!    as 1000 on every monitor here. The plane's arithmetic uses the sizes and not the
//!    scale, so this costs an interface a label rather than costing the pointer its
//!    place.
//! 7. **The X keycode of a HID usage is the Linux evdev code plus 8.** That is a
//!    convention rather than a protocol rule, and it is the convention every X server
//!    with the evdev or libinput driver has used for two decades, XWayland included.
//!    It is what makes positional mode work at all, and the live suite checks it
//!    against the server's own keymap rather than trusting it: every usage in the table
//!    is resolved and the keysym the server reports for the resulting keycode is
//!    compared with the one the usage means.
//! 8. **A grab does not stop ANOTHER client's raw events, and it does not hide an
//!    injection either.** Measured with a second client on the same server: a client
//!    that has selected `XI_RawKeyPress` on the root keeps receiving keys while another
//!    client holds a keyboard grab, which is the opposite of what the obvious reading of
//!    the XInput2 specification suggests. That is why the swallow is proved against a
//!    FOCUSED WINDOW (which does stop receiving keys) rather than against a raw
//!    watcher, and it is also why this backend hears its own XTEST injections come
//!    back: X11 offers no `dwExtraInfo` to mark them with. In v1 nothing needs it to
//!    (a machine being driven is not capturing, D15), and there IS a mechanism when
//!    something does: a raw event's `source` device is the XTEST virtual device, which
//!    a real keyboard's never is.
//!
//!    The GRABBING client is the exception, and it is the sharpest thing in this file:
//!    while a device is grabbed the server hands its events to the grab rather than to
//!    the ordinary selections, and a raw event has no core form to convert into, so
//!    while this backend holds its grabs it receives NO raw events at all. Measured, not
//!    reasoned: a Watch to Swallow transition followed by six faked moves and twelve
//!    faked keys produced zero upcalls. That is why the grabs carry an event mask and why
//!    `on_event` reads the core stream while grabbed and the raw stream while watching
//!    ([`Backend::grab`] has the whole measurement), and it is why two live tests now
//!    prove the grab is installed before they look for anything.

use std::collections::VecDeque;
use std::future::Future;
use std::os::unix::io::AsRawFd;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token, Waker};
use tokio::sync::{mpsc, oneshot};
use xcb::{Xid, randr, x, xfixes, xinput, xkb, xtest};

use crate::backend::{
    Action, BackendEvent, Capabilities, CaptureLoss, CaptureMode, InputBackend, KeyEvent, Monitor,
    Motion, PlatformKey, Point, Problem, Rect, Resolved, Want,
};
use crate::keys;
use crate::os::Unsupported;

// --------------------------------------------------------------- constants

const XCB_TOKEN: Token = Token(0);
const CMD_TOKEN: Token = Token(1);

/// Cap of the loop's poll, so `shutdown` and a lost `Waker::wake` are always
/// observed. Shorter than the clipboard's 250 ms, and for a reason: libxcb can hold
/// events in its own queue after any read of the socket, and this loop's queue carries
/// pointer motion. A tenth of a second of stale positions would be visible where a
/// tenth of a second of stale clipboard would not.
const POLL_CAP: Duration = Duration::from_millis(100);

/// Bounded capacity of the upcall channel. Generous: the engine drains it as its own
/// loop turns, and a full queue means it has stalled or gone.
const BACKEND_EVENT_CAPACITY: usize = 256;

/// The XTEST event types, which are the core protocol's own event numbers.
const FAKE_KEY_PRESS: u8 = 2;
const FAKE_KEY_RELEASE: u8 = 3;
const FAKE_BUTTON_PRESS: u8 = 4;
const FAKE_BUTTON_RELEASE: u8 = 5;
const FAKE_MOTION: u8 = 6;

/// `XTestFakeInput`'s device id for "the default XTEST device".
const FAKE_DEVICE: u8 = 0;

/// One wheel notch, as X11 counts them: a button press and release.
const WHEEL_PIXELS_PER_NOTCH: i32 = 120;

/// One line to the standard error, and never a panic.
///
/// `eprintln!` PANICS when the write fails, and a supervised component's stderr can be closed
/// or full. Every caller here is on the loop's own thread, so a panic would unwind into
/// `Drop for Backend` and be survivable, but a warning is not worth a stop either way.
fn warn(message: &str) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "[1device-input] {message}");
}

/// Which of the four control upcalls this is, so at most one of each kind is deferred.
///
/// A number rather than `std::mem::discriminant`, because `LayoutChanged` carries fields and
/// two of them with different fields are still the same FACT: the newest one is the truth.
fn control_kind(event: &BackendEvent) -> u8 {
    match event {
        BackendEvent::LayoutChanged { .. } => 0,
        BackendEvent::CapabilitiesChanged => 1,
        BackendEvent::MonitorsChanged => 2,
        BackendEvent::CaptureLost(_) => 3,
        _ => 4,
    }
}

// ------------------------------------------------------------- the key table

/// HID usage to Linux evdev keycode. The X keycode is this plus 8 (truth 7).
///
/// Read in both directions, so it is one list rather than two: a pair that disagreed
/// would send a key the far side could not press back. The values were taken from
/// `linux/input-event-codes.h` by name (`KEY_A`, `KEY_ENTER`), not from memory, and
/// the live suite checks every entry against the server's own keymap.
#[rustfmt::skip]
const USAGE_EVDEV: &[(u32, u8)] = &[
    // Letters, in HID order (a to z), which is not evdev order.
    (0x04, 30), (0x05, 48), (0x06, 46), (0x07, 32), (0x08, 18), (0x09, 33),
    (0x0A, 34), (0x0B, 35), (0x0C, 23), (0x0D, 36), (0x0E, 37), (0x0F, 38),
    (0x10, 50), (0x11, 49), (0x12, 24), (0x13, 25), (0x14, 16), (0x15, 19),
    (0x16, 31), (0x17, 20), (0x18, 22), (0x19, 47), (0x1A, 17), (0x1B, 45),
    (0x1C, 21), (0x1D, 44),
    // Digits 1 to 9 then 0.
    (0x1E, 2), (0x1F, 3), (0x20, 4), (0x21, 5), (0x22, 6), (0x23, 7),
    (0x24, 8), (0x25, 9), (0x26, 10), (0x27, 11),
    (0x28, 28),  // Enter
    (0x29, 1),   // Escape
    (0x2A, 14),  // Backspace
    (0x2B, 15),  // Tab
    (0x2C, 57),  // Space
    (0x2D, 12),  // minus
    (0x2E, 13),  // equal
    (0x2F, 26),  // bracket left
    (0x30, 27),  // bracket right
    (0x31, 43),  // backslash
    (0x32, 43),  // the non-US hash, the same key
    (0x33, 39),  // semicolon
    (0x34, 40),  // apostrophe
    (0x35, 41),  // grave
    (0x36, 51),  // comma
    (0x37, 52),  // period
    (0x38, 53),  // slash
    (0x39, 58),  // Caps Lock
    (0x3A, 59), (0x3B, 60), (0x3C, 61), (0x3D, 62), (0x3E, 63), (0x3F, 64),
    (0x40, 65), (0x41, 66), (0x42, 67), (0x43, 68), // F1 to F10
    (0x44, 87), (0x45, 88),                          // F11, F12
    (0x46, 99),  // Print Screen
    (0x47, 70),  // Scroll Lock
    (0x48, 119), // Pause
    (0x49, 110), // Insert
    (0x4A, 102), // Home
    (0x4B, 104), // Page Up
    (0x4C, 111), // Delete
    (0x4D, 107), // End
    (0x4E, 109), // Page Down
    (0x4F, 106), // Right
    (0x50, 105), // Left
    (0x51, 108), // Down
    (0x52, 103), // Up
    (0x53, 69),  // Num Lock
    (0x54, 98),  // keypad slash
    (0x55, 55),  // keypad asterisk
    (0x56, 74),  // keypad minus
    (0x57, 78),  // keypad plus
    (0x58, 96),  // keypad Enter
    (0x59, 79), (0x5A, 80), (0x5B, 81), (0x5C, 75), (0x5D, 76), (0x5E, 77),
    (0x5F, 71), (0x60, 72), (0x61, 73), // keypad 1 to 9
    (0x62, 82),  // keypad 0
    (0x63, 83),  // keypad period
    (0x64, 86),  // the 102nd key
    (0x65, 127), // Menu (compose)
    (0x66, 116), // Power
    (0x67, 117), // keypad equal
    (0x68, 183), (0x69, 184), (0x6A, 185), (0x6B, 186), (0x6C, 187), (0x6D, 188),
    (0x6E, 189), (0x6F, 190), (0x70, 191), (0x71, 192), (0x72, 193), (0x73, 194),
    // F13 to F24
    (0x74, 134), // Execute (open)
    (0x75, 138), // Help
    (0x76, 130), // Menu (props)
    (0x77, 132), // Select (front)
    (0x78, 128), // Stop
    (0x79, 129), // Again
    (0x7A, 131), // Undo
    (0x7B, 137), // Cut
    (0x7C, 133), // Copy
    (0x7D, 135), // Paste
    (0x7E, 136), // Find
    (0x7F, 113), // Mute
    (0x80, 115), // Volume Up
    (0x81, 114), // Volume Down
    (0x85, 121), // keypad comma
    (0x87, 89),  // international 1 (Ro)
    (0x88, 93),  // international 2 (Katakana/Hiragana)
    (0x89, 124), // international 3 (Yen)
    (0x8A, 92),  // international 4 (Henkan)
    (0x8B, 94),  // international 5 (Muhenkan)
    (0x8C, 95),  // international 6 (keypad JP comma)
    (0x90, 122), // language 1 (Hangul)
    (0x91, 123), // language 2 (Hanja)
    (0x92, 90),  // language 3 (Katakana)
    (0x93, 91),  // language 4 (Hiragana)
    (0x94, 85),  // language 5 (Zenkaku/Hankaku)
    (0xE0, 29),  // left Control
    (0xE1, 42),  // left Shift
    (0xE2, 56),  // left Alt
    (0xE3, 125), // left Super
    (0xE4, 97),  // right Control
    (0xE5, 54),  // right Shift
    (0xE6, 100), // right Alt, which is AltGr on a layout that has one
    (0xE7, 126), // right Super
];

/// The consumer page, which has evdev codes of its own.
#[rustfmt::skip]
const USAGE_EVDEV_CONSUMER: &[(u32, u8)] = &[
    (0xB5, 163), // next track
    (0xB6, 165), // previous track
    (0xB7, 166), // stop
    (0xCD, 164), // play/pause
    (0xE2, 113), // mute
    (0xE9, 115), // volume up
    (0xEA, 114), // volume down
];

/// The X keycode a wire usage means, by truth 7.
fn keycode_of(usage: u32) -> Option<u8> {
    let page = usage >> 16;
    let id = usage & 0xFFFF;
    let table = match page {
        keys::PAGE_KEYBOARD => USAGE_EVDEV,
        keys::PAGE_CONSUMER => USAGE_EVDEV_CONSUMER,
        _ => return None,
    };
    table
        .iter()
        .find(|(u, _)| *u == id)
        .and_then(|(_, evdev)| evdev.checked_add(8))
}

/// The wire usage an X keycode means. The inverse of [`keycode_of`].
///
/// Two usages share evdev 43 (the backslash and the non-US hash are one key) and two
/// share 113, 114 and 115 (mute and the volume keys exist on both HID pages), so the
/// FIRST match wins and the keyboard page is searched before the consumer page: the
/// answer is then the usage a person would name for that physical key.
fn usage_of_keycode(keycode: u8) -> Option<u32> {
    let evdev = keycode.checked_sub(8)?;
    // The CONSUMER page first, and it matters for exactly three keys. Mute and the two
    // volume keys exist on both HID pages, and `keys::NAMED` names them on the consumer
    // page: reported on the keyboard page they would arrive with `key: None`, which is the
    // very fallback a target with an incomplete usage table relies on, and they would be
    // `UNRESOLVED` on a Windows target whose table has the consumer page only. No other
    // evdev code appears in both tables, so this order costs nothing else.
    if let Some((id, _)) = USAGE_EVDEV_CONSUMER.iter().find(|(_, e)| *e == evdev) {
        return Some(keys::usage(keys::PAGE_CONSUMER, *id));
    }
    USAGE_EVDEV
        .iter()
        .find(|(_, e)| *e == evdev)
        .map(|(id, _)| keys::usage(keys::PAGE_KEYBOARD, *id))
}

/// The keysym a canonical key name means, for the names the dialect defines that
/// produce no character.
///
/// The name to keysym direction only: the reverse (a keysym to a name) goes through
/// [`usage_of_keycode`], because a name is what the WIRE carries and a keycode is
/// what the OS reports.
#[rustfmt::skip]
fn keysym_of_name(name: &str) -> Option<u32> {
    Some(match name {
        "Enter" => 0xFF0D,        // XK_Return
        "Escape" => 0xFF1B,
        "Backspace" => 0xFF08,
        "Tab" => 0xFF09,
        "Space" => 0x0020,
        "CapsLock" => 0xFFE5,     // XK_Caps_Lock
        "F1" => 0xFFBE, "F2" => 0xFFBF, "F3" => 0xFFC0, "F4" => 0xFFC1,
        "F5" => 0xFFC2, "F6" => 0xFFC3, "F7" => 0xFFC4, "F8" => 0xFFC5,
        "F9" => 0xFFC6, "F10" => 0xFFC7, "F11" => 0xFFC8, "F12" => 0xFFC9,
        "F13" => 0xFFCA, "F14" => 0xFFCB, "F15" => 0xFFCC, "F16" => 0xFFCD,
        "F17" => 0xFFCE, "F18" => 0xFFCF, "F19" => 0xFFD0, "F20" => 0xFFD1,
        "F21" => 0xFFD2, "F22" => 0xFFD3, "F23" => 0xFFD4, "F24" => 0xFFD5,
        "PrintScreen" => 0xFF61,  // XK_Print
        "ScrollLock" => 0xFF14,
        "Pause" => 0xFF13,
        "Insert" => 0xFF63,
        "Home" => 0xFF50,
        "PageUp" => 0xFF55,       // XK_Prior
        "Delete" => 0xFFFF,
        "End" => 0xFF57,
        "PageDown" => 0xFF56,     // XK_Next
        "Right" => 0xFF53,
        "Left" => 0xFF51,
        "Down" => 0xFF54,
        "Up" => 0xFF52,
        "NumLock" => 0xFF7F,
        "NumpadEnter" => 0xFF8D,  // XK_KP_Enter
        "Menu" => 0xFF67,         // XK_Menu
        "MediaPlayPause" => 0x1008FF14,
        "MediaStop" => 0x1008FF15,
        "MediaNext" => 0x1008FF17,
        "MediaPrevious" => 0x1008FF16,
        "VolumeUp" => 0x1008FF13,
        "VolumeDown" => 0x1008FF11,
        "VolumeMute" => 0x1008FF12,
        _ => return None,
    })
}

/// The text a keysym produces, or `None` for a keysym that produces none.
///
/// Two algorithmic rules cover most of it: a keysym in the Latin-1 ranges IS its own
/// code point, and a keysym with `0x01000000` set carries its code point in the low
/// 24 bits. The rest is [`KEYSYM_UNICODE`], which was generated from the `U+` comments
/// in the system's own `keysymdef.h`, the same source libX11's table is built from.
fn text_of_keysym(keysym: u32) -> Option<String> {
    if (0x0020..=0x007E).contains(&keysym) || (0x00A0..=0x00FF).contains(&keysym) {
        return char::from_u32(keysym).map(String::from);
    }
    if keysym & 0xFF00_0000 == 0x0100_0000 {
        return char::from_u32(keysym & 0x00FF_FFFF).map(String::from);
    }
    KEYSYM_UNICODE
        .iter()
        .find(|(k, _)| *k == keysym)
        .map(|(_, c)| String::from(*c))
}

/// The keysym that produces a character, for the keymap search and for the Unicode
/// path's remap.
///
/// The inverse of [`text_of_keysym`], and it answers with the LEGACY keysym where one
/// exists rather than with the `0x01000000` form, because a keymap is written in
/// legacy keysyms and a search for the Unicode form would miss every key on it.
fn keysym_of_char(ch: char) -> u32 {
    let cp = u32::from(ch);
    if (0x0020..=0x007E).contains(&cp) || (0x00A0..=0x00FF).contains(&cp) {
        return cp;
    }
    if let Some((keysym, _)) = KEYSYM_UNICODE.iter().find(|(_, c)| *c == ch) {
        return *keysym;
    }
    0x0100_0000 | cp
}

// ------------------------------------------------------------------ the keymap

/// One keycode's keysyms, as XKB describes them: `width` levels per group,
/// `groups` groups, laid out group-major.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct KeyEntry {
    groups: u8,
    width: u8,
    syms: Vec<u32>,
}

impl KeyEntry {
    fn sym(&self, group: u32, level: u32) -> Option<u32> {
        if group >= u32::from(self.groups) || level >= u32::from(self.width) {
            return None;
        }
        let index = group * u32::from(self.width) + level;
        self.syms
            .get(index as usize)
            .copied()
            .filter(|sym| *sym != 0)
    }
}

/// The whole keyboard, and an identity for it.
#[derive(Clone, Debug, Default)]
struct Keymap {
    first: u8,
    entries: Vec<KeyEntry>,
    /// A hash over every keysym EXCEPT the spare keycode's, so this backend's own
    /// remaps for the Unicode path never look like a layout change (truth 5).
    fingerprint: String,
}

impl Keymap {
    fn entry(&self, keycode: u8) -> Option<&KeyEntry> {
        keycode
            .checked_sub(self.first)
            .and_then(|i| self.entries.get(usize::from(i)))
    }

    /// The keysym at one exact place. Kept for the tests, which is where the level
    /// arithmetic is pinned: the capture path goes through [`level_of_stroke`], which needs
    /// the whole entry rather than one level of it.
    #[cfg(test)]
    fn sym(&self, keycode: u8, group: u32, level: u32) -> Option<u32> {
        self.entry(keycode).and_then(|e| e.sym(group, level))
    }

    /// Which levels of which group produce this keysym. Searched in the order the
    /// caller wants tried: the active group first, then the others, and the lowest
    /// level of each first.
    fn find(&self, keysym: u32, active: u32) -> Option<(u8, u32, u32)> {
        let mut best: Option<(u8, u32, u32)> = None;
        for (index, entry) in self.entries.iter().enumerate() {
            // `checked_add` and not `saturating_add`: past 255 there is no keycode, and
            // saturating would make every entry beyond it answer as keycode 255, which is
            // a wrong key rather than no key. Unreachable (both readers are bounded by the
            // server's own `u8` range) and worth not depending on.
            let Some(keycode) = self.first.checked_add(index as u8) else {
                break;
            };
            for group in 0..u32::from(entry.groups) {
                for level in 0..u32::from(entry.width) {
                    if entry.sym(group, level) != Some(keysym) {
                        continue;
                    }
                    // A level above the fourth needs `ISO_Level5_Shift`, which the
                    // dialect's eight canonical modifier bits cannot name, so it is
                    // not an answer this seam can carry. A group above the fourth is
                    // refused for the mirror reason: `XkbLatchLockState` carries a group
                    // of one to four, so a resolution naming a fifth would have the
                    // engine switch to the fourth and press the key on the wrong one.
                    if level > 3 || group > 3 {
                        continue;
                    }
                    let candidate = (keycode, group, level);
                    let rank = |(_, g, l): (u8, u32, u32)| (g != active, l, g);
                    if best.is_none_or(|current| rank(candidate) < rank(current)) {
                        best = Some(candidate);
                    }
                }
            }
        }
        best
    }
}

/// The canonical modifier bits a level needs.
///
/// The two layout modifiers and nothing else: level 1 is unshifted, 2 is Shift, 3 is
/// AltGr and 4 is both, which is what every keymap XKB compiles from
/// `xkeyboard-config` uses. Anything above is refused by [`Keymap::find`].
fn mods_for_level(level: u32) -> u16 {
    match level {
        0 => 0,
        1 => keys::mods::SHIFT,
        2 => keys::mods::ALTGR,
        _ => keys::mods::SHIFT | keys::mods::ALTGR,
    }
}

/// Which level of a key the modifiers currently held select, for reading the symbol a
/// stroke produced.
fn level_for_mods(mods: u16) -> u32 {
    let shift = mods & keys::mods::SHIFT != 0;
    let altgr = mods & keys::mods::ALTGR != 0;
    match (altgr, shift) {
        (false, false) => 0,
        (false, true) => 1,
        (true, false) => 2,
        (true, true) => 3,
    }
}

/// Is this pair of keysyms a letter and its own capital?
///
/// XKB calls a key type with this shape `ALPHABETIC`, and it is the one type Caps Lock
/// acts on: the lock inverts the shift level for a letter and does nothing at all for a
/// digit or a punctuation mark. Answered from the KEYSYMS rather than from a table of key
/// types, because the two keysyms are what this backend has and they say it exactly.
fn is_alphabetic(lower: Option<u32>, upper: Option<u32>) -> bool {
    let (Some(lower), Some(upper)) = (
        lower.and_then(text_of_keysym),
        upper.and_then(text_of_keysym),
    ) else {
        return false;
    };
    lower != upper && lower.to_uppercase() == upper && upper.to_lowercase() == lower
}

/// The level a stroke really produced, with the locks applied.
///
/// Caps Lock is why this exists and not `level_for_mods` alone. With the lock on, a
/// person presses the `a` key and X produces `A`; a backend that read the unshifted level
/// would report `sym: "a"`, and the target would type a lower case letter for every
/// capital its owner could see on their own screen. The lock INVERTS the shift level, and
/// only for a key whose two levels are a letter and its capital, which is XKB's own rule.
fn level_of_stroke(entry: &KeyEntry, group: u32, mods: u16) -> u32 {
    let level = level_for_mods(mods);
    if mods & keys::mods::CAPS == 0 {
        return level;
    }
    let base = level & !1;
    if !is_alphabetic(entry.sym(group, base), entry.sym(group, base | 1)) {
        return level;
    }
    level ^ 1
}

// -------------------------------------------------------------- the commands

/// A downcall, queued for the loop thread. Everything is here, answering calls
/// included: see the module header for why one thread owns the connection.
enum Cmd {
    Capture(CaptureMode),
    Confine(Option<Rect>),
    Warp(Point),
    Inject(Vec<Action>),
    Release(Vec<PlatformKey>),
    Monitors(oneshot::Sender<Vec<Monitor>>),
    Pointer(oneshot::Sender<Option<Point>>),
    Resolve(Want, oneshot::Sender<Option<Resolved>>),
    Exit(i32),
}

impl std::fmt::Debug for Cmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Cmd::Capture(mode) => write!(f, "Capture({mode:?})"),
            Cmd::Confine(rect) => write!(f, "Confine({rect:?})"),
            Cmd::Warp(to) => write!(f, "Warp({to:?})"),
            Cmd::Inject(actions) => write!(f, "Inject({} actions)", actions.len()),
            Cmd::Release(keys) => write!(f, "Release({} keys)", keys.len()),
            Cmd::Monitors(_) => write!(f, "Monitors"),
            Cmd::Pointer(_) => write!(f, "Pointer"),
            Cmd::Resolve(want, _) => write!(f, "Resolve({want:?})"),
            Cmd::Exit(code) => write!(f, "Exit({code})"),
        }
    }
}

// ---------------------------------------------------------------- the handle

/// The cheap, `Clone` handle the engine holds. It carries no X resource: a command
/// queue, the loop's waker, and the capabilities the loop keeps up to date.
#[derive(Clone, Debug)]
pub struct X11Backend {
    cmds: Arc<Mutex<VecDeque<Cmd>>>,
    waker: Arc<Waker>,
    /// Read synchronously by the engine on every session decision, so it cannot be an
    /// answering downcall: the loop writes it whenever something changes and this is a
    /// lock nobody contends for.
    caps: Arc<Mutex<Capabilities>>,
}

impl X11Backend {
    /// Pushes a command, then wakes the loop. Push BEFORE the wake, or a coalesced
    /// wake could drop the command. A poisoned mutex is recovered so an `Exit` or a
    /// `Release` is never lost.
    fn push(&self, cmd: Cmd) {
        match self.cmds.lock() {
            Ok(mut q) => q.push_back(cmd),
            Err(p) => p.into_inner().push_back(cmd),
        }
        // A lost wake leaves the command queued and the loop caps its poll at
        // POLL_CAP, so it costs latency and never correctness.
        if let Err(e) = self.waker.wake() {
            warn(&format!("waking the X11 loop failed: {e}"));
        }
    }
}

impl InputBackend for X11Backend {
    fn capabilities(&self) -> Capabilities {
        match self.caps.lock() {
            Ok(caps) => caps.clone(),
            Err(p) => p.into_inner().clone(),
        }
    }

    fn monitors(&self) -> impl Future<Output = Vec<Monitor>> + Send {
        // Pushed synchronously, before the future is returned, so the ordering with
        // the fire and forget downcalls holds however late it is polled.
        let (reply, rx) = oneshot::channel();
        self.push(Cmd::Monitors(reply));
        // A dropped sender (the loop is gone) is no monitors, which is what a machine
        // that cannot enumerate its screens publishes.
        async move { rx.await.unwrap_or_default() }
    }

    fn pointer(&self) -> impl Future<Output = Option<Point>> + Send {
        let (reply, rx) = oneshot::channel();
        self.push(Cmd::Pointer(reply));
        async move { rx.await.unwrap_or(None) }
    }

    fn resolve(&self, want: Want) -> impl Future<Output = Option<Resolved>> + Send {
        let (reply, rx) = oneshot::channel();
        self.push(Cmd::Resolve(want, reply));
        // `None` is a legitimate answer, so a loop that has gone reads as "this
        // machine cannot produce that", which is true of a machine with no loop.
        async move { rx.await.unwrap_or(None) }
    }

    fn capture(&self, mode: CaptureMode) {
        self.push(Cmd::Capture(mode));
    }

    fn confine(&self, rect: Option<Rect>) {
        self.push(Cmd::Confine(rect));
    }

    fn warp(&self, to: Point) {
        self.push(Cmd::Warp(to));
    }

    fn inject(&self, actions: Vec<Action>) {
        self.push(Cmd::Inject(actions));
    }

    fn release_all(&self, keys: Vec<PlatformKey>) {
        self.push(Cmd::Release(keys));
    }

    fn request_exit(&self, code: i32) {
        // Never `std::process::exit`: this runs on the engine's thread, and exiting
        // here would skip the ungrab and the show-cursor on a machine that may be
        // holding somebody's keyboard.
        self.push(Cmd::Exit(code));
    }
}

// ------------------------------------------------------------- the loop side

/// Which extensions this server actually has. Every one is optional and every
/// absence narrows [`Capabilities`].
#[derive(Clone, Copy, Debug, Default)]
struct Ext {
    test: bool,
    xi: bool,
    xkb: bool,
    randr: bool,
    xfixes: bool,
}

/// Backend state, living on the pump thread. It owns the connection, which is where
/// every X call in this file happens.
struct Backend {
    conn: xcb::Connection,
    root: x::Window,
    ext: Ext,
    poll: Poll,
    cmds: Arc<Mutex<VecDeque<Cmd>>>,
    events_tx: mpsc::Sender<BackendEvent>,
    caps: Arc<Mutex<Capabilities>>,

    keymap: Keymap,
    keysyms_per: u8,
    /// A keycode nothing is bound to, for the Unicode path (truth 5).
    spare: Option<u8>,
    /// Whether that keycode is bound RIGHT NOW, which it is only during a `Text`
    /// injection.
    ///
    /// A keymap change is the one thing this backend does that does NOT belong to its X
    /// client: the server undoes a grab, a cursor hide and an event selection when the
    /// connection closes, and it keeps a `ChangeKeyboardMapping` for ever. So a death
    /// between the remap and the restore would leave a stray keysym on a keycode for
    /// every application on the display, for the rest of the X session, and the next
    /// start would pick a different spare and strand another one. This flag is what lets
    /// every ordinary path out put it back.
    spare_bound: bool,
    /// The active keyboard group, from XKB.
    group: u32,
    /// The canonical modifier bits, tracked from the captured stream because a
    /// swallowed modifier is still one the source is holding, and seeded from XKB
    /// whenever capture starts.
    mods: u16,
    layout: String,

    mode: CaptureMode,
    grabbed: bool,
    hidden: bool,
    confine: Option<Rect>,
    anchor: Point,

    /// Raw motion summed since the last turn, in 32nds of a pixel (see [`raw_delta`]),
    /// so a burst of device events becomes one upcall with one round trip for the
    /// position rather than one each, and a fraction of a pixel is carried over rather
    /// than truncated away.
    pending_dx: i64,
    pending_dy: i64,
    /// Scroll movement below one notch, in pixels, kept so a trackpad is not rounded to
    /// nothing event after event. In `i64` so the arithmetic on a peer's number cannot
    /// overflow (see [`Backend::wheel`]).
    wheel_x: i64,
    wheel_y: i64,

    monitors_stable: bool,
    /// The control upcalls a full queue turned away, retried next turn (see
    /// [`Backend::emit`]).
    deferred: Vec<BackendEvent>,
    shutdown: bool,
    exit_code: Option<i32>,
    dropped: u64,
    /// Protocol errors seen, so the log says how many rather than saying each.
    errors: u64,
    /// The engine's end of the upcall channel has closed.
    ///
    /// It means the engine has gone, and it is worth ONE more turn before acting on it.
    /// `main` drops the receiver when the engine returns and only then calls
    /// `request_exit`, so at a clean stop this flag can be set a moment before the exit
    /// code arrives: shutting down on the spot would exit 1 (the supervisor's signal to
    /// restart) for a component that stopped exactly as it was asked to. One turn is
    /// enough, because a turn begins by draining the command queue.
    engine_gone: bool,
    /// Whether that has already been seen once.
    gone_seen: bool,
    /// The last pointer position a GRABBED pointer reported, so an unpinned grab still
    /// produces a movement rather than a position (see `on_core_motion`).
    last_core: Option<Point>,
}

impl Backend {
    /// Pushes an upcall, and NEVER blocks the loop.
    ///
    /// A full queue is treated differently depending on what is in the event, and that
    /// distinction is the whole of this function. A motion, a button, a wheel notch or a
    /// keystroke is a moment: dropping one costs a frame nobody will notice, and the
    /// design already says positions are coalesced and superseded ones thrown away. The
    /// four CONTROL events are not moments, they are facts that change what everything
    /// afterwards means, and dropping one is a bug with a long tail: a `LayoutChanged`
    /// lost behind a key repeat burst leaves the engine holding a resolution cache about
    /// a keymap that is gone and a held set it will never release, which is the stuck
    /// Control this whole component is built to avoid. So those are kept and retried on
    /// the next turn.
    fn emit(&mut self, event: BackendEvent) {
        use mpsc::error::TrySendError;
        let control = matches!(
            event,
            BackendEvent::LayoutChanged { .. }
                | BackendEvent::CapabilitiesChanged
                | BackendEvent::MonitorsChanged
                | BackendEvent::CaptureLost(_)
        );
        match self.events_tx.try_send(event) {
            Ok(()) => {}
            // Not a shutdown on the spot: see `Backend::engine_gone`.
            Err(TrySendError::Closed(_)) => self.engine_gone = true,
            Err(TrySendError::Full(event)) => {
                if control {
                    // At most one per KIND, which is what makes this need no cap at all.
                    // All four are idempotent FACTS rather than moments, so the newest of a
                    // kind says everything an older one did. It used to be a flat cap of 32
                    // events, which silently dropped everything past it: a mode set emits a
                    // `MonitorsChanged` per CRTC, so 32 of those could fill the buffer while
                    // the engine was stalled and the `LayoutChanged` behind them, whose loss
                    // strands a held key for ever, would be the one thrown away.
                    let kind = control_kind(&event);
                    if let Some(slot) = self
                        .deferred
                        .iter_mut()
                        .find(|held| control_kind(held) == kind)
                    {
                        *slot = event;
                    } else {
                        self.deferred.push(event);
                    }
                    return;
                }
                self.dropped += 1;
                if self.dropped.is_multiple_of(128) {
                    warn("the engine is not draining input events; dropping some");
                }
            }
        }
    }

    /// Tries the control upcalls a full queue turned away.
    ///
    /// One per kind is held (see `emit`), so this is at most four sends.
    fn retry_deferred(&mut self) {
        if self.deferred.is_empty() {
            return;
        }
        for event in std::mem::take(&mut self.deferred) {
            self.emit(event);
        }
    }

    /// The main-thread pump. Commands first, then X events, then the work a burst of
    /// events leaves behind, then a bounded wait. The order is
    /// `clipboard/src/x11.rs`'s and for its reason: a command must be able to land
    /// before the OS event that would otherwise race it.
    fn run(&mut self) -> i32 {
        let mut events = Events::with_capacity(8);
        loop {
            if let Err(e) = self.conn.flush() {
                warn(&format!("flush (main loop) failed, stopping: {e:?}"));
                return self.exit_code.unwrap_or(1);
            }
            if let Err(e) = self.poll.poll(&mut events, Some(POLL_CAP)) {
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                warn(&format!("mio poll (main loop) failed, stopping: {e}"));
                return self.exit_code.unwrap_or(1);
            }
            // Both queues drained unconditionally rather than per readiness token.
            // Under edge triggered epoll a readiness can be consumed without the work
            // being done (a command queued while a reply was awaited, X events libxcb
            // buffered during that read), so it would not fire again.
            self.retry_deferred();
            self.process_cmds();
            // Asked here as well as learned in `emit`, because `emit` only learns it when
            // there is something to send. The state in which this machine has keys physically
            // DOWN is `driven`, which is capture Off: no grab, no raw events selected, nothing
            // that can ever emit. So the one case that matters most is the one traffic alone
            // would never notice.
            if self.events_tx.is_closed() {
                self.engine_gone = true;
            }
            if self.engine_gone {
                if self.gone_seen || self.exit_code.is_some() {
                    if self.exit_code.is_none() {
                        warn("the engine's end of the upcall channel closed; stopping");
                    }
                    self.shutdown = true;
                } else {
                    self.gone_seen = true;
                }
            }
            self.drain_x();
            self.flush_motion();
            if self.shutdown {
                self.release_everything();
                let _ = self.conn.flush();
                return self.exit_code.unwrap_or(1);
            }
        }
    }

    fn process_cmds(&mut self) {
        let drained: Vec<Cmd> = {
            let mut guard = match self.cmds.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            guard.drain(..).collect()
        };
        for cmd in drained {
            // Nothing queued behind an `Exit` runs. The teardown that follows would undo it
            // anyway on this platform, so this is tidiness here; the same `break` is not
            // tidiness on macOS, where a `Capture` behind an `Exit` can put a permission
            // dialog on the screen of a component that is shutting down.
            if self.shutdown {
                break;
            }
            match cmd {
                Cmd::Capture(mode) => self.set_capture(mode),
                Cmd::Confine(rect) => self.set_confine(rect),
                Cmd::Warp(to) => self.warp_to(to),
                Cmd::Inject(actions) => self.inject(&actions),
                Cmd::Release(keys) => self.release(&keys),
                Cmd::Monitors(reply) => {
                    let monitors = self.read_monitors();
                    let _ = reply.send(monitors);
                }
                Cmd::Pointer(reply) => {
                    let at = self.query_pointer();
                    let _ = reply.send(at);
                }
                Cmd::Resolve(want, reply) => {
                    let answer = self.resolve(&want);
                    let _ = reply.send(answer);
                }
                Cmd::Exit(code) => {
                    self.exit_code = Some(code);
                    self.shutdown = true;
                }
            }
        }
    }

    // ------------------------------------------------------------- the events

    fn drain_x(&mut self) {
        loop {
            match self.conn.poll_for_event() {
                Ok(Some(event)) => self.on_event(event),
                Ok(None) => return,
                Err(xcb::Error::Protocol(e)) => {
                    // Recoverable: a request we fired unchecked was refused (a grab
                    // on a device that went away, a property on an output that did).
                    // The connection is fine, so keep draining.
                    //
                    // Counted and only occasionally said out loud. This repository has
                    // already paid once for an unthrottled line per event (the Android
                    // log flood, PR #74), and the requests that can produce one here are
                    // on the injection path, so one error can mean one per keystroke.
                    self.errors += 1;
                    if self.errors.is_multiple_of(64) {
                        warn(&format!(
                            "X protocol errors, {} so far, latest: {e:?}",
                            self.errors
                        ));
                    }
                }
                Err(xcb::Error::Connection(e)) => {
                    warn(&format!("X connection lost, stopping: {e:?}"));
                    // Every session using this capture ends, and the engine says why.
                    self.emit(BackendEvent::CaptureLost(CaptureLoss::SessionEnded));
                    self.shutdown = true;
                    return;
                }
            }
        }
    }

    fn on_event(&mut self, event: xcb::Event) {
        match event {
            // The RAW stream, which is what arrives while this backend only WATCHES.
            // Ignored while grabbed, and that is not belt and braces: a server that
            // delivered both streams at once would report every movement twice.
            xcb::Event::Input(xinput::Event::RawMotion(ev)) => {
                if !self.grabbed {
                    self.on_raw_motion(&ev);
                }
            }
            xcb::Event::Input(xinput::Event::RawKeyPress(ev)) => {
                if !self.grabbed {
                    self.on_raw_key(&ev, true);
                }
            }
            xcb::Event::Input(xinput::Event::RawKeyRelease(ev)) => {
                if !self.grabbed {
                    self.on_raw_key(&ev, false);
                }
            }
            xcb::Event::Input(xinput::Event::RawButtonPress(ev)) => {
                if !self.grabbed {
                    self.on_raw_button(&ev, true);
                }
            }
            xcb::Event::Input(xinput::Event::RawButtonRelease(ev)) => {
                if !self.grabbed {
                    self.on_raw_button(&ev, false);
                }
            }
            // The CORE stream, which is what a GRAB delivers and the only thing a
            // swallowing backend can see (truth 8's exception). The mode gate is the
            // other half of the one above: outside a grab of ours these are events this
            // backend never asked for.
            xcb::Event::X(x::Event::KeyPress(ev)) => {
                if self.grabbed {
                    self.on_key(ev.detail(), true);
                }
            }
            xcb::Event::X(x::Event::KeyRelease(ev)) => {
                if self.grabbed {
                    self.on_key(ev.detail(), false);
                }
            }
            xcb::Event::X(x::Event::ButtonPress(ev)) => {
                if self.grabbed {
                    self.on_button(u32::from(ev.detail()), true);
                }
            }
            xcb::Event::X(x::Event::ButtonRelease(ev)) => {
                if self.grabbed {
                    self.on_button(u32::from(ev.detail()), false);
                }
            }
            xcb::Event::X(x::Event::MotionNotify(ev)) => {
                if self.grabbed {
                    self.on_core_motion(ev.root_x(), ev.root_y());
                }
            }
            xcb::Event::Xkb(xkb::Event::StateNotify(ev)) => {
                let group = ev.group() as u32;
                if group != self.group {
                    self.group = group;
                    self.announce_layout();
                }
            }
            xcb::Event::Xkb(xkb::Event::MapNotify(_))
            | xcb::Event::Xkb(xkb::Event::NewKeyboardNotify(_)) => self.reload_keymap(),
            xcb::Event::X(x::Event::MappingNotify(ev)) => {
                // Our OWN remap for the Unicode path is not a keymap change worth
                // reading the whole keyboard for: the fingerprint excludes that keycode,
                // so the reload would compute the same answer, and a thirty two character
                // string would have paid for thirty three of them (two more round trips
                // each) while the command queue waited.
                let ours = ev.request() == x::Mapping::Keyboard
                    && ev.count() == 1
                    && self.spare == Some(ev.first_keycode());
                if ev.request() != x::Mapping::Pointer && !ours {
                    self.reload_keymap();
                }
            }
            xcb::Event::RandR(_) => {
                self.emit(BackendEvent::MonitorsChanged);
            }
            _ => {}
        }
    }

    /// One device motion. The valuators are the unaccelerated device deltas (truth 2)
    /// and they are SUMMED until the end of the turn: a burst of ten events becomes
    /// one upcall with one round trip for the position instead of ten.
    fn on_raw_motion(&mut self, ev: &xinput::RawMotionEvent) {
        let (dx, dy) = raw_delta(ev);
        self.pending_dx = self.pending_dx.saturating_add(dx);
        self.pending_dy = self.pending_dy.saturating_add(dy);
        // The pin, and it needs no position: a warp to the anchor is idempotent, and it
        // generates no raw event of its own because no device moved (truth 3).
        if let Some(anchor) = self.confine.map(|_| self.anchor) {
            self.warp_to(anchor);
        }
    }

    /// One pointer position from a grabbed pointer, turned into the movement it means.
    ///
    /// The delta is a DIFFERENCE of positions here and not a device delta, because a grab
    /// is what makes the raw stream unavailable (truth 8's exception) and this is what the
    /// grab hands over instead. Two consequences worth having written down. It is the
    /// ACCELERATED movement, the same thing the Windows backend gets from its hook, so the
    /// pointer feels the way this machine's own pointer feels rather than linear; that is
    /// the choice the seam's truth 4 asked to be made on purpose, and on this platform it is
    /// now made by what a grab will and will not deliver. And while the pin is in place the
    /// difference is taken against the ANCHOR, which is where the last event's warp put the
    /// cursor, so one event can carry at most half a screen of movement before the server's
    /// own clamp eats the rest.
    fn on_core_motion(&mut self, x: i16, y: i16) {
        let (at, from) = (
            Point {
                x: i32::from(x),
                y: i32::from(y),
            },
            match self.confine {
                Some(_) => self.anchor,
                None => self.last_core.unwrap_or(Point {
                    x: i32::from(x),
                    y: i32::from(y),
                }),
            },
        );
        self.last_core = Some(at);
        let (dx, dy) = (at.x - from.x, at.y - from.y);
        // Whole pixels already, so they go straight into the same accumulator the raw path
        // uses: the fixed point shift keeps one arithmetic for both streams.
        self.pending_dx = self.pending_dx.saturating_add(i64::from(dx) << 32);
        self.pending_dy = self.pending_dy.saturating_add(i64::from(dy) << 32);
        // The pin, and only when the pointer is not already on it: this stream includes the
        // motion the WARP itself generates, which arrives at the anchor and would otherwise
        // buy a second warp request for a pointer that is already where it belongs.
        if self.confine.is_some() && (at.x, at.y) != (self.anchor.x, self.anchor.y) {
            self.warp_to(self.anchor);
        }
    }

    /// The end of a turn: one motion upcall for everything the burst added up to.
    fn flush_motion(&mut self) {
        if self.pending_dx == 0 && self.pending_dy == 0 {
            return;
        }
        // Whole pixels out, the fraction kept for the next turn (see `raw_delta`).
        let dx = spend_pixels(&mut self.pending_dx);
        let dy = spend_pixels(&mut self.pending_dy);
        if dx == 0 && dy == 0 {
            return;
        }
        // While watching, the engine needs to know where the pointer IS: it asks the
        // crossing graph about that position plus this delta, and the OS clamps the
        // pointer at its own desktop's edge, so the delta is the only thing that can
        // ever go past it. While confined, the position is whatever the pin left it at
        // and the engine ignores it, so the round trip is not paid.
        let at = if self.confine.is_some() {
            self.anchor
        } else {
            self.query_pointer().unwrap_or(self.anchor)
        };
        self.emit(BackendEvent::Motion(Motion { at, dx, dy }));
    }

    fn on_raw_key(&mut self, ev: &xinput::RawKeyPressEvent, down: bool) {
        self.on_key((ev.detail() & 0xFF) as u8, down);
    }

    /// One key, from whichever of the two streams this mode gets (see `on_event`).
    fn on_key(&mut self, keycode: u8, down: bool) {
        // A key repeat is the OS repeating what is already held. The far side would
        // receive a press for a key it believes is down, and its held set would then
        // hold one entry for two presses; the release still matches, so this is
        // forwarded rather than dropped, exactly as a local application would see it.
        let usage = usage_of_keycode(keycode).unwrap_or(0);
        if let Some(bit) = keys::mod_of_usage(usage) {
            if down {
                self.mods |= bit;
            } else {
                self.mods &= !bit;
            }
        }
        let is_lock = usage != 0 && keys::is_lock(usage);
        if is_lock {
            if !down {
                // A half duplex lock reports only its press: a target that waited for
                // the release would hold the lock down for ever.
                return;
            }
            let bit = match usage {
                u if Some(u) == keys::usage_of("CapsLock") => keys::mods::CAPS,
                u if Some(u) == keys::usage_of("NumLock") => keys::mods::NUM,
                _ => keys::mods::SCROLL,
            };
            self.mods ^= bit;
        }
        let sym = self
            .keymap
            .entry(keycode)
            .and_then(|entry| entry.sym(self.group, level_of_stroke(entry, self.group, self.mods)))
            .and_then(text_of_keysym);
        let key = (usage != 0).then(|| keys::name_of(usage)).flatten();
        let event = BackendEvent::Key(KeyEvent {
            usage,
            key: key.map(str::to_string),
            sym,
            mods: self.mods,
            down,
            lock: is_lock,
        });
        self.emit(event);
    }

    fn on_raw_button(&mut self, ev: &xinput::RawButtonPressEvent, down: bool) {
        self.on_button(ev.detail(), down);
    }

    /// One button or wheel notch, from whichever of the two streams this mode gets.
    fn on_button(&mut self, detail: u32, down: bool) {
        match detail {
            // X11 delivers the wheel as buttons: 4 up, 5 down, 6 left, 7 right. Only
            // the press carries the notch, so the release is dropped rather than
            // doubling every scroll.
            4..=7 => {
                if !down {
                    return;
                }
                let (dx, dy) = match detail {
                    4 => (0, 1),
                    5 => (0, -1),
                    6 => (-1, 0),
                    _ => (1, 0),
                };
                self.emit(BackendEvent::Wheel {
                    dx,
                    dy,
                    pixels: false,
                });
            }
            1..=3 => self.emit(BackendEvent::Button {
                button: detail as u8,
                down,
            }),
            // X's back and forward, which the dialect numbers 4 and 5.
            8 => self.emit(BackendEvent::Button { button: 4, down }),
            9 => self.emit(BackendEvent::Button { button: 5, down }),
            // Anything else is a button this dialect has no number for, and inventing
            // one would have the far side press something nobody touched.
            _ => {}
        }
    }

    // ------------------------------------------------------------- the capture

    fn set_capture(&mut self, mode: CaptureMode) {
        if !self.ext.xi {
            return;
        }
        if mode == self.mode {
            return;
        }
        let was_off = self.mode == CaptureMode::Off;
        self.mode = mode;
        match mode {
            CaptureMode::Off => {
                self.ungrab();
                self.select_raw(false);
                // The pin goes with it. The engine always releases it first, so this is
                // belt rather than braces: a confinement left behind would have the next
                // Watch pinning the pointer with no session asking for it.
                self.confine = None;
            }
            CaptureMode::Watch | CaptureMode::Swallow => {
                if was_off {
                    self.select_raw(true);
                    // Seeded from the server, and only here: while nothing is grabbed
                    // the server's own state IS the truth, and once this backend
                    // swallows, a modifier the user is holding never reaches it.
                    self.read_state();
                }
                if mode == CaptureMode::Swallow {
                    self.grab();
                } else {
                    self.ungrab();
                }
            }
        }
    }

    /// Selects (or deselects) the raw device events on the root window.
    ///
    /// On the ROOT and for the master devices, which is not a detail: raw events are
    /// only delivered to a client that selected them there (truth 1).
    fn select_raw(&mut self, on: bool) {
        let mask = if on {
            xinput::XiEventMask::RAW_MOTION
                | xinput::XiEventMask::RAW_KEY_PRESS
                | xinput::XiEventMask::RAW_KEY_RELEASE
                | xinput::XiEventMask::RAW_BUTTON_PRESS
                | xinput::XiEventMask::RAW_BUTTON_RELEASE
        } else {
            xinput::XiEventMask::empty()
        };
        let masks = [xinput::EventMaskBuf::new(
            xinput::Device::AllMaster,
            &[mask],
        )];
        self.conn.send_request(&xinput::XiSelectEvents {
            window: self.root,
            masks: &masks,
        });
        let _ = self.conn.flush();
    }

    /// Takes the two grabs that make the source's own input stop acting locally, and
    /// hides the pointer while they last.
    ///
    /// # Why the CORE grabs and not `XIGrabDevice`
    ///
    /// The XInput2 request is the obvious one and it is not usable from this crate:
    /// `xcb` 1.7.1's hand written `impl WiredOut for xinput::Device` writes a `u16`
    /// through an unaligned raw pointer (`*(wire_buf.as_mut_ptr() as *mut u16)`) into a
    /// one-byte-aligned stack buffer, and rustc's alignment check turns that into a
    /// non-unwinding panic in any build with debug assertions. It aborted the process
    /// on the first live test that took a grab. Every request carrying an
    /// `xinput::Device` field is affected, so this backend avoids all of them:
    /// `XiSelectEvents` is the exception and is safe, because its `EventMaskBuf`
    /// serializes the device at offset zero of a `Vec`, which the allocator aligns.
    ///
    /// The core requests are not a downgrade. `GrabKeyboard` and `GrabPointer` deliver the
    /// device's events to the GRAB WINDOW and to nobody else, and this backend selects
    /// nothing on that window, so the events are discarded: that is exactly what swallowing
    /// is, and the live suite proves it against a second client whose focused window stops
    /// receiving keys. The pointer grab's event mask is empty for the same reason.
    ///
    /// # The pointer mask is not optional, and finding that out cost a measurement
    ///
    /// The mask used to be empty, on the reasoning that this backend reads the raw XInput2
    /// stream and wants none of the events the grab takes away from everybody else. That
    /// reasoning was wrong, and wrong in the worst possible place: while a device is
    /// GRABBED the server hands its events to the grab rather than to the ordinary
    /// selections, and a raw event has no core form to convert into, so a core grab drops
    /// it entirely. Measured on an Xvfb after a Watch to Swallow transition, which is the
    /// sequence a real session takes: six faked pointer moves and twelve faked keys
    /// produced ZERO upcalls. The capture died at the exact moment a session started, and
    /// two live tests passed anyway because the one event each waited for had been
    /// generated before the server processed the grab request.
    ///
    /// So while grabbed this backend reads the CORE events the grab delivers (this mask,
    /// plus the key events `GrabKeyboard` reports by definition), and while only watching it
    /// reads the raw stream, which is the only thing that works THERE because a core mask
    /// on the root is subject to propagation (truth 1). `on_event` gates the two on
    /// `grabbed` so nothing is ever counted twice, and `on_core_motion` says what the
    /// difference costs: the movement becomes the accelerated one, which is what the
    /// Windows backend gets from its hook and is the choice the seam's truth 4 asked to be
    /// made deliberately.
    ///
    /// `owner_events: false` stays, and now it is the whole swallow rather than half of it:
    /// every pointer and key event goes to the grab window and to nobody else. The live
    /// suite proves the grab is in place BEFORE it looks for an upcall, and drains
    /// everything older, so neither half can pass by that race again.
    fn grab(&mut self) {
        if self.grabbed {
            return;
        }
        let keyboard = self.conn.send_request(&x::GrabKeyboard {
            owner_events: false,
            grab_window: self.root,
            time: x::CURRENT_TIME,
            pointer_mode: x::GrabMode::Async,
            keyboard_mode: x::GrabMode::Async,
        });
        let pointer = self.conn.send_request(&x::GrabPointer {
            owner_events: false,
            grab_window: self.root,
            // NOT empty, and this mask is the swallowing backend's only source of
            // observation: while the pointer is grabbed the server hands its events to
            // this grab instead of to the raw selection, so an empty mask is a session
            // that sees nothing at all (see this function's own documentation).
            event_mask: x::EventMask::POINTER_MOTION
                | x::EventMask::BUTTON_PRESS
                | x::EventMask::BUTTON_RELEASE,
            pointer_mode: x::GrabMode::Async,
            keyboard_mode: x::GrabMode::Async,
            confine_to: x::WINDOW_NONE,
            cursor: x::CURSOR_NONE,
            time: x::CURRENT_TIME,
        });
        let keyboard = self
            .conn
            .wait_for_reply(keyboard)
            .map(|reply| reply.status());
        let pointer = self
            .conn
            .wait_for_reply(pointer)
            .map(|reply| reply.status());
        let ok = matches!(keyboard, Ok(x::GrabStatus::Success))
            && matches!(pointer, Ok(x::GrabStatus::Success));
        if !ok {
            warn(&format!(
                "the keyboard or the pointer could not be grabbed: {keyboard:?} and \
                 {pointer:?}"
            ));
            // Half a grab is worse than none: the keyboard would be consumed and the
            // pointer would not, or the other way round. So whichever half succeeded is
            // undone (ungrabbing one this client does not hold is a no-op, which is why
            // it can be done without knowing which half it was) and the engine is told,
            // which ends the session with a reason.
            self.grabbed = true;
            self.ungrab();
            // And the mode is put BACK to what is actually installed. Leaving it saying
            // Swallow would make the next `capture(Swallow)` a no-op ("already in that
            // mode") and this machine would report that it is swallowing while every
            // keystroke acted locally and remotely at once.
            self.mode = CaptureMode::Watch;
            self.emit(BackendEvent::CaptureLost(CaptureLoss::Broken));
            return;
        }
        self.grabbed = true;
        // Nothing carries over from the last grab: the first position a new one reports is
        // where the pointer IS, not a movement from wherever it was left.
        self.last_core = None;
        // The pointer is about to sit on one pixel for the whole session; hiding it is
        // what makes that invisible rather than odd. XFixes counts this per client, so
        // the server restores the cursor by itself if this process dies.
        if self.ext.xfixes && !self.hidden {
            self.conn
                .send_request(&xfixes::HideCursor { window: self.root });
            self.hidden = true;
        }
        let _ = self.conn.flush();
    }

    fn ungrab(&mut self) {
        self.last_core = None;
        if self.grabbed {
            self.conn.send_request(&x::UngrabKeyboard {
                time: x::CURRENT_TIME,
            });
            self.conn.send_request(&x::UngrabPointer {
                time: x::CURRENT_TIME,
            });
            self.grabbed = false;
        }
        if self.hidden {
            self.conn
                .send_request(&xfixes::ShowCursor { window: self.root });
            self.hidden = false;
        }
        let _ = self.conn.flush();
    }

    fn set_confine(&mut self, rect: Option<Rect>) {
        self.confine = rect;
        if let Some(rect) = rect {
            self.anchor = Point {
                x: rect.x.saturating_add(rect.w / 2),
                y: rect.y.saturating_add(rect.h / 2),
            };
            // The centre, so a single event's movement has half a monitor of room
            // before the server's own clamp at the screen edge could eat part of it.
            self.warp_to(self.anchor);
        }
        let _ = self.conn.flush();
    }

    fn warp_to(&mut self, to: Point) {
        self.conn.send_request(&x::WarpPointer {
            src_window: x::WINDOW_NONE,
            dst_window: self.root,
            src_x: 0,
            src_y: 0,
            src_width: 0,
            src_height: 0,
            dst_x: clamp_i16(to.x),
            dst_y: clamp_i16(to.y),
        });
    }

    // ---------------------------------------------------------- the injection

    fn inject(&mut self, actions: &[Action]) {
        if !self.ext.test {
            return;
        }
        for action in actions {
            match action {
                Action::MoveTo(to) => self.fake(FAKE_MOTION, 0, clamp_i16(to.x), clamp_i16(to.y)),
                Action::MoveBy { dx, dy } => {
                    if *dx != 0 || *dy != 0 {
                        // Detail 1 means the coordinates are relative.
                        self.fake(FAKE_MOTION, 1, clamp_i16(*dx), clamp_i16(*dy));
                    }
                }
                Action::Button { button, down } => {
                    if let Some(detail) = x_button(*button) {
                        let kind = if *down {
                            FAKE_BUTTON_PRESS
                        } else {
                            FAKE_BUTTON_RELEASE
                        };
                        self.fake(kind, detail, 0, 0);
                    }
                }
                Action::Wheel { dx, dy, pixels } => self.wheel(*dx, *dy, *pixels),
                Action::Key { code, down } => {
                    let kind = if *down {
                        FAKE_KEY_PRESS
                    } else {
                        FAKE_KEY_RELEASE
                    };
                    self.fake(kind, (code.code & 0xFF) as u8, 0, 0);
                }
                Action::Text(text) => self.type_text(text),
                Action::Group(group) => self.lock_group(*group),
            }
        }
        let _ = self.conn.flush();
    }

    fn release(&mut self, keys: &[PlatformKey]) {
        if !self.ext.test {
            return;
        }
        for code in keys {
            self.fake(FAKE_KEY_RELEASE, (code.code & 0xFF) as u8, 0, 0);
        }
        let _ = self.conn.flush();
    }

    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) {
        self.conn.send_request(&xtest::FakeInput {
            r#type: kind,
            detail,
            time: 0,
            root: self.root,
            root_x: x,
            root_y: y,
            deviceid: FAKE_DEVICE,
        });
    }

    /// The wheel, as X11 has it: a button press and release per notch.
    ///
    /// A pixel precise scroll is accumulated and spent in whole notches, with the
    /// remainder kept: X has no pixel wheel to inject into, and rounding each event to
    /// nothing would make a trackpad scroll nothing at all.
    fn wheel(&mut self, dx: i32, dy: i32, pixels: bool) {
        // Clamped first, and in `i64` below, because the numbers come off a peer's frame:
        // the accumulator's `nx * WHEEL_PIXELS_PER_NOTCH` overflows an `i32` for a large
        // enough delta, which is a PANIC in a debug build. The dialect bounds it too
        // ([`crate::wire::WHEEL_MAX`]); this is the second bound, because a backend that
        // trusts its caller for arithmetic is one refactor away from not being bounded.
        let cap = crate::wire::WHEEL_MAX;
        let (dx, dy) = (dx.clamp(-cap, cap), dy.clamp(-cap, cap));
        let (dx, dy) = if pixels {
            self.wheel_x = self.wheel_x.saturating_add(i64::from(dx));
            self.wheel_y = self.wheel_y.saturating_add(i64::from(dy));
            let per = i64::from(WHEEL_PIXELS_PER_NOTCH);
            let nx = self.wheel_x / per;
            let ny = self.wheel_y / per;
            self.wheel_x -= nx * per;
            self.wheel_y -= ny * per;
            (nx as i32, ny as i32)
        } else {
            (dx, dy)
        };
        // And a third bound, on the number of REQUESTS: X11 has no wheel event, only a
        // button press and release per notch, so a peer sending the dialect's maximum
        // would otherwise cost eight thousand X requests for one frame. Nothing a device
        // produces comes near this.
        for _ in 0..dy.unsigned_abs().min(64) {
            let button = if dy > 0 { 4 } else { 5 };
            self.fake(FAKE_BUTTON_PRESS, button, 0, 0);
            self.fake(FAKE_BUTTON_RELEASE, button, 0, 0);
        }
        for _ in 0..dx.unsigned_abs().min(64) {
            let button = if dx > 0 { 7 } else { 6 };
            self.fake(FAKE_BUTTON_PRESS, button, 0, 0);
            self.fake(FAKE_BUTTON_RELEASE, button, 0, 0);
        }
    }

    /// Types a string through the one Unicode path X11 has: a keycode nothing is
    /// bound to, remapped for one stroke and restored (truth 5).
    fn type_text(&mut self, text: &str) {
        let Some(spare) = self.spare else {
            // Should be unreachable: the engine only emits `Text` when the capabilities
            // say `unicode`, which says `spare.is_some()`. Said out loud rather than
            // dropped, because a character that vanishes with nobody told is the one
            // behaviour this whole feature exists not to have.
            warn("a character cannot be typed: this keyboard has no spare keycode");
            return;
        };
        for ch in text.chars() {
            let keysym = keysym_of_char(ch);
            let syms = vec![keysym; usize::from(self.keysyms_per.max(1))];
            self.spare_bound = true;
            self.conn.send_request(&x::ChangeKeyboardMapping {
                keycode_count: 1,
                first_keycode: spare,
                keysyms_per_keycode: self.keysyms_per.max(1),
                keysyms: &syms,
            });
            // A round trip, and it is not optional: the server translates a fake key
            // with the keymap it HAS, and without a sync the stroke can be processed
            // before the remap that gave it a meaning.
            self.sync();
            self.fake(FAKE_KEY_PRESS, spare, 0, 0);
            self.fake(FAKE_KEY_RELEASE, spare, 0, 0);
            self.sync();
        }
        self.unbind_spare(false);
    }

    /// Puts the spare keycode back to nothing.
    ///
    /// Called at the end of a `Text` and again from every teardown, because a keymap
    /// change outlives the client that made it (see [`Backend::spare_bound`]).
    ///
    /// `teardown` is what tells the ordinary call from the last one. A `sync` here is a
    /// `wait_for_reply` with no timeout, and there is no timeout to give it: a server that is
    /// alive but WEDGED (a client holding `GrabServer` and then hanging, which a screen locker
    /// or a window manager mid-operation can be) would hang the teardown for ever, and then
    /// `main` never exits, so the supervisor's `wait` never returns and nothing ever restarts
    /// this component. At teardown the request only has to be SENT: a flush writes it to the
    /// socket, and the server applies what it has already read whether this process is still
    /// there or not.
    fn unbind_spare(&mut self, teardown: bool) {
        if !self.spare_bound {
            return;
        }
        let Some(spare) = self.spare else {
            self.spare_bound = false;
            return;
        };
        // Restored to nothing, so the keymap is exactly what it was. The fingerprint
        // excludes this keycode, so none of this looked like a layout change.
        let empty = vec![x::NO_SYMBOL; usize::from(self.keysyms_per.max(1))];
        self.conn.send_request(&x::ChangeKeyboardMapping {
            keycode_count: 1,
            first_keycode: spare,
            keysyms_per_keycode: self.keysyms_per.max(1),
            keysyms: &empty,
        });
        self.spare_bound = false;
        if teardown {
            let _ = self.conn.flush();
        } else {
            self.sync();
        }
    }

    /// One round trip, for the ordering the Unicode path needs.
    fn sync(&self) {
        let cookie = self.conn.send_request(&x::GetInputFocus {});
        let _ = self.conn.wait_for_reply(cookie);
    }

    /// Switches the locked keyboard group, which is what a symbol resolved in another
    /// group needs, and what the engine switches back afterwards.
    fn lock_group(&mut self, group: u32) {
        if !self.ext.xkb {
            return;
        }
        let group = match group {
            0 => xkb::Group::N1,
            1 => xkb::Group::N2,
            2 => xkb::Group::N3,
            _ => xkb::Group::N4,
        };
        self.conn.send_request(&xkb::LatchLockState {
            device_spec: xkb::Id::UseCoreKbd as u32 as xkb::DeviceSpec,
            affect_mod_locks: x::ModMask::empty(),
            mod_locks: x::ModMask::empty(),
            lock_group: true,
            group_lock: group,
            affect_mod_latches: x::ModMask::empty(),
            latch_group: false,
            group_latch: 0,
        });
    }

    // ------------------------------------------------------------ the answers

    fn query_pointer(&self) -> Option<Point> {
        let cookie = self
            .conn
            .send_request(&x::QueryPointer { window: self.root });
        let reply = self.conn.wait_for_reply(cookie).ok()?;
        Some(Point {
            x: i32::from(reply.root_x()),
            y: i32::from(reply.root_y()),
        })
    }

    /// How this machine can produce `want` on its active keymap.
    fn resolve(&self, want: &Want) -> Option<Resolved> {
        match want {
            Want::Usage(usage) => self.positional(*usage),
            Want::Named(name) => {
                if let Some(keysym) = keysym_of_name(name)
                    && let Some((keycode, group, level)) = self.keymap.find(keysym, self.group)
                {
                    return Some(Resolved {
                        code: PlatformKey {
                            code: u32::from(keycode),
                            detail: 0,
                        },
                        mods: mods_for_level(level),
                        prefix: None,
                        group: self.switch_to(group),
                    });
                }
                // A name this keymap does not carry is still a POSITION every keyboard
                // has, so the second chance is the usage the name means.
                self.positional(keys::usage_of(name)?)
            }
            Want::Symbol(sym) => {
                let mut chars = sym.chars();
                let ch = chars.next()?;
                if chars.next().is_some() {
                    // More than one character is the Unicode path's business, not a
                    // key's.
                    return None;
                }
                let (keycode, group, level) = self.keymap.find(keysym_of_char(ch), self.group)?;
                Some(Resolved {
                    code: PlatformKey {
                        code: u32::from(keycode),
                        detail: 0,
                    },
                    mods: mods_for_level(level),
                    prefix: None,
                    group: self.switch_to(group),
                })
            }
        }
    }

    /// The group a resolution should ask the engine to switch to, or `None` when it
    /// should not ask at all.
    ///
    /// `None` when it is the active group already, and `None` when this server has no
    /// XKB: without the extension there is nothing to switch WITH ([`Backend::lock_group`]
    /// returns early), so promising a switch would have the engine press the key on the
    /// wrong group and type the wrong character rather than fall through to the Unicode
    /// path.
    fn switch_to(&self, group: u32) -> Option<u32> {
        (self.ext.xkb && group != self.group && group < 4).then_some(group)
    }

    /// The physical key a usage means, checked against the keymap so a keycode the
    /// server does not have is `None` rather than a stroke that produces nothing.
    fn positional(&self, usage: u32) -> Option<Resolved> {
        let keycode = keycode_of(usage)?;
        let entry = self.keymap.entry(keycode)?;
        // A keycode with no keysym at all is not a key on this keyboard. A MODIFIER is
        // the exception that proves the rule: it has a keysym (`Shift_L`) and produces
        // no character, which is why the test is for the keysym and not for the text.
        if entry.sym(0, 0).is_none() && entry.sym(self.group, 0).is_none() {
            return None;
        }
        Some(Resolved {
            code: PlatformKey {
                code: u32::from(keycode),
                // Nothing to carry: an X keycode is the whole identity of a physical
                // key, and the group has its own field on `Resolved` precisely so it
                // is not folded in here (a layout change must not make a held key
                // compare unequal to its own release).
                detail: 0,
            },
            mods: 0,
            prefix: None,
            group: None,
        })
    }

    /// This machine's monitors, from RandR.
    fn read_monitors(&mut self) -> Vec<Monitor> {
        if !self.ext.randr {
            return Vec::new();
        }
        let cookie = self
            .conn
            .send_request(&randr::GetScreenResourcesCurrent { window: self.root });
        let Ok(resources) = self.conn.wait_for_reply(cookie) else {
            return Vec::new();
        };
        let config = resources.config_timestamp();
        let primary = {
            let cookie = self
                .conn
                .send_request(&randr::GetOutputPrimary { window: self.root });
            self.conn.wait_for_reply(cookie).ok().map(|r| r.output())
        };
        let edid = self.intern("EDID");
        let mut out: Vec<Monitor> = Vec::new();
        let mut stable = true;
        for output in resources.outputs() {
            let cookie = self.conn.send_request(&randr::GetOutputInfo {
                output: *output,
                config_timestamp: config,
            });
            let Ok(info) = self.conn.wait_for_reply(cookie) else {
                continue;
            };
            if info.connection() != randr::Connection::Connected {
                continue;
            }
            let crtc = info.crtc();
            if crtc.resource_id() == 0 {
                // Connected and not driven by any CRTC: a screen that is plugged in
                // and switched off. It has no place on the plane until it does.
                continue;
            }
            let cookie = self.conn.send_request(&randr::GetCrtcInfo {
                crtc,
                config_timestamp: config,
            });
            let Ok(geometry) = self.conn.wait_for_reply(cookie) else {
                continue;
            };
            if geometry.width() == 0 || geometry.height() == 0 {
                continue;
            }
            let name = String::from_utf8_lossy(info.name()).to_string();
            let fingerprint = edid.and_then(|atom| self.output_edid(*output, atom));
            let id = match &fingerprint {
                Some(hash) => format!("x11:edid:{name}:{hash}"),
                None => {
                    // An identity that is the OUTPUT SLOT and not the screen: unplug
                    // two and plug them back the other way round and they have
                    // swapped. Said out loud through `monitors_stable` rather than
                    // letting the plane rearrange itself.
                    stable = false;
                    format!("x11:out:{name}")
                }
            };
            out.push(Monitor {
                id,
                name,
                w: i32::from(geometry.width()),
                h: i32::from(geometry.height()),
                x: i32::from(geometry.x()),
                y: i32::from(geometry.y()),
                // X11 has no per-monitor scale to report (truth 6).
                scale: 1000,
                primary: primary == Some(*output),
            });
        }
        // Exactly one primary, whatever RandR said: the plane needs a machine's own
        // arrangement to have one anchor, and a server with no primary set at all
        // would give it none.
        if !out.is_empty() && !out.iter().any(|m| m.primary) {
            out[0].primary = true;
        }
        if stable != self.monitors_stable {
            self.monitors_stable = stable;
            self.publish_caps();
            // Nothing else would tell the engine: it reads the capabilities BEFORE it asks
            // for the monitors, so a change discovered here would otherwise wait for the
            // next unrelated reason to re-read them.
            self.emit(BackendEvent::CapabilitiesChanged);
        }
        out
    }

    /// A hash of an output's EDID, which is the only thing about a monitor on X11 that
    /// survives being unplugged.
    fn output_edid(&self, output: randr::Output, property: x::Atom) -> Option<String> {
        let cookie = self.conn.send_request(&randr::GetOutputProperty {
            output,
            property,
            r#type: x::ATOM_ANY,
            long_offset: 0,
            // 512 bytes covers a base EDID block and its extensions; a longer read
            // would be a monitor describing itself at length and nothing this needs.
            long_length: 128,
            delete: false,
            pending: false,
        });
        let reply = self.conn.wait_for_reply(cookie).ok()?;
        if reply.format() != 8 {
            return None;
        }
        let bytes = reply.data::<u8>();
        if bytes.len() < 16 {
            return None;
        }
        Some(hex::encode(&blake3::hash(bytes).as_bytes()[..8]))
    }

    fn intern(&self, name: &str) -> Option<x::Atom> {
        let cookie = self.conn.send_request(&x::InternAtom {
            only_if_exists: true,
            name: name.as_bytes(),
        });
        let reply = self.conn.wait_for_reply(cookie).ok()?;
        let atom = reply.atom();
        (atom != x::ATOM_NONE).then_some(atom)
    }

    // ------------------------------------------------------------- the keymap

    /// Reads the whole keyboard, from XKB where it exists and from the core protocol
    /// where it does not (truth 4).
    fn reload_keymap(&mut self) {
        // The spare keycode FIRST, and the order is not cosmetic: the fingerprint
        // excludes it, so it has to be known before the first fingerprint is computed.
        // With the two the other way round, the first fingerprint included the spare's
        // empty keysyms and every later one excluded them, so the first character this
        // backend typed through the Unicode path announced a layout change and the
        // engine let go of every key a person was holding, in the middle of a word.
        self.refresh_spare();
        let keymap = if self.ext.xkb {
            self.read_keymap_xkb()
        } else {
            None
        };
        let keymap = keymap
            .or_else(|| self.read_keymap_core())
            .unwrap_or_default();
        let changed = keymap.fingerprint != self.keymap.fingerprint;
        self.keymap = keymap;
        if changed {
            self.announce_layout();
        }
    }

    fn read_keymap_xkb(&self) -> Option<Keymap> {
        let cookie = self.conn.send_request(&xkb::GetMap {
            device_spec: xkb::Id::UseCoreKbd as u32 as xkb::DeviceSpec,
            full: xkb::MapPart::KEY_SYMS,
            partial: xkb::MapPart::empty(),
            first_type: 0,
            n_types: 0,
            first_key_sym: 0,
            n_key_syms: 0,
            first_key_action: 0,
            n_key_actions: 0,
            first_key_behavior: 0,
            n_key_behaviors: 0,
            virtual_mods: xkb::VMod::empty(),
            first_key_explicit: 0,
            n_key_explicit: 0,
            first_mod_map_key: 0,
            n_mod_map_keys: 0,
            first_v_mod_map_key: 0,
            n_v_mod_map_keys: 0,
        });
        let reply = self.conn.wait_for_reply(cookie).ok()?;
        let first = reply.first_key_sym();
        for part in reply.map() {
            if let xkb::GetMapReplyMap::KeySyms(syms) = part {
                let entries: Vec<KeyEntry> = syms
                    .iter()
                    .map(|km| KeyEntry {
                        // The low four bits of `group_info` are the number of groups;
                        // the top two are what to do with a group index out of range,
                        // which nothing here ever produces.
                        groups: km.group_info() & 0x0F,
                        width: km.width(),
                        syms: km.syms().to_vec(),
                    })
                    .collect();
                return Some(self.fingerprinted(first, entries));
            }
        }
        None
    }

    /// The core protocol's answer, which is ambiguous about groups and levels
    /// (truth 4) and is what a server without XKB leaves.
    fn read_keymap_core(&self) -> Option<Keymap> {
        let setup = self.conn.get_setup();
        let first = setup.min_keycode();
        let count = setup.max_keycode().saturating_sub(first).saturating_add(1);
        let cookie = self.conn.send_request(&x::GetKeyboardMapping {
            first_keycode: first,
            count,
        });
        let reply = self.conn.wait_for_reply(cookie).ok()?;
        let per = usize::from(reply.keysyms_per_keycode().max(1));
        let entries: Vec<KeyEntry> = reply
            .keysyms()
            .chunks(per)
            .map(|chunk| KeyEntry {
                // ONE group of however many levels, and that is a deliberate choice
                // rather than the documented reading. The core protocol says the first
                // four keysyms are two groups of two levels, and it gives nothing that
                // tells that apart from one group of four (truth 4). Claiming two groups
                // is the dangerous half of the ambiguity: a resolution would answer
                // "that character is on group 2 level 1", the engine would switch group
                // and press the key with NO modifier, and the character produced would be
                // the unshifted one. Claiming one group of four levels makes the same key
                // resolve as "level 3", which is AltGr, which is what a four level key
                // actually is on every layout `xkeyboard-config` produces. And a server
                // with no XKB cannot switch groups anyway, so the group half was never
                // reachable.
                groups: 1,
                width: (per.min(4)) as u8,
                syms: chunk.to_vec(),
            })
            .collect();
        Some(self.fingerprinted(first, entries))
    }

    /// Wraps a keysym table in an identity, excluding the spare keycode (truth 5).
    fn fingerprinted(&self, first: u8, entries: Vec<KeyEntry>) -> Keymap {
        let mut hasher = blake3::Hasher::new();
        // The SHAPE as well as the keysyms, and truth 4 is why: one group of four levels
        // and two groups of two are the same four keysyms with different meanings, so a
        // fingerprint over the keysyms alone calls that keymap change no change at all,
        // keeps a resolution cache whose levels now mean something else, and never
        // releases what is held. The first keycode is in there too, for a
        // `NewKeyboardNotify` that shifts the range without touching a symbol.
        hasher.update(&[first]);
        for (index, entry) in entries.iter().enumerate() {
            let keycode = first.checked_add(index as u8);
            if keycode.is_none() || keycode == self.spare {
                continue;
            }
            hasher.update(&[entry.groups, entry.width]);
            for sym in &entry.syms {
                hasher.update(&sym.to_be_bytes());
            }
        }
        Keymap {
            first,
            entries,
            fingerprint: hex::encode(&hasher.finalize().as_bytes()[..8]),
        }
    }

    /// Finds a keycode nothing is bound to, for the Unicode path, and reads how many
    /// keysyms a keycode carries on this server.
    ///
    /// The one already chosen is KEPT while it is still empty, and only replaced when a
    /// keymap change has bound it. That stability is what the fingerprint rests on: a
    /// spare that moved between two reads would make the two fingerprints exclude
    /// different keycodes and every keymap change would look like a bigger one.
    ///
    /// A new one is the HIGHEST empty keycode, as far as possible from anything a
    /// keyboard has.
    fn refresh_spare(&mut self) {
        let first = self.conn.get_setup().min_keycode();
        let count = self
            .conn
            .get_setup()
            .max_keycode()
            .saturating_sub(first)
            .saturating_add(1);
        let cookie = self.conn.send_request(&x::GetKeyboardMapping {
            first_keycode: first,
            count,
        });
        let Ok(reply) = self.conn.wait_for_reply(cookie) else {
            self.spare = None;
            // Republished, or the capabilities would keep claiming `unicode` while
            // `type_text` silently dropped every character.
            self.publish_caps();
            return;
        };
        self.keysyms_per = reply.keysyms_per_keycode().max(1);
        let per = usize::from(self.keysyms_per);
        let empty = |keycode: u8| -> bool {
            let Some(index) = keycode.checked_sub(first) else {
                return false;
            };
            reply
                .keysyms()
                .chunks(per)
                .nth(usize::from(index))
                .is_some_and(|chunk| chunk.iter().all(|sym| *sym == x::NO_SYMBOL))
        };
        if self.spare.is_some_and(empty) {
            return;
        }
        let mut found = None;
        for (index, chunk) in reply.keysyms().chunks(per).enumerate() {
            if chunk.iter().all(|sym| *sym == x::NO_SYMBOL) {
                found = first.checked_add(index as u8);
            }
        }
        self.spare = found;
        self.publish_caps();
    }

    /// The active group and the modifiers the server believes are held.
    fn read_state(&mut self) {
        if !self.ext.xkb {
            return;
        }
        let cookie = self.conn.send_request(&xkb::GetState {
            device_spec: xkb::Id::UseCoreKbd as u32 as xkb::DeviceSpec,
        });
        let Ok(state) = self.conn.wait_for_reply(cookie) else {
            return;
        };
        self.group = state.group() as u32;
        // The two locks the core protocol names, which are the two a keyboard has:
        // Lock is Caps and Mod2 is Num. Scroll Lock has no standard modifier, so it is
        // tracked from the captured stream alone.
        let locked = state.locked_mods();
        let mut mods = self.mods & keys::mods::SCROLL;
        if locked.contains(x::ModMask::LOCK) {
            mods |= keys::mods::CAPS;
        }
        if locked.contains(x::ModMask::N2) {
            mods |= keys::mods::NUM;
        }
        let held = state.mods();
        if held.contains(x::ModMask::SHIFT) {
            mods |= keys::mods::SHIFT;
        }
        if held.contains(x::ModMask::CONTROL) {
            mods |= keys::mods::CTRL;
        }
        // Mod1 is Alt and Mod4 is Super on every layout `xkeyboard-config` produces,
        // and Mod5 carries `ISO_Level3_Shift`, which IS AltGr. Read from the fixed
        // indices rather than from the modifier map, because the mapping says which
        // KEY is on which modifier and this is a question about the modifier.
        if held.contains(x::ModMask::N1) {
            mods |= keys::mods::ALT;
        }
        if held.contains(x::ModMask::N4) {
            mods |= keys::mods::META;
        }
        if held.contains(x::ModMask::N5) {
            mods |= keys::mods::ALTGR;
        }
        self.mods = mods;
        // Announced from here as well as from `reload_keymap`, because this is the other
        // place the group can change: capture starting again after a spell of Off is the
        // one moment the server's state is worth re-reading, and a group that moved in
        // between has to reach the engine or its cached resolutions are about the wrong
        // layout. `announce_layout` says nothing when the identity has not moved.
        self.announce_layout();
    }

    fn announce_layout(&mut self) {
        if self.keymap.fingerprint.is_empty() {
            // No keymap has been read yet, so there is nothing to name. It happens once,
            // at construction, where the STATE is read before the keymap (so the first
            // identity carries the right group) and this is what keeps that from
            // announcing a layout with no fingerprint and then announcing the real one a
            // moment later. Two `LayoutChanged` at start would cost the engine two
            // relearns and would tell a reader of the log that something changed when
            // nothing had.
            return;
        }
        let layout = format!("x11:{}:{}", self.group, self.keymap.fingerprint);
        if layout == self.layout {
            return;
        }
        self.layout = layout.clone();
        let group = self.group;
        self.emit(BackendEvent::LayoutChanged { layout, group });
    }

    // ------------------------------------------------------------ the capabilities

    fn publish_caps(&self) {
        let caps = self.compute_caps();
        match self.caps.lock() {
            Ok(mut slot) => *slot = caps,
            Err(p) => *p.into_inner() = caps,
        }
    }

    fn compute_caps(&self) -> Capabilities {
        let inject = self.ext.test;
        let observe = self.ext.xi;
        let problem = if !inject && !observe {
            Some(Problem::NoBackend)
        } else if !self.monitors_stable {
            Some(Problem::MonitorsUnstable)
        } else {
            None
        };
        Capabilities {
            capture: observe,
            // Raw events can be observed but not consumed; swallowing is the device
            // grab, which is the same extension.
            swallow: observe,
            // The two part promise: the pin is a grab plus a warp to an anchor, and
            // the OS native relative source is the raw valuators. Both are XInput2,
            // and neither works without the ability to warp, which is core.
            confine: observe,
            warp: true,
            inject_keys: inject,
            inject_pointer: inject,
            // No Unicode injection on X11 except through a keycode nothing is bound to
            // (truth 5), so this is false on a keyboard with no spare keycode, and the
            // engine then reports `UNRESOLVED` for a character this layout cannot
            // produce rather than typing the wrong one.
            unicode: inject && self.spare.is_some(),
            monitors_stable: self.monitors_stable,
            problem,
        }
    }

    /// The last thing this loop does, whatever ended it, and the moment a keyboard is
    /// left working or left dead.
    fn release_everything(&mut self) {
        self.mode = CaptureMode::Off;
        self.ungrab();
        self.select_raw(false);
        self.confine = None;
        // A fraction of a notch left over from this session must not spend itself as a
        // phantom scroll in the next one.
        self.wheel_x = 0;
        self.wheel_y = 0;
        // The one thing the server will not undo for us.
        self.unbind_spare(true);
        // And the commands nobody will ever answer: an answering downcall's reply
        // travels INSIDE its queued command, so a `monitors` or a `resolve` pushed after
        // the last turn would leave its sender in the queue and its caller awaiting a
        // future that can never resolve. Dropping them here resolves each one to the
        // neutral answer its own documentation promises.
        // One `lock`, not two. The `if let Ok(..) else if let Err(..)` this replaces took the
        // lock a second time in the `else`, which is a self deadlock on any edition that keeps
        // the scrutinee's temporary alive across the arm: it happened to be safe under this
        // crate's edition and was not something to leave resting on that.
        match self.cmds.lock() {
            Ok(mut queue) => queue.clear(),
            Err(p) => p.into_inner().clear(),
        }
        let _ = self.conn.flush();
    }
}

impl Drop for Backend {
    /// The safety net at thread death. It is thinner than it looks and that is the
    /// good news about this platform: the X server drops every grab, every XFixes
    /// cursor hide and every event selection of a client whose connection closes, so a
    /// process that dies here cannot leave a keyboard swallowed or a pointer hidden.
    /// This is the clean path being tidy, not the guard.
    fn drop(&mut self) {
        self.release_everything();
        let _ = self.conn.flush();
    }
}

/// Owns the pinned [`Backend`]; `run` is the blocking main-thread pump and its return
/// value is the process exit code.
pub struct X11Loop {
    backend: Backend,
}

impl X11Loop {
    pub fn run(mut self) -> i32 {
        self.backend.run()
    }
}

fn clamp_i16(value: i32) -> i16 {
    value.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

/// The X button a dialect button number means. X counts its own back and forward as 8
/// and 9, and 4 to 7 are the wheel, which is why the dialect's 4 and 5 are not X's.
fn x_button(button: u8) -> Option<u8> {
    match button {
        1..=3 => Some(button),
        4 => Some(8),
        5 => Some(9),
        _ => None,
    }
}

/// The relative movement a raw motion event carries, in 32nds of a pixel.
///
/// The first two valuators of a pointer are x and y, and `valuator_mask` says which
/// of them this event actually carries: a device that moved only vertically sets one
/// bit, and reading `axisvalues` positionally without the mask would read the y
/// movement as an x movement. `axisvalues_raw` rather than `axisvalues` because the
/// former is the device's own numbers before any transformation the server applies
/// (truth 2).
fn raw_delta(ev: &xinput::RawMotionEvent) -> (i64, i64) {
    let mask = ev.valuator_mask();
    let values = ev.axisvalues_raw();
    let present = |axis: usize| mask.get(axis / 32).copied().unwrap_or(0) & (1 << (axis % 32)) != 0;
    // A valuator is a 32.32 fixed point number, and the fraction is kept rather than
    // truncated: a high resolution device (a touchpad, a mouse reporting at 1000 Hz)
    // sends a fraction of a pixel per event, and rounding each one to an integer would
    // report zero for ever, so the pointer would not move at all however far the hand
    // went. The integral part is signed and the fraction is the unsigned 32nds below
    // it, so half a pixel backwards is integral -1 with frac 0x80000000.
    let fixed = |v: &xinput::Fp3232| (i64::from(v.integral) << 32) | i64::from(v.frac);
    // The values are packed: one entry per axis PRESENT, in axis order. So the index
    // of axis one depends on whether axis zero is there, which is why this counts
    // rather than indexing.
    let mut next = 0usize;
    let mut dx = 0i64;
    let mut dy = 0i64;
    if present(0) {
        dx = values.first().map_or(0, fixed);
        next += 1;
    }
    if present(1) {
        dy = values.get(next).map_or(0, fixed);
    }
    (dx, dy)
}

/// One fixed point accumulator as whole pixels, with the remainder left behind.
fn spend_pixels(accumulator: &mut i64) -> i32 {
    let whole = *accumulator >> 32;
    *accumulator -= whole << 32;
    whole.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

// ------------------------------------------------------------------ the build

/// Builds the X11 backend, or says why it could not.
///
/// # The platform half of the crash guard, and why it is short here
///
/// A grab, an XFixes cursor hide and a raw event selection all belong to the CLIENT
/// that made them, and the X server undoes every one of them when that client's
/// connection closes. So a component that dies holding this machine's keyboard cannot
/// leave it swallowed: the socket closing is the release. That is a property of the
/// protocol rather than of this code, and the live suite proves it by killing a process
/// mid-grab rather than by asserting it.
///
/// # The one thing this function CANNOT clear, and the honest size of it
///
/// The keymap is the exception, and it is the exception because it is the one piece of
/// state the server keeps for the whole session no matter who made it (see
/// [`Backend::spare_bound`]). A run that dies while the Unicode path has the spare
/// keycode bound leaves that keysym on that keycode for every client until the X
/// session ends, and the next start cannot find it: `refresh_spare` chooses a keycode
/// whose entry is entirely `NO_SYMBOL`, so a keycode a predecessor left BOUND is by
/// construction never the one picked and never the one cleared.
///
/// Nothing here fixes that, deliberately, and this is what it costs: the window is the
/// few milliseconds of one `Text` injection, every ordinary teardown (including an
/// unwinding panic, through `Drop`) does unbind it, and what is left behind is one
/// keysym on a keycode no physical key produces, so nothing types it and nothing else
/// breaks. Clearing it properly means writing the chosen keycode down where the next
/// start can read it, which means the state directory reaching this backend, which the
/// seam does not carry today. It is on the deferred list rather than guessed at here.
///
/// What is NOT covered either, and is the engine's business rather than this backend's:
/// the keys a dead run left pressed through XTEST. Those are in `held.json` and are
/// released at the next start.
pub fn create() -> Result<crate::os::Created, Unsupported> {
    let (conn, screen_num) = xcb::Connection::connect_with_extensions(
        None,
        // Nothing is mandatory: each extension's absence narrows what this backend
        // says it can do, and only the connection itself is required.
        &[],
        &[
            xcb::Extension::Test,
            xcb::Extension::Input,
            xcb::Extension::Xkb,
            xcb::Extension::RandR,
            xcb::Extension::XFixes,
        ],
    )
    .map_err(|e| {
        warn(&format!("no X connection: {e:?}"));
        Unsupported(Problem::NoBackend)
    })?;

    let root = conn
        .get_setup()
        .roots()
        .nth(screen_num.max(0) as usize)
        .ok_or(Unsupported(Problem::NoBackend))?
        .root();

    let mut ext = Ext::default();
    let active: Vec<xcb::Extension> = conn.active_extensions().collect();

    // Every extension needs its own version handshake before any of its requests is
    // legal, and a handshake that fails is the extension being absent as far as this
    // backend is concerned.
    if active.contains(&xcb::Extension::Test) {
        let cookie = conn.send_request(&xtest::GetVersion {
            major_version: 2,
            minor_version: 2,
        });
        ext.test = conn.wait_for_reply(cookie).is_ok();
    }
    if active.contains(&xcb::Extension::Input) {
        let cookie = conn.send_request(&xinput::XiQueryVersion {
            major_version: 2,
            minor_version: 2,
        });
        ext.xi = conn
            .wait_for_reply(cookie)
            .is_ok_and(|reply| reply.major_version() >= 2);
    }
    if active.contains(&xcb::Extension::Xkb) {
        let cookie = conn.send_request(&xkb::UseExtension {
            wanted_major: 1,
            wanted_minor: 0,
        });
        ext.xkb = conn
            .wait_for_reply(cookie)
            .is_ok_and(|reply| reply.supported());
    }
    if active.contains(&xcb::Extension::RandR) {
        let cookie = conn.send_request(&randr::QueryVersion {
            major_version: 1,
            minor_version: 3,
        });
        ext.randr = conn.wait_for_reply(cookie).is_ok();
    }
    if active.contains(&xcb::Extension::XFixes) {
        let cookie = conn.send_request(&xfixes::QueryVersion {
            client_major_version: 5,
            client_minor_version: 0,
        });
        ext.xfixes = conn.wait_for_reply(cookie).is_ok();
    }
    if !ext.test && !ext.xi {
        warn("this X server has neither XTEST nor XInput2: nothing here can type or observe");
    }

    // The keyboard's changes, so a layout switch is noticed rather than polled for.
    if ext.xkb {
        conn.send_request(&xkb::SelectEvents {
            device_spec: xkb::Id::UseCoreKbd as u32 as xkb::DeviceSpec,
            // `MAP_NOTIFY` belongs in `affect_which` too: the `affect_map` and `map`
            // fields below are only consulted for the event types named here, so without
            // it the map notify was never selected and its arm in `on_event` was dead
            // code. Nothing was lost while it was (the core `MappingNotify` reaches every
            // client on any keysym change and is handled), which is exactly why it went
            // unnoticed.
            affect_which: xkb::EventType::NEW_KEYBOARD_NOTIFY
                | xkb::EventType::STATE_NOTIFY
                | xkb::EventType::MAP_NOTIFY,
            clear: xkb::EventType::empty(),
            select_all: xkb::EventType::empty(),
            affect_map: xkb::MapPart::KEY_SYMS,
            map: xkb::MapPart::KEY_SYMS,
            // Sorted and distinct, which the binding asserts on: new keyboard first,
            // then state. A map notify is selected through `affect_map` above and has
            // no details entry of its own.
            details: &[
                xkb::SelectEventsDetails::NewKeyboardNotify {
                    affect_new_keyboard: xkb::NknDetail::KEYCODES | xkb::NknDetail::GEOMETRY,
                    new_keyboard_details: xkb::NknDetail::KEYCODES | xkb::NknDetail::GEOMETRY,
                },
                xkb::SelectEventsDetails::StateNotify {
                    affect_state: xkb::StatePart::GROUP_STATE | xkb::StatePart::MODIFIER_STATE,
                    state_details: xkb::StatePart::GROUP_STATE | xkb::StatePart::MODIFIER_STATE,
                },
            ],
        });
    }
    // The screens' changes, for the same reason.
    if ext.randr {
        conn.send_request(&randr::SelectInput {
            window: root,
            enable: randr::NotifyMask::SCREEN_CHANGE | randr::NotifyMask::CRTC_CHANGE,
        });
    }
    if let Err(e) = conn.flush() {
        warn(&format!("flush during setup failed: {e:?}"));
        return Err(Unsupported(Problem::NoBackend));
    }

    let poll = Poll::new().map_err(|e| {
        warn(&format!("mio Poll: {e}"));
        Unsupported(Problem::NoBackend)
    })?;
    poll.registry()
        .register(
            &mut SourceFd(&conn.as_raw_fd()),
            XCB_TOKEN,
            Interest::READABLE,
        )
        .map_err(|e| {
            warn(&format!("register the X fd: {e}"));
            Unsupported(Problem::NoBackend)
        })?;
    let waker = Arc::new(Waker::new(poll.registry(), CMD_TOKEN).map_err(|e| {
        warn(&format!("mio Waker: {e}"));
        Unsupported(Problem::NoBackend)
    })?);

    let cmds: Arc<Mutex<VecDeque<Cmd>>> = Arc::new(Mutex::new(VecDeque::new()));
    let (events_tx, backend_events) = mpsc::channel(BACKEND_EVENT_CAPACITY);
    let caps = Arc::new(Mutex::new(Capabilities::none(Some(Problem::NoBackend))));
    let mut backend = Backend {
        conn,
        root,
        ext,
        poll,
        cmds: Arc::clone(&cmds),
        events_tx,
        caps: Arc::clone(&caps),
        keymap: Keymap::default(),
        keysyms_per: 1,
        spare: None,
        spare_bound: false,
        group: 0,
        mods: 0,
        layout: String::new(),
        mode: CaptureMode::Off,
        grabbed: false,
        hidden: false,
        confine: None,
        anchor: Point { x: 0, y: 0 },
        pending_dx: 0,
        pending_dy: 0,
        wheel_x: 0,
        wheel_y: 0,
        monitors_stable: true,
        deferred: Vec::new(),
        shutdown: false,
        exit_code: None,
        dropped: 0,
        errors: 0,
        engine_gone: false,
        last_core: None,
        gone_seen: false,
    };
    // Read before the handle is handed over, so the first `capabilities()` the engine
    // asks for is the truth rather than the pessimistic placeholder above.
    //
    // The STATE first, and the order is not cosmetic. `reload_keymap` announces the
    // layout, and a layout identity carries the group; announcing before the group was
    // read told the engine it was in group 0 whatever group its owner was actually in.
    // What that cost: the engine switches BACK to the group it believes the machine was
    // in, so a person typing Russian had their keyboard left in the Latin group by the
    // very code written to prevent exactly that. And it hid the repair, because the real
    // switch to group 0 later produced the identity that had already been announced, so
    // `announce_layout` said nothing and the resolution cache was never dropped.
    backend.read_state();
    backend.reload_keymap();
    // The screens too, for the same reason: `monitors_stable` is only computed here, and
    // the engine reads the capabilities BEFORE it asks for the monitors, so without this
    // the first answer claimed every screen had a stable identity.
    let _ = backend.read_monitors();
    backend.publish_caps();

    let handle = X11Backend { cmds, waker, caps };
    Ok(crate::os::Created {
        handle: crate::os::Backend::X11(handle),
        backend_events,
        event_loop: crate::os::EventLoop::X11(Box::new(X11Loop { backend })),
    })
}

// ---------------------------------------------------------- the keysym table

/// Every keysym that is neither Latin-1 nor a `0x01000000` code point, paired with the
/// character it produces.
///
/// Generated from the `U+` comments in the system's `keysymdef.h`, which is the
/// canonical source and the one libX11's own table is built from; the two algorithmic
/// ranges are excluded because [`text_of_keysym`] handles them without a lookup. It
/// covers Latin-2 to Latin-9, Greek, Cyrillic, Hebrew, Arabic, Thai, Korean and the
/// punctuation blocks, which is what a keymap on a machine whose language is not
/// English is written in.
#[rustfmt::skip]
const KEYSYM_UNICODE: &[(u32, char)] = &[
    (0x01a1, '\u{0104}'), // XK_Aogonek
    (0x01a2, '\u{02d8}'), // XK_breve
    (0x01a3, '\u{0141}'), // XK_Lstroke
    (0x01a5, '\u{013d}'), // XK_Lcaron
    (0x01a6, '\u{015a}'), // XK_Sacute
    (0x01a9, '\u{0160}'), // XK_Scaron
    (0x01aa, '\u{015e}'), // XK_Scedilla
    (0x01ab, '\u{0164}'), // XK_Tcaron
    (0x01ac, '\u{0179}'), // XK_Zacute
    (0x01ae, '\u{017d}'), // XK_Zcaron
    (0x01af, '\u{017b}'), // XK_Zabovedot
    (0x01b1, '\u{0105}'), // XK_aogonek
    (0x01b2, '\u{02db}'), // XK_ogonek
    (0x01b3, '\u{0142}'), // XK_lstroke
    (0x01b5, '\u{013e}'), // XK_lcaron
    (0x01b6, '\u{015b}'), // XK_sacute
    (0x01b7, '\u{02c7}'), // XK_caron
    (0x01b9, '\u{0161}'), // XK_scaron
    (0x01ba, '\u{015f}'), // XK_scedilla
    (0x01bb, '\u{0165}'), // XK_tcaron
    (0x01bc, '\u{017a}'), // XK_zacute
    (0x01bd, '\u{02dd}'), // XK_doubleacute
    (0x01be, '\u{017e}'), // XK_zcaron
    (0x01bf, '\u{017c}'), // XK_zabovedot
    (0x01c0, '\u{0154}'), // XK_Racute
    (0x01c3, '\u{0102}'), // XK_Abreve
    (0x01c5, '\u{0139}'), // XK_Lacute
    (0x01c6, '\u{0106}'), // XK_Cacute
    (0x01c8, '\u{010c}'), // XK_Ccaron
    (0x01ca, '\u{0118}'), // XK_Eogonek
    (0x01cc, '\u{011a}'), // XK_Ecaron
    (0x01cf, '\u{010e}'), // XK_Dcaron
    (0x01d0, '\u{0110}'), // XK_Dstroke
    (0x01d1, '\u{0143}'), // XK_Nacute
    (0x01d2, '\u{0147}'), // XK_Ncaron
    (0x01d5, '\u{0150}'), // XK_Odoubleacute
    (0x01d8, '\u{0158}'), // XK_Rcaron
    (0x01d9, '\u{016e}'), // XK_Uring
    (0x01db, '\u{0170}'), // XK_Udoubleacute
    (0x01de, '\u{0162}'), // XK_Tcedilla
    (0x01e0, '\u{0155}'), // XK_racute
    (0x01e3, '\u{0103}'), // XK_abreve
    (0x01e5, '\u{013a}'), // XK_lacute
    (0x01e6, '\u{0107}'), // XK_cacute
    (0x01e8, '\u{010d}'), // XK_ccaron
    (0x01ea, '\u{0119}'), // XK_eogonek
    (0x01ec, '\u{011b}'), // XK_ecaron
    (0x01ef, '\u{010f}'), // XK_dcaron
    (0x01f0, '\u{0111}'), // XK_dstroke
    (0x01f1, '\u{0144}'), // XK_nacute
    (0x01f2, '\u{0148}'), // XK_ncaron
    (0x01f5, '\u{0151}'), // XK_odoubleacute
    (0x01f8, '\u{0159}'), // XK_rcaron
    (0x01f9, '\u{016f}'), // XK_uring
    (0x01fb, '\u{0171}'), // XK_udoubleacute
    (0x01fe, '\u{0163}'), // XK_tcedilla
    (0x01ff, '\u{02d9}'), // XK_abovedot
    (0x02a1, '\u{0126}'), // XK_Hstroke
    (0x02a6, '\u{0124}'), // XK_Hcircumflex
    (0x02a9, '\u{0130}'), // XK_Iabovedot
    (0x02ab, '\u{011e}'), // XK_Gbreve
    (0x02ac, '\u{0134}'), // XK_Jcircumflex
    (0x02b1, '\u{0127}'), // XK_hstroke
    (0x02b6, '\u{0125}'), // XK_hcircumflex
    (0x02b9, '\u{0131}'), // XK_idotless
    (0x02bb, '\u{011f}'), // XK_gbreve
    (0x02bc, '\u{0135}'), // XK_jcircumflex
    (0x02c5, '\u{010a}'), // XK_Cabovedot
    (0x02c6, '\u{0108}'), // XK_Ccircumflex
    (0x02d5, '\u{0120}'), // XK_Gabovedot
    (0x02d8, '\u{011c}'), // XK_Gcircumflex
    (0x02dd, '\u{016c}'), // XK_Ubreve
    (0x02de, '\u{015c}'), // XK_Scircumflex
    (0x02e5, '\u{010b}'), // XK_cabovedot
    (0x02e6, '\u{0109}'), // XK_ccircumflex
    (0x02f5, '\u{0121}'), // XK_gabovedot
    (0x02f8, '\u{011d}'), // XK_gcircumflex
    (0x02fd, '\u{016d}'), // XK_ubreve
    (0x02fe, '\u{015d}'), // XK_scircumflex
    (0x03a2, '\u{0138}'), // XK_kra
    (0x03a3, '\u{0156}'), // XK_Rcedilla
    (0x03a5, '\u{0128}'), // XK_Itilde
    (0x03a6, '\u{013b}'), // XK_Lcedilla
    (0x03aa, '\u{0112}'), // XK_Emacron
    (0x03ab, '\u{0122}'), // XK_Gcedilla
    (0x03ac, '\u{0166}'), // XK_Tslash
    (0x03b3, '\u{0157}'), // XK_rcedilla
    (0x03b5, '\u{0129}'), // XK_itilde
    (0x03b6, '\u{013c}'), // XK_lcedilla
    (0x03ba, '\u{0113}'), // XK_emacron
    (0x03bb, '\u{0123}'), // XK_gcedilla
    (0x03bc, '\u{0167}'), // XK_tslash
    (0x03bd, '\u{014a}'), // XK_ENG
    (0x03bf, '\u{014b}'), // XK_eng
    (0x03c0, '\u{0100}'), // XK_Amacron
    (0x03c7, '\u{012e}'), // XK_Iogonek
    (0x03cc, '\u{0116}'), // XK_Eabovedot
    (0x03cf, '\u{012a}'), // XK_Imacron
    (0x03d1, '\u{0145}'), // XK_Ncedilla
    (0x03d2, '\u{014c}'), // XK_Omacron
    (0x03d3, '\u{0136}'), // XK_Kcedilla
    (0x03d9, '\u{0172}'), // XK_Uogonek
    (0x03dd, '\u{0168}'), // XK_Utilde
    (0x03de, '\u{016a}'), // XK_Umacron
    (0x03e0, '\u{0101}'), // XK_amacron
    (0x03e7, '\u{012f}'), // XK_iogonek
    (0x03ec, '\u{0117}'), // XK_eabovedot
    (0x03ef, '\u{012b}'), // XK_imacron
    (0x03f1, '\u{0146}'), // XK_ncedilla
    (0x03f2, '\u{014d}'), // XK_omacron
    (0x03f3, '\u{0137}'), // XK_kcedilla
    (0x03f9, '\u{0173}'), // XK_uogonek
    (0x03fd, '\u{0169}'), // XK_utilde
    (0x03fe, '\u{016b}'), // XK_umacron
    (0x047e, '\u{203e}'), // XK_overline
    (0x04a1, '\u{3002}'), // XK_kana_fullstop
    (0x04a2, '\u{300c}'), // XK_kana_openingbracket
    (0x04a3, '\u{300d}'), // XK_kana_closingbracket
    (0x04a4, '\u{3001}'), // XK_kana_comma
    (0x04a5, '\u{30fb}'), // XK_kana_conjunctive
    (0x04a6, '\u{30f2}'), // XK_kana_WO
    (0x04a7, '\u{30a1}'), // XK_kana_a
    (0x04a8, '\u{30a3}'), // XK_kana_i
    (0x04a9, '\u{30a5}'), // XK_kana_u
    (0x04aa, '\u{30a7}'), // XK_kana_e
    (0x04ab, '\u{30a9}'), // XK_kana_o
    (0x04ac, '\u{30e3}'), // XK_kana_ya
    (0x04ad, '\u{30e5}'), // XK_kana_yu
    (0x04ae, '\u{30e7}'), // XK_kana_yo
    (0x04af, '\u{30c3}'), // XK_kana_tsu
    (0x04b0, '\u{30fc}'), // XK_prolongedsound
    (0x04b1, '\u{30a2}'), // XK_kana_A
    (0x04b2, '\u{30a4}'), // XK_kana_I
    (0x04b3, '\u{30a6}'), // XK_kana_U
    (0x04b4, '\u{30a8}'), // XK_kana_E
    (0x04b5, '\u{30aa}'), // XK_kana_O
    (0x04b6, '\u{30ab}'), // XK_kana_KA
    (0x04b7, '\u{30ad}'), // XK_kana_KI
    (0x04b8, '\u{30af}'), // XK_kana_KU
    (0x04b9, '\u{30b1}'), // XK_kana_KE
    (0x04ba, '\u{30b3}'), // XK_kana_KO
    (0x04bb, '\u{30b5}'), // XK_kana_SA
    (0x04bc, '\u{30b7}'), // XK_kana_SHI
    (0x04bd, '\u{30b9}'), // XK_kana_SU
    (0x04be, '\u{30bb}'), // XK_kana_SE
    (0x04bf, '\u{30bd}'), // XK_kana_SO
    (0x04c0, '\u{30bf}'), // XK_kana_TA
    (0x04c1, '\u{30c1}'), // XK_kana_CHI
    (0x04c2, '\u{30c4}'), // XK_kana_TSU
    (0x04c3, '\u{30c6}'), // XK_kana_TE
    (0x04c4, '\u{30c8}'), // XK_kana_TO
    (0x04c5, '\u{30ca}'), // XK_kana_NA
    (0x04c6, '\u{30cb}'), // XK_kana_NI
    (0x04c7, '\u{30cc}'), // XK_kana_NU
    (0x04c8, '\u{30cd}'), // XK_kana_NE
    (0x04c9, '\u{30ce}'), // XK_kana_NO
    (0x04ca, '\u{30cf}'), // XK_kana_HA
    (0x04cb, '\u{30d2}'), // XK_kana_HI
    (0x04cc, '\u{30d5}'), // XK_kana_FU
    (0x04cd, '\u{30d8}'), // XK_kana_HE
    (0x04ce, '\u{30db}'), // XK_kana_HO
    (0x04cf, '\u{30de}'), // XK_kana_MA
    (0x04d0, '\u{30df}'), // XK_kana_MI
    (0x04d1, '\u{30e0}'), // XK_kana_MU
    (0x04d2, '\u{30e1}'), // XK_kana_ME
    (0x04d3, '\u{30e2}'), // XK_kana_MO
    (0x04d4, '\u{30e4}'), // XK_kana_YA
    (0x04d5, '\u{30e6}'), // XK_kana_YU
    (0x04d6, '\u{30e8}'), // XK_kana_YO
    (0x04d7, '\u{30e9}'), // XK_kana_RA
    (0x04d8, '\u{30ea}'), // XK_kana_RI
    (0x04d9, '\u{30eb}'), // XK_kana_RU
    (0x04da, '\u{30ec}'), // XK_kana_RE
    (0x04db, '\u{30ed}'), // XK_kana_RO
    (0x04dc, '\u{30ef}'), // XK_kana_WA
    (0x04dd, '\u{30f3}'), // XK_kana_N
    (0x04de, '\u{309b}'), // XK_voicedsound
    (0x04df, '\u{309c}'), // XK_semivoicedsound
    (0x05ac, '\u{060c}'), // XK_Arabic_comma
    (0x05bb, '\u{061b}'), // XK_Arabic_semicolon
    (0x05bf, '\u{061f}'), // XK_Arabic_question_mark
    (0x05c1, '\u{0621}'), // XK_Arabic_hamza
    (0x05c2, '\u{0622}'), // XK_Arabic_maddaonalef
    (0x05c3, '\u{0623}'), // XK_Arabic_hamzaonalef
    (0x05c4, '\u{0624}'), // XK_Arabic_hamzaonwaw
    (0x05c5, '\u{0625}'), // XK_Arabic_hamzaunderalef
    (0x05c6, '\u{0626}'), // XK_Arabic_hamzaonyeh
    (0x05c7, '\u{0627}'), // XK_Arabic_alef
    (0x05c8, '\u{0628}'), // XK_Arabic_beh
    (0x05c9, '\u{0629}'), // XK_Arabic_tehmarbuta
    (0x05ca, '\u{062a}'), // XK_Arabic_teh
    (0x05cb, '\u{062b}'), // XK_Arabic_theh
    (0x05cc, '\u{062c}'), // XK_Arabic_jeem
    (0x05cd, '\u{062d}'), // XK_Arabic_hah
    (0x05ce, '\u{062e}'), // XK_Arabic_khah
    (0x05cf, '\u{062f}'), // XK_Arabic_dal
    (0x05d0, '\u{0630}'), // XK_Arabic_thal
    (0x05d1, '\u{0631}'), // XK_Arabic_ra
    (0x05d2, '\u{0632}'), // XK_Arabic_zain
    (0x05d3, '\u{0633}'), // XK_Arabic_seen
    (0x05d4, '\u{0634}'), // XK_Arabic_sheen
    (0x05d5, '\u{0635}'), // XK_Arabic_sad
    (0x05d6, '\u{0636}'), // XK_Arabic_dad
    (0x05d7, '\u{0637}'), // XK_Arabic_tah
    (0x05d8, '\u{0638}'), // XK_Arabic_zah
    (0x05d9, '\u{0639}'), // XK_Arabic_ain
    (0x05da, '\u{063a}'), // XK_Arabic_ghain
    (0x05e0, '\u{0640}'), // XK_Arabic_tatweel
    (0x05e1, '\u{0641}'), // XK_Arabic_feh
    (0x05e2, '\u{0642}'), // XK_Arabic_qaf
    (0x05e3, '\u{0643}'), // XK_Arabic_kaf
    (0x05e4, '\u{0644}'), // XK_Arabic_lam
    (0x05e5, '\u{0645}'), // XK_Arabic_meem
    (0x05e6, '\u{0646}'), // XK_Arabic_noon
    (0x05e7, '\u{0647}'), // XK_Arabic_ha
    (0x05e8, '\u{0648}'), // XK_Arabic_waw
    (0x05e9, '\u{0649}'), // XK_Arabic_alefmaksura
    (0x05ea, '\u{064a}'), // XK_Arabic_yeh
    (0x05eb, '\u{064b}'), // XK_Arabic_fathatan
    (0x05ec, '\u{064c}'), // XK_Arabic_dammatan
    (0x05ed, '\u{064d}'), // XK_Arabic_kasratan
    (0x05ee, '\u{064e}'), // XK_Arabic_fatha
    (0x05ef, '\u{064f}'), // XK_Arabic_damma
    (0x05f0, '\u{0650}'), // XK_Arabic_kasra
    (0x05f1, '\u{0651}'), // XK_Arabic_shadda
    (0x05f2, '\u{0652}'), // XK_Arabic_sukun
    (0x06a1, '\u{0452}'), // XK_Serbian_dje
    (0x06a2, '\u{0453}'), // XK_Macedonia_gje
    (0x06a3, '\u{0451}'), // XK_Cyrillic_io
    (0x06a4, '\u{0454}'), // XK_Ukrainian_ie
    (0x06a5, '\u{0455}'), // XK_Macedonia_dse
    (0x06a6, '\u{0456}'), // XK_Ukrainian_i
    (0x06a7, '\u{0457}'), // XK_Ukrainian_yi
    (0x06a8, '\u{0458}'), // XK_Cyrillic_je
    (0x06a9, '\u{0459}'), // XK_Cyrillic_lje
    (0x06aa, '\u{045a}'), // XK_Cyrillic_nje
    (0x06ab, '\u{045b}'), // XK_Serbian_tshe
    (0x06ac, '\u{045c}'), // XK_Macedonia_kje
    (0x06ad, '\u{0491}'), // XK_Ukrainian_ghe_with_upturn
    (0x06ae, '\u{045e}'), // XK_Byelorussian_shortu
    (0x06af, '\u{045f}'), // XK_Cyrillic_dzhe
    (0x06b0, '\u{2116}'), // XK_numerosign
    (0x06b1, '\u{0402}'), // XK_Serbian_DJE
    (0x06b2, '\u{0403}'), // XK_Macedonia_GJE
    (0x06b3, '\u{0401}'), // XK_Cyrillic_IO
    (0x06b4, '\u{0404}'), // XK_Ukrainian_IE
    (0x06b5, '\u{0405}'), // XK_Macedonia_DSE
    (0x06b6, '\u{0406}'), // XK_Ukrainian_I
    (0x06b7, '\u{0407}'), // XK_Ukrainian_YI
    (0x06b8, '\u{0408}'), // XK_Cyrillic_JE
    (0x06b9, '\u{0409}'), // XK_Cyrillic_LJE
    (0x06ba, '\u{040a}'), // XK_Cyrillic_NJE
    (0x06bb, '\u{040b}'), // XK_Serbian_TSHE
    (0x06bc, '\u{040c}'), // XK_Macedonia_KJE
    (0x06bd, '\u{0490}'), // XK_Ukrainian_GHE_WITH_UPTURN
    (0x06be, '\u{040e}'), // XK_Byelorussian_SHORTU
    (0x06bf, '\u{040f}'), // XK_Cyrillic_DZHE
    (0x06c0, '\u{044e}'), // XK_Cyrillic_yu
    (0x06c1, '\u{0430}'), // XK_Cyrillic_a
    (0x06c2, '\u{0431}'), // XK_Cyrillic_be
    (0x06c3, '\u{0446}'), // XK_Cyrillic_tse
    (0x06c4, '\u{0434}'), // XK_Cyrillic_de
    (0x06c5, '\u{0435}'), // XK_Cyrillic_ie
    (0x06c6, '\u{0444}'), // XK_Cyrillic_ef
    (0x06c7, '\u{0433}'), // XK_Cyrillic_ghe
    (0x06c8, '\u{0445}'), // XK_Cyrillic_ha
    (0x06c9, '\u{0438}'), // XK_Cyrillic_i
    (0x06ca, '\u{0439}'), // XK_Cyrillic_shorti
    (0x06cb, '\u{043a}'), // XK_Cyrillic_ka
    (0x06cc, '\u{043b}'), // XK_Cyrillic_el
    (0x06cd, '\u{043c}'), // XK_Cyrillic_em
    (0x06ce, '\u{043d}'), // XK_Cyrillic_en
    (0x06cf, '\u{043e}'), // XK_Cyrillic_o
    (0x06d0, '\u{043f}'), // XK_Cyrillic_pe
    (0x06d1, '\u{044f}'), // XK_Cyrillic_ya
    (0x06d2, '\u{0440}'), // XK_Cyrillic_er
    (0x06d3, '\u{0441}'), // XK_Cyrillic_es
    (0x06d4, '\u{0442}'), // XK_Cyrillic_te
    (0x06d5, '\u{0443}'), // XK_Cyrillic_u
    (0x06d6, '\u{0436}'), // XK_Cyrillic_zhe
    (0x06d7, '\u{0432}'), // XK_Cyrillic_ve
    (0x06d8, '\u{044c}'), // XK_Cyrillic_softsign
    (0x06d9, '\u{044b}'), // XK_Cyrillic_yeru
    (0x06da, '\u{0437}'), // XK_Cyrillic_ze
    (0x06db, '\u{0448}'), // XK_Cyrillic_sha
    (0x06dc, '\u{044d}'), // XK_Cyrillic_e
    (0x06dd, '\u{0449}'), // XK_Cyrillic_shcha
    (0x06de, '\u{0447}'), // XK_Cyrillic_che
    (0x06df, '\u{044a}'), // XK_Cyrillic_hardsign
    (0x06e0, '\u{042e}'), // XK_Cyrillic_YU
    (0x06e1, '\u{0410}'), // XK_Cyrillic_A
    (0x06e2, '\u{0411}'), // XK_Cyrillic_BE
    (0x06e3, '\u{0426}'), // XK_Cyrillic_TSE
    (0x06e4, '\u{0414}'), // XK_Cyrillic_DE
    (0x06e5, '\u{0415}'), // XK_Cyrillic_IE
    (0x06e6, '\u{0424}'), // XK_Cyrillic_EF
    (0x06e7, '\u{0413}'), // XK_Cyrillic_GHE
    (0x06e8, '\u{0425}'), // XK_Cyrillic_HA
    (0x06e9, '\u{0418}'), // XK_Cyrillic_I
    (0x06ea, '\u{0419}'), // XK_Cyrillic_SHORTI
    (0x06eb, '\u{041a}'), // XK_Cyrillic_KA
    (0x06ec, '\u{041b}'), // XK_Cyrillic_EL
    (0x06ed, '\u{041c}'), // XK_Cyrillic_EM
    (0x06ee, '\u{041d}'), // XK_Cyrillic_EN
    (0x06ef, '\u{041e}'), // XK_Cyrillic_O
    (0x06f0, '\u{041f}'), // XK_Cyrillic_PE
    (0x06f1, '\u{042f}'), // XK_Cyrillic_YA
    (0x06f2, '\u{0420}'), // XK_Cyrillic_ER
    (0x06f3, '\u{0421}'), // XK_Cyrillic_ES
    (0x06f4, '\u{0422}'), // XK_Cyrillic_TE
    (0x06f5, '\u{0423}'), // XK_Cyrillic_U
    (0x06f6, '\u{0416}'), // XK_Cyrillic_ZHE
    (0x06f7, '\u{0412}'), // XK_Cyrillic_VE
    (0x06f8, '\u{042c}'), // XK_Cyrillic_SOFTSIGN
    (0x06f9, '\u{042b}'), // XK_Cyrillic_YERU
    (0x06fa, '\u{0417}'), // XK_Cyrillic_ZE
    (0x06fb, '\u{0428}'), // XK_Cyrillic_SHA
    (0x06fc, '\u{042d}'), // XK_Cyrillic_E
    (0x06fd, '\u{0429}'), // XK_Cyrillic_SHCHA
    (0x06fe, '\u{0427}'), // XK_Cyrillic_CHE
    (0x06ff, '\u{042a}'), // XK_Cyrillic_HARDSIGN
    (0x07a1, '\u{0386}'), // XK_Greek_ALPHAaccent
    (0x07a2, '\u{0388}'), // XK_Greek_EPSILONaccent
    (0x07a3, '\u{0389}'), // XK_Greek_ETAaccent
    (0x07a4, '\u{038a}'), // XK_Greek_IOTAaccent
    (0x07a5, '\u{03aa}'), // XK_Greek_IOTAdieresis
    (0x07a7, '\u{038c}'), // XK_Greek_OMICRONaccent
    (0x07a8, '\u{038e}'), // XK_Greek_UPSILONaccent
    (0x07a9, '\u{03ab}'), // XK_Greek_UPSILONdieresis
    (0x07ab, '\u{038f}'), // XK_Greek_OMEGAaccent
    (0x07ae, '\u{0385}'), // XK_Greek_accentdieresis
    (0x07af, '\u{2015}'), // XK_Greek_horizbar
    (0x07b1, '\u{03ac}'), // XK_Greek_alphaaccent
    (0x07b2, '\u{03ad}'), // XK_Greek_epsilonaccent
    (0x07b3, '\u{03ae}'), // XK_Greek_etaaccent
    (0x07b4, '\u{03af}'), // XK_Greek_iotaaccent
    (0x07b5, '\u{03ca}'), // XK_Greek_iotadieresis
    (0x07b6, '\u{0390}'), // XK_Greek_iotaaccentdieresis
    (0x07b7, '\u{03cc}'), // XK_Greek_omicronaccent
    (0x07b8, '\u{03cd}'), // XK_Greek_upsilonaccent
    (0x07b9, '\u{03cb}'), // XK_Greek_upsilondieresis
    (0x07ba, '\u{03b0}'), // XK_Greek_upsilonaccentdieresis
    (0x07bb, '\u{03ce}'), // XK_Greek_omegaaccent
    (0x07c1, '\u{0391}'), // XK_Greek_ALPHA
    (0x07c2, '\u{0392}'), // XK_Greek_BETA
    (0x07c3, '\u{0393}'), // XK_Greek_GAMMA
    (0x07c4, '\u{0394}'), // XK_Greek_DELTA
    (0x07c5, '\u{0395}'), // XK_Greek_EPSILON
    (0x07c6, '\u{0396}'), // XK_Greek_ZETA
    (0x07c7, '\u{0397}'), // XK_Greek_ETA
    (0x07c8, '\u{0398}'), // XK_Greek_THETA
    (0x07c9, '\u{0399}'), // XK_Greek_IOTA
    (0x07ca, '\u{039a}'), // XK_Greek_KAPPA
    (0x07cb, '\u{039b}'), // XK_Greek_LAMDA
    (0x07cc, '\u{039c}'), // XK_Greek_MU
    (0x07cd, '\u{039d}'), // XK_Greek_NU
    (0x07ce, '\u{039e}'), // XK_Greek_XI
    (0x07cf, '\u{039f}'), // XK_Greek_OMICRON
    (0x07d0, '\u{03a0}'), // XK_Greek_PI
    (0x07d1, '\u{03a1}'), // XK_Greek_RHO
    (0x07d2, '\u{03a3}'), // XK_Greek_SIGMA
    (0x07d4, '\u{03a4}'), // XK_Greek_TAU
    (0x07d5, '\u{03a5}'), // XK_Greek_UPSILON
    (0x07d6, '\u{03a6}'), // XK_Greek_PHI
    (0x07d7, '\u{03a7}'), // XK_Greek_CHI
    (0x07d8, '\u{03a8}'), // XK_Greek_PSI
    (0x07d9, '\u{03a9}'), // XK_Greek_OMEGA
    (0x07e1, '\u{03b1}'), // XK_Greek_alpha
    (0x07e2, '\u{03b2}'), // XK_Greek_beta
    (0x07e3, '\u{03b3}'), // XK_Greek_gamma
    (0x07e4, '\u{03b4}'), // XK_Greek_delta
    (0x07e5, '\u{03b5}'), // XK_Greek_epsilon
    (0x07e6, '\u{03b6}'), // XK_Greek_zeta
    (0x07e7, '\u{03b7}'), // XK_Greek_eta
    (0x07e8, '\u{03b8}'), // XK_Greek_theta
    (0x07e9, '\u{03b9}'), // XK_Greek_iota
    (0x07ea, '\u{03ba}'), // XK_Greek_kappa
    (0x07eb, '\u{03bb}'), // XK_Greek_lamda
    (0x07ec, '\u{03bc}'), // XK_Greek_mu
    (0x07ed, '\u{03bd}'), // XK_Greek_nu
    (0x07ee, '\u{03be}'), // XK_Greek_xi
    (0x07ef, '\u{03bf}'), // XK_Greek_omicron
    (0x07f0, '\u{03c0}'), // XK_Greek_pi
    (0x07f1, '\u{03c1}'), // XK_Greek_rho
    (0x07f2, '\u{03c3}'), // XK_Greek_sigma
    (0x07f3, '\u{03c2}'), // XK_Greek_finalsmallsigma
    (0x07f4, '\u{03c4}'), // XK_Greek_tau
    (0x07f5, '\u{03c5}'), // XK_Greek_upsilon
    (0x07f6, '\u{03c6}'), // XK_Greek_phi
    (0x07f7, '\u{03c7}'), // XK_Greek_chi
    (0x07f8, '\u{03c8}'), // XK_Greek_psi
    (0x07f9, '\u{03c9}'), // XK_Greek_omega
    (0x08a1, '\u{23b7}'), // XK_leftradical
    (0x08a2, '\u{250c}'), // XK_topleftradical
    (0x08a3, '\u{2500}'), // XK_horizconnector
    (0x08a4, '\u{2320}'), // XK_topintegral
    (0x08a5, '\u{2321}'), // XK_botintegral
    (0x08a6, '\u{2502}'), // XK_vertconnector
    (0x08a7, '\u{23a1}'), // XK_topleftsqbracket
    (0x08a8, '\u{23a3}'), // XK_botleftsqbracket
    (0x08a9, '\u{23a4}'), // XK_toprightsqbracket
    (0x08aa, '\u{23a6}'), // XK_botrightsqbracket
    (0x08ab, '\u{239b}'), // XK_topleftparens
    (0x08ac, '\u{239d}'), // XK_botleftparens
    (0x08ad, '\u{239e}'), // XK_toprightparens
    (0x08ae, '\u{23a0}'), // XK_botrightparens
    (0x08af, '\u{23a8}'), // XK_leftmiddlecurlybrace
    (0x08b0, '\u{23ac}'), // XK_rightmiddlecurlybrace
    (0x08bc, '\u{2264}'), // XK_lessthanequal
    (0x08bd, '\u{2260}'), // XK_notequal
    (0x08be, '\u{2265}'), // XK_greaterthanequal
    (0x08bf, '\u{222b}'), // XK_integral
    (0x08c0, '\u{2234}'), // XK_therefore
    (0x08c1, '\u{221d}'), // XK_variation
    (0x08c2, '\u{221e}'), // XK_infinity
    (0x08c5, '\u{2207}'), // XK_nabla
    (0x08c8, '\u{223c}'), // XK_approximate
    (0x08c9, '\u{2243}'), // XK_similarequal
    (0x08cd, '\u{21d4}'), // XK_ifonlyif
    (0x08ce, '\u{21d2}'), // XK_implies
    (0x08cf, '\u{2261}'), // XK_identical
    (0x08d6, '\u{221a}'), // XK_radical
    (0x08da, '\u{2282}'), // XK_includedin
    (0x08db, '\u{2283}'), // XK_includes
    (0x08dc, '\u{2229}'), // XK_intersection
    (0x08dd, '\u{222a}'), // XK_union
    (0x08de, '\u{2227}'), // XK_logicaland
    (0x08df, '\u{2228}'), // XK_logicalor
    (0x08ef, '\u{2202}'), // XK_partialderivative
    (0x08f6, '\u{0192}'), // XK_function
    (0x08fb, '\u{2190}'), // XK_leftarrow
    (0x08fc, '\u{2191}'), // XK_uparrow
    (0x08fd, '\u{2192}'), // XK_rightarrow
    (0x08fe, '\u{2193}'), // XK_downarrow
    (0x09e0, '\u{25c6}'), // XK_soliddiamond
    (0x09e1, '\u{2592}'), // XK_checkerboard
    (0x09e2, '\u{2409}'), // XK_ht
    (0x09e3, '\u{240c}'), // XK_ff
    (0x09e4, '\u{240d}'), // XK_cr
    (0x09e5, '\u{240a}'), // XK_lf
    (0x09e8, '\u{2424}'), // XK_nl
    (0x09e9, '\u{240b}'), // XK_vt
    (0x09ea, '\u{2518}'), // XK_lowrightcorner
    (0x09eb, '\u{2510}'), // XK_uprightcorner
    (0x09ec, '\u{250c}'), // XK_upleftcorner
    (0x09ed, '\u{2514}'), // XK_lowleftcorner
    (0x09ee, '\u{253c}'), // XK_crossinglines
    (0x09ef, '\u{23ba}'), // XK_horizlinescan1
    (0x09f0, '\u{23bb}'), // XK_horizlinescan3
    (0x09f1, '\u{2500}'), // XK_horizlinescan5
    (0x09f2, '\u{23bc}'), // XK_horizlinescan7
    (0x09f3, '\u{23bd}'), // XK_horizlinescan9
    (0x09f4, '\u{251c}'), // XK_leftt
    (0x09f5, '\u{2524}'), // XK_rightt
    (0x09f6, '\u{2534}'), // XK_bott
    (0x09f7, '\u{252c}'), // XK_topt
    (0x09f8, '\u{2502}'), // XK_vertbar
    (0x0aa1, '\u{2003}'), // XK_emspace
    (0x0aa2, '\u{2002}'), // XK_enspace
    (0x0aa3, '\u{2004}'), // XK_em3space
    (0x0aa4, '\u{2005}'), // XK_em4space
    (0x0aa5, '\u{2007}'), // XK_digitspace
    (0x0aa6, '\u{2008}'), // XK_punctspace
    (0x0aa7, '\u{2009}'), // XK_thinspace
    (0x0aa8, '\u{200a}'), // XK_hairspace
    (0x0aa9, '\u{2014}'), // XK_emdash
    (0x0aaa, '\u{2013}'), // XK_endash
    (0x0aac, '\u{2423}'), // XK_signifblank
    (0x0aae, '\u{2026}'), // XK_ellipsis
    (0x0aaf, '\u{2025}'), // XK_doubbaselinedot
    (0x0ab0, '\u{2153}'), // XK_onethird
    (0x0ab1, '\u{2154}'), // XK_twothirds
    (0x0ab2, '\u{2155}'), // XK_onefifth
    (0x0ab3, '\u{2156}'), // XK_twofifths
    (0x0ab4, '\u{2157}'), // XK_threefifths
    (0x0ab5, '\u{2158}'), // XK_fourfifths
    (0x0ab6, '\u{2159}'), // XK_onesixth
    (0x0ab7, '\u{215a}'), // XK_fivesixths
    (0x0ab8, '\u{2105}'), // XK_careof
    (0x0abb, '\u{2012}'), // XK_figdash
    (0x0abc, '\u{2329}'), // XK_leftanglebracket
    (0x0abd, '\u{002e}'), // XK_decimalpoint
    (0x0abe, '\u{232a}'), // XK_rightanglebracket
    (0x0ac3, '\u{215b}'), // XK_oneeighth
    (0x0ac4, '\u{215c}'), // XK_threeeighths
    (0x0ac5, '\u{215d}'), // XK_fiveeighths
    (0x0ac6, '\u{215e}'), // XK_seveneighths
    (0x0ac9, '\u{2122}'), // XK_trademark
    (0x0aca, '\u{2613}'), // XK_signaturemark
    (0x0acc, '\u{25c1}'), // XK_leftopentriangle
    (0x0acd, '\u{25b7}'), // XK_rightopentriangle
    (0x0ace, '\u{25cb}'), // XK_emopencircle
    (0x0acf, '\u{25af}'), // XK_emopenrectangle
    (0x0ad0, '\u{2018}'), // XK_leftsinglequotemark
    (0x0ad1, '\u{2019}'), // XK_rightsinglequotemark
    (0x0ad2, '\u{201c}'), // XK_leftdoublequotemark
    (0x0ad3, '\u{201d}'), // XK_rightdoublequotemark
    (0x0ad4, '\u{211e}'), // XK_prescription
    (0x0ad5, '\u{2030}'), // XK_permille
    (0x0ad6, '\u{2032}'), // XK_minutes
    (0x0ad7, '\u{2033}'), // XK_seconds
    (0x0ad9, '\u{271d}'), // XK_latincross
    (0x0adb, '\u{25ac}'), // XK_filledrectbullet
    (0x0adc, '\u{25c0}'), // XK_filledlefttribullet
    (0x0add, '\u{25b6}'), // XK_filledrighttribullet
    (0x0ade, '\u{25cf}'), // XK_emfilledcircle
    (0x0adf, '\u{25ae}'), // XK_emfilledrect
    (0x0ae0, '\u{25e6}'), // XK_enopencircbullet
    (0x0ae1, '\u{25ab}'), // XK_enopensquarebullet
    (0x0ae2, '\u{25ad}'), // XK_openrectbullet
    (0x0ae3, '\u{25b3}'), // XK_opentribulletup
    (0x0ae4, '\u{25bd}'), // XK_opentribulletdown
    (0x0ae5, '\u{2606}'), // XK_openstar
    (0x0ae6, '\u{2022}'), // XK_enfilledcircbullet
    (0x0ae7, '\u{25aa}'), // XK_enfilledsqbullet
    (0x0ae8, '\u{25b2}'), // XK_filledtribulletup
    (0x0ae9, '\u{25bc}'), // XK_filledtribulletdown
    (0x0aea, '\u{261c}'), // XK_leftpointer
    (0x0aeb, '\u{261e}'), // XK_rightpointer
    (0x0aec, '\u{2663}'), // XK_club
    (0x0aed, '\u{2666}'), // XK_diamond
    (0x0aee, '\u{2665}'), // XK_heart
    (0x0af0, '\u{2720}'), // XK_maltesecross
    (0x0af1, '\u{2020}'), // XK_dagger
    (0x0af2, '\u{2021}'), // XK_doubledagger
    (0x0af3, '\u{2713}'), // XK_checkmark
    (0x0af4, '\u{2717}'), // XK_ballotcross
    (0x0af5, '\u{266f}'), // XK_musicalsharp
    (0x0af6, '\u{266d}'), // XK_musicalflat
    (0x0af7, '\u{2642}'), // XK_malesymbol
    (0x0af8, '\u{2640}'), // XK_femalesymbol
    (0x0af9, '\u{260e}'), // XK_telephone
    (0x0afa, '\u{2315}'), // XK_telephonerecorder
    (0x0afb, '\u{2117}'), // XK_phonographcopyright
    (0x0afc, '\u{2038}'), // XK_caret
    (0x0afd, '\u{201a}'), // XK_singlelowquotemark
    (0x0afe, '\u{201e}'), // XK_doublelowquotemark
    (0x0ba3, '\u{003c}'), // XK_leftcaret
    (0x0ba6, '\u{003e}'), // XK_rightcaret
    (0x0ba8, '\u{2228}'), // XK_downcaret
    (0x0ba9, '\u{2227}'), // XK_upcaret
    (0x0bc0, '\u{00af}'), // XK_overbar
    (0x0bc2, '\u{22a4}'), // XK_downtack
    (0x0bc3, '\u{2229}'), // XK_upshoe
    (0x0bc4, '\u{230a}'), // XK_downstile
    (0x0bc6, '\u{005f}'), // XK_underbar
    (0x0bca, '\u{2218}'), // XK_jot
    (0x0bcc, '\u{2395}'), // XK_quad
    (0x0bce, '\u{22a5}'), // XK_uptack
    (0x0bcf, '\u{25cb}'), // XK_circle
    (0x0bd3, '\u{2308}'), // XK_upstile
    (0x0bd6, '\u{222a}'), // XK_downshoe
    (0x0bd8, '\u{2283}'), // XK_rightshoe
    (0x0bda, '\u{2282}'), // XK_leftshoe
    (0x0bdc, '\u{22a3}'), // XK_lefttack
    (0x0bfc, '\u{22a2}'), // XK_righttack
    (0x0cdf, '\u{2017}'), // XK_hebrew_doublelowline
    (0x0ce0, '\u{05d0}'), // XK_hebrew_aleph
    (0x0ce1, '\u{05d1}'), // XK_hebrew_bet
    (0x0ce2, '\u{05d2}'), // XK_hebrew_gimel
    (0x0ce3, '\u{05d3}'), // XK_hebrew_dalet
    (0x0ce4, '\u{05d4}'), // XK_hebrew_he
    (0x0ce5, '\u{05d5}'), // XK_hebrew_waw
    (0x0ce6, '\u{05d6}'), // XK_hebrew_zain
    (0x0ce7, '\u{05d7}'), // XK_hebrew_chet
    (0x0ce8, '\u{05d8}'), // XK_hebrew_tet
    (0x0ce9, '\u{05d9}'), // XK_hebrew_yod
    (0x0cea, '\u{05da}'), // XK_hebrew_finalkaph
    (0x0ceb, '\u{05db}'), // XK_hebrew_kaph
    (0x0cec, '\u{05dc}'), // XK_hebrew_lamed
    (0x0ced, '\u{05dd}'), // XK_hebrew_finalmem
    (0x0cee, '\u{05de}'), // XK_hebrew_mem
    (0x0cef, '\u{05df}'), // XK_hebrew_finalnun
    (0x0cf0, '\u{05e0}'), // XK_hebrew_nun
    (0x0cf1, '\u{05e1}'), // XK_hebrew_samech
    (0x0cf2, '\u{05e2}'), // XK_hebrew_ayin
    (0x0cf3, '\u{05e3}'), // XK_hebrew_finalpe
    (0x0cf4, '\u{05e4}'), // XK_hebrew_pe
    (0x0cf5, '\u{05e5}'), // XK_hebrew_finalzade
    (0x0cf6, '\u{05e6}'), // XK_hebrew_zade
    (0x0cf7, '\u{05e7}'), // XK_hebrew_qoph
    (0x0cf8, '\u{05e8}'), // XK_hebrew_resh
    (0x0cf9, '\u{05e9}'), // XK_hebrew_shin
    (0x0cfa, '\u{05ea}'), // XK_hebrew_taw
    (0x0da1, '\u{0e01}'), // XK_Thai_kokai
    (0x0da2, '\u{0e02}'), // XK_Thai_khokhai
    (0x0da3, '\u{0e03}'), // XK_Thai_khokhuat
    (0x0da4, '\u{0e04}'), // XK_Thai_khokhwai
    (0x0da5, '\u{0e05}'), // XK_Thai_khokhon
    (0x0da6, '\u{0e06}'), // XK_Thai_khorakhang
    (0x0da7, '\u{0e07}'), // XK_Thai_ngongu
    (0x0da8, '\u{0e08}'), // XK_Thai_chochan
    (0x0da9, '\u{0e09}'), // XK_Thai_choching
    (0x0daa, '\u{0e0a}'), // XK_Thai_chochang
    (0x0dab, '\u{0e0b}'), // XK_Thai_soso
    (0x0dac, '\u{0e0c}'), // XK_Thai_chochoe
    (0x0dad, '\u{0e0d}'), // XK_Thai_yoying
    (0x0dae, '\u{0e0e}'), // XK_Thai_dochada
    (0x0daf, '\u{0e0f}'), // XK_Thai_topatak
    (0x0db0, '\u{0e10}'), // XK_Thai_thothan
    (0x0db1, '\u{0e11}'), // XK_Thai_thonangmontho
    (0x0db2, '\u{0e12}'), // XK_Thai_thophuthao
    (0x0db3, '\u{0e13}'), // XK_Thai_nonen
    (0x0db4, '\u{0e14}'), // XK_Thai_dodek
    (0x0db5, '\u{0e15}'), // XK_Thai_totao
    (0x0db6, '\u{0e16}'), // XK_Thai_thothung
    (0x0db7, '\u{0e17}'), // XK_Thai_thothahan
    (0x0db8, '\u{0e18}'), // XK_Thai_thothong
    (0x0db9, '\u{0e19}'), // XK_Thai_nonu
    (0x0dba, '\u{0e1a}'), // XK_Thai_bobaimai
    (0x0dbb, '\u{0e1b}'), // XK_Thai_popla
    (0x0dbc, '\u{0e1c}'), // XK_Thai_phophung
    (0x0dbd, '\u{0e1d}'), // XK_Thai_fofa
    (0x0dbe, '\u{0e1e}'), // XK_Thai_phophan
    (0x0dbf, '\u{0e1f}'), // XK_Thai_fofan
    (0x0dc0, '\u{0e20}'), // XK_Thai_phosamphao
    (0x0dc1, '\u{0e21}'), // XK_Thai_moma
    (0x0dc2, '\u{0e22}'), // XK_Thai_yoyak
    (0x0dc3, '\u{0e23}'), // XK_Thai_rorua
    (0x0dc4, '\u{0e24}'), // XK_Thai_ru
    (0x0dc5, '\u{0e25}'), // XK_Thai_loling
    (0x0dc6, '\u{0e26}'), // XK_Thai_lu
    (0x0dc7, '\u{0e27}'), // XK_Thai_wowaen
    (0x0dc8, '\u{0e28}'), // XK_Thai_sosala
    (0x0dc9, '\u{0e29}'), // XK_Thai_sorusi
    (0x0dca, '\u{0e2a}'), // XK_Thai_sosua
    (0x0dcb, '\u{0e2b}'), // XK_Thai_hohip
    (0x0dcc, '\u{0e2c}'), // XK_Thai_lochula
    (0x0dcd, '\u{0e2d}'), // XK_Thai_oang
    (0x0dce, '\u{0e2e}'), // XK_Thai_honokhuk
    (0x0dcf, '\u{0e2f}'), // XK_Thai_paiyannoi
    (0x0dd0, '\u{0e30}'), // XK_Thai_saraa
    (0x0dd1, '\u{0e31}'), // XK_Thai_maihanakat
    (0x0dd2, '\u{0e32}'), // XK_Thai_saraaa
    (0x0dd3, '\u{0e33}'), // XK_Thai_saraam
    (0x0dd4, '\u{0e34}'), // XK_Thai_sarai
    (0x0dd5, '\u{0e35}'), // XK_Thai_saraii
    (0x0dd6, '\u{0e36}'), // XK_Thai_saraue
    (0x0dd7, '\u{0e37}'), // XK_Thai_sarauee
    (0x0dd8, '\u{0e38}'), // XK_Thai_sarau
    (0x0dd9, '\u{0e39}'), // XK_Thai_sarauu
    (0x0dda, '\u{0e3a}'), // XK_Thai_phinthu
    (0x0ddf, '\u{0e3f}'), // XK_Thai_baht
    (0x0de0, '\u{0e40}'), // XK_Thai_sarae
    (0x0de1, '\u{0e41}'), // XK_Thai_saraae
    (0x0de2, '\u{0e42}'), // XK_Thai_sarao
    (0x0de3, '\u{0e43}'), // XK_Thai_saraaimaimuan
    (0x0de4, '\u{0e44}'), // XK_Thai_saraaimaimalai
    (0x0de5, '\u{0e45}'), // XK_Thai_lakkhangyao
    (0x0de6, '\u{0e46}'), // XK_Thai_maiyamok
    (0x0de7, '\u{0e47}'), // XK_Thai_maitaikhu
    (0x0de8, '\u{0e48}'), // XK_Thai_maiek
    (0x0de9, '\u{0e49}'), // XK_Thai_maitho
    (0x0dea, '\u{0e4a}'), // XK_Thai_maitri
    (0x0deb, '\u{0e4b}'), // XK_Thai_maichattawa
    (0x0dec, '\u{0e4c}'), // XK_Thai_thanthakhat
    (0x0ded, '\u{0e4d}'), // XK_Thai_nikhahit
    (0x0df0, '\u{0e50}'), // XK_Thai_leksun
    (0x0df1, '\u{0e51}'), // XK_Thai_leknung
    (0x0df2, '\u{0e52}'), // XK_Thai_leksong
    (0x0df3, '\u{0e53}'), // XK_Thai_leksam
    (0x0df4, '\u{0e54}'), // XK_Thai_leksi
    (0x0df5, '\u{0e55}'), // XK_Thai_lekha
    (0x0df6, '\u{0e56}'), // XK_Thai_lekhok
    (0x0df7, '\u{0e57}'), // XK_Thai_lekchet
    (0x0df8, '\u{0e58}'), // XK_Thai_lekpaet
    (0x0df9, '\u{0e59}'), // XK_Thai_lekkao
    (0x0ea1, '\u{3131}'), // XK_Hangul_Kiyeog
    (0x0ea2, '\u{3132}'), // XK_Hangul_SsangKiyeog
    (0x0ea3, '\u{3133}'), // XK_Hangul_KiyeogSios
    (0x0ea4, '\u{3134}'), // XK_Hangul_Nieun
    (0x0ea5, '\u{3135}'), // XK_Hangul_NieunJieuj
    (0x0ea6, '\u{3136}'), // XK_Hangul_NieunHieuh
    (0x0ea7, '\u{3137}'), // XK_Hangul_Dikeud
    (0x0ea8, '\u{3138}'), // XK_Hangul_SsangDikeud
    (0x0ea9, '\u{3139}'), // XK_Hangul_Rieul
    (0x0eaa, '\u{313a}'), // XK_Hangul_RieulKiyeog
    (0x0eab, '\u{313b}'), // XK_Hangul_RieulMieum
    (0x0eac, '\u{313c}'), // XK_Hangul_RieulPieub
    (0x0ead, '\u{313d}'), // XK_Hangul_RieulSios
    (0x0eae, '\u{313e}'), // XK_Hangul_RieulTieut
    (0x0eaf, '\u{313f}'), // XK_Hangul_RieulPhieuf
    (0x0eb0, '\u{3140}'), // XK_Hangul_RieulHieuh
    (0x0eb1, '\u{3141}'), // XK_Hangul_Mieum
    (0x0eb2, '\u{3142}'), // XK_Hangul_Pieub
    (0x0eb3, '\u{3143}'), // XK_Hangul_SsangPieub
    (0x0eb4, '\u{3144}'), // XK_Hangul_PieubSios
    (0x0eb5, '\u{3145}'), // XK_Hangul_Sios
    (0x0eb6, '\u{3146}'), // XK_Hangul_SsangSios
    (0x0eb7, '\u{3147}'), // XK_Hangul_Ieung
    (0x0eb8, '\u{3148}'), // XK_Hangul_Jieuj
    (0x0eb9, '\u{3149}'), // XK_Hangul_SsangJieuj
    (0x0eba, '\u{314a}'), // XK_Hangul_Cieuc
    (0x0ebb, '\u{314b}'), // XK_Hangul_Khieuq
    (0x0ebc, '\u{314c}'), // XK_Hangul_Tieut
    (0x0ebd, '\u{314d}'), // XK_Hangul_Phieuf
    (0x0ebe, '\u{314e}'), // XK_Hangul_Hieuh
    (0x0ebf, '\u{314f}'), // XK_Hangul_A
    (0x0ec0, '\u{3150}'), // XK_Hangul_AE
    (0x0ec1, '\u{3151}'), // XK_Hangul_YA
    (0x0ec2, '\u{3152}'), // XK_Hangul_YAE
    (0x0ec3, '\u{3153}'), // XK_Hangul_EO
    (0x0ec4, '\u{3154}'), // XK_Hangul_E
    (0x0ec5, '\u{3155}'), // XK_Hangul_YEO
    (0x0ec6, '\u{3156}'), // XK_Hangul_YE
    (0x0ec7, '\u{3157}'), // XK_Hangul_O
    (0x0ec8, '\u{3158}'), // XK_Hangul_WA
    (0x0ec9, '\u{3159}'), // XK_Hangul_WAE
    (0x0eca, '\u{315a}'), // XK_Hangul_OE
    (0x0ecb, '\u{315b}'), // XK_Hangul_YO
    (0x0ecc, '\u{315c}'), // XK_Hangul_U
    (0x0ecd, '\u{315d}'), // XK_Hangul_WEO
    (0x0ece, '\u{315e}'), // XK_Hangul_WE
    (0x0ecf, '\u{315f}'), // XK_Hangul_WI
    (0x0ed0, '\u{3160}'), // XK_Hangul_YU
    (0x0ed1, '\u{3161}'), // XK_Hangul_EU
    (0x0ed2, '\u{3162}'), // XK_Hangul_YI
    (0x0ed3, '\u{3163}'), // XK_Hangul_I
    (0x0ed4, '\u{11a8}'), // XK_Hangul_J_Kiyeog
    (0x0ed5, '\u{11a9}'), // XK_Hangul_J_SsangKiyeog
    (0x0ed6, '\u{11aa}'), // XK_Hangul_J_KiyeogSios
    (0x0ed7, '\u{11ab}'), // XK_Hangul_J_Nieun
    (0x0ed8, '\u{11ac}'), // XK_Hangul_J_NieunJieuj
    (0x0ed9, '\u{11ad}'), // XK_Hangul_J_NieunHieuh
    (0x0eda, '\u{11ae}'), // XK_Hangul_J_Dikeud
    (0x0edb, '\u{11af}'), // XK_Hangul_J_Rieul
    (0x0edc, '\u{11b0}'), // XK_Hangul_J_RieulKiyeog
    (0x0edd, '\u{11b1}'), // XK_Hangul_J_RieulMieum
    (0x0ede, '\u{11b2}'), // XK_Hangul_J_RieulPieub
    (0x0edf, '\u{11b3}'), // XK_Hangul_J_RieulSios
    (0x0ee0, '\u{11b4}'), // XK_Hangul_J_RieulTieut
    (0x0ee1, '\u{11b5}'), // XK_Hangul_J_RieulPhieuf
    (0x0ee2, '\u{11b6}'), // XK_Hangul_J_RieulHieuh
    (0x0ee3, '\u{11b7}'), // XK_Hangul_J_Mieum
    (0x0ee4, '\u{11b8}'), // XK_Hangul_J_Pieub
    (0x0ee5, '\u{11b9}'), // XK_Hangul_J_PieubSios
    (0x0ee6, '\u{11ba}'), // XK_Hangul_J_Sios
    (0x0ee7, '\u{11bb}'), // XK_Hangul_J_SsangSios
    (0x0ee8, '\u{11bc}'), // XK_Hangul_J_Ieung
    (0x0ee9, '\u{11bd}'), // XK_Hangul_J_Jieuj
    (0x0eea, '\u{11be}'), // XK_Hangul_J_Cieuc
    (0x0eeb, '\u{11bf}'), // XK_Hangul_J_Khieuq
    (0x0eec, '\u{11c0}'), // XK_Hangul_J_Tieut
    (0x0eed, '\u{11c1}'), // XK_Hangul_J_Phieuf
    (0x0eee, '\u{11c2}'), // XK_Hangul_J_Hieuh
    (0x0eef, '\u{316d}'), // XK_Hangul_RieulYeorinHieuh
    (0x0ef0, '\u{3171}'), // XK_Hangul_SunkyeongeumMieum
    (0x0ef1, '\u{3178}'), // XK_Hangul_SunkyeongeumPieub
    (0x0ef2, '\u{317f}'), // XK_Hangul_PanSios
    (0x0ef3, '\u{3181}'), // XK_Hangul_KkogjiDalrinIeung
    (0x0ef4, '\u{3184}'), // XK_Hangul_SunkyeongeumPhieuf
    (0x0ef5, '\u{3186}'), // XK_Hangul_YeorinHieuh
    (0x0ef6, '\u{318d}'), // XK_Hangul_AraeA
    (0x0ef7, '\u{318e}'), // XK_Hangul_AraeAE
    (0x0ef8, '\u{11eb}'), // XK_Hangul_J_PanSios
    (0x0ef9, '\u{11f0}'), // XK_Hangul_J_KkogjiDalrinIeung
    (0x0efa, '\u{11f9}'), // XK_Hangul_J_YeorinHieuh
    (0x0eff, '\u{20a9}'), // XK_Korean_Won
    (0x13bc, '\u{0152}'), // XK_OE
    (0x13bd, '\u{0153}'), // XK_oe
    (0x13be, '\u{0178}'), // XK_Ydiaeresis
    (0x20ac, '\u{20ac}'), // XK_EuroSign
];

#[cfg(test)]
mod tests {
    //! Pure-helper tests: the key tables, the keysym conversions, the keymap reading
    //! and the level arithmetic. Nothing here opens a connection; the live suite that
    //! drives a real X server is `tests/x11.rs`.
    use super::*;

    #[test]
    fn the_usage_table_is_a_function_both_ways_except_where_it_says_so() {
        let mut usages: Vec<u32> = USAGE_EVDEV.iter().map(|(u, _)| *u).collect();
        usages.sort_unstable();
        let before = usages.len();
        usages.dedup();
        assert_eq!(before, usages.len(), "a usage appears twice");

        let mut collisions: Vec<u8> = Vec::new();
        let mut seen: Vec<u8> = Vec::new();
        for (_, evdev) in USAGE_EVDEV {
            if seen.contains(evdev) {
                collisions.push(*evdev);
            }
            seen.push(*evdev);
        }
        assert_eq!(
            collisions,
            vec![43],
            "the only key two usages share is the backslash with the non-US hash"
        );
    }

    /// The three keys that exist on both HID pages travel as the usage the frozen table
    /// NAMES, which is the consumer one.
    ///
    /// Reported on the keyboard page they arrived with no canonical name, which is the
    /// fallback a target with an incomplete usage table relies on, and they were
    /// unresolvable on a Windows target whose own table has the consumer page only.
    #[test]
    fn a_media_key_travels_as_the_page_the_dialect_names_it_on() {
        for (name, evdev) in [
            ("VolumeMute", 113u8),
            ("VolumeDown", 114),
            ("VolumeUp", 115),
        ] {
            let usage = usage_of_keycode(evdev + 8).expect("a key");
            assert_eq!(
                keys::name_of(usage),
                Some(name),
                "evdev {evdev} must be the usage the frozen table calls {name}"
            );
        }
        // And nothing else moved: the ordinary keys are still the keyboard page.
        assert_eq!(
            usage_of_keycode(36),
            Some(keys::usage(keys::PAGE_KEYBOARD, 0x28)),
            "Enter"
        );
    }

    /// The X keycode of a usage is the evdev code plus 8 (truth 7), and the round trip
    /// has to give the usage back or a captured key would travel as another one.
    #[test]
    fn a_usage_becomes_a_keycode_and_the_keycode_becomes_it_again() {
        assert_eq!(keycode_of(keys::usage(keys::PAGE_KEYBOARD, 0x28)), Some(36));
        assert_eq!(keycode_of(keys::usage(keys::PAGE_KEYBOARD, 0x04)), Some(38));
        assert_eq!(keycode_of(keys::usage(keys::PAGE_KEYBOARD, 0xE0)), Some(37));
        for (id, _) in USAGE_EVDEV {
            let usage = keys::usage(keys::PAGE_KEYBOARD, *id);
            let keycode = keycode_of(usage).expect("every entry has a keycode");
            let back = usage_of_keycode(keycode).expect("and the keycode names a usage");
            // The exceptions, each a key two usages really do share: the backslash and the
            // non-US hash are one key, and mute and the two volume keys exist on both HID
            // pages, where the CONSUMER one is the page the frozen table names them on and
            // therefore the one a captured key travels as.
            if ![0x32, 0x7F, 0x80, 0x81].contains(id) {
                assert_eq!(back, usage, "usage {id:#04x} did not round trip");
            }
        }
        assert_eq!(keycode_of(0x0007_00FF), None);
        assert_eq!(usage_of_keycode(0), None, "a keycode below 8 is not a key");
    }

    /// Every canonical name the dialect defines has a way in on X11: a keysym, or a
    /// position.
    #[test]
    fn every_named_key_of_the_dialect_has_a_keysym_or_a_position() {
        for (name, usage) in keys::NAMED {
            assert!(
                keysym_of_name(name).is_some() || keycode_of(*usage).is_some(),
                "{name} has no way in on X11"
            );
        }
    }

    /// The two algorithmic rules and the table, in both directions. A character that
    /// converted to a keysym the keymap is not written in would never be found on it.
    #[test]
    fn a_character_and_its_keysym_convert_both_ways() {
        for (ch, keysym) in [
            ('a', 0x0061),
            (' ', 0x0020),
            ('~', 0x007E),
            ('e', 0x0065),
            ('\u{00e9}', 0x00E9),
            ('\u{00ff}', 0x00FF),
        ] {
            assert_eq!(keysym_of_char(ch), keysym, "{ch:?} to keysym");
            assert_eq!(
                text_of_keysym(keysym).as_deref(),
                Some(ch.to_string().as_str()),
                "{keysym:#x} to text"
            );
        }
        // The Unicode form, for a character with no legacy keysym at all.
        assert_eq!(keysym_of_char('\u{1F600}'), 0x0101_F600);
        assert_eq!(text_of_keysym(0x0101_F600).as_deref(), Some("\u{1F600}"));
        // A function keysym produces no text, and must not be mistaken for one.
        assert_eq!(text_of_keysym(0xFF0D), None, "Return produces no character");
        assert_eq!(text_of_keysym(0xFFE1), None, "nor does a Shift");
        assert_eq!(text_of_keysym(0), None);
    }

    /// The legacy table is what makes a keymap in another script readable at all, and
    /// it has to be a function in both directions.
    #[test]
    fn the_legacy_keysym_table_is_consistent() {
        assert!(
            KEYSYM_UNICODE.len() > 500,
            "the generated table is suspiciously small: {}",
            KEYSYM_UNICODE.len()
        );
        let mut keysyms: Vec<u32> = KEYSYM_UNICODE.iter().map(|(k, _)| *k).collect();
        keysyms.sort_unstable();
        let before = keysyms.len();
        keysyms.dedup();
        assert_eq!(before, keysyms.len(), "a keysym appears twice");
        // Nothing in it may overlap the two algorithmic ranges, or a lookup would
        // shadow a rule and the two directions would disagree.
        for (keysym, _) in KEYSYM_UNICODE {
            assert!(
                !((0x0020..=0x007E).contains(keysym) || (0x00A0..=0x00FF).contains(keysym)),
                "{keysym:#x} is in the Latin-1 range the rule already covers"
            );
            assert!(keysym & 0xFF00_0000 != 0x0100_0000);
        }
        // Two entries this feature actually needs: an AltGr level on a French layout,
        // and a Cyrillic letter.
        assert_eq!(text_of_keysym(0x06C1).as_deref(), Some("\u{0430}"));
        assert_eq!(keysym_of_char('\u{0430}'), 0x06C1);
    }

    /// A level is a pair of canonical modifier bits, and the two directions have to
    /// agree or a resolution would ask for a level it did not mean.
    #[test]
    fn a_level_and_its_modifiers_agree() {
        for level in 0..4u32 {
            assert_eq!(level_for_mods(mods_for_level(level)), level);
        }
        assert_eq!(mods_for_level(0), 0);
        assert_eq!(mods_for_level(1), keys::mods::SHIFT);
        assert_eq!(mods_for_level(2), keys::mods::ALTGR);
        // Modifiers that do not select a level are ignored rather than changing one:
        // Control plus c must stay the c key's first level, or a copy would become a
        // letter.
        assert_eq!(level_for_mods(keys::mods::CTRL), 0);
        assert_eq!(level_for_mods(keys::mods::CTRL | keys::mods::SHIFT), 1);
    }

    fn keymap_of(entries: &[(u8, u8, &[u32])]) -> Keymap {
        Keymap {
            first: 8,
            entries: entries
                .iter()
                .map(|(groups, width, syms)| KeyEntry {
                    groups: *groups,
                    width: *width,
                    syms: syms.to_vec(),
                })
                .collect(),
            fingerprint: "test".into(),
        }
    }

    /// The search that turns a symbol into a key, and the two rules it follows: the
    /// ACTIVE group wins over another one, and the lowest level wins inside a group.
    ///
    /// The second rule is what makes a lower case letter cost no modifier; the first
    /// is what keeps a session from switching the machine's keyboard group for a
    /// character that was available where it already was.
    #[test]
    fn the_keymap_search_prefers_the_active_group_and_the_lowest_level() {
        // Keycode 8: one group, four levels (a, A, ae, AE). Keycode 9: two groups,
        // two levels each, with the letter a again in the second group.
        let map = keymap_of(&[
            (1, 4, &[0x61, 0x41, 0x00E6, 0x00C6]),
            (2, 2, &[0x7A, 0x5A, 0x61, 0x41]),
        ]);
        assert_eq!(map.find(0x61, 0), Some((8, 0, 0)), "the plain a, unshifted");
        assert_eq!(map.find(0x41, 0), Some((8, 0, 1)), "the capital A, shifted");
        assert_eq!(
            map.find(0x00E6, 0),
            Some((8, 0, 2)),
            "and the third level is AltGr, which is what typing on AZERTY needs"
        );
        // With group 1 active, the a of the second key's second group is the closer
        // answer.
        assert_eq!(map.find(0x61, 1), Some((9, 1, 0)));
        assert_eq!(map.find(0x7A, 0), Some((9, 0, 0)));
        assert_eq!(map.find(0x1234, 0), None, "a keysym nothing produces");
    }

    /// A level above the fourth needs `ISO_Level5_Shift`, which the dialect's eight
    /// canonical bits cannot name, so it must not be answered at all: a resolution
    /// that named it would press the key without the modifier and type the wrong
    /// character.
    #[test]
    fn a_fifth_level_is_not_an_answer_this_seam_can_carry() {
        let map = keymap_of(&[(1, 6, &[0x61, 0x41, 0x62, 0x42, 0x63, 0x43])]);
        assert_eq!(map.find(0x61, 0), Some((8, 0, 0)));
        assert_eq!(map.find(0x42, 0), Some((8, 0, 3)));
        assert_eq!(map.find(0x63, 0), None, "the fifth level");
        assert_eq!(map.find(0x43, 0), None, "and the sixth");
    }

    /// A keysym of zero is no keysym: a keymap is padded with them, and a search that
    /// matched one would resolve every unproducible character to the first padded key
    /// it found.
    #[test]
    fn a_padded_keymap_entry_is_not_a_key() {
        let map = keymap_of(&[(1, 4, &[0x61, 0x41, 0, 0])]);
        assert_eq!(map.sym(8, 0, 2), None);
        assert_eq!(map.find(0, 0), None);
        // And a group or a level outside the entry is not a key either.
        assert_eq!(map.sym(8, 5, 0), None);
        assert_eq!(map.sym(8, 0, 9), None);
        assert_eq!(map.sym(200, 0, 0), None, "a keycode outside the map");
        assert_eq!(map.sym(0, 0, 0), None, "and one below its first");
    }

    /// The dialect's button numbers are not X's, and the wheel is not a button at all
    /// as far as the dialect is concerned.
    #[test]
    fn the_button_numbers_are_translated_and_the_rest_is_refused() {
        assert_eq!(x_button(1), Some(1));
        assert_eq!(x_button(3), Some(3));
        assert_eq!(x_button(4), Some(8), "back is X's button 8");
        assert_eq!(x_button(5), Some(9), "forward is X's button 9");
        assert_eq!(x_button(6), None);
        assert_eq!(x_button(0), None);
    }

    /// A fraction of a pixel is carried over rather than truncated, which is what makes
    /// a high resolution device move the pointer at all: a touchpad sending an eighth of
    /// a pixel per event would otherwise report zero for ever.
    #[test]
    fn a_fraction_of_a_pixel_is_kept_until_it_is_a_whole_one() {
        let eighth = 1i64 << 29;
        let mut acc = 0i64;
        for _ in 0..7 {
            acc += eighth;
            assert_eq!(spend_pixels(&mut acc), 0, "seven eighths are no pixel yet");
        }
        acc += eighth;
        assert_eq!(spend_pixels(&mut acc), 1, "and the eighth one is");
        assert_eq!(acc, 0, "with nothing left over");

        // Backwards, which must not round towards zero and lose the movement.
        let mut acc = 0i64;
        for _ in 0..3 {
            acc -= eighth;
        }
        assert_eq!(
            spend_pixels(&mut acc),
            -1,
            "an arithmetic shift rounds down"
        );
        assert_eq!(
            acc,
            5i64 << 29,
            "and the remainder is what is left above it"
        );
        for _ in 0..5 {
            acc -= eighth;
        }
        assert_eq!(spend_pixels(&mut acc), 0);
        assert_eq!(acc, 0, "eight eighths backwards are exactly one pixel back");

        // Whole pixels pass straight through, and a number no coordinate could hold is
        // clamped rather than wrapped.
        let mut acc = 24i64 << 32;
        assert_eq!(spend_pixels(&mut acc), 24);
        let mut acc = i64::MAX;
        assert!(spend_pixels(&mut acc) > 0);
    }

    /// Caps Lock inverts the shift level of a LETTER and of nothing else, which is
    /// XKB's own rule and the reason a capital typed with the lock on arrives as a
    /// capital.
    ///
    /// Without it the backend reported the unshifted level, so a person with Caps Lock on
    /// saw capitals on their own screen and the other computer typed lower case for every
    /// one of them.
    #[test]
    fn caps_lock_changes_the_level_of_a_letter_and_of_nothing_else() {
        // A letter (a, A), a digit whose shifted level is punctuation (1, exclam), and a
        // four level letter (a, A, ae, AE).
        let letter = KeyEntry {
            groups: 1,
            width: 2,
            syms: vec![0x61, 0x41],
        };
        let digit = KeyEntry {
            groups: 1,
            width: 2,
            syms: vec![0x31, 0x21],
        };
        let four = KeyEntry {
            groups: 1,
            width: 4,
            syms: vec![0x61, 0x41, 0x00E6, 0x00C6],
        };
        let caps = keys::mods::CAPS;
        let shift = keys::mods::SHIFT;
        let altgr = keys::mods::ALTGR;

        assert_eq!(level_of_stroke(&letter, 0, 0), 0, "no lock, no shift");
        assert_eq!(
            level_of_stroke(&letter, 0, caps),
            1,
            "the lock is a shift here"
        );
        assert_eq!(
            level_of_stroke(&letter, 0, caps | shift),
            0,
            "and a shift with the lock on is the unshifted level again"
        );
        assert_eq!(
            level_of_stroke(&digit, 0, caps),
            0,
            "a digit is not a letter: the lock does nothing to it"
        );
        assert_eq!(level_of_stroke(&digit, 0, shift), 1);
        // The AltGr pair of a four level key is its own two levels, and the lock inverts
        // within the pair rather than across it.
        assert_eq!(level_of_stroke(&four, 0, altgr), 2);
        assert_eq!(
            level_of_stroke(&four, 0, altgr | caps),
            3,
            "the AltGr pair of this key is ae and AE, which IS a case pair, so the lock \
             inverts within it: XKB's rule, and not an accident of this key"
        );
        assert_eq!(
            level_of_stroke(&letter, 0, altgr),
            2,
            "and a level the key does not have is asked for as it is, so the caller finds \
             nothing rather than the wrong thing"
        );
    }

    /// The letter test's other half: what counts as a case pair.
    #[test]
    fn only_a_letter_and_its_own_capital_are_a_case_pair() {
        assert!(is_alphabetic(Some(0x61), Some(0x41)), "a and A");
        assert!(
            is_alphabetic(Some(0x00E9), Some(0x00C9)),
            "e acute and its capital"
        );
        assert!(!is_alphabetic(Some(0x31), Some(0x21)), "1 and exclam");
        assert!(
            !is_alphabetic(Some(0x61), Some(0x61)),
            "a key with one level twice"
        );
        assert!(
            !is_alphabetic(Some(0x61), None),
            "and half a pair is no pair"
        );
        assert!(!is_alphabetic(None, None));
        // A keysym with no text at all is not a letter, whatever else it is.
        assert!(!is_alphabetic(Some(0xFFE1), Some(0xFFE2)), "two Shifts");
    }

    /// The fingerprint has to change when the SHAPE of the keymap changes, not only its
    /// keysyms: truth 4's own example is a change that leaves the keysyms alone.
    #[test]
    fn the_fingerprint_notices_a_change_of_shape() {
        let flat = vec![0x61, 0x41, 0x00E6, 0x00C6];
        let one_group_of_four = KeyEntry {
            groups: 1,
            width: 4,
            syms: flat.clone(),
        };
        let two_groups_of_two = KeyEntry {
            groups: 2,
            width: 2,
            syms: flat,
        };
        let hash = |entry: &KeyEntry, first: u8| {
            let mut hasher = blake3::Hasher::new();
            hasher.update(&[first]);
            hasher.update(&[entry.groups, entry.width]);
            for sym in &entry.syms {
                hasher.update(&sym.to_be_bytes());
            }
            hex::encode(&hasher.finalize().as_bytes()[..8])
        };
        assert_ne!(
            hash(&one_group_of_four, 8),
            hash(&two_groups_of_two, 8),
            "the same four keysyms with two different meanings are two keymaps"
        );
        assert_ne!(
            hash(&one_group_of_four, 8),
            hash(&one_group_of_four, 9),
            "and a keycode range that moved is a keymap that moved"
        );
    }

    /// A coordinate that does not fit the wire is clamped rather than wrapped: an X
    /// request carries a signed sixteen bit position, and a wrapped one would warp the
    /// pointer to the opposite corner.
    #[test]
    fn a_coordinate_is_clamped_to_what_the_protocol_can_carry() {
        assert_eq!(clamp_i16(0), 0);
        assert_eq!(clamp_i16(1920), 1920);
        assert_eq!(clamp_i16(-1080), -1080);
        assert_eq!(clamp_i16(i32::MAX), i16::MAX);
        assert_eq!(clamp_i16(i32::MIN), i16::MIN);
    }
}
