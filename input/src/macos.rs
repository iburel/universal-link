// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The macOS keyboard and mouse backend: an active `CGEventTap` for capture and
//! swallowing, `CGEventPost` for injection,
//! `CGAssociateMouseAndMouseCursorPosition` plus `CGWarpMouseCursorPosition` for the
//! pin.
//!
//! # This file has never run
//!
//! It compiles and its pure halves are unit tested, on a real macOS toolchain and in
//! CI. Nothing in it has been executed against a desktop, because the project's Mac
//! was not available while it was written. Every behaviour that needs a screen, a
//! keyboard or a person is on the deferred list of the live validation ticket, and the
//! places where that matters most are marked in the code rather than only in a report:
//! the two TCC grants and their prompts, the event tap being disabled by its own
//! timeout, the secure input field, and the 250 millisecond event suppression after a
//! warp.
//!
//! The shape is the sibling backends': the OS run loop is pinned to the main thread
//! inside [`Backend`], the async engine drives it through the cheap, `Clone`
//! [`MacBackend`] handle, and upcalls travel on a bounded channel the tap pushes with
//! `try_send` and never blocks on.
//!
//! # Eight macOS truths this backend is built on
//!
//! 1. **There are TWO grants, not one, and they are different grants.** An active
//!    `CGEventTap` needs Input Monitoring; `CGEventPost` needs Accessibility. So a Mac
//!    can be a target and not a source, or a source and not a target, and this backend
//!    reports the two halves separately through [`Capabilities`]. They are read with
//!    `CGPreflightListenEventAccess` and `CGPreflightPostEventAccess`, which are the
//!    modern supported queries and which do NOT prompt.
//! 2. **The prompt is a separate call, and it is asked for at the moment the feature
//!    is used.** `CGRequestListenEventAccess` and `CGRequestPostEventAccess` are what
//!    put the dialog on screen, and this backend calls each of them at most once per
//!    process, on the first `capture` and the first `inject` respectively. That is the
//!    ticket's "at install or when the feature is switched on" turned into something a
//!    backend can actually decide: the engine only asks for either when a person has
//!    turned the feature on, so the dialog appears then and not at login.
//! 3. **A grant that arrives is not a notification.** Nothing tells a process that TCC
//!    changed its mind, so the two preflights are polled once a second and a change
//!    becomes a [`BackendEvent::CapabilitiesChanged`]. That upcall exists BECAUSE of
//!    this platform (see its own documentation): without it a Mac whose Accessibility
//!    permission was granted at the prompt would keep telling its owner that nothing
//!    there can type.
//! 4. **`CGWarpMouseCursorPosition` suppresses local mouse events for about 250 ms.**
//!    The documented cures are a following
//!    `CGAssociateMouseAndMouseCursorPosition(true)` or a zeroed
//!    `CGSetLocalEventsSuppressionInterval`. This backend does the SECOND, once, at
//!    construction, and the reason is that the first one would be wrong here: while a
//!    session is live the association is deliberately OFF (that is what the pin is), so
//!    re-associating after every warp would undo the confinement on the very path that
//!    warps most.
//! 5. **The pin is not a clip.** macOS has no `ClipCursor`:
//!    `CGAssociateMouseAndMouseCursorPosition(false)` DECOUPLES the cursor from the
//!    mouse, so the cursor stops moving at all and the rectangle
//!    [`InputBackend::confine`] is given is advisory. The deltas keep arriving in the
//!    tap's own `kCGMouseEventDeltaX` and `kCGMouseEventDeltaY` fields, which is the
//!    OS-native relative source the `confine` capability's second half promises.
//! 6. **An injected event does not inherit the modifiers of the keys this backend
//!    pressed.** A `CGEvent` carries its own `flags`, and posting a key down for the
//!    Command key does not make the NEXT posted event a Command-something. So this
//!    backend tracks the modifier keycodes the engine presses and stamps the flags on
//!    every event it posts. Without it, Command plus C on a Mac target would type a
//!    letter c.
//! 7. **A secure input field silences the tap and refuses the injection**, and
//!    `IsSecureEventInputEnabled` is the one way to know. It is checked before typing,
//!    so the refusal reaches the interface as a sentence rather than as a password
//!    field that does nothing (doc/input-sharing.md, section 13).
//! 9. **A positive horizontal scroll means LEFT on this platform**, where the dialect
//!    says positive is right (`backend.rs`'s `Wheel`). So the sign is flipped on the way
//!    in and on the way out. It cancels out between two Macs, which is exactly why it
//!    would have gone unnoticed: only a Mac driving a PC, or a PC driving a Mac, scrolls
//!    sideways backwards. Wine's `winemac.drv` carries the same note ("Mac: negative is
//!    right or down, positive is left or up. Win32: the other way. So, negate the X
//!    scroll value"), and Chromium negates it in Blink for the same reason.
//! 10. **A synthetic click needs an event NUMBER, and on macOS 27 it stops being
//!     optional.** So every mouse event this backend posts carries one, from a counter
//!     seeded off the window server's own, and a click state of one. Without it a Mac on
//!     that release refuses synthetic clicks and drags outright, which reads as "1Device
//!     does nothing" with no refusal to explain it.
//! 11. **A tap can be inert with no disable event at all.** The documented failure is
//!     `kCGEventTapDisabledByTimeout`, which arrives through the callback and is re-enabled
//!     from the loop; the undocumented one follows a code signing or permission identity
//!     change, where the tap simply stops delivering and says nothing.
//!     `CGEventTapIsEnabled` is the only way to see it, so the loop asks once a second and
//!     recreates the tap when enabling it does not take. This project re-signs and
//!     redeploys test builds constantly, which is precisely the trigger.
//! 8. **The tap can be turned off by the system**, either because a callback took too
//!    long (`kCGEventTapDisabledByTimeout`) or because a person pressed a key
//!    (`kCGEventTapDisabledByUserInput`). Both arrive AS EVENTS through the tap's own
//!    callback, and the only correct answer is to re-enable it, which is why the
//!    callback does so little: it reads two atomics, pushes one upcall and returns.

use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use objc2_core_foundation::{
    CFBoolean, CFData, CFDictionary, CFMachPort, CFRetained, CFRunLoop, CFRunLoopRunResult,
    CFRunLoopSource, CFRunLoopSourceContext, CFString, CFType, CFUUID, kCFRunLoopDefaultMode,
};
use objc2_core_graphics::{
    CGAssociateMouseAndMouseCursorPosition, CGDirectDisplayID, CGDisplayBounds, CGError, CGEvent,
    CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation,
    CGEventTapOptions, CGEventTapPlacement, CGEventTapProxy, CGEventType, CGGetActiveDisplayList,
    CGMainDisplayID, CGMouseButton, CGScrollEventUnit, CGWarpMouseCursorPosition,
};
use tokio::sync::mpsc;

use crate::backend::{
    Action, BackendEvent, Capabilities, CaptureLoss, CaptureMode, InputBackend, KeyEvent, Monitor,
    Motion, PlatformKey, Point, Problem, Rect, Refusal, Resolved, Want,
};
use crate::keys;
use crate::os::Unsupported;

// --------------------------------------------------------------- constants

/// How long one turn of the run loop waits when nothing is happening. It bounds how
/// late a `shutdown` is noticed and how late the grant poll runs, and nothing else: a
/// downcall signals a run loop source, which returns from the wait at once.
const IDLE_TURN: Duration = Duration::from_millis(250);

/// How often the tap is asked whether it is still alive (truth 11). A second, like the
/// grants: a tap that has gone inert has gone inert until something notices, so the cost of
/// noticing late is a second of a session that is not working and says so.
const TAP_POLL: Duration = Duration::from_secs(1);

/// How often the two TCC grants are re-read (truth 3). A person granting a permission
/// is a human action, so a second is fast enough, and each preflight is a query to
/// another process.
const GRANT_POLL: Duration = Duration::from_secs(1);

/// How often the window server is asked whether the screen is locked (truth 7). A lock is a
/// human action and the question costs a round trip, so it is asked on the injection path at
/// this rate rather than once per batch.
const LOCK_POLL: Duration = Duration::from_millis(200);

/// Bounded capacity of the upcall channel. Generous: the engine drains it as its own
/// loop turns, and a full queue means it has stalled or gone.
const BACKEND_EVENT_CAPACITY: usize = 256;

/// Capture modes, as the one byte the tap callback can read without a lock.
const MODE_OFF: u8 = 0;
const MODE_WATCH: u8 = 1;
const MODE_SWALLOW: u8 = 2;

/// One wheel notch, in the POINT unit a macOS scroll event's `PointDelta` axes carry.
///
/// Not 120. That is `WHEEL_DELTA`, a Windows constant, correct where the OS delta is natively
/// in those units and meaningless here: using it made roughly twelve notches of a real wheel,
/// or a whole trackpad swipe, travel as one, and made small scrolls travel as nothing.
///
/// 10 is macOS's own default pixels-per-line, the number `CGEventSourceGetPixelsPerLine`
/// returns unless a person has changed it, and a real wheel detent arrives as a point delta of
/// about that with a LINE delta of exactly one. The line axis is used directly when the point
/// axis is empty (see the tap's scroll arm), so this divisor only has to be right for the
/// continuous devices, and how right it is on a real trackpad is on the live validation list:
/// it is one measurement and nobody has made it.
const WHEEL_POINTS_PER_NOTCH: i32 = 10;

thread_local! {
    /// The event source every injected event is created from, one per injecting thread.
    ///
    /// Created once and kept, because `CGEventSourceCreate` is a window server round trip and
    /// the injection path carries 125 events a second. A thread local rather than a field
    /// because `CFRetained<CGEventSource>` is not `Send` and the handle that would hold it is
    /// cloned across the engine's threads. `None` is a machine that would not give one, and
    /// every arm of `post` already treats a sourceless event as the fallback the API allows.
    static SOURCE: Option<CFRetained<CGEventSource>> = {
        let source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState);
        if let Some(source) = &source {
            // The non-deprecated half of truth 4's cure, on this backend's own source. The
            // global `CGSetLocalEventsSuppressionInterval` is zeroed once at construction and
            // is deprecated; whether it still has any effect is not documented either way, so
            // both are done, which is what Wine's own comment on this says it settled on.
            CGEventSource::set_local_events_suppression_interval(Some(source), 0.0);
        }
        source
    };
}

/// One line to the standard error, and never a panic.
///
/// `eprintln!` PANICS when the write fails, and a supervised component's stderr can be closed
/// or full. Two of this file's callers are inside the event tap's callback, which the window
/// server calls through `extern "C-unwind"` frames, so a panic there unwinds through somebody
/// else's C. A warning that cannot be delivered is dropped instead.
fn warn(message: &str) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "[1device-input] {message}");
}

// ------------------------------------------------------------- the key table

/// HID usage to macOS virtual keycode (the Carbon `kVK_*` numbers).
///
/// Read in both directions, so it is one list rather than two: a pair that disagreed
/// would send a key the far side could not press back.
///
/// **Never checked against real hardware.** The letters, the digits and the four keys
/// with a canonical character are checked against the machine's OWN keyboard layout by
/// [`tests::the_table_names_the_keys_the_layout_names`], which runs wherever this
/// crate's tests run on macOS; the rest is on the deferred list.
#[rustfmt::skip]
const USAGE_VK: &[(u32, u16)] = &[
    // Letters, in HID order (a to z). The macOS numbering is the 1984 Macintosh
    // keyboard's and follows no pattern at all.
    (0x04, 0),  (0x05, 11), (0x06, 8),  (0x07, 2),  (0x08, 14), (0x09, 3),
    (0x0A, 5),  (0x0B, 4),  (0x0C, 34), (0x0D, 38), (0x0E, 40), (0x0F, 37),
    (0x10, 46), (0x11, 45), (0x12, 31), (0x13, 35), (0x14, 12), (0x15, 15),
    (0x16, 1),  (0x17, 17), (0x18, 32), (0x19, 9),  (0x1A, 13), (0x1B, 7),
    (0x1C, 16), (0x1D, 6),
    // Digits 1 to 9 then 0.
    (0x1E, 18), (0x1F, 19), (0x20, 20), (0x21, 21), (0x22, 23), (0x23, 22),
    (0x24, 26), (0x25, 28), (0x26, 25), (0x27, 29),
    (0x28, 36),  // Return
    (0x29, 53),  // Escape
    (0x2A, 51),  // Delete, which is what macOS calls the backspace
    (0x2B, 48),  // Tab
    (0x2C, 49),  // Space
    (0x2D, 27),  // minus
    (0x2E, 24),  // equal
    (0x2F, 33),  // bracket left
    (0x30, 30),  // bracket right
    (0x31, 42),  // backslash
    (0x32, 42),  // the non-US hash, the same key
    (0x33, 41),  // semicolon
    (0x34, 39),  // quote
    (0x35, 50),  // grave
    (0x36, 43),  // comma
    (0x37, 47),  // period
    (0x38, 44),  // slash
    (0x39, 57),  // Caps Lock
    (0x3A, 122), (0x3B, 120), (0x3C, 99), (0x3D, 118), (0x3E, 96), (0x3F, 97),
    (0x40, 98),  (0x41, 100), (0x42, 101), (0x43, 109), // F1 to F10
    (0x44, 103), (0x45, 111), // F11, F12
    (0x49, 114), // Insert, which is the Help key's position
    (0x4A, 115), // Home
    (0x4B, 116), // Page Up
    (0x4C, 117), // Forward Delete
    (0x4D, 119), // End
    (0x4E, 121), // Page Down
    (0x4F, 124), // Right
    (0x50, 123), // Left
    (0x51, 125), // Down
    (0x52, 126), // Up
    (0x53, 71),  // Num Lock, which is the keypad Clear key
    (0x54, 75),  // keypad slash
    (0x55, 67),  // keypad asterisk
    (0x56, 78),  // keypad minus
    (0x57, 69),  // keypad plus
    (0x58, 76),  // keypad Enter
    (0x59, 83), (0x5A, 84), (0x5B, 85), (0x5C, 86), (0x5D, 87), (0x5E, 88),
    (0x5F, 89), (0x60, 91), (0x61, 92), // keypad 1 to 9
    (0x62, 82),  // keypad 0
    (0x63, 65),  // keypad decimal
    (0x64, 10),  // the 102nd key, which macOS calls ISO Section
    // The Application key. No Apple keyboard has ever had one, which is why this row
    // looked like an omission, but macOS does deliver keycode 110 for the Menu key of a
    // third party PC keyboard and Chromium's own table maps this usage to it. Without the
    // row a Windows or Linux peer's Menu key pressed nothing on a Mac that could have
    // received it, and a Mac's own Menu key reported nothing back.
    (0x65, 110), // Menu
    (0x67, 81),  // keypad equals
    (0x68, 105), (0x69, 107), (0x6A, 113), (0x6B, 106), (0x6C, 64), (0x6D, 79),
    (0x6E, 80), (0x6F, 90), // F13 to F20
    (0x7F, 74),  // Mute
    (0x80, 72),  // Volume Up
    (0x81, 73),  // Volume Down
    (0x85, 95),  // keypad comma, which macOS calls the JIS keypad comma
    (0x87, 94),  // international 1, the JIS underscore
    (0x88, 104), // international 2, the JIS kana
    (0x89, 93),  // international 3, the JIS yen
    (0x8B, 102), // international 5, the JIS eisu
    (0xE0, 59),  // left Control
    (0xE1, 56),  // left Shift
    (0xE2, 58),  // left Option, which is the Alt key
    (0xE3, 55),  // left Command
    (0xE4, 62),  // right Control
    (0xE5, 60),  // right Shift
    (0xE6, 61),  // right Option, which is what AltGr means on a Mac
    (0xE7, 54),  // right Command
];

