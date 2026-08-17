// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The Wayland half: the `InputCapture` portal for reading this machine's keyboard
//! and mouse, the `RemoteDesktop` portal for typing on it, and above all an honest
//! account of which of those a given desktop actually has.
//!
//! # Read this before anything else in this file
//!
//! **Nothing in this module's transport has ever been executed against a real
//! compositor.** No machine available while it was written implemented either
//! portal: the development machine runs a Weston-derived compositor under
//! `xdg-desktop-portal` 1.18.4 with only the GTK backend, whose bus carries
//! nineteen portal interfaces and neither of the two this needs. So the honesty is
//! structural rather than promised, and it is arranged in three layers:
//!
//! 1. **The probe RUNS.** [`probe`] talks to the real session bus, and the
//!    machine this was written on is a witness for the answer that matters most:
//!    a Wayland session whose portal does not offer these interfaces, which
//!    reports [`Problem::WaylandNoPortal`] and says so on screen. The mapping from
//!    a D-Bus error to that answer was derived from that bus rather than from the
//!    specification, and the difference between the two is a trap recorded in
//!    [`classify`].
//! 2. **The logic is provable without a bus.** Everything between the probe and
//!    the transport (the barrier geometry, the zone serial rule, the capability
//!    negotiation, the session lifecycle, the injection mapping, the event
//!    decoding) is a pure function or a state machine over the [`Portal`] seam,
//!    and the tests drive it with a scripted double, including every refusal.
//! 3. **The transport is one separate file**, [`crate::wayland_portal`], which is
//!    the D-Bus and EI protocol code and nothing else. It is the file nobody has
//!    run. Keeping it out of here is what lets a reviewer see the boundary instead
//!    of being told about it.
//!
//! And the gate: the backend is not built at all unless
//! [`crate::os::WAYLAND_ENV`] is set. A desktop with everything present therefore
//! reports [`Problem::WaylandUntested`] rather than claiming `capture` and
//! `inject_keys` on the strength of code nobody has executed. That claim is the
//! exact lie this component exists not to tell.
//!
//! # What each portal can and cannot do, which is not symmetric
//!
//! ## Capture: `org.freedesktop.portal.InputCapture`, and libei is not optional
//!
//! The portal manages WHEN input is captured and nothing else. Its own words:
//! "The transport of actual input events is delegated to a transport layer,
//! specifically libei." It has four signals (`Activated`, `Deactivated`,
//! `Disabled`, `ZonesChanged`) and not one of them carries a keystroke, and no
//! method returns events. `ConnectToEIS` hands back a file descriptor and the
//! events arrive on it, over the EI protocol, or they do not arrive.
//!
//! So capture on Wayland is: a session, the pointer barriers that say where the
//! pointer leaves, an EI connection, `Enable`, and then an `Activated` signal each
//! time the pointer crosses one of those barriers. There is no way to do it over
//! D-Bus alone, so an EI client is a hard requirement of the capture half and not a
//! choice, which is what shapes the dependency argument in the pull request.
//!
//! ## Injection: `org.freedesktop.portal.RemoteDesktop`, and libei IS optional
//!
//! The reverse. `RemoteDesktop` has `NotifyKeyboardKeycode`,
//! `NotifyKeyboardKeysym`, `NotifyPointerMotion`, `NotifyPointerButton` and both
//! axis calls, all over plain D-Bus, none of them deprecated (the XML carries a
//! deprecation annotation on `InputCapture.CreateSession` and on none of these),
//! and both GNOME's mutter and KDE's portal implement every one of them today. So
//! this build types over D-Bus and does NOT open an EI connection for injection.
//!
//! Two reasons, and the second is the load-bearing one. It is one fewer never-run
//! protocol on the target side; and the two are mutually exclusive by
//! specification, so choosing wrongly is not a performance question but a broken
//! session: "Once an EIS connection is established, input events must be sent
//! exclusively via the EIS connection. Any events submitted via
//! NotifyPointerMotion, NotifyKeyboardKeycode and other Notify* methods will
//! return an error."
//!
//! ## The one capability a Wayland target genuinely does not have
//!
//! **Absolute pointer positions.** `NotifyPointerMotionAbsolute` takes a `stream`
//! argument, "the PipeWire stream node the coordinate is relative to", and there is
//! no stream without a `org.freedesktop.portal.ScreenCast` session sharing the same
//! session handle. mutter's implementation is explicit about it: with no screen
//! cast paired to the session the call fails with "No screen cast active".
//!
//! This engine sends absolute positions by default, computed by the source in the
//! target's own space, because a lost update must not make the pointer drift
//! (doc/input-sharing.md, section 5). A Wayland target can therefore be typed on
//! and cannot be pointed at, unless its owner also consents to a SCREEN CAPTURE,
//! which is a permission this feature has no business asking for.
//!
//! So a Wayland machine is a **keyboard-only target**, which is a shape the design
//! already has a name and a sentence for, and [`Capabilities::inject_pointer`] is
//! `false` there. It is reported as a capability rather than discovered as a
//! refusal, which is the whole point of that structure. Lifting it needs a product
//! decision (pair a screen cast) or an engine change (a relative-only target mode),
//! and neither is a line of code in this file.
//!
//! # Per compositor, as of August 2026
//!
//! The table lives in doc/input-sharing.md, section 10, because it is the sort of
//! fact a person reads before installing something. The short version, and every
//! row of it is from a manifest or a backend's own source rather than from a run:
//! GNOME has both portals since 45, KDE Plasma both since 6.1, Hyprland has
//! `InputCapture` since its portal 1.4.0 and no `RemoteDesktop` at all, wlroots'
//! portal (sway, river, Wayfire) has neither, Weston has neither, and the GTK
//! backend has neither and cannot stand in for one.

use std::fmt;

use crate::backend::{Capabilities, Problem};
use crate::os::{SessionKind, Unsupported};

