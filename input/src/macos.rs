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
//! 8. **The tap can be turned off by the system**, either because a callback took too
//!    long (`kCGEventTapDisabledByTimeout`) or because a person pressed a key
//!    (`kCGEventTapDisabledByUserInput`). Both arrive AS EVENTS through the tap's own
//!    callback, and the only correct answer is to re-enable it, which is why the
//!    callback does so little: it reads two atomics, pushes one upcall and returns.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use objc2_core_foundation::{
    CFBoolean, CFData, CFDictionary, CFMachPort, CFRetained, CFRunLoop, CFRunLoopSource,
    CFRunLoopSourceContext, CFString, CFType, CFUUID, kCFRunLoopDefaultMode,
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

/// How often the two TCC grants are re-read (truth 3). A person granting a permission
/// is a human action, so a second is fast enough, and each preflight is a query to
/// another process.
const GRANT_POLL: Duration = Duration::from_secs(1);

/// Bounded capacity of the upcall channel. Generous: the engine drains it as its own
/// loop turns, and a full queue means it has stalled or gone.
const BACKEND_EVENT_CAPACITY: usize = 256;

/// Capture modes, as the one byte the tap callback can read without a lock.
const MODE_OFF: u8 = 0;
const MODE_WATCH: u8 = 1;
const MODE_SWALLOW: u8 = 2;

/// One wheel notch, in the pixel unit macOS scroll events use.
const WHEEL_PIXELS_PER_NOTCH: i32 = 120;

fn warn(message: &str) {
    eprintln!("[1device-input] {message}");
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
    if flags.contains(CGEventFlags::MaskNumericPad) {
        bits |= keys::mods::NUM;
    }
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

    /// The key and the modifiers that produce `text`, searched over the levels a
    /// canonical modifier set can name.
    ///
    /// The order is the order of preference: nothing held first, then Shift, then
    /// Option, then both, and the lowest virtual keycode within each. So a lower case
    /// letter costs no modifier and a capital costs a Shift, which is what the engine's
    /// sequence expects.
    fn find(&self, text: &str) -> Option<(u16, u16)> {
        for mods in [
            0,
            keys::mods::SHIFT,
            keys::mods::ALTGR,
            keys::mods::SHIFT | keys::mods::ALTGR,
        ] {
            for vk in 0u16..128 {
                if self.text(vk, mods).as_deref() == Some(text) {
                    return Some((vk, mods));
                }
            }
        }
        None
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
    /// The canonical modifier bits, from the flags of every event the tap sees.
    mods: AtomicU32,
    /// Scroll movement below one notch, kept so a trackpad is not rounded to nothing
    /// event after event.
    wheel_x: AtomicI32,
    wheel_y: AtomicI32,
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
                let n = self.dropped.fetch_add(1, Ordering::Relaxed);
                if n.is_multiple_of(128) {
                    warn("the engine is not draining input events; dropping some");
                }
            }
        }
    }

    fn mode(&self) -> u8 {
        self.mode.load(Ordering::Relaxed)
    }
}

