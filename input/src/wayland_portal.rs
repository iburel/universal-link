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
//! Every protocol fact below is therefore sourced from the interface's own XML and
//! documentation, and the comments say so where it matters. Where a fact could only
//! come from a run, it is not claimed.
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
//!
//! # The `Request` race, and why the answer is one pump thread
//!
//! Every portal call that needs a person's consent answers immediately with an
//! object path and says the real answer LATER, as a `Response` signal on that
//! object. Which means there is a window: subscribe after the call returns and the
//! signal can already have been and gone. The interface names this as the reason its
//! `Request` paths became predictable rather than server-chosen: "This change was
//! made to let applications subscribe to the Response signal before making the
//! initial portal call, thereby avoiding a race condition."
//!
//! So the rule this file obeys is: **the match rule is registered with the bus, and
//! the path is on the waiting list, before the call is written to the socket.** It is
//! enforced structurally by [`Bus::request`], which is the only way a `Request` call
//! is ever made from here.
//!
//! One match rule and one thread, rather than one per interface, and the reason is a
//! property of `zbus` rather than of D-Bus: a `zbus::blocking::MessageIterator`
//! carries exactly ONE match rule and is consumed by blocking on `next()`, so three
//! narrow rules (`InputCapture`, `Session.Closed`, `Request.Response`) would have
//! meant three threads. A single rule keyed on the portal's bus name and on the
//! PATH NAMESPACE `/org/freedesktop/portal/desktop` covers all three, because every
//! object of all three interfaces lives at or under that path. The cost is the
//! portal's other signals (a settings change, a network state change) arriving to be
//! ignored, which is a handful of messages an hour.
//!
//! # What the match rule does NOT do: keep another process's signals out
//!
//! The rule names the portal as its sender, and the bus honours that for the signals
//! it BROADCASTS. It does not follow that everything reaching [`route`] came from the
//! portal, and two facts say so. A signal sent with a destination is delivered to that
//! destination whatever match rules it has registered; and `zbus`'s own client-side
//! re-check of the rule deliberately skips a well-known sender name, because "we can't
//! match against a well-known name" (its comment, and it is right: the message carries
//! the sender's unique name).
//!
//! So any process on this session bus can emit a directed
//! `org.freedesktop.portal.InputCapture.Activated` at this one and have it decoded and
//! filed. What that is worth to an attacker is bounded, and each bound is somewhere
//! else in the design rather than here:
//!
//! - **A forged `Response` cannot be aimed.** The path it would have to arrive on is
//!   built from an unguessable token, and [`Waiting::answers`] files an answer only for
//!   a path a caller put on the list. That is what the token's randomness is for.
//! - **A forged session signal reaches no session.** Every signal is matched against
//!   the live session handle by `crate::wayland::Capture::signal`, and the handles come
//!   from the portal's own `Response`, not from the signal.
//! - **The buffer is bounded** ([`file`]), so a flood costs a warning and some dropped
//!   crossings rather than memory.
//!
//! The residual is therefore a nuisance and not a compromise: a hostile local process
//! can make somebody's crossing fail to register. Rejecting it properly means resolving
//! the portal's unique name and checking every signal's sender against it, which needs
//! a `GetNameOwner` and a `NameOwnerChanged` subscription to stay correct across a
//! portal restart. That is a brick of its own and is on the deferred list, not a line
//! of this file.
//!
//! # The two budgets, and why silence is not the same as a slow person
//!
//! A portal that never answers must become [`PortalError::Timeout`] rather than a
//! wedged thread. But the size of the wait is not one number, because two very
//! different things are being waited for:
//!
//! - [`CONSENT_BUDGET`] covers the two calls a HUMAN answers (`InputCapture`'s
//!   `CreateSession` and `RemoteDesktop`'s `Start`, each of which raises a dialog).
//!   It is about somebody's attention: they may be reading the dialog, or looking
//!   for their glasses, or on the phone. Three minutes is longer than any dialog a
//!   person actually reads and short enough that a portal which crashed with the
//!   dialog up frees this backend's thread before anybody files a bug.
//! - [`ANSWER_BUDGET`] covers the round trips no person is in (`GetZones`,
//!   `SetPointerBarriers`, `RemoteDesktop`'s `CreateSession` and `SelectDevices`).
//!   Ten seconds is four orders of magnitude more than the millisecond these take on
//!   a warm bus, and it is chosen that loose on purpose: the number exists to end a
//!   hang, not to police latency.
//!
//! # Why the EI half is `reis`, and what that costs
//!
//! The capture portal transports not one keystroke: "The transport of actual input
//! events is delegated to a transport layer, specifically libei." So an EI client is
//! a REQUIREMENT of `InputCapture` rather than a choice, and the argument for the
//! crate is in `input/Cargo.toml` next to the line: pure Rust, no C library to
//! install, `#![forbid(unsafe_code)]`, one package in the lock.
//!
//! What it costs here is one honest caveat, recorded on [`EiStream::poll`]: `reis`
//! documents that its event converter PANICS on a poisoned internal mutex and on an
//! event carrying a timestamp with no device. That is handled rather than hoped
//! about, and the handling is argued where it lives.

use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::wayland::{
    Barrier, CaptureGrant, EiPoll, EiSource, INPUT_CAPTURE, Notify, PORTAL_BUS, PORTAL_PATH,
    Portal, PortalError, REMOTE_DESKTOP, RemoteGrant, SessionId, Signal, Zone, ZoneSet, classify,
    response, warn,
};

/// The D-Bus interface every property read goes through.
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
/// The interface the delayed answer to a portal call arrives on.
const REQUEST: &str = "org.freedesktop.portal.Request";
/// The interface a portal session is closed through, and says it closed on.
const SESSION: &str = "org.freedesktop.portal.Session";

/// The error NAME this build puts on a failure of its OWN, in the one arm
/// ([`PortalError::Failed`]) whose behaviour fits: not a missing piece, worth trying
/// again, and reported to a person as "asked and did not get it".
///
/// It carries no dot on purpose, so that a line in a log can never be mistaken for
/// something a compositor said: every real D-Bus error name is reverse-domain, and
/// this one is deliberately not a name of that shape.
const LOCAL: &str = "1device-input";

/// How long to wait for an answer a PERSON has to give. See the module header.
const CONSENT_BUDGET: Duration = Duration::from_secs(180);
/// How long to wait for an answer no person is in. See the module header.
const ANSWER_BUDGET: Duration = Duration::from_secs(10);

/// How many decoded signals may wait for [`Portal::take_signals`].
///
/// The engine drains them every turn of its loop, so the steady state is a handful.
/// The cap is for the pathological case (a compositor emitting `Activated` in a
/// loop, or an engine thread that has stopped draining) where an unbounded `Vec`
/// would be a memory leak driven by another process.
const SIGNAL_CAP: usize = 1024;

/// How many messages the bus may queue for the pump thread before it starts
/// dropping them. Generous because the pump does nothing but decode and file.
const QUEUE_DEPTH: usize = 256;

/// The `a{sv}` a portal answers a `Request` with, and the shape every result is read
/// out of.
type Vardict = HashMap<String, zvariant::OwnedValue>;

/// The `a{sv}` every portal call takes as its options.
type Options<'a> = HashMap<&'a str, zvariant::Value<'a>>;

/// A session bus connection, and the whole of this build's D-Bus state.
pub struct Bus {
    conn: zbus::blocking::Connection,
    /// This client's own unique bus name, mangled into the form a portal `Request`
    /// path carries it in. See [`mangle_sender`].
    sender: String,
    /// The random half of every token this process makes. See [`token_text`].
    prefix: String,
    /// The counted half.
    counter: AtomicU64,
    /// What the portal has said, and what it has answered.
    inbox: Arc<Inbox>,
    /// The liveness flag of the CURRENT signal pump, or `None` before there is one.
    ///
    /// A slot rather than a `OnceLock` for one reason that does fire and one that,
    /// honestly, does not:
    ///
    /// - **It fires**: a pump that could not be STARTED must be startable again. A
    ///   thread that failed to spawn under memory pressure is a transient failure and a
    ///   `OnceLock` would remember it for the life of the process.
    /// - **It does not fire**: replacing a pump that STOPPED. Two facts close that
    ///   door together. [`pump`] only ever stops when the bus connection dies (see its
    ///   own note), and [`Bus::ensure_pump`] is only ever called from
    ///   [`Bus::request`], which no live session makes: `Enable`, `Disable`,
    ///   `Release`, every `Notify*` and `Session.Close` all go the plain route. So a
    ///   dead pump is noticed no earlier than the next attempt to START a session, and
    ///   by then the connection it would be replaced on is dead too and that call is
    ///   failing for its own reasons. **The replacement machinery has a buckle nothing
    ///   can reach**, and it is kept only because it is four lines and is what makes
    ///   the START case correct. It is written down rather than dressed up as a
    ///   recovery, because a comment promising a remedy that cannot run is worse than
    ///   no comment.
    ///
    /// One flag per pump and not one flag in the [`Inbox`], and that is the whole
    /// reason for the `Arc`: a dying pump clears only its OWN flag, so it cannot clear
    /// the flag of a replacement started while it was winding down. With one shared
    /// flag that race would leave two pumps on the same match rule, each filing every
    /// signal twice.
    pump: Mutex<Option<Arc<AtomicBool>>>,
}

/// Opens the session bus.
///
/// The failure is [`PortalError::NoBus`] and not a panic: a Wayland session with no
/// session bus is a real configuration (a compositor started from a tty without
/// `dbus-run-session`) and it has its own reason code and its own sentence, because
/// its remedy is its own.
pub fn connect() -> Result<Arc<dyn Portal>, PortalError> {
    let conn = match zbus::blocking::Connection::session() {
        Ok(conn) => conn,
        Err(e) => return Err(PortalError::NoBus(e.to_string())),
    };
    // The unique name is read once, here, because every `Request` object path is
    // computed from it and a portal call cannot be made without one. A message bus
    // always assigns one at connection time; the `None` arm exists because the type
    // admits it (a peer-to-peer connection has no unique name) and a peer-to-peer
    // connection is not a session bus, which is exactly what `NoBus` says.
    let unique = match conn.unique_name() {
        Some(name) => name.as_str().to_string(),
        None => {
            return Err(PortalError::NoBus(
                "this connection has no unique name, so it is not a message bus".to_string(),
            ));
        }
    };
    Ok(Arc::new(Bus {
        sender: mangle_sender(&unique),
        conn,
        prefix: token_prefix(),
        counter: AtomicU64::new(0),
        inbox: Arc::new(Inbox::default()),
        pump: Mutex::new(None),
    }))
}

// ------------------------------------------------------- tokens and their paths

/// The `SENDER` component of a portal `Request` or `Session` object path: this
/// client's unique bus name with **the leading colon removed and every dot replaced
/// by an underscore**.
///
/// So `:1.42` becomes `1_42`. The rule is the interface's own, and it exists because
/// a D-Bus object path element may contain only ASCII letters, digits and
/// underscores, which a unique name's punctuation is not.
fn mangle_sender(unique: &str) -> String {
    unique.strip_prefix(':').unwrap_or(unique).replace('.', "_")
}

/// The random, per-process half of every token.
///
/// The interface asks that tokens "should be unique and not guessable". Uniqueness
/// alone would be a counter; the second half is what the randomness is for, since a
/// guessable token is a `Request` object another application on the same bus can
/// predict and emit a forged `Response` on.
fn token_prefix() -> String {
    format!("onedevice_{:016x}", rand::random::<u64>())
}

