// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The crossing graph, derived from the plane, and the guards that decide
//! whether the pointer really crosses (doc/input-sharing.md, section 7).
//!
//! Nothing here is typed in by a human: the graph is computed from the
//! rectangles of the layout document, which is what makes an imported
//! arrangement work without anyone describing their desk twice.
//!
//! # The projection is the identity, and that is the honest answer
//!
//! The epic asks for "crossing projected proportionally over the portion of edge
//! the two monitors actually share". That phrasing comes from the prior art,
//! which has no plane and must therefore map the fraction along one monitor's
//! edge to the same fraction along another's. On ONE shared coordinate system
//! that stretching would move the pointer vertically at a crossing, which is
//! exactly what a plane exists to prevent: a pointer leaving M at plane-y Y
//! enters N at plane-y Y, and monitors of different heights are handled by the
//! shared stretch being shorter than either edge. What the epic is really asking
//! for is that the mapping be DERIVED from the geometry rather than typed in, and
//! it is.
//!
//! Scale differences need no conversion either: the plane is in logical pixels, a
//! logical pixel is about the same physical size everywhere, and the conversion
//! to device pixels happens on the target at injection. Mixing a 4K at 100% with
//! a Retina at 200% therefore does not change how the mouse feels.

use std::collections::BTreeMap;

use crate::backend::Monitor;
use crate::keys::mods;
use crate::plane::{Plane, Spot, split_spot_key, spot_key};

/// How close two edges must be to count as abutting, in logical pixels.
///
/// Not zero, and that is deliberate: a plane built by importing two machines'
/// own arrangements has no reason to be pixel perfect, and a human dragging a
/// rectangle cannot be asked to land on the exact pixel. Small enough that two
/// screens a hand's width apart on the plane are not silently joined.
pub const SNAP: i32 = 16;

/// How far from either end of a crossing segment the pointer must stay, in
/// logical pixels. Corners hold the Start menu and the macOS hot corners, and a
/// feature that steals them is a feature people turn off.
pub const DEAD_CORNER: i32 = 16;

/// How long the pointer must rest against a segment before the crossing fires.
/// 250 ms is where the prior art converged.
pub const DWELL_MS: u32 = 250;

/// Which edge of a monitor a crossing leaves by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

impl Side {
    pub fn as_str(self) -> &'static str {
        match self {
            Side::Left => "left",
            Side::Right => "right",
            Side::Top => "top",
            Side::Bottom => "bottom",
        }
    }

    pub fn parse(s: &str) -> Option<Side> {
        match s {
            "left" => Some(Side::Left),
            "right" => Some(Side::Right),
            "top" => Some(Side::Top),
            "bottom" => Some(Side::Bottom),
            _ => None,
        }
    }

    /// The side of the neighbour the pointer arrives THROUGH: leaving by a right
    /// edge is arriving by a left one. [`entry_point`] does the placing itself
    /// (it needs the far rectangle, not just the side), so this exists for the
    /// interface's sake, which has to name the far side of a crossing when it
    /// describes a neighbour's guards.
    pub fn opposite(self) -> Side {
        match self {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
            Side::Top => Side::Bottom,
            Side::Bottom => Side::Top,
        }
    }
}

/// A monitor placed on the shared plane: the geometry the graph works in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placed {
    /// The device whose screen this is.
    pub node_id: String,
    /// `<node_id>/<monitor id>`.
    pub key: String,
    /// Position on the shared plane, logical pixels.
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// The monitor as its own machine describes it, so a crossing can be turned
    /// into that machine's own desktop coordinates.
    pub own: Monitor,
    /// Is the screen actually there? A screen that is away keeps its place and
    /// its edges become walls, so the pointer is never swallowed by a screen that
    /// is not there.
    pub present: bool,
}

impl Placed {
    fn right(&self) -> i32 {
        self.x.saturating_add(self.w)
    }

    fn bottom(&self) -> i32 {
        self.y.saturating_add(self.h)
    }

    fn contains(&self, x: i32, y: i32) -> bool {
        (self.x..self.right()).contains(&x) && (self.y..self.bottom()).contains(&y)
    }

    /// Plane coordinates to this machine's own desktop coordinates. The two
    /// differ by exactly where the plane put the monitor, which is the whole
    /// content of the layout document.
    pub fn to_own(&self, x: i32, y: i32) -> (i32, i32) {
        (
            self.own.x.saturating_add(x.saturating_sub(self.x)),
            self.own.y.saturating_add(y.saturating_sub(self.y)),
        )
    }
}

/// One stretch of one monitor's edge with a live neighbour across it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    /// The monitor being left.
    pub from: String,
    /// The monitor being entered.
    pub to: String,
    /// The device that owns `to`: who would be driven.
    pub node_id: String,
    pub side: Side,
    /// The shared stretch, in plane coordinates along the edge (y for a left or
    /// right side, x for a top or bottom one). Half open.
    pub lo: i32,
    pub hi: i32,
}

impl Segment {
    /// Is `along` inside the stretch, and far enough from both ends to clear the
    /// dead corners?
    fn admits(&self, along: i32, dead_corner: i32) -> bool {
        // A NEGATIVE dead corner would extend the admitted stretch PAST the shared
        // segment, so a crossing would fire from a stretch of edge the graph calls
        // a wall. [`Guards::from_value`] clamps at zero, but the field is public and
        // [`crate::settings::Settings::set_guards`] takes a value the caller built,
        // so the clamp that matters is the one at the point of use.
        let dead_corner = dead_corner.max(0);
        let lo = self.lo.saturating_add(dead_corner);
        let hi = self.hi.saturating_sub(dead_corner);
        // A stretch shorter than two dead corners admits nothing rather than
        // inverting: two screens overlapping by a few pixels is a wall, not a
        // crossing whose bounds have crossed over each other.
        lo < hi && (lo..hi).contains(&along)
    }
}

/// The guards on one neighbour, as the machine that captures enforces them
/// (doc/input-sharing.md, section 7). Stored per `(our monitor, their node_id,
/// their monitor, side)`; a pair with nothing stored uses [`Guards::default`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Guards {
    pub dwell_ms: u32,
    /// Leave the edge and come back within this many milliseconds. Zero is off,
    /// and off is the default: two ways of saying "not by accident" is one too
    /// many for a default.
    pub double_tap_ms: u32,
    pub dead_corner: i32,
    /// Exactly these canonical modifier bits must be held for a crossing to be
    /// allowed at all. Zero is off.
    pub require_mods: u16,
    /// A wall a human asked for: this neighbour is not crossed to, whatever the
    /// geometry says.
    pub wall: bool,
}

impl Default for Guards {
    fn default() -> Guards {
        Guards {
            dwell_ms: DWELL_MS,
            double_tap_ms: 0,
            dead_corner: DEAD_CORNER,
            require_mods: 0,
            wall: false,
        }
    }
}

