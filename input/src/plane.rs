// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The layout document: one plane, in logical pixels, holding every monitor of
//! every computer of the account (doc/input-sharing.md, section 6).
//!
//! Two authorities that do not overlap. Each device's own SIGNED word for its own
//! monitors, accepted only under that device's pinned engine key and only with a
//! higher `seq`. And last writer wins on the whole plane for the arrangement a
//! human drags, compared by `(seq, by, sig)`, which is a total order on the
//! placements themselves, so every device converges on the same plane from the
//! same set of messages in any order with no coordination at all.
//!
//! Convergence is the property everything else in this component rests on, and
//! every rule here is written to protect it: two devices that hold different
//! planes refuse every session between them (`PLANE_STALE`) with nothing to
//! repair it, because each keeps re-offering a document the other ignores. So the
//! order is total on the content and not on a projection of it, the bounds are
//! functions of the content and never of what this device happens to hold, and
//! anything device-local (`verified`, and therefore the pins) is kept out of what
//! decides rank.
//!
//! Not carried by the sync engine, deliberately: that engine syncs folders of
//! files, and routing a layout through a file would be a coupling nothing
//! justifies. It rides one `peers.send` message instead (64 KiB, where this
//! document is bounded at a few KiB by construction), merged idempotently.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::backend::Monitor;
use crate::identity::{Identity, verify};

/// Domain prefix of a monitors entry's signed bytes.
const DOMAIN_MONITORS: &[u8] = b"1device-input:monitors:";
/// Domain prefix of a placement's signed bytes. A constant prefix per kind so
/// the one engine key cannot sign one kind and have it verify as the other.
const DOMAIN_PLACEMENT: &[u8] = b"1device-input:placement:";

/// At most this many devices contribute monitors, and **this is this
/// component's one answer to "how big is an account"**: `settings.rs` derives
/// its own peer bound from this constant rather than picking a second number,
/// because two files of one component answering that question differently is
/// how a table that refuses and a table that accepts end up facing each other.
///
/// A bound and not a hope: the document is signed, replicated and held in
/// memory by every peer, so a peer that could grow it without limit could grow
/// everyone's. It bounds the STORED map as well as an incoming message, which
/// is the part that was missing at first and mattered most: four messages each
/// naming sixteen fabricated node_ids left sixty-four devices stored and an
/// offered document of 314876 bytes, over `peers.send`'s 64 KiB, so this device
/// could never announce its own layout again and every peer receiving that
/// document learned nothing about anybody. Persisted, so permanent.
pub const MAX_DEVICES: usize = 16;
/// At most this many monitors per device.
pub const MAX_MONITORS: usize = 16;
/// At most this many placed spots. Larger than `MAX_DEVICES * MAX_MONITORS`
/// would be pointless; smaller would drop the spots of screens that are away,
/// and a screen that is away keeps its place.
pub const MAX_SPOTS: usize = MAX_DEVICES * MAX_MONITORS;

/// Longest monitor id and name accepted, in bytes.
///
/// A monitor identity is an output name plus a hash of the EDID where one is
/// readable, and on Windows that is a display device path: a realistic one plus
/// a hash goes past 128 bytes, which used to make one bad monitor refuse the
/// whole entry and a machine's screens silently absent from every sibling's
/// plane. 256 is chosen to fit the longest identity a real backend produces.
///
/// A NAME is cosmetic, so it is truncated at the source instead of refusing
/// anything: a screen called something very long is still a screen.
pub const MAX_ID: usize = 256;
pub const MAX_NAME: usize = 128;

/// The highest `seq` either authority may carry, deliberately far below
/// `u32::MAX`.
///
/// `seq` is the whole of the ordering, and with no ceiling ONE message freezes
/// the plane for ever: a placement planted at `u32::MAX` is adopted, a human's
/// drag mints `saturating_add(1)` and lands on the same number, the tiebreak on
/// `by` then decides, and an author of `ff..ff` (a node nobody can ever pin, so
/// the pin re-examination of D11 can never fire) wins every comparison from
/// then on. The next epidemic sweep overwrites the drag, for ever. That
/// destroys the load-bearing half of D11's argument, which is that a human
/// fixes a forged arrangement with one drag.
///
/// A ceiling of sixteen million leaves more drags than a desk will ever see (one
/// a second for six months) and makes `saturating_add` unreachable, so a mint is
/// always strictly greater than anything a peer can have said.
///
/// What the ceiling does NOT buy, written down so it is not rediscovered as a
/// surprise: it is no defence against a device of the account that keeps lying.
/// Such a device can always mint `ours + 1` and win the next round, ceiling or
/// no ceiling, and no rule local to this module can stop it without making the
/// merge depend on what this device happens to know, which is exactly what makes
/// two planes diverge for ever. A member that squats on the ceiling itself is a
/// member to revoke; what the ceiling guarantees is that no single message can
/// make the arrangement unfixable by ARITHMETIC.
pub const MAX_SEQ: u32 = 1 << 24;

/// The most one placement may cost in the offered document, in bytes.
///
/// `MAX_SPOTS` alone does not bound it usefully: a spot key is a 64 character
/// node_id, a slash and a monitor id, so 256 spots of maximal monitor ids come
/// to about 88 KiB and `peers.send` carries 64. A byte bound is a function of
/// the content alone, so every device refuses the same placement, which is what
/// keeps the refusal from splitting the fleet.
///
/// Chosen so that the count and the byte bound do not contradict each other for
/// anything real: a full 256 spots with monitor ids of the ordinary sort
/// (`DP-1:9f3a1c7d`, about 14 bytes) comes to some 28 KiB and fits. The pair
/// that does not fit is 256 spots of MAXIMAL ids, and no bound could make that
/// fit a 64 KiB transport.
pub const MAX_PLACEMENT_BYTES: usize = 32 * 1024;

/// The most the monitors half of an offered document may cost, in bytes.
///
/// With [`MAX_PLACEMENT_BYTES`] this keeps the whole document inside one
/// `peers.send` by construction rather than by hope. It is a budget on what is
/// SENT and never on what is stored, and the difference is the point: an entry
/// the budget sheds is one this device declines to RELAY, while every device
/// still announces its own word itself, to every peer it can reach directly. A
/// budget applied at the door instead would let two devices refuse different
/// entries and hold different planes for ever.
const MONITORS_BUDGET: usize = 24 * 1024;

/// The bounds a monitor's geometry must satisfy to be part of a plane. Not
/// taste: a plane is arithmetic on `i32`, and a rectangle whose corner overflows
/// makes the crossing graph nonsense. Well beyond any real display wall.
pub const MAX_EXTENT: i32 = 1_000_000;

/// Where a monitor sits on the shared plane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Spot {
    pub x: i32,
    pub y: i32,
}

/// One device's signed word about its own monitors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Monitors {
    pub seq: u32,
    pub list: Vec<Monitor>,
    pub sig: String,
    /// Did the signature verify under a key this device has pinned? An entry
    /// that did not is held and shown as pending at most: never counted in the
    /// plane id, never used to derive a placement, never crossed to.
    pub verified: bool,
}

/// The arrangement a human dragged, one object for the whole plane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placement {
    pub seq: u32,
    /// The node_id that dragged. Also the signer, and the tiebreak of the total
    /// order.
    pub by: String,
    /// Informational only, for the interface. NEVER compared: clocks across
    /// machines are not comparable, and a design that quietly relies on them is
    /// a design that fails at the worst moment.
    pub at: u64,
    /// Keyed `<node_id>/<monitor id>`.
    pub spots: BTreeMap<String, Spot>,
    pub sig: String,
    /// Did the signature verify under `by`'s pinned key? An arrangement is
    /// adopted whether it did or not (see [`Plane::absorb_placement`]); the flag
    /// is what lets [`Plane::pin`] throw out a forgery the moment the real key
    /// arrives, and what the interface uses to say who arranged the plane rather
    /// than who claims to have.
    pub verified: bool,
}

impl Placement {
    /// The total order that decides last writer wins: higher `seq`, then greater
    /// `by`, then greater `sig`.
    ///
    /// **The signature is in the order, and it has to be.** `(seq, by)` is a
    /// total order on the PAIR, not on placements: two DIFFERENT arrangements
    /// from the same author at the same seq never displace each other, so the
    /// first to arrive wins and two peers who saw them in opposite orders hold
    /// different planes with different plane ids for ever. Every session between
    /// them is then refused `PLANE_STALE` with nothing to repair it, because each
    /// keeps re-offering a document the other ignores. `sig` is a function of the
    /// content (one fixed encoding, one key), so adding it makes the order total
    /// on the thing being ordered rather than on a projection of it. It is
    /// deliberately LAST: it decides only where a human's rank and the author
    /// cannot.
    ///
    /// `verified` is deliberately NOT in the order, and that was weighed: it
    /// would let a human's drag win a tie against a squatter at the ceiling, but
    /// it is this device's own knowledge rather than a fact about the two
    /// placements, so two devices that have met different peers would order the
    /// same pair differently. That is the one thing this order exists to prevent.
    fn outranks(&self, other: &Placement) -> bool {
        (self.seq, self.by.as_str(), self.sig.as_str())
            > (other.seq, other.by.as_str(), other.sig.as_str())
    }
}

/// The whole layout document, as this device holds it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plane {
    /// Each device's own word about its own screens, keyed by node_id.
    pub monitors: BTreeMap<String, Monitors>,
    /// The arrangement, if anyone has ever dragged one.
    pub placement: Option<Placement>,
    /// The engine keys this device has pinned, keyed by node_id. A pin is
    /// created or replaced ONLY by a channel-authenticated message from that
    /// device (the sync engine's doctrine): a `peer.message` arrives named by
    /// the receiving Core's directory, never by the sender's claim, so a key
    /// received directly from a device is that device's word.
    pub pins: BTreeMap<String, String>,
    /// This device's OWN node_id, once it knows it, and empty until then.
    ///
    /// The engine has to know its own name here, because a peer's document can
    /// carry an entry for it and an entry for our own screens from anybody but us
    /// is not a statement about anything: we are the only author of our own word.
    /// Without this the trick works and is permanent, because it needs one
    /// message: an entry planted for our node_id at a high seq erases our screens
    /// from a sibling's plane and holds out every entry we sign afterwards.
    ///
    /// Not persisted, deliberately: a file is nobody's word about who this device
    /// is. [`Plane::set_own`] is called from the orchestrator with the name the
    /// Core's directory gives, and [`Plane::publish_monitors`] and
    /// [`Plane::place`] set it too, neither being callable without it.
    pub own: String,
}

/// What changed when a document was merged, so the caller knows whether to
/// persist, to re-offer, and to tell the interface.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Merged {
    /// Something in the plane changed: persist it and offer it onward.
    pub changed: bool,
    /// A monitors entry was refused for a signature that did not verify, or a
    /// bound that was broken. Worth a debug log and nothing else: a peer is
    /// semi-trusted and a refused entry costs it nothing here.
    pub refused: bool,
}

