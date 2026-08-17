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
use crate::keys;
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
    // **Only the one phrase**, and the second test this used to have was a real bug a
    // review found. It was "the message names the interface AND says `No such`", which
    // QtDBus satisfies for a method that is MISSING from an interface that is THERE:
    // its wording is `No such method 'X' in interface 'Y' at object path '...'`. KDE's
    // portal is Qt, so on Plasma a portal too old for a call would have been reported as
    // a portal that does not exist, and its owner told to install what they already had
    // instead of to update it. GDBus names only the member, libdbus says "doesn't
    // exist", and sd-bus says "Unknown method X or interface Y": none of the three needs
    // the looser test, and Qt is broken by it.
    let absent_interface = message.contains("No such interface");
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
    ///
    /// The interface's own name is passed in rather than held on the [`Offer`],
    /// because an `Offer` is one answer about one question and duplicating the name
    /// in it would mean two places for it to be wrong. It matters: the name is what
    /// the log line says, and "is at version 0, which is too old" with nothing in
    /// front of it names neither half of a desktop.
    pub fn why(&self, interface: &str, min_version: u32, wanted: u32) -> Option<PortalError> {
        match self {
            Offer::Missing(e) => Some(e.clone()),
            Offer::Present {
                version,
                capabilities,
            } => {
                if *version < min_version {
                    Some(PortalError::TooOld {
                        interface: interface.to_string(),
                        version: *version,
                    })
                } else if capabilities & wanted == 0 {
                    // Present, current, and offering nothing this engine can use:
                    // a touchscreen-only capture portal is the real shape of it.
                    // "Not there" is the honest word from where a person sits.
                    Some(PortalError::NoInterface {
                        interface: interface.to_string(),
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
    /// - **`monitors_stable` is `false`, always**, and the first version had it `true`
    ///   on the strength of the zone-set serial. A review took that apart and it was
    ///   right to: the bit means "monitor identities survive an unplug", and a zone
    ///   carries no name, no EDID and no identity of any kind, so [`monitors_from`]
    ///   synthesises one from the size and the position. Unplug a 1920x1080 screen and
    ///   plug a DIFFERENT 1920x1080 screen into the same place and the identity is
    ///   byte-for-byte the same: the plane silently puts a different screen where the
    ///   old one was, which is the exact outcome the bit exists to warn about. The X11
    ///   backend computes its own from whether an EDID could be read; this one cannot
    ///   read anything, so it says so. `xdg-output` is the way out and it is on the
    ///   deferred list.
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
            monitors_stable: false,
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
        let capture_why = self
            .capture
            .why(INPUT_CAPTURE, INPUT_CAPTURE_MIN, caps::WANTED);
        let inject_why = self
            .inject
            .why(REMOTE_DESKTOP, REMOTE_DESKTOP_MIN, caps::WANTED);
        match (capture_why, inject_why) {
            // Everything is there. Two truths are left, in order: whether this has ever
            // been run, and, once it has, that the screens of a Wayland session cannot
            // be told apart (see [`Report::capabilities`] on `monitors_stable`).
            // `monitors_stable` reaches no part of the interface on its own, so if this
            // slot does not say it, nobody is ever told.
            (None, None) => Some(if gate == Gate::Off {
                Problem::WaylandUntested
            } else {
                Problem::MonitorsUnstable
            }),
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

    // ---- the capture half: org.freedesktop.portal.InputCapture --------------

    /// `CreateSession`, asking for `want` (a mask of [`caps`]).
    ///
    /// **This is the call that raises the consent dialog** on a version 1 portal,
    /// which is every portal that exists on a released desktop today, so it is
    /// never made at start-up: it happens when a person switches the feature on.
    /// The answer's capabilities are always a SUBSET of what was asked for, so they
    /// are read back rather than assumed.
    fn capture_create_session(&self, want: u32) -> Result<CaptureGrant, PortalError>;

    /// `GetZones`. The screens, in the portal's own coordinate space, with the
    /// serial that makes barriers addressable.
    fn capture_zones(&self, session: &SessionId) -> Result<ZoneSet, PortalError>;

    /// `SetPointerBarriers`. Answers the ids the compositor REFUSED, which is a
    /// per-barrier answer and not a failure of the call.
    fn capture_set_barriers(
        &self,
        session: &SessionId,
        serial: u32,
        barriers: &[Barrier],
    ) -> Result<Vec<u32>, PortalError>;

    /// `ConnectToEIS`, and then the EI handshake, answering the live event stream.
    /// Must happen before `Enable`.
    fn capture_connect(&self, session: &SessionId) -> Result<Box<dyn EiSource>, PortalError>;

    /// `Enable`: from here the compositor watches the barriers.
    fn capture_enable(&self, session: &SessionId) -> Result<(), PortalError>;

    /// `Disable`: stop watching. Emits no signal of its own.
    fn capture_disable(&self, session: &SessionId) -> Result<(), PortalError>;

    /// `Release`: end an ACTIVE capture, suggesting where to leave the pointer.
    /// The suggestion is advisory and the compositor may ignore it.
    fn capture_release(
        &self,
        session: &SessionId,
        activation: u32,
        cursor: Option<(f64, f64)>,
    ) -> Result<(), PortalError>;

    // ---- the injection half: org.freedesktop.portal.RemoteDesktop -----------

    /// `CreateSession` then `SelectDevices(types)` then `Start`, answering the
    /// device types actually granted. Three calls behind one seam method because
    /// they are one gesture from a person's point of view and there is nothing
    /// useful to do between them.
    ///
    /// **`Start` is what raises the dialog here**, so this too is never called at
    /// start-up.
    fn remote_start(&self, want: u32) -> Result<RemoteGrant, PortalError>;

    /// One `Notify*` call.
    ///
    /// Fire and forget from the engine's point of view (the seam's `inject` is),
    /// and it answers here because the transport needs to know when the session has
    /// died: a target that keeps writing into a closed session types nothing and
    /// says nothing, which is the failure this whole component exists to avoid.
    fn remote_notify(&self, session: &SessionId, what: &Notify) -> Result<(), PortalError>;

    // ---- both halves -------------------------------------------------------

    /// `org.freedesktop.portal.Session.Close`. Best effort by construction: it is
    /// called from every teardown path including the ones that happen because
    /// something is already wrong, so it reports nothing and must never block for
    /// long. A client vanishing from the bus closes its sessions anyway, which is
    /// the backstop when this call cannot get through.
    fn close(&self, session: &SessionId);

    /// Whatever the portal has said since it was last asked. Never blocks.
    fn take_signals(&self) -> Vec<Signal>;
}

/// A capture session the portal granted, with what it granted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureGrant {
    pub session: SessionId,
    /// Always a subset of what was asked for.
    pub capabilities: u32,
}

/// An injection session the portal granted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteGrant {
    pub session: SessionId,
    /// The device types `Start` answered with, and not the ones requested.
    pub devices: u32,
}

/// A portal session handle. Opaque above the transport.
///
/// A `String` and not an object path type, because `RemoteDesktop.CreateSession`
/// answers its handle as a plain string rather than an object path: the interface
/// says so out loud ("erroneously implemented as `s`. For backwards compatibility
/// it will remain this type"), and a seam typed as an object path would have to
/// convert on one side and not the other.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionId(pub String);

/// What the portal said, unprompted.
///
/// `PartialEq` and not `Eq`: the cursor position is a pair of floats, because that
/// is what the interface carries.
#[derive(Clone, Debug, PartialEq)]
pub enum Signal {
    /// The pointer crossed a barrier and this capture is now live.
    Activated {
        session: SessionId,
        activation: u32,
        /// Where the compositor says the pointer is, in the zones' space.
        /// **Regularly outside the zones**: barriers sit at their edges and a fast
        /// movement lands past them, and the interface says adjusting it is the
        /// application's job. [`clamp_into`] is that adjustment.
        cursor: Option<(f64, f64)>,
        /// Which barrier. `Some(0)` means the compositor could not tell, `None`
        /// means this activation was not a barrier crossing at all.
        barrier: Option<u32>,
    },
    /// This capture is no longer live. The barriers are still armed.
    Deactivated { session: SessionId, activation: u32 },
    /// Nothing more will be captured until `Enable` is called again.
    Disabled { session: SessionId },
    /// The screens moved, so every barrier is stale.
    ZonesChanged { session: SessionId, serial: u32 },
    /// The session is over.
    Closed { session: SessionId },
}

/// The live event stream of a capture session.
///
/// Its own seam rather than a method on [`Portal`], because it is a different
/// protocol on a different socket with a different failure mode, and because it is
/// the piece that has to be replaced by a scripted double for the state machine to
/// be testable at all.
///
/// **Neither `Send` nor `Sync`, and that is a fact about the protocol library rather
/// than a convenience.** The pure-Rust EI implementation's event converter holds
/// boxed callbacks with no `Send` bound, so it cannot leave the thread it was built
/// on. Requiring `Send` here would therefore be a bound no real implementation can
/// satisfy, and the tempting workaround (a thread of its own, feeding a channel) is
/// worse than useless: the thread would have to build the converter itself, so the
/// file descriptor and the handshake would move there too, and the session lifecycle
/// would be split across two threads for nothing. It lives where the backend's loop
/// lives, which is where every other platform backend keeps its OS state.
pub trait EiSource {
    /// Whatever arrived, waiting at most `budget`.
    fn poll(&mut self, budget: std::time::Duration) -> EiPoll;
}

/// One turn of the EI stream.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EiPoll {
    pub events: Vec<EiEvent>,
    /// The far end is gone. Every session using this stream ends.
    pub gone: bool,
}

/// What arrives over the EI socket, reduced to what this engine consumes.
///
/// This engine's own vocabulary and not the EI crate's, for the reason every seam
/// in this file exists: the decoding below ([`Decoder`]) is then a pure function
/// over a type a test can construct, and the EI library appears in exactly one
/// file. It also means the numbers are pinned here rather than in a dependency:
/// buttons and keys are Linux evdev codes, deltas are the compositor's own
/// accelerated movement, and the ONE thing that must be threaded through is
/// [`EiEvent::StartEmulating`]'s sequence.
#[derive(Clone, Debug, PartialEq)]
pub enum EiEvent {
    /// The compositor is about to send events for this activation. Its `sequence`
    /// is the same number the portal's `Activated` carried as `activation_id`, and
    /// checking them against each other is what stops a previous crossing's stale
    /// movement from landing on this one.
    StartEmulating { sequence: u32 },
    /// This activation's events are finished.
    StopEmulating,
    /// Relative movement, in the compositor's logical pixels. `f32` because that
    /// is what the protocol carries, and the fraction matters: see
    /// [`Decoder::motion`].
    Motion { dx: f32, dy: f32 },
    /// A button, as a Linux evdev code (`BTN_LEFT` is 272).
    Button { button: u32, down: bool },
    /// A smooth scroll, in logical pixels.
    ScrollDelta { dx: f32, dy: f32 },
    /// A stepped scroll, in [`SCROLL_STEP`] units per notch.
    ScrollDiscrete { dx: i32, dy: i32 },
    /// A key, as a Linux evdev keycode (`KEY_A` is 30).
    Key { keycode: u32, down: bool },
    /// The XKB modifier masks the compositor says are in effect.
    Modifiers {
        depressed: u32,
        latched: u32,
        locked: u32,
    },
}

// ------------------------------------------------------ zones, and the barriers

/// One screen, as `GetZones` reports it.
///
/// The portal's own shape is `(width u, height u, x_off i, y_off i)`: unsigned
/// sizes, signed offsets, and no name, no scale and no identity of any kind. What
/// that costs is written up on [`monitors_from`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Zone {
    pub w: u32,
    pub h: u32,
    pub x: i32,
    pub y: i32,
}

impl Zone {
    /// The half-open horizontal span, `x .. x + w`.
    fn xs(&self) -> (i64, i64) {
        (self.x as i64, self.x as i64 + self.w as i64)
    }

    /// The half-open vertical span, `y .. y + h`.
    fn ys(&self) -> (i64, i64) {
        (self.y as i64, self.y as i64 + self.h as i64)
    }

    fn empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }

    /// Is this row of pixels inside this zone?
    fn covers_row(&self, y: i64) -> bool {
        let (y0, y1) = self.ys();
        y0 <= y && y < y1
    }

    /// Is this column of pixels inside this zone?
    fn covers_column(&self, x: i64) -> bool {
        let (x0, x1) = self.xs();
        x0 <= x && x < x1
    }
}

/// The zones and the serial that addresses them.
///
/// The serial is not decoration: `SetPointerBarriers` carries it and the compositor
/// refuses barriers whose serial is not the one it last handed out, which is what
/// stops a barrier being placed on a screen that has since moved.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ZoneSet {
    pub zones: Vec<Zone>,
    pub serial: u32,
}

/// One pointer barrier, in the zones' coordinate space.
///
/// Horizontal means `y1 == y2` and vertical means `x1 == x2`; the interface
/// supports nothing else, so a diagonal is not a barrier the compositor will
/// refuse, it is a barrier this code must never build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Barrier {
    /// Non-zero, and unique within the set. Zero is reserved: an `Activated`
    /// carrying `barrier_id: 0` means the compositor could not say which barrier
    /// it was.
    pub id: u32,
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
}

/// Which edge of this machine's desktop a barrier sits on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Top,
    Bottom,
    Left,
    Right,
}

/// A barrier and what it means, kept so an `Activated` can be turned back into an
/// edge the engine understands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placed {
    pub barrier: Barrier,
    pub side: Side,
    /// Index into the [`ZoneSet`] the barrier was computed from.
    pub zone: usize,
}

/// The barriers for a set of zones: every stretch of the outside boundary of their
/// union, and nothing else.
///
/// # The two rules that decide the whole shape of this
///
/// From the interface, and both are hard: "A pointer barrier must be situated at
/// the outside boundary of the union of all zones", and "A pointer barrier must be
/// fully contained within one zone". So a barrier is per zone AND per edge AND
/// clipped to the parts of that edge with nothing of this machine's own across
/// them. Two screens side by side get six barriers and not eight: the edge they
/// share is inside the union, and a barrier there would fire every time the pointer
/// moved from one of this machine's screens to the other.
///
/// The clipping is not a nicety. An L-shaped arrangement (a laptop screen below the
/// right half of an external one) has an edge that is PARTLY shared, so one edge
/// becomes one barrier over the exposed part and none over the rest. Emitting the
/// whole edge would have the compositor take the pointer away in the middle of this
/// machine's own desktop, and emitting nothing would swallow the pointer at a real
/// boundary.
///
/// The test for "is there anything across this boundary" is about the PIXEL ACROSS it,
/// not about a neighbour's edge meeting this one. That distinction was got wrong first
/// time and a review caught it: an exact-abutment test is blind to zones that OVERLAP,
/// and a compositor reports overlapping zones for real (one pixel of fractional-scale
/// rounding is enough, a mirrored pair is reported twice, and a small screen can sit
/// entirely inside a large one). Each of those produced barriers in the interior of the
/// union, which is either a pointer taken away mid-desktop or, on a compositor that
/// validates, every barrier refused and the session dying with "no pointer barrier
/// could be placed".
///
/// # Why every edge, and not only the edges that lead somewhere
///
/// The engine knows which edges of the plane have a neighbour across them and this
/// function does not ask, and that is deliberate. Handing that knowledge down would
/// mean a new downcall on [`crate::backend::InputBackend`], which is a frozen seam
/// four backends and a fake implement, for the benefit of one of them. Instead every
/// outer edge is armed and the engine's own crossing graph decides: an activation on
/// an edge that leads nowhere is released at once, by the code that already exists
/// for it.
///
/// What that costs, honestly: the compositor briefly takes the pointer at a dead
/// edge, so the pointer stops for as long as the round trip takes. Whether that is
/// perceptible is a question for a real desk, and it is on the deferred list. The
/// fix, if it is, is the extra downcall.
///
/// # The pixel convention
///
/// A horizontal barrier sits on the TOP edge of its pixels and a vertical one on
/// the LEFT edge, each inclusive of the pixel's width or height, so `x1 = 0,
/// x2 = 1` is two pixels wide. That is why a zone's top edge is at `y` and its
/// bottom edge is at `y + h`: the bottom boundary is the top edge of the first row
/// that is NOT in the zone, which is exactly the line the pointer crosses on its
/// way out.
///
/// **This reading is from the interface's prose and has never been checked against a
/// compositor**, and it is the largest single assumption in this file, so it is named
/// rather than asserted. It puts the bottom and right barriers one pixel outside the
/// zone's own pixel rectangle, and a compositor that reads "fully contained within one
/// zone" as containment of the barrier's own pixels would refuse all four barriers of a
/// one-screen desktop. The symptom would be unmistakable and is worth knowing in
/// advance: `rearm_barriers` dies with "no pointer barrier could be placed on this
/// desktop" everywhere, on every compositor. The remedy is `y + h - 1` and `x + w - 1`
/// here, and nothing else changes. It is on the deferred list under its own name.
pub fn barriers_for(zones: &[Zone]) -> Vec<Placed> {
    let mut out = Vec::new();
    let mut next_id: u32 = 1;
    for (index, zone) in zones.iter().enumerate() {
        if zone.empty() {
            // A zone of no size has no boundary to leave through, and a barrier on
            // it would be a barrier the compositor refuses. Skipped rather than
            // reported: an empty zone is the compositor's business.
            continue;
        }
        let (zx0, zx1) = zone.xs();
        let (zy0, zy1) = zone.ys();
        for side in [Side::Top, Side::Bottom, Side::Left, Side::Right] {
            // The span of the edge, and the spans of it where the pixel ACROSS the
            // boundary belongs to another of this machine's own screens.
            //
            // **Across, not abutting**, and a review is why. The first version asked
            // whether a neighbour's edge met this one exactly, which is blind to any
            // overlap: two zones overlapping by one pixel (a fractional-scale rounding
            // artefact is enough), a zone entirely inside another, and a mirrored pair
            // reported twice each produced barriers in the MIDDLE of the union, where
            // the pointer is taken away in the middle of somebody's desktop. Asking
            // about the pixel across the boundary IS the definition of "is this boundary
            // on the outside of the union", and it answers all four cases at once.
            let (span, covered): ((i64, i64), Vec<(i64, i64)>) = match side {
                Side::Top => (
                    (zx0, zx1),
                    neighbours(zones, index, |n| n.covers_row(zy0 - 1), |n| n.xs()),
                ),
                Side::Bottom => (
                    (zx0, zx1),
                    neighbours(zones, index, |n| n.covers_row(zy1), |n| n.xs()),
                ),
                Side::Left => (
                    (zy0, zy1),
                    neighbours(zones, index, |n| n.covers_column(zx0 - 1), |n| n.ys()),
                ),
                Side::Right => (
                    (zy0, zy1),
                    neighbours(zones, index, |n| n.covers_column(zx1), |n| n.ys()),
                ),
            };
            for (a, b) in subtract(span, &covered) {
                // Half-open in, inclusive out: the last pixel of the span is
                // `b - 1`, and a barrier's second coordinate names a pixel.
                let (a, b) = (clamp_i32(a), clamp_i32(b - 1));
                let barrier = match side {
                    Side::Top => Barrier {
                        id: next_id,
                        x1: a,
                        y1: zone.y,
                        x2: b,
                        y2: zone.y,
                    },
                    Side::Bottom => Barrier {
                        id: next_id,
                        x1: a,
                        y1: clamp_i32(zy1),
                        x2: b,
                        y2: clamp_i32(zy1),
                    },
                    Side::Left => Barrier {
                        id: next_id,
                        x1: zone.x,
                        y1: a,
                        x2: zone.x,
                        y2: b,
                    },
                    Side::Right => Barrier {
                        id: next_id,
                        x1: clamp_i32(zx1),
                        y1: a,
                        x2: clamp_i32(zx1),
                        y2: b,
                    },
                };
                // Two zones reported identically (a mirrored pair, which a compositor
                // really does report twice) would otherwise put two barriers on one
                // line with two ids: legal, wasteful, and an `Activated` could then
                // name either of them.
                if out.iter().any(|p: &Placed| {
                    let b = p.barrier;
                    (b.x1, b.y1, b.x2, b.y2) == (barrier.x1, barrier.y1, barrier.x2, barrier.y2)
                }) {
                    continue;
                }
                next_id += 1;
                out.push(Placed {
                    barrier,
                    side,
                    zone: index,
                });
            }
        }
    }
    out
}

/// The spans of the other zones that touch this edge, as picked out by `touches`
/// and measured by `span`.
fn neighbours(
    zones: &[Zone],
    skip: usize,
    touches: impl Fn(&Zone) -> bool,
    span: impl Fn(&Zone) -> (i64, i64),
) -> Vec<(i64, i64)> {
    zones
        .iter()
        .enumerate()
        .filter(|(i, n)| *i != skip && !n.empty() && touches(n))
        .map(|(_, n)| span(n))
        .collect()
}

/// `span` minus every range in `cover`, as the ranges that are left, in order.
///
/// Half-open throughout. The ranges in `cover` may overlap each other and need not
/// be sorted, which is why they are sorted here rather than assumed: a compositor
/// lists its zones in whatever order it pleases.
fn subtract(span: (i64, i64), cover: &[(i64, i64)]) -> Vec<(i64, i64)> {
    let (start, end) = span;
    if start >= end {
        return Vec::new();
    }
    let mut cuts: Vec<(i64, i64)> = cover
        .iter()
        .copied()
        .map(|(a, b)| (a.max(start), b.min(end)))
        .filter(|(a, b)| a < b)
        .collect();
    cuts.sort_unstable();
    let mut out = Vec::new();
    let mut at = start;
    for (a, b) in cuts {
        if a > at {
            out.push((at, a));
        }
        at = at.max(b);
        if at >= end {
            return out;
        }
    }
    if at < end {
        out.push((at, end));
    }
    out
}