impl Guards {
    pub fn to_value(self) -> serde_json::Value {
        serde_json::json!({
            "dwell_ms": self.dwell_ms,
            "double_tap_ms": self.double_tap_ms,
            "dead_corner": self.dead_corner,
            "require_mods": self.require_mods,
            "wall": self.wall,
        })
    }

    /// Reads guards back, falling back to the default for anything missing or of
    /// the wrong type, and bounding what is there. Bounds first: a dwell of an
    /// hour is a pointer that never crosses and a human who cannot tell why, and
    /// a negative dead corner would invert a segment's admission test.
    pub fn from_value(v: &serde_json::Value) -> Guards {
        let default = Guards::default();
        let ms = |key: &str, fallback: u32| -> u32 {
            v.get(key)
                .and_then(serde_json::Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .map_or(fallback, |n| n.min(10_000))
        };
        Guards {
            dwell_ms: ms("dwell_ms", default.dwell_ms),
            double_tap_ms: ms("double_tap_ms", default.double_tap_ms),
            dead_corner: v
                .get("dead_corner")
                .and_then(serde_json::Value::as_i64)
                .and_then(|n| i32::try_from(n).ok())
                .map_or(default.dead_corner, |n| n.clamp(0, 1000)),
            require_mods: v
                .get("require_mods")
                .and_then(serde_json::Value::as_u64)
                .and_then(|n| u16::try_from(n).ok())
                // Only holdable bits can be required: requiring Caps Lock's
                // STATE would be a guard nobody could satisfy on purpose.
                .map_or(default.require_mods, |n| n & mods::HOLDABLE),
            wall: v
                .get("wall")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(default.wall),
        }
    }
}

/// The plane laid out, and the crossing segments derived from it.
#[derive(Clone, Debug, Default)]
pub struct Graph {
    /// Every monitor of every device, placed. Keyed `<node_id>/<monitor id>`.
    pub placed: BTreeMap<String, Placed>,
    /// Every crossing stretch, from a monitor of THIS device to a monitor of
    /// another. Only outbound: a crossing is a local decision, and the graph a
    /// device needs is the one leaving its own screens.
    pub segments: Vec<Segment>,
}

/// Lays the plane out and derives the graph, from this device's point of view.
///
/// `present` says which devices are here right now (reachable, and holding a
/// verified monitors entry). A monitor whose device is absent, or which its
/// device's current list no longer names, keeps its spot and becomes a ghost: its
/// edges are walls.
pub fn build(plane: &Plane, own_node_id: &str, present: &[String]) -> Graph {
    let placed = lay_out(plane, present);
    let segments = derive(&placed, own_node_id);
    Graph { placed, segments }
}

/// Places every monitor of every device on the plane.
///
/// A monitor with a spot goes where the spot says. A device with NO spot for any
/// of its monitors is appended to the right of the current bounding box, in
/// ascending node_id order, keeping its own arrangement as a movable block: the
/// epic's rule, because Windows and macOS already know where their screens are.
///
/// The derivation is a function of the records alone, so every device computes the
/// same layout from the same document. That is load bearing: two devices that
/// derived differently would disagree about what an absolute coordinate means,
/// and every session between them would refuse `PLANE_STALE` with nothing to
/// repair.
fn lay_out(plane: &Plane, present: &[String]) -> BTreeMap<String, Placed> {
    let mut out: BTreeMap<String, Placed> = BTreeMap::new();
    // Ordered by node_id, because a BTreeMap is, and the append order below
    // depends on it.
    let mut unplaced: Vec<&String> = Vec::new();
    for (node_id, entry) in &plane.monitors {
        // An unverified entry is not that device's word about its own screens, so
        // it is not part of the plane and is not part of the plane id either.
        if !entry.verified {
            continue;
        }
        let here = present.iter().any(|p| p == node_id);
        let spots: Vec<Option<Spot>> = entry
            .list
            .iter()
            .map(|m| {
                plane
                    .placement
                    .as_ref()
                    .and_then(|p| p.spots.get(&spot_key(node_id, &m.id)).copied())
            })
            .collect();
        if spots.iter().all(Option::is_none) && !entry.list.is_empty() {
            unplaced.push(node_id);
            continue;
        }
        for (m, spot) in entry.list.iter().zip(spots) {
            // One monitor of a placed block with no spot of its own (a screen
            // plugged in after the last drag) is placed by its own machine's
            // arrangement relative to a sibling that IS placed, which keeps the
            // block coherent. Falling back to its own desktop coordinates does
            // exactly that when the block's origin was not moved, and when it was
            // the human drags once more.
            let spot = spot.unwrap_or(Spot { x: m.x, y: m.y });
            out.insert(
                spot_key(node_id, &m.id),
                Placed {
                    node_id: node_id.clone(),
                    key: spot_key(node_id, &m.id),
                    x: spot.x,
                    y: spot.y,
                    w: m.w,
                    h: m.h,
                    own: m.clone(),
                    present: here,
                },
            );
        }
    }
    // Ghosts: a spot whose monitor no device currently claims. Its place is kept
    // so nothing else shifts, and it is a wall.
    if let Some(placement) = &plane.placement {
        for (key, spot) in &placement.spots {
            if out.contains_key(key) {
                continue;
            }
            let Some((node_id, monitor_id)) = split_spot_key(key) else {
                continue;
            };
            // A spot for a device the account no longer has is not a screen whose
            // place is kept, it is litter: dropped, so a departed device cannot
            // leave a permanent hole in the plane.
            //
            // **The gate is the VERIFIED entry and it has to be**, which is the
            // subtlest line in this file. `plane_id` hashes exactly the verified
            // entries, so gating on "any entry at all" made the layout depend on a
            // record the id excludes: a device holding an unverified entry for C
            // laid out a ghost, a device that had never heard of C laid out none,
            // and their two plane ids were EQUAL. A 1920x1080 ghost then shifts the
            // append offset of every undragged device, so the same absolute
            // coordinate meant a different screen on the two machines and the
            // `start` frame's plane check could not notice. The set of ghosts must
            // be decided by exactly the records the id covers.
            if !plane
                .monitors
                .get(node_id)
                .is_some_and(|entry| entry.verified)
            {
                continue;
            }
            // The size is not known (the device's list does not name it), so the
            // ghost occupies a nominal rectangle. It is never crossed into, so
            // only its extent as an obstacle matters, and a wrong extent is
            // better than a screen that vanishes and lets the pointer through to
            // whatever is behind it.
            out.insert(
                key.clone(),
                Placed {
                    node_id: node_id.to_string(),
                    key: key.clone(),
                    x: spot.x,
                    y: spot.y,
                    w: 1920,
                    h: 1080,
                    own: Monitor {
                        id: monitor_id.to_string(),
                        name: String::new(),
                        w: 1920,
                        h: 1080,
                        x: 0,
                        y: 0,
                        scale: 1000,
                        primary: false,
                    },
                    present: false,
                },
            );
        }
    }
    // Then the blocks nobody has ever dragged, appended to the right of
    // everything placed so far, in node_id order.
    for node_id in unplaced {
        let entry = &plane.monitors[node_id];
        let here = present.iter().any(|p| p == node_id);
        let offset = out.values().map(Placed::right).max().unwrap_or(0);
        // The block's own top-left, so its internal arrangement is preserved
        // exactly rather than each monitor being packed independently.
        let block_x = entry.list.iter().map(|m| m.x).min().unwrap_or(0);
        let block_y = entry.list.iter().map(|m| m.y).min().unwrap_or(0);
        for m in &entry.list {
            out.insert(
                spot_key(node_id, &m.id),
                Placed {
                    node_id: node_id.clone(),
                    key: spot_key(node_id, &m.id),
                    x: offset.saturating_add(m.x.saturating_sub(block_x)),
                    y: m.y.saturating_sub(block_y),
                    w: m.w,
                    h: m.h,
                    own: m.clone(),
                    present: here,
                },
            );
        }
    }
    out
}

/// Derives every crossing stretch leaving this device's monitors.
fn derive(placed: &BTreeMap<String, Placed>, own_node_id: &str) -> Vec<Segment> {
    let mut segments = Vec::new();
    for from in placed.values().filter(|p| p.node_id == own_node_id) {
        for to in placed.values() {
            // Never to our own screens (the pointer walks between them by
            // itself: they are one desktop) and never into a ghost.
            if to.node_id == own_node_id || !to.present {
                continue;
            }
            for side in [Side::Left, Side::Right, Side::Top, Side::Bottom] {
                if let Some((lo, hi)) = shared(from, to, side) {
                    segments.push(Segment {
                        from: from.key.clone(),
                        to: to.key.clone(),
                        node_id: to.node_id.clone(),
                        side,
                        lo,
                        hi,
                    });
                }
            }
        }
    }
    // Ordered, so the graph is a function of the plane and a test can compare
    // two of them.
    segments.sort_by(|a, b| (&a.from, a.side, &a.to, a.lo).cmp(&(&b.from, b.side, &b.to, b.lo)));
    segments
}

/// The stretch of `from`'s `side` that `to` is across, if any.
fn shared(from: &Placed, to: &Placed, side: Side) -> Option<(i32, i32)> {
    // Abutment within the snap tolerance, and only from the correct direction:
    // "beyond or at the edge" rather than "near the edge", so an overlapping
    // rectangle behind us is not read as a neighbour in front.
    let abuts = match side {
        Side::Right => (to.x - from.right()).abs() <= SNAP && to.x >= from.right() - SNAP,
        Side::Left => (from.x - to.right()).abs() <= SNAP && to.right() <= from.x + SNAP,
        Side::Bottom => (to.y - from.bottom()).abs() <= SNAP && to.y >= from.bottom() - SNAP,
        Side::Top => (from.y - to.bottom()).abs() <= SNAP && to.bottom() <= from.y + SNAP,
    };
    if !abuts {
        return None;
    }
    let (lo, hi) = match side {
        Side::Left | Side::Right => (from.y.max(to.y), from.bottom().min(to.bottom())),
        Side::Top | Side::Bottom => (from.x.max(to.x), from.right().min(to.right())),
    };
    // A stretch of zero or negative length is not shared: two screens that touch
    // at a corner only are a wall, not a crossing through a point.
    if lo >= hi { None } else { Some((lo, hi)) }
}

/// What the pointer's position on the plane means for a crossing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AtEdge {
    /// Inside one of our own monitors, nowhere near a crossing.
    Inside,
    /// Against a stretch of edge with a live neighbour across it.
    Segment(Segment),
    /// Against an edge with nothing across it, or something a guard forbids.
    /// Carries what the interface can say.
    Wall(WallReason),
}

