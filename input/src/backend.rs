// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The seam between the OS-agnostic engine and a platform input backend
//! (doc/input-sharing.md, section 10). A backend reports what the real keyboard
//! and mouse did through [`BackendEvent`] (upcalls, pushed on an
//! `mpsc::Sender<BackendEvent>` it is handed at construction) and is driven by
//! the engine through [`InputBackend`] (downcalls). Real backends bind the OS
//! event loop to the main thread (a message pump on Windows, a run loop on
//! macOS, a non-`Send` X connection on Linux) and expose a cheap, `Clone` handle
//! here (channel senders to that thread); the test double ([`crate::fake`])
//! implements it directly.
//!
//! # Why the split is where it is
//!
//! Injection is a FIRE AND FORGET downcall and refusals come back as coalesced
//! upcalls. A result per event would cost a round trip to the OS thread on a
//! path where the OS call itself is about 100 microseconds (#123 measured
//! `SendInput` at 91 microseconds bare, 113 with our own hooks installed), and a
//! refusal reported per keystroke would be a flood; one code with a count per
//! second is what an interface can actually say.
//!
//! [`InputBackend::resolve`] is the one downcall that answers, and it is the
//! reason the seam exists at all. "What key and what modifiers produce this
//! symbol on this machine's active layout right now" is platform knowledge
//! (`VkKeyScanEx` on Windows, an Xkb keymap search on X11, a reverse
//! `UCKeyTranslate` on macOS), while sequencing the answer, restoring what was
//! held and owning modifier hygiene is OS-independent logic that belongs in the
//! engine and must be testable without a desk. So the backend answers the
//! question and the engine builds the sequence, caching the answers per active
//! layout and dropping the cache on [`BackendEvent::LayoutChanged`].
//!
//! # Three platform truths this seam is shaped by
//!
//! Written here so the platform ticket does not have to rediscover them:
//!
//! 1. **Under confinement, [`Motion::dx`] and [`Motion::dy`] must come from the
//!    OS's own relative source** (raw input on Windows, XI2 raw events on X11,
//!    the `CGEvent` delta fields on macOS), NEVER from differencing successive
//!    absolute positions. While driving, the engine warps the pointer back on
//!    every event to keep it pinned, so the difference between two absolute
//!    positions is zero exactly when the hand is moving fastest.
//! 2. **A relative mouse move of (0, 0) is discarded by Windows** and reaches no
//!    hook at all (#123). A backend must not rely on ever seeing one; the engine
//!    never emits one either.
//! 3. **The marker in `dwExtraInfo` does not survive a relative mouse move** (it
//!    does survive a key event), so a backend cannot recognise its own injected
//!    motion that way. In v1 it does not have to: a machine being driven is not
//!    capturing (doc/input-sharing.md, section 4, rule 3, and D15). Anything
//!    that lifts that rule owes echo suppression a different mechanism, and this
//!    is the note that says so.
//!
//! # Three more, verified against the three real platforms during review
//!
//! The first three were what the design was shaped by. These three are what an
//! implementation of it walks into, and they are written here for the same reason:
//! a truth rediscovered at a desk costs a week and a truth written down costs a
//! paragraph.
//!
//! 4. **On X11 the relative source is not the same one in both modes, and the
//!    platform decides which.** `XI_RawMotion`'s valuators are the raw device
//!    deltas with no acceleration profile applied, and they are what an X11 backend
//!    reads while it only watches. Under a GRAB it gets nothing from them: a
//!    grabbed device's events go to the grab, and a raw event has no core form to
//!    convert into, so the grabbing client is the one client that stops receiving
//!    them (measured in #125 as zero upcalls for eighteen faked events, and the
//!    same measurement is the reason the grabs there carry an event mask). So while
//!    swallowing, the delta is the difference of the positions the grab reports:
//!    the ACCELERATED movement, which is what Windows gets for free from its hook
//!    (truth 7), bounded by the pin's anchor being at the centre of the screen so
//!    one event carries up to half a screen. Applying the device's own acceleration
//!    profile to the raw valuators instead is a third option and a ticket of its
//!    own. Nothing on this seam changes either way: the engine consumes
//!    [`Motion::dx`] and [`Motion::dy`] and asks no questions.
//! 5. **`CGWarpMouseCursorPosition` suppresses local mouse events for about
//!    250 ms** unless it is followed by
//!    `CGAssociateMouseAndMouseCursorPosition(true)`, or unless
//!    `CGSetLocalEventsSuppressionInterval` is zeroed first. The engine warps on
//!    the return path and on every teardown, so a macOS backend that omits that
//!    call gives its own user a quarter of a second of dead mouse at the exact
//!    moment they asked for their machine back. A macOS backend must therefore do
//!    ONE of the two, and it cannot always do the re-association: while a session
//!    holds the pointer, the decoupling IS the confinement, so re-associating there
//!    would undo the pin. The backend written against this seam zeroes the interval
//!    on its own event source and re-associates only after the warps that are not
//!    part of a pin. (This truth is numbered 5 here and 4 in `input/src/macos.rs`,
//!    which numbers its own list in the order that file needs.)
//! 6. **`Want::Symbol` on Windows cannot express a dead key through the easy
//!    API.** `VkKeyScanEx` returns -1 for a character that needs one, and finding
//!    the pair means walking the virtual keys by modifier state through
//!    `ToUnicodeEx` and driving its own dead-key state machine. A Windows backend
//!    may legitimately answer `None` for a dead-key symbol, and that is covered
//!    rather than lossy: Windows always has [`Capabilities::unicode`], so the
//!    engine's Unicode fallback types the character (doc/input-sharing.md,
//!    section 8's typing table, level 2).

