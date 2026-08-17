// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The engine's whole state and every rule that reads a clock
//! (doc/input-sharing.md, sections 4, 5, 7, 8 and 12).
//!
//! One struct, no locks. [`Engine`] holds the plane, the graph, the settings, the
//! directory, the per peer channel state, the two session halves and the held
//! set; [`crate::orchestrator`] owns the `select!` loop that feeds it and the
//! tasks that carry its [`Effect`]s. Nothing here spawns, dials or awaits the
//! Core, which is what lets every rule below be proven against
//! [`crate::fake::FakeBackend`] with no desk and no Core in the room.
//!
//! # Time is a parameter, never a call
//!
//! Every function that needs "now" takes `now: std::time::Instant`. There is no
//! clock trait in this repository and this needs none: the dwell, the double tap,
//! the token bucket, the session idle, the stall watchdog and the emission
//! coalescing are then arithmetic a unit test does in nanoseconds. The loop is
//! the only caller of `Instant::now`.
//!
//! # One deadline, and what it is not
//!
//! [`Engine::next_deadline`] is the single point in time the loop must be woken
//! at, and [`Engine::pump`] does everything that is due. The FLOW contributes
//! exactly one deadline to it, the trailing-edge flush of a coalesced position
//! (D5), and it is armed only while a position is pending: #123 measured a
//! `tokio::time::sleep` waking 1.158 ms late at p50 against 0.35 ms of network
//! jitter, so the flow is driven by capture events and never by a tick. The other
//! deadlines (the keepalive, the dwell, the session idle, the stall, the layout
//! sweep, the emission window) are not the flow and are not paced by it.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::backend::{
    Action, BackendEvent, Capabilities, CaptureLoss, CaptureMode, InputBackend, KeyEvent, Monitor,
    PeerProblem, Point, Rect, Refusal,
};
use crate::graph::{self, AtEdge, Graph, Guards, Placed, Segment};
use crate::keys::{self, Held, Hotkey, ModKeys, Resolver};
use crate::plane::{self, Plane, Spot};
use crate::settings::{Applied, Settings};
// `HELD_FILE` is used by the tests alone: the engine reads that file through
// `Store::load_held`, which is deliberately more tolerant than the generic
// reader, and the tests read it raw to check what really reached the disk.
#[cfg(test)]
use crate::store::HELD_FILE;
use crate::store::{SETTINGS_FILE, Store};
use crate::wire::{self, Frame, KeyMode, Mode, ended, refused, stopped};

/// No frame from the source for this long and the target ends the session with
/// `end IDLE`, releasing every key. Tighter than the Core's 10 s channel sweep on
/// purpose: a source that went quiet mid-session is a hung source, and a target
/// sitting with Control held is exactly the failure this feature must not have.
pub const SESSION_IDLE: Duration = Duration::from_secs(5);

/// No `pong` for this long while driving and the source brings the keyboard home
/// by itself. Not negotiated with the target, because a hung target must not be
/// able to keep your keyboard (D20).
pub const SOURCE_STALL: Duration = Duration::from_secs(2);

/// How long a crossing waits for a channel that is still opening. A LAN open
/// finishes inside the dwell; a cold relay open (134 to 151 ms, #123) finishes
/// inside this bound; anything slower is refused with a sentence while the
/// channel keeps warming, so the second attempt succeeds.
pub const CROSS_OPEN_BOUND: Duration = Duration::from_secs(1);

/// Keepalive cadence on a warm channel with no session, comfortably inside the
/// Core's 10 s sweep even through a scheduler hiccup.
pub const WARM_PING: Duration = Duration::from_secs(3);

/// Keepalive cadence while a session is live: the number the interface shows
/// stays honest and a degrading path is noticed.
pub const SESSION_PING: Duration = Duration::from_secs(1);

/// The layout's periodic backstop. Rounds also run at start, when a peer becomes
/// reachable, when our monitors change, when a human drags and when a merge
/// changed something; this is what catches whatever those miss.
pub const LAYOUT_SWEEP: Duration = Duration::from_secs(60);

/// At most one layout message per peer per this long, coalesced.
pub const LAYOUT_MIN_GAP: Duration = Duration::from_secs(2);

/// `input.updated` is coalesced to at most ten per second: a crossing changes the
/// state, a pointer position does not.
pub const EMIT_GAP: Duration = Duration::from_millis(100);

/// One refusal per code per device per this long, with a count. Both the `oops`
/// frame and the `input.refused` notification.
pub const REFUSAL_WINDOW: Duration = Duration::from_secs(1);

/// Round trip under this many milliseconds: hand the pointer over silently, and
/// coalesce at the fast ceiling.
pub const RTT_SILENT_MS: u64 = 10;

/// Round trip above this many milliseconds: decline the pointer and offer the
/// keyboard-only session (`INPUT_TOO_SLOW`).
pub const RTT_MAX_MS: u64 = 60;

/// Coalescing ceiling on a fast path, in frames per second (#123: 125 and 250 Hz
/// carried cleanly on every path measured).
pub const FLOW_RATE_FAST: f64 = 250.0;
/// Coalescing ceiling above [`RTT_SILENT_MS`]. Halving the ceiling on a slow path
/// halves the queue a freeze can build (#123 saw one freeze above 20 ms at
/// 1000 Hz over a relay, with 19 stale frames behind it).
pub const FLOW_RATE_SLOW: f64 = 125.0;
/// The token bucket's burst allowance, and no more.
const FLOW_BURST: f64 = 2.0;

/// How long an `input.take` waiting for its channel to warm stays valid.
///
/// Deliberately NOT [`CROSS_OPEN_BOUND`]: that bound is about a crossing, where a
/// session starting a second after the hand moved on would be a surprise. A button
/// press is an explicit intent, and it deserves the open's real budget (a cold
/// relay open is 134 to 151 ms, #123, and a first open can be slower). Past this
/// the take is dropped with a word rather than fired into a hand that has moved
/// on, and the gesture's own answer said `{}` long before either way.
pub const TAKE_PARK_BOUND: Duration = Duration::from_secs(5);

/// How long a `peer.channel_closed` is ignored for a channel that has only just
/// attached.
///
/// The notification identifies the PAIR and not the channel (doc/core-api.md),
/// so a `REPLACED` for the channel a peer's own open displaced can arrive after
/// its successor is already live, and tearing down on it would kill the fresh
/// pipe. Ignoring it costs nothing that matters: if the current channel really is
/// dead, its own task reports the end of the pipe independently, which tears down
/// with no reason attached. The reason-specific work (a grant that dies with its
/// device, an account that was left) always runs, because it is about the pair
/// rather than about one pipe.
pub const CLOSURE_GRACE: Duration = Duration::from_millis(250);

/// How long a peer that refused a session is left alone. Long enough that a
/// machine whose owner said no is not hammered, short enough that saying yes
/// takes effect while a hand is still on the mouse. Cleared early by an explicit
/// `input.take`: a human asking again is a reason to try again.
pub const BACKOFF_REFUSED: Duration = Duration::from_secs(30);
/// How long a peer whose channel would not open is left alone. Shorter, because
/// the cause is usually the network and the `devices` topic will say when it
/// changes.
pub const BACKOFF_OPEN: Duration = Duration::from_secs(5);
/// How long after a cap cut (`FRAME_TOO_LARGE`, `RATE_EXCEEDED`). This is a bug
/// or a hostile peer, not a hiccup.
pub const BACKOFF_CUT: Duration = Duration::from_secs(60);
/// How long after a `PLANE_STALE`: the shortest of the three, because a layout
/// round is already running and this refusal repairs itself.
pub const BACKOFF_STALE: Duration = Duration::from_secs(2);

/// Frames buffered towards one channel task. Two seconds of the fast ceiling: a
/// task that has not drained this much is a wedged write, and the channel is
/// already dying.
pub const OUTBOX: usize = 512;

/// The transient refusal code for a crossing whose channel did not finish
/// warming inside [`CROSS_OPEN_BOUND`]. Not one of the snapshot's `problem`
/// values (the pair is not refusing anything, it is not ready yet) and not a
/// dialect code: it travels only on `input.refused`, which is transient by
/// contract.
pub const NOT_WARM: &str = "NOT_WARM";

/// What the loop must do on the engine's behalf, because it involves the Core.
///
/// Everything else the engine does itself: the backend's downcalls are fire and
/// forget, and a frame on a warm channel is a `try_send` into that channel's
/// outbox, which is what keeps the emission order of a session identical to the
/// order the rules produced it in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// `peers.channel { device_id }`, then attach and run the channel task. Never
    /// on a timer: only when the directory changes, when a gesture asks, or on
    /// the slow layout sweep.
    Open { node_id: String, device_id: String },
    /// `peers.send { device_id, payload }`: one layout round.
    Send { device_id: String, payload: Value },
    /// `input.emit { method, params }`. Issued on ONE task, in order: two
    /// snapshots emitted from independent tasks could arrive newest first, and an
    /// interface that trusted the last one would show a stale state.
    Emit { method: String, params: Value },
}

/// One flow position, and the only way one is built.
///
/// [`Flow::relative`] is where "a relative move of (0, 0) is never emitted"
/// lives: Windows discards one and it reaches no hook at all (#123), so it can
/// only ever be noise. v1's source integrates deltas into a virtual cursor and
/// emits absolute positions ([`Flow::At`]); the relative frame is on the wire for
/// the per session relative mode that v1 does not offer, and this constructor is
/// what that mode will build its positions with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    /// An absolute position, in the target's own logical desktop coordinates.
    At(i32, i32),
    /// A relative move, in logical pixels, never both zero.
    By(i32, i32),
}

impl Flow {
    /// A relative move, or `None` for (0, 0), which is not a move.
    pub fn relative(dx: i32, dy: i32) -> Option<Flow> {
        (dx != 0 || dy != 0).then_some(Flow::By(dx, dy))
    }

    fn frame(self, session: u32, n: u32) -> Frame {
        match self {
            Flow::At(x, y) => Frame::Pointer { session, n, x, y },
            Flow::By(dx, dy) => Frame::Motion { session, n, dx, dy },
        }
    }
}

/// One row of `devices.list`, as this engine needs it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DirEntry {
    pub device_id: String,
    pub name: String,
    pub online: bool,
    pub lan: bool,
    pub reachable: bool,
}

/// The account's directory: the only translation between the node_id every
/// protocol object and every persisted key uses and the `device_id` the API
/// boundary speaks (doc/input-sharing.md, section 1).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Directory {
    /// This device, from the row carrying `is_self`.
    pub own_node: Option<String>,
    pub own_device: Option<String>,
    /// What a person calls this computer, so the plane can say "you are here".
    pub own_name: String,
    /// Every OTHER device of the account, keyed by node_id.
    pub peers: BTreeMap<String, DirEntry>,
}

impl Directory {
    /// Reads a `devices.list` snapshot. A row with no node_id or no device_id is
    /// skipped: it is not a device this engine can name.
    pub fn parse(snapshot: &Value) -> Directory {
        let mut dir = Directory::default();
        let Some(rows) = snapshot.as_array() else {
            return dir;
        };
        for row in rows {
            let (Some(node), Some(device)) = (
                row.get("node_id").and_then(Value::as_str),
                row.get("device_id").and_then(Value::as_str),
            ) else {
                continue;
            };
            if row.get("is_self").and_then(Value::as_bool) == Some(true) {
                dir.own_node = Some(node.to_string());
                dir.own_device = Some(device.to_string());
                dir.own_name = row
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                continue;
            }
            dir.peers.insert(
                node.to_string(),
                DirEntry {
                    device_id: device.to_string(),
                    name: row
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    online: row.get("online").and_then(Value::as_bool) == Some(true),
                    lan: row.get("lan").and_then(Value::as_bool) == Some(true),
                    reachable: row.get("reachable").and_then(Value::as_bool) == Some(true),
                },
            );
        }
        dir
    }

    /// The node_id behind a `device_id`, the lookup every gesture starts with.
    pub fn node_of(&self, device_id: &str) -> Option<String> {
        self.peers
            .iter()
            .find(|(_, e)| e.device_id == device_id)
            .map(|(node, _)| node.clone())
    }

    fn device_of(&self, node_id: &str) -> Option<String> {
        if self.own_node.as_deref() == Some(node_id) {
            return self.own_device.clone();
        }
        self.peers.get(node_id).map(|e| e.device_id.clone())
    }
}

/// The source half's channel state (doc/input-sharing.md, section 4).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Link {
    #[default]
    Cold,
    Warming,
    /// Attached and `hi` received: the caps and the plane id are known.
    Warm,
}

/// What a peer's handshake said.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Hi {
    /// The whole capability report, not a pair of booleans: what a session cannot
    /// do is then sayable from the snapshot before anyone tries, which is the
    /// whole reason `caps` is on the wire (section 3).
    caps: Capabilities,
    plane: String,
}

/// Everything this engine holds about one peer.
struct Peer {
    link: Link,
    /// Did WE open this channel? Only ours are dropped when a peer leaves the
    /// warm set: a channel the PEER opened is its decision to be driven by us,
    /// and dropping it because we do not drive it in return would make an
    /// asymmetric pair impossible (`input.drive` is a local convenience, section
    /// 9, and consenting to be driven says nothing about driving).
    mine: bool,
    /// The outbox of the live channel task. `None` is no channel: dropping it is
    /// how the task is asked to end, which closes the pipe.
    out: Option<mpsc::Sender<Vec<u8>>>,
    hi: Option<Hi>,
    /// Monotonic per channel, from 1.
    next_session: u32,
    rtt_ms: Option<u64>,
    last_ping: Option<Instant>,
    last_pong: Option<Instant>,
    /// When the live channel attached, for [`CLOSURE_GRACE`].
    attached_at: Option<Instant>,
    /// Nothing is tried towards this peer before this instant.
    backoff_until: Option<Instant>,
    /// What the interface says about this pair, from the snapshot alone. A code of
    /// the pair vocabulary rather than a string, so a spelling cannot be invented at
    /// a call site: [`PeerProblem`] is what the interface has sentences for, and a
    /// word outside it reaches a person as "this version does not know".
    ///
    /// This is the REMEMBERED half only: what the far side said when it refused, or
    /// what a channel that would not open said. The standing half that a peer's
    /// handshake implies is derived at snapshot time (`peer_problem`) rather than
    /// copied in here, and the reason that holds TODAY is the first of these two: a
    /// `hi` clears this field (a handshake is a fresh start for a pair), so a copy
    /// would erase a standing fact along with the stale refusal it came with. The
    /// second is free rather than load-bearing: the dialect accepts a repeated `hi`
    /// and a derived value would follow one, while nothing in this build sends a
    /// second one (`CapabilitiesChanged` refreshes our own caps and tells no peer),
    /// which is its own gap and its own ticket.
    problem: Option<PeerProblem>,
    /// When the last layout message went out, for the rate limit.
    layout_sent: Option<Instant>,
    /// A layout round was wanted while the rate limit was closed.
    layout_due: bool,
    /// An `input.take` is waiting for this channel to warm, and the mode it
    /// asked for.
    take: Option<(Option<Mode>, Instant)>,
}

impl Default for Peer {
    fn default() -> Peer {
        Peer {
            link: Link::Cold,
            mine: false,
            out: None,
            attached_at: None,
            hi: None,
            next_session: 1,
            rtt_ms: None,
            last_ping: None,
            last_pong: None,
            backoff_until: None,
            problem: None,
            layout_sent: None,
            layout_due: false,
            take: None,
        }
    }
}

/// The source half of a live session. Present from the `start` frame onward, so
/// `Starting` and `Driving` are one struct with a flag: the exclusion rules ask
/// "is a session going out at all", and the answer must be yes from the moment
/// the frame is written.
struct Driving {
    node: String,
    session: u32,
    mode: Mode,
    accepted: bool,
    since: Instant,
    since_unix: u64,
    /// The virtual cursor, in PLANE coordinates. The absolute position stops
    /// being meaningful the moment the pointer is confined, so the engine
    /// integrates the backend's own relative deltas instead.
    cursor: (i32, i32),
    /// The placed monitor the cursor is on.
    on: String,
    /// Our own monitor the crossing left from, and the point in its own desktop
    /// coordinates the pointer was at: the warp home.
    from: String,
    home: Point,
    /// The flow counter, for the life of the session.
    n: u32,
    /// At most one pending position exists; a new one replaces it.
    pending: Option<Flow>,
    tokens: f64,
    tokens_at: Instant,
    /// The whole emission's backstop bucket ([`wire::OUT_RATE_MAX`]).
    out_tokens: f64,
    out_at: Instant,
}

/// The target half of a live session.
struct Driven {
    node: String,
    session: u32,
    keys: KeyMode,
    mode: Mode,
    since_unix: u64,
    /// Any frame from the source, for the session idle.
    last_frame: Instant,
    /// The highest flow counter applied, so no reordering and no replay can walk
    /// the pointer backwards.
    last_n: u32,
}

/// A dwell in progress: the pointer resting against one crossing segment.
struct Dwell {
    seg: Segment,
    since: Instant,
    /// Where along the segment the pointer is, for the entry point.
    along: i32,
    /// The double tap is satisfied for this touch (or was not required).
    tapped: bool,
    /// The channel started warming for this dwell, and when.
    warming: Option<Instant>,
    /// The extra latency probe went out.
    probed: bool,
}

/// One refusal window: at most one word per code per device per second.
struct Window {
    until: Instant,
    pending: u32,
    /// Does this code ride the wire as an `oops` as well? A backend's refusal
    /// does (the source has to learn nothing was typed); a `no` refusal does not.
    oops: bool,
}

/// The engine: all the state, none of the plumbing.
pub struct Engine<B> {
    backend: B,
    store: Store,
    directory: Directory,
    plane: Plane,
    plane_id: String,
    settings: Settings,
    graph: Graph,
    caps: Capabilities,
    monitors: Vec<Monitor>,
    /// This machine's active layout identity, for the `l` field of a key frame.
    layout: String,
    /// This machine's active keyboard group, so a stroke that switches can switch
    /// back. Zero until a backend says otherwise.
    group: u32,
    /// The canonical modifiers the machine's own user is holding, from the last
    /// key upcall: what the required-modifier guard reads.
    mods_held: u16,
    last_pointer: Option<Point>,
    /// The capture mode this engine WANTS, which is what the resting-value rule computes.
    capture: Option<CaptureMode>,
    /// The capture mode the backend was actually TOLD, which is not the same thing: the call
    /// is gated on the capability, so the two diverge whenever the gate skips it.
    ///
    /// Two fields because "was it on" has exactly one honest answer and it is this one. Reading
    /// the wanted mode instead meant that a backend which had never been started could be told
    /// to stop, and that a start which the gate had skipped could never happen later, because
    /// the wanted mode already matched and the call deduplicated itself away. Neither is
    /// reachable today, and only because `can_drive()` happens to require `capture`; the seam
    /// states no such coupling, so this does not rely on it.
    capture_sent: Option<CaptureMode>,
    /// Whether a confinement of OURS is in place, so the release can happen even
    /// after the capability that allowed it has gone (see [`Engine::confine`]).
    confined: bool,
    peers: BTreeMap<String, Peer>,
    driving: Option<Driving>,
    driven: Option<Driven>,
    /// What WE pressed on this machine and have not released.
    held: Held,
    /// Keys that may still be DOWN and that this engine could not release, which is not the
    /// same thing as keys it is holding, and the difference is the whole reason this is its
    /// own field.
    ///
    /// It fills from two places: a `held.json` found at startup on a machine that cannot type,
    /// and a `release_held` whose release could not have reached the OS. `held` would have been
    /// the obvious place to keep them, and it is the wrong one: `held` is an INPUT to the
    /// injection planner, so a set kept there made the planner believe a modifier was already
    /// down. Proved end to end in review: an `A` came out as `a` because the Shift press was
    /// skipped, and the next keystroke pressed that phantom Shift FOR REAL, on a machine
    /// nobody had touched.
    ///
    /// Nothing reads this except the drain, and the drain is [`Engine::refresh_caps`], so
    /// every path that re-reads the capabilities is a chance to try again.
    stranded: Held,
    held_on_disk: Value,
    resolver: Resolver,
    mod_keys: ModKeys,
    dwell: Option<Dwell>,
    /// When each segment was last left, for the double tap.
    left: BTreeMap<String, Instant>,
    /// The `ping` stamps are milliseconds from here, so a round trip needs no
    /// clock synchronisation between the two machines.
    epoch: Instant,
    effects: Vec<Effect>,
    dirty: bool,
    last_emit: Instant,
    last_snapshot: Value,
    sweep_at: Instant,
    /// A trigger asked for the warm set to be opened: a directory change, a
    /// gesture, a dwell, a merge that changed the adjacency, or the slow sweep.
    open_due: bool,
    windows: BTreeMap<(String, String), Window>,
}

impl<B: InputBackend> Engine<B> {
    /// Loads the persisted state and drains the crash guard.
    ///
    /// A corrupt `settings.json` or `plane.json` is an ERROR, not a fresh start:
    /// the store's rule, for the store's reason. A `held.json` found here is
    /// released at once, which is the whole point of writing it: an injected key
    /// stays down after the injector exits.
    pub fn open(backend: B, store: Store, now: Instant) -> std::io::Result<Engine<B>> {
        // Through the store's own per-file readers, not the generic one: the
        // corruption policy differs per file and it differs on purpose (section
        // 11). A plane is rebuilt by one round with any peer, so a torn one is a
        // warning and an empty plane; the settings are permissions and a torn one
        // is fatal, because starting with a permission set nobody chose is the one
        // thing this engine must never do.
        let plane = match store.load_plane()? {
            Some(doc) => Plane::from_stored(&doc),
            None => Plane::default(),
        };
        let settings = match store.load(SETTINGS_FILE)? {
            Some(doc) => Settings::from_value(&doc),
            None => Settings::default(),
        };
        // The crash guard, drained before anything else happens: a machine whose
        // engine died with Control down has a dead keyboard until this runs. Its
        // reader is the most tolerant of the three, and it has to be: this is the
        // one file written without an fsync, so it is the one most likely to be
        // torn, and refusing to start over it would withhold the remedy exactly
        // when it is needed.
        //
        // The capabilities are read FIRST, and that ordering is the difference between a
        // remedy and the appearance of one. The record used to be deleted unconditionally,
        // including when this machine could not possibly have typed the release: a macOS whose
        // Accessibility grant was reset by the very update that restarted this component, or a
        // platform with no backend at all, whose `release_all` is a documented no-op. The
        // release was dropped, the record was wiped, and the key stayed down with nothing left
        // in the system that would ever lift it.
        let caps = backend.capabilities();
        let stored = store.load_held();
        let found = Held::from_value(&stored);
        let mut stranded = Held::new();
        let mut held_on_disk = stored;
        if !found.is_empty() {
            if caps.inject_keys {
                eprintln!(
                    "[1device-input] releasing {} key(s) a previous run left down",
                    found.len()
                );
                backend.release_all(found.release_plan());
                let empty = Held::new().to_value();
                match store.save_held(&empty) {
                    Ok(()) => held_on_disk = empty,
                    Err(e) => eprintln!("[1device-input] cannot write the held set: {e}"),
                }
            } else {
                // KEPT, on disk and in `stranded`, which is deliberately NOT `held`: see the
                // field's own documentation for what putting it there did. Every path that
                // re-reads the capabilities retries the release (`refresh_caps`).
                eprintln!(
                    "[1device-input] {} key(s) a previous run left down cannot be released \
                     yet: this computer cannot type at the moment. The record is kept and the \
                     release is retried as soon as it can",
                    found.len()
                );
                stranded = found;
            }
        }
        Ok(Engine {
            backend,
            store,
            directory: Directory::default(),
            plane_id: plane::plane_id(&plane),
            plane,
            settings,
            graph: Graph::default(),
            caps,
            monitors: Vec::new(),
            layout: String::new(),
            group: 0,
            mods_held: 0,
            last_pointer: None,
            capture: None,
            capture_sent: None,
            confined: false,
            peers: BTreeMap::new(),
            driving: None,
            driven: None,
            held: Held::new(),
            stranded,
            held_on_disk,
            resolver: Resolver::new(),
            mod_keys: ModKeys::new(),
            dwell: None,
            left: BTreeMap::new(),
            epoch: now,
            effects: Vec::new(),
            dirty: true,
            last_emit: now,
            last_snapshot: Value::Null,
            sweep_at: now,
            open_due: true,
            windows: BTreeMap::new(),
        })
    }

    /// Reads what only the OS can answer: the monitors, the pointer, and the
    /// platform key of each canonical modifier. Called once at start and again
    /// whenever the OS says one of them changed.
    pub async fn start(&mut self, now: Instant) {
        // Re-read, rather than trust what construction saw: a backend can learn
        // its own permission state after it is built (the macOS Accessibility
        // grant is the case), and every sentence the interface says about this
        // machine comes from here.
        self.refresh_caps();
        self.monitors = self.backend.monitors().await;
        self.last_pointer = self.backend.pointer().await;
        self.relearn().await;
        self.republish(now);
    }