impl Plane {
    /// Pins a peer's engine key, learned from a channel-authenticated message.
    ///
    /// A DIFFERENT key from the same device replaces the pin (the engine was
    /// reinstalled) and retires what the old key signed: that device's monitors
    /// entry becomes unverified until it says it again.
    ///
    /// A pin also RE-CHECKS an arrangement this device adopted ON FAITH, and this
    /// is the closing of the loop that lets an unverifiable placement be adopted
    /// in the first place: one attributed to this device that does not verify
    /// under the key the device itself just handed over was a forgery, and it is
    /// thrown out here. The plane then falls back to the derived arrangement,
    /// which a single drag replaces, so the outcome of a forgery is a screen
    /// briefly in the wrong place rather than a plane nobody can fix.
    ///
    /// **The re-examination is symmetric: a first pin and a replacement do the
    /// same thing to an arrangement, and that symmetry is a fix rather than a
    /// simplification.** An earlier draft (and D11 as first written) let an
    /// arrangement this device had ALREADY verified survive a key replacement
    /// untouched, on the reasoning that it was proven genuine under the key that
    /// was current when it arrived. The comfort was real and the cost was
    /// convergence, which is the property everything else here rests on: whether
    /// an arrangement is verified depends on whether THIS device held the
    /// author's old key when it arrived, so the same three facts (an arrangement,
    /// a key, its replacement) in two different orders gave two different planes
    /// and two different plane ids. Its realistic form is a device reinstalling
    /// and the account splitting into the devices that had verified its
    /// arrangement and those that had adopted it on faith, with every session
    /// across the split refused until a human drags again, which is exactly what
    /// D11 exists to prevent.
    ///
    /// So the invariant is stated once and re-established by every event: **if
    /// this device holds a key for the arrangement's author, the arrangement
    /// verifies under it; otherwise the arrangement is held on faith.** The door
    /// ([`Plane::absorb_placement`]) enforces it on arrival and this enforces it
    /// on a pin, which is what makes the outcome a function of the facts rather
    /// than of their order. The alternative considered was to keep a disproven
    /// arrangement and merely mark it unverified: it is more convergent for the
    /// pin taken alone, and LESS convergent overall, because the door already
    /// refuses such an arrangement outright and the two rules would then disagree
    /// permanently about the same document.
    ///
    /// Returns whether anything changed.
    pub fn pin(&mut self, node_id: &str, key: &str) -> bool {
        if !is_hex64(node_id) || !is_hex64(key) {
            return false;
        }
        match self.pins.get(node_id) {
            Some(known) if known == key => false,
            _ => {
                self.pins.insert(node_id.to_string(), key.to_string());
                if let Some(entry) = self.monitors.get_mut(node_id) {
                    let bytes = monitors_bytes(node_id, entry.seq, &entry.list);
                    entry.verified = verify(key, &bytes, &entry.sig);
                }
                if let Some(placement) = self.placement.as_mut()
                    && placement.by == node_id
                {
                    let bytes = placement_bytes(node_id, placement.seq, &placement.spots);
                    placement.verified = verify(key, &bytes, &placement.sig);
                    if !placement.verified {
                        // Loud, because it is the one thing in this module that
                        // can only be a device of the account misbehaving.
                        eprintln!(
                            "[1device-input] an arrangement attributed to a device does not \
                             verify under the key that device published: discarded"
                        );
                        self.placement = None;
                    }
                }
                true
            }
        }
    }

    /// Tells the plane this device's own node_id (see [`Plane::own`]).
    ///
    /// Also retires an entry stored for our own node_id that we cannot have
    /// written, which closes the one window the drop rule in
    /// [`Plane::absorb_monitors`] leaves open: a document that arrived before the
    /// Core had named this device. Our own entry is always verified (we hold our
    /// own key), so an unverified one under our own name is somebody else's claim
    /// about our screens and nothing else.
    ///
    /// Returns whether anything changed.
    pub fn set_own(&mut self, node_id: &str) -> bool {
        if !is_hex64(node_id) {
            return false;
        }
        let mut changed = false;
        if self.own != node_id {
            self.own = node_id.to_string();
            changed = true;
        }
        if self.monitors.get(node_id).is_some_and(|e| !e.verified) {
            eprintln!(
                "[1device-input] dropping a monitors entry a peer planted under this device's \
                 own name: nobody else writes our own screens"
            );
            self.monitors.remove(node_id);
            changed = true;
        }
        changed
    }

    /// Publishes THIS device's own monitors, signed, at `seq`. The one place a
    /// device writes its own entry, and no peer can write it.
    ///
    /// **The local backend's list gets exactly the sanity a peer's does**, which
    /// it did not at first, and the consequences were not theoretical. A monitor
    /// of `w: 0, h: 0` (what X11 RandR reports for a disabled CRTC) reached the
    /// crossing graph and panicked its clamps with `min > max`, taking the
    /// component down. And an id over the old 128 byte bound made this device's
    /// screens absent from every sibling's plane with nothing said, because one
    /// unusable monitor refuses the whole entry. So a monitor that cannot be part
    /// of a plane is dropped HERE, named in a warning, and the rest of the list
    /// still travels.
    pub fn publish_monitors(
        &mut self,
        node_id: &str,
        identity: &Identity,
        list: Vec<Monitor>,
    ) -> bool {
        // Our own name is checked like anybody's: a `device_id` passed here by
        // mistake would mint an entry under a key every peer refuses, and say
        // nothing locally.
        if !is_hex64(node_id) {
            eprintln!(
                "[1device-input] refusing to publish monitors under a name that is not a node_id"
            );
            return false;
        }
        self.set_own(node_id);
        let list: Vec<Monitor> = list
            .into_iter()
            .filter_map(|m| {
                let m = Monitor {
                    // Cosmetic, so it is cut rather than allowed to cost the
                    // machine its place on the plane.
                    name: truncated(&m.name, MAX_NAME),
                    ..m
                };
                if is_sane(&m) {
                    return Some(m);
                }
                eprintln!(
                    "[1device-input] this machine reported a monitor a plane cannot hold \
                     ({}x{} at {},{}, id of {} bytes): left out of the layout",
                    m.w,
                    m.h,
                    m.x,
                    m.y,
                    m.id.len()
                );
                None
            })
            .take(MAX_MONITORS)
            .collect();
        // Same monitors as last time: no new seq, nothing to say. Without this a
        // restart or a spurious change notification would bump the seq on every
        // sibling and set off a round for nothing.
        let seq = match self.monitors.get(node_id) {
            Some(entry) if entry.list == list && entry.verified => return false,
            // Clamped at the ceiling rather than saturated. The clamp is
            // reachable only through an entry somebody else planted (sixteen
            // million monitor changes is not a desk), and at the ceiling our own
            // word still wins, because a verified entry outranks an unverified one
            // at equal seq and nobody but us can sign ours.
            Some(entry) => entry.seq.saturating_add(1).min(MAX_SEQ),
            None => 1,
        };
        let bytes = monitors_bytes(node_id, seq, &list);
        let sig = identity.sign(&bytes);
        // Our own key is pinned by construction: it is the one key we do not
        // learn from anybody.
        self.pins.insert(node_id.to_string(), identity.public_hex());
        self.monitors.insert(
            node_id.to_string(),
            Monitors {
                seq,
                list,
                sig,
                verified: true,
            },
        );
        true
    }

    /// Writes a new placement: the arrangement a human dragged, at
    /// `max(seen) + 1` so it outranks everything this device knows about.
    ///
    /// `spots` is taken as given and bounded; a spot naming a monitor nobody
    /// claims is KEPT, because a screen that is away keeps its place and its
    /// device may be offline for a week.
    pub fn place(
        &mut self,
        node_id: &str,
        identity: &Identity,
        spots: BTreeMap<String, Spot>,
    ) -> Result<(), PlaceError> {
        // Checked like anybody's name, for the same reason: a `device_id` passed
        // here by mistake would sign a `by` every peer refuses and report success.
        if !is_hex64(node_id) {
            return Err(PlaceError::BadAuthor);
        }
        if spots.len() > MAX_SPOTS {
            return Err(PlaceError::TooMany);
        }
        for (key, spot) in &spots {
            if split_spot_key(key).is_none() {
                return Err(PlaceError::BadKey);
            }
            if !in_extent(spot.x) || !in_extent(spot.y) {
                return Err(PlaceError::OffPlane);
            }
        }
        if spots_bytes(&spots) > MAX_PLACEMENT_BYTES {
            return Err(PlaceError::TooBig);
        }
        self.set_own(node_id);
        let seq = self
            .placement
            .as_ref()
            .map_or(1, |p| p.seq.saturating_add(1).min(MAX_SEQ));
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let bytes = placement_bytes(node_id, seq, &spots);
        let sig = identity.sign(&bytes);
        let candidate = Placement {
            seq,
            by: node_id.to_string(),
            at,
            spots,
            sig,
            verified: true,
        };
        // A drag that does not outrank what it replaces is not a drag: it would be
        // overwritten by the next sweep and the human would watch their
        // arrangement snap back with nothing said. Only reachable at the ceiling,
        // where a peer has claimed the highest rank there is, and there the honest
        // answer is a refusal the interface can put a sentence on.
        if let Some(held) = &self.placement
            && !candidate.outranks(held)
        {
            return Err(PlaceError::Ceiling);
        }
        self.pins.insert(node_id.to_string(), identity.public_hex());
        self.placement = Some(candidate);
        Ok(())
    }

    /// Merges a document received from `from` (the node_id our own Core named as
    /// the sender: never a claim inside the message).
    ///
    /// Idempotent, and a function of the two documents AND OF THE PINS this
    /// device holds. The old wording here ("a function of the two documents
    /// alone") was not true and the difference is not pedantry: the pins decide
    /// which statements verify, and `verified` decides whether a monitors entry is
    /// part of the plane at all. What IS guaranteed, and what convergence rests
    /// on, is that two devices holding the same pins reach the same plane from the
    /// same messages in any order, and that pins converge because every device of
    /// the account hands its own key to every peer it speaks to.
    ///
    /// **A document that breaks a bound or a shape is refused WHOLE**, and it is
    /// parsed entirely before anything is applied so that "whole" is true rather
    /// than aspirational. The first shape absorbed entry by entry and only then
    /// refused the map, so a peer over the bound got its placement in while its
    /// monitors were refused, and the placement is the half that matters (it is
    /// the half a planted `seq` freezes).
    ///
    /// A failed SIGNATURE is not a bound: it refuses that one statement and leaves
    /// the rest of the document standing. Every statement in here is separately
    /// signed by a different device, and dropping A's genuine word about its
    /// screens because B lied about the arrangement would let one peer suppress
    /// another's.
    pub fn merge(&mut self, from: &str, doc: &Value) -> Merged {
        let mut out = Merged::default();
        if doc.get("d").and_then(Value::as_str) != Some(crate::wire::DIALECT) {
            return out;
        }
        // The sender's own key, if it named one. Direct contact, so this is that
        // device's word about itself and it creates or replaces the pin.
        //
        // Taken BEFORE the document is judged, and kept even when the document is
        // refused whole: the key is not part of the bounded content, and a peer
        // must not be able to keep itself unpinned by malforming the rest of what
        // it says. The pin is what closes D11's loop.
        if let Some(key) = doc.get("key").and_then(Value::as_str)
            && self.pin(from, key)
        {
            out.changed = true;
        }
        // ---- parse, and refuse the whole document if anything does not parse.
        let mut entries: Vec<(String, Monitors)> = Vec::new();
        match doc.get("monitors") {
            None | Some(Value::Null) => {}
            Some(Value::Object(monitors)) => {
                // A document naming more devices than the bound is refused WHOLE:
                // taking the first sixteen would make the merge depend on the
                // map's order, and the merge must be a function of the content.
                if monitors.len() > MAX_DEVICES {
                    out.refused = true;
                    return out;
                }
                for (node_id, entry) in monitors {
                    match self.read_monitors(node_id, entry) {
                        Some(parsed) => entries.push((node_id.clone(), parsed)),
                        None => {
                            out.refused = true;
                            return out;
                        }
                    }
                }
            }
            // Present and not a map: not a document.
            Some(_) => {
                out.refused = true;
                return out;
            }
        }
        let placement = match doc.get("placement") {
            None | Some(Value::Null) => None,
            Some(value) => match placement_from(value) {
                Some(parsed) if spots_bytes(&parsed.spots) <= MAX_PLACEMENT_BYTES => Some(parsed),
                _ => {
                    out.refused = true;
                    return out;
                }
            },
        };
        // ---- apply. Nothing below can refuse the document: what it can do is
        // decline one statement, for want of room or want of rank.
        for (node_id, entry) in entries {
            match self.absorb_monitors(&node_id, entry) {
                Absorbed::Changed => out.changed = true,
                Absorbed::Declined => out.refused = true,
                Absorbed::Ignored => {}
            }
        }
        if let Some(placement) = placement {
            match self.absorb_placement(placement) {
                Absorbed::Changed => out.changed = true,
                Absorbed::Declined => out.refused = true,
                Absorbed::Ignored => {}
            }
        }
        out
    }