use std::future::Future;

/// One monitor, as the platform reports it and as the layout document carries
/// it (doc/input-sharing.md, section 6).
///
/// All lengths are LOGICAL pixels and `scale` is in permille (1500 is 150%):
/// integers everywhere, because this travels inside a signed document and a
/// float on a signed wire is a round-tripping trap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Monitor {
    /// Stable identity across unplugging: the output name plus a hash of the
    /// EDID where one is readable (`CGDisplayCreateUUIDFromDisplayID` on macOS,
    /// RandR output plus EDID on X11, the display device path on Windows). A
    /// backend that cannot make it stable says so through
    /// [`Capabilities::monitors_stable`] rather than inventing one that moves:
    /// a monitor whose identity changes silently swaps places in the plane, and
    /// the pointer then crosses to the wrong screen with nobody able to explain
    /// why.
    pub id: String,
    /// What a person calls it ("Dell U2720Q", "Built-in Retina Display").
    pub name: String,
    /// Logical size.
    pub w: i32,
    pub h: i32,
    /// Position in THIS machine's own desktop, logical, top-left origin. This
    /// is what lets a machine's own arrangement be imported as a movable block
    /// rather than re-invented, and it is also the space absolute pointer
    /// frames are expressed in when this machine is the target.
    pub x: i32,
    pub y: i32,
    /// Scale factor in permille: 1000 is 100%, 1500 is 150%, 2000 is 200%.
    pub scale: i32,
    pub primary: bool,
}

/// A point in a machine's own logical desktop coordinates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

/// A rectangle in a machine's own logical desktop coordinates, half open
/// (`x .. x + w`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// What a platform backend can actually do, and what it cannot.
///
/// Every `false` is a sentence an interface can say BEFORE anyone tries, which
/// is the whole differentiator this feature claims: best effort is acceptable,
/// best effort that lies is not. A target whose backend cannot inject the
/// pointer is a keyboard-only target and the source can say so, instead of
/// discovering it one refusal at a time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capabilities {
    /// Can observe the real keyboard and mouse (this machine can be a source).
    pub capture: bool,
    /// Can CONSUME what it observes, so the source's own keystrokes do not also
    /// act locally. On X11 raw XI2 events can be observed but not consumed:
    /// swallowing needs a grab.
    pub swallow: bool,
    /// Can pin the pointer to a rectangle, so it does not walk off this
    /// machine's desktop while a session is live, AND can report OS-native
    /// relative deltas while it is pinned.
    ///
    /// The second half of that sentence is deliberately folded in rather than
    /// given a bit of its own, and the reason is worth recording because the
    /// alternative was considered. [`Capabilities::can_drive`] is
    /// `capture && swallow && confine`, so a backend that could pin the pointer
    /// but not report relative motion would pass and then produce a FROZEN
    /// pointer: pinned, and with every delta zero (see the module header,
    /// truth 1). A separate `relative` bit would say so, and would also change the
    /// wire's `caps` object, section 10's frozen `Capabilities` list, the
    /// `input.status` snapshot and every interface reading it, for a distinction
    /// no real platform can satisfy one half of: on all three, the confining
    /// mechanism and the raw relative source are the same subsystem (a grab plus
    /// XI2 raw events, `ClipCursor` plus raw input, association-off plus the
    /// `CGEvent` delta fields). So it is one bit with a two-part contract, and a
    /// backend that can only do the pinning half must report `confine: false`.
    ///
    /// **On macOS this is not a clip.** There is no `ClipCursor` equivalent: the
    /// implementation is `CGAssociateMouseAndMouseCursorPosition(false)` plus a
    /// warp per event, which DECOUPLES the cursor from the mouse rather than
    /// pinning it inside a rectangle, so the cursor does not move at all and the
    /// rect handed to [`InputBackend::confine`] is advisory there. `confine: true`
    /// on macOS therefore means "can decouple", which serves the same purpose (the
    /// pointer does not walk off this desktop while a session is live) by a
    /// different mechanism. X11 needs an `InputOnly` window grab or XFixes pointer
    /// barriers; Windows is `ClipCursor` verbatim.
    pub confine: bool,
    /// Can put the pointer somewhere (the return, and leaving an edge).
    pub warp: bool,
    /// Can type (this machine can be a target).
    pub inject_keys: bool,
    /// Can move the pointer and press buttons.
    pub inject_pointer: bool,
    /// Can inject an arbitrary Unicode string, which is the last resort of key
    /// resolution (`KEYEVENTF_UNICODE`, `CGEventKeyboardSetUnicodeString`, an
    /// X11 spare keycode remapped for the stroke).
    pub unicode: bool,
    /// Monitor identities survive an unplug. When false the interface says the
    /// screens on this machine may swap places, rather than the plane quietly
    /// rearranging itself.
    pub monitors_stable: bool,
    /// The honest hook for something missing that a human can fix, the
    /// `session.status.problem` pattern. `None` when all is well.
    pub problem: Option<Problem>,
}

