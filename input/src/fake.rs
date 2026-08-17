// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The fake platform backend: what every test of this engine runs against, and
//! the reference implementation of the seam for whoever writes a real one.
//!
//! It lives in the lib rather than in `tests/`, for two reasons. The integration
//! suite and the unit tests both need it, and a `#[cfg(test)]` double is
//! invisible to the former. And it is the seam's worked example: a real backend
//! author reads this file to see exactly which downcall is expected to do what,
//! which is a better contract than prose alone. It costs no dependency, touches
//! no OS API, and compiles to a few hundred bytes of nothing on a machine that
//! never constructs one.
//!
//! It records every downcall so a test can assert on what the engine DID to the
//! OS (a released modifier, a lifted confinement, a warp home), which is how the
//! hygiene rules are proven: the engine's own view of its held set is not
//! evidence, the calls it made are.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use crate::backend::{
    Action, BackendEvent, Capabilities, CaptureMode, InputBackend, Monitor, PlatformKey, Point,
    Problem, Rect, Resolved, Want,
};

/// Everything the fake was told, in order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Calls {
    /// Every [`InputBackend::capture`] in order, so a test can prove that a
    /// teardown reached [`CaptureMode::Off`] and did not merely intend to.
    pub capture: Vec<CaptureMode>,
    /// Every [`InputBackend::confine`] in order. A `None` is the release, and a
    /// session that ends without one has left a machine's mouse pinned.
    pub confine: Vec<Option<Rect>>,
    pub warps: Vec<Point>,
    /// Every action of every batch, flattened: the engine's contract is that a
    /// batch is applied in order, so the flattening loses nothing a test cares
    /// about and makes an assertion readable.
    pub actions: Vec<Action>,
    /// Every [`InputBackend::release_all`], each with the keys it named.
    pub releases: Vec<Vec<PlatformKey>>,
    pub exit: Option<i32>,
}

struct State {
    caps: Capabilities,
    monitors: Vec<Monitor>,
    pointer: Option<Point>,
    /// What [`InputBackend::resolve`] answers, keyed by the request. Anything
    /// absent resolves to `None`, which is how a test stages the
    /// `UNRESOLVED` path.
    table: BTreeMap<String, Resolved>,
    calls: Calls,
}

/// A platform backend that types nowhere and remembers everything.
#[derive(Clone)]
pub struct FakeBackend {
    state: Arc<Mutex<State>>,
    events: mpsc::Sender<BackendEvent>,
}

impl FakeBackend {
    /// A fake that can do everything, with one monitor at the origin, plus the
    /// receiver the engine consumes.
    ///
    /// The channel is bounded and generous: the engine drains it as its own
    /// event loop turns, and a test that fills it is a test whose engine is
    /// wedged, which is worth failing on rather than hiding behind an unbounded
    /// queue.
    pub fn new() -> (FakeBackend, mpsc::Receiver<BackendEvent>) {
        let (events, rx) = mpsc::channel(256);
        let backend = FakeBackend {
            state: Arc::new(Mutex::new(State {
                caps: Capabilities {
                    capture: true,
                    swallow: true,
                    confine: true,
                    warp: true,
                    inject_keys: true,
                    inject_pointer: true,
                    unicode: true,
                    monitors_stable: true,
                    problem: None,
                },
                monitors: vec![Monitor {
                    id: "FAKE-1".into(),
                    name: "Fake screen".into(),
                    w: 1920,
                    h: 1080,
                    x: 0,
                    y: 0,
                    scale: 1000,
                    primary: true,
                }],
                pointer: Some(Point { x: 960, y: 540 }),
                table: BTreeMap::new(),
                calls: Calls::default(),
            })),
            events,
        };
        (backend, rx)
    }

    /// Narrows what the fake claims it can do: the keyboard-only target, the
    /// machine with no permission, the Wayland session.
    pub fn set_capabilities(&self, caps: Capabilities) {
        self.lock().caps = caps;
    }

    /// A fake that cannot inject the pointer, which is what a keyboard-only
    /// target looks like from the engine's side.
    pub fn keyboard_only(&self) {
        let mut state = self.lock();
        state.caps.inject_pointer = false;
        state.caps.confine = false;
    }

    /// A fake whose OS grant is missing.
    pub fn refused(&self) {
        let mut state = self.lock();
        state.caps = Capabilities::none(Some(Problem::NoPermission));
    }

    pub fn set_monitors(&self, monitors: Vec<Monitor>) {
        self.lock().monitors = monitors;
    }

    pub fn set_pointer(&self, at: Option<Point>) {
        self.lock().pointer = at;
    }

    /// Teaches the fake that `want` is producible as `how`. Anything untaught
    /// resolves to `None`.
    pub fn teach(&self, want: &Want, how: Resolved) {
        self.lock().table.insert(key_of(want), how);
    }

    /// Teaches the ordinary case: this symbol comes from this key with these
    /// modifiers.
    pub fn teach_symbol(&self, sym: &str, code: u32, mods: u16) {
        self.teach(
            &Want::Symbol(sym.to_string()),
            Resolved {
                code: PlatformKey { code, detail: 0 },
                mods,
                prefix: None,
                group: None,
            },
        );
    }

    /// Teaches a HID usage, which is what `positional` mode asks for.
    pub fn teach_usage(&self, usage: u32, code: u32) {
        self.teach(
            &Want::Usage(usage),
            Resolved {
                code: PlatformKey { code, detail: 0 },
                mods: 0,
                prefix: None,
                group: None,
            },
        );
    }