// ---------------------------------------------------------- the portals' names

/// The bus name every desktop portal lives behind.
pub const PORTAL_BUS: &str = "org.freedesktop.portal.Desktop";
/// The one object all of them are exported on.
pub const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
/// The capture portal.
pub const INPUT_CAPTURE: &str = "org.freedesktop.portal.InputCapture";
/// The injection portal.
pub const REMOTE_DESKTOP: &str = "org.freedesktop.portal.RemoteDesktop";

/// The capability bits both portals use, for `InputCapture`'s
/// `SupportedCapabilities` and its session request, and for `RemoteDesktop`'s
/// `AvailableDeviceTypes` and its `SelectDevices`.
///
/// The same three numbers in both interfaces, which is why they are one module
/// here. "Applications must ignore unknown capabilities", so a bit this build does
/// not know is never an error.
pub mod caps {
    pub const KEYBOARD: u32 = 1;
    pub const POINTER: u32 = 2;
    pub const TOUCHSCREEN: u32 = 4;

    /// What this engine ever asks for. Touch is not an input this feature carries
    /// (doc/input-sharing.md, section 15), so asking for it would be asking a
    /// person to consent to something that is never used.
    pub const WANTED: u32 = KEYBOARD | POINTER;
}

/// The lowest `InputCapture` version this build can work with.
///
/// 1, which is the first and is what GNOME through 50 and KDE through 6.6 report.
/// Version 2 (xdg-desktop-portal 1.21.1, `CreateSession2` plus `Start` plus
/// persistence) is a better session and not a required one: this build uses the
/// deprecated-in-2 `CreateSession`, which keeps working against both, so that one
/// code path serves every desktop that has the portal at all. Persistence is what
/// version 2 buys and it is a comfort (one consent dialog per session instead of
/// per start), so it is a follow-up rather than a floor.
pub const INPUT_CAPTURE_MIN: u32 = 1;

/// The lowest `RemoteDesktop` version this build can work with.
///
/// 1, because every `Notify*` method this build uses is in version 1.
///
/// **And its `version` property must not be used for anything else.** The portal
/// frontend hardcodes it to 2 whatever the desktop backend implements, so it says
/// nothing about whether `ConnectToEIS` exists (the frontend forwards that call
/// blindly and a version 1 backend answers with its own `UnknownMethod`). It is
/// evidence that the interface is THERE and evidence of nothing else. This build
/// never calls `ConnectToEIS`, so the trap costs it nothing, and the note is here
/// so that the next person to want libei injection does not read the number and
/// believe it.
pub const REMOTE_DESKTOP_MIN: u32 = 1;

// -------------------------------------------------------- what a portal answered

/// What came back when a portal was asked something, and it was not an answer.
///
/// Every arm is a different thing to do about it, which is why there are nine of
/// them rather than one `String`: [`PortalError::problem`] turns each into the
/// reason code an interface says a sentence for, and a session-lifetime failure
/// ([`PortalError::Refused`], [`PortalError::SessionClosed`]) has to be
/// distinguished from a start-up one ([`PortalError::NoInterface`]) or the person
/// is told to install something they already have.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PortalError {
    /// There is no D-Bus session bus, or it could not be reached.
    NoBus(String),
    /// Nothing owns the portal's bus name: no `xdg-desktop-portal` at all.
    NoPortal(String),
    /// The interface is not exported on the portal's object. The ordinary answer
    /// on a desktop whose portal backend does not implement it, which is most of
    /// them.
    NoInterface { interface: String },
    /// The interface is there and the method is not: a portal older than the call.
    NoMethod { interface: String, method: String },
    /// The interface is there at a version this build cannot work with.
    TooOld { interface: String, version: u32 },
    /// A human said no. `cancelled` separates "closed the dialog" (response 1)
    /// from "the interaction ended some other way" (response 2), because only the
    /// first is worth offering again immediately.
    Refused { cancelled: bool },
    /// The session is gone: closed by the compositor, revoked, or the portal
    /// restarted under us.
    SessionClosed,
    /// The portal answered with an error of its own. Kept whole, name and message,
    /// because this is the arm a compositor's own vocabulary arrives in and a
    /// truncated one is unreadable in a bug report.
    Failed { name: String, message: String },
    /// The answer did not have the shape the interface says it has. Its own arm
    /// rather than a `Failed`, because it means this build and that portal disagree
    /// about the protocol, which is a different bug from the portal refusing.
    Malformed(String),
    /// Nothing answered inside the budget.
    Timeout,
}

impl PortalError {
    /// The reason code an interface can say a sentence for.
    ///
    /// The mapping is deliberately lossy in one direction only: several transport
    /// failures collapse onto [`Problem::WaylandPortalRefused`] because from a
    /// person's chair they are the same event (they asked for it and did not get
    /// it), while everything that means "something is missing on this computer"
    /// keeps its own code because each has a different remedy.
    pub fn problem(&self) -> Problem {
        match self {
            PortalError::NoBus(_) => Problem::WaylandNoBus,
            // No portal at all and no interface on it are one sentence: in both
            // cases this desktop does not offer what is needed, and the remedy is
            // the same package. The capability bits say which half is missing.
            PortalError::NoPortal(_) | PortalError::NoInterface { .. } => Problem::WaylandNoPortal,
            PortalError::NoMethod { .. } | PortalError::TooOld { .. } => Problem::WaylandPortalOld,
            PortalError::Refused { .. } => Problem::WaylandPortalRefused,
            // A session that closed, a portal that failed and a portal that
            // answered nonsense are all "asked and did not get it", and all three
            // are recoverable by asking again. The log carries which it was.
            PortalError::SessionClosed
            | PortalError::Failed { .. }
            | PortalError::Malformed(_)
            | PortalError::Timeout => Problem::WaylandPortalRefused,
        }
    }

