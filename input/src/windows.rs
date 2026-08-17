// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The Windows keyboard and mouse backend: `SendInput` for injection, the two low
//! level hooks for capture and swallowing, `ClipCursor` plus the hooks for the pin.
//!
//! The shape is `clipboard/src/windows.rs`'s, for its reason: a message-only window
//! and its pump are pinned to the main thread inside [`Backend`], the async engine
//! runs on a side thread and drives the OS through the cheap, `Clone`
//! [`WindowsBackend`] handle, and upcalls travel back on a bounded channel the pump
//! pushes with `try_send` and never blocks on.
//!
//! # Which downcalls cross the thread boundary, and which do not
//!
//! Only two: [`InputBackend::capture`], because a low level hook must be installed
//! and removed on the thread that pumps its messages, and
//! [`InputBackend::request_exit`], which is the pump's own business. Everything else
//! runs INLINE on the engine's thread, because the Win32 calls behind it are
//! callable from any thread: `SendInput`, `ClipCursor`, `GetCursorPos`,
//! `SetCursorPos`, `EnumDisplayMonitors`, `VkKeyScanExW`, `MapVirtualKeyExW`.
//!
//! That is a deliberate divergence from the clipboard backend, which marshals
//! everything, and it is worth the asymmetry: an injection that went through the
//! command queue would wait for the pump to wake, and the pump's idle wait is capped
//! at [`SHUTDOWN_POLL_CAP`]. A keystroke arriving a quarter of a second late is not a
//! keystroke. `PostMessageW` would shorten that to a wake, but a wake is still a
//! context switch on a path where the OS call itself costs 67 to 91 microseconds
//! (#123), and it buys nothing: none of these functions has any thread affinity.
//!
//! The ordering consequence is written down rather than hidden: a `capture` and an
//! `inject` issued back to back can reach the OS in either order. Nothing depends on
//! that order, because a machine being driven does not capture
//! (doc/input-sharing.md, section 4, rule 3), so the two never overlap in one
//! session.
//!
//! # Six Windows truths this backend is built on
//!
//! 1. **The low level mouse hook's `pt` is the position the pointer is ABOUT to
//!    take, and the cursor has not moved yet when the hook runs.** That is the whole
//!    relative source on this platform: `pt` minus `GetCursorPos()` is the
//!    accelerated delta of this one event, and it is accelerated, which is what
//!    makes driving feel like the machine you are looking at. There is no need for
//!    raw input, and this backend does not use it: `WM_INPUT` would arrive as a
//!    separate message with no defined ordering against the hook callback, and
//!    pairing the two streams would buy an UNACCELERATED delta, which is the wrong
//!    one.
//! 2. **While swallowing, the cursor does not move at all**, because the hook
//!    consumes the move before the system applies it. So the subtraction above
//!    keeps working (the cursor stays where it was, `pt` keeps arriving at cursor
//!    plus delta) and the pin needs no warp per event. `ClipCursor` is kept anyway,
//!    as the belt to the hook's braces: a hook Windows drops for exceeding
//!    `LowLevelHooksTimeout` would otherwise let the pointer walk off this desktop.
//! 3. **A relative mouse move of (0, 0) is discarded by Windows** and reaches no
//!    hook at all (#123), so this backend never emits one and never waits for one.
//! 4. **`dwExtraInfo` does not survive a relative mouse move** (it does survive a
//!    key event, #123). Every injection here carries [`MARKER`] and the hooks drop
//!    what carries it, which covers keys and absolute moves; a relative move
//!    injected by this process would not be recognised. In v1 nothing needs it to
//!    be (truth 2 of the seam, and D15): a machine being driven is not capturing.
//! 5. **The active keyboard layout on Windows belongs to the FOREGROUND THREAD, not
//!    to the machine.** So the layout can change by alt-tabbing between two
//!    applications configured differently, and this backend reports that as a
//!    `LayoutChanged` because it genuinely is one: the next injection will be
//!    translated by the new layout. The engine releases everything it holds on that
//!    event, which is the safe direction.
//! 7. **`MapVirtualKeyExW(MAPVK_VSC_TO_VK_EX)` gives a keypad key its NUM LOCK OFF
//!    virtual key**: the scancode of the keypad 7 names `VK_HOME`, not `VK_NUMPAD7`.
//!    The mapping is a layout level question and Num Lock is a runtime state, so
//!    there is no other answer it could give. It costs nothing, because injection
//!    goes out by SCANCODE and the operating system applies Num Lock itself, and it
//!    is written down because the first version of the table's test expected
//!    `VK_NUMPAD0` and was wrong about Windows rather than about the table.
//! 6. **A Windows session can be one nobody is attached to, and it looks almost
//!    exactly like a normal one.** Measured on a real host whose interactive session
//!    was a DISCONNECTED remote desktop: this process was on `WinSta0\Default`, the
//!    input desktop was also named `Default`, 382 windows and the shell window were
//!    enumerable, and `GetCursorPos`, `GetForegroundWindow` and `SendInput` were all
//!    three denied with `ERROR_ACCESS_DENIED`. A locked machine is the same shape. So
//!    the check that decides whether anybody can see what is typed is
//!    `GetCursorPos` succeeding, not the window station's name, and it happens BEFORE
//!    the injection rather than after it (see [`input_desktop_is_ours`]). The first
//!    version compared the two desktop names only, said yes on that host, and would
//!    have typed a whole session into nothing while reporting success.
//!
//! # What cannot be reached, and is said rather than shrugged at
//!
//! A window of higher integrity (UIPI) and the secure desktop. Both are detected:
//! `SendInput` returning short is the signal, and the two follow-up checks name
//! which one it was, so the refusal reaches the interface as a sentence
//! (doc/input-sharing.md, section 13) instead of as nothing happening.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{
    DISPLAY_DEVICEW, EnumDisplayDevicesW, EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR,
    MONITORINFO, MONITORINFOEXW,
};
use windows_sys::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
    TokenIntegrityLevel,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_READOBJECTS, GetThreadDesktop, GetUserObjectInformationW,
    OpenInputDesktop, UOI_NAME,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThreadId, OpenProcess, OpenProcessToken,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, MDT_EFFECTIVE_DPI,
    SetProcessDpiAwarenessContext,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetKeyState, GetKeyboardLayout, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE,
    KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE,
    MAPVK_VK_TO_VSC_EX, MAPVK_VSC_TO_VK_EX, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
    MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, MapVirtualKeyExW, SendInput,
    ToUnicodeEx, VK_CAPITAL, VK_CONTROL, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU,
    VK_NUMLOCK, VK_RCONTROL, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SCROLL, VK_SHIFT, VkKeyScanExW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, ClipCursor, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GWLP_USERDATA, GetClipCursor, GetCursorPos, GetForegroundWindow, GetSystemMetrics,
    GetWindowLongPtrW, GetWindowThreadProcessId, HHOOK, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_UP,
    MSG, MSLLHOOKSTRUCT, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, PM_REMOVE, PeekMessageW,
    PostMessageW, QS_ALLINPUT, RegisterClassW, SM_CMONITORS, SM_CXVIRTUALSCREEN,
    SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SetCursorPos, SetWindowLongPtrW,
    SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_APP,
    WM_DESTROY, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL,
    WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
    WNDCLASSW, XBUTTON1,
};

use crate::backend::{
    Action, BackendEvent, Capabilities, CaptureLoss, CaptureMode, InputBackend, Monitor,
    PlatformKey, Point, Problem, Rect, Refusal, Resolved, Want,
};
use crate::keys;
use crate::os::Unsupported;

// --------------------------------------------------------------- constants

/// Parent handle of a message-only window: `HWND_MESSAGE == (HWND)-3`.
const HWND_MESSAGE: isize = -3;

/// Private wake message (in the `WM_APP` range): the engine posted a [`Cmd`].
const WM_APP_CMD: u32 = WM_APP + 1;

/// Cap of the pump's idle wait, so a set `shutdown` and a coalesced or lost wake
/// are always observed, and so the periodic work (the layout poll, the monitor
/// poll, re-applying a clip Windows dropped) happens without a message arriving.
const SHUTDOWN_POLL_CAP: Duration = Duration::from_millis(250);

/// Bounded capacity of the upcall channel. Generous: the engine drains it as its
/// own loop turns, and a full queue means it has stalled or gone.
const BACKEND_EVENT_CAPACITY: usize = 256;

/// The marker every injection of ours carries in `dwExtraInfo`, so the hooks can
/// drop their own echo. Chosen to be recognisable in a debugger and unlikely to
/// collide with another tool's marker.
///
/// It survives a key event and an absolute mouse move, and NOT a relative one
/// (#123, and truth 4 of the module header). Nothing in v1 depends on the case it
/// cannot cover.
const MARKER: usize = 0x3144_3235;

/// `SECURITY_MANDATORY_HIGH_RID`, documented by the API. The typed constant lives
/// behind the `Win32_System_SystemServices` feature; hard-coding the one integer we
/// compare against avoids pulling that whole module in for it, which is the
/// precedent `clipboard/src/windows.rs` set for the `CF_*` ids.
const SECURITY_MANDATORY_HIGH_RID: u32 = 0x3000;

/// `HC_ACTION`, the hook code that means "this is a real event". Hard-coded for the
/// same reason as above.
const HC_ACTION: i32 = 0;

/// `MONITORINFOF_PRIMARY`, the one flag `MONITORINFO::dwFlags` can carry. Not
/// exported by `windows-sys` 0.61 under any of the features this crate enables, and
/// it is documented as 1.
const MONITORINFOF_PRIMARY: u32 = 0x0000_0001;

/// `XBUTTON2`, the second extra mouse button. `XBUTTON1` is exported by
/// `Win32_UI_WindowsAndMessaging` and its sibling is not, so it is spelled here.
const XBUTTON2_ID: u16 = 0x0002;

/// One wheel notch, `WHEEL_DELTA`.
const WHEEL_DELTA_I32: i32 = 120;

/// How often the input desktop is re-checked. Between checks the last answer
/// stands, so injecting at 125 Hz costs one check every quarter second rather than
/// one per frame (the check opens and closes a desktop handle).
const DESKTOP_RECHECK: Duration = Duration::from_millis(250);

/// Capture modes, as the one byte the hook callbacks can read without a lock.
const MODE_OFF: u8 = 0;
const MODE_WATCH: u8 = 1;
const MODE_SWALLOW: u8 = 2;

fn warn(message: &str) {
    eprintln!("[1device-input] {message}");
}

/// A `&str` to a `NUL`-terminated wide string for the `...W` APIs.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ------------------------------------------------------------ the key table

