# 1Device sync engine (onedevice-sync)

> Design of the official sync engine, on top of the generic Core
> primitives (published transactions, `peers.send`, the `sync.*` facade)
> specified in [core-api.md](core-api.md).
> Status: settled. This document is the letter the implementation
> follows; the decisions recorded in section 12 are closed. Changing a
> rule here means changing this document first.

## 1. Position

A new workspace crate `onedevice-sync`, an official component added to
`official_components()` with the standard spawn contract (spawn token on
stdin, exit when stdin closes or the IPC is lost). Role `sync-backend`
(exclusive), scopes exactly the documented profile: `sync.serve +
transactions.publish + peers.message + devices.read + session.read +
transfers.read`.

It speaks ONLY the public Core API. Consequences that shape everything
below:

- It has no device key and cannot ask the Core to sign anything. Membership
  records are therefore signed with an engine-level keypair (section 2).
- It never touches the network directly: control messages ride
  `peers.send` (role-to-same-role, so they reach exactly the peer engines),
  bytes ride published transactions and `transactions.fill` (the Core reads
  and writes the disks on both sides; bytes never cross the IPC).
- It owns its persistence entirely (section 9); the Core stores nothing
  about sync.

**Identity.** Every protocol object and every persisted key that names a
device (vv components, set_vv, membership records, `created_by`,
`invited_by`, conflict versions, pinned sync keys) uses the device's
**node_id**: it is the one label the whole account shares. The Core's
`device_id` is NOT stable across the account (a server-enrolled sibling is
keyed by the server's id there and by node_id on a serverless sibling, and
a logout re-keys it), so it appears only at the API boundary: `peers.send`
targets, `peer.message` senders, `transactions.adopt`, and the Card rows
the GUI reads. `devices.list` carries both, so the translation is a lookup.
A `peer.message` whose sender cannot be resolved to a directory entry is
dropped.