/// One token: the per-process prefix, a counter, and 64 fresh random bits.
///
/// All three, and each earns its place. The prefix distinguishes two processes of
/// this build on one bus; the counter guarantees that two tokens made in the same
/// microsecond differ even if the random source were to repeat; the fresh bits are
/// what make the NEXT token unguessable to somebody who has seen this one.
///
/// Hexadecimal and underscores only, so the result is a valid D-Bus object path
/// element by construction rather than by luck.
fn token_text(prefix: &str, counter: u64, salt: u64) -> String {
    format!("{prefix}_{counter:x}_{salt:016x}")
}

/// The object path a portal `Request` for `token` will be exported on.
///
/// Predictable by design, and this function is the whole of that prediction: see the
/// module header for why the prediction is what closes the race.
fn request_path(sender: &str, token: &str) -> String {
    format!("{PORTAL_PATH}/request/{sender}/{token}")
}

impl Bus {
    fn token(&self) -> String {
        token_text(
            &self.prefix,
            self.counter.fetch_add(1, Ordering::Relaxed),
            rand::random::<u64>(),
        )
    }

    /// Turns a `zbus` error into this engine's vocabulary.
    ///
    /// A D-Bus method error keeps its name and its message and goes through
    /// [`classify`], which is where the empirical rules live.
    ///
    /// # Everything else is TRANSIENT, and saying otherwise disables the feature
    ///
    /// This function used to answer [`PortalError::NoBus`] for
    /// `zbus::Error::InputOutput` and [`PortalError::Malformed`] for everything it did
    /// not recognise, and both were wrong twice over.
    ///
    /// Neither arm is [`PortalError::worth_retrying`], so ONE reset connection, one
    /// bus restart, one write that would not complete, turned into a permanent refusal
    /// of keyboard sharing for the rest of that login. And `NoBus` renders to a person
    /// as "no D-Bus session bus", which is a sentence telling somebody their machine
    /// lacks a thing that is plainly running: the remedy it implies cannot be carried
    /// out because there is nothing wrong to fix.
    ///
    /// So only `Address` keeps `NoBus`, because a bus address that will not parse or
    /// will not be reached IS a configuration rather than a hiccup. Everything else
    /// becomes [`PortalError::Failed`], which is retryable and keeps the whole text
    /// for a bug report, and whose name says the failure was not the portal's answer.
    fn error(&self, e: zbus::Error, interface: &str) -> PortalError {
        match &e {
            zbus::Error::MethodError(name, detail, _) => classify(
                name.as_str(),
                detail.as_deref().unwrap_or_default(),
                interface,
            ),
            zbus::Error::Address(_) => PortalError::NoBus(e.to_string()),
            _ => PortalError::Failed {
                name: LOCAL.to_string(),
                message: e.to_string(),
            },
        }
    }

    /// One method call that answers nothing worth reading.
    ///
    /// The three `InputCapture` calls with no out argument and the six `Notify*` calls
    /// all end here, so that the error mapping is written once. A closure rather than
    /// a generic body, and the reason is a dependency rather than a taste: the bound
    /// `zbus::blocking::Connection::call_method` wants is `serde::Serialize`, and
    /// `serde` is not a dependency of this crate, so naming that bound would mean
    /// either a new dependency edge or leaning on `zbus`'s hidden re-export of it.
    /// Passing the call in already made costs one closure per call site and nothing
    /// else.
    fn plain(
        &self,
        interface: &str,
        call: impl FnOnce() -> Result<zbus::message::Message, zbus::Error>,
    ) -> Result<(), PortalError> {
        call().map(|_| ()).map_err(|e| self.error(e, interface))
    }

    /// Makes a portal call that answers an `o handle`, and hands back the `results`
    /// vardict of that handle's `Response` signal.
    ///
    /// **The order of the four steps is the whole point of this function** and is
    /// the reason no caller is allowed to make such a call itself:
    ///
    /// 1. the pump is running, so the bus is already forwarding `Response` signals;
    /// 2. the token is minted and its predicted path put on the waiting list;
    /// 3. only THEN is the call made;
    /// 4. and the path the call answered is checked against the prediction.
    ///
    /// On a mismatch at step 4 the portal's answer wins and the predicted path stays
    /// on the list as well, so the wait succeeds whichever of the two the signal
    /// arrives on. Believing the portal is the safe direction: the prediction is a
    /// rule of the interface rather than a guarantee of an implementation, and
    /// waiting on a path nobody will ever emit on is a timeout where an answer was
    /// available. The mismatch is warned about because it means this build and that
    /// portal disagree about a documented rule, which a bug report needs.
    fn request(
        &self,
        interface: &str,
        method: &str,
        budget: Duration,
        call: impl FnOnce(&str) -> Result<zbus::message::Message, zbus::Error>,
    ) -> Result<Vardict, PortalError> {
        self.ensure_pump()?;
        let token = self.token();
        let expected = request_path(&self.sender, &token);
        // The guard, not a pair of removals: every exit below (the call failing, a
        // malformed handle, the timeout, the refusal) has to take the path off the
        // waiting list, and a `Drop` cannot be forgotten by a later edit.
        let mut waited = Waited {
            inbox: &self.inbox,
            paths: Vec::new(),
        };
        waited.watch(expected.clone());
        let reply = call(&token).map_err(|e| self.error(e, interface))?;
        let handle: zvariant::OwnedObjectPath = match reply.body().deserialize() {
            Ok(handle) => handle,
            Err(e) => {
                // The request exists on the portal's side even though its handle came
                // back in a shape this build cannot read, so it is cancelled at the
                // path the rule predicts rather than left to run to a dialog nobody
                // is listening for.
                self.abandon(&expected);
                return Err(PortalError::Malformed(format!(
                    "{interface}.{method} did not answer a request handle: {e}"
                )));
            }
        };
        if handle.as_str() != expected {
            warn(&format!(
                "{interface}.{method} answered the request handle {} and not the {expected} the \
                 interface's own rule predicts: waiting on both",
                handle.as_str()
            ));
            waited.watch(handle.as_str().to_string());
        }
        let answer = match waited.wait(budget) {
            Some(answer) => answer,
            None => {
                warn(&format!(
                    "{interface}.{method} did not answer within {}s",
                    budget.as_secs()
                ));
                // **Cancelled, not just abandoned**, and this is the difference
                // between a timeout and a leak. Giving up on the wait leaves the
                // portal's dialog on screen; the person comes back, clicks Allow, and
                // the portal creates and EXPORTS a session that nothing in this
                // process is holding a handle to. On a desktop with a "remote input"
                // indicator that stays lit for the rest of their login, and no code
                // anywhere can close it, because the handle only ever existed in a
                // `Response` nobody read. `Request.Close` ends the request and, the
                // interface says, "all related user interaction (window closes, etc.)".
                for path in &waited.paths {
                    self.abandon(path);
                }
                return Err(PortalError::Timeout);
            }
        };
        // The fail-closed reading of an unknown code lives in one place, and it is
        // not here.
        response(answer.code)?;
        Ok(answer.results)
    }

    /// `org.freedesktop.portal.Request.Close()` on a request this side has stopped
    /// waiting for.
    ///
    /// Best effort and never awaited, for the reason [`Portal::close`] gives at
    /// length: this runs on a path where something has already gone wrong, and a reply
    /// wait here would add the bus's own twenty-five second ceiling to a failure.
    fn abandon(&self, request: &str) {
        self.forget(request, REQUEST, "Close", "abandoning a request");
    }

    /// One method call sent with `NO_REPLY_EXPECTED` and not awaited.
    ///
    /// The shape both `Request.Close` and `Session.Close` need: a teardown that must
    /// leave, and must not be able to block while it does.
    fn forget(&self, path: &str, interface: &str, method: &str, what: &str) {
        let message = zbus::message::Message::method_call(path, method)
            .and_then(|builder| builder.destination(PORTAL_BUS))
            .and_then(|builder| builder.interface(interface))
            .and_then(|builder| builder.with_flags(zbus::message::Flags::NoReplyExpected))
            .and_then(|builder| builder.build(&()));
        match message {
            Ok(message) => {
                if let Err(e) = self.conn.send(&message) {
                    warn(&format!("{what} ({path}) failed: {e}"));
                }
            }
            Err(e) => warn(&format!("{what} ({path}) would not build a message: {e}")),
        }
    }

    /// Starts the signal pump if it is not running, and registers its match rule
    /// with the bus before returning.
    ///
    /// Lazily rather than in [`connect`], because [`connect`] runs on every start of
    /// this component on every Wayland desktop, including the overwhelming majority
    /// that have neither portal: a thread and a bus-side match rule for a session
    /// nobody will ever open is a cost paid by machines that cannot use the feature.
    /// The first call that could produce a signal is a session call, and every
    /// session call goes through [`Bus::request`], which calls this first.
    ///
    /// **And this is the ONLY caller**, which is what makes the replacement path in
    /// [`Bus::pump`] all but unreachable: see that field's own note.
    fn ensure_pump(&self) -> Result<(), PortalError> {
        let mut current = lock(&self.pump);
        if current
            .as_ref()
            .is_some_and(|alive| alive.load(Ordering::Acquire))
        {
            return Ok(());
        }
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            // The bus filters on the well-known name and resolves it to the portal's
            // unique name itself. `zbus`'s own client-side re-check skips a
            // well-known sender (it "can't match against a well-known name"), so
            // naming it here narrows what the bus sends and never narrows what this
            // process accepts.
            .sender(PORTAL_BUS)
            .and_then(|b| b.path_namespace(PORTAL_PATH))
            .map_err(|e| {
                PortalError::Malformed(format!("the portal's own name will not parse: {e}"))
            })?
            .build();
        // NOT classified against either portal interface, and that matters. The call
        // this makes is `org.freedesktop.DBus.AddMatch` on the bus itself, so an
        // `AccessDenied` from a restricted bus policy has nothing to do with
        // `InputCapture`: routing it through `Bus::error` named the wrong interface and
        // turned it into `Refused { cancelled: false }`, which tells a person they
        // declined a dialog that was never put in front of them. `Failed` keeps the
        // bus's own text, is retryable, and its name says the failure was not a
        // portal's answer. And `ensure_pump` serves BOTH halves, so there is no one
        // interface it could honestly be blamed on anyway.
        let iterator =
            zbus::blocking::MessageIterator::for_match_rule(rule, &self.conn, Some(QUEUE_DEPTH))
                .map_err(|e| PortalError::Failed {
                    name: LOCAL.to_string(),
                    message: format!("the portal's signals could not be subscribed to: {e}"),
                })?;
        let alive = Arc::new(AtomicBool::new(true));
        let inbox = Arc::clone(&self.inbox);
        let mine = Arc::clone(&alive);
        if let Err(e) = std::thread::Builder::new()
            .name("1device-portal".to_string())
            .spawn(move || pump(iterator, &inbox, &mine))
        {
            // The flag is dropped with the failed spawn, so nothing is left claiming
            // a pump exists.
            return Err(PortalError::Failed {
                name: LOCAL.to_string(),
                message: format!("no thread could be started for the portal's signals: {e}"),
            });
        }
        *current = Some(alive);
        Ok(())
    }