/// HID usage to PS/2 set 1 scancode, with `0xE0` in the high byte for an extended
/// key, which is the form `MapVirtualKeyExW(MAPVK_VSC_TO_VK_EX)` takes and the form
/// the low level hook reports (as a scancode plus [`LLKHF_EXTENDED`]).
///
/// Read in both directions: a `Want::Usage` becomes a scancode here and then a
/// virtual key through the layout, and a captured scancode becomes the usage that
/// travels on the wire. It is the one table on this platform that a person had to
/// write, so it is also the one the live suite checks against the OS: every entry's
/// scancode is fed back through `MapVirtualKeyExW` and the virtual key it returns is
/// compared with what the usage means.
///
/// The main block (usages 0x04 to 0x53) is the original PC/AT scancode order, which
/// is also, byte for byte, the Linux evdev keycode numbering: that is not a
/// coincidence but the same historical table, and it is why the X11 backend can
/// share the first half of this list by adding 8.
#[rustfmt::skip]
const USAGE_SCANCODE: &[(u32, u16)] = &[
    // Letters, in HID order (a to z), which is not scancode order.
    (0x04, 0x1E), (0x05, 0x30), (0x06, 0x2E), (0x07, 0x20), (0x08, 0x12), (0x09, 0x21),
    (0x0A, 0x22), (0x0B, 0x23), (0x0C, 0x17), (0x0D, 0x24), (0x0E, 0x25), (0x0F, 0x26),
    (0x10, 0x32), (0x11, 0x31), (0x12, 0x18), (0x13, 0x19), (0x14, 0x10), (0x15, 0x13),
    (0x16, 0x1F), (0x17, 0x14), (0x18, 0x16), (0x19, 0x2F), (0x1A, 0x11), (0x1B, 0x2D),
    (0x1C, 0x15), (0x1D, 0x2C),
    // Digits 1 to 9 then 0.
    (0x1E, 0x02), (0x1F, 0x03), (0x20, 0x04), (0x21, 0x05), (0x22, 0x06), (0x23, 0x07),
    (0x24, 0x08), (0x25, 0x09), (0x26, 0x0A), (0x27, 0x0B),
    (0x28, 0x1C), // Enter
    (0x29, 0x01), // Escape
    (0x2A, 0x0E), // Backspace
    (0x2B, 0x0F), // Tab
    (0x2C, 0x39), // Space
    (0x2D, 0x0C), // minus
    (0x2E, 0x0D), // equal
    (0x2F, 0x1A), // bracket left
    (0x30, 0x1B), // bracket right
    (0x31, 0x2B), // backslash
    (0x32, 0x2B), // the non-US hash, the same key
    (0x33, 0x27), // semicolon
    (0x34, 0x28), // apostrophe
    (0x35, 0x29), // grave
    (0x36, 0x33), // comma
    (0x37, 0x34), // period
    (0x38, 0x35), // slash
    (0x39, 0x3A), // Caps Lock
    (0x3A, 0x3B), (0x3B, 0x3C), (0x3C, 0x3D), (0x3D, 0x3E), (0x3E, 0x3F), (0x3F, 0x40),
    (0x40, 0x41), (0x41, 0x42), (0x42, 0x43), (0x43, 0x44), // F1 to F10
    (0x44, 0x57), (0x45, 0x58), // F11, F12
    (0x46, 0xE037), // Print Screen
    (0x47, 0x46),   // Scroll Lock
    (0x48, 0x45),   // Pause (the E1 1D 45 sequence collapses to this)
    (0x49, 0xE052), // Insert
    (0x4A, 0xE047), // Home
    (0x4B, 0xE049), // Page Up
    (0x4C, 0xE053), // Delete
    (0x4D, 0xE04F), // End
    (0x4E, 0xE051), // Page Down
    (0x4F, 0xE04D), // Right
    (0x50, 0xE04B), // Left
    (0x51, 0xE050), // Down
    (0x52, 0xE048), // Up
    (0x53, 0x45),   // Num Lock
    (0x54, 0xE035), // keypad slash
    (0x55, 0x37),   // keypad asterisk
    (0x56, 0x4A),   // keypad minus
    (0x57, 0x4E),   // keypad plus
    (0x58, 0xE01C), // keypad Enter
    (0x59, 0x4F), (0x5A, 0x50), (0x5B, 0x51), (0x5C, 0x4B), (0x5D, 0x4C), (0x5E, 0x4D),
    (0x5F, 0x47), (0x60, 0x48), (0x61, 0x49), // keypad 1 to 9
    (0x62, 0x52), // keypad 0
    (0x63, 0x53), // keypad period
    (0x64, 0x56), // the 102nd key
    (0x65, 0xE05D), // Menu (application)
    (0x67, 0x59),   // keypad equal
    (0x68, 0x64), (0x69, 0x65), (0x6A, 0x66), (0x6B, 0x67), (0x6C, 0x68), (0x6D, 0x69),
    (0x6E, 0x6A), (0x6F, 0x6B), (0x70, 0x6C), (0x71, 0x6D), (0x72, 0x6E), (0x73, 0x76),
    // F13 to F24
    (0x85, 0x7E),   // keypad comma
    (0x87, 0x73),   // international 1 (Ro)
    (0x88, 0x70),   // international 2 (Katakana/Hiragana)
    (0x89, 0x7D),   // international 3 (Yen)
    (0x8A, 0x79),   // international 4 (Henkan)
    (0x8B, 0x7B),   // international 5 (Muhenkan)
    (0x8C, 0x5C),   // international 6 (keypad JP comma)
    (0x90, 0xF2),   // language 1 (Hangul)
    (0x91, 0xF1),   // language 2 (Hanja)
    (0xE0, 0x1D),   // left Control
    (0xE1, 0x2A),   // left Shift
    (0xE2, 0x38),   // left Alt
    (0xE3, 0xE05B), // left Windows
    (0xE4, 0xE01D), // right Control
    (0xE5, 0x36),   // right Shift
    (0xE6, 0xE038), // right Alt, which is AltGr on a layout that has one
    (0xE7, 0xE05C), // right Windows
];

/// The virtual key a HID usage means, for the keys `MapVirtualKeyExW` cannot name
/// from a scancode on some layouts, and for the consumer page, which has no
/// scancode at all.
///
/// Second chance rather than the primary route: the scancode is what a game reads
/// and what a layout cannot move, so [`USAGE_SCANCODE`] leads and this fills in.
#[rustfmt::skip]
const USAGE_VK: &[(u32, u16)] = &[
    (keys::usage(keys::PAGE_CONSUMER, 0xB5), 0xB0), // VK_MEDIA_NEXT_TRACK
    (keys::usage(keys::PAGE_CONSUMER, 0xB6), 0xB1), // VK_MEDIA_PREV_TRACK
    (keys::usage(keys::PAGE_CONSUMER, 0xB7), 0xB2), // VK_MEDIA_STOP
    (keys::usage(keys::PAGE_CONSUMER, 0xCD), 0xB3), // VK_MEDIA_PLAY_PAUSE
    (keys::usage(keys::PAGE_CONSUMER, 0xE2), 0xAD), // VK_VOLUME_MUTE
    (keys::usage(keys::PAGE_CONSUMER, 0xE9), 0xAF), // VK_VOLUME_UP
    (keys::usage(keys::PAGE_CONSUMER, 0xEA), 0xAE), // VK_VOLUME_DOWN
];

/// The scancode (with its `0xE0` prefix, if any) for a keyboard-page usage.
fn scancode_of(usage: u32) -> Option<u16> {
    let page = usage >> 16;
    let id = usage & 0xFFFF;
    if page != keys::PAGE_KEYBOARD {
        return None;
    }
    USAGE_SCANCODE
        .iter()
        .find(|(u, _)| *u == id)
        .map(|(_, sc)| *sc)
}

/// The usage a captured scancode means. The inverse of [`scancode_of`], and the
/// reason the table is one list rather than two: a pair that disagreed would send a
/// key the far side could not press back.
///
/// Two usages share scancode `0x2B` (the backslash and the non-US hash are one key)
/// and two share `0x45` (Pause and Num Lock differ only by the prefix Windows has
/// already collapsed), so the FIRST match wins and the answer is the usage a person
/// would name. Both are recorded here rather than left to be discovered.
fn usage_of_scancode(scancode: u16) -> Option<u32> {
    USAGE_SCANCODE
        .iter()
        .find(|(_, sc)| *sc == scancode)
        .map(|(u, _)| keys::usage(keys::PAGE_KEYBOARD, *u))
}

/// The canonical modifier bit a virtual key means, for the state this backend keeps
/// while it swallows.
///
/// It has to keep its own: a swallowed key never reaches the OS's keyboard state, so
/// `GetAsyncKeyState` would report a Shift the user is holding as up, and every
/// symbol read off the layout would come out lower case.
fn mod_of_vk(vk: u16) -> Option<u16> {
    match vk {
        v if v == VK_LSHIFT || v == VK_RSHIFT || v == VK_SHIFT => Some(keys::mods::SHIFT),
        v if v == VK_LCONTROL || v == VK_CONTROL => Some(keys::mods::CTRL),
        v if v == VK_RCONTROL => Some(keys::mods::CTRL),
        v if v == VK_LMENU || v == VK_MENU => Some(keys::mods::ALT),
        v if v == VK_RMENU => Some(keys::mods::ALTGR),
        v if v == VK_LWIN || v == VK_RWIN => Some(keys::mods::META),
        _ => None,
    }
}

// -------------------------------------------------------------- the commands

/// A downcall that has to happen on the pump thread. There are only two: see the
/// module header.
#[derive(Debug)]
enum Cmd {
    /// Install, change or remove the low level hooks.
    Capture(CaptureMode),
    /// Stop the loop with this process exit code, releasing everything first.
    Exit(i32),
}

// ---------------------------------------------------------------- the state

/// What the hook callbacks and the engine's thread both need, without a lock on the
/// path a hook callback takes.
///
/// Every field a callback touches is an atomic or a channel sender, and that is a
/// hard requirement rather than a preference: Windows silently stops calling a low
/// level hook that takes longer than `LowLevelHooksTimeout` (300 ms by default, and
/// the callback itself costs 50 nanoseconds when it does nothing, #123). A mutex a
/// slower thread could be holding would turn a stall on the engine's side into a
/// keyboard that stops being observed, so there is none.
#[derive(Debug)]
struct Shared {
    /// [`MODE_OFF`], [`MODE_WATCH`] or [`MODE_SWALLOW`].
    mode: AtomicU8,
    events_tx: mpsc::Sender<BackendEvent>,
    /// Where the pointer is pinned while confined, and whether it is.
    anchor_x: AtomicI32,
    anchor_y: AtomicI32,
    confined: AtomicBool,
    /// The canonical modifier bits, tracked from the hook's own stream because a
    /// swallowed modifier never reaches the OS state (see [`mod_of_vk`]). The three
    /// lock bits live in here too.
    mods: AtomicU32,
    /// Wheel movement below one notch, kept so a high resolution wheel is not
    /// rounded to nothing event after event.
    wheel_x: AtomicI32,
    wheel_y: AtomicI32,
    /// False once a hook could not be installed, so [`Capabilities`] stops claiming
    /// this machine can capture.
    capture_ok: AtomicBool,
    /// False when a monitor could not be given an identity that survives an unplug.
    monitors_stable: AtomicBool,
    /// The layout the last resolution used, as an identity string. Owned by the
    /// pump, read by nobody else, and here rather than on [`Backend`] so
    /// [`Shared::emit`] is the only way an upcall leaves this file.
    dropped: AtomicU32,
}