Because a third-party engine may occupy the same role, every wire message
opens with `"dialect": "1device-sync/1"`. A message in an unknown dialect is
ignored (debug log; the Core's ack is not ours to shape). Two devices
running different engines simply do not sync, which is the replaceability
contract working as intended.

## 2. Identity and membership

**The sync keypair.** At first start the engine mints an Ed25519 keypair
and persists it. The public key is bound to the node_id by the channel, not
by a certificate: a `peer.message` arrives named by the receiving Core's
directory (the transport-authenticated node, never a claim), so a key
received DIRECTLY from a device in a `head` or `invite` message is that
device's word. The pinning rules, exactly:

- A pin is created or replaced ONLY by a direct, channel-authenticated
  message from that device. A different key from the same device replaces
  the pin (the engine was reinstalled) and retires the records the OLD key
  signed, untrusted pending re-receipt; the device's invitation survives
  (it is the inviter's signature), so a reinstalled member shows as
  invited rather than vanishing. The highest TERMINAL seq the old key had
  signed survives the rotation as a floor the door keeps measuring
  against: wiping one's engine does not void a binding `left` or
  `declined`, re-invitation does.
- Gossiped records about a device with NO pin are held as **unverified**:
  shown as pending at most, never counted `active`, never sent data, never
  allowed to supersede anything. But unverified is a state the protocol
  actively LEAVES: gossiped records that place an unpinned account device
  in the set make it a target for **introduction rounds** (records-only
  head exchanges); the answering head's channel-authenticated `sync_pub`
  creates the pin and confirms or refutes the gossiped records. Without
  this, two invitees of a third creator could never verify each other and
  a 3-member set would need the creator online forever. A device that
  holds NO state for the named set answers with a no-membership marker
  (its own channel-authenticated word about itself): the members drop the
  pending row and stop introducing; the stub's generous retry bound is
  the backstop for a target that never answers at all.
- **Endorsements close the signature chain.** Verifying a member's DOOR
  (below) means verifying its inviter's signature, and the inviter may be
  gone before ever being pinned. So a member may attach to its records
  pages **endorsements**: `{ node_id, sync_pub }` signed by the endorser,
  issued ONLY for keys the endorser pinned by direct contact. A device
  accepts a binding endorsed by a device it has itself directly pinned
  and verified `active`: one explicit, signed hop of witness, never a
  chain of hearsay (an endorsement is not transitive). An accepted
  endorsement only ever FILLS ABSENCE: it never replaces an existing
  binding, direct or endorsed; it is accepted only for a device the
  set's records already name (a compromised active sibling must not be
  able to grow state for fabricated nodes); and direct contact retires
  the stored witness, the device's own word superseding it. Since
  introductions eventually pin every reachable pair directly,
  endorsements only need to cover the departed; the residual case (an
  inviter gone before anyone it met can endorse it) is remedied by a
  re-invite from a reachable member. Beyond that one signed hop, a
  compromised sibling can still relay records but never invent a member:
  courier, not witness, exactly the directory's model.

**Record signatures.** Signatures cover a deterministic encoding of the
record's full field list, `set_id` and `status` included: a record cannot
be replayed across sets or re-labelled. A record whose re-encoding does
not verify is dropped. In full: the signed bytes are a constant domain
prefix per object kind (`1device-sync:descriptor:`,
`1device-sync:member-record:`, `1device-sync:invited-record:`,
`1device-sync:endorsement:`, so the kinds can never cross-verify under
the one engine key) followed by the RFC 8785 canonical JSON of the
fields, `sig` excluded, in the INTEGER PROFILE of the RFC: integers
within 2^53 - 1 only, floats refused (beyond that bound JCS's IEEE-754
number serialization stops round-tripping). Parsing is strict: an
unknown extra field is a malformed record, not an extension point, and
formats are bounded (set_id = 22 base64url chars, ids and keys lowercase
hex, seq-like integers within 2^32, with seq seeding saturating AT that
cap so a hostile `supersedes_seq` can never drive a device's own records
past what peers absorb), so a signature is never ambiguous about what it
covered.

**The set descriptor.** Created once, signed by the creator's sync key:

```json
{ "set_id": "<22 chars base64url, unguessable>",
  "kind": "dir" | "file",
  "name": "Projects",
  "created_by": "<node_id>",
  "created_at": <unix seconds> }
```

The descriptor carries NO member list: membership is each device's own
signed word, like the directory. `name` is the root's basename at creation
(no rename in v1).

**Membership records.** One per (set, device), self-signed, monotonic:

```json
{ "set_id": "...", "node_id": "...",
  "status": "invited" | "active" | "declined" | "paused" | "left",
  "seq": <u64>, "gen": <u64>, "sync_pub": "<the signer's public key>",
  "at": <unix seconds>, "sig": "..." }
```

`gen` is the device's clock generation for the set (section 3): peers key
its vv components by it, so a wiped-and-rejoined device can never alias
its own history.

- `invited` is the one exception to self-signature: it is the INVITER's
  signed word about the invitee (fields `invited_by`, and
  `supersedes_seq` = the invitee's highest seq the inviter holds at
  issuance, 0 at first invite). An `invited` record takes effect only
  against invitee-signed records with `seq <= supersedes_seq`: a re-invite
  after `left(5)` carries `supersedes_seq: 5` and reopens the membership; a
  REPLAYED old invitation carries a stale `supersedes_seq` and loses to
  every later self-signed record. Replay is thereby ordered out, without
  breaking re-invitation. And an `invited` record is ABSORBED only while
  its inviter itself stands admitted (active or paused; the creator
  through its own first-join record): one signed by a device the set
  never admitted parks like an unverified record and is re-evaluated as
  records land, so out-of-order pages lose nothing, admission chains stay
  well-founded (rooted at the creator), and two never-admitted account
  devices cannot mint each other into a set by exchanging invitations.
  Absorption is the gate: an invitation that landed while its inviter was
  admitted keeps its effect if the inviter later leaves.
- All other statuses are signed by the device they describe; higher `seq`
  wins, ties by `at` then lexicographic sig. **Seq seeding:** a device
  never restarts a per-set seq from a local counter; every self-signed
  record uses `1 + max(seq over every record about self it has ever seen)`,
  including the records carried by the invite payload and the
  `supersedes_seq` of every invitation naming it.
- **The invitation is the door, both ways.** An `active` record is valid
  only if the signer's admitting `invited` record exists and covers it
  (its `supersedes_seq` >= the signer's latest terminal seq, 0 for a
  first join); the creator's own `active` for a set it created is the one
  exception, and it covers the FIRST join only: a creator that left its
  own set is re-invited like anyone. `paused` stands behind the same door
  as `active` (a live status is a live status). A device that declined or
  left therefore cannot re-sign itself live at will: `left` and
  `declined` are binding until someone re-invites. An `active` whose admitting record has not arrived yet
  (gossip pages out of order) is HELD like an unverified record, counted
  for nothing, and re-evaluated whenever an `invited` record for its
  signer is absorbed: only records failing SIGNATURE verification are
  dropped, ordering never loses a legitimate member. And nobody can
  enroll a third device by force: `active` is only ever produced by the
  device itself, on the local accept gesture.
- A device the ACCOUNT revoked (gone from the directory) is dead here too:
  its rows are dropped from cards, and `peers.send` to it fails closed at
  the Core. Real distrust is `devices.revoke`; `sync.leave` is just
  leaving.

**Message authorization.** Incoming dialect messages are gated on the
sender's VERIFIED membership for the named set, before any other
processing:

| Message | Accepted from |
|---|---|
| `invite` | any account device whose carried records make it `active` in the set |
| `invite_ack` | the device an outstanding invite targeted |
| `head` | any account device; absorbed records-only unless the sender is verified `active` |
| `records` | any account device (verification and keep-newest do the real gating) |
| `entries`, `need`, `done` | verified `active` members only |
| `offer` | only the device to which we sent the matching outstanding `need` |

Anything else is dropped with a debug log. Data never flows to or from a
device the set's own records do not admit; records-only traffic (heads,
records pages) flows to and from any account device, which is what lets
introductions, pauses, declines and leaves travel.

**Gossip.** Membership and conflict records travel with the rounds
(section 4), verify-then-keep-newest, silent when nothing changed.
Devices named ONLY by unverifiable gossip are capped in number and in
parked records per device; verified members never are.
"Paused" travels: a paused device pushes a records-only head to every
reachable member at the pause gesture, and keeps ANSWERING rounds with
records-only heads while paused, so peers keep showing "paused" rather
than "offline".

