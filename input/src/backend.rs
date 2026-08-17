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
//!    swallowing, the delta is the difference of two REPORTED POSITIONS: the
//!    ACCELERATED movement, which is what Windows gets for free from its hook
//!    (truth 7), bounded by the pin's anchor being at the centre of the screen so
//!    one event carries up to half a screen. Two traps in that arithmetic cost a
//!    measurement each and are written up in `input/src/x11.rs`: differencing
//!    against the ANCHOR rather than against the last position multiplies the
//!    movement of a burst, and a warp generates a motion event even when it changes
//!    nothing, so a pin whose anchor the server will not accept warps for ever.
//!    Applying the device's own acceleration profile to the raw valuators instead is
//!    a third option and a ticket of its own. Nothing on this seam changes either
//!    way: the engine consumes [`Motion::dx`] and [`Motion::dy`] and asks no
//!    questions.
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
///
/// # Why Linux has seven of these and the other two platforms share three
///
/// Because Linux is the one platform where the answer depends on which desktop
/// the person runs, and the epic's promise is that the interface can say WHICH
/// PARTS this desktop supports. On Windows and macOS a refusal is a permission
/// or nothing. On Linux the same build meets: a session reached through its
/// XWayland; a Wayland session with no D-Bus session bus to ask; a desktop portal
/// that does not offer the input portals at all; one that offers a version this
/// build cannot speak; one that offers everything and then has its consent dialog
/// dismissed; and one that offers everything and would work, on a path nobody has
/// yet run. Collapsing those six into one `wayland` was what the first version
/// did, and the sentence it produced ("this is a Wayland session, X11 only for
/// now") names no remedy for any of them, and on the last two it is simply
/// false.
///
/// Every variant here has a sentence in `gui/ui/src/lib/input.ts`, and
/// [`Problem::ALL`] plus the test that walks it are what keep a new code from
/// arriving without one.
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
    /// A Wayland session this build has no native path for, and nothing more
    /// precise could be established. The catch-all rather than the usual answer:
    /// the six below are what a Wayland session normally gets, and this one is
    /// kept because it is on the frozen wire (a peer on an older build sends it)
    /// and as the honest word for a state a later compositor invents.
    Wayland,
    /// A Wayland session being served through its XWayland: the X11 backend is
    /// real and works, for X11 windows, and native Wayland windows can be
    /// neither observed nor injected to. Most windows on a modern desktop are
    /// the second kind, which is why this is a problem and not a footnote.
    ///
    /// Reported by a session that was FORCED to X11
    /// ([`crate::os::FORCE_X11_ENV`]), which is the ordinary route, and by one more
    /// that a review found: a session with no evidence of anything graphical is
    /// handed to the X11 backend anyway (the probe is advice and the backend is the
    /// authority), and an XWayland whose socket had not yet appeared when the probe
    /// ran arrives there. So it is reachable without anybody forcing anything, by a
    /// start-up race, which is exactly why the code exists rather than being an
    /// assertion that this cannot happen.
    ///
    /// Before this code existed a forced session reported `monitors_unstable`
    /// (XWayland's RandR outputs carry no EDID) and nothing at all about XWayland,
    /// which is a true sentence in place of the one that mattered.
    ///
    /// **It also travels**, which took a second change to arrange. The capability
    /// bits are unchanged under XWayland (this machine really can type, into X11
    /// windows), so for a while a peer being driven through one was offered to the
    /// driving side with no problem of its own: a person crossed to it, typed into a
    /// native Wayland window and nothing happened, with nothing anywhere saying why.
    /// [`Problem::as_peer`] is the mapping that closed it, and
    /// [`PeerProblem::XWayland`] is the far side's half of this sentence.
    XWayland,
    /// A Wayland session with no D-Bus session bus, so the portals cannot even
    /// be asked. A compositor started from a tty without `dbus-run-session` is
    /// the real case.
    WaylandNoBus,
    /// A Wayland session whose desktop portal does not export the input portals.
    /// The capability bits say which half is missing: `InputCapture` is the
    /// source half, `RemoteDesktop` the target half, and a desktop can have one
    /// without the other.
    ///
    /// **`xdg-desktop-portal` running is not evidence that these exist.** Both
    /// are proxied to an `org.freedesktop.impl.portal.*` backend, and a portal
    /// with only the GTK backend installed exports neither: verified on
    /// `xdg-desktop-portal` 1.18.4 with `xdg-desktop-portal-gtk` 1.15.1, whose
    /// bus carries nineteen portal interfaces and none of these two.
    WaylandNoPortal,
    /// The input portals are there at a version older than this build can speak.
    /// Its own number is in the log; the remedy is a newer
    /// `xdg-desktop-portal` and a newer backend for the desktop.
    WaylandPortalOld,
    /// The portal is there, new enough, and refused: the consent dialog was
    /// dismissed, or the compositor said no. Distinct from the three above
    /// because nothing is missing and nothing needs installing: the remedy is to
    /// switch the feature on again and allow it.
    WaylandPortalRefused,
    /// Every piece the Wayland path needs is present, and that path has never
    /// been run against a real compositor, so it stays off unless it is switched
    /// on deliberately ([`crate::os::WAYLAND_ENV`]).
    ///
    /// The honest state rather than a missing one, and the reason it is a
    /// `Problem` at all: a backend that claimed `capture` and `inject_keys` on
    /// the strength of code nobody has executed would be exactly the lie this
    /// whole component is built not to tell.
    WaylandUntested,
}