    /// Is asking again worth anything, or is this machine simply not able to?
    ///
    /// The engine consults this before it re-tries a session start: a refusal is
    /// worth another gesture from a person, a missing interface is not, and
    /// re-asking a portal that does not exist once per session start is how a log
    /// fills up with the same line.
    pub fn worth_retrying(&self) -> bool {
        match self {
            PortalError::Refused { .. }
            | PortalError::SessionClosed
            | PortalError::Failed { .. }
            | PortalError::Timeout => true,
            PortalError::NoBus(_)
            | PortalError::NoPortal(_)
            | PortalError::NoInterface { .. }
            | PortalError::NoMethod { .. }
            | PortalError::TooOld { .. }
            | PortalError::Malformed(_) => false,
        }
    }
}

impl fmt::Display for PortalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PortalError::NoBus(why) => write!(f, "no D-Bus session bus: {why}"),
            PortalError::NoPortal(why) => write!(f, "no {PORTAL_BUS} on the session bus: {why}"),
            PortalError::NoInterface { interface } => {
                write!(f, "{interface} is not exported on {PORTAL_PATH}")
            }
            PortalError::NoMethod { interface, method } => {
                write!(
                    f,
                    "{interface} has no {method}: this portal is older than the call"
                )
            }
            PortalError::TooOld { interface, version } => {
                write!(f, "{interface} is at version {version}, which is too old")
            }
            PortalError::Refused { cancelled } => {
                if *cancelled {
                    write!(f, "the request was cancelled")
                } else {
                    write!(f, "the request ended without being granted")
                }
            }
            PortalError::SessionClosed => write!(f, "the portal session is closed"),
            PortalError::Failed { name, message } => write!(f, "{name}: {message}"),
            PortalError::Malformed(what) => {
                write!(f, "the portal answered something unexpected: {what}")
            }
            PortalError::Timeout => write!(f, "the portal did not answer in time"),
        }
    }
}

/// The D-Bus error names this build recognises, so the strings live in one place.
mod dbus_errors {
    pub const SERVICE_UNKNOWN: &str = "org.freedesktop.DBus.Error.ServiceUnknown";
    pub const NAME_HAS_NO_OWNER: &str = "org.freedesktop.DBus.Error.NameHasNoOwner";
    pub const UNKNOWN_INTERFACE: &str = "org.freedesktop.DBus.Error.UnknownInterface";
    pub const UNKNOWN_PROPERTY: &str = "org.freedesktop.DBus.Error.UnknownProperty";
    pub const UNKNOWN_METHOD: &str = "org.freedesktop.DBus.Error.UnknownMethod";
    pub const UNKNOWN_OBJECT: &str = "org.freedesktop.DBus.Error.UnknownObject";
    pub const INVALID_ARGS: &str = "org.freedesktop.DBus.Error.InvalidArgs";
    pub const NO_REPLY: &str = "org.freedesktop.DBus.Error.NoReply";
    pub const TIMEOUT: &str = "org.freedesktop.DBus.Error.Timeout";
    pub const TIMED_OUT: &str = "org.freedesktop.DBus.Error.TimedOut";
    pub const DISCONNECTED: &str = "org.freedesktop.DBus.Error.Disconnected";
    pub const ACCESS_DENIED: &str = "org.freedesktop.DBus.Error.AccessDenied";
}

/// Turns a D-Bus error into something this engine can act on.
///
/// # The trap, and it is why this function exists rather than a `match` on the name
///
/// **Asking for a property of an interface that is not there answers
/// `org.freedesktop.DBus.Error.InvalidArgs`, not `UnknownInterface`.** Measured on
/// the development machine's own bus against `xdg-desktop-portal` 1.18.4:
///
/// ```text
/// Properties.Get(org.freedesktop.portal.InputCapture, "version")
///   -> org.freedesktop.DBus.Error.InvalidArgs: No such interface "..."
/// InputCapture.CreateSession(...)
///   -> org.freedesktop.DBus.Error.UnknownMethod: No such interface "..." on object at path ...
/// Properties.GetAll(org.freedesktop.portal.InputCapture)
///   -> org.freedesktop.DBus.Error.InvalidArgs: No such interface "..."
/// ```
///
/// So the two ways of asking give two different error NAMES for one fact, and
/// neither is the name a reading of the specification would predict. A classifier
/// keyed on the name alone reports `WaylandPortalRefused` (through the `Failed`
/// arm) for a desktop that simply does not have the portal, and then tells its
/// owner to try again, for ever.
///
/// The rule that survives both, and any third spelling a future portal invents:
/// **if the message says the interface is not there, the interface is not there**,
/// whatever the name on the envelope. The interface we asked about is passed in so
/// the message can be checked against it rather than against a substring that could
/// belong to some other interface.
///
/// `GetAll` was considered as the probe instead of `Get`, on the theory that it
/// might answer an empty dictionary rather than an error and so be less ambiguous.
/// It does not: it answers the same `InvalidArgs`. Recorded because it is the
/// obvious next idea.
pub fn classify(name: &str, message: &str, interface: &str) -> PortalError {
    let absent_interface = message.contains("No such interface")
        || (message.contains(interface) && message.contains("No such"));
    match name {
        dbus_errors::SERVICE_UNKNOWN | dbus_errors::NAME_HAS_NO_OWNER => {
            PortalError::NoPortal(message.to_string())
        }
        dbus_errors::DISCONNECTED => PortalError::NoBus(message.to_string()),
        dbus_errors::UNKNOWN_INTERFACE | dbus_errors::UNKNOWN_OBJECT => PortalError::NoInterface {
            interface: interface.to_string(),
        },
        // A property this build asked for and the interface does not have. Treated
        // as the interface being unusable rather than as a soft miss: the only
        // properties this build reads are `version` and the capability bits, and an
        // interface missing either is not one it can negotiate with.
        dbus_errors::UNKNOWN_PROPERTY => PortalError::NoInterface {
            interface: interface.to_string(),
        },
        // The name that means "the interface is not there" when a METHOD was
        // called, and means "this portal is older than the method" when it is.
        // Only the message can tell them apart, and getting it wrong the pessimistic
        // way costs a sentence while getting it wrong the optimistic way costs a
        // person being told to upgrade software that is already current.
        dbus_errors::UNKNOWN_METHOD if absent_interface => PortalError::NoInterface {
            interface: interface.to_string(),
        },
        dbus_errors::UNKNOWN_METHOD => PortalError::NoMethod {
            interface: interface.to_string(),
            method: message.to_string(),
        },
        // And the trap itself.
        dbus_errors::INVALID_ARGS if absent_interface => PortalError::NoInterface {
            interface: interface.to_string(),
        },
        dbus_errors::NO_REPLY | dbus_errors::TIMEOUT | dbus_errors::TIMED_OUT => {
            PortalError::Timeout
        }
        // A bus policy or a portal that will not talk to this client. Not a missing
        // piece and not a person's choice, but from the chair it is the same as
        // being refused, and it is worth another try after whatever changed.
        dbus_errors::ACCESS_DENIED => PortalError::Refused { cancelled: false },
        _ if absent_interface => PortalError::NoInterface {
            interface: interface.to_string(),
        },
        _ => PortalError::Failed {
            name: name.to_string(),
            message: message.to_string(),
        },
    }
}