impl Capabilities {
    /// A backend that can do nothing: the shape a platform without an
    /// implementation reports, and the baseline a real backend narrows.
    pub fn none(problem: Option<Problem>) -> Capabilities {
        Capabilities {
            capture: false,
            swallow: false,
            confine: false,
            warp: false,
            inject_keys: false,
            inject_pointer: false,
            unicode: false,
            monitors_stable: false,
            problem,
        }
    }

    /// Can this machine be driven at all? Keyboard only counts: a keyboard-only
    /// session is a real session and the epic asks for it by name.
    pub fn can_be_driven(&self) -> bool {
        self.inject_keys
    }

    /// Can this machine drive another? Observing is not enough: without
    /// swallowing, every keystroke would act locally AND remotely, and without
    /// confinement the pointer would walk off this desktop while it drives.
    ///
    /// [`Capabilities::confine`] carries a two-part contract (pin the pointer AND
    /// report OS-native relative deltas while pinned) precisely so that this
    /// three-way `and` is enough. A backend that satisfies only the pinning half
    /// and reports `true` here produces a pointer that is pinned and never moves.
    pub fn can_drive(&self) -> bool {
        self.capture && self.swallow && self.confine
    }

    /// The `caps` object of the `hi` frame (doc/input-sharing.md, section 3).
    ///
    /// Here rather than hand-rolled at the call site, and paired with
    /// [`Capabilities::from_value`], because `caps` is the one field of the dialect
    /// whose shape is open: a hand-written encoder on one side and no decoder at
    /// all on the other is how "a target whose backend cannot inject the pointer is
    /// a keyboard-only target and the source can say so" ends up being served by
    /// reading raw peer JSON at the point of use.
    ///
    /// `problem` travels as its code, so an interface can say WHY a peer cannot be
    /// driven ("1Device needs Accessibility permission on that computer") rather
    /// than only that it cannot. It is absent when there is nothing wrong.
    pub fn to_value(&self) -> serde_json::Value {
        let mut v = serde_json::json!({
            "capture": self.capture,
            "swallow": self.swallow,
            "confine": self.confine,
            "warp": self.warp,
            "inject_keys": self.inject_keys,
            "inject_pointer": self.inject_pointer,
            "unicode": self.unicode,
            "monitors_stable": self.monitors_stable,
        });
        if let Some(problem) = self.problem {
            v["problem"] = serde_json::json!(problem.code());
        }
        v
    }

    /// Reads a peer's `caps`, FAILING CLOSED on every field.
    ///
    /// Absent, of the wrong type, not an object at all: each reads as `false`. A
    /// peer whose `caps` we cannot read is a peer we assume can do nothing, and
    /// that is the safe direction by construction, because every capability here
    /// gates something the engine would otherwise TRY: a `false` costs a session
    /// that is refused honestly up front, where a wrongly optimistic `true` costs a
    /// session that starts and then swallows somebody's keystrokes into a machine
    /// that cannot type them.
    ///
    /// `problem` is read only when it is one of the codes this build knows, so an
    /// unknown one becomes `None` rather than a peer-chosen string reaching the
    /// interface. That loses a sentence and keeps the honest half: the booleans
    /// already say what does not work.
    pub fn from_value(v: &serde_json::Value) -> Capabilities {
        let flag = |name: &str| v.get(name).and_then(serde_json::Value::as_bool) == Some(true);
        Capabilities {
            capture: flag("capture"),
            swallow: flag("swallow"),
            confine: flag("confine"),
            warp: flag("warp"),
            inject_keys: flag("inject_keys"),
            inject_pointer: flag("inject_pointer"),
            unicode: flag("unicode"),
            monitors_stable: flag("monitors_stable"),
            problem: v
                .get("problem")
                .and_then(serde_json::Value::as_str)
                .and_then(Problem::parse),
        }
    }
}