**Invitations and terminal records: engine-level confirmation.** The
Core's ack only says a message was queued to a component, so one-shot
membership news is confirmed in the dialect, not assumed delivered:

- `invite { descriptor, records, stats: { entries, total_size },
  inviter's sync_pub }` is retried on every reachability change and on the
  safety tick until the inviter holds an `invite_ack` (sent by the invitee
  after persisting the invitation) or any invitee-signed record for the
  set. `stats` are the inviter's CLAIM, and the card labels them so.
  During initial sync the engine tracks bytes landed against the consented
  `total_size` and PAUSES the set with a visible card problem when the
  claim is exceeded by a stated margin (2x plus 100 MB): resuming is a
  user gesture. Consent in full knowledge, honestly enforced.
- A device that declined or left keeps a small per-set stub that keeps
  sending records-only heads (reachability changes and safety tick) to
  each member until that member's answer proves absorption: the answer to
  ANY head always includes the records the answerer holds ABOUT THE
  HEAD'S SENDER (tiny, bounded; "silent when nothing changed" governs
  round initiation, never answer content), so the stub sees its terminal
  record (or one superseding it) echoed back, per peer, in one exchange.
  The stub drops per peer on that proof, and entirely once every non-left
  member proved it or a generous retry bound expires. A 2-device set
  therefore cannot lose a decline or a leave to one dropped message.

`sync.invite { set_id, device_ids }` reuses the identical path for adding a
device later; creation is create-plus-N-invites. Only an `active` member
may invite. On the invitee, accept = choose a local root, sign `active`,
then the FIRST ordinary reconciliation round pulls everything: initial sync
is not a special path (clock seeding at accept: section 3).

## 3. The index: clocks and version vectors

Per set, per device, ONE monotonic counter: `clock[self]`, bumped every
time the engine records a local change of any file of the set. A file entry
carries a version vector whose components are those per-set clocks:

```
entry = { path,                  wire path, "/"-separated, NFC-normalized,
                                 relative to root
          kind: file | dir,
          size, mtime, exec,     as last recorded (mtime = value read BACK
                                 from disk after apply, section 6)
          hash,                  BLAKE3 of content (empty for dirs)
          vv: { "node_id@gen": clock, ... },
          deleted: bool }        tombstone keeps the vv
```

vv components are keyed by `node_id@gen`: `gen` is the generation minted
with each FRESH lease (section below), so components from different
incarnations of one device are different components, each monotonic
forever, and a wiped device can never alias values a partitioned member
still holds. Old-generation components simply stop growing: frozen
history.

- Local edit detected: `clock[self] += 1; entry.vv[self] = clock[self]`.
- Comparison is standard VV dominance: `a` descends from `b` iff
  `a.vv >= b.vv` componentwise. Incomparable = concurrent = conflict
  (section 7).
- Deletes are tombstones: `deleted: true`, vv bumped, kept forever in v1
  (a tombstone is ~100 bytes; compaction is a later, additive concern).
- Renames/moves are delete-plus-create in v1: no move detection. Cost: a
  moved file re-transfers. Accepted for v1 (mostly-LAN, and correctness
  first); move detection by hash is a purely local, additive optimization.
- Directories are entries too (existence and name only): empty folders
  sync; tombstoning a folder locally is ordered after its contents, and
  APPLYING a folder tombstone is guarded (sections 6 and 7).

**The delta.** The per-set summary `set_vv` names what a device holds, and
the answer to a head sends EVERY entry not componentwise covered by the
peer's ADVERTISED vv, whoever authored it (a relay forwards third-party
entries; this is what lets B carry C's changes to A when C is away). The
advertised vv is defined in section 4: own clock joined with what has been
FULLY absorbed, never "max component seen in the index".

**Clock durability: the lease and the generation.** vv monotonicity is
what correctness rests on, so `clock[self]` is never trusted to a file
written "often enough". Per set, a tiny lease file persists the current
`gen` and `reserved = clock + 1000`, fsynced BEFORE any clock in that
block is issued; on ANY restart the clock resumes at `max(lease, highest
vv[self@gen] in the index)`. A crash can waste up to 1000 values, never
reuse one. The lease survives `sync.leave` and decline (leaving keeps
local files; it keeps the clock too), so a re-invited device continues
its old generation and clock. A FRESH lease (accept on a wiped device,
the reinstall case) mints a NEW generation (`gen` = unix seconds at
seeding, so even a device that cannot see its own older generations
cannot re-mint one; ties need two wipes in one second) and starts its
clock at 1: no seeding from heads is needed for safety, because a new
generation is a new component by construction. The loud self-component
guard (below) remains the backstop for the current generation.

**The self-component guard.** Any received head or entry claiming
`vv[self]` (or `set_vv[self]`) ABOVE our own clock is an impossible claim
about our own history: the engine jumps its clock above the claim, logs
loudly, and carries on. A hostile member inflating others' components can
therefore delay nothing for long and suppress nothing silently.

**Echo suppression.** The applier's own writes must not read back as local
changes. What guarantees it is the apply order itself: the index records
the mtime READ BACK from the disk after the write (section 6), so the
watcher's echo finds size and mtime exactly as indexed and the rescan sees
no difference at all. Where that comparison cannot be trusted, the content
decides: a whole-second stamp means a coarse-grained filesystem (FAT counts
in 2 s steps), so the file is hashed rather than believed, and an identical
hash is not a change. "Doubt" is defined, not felt. Elsewhere the
quick-check is size+mtime, hash on mismatch. (No timed pending-apply table
is needed: a hash that matches is proof, and a grace window would only
have been a weaker version of it.)

