// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! **The file nobody has run.**
//!
//! The D-Bus and EI protocol code behind [`crate::wayland`]'s seams, and nothing
//! else. It is a separate file for one reason: the boundary between what has been
//! executed and what has not is the most important fact about this ticket, and a
//! boundary a reviewer can see beats a boundary they are told about.
//!
//! What HAS run, on the machine this was written on, and it is the load-bearing
//! half of the whole ticket: [`connect`] and [`Bus::property_u32`], against a real
//! `xdg-desktop-portal` 1.18.4 on a real Wayland session, answering with the real
//! errors that [`crate::wayland::classify`] is built around. That is how the
//! "this desktop does not have these portals" path is proven rather than asserted.
//!
//! What has NOT run: every other line. No machine available implemented either
//! portal, so the session calls, the barrier round trip and the EI stream have been
//! compiled, reviewed and unit tested against scripted doubles, and never once
//! spoken to a compositor. They are gated off behind
//! [`crate::os::WAYLAND_ENV`] for exactly that reason.
//!
//! # Why zbus, and why it costs nothing
//!
//! `zbus` 5.17.0 is ALREADY in this workspace's lock file, pulled in twice over by
//! `keyring` (the daemon's secret store) and by two Tauri plugins (the GUI). So
//! naming it here adds no package and no version, exactly as `xcb`,
//! `objc2-core-graphics` and `blake3` add none in the sibling backends: what is new
//! is a dependency edge. Its default features are taken as they are, unchanged,
//! precisely so that the feature set the rest of the workspace already resolves is
//! not disturbed by this line.
//!
//! The blocking API is used throughout. This backend owns a thread, as the other
//! three do, and a portal round trip on a warm session bus is about a millisecond:
//! an async seam would have bought nothing measurable and cost
//! [`crate::wayland`]'s state machine the property that makes it testable.
//!
//! # Why the property read is a raw `call_method` and not a `Proxy`
//!
//! `zbus::blocking::Proxy` is the ergonomic way and it does more than is wanted
//! here: it can cache properties and subscribe to `PropertiesChanged`, which for a
//! one-shot question asked once per process is machinery whose failure modes would
//! have to be understood before the answer could be trusted. This is the one call
//! whose behaviour against an absent interface the whole detection rests on, so it
//! is the plainest call that can be made: one message, one reply, one variant out.

use std::sync::Arc;

use crate::wayland::{PORTAL_BUS, PORTAL_PATH, Portal, PortalError, classify};

/// The D-Bus interface every property read goes through.
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";

/// A session bus connection, and the whole of this build's D-Bus state.
pub struct Bus {
    conn: zbus::blocking::Connection,
}

/// Opens the session bus.
///
/// The failure is [`PortalError::NoBus`] and not a panic: a Wayland session with no
/// session bus is a real configuration (a compositor started from a tty without
/// `dbus-run-session`) and it has its own reason code and its own sentence, because
/// its remedy is its own.
pub fn connect() -> Result<Arc<dyn Portal>, PortalError> {
    match zbus::blocking::Connection::session() {
        Ok(conn) => Ok(Arc::new(Bus { conn })),
        Err(e) => Err(PortalError::NoBus(e.to_string())),
    }
}

impl Bus {
    /// Turns a `zbus` error into this engine's vocabulary.
    ///
    /// A D-Bus method error keeps its name and its message and goes through
    /// [`classify`], which is where the empirical rules live. Anything else (a
    /// connection that died, a body that would not deserialise) is a transport
    /// failure rather than a portal's answer, and is reported as such rather than
    /// being dressed up as one of the portal's own refusals.
    fn error(&self, e: zbus::Error, interface: &str) -> PortalError {
        match &e {
            zbus::Error::MethodError(name, detail, _) => classify(
                name.as_str(),
                detail.as_deref().unwrap_or_default(),
                interface,
            ),
            zbus::Error::InputOutput(_) | zbus::Error::Address(_) => {
                PortalError::NoBus(e.to_string())
            }
            _ => PortalError::Malformed(e.to_string()),
        }
    }
}

impl Portal for Bus {
    fn property_u32(&self, interface: &str, property: &str) -> Result<u32, PortalError> {
        let reply = self
            .conn
            .call_method(
                Some(PORTAL_BUS),
                PORTAL_PATH,
                Some(PROPERTIES),
                "Get",
                &(interface, property),
            )
            .map_err(|e| self.error(e, interface))?;
        // `Properties.Get` answers a variant, so the body is `v` and the number is
        // inside it. Deserialised as an owned value: a borrowed one would hold the
        // message alive for the sake of a `u32`.
        let value: zvariant::OwnedValue = reply
            .body()
            .deserialize()
            .map_err(|e| PortalError::Malformed(format!("{interface}.{property}: {e}")))?;
        u32::try_from(value).map_err(|e| {
            // A portal answering the wrong TYPE for a documented property is this
            // build and that portal disagreeing about the interface, which is
            // `Malformed` and not a refusal: the difference decides whether a person
            // is told to try again.
            PortalError::Malformed(format!("{interface}.{property} is not a u32: {e}"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The witness: this machine's own session bus, asked for real.**
    ///
    /// The one test in the whole Wayland half that talks to something outside this
    /// process, and the reason the honest-degradation path is proven rather than
    /// asserted. It cannot demand a particular answer, because it runs on a
    /// developer's Wayland desktop, on a CI runner with no session bus at all, and
    /// one day on a machine that HAS these portals. What it demands is that every
    /// possible answer is one this build has a reason code and a sentence for, and
    /// that nothing here panics or hangs.
    ///
    /// On the machine it was written on it takes the third branch: a real bus, a
    /// real `xdg-desktop-portal`, and `org.freedesktop.portal.InputCapture` absent,
    /// classified through the `InvalidArgs` trap into
    /// [`crate::backend::Problem::WaylandNoPortal`].
    #[test]
    fn this_machines_own_bus_gives_an_answer_this_build_can_word() {
        let portal = match connect() {
            Err(e) => {
                // No session bus: a CI runner, or a machine with no desktop at all.
                assert_eq!(e.problem(), crate::backend::Problem::WaylandNoBus);
                assert!(!e.worth_retrying());
                return;
            }
            Ok(portal) => portal,
        };

        for interface in [
            crate::wayland::INPUT_CAPTURE,
            crate::wayland::REMOTE_DESKTOP,
        ] {
            match portal.property_u32(interface, "version") {
                Ok(version) => {
                    // A machine that HAS the portal. Nothing to assert about the
                    // number except that a portal reporting version 0 would be
                    // below every floor, which the negotiation already refuses.
                    assert!(version <= 100, "{interface} reported version {version}");
                }
                Err(e) => {
                    let problem = e.problem();
                    assert!(
                        crate::backend::Problem::ALL.contains(&problem),
                        "{interface}: {e} maps to a problem this build does not know"
                    );
                    assert!(!e.to_string().is_empty());
                }
            }
        }
    }
}
