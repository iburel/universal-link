# 1Device input engine (onedevice-input)

> Design of the official keyboard and mouse engine, on top of the generic
> Core primitives (`peers.send` for a gesture, `peers.channel` for a flow,
> the routed `input.*` facade) specified in [core-api.md](core-api.md).
> Status: settled. This document is the letter the implementation follows;
> the decisions recorded in section 16 are closed. Changing a rule here
> means changing this document first, in the same commit as the code that
> proved it wrong.

## 1. Position

A new workspace crate `onedevice-input`, an official component added to
`official_components()` with the standard spawn contract (spawn token on
stdin, exit when stdin closes or the IPC is lost). Role `input-backend`
(exclusive), scopes exactly the documented profile: `input.serve +
peers.channel + peers.message + devices.read + session.read`.

It speaks ONLY the public Core API, and it is REPLACEABLE by construction:
a third-party engine holding the same exclusive role answers the same
facade, so nothing in the Core, the GUI or the tray knows this
implementation exists. Consequences that shape everything below:

- It has no device key and cannot ask the Core to sign anything. The layout
  document is therefore signed with an engine-level keypair (section 6),
  pinned by channel-authenticated contact, the sync engine's doctrine.
- It never touches the network directly: the layout rides `peers.send`
  (role to same role, so it reaches exactly the peer engines), the live
  flow rides `peers.channel`.
- It owns its persistence entirely (section 11); the Core stores nothing
  about input.
- It never touches an OS input API either. Capture and injection live
  behind one seam (section 10), filled in per platform by ticket #125. Every
  rule in this document is provable against a fake backend, and that is on
  purpose: the OS-independent engine is testable without a desk.

**Identity.** Every protocol object and every persisted key that names a
device uses the device's **node_id**, for the sync engine's reason: it is
the one label the whole account shares, where the Core's `device_id` is not
stable across the account (a server-enrolled sibling is keyed by the
server's id there and by node_id on a serverless sibling, and a logout
re-keys it). `device_id` appears only at the API boundary: `peers.send`
targets, `peers.channel` targets, `peer.message` and `peer.channel`
senders, and the rows the GUI reads. `devices.list` carries both, so the
translation is a lookup. A `peer.message` or a `peer.channel` whose sender
cannot be resolved to a directory entry is dropped.

Because a third-party engine may occupy the same role, every wire message
opens with `"d": "1device-input/1"`. A message in an unknown dialect is
ignored on `peers.send` (debug log) and CLOSES a channel (a live resource
must not be held by two ends that cannot talk). Two devices running
different engines simply do not share a keyboard, which is the
replaceability contract working as intended.

## 2. The two transports, and the frame budget