**Startup and safety rescan.** On start (and on a slow safety tick, and at
`sync.resume`): walk the root, compare against the index, bump
`clock[self]` for every real difference, tombstone what disappeared: a
path present only in the PENDING set (section 4) is expected-absent and
is never tombstoned.
Crash recovery falls out (section 6 makes the crash windows converge to
the RIGHT state). Watcher overflow (inotify queue, ReadDirectoryChangesW
buffer, FSEvents must-scan flags) triggers an immediate targeted rescan of
the affected subtree; failure to install watches at all degrades the set to
periodic scanning, surfaced as a card problem, never silent.

## 4. Reconciliation: rounds over peers.send

The counterpart of `dir_sync` for file trees. All control messages are
`peers.send` payloads (opaque JSON, hard cap 64 KiB), self-describing and
idempotent: a lost message costs a round, never correctness. Every
outgoing frame is BYTE-budgeted at serialization time (48 KiB target,
never a count heuristic); membership records, conflict records and entries
all page.

Message types (all carry `dialect` and `set_id`):

| Type | Content | Meaning |
|---|---|---|
| `head` | descriptor hash, sender's `set_vv`, `records_pages` and `entries_pages` counts for the pages that FOLLOW it, `entries_complete`, sender's sync_pub, the `answers` round when it is an answer; or a no-membership marker | "here is where I stand"; every batch of pages is preceded by a head declaring it, on BOTH legs. The counts make a page batch complete-or-discarded (so no per-page `of` is needed), and `entries_complete` says whether the delta was COMPUTED at all: an empty delta and an uncomputed one are the difference between advancing a watermark and advancing nothing |
| `records` | page of membership + conflict records and endorsements, `round`/`page` | the gossip, paged |
| `entries` | page of index entries, `round`/`page` | the delta the peer's head showed it lacks |
| `need` | list of wire paths, `need_id` | "publish these for me" |
| `offer` | `need_id`, `tx_id`, `files: [{ wire_path, set_path }]` | "adopt this and pull" |
| `done` | `need_id`, `tx_id` | "I have landed everything; you may revoke" |
| `invite` / `invite_ack` | section 2 | the one confirmed one-shot |