    /// Everything the loop hands over, drained once per turn.
    pub fn take_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.effects)
    }

    /// This device's node_id, once the directory has named it.
    pub fn own_node(&self) -> Option<&str> {
        self.directory.own_node.as_deref()
    }

    /// The node_id behind a `device_id`.
    pub fn node_of(&self, device_id: &str) -> Option<String> {
        self.directory.node_of(device_id)
    }

    /// This device's plane id: what a `start` frame carries, and what makes an
    /// absolute coordinate mean one thing.
    pub fn plane_id(&self) -> &str {
        &self.plane_id
    }

    // ----------------------------------------------------------------- clock

    /// When the loop must wake up next, or `None` for "nothing is pending".
    ///
    /// The minimum of every deadline the rules own. The flow contributes exactly
    /// one of them (the trailing-edge flush), and only while a position is
    /// pending.
    ///
    /// # The invariant, and it is load bearing
    ///
    /// Every deadline this returns must be one [`Engine::pump`] can CLEAR: either
    /// it is in the future, or pumping at it changes the state that produced it. A
    /// deadline in the past that pumping cannot clear is a loop that wakes,
    /// changes nothing, and wakes again: a component at 100% of a core, which is
    /// why `now` is a parameter here at all (the dwell is the case that needs it).
    pub fn next_deadline(&self, now: Instant) -> Option<Instant> {
        let mut soonest: Option<Instant> = None;
        let mut at = |when: Instant| {
            soonest = Some(match soonest {
                Some(held) if held <= when => held,
                _ => when,
            });
        };
        if self.dirty {
            at(self.last_emit + EMIT_GAP);
        }
        at(self.sweep_at + LAYOUT_SWEEP);
        for (node, peer) in &self.peers {
            if peer.layout_due {
                at(peer
                    .layout_sent
                    .map_or(self.epoch, |sent| sent + LAYOUT_MIN_GAP));
            }
            if peer.link == Link::Warm {
                let gap = if self
                    .driving
                    .as_ref()
                    .is_some_and(|d| d.accepted && d.node == *node)
                {
                    SESSION_PING
                } else {
                    WARM_PING
                };
                at(peer.last_ping.map_or(self.epoch, |ping| ping + gap));
            }
        }
        if let Some(driving) = &self.driving {
            if driving.pending.is_some() {
                let rate = self.flow_rate(&driving.node);
                let wait = ((1.0 - driving.tokens).max(0.0) / rate).max(0.0);
                at(driving.tokens_at + Duration::from_secs_f64(wait));
            }
            if driving.accepted {
                let last = self
                    .peers
                    .get(&driving.node)
                    .and_then(|p| p.last_pong)
                    .unwrap_or(driving.since);
                at(last + SOURCE_STALL);
            }
        }
        if let Some(driven) = &self.driven {
            at(driven.last_frame + SESSION_IDLE);
        }
        if let Some(dwell) = &self.dwell {
            let guards = self.guards_of(&dwell.seg);
            let ready = dwell.since + Duration::from_millis(u64::from(guards.dwell_ms));
            // Armed only while the dwell could still FIRE. A dwell that cannot
            // (its double tap was never satisfied, a session is live, or it is
            // already waiting on a channel) has a deadline in the past that
            // pumping cannot clear, and arming it would spin.
            if now < ready && dwell.tapped && self.driving.is_none() && self.driven.is_none() {
                at(ready);
            }
            if let Some(started) = dwell.warming
                && now < started + CROSS_OPEN_BOUND
            {
                at(started + CROSS_OPEN_BOUND);
            }
        }
        for window in self.windows.values() {
            if window.pending > 0 {
                at(window.until);
            }
        }
        soonest
    }

    /// Does everything that is due. Called at the end of every loop turn and
    /// whenever the deadline fires, so "due" is the only scheduling concept the
    /// engine has.
    pub fn pump(&mut self, now: Instant) {
        self.sync_channels(now);
        self.parked_takes(now);
        self.keepalives(now);
        self.flow_flush(now);
        self.watchdogs(now);
        self.crossing(now);
        self.layout_rounds(now);
        self.refusal_windows(now);
        self.emit_snapshot(now);
    }

    /// Milliseconds from this engine's own epoch: what a `ping` carries and a
    /// `pong` echoes back untouched.
    fn stamp(&self, now: Instant) -> u64 {
        now.saturating_duration_since(self.epoch).as_millis() as u64
    }

    // ------------------------------------------------------------- directory

    /// A fresh `devices.list`. Returns whether anything the interface can see
    /// changed.
    pub fn set_directory(&mut self, directory: Directory, now: Instant) {
        if self.directory == directory {
            return;
        }
        let first = self.directory.own_node.is_none() && directory.own_node.is_some();
        // A device that left the account takes its peer state with it: the
        // channel goes, and so does anything a session was holding.
        let gone: Vec<String> = self
            .peers
            .keys()
            .filter(|node| !directory.peers.contains_key(*node))
            .cloned()
            .collect();
        for node in gone {
            self.tear_down(&node, None, now);
            self.peers.remove(&node);
        }
        let became_reachable: Vec<String> = directory
            .peers
            .iter()
            .filter(|(node, entry)| {
                entry.reachable
                    && !self
                        .directory
                        .peers
                        .get(*node)
                        .is_some_and(|was| was.reachable)
            })
            .map(|(node, _)| node.clone())
            .collect();
        self.directory = directory;
        // The plane learns our own node_id the moment the Core names it, and
        // BEFORE any peer document can be merged (nothing is dialled until the
        // directory resolves). That is what lets the plane drop an entry filed
        // under our own name by anybody else: we are the only author of our own
        // word about our own screens, and a sibling that claimed otherwise would
        // otherwise be able to erase our screens from its own plane and refuse
        // every session with us.
        if let Some(own) = self.directory.own_node.clone()
            && self.plane.set_own(&own)
        {
            self.persist_plane();
        }
        if first {
            self.republish(now);
        }
        for node in became_reachable {
            self.want_layout(&node, now);
        }
        self.rebuild(now);
        self.open_due = true;
        self.dirty = true;
    }

    /// Publishes our own monitors and rebuilds everything that depends on them.
    fn republish(&mut self, now: Instant) {
        let Some(own) = self.directory.own_node.clone() else {
            return;
        };
        let monitors = self.monitors.clone();
        if self
            .plane
            .publish_monitors(&own, self.store.identity(), monitors)
        {
            self.persist_plane();
            self.rebuild(now);
            self.layout_everyone(now);
            self.open_due = true;
            self.dirty = true;
        } else {
            self.rebuild(now);
        }
    }

    /// Rebuilds the crossing graph and the plane id from the records.
    fn rebuild(&mut self, _now: Instant) {
        let Some(own) = self.directory.own_node.clone() else {
            return;
        };
        // Present: us, plus every reachable peer. A monitor whose device is
        // absent keeps its spot and becomes a ghost, whose edges are walls.
        let mut present = vec![own.clone()];
        for (node, entry) in &self.directory.peers {
            if entry.reachable {
                present.push(node.clone());
            }
        }
        self.graph = graph::build(&self.plane, &own, &present);
        self.plane_id = plane::plane_id(&self.plane);
    }

    // -------------------------------------------------------------- channels

    /// Opens what belongs in the warm set and drops what does not.
    ///
    /// Never a retry timer: this runs when the directory changes, when a gesture
    /// asks and on the slow sweep, which is exactly what the letter asks for.
    fn sync_channels(&mut self, now: Instant) {
        if self.directory.own_node.is_none() {
            return;
        }
        // The OPEN half is gated on a trigger, and that is the letter's rule
        // rather than an optimisation: an ungated re-evaluation would become a
        // retry timer by accident, dialling a peer that just refused every time
        // any other deadline happened to fire.
        let opening = std::mem::take(&mut self.open_due);
        let nodes: Vec<String> = self.directory.peers.keys().cloned().collect();
        for node in nodes {
            let wanted = self.wants_warm(&node, now);
            let peer = self.peers.entry(node.clone()).or_default();
            if opening && wanted && peer.link == Link::Cold {
                peer.link = Link::Warming;
                let device_id = self
                    .directory
                    .peers
                    .get(&node)
                    .map(|e| e.device_id.clone())
                    .unwrap_or_default();
                self.effects.push(Effect::Open {
                    node_id: node.clone(),
                    device_id,
                });
                self.dirty = true;
                continue;
            }
            if !wanted && peer.mine && peer.out.is_some() {
                let busy = self.driving.as_ref().is_some_and(|d| d.node == node)
                    || self.driven.as_ref().is_some_and(|d| d.node == node);
                if !busy {
                    // Dropping the outbox is how the task is asked to end: its
                    // `recv` returns `None`, the pipe closes, and the Core ends
                    // the channel with `CLOSED`.
                    let peer = self.peers.get_mut(&node).expect("just looked it up");
                    peer.out = None;
                    peer.link = Link::Cold;
                    peer.hi = None;
                    self.dirty = true;
                }
            }
        }
    }

    /// All four at once (doc/input-sharing.md, section 4): in the directory,
    /// reachable, enabled outbound here, and adjacent in the plane or named by an
    /// explicit `input.take`.
    fn wants_warm(&self, node: &str, now: Instant) -> bool {
        // A machine that cannot drive anyone warms nothing: the channel exists to
        // carry our keyboard there, and a backend that cannot capture, swallow
        // and confine has no keyboard to send. The engine still answers the whole
        // facade, still replicates the plane and still accepts being driven, all
        // of which need no channel of ours.
        if !self.caps.can_drive() {
            return false;
        }
        let Some(entry) = self.directory.peers.get(node) else {
            return false;
        };
        if !entry.reachable || !self.settings.drives(node) {
            return false;
        }
        if let Some(peer) = self.peers.get(node)
            && peer.out.is_none()
            && peer.backoff_until.is_some_and(|until| now < until)
        {
            return false;
        }
        let asked = self.peers.get(node).is_some_and(|p| p.take.is_some());
        asked || self.graph.segments.iter().any(|s| s.node_id == node)
    }

    /// A channel task attached: the handshake goes out at once, without waiting
    /// for the peer's.
    pub fn attach(&mut self, node: &str, out: mpsc::Sender<Vec<u8>>, mine: bool, now: Instant) {
        // Through the capabilities' own codec, so the two ends cannot spell the
        // same eight answers differently, and so the `problem` travels with them:
        // an interface can then say what a session cannot do BEFORE anyone tries.
        let caps = self.caps.to_value();
        let hi = Frame::Hi {
            version: wire::VERSION,
            caps,
            plane: self.plane_id.clone(),
        };
        {
            let peer = self.peers.entry(node.to_string()).or_default();
            // One channel per peer: a second attach means the Core replaced ours,
            // so the old task is dropped here rather than left holding a pipe.
            peer.out = Some(out);
            peer.mine = mine;
            peer.attached_at = Some(now);
            peer.link = Link::Warming;
            peer.hi = None;
            peer.last_ping = None;
            peer.last_pong = None;
            peer.next_session = 1;
        }
        self.send(node, &hi, now);
        self.dirty = true;
    }

    /// A channel could not be opened. Not retried on a timer: the next `devices`
    /// change, the next gesture or the slow sweep will ask again.
    pub fn on_open_failed(&mut self, node: &str, code: &str, now: Instant) {
        {
            let peer = self.peers.entry(node.to_string()).or_default();
            peer.link = Link::Cold;
            peer.out = None;
            // The handshake goes with the channel, which every OTHER path to Cold
            // already knew (`tear_down`, and the pump dropping an outbox). This one
            // kept it, and a review found what that costs now that the snapshot reads
            // it: a peer with a stale `hi` and no channel rendered "Not connected",
            // "is not answering right now" and a standing sentence about a session
            // that cannot start, all three at once.
            peer.hi = None;
            peer.backoff_until = Some(now + BACKOFF_OPEN);
            peer.problem = match code {
                "NO_DIRECT_PATH" => Some(PeerProblem::NoPath),
                "COMPONENT_ABSENT" => Some(PeerProblem::NoBackend),
                _ => None,
            };
        }
        self.announce(node, code, false, now);
        self.dirty = true;
    }

    /// The pipe ended. Either this or `peer.channel_closed` can come first, so
    /// both tear down at once and the reason is recorded by whichever brings it.
    pub fn on_channel_ended(&mut self, node: &str, now: Instant) {
        self.tear_down(node, None, now);
    }

    /// `peer.channel_closed { device_id, reason }`, the whole teardown matrix
    /// (doc/input-sharing.md, section 4).
    pub fn on_channel_closed(&mut self, node: &str, reason: &str, now: Instant) {
        let fresh = self.peers.get(node).is_some_and(|peer| {
            peer.out.is_some()
                && peer
                    .attached_at
                    .is_some_and(|at| now.saturating_duration_since(at) < CLOSURE_GRACE)
        });
        if !fresh {
            self.tear_down(node, Some(reason), now);
        }
        match reason {
            // A grant dies the instant the device is revoked, and the plane
            // forgets its screens: enforced on both sides of the pair.
            "DEVICE_REVOKED" => {
                let changed = self.settings.forget(node);
                self.plane.monitors.remove(node);
                self.plane.pins.remove(node);
                if changed {
                    self.persist_settings();
                }
                self.persist_plane();
                self.rebuild(now);
                self.peers.remove(node);
            }
            // Fail closed: re-joining must not silently restore a door opened
            // under a membership that has ended.
            "ACCOUNT_LEFT" => {
                self.end_everything(now);
                if self.settings.forget_all_grants() {
                    self.persist_settings();
                }
            }
            // Grants SURVIVE: they are keyed by node_id, not by the re-keyed
            // device_id.
            "LOGGED_OUT" | "SHUTDOWN" => self.end_everything(now),
            "FRAME_TOO_LARGE" | "RATE_EXCEEDED" => {
                eprintln!(
                    "[1device-input] the channel to a device was cut with {reason}: \
                     backing off before warming it again"
                );
                let peer = self.peers.entry(node.to_string()).or_default();
                peer.backoff_until = Some(now + BACKOFF_CUT);
            }
            "NO_DIRECT_PATH" => {
                let peer = self.peers.entry(node.to_string()).or_default();
                peer.backoff_until = Some(now + BACKOFF_OPEN);
                peer.problem = Some(PeerProblem::NoPath);
            }
            // The two the matrix says to re-warm from: a newer channel took the
            // pair, or our own keepalive did not keep this one alive.
            "REPLACED" | "IDLE_TIMEOUT" => self.open_due = true,
            _ => {}
        }
        self.dirty = true;
    }

    /// One channel is over: both halves end, hygiene first and the wording after,
    /// so a reason that arrives late cannot delay a release.
    fn tear_down(&mut self, node: &str, reason: Option<&str>, now: Instant) {
        // The outbox goes FIRST, so neither half tries to write a frame down a
        // pipe that is gone.
        if let Some(peer) = self.peers.get_mut(node) {
            peer.out = None;
            peer.attached_at = None;
            peer.link = Link::Cold;
            peer.hi = None;
            peer.rtt_ms = None;
            peer.last_ping = None;
            peer.last_pong = None;
            peer.take = None;
        }
        if self.driving.as_ref().is_some_and(|d| d.node == node) {
            self.bring_home(stopped::GONE, now);
        }
        if self.driven.as_ref().is_some_and(|d| d.node == node) {
            self.end_driven(None, now);
        }
        if let Some(dwell) = &self.dwell
            && dwell.seg.node_id == node
        {
            self.forget_dwell(now);
        }
        if let Some(reason) = reason
            && let Some(peer) = self.peers.get_mut(node)
        {
            peer.problem = match reason {
                "PEER_GONE" | "IDLE_TIMEOUT" => None,
                "NO_DIRECT_PATH" => Some(PeerProblem::NoPath),
                _ => peer.problem,
            };
        }
        self.apply_capture();
        self.dirty = true;
    }

    /// Every session and every channel ends: the deaths that are about this
    /// device rather than about one peer.
    fn end_everything(&mut self, now: Instant) {
        let nodes: Vec<String> = self.peers.keys().cloned().collect();
        for node in nodes {
            self.tear_down(&node, None, now);
        }
    }

    // ----------------------------------------------------------------- frames

    /// One batch of inbound frames from one peer, drained from the queue without
    /// blocking and applied with the read-side coalescing rule (D6).
    ///
    /// > Drop a pointer frame when the very next frame in the same batch is also
    /// > a pointer frame.
    ///
    /// Everything else is applied in arrival order, so a key or a button is
    /// always applied at the position it was sent from. Injection is about 100
    /// microseconds per event and does not amortise (#123), so replaying a stale
    /// burst costs real time and shows as a rubber band.
    pub async fn on_frames(&mut self, node: &str, batch: Vec<Vec<u8>>, now: Instant) {
        // A sender the directory cannot name is dropped (section 1).
        if !self.directory.peers.contains_key(node) {
            return;
        }
        // Nothing is legal before a `hi`, and a foreign dialect closes the channel
        // too. The two are ONE check here, because they are one thing from this
        // side: a frame this build cannot read as our own handshake. It is checked
        // on the batch's first frame rather than inside the loop below, so a bad
        // frame LATER in a batch is still merely dropped, which is what keeps a
        // misbehaving peer from being able to end a session with one torn frame.
        if self.peers.get(node).is_none_or(|p| p.hi.is_none())
            && !batch
                .first()
                .and_then(|bytes| wire::decode(bytes))
                .is_some_and(|frame| matches!(frame, Frame::Hi { .. }))
        {
            eprintln!(
                "[1device-input] a peer spoke before its handshake, or in another \
                 dialect: closing the channel"
            );
            self.tear_down(node, None, now);
            return;
        }
        let frames: Vec<Frame> = batch
            .iter()
            .filter_map(|bytes| wire::decode(bytes))
            .collect();
        for (i, frame) in frames.iter().enumerate() {
            if self.peers.get(node).is_none_or(|p| p.hi.is_none())
                && let Frame::Hi { caps, plane, .. } = frame
            {
                self.on_hi(node, caps, plane, now);
                continue;
            }
            if let Frame::Hi { caps, plane, .. } = frame {
                self.on_hi(node, caps, plane, now);
                continue;
            }
            // The one drop, and the only one: a position whose immediate
            // successor in this batch is also a position.
            if frame.is_position() && frames.get(i + 1).is_some_and(Frame::is_position) {
                continue;
            }
            self.on_frame(node, frame, now).await;
        }
    }

    fn on_hi(&mut self, node: &str, caps: &Value, plane: &str, now: Instant) {
        let peer = self.peers.entry(node.to_string()).or_default();
        peer.hi = Some(Hi {
            // Fails closed on every field, so a peer that says nothing is a peer
            // that can do nothing.
            caps: Capabilities::from_value(caps),
            plane: plane.to_string(),
        });
        peer.link = Link::Warm;
        peer.problem = None;
        peer.last_ping = None;
        self.want_layout(node, now);
        self.apply_capture();
        self.dirty = true;
    }

    async fn on_frame(&mut self, node: &str, frame: &Frame, now: Instant) {
        if self.driven.as_ref().is_some_and(|d| d.node == node)
            && let Some(driven) = &mut self.driven
        {
            driven.last_frame = now;
        }
        match frame {
            Frame::Hi { .. } => {}
            Frame::Ping { ms } => {
                let pong = Frame::Pong { ms: *ms };
                self.send(node, &pong, now);
            }
            Frame::Pong { ms } => {
                let rtt = self.stamp(now).saturating_sub(*ms);
                if let Some(peer) = self.peers.get_mut(node) {
                    peer.last_pong = Some(now);
                    // An echo from the future, or one older than any plausible
                    // path, says nothing: the stamp is our own clock, so either
                    // is a peer not echoing it untouched.
                    if rtt <= 60_000 {
                        peer.rtt_ms = Some(rtt);
                    }
                }
                self.dirty = true;
            }
            Frame::Start {
                session,
                mode,
                keys,
                plane,
                n,
                x,
                y,
            } => {
                self.on_start(node, *session, *mode, *keys, plane, *n, *x, *y, now)
                    .await;
            }
            Frame::Accepted { session } => self.on_accepted(node, *session, now),
            Frame::Refused { session, code, .. } => self.on_refused(node, *session, code, now),
            // The NODE as well as the session number, in every arm below. A
            // session number is per peer and starts at 1 on every attach, so "the
            // session I am in" and "the session this frame names" agree by
            // accident all the time: without the node, one stale `Ended` from a
            // peer whose `Stop` was dropped ends the session this machine is
            // holding with a DIFFERENT computer, and names the wrong one in the
            // sentence it emits on the way out. `on_refused` above always checked
            // both; these did not.
            Frame::Stop { session, .. } | Frame::Ended { session, .. } => {
                if self
                    .driven
                    .as_ref()
                    .is_some_and(|d| d.node == node && d.session == *session)
                {
                    // The source ended it: release everything, say nothing back.
                    self.end_driven(None, now);
                } else if self
                    .driving
                    .as_ref()
                    .is_some_and(|d| d.node == node && d.session == *session)
                {
                    // The target ended it unilaterally.
                    if let Frame::Ended { code, .. } = frame {
                        self.remember_refusal(node, code, now);
                        // And said out loud, the way a `no` frame is: the
                        // standing half of it is that device's `problem`, but
                        // `IDLE` deliberately sets none (it is nobody's fault),
                        // so without this the one case where a target lets go of
                        // a hung source's keyboard would be the one case with no
                        // sentence at all.
                        self.emit_refused(node, code, 1);
                    }
                    self.bring_home_quietly(now);
                }
            }
            Frame::ReleaseAll { session } => {
                if self
                    .driven
                    .as_ref()
                    .is_some_and(|d| d.node == node && d.session == *session)
                {
                    self.release_held();
                }
            }
            Frame::Oops {
                session,
                code,
                count,
            } => {
                if self
                    .driving
                    .as_ref()
                    .is_some_and(|d| d.node == node && d.session == *session)
                {
                    self.emit_refused(node, code, *count);
                }
            }
            Frame::Pointer { .. }
            | Frame::Motion { .. }
            | Frame::Button { .. }
            | Frame::Wheel { .. }
            | Frame::Key { .. } => self.apply_flow(node, frame, now).await,
        }
    }

    // ------------------------------------------------------------ the target

    /// A source asks to drive. The five checks, in the letter's order, each with
    /// its own refusal code.
    #[allow(clippy::too_many_arguments)]
    async fn on_start(
        &mut self,
        node: &str,
        session: u32,
        mode: Mode,
        keys: KeyMode,
        plane: &str,
        n: u32,
        x: i32,
        y: i32,
        now: Instant,
    ) {
        // 1. The grant. The driving side learns by trying; nothing earlier ever
        //    hinted at it, because a grant can be withdrawn between a hint and
        //    its use.
        let mut refusal: Option<(&str, Option<String>)> = None;
        if !self.settings.allows(node) {
            refusal = Some((refused::NOT_ALLOWED, None));
        } else if let Some(holder) = self.busy_with(node) {
            // 2. No preemption, ever (D16). The holder is named so the interface
            //    can say who has it.
            refusal = Some((refused::BUSY, holder));
        } else if plane != self.plane_id {
            // 3. The one refusal that repairs itself.
            refusal = Some((refused::PLANE_STALE, None));
        } else if !self.caps.can_be_driven() {
            refusal = Some((refused::NO_BACKEND, None));
        } else if self.settings.locked {
            refusal = Some((refused::LOCKED, None));
        }
        if let Some((code, by)) = refusal {
            let frame = Frame::Refused {
                session,
                code: code.to_string(),
                by,
            };
            self.send(node, &frame, now);
            if code == refused::PLANE_STALE {
                // Both ends run a round; ours starts here.
                self.want_layout(node, now);
            }
            // Deliberately NOT announced here. `input.refused` names a device and
            // nothing else, so a sentence built from one names the machine that
            // ASKED while describing the state of the machine that refused: on
            // this side that is exactly the wrong way round, in all five codes
            // ("that computer has not been told to accept your keyboard" about
            // the computer that just asked to use ours). The side that needs the
            // sentence is the one whose person pressed something, and it gets it
            // from the `no` frame above (`on_refused`). What this machine shows
            // is its own standing state, which the snapshot already carries: the
            // switch that is off, the pin, the session it is already in.
            return;
        }
        // The same source starting again: its previous session is over as far as
        // it is concerned, and the keys it left down are ours to release BEFORE
        // the new session's held set begins.
        if self.driven.is_some() {
            self.end_driven(None, now);
        }
        let frame = Frame::Accepted { session };
        self.send(node, &frame, now);
        // Before the first keystroke can arrive: what this machine can produce is
        // asked again here (see [`Engine::relearn`]).
        self.relearn().await;
        self.driven = Some(Driven {
            node: node.to_string(),
            session,
            keys,
            mode,
            since_unix: unix_millis(),
            last_frame: now,
            last_n: n.saturating_sub(1),
        });
        // A machine being driven does not capture (D15), which is what makes echo
        // suppression a non-problem in v1.
        self.apply_capture();
        if mode == Mode::Full && self.caps.inject_pointer {
            self.backend.inject(vec![Action::MoveTo(Point { x, y })]);
        }
        self.dirty = true;
    }

    /// Who holds this machine, if a `start` cannot be accepted. `None` means
    /// nobody does.
    fn busy_with(&self, from: &str) -> Option<Option<String>> {
        if self.driving.is_some() {
            // A machine that is driving refuses to be driven (D15), and the
            // holder of this keyboard is THIS machine: naming the computer it is
            // typing on would name the wrong end of the sentence.
            return Some(self.directory.own_device.clone());
        }
        if let Some(driven) = &self.driven {
            if driven.node == from {
                // The same source starting again: its own previous session is
                // over as far as it is concerned, so this is not BUSY.
                return None;
            }
            return Some(self.directory.device_of(&driven.node));
        }
        None
    }

    /// Applies one flow frame to this machine.
    async fn apply_flow(&mut self, node: &str, frame: &Frame, now: Instant) {
        let Some(driven) = &self.driven else {
            return;
        };
        if driven.node != node || Some(driven.session) != frame.session() {
            return;
        }
        // A keyboard-only session carries no pointer, and the mode is not
        // switchable mid session (a v1 limit, section 15): a peer that sends one
        // anyway is not obeyed.
        if driven.mode == Mode::Keys
            && !matches!(frame, Frame::Key { .. } | Frame::ReleaseAll { .. })
        {
            return;
        }
        // The `n` check: a position is applied only if its counter exceeds the
        // last applied, so no reordering and no replay walks the pointer
        // backwards.
        if let Some(n) = frame.flow() {
            if frame.is_position() && n <= driven.last_n {
                return;
            }
            if let Some(driven) = &mut self.driven {
                driven.last_n = driven.last_n.max(n);
            }
        }
        match frame {
            Frame::Pointer { x, y, .. } => {
                if self.caps.inject_pointer {
                    self.backend
                        .inject(vec![Action::MoveTo(Point { x: *x, y: *y })]);
                }
            }
            Frame::Motion { dx, dy, .. } => {
                if self.caps.inject_pointer {
                    self.backend
                        .inject(vec![Action::MoveBy { dx: *dx, dy: *dy }]);
                }
            }
            Frame::Button { button, down, .. } => {
                if self.caps.inject_pointer {
                    self.backend.inject(vec![Action::Button {
                        button: *button,
                        down: *down,
                    }]);
                }
            }
            Frame::Wheel { dx, dy, pixels, .. } => {
                if self.caps.inject_pointer {
                    self.backend.inject(vec![Action::Wheel {
                        dx: *dx,
                        dy: *dy,
                        pixels: *pixels,
                    }]);
                }
            }
            Frame::Key {
                usage,
                key,
                sym,
                mods: bits,
                down,
                lock,
                ..
            } => {
                self.apply_key(
                    node,
                    *usage,
                    key.as_deref(),
                    sym.as_deref(),
                    *bits,
                    *down,
                    *lock,
                    now,
                )
                .await;
            }
            _ => {}
        }
    }

    /// One keystroke: remap, resolve, sequence, and write the crash guard BEFORE
    /// the batch (doc/input-sharing.md, section 8).
    #[allow(clippy::too_many_arguments)]
    async fn apply_key(
        &mut self,
        node: &str,
        usage: u32,
        key: Option<&str>,
        sym: Option<&str>,
        bits: u16,
        down: bool,
        lock: bool,
        now: Instant,
    ) {
        if !self.caps.inject_keys {
            return;
        }
        let mode = self.driven.as_ref().map_or(KeyMode::Typing, |d| d.keys);
        // The remapping is applied on arrival, before resolution, so everything
        // downstream sees one modifier vocabulary (D10). Two halves, and the
        // second is the one that does the work: the modifier dance never reads a
        // frame's `m`, because modifiers arrive as their own key frames (a Control
        // press is usage 0xE0). So a remapped modifier KEY has to resolve to the
        // other bit's key, and remapping `m` alone would change nothing at all.
        let remap = self.settings.peer(node).remap;
        let (mut usage, mut key) = (usage, key.map(str::to_string));
        if let Some(bit) = keys::mod_of_usage(usage) {
            let to = keys::remap(bit, &remap);
            if to != bit
                && let Some(swapped) = keys::mod_usage(to)
            {
                usage = swapped;
                // The name travelled for the position the source pressed and
                // would resolve straight back to it: dropped, so only the
                // remapped usage speaks.
                key = None;
            }
        }
        // The frame's own `m`, in this machine's vocabulary. Nothing downstream
        // reads it today (the held set is authoritative and derives the modifier
        // state from the presses this engine made), and it is computed here so
        // that the day something does, it reads the remapped one.
        let _canonical_mods = keys::remap(bits, &remap);
        let wants = keys::levels(mode, usage, key.as_deref(), sym);
        let mut answer = None;
        for want in &wants {
            if let Some(how) = self.resolver.resolve(&self.backend, want.clone()).await {
                answer = Some((want.clone(), how));
                break;
            }
        }
        let answer_ref = answer
            .as_ref()
            .map(|(level, how)| keys::Answer { level, how });
        let stroke = keys::Stroke {
            mode,
            answer: answer_ref,
            down,
            lock,
            sym,
            unicode: self.caps.unicode,
            group: self.group,
        };
        let Some(plan) = keys::plan(&stroke, &self.held, &self.mod_keys) else {
            // Nothing on this machine can produce it.
            self.announce(node, wire::oops::UNRESOLVED, true, now);
            return;
        };
        if plan.actions.is_empty() {
            self.held = plan.held;
            return;
        }
        // The PEAK, not the end state, and BEFORE the batch: the `@` stroke holds
        // AltGr in the middle and none of it at the end, so a process death
        // between the two would strand AltGr on a machine whose file never
        // mentioned it.
        //
        // Plus whatever is STRANDED, in both writes. The file is the union of the two sets
        // for the reason `write_stranded` gives, and a peak written without the stranded half
        // would quietly drop keys that are still down from the record the next run reads.
        let mut peak = plan.peak.clone();
        peak.absorb(&self.stranded);
        self.write_held(&peak);
        self.backend.inject(plan.actions);
        self.held = plan.held;
        self.write_stranded();
    }

    /// Forgets every cached resolution and asks the backend for the modifier keys
    /// again.
    ///
    /// Run at start, and again when a session starts. The second one is what makes
    /// the engine survive a backend that could not answer EARLIER: the resolver
    /// caches negative answers on purpose (a symbol this layout cannot produce
    /// must cost one round trip rather than one per keystroke), so a backend asked
    /// before its permission was granted, or before the OS had a keymap, would
    /// otherwise be remembered as unable to produce anything for the life of the
    /// process. A session is a human action, so paying one round trip per
    /// character again is free, and the alternative is a machine that silently
    /// cannot type until it is restarted.
    async fn relearn(&mut self) {
        self.resolver = Resolver::new();
        self.resolver.layout_changed(&self.layout);
        self.mod_keys.learn(&self.backend, &mut self.resolver).await;
    }

    /// Re-reads what this machine can do, and retries anything the last answer made
    /// impossible.
    ///
    /// EVERY assignment to `self.caps` goes through here, and that is the point rather than
    /// tidiness. The crash guard's retry hangs off the moment `inject_keys` becomes true, and
    /// when only one of the four capability re-reads looked for that moment, whichever of the
    /// other three ran first consumed it: the record then sat in memory for the life of the
    /// process with nothing left that would ever drain it. Found in review, driven end to end.
    fn refresh_caps(&mut self) {
        self.caps = self.backend.capabilities();
        self.drain_stranded();
    }

    /// Tries again to release the keys a previous release could not have delivered.
    ///
    /// Fire and forget, like every release: what the engine can know is whether the release
    /// COULD have landed, and `inject_keys` is that. The record leaves memory and the file
    /// only when it could.
    fn drain_stranded(&mut self) {
        if self.stranded.is_empty() || !self.caps.inject_keys {
            return;
        }
        eprintln!(
            "[1device-input] releasing {} key(s) that could not be released earlier",
            self.stranded.len()
        );
        self.backend.release_all(self.stranded.release_plan());
        self.stranded.clear();
        self.write_stranded();
    }

    /// Writes the crash guard's file from the two sets that belong in it.
    ///
    /// `held` is what this engine is holding and `stranded` is what it could not release, and
    /// the file has to name both: it exists so that the NEXT run releases what this one left
    /// down, and that is the union.
    fn write_stranded(&mut self) {
        let mut record = self.held.clone();
        record.absorb(&self.stranded);
        self.write_held(&record);
    }

    /// Releases every key WE pressed, in reverse press order, and clears the
    /// file. The only way a held set empties.
    ///
    /// The record is KEPT when this machine cannot type, for the same reason
    /// [`Engine::open`] keeps it: `release_all` is fire and forget, so the only thing
    /// the engine can know about whether it landed is whether it could have. A grant
    /// withdrawn mid session (the macOS case, and the one that made this a bug) means
    /// the release posts nothing while the engine forgets what it was holding, and then
    /// nothing anywhere in the system knows that a key is down.
    fn release_held(&mut self) {
        if self.held.is_empty() {
            return;
        }
        self.backend.release_all(self.held.release_plan());
        if !self.caps.inject_keys {
            // The release cannot have landed, so what it named MOVES to the stranded record
            // rather than being forgotten. Out of `held` either way: leaving it there would
            // have the injection planner treat a key nobody is holding as held (see
            // `Engine::stranded`), and the next session would inherit the phantom.
            let missed = std::mem::replace(&mut self.held, Held::new());
            self.stranded.absorb(&missed);
            self.write_stranded();
            return;
        }
        self.held.clear();
        self.write_stranded();
    }

    /// Ends the session this machine is being driven in. The ONE function, and
    /// every one of the ten channel deaths funnels into it.
    fn end_driven(&mut self, code: Option<&str>, now: Instant) {
        let Some(driven) = self.driven.take() else {
            return;
        };
        self.release_held();
        if let Some(code) = code {
            let frame = Frame::Ended {
                session: driven.session,
                code: code.to_string(),
            };
            self.send(&driven.node, &frame, now);
        }
        self.apply_capture();
        self.dirty = true;
    }

    fn write_held(&mut self, held: &Held) {
        let value = held.to_value();
        if value != self.held_on_disk {
            self.write_value(value);
        }
    }

    fn write_value(&mut self, value: Value) {
        if let Err(e) = self.store.save_held(&value) {
            eprintln!("[1device-input] cannot write the held set: {e}");
            return;
        }
        self.held_on_disk = value;
    }

    // ------------------------------------------------------------ the source

    /// The peer accepted: only now is the pointer confined and the keyboard
    /// swallowed, which is why the `start` goes first.
    fn on_accepted(&mut self, node: &str, session: u32, now: Instant) {
        let Some(driving) = &mut self.driving else {
            return;
        };
        if driving.node != node || driving.session != session || driving.accepted {
            return;
        }
        driving.accepted = true;
        driving.tokens = 1.0;
        driving.tokens_at = now;
        let (from, mode) = (driving.from.clone(), driving.mode);
        if let Some(peer) = self.peers.get_mut(node) {
            peer.problem = None;
            peer.last_pong = Some(now);
            peer.take = None;
        }
        // Only a Full session confines: a keyboard-only session leaves the
        // pointer where it is, on this machine's own desktop, because pinning it
        // for a session that carries no pointer would freeze a mouse for nothing.
        if mode == Mode::Full
            && let Some(placed) = self.graph.placed.get(&from)
        {
            let rect = Rect {
                x: placed.own.x,
                y: placed.own.y,
                w: placed.own.w,
                h: placed.own.h,
            };
            self.confine(Some(rect));
        }
        self.apply_capture();
        self.dirty = true;
    }

    /// The peer refused: back off, record the reason as its `problem`, and
    /// restore. Nothing was confined or swallowed yet, which is why the order is
    /// `start` first.
    fn on_refused(&mut self, node: &str, session: u32, code: &str, now: Instant) {
        if !self
            .driving
            .as_ref()
            .is_some_and(|d| d.node == node && d.session == session)
        {
            return;
        }
        self.bring_home_quietly(now);
        self.remember_refusal(node, code, now);
        if code == refused::PLANE_STALE {
            // The one refusal that repairs itself: both ends run a round, and the
            // source retries once the plane converges.
            self.want_layout(node, now);
        }
        // The `by` of a BUSY names the holder for the interface's sentence, and
        // `input.refused`'s frozen shape has no room for it: the snapshot says
        // `problem: "busy"`, and naming the holder there is a change to section
        // 12 rather than a field invented here.
        self.emit_refused(node, code, 1);
    }

    fn remember_refusal(&mut self, node: &str, code: &str, now: Instant) {
        // `IDLE` is the one word here that is not about the far side at all: OUR
        // source went quiet, so there is nothing to tell the interface about that
        // machine and nothing to back off from beyond a breath.
        let backoff = match code {
            refused::PLANE_STALE | ended::IDLE => BACKOFF_STALE,
            _ => BACKOFF_REFUSED,
        };
        let problem = match code {
            refused::NOT_ALLOWED | ended::REVOKED => Some(PeerProblem::NotAllowed),
            // A target's own user asked for it back, which from here is the same
            // sentence as another computer holding it: that computer is in use.
            refused::BUSY | ended::TAKEN => Some(PeerProblem::Busy),
            refused::PLANE_STALE => Some(PeerProblem::PlaneStale),
            refused::NO_BACKEND => Some(PeerProblem::NoBackend),
            // Nobody is holding that machine: its pointer is pinned to its own
            // screen, and the remedy is a switch rather than waiting.
            refused::LOCKED => Some(PeerProblem::Locked),
            ended::IDLE => None,
            // A code from a newer peer than this build arrives as `UNKNOWN`
            // (`wire::code_in` normalises anything outside a closed set), and
            // there is nothing to say about it beyond that. It used to become
            // "busy", which put a fabricated sentence on the row ("that computer
            // is already being driven") next to a transient one that honestly said
            // the reason was not known. The backoff below still applies: what is
            // dropped is the invented standing state, not the caution.
            _ => None,
        };
        let peer = self.peers.entry(node.to_string()).or_default();
        peer.backoff_until = Some(now + backoff);
        peer.problem = problem;
        self.dirty = true;
    }

    /// Brings the keyboard home: the ONE way a driving session ends.
    ///
    /// Unpin, stop swallowing, warp the pointer to the point it left from, clear
    /// the session, and tell the target if the pipe still lives. Every one of the
    /// ten channel deaths, the hotkey, the stall watchdog and `input.release`
    /// funnel into it.
    fn bring_home(&mut self, code: &str, now: Instant) {
        let Some(driving) = self.driving.take() else {
            return;
        };
        let alive = self
            .peers
            .get(&driving.node)
            .is_some_and(|p| p.out.is_some());
        if alive && driving.accepted {
            // The pending position first, so a `stop` never overtakes the
            // position it was sent from.
            self.flush_now(&driving, now);
            // `rel` BEFORE `stop`, deliberately: a target ends its session on the
            // `stop`, so a `rel` after it names a session that no longer exists
            // and does nothing.
            let rel = Frame::ReleaseAll {
                session: driving.session,
            };
            self.send(&driving.node, &rel, now);
            let stop = Frame::Stop {
                session: driving.session,
                code: code.to_string(),
            };
            self.send(&driving.node, &stop, now);
        }
        self.confine(None);
        // Only a Full session warps: a keyboard-only session never moved the
        // pointer, so putting it back where the session started would move it
        // for the first time on the way out.
        if driving.accepted && driving.mode == Mode::Full {
            self.warp(driving.home);
            self.last_pointer = Some(driving.home);
        }
        self.apply_capture();
        self.forget_dwell(now);
        // A session the human did not end gets a sentence. Their keyboard just
        // came back on its own, which is exactly the kind of silence the epic
        // forbids, and neither of these two reasons leaves any other trace: the
        // session simply vanishes from the snapshot and no `problem` is set (the
        // fault is not the far side's word about itself). `RETURNED` and `MOVED`
        // are the human's own gesture, and announcing those would be noise.
        // `announce` coalesces per code per second, so a flapping link cannot
        // flood an interface with it.
        if code == stopped::SLOW || code == stopped::GONE {
            self.announce(&driving.node, code, false, now);
        }
        self.dirty = true;
    }

    /// The same teardown with no `stop`: the session is already over on the far
    /// side (it refused, or it ended it).
    fn bring_home_quietly(&mut self, now: Instant) {
        let Some(driving) = self.driving.take() else {
            return;
        };
        self.confine(None);
        if driving.accepted && driving.mode == Mode::Full {
            self.warp(driving.home);
            self.last_pointer = Some(driving.home);
        }
        self.apply_capture();
        self.forget_dwell(now);
        self.dirty = true;
    }

    /// Watch whenever a session could start, Swallow while driving, Off
    /// otherwise. Every teardown path reaches a resting value: a swallow that
    /// outlives its session leaves a machine with a dead keyboard.
    fn apply_capture(&mut self) {
        let want = if self.driving.is_some() {
            CaptureMode::Swallow
        } else if self.driven.is_some() || !self.caps.can_drive() {
            // A machine being driven does not capture (D15).
            CaptureMode::Off
        } else if self.graph.segments.iter().any(|s| {
            self.settings.drives(&s.node_id)
                && self
                    .peers
                    .get(&s.node_id)
                    .is_some_and(|p| p.link == Link::Warm)
        }) {
            CaptureMode::Watch
        } else {
            CaptureMode::Off
        };
        if self.capture == Some(want) && self.capture_sent == Some(want) {
            return;
        }
        let was_on = matches!(
            self.capture_sent,
            Some(CaptureMode::Watch) | Some(CaptureMode::Swallow)
        );
        self.capture = Some(want);
        // A backend that cannot capture is not asked to START: with every
        // capability false there is nothing to turn on, and calling anyway would
        // make a test of the capability-less platform read as if the engine had
        // tried.
        //
        // Turning it OFF is not the same call and is deliberately NOT gated on the
        // capability, as long as we are undoing something we did. The case is a
        // grant withdrawn mid-session: the capabilities narrow to nothing while the
        // backend is still swallowing, and a gate on `caps.capture` would then
        // leave the machine swallowing its owner's keystrokes with no session left
        // to send them to, which is a dead keyboard until the process is restarted.
        // Stopping something that was never started stays silent, which is what
        // keeps the capability-less platform honest.
        if self.caps.capture || (want == CaptureMode::Off && was_on) {
            self.capture_sent = Some(want);
            self.backend.capture(want);
        }
    }

    /// Capture Off for good, whatever the capability says, as long as there is something of
    /// ours to stop.
    ///
    /// Not `apply_capture`, which would compute Watch again for a peer that is still warm: its
    /// one caller is the end of the process and nothing comes after it. The rule about the
    /// capability is [`Engine::apply_capture`]'s and the reason is the same one, which is why
    /// it is stated in one place rather than copied inline where it used to be.
    fn force_capture_off(&mut self) {
        let was_on = matches!(
            self.capture_sent,
            Some(CaptureMode::Watch) | Some(CaptureMode::Swallow)
        );
        let already_off = self.capture_sent == Some(CaptureMode::Off);
        self.capture = Some(CaptureMode::Off);
        if !already_off && (self.caps.capture || was_on) {
            self.capture_sent = Some(CaptureMode::Off);
            self.backend.capture(CaptureMode::Off);
        }
    }

    /// Pins or releases the pointer, if this machine can.
    ///
    /// Releasing follows [`Engine::apply_capture`]'s rule for the same reason: a
    /// pointer pinned by a session whose grant was withdrawn mid-flight would stay
    /// pinned for ever, so `confine(None)` goes through whenever there is a
    /// confinement of ours to lift, capability or not.
    fn confine(&mut self, rect: Option<Rect>) {
        if self.caps.confine || (rect.is_none() && self.confined) {
            self.confined = rect.is_some();
            self.backend.confine(rect);
        }
    }

    /// Puts the pointer somewhere, if this machine can.
    fn warp(&self, to: Point) {
        if self.caps.warp {
            self.backend.warp(to);
        }
    }

    // ------------------------------------------------------- backend upcalls

    /// One upcall from the platform backend.
    pub async fn on_backend(&mut self, event: BackendEvent, now: Instant) {
        match event {
            BackendEvent::Motion(motion) => self.on_motion(motion, now),
            BackendEvent::Button { button, down } => {
                if let Some(driving) = &self.driving
                    && driving.accepted
                    && driving.mode == Mode::Full
                {
                    let (node, session) = (driving.node.clone(), driving.session);
                    // The pending position FIRST, and only then this frame's own
                    // counter: a click at the previous position is a click in the
                    // wrong place, and a counter taken first would number the two
                    // frames in the order they were not sent.
                    self.flush_pending(now);
                    let n = self.next_n();
                    let frame = Frame::Button {
                        session,
                        n,
                        button,
                        down,
                    };
                    self.send(&node, &frame, now);
                }
            }
            BackendEvent::Wheel { dx, dy, pixels } => {
                if let Some(driving) = &self.driving
                    && driving.accepted
                    && driving.mode == Mode::Full
                {
                    let (node, session) = (driving.node.clone(), driving.session);
                    self.flush_pending(now);
                    let n = self.next_n();
                    let frame = Frame::Wheel {
                        session,
                        n,
                        dx,
                        dy,
                        pixels,
                    };
                    self.send(&node, &frame, now);
                }
            }
            BackendEvent::Key(event) => self.on_key(event, now),
            BackendEvent::MonitorsChanged => {
                self.monitors = self.backend.monitors().await;
                self.refresh_caps();
                self.republish(now);
                self.apply_capture();
                self.dirty = true;
            }
            BackendEvent::LayoutChanged { layout, group } => {
                self.layout = layout;
                // The active group, so a stroke that has to switch group can switch
                // BACK to it: a session that silently left the machine in another
                // group would break the next thing its owner types. Nothing in
                // `keys.rs` can read it, so it is tracked here from what the
                // backend says and from nowhere else.
                self.group = group;
                // Everything we are holding goes FIRST, and this is not
                // housekeeping: a `PlatformKey` is a code AND a detail, and a
                // re-resolve after a layout change can hand back the same code
                // with a different detail. `Held::holds` would then say false, and
                // a game's held W (or a held Control) would stay physically down
                // until the session ended. Releasing here is the only moment the
                // old keys are still describable.
                self.release_held();
                self.resolver.layout_changed(&self.layout);
                // Beside the cache it just emptied, and not a line later: without
                // this every symbol needing AltGr would degrade silently to a
                // Unicode injection, or to `UNRESOLVED` on a target whose backend
                // has none.
                self.mod_keys.learn(&self.backend, &mut self.resolver).await;
            }
            BackendEvent::CapabilitiesChanged => {
                // The whole point of the event: what this machine can do is asked
                // again, and what it can PRODUCE with it. The resolver caches
                // negative answers on purpose, so without the relearn a backend
                // that had no keymap or no grant when it was first asked would be
                // remembered as unable to type for the life of the process.
                let fresh = self.backend.capabilities();
                let before = std::mem::replace(&mut self.caps, fresh);
                // BEFORE the teardowns and before the relearn, because it is the cheapest
                // thing here and the most urgent: a key somewhere may be physically down.
                self.drain_stranded();
                // The teardowns BEFORE the relearn, which is the opposite of the order this
                // arm had. A session that is about to end has no use for a resolver rebuilt
                // for capabilities it will never produce anything with, and the relearn is
                // five round trips it would have waited for first.
                if self.driving.is_some() && !self.caps.can_drive() {
                    // The grant went the other way: this machine can no longer
                    // swallow or pin, so the session it is holding somebody's
                    // keyboard in has to end rather than half work.
                    self.bring_home(stopped::GONE, now);
                }
                if self.driven.is_some() && !self.caps.can_be_driven() {
                    // `ended::`, because the frame is an `Ended`. It used to be the `refused::`
                    // constant of the same name, which put the right STRING on the wire by
                    // coincidence: the two are separate namespaces with separate closed sets,
                    // and the day either value changes a strict peer reads this as UNKNOWN.
                    self.end_driven(Some(ended::NO_BACKEND), now);
                }
                // Only when something the resolver depends on actually moved. `relearn` throws
                // away every cached answer and re-runs the modifier learning, and the cost
                // lands on the next press of every distinct symbol; the seam puts no rate limit
                // on this upcall, and a backend that emits it for its own reasons (a rebuilt
                // event tap, a monitor coming back) would otherwise pay it every time.
                if before.inject_keys != self.caps.inject_keys
                    || before.unicode != self.caps.unicode
                    || before.inject_pointer != self.caps.inject_pointer
                {
                    self.relearn().await;
                }
                self.republish(now);
                self.apply_capture();
                self.dirty = true;
            }
            BackendEvent::Refused(refusal) => self.on_backend_refusal(refusal, now),
            BackendEvent::CaptureLost(why) => {
                self.refresh_caps();
                if self.driving.is_some() {
                    // The local backend died under us.
                    self.bring_home(stopped::GONE, now);
                }
                if why == CaptureLoss::Permission {
                    eprintln!(
                        "[1device-input] the OS withdrew permission to read the input devices"
                    );
                }
                // Neither the want nor what the backend was told survives: the capture this
                // engine believed it had is precisely what has just been lost.
                self.capture = None;
                self.capture_sent = None;
                self.apply_capture();
                self.dirty = true;
            }
        }
    }

    /// A refusal the OS handed back, coalesced to one `oops` per code per second
    /// with a count (D19).
    fn on_backend_refusal(&mut self, refusal: Refusal, now: Instant) {
        let Some(driven) = &self.driven else {
            return;
        };
        let node = driven.node.clone();
        self.announce(&node, refusal.code(), true, now);
    }

    /// A key the machine's own user pressed.
    fn on_key(&mut self, event: KeyEvent, now: Instant) {
        self.mods_held = event.mods;
        // The return hotkey is recognised HERE, swallowed here, never forwarded
        // and never negotiated. It works while the channel is dead, because
        // nothing about it involves the channel.
        if self.settings.hotkey().fires(&event) {
            if self.driving.is_some() {
                self.bring_home(stopped::RETURNED, now);
            }
            return;
        }
        let Some(driving) = &self.driving else {
            return;
        };
        if !driving.accepted {
            return;
        }
        let (node, session) = (driving.node.clone(), driving.session);
        self.flush_pending(now);
        let n = self.next_n();
        let frame = Frame::Key {
            session,
            n,
            usage: event.usage,
            key: event.key,
            sym: event.sym,
            mods: event.mods,
            layout: self.layout.clone(),
            down: event.down,
            lock: event.lock,
        };
        self.send(&node, &frame, now);
    }

    /// One raw pointer motion. Two jobs, and they never overlap: while driving it
    /// integrates the delta into the virtual cursor, and while watching it asks
    /// the graph about the INTENDED position.
    fn on_motion(&mut self, motion: crate::backend::Motion, now: Instant) {
        if self.driving.is_some() {
            self.integrate(motion.dx, motion.dy, now);
            return;
        }
        self.last_pointer = Some(motion.at);
        if self.driven.is_some() || !self.caps.can_drive() {
            return;
        }
        // The OS clamps its own pointer at the boundary of its own desktop, so
        // the position reported never goes past the edge. What the graph is asked
        // about is the position PLUS the delta, which does (section 7).
        let Some((from, px, py)) = self.own_to_plane(motion.at) else {
            self.forget_dwell(now);
            return;
        };
        let (ix, iy) = (px.saturating_add(motion.dx), py.saturating_add(motion.dy));
        let settings = &self.settings;
        let at = graph::at_edge(
            &self.graph,
            &from,
            ix,
            iy,
            self.mods_held,
            settings.locked,
            &|s: &Segment| settings.guards(&s.from, &s.to, s.side),
        );
        match at {
            AtEdge::Segment(seg) => {
                let along = match seg.side {
                    graph::Side::Left | graph::Side::Right => iy,
                    graph::Side::Top | graph::Side::Bottom => ix,
                };
                self.press_edge(seg, along, now);
            }
            AtEdge::Inside | AtEdge::Wall(_) => self.forget_dwell(now),
        }
        self.crossing(now);
    }

    /// The pointer is against a crossing segment: start or continue the dwell.
    fn press_edge(&mut self, seg: Segment, along: i32, now: Instant) {
        let key = segment_key(&seg);
        if let Some(dwell) = &mut self.dwell {
            if segment_key(&dwell.seg) == key {
                dwell.along = along;
                return;
            }
            let old = segment_key(&dwell.seg);
            self.left.insert(old, now);
        }
        let guards = self.settings.guards(&seg.from, &seg.to, seg.side);
        // The double tap: the segment must have been left and re-reached within
        // its window before the dwell counts. Off by default, because two ways
        // of saying "not by accident" is one too many for a default.
        let tapped = if guards.double_tap_ms == 0 {
            true
        } else {
            self.left.get(&key).is_some_and(|left| {
                now.saturating_duration_since(*left)
                    <= Duration::from_millis(u64::from(guards.double_tap_ms))
            })
        };
        let node = seg.node_id.clone();
        // The seventh guard: if the target's channel is not warm, start warming
        // it now and let the crossing wait for both.
        let warm = self.peers.get(&node).is_some_and(|p| p.link == Link::Warm);
        let warming = if warm {
            None
        } else {
            // The channel starts warming when the dwell starts, and the crossing
            // waits for both. Adjacency already puts this peer in the warm set,
            // so all this needs is the trigger.
            self.open_due = true;
            Some(now)
        };
        self.dwell = Some(Dwell {
            seg,
            since: now,
            along,
            tapped,
            warming,
            probed: false,
        });
        // One extra probe the moment a dwell starts: a three second old figure is
        // not what this decision deserves.
        if warm {
            let ms = self.stamp(now);
            let frame = Frame::Ping { ms };
            self.send(&node, &frame, now);
            if let Some(peer) = self.peers.get_mut(&node) {
                peer.last_ping = Some(now);
            }
            if let Some(dwell) = &mut self.dwell {
                dwell.probed = true;
            }
        }
    }

    fn forget_dwell(&mut self, now: Instant) {
        if let Some(dwell) = self.dwell.take() {
            self.left.insert(segment_key(&dwell.seg), now);
            if self.left.len() > 64 {
                // A bound, because the map is keyed by a segment and a plane can
                // be rebuilt: the oldest entries are of no use to a double tap.
                let oldest = self
                    .left
                    .iter()
                    .min_by_key(|(_, when)| **when)
                    .map(|(key, _)| key.clone());
                if let Some(oldest) = oldest {
                    self.left.remove(&oldest);
                }
            }
        }
    }

    /// Has the dwell earned its crossing? Checked on every motion event and on
    /// the deadline, because a hand that pushes to the edge and holds still sends
    /// no more events.
    fn crossing(&mut self, now: Instant) {
        let Some(dwell) = &self.dwell else {
            return;
        };
        if !dwell.tapped || self.driving.is_some() || self.driven.is_some() {
            return;
        }
        let guards = self.guards_of(&dwell.seg);
        if now.saturating_duration_since(dwell.since)
            < Duration::from_millis(u64::from(guards.dwell_ms))
        {
            return;
        }
        let node = dwell.seg.node_id.clone();
        let warm = self.peers.get(&node).is_some_and(|p| p.link == Link::Warm);
        if !warm {
            let waited = dwell
                .warming
                .map(|started| now.saturating_duration_since(started));
            if waited.is_none_or(|waited| waited <= CROSS_OPEN_BOUND) {
                return;
            }
            // Beyond the bound: refused with a sentence, while the channel keeps
            // warming in the background, so the second attempt succeeds.
            self.announce(&node, NOT_WARM, false, now);
            self.forget_dwell(now);
            return;
        }
        let (seg, along) = {
            let dwell = self.dwell.as_ref().expect("checked");
            (dwell.seg.clone(), dwell.along)
        };
        let Some((x, y)) = graph::entry_point(&self.graph, &seg, along) else {
            self.forget_dwell(now);
            return;
        };
        let home = self.last_pointer.unwrap_or(Point { x, y });
        let cursor = match seg.side {
            graph::Side::Left | graph::Side::Right => {
                let to = self.graph.placed.get(&seg.to);
                to.map(|to| (crossing_x(to, seg.side), along))
            }
            graph::Side::Top | graph::Side::Bottom => {
                let to = self.graph.placed.get(&seg.to);
                to.map(|to| (along, crossing_y(to, seg.side)))
            }
        };
        let Some(cursor) = cursor else {
            self.forget_dwell(now);
            return;
        };
        self.forget_dwell(now);
        self.start_session(
            &node,
            None,
            cursor,
            seg.to.clone(),
            seg.from.clone(),
            home,
            x,
            y,
            now,
        );
    }

    /// Mints a session and sends the `start`. Nothing is confined or swallowed
    /// until the `ok` comes back.
    #[allow(clippy::too_many_arguments)]
    fn start_session(
        &mut self,
        node: &str,
        asked: Option<Mode>,
        cursor: (i32, i32),
        on: String,
        from: String,
        home: Point,
        x: i32,
        y: i32,
        now: Instant,
    ) -> bool {
        if self.driving.is_some() || self.driven.is_some() {
            return false;
        }
        if !self.caps.can_drive() {
            return false;
        }
        // The outbound enablement gates every session we start, not only the
        // channels we open: a channel a peer opened to be driven by us must not
        // become a way for us to drive it.
        if !self.settings.drives(node) {
            return false;
        }
        let Some(peer) = self.peers.get_mut(node) else {
            return false;
        };
        if peer.link != Link::Warm || peer.out.is_none() {
            return false;
        }
        let rtt = peer.rtt_ms;
        let session = peer.next_session;
        peer.next_session = peer.next_session.saturating_add(1);
        let pointer_ok = peer.hi.as_ref().is_some_and(|hi| hi.caps.inject_pointer);
        let mode = match asked {
            Some(Mode::Keys) => Mode::Keys,
            // Keys when the peer cannot inject the pointer, or when the path is
            // too far for a pointer to feel right. Between 10 and 60 ms the
            // session is Full and the number is in the snapshot; under 10 ms
            // nothing is said.
            _ if !pointer_ok || rtt.is_some_and(|ms| ms > RTT_MAX_MS) => Mode::Keys,
            _ => Mode::Full,
        };
        let keys = self.settings.peer(node).mode;
        let frame = Frame::Start {
            session,
            mode,
            keys,
            plane: self.plane_id.clone(),
            n: 1,
            x,
            y,
        };
        self.send(node, &frame, now);
        self.driving = Some(Driving {
            node: node.to_string(),
            session,
            mode,
            accepted: false,
            since: now,
            since_unix: unix_millis(),
            cursor,
            on,
            from,
            home,
            n: 1,
            pending: None,
            tokens: 1.0,
            tokens_at: now,
            out_tokens: f64::from(wire::OUT_RATE_MAX) / 10.0,
            out_at: now,
        });
        self.dirty = true;
        true
    }

    /// Starts the session an `input.take` asked for as soon as its channel is
    /// warm.
    ///
    /// The ordinary case is a take on a COLD channel: a human ticks "may drive"
    /// and presses the button, and the open is asynchronous. The gesture parks the
    /// intent (it must return inside the facade's budget and cannot wait for a
    /// dial), and this is what turns the parked intent into a session. Without it
    /// a take would answer `{}` and nothing at all would ever happen, which is the
    /// worst kind of bug: silent.
    fn parked_takes(&mut self, now: Instant) {
        if self.driving.is_some() || self.driven.is_some() {
            return;
        }
        let ready: Vec<(String, Option<Mode>, Instant)> = self
            .peers
            .iter()
            .filter(|(_, peer)| peer.link == Link::Warm && peer.out.is_some())
            .filter_map(|(node, peer)| peer.take.map(|(mode, parked)| (node.clone(), mode, parked)))
            .collect();
        for (node, mode, parked) in ready {
            if let Some(peer) = self.peers.get_mut(&node) {
                peer.take = None;
            }
            if now.saturating_duration_since(parked) > TAKE_PARK_BOUND {
                // A take parked long ago must not fire when the channel finally
                // warms: the hand that asked has moved on.
                self.announce(&node, NOT_WARM, false, now);
                continue;
            }
            let Some((key, cursor, x, y)) = self.take_entry(&node) else {
                continue;
            };
            let (home, from) = self.leaving_from();
            self.start_session(&node, mode, cursor, key, from, home, x, y, now);
        }
    }

    /// Where a session that crosses no edge leaves from: the pointer's last known
    /// place, and the monitor of ours it is on.
    fn leaving_from(&self) -> (Point, String) {
        let own = self.directory.own_node.clone().unwrap_or_default();
        let home = self.last_pointer.unwrap_or(Point { x: 0, y: 0 });
        let from = self
            .last_pointer
            .and_then(|at| self.own_to_plane(at))
            .map(|(key, _, _)| key)
            .or_else(|| {
                self.graph
                    .placed
                    .values()
                    .find(|p| p.node_id == own)
                    .map(|p| p.key.clone())
            })
            .unwrap_or_default();
        (home, from)
    }

    /// Integrates one relative delta into the virtual cursor and decides what it
    /// means: a position for the same peer, a handover, or the way home.
    fn integrate(&mut self, dx: i32, dy: i32, now: Instant) {
        let Some(driving) = &self.driving else {
            return;
        };
        if !driving.accepted || driving.mode != Mode::Full {
            return;
        }
        let (node, current) = (driving.node.clone(), driving.on.clone());
        let (nx, ny) = (
            driving.cursor.0.saturating_add(dx),
            driving.cursor.1.saturating_add(dy),
        );
        let landed = self
            .graph
            .placed
            .values()
            .find(|p| p.present && contains(p, nx, ny))
            .map(|p| (p.key.clone(), p.node_id.clone()));
        let own = self.directory.own_node.clone().unwrap_or_default();
        match landed {
            // Off the plane, or into a ghost: the wall, made arithmetic.
            None => {
                let (cx, cy) = graph::clamp_to(&self.graph, &current, nx, ny);
                self.move_cursor(cx, cy, now);
            }
            Some((key, at)) if at == node => {
                if let Some(driving) = &mut self.driving {
                    driving.on = key;
                }
                self.move_cursor(nx, ny, now);
            }
            // Back on one of our own monitors: bring the keyboard home.
            Some((_, at)) if at == own => self.bring_home(stopped::RETURNED, now),
            Some((key, at)) => {
                // On to another computer's screen. A neighbour we cannot start a
                // session with is a wall rather than a dead end.
                if !self.can_hand_over(&at, now) {
                    let (cx, cy) = graph::clamp_to(&self.graph, &current, nx, ny);
                    self.move_cursor(cx, cy, now);
                    return;
                }
                let Some((x, y)) = self
                    .graph
                    .placed
                    .get(&key)
                    .map(|placed| placed.to_own(nx, ny))
                else {
                    return;
                };
                let (from, home) = {
                    let driving = self.driving.as_ref().expect("checked");
                    (driving.from.clone(), driving.home)
                };
                self.bring_home(stopped::MOVED, now);
                self.start_session(&at, None, (nx, ny), key, from, home, x, y, now);
            }
        }
    }

    /// Can a handover start with this peer right now?
    fn can_hand_over(&self, node: &str, now: Instant) -> bool {
        self.settings.drives(node)
            && self.peers.get(node).is_some_and(|p| {
                p.link == Link::Warm
                    && p.out.is_some()
                    && !p.backoff_until.is_some_and(|until| now < until)
            })
    }

    /// The cursor moved: turn it into the target's own coordinates and offer it
    /// to the coalescer.
    fn move_cursor(&mut self, x: i32, y: i32, now: Instant) {
        let Some(driving) = &mut self.driving else {
            return;
        };
        if driving.cursor == (x, y) {
            return;
        }
        driving.cursor = (x, y);
        let on = driving.on.clone();
        let Some((tx, ty)) = self.graph.placed.get(&on).map(|p| p.to_own(x, y)) else {
            return;
        };
        self.offer_flow(Flow::At(tx, ty), now);
    }

    /// Coalescing by superseding, with a token bucket read on arrival and never
    /// slept on (D5).
    fn offer_flow(&mut self, flow: Flow, now: Instant) {
        let rate = {
            let Some(driving) = &self.driving else {
                return;
            };
            self.flow_rate(&driving.node)
        };
        let Some(driving) = &mut self.driving else {
            return;
        };
        let elapsed = now
            .saturating_duration_since(driving.tokens_at)
            .as_secs_f64();
        driving.tokens = (driving.tokens + elapsed * rate).min(FLOW_BURST);
        driving.tokens_at = now;
        if driving.tokens < 1.0 {
            // At most one pending position exists; a new one replaces it, and
            // nothing queues.
            driving.pending = Some(flow);
            return;
        }
        driving.tokens -= 1.0;
        driving.pending = None;
        let (node, session) = (driving.node.clone(), driving.session);
        let n = self.next_n();
        let frame = flow.frame(session, n);
        self.send_droppable(&node, &frame, now);
    }

    /// The trailing-edge flush: the only deadline the flow owns, armed only while
    /// a position is pending, so the last position after the flow stops is still
    /// delivered. Its jitter is invisible by construction: by the time it fires,
    /// nothing is moving.
    fn flow_flush(&mut self, now: Instant) {
        let Some(driving) = &self.driving else {
            return;
        };
        if driving.pending.is_none() {
            return;
        }
        let rate = self.flow_rate(&driving.node);
        let wait = Duration::from_secs_f64(((1.0 - driving.tokens).max(0.0) / rate).max(0.0));
        if now.saturating_duration_since(driving.tokens_at) < wait {
            return;
        }
        self.flush_pending(now);
    }

    /// Delivers the pending position, if there is one. Called before every frame
    /// that must not be coalesced: a click at the previous position is a click in
    /// the wrong place.
    fn flush_pending(&mut self, now: Instant) {
        let Some(driving) = &mut self.driving else {
            return;
        };
        let Some(flow) = driving.pending.take() else {
            return;
        };
        driving.tokens = 0.0;
        driving.tokens_at = now;
        let (node, session) = (driving.node.clone(), driving.session);
        let n = self.next_n();
        let frame = flow.frame(session, n);
        self.send_droppable(&node, &frame, now);
    }

    /// The same flush against a session already taken out of the state.
    fn flush_now(&mut self, driving: &Driving, now: Instant) {
        let Some(flow) = driving.pending else {
            return;
        };
        let frame = flow.frame(driving.session, driving.n);
        self.send_droppable(&driving.node, &frame, now);
    }

    /// The flow counter, once per flow frame for the life of a session.
    fn next_n(&mut self) -> u32 {
        match &mut self.driving {
            Some(driving) => {
                let n = driving.n;
                driving.n = driving.n.saturating_add(1);
                n
            }
            None => 0,
        }
    }

    /// 250 Hz under [`RTT_SILENT_MS`], 125 Hz above it. An unmeasured path reads
    /// as fast: a channel that just attached is pinged within the second, and
    /// either ceiling is far below the Core's rate cap.
    fn flow_rate(&self, node: &str) -> f64 {
        let slow = self
            .peers
            .get(node)
            .and_then(|p| p.rtt_ms)
            .is_some_and(|ms| ms >= RTT_SILENT_MS);
        if slow { FLOW_RATE_SLOW } else { FLOW_RATE_FAST }
    }

    // ------------------------------------------------------------ watchdogs

    fn keepalives(&mut self, now: Instant) {
        let session_with = self
            .driving
            .as_ref()
            .filter(|d| d.accepted)
            .map(|d| d.node.clone());
        let due: Vec<String> = self
            .peers
            .iter()
            .filter(|(node, peer)| {
                if peer.link != Link::Warm {
                    return false;
                }
                let gap = if session_with.as_deref() == Some(node.as_str()) {
                    SESSION_PING
                } else {
                    WARM_PING
                };
                peer.last_ping
                    .is_none_or(|last| now.saturating_duration_since(last) >= gap)
            })
            .map(|(node, _)| node.clone())
            .collect();
        for node in due {
            let ms = self.stamp(now);
            let frame = Frame::Ping { ms };
            self.send(&node, &frame, now);
            if let Some(peer) = self.peers.get_mut(&node) {
                peer.last_ping = Some(now);
            }
        }
    }

    fn watchdogs(&mut self, now: Instant) {
        // The source's own watchdog, and it is a security property: a hung or
        // misbehaving target must not be able to keep your keyboard.
        if let Some(driving) = &self.driving
            && driving.accepted
        {
            let last = self
                .peers
                .get(&driving.node)
                .and_then(|p| p.last_pong)
                .unwrap_or(driving.since);
            if now.saturating_duration_since(last) >= SOURCE_STALL {
                self.bring_home(stopped::SLOW, now);
            }
        }
        // The target's session idle. A live session pings every second, so a
        // healthy quiet session never trips it.
        if let Some(driven) = &self.driven
            && now.saturating_duration_since(driven.last_frame) >= SESSION_IDLE
        {
            self.end_driven(Some(ended::IDLE), now);
        }
    }

    // --------------------------------------------------------------- layout

    /// A layout round is wanted with this peer, rate limited to one message per
    /// peer per [`LAYOUT_MIN_GAP`] and coalesced.
    fn want_layout(&mut self, node: &str, now: Instant) {
        let peer = self.peers.entry(node.to_string()).or_default();
        peer.layout_due = true;
        let _ = now;
    }

    fn layout_everyone(&mut self, now: Instant) {
        let nodes: Vec<String> = self.directory.peers.keys().cloned().collect();
        for node in nodes {
            self.want_layout(&node, now);
        }
    }

    fn layout_rounds(&mut self, now: Instant) {
        if now.saturating_duration_since(self.sweep_at) >= LAYOUT_SWEEP {
            self.sweep_at = now;
            self.layout_everyone(now);
            // The slow sweep is also the backstop that re-warms a peer whose
            // channel would not open earlier.
            self.open_due = true;
        }
        let Some(own_key) = self.own_key() else {
            return;
        };
        let due: Vec<String> = self
            .peers
            .iter()
            .filter(|(_, peer)| {
                peer.layout_due
                    && peer
                        .layout_sent
                        .is_none_or(|sent| now.saturating_duration_since(sent) >= LAYOUT_MIN_GAP)
            })
            .map(|(node, _)| node.clone())
            .collect();
        for node in due {
            // Cleared whether or not a message goes out, and that is not tidiness:
            // a peer left `layout_due` with nothing to send it to would keep the
            // deadline permanently in the past and spin the loop. A peer that is
            // away is marked again by the directory the moment it becomes
            // reachable, which is one of the letter's own triggers.
            if let Some(peer) = self.peers.get_mut(&node) {
                peer.layout_due = false;
            }
            let Some(entry) = self.directory.peers.get(&node) else {
                continue;
            };
            if !entry.reachable {
                continue;
            }
            let device_id = entry.device_id.clone();
            let payload = self.plane.offer(&own_key);
            self.effects.push(Effect::Send { device_id, payload });
            if let Some(peer) = self.peers.get_mut(&node) {
                peer.layout_sent = Some(now);
            }
        }
    }

    fn own_key(&self) -> Option<String> {
        self.directory
            .own_node
            .as_ref()
            .map(|_| self.store.identity().public_hex())
    }

    /// A `peer.message`: one layout document, merged idempotently.
    pub fn on_peer_message(&mut self, node: &str, payload: &Value, now: Instant) {
        // A sender the directory cannot name is dropped (section 1).
        if !self.directory.peers.contains_key(node) {
            return;
        }
        if payload.get("t").and_then(Value::as_str) != Some("layout") {
            return;
        }
        let merged = self.plane.merge(node, payload);
        if !merged.changed {
            return;
        }
        self.persist_plane();
        self.rebuild(now);
        // A plane that changed can have changed who is adjacent, and a peer that
        // has just become a neighbour belongs in the warm set.
        self.open_due = true;
        // Epidemic convergence: a merge that changed something is told onward,
        // which is what makes a plane reach a device that was offline when the
        // drag happened. A merge that changed nothing sends nothing, which is
        // what makes it terminate.
        self.layout_everyone(now);
        self.apply_capture();
        self.dirty = true;
    }

    // -------------------------------------------------------------- refusals

    /// One refusal, coalesced to at most one per code per device per second, with
    /// a count. `oops` says whether the driving side hears it on the wire too.
    fn announce(&mut self, node: &str, code: &str, oops: bool, now: Instant) {
        let key = (node.to_string(), code.to_string());
        let send = match self.windows.get_mut(&key) {
            Some(window) if now < window.until => {
                window.pending = window.pending.saturating_add(1);
                window.oops |= oops;
                None
            }
            _ => {
                let pending = self.windows.get(&key).map_or(0, |w| w.pending);
                self.windows.insert(
                    key,
                    Window {
                        until: now + REFUSAL_WINDOW,
                        pending: 0,
                        oops,
                    },
                );
                Some(pending.saturating_add(1))
            }
        };
        if let Some(count) = send {
            self.say_refusal(node, code, count, oops, now);
        }
    }

    fn refusal_windows(&mut self, now: Instant) {
        let due: Vec<(String, String, u32, bool)> = self
            .windows
            .iter()
            .filter(|(_, window)| window.pending > 0 && now >= window.until)
            .map(|((node, code), window)| (node.clone(), code.clone(), window.pending, window.oops))
            .collect();
        for (node, code, count, oops) in due {
            self.windows.insert(
                (node.clone(), code.clone()),
                Window {
                    until: now + REFUSAL_WINDOW,
                    pending: 0,
                    oops,
                },
            );
            self.say_refusal(&node, &code, count, oops, now);
        }
        // A window with nothing pending and long expired is litter.
        self.windows
            .retain(|_, window| window.pending > 0 || now < window.until + REFUSAL_WINDOW);
    }

    fn say_refusal(&mut self, node: &str, code: &str, count: u32, oops: bool, now: Instant) {
        if oops
            && let Some(driven) = &self.driven
            && driven.node == node
        {
            let frame = Frame::Oops {
                session: driven.session,
                code: code.to_string(),
                count,
            };
            self.send(node, &frame, now);
        }
        self.emit_refused(node, code, count);
    }

    fn emit_refused(&mut self, node: &str, code: &str, count: u32) {
        let Some(device_id) = self.directory.device_of(node) else {
            return;
        };
        self.effects.push(Effect::Emit {
            method: "input.refused".into(),
            params: json!({ "device_id": device_id, "code": code, "count": count }),
        });
    }

    // ------------------------------------------------------------- emission

    fn emit_snapshot(&mut self, now: Instant) {
        if !self.dirty || now.saturating_duration_since(self.last_emit) < EMIT_GAP {
            return;
        }
        let state = self.status();
        self.dirty = false;
        self.last_emit = now;
        if state == self.last_snapshot {
            return;
        }
        self.last_snapshot = state.clone();
        self.effects.push(Effect::Emit {
            method: "input.updated".into(),
            params: json!({ "state": state }),
        });
    }

    // ---------------------------------------------------------------- frames

    /// Writes one frame to a peer's outbox. Never blocks the loop: a frame the
    /// outbox cannot take is a channel task that is not draining, which is a
    /// channel that is already dying.
    fn send(&mut self, node: &str, frame: &Frame, now: Instant) {
        self.write_frame(node, frame, false, now);
    }

    /// The same, for a frame that may be dropped: a position, and only a
    /// position.
    fn send_droppable(&mut self, node: &str, frame: &Frame, now: Instant) {
        self.write_frame(node, frame, true, now);
    }

    fn write_frame(&mut self, node: &str, frame: &Frame, droppable: bool, now: Instant) {
        let bytes = match wire::encode(frame) {
            Ok(bytes) => bytes,
            // Never retried into the Core's `FRAME_TOO_LARGE`: our own cap is half
            // of the Core's, and a frame above it is a bug in this engine. What the
            // letter asks for instead is a DEGRADE where one exists: a key frame
            // drops its symbol and keeps its usage, so the keystroke still lands
            // positionally rather than being lost.
            Err(e) => {
                let mut degraded = frame.clone();
                if degraded.degrade()
                    && let Ok(bytes) = wire::encode(&degraded)
                {
                    eprintln!("[1device-input] an oversized frame went out degraded: {e}");
                    bytes
                } else {
                    if !droppable {
                        eprintln!("[1device-input] refusing to send an oversized frame: {e}");
                    }
                    return;
                }
            }
        };
        if !self.allow_out(node, droppable, now) {
            return;
        }
        let Some(peer) = self.peers.get(node) else {
            return;
        };
        let Some(out) = &peer.out else {
            return;
        };
        if let Err(e) = out.try_send(bytes)
            && !droppable
        {
            eprintln!("[1device-input] a channel is not draining, dropping a frame: {e}");
        }
    }

    /// The whole emission's backstop, a quarter of the Core's rate cap. A
    /// position is dropped when the bucket is empty; anything else is let
    /// through, because the frames that are not positions are bounded by a pair
    /// of hands and cannot reach this cap on their own.
    fn allow_out(&mut self, node: &str, droppable: bool, now: Instant) -> bool {
        let Some(driving) = &mut self.driving else {
            return true;
        };
        if driving.node != node {
            return true;
        }
        let rate = f64::from(wire::OUT_RATE_MAX);
        let elapsed = now.saturating_duration_since(driving.out_at).as_secs_f64();
        driving.out_tokens = (driving.out_tokens + elapsed * rate).min(rate / 10.0);
        driving.out_at = now;
        if driving.out_tokens >= 1.0 {
            driving.out_tokens -= 1.0;
            return true;
        }
        !droppable
    }

    // ----------------------------------------------------------- persistence

    fn persist_plane(&mut self) {
        if let Err(e) = self.store.save_plane(&self.plane.to_stored()) {
            eprintln!("[1device-input] cannot write the plane: {e}");
        }
    }

    fn persist_settings(&mut self) {
        if let Err(e) = self.store.save_settings(&self.settings.to_value()) {
            eprintln!("[1device-input] cannot write the settings: {e}");
        }
    }

    // -------------------------------------------------------------- geometry

    /// This machine's own desktop coordinates to the plane, and which of our
    /// monitors the point is on. The inverse of [`Placed::to_own`].
    fn own_to_plane(&self, at: Point) -> Option<(String, i32, i32)> {
        let own = self.directory.own_node.as_deref()?;
        self.graph
            .placed
            .values()
            .find(|p| {
                p.node_id == own
                    && at.x >= p.own.x
                    && at.x < p.own.x.saturating_add(p.own.w)
                    && at.y >= p.own.y
                    && at.y < p.own.y.saturating_add(p.own.h)
            })
            .map(|p| {
                (
                    p.key.clone(),
                    p.x.saturating_add(at.x.saturating_sub(p.own.x)),
                    p.y.saturating_add(at.y.saturating_sub(p.own.y)),
                )
            })
    }

    fn guards_of(&self, seg: &Segment) -> Guards {
        self.settings.guards(&seg.from, &seg.to, seg.side)
    }

    /// Where a `input.take` puts the pointer: the middle of the target's primary
    /// screen, since no edge was crossed. Returns the plane cursor, the placed
    /// monitor it is on, and the point in the target's own desktop coordinates.
    fn take_entry(&self, node: &str) -> Option<(String, (i32, i32), i32, i32)> {
        let mut best: Option<&Placed> = None;
        for placed in self.graph.placed.values() {
            if placed.node_id != node || !placed.present {
                continue;
            }
            if best.is_none_or(|held| !held.own.primary && placed.own.primary) {
                best = Some(placed);
            }
        }
        let placed = best?;
        let (cx, cy) = (
            placed.x.saturating_add(placed.w / 2),
            placed.y.saturating_add(placed.h / 2),
        );
        let (x, y) = placed.to_own(cx, cy);
        Some((placed.key.clone(), (cx, cy), x, y))
    }

    // ----------------------------------------------------------------- facade

    /// The ten methods of the frozen vocabulary (doc/input-sharing.md, section
    /// 12). `Err` carries this engine's own application code, or `param:<field>`
    /// for a malformed shape, which the loop turns into a genuine JSON-RPC
    /// `-32602`.
    ///
    /// A gesture validates, persists and RETURNS, and it answers only with what
    /// THIS machine already knows. Two reasons, and they point the same way: the
    /// facade's budget is 10 s and this loop is the only one there is, so a gesture
    /// that awaited a round trip would stall the return hotkey; and answering for
    /// the far side would mean caching its grant, which D14 forbids because a
    /// cached grant is wrong exactly when it matters. What the far side says
    /// arrives within a round trip, as that device's `problem` in the snapshot and
    /// as an `input.refused`.
    pub fn serve(&mut self, method: &str, params: &Value, now: Instant) -> Result<Value, String> {
        if method == "input.status" {
            return Ok(self.status());
        }
        // Every gesture needs the directory: it is what turns the `device_id` at
        // the API boundary into the node_id everything here is keyed by, and it
        // is what names this device to itself. The sync engine's precedent.
        let Some(own) = self.directory.own_node.clone() else {
            return Err("INPUT_NOT_READY".into());
        };
        match method {
            "input.place" => {
                let spots = read_spots(params)?;
                for key in spots.keys() {
                    if plane::split_spot_key(key).is_none() {
                        return Err("param:spots".into());
                    }
                }
                // A spot naming a monitor NO record currently claims is kept, not
                // refused, and that is the ghost rule rather than laxity: a screen
                // that is away keeps its place (section 6), so the snapshot an
                // interface drags contains spots marked `present: false`, and it
                // sends the whole set back. Refusing them would make one unplugged
                // screen refuse every future drag on that plane, which is the
                // opposite of "undocking must not lose the arrangement". The only
                // thing checked here is the shape; the plane bounds the count, and
                // the lay-out drops a spot whose device the account no longer has.
                self.plane
                    .place(&own, self.store.identity(), spots)
                    .map_err(|why| match why {
                        // A shape this engine cannot store: the caller's request
                        // is malformed, whatever it meant.
                        plane::PlaceError::BadKey
                        | plane::PlaceError::OffPlane
                        | plane::PlaceError::TooMany
                        | plane::PlaceError::TooBig => "param:spots".to_string(),
                        // Not a shape at all: the arrangement counter is at its
                        // ceiling, so this drag cannot outrank what is held. It is
                        // a local failure a caller can only be told about, and it
                        // gets its own code rather than being dressed as a
                        // malformed request, because the request was fine.
                        plane::PlaceError::Ceiling | plane::PlaceError::BadAuthor => {
                            "INPUT_INTERNAL".to_string()
                        }
                    })?;
                self.persist_plane();
                self.rebuild(now);
                self.layout_everyone(now);
                self.apply_capture();
                self.open_due = true;
                self.dirty = true;
                Ok(json!({}))
            }
            "input.allow" => {
                let allowed = bool_of(params, "allowed")?;
                let node = self.resolve(params)?;
                // A refusal here is a table that is full, and the interface is
                // owed a refusal rather than a success: showing a door as open
                // because a bound silently dropped the grant is the one failure
                // this vocabulary has a code for.
                let applied = self.settings.set_allow(&node, allowed);
                self.applied(applied)?;
                // A grant withdrawn while a session runs ends it: the whole
                // point of a grant being the security boundary.
                if !allowed && self.driven.as_ref().is_some_and(|d| d.node == node) {
                    self.end_driven(Some(ended::REVOKED), now);
                }
                Ok(json!({}))
            }
            "input.drive" => {
                let allowed = bool_of(params, "allowed")?;
                // `input.drive`'s mode is the KEY mode (`typing` or `positional`),
                // a different axis from `input.take`'s (`full` or `keys`).
                let mode = key_mode(params)?;
                let node = self.resolve(params)?;
                let applied = self.settings.set_drive(&node, allowed, mode);
                self.applied(applied)?;
                if !allowed && self.driving.as_ref().is_some_and(|d| d.node == node) {
                    self.bring_home(stopped::RETURNED, now);
                }
                Ok(json!({}))
            }
            "input.take" => {
                // `input.take`'s mode is what the session DRIVES (`full` or
                // `keys`), not how a keystroke resolves.
                let asked = match params.get("mode") {
                    None | Some(Value::Null) => None,
                    Some(value) => Some(
                        value
                            .as_str()
                            .and_then(Mode::parse)
                            .ok_or("param:mode".to_string())?,
                    ),
                };
                let node = self.resolve(params)?;
                if !self.caps.can_drive() {
                    return Err("INPUT_NO_BACKEND".into());
                }
                // The lock is about THIS screen, in both directions: a machine
                // whose pointer is pinned neither crosses nor hands its keyboard
                // over on request.
                if self.settings.locked {
                    return Err("INPUT_LOCKED".into());
                }
                // Nothing about the FAR side is answered here, and that is the
                // grant doctrine rather than an omission (D14, and section 12's
                // "Where" column). A gesture that answered `INPUT_NOT_ALLOWED`
                // would have had to cache the far side's grant, and a cached grant
                // is wrong exactly when it matters: a human who has just ticked
                // "may drive this computer" over there would be refused here from a
                // word that is already false. The far side's word arrives within a
                // round trip instead, as that device's `problem` in the snapshot
                // and as an `input.refused`, which is what the interface renders
                // either way.
                //
                // No preemption, and a machine being driven cannot also drive
                // (D15, D16).
                if self.driven.is_some() {
                    return Err("INPUT_BUSY".into());
                }
                if let Some(driving) = &self.driving {
                    if driving.node == node {
                        return Ok(json!({}));
                    }
                    return Err("INPUT_BUSY".into());
                }
                let rtt = self.peers.get(&node).and_then(|p| p.rtt_ms);
                if asked != Some(Mode::Keys) && rtt.is_some_and(|ms| ms > RTT_MAX_MS) {
                    // The offer is the keyboard alone, and the number is in the
                    // snapshot so the error needs no payload (D2).
                    return Err("INPUT_TOO_SLOW".into());
                }
                {
                    let peer = self.peers.entry(node.clone()).or_default();
                    // A human asking is a reason to try again, unambiguously: no
                    // gesture answers from a backoff, so clearing it here can only
                    // mean "attempt it now" and can never hide a word the
                    // interface still needed.
                    peer.backoff_until = None;
                    peer.problem = None;
                    peer.take = Some((asked, now));
                }
                self.open_due = true;
                self.dirty = true;
                // Warm already: the session starts inside the gesture. Cold: the
                // intent is parked and [`Engine::parked_takes`] starts it the
                // moment the handshake lands.
                if let Some((key, cursor, x, y)) = self.take_entry(&node) {
                    let (home, from) = self.leaving_from();
                    if self.start_session(&node, asked, cursor, key, from, home, x, y, now)
                        && let Some(peer) = self.peers.get_mut(&node)
                    {
                        peer.take = None;
                    }
                }
                Ok(json!({}))
            }
            "input.release" => {
                if self.driving.is_some() {
                    self.bring_home(stopped::RETURNED, now);
                } else if self.driven.is_some() {
                    // Its own user asked for it back, which is exactly what the
                    // dialect's `TAKEN` says.
                    self.end_driven(Some(ended::TAKEN), now);
                }
                Ok(json!({}))
            }
            "input.guards" => {
                let named = string_of(params, "monitor")?;
                let side = graph::Side::parse(&string_of(params, "side")?)
                    .ok_or("param:side".to_string())?;
                let guards = Guards::from_value(params.get("guards").ok_or("param:guards")?);
                let node = self.resolve(params)?;
                // `monitor` may name either end of the crossing: the neighbour's
                // screen (which is what `device_id` and `monitor` read like
                // together) or one of ours. Both are unambiguous, because a
                // monitor belongs to exactly one device.
                let far = qualify(&named, &node);
                let mine = qualify(&named, &own);
                let pairs: Vec<(String, String)> = self
                    .graph
                    .segments
                    .iter()
                    .filter(|s| {
                        s.side == side && s.node_id == node && (s.to == far || s.from == mine)
                    })
                    .map(|s| (s.from.clone(), s.to.clone()))
                    .collect();
                if pairs.is_empty() {
                    return Err("INPUT_UNKNOWN_MONITOR".into());
                }
                let mut applied = Applied::Unchanged;
                for (from, to) in pairs {
                    match self.settings.set_guards(&from, &to, side, guards) {
                        Applied::Refused => applied = Applied::Refused,
                        Applied::Changed if applied != Applied::Refused => {
                            applied = Applied::Changed;
                        }
                        _ => {}
                    }
                }
                self.applied(applied)?;
                Ok(json!({}))
            }
            "input.lock" => {
                let locked = bool_of(params, "locked")?;
                if self.settings.locked != locked {
                    self.settings.locked = locked;
                    self.persist_settings();
                    self.dirty = true;
                }
                if locked {
                    if self.driving.is_some() {
                        self.bring_home(stopped::RETURNED, now);
                    }
                    if self.driven.is_some() {
                        self.end_driven(Some(ended::LOCKED), now);
                    }
                }
                Ok(json!({}))
            }
            "input.hotkey" => {
                let names: Vec<String> = params
                    .get("keys")
                    .and_then(Value::as_array)
                    .ok_or("param:keys".to_string())?
                    .iter()
                    .map(|v| v.as_str().unwrap_or_default().to_string())
                    .collect();
                let hotkey = Hotkey::parse(&names).ok_or("param:keys".to_string())?;
                if self.settings.set_hotkey(hotkey) {
                    self.persist_settings();
                    self.dirty = true;
                }
                Ok(json!({}))
            }
            "input.remap" => {
                let map = params
                    .get("map")
                    .and_then(Value::as_object)
                    .ok_or("param:map".to_string())?;
                let mut table = BTreeMap::new();
                for (from, to) in map {
                    let Some(to) = to.as_str() else {
                        return Err("param:map".into());
                    };
                    table.insert(from.clone(), to.to_string());
                }
                let node = self.resolve(params)?;
                let applied = self.settings.set_remap(&node, table);
                self.applied(applied)?;
                Ok(json!({}))
            }
            // Unreachable while this list and [`crate::orchestrator::SERVED_METHODS`]
            // agree; refusing honestly beats a dropped reply.
            _ => Err("-32601".into()),
        }
    }

    /// Persists what a local gesture changed, and refuses what it could not store.
    ///
    /// The three answers of [`Applied`] are three different things and only one of
    /// them is a write: a bound that dropped a grant must reach the caller as
    /// `INPUT_INTERNAL`, or the interface shows a door as open that nothing here
    /// ever opened.
    fn applied(&mut self, applied: Applied) -> Result<(), String> {
        if applied.refused() {
            return Err("INPUT_INTERNAL".into());
        }
        if applied.changed() {
            self.persist_settings();
            self.dirty = true;
        }
        Ok(())
    }

    /// The `device_id` of a gesture, as a node_id.
    fn resolve(&self, params: &Value) -> Result<String, String> {
        let device_id = string_of(params, "device_id")?;
        self.directory
            .node_of(&device_id)
            .ok_or_else(|| "INPUT_DEVICE_UNKNOWN".to_string())
    }

    // --------------------------------------------------------------- snapshot

    /// The whole state, and the AUTHORITATIVE answer the notifications merely
    /// echo (doc/input-sharing.md, section 12). Answered entirely from this
    /// struct plus the live session: the Core stores nothing about input.
    pub fn status(&self) -> Value {
        let here = json!({
            "device_id": self.directory.own_device,
            "name": self.directory.own_name,
            "monitors": self.monitors.iter().map(|m| monitor_json(m, true)).collect::<Vec<Value>>(),
            "problem": self.caps.problem.map(crate::backend::Problem::code),
            "can_drive": self.caps.can_drive(),
            "can_be_driven": self.caps.can_be_driven(),
        });
        // One list an interface can draw the plane from, rectangles included, so
        // it never has to join two lists to find a width and end up drawing a
        // plane that disagrees with the one the engine crosses on.
        let spots: Vec<Value> = self
            .graph
            .placed
            .values()
            .map(|placed| {
                json!({
                    "monitor": placed.key,
                    "device_id": self.directory.device_of(&placed.node_id),
                    "name": placed.own.name,
                    "x": placed.x,
                    "y": placed.y,
                    "w": placed.w,
                    "h": placed.h,
                    "present": placed.present,
                    "primary": placed.own.primary,
                })
            })
            .collect();
        let devices: Vec<Value> = self
            .directory
            .peers
            .iter()
            .map(|(node, entry)| {
                let peer = self.peers.get(node);
                let settings = self.settings.peer(node);
                let monitors: Vec<Value> = self
                    .plane
                    .monitors
                    .get(node)
                    .filter(|held| held.verified)
                    .map(|held| {
                        held.list
                            .iter()
                            .map(|m| {
                                let key = plane::spot_key(node, &m.id);
                                let present = self
                                    .graph
                                    .placed
                                    .get(&key)
                                    .is_some_and(|placed| placed.present);
                                monitor_json(m, present)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                json!({
                    "device_id": entry.device_id,
                    "name": entry.name,
                    "state": self.device_state(node),
                    "monitors": monitors,
                    "rtt_ms": peer.and_then(|p| p.rtt_ms),
                    "lan": entry.lan,
                    "allowed": settings.allow,
                    "drive": settings.drive,
                    "mode": settings.mode.as_str(),
                    // A pair that cannot do its job says why from the snapshot
                    // alone, and the handshake is what lets it say so BEFORE
                    // anyone tries: a peer whose backend cannot type is
                    // `no_backend` here with no refusal ever having happened.
                    "problem": peer.and_then(Self::peer_problem)
                        .map(PeerProblem::code),
                })
            })
            .collect();
        let session = match (&self.driving, &self.driven) {
            (Some(driving), _) => json!({
                "device_id": self.directory.device_of(&driving.node),
                "direction": "out",
                "mode": driving.mode.as_str(),
                "since": driving.since_unix,
                "rtt_ms": self.peers.get(&driving.node).and_then(|p| p.rtt_ms),
            }),
            (None, Some(driven)) => json!({
                "device_id": self.directory.device_of(&driven.node),
                "direction": "in",
                "mode": driven.mode.as_str(),
                "since": driven.since_unix,
                "rtt_ms": self.peers.get(&driven.node).and_then(|p| p.rtt_ms),
            }),
            (None, None) => Value::Null,
        };
        json!({
            "here": here,
            "plane": {
                "id": self.plane_id,
                "spots": spots,
                "by": self.plane.placement.as_ref()
                    .and_then(|p| self.directory.device_of(&p.by)),
            },
            "devices": devices,
            "session": session,
            "guards": self.guards_json(),
            "lock": self.settings.locked,
            "hotkey": self.settings.hotkey().to_names(),
        })
    }

    /// Only the guards a human actually set: a default is an absence, exactly as
    /// the store keeps it, so the list says what was chosen rather than repeating
    /// what the code already says.
    ///
    /// Read back out of the settings' own document, which is the one place that
    /// knows how a guard key is spelled. `monitor` is the NEIGHBOUR's screen,
    /// fully qualified, which is what `input.guards` takes back.
    fn guards_json(&self) -> Vec<Value> {
        let stored = self.settings.to_value();
        let Some(guards) = stored.get("guards").and_then(Value::as_object) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (key, entry) in guards {
            let mut parts = key.split('|');
            let (Some(_from), Some(to), Some(side)) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            let Some((node, _)) = plane::split_spot_key(to) else {
                continue;
            };
            let mut row = json!({
                "device_id": self.directory.device_of(node),
                "monitor": to,
                "side": side,
            });
            if let (Some(row), Some(entry)) = (row.as_object_mut(), entry.as_object()) {
                for (field, value) in entry {
                    row.insert(field.clone(), value.clone());
                }
            }
            out.push(row);
        }
        out
    }

    /// The last thing the loop does, whatever ended it: this is the moment a
    /// keyboard is left working or left dead.
    pub fn shutdown(&mut self, now: Instant) {
        if self.driving.is_some() {
            self.bring_home(stopped::GONE, now);
        }
        if self.driven.is_some() {
            // Our own injector is going away, which is exactly what the
            // dialect's `NO_BACKEND` says.
            self.end_driven(Some(ended::NO_BACKEND), now);
        }
        self.release_held();
        // A forced Off rather than `apply_capture`, which would compute Watch again for a peer
        // that is still warm: this is the end of the process.
        self.force_capture_off();
        self.confine(None);
    }

    /// What the interface says about one pair: the refusal this engine remembers,
    /// and failing that whatever the peer's own handshake implies.
    ///
    /// **The order is remembered-first and it is not arbitrary.** A refusal is
    /// something that just happened to the person and names what to do next; the
    /// standing facts below it are permanent until that machine's session changes, so
    /// they are still there when the transient one clears (a `hi` and an accepted
    /// session both clear the remembered half, and this function keeps deriving the
    /// standing one afterwards).
    ///
    /// **A problem derived here is not a refusal**, which is why it lives here and
    /// not in `Peer::problem`: `device_state` reads that field and would call the pair
    /// `refused`, and an XWayland peer is `ready`. It really can be driven, it will
    /// really type into the X11 windows on that screen, and the sentence exists so a
    /// person knows what they are getting rather than being stopped from having it.
    fn peer_problem(peer: &Peer) -> Option<PeerProblem> {
        if let Some(remembered) = peer.problem {
            return Some(remembered);
        }
        let caps = &peer.hi.as_ref()?.caps;
        // Nothing there can type at all: the flattest thing to say, and it outranks
        // any partial word below it.
        if !caps.can_be_driven() {
            return Some(PeerProblem::NoBackend);
        }
        // That machine's own problem, seen from here. It travelled in `caps` from the
        // first version of the dialect and used to be read by nobody, which is how a
        // peer able to type into half its windows was offered as a peer with nothing
        // wrong at all.
        caps.problem.and_then(crate::backend::Problem::as_peer)
    }

    fn device_state(&self, node: &str) -> &'static str {
        if self.driving.as_ref().is_some_and(|d| d.node == node) {
            return "driving";
        }
        if self.driven.as_ref().is_some_and(|d| d.node == node) {
            return "driven";
        }
        let Some(peer) = self.peers.get(node) else {
            return "off";
        };
        if peer.problem.is_some() {
            return "refused";
        }
        match peer.link {
            Link::Warm => "ready",
            Link::Warming => "warming",
            Link::Cold => "off",
        }
    }
}

/// One monitor, as the snapshot carries it: its own machine's word about its own
/// screen, plus whether it is there right now.
fn monitor_json(m: &Monitor, present: bool) -> Value {
    json!({ "id": m.id, "name": m.name, "w": m.w, "h": m.h, "x": m.x, "y": m.y,
            "scale": m.scale, "primary": m.primary, "present": present })
}

/// A monitor named bare or fully qualified, as a spot key.
fn qualify(named: &str, node: &str) -> String {
    if named.contains('/') {
        named.to_string()
    } else {
        plane::spot_key(node, named)
    }
}

/// The arrangement a human dragged, from either shape the interface may send: the
/// snapshot's own list of spots, or a bare map keyed by monitor.
fn read_spots(params: &Value) -> Result<BTreeMap<String, Spot>, String> {
    let bad = || "param:spots".to_string();
    let spots = params.get("spots").ok_or_else(bad)?;
    let mut out = BTreeMap::new();
    let point = |value: &Value| -> Option<Spot> {
        Some(Spot {
            x: i32::try_from(value.get("x")?.as_i64()?).ok()?,
            y: i32::try_from(value.get("y")?.as_i64()?).ok()?,
        })
    };
    if let Some(list) = spots.as_array() {
        for entry in list {
            let monitor = entry
                .get("monitor")
                .and_then(Value::as_str)
                .ok_or_else(bad)?;
            out.insert(monitor.to_string(), point(entry).ok_or_else(bad)?);
        }
        return Ok(out);
    }
    if let Some(map) = spots.as_object() {
        for (monitor, value) in map {
            out.insert(monitor.clone(), point(value).ok_or_else(bad)?);
        }
        return Ok(out);
    }
    Err(bad())
}

fn string_of(params: &Value, key: &str) -> Result<String, String> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("param:{key}"))
}