    /// Parses one monitors entry and decides whether this device can vouch for it.
    /// `None` means the entry breaks a bound or a shape, which refuses the whole
    /// document it came in. Reads nothing but the pins, writes nothing, so a
    /// document can be judged before any of it is applied.
    fn read_monitors(&self, node_id: &str, entry: &Value) -> Option<Monitors> {
        if !is_hex64(node_id) {
            return None;
        }
        let seq = entry.get("seq")?.as_u64().and_then(u32_of)?;
        // The ceiling, on this authority too: an entry planted at `u32::MAX` for a
        // real device holds that device's own screens out of the plane for ever,
        // and the entry that holds them out can never be verified because nobody
        // signed it.
        if seq > MAX_SEQ {
            return None;
        }
        let sig = entry.get("sig")?.as_str()?;
        let list = entry.get("list")?.as_array()?;
        if list.len() > MAX_MONITORS {
            return None;
        }
        let mut monitors = Vec::with_capacity(list.len());
        for m in list {
            monitors.push(monitor_from(m)?);
        }
        // Its own word about its own screens, so it is only worth anything under
        // that device's key. A key we have not pinned yet leaves the entry
        // UNVERIFIED: held, shown as pending at most, never crossed to.
        let bytes = monitors_bytes(node_id, seq, &monitors);
        let verified = self
            .pins
            .get(node_id)
            .is_some_and(|key| verify(key, &bytes, sig));
        Some(Monitors {
            seq,
            list: monitors,
            sig: sig.to_string(),
            verified,
        })
    }

    /// Stores a parsed entry if it is worth more than the one held, and finds it
    /// room if the table is full.
    fn absorb_monitors(&mut self, node_id: &str, candidate: Monitors) -> Absorbed {
        // Our own word about our own screens is ours alone, so an entry under our
        // own name arriving from anywhere else is not a statement about anything.
        // Dropping it here is what stops the one trick that would otherwise be
        // permanent AND remote: `monitors[<us>] = { seq: <huge>, list: [] }` erases
        // our screens from a sibling's plane and holds out every entry we sign
        // afterwards.
        if !self.own.is_empty() && node_id == self.own {
            return Absorbed::Ignored;
        }
        match self.monitors.get(node_id) {
            // A verified entry is that device's proven word; an unverified one is
            // somebody's claim about it. No seq makes a claim outrank a proof, and
            // no seq keeps a proof out: without these two lines one message at a
            // high seq blocks a device's real screens for ever, and the entry doing
            // the blocking can never be verified because nobody signed it.
            Some(held) if held.verified && !candidate.verified => Absorbed::Ignored,
            Some(held) if !held.verified && candidate.verified => {
                self.monitors.insert(node_id.to_string(), candidate);
                Absorbed::Changed
            }
            // A seq we already have, or an older one: nothing to learn. The equal
            // case matters as much as the older one, because a round is epidemic
            // and the same entry comes back from every peer.
            Some(held) if held.seq >= candidate.seq => Absorbed::Ignored,
            Some(_) => {
                self.monitors.insert(node_id.to_string(), candidate);
                Absorbed::Changed
            }
            None if self.monitors.len() >= MAX_DEVICES => {
                // The table is full, and this is a device it does not have. A
                // VERIFIED entry is part of the plane and of the plane id, so it is
                // never displaced for a newcomer. An unverified one is part of
                // neither (excluded from both by construction), so it may be, and
                // that is what keeps sixteen fabricated names from blocking every
                // real device of the account for ever.
                //
                // Two rules, and the first is the one that matters. A PROOF always
                // finds room, because a fabricated name must never be able to keep a
                // real device's screens out of the plane: an attacker picks node_ids
                // freely, so a rule that only displaced GREATER names would simply be
                // attacked with the sixteen smallest. A CLAIM only displaces a
                // greater claim, so the unverified half of the table keeps the
                // smallest names, which is deterministic and, more importantly,
                // TERMINATING: a rule that swapped an entry out on every arrival
                // would make an epidemic that never stops, since each swap is a
                // change and a change sends a round. Displacing a claim with a proof
                // terminates too, there being at most sixteen claims to displace and
                // no way back.
                let displaceable = self
                    .monitors
                    .iter()
                    .filter(|(_, held)| !held.verified)
                    .map(|(key, _)| key.clone())
                    .max();
                match displaceable {
                    Some(worst) if candidate.verified || node_id < worst.as_str() => {
                        self.monitors.remove(&worst);
                        self.monitors.insert(node_id.to_string(), candidate);
                        Absorbed::Changed
                    }
                    _ => Absorbed::Declined,
                }
            }
            None => {
                self.monitors.insert(node_id.to_string(), candidate);
                Absorbed::Changed
            }
        }
    }

    /// Absorbs a placement if it outranks the one held.
    ///
    /// A placement whose author's key this device does not hold is ADOPTED
    /// UNVERIFIED, and that is a deliberate divergence from the sync engine's
    /// fail-closed pinning (doc/input-sharing.md, D11). The plane authorizes
    /// nothing (the grants do), so the worst a forged arrangement can do is put a
    /// screen in the wrong place, which a human sees and fixes with one drag,
    /// while refusing to converge would break sessions between two innocent
    /// devices because a third is absent: its signature unverifiable, its
    /// arrangement therefore unusable, and every session between the two refused
    /// `PLANE_STALE` for ever.
    ///
    /// What DOES fail closed is a signature that is present and wrong: an
    /// arrangement attributed to a device whose key this one holds must verify
    /// under it, or it is refused here, and one adopted before that key arrived
    /// is thrown out by [`Plane::pin`].
    ///
    /// An earlier draft also required the message to come from a peer this device
    /// had pinned. That check was removed because it is vacuous: a peer's key
    /// arrives in its own message, so any device of the account makes itself
    /// pinned by the act of speaking, and the condition could never refuse
    /// anybody. Every sender is an attested device of the account already, which
    /// the Core guarantees before a byte reaches this engine, and that is the
    /// real bound on who may say anything at all here.
    fn absorb_placement(&mut self, mut candidate: Placement) -> Absorbed {
        let bytes = placement_bytes(&candidate.by, candidate.seq, &candidate.spots);
        match self.pins.get(&candidate.by) {
            // Present and WRONG: the one thing that fails closed here. This is
            // also the door half of the invariant [`Plane::pin`] states, and the
            // reason the pin re-examines an arrangement whether it was proven
            // before or not: the two must agree about the same document or they
            // disagree for ever.
            Some(key) if !verify(key, &bytes, &candidate.sig) => return Absorbed::Declined,
            Some(_) => candidate.verified = true,
            None => candidate.verified = false,
        }
        match &self.placement {
            Some(held) if !candidate.outranks(held) => Absorbed::Ignored,
            _ => {
                self.placement = Some(candidate);
                Absorbed::Changed
            }
        }
    }

    /// The document to send, `peers.send`-shaped. Carries this device's own
    /// engine key, which is what lets the receiver pin it: the message arrives
    /// named by the receiver's own Core, so the key inside it is this device's
    /// word about itself.
    ///
    /// **Two things are shed here, and both are additions rather than losses.**
    ///
    /// An UNVERIFIED entry is not offered, because relaying one is provably
    /// useless: a pin is only ever created by a message that came from the device
    /// itself, and that message carries that device's own entry, so an entry this
    /// device cannot verify can never become useful on the far side before its
    /// author's own word arrives there directly. It is held (the interface may
    /// show it as pending) and it is not passed on.
    ///
    /// And what is verified is offered while it fits [`MONITORS_BUDGET`], in
    /// node_id order, with OUR OWN entry first and unconditionally. Being unable
    /// to announce our own screens is the one failure nobody else can repair for
    /// us; relaying somebody else's word is a convenience the epidemic does not
    /// depend on. That is what makes "the document fits in one `peers.send`" true
    /// by construction instead of true for realistic desks.
    pub fn offer(&self, own_key: &str) -> Value {
        let mut monitors = serde_json::Map::new();
        let mut left = MONITORS_BUDGET;
        let mut offer_entry = |node_id: &String, entry: &Monitors, left: &mut usize| {
            let value = entry_json(entry);
            // The key, the value, the quotes, the colon and the comma: what this
            // entry will really cost in the document, counted rather than guessed.
            let cost = node_id.len() + value.to_string().len() + 4;
            if cost > *left {
                return;
            }
            *left -= cost;
            monitors.insert(node_id.clone(), value);
        };
        if let Some(entry) = self.monitors.get(&self.own) {
            offer_entry(&self.own, entry, &mut left);
        }
        for (node_id, entry) in &self.monitors {
            if *node_id == self.own || !entry.verified {
                continue;
            }
            offer_entry(node_id, entry, &mut left);
        }
        let mut doc = json!({
            "d": crate::wire::DIALECT,
            "t": "layout",
            "key": own_key,
            "monitors": Value::Object(monitors),
        });
        if let Some(p) = &self.placement {
            doc["placement"] = placement_json(p);
        }
        doc
    }

    /// The document as it is persisted, and its inverse [`Plane::from_stored`].
    ///
    /// Not [`Plane::offer`] with the pins bolted on, which it used to be: a file
    /// has no 64 KiB budget and no reason to shed anything, so this writes
    /// everything held, including the entries that are only pending, plus the pins
    /// and the per-entry verified flag, which are this device's own knowledge and
    /// travel nowhere. `own` is deliberately absent: a file is nobody's word about
    /// who this device is.
    pub fn to_stored(&self) -> Value {
        let mut monitors = serde_json::Map::new();
        for (node_id, entry) in &self.monitors {
            let mut value = entry_json(entry);
            value["verified"] = json!(entry.verified);
            monitors.insert(node_id.clone(), value);
        }
        let mut doc = json!({
            "d": crate::wire::DIALECT,
            "t": "layout",
            "monitors": Value::Object(monitors),
            "pins": self.pins,
        });
        if let Some(p) = &self.placement {
            doc["placement"] = placement_json(p);
        }
        doc
    }