    /// A session handle as the object path every call after `CreateSession` wants.
    ///
    /// It is a conversion and not a cast because of one wart the interface admits to:
    /// `RemoteDesktop.CreateSession` answers its `session_handle` as a plain STRING
    /// ("erroneously implemented as `s`. For backwards compatibility it will remain
    /// this type") while every method that takes it declares `o`. So a handle from
    /// that half arrives untyped and has to be re-typed here, and one that will not
    /// parse as an object path is a portal that answered something this build cannot
    /// use, which is [`PortalError::Malformed`] and not a refusal.
    fn path_of(&self, session: &SessionId) -> Result<zvariant::OwnedObjectPath, PortalError> {
        match zvariant::ObjectPath::try_from(session.0.as_str()) {
            Ok(path) => Ok(path.into()),
            Err(e) => Err(PortalError::Malformed(format!(
                "the session handle {:?} is not an object path: {e}",
                session.0
            ))),
        }
    }
}

// ------------------------------------------------------------ what the portal says

/// What the portal has said, and what it has answered. Shared with the pump thread.
#[derive(Default)]
struct Inbox {
    /// One lock for both halves, because one [`Condvar`] can only be paired with one
    /// mutex and the waiting half needs one. The critical sections are a push and a
    /// `mem::take`, so contention between the pump and the engine is not a thing
    /// that can be measured.
    state: Mutex<Waiting>,
    answered: Condvar,
}

#[derive(Default)]
struct Waiting {
    /// Decoded signals waiting for [`Portal::take_signals`].
    signals: Vec<Signal>,
    /// The `Request` paths somebody is waiting on, and the answer once it lands.
    ///
    /// The keys are what BOUNDS this map: the pump files an answer only for a path
    /// that is already a key, so a portal emitting `Response` on paths of its own
    /// invention adds nothing, and a caller that has given up takes its key with it.
    answers: HashMap<String, Option<Answer>>,
    /// Signals dropped for want of room, reported once at the next drain.
    dropped: u64,
}

/// One `Request.Response`.
struct Answer {
    code: u32,
    results: Vardict,
}

/// A caller's interest in one or more `Request` paths, withdrawn on drop.
struct Waited<'a> {
    inbox: &'a Inbox,
    paths: Vec<String>,
}

impl Waited<'_> {
    fn watch(&mut self, path: String) {
        lock(&self.inbox.state).answers.insert(path.clone(), None);
        self.paths.push(path);
    }

    /// Waits for any watched path to be answered, at most `budget`.
    ///
    /// The re-check before every wait and after every wake is not belt and braces:
    /// the answer can already be on the table when this is first called, because the
    /// path went on the waiting list before the call was made, which is the whole
    /// point of doing it in that order.
    fn wait(&self, budget: Duration) -> Option<Answer> {
        let deadline = Instant::now() + budget;
        let mut state = lock(&self.inbox.state);
        loop {
            for path in &self.paths {
                if let Some(answer) = state.answers.get_mut(path).and_then(Option::take) {
                    return Some(answer);
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            // A poisoned lock is recovered from rather than propagated, for the
            // reason `lock` gives.
            let (next, _) = self
                .inbox
                .answered
                .wait_timeout(state, left)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = next;
        }
    }
}

impl Drop for Waited<'_> {
    fn drop(&mut self) {
        let mut state = lock(&self.inbox.state);
        for path in &self.paths {
            state.answers.remove(path);
        }
    }
}

/// Takes a lock, recovering a poisoned one instead of panicking.
///
/// Nothing this file does while holding either lock can panic, so a poisoned lock
/// means something else in the process panicked while holding it, and there is no
/// invariant of THIS data that a panic elsewhere could have broken: the two halves
/// are a `Vec` of decoded signals and a map of answers, each written in one
/// statement. Refusing to work from then on would leave the engine unable to hear
/// that a capture ended, which is a machine holding somebody's keyboard, and that is
/// the one failure this whole component exists to prevent.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The signal pump: one thread, one match rule.
///
/// # This loop ends exactly when the bus connection dies, and never otherwise
///
/// Which is worth stating because the obvious defensive structure here (count the
/// consecutive read failures, give up after N) would be dead code. Two facts from
/// `zbus` 5.17.0's own source say so:
///
/// 1. **A signal this build cannot decode is not an `Err` here.** It arrives as
///    `Ok(message)`, because the D-Bus framing parsed perfectly well; it is the BODY
///    that this build reads and can find wrong, and that happens in [`on_response`]
///    and [`on_capture`], which warn and carry on. Nothing about a portal sending
///    nonsense reaches this `Err` arm.
/// 2. **The iterator yields at most ONE `Err`, and then `None`.** `zbus`'s socket
///    reader broadcasts a read failure once to every stream, then calls
///    `senders.clear()` and returns. So there is no second failure to count, no busy
///    loop to guard against, and a counter could never reach two.
///
/// The pump therefore has one death: the connection. It is warned about loudly on the
/// way out, because from that moment nothing the portal says will be heard, and the
/// most important thing it says is that a session ended.
///
/// `alive` is this pump's own flag and nobody else's, and it is cleared exactly once,
/// on the way out. See [`Bus::pump`] for why it is not shared, and for the honest
/// account of what the flag can and cannot lead to.
fn pump(iterator: zbus::blocking::MessageIterator, inbox: &Inbox, alive: &AtomicBool) {
    for message in iterator {
        match message {
            Ok(message) => route(&message, inbox),
            // The connection's one and only failure. The loop ends on the `None` that
            // follows it whether or not anything is done here.
            Err(e) => warn(&format!("the portal's signal connection failed: {e}")),
        }
    }
    alive.store(false, Ordering::Release);
    warn(
        "the portal's signal pump stopped, so no capture can learn that its session ended: the \
         session bus connection is gone",
    );
}

/// One message from the bus, filed or ignored.
fn route(message: &zbus::message::Message, inbox: &Inbox) {
    let header = message.header();
    let (Some(interface), Some(member)) = (header.interface(), header.member()) else {
        return;
    };
    match (interface.as_str(), member.as_str()) {
        (REQUEST, "Response") => on_response(message, inbox),
        (SESSION, "Closed") => {
            // **The session is named by the object the signal came FROM**, because
            // `Closed` carries only `a{sv} details` and no handle. That is the
            // asymmetry with `InputCapture`'s signals, whose first ARGUMENT is the
            // session, and reading the wrong one of the two gives a signal that
            // matches no session and is discarded in silence.
            let Some(path) = header.path() else {
                warn("a portal session closed and the signal carried no path");
                return;
            };
            file(
                inbox,
                Signal::Closed {
                    session: SessionId(path.as_str().to_string()),
                },
            );
        }
        (INPUT_CAPTURE, member) => on_capture(message, member, inbox),
        // Every other portal interface the path namespace lets through, and any
        // future member of the three above. Ignored rather than logged: the rule is
        // deliberately broad (see the module header) and a log line per settings
        // change would be noise.
        _ => {}
    }
}

/// A `Request.Response`, matched against whoever is waiting for it.
fn on_response(message: &zbus::message::Message, inbox: &Inbox) {
    let Some(path) = message.header().path().map(|p| p.as_str().to_string()) else {
        return;
    };
    let (code, results) = match message.body().deserialize::<(u32, Vardict)>() {
        Ok(body) => body,
        Err(e) => {
            // A `Response` this build cannot read is a request that will now time
            // out, which is a worse outcome than a refusal and so is said loudly.
            warn(&format!("the response on {path} could not be read: {e}"));
            return;
        }
    };
    {
        let mut state = lock(&inbox.state);
        match state.answers.get_mut(&path) {
            Some(slot) => *slot = Some(Answer { code, results }),
            // Nobody is waiting: a request that timed out, or a `Response` for an
            // object this process never asked for. Dropped, which is what keeps the
            // map bounded.
            None => return,
        }
    }
    // Notified with the lock released, and `notify_all` rather than `notify_one`
    // because two threads can legitimately be waiting on two different requests and
    // waking the wrong one would lose this wake-up.
    inbox.answered.notify_all();
}

/// One of `InputCapture`'s four signals, decoded into this engine's vocabulary.
///
/// Every option key below is read as the interface types it, and a key that is
/// absent or of the wrong type is handled by NAME rather than defaulted:
///
/// - `activation_id` is required by both `Activated` and `Deactivated`, and it is
///   the number the EI stream's own sequence is checked against
///   ([`crate::wayland::Decoder::decode`]). A signal without it is dropped, because
///   passing one on with an invented id would confirm an activation that the
///   compositor is emulating under a different number, which is the exact
///   cross-activation mix-up that gate exists to prevent.
/// - `zone_set` is required by `ZonesChanged`, and a signal without it is dropped
///   for the same kind of reason: the state machine's rule 3 acts on the serial
///   DIFFERING from the one it holds, so any invented number is either a needless
///   round trip or, if it happens to collide, a barrier set left stale in silence.
///   Dropping it means the barriers stay stale until the next real signal, which is
///   the lesser of the two and is warned about.
/// - `cursor_position` and `barrier_id` are optional in the seam itself, so absent
///   is `None` and needs no warning. A present one of the wrong type does get a
///   warning, because that is a portal disagreeing with its own XML.
fn on_capture(message: &zbus::message::Message, member: &str, inbox: &Inbox) {
    let (handle, options) = match message
        .body()
        .deserialize::<(zvariant::OwnedObjectPath, Vardict)>()
    {
        Ok(body) => body,
        Err(e) => {
            warn(&format!("{INPUT_CAPTURE}.{member} could not be read: {e}"));
            return;
        }
    };
    let session = SessionId(handle.as_str().to_string());
    let signal = match member {
        "Activated" => {
            let Some(activation) = maybe_u32(&options, member, "activation_id") else {
                warn(&format!(
                    "{INPUT_CAPTURE}.Activated carried no usable activation_id, so its events \
                     could never be matched to it: ignoring the activation"
                ));
                return;
            };
            Signal::Activated {
                session,
                activation,
                cursor: maybe_pair(&options, member, "cursor_position"),
                barrier: maybe_u32(&options, member, "barrier_id"),
            }
        }
        "Deactivated" => {
            let Some(activation) = maybe_u32(&options, member, "activation_id") else {
                warn(&format!(
                    "{INPUT_CAPTURE}.Deactivated carried no usable activation_id, so it cannot \
                     be told from a stale one: ignoring it"
                ));
                return;
            };
            Signal::Deactivated {
                session,
                activation,
            }
        }
        // No options are used by this one, so there is nothing that can be missing.
        "Disabled" => Signal::Disabled { session },
        "ZonesChanged" => {
            let Some(serial) = maybe_u32(&options, member, "zone_set") else {
                warn(&format!(
                    "{INPUT_CAPTURE}.ZonesChanged carried no usable zone_set, so the barriers \
                     cannot be replaced from it: they stay as they are until the next one"
                ));
                return;
            };
            Signal::ZonesChanged { session, serial }
        }
        // A signal a later version of the interface adds. Ignored, which is the same
        // rule the capability bits follow: an unknown thing from the portal is not
        // an error.
        _ => return,
    };
    file(inbox, signal);
}