fn bool_of(params: &Value, key: &str) -> Result<bool, String> {
    params
        .get(key)
        .and_then(Value::as_bool)
        .ok_or_else(|| format!("param:{key}"))
}

/// The optional `mode` of `input.drive`: which level of a key frame that peer's
/// sessions resolve first.
fn key_mode(params: &Value) -> Result<Option<KeyMode>, String> {
    match params.get("mode") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .and_then(KeyMode::parse)
            .map(Some)
            .ok_or_else(|| "param:mode".to_string()),
    }
}

/// One monitor's identity as a segment: what the dwell compares against.
fn segment_key(seg: &Segment) -> String {
    format!("{}|{}|{}", seg.from, seg.to, seg.side.as_str())
}

/// Is this plane point inside this placed monitor? The three lines
/// [`crate::graph`] keeps private, needed here to ask about ANY monitor rather
/// than only our own.
fn contains(p: &Placed, x: i32, y: i32) -> bool {
    x >= p.x && x < p.x.saturating_add(p.w) && y >= p.y && y < p.y.saturating_add(p.h)
}

/// Where the virtual cursor starts on the far monitor, in plane coordinates: one
/// pixel inside, so it does not read as against the wall it just came through.
fn crossing_x(to: &Placed, side: graph::Side) -> i32 {
    match side {
        graph::Side::Right => to.x,
        _ => to.x.saturating_add(to.w).saturating_sub(1),
    }
}