impl Shared {
    /// Pushes an upcall, never blocking. A full queue is the engine having stalled,
    /// and dropping is better than wedging the pump; a closed one is the engine
    /// having gone, which the pump notices on its next turn.
    fn emit(&self, event: BackendEvent) {
        use mpsc::error::TrySendError;
        match self.events_tx.try_send(event) {
            Ok(()) => {}
            Err(TrySendError::Closed(_)) => {}
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

    fn mods(&self) -> u16 {
        self.mods.load(Ordering::Relaxed) as u16
    }
}

thread_local! {
    /// The hooks' way back to [`Shared`].
    ///
    /// A thread local rather than a global, and it says something true:
    /// `SetWindowsHookExW` gives a hook callback no user data at all, and a low
    /// level hook is delivered on the queue of the thread that installed it, so it
    /// can only ever fire on the pump thread. A `static` would compile and would
    /// claim the state is reachable from anywhere, which is exactly the claim that
    /// makes a second backend in one process (the live suite builds one) hard to
    /// reason about.
    static HOOK_SHARED: std::cell::OnceCell<Arc<Shared>> = const { std::cell::OnceCell::new() };
}

// -------------------------------------------------------------- the handle

/// The cheap, `Clone` handle the engine holds.
///
/// It carries no `HWND`: the window handle travels as an `isize` and is rebuilt for
/// the one call that needs it (`PostMessageW`, which is safe from any thread). That
/// is `clipboard/src/windows.rs`'s answer to "make a raw handle `Send`", and it is
/// the right one: do not, store the integer.
#[derive(Clone, Debug)]
pub struct WindowsBackend {
    hwnd: isize,
    cmds: Arc<Mutex<VecDeque<Cmd>>>,
    shared: Arc<Shared>,
    /// The clip rectangle, in this machine's own desktop coordinates. Read by the
    /// pump to re-apply a clip Windows dropped, written by `confine`.
    clip: Arc<Mutex<Option<Rect>>>,
    /// When the input desktop was last checked, and what the answer was. Shared so
    /// the answer is not recomputed once per injected frame.
    desktop: Arc<Mutex<Desktop>>,
}

/// Whether the input desktop is still ours, and when we last asked.
///
/// The whole reason this exists is truth 6: a locked machine accepts an injection
/// into a desktop nobody is looking at, so the only way not to lie about it is to
/// ask before typing. The answer is cached for [`DESKTOP_RECHECK`] because the check
/// opens and closes a desktop handle, and a lock is not something that happens
/// twice a frame.
#[derive(Debug)]
struct Desktop {
    ours: bool,
    checked: Option<Instant>,
}

impl WindowsBackend {
    /// Enqueue a command for the pump, then wake it. Push BEFORE the wake, or a
    /// coalesced wake could drop the command. A poisoned mutex is recovered so an
    /// `Exit` is never lost.
    fn push(&self, cmd: Cmd) {
        match self.cmds.lock() {
            Ok(mut q) => q.push_back(cmd),
            Err(p) => p.into_inner().push_back(cmd),
        }
        // A lost or coalesced wake leaves the command queued, and the pump caps its
        // idle wait at SHUTDOWN_POLL_CAP, so it costs latency and never correctness.
        // SAFETY: `PostMessageW` is documented as callable from any thread, and the
        // window either exists or the call fails harmlessly.
        unsafe {
            PostMessageW(self.hwnd as HWND, WM_APP_CMD, 0, 0);
        }
    }

    /// Is the input desktop still ours? Cached for [`DESKTOP_RECHECK`].
    fn desktop_is_ours(&self, force: bool) -> bool {
        let mut guard = match self.desktop.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let fresh = guard
            .checked
            .is_some_and(|at| at.elapsed() < DESKTOP_RECHECK);
        if fresh && !force {
            return guard.ours;
        }
        let ours = input_desktop_is_ours();
        guard.ours = ours;
        guard.checked = Some(Instant::now());
        ours
    }
}

// ------------------------------------------------------------ the OS queries

/// Is there a desktop receiving input, and is it the one this process is on?
///
/// Three tests, and every one of them was put there by a real machine rather than by
/// reading the documentation:
///
/// 1. `GetCursorPos` failing. This is the one that matters most and the one a first
///    version did not have. A session nobody is attached to (a DISCONNECTED remote
///    desktop) and a locked one both leave this process able to enumerate the
///    session's 382 windows and to find its shell window, while `GetCursorPos`,
///    `GetForegroundWindow` and `SendInput` are all denied. Measured on a real host
///    in exactly that state: `GetCursorPos` returns 0 with `ERROR_ACCESS_DENIED` and
///    `SendInput` returns 0 with the same, so the injection would have gone nowhere
///    and this backend would have had nothing to say about it. One call, 1.3
///    microseconds (#123), and it is the difference between a refusal with a sentence
///    and a keyboard that silently types into the void.
/// 2. `OpenInputDesktop` failing, which is the answer on the secure desktop: a
///    process on the default desktop is denied a handle to Winlogon's.
/// 3. The two desktop names differing, which catches a screen saver desktop and a
///    switch that was not denied outright.
fn input_desktop_is_ours() -> bool {
    // SAFETY: an out parameter we own.
    let cursor_readable = unsafe {
        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt) != 0
    };
    if !cursor_readable {
        return false;
    }
    // SAFETY: every call below takes handles this function owns and buffers it
    // sizes; the desktop handle is closed on every path.
    unsafe {
        let input = OpenInputDesktop(0, 0, DESKTOP_READOBJECTS);
        if input.is_null() {
            return false;
        }
        let mine = GetThreadDesktop(GetCurrentThreadId());
        let ours = match (desktop_name(input), desktop_name(mine)) {
            (Some(a), Some(b)) => a == b,
            // A name we cannot read is not evidence of anything, and the honest
            // direction here is to believe the two calls that DID answer: the open
            // succeeded and the cursor was readable, so somebody is looking at this
            // desktop.
            _ => true,
        };
        CloseDesktop(input);
        ours
    }
}

/// The name of a desktop or window station handle.
///
/// SAFETY: `handle` must be a live desktop handle.
unsafe fn desktop_name(handle: HANDLE) -> Option<String> {
    if handle.is_null() {
        return None;
    }
    let mut buf = [0u16; 256];
    let mut needed: u32 = 0;
    let ok = unsafe {
        GetUserObjectInformationW(
            handle,
            UOI_NAME,
            buf.as_mut_ptr().cast(),
            std::mem::size_of_val(&buf) as u32,
            &mut needed,
        )
    };
    if ok == 0 {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}

/// The integrity level of a process's token, as its mandatory label RID.
///
/// `None` when the token could not be read, and the caller must treat that as
/// evidence rather than as nothing: `OpenProcess` and `OpenProcessToken` are denied
/// precisely when the target is more privileged than we are, which is the case this
/// function exists to detect.
fn integrity_of(pid: u32) -> Option<u32> {
    // SAFETY: handles are closed on every path, and `GetTokenInformation` is called
    // twice in the documented way (once for the size, once for the data).
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return None;
        }
        let level = token_integrity(process);
        CloseHandle(process);
        level
    }
}

/// Our own integrity level.
fn own_integrity() -> Option<u32> {
    // SAFETY: the pseudo handle needs no close.
    unsafe { token_integrity(GetCurrentProcess()) }
}

/// SAFETY: `process` must be a live process handle with query access.
unsafe fn token_integrity(process: HANDLE) -> Option<u32> {
    let mut token: HANDLE = std::ptr::null_mut();
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return None;
    }
    let mut needed: u32 = 0;
    unsafe {
        windows_sys::Win32::Security::GetTokenInformation(
            token,
            TokenIntegrityLevel,
            std::ptr::null_mut(),
            0,
            &mut needed,
        )
    };
    // A label is a few dozen bytes; a size the OS reports as huge is not one we
    // are willing to allocate on a path that runs while somebody types.
    if needed == 0 || needed > 4096 {
        unsafe { CloseHandle(token) };
        return None;
    }
    let mut buf = vec![0u8; needed as usize];
    let ok = unsafe {
        windows_sys::Win32::Security::GetTokenInformation(
            token,
            TokenIntegrityLevel,
            buf.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    };
    unsafe { CloseHandle(token) };
    if ok == 0 {
        return None;
    }
    // SAFETY: the buffer holds a TOKEN_MANDATORY_LABEL the OS just wrote, and the
    // SID it points at lives inside the same buffer.
    unsafe {
        let label = buf.as_ptr().cast::<TOKEN_MANDATORY_LABEL>();
        let sid = (*label).Label.Sid;
        if sid.is_null() {
            return None;
        }
        let count = GetSidSubAuthorityCount(sid);
        if count.is_null() || *count == 0 {
            return None;
        }
        let last = GetSidSubAuthority(sid, (*count - 1) as u32);
        if last.is_null() {
            return None;
        }
        Some(*last)
    }
}

/// Which refusal explains a `SendInput` that came up short, or `None` when nothing
/// here can name one.
///
/// Every branch is evidence, not a guess, which is the point of the whole feature:
///
/// - the input desktop is not ours: proved by `OpenInputDesktop` or by the names
///   differing;
/// - the foreground window's process is of higher integrity: proved by its
///   mandatory label;
/// - its token could not be opened at all: that denial IS the evidence, because
///   `PROCESS_QUERY_LIMITED_INFORMATION` is granted to a process of equal or lower
///   integrity and refused by a higher one.
///
/// When none of the three holds, this returns `None` and the caller logs the raw
/// error instead of putting a sentence on the wire that it did not earn.
fn refusal_for(desktop_ours: bool) -> Option<Refusal> {
    if !desktop_ours {
        return Some(Refusal::ScreenLocked);
    }
    // SAFETY: reading the foreground window and its owning process id.
    let (hwnd, pid) = unsafe {
        let hwnd = GetForegroundWindow();
        let mut pid: u32 = 0;
        if !hwnd.is_null() {
            GetWindowThreadProcessId(hwnd, &mut pid);
        }
        (hwnd, pid)
    };
    if hwnd.is_null() || pid == 0 {
        // No foreground window at all is what the secure desktop looks like from
        // here, and it is the honest reading: there is nothing on this desktop to
        // type into.
        return Some(Refusal::ScreenLocked);
    }
    let ours = own_integrity().unwrap_or(SECURITY_MANDATORY_HIGH_RID);
    match integrity_of(pid) {
        Some(level) if level > ours => Some(Refusal::ElevatedWindow),
        // The denial is the evidence: see the doc comment.
        None => Some(Refusal::ElevatedWindow),
        Some(_) => None,
    }
}

/// The virtual screen rectangle, in this machine's own desktop coordinates.
fn virtual_screen() -> RECT {
    // SAFETY: four reads of a system metric.
    unsafe {
        let x = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let y = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let w = GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1);
        let h = GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1);
        RECT {
            left: x,
            top: y,
            right: x + w,
            bottom: y + h,
        }
    }
}

/// An absolute desktop coordinate as the `0 .. 65535` `SendInput` expects with
/// [`MOUSEEVENTF_VIRTUALDESK`].
///
/// Pure, and unit tested, because the normalisation is where an absolute pointer
/// lands one pixel out: the mapping divides by `span - 1`, so it is rounded to
/// nearest rather than truncated, which halves the worst case. The residual is up to
/// one pixel on a wide virtual desktop and it is the price of injecting REAL mouse
/// input: `SetCursorPos` would be exact and would not be seen by a game reading
/// through raw input.
fn normalise(value: i32, origin: i32, span: i32) -> i32 {
    let span = span.max(2);
    let offset = i64::from(value - origin).clamp(0, i64::from(span - 1));
    let scaled = (offset * 65535 + i64::from(span - 1) / 2) / i64::from(span - 1);
    scaled.clamp(0, 65535) as i32
}