/// Files one decoded signal, keeping the buffer's bound.
///
/// # Why the OLDEST is not simply dropped
///
/// Because the signals are not interchangeable. Losing an `Activated` costs one
/// crossing that does not happen, and the person moves the mouse again. Losing a
/// `Closed` costs the engine its only notification that a session is dead: the state
/// machine would stay in [`crate::wayland::Phase::Armed`] for a session the
/// compositor has forgotten, the interface would say this machine can drive, and
/// every crossing from then on would do nothing at all with nothing said. A
/// terminal fact and a repeatable one cannot share an eviction rule.
///
/// So the rule is three lines and each is argued:
///
/// 1. A second `Closed` for a session already in the buffer says nothing new and is
///    dropped, which is also what makes the count of `Closed`s in the buffer at most
///    the number of live sessions (two, in this build).
/// 2. When the buffer is full, the oldest NON-`Closed` entry makes room. Oldest,
///    because a stale `Activated` is the least useful thing in there.
/// 3. Only if there is nothing but `Closed`s to evict is the incoming signal
///    dropped, and that needs a thousand distinct sessions to reach, which no
///    version of this engine can produce.
fn file(inbox: &Inbox, signal: Signal) {
    let mut state = lock(&inbox.state);
    if let Signal::Closed { session } = &signal
        && state
            .signals
            .iter()
            .any(|held| matches!(held, Signal::Closed { session: had } if had == session))
    {
        return;
    }
    if state.signals.len() >= SIGNAL_CAP {
        state.dropped += 1;
        match state
            .signals
            .iter()
            .position(|held| !matches!(held, Signal::Closed { .. }))
        {
            Some(at) => {
                state.signals.remove(at);
            }
            None => return,
        }
    }
    state.signals.push(signal);
}

// -------------------------------------------------------- reading a results vardict

/// One key of a `results` vardict, or [`PortalError::Malformed`] naming it.
///
/// Absent is Malformed and never a default, and the rule has one reason: every key
/// this build reads is documented by the interface as part of the answer, so an
/// answer without one means this build and that portal disagree about the protocol.
/// Substituting a zero would turn that disagreement into a capture session with no
/// capabilities, or a barrier set believed to be fully placed when it was refused,
/// each of which is a machine that looks like it works and does nothing.
fn need<'v>(results: &'v Vardict, key: &str) -> Result<&'v zvariant::Value<'v>, PortalError> {
    match results.get(key) {
        Some(value) => Ok(peel(value)),
        None => Err(PortalError::Malformed(format!(
            "the portal's answer carried no {key}"
        ))),
    }
}

/// A `v` unwrapped once, if the portal nested one inside its vardict value.
///
/// `zvariant` hands back the CONTENTS of an `a{sv}`'s variant, so the ordinary case
/// needs no unwrapping at all. This exists for the portal that writes a variant
/// inside a variant, which is legal D-Bus and which a reading of the XML does not
/// forbid: one level is peeled rather than dying on a shape that is only unusual.
fn peel<'v>(value: &'v zvariant::Value<'v>) -> &'v zvariant::Value<'v> {
    match value {
        zvariant::Value::Value(inner) => inner.as_ref(),
        other => other,
    }
}

/// A `(dd)` or a `(iiii)` as the portal wants it in an options vardict.
///
/// A builder rather than `Structure::from(tuple)`, which exists and is one line: that
/// `From` implementation ends in an `unwrap` on the builder, and this file does not
/// contain a panic even by way of a dependency's convenience. The error is
/// unreachable for a non-empty field list, which is why nothing branches on it beyond
/// naming it.
fn structure<'v>(
    fields: Vec<zvariant::Value<'v>>,
    what: &str,
) -> Result<zvariant::Value<'v>, PortalError> {
    let mut builder = zvariant::StructureBuilder::new();
    for field in fields {
        builder.push_value(field);
    }
    builder
        .build()
        .map(zvariant::Value::Structure)
        .map_err(|e| PortalError::Malformed(format!("this build could not encode {what}: {e}")))
}

/// The one shape of complaint, so that every `Malformed` from a results vardict
/// names both the key and what was expected of it.
fn shape(key: &str, wanted: &str) -> PortalError {
    PortalError::Malformed(format!("the portal's {key} is not {wanted}"))
}

fn u32_of(results: &Vardict, key: &str) -> Result<u32, PortalError> {
    match need(results, key)? {
        zvariant::Value::U32(n) => Ok(*n),
        _ => Err(shape(key, "a u32")),
    }
}

fn u32_field(value: &zvariant::Value<'_>, key: &str) -> Result<u32, PortalError> {
    match peel(value) {
        zvariant::Value::U32(n) => Ok(*n),
        _ => Err(shape(key, "a u32 where the interface says u")),
    }
}

fn i32_field(value: &zvariant::Value<'_>, key: &str) -> Result<i32, PortalError> {
    match peel(value) {
        zvariant::Value::I32(n) => Ok(*n),
        _ => Err(shape(key, "an i32 where the interface says i")),
    }
}

/// `failed_barriers` (`au`), which is `SetPointerBarriers`'s list of refusals.
///
/// # The one key whose ABSENCE is not `Malformed`, and why the asymmetry is right
///
/// Everywhere else in this file an absent documented key is
/// [`PortalError::Malformed`], because it means this build and that portal disagree
/// about the protocol ([`need`] carries the argument). Here it is an EMPTY LIST, and
/// the asymmetry is deliberate rather than a lapse.
///
/// What an absent `failed_barriers` most plausibly means is "no barrier failed", which
/// is the most SUCCESSFUL outcome the call has. Reading that as `Malformed` would be a
/// disaster out of all proportion: `Malformed` is not
/// [`PortalError::worth_retrying`], `crate::wayland::Capture::rearm_barriers`
/// propagates it, and one portal implementation that omits an empty array would
/// therefore turn a perfect result into a permanent refusal of the whole feature,
/// reported to a person as "the portal answered something unexpected".
///
/// Compare the keys that keep the strict rule: an absent `session_handle` leaves
/// nothing to hold, an absent `capabilities` would have to be invented and a wrong
/// guess arms a session that can never see the pointer leave, and an absent `devices`
/// (see [`Bus::start_devices`]) would have to be invented the same way. Those three
/// fail closed and say so. This one has a correct, safe reading available, so it takes
/// it. A key that is PRESENT and of the wrong type is still `Malformed`, because that
/// is a real disagreement rather than an omission.
fn refused_of(results: &Vardict) -> Result<Vec<u32>, PortalError> {
    const KEY: &str = "failed_barriers";
    if !results.contains_key(KEY) {
        return Ok(Vec::new());
    }
    let zvariant::Value::Array(array) = need(results, KEY)? else {
        return Err(shape(KEY, "an array"));
    };
    array
        .inner()
        .iter()
        .map(|value| u32_field(value, KEY))
        .collect()
}

/// `a(uuii)`, which is what `GetZones` answers its screens as: width and height
/// unsigned, offsets signed, in that order and with nothing else in the struct.
fn zones_of(results: &Vardict, key: &str) -> Result<Vec<Zone>, PortalError> {
    let zvariant::Value::Array(array) = need(results, key)? else {
        return Err(shape(key, "an array of (uuii)"));
    };
    let mut out = Vec::with_capacity(array.len());
    for element in array.inner() {
        let zvariant::Value::Structure(zone) = peel(element) else {
            return Err(shape(key, "an array of structs"));
        };
        let [w, h, x, y] = zone.fields() else {
            return Err(shape(key, "an array of four-field structs"));
        };
        out.push(Zone {
            w: u32_field(w, key)?,
            h: u32_field(h, key)?,
            x: i32_field(x, key)?,
            y: i32_field(y, key)?,
        });
    }
    Ok(out)
}

/// A session handle out of a `results` vardict.
///
/// **Typed `o` by `InputCapture` and `s` by `RemoteDesktop`**, and both are accepted
/// here rather than in two functions, because the seam's [`SessionId`] is a `String`
/// for exactly that reason: the interface admits the second one is a mistake it will
/// keep ("erroneously implemented as `s`. For backwards compatibility it will remain
/// this type"). Accepting either is not laxity, it is the union of two documented
/// signatures.
fn session_of(results: &Vardict, key: &str) -> Result<SessionId, PortalError> {
    match need(results, key)? {
        zvariant::Value::ObjectPath(path) => Ok(SessionId(path.as_str().to_string())),
        zvariant::Value::Str(text) => Ok(SessionId(text.as_str().to_string())),
        _ => Err(shape(key, "an object path or a string")),
    }
}

/// An optional `u` out of a signal's options.
fn maybe_u32(options: &Vardict, member: &str, key: &str) -> Option<u32> {
    match options.get(key).map(|value| peel(value)) {
        Some(zvariant::Value::U32(n)) => Some(*n),
        // Present and not a `u`: the portal disagreeing with its own XML, which is
        // worth a line even though the caller can carry on without the value.
        Some(other) => {
            warn(&format!(
                "{INPUT_CAPTURE}.{member} carried a {key} of signature {}, not u",
                other.value_signature()
            ));
            None
        }
        None => None,
    }
}

/// An optional `(dd)` out of a signal's options, which is how a cursor position
/// travels.
fn maybe_pair(options: &Vardict, member: &str, key: &str) -> Option<(f64, f64)> {
    let value = options.get(key).map(|value| peel(value))?;
    let zvariant::Value::Structure(fields) = value else {
        warn(&format!(
            "{INPUT_CAPTURE}.{member} carried a {key} of signature {}, not (dd)",
            value.value_signature()
        ));
        return None;
    };
    match fields.fields() {
        [zvariant::Value::F64(x), zvariant::Value::F64(y)] => Some((*x, *y)),
        _ => {
            warn(&format!(
                "{INPUT_CAPTURE}.{member} carried a {key} that is not a pair of doubles"
            ));
            None
        }
    }
}