impl Problem {
    /// Every problem this build knows, so a test can walk them.
    ///
    /// The list exists because the codes cross a language boundary: each one
    /// needs a sentence in `gui/ui/src/lib/input.ts` and a name in that file's
    /// `InputProblem` union, and neither the Rust compiler nor `tsc` can see the
    /// other side. The test in this module reads that file and checks every code
    /// against it, which is the only bridge available and is cheap.
    ///
    /// **A new variant cannot be left out of it**, and that took a review to arrange.
    /// The first version was a hand-written array, so adding a variant broke `code()`
    /// and `os::describe` (both exhaustive matches) and left this list quietly short,
    /// which made every test that walks it pass while a person hit "this version does
    /// not know why". [`Problem::position`] is the exhaustive match that closes it: a
    /// new variant fails to compile there, and the test below fails if it is not in
    /// this array at that position.
    pub const ALL: &'static [Problem] = &[
        Problem::NoBackend,
        Problem::NoPermission,
        Problem::MonitorsUnstable,
        Problem::Wayland,
        Problem::XWayland,
        Problem::WaylandNoBus,
        Problem::WaylandNoPortal,
        Problem::WaylandPortalOld,
        Problem::WaylandPortalRefused,
        Problem::WaylandUntested,
    ];

    /// Where this problem sits in [`Problem::ALL`].
    ///
    /// Exists ONLY to make that list exhaustive by construction: it is an exhaustive
    /// match, so a variant added to this enum is a compile error here until somebody
    /// gives it a position, and the test then checks that the array really holds it
    /// there. Nothing else calls it, which is why it is test-only: `cargo clippy
    /// --all-targets` and CI both compile the tests, so the compile error still lands
    /// on whoever adds a variant.
    #[cfg(test)]
    const fn position(self) -> usize {
        match self {
            Problem::NoBackend => 0,
            Problem::NoPermission => 1,
            Problem::MonitorsUnstable => 2,
            Problem::Wayland => 3,
            Problem::XWayland => 4,
            Problem::WaylandNoBus => 5,
            Problem::WaylandNoPortal => 6,
            Problem::WaylandPortalOld => 7,
            Problem::WaylandPortalRefused => 8,
            Problem::WaylandUntested => 9,
        }
    }

    /// The wire and snapshot spelling (`input.status`'s `problem`).
    pub fn code(self) -> &'static str {
        match self {
            Problem::NoBackend => "no_backend",
            Problem::NoPermission => "no_permission",
            Problem::MonitorsUnstable => "monitors_unstable",
            Problem::Wayland => "wayland",
            Problem::XWayland => "xwayland",
            Problem::WaylandNoBus => "wayland_no_bus",
            Problem::WaylandNoPortal => "wayland_no_portal",
            Problem::WaylandPortalOld => "wayland_portal_old",
            Problem::WaylandPortalRefused => "wayland_portal_refused",
            Problem::WaylandUntested => "wayland_untested",
        }
    }

    /// The problem a code names, for reading a peer's `caps`. `None` for a code
    /// this build does not know, which is the fail-closed answer: a problem we
    /// cannot name is one we do not repeat to the interface as peer-chosen prose.
    pub fn parse(code: &str) -> Option<Problem> {
        Problem::ALL.iter().copied().find(|p| p.code() == code)
    }

    /// What THIS problem means for the machine on the other end of it, when that
    /// machine is a target we could drive.
    ///
    /// The two vocabularies are separate on purpose (section 12: one is about this
    /// computer, the other about a pair), and this is the one bridge between them.
    /// It exists because a peer's `caps` already carries its `problem` code and the
    /// snapshot used to drop it on the floor: it read `can_be_driven()` and nothing
    /// else, so a peer that could type into HALF its windows was offered as a peer
    /// with nothing wrong.
    ///
    /// **The match is exhaustive so that a new [`Problem`] cannot be added without
    /// somebody deciding whether it has a far side.** Most of them do not, and the
    /// reason is the same for all: they leave the peer unable to type at all, so
    /// `can_be_driven()` is false and the snapshot already says `no_backend`, which is
    /// both truer and more useful than repeating a local remedy that only the person
    /// sitting at that other machine can carry out.
    pub fn as_peer(self) -> Option<PeerProblem> {
        match self {
            // The one that is neither "all of it works" nor "none of it does".
            Problem::XWayland => Some(PeerProblem::XWayland),
            // Nothing there can type. The peer's own `caps` say so through
            // `can_be_driven()`, and `no_backend` is where those arrive.
            Problem::NoBackend
            | Problem::NoPermission
            | Problem::Wayland
            | Problem::WaylandNoBus
            | Problem::WaylandNoPortal
            | Problem::WaylandPortalOld
            | Problem::WaylandPortalRefused
            | Problem::WaylandUntested => None,
            // A pair whose target cannot tell its own screens apart is a real
            // thing to know, and it is not a thing the driving side needs a
            // REFUSAL slot for: the plane already keeps an absent screen's place
            // and says so on the screen itself (section 6), and the one `problem`
            // slot per device is worth more to a reason a person can act on. Giving
            // it a peer code is a product decision about the plane's own wording,
            // not this mapping's to take.
            Problem::MonitorsUnstable => None,
        }
    }
}