/// Something a human can fix, or at least be told about. Codes, not sentences:
/// the interface owns the wording (doc/input-sharing.md, section 13).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Problem {
    /// No backend for this platform, or the session cannot provide one.
    NoBackend,
    /// The OS grant is missing or refused (the macOS Accessibility grant, which
    /// also needs re-approval after some updates). Asked for with a reason, and
    /// while it is refused the interface says what does not work rather than
    /// pretending.
    NoPermission,
    /// Monitor identities are not stable here, so the plane may swap screens.
    MonitorsUnstable,
    /// A Wayland session, where the portals are the way in and this build has
    /// no Wayland backend yet. Said out loud rather than staying silent, which
    /// is the standing decision for Linux.
    Wayland,
}

impl Problem {
    /// The wire and snapshot spelling (`input.status`'s `problem`).
    pub fn code(self) -> &'static str {
        match self {
            Problem::NoBackend => "no_backend",
            Problem::NoPermission => "no_permission",
            Problem::MonitorsUnstable => "monitors_unstable",
            Problem::Wayland => "wayland",
        }
    }

    /// The problem a code names, for reading a peer's `caps`. `None` for a code
    /// this build does not know, which is the fail-closed answer: a problem we
    /// cannot name is one we do not repeat to the interface as peer-chosen prose.
    pub fn parse(code: &str) -> Option<Problem> {
        match code {
            "no_backend" => Some(Problem::NoBackend),
            "no_permission" => Some(Problem::NoPermission),
            "monitors_unstable" => Some(Problem::MonitorsUnstable),
            "wayland" => Some(Problem::Wayland),
            _ => None,
        }
    }
}

/// What the engine asks a backend to do about the real input devices.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureMode {
    /// Report nothing, hold nothing. The resting state, and the state every
    /// teardown path must reach: a swallow that outlives its session leaves a
    /// machine with a dead keyboard.
    Off,
    /// Report events, let them act locally too. What the engine needs while it
    /// watches an edge for a crossing: the pointer is still the user's.
    Watch,
    /// Report events AND consume them. What driving needs.
    Swallow,
}

/// A refusal the OS handed back, detected rather than guessed
/// (doc/input-sharing.md, section 13).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// `SendInput` returned 0 and the foreground window's integrity level says
    /// why: UIPI blocks a higher integrity window.
    ElevatedWindow,
    /// `IsSecureEventInputEnabled`: macOS has a password field focused.
    SecureInput,
    /// The machine is locked (or on Windows, the secure desktop is up, which is
    /// a different desktop and out of reach by construction).
    ScreenLocked,
    /// The OS grant is missing: nothing can be typed until a human gives it.
    NoPermission,
}

impl Refusal {
    /// The wire spelling, carried by the dialect's `oops` frame.
    ///
    /// The constants come from [`crate::wire::oops`] rather than being written
    /// again here, because the `oops` codes are part of the frozen dialect and a
    /// dialect with two spellings of a word has one bug waiting: this enum covers
    /// four of the five codes and the fifth (`UNRESOLVED`) is generated by the
    /// engine, so before the module existed the set was split between a `match` arm
    /// and a string literal at the point of use.
    pub fn code(self) -> &'static str {
        match self {
            Refusal::ElevatedWindow => crate::wire::oops::ELEVATED_WINDOW,
            Refusal::SecureInput => crate::wire::oops::SECURE_INPUT,
            Refusal::ScreenLocked => crate::wire::oops::SCREEN_LOCKED,
            Refusal::NoPermission => crate::wire::oops::NO_PERMISSION,
        }
    }
}

/// Why capture stopped without the engine asking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureLoss {
    /// The grab was broken by something else on the machine (another client
    /// took an X grab, a hook was unhooked, the tap was disabled by a timeout).
    Broken,
    /// The OS grant went away under us.
    Permission,
    /// The session itself is going (the display server is stopping, the user is
    /// logging out).
    SessionEnded,
}

/// One raw pointer motion as the OS reported it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Motion {
    /// Absolute position in this machine's own logical desktop coordinates.
    /// Meaningful while capture is [`CaptureMode::Watch`]; while the pointer is
    /// confined it is whatever the pin left it at, and the engine ignores it.
    pub at: Point,
    /// Relative movement in logical pixels since the previous event. **Under
    /// confinement this MUST come from the OS's own relative source**, never
    /// from differencing `at` (see the module header, truth 1).
    pub dx: i32,
    pub dy: i32,
}