/// Why a crossing did not happen, for the sentence the interface says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WallReason {
    /// Nothing is across this stretch of edge.
    Nothing,
    /// A screen is across it, but it is not connected right now. Its place is
    /// kept.
    ScreenAway,
    /// The pointer is pinned to this screen (`input.lock`).
    Locked,
    /// A human made this neighbour a wall.
    Refused,
    /// Too close to a corner.
    Corner,
    /// The neighbour needs a modifier held.
    NeedsModifier,
}

impl WallReason {
    /// Where this refusal sits in the guard chain of section 7 (locked, wall,
    /// dead corner, required modifier), lowest first.
    ///
    /// The contract is that "an earlier guard's refusal is the one the interface
    /// reports", and the first implementation was last-assignment-wins over the
    /// segments on one side instead, so which sentence a human saw depended on the
    /// order two neighbours happened to sit in the graph. A priority makes the
    /// most specific and earliest refusal win, whatever that order is.
    fn rank(self) -> u8 {
        match self {
            WallReason::Locked => 0,
            // The two faces of guard 2: a human's wall, and the geometry's.
            WallReason::Refused => 1,
            WallReason::Nothing | WallReason::ScreenAway => 2,
            WallReason::Corner => 3,
            WallReason::NeedsModifier => 4,
        }
    }
}

/// Keeps the EARLIEST refusal in the guard chain (see [`WallReason::rank`]),
/// which is the one the contract says the interface reports.
fn keep_earliest(worst: &mut Option<WallReason>, reason: WallReason) {
    if worst.is_none_or(|held| reason.rank() < held.rank()) {
        *worst = Some(reason);
    }
}