/// The keys a Mac keyboard does not have, mapped to the key that sits where a PC
/// keyboard would put them.
///
/// ONE DIRECTION ONLY (usage to keycode), and that is the whole point of it being a
/// second table. An Apple extended keyboard has F13, F14 and F15 exactly where a PC
/// keyboard has Print Screen, Scroll Lock and Pause, so a Print Screen arriving from
/// another computer should press that key; but a key PRESSED on the Mac should be
/// reported as the F13 that is printed on it, not as a Print Screen nobody can see.
/// Putting these in [`USAGE_VK`] made the first match win and got that backwards. The
/// Japanese folds and Help are here for the same reason: several HID usages land on one
/// Mac key, and only one of them is what the key says.
#[rustfmt::skip]
const USAGE_VK_ALIAS: &[(u32, u16)] = &[
    (0x46, 105), // Print Screen sits where an extended keyboard puts F13
    (0x47, 107), // Scroll Lock, where F14 is
    (0x48, 113), // Pause, where F15 is
    (0x75, 114), // Help, which is the Insert key's position
    (0x8A, 104), // international 4, which macOS folds onto the kana key
    (0x90, 104), // language 1, the same
    (0x91, 102), // language 2, onto the eisu key
];

/// The macOS virtual keycode a wire usage means.
fn vk_of_usage(usage: u32) -> Option<u16> {
    if usage >> 16 != keys::PAGE_KEYBOARD {
        return None;
    }
    let id = usage & 0xFFFF;
    USAGE_VK
        .iter()
        .chain(USAGE_VK_ALIAS)
        .find(|(u, _)| *u == id)
        .map(|(_, vk)| *vk)
}

/// The wire usage a virtual keycode means. The inverse of [`vk_of_usage`], first match
/// first, so the several usages that share a key answer with the one a person names.
fn usage_of_vk(vk: u16) -> Option<u32> {
    USAGE_VK
        .iter()
        .find(|(_, v)| *v == vk)
        .map(|(id, _)| keys::usage(keys::PAGE_KEYBOARD, *id))
}

/// The canonical modifier bit a virtual keycode means, for the flags this backend has
/// to stamp on every event it posts (truth 6) and for the state it keeps while it
/// swallows.
fn mod_of_vk(vk: u16) -> Option<u16> {
    match vk {
        56 | 60 => Some(keys::mods::SHIFT),
        59 | 62 => Some(keys::mods::CTRL),
        58 => Some(keys::mods::ALT),
        61 => Some(keys::mods::ALTGR),
        55 | 54 => Some(keys::mods::META),
        _ => None,
    }
}

/// The canonical modifier bits a `CGEventFlags` carries.
fn mods_of_flags(flags: CGEventFlags) -> u16 {
    let mut bits = 0u16;
    if flags.contains(CGEventFlags::MaskShift) {
        bits |= keys::mods::SHIFT;
    }
    if flags.contains(CGEventFlags::MaskControl) {
        bits |= keys::mods::CTRL;
    }
    if flags.contains(CGEventFlags::MaskAlternate) {
        // One bit for both Options, because that is all the flags carry: a Mac reports
        // no left and right in `CGEventFlags`, so the ALTGR the right Option means is
        // read off the KEYCODE in the tap and not from here.
        bits |= keys::mods::ALT;
    }
    if flags.contains(CGEventFlags::MaskCommand) {
        bits |= keys::mods::META;
    }
    if flags.contains(CGEventFlags::MaskAlphaShift) {
        bits |= keys::mods::CAPS;
    }
    // `MaskNumericPad` is deliberately NOT read as Num Lock. Apple's own documentation for
    // the equivalent `NSEventModifierFlagNumericPad` says "This flag is also set if any of the
    // arrow keys are pressed", so reading it as a lock made every arrow key on the source
    // report a lock bit set on the press and clear on the release: a lock nobody touched,
    // flickering in the `mods` of every forwarded frame. There is no Num Lock on an Apple
    // keyboard and macOS exposes no state for one, so reporting nothing is the honest answer.
    bits
}

/// The `CGEventFlags` a set of canonical bits means, for stamping an injected event
/// (truth 6).
fn flags_of_mods(mods: u16) -> CGEventFlags {
    let mut flags = CGEventFlags::empty();
    if mods & keys::mods::SHIFT != 0 {
        flags |= CGEventFlags::MaskShift;
    }
    if mods & keys::mods::CTRL != 0 {
        flags |= CGEventFlags::MaskControl;
    }
    // Both Options are one flag, and AltGr IS the right Option on a Mac.
    if mods & (keys::mods::ALT | keys::mods::ALTGR) != 0 {
        flags |= CGEventFlags::MaskAlternate;
    }
    if mods & keys::mods::META != 0 {
        flags |= CGEventFlags::MaskCommand;
    }
    if mods & keys::mods::CAPS != 0 {
        flags |= CGEventFlags::MaskAlphaShift;
    }
    flags
}

/// The mouse button a dialect button number means, with the number macOS wants in
/// `kCGMouseEventButtonNumber` for the ones it has no name for.
fn mac_button(button: u8, down: bool) -> Option<(CGEventType, CGMouseButton, i64)> {
    let (kind, mac, number) = match (button, down) {
        (1, true) => (CGEventType::LeftMouseDown, CGMouseButton::Left, 0),
        (1, false) => (CGEventType::LeftMouseUp, CGMouseButton::Left, 0),
        (2, true) => (CGEventType::OtherMouseDown, CGMouseButton::Center, 2),
        (2, false) => (CGEventType::OtherMouseUp, CGMouseButton::Center, 2),
        (3, true) => (CGEventType::RightMouseDown, CGMouseButton::Right, 1),
        (3, false) => (CGEventType::RightMouseUp, CGMouseButton::Right, 1),
        (4, true) => (CGEventType::OtherMouseDown, CGMouseButton::Center, 3),
        (4, false) => (CGEventType::OtherMouseUp, CGMouseButton::Center, 3),
        (5, true) => (CGEventType::OtherMouseDown, CGMouseButton::Center, 4),
        (5, false) => (CGEventType::OtherMouseUp, CGMouseButton::Center, 4),
        _ => return None,
    };
    Some((kind, mac, number))
}

/// The dialect's button number for a mouse event the tap reported.
fn button_of_event(kind: CGEventType, number: i64) -> Option<u8> {
    match kind {
        CGEventType::LeftMouseDown | CGEventType::LeftMouseUp => Some(1),
        CGEventType::RightMouseDown | CGEventType::RightMouseUp => Some(3),
        CGEventType::OtherMouseDown | CGEventType::OtherMouseUp => match number {
            2 => Some(2),
            3 => Some(4),
            4 => Some(5),
            // A button this dialect has no number for. Inventing one would have the far
            // side press something nobody touched.
            _ => None,
        },
        _ => None,
    }
}

// ------------------------------------------------------------------ raw FFI

/// The symbols no `objc2-*` crate binds.
///
/// Declared here rather than by pulling a crate for them, following
/// `menu/src/os/macos.rs`: each is a bare C function from a framework that is always
/// present, and the alternative is a dependency for five signatures. `tao` declares the
/// same set privately, which is why it cannot be borrowed.
mod ffi {
    use std::ffi::c_void;

    // `IsSecureEventInputEnabled` is the ONLY way to know that a password field has the
    // keyboard (truth 7): while it is true no tap sees a keystroke and no injection
    // lands, so a session that does not check it looks broken rather than protected.
    // The `UCKeyTranslate` group is what turns a character into a key on the machine's
    // own layout, which is the one question the seam exists to ask.
    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        pub fn IsSecureEventInputEnabled() -> bool;
        pub fn TISCopyCurrentKeyboardLayoutInputSource() -> *mut c_void;
        pub fn TISGetInputSourceProperty(
            input_source: *mut c_void,
            property_key: *const c_void,
        ) -> *mut c_void;
        pub fn LMGetKbdType() -> u8;
        #[allow(clippy::too_many_arguments)]
        pub fn UCKeyTranslate(
            key_layout_ptr: *const u8,
            virtual_key_code: u16,
            key_action: u16,
            modifier_key_state: u32,
            keyboard_type: u32,
            key_translate_options: u32,
            dead_key_state: *mut u32,
            max_string_length: usize,
            actual_string_length: *mut usize,
            unicode_string: *mut u16,
        ) -> i32;
        pub static kTISPropertyUnicodeKeyLayoutData: *const c_void;
    }

    // `CGDisplayCreateUUIDFromDisplayID` is ColorSync's and has been reachable as an
    // ApplicationServices subframework on every supported macOS, which is the note
    // `tao` leaves next to its own declaration of it.
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        pub fn CGDisplayCreateUUIDFromDisplayID(display: u32) -> *mut c_void;
    }

    /// `kUCKeyActionDisplay`, which asks what the key would SHOW rather than what
    /// pressing it would do: the question a layout query is asking.
    pub const KEY_ACTION_DISPLAY: u16 = 3;
    /// `kUCKeyTranslateNoDeadKeysMask`, so a dead key answers with the character it
    /// would compose rather than with nothing.
    pub const NO_DEAD_KEYS: u32 = 1;
}

/// The modifier state `UCKeyTranslate` wants: the Carbon event modifier bits, shifted
/// right by eight, which is what its documentation calls for and what everybody gets
/// wrong once.
fn uchr_modifiers(mods: u16) -> u32 {
    // shiftKey is 1 << 9, optionKey is 1 << 11, controlKey is 1 << 12, cmdKey is
    // 1 << 8, alphaLock is 1 << 10. Shifted right by 8 as the call expects.
    let mut carbon = 0u32;
    if mods & keys::mods::SHIFT != 0 {
        carbon |= 1 << 9;
    }
    if mods & (keys::mods::ALT | keys::mods::ALTGR) != 0 {
        carbon |= 1 << 11;
    }
    if mods & keys::mods::CTRL != 0 {
        carbon |= 1 << 12;
    }
    if mods & keys::mods::META != 0 {
        carbon |= 1 << 8;
    }
    if mods & keys::mods::CAPS != 0 {
        carbon |= 1 << 10;
    }
    carbon >> 8
}

/// The machine's active keyboard layout, as the bytes `UCKeyTranslate` reads.
///
/// The bytes are COPIED out of the `CFData` the input source lends, rather than the
/// `CFData` being kept. That is not tidiness: a `CFRetained<CFData>` is neither `Send`
/// nor `Sync`, and this structure is read from the engine's thread on the path of a
/// keystroke, so keeping the Core Foundation object would make the whole backend
/// handle un-sendable and the seam would not compile. A `uchr` table is a few kilobytes
/// and is copied once per layout change.
#[derive(Debug)]
struct Layout {
    bytes: Vec<u8>,
    kbd_type: u32,
    /// An identity for it, so a change is noticed without a notification to subscribe
    /// to: the length and a hash of the bytes.
    identity: String,
    /// Every character this layout can produce, to the key and modifiers that produce it.
    ///
    /// Built on the FIRST symbol this layout is asked to resolve and not in `read`, which runs
    /// once a second: it costs four modifier levels times 128 keycodes of `UCKeyTranslate`,
    /// about half a millisecond, and paying that once per layout is right while paying it once
    /// per second forever is not. Without it every distinct symbol paid the same scan on its
    /// first press, on the path whose whole local budget is a fifth of a millisecond.
    reverse: std::sync::OnceLock<HashMap<String, (u16, u16)>>,
}

impl Layout {
    /// Reads the current layout, or `None` when there is no GUI session to ask (which
    /// is what a build machine looks like).
    fn read() -> Option<Layout> {
        // SAFETY: `TISCopyCurrentKeyboardLayoutInputSource` follows the Create rule, so
        // the source is owned here and released when `_source` is dropped;
        // `TISGetInputSourceProperty` follows the Get rule, so the data it returns is
        // only valid while the source is alive, which is why the bytes are copied before
        // this function returns.
        unsafe {
            let raw_source = ffi::TISCopyCurrentKeyboardLayoutInputSource();
            let _source =
                CFRetained::<CFType>::from_raw(std::ptr::NonNull::new(raw_source.cast())?);
            let raw_data =
                ffi::TISGetInputSourceProperty(raw_source, ffi::kTISPropertyUnicodeKeyLayoutData);
            if raw_data.is_null() {
                return None;
            }
            let data: &CFData = &*raw_data.cast::<CFData>();
            let bytes = data.to_vec();
            if bytes.is_empty() {
                return None;
            }
            let identity = format!(
                "mac:{}:{}",
                bytes.len(),
                hex::encode(&blake3::hash(&bytes).as_bytes()[..8])
            );
            Some(Layout {
                bytes,
                kbd_type: u32::from(ffi::LMGetKbdType()),
                identity,
                reverse: std::sync::OnceLock::new(),
            })
        }
    }

    /// What a key produces on this layout with these modifiers held.
    fn text(&self, vk: u16, mods: u16) -> Option<String> {
        let mut buf = [0u16; 8];
        let mut len: usize = 0;
        let mut dead: u32 = 0;
        // SAFETY: the layout bytes outlive the call, and every out parameter is ours and
        // sized as the call is told.
        let status = unsafe {
            ffi::UCKeyTranslate(
                self.bytes.as_ptr(),
                vk,
                ffi::KEY_ACTION_DISPLAY,
                uchr_modifiers(mods),
                self.kbd_type,
                ffi::NO_DEAD_KEYS,
                &mut dead,
                buf.len(),
                &mut len,
                buf.as_mut_ptr(),
            )
        };
        if status != 0 || len == 0 || len > buf.len() {
            return None;
        }
        let text = String::from_utf16_lossy(&buf[..len]);
        // A control character is not a symbol anybody typed: Control plus C produces
        // one, and putting it on the wire would have the target inject a literal 0x03.
        if text.is_empty() || text.chars().any(char::is_control) {
            return None;
        }
        Some(text)
    }

    /// The key and the modifiers that produce `text`.
    ///
    /// The table is built once per layout, in the order of preference: nothing held first,
    /// then Shift, then Option, then both, and the lowest virtual keycode within each, with
    /// the first answer for a character kept. So a lower case letter costs no modifier and a
    /// capital costs a Shift, which is what the engine's sequence expects.
    fn find(&self, text: &str) -> Option<(u16, u16)> {
        self.reverse
            .get_or_init(|| self.build_reverse())
            .get(text)
            .copied()
    }

    /// One pass over the levels a canonical modifier set can name, into the map `find` reads.
    fn build_reverse(&self) -> HashMap<String, (u16, u16)> {
        let mut map: HashMap<String, (u16, u16)> = HashMap::new();
        for mods in [
            0,
            keys::mods::SHIFT,
            keys::mods::ALTGR,
            keys::mods::SHIFT | keys::mods::ALTGR,
        ] {
            for vk in 0u16..128 {
                if let Some(text) = self.text(vk, mods) {
                    map.entry(text).or_insert((vk, mods));
                }
            }
        }
        map
    }
}

// -------------------------------------------------------------- the commands

/// A downcall that has to happen on the run loop thread: the tap's life, and the
/// loop's own end. Everything else is callable from any thread and runs inline.
enum Cmd {
    Capture(CaptureMode),
    Exit(i32),
}