Two primitives, two jobs, and the split is a measurement rather than a
taste (#123, section 14):

| | `peers.send` | `peers.channel` |
|---|---|---|
| shape | fresh stream, one message, one ack, close | one live duplex pipe, no acks |
| cost | 4.2 ms direct, 32.5 ms relayed, PER MESSAGE | half a round trip, waiting for nothing |
| cap | 64 KiB | **1 KiB per frame** |
| carries | the layout document | the whole live flow |

The reason `peers.send` cannot carry the flow is NOT stream churn: opening
a fresh QUIC stream costs 30 to 50 microseconds. It is that every message
pays a full round trip for its ack, plus one local request and response per
event instead of one local notification. Say it correctly; the epic's
conclusion was right and its stated reason was not.

**The frame cap is 1 KiB**, set by #124 and justified by #123: a frame
carrying a HID usage, a symbol, the modifiers, a layout identity and a
numbered position fits in tens of bytes, and frame size is free in this
range on every path measured (a 256 byte frame costs exactly what a 24 byte
frame costs, direct and relayed alike). The cap is what the use needs, not
what the transport tolerates.

**Encoding: one JSON object per frame.** JSON costs nothing at this size
(#123 measured a 131 byte LSP-framed JSON-RPC notification against 24 raw
bytes on both the Unix socket and the Windows named pipe: identical), it is
the house register for everything control-shaped, a frame is readable in a
debug dump, and unknown fields are ignorable so the dialect can grow. The
cost is that a pointer frame is about 45 bytes where a packed binary one
would be 14, and 45 bytes is free by measurement.

**Two guards on our own caps, because a component that can shoot its own
channel dead is a bug factory.** The Core cuts a channel with
`FRAME_TOO_LARGE` above 1 KiB and with `RATE_EXCEEDED` above 4000 frames
per second for two consecutive seconds. Neither must ever be reachable from
this engine:

- `MAX_OUT_FRAME = 512` bytes, half the Core's cap. A frame we would emit
  above it is a bug: the pointer frame is dropped, the key frame is
  degraded (the symbol is dropped, the usage kept) and both are logged at
  warn. Every variable-length field on the wire is bounded at the source:
  `sym` at 32 bytes of UTF-8, the layout identity at 64 bytes, the key
  name at 32. With those bounds the largest legal frame this engine can
  build is under 300 bytes, so the check is a belt on a fastened belt.
- `OUT_RATE_MAX = 1000` frames per second, a quarter of the Core's cap,
  enforced by a token bucket (section 5). A flow that hits it coalesces
  rather than floods.

## 3. The wire dialect (frozen)

Every frame is one JSON object. `t` names the type. Field names are short
because they are on a 125 Hz path and long because they are read by people:
the balance below is the frozen one. Unknown fields are ignored; a missing
required field makes the frame malformed, and a malformed frame is dropped
with a debug log (never a panic, never a channel cut: a peer is
semi-trusted, and cutting the channel would hand a misbehaving peer a way to
end a session).

### The handshake

| field | type | meaning |
|---|---|---|
| `t` | `"hi"` | |
| `d` | string | dialect marker, `"1device-input/1"` |
| `v` | u32 | highest dialect version this end speaks |
| `caps` | object | what this end's platform backend can do (section 10) |
| `plane` | 32 hex chars | this end's plane id (section 6) |

Both ends send `hi` as their first frame, immediately on attach, without
waiting for the other's. Nothing else is legal before it: a frame arriving
first closes the channel. The session version is `min(v_local, v_peer)`;
v1 is the only version, so `v` is a hook and not yet a negotiation. A `d`
that is not ours closes the channel.

`caps` is what makes the interface able to say what a session cannot do
BEFORE anyone tries: a target whose backend cannot inject the pointer is a
keyboard-only target, and the source can say so instead of discovering it
one refusal at a time.

### Starting and ending a session

| field | type | meaning |
|---|---|---|
| `t` | `"start"` | source asks to drive |
| `s` | u32 | session id: monotonic per channel, starting at 1 |
| `mode` | `"full"` \| `"keys"` | pointer and keyboard, or keyboard only |
| `keys` | `"typing"` \| `"positional"` | key resolution mode (section 8) |
| `plane` | 32 hex chars | the source's plane id |
| `n` | u32 | the flow counter's first value |
| `x`, `y` | i32 | where the pointer enters, in the TARGET's own logical desktop coordinates |

| field | type | meaning |
|---|---|---|
| `t` | `"ok"` | target accepts |
| `s` | u32 | the session id being accepted |

| field | type | meaning |
|---|---|---|
| `t` | `"no"` | target refuses |
| `s` | u32 | |
| `c` | code | `NOT_ALLOWED`, `BUSY`, `PLANE_STALE`, `NO_BACKEND`, `LOCKED` |
| `by` | device_id | on `BUSY` only: who holds it |

| field | type | meaning |
|---|---|---|
| `t` | `"stop"` | the source ends it |
| `s` | u32 | |
| `c` | code | `RETURNED` (hotkey or the pointer crossed home), `MOVED` (on to another screen), `GONE` (the local backend died), `SLOW` (the path degraded past the pointer threshold) |

| field | type | meaning |
|---|---|---|
| `t` | `"end"` | the target ends it unilaterally |
| `s` | u32 | |
| `c` | code | `REVOKED` (the grant was withdrawn), `NO_BACKEND`, `LOCKED`, `IDLE`, `TAKEN` (its own user asked for it back) |

`plane` on `start` is the whole reason absolute positions are safe. `x` and
`y` are in the target's own desktop coordinates, computed by the source from
the layout document; if the two ends do not hold the same plane, those
coordinates mean two different things. So the target compares and refuses
`PLANE_STALE`, and both ends then run a layout round (section 6). This is
the one refusal that repairs itself: the source retries once the round
converges.

### The flow

| field | type | meaning |
|---|---|---|
| `t` | `"p"` | absolute pointer |
| `s`, `n` | u32 | session, flow counter |
| `x`, `y` | i32 | the target's own logical desktop coordinates |

| field | type | meaning |
|---|---|---|
| `t` | `"r"` | relative pointer (games, raw input) |
| `s`, `n` | u32 | |
| `dx`, `dy` | i32 | logical pixels, never both zero |

| field | type | meaning |
|---|---|---|
| `t` | `"b"` | button |
| `s`, `n` | u32 | |
| `i` | u8 | 1 left, 2 middle, 3 right, 4 back, 5 forward |
| `dn` | bool | pressed or released |

| field | type | meaning |
|---|---|---|
| `t` | `"w"` | wheel |
| `s`, `n` | u32 | |
| `dx`, `dy` | i32 | positive is right and up |
| `u` | `"line"` \| `"px"` | what a unit of `dx`/`dy` is |

| field | type | meaning |
|---|---|---|
| `t` | `"k"` | keystroke |
| `s`, `n` | u32 | |
| `u` | u32 | HID usage, `(page << 16) \| id`, 0 when unknown |
| `key` | string | canonical name of a key with no symbol (section 8), absent otherwise |
| `sym` | string | the text the source's own layout produced, absent for a key that produces none. At most 32 bytes of UTF-8 |
| `m` | u16 | modifier bitfield, canonical (below) |
| `l` | string | the source's layout identity, at most 64 bytes |
| `dn` | bool | pressed or released |
| `lk` | bool | present and true for a half-duplex lock: send a press only, expect no release |

| field | type | meaning |
|---|---|---|
| `t` | `"rel"` | release everything you believe I hold |
| `s` | u32 | |

| field | type | meaning |
|---|---|---|
| `t` | `"ping"` | keepalive and latency probe |
| `ms` | u64 | the sender's own monotonic milliseconds, echoed back untouched |

| field | type | meaning |
|---|---|---|
| `t` | `"pong"` | |
| `ms` | u64 | the value from the `ping`, verbatim |

| field | type | meaning |
|---|---|---|
| `t` | `"oops"` | an injection was refused, coalesced |
| `s` | u32 | |
| `c` | code | `ELEVATED_WINDOW`, `SECURE_INPUT`, `SCREEN_LOCKED`, `NO_PERMISSION`, `UNRESOLVED` |
| `k` | u32 | how many were refused with this code in the window |

**The canonical modifier bitfield.** Platform modifiers never travel: each
end maps its own to these bits and back, which is what makes a per machine
remapping a table with two canonical sides (section 8).

| bit | 0 | 1 | 2 | 3 | 4 | 5 | 6 | 7 |
|---|---|---|---|---|---|---|---|---|
| | shift | ctrl | alt | altgr | meta | caps | num | scroll |

`meta` is the Windows key, the Command key and Super: one bit, because they
are one position under three names, and the machine that receives decides
what it means locally.

**`ping` is the keepalive AND the measurement.** The Core sweeps a channel
with no frame in either direction for 10 seconds, so a warm channel has to
say something; making that something a round trip probe costs one extra
frame and buys the number the pointer thresholds need (section 14). `ms` is
the sender's own clock echoed back, so the round trip is computed without
any clock synchronisation between the two machines. Cadence: every 3 s on
a warm channel (comfortably inside the 10 s sweep, even through a scheduler
hiccup), every 1 s while a session is live (so the number the interface
shows stays honest and a degrading path is noticed), and one extra probe the
moment a crossing becomes imminent (a dwell starts), because a 3 s old
figure is not what the decision deserves.

**`n`, the flow counter**, increments once per flow frame (`p`, `r`, `b`,
`w`, `k`) for the life of a session. On an ordered reliable pipe it never
causes a drop by itself, and saying otherwise would be wrong: what drops
positions is the target's own read-side coalescing (section 5), and `n` is
what makes that provably safe. A position is applied only if `n` is greater
than the last one applied, so no reordering, no replay and no future
unreliable transport can walk the pointer backwards.

### Every field a peer chooses is bounded on arrival

Not one of these is theoretical: a peer holding the same role is another
implementation, possibly an older or a stranger one, and these values reach
the OS, the snapshot an interface renders, and this engine's memory.

| Field | Bound on arrival | Over it |
|---|---|---|
| `i` (button) | 1 to 5, the five this dialect defines | the frame is dropped. Not clamped: a clamp would be us choosing a different action on the peer's behalf, and an undefined button number is a real `MOUSEEVENTF_X*` value on Windows and a real XTEST button on X11 |
| `c` (a code) | the closed set of its frame kind (section 13) | mapped to `UNKNOWN`, so a later version's word degrades to one an interface can still say something about, instead of reaching the snapshot as peer-chosen prose |
| `by` | 64 bytes, the length of a node_id | dropped. A refusal with no name is still a refusal |
| `plane` | exactly 32 hex characters | the frame is dropped. A plane id is the whole reason a coordinate means anything |
| `m` (modifiers) | masked to the eight defined bits | the undefined bits are discarded |
| `sym` | 32 bytes of UTF-8 | dropped, keeping the usage (the same rule as on the way out) |
| `l` (layout) | 64 bytes | dropped |
| `key` | 32 bytes | dropped |
| `caps` | 256 bytes, and an object | replaced by `{}`, which reads as a peer that can do nothing. `hi` is the one frame that must always be sent, so refusing it would make the session impossible where degrading it makes the session honestly refused |

## 4. The session state machine, exclusion, and the teardown matrix

### The warm channel

A channel is opened AHEAD of the crossing, never at it. #123: the first
open through a relay takes 134 to 151 ms on a path whose steady round trip
is 4 ms, and a handover cannot wait for that. So the engine keeps a channel
warm to every peer that is, all four at once:

- an attested device of the account, present in `devices.list`;
- reachable (`reachable` true);
- enabled outbound here (`input.drive`, section 9);
- adjacent in the plane, meaning at least one crossing segment exists
  between one of our monitors and one of its (section 7), OR named by an
  explicit `input.take`.

That set is bounded by the account's size, one channel per peer at most
(the Core allows exactly one per (role, peer) pair), and each costs one
`ping` every 3 s. When a peer leaves the set the channel is dropped.

### The two halves, and they are mutually exclusive

Per peer, the source half:

```
Cold ---(peer enters the warm set)---> Warming ---(peers.channel returns,
    hi exchanged)---> Warm ---(a crossing fires, or input.take)---> Starting
    ---(ok)---> Driving ---(stop)---> Warm
                       \---(no)---> Warm, with a backoff and a sentence
```

Per peer, the target half:

```
Idle ---(start accepted)---> Driven ---(stop, end, or any channel death)---> Idle
```

Three exclusion rules, all enforced locally, no negotiation:

1. **At most one `Driving` across all peers.** There is one keyboard.
2. **At most one `Driven` across all peers.** A second source's `start` is
   refused `BUSY` with the holder named, so the interface can say "That
   computer is being driven by another of your computers right now."
3. **`Driving` and `Driven` are mutually exclusive on one machine.** A
   machine being driven refuses to start driving; a machine driving refuses
   `start` with `BUSY`. This is a v1 limit with a large payoff: it makes
   echo suppression a non-problem for the engine (a machine that is being
   driven is not capturing, so an injected event can never be captured and
   forwarded), which matters because on Windows the marker in `dwExtraInfo`
   does NOT survive a relative mouse move (#123), so a backend cannot
   recognise its own injected motion that way. Chaining three machines
   through one keyboard would need that recognition; v1 does not offer
   chaining, and #125 is not asked to solve it.

**Handover is explicit, and there is no preemption.** `BUSY` is `BUSY`. A
second source waits for the holder to stop, for its channel to die, or for
the target's session idle to fire. The alternative, letting a second machine
take the keyboard out of a live session, is a way for one machine to
interrupt a human typing on another, and no automatic rule can tell that
apart from the case it is meant to serve. The interface says who holds it;
a human decides.

**The target's session idle.** No frame from the source for
`SESSION_IDLE = 5 s` ends the session with `end IDLE` and releases every
key. Tighter than the Core's 10 s channel sweep on purpose: a source that
went quiet mid-session is a hung source, and a target sitting with Control
held is exactly the failure this feature must not have. A live session
pings every 1 s, so a healthy quiet session never trips it.

**The source's own watchdog, and it is a security property.** No `pong` for
`SOURCE_STALL = 2 s` while driving and the source brings the keyboard home
by itself: unpin the pointer, restore local input, warp the pointer back to
the edge it left, say the sentence. Together with the return hotkey (section
8) this is the whole of "a hung or misbehaving target must not be able to
keep your keyboard", and neither half is negotiated with the target.

### The teardown matrix

A `peers.channel` can die in ten ways, and every one of them has to end
with a working keyboard on both machines. The invariant, stated once:

> **Every path out of a session goes through one function.** On the target
> that function releases every key the engine believes it is holding, in
> reverse press order, and clears the held file. On the source it unpins the
> pointer, stops swallowing, warps the pointer to the point it left from,
> and clears the session. There is no second way to end a session, so there
> is no path that can forget.

| Core reason | what the SOURCE end does | what the TARGET end does |
|---|---|---|
| `CLOSED` | our own end closed (our process is stopping, or we closed the pipe). Bring the keyboard home. No `stop` can be sent: the pipe is gone, and the target's own end is what tells it | its half ended: release all keys, back to `Idle` |
| `REPLACED` | a newer channel took the pair. The session on the old channel is over: bring the keyboard home, then let the state machine re-warm | release all keys, back to `Idle`. The successor channel starts from `hi`, so no session survives a replacement |
| `PEER_GONE` | the target vanished (its component left, its Core stopped, the path broke). Bring the keyboard home AT ONCE and say so | not reachable on this side: we are the vanished one |
| `DEVICE_REVOKED` | the peer is no longer a device of the account. Bring the keyboard home, drop the warm channel, drop the peer's monitors from the plane, **delete the grant and the outbound enablement for that node_id** | release all keys, **delete the grant**, drop its monitors. A grant dies the instant the device is revoked, which is the epic's rule and it is enforced on both sides of the pair |
| `LOGGED_OUT` | this device logged out. Bring the keyboard home, drop every channel. Grants survive (they are keyed by node_id, not by the re-keyed `device_id`) | release all keys |
| `ACCOUNT_LEFT` | this device was struck off. Bring the keyboard home, drop every channel, and **drop every grant and every enablement**: fail closed, so re-joining does not silently restore a door | release all keys, drop every grant |
| `SHUTDOWN` | the Core is stopping. Bring the keyboard home, then exit (stdin will close too) | release all keys |
| `FRAME_TOO_LARGE` | our encoder or the peer's produced an oversized frame. Bring the keyboard home, log at warn, and back off before re-warming: this is a bug or a hostile peer, not a hiccup. Our own `MAX_OUT_FRAME` guard makes our half unreachable | release all keys, log at warn |
| `RATE_EXCEEDED` | our coalescer failed or the peer flooded. Same treatment. Our own `OUT_RATE_MAX` guard makes our half unreachable | release all keys, log at warn |
| `IDLE_TIMEOUT` | our keepalive did not keep the channel alive (a wedged write, a stalled task). Bring the keyboard home, re-warm | release all keys |

Two more deaths that are not the channel's:

- **`NO_DIRECT_PATH`**, at the open (a deployment whose relays are
  rendezvous-only above a cap, #88) or mid-stream (its watcher closed a
  connection whose direct path died). No session, and the sentence is the
  deployment's, not a mystery. The channel is not retried on a timer: it is
  retried on the next `devices` change or the next gesture.
- **The component's own death.** The OS keeps an injected key down after
  the injector exits, so the target writes the held modifier set to disk
  before pressing (section 11) and releases whatever it finds there at the
  next start.

## 5. The pointer: coalescing without a pace, and numbering

### Never pace on a timer

#123's surprise: a `tokio::time::sleep` wakes 1.158 ms late at p50 under
WSL2 and 1.829 ms late on bare metal, against 0.35 ms of network jitter.
**The sender's own timer jitters more than the network.** So the flow is
never driven by a tick. It is driven by capture events, and it is shaped by
two rules:

1. **Coalesce by superseding.** At most one pending position exists. A new
   position replaces the pending one; nothing queues.
2. **A token bucket read on arrival, never slept on.** When a capture event
   arrives, the tokens accrued since the last emission are computed from the
   monotonic clock (elapsed times the rate, capped at a small burst of 2).
   With a token, the position goes out at once. Without one, it becomes the
   pending position. No task ever sleeps to decide this.

The rate is measured rather than chosen: **250 Hz when the last measured
round trip is under 10 ms, 125 Hz above it.** #123 carried 125 and 250 Hz
cleanly on every path; 1000 Hz stayed clean on a direct path and produced,
over a relay, one freeze above 20 ms with 19 stale frames queued behind it.
Halving the ceiling on a slow path halves the queue such a freeze can build.

**One timer exists, and it is not a pace.** When the flow stops, the last
pending position must still be delivered, or the pointer would come to rest
one event short of where the hand put it. A single trailing-edge flush
timer, armed only while a position is pending, delivers it. Its 1 ms of
jitter is invisible by construction: by the time it fires, nothing is
moving. That is the entire exception, and it is why the prohibition is
worded "never pace the flow on a timer" rather than "no timers".

### The flush rule

A `b`, `w`, `k`, `rel` or `stop` frame **flushes the pending position
first**, and is itself never coalesced and never dropped. A click at the
previous position is a click in the wrong place, and that is the kind of bug
that makes a feature untrustworthy.

### The target coalesces on receipt too

A freeze on a relayed path delivers a burst of positions at once. Injecting
all of them costs real time (`SendInput` is about 100 microseconds per event
and #123 proved it does not amortise: a batch of 8 moves in one call costs 8
times one move) and shows as a rubber band. So the target drains what is
readable and applies the batch with one rule:

> **Drop a pointer frame when the very next frame in the same batch is also
> a pointer frame.**

Everything else is applied in arrival order, so a key or a button is always
applied at the position it was sent from. Combined with the `n` check
(apply a position only if `n` exceeds the last applied), the pointer is
provably monotonic in time and never lands somewhere it was not sent.

### While driving, the source integrates deltas

The source pins its own pointer (`confine`) so it does not walk off its own
desktop, which means its absolute position stops being meaningful. The
engine keeps a virtual cursor in plane coordinates, starts it at the
crossing point, and integrates the relative deltas the backend reports. The
virtual cursor is clamped by the crossing graph (a wall stops it), and
crossing back over the boundary of one of our own monitors brings the
keyboard home. The target's own coordinates are computed from the virtual
cursor at emission time. A relative move of (0, 0) is never emitted:
Windows discards one and it reaches no hook at all (#123), so it can only
ever be noise.

## 6. The layout document

One plane, in logical pixels, holding every monitor of every computer of the
account. Not one box per machine: the epic's decision, from the first day.

### The shape

```json
{
  "d": "1device-input/1",
  "t": "layout",
  "key": "<the sender's own engine public key, 64 hex>",
  "monitors": {
    "<node_id>": {
      "seq": 7,
      "list": [
        { "id": "DP-1:9f3a…", "name": "Dell U2720Q",
          "w": 2560, "h": 1440, "x": 0, "y": 0,
          "scale": 1500, "primary": true }
      ],
      "sig": "<base64url>"
    }
  },
  "placement": {
    "seq": 12,
    "by": "<node_id>",
    "at": 1755400000000,
    "spots": { "<node_id>/<monitor id>": { "x": -2560, "y": 120 } },
    "sig": "<base64url>"
  }
}
```

- `w`, `h` are the monitor's **logical** size, `x`, `y` its position in its
  own machine's desktop, `scale` its scale factor in **permille** (1500 is
  150%). Integers only, everywhere: a float on a signed wire is a
  round-tripping trap, and this document is signed.
- `id` is the stable identity, and stability across unplugging is the whole
  point: the output name plus a hash of the EDID where one is readable
  (`CGDisplayCreateUUIDFromDisplayID` on macOS, RandR output plus EDID on
  X11, the display device path on Windows). A monitor whose identity cannot
  be made stable is reported as such by the backend and degrades VISIBLY
  (the interface says the screens on that machine may swap places), never
  silently.
- `at` is informational, for the interface. It is never compared: clocks
  across machines are not comparable, and a design that quietly relies on
  them is a design that fails at the worst moment.
- `key` is the SENDER's own engine public key, and it is what creates every
  pin: the message arrives named by the receiving Core's directory, so the
  key inside it is that device's word about itself. It is the sender's own
  and never the author's, so a relayed document pins the courier and nobody
  else. It is part of the frozen shape rather than of D11's prose alone,
  because every pin in this section depends on it travelling.

### Two authorities, and they do not overlap

**Each device's own signed word for its own monitors.** `monitors[n]` is
accepted only if `sig` verifies under n's pinned engine key and `seq` is
greater than the stored one. Nobody else can write it, and nobody else can
invent one: a `monitors` entry for a node with no pin is held, unverified,
until that node is pinned by direct contact (which the warm channel and the
layout rounds produce for every reachable pair).

**Last writer wins on the whole plane for the arrangement a human drags.**
`placement` is one object, compared by `(seq, by, sig)`: the greater `seq`
wins, a tie is broken by the greater `by` (lexicographic on the hex node_id),
and a tie there by the greater `sig`, so every device converges on the same
plane from the same set of messages, in any order, with no coordination. A
human who drags writes `seq = max(seen) + 1`.

**The signature is in the order and it has to be**, which is easy to get
wrong and was: `(seq, by)` is a total order on the PAIR and not on
placements, so two DIFFERENT arrangements from the same author at the same
seq can never displace each other and the first to arrive wins. Two devices
that saw them in opposite orders then hold different planes with different
plane ids for ever, every session between them refused `PLANE_STALE` with
nothing to repair it, because each keeps re-offering a document the other
ignores. `sig` is a function of the content, so it makes the order total on
the thing being ordered. Nothing device-local is ever in it (in particular
not `verified`), because an order that depended on which peers a device has
met is exactly the order that makes two planes diverge.

Bounds, so a peer cannot grow the document: at most 16 devices in
`monitors`, at most 16 monitors each, at most 256 entries in `spots` (16
devices of 16 monitors can legitimately fill it, and a smaller bound would
drop the spots of screens that are away, which keep their place), at most 256
bytes of monitor `id` and 128 of `name`, and at most 32 KiB of `placement`
(which a full 256 spots of ordinary monitor ids fits inside; 256 spots of
MAXIMAL ids is the one combination no bound could fit a 64 KiB transport).
A document that breaks a bound is refused whole, monitors and placement
together: refusing half a document would let a peer over the bound slip its
arrangement in while its monitors were refused, and the arrangement is the
half that decides where the pointer goes.

The bounds are all functions of the CONTENT, deliberately, so every device
refuses the same document. Two further limits are therefore expressed as
budgets on what is SENT rather than as refusals at the door: the stored
`monitors` map keeps at most 16 devices (an unverified entry may be displaced
by a smaller node_id, a verified one never is, since only unverified entries
are outside the plane and the plane id), and what a device offers carries its
OWN entry first and then as much of the rest as fits one `peers.send`. Being
unable to announce its own screens is the one failure nobody else can repair
for a machine; relaying somebody else's word is a convenience the epidemic
does not depend on.

A draft of this section had a `spots` key whose node_id is in no directory
entry of this account "dropped on merge". It must NOT be, and the code never
did it: dropping against the LOCAL directory would make the signed bytes, and
so `plane_id`, depend on which devices each machine currently knows, which is
permanent `PLANE_STALE` between two machines that disagree about the account
for a second. The drop happens at LAY-OUT time instead (section 7): a spot no
verified record claims is not placed, and the document keeps it.

**A `placement` whose signer is unknown here is still adopted, marked
unverified.** This is a deliberate divergence from the sync engine's
fail-closed pinning, and the reason is that the two documents are not the
same kind of thing. A sync membership record grants access to file bytes; a
plane grants nothing at all. The worst a forged placement can do is put a
screen in the wrong place, which a human sees and fixes with a drag, while
refusing to converge would break sessions between two innocent devices
because a third is absent (its signature unverifiable, its arrangement
therefore unusable, the two ends stuck on different planes and every session
refused `PLANE_STALE`). Signing `placement` remains worth it for attribution
(the interface can say who arranged the plane rather than who claims to
have) and because a device that IS pinned cannot then be impersonated: a
signature that is present and WRONG is refused outright.

**A pin re-examines the arrangement, every time, and that flatness is the
rule.** When a key arrives for the author of the arrangement this device
holds, the arrangement is verified against it: a forgery adopted on faith is
thrown out, and so is a genuine one whose author has since replaced its key.
The plane then falls back to the derived arrangement, which one drag replaces.

A draft had it kinder, keeping an arrangement this device had ALREADY proven
across a key replacement, on the reasoning that it had been proven under the
key that was current when it arrived. That is where the rule stopped being a
function of the records: whether an arrangement was verified depends on
whether this device held the author's OLD key when it arrived, so a device
reinstalling split the account into the machines that had verified its
arrangement and kept it and the machines that had adopted it on faith and
threw it out, with two plane ids, every session across the split refused
`PLANE_STALE`, and nothing to repair it. Convergence is the property
everything else here rests on, and the kindness was only ever a comfort.

A draft also required the message to come from a peer this device had already
pinned. Implementation showed that condition is vacuous and it was removed: a
peer's engine key travels in the peer's own message, so any device of the
account makes itself pinned by the act of speaking, and the check could never
refuse anybody. The real bound on who may say anything here is that every
sender is an attested device of the account, which the Core guarantees before
a byte reaches this engine.

`monitors` entries are never adopted unverified: a device's own word about
its own screens must be its own, and an unverified entry is held, shown as
pending at most, never placed on the plane, never counted in the plane id and
never crossed to.

### The signed bytes

Not canonical JSON. The two signed objects are small and entirely ours, so
they get an explicit byte encoding instead, which is shorter to specify,
impossible to get subtly wrong, and free of the integer-profile trap that
canonical JSON's number handling carries:

```
monitors entry:  "1device-input:monitors:" || node_id || u32be(seq)
                 || u32be(count) || for each monitor, in the list's order:
                    u16be(len(id)) || id || u16be(len(name)) || name
                 || i32be(w) i32be(h) i32be(x) i32be(y) i32be(scale)
                 || u8(primary)

placement:       "1device-input:placement:" || by || u32be(seq)
                 || u32be(count) || for each spot, sorted bytewise by key:
                    u16be(len(key)) || key || i32be(x) || i32be(y)
```

Length prefixes everywhere, so no two field values can be confused for one
another; a constant domain prefix per kind, so the one engine key cannot be
made to sign one kind and have it verify as the other; and the spot order is
fixed by sorting, so the encoding is a function of the content.

`plane_id` is `blake3(the placement's signed bytes || every monitors
entry's signed bytes, in node_id order)`, truncated to 16 bytes and rendered
as 32 hex characters. Two devices holding the same records compute the same
id by construction, which is what makes the `start` frame's plane check
meaningful.

### Replication: one message, merged idempotently

One `peers.send` message, and it is the document itself: `{ "d", "t":
"layout", "key", "monitors", "placement" }`, exactly the shape at the top of
this section, with no `doc` wrapper around it. (An earlier draft of this
paragraph nested the document under a `"doc"` key, which contradicted the
JSON block a few lines above it and the merge that reads the top level.) The
receiver merges it. No head, no need queue, no delta protocol: a realistic
desk is 3 devices of 2 monitors, a few hundred bytes, so a delta protocol
would be complexity bought with nothing.

`peers.send` carries 64 KiB, and the caps of this section do NOT by
themselves fit inside it: 16 devices of 16 monitors with identities of the
length Windows really produces is over 100 KiB of JSON. That is why the
offered document has a byte budget as well as counts (above): a device always
announces its own entry and relays what fits, so the message is inside the
transport by construction rather than by hope.

A round happens when there is something to converge: at start, when a peer
becomes reachable, when our own monitors change, when a human drags, when a
merge changed anything (epidemic convergence, which is what makes a plane
reach a device that was offline when the drag happened), and on a slow
periodic sweep every 60 s as the backstop. Rate limited to at most one
message per peer per 2 s, coalesced. The whole thing terminates because
`seq` only increases and a merge that changes nothing sends nothing.

### Absent screens keep their place

A monitor whose device is offline, or which its device's current `monitors`
list no longer names (it was unplugged), stays in the plane at its spot and
becomes a **ghost**: nothing else moves, and every crossing segment into it
is a wall. "This screen is not connected right now. Its place is kept." A
ghost between two live monitors makes the far one unreachable, and that is
the correct outcome: the pointer stops at a wall instead of being swallowed
by a screen that is not there.

**A ghost needs a spot, so only a screen a human placed can keep its place.**
On a plane nobody has ever dragged, an unplugged screen simply leaves, and the
remaining blocks are re-derived without it. That is a real limit and it is not
an oversight: the derived arrangement is a function of the `monitors` records
ALONE, which is what makes two devices compute the same plane from the same
messages, and remembering a screen that is no longer in any record would mean
remembering something outside them. Two devices would then disagree the moment
one of them had met a screen the other never saw, which is the one failure this
document cannot afford (the plane id would agree while the layouts differed,
so nothing would even notice).

Worth knowing which way the discomfort points: the person who never opened the
Input tab is exactly the person who unplugs a screen, so the case is real. What
they get is a plane that rearranges itself rather than one that keeps a gap,
which is visible and self-correcting, where the alternative is two machines
silently disagreeing about where the pointer goes. One drag fixes it for good,
and the interface has every reason to invite one.

### Monitors with no spot

A device whose monitors were never placed by a human must land somewhere,
and every device must compute the same somewhere or their planes diverge. So
the placement is DERIVED, deterministically, from the records alone: unplaced
devices are appended to the right of the current bounding box in ascending
node_id order, each device's block translated so its own desktop's top-left
corner sits at (bounding box right edge, 0). The block keeps the machine's
own arrangement, which is the epic's rule: Windows and macOS already know
where their screens are, and that arrangement arrives as a movable block
rather than being re-invented. Derived placement is not written into
`placement`; only a human's drag writes that.

## 7. The crossing graph and the guards

### Derived, never typed in

The plane is one coordinate system, so the graph is computed from the
rectangles. For our monitor M's right edge (and symmetrically for the other
three sides):

- normalize first: any monitor whose left edge is within `SNAP = 16`
  logical pixels of M's right edge counts as abutting it. Without a
  tolerance an imported arrangement would need pixel-perfect dragging to
  work at all;
- the crossing segment is the intersection of the two y intervals,
  `[max(M.y, N.y), min(M.y + M.h, N.y + N.h))`. A stretch of M's edge with
  nothing across it is a **wall**;
- overlapping rectangles are not a crossing case: only monitors at or
  beyond M's right edge are considered, so a plane a human dragged into an
  overlap degrades to fewer crossings rather than to nonsense.

**The projection is the identity on the shared stretch, and that is what
"proportional over the portion two monitors actually share" means on a
plane.** The epic's phrasing comes from the prior art, which has no plane
and must therefore map the fraction along one edge to the same fraction
along another. On one shared coordinate system that stretching would move
the pointer vertically at a crossing, which is precisely what a plane exists
to avoid: a pointer leaving M at plane-y Y enters N at plane-y Y, and
different monitor heights are handled by the shared stretch being shorter
than either edge. The derivation, not the formula, is the epic's point:
computed from the geometry rather than typed in.

Scale differences need no conversion either: the plane is in logical pixels,
a logical pixel is about the same physical size everywhere, and the
conversion to device pixels happens on the target at injection. Mixing a 4K
at 100% with a Retina at 200% therefore does not change how the mouse feels,
which is the epic's requirement.

### The guards, per neighbour, evaluated as a chain

A crossing fires only when every guard passes, in this order. The order is
part of the contract: an earlier guard's refusal is the one the interface
reports.

1. **Locked to screen.** `input.lock` pins the pointer where it is, for
   games and virtual machines. Nothing crosses. A global toggle.
2. **Wall.** No crossing segment at this point, the segment leads to a
   ghost, or a human made this neighbour a wall (`wall`). All three are one
   guard and one rank in the chain: they differ in the sentence the interface
   says, not in when they are asked. A candidate whose stretch does not cover
   this point contributes no sentence at all, so a wall on one neighbour never
   explains a refusal against another.
3. **Dead corner.** Within `dead_corner = 16` logical pixels of either end
   of the segment. Corners hold the Start menu and the macOS hot corners,
   and a feature that steals them is a feature people turn off.
4. **Required modifier.** If `require_mods` is non-zero, exactly those
   canonical modifier bits must be held.
5. **Double tap.** If `double_tap_ms` is non-zero, the pointer must have
   touched this segment, left it, and returned within that many
   milliseconds. Off by default: two ways to say "not by accident" is one
   too many for a default.
6. **Dwell.** The pointer must remain against the segment for `dwell_ms`.
   Default **250 ms**, which is where the prior art converged.
7. **Warm.** The target's channel must be warm and its session startable.
   If the channel is not warm, the engine starts warming it when the dwell
   starts and the crossing waits for both, bounded by
   `CROSS_OPEN_BOUND = 1 s`. A LAN open finishes inside the dwell; a cold
   relay open (134 to 151 ms, #123) finishes inside the bound; anything
   slower is refused with a sentence while the channel keeps warming in the
   background, so the second attempt succeeds.

**How edge pressure is even detected**, since it is not obvious: while the
machine is only watching, the OS clamps its own pointer at the boundary of
its own desktop, so the position reported never goes past the edge. What the
engine reads is the position PLUS the motion event's delta, which does go
past it, and that intended position is what the graph is asked about. So
nothing has to be pinned while a dwell runs: the OS holds the pointer at the
boundary for free, and the guards are about a pointer that is already
resting there. Confinement starts when a session starts, and its job is
different (keeping the pointer from moving on this machine's desktop while
it is driving another).

The one case that escapes this is a plane a human dragged into disagreement
with a machine's own desktop, putting another computer's screen where this
one's desktop continues. The OS then clamps somewhere else entirely and the
crossing edge is in the middle of a desktop with nothing holding the pointer
there. It is detected the same way (position plus delta) and simply crosses
without a rest, which is the honest outcome: the remedy is one more drag,
and the interface's plane is where a person can see the disagreement.

Pinning and warping are the platform's half (#125); the decision is here.

The guards are stored per neighbour, keyed by `(our monitor id, their
node_id, their monitor id, side)`, and set through `input.guards`. A pair
with no stored guards uses the defaults above.

## 8. The keyboard

### One frame carries every level

The wire carries HID usage, canonical key name, symbol, canonical
modifiers and layout identity together, in about 80 bytes. No negotiation,
no round trip, nothing thrown away: the target picks the level it can serve.

### The resolution table

Two modes, declared by the source in `start`, because the source is where
the human made the gesture (and where "this is a game session" is known):

**`typing` (the default).** What a person means when they type.

| the frame has | the target tries, in order |
|---|---|
| a `sym` | 1. a key and modifier combination producing `sym` on the target's active layout; 2. direct Unicode injection of `sym`; 3. the `u` usage positionally; 4. the `key` name |
| no `sym` (a named key) | 1. the `key` name; 2. the `u` usage positionally; 3. nothing, and report `UNRESOLVED` |

**`positional` (games, positional shortcuts).** What a real HID keyboard
would have sent.

| the frame has | the target tries, in order |
|---|---|
| anything | 1. the `u` usage; 2. the `key` name; 3. a combination producing `sym`; 4. direct Unicode injection of `sym` |

**This is a correction of the epic, made on purpose and recorded here.**
The epic states the preference order as "HID usage, then virtual key, then
Unicode" and, two lines later, gives the example that typing `@` on an
AZERTY target from a QWERTY source is AltGr plus 0. Those two statements
cannot both hold under one order: the source pressed the position of the
`2` key with Shift, and on AZERTY that position with Shift produces `2`, not
`@`. Symbol-first is what makes the epic's own example work, and it is also
right for shortcuts (Ctrl plus the key labelled C is what a person means by
copy, on any layout). The epic's order is the `positional` mode's order, and
the mode is what chooses which level leads, which the epic also says ("the
target picks according to its backend and its mode"). Nothing is lost: every
level still travels in every frame.

"Virtual key" does not appear on the wire, because it is not portable (a
Windows virtual key, an X11 keysym and a macOS virtual keycode are three
things). Its job, naming a key that produces no character, is done by `key`,
a canonical name, and by `u`, since the HID keyboard page covers Enter, Tab,
the function keys, the arrows, Home and End (the consumer page covers the
media keys, which is why `u` carries the page as well as the id). Both
travel: `key` is cheap, self-describing in a debug dump, and lets a target
whose usage table is incomplete still do the right thing.

Canonical key names are a frozen table in the code, ASCII, at most 32 bytes:
`Enter`, `Tab`, `Escape`, `Backspace`, `Delete`, `Insert`, `Home`, `End`,
`PageUp`, `PageDown`, `Up`, `Down`, `Left`, `Right`, `F1` to `F24`,
`PrintScreen`, `Pause`, `Menu`, `NumpadEnter`, the lock keys, and the media
keys. An unknown name is not an error: the target falls through to the next
level of the table and, failing that, reports `UNRESOLVED`.

### A keystroke is a sequence, not an event

Resolving `@` on an AZERTY target gives "AltGr plus the 0 key". Injecting
that means:

1. release the **layout** modifiers the target is holding that the
   combination does not want;
2. press the modifiers the combination wants and the target is not holding;
3. press the key, and release it (unless the frame is a lock, `lk`);
4. restore the modifiers the target was holding before step 1, and release
   the ones step 2 added.

Step 4 is what stops a session from leaving a machine in a state nobody
asked for, and it is also why the held set (below) is authoritative rather
than derived from the frames.

**Step 1 says "layout modifiers" and it has to**, which implementation
proved and this paragraph records. The modifiers split in two:

- **layout modifiers**, Shift and AltGr, which choose WHICH SYMBOL a key
  produces. A resolution that does not want them means it: holding Shift
  while typing `@` on AZERTY would produce something else.
- **command modifiers**, Control, Alt and Meta, which change what a stroke
  MEANS rather than which character it makes. A resolution that does not
  name them is not saying they are unwanted, it is saying nothing about
  them, so they are left exactly as they are.

Without that split the two halves of this section contradict each other:
symbol-first resolution is what makes Control plus the key labelled C be
copy on any layout, and releasing Control in order to produce a `c` turns
that copy into a letter, on the most used shortcut there is. Command
modifiers travel to the target as their own key frames anyway (a Control
press is usage 0xE0) and the held set carries them across frames.

The known cost, recorded rather than hidden: on macOS the Option key IS a
layout modifier (Option plus e is a dead key for an acute accent), so a Mac
target holding Option while a symbol resolves without it produces the wrong
character. That is the lesser of the two failures, it only arises while a
human on the SOURCE holds Option, and accents reach a Mac through the
resolution's own dead-key prefix or through the Unicode fallback in any
case.

**Dead keys** are the same machinery with two strokes: a symbol reachable
only through a dead key resolves to the dead key's combination followed by
the base key's. **Layout groups**, where the target's active group does not
contain the symbol at all, are the same machinery with a switch: the engine
switches the group, injects, and **switches it back**, because a session
that silently changes the machine's keyboard layout is a session that
breaks the next thing its owner types.

**Half-duplex locks** (Caps Lock, Num Lock, and Scroll Lock on some
keyboards) send a press only; the frame carries `lk` and the target injects
a press with no release. A lock is never in the held set: releasing it later
would toggle it, which is the opposite of hygiene.

**The target applies that rule to any key whose resolution names a lock,
whether the frame carried `lk` or not.** Hygiene is a rule the target owns, so
it must not depend on the source's manners: a third-party engine holding the
same role, or our own capture on a backend that reports Caps Lock as a full
press and release pair, would otherwise get a lock into the held set and have
the teardown send a Caps Lock release, toggling the machine into caps on the
way out.

The residual ambiguity, recorded rather than hidden: a **physically remapped
Caps Lock** (caps to control, which is common on Linux) travels as the Caps
Lock usage and the target treats it as a lock. The wire has no way to say
"this position is not a lock here", so a backend should report the usage of
what a key MEANS on that machine wherever it can tell.

**Per machine modifier remapping** is a table with two canonical sides,
stored on the machine being driven, per source device
(`input.remap { device_id, map }`). It is the first thing anyone needs the
moment a Mac and a PC share a desk (Command against Control). It is applied
on arrival, before resolution, so everything downstream sees one modifier
vocabulary. The default is the identity: guessing that a Mac target wants
Control and Command swapped is an opinion, and an opinion that is wrong for
half of its users is worse than a switch.

**The map must be injective on the holdable modifiers, and `input.remap`
refuses one that is not.** It is applied as a simultaneous permutation, so
mapping Control to Meta and Meta to Control SWAPS them, which is the case
people actually want. But a map that sends two modifiers to one (Control to
Meta, with Meta left alone) makes a modifier vanish when both are held, and it
vanishes silently, on the very path where someone was trying to make a Mac
behave. The function cannot fix that once it is asked, so the gesture refuses
it.

### Modifier hygiene, which is the thing that bites hardest

The target keeps a **held set**: the platform keys it has itself pressed and
not released, in press order. Not the modifiers the frames announced, and
not the OS's keyboard state: what WE pressed, which is the only thing we may
release. A key the machine's own user is holding is none of our business.

On session end, on link loss, on timeout, on any of the ten channel deaths,
on an explicit `rel` frame, **and on a layout change**, the set is released in
reverse order and cleared. That is one function, called from one place
(section 4).

The layout change belongs on that list for a reason worth stating, because it
is the one entry that is not about a session ending: the set holds the
platform's own key identities, and a re-resolve after the keymap changed can
name the same key differently. A held key that no longer matches is a key
nothing will ever release. Letting go of everything at that moment costs one
spurious release, which every platform treats as a no-op.

The set is also the crash guard. An injected key stays down after the
injector exits, so the modifiers in the set are written to
`held.json` **before** the press that adds one, and the file is drained with
a release-all at the next start.

**What is written is the set at its WIDEST point during the sequence, not
the set the sequence ends with**, and the `@` stroke is exactly why: it
holds AltGr in the middle and holds none of it at the end, so a process
death between the two would strand AltGr on a machine whose `held.json`
never mentioned it. Writing the peak costs one unnecessary release at the
next start in the ordinary case, which every platform treats as a no-op.

Two deliberate limits:

- **Modifiers only.** An ordinary character key is down for milliseconds, so
  the window in which a crash could strand one is negligible and its damage
  is one repeated character. A modifier is down for seconds and its damage
  is a machine with a dead keyboard, which is the classic failure of every
  tool in this category. Writing the file per character would put a file
  write on the typing path for no gain.
- **No fsync.** The failure guarded against is a PROCESS death, and the page
  cache survives that. A machine that loses power releases every key by
  rebooting, so the durability the sync engine's state needs is durability
  this file does not.

### The return hotkey

Recognised in the captured stream by the machine that CAPTURES, swallowed
there, and never forwarded and never negotiated. It works while a session is
live, which is exactly when the source sees every key, and it works when the
channel is dead, because nothing about it involves the channel.

A chord is a SET of modifiers plus one key, so `input.hotkey` does not care
what order it is given and `input.status` always reports one spelling: one
binding must not have two renderings, or an interface shows two chords for one
setting. The order it reports is the one all three desktops write chords in,
Control, then the alternate keys, then Shift, then the platform key (Windows
writes Ctrl+Alt+Shift+Win, macOS menus render Control, Option, Shift,
Command). Deliberately NOT the order of the modifier bits, which would put
Shift first because it is bit 0 and produce "Shift+Ctrl+Home", which nobody
writes.

The default is **Ctrl + Alt + Escape**, and it is configurable through
`input.hotkey`. The reasoning, since every candidate is taken somewhere:
Ctrl + Alt + Delete is reserved by Windows and unreachable from a hook,
Command + Option + Escape is Force Quit on macOS, and a bare modifier double
tap fires by accident. Control rather than Command is what makes one default
work on all three desktops.

## 9. The grants

**Who may drive this computer** is stored HERE, per source device and per
direction, and is **never replicated**. It is the security boundary of the
whole feature: driving a computer means typing on it, which is remote code
execution with a friendly interface, and it is therefore never implied by
account membership. A replicated grant would be a door openable from
somewhere else.

- `input.allow { device_id, allowed }` writes it, on the machine that will
  be driven. Keyed by node_id.
- A `start` from a device with no grant is refused `no NOT_ALLOWED`. The
  driving side **learns by trying**, and gets a clean refusal with a
  sentence rather than silence. Nothing in the handshake hints at it: a
  grant can be withdrawn between a hint and its use, so a cached answer
  would be wrong exactly when it mattered, and the interface must not imply
  that the far side's list is knowable from here.
- A grant dies the instant the device is revoked from the account
  (`DEVICE_REVOKED`, section 4), and every grant dies when this device
  leaves the account (`ACCOUNT_LEFT`). Fail closed, both.
- It is visible in the tray for the whole time it is used, which is the
  epic's rule and the interface's job.

**Who this computer may drive** is a local convenience, `input.drive`. It
holds which peers we are willing to hand our keyboard to (which gates the
warm set), and the per peer session settings (the key mode). It grants
nothing anywhere, and the interface must not imply it does. Consenting to be
driven says nothing about driving: the invitation is not a door that opens
both ways here, unlike a sync set.

## 10. The platform seam

One trait pair, a backend per platform chosen at compile time in `os.rs`
exactly as `clipboard/src/os.rs` does, and a **fake backend** that stands in
for all of it in the tests. Ticket #125 fills in `x11.rs`, `windows.rs` and
`macos.rs` behind this seam without touching a caller.

**Unlike the clipboard's, this seam never fails, and the component never
exits when the OS half is missing.** A first draft of this section said it
would (an `Unsupported`, a clean exit, and the supervisor simply not
registering the component); that was wrong twice over, and both halves are
worth writing down.

It is wrong about the supervisor: registration is a static per-OS list read
at the Core's start, and the only thing that skips a component there is its
binary being absent. A component that exits is RESTARTED, with a backoff
capped at a minute and a reset that an instant exit never reaches, so a
machine with no OS half would relaunch it every minute for the session's
whole life. (The clipboard does exactly that today on a headless or
Wayland-only Linux box; here it would be worse.)

And it is wrong about the product: a machine with no OS half still has real
work. Its interface has to be able to say "nothing here can type", which it
can only do if something is answering `input.status`. Its screens still
belong on the shared plane, so its siblings know where it is. And the
permissions a person granted still have to be held and honoured, because they
are the security boundary of the whole feature and they outlive any one
build.

So `os::create` always succeeds and hands back a backend that reports what it
CANNOT do, with a `problem` naming why. The engine reads the capabilities and
behaves accordingly: it opens no channel for driving (it could never drive
anyone), it refuses an incoming session with `NO_BACKEND`, it calls no
downcall the capabilities do not offer, and it does everything else exactly
as it would with a real backend. That is the same path a real backend takes
when its OS grant has been refused, which is the second reason to have only
one.

The shape follows the clipboard's, for the reason the clipboard has it: the
OS event loop is pinned to the main thread (a message pump on Windows, a run
loop on macOS, a non-`Send` X connection on Linux), the engine runs in tokio
on another thread, and the two are bridged by a cheap `Clone` handle for
downcalls and an `mpsc` of events for upcalls.

### Downcalls, fire and forget

Cheap, callable from any thread, no result. Injection is on this path
because a result would cost a round trip to the OS thread per event, and
`SendInput` itself is only about 100 microseconds (#123): refusals come back
as upcalls instead, coalesced, which is all the interface needs.

- `capture(mode)`: observe, observe and swallow, or stop. Swallowing is
  what keeps the source's own keystrokes from acting locally.
- `confine(rect)`: pin the pointer, `None` releases it.
- `warp(point)`: put the pointer somewhere (the return, and leaving an
  edge).
- `inject(actions)`: pointer moves, buttons, wheel, key presses and
  releases, in order, resolved already.
- `release_all(keys)`: the hygiene path, and it must work even when the
  engine is halfway through anything else.
- `request_exit(code)`: ask the main-thread loop to end.

### Downcalls that answer, and are therefore cached

- `monitors()`: the list, with stable identities. Called at start and on
  `MonitorsChanged`.
- `pointer()`: the current position, called when capture starts.
- `resolve(want)`: **the layout query**, and the reason the seam is split
  where it is. "What key and what modifiers produce this symbol on this
  machine's active layout right now" is platform knowledge
  (`VkKeyScanEx` on Windows, an Xkb keymap search on X11, a reverse
  `UCKeyTranslate` on macOS), while sequencing the result and owning the
  held set is OS-independent logic that belongs in the engine and must be
  testable without a desk. So the backend answers the question and the
  engine builds the sequence. The engine caches answers per active layout
  and drops the cache on `LayoutChanged`, so a character costs one round
  trip once and nothing afterwards.

### Upcalls

`Motion { x, y, dx, dy }`, `Button`, `Wheel`, `Key { u, key, sym, m, lk,
dn }`, `MonitorsChanged`, `LayoutChanged { layout, group }`,
`CapabilitiesChanged`, `Refused { code }`, `CaptureLost { why }`.

`Key` upcalls carry the same levels the wire carries, because the source's
job is to read them off the OS and put them in a frame. Reading the symbol
the local layout produced is part of capture, not a separate lookup.

**`CapabilitiesChanged` was added by #125, and the seam was wrong without
it.** The engine re-read the capabilities on exactly three occasions: at
start, on `MonitorsChanged`, and on `CaptureLost`. An OS grant given after
the component started is none of the three, so a Mac whose Accessibility
permission a person granted at the prompt kept saying "nothing here can
type" until something unrelated happened. Reusing `MonitorsChanged` would
have worked by accident and lied in the log; `CaptureLost` would have ended a
live session for a permission that had just been GIVEN.

It carries the `resolve` cache with it, which is the half that is not
obvious: negative answers are cached on purpose, so a backend asked what it
can produce before its grant or its keymap existed is remembered as able to
produce nothing for the life of the process. The engine therefore re-learns
on this event exactly as it does when a session starts, and a withdrawal in
the other direction ends whatever the machine was in the middle of (a
`stop` when it was driving, an `end NO_BACKEND` when it was being driven)
rather than leaving a session that can no longer work.

**`LayoutChanged` carries the active GROUP as well as the identity**, and it
has to. A stroke whose symbol resolves in another keyboard group switches to
it and switches back afterwards, and "back" means the group the machine was
in; without a number arriving from the OS the engine would only ever know 0,
so a user working in group 1 would be left in group 0 by the very code
written to prevent exactly that. It is free where it matters (the same X11
`XkbStateNotify` a backend watches to notice the change carries it) and 0 on
Windows and macOS, which have no equivalent.

Two things the caller owes that event, both proven necessary by review:
adopt the group, and **release everything held**. The second is not
housekeeping. The held set is keyed by the platform's own key identity, a
re-resolve after a layout change can name the same key differently, and a
held key that no longer matches is a key nothing will ever release: a game's
W stuck down, or a Control, for the rest of the session.

### Six platform truths this seam is shaped by, and #125 must not have to
### rediscover

1. **Under confinement, `dx` and `dy` must come from the OS's own relative
   source** (the low level hook's intended point against the real cursor on
   Windows, XI2 raw events on X11, the `CGEvent` delta fields on macOS),
   never from differencing successive absolute positions. The BACKEND is what
   keeps the pointer pinned while a session is live (a clip plus a swallow, a
   grab plus a warp, a decoupling), so the difference between two absolute
   positions is zero exactly when the hand is moving fastest.

   Corrected by #125: the first version of this said the ENGINE warps the
   pointer back on every event, and it does not. It confines once when the
   peer accepts and integrates the deltas into a virtual cursor in the
   target's space; nothing local moves for the whole session. The conclusion
   was right and the mechanism named was not, which matters because a backend
   author reading the old sentence would have waited for a warp that never
   comes.

   And it is not only "under confinement". The same is true while merely
   WATCHING an edge, for a reason that is easy to miss: the OS clamps its own
   pointer at the boundary of its own desktop, `at_edge` asks whether the
   pointer went strictly PAST the last pixel, and a pointer already sitting on
   that pixel generates no further absolute movement at all. Differenced
   deltas are zero there, which is precisely where a crossing has to fire, so
   a backend with no relative source can never hand a pointer over.
2. **A relative mouse move of (0, 0) is discarded by Windows** and reaches
   no hook at all (#123). A backend must not rely on seeing one; the engine
   never emits one.
3. **The marker in `dwExtraInfo` does not survive a relative mouse move**
   (it does survive a key event), so a backend cannot recognise its own
   injected motion that way. In v1 it does not have to: a machine being
   driven is not capturing (section 4, rule 3). Anything that changes that
   rule owes echo suppression a different mechanism, and this is the note
   that says so.
4. **On X11 the only OS-native relative source under a confining grab is
   `XI_RawMotion`, whose valuators are UNACCELERATED device deltas.** So the
   pointer will feel materially different while driving than while local, on
   the one platform where "mixing a 4K at 100% with a Retina at 200% does not
   change how the mouse feels" is actually checked. #125 has to choose, and
   name its choice: apply the device's own acceleration profile, or use
   `XI_Motion` deltas against the confining window and accept its clamp.

   **#125 chose the raw valuators and then the platform overruled it**, which
   is worth reading in that order because the first half was implemented and
   shipped in a review before the second half was measured. The raw valuators
   are what the backend reads while it only WATCHES. While it SWALLOWS it
   cannot read them at all: a grabbed device's events go to the grab, and a raw
   event has no core form to convert into, so the grabbing client gets nothing
   (truth 9 below, measured). So the delta while driving is the difference of
   the positions the grab reports, which is the ACCELERATED movement, the same
   thing Windows gets for free from its hook (truth 7). The clamp the choice
   was made to avoid is real and is bounded where it is paid: the difference is
   taken against the pin's anchor at the centre of the screen, so one event
   carries up to half a screen. Reading the device's acceleration profile out
   of its XI2 properties and applying it to the raw valuators is a third option
   and a ticket of its own.
5. **`CGWarpMouseCursorPosition` suppresses local mouse events for about
   250 ms** unless it is followed by
   `CGAssociateMouseAndMouseCursorPosition(true)` (or
   `CGSetLocalEventsSuppressionInterval` is zeroed). A warp on macOS is
   therefore two calls, not one. The engine warps on every return and on
   every teardown, so a quarter of a second of dead mouse would land at the
   worst possible moment there is.
6. **`VkKeyScanEx` returns -1 for a character that needs a dead key**, and
   finding the pair means walking virtual keys by modifier state through
   `ToUnicodeEx` and its own dead-key state machine. So a Windows `resolve`
   may legitimately answer `None` for a dead-key symbol, and that is covered:
   Windows always has `unicode`, so the fallback types the character.

### Four more #125 measured, which were not in this list because nobody knew

7. **On Windows the relative source is the low level mouse hook itself.** Its
   `pt` is the position the pointer is ABOUT to take and the cursor has not
   moved yet when the hook runs, so `pt` minus `GetCursorPos()` is the
   accelerated delta of that one event. While swallowing, the cursor does not
   move at all (the hook consumes the move before the system applies it), so
   the subtraction keeps working and the pin needs no warp per event. Raw
   input is therefore NOT used: it would arrive as a separate message with no
   defined ordering against the hook callback, and pairing the two streams
   would buy an unaccelerated delta, which is the wrong one. `ClipCursor` is
   kept as the belt to the hook's braces, for a hook Windows drops on its own
   timeout.
8. **A Windows session can be one nobody is attached to, and it looks almost
   exactly like a normal one.** Measured on a real host whose interactive
   session was a disconnected remote desktop: the process was on
   `WinSta0\Default`, the input desktop was also named `Default`, 382 windows
   and the shell window were enumerable, and `GetCursorPos`,
   `GetForegroundWindow` and `SendInput` were all three denied with
   `ERROR_ACCESS_DENIED`. A locked machine is the same shape. So the check
   that decides whether anybody can see what is typed is `GetCursorPos`
   succeeding, not the window station's name, and it happens BEFORE the
   injection. The first version compared the two desktop names only, said yes
   on that host, and would have typed a whole session into nothing while
   reporting success. This is the sharpest example there is of the difference
   between "best effort" and "best effort that lies".
9. **On X11 a grab does not stop ANOTHER client's raw events, and it stops the
   grabbing client's own completely.** Both halves are measured with a second
   client on the same server, and the second half is the sharpest thing #125
   found. A client that has selected `XI_RawKeyPress` on the root keeps
   receiving keys while another client holds a keyboard grab, which is the
   opposite of what the obvious reading of the XInput2 specification suggests:
   so the swallow has to be proved against a FOCUSED WINDOW, which does stop
   receiving keys, and that is what the live suite does. But the GRABBING
   client receives no raw events at all, because while a device is grabbed the
   server hands its events to the grab instead of to the ordinary selections
   and a raw event has no core form to convert into. A backend that grabs and
   then waits for raw events observes NOTHING for the whole session: measured
   as zero upcalls from six faked moves and twelve faked keys after a Watch to
   Swallow transition, which is exactly the sequence a real session takes. So
   the X11 backend gives its grabs an event mask and reads the CORE events
   while grabbed, keeping the raw stream for watching, where a core mask would
   be propagated away instead. That decides truth 4 above on this platform: the
   delta is unaccelerated while watching and accelerated while driving, because
   that is what a grab delivers, not because either was preferred. Finally, an
   X11 backend hears its own XTEST injections come back, because X11 offers no
   `dwExtraInfo` to mark them with; v1 does not need it to (rule 3 again), and
   when something does, a raw event's `source` device is the XTEST virtual
   device, which a real keyboard's never is.
10. **A wheel notch is a different number on each of the three platforms, and
    one of them is not a fixed number at all.** Windows delivers the notch in
    `WHEEL_DELTA` units, which is 120 by definition, and X11 delivers it as a
    button press and release, which is one notch by definition. macOS delivers
    BOTH a line count (a detent is exactly 1) and a point delta, which is
    acceleration-dependent and scaled by `CGEventSourceGetPixelsPerLine`, about
    10 by default. So the pixel-per-notch constant is per platform and the
    Windows one is not reusable: applying 120 to a macOS point delta made about
    twelve notches of a real wheel, or a whole trackpad swipe, travel as one,
    and made every small scroll travel as nothing. The macOS backend divides
    the point delta by 10, keeps the remainder, and falls back to the line
    count per axis when the point delta on that axis is zero.
11. **The X11 core keyboard mapping cannot be read unambiguously, and XKB
    can.** The core protocol says the first four keysyms of a keycode are
    "group 1 levels 1 and 2, group 2 levels 1 and 2", and an XKB server
    synthesises that list as `width * groups` entries in group-major order.
    One group of four levels and two groups of two levels are therefore the
    SAME four numbers with different meanings, and telling them apart decides
    the one example this document leads with (typing `@` on an AZERTY target
    is AltGr plus 0, which is group 1 level 3). `xkb::GetMap` answers with the
    per-key group count and width, so the X11 backend asks XKB and falls back
    to the ambiguous core reading only when the extension is missing.

### What "logical pixels" turned out to mean, per platform

Section 6 says the plane is in logical pixels and section 10's `Monitor`
carries logical sizes. Implementing the three backends showed that the seam
needs something slightly different and stricter, so the contract is now
stated as what it has to be: **`Monitor`'s rectangle and every `Point` are in
the machine's own POINTER COORDINATE SPACE, whatever that space is.** They
have to agree, because the engine converts between them, and no platform
offers a second space in which both are expressed.

- **X11** has one space and no per-monitor scale at all: RandR reports pixels
  and millimetres and nothing about what a desktop environment is scaling by,
  so `scale` is 1000 everywhere and an interface has one fewer label.
- **macOS** has exactly what the section imagined: the global display space is
  in points, a Retina display reports half its pixel width, and `scale` is the
  ratio of the two. The ratio has to come from the display MODE
  (`CGDisplayModeGetPixelWidth` over `CGDisplayModeGetWidth`): the obvious call,
  `CGDisplayPixelsWide`, does not return pixels despite its name and its
  documentation, it returns the same points as the bounds, so the ratio is
  exactly 1 on every display including every Retina one. In a fractional "More
  Space" mode the framebuffer can exceed the panel's own pixels and this reports
  2.0 where the physical ratio is nearer 1.7, which is not an error: it is the
  same number `NSScreen.backingScaleFactor` gives, and that is the number an
  interface showing "200%" means.
- **Windows** has neither unless a process chooses. A DPI-unaware process gets
  every monitor scaled by the PRIMARY monitor's factor, which is right for one
  screen and wrong for a second one at a different scale; a per-monitor-aware
  one gets physical pixels throughout. The backend declares per-monitor
  awareness v2 and reports physical pixels with `scale` saying each monitor's
  DPI, because one consistent space that is honestly labelled beats a
  "logical" space that does not exist.

The plane's arithmetic uses the rectangles and not the scale, so the cost of
the mismatch is a 4K screen drawn twice the size of a Retina one next to it in
the interface, and nothing about where the pointer goes.

### Capabilities, and refusals that are detected rather than guessed

`Capabilities { capture, swallow, confine, warp, inject_keys, inject_pointer,
unicode, monitors_stable, problem }`. Every false is a sentence the
interface can say before anyone tries, and `problem` is the honest hook for
a permission that is missing (the `session.status.problem` pattern): on
macOS the Accessibility grant, refused or not yet asked for, makes a backend
that says exactly what it cannot do rather than one that pretends.

Two of them mean more than their names suggest, and the prose is the contract
rather than an extra bit, because no real platform can satisfy half of either:

- **`confine` is a two-part promise**: pin the pointer, AND report OS-native
  relative deltas while it is pinned. A backend that could do the first and
  not the second would pass `can_drive()` and then produce a frozen pointer,
  which is the worst of both answers. On macOS `confine` also means "can
  decouple" rather than "can clip": there is no `ClipCursor` equivalent, the
  implementation is `CGAssociateMouseAndMouseCursorPosition(false)` plus a
  warp, and the rectangle is advisory.
- **`Action::Group` has no macOS meaning at all** (input sources are switched
  with `TISSelectInputSource`, which is user-visible and slow). Harmless as
  specified: the engine only ever emits a group `resolve` handed it, and a
  macOS `resolve` will never set one.

The refusal codes a backend reports upward, each with a sentence in section
13: `ELEVATED_WINDOW` (`SendInput` returned 0 and the foreground window's
integrity level says why), `SECURE_INPUT` (`IsSecureEventInputEnabled`),
`SCREEN_LOCKED`, `NO_PERMISSION`. Best effort is acceptable. Best effort
that lies is not, and that is the whole differentiator against the prior art
in this category.

## 11. Persistence

All under `data_dir()/input`, all JSON, all written with the store's
atomic discipline (temp, fsync, rename, directory fsync) except where noted:

- `identity.json`: the engine keypair. A corrupt one is deliberately NOT
  self-healing (a silently fresh key would unpin this device everywhere):
  report and stay down, visibly, until a human decides. The sync engine's
  rule, for the sync engine's reason.
- `plane.json`: the merged layout document, plus the pinned engine keys of
  the peers and the derived-placement cache.
- `settings.json`: the grants (who may drive this computer), the outbound
  enablements and their modes, the per neighbour guards, the incoming
  modifier remappings, the return hotkey, the lock toggle.
- `held.json`: the crash guard of section 8. Written without fsync, on
  purpose, and drained at the next start.

**A corrupt file's policy is PER FILE, and the difference is deliberate.** One
rule for all four was wrong in both directions, and the two it was wrong about
are the two most in need of being read:

- `identity.json`: **fatal**, as above. Identity.
- `settings.json`: **fatal**. Permissions. Starting fresh over an unreadable
  one would silently re-open a door somebody closed, or close one they opened,
  and neither is something to guess at.
- `plane.json`: **lenient**. An unreadable one starts an EMPTY plane with a
  warning, because a plane authorizes nothing and one round with any peer
  rebuilds it, so staying down over a file that will be replaced in seconds is
  strictness with no payoff.
- `held.json`: **lenient**, and this one is the sharpest of the four. It is the
  file written WITHOUT fsync, so it is precisely the one a power cut leaves
  zero-length or torn, and it is the guard against a machine left with Control
  held down. Treating it as fatal made a corrupt guard the reason the guard
  could not run, which is the opposite of what the file is for. Unreadable
  reads as "nothing was held".

The Core stores nothing; `input.status` is answered entirely from this state
plus the live session.

## 12. The `input.*` vocabulary (frozen)

Methods, all routed through the facade (verbatim relay; `input.status` under
`input.read`, the rest under `input.manage`). The facade's proxy budget is
10 s, so no method does long work inline: a gesture validates, persists, and
RETURNS, with everything else behind `input.updated`.

| Method | Description |
|---|---|
| `input.status {}` | the whole state: the plane, per device state and measured round trip, the live session, the guards, any problem. The `input` topic's snapshot, and the AUTHORITATIVE state the notifications merely echo |
| `input.place { spots }` | the arrangement a human dragged. Writes a placement at `max(seen) + 1`, signs it, replicates it |
| `input.allow { device_id, allowed }` | who may drive THIS computer. The authority, stored here, never replicated |
| `input.drive { device_id, allowed, mode? }` | who this computer may drive, and how (`"typing"` or `"positional"`). A local convenience; the far side still decides |
| `input.take { device_id, mode? }` | take the keyboard and mouse there now, without crossing an edge. `mode` is `"full"` or `"keys"` |
| `input.release {}` | bring them back. The return hotkey's method twin |
| `input.guards { device_id, monitor, side, guards }` | the per neighbour crossing guards: `dwell_ms`, `double_tap_ms`, `dead_corner`, `require_mods`, `wall` |
| `input.lock { locked }` | pin the pointer to this screen |
| `input.hotkey { keys }` | the return hotkey, enforced locally by the machine that captures |
| `input.remap { device_id, map }` | incoming modifier remapping on this machine. Canonical name to canonical name, holdable modifiers only, and refused unless it is injective: a map that sends two modifiers to one makes a modifier vanish silently |

Errors (the engine's own, relayed verbatim).

**A gesture answers only with what THIS machine already knows.** Everything
the far side decides arrives afterwards, as that device's `problem` in the
snapshot and as an `input.refused` notification, and the reason is the grant
doctrine: the driving side learns by TRYING, so a gesture that answered
`INPUT_NOT_ALLOWED` would have had to cache the far side's grant, and a
cached grant is wrong exactly when it matters. Nothing is lost, because the
answer arrives within a round trip and the interface renders it from the
snapshot either way. The column below says which of the two each code is.

| Code | Where | When |
|---|---|---|
| `INPUT_DEVICE_UNKNOWN` | gesture | not a device of this account, or a mobile (never a source and never a target in v1) |
| `INPUT_UNKNOWN_MONITOR` | gesture | a guard names a crossing no segment of the plane has |
| `INPUT_BUSY` | gesture | THIS computer is being driven, or is already driving another one. There is one keyboard, and no preemption |
| `INPUT_LOCKED` | gesture | THIS computer's pointer is pinned to its own screen (`input.lock`). Distinct from `INPUT_BUSY` on purpose: nobody is holding it, and the remedy is a switch rather than waiting |
| `INPUT_NO_BACKEND` | gesture | THIS computer's own backend cannot capture, so it has no keyboard to send |
| `INPUT_TOO_SLOW` | gesture | the round trip THIS computer measured is above the pointer threshold. `input.take` with `mode: "keys"` is the offer |
| `INPUT_NOT_READY` | gesture | the engine has not resolved the account's directory yet |
| `INPUT_INTERNAL` | gesture | a local failure the caller can only retry |
| `-32602` | gesture | the request's shape. Checked BEFORE anything is evaluated: a request missing a required field has no semantics, so an application code would be a category error |
| `not_allowed` | `problem` | the far side has no grant for this device. Its word, learned by trying |
| `busy` | `problem` | that computer is being driven by another already, or is driving one |
| `locked` | `problem` | that computer's pointer is pinned to its own screen, or it is locked and cannot be driven at all |
| `no_backend` | `problem` | nothing on that computer can type, or its permission is refused |
| `no_path` | `problem` | the deployment's relays are rendezvous-only above a cap (#88) and no direct path formed |
| `too_slow` | `problem` | the path to that computer is past the pointer threshold |
| `plane_stale` | `problem` | the two ends do not hold the same plane. Self-repairing: a layout round is already running |

A malformed request is not an application state, and the engine emits the
real JSON-RPC code rather than dressing one as an app code. It also checks
the whole shape BEFORE evaluating any of it, so a call that is both
malformed and about an unknown device is answered `-32602`: there is nothing
to evaluate.

**Two fields are called `mode` and they are two different axes**, which is
worth saying plainly because it is the easiest thing in this vocabulary to
confuse. `input.take`'s `mode` is what a SESSION carries (`"full"` or
`"keys"`: pointer and keyboard, or keyboard alone), and it is the same field
the `start` frame carries. `input.drive`'s `mode` is how a target RESOLVES a
key (`"typing"` or `"positional"`), stored per peer. The two value sets are
disjoint on purpose, so passing one where the other belongs is refused rather
than silently taken for a default.

Notifications (topic `input`, published via `input.emit`):

| Notification | When |
|---|---|
| `input.updated { state }` | any state change. Coalesced to at most 10 per second: a crossing changes the state, a pointer position does not |
| `input.refused { device_id, code, count }` | a refusal to say. Transient, not state: at most one per code per second per device, with a count |

Shapes:

```json
State = { "here": { "device_id", "name",       // this computer, so the plane
                                              // can say "you are here"
                    "monitors": [Monitor], "problem": null | "no_backend"
                    | "no_permission" | "monitors_unstable" | "wayland",
                    "can_drive": <bool>,      // this machine could take the
                    "can_be_driven": <bool> },// keyboard away, or accept it
          "plane": { "id": "<32 hex>", "spots": [Spot], "by": "<device_id>" },
          "devices": [ { "device_id", "name",
                         "state": "off" | "warming" | "ready" | "driving"
                                | "driven" | "refused",
                         "monitors": [Monitor],
                         "rtt_ms": <n or null>,
                         "lan": <bool>,
                         "allowed": <bool>,   // may drive this computer
                         "drive": <bool>,     // this computer may drive it
                         "mode": "typing" | "positional",
                         "problem": null | "not_allowed" | "busy" | "locked"
                                  | "no_backend" | "no_path" | "too_slow"
                                  | "plane_stale" } ],
          "session": null | { "device_id", "direction": "out" | "in",
                              "mode": "full" | "keys", "since": <ts>,
                              "rtt_ms": <n or null> },
          "guards": [ { "device_id", "monitor", "side",
                        "dwell_ms", "double_tap_ms", "dead_corner",
                        "require_mods", "wall" } ],   // only what a human set
          "lock": <bool>,
          "hotkey": ["ctrl", "alt", "Escape"] }

Monitor = { "id", "name", "w", "h", "x", "y", "scale", "primary",
            "present": <bool> }   // false = a ghost, its place kept

Spot = { "monitor": "<node_id>/<monitor id>", "device_id", "name",
         "x", "y", "w", "h",          // the rectangle ON THE PLANE
         "present": <bool>,           // false = a ghost, its place kept
         "primary": <bool> }
```

`plane.spots` carries the whole rectangle rather than only its corner, and
that is deliberate: it is the one list an interface needs in order to draw
the plane, and making it join `spots` against `devices[].monitors` to find a
width would be an invitation to draw a plane that disagrees with the one the
engine crosses on. `devices[].monitors` remains each machine's own word about
its own screens (its own desktop coordinates included, which is what a human
sees when the interface offers to import an arrangement).

`problem`, both on `here` and per device, is the honest sentence hook: a
pair that cannot do its job says why, from the snapshot alone, so a window
opening late renders everything and acts from that snapshot without
replaying any notification. Device NAMES are the interface's job
(`devices.read` both sides); the dialect speaks node_id and the facade
speaks `device_id`.

## 13. The refusals, and the sentence for each

Best effort, never silent. Every refusal below is DETECTED, not guessed, and
every one has a sentence an interface can say. The interface owns the
wording; these are the sentences the epic and #127 asked for, and the codes
that carry them.

| Code | Where | The sentence |
|---|---|---|
| `NOT_ALLOWED` | `no` frame, `INPUT_NOT_ALLOWED` | "That computer has not been told to accept your keyboard. Allow it there." |
| `BUSY` | `no` frame, `INPUT_BUSY` | "Another of your computers is using that keyboard right now." |
| `PLANE_STALE` | `no` frame | "The two computers do not agree on where the screens are yet. Trying again." |
| `NO_BACKEND` | `no` / `end`, `INPUT_NO_BACKEND` | "That computer cannot be driven: nothing there can type." On macOS with the grant refused: "1Device needs Accessibility permission on that computer to type on it." |
| `LOCKED` | `no` / `end`, `INPUT_LOCKED` | "This computer cannot be driven while it is locked." Or, when the pointer was pinned there on purpose: "That computer's pointer is locked to its own screen." |
| `IDLE` | `end` | "Your keyboard went quiet, so that computer released it." |
| `TAKEN` | `end` | "Someone is using that computer directly." |
| `ELEVATED_WINDOW` | `oops` | "Nothing was typed: this window runs as administrator." |
| `SECURE_INPUT` | `oops` | "Nothing was typed: password fields block synthetic keystrokes on macOS." |
| `SCREEN_LOCKED` | `oops` | "Nothing was typed: that computer is locked." |
| `NO_PERMISSION` | `oops` | "Nothing was typed: 1Device is not allowed to type on that computer." |
| `UNRESOLVED` | `oops` | "That key does not exist on the other computer's keyboard." |
| `NO_DIRECT_PATH` | `INPUT_NO_PATH` | "This account's relays do not carry a keyboard session. The two computers need a network they share." |
| `INPUT_TOO_SLOW` | `input.take` | "That computer is <n> ms away, too far for the pointer to feel right. Its keyboard alone would work." |
| `SLOW` | `stop` | "The connection to that computer slowed down, so your keyboard came back." |
| `UNKNOWN` | any of the four | "That computer refused, and this version does not know the reason it gave." What every code outside a frame's closed set becomes on arrival, so a later version's vocabulary degrades to a sentence rather than reaching an interface as prose a peer chose |

Plus the two states that are not refusals and still need words: a session
over a slow path announces its number ("Your keyboard is on **Desk**, 32 ms
away."), and an absent screen keeps its place and says so ("This screen is
not connected right now. Its place is kept.").

## 14. The numbers this design rests on (#123)

Measured in #123, not re-measured here. All p50 in milliseconds unless the
unit says otherwise.

| What | Number | What it decides here |
|---|---|---|
| one long-lived stream, 125 Hz, one way | 0.417 bare metal, 0.508 WSL2, same machine | the live channel is the right transport |
| jitter p99, direct wide area / relayed | 1.66 / 2.20 | a relayed session is usable |
| round trip: same machine / direct / relayed | 0.641 / **4.05** / **32.4** | the pointer thresholds below |
| loss at 125 and 250 Hz, every path | zero | the coalescing ceiling (section 5) |
| 1000 Hz over a relay | one freeze above 20 ms, 19 stale frames behind it | why the ceiling halves on a slow path, and why the target coalesces on receipt |
| a fresh QUIC stream | 30 to 50 microseconds | `peers.send`'s cost is its ACK, not stream churn |
| `peers.send` per message | 4.19 direct, 32.5 relayed | it carries the layout, never the flow |
| frame size, 24 against 256 bytes | identical on every path | JSON on the wire is free |
| Core to component notification at 125 Hz | 0.225 one way, p99 0.552, 1250 of 1250 | the local plane is not the bottleneck |
| a request the Core routes to another component | 0.383 | the facade's shape is affordable |
| `SendInput`, one move | 91 microseconds bare, 113 with hooks installed | injection is not free |
| `SendInput`, a batch of 8 moves | 857 microseconds | batching does NOT amortise: coalesce instead |
| a low level hook callback | 50 nanoseconds | a hook that stamps and queues costs nothing |
| XTEST motion, fire and forget | 0.80 microseconds | X11 injection is essentially free |
| XTEST to an XI2 raw event | 81 microseconds, 0 missed of 300 | X11 capture is reliable |
| a `tokio::time::sleep` waking late | **1.158 ms WSL2, 1.829 bare metal** | never pace the flow on a timer |
| the first open through a relay | 134 to 151 ms | the warm channel doctrine |

**The pointer thresholds** are a judgement about people, not a measurement,
and the measurement's contribution is that a relayed path is genuinely
usable and that its jitter is small next to its mean, so the mean is what a
hand will feel:

- under **10 ms** round trip: hand the pointer over silently;
- **10 to 60 ms**: hand it over, and say the number;
- above **60 ms**: decline the pointer (`INPUT_TOO_SLOW`) and offer the
  keyboard only session.

**Where the number comes from, and it is not where the epic expected.**
`iroh`'s `path.rtt()` gives the selected path's round trip for free, and the
Core does not expose it: there is no `rtt` anywhere in the Core, the daemon
or the IPC client, and `devices.list` carries `online`, `lan`, `relay_url`
and `reachable` but nothing about the path or its latency. So the engine
measures it itself, with `ping` and `pong` on the warm channel (section 3),
and that is arguably the better number anyway: it is the round trip a
keystroke really travels, both local planes included, where `path.rtt()` is
the wire alone. `lan` remains useful as the one thing the Core does say
about the route, and the interface can pair it with the measured figure.
Should the Core ever expose the path, it would be worth using to seed a
first estimate before a channel exists, and nothing else.

## 15. v1 limits (all deliberate, all additive to lift)

Computers only (a phone is neither a source nor a target: `INJECT_EVENTS` is
signature level on Android, and being a source is a different feature). One
source and one target per machine, and never both at once (section 4, rule
3), so no chaining. X11 only on Linux, with Wayland saying what it cannot do
rather than staying silent (#128). No preemption of a live session. No local
input detection on the target (a machine being driven does not notice its own
user reaching for the keyboard; `end TAKEN` exists on the wire for the day it
does). Absolute positions by default and relative as a per session mode, but
not switchable mid session. No drag and drop across an edge. No editing the
crossing graph directly, for arrangements a plane cannot express. Character
keys are not in the crash guard, only modifiers. No kernel driver, per the
epic's decision, which is a strategy and not an omission.

One field was not bounded, and #125 found it by writing arithmetic against it. A
wheel frame's `dx` and `dy` were carried as whatever `i32` a peer put there, and
a notch count times the Windows wheel unit overflows, as does the X11 backend's
own pixel accumulator: both PANIC in a debug build, so a peer chose whether the
component it was driving stayed alive. They are now clamped to `WHEEL_MAX`
(4096) on arrival and on the way out, which is more than any device produces in
one event and is the honest reading of a number no device could mean.

And the per-platform gaps #125 found, each of which degrades to a refusal with
a sentence rather than to something wrong:

- **X11**: a level above the fourth needs `ISO_Level5_Shift`, which the eight
  canonical modifier bits cannot name, so a symbol that only lives there is
  `UNRESOLVED`. The Unicode path is a keycode nothing is bound to, remapped
  for one stroke and restored, so a keyboard whose every keycode is bound has
  no Unicode path and says so through `unicode: false`. A key a keymap does
  not bind at all (an `xkeyboard-config` `pc105` leaves F13 to F24 unbound)
  resolves to nothing rather than to a keystroke that produces nothing.
- **Windows**: `resolve` answers `None` for a symbol needing a dead key
  (truth 6), which the Unicode path covers.
  A keycode the Unicode path leaves bound survives the client that bound it:
  every ordinary teardown unbinds it, including an unwinding panic, but a run
  killed outright during one `Text` injection leaves one keysym on one keycode
  no physical key produces, and the next start cannot find it (a spare is
  chosen by being entirely unbound). Clearing that needs the state directory
  to reach the backend, which this seam does not carry.
- **Windows**: `resolve` answers `None` for a symbol needing a dead key
  (truth 6), which the Unicode path covers. And `inject_keys` is
  unconditionally true, because the lock is reported per injection as
  `SCREEN_LOCKED` rather than as a capability; the one consequence is that the
  crash guard of section 8 is drained optimistically on a machine whose input
  desktop is not ours, where the release cannot land (the other platforms
  report the inability in `inject_keys` and the guard is kept).
- **macOS**: the media keys are not virtual keycodes at all, they travel as
  `NSSystemDefined` events, so this backend does not inject them and the
  engine reports `UNRESOLVED`. Nor are F21 to F24 (macOS stops at F20). There
  are TWO grants and not one (Input Monitoring for the tap, Accessibility for
  the injection), so a Mac can be a target and not a source or the other way
  round, and both halves are reported separately. A double click is a single
  click twice: the chain a Mac builds from its own double-click interval and a
  distance threshold is not synthesised here, because the interval lives in
  AppKit, which this component does not link. A display's `name` is its
  position in the active list ("Display 1" is the main one) and not the name a
  person gave it, which is `NSScreen.localizedName` and therefore AppKit too.
  Whether an injected Caps Lock toggles a Mac's own lock is unknown and on the
  live list: the lock is handled below the event system there, so posting the
  keycode may do nothing, and the capture side (which reports a Mac's own Caps
  Lock on its transition) is the half that is certainly right.
  And macOS has no `dwExtraInfo`: nothing marks an injected event as this
  component's own, so a tap cannot tell its own injection from a hand. Rule 3
  is what makes that safe in v1 (a machine being driven does not capture), and
  the day something needs it, `kCGEventSourceUserData` is the field.

## 16. Settled decisions

The choices that were explicitly weighed and closed, recorded with their
reasons so they are not relitigated by accident.

D1. **The channel is warmed ahead of the crossing and kept alive by a
    `ping` that doubles as the latency probe.** A cold open through a relay
    is 134 to 151 ms (#123) and a handover cannot wait for it; the Core
    sweeps a silent channel at 10 s so something must be sent anyway; and
    making that something a round trip gives the pointer thresholds a
    measured number for one extra frame.

D2. **The measured round trip comes from the dialect, not from the Core.**
    `path.rtt()` is not exposed by the public API (verified: no `rtt`
    anywhere in the Core, the daemon or the IPC client), and the dialect's
    own probe measures the whole path a keystroke travels rather than the
    wire alone. Reported in `input.status` per device, so `INPUT_TOO_SLOW`
    needs no error payload.

D3. **JSON on the wire, one object per frame.** Free at this size by
    measurement (#123), the house register, debuggable, extensible. A
    packed binary encoding would save about 30 bytes per pointer frame and
    buy nothing measurable.

D4. **Two self-imposed caps below the Core's**, `MAX_OUT_FRAME = 512` and
    `OUT_RATE_MAX = 1000` frames per second. A component that can reach
    `FRAME_TOO_LARGE` or `RATE_EXCEEDED` can cut its own channel, and every
    variable-length field is bounded at the source so the caps are
    unreachable rather than merely respected.

D5. **Coalescing by superseding, with a token bucket read on arrival, and
    exactly one trailing-edge flush timer.** The flow is never paced by a
    tick (#123: a sleep wakes 1.158 ms late at p50, against 0.35 ms of
    network jitter). The one timer delivers the final pending position after
    the flow stops, when nothing is moving and its jitter is invisible.

D6. **The target coalesces on receipt as well**, by dropping a pointer frame
    whose immediate successor in the same batch is also a pointer frame.
    Injection is about 100 microseconds per event on Windows and does not
    amortise, so replaying a stale burst costs real time and shows as a
    rubber band. The `n` counter is what makes the drop provably safe;
    on an ordered pipe it never fires by itself, and saying it does would be
    wrong.

D7. **Absolute positions are computed by the source in the target's own
    logical desktop coordinates**, and `start` carries the plane id so the
    two ends cannot disagree about what those coordinates mean. A mismatch
    is `PLANE_STALE`, which repairs itself through a layout round.

D8. **The projection at a crossing is the identity on the shared stretch of
    edge.** On one plane, that IS "proportional over the portion two
    monitors actually share"; proportional stretching would move the pointer
    vertically at a crossing, which is what a plane exists to prevent. The
    epic's point is that the graph is derived rather than typed in, and it
    is.

D9. **Symbol-first resolution in `typing` mode, usage-first in
    `positional`.** The epic's stated order and the epic's own `@` example
    cannot both hold, and the mode is what the epic itself says chooses.
    Recorded as a correction, with the reason, in section 8.

D10. **The canonical modifier bitfield is platform-free**, `meta` being one
    bit for the Windows key, Command and Super. Remapping is then a table
    with two canonical sides, stored on the machine being driven, per source
    device, defaulting to the identity because swapping Control and Command
    for a Mac target is an opinion.

D11. **A `placement` from an unverifiable signer is adopted, marked
    unverified; one that is signed WRONG is refused; a `monitors` entry is
    never adopted unverified at all.** The plane authorizes nothing (the
    grants do), so fail-closed pinning here would break sessions between two
    innocent devices over a third's absence, while the worst a forged
    placement can do is misplace a screen. The loop is closed by re-examining
    the arrangement on EVERY pin, so a forgery adopted on faith goes the moment
    its author's key arrives, and so does a genuine one whose author has
    replaced its key. A deliberate divergence from the sync engine, with its
    reason. Two conditions from the draft were removed during implementation,
    both recorded in section 6: "relayed by a pinned peer" as vacuous (a peer's
    key rides its own message, so speaking is what makes a device pinned), and
    "an already verified arrangement survives a key replacement" as
    divergent (it made the plane depend on the order two keys arrived in, which
    is the one thing this document cannot afford).

D12. **The layout replicates as one whole document, not as deltas.** A real
    desk is a few hundred bytes (3 devices of 2 monitors), the caps are 16
    devices of 16 monitors and 256 spots, and a merge is idempotent, so a
    head/need/give round would be complexity with no payoff. The caps alone do
    not fit `peers.send`'s 64 KiB, so what is OFFERED also has a byte budget:
    a device's own entry always travels, the rest is relayed while it fits.

D13. **An explicit length-prefixed byte encoding for the signed objects**,
    not canonical JSON. Two small objects entirely under our control, a
    domain prefix per kind so they cannot cross-verify, and no
    integer-profile trap. It also avoids copying the sync engine's canonical
    encoder into a second component.

D14. **Grants are never replicated and the driving side learns by trying.**
    The handshake carries no hint of the far side's grant, because a grant
    can be withdrawn between a hint and its use and because the interface
    must not imply the far side's list is knowable from here.

D15. **Driving and being driven are mutually exclusive on one machine.** It
    makes echo suppression a non-problem for the engine in v1, which matters
    because the Windows `dwExtraInfo` marker does not survive a relative
    mouse move (#123). Chaining is the feature this forecloses, and it is
    not in v1.

D16. **No preemption: `BUSY` is `BUSY`.** No automatic rule can tell "take
    the keyboard back from a machine nobody is using" apart from "interrupt
    a human typing", so a human decides and the interface says who holds it.

D17. **The held set holds what WE pressed, and the crash guard holds
    modifiers only, without fsync.** Releasing a key the machine's own user
    is holding is not ours to do; a character key stranded by a crash costs
    one repeated character where a modifier costs a dead keyboard; and the
    failure guarded against is a process death, which the page cache
    survives.

D18. **The backend answers the layout question and the engine builds the
    sequence.** "What produces this symbol here" is platform knowledge;
    sequencing, restoration and hygiene are OS-independent logic that has to
    be testable without a desk. The engine caches the answers per active
    layout and drops the cache on `LayoutChanged`.

D19. **Injection is a fire-and-forget downcall and refusals come back as
    coalesced upcalls.** A result per event would cost a round trip to the
    OS thread on a path where the OS call itself is 100 microseconds, and a
    refusal per keystroke would be a flood; one code with a count per second
    is what the interface can actually say.

D20. **The return hotkey defaults to Ctrl + Alt + Escape** because Ctrl +
    Alt + Delete is unreachable from a Windows hook, Command + Option +
    Escape is Force Quit on macOS, and a bare modifier double tap fires by
    accident. It is enforced locally by the machine that captures, and so is
    the 2 s stall watchdog that brings the keyboard home when a target stops
    answering: neither is negotiated, because a hung target must not be able
    to keep your keyboard.

D21. **A gesture answers only with what THIS machine knows; everything the
    far side decides arrives as that device's `problem`.** The first draft
    let `input.take` answer from the far side's last word while its backoff
    stood, which reads well and is a cached grant in all but name. It also
    creates a dead window exactly where the product's main flow is: tick the
    box on the machine to be driven, walk to the other one, press the button,
    and be told "not allowed" for the length of a backoff from a word that is
    now false. So the driving side learns by TRYING, every time, and the
    answer it gets back within a round trip is what the interface says.

D22. **The component never exits because the OS half is missing, and
    `os::create` cannot fail.** The supervisor restarts what dies, with a
    backoff capped at a minute and no notion of "unsupported", so an exit
    here is a relaunch every minute for ever. And a machine with no OS half
    still owns its screens on the plane, its grants, and the sentence saying
    what it cannot do, none of which a dead process can serve. What varies
    per platform is what the backend SAYS it can do, which is the same path a
    real backend takes when its OS grant has been refused: one path, not two.