/// A zone coordinate as an `i32`, saturating.
///
/// The arithmetic above is in `i64` on purpose: a zone's size is a `u32` and its
/// offset an `i32`, so `x + w` overflows `i32` for a compositor reporting something
/// absurd, and an overflow here would be a barrier at a position nobody asked for
/// (or a panic in a debug build). Saturating is the honest end of it: a barrier
/// clamped to the edge of the coordinate space is refused by the compositor and
/// reported as a failed barrier, which is a path that already exists.
fn clamp_i32(v: i64) -> i32 {
    v.clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

/// Pulls a point back inside the zones, for an `Activated` whose cursor position is
/// past the barrier it crossed.
///
/// The interface warns about this in as many words: the position is "usually
/// outside the Zones", because the barriers are at their edges and a fast movement
/// carries the pointer past one before the compositor stops it, and "it is the
/// application's responsibility to calculate and adjust the cursor position".
///
/// Adjusted to the NEAREST point of the nearest zone, by squared distance, so a
/// pointer that left the top right corner comes back to that corner rather than to
/// the middle of a screen. `None` when there are no zones to come back to, which is
/// a compositor that reported an empty screen set and where nothing can be placed
/// anyway.
pub fn clamp_into(zones: &[Zone], at: (f64, f64)) -> Option<(f64, f64)> {
    let mut best: Option<((f64, f64), f64)> = None;
    for zone in zones.iter().filter(|z| !z.empty()) {
        let (x0, x1) = zone.xs();
        let (y0, y1) = zone.ys();
        // The last addressable pixel, not the boundary: a position at `x + w` is
        // already off this screen.
        let x = at.0.clamp(x0 as f64, (x1 - 1) as f64);
        let y = at.1.clamp(y0 as f64, (y1 - 1) as f64);
        let d = (at.0 - x).powi(2) + (at.1 - y).powi(2);
        if best.is_none_or(|(_, bd)| d < bd) {
            best = Some(((x, y), d));
        }
    }
    best.map(|(p, _)| p)
}

/// The zones, as the layout document's monitors.
///
/// # What a zone does not carry, and what that costs
///
/// A name, a scale, and above all an IDENTITY. The interface answers
/// `(width, height, x_offset, y_offset)` and nothing else, so:
///
/// - the identity is synthesised from the position and size, which is stable while
///   the arrangement is stable and changes the moment a screen moves. That is the
///   opposite of what [`crate::backend::Monitor::id`] asks for, and it is the
///   honest best available: a monitor whose identity moved shows up as a monitor
///   that left and one that arrived, so the plane loses that screen's place rather
///   than silently putting a different screen there. Losing a place is visible;
///   swapping two screens is not.
/// - the name is the size, because a person needs something to read on the plane
///   and "1920x1080 at 0,0" is at least true.
/// - the scale is 1000 (100%), because the interface says nothing about scale. The
///   zones are in the compositor's logical space, which is the space this engine
///   works in, so the number is right for the arithmetic and wrong as a
///   description of a HiDPI screen.
/// - the primary screen is the one at the origin, or failing that the first, since
///   nothing in the interface says which it is. Exactly one is marked, because the
///   plane needs an anchor.
///
/// The remedy for all four is `xdg-output`, a Wayland protocol which does carry the
/// name and the scale, and reaching it means a Wayland connection of this component's
/// own. That is a brick of its own and it is on the deferred list rather than guessed
/// at here.
pub fn monitors_from(zones: &[Zone]) -> Vec<crate::backend::Monitor> {
    let usable: Vec<&Zone> = zones.iter().filter(|z| !z.empty()).collect();
    let primary = usable
        .iter()
        .position(|z| z.x == 0 && z.y == 0)
        .unwrap_or(0);
    usable
        .iter()
        .enumerate()
        .map(|(i, z)| crate::backend::Monitor {
            id: format!("wayland:zone:{}x{}+{}+{}", z.w, z.h, z.x, z.y),
            name: format!("{}x{} at {},{}", z.w, z.h, z.x, z.y),
            w: clamp_i32(z.w as i64),
            h: clamp_i32(z.h as i64),
            x: z.x,
            y: z.y,
            scale: 1000,
            primary: i == primary,
        })
        .collect()
}

/// Warnings go to stderr, which the supervisor captures, exactly as the other
/// platform backends do.
pub(crate) fn warn(what: &str) {
    eprintln!("[1device-input] wayland: {what}");
}

// ------------------------------------------------- typing: HID to evdev, and back

/// One `RemoteDesktop` `Notify*` call, with its arguments in the interface's own
/// types.
///
/// The types are the D-Bus signatures verbatim (`i` for a button, a keycode and a
/// keysym, `u` for a state and an axis, `d` for a delta) so that the transport is a
/// transcription and never a conversion. A signed keycode looks wrong and is what
/// the interface asks for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Notify {
    /// `NotifyPointerMotion(d dx, d dy)`. Relative, and the only pointer motion a
    /// session without a paired screen cast can make.
    PointerMotion { dx: f64, dy: f64 },
    /// `NotifyPointerButton(i button, u state)`. The button is a Linux evdev code,
    /// which the interface says out loud.
    PointerButton { button: i32, pressed: bool },
    /// `NotifyPointerAxis(d dx, d dy)`, the smooth scroll.
    PointerAxis { dx: f64, dy: f64 },
    /// `NotifyPointerAxisDiscrete(u axis, i steps)`, the stepped scroll. Axis 0 is
    /// vertical and 1 is horizontal.
    PointerAxisDiscrete { axis: u32, steps: i32 },
    /// `NotifyKeyboardKeycode(i keycode, u state)`. Linux evdev, NOT X11: they
    /// differ by 8 and getting it wrong types a key eight positions along.
    KeyboardKeycode { keycode: i32, pressed: bool },
    /// `NotifyKeyboardKeysym(i keysym, u state)`. An X keysym, which is how any
    /// character at all is typed without a keymap.
    KeyboardKeysym { keysym: i32, pressed: bool },
}

/// The vertical axis of `NotifyPointerAxisDiscrete`.
pub const AXIS_VERTICAL: u32 = 0;
/// The horizontal axis.
pub const AXIS_HORIZONTAL: u32 = 1;

/// One notch of a stepped scroll, in the units EI's `scroll_discrete` carries.
///
/// **From the protocol's own description and not from a run**: libei describes the
/// discrete scroll value as a multiple of 120, where 120 is one logical click, which
/// is the convention Windows started with `WHEEL_DELTA` and the one
/// `wl_pointer.axis_value120` uses. It is named here as one constant rather than
/// spelled at the point of use precisely because it is unverified: if a real desk
/// shows a mouse wheel scrolling a hundred and twenty times too far, this is the
/// line to change, and it is on the deferred list for that ticket.
pub const SCROLL_STEP: i32 = 120;

/// The evdev button codes, so the numbers appear once.
mod btn {
    pub const LEFT: u32 = 272;
    pub const RIGHT: u32 = 273;
    pub const MIDDLE: u32 = 274;
    /// The near thumb button. What a mouse actually reports for "back", and what
    /// X11 presents as button 8.
    pub const SIDE: u32 = 275;
    /// The far thumb button, X11's button 9.
    pub const EXTRA: u32 = 276;
    /// The codes a keyboard-shaped device may use for the same two, which some
    /// hardware sends instead.
    pub const BACK: u32 = 278;
    pub const FORWARD: u32 = 279;
}

/// This engine's button number as an evdev code, or `None` for a number this
/// platform has nothing for.
///
/// The engine's numbering is closed (1 left, 2 middle, 3 right, 4 back, 5 forward,
/// and the wire refuses anything else), so the table is five entries and a `None`
/// here is a bug in the caller rather than a peer's doing.
///
/// Back and forward go to `BTN_SIDE` and `BTN_EXTRA` rather than to `BTN_BACK` and
/// `BTN_FORWARD`, and that is the pragmatic choice rather than the literal one:
/// those are the codes real mice send for their thumb buttons and the codes X11
/// presents as buttons 8 and 9, so they are what a desktop's own shortcuts are bound
/// to. The literal pair is accepted on the way IN ([`button_from_evdev`]) so that a
/// device sending it is not ignored.
pub fn button_to_evdev(button: u8) -> Option<u32> {
    match button {
        1 => Some(btn::LEFT),
        2 => Some(btn::MIDDLE),
        3 => Some(btn::RIGHT),
        4 => Some(btn::SIDE),
        5 => Some(btn::EXTRA),
        _ => None,
    }
}

/// An evdev button code as this engine's button number, or `None` for a button this
/// engine does not carry (a tablet stylus, a joystick trigger, the eight further
/// mouse buttons a gaming mouse has).
///
/// Dropping one is the right answer rather than passing an invented number: the
/// receiving side checks the number against the same closed set as it leaves the
/// wire, so an invented one would be refused there anyway, one hop later and with
/// less context.
pub fn button_from_evdev(code: u32) -> Option<u8> {
    match code {
        btn::LEFT => Some(1),
        btn::MIDDLE => Some(2),
        btn::RIGHT => Some(3),
        btn::SIDE | btn::BACK => Some(4),
        btn::EXTRA | btn::FORWARD => Some(5),
        _ => None,
    }
}

/// The Linux evdev keycode for a wire usage, or `None` when this build has none.
///
/// # There is ONE table and it lives in the X11 backend
///
/// A Wayland target is typed on by evdev keycode (`RemoteDesktop`'s
/// `NotifyKeyboardKeycode` takes one) and there is no keymap to derive it from: the
/// portal offers none, and libei's keymap belongs to the CAPTURE side and describes
/// the machine doing the capturing. So the positional half of key resolution is a
/// table of about a hundred and fifty numbers, which is exactly the sort of thing that
/// is wrong in three places and nobody notices.
///
/// **So there is one copy of it, in [`crate::x11`], and this delegates.** The first
/// version of this file had its own, hand-written from the HID and evdev tables, and
/// an adversarial review found what that costs: the two agreed on every entry they
/// shared and the duplicate was TWENTY-SIX ENTRIES SHORT, missing Execute, Help,
/// Menu, Select, Stop, Again, Undo, Cut, Copy, Paste, Find, the keypad comma and the
/// whole international and language block (Henkan, Muhenkan, Hangul, Hanja, Katakana,
/// Hiragana). Which is to say: Japanese and Korean keyboards would have driven a
/// Wayland target with a hole in the middle of their layout, and nothing would have
/// said so.
///
/// Sharing it also moves this table to where it is PROVED. The X11 backend's live
/// suite asks a real X server which key produces which keysym and checks these
/// entries against the answer (`the_usage_table_names_the_keys_the_server_names`),
/// which is evidence no Wayland machine here could give, and the relation between the
/// two platforms is one documented fact: an X keycode is the evdev code plus 8.
///
/// `None` is a legitimate answer that the engine already handles: the resolution falls
/// through to the next level of the typing table and, failing everything, to the
/// Unicode path, which on Wayland is the keysym call and always works.
pub fn evdev_of_usage(usage: u32) -> Option<u32> {
    crate::x11::evdev_of_usage(usage).map(u32::from)
}

/// The wire usage for an evdev keycode, or `None`.
///
/// The same one table, read the other way, including its rule that the consumer page
/// wins for the three keys that appear on both (mute and the two volume keys).
///
/// The argument is a `u32` because that is what arrives on the EI stream, and evdev
/// codes go above 255 (`KEY_MICMUTE` is 248 and the numbering runs to 0x2FF), so a
/// code the shared table cannot even represent answers `None` rather than wrapping
/// into a byte and naming the wrong key.
pub fn usage_of_evdev(code: u32) -> Option<u32> {
    crate::x11::usage_of_evdev(u8::try_from(code).ok()?)
}

/// A character as an X keysym.
///
/// Latin-1 code points ARE their own keysyms, which is the whole of the historical
/// table this build needs; everything else is the `0x01000000` Unicode escape, which
/// covers every code point there is. That is why a Wayland target has
/// [`Capabilities::unicode`] and an X11 one may not: X11 needs a spare keycode to
/// bind a keysym to, and this path needs nothing at all.
pub fn keysym_of(c: char) -> u32 {
    let cp = c as u32;
    // The two Latin-1 runs that are keysyms by identity. The gaps between them (the
    // C0 and C1 control ranges) have no keysym of their own and take the escape,
    // which is harmless: nothing types a control character through this path.
    if (0x20..=0x7E).contains(&cp) || (0xA0..=0xFF).contains(&cp) {
        cp
    } else {
        0x0100_0000 | cp
    }
}

/// How this machine can produce what the engine wants, in evdev terms.
///
/// # Why a symbol always answers `None`, and why that is not a gap
///
/// There is no keymap. `RemoteDesktop` offers none, the portal offers none, and
/// libei's keymap belongs to the CAPTURE side and describes the machine doing the
/// capturing. So "which key and which modifiers produce an at sign on this machine's
/// active layout" is a question this platform cannot answer, and answering it with a
/// guess would type the wrong character.
///
/// `None` sends the engine to the next level of the typing table and finally to
/// [`crate::backend::Action::Text`], which becomes a keysym per character and is
/// layout-independent by construction. So the outcome is BETTER than a keymap
/// lookup, not worse: an at sign typed as a keysym arrives as an at sign on an
/// AZERTY target, a QWERTY one and a Dvorak one, with no modifier dance and no
/// group switch.
///
/// What is genuinely lost: nothing for text, and for a POSITIONAL want nothing
/// either, since that is exactly what the table above answers. The one thing this
/// platform cannot do is honour a resolution that needs modifiers, and it never has
/// to, because it never produces one.
pub fn resolve(want: &crate::backend::Want) -> Option<crate::backend::Resolved> {
    let usage = match want {
        crate::backend::Want::Usage(u) => *u,
        crate::backend::Want::Named(name) => keys::usage_of(name)?,
        crate::backend::Want::Symbol(_) => return None,
    };
    Some(crate::backend::Resolved {
        code: crate::backend::PlatformKey {
            code: evdev_of_usage(usage)?,
            // Nothing else is needed to press an evdev key, and the field must not
            // carry anything a layout change can move: the seam says so, and here
            // there is no layout to move.
            detail: 0,
        },
        mods: 0,
        prefix: None,
        // Wayland has no keyboard groups to switch, exactly as Windows and macOS
        // have none, so a `Group` action is never produced and never received.
        group: None,
    })
}

/// The `Notify*` calls one batch of actions becomes, plus what had to be dropped.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Injection {
    pub calls: Vec<Notify>,
    /// An absolute pointer move arrived, and this platform cannot make one.
    ///
    /// Counted rather than silently discarded, and reported as a
    /// [`crate::backend::Refusal`] by the backend, because the whole promise of this
    /// component is that nothing disappears in silence. It should never happen:
    /// [`Capabilities::inject_pointer`] is false, so the engine never sends one. A
    /// non-zero count here is therefore a bug upstream, and it is the sort of bug
    /// that otherwise shows up as "the pointer does not move and nobody knows why".
    pub absolute_dropped: u32,
    /// A key the engine asked for that this platform has no code for.
    pub unresolved: u32,
}

/// Turns a batch of the engine's actions into `Notify*` calls, in order.
///
/// A batch is applied as given: the engine has already resolved the keys and
/// computed the sequence, so nothing here interprets, reorders or coalesces. The
/// mapping is mechanical except for the two sign conventions, which are not.
///
/// # The scroll signs, which are the one thing here to be suspicious of
///
/// This engine's convention is "positive is right and up"
/// (`crate::backend::BackendEvent::Wheel`). The portal's is evdev's and Wayland's:
/// a positive vertical value scrolls the content DOWN, which is what a wheel pushed
/// away from the hand does. So the vertical value is negated and the horizontal one
/// is not.
///
/// **Both halves of that are from the documentation, not from a desk.** They are in
/// one place, here, so that a live run finding the wheel inverted has one line to
/// change, and the deferred list names it.
pub fn inject(actions: &[crate::backend::Action]) -> Injection {
    use crate::backend::Action;
    let mut out = Injection::default();
    for action in actions {
        match action {
            Action::MoveTo(_) => out.absolute_dropped += 1,
            Action::MoveBy { dx, dy } => out.calls.push(Notify::PointerMotion {
                dx: f64::from(*dx),
                dy: f64::from(*dy),
            }),
            Action::Button { button, down } => match button_to_evdev(*button) {
                Some(code) => out.calls.push(Notify::PointerButton {
                    // The interface's argument is signed and an evdev button code
                    // never approaches the top of a `u32`, so the cast is exact.
                    button: code as i32,
                    pressed: *down,
                }),
                None => out.unresolved += 1,
            },
            Action::Wheel { dx, dy, pixels } => {
                if *pixels {
                    out.calls.push(Notify::PointerAxis {
                        dx: f64::from(*dx),
                        dy: -f64::from(*dy),
                    });
                } else {
                    // Two calls rather than one, because the interface's stepped
                    // scroll carries ONE axis per call. A notch in each direction at
                    // once is two calls, and a zero on an axis is no call: a
                    // scroll of nothing is not an event.
                    if *dy != 0 {
                        out.calls.push(Notify::PointerAxisDiscrete {
                            axis: AXIS_VERTICAL,
                            // Saturating, because `-i32::MIN` panics in a debug build.
                            // Unreachable through the engine (the wire clamps a wheel to
                            // 4096 and `inject_pointer` is false), and this function is
                            // `pub`, so it is checked rather than argued about.
                            steps: dy.saturating_neg(),
                        });
                    }
                    if *dx != 0 {
                        out.calls.push(Notify::PointerAxisDiscrete {
                            axis: AXIS_HORIZONTAL,
                            steps: *dx,
                        });
                    }
                }
            }
            // The interface's argument is SIGNED and this one is not, so the
            // conversion is checked rather than cast. It cannot fail on anything
            // this engine produces (every code comes from `resolve`, so from the
            // table), and an unchecked cast of a number near the top of a `u32`
            // would become a negative keycode, which is the sort of thing that
            // types a key nobody pressed.
            Action::Key { code, down } => match i32::try_from(code.code) {
                Ok(keycode) => out.calls.push(Notify::KeyboardKeycode {
                    keycode,
                    pressed: *down,
                }),
                Err(_) => out.unresolved += 1,
            },
            Action::Text(text) => {
                for c in text.chars() {
                    let keysym = keysym_of(c) as i32;
                    out.calls.push(Notify::KeyboardKeysym {
                        keysym,
                        pressed: true,
                    });
                    out.calls.push(Notify::KeyboardKeysym {
                        keysym,
                        pressed: false,
                    });
                }
            }
            // No meaning here, and harmless by the seam's own rule: the engine only
            // ever emits a group a `resolve` on this machine handed back, and this
            // machine's `resolve` never sets one.
            Action::Group(_) => {}
        }
    }
    out
}

// ------------------------------------------------------- reading: EI to the engine

/// The XKB real-modifier masks, which is what EI's modifier event carries.
///
/// Indices 0 to 7 of an XKB keymap's real modifiers, and the names below the first
/// three are a CONVENTION of ordinary keymaps rather than a guarantee of the
/// protocol: `Mod1` is Alt, `Mod4` is Super and `Mod5` is AltGr on every layout
/// `xkeyboard-config` ships and on nothing by definition. The X11 backend of this
/// same component makes exactly the same assumption for exactly the same reason
/// (there is no other way to name them), so the two agree, which matters more than
/// either being provably right.
mod xkb {
    pub const SHIFT: u32 = 1 << 0;
    pub const LOCK: u32 = 1 << 1;
    pub const CONTROL: u32 = 1 << 2;
    /// Alt.
    pub const MOD1: u32 = 1 << 3;
    /// Num Lock.
    pub const MOD2: u32 = 1 << 4;
    /// Super.
    pub const MOD4: u32 = 1 << 6;
    /// AltGr.
    pub const MOD5: u32 = 1 << 7;
}

/// An XKB modifier mask as this dialect's canonical bits.
pub fn mods_of_xkb(mask: u32) -> u16 {
    let mut out = 0u16;
    for (from, to) in [
        (xkb::SHIFT, keys::mods::SHIFT),
        (xkb::CONTROL, keys::mods::CTRL),
        (xkb::MOD1, keys::mods::ALT),
        (xkb::MOD5, keys::mods::ALTGR),
        (xkb::MOD4, keys::mods::META),
        (xkb::LOCK, keys::mods::CAPS),
        (xkb::MOD2, keys::mods::NUM),
    ] {
        if mask & from != 0 {
            out |= to;
        }
    }
    // Scroll Lock has no real modifier of its own in any keymap this build has met,
    // so the bit is never set from here. The lock's own half-duplex key frame still
    // travels, which is what actually toggles it on the target.
    out
}

/// Turns the EI stream into the seam's upcalls.
///
/// State, and every field of it earns its place:
#[derive(Clone, Debug, Default)]
pub struct Decoder {
    /// Where the pointer is, integrated from the activation's own position plus
    /// every delta since. The only absolute position this platform ever has: EI's
    /// relative motion is all that arrives, and there is nothing to ask.
    at: crate::backend::Point,
    /// The fraction of a pixel not yet spent.
    ///
    /// **Not an optimisation.** EI carries deltas as `f32`, a compositor applying an
    /// acceleration profile sends fractions constantly, and truncating each one
    /// independently loses every movement below one pixel: a slow, deliberate hand
    /// moves the pointer not at all. Carrying the remainder makes ten movements of
    /// 0.3 into three pixels instead of zero.
    frac_x: f32,
    frac_y: f32,
    /// The canonical modifiers, from the last modifier event the compositor sent.
    mods: u16,
    /// The activation these events belong to, from the portal's `Activated`.
    activation: Option<u32>,
    /// The EI stream has confirmed that activation with its own sequence.
    ///
    /// Events before it are dropped: see [`Decoder::decode`].
    confirmed: bool,
    /// A sequence the EI stream announced that no portal signal had named YET.
    ///
    /// **Without this the gate could never open, and a review found it.** The two
    /// arrive on different sockets and the EI one usually arrives FIRST, because the
    /// EI socket is what wakes the loop: a crossing produces
    /// `[StartEmulating{N}, Motion, Motion, ...]` in one read while `Activated{N}` is
    /// still queued on the bus. The first version threw an unmatched sequence away, so
    /// the `Activated` that came a moment later waited for a `start_emulating` that had
    /// already been consumed and could not arrive again: every event of every crossing
    /// was dropped, for ever, with the compositor holding the pointer. Remembering it is
    /// what makes the gate a gate rather than a wall.
    ///
    /// One value and not a set, deliberately: the compositor increments the id per
    /// activation and only one can be live, so a second announcement means the first is
    /// over and the newer one is the one that matters.
    announced: Option<u32>,
}