// ---------------------------------------------------------------------- the seam

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

    /// `CreateSession(s parent_window, a{sv} options) -> o handle`, whose response
    /// carries `session_handle` (o) and `capabilities` (u).
    fn capture_create_session(&self, want: u32) -> Result<CaptureGrant, PortalError> {
        if want == 0 {
            // The interface says the `capabilities` option is required and non-zero.
            // Asking anyway would earn an `InvalidArgs`, which
            // [`classify`] would read through its own trap as "the interface is not
            // there" and tell somebody to install a portal they already have.
            //
            // `Malformed` for a request rather than an answer, and it is chosen for
            // its BEHAVIOUR: it is the one arm that is not worth retrying, which is
            // right, because asking again with the same empty mask fails the same
            // way. The message says whose fault it is.
            return Err(PortalError::Malformed(
                "this build asked for a capture session with no capabilities at all".to_string(),
            ));
        }
        // A token for the SESSION as well as for the request. The session's own path
        // is read back out of the response rather than computed from this, because
        // the response is authoritative and a portal is free to hand back a path of
        // its own; the option is sent because the interface documents it.
        let session_token = self.token();
        let results = self.request(
            INPUT_CAPTURE,
            "CreateSession",
            // The call that raises the dialog on a version 1 portal, which is every
            // portal on a released desktop today.
            CONSENT_BUDGET,
            |token| {
                let mut options = Options::new();
                options.insert("handle_token", zvariant::Value::from(token));
                options.insert(
                    "session_handle_token",
                    zvariant::Value::from(session_token.as_str()),
                );
                options.insert("capabilities", zvariant::Value::from(want));
                // `parent_window` is empty, and truthfully so: this component is a
                // headless daemon with no window for the portal to attach a dialog
                // to. The interface allows the empty string for exactly that.
                self.conn.call_method(
                    Some(PORTAL_BUS),
                    PORTAL_PATH,
                    Some(INPUT_CAPTURE),
                    "CreateSession",
                    &("", options),
                )
            },
        )?;
        // A handle in hand means the portal has CREATED and exported a session, and a
        // person has already said yes to it. Everything after this line therefore has
        // to close it on the way out: returning an error while holding the only handle
        // to a consented session leaves it open with nothing able to close it, which on
        // some desktops is an indicator lit for the rest of that login. There is
        // nothing to close if this line itself fails, because then there is no handle.
        let session = session_of(&results, "session_handle")?;
        // Read back and never assumed: the interface says what comes back is always a
        // subset of what was asked for.
        let capabilities = match u32_of(&results, "capabilities") {
            Ok(capabilities) => capabilities,
            Err(e) => {
                self.close(&session);
                return Err(e);
            }
        };
        Ok(CaptureGrant {
            session,
            capabilities,
        })
    }

    /// `GetZones(o session_handle, a{sv} options) -> o handle`, whose response
    /// carries `zones` (a(uuii)) and `zone_set` (u).
    fn capture_zones(&self, session: &SessionId) -> Result<ZoneSet, PortalError> {
        let path = self.path_of(session)?;
        let results = self.request(INPUT_CAPTURE, "GetZones", ANSWER_BUDGET, |token| {
            let mut options = Options::new();
            options.insert("handle_token", zvariant::Value::from(token));
            self.conn.call_method(
                Some(PORTAL_BUS),
                PORTAL_PATH,
                Some(INPUT_CAPTURE),
                "GetZones",
                &(&path, options),
            )
        })?;
        Ok(ZoneSet {
            zones: zones_of(&results, "zones")?,
            serial: u32_of(&results, "zone_set")?,
        })
    }

    /// `SetPointerBarriers(o session_handle, a{sv} options, aa{sv} barriers,
    /// u zone_set) -> o handle`, whose response carries `failed_barriers` (au).
    ///
    /// Each barrier is a vardict of `barrier_id` (u, which the interface reserves
    /// zero from) and `position` ((iiii) as x1, y1, x2, y2).
    ///
    /// A barrier with a zero id is sent as given rather than filtered out here.
    /// Filtering would leave the caller believing it had been placed; sending it
    /// gets it back in `failed_barriers`, which is a path
    /// [`crate::wayland::Capture::rearm_barriers`] already handles and already says
    /// out loud. [`crate::wayland::barriers_for`] numbers from one, so this is a
    /// belt rather than a case that happens.
    fn capture_set_barriers(
        &self,
        session: &SessionId,
        serial: u32,
        barriers: &[Barrier],
    ) -> Result<Vec<u32>, PortalError> {
        let path = self.path_of(session)?;
        let mut wire: Vec<Options<'_>> = Vec::with_capacity(barriers.len());
        for barrier in barriers {
            let mut one = Options::new();
            one.insert("barrier_id", zvariant::Value::from(barrier.id));
            one.insert(
                "position",
                structure(
                    vec![
                        zvariant::Value::from(barrier.x1),
                        zvariant::Value::from(barrier.y1),
                        zvariant::Value::from(barrier.x2),
                        zvariant::Value::from(barrier.y2),
                    ],
                    "a barrier position",
                )?,
            );
            wire.push(one);
        }
        let results = self.request(
            INPUT_CAPTURE,
            "SetPointerBarriers",
            ANSWER_BUDGET,
            |token| {
                let mut options = Options::new();
                options.insert("handle_token", zvariant::Value::from(token));
                self.conn.call_method(
                    Some(PORTAL_BUS),
                    PORTAL_PATH,
                    Some(INPUT_CAPTURE),
                    "SetPointerBarriers",
                    // The serial is the one the zones were just read with: the
                    // compositor refuses barriers carrying any other, which is the
                    // rule that stops a barrier landing on a screen that has moved.
                    &(&path, options, &wire, serial),
                )
            },
        )?;
        refused_of(&results)
    }

    /// `ConnectToEIS(o session_handle, a{sv} options) -> h fd`, and then the EI
    /// handshake.
    ///
    /// **Not a `Request`**: this is the one session call that answers its result on
    /// the spot, as a file descriptor in the reply, and so it goes nowhere near
    /// [`Bus::request`]. The interface requires it BEFORE `Enable`, which
    /// [`crate::wayland::Capture::begin`] is what enforces.
    fn capture_connect(&self, session: &SessionId) -> Result<Box<dyn EiSource>, PortalError> {
        let path = self.path_of(session)?;
        let options = Options::new();
        let reply = self
            .conn
            .call_method(
                Some(PORTAL_BUS),
                PORTAL_PATH,
                Some(INPUT_CAPTURE),
                "ConnectToEIS",
                &(&path, options),
            )
            .map_err(|e| self.error(e, INPUT_CAPTURE))?;
        let fd: zvariant::OwnedFd = reply.body().deserialize().map_err(|e| {
            PortalError::Malformed(format!(
                "{INPUT_CAPTURE}.ConnectToEIS did not answer a file descriptor: {e}"
            ))
        })?;
        let stream = std::os::unix::net::UnixStream::from(std::os::fd::OwnedFd::from(fd));
        EiStream::open(stream).map(|source| Box::new(source) as Box<dyn EiSource>)
    }

    /// `Enable(o session_handle, a{sv} options)`. No out argument and no `Request`:
    /// the interface's XML declares neither, so there is nothing to wait for.
    fn capture_enable(&self, session: &SessionId) -> Result<(), PortalError> {
        let path = self.path_of(session)?;
        self.plain(INPUT_CAPTURE, || {
            self.conn.call_method(
                Some(PORTAL_BUS),
                PORTAL_PATH,
                Some(INPUT_CAPTURE),
                "Enable",
                &(&path, Options::new()),
            )
        })
    }

    /// `Disable(o session_handle, a{sv} options)`. Emits no signal of its own, so a
    /// caller learns nothing from the portal about it having taken effect.
    fn capture_disable(&self, session: &SessionId) -> Result<(), PortalError> {
        let path = self.path_of(session)?;
        self.plain(INPUT_CAPTURE, || {
            self.conn.call_method(
                Some(PORTAL_BUS),
                PORTAL_PATH,
                Some(INPUT_CAPTURE),
                "Disable",
                &(&path, Options::new()),
            )
        })
    }

    /// `Release(o session_handle, a{sv} options)`, whose options are `activation_id`
    /// (u) and `cursor_position` ((dd)).
    ///
    /// The position is a SUGGESTION the interface says the compositor may ignore, so
    /// it is sent when there is one and omitted when there is not: an absent key is
    /// the interface's way of saying "wherever you like", and a fabricated pair
    /// would move somebody's pointer somewhere nobody chose.
    fn capture_release(
        &self,
        session: &SessionId,
        activation: u32,
        cursor: Option<(f64, f64)>,
    ) -> Result<(), PortalError> {
        let path = self.path_of(session)?;
        let mut options = Options::new();
        options.insert("activation_id", zvariant::Value::from(activation));
        if let Some((x, y)) = cursor {
            options.insert(
                "cursor_position",
                structure(
                    vec![zvariant::Value::from(x), zvariant::Value::from(y)],
                    "a cursor position",
                )?,
            );
        }
        self.plain(INPUT_CAPTURE, || {
            self.conn.call_method(
                Some(PORTAL_BUS),
                PORTAL_PATH,
                Some(INPUT_CAPTURE),
                "Release",
                &(&path, options),
            )
        })
    }

    /// `CreateSession` then `SelectDevices` then `Start`, which is the whole of
    /// starting a `RemoteDesktop` session.
    ///
    /// # The three calls, and the one wart
    ///
    /// `CreateSession(a{sv} options) -> o handle` takes no `parent_window` (unlike
    /// `InputCapture`'s) and answers a response whose **`session_handle` is typed
    /// `s`, not `o`**. The interface says so out loud, and calls it a mistake it is
    /// keeping: "erroneously implemented as `s`. For backwards compatibility it will
    /// remain this type". Every one of the calls below then declares that same
    /// handle as `o`, so it is re-typed by [`Bus::path_of`] on its way back in, and
    /// [`SessionId`] is a `String` in the seam so that the wart lives here and not
    /// in the state machine.
    ///
    /// `SelectDevices(o, a{sv}) -> o handle` carries `types` (u), the device kinds
    /// wanted. `Start(o, s parent_window, a{sv}) -> o handle` is **the call that
    /// raises the dialog** on this half, and its response carries `devices` (u),
    /// which is what was actually granted rather than what was asked for.
    ///
    /// **No `ConnectToEIS` here, ever.** `RemoteDesktop` has one and this build must
    /// not call it: the interface makes the two mutually exclusive ("Once an EIS
    /// connection is established, input events must be sent exclusively via the EIS
    /// connection. Any events submitted via NotifyPointerMotion,
    /// NotifyKeyboardKeycode and other Notify* methods will return an error"), and
    /// [`crate::wayland`]'s header carries the argument for choosing the `Notify*`
    /// side.
    fn remote_start(&self, want: u32) -> Result<RemoteGrant, PortalError> {
        let session_token = self.token();
        let created = self.request(
            REMOTE_DESKTOP,
            "CreateSession",
            // No dialog on this one: the consent for this half is asked for by
            // `Start`, below.
            ANSWER_BUDGET,
            |token| {
                let mut options = Options::new();
                options.insert("handle_token", zvariant::Value::from(token));
                options.insert(
                    "session_handle_token",
                    zvariant::Value::from(session_token.as_str()),
                );
                self.conn.call_method(
                    Some(PORTAL_BUS),
                    PORTAL_PATH,
                    Some(REMOTE_DESKTOP),
                    "CreateSession",
                    &(options,),
                )
            },
        )?;
        let session = session_of(&created, "session_handle")?;
        // **From here every failure closes the session**, and `path_of` is inside that
        // and not before it, which is the whole point of the shape: a session left open
        // is a consent a person gave that nothing will ever use, and on some desktops an
        // indicator that stays lit for the rest of their login.
        //
        // The one case with no remedy from here is `path_of` itself failing, because
        // `close` needs the same conversion to address the session at all. A handle that
        // is not an object path cannot be closed by anybody, and the attempt is still
        // made so that the second warning names it: a leak that is reported beats a leak
        // that is not.
        let devices = match self
            .path_of(&session)
            .and_then(|path| self.start_devices(&path, want))
        {
            Ok(devices) => devices,
            Err(e) => {
                self.close(&session);
                return Err(e);
            }
        };
        Ok(RemoteGrant { session, devices })
    }

    /// One `Notify*` call.
    ///
    /// The mapping is a transcription: [`Notify`]'s fields are already the
    /// interface's own types, so nothing here converts. The `state` argument is 1 for
    /// pressed and 0 for released, and the axis numbers of the stepped scroll are
    /// [`crate::wayland::AXIS_VERTICAL`] and `AXIS_HORIZONTAL`, both of which the
    /// caller has already chosen.
    ///
    /// # What a dead session looks like here, and what it does not
    ///
    /// It answers an error, and that error goes through [`classify`] like any other,
    /// which for a compositor's own vocabulary means [`PortalError::Failed`]: worth
    /// retrying, and reported. It is deliberately NOT turned into
    /// [`PortalError::SessionClosed`], because no portal documents an error name that
    /// means "that session is gone" and guessing at one would put a session's death
    /// on the wrong evidence. The authoritative word for a session that has ended is
    /// the `Session.Closed` signal, which the pump delivers, and the state machine
    /// acts on that.
    ///
    /// # DEFERRED, three of them, and all three are named rather than hidden
    ///
    /// 1. **`NotifyPointerAxis` never sends `finish`.** The interface's options carry a
    ///    `finish` (b) that marks the end of a scroll gesture, and this build always
    ///    omits it. On a compositor with kinetic scrolling that means a fling may never
    ///    be told it has ended. It is omitted because the engine's own wheel event
    ///    (`crate::backend::BackendEvent::Wheel`) carries no notion of a gesture ending,
    ///    so there is nothing truthful to put in the flag: sending `finish` on every
    ///    call would kill every fling, and sending it on none is the current behaviour.
    ///    Fixing it properly means the source side learning to say "that scroll is
    ///    over", which is a change to the wire and not to this file.
    /// 2. **A portal RESTART is invisible to this half.** There is no
    ///    `NameOwnerChanged` subscription anywhere in this file, so if
    ///    `xdg-desktop-portal` is replaced under a live session, the first `Notify*`
    ///    after it finds out by erroring, and the session is only really known to be
    ///    dead when something says so. The capture half recovers by itself, because its
    ///    EI socket reaches EOF and [`EiStream::drain`] reports `gone`; the injection
    ///    half has no equivalent channel. A `NameOwnerChanged` watch is the fix and it
    ///    is a brick of its own (it is also what finding the portal's unique name for
    ///    sender checking would need, see the module header).
    /// 3. **This is a blocking round trip per event on a 125 Hz path.** Every keystroke
    ///    and every pointer delta waits for a method return, with the bus's own
    ///    twenty-five second ceiling behind it, on the thread that is also draining the
    ///    capture stream. Measured latency for the local leg is not the concern (a warm
    ///    session bus round trip is about a millisecond, and #123 measured the network
    ///    legs at 4 ms and 32 ms); the concern is the tail, where one slow reply stalls
    ///    the whole engine thread. It answers rather than fires and forgets because the
    ///    seam needs the failure (see above), and the way to have both is
    ///    `NO_REPLY_EXPECTED` plus a separate error-signal path, which is a redesign of
    ///    the seam and not a line here.
    fn remote_notify(&self, session: &SessionId, what: &Notify) -> Result<(), PortalError> {
        let path = self.path_of(session)?;
        let options = Options::new();
        /// The `u state` both the key and the button notifications carry: 1 pressed,
        /// 0 released, which is the interface's own numbering and the same on both.
        fn state(pressed: bool) -> u32 {
            u32::from(pressed)
        }
        self.plain(REMOTE_DESKTOP, || match what {
            Notify::PointerMotion { dx, dy } => self.conn.call_method(
                Some(PORTAL_BUS),
                PORTAL_PATH,
                Some(REMOTE_DESKTOP),
                "NotifyPointerMotion",
                &(&path, options, *dx, *dy),
            ),
            Notify::PointerButton { button, pressed } => self.conn.call_method(
                Some(PORTAL_BUS),
                PORTAL_PATH,
                Some(REMOTE_DESKTOP),
                "NotifyPointerButton",
                &(&path, options, *button, state(*pressed)),
            ),
            Notify::PointerAxis { dx, dy } => self.conn.call_method(
                Some(PORTAL_BUS),
                PORTAL_PATH,
                Some(REMOTE_DESKTOP),
                "NotifyPointerAxis",
                &(&path, options, *dx, *dy),
            ),
            Notify::PointerAxisDiscrete { axis, steps } => self.conn.call_method(
                Some(PORTAL_BUS),
                PORTAL_PATH,
                Some(REMOTE_DESKTOP),
                "NotifyPointerAxisDiscrete",
                &(&path, options, *axis, *steps),
            ),
            Notify::KeyboardKeycode { keycode, pressed } => self.conn.call_method(
                Some(PORTAL_BUS),
                PORTAL_PATH,
                Some(REMOTE_DESKTOP),
                "NotifyKeyboardKeycode",
                &(&path, options, *keycode, state(*pressed)),
            ),
            Notify::KeyboardKeysym { keysym, pressed } => self.conn.call_method(
                Some(PORTAL_BUS),
                PORTAL_PATH,
                Some(REMOTE_DESKTOP),
                "NotifyKeyboardKeysym",
                &(&path, options, *keysym, state(*pressed)),
            ),
        })
    }

    /// `org.freedesktop.portal.Session.Close()`, sent and not awaited.
    ///
    /// # Why this is a hand-built message and not a `call_method`
    ///
    /// Because `call_method` waits for the reply, with the D-Bus default timeout of
    /// twenty-five seconds, and this function is called from every teardown path
    /// there is, including the ones running because something is already wrong. A
    /// teardown that can block for twenty-five seconds is a teardown that will one
    /// day be interrupted, leaving a capture the compositor still believes in.
    ///
    /// So the call carries `NO_REPLY_EXPECTED` and this returns as soon as the
    /// message is written. What is given up is knowing whether it worked, which this
    /// function was never allowed to report anyway, and the backstop is the
    /// interface's own: a client that vanishes from the bus has all of its sessions
    /// closed for it.
    ///
    /// # The residual exposure, which this function does NOT remove
    ///
    /// **This call is not the whole teardown, and its two neighbours on the same path
    /// are ordinary round trips with the bus's own twenty-five second ceiling.**
    /// `crate::wayland::Capture::close` calls `capture_release` before it reaches here,
    /// and `crate::wayland::Capture::disarm` calls `capture_disable`, and both wait for
    /// a reply. So hardening this one message narrows the window and does not shut it,
    /// and a reader must not take the argument above as a claim that the path is safe.
    ///
    /// They are round trips on purpose and are left that way. `capture_release`'s answer
    /// is load-bearing: `crate::wayland::Capture::stop` reads it and kills the session on
    /// a failure that is not worth retrying, because a release that did not arrive leaves
    /// the compositor holding somebody's pointer, which is the one failure that must not
    /// be quiet. Making it fire-and-forget would trade a bounded wait for a silent
    /// swallowed pointer, which is the worse of the two. Bounding it properly means a
    /// per-call D-Bus timeout, which `zbus`'s blocking API does not offer, so it is a
    /// brick of its own (an async call with a `select`, or a reply-matching thread) and
    /// is on the deferred list.
    fn close(&self, session: &SessionId) {
        let path = match self.path_of(session) {
            Ok(path) => path,
            Err(e) => {
                warn(&format!("a session cannot be closed: {e}"));
                return;
            }
        };
        self.forget(path.as_str(), SESSION, "Close", "closing a session");
    }

    fn take_signals(&self) -> Vec<Signal> {
        drain_signals(&self.inbox)
    }
}