/// A key as the OS reported it, carrying every level the wire carries because
/// reading them is part of capture and not a separate lookup
/// (doc/input-sharing.md, section 3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyEvent {
    /// HID usage, `(page << 16) | id`, 0 when the backend cannot tell. The
    /// keyboard page (0x07) covers Enter, Tab, the function keys, the arrows,
    /// Home and End; the media keys are on the consumer page (0x0C), which is
    /// why the page travels with the id.
    pub usage: u32,
    /// Canonical name of a key that produces no character (`Enter`, `F5`,
    /// `Up`), from the frozen table in [`crate::keys`]. `None` for a key that
    /// produces one.
    pub key: Option<String>,
    /// The text this machine's own layout produced for the stroke. `None` for a
    /// key that produces none. Bounded by the engine before it goes on the
    /// wire.
    pub sym: Option<String>,
    /// Canonical modifier bits held at the moment of the stroke
    /// ([`crate::keys::mods`]).
    pub mods: u16,
    /// Pressed, or released.
    pub down: bool,
    /// A half-duplex lock (Caps Lock, Num Lock, Scroll Lock on some keyboards)
    /// that reports only a press. The target then injects only a press: a lock
    /// is never held, and releasing one later would toggle it.
    pub lock: bool,
}

/// What a platform backend reports to the engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackendEvent {
    Motion(Motion),
    /// A mouse button: 1 left, 2 middle, 3 right, 4 back, 5 forward.
    Button {
        button: u8,
        down: bool,
    },
    /// A wheel notch or a pixel-precise scroll. Positive is right and up.
    Wheel {
        dx: i32,
        dy: i32,
        pixels: bool,
    },
    Key(KeyEvent),
    /// The set of monitors changed (plugged, unplugged, rearranged, rescaled).
    /// The engine re-reads [`InputBackend::monitors`] and publishes its own
    /// word about its own screens.
    MonitorsChanged,
    /// The active keyboard layout changed, so every cached resolution is stale.
    /// `layout` is this machine's identity for it, bounded by the engine before
    /// it goes on the wire.
    ///
    /// `group` is the keyboard layout GROUP the machine is in now, and it is not
    /// decoration: it is the only way the engine can ever learn it.
    /// [`Resolved::group`] lets a backend answer "that symbol lives in group 2",
    /// and the engine then switches to it and switches BACK, because a session that
    /// silently changes the machine's keyboard layout breaks the next thing its
    /// owner types. Switching back means knowing what to switch back TO, and
    /// without this field the engine's answer was 0 for ever: a user working in
    /// group 1 whose symbol resolved in group 2 got `Group(2) ... Group(0)` and was
    /// left in group 0, which is the exact failure the switch-back exists to
    /// prevent. It is free to report on X11 (the same `XkbStateNotify` a backend
    /// reads to notice the change carries the group) and 0 on Windows and macOS,
    /// where groups do not exist in this sense: Windows switches whole layouts
    /// (each is its own identity, so `layout` already says it) and macOS switches
    /// input sources with `TISSelectInputSource`, which is user-visible and slow
    /// and which a `resolve` must therefore never ask for.
    ///
    /// **The caller owes this event two things**, both documented where they are
    /// implemented and both worth naming together here, because they are what makes
    /// a layout change safe rather than merely noticed:
    ///
    /// 1. Seed the group it tracks from `group` (it is what
    ///    [`crate::keys::Stroke::group`] must carry), re-learn the modifier keys
    ///    ([`crate::keys::ModKeys::learn`]) and drop the resolution cache
    ///    ([`crate::keys::Resolver::layout_changed`]).
    /// 2. RELEASE EVERYTHING IT HOLDS, through
    ///    [`crate::keys::Held::release_plan`] and [`InputBackend::release_all`],
    ///    then clear the held set. Every [`PlatformKey`] in that set was named by a
    ///    resolution against the keymap that has just been replaced, so the release
    ///    the engine would compute AFTER the change may not equal the press it made
    ///    BEFORE it, and a key whose release does not match stays physically down
    ///    until the session tears down. A held Control in that state is a machine
    ///    with a dead keyboard for the rest of the session. Releasing on the change
    ///    costs a game one re-press of the key it is holding; not releasing costs
    ///    the keyboard.
    LayoutChanged {
        layout: String,
        group: u32,
    },
    /// What this backend can do has changed, and nothing else has: the OS grant
    /// arrived, or was taken away, without a monitor moving and without capture
    /// being lost. The engine re-reads [`InputBackend::capabilities`], asks what
    /// this machine can produce all over again, applies the capture mode the new
    /// answer allows and republishes.
    ///
    /// # Why the seam needed one more upcall than the design listed
    ///
    /// Added by the platform ticket, because without it a macOS machine whose
    /// Accessibility grant is given AFTER the component started keeps saying it
    /// cannot type until something unrelated happens. The engine re-reads the
    /// capabilities on exactly three occasions: at start, on
    /// [`BackendEvent::MonitorsChanged`], and on [`BackendEvent::CaptureLost`].
    /// A grant is none of those. Reusing `MonitorsChanged` would work by accident
    /// (it does re-read the capabilities) and would lie in the log about what
    /// happened, and `CaptureLost` would end a live session for a permission that
    /// had just been GIVEN.
    ///
    /// It also closes the trap the seam's `resolve` documents: the resolution cache
    /// keeps negative answers on purpose, so a backend asked what it can produce
    /// before its grant exists is remembered as able to produce nothing. A session
    /// start already re-learns; this makes the moment the grant lands re-learn too,
    /// so an interface that says "1Device can type on this computer now" is telling
    /// the truth rather than predicting it.
    CapabilitiesChanged,
    /// An injection was refused. Coalesced by the engine into one `oops` frame
    /// per code per second, with a count.
    Refused(Refusal),
    /// Capture stopped without being asked to. Every session using it ends, and
    /// the engine says why.
    CaptureLost(CaptureLoss),
}