/// The centre of a rectangle, which is where the pointer is anchored while
/// confined.
///
/// The centre and not a corner, and the reason is truth 1: the delta of one event is
/// `pt` minus the cursor, and `pt` is clamped by Windows to the desktop, so a cursor
/// anchored at the edge would report a delta of zero for the direction it is already
/// against. Anchoring at the centre leaves half a monitor of room before a single
/// event's movement can be clipped.
fn centre(rect: &Rect) -> Point {
    Point {
        x: rect.x.saturating_add(rect.w / 2),
        y: rect.y.saturating_add(rect.h / 2),
    }
}

// ------------------------------------------------------- the hook callbacks

/// The low level keyboard hook.
///
/// It does three things and nothing else: read the atomics, push one upcall, decide
/// whether to swallow. Anything slower would risk the `LowLevelHooksTimeout` that
/// makes Windows stop calling a hook, and a hook that has silently stopped being
/// called is a source whose keystrokes reach the local machine again.
///
/// SAFETY: called by the OS on the thread that installed the hook, with `lparam`
/// pointing at a `KBDLLHOOKSTRUCT` that lives for the duration of the call.
unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code != HC_ACTION {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    }
    let swallow = HOOK_SHARED
        .try_with(|cell| {
            let Some(shared) = cell.get() else {
                return false;
            };
            let mode = shared.mode();
            if mode == MODE_OFF {
                return false;
            }
            // SAFETY: the OS's own structure, valid for this call.
            let info = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
            if info.dwExtraInfo == MARKER {
                // Our own injection coming back round. It never happens in v1 (a
                // machine being driven does not capture) and dropping it costs nothing
                // if that ever changes.
                return false;
            }
            let down = info.flags & LLKHF_UP == 0;
            let extended = info.flags & LLKHF_EXTENDED != 0;
            let vk = info.vkCode as u16;
            let scancode = if extended {
                0xE000 | (info.scanCode as u16 & 0xFF)
            } else {
                info.scanCode as u16 & 0xFF
            };

            // The modifier state, kept here because a swallowed modifier never reaches
            // the OS's own keyboard state.
            let mut mods = shared.mods();
            if let Some(bit) = mod_of_vk(vk) {
                if down {
                    mods |= bit;
                } else {
                    mods &= !bit;
                }
            }
            let usage = usage_of_scancode(scancode).unwrap_or(0);
            let is_lock = usage != 0 && keys::is_lock(usage);
            if is_lock && down {
                // A lock toggles on the press. Tracked rather than read back, for the
                // same reason as the modifiers.
                let bit = match usage & 0xFFFF {
                    0x39 => keys::mods::CAPS,
                    0x53 => keys::mods::NUM,
                    _ => keys::mods::SCROLL,
                };
                mods ^= bit;
            }
            shared.mods.store(u32::from(mods), Ordering::Relaxed);

            // A half duplex lock reports only its press: a target that waited for the
            // release would hold the lock down for ever, and one that replayed the
            // release would toggle it back.
            if is_lock && !down {
                return mode == MODE_SWALLOW;
            }

            let sym = symbol_for(vk, scancode, mods);
            shared.emit(BackendEvent::Key(crate::backend::KeyEvent {
                usage,
                key: (usage != 0)
                    .then(|| keys::name_of(usage))
                    .flatten()
                    .map(str::to_string),
                sym,
                mods,
                down,
                lock: is_lock,
            }));
            mode == MODE_SWALLOW
        })
        .unwrap_or(false);
    if swallow {
        return 1;
    }
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

/// What this machine's own layout produces for a stroke, or `None` for a key that
/// produces nothing (and for a dead key, which `ToUnicodeEx` reports as a negative
/// count).
///
/// The keyboard state handed to `ToUnicodeEx` is built from the modifiers this
/// backend tracks, NOT from `GetKeyboardState`: the pump thread never processes key
/// messages, so its own key state is whatever it was when the thread started. The
/// `0x04` flag is what keeps the call from disturbing the kernel's dead key state,
/// which a person composing an accent on this very machine would otherwise lose.
fn symbol_for(vk: u16, scancode: u16, mods: u16) -> Option<String> {
    let layout = foreground_layout();
    let mut state = [0u8; 256];
    if mods & keys::mods::SHIFT != 0 {
        state[VK_SHIFT as usize] = 0x80;
        state[VK_LSHIFT as usize] = 0x80;
    }
    if mods & keys::mods::CTRL != 0 {
        state[VK_CONTROL as usize] = 0x80;
        state[VK_LCONTROL as usize] = 0x80;
    }
    if mods & keys::mods::ALT != 0 {
        state[VK_MENU as usize] = 0x80;
        state[VK_LMENU as usize] = 0x80;
    }
    if mods & keys::mods::ALTGR != 0 {
        // AltGr IS Control plus Alt as far as the translation tables are concerned,
        // and naming only the right Alt would produce the unshifted character.
        state[VK_CONTROL as usize] = 0x80;
        state[VK_MENU as usize] = 0x80;
        state[VK_RMENU as usize] = 0x80;
    }
    if mods & keys::mods::CAPS != 0 {
        state[VK_CAPITAL as usize] = 0x01;
    }
    let mut buf = [0u16; 8];
    // SAFETY: the buffer and the state array are ours and correctly sized.
    let n = unsafe {
        ToUnicodeEx(
            u32::from(vk),
            u32::from(scancode & 0xFF),
            state.as_ptr(),
            buf.as_mut_ptr(),
            buf.len() as i32,
            0x04,
            layout,
        )
    };
    if n <= 0 {
        return None;
    }
    let text = String::from_utf16_lossy(&buf[..n as usize]);
    // A control character is not a symbol anybody typed: Ctrl plus C produces one,
    // and putting it on the wire would have the target inject a literal 0x03.
    if text.is_empty() || text.chars().any(|c| c.is_control()) {
        return None;
    }
    Some(text)
}

/// The keyboard layout the next injection will be translated by: the FOREGROUND
/// thread's, because on Windows a layout belongs to a thread and not to the machine
/// (truth 5).
fn foreground_layout() -> windows_sys::Win32::UI::Input::KeyboardAndMouse::HKL {
    // SAFETY: three reads; a null foreground window falls back to our own thread's
    // layout, which is the system default.
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_null() {
            return GetKeyboardLayout(0);
        }
        let mut pid = 0u32;
        let tid = GetWindowThreadProcessId(hwnd, &mut pid);
        GetKeyboardLayout(tid)
    }
}

/// The low level mouse hook. See [`keyboard_hook`] for why it does so little.
///
/// SAFETY: called by the OS on the installing thread, with `lparam` pointing at an
/// `MSLLHOOKSTRUCT` that lives for the duration of the call.
unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code != HC_ACTION {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    }
    let swallow = HOOK_SHARED
        .try_with(|cell| {
            let Some(shared) = cell.get() else {
                return false;
            };
            let mode = shared.mode();
            if mode == MODE_OFF {
                return false;
            }
            // SAFETY: the OS's own structure, valid for this call.
            let info = unsafe { &*(lparam as *const MSLLHOOKSTRUCT) };
            if info.dwExtraInfo == MARKER {
                return false;
            }
            let swallow = mode == MODE_SWALLOW;
            match wparam as u32 {
                WM_MOUSEMOVE => {
                    // Truth 1: the cursor has NOT moved yet, so this subtraction is the
                    // accelerated delta of this one event. `at` is where the pointer
                    // actually is, which keeps it inside this machine's desktop even
                    // when the intended point is not, and the engine adds the delta back
                    // on to ask whether an edge was crossed.
                    let mut cur = POINT { x: 0, y: 0 };
                    // SAFETY: an out parameter we own.
                    if unsafe { GetCursorPos(&mut cur) } == 0 {
                        return swallow;
                    }
                    let (dx, dy) = (info.pt.x - cur.x, info.pt.y - cur.y);
                    if dx != 0 || dy != 0 {
                        shared.emit(BackendEvent::Motion(crate::backend::Motion {
                            at: Point { x: cur.x, y: cur.y },
                            dx,
                            dy,
                        }));
                    }
                    // The pointer only drifts off the anchor if something else moved it
                    // (an injection by another tool, or a hook Windows dropped), because
                    // a swallowed move never reaches the cursor. So this is a no-op in
                    // the ordinary case and the repair in the case that matters.
                    if shared.confined.load(Ordering::Relaxed) {
                        let (ax, ay) = (
                            shared.anchor_x.load(Ordering::Relaxed),
                            shared.anchor_y.load(Ordering::Relaxed),
                        );
                        if cur.x != ax || cur.y != ay {
                            // SAFETY: a coordinate pair.
                            unsafe { SetCursorPos(ax, ay) };
                        }
                    }
                }
                WM_LBUTTONDOWN => shared.emit(button(1, true)),
                WM_LBUTTONUP => shared.emit(button(1, false)),
                WM_MBUTTONDOWN => shared.emit(button(2, true)),
                WM_MBUTTONUP => shared.emit(button(2, false)),
                WM_RBUTTONDOWN => shared.emit(button(3, true)),
                WM_RBUTTONUP => shared.emit(button(3, false)),
                WM_XBUTTONDOWN | WM_XBUTTONUP => {
                    let which = (info.mouseData >> 16) as u16;
                    let n = if which == XBUTTON1 { 4 } else { 5 };
                    shared.emit(button(n, wparam as u32 == WM_XBUTTONDOWN));
                }
                WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                    let raw = i32::from((info.mouseData >> 16) as i16);
                    let horizontal = wparam as u32 == WM_MOUSEHWHEEL;
                    let acc = if horizontal {
                        &shared.wheel_x
                    } else {
                        &shared.wheel_y
                    };
                    // Whole notches out, the remainder kept: a high resolution wheel
                    // sends a fraction of a notch per event, and rounding each one to
                    // zero would make it scroll nothing at all.
                    let total = acc.fetch_add(raw, Ordering::Relaxed) + raw;
                    let notches = total / WHEEL_DELTA_I32;
                    if notches != 0 {
                        acc.fetch_sub(notches * WHEEL_DELTA_I32, Ordering::Relaxed);
                        let (dx, dy) = if horizontal {
                            (notches, 0)
                        } else {
                            (0, notches)
                        };
                        shared.emit(BackendEvent::Wheel {
                            dx,
                            dy,
                            pixels: false,
                        });
                    }
                }
                _ => {}
            }
            swallow
        })
        .unwrap_or(false);
    if swallow {
        return 1;
    }
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

fn button(button: u8, down: bool) -> BackendEvent {
    BackendEvent::Button { button, down }
}

// ------------------------------------------------------------ the trait impl

impl InputBackend for WindowsBackend {
    fn capabilities(&self) -> Capabilities {
        let capture = self.shared.capture_ok.load(Ordering::Relaxed);
        Capabilities {
            capture,
            // A low level hook can consume what it sees, for both devices: proved by
            // #123 on this very machine.
            swallow: capture,
            // `ClipCursor` pins, and the hook's own `pt` is the OS relative source
            // the other half of this promise asks for (truth 1).
            confine: capture,
            warp: true,
            inject_keys: true,
            inject_pointer: true,
            // `KEYEVENTF_UNICODE`, which is why a dead key resolution answering
            // `None` costs nothing here (truth 6 of the seam).
            unicode: true,
            monitors_stable: self.shared.monitors_stable.load(Ordering::Relaxed),
            problem: (!capture).then_some(Problem::NoPermission),
        }
    }