**The wire-path gate.** Every path arriving in ANY dialect message
(entries, need, offer, conflict records) passes the same fail-closed
validation the Core applies to remote manifests, BEFORE absorption:
relative, `/`-separated, NFC, no empty or `.` or `..` component, no `\`,
no `:`, no control characters, no Windows reserved name (CON, NUL, COMn,
LPTn), no trailing dot or space in a component, no component over 200
bytes UTF-8 (a budget every major filesystem can create, with room for
the conflict-copy suffix), and nothing under the reserved temp names
(section 6). One invalid path drops the whole page,
loudly. Deletes, applies, resolves and fills operate only on gated index
entries, and fill `dest_path`s are built from the ADOPTED manifest
(Core-validated), never from a peer's claim. A hostile sibling's engine
gets no write and no unlink outside the root, ever.

**A round with peer P for set S:** send `head`. P answers with its own
`head` (declaring the pages that follow) plus those pages, computed
against our advertised vv per the section 3 delta rule. Symmetric: our
absorbing of P's head sends P what IT lacks, preceded by our own
declaring head. The answer to ANY head always includes the records the
answerer holds ABOUT THE SENDER (bounded; it is what proves absorption to
stubs and pause pushes): "silent when nothing changed" governs whether a
round is initiated, never what an answer contains.

**Watermarks advance on completed-and-persisted rounds only.** Per peer,
the engine persists an advertised watermark. Pages of one round are
buffered and absorbed atomically once all declared pages arrived; a gap
or a round timeout discards the partial batch (the next round resends).
Absorbed entries whose bytes are still to pull do NOT enter the live
index: they land in the persisted **pending set** (section 9), which is
what drives `need`s, survives crashes, and is invisible to the rescan's
tombstoning (a pending path is expected-absent on disk). The watermark
advances to the sender's declared head vv only once the round's entries
are each applied, tombstone-guarded, parked, or persisted as pending:
after that fsync, nothing the watermark covers can be lost, and before
it, nothing is advertised. A records-only round (a paused device's
answer, an introduction) advances NOTHING: a paused device's set_vv names
edits it has not sent, and covering them would punch the exact hole the
rule exists to prevent. What a device ADVERTISES as its set_vv is its own
clock joined with the componentwise max of its fully-absorbed watermarks:
never the max components of individual index entries (an entry learned
via a relay must not advance a watermark for entries never received).

**When rounds run** (dirsync's rhythm, deliberately): on watcher
quiescence (debounced ~2 s), on a peer becoming reachable (`devices`
topic), on an explicit nudge (a `head` we did not expect,
pause/resume/leave gestures), and on a slow safety tick (15 min). Full
rounds target verified-`active` members with a route; INTRODUCTION rounds
(records-only) additionally target unpinned account devices the gossiped
records place in the set (section 2). One connect failure is debug-level
ordinary.

**Behindness for the cards.** "Up to date" = the peer's advertised vv
covers ours; "N files behind" = count of our entries it does not cover;
"offline since T, will catch up" = no route (devices topic) with T = last
successful round. Per-device truth, never an average: exactly the #82
card.

## 5. Moving bytes: published transactions

The device holding the newer bytes publishes; the needer pulls
(pull-driven: a transaction exists only while a peer wants it, decision
D10).

- Needer sends `need { need_id, paths }`. Needs are chunked so that no
  chunk contains two paths with the same basename: the published wire
  paths (collision-suffixed basenames) are then a bijection, and `offer`
  carries the explicit `{wire_path, set_path}` map. Chunks also respect
  the manifest cap and a sane total size.
- The source serves a need ONLY for paths that string-match live,
  non-ignored, non-deleted entries of its own index (watcher-derived,
  inside the root by construction): anything else drops the message,
  loudly. It calls `transactions.publish` on those live paths and answers
  `offer`. Per peer, at most 2 concurrent published transactions and
  rate-limited need handling: a member cannot make a source's Core walk
  trees forever.
- Every published tx is keyed `(peer, need_id)`: `done` and supersession
  are honored only from THAT peer for that key. One member's needs never
  revoke another member's in-flight pull.
- Needer adopts only offers matching its own outstanding `need` to that
  sender (`transactions.adopt { device_id, tx_id }`), fills into staging
  (section 6), VERIFIES each landed file's BLAKE3 against the expected
  entry hash (mismatch = re-need), applies, then sends `done`; the source
  revokes. Safety nets: every published tx also dies with the engine's
  connection or the Core's stop (the #83 lifetime doctrine): nothing
  leaks.
- `TX_STALE` at adopt/open (source revoked or restarted): re-run the
  round. `FILE_CHANGED` on a pull: the file changed on the source between
  publish and pull; its newer entry arrives with the next head.
- `transfer.failed` is whole-batch, so the engine salvages: each staged
  file is verified by hash and the complete ones are applied; only the
  remainder is re-needed. A path whose bytes keep failing to land (a log, a
  database rewritten under the pull) collects strikes, and beyond the first
  it is needed ALONE and only once the rest of the batch has had its turn:
  a hot file must not starve what it shares a need with.
- A full disk stops the set instead of spinning: the "disk full" card
  problem is raised from the FAILURE's own words (the Core relays them) and
  pulling stops until a resume gesture. v1 reads the refusal rather than
  predicting it: asking a filesystem how much room is left is per-OS work
  or a new dependency, and the property that matters (a visible problem
  instead of a retry loop) does not need the prediction.

## 6. Applying changes: staging and atomicity

Inside each set root, a reserved directory `.1device.tmp/` (hidden
attribute on Windows), excluded from watching, indexing and publishing. It
must live inside the root because the final step is a rename, and rename
wants the same filesystem; on EXDEV (a mount point inside the root) the
engine falls back to a temp file beside the target, pattern
`.<name>.1dtmp`, on the SAME exclusion list. A PRE-EXISTING user entry
named `.1device.tmp` is refused at `sync.create`/`sync.accept`; one
created LATER is surfaced in the card's ignored list with a reason, like
every other refused name, never silently shadowed. The startup sweep
removes only staging subdirectories matching the engine's own random-name
pattern AND not referenced by a parked entry (below): the engine never
deletes what it did not write, and a restart does not throw away verified
bytes a locked file is waiting on.

**Filesystem first, index after.** Apply order for a pulled file: fill
lands bytes in `.1device.tmp/<8 chars>/<wire path>`; verify size and
BLAKE3; set mtime to the source's and read the resulting value BACK (the
filesystem's rounding, not our wish, is what the index records); enter the
path in the in-memory pending-apply table; `rename()` over the
destination (creating parents); THEN write the index entry (vv = the
received vv), moving it out of the persisted pending set in the same
write. Deletes: remove the file, then write the tombstone. The
index never claims what the disk does not hold, so every crash window
converges to the TRUTH: a crash after rename but before the index write is
re-detected by the rescan as a local difference whose hash equals the
remote version, and the same-hash rule (section 7) merges it silently; a
crash after remove but before the tombstone re-detects the deletion
locally and re-propagates an idempotent tombstone. (The reviewed v1 order,
index first, converged crashes onto STALE data with a dominating vv:
fleet-wide silent loss. Order is load-bearing.)

Windows realities, specified: all staging and apply I/O uses `\\?\`
extended-length paths (a deep tree under a long root must not wedge);
rename-over-existing uses the platform replace primitive;
FILE_ATTRIBUTE_READONLY is cleared deliberately before replacing; a
destination locked by an open application (the Office case) keeps its
verified staged copy (persisted as a parked entry) and retries the RENAME
alone with backoff, no re-transfer, and after N failures parks as
"blocked: file in use" in the card's details (fail-visible, like every
ignored entry). A kind change (file to dir or back) applies as
remove-old-kind-then-create.

**Tombstone apply guards.** Before removing for a tombstone: no LIVE index
entry may case-fold or normalization-fold to the same path (on a
case-insensitive filesystem the on-disk file belongs to the live entry: a
case-only rename arriving as create-then-tombstone across two rounds must
not delete the renamed file). Within one round's batch, file deletes apply
before creates; DIRECTORY tombstones are evaluated at the END of the
batch, after every other operation of the round has been applied, and
remove only a directory that is then empty; otherwise the delete-vs-edit
rule resurrects the chain (section 7).

A local write that races an apply (the "catching up" window): the watcher
event arrives for a path that has a pending remote version; the engine
treats it as concurrent (it is) and takes the conflict path. #82's honest
fraction of a lock: while behind-count > 0 the card says "catching up, N
files on the way", and the race window is real but handled, not denied.

**Persistence discipline** (all engine state, section 9 files): write
temp, fsync the temp, rename, fsync the directory (on Windows:
FlushFileBuffers before the replacing rename; the directory flush has no
std equivalent there and the rename itself is the ordering). Rename-alone
orders the namespace, not the data; a power cut must not hand back a
zero-length index.

## 7. Conflicts

Detected, not prevented: entry received with vv INCOMPARABLE to the local
entry's.

- **Same hash, concurrent vv** (both sides made the identical change):
  merge silently: vv = componentwise max, no conflict. This rule is also
  what makes parallel detections below converge. Concurrent TOMBSTONES
  merge the same way (both sides deleted): vv = max, no conflict, nothing
  to materialize.
- **Delete vs edit: the edit wins, everywhere, and up the tree.** A
  tombstone loses to any concurrent live version (lossless doctrine: a
  resurrection is recoverable, a lost edit is not), and a DIRECTORY
  tombstone loses to any concurrent live descendant: the whole ancestor
  chain is resurrected in the index, and EVERY surviving or resurrected
  entry (the descendant and each ancestor alike, on every device that
  keeps them) takes `vv = componentwise max(tombstone's, own) + local
  bump`, so the survivors dominate the tombstone everywhere and the
  tombstone retires instead of oscillating back from a third device.
- **Edit vs edit (or create vs create, different content): keep BOTH,
  deterministically.** Materialized BEFORE `sync.conflict` is announced
  (the #82 contract), by a pure function of the two entries so that every
  device computes the SAME outcome with no negotiation:
  1. `version_id = BLAKE3(canonical vv || content hash)` for each side;
     `conflict_id = BLAKE3(the two version_ids, sorted)`. The WINNER of
     the plain path is the lower version_id: symmetric, arbitrary, and
     identical everywhere. (v1's "the local file stays put" was
     symmetric-and-divergent: both devices kept DIFFERENT content at the
     plain path forever. Determinism is what makes "a folder kept
     identical" true again; the protection for the user is the conflict
     card and the resolve gesture, not the plain path's occupant.)
  2. The winner's content takes the plain path with
     `vv = componentwise max of both + detector's bump` (parallel
     detections produce same-content entries that the same-hash rule
     merges).
  3. The loser is materialized beside it as
     `name (DeviceName, 2026-08-16 14h02 UTC).ext`: the timestamp is the
     LOSING version's recorded mtime in UTC (wire data, identical
     everywhere, never the detection time); DeviceName is the losing
     device's name from the local directory, sanitized against the FULL
     wire gate (separators, `:`, control chars, leading dots stripped;
     NFC; capped at 32 chars; trailing dot/space trimmed after the cap;
     node_id prefix if empty), and the generated path is itself run
     through the gate before use, the base name truncated
     deterministically so the whole component fits the gate's byte
     budget. On collision at the computed name, a deterministic counter
     is appended (it is part of the record's authoritative path). Both
     files are ordinary synced entries from that moment. Placing a
     version at ANY path (first materialization or a later fold) never
     overwrites: a live entry already at that path with a DIFFERENT hash
     is a create-vs-create conflict like any other (fresh version_ids,
     new record), and a same-hash occupant merges silently: the ordinary
     concurrent-entry rules, applied to the machinery's own writes.
  4. The conflict record is a FIRST-CLASS gossiped object (it rides the
     `records` pages): signed by the detecting device, keyed
     `(set_id, path, conflict_id)`, carrying the authoritative
     `path_on_disk`, with a lifecycle
     `open -> resolved { resolved_by, seq }` where `resolved` is a kept
     tombstone that wins absorption. Two parallel detectors may sign the
     same key (names can transiently differ): the deterministic winner
     among same-key open records is the lower signing node_id, then
     lexicographic sig; only the winning record's `path_on_disk` is
     authoritative, losing duplicates are absorbed and dropped, and a
     live entry whose hash equals the loser's at a different path is
     folded onto the winning record's path. Resolving anywhere therefore
     resolves everywhere, a peer that missed the resolution cannot
     resurrect the conflict from a stale open record, and the fold cannot
     ping-pong. Only a NEW incomparable pair (fresh version_ids) opens a
     new record. (v1 defers the PHYSICAL fold: the divergence needs two
     detectors holding different directory names at the same instant, and
     what it leaves is a same-content, synced, harmless duplicate; the
     winning record's `path_on_disk` remains the authority the fold will
     use when it lands.)
  5. `sync.resolve { set_id, path, keep }`: `keep: version_id` puts that
     content at the plain path and deletes the other copy (ordinary
     synced operations, they propagate by themselves); `keep: "all"`
     marks the record resolved and leaves both files. Nothing else ever
     deletes; the gesture is the only eraser.

Conflict copies never cascade: a file named like a conflict copy is still
just a file; only the conflict RECORD gives it meaning.

## 8. Watching the root

The `notify` crate (inotify / FSEvents / ReadDirectoryChangesW), debounced
~2 s of quiescence before a round: propagation is immediate whenever
devices can talk, exactly #82's rule 1 (conflicts rare by speed). Overflow
and degraded modes: section 3.

Excluded and refused:

- `.1device.tmp/` (staging) and `.<name>.1dtmp` (the EXDEV fallback),
  always; a USER entry usurping the reserved directory name is surfaced
  as ignored-with-reason (section 6).
- Symlinks: ignored in v1 (not followed, not synced, surfaced as ignored).
- Names that fail the wire gate (section 4) or collide under case-folding
  OR Unicode normalization-folding: indexed as `ignored` with a reason,
  surfaced in the card, synced nothing (fail-visible, never fail-silent).
  Wire paths and index keys are NFC everywhere; on macOS the disk's NFD
  forms are mapped to NFC on read (the classic Mac trap: without it, one
  accented filename oscillates tombstone/create between a Mac and a Linux
  forever).
- The exec bit on filesystems that lack one (NTFS, FAT) is write-only
  passthrough: the index preserves the last received value, apply ignores
  it, the rescan never diffs on it, local edits carry it forward. A
  Windows member must not strip `+x` from every script it touches. v1
  keys this on the OS rather than the filesystem, so a FAT or NTFS volume
  mounted on unix reports whatever mode the mount options invent; lifting
  it means asking the filesystem, which is additive.
- The set-of-one-file case (`kind: "file"`) watches the parent directory
  filtered to the one name (editors replace-by-rename on save).

Nested or overlapping roots are refused at `sync.create`/`sync.accept`
(`SYNC_ROOT_OVERLAP`): one file, one set.

## 9. Persistence

All under the engine's platform data dir, all JSON, all written with the
full fsync discipline (section 6):

- `identity.json`: the sync keypair.
- `sets/<set_id>/meta.json`: descriptor, local root, membership records,
  endorsements, pinned peer keys, per-peer advertised watermarks, the
  PENDING set (entries absorbed but not yet applied: what the rescan must
  not tombstone and the `need` queue resumes from), conflict records
  (pending detections included: a conflict absorbed but not yet
  materialized survives a crash), parked entries, pending invitations and
  stubs, local status.
- `sets/<set_id>/clock.lease`: the generation and clock reservation
  (section 3), tiny, fsynced before use, surviving leave.
- `sets/<set_id>/index.json`: the entries. Rewritten on debounced change
  and on clean shutdown; ~65k entries is a few MB, acceptable for v1 (a
  paged store is a later, invisible swap).
- **Write ordering:** a round's absorbed state (pending set, conflict
  records) is fsynced BEFORE its watermark advances on disk. A crash
  between the two re-absorbs a round already taken in (idempotent); the
  reverse order would advance the watermark over entries the crash threw
  away: the silent permanent hole, again. Order is load-bearing here too.