/// The two ways to reach the run loop from another thread, in one place that is safe
/// to send.
///
/// `CFRunLoopSourceSignal` and `CFRunLoopWakeUp` are the two Core Foundation calls
/// documented as safe from any thread, and they are the only two made through this
/// handle. Nothing else touches either pointer off the loop's own thread.
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
    fn may_post(&self) -> bool {
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
                associate(false);
                warp(anchor, false);
            }
            None => {
                self.shared.confined.store(false, Ordering::Relaxed);
                associate(true);
            }
        }
    }

    fn warp(&self, to: Point) {
        let confined = self.shared.confined.load(Ordering::Relaxed);
        warp(to, !confined);
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
            let has_keys = actions
                .iter()
                .any(|a| matches!(a, Action::Key { .. } | Action::Text(_) | Action::Group(_)));
            if secure && has_keys {
                self.shared
                    .emit(BackendEvent::Refused(Refusal::SecureInput));
                return;
            }
            if screen_is_locked() {
                self.shared
                    .emit(BackendEvent::Refused(Refusal::ScreenLocked));
                return;
            }
        }
        let source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState);
        for action in &actions {
            self.post(source.as_deref(), action);
        }
    }

    /// Posts one action, stamping the modifier flags this backend believes are held
    /// (truth 6).
    fn post(&self, source: Option<&CGEventSource>, action: &Action) {
        let mods = self.shared.mods.load(Ordering::Relaxed) as u16;
        let flags = flags_of_mods(mods);
        match action {
            Action::Key { code, down } => {
                let vk = (code.code & 0xFFFF) as u16;
                // The flags are updated BEFORE the event is posted for a press and
                // AFTER for a release, so a modifier's own event carries the state that
                // includes it and the one that follows a release does not.
                if let Some(bit) = mod_of_vk(vk) {
                    let mut now = mods;
                    if *down {
                        now |= bit;
                    } else {
                        now &= !bit;
                    }
                    self.shared.mods.store(u32::from(now), Ordering::Relaxed);
                }
                let stamped = if let Some(bit) = mod_of_vk(vk) {
                    if *down {
                        flags_of_mods(mods | bit)
                    } else {
                        flags_of_mods(mods & !bit)
                    }
                } else {
                    flags
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
                let at = objc2_core_foundation::CGPoint {
                    x: f64::from(to.x),
                    y: f64::from(to.y),
                };
                if let Some(event) = CGEvent::new_mouse_event(
                    source,
                    CGEventType::MouseMoved,
                    at,
                    CGMouseButton::Left,
                ) {
                    CGEvent::set_flags(Some(&event), flags);
                    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
                }
            }
            Action::MoveBy { dx, dy } => {
                if *dx == 0 && *dy == 0 {
                    return;
                }
                // macOS has no relative mouse event: the position is computed here and
                // the delta travels in the event's own fields, which is what a game
                // reading raw movement looks at.
                let Some(from) = cursor_position() else {
                    return;
                };
                let at = objc2_core_foundation::CGPoint {
                    x: f64::from(from.x.saturating_add(*dx)),
                    y: f64::from(from.y.saturating_add(*dy)),
                };
                if let Some(event) = CGEvent::new_mouse_event(
                    source,
                    CGEventType::MouseMoved,
                    at,
                    CGMouseButton::Left,
                ) {
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
                    CGEvent::set_flags(Some(&event), flags);
                    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
                }
            }
            Action::Button { button, down } => {
                let Some((kind, mac, number)) = mac_button(*button, *down) else {
                    return;
                };
                let at = cursor_position().unwrap_or(Point { x: 0, y: 0 });
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
                    CGEvent::set_flags(Some(&event), flags);
                    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
                }
            }
            Action::Wheel { dx, dy, pixels } => {
                let unit = if *pixels {
                    CGScrollEventUnit::Pixel
                } else {
                    CGScrollEventUnit::Line
                };
                if let Some(event) = CGEvent::new_scroll_wheel_event2(source, unit, 2, *dy, *dx, 0)
                {
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

/// Couples or decouples the cursor from the mouse (truth 5).
fn associate(connected: bool) {
    let status = CGAssociateMouseAndMouseCursorPosition(connected);
    if status != CGError::Success {
        warn(&format!(
            "the pointer could not be {}: CGError {status:?}",
            if connected { "released" } else { "pinned" }
        ));
    }
}

/// Puts the pointer somewhere.
///
/// `reassociate` couples the cursor back to the mouse afterwards, which is one of the
/// two documented cures for the 250 millisecond suppression (truth 4). It must NOT
/// happen while a session has the pointer pinned, because the pin IS the decoupling:
/// the caller decides, and the suppression interval is zeroed once at construction so
/// that the choice is safe either way.
fn warp(to: Point, reassociate: bool) {
    let at = objc2_core_foundation::CGPoint {
        x: f64::from(to.x),
        y: f64::from(to.y),
    };
    let status = CGWarpMouseCursorPosition(at);
    if status != CGError::Success {
        warn(&format!(
            "the pointer could not be moved: CGError {status:?}"
        ));
    }
    if reassociate {
        associate(true);
    }
}

/// Is the screen locked, or is somebody else's session in front?
///
/// The session dictionary is the documented way to ask, and both keys matter: a locked
/// screen and a session that is not on the console are the same thing as far as an
/// injection is concerned, which is that nobody would see it.
fn screen_is_locked() -> bool {
    let Some(session) = objc2_core_graphics::CGSessionCopyCurrentDictionary() else {
        // No session dictionary at all is what a process with no window server
        // connection looks like, and it cannot type either.
        return true;
    };
    let locked = dictionary_flag(&session, "CGSSessionScreenIsLocked");
    let on_console = dictionary_flag(&session, "kCGSSessionOnConsoleKey");
    locked == Some(true) || on_console == Some(false)
}

/// One boolean out of a Core Foundation dictionary, or `None` when it is not there.
fn dictionary_flag(dictionary: &CFDictionary, key: &str) -> Option<bool> {
    let key = CFString::from_str(key);
    // SAFETY: a key this function owns and a dictionary it borrows; the value follows
    // the Get rule and is only read before either is dropped.
    let value = unsafe { dictionary.value((&*key) as *const CFString as *const c_void) };
    if value.is_null() {
        return None;
    }
    // SAFETY: the session dictionary's documented values for these two keys are
    // `CFBoolean`s. A dictionary from the window server is not peer input, so trusting
    // the documented class is reasonable here in a way it would not be for a value that
    // crossed the network.
    let flag = unsafe { &*value.cast::<CFBoolean>() };
    Some(flag.value())
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
        return (Vec::new(), true);
    }
    let main = CGMainDisplayID();
    let mut out = Vec::new();
    let mut stable = true;
    for id in ids.iter().take((count as usize).min(ids.len())) {
        let bounds = CGDisplayBounds(*id);
        if bounds.size.width < 1.0 || bounds.size.height < 1.0 {
            continue;
        }
        let pixels_wide = objc2_core_graphics::CGDisplayPixelsWide(*id) as f64;
        let scale = if bounds.size.width > 0.0 {
            ((pixels_wide / bounds.size.width) * 1000.0).round() as i32
        } else {
            1000
        };
        let (identity, named) = display_identity(*id);
        if !named {
            stable = false;
        }
        out.push(Monitor {
            id: identity,
            // Core Graphics has no display NAME: the localised one belongs to AppKit's
            // `NSScreen`, which this component does not link. The number is what a
            // person sees until the interface pairs it with something better.
            name: format!("Display {id}"),
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
            // The right Option is AltGr on a Mac and the flags cannot tell the two
            // apart (see `mods_of_flags`), so the keycode decides.
            if vk == 61 && mods & keys::mods::ALT != 0 {
                mods = (mods & !keys::mods::ALT) | keys::mods::ALTGR;
            }
            shared.mods.store(u32::from(mods), Ordering::Relaxed);
            // A `FlagsChanged` is a modifier going down or coming up and macOS does not
            // say which: the flags after the change do. A modifier whose bit is now set
            // went down.
            let down = match kind {
                CGEventType::KeyDown => true,
                CGEventType::KeyUp => false,
                _ => mod_of_vk(vk).is_some_and(|bit| mods & bit != 0),
            };
            let usage = usage_of_vk(vk).unwrap_or(0);
            let is_lock = usage != 0 && keys::is_lock(usage);
            if is_lock && !down {
                // A half duplex lock reports only its press.
                return if swallow { std::ptr::null_mut() } else { pass };
            }
            let sym = tap_symbol(event_ref);
            let key = (usage != 0).then(|| keys::name_of(usage)).flatten();
            shared.emit(BackendEvent::Key(KeyEvent {
                usage,
                key: key.map(str::to_string),
                sym,
                mods,
                down,
                lock: is_lock,
            }));
        }
        CGEventType::MouseMoved
        | CGEventType::LeftMouseDragged
        | CGEventType::RightMouseDragged
        | CGEventType::OtherMouseDragged => {
            shared.mods.store(u32::from(mods), Ordering::Relaxed);
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
                if (at.x as i32, at.y as i32) != (anchor.x, anchor.y) {
                    warp(anchor, false);
                }
            }
        }
        CGEventType::ScrollWheel => {
            shared.mods.store(u32::from(mods), Ordering::Relaxed);
            // The pixel axis, accumulated into whole notches with the remainder kept: a
            // trackpad sends a fraction of a notch per event and rounding each one to
            // zero would scroll nothing at all.
            let py = CGEvent::integer_value_field(
                Some(event_ref),
                CGEventField::ScrollWheelEventPointDeltaAxis1,
            ) as i32;
            let px = CGEvent::integer_value_field(
                Some(event_ref),
                CGEventField::ScrollWheelEventPointDeltaAxis2,
            ) as i32;
            let ny = notches(&shared.wheel_y, py);
            let nx = notches(&shared.wheel_x, px);
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
                shared.mods.store(u32::from(mods), Ordering::Relaxed);
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
    if swallow {
        // Null is how a tap consumes an event: it never reaches the application that
        // would have had it.
        std::ptr::null_mut()
    } else {
        pass
    }
}

/// Whole notches out of a pixel accumulator, with the remainder kept.
fn notches(accumulator: &AtomicI32, pixels: i32) -> i32 {
    if pixels == 0 {
        return 0;
    }
    let total = accumulator.fetch_add(pixels, Ordering::Relaxed) + pixels;
    let whole = total / WHEEL_PIXELS_PER_NOTCH;
    if whole != 0 {
        accumulator.fetch_sub(whole * WHEEL_PIXELS_PER_NOTCH, Ordering::Relaxed);
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
    shutdown: bool,
    exit_code: Option<i32>,
}

impl Backend {
    /// The main-thread pump. Commands first, then the periodic work, then a turn of the
    /// run loop, which is where the tap's callbacks are delivered.
    fn run(&mut self) -> i32 {
        // SAFETY: an AppKit extern static, valid while the framework is loaded.
        let mode = unsafe { kCFRunLoopDefaultMode };
        let run_loop = CFRunLoop::current().expect("a run loop on the main thread");
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
            CFRunLoop::run_in_mode(mode, IDLE_TURN.as_secs_f64(), true);
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
        if self.shared.engine_gone.load(Ordering::Relaxed) {
            // The engine has gone without asking to stop, which means its thread died.
            // Stopping non-zero has the supervisor restart the component fresh, and the
            // teardown on the way out is what puts the tap and the cursor back.
            warn("the engine's end of the upcall channel closed; stopping");
            self.shutdown = true;
            return;
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
            warn("the event tap could not be created");
            self.shared.mode.store(MODE_OFF, Ordering::Relaxed);
            self.mode = CaptureMode::Off;
            self.shared
                .emit(BackendEvent::CaptureLost(CaptureLoss::Permission));
            return;
        };
        let source = CFMachPort::new_run_loop_source(None, Some(&tap), 0);
        let Some(source) = source else {
            warn("the event tap's run loop source could not be made");
            self.shared.mode.store(MODE_OFF, Ordering::Relaxed);
            self.mode = CaptureMode::Off;
            self.shared
                .emit(BackendEvent::CaptureLost(CaptureLoss::Broken));
            return;
        };
        // SAFETY: an AppKit extern static.
        let mode = unsafe { kCFRunLoopDefaultMode };
        if let Some(run_loop) = CFRunLoop::current() {
            run_loop.add_source(Some(&source), mode);
        }
        CGEvent::tap_enable(&tap, true);
        self.tap = Some(tap);
        self.tap_source = Some(source);
        self.publish_caps();
    }

    fn destroy_tap(&mut self) {
        if let Some(tap) = &self.tap {
            CGEvent::tap_enable(tap, false);
        }
        if let Some(source) = self.tap_source.take() {
            source.invalidate();
        }
        self.tap = None;
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
        let stable = match self.caps.lock() {
            Ok(caps) => caps.monitors_stable,
            Err(p) => p.into_inner().monitors_stable,
        };
        let problem = if !listen && !post {
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
        if self.shared.confined.swap(false, Ordering::Relaxed) {
            // A cursor left decoupled is a mouse that moves nothing, which is this
            // platform's version of a pointer stuck in a corner.
            associate(true);
        }
    }
}

impl Drop for Backend {
    /// The safety net at thread death: a tap that outlives its process cannot exist (it
    /// belongs to the process), but a DECOUPLED cursor is a window server setting and
    /// this is the last chance to put it back.
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
    // The platform half of the crash guard: see this function's own documentation.
    associate(true);

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
        mods: AtomicU32::new(0),
        wheel_x: AtomicI32::new(0),
        wheel_y: AtomicI32::new(0),
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
        problem: if !grants.0 && !grants.1 {
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
        shutdown: false,
        exit_code: None,
    };
    let handle = MacBackend {
        cmds,
        wake,
        shared,
        caps,
        layout,
        asked_to_post: Arc::new(AtomicBool::new(false)),
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
        let absent = [
            "Menu", // the Application key, which no Apple keyboard has ever had
            "F21", "F22", "F23", "F24", // macOS stops at F20
        ];
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

    /// The pixel accumulator, which is what keeps a trackpad from scrolling nothing.
    #[test]
    fn a_fraction_of_a_notch_is_kept_until_it_is_a_whole_one() {
        let acc = AtomicI32::new(0);
        assert_eq!(notches(&acc, 40), 0);
        assert_eq!(notches(&acc, 40), 0);
        assert_eq!(notches(&acc, 40), 1, "three forties are one notch");
        assert_eq!(acc.load(Ordering::Relaxed), 0);
        // And the other way, which must not round towards zero and lose it.
        assert_eq!(notches(&acc, -60), 0);
        assert_eq!(notches(&acc, -60), -1);
        assert_eq!(notches(&acc, 0), 0);
        // Whole notches at once pass straight through.
        assert_eq!(notches(&acc, 360), 3);
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