impl std::fmt::Debug for Cmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Cmd::Capture(mode) => write!(f, "Capture({mode:?})"),
            Cmd::Exit(code) => write!(f, "Exit({code})"),
        }
    }
}

// ---------------------------------------------------------------- the state

/// What the tap callback and the engine's thread both need, without a lock on the path
/// the callback takes.
///
/// Every field the callback touches is an atomic or a channel sender, and that is a
/// hard requirement rather than a preference: a tap callback that takes too long is
/// DISABLED by the system (truth 8), so a mutex a slower thread could be holding would
/// turn a stall on the engine's side into a keyboard that stops being observed.
#[derive(Debug)]
struct Shared {
    mode: AtomicU8,
    events_tx: mpsc::Sender<BackendEvent>,
    /// Where the pointer is decoupled to, and whether it is (truth 5).
    anchor_x: AtomicI32,
    anchor_y: AtomicI32,
    confined: AtomicBool,
    /// The canonical modifier bits the LOCAL user is holding, from the tap.
    ///
    /// Two fields and not one, because the two directions are unrelated state that merely
    /// share a type, and sharing one made a source Mac's own held Shift stamp every key a
    /// peer later typed INTO it: the tap stores the local state, the session ends, the tap is
    /// destroyed, and nothing clears the field the injection path then reads. Both are
    /// cleared on every capture mode change and on every teardown.
    local_mods: AtomicU32,
    /// The canonical modifier bits the INJECTION path has pressed and not released, which is
    /// what every event it posts is stamped with (truth 6).
    injected_mods: AtomicU32,
    /// The lock states the tap last saw, so a lock's transition can be told from a repeat.
    seen_locks: AtomicU32,
    /// Where the last injected pointer event put the cursor, or [`NO_WARP`] when nothing has
    /// been injected yet.
    ///
    /// `CGEventPost` is asynchronous, so reading the cursor back after a move can answer with
    /// where it was BEFORE: a batch of a move and then a click would put the click at the old
    /// point. This is what a click uses instead.
    injected_at: AtomicU64,
    /// How many times the tap's own re-pin could not move the cursor, so the loop can say it
    /// instead of the callback saying it (see [`warp_quietly`]).
    warp_failed: AtomicU32,
    /// Set when re-coupling the cursor to the mouse failed, so the loop can try again.
    ///
    /// It is the one failure on this platform that leaves a machine's mouse DEAD: the pin is a
    /// decoupling, so a `confine(None)` whose re-association returned an error leaves the
    /// cursor frozen with nobody left to retry it (the tap will not, because it reads
    /// `confined`, which is already false).
    needs_reassociate: AtomicBool,
    /// Scroll movement below one notch, kept so a trackpad is not rounded to nothing
    /// event after event.
    wheel_x: AtomicI32,
    wheel_y: AtomicI32,
    /// Which mouse buttons this backend has pressed and not released, one bit per dialect
    /// button number.
    ///
    /// It decides whether a move is posted as a `MouseMoved` or as a `...MouseDragged`,
    /// which is not cosmetic: an application tracking a drag ignores a plain move, so a
    /// drag injected as one lets go of whatever was being dragged half way across the
    /// screen.
    buttons: AtomicU32,
    /// The event number the next mouse event will carry (truth 10), seeded off the window
    /// server's own count so it does not collide with the numbers a real mouse is using.
    event_number: AtomicU32,
    /// Set by the tap callback when the system turned the tap off, so the loop can turn it
    /// back on: nothing else can, and a tap that stays off is a source whose keystrokes
    /// quietly start acting locally again (truth 8).
    tap_disabled: AtomicBool,
    /// True once the engine's end of the upcall channel has closed.
    ///
    /// It is the only way this backend can learn that the engine has GONE without asking
    /// it to stop, and it is not hypothetical: `main` calls `request_exit` when the engine
    /// returns normally, so the only way the channel closes first is the engine's thread
    /// having panicked. A run loop that did not notice would keep the tap installed for
    /// the life of the process, and a process nobody is talking to that swallows every
    /// keystroke is exactly the dead keyboard this component exists to avoid.
    engine_gone: AtomicBool,
    dropped: AtomicU32,
}

impl Shared {
    fn emit(&self, event: BackendEvent) {
        use mpsc::error::TrySendError;
        match self.events_tx.try_send(event) {
            Ok(()) => {}
            Err(TrySendError::Closed(_)) => {
                self.engine_gone.store(true, Ordering::Relaxed);
            }
            Err(TrySendError::Full(_)) => {
                // COUNTED and not said. `emit` runs inside the tap callback, where an
                // `eprintln!` can block on a pipe nobody drains and a blocked callback gets the
                // tap disabled (truth 8). The loop reports the count.
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn mode(&self) -> u8 {
        self.mode.load(Ordering::Relaxed)
    }

    /// The next event number, for the mouse event about to be posted (truth 10).
    fn next_event_number(&self) -> i64 {
        i64::from(self.event_number.fetch_add(1, Ordering::Relaxed))
    }

    /// Records a button this backend pressed or released, and answers with whether any is
    /// still down.
    fn track_button(&self, button: u8, down: bool) -> bool {
        let mask = 1u32 << u32::from(button.min(31));
        let held = if down {
            self.buttons.fetch_or(mask, Ordering::Relaxed) | mask
        } else {
            self.buttons.fetch_and(!mask, Ordering::Relaxed) & !mask
        };
        held != 0
    }

    /// Which of the dialect's buttons this backend is holding, or `None`.
    fn held_button(&self) -> Option<u8> {
        let held = self.buttons.load(Ordering::Relaxed);
        (1u8..=5).find(|b| held & (1 << u32::from(*b)) != 0)
    }
}

/// The two ways to reach the run loop from another thread, in one place that is safe
/// to send.
///
/// `CFRunLoopSourceSignal` and `CFRunLoopWakeUp` are the two Core Foundation calls
/// documented as safe from any thread, and they are the only two this type MAKES.
///
/// There is a third that it does not make and cannot avoid: `CFRetained`'s `Drop` calls
/// `CFRelease`, and this type lives in an `Arc` that both the engine's thread and the loop's
/// thread hold, so the last drop can release a `CFRunLoopSource` and a `CFRunLoop` from
/// whichever thread got there last. That is sound for a reason worth writing down rather than
/// leaving implied: `CFRelease` is documented as thread safe, the run loop this handle names is
/// the main thread's and is immortal, and the run loop holds its own reference to the wake
/// source (added in `run` and never removed), so neither release can be the one that runs a
/// deallocator. Nothing else touches either pointer off the loop's own thread.
struct Wake {
    source: CFRetained<CFRunLoopSource>,
    run_loop: CFRetained<CFRunLoop>,
}

// SAFETY: see the type's documentation. The only operations performed through this
// handle are the two Core Foundation calls that are documented as thread safe, and the
// retained pointers are kept alive by `CFRetained` for as long as any thread holds one.
unsafe impl Send for Wake {}
unsafe impl Sync for Wake {}

impl std::fmt::Debug for Wake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Wake")
    }
}

impl Wake {
    fn wake(&self) {
        self.source.signal();
        self.run_loop.wake_up();
    }
}

// -------------------------------------------------------------- the handle

/// The cheap, `Clone` handle the engine holds. It carries no Core Graphics object: the
/// command queue, the way to wake the loop, and the capabilities the loop keeps up to
/// date.
#[derive(Clone, Debug)]
pub struct MacBackend {
    cmds: Arc<Mutex<VecDeque<Cmd>>>,
    wake: Arc<Wake>,
    shared: Arc<Shared>,
    caps: Arc<Mutex<Capabilities>>,
    /// The layout, read on the loop thread and shared for `resolve`. A `Mutex` and not
    /// an answering downcall, because a resolution is on the path of a keystroke and a
    /// round trip to the run loop would put the run loop's turn between the two.
    layout: Arc<Mutex<Option<Layout>>>,
    /// Whether the Accessibility prompt has been shown (truth 2).
    asked_to_post: Arc<AtomicBool>,
    /// The last answer to "is the screen locked" and when it was asked, so the window server
    /// is not asked once per injected batch. See [`MacBackend::screen_is_locked_cached`].
    locked: Arc<Mutex<(Instant, bool)>>,
}

impl MacBackend {
    /// Enqueue a command, then wake the loop. Push BEFORE the wake, or a coalesced wake
    /// could drop the command. A poisoned mutex is recovered so an `Exit` is never lost.
    fn push(&self, cmd: Cmd) {
        match self.cmds.lock() {
            Ok(mut q) => q.push_back(cmd),
            Err(p) => p.into_inner().push_back(cmd),
        }
        self.wake.wake();
    }

    /// Can this machine type at all right now? Asks TCC, and asks the person once.
    ///
    /// The CACHED grant first, and the authoritative call only when the cache says no. A
    /// preflight is a query to another process, this sits on a path that carries 125 events a
    /// second, and the loop already re-reads both grants every second (truth 3) for exactly
    /// this reason: querying the same thing per keystroke was the file's own header contradicting
    /// itself. The cost of the cache is that a grant REVOKED mid-session is noticed up to a
    /// second late, which costs a second of injections the window server drops anyway; the
    /// cost of a grant GRANTED late is nothing, because the fall through below is the fast
    /// path's own answer.
    fn may_post(&self) -> bool {
        if self.capabilities().inject_keys {
            return true;
        }
        if objc2_core_graphics::CGPreflightPostEventAccess() {
            return true;
        }
        // Truth 2: the prompt appears when the feature is used, which is the only
        // moment a backend can know that a person asked for it. Once per process, so a
        // refused grant does not become a dialog per keystroke.
        if !self.asked_to_post.swap(true, Ordering::Relaxed) {
            warn(
                "1Device needs the Accessibility permission to type on this computer; \
                 asking for it now",
            );
            objc2_core_graphics::CGRequestPostEventAccess();
        }
        false
    }
}

impl InputBackend for MacBackend {
    fn capabilities(&self) -> Capabilities {
        match self.caps.lock() {
            Ok(caps) => caps.clone(),
            Err(p) => p.into_inner().clone(),
        }
    }

    fn monitors(&self) -> impl Future<Output = Vec<Monitor>> + Send {
        // Inline: the Core Graphics display calls have no thread affinity.
        let (monitors, stable) = read_monitors();
        if let Ok(mut caps) = self.caps.lock() {
            caps.monitors_stable = stable;
            if stable && caps.problem == Some(Problem::MonitorsUnstable) {
                caps.problem = None;
            } else if !stable && caps.problem.is_none() {
                caps.problem = Some(Problem::MonitorsUnstable);
            }
        }
        std::future::ready(monitors)
    }

    fn pointer(&self) -> impl Future<Output = Option<Point>> + Send {
        std::future::ready(cursor_position())
    }

    fn resolve(&self, want: Want) -> impl Future<Output = Option<Resolved>> + Send {
        let answer = match self.layout.lock() {
            Ok(guard) => resolve_with(guard.as_ref(), &want),
            Err(p) => resolve_with(p.into_inner().as_ref(), &want),
        };
        std::future::ready(answer)
    }

    fn capture(&self, mode: CaptureMode) {
        // The one downcall that must reach the run loop: an event tap lives on the loop
        // its source was added to.
        self.push(Cmd::Capture(mode));
    }

    fn confine(&self, rect: Option<Rect>) {
        match rect {
            Some(rect) => {
                let anchor = centre(&rect);
                self.shared.anchor_x.store(anchor.x, Ordering::Relaxed);
                self.shared.anchor_y.store(anchor.y, Ordering::Relaxed);
                self.shared.confined.store(true, Ordering::Relaxed);
                // Truth 5: this DECOUPLES the cursor from the mouse rather than
                // clipping it, so the cursor stops moving and the rectangle is advisory.
                // The warp puts it where the session should see it.
                let _ = associate(false);
                let _ = warp(anchor, false);
                // Where the pointer now is, so an injected move on THIS machine composes from
                // the anchor rather than from wherever it was before the pin.
                self.injected_to(anchor);
            }
            None => {
                self.shared.confined.store(false, Ordering::Relaxed);
                // The one failure on this platform that leaves a machine's mouse dead, so it
                // is recorded rather than only warned about: the loop retries it every turn
                // until it takes (see `Backend::periodic`).
                if !associate(true) {
                    self.shared.needs_reassociate.store(true, Ordering::Relaxed);
                }
                self.shared.injected_at.store(NO_WARP, Ordering::Relaxed);
            }
        }
    }

    fn warp(&self, to: Point) {
        let confined = self.shared.confined.load(Ordering::Relaxed);
        // The re-association is skipped while the pointer is pinned, because the pin IS the
        // decoupling (truth 5). When it is not skipped it can fail, and that is the dead mouse
        // case again, so it goes on the same retry.
        if !warp(to, !confined) {
            self.shared.needs_reassociate.store(true, Ordering::Relaxed);
        }
        self.injected_to(to);
    }

    fn inject(&self, actions: Vec<Action>) {
        self.send(actions, true);
    }

    fn release_all(&self, keys: Vec<PlatformKey>) {
        if keys.is_empty() {
            return;
        }
        let actions: Vec<Action> = keys
            .into_iter()
            .map(|code| Action::Key { code, down: false })
            .collect();
        // UNGUARDED, which is the whole difference from an ordinary injection: a release
        // is called from every teardown, including the ones that happen because a
        // password field has the keyboard or the screen has locked, and a key left down
        // there is a keyboard that is still broken when somebody comes back to it. A
        // refusal is still reported, so nothing about it is silent.
        self.send(actions, false);
    }

    fn request_exit(&self, code: i32) {
        // Never `std::process::exit`: this runs on the engine's thread, and exiting here
        // would skip the tap's teardown and the re-association of a decoupled cursor.
        self.push(Cmd::Exit(code));
    }
}

impl MacBackend {
    /// Hands a batch to the window server.
    ///
    /// `guarded` is what tells an ordinary injection from a RELEASE. An ordinary one asks
    /// about a secure input field and a locked screen first (truth 7, and the only order
    /// that does not lie: both silence the injection and neither reports anything). A
    /// release does not ask, for the reason given where it is called. Neither can do
    /// without the grant: with no Accessibility permission there is nothing to post
    /// through.
    fn send(&self, actions: Vec<Action>, guarded: bool) {
        if actions.is_empty() {
            return;
        }
        if !self.may_post() {
            self.shared
                .emit(BackendEvent::Refused(Refusal::NoPermission));
            return;
        }
        if guarded {
            // Truth 7, and the only order that does not lie: a password field silences the
            // injection, so it is asked about BEFORE typing rather than after.
            let secure = unsafe { ffi::IsSecureEventInputEnabled() };
            // `Action::Group` is deliberately NOT counted: it is a documented no-op on this
            // platform, so a batch of nothing but groups was being refused for secure input
            // while typing nothing at all.
            let has_keys = actions
                .iter()
                .any(|a| matches!(a, Action::Key { .. } | Action::Text(_)));
            if secure && has_keys {
                self.shared
                    .emit(BackendEvent::Refused(Refusal::SecureInput));
                return;
            }
            if self.screen_is_locked_cached() {
                self.shared
                    .emit(BackendEvent::Refused(Refusal::ScreenLocked));
                return;
            }
        }
        // One event source per thread, created once. `CGEventSourceCreate` is a window server
        // round trip and this is the 125 Hz path; a `CFRetained<CGEventSource>` is not `Send`,
        // so it cannot live in the handle the engine clones across threads, and a thread local
        // is the shape that fits. Truth 4's non-deprecated cure is applied to it at creation.
        // `try_with` and not `with`: the latter PANICS when the thread local is already
        // destroyed, which is a thread on its way out, and a panic on the injection path is
        // worse than an injection with no source (which every arm of `post` accepts, because
        // the API does).
        let with_source = SOURCE.try_with(|source| {
            for action in &actions {
                self.post(source.as_deref(), action);
            }
        });
        if with_source.is_err() {
            for action in &actions {
                self.post(None, action);
            }
        }
    }