- Losing the index is loud but safe: the lease keeps the clock monotonic,
  and the startup rescan re-detects the tree as local changes (same-hash
  merges absorb the non-changes fleet-wide). Losing the lease too (both
  files gone = the reinstall case) mints a new generation (section 3):
  a new component by construction, aliasing impossible.

The Core stores nothing; `sync.status` is answered entirely from this
state.

## 10. The `sync.*` vocabulary (frozen)

Methods, all routed through the facade (verbatim relay; `sync.status`
under `sync.read`, the rest under `sync.manage`). The facade's proxy
budget is 10 s, so no method does long work inline: `sync.create` and
`sync.accept` validate, persist, and RETURN; scanning, hashing and
inviting run behind `sync.updated` progress. Retrying a `create` cannot
mint twins: the root is registered before the reply, and a second create
on the same root is `SYNC_ROOT_OVERLAP`.

| Method | Description |
|---|---|
| `sync.status {}` | → `{ sets: [Card], invitations: [Invitation] }`: the `sync` topic's snapshot, and the AUTHORITATIVE state the notifications merely echo |
| `sync.create { path, device_ids }` | → `{ set_id }`. Registers the set, then scans and invites asynchronously. `path` may be a file (set of one) |
| `sync.invite { set_id, device_ids }` | → `{}`. Same machinery, existing set |
| `sync.accept { set_id, path }` | → `{}`. The local consent; `path` = locally chosen root (must not exist, or be an empty dir) |
| `sync.decline { set_id }` | → `{}`. Signed, travels (with the stub guarantee): the inviter's card stops waiting |
| `sync.pause { set_id }` / `sync.resume { set_id }` | → `{}`. Travels as membership |
| `sync.leave { set_id }` | → `{}`. Local files stay in place (and the interface says so) |
| `sync.resolve { set_id, path, keep }` | → `{}`. `keep`: a `version_id`, or `"all"` |