impl Decoder {
    /// Starts an activation. Called when the portal says `Activated`, with the
    /// position it reported, already pulled back inside the zones.
    pub fn activated(&mut self, activation: u32, at: crate::backend::Point) {
        self.at = at;
        self.frac_x = 0.0;
        self.frac_y = 0.0;
        self.activation = Some(activation);
        // The EI stream may already have announced this very activation, and usually
        // has: it is the socket that wakes the loop. So the gate opens here when the two
        // numbers already agree, rather than waiting for a `start_emulating` that has
        // been and gone.
        self.confirmed = self.announced == Some(activation);
        // **The modifiers are reset**, and a review is why. They were carried across,
        // and EI sends them only while emulating, so the first key of a crossing arrived
        // wearing the PREVIOUS crossing's mask: a Control the user had long released
        // fires a shortcut on the target instead of typing a letter. An empty mask is the
        // honest starting point, and the compositor sends the real one with its first
        // frame.
        self.mods = 0;
    }

    /// Ends it. Everything that arrives afterwards is stale by definition.
    pub fn deactivated(&mut self) {
        self.activation = None;
        self.confirmed = false;
        // Not the announcement: a `Deactivated` that overtook its own
        // `start_emulating` must not erase the evidence the next `Activated` needs. It
        // is cleared by `StopEmulating`, which is the EI stream's own word for the same
        // thing and cannot arrive out of order with itself.
    }

    pub fn at(&self) -> crate::backend::Point {
        self.at
    }

    pub fn mods(&self) -> u16 {
        self.mods
    }

    /// One EI event as one upcall, or nothing.
    ///
    /// # The sequence gate, which is the safety property of this function
    ///
    /// The portal's `Activated` carries an `activation_id`, and the interface
    /// promises the EI stream will carry the same number as the sequence of its
    /// `start_emulating`: "if the compositor sends a activation_id of N as part of
    /// this request it will also set the sequence in EIS' start_emulating event the
    /// same value N". Those are two different sockets, so they can and will arrive
    /// out of order, and a fast hand can cross an edge twice before either has
    /// settled.
    ///
    /// So: **nothing is reported until the two numbers agree.** An event before the
    /// EI side has confirmed, or after a mismatched confirmation, is dropped. What
    /// that prevents is the previous crossing's queued movement being applied to the
    /// new one, which on a plane of several screens sends the pointer to the wrong
    /// machine.
    ///
    /// What it costs, and it is a real cost: the first few events of an activation
    /// are dropped if the compositor sends them before `start_emulating`, which the
    /// protocol says it does not do. And `activation_id` WRAPS (the interface says
    /// so and says the increment is unspecified), which is why the comparison is
    /// equality and never an ordering.
    pub fn decode(&mut self, event: EiEvent) -> Option<crate::backend::BackendEvent> {
        use crate::backend::{BackendEvent, KeyEvent};
        // The three that are STATE rather than an event, handled before the gate
        // below. The modifiers in particular: they are what the next keystroke
        // needs, and dropping them for want of a confirmation would send the first
        // key of a session with no modifiers on it.
        match &event {
            EiEvent::StartEmulating { sequence } => {
                self.announced = Some(*sequence);
                match self.activation {
                    Some(id) if id == *sequence => self.confirmed = true,
                    Some(id) => {
                        warn(&format!(
                            "the EI stream is emulating sequence {sequence} while the portal \
                             activated {id}: dropping its events"
                        ));
                        self.confirmed = false;
                    }
                    // Remembered rather than refused, and this is the NORMAL order: the
                    // EI socket is what wakes the loop, so its word usually arrives
                    // first and the portal's a moment later. Nothing can be reported
                    // until the portal's lands anyway, because the position comes from
                    // it, and `activated` opens the gate on the strength of what is
                    // remembered here.
                    None => self.confirmed = false,
                }
                return None;
            }
            EiEvent::StopEmulating => {
                self.confirmed = false;
                self.announced = None;
                return None;
            }
            EiEvent::Modifiers {
                depressed,
                latched,
                locked,
            } => {
                self.mods = mods_of_xkb(depressed | latched | locked);
                return None;
            }
            _ => {}
        }
        if !self.confirmed {
            return None;
        }
        match event {
            EiEvent::Motion { dx, dy } => self.motion(dx, dy).map(BackendEvent::Motion),
            EiEvent::Button { button, down } => {
                button_from_evdev(button).map(|button| BackendEvent::Button { button, down })
            }
            EiEvent::ScrollDelta { dx, dy } => {
                // Pixels, and the sign flipped back into this engine's convention
                // (positive is right and up). Rounded rather than truncated, and no
                // fraction is carried: a scroll of nothing is not an event, and a
                // dropped fraction of a pixel of scroll is not a lost movement the
                // way a dropped fraction of a pixel of pointer motion is.
                let (dx, dy) = (dx.round() as i32, -dy.round() as i32);
                (dx != 0 || dy != 0).then_some(BackendEvent::Wheel {
                    dx,
                    dy,
                    pixels: true,
                })
            }
            EiEvent::ScrollDiscrete { dx, dy } => {
                let (dx, dy) = (dx / SCROLL_STEP, -(dy / SCROLL_STEP));
                (dx != 0 || dy != 0).then_some(BackendEvent::Wheel {
                    dx,
                    dy,
                    pixels: false,
                })
            }
            EiEvent::Key { keycode, down } => {
                let usage = usage_of_evdev(keycode).unwrap_or(0);
                Some(BackendEvent::Key(KeyEvent {
                    usage,
                    key: keys::name_of(usage).map(str::to_string),
                    // **No symbol, and this is the one real hole in a Wayland
                    // source.** The character this machine's layout produced is not
                    // on the EI stream: what libei hands over is an XKB keymap as a
                    // file descriptor, and reading it needs an XKB parser, which
                    // means a native library this build deliberately does not link.
                    // So the wire carries the POSITION and the modifiers, which is
                    // level one of the typing table and the level a target prefers
                    // anyway, and what is lost is typing text between two different
                    // layouts.
                    //
                    // The way out, for whoever picks it up: under XWayland the X
                    // server's keymap IS the compositor's, so the X11 backend's own
                    // Xkb reading would fill this in for the common case with no new
                    // dependency at all. It is on the deferred list.
                    sym: None,
                    mods: self.mods,
                    down,
                    lock: keys::is_lock(usage),
                }))
            }
            // Returned from above, and matched rather than left to a catch-all so
            // that a new EI event cannot be silently ignored here.
            EiEvent::StartEmulating { .. } | EiEvent::StopEmulating | EiEvent::Modifiers { .. } => {
                None
            }
        }
    }

    /// Integrates one relative movement, carrying the fraction.
    fn motion(&mut self, dx: f32, dy: f32) -> Option<crate::backend::Motion> {
        // A delta that is not a number would poison the carried fraction FOR EVER:
        // `NaN` plus anything is `NaN`, `NaN.trunc()` is `NaN`, and the subtraction
        // that computes the remainder keeps it. So the pointer would stop moving for
        // the rest of the session because of one bad frame. Dropped and said out
        // loud instead: no compositor should send one, and a component that seizes
        // up in silence is the failure this whole engine is written against.
        if !dx.is_finite() || !dy.is_finite() {
            warn(&format!(
                "the EI stream sent a movement of ({dx}, {dy}), which is not a number"
            ));
            return None;
        }
        let x = self.frac_x + dx;
        let y = self.frac_y + dy;
        let (whole_x, whole_y) = (x.trunc(), y.trunc());
        self.frac_x = x - whole_x;
        self.frac_y = y - whole_y;
        let (whole_x, whole_y) = (whole_x as i32, whole_y as i32);
        if whole_x == 0 && whole_y == 0 {
            // Not an event. The engine never emits a relative move of nothing either
            // (Windows discards one and it reaches no hook at all), and the fraction
            // stays behind to be spent on the next one.
            return None;
        }
        self.at = crate::backend::Point {
            x: self.at.x.saturating_add(whole_x),
            y: self.at.y.saturating_add(whole_y),
        };
        Some(crate::backend::Motion {
            at: self.at,
            dx: whole_x,
            dy: whole_y,
        })
    }
}

// ------------------------------------------------------- the capture lifecycle

/// Where a capture session has got to.
///
/// Five states, and the shape is the interface's rather than a convenience: the
/// portal separates ARMED (the barriers are watched) from ACTIVE (the pointer has
/// crossed one and events are flowing), and conflating them would lose the only
/// moment at which this machine knows it has the keyboard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    /// No session with the portal. The resting state, and the state every teardown
    /// path must reach.
    Idle,
    /// A session exists, the barriers are placed, `Enable` has been called. The
    /// compositor is watching the edges and nothing is being captured.
    Armed,
    /// The pointer crossed a barrier. Events are flowing, and this machine is the
    /// source.
    Active { activation: u32 },
    /// The portal said `Disabled`: it is watching nothing until `Enable` is called
    /// again. The session and its barriers survive.
    Suspended,
    /// The session is over and a new one is needed. Carries what ended it, so the
    /// interface can say which.
    Dead(PortalError),
}

impl Phase {
    /// Is this machine capturing right now?
    pub fn capturing(&self) -> bool {
        matches!(self, Phase::Active { .. })
    }
}

/// A capture session, driven by the engine's wishes and the portal's signals.
///
/// # The five rules of the interface this encodes, each of which costs a session
/// # when it is missed
///
/// 1. **`SetPointerBarriers` suspends the session.** The interface says it
///    "immediately suspends the current session and the application must call
///    Enable() after this method". So every barrier change is followed by an
///    `Enable`, unconditionally, and a barrier change while ACTIVE also ends that
///    activation.
/// 2. **The zone serial must be the one the compositor last handed out.** Barriers
///    carrying a stale serial fail, so `GetZones` and `SetPointerBarriers` are one
///    step and never two.
/// 3. **`ZonesChanged` may carry a serial this side has never seen**, and the serial
///    always increases (wrapping). So a signal whose serial equals the one already
///    held is discarded rather than causing a needless round trip, and any other is
///    acted on.
/// 4. **A stale `Deactivated` can arrive after `Disable` or `Release`.** The
///    interface warns about it in as many words. So a `Deactivated` for an
///    activation that is not the current one changes nothing.
/// 5. **`activation_id` wraps**, and its increment is unspecified. Every comparison
///    is equality; no arithmetic and no ordering anywhere.
///
/// # What it does NOT do, deliberately
///
/// It does not decide whether a crossing should happen. That is the engine's
/// crossing graph and its guards, which know the plane and the neighbours; this only
/// knows that the pointer left through barrier N at position P and reports it as a
/// motion. So an activation on an edge with nothing across it is released by the
/// engine through [`Capture::stop`], by the code that already exists for that case.
pub struct Capture {
    /// Held by `Arc` and not borrowed, so that the backend's loop can own both the
    /// portal and this. A borrow would make the loop self-referential, which is the
    /// sort of thing that gets solved with a lifetime nobody can name.
    portal: std::sync::Arc<dyn Portal>,
    session: Option<SessionId>,
    phase: Phase,
    zones: ZoneSet,
    placed: Vec<Placed>,
    /// The barriers the compositor refused, kept for the log and for the interface:
    /// a refused barrier is an edge the pointer cannot leave through, which is a
    /// crossing that will never fire and would otherwise be a mystery.
    refused: Vec<u32>,
    /// The screens moved, so the barriers this side holds are for a serial the
    /// compositor no longer accepts.
    ///
    /// Not derivable from the phase, which is why it is a field: `Suspended` is
    /// reached both by a `Disable` (barriers fine, one `Enable` away) and by a
    /// `ZonesChanged` (barriers worthless). An `arm` that could not tell them apart
    /// enabled the stale ones, and the compositor then watched edges that had moved:
    /// the pointer would leave at the wrong place, or nowhere.
    barriers_stale: bool,
    decoder: Decoder,
    /// The live event stream, opened before `Enable` because the interface requires
    /// that order, and dropped with the session. Held here and not beside the
    /// backend so that a session's death takes its stream with it: a stream that
    /// outlived its session would deliver events for a capture nobody is in.
    ei: Option<Box<dyn EiSource>>,
}

impl Capture {
    pub fn new(portal: std::sync::Arc<dyn Portal>) -> Capture {
        Capture {
            portal,
            session: None,
            phase: Phase::Idle,
            zones: ZoneSet::default(),
            placed: Vec::new(),
            refused: Vec::new(),
            barriers_stale: false,
            decoder: Decoder::default(),
            ei: None,
        }
    }

    pub fn phase(&self) -> &Phase {
        &self.phase
    }

    pub fn zones(&self) -> &ZoneSet {
        &self.zones
    }

    pub fn placed(&self) -> &[Placed] {
        &self.placed
    }

    pub fn refused(&self) -> &[u32] {
        &self.refused
    }

    pub fn decoder(&mut self) -> &mut Decoder {
        &mut self.decoder
    }

    /// Asks the portal for a session, places the barriers and arms them.
    ///
    /// **This is the call that raises the consent dialog**, so it happens when a
    /// person switches the feature on and never at start-up. Idempotent: an
    /// already-armed session is left alone, because asking again would ask a second
    /// time for permission that has already been given.
    pub fn arm(&mut self) -> Result<(), PortalError> {
        match &self.phase {
            Phase::Armed | Phase::Active { .. } => return Ok(()),
            // A dead session is not re-armed here. The caller decides whether to
            // start over, because starting over means another dialog and that is a
            // decision about a person's attention rather than about state.
            Phase::Dead(e) => return Err(e.clone()),
            // Suspended with good barriers is one call away; suspended because the
            // screens moved needs them placed again first.
            Phase::Suspended if self.barriers_stale => return self.rearm_barriers(),
            Phase::Suspended => return self.enable(),
            Phase::Idle => {}
        }
        let grant = self.begin()?;
        self.session = Some(grant.session);
        // From here on, a failure CLOSES the session rather than returning with it
        // still open. A review found what the first version cost: `arm` returned `Err`
        // with the phase still `Idle` and a live session in the field, the engine
        // re-armed, `begin` ran a SECOND `CreateSession` (a second consent dialog) and
        // overwrote the handle, so the first session lived until the process exited.
        // And from that state a `Capture(Off)` reached `Suspended` with no barriers at
        // all, which the next `arm` then simply `Enable`d: armed, watching nothing,
        // claiming it could drive.
        // Read back rather than assumed: the granted capabilities are always a
        // subset of what was asked for, and a grant with no pointer is a session
        // that can never see the pointer leave.
        if grant.capabilities & caps::POINTER == 0 {
            let e = PortalError::Refused { cancelled: false };
            warn("the capture session was granted without the pointer, so no edge can be watched");
            self.die(e.clone());
            return Err(e);
        }
        self.rearm_barriers().inspect_err(|e| {
            // `rearm_barriers` dies on its own for the two failures it recognises; this
            // covers the rest, so no route out of `arm` leaves a session behind.
            if !matches!(self.phase, Phase::Dead(_)) {
                self.die(e.clone());
            }
        })?;
        Ok(())
    }

    /// `CreateSession` plus `ConnectToEIS`, in the order the interface requires:
    /// "This call must be invoked before ... Enable()". Getting that order wrong
    /// gives a session that arms, activates, and delivers not one event.
    fn begin(&mut self) -> Result<CaptureGrant, PortalError> {
        let grant = self.portal.capture_create_session(caps::WANTED)?;
        match self.portal.capture_connect(&grant.session) {
            Ok(ei) => self.ei = Some(ei),
            Err(e) => {
                // A session with no event stream can never report anything, so it is
                // closed rather than left armed and mute. Hyprland's portal answers
                // "EIS fd is not ready" here when its compositor has not set the
                // stream up yet, which is a real case and a retryable one.
                self.portal.close(&grant.session);
                return Err(e);
            }
        }
        Ok(grant)
    }

    /// Drains the live stream into the seam's upcalls.
    ///
    /// Answers the upcalls and whether the stream has gone. A stream that has gone
    /// ends the session: it is the only way events can arrive, so a capture without
    /// it is a machine holding a keyboard it cannot read.
    pub fn poll(
        &mut self,
        budget: std::time::Duration,
    ) -> (Vec<crate::backend::BackendEvent>, bool) {
        // The borrow of `self.ei` ends with this statement, on purpose: the loop
        // below needs `self.decoder`, and holding both at once is a second mutable
        // borrow of the same value.
        let poll = match self.ei.as_mut() {
            Some(ei) => ei.poll(budget),
            None => return (Vec::new(), false),
        };
        let mut out = Vec::new();
        for event in poll.events {
            if let Some(up) = self.decoder.decode(event) {
                out.push(up);
            }
        }
        if poll.gone {
            warn("the EI stream ended, so this capture session is over");
            self.die(PortalError::SessionClosed);
        }
        (out, poll.gone)
    }

    /// Reads the zones, places the barriers on them, and enables.
    ///
    /// One step for the three because of rules 1 and 2: the serial the barriers carry
    /// must be the one the zones just answered with, and setting barriers suspends
    /// the session so the `Enable` is not optional.
    pub fn rearm_barriers(&mut self) -> Result<(), PortalError> {
        let session = self.session.clone().ok_or(PortalError::SessionClosed)?;
        let zones = self.portal.capture_zones(&session)?;
        let placed = barriers_for(&zones.zones);
        let barriers: Vec<Barrier> = placed.iter().map(|p| p.barrier).collect();
        let refused = self
            .portal
            .capture_set_barriers(&session, zones.serial, &barriers)?;
        if !refused.is_empty() {
            warn(&format!(
                "the compositor refused {} of {} barriers ({refused:?}): the pointer cannot \
                 leave through those edges",
                refused.len(),
                barriers.len()
            ));
        }
        self.zones = zones;
        self.barriers_stale = false;
        // The refused ones are dropped from what is remembered, so an `Activated`
        // can never be attributed to a barrier that was never placed.
        self.placed = placed
            .into_iter()
            .filter(|p| !refused.contains(&p.barrier.id))
            .collect();
        self.refused = refused;
        if self.placed.is_empty() {
            // Every barrier refused, or no zones at all. Armed with nothing to watch
            // is worse than not armed: the interface would say this machine can drive
            // and the pointer would never leave.
            let e = PortalError::Failed {
                name: "org.freedesktop.portal.Error.Failed".to_string(),
                message: "no pointer barrier could be placed on this desktop".to_string(),
            };
            self.die(e.clone());
            return Err(e);
        }
        // Rule 1: the barriers were just replaced, so the session is suspended
        // whatever it was doing, and the activation is over.
        self.decoder.deactivated();
        self.phase = Phase::Suspended;
        self.enable()
    }

    fn enable(&mut self) -> Result<(), PortalError> {
        let session = self.session.clone().ok_or(PortalError::SessionClosed)?;
        match self.portal.capture_enable(&session) {
            Ok(()) => {
                self.phase = Phase::Armed;
                Ok(())
            }
            Err(e) => {
                self.die(e.clone());
                Err(e)
            }
        }
    }

    /// Gives the pointer back, at `cursor` if the compositor will take the
    /// suggestion, and leaves the barriers armed.
    ///
    /// The path the return hotkey and every ordinary crossing-back take. `Release`
    /// rather than `Disable`, because the session is wanted again the moment the
    /// pointer touches an edge.
    pub fn stop(&mut self, cursor: Option<(f64, f64)>) {
        let Phase::Active { activation } = self.phase else {
            return;
        };
        let Some(session) = self.session.clone() else {
            return;
        };
        // Pulled back inside the zones before it is suggested: a position past the
        // barrier is a position on a screen that is not there.
        let cursor = cursor.and_then(|at| clamp_into(&self.zones.zones, at));
        if let Err(e) = self.portal.capture_release(&session, activation, cursor) {
            // **Any failure ends the session, retryable or not**, and a review is why.
            // The first version carried on for a retryable one (a `Timeout`, which is
            // the likeliest of all against a busy compositor) and set `Armed`. But the
            // compositor is still capturing: the decoder is now unconfirmed so every
            // event is dropped, the late `Deactivated` finds `Armed` and is ignored by
            // rule 4, and nothing will ever call `Release` for that activation again
            // because the phase no longer carries it. The person's mouse and keyboard
            // are dead and the only trace is one line on standard error.
            //
            // A release we cannot confirm means we do not know who holds the pointer,
            // and the only honest answer to that is to end the session and say so: the
            // engine hears `CaptureLost`, the interface gets a sentence, and a fresh
            // session starts clean.
            warn(&format!(
                "releasing the capture failed, so this session is over: {e}"
            ));
            self.die(e);
            return;
        }
        self.decoder.deactivated();
        self.phase = Phase::Armed;
    }

    /// Stops watching the edges entirely, keeping the session and its consent.
    ///
    /// What the engine asks for when it goes from watching an edge to resting: the
    /// session is expensive to re-create (it costs a person a dialog) and cheap to
    /// re-enable.
    pub fn disarm(&mut self) {
        // **A dead session stays dead**, and a review found why that matters. The engine
        // reacts to a `CaptureLost` by re-reading the capabilities and sending
        // `Capture(Off)`, which lands here; the first version then reached `Idle`,
        // throwing the reason away, and the next `publish` recomputed the capabilities
        // from the portal's own offer and put `capture`, `swallow`, `confine` and
        // `problem: None` back. The interface said the machine was fine, the engine saw
        // it could drive again and sent `Watch`, and a compositor that keeps refusing
        // produced a consent dialog per cycle for as long as a peer stayed warm.
        if matches!(self.phase, Phase::Dead(_)) {
            return;
        }
        if let Phase::Active { .. } = self.phase {
            self.stop(None);
        }
        let Some(session) = self.session.clone() else {
            self.phase = Phase::Idle;
            return;
        };
        if let Err(e) = self.portal.capture_disable(&session) {
            warn(&format!("disabling the capture failed: {e}"));
        }
        self.decoder.deactivated();
        // Suspended and not Idle: the session is still there, and rule 1's `Enable`
        // is all it takes to arm it again.
        self.phase = Phase::Suspended;
    }

    /// Ends everything and reaches [`Phase::Idle`].
    ///
    /// **Every teardown path goes through here, including the ones that happen
    /// because something is already wrong.** It releases before it closes, it closes
    /// whatever it has, and it reaches `Idle` whether or not any of that worked: a
    /// capture that outlives its session is a machine whose keyboard belongs to
    /// nobody, and this is the one function that must not be able to fail.
    pub fn close(&mut self) {
        if let Phase::Active { .. } = self.phase {
            self.stop(None);
        }
        if let Some(session) = self.session.take() {
            self.portal.close(&session);
        }
        self.ei = None;
        self.decoder.deactivated();
        self.placed.clear();
        self.refused.clear();
        self.zones = ZoneSet::default();
        self.barriers_stale = false;
        self.phase = Phase::Idle;
    }