    fn monitors(&self) -> impl Future<Output = Vec<Monitor>> + Send {
        // Inline: `EnumDisplayMonitors` has no thread affinity, so there is nothing
        // to wait for and the future is ready.
        let monitors = enumerate_monitors();
        self.shared.monitors_stable.store(
            monitors.iter().all(|m| m.id.starts_with("win:dev:")),
            Ordering::Relaxed,
        );
        std::future::ready(monitors)
    }

    fn pointer(&self) -> impl Future<Output = Option<Point>> + Send {
        let mut pt = POINT { x: 0, y: 0 };
        // SAFETY: an out parameter we own.
        let ok = unsafe { GetCursorPos(&mut pt) } != 0;
        std::future::ready(ok.then_some(Point { x: pt.x, y: pt.y }))
    }

    fn resolve(&self, want: Want) -> impl Future<Output = Option<Resolved>> + Send {
        std::future::ready(resolve_now(&want))
    }

    fn capture(&self, mode: CaptureMode) {
        // The one downcall that must reach the pump thread: a low level hook lives
        // on the thread that installed it.
        self.push(Cmd::Capture(mode));
    }

    fn confine(&self, rect: Option<Rect>) {
        match rect {
            Some(rect) => {
                let anchor = centre(&rect);
                self.shared.anchor_x.store(anchor.x, Ordering::Relaxed);
                self.shared.anchor_y.store(anchor.y, Ordering::Relaxed);
                self.shared.confined.store(true, Ordering::Relaxed);
                if let Ok(mut clip) = self.clip.lock() {
                    *clip = Some(rect);
                }
                apply_clip(Some(rect));
                // SAFETY: a coordinate pair. The pointer starts at the anchor so
                // there is a whole half monitor of room in every direction before
                // Windows clips a single event's movement.
                unsafe { SetCursorPos(anchor.x, anchor.y) };
            }
            None => {
                self.shared.confined.store(false, Ordering::Relaxed);
                if let Ok(mut clip) = self.clip.lock() {
                    *clip = None;
                }
                apply_clip(None);
            }
        }
    }

    fn warp(&self, to: Point) {
        // SAFETY: a coordinate pair.
        unsafe { SetCursorPos(to.x, to.y) };
    }

    fn inject(&self, actions: Vec<Action>) {
        if actions.is_empty() {
            return;
        }
        // Truth 6, and the only order that does not lie: ask whether anybody is
        // looking at this desktop BEFORE typing into it, because a locked machine
        // accepts the injection and shows it to nobody.
        if !self.desktop_is_ours(false) {
            self.shared
                .emit(BackendEvent::Refused(Refusal::ScreenLocked));
            return;
        }
        let vs = virtual_screen();
        let mut inputs: Vec<INPUT> = Vec::with_capacity(actions.len() * 2);
        for action in &actions {
            push_input(&mut inputs, action, &vs);
        }
        if inputs.is_empty() {
            return;
        }
        // SAFETY: a slice we built, with the size the API asks for.
        let sent = unsafe {
            SendInput(
                inputs.len() as u32,
                inputs.as_ptr(),
                std::mem::size_of::<INPUT>() as i32,
            )
        };
        if sent as usize == inputs.len() {
            return;
        }
        // Detected, not guessed: the desktop is re-checked (it may have changed
        // between the check above and the call) and the foreground window's token is
        // read. When neither names a cause, the raw error is logged rather than a
        // sentence being invented.
        let ours = self.desktop_is_ours(true);
        match refusal_for(ours) {
            Some(refusal) => self.shared.emit(BackendEvent::Refused(refusal)),
            None => {
                // SAFETY: reading the thread's last error.
                let err = unsafe { windows_sys::Win32::Foundation::GetLastError() };
                warn(&format!(
                    "SendInput accepted {sent} of {} events, error {err}, and nothing \
                     here can say why",
                    inputs.len()
                ));
            }
        }
    }

    fn release_all(&self, keys: Vec<PlatformKey>) {
        if keys.is_empty() {
            return;
        }
        let actions: Vec<Action> = keys
            .into_iter()
            .map(|code| Action::Key { code, down: false })
            .collect();
        // Through the ordinary path, so a release is subject to the same refusal
        // detection as anything else. It must work while anything else is in flight,
        // and it does: `SendInput` is callable from any thread and takes no lock of
        // ours.
        self.inject(actions);
    }

    fn request_exit(&self, code: i32) {
        // Never `std::process::exit` here: this runs on the engine's thread, and
        // exiting would skip the teardown on a machine that may be holding somebody's
        // modifiers down.
        self.push(Cmd::Exit(code));
    }
}

/// Applies or releases the cursor clip.
fn apply_clip(rect: Option<Rect>) {
    match rect {
        Some(rect) => {
            let r = RECT {
                left: rect.x,
                top: rect.y,
                right: rect.x.saturating_add(rect.w),
                bottom: rect.y.saturating_add(rect.h),
            };
            // SAFETY: a rectangle we own.
            unsafe { ClipCursor(&r) };
        }
        // SAFETY: a null rectangle is the documented release.
        None => unsafe {
            ClipCursor(std::ptr::null());
        },
    }
}

/// Turns one [`Action`] into the `INPUT` records it needs.
fn push_input(out: &mut Vec<INPUT>, action: &Action, vs: &RECT) {
    match action {
        Action::MoveTo(to) => {
            let nx = normalise(to.x, vs.left, vs.right - vs.left);
            let ny = normalise(to.y, vs.top, vs.bottom - vs.top);
            out.push(mouse_input(
                nx,
                ny,
                0,
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
            ));
        }
        Action::MoveBy { dx, dy } => {
            // Truth 3: Windows discards a relative move of (0, 0), so it is not sent
            // rather than sent and lost.
            if *dx != 0 || *dy != 0 {
                out.push(mouse_input(*dx, *dy, 0, MOUSEEVENTF_MOVE));
            }
        }
        Action::Button { button, down } => {
            let (flag, data) = match (button, down) {
                (1, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                (1, false) => (MOUSEEVENTF_LEFTUP, 0),
                (2, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                (2, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                (3, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                (3, false) => (MOUSEEVENTF_RIGHTUP, 0),
                (4, true) => (MOUSEEVENTF_XDOWN, u32::from(XBUTTON1)),
                (4, false) => (MOUSEEVENTF_XUP, u32::from(XBUTTON1)),
                (5, true) => (MOUSEEVENTF_XDOWN, u32::from(XBUTTON2_ID)),
                (5, false) => (MOUSEEVENTF_XUP, u32::from(XBUTTON2_ID)),
                // The engine checks the button against a closed set as it leaves the
                // wire, so this is unreachable; dropping is still the right answer
                // for an impossible value.
                _ => return,
            };
            out.push(mouse_input(0, 0, data, flag));
        }
        Action::Wheel { dx, dy, pixels } => {
            // Windows measures the wheel in `WHEEL_DELTA` units, so a notch is 120
            // and a pixel precise scroll travels as the fraction of a notch that a
            // high resolution wheel itself reports.
            let scale = if *pixels { 1 } else { WHEEL_DELTA_I32 };
            if *dy != 0 {
                out.push(mouse_input(0, 0, (dy * scale) as u32, MOUSEEVENTF_WHEEL));
            }
            if *dx != 0 {
                out.push(mouse_input(0, 0, (dx * scale) as u32, MOUSEEVENTF_HWHEEL));
            }
        }
        Action::Key { code, down } => {
            out.push(key_input(*code, *down));
        }
        Action::Text(text) => {
            // One event pair per UTF-16 code unit, so a character outside the basic
            // plane travels as its surrogate pair, which is what Windows expects.
            for unit in text.encode_utf16() {
                out.push(unicode_input(unit, false));
                out.push(unicode_input(unit, true));
            }
        }
        Action::Group(_) => {
            // Windows switches whole layouts rather than groups, and a `resolve` here
            // never names one, so the engine never emits one. A no-op and not a
            // panic, deliberately: the seam says so.
        }
    }
}

fn mouse_input(dx: i32, dy: i32, data: u32, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: MARKER,
            },
        },
    }
}

/// One key press or release.
///
/// By SCANCODE where we have one, which is what a game reading through raw input
/// sees and what no layout can move, with the virtual key as the fallback for a key
/// that has no scancode (the media keys). The `0xE0` high byte of
/// [`PlatformKey::detail`] becomes [`KEYEVENTF_EXTENDEDKEY`].
fn key_input(code: PlatformKey, down: bool) -> INPUT {
    let scancode = (code.detail & 0xFF) as u16;
    let extended = code.detail & 0xFF00 != 0;
    let mut flags = 0u32;
    if !down {
        flags |= KEYEVENTF_KEYUP;
    }
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if scancode != 0 {
        flags |= KEYEVENTF_SCANCODE;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                // The virtual key travels even on the scancode path: Windows ignores
                // it there, and a debugger reading the batch can still see which key
                // was meant.
                wVk: code.code as u16,
                wScan: scancode,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: MARKER,
            },
        },
    }
}

fn unicode_input(unit: u16, up: bool) -> INPUT {
    let mut flags = KEYEVENTF_UNICODE;
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: 0,
                wScan: unit,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: MARKER,
            },
        },
    }
}

/// How this machine can produce `want` on the layout the next injection will use.
fn resolve_now(want: &Want) -> Option<Resolved> {
    let layout = foreground_layout();
    match want {
        Want::Usage(usage) => platform_key(*usage, layout).map(|code| Resolved {
            code,
            mods: 0,
            prefix: None,
            group: None,
        }),
        Want::Named(name) => {
            let usage = keys::usage_of(name)?;
            platform_key(usage, layout).map(|code| Resolved {
                code,
                mods: 0,
                prefix: None,
                group: None,
            })
        }
        Want::Symbol(sym) => {
            // One character: `VkKeyScanExW` answers about one, and a longer string
            // is the Unicode path's business rather than a key's.
            let mut chars = sym.chars();
            let ch = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            let mut buf = [0u16; 2];
            let units = ch.encode_utf16(&mut buf);
            if units.len() != 1 {
                // Outside the basic plane: no single key produces it, and
                // `KEYEVENTF_UNICODE` will.
                return None;
            }
            // SAFETY: a scalar and a layout handle.
            let scan = unsafe { VkKeyScanExW(units[0], layout) };
            // Minus one is the documented "this layout cannot produce it with one
            // stroke", which includes every character that needs a DEAD KEY (truth 6
            // of the seam). Answering `None` is covered: Windows always has the
            // Unicode path.
            if scan == -1 {
                return None;
            }
            let vk = (scan as u16) & 0xFF;
            let shift_state = ((scan as u16) >> 8) & 0xFF;
            let mut mods = 0u16;
            if shift_state & 0x01 != 0 {
                mods |= keys::mods::SHIFT;
            }
            // Control plus Alt IS AltGr, and reporting the two separately would have
            // the engine press a left Alt that produces nothing on the level the
            // character lives on.
            if shift_state & 0x06 == 0x06 {
                mods |= keys::mods::ALTGR;
            } else {
                if shift_state & 0x02 != 0 {
                    mods |= keys::mods::CTRL;
                }
                if shift_state & 0x04 != 0 {
                    mods |= keys::mods::ALT;
                }
            }
            Some(Resolved {
                code: canonical_key(vk, layout),
                mods,
                prefix: None,
                group: None,
            })
        }
    }
}