/// One thing to do to the OS, in order. A batch is applied as given: the engine
/// has already resolved the keys and computed the sequence, so a backend never
/// interprets, never reorders and never coalesces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Move the pointer to an absolute position in this machine's own logical
    /// desktop coordinates.
    MoveTo(Point),
    /// Move the pointer by a relative amount, in logical pixels. Never (0, 0):
    /// Windows discards that and it reaches no hook at all.
    MoveBy {
        dx: i32,
        dy: i32,
    },
    /// A mouse button: 1 left, 2 middle, 3 right, 4 back, 5 forward, and never
    /// anything else. The engine checks the number against that closed set as it
    /// leaves the wire ([`crate::wire::BUTTON_MAX`]), so a backend can hand it
    /// straight to the platform.
    Button {
        button: u8,
        down: bool,
    },
    Wheel {
        dx: i32,
        dy: i32,
        pixels: bool,
    },
    /// Press or release a key the backend itself named, in the platform's own
    /// terms ([`Resolved::code`]). The engine never invents one: it only ever
    /// replays what [`InputBackend::resolve`] handed back, which is what keeps
    /// the engine free of platform key tables.
    Key {
        code: PlatformKey,
        down: bool,
    },
    /// Type a string through the platform's Unicode path, the last resort of
    /// key resolution. Only ever emitted when [`Capabilities::unicode`].
    Text(String),
    /// Switch to a keyboard layout group and, later in the same batch, switch
    /// back: a session that silently changes the machine's layout breaks the
    /// next thing its owner types.
    ///
    /// **No macOS meaning, and it is harmless there rather than needing a
    /// capability.** Input sources are switched with `TISSelectInputSource`, which
    /// is user-visible and slow, so a macOS backend must never offer one: it simply
    /// leaves [`Resolved::group`] `None`. The engine only ever emits a `Group` whose
    /// number a `resolve` on THIS machine handed back, so a backend that never sets
    /// one never receives one, and a macOS implementation may leave this arm
    /// unimplemented as long as it is a no-op and not a panic. On X11 it is the
    /// group of `XkbLockGroup`; Windows switches whole layouts rather than groups
    /// and is in the same position as macOS.
    Group(u32),
}

/// A key in the platform's own terms, opaque to the engine.
///
/// A newtype over the widest thing the three platforms need (a Windows virtual
/// key plus its scancode flags, an X11 keycode plus a group and a level, a macOS
/// virtual keycode) so the engine can hold one in its held set, compare two, and
/// hand one back for a release, while never having to know what it means. The
/// engine's own tables are keyed by HID usage and canonical name; this type is
/// only ever produced by a backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PlatformKey {
    /// The platform's own code for the key (virtual key, keycode, or virtual
    /// keycode).
    pub code: u32,
    /// Whatever else the platform needs to press exactly this key: a Windows
    /// extended-key flag, a scancode. Opaque here.
    ///
    /// **It MUST NOT carry anything a layout change can move.** The whole pair is
    /// compared for equality, and that comparison is what decides whether an
    /// incoming release is a key the engine is holding: a release that resolves to
    /// the same physical key with a different `detail` leaves that key physically
    /// DOWN, because "never release what we did not press" then reads as "we did
    /// not press this". A game holding W across a layout change, or worse a held
    /// Control, is the failure. The group in particular has its own field for
    /// exactly this reason ([`Resolved::group`]), so a backend must never fold it
    /// in here. The engine also releases everything on a
    /// [`BackendEvent::LayoutChanged`], which closes the same hole from the other
    /// side; both, because one of them is a rule a backend author has to remember
    /// and the other is not.
    pub detail: u32,
}