fn crossing_y(to: &Placed, side: graph::Side) -> i32 {
    match side {
        graph::Side::Bottom => to.y,
        _ => to.y.saturating_add(to.h).saturating_sub(1),
    }
}

/// Wall-clock milliseconds, for the one field of the snapshot that is a
/// timestamp. Never compared against another machine's.
fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::backend::{
        Action, CaptureMode, Motion as RawMotion, PlatformKey, Problem, Resolved, Want,
    };
    use crate::fake::{Calls, FakeBackend};
    use crate::identity::Identity;

    /// The ten ways a `peers.channel` can die (doc/core-api.md), whole. The
    /// hygiene matrix is table driven over exactly this list, so a reason cannot
    /// join the vocabulary without joining the test.
    const DEATHS: [&str; 10] = [
        "CLOSED",
        "REPLACED",
        "PEER_GONE",
        "DEVICE_REVOKED",
        "LOGGED_OUT",
        "ACCOUNT_LEFT",
        "SHUTDOWN",
        "FRAME_TOO_LARGE",
        "RATE_EXCEEDED",
        "IDLE_TIMEOUT",
    ];

    fn node(n: u8) -> String {
        std::iter::repeat_n(format!("{n:02x}"), 32).collect()
    }

    fn screen(id: &str, x: i32, y: i32, w: i32, h: i32) -> Monitor {
        Monitor {
            id: id.into(),
            name: format!("Screen {id}"),
            w,
            h,
            x,
            y,
            scale: 1000,
            primary: true,
        }
    }

    fn directory(peers: &[(&str, &str, bool)]) -> Directory {
        let mut rows = vec![json!({
            "device_id": "d_self", "node_id": node(0xa), "is_self": true,
            "name": "Here", "online": true, "reachable": true, "lan": true,
        })];
        for (device_id, node_id, reachable) in peers {
            rows.push(json!({
                "device_id": device_id, "node_id": node_id, "is_self": false,
                "name": "There", "online": true, "reachable": reachable, "lan": true,
            }));
        }
        Directory::parse(&Value::Array(rows))
    }

    /// One engine, its fake backend, and the outboxes of the channels a test
    /// attached, so a test reads exactly the frames the engine wrote.
    struct Harness {
        engine: Engine<FakeBackend>,
        fake: FakeBackend,
        out: BTreeMap<String, mpsc::Receiver<Vec<u8>>>,
        t0: Instant,
        root: PathBuf,
        _events: mpsc::Receiver<BackendEvent>,
        _dir: tempfile::TempDir,
    }

    impl Harness {
        fn new() -> Harness {
            let dir = tempfile::tempdir().expect("tempdir");
            let root = dir.path().join("input");
            let store = Store::open(root.clone()).expect("open");
            let (fake, events) = FakeBackend::new();
            let t0 = Instant::now();
            let engine = Engine::open(fake.clone(), store, t0).expect("open the engine");
            Harness {
                engine,
                fake,
                out: BTreeMap::new(),
                t0,
                root,
                _events: events,
                _dir: dir,
            }
        }

        /// What `held.json` says right now: the crash guard, as a reader at the
        /// next start would find it.
        fn held_file(&self) -> Value {
            let text = std::fs::read_to_string(self.root.join(HELD_FILE)).unwrap_or_default();
            serde_json::from_str(&text).unwrap_or(Value::Null)
        }

        /// Us at the origin, one peer's screen to our right, both placed, both in
        /// the directory. The plane every session test starts from.
        async fn desk(&mut self) -> String {
            let peer = node(0xb);
            self.fake.set_monitors(vec![screen("A", 0, 0, 1920, 1080)]);
            self.engine.start(self.t0).await;
            self.engine
                .set_directory(directory(&[("d_b", &peer, true)]), self.t0);
            // The peer's own signed word about its own screens, arriving the way
            // it really does.
            let their_dir = tempfile::tempdir().expect("tempdir");
            let theirs = Identity::load_or_generate(their_dir.path()).expect("mint");
            let mut plane = Plane::default();
            plane.publish_monitors(&peer, &theirs, vec![screen("B", 0, 0, 1920, 1080)]);
            self.engine
                .on_peer_message(&peer, &plane.offer(&theirs.public_hex()), self.t0);
            self.serve(
                "input.place",
                json!({ "spots": {
                    plane::spot_key(&node(0xa), "A"): { "x": 0, "y": 0 },
                    plane::spot_key(&peer, "B"): { "x": 1920, "y": 0 },
                } }),
            )
            .expect("place");
            peer
        }

        fn serve(&mut self, method: &str, params: Value) -> Result<Value, String> {
            self.engine.serve(method, &params, self.t0)
        }

        /// Does everything that is due at `t0` and forgets the effects, so a test
        /// measures its own deadline rather than a layout round's.
        fn settle(&mut self) {
            self.engine.pump(self.t0);
            let _ = self.engine.take_effects();
        }

        /// Attaches a channel the way the loop does, and answers its handshake.
        async fn warm(&mut self, peer: &str, pointer: bool) {
            self.warm_with(
                peer,
                json!({ "inject_keys": true, "inject_pointer": pointer }),
            )
            .await;
        }

        /// The same, with the peer's whole `caps` object written out. What a peer
        /// says about itself is the entire input of a pair's standing problem, so a
        /// test about that has to be able to say it.
        async fn warm_with(&mut self, peer: &str, caps: Value) {
            let (tx, rx) = mpsc::channel(256);
            // Attached the way an INCOMING offer is: the harness's default case is
            // a peer that opened a channel to be driven by us.
            self.engine.attach(peer, tx, false, self.t0);
            self.out.insert(peer.to_string(), rx);
            let hi = Frame::Hi {
                version: wire::VERSION,
                caps,
                plane: self.engine.plane_id().to_string(),
            };
            self.feed(peer, vec![hi], self.t0).await;
        }

        async fn feed(&mut self, peer: &str, frames: Vec<Frame>, now: Instant) {
            let batch: Vec<Vec<u8>> = frames
                .iter()
                .map(|f| wire::encode(f).expect("encodable"))
                .collect();
            self.engine.on_frames(peer, batch, now).await;
        }

        /// Everything the engine wrote to a peer since the last drain.
        fn frames(&mut self, peer: &str) -> Vec<Frame> {
            let mut out = Vec::new();
            if let Some(rx) = self.out.get_mut(peer) {
                while let Ok(bytes) = rx.try_recv() {
                    if let Some(frame) = wire::decode(&bytes) {
                        out.push(frame);
                    }
                }
            }
            out
        }

        fn calls(&self) -> Calls {
            self.fake.calls()
        }

        /// A live outbound session, accepted, in Full mode.
        async fn driving(&mut self, peer: &str) {
            self.serve(
                "input.drive",
                json!({ "device_id": "d_b", "allowed": true }),
            )
            .expect("drive");
            self.warm(peer, true).await;
            self.serve("input.take", json!({ "device_id": "d_b" }))
                .expect("take");
            let start = self
                .frames(peer)
                .into_iter()
                .find_map(|f| match f {
                    Frame::Start { session, .. } => Some(session),
                    _ => None,
                })
                .expect("a start went out");
            self.feed(peer, vec![Frame::Accepted { session: start }], self.t0)
                .await;
            self.settle();
        }

        /// A live inbound session, accepted, with the grant in place.
        async fn driven(&mut self, peer: &str) -> u32 {
            self.serve(
                "input.allow",
                json!({ "device_id": "d_b", "allowed": true }),
            )
            .expect("allow");
            self.warm(peer, true).await;
            let plane = self.engine.plane_id().to_string();
            self.feed(
                peer,
                vec![Frame::Start {
                    session: 1,
                    mode: Mode::Full,
                    keys: KeyMode::Typing,
                    plane,
                    n: 1,
                    x: 10,
                    y: 10,
                }],
                self.t0,
            )
            .await;
            assert!(
                self.frames(peer)
                    .iter()
                    .any(|f| matches!(f, Frame::Accepted { .. })),
                "the start must have been accepted"
            );
            self.settle();
            1
        }

        /// Spends whatever the token bucket had accrued by `now`, leaving it
        /// empty and a position pending. Returns `now`.
        async fn spend_the_burst(&mut self, peer: &str, now: Instant) -> Instant {
            for _ in 0..4 {
                let event = self.motion(0, 0, 1, 0);
                self.engine.on_backend(event, now).await;
            }
            let _ = self.frames(peer);
            now
        }

        fn motion(&mut self, x: i32, y: i32, dx: i32, dy: i32) -> BackendEvent {
            let _ = self;
            BackendEvent::Motion(RawMotion {
                at: Point { x, y },
                dx,
                dy,
            })
        }
    }

    // ------------------------------------------------------------ coalescing

    /// The coalescer, whole (doc/input-sharing.md, section 5 and D5): a burst
    /// with one token yields ONE frame and a pending position, a button flushes
    /// that position BEFORE itself, and the flow counter never repeats.
    #[tokio::test]
    async fn a_burst_of_positions_costs_one_frame_and_a_button_flushes_the_rest() {
        let mut h = Harness::new();
        let peer = h.desk().await;
        h.driving(&peer).await;
        let _ = h.frames(&peer);

        // Three motions at the SAME instant: one token, so one frame, and the
        // last position pending.
        for _ in 0..3 {
            let event = h.motion(0, 0, 1, 0);
            h.engine.on_backend(event, h.t0).await;
        }
        let frames = h.frames(&peer);
        assert_eq!(
            positions(&frames),
            vec![(961, 540)],
            "one token is one frame: {frames:?}"
        );

        // The button flushes the pending position first, and is never coalesced.
        let event = BackendEvent::Button {
            button: 1,
            down: true,
        };
        h.engine.on_backend(event, h.t0).await;
        let frames = h.frames(&peer);
        assert_eq!(
            positions(&frames),
            vec![(963, 540)],
            "the pending position goes out before the click: {frames:?}"
        );
        assert!(
            matches!(frames.last(), Some(Frame::Button { down: true, .. })),
            "and the click comes after it: {frames:?}"
        );
        let mut seen = counters(&frames);
        assert_eq!(seen, vec![2, 3], "the counter never repeats: {seen:?}");
        seen.dedup();
        assert_eq!(seen.len(), 2);
    }

    /// The trailing-edge flush delivers the final position after the flow stops:
    /// the pointer must not come to rest one event short of where the hand put it.
    #[tokio::test]
    async fn the_final_position_is_delivered_after_the_flow_stops() {
        let mut h = Harness::new();
        let peer = h.desk().await;
        h.driving(&peer).await;
        let _ = h.frames(&peer);
        for _ in 0..2 {
            let event = h.motion(0, 0, 1, 0);
            h.engine.on_backend(event, h.t0).await;
        }
        assert_eq!(positions(&h.frames(&peer)).len(), 1);
        // Nothing else arrives; the deadline fires one token later, and it is the
        // soonest thing the engine is waiting for.
        let deadline = h.engine.next_deadline(h.t0).expect("a flush is armed");
        assert!(
            deadline > h.t0 && deadline <= h.t0 + Duration::from_millis(10),
            "the flush is armed within one token of the last position"
        );
        h.engine.pump(deadline);
        assert_eq!(
            positions(&h.frames(&peer)),
            vec![(962, 540)],
            "the last position is delivered when the flow stops"
        );
        assert!(
            h.engine
                .next_deadline(deadline)
                .is_none_or(|next| next > deadline),
            "and the flush is not re-armed with nothing pending"
        );
    }

    /// The ceiling is a measurement, not a taste: 250 Hz under 10 ms of round
    /// trip, 125 Hz above it (#123).
    #[tokio::test]
    async fn the_rate_ceiling_halves_above_ten_milliseconds() {
        // Five milliseconds buys a token at 250 Hz (one every 4 ms) and does not
        // at 125 Hz (one every 8 ms). The burst allowance is spent first, so the
        // measurement is about the rate and not about the start.
        let gap = Duration::from_millis(5);
        let mut fast = Harness::new();
        let peer = fast.desk().await;
        fast.driving(&peer).await;
        let after = fast.spend_the_burst(&peer, fast.t0).await;
        let event = fast.motion(0, 0, 1, 0);
        fast.engine.on_backend(event, after + gap).await;
        assert_eq!(
            positions(&fast.frames(&peer)).len(),
            1,
            "under ten milliseconds a token arrives inside five"
        );

        let mut slow = Harness::new();
        let peer = slow.desk().await;
        slow.driving(&peer).await;
        // A round trip of twenty milliseconds, measured the way the dialect
        // measures it: our own stamp, echoed back untouched, so no clock
        // synchronisation is involved.
        let at = slow.t0 + Duration::from_millis(20);
        slow.feed(&peer, vec![Frame::Pong { ms: 0 }], at).await;
        assert_eq!(
            slow.engine.status()["devices"][0]["rtt_ms"],
            json!(20),
            "the measured round trip is in the snapshot"
        );
        let after = slow.spend_the_burst(&peer, at).await;
        let event = slow.motion(0, 0, 1, 0);
        slow.engine.on_backend(event, after + gap).await;
        assert!(
            positions(&slow.frames(&peer)).is_empty(),
            "above ten milliseconds the position waits for its token"
        );
    }

    /// A relative move of (0, 0) cannot be built, which is where "never emitted"
    /// lives: Windows discards one and it reaches no hook at all (#123). And the
    /// absolute path never produces a relative frame at all.
    #[tokio::test]
    async fn a_relative_move_of_nothing_is_not_a_move() {
        assert_eq!(Flow::relative(0, 0), None);
        assert_eq!(Flow::relative(1, 0), Some(Flow::By(1, 0)));
        assert_eq!(Flow::relative(0, -1), Some(Flow::By(0, -1)));

        let mut h = Harness::new();
        let peer = h.desk().await;
        h.driving(&peer).await;
        let _ = h.frames(&peer);
        for dx in [1, 0, 2] {
            let event = h.motion(0, 0, dx, 0);
            h.engine.on_backend(event, h.t0).await;
        }
        h.engine.pump(h.t0 + Duration::from_millis(50));
        let frames = h.frames(&peer);
        assert!(
            !frames.iter().any(|f| matches!(f, Frame::Motion { .. })),
            "v1 integrates deltas and sends absolute positions: {frames:?}"
        );
    }

    // ------------------------------------------------------- the target's side

    /// The read-side coalescing rule (D6): drop a position whose immediate
    /// successor in the same batch is also a position, apply everything else in
    /// arrival order, and never apply a position whose counter is stale.
    #[tokio::test]
    async fn a_batch_drops_only_a_superseded_position() {
        let mut h = Harness::new();
        let peer = h.desk().await;
        h.fake.teach_symbol("a", 65, 0);
        h.driven(&peer).await;
        h.fake.forget();

        let batch = vec![
            Frame::Pointer {
                session: 1,
                n: 1,
                x: 11,
                y: 11,
            },
            Frame::Key {
                session: 1,
                n: 2,
                usage: 0,
                key: None,
                sym: Some("a".into()),
                mods: 0,
                layout: "us".into(),
                down: true,
                lock: false,
            },
            Frame::Pointer {
                session: 1,
                n: 3,
                x: 22,
                y: 22,
            },
            Frame::Pointer {
                session: 1,
                n: 4,
                x: 33,
                y: 33,
            },
            Frame::Button {
                session: 1,
                n: 5,
                button: 1,
                down: true,
            },
        ];
        h.feed(&peer, batch, h.t0).await;
        let actions = h.calls().actions;
        let moves: Vec<Point> = actions
            .iter()
            .filter_map(|a| match a {
                Action::MoveTo(at) => Some(*at),
                _ => None,
            })
            .collect();
        assert_eq!(
            moves,
            vec![Point { x: 11, y: 11 }, Point { x: 33, y: 33 }],
            "the middle position is dropped and only it: {actions:?}"
        );
        // In arrival order: the key is applied at the position it was sent from,
        // and the click at the one it was sent from.
        let key_at = actions
            .iter()
            .position(|a| matches!(a, Action::Key { .. }))
            .expect("the key was applied");
        let last_move = actions
            .iter()
            .rposition(|a| matches!(a, Action::MoveTo(_)))
            .expect("a move");
        let click = actions
            .iter()
            .position(|a| matches!(a, Action::Button { .. }))
            .expect("the click was applied");
        assert!(key_at < last_move && last_move < click, "{actions:?}");

        // A position from the past is dropped: no reordering, no replay and no
        // future unreliable transport walks the pointer backwards.
        h.fake.forget();
        h.feed(
            &peer,
            vec![Frame::Pointer {
                session: 1,
                n: 2,
                x: 44,
                y: 44,
            }],
            h.t0,
        )
        .await;
        assert!(
            h.calls().actions.is_empty(),
            "a stale counter is not a position: {:?}",
            h.calls().actions
        );
    }

    // ---------------------------------------------------------- the guards

    /// The two guards that are TIME, and they are the engine's own half of the
    /// chain (doc/input-sharing.md, section 7): the dwell does not fire early, it
    /// fires at the boundary, and it starts again when the segment changes.
    #[tokio::test]
    async fn the_dwell_fires_at_its_boundary_and_resets_when_the_segment_changes() {
        let mut h = Harness::new();
        let peer = h.desk().await;
        h.serve(
            "input.drive",
            json!({ "device_id": "d_b", "allowed": true }),
        )
        .expect("drive");
        h.warm(&peer, true).await;
        h.settle();
        let _ = h.frames(&peer);
        assert_eq!(
            h.calls().capture.last(),
            Some(&CaptureMode::Watch),
            "a warm adjacent peer is what watching is for"
        );

        // Against the edge: the OS clamps the pointer at 1919, and the graph is
        // asked about 1919 PLUS the delta, which does go past it.
        let event = h.motion(1919, 540, 1, 0);
        h.engine.on_backend(event, h.t0).await;
        let dwell = Duration::from_millis(u64::from(crate::graph::DWELL_MS));
        h.engine.pump(h.t0 + dwell - Duration::from_millis(1));
        assert!(
            !h.frames(&peer)
                .iter()
                .any(|f| matches!(f, Frame::Start { .. })),
            "a dwell one millisecond short is not a crossing"
        );
        h.engine.pump(h.t0 + dwell);
        assert!(
            h.frames(&peer)
                .iter()
                .any(|f| matches!(f, Frame::Start { .. })),
            "and at its boundary it is"
        );

        // Now the reset. A fresh engine, a touch, a departure, and a touch again:
        // the second dwell is measured from the second touch.
        let mut h = Harness::new();
        let peer = h.desk().await;
        h.serve(
            "input.drive",
            json!({ "device_id": "d_b", "allowed": true }),
        )
        .expect("drive");
        h.warm(&peer, true).await;
        h.settle();
        let _ = h.frames(&peer);
        let event = h.motion(1919, 540, 1, 0);
        h.engine.on_backend(event, h.t0).await;
        // Away from the edge, well inside our own screen.
        let event = h.motion(960, 540, 0, 0);
        h.engine
            .on_backend(event, h.t0 + Duration::from_millis(100))
            .await;
        let event = h.motion(1919, 540, 1, 0);
        h.engine
            .on_backend(event, h.t0 + Duration::from_millis(200))
            .await;
        h.engine.pump(h.t0 + dwell + Duration::from_millis(100));
        assert!(
            !h.frames(&peer)
                .iter()
                .any(|f| matches!(f, Frame::Start { .. })),
            "leaving the segment starts the dwell over"
        );
        h.engine.pump(h.t0 + dwell + Duration::from_millis(200));
        assert!(
            h.frames(&peer)
                .iter()
                .any(|f| matches!(f, Frame::Start { .. })),
            "and it fires a dwell after the second touch"
        );
    }

    /// The double tap: the pointer must have touched the segment, left it, and
    /// come back inside its window before the dwell counts for anything.
    #[tokio::test]
    async fn the_double_tap_needs_the_second_touch_inside_its_window() {
        let mut h = Harness::new();
        let peer = h.desk().await;
        h.serve(
            "input.drive",
            json!({ "device_id": "d_b", "allowed": true }),
        )
        .expect("drive");
        h.serve(
            "input.guards",
            json!({ "device_id": "d_b", "monitor": plane::spot_key(&peer, "B"),
                    "side": "right", "guards": { "double_tap_ms": 300 } }),
        )
        .expect("guards");
        h.warm(&peer, true).await;
        h.settle();
        let _ = h.frames(&peer);
        let ms = Duration::from_millis;

        // First touch: nothing was left and re-reached, so the dwell counts for
        // nothing however long it lasts.
        let event = h.motion(1919, 540, 1, 0);
        h.engine.on_backend(event, h.t0).await;
        h.engine.pump(h.t0 + ms(1000));
        assert!(
            !h.frames(&peer)
                .iter()
                .any(|f| matches!(f, Frame::Start { .. })),
            "one touch is not a double tap"
        );

        // Leave, and come back too late: still nothing.
        let event = h.motion(960, 540, 0, 0);
        h.engine.on_backend(event, h.t0 + ms(1000)).await;
        let event = h.motion(1919, 540, 1, 0);
        h.engine.on_backend(event, h.t0 + ms(1400)).await;
        h.engine.pump(h.t0 + ms(2000));
        assert!(
            !h.frames(&peer)
                .iter()
                .any(|f| matches!(f, Frame::Start { .. })),
            "a second touch outside the window is not a double tap either"
        );

        // Leave, and come back inside the window: now the dwell counts.
        let event = h.motion(960, 540, 0, 0);
        h.engine.on_backend(event, h.t0 + ms(2000)).await;
        let event = h.motion(1919, 540, 1, 0);
        h.engine.on_backend(event, h.t0 + ms(2100)).await;
        h.engine.pump(h.t0 + ms(2100) + ms(250));
        assert!(
            h.frames(&peer)
                .iter()
                .any(|f| matches!(f, Frame::Start { .. })),
            "a second touch inside the window earns its crossing"
        );
    }

    // -------------------------------------------------------- the exclusion

    /// At most one `Driven` across all peers, the holder is named, and there is
    /// no preemption: `BUSY` is `BUSY` (D16).
    #[tokio::test]
    async fn a_second_source_is_refused_busy_and_the_holder_keeps_it() {
        let mut h = Harness::new();
        let first = h.desk().await;
        let second = node(0xc);
        h.engine.set_directory(
            directory(&[("d_b", &first, true), ("d_c", &second, true)]),
            h.t0,
        );
        h.serve(
            "input.allow",
            json!({ "device_id": "d_c", "allowed": true }),
        )
        .expect("allow");
        h.driven(&first).await;
        h.warm(&second, true).await;
        let plane = h.engine.plane_id().to_string();
        h.feed(
            &second,
            vec![Frame::Start {
                session: 1,
                mode: Mode::Full,
                keys: KeyMode::Typing,
                plane,
                n: 1,
                x: 0,
                y: 0,
            }],
            h.t0,
        )
        .await;
        let refusal = h
            .frames(&second)
            .into_iter()
            .find_map(|f| match f {
                Frame::Refused { code, by, .. } => Some((code, by)),
                _ => None,
            })
            .expect("a refusal came back");
        assert_eq!(refusal.0, refused::BUSY);
        assert_eq!(
            refusal.1.as_deref(),
            Some("d_b"),
            "the holder is named, so the interface can say who has it"
        );
        assert_eq!(
            h.engine.status()["session"]["device_id"],
            json!("d_b"),
            "and the holder keeps it: no preemption, ever"
        );
    }

    /// Driving and being driven are mutually exclusive on one machine (D15),
    /// which is what makes echo suppression a non-problem in v1.
    #[tokio::test]
    async fn a_machine_that_drives_refuses_to_be_driven_and_the_other_way_round() {
        // Driving, then asked to be driven.
        let mut h = Harness::new();
        let peer = h.desk().await;
        h.serve(
            "input.allow",
            json!({ "device_id": "d_b", "allowed": true }),
        )
        .expect("allow");
        h.driving(&peer).await;
        let plane = h.engine.plane_id().to_string();
        let _ = h.frames(&peer);
        h.feed(
            &peer,
            vec![Frame::Start {
                session: 7,
                mode: Mode::Full,
                keys: KeyMode::Typing,
                plane,
                n: 1,
                x: 0,
                y: 0,
            }],
            h.t0,
        )
        .await;
        let refusal = h
            .frames(&peer)
            .into_iter()
            .find_map(|f| match f {
                Frame::Refused { code, by, .. } => Some((code, by)),
                _ => None,
            })
            .expect("a refusal came back");
        assert_eq!(refusal.0, refused::BUSY);
        assert_eq!(
            refusal.1.as_deref(),
            Some("d_self"),
            "this machine is the one holding its own keyboard"
        );
        assert_eq!(
            h.engine.status()["session"]["direction"],
            json!("out"),
            "and the outbound session is untouched"
        );

        // Driven, then asked to drive.
        let mut h = Harness::new();
        let peer = h.desk().await;
        h.serve(
            "input.drive",
            json!({ "device_id": "d_b", "allowed": true }),
        )
        .expect("drive");
        h.driven(&peer).await;
        assert_eq!(
            h.serve("input.take", json!({ "device_id": "d_b" })),
            Err("INPUT_BUSY".into()),
            "a machine being driven does not also drive"
        );
        // And the edge does not cross either, however long the pointer rests on
        // it: a driven machine is not even capturing (D15).
        let event = h.motion(1919, 540, 1, 0);
        h.engine.on_backend(event, h.t0).await;
        h.engine.pump(h.t0 + Duration::from_millis(1000));
        assert!(
            !h.frames(&peer)
                .iter()
                .any(|f| matches!(f, Frame::Start { .. })),
            "and no crossing fires while it is being driven"
        );
    }

    /// The plane check, which is what makes absolute coordinates safe (D7), and
    /// the one refusal that repairs itself.
    #[tokio::test]
    async fn a_start_on_another_plane_is_refused_and_starts_a_layout_round() {
        let mut h = Harness::new();
        let peer = h.desk().await;
        h.serve(
            "input.allow",
            json!({ "device_id": "d_b", "allowed": true }),
        )
        .expect("allow");
        h.warm(&peer, true).await;
        h.settle();
        let _ = h.frames(&peer);
        h.feed(
            &peer,
            vec![Frame::Start {
                session: 1,
                mode: Mode::Full,
                keys: KeyMode::Typing,
                plane: "0".repeat(32),
                n: 1,
                x: 0,
                y: 0,
            }],
            h.t0,
        )
        .await;
        let seen = h.frames(&peer);
        assert!(
            seen.iter()
                .any(|f| matches!(f, Frame::Refused { code, .. } if code == refused::PLANE_STALE)),
            "coordinates on another plane mean something else: {seen:?}"
        );
        assert_eq!(h.engine.status()["session"], Value::Null);
        h.engine.pump(h.t0 + LAYOUT_MIN_GAP);
        assert!(
            h.engine
                .take_effects()
                .iter()
                .any(|e| matches!(e, Effect::Send { .. })),
            "and a layout round repairs it"
        );
    }

    /// The grant is the security boundary, the driving side learns by TRYING, and
    /// nothing in the handshake hinted at it (D14).
    #[tokio::test]
    async fn a_start_without_a_grant_is_refused_and_the_handshake_never_hinted() {
        let mut h = Harness::new();
        let peer = h.desk().await;
        h.warm(&peer, true).await;
        let hi = h
            .frames(&peer)
            .into_iter()
            .find_map(|f| match f {
                Frame::Hi { caps, .. } => Some(caps),
                _ => None,
            })
            .expect("our own handshake went out first");
        let mut said: Vec<&String> = hi.as_object().expect("an object").keys().collect();
        said.sort();
        assert_eq!(
            said,
            vec![
                "capture",
                "confine",
                "inject_keys",
                "inject_pointer",
                "monitors_stable",
                "swallow",
                "unicode",
                "warp",
            ],
            "the handshake says what this machine CAN do, and nothing about who \
             may do it here"
        );

        let plane = h.engine.plane_id().to_string();
        h.feed(
            &peer,
            vec![Frame::Start {
                session: 1,
                mode: Mode::Full,
                keys: KeyMode::Typing,
                plane,
                n: 1,
                x: 0,
                y: 0,
            }],
            h.t0,
        )
        .await;
        assert!(
            h.frames(&peer)
                .iter()
                .any(|f| matches!(f, Frame::Refused { code, .. } if code == refused::NOT_ALLOWED)),
            "a device nobody allowed cannot type here"
        );
        assert_eq!(h.engine.status()["session"], Value::Null);
        assert!(
            h.calls().actions.is_empty(),
            "and nothing at all was injected: {:?}",
            h.calls().actions
        );
    }

    // ------------------------------------------------------- the ten deaths

    /// The hygiene matrix, table driven over EVERY reason a `peers.channel` can
    /// die, on the TARGET side (doc/input-sharing.md, section 4).
    ///
    /// The assertions go through the fake's recorded calls and never through the
    /// engine's own bookkeeping, because the engine's view of its held set is
    /// exactly what a hygiene test must not trust. A reason cannot join the
    /// vocabulary without joining this table.
    #[tokio::test]
    async fn every_channel_death_releases_what_the_target_was_holding() {
        let control = keys::usage(keys::PAGE_KEYBOARD, 0xE0);
        for reason in DEATHS {
            let mut h = Harness::new();
            // Taught BEFORE the engine starts: `ModKeys::learn` asks for every
            // modifier key at start, and the resolver caches a NEGATIVE answer
            // too (which is the half that matters on a hot path).
            h.fake.teach_usage(control, 17);
            let peer = h.desk().await;
            h.driven(&peer).await;
            // The source holds Control: a positional stroke, so it stays down
            // across frames until the frame that releases it.
            h.feed(
                &peer,
                vec![Frame::Key {
                    session: 1,
                    n: 2,
                    usage: control,
                    key: None,
                    sym: None,
                    mods: keys::mods::CTRL,
                    layout: "us".into(),
                    down: true,
                    lock: false,
                }],
                h.t0,
            )
            .await;
            assert_eq!(
                h.fake.keys_down(),
                vec![PlatformKey {
                    code: 17,
                    detail: 0
                }],
                "{reason}: the modifier must be down before the death"
            );
            assert_eq!(
                h.held_file()["keys"][0]["code"],
                json!(17),
                "{reason}: and on disk, because an injected key outlives its injector"
            );

            // A channel does not die in the instant it attached, and a closure
            // that arrives inside [`CLOSURE_GRACE`] is one the Core wrote about a
            // pipe we no longer hold (see the constant).
            h.engine
                .on_channel_closed(&peer, reason, h.t0 + CLOSURE_GRACE);

            assert!(
                h.fake.keys_down().is_empty(),
                "{reason}: every key we pressed is released"
            );
            let calls = h.calls();
            assert_eq!(
                calls.releases.last(),
                Some(&vec![PlatformKey {
                    code: 17,
                    detail: 0
                }]),
                "{reason}: released through the hygiene path, exactly what was down"
            );
            assert!(
                matches!(
                    calls.capture.last(),
                    Some(CaptureMode::Off) | Some(CaptureMode::Watch)
                ),
                "{reason}: the capture mode is back to a resting value: {:?}",
                calls.capture
            );
            assert_eq!(
                h.held_file(),
                json!({ "keys": [] }),
                "{reason}: and the crash guard is emptied, by a write and not a delete"
            );
            assert_eq!(
                h.engine.status()["session"],
                Value::Null,
                "{reason}: the session is over"
            );
            // The three reasons that do more than end a session, and the one that
            // deliberately does less.
            let allowed = h.engine.status()["devices"][0]["allowed"].clone();
            match reason {
                "DEVICE_REVOKED" | "ACCOUNT_LEFT" => assert_eq!(
                    allowed,
                    json!(false),
                    "{reason}: a grant dies with its device, and every grant dies \
                     when this device leaves the account"
                ),
                "LOGGED_OUT" => assert_eq!(
                    allowed,
                    json!(true),
                    "a logout re-keys the device_id and the grants are keyed by \
                     node_id: they SURVIVE"
                ),
                _ => assert_eq!(allowed, json!(true), "{reason}: nothing else is a grant"),
            }
        }
    }

    /// The same table on the SOURCE side: unpinned, no longer swallowing, and the
    /// pointer back where it left from, whatever ended the channel.
    #[tokio::test]
    async fn every_channel_death_unpins_the_source_and_brings_the_pointer_home() {
        for reason in DEATHS {
            let mut h = Harness::new();
            let peer = h.desk().await;
            h.driving(&peer).await;
            assert_eq!(
                h.calls().capture.last(),
                Some(&CaptureMode::Swallow),
                "{reason}: driving swallows"
            );
            assert!(
                matches!(h.calls().confine.last(), Some(Some(_))),
                "{reason}: and pins the pointer"
            );
            h.fake.forget();

            h.engine
                .on_channel_closed(&peer, reason, h.t0 + CLOSURE_GRACE);

            let calls = h.calls();
            assert_eq!(
                calls.confine.last(),
                Some(&None),
                "{reason}: the confinement is released, or the mouse is stuck in a corner"
            );
            assert_eq!(
                calls.warps.last(),
                Some(&Point { x: 960, y: 540 }),
                "{reason}: the pointer goes back where it left from"
            );
            assert!(
                matches!(
                    calls.capture.last(),
                    Some(CaptureMode::Off) | Some(CaptureMode::Watch)
                ),
                "{reason}: a swallow that outlives its session is a dead keyboard: {:?}",
                calls.capture
            );
            assert_eq!(
                h.engine.status()["session"],
                Value::Null,
                "{reason}: the session is over"
            );
        }
    }

    /// The pipe's own end and the Core's reason can arrive in either order, and
    /// the teardown happens on whichever comes first: hygiene before wording.
    #[tokio::test]
    async fn either_half_of_a_death_can_arrive_first() {
        for pipe_first in [true, false] {
            let mut h = Harness::new();
            let peer = h.desk().await;
            h.driving(&peer).await;
            h.fake.forget();
            let later = h.t0 + CLOSURE_GRACE;
            if pipe_first {
                h.engine.on_channel_ended(&peer, later);
                assert_eq!(
                    h.calls().confine.last(),
                    Some(&None),
                    "the pipe ending is enough to bring the keyboard home"
                );
                h.engine.on_channel_closed(&peer, "PEER_GONE", later);
            } else {
                h.engine.on_channel_closed(&peer, "PEER_GONE", later);
                assert_eq!(h.calls().confine.last(), Some(&None));
                h.engine.on_channel_ended(&peer, later);
            }
            assert_eq!(
                h.engine.status()["session"],
                Value::Null,
                "and the second half changes nothing"
            );
        }
    }

    // -------------------------------------------------------- the crash guard

    /// A backend that reads `held.json` at the instant a batch is handed to it,
    /// which is the only way to prove the ORDER the crash guard depends on.
    #[derive(Clone)]
    struct Spy {
        inner: FakeBackend,
        root: PathBuf,
        seen: Arc<Mutex<Vec<Value>>>,
    }

    impl InputBackend for Spy {
        fn capabilities(&self) -> Capabilities {
            self.inner.capabilities()
        }

        async fn monitors(&self) -> Vec<Monitor> {
            self.inner.monitors().await
        }

        async fn pointer(&self) -> Option<Point> {
            self.inner.pointer().await
        }

        async fn resolve(&self, want: Want) -> Option<Resolved> {
            self.inner.resolve(want).await
        }

        fn capture(&self, mode: CaptureMode) {
            self.inner.capture(mode);
        }

        fn confine(&self, rect: Option<Rect>) {
            self.inner.confine(rect);
        }

        fn warp(&self, to: Point) {
            self.inner.warp(to);
        }

        fn inject(&self, actions: Vec<Action>) {
            let text = std::fs::read_to_string(self.root.join(HELD_FILE)).unwrap_or_default();
            self.seen
                .lock()
                .expect("lock")
                .push(serde_json::from_str(&text).unwrap_or(Value::Null));
            self.inner.inject(actions);
        }

        fn release_all(&self, keys: Vec<PlatformKey>) {
            self.inner.release_all(keys);
        }

        fn request_exit(&self, code: i32) {
            self.inner.request_exit(code);
        }
    }

    /// The crash guard's whole point: the set at its WIDEST point during the
    /// sequence reaches `held.json` BEFORE the batch is handed over.
    ///
    /// The `@` stroke is the case that proves it: it holds AltGr in the middle and
    /// holds none of it at the end, so a process death between the two would
    /// strand AltGr on a machine whose file never mentioned it.
    #[tokio::test]
    async fn the_peak_of_a_stroke_is_on_disk_before_the_batch_is_injected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("input");
        let store = Store::open(root.clone()).expect("open");
        let (fake, _events) = FakeBackend::new();
        let altgr = keys::mod_usage(keys::mods::ALTGR).expect("AltGr has a key");
        fake.teach_usage(altgr, 165);
        fake.teach_symbol("@", 50, keys::mods::ALTGR);
        let spy = Spy {
            inner: fake.clone(),
            root: root.clone(),
            seen: Arc::new(Mutex::new(Vec::new())),
        };
        let t0 = Instant::now();
        let mut engine = Engine::open(spy.clone(), store, t0).expect("open");
        engine.start(t0).await;
        let peer = node(0xb);
        engine.set_directory(directory(&[("d_b", &peer, true)]), t0);
        engine
            .serve(
                "input.allow",
                &json!({ "device_id": "d_b", "allowed": true }),
                t0,
            )
            .expect("allow");
        let (tx, _rx) = mpsc::channel(64);
        engine.attach(&peer, tx, false, t0);
        let plane = engine.plane_id().to_string();
        let hi = wire::encode(&Frame::Hi {
            version: wire::VERSION,
            caps: json!({ "inject_keys": true }),
            plane: plane.clone(),
        })
        .expect("encode");
        engine.on_frames(&peer, vec![hi], t0).await;
        let start = wire::encode(&Frame::Start {
            session: 1,
            mode: Mode::Keys,
            keys: KeyMode::Typing,
            plane,
            n: 1,
            x: 0,
            y: 0,
        })
        .expect("encode");
        engine.on_frames(&peer, vec![start], t0).await;
        spy.seen.lock().expect("lock").clear();

        // An at sign, from a source holding nothing: AltGr down, the key struck,
        // AltGr back up, all inside one batch.
        let at_sign = wire::encode(&Frame::Key {
            session: 1,
            n: 2,
            usage: 0,
            key: None,
            sym: Some("@".into()),
            mods: 0,
            layout: "us".into(),
            down: true,
            lock: false,
        })
        .expect("encode");
        engine.on_frames(&peer, vec![at_sign], t0).await;

        let seen = spy.seen.lock().expect("lock").clone();
        assert_eq!(seen.len(), 1, "one batch: {seen:?}");
        assert_eq!(
            seen[0]["keys"][0]["code"],
            json!(165),
            "the PEAK is on disk before the batch: {seen:?}"
        );
        assert_eq!(
            seen[0]["keys"][0]["mod"],
            json!(keys::mods::ALTGR),
            "and it says which modifier it was"
        );
        // And the stroke is atomic, so the file ends up empty again: nothing is
        // left claimed that is not held.
        let after = std::fs::read_to_string(root.join(HELD_FILE)).expect("read");
        assert_eq!(
            serde_json::from_str::<Value>(&after).expect("json"),
            json!({ "keys": [] }),
            "a symbol stroke holds nothing at the end"
        );
        assert!(
            fake.keys_down().is_empty(),
            "and nothing is really down either: {:?}",
            fake.calls().actions
        );
    }

    /// A `held.json` found at startup is released at once: that is what makes the
    /// guard a guard rather than a diary.
    #[test]
    fn a_held_set_found_at_startup_is_released() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("input");
        let store = Store::open(root.clone()).expect("open");
        store
            .save_held(&json!({ "keys": [
                { "code": 16, "detail": 0, "mod": keys::mods::SHIFT },
                { "code": 17, "detail": 0, "mod": keys::mods::CTRL },
            ] }))
            .expect("save");
        drop(store);

        let store = Store::open(root.clone()).expect("reopen");
        let (fake, _events) = FakeBackend::new();
        let engine = Engine::open(fake.clone(), store, Instant::now()).expect("open");
        assert_eq!(
            fake.calls().releases,
            vec![vec![
                // Reverse press order: releasing Shift before the key it modified
                // can make that key arrive unmodified.
                PlatformKey {
                    code: 17,
                    detail: 0
                },
                PlatformKey {
                    code: 16,
                    detail: 0
                },
            ]],
            "what a previous run left down is released, newest first"
        );
        assert_eq!(
            std::fs::read_to_string(root.join(HELD_FILE))
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok()),
            Some(json!({ "keys": [] })),
            "and the file is emptied by a write, so a reader can tell nothing is held"
        );
        drop(engine);
    }

    /// The same record found by a machine that CANNOT type yet, which is the case the
    /// guard was quietly failing.
    ///
    /// The grant a macOS asks for can be missing at exactly this moment: the update that
    /// restarted this component is what reset it. The release then reaches a backend that
    /// cannot post it, and deleting the record there loses the only thing in the system that
    /// knows a key is down. So the record is kept, and the event that says the grant arrived is
    /// what drains it.
    #[tokio::test]
    async fn a_held_set_found_when_this_machine_cannot_type_is_kept_until_it_can() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("input");
        let store = Store::open(root.clone()).expect("open");
        let record = json!({ "keys": [
            { "code": 17, "detail": 0, "mod": keys::mods::CTRL },
        ] });
        store.save_held(&record).expect("save");
        drop(store);

        let store = Store::open(root.clone()).expect("reopen");
        let (fake, _events) = FakeBackend::new();
        fake.refused();
        let mut engine = Engine::open(fake.clone(), store, Instant::now()).expect("open");
        let on_disk = || {
            std::fs::read_to_string(root.join(HELD_FILE))
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        };
        assert_eq!(
            on_disk(),
            Some(record),
            "the record survives a start that could not have released it"
        );

        // The grant lands.
        let (full, _rx) = FakeBackend::new();
        fake.set_capabilities(full.capabilities());
        fake.forget();
        engine
            .on_backend(BackendEvent::CapabilitiesChanged, Instant::now())
            .await;

        assert_eq!(
            fake.calls().releases,
            vec![vec![PlatformKey {
                code: 17,
                detail: 0
            }]],
            "and the key is released the moment this machine can type"
        );
        assert_eq!(
            on_disk(),
            Some(json!({ "keys": [] })),
            "and only then is the record emptied"
        );
    }

    /// The kept record is not the same thing as a key this engine is HOLDING, and the
    /// difference is not academic: it was driven end to end in review.
    ///
    /// Keeping it in `held` made the injection planner believe a modifier was already down, so
    /// an `A` came out as `a` (the Shift press was skipped as redundant) and the NEXT keystroke
    /// pressed that phantom Shift for real, on a machine nobody had touched. It lives in its
    /// own field now, which nothing but the drain reads.
    ///
    /// The second half of the test is the drain's own trap: it used to hang off the
    /// `CapabilitiesChanged` arm alone, so whichever OTHER capability re-read ran first
    /// consumed the moment the grant arrived and the record was stranded for the life of the
    /// process. `start()` is one of those re-reads, and here it is the only thing that runs.
    #[tokio::test]
    async fn a_kept_record_is_never_mistaken_for_a_key_this_engine_is_holding() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("input");
        let store = Store::open(root.clone()).expect("open");
        let record = json!({ "keys": [
            { "code": 16, "detail": 0, "mod": keys::mods::SHIFT },
        ] });
        store.save_held(&record).expect("save");
        drop(store);

        let store = Store::open(root.clone()).expect("reopen");
        let (fake, _events) = FakeBackend::new();
        fake.refused();
        let mut engine = Engine::open(fake.clone(), store, Instant::now()).expect("open");
        assert!(
            engine.held.is_empty(),
            "what a previous run left down is not something THIS engine holds"
        );
        assert_eq!(
            engine.stranded.len(),
            1,
            "it is stranded, and named as that"
        );

        // The grant lands, and the ONLY thing that runs is `start`, which is not the arm the
        // drain used to live in.
        let (full, _rx) = FakeBackend::new();
        fake.set_capabilities(full.capabilities());
        fake.forget();
        engine.start(Instant::now()).await;

        assert_eq!(
            fake.calls().releases,
            vec![vec![PlatformKey {
                code: 16,
                detail: 0
            }]],
            "the release is retried by whichever capability re-read comes first"
        );
        assert!(engine.stranded.is_empty(), "and the record is done with");
        assert_eq!(
            std::fs::read_to_string(root.join(HELD_FILE))
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok()),
            Some(json!({ "keys": [] })),
            "on disk too, and only once it could have landed"
        );
    }

    // ------------------------------------------------------- the return hotkey

    /// The hotkey is recognised in the captured stream, swallowed there, never
    /// forwarded and never negotiated: it works when the channel is DEAD, because
    /// nothing about it involves the channel.
    #[tokio::test]
    async fn the_return_hotkey_brings_the_keyboard_home_with_a_dead_channel() {
        let mut h = Harness::new();
        let peer = h.desk().await;
        h.driving(&peer).await;
        // The pipe is gone: every write from here on fails.
        h.out.remove(&peer);
        h.fake.forget();

        let event = BackendEvent::Key(KeyEvent {
            usage: keys::usage_of("Escape").expect("Escape is in the frozen table"),
            key: Some("Escape".into()),
            sym: None,
            mods: keys::mods::CTRL | keys::mods::ALT,
            down: true,
            lock: false,
        });
        h.engine.on_backend(event, h.t0).await;

        let calls = h.calls();
        assert_eq!(
            calls.confine.last(),
            Some(&None),
            "the pointer is unpinned even though nothing could be told"
        );
        assert_eq!(calls.warps.last(), Some(&Point { x: 960, y: 540 }));
        assert!(matches!(
            calls.capture.last(),
            Some(CaptureMode::Off) | Some(CaptureMode::Watch)
        ));
        assert_eq!(h.engine.status()["session"], Value::Null);
        assert!(
            !calls
                .actions
                .iter()
                .any(|a| matches!(a, Action::Key { .. })),
            "and the chord itself was swallowed, never injected: {:?}",
            calls.actions
        );
    }

    // ------------------------------------------------------------- the facade

    /// The snapshot answers BEFORE the directory has named this device, because an
    /// interface that opened first would otherwise have nothing to draw. Only the
    /// gestures answer `INPUT_NOT_READY` (the sync engine's precedent).
    #[tokio::test]
    async fn the_snapshot_answers_before_the_directory_and_the_gestures_do_not() {
        let mut h = Harness::new();
        h.fake.set_monitors(vec![screen("A", 0, 0, 1920, 1080)]);
        h.engine.start(h.t0).await;

        let status = h.engine.status();
        assert_eq!(status["devices"], json!([]), "nobody is known yet");
        assert_eq!(status["session"], Value::Null);
        assert_eq!(status["lock"], json!(false));
        assert_eq!(status["guards"], json!([]), "a default is an absence");
        assert_eq!(
            status["hotkey"],
            json!(["ctrl", "alt", "Escape"]),
            "and the way home is knowable from the first paint"
        );
        assert_eq!(
            status["here"]["monitors"][0]["id"],
            json!("A"),
            "what this machine can say about itself, it says"
        );
        assert_eq!(status["here"]["problem"], Value::Null);
        assert_eq!(status["here"]["can_drive"], json!(true));

        for (method, params) in [
            ("input.place", json!({ "spots": [] })),
            (
                "input.allow",
                json!({ "device_id": "d_b", "allowed": true }),
            ),
            (
                "input.drive",
                json!({ "device_id": "d_b", "allowed": true }),
            ),
            ("input.take", json!({ "device_id": "d_b" })),
            ("input.guards", json!({ "device_id": "d_b" })),
            ("input.remap", json!({ "device_id": "d_b", "map": {} })),
        ] {
            assert_eq!(
                h.serve(method, params),
                Err("INPUT_NOT_READY".into()),
                "{method} needs the directory to name a device at all"
            );
        }
    }

    /// A malformed request is not an application state: it gets the real JSON-RPC
    /// code, which the loop writes from the `param:` prefix. An unknown device is
    /// an application state, and gets one of ours.
    #[tokio::test]
    async fn a_bad_shape_is_told_apart_from_a_bad_state() {
        let mut h = Harness::new();
        let _peer = h.desk().await;
        assert_eq!(
            h.serve("input.allow", json!({ "device_id": "d_b" })),
            Err("param:allowed".into())
        );
        assert_eq!(
            h.serve("input.allow", json!({ "allowed": true })),
            Err("param:device_id".into())
        );
        assert_eq!(
            h.serve("input.hotkey", json!({ "keys": ["ctrl", "nonsense"] })),
            Err("param:keys".into()),
            "a chord this build cannot honour leaves the working one in place"
        );
        assert_eq!(
            h.serve(
                "input.drive",
                json!({ "device_id": "d_b", "allowed": true,
                                          "mode": "telepathy" })
            ),
            Err("param:mode".into())
        );
        assert_eq!(
            h.serve(
                "input.allow",
                json!({ "device_id": "d_nobody", "allowed": true })
            ),
            Err("INPUT_DEVICE_UNKNOWN".into())
        );
        // A spot naming a monitor NO record claims is kept, which is the ghost
        // rule and not laxity: a screen that is away keeps its place, so the
        // snapshot an interface drags carries spots marked `present: false` and it
        // sends the whole set back. Refusing them would make one unplugged screen
        // refuse every future drag on that plane.
        assert_eq!(
            h.serve(
                "input.place",
                json!({ "spots": [{ "monitor": plane::spot_key(&node(0xf), "Z"),
                                    "x": 0, "y": 0 }] })
            ),
            Ok(json!({})),
            "an arrangement may place a screen that is not here right now"
        );
        // The SHAPE is still checked, and a key that is not one is a malformed
        // request rather than an application state.
        assert_eq!(
            h.serve(
                "input.place",
                json!({ "spots": [{ "monitor": "not-a-spot-key", "x": 0, "y": 0 }] })
            ),
            Err("param:spots".into())
        );
        assert_eq!(
            h.serve("input.lock", json!({ "locked": true })),
            Ok(json!({}))
        );
        assert_eq!(
            h.serve("input.take", json!({ "device_id": "d_b" })),
            Err("INPUT_LOCKED".into()),
            "a machine pinned to its own screen neither crosses nor hands over"
        );
        assert_eq!(h.engine.status()["lock"], json!(true));
    }

    /// A machine with no platform backend at all still runs, and every honest
    /// sentence is available from the snapshot alone: the interface can say what
    /// this computer cannot do before anyone tries.
    #[tokio::test]
    async fn a_machine_with_no_backend_still_answers_and_refuses_honestly() {
        let mut h = Harness::new();
        h.fake
            .set_capabilities(Capabilities::none(Some(Problem::NoBackend)));
        let peer = h.desk().await;
        let status = h.engine.status();
        assert_eq!(status["here"]["problem"], json!("no_backend"));
        assert_eq!(status["here"]["can_drive"], json!(false));
        assert_eq!(status["here"]["can_be_driven"], json!(false));

        // The grants are still storable and the plane is still replicated: this
        // machine has real work to do before its platform half exists.
        h.serve(
            "input.allow",
            json!({ "device_id": "d_b", "allowed": true }),
        )
        .expect("allow");
        h.serve(
            "input.drive",
            json!({ "device_id": "d_b", "allowed": true }),
        )
        .expect("drive");
        assert_eq!(h.engine.status()["devices"][0]["allowed"], json!(true));
        h.engine.pump(h.t0);
        let effects = h.engine.take_effects();
        assert!(
            effects.iter().any(|e| matches!(e, Effect::Send { .. })),
            "the layout still replicates: {effects:?}"
        );
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::Open { .. })),
            "and nothing is warmed: we could never drive anyone: {effects:?}"
        );
        assert_eq!(
            h.serve("input.take", json!({ "device_id": "d_b" })),
            Err("INPUT_NO_BACKEND".into())
        );

        // An incoming session is refused with the word that explains it.
        h.warm(&peer, true).await;
        let plane = h.engine.plane_id().to_string();
        h.feed(
            &peer,
            vec![Frame::Start {
                session: 1,
                mode: Mode::Keys,
                keys: KeyMode::Typing,
                plane,
                n: 1,
                x: 0,
                y: 0,
            }],
            h.t0,
        )
        .await;
        assert!(
            h.frames(&peer)
                .iter()
                .any(|f| matches!(f, Frame::Refused { code, .. } if code == refused::NO_BACKEND)),
            "nothing here can type, and it says so"
        );
        assert!(
            h.calls().capture.is_empty() && h.calls().actions.is_empty(),
            "and a backend that can do nothing is never asked to: {:?}",
            h.calls()
        );
    }

    /// A session number is per peer and starts at 1 on every attach, so "the
    /// session I am in" and "the session this frame names" agree by accident all
    /// the time. A frame from the WRONG computer must therefore change nothing:
    /// otherwise one stale `Ended` from a peer whose `Stop` was dropped ends the
    /// session this machine is holding with a DIFFERENT computer, and puts that
    /// other computer's name on the sentence it emits on the way out.
    #[tokio::test]
    async fn a_frame_from_another_computer_cannot_end_this_session() {
        let mut h = Harness::new();
        let peer = h.desk().await;
        let stranger = node(0xc);
        h.engine.set_directory(
            directory(&[("d_b", &peer, true), ("d_c", &stranger, true)]),
            h.t0,
        );
        h.driving(&peer).await;
        assert_ne!(h.engine.status()["session"], Value::Null);

        // The stranger attaches and speaks about "session 1", which is exactly
        // the number the live session with B has.
        h.warm(&stranger, true).await;
        let _ = h.engine.take_effects();
        h.feed(
            &stranger,
            vec![Frame::Ended {
                session: 1,
                code: ended::IDLE.to_string(),
            }],
            h.t0,
        )
        .await;

        assert_ne!(
            h.engine.status()["session"],
            Value::Null,
            "the session with B survives a frame from C"
        );
        let effects = h.engine.take_effects();
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::Emit { method, .. } if method == "input.refused")),
            "and nothing is said about C, which said nothing: {effects:?}"
        );
        // Nor about B, whose row is untouched.
        assert_eq!(h.engine.status()["devices"][0]["problem"], Value::Null);
        assert_eq!(h.engine.status()["devices"][1]["problem"], Value::Null);

        // And the same frame from B really does end it, so the guard is a guard
        // and not a mute.
        let session = 1;
        h.feed(
            &peer,
            vec![Frame::Ended {
                session,
                code: ended::IDLE.to_string(),
            }],
            h.t0,
        )
        .await;
        assert_eq!(h.engine.status()["session"], Value::Null);
    }

    /// The OS grant that arrives LATE, which is the macOS Accessibility case and
    /// the reason the seam has a `CapabilitiesChanged` upcall at all.
    ///
    /// Two things have to happen when it lands, and only one of them is obvious.
    /// The capabilities are re-read, so the interface stops saying this computer
    /// cannot type. And what this machine can PRODUCE is asked all over again,
    /// because the resolver caches negative answers on purpose: a backend asked
    /// before its grant existed is otherwise remembered as unable to produce
    /// anything for the life of the process, and the machine would accept a session
    /// and then type nothing into it.
    #[tokio::test]
    async fn a_grant_that_arrives_late_is_believed_and_everything_is_asked_again() {
        let shift = keys::usage(keys::PAGE_KEYBOARD, 0xE1);
        let mut h = Harness::new();
        h.fake.refused();
        let peer = h.desk().await;
        assert_eq!(h.engine.status()["here"]["problem"], json!("no_permission"));
        assert_eq!(h.engine.status()["here"]["can_be_driven"], json!(false));

        // The grant lands. The backend widens what it claims and, in the same
        // breath, can answer the layout questions it could not answer before.
        let (full, _rx) = FakeBackend::new();
        let caps = full.capabilities();
        h.fake.teach_usage(shift, 16);
        h.fake.teach_symbol("A", 65, keys::mods::SHIFT);
        h.fake.grant_changed(caps).await;
        h.engine
            .on_backend(BackendEvent::CapabilitiesChanged, h.t0)
            .await;

        assert_eq!(h.engine.status()["here"]["problem"], Value::Null);
        assert_eq!(h.engine.status()["here"]["can_be_driven"], json!(true));

        // And the proof that it asked again rather than only re-read the booleans:
        // a session started now can type a symbol that needs the modifier the
        // engine could not resolve a moment ago.
        h.driven(&peer).await;
        h.fake.forget();
        h.feed(
            &peer,
            vec![Frame::Key {
                session: 1,
                n: 2,
                usage: 0,
                key: None,
                sym: Some("A".into()),
                mods: keys::mods::SHIFT,
                layout: "us".into(),
                down: true,
                lock: false,
            }],
            h.t0,
        )
        .await;
        let actions = h.calls().actions;
        assert!(
            actions.contains(&Action::Key {
                code: PlatformKey {
                    code: 16,
                    detail: 0
                },
                down: true
            }),
            "the modifier was re-learned after the grant: {actions:?}"
        );
        assert!(
            actions.contains(&Action::Key {
                code: PlatformKey {
                    code: 65,
                    detail: 0
                },
                down: true
            }),
            "and the symbol resolves now: {actions:?}"
        );
    }

    /// The grant going the OTHER way, mid-session, and this is the dangerous
    /// direction: a permission withdrawn while this machine is DRIVING leaves a
    /// backend that is swallowing its owner's keystrokes and a pointer that is
    /// pinned, for a session that can no longer exist.
    ///
    /// So the two calls that undo those two things are not allowed to depend on the
    /// capability that allowed them: a gate on `caps` there reads as "we cannot
    /// capture, so there is nothing to stop", and the machine keeps the keyboard.
    #[tokio::test]
    async fn a_grant_taken_away_while_driving_stops_the_swallow_and_lifts_the_pin() {
        let mut h = Harness::new();
        let peer = h.desk().await;
        h.driving(&peer).await;
        assert_eq!(
            h.calls().capture.last(),
            Some(&CaptureMode::Swallow),
            "the source is swallowing before the grant goes"
        );
        assert!(
            h.calls().confine.last().is_some_and(Option::is_some),
            "and the pointer is pinned: {:?}",
            h.calls().confine
        );
        h.fake.forget();

        h.fake.refused();
        h.engine
            .on_backend(BackendEvent::CapabilitiesChanged, h.t0)
            .await;

        assert_eq!(
            h.calls().capture,
            vec![CaptureMode::Off],
            "the swallow is lifted even though the capability that allowed it is gone"
        );
        assert_eq!(
            h.calls().confine,
            vec![None],
            "and so is the pin: a pointer stuck in a corner is the other dead keyboard"
        );
        assert!(
            h.frames(&peer)
                .iter()
                .any(|f| matches!(f, Frame::Stop { code, .. } if code == stopped::GONE)),
            "and the peer is told the keyboard went home"
        );
    }

    /// The same withdrawal on the machine being DRIVEN: it can no longer type what
    /// it is sent, so the session ends with the word that explains it rather than
    /// swallowing frames in silence.
    #[tokio::test]
    async fn a_grant_taken_away_while_driven_ends_the_session_with_a_reason() {
        let mut h = Harness::new();
        let peer = h.desk().await;
        h.driven(&peer).await;
        h.fake.forget();

        h.fake.refused();
        h.engine
            .on_backend(BackendEvent::CapabilitiesChanged, h.t0)
            .await;

        assert!(
            h.frames(&peer)
                .iter()
                .any(|f| matches!(f, Frame::Ended { code, .. } if code == ended::NO_BACKEND)),
            "the source is told why its keyboard stopped arriving"
        );
        assert_eq!(
            h.engine.status()["here"]["can_be_driven"],
            json!(false),
            "and the snapshot says what this computer cannot do now"
        );
    }

    /// A `peer.channel_closed` names the PAIR and not the channel, so a closure
    /// about a pipe that has already been replaced must not kill its successor.
    /// The successor's own pipe is what reports a real death.
    #[tokio::test]
    async fn a_closure_about_a_replaced_pipe_does_not_kill_its_successor() {
        let mut h = Harness::new();
        let peer = h.desk().await;
        h.driving(&peer).await;
        // The Core's word about the channel a newer open displaced, arriving after
        // the newer one is already live.
        h.engine.on_channel_closed(&peer, "REPLACED", h.t0);
        assert_ne!(
            h.engine.status()["session"],
            Value::Null,
            "the live channel keeps its session"
        );
        // And when a channel really does die, its own pipe says so, with no reason
        // attached and no grace at all.
        h.engine.on_channel_ended(&peer, h.t0);
        assert_eq!(h.engine.status()["session"], Value::Null);
        assert_eq!(h.calls().confine.last(), Some(&None));
    }

    /// A take on a COLD channel is the ordinary case: a human ticks "may drive"
    /// and presses the button, and the open is asynchronous. The gesture parks the
    /// intent and the handshake is what turns it into a session.
    #[tokio::test]
    async fn a_take_parked_on_a_cold_channel_starts_when_the_channel_warms() {
        let mut h = Harness::new();
        let peer = h.desk().await;
        h.serve(
            "input.drive",
            json!({ "device_id": "d_b", "allowed": true }),
        )
        .expect("drive");
        assert_eq!(
            h.serve("input.take", json!({ "device_id": "d_b" })),
            Ok(json!({})),
            "the gesture returns: it cannot wait for a dial"
        );
        // The channel is what the gesture asked the loop to open.
        h.engine.pump(h.t0);
        assert!(
            h.engine
                .take_effects()
                .iter()
                .any(|e| matches!(e, Effect::Open { .. })),
            "a take on a cold peer opens the channel"
        );
        assert_eq!(h.engine.status()["devices"][0]["state"], json!("warming"));

        // The handshake lands, and the session starts by itself.
        h.warm(&peer, true).await;
        h.engine.pump(h.t0 + Duration::from_millis(200));
        assert!(
            h.frames(&peer)
                .iter()
                .any(|f| matches!(f, Frame::Start { .. })),
            "the parked take becomes a session the moment the channel is warm"
        );
        assert_eq!(h.engine.status()["devices"][0]["state"], json!("driving"));
    }

    /// A take parked long ago must not fire when the channel finally warms: the
    /// hand that asked has moved on, and it is told rather than surprised.
    #[tokio::test]
    async fn a_take_parked_too_long_ago_is_dropped_with_a_word() {
        let mut h = Harness::new();
        let peer = h.desk().await;
        h.serve(
            "input.drive",
            json!({ "device_id": "d_b", "allowed": true }),
        )
        .expect("drive");
        h.serve("input.take", json!({ "device_id": "d_b" }))
            .expect("take");
        h.warm(&peer, true).await;
        let _ = h.engine.take_effects();

        let late = h.t0 + TAKE_PARK_BOUND + Duration::from_millis(1);
        h.engine.pump(late);
        assert!(
            !h.frames(&peer)
                .iter()
                .any(|f| matches!(f, Frame::Start { .. })),
            "a stale intent is not a session"
        );
        let said = h.engine.take_effects();
        assert!(
            said.iter()
                .any(|e| matches!(e, Effect::Emit { method, params }
                if method == "input.refused" && params["code"] == json!(NOT_WARM))),
            "and the interface is told why: {said:?}"
        );
    }

    /// A gesture never answers for the far side: `input.take` returns, and what
    /// came back arrives as that device's `problem` and as an `input.refused`.
    ///
    /// This is the grant doctrine in a test (D14): answering `INPUT_NOT_ALLOWED`
    /// here would mean caching the far side's grant, and a human who has just
    /// allowed it over there would then be refused from a word already false.
    #[tokio::test]
    async fn the_far_sides_word_arrives_as_a_problem_and_never_as_a_gesture_error() {
        for (code, problem) in [
            (refused::NOT_ALLOWED, "not_allowed"),
            (refused::NO_BACKEND, "no_backend"),
            (refused::BUSY, "busy"),
            (refused::LOCKED, "locked"),
            (refused::PLANE_STALE, "plane_stale"),
        ] {
            let mut h = Harness::new();
            let peer = h.desk().await;
            h.serve(
                "input.drive",
                json!({ "device_id": "d_b", "allowed": true }),
            )
            .expect("drive");
            h.warm(&peer, true).await;
            assert_eq!(
                h.serve("input.take", json!({ "device_id": "d_b" })),
                Ok(json!({})),
                "{code}: the gesture returns, whatever the far side will say"
            );
            let session = h
                .frames(&peer)
                .into_iter()
                .find_map(|f| match f {
                    Frame::Start { session, .. } => Some(session),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("{code}: a start went out"));
            let _ = h.engine.take_effects();
            h.feed(
                &peer,
                vec![Frame::Refused {
                    session,
                    code: code.to_string(),
                    by: None,
                }],
                h.t0,
            )
            .await;

            let status = h.engine.status();
            assert_eq!(
                status["devices"][0]["problem"],
                json!(problem),
                "{code}: the snapshot is where the far side's word lives"
            );
            assert_eq!(
                status["devices"][0]["state"],
                json!("refused"),
                "{code}: and the row says so"
            );
            assert_eq!(status["session"], Value::Null, "{code}");
            let said = h.engine.take_effects();
            assert!(
                said.iter()
                    .any(|e| matches!(e, Effect::Emit { method, params }
                    if method == "input.refused" && params["code"] == json!(code))),
                "{code}: and it is announced once, transiently: {said:?}"
            );
            // And a second press still answers with nothing but this machine's own
            // knowledge, so a grant fixed over there takes effect at once.
            assert_eq!(
                h.serve("input.take", json!({ "device_id": "d_b" })),
                Ok(json!({})),
                "{code}: no gesture ever answers from a cached refusal"
            );
        }
    }

    /// A target reached through its XWayland is a pair that HALF works, and the
    /// snapshot says which half before anybody types into the other one.
    ///
    /// This is the sentence #128 left missing: the peer's `caps` already carried its
    /// `xwayland` code and the snapshot read nothing but `can_be_driven()`, so a
    /// person crossed to a machine that types into X11 windows only, typed into a
    /// native Wayland one, and nothing happened with nothing anywhere saying why.
    #[tokio::test]
    async fn a_target_reached_through_xwayland_says_which_windows_will_receive() {
        // Verbatim what the X11 backend reports for a forced XWayland session,
        // measured in #128: every capability true, and the one thing wrong is the
        // half of the screen it cannot reach.
        let xwayland = json!({
            "capture": true, "swallow": true, "confine": true, "warp": true,
            "inject_keys": true, "inject_pointer": true, "unicode": true,
            "monitors_stable": false, "problem": "xwayland",
        });
        let mut h = Harness::new();
        let peer = h.desk().await;
        h.serve(
            "input.drive",
            json!({ "device_id": "d_b", "allowed": true }),
        )
        .expect("drive");
        h.warm_with(&peer, xwayland.clone()).await;

        let status = h.engine.status();
        assert_eq!(
            status["devices"][0]["problem"],
            json!("xwayland"),
            "the far side's own word about itself reaches the snapshot"
        );
        assert_eq!(
            status["devices"][0]["state"],
            json!("ready"),
            "and it is not a refusal: this pair works, for X11 windows"
        );

        // A refusal that has just happened outranks it while it stands, being the
        // thing that just happened to the person, and a fresh handshake brings the
        // standing word back rather than losing it with the transient one.
        h.engine.on_open_failed(&peer, "NO_DIRECT_PATH", h.t0);
        assert_eq!(
            h.engine.status()["devices"][0]["problem"],
            json!("no_path"),
            "what just happened comes first"
        );
        // A repeated handshake on the live channel, which is what clears the
        // remembered half, rather than a fresh `attach`: the point being tested is
        // that the standing word survives the thing that erases the transient one.
        h.warm_with(&peer, xwayland.clone()).await;
        h.feed(
            &peer,
            vec![Frame::Hi {
                version: wire::VERSION,
                caps: xwayland.clone(),
                plane: h.engine.plane_id().to_string(),
            }],
            h.t0,
        )
        .await;
        assert_eq!(
            h.engine.status()["devices"][0]["problem"],
            json!("xwayland"),
            "a standing fact is derived, so clearing a refusal cannot erase it"
        );

        // The flatter word wins when both are true: a machine that cannot type at
        // all is not a machine whose Wayland windows are out of reach.
        h.warm_with(&peer, json!({ "problem": "xwayland" })).await;
        assert_eq!(
            h.engine.status()["devices"][0]["problem"],
            json!("no_backend"),
            "nothing there can type, which is the whole of it"
        );

        // And it blocks nothing. A person told what will work may want it.
        //
        // The assertion is that a session really goes OUT, not that the gesture
        // returned `Ok`: by D21 a gesture answers only from what this machine knows,
        // so `input.take` answers `Ok` for every peer including one that cannot type
        // at all, and asserting that would have proved the pre-existing contract
        // rather than anything about this code.
        h.warm_with(&peer, xwayland).await;
        assert_eq!(
            h.serve("input.take", json!({ "device_id": "d_b" })),
            Ok(json!({}))
        );
        assert!(
            h.frames(&peer)
                .iter()
                .any(|f| matches!(f, Frame::Start { .. })),
            "an honest partial target is offered, never withheld: a start went out"
        );
    }

    /// A peer on a build newer than ours says a word we do not have, and the pair
    /// stays honest: the booleans it sent are still read, and no peer-chosen string
    /// reaches the interface as if it were a code with a sentence.
    #[tokio::test]
    async fn a_problem_code_from_the_future_is_dropped_and_the_pair_still_works() {
        let mut h = Harness::new();
        let peer = h.desk().await;
        h.warm_with(
            &peer,
            json!({ "inject_keys": true, "inject_pointer": true,
                    "problem": "wayland_something_new" }),
        )
        .await;
        let status = h.engine.status();
        assert_eq!(
            status["devices"][0]["problem"],
            Value::Null,
            "an unknown code is not repeated, and the caps say the pair can work"
        );
        assert_eq!(status["devices"][0]["state"], json!("ready"));
    }

    /// A layout change is the one moment the old platform keys are still
    /// describable, so it is where they have to be let go of.
    ///
    /// A `PlatformKey` is a code AND a detail, and a re-resolve after a layout
    /// change can hand back the same code with a different detail: `Held::holds`
    /// would then say false and a held key would stay physically down until the
    /// session ended. The group is adopted in the same breath, because a stroke
    /// that switches group has to be able to switch back to the RIGHT one.
    #[tokio::test]
    async fn a_layout_change_adopts_the_group_and_releases_everything_held() {
        let control = keys::usage(keys::PAGE_KEYBOARD, 0xE0);
        let mut h = Harness::new();
        h.fake.teach_usage(control, 17);
        let peer = h.desk().await;
        h.driven(&peer).await;
        h.feed(
            &peer,
            vec![Frame::Key {
                session: 1,
                n: 2,
                usage: control,
                key: None,
                sym: None,
                mods: keys::mods::CTRL,
                layout: "us".into(),
                down: true,
                lock: false,
            }],
            h.t0,
        )
        .await;
        assert_eq!(
            h.fake.keys_down(),
            vec![PlatformKey {
                code: 17,
                detail: 0
            }],
            "the modifier is down before the layout moves"
        );

        h.engine
            .on_backend(
                BackendEvent::LayoutChanged {
                    layout: "fr(azerty)".into(),
                    group: 2,
                },
                h.t0,
            )
            .await;

        assert!(
            h.fake.keys_down().is_empty(),
            "a key described by a keymap that is gone is released: {:?}",
            h.calls().releases
        );
        assert_eq!(
            h.held_file(),
            json!({ "keys": [] }),
            "and the crash guard says so too"
        );
        // The group is adopted, which shows in what a stroke that switches group
        // switches BACK to. Group 2 is now the resting group, so a resolution that
        // wants group 2 needs no switch at all.
        h.fake.forget();
        h.fake.teach(
            &Want::Symbol("é".into()),
            Resolved {
                code: PlatformKey {
                    code: 50,
                    detail: 0,
                },
                mods: 0,
                prefix: None,
                group: Some(2),
            },
        );
        h.feed(
            &peer,
            vec![Frame::Key {
                session: 1,
                n: 3,
                usage: 0,
                key: None,
                sym: Some("é".into()),
                mods: 0,
                layout: "fr(azerty)".into(),
                down: true,
                lock: false,
            }],
            h.t0,
        )
        .await;
        let actions = h.calls().actions;
        assert!(
            !actions.iter().any(|a| matches!(a, Action::Group(_))),
            "a stroke on the group the machine is already in switches nothing: \
             {actions:?}"
        );
        assert!(
            actions.iter().any(|a| matches!(
                a,
                Action::Key {
                    code: PlatformKey { code: 50, .. },
                    down: true
                }
            )),
            "and it still types: {actions:?}"
        );
    }

    fn positions(frames: &[Frame]) -> Vec<(i32, i32)> {
        frames
            .iter()
            .filter_map(|f| match f {
                Frame::Pointer { x, y, .. } => Some((*x, *y)),
                _ => None,
            })
            .collect()
    }

    fn counters(frames: &[Frame]) -> Vec<u32> {
        frames.iter().filter_map(Frame::flow).collect()
    }
}