/// The response code of a portal `Request`, as its `Response` signal carries it.
///
/// 0 granted, 1 the person cancelled, 2 the interaction ended some other way. A
/// code this build does not know is treated as a refusal rather than as a success,
/// which is the fail-closed direction: a session that never starts costs a sentence
/// and a session that starts on a "maybe" swallows somebody's keyboard.
pub fn response(code: u32) -> Result<(), PortalError> {
    match code {
        0 => Ok(()),
        1 => Err(PortalError::Refused { cancelled: true }),
        _ => Err(PortalError::Refused { cancelled: false }),
    }
}

// ------------------------------------------------------------------- the probe

/// What one portal interface turned out to be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Offer {
    /// Exported, at this version, advertising these capability bits.
    Present { version: u32, capabilities: u32 },
    /// Not usable, and why.
    Missing(PortalError),
}

impl Offer {
    /// Is this interface there, new enough, and offering everything asked of it?
    ///
    /// All three, not one: an interface present at a version this build cannot
    /// speak, or present and advertising neither a keyboard nor a pointer, is as
    /// unusable as an absent one and the difference is only in the sentence.
    pub fn usable(&self, min_version: u32, wanted: u32) -> bool {
        match self {
            Offer::Present {
                version,
                capabilities,
            } => *version >= min_version && capabilities & wanted != 0,
            Offer::Missing(_) => false,
        }
    }

    /// Which of `wanted` this interface actually offers.
    pub fn granted(&self, wanted: u32) -> u32 {
        match self {
            Offer::Present { capabilities, .. } => capabilities & wanted,
            Offer::Missing(_) => 0,
        }
    }

    /// Why this interface cannot be used, or `None` when it can.
    pub fn why(&self, min_version: u32, wanted: u32) -> Option<PortalError> {
        match self {
            Offer::Missing(e) => Some(e.clone()),
            Offer::Present {
                version,
                capabilities,
            } => {
                if *version < min_version {
                    Some(PortalError::TooOld {
                        interface: String::new(),
                        version: *version,
                    })
                } else if capabilities & wanted == 0 {
                    // Present, current, and offering nothing this engine can use:
                    // a touchscreen-only capture portal is the real shape of it.
                    // "Not there" is the honest word from where a person sits.
                    Some(PortalError::NoInterface {
                        interface: String::new(),
                    })
                } else {
                    None
                }
            }
        }
    }
}

/// What this desktop's portal offers, both halves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    /// The session this machine is in, which decides whether any of this applies.
    pub session: SessionKind,
    /// The capture half.
    pub capture: Offer,
    /// The injection half.
    pub inject: Offer,
}

impl Report {
    /// The capabilities this desktop can honestly claim, before anything is tried.
    ///
    /// # Every bit here is argued, because every optimistic one is a lie a person
    /// # discovers by losing their keyboard
    ///
    /// - `capture` and `swallow` are one thing on this platform and always agree.
    ///   An `InputCapture` session IS exclusive: while it is active the events go
    ///   to this client and not to the desktop. There is no observe-without-consume
    ///   mode to report separately, which is the opposite of X11, where raw XI2
    ///   events can be watched and only a grab can swallow.
    /// - `confine` is `true` when capture is, and it is the two-part promise that
    ///   [`Capabilities::confine`] documents rather than a pin: while a capture is
    ///   active the compositor holds the pointer at the barrier, so it cannot walk
    ///   off this desktop, and the relative motion arrives over EI as the
    ///   compositor's own accelerated deltas. Both halves hold, by a mechanism that
    ///   is neither `ClipCursor` nor a grab. The rectangle handed to
    ///   [`crate::backend::InputBackend::confine`] is advisory here, as it is on
    ///   macOS and for the same kind of reason.
    /// - **`warp` is `false`, and that is not a gap in this code.** There is no
    ///   call anywhere in either portal that puts the pointer somewhere. The
    ///   closest thing is `Release`'s `cursor_position`, which the interface
    ///   itself calls a suggestion the compositor may ignore, and which only
    ///   applies at the moment a capture ends. The backend does use it (a warp
    ///   asked for while a capture is active becomes the release position), and
    ///   that is a best effort on one path rather than the capability, so the bit
    ///   says false.
    /// - `inject_keys` is `true` when `RemoteDesktop` offers a keyboard.
    /// - `unicode` is `true` with it, and this is the one place Wayland is BETTER
    ///   off than X11: `NotifyKeyboardKeysym` takes a keysym, and the
    ///   `0x01000000 | code point` convention expresses any character there is, on
    ///   any layout, with no keymap to consult and nothing to bind. X11 needs a
    ///   spare keycode for the same job and does not always have one.
    /// - **`inject_pointer` is `false` even when `RemoteDesktop` offers a
    ///   pointer**, and the module header carries the whole argument: absolute
    ///   positions need a paired screen cast, this engine sends absolute
    ///   positions, and a bit that said `true` and then produced a frozen pointer
    ///   is precisely the failure [`Capabilities::confine`]'s two-part contract
    ///   exists to prevent. A Wayland machine is a keyboard-only target and says
    ///   so up front.
    /// - `monitors_stable` is `true`: the layout's screens come from
    ///   `GetZones`, whose `zone_set` serial changes whenever they do, so a screen
    ///   never silently becomes a different screen. What zones do NOT carry is a
    ///   name or an EDID, which costs the identity its meaning across a reboot and
    ///   is written up on [`crate::backend::Monitor`]'s deferred list rather than
    ///   hidden in this bit.
    pub fn capabilities(&self, gate: Gate) -> Capabilities {
        let capture = self.capture.usable(INPUT_CAPTURE_MIN, caps::WANTED)
            && self.capture.granted(caps::WANTED) & caps::POINTER != 0;
        let keyboard = self.inject.usable(REMOTE_DESKTOP_MIN, caps::WANTED)
            && self.inject.granted(caps::WANTED) & caps::KEYBOARD != 0;
        // The gate is applied here rather than at the call site so that there is
        // exactly one place where "present" becomes "claimed".
        let on = gate == Gate::On;
        Capabilities {
            capture: on && capture,
            swallow: on && capture,
            confine: on && capture,
            warp: false,
            inject_keys: on && keyboard,
            unicode: on && keyboard,
            inject_pointer: false,
            monitors_stable: on && capture,
            problem: self.problem(gate),
        }
    }