/// The platform key for a usage: its scancode names a virtual key through the
/// layout, and the pair is then canonicalised.
fn platform_key(
    usage: u32,
    layout: windows_sys::Win32::UI::Input::KeyboardAndMouse::HKL,
) -> Option<PlatformKey> {
    if let Some(scancode) = scancode_of(usage) {
        // SAFETY: a scancode and a layout handle.
        let vk = unsafe { MapVirtualKeyExW(u32::from(scancode), MAPVK_VSC_TO_VK_EX, layout) };
        if vk != 0 {
            return Some(PlatformKey {
                code: vk,
                // The scancode from the TABLE and not from a round trip through the
                // virtual key: this is the physical key the source pressed, and
                // `MAPVK_VK_TO_VSC_EX` would answer about whichever key the layout
                // currently binds that virtual key to.
                detail: u32::from(scancode),
            });
        }
        // A scancode the layout does not bind is still a scancode the OS will
        // deliver: injected by scancode alone, it reaches the foreground window as
        // the key at that position.
        return Some(PlatformKey {
            code: 0,
            detail: u32::from(scancode),
        });
    }
    USAGE_VK
        .iter()
        .find(|(u, _)| *u == usage)
        .map(|(_, vk)| canonical_key(*vk, layout))
}

/// One virtual key with the scancode this layout gives it.
///
/// Always derived the same way, and that is the point: a `PlatformKey` is compared
/// whole to decide whether an incoming release is a key the engine is holding, so
/// two routes to the same physical key that disagreed about `detail` would leave that
/// key physically DOWN. Everything that starts from a virtual key comes through here.
fn canonical_key(
    vk: u16,
    layout: windows_sys::Win32::UI::Input::KeyboardAndMouse::HKL,
) -> PlatformKey {
    // SAFETY: a virtual key and a layout handle.
    let scancode = unsafe { MapVirtualKeyExW(u32::from(vk), MAPVK_VK_TO_VSC_EX, layout) };
    PlatformKey {
        code: u32::from(vk),
        detail: scancode,
    }
}

/// This machine's monitors.
///
/// In the coordinate space `GetCursorPos` and `SendInput` use, which after the
/// per monitor awareness this backend declares is PHYSICAL pixels, with `scale`
/// saying each monitor's DPI. The alternative (leaving the process DPI unaware) puts
/// every monitor into a space scaled by the PRIMARY monitor's factor, which is right
/// for one screen and wrong for a second one at a different scale.
fn enumerate_monitors() -> Vec<Monitor> {
    let mut out: Vec<Monitor> = Vec::new();
    // SAFETY: the callback receives a pointer to our own vector and does nothing
    // else with it; `EnumDisplayMonitors` calls it synchronously.
    unsafe {
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(monitor_callback),
            &mut out as *mut Vec<Monitor> as LPARAM,
        );
    }
    out
}

/// SAFETY: `data` must be a `*mut Vec<Monitor>` and `monitor` a live `HMONITOR`.
unsafe extern "system" fn monitor_callback(
    monitor: HMONITOR,
    _dc: HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> windows_sys::core::BOOL {
    let out = unsafe { &mut *(data as *mut Vec<Monitor>) };
    let mut info = MONITORINFOEXW {
        monitorInfo: MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFOEXW>() as u32,
            rcMonitor: RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            },
            rcWork: RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            },
            dwFlags: 0,
        },
        szDevice: [0u16; 32],
    };
    let ok = unsafe {
        GetMonitorInfoW(
            monitor,
            (&mut info as *mut MONITORINFOEXW).cast::<MONITORINFO>(),
        )
    };
    if ok == 0 {
        return 1;
    }
    let rect = info.monitorInfo.rcMonitor;
    let mut dpi_x: u32 = 96;
    let mut dpi_y: u32 = 96;
    // SAFETY: two out parameters we own. A failure leaves them at 96, which is 100%.
    unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
    let device = {
        let len = info
            .szDevice
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(info.szDevice.len());
        String::from_utf16_lossy(&info.szDevice[..len])
    };
    // SAFETY: the device name comes from the OS and the structure is sized for it.
    let (id, name) = unsafe { display_identity(&device) };
    out.push(Monitor {
        id,
        name,
        w: rect.right - rect.left,
        h: rect.bottom - rect.top,
        x: rect.left,
        y: rect.top,
        scale: (dpi_x.max(1) as i32) * 1000 / 96,
        primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
    });
    1
}

/// A monitor's stable identity and the name a person would read.
///
/// `szDevice` is `\\.\DISPLAY1`, which is a SLOT and not a monitor: unplug two
/// screens and plug them back the other way round and the names have swapped. The
/// device interface name is the hardware's own (`\\?\DISPLAY#GSM5B08#...`), so it is
/// what the identity is built from, and a monitor whose interface name cannot be read
/// gets the slot with a prefix that says so, which is what makes
/// [`Capabilities::monitors_stable`] false.
///
/// SAFETY: `device` must be a display device name the OS gave us.
unsafe fn display_identity(device: &str) -> (String, String) {
    let wide_device = wide(device);
    let mut dd = DISPLAY_DEVICEW {
        cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
        DeviceName: [0u16; 32],
        DeviceString: [0u16; 128],
        StateFlags: 0,
        DeviceID: [0u16; 128],
        DeviceKey: [0u16; 128],
    };
    // `EDD_GET_DEVICE_INTERFACE_NAME`, which puts the hardware's own interface path
    // in `DeviceID`. Hard-coded for the reason the other constants here are.
    const EDD_GET_DEVICE_INTERFACE_NAME: u32 = 0x0000_0001;
    let ok = unsafe {
        EnumDisplayDevicesW(
            wide_device.as_ptr(),
            0,
            &mut dd,
            EDD_GET_DEVICE_INTERFACE_NAME,
        )
    };
    let readable = |buf: &[u16]| {
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..len])
    };
    let friendly = {
        let s = readable(&dd.DeviceString);
        if s.is_empty() { device.to_string() } else { s }
    };
    if ok == 0 {
        return (format!("win:slot:{device}"), friendly);
    }
    let interface = readable(&dd.DeviceID);
    if interface.is_empty() {
        return (format!("win:slot:{device}"), friendly);
    }
    (format!("win:dev:{interface}"), friendly)
}

// ------------------------------------------------------------- the pump side

/// Backend state, living on the pump thread. It owns the raw window handle and the
/// two hook handles, so it is not `Send`: a low level hook must be removed by the
/// thread that installed it, which is the one fact that pins this whole structure.
struct Backend {
    hwnd: HWND,
    cmds: Arc<Mutex<VecDeque<Cmd>>>,
    shared: Arc<Shared>,
    clip: Arc<Mutex<Option<Rect>>>,
    keyboard: HHOOK,
    mouse: HHOOK,
    shutdown: bool,
    exit_code: Option<i32>,
    /// The layout the last poll saw, so a change is reported once.
    layout: usize,
    /// A cheap fingerprint of the monitor set, for the same reason: a message-only
    /// window is not a top level window and receives no `WM_DISPLAYCHANGE`
    /// broadcast, so the set has to be watched rather than waited for.
    monitors: (i32, RECT),
}