    fn die(&mut self, why: PortalError) {
        if let Some(session) = self.session.take() {
            self.portal.close(&session);
        }
        self.ei = None;
        self.decoder.deactivated();
        self.placed.clear();
        self.refused.clear();
        // The screens too. `publish` reports no screens when there are none, and a
        // review found that `die` kept them: after any session death the plane went on
        // carrying an arrangement from a session that was gone, and since nothing had
        // changed there was not even an upcall to say so.
        self.zones = ZoneSet::default();
        self.barriers_stale = false;
        self.phase = Phase::Dead(why);
    }

    /// Applies one signal from the portal. Answers `true` when the barriers need
    /// placing again, which the caller does rather than this function so that the
    /// dialog-free round trips stay out of a signal handler.
    pub fn signal(&mut self, signal: &Signal) -> bool {
        // A signal for some other session is not this session's business. It can
        // happen: one connection carries every session the process has.
        let mine = |s: &SessionId| self.session.as_ref() == Some(s);
        match signal {
            Signal::Activated {
                session,
                activation,
                cursor,
                barrier,
            } if mine(session) => {
                if !matches!(self.phase, Phase::Armed) {
                    // Activated while not armed: a signal that overtook a `Disable`,
                    // or a compositor doing something the interface does not
                    // describe. Not acted on, and said out loud, because acting on
                    // it would start capturing with no barriers this side believes
                    // in.
                    warn(&format!(
                        "the portal activated a capture while this side was {:?}",
                        self.phase
                    ));
                    return false;
                }
                // The position is regularly outside the zones, and adjusting it is
                // documented as the application's job.
                // **A missing position is refused, not invented.** The first version
                // used `(0, 0)`, and the engine decides the crossing from the position:
                // an activation placed at the top-left corner of the desktop picks the
                // wrong edge, or the wrong screen, and says nothing about it. The
                // interface documents the position as always present on this signal, so
                // its absence is a compositor this build cannot follow.
                let Some(at) =
                    cursor
                        .and_then(|at| clamp_into(&self.zones.zones, at))
                        .map(|(x, y)| crate::backend::Point {
                            x: x.round() as i32,
                            y: y.round() as i32,
                        })
                else {
                    warn(
                        "the portal activated a capture without saying where the pointer is, \
                         so there is no crossing to report: releasing it",
                    );
                    // Given back rather than held: the compositor is capturing and
                    // nothing can be done with it.
                    self.phase = Phase::Active {
                        activation: *activation,
                    };
                    self.stop(None);
                    return false;
                };
                match barrier {
                    Some(0) | None => warn(
                        "the portal activated a capture without saying which barrier: the \
                         crossing will be decided from the pointer position alone",
                    ),
                    Some(id) if !self.placed.iter().any(|p| p.barrier.id == *id) => {
                        warn(&format!(
                            "the portal named barrier {id}, which this side did not place"
                        ));
                    }
                    Some(_) => {}
                }
                self.decoder.activated(*activation, at);
                self.phase = Phase::Active {
                    activation: *activation,
                };
                false
            }
            Signal::Deactivated {
                session,
                activation,
            } if mine(session) => {
                // Rule 4: a stale one changes nothing. Equality only, because the id
                // wraps.
                if self.phase
                    == (Phase::Active {
                        activation: *activation,
                    })
                {
                    self.decoder.deactivated();
                    self.phase = Phase::Armed;
                }
                false
            }
            Signal::Disabled { session } if mine(session) => {
                self.decoder.deactivated();
                self.phase = Phase::Suspended;
                false
            }
            Signal::ZonesChanged { session, serial } if mine(session) => {
                // Rule 3: the serial always increases and may skip, so anything but
                // the one already held is worth acting on, and the one already held
                // is not.
                if *serial == self.zones.serial {
                    return false;
                }
                // The pointer comes back FIRST if a capture is live. The first version
                // dropped the activation id and set `Suspended`, and a review found
                // where that ends: with `want == Off` the caller never places the
                // barriers again, so no `SetPointerBarriers` implicitly ends the
                // activation either, and the compositor keeps capturing while this side
                // drops every event.
                if self.phase.capturing() {
                    self.stop(None);
                    if matches!(self.phase, Phase::Dead(_)) {
                        return false;
                    }
                }
                self.decoder.deactivated();
                self.barriers_stale = true;
                self.phase = Phase::Suspended;
                true
            }
            Signal::Closed { session } if mine(session) => {
                // The compositor ended it. Not `close`, which would try to close a
                // session that is already gone: the handle is dropped and the phase
                // says why, so the interface can tell somebody.
                self.session = None;
                self.ei = None;
                self.decoder.deactivated();
                self.placed.clear();
                self.refused.clear();
                self.zones = ZoneSet::default();
                self.barriers_stale = false;
                self.phase = Phase::Dead(PortalError::SessionClosed);
                false
            }
            _ => false,
        }
    }
}

// ----------------------------------------------------------------- the backend

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;

use crate::backend::{
    Action, BackendEvent, CaptureLoss, CaptureMode, InputBackend, Monitor, PlatformKey, Point,
    Rect, Refusal, Resolved, Want,
};

/// How much of a session the backend's loop keeps between wakes.
///
/// **Two budgets, and the reason is latency where latency exists.** While a capture
/// is live, the loop is the only thing between this machine's mouse and the wire, so
/// it waits 2 ms; the rest of the time nothing is urgent and it waits 50 ms.
///
/// **This is a poll loop and the right answer is not.** The X11 backend of this same
/// component waits on ONE waitable set (the X socket plus a `mio` waker for its
/// command queue) and wakes only when something happened. The same is available here
/// (the EI file descriptor is pollable and `mio` is already a dependency), and it is
/// not written, deliberately: this loop has never run against a compositor, and
/// optimising the wake pattern of code nobody has executed is work spent before the
/// measurement that would justify it. 2 ms is a real cost on a path whose round trip
/// #123 measured at 4 ms, so it is on the deferred list under its own name and not
/// hidden here.
const POLL_ACTIVE: Duration = Duration::from_millis(2);
const POLL_IDLE: Duration = Duration::from_millis(50);

/// How many upcalls the loop may have outstanding towards the engine.
///
/// The same number the X11 backend uses, for the same reason: a burst of pointer
/// motion at 125 Hz must not block the loop, and a channel that filled would.
const BACKEND_EVENT_CAPACITY: usize = 256;

/// The command queue between the engine's thread and the loop's.
///
/// A `Mutex` plus a `Condvar` rather than a channel, and the reason is a type: the
/// seam requires the handle to be `Sync` (the engine clones it into per-session
/// work), and `std::sync::mpsc::Sender` is `Send` and not `Sync`. This is also the
/// shape the X11 backend uses, for its own version of the same constraint, so the two
/// read alike.
#[derive(Default)]
struct Queue {
    cmds: Mutex<std::collections::VecDeque<Cmd>>,
    woke: std::sync::Condvar,
}

impl std::fmt::Debug for Queue {
    /// The seam's handle is `Debug` (the engine logs it), and a queue's contents are
    /// not something a log line should take a lock for, so this says how deep it is
    /// and nothing else. Hand-written rather than derived because [`Cmd`] carries a
    /// batch of actions and printing one would be a page of output per line.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let queued = match self.cmds.try_lock() {
            Ok(q) => q.len().to_string(),
            // Never blocks for a log line, and never panics on a poisoned lock:
            // both would turn a diagnostic into a fault.
            Err(_) => "?".to_string(),
        };
        write!(f, "Queue({queued} queued)")
    }
}

impl Queue {
    /// Pushes, then wakes. In that order: a wake before the push can be consumed by a
    /// loop that then finds nothing.
    fn push(&self, cmd: Cmd) {
        match self.cmds.lock() {
            Ok(mut q) => q.push_back(cmd),
            // Recovered rather than propagated, so an `Exit` or a `Release` is never
            // lost to a panic that happened somewhere else. The X11 backend does the
            // same, for the same reason: these two commands are what keep a machine's
            // keyboard alive.
            Err(p) => p.into_inner().push_back(cmd),
        }
        self.woke.notify_one();
    }

    /// Everything queued, and nothing waits.
    fn drain(&self) -> std::collections::VecDeque<Cmd> {
        match self.cmds.lock() {
            Ok(mut q) => std::mem::take(&mut *q),
            Err(p) => std::mem::take(&mut *p.into_inner()),
        }
    }

    /// Waits up to `budget` for something to arrive. Answers what arrived, which may
    /// be nothing: a spurious wake is not an error and the caller loops anyway.
    fn wait(&self, budget: Duration) -> std::collections::VecDeque<Cmd> {
        let Ok(guard) = self.cmds.lock() else {
            // A poisoned queue is drained rather than waited on: something already
            // panicked, and the one thing that must still work is the teardown.
            return self.drain();
        };
        if !guard.is_empty() {
            let mut guard = guard;
            return std::mem::take(&mut *guard);
        }
        match self.woke.wait_timeout(guard, budget) {
            Ok((mut guard, _)) => std::mem::take(&mut *guard),
            Err(p) => {
                let (mut guard, _) = p.into_inner();
                std::mem::take(&mut *guard)
            }
        }
    }
}

/// What the engine asked the loop to do.
enum Cmd {
    Capture(CaptureMode),
    Warp(Point),
    Inject(Vec<Action>),
    Release(Vec<PlatformKey>),
    Exit(i32),
}

/// The `Clone` handle the engine drives. Three `Arc`s and nothing else.
#[derive(Clone, Debug)]
pub struct WaylandBackend {
    queue: Arc<Queue>,
    caps: Arc<Mutex<Capabilities>>,
    /// Kept up to date by the loop rather than fetched on demand, which is the one
    /// place this backend differs from its siblings and it is not laziness: the
    /// screens come from `GetZones`, that call needs a live capture session, and a
    /// session needs a person's consent. So there is nothing to ask when nobody has
    /// consented, and something to remember when they have.
    monitors: Arc<Mutex<Vec<Monitor>>>,
}

impl WaylandBackend {
    fn push(&self, cmd: Cmd) {
        self.queue.push(cmd);
    }
}

impl InputBackend for WaylandBackend {
    fn capabilities(&self) -> Capabilities {
        match self.caps.lock() {
            Ok(caps) => caps.clone(),
            Err(p) => p.into_inner().clone(),
        }
    }

    fn monitors(&self) -> impl std::future::Future<Output = Vec<Monitor>> + Send {
        let monitors = match self.monitors.lock() {
            Ok(m) => m.clone(),
            Err(p) => p.into_inner().clone(),
        };
        async move { monitors }
    }

    fn pointer(&self) -> impl std::future::Future<Output = Option<Point>> + Send {
        // **Nothing in either portal says where the pointer is.** `InputCapture`
        // reports a position when a capture STARTS and nothing before or after;
        // `RemoteDesktop` has no query at all. So this answers `None`, which the
        // seam allows and the engine handles: it is the same answer a machine that
        // cannot enumerate its own pointer gives, and it is true here.
        std::future::ready(None)
    }

    fn resolve(&self, want: Want) -> impl std::future::Future<Output = Option<Resolved>> + Send {
        // The one downcall that answers, and on this platform it needs no round trip
        // at all: there is no keymap to consult, so the answer is a table lookup.
        // That is the whole of what a Wayland target can say about its keyboard, and
        // it is why every symbol falls through to the keysym path.
        let answer = resolve(&want);
        async move { answer }
    }

    fn capture(&self, mode: CaptureMode) {
        self.push(Cmd::Capture(mode));
    }

    fn confine(&self, _rect: Option<Rect>) {
        // **Nothing to do, and it is not a gap.** Under `InputCapture` the
        // confinement IS the capture: while one is active the compositor holds the
        // pointer at the barrier it crossed, so it cannot walk off this desktop, and
        // there is no rectangle to hand anybody. The rectangle is advisory here in
        // the same sense as on macOS, where the mechanism is a decoupling rather than
        // a clip, and `Capabilities::confine` carries the whole two-part contract.
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
        // here would skip the release and the session close on a machine that may be
        // holding somebody's keyboard through a compositor.
        self.push(Cmd::Exit(code));
    }
}

/// The Wayland backend's own thread: the portal, the capture session, the event
/// stream and the command queue.
pub struct WaylandLoop {
    portal: Arc<dyn Portal>,
    capture: Capture,
    queue: Arc<Queue>,
    events: mpsc::Sender<BackendEvent>,
    caps: Arc<Mutex<Capabilities>>,
    monitors: Arc<Mutex<Vec<Monitor>>>,
    report: Report,
    /// What the engine last asked for.
    want: CaptureMode,
    /// The injection session, created on the first thing to type rather than at
    /// start-up, because creating it raises a dialog and a machine nobody is driving
    /// should not be asking for permission to be driven.
    remote: Option<RemoteGrant>,
    /// A `RemoteDesktop` session that was refused, so it is not asked for again on
    /// every keystroke. Cleared when the engine stops and starts again, which is the
    /// gesture that means "ask me once more".
    remote_refused: Option<PortalError>,
    /// Where a warp asked for the pointer to go, spent as the next `Release`'s
    /// suggestion. The nearest thing to a warp either portal has.
    ///
    /// Cleared whenever a capture ends, so a warp asked for during one activation is
    /// never spent as the release position of a much later one. A review found that: it
    /// was set and only ever consumed by a `Capture(Off)`, so it survived indefinitely.
    warp_to: Option<Point>,
    /// The facts the engine is too far behind to have been told yet. See [`Self::emit`].
    deferred: Vec<BackendEvent>,
    exit: Option<i32>,
}

impl WaylandLoop {
    /// Pumps until the engine asks to exit or the command channel closes.
    pub fn run(mut self) -> i32 {
        // Whatever ends this loop, the session is given back and closed. A capture
        // that outlives its process leaves the compositor holding a pointer that
        // nothing will ever release.
        let code = self.pump();
        self.capture.close();
        if let Some(remote) = self.remote.take() {
            self.portal.close(&remote.session);
        }
        code
    }

    fn pump(&mut self) -> i32 {
        loop {
            if let Some(code) = self.exit {
                return code;
            }
            // Drained BEFORE the liveness check, and a review found why. `main` pushes
            // `request_exit(code)` and then drops the handle, and the engine's teardown
            // pushes the release of every key a peer left down just before that; both
            // land while this loop is inside its poll. Checking the count first threw
            // them away: the process reported 0 instead of the engine's code, and the
            // keys stayed physically down on this desktop with `held.json` already
            // cleared, so the next run would not release them either. One more turn is
            // all it takes, and a turn begins by draining, which is the same rule the
            // X11 backend's loop follows for the same reason.
            let queued = self.queue.drain();
            let gone = Arc::strong_count(&self.queue) == 1;
            for cmd in queued {
                self.command(cmd);
            }
            if let Some(code) = self.exit {
                return code;
            }
            // Every handle has been dropped, so the engine is gone without having asked.
            // The backstop rather than the normal road (it calls `request_exit` on its
            // way out, even when it panicked), and it is what stops the loop spinning for
            // ever if that road is ever missed: a session held open by a loop nobody can
            // talk to is a compositor holding a pointer for ever.
            if gone {
                return 0;
            }
            // Every signal is followed by a republish, and not only the ones that
            // asked for new barriers. That was a real bug and a bad one: a `Closed`
            // signal set the phase to dead and published nothing, so the interface
            // went on saying this machine could drive through a session the
            // compositor had already taken away. `publish` says nothing when nothing
            // moved, so the unconditional call costs a comparison.
            let signals = self.portal.take_signals();
            let any = !signals.is_empty();
            for signal in signals {
                if self.capture.signal(&signal) && self.want != CaptureMode::Off {
                    // The screens moved. Placed again here rather than inside the
                    // signal handler, so a round trip never happens while a signal
                    // is being decoded.
                    if let Err(e) = self.capture.rearm_barriers() {
                        self.lost(&e);
                    }
                }
            }
            if any {
                // A capture that ended takes an unspent warp with it: it was asked for
                // during that activation and means nothing to the next.
                if !self.capture.phase().capturing() {
                    self.warp_to = None;
                }
                self.publish();
            }
            let budget = if self.capture.phase().capturing() {
                POLL_ACTIVE
            } else {
                POLL_IDLE
            };
            // Polled while there is a stream to poll, which is any live session:
            // armed, because the compositor can activate at any moment, and active,
            // because that is when the events are.
            if matches!(self.capture.phase(), Phase::Armed | Phase::Active { .. }) {
                let (events, gone) = self.capture.poll(budget);
                for event in events {
                    self.emit(event);
                }
                if gone {
                    self.emit(BackendEvent::CaptureLost(CaptureLoss::SessionEnded));
                    self.publish();
                }
            } else {
                // Nothing to poll, so the wait happens on the command queue instead
                // and a downcall wakes the loop at once rather than after a budget.
                for cmd in self.queue.wait(budget) {
                    self.command(cmd);
                }
            }
        }
    }

    fn command(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Capture(mode) => self.set_capture(mode),
            // A warp is remembered rather than done, and it is spent by the next
            // release. If no release comes it must not survive: a review found it
            // outliving its own activation and being handed to a much later one as
            // that one's release position.
            Cmd::Warp(to) => {
                // Remembered rather than done: the nearest thing to a warp in either
                // portal is `Release`'s `cursor_position`, which applies only when a
                // capture ends and which the compositor may ignore. That is why
                // `Capabilities::warp` is false: this is a best effort on one path,
                // not a capability.
                if self.capture.phase().capturing() {
                    self.warp_to = Some(to);
                } else {
                    // Not during a capture, so there is no release to spend it on and no
                    // other mechanism to do it with. Dropped, and said out loud, because
                    // `Capabilities::warp` is false and the engine should not be asking.
                    warn(&format!("nothing here can put the pointer at {to:?}"));
                }
            }
            Cmd::Inject(actions) => self.inject(actions),
            Cmd::Release(keys) => {
                // The hygiene path, and it must work while anything else is in
                // flight. Built as ordinary key actions so it goes down exactly the
                // same road as a press: a release that took a different path is a
                // release that can differ from its press.
                let actions: Vec<Action> = keys
                    .into_iter()
                    .map(|code| Action::Key { code, down: false })
                    .collect();
                self.inject(actions);
            }
            Cmd::Exit(code) => self.exit = Some(code),
        }
    }

    fn set_capture(&mut self, mode: CaptureMode) {
        self.want = mode;
        match mode {
            // **`Watch` and `Swallow` are one thing here**, unlike on X11 where
            // watching is a raw event selection and swallowing is a grab. An
            // `InputCapture` session is exclusive by construction: while it is
            // active the events come here and not to the desktop, and there is no
            // way to ask for the other. So the engine's `Watch` arms the barriers
            // and the crossing itself is what swallows, which is the same outcome by
            // a different road and is why `capture` and `swallow` always agree in
            // the capabilities.
            CaptureMode::Watch | CaptureMode::Swallow => {
                // A fresh gesture is also the moment to ask about a refused
                // injection session again.
                self.remote_refused = None;
                if let Err(e) = self.capture.arm() {
                    self.lost(&e);
                }
            }
            CaptureMode::Off => {
                // Disarmed and not closed: the session cost a person a dialog, and
                // enabling one again costs a round trip. What must be true either
                // way is that nothing is being captured when this returns.
                let cursor = self
                    .warp_to
                    .take()
                    .map(|p| (f64::from(p.x), f64::from(p.y)));
                self.capture.stop(cursor);
                self.capture.disarm();
            }
        }
        self.publish();
    }

    fn inject(&mut self, actions: Vec<Action>) {
        let out = inject(&actions);
        if out.absolute_dropped > 0 {
            // It should never happen: `inject_pointer` is false, so the engine never
            // sends one. Reported rather than counted in silence, because a silent
            // drop here is a pointer that does not move with nobody able to say why.
            warn(&format!(
                "{} absolute pointer moves were dropped: this platform has no absolute \
                 injection without a paired screen cast",
                out.absolute_dropped
            ));
        }
        if out.unresolved > 0 {
            // **Not a refusal upcall**, and a review corrected this: the four codes this
            // seam defines are all about the OS withholding something, and "this
            // platform has no code for that key" is none of them. The engine already
            // reports `UNRESOLVED` for a key its own resolution could not place, which
            // is the accurate word and has its own sentence; emitting `NO_PERMISSION`
            // here told somebody to grant a permission that had nothing to do with it.
            warn(&format!(
                "{} keys or buttons in this batch have no code on this platform",
                out.unresolved
            ));
        }
        if out.calls.is_empty() {
            return;
        }
        let Some(session) = self.remote_session() else {
            // Nothing was typed, and a review found that the first version said nothing
            // either: after one refusal every later batch was dropped in silence, with
            // no refusal upcall and no log line, which is the exact failure this
            // component exists to prevent. Said every time rather than once, because
            // every batch is a keystroke somebody made.
            self.emit(BackendEvent::Refused(Refusal::NoPermission));
            return;
        };
        for call in &out.calls {
            if let Err(e) = self.portal.remote_notify(&session, call) {
                // The first failure ends the batch: the rest of a resolved sequence
                // is a modifier dance whose halves must not be split, and sending
                // the tail into a session that has just refused the head would leave
                // keys held on the far machine.
                warn(&format!("typing on this computer failed: {e}"));
                self.remote = None;
                self.remote_refused = Some(e.clone());
                // `NoPermission` whatever went wrong, and it is the accurate one of
                // the four this seam defines: there is no elevated window here, no
                // secure input field and no separate locked desktop, so every way a
                // portal can refuse to type is the compositor withholding permission.
                self.emit(BackendEvent::Refused(Refusal::NoPermission));
                self.publish();
                return;
            }
        }
    }

    /// The injection session, asked for on first use.
    fn remote_session(&mut self) -> Option<SessionId> {
        if let Some(remote) = &self.remote {
            return Some(remote.session.clone());
        }
        if self.remote_refused.is_some() {
            return None;
        }
        match self.portal.remote_start(caps::WANTED) {
            Ok(grant) => {
                if grant.devices & caps::KEYBOARD == 0 {
                    // Granted without a keyboard: nothing can be typed, so the
                    // session is closed rather than held open and useless.
                    warn("the injection session was granted without a keyboard");
                    self.portal.close(&grant.session);
                    self.remote_refused = Some(PortalError::Refused { cancelled: false });
                    self.publish();
                    return None;
                }
                let session = grant.session.clone();
                self.remote = Some(grant);
                Some(session)
            }
            Err(e) => {
                warn(&format!("no injection session: {e}"));
                self.remote_refused = Some(e);
                self.publish();
                None
            }
        }
    }

    /// A capture failure the engine has to hear about.
    fn lost(&mut self, why: &PortalError) {
        warn(&format!("the capture session failed: {why}"));
        self.emit(BackendEvent::CaptureLost(match why {
            PortalError::Refused { .. } => CaptureLoss::Permission,
            PortalError::SessionClosed => CaptureLoss::SessionEnded,
            _ => CaptureLoss::Broken,
        }));
        self.publish();
    }

    /// Recomputes the capabilities and the screens from where the session has got
    /// to, and tells the engine if either moved.
    fn publish(&mut self) {
        let monitors = monitors_from(&self.capture.zones().zones);
        let mut caps = self.report.capabilities(Gate::On);
        // What the desktop OFFERS, narrowed by what this session has actually got.
        // A dead session claims nothing, whatever the portal advertises: the
        // interface must not say this machine can drive while its session is gone.
        if let Phase::Dead(why) = self.capture.phase() {
            caps.capture = false;
            caps.swallow = false;
            caps.confine = false;
            caps.monitors_stable = false;
            caps.problem = Some(why.problem());
        }
        if let Some(why) = &self.remote_refused {
            caps.inject_keys = false;
            caps.unicode = false;
            // Only if there is nothing worse to say: a capture that died outranks an
            // injection session that was refused, because the first is this machine
            // losing a keyboard it had. `monitors_unstable` does NOT outrank it, and
            // that took a test to notice: it is the standing word of every healthy
            // Wayland session, so treating it as "something to say" hid every refusal
            // behind it.
            if matches!(caps.problem, None | Some(Problem::MonitorsUnstable)) {
                caps.problem = Some(why.problem());
            }
        }
        // No session, so no screens: `GetZones` needs one. Reported honestly rather
        // than as a stale arrangement, which would have this machine's siblings
        // crossing towards screens whose positions nobody has confirmed.
        if monitors.is_empty() {
            caps.monitors_stable = false;
        }

        let caps_moved = match self.caps.lock() {
            Ok(mut slot) => {
                let moved = *slot != caps;
                *slot = caps;
                moved
            }
            Err(p) => {
                let mut slot = p.into_inner();
                let moved = *slot != caps;
                *slot = caps;
                moved
            }
        };
        let monitors_moved = match self.monitors.lock() {
            Ok(mut slot) => {
                let moved = *slot != monitors;
                *slot = monitors;
                moved
            }
            Err(p) => {
                let mut slot = p.into_inner();
                let moved = *slot != monitors;
                *slot = monitors;
                moved
            }
        };
        if monitors_moved {
            self.emit(BackendEvent::MonitorsChanged);
        } else if caps_moved {
            self.emit(BackendEvent::CapabilitiesChanged);
        }
    }

    /// Hands an upcall to the engine.
    ///
    /// # A moment may be dropped, a FACT may not
    ///
    /// The loop must keep moving, so it never blocks on a full channel: a burst of
    /// pointer motion at 125 Hz can fill all 256 slots while the engine is stalled. But
    /// the things that fill it are moments (this motion, this keystroke), and behind
    /// them may be a fact about the world (`CaptureLost`, `CapabilitiesChanged`,
    /// `MonitorsChanged`) that is still true a moment later and whose loss has a long
    /// tail: a dropped `CaptureLost` leaves the engine believing it still holds a
    /// capture that is gone.
    ///
    /// So a fact that will not fit is HELD, one per kind, and retried on the next turn.
    /// This is the X11 backend's rule, arrived at there for the same reason, and a
    /// review is what made this loop follow it: the first version dropped everything
    /// alike and logged it.
    fn emit(&mut self, event: BackendEvent) {
        // Whatever was held first, so the order the engine sees is the order things
        // happened in.
        let mut still_waiting = Vec::new();
        for held in std::mem::take(&mut self.deferred) {
            if let Err(mpsc::error::TrySendError::Full(back)) = self.events.try_send(held) {
                still_waiting.push(back);
            }
        }
        self.deferred = still_waiting;
        if let Err(e) = self.events.try_send(event) {
            let (dropped, closed) = match e {
                mpsc::error::TrySendError::Full(event) => (event, false),
                mpsc::error::TrySendError::Closed(event) => (event, true),
            };
            if closed {
                // Every receiver is gone, so the engine is. Nothing to hold it for.
                warn(&format!("the engine is gone, so {dropped:?} went nowhere"));
                return;
            }
            match dropped {
                BackendEvent::CaptureLost(_)
                | BackendEvent::CapabilitiesChanged
                | BackendEvent::MonitorsChanged => {
                    // One per kind: a second `MonitorsChanged` behind a first says
                    // nothing the first does not, and the engine re-reads the world when
                    // it gets either.
                    if !self.deferred.iter().any(|held| {
                        std::mem::discriminant(held) == std::mem::discriminant(&dropped)
                    }) {
                        warn(&format!("the engine is behind, so {dropped:?} is held"));
                        self.deferred.push(dropped);
                    }
                }
                // A moment. It is gone, and saying so is all that can be done: a
                // held pointer position is a stale pointer position.
                _ => warn(&format!("the engine is behind, so {dropped:?} was dropped")),
            }
        }
    }
}