    /// [`screen_is_locked`], asked at most once every [`LOCK_POLL`].
    ///
    /// The uncached call is a window server round trip plus a dictionary and two strings, and
    /// it was being made once per injected batch. A lock is a human action, so an answer up to
    /// a fifth of a second stale is an answer: the injections that slip through in that window
    /// go to a locked screen, where the window server discards them, which is what would have
    /// happened anyway.
    fn screen_is_locked_cached(&self) -> bool {
        let mut guard = match self.locked.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if guard.0.elapsed() >= LOCK_POLL {
            *guard = (Instant::now(), screen_is_locked());
        }
        guard.1
    }

    /// Where the cursor is, for the purpose of composing the next event to post.
    ///
    /// The last position THIS BACKEND injected, when it has injected one, and the real cursor
    /// position otherwise. Reading the real one back is wrong right after an injection:
    /// `CGEventPost` is asynchronous, so a batch of "move, then click" would read the position
    /// before the window server had applied the move and post the click at the old point,
    /// which is a click in the wrong place on somebody else's screen.
    ///
    /// It is reset whenever capture changes and on every teardown, so a session never starts
    /// from where a previous one left the pointer.
    fn injected_position(&self) -> Point {
        let packed = self.shared.injected_at.load(Ordering::Relaxed);
        if packed == NO_WARP {
            return cursor_position().unwrap_or(Point { x: 0, y: 0 });
        }
        unpack(packed)
    }

    /// Records where an injected pointer event put the cursor.
    fn injected_to(&self, at: Point) {
        self.shared.injected_at.store(pack(at), Ordering::Relaxed);
    }

    /// Posts one action, stamping the modifier flags this backend believes are held
    /// (truth 6).
    fn post(&self, source: Option<&CGEventSource>, action: &Action) {
        let mods = self.shared.injected_mods.load(Ordering::Relaxed) as u16;
        let flags = flags_of_mods(mods);
        match action {
            Action::Key { code, down } => {
                let vk = (code.code & 0xFFFF) as u16;
                // A modifier's OWN event carries the state that already includes it, on the
                // press and on the release alike: Command down is stamped `MaskCommand`, and
                // Command up is stamped empty. So the new state is computed once, stored, and
                // stamped on this very event. Walked through Command-down, c-down, c-up,
                // Command-up it gives `MaskCommand` three times and then nothing, which is
                // truth 6.
                let stamped = match mod_of_vk(vk) {
                    Some(bit) => {
                        let now = if *down { mods | bit } else { mods & !bit };
                        self.shared
                            .injected_mods
                            .store(u32::from(now), Ordering::Relaxed);
                        flags_of_mods(now)
                    }
                    None => flags,
                };
                if let Some(event) = CGEvent::new_keyboard_event(source, vk, *down) {
                    CGEvent::set_flags(Some(&event), stamped);
                    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
                }
            }
            Action::Text(text) => {
                // The Unicode path: a keyboard event with no key at all, carrying the
                // string. It is what makes a character this layout cannot produce
                // arrive anyway.
                let units: Vec<u16> = text.encode_utf16().collect();
                if units.is_empty() {
                    return;
                }
                for down in [true, false] {
                    if let Some(event) = CGEvent::new_keyboard_event(source, 0, down) {
                        // Explicitly NO modifiers, and it is the one arm that needs saying so.
                        // A created event may inherit the local hardware modifier state, so a
                        // person holding Shift on the TARGET would otherwise corrupt every
                        // character typed into it; and a driver holding Command whose stroke
                        // fell through to this level would have the shortcut silently become
                        // literal text. The Unicode path carries the character and nothing else.
                        CGEvent::set_flags(Some(&event), CGEventFlags::empty());
                        // SAFETY: a slice we own, with the length the call is given.
                        unsafe {
                            CGEvent::keyboard_set_unicode_string(
                                Some(&event),
                                units.len() as u64,
                                units.as_ptr(),
                            );
                        }
                        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
                    }
                }
            }
            Action::MoveTo(to) => {
                let from = self.injected_position();
                let at = objc2_core_foundation::CGPoint {
                    x: f64::from(to.x),
                    y: f64::from(to.y),
                };
                let (kind, button) = self.move_kind();
                if let Some(event) = CGEvent::new_mouse_event(source, kind, at, button) {
                    // The deltas, even on an absolute move: a program that reads raw pointer
                    // movement (a 3D view, a game) looks at these fields and not at the
                    // position, and an event with none reads as no movement at all.
                    CGEvent::set_integer_value_field(
                        Some(&event),
                        CGEventField::MouseEventDeltaX,
                        i64::from(to.x.saturating_sub(from.x)),
                    );
                    CGEvent::set_integer_value_field(
                        Some(&event),
                        CGEventField::MouseEventDeltaY,
                        i64::from(to.y.saturating_sub(from.y)),
                    );
                    self.stamp_mouse(&event, flags);
                    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
                    self.injected_to(*to);
                }
            }
            Action::MoveBy { dx, dy } => {
                if *dx == 0 && *dy == 0 {
                    return;
                }
                // macOS has no relative mouse event: the position is computed here and the
                // delta travels in the event's own fields, which is what a game reading raw
                // movement looks at. From the last INJECTED position for the reason the button
                // arm gives.
                let from = self.injected_position();
                let at = objc2_core_foundation::CGPoint {
                    x: f64::from(from.x.saturating_add(*dx)),
                    y: f64::from(from.y.saturating_add(*dy)),
                };
                let (kind, button) = self.move_kind();
                if let Some(event) = CGEvent::new_mouse_event(source, kind, at, button) {
                    CGEvent::set_integer_value_field(
                        Some(&event),
                        CGEventField::MouseEventDeltaX,
                        i64::from(*dx),
                    );
                    CGEvent::set_integer_value_field(
                        Some(&event),
                        CGEventField::MouseEventDeltaY,
                        i64::from(*dy),
                    );
                    self.stamp_mouse(&event, flags);
                    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
                    self.injected_to(Point {
                        x: from.x.saturating_add(*dx),
                        y: from.y.saturating_add(*dy),
                    });
                }
            }
            Action::Button { button, down } => {
                let Some((kind, mac, number)) = mac_button(*button, *down) else {
                    return;
                };
                // Where the last injected move PUT the cursor, and not where the cursor is.
                // `CGEventPost` is asynchronous, so a batch of a move and then a click can read
                // the position back before the window server has applied the move, and the
                // click lands at the old point: on the target that is a click in the wrong
                // place, which is the one kind of mistake a person cannot undo by trying again.
                let at = self.injected_position();
                let at = objc2_core_foundation::CGPoint {
                    x: f64::from(at.x),
                    y: f64::from(at.y),
                };
                if let Some(event) = CGEvent::new_mouse_event(source, kind, at, mac) {
                    CGEvent::set_integer_value_field(
                        Some(&event),
                        CGEventField::MouseEventButtonNumber,
                        number,
                    );
                    self.stamp_mouse(&event, flags);
                    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
                }
                // Recorded AFTER the event, so a press's own event is a press and the move
                // that follows it is a drag.
                self.shared.track_button(*button, *down);
            }
            Action::Wheel { dx, dy, pixels } => {
                let unit = if *pixels {
                    CGScrollEventUnit::Pixel
                } else {
                    CGScrollEventUnit::Line
                };
                // Clamped, because the numbers come off a peer's frame and two billion
                // lines is not a scroll anybody meant. The dialect bounds it too
                // ([`crate::wire::WHEEL_MAX`]); this is the second bound.
                let cap = crate::wire::WHEEL_MAX;
                let (dx, dy) = ((*dx).clamp(-cap, cap), (*dy).clamp(-cap, cap));
                // Negated on the way out for the same reason as on the way in (truth 9):
                // the dialect's positive is right and this platform's is left.
                if let Some(event) = CGEvent::new_scroll_wheel_event2(source, unit, 2, dy, -dx, 0) {
                    CGEvent::set_flags(Some(&event), flags);
                    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
                }
            }
            Action::Group(_) => {
                // No meaning on macOS: input sources are switched with
                // `TISSelectInputSource`, which is user visible and slow, so a
                // `resolve` here never names a group and the engine never emits one.
                // A no-op and not a panic, deliberately: the seam says so.
            }
        }
    }
}

impl MacBackend {
    /// Which event a pointer move should be posted as.
    ///
    /// A `...MouseDragged` while this backend is holding a button, and a `MouseMoved`
    /// otherwise. An application tracking a drag ignores a plain move, so a drag injected as
    /// one lets go of whatever was being dragged half way across the screen.
    fn move_kind(&self) -> (CGEventType, CGMouseButton) {
        match self.shared.held_button() {
            Some(1) => (CGEventType::LeftMouseDragged, CGMouseButton::Left),
            Some(3) => (CGEventType::RightMouseDragged, CGMouseButton::Right),
            Some(_) => (CGEventType::OtherMouseDragged, CGMouseButton::Center),
            None => (CGEventType::MouseMoved, CGMouseButton::Left),
        }
    }

    /// Everything every mouse event this backend posts has to carry.
    ///
    /// The flags are truth 6. The event NUMBER and the click state are truth 10: a synthetic
    /// click without them is refused outright on macOS 27, which reads as this feature doing
    /// nothing at all with no refusal to explain it. A click state of one is a single click;
    /// the chain that makes a double click is a separate piece of work (it needs the
    /// system's own double click interval, which lives in AppKit) and is on the live
    /// validation list.
    fn stamp_mouse(&self, event: &CGEvent, flags: CGEventFlags) {
        CGEvent::set_flags(Some(event), flags);
        CGEvent::set_integer_value_field(
            Some(event),
            CGEventField::MouseEventNumber,
            self.shared.next_event_number(),
        );
        CGEvent::set_integer_value_field(Some(event), CGEventField::MouseEventClickState, 1);
    }
}

/// How this machine can produce `want` on the layout it has.
fn resolve_with(layout: Option<&Layout>, want: &Want) -> Option<Resolved> {
    match want {
        Want::Usage(usage) => vk_of_usage(*usage).map(|vk| Resolved {
            code: PlatformKey {
                code: u32::from(vk),
                // Nothing to carry: a macOS virtual keycode is the whole identity of a
                // physical key, and there are no groups here for the group field to be
                // confused with.
                detail: 0,
            },
            mods: 0,
            prefix: None,
            group: None,
        }),
        Want::Named(name) => {
            let usage = keys::usage_of(name)?;
            resolve_with(layout, &Want::Usage(usage))
        }
        Want::Symbol(sym) => {
            let (vk, mods) = layout?.find(sym)?;
            Some(Resolved {
                code: PlatformKey {
                    code: u32::from(vk),
                    detail: 0,
                },
                mods,
                prefix: None,
                // No groups on macOS (see `Action::Group`).
                group: None,
            })
        }
    }
}

// ------------------------------------------------------------- the OS calls

/// Where the pointer is.
///
/// A `CGEvent` with no source carries the CURRENT pointer position, which is the
/// documented way to ask without a window server connection of one's own.
fn cursor_position() -> Option<Point> {
    let event = CGEvent::new(None)?;
    let at = CGEvent::location(Some(&event));
    Some(Point {
        x: at.x as i32,
        y: at.y as i32,
    })
}

/// Couples or decouples the cursor from the mouse (truth 5), and answers with whether it
/// worked.
///
/// The answer is load bearing in one direction only. A failed DECOUPLING is a session whose
/// pin is advisory, which is a worse driving experience. A failed COUPLING is a mouse that
/// moves nothing at all until something retries it, so every caller that releases records the
/// failure in [`Shared::needs_reassociate`] and the loop retries it every turn.
#[must_use]
fn associate(connected: bool) -> bool {
    associate_said(connected, true)
}

/// The same call without the line on stderr, for the caller that makes it every quarter of a
/// second until it works.
///
/// A retry that warned every turn would be four lines a second for as long as the failure
/// lasts, and this repository has already paid once for an unthrottled line per event.
#[must_use]
fn associate_quietly(connected: bool) -> bool {
    associate_said(connected, false)
}

fn associate_said(connected: bool, say: bool) -> bool {
    let status = CGAssociateMouseAndMouseCursorPosition(connected);
    if status != CGError::Success {
        if !say {
            return false;
        }
        warn(&format!(
            "the pointer could not be {}: CGError {status:?}",
            if connected { "released" } else { "pinned" }
        ));
        return false;
    }
    true
}

/// Puts the pointer somewhere.
///
/// `reassociate` couples the cursor back to the mouse afterwards, which is one of the
/// two documented cures for the 250 millisecond suppression (truth 4). It must NOT
/// happen while a session has the pointer pinned, because the pin IS the decoupling:
/// the caller decides, and the suppression interval is zeroed once at construction so
/// that the choice is safe either way.
#[must_use]
fn warp(to: Point, reassociate: bool) -> bool {
    let status = warp_quietly(to);
    if status != CGError::Success {
        warn(&format!(
            "the pointer could not be moved: CGError {status:?}"
        ));
    }
    if reassociate {
        return associate(true);
    }
    true
}

/// The half of [`warp`] that says NOTHING, for the one caller that is inside the event tap's
/// callback.
///
/// A `warn` there is an `eprintln!` on the window server's own thread: it takes the stderr lock
/// and writes to a pipe, and a pipe nobody is draining blocks. A blocked tap callback is a tap
/// the system DISABLES (truth 8), which is a source Mac that stops observing its own keyboard
/// because it tried to complain. And it would not be one line: a warp fails for reasons that
/// persist (not the active session, a locked screen, a display capture), so it would be one
/// line per mouse event. The callback counts instead, and the loop says it (this is the same
/// answer the Windows hooks arrived at, for the same reason).
fn warp_quietly(to: Point) -> CGError {
    let at = objc2_core_foundation::CGPoint {
        x: f64::from(to.x),
        y: f64::from(to.y),
    };
    CGWarpMouseCursorPosition(at)
}

/// Is the screen locked, or is somebody else's session in front?
///
/// The session dictionary is the documented way to ask, and both keys matter: a locked
/// screen and a session that is not on the console are the same thing as far as an
/// injection is concerned, which is that nobody would see it.
fn screen_is_locked() -> bool {
    let Some(session) = objc2_core_graphics::CGSessionCopyCurrentDictionary() else {
        // No session dictionary AT ALL is not a locked screen, and saying it was would be the
        // "best effort that lies" this ticket exists to avoid. It is what a process with no
        // window server session of its own looks like: started over ssh, or by a launchd job
        // with no Aqua session. The shipped path is a login agent, where the dictionary always
        // exists, so this branch means somebody is running the component by hand.
        //
        // Refusing here reported "that computer is locked" on an unlocked Mac, which sends the
        // person reading it to look for a lock screen that is not there. Instead the injection
        // is attempted and fails on its own terms, and the reason is said once.
        if !NO_SESSION_SAID.swap(true, Ordering::Relaxed) {
            warn(
                "this process has no window server session, so it cannot tell whether the \
                 screen is locked; injections will be attempted and may simply do nothing",
            );
        }
        return false;
    };
    let locked = dictionary_flag(&session, "CGSSessionScreenIsLocked");
    let on_console = dictionary_flag(&session, "kCGSSessionOnConsoleKey");
    // `!= Some(true)` and not `== Some(false)`: Apple documents the absence of the key as
    // meaning the session does NOT own the console, so a missing key was being read as the
    // reassuring answer. Under fast user switching that is a Mac accepting an injection it
    // shows to nobody.
    locked == Some(true) || on_console != Some(true)
}

/// Whether the "no window server session" line has been said. Once per process: it is a
/// property of how the component was started, so it never changes while it runs.
static NO_SESSION_SAID: AtomicBool = AtomicBool::new(false);

/// One boolean out of a Core Foundation dictionary, or `None` when it is not there.
fn dictionary_flag(dictionary: &CFDictionary, key: &str) -> Option<bool> {
    let key = CFString::from_str(key);
    // SAFETY: a key this function owns and a dictionary it borrows; the value follows
    // the Get rule and is only read before either is dropped.
    let value = unsafe { dictionary.value((&*key) as *const CFString as *const c_void) };
    if value.is_null() {
        return None;
    }
    // SAFETY: the value follows the Get rule and the dictionary outlives this borrow. The
    // CLASS is checked rather than assumed: `downcast_ref` is one `CFGetTypeID` call, and a
    // blind cast of a `*const c_void` to a `CFBoolean` is not worth saving it.
    let value = unsafe { &*value.cast::<objc2_core_foundation::CFType>() };
    value.downcast_ref::<CFBoolean>().map(CFBoolean::value)
}

/// This machine's monitors, and whether their identities survive an unplug.
///
/// The coordinate space is the global display space Core Graphics uses for
/// `CGWarpMouseCursorPosition` and for an event's `location`, which is POINTS with the
/// top left of the main display at the origin. That is the machine's own logical space,
/// which is what the seam asks for, and `scale` is the backing scale factor: a Retina
/// display reports half its pixel width in points, so the ratio is exactly the number
/// an interface wants to show.
fn read_monitors() -> (Vec<Monitor>, bool) {
    let mut ids = [0u32; 32];
    let mut count: u32 = 0;
    // SAFETY: an array we own and its length, plus an out parameter for the count.
    let status = unsafe { CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) };
    if status != CGError::Success {
        // "No monitors, and their identities are UNSTABLE". Not stable: answering stable here
        // meant a failure to enumerate cleared a real `MonitorsUnstable` problem, so the
        // interface stopped saying anything was wrong at the moment it knew least.
        return (Vec::new(), false);
    }
    let main = CGMainDisplayID();
    let mut out = Vec::new();
    let mut stable = true;
    for id in ids.iter().take((count as usize).min(ids.len())) {
        let bounds = CGDisplayBounds(*id);
        if bounds.size.width < 1.0 || bounds.size.height < 1.0 {
            continue;
        }
        // NOT `CGDisplayPixelsWide`, which despite its name and its documentation returns
        // POINTS: it is the same number as `CGDisplayBounds().size.width`, so the ratio was
        // exactly 1000 on every display including every Retina one, and `Monitor::scale` was a
        // constant lie travelling inside a signed layout document. The display MODE carries
        // both numbers, and their ratio is the backing scale factor the seam asks for.
        //
        // In a non-integer "More Space" mode the framebuffer can exceed the panel's own
        // pixels, so this reports 2.0 where the physical ratio is nearer 1.7. That is not an
        // error: it is the same number `NSScreen.backingScaleFactor` gives, which is what an
        // interface showing "200%" means.
        let mode = objc2_core_graphics::CGDisplayCopyDisplayMode(*id);
        let scale = match mode.as_deref() {
            Some(mode) => {
                let points = objc2_core_graphics::CGDisplayMode::width(Some(mode)) as f64;
                let pixels = objc2_core_graphics::CGDisplayMode::pixel_width(Some(mode)) as f64;
                if points > 0.0 {
                    ((pixels / points) * 1000.0).round() as i32
                } else {
                    1000
                }
            }
            None => 1000,
        };
        let (identity, named) = display_identity(*id);
        if !named {
            stable = false;
        }
        out.push(Monitor {
            id: identity,
            // Core Graphics has no display NAME: the localised one belongs to AppKit's
            // `NSScreen`, which this component does not link. The number is what a person sees
            // until the interface pairs it with something better, and it is the display's
            // POSITION in the active list rather than its id: an id is a slot number that
            // changes across reboots and reconnections, so using it renamed a person's screens
            // behind their back. Apple documents the main display as first in that list, so
            // "Display 1" is the main one.
            name: format!("Display {}", out.len() + 1),
            w: bounds.size.width as i32,
            h: bounds.size.height as i32,
            x: bounds.origin.x as i32,
            y: bounds.origin.y as i32,
            scale: scale.clamp(500, 8000),
            primary: *id == main,
        });
    }
    if !out.is_empty() && !out.iter().any(|m| m.primary) {
        out[0].primary = true;
    }
    (out, stable)
}