impl Backend {
    fn new(
        cmds: Arc<Mutex<VecDeque<Cmd>>>,
        shared: Arc<Shared>,
        clip: Arc<Mutex<Option<Rect>>>,
    ) -> Result<Backend, Unsupported> {
        // SAFETY: a window class and a message-only window, both ours, with every
        // failure checked before the next call uses its result.
        unsafe {
            let hinstance = GetModuleHandleW(std::ptr::null());
            let class_name = wide(&format!(
                "1device-input-window-{}",
                CLASS_SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            let wc = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(wndproc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinstance,
                hIcon: std::ptr::null_mut(),
                hCursor: std::ptr::null_mut(),
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: class_name.as_ptr(),
            };
            if RegisterClassW(&wc) == 0 {
                return Err(Unsupported(Problem::NoBackend));
            }
            let window_name = wide("1device-input");
            let hwnd = CreateWindowExW(
                0,
                class_name.as_ptr(),
                window_name.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE as HWND,
                std::ptr::null_mut(),
                hinstance,
                std::ptr::null(),
            );
            if hwnd.is_null() {
                return Err(Unsupported(Problem::NoBackend));
            }
            Ok(Backend {
                hwnd,
                cmds,
                shared,
                clip,
                keyboard: std::ptr::null_mut(),
                mouse: std::ptr::null_mut(),
                shutdown: false,
                exit_code: None,
                layout: 0,
                monitors: (0, virtual_screen()),
            })
        }
    }

    /// The main-thread pump. Commands first, then OS messages, then the periodic
    /// work, then a bounded idle wait, exactly as the clipboard's loop is ordered and
    /// for the same reason: a command must be able to land before the OS event that
    /// would otherwise race it.
    fn run(&mut self) -> i32 {
        // The hooks' way back to the shared state. Set before any hook can be
        // installed, and only ever on this thread.
        HOOK_SHARED.with(|cell| {
            let _ = cell.set(Arc::clone(&self.shared));
        });
        // Driven through a raw self pointer, never a live `&mut self` held across
        // `DispatchMessageW`: that call re-enters `wndproc`, which re-derives
        // `&mut Backend` from `GWLP_USERDATA`, and two live `&mut` would be aliasing
        // undefined behaviour. Every access below is a transient reborrow.
        let this: *mut Backend = self;
        // SAFETY: publishing the pointer for `wndproc`, and un-published in `Drop`.
        unsafe {
            SetWindowLongPtrW((*this).hwnd, GWLP_USERDATA, this as isize);
        }
        loop {
            unsafe { (*this).process_cmds() };
            let mut msg = MSG {
                hwnd: std::ptr::null_mut(),
                message: 0,
                wParam: 0,
                lParam: 0,
                time: 0,
                pt: POINT { x: 0, y: 0 },
            };
            // SAFETY: a message structure we own. `PeekMessageW` also dispatches
            // pending sent messages before returning.
            while unsafe { PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
                unsafe {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            unsafe { (*this).periodic() };
            if unsafe { (*this).shutdown } {
                unsafe { (*this).release_everything() };
                return unsafe { (*this).exit_code }.unwrap_or(1);
            }
            // A bounded wait, so a lost wake and a set `shutdown` are always seen,
            // and so the periodic work happens without a message.
            // SAFETY: no handles to wait on, just the message queue.
            unsafe {
                MsgWaitForMultipleObjectsEx(
                    0,
                    std::ptr::null(),
                    SHUTDOWN_POLL_CAP.as_millis() as u32,
                    QS_ALLINPUT,
                    MWMO_INPUTAVAILABLE,
                );
            }
        }
    }

    fn process_cmds(&mut self) {
        // The whole queue under one lock, and a poisoned mutex recovered so an
        // `Exit` is never dropped.
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

    /// Installs, keeps or removes the hooks.
    ///
    /// The MODE is published before the hooks are installed and after they are
    /// removed, and the order is the whole safety of this function: a hook installed
    /// while the mode still said `Off` would pass its first events through, and a
    /// mode left saying `Swallow` after the hooks were removed would claim this
    /// machine is still consuming keystrokes it is not.
    fn set_capture(&mut self, mode: CaptureMode) {
        let want = match mode {
            CaptureMode::Off => MODE_OFF,
            CaptureMode::Watch => MODE_WATCH,
            CaptureMode::Swallow => MODE_SWALLOW,
        };
        if want == MODE_OFF {
            self.shared.mode.store(MODE_OFF, Ordering::Relaxed);
            self.unhook();
            return;
        }
        if self.keyboard.is_null() && self.mouse.is_null() {
            // Coming back from Off, so the modifier state this backend keeps has
            // been blind for however long that lasted: a person who pressed and let
            // go of Shift in between would leave a Shift bit set for ever. While
            // nothing is hooked the OS's own state IS authoritative (nothing was
            // swallowed), so this is the one moment it can be trusted, and it is the
            // only moment it is read.
            self.shared
                .mods
                .store(u32::from(live_mods() | initial_locks()), Ordering::Relaxed);
        }
        self.shared.mode.store(want, Ordering::Relaxed);
        if !self.keyboard.is_null() && !self.mouse.is_null() {
            return;
        }
        // SAFETY: two hook installations with our own callbacks, on this thread.
        unsafe {
            if self.keyboard.is_null() {
                self.keyboard =
                    SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), std::ptr::null_mut(), 0);
            }
            if self.mouse.is_null() {
                self.mouse =
                    SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), std::ptr::null_mut(), 0);
            }
        }
        if self.keyboard.is_null() || self.mouse.is_null() {
            warn("the low level input hooks could not be installed");
            self.shared.mode.store(MODE_OFF, Ordering::Relaxed);
            self.unhook();
            self.shared.capture_ok.store(false, Ordering::Relaxed);
            self.shared
                .emit(BackendEvent::CaptureLost(CaptureLoss::Broken));
        }
    }

    fn unhook(&mut self) {
        // SAFETY: handles this thread installed, each removed once and then
        // forgotten, so a second call cannot unhook twice.
        unsafe {
            if !self.keyboard.is_null() {
                UnhookWindowsHookEx(self.keyboard);
                self.keyboard = std::ptr::null_mut();
            }
            if !self.mouse.is_null() {
                UnhookWindowsHookEx(self.mouse);
                self.mouse = std::ptr::null_mut();
            }
        }
    }

    /// The work that has no message to hang off: the layout, the monitor set, and a
    /// clip Windows dropped.
    ///
    /// SAFETY: called from `run` with no other borrow of `self` live.
    unsafe fn periodic(&mut self) {
        // The layout belongs to the foreground thread (truth 5), so it changes by
        // alt-tabbing as well as by a person switching one, and both are real: the
        // next injection will be translated by whatever is active now.
        let layout = foreground_layout() as usize;
        if layout != 0 && layout != self.layout {
            let first = self.layout == 0;
            self.layout = layout;
            if !first {
                self.shared.emit(BackendEvent::LayoutChanged {
                    // The handle itself is the identity: its low word is the language
                    // and its high word the layout, it is stable for the session, and
                    // `GetKeyboardLayoutNameW` can only answer about the CALLING
                    // thread's layout, which is never the one we care about.
                    layout: format!("win:{layout:08x}"),
                    // Windows has no keyboard group in the X11 sense: it switches
                    // whole layouts, and each one is its own identity above.
                    group: 0,
                });
            }
        }

        // SAFETY: one system metric and one virtual screen read.
        let count = unsafe { GetSystemMetrics(SM_CMONITORS) };
        let vs = virtual_screen();
        let fingerprint = (count, vs);
        if fingerprint.0 != self.monitors.0 || !same_rect(&fingerprint.1, &self.monitors.1) {
            let first = self.monitors.0 == 0;
            self.monitors = fingerprint;
            if !first {
                self.shared.emit(BackendEvent::MonitorsChanged);
            }
        }

        // Windows drops the cursor clip on a desktop switch, a display change and
        // whenever another process clips. A session whose pin quietly went away is a
        // pointer that walks off this machine while its owner is looking at another
        // one, so it is put back.
        if self.shared.confined.load(Ordering::Relaxed) {
            let wanted = match self.clip.lock() {
                Ok(g) => *g,
                Err(p) => *p.into_inner(),
            };
            if let Some(rect) = wanted {
                let mut current = RECT {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 0,
                };
                // SAFETY: an out parameter we own.
                let ok = unsafe { GetClipCursor(&mut current) } != 0;
                let want = RECT {
                    left: rect.x,
                    top: rect.y,
                    right: rect.x.saturating_add(rect.w),
                    bottom: rect.y.saturating_add(rect.h),
                };
                if !ok || !same_rect(&current, &want) {
                    apply_clip(Some(rect));
                }
            }
        }
    }

    /// The last thing this loop does, whatever ended it, and the moment a keyboard is
    /// left working or left dead.
    ///
    /// SAFETY: called from `run` or from `Drop`, on the thread that installed the
    /// hooks.
    unsafe fn release_everything(&mut self) {
        self.shared.mode.store(MODE_OFF, Ordering::Relaxed);
        self.unhook();
        self.shared.confined.store(false, Ordering::Relaxed);
        apply_clip(None);
    }
}

fn same_rect(a: &RECT, b: &RECT) -> bool {
    a.left == b.left && a.top == b.top && a.right == b.right && a.bottom == b.bottom
}

impl Drop for Backend {
    /// The safety net at thread death: a swallow that outlives its process is a
    /// machine with a dead keyboard, and a clip that outlives it is a mouse stuck in
    /// a corner. Both are undone here as well as on the normal path, because a panic
    /// on the pump thread would skip the normal path.
    fn drop(&mut self) {
        // SAFETY: on the pump thread, which is where the hooks were installed.
        unsafe {
            self.release_everything();
            SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0);
            DestroyWindow(self.hwnd);
        }
    }
}

/// Whether the process's DPI awareness has been declared. See [`create`].
static DPI_DECLARED: AtomicBool = AtomicBool::new(false);