/// The body of [`Portal::take_signals`], as a free function over the [`Inbox`].
///
/// Split out for one reason and it is a good one: the drain has behaviour worth a test
/// (the bound, the terminal signal that survives it, the drop count reported once and
/// then forgotten) and every one of those is a property of the inbox rather than of the
/// bus. A test that reached into `state.signals` itself would assert on the storage and
/// prove nothing about the method a caller actually uses.
fn drain_signals(inbox: &Inbox) -> Vec<Signal> {
    let mut state = lock(&inbox.state);
    if state.dropped != 0 {
        // Reported at the drain rather than at the drop, so that a storm costs one line
        // and not one line per signal, which is the mistake the Android log flood taught
        // this repository.
        warn(&format!(
            "{} portal signals were dropped because nothing drained them",
            state.dropped
        ));
        state.dropped = 0;
    }
    std::mem::take(&mut state.signals)
}

impl Bus {
    /// `SelectDevices` then `Start`, the two calls that follow a `RemoteDesktop`
    /// session into existence. Split out so that [`Portal::remote_start`] has one
    /// error path to close the session on.
    fn start_devices(
        &self,
        path: &zvariant::OwnedObjectPath,
        want: u32,
    ) -> Result<u32, PortalError> {
        self.request(REMOTE_DESKTOP, "SelectDevices", ANSWER_BUDGET, |token| {
            let mut options = Options::new();
            options.insert("handle_token", zvariant::Value::from(token));
            options.insert("types", zvariant::Value::from(want));
            self.conn.call_method(
                Some(PORTAL_BUS),
                PORTAL_PATH,
                Some(REMOTE_DESKTOP),
                "SelectDevices",
                &(path, options),
            )
        })?;
        let started = self.request(
            REMOTE_DESKTOP,
            "Start",
            // The dialog is here.
            CONSENT_BUDGET,
            |token| {
                let mut options = Options::new();
                options.insert("handle_token", zvariant::Value::from(token));
                self.conn.call_method(
                    Some(PORTAL_BUS),
                    PORTAL_PATH,
                    Some(REMOTE_DESKTOP),
                    "Start",
                    // The empty `parent_window`, for the reason
                    // `capture_create_session` gives: no window exists to parent a
                    // dialog to.
                    &(path, "", options),
                )
            },
        )?;
        // What was GRANTED, which the interface says need not be what was asked for.
        u32_of(&started, "devices")
    }
}

// ------------------------------------------------------------------ the EI stream

/// The mio token of the EI socket. One source, so one token.
const EI_TOKEN: mio::Token = mio::Token(0);

/// The EI event stream of one capture session.
///
/// # Why the converter is held here and not on a thread of its own
///
/// `reis::event::EiEventConverter` is neither `Send` nor `Sync`: it stores
/// `Box<dyn FnOnce(u64)>` callbacks, which carry no `Send` bound. So it has to live
/// on whichever thread polls it, and [`EiSource`] is declared without a `Send` bound
/// for exactly that reason. The Wayland backend has one thread and this lives on it,
/// next to the `mio::Poll` that wakes it.
struct EiStream {
    /// The socket and the protocol buffer. `Clone`, `Send` and `Sync`, unlike the
    /// converter, but there is no reason to hand a copy anywhere.
    context: reis::ei::Context,
    /// The frame accumulator, taken out and abandoned if it ever panics. See
    /// [`EiStream::poll`].
    converter: Option<reis::event::EiEventConverter>,
    poll: mio::Poll,
    events: mio::Events,
    /// Events decoded by [`EiStream::open`]'s priming pass, before the caller ever
    /// polls. Normally empty, and never carrying input: the portal's `Enable` has not
    /// been called when priming runs, so there is nothing for a compositor to emulate
    /// yet. Kept rather than discarded because "normally" is not "provably".
    primed: Vec<crate::wayland::EiEvent>,
    /// Sticky. Once the far end is gone it stays gone, and no later poll touches the
    /// socket: the session above is ending and asking a dead socket once per turn
    /// until it does would fill a log.
    gone: bool,
}