Errors (engine's own, relayed verbatim): `SYNC_UNKNOWN_SET` (including a
`set_id` of the wrong shape: not one this engine could hold),
`SYNC_ROOT_OVERLAP`, `SYNC_ROOT_NOT_EMPTY`, `SYNC_ROOT_UNKNOWN` (the path
is not there, or cannot be made), `SYNC_ROOT_RESERVED` (a pre-existing
entry named like the staging directory), `SYNC_NOT_INVITED`,
`SYNC_NO_CONFLICT`, `SYNC_DEVICE_INELIGIBLE` (mobile, unknown, or already
a member), `SYNC_NOT_READY` (the engine has not resolved the account's
directory yet: a Core that joined nothing has no sets to manage either),
`SYNC_INTERNAL` (a local failure the caller can only retry), plus a
genuine JSON-RPC `-32602` for shape - a malformed request is not an
application state, and the engine emits the real code rather than
dressing one as an app code.

Notifications (topic `sync`, published via `sync.emit`):

| Notification | When |
|---|---|
| `sync.updated { set: Card }` | any card change (state, progress, membership, per-device truth) |
| `sync.invitation { invitation: Invitation }` | an invitation arrived |
| `sync.conflict { set_id, conflict: Conflict }` | AFTER materialization, per contract |
| `sync.removed { set_id }` | the set left this device (leave, decline, account revoke) |