// --------------------------------------------------------------- construction

/// Builds the Wayland backend, or says precisely why it cannot be built.
///
/// Never a panic and never an optimistic success: the caller
/// ([`crate::os::create`]) turns the error into an [`crate::os::Absent`] backend
/// carrying the reason, which is what puts the sentence on screen.
pub fn create(kind: SessionKind) -> Result<crate::os::Created, Unsupported> {
    let gate = Gate::from_env();
    let portal = match crate::wayland_portal::connect() {
        Ok(portal) => portal,
        Err(e) => {
            warn(&format!("{e}"));
            return Err(Unsupported(e.problem()));
        }
    };
    let report = probe(portal.as_ref(), kind);
    warn(&format!(
        "{INPUT_CAPTURE}: {:?}; {REMOTE_DESKTOP}: {:?}",
        report.capture, report.inject
    ));
    // The probe is the whole of the decision, and it asks for nothing: no session is
    // created here and no dialog is raised, which is why it can run at start-up on
    // every Linux machine there is.
    //
    // **The test is whether EITHER half is usable, not both**, and a review caught the
    // first version getting that wrong. `Report::problem` answers `Some` whenever either
    // half failed, by its own documented rule, so refusing on `Some` refused a desktop
    // with one working half: Hyprland, which has the capture portal and no injection
    // one, got the absent backend and could neither drive nor be driven, while the
    // CHANGELOG promised in so many words that it "can drive and cannot be driven". The
    // capability bits are what say which half, and they only exist if a backend exists
    // to report them.
    let caps = report.capabilities(gate);
    if !caps.can_drive() && !caps.can_be_driven() {
        return Err(Unsupported(
            report.problem(gate).unwrap_or(Problem::Wayland),
        ));
    }
    warn(&format!(
        "{} is set, so the Wayland path is being built. Nothing on it has ever been \
         run against a real compositor.",
        crate::os::WAYLAND_ENV
    ));
    Ok(build(portal, report))
}