impl EiStream {
    /// Handshakes on the descriptor `ConnectToEIS` handed over, and answers a stream
    /// ready to be polled.
    ///
    /// # The handshake is driven by hand, and that is deliberate
    ///
    /// `reis` offers `handshake::ei_handshake_blocking`, which is one call and does
    /// exactly this. It is not used because it polls the socket with **no timeout**:
    /// a compositor that accepts the connection and then says nothing would wedge
    /// this backend's thread for ever, with no way out and nothing on screen. So the
    /// same `EiHandshaker` state machine is driven here against a deadline, and a
    /// compositor that goes quiet becomes [`PortalError::Timeout`], which is a
    /// sentence and a retry.
    fn open(socket: std::os::unix::net::UnixStream) -> Result<EiStream, PortalError> {
        // `Context::new` puts the socket into non-blocking mode, which is what makes
        // the poll loop below correct; its only failure is that call failing.
        let context = reis::ei::Context::new(socket).map_err(|e| PortalError::Failed {
            name: LOCAL.to_string(),
            message: format!("the EI socket could not be prepared: {e}"),
        })?;
        let mut poll = mio::Poll::new().map_err(|e| PortalError::Failed {
            name: LOCAL.to_string(),
            message: format!("no mio poll for the EI socket: {e}"),
        })?;
        // Registered by raw descriptor through `SourceFd`, as the X11 backend
        // registers the X socket, because the descriptor belongs to the context and
        // must not be handed to mio to own.
        poll.registry()
            .register(
                &mut mio::unix::SourceFd(&context.as_raw_fd()),
                EI_TOKEN,
                mio::Interest::READABLE,
            )
            .map_err(|e| PortalError::Failed {
                name: LOCAL.to_string(),
                message: format!("the EI socket could not be registered: {e}"),
            })?;
        let mut events = mio::Events::with_capacity(1);

        let mut handshaker = reis::handshake::EiHandshaker::new(
            // The name a compositor may show a person, so it is the product's name
            // and not the crate's.
            "1device",
            // **Receiver, and never Sender.** A receiver takes events and sends
            // none: every emulation request belongs to a sender context and would be
            // a protocol violation from here. The injection half of this component
            // does not use EI at all (see `remote_notify`), so this is the only
            // context type this build ever asks for.
            reis::ei::handshake::ContextType::Receiver,
        );
        let deadline = Instant::now() + ANSWER_BUDGET;
        let resp = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(PortalError::Timeout);
            }
            if let Err(e) = poll.poll(&mut events, Some(left)) {
                if e.kind() == std::io::ErrorKind::Interrupted {
                    // A signal, not a failure. The deadline above is what ends this
                    // loop, so retrying cannot spin for ever.
                    continue;
                }
                return Err(PortalError::Failed {
                    name: LOCAL.to_string(),
                    message: format!("waiting on the EI socket failed: {e}"),
                });
            }
            if let Err(e) = context.read() {
                return Err(ei_read_error(&e));
            }
            let mut done = None;
            while let Some(pending) = context.pending_event() {
                let event = match pending {
                    reis::PendingRequestResult::Request(event) => event,
                    reis::PendingRequestResult::ParseError(e) => {
                        return Err(PortalError::Malformed(format!(
                            "the EI handshake could not be parsed: {e}"
                        )));
                    }
                    reis::PendingRequestResult::InvalidObject(id) => {
                        // An event for an object this side has already destroyed.
                        // Skippable by construction, and nothing is destroyed during
                        // a handshake, so it is also not expected.
                        warn(&format!("the EI handshake mentioned object {id}"));
                        continue;
                    }
                };
                match handshaker.handle_event(event) {
                    Ok(Some(resp)) => {
                        done = Some(resp);
                        break;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        return Err(PortalError::Malformed(format!(
                            "the EI handshake failed: {e}"
                        )));
                    }
                }
            }
            if let Some(resp) = done {
                break resp;
            }
        };

        let mut stream = EiStream {
            converter: Some(reis::event::EiEventConverter::new(&context, resp)),
            context,
            poll,
            events,
            primed: Vec::new(),
            gone: false,
        };

        // **The priming pass, and without it the first activation of every session can
        // deliver nothing.**
        //
        // Nothing arrives over EI until `bind_capabilities` has been sent for a seat,
        // and the seat only exists once `EiEvent::SeatAdded` has come out of the
        // converter. That happens in `translate`, which only runs from `poll`. So
        // without this pass the sequence is: `open` returns, the caller runs
        // `GetZones`, `SetPointerBarriers` and `Enable`, the compositor starts watching
        // the barriers, somebody crosses one, and only THEN does the first `poll` bind
        // the seat that the events needed.
        //
        // And it is worse than a race, because the bytes are usually already here. The
        // compositor sends the seat burst right behind the handshake reply, and the
        // handshake's own `context.read()` drains the socket to exhaustion, so those
        // bytes are sitting in `reis`'s internal buffer with the kernel socket empty.
        // mio's epoll is edge triggered: it has nothing left to report, so the first
        // `poll` would block for its whole budget before looking at a buffer that was
        // already full.
        //
        // Draining once here closes both. It cannot block: the socket is non-blocking,
        // so a read with nothing behind it answers zero rather than EOF.
        let mut primed = Vec::new();
        stream.drain(&mut primed);
        stream.primed = primed;
        if stream.gone {
            // The stream died between the handshake and this line, which means there is
            // no capture to hand back. Reported rather than returned as a live stream
            // that will answer `gone` on its first use, so the caller closes the portal
            // session it has just consented to instead of arming it.
            return Err(PortalError::SessionClosed);
        }
        Ok(stream)
    }

    /// Binds what this engine consumes, and flushes, which is what makes anything
    /// arrive at all.
    ///
    /// **Nothing is delivered before this.** A seat advertises capabilities and the
    /// client has to bind the ones it wants; an unbound seat produces no device and
    /// therefore no event, and the flush is what puts the request on the wire.
    ///
    /// Four capabilities and not seven. `Touch` and `PointerAbsolute` are left
    /// unbound because this engine carries neither (doc/input-sharing.md, section
    /// 15), and `Text` because the character a key produced is not read from EI at
    /// all. Asking for a capability that is never used is asking a person to consent
    /// to something pointless, which is the same rule
    /// [`crate::wayland::caps::WANTED`] follows one layer up.
    ///
    /// # DEFERRED: what the seat actually offered is not checked
    ///
    /// `reis`'s capability map answers 0 for a capability the seat never advertised
    /// ("Defaults to 0", its own words), and those zeroes are simply OR-ed into the
    /// mask. So a seat offering no relative pointer binds successfully, reports nothing
    /// wrong, and forwards nothing, which from here looks exactly like a quiet desk.
    /// Reading the advertised set back and refusing the session with a reason needs
    /// either an addition to `reis` (`Seat` exposes no accessor for it, only the
    /// `TODO: has_capability` its author left) or tracking the `ei_seat.capability`
    /// events before `done` by hand. On the deferred list, and named here because it is
    /// the second way this stream can be armed and mute.
    ///
    /// # A failed flush ends the stream, and it used to only warn
    ///
    /// `flush` is what puts the bind on the wire. If it fails, the compositor never
    /// hears it, no device is ever created, and not one event will ever arrive on this
    /// socket. The session above would sit in `crate::wayland::Phase::Armed`, the
    /// interface would say this machine can drive, and every crossing would take
    /// somebody's pointer and deliver nothing.
    ///
    /// This used to be a single WARN line, on the stated grounds that "the silence is
    /// what the engine's own timeout sees". **There is no such timeout.** Nothing in
    /// `crate::wayland` measures the gap between an activation and its first event;
    /// `POLL_ACTIVE` and `POLL_IDLE` are poll budgets, not stall detectors. So the claim
    /// was false and it hid the bug behind itself, which is the worse of its two
    /// faults. Setting `gone` makes the stream report itself dead, which
    /// `crate::wayland::Capture::poll` already turns into a session that ends with a
    /// reason a person can be shown.
    ///
    /// `flush` answers a `rustix::io::Result`, and `rustix` is not re-exported by
    /// `reis`, so the error type cannot be named here: it is rendered through `Display`
    /// and nothing branches on which failure it was.
    fn bind(&mut self, seat: &reis::event::Seat) {
        use reis::event::DeviceCapability;
        seat.bind_capabilities(
            DeviceCapability::Pointer
                | DeviceCapability::Keyboard
                | DeviceCapability::Scroll
                | DeviceCapability::Button,
        );
        if let Err(e) = self.context.flush() {
            warn(&format!(
                "the EI capability bind could not be flushed ({e}), so this stream can never \
                 deliver an event: ending it"
            ));
            self.gone = true;
        }
    }
}

/// What a failed `Context::read` means.
///
/// `UnexpectedEof` is how the far end going away is reported, and it is the ordinary
/// end of every capture session: the compositor closes the socket when the portal
/// session closes. Anything else is a real socket failure.
fn ei_read_error(e: &std::io::Error) -> PortalError {
    if e.kind() == std::io::ErrorKind::UnexpectedEof {
        PortalError::SessionClosed
    } else {
        PortalError::Failed {
            name: LOCAL.to_string(),
            message: format!("reading the EI socket failed: {e}"),
        }
    }
}

impl EiSource for EiStream {
    /// One turn: hand over whatever is already decoded, and if there is nothing, wait
    /// up to `budget` for more.
    ///
    /// # Why the drain comes BEFORE the wait
    ///
    /// Because `reis` has a buffer of its own and mio cannot see into it. `reis`'s
    /// `read` drains the kernel socket to exhaustion, so a turn that reads a batch and
    /// leaves an incomplete message behind (or a batch whose events are still waiting
    /// for their frame) leaves data this side of the socket. mio's epoll is edge
    /// triggered, so it will not report readiness for bytes that have already been read
    /// off. Waiting first would therefore be waiting on the wrong thing.
    ///
    /// The earlier comment here claimed "there is never data left behind for a
    /// readiness that will not fire again". That is true of the KERNEL socket and false
    /// of `reis`'s buffer, and the difference is exactly where the seat setup sits after
    /// a handshake, which is what [`EiStream::open`]'s priming pass exists to deal with.
    ///
    /// # Events but no frames
    ///
    /// `reis` accumulates by FRAME, as the protocol does: a motion, a button or a key
    /// goes into a per-device queue and comes out only when that device's
    /// `ei_device.frame` arrives, which is what makes a pointer move and a button press
    /// that happened together arrive together. So a turn that read bytes and yielded no
    /// event is normal, the frame has not landed.
    ///
    /// **A stream that keeps doing it is not detected by anything.** A compositor that
    /// stops framing looks exactly like a quiet desk from here, and there is no stall
    /// detector anywhere in `crate::wayland` to tell them apart: `POLL_ACTIVE` and
    /// `POLL_IDLE` there are poll budgets, not watchdogs. This is stated rather than
    /// waved at, because an earlier version of this comment claimed the engine would
    /// notice, and a comment that invents a safety net is worse than none.
    ///
    /// # The panic, and the exact width of the catch
    ///
    /// `reis` documents that `EiEventConverter::handle_event` "will panic if an internal
    /// Mutex is poisoned, or if an event carrying a timestamp has no device". The second
    /// is an `expect` on a path a malformed frame from a compositor can reach.
    ///
    /// Uncaught, it would kill the Wayland backend's thread and not the process, so
    /// nothing would ever report [`EiPoll::gone`] and the session would sit armed and
    /// mute for ever. So it is caught, and the converter is then **abandoned**: taken
    /// out of its slot and never touched again.
    ///
    /// **The catch is narrower than the risk, and that is worth knowing.** Only
    /// `handle_event` is inside it. `Context::pending_event` takes `reis`'s read mutex
    /// with an `unwrap` of its own, and [`EiStream::bind`] reaches its write mutex and
    /// its flush the same way, so a poisoned mutex can be raised by a line this catch
    /// does not cover, and that panic still kills the thread. Widening it means wrapping
    /// three more calls whose failure leaves the same unknown state, which buys little,
    /// so what is done instead is to name it. (`next_event` is safe: it is a
    /// `pop_front`.)
    ///
    /// `AssertUnwindSafe` is required because the converter is not `UnwindSafe`.
    /// **The early return below is what makes the assertion sound**, not the catch
    /// itself: what `UnwindSafe` guards against is observing half-updated state after a
    /// panic, and the guarantee here is that the function returns immediately and the
    /// converter is gone from its slot, so no later line can observe it. An edit that
    /// removes that early return removes the whole argument.
    ///
    /// And it is not dropped. `reis`'s own `Drop` for the converter locks the same
    /// mutexes that may now be poisoned, and a panic while unwinding aborts the process,
    /// so `mem::forget` is the safe end of it. **What that leaks is not just the frame
    /// queues**: the converter holds an `Arc` on the `ei::Context`, which owns the
    /// `UnixStream`, so forgetting it holds the EIS file descriptor, and the
    /// compositor's peer end of it, open for the life of the process. Once per panicking
    /// session. Dropping it inside a second `catch_unwind` would recover the descriptor
    /// in the common case (the mutex is only poisoned if the panic was a poisoning one),
    /// and is not done because it puts a second panic-in-drop on the same path for a
    /// resource that leaks once on a path that is already a bug report. Both halves are
    /// written down so the trade can be revisited rather than rediscovered.
    fn poll(&mut self, budget: Duration) -> EiPoll {
        if self.gone {
            return EiPoll {
                events: Vec::new(),
                gone: true,
            };
        }
        let mut out = std::mem::take(&mut self.primed);
        self.drain(&mut out);
        if out.is_empty() && !self.gone && self.wait(budget) {
            self.drain(&mut out);
        }
        EiPoll {
            events: out,
            gone: self.gone,
        }
    }
}