/// What the engine wants to produce on the target, handed to
/// [`InputBackend::resolve`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Want {
    /// This exact physical position: a HID usage, `(page << 16) | id`.
    Usage(u32),
    /// This named key with no symbol (`Enter`, `F5`), from the frozen table.
    Named(String),
    /// This text, however this machine's active layout can produce it. The
    /// answer may need modifiers the source never held (AltGr plus 0 for `@` on
    /// AZERTY), and may need a dead key first, which is what
    /// [`Resolved::prefix`] carries.
    Symbol(String),
}

/// How this machine can produce what was wanted, in its own terms.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    /// The key to press.
    pub code: PlatformKey,
    /// The canonical modifier bits that must be held for this key to produce
    /// what was wanted, and ONLY those: the engine releases what is held and
    /// not wanted, presses what is wanted and not held, and restores afterwards
    /// (doc/input-sharing.md, section 8).
    pub mods: u16,
    /// A dead key (or any other stroke) that must be pressed and released
    /// before `code`, with its own modifiers. `None` for the ordinary case.
    pub prefix: Option<Box<Resolved>>,
    /// The keyboard layout group `code` belongs to, when the active one does not
    /// contain what was wanted. The engine switches to it and switches back.
    pub group: Option<u32>,
}

/// The engine's downcalls into a platform backend. A cheap-to-`Clone` handle
/// (the engine clones it into per-session work); real backends make the methods
/// forward to the main-thread OS loop.
pub trait InputBackend: Clone + Send + Sync + 'static {
    /// What this backend can do. Cheap and callable at any time: the engine
    /// re-reads it when a permission may have changed (a `CaptureLost` upcall,
    /// a `MonitorsChanged`), and it is what the interface renders.
    fn capabilities(&self) -> Capabilities;

    /// This machine's monitors. Answers, because it round trips to the OS
    /// thread; called at start and on [`BackendEvent::MonitorsChanged`], never
    /// on a hot path.
    fn monitors(&self) -> impl Future<Output = Vec<Monitor>> + Send;

    /// Where the pointer is now, in this machine's own logical desktop
    /// coordinates. Called when capture starts and after a warp, never per
    /// event: motion upcalls carry the position.
    fn pointer(&self) -> impl Future<Output = Option<Point>> + Send;

    /// How this machine can produce `want` on its ACTIVE layout right now, or
    /// `None` if it cannot. Answers, and the engine caches the answers per
    /// layout identity and drops the cache on
    /// [`BackendEvent::LayoutChanged`], so a character costs one round trip
    /// once and nothing afterwards.
    ///
    /// `None` is a legitimate answer and not a failure: the engine falls through to
    /// the next level of section 8's table and, failing everything, to the Unicode
    /// injection. A Windows backend in particular may answer `None` for a
    /// [`Want::Symbol`] that needs a DEAD KEY, because `VkKeyScanEx` returns -1 for
    /// one and the alternative is walking the virtual keys by modifier state through
    /// `ToUnicodeEx` and driving its dead-key state machine; the character still
    /// arrives, through the Unicode path, which Windows always has (see the module
    /// header, truth 6).
    fn resolve(&self, want: Want) -> impl Future<Output = Option<Resolved>> + Send;

    /// Start or stop observing (and consuming) the real devices. Fire and
    /// forget: the effect shows up as upcalls, and a failure shows up as
    /// [`BackendEvent::CaptureLost`] or as [`Capabilities`] narrowing.
    fn capture(&self, mode: CaptureMode);

    /// Pin the pointer inside a rectangle, or release it with `None`. Releasing
    /// is on every teardown path: a confinement that outlives its session is a
    /// machine whose mouse is stuck in a corner.
    ///
    /// **`rect` is a hint on macOS**, where there is no `ClipCursor` equivalent and
    /// the mechanism decouples the cursor from the mouse instead of pinning it (see
    /// [`Capabilities::confine`], which carries the whole contract including the
    /// relative-source half of it). Windows is `ClipCursor` verbatim; X11 needs an
    /// `InputOnly` window grab or XFixes barriers.
    fn confine(&self, rect: Option<Rect>);

    /// Put the pointer at `to`, in this machine's own logical desktop
    /// coordinates.
    ///
    /// **On macOS this is two calls, not one**: `CGWarpMouseCursorPosition`
    /// followed by `CGAssociateMouseAndMouseCursorPosition(true)`, or with
    /// `CGSetLocalEventsSuppressionInterval` zeroed beforehand. Without that,
    /// macOS suppresses local mouse events for about 250 ms after the warp, and the
    /// engine warps on the return path and on every teardown: a quarter of a second
    /// of dead mouse at the exact moment its owner asked for their machine back
    /// (see the module header, truth 5).
    fn warp(&self, to: Point);

    /// Do these things to the OS, in order. Fire and forget by design (see the
    /// module header): refusals come back as [`BackendEvent::Refused`].
    fn inject(&self, actions: Vec<Action>);

    /// Release exactly these keys, in the order given. The hygiene path, and it
    /// must work while anything else is in flight: it is called from every
    /// teardown, including the ones that happen because the backend is already
    /// unhappy.
    fn release_all(&self, keys: Vec<PlatformKey>);

    /// Ask the main-thread OS loop to end, so the process can exit with `code`.
    /// The clipboard component's shape.
    ///
    /// **A real backend must ROUTE this to its own loop and must not exit from
    /// here**, however tempting a one-line `std::process::exit` looks: this is
    /// called from the engine's thread, and exiting there skips every teardown path
    /// on a machine that may be holding somebody's modifiers down.
    /// [`crate::os::Absent`] does exit, because it holds nothing and has no loop to
    /// ask, and that is the one case where it is honest.
    fn request_exit(&self, code: i32);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `caps` object round trips, and a peer's `caps` we cannot read means
    /// "this peer can do nothing" rather than "this peer can do anything".
    ///
    /// This is the one field of the dialect whose shape is open, and it decides
    /// whether the interface can say what a session cannot do BEFORE anyone tries.
    /// Reading it optimistically would mean a session that starts, swallows
    /// somebody's keystrokes and types them nowhere.
    #[test]
    fn the_capabilities_round_trip_and_an_unreadable_peer_can_do_nothing() {
        let full = Capabilities {
            capture: true,
            swallow: true,
            confine: true,
            warp: true,
            inject_keys: true,
            inject_pointer: true,
            unicode: true,
            monitors_stable: true,
            problem: None,
        };
        assert_eq!(
            Capabilities::from_value(&full.to_value()),
            full,
            "every bit survives its own codec"
        );

        // The keyboard-only target: the shape the epic asks for by name.
        let keyboard_only = Capabilities {
            inject_pointer: false,
            confine: false,
            ..full.clone()
        };
        let read = Capabilities::from_value(&keyboard_only.to_value());
        assert_eq!(read, keyboard_only);
        assert!(
            read.can_be_driven() && !read.can_drive(),
            "a keyboard-only target is a real target"
        );

        // The problem travels as its code, so an interface can say WHY rather than
        // only that something is wrong.
        let refused = Capabilities::none(Some(Problem::NoPermission));
        let read = Capabilities::from_value(&refused.to_value());
        assert_eq!(read, refused);
        assert_eq!(read.problem, Some(Problem::NoPermission));

        // Fail closed, field by field: absent, wrong type, not an object at all,
        // and a `problem` this build does not know.
        for hostile in [
            serde_json::json!({}),
            serde_json::json!(null),
            serde_json::json!("everything"),
            serde_json::json!([true, true, true]),
            serde_json::json!({ "capture": "yes", "inject_keys": 1, "unicode": [] }),
            serde_json::json!({ "problem": "gremlins" }),
        ] {
            let read = Capabilities::from_value(&hostile);
            assert_eq!(
                read,
                Capabilities::none(None),
                "{hostile} must read as a peer that can do nothing"
            );
        }
        assert!(
            Capabilities::from_value(&serde_json::json!({ "inject_keys": true })).inject_keys,
            "and a field we CAN read is read"
        );
    }

    /// Every problem code round trips, and an unknown one is `None` rather than a
    /// peer-chosen string reaching the interface.
    #[test]
    fn every_problem_code_round_trips_and_an_unknown_one_is_dropped() {
        for problem in [
            Problem::NoBackend,
            Problem::NoPermission,
            Problem::MonitorsUnstable,
            Problem::Wayland,
        ] {
            assert_eq!(Problem::parse(problem.code()), Some(problem));
        }
        assert_eq!(Problem::parse("no_backend_at_all"), None);
        assert_eq!(Problem::parse(""), None);
    }

    /// The `oops` codes have ONE spelling, the dialect's. Four of the five are
    /// refusals a backend detects; the fifth is the engine's own, which is why the
    /// set lives in `wire` and this enum borrows it.
    #[test]
    fn a_refusal_is_spelled_the_way_the_dialect_spells_it() {
        for refusal in [
            Refusal::ElevatedWindow,
            Refusal::SecureInput,
            Refusal::ScreenLocked,
            Refusal::NoPermission,
        ] {
            assert!(
                crate::wire::oops::ALL.contains(&refusal.code()),
                "{refusal:?} must be a code the dialect defines"
            );
        }
        assert_eq!(
            crate::wire::oops::ALL.len(),
            5,
            "the fifth is UNRESOLVED, which the engine generates rather than detects"
        );
    }
}