    /// Reads back what [`Plane::to_stored`] wrote. Anything unreadable yields an
    /// empty plane rather than an error: unlike the grants, a plane can be
    /// rebuilt by one round with any peer, and refusing to start over a file
    /// that will be replaced in seconds would be strictness with no payoff.
    /// ([`crate::store::Store::load_plane`] is the other half of that promise,
    /// and it was missing at first: this function claimed a leniency the store
    /// did not give it, so a torn `plane.json` kept the whole component down.)
    /// The verified flags are RE-DERIVED from the pins rather than trusted, so a
    /// hand-edited file cannot promote an entry.
    pub fn from_stored(doc: &Value) -> Plane {
        let mut plane = Plane::default();
        if let Some(pins) = doc.get("pins").and_then(Value::as_object) {
            for (node_id, key) in pins {
                if let Some(key) = key.as_str() {
                    plane.pin(node_id, key);
                }
            }
        }
        // Merged through the ordinary path, so a stored document gets exactly the
        // checks a received one does: one set of rules, one place. The `key`
        // field is stripped because a file is nobody's word: the pins above are
        // this device's own record of who said what, and a hand-edited `key`
        // must not be able to create one.
        let mut as_received = doc.clone();
        as_received["d"] = json!(crate::wire::DIALECT);
        if let Some(object) = as_received.as_object_mut() {
            object.remove("key");
        }
        // The sender's name decides nothing about a stored document (it pins
        // nothing and the placement's rank does not depend on the courier), so
        // one pass under an empty name is the whole load.
        plane.merge("", &as_received);
        plane
    }
}

/// What became of one statement of a document. A statement is DECLINED, never
/// the document: a document that breaks a bound or a shape never reaches here
/// (see [`Plane::merge`]).
enum Absorbed {
    /// Stored: the plane changed.
    Changed,
    /// Nothing to learn: an older or equal rank, a claim against a proof, or our
    /// own name in somebody else's mouth.
    Ignored,
    /// Refused: the table had no room it was allowed to make, or the signature was
    /// present and wrong. Worth a debug line and nothing else.
    Declined,
}

/// Why a placement was refused at the door (a local gesture, so it gets a real
/// error rather than a silent drop).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaceError {
    /// More spots than the plane's bound.
    TooMany,
    /// A key that is not `<node_id>/<monitor id>`.
    BadKey,
    /// A coordinate outside the plane's arithmetic bounds.
    OffPlane,
    /// More bytes than one `peers.send` can carry (see [`MAX_PLACEMENT_BYTES`]):
    /// an arrangement nobody could replicate is not an arrangement.
    TooBig,
    /// The author's name is not a node_id. Almost always a `device_id` passed by
    /// mistake, which used to produce a `by` every peer refused with nothing said
    /// locally.
    BadAuthor,
    /// The held arrangement is at [`MAX_SEQ`], so this drag cannot outrank it.
    /// Only reachable when a peer has claimed the highest rank there is; the
    /// interface owes a sentence rather than a silent snap back.
    Ceiling,
}

/// Splits a spot key into its node_id and monitor id. `None` for anything that
/// is not exactly one of each, which is what keeps a hostile key from being read
/// as a node_id with a slash in it.
pub fn split_spot_key(key: &str) -> Option<(&str, &str)> {
    let (node_id, monitor) = key.split_once('/')?;
    if !is_hex64(node_id) || monitor.is_empty() || monitor.len() > MAX_ID {
        return None;
    }
    // A slash would make this key ambiguous; a `|` would make the GUARD key built
    // from it ambiguous (`settings.rs`), and a guard that stops matching on the
    // next restart is a wall a human asked for that quietly stops holding. Both
    // are refused here as well as in [`is_sane`], because a spot can arrive in a
    // placement without any monitor record ever claiming it.
    if monitor.contains('/') || monitor.contains('|') {
        return None;
    }
    Some((node_id, monitor))
}

/// The spot key for a device's monitor.
pub fn spot_key(node_id: &str, monitor_id: &str) -> String {
    format!("{node_id}/{monitor_id}")
}

/// The signed bytes of a monitors entry (doc/input-sharing.md, section 6).
/// Length prefixes everywhere, so no two field values can be confused for one
/// another, and a fixed order, so the encoding is a function of the content.
fn monitors_bytes(node_id: &str, seq: u32, list: &[Monitor]) -> Vec<u8> {
    let mut out = Vec::with_capacity(DOMAIN_MONITORS.len() + 64 + list.len() * 64);
    out.extend_from_slice(DOMAIN_MONITORS);
    out.extend_from_slice(node_id.as_bytes());
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(&(list.len() as u32).to_be_bytes());
    for m in list {
        out.extend_from_slice(&(m.id.len() as u16).to_be_bytes());
        out.extend_from_slice(m.id.as_bytes());
        out.extend_from_slice(&(m.name.len() as u16).to_be_bytes());
        out.extend_from_slice(m.name.as_bytes());
        for v in [m.w, m.h, m.x, m.y, m.scale] {
            out.extend_from_slice(&v.to_be_bytes());
        }
        out.push(u8::from(m.primary));
    }
    out
}

/// The signed bytes of a placement. The spots are in the map's key order, which
/// is bytewise ascending, so the encoding does not depend on how the document
/// was built.
fn placement_bytes(by: &str, seq: u32, spots: &BTreeMap<String, Spot>) -> Vec<u8> {
    let mut out = Vec::with_capacity(DOMAIN_PLACEMENT.len() + 64 + spots.len() * 48);
    out.extend_from_slice(DOMAIN_PLACEMENT);
    out.extend_from_slice(by.as_bytes());
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(&(spots.len() as u32).to_be_bytes());
    for (key, spot) in spots {
        out.extend_from_slice(&(key.len() as u16).to_be_bytes());
        out.extend_from_slice(key.as_bytes());
        out.extend_from_slice(&spot.x.to_be_bytes());
        out.extend_from_slice(&spot.y.to_be_bytes());
    }
    out
}

/// The plane id: one hash over the signed bytes of everything that decides where
/// a pointer goes, so two devices holding the same records compute the same id
/// and a `start` frame's absolute coordinates cannot be misread.
///
/// UNVERIFIED monitors entries are excluded, and that is the subtle part: they
/// are not crossed to either, so including them would make two devices disagree
/// on the id purely because one of them has met a third device and the other has
/// not.
pub fn plane_id(plane: &Plane) -> String {
    let mut hasher = blake3::Hasher::new();
    match &plane.placement {
        Some(p) => {
            hasher.update(&placement_bytes(&p.by, p.seq, &p.spots));
        }
        // A distinct, non-empty marker for "nobody has ever dragged": an empty
        // update is indistinguishable from a placement with no spots, and those
        // are different planes.
        None => {
            hasher.update(b"1device-input:no-placement");
        }
    }
    for (node_id, entry) in &plane.monitors {
        if entry.verified {
            hasher.update(&monitors_bytes(node_id, entry.seq, &entry.list));
        }
    }
    hex::encode(&hasher.finalize().as_bytes()[..16])
}

fn monitor_to(m: &Monitor) -> Value {
    json!({ "id": m.id, "name": m.name, "w": m.w, "h": m.h, "x": m.x, "y": m.y,
            "scale": m.scale, "primary": m.primary })
}

/// One monitors entry as both the wire and the file write it.
fn entry_json(entry: &Monitors) -> Value {
    json!({
        "seq": entry.seq,
        "list": entry.list.iter().map(monitor_to).collect::<Vec<Value>>(),
        "sig": entry.sig,
    })
}

/// One placement as both the wire and the file write it.
fn placement_json(p: &Placement) -> Value {
    let mut spots = serde_json::Map::new();
    for (key, spot) in &p.spots {
        spots.insert(key.clone(), json!({ "x": spot.x, "y": spot.y }));
    }
    json!({
        "seq": p.seq,
        "by": p.by,
        "at": p.at,
        "spots": Value::Object(spots),
        "sig": p.sig,
    })
}

/// What a spot set will cost in a document, in bytes: the key, the two
/// coordinates and the punctuation around them. An upper bound, counted rather
/// than estimated, and a function of the content alone so that every device
/// refuses the same oversized placement.
fn spots_bytes(spots: &BTreeMap<String, Spot>) -> usize {
    spots
        .keys()
        .map(|key| key.len() + "\":{\"x\":-1000000,\"y\":-1000000},\"".len())
        .sum()
}