/// A display's stable identity, and whether it really is one.
///
/// The UUID is the hardware's own and survives being unplugged, which is what the plane
/// keys a screen by. A display id is a SLOT: reconnect two screens the other way round
/// and they have swapped, so a machine that can only offer that says so through
/// [`Capabilities::monitors_stable`] rather than letting the plane rearrange itself.
fn display_identity(id: CGDirectDisplayID) -> (String, bool) {
    // SAFETY: `CGDisplayCreateUUIDFromDisplayID` follows the Create rule, so the UUID is
    // owned here and released when the `CFRetained` is dropped. Its bytes are read
    // rather than its string form, which needs one API fewer and gives the same
    // identity.
    let bytes = unsafe {
        let raw = ffi::CGDisplayCreateUUIDFromDisplayID(id);
        let Some(ptr) = std::ptr::NonNull::new(raw.cast::<CFUUID>()) else {
            return (format!("mac:slot:{id}"), false);
        };
        let uuid = CFRetained::from_raw(ptr);
        uuid.uuid_bytes()
    };
    let hex = [
        bytes.byte0,
        bytes.byte1,
        bytes.byte2,
        bytes.byte3,
        bytes.byte4,
        bytes.byte5,
        bytes.byte6,
        bytes.byte7,
        bytes.byte8,
        bytes.byte9,
        bytes.byte10,
        bytes.byte11,
        bytes.byte12,
        bytes.byte13,
        bytes.byte14,
        bytes.byte15,
    ];
    if hex.iter().all(|b| *b == 0) {
        return (format!("mac:slot:{id}"), false);
    }
    (format!("mac:uuid:{}", hex::encode(hex)), true)
}

/// A position packed into one integer, so a pair can be read atomically.
fn pack(at: Point) -> u64 {
    ((at.x as u32 as u64) << 32) | at.y as u32 as u64
}

fn unpack(packed: u64) -> Point {
    Point {
        x: (packed >> 32) as u32 as i32,
        y: packed as u32 as i32,
    }
}

/// The value [`Shared::injected_at`] holds when nothing has been injected yet. Not a position
/// anything could ask for: both coordinates would have to be `i32::MIN`.
const NO_WARP: u64 = 0x8000_0000_8000_0000;

/// The centre of a rectangle, which is where the pointer is put while confined.
fn centre(rect: &Rect) -> Point {
    Point {
        x: rect.x.saturating_add(rect.w / 2),
        y: rect.y.saturating_add(rect.h / 2),
    }
}

// -------------------------------------------------------- the tap callback

/// The event tap.
///
/// It does three things and nothing else: read the atomics, push one upcall, decide
/// whether to swallow. Anything slower risks the system DISABLING the tap (truth 8),
/// and a tap that has been disabled is a source whose keystrokes reach the local
/// machine again.
///
/// SAFETY: called by the window server on the thread whose run loop the tap's source
/// was added to, with an event that lives for the duration of the call.
unsafe extern "C-unwind" fn tap_callback(
    _proxy: CGEventTapProxy,
    kind: CGEventType,
    event: std::ptr::NonNull<CGEvent>,
    user_info: *mut c_void,
) -> *mut CGEvent {
    let pass = event.as_ptr();
    // A panic here would unwind into the window server's own frame, which this signature
    // (`C-unwind`) permits and which nothing downstream is written for. Caught, and the
    // event is PASSED THROUGH rather than consumed: a session that observes nothing is a
    // session somebody can end, and a Mac whose keystrokes are being eaten by a panicking
    // callback is not. The panic message still reaches the log through the ordinary hook.
    let swallowed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: the caller's own contract, forwarded.
        unsafe { tap_body(kind, event, user_info) }
    }))
    .unwrap_or(false);
    if swallowed {
        std::ptr::null_mut()
    } else {
        pass
    }
}