impl EiStream {
    /// Waits up to `budget` for the socket to have something. Answers whether reading
    /// it is worth a try.
    fn wait(&mut self, budget: Duration) -> bool {
        match self.poll.poll(&mut self.events, Some(budget)) {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                // A signal, not a failure, and not a reason to end the stream. The
                // caller polls again.
                false
            }
            Err(e) => {
                warn(&format!("waiting on the EI socket failed: {e}"));
                self.gone = true;
                false
            }
        }
    }

    /// Reads whatever the socket has, runs it through the converter, and translates
    /// everything the converter lets out. Never blocks.
    ///
    /// The single registered source means the readiness token carries no information
    /// worth branching on, so the read is unconditional.
    fn drain(&mut self, out: &mut Vec<crate::wayland::EiEvent>) {
        if self.gone {
            return;
        }
        if let Err(e) = self.context.read() {
            let why = ei_read_error(&e);
            // A closed socket is the ordinary end of a capture and needs no line of its
            // own; anything else is a fault worth naming.
            if why != PortalError::SessionClosed {
                warn(&format!("{why}"));
            }
            self.gone = true;
            return;
        }
        let Some(converter) = self.converter.as_mut() else {
            // Abandoned by a caught panic on an earlier turn, which also set `gone`.
            self.gone = true;
            return;
        };
        let mut panicked = false;
        while let Some(pending) = self.context.pending_event() {
            let event = match pending {
                reis::PendingRequestResult::Request(event) => event,
                reis::PendingRequestResult::ParseError(e) => {
                    warn(&format!("an EI message could not be parsed: {e}"));
                    // The buffer's framing is no longer trustworthy after a parse
                    // error, so the stream ends rather than being resynchronised on a
                    // guess.
                    self.gone = true;
                    break;
                }
                reis::PendingRequestResult::InvalidObject(id) => {
                    // An event for an object already destroyed. `reis` reports it
                    // separately from a parse error precisely because the stream is
                    // still in sync, so it is skipped and nothing else changes.
                    warn(&format!("an EI event named the destroyed object {id}"));
                    continue;
                }
            };
            let handled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                converter.handle_event(event)
            }));
            match handled {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    warn(&format!("an EI event violated the protocol: {e}"));
                    self.gone = true;
                    break;
                }
                Err(_) => {
                    warn("the EI event converter panicked; abandoning this capture's stream");
                    panicked = true;
                    self.gone = true;
                    break;
                }
            }
        }
        if panicked {
            // The early return the `AssertUnwindSafe` argument rests on. Nothing below
            // may observe the abandoned converter, and nothing does, because this
            // returns here.
            if let Some(converter) = self.converter.take() {
                std::mem::forget(converter);
            }
            return;
        }
        // Borrowed again rather than kept from above, because the loop that follows
        // calls `self.translate`, which needs `&mut self`.
        while let Some(event) = self
            .converter
            .as_mut()
            .and_then(reis::event::EiEventConverter::next_event)
        {
            self.translate(event, out);
        }
    }
}

impl EiStream {
    /// One `reis` event as zero or one of this engine's, with the two that are
    /// answered here rather than passed up.
    ///
    /// The numbers travel unchanged and that is the point: buttons and keys are Linux
    /// evdev codes on both sides, the deltas are the compositor's own accelerated
    /// movement, and the stepped scroll's `discrete_dx`/`discrete_dy` go straight
    /// through because the divisor ([`crate::wayland::SCROLL_STEP`]) belongs to the
    /// decoder and not to the transport.
    fn translate(&mut self, event: reis::event::EiEvent, out: &mut Vec<crate::wayland::EiEvent>) {
        use crate::wayland::EiEvent as Up;
        use reis::event::EiEvent as Ei;
        match event {
            // Answered here and not surfaced: binding is a protocol obligation of
            // this transport rather than an event the engine has anything to do
            // about.
            Ei::SeatAdded(added) => self.bind(&added.seat),
            Ei::Disconnected(bye) => {
                warn(&format!(
                    "the compositor disconnected the EI stream ({:?}): {}",
                    bye.reason,
                    bye.explanation.as_deref().unwrap_or("no explanation")
                ));
                // Marked gone here and not left to the next `read`. The socket does
                // normally close right behind this event, which would make the next
                // read answer `UnexpectedEof` and reach the same place; but the
                // protocol does not promise that ordering, and a stream that has been
                // told it is disconnected must not be polled again as though it might
                // still deliver a keystroke.
                self.gone = true;
            }
            // The sequence the portal's `activation_id` is checked against. It is the
            // one number that MUST be threaded through unchanged.
            Ei::DeviceStartEmulating(start) => out.push(Up::StartEmulating {
                sequence: start.sequence,
            }),
            Ei::DeviceStopEmulating(_) => out.push(Up::StopEmulating),
            Ei::PointerMotion(motion) => out.push(Up::Motion {
                dx: motion.dx,
                dy: motion.dy,
            }),
            Ei::Button(button) => out.push(Up::Button {
                button: button.button,
                down: button.state == reis::ei::button::ButtonState::Press,
            }),
            Ei::ScrollDelta(scroll) => out.push(Up::ScrollDelta {
                dx: scroll.dx,
                dy: scroll.dy,
            }),
            Ei::ScrollDiscrete(scroll) => out.push(Up::ScrollDiscrete {
                dx: scroll.discrete_dx,
                dy: scroll.discrete_dy,
            }),
            Ei::KeyboardKey(key) => out.push(Up::Key {
                keycode: key.key,
                down: key.state == reis::ei::keyboard::KeyState::Press,
            }),
            Ei::KeyboardModifiers(mods) => out.push(Up::Modifiers {
                depressed: mods.depressed,
                latched: mods.latched,
                locked: mods.locked,
            }),
            // Everything else, and there is a lot of it: the frame that committed the
            // events above (already accounted for by the accumulation), the device
            // and seat lifecycle, the pause and resume, the absolute pointer, touch
            // and the text interfaces this build binds no capability for and so can
            // never receive. A wildcard rather than a list, because `reis`'s event
            // enum is a protocol's worth of variants and the seam's is eight: naming
            // them all here would be a list to maintain with nothing checking it.
            _ => {}
        }
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

        for (interface, floor) in [
            (
                crate::wayland::INPUT_CAPTURE,
                crate::wayland::INPUT_CAPTURE_MIN,
            ),
            (
                crate::wayland::REMOTE_DESKTOP,
                crate::wayland::REMOTE_DESKTOP_MIN,
            ),
        ] {
            match portal.property_u32(interface, "version") {
                Ok(version) => {
                    // A machine that HAS the portal. There is nothing to assert about
                    // the NUMBER, since it is whatever this desktop ships, so what is
                    // asserted is that the build has a coherent reading of it: below
                    // the floor it must name the version as the reason and refuse, at
                    // or above it the version must not be a reason at all. The old
                    // `version <= 100` here asserted nothing of the kind (a portal
                    // reporting 0 sailed through it) while its comment discussed
                    // exactly this.
                    let offer = crate::wayland::Offer::Present {
                        version,
                        capabilities: crate::wayland::caps::WANTED,
                    };
                    let why = offer.why(interface, floor, crate::wayland::caps::WANTED);
                    if version < floor {
                        assert!(
                            matches!(why, Some(PortalError::TooOld { .. })),
                            "{interface} at version {version} is below the floor {floor} and \
                             this build did not say so: {why:?}"
                        );
                        assert!(!offer.usable(floor, crate::wayland::caps::WANTED));
                    } else {
                        assert_eq!(
                            why, None,
                            "{interface} at version {version} is at or above the floor {floor}, \
                             so the version cannot be what makes it unusable"
                        );
                        assert!(offer.usable(floor, crate::wayland::caps::WANTED));
                    }
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

    /// **The mangling rule, which is what makes a `Request` path predictable.**
    ///
    /// Getting it wrong is the one bug in this file that no error would name: the
    /// call would succeed, the `Response` would arrive on a path nothing is watching,
    /// and every portal call would end in [`PortalError::Timeout`]. It is a pure
    /// string rule, so it is provable here with no bus at all.
    #[test]
    fn a_unique_bus_name_becomes_a_path_element() {
        assert_eq!(mangle_sender(":1.42"), "1_42");
        assert_eq!(mangle_sender(":1.2.3"), "1_2_3");
        // Already bare, which is not a name a bus assigns but is what the strip has
        // to do with one.
        assert_eq!(mangle_sender("1_42"), "1_42");
        assert_eq!(
            request_path("1_42", "onedevice_dead_0_beef"),
            "/org/freedesktop/portal/desktop/request/1_42/onedevice_dead_0_beef"
        );
    }

    /// A token is unguessable AND a legal object path element, and the second half is
    /// the one a compiler cannot check: a token with a dot in it would make every
    /// portal call fail with `InvalidArgs`, which [`classify`] reads as "the interface
    /// is not there" and would report as a missing portal on a machine that has one.
    #[test]
    fn a_token_is_a_legal_path_element_and_changes_every_time() {
        let prefix = token_prefix();
        let mut seen = std::collections::HashSet::new();
        for counter in 0..64u64 {
            let token = token_text(&prefix, counter, rand::random::<u64>());
            assert!(
                token
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
                "{token} is not a legal object path element"
            );
            assert!(seen.insert(token), "a token repeated");
        }
    }

    fn activated(activation: u32) -> Signal {
        Signal::Activated {
            session: SessionId("/s".to_string()),
            activation,
            cursor: None,
            barrier: None,
        }
    }

    /// **The signal buffer's bound, and the one signal it must never lose.**
    ///
    /// A compositor that emits without end, or an engine thread that has stopped
    /// draining, must not grow this process's memory; and whatever else is evicted, a
    /// `Closed` must survive, because it is the only notification that a session is
    /// dead and losing it leaves the engine armed for a capture nobody is in.
    #[test]
    fn a_storm_of_signals_is_bounded_and_never_evicts_a_closed_one() {
        let inbox = Inbox::default();
        let closed = Signal::Closed {
            session: SessionId("/gone".to_string()),
        };
        file(&inbox, closed.clone());
        // A second one for the same session says nothing new.
        file(&inbox, closed.clone());
        for activation in 0..(SIGNAL_CAP as u32 * 3) {
            file(&inbox, activated(activation));
        }

        assert!(
            lock(&inbox.state).dropped > 0,
            "the drops were counted rather than hidden"
        );

        // Through the real drain, which is what `Portal::take_signals` calls, and not
        // by reaching into the storage: the point of the test is the behaviour a caller
        // gets.
        let held = drain_signals(&inbox);
        assert_eq!(held.len(), SIGNAL_CAP, "the buffer is bounded");
        assert_eq!(
            held.iter().filter(|s| **s == closed).count(),
            1,
            "the terminal signal survived the storm, exactly once"
        );
        assert_eq!(
            lock(&inbox.state).dropped,
            0,
            "the drop count is reported once and then forgotten, so a storm costs one log line"
        );
        assert!(
            drain_signals(&inbox).is_empty(),
            "and the drain really drained"
        );
    }
}