/// Where the pointer would leave, given the plane position it is being pushed to.
///
/// `mods_held` and `locked` are the state the guards read; `guards_for` supplies
/// the per neighbour guards. This answers only the geometric and stateless half:
/// the dwell and the double tap are time, and they live in the session
/// ([`crate::session`]), because they are about how long the pointer has been here
/// rather than about where it is.
pub fn at_edge(
    graph: &Graph,
    from_key: &str,
    x: i32,
    y: i32,
    mods_held: u16,
    locked: bool,
    guards_for: &dyn Fn(&Segment) -> Guards,
) -> AtEdge {
    let Some(from) = graph.placed.get(from_key) else {
        return AtEdge::Wall(WallReason::Nothing);
    };
    if from.contains(x, y) {
        return AtEdge::Inside;
    }
    // Which side is being pushed through, and where along it. Diagonal pressure
    // resolves to the axis it left by furthest, so a pointer shoved into a corner
    // does not oscillate between two answers.
    let over_left = from.x - x;
    let over_right = x - (from.right() - 1);
    let over_top = from.y - y;
    let over_bottom = y - (from.bottom() - 1);
    let side = [
        (over_left, Side::Left),
        (over_right, Side::Right),
        (over_top, Side::Top),
        (over_bottom, Side::Bottom),
    ]
    .into_iter()
    .filter(|(over, _)| *over > 0)
    .max_by_key(|(over, side)| (*over, std::cmp::Reverse(*side)))
    .map(|(_, side)| side);
    let Some(side) = side else {
        return AtEdge::Inside;
    };
    if locked {
        return AtEdge::Wall(WallReason::Locked);
    }
    // Clamped to the edge being left. `.max(from.x)` is the belt against a
    // rectangle of zero width or height: `Ord::clamp` asserts `min <= max` in
    // release as well as debug, so a monitor a backend reported as 0x0 (an X11
    // RandR disabled CRTC does exactly that) would take the whole component down
    // here. [`crate::plane::is_sane`] is the braces; a panic in the geometry is
    // worth both.
    let along = match side {
        Side::Left | Side::Right => y.clamp(from.y, from.bottom().saturating_sub(1).max(from.y)),
        Side::Top | Side::Bottom => x.clamp(from.x, from.right().saturating_sub(1).max(from.x)),
    };
    // Every candidate on this side, and the first that admits the pointer wins.
    // Ordered by the graph's own order, so the answer is deterministic when two
    // neighbours overlap on the plane.
    //
    // The guards inside the loop are evaluated in the chain's order (wall, then
    // dead corner, then required modifier), because "an earlier guard's refusal is
    // the one the interface reports" is part of the contract and the first
    // implementation read the dead corner before the wall.
    let mut worst: Option<WallReason> = None;
    for segment in graph
        .segments
        .iter()
        .filter(|s| s.from == from_key && s.side == side)
    {
        // A neighbour whose stretch does not cover this point is not this point's
        // neighbour, and it contributes no sentence at all: reporting a human's
        // wall for a segment the pointer is nowhere near would name the wrong
        // neighbour.
        if !(segment.lo..segment.hi).contains(&along) {
            continue;
        }
        let guards = guards_for(segment);
        if guards.wall {
            keep_earliest(&mut worst, WallReason::Refused);
            continue;
        }
        if !segment.admits(along, guards.dead_corner) {
            // Inside the stretch but inside a dead corner is a more specific
            // answer than "nothing is across this edge".
            keep_earliest(&mut worst, WallReason::Corner);
            continue;
        }
        if guards.require_mods != 0 && mods_held & mods::HOLDABLE != guards.require_mods {
            keep_earliest(&mut worst, WallReason::NeedsModifier);
            continue;
        }
        return AtEdge::Segment(segment.clone());
    }
    // Nothing across this stretch: is that because a screen is away? The ghost
    // check is last because it is the least specific and the most expensive.
    if worst.is_none()
        && graph.placed.values().any(|p| {
            !p.present && shared(from, p, side).is_some_and(|(lo, hi)| (lo..hi).contains(&along))
        })
    {
        keep_earliest(&mut worst, WallReason::ScreenAway);
    }
    AtEdge::Wall(worst.unwrap_or(WallReason::Nothing))
}

/// Where the pointer lands on the far side of a crossing, in the TARGET's own
/// logical desktop coordinates.
///
/// The plane coordinate carries over unchanged (the projection is the identity);
/// what changes is which machine's origin it is measured from. The pointer is
/// placed one pixel INSIDE the far monitor, not on its boundary: on the boundary
/// it would immediately read as against the wall it just came through, and the
/// crossing would fight itself.
pub fn entry_point(graph: &Graph, segment: &Segment, along: i32) -> Option<(i32, i32)> {
    let to = graph.placed.get(&segment.to)?;
    // `.max(segment.lo)` for the same reason as the clamps above: a segment always
    // has `hi > lo` by construction, and a clamp is not the place to find out that
    // an invariant slipped.
    let along = along.clamp(segment.lo, segment.hi.saturating_sub(1).max(segment.lo));
    let (x, y) = match segment.side {
        Side::Right => (to.x, along),
        Side::Left => (to.right().saturating_sub(1), along),
        Side::Bottom => (along, to.y),
        Side::Top => (along, to.bottom().saturating_sub(1)),
    };
    Some(to.to_own(x, y))
}

/// Clamps a plane position to the monitor it is on, or to the last one it was on
/// when it has left the plane altogether: the wall, made arithmetic.
///
/// This is what stops the virtual cursor from wandering off into empty plane
/// while the pointer is pinned on the source: a push that no segment admits
/// leaves the cursor against the edge rather than beyond it.
pub fn clamp_to(graph: &Graph, key: &str, x: i32, y: i32) -> (i32, i32) {
    match graph.placed.get(key) {
        // `.max(m.x)` and `.max(m.y)`: a rectangle of zero width or height would
        // make `min > max` and `Ord::clamp` panics on that in release too, so a
        // monitor reported as 0x0 must not be able to take the component down from
        // here (see [`at_edge`]).
        Some(m) => (
            x.clamp(m.x, m.right().saturating_sub(1).max(m.x)),
            y.clamp(m.y, m.bottom().saturating_sub(1).max(m.y)),
        ),
        None => (x, y),
    }
}