/// Unique window class suffix per backend instance: avoids
/// `ERROR_CLASS_ALREADY_EXISTS` when two backends coexist in one process, which the
/// live suite does.
static CLASS_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Message router for the backend window.
///
/// Only the wake message borrows the backend, and it is posted rather than sent, so
/// it can never be re-entrant. `WM_DESTROY` is a no-op that touches nothing: it
/// arrives from our own `DestroyWindow` inside `Drop`, where the `&mut Backend` is
/// already held.
///
/// SAFETY: called by the OS on the pump thread.
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_DESTROY => 0,
        WM_APP_CMD => {
            let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut Backend;
            if ptr.is_null() {
                return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
            }
            // SAFETY: the pointer was published by `run` on this thread and cleared
            // before the window is destroyed, and this message is posted rather than
            // sent, so no other borrow can be live.
            let backend = unsafe { &mut *ptr };
            backend.process_cmds();
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Owns the pinned [`Backend`]; `run` is the blocking main-thread pump and its
/// return value is the process exit code.
pub struct WindowsLoop {
    backend: Backend,
}

impl WindowsLoop {
    pub fn run(self) -> i32 {
        let WindowsLoop { mut backend } = self;
        let code = backend.run();
        // Dropped before returning, so the hooks and the clip are gone before the
        // process exits rather than at whatever moment the runtime decides.
        drop(backend);
        code
    }
}

/// Builds the Windows backend.
///
/// # The platform half of the crash guard
///
/// The first thing this does is release a cursor clip, and it is not paranoia. The
/// engine's own crash guard (`held.json`) un-sticks keys a dead run left down, and
/// the two low level hooks need no guard at all because Windows removes a hook when
/// the process that owns it dies. The cursor clip is the one piece of this backend's
/// state that a dead process cannot clean up itself, so a fresh one clears whatever
/// its predecessor may have left, before it does anything else. The supervisor
/// restarts this component, so that moment always comes.
pub fn create() -> Result<crate::os::Created, Unsupported> {
    // Per monitor awareness FIRST, before any window exists and before any
    // coordinate is read: it cannot be changed afterwards, and it is what makes the
    // whole coordinate space one thing (see `enumerate_monitors`).
    // Once per process, and only the FIRST attempt can say anything: the awareness of
    // a process cannot be changed after it is set, so a second backend in one process
    // (which the live suite builds) is denied and that denial means the first one
    // succeeded. Warning about it would have every test run print a line about a
    // problem that does not exist.
    if !DPI_DECLARED.swap(true, Ordering::Relaxed) {
        // SAFETY: one call with a documented context handle.
        let ok =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        if ok == 0 {
            warn(
                "this build could not declare per monitor DPI awareness; monitor sizes \
                 will be reported in the primary screen's scaled space",
            );
        }
    }
    apply_clip(None);

    let cmds: Arc<Mutex<VecDeque<Cmd>>> = Arc::new(Mutex::new(VecDeque::new()));
    let (events_tx, backend_events) = mpsc::channel(BACKEND_EVENT_CAPACITY);
    let shared = Arc::new(Shared {
        mode: AtomicU8::new(MODE_OFF),
        events_tx,
        anchor_x: AtomicI32::new(0),
        anchor_y: AtomicI32::new(0),
        confined: AtomicBool::new(false),
        mods: AtomicU32::new(u32::from(initial_locks())),
        wheel_x: AtomicI32::new(0),
        wheel_y: AtomicI32::new(0),
        capture_ok: AtomicBool::new(true),
        monitors_stable: AtomicBool::new(true),
        dropped: AtomicU32::new(0),
    });
    let clip: Arc<Mutex<Option<Rect>>> = Arc::new(Mutex::new(None));
    let backend = Backend::new(Arc::clone(&cmds), Arc::clone(&shared), Arc::clone(&clip))?;
    let handle = WindowsBackend {
        hwnd: backend.hwnd as isize,
        cmds,
        shared,
        clip,
        desktop: Arc::new(Mutex::new(Desktop {
            ours: true,
            checked: None,
        })),
    };
    Ok(crate::os::Created {
        handle: crate::os::Backend::Windows(handle),
        backend_events,
        event_loop: crate::os::EventLoop::Windows(Box::new(WindowsLoop { backend })),
    })
}

/// The holdable modifiers the OS believes are down right now.
///
/// Only ever read while nothing is hooked (see [`Backend::set_capture`]): once this
/// backend swallows, a modifier the user is holding never reaches the state
/// `GetAsyncKeyState` reports, and believing it then would type every symbol in the
/// wrong case.
fn live_mods() -> u16 {
    // SAFETY: five reads of a virtual key's asynchronous state.
    unsafe {
        let down = |vk: u16| GetAsyncKeyState(vk as i32) as u16 & 0x8000 != 0;
        let mut bits = 0u16;
        if down(VK_LSHIFT) || down(VK_RSHIFT) {
            bits |= keys::mods::SHIFT;
        }
        if down(VK_LCONTROL) {
            bits |= keys::mods::CTRL;
        }
        if down(VK_LMENU) {
            bits |= keys::mods::ALT;
        }
        if down(VK_RMENU) {
            bits |= keys::mods::ALTGR;
        }
        if down(VK_LWIN) || down(VK_RWIN) {
            bits |= keys::mods::META;
        }
        // The right Control is deliberately folded into CTRL and the right Alt into
        // ALTGR, matching `mod_of_vk` and `keys::mod_of_usage`: a disagreement
        // between the two would have the engine holding a bit it never sees released.
        if down(VK_RCONTROL) {
            bits |= keys::mods::CTRL;
        }
        bits
    }
}

/// The three lock states as this process can best see them at start.
///
/// `GetKeyState` answers about the CALLING thread's copy of the keyboard state,
/// which Windows seeds from the global state when the thread's queue is created, so
/// it is right at start and stops being right the moment we swallow a lock key. From
/// then on the hook tracks the toggles itself, which is why this is a seed and not a
/// query.
fn initial_locks() -> u16 {
    // SAFETY: three reads of a virtual key's state.
    unsafe {
        let mut bits = 0u16;
        if GetKeyState(VK_CAPITAL as i32) & 1 != 0 {
            bits |= keys::mods::CAPS;
        }
        if GetKeyState(VK_NUMLOCK as i32) & 1 != 0 {
            bits |= keys::mods::NUM;
        }
        if GetKeyState(VK_SCROLL as i32) & 1 != 0 {
            bits |= keys::mods::SCROLL;
        }
        bits
    }
}

#[cfg(test)]
mod tests {
    //! Pure-helper tests: the key table, the absolute normalisation, the anchor.
    //! Nothing here creates a window, installs a hook or touches the desktop; that
    //! is the `#[ignore]`d live suite in `tests/windows.rs`. The module is
    //! `cfg(windows)`, so these run on the Windows target in CI.
    use super::*;

    /// The one table a person wrote by hand, checked for the properties a wrong
    /// entry would break: no usage twice, and the two deliberate scancode collisions
    /// and no others.
    #[test]
    fn the_usage_table_is_a_function_both_ways_except_where_it_says_so() {
        let mut usages: Vec<u32> = USAGE_SCANCODE.iter().map(|(u, _)| *u).collect();
        usages.sort_unstable();
        let before = usages.len();
        usages.dedup();
        assert_eq!(before, usages.len(), "a usage appears twice");

        let mut collisions: Vec<u16> = Vec::new();
        let mut seen: Vec<u16> = Vec::new();
        for (_, sc) in USAGE_SCANCODE {
            if seen.contains(sc) {
                collisions.push(*sc);
            }
            seen.push(*sc);
        }
        collisions.sort_unstable();
        assert_eq!(
            collisions,
            vec![0x2B, 0x45],
            "the only two keys that share a scancode are the backslash with the \
             non-US hash, and Pause with Num Lock"
        );
    }

    /// Every canonical name the dialect defines has a key on this platform, or the
    /// interface would say a target cannot type Enter.
    #[test]
    fn every_named_key_of_the_dialect_has_a_scancode_or_a_virtual_key() {
        for (name, usage) in keys::NAMED {
            let has = scancode_of(*usage).is_some() || USAGE_VK.iter().any(|(u, _)| u == usage);
            assert!(has, "{name} ({usage:#x}) has no way in on Windows");
        }
    }

    /// The modifier usages the engine asks for at every session start.
    #[test]
    fn every_modifier_the_engine_learns_has_a_scancode() {
        for bit in keys::mods::holdable_bits(keys::mods::HOLDABLE) {
            let usage = keys::mod_usage(bit).expect("a holdable bit has a key");
            assert!(
                scancode_of(usage).is_some(),
                "the key for {bit:#x} has no scancode"
            );
        }
    }

    /// A round trip through the scancode table, which is what a captured key and an
    /// injected one have to agree on.
    #[test]
    fn a_captured_scancode_names_the_usage_that_produced_it() {
        for (usage, scancode) in USAGE_SCANCODE {
            let wire = keys::usage(keys::PAGE_KEYBOARD, *usage);
            let back = usage_of_scancode(*scancode).expect("the table is complete both ways");
            if *scancode != 0x2B && *scancode != 0x45 {
                assert_eq!(back, wire, "{scancode:#x} did not round trip");
            }
        }
        assert_eq!(usage_of_scancode(0x9999), None);
    }

    /// The absolute normalisation, where an off by one puts the pointer one pixel
    /// out on every frame.
    #[test]
    fn an_absolute_position_normalises_to_the_ends_of_the_range() {
        assert_eq!(normalise(0, 0, 1920), 0);
        assert_eq!(normalise(1919, 0, 1920), 65535);
        assert_eq!(normalise(960, 0, 1921), 32768, "the middle is the middle");
        // A negative origin is the ordinary case of a second screen to the left.
        assert_eq!(normalise(-1920, -1920, 3840), 0);
        assert_eq!(normalise(1919, -1920, 3840), 65535);
        // Out of range is clamped rather than wrapped: the pointer lands on the edge
        // of the desktop and not on the other side of it.
        assert_eq!(normalise(-5000, 0, 1920), 0);
        assert_eq!(normalise(5000, 0, 1920), 65535);
        // A degenerate span must not divide by zero.
        assert_eq!(normalise(0, 0, 0), 0);
        assert_eq!(
            normalise(7, 0, 1),
            65535,
            "a one pixel span is treated as two, so anything past the origin is the far end"
        );
    }

    /// The anchor is the centre, because a pointer anchored at an edge reports a
    /// delta of zero for the direction it is already against.
    #[test]
    fn the_anchor_is_the_middle_of_the_rectangle_it_is_given() {
        assert_eq!(
            centre(&Rect {
                x: 0,
                y: 0,
                w: 1920,
                h: 1080
            }),
            Point { x: 960, y: 540 }
        );
        assert_eq!(
            centre(&Rect {
                x: -1920,
                y: 200,
                w: 1920,
                h: 1080
            }),
            Point { x: -960, y: 740 }
        );
        // Degenerate, and it must not panic or overflow.
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

    /// A refusal is only named when something proved it. With the desktop not ours
    /// the answer is the locked screen, and nothing else is consulted.
    #[test]
    fn a_desktop_that_is_not_ours_is_the_locked_screen() {
        assert_eq!(refusal_for(false), Some(Refusal::ScreenLocked));
    }

    /// The table checked against the OPERATING SYSTEM rather than against itself:
    /// every scancode is fed through `MapVirtualKeyExW` and the virtual key it names
    /// is compared with the one the usage means.
    ///
    /// This is the test that catches a transcription error, and a transcription error
    /// here is a remote keystroke that types the wrong key. It touches no desktop and
    /// no global state (a keyboard layout exists even on a session-0 runner with
    /// nothing to look at), which is why it runs in CI rather than living in the
    /// `#[ignore]`d live suite: the one table a person wrote by hand is worth having
    /// checked on every commit.
    ///
    /// Only the keys whose virtual key is the same on every layout are listed. A
    /// letter's position moves between QWERTY and AZERTY, so asserting one would be
    /// asserting whichever layout the machine running the test happens to have.
    #[test]
    fn the_table_names_the_keys_the_operating_system_names() {
        // SAFETY: the calling thread's layout, which exists in every session.
        let layout = unsafe { GetKeyboardLayout(0) };
        #[rustfmt::skip]
        let expected: &[(u32, u16)] = &[
            (0x28, 0x0D), // Enter, VK_RETURN
            (0x29, 0x1B), // Escape, VK_ESCAPE
            (0x2A, 0x08), // Backspace, VK_BACK
            (0x2B, 0x09), // Tab, VK_TAB
            (0x2C, 0x20), // Space, VK_SPACE
            (0x39, 0x14), // Caps Lock, VK_CAPITAL
            (0x3A, 0x70), // F1
            (0x43, 0x79), // F10
            (0x44, 0x7A), // F11
            (0x45, 0x7B), // F12
            (0x46, 0x2C), // Print Screen, VK_SNAPSHOT
            (0x47, 0x91), // Scroll Lock, VK_SCROLL
            (0x49, 0x2D), // Insert
            (0x4A, 0x24), // Home
            (0x4B, 0x21), // Page Up
            (0x4C, 0x2E), // Delete
            (0x4D, 0x23), // End
            (0x4E, 0x22), // Page Down
            (0x4F, 0x27), // Right
            (0x50, 0x25), // Left
            (0x51, 0x28), // Down
            (0x52, 0x26), // Up
            (0x53, 0x90), // Num Lock, VK_NUMLOCK
            (0x54, 0x6F), // keypad slash, VK_DIVIDE
            (0x55, 0x6A), // keypad asterisk, VK_MULTIPLY
            (0x56, 0x6D), // keypad minus, VK_SUBTRACT
            (0x57, 0x6B), // keypad plus, VK_ADD
            (0x58, 0x0D), // keypad Enter is VK_RETURN too, told apart by the prefix
            // The keypad's digits and its period name their NUM LOCK OFF virtual
            // keys, and that is the operating system's answer rather than a
            // surprise: `MAPVK_VSC_TO_VK_EX` is a layout level question and Num Lock
            // is a runtime state, so the scancode of the keypad 7 maps to VK_HOME.
            // It costs nothing here, because injection goes out BY SCANCODE and the
            // OS applies Num Lock itself (see truth 7 in the module header).
            (0x59, 0x23), // keypad 1, VK_END
            (0x5A, 0x28), // keypad 2, VK_DOWN
            (0x5B, 0x22), // keypad 3, VK_NEXT
            (0x5C, 0x25), // keypad 4, VK_LEFT
            (0x5D, 0x0C), // keypad 5, VK_CLEAR
            (0x5E, 0x27), // keypad 6, VK_RIGHT
            (0x5F, 0x24), // keypad 7, VK_HOME
            (0x60, 0x26), // keypad 8, VK_UP
            (0x61, 0x21), // keypad 9, VK_PRIOR
            (0x62, 0x2D), // keypad 0, VK_INSERT
            (0x63, 0x2E), // keypad period, VK_DELETE
            (0x65, 0x5D), // Menu, VK_APPS
            (0x68, 0x7C), // F13
            (0x73, 0x87), // F24
            (0xE0, 0xA2), // left Control, VK_LCONTROL
            (0xE1, 0xA0), // left Shift, VK_LSHIFT
            (0xE2, 0xA4), // left Alt, VK_LMENU
            (0xE3, 0x5B), // left Windows, VK_LWIN
            (0xE4, 0xA3), // right Control, VK_RCONTROL
            (0xE5, 0xA1), // right Shift, VK_RSHIFT
            (0xE6, 0xA5), // right Alt, VK_RMENU
            (0xE7, 0x5C), // right Windows, VK_RWIN
        ];
        for (id, want_vk) in expected {
            let usage = keys::usage(keys::PAGE_KEYBOARD, *id);
            let scancode =
                scancode_of(usage).unwrap_or_else(|| panic!("{id:#04x} is not in the table"));
            // SAFETY: a scancode and a layout handle.
            let vk = unsafe { MapVirtualKeyExW(u32::from(scancode), MAPVK_VSC_TO_VK_EX, layout) };
            assert_eq!(
                vk,
                u32::from(*want_vk),
                "usage {id:#04x} has scancode {scancode:#06x}, which this layout calls \
                 virtual key {vk:#04x} and the table says means {want_vk:#04x}"
            );
        }
    }

    /// The other direction, on the real OS: the virtual key a modifier resolves to
    /// gives back the scancode the table started from, so a press and its release
    /// cannot describe one key two ways.
    ///
    /// It is the property [`canonical_key`] exists for, and the failure it prevents is
    /// a held Control nothing will ever release.
    #[test]
    fn a_modifier_resolves_to_one_key_whichever_way_it_is_asked() {
        for bit in keys::mods::holdable_bits(keys::mods::HOLDABLE) {
            let usage = keys::mod_usage(bit).expect("a holdable bit has a key");
            let layout = unsafe { GetKeyboardLayout(0) };
            let from_usage = platform_key(usage, layout).expect("a modifier has a key");
            let from_vk = canonical_key(from_usage.code as u16, layout);
            assert_eq!(
                from_usage.detail & 0xFF,
                from_vk.detail & 0xFF,
                "the two routes to {bit:#x} disagree about its scancode: \
                 {from_usage:?} against {from_vk:?}"
            );
            assert_eq!(from_usage.code, from_vk.code);
        }
    }
}