/// What a PAIR cannot do: the far side's word, learned from its handshake or by
/// trying (doc/input-sharing.md, section 12).
///
/// A separate vocabulary from [`Problem`], and separate on purpose: that one is
/// about the computer a person is sitting at and names remedies they can carry out
/// where they are, this one is about another computer and has to send them there, or
/// tell them what will and will not work if they stay. Two of the codes are spelled
/// the same in both ([`Problem::NoBackend`] and this one's `no_backend`, and now
/// XWayland), because they are the same platform fact seen from the two ends, and
/// the interface holds two different sentences for each.
///
/// Every member has a sentence in `gui/ui/src/lib/input.ts` and a name in
/// `api.ts`'s `PeerProblem` union, and the test in this module reads both files to
/// make sure of it. [`PeerProblem::position`] is what makes [`PeerProblem::ALL`]
/// complete by construction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerProblem {
    /// The far side has no grant for this device. Its word, learned by trying:
    /// grants are never replicated (section 9).
    NotAllowed,
    /// That computer is being driven by another already, or is driving one. One
    /// keyboard, no preemption.
    Busy,
    /// Its pointer is pinned to its own screen, or it is locked and cannot be
    /// driven at all.
    Locked,
    /// Nothing on that computer can type, or its permission is refused. Said from
    /// the handshake alone, before anyone tries, which is what `caps` is on the wire
    /// for.
    NoBackend,
    /// The deployment's relays are rendezvous-only above a cap (#88) and no direct
    /// path formed.
    NoPath,
    /// The path to that computer is past the pointer threshold. **Reserved**:
    /// nothing sets it in v1, because the threshold is enforced where a pointer is
    /// actually asked for (`input.take` answers `INPUT_TOO_SLOW`) and the number an
    /// interface would word it from is already in `rtt_ms`. It keeps its sentence
    /// for the day something does.
    TooSlow,
    /// The two ends do not hold the same plane. Self-repairing: a layout round is
    /// already running.
    PlaneStale,
    /// That computer is a Wayland desktop reached through its XWayland, so it can be
    /// typed into and it cannot be typed into everywhere: X11 programs receive the
    /// keyboard and the pointer, native Wayland ones receive nothing at all.
    ///
    /// **This is a partial success and it is the reason this member exists.** Every
    /// other code here means a session did not happen; this one means a session
    /// happens and then silently does nothing for most of the windows on that
    /// screen, which is the "best effort that lies" the whole component is built to
    /// forbid. It is read from the peer's own `caps.problem` (which already
    /// travelled), never guessed, and it never blocks anything: a person told what
    /// will work may perfectly well want it.
    XWayland,
}