/// Assembles the backend, its loop and the channel between them.
///
/// Infallible on purpose: everything that could refuse has already refused, in the
/// probe, and a construction that could fail here would need a second reason code for
/// a state the interface has no sentence for.
fn build(portal: Arc<dyn Portal>, report: Report) -> crate::os::Created {
    let queue = Arc::new(Queue::default());
    let (events, backend_events) = mpsc::channel(BACKEND_EVENT_CAPACITY);
    // The capabilities the desktop offers, before any session exists. Narrowed by
    // the loop's first `publish` and by every one after it, never widened past this.
    let caps = Arc::new(Mutex::new(report.capabilities(Gate::On)));
    let monitors = Arc::new(Mutex::new(Vec::new()));
    let handle = WaylandBackend {
        queue: Arc::clone(&queue),
        caps: Arc::clone(&caps),
        monitors: Arc::clone(&monitors),
    };
    let looper = WaylandLoop {
        capture: Capture::new(Arc::clone(&portal)),
        portal,
        queue,
        events,
        caps,
        monitors,
        report,
        want: CaptureMode::Off,
        remote: None,
        remote_refused: None,
        warp_to: None,
        deferred: Vec::new(),
        exit: None,
    };
    crate::os::Created {
        handle: crate::os::Backend::Wayland(handle),
        backend_events,
        event_loop: crate::os::EventLoop::Wayland(Box::new(looper)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// A portal that answers from a script and writes down what it was asked. Every
    /// test's whole world, and the reason the state machine above is provable at all.
    #[derive(Default)]
    struct FakePortal {
        properties: Mutex<HashMap<(String, String), Result<u32, PortalError>>>,
        /// Every call, in order, as a short word. What a test asserts on when the
        /// question is "did it do this exactly once".
        calls: Mutex<Vec<String>>,
        /// What the scripted portal answers.
        script: Mutex<Script>,
    }

    /// The answers a fake portal is set up to give.
    struct Script {
        capture_capabilities: u32,
        create: Option<PortalError>,
        connect: Option<PortalError>,
        zones: ZoneSet,
        zones_error: Option<PortalError>,
        refuse: Vec<u32>,
        set_barriers_error: Option<PortalError>,
        enable_error: Option<PortalError>,
        release_error: Option<PortalError>,
        /// The events the EI stream hands over on its first poll.
        ei: Vec<EiEvent>,
        ei_gone: bool,
        remote_devices: u32,
        remote_error: Option<PortalError>,
        notify_error: Option<PortalError>,
        signals: Vec<Signal>,
    }

    impl Default for Script {
        fn default() -> Script {
            Script {
                capture_capabilities: caps::WANTED,
                create: None,
                connect: None,
                zones: ZoneSet {
                    zones: vec![Zone {
                        w: 1920,
                        h: 1080,
                        x: 0,
                        y: 0,
                    }],
                    serial: 7,
                },
                zones_error: None,
                refuse: Vec::new(),
                set_barriers_error: None,
                enable_error: None,
                release_error: None,
                ei: Vec::new(),
                ei_gone: false,
                remote_devices: caps::WANTED,
                remote_error: None,
                notify_error: None,
                signals: Vec::new(),
            }
        }
    }

    /// The scripted event stream.
    struct FakeEi {
        events: Vec<EiEvent>,
        gone: bool,
    }

    impl EiSource for FakeEi {
        fn poll(&mut self, _budget: std::time::Duration) -> EiPoll {
            EiPoll {
                events: std::mem::take(&mut self.events),
                gone: self.gone,
            }
        }
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

        /// An `Arc` because that is what [`Capture`] holds, and the test keeps a
        /// clone of its own so it can read back what the portal was asked.
        fn scripted(edit: impl FnOnce(&mut Script)) -> std::sync::Arc<FakePortal> {
            let fake = FakePortal::default();
            edit(&mut fake.script.lock().expect("fresh lock"));
            std::sync::Arc::new(fake)
        }

        fn note(&self, what: &str) {
            self.calls
                .lock()
                .expect("fresh lock")
                .push(what.to_string());
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("fresh lock").clone()
        }

        fn counted(&self, what: &str) -> usize {
            self.calls().iter().filter(|c| c.as_str() == what).count()
        }

        fn session(&self) -> SessionId {
            SessionId("/org/freedesktop/portal/desktop/session/1_1/onedevice".to_string())
        }

        fn push_signal(&self, signal: Signal) {
            self.script.lock().expect("fresh lock").signals.push(signal);
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

        fn capture_create_session(&self, want: u32) -> Result<CaptureGrant, PortalError> {
            self.note("create");
            let script = self.script.lock().expect("fresh lock");
            if let Some(e) = &script.create {
                return Err(e.clone());
            }
            Ok(CaptureGrant {
                session: self.session(),
                capabilities: script.capture_capabilities & want,
            })
        }

        fn capture_zones(&self, _session: &SessionId) -> Result<ZoneSet, PortalError> {
            self.note("zones");
            let script = self.script.lock().expect("fresh lock");
            match &script.zones_error {
                Some(e) => Err(e.clone()),
                None => Ok(script.zones.clone()),
            }
        }

        fn capture_set_barriers(
            &self,
            _session: &SessionId,
            serial: u32,
            barriers: &[Barrier],
        ) -> Result<Vec<u32>, PortalError> {
            self.note(&format!("barriers:{serial}:{}", barriers.len()));
            let script = self.script.lock().expect("fresh lock");
            match &script.set_barriers_error {
                Some(e) => Err(e.clone()),
                None => Ok(script.refuse.clone()),
            }
        }

        fn capture_connect(&self, _session: &SessionId) -> Result<Box<dyn EiSource>, PortalError> {
            self.note("connect");
            let script = self.script.lock().expect("fresh lock");
            match &script.connect {
                Some(e) => Err(e.clone()),
                None => Ok(Box::new(FakeEi {
                    events: script.ei.clone(),
                    gone: script.ei_gone,
                })),
            }
        }

        fn capture_enable(&self, _session: &SessionId) -> Result<(), PortalError> {
            self.note("enable");
            match &self.script.lock().expect("fresh lock").enable_error {
                Some(e) => Err(e.clone()),
                None => Ok(()),
            }
        }

        fn capture_disable(&self, _session: &SessionId) -> Result<(), PortalError> {
            self.note("disable");
            Ok(())
        }

        fn capture_release(
            &self,
            _session: &SessionId,
            activation: u32,
            cursor: Option<(f64, f64)>,
        ) -> Result<(), PortalError> {
            self.note(&format!("release:{activation}:{cursor:?}"));
            match &self.script.lock().expect("fresh lock").release_error {
                Some(e) => Err(e.clone()),
                None => Ok(()),
            }
        }

        fn remote_start(&self, want: u32) -> Result<RemoteGrant, PortalError> {
            self.note("remote_start");
            let script = self.script.lock().expect("fresh lock");
            match &script.remote_error {
                Some(e) => Err(e.clone()),
                None => Ok(RemoteGrant {
                    session: self.session(),
                    devices: script.remote_devices & want,
                }),
            }
        }

        fn remote_notify(&self, _session: &SessionId, what: &Notify) -> Result<(), PortalError> {
            self.note(&format!("notify:{what:?}"));
            match &self.script.lock().expect("fresh lock").notify_error {
                Some(e) => Err(e.clone()),
                None => Ok(()),
            }
        }

        fn close(&self, _session: &SessionId) {
            self.note("close");
        }

        fn take_signals(&self) -> Vec<Signal> {
            std::mem::take(&mut self.script.lock().expect("fresh lock").signals)
        }
    }

    // --- the barriers ------------------------------------------------------

    fn zone(x: i32, y: i32, w: u32, h: u32) -> Zone {
        Zone { w, h, x, y }
    }

    /// Every barrier is horizontal or vertical, sits on its own zone's boundary, and
    /// has a unique non-zero id. Asserted for every arrangement the tests below build,
    /// because the interface refuses anything else and a refused barrier is an edge
    /// the pointer can never leave through.
    fn check_invariants(zones: &[Zone], placed: &[Placed]) {
        let mut ids: Vec<u32> = placed.iter().map(|p| p.barrier.id).collect();
        assert!(ids.iter().all(|id| *id != 0), "zero is a reserved id");
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "two barriers share an id: {placed:?}");
        for p in placed {
            let b = p.barrier;
            // Horizontal or vertical, and a one-pixel barrier is legitimately both.
            // The interface supports nothing else, so a diagonal is not a barrier a
            // compositor refuses, it is one this code must never build.
            assert!(b.y1 == b.y2 || b.x1 == b.x2, "a diagonal barrier: {b:?}");
            assert!(
                b.x1 <= b.x2 && b.y1 <= b.y2,
                "a barrier drawn backwards: {b:?}"
            );
            let z = zones[p.zone];
            match p.side {
                Side::Top => assert_eq!(b.y1, z.y),
                Side::Bottom => assert_eq!(b.y1, z.y + z.h as i32),
                Side::Left => assert_eq!(b.x1, z.x),
                Side::Right => assert_eq!(b.x1, z.x + z.w as i32),
            }
        }
    }

    /// One screen: four barriers, one per edge, each the full width or height.
    #[test]
    fn one_screen_is_fenced_on_all_four_sides() {
        let zones = [zone(0, 0, 1920, 1080)];
        let placed = barriers_for(&zones);
        check_invariants(&zones, &placed);
        assert_eq!(placed.len(), 4, "{placed:?}");
        let by = |side: Side| {
            placed
                .iter()
                .find(|p| p.side == side)
                .map(|p| p.barrier)
                .expect("every side")
        };
        // The pixel convention: a horizontal barrier sits on the top edge of its
        // pixels, so the bottom boundary is at `y + h` and the last pixel it spans
        // is `x + w - 1`.
        assert_eq!(
            by(Side::Top),
            Barrier {
                id: by(Side::Top).id,
                x1: 0,
                y1: 0,
                x2: 1919,
                y2: 0
            }
        );
        assert_eq!(by(Side::Bottom).y1, 1080);
        assert_eq!((by(Side::Bottom).x1, by(Side::Bottom).x2), (0, 1919));
        assert_eq!(by(Side::Left).x1, 0);
        assert_eq!((by(Side::Left).y1, by(Side::Left).y2), (0, 1079));
        assert_eq!(by(Side::Right).x1, 1920);
    }

    /// **Two screens side by side get six barriers, not eight.** The edge they share
    /// is inside the union, and a barrier there would take the pointer away every
    /// time it moved between this machine's own screens.
    #[test]
    fn the_edge_between_two_of_this_machines_own_screens_carries_no_barrier() {
        let zones = [zone(0, 0, 1920, 1080), zone(1920, 0, 1920, 1080)];
        let placed = barriers_for(&zones);
        check_invariants(&zones, &placed);
        assert_eq!(placed.len(), 6, "{placed:?}");
        assert!(
            !placed.iter().any(|p| p.zone == 0 && p.side == Side::Right),
            "the left screen's right edge is inside the union"
        );
        assert!(
            !placed.iter().any(|p| p.zone == 1 && p.side == Side::Left),
            "and so is the right screen's left edge"
        );
        // The outer left and right edges are there, once each.
        assert_eq!(
            placed
                .iter()
                .filter(|p| p.side == Side::Left || p.side == Side::Right)
                .count(),
            2
        );
    }

    /// **A partially shared edge SPLITS.** The case the clipping exists for: a small
    /// screen under the left part of a big one. The big screen's bottom edge is
    /// covered for 1280 pixels and exposed for 640, so it becomes one barrier over the
    /// exposed part and none over the rest.
    ///
    /// Emitting the whole edge would have the compositor take the pointer in the
    /// middle of this machine's own desktop; emitting nothing would swallow it at a
    /// real boundary.
    #[test]
    fn an_edge_that_is_only_partly_shared_becomes_a_barrier_over_the_exposed_part() {
        let zones = [zone(0, 0, 1920, 1080), zone(0, 1080, 1280, 720)];
        let placed = barriers_for(&zones);
        check_invariants(&zones, &placed);
        let bottoms: Vec<Barrier> = placed
            .iter()
            .filter(|p| p.zone == 0 && p.side == Side::Bottom)
            .map(|p| p.barrier)
            .collect();
        assert_eq!(bottoms.len(), 1, "one exposed stretch: {bottoms:?}");
        assert_eq!((bottoms[0].x1, bottoms[0].x2), (1280, 1919));
        assert_eq!(bottoms[0].y1, 1080);
        // And the small screen's top edge is entirely covered, so it has none.
        assert!(!placed.iter().any(|p| p.zone == 1 && p.side == Side::Top));
    }

    /// An edge with a neighbour in the middle of it splits into TWO barriers. The
    /// arrangement that catches an implementation which only ever trims the ends.
    #[test]
    fn a_neighbour_in_the_middle_of_an_edge_splits_it_in_two() {
        let zones = [zone(0, 0, 3000, 1000), zone(1000, 1000, 1000, 1000)];
        let placed = barriers_for(&zones);
        check_invariants(&zones, &placed);
        let mut bottoms: Vec<(i32, i32)> = placed
            .iter()
            .filter(|p| p.zone == 0 && p.side == Side::Bottom)
            .map(|p| (p.barrier.x1, p.barrier.x2))
            .collect();
        bottoms.sort_unstable();
        assert_eq!(bottoms, vec![(0, 999), (2000, 2999)], "{placed:?}");
    }

    /// **Overlapping zones produce no barrier in the interior of the union**, and a
    /// review found the first version producing four of them. A compositor reports
    /// overlapping zones for real: one pixel of fractional-scale rounding is enough.
    #[test]
    fn zones_that_overlap_are_still_fenced_only_on_the_outside() {
        // Half overlapping, side by side.
        let zones = [zone(0, 0, 100, 100), zone(50, 0, 100, 100)];
        let placed = barriers_for(&zones);
        check_invariants(&zones, &placed);
        assert!(
            !placed.iter().any(|p| p.zone == 0 && p.side == Side::Right),
            "x=100 is inside the second zone: {placed:?}"
        );
        assert!(
            !placed.iter().any(|p| p.zone == 1 && p.side == Side::Left),
            "x=50 is inside the first zone: {placed:?}"
        );
        assert_eq!(placed.len(), 6);

        // One pixel of overlap, stacked. The rounding artefact case.
        let zones = [zone(0, 0, 1920, 1080), zone(0, 1079, 1920, 1080)];
        let placed = barriers_for(&zones);
        check_invariants(&zones, &placed);
        assert!(!placed.iter().any(|p| p.zone == 0 && p.side == Side::Bottom));
        assert!(!placed.iter().any(|p| p.zone == 1 && p.side == Side::Top));
        assert_eq!(placed.len(), 6);
    }

    /// **A zone entirely inside another gets no barrier at all**: every one of its
    /// edges is in the interior. Four interior barriers is what the first version
    /// produced, right in the middle of somebody's main screen.
    #[test]
    fn a_screen_reported_inside_another_gets_no_barrier_of_its_own() {
        let zones = [zone(0, 0, 1920, 1080), zone(100, 100, 200, 200)];
        let placed = barriers_for(&zones);
        check_invariants(&zones, &placed);
        assert_eq!(
            placed.len(),
            4,
            "only the outer screen is fenced: {placed:?}"
        );
        assert!(placed.iter().all(|p| p.zone == 0));
    }

    /// A mirrored pair, which a compositor reports as two identical zones, is fenced
    /// once. Twice would have the compositor watching one edge under two ids, and an
    /// `Activated` could name either.
    #[test]
    fn a_mirrored_pair_reported_twice_is_fenced_once() {
        let zones = [zone(0, 0, 1280, 720), zone(0, 0, 1280, 720)];
        let placed = barriers_for(&zones);
        check_invariants(&zones, &placed);
        assert_eq!(placed.len(), 4, "{placed:?}");
    }

    /// Two screens that do not touch keep all eight edges: nothing is inside the
    /// union between them, so the gap is a real boundary in both directions.
    #[test]
    fn screens_with_a_gap_between_them_keep_every_edge() {
        let zones = [zone(0, 0, 800, 600), zone(2000, 0, 800, 600)];
        let placed = barriers_for(&zones);
        check_invariants(&zones, &placed);
        assert_eq!(placed.len(), 8);
    }

    /// Screens that touch on an edge but do not OVERLAP along it share nothing: a
    /// diagonal arrangement is two separate boundaries, not one shared edge.
    #[test]
    fn screens_touching_only_at_a_corner_share_no_edge() {
        let zones = [zone(0, 0, 1000, 1000), zone(1000, 1000, 1000, 1000)];
        let placed = barriers_for(&zones);
        check_invariants(&zones, &placed);
        assert_eq!(placed.len(), 8, "{placed:?}");
    }

    /// A zone of no size is skipped: it has no boundary to leave through and a
    /// barrier on it is one the compositor refuses.
    #[test]
    fn a_zone_of_no_size_gets_no_barrier_and_hides_nothing() {
        let zones = [zone(0, 0, 1920, 1080), zone(1920, 0, 0, 1080)];
        let placed = barriers_for(&zones);
        check_invariants(&zones, &placed);
        assert_eq!(placed.len(), 4, "only the real screen is fenced");
        assert!(
            placed.iter().any(|p| p.side == Side::Right),
            "and an empty neighbour does not cover the real screen's edge"
        );
        assert!(barriers_for(&[]).is_empty(), "no zones, no barriers");
    }

    /// A compositor reporting a zone that runs off the end of the coordinate space
    /// must not overflow the arithmetic. The barrier that comes out is clamped and
    /// the compositor refuses it, which is a path that already exists; a panic here
    /// would be a component death holding somebody's keyboard.
    #[test]
    fn an_absurd_zone_saturates_rather_than_overflowing() {
        let zones = [zone(i32::MAX - 10, i32::MAX - 10, u32::MAX, u32::MAX)];
        let placed = barriers_for(&zones);
        assert_eq!(placed.len(), 4);
        for p in &placed {
            assert!(p.barrier.x1 <= p.barrier.x2 && p.barrier.y1 <= p.barrier.y2);
        }
    }

    /// The range algebra on its own, because the barrier cases above cannot reach
    /// every shape of it: cuts that overlap each other, cuts in any order, a cut
    /// bigger than the span, and a span that vanishes.
    #[test]
    fn subtracting_covered_stretches_handles_overlap_and_any_order() {
        assert_eq!(subtract((0, 10), &[]), vec![(0, 10)]);
        assert_eq!(subtract((0, 10), &[(0, 10)]), vec![]);
        assert_eq!(subtract((0, 10), &[(-5, 15)]), vec![]);
        assert_eq!(subtract((0, 10), &[(3, 6)]), vec![(0, 3), (6, 10)]);
        assert_eq!(
            subtract((0, 10), &[(6, 8), (2, 4)]),
            vec![(0, 2), (4, 6), (8, 10)],
            "the cuts are sorted here rather than assumed sorted"
        );
        assert_eq!(
            subtract((0, 10), &[(2, 6), (4, 8)]),
            vec![(0, 2), (8, 10)],
            "overlapping cuts merge"
        );
        assert_eq!(
            subtract((0, 10), &[(3, 3)]),
            vec![(0, 10)],
            "an empty cut covers nothing"
        );
        assert_eq!(subtract((5, 5), &[]), vec![], "an empty span has no parts");
        assert_eq!(
            subtract((7, 3), &[]),
            vec![],
            "and neither has a backwards one"
        );
    }

    /// **The cursor position an `Activated` carries is regularly outside the zones**,
    /// because the barriers are at their edges and a fast hand carries the pointer
    /// past one. The interface says adjusting it is the application's job.
    #[test]
    fn a_pointer_that_flew_past_the_barrier_comes_back_to_the_corner_it_left_by() {
        let zones = [zone(0, 0, 1920, 1080)];
        assert_eq!(clamp_into(&zones, (1000.0, 500.0)), Some((1000.0, 500.0)));
        assert_eq!(
            clamp_into(&zones, (2500.0, -80.0)),
            Some((1919.0, 0.0)),
            "past the top right corner comes back to that corner, not to the middle"
        );
        assert_eq!(
            clamp_into(&zones, (960.0, 1200.0)),
            Some((960.0, 1079.0)),
            "the last addressable row, not the boundary"
        );
        // The nearest zone wins, so a pointer leaving the right-hand screen does not
        // reappear on the left one.
        let two = [zone(0, 0, 1000, 1000), zone(2000, 0, 1000, 1000)];
        assert_eq!(clamp_into(&two, (2500.0, 2000.0)), Some((2500.0, 999.0)));
        assert_eq!(clamp_into(&two, (1400.0, 500.0)), Some((999.0, 500.0)));
        assert_eq!(clamp_into(&[], (1.0, 1.0)), None);
        assert_eq!(clamp_into(&[zone(0, 0, 0, 0)], (1.0, 1.0)), None);
    }

    /// The zones become monitors with exactly one primary and distinct identities,
    /// which is what the plane needs of them. What they cannot become is a stable
    /// identity, and the doc comment on `monitors_from` is where that is written down.
    #[test]
    fn the_zones_become_screens_with_one_anchor_and_no_two_alike() {
        let zones = [
            zone(1920, 0, 1280, 1024),
            zone(0, 0, 1920, 1080),
            zone(0, 0, 0, 0),
        ];
        let monitors = monitors_from(&zones);
        assert_eq!(monitors.len(), 2, "the empty zone is not a screen");
        assert_eq!(monitors.iter().filter(|m| m.primary).count(), 1);
        assert!(
            monitors.iter().find(|m| m.primary).expect("one").x == 0,
            "the screen at the origin anchors the arrangement"
        );
        let mut ids: Vec<&str> = monitors.iter().map(|m| m.id.as_str()).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len());
        for m in &monitors {
            assert!(m.w > 0 && m.h > 0 && m.scale == 1000 && !m.name.is_empty());
        }
        // No zone at the origin: the first one anchors instead, and there is still
        // exactly one.
        let offset = monitors_from(&[zone(100, 100, 800, 600), zone(900, 100, 800, 600)]);
        assert_eq!(offset.iter().filter(|m| m.primary).count(), 1);
    }

    // --- typing ------------------------------------------------------------

    /// **The absolute pointer move this platform cannot make is counted, not
    /// swallowed.** It should never arrive (`inject_pointer` is false, so the engine
    /// never sends one), which is exactly why a silent drop would be invisible for
    /// ever.
    #[test]
    fn an_absolute_pointer_move_is_counted_rather_than_disappearing() {
        use crate::backend::{Action, Point};
        let out = inject(&[
            Action::MoveTo(Point { x: 10, y: 20 }),
            Action::MoveBy { dx: 3, dy: -4 },
        ]);
        assert_eq!(out.absolute_dropped, 1);
        assert_eq!(
            out.calls,
            vec![Notify::PointerMotion { dx: 3.0, dy: -4.0 }],
            "the relative one goes through untouched"
        );
    }

    /// The wheel: this engine says positive is up, the portal says positive is down,
    /// so the vertical value is negated and the horizontal one is not. A stepped
    /// scroll is one call per axis, and a scroll of nothing is no call.
    #[test]
    fn the_wheel_is_flipped_into_the_portals_convention_and_split_by_axis() {
        use crate::backend::Action;
        assert_eq!(
            inject(&[Action::Wheel {
                dx: 5,
                dy: 12,
                pixels: true
            }])
            .calls,
            vec![Notify::PointerAxis { dx: 5.0, dy: -12.0 }]
        );
        assert_eq!(
            inject(&[Action::Wheel {
                dx: 0,
                dy: 1,
                pixels: false
            }])
            .calls,
            vec![Notify::PointerAxisDiscrete {
                axis: AXIS_VERTICAL,
                steps: -1
            }]
        );
        assert_eq!(
            inject(&[Action::Wheel {
                dx: -2,
                dy: 3,
                pixels: false
            }])
            .calls,
            vec![
                Notify::PointerAxisDiscrete {
                    axis: AXIS_VERTICAL,
                    steps: -3
                },
                Notify::PointerAxisDiscrete {
                    axis: AXIS_HORIZONTAL,
                    steps: -2
                },
            ],
            "one call per axis, vertical first"
        );
        assert!(
            inject(&[Action::Wheel {
                dx: 0,
                dy: 0,
                pixels: false
            }])
            .calls
            .is_empty(),
            "a scroll of nothing is not an event"
        );
    }

    /// The buttons, both ways, and the thumb buttons in particular: back and forward
    /// go out as the codes a real mouse sends and come in from either spelling.
    #[test]
    fn the_five_buttons_go_out_as_evdev_and_come_back_from_either_spelling() {
        for (n, code) in [
            (1u8, btn::LEFT),
            (2, btn::MIDDLE),
            (3, btn::RIGHT),
            (4, btn::SIDE),
            (5, btn::EXTRA),
        ] {
            assert_eq!(button_to_evdev(n), Some(code));
            assert_eq!(button_from_evdev(code), Some(n));
        }
        assert_eq!(button_from_evdev(btn::BACK), Some(4));
        assert_eq!(button_from_evdev(btn::FORWARD), Some(5));
        assert_eq!(button_to_evdev(0), None);
        assert_eq!(button_to_evdev(6), None);
        // A button this engine does not carry is dropped rather than given an
        // invented number: the far side checks the number against the same closed
        // set, so an invented one would be refused a hop later with less context.
        assert_eq!(button_from_evdev(0x110 + 100), None);
        assert_eq!(button_from_evdev(0), None);

        use crate::backend::Action;
        let out = inject(&[
            Action::Button {
                button: 1,
                down: true,
            },
            Action::Button {
                button: 9,
                down: true,
            },
        ]);
        assert_eq!(out.unresolved, 1, "and a number off the set is counted");
        assert_eq!(out.calls.len(), 1);
    }

    /// **Text becomes keysyms, which is the one thing Wayland does better than
    /// X11**: any character at all, on any layout, with no keymap and nothing to
    /// bind. Latin-1 code points are their own keysyms and everything else takes the
    /// Unicode escape.
    #[test]
    fn any_character_types_as_a_keysym_with_no_keymap_at_all() {
        assert_eq!(keysym_of('a'), 0x61);
        assert_eq!(keysym_of(' '), 0x20);
        assert_eq!(keysym_of('~'), 0x7E);
        assert_eq!(keysym_of('\u{e9}'), 0xE9, "Latin-1 is its own keysym");
        assert_eq!(keysym_of('\u{ff}'), 0xFF);
        assert_eq!(
            keysym_of('\u{100}'),
            0x0100_0100,
            "and beyond it, the escape"
        );
        assert_eq!(keysym_of('\u{20ac}'), 0x0100_20AC, "the euro sign");
        assert_eq!(keysym_of('\u{1f600}'), 0x0101_F600);
        assert_eq!(keysym_of('\n'), 0x0100_000A, "a control code has no keysym");

        use crate::backend::Action;
        let out = inject(&[Action::Text("h\u{e9}".to_string())]);
        assert_eq!(
            out.calls,
            vec![
                Notify::KeyboardKeysym {
                    keysym: 0x68,
                    pressed: true
                },
                Notify::KeyboardKeysym {
                    keysym: 0x68,
                    pressed: false
                },
                Notify::KeyboardKeysym {
                    keysym: 0xE9,
                    pressed: true
                },
                Notify::KeyboardKeysym {
                    keysym: 0xE9,
                    pressed: false
                },
            ],
            "press and release, per character, in order"
        );
    }

    /// A resolution: a position and a named key answer, a symbol never does, and the
    /// engine's Unicode path is what covers the difference.
    #[test]
    fn a_position_resolves_and_a_symbol_deliberately_does_not() {
        use crate::backend::Want;
        let a =
            resolve(&Want::Usage(keys::usage(keys::PAGE_KEYBOARD, 0x04))).expect("the A position");
        assert_eq!(a.code.code, 30, "KEY_A");
        assert_eq!(a.code.detail, 0);
        assert_eq!(a.mods, 0);
        assert!(a.prefix.is_none() && a.group.is_none());

        assert_eq!(
            resolve(&Want::Named("Enter".to_string()))
                .expect("Enter")
                .code
                .code,
            28
        );
        assert!(
            resolve(&Want::Symbol("@".to_string())).is_none(),
            "there is no keymap to ask, so a guess would type the wrong character"
        );
        assert!(resolve(&Want::Named("NoSuchKey".to_string())).is_none());
        assert!(
            resolve(&Want::Usage(keys::usage(0x99, 0x01))).is_none(),
            "a page this build has no table for"
        );
    }

    /// Every key in the engine's frozen table has an evdev code, and every code maps
    /// back to exactly the usage it came from. A table with a hole types nothing for
    /// that key; a table that is not a bijection types the wrong key on the way back.
    ///
    /// It also pins the relation between the two Linux platforms, which is the one
    /// fact this file borrows from the other: **the evdev code and the X keycode differ
    /// by exactly 8**, for every entry, so a change to either side of the shared table
    /// fails here.
    #[test]
    fn every_named_key_and_every_modifier_has_a_code_that_round_trips() {
        for (name, usage) in keys::NAMED {
            let code = evdev_of_usage(*usage)
                .unwrap_or_else(|| panic!("{name} ({usage:#x}) has no evdev code"));
            assert_eq!(
                usage_of_evdev(code),
                Some(*usage),
                "{name} does not come back from {code}"
            );
        }
        // The five holdable modifiers, which the engine looks up by bit rather than by
        // name.
        for bit in [
            keys::mods::SHIFT,
            keys::mods::CTRL,
            keys::mods::ALT,
            keys::mods::ALTGR,
            keys::mods::META,
        ] {
            let usage = keys::mod_usage(bit).expect("every holdable bit names a key");
            let code = evdev_of_usage(usage).expect("and every one has a code");
            assert_eq!(usage_of_evdev(code), Some(usage));
        }

        // The letters and the digits, spot-checked against the numbers in
        // `linux/input-event-codes.h`, so the delegation cannot silently start
        // answering from somewhere else.
        for (usage_id, evdev) in [(0x04u32, 30u32), (0x1D, 44), (0x1E, 2), (0x27, 11)] {
            assert_eq!(
                evdev_of_usage(keys::usage(keys::PAGE_KEYBOARD, usage_id)),
                Some(evdev)
            );
        }

        // A code above what the shared table can even represent answers nothing rather
        // than wrapping into a byte and naming the wrong key. The EI stream really can
        // carry one: evdev numbering runs to 0x2FF.
        assert_eq!(usage_of_evdev(0x2FF), None);
        assert_eq!(usage_of_evdev(256), None);
        assert_eq!(usage_of_evdev(u32::MAX), None);
        assert_eq!(evdev_of_usage(keys::usage(0x99, 0x01)), None);
    }

    /// The XKB modifier masks as this dialect's bits, including the two locks, which
    /// are states rather than held keys.
    #[test]
    fn the_compositors_modifier_mask_becomes_this_dialects_bits() {
        assert_eq!(mods_of_xkb(0), 0);
        assert_eq!(mods_of_xkb(xkb::SHIFT), keys::mods::SHIFT);
        assert_eq!(mods_of_xkb(xkb::CONTROL), keys::mods::CTRL);
        assert_eq!(mods_of_xkb(xkb::MOD1), keys::mods::ALT);
        assert_eq!(mods_of_xkb(xkb::MOD5), keys::mods::ALTGR);
        assert_eq!(mods_of_xkb(xkb::MOD4), keys::mods::META);
        assert_eq!(mods_of_xkb(xkb::LOCK), keys::mods::CAPS);
        assert_eq!(mods_of_xkb(xkb::MOD2), keys::mods::NUM);
        assert_eq!(
            mods_of_xkb(xkb::SHIFT | xkb::CONTROL | xkb::MOD5),
            keys::mods::SHIFT | keys::mods::CTRL | keys::mods::ALTGR
        );
        // A bit no keymap this build knows uses, and the whole mask: nothing outside
        // the dialect's own eight is ever produced.
        assert_eq!(mods_of_xkb(1 << 5), 0, "Mod3 has no canonical meaning");
        assert_eq!(mods_of_xkb(u32::MAX) & !keys::mods::DEFINED, 0);
    }

    // --- the decoder -------------------------------------------------------

    fn activated_decoder() -> Decoder {
        let mut d = Decoder::default();
        d.activated(42, crate::backend::Point { x: 100, y: 200 });
        assert!(
            d.decode(EiEvent::StartEmulating { sequence: 42 }).is_none(),
            "the confirmation is not itself an event"
        );
        d
    }

    /// **The fraction of a pixel is carried, or a slow hand moves the pointer not at
    /// all.** EI carries `f32` deltas and a compositor applying an acceleration
    /// profile sends fractions constantly; truncating each independently loses every
    /// movement below a pixel.
    #[test]
    fn ten_movements_of_a_third_of_a_pixel_move_the_pointer_three_pixels() {
        let mut d = activated_decoder();
        let mut reported = 0;
        for _ in 0..10 {
            if let Some(crate::backend::BackendEvent::Motion(m)) =
                d.decode(EiEvent::Motion { dx: 0.3, dy: 0.0 })
            {
                reported += m.dx;
            }
        }
        assert_eq!(
            reported, 3,
            "ten times a third is three, not zero and not ten"
        );
        assert_eq!(d.at().x, 103);
        assert_eq!(d.at().y, 200);
    }

    /// A movement that rounds to nothing is not an event: the engine never emits a
    /// relative move of nothing either, and Windows discards one outright.
    #[test]
    fn a_movement_of_less_than_a_pixel_is_not_an_event_yet() {
        let mut d = activated_decoder();
        assert!(d.decode(EiEvent::Motion { dx: 0.4, dy: 0.4 }).is_none());
        assert_eq!(d.at(), crate::backend::Point { x: 100, y: 200 });
        // And the remainder is still there to be spent.
        let event = d.decode(EiEvent::Motion { dx: 0.7, dy: 0.7 });
        assert!(matches!(
            event,
            Some(crate::backend::BackendEvent::Motion(m)) if m.dx == 1 && m.dy == 1
        ));
    }

    /// **The sequence gate: nothing is reported until the portal's activation and the
    /// EI stream's sequence agree.** Two sockets, so they arrive out of order, and a
    /// fast hand can cross an edge twice before either has settled. Without this,
    /// the previous crossing's queued movement lands on the new one and sends the
    /// pointer to the wrong machine.
    #[test]
    fn events_are_dropped_until_the_two_sockets_agree_about_the_activation() {
        let mut d = Decoder::default();
        // Before any activation at all.
        assert!(d.decode(EiEvent::Motion { dx: 9.0, dy: 9.0 }).is_none());
        assert!(d.decode(EiEvent::StartEmulating { sequence: 1 }).is_none());
        assert!(
            d.decode(EiEvent::Motion { dx: 9.0, dy: 9.0 }).is_none(),
            "a stream emulating an activation this side never heard of is not believed"
        );

        // The portal says one number and the stream says another.
        d.activated(7, crate::backend::Point::default());
        assert!(
            d.decode(EiEvent::Motion { dx: 9.0, dy: 9.0 }).is_none(),
            "activated is not enough: the stream has not confirmed"
        );
        assert!(d.decode(EiEvent::StartEmulating { sequence: 8 }).is_none());
        assert!(
            d.decode(EiEvent::Motion { dx: 9.0, dy: 9.0 }).is_none(),
            "a mismatched sequence stays shut"
        );

        // And now they agree.
        assert!(d.decode(EiEvent::StartEmulating { sequence: 7 }).is_none());
        assert!(d.decode(EiEvent::Motion { dx: 9.0, dy: 9.0 }).is_some());

        // The stream stops, and so does the reporting.
        assert!(d.decode(EiEvent::StopEmulating).is_none());
        assert!(d.decode(EiEvent::Motion { dx: 9.0, dy: 9.0 }).is_none());

        // So does the portal ending it.
        d.activated(9, crate::backend::Point::default());
        assert!(d.decode(EiEvent::StartEmulating { sequence: 9 }).is_none());
        assert!(d.decode(EiEvent::Motion { dx: 1.0, dy: 1.0 }).is_some());
        d.deactivated();
        assert!(d.decode(EiEvent::Motion { dx: 1.0, dy: 1.0 }).is_none());
    }

    /// The activation id WRAPS and its increment is unspecified, so every comparison
    /// is equality. A gate that compared with an ordering would refuse every
    /// activation after the counter turned over.
    #[test]
    fn an_activation_id_that_wrapped_round_is_still_believed() {
        let mut d = Decoder::default();
        d.activated(u32::MAX, crate::backend::Point::default());
        assert!(
            d.decode(EiEvent::StartEmulating { sequence: u32::MAX })
                .is_none()
        );
        assert!(d.decode(EiEvent::Motion { dx: 2.0, dy: 0.0 }).is_some());
        // And the next one, which is a smaller number.
        d.activated(0, crate::backend::Point::default());
        assert!(d.decode(EiEvent::StartEmulating { sequence: 0 }).is_none());
        assert!(d.decode(EiEvent::Motion { dx: 2.0, dy: 0.0 }).is_some());
    }

    /// The modifiers are tracked whether or not the activation is confirmed, because
    /// they are state and not an event: without this the first key of every session
    /// would travel with no modifiers on it.
    #[test]
    fn the_modifiers_are_learned_before_the_first_key_even_arrives() {
        let mut d = Decoder::default();
        assert!(
            d.decode(EiEvent::Modifiers {
                depressed: xkb::SHIFT,
                latched: 0,
                locked: xkb::LOCK,
            })
            .is_none()
        );
        assert_eq!(d.mods(), keys::mods::SHIFT | keys::mods::CAPS);

        // **And a new activation starts with none of them.** The first version carried
        // the mask across, and EI sends one only while emulating, so the first key of a
        // crossing wore the PREVIOUS crossing's modifiers: a Control the user released
        // long ago fires a shortcut on the target instead of typing a letter.
        d.activated(1, crate::backend::Point::default());
        assert_eq!(d.mods(), 0, "a fresh crossing holds nothing");
        assert!(d.decode(EiEvent::StartEmulating { sequence: 1 }).is_none());
        assert!(
            d.decode(EiEvent::Modifiers {
                depressed: xkb::CONTROL,
                latched: 0,
                locked: 0,
            })
            .is_none()
        );
        let Some(crate::backend::BackendEvent::Key(k)) = d.decode(EiEvent::Key {
            keycode: 30,
            down: true,
        }) else {
            panic!("a key");
        };
        assert_eq!(
            k.mods,
            keys::mods::CTRL,
            "the compositor's own word for THIS crossing, and nothing left over"
        );
    }

    /// A key becomes a position and a name, and never a symbol. A key the table does
    /// not know still TRAVELS, with no position and no name, so the far side refuses
    /// it with a sentence rather than it vanishing here.
    #[test]
    fn a_key_travels_as_a_position_and_a_name_and_never_as_a_character() {
        let mut d = activated_decoder();
        let Some(crate::backend::BackendEvent::Key(a)) = d.decode(EiEvent::Key {
            keycode: 30,
            down: true,
        }) else {
            panic!("a key");
        };
        assert_eq!(a.usage, keys::usage(keys::PAGE_KEYBOARD, 0x04));
        assert_eq!(
            a.key, None,
            "the A key produces a character, so it has no name"
        );
        assert_eq!(
            a.sym, None,
            "and its character is not knowable on this platform"
        );
        assert!(a.down && !a.lock);

        let Some(crate::backend::BackendEvent::Key(enter)) = d.decode(EiEvent::Key {
            keycode: 28,
            down: false,
        }) else {
            panic!("a key");
        };
        assert_eq!(enter.key.as_deref(), Some("Enter"));
        assert!(!enter.down);

        // A half-duplex lock, whose press has no release to come.
        let Some(crate::backend::BackendEvent::Key(caps)) = d.decode(EiEvent::Key {
            keycode: 58,
            down: true,
        }) else {
            panic!("a key");
        };
        assert!(caps.lock, "Caps Lock reports only its press");

        // And a key nothing in the table names.
        let Some(crate::backend::BackendEvent::Key(unknown)) = d.decode(EiEvent::Key {
            keycode: 0x2FF,
            down: true,
        }) else {
            panic!("an unknown key must still travel");
        };
        assert_eq!(unknown.usage, 0);
        assert_eq!(unknown.key, None);
    }

    /// The scroll, both kinds, with the sign flipped back into this engine's
    /// convention and the notch divisor applied.
    #[test]
    fn the_scroll_comes_back_in_this_engines_own_convention() {
        let mut d = activated_decoder();
        assert_eq!(
            d.decode(EiEvent::ScrollDelta { dx: 3.4, dy: 12.0 }),
            Some(crate::backend::BackendEvent::Wheel {
                dx: 3,
                dy: -12,
                pixels: true
            })
        );
        assert_eq!(
            d.decode(EiEvent::ScrollDelta { dx: 0.2, dy: 0.2 }),
            None,
            "a scroll that rounds to nothing is not an event"
        );
        assert_eq!(
            d.decode(EiEvent::ScrollDiscrete {
                dx: 0,
                dy: SCROLL_STEP
            }),
            Some(crate::backend::BackendEvent::Wheel {
                dx: 0,
                dy: -1,
                pixels: false
            })
        );
        assert_eq!(
            d.decode(EiEvent::ScrollDiscrete {
                dx: 0,
                dy: SCROLL_STEP / 2
            }),
            None,
            "half a notch is not a notch"
        );
    }

    /// A button off the closed set is dropped by the decoder too, and the ones on it
    /// come through.
    #[test]
    fn a_button_this_engine_does_not_carry_is_dropped_on_the_way_in() {
        let mut d = activated_decoder();
        assert_eq!(
            d.decode(EiEvent::Button {
                button: btn::RIGHT,
                down: true
            }),
            Some(crate::backend::BackendEvent::Button {
                button: 3,
                down: true
            })
        );
        assert_eq!(
            d.decode(EiEvent::Button {
                button: 0x140,
                down: true
            }),
            None,
            "a tablet stylus is not one of the five"
        );
    }

    // --- the capture lifecycle ---------------------------------------------

    /// Arming: one session, one connection, one set of barriers, one enable, in that
    /// order, and the connection BEFORE the enable because the interface requires it.
    /// Asking twice does not ask a person twice.
    #[test]
    fn arming_asks_for_consent_once_and_connects_before_it_enables() {
        let portal = FakePortal::scripted(|_| {});
        let mut capture = Capture::new(portal.clone());
        capture.arm().expect("armed");
        assert_eq!(capture.phase(), &Phase::Armed);
        assert_eq!(
            portal.calls(),
            vec![
                "create".to_string(),
                "connect".to_string(),
                "zones".to_string(),
                "barriers:7:4".to_string(),
                "enable".to_string(),
            ],
            "the EI connection is opened before Enable, which the interface requires"
        );

        // Again: nothing happens, because asking again asks for permission that has
        // already been given.
        capture.arm().expect("still armed");
        assert_eq!(portal.counted("create"), 1);
        assert_eq!(portal.counted("enable"), 1);
    }

    /// **A session granted without the pointer is closed rather than armed.** It
    /// would arm, watch nothing, and leave the interface saying this machine can
    /// drive: the grant is always a subset of the request, so this is a real answer
    /// and not a hypothetical one.
    #[test]
    fn a_grant_with_no_pointer_is_closed_instead_of_watching_nothing() {
        let portal = FakePortal::scripted(|s| s.capture_capabilities = caps::KEYBOARD);
        let mut capture = Capture::new(portal.clone());
        let e = capture.arm().expect_err("refused");
        assert_eq!(e.problem(), Problem::WaylandPortalRefused);
        assert!(matches!(capture.phase(), Phase::Dead(_)));
        assert_eq!(portal.counted("close"), 1, "and the session is closed");
        assert_eq!(portal.counted("enable"), 0);
    }

    /// Every barrier refused is the same failure: armed with nothing to watch is
    /// worse than not armed, because the pointer would never leave and the interface
    /// would say it could.
    #[test]
    fn a_desktop_that_refuses_every_barrier_is_not_armed_at_all() {
        let portal = FakePortal::scripted(|s| s.refuse = (1..=4).collect());
        let mut capture = Capture::new(portal.clone());
        capture.arm().expect_err("nothing to watch");
        assert!(matches!(capture.phase(), Phase::Dead(_)));
        assert_eq!(portal.counted("enable"), 0);
        assert_eq!(portal.counted("close"), 1);

        // A desktop with NO zones is the same answer by the same route.
        let portal = FakePortal::scripted(|s| s.zones = ZoneSet::default());
        Capture::new(portal.clone()).arm().expect_err("no screens");
    }

    /// A barrier the compositor refused is remembered as refused and forgotten as
    /// placed, so an `Activated` can never be attributed to one that was never there.
    #[test]
    fn a_refused_barrier_is_remembered_as_refused_and_not_as_placed() {
        let portal = FakePortal::scripted(|s| s.refuse = vec![2]);
        let mut capture = Capture::new(portal.clone());
        capture.arm().expect("armed with three of four");
        assert_eq!(capture.refused(), &[2]);
        assert_eq!(capture.placed().len(), 3);
        assert!(!capture.placed().iter().any(|p| p.barrier.id == 2));
    }

    /// The connection failing closes the session rather than leaving it armed and
    /// mute. A portal answering "EIS fd is not ready" is a real case and a retryable
    /// one, so it must not look like a person's refusal.
    #[test]
    fn a_session_with_no_event_stream_is_closed_rather_than_left_mute() {
        let portal = FakePortal::scripted(|s| {
            s.connect = Some(PortalError::Failed {
                name: "org.freedesktop.portal.Error.Failed".into(),
                message: "EIS fd is not ready".into(),
            })
        });
        let mut capture = Capture::new(portal.clone());
        let e = capture.arm().expect_err("no stream");
        assert!(e.worth_retrying());
        assert_eq!(portal.counted("close"), 1);
        assert_eq!(portal.counted("enable"), 0);
        assert_eq!(capture.phase(), &Phase::Idle, "and nothing was left behind");
    }

    /// The activation, from both sides: the portal says where the pointer went and
    /// the position is pulled back inside the zones before anybody uses it.
    #[test]
    fn an_activation_places_the_pointer_inside_the_screens_it_flew_out_of() {
        let portal = FakePortal::scripted(|_| {});
        let mut capture = Capture::new(portal.clone());
        capture.arm().expect("armed");
        let id = capture.placed()[0].barrier.id;
        assert!(!capture.signal(&Signal::Activated {
            session: portal.session(),
            activation: 5,
            // Well past the right hand edge, which is the ordinary case.
            cursor: Some((2400.0, -30.0)),
            barrier: Some(id),
        }));
        assert_eq!(capture.phase(), &Phase::Active { activation: 5 });
        assert_eq!(
            capture.decoder().at(),
            crate::backend::Point { x: 1919, y: 0 }
        );
    }

    /// **A stale `Deactivated` changes nothing.** The interface warns that one can
    /// arrive after a `Disable` or a `Release`, and acting on it would end the
    /// activation that had just begun.
    #[test]
    fn a_deactivated_for_a_previous_crossing_does_not_end_the_current_one() {
        let portal = FakePortal::scripted(|_| {});
        let mut capture = Capture::new(portal.clone());
        capture.arm().expect("armed");
        capture.signal(&Signal::Activated {
            session: portal.session(),
            activation: 12,
            cursor: Some((10.0, 10.0)),
            barrier: None,
        });
        capture.signal(&Signal::Deactivated {
            session: portal.session(),
            activation: 11,
        });
        assert_eq!(
            capture.phase(),
            &Phase::Active { activation: 12 },
            "the previous activation's late word is not this one's"
        );
        capture.signal(&Signal::Deactivated {
            session: portal.session(),
            activation: 12,
        });
        assert_eq!(capture.phase(), &Phase::Armed);
    }

    /// **An activation with no cursor position is given straight back**, because the
    /// engine decides the crossing from that position and inventing one picks the wrong
    /// edge or the wrong screen in silence. The interface documents the key as always
    /// present, so its absence is a compositor this build cannot follow.
    #[test]
    fn an_activation_that_does_not_say_where_the_pointer_is_is_given_straight_back() {
        let portal = FakePortal::scripted(|_| {});
        let mut capture = Capture::new(portal.clone());
        capture.arm().expect("armed");
        capture.signal(&Signal::Activated {
            session: portal.session(),
            activation: 77,
            cursor: None,
            barrier: None,
        });
        assert_eq!(
            capture.phase(),
            &Phase::Armed,
            "not capturing, and not dead either: the barriers are still good"
        );
        assert_eq!(
            portal.counted("release:77:None"),
            1,
            "and the pointer went back: {:?}",
            portal.calls()
        );
    }

    /// An `Activated` that arrives while this side is not armed is refused: acting on
    /// it would start capturing against barriers this side does not believe in.
    #[test]
    fn an_activation_while_not_armed_is_refused_rather_than_obeyed() {
        let portal = FakePortal::scripted(|_| {});
        let mut capture = Capture::new(portal.clone());
        let signal = Signal::Activated {
            session: portal.session(),
            activation: 1,
            cursor: None,
            barrier: None,
        };
        capture.signal(&signal);
        assert_eq!(capture.phase(), &Phase::Idle);
        capture.arm().expect("armed");
        capture.disarm();
        assert_eq!(capture.phase(), &Phase::Suspended);
        capture.signal(&signal);
        assert_eq!(capture.phase(), &Phase::Suspended);
    }

    /// The zone serial rule: the serial always increases and may skip, so the one
    /// already held is worth no round trip and anything else is.
    #[test]
    fn zones_changing_asks_for_new_barriers_unless_the_serial_is_the_one_we_hold() {
        let portal = FakePortal::scripted(|_| {});
        let mut capture = Capture::new(portal.clone());
        capture.arm().expect("armed");
        assert!(
            !capture.signal(&Signal::ZonesChanged {
                session: portal.session(),
                serial: 7
            }),
            "the serial we already placed barriers against needs nothing"
        );
        assert_eq!(capture.phase(), &Phase::Armed);
        assert!(
            capture.signal(&Signal::ZonesChanged {
                session: portal.session(),
                serial: 40
            }),
            "a serial that skipped ahead still means the screens moved"
        );
        assert_eq!(capture.phase(), &Phase::Suspended);

        // And placing them again re-enables, because setting barriers suspends.
        portal.script.lock().expect("lock").zones.serial = 40;
        capture.rearm_barriers().expect("armed again");
        assert_eq!(capture.phase(), &Phase::Armed);
        assert_eq!(portal.counted("barriers:40:4"), 1);
        assert_eq!(portal.counted("enable"), 2, "one per arming");
        assert_eq!(portal.counted("create"), 1, "and nobody was asked twice");
    }

    /// Replacing the barriers while a capture is live ends that activation, because
    /// the interface says setting barriers suspends the session.
    #[test]
    fn replacing_the_barriers_ends_the_live_capture_the_interface_says_it_ends() {
        let portal = FakePortal::scripted(|_| {});
        let mut capture = Capture::new(portal.clone());
        capture.arm().expect("armed");
        capture.signal(&Signal::Activated {
            session: portal.session(),
            activation: 3,
            cursor: Some((10.0, 10.0)),
            barrier: None,
        });
        assert!(capture.phase().capturing());
        capture.rearm_barriers().expect("armed again");
        assert_eq!(capture.phase(), &Phase::Armed);
        assert!(!capture.phase().capturing());
    }

    /// The return: `Release` with the position pulled inside the zones, and the
    /// barriers left armed so the next crossing needs nothing.
    #[test]
    fn giving_the_pointer_back_suggests_a_position_inside_the_screens() {
        let portal = FakePortal::scripted(|_| {});
        let mut capture = Capture::new(portal.clone());
        capture.arm().expect("armed");
        capture.signal(&Signal::Activated {
            session: portal.session(),
            activation: 4,
            cursor: Some((10.0, 10.0)),
            barrier: None,
        });
        capture.stop(Some((5000.0, 5000.0)));
        assert_eq!(capture.phase(), &Phase::Armed);
        assert_eq!(
            portal.counted("release:4:Some((1919.0, 1079.0))"),
            1,
            "the suggestion is a place that exists: {:?}",
            portal.calls()
        );
        // Stopping again does nothing: there is no activation to release.
        capture.stop(None);
        assert_eq!(
            portal
                .calls()
                .iter()
                .filter(|c| c.starts_with("release"))
                .count(),
            1
        );
    }

    /// **A release that could not be delivered is not quiet.** The compositor is left
    /// holding the pointer, which is the one failure of this whole feature that must
    /// never be silent.
    #[test]
    fn a_release_that_could_not_be_delivered_kills_the_session_rather_than_pretending() {
        let portal = FakePortal::scripted(|s| {
            s.release_error = Some(PortalError::NoInterface {
                interface: INPUT_CAPTURE.into(),
            })
        });
        let mut capture = Capture::new(portal.clone());
        capture.arm().expect("armed");
        capture.signal(&Signal::Activated {
            session: portal.session(),
            activation: 1,
            cursor: Some((10.0, 10.0)),
            barrier: None,
        });
        capture.stop(None);
        assert!(
            matches!(capture.phase(), Phase::Dead(_)),
            "an undeliverable release is not something to carry on from"
        );

        // **A RETRYABLE failure ends it too**, and the first version of this test
        // asserted the opposite while its own name said this. A `Timeout` is the
        // likeliest failure of all against a busy compositor, and carrying on left the
        // compositor capturing while this side reported `Armed`: the decoder unconfirmed
        // so every event dropped, the late `Deactivated` ignored by rule 4, and no way
        // to ever release that activation again. A release we cannot confirm means we do
        // not know who holds the pointer.
        let portal = FakePortal::scripted(|s| s.release_error = Some(PortalError::Timeout));
        let mut capture = Capture::new(portal.clone());
        capture.arm().expect("armed");
        capture.signal(&Signal::Activated {
            session: portal.session(),
            activation: 1,
            cursor: Some((10.0, 10.0)),
            barrier: None,
        });
        capture.stop(None);
        assert!(
            matches!(capture.phase(), Phase::Dead(_)),
            "not knowing who holds the pointer is not a state to carry on from"
        );
    }

    /// **`close` reaches `Idle` from every phase and leaks nothing.** The one
    /// function that must not be able to fail: a capture that outlives its session is
    /// a machine whose keyboard belongs to nobody.
    #[test]
    fn closing_reaches_the_resting_state_from_every_phase_and_leaves_no_session() {
        // From armed.
        let portal = FakePortal::scripted(|_| {});
        let mut capture = Capture::new(portal.clone());
        capture.arm().expect("armed");
        capture.close();
        assert_eq!(capture.phase(), &Phase::Idle);
        assert_eq!(portal.counted("close"), 1);
        assert!(capture.placed().is_empty() && capture.zones().zones.is_empty());

        // From active: the release comes first, then the close.
        let portal = FakePortal::scripted(|_| {});
        let mut capture = Capture::new(portal.clone());
        capture.arm().expect("armed");
        capture.signal(&Signal::Activated {
            session: portal.session(),
            activation: 2,
            cursor: Some((10.0, 10.0)),
            barrier: None,
        });
        capture.close();
        assert_eq!(capture.phase(), &Phase::Idle);
        let calls = portal.calls();
        let release = calls.iter().position(|c| c.starts_with("release"));
        let close = calls.iter().position(|c| c == "close");
        assert!(
            release < close,
            "the pointer is given back before the door shuts"
        );

        // From idle, twice, and from dead: harmless in all three.
        let portal = FakePortal::scripted(|_| {});
        let mut capture = Capture::new(portal.clone());
        capture.close();
        capture.close();
        assert_eq!(capture.phase(), &Phase::Idle);
        assert_eq!(portal.counted("close"), 0, "there was nothing to close");

        let portal = FakePortal::scripted(|s| s.capture_capabilities = 0);
        let mut capture = Capture::new(portal.clone());
        capture.arm().expect_err("refused");
        capture.close();
        assert_eq!(capture.phase(), &Phase::Idle);
    }

    /// A session the compositor closed is `Dead` and not re-armed behind a person's
    /// back: re-arming means another dialog, which is a decision about their
    /// attention.
    #[test]
    fn a_session_the_compositor_closed_says_so_and_is_not_quietly_replaced() {
        let portal = FakePortal::scripted(|_| {});
        let mut capture = Capture::new(portal.clone());
        capture.arm().expect("armed");
        capture.signal(&Signal::Closed {
            session: portal.session(),
        });
        assert_eq!(capture.phase(), &Phase::Dead(PortalError::SessionClosed));
        assert_eq!(
            capture.arm().expect_err("dead"),
            PortalError::SessionClosed,
            "arming a dead session answers why rather than asking again"
        );
        assert_eq!(portal.counted("create"), 1);
        // And closing a session the compositor already took needs no call.
        capture.close();
        assert_eq!(portal.counted("close"), 0);
    }

    /// A signal for somebody else's session is not this session's business: one
    /// connection carries every session the process has.
    #[test]
    fn a_signal_for_another_session_is_ignored() {
        let portal = FakePortal::scripted(|_| {});
        let mut capture = Capture::new(portal.clone());
        capture.arm().expect("armed");
        let other = SessionId("/org/freedesktop/portal/desktop/session/1_1/other".into());
        for signal in [
            Signal::Activated {
                session: other.clone(),
                activation: 1,
                cursor: Some((10.0, 10.0)),
                barrier: None,
            },
            Signal::Disabled {
                session: other.clone(),
            },
            Signal::Closed {
                session: other.clone(),
            },
        ] {
            capture.signal(&signal);
            assert_eq!(capture.phase(), &Phase::Armed, "{signal:?}");
        }
        assert!(!capture.signal(&Signal::ZonesChanged {
            session: other,
            serial: 999
        }));
    }

    /// **The event stream going away ends the session.** It is the only way events
    /// can arrive, so a capture without it is a machine holding a keyboard it cannot
    /// read.
    #[test]
    fn the_event_stream_ending_ends_the_capture_that_depended_on_it() {
        let portal = FakePortal::scripted(|s| {
            s.ei = vec![
                EiEvent::StartEmulating { sequence: 6 },
                EiEvent::Motion { dx: 4.0, dy: 0.0 },
            ];
            s.ei_gone = true;
        });
        let mut capture = Capture::new(portal.clone());
        capture.arm().expect("armed");
        capture.signal(&Signal::Activated {
            session: portal.session(),
            activation: 6,
            cursor: Some((10.0, 10.0)),
            barrier: None,
        });
        let (events, gone) = capture.poll(std::time::Duration::from_millis(1));
        assert!(gone);
        assert_eq!(events.len(), 1, "what did arrive is still delivered");
        assert!(matches!(
            capture.phase(),
            Phase::Dead(PortalError::SessionClosed)
        ));
        assert_eq!(portal.counted("close"), 1);
    }

    /// Disarming keeps the session and its consent, because a session costs a person
    /// a dialog and enabling one again costs a round trip.
    #[test]
    fn disarming_keeps_the_consent_and_re_arming_costs_one_call() {
        let portal = FakePortal::scripted(|_| {});
        let mut capture = Capture::new(portal.clone());
        capture.arm().expect("armed");
        capture.disarm();
        assert_eq!(capture.phase(), &Phase::Suspended);
        assert_eq!(portal.counted("disable"), 1);
        capture.arm().expect("armed again");
        assert_eq!(capture.phase(), &Phase::Armed);
        assert_eq!(
            portal.counted("create"),
            1,
            "nobody was asked a second time"
        );
        assert_eq!(portal.counted("enable"), 2);
    }

    /// Disarming while live gives the pointer back first: a disable that left the
    /// compositor holding it would leave this machine's own keyboard dead.
    #[test]
    fn disarming_while_live_gives_the_pointer_back_before_it_stops_watching() {
        let portal = FakePortal::scripted(|_| {});
        let mut capture = Capture::new(portal.clone());
        capture.arm().expect("armed");
        capture.signal(&Signal::Activated {
            session: portal.session(),
            activation: 8,
            cursor: Some((10.0, 10.0)),
            barrier: None,
        });
        capture.disarm();
        let calls = portal.calls();
        assert!(
            calls.iter().position(|c| c.starts_with("release"))
                < calls.iter().position(|c| c == "disable")
        );
        assert_eq!(capture.phase(), &Phase::Suspended);
    }

    /// **Suspended means two different things and arming must tell them apart.**
    /// A `Disable` leaves the barriers good and one call away; a `ZonesChanged`
    /// leaves them worthless. Arming after the screens moved must place them again,
    /// or the compositor watches edges that are no longer there.
    #[test]
    fn arming_after_the_screens_moved_places_the_barriers_again() {
        let portal = FakePortal::scripted(|_| {});
        let mut capture = Capture::new(portal.clone());
        capture.arm().expect("armed");

        // Disabled: the barriers are fine, so arming is one Enable.
        capture.signal(&Signal::Disabled {
            session: portal.session(),
        });
        capture.arm().expect("armed again");
        assert_eq!(
            portal.counted("zones"),
            1,
            "nothing was re-read: {:?}",
            portal.calls()
        );
        assert_eq!(portal.counted("enable"), 2);

        // The screens moved: the barriers are worthless, so arming re-reads.
        portal.script.lock().expect("lock").zones.serial = 99;
        assert!(capture.signal(&Signal::ZonesChanged {
            session: portal.session(),
            serial: 99
        }));
        capture.arm().expect("armed against the new screens");
        assert_eq!(portal.counted("zones"), 2);
        assert_eq!(portal.counted("barriers:99:4"), 1);
        assert_eq!(capture.phase(), &Phase::Armed);
        assert_eq!(
            portal.counted("create"),
            1,
            "and still nobody was asked twice"
        );
    }

    /// A movement that is not a number is dropped and does not poison the carried
    /// fraction: `NaN` plus anything stays `NaN`, so one bad frame would stop the
    /// pointer for the rest of the session.
    #[test]
    fn a_movement_that_is_not_a_number_does_not_freeze_the_pointer_for_ever() {
        let mut d = activated_decoder();
        assert!(
            d.decode(EiEvent::Motion {
                dx: f32::NAN,
                dy: 0.0
            })
            .is_none()
        );
        assert!(
            d.decode(EiEvent::Motion {
                dx: f32::INFINITY,
                dy: f32::NEG_INFINITY
            })
            .is_none()
        );
        // And the very next real movement still works.
        assert!(matches!(
            d.decode(EiEvent::Motion { dx: 5.0, dy: 0.0 }),
            Some(crate::backend::BackendEvent::Motion(m)) if m.dx == 5
        ));
    }

    /// An interface too old names itself in its own failure, so a log line says
    /// which half of a desktop needs updating.
    #[test]
    fn a_version_below_the_floor_names_the_interface_it_is_talking_about() {
        let offer = Offer::Present {
            version: 0,
            capabilities: caps::WANTED,
        };
        let why = offer
            .why(INPUT_CAPTURE, INPUT_CAPTURE_MIN, caps::WANTED)
            .expect("too old");
        assert!(
            why.to_string().contains(INPUT_CAPTURE),
            "a failure that names nothing is a log line nobody can act on: {why}"
        );
        assert_eq!(why.problem(), Problem::WaylandPortalOld);
    }

    // --- the backend, against a scripted portal ----------------------------

    /// Spawns the real loop on a thread of its own with a scripted portal, and hands
    /// back the handle the engine would drive, the upcalls and the join.
    ///
    /// The loop is the one part of this module that cannot be a pure function (it owns
    /// a thread, a channel and a session), so it is exercised the way the other
    /// platform backends' loops are: for real, against a double for the only thing
    /// outside the process.
    fn spawn_loop(
        portal: std::sync::Arc<FakePortal>,
    ) -> (
        WaylandBackend,
        mpsc::Receiver<BackendEvent>,
        std::thread::JoinHandle<i32>,
    ) {
        let report = Report {
            session: SessionKind::Wayland,
            capture: Offer::Present {
                version: 1,
                capabilities: caps::WANTED,
            },
            inject: Offer::Present {
                version: 2,
                capabilities: caps::WANTED,
            },
        };
        let queue = Arc::new(Queue::default());
        let (events, rx) = mpsc::channel(BACKEND_EVENT_CAPACITY);
        let caps = Arc::new(Mutex::new(report.capabilities(Gate::On)));
        let monitors = Arc::new(Mutex::new(Vec::new()));
        let handle = WaylandBackend {
            queue: Arc::clone(&queue),
            caps: Arc::clone(&caps),
            monitors: Arc::clone(&monitors),
        };
        let arc: std::sync::Arc<dyn Portal> = portal;
        let (loop_caps, loop_monitors) = (Arc::clone(&caps), Arc::clone(&monitors));
        // Built INSIDE the thread, because a live capture holds its event stream and
        // that stream is not `Send`: in production the loop is built on the main
        // thread and stays there, which is the same rule from the other side.
        let join = std::thread::spawn(move || {
            WaylandLoop {
                capture: Capture::new(std::sync::Arc::clone(&arc)),
                portal: arc,
                queue,
                events,
                caps: loop_caps,
                monitors: loop_monitors,
                report,
                want: CaptureMode::Off,
                remote: None,
                remote_refused: None,
                warp_to: None,
                deferred: Vec::new(),
                exit: None,
            }
            .run()
        });
        (handle, rx, join)
    }

    /// Waits for a condition the loop reaches, bounded, so a failure is a failure and
    /// never a hung test.
    fn wait_for(mut done: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        done()
    }

    /// **Whatever ends the loop, the session is given back and closed.** A capture
    /// that outlives its process leaves the compositor holding a pointer nothing will
    /// ever release, which is the worst failure this component has.
    #[test]
    fn the_loop_gives_the_pointer_back_and_closes_the_session_on_its_way_out() {
        let portal = FakePortal::scripted(|_| {});
        let (handle, _events, join) = spawn_loop(std::sync::Arc::clone(&portal));
        handle.capture(CaptureMode::Swallow);
        assert!(
            wait_for(|| portal.counted("enable") == 1),
            "the loop arms on a capture downcall: {:?}",
            portal.calls()
        );
        handle.request_exit(3);
        assert_eq!(join.join().expect("the loop ended"), 3);
        assert_eq!(portal.counted("close"), 1, "{:?}", portal.calls());
        assert_eq!(portal.counted("create"), 1);
    }

    /// The engine's handle dying is the engine dying, and the loop tears down for it
    /// exactly as it does for an explicit exit. Without this the session would outlive
    /// a component whose engine had gone.
    #[test]
    fn the_loop_tears_down_when_every_handle_is_dropped() {
        let portal = FakePortal::scripted(|_| {});
        let (handle, _events, join) = spawn_loop(std::sync::Arc::clone(&portal));
        handle.capture(CaptureMode::Watch);
        assert!(wait_for(|| portal.counted("enable") == 1));
        drop(handle);
        assert_eq!(join.join().expect("the loop ended"), 0);
        assert_eq!(portal.counted("close"), 1);
    }

    /// Stopping the capture reaches the resting state, and the session is kept: it
    /// cost a person a dialog, and enabling one again costs a round trip.
    #[test]
    fn stopping_the_capture_keeps_the_consent_and_stops_the_capturing() {
        let portal = FakePortal::scripted(|_| {});
        let (handle, _events, join) = spawn_loop(std::sync::Arc::clone(&portal));
        handle.capture(CaptureMode::Swallow);
        assert!(wait_for(|| portal.counted("enable") == 1));
        handle.capture(CaptureMode::Off);
        assert!(
            wait_for(|| portal.counted("disable") == 1),
            "{:?}",
            portal.calls()
        );
        handle.capture(CaptureMode::Swallow);
        assert!(wait_for(|| portal.counted("enable") == 2));
        assert_eq!(portal.counted("create"), 1, "nobody was asked twice");
        handle.request_exit(0);
        join.join().expect("ended");
    }

    /// **The injection session is asked for on the first thing to type, and never at
    /// start-up.** Creating it raises a dialog, and a machine nobody is driving has no
    /// business asking for permission to be driven.
    #[test]
    fn nothing_asks_to_be_driven_until_something_is_actually_typed() {
        let portal = FakePortal::scripted(|_| {});
        let (handle, _events, join) = spawn_loop(std::sync::Arc::clone(&portal));
        handle.capture(CaptureMode::Watch);
        assert!(wait_for(|| portal.counted("enable") == 1));
        assert_eq!(
            portal.counted("remote_start"),
            0,
            "arming to DRIVE must not ask to BE driven"
        );

        handle.inject(vec![Action::Key {
            code: PlatformKey {
                code: 30,
                detail: 0,
            },
            down: true,
        }]);
        assert!(
            wait_for(|| portal.counted("remote_start") == 1),
            "{:?}",
            portal.calls()
        );
        assert!(
            portal
                .calls()
                .iter()
                .any(|c| c.contains("KeyboardKeycode") && c.contains("30")),
            "and the key really went out as an evdev keycode: {:?}",
            portal.calls()
        );

        // A second batch reuses the session: one dialog, not one per keystroke.
        handle.inject(vec![Action::Key {
            code: PlatformKey {
                code: 30,
                detail: 0,
            },
            down: false,
        }]);
        assert!(wait_for(|| portal
            .calls()
            .iter()
            .filter(|c| c.contains("KeyboardKeycode"))
            .count()
            == 2));
        assert_eq!(portal.counted("remote_start"), 1);
        handle.request_exit(0);
        join.join().expect("ended");
        assert_eq!(
            portal.counted("close"),
            2,
            "both sessions are closed on the way out: {:?}",
            portal.calls()
        );
    }

    /// **A refused injection session narrows the capabilities and stops asking.** The
    /// interface then says this machine cannot be driven, which is true, instead of a
    /// dialog appearing on every keystroke.
    #[test]
    fn a_refused_injection_session_is_said_once_and_not_asked_for_again() {
        let portal = FakePortal::scripted(|s| {
            s.remote_error = Some(PortalError::Refused { cancelled: true })
        });
        let (handle, _events, join) = spawn_loop(std::sync::Arc::clone(&portal));
        for _ in 0..3 {
            handle.inject(vec![Action::Text("a".to_string())]);
        }
        assert!(
            wait_for(|| !handle.capabilities().can_be_driven()),
            "the capabilities must say what just failed"
        );
        assert_eq!(
            handle.capabilities().problem,
            Some(Problem::WaylandPortalRefused),
            "the injection refusal outranks anything else there is to say"
        );
        assert_eq!(
            portal.counted("remote_start"),
            1,
            "asked once, not once per keystroke: {:?}",
            portal.calls()
        );
        // Arming again is the gesture that means "ask me once more".
        handle.capture(CaptureMode::Watch);
        handle.inject(vec![Action::Text("a".to_string())]);
        assert!(wait_for(|| portal.counted("remote_start") == 2));
        handle.request_exit(0);
        join.join().expect("ended");
    }

    /// A session granted without a keyboard is closed rather than held: nothing could
    /// be typed through it, and holding it would have the interface claim otherwise.
    #[test]
    fn an_injection_session_with_no_keyboard_is_closed_rather_than_kept() {
        let portal = FakePortal::scripted(|s| s.remote_devices = caps::POINTER);
        let (handle, _events, join) = spawn_loop(std::sync::Arc::clone(&portal));
        handle.inject(vec![Action::Text("a".to_string())]);
        assert!(wait_for(|| !handle.capabilities().can_be_driven()));
        assert!(wait_for(|| portal.counted("close") == 1));
        assert!(
            !portal.calls().iter().any(|c| c.contains("notify")),
            "and nothing was typed into it: {:?}",
            portal.calls()
        );
        handle.request_exit(0);
        join.join().expect("ended");
    }

    /// **A dead capture session claims nothing, whatever the desktop advertises.** The
    /// interface must not say this machine can drive while the session it would drive
    /// through is gone.
    #[test]
    fn a_capture_session_the_compositor_took_away_stops_claiming_it_can_drive() {
        let portal = FakePortal::scripted(|_| {});
        let (handle, _events, join) = spawn_loop(std::sync::Arc::clone(&portal));
        handle.capture(CaptureMode::Swallow);
        assert!(wait_for(|| portal.counted("enable") == 1));
        assert!(handle.capabilities().can_drive());

        portal.push_signal(Signal::Closed {
            session: portal.session(),
        });
        assert!(
            wait_for(|| !handle.capabilities().can_drive()),
            "a closed session is not a session"
        );
        assert_eq!(
            handle.capabilities().problem,
            Some(Problem::WaylandPortalRefused),
            "and it says why"
        );
        handle.request_exit(0);
        join.join().expect("ended");
    }

    /// The screens arrive with the session, because `GetZones` needs one, and the
    /// engine is told they moved.
    #[tokio::test]
    async fn the_screens_arrive_with_the_session_and_the_engine_is_told() {
        let portal = FakePortal::scripted(|_| {});
        let (handle, mut events, join) = spawn_loop(std::sync::Arc::clone(&portal));
        assert!(
            handle.monitors().await.is_empty(),
            "no session, so no screens: GetZones needs one"
        );
        handle.capture(CaptureMode::Watch);
        let mut told = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            match events.try_recv() {
                Ok(BackendEvent::MonitorsChanged) => {
                    told = true;
                    break;
                }
                Ok(_) => {}
                Err(_) => tokio::time::sleep(Duration::from_millis(5)).await,
            }
        }
        assert!(
            told,
            "the engine hears about the screens: {:?}",
            portal.calls()
        );
        let monitors = handle.monitors().await;
        assert_eq!(monitors.len(), 1);
        assert_eq!((monitors[0].w, monitors[0].h), (1920, 1080));
        handle.request_exit(0);
        join.join().expect("ended");
    }

    /// The pointer cannot be read on this platform and the backend says so rather than
    /// inventing a position: nothing in either portal answers where the pointer is.
    #[tokio::test]
    async fn this_platform_cannot_say_where_the_pointer_is_and_does_not_pretend() {
        let portal = FakePortal::scripted(|_| {});
        let (handle, _events, join) = spawn_loop(std::sync::Arc::clone(&portal));
        assert!(handle.pointer().await.is_none());
        // And a resolution needs no round trip at all, which is the compensation:
        // there is no keymap to consult, so the answer is a table lookup.
        assert!(
            handle
                .resolve(Want::Named("Enter".to_string()))
                .await
                .is_some()
        );
        handle.request_exit(0);
        join.join().expect("ended");
    }

    /// **The events really flow in the order the two sockets actually deliver them.**
    ///
    /// This is the test for the bug that would have made the whole feature deliver
    /// nothing. The EI socket is what wakes the loop, so its `start_emulating` normally
    /// arrives BEFORE the portal's `Activated`, and the first version threw an
    /// unmatched sequence away: the `Activated` that followed then waited for a
    /// confirmation that had been consumed and could not come again, so every event of
    /// every crossing was dropped for ever while the compositor held the pointer.
    #[test]
    fn the_stream_may_announce_a_crossing_before_the_portal_does() {
        let mut d = Decoder::default();
        // The EI socket first, which is the ordinary order.
        assert!(d.decode(EiEvent::StartEmulating { sequence: 31 }).is_none());
        assert!(
            d.decode(EiEvent::Motion { dx: 5.0, dy: 0.0 }).is_none(),
            "still nothing to report: the position comes from the portal"
        );
        // And now the portal catches up.
        d.activated(31, crate::backend::Point { x: 50, y: 50 });
        assert!(
            matches!(
                d.decode(EiEvent::Motion { dx: 5.0, dy: 0.0 }),
                Some(crate::backend::BackendEvent::Motion(m)) if m.dx == 5
            ),
            "the gate opens on what the stream already said"
        );

        // A DIFFERENT activation is still refused: remembering the number is not the
        // same as believing any number.
        let mut d = Decoder::default();
        assert!(d.decode(EiEvent::StartEmulating { sequence: 31 }).is_none());
        d.activated(32, crate::backend::Point::default());
        assert!(d.decode(EiEvent::Motion { dx: 5.0, dy: 0.0 }).is_none());

        // And a stream that has stopped forgets what it announced, so a later
        // activation with the same number does not open on stale evidence.
        let mut d = Decoder::default();
        assert!(d.decode(EiEvent::StartEmulating { sequence: 4 }).is_none());
        assert!(d.decode(EiEvent::StopEmulating).is_none());
        d.activated(4, crate::backend::Point::default());
        assert!(d.decode(EiEvent::Motion { dx: 5.0, dy: 0.0 }).is_none());
    }

    /// **A failure while placing the barriers closes the session.** It must not return
    /// with a live one: the engine re-arms, a second `CreateSession` raises a second
    /// consent dialog, the first session lives until the process exits, and a
    /// `Capture(Off)` from that state reaches `Suspended` with no barriers at all, which
    /// the next arming simply enables.
    #[test]
    fn a_failure_while_placing_the_barriers_closes_the_session_it_opened() {
        for script in [
            |s: &mut Script| s.zones_error = Some(PortalError::Timeout),
            |s: &mut Script| s.set_barriers_error = Some(PortalError::Timeout),
            |s: &mut Script| s.enable_error = Some(PortalError::Timeout),
        ] {
            let portal = FakePortal::scripted(script);
            let mut capture = Capture::new(portal.clone());
            capture.arm().expect_err("it could not arm");
            assert!(
                matches!(capture.phase(), Phase::Dead(_)),
                "{:?}",
                portal.calls()
            );
            assert_eq!(
                portal.counted("close"),
                1,
                "the session it opened is closed: {:?}",
                portal.calls()
            );

            // And from there nothing re-asks a person for consent behind their back,
            // nor reaches `Armed` with nothing to watch.
            capture.arm().expect_err("still dead");
            capture.disarm();
            assert!(matches!(capture.phase(), Phase::Dead(_)));
            assert_eq!(portal.counted("create"), 1);
        }
    }

    /// A dead session is not quietly resurrected by the engine's ordinary
    /// `Capture(Off)`, which is what it sends the moment the capabilities narrow.
    #[test]
    fn stopping_a_dead_session_does_not_forget_that_it_died() {
        let portal = FakePortal::scripted(|_| {});
        let mut capture = Capture::new(portal.clone());
        capture.arm().expect("armed");
        capture.signal(&Signal::Closed {
            session: portal.session(),
        });
        capture.disarm();
        assert_eq!(
            capture.phase(),
            &Phase::Dead(PortalError::SessionClosed),
            "disarming must not erase the reason, or the next publish re-widens the \
             capabilities and the engine asks for a fresh dialog on every cycle"
        );
        assert!(
            capture.zones().zones.is_empty(),
            "and the screens of a dead session are not still published"
        );
    }

    /// **A typing failure ends the batch, narrows the capabilities and says so**, and
    /// none of that had a test until a review pointed out that the fake's
    /// `notify_error` was declared and never set.
    #[test]
    fn a_typing_failure_stops_the_batch_and_says_this_machine_cannot_be_driven() {
        let portal = FakePortal::scripted(|s| {
            s.notify_error = Some(PortalError::Failed {
                name: "org.freedesktop.DBus.Error.Failed".into(),
                message: "the session is gone".into(),
            })
        });
        let (handle, mut events, join) = spawn_loop(std::sync::Arc::clone(&portal));
        handle.inject(vec![Action::Text("abc".to_string())]);
        assert!(
            wait_for(|| !handle.capabilities().can_be_driven()),
            "the capabilities must say it: {:?}",
            portal.calls()
        );
        let notifies = portal
            .calls()
            .iter()
            .filter(|c| c.starts_with("notify"))
            .count();
        assert_eq!(
            notifies,
            1,
            "the batch stops at the first failure rather than pushing the rest of a \
             sequence into a session that has just refused its head: {:?}",
            portal.calls()
        );
        // And a later batch is refused out loud rather than vanishing.
        handle.inject(vec![Action::Text("d".to_string())]);
        let mut refused = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline && !refused {
            match events.try_recv() {
                Ok(BackendEvent::Refused(_)) => refused = true,
                Ok(_) => {}
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        assert!(refused, "a keystroke that went nowhere is never silent");
        handle.request_exit(0);
        join.join().expect("ended");
    }

    /// **The last command before the engine lets go is not thrown away.**
    ///
    /// `main` pushes `request_exit(code)` and then drops the handle, and the engine's
    /// teardown pushes the release of every key a peer left down just before that. Both
    /// land while the loop is inside its poll. A liveness check that ran before the
    /// drain discarded them: the process reported 0 instead of the engine's code, and
    /// the keys stayed physically down with `held.json` already cleared, so the next run
    /// would not release them either.
    #[test]
    fn the_last_release_and_the_exit_code_survive_the_handle_being_dropped() {
        let portal = FakePortal::scripted(|_| {});
        let (handle, _events, join) = spawn_loop(std::sync::Arc::clone(&portal));
        handle.capture(CaptureMode::Watch);
        assert!(wait_for(|| portal.counted("enable") == 1));

        // Exactly what `main` does, in that order, and then nothing.
        handle.release_all(vec![PlatformKey {
            code: 29,
            detail: 0,
        }]);
        handle.request_exit(7);
        drop(handle);
        assert_eq!(
            join.join().expect("the loop ended"),
            7,
            "the engine's own exit code, not the backstop's zero"
        );
        assert!(
            portal
                .calls()
                .iter()
                .any(|c| c.contains("KeyboardKeycode") && c.contains("29")),
            "and the key a peer left down was released: {:?}",
            portal.calls()
        );
    }

    /// The signal queue drains, and the seam hands over whatever the portal said
    /// since it was last asked.
    #[test]
    fn the_portals_signals_drain_once_and_only_once() {
        let portal = FakePortal::scripted(|_| {});
        portal.push_signal(Signal::Disabled {
            session: portal.session(),
        });
        assert_eq!(portal.take_signals().len(), 1);
        assert!(portal.take_signals().is_empty());
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
            on.problem,
            Some(Problem::MonitorsUnstable),
            "switched on, the one thing left to say is that a zone carries no identity, \
             and this slot is the only place it can be said"
        );
        assert!(
            !on.monitors_stable,
            "an identity synthesised from a screen's size and position is not stable: \
             a different screen of the same size in the same place reads as the same one"
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
        assert_eq!(
            caps.problem,
            Some(Problem::MonitorsUnstable),
            "nothing about the portal is wrong, and the screens still have no identity"
        );
    }
}