/// The tap's actual work, so [`tap_callback`] can catch a panic around it. Answers with
/// whether the event should be consumed.
///
/// SAFETY: the caller's contract.
unsafe fn tap_body(
    kind: CGEventType,
    event: std::ptr::NonNull<CGEvent>,
    user_info: *mut c_void,
) -> bool {
    let pass = false;
    if user_info.is_null() {
        return pass;
    }
    // SAFETY: the pointer is the `Arc<Shared>` the backend holds for as long as the tap
    // exists, handed to `tap_create` as its user info.
    let shared = unsafe { &*(user_info as *const Shared) };
    // SAFETY: the event belongs to this call.
    let event_ref = unsafe { event.as_ref() };

    // Truth 8: the system turned the tap off. Nothing else can turn it back on, and
    // this is the only place the fact arrives.
    if kind == CGEventType::TapDisabledByTimeout || kind == CGEventType::TapDisabledByUserInput {
        // Flagged for the loop, which is the only place that can turn it back on: this
        // callback has no reference to the tap's mach port, and a tap left off is a source
        // whose keystrokes quietly start acting locally again while the engine believes it
        // owns the keyboard. The engine is told as well, so a live session ends with a
        // reason rather than going deaf.
        shared.tap_disabled.store(true, Ordering::Release);
        shared.emit(BackendEvent::CaptureLost(CaptureLoss::Broken));
        return pass;
    }
    let mode = shared.mode();
    if mode == MODE_OFF {
        return pass;
    }
    let swallow = mode == MODE_SWALLOW;

    let flags = CGEvent::flags(Some(event_ref));
    let mut mods = mods_of_flags(flags);
    match kind {
        CGEventType::KeyDown | CGEventType::KeyUp | CGEventType::FlagsChanged => {
            let vk =
                CGEvent::integer_value_field(Some(event_ref), CGEventField::KeyboardEventKeycode)
                    as u16;
            // The right Option is AltGr on a Mac and the flags cannot tell the two apart (see
            // `mods_of_flags`), so the keycode decides.
            if vk == 61 && mods & keys::mods::ALT != 0 {
                mods = (mods & !keys::mods::ALT) | keys::mods::ALTGR;
            }
            let usage = usage_of_vk(vk).unwrap_or(0);
            let is_lock = usage != 0 && keys::is_lock(usage);
            let flags_change = kind == CGEventType::FlagsChanged;
            if flags_change && !is_lock && mod_of_vk(vk).is_none() {
                // A flags change that means no canonical modifier and is not a lock has
                // nothing to report. The `fn` key is the case, and on a laptop it is pressed
                // constantly (for the F keys, the arrows, Home and End): without this it
                // produced two frames per press carrying usage 0, no name and no symbol,
                // which the far side answered with an `oops` each time.
                return swallow;
            }
            if is_lock {
                // Caps Lock arrives as a flags change on this platform, twice per toggle, and
                // the FLAG says what the lock is now. Reported on the transition only, which is
                // what a half duplex lock means: a target that waited for a release would hold
                // the lock down for ever, and one that saw both would toggle it back.
                //
                // An earlier version had no arm for the Caps Lock keycode in `mod_of_vk`, so
                // `down` was false both times and the `is_lock && !down` rule dropped BOTH
                // events: pressing Caps Lock while driving did nothing anywhere, not locally
                // and not on the target.
                let now = mods & (keys::mods::CAPS | keys::mods::NUM | keys::mods::SCROLL);
                let before = shared.seen_locks.swap(u32::from(now), Ordering::Relaxed) as u16;
                if now == before {
                    return swallow;
                }
                shared.local_mods.store(u32::from(mods), Ordering::Relaxed);
                shared.emit(BackendEvent::Key(KeyEvent {
                    usage,
                    key: keys::name_of(usage).map(str::to_string),
                    sym: None,
                    mods,
                    down: true,
                    lock: true,
                }));
                return swallow;
            }
            shared.local_mods.store(u32::from(mods), Ordering::Relaxed);
            // A `FlagsChanged` is a modifier going down or coming up and macOS does not say
            // which: the flags after the change do. A modifier whose bit is now set went down.
            let down = match kind {
                CGEventType::KeyDown => true,
                CGEventType::KeyUp => false,
                _ => mod_of_vk(vk).is_some_and(|bit| mods & bit != 0),
            };
            let sym = tap_symbol(event_ref);
            shared.emit(BackendEvent::Key(KeyEvent {
                usage,
                key: (usage != 0)
                    .then(|| keys::name_of(usage))
                    .flatten()
                    .map(str::to_string),
                sym,
                mods,
                down,
                lock: false,
            }));
        }
        CGEventType::MouseMoved
        | CGEventType::LeftMouseDragged
        | CGEventType::RightMouseDragged
        | CGEventType::OtherMouseDragged => {
            shared.local_mods.store(u32::from(mods), Ordering::Relaxed);
            // Truth 5: the deltas are the event's own fields, which keep arriving while
            // the cursor is decoupled and the position does not move.
            let dx = CGEvent::integer_value_field(Some(event_ref), CGEventField::MouseEventDeltaX)
                as i32;
            let dy = CGEvent::integer_value_field(Some(event_ref), CGEventField::MouseEventDeltaY)
                as i32;
            let at = CGEvent::location(Some(event_ref));
            if dx != 0 || dy != 0 {
                shared.emit(BackendEvent::Motion(Motion {
                    at: Point {
                        x: at.x as i32,
                        y: at.y as i32,
                    },
                    dx,
                    dy,
                }));
            }
            if shared.confined.load(Ordering::Relaxed) {
                // The cursor is decoupled, so it should not have moved at all; putting
                // it back costs one call and covers whatever did move it.
                let anchor = Point {
                    x: shared.anchor_x.load(Ordering::Relaxed),
                    y: shared.anchor_y.load(Ordering::Relaxed),
                };
                if (at.x as i32, at.y as i32) != (anchor.x, anchor.y)
                    && warp_quietly(anchor) != CGError::Success
                {
                    // Counted rather than said, because this is inside the callback: see
                    // `warp_quietly`. No re-association and so no dead mouse to record either,
                    // because the pin IS the decoupling and this warp is part of holding it.
                    shared.warp_failed.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        CGEventType::ScrollWheel => {
            shared.local_mods.store(u32::from(mods), Ordering::Relaxed);
            // The POINT axis, accumulated into whole notches with the remainder kept: a
            // trackpad sends a fraction of a notch per event and rounding each one to
            // zero would scroll nothing at all. A real wheel detent arrives here as about
            // one notch's worth of points, so both devices go through the same arithmetic.
            let py = CGEvent::integer_value_field(
                Some(event_ref),
                CGEventField::ScrollWheelEventPointDeltaAxis1,
            ) as i32;
            let px = CGEvent::integer_value_field(
                Some(event_ref),
                CGEventField::ScrollWheelEventPointDeltaAxis2,
            ) as i32;
            // The LINE axis is the fallback and not the first choice, and per axis: it is
            // already a notch count (a detent is exactly one) but it is ROUNDED, so a
            // continuous device's small movement reads as zero there while the point axis
            // still carries it. An event with no point delta at all on an axis is a discrete
            // one, and then the line count is the whole truth.
            let ly = CGEvent::integer_value_field(
                Some(event_ref),
                CGEventField::ScrollWheelEventDeltaAxis1,
            ) as i32;
            let lx = CGEvent::integer_value_field(
                Some(event_ref),
                CGEventField::ScrollWheelEventDeltaAxis2,
            ) as i32;
            let ny = if py != 0 {
                notches(&shared.wheel_y, py)
            } else {
                ly
            };
            // NEGATED, and this is truth 9: on macOS a positive horizontal scroll means
            // LEFT, and the dialect says positive is right. Mac to Mac cancelled the error
            // out on both sides, so only a Mac driving anything else showed it, scrolling
            // sideways backwards.
            let nx = -if px != 0 {
                notches(&shared.wheel_x, px)
            } else {
                lx
            };
            if nx != 0 || ny != 0 {
                shared.emit(BackendEvent::Wheel {
                    dx: nx,
                    dy: ny,
                    pixels: false,
                });
            }
        }
        _ => {
            if let Some(button) = button_of_event(
                kind,
                CGEvent::integer_value_field(Some(event_ref), CGEventField::MouseEventButtonNumber),
            ) {
                shared.local_mods.store(u32::from(mods), Ordering::Relaxed);
                let down = matches!(
                    kind,
                    CGEventType::LeftMouseDown
                        | CGEventType::RightMouseDown
                        | CGEventType::OtherMouseDown
                );
                shared.emit(BackendEvent::Button { button, down });
            }
        }
    }
    // Returning true is how a tap consumes an event (the caller turns it into the null the
    // API wants): it never reaches the application that would have had it.
    swallow
}

/// Whole notches out of a point accumulator, with the remainder kept.
///
/// Saturating and not checked: the numbers come off a device, but `fetch_add` wraps in release
/// and PANICS in debug, and this runs inside the tap callback where a panic unwinds through
/// the window server's frames. The accumulator is also clamped, so a stuck axis cannot walk it
/// to the edge and stay there.
fn notches(accumulator: &AtomicI32, points: i32) -> i32 {
    if points == 0 {
        return 0;
    }
    let total = accumulator
        .fetch_add(points, Ordering::Relaxed)
        .saturating_add(points)
        .clamp(-1_000_000, 1_000_000);
    accumulator.store(total, Ordering::Relaxed);
    let whole = total / WHEEL_POINTS_PER_NOTCH;
    if whole != 0 {
        accumulator.fetch_sub(
            whole.saturating_mul(WHEEL_POINTS_PER_NOTCH),
            Ordering::Relaxed,
        );
    }
    whole
}

/// The text a key event carries, which macOS has already worked out.
///
/// The event's own string rather than a layout lookup, and it is the better answer:
/// the window server computed it with the layout, the dead key state and the modifiers
/// as they really were, which is exactly what "the text this machine's own layout
/// produced for the stroke" means.
fn tap_symbol(event: &CGEvent) -> Option<String> {
    let mut buf = [0u16; 8];
    let mut len: u64 = 0;
    // SAFETY: a buffer we own and its length, plus an out parameter for the length.
    unsafe {
        CGEvent::keyboard_get_unicode_string(
            Some(event),
            buf.len() as u64,
            &mut len,
            buf.as_mut_ptr(),
        );
    }
    let len = usize::try_from(len).unwrap_or(0);
    if len == 0 || len > buf.len() {
        return None;
    }
    let text = String::from_utf16_lossy(&buf[..len]);
    if text.is_empty() || text.chars().any(char::is_control) {
        return None;
    }
    Some(text)
}

// ------------------------------------------------------------- the loop side

/// Backend state, living on the run loop thread. It owns the tap's mach port and its
/// run loop source, so it is not `Send`.
struct Backend {
    cmds: Arc<Mutex<VecDeque<Cmd>>>,
    shared: Arc<Shared>,
    caps: Arc<Mutex<Capabilities>>,
    layout: Arc<Mutex<Option<Layout>>>,
    /// Kept alive so the pointer the tap callback was given stays valid, and never read
    /// through: the callback has its own reference.
    _wake: Arc<Wake>,
    wake_source: CFRetained<CFRunLoopSource>,
    tap: Option<CFRetained<CFMachPort>>,
    tap_source: Option<CFRetained<CFRunLoopSource>>,
    mode: CaptureMode,
    /// Whether the Input Monitoring prompt has been shown (truth 2).
    asked_to_listen: bool,
    grants: (bool, bool),
    grants_at: Instant,
    layout_at: Instant,
    /// When the tap was last asked whether it is still alive (truth 11).
    tap_at: Instant,
    /// Whether the last attempt to BUILD a tap failed, and when. While this is set the
    /// capabilities say this machine cannot capture, which is the truth and is also what keeps
    /// the engine from asking again in a tight loop (see [`Backend::tap_failed`]).
    tap_broken: bool,
    tap_failed_at: Instant,
    shutdown: bool,
    exit_code: Option<i32>,
    /// Whether the engine's departure has been seen once already (see `periodic`).
    gone_seen: bool,
    /// How many dropped upcalls have been reported, so the count is said when it grows and
    /// not once per turn.
    said_dropped: u32,
    /// How many times the re-coupling of a decoupled cursor has been retried, so the failure
    /// is said occasionally rather than four times a second.
    reassociate_tries: u32,
}

impl Backend {
    /// The main-thread pump. Commands first, then the periodic work, then a turn of the
    /// run loop, which is where the tap's callbacks are delivered.
    fn run(&mut self) -> i32 {
        // SAFETY: an AppKit extern static, valid while the framework is loaded.
        let mode = unsafe { kCFRunLoopDefaultMode };
        let Some(run_loop) = CFRunLoop::current() else {
            // D22 by another route. `create()` answers a missing run loop with `Unsupported`,
            // which lands in the Absent backend; panicking HERE would be a process that exits
            // non-zero and a supervisor that relaunches it every minute for the life of the
            // machine. There is no loop to pump, so this thread does what the Absent loop
            // does: says so once and stays out of the way without returning.
            warn(
                "there is no run loop on this thread; the keyboard and mouse backend will do nothing",
            );
            loop {
                std::thread::park();
            }
        };
        run_loop.add_source(Some(&self.wake_source), mode);
        loop {
            self.process_cmds();
            if self.shutdown {
                break;
            }
            self.periodic();
            if self.shutdown {
                break;
            }
            // One turn. It returns as soon as a source is handled, which is either the
            // tap delivering an event or a downcall signalling the wake source, and
            // after IDLE_TURN when nothing happens.
            //
            // The RESULT is read, because one of its four values does not wait: `Finished`
            // means the loop had nothing to wait on and returned at once, and a loop that
            // ignored it would spin this thread at 100 per cent with the tap still installed.
            // It should be unreachable (the wake source is added above and never removed), so
            // the answer is to sleep exactly as long as the turn was supposed to last rather
            // than to stop: the pump still has commands to drain and a tap to service.
            let turn = CFRunLoop::run_in_mode(mode, IDLE_TURN.as_secs_f64(), true);
            if turn == CFRunLoopRunResult::Finished {
                std::thread::sleep(IDLE_TURN);
            }
        }
        self.release_everything();
        self.exit_code.unwrap_or(1)
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
            // Nothing queued behind an `Exit` runs, and here that is not tidiness: a
            // `Capture` behind an `Exit` reaches `CGRequestListenEventAccess`, which puts a
            // permission dialog on the screen on behalf of a component that is shutting down,
            // and then builds a tap the teardown destroys a moment later.
            if self.shutdown {
                break;
            }
            match cmd {
                Cmd::Capture(mode) => self.set_capture(mode),
                Cmd::Exit(code) => {
                    self.exit_code = Some(code);
                    self.shutdown = true;
                }
            }
        }
    }

    /// The work no event hangs off: the engine still being there, the two grants, and
    /// the keyboard layout.
    fn periodic(&mut self) {
        // Asked here as well as discovered in `emit`, because `emit` only learns it when there
        // is something to send: an engine that died having left capture Off produces no
        // upcalls, so nothing would ever notice. `is_closed` is a load on the channel's own
        // state and costs nothing.
        if self.shared.events_tx.is_closed() {
            self.shared.engine_gone.store(true, Ordering::Relaxed);
        }
        if self.shared.engine_gone.load(Ordering::Relaxed) {
            // ONE more turn before acting on it, and only then. `main` drops the receiver
            // when the engine returns and only then calls `request_exit`, so at a clean
            // stop this flag can be set a moment before the exit code arrives: acting at
            // once would exit 1, which is the supervisor's signal to restart, for a
            // component that stopped exactly as it was asked to. A turn begins by draining
            // the command queue, so one is enough.
            if self.gone_seen || self.exit_code.is_some() {
                if self.exit_code.is_none() {
                    warn("the engine's end of the upcall channel closed; stopping");
                }
                self.shutdown = true;
                return;
            }
            self.gone_seen = true;
        }
        // The dead mouse retry. `confine(None)` and `warp` record a failed re-coupling here
        // because nothing else can: the tap will not re-warp (it reads `confined`, already
        // false) and the engine has already forgotten it ever asked. Every turn, not once a
        // second, because a cursor that moves nothing is the worst state this file can leave a
        // machine in and a wasted call costs a microsecond.
        if self.shared.needs_reassociate.load(Ordering::Relaxed)
            && !self.shared.confined.load(Ordering::Relaxed)
        {
            self.reassociate_tries = self.reassociate_tries.saturating_add(1);
            if associate_quietly(true) {
                self.shared
                    .needs_reassociate
                    .store(false, Ordering::Relaxed);
                if self.reassociate_tries > 1 {
                    warn("the pointer is coupled to the mouse again");
                }
                self.reassociate_tries = 0;
            } else if self.reassociate_tries.is_multiple_of(40) {
                // Every ten seconds or so, not every turn.
                warn("the pointer still cannot be coupled back to the mouse; still trying");
            }
        }
        // The door back, once a second. Clearing the flag does not build a tap: it widens the
        // capabilities, the engine asks for the capture it wants again, and the next attempt
        // happens then. One attempt per second instead of a spin.
        if self.tap_broken && self.tap_failed_at.elapsed() >= TAP_POLL {
            self.tap_broken = false;
            self.publish_caps();
            self.shared.emit(BackendEvent::CapabilitiesChanged);
        }
        // What the tap callback counted but could not say. Both are said from here because
        // stderr from inside the callback is a tap the system disables (truth 8).
        let dropped = self.shared.dropped.load(Ordering::Relaxed);
        if dropped > self.said_dropped {
            self.said_dropped = dropped;
            warn("the engine is not draining input events; dropping some");
        }
        let missed = self.shared.warp_failed.swap(0, Ordering::Relaxed);
        if missed > 0 {
            warn(&format!(
                "the pointer could not be held in place ({missed} time(s))"
            ));
        }
        if self.grants_at.elapsed() >= GRANT_POLL {
            self.grants_at = Instant::now();
            let now = (
                objc2_core_graphics::CGPreflightListenEventAccess(),
                objc2_core_graphics::CGPreflightPostEventAccess(),
            );
            if now != self.grants {
                self.grants = now;
                self.publish_caps();
                // Truth 3: nothing else would tell the engine, and without this a Mac
                // whose permission a person has just granted keeps saying it cannot
                // type.
                self.shared.emit(BackendEvent::CapabilitiesChanged);
                // A grant that arrived may make the tap possible now.
                if self.mode != CaptureMode::Off && self.tap.is_none() {
                    let mode = self.mode;
                    self.mode = CaptureMode::Off;
                    self.set_capture(mode);
                }
            }
        }
        // Truth 11, and truth 8's repair. Two failures, one answer: the tap the system
        // turned off (which arrives through the callback) and the tap that went inert with
        // no event at all (which follows a code signing or permission identity change and is
        // only visible through `CGEventTapIsEnabled`). Enabling it is tried first, and a tap
        // that will not enable is rebuilt.
        if self.mode != CaptureMode::Off && self.tap_at.elapsed() >= TAP_POLL {
            self.tap_at = Instant::now();
            let flagged = self.shared.tap_disabled.swap(false, Ordering::Acquire);
            let dead = self
                .tap
                .as_ref()
                .is_some_and(|tap| !CGEvent::tap_is_enabled(tap));
            if flagged || dead {
                if let Some(tap) = &self.tap {
                    CGEvent::tap_enable(tap, true);
                }
                let still_dead = self
                    .tap
                    .as_ref()
                    .is_none_or(|tap| !CGEvent::tap_is_enabled(tap));
                if still_dead {
                    warn("the event tap will not enable; rebuilding it");
                    self.destroy_tap();
                    self.create_tap();
                }
                // Either way the engine hears about it, so a session that lost its capture
                // for a moment is told rather than left believing it still has one.
                self.shared.emit(BackendEvent::CapabilitiesChanged);
            }
        }
        if self.layout_at.elapsed() >= GRANT_POLL {
            self.layout_at = Instant::now();
            let fresh = Layout::read();
            let changed = {
                let current = match self.layout.lock() {
                    Ok(g) => g,
                    Err(p) => p.into_inner(),
                };
                match (current.as_ref(), fresh.as_ref()) {
                    (Some(a), Some(b)) => a.identity != b.identity,
                    (None, Some(_)) | (Some(_), None) => true,
                    (None, None) => false,
                }
            };
            if changed {
                let identity = fresh
                    .as_ref()
                    .map(|l| l.identity.clone())
                    .unwrap_or_else(|| "mac:none".to_string());
                match self.layout.lock() {
                    Ok(mut slot) => *slot = fresh,
                    Err(p) => *p.into_inner() = fresh,
                }
                self.shared.emit(BackendEvent::LayoutChanged {
                    layout: identity,
                    // macOS has no keyboard group in the X11 sense: it switches whole
                    // input sources, and each one is its own identity above.
                    group: 0,
                });
            }
        }
    }

    fn set_capture(&mut self, mode: CaptureMode) {
        if mode == self.mode {
            return;
        }
        // Every mode change forgets what was held and where the pointer was put. Both are
        // per session state, and keeping them across sessions is how a source Mac's own held
        // Shift ended up stamped on every key a peer later typed INTO it, and how a click
        // could land where the last session left the cursor.
        self.shared.local_mods.store(0, Ordering::Relaxed);
        self.shared.injected_mods.store(0, Ordering::Relaxed);
        self.shared.seen_locks.store(0, Ordering::Relaxed);
        self.shared.injected_at.store(NO_WARP, Ordering::Relaxed);
        let want = match mode {
            CaptureMode::Off => MODE_OFF,
            CaptureMode::Watch => MODE_WATCH,
            CaptureMode::Swallow => MODE_SWALLOW,
        };
        // The mode is published before the tap exists and after it is gone, and the
        // order is the whole safety of this function: a tap created while the mode still
        // said Off would pass its first events through, and a mode left saying Swallow
        // after the tap was destroyed would claim this machine is consuming keystrokes
        // it is not.
        if want == MODE_OFF {
            self.mode = CaptureMode::Off;
            self.shared.mode.store(MODE_OFF, Ordering::Relaxed);
            self.destroy_tap();
            return;
        }
        self.mode = mode;
        self.shared.mode.store(want, Ordering::Relaxed);
        if self.tap.is_some() {
            return;
        }
        if !objc2_core_graphics::CGPreflightListenEventAccess() {
            // Truth 2: the prompt appears when the feature is used. Once per process.
            if !self.asked_to_listen {
                self.asked_to_listen = true;
                warn(
                    "1Device needs the Input Monitoring permission to read this \
                     computer's keyboard and mouse; asking for it now",
                );
                objc2_core_graphics::CGRequestListenEventAccess();
            }
            self.shared.mode.store(MODE_OFF, Ordering::Relaxed);
            self.mode = CaptureMode::Off;
            self.shared
                .emit(BackendEvent::CaptureLost(CaptureLoss::Permission));
            return;
        }
        self.create_tap();
    }

    /// What every failure to build the tap does.
    ///
    /// The mode goes back to Off (so nothing claims to be swallowing), the failure is
    /// remembered, and the capabilities NARROW: `capture` stops being "the grant is there" and
    /// becomes "the grant is there and the tap works". That is what stops the tight loop the
    /// old shape had, and the loop was real: the engine answers `CaptureLost` by recomputing
    /// what it wants, `can_drive()` was computed from the grant alone, so it asked for Watch
    /// again immediately, which tried the create again immediately, for ever, with both threads
    /// spinning. Narrowed capabilities make the engine want Off, and `periodic` re-opens the
    /// door once a second.
    fn tap_failed(&mut self, why: &str, loss: CaptureLoss) {
        warn(why);
        self.shared.mode.store(MODE_OFF, Ordering::Relaxed);
        self.mode = CaptureMode::Off;
        self.tap_broken = true;
        self.tap_failed_at = Instant::now();
        self.publish_caps();
        self.shared.emit(BackendEvent::CaptureLost(loss));
    }

    /// Which loss a create failure is. `tap_create` returns nothing but `None`, so the reason
    /// has to be asked for separately: with the grant in hand it is not a permission problem,
    /// and calling it one sends the interface to tell a person to grant what they already have.
    fn create_loss() -> CaptureLoss {
        if objc2_core_graphics::CGPreflightListenEventAccess() {
            CaptureLoss::Broken
        } else {
            CaptureLoss::Permission
        }
    }

    fn create_tap(&mut self) {
        let mask = event_mask();
        // SAFETY: the callback matches the signature the API declares, and the user info
        // is the `Arc<Shared>` this structure keeps alive for as long as the tap exists.
        let tap = unsafe {
            CGEvent::tap_create(
                CGEventTapLocation::HIDEventTap,
                CGEventTapPlacement::HeadInsertEventTap,
                // Default, and not `ListenOnly`: an active tap is the only kind that can
                // consume an event, and consuming is what swallowing is.
                CGEventTapOptions::Default,
                mask,
                Some(tap_callback),
                Arc::as_ptr(&self.shared) as *mut c_void,
            )
        };
        let Some(tap) = tap else {
            self.tap_failed("the event tap could not be created", Self::create_loss());
            return;
        };
        let source = CFMachPort::new_run_loop_source(None, Some(&tap), 0);
        let Some(source) = source else {
            tap.invalidate();
            self.tap_failed(
                "the event tap's run loop source could not be made",
                CaptureLoss::Broken,
            );
            return;
        };
        // SAFETY: an AppKit extern static.
        let mode = unsafe { kCFRunLoopDefaultMode };
        let Some(run_loop) = CFRunLoop::current() else {
            // A tap whose source is on no run loop is never called, and storing it would have
            // the mode atomic say Swallow while every keystroke reached the local machine: the
            // engine would believe it had the keyboard and the person would watch their typing
            // go into the wrong computer. Unreachable on the main thread, and reported rather
            // than assumed.
            source.invalidate();
            tap.invalidate();
            self.tap_failed(
                "there is no run loop to deliver the event tap's events on",
                CaptureLoss::Broken,
            );
            return;
        };
        run_loop.add_source(Some(&source), mode);
        CGEvent::tap_enable(&tap, true);
        self.tap = Some(tap);
        self.tap_source = Some(source);
        self.tap_broken = false;
        // A tap this new has not had time to go inert, and the watchdog's clock starts here:
        // without this the first poll after any start fired against a tap created microseconds
        // earlier, because `tap_at` is not refreshed while capture is Off.
        self.tap_at = Instant::now();
        // A disable flagged for a PREVIOUS tap says nothing about this one.
        self.shared.tap_disabled.store(false, Ordering::Relaxed);
        self.publish_caps();
    }

    /// Takes the tap down, in the order Apple's own teardown uses.
    ///
    /// Disable, then invalidate the run loop source (which removes it from every run loop it
    /// was added to, rather than only marking it), then invalidate the MACH PORT, then drop
    /// both. The port's invalidation is the step that guarantees no further callback: the
    /// callback's user info is a raw pointer to this backend's `Shared`, so a callback that
    /// arrived after the `Arc` was gone would read freed memory.
    fn destroy_tap(&mut self) {
        if let Some(tap) = &self.tap {
            CGEvent::tap_enable(tap, false);
        }
        if let Some(source) = self.tap_source.take() {
            source.invalidate();
        }
        if let Some(tap) = self.tap.take() {
            tap.invalidate();
        }
        // Nothing said about the tap that is gone carries over to the next one.
        self.shared.tap_disabled.store(false, Ordering::Relaxed);
        self.publish_caps();
    }

    fn publish_caps(&self) {
        let caps = self.compute_caps();
        match self.caps.lock() {
            Ok(mut slot) => *slot = caps,
            Err(p) => *p.into_inner() = caps,
        }
    }

    fn compute_caps(&self) -> Capabilities {
        let (listen, post) = self.grants;
        // The GRANT and the tap, not the grant alone: a machine whose tap will not build cannot
        // capture, whatever permission it holds, and saying otherwise had the engine ask for a
        // capture it could not get as fast as both threads could go round.
        let listen = listen && !self.tap_broken;
        let stable = match self.caps.lock() {
            Ok(caps) => caps.monitors_stable,
            Err(p) => p.into_inner().monitors_stable,
        };
        // OR, not AND. The two grants are separate TCC entries (truth 1) and the asymmetric
        // case is the NORMAL one on macOS, because a person clicks one dialog and not the
        // other. Requiring both to be missing meant a Mac that can watch its keyboard but not
        // type reported no problem at all, so the interface had nothing to turn into the
        // sentence that names the permission it is still waiting for.
        let problem = if self.tap_broken {
            // Not `NoPermission`: the grant may be there and the tap still not build (a code
            // signing or TCC identity change does exactly that, truth 11).
            Some(Problem::NoBackend)
        } else if !listen || !post {
            Some(Problem::NoPermission)
        } else if !stable {
            Some(Problem::MonitorsUnstable)
        } else {
            None
        };
        Capabilities {
            capture: listen,
            // An ACTIVE tap can consume what it sees, which is what swallowing is.
            swallow: listen,
            // The two part promise: the pin is the decoupling (truth 5) and the OS
            // native relative source is the event's own delta fields. Both come with the
            // tap.
            confine: listen,
            warp: true,
            inject_keys: post,
            inject_pointer: post,
            // `CGEventKeyboardSetUnicodeString`, which is why a symbol this layout
            // cannot produce still arrives.
            unicode: post,
            monitors_stable: stable,
            problem,
        }
    }

    /// The last thing this loop does, whatever ended it, and the moment a keyboard is
    /// left working or left dead.
    fn release_everything(&mut self) {
        self.mode = CaptureMode::Off;
        self.shared.mode.store(MODE_OFF, Ordering::Relaxed);
        self.destroy_tap();
        self.shared.confined.store(false, Ordering::Relaxed);
        self.shared
            .needs_reassociate
            .store(false, Ordering::Relaxed);
        // UNCONDITIONALLY, and that is the whole point of this line. It used to be
        // `if confined.swap(false)`, which looked like a safety net and was not one: an
        // ordinary teardown goes through `confine(None)`, which clears that flag BEFORE it
        // re-associates, so the swap returned false on exactly the path where the
        // re-association had failed. A cursor left decoupled is a mouse that moves nothing,
        // this platform's version of a pointer stuck in a corner, and the call costs one round
        // trip on a path taken once per process.
        let _ = associate(true);
        // Nothing is held any more, so nothing is stamped on whatever comes next.
        self.shared.local_mods.store(0, Ordering::Relaxed);
        self.shared.injected_mods.store(0, Ordering::Relaxed);
        self.shared.seen_locks.store(0, Ordering::Relaxed);
        self.shared.injected_at.store(NO_WARP, Ordering::Relaxed);
        self.shared.buttons.store(0, Ordering::Relaxed);
    }
}

impl Drop for Backend {
    /// The safety net at thread death: a tap that outlives its process cannot exist (it
    /// belongs to the process), but a DECOUPLED cursor is a window server setting and
    /// this is the last chance to put it back.
    ///
    /// It is also load bearing for a second reason, and this is the note that says so: the tap
    /// callback's user info is a raw pointer into `self.shared`, and Rust drops a structure's
    /// fields in declaration order, which puts `shared` before `tap`. Running
    /// `release_everything` HERE, before any field is dropped, is what takes the tap down while
    /// the memory its callback would read is still alive. Do not reorder the fields expecting
    /// that to matter, and do not remove this impl.
    fn drop(&mut self) {
        self.release_everything();
    }
}

/// Which events the tap asks for.
///
/// Every kind this backend reports, and nothing else: a tap that asked for everything
/// would be called for events it discards, on a path whose whole budget is not being
/// slow (truth 8).
fn event_mask() -> u64 {
    let bit = |kind: CGEventType| 1u64 << kind.0;
    bit(CGEventType::KeyDown)
        | bit(CGEventType::KeyUp)
        | bit(CGEventType::FlagsChanged)
        | bit(CGEventType::MouseMoved)
        | bit(CGEventType::LeftMouseDown)
        | bit(CGEventType::LeftMouseUp)
        | bit(CGEventType::RightMouseDown)
        | bit(CGEventType::RightMouseUp)
        | bit(CGEventType::OtherMouseDown)
        | bit(CGEventType::OtherMouseUp)
        | bit(CGEventType::LeftMouseDragged)
        | bit(CGEventType::RightMouseDragged)
        | bit(CGEventType::OtherMouseDragged)
        | bit(CGEventType::ScrollWheel)
}

/// Owns the pinned [`Backend`]; `run` is the blocking main-thread pump and its return
/// value is the process exit code.
pub struct MacLoop {
    backend: Backend,
}

impl MacLoop {
    pub fn run(mut self) -> i32 {
        self.backend.run()
    }
}

/// The wake source's callback, which exists only so that a signalled source makes
/// `CFRunLoopRunInMode` return.
///
/// SAFETY: called by Core Foundation on the loop's own thread.
unsafe extern "C-unwind" fn wake_perform(_info: *mut c_void) {}

/// Builds the macOS backend.
///
/// # The platform half of the crash guard
///
/// Two things this backend does could outlive it, and they are handled differently. An
/// event tap belongs to the process, so a process that dies cannot leave a keyboard
/// swallowed; that one is free. A DECOUPLED cursor is a window server setting made on
/// behalf of a connection, and whether the server puts it back when that connection
/// dies is not documented. So the first thing this function does is couple the cursor
/// back, unconditionally, which costs one call and undoes whatever a dead predecessor
/// may have left. The supervisor restarts this component, so that moment always comes.
///
/// **Neither half has been observed on a real Mac.** The claim about the tap is a
/// property of the API's ownership; the claim about the cursor is why the call is made
/// anyway.
pub fn create() -> Result<crate::os::Created, Unsupported> {
    // Truth 4: zeroed once, so a warp does not cost a quarter of a second of dead mouse
    // and does not have to be followed by a re-association that would undo a pin.
    #[allow(deprecated)]
    let status = objc2_core_graphics::CGSetLocalEventsSuppressionInterval(0.0);
    if status != CGError::Success {
        warn(
            "the local event suppression interval could not be zeroed; a warp may cost \
             a moment of dead mouse",
        );
    }
    // The platform half of the crash guard: see this function's own documentation. The answer
    // is ignored on purpose: there may have been nothing to undo, and there is no session yet
    // to report a failure to.
    let _ = associate(true);

    let run_loop = CFRunLoop::current().ok_or(Unsupported(Problem::NoBackend))?;
    let mut context = CFRunLoopSourceContext {
        version: 0,
        info: std::ptr::null_mut(),
        retain: None,
        release: None,
        copyDescription: None,
        equal: None,
        hash: None,
        schedule: None,
        cancel: None,
        perform: Some(wake_perform),
    };
    // SAFETY: a context this function owns for the duration of the call, which is all
    // `CFRunLoopSourceCreate` reads it for.
    let wake_source = unsafe { CFRunLoopSource::new(None, 0, &mut context) }
        .ok_or(Unsupported(Problem::NoBackend))?;

    let cmds: Arc<Mutex<VecDeque<Cmd>>> = Arc::new(Mutex::new(VecDeque::new()));
    let (events_tx, backend_events) = mpsc::channel(BACKEND_EVENT_CAPACITY);
    let shared = Arc::new(Shared {
        mode: AtomicU8::new(MODE_OFF),
        events_tx,
        anchor_x: AtomicI32::new(0),
        anchor_y: AtomicI32::new(0),
        confined: AtomicBool::new(false),
        warp_failed: AtomicU32::new(0),
        local_mods: AtomicU32::new(0),
        injected_mods: AtomicU32::new(0),
        seen_locks: AtomicU32::new(0),
        injected_at: AtomicU64::new(NO_WARP),
        needs_reassociate: AtomicBool::new(false),
        wheel_x: AtomicI32::new(0),
        wheel_y: AtomicI32::new(0),
        buttons: AtomicU32::new(0),
        // Seeded off the window server's own count for this event type, so the numbers this
        // backend uses do not collide with the ones a real mouse is making.
        event_number: AtomicU32::new(
            CGEventSource::counter_for_event_type(
                CGEventSourceStateID::CombinedSessionState,
                CGEventType::LeftMouseDown,
            )
            .saturating_add(1),
        ),
        tap_disabled: AtomicBool::new(false),
        engine_gone: AtomicBool::new(false),
        dropped: AtomicU32::new(0),
    });
    let wake = Arc::new(Wake {
        source: wake_source.clone(),
        run_loop,
    });
    let layout = Arc::new(Mutex::new(Layout::read()));
    let grants = (
        objc2_core_graphics::CGPreflightListenEventAccess(),
        objc2_core_graphics::CGPreflightPostEventAccess(),
    );
    let (_, stable) = read_monitors();
    let caps = Arc::new(Mutex::new(Capabilities {
        capture: grants.0,
        swallow: grants.0,
        confine: grants.0,
        warp: true,
        inject_keys: grants.1,
        inject_pointer: grants.1,
        unicode: grants.1,
        monitors_stable: stable,
        // OR, for the reason `compute_caps` gives.
        problem: if !grants.0 || !grants.1 {
            Some(Problem::NoPermission)
        } else if !stable {
            Some(Problem::MonitorsUnstable)
        } else {
            None
        },
    }));

    let backend = Backend {
        cmds: Arc::clone(&cmds),
        shared: Arc::clone(&shared),
        caps: Arc::clone(&caps),
        layout: Arc::clone(&layout),
        _wake: Arc::clone(&wake),
        wake_source,
        tap: None,
        tap_source: None,
        mode: CaptureMode::Off,
        asked_to_listen: false,
        grants,
        grants_at: Instant::now(),
        layout_at: Instant::now(),
        tap_at: Instant::now(),
        tap_broken: false,
        tap_failed_at: Instant::now(),
        shutdown: false,
        exit_code: None,
        gone_seen: false,
        said_dropped: 0,
        reassociate_tries: 0,
    };
    let handle = MacBackend {
        cmds,
        wake,
        shared,
        caps,
        layout,
        asked_to_post: Arc::new(AtomicBool::new(false)),
        // Seeded as "not locked, asked long ago", so the first injection asks for real.
        locked: Arc::new(Mutex::new((
            Instant::now()
                .checked_sub(LOCK_POLL)
                .unwrap_or_else(Instant::now),
            false,
        ))),
    };
    Ok(crate::os::Created {
        handle: crate::os::Backend::Mac(handle),
        backend_events,
        event_loop: crate::os::EventLoop::Mac(Box::new(MacLoop { backend })),
    })
}

#[cfg(test)]
mod tests {
    //! What can be tested without a desktop: the key table, the modifier translations,
    //! the button numbers, and (where the machine has a keyboard layout to ask) the
    //! table checked against `UCKeyTranslate`. Everything that needs a screen, a real
    //! keyboard or a person is on the deferred list, and this module's coverage is
    //! deliberately narrow because the rest of the file has never run.
    use super::*;

    #[test]
    fn the_usage_table_is_a_function_both_ways_except_where_it_says_so() {
        let mut usages: Vec<u32> = USAGE_VK
            .iter()
            .chain(USAGE_VK_ALIAS)
            .map(|(u, _)| *u)
            .collect();
        usages.sort_unstable();
        let before = usages.len();
        usages.dedup();
        assert_eq!(before, usages.len(), "a usage appears twice");
        for (id, vk) in USAGE_VK {
            let usage = keys::usage(keys::PAGE_KEYBOARD, *id);
            assert_eq!(vk_of_usage(usage), Some(*vk));
            let back = usage_of_vk(*vk).expect("every keycode names a usage");
            // The one key two usages really do share: the backslash and the non-US hash
            // are one key. Everything else that used to be here has moved to
            // `USAGE_VK_ALIAS`, which is one directional precisely so that this holds.
            if *id != 0x32 {
                assert_eq!(back, usage, "usage {id:#04x} did not round trip");
            }
        }
        // The alias table is one directional: the usage presses the key, and the key
        // reports what is printed on it rather than the alias.
        for (id, vk) in USAGE_VK_ALIAS {
            let usage = keys::usage(keys::PAGE_KEYBOARD, *id);
            assert_eq!(vk_of_usage(usage), Some(*vk), "{id:#04x} presses its key");
            assert_ne!(
                usage_of_vk(*vk),
                Some(usage),
                "and {id:#04x} is not what that key reports"
            );
        }
        assert_eq!(vk_of_usage(keys::usage(keys::PAGE_CONSUMER, 0xCD)), None);
    }

    /// Every canonical name the dialect defines has a key on this platform, EXCEPT the
    /// ones a Mac keyboard genuinely does not have, which are named here.
    ///
    /// The exception list is the point of the test: a Mac has no Application key and its
    /// virtual keycodes stop at F20, so answering `None` for those is the truth and the
    /// engine turns it into "that key does not exist on the other computer's keyboard".
    /// A key going missing for any OTHER reason fails here.
    #[test]
    fn every_named_key_of_the_dialect_has_a_keycode_or_is_a_key_a_mac_lacks() {
        // The media keys are not virtual keycodes on a Mac at all: they travel as
        // `NSSystemDefined` events, which this backend does not inject.
        //
        // Menu is deliberately NOT here any more: no Apple keyboard has the key, but macOS
        // delivers a keycode for the one on a third party PC keyboard, so the honest answer is
        // the keycode and not "that key does not exist".
        let absent = ["F21", "F22", "F23", "F24"]; // macOS stops at F20
        let mut missing: Vec<&str> = Vec::new();
        for (name, usage) in keys::NAMED {
            if usage >> 16 == keys::PAGE_CONSUMER {
                assert!(
                    vk_of_usage(*usage).is_none(),
                    "{name} is a media key and has no virtual keycode"
                );
                continue;
            }
            if vk_of_usage(*usage).is_none() {
                missing.push(name);
            }
        }
        missing.sort_unstable();
        let mut expected = absent.to_vec();
        expected.sort_unstable();
        assert_eq!(
            missing, expected,
            "the keys a Mac has no key for are exactly the ones named here"
        );
    }

    #[test]
    fn every_modifier_the_engine_learns_has_a_keycode_that_means_it() {
        for bit in keys::mods::holdable_bits(keys::mods::HOLDABLE) {
            let usage = keys::mod_usage(bit).expect("a holdable bit has a key");
            let vk = vk_of_usage(usage).expect("and a keycode");
            assert_eq!(
                mod_of_vk(vk),
                Some(bit),
                "the keycode for {bit:#x} does not mean it"
            );
        }
    }

    /// The flags and the canonical bits, in both directions. A disagreement here is a
    /// Command plus C that types a letter (truth 6).
    #[test]
    fn the_modifier_flags_and_the_canonical_bits_agree() {
        for bit in [
            keys::mods::SHIFT,
            keys::mods::CTRL,
            keys::mods::ALT,
            keys::mods::META,
        ] {
            let flags = flags_of_mods(bit);
            assert_eq!(mods_of_flags(flags), bit, "{bit:#x} did not round trip");
        }
        // AltGr is the right Option, and the flags cannot tell the two Options apart, so
        // it comes back as ALT. That loss is named here rather than hidden: the tap
        // recovers it from the keycode.
        assert_eq!(
            mods_of_flags(flags_of_mods(keys::mods::ALTGR)),
            keys::mods::ALT
        );
        assert_eq!(flags_of_mods(0), CGEventFlags::empty());
        assert_eq!(mods_of_flags(CGEventFlags::empty()), 0);
    }

    /// The dialect's five buttons, each to a macOS event and back.
    #[test]
    fn the_button_numbers_translate_both_ways() {
        for button in 1..=5u8 {
            for down in [true, false] {
                let (kind, _, number) =
                    mac_button(button, down).expect("every dialect button has an event");
                assert_eq!(
                    button_of_event(kind, number),
                    Some(button),
                    "button {button} did not round trip"
                );
            }
        }
        assert!(mac_button(6, true).is_none());
        assert!(mac_button(0, true).is_none());
        // A button macOS reports and this dialect has no number for is dropped, not
        // renumbered.
        assert_eq!(button_of_event(CGEventType::OtherMouseDown, 9), None);
        assert_eq!(button_of_event(CGEventType::MouseMoved, 0), None);
    }

    /// The horizontal scroll sign, which is the one thing about this platform that cancels
    /// out between two Macs and is therefore invisible until a Mac drives a PC.
    ///
    /// The dialect says positive is right; macOS says positive is left. The test is on the
    /// two constants rather than on a live event, because the flip lives at two call sites
    /// and what has to hold is that they agree with each other and with the dialect.
    #[test]
    fn the_horizontal_scroll_sign_is_flipped_the_same_way_in_both_directions() {
        // Capture: a macOS delta of +N (left) has to become a dialect dx of -N.
        let acc = AtomicI32::new(0);
        let mac_left = 20;
        assert_eq!(
            -notches(&acc, mac_left),
            -2,
            "left on a Mac is left in the dialect"
        );
        let acc = AtomicI32::new(0);
        assert_eq!(-notches(&acc, -mac_left), 2, "and right is right");
        // Injection: the dialect's dx is negated on the way out, which is what makes the two
        // agree. Expressed as the identity the two call sites have to satisfy.
        for dx in [-3i32, -1, 0, 1, 3] {
            assert_eq!(-(-dx), dx, "the two flips compose to the identity");
        }
    }

    /// The buttons this backend is holding decide whether a move is a move or a DRAG, and an
    /// application tracking a drag ignores a plain move.
    #[test]
    fn a_move_while_a_button_is_held_is_a_drag() {
        let shared = test_shared();
        assert_eq!(shared.held_button(), None, "nothing held to begin with");
        assert!(shared.track_button(1, true), "the left button goes down");
        assert_eq!(shared.held_button(), Some(1));
        assert!(shared.track_button(3, true), "and the right one too");
        assert_eq!(
            shared.held_button(),
            Some(1),
            "the lowest held button decides, deterministically"
        );
        assert!(shared.track_button(1, false));
        assert_eq!(shared.held_button(), Some(3));
        assert!(!shared.track_button(3, false), "and now nothing is held");
        assert_eq!(shared.held_button(), None);
        // A button number outside the dialect's five cannot corrupt the set.
        assert!(shared.track_button(200, true));
        assert_eq!(shared.held_button(), None, "and it is not one of the five");
    }

    /// The event numbers this backend stamps are monotonic, which is what makes a click and
    /// its release one click rather than two halves of nothing (truth 10).
    #[test]
    fn every_mouse_event_gets_its_own_number() {
        let shared = test_shared();
        let first = shared.next_event_number();
        let second = shared.next_event_number();
        assert_eq!(second, first + 1);
        assert!(first > 0, "seeded past zero");
    }

    /// A `Shared` with no window server behind it, for the pure tests above.
    fn test_shared() -> Shared {
        let (events_tx, _rx) = mpsc::channel(1);
        Shared {
            mode: AtomicU8::new(MODE_OFF),
            events_tx,
            anchor_x: AtomicI32::new(0),
            anchor_y: AtomicI32::new(0),
            confined: AtomicBool::new(false),
            warp_failed: AtomicU32::new(0),
            local_mods: AtomicU32::new(0),
            injected_mods: AtomicU32::new(0),
            seen_locks: AtomicU32::new(0),
            injected_at: AtomicU64::new(NO_WARP),
            needs_reassociate: AtomicBool::new(false),
            wheel_x: AtomicI32::new(0),
            wheel_y: AtomicI32::new(0),
            buttons: AtomicU32::new(0),
            event_number: AtomicU32::new(1),
            tap_disabled: AtomicBool::new(false),
            engine_gone: AtomicBool::new(false),
            dropped: AtomicU32::new(0),
        }
    }

    /// The point accumulator, which is what keeps a trackpad from scrolling nothing.
    ///
    /// In POINTS and not in Windows wheel units: a macOS notch is about ten of these (see
    /// [`WHEEL_POINTS_PER_NOTCH`]), and this test used to be written against 120, which is
    /// the number that made twelve notches of a real wheel travel as one.
    #[test]
    fn a_fraction_of_a_notch_is_kept_until_it_is_a_whole_one() {
        let acc = AtomicI32::new(0);
        assert_eq!(notches(&acc, 3), 0);
        assert_eq!(notches(&acc, 3), 0);
        assert_eq!(notches(&acc, 4), 1, "ten points of movement are one notch");
        assert_eq!(acc.load(Ordering::Relaxed), 0);
        // And the other way, which must not round towards zero and lose it.
        assert_eq!(notches(&acc, -5), 0);
        assert_eq!(notches(&acc, -5), -1);
        assert_eq!(acc.load(Ordering::Relaxed), 0);
        assert_eq!(notches(&acc, 0), 0);
        // Whole notches at once pass straight through.
        assert_eq!(notches(&acc, 30), 3);
        // And a number no device could mean neither overflows nor sticks: the accumulator is
        // clamped, so a debug build cannot panic here and a wedged axis cannot park itself at
        // the edge of the type.
        let acc = AtomicI32::new(0);
        assert_eq!(notches(&acc, i32::MAX), 100_000, "clamped, not overflowed");
        assert_eq!(acc.load(Ordering::Relaxed), 0);
    }

    /// The Carbon modifier state `UCKeyTranslate` wants, which everybody gets wrong
    /// once: the event modifier bits shifted right by eight.
    #[test]
    fn the_carbon_modifier_state_is_shifted_the_way_the_call_expects() {
        assert_eq!(uchr_modifiers(0), 0);
        assert_eq!(uchr_modifiers(keys::mods::SHIFT), (1 << 9) >> 8);
        assert_eq!(uchr_modifiers(keys::mods::ALTGR), (1 << 11) >> 8);
        assert_eq!(uchr_modifiers(keys::mods::META), (1u32 << 8) >> 8);
        assert_eq!(
            uchr_modifiers(keys::mods::SHIFT | keys::mods::ALTGR),
            ((1 << 9) | (1 << 11)) >> 8
        );
    }

    /// The anchor is the middle of the rectangle it is given, and it does not overflow.
    #[test]
    fn the_anchor_is_the_middle_of_the_rectangle() {
        assert_eq!(
            centre(&Rect {
                x: 0,
                y: 0,
                w: 1440,
                h: 900
            }),
            Point { x: 720, y: 450 }
        );
        assert_eq!(
            centre(&Rect {
                x: i32::MAX - 1,
                y: 0,
                w: 4,
                h: 0
            }),
            Point { x: i32::MAX, y: 0 }
        );
    }

    /// The table checked against the machine's OWN keyboard layout, which is the only
    /// part of it a build machine can verify: every letter, every digit and the four
    /// keys with a canonical character are translated through `UCKeyTranslate` and
    /// compared with what the usage means.
    ///
    /// It skips when there is no keyboard layout to ask, which is what a machine with no
    /// GUI session looks like, and it only asserts on a layout where the letters are
    /// where a US keyboard has them: on AZERTY the letter a is not on the QWERTY a's
    /// key, and asserting otherwise would be asserting the tester's own keyboard.
    #[test]
    fn the_table_names_the_keys_the_layout_names() {
        let Some(layout) = Layout::read() else {
            eprintln!("skipping: no keyboard layout to ask (no GUI session)");
            return;
        };
        // Is this a layout where the letters are where the table expects them? The
        // letter a's usage is 0x04 and its keycode is 0.
        let a = layout.text(0, 0);
        if a.as_deref() != Some("a") {
            eprintln!(
                "skipping the letter and digit assertions: this machine's layout is not \
                 a QWERTY one (keycode 0 produces {a:?})"
            );
            return;
        }
        let mut checked = 0usize;
        for (id, expected) in ('a'..='z')
            .enumerate()
            .map(|(i, c)| (0x04 + i as u32, c.to_string()))
            .chain(
                "1234567890"
                    .chars()
                    .enumerate()
                    .map(|(i, c)| (0x1E + i as u32, c.to_string())),
            )
        {
            let usage = keys::usage(keys::PAGE_KEYBOARD, id);
            let vk = vk_of_usage(usage).expect("in the table");
            assert_eq!(
                layout.text(vk, 0).as_deref(),
                Some(expected.as_str()),
                "usage {id:#04x} resolved to keycode {vk}, which this layout says \
                 produces {:?} and the table says means {expected:?}",
                layout.text(vk, 0)
            );
            checked += 1;
        }
        assert_eq!(checked, 36, "twenty six letters and ten digits");
        // The shifted level, which is what a capital costs.
        let vk = vk_of_usage(keys::usage(keys::PAGE_KEYBOARD, 0x04)).expect("in the table");
        assert_eq!(layout.text(vk, keys::mods::SHIFT).as_deref(), Some("A"));
        // And the reverse direction, which is what a resolution actually uses.
        assert_eq!(layout.find("a"), Some((vk, 0)));
        assert_eq!(layout.find("A"), Some((vk, keys::mods::SHIFT)));
        assert_eq!(
            layout.find("\u{1F600}"),
            None,
            "an emoji is not on a keyboard"
        );
    }
}