/// Cuts a string to `max` BYTES on a character boundary. Used on a monitor name,
/// which is cosmetic: cutting it keeps a screen on the plane where refusing it
/// would take a whole machine off every sibling's plane.
fn truncated(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Can this monitor be part of a plane at all?
///
/// The ONE set of rules, applied to a peer's word ([`monitor_from`]) and to this
/// machine's own backend ([`Plane::publish_monitors`]) alike. It was two, and the
/// local half had none of them: a disabled CRTC reported as `w: 0, h: 0` (which
/// is exactly what X11 RandR does) reached the crossing graph and panicked its
/// clamps with `min > max`, in release as well as debug, so one unplugged screen
/// took the component down.
pub fn is_sane(m: &Monitor) -> bool {
    // A spot key is `<node_id>/<monitor id>` and a guard key is
    // `<monitor key>|<monitor key>|<side>`, so neither separator can be inside an
    // id or a stored setting stops matching what it was set for.
    if m.id.is_empty() || m.id.len() > MAX_ID || m.id.contains('/') || m.id.contains('|') {
        return false;
    }
    if m.name.len() > MAX_NAME {
        return false;
    }
    // A monitor of no size is not a monitor, and a scale of zero or a negative one
    // is arithmetic waiting to divide by nothing.
    if m.w <= 0 || m.h <= 0 || m.scale <= 0 || m.scale > 100_000 {
        return false;
    }
    // The corners must stay inside the plane's arithmetic, or the crossing graph
    // is nonsense and an overflow is a panic in a debug build.
    if !in_extent(m.w) || !in_extent(m.h) || !in_extent(m.x) || !in_extent(m.y) {
        return false;
    }
    match (m.x.checked_add(m.w), m.y.checked_add(m.h)) {
        (Some(right), Some(bottom)) => in_extent(right) && in_extent(bottom),
        _ => false,
    }
}

/// Parses one monitor, bounding every field. `None` makes the whole entry
/// refused, which is the right blast radius: a device's word about its screens is
/// one statement, and half of it is not a statement.
fn monitor_from(v: &Value) -> Option<Monitor> {
    let field = |key: &str| -> Option<i32> { i32::try_from(v.get(key)?.as_i64()?).ok() };
    let m = Monitor {
        id: v.get("id")?.as_str()?.to_string(),
        name: v.get("name")?.as_str()?.to_string(),
        w: field("w")?,
        h: field("h")?,
        x: field("x")?,
        y: field("y")?,
        scale: field("scale")?,
        primary: v.get("primary").and_then(Value::as_bool).unwrap_or(false),
    };
    // A peer's word is REFUSED rather than trimmed: its bytes are signed, so a
    // trimmed copy would verify against nothing.
    if !is_sane(&m) {
        return None;
    }
    Some(m)
}

fn placement_from(v: &Value) -> Option<Placement> {
    let by = v.get("by")?.as_str()?;
    if !is_hex64(by) {
        return None;
    }
    let seq = u32_of(v.get("seq")?.as_u64()?)?;
    // The ceiling ([`MAX_SEQ`]), on this authority too and above all on this one:
    // it is what keeps a human's drag able to outrank a planted arrangement.
    if seq > MAX_SEQ {
        return None;
    }
    let sig = v.get("sig")?.as_str()?;
    let spots_of = v.get("spots")?.as_object()?;
    if spots_of.len() > MAX_SPOTS {
        return None;
    }
    let mut spots = BTreeMap::new();
    for (key, spot) in spots_of {
        split_spot_key(key)?;
        let x = i32::try_from(spot.get("x")?.as_i64()?).ok()?;
        let y = i32::try_from(spot.get("y")?.as_i64()?).ok()?;
        if !in_extent(x) || !in_extent(y) {
            return None;
        }
        spots.insert(key.clone(), Spot { x, y });
    }
    Some(Placement {
        seq,
        by: by.to_string(),
        // Informational, never compared, so a missing or absurd value costs
        // nothing.
        at: v.get("at").and_then(Value::as_u64).unwrap_or(0),
        spots,
        sig: sig.to_string(),
        // Set by the absorber, which is the only place that holds the pins.
        verified: false,
    })
}

fn in_extent(v: i32) -> bool {
    (-MAX_EXTENT..=MAX_EXTENT).contains(&v)
}

fn u32_of(v: u64) -> Option<u32> {
    u32::try_from(v).ok()
}

/// Is this 64 lowercase hex characters, the shape of a node_id and of an engine
/// key? Checked wherever one becomes a map key, so nothing else can be one.
pub fn is_hex64(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

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
            primary: true,
        }
    }

    /// A device's own word about its own screens travels, verifies under the key
    /// it published, and cannot be written by anybody else.
    #[test]
    fn a_device_is_the_only_author_of_its_own_monitors() {
        let (_dir, a_id) = identity();
        let (_dir2, b_id) = identity();
        let (a, b) = (key(1), key(2));

        let mut theirs = Plane::default();
        theirs.publish_monitors(&a, &a_id, vec![screen("DP-1", 0, 0, 2560, 1440)]);

        let mut mine = Plane::default();
        let merged = mine.merge(&a, &theirs.offer(&a_id.public_hex()));
        assert!(merged.changed && !merged.refused);
        assert!(
            mine.monitors[&a].verified,
            "the key came from the device itself, so its word verifies"
        );

        // B relays A's entry but signs nothing of it: still A's word, still
        // verified, because verification is about A's key and not about the
        // courier.
        let mut third = Plane::default();
        third.pin(&a, &a_id.public_hex());
        let relayed = third.merge(&b, &theirs.offer(&b_id.public_hex()));
        assert!(relayed.changed);
        assert!(third.monitors[&a].verified);

        // And B cannot forge A's entry: signed by B, verified against A's pinned
        // key, refused.
        let mut forged = Plane::default();
        forged.publish_monitors(&a, &b_id, vec![screen("EVIL", 0, 0, 100, 100)]);
        let mut victim = Plane::default();
        victim.pin(&a, &a_id.public_hex());
        victim.merge(&b, &forged.offer(&b_id.public_hex()));
        assert!(
            !victim.monitors[&a].verified,
            "a forged entry is held unverified, never accepted as that device's word"
        );
        assert!(
            !plane_id(&victim).contains("xx"),
            "and it contributes nothing to the plane id"
        );
        let mut empty = Plane::default();
        empty.pin(&a, &a_id.public_hex());
        assert_eq!(
            plane_id(&victim),
            plane_id(&empty),
            "an unverified entry is not part of the plane"
        );
    }

    /// The merge is idempotent, and three PLAIN messages (every key already
    /// pinned, nobody reinstalling, one arrangement) reach the same plane in every
    /// order.
    ///
    /// Named for what it covers, which is narrower than it looks: it passed while
    /// two placements of equal rank could not displace each other and while a key
    /// replacement made the plane depend on arrival order. The two properties it
    /// does NOT reach are worth their own tests, and have them
    /// ([`two_arrangements_of_equal_rank_and_different_spots_converge_in_either_order`]
    /// and [`two_independent_planes_converge_from_the_same_facts_in_any_order`]).
    #[test]
    fn the_merge_is_idempotent_and_three_plain_messages_agree_in_any_order() {
        let (_d1, a_id) = identity();
        let (_d2, b_id) = identity();
        let (a, b) = (key(0xa), key(0xb));

        let mut from_a = Plane::default();
        from_a.publish_monitors(&a, &a_id, vec![screen("A1", 0, 0, 1920, 1080)]);
        let mut from_b = Plane::default();
        from_b.publish_monitors(&b, &b_id, vec![screen("B1", 0, 0, 2560, 1440)]);
        let mut dragged = from_b.clone();
        dragged.pin(&a, &a_id.public_hex());
        dragged
            .place(
                &b,
                &b_id,
                BTreeMap::from([
                    (spot_key(&a, "A1"), Spot { x: -1920, y: 0 }),
                    (spot_key(&b, "B1"), Spot { x: 0, y: 0 }),
                ]),
            )
            .expect("place");

        let messages = [
            (a.clone(), from_a.offer(&a_id.public_hex())),
            (b.clone(), from_b.offer(&b_id.public_hex())),
            (b.clone(), dragged.offer(&b_id.public_hex())),
        ];
        let mut ids = Vec::new();
        for order in [[0, 1, 2], [2, 1, 0], [1, 2, 0], [2, 0, 1]] {
            let mut plane = Plane::default();
            for i in order {
                let (from, doc) = &messages[i];
                plane.merge(from, doc);
                // Twice, so idempotence is proven at every step and not only at
                // the end.
                let second = plane.merge(from, doc);
                assert!(
                    !second.changed,
                    "merging the same message twice must change nothing"
                );
            }
            ids.push(plane_id(&plane));
            assert_eq!(plane.placement.as_ref().expect("a placement").seq, 1);
        }
        assert!(
            ids.windows(2).all(|w| w[0] == w[1]),
            "every order must reach the same plane: {ids:?}"
        );
    }

    /// Last writer wins on `(seq, by)`, and the tiebreak is what makes two
    /// simultaneous drags converge instead of oscillating.
    #[test]
    fn the_higher_seq_wins_and_the_greater_node_id_breaks_a_tie() {
        let (_d1, a_id) = identity();
        let (_d2, b_id) = identity();
        let (a, b) = (key(1), key(2));
        assert!(a < b, "the fixture depends on the ordering");

        let mut low = Plane::default();
        low.place(&a, &a_id, BTreeMap::new()).expect("place");
        let mut high = Plane::default();
        high.place(&b, &b_id, BTreeMap::new()).expect("place");
        assert_eq!(low.placement.as_ref().expect("p").seq, 1);
        assert_eq!(high.placement.as_ref().expect("p").seq, 1);

        // Same seq: the greater node_id wins, whichever arrives first.
        for (first, second) in [(&low, &high), (&high, &low)] {
            let mut plane = Plane::default();
            plane.pin(&a, &a_id.public_hex());
            plane.pin(&b, &b_id.public_hex());
            plane.merge(&a, &first.offer(&a_id.public_hex()));
            plane.merge(&b, &second.offer(&b_id.public_hex()));
            assert_eq!(
                plane.placement.as_ref().expect("p").by,
                b,
                "the greater node_id breaks the tie, in either arrival order"
            );
        }

        // A higher seq beats the tiebreak outright.
        let mut later = low.clone();
        later.place(&a, &a_id, BTreeMap::new()).expect("place");
        assert_eq!(later.placement.as_ref().expect("p").seq, 2);
        let mut plane = Plane::default();
        plane.pin(&a, &a_id.public_hex());
        plane.pin(&b, &b_id.public_hex());
        plane.merge(&b, &high.offer(&b_id.public_hex()));
        plane.merge(&a, &later.offer(&a_id.public_hex()));
        assert_eq!(plane.placement.as_ref().expect("p").seq, 2);
        assert_eq!(plane.placement.as_ref().expect("p").by, a);
    }

    /// An arrangement nobody here can vouch for is adopted (doc/input-sharing.md,
    /// D11: the plane authorizes nothing, and refusing to converge would strand
    /// two innocent devices because a third is absent), and a signature that is
    /// present and WRONG is refused. Both halves matter, because this is where
    /// this engine deliberately diverges from the sync engine.
    #[test]
    fn an_unverifiable_arrangement_is_adopted_and_a_wrong_signature_is_not() {
        let (_d1, gone_id) = identity();
        let (_d2, courier_id) = identity();
        let (gone, courier) = (key(0xa), key(0xb));

        let mut departed = Plane::default();
        departed
            .place(&gone, &gone_id, BTreeMap::new())
            .expect("place");
        // A relayed document carries the COURIER's own key, never the author's:
        // `key` is always the sender's word about itself, which is what makes it
        // pinnable at all.
        let doc = departed.offer(&courier_id.public_hex());

        // Relayed by a device that signed none of it: adopted, unverified,
        // because the arrangement is nobody's authorization and a plane that
        // will not converge refuses every session for ever.
        let mut plane = Plane::default();
        plane.pin(&courier, &courier_id.public_hex());
        assert!(plane.merge(&courier, &doc).changed);
        let held = plane.placement.as_ref().expect("adopted");
        assert_eq!(held.by, gone);
        assert!(!held.verified, "adopted on faith, and it says so");

        // With the author's key in hand, the same arrangement verifies and the
        // flag flips: the interface can then name who arranged the plane.
        let mut trusting = Plane::default();
        trusting.pin(&gone, &gone_id.public_hex());
        assert!(trusting.merge(&courier, &doc).changed);
        assert!(trusting.placement.as_ref().expect("adopted").verified);

        // A signature that is present and wrong is refused outright.
        let mut tampered = doc.clone();
        tampered["placement"]["sig"] = json!("ff".repeat(64));
        let mut victim = Plane::default();
        victim.pin(&gone, &gone_id.public_hex());
        victim.pin(&courier, &courier_id.public_hex());
        assert!(
            !victim.merge(&courier, &tampered).changed,
            "nothing at all is learned from a document whose only content is a lie"
        );
        assert!(victim.placement.is_none());
    }

    /// The loop the adoption rule leaves open, closed: an arrangement taken on
    /// faith is re-examined the moment its author's key arrives, and a forgery is
    /// thrown out. And the re-examination is SYMMETRIC, which is what makes the
    /// plane a function of the facts rather than of their order: a key replacement
    /// retires what the old key signed, whether or not this device had already
    /// proven it.
    #[test]
    fn a_pin_re_examines_an_arrangement_whether_or_not_it_was_already_proven() {
        let (_d1, real) = identity();
        let (_d2, forger) = identity();
        let (author, relay) = (key(0xa), key(0xb));

        // The forger signs an arrangement in the author's name, at a rank nothing
        // else can beat, and it is adopted while the author's key is unknown.
        let mut forged = Plane::default();
        forged
            .place(&author, &forger, BTreeMap::new())
            .expect("sign");
        let mut plane = Plane::default();
        assert!(
            plane
                .merge(&relay, &forged.offer(&forger.public_hex()))
                .changed
        );
        assert!(plane.placement.is_some());

        // The author itself turns up: the arrangement does not verify, so it goes.
        plane.pin(&author, &real.public_hex());
        assert!(
            plane.placement.is_none(),
            "an arrangement taken on faith and disproven must not survive"
        );

        // And the genuine case, which is retired by a key REPLACEMENT just the
        // same. An earlier draft kept an already-proven arrangement across a
        // reinstall, on the reasoning that it had been proven under the key that
        // was current when it arrived. That asymmetry made the final plane depend
        // on the ORDER the two keys arrived in, and with it the plane id: the
        // devices that had verified the arrangement kept it, the devices that had
        // adopted it on faith threw it out, every session across the split was
        // refused `PLANE_STALE`, and nothing repaired it, which is precisely the
        // failure D11 exists to prevent. Convergence is the property everything
        // else here rests on, so the rule is now flat: a key replacement retires
        // what the old key signed, arrangement and monitors alike, and one drag
        // puts it back.
        let mut genuine = Plane::default();
        genuine
            .place(&author, &real, BTreeMap::new())
            .expect("sign");
        let mut plane = Plane::default();
        plane.pin(&author, &real.public_hex());
        assert!(
            plane
                .merge(&author, &genuine.offer(&real.public_hex()))
                .changed
        );
        assert!(plane.placement.as_ref().expect("p").verified);
        let (_d3, reinstalled) = identity();
        plane.pin(&author, &reinstalled.public_hex());
        assert!(
            plane.placement.is_none(),
            "a key replacement retires what the old key signed, so every device \
             ends on the same plane whichever order it learned the two keys in"
        );
    }

    /// Every bound IN A DOCUMENT'S OWN SHAPE is a bound: a document over one of
    /// them is refused rather than truncated, because truncation would make the
    /// merge depend on iteration order and the merge must be a function of the
    /// content.
    ///
    /// Named for its scope, because the old name claimed all of them and this
    /// passed while the STORED map had no bound at all: see
    /// [`the_stored_document_stays_within_one_peers_send_however_many_peers_talk`]
    /// for the half that mattered, and
    /// [`a_document_that_breaks_a_bound_takes_its_own_placement_down_with_it`] for
    /// "refused whole" really meaning whole.
    #[test]
    fn a_document_over_a_bound_in_its_own_shape_is_refused_not_truncated() {
        let (_dir, id) = identity();
        let mine = key(1);

        // Too many devices.
        let mut monitors = serde_json::Map::new();
        for i in 0..=MAX_DEVICES {
            monitors.insert(key(i as u8), json!({ "seq": 1, "list": [], "sig": "00" }));
        }
        let mut plane = Plane::default();
        let out = plane.merge(
            &mine,
            &json!({ "d": crate::wire::DIALECT, "monitors": monitors }),
        );
        assert!(out.refused && !out.changed);
        assert!(plane.monitors.is_empty());

        // Too many monitors for one device.
        let many: Vec<Value> = (0..=MAX_MONITORS)
            .map(|i| monitor_to(&screen(&format!("M{i}"), 0, 0, 100, 100)))
            .collect();
        let mut plane = Plane::default();
        let out = plane.merge(
            &mine,
            &json!({ "d": crate::wire::DIALECT,
                     "monitors": { key(1): { "seq": 1, "list": many, "sig": "00" } } }),
        );
        assert!(out.refused && !out.changed);

        // Too many spots, from the local gesture's side.
        let mut spots = BTreeMap::new();
        for i in 0..=MAX_SPOTS {
            spots.insert(spot_key(&key(1), &format!("M{i}")), Spot { x: 0, y: 0 });
        }
        assert_eq!(
            Plane::default().place(&mine, &id, spots),
            Err(PlaceError::TooMany)
        );

        // Few enough spots and too many BYTES: an arrangement nobody could
        // replicate is not an arrangement, and the count alone does not bound the
        // size, a spot key being a node_id plus a monitor id.
        let mut heavy = BTreeMap::new();
        for i in 0..MAX_SPOTS {
            heavy.insert(
                spot_key(&key(1), &format!("{}-{i}", "M".repeat(200))),
                Spot { x: 0, y: 0 },
            );
        }
        assert_eq!(heavy.len(), MAX_SPOTS, "under the count bound");
        assert_eq!(
            Plane::default().place(&mine, &id, heavy.clone()),
            Err(PlaceError::TooBig)
        );
        // And the same document from a peer is refused, by the same bound read off
        // the same content, so no two devices disagree about it.
        let mut json_spots = serde_json::Map::new();
        for key in heavy.keys() {
            json_spots.insert(key.clone(), json!({ "x": 0, "y": 0 }));
        }
        let out = Plane::default().merge(
            &mine,
            &json!({ "d": crate::wire::DIALECT,
                     "placement": { "seq": 1, "by": key(1), "at": 0,
                                    "spots": Value::Object(json_spots), "sig": "00" } }),
        );
        assert!(out.refused && !out.changed);

        // A full 256 spots of ORDINARY monitor ids is not over any bound, which is
        // the other half of the promise: the two bounds must not contradict each
        // other for anything a real fleet can have.
        let mut ordinary = BTreeMap::new();
        for i in 0..MAX_SPOTS {
            ordinary.insert(
                spot_key(&key(1), &format!("DP-{i}:9f3a1c7d")),
                Spot { x: 0, y: 0 },
            );
        }
        assert_eq!(Plane::default().place(&mine, &id, ordinary), Ok(()));
    }

    /// Geometry a plane cannot do arithmetic on is refused: a zero-sized
    /// monitor, a non-positive scale, a coordinate that would overflow a corner.
    #[test]
    fn impossible_geometry_never_enters_the_plane() {
        for bad in [
            json!({ "id": "M", "name": "n", "w": 0, "h": 100, "x": 0, "y": 0, "scale": 1000 }),
            json!({ "id": "M", "name": "n", "w": 100, "h": -1, "x": 0, "y": 0, "scale": 1000 }),
            json!({ "id": "M", "name": "n", "w": 100, "h": 100, "x": 0, "y": 0, "scale": 0 }),
            json!({ "id": "M", "name": "n", "w": 100, "h": 100, "x": 0, "y": 0, "scale": -1000 }),
            json!({ "id": "M", "name": "n", "w": 100, "h": 100, "x": i32::MAX, "y": 0, "scale": 1000 }),
            json!({ "id": "", "name": "n", "w": 100, "h": 100, "x": 0, "y": 0, "scale": 1000 }),
            // A slash in a monitor id would make a spot key ambiguous.
            json!({ "id": "a/b", "name": "n", "w": 100, "h": 100, "x": 0, "y": 0, "scale": 1000 }),
            // And a `|` would make the GUARD key built from it ambiguous, which is
            // worse because it fails LATER: the guard held until the next restart
            // and then silently stopped applying.
            json!({ "id": "a|b", "name": "n", "w": 100, "h": 100, "x": 0, "y": 0, "scale": 1000 }),
            json!({ "id": "M", "name": "n", "w": 100, "h": 100, "x": 0, "scale": 1000 }),
            json!({ "id": "d".repeat(MAX_ID + 1), "name": "n", "w": 100, "h": 100,
                    "x": 0, "y": 0, "scale": 1000 }),
            json!({ "id": "M", "name": "n".repeat(MAX_NAME + 1), "w": 100, "h": 100,
                    "x": 0, "y": 0, "scale": 1000 }),
        ] {
            assert!(monitor_from(&bad).is_none(), "{bad} must be refused");
        }
        assert!(
            monitor_from(&monitor_to(&screen("M", -2560, 0, 2560, 1440))).is_some(),
            "an ordinary screen to the left of the origin is fine"
        );
        // A Windows display device path plus an EDID hash is realistically over the
        // old 128 byte bound, and refusing it made a whole machine's screens absent
        // from every sibling's plane with nothing said.
        let realistic = "\\\\?\\DISPLAY#DELA1C6#5&1b2c3d4e&0&UID4353#{e6f07b5f-ee97-4a90-b076-3\
                         3f57bf4eaa7}:9f3a1c7d4e5b6a8f9f3a1c7d4e5b6a8f9f3a1c7d4e5b6a8f9f3a1c7d4\
                         e5b6a8f";
        assert!(realistic.len() > 128 && realistic.len() <= MAX_ID);
        let real_screen = Monitor {
            id: realistic.into(),
            name: "Dell U2720Q".into(),
            ..screen("unused", 0, 0, 2560, 1440)
        };
        assert!(
            monitor_from(&monitor_to(&real_screen)).is_some(),
            "a real monitor identity is not an attack"
        );
    }

    /// A spot key is exactly a node_id and a monitor id, so nothing else can be
    /// read as one.
    #[test]
    fn a_spot_key_is_a_node_id_and_a_monitor_id_and_nothing_else() {
        // A key with hex LETTERS in it, so the lowercase rule is really tested:
        // a fixture of digits alone is unchanged by `to_uppercase` and the
        // assertion below would pass for the wrong reason.
        let node = key(0xab);
        assert_eq!(
            split_spot_key(&spot_key(&node, "DP-1")),
            Some((node.as_str(), "DP-1"))
        );
        for bad in [
            "DP-1",
            "/DP-1",
            "abc/DP-1",
            &format!("{node}/"),
            &format!("{node}/a/b"),
            &format!("{}/DP-1", node.to_uppercase()),
        ] {
            assert!(
                split_spot_key(bad).is_none(),
                "{bad} must not be a spot key"
            );
        }
    }

    /// The plane id is a function of what decides where the pointer goes, and of
    /// nothing else: the informational timestamp does not move it, an unverified
    /// entry does not move it, and a real change does.
    #[test]
    fn the_plane_id_moves_with_the_plane_and_with_nothing_else() {
        let (_dir, id) = identity();
        let mine = key(1);
        let mut plane = Plane::default();
        let empty = plane_id(&plane);
        plane.publish_monitors(&mine, &id, vec![screen("M", 0, 0, 1920, 1080)]);
        let with_screen = plane_id(&plane);
        assert_ne!(empty, with_screen);

        plane
            .place(
                &mine,
                &id,
                BTreeMap::from([(spot_key(&mine, "M"), Spot { x: 0, y: 0 })]),
            )
            .expect("place");
        let placed = plane_id(&plane);
        assert_ne!(with_screen, placed);

        // The timestamp is not part of the plane.
        let mut touched = plane.clone();
        touched.placement.as_mut().expect("p").at += 100_000;
        assert_eq!(plane_id(&touched), placed);

        // Nor is a signature: the id is about the content, and two devices with
        // the same content must agree even if one holds a relayed copy.
        let mut resigned = plane.clone();
        resigned.placement.as_mut().expect("p").sig = "ff".repeat(64);
        assert_eq!(plane_id(&resigned), placed);

        // But an empty spot set and no placement at all are different planes.
        let mut none = plane.clone();
        none.placement = None;
        assert_ne!(plane_id(&none), placed);
        let mut nothing_placed = plane.clone();
        nothing_placed.placement.as_mut().expect("p").spots.clear();
        assert_ne!(plane_id(&nothing_placed), plane_id(&none));
    }

    /// Persistence round trips, and a hand-edited file cannot promote an entry:
    /// the verified flags are re-derived from the pins, never trusted.
    #[test]
    fn the_stored_plane_round_trips_and_cannot_be_edited_into_trust() {
        let (_d1, a_id) = identity();
        let (_d2, b_id) = identity();
        let (a, b) = (key(1), key(2));

        let mut plane = Plane::default();
        plane.publish_monitors(&a, &a_id, vec![screen("A1", 0, 0, 1920, 1080)]);
        let mut theirs = Plane::default();
        theirs.publish_monitors(&b, &b_id, vec![screen("B1", 0, 0, 1280, 800)]);
        plane.merge(&b, &theirs.offer(&b_id.public_hex()));
        plane
            .place(
                &a,
                &a_id,
                BTreeMap::from([(spot_key(&b, "B1"), Spot { x: 1920, y: 0 })]),
            )
            .expect("place");

        let reloaded = Plane::from_stored(&plane.to_stored());
        assert_eq!(plane_id(&reloaded), plane_id(&plane));
        assert_eq!(reloaded.pins, plane.pins);
        assert_eq!(
            reloaded.placement.as_ref().map(|p| (p.seq, p.by.clone())),
            plane.placement.as_ref().map(|p| (p.seq, p.by.clone()))
        );

        // Somebody edits the file to claim an unpinned device's entry verified.
        let mut tampered = plane.to_stored();
        tampered["pins"].as_object_mut().expect("pins").remove(&b);
        tampered["monitors"][&b]["verified"] = json!(true);
        let loaded = Plane::from_stored(&tampered);
        assert!(
            !loaded.monitors[&b].verified,
            "trust is derived from the pins, never read from the file"
        );
    }

    /// Republishing the same monitors says nothing: without this, every restart
    /// would bump the seq on every sibling and set off a round for nothing.
    #[test]
    fn republishing_the_same_screens_is_silent() {
        let (_dir, id) = identity();
        let mine = key(1);
        let mut plane = Plane::default();
        let screens = vec![screen("M", 0, 0, 1920, 1080)];
        assert!(plane.publish_monitors(&mine, &id, screens.clone()));
        assert!(!plane.publish_monitors(&mine, &id, screens.clone()));
        assert_eq!(plane.monitors[&mine].seq, 1);
        assert!(plane.publish_monitors(&mine, &id, vec![screen("M", 0, 0, 2560, 1440)]));
        assert_eq!(plane.monitors[&mine].seq, 2);
    }

    /// A message in another engine's dialect is not ours to read, whatever its
    /// shape happens to be.
    #[test]
    fn a_foreign_dialect_is_ignored_whole() {
        let mut plane = Plane::default();
        let out = plane.merge(
            &key(1),
            &json!({ "d": "someone-else/1", "key": key(2),
                     "monitors": { key(1): { "seq": 9, "list": [], "sig": "00" } } }),
        );
        assert!(!out.changed && !out.refused);
        assert!(plane.pins.is_empty() && plane.monitors.is_empty());
    }

    /// A pin is replaced by the device's own new key (a reinstall) and that
    /// retires what the old key signed, so a stale entry cannot keep passing as
    /// that device's word.
    #[test]
    fn a_reinstalled_engine_replaces_its_pin_and_retires_its_old_word() {
        let (_d1, old) = identity();
        let (_d2, new) = identity();
        let node = key(1);

        let mut theirs = Plane::default();
        theirs.publish_monitors(&node, &old, vec![screen("M", 0, 0, 1920, 1080)]);
        let mut plane = Plane::default();
        plane.merge(&node, &theirs.offer(&old.public_hex()));
        assert!(plane.monitors[&node].verified);

        assert!(plane.pin(&node, &new.public_hex()));
        assert!(
            !plane.monitors[&node].verified,
            "the old key's word is retired, pending re-receipt"
        );
    }

    /// The STORED document stays inside its bounds and inside one `peers.send`,
    /// however many peers say however much.
    ///
    /// This is the failure that was permanent, and it needed no signature at all:
    /// four messages each naming sixteen fabricated node_ids with sixteen monitors
    /// left sixty-four devices stored and an offer of 314876 bytes. `peers.send`
    /// carries 64 KiB, so this device could never announce its own layout again,
    /// and any peer that did receive the document refused the whole monitors map
    /// and learned nothing about anybody. Written to `plane.json`, so it survived
    /// every restart.
    #[test]
    fn the_stored_document_stays_within_one_peers_send_however_many_peers_talk() {
        let (_dir, id) = identity();
        let mine = key(0);
        let mut plane = Plane::default();
        plane.publish_monitors(&mine, &id, vec![screen("MINE", 0, 0, 1920, 1080)]);

        // Four floods from one courier, none of them signed by anybody, each at the
        // document's own bound.
        for message in 0..4u8 {
            let mut monitors = serde_json::Map::new();
            for device in 0..MAX_DEVICES {
                let list: Vec<Value> = (0..MAX_MONITORS)
                    .map(|m| monitor_to(&screen(&format!("output-{m}"), 0, 0, 2560, 1440)))
                    .collect();
                monitors.insert(
                    key(1 + message * 16 + device as u8),
                    json!({ "seq": 1, "list": list, "sig": "00" }),
                );
            }
            plane.merge(
                &key(200),
                &json!({ "d": crate::wire::DIALECT, "monitors": monitors }),
            );
            assert!(
                plane.monitors.len() <= MAX_DEVICES,
                "the stored map is bounded after every merge, not only in the message: {}",
                plane.monitors.len()
            );
        }
        assert!(
            plane.monitors.contains_key(&mine),
            "our own word is never what a flood displaces"
        );

        // And a REAL device turning up afterwards still gets in, whatever names the
        // flood chose. This is why a proof displaces a claim rather than only a
        // greater name: fabricated node_ids are free, so a rule that kept the
        // smallest names would simply be attacked with the smallest sixteen, and a
        // real device with a high node_id would never reach anybody's plane again.
        let (_late_dir, late_id) = identity();
        let late = "ff".repeat(32);
        let mut theirs = Plane::default();
        theirs.publish_monitors(&late, &late_id, vec![screen("LATE", 0, 0, 1280, 800)]);
        assert!(
            plane
                .merge(&late, &theirs.offer(&late_id.public_hex()))
                .changed
        );
        assert!(
            plane.monitors[&late].verified,
            "a device's own proven word always finds room, and the table stays bounded"
        );
        assert!(plane.monitors.len() <= MAX_DEVICES);
        assert!(plane.monitors.contains_key(&mine));

        // And the same flood from devices whose word this one CAN verify, with
        // identities the length a real backend produces, which is what the byte
        // budget exists for: sixteen devices of sixteen such monitors is more than
        // 64 KiB of JSON however small the map is.
        let mut big = Plane::default();
        let mut dirs = Vec::new();
        for device in 0..MAX_DEVICES {
            let (dir, theirs) = identity();
            dirs.push(dir);
            let list: Vec<Monitor> = (0..MAX_MONITORS)
                .map(|m| screen(&format!("{}-{m}", "identity".repeat(25)), 0, 0, 2560, 1440))
                .collect();
            big.publish_monitors(&key(100 + device as u8), &theirs, list);
        }
        // Published last, so it is ours rather than the last foreign name published.
        big.publish_monitors(&mine, &id, vec![screen("MINE", 0, 0, 1920, 1080)]);
        big.place(
            &mine,
            &id,
            BTreeMap::from([(spot_key(&mine, "MINE"), Spot { x: 0, y: 0 })]),
        )
        .expect("place");
        assert!(
            big.monitors.values().all(|e| e.verified),
            "the fixture is only interesting if every entry is one this device can vouch for"
        );
        let offered = serde_json::to_vec(&big.offer(&id.public_hex())).expect("serialize");
        assert!(
            offered.len() < 64 * 1024,
            "what this device offers must fit one `peers.send`: {} bytes",
            offered.len()
        );
        assert!(
            big.offer(&id.public_hex())["monitors"]
                .as_object()
                .expect("a map")
                .contains_key(&mine),
            "and our own screens are in it, which is the entry nobody else can announce for us"
        );
    }

    /// A `seq` at the top of the range cannot freeze either authority.
    ///
    /// Proven before the ceiling existed: a placement planted at `u32::MAX` by an
    /// author of `ff..ff` (a node nobody can ever pin, so the pin re-examination can
    /// never fire) was adopted, a human's drag saturated to the same number and
    /// lost the tiebreak, and the next sweep overwrote the drag for ever. On the
    /// monitors side the same trick held a real device's screens out of the plane
    /// permanently.
    #[test]
    fn a_seq_above_the_ceiling_is_refused_on_both_authorities() {
        let (_dir, id) = identity();
        let (_d2, theirs) = identity();
        let (mine, peer) = (key(1), key(2));
        let nobody = "ff".repeat(32);
        let planted = |seq: u64| {
            json!({ "d": crate::wire::DIALECT,
                    "placement": { "seq": seq, "by": nobody, "at": 0, "spots": {}, "sig": "00" } })
        };

        let mut plane = Plane::default();
        let out = plane.merge(&peer, &planted(u64::from(u32::MAX)));
        assert!(out.refused && !out.changed);
        assert!(
            plane.placement.is_none(),
            "a rank nothing could ever outrank is not a rank"
        );

        // One below the ceiling, D11's remedy works: one drag outranks it.
        let mut plane = Plane::default();
        assert!(plane.merge(&peer, &planted(u64::from(MAX_SEQ - 1))).changed);
        plane
            .place(&mine, &id, BTreeMap::new())
            .expect("a drag outranks a planted arrangement");
        assert_eq!(plane.placement.as_ref().expect("p").by, mine);
        assert_eq!(plane.placement.as_ref().expect("p").seq, MAX_SEQ);

        // At the ceiling itself a drag cannot outrank, and it says so instead of
        // writing an arrangement the next sweep would snap back. A peer that squats
        // there is a peer to revoke, which is a decision above this module.
        let mut squatted = Plane::default();
        assert!(squatted.merge(&peer, &planted(u64::from(MAX_SEQ))).changed);
        assert_eq!(
            squatted.place(&mine, &id, BTreeMap::new()),
            Err(PlaceError::Ceiling),
            "a refusal a human can be told beats an arrangement that silently reverts"
        );

        // The monitors authority, same ceiling.
        let mut plane = Plane::default();
        let out = plane.merge(
            &key(200),
            &json!({ "d": crate::wire::DIALECT,
                     "monitors": { peer.clone(): { "seq": u32::MAX, "list": [], "sig": "00" } } }),
        );
        assert!(out.refused && !out.changed);
        assert!(plane.monitors.is_empty());
        let mut real = Plane::default();
        real.publish_monitors(&peer, &theirs, vec![screen("P", 0, 0, 1920, 1080)]);
        assert!(
            plane
                .merge(&peer, &real.offer(&theirs.public_hex()))
                .changed,
            "and the real device's own word is still learnable afterwards"
        );
        assert!(plane.monitors[&peer].verified);
    }

    /// A claim about a device's screens never holds out that device's own word, and
    /// nobody but us ever writes ours.
    ///
    /// The first version decided on `seq` alone, so `monitors[<real device>] =
    /// { seq: <the top of the range>, list: [], sig: "00" }` from any peer made that
    /// device's screens unable to enter the plane for ever (no session with it was
    /// then possible), and the same trick aimed at OUR OWN node_id erased our
    /// screens from a sibling's plane.
    #[test]
    fn a_claim_never_holds_out_a_devices_own_word_and_never_speaks_for_us() {
        let (_d1, id) = identity();
        let (_d2, theirs) = identity();
        let (mine, peer, courier) = (key(1), key(2), key(3));

        let claim = |node: &str| {
            json!({ "d": crate::wire::DIALECT,
                    "monitors": { node: { "seq": MAX_SEQ, "list": [], "sig": "00" } } })
        };
        let mut plane = Plane::default();
        plane.publish_monitors(&mine, &id, vec![screen("MINE", 0, 0, 1920, 1080)]);

        // A claim about a THIRD device is held, unverified, and out of the plane.
        assert!(plane.merge(&courier, &claim(&peer)).changed);
        assert!(!plane.monitors[&peer].verified);

        // The device's own signed word, at seq 1, displaces it anyway: a proof beats
        // a claim whatever the seqs say.
        let mut real = Plane::default();
        real.publish_monitors(&peer, &theirs, vec![screen("P", 0, 0, 2560, 1440)]);
        assert!(
            plane
                .merge(&peer, &real.offer(&theirs.public_hex()))
                .changed
        );
        assert!(plane.monitors[&peer].verified);
        assert_eq!(plane.monitors[&peer].seq, 1);
        assert!(
            !plane.merge(&courier, &claim(&peer)).changed,
            "and the claim cannot come back, whatever seq it carries"
        );

        // A claim about US is not held at all: we are the only author of our own
        // word, and this one is dropped whoever relays it.
        let before = plane.monitors[&mine].clone();
        let out = plane.merge(&courier, &claim(&mine));
        assert!(!out.changed);
        assert_eq!(
            plane.monitors[&mine], before,
            "nobody else writes this device's own screens"
        );

        // Even when it arrives before the Core has named this device: learning our
        // own name retires what was planted under it.
        let mut early = Plane::default();
        assert!(early.merge(&courier, &claim(&mine)).changed);
        assert!(early.set_own(&mine));
        assert!(
            !early.monitors.contains_key(&mine),
            "an unverified entry under our own name is somebody else's claim, and goes"
        );
    }

    /// Two arrangements of EQUAL rank and different spots converge, in either
    /// arrival order.
    ///
    /// `(seq, by)` is a total order on the pair and not on placements: with only
    /// those two keys, two different arrangements from the same author at the same
    /// seq could never displace each other, so the first to arrive won. Two peers
    /// who saw them in opposite orders then held different planes with different
    /// plane ids, and every session between them was refused `PLANE_STALE` for ever
    /// with nothing to repair it, because each kept re-offering a document the other
    /// ignored.
    #[test]
    fn two_arrangements_of_equal_rank_and_different_spots_converge_in_either_order() {
        let (_dir, author_id) = identity();
        let author = key(0xa);
        let arrangement = |x: i32| {
            let mut plane = Plane::default();
            plane
                .place(
                    &author,
                    &author_id,
                    BTreeMap::from([(spot_key(&author, "A"), Spot { x, y: 0 })]),
                )
                .expect("place");
            plane.offer(&author_id.public_hex())
        };
        let messages = [arrangement(0), arrangement(1920)];

        let mut planes = Vec::new();
        for order in [[0, 1], [1, 0]] {
            let mut plane = Plane::default();
            for i in order {
                plane.merge(&author, &messages[i]);
            }
            planes.push((plane_id(&plane), plane.placement.expect("a placement")));
        }
        assert_eq!(planes[0].1.seq, 1, "the fixture must be a real tie");
        assert_eq!(planes[0].1.by, planes[1].1.by);
        assert_eq!(
            planes[0].0, planes[1].0,
            "the same two arrangements must give the same plane in either order"
        );
        assert_eq!(
            planes[0].1.spots, planes[1].1.spots,
            "and the same spots, which is what an absolute coordinate means"
        );
    }

    /// Two independent planes converge from the same facts in any order, including
    /// a pin that arrives before or after an arrangement, and a key replacement.
    ///
    /// The property everything else rests on, and the one the merge's own tests did
    /// not reach. It is stated over a steady state rather than a single pass,
    /// deliberately: a device that refused a document while it held the wrong key
    /// gets it again on the next round, which is exactly what the epidemic is for,
    /// and convergence that needed a lucky order would not be convergence.
    #[test]
    fn two_independent_planes_converge_from_the_same_facts_in_any_order() {
        let (_d1, old) = identity();
        let (_d2, new) = identity();
        let (_d3, courier_id) = identity();
        let (a, courier) = (key(0xa), key(0xb));

        // A's own word under the key it had, then under the key a reinstall gave it.
        // `key` in a message is what creates a pin, so these two messages ARE the
        // pin and its replacement.
        let mut then = Plane::default();
        then.publish_monitors(&a, &old, vec![screen("A1", 0, 0, 1920, 1080)]);
        let first = then.offer(&old.public_hex());
        let mut now = Plane::default();
        now.publish_monitors(&a, &new, vec![screen("A1", 0, 0, 2560, 1440)]);
        now.publish_monitors(&a, &new, vec![screen("A1", 0, 0, 2560, 1440)]);
        let second = now.offer(&new.public_hex());

        // An arrangement signed by A, relayed by a courier, so that learning the
        // arrangement and learning A's key are two separate events that can arrive
        // in either order.
        let relayed = |identity: &Identity| {
            let mut dragged = Plane::default();
            dragged
                .place(
                    &a,
                    identity,
                    BTreeMap::from([(spot_key(&a, "A1"), Spot { x: -1920, y: 0 })]),
                )
                .expect("place");
            json!({ "d": crate::wire::DIALECT, "key": courier_id.public_hex(),
                    "placement": dragged.offer("")["placement"].clone() })
        };

        // The orders keep the two keys in the order A really published them (a
        // device never hears an older key from A after a newer one), and move the
        // arrangement through every position.
        for (signed_with, present, what) in [
            (&old, false, "signed with the key a reinstall replaced"),
            (&new, true, "signed with the key A published last"),
        ] {
            let messages = [
                (a.clone(), first.clone()),
                (courier.clone(), relayed(signed_with)),
                (a.clone(), second.clone()),
            ];
            let mut ids = Vec::new();
            for order in [[0, 1, 2], [0, 2, 1], [1, 0, 2]] {
                let mut plane = Plane::default();
                for i in order {
                    let (from, doc) = &messages[i];
                    plane.merge(from, doc);
                }
                // Then the rounds that follow, which re-offer what devices HOLD:
                // the courier's relay and A's CURRENT word. A never re-offers the
                // key a reinstall replaced, so the replaced key never comes back
                // after its replacement and this model must not pretend otherwise;
                // what does come back, again and again, is the arrangement, which
                // is what repairs a device that refused it while holding the wrong
                // key.
                for _ in 0..2 {
                    for i in [2, 1] {
                        let (from, doc) = &messages[i];
                        plane.merge(from, doc);
                    }
                }
                ids.push((plane_id(&plane), plane.placement.is_some()));
            }
            assert!(
                ids.windows(2).all(|w| w[0] == w[1]),
                "every order must reach the same plane for an arrangement {what}: {ids:?}"
            );
            assert_eq!(
                ids[0].1,
                present,
                "an arrangement {what} must be {} everywhere",
                if present { "kept" } else { "gone" }
            );
        }
    }

    /// "A document that breaks a bound is refused whole" means whole: its placement
    /// goes with its monitors.
    ///
    /// It did not. A peer over the monitors bound had its map refused and its
    /// placement absorbed, and the placement is the half that matters: it is the
    /// half a planted rank freezes.
    #[test]
    fn a_document_that_breaks_a_bound_takes_its_own_placement_down_with_it() {
        let (_dir, id) = identity();
        let (courier, author) = (key(200), key(1));

        let mut monitors = serde_json::Map::new();
        for i in 0..=MAX_DEVICES {
            monitors.insert(key(i as u8), json!({ "seq": 1, "list": [], "sig": "00" }));
        }
        let mut plane = Plane::default();
        let out = plane.merge(
            &courier,
            &json!({ "d": crate::wire::DIALECT, "monitors": monitors,
                     "placement": { "seq": 9, "by": author, "at": 0,
                                    "spots": {}, "sig": "00" } }),
        );
        assert!(out.refused && !out.changed);
        assert!(
            plane.monitors.is_empty() && plane.placement.is_none(),
            "not one statement of a refused document is kept"
        );

        // And the other way round: a placement over its own bound takes the
        // monitors down with it, because the document is what is refused.
        let mut spots = serde_json::Map::new();
        for i in 0..=MAX_SPOTS {
            spots.insert(
                spot_key(&author, &format!("M{i}")),
                json!({ "x": 0, "y": 0 }),
            );
        }
        let mut real = Plane::default();
        real.publish_monitors(&author, &id, vec![screen("M", 0, 0, 1920, 1080)]);
        let mut plane = Plane::default();
        let out = plane.merge(
            &courier,
            &json!({ "d": crate::wire::DIALECT,
                     "monitors": real.offer("")["monitors"].clone(),
                     "placement": { "seq": 2, "by": author, "at": 0,
                                    "spots": Value::Object(spots), "sig": "00" } }),
        );
        assert!(out.refused);
        assert!(
            plane.monitors.is_empty(),
            "a document with one impossible half is not half a document"
        );
    }

    /// A hostile local backend never reaches the plane, and a machine is never
    /// silently absent from it either.
    ///
    /// Both halves were real. `publish_monitors` applied NONE of the wire's
    /// validation to this machine's own list, so a monitor of `w: 0, h: 0` (what
    /// X11 RandR reports for a disabled CRTC) reached the crossing graph and
    /// panicked its clamps. And a monitor id over the bound refused the WHOLE entry,
    /// so a realistic Windows display device path made this machine's screens
    /// absent from every sibling's plane with nothing said about why.
    #[test]
    fn a_hostile_local_backend_loses_its_bad_monitors_and_keeps_the_rest() {
        let (_dir, id) = identity();
        let mine = key(1);
        let mut plane = Plane::default();

        let mut list = vec![
            Monitor {
                w: 0,
                h: 0,
                ..screen("dead-crtc", 0, 0, 1920, 1080)
            },
            screen(&"d".repeat(300), 0, 0, 1920, 1080),
            Monitor {
                id: "with/slash".into(),
                ..screen("s", 0, 0, 1920, 1080)
            },
            Monitor {
                id: "with|pipe".into(),
                ..screen("p", 0, 0, 1920, 1080)
            },
            Monitor {
                x: i32::MAX,
                ..screen("far", 0, 0, 1920, 1080)
            },
            Monitor {
                name: "n".repeat(400),
                ..screen("long-name", 0, 0, 1920, 1080)
            },
        ];
        // And seventeen good ones, one over the per-device bound.
        for i in 0..=MAX_MONITORS {
            list.push(screen(&format!("good-{i}"), 0, 0, 1920, 1080));
        }
        assert!(plane.publish_monitors(&mine, &id, list));

        let kept = &plane.monitors[&mine].list;
        assert_eq!(
            kept.len(),
            MAX_MONITORS,
            "the list is bounded rather than refused"
        );
        assert!(
            kept.iter().all(is_sane),
            "everything stored can be part of a plane, which is what the graph assumes"
        );
        for gone in ["dead-crtc", "with/slash", "with|pipe", "far"] {
            assert!(
                !kept.iter().any(|m| m.id == gone),
                "{gone} must not be on the plane"
            );
        }
        assert!(
            !kept.iter().any(|m| m.id.len() > MAX_ID),
            "nor an id no key can hold"
        );
        let named = kept
            .iter()
            .find(|m| m.id == "long-name")
            .expect("a long NAME is cut, not a reason to lose a screen");
        assert_eq!(named.name.len(), MAX_NAME);
        assert!(
            kept.iter().any(|m| m.id == "good-0"),
            "and the machine's real screens are still there"
        );
        // The entry is this device's own word, so it verifies under its own key.
        assert!(plane.monitors[&mine].verified);
    }

    /// A local gesture validates its own node_id, because a `device_id` passed here
    /// by mistake used to mint a document every peer refused, with no local error
    /// at all.
    #[test]
    fn a_local_gesture_refuses_a_name_that_is_not_a_node_id() {
        let (_dir, id) = identity();
        let mut plane = Plane::default();
        assert_eq!(
            plane.place("d_a", &id, BTreeMap::new()),
            Err(PlaceError::BadAuthor)
        );
        assert!(!plane.publish_monitors("d_a", &id, vec![screen("M", 0, 0, 1920, 1080)]));
        assert!(
            plane.monitors.is_empty() && plane.placement.is_none() && plane.pins.is_empty(),
            "nothing at all is written under a name no peer would accept"
        );
    }
}