impl PeerProblem {
    /// Every problem a pair can have, so a test can walk them. Complete by
    /// construction through [`PeerProblem::position`], for the reason
    /// [`Problem::ALL`] documents at length: a hand-written array silently falls
    /// short, and every test that walks it then passes while a person is told this
    /// version does not know why.
    pub const ALL: &'static [PeerProblem] = &[
        PeerProblem::NotAllowed,
        PeerProblem::Busy,
        PeerProblem::Locked,
        PeerProblem::NoBackend,
        PeerProblem::NoPath,
        PeerProblem::TooSlow,
        PeerProblem::PlaneStale,
        PeerProblem::XWayland,
    ];

    /// Where this problem sits in [`PeerProblem::ALL`]. Exists only to make that
    /// array exhaustive by construction: a new variant is a compile error here until
    /// somebody gives it a position, and the test then checks the array holds it
    /// there.
    #[cfg(test)]
    const fn position(self) -> usize {
        match self {
            PeerProblem::NotAllowed => 0,
            PeerProblem::Busy => 1,
            PeerProblem::Locked => 2,
            PeerProblem::NoBackend => 3,
            PeerProblem::NoPath => 4,
            PeerProblem::TooSlow => 5,
            PeerProblem::PlaneStale => 6,
            PeerProblem::XWayland => 7,
        }
    }

    /// The snapshot spelling (`input.status`'s per device `problem`).
    pub fn code(self) -> &'static str {
        match self {
            PeerProblem::NotAllowed => "not_allowed",
            PeerProblem::Busy => "busy",
            PeerProblem::Locked => "locked",
            PeerProblem::NoBackend => "no_backend",
            PeerProblem::NoPath => "no_path",
            PeerProblem::TooSlow => "too_slow",
            PeerProblem::PlaneStale => "plane_stale",
            PeerProblem::XWayland => "xwayland",
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
    ///
    /// # What it does NOT mean, and what to send instead
    ///
    /// It means the CAPABILITIES moved. The engine throws the resolution cache away only
    /// when one of the three that decide what this machine can produce has changed
    /// (`inject_keys`, `unicode`, `inject_pointer`), because throwing it away is five
    /// round trips plus a fresh resolve on the next press of every distinct symbol, and
    /// this seam puts no rate limit on the upcall: a backend that emits it for its own
    /// housekeeping (a rebuilt event tap, a monitor coming back) would otherwise pay that
    /// on every occurrence. So a backend whose KEYMAP connection was rebuilt, and which
    /// therefore wants the cache emptied, sends [`BackendEvent::LayoutChanged`], which is
    /// the upcall that means exactly that. Emitting this one with unchanged capabilities is
    /// harmless and is meant to be.
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
        for &problem in Problem::ALL {
            assert_eq!(Problem::parse(problem.code()), Some(problem));
        }
        assert_eq!(Problem::parse("no_backend_at_all"), None);
        assert_eq!(Problem::parse(""), None);

        // Two codes for one problem, or one code for two, would both be a
        // vocabulary with a bug waiting: the snapshot carries the code and the
        // interface matches on it.
        let mut codes: Vec<&str> = Problem::ALL.iter().map(|p| p.code()).collect();
        let before = codes.len();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(before, codes.len(), "two problems share a code");

        // **And the list holds every variant, which is what makes every other test
        // that walks it worth anything.** `position` is an exhaustive match, so a new
        // variant cannot compile without one; this is the half that checks the array
        // agrees. A variant with a position and no entry fails here, and so does an
        // array somebody has shortened.
        for (index, problem) in Problem::ALL.iter().enumerate() {
            assert_eq!(
                problem.position(),
                index,
                "{problem:?} sits at {index} in Problem::ALL and says it belongs at {}",
                problem.position()
            );
        }
        assert_eq!(
            Problem::ALL.len(),
            Problem::WaylandUntested.position() + 1,
            "the array is shorter than the positions the enum defines"
        );
    }

    /// **Every problem this build can report has a sentence in the interface.**
    ///
    /// A reason code with no sentence is the defect this test exists to catch, and
    /// it is a defect a compiler cannot see: the codes are produced in Rust and
    /// worded in TypeScript, and the two halves are checked by different tools. So
    /// this reads the interface's own source and looks for each code in it. It is a
    /// coarse check (a code in a comment would satisfy it) and it is the right
    /// coarseness: the expensive failure is a code that reaches a person as "this
    /// version does not know the reason", and a code mentioned nowhere in that file
    /// is guaranteed to do exactly that.
    ///
    /// The companion check is on the other side, in `input.test.ts`, which walks its
    /// own `InputProblem` union and asserts each member gets a real sentence rather
    /// than the fallback. Together they close the loop in both directions.
    #[test]
    fn every_problem_this_build_can_report_has_a_sentence_in_the_interface() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../gui/ui/src/lib/input.ts")
            .canonicalize()
            .expect("the interface's input module is where this crate's sibling GUI keeps it");
        let source = std::fs::read_to_string(&path).expect("read the interface's input module");
        for &problem in Problem::ALL {
            let quoted = format!("\"{}\"", problem.code());
            assert!(
                source.contains(&quoted),
                "{problem:?} is reported by this build and {} says nothing about it, \
                 so a person hitting it is told this version does not know why",
                path.display()
            );
        }

        // And the union the interface types its snapshot with, which is what makes
        // the sentence reachable rather than dead code behind a `default:` arm.
        let types = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../gui/ui/src/lib/api.ts")
            .canonicalize()
            .expect("the interface's api module");
        let types = std::fs::read_to_string(&types).expect("read the interface's api module");
        let union = types
            .split_once("export type InputProblem =")
            .expect("the InputProblem union")
            .1
            .split_once(';')
            .expect("the union ends")
            .0;
        for &problem in Problem::ALL {
            assert!(
                union.contains(&format!("\"{}\"", problem.code())),
                "{problem:?} is missing from the interface's InputProblem union"
            );
        }
    }

    /// The pair vocabulary gets the same two guarantees as the local one: no two
    /// members share a code, and the array holds every member at the position the
    /// enum's exhaustive match gives it.
    #[test]
    fn every_problem_a_pair_can_have_is_in_the_list_exactly_once() {
        let mut codes: Vec<&str> = PeerProblem::ALL.iter().map(|p| p.code()).collect();
        let before = codes.len();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(before, codes.len(), "two peer problems share a code");
        for (index, problem) in PeerProblem::ALL.iter().enumerate() {
            assert_eq!(
                problem.position(),
                index,
                "{problem:?} sits at {index} in PeerProblem::ALL and says it belongs at {}",
                problem.position()
            );
        }
        assert_eq!(
            PeerProblem::ALL.len(),
            PeerProblem::XWayland.position() + 1,
            "the array is shorter than the positions the enum defines"
        );
    }

    /// **Every problem a pair can have has a sentence in the interface**, the same
    /// check across the same language boundary as the one above it, for the other
    /// vocabulary. A code with no sentence reaches a person as "reported a problem
    /// this version does not know", which is the whole defect.
    ///
    /// The interface's own half of this bridge is stronger than a string scan and
    /// worth naming here: `PEER_PROBLEMS` is typed `Record<PeerProblem, string>`, so
    /// `tsc` refuses to compile a member with no sentence, and `input.test.ts` walks a
    /// `Record<PeerProblem, true>` so a member added to the union cannot go
    /// unexercised. This side is what catches the case those cannot see: a code minted
    /// in Rust that never reached the union at all.
    #[test]
    fn every_problem_a_pair_can_have_has_a_sentence_in_the_interface() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let path = dir
            .join("../gui/ui/src/lib/input.ts")
            .canonicalize()
            .expect("the interface's input module is where this crate's sibling GUI keeps it");
        let source = std::fs::read_to_string(&path).expect("read the interface's input module");
        // The table itself rather than the whole file: these codes are keys of one
        // object, and a mention in a comment somewhere else in the module is not a
        // sentence. `not_allowed` also appears in this file as an `input.allow`
        // gesture's own wording, which a file-wide scan would have accepted.
        let table = source
            .split_once("const PEER_PROBLEMS")
            .expect("the interface's table of pair sentences")
            .1
            .split_once("\n};")
            .expect("the table ends")
            .0;
        for &problem in PeerProblem::ALL {
            assert!(
                table.contains(&format!("{}:", problem.code())),
                "{problem:?} is reported by this build and PEER_PROBLEMS in {} has no sentence \
                 for it, so a person hitting it is told this version does not know why",
                path.display()
            );
        }

        let types = dir
            .join("../gui/ui/src/lib/api.ts")
            .canonicalize()
            .expect("the interface's api module");
        let types = std::fs::read_to_string(&types).expect("read the interface's api module");
        let union = types
            .split_once("export type PeerProblem =")
            .expect("the PeerProblem union")
            .1
            .split_once(';')
            .expect("the union ends")
            .0;
        for &problem in PeerProblem::ALL {
            assert!(
                union.contains(&format!("\"{}\"", problem.code())),
                "{problem:?} is missing from the interface's PeerProblem union"
            );
        }
    }

    /// The bridge between the two vocabularies, in the only direction it goes.
    ///
    /// The far side of an XWayland session is the one local problem with a pair
    /// meaning, and it is a partial success rather than a failure: the peer can still
    /// be driven, so nothing about it may be read as a refusal.
    #[test]
    fn a_local_problem_with_a_far_side_has_exactly_one() {
        assert_eq!(Problem::XWayland.as_peer(), Some(PeerProblem::XWayland));
        for &problem in Problem::ALL {
            if let Some(peer) = problem.as_peer() {
                assert_eq!(
                    problem,
                    Problem::XWayland,
                    "{problem:?} gained a far side ({peer:?}): give it a row in section 12 \
                     and a sentence, then say so here"
                );
                assert!(
                    PeerProblem::ALL.contains(&peer),
                    "{peer:?} is not in the pair vocabulary"
                );
            }
        }
        // Deliberately no assertion about the other nine here. The reason they map to
        // nothing is that a machine reporting one cannot type at all, so its own
        // capability bits already carry it to the driving side as `no_backend`, and
        // that is a property of the three platform backends rather than of this enum:
        // asserting it from a `Capabilities::none` would be a tautology dressed as
        // coverage. What guards it is the exhaustive match itself, which stops a tenth
        // problem from being added without somebody answering the question.
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