Shapes:

```json
Card = { "set_id", "kind": "dir"|"file", "name", "path",
         "state": "in_order"|"catching_up"|"paused"|"waiting"|"conflicts",
         "problem": null | "disk_full" | "watch_degraded"
                  | "size_exceeds_invitation",
         "behind": <n, when catching_up>,
         "devices": [ { "device_id",
                        "membership": "active"|"invited"|"declined"
                                      |"paused"|"left",
                        "sync": "up_to_date"|"behind"|"offline",
                        "behind": <n>, "last_seen": <ts> } ],
         "conflicts": [ Conflict ],
         "ignored": [ { "path", "reason" } ],
         "blocked": [ { "path", "reason" } ] }

Invitation = { "set_id", "name", "kind", "device_id" (inviter),
               "entries": <n>, "total_size": <bytes>,   // inviter's claim
               "default_path": "<suggested local root>" }

Conflict = { "path", "conflict_id",
             "versions": [ { "version_id", "device_id",
                             "mtime", "size", "path_on_disk" } ] }
```

`problem` is the card-level honest sentence hook (the
`session.status.problem` pattern): a set that cannot do its job says why,
from the snapshot alone. The Card carries the full conflict and
ignored/blocked lists (a GUI that starts after a materialization must be
able to render #82's "lists the files; each shows both versions" and call
`sync.resolve` from the snapshot alone). Device NAMES are the GUI's job
(`devices.read` both sides); the dialect speaks node_id and the facade
`device_id`. The one exception: conflict copy FILENAMES, sanitized,
section 7.

## 11. v1 limits (all deliberate, all additive to lift)

Computers only (mobiles never in a list); bidirectional only; no move
detection; the exec bit keyed on the OS rather than the filesystem; no selective sub-folder sync; no filters/ignore patterns beyond
the built-in exclusions; symlinks ignored; tombstones and conflict records
never compacted; index snapshot not paged; accept target must be empty; no
set rename; no cross-device eviction (`devices.revoke` is the
account-level answer).

## 12. Settled decisions

The design choices that were explicitly weighed and closed, recorded with
their reasons so they are not relitigated by accident:

D1. The engine-level keypair (channel-pinned; pins minted by direct
    contact, introduction rounds so every pair eventually meets, one-hop
    signed endorsements for signers who left before being pinned) as the
    membership trust root: the consequence of "speaks only the public
    API". Alternative: a Core signing primitive (new #83-style scope),
    heavier, rejected here.
D2. Per-set per-device clocks so the set_vv doubles as the delta index,
    with the lease file and clock GENERATIONS (vv components keyed
    `node_id@gen`) as the durability story: a wiped device is a new
    component, never an alias.
D3. Renames = delete+create in v1.
D4. Conflict materialization is DETERMINISTIC: the plain path goes to the
    tie-break winner (which may be the OTHER device's version; your
    version then sits beside it, honestly named). "Your file never moves
    under you" was tried and is provably divergent; the user's protection
    is the conflict card and the one-gesture resolve.
D5. Delete-vs-edit: the edit wins, up the tree included (a directory
    tombstone never destroys a concurrently edited descendant).
D6. `sync.invite` (adding a device to an existing set) included in v1: it
    is creation's own machinery, zero extra protocol.
D7. `declined` added to the membership statuses (the ticket lists four;
    without it the inviter waits forever) and to the Card; and terminal
    statuses are BINDING (re-signing `active` requires a fresh
    invitation: the invitation is the door, both ways).
D8. Accept requires an empty (or absent) target directory in v1: merging a
    pre-existing tree at accept time is a conflict storm by construction.
D9. Default suggested root: `~/1Device/<set name>` (GUI may override).
D10. Byte serving is pull-driven (publish on `need`), not
    publish-on-change as #84's summary sketched: no long-lived
    transactions leaking while nobody pulls, at the price of one extra
    round-trip of latency per delta. The revoke-the-superseded rule
    becomes per-need supersession.
D11. A reserved `.1device.tmp/` directory lives INSIDE each set root
    (atomic rename wants the same filesystem). Hidden on Windows, swept
    only by its own pattern, pre-existing user entries by that name
    refused at create/accept, later usurpers surfaced as ignored.