/// Which of this device's monitors a plane position is on, if any.
pub fn monitor_at(graph: &Graph, own_node_id: &str, x: i32, y: i32) -> Option<String> {
    graph
        .placed
        .values()
        .find(|p| p.node_id == own_node_id && p.contains(x, y))
        .map(|p| p.key.clone())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::identity::Identity;
    use crate::plane::Spot;
    use crate::plane::plane_id;

    fn key(n: u8) -> String {
        std::iter::repeat_n(format!("{n:02x}"), 32).collect()
    }

    fn identity() -> (tempfile::TempDir, Identity) {
        let dir = tempfile::tempdir().expect("tempdir");
        let identity = Identity::load_or_generate(dir.path()).expect("mint");
        (dir, identity)
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
            primary: false,
        }
    }

    fn defaults(_: &Segment) -> Guards {
        Guards::default()
    }

    /// Two machines side by side, one screen each: the crossing exists on the
    /// touching edges only, the stretch is what the two really share, and the
    /// projection carries the plane coordinate over unchanged.
    #[test]
    fn two_screens_side_by_side_share_the_stretch_they_really_share() {
        let (_d1, mine) = identity();
        let (_d2, theirs) = identity();
        let (a, b) = (key(0xa), key(0xb));
        let mut plane = Plane::default();
        plane.publish_monitors(&a, &mine, vec![screen("A", 0, 0, 1920, 1080)]);
        let mut other = Plane::default();
        other.publish_monitors(&b, &theirs, vec![screen("B", 0, 0, 2560, 1440)]);
        plane.merge(&b, &other.offer(&theirs.public_hex()));
        plane
            .place(
                &a,
                &mine,
                BTreeMap::from([
                    (spot_key(&a, "A"), Spot { x: 0, y: 0 }),
                    (spot_key(&b, "B"), Spot { x: 1920, y: -100 }),
                ]),
            )
            .expect("place");

        let graph = build(&plane, &a, &[a.clone(), b.clone()]);
        assert_eq!(graph.segments.len(), 1, "{:?}", graph.segments);
        let segment = &graph.segments[0];
        assert_eq!(segment.side, Side::Right);
        assert_eq!(segment.node_id, b);
        // A is 0..1080, B is -100..1340: the shared stretch is 0..1080.
        assert_eq!((segment.lo, segment.hi), (0, 1080));

        // The entry point keeps the plane's y and lands one pixel inside B, in
        // B's own desktop coordinates (B's own origin is 0,0 and the plane put it
        // at 1920,-100, so plane y 540 is B's own y 640).
        assert_eq!(entry_point(&graph, segment, 540), Some((0, 640)));
    }

    /// A stretch of edge with nothing across it is a wall, and the interface can
    /// tell "nothing there" from "a screen that is away" from "a corner".
    #[test]
    fn an_edge_with_nothing_across_it_is_a_wall_and_says_which_kind() {
        let (_d1, mine) = identity();
        let (_d2, theirs) = identity();
        let (a, b) = (key(0xa), key(0xb));
        let mut plane = Plane::default();
        plane.publish_monitors(&a, &mine, vec![screen("A", 0, 0, 1920, 1080)]);
        let mut other = Plane::default();
        // Their screen shares only the lower half of A's right edge.
        other.publish_monitors(&b, &theirs, vec![screen("B", 0, 0, 1280, 540)]);
        plane.merge(&b, &other.offer(&theirs.public_hex()));
        plane
            .place(
                &a,
                &mine,
                BTreeMap::from([
                    (spot_key(&a, "A"), Spot { x: 0, y: 0 }),
                    (spot_key(&b, "B"), Spot { x: 1920, y: 540 }),
                ]),
            )
            .expect("place");
        let graph = build(&plane, &a, &[a.clone(), b.clone()]);

        // The middle of the shared stretch crosses.
        let AtEdge::Segment(_) =
            at_edge(&graph, &spot_key(&a, "A"), 1920, 800, 0, false, &defaults)
        else {
            panic!("the shared stretch must cross");
        };
        // The upper half of the same edge has nothing across it.
        assert_eq!(
            at_edge(&graph, &spot_key(&a, "A"), 1920, 200, 0, false, &defaults),
            AtEdge::Wall(WallReason::Nothing)
        );
        // The left edge has nothing across it either.
        assert_eq!(
            at_edge(&graph, &spot_key(&a, "A"), -1, 500, 0, false, &defaults),
            AtEdge::Wall(WallReason::Nothing)
        );
        // Just inside the stretch but inside its dead corner.
        assert_eq!(
            at_edge(&graph, &spot_key(&a, "A"), 1920, 545, 0, false, &defaults),
            AtEdge::Wall(WallReason::Corner)
        );
        // And inside the monitor is inside.
        assert_eq!(
            at_edge(&graph, &spot_key(&a, "A"), 960, 540, 0, false, &defaults),
            AtEdge::Inside
        );
    }

    /// A screen that is away keeps its place, its edges are walls, and the
    /// pointer is never swallowed by a screen that is not there: the epic's rule,
    /// including the consequence that a live screen BEHIND a ghost becomes
    /// unreachable rather than the pointer jumping over the gap.
    #[test]
    fn a_screen_that_is_away_keeps_its_place_and_walls_the_edge() {
        let (_d1, mine) = identity();
        let (_d2, away) = identity();
        let (_d3, far) = identity();
        let (a, b, c) = (key(0xa), key(0xb), key(0xc));
        let mut plane = Plane::default();
        plane.publish_monitors(&a, &mine, vec![screen("A", 0, 0, 1920, 1080)]);
        let mut theirs = Plane::default();
        theirs.publish_monitors(&b, &away, vec![screen("B", 0, 0, 1920, 1080)]);
        plane.merge(&b, &theirs.offer(&away.public_hex()));
        let mut third = Plane::default();
        third.publish_monitors(&c, &far, vec![screen("C", 0, 0, 1920, 1080)]);
        plane.merge(&c, &third.offer(&far.public_hex()));
        plane
            .place(
                &a,
                &mine,
                BTreeMap::from([
                    (spot_key(&a, "A"), Spot { x: 0, y: 0 }),
                    (spot_key(&b, "B"), Spot { x: 1920, y: 0 }),
                    (spot_key(&c, "C"), Spot { x: 3840, y: 0 }),
                ]),
            )
            .expect("place");

        // Everyone here: the crossing to B exists, and C is not adjacent to us.
        let all = build(&plane, &a, &[a.clone(), b.clone(), c.clone()]);
        assert_eq!(all.segments.len(), 1);
        assert_eq!(all.segments[0].node_id, b);

        // B goes away: its place is kept, and A's right edge is a wall that says
        // so. C, behind it, does NOT become reachable.
        let without_b = build(&plane, &a, &[a.clone(), c.clone()]);
        assert!(without_b.segments.is_empty(), "{:?}", without_b.segments);
        assert_eq!(
            at_edge(
                &without_b,
                &spot_key(&a, "A"),
                1920,
                540,
                0,
                false,
                &defaults
            ),
            AtEdge::Wall(WallReason::ScreenAway)
        );
        // And its rectangle is still in the plane, so nothing shifted.
        let ghost = &without_b.placed[&spot_key(&b, "B")];
        assert_eq!((ghost.x, ghost.y), (1920, 0));
        assert!(!ghost.present);
    }

    /// The guards, one by one, each refusing with its own word.
    ///
    /// Named for one segment, because that is all it can prove: it hands the SAME
    /// `Guards` back for every segment, so it could never notice `guards_for` being
    /// asked about the wrong neighbour. The two things it does not reach have their
    /// own tests, [`the_guards_asked_about_are_the_neighbours_own`] and
    /// [`the_earliest_guard_in_the_chain_is_the_one_reported`].
    #[test]
    fn every_guard_refuses_with_its_own_word_on_the_one_segment_it_watches() {
        let (_d1, mine) = identity();
        let (_d2, theirs) = identity();
        let (a, b) = (key(0xa), key(0xb));
        let mut plane = Plane::default();
        plane.publish_monitors(&a, &mine, vec![screen("A", 0, 0, 1920, 1080)]);
        let mut other = Plane::default();
        other.publish_monitors(&b, &theirs, vec![screen("B", 0, 0, 1920, 1080)]);
        plane.merge(&b, &other.offer(&theirs.public_hex()));
        plane
            .place(
                &a,
                &mine,
                BTreeMap::from([
                    (spot_key(&a, "A"), Spot { x: 0, y: 0 }),
                    (spot_key(&b, "B"), Spot { x: 1920, y: 0 }),
                ]),
            )
            .expect("place");
        let graph = build(&plane, &a, &[a.clone(), b.clone()]);
        assert_eq!(
            graph.segments.len(),
            1,
            "one segment is the whole scope of this test"
        );
        let at = |mods_held, locked, guards: Guards| {
            at_edge(
                &graph,
                &spot_key(&a, "A"),
                1920,
                540,
                mods_held,
                locked,
                &|_: &Segment| guards,
            )
        };

        // Locked to screen outranks everything, including a perfectly good
        // segment.
        assert_eq!(
            at(0, true, Guards::default()),
            AtEdge::Wall(WallReason::Locked)
        );
        // A human's wall.
        assert_eq!(
            at(
                0,
                false,
                Guards {
                    wall: true,
                    ..Guards::default()
                }
            ),
            AtEdge::Wall(WallReason::Refused)
        );
        // A required modifier, and EXACTLY it: an extra one does not satisfy the
        // guard, because a crossing that fires while a shortcut is half typed is
        // the accident the guard exists to prevent.
        let needs_shift = Guards {
            require_mods: mods::SHIFT,
            ..Guards::default()
        };
        assert_eq!(
            at(0, false, needs_shift),
            AtEdge::Wall(WallReason::NeedsModifier)
        );
        assert_eq!(
            at(mods::SHIFT | mods::CTRL, false, needs_shift),
            AtEdge::Wall(WallReason::NeedsModifier)
        );
        assert!(matches!(
            at(mods::SHIFT, false, needs_shift),
            AtEdge::Segment(_)
        ));
        // A lock STATE is not a held modifier, so it neither satisfies nor
        // breaks the guard.
        assert!(matches!(
            at(mods::SHIFT | mods::CAPS, false, needs_shift),
            AtEdge::Segment(_)
        ));
        // A dead corner big enough to swallow the whole stretch admits nothing,
        // rather than inverting its bounds.
        assert_eq!(
            at(
                0,
                false,
                Guards {
                    dead_corner: 1000,
                    ..Guards::default()
                }
            ),
            AtEdge::Wall(WallReason::Corner)
        );
    }

    /// A machine nobody has dragged is appended to the right of the plane,
    /// keeping its own arrangement as one block, and every device derives the
    /// same layout from the same records. That last part is load bearing: a
    /// derivation that differed between two devices would make every session
    /// between them refuse over a plane nobody could repair.
    #[test]
    fn an_undragged_machine_is_appended_as_a_block_and_every_device_agrees() {
        let (_d1, first) = identity();
        let (_d2, second) = identity();
        let (a, b) = (key(0xa), key(0xb));
        let mut a_side = Plane::default();
        a_side.publish_monitors(&a, &first, vec![screen("A", 0, 0, 1920, 1080)]);
        let mut b_side = Plane::default();
        // Two screens, the second to the right of the first, and the machine's
        // own origin is not zero.
        b_side.publish_monitors(
            &b,
            &second,
            vec![
                screen("B1", -1280, 0, 1280, 800),
                screen("B2", 0, 0, 1920, 1080),
            ],
        );
        // TWO planes, each built from the messages a real device would have
        // received, and not one object read twice: the "every device agrees" half of
        // this test could not fail while both graphs came out of the same `Plane`,
        // which is exactly the half that is load bearing.
        a_side.merge(&b, &b_side.offer(&second.public_hex()));
        b_side.merge(&a, &a_side.offer(&first.public_hex()));

        let from_a = build(&a_side, &a, &[a.clone(), b.clone()]);
        let from_b = build(&b_side, &b, &[a.clone(), b.clone()]);
        assert_eq!(
            plane_id(&a_side),
            plane_id(&b_side),
            "the two devices hold the same records, which is what makes agreeing meaningful"
        );
        assert_eq!(
            from_a.placed.keys().collect::<Vec<_>>(),
            from_b.placed.keys().collect::<Vec<_>>(),
            "and they place the same screens"
        );
        // The same rectangles on both sides.
        for key in from_a.placed.keys() {
            let (mine, theirs) = (&from_a.placed[key], &from_b.placed[key]);
            assert_eq!(
                (mine.x, mine.y, mine.w, mine.h),
                (theirs.x, theirs.y, theirs.w, theirs.h)
            );
        }
        // B's block sits to the right of A, with its internal arrangement intact.
        let b1 = &from_a.placed[&spot_key(&b, "B1")];
        let b2 = &from_a.placed[&spot_key(&b, "B2")];
        assert_eq!((b1.x, b1.y), (1920, 0));
        assert_eq!((b2.x, b2.y), (1920 + 1280, 0));
        // And the crossing exists, from A's right edge into B1.
        assert_eq!(from_a.segments.len(), 1);
        assert_eq!(from_a.segments[0].to, spot_key(&b, "B1"));
        // Seen from B, the crossing goes the other way.
        assert_eq!(from_b.segments.len(), 1);
        assert_eq!(from_b.segments[0].to, spot_key(&a, "A"));
        assert_eq!(from_b.segments[0].side, Side::Left);
    }

    /// The snap tolerance joins an arrangement nobody dragged pixel perfectly,
    /// and does not join two screens a hand's width apart.
    #[test]
    fn the_snap_tolerance_joins_a_near_miss_and_not_a_real_gap() {
        let (_d1, mine) = identity();
        let (_d2, theirs) = identity();
        let (a, b) = (key(0xa), key(0xb));
        let build_with = |gap: i32| {
            let mut plane = Plane::default();
            plane.publish_monitors(&a, &mine, vec![screen("A", 0, 0, 1920, 1080)]);
            let mut other = Plane::default();
            other.publish_monitors(&b, &theirs, vec![screen("B", 0, 0, 1920, 1080)]);
            plane.merge(&b, &other.offer(&theirs.public_hex()));
            plane
                .place(
                    &a,
                    &mine,
                    BTreeMap::from([
                        (spot_key(&a, "A"), Spot { x: 0, y: 0 }),
                        (
                            spot_key(&b, "B"),
                            Spot {
                                x: 1920 + gap,
                                y: 0,
                            },
                        ),
                    ]),
                )
                .expect("place");
            build(&plane, &a, &[a.clone(), b.clone()])
        };
        assert_eq!(build_with(0).segments.len(), 1, "touching");
        assert_eq!(build_with(SNAP).segments.len(), 1, "within the tolerance");
        assert_eq!(
            build_with(SNAP + 1).segments.len(),
            0,
            "beyond the tolerance is a real gap"
        );
        // And a screen BEHIND us is not a neighbour in front of us.
        assert_eq!(
            build_with(-1920).segments.len(),
            0,
            "overlapping is not across"
        );
    }

    /// Two screens that touch only at a corner are a wall: a crossing through a
    /// single point is a crossing nobody can aim at.
    #[test]
    fn screens_that_touch_only_at_a_corner_do_not_cross() {
        let (_d1, mine) = identity();
        let (_d2, theirs) = identity();
        let (a, b) = (key(0xa), key(0xb));
        let mut plane = Plane::default();
        plane.publish_monitors(&a, &mine, vec![screen("A", 0, 0, 1920, 1080)]);
        let mut other = Plane::default();
        other.publish_monitors(&b, &theirs, vec![screen("B", 0, 0, 1920, 1080)]);
        plane.merge(&b, &other.offer(&theirs.public_hex()));
        plane
            .place(
                &a,
                &mine,
                BTreeMap::from([
                    (spot_key(&a, "A"), Spot { x: 0, y: 0 }),
                    (spot_key(&b, "B"), Spot { x: 1920, y: 1080 }),
                ]),
            )
            .expect("place");
        assert!(
            build(&plane, &a, &[a.clone(), b.clone()])
                .segments
                .is_empty()
        );
    }

    /// The guards survive their own storage, and a stored value that is absurd is
    /// bounded rather than obeyed: a dwell of an hour is a pointer that never
    /// crosses and a human who cannot tell why.
    #[test]
    fn stored_guards_round_trip_and_absurd_values_are_bounded() {
        let guards = Guards {
            dwell_ms: 400,
            double_tap_ms: 250,
            dead_corner: 8,
            require_mods: mods::CTRL,
            wall: true,
        };
        assert_eq!(Guards::from_value(&guards.to_value()), guards);
        assert_eq!(
            Guards::from_value(&serde_json::json!({})),
            Guards::default(),
            "an empty document is the default, not a broken guard"
        );
        let absurd = Guards::from_value(&serde_json::json!({
            "dwell_ms": 3_600_000u64, "dead_corner": -50, "require_mods": 0xFFFFu64,
            "double_tap_ms": "soon", "wall": 7,
        }));
        assert_eq!(absurd.dwell_ms, 10_000);
        assert_eq!(absurd.dead_corner, 0);
        assert_eq!(absurd.require_mods, mods::HOLDABLE, "only holdable bits");
        assert_eq!(absurd.double_tap_ms, Guards::default().double_tap_ms);
        assert!(!absurd.wall);
    }

    /// The clamp is the wall made arithmetic: a push no segment admits leaves the
    /// virtual cursor against the edge rather than wandering off the plane.
    #[test]
    fn the_clamp_keeps_the_cursor_on_the_screen_it_is_on() {
        let (_d1, mine) = identity();
        let a = key(0xa);
        let mut plane = Plane::default();
        plane.publish_monitors(&a, &mine, vec![screen("A", 0, 0, 1920, 1080)]);
        let graph = build(&plane, &a, std::slice::from_ref(&a));
        let key_a = spot_key(&a, "A");
        assert_eq!(clamp_to(&graph, &key_a, 5000, 5000), (1919, 1079));
        assert_eq!(clamp_to(&graph, &key_a, -5000, -5000), (0, 0));
        assert_eq!(clamp_to(&graph, &key_a, 960, 540), (960, 540));
        assert_eq!(monitor_at(&graph, &a, 960, 540), Some(key_a));
        assert_eq!(monitor_at(&graph, &a, 5000, 540), None);
    }

    /// **The same plane id means the same layout.** The highest-value assertion this
    /// component can make, because everything about a session's coordinates rests on
    /// it: two devices that agreed on the id and laid the plane out differently would
    /// disagree about what an absolute coordinate means, and the `start` frame's
    /// plane check could not notice.
    ///
    /// So the test builds planes that differ ONLY in facts the id excludes, and the
    /// layout has to come out identical. The case it was written for was real: the
    /// ghost gate asked whether a monitors entry EXISTED, which is true for an
    /// unverified one, while the id hashes only verified ones. A device holding an
    /// unverified entry for a third machine laid out a 1920x1080 ghost, a device that
    /// had never heard of that machine laid out none, the ghost shifted the append
    /// offset of every undragged device, and the two plane ids were equal.
    #[test]
    fn the_same_plane_id_means_the_same_placed_map() {
        let (_d1, mine) = identity();
        let (_d2, theirs) = identity();
        let (_d3, absent) = identity();
        let (a, b, c) = (key(0xa), key(0xb), key(0xc));

        // The common ground: two devices that know each other, nobody dragged.
        let common = || {
            let mut plane = Plane::default();
            plane.publish_monitors(&a, &mine, vec![screen("A", 0, 0, 1920, 1080)]);
            let mut other = Plane::default();
            other.publish_monitors(&b, &theirs, vec![screen("B", 0, 0, 1280, 800)]);
            plane.merge(&b, &other.offer(&theirs.public_hex()));
            plane
        };
        // A third machine's entry, UNVERIFIED (relayed by a courier whose word about
        // itself says nothing about C), plus a spot for it that no verified record
        // claims. Both are invisible to `plane_id` by construction.
        let mut heard_of_c = common();
        let mut c_side = Plane::default();
        c_side.publish_monitors(&c, &absent, vec![screen("C", 0, 0, 2560, 1440)]);
        let mut relayed = c_side.to_stored();
        relayed["d"] = serde_json::json!(crate::wire::DIALECT);
        relayed
            .as_object_mut()
            .expect("a map")
            .remove("pins")
            .expect("the courier's document carries no pins");
        heard_of_c.merge(&b, &relayed);
        assert!(
            !heard_of_c.monitors[&c].verified,
            "the fixture depends on C's entry being held rather than proven"
        );

        // A pin nobody else has: another fact the id excludes.
        let mut pinned = common();
        pinned.pin(&c, &absent.public_hex());

        let plain = common();
        assert_eq!(plane_id(&plain), plane_id(&heard_of_c));
        assert_eq!(plane_id(&plain), plane_id(&pinned));

        // `present` is not part of the id either, so it must not move a rectangle:
        // a screen that is away keeps its place, which is the whole rule.
        let rects = |plane: &Plane, present: &[String]| {
            build(plane, &a, present)
                .placed
                .iter()
                .map(|(key, p)| (key.clone(), (p.x, p.y, p.w, p.h)))
                .collect::<BTreeMap<String, (i32, i32, i32, i32)>>()
        };
        let both = [a.clone(), b.clone()];
        let baseline = rects(&plain, &both);
        assert_eq!(
            baseline,
            rects(&heard_of_c, &both),
            "an unverified entry the plane id excludes must not lay out a ghost"
        );
        assert_eq!(baseline, rects(&pinned, &both));
        assert_eq!(
            baseline,
            rects(&plain, std::slice::from_ref(&a)),
            "and a device being away moves nothing"
        );
    }

    /// The guards a crossing is judged by are its OWN neighbour's, which a test that
    /// hands the same `Guards` back for every segment cannot show.
    #[test]
    fn the_guards_asked_about_are_the_neighbours_own() {
        let (_d1, mine) = identity();
        let (_d2, upper) = identity();
        let (_d3, lower) = identity();
        let (a, b, c) = (key(0xa), key(0xb), key(0xc));
        let mut plane = Plane::default();
        plane.publish_monitors(&a, &mine, vec![screen("A", 0, 0, 1920, 1080)]);
        // Two neighbours across the SAME edge, each across half of it.
        for (node, id, name) in [(&b, &upper, "B"), (&c, &lower, "C")] {
            let mut other = Plane::default();
            other.publish_monitors(node, id, vec![screen(name, 0, 0, 1280, 540)]);
            plane.merge(node, &other.offer(&id.public_hex()));
        }
        plane
            .place(
                &a,
                &mine,
                BTreeMap::from([
                    (spot_key(&a, "A"), Spot { x: 0, y: 0 }),
                    (spot_key(&b, "B"), Spot { x: 1920, y: 0 }),
                    (spot_key(&c, "C"), Spot { x: 1920, y: 540 }),
                ]),
            )
            .expect("place");
        let graph = build(&plane, &a, &[a.clone(), b.clone(), c.clone()]);
        assert_eq!(graph.segments.len(), 2, "{:?}", graph.segments);

        // A wall on the UPPER neighbour only.
        let walled_upper = |s: &Segment| Guards {
            wall: s.to == spot_key(&b, "B"),
            ..Guards::default()
        };
        assert_eq!(
            at_edge(
                &graph,
                &spot_key(&a, "A"),
                1920,
                200,
                0,
                false,
                &walled_upper
            ),
            AtEdge::Wall(WallReason::Refused),
            "the upper stretch is the one a human walled"
        );
        let AtEdge::Segment(crossed) = at_edge(
            &graph,
            &spot_key(&a, "A"),
            1920,
            800,
            0,
            false,
            &walled_upper,
        ) else {
            panic!("the lower stretch has no wall and must cross");
        };
        assert_eq!(
            crossed.to,
            spot_key(&c, "C"),
            "and the guards of one neighbour must not decide the other's crossing"
        );
    }

    /// The refusal the interface is told is the EARLIEST guard in section 7's chain
    /// (locked, wall, dead corner, required modifier), whatever order the segments
    /// happen to sit in.
    ///
    /// It was last-assignment-wins over the candidates on one side, and the dead
    /// corner was read before the wall, so which sentence a human saw depended on
    /// the graph's ordering rather than on the contract.
    #[test]
    fn the_earliest_guard_in_the_chain_is_the_one_reported() {
        let (_d1, mine) = identity();
        let (_d2, theirs) = identity();
        let (a, b) = (key(0xa), key(0xb));
        let mut plane = Plane::default();
        plane.publish_monitors(&a, &mine, vec![screen("A", 0, 0, 1920, 1080)]);
        let mut other = Plane::default();
        other.publish_monitors(&b, &theirs, vec![screen("B", 0, 0, 1920, 1080)]);
        plane.merge(&b, &other.offer(&theirs.public_hex()));
        plane
            .place(
                &a,
                &mine,
                BTreeMap::from([
                    (spot_key(&a, "A"), Spot { x: 0, y: 0 }),
                    (spot_key(&b, "B"), Spot { x: 1920, y: 0 }),
                ]),
            )
            .expect("place");
        let graph = build(&plane, &a, &[a.clone(), b.clone()]);
        let at = |locked, guards: Guards| {
            at_edge(
                &graph,
                &spot_key(&a, "A"),
                1920,
                540,
                0,
                locked,
                &|_: &Segment| guards,
            )
        };
        // Every guard broken at once, and each in turn is the one reported.
        let everything = Guards {
            wall: true,
            dead_corner: 1000,
            require_mods: mods::SHIFT,
            ..Guards::default()
        };
        assert_eq!(
            at(true, everything),
            AtEdge::Wall(WallReason::Locked),
            "1. locked to screen"
        );
        assert_eq!(
            at(false, everything),
            AtEdge::Wall(WallReason::Refused),
            "2. a wall, before the dead corner it also breaks"
        );
        assert_eq!(
            at(
                false,
                Guards {
                    wall: false,
                    ..everything
                }
            ),
            AtEdge::Wall(WallReason::Corner),
            "3. the dead corner, before the modifier it also lacks"
        );
        assert_eq!(
            at(
                false,
                Guards {
                    wall: false,
                    dead_corner: 0,
                    ..everything
                }
            ),
            AtEdge::Wall(WallReason::NeedsModifier),
            "4. the required modifier"
        );
    }

    /// A negative dead corner does not widen a crossing.
    ///
    /// [`Guards::from_value`] clamps at zero, but the fields are public and
    /// [`crate::settings::Settings::set_guards`] takes a value its caller built, so a
    /// negative dead corner reached [`Segment::admits`] and extended the admitted
    /// stretch PAST the shared segment: a crossing fired from a stretch of edge the
    /// graph itself calls a wall.
    #[test]
    fn a_negative_dead_corner_does_not_widen_the_crossing() {
        let (_d1, mine) = identity();
        let (_d2, theirs) = identity();
        let (a, b) = (key(0xa), key(0xb));
        let mut plane = Plane::default();
        plane.publish_monitors(&a, &mine, vec![screen("A", 0, 0, 1920, 1080)]);
        let mut other = Plane::default();
        // Across the lower half of A's right edge only, so the upper half is a wall.
        other.publish_monitors(&b, &theirs, vec![screen("B", 0, 0, 1280, 540)]);
        plane.merge(&b, &other.offer(&theirs.public_hex()));
        plane
            .place(
                &a,
                &mine,
                BTreeMap::from([
                    (spot_key(&a, "A"), Spot { x: 0, y: 0 }),
                    (spot_key(&b, "B"), Spot { x: 1920, y: 540 }),
                ]),
            )
            .expect("place");
        let graph = build(&plane, &a, &[a.clone(), b.clone()]);
        let hostile = |_: &Segment| Guards {
            dead_corner: -400,
            ..Guards::default()
        };
        assert_eq!(
            at_edge(&graph, &spot_key(&a, "A"), 1920, 300, 0, false, &hostile),
            AtEdge::Wall(WallReason::Nothing),
            "a wall is a wall however negative a stored dead corner is"
        );
        assert!(
            matches!(
                at_edge(&graph, &spot_key(&a, "A"), 1920, 800, 0, false, &hostile),
                AtEdge::Segment(_)
            ),
            "and the real stretch still crosses"
        );
    }

    /// A monitor of no size cannot panic the geometry.
    ///
    /// `Ord::clamp` asserts `min <= max` in release as well as debug, so a rectangle
    /// of zero width took the whole component down through either clamp. X11 RandR
    /// reports `width = 0, height = 0` for a disabled CRTC, so this is the platform
    /// ticket's ordinary case and not an attack; the plane refuses such a monitor at
    /// the door now, and this is the belt behind that brace.
    #[test]
    fn a_monitor_of_no_size_cannot_panic_the_geometry() {
        let (_d1, mine) = identity();
        let a = key(0xa);
        let mut plane = Plane::default();
        plane.publish_monitors(&a, &mine, vec![screen("A", 0, 0, 1920, 1080)]);
        // Injected past the door, which is the only way in now: a hand-edited file
        // from an older build, or a record a future one writes.
        let entry = plane.monitors.get_mut(&a).expect("our own entry");
        entry.list.push(Monitor {
            w: 0,
            h: 0,
            ..screen("dead-crtc", 3000, 3000, 0, 0)
        });

        let graph = build(&plane, &a, std::slice::from_ref(&a));
        let dead = spot_key(&a, "dead-crtc");
        assert_eq!(
            clamp_to(&graph, &dead, 5000, 5000),
            (3000, 3000),
            "a rectangle of no size clamps to its own corner rather than panicking"
        );
        assert_eq!(clamp_to(&graph, &dead, -5000, -5000), (3000, 3000));
        assert_eq!(
            monitor_at(&graph, &a, 3000, 3000),
            None,
            "and nothing is ever on it, so no pointer is ever there"
        );
        assert_eq!(
            at_edge(&graph, &dead, 3001, 3000, 0, false, &defaults),
            AtEdge::Wall(WallReason::Nothing),
            "and being pushed off it is a wall rather than a panic"
        );
    }
}