    /// The one code that reaches the interface, and its precedence.
    ///
    /// One slot for two halves, so a rule is needed. It is: **name what is
    /// missing before naming what is merely unproven, and name the same thing for
    /// both halves when both are missing the same way.** So a desktop with no
    /// portal at all says `wayland_no_portal` whether it wanted to capture or to
    /// be typed on; a desktop with everything says `wayland_untested`, because
    /// that is then the only thing wrong with it; and a desktop with one half
    /// says whatever that half's failure was, with the capability bits carrying
    /// which half it is.
    pub fn problem(&self, gate: Gate) -> Option<Problem> {
        let capture_why = self.capture.why(INPUT_CAPTURE_MIN, caps::WANTED);
        let inject_why = self.inject.why(REMOTE_DESKTOP_MIN, caps::WANTED);
        match (capture_why, inject_why) {
            // Everything is there. The only remaining truth is whether it has ever
            // been run, and until it has, that is the problem.
            (None, None) => (gate == Gate::Off).then_some(Problem::WaylandUntested),
            // Both halves failed. One sentence, and the worse code wins: a bus that
            // is not there outranks an interface that is not there, which outranks
            // a version that is too old.
            (Some(a), Some(b)) => Some(worse(a.problem(), b.problem())),
            (Some(one), None) | (None, Some(one)) => Some(one.problem()),
        }
    }
}

/// Which of two problems is the more fundamental, for the single `problem` slot.
///
/// The order is the order in which a person would have to fix them: there is no
/// point telling someone to allow a dialog on a machine with no session bus.
fn worse(a: Problem, b: Problem) -> Problem {
    fn rank(p: Problem) -> u8 {
        match p {
            Problem::WaylandNoBus => 0,
            Problem::WaylandNoPortal => 1,
            Problem::WaylandPortalOld => 2,
            Problem::WaylandPortalRefused => 3,
            Problem::WaylandUntested => 4,
            // Anything else did not come from this module and is left alone at the
            // bottom, so a future code cannot silently outrank a real absence.
            _ => 5,
        }
    }
    if rank(a) <= rank(b) { a } else { b }
}

/// Whether the never-executed Wayland path is switched on.
///
/// A two-state type rather than a `bool` at half a dozen call sites, because the
/// two states are not "more" and "less" of anything: `Off` is the shipped default
/// and means the capabilities are all false with a `wayland_untested` on them, and
/// `On` means somebody deliberately asked for unproven code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gate {
    Off,
    On,
}

impl Gate {
    /// From the environment ([`crate::os::WAYLAND_ENV`]).
    pub fn from_env() -> Gate {
        if crate::os::env_switch(crate::os::WAYLAND_ENV) {
            Gate::On
        } else {
            Gate::Off
        }
    }
}

/// Reads one interface's `version` and capability bits.
///
/// Two round trips per interface and four in all, once per process, on a path that
/// decides what an interface tells a person for the rest of the session. The
/// capability property is read only when the version reads, because there is
/// nothing to ask an interface that is not there and a second error in the log
/// helps nobody.
pub fn offer_of(portal: &dyn Portal, interface: &str, capability_property: &str) -> Offer {
    match portal.property_u32(interface, "version") {
        Err(e) => Offer::Missing(e),
        Ok(version) => match portal.property_u32(interface, capability_property) {
            Ok(capabilities) => Offer::Present {
                version,
                capabilities,
            },
            // The version answered and the capabilities did not. Reported as
            // present with NO capabilities rather than as missing, because the
            // interface demonstrably exists: the honest reading is "this portal
            // offers nothing we can use", which `usable` already turns into false.
            Err(e) => {
                warn(&format!("{interface}.{capability_property}: {e}"));
                Offer::Present {
                    version,
                    capabilities: 0,
                }
            }
        },
    }
}

/// Asks this desktop's portal what it can do. The one part of this module that
/// runs against a real bus today.
pub fn probe(portal: &dyn Portal, session: SessionKind) -> Report {
    Report {
        session,
        capture: offer_of(portal, INPUT_CAPTURE, "SupportedCapabilities"),
        inject: offer_of(portal, REMOTE_DESKTOP, "AvailableDeviceTypes"),
    }
}