    /// Teaches a named key (`Enter`, `F5`).
    pub fn teach_named(&self, name: &str, code: u32) {
        self.teach(
            &Want::Named(name.to_string()),
            Resolved {
                code: PlatformKey { code, detail: 0 },
                mods: 0,
                prefix: None,
                group: None,
            },
        );
    }

    /// Pushes an upcall, as a real backend would when the OS says something.
    /// Awaits the engine's queue, so a test cannot silently outrun it.
    pub async fn emit(&self, event: BackendEvent) {
        let _ = self.events.send(event).await;
    }

    /// The active layout changed, and the machine is now in `group`.
    ///
    /// Spelled out as its own helper because the GROUP is the half of this event a
    /// caller forgets, and forgetting it is not cosmetic: it is the only way the
    /// engine can learn which group to switch BACK to, so an engine that ignores it
    /// leaves the machine in whatever group the last cross-group stroke needed. A
    /// real backend reads both from the same `XkbStateNotify` on X11 and reports
    /// group 0 on Windows and macOS, where groups do not exist in this sense.
    pub async fn layout_changed(&self, layout: &str, group: u32) {
        self.emit(BackendEvent::LayoutChanged {
            layout: layout.to_string(),
            group,
        })
        .await;
    }

    /// The OS grant arrived (or went away): narrow or widen what the fake claims
    /// and tell the engine to ask again, which is the pair a real backend does in
    /// this order.
    pub async fn grant_changed(&self, caps: Capabilities) {
        self.set_capabilities(caps);
        self.emit(BackendEvent::CapabilitiesChanged).await;
    }

    /// Everything the engine has done to this backend so far.
    pub fn calls(&self) -> Calls {
        self.lock().calls.clone()
    }

    /// Forgets the record, so a test can assert about one phase without
    /// counting the setup's calls.
    pub fn forget(&self) {
        self.lock().calls = Calls::default();
    }

    /// The keys the engine has pressed and not released, derived from the
    /// recorded calls alone: the engine's own bookkeeping is exactly what a
    /// hygiene test must NOT trust.
    pub fn keys_down(&self) -> Vec<PlatformKey> {
        let calls = self.calls();
        let mut down: Vec<PlatformKey> = Vec::new();
        for action in &calls.actions {
            if let Action::Key {
                code,
                down: is_down,
            } = action
            {
                if *is_down {
                    if !down.contains(code) {
                        down.push(*code);
                    }
                } else {
                    down.retain(|held| held != code);
                }
            }
        }
        for released in &calls.releases {
            for code in released {
                down.retain(|held| held != code);
            }
        }
        down
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("lock the fake backend")
    }
}

/// The table's key. A string rather than the `Want` itself so the map needs no
/// ordering on a type that will grow variants; the shape is an implementation
/// detail of the fake and nothing outside depends on it.
fn key_of(want: &Want) -> String {
    match want {
        Want::Usage(u) => format!("usage:{u}"),
        Want::Named(n) => format!("named:{n}"),
        Want::Symbol(s) => format!("symbol:{s}"),
    }
}

impl InputBackend for FakeBackend {
    fn capabilities(&self) -> Capabilities {
        self.lock().caps.clone()
    }

    async fn monitors(&self) -> Vec<Monitor> {
        self.lock().monitors.clone()
    }

    async fn pointer(&self) -> Option<Point> {
        self.lock().pointer
    }

    async fn resolve(&self, want: Want) -> Option<Resolved> {
        self.lock().table.get(&key_of(&want)).cloned()
    }

    fn capture(&self, mode: CaptureMode) {
        self.lock().calls.capture.push(mode);
    }

    fn confine(&self, rect: Option<Rect>) {
        self.lock().calls.confine.push(rect);
    }

    fn warp(&self, to: Point) {
        let mut state = self.lock();
        state.calls.warps.push(to);
        state.pointer = Some(to);
    }

    fn inject(&self, actions: Vec<Action>) {
        self.lock().calls.actions.extend(actions);
    }

    fn release_all(&self, keys: Vec<PlatformKey>) {
        self.lock().calls.releases.push(keys);
    }

    fn request_exit(&self, code: i32) {
        self.lock().calls.exit = Some(code);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fake's own contract, because every hygiene assertion in the suite
    /// rests on it: `keys_down` is derived from the recorded calls, so a key
    /// pressed and never released shows, and `release_all` clears it.
    #[tokio::test]
    async fn the_fake_reports_what_is_still_held_from_the_calls_alone() {
        let (fake, _events) = FakeBackend::new();
        let shift = PlatformKey {
            code: 16,
            detail: 0,
        };
        let a = PlatformKey {
            code: 65,
            detail: 0,
        };
        fake.inject(vec![
            Action::Key {
                code: shift,
                down: true,
            },
            Action::Key {
                code: a,
                down: true,
            },
            Action::Key {
                code: a,
                down: false,
            },
        ]);
        assert_eq!(fake.keys_down(), vec![shift], "the modifier is still down");
        fake.release_all(vec![shift]);
        assert!(fake.keys_down().is_empty(), "the release clears it");
    }

    /// An untaught symbol resolves to nothing, which is how the suite stages
    /// the key that does not exist on the other machine's keyboard.
    #[tokio::test]
    async fn an_untaught_symbol_cannot_be_produced() {
        let (fake, _events) = FakeBackend::new();
        fake.teach_symbol("a", 65, 0);
        assert!(fake.resolve(Want::Symbol("a".into())).await.is_some());
        assert!(fake.resolve(Want::Symbol("@".into())).await.is_none());
        assert!(fake.resolve(Want::Usage(0x0007_0028)).await.is_none());
    }
}