// ------------------------------------------------------------------- the seam

/// Everything the Wayland half needs from the outside world.
///
/// Shaped as the operations this engine performs rather than as D-Bus, on purpose.
/// A generic "call a method with a vardict" seam would have moved the protocol into
/// the caller and left the tests asserting on argument encodings, which is exactly
/// the code that cannot be checked without a bus. Written this way, everything
/// above the seam is about sessions and barriers and keystrokes, and the whole of
/// the D-Bus is on the other side of it, in one file, unrun.
///
/// Every method is synchronous and blocking. The Wayland backend has a thread of
/// its own, as the other three platform backends do, and a portal round trip is a
/// millisecond on a warm bus: an async seam would have bought nothing and cost the
/// state machine's testability.
pub trait Portal: Send + Sync {
    /// `org.freedesktop.DBus.Properties.Get` of a `u32`.
    fn property_u32(&self, interface: &str, property: &str) -> Result<u32, PortalError>;
}

/// Warnings go to stderr, which the supervisor captures, exactly as the other
/// platform backends do.
pub(crate) fn warn(what: &str) {
    eprintln!("[1device-input] wayland: {what}");
}

// --------------------------------------------------------------- construction

/// Builds the Wayland backend, or says precisely why it cannot be built.
///
/// Never a panic and never an optimistic success: the caller
/// ([`crate::os::create`]) turns the error into an [`crate::os::Absent`] backend
/// carrying the reason, which is what puts the sentence on screen.
pub fn create(kind: SessionKind) -> Result<crate::os::Created, Unsupported> {
    let gate = Gate::from_env();
    let report = match crate::wayland_portal::connect() {
        Ok(portal) => probe(portal.as_ref(), kind),
        Err(e) => {
            warn(&format!("{e}"));
            Report {
                session: kind,
                capture: Offer::Missing(e.clone()),
                inject: Offer::Missing(e),
            }
        }
    };
    warn(&format!(
        "{INPUT_CAPTURE}: {:?}; {REMOTE_DESKTOP}: {:?}",
        report.capture, report.inject
    ));
    let problem = report
        .problem(gate)
        // Everything present and the gate open: there is nothing left to say, and
        // the backend itself is not built yet either. Until it is, the honest word
        // for "asked for a path that does not exist in this build" is the generic
        // one.
        .unwrap_or(Problem::Wayland);
    Err(Unsupported(problem))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// A portal that answers from a script. Every test's whole world.
    #[derive(Default)]
    struct FakePortal {
        properties: Mutex<HashMap<(String, String), Result<u32, PortalError>>>,
    }

    impl FakePortal {
        fn with(entries: &[(&str, &str, Result<u32, PortalError>)]) -> FakePortal {
            let fake = FakePortal::default();
            {
                let mut p = fake.properties.lock().expect("fresh lock");
                for (interface, property, answer) in entries {
                    p.insert(
                        (interface.to_string(), property.to_string()),
                        answer.clone(),
                    );
                }
            }
            fake
        }
    }

    impl Portal for FakePortal {
        fn property_u32(&self, interface: &str, property: &str) -> Result<u32, PortalError> {
            self.properties
                .lock()
                .expect("fresh lock")
                .get(&(interface.to_string(), property.to_string()))
                .cloned()
                // Nothing scripted means nothing there, which is the state of
                // every desktop that does not implement the interface.
                .unwrap_or(Err(PortalError::NoInterface {
                    interface: interface.to_string(),
                }))
        }
    }

    fn both_present(capture_caps: u32, inject_caps: u32) -> FakePortal {
        FakePortal::with(&[
            (INPUT_CAPTURE, "version", Ok(1)),
            (INPUT_CAPTURE, "SupportedCapabilities", Ok(capture_caps)),
            (REMOTE_DESKTOP, "version", Ok(2)),
            (REMOTE_DESKTOP, "AvailableDeviceTypes", Ok(inject_caps)),
        ])
    }

    /// **A desktop with everything says so, and still claims nothing.**
    ///
    /// The state of a GNOME 45 or a Plasma 6.1 machine, and the one this build
    /// cannot honestly serve: every piece is there, no line of the transport has
    /// ever run, so the capabilities are all false and the problem is
    /// `wayland_untested`. Turning the gate on is what turns the claims on.
    #[test]
    fn a_desktop_with_both_portals_is_told_it_is_unproven_rather_than_working() {
        let portal = both_present(caps::WANTED, caps::WANTED);
        let report = probe(&portal, SessionKind::Wayland);

        let off = report.capabilities(Gate::Off);
        assert_eq!(off.problem, Some(Problem::WaylandUntested));
        assert!(
            !off.capture && !off.swallow && !off.confine && !off.inject_keys && !off.unicode,
            "not one capability is claimed while the path is unproven: {off:?}"
        );
        assert!(!off.can_drive() && !off.can_be_driven());

        let on = report.capabilities(Gate::On);
        assert_eq!(
            on.problem, None,
            "switched on, there is nothing left to say"
        );
        assert!(
            on.can_drive(),
            "capture, swallow and confine all come together"
        );
        assert!(on.can_be_driven(), "and the keyboard half is a real target");
        assert!(on.unicode, "the keysym path types anything, with no keymap");
        assert!(
            !on.inject_pointer,
            "absolute positions need a screen cast, so a Wayland machine is a keyboard-only target"
        );
        assert!(
            !on.warp,
            "no call in either portal puts the pointer anywhere"
        );
    }

    /// **This machine's own answer: a Wayland session whose portal has neither
    /// interface.** The witness the whole ticket rests on.
    #[test]
    fn a_portal_with_neither_interface_names_the_portal_and_not_the_person() {
        let portal = FakePortal::default();
        let report = probe(&portal, SessionKind::XWayland);
        for gate in [Gate::Off, Gate::On] {
            let caps = report.capabilities(gate);
            assert_eq!(
                caps.problem,
                Some(Problem::WaylandNoPortal),
                "a missing portal is missing whether or not the gate is open"
            );
            assert_eq!(caps, Capabilities::none(Some(Problem::WaylandNoPortal)));
        }
    }

    /// Half a desktop: `InputCapture` and no `RemoteDesktop`, which is exactly
    /// Hyprland with its portal 1.4.0. It can drive and cannot be driven, and the
    /// capability bits are what say which half, because there is one problem slot.
    #[test]
    fn a_desktop_with_only_the_capture_portal_can_drive_and_cannot_be_driven() {
        let portal = FakePortal::with(&[
            (INPUT_CAPTURE, "version", Ok(1)),
            (INPUT_CAPTURE, "SupportedCapabilities", Ok(caps::WANTED)),
        ]);
        let report = probe(&portal, SessionKind::Wayland);
        let caps = report.capabilities(Gate::On);
        assert!(caps.can_drive());
        assert!(!caps.can_be_driven());
        assert_eq!(caps.problem, Some(Problem::WaylandNoPortal));

        // And the mirror image: a `RemoteDesktop` and no `InputCapture`.
        let portal = FakePortal::with(&[
            (REMOTE_DESKTOP, "version", Ok(2)),
            (REMOTE_DESKTOP, "AvailableDeviceTypes", Ok(caps::WANTED)),
        ]);
        let caps = probe(&portal, SessionKind::Wayland).capabilities(Gate::On);
        assert!(!caps.can_drive());
        assert!(caps.can_be_driven());
        assert_eq!(caps.problem, Some(Problem::WaylandNoPortal));
    }

    /// A portal that offers only a touchscreen offers nothing this engine uses, and
    /// it says "not there" rather than claiming a capture that would produce
    /// nothing.
    #[test]
    fn a_touchscreen_only_portal_is_no_portal_as_far_as_this_engine_is_concerned() {
        let portal = both_present(caps::TOUCHSCREEN, caps::TOUCHSCREEN);
        let caps = probe(&portal, SessionKind::Wayland).capabilities(Gate::On);
        assert!(!caps.capture && !caps.inject_keys);
        assert_eq!(caps.problem, Some(Problem::WaylandNoPortal));

        // A keyboard-only capture portal is the same answer for the SOURCE half:
        // driving needs the pointer, and a capture with no pointer would swallow
        // keystrokes and never let the pointer leave.
        let portal = both_present(caps::KEYBOARD, caps::WANTED);
        let caps = probe(&portal, SessionKind::Wayland).capabilities(Gate::On);
        assert!(!caps.capture, "no pointer to capture means no source half");
        assert!(caps.inject_keys, "and the target half is untouched by it");
    }

    /// A version below the floor is its own answer, because its remedy is its own:
    /// update the portal, do not install one and do not allow a dialog.
    #[test]
    fn a_portal_older_than_this_build_says_so_rather_than_saying_it_is_absent() {
        let portal = FakePortal::with(&[
            (INPUT_CAPTURE, "version", Ok(0)),
            (INPUT_CAPTURE, "SupportedCapabilities", Ok(caps::WANTED)),
            (REMOTE_DESKTOP, "version", Ok(0)),
            (REMOTE_DESKTOP, "AvailableDeviceTypes", Ok(caps::WANTED)),
        ]);
        let caps = probe(&portal, SessionKind::Wayland).capabilities(Gate::On);
        assert_eq!(caps.problem, Some(Problem::WaylandPortalOld));
        assert!(!caps.capture && !caps.inject_keys);
    }

    /// No session bus at all outranks everything: telling somebody to allow a
    /// dialog on a machine with no bus to carry it is advice that cannot work.
    #[test]
    fn no_session_bus_outranks_every_other_reason() {
        let bus = PortalError::NoBus("no DBUS_SESSION_BUS_ADDRESS".into());
        let report = Report {
            session: SessionKind::Wayland,
            capture: Offer::Missing(bus.clone()),
            inject: Offer::Missing(bus),
        };
        assert_eq!(
            report.capabilities(Gate::On).problem,
            Some(Problem::WaylandNoBus)
        );

        // And the ranking itself, pair by pair, so the order is a fact rather than
        // an accident of which arm the match tried first.
        assert_eq!(
            worse(Problem::WaylandNoPortal, Problem::WaylandNoBus),
            Problem::WaylandNoBus
        );
        assert_eq!(
            worse(Problem::WaylandPortalOld, Problem::WaylandNoPortal),
            Problem::WaylandNoPortal
        );
        assert_eq!(
            worse(Problem::WaylandUntested, Problem::WaylandPortalRefused),
            Problem::WaylandPortalRefused
        );
    }

    /// **The trap: asking for a property of an absent interface answers
    /// `InvalidArgs`, not `UnknownInterface`.**
    ///
    /// Measured on this repository's own session bus against
    /// `xdg-desktop-portal` 1.18.4. A classifier keyed on the error name alone
    /// reads that as a generic failure, reports `wayland_portal_refused`, and tells
    /// a person with no portal at all to try allowing the dialog again, for ever.
    #[test]
    fn an_absent_interface_is_recognised_through_every_name_the_bus_uses_for_it() {
        let iface = INPUT_CAPTURE;
        let absent = PortalError::NoInterface {
            interface: iface.to_string(),
        };

        // The two real measurements, verbatim.
        assert_eq!(
            classify(
                dbus_errors::INVALID_ARGS,
                "No such interface \u{201c}org.freedesktop.portal.InputCapture\u{201d}",
                iface
            ),
            absent,
            "Properties.Get on an absent interface, as this machine's bus answers it"
        );
        assert_eq!(
            classify(
                dbus_errors::UNKNOWN_METHOD,
                "No such interface \u{201c}org.freedesktop.portal.InputCapture\u{201d} \
                 on object at path /org/freedesktop/portal/desktop",
                iface
            ),
            absent,
            "a method call on an absent interface, as this machine's bus answers it"
        );
        // And the names a reading of the specification would have predicted, which
        // some other implementation may well use.
        for name in [
            dbus_errors::UNKNOWN_INTERFACE,
            dbus_errors::UNKNOWN_OBJECT,
            dbus_errors::UNKNOWN_PROPERTY,
        ] {
            assert_eq!(classify(name, "", iface), absent, "{name}");
        }
        // An unknown name whose MESSAGE says the interface is absent is believed:
        // the rule survives a spelling this build has never seen.
        assert_eq!(
            classify("org.example.Whatever", "No such interface", iface),
            absent
        );

        // What must NOT collapse into it. A method missing from an interface that
        // IS there is a portal to update, not one to install.
        assert_eq!(
            classify(
                dbus_errors::UNKNOWN_METHOD,
                "No such method CreateSession2",
                iface
            ),
            PortalError::NoMethod {
                interface: iface.to_string(),
                method: "No such method CreateSession2".to_string(),
            }
        );
        assert_eq!(
            classify(
                dbus_errors::SERVICE_UNKNOWN,
                "not provided by any .service files",
                iface
            ),
            PortalError::NoPortal("not provided by any .service files".to_string())
        );
        assert_eq!(
            classify(dbus_errors::DISCONNECTED, "bus is gone", iface),
            PortalError::NoBus("bus is gone".to_string())
        );
        assert_eq!(
            classify(dbus_errors::NO_REPLY, "", iface),
            PortalError::Timeout
        );
        assert_eq!(
            classify(dbus_errors::ACCESS_DENIED, "no", iface),
            PortalError::Refused { cancelled: false }
        );
        assert_eq!(
            classify(
                "org.freedesktop.portal.Error.Failed",
                "EIS fd is not ready",
                iface
            ),
            PortalError::Failed {
                name: "org.freedesktop.portal.Error.Failed".to_string(),
                message: "EIS fd is not ready".to_string(),
            },
            "a compositor's own vocabulary is kept whole for the bug report"
        );
    }

    /// Every failure knows whether asking again could help, and every one of them
    /// has a reason code and a prose line. A missing piece re-asked once per
    /// session start is a log filling up with the same sentence.
    #[test]
    fn every_failure_says_whether_asking_again_is_worth_anything() {
        let retry = [
            PortalError::Refused { cancelled: true },
            PortalError::Refused { cancelled: false },
            PortalError::SessionClosed,
            PortalError::Failed {
                name: "n".into(),
                message: "m".into(),
            },
            PortalError::Timeout,
        ];
        let hopeless = [
            PortalError::NoBus("x".into()),
            PortalError::NoPortal("x".into()),
            PortalError::NoInterface {
                interface: INPUT_CAPTURE.into(),
            },
            PortalError::NoMethod {
                interface: INPUT_CAPTURE.into(),
                method: "m".into(),
            },
            PortalError::TooOld {
                interface: INPUT_CAPTURE.into(),
                version: 0,
            },
            PortalError::Malformed("shape".into()),
        ];
        for e in &retry {
            assert!(e.worth_retrying(), "{e:?}");
        }
        for e in &hopeless {
            assert!(!e.worth_retrying(), "{e:?}");
        }
        for e in retry.iter().chain(hopeless.iter()) {
            assert!(!e.to_string().is_empty(), "{e:?} has no prose");
            let problem = e.problem();
            assert!(
                Problem::ALL.contains(&problem),
                "{e:?} maps to a problem this build does not know"
            );
        }
    }

    /// A `Request`'s response code, and the fail-closed reading of one this build
    /// does not know: a session that never starts costs a sentence, a session that
    /// starts on a maybe swallows somebody's keyboard.
    #[test]
    fn only_a_zero_response_is_a_yes() {
        assert_eq!(response(0), Ok(()));
        assert_eq!(response(1), Err(PortalError::Refused { cancelled: true }));
        assert_eq!(response(2), Err(PortalError::Refused { cancelled: false }));
        for code in [3, 42, u32::MAX] {
            assert_eq!(
                response(code),
                Err(PortalError::Refused { cancelled: false }),
                "a response code this build does not know is not a yes"
            );
        }
    }

    /// A version that reads and capabilities that do not is an interface that
    /// exists and offers nothing usable, which is a different sentence from one that
    /// does not exist. It must not read as usable either way.
    #[test]
    fn an_interface_whose_capabilities_cannot_be_read_offers_nothing() {
        let portal = FakePortal::with(&[
            (INPUT_CAPTURE, "version", Ok(1)),
            (
                INPUT_CAPTURE,
                "SupportedCapabilities",
                Err(PortalError::Failed {
                    name: "org.freedesktop.DBus.Error.Failed".into(),
                    message: "no".into(),
                }),
            ),
        ]);
        let offer = offer_of(&portal, INPUT_CAPTURE, "SupportedCapabilities");
        assert_eq!(
            offer,
            Offer::Present {
                version: 1,
                capabilities: 0
            }
        );
        assert!(!offer.usable(INPUT_CAPTURE_MIN, caps::WANTED));
        assert_eq!(offer.granted(caps::WANTED), 0);
    }

    /// A capability bit this build does not know is ignored, which the interface
    /// requires by name, rather than making the whole answer unreadable.
    #[test]
    fn an_unknown_capability_bit_is_ignored_and_the_known_ones_still_count() {
        let future = caps::WANTED | 0x8000_0000;
        let portal = both_present(future, future);
        let caps = probe(&portal, SessionKind::Wayland).capabilities(Gate::On);
        assert!(caps.capture && caps.inject_keys);
        assert_eq!(caps.problem, None);
    }
}
