// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Rendering the target list onto the OS surfaces, off the async reactor.
//!
//! Two properties the orchestrator relies on:
//! - **serialized**: never two rewrites of the same registry key or folder at
//!   once, whatever the rate of device events;
//! - **latest wins**: a burst does not queue up intermediate lists. The input is
//!   a `watch` channel, so an apply that takes a while is followed by the state
//!   at the time it finishes, not by every state it missed.
//!
//! When the sender goes away — the manager is stopping — the applier renders an
//! EMPTY list before exiting: no manager, no entry (doc/architecture.md's
//! fail-closed rule). That is also the only cleanup opportunity Windows offers,
//! since the supervisor sends no signal there and `TerminateProcess` follows the
//! stdin-EOF grace.
//!
//! A failed `apply` is RETRIED. It has to be: the orchestrator does not re-publish
//! a list it has already sent, so without a retry a single transient failure — a
//! registry key Explorer has open, a `.desktop` write that hits ENOSPC — would
//! leave the menu showing something else until the target list happened to change
//! again, possibly hours later.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use crate::surface::{MenuSurface, Target};

/// First wait before re-applying a list a surface refused, doubled at each
/// further failure.
const RETRY_BASE: Duration = Duration::from_secs(1);
/// Ceiling on that wait: a permanently broken surface must not spin, but must
/// still recover on its own if it heals.
const RETRY_CAP: Duration = Duration::from_secs(60);

/// Renders every change of the target list until the sender is dropped.
///
/// Returns when the orchestrator drops its sender, after a final empty render.
pub async fn run(
    mut surfaces: Vec<Box<dyn MenuSurface>>,
    mut targets: watch::Receiver<Arc<[Target]>>,
) {
    // The startup render: whatever a previous run (or a previous version) left
    // behind is replaced by exactly what we can offer right now — which at
    // startup is nothing, since no snapshot has arrived yet.
    // Bound to a local first: the borrow guard must not survive into the await.
    let mut wanted = targets.borrow_and_update().clone();
    let mut backoff = RETRY_BASE;
    loop {
        let failed;
        (surfaces, failed) = apply(surfaces, wanted.clone()).await;
        if failed {
            // Wait out the backoff, unless a new list arrives first — a fresh
            // list supersedes the one that failed, so there is nothing left to
            // retry.
            tokio::select! {
                _ = tokio::time::sleep(backoff) => {
                    backoff = (backoff * 2).min(RETRY_CAP);
                    continue;
                }
                changed = targets.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    backoff = RETRY_BASE;
                }
            }
        } else {
            backoff = RETRY_BASE;
            if targets.changed().await.is_err() {
                break;
            }
        }
        wanted = targets.borrow_and_update().clone();
    }
    apply(surfaces, Arc::from([])).await;
}

/// Hands the surfaces to a blocking thread, applies `targets` to each, and takes
/// them back. Moving them through the closure (rather than sharing them behind a
/// lock) is what makes "one rewrite at a time" structural.
///
/// Returns the surfaces and whether ANY of them failed to show the list.
async fn apply(
    mut surfaces: Vec<Box<dyn MenuSurface>>,
    targets: Arc<[Target]>,
) -> (Vec<Box<dyn MenuSurface>>, bool) {
    if surfaces.is_empty() {
        return (surfaces, false);
    }
    let blocking = tokio::task::spawn_blocking(move || {
        let mut failed = false;
        for surface in surfaces.iter_mut() {
            // A surface is a pile of platform FFI (registry, COM, Cocoa): one
            // that panics must not take the others — nor our only way to remove
            // the entries — down with it.
            let outcome =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| surface.apply(&targets)));
            match outcome {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    eprintln!(
                        "[1device-menu] {} could not be updated: {e}",
                        surface.name()
                    );
                    failed = true;
                }
                Err(_) => {
                    eprintln!("[1device-menu] {} panicked", surface.name());
                    failed = true;
                }
            }
        }
        (surfaces, failed)
    });
    // On a join error the blocking pool is gone (the runtime is shutting down):
    // no surfaces left to render onto, and the caller's loop ends on its next
    // iteration.
    blocking.await.unwrap_or((Vec::new(), false))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// One apply attempt: the device ids the surface was asked to show, and
    /// whether it accepted them.
    type Attempt = (Vec<String>, bool);

    /// Records every apply ATTEMPT, not just the renders — the retry loop is only
    /// observable that way.
    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<Attempt>>>);

    impl Recorder {
        fn attempts(&self) -> Vec<Attempt> {
            self.0.lock().expect("recorder").clone()
        }

        fn count(&self) -> usize {
            self.0.lock().expect("recorder").len()
        }

        /// The lists that were actually shown.
        fn shown(&self) -> Vec<Vec<String>> {
            self.attempts()
                .into_iter()
                .filter(|(_, ok)| *ok)
                .map(|(ids, _)| ids)
                .collect()
        }

        fn push(&self, ids: Vec<String>, ok: bool) {
            self.0.lock().expect("recorder").push((ids, ok));
        }
    }

    /// A surface that can be made to fail (or panic) a fixed number of times
    /// before it starts working — the transient a real registry or filesystem
    /// write produces.
    struct Fake {
        recorder: Recorder,
        failures_left: usize,
        panics: bool,
    }

    impl MenuSurface for Fake {
        fn name(&self) -> &'static str {
            "fake"
        }

        fn apply(&mut self, targets: &[Target]) -> std::io::Result<()> {
            let ids: Vec<String> = targets.iter().map(|t| t.device_id.clone()).collect();
            if self.failures_left > 0 {
                self.failures_left -= 1;
                self.recorder.push(ids, false);
                if self.panics {
                    panic!("surface exploded");
                }
                return Err(std::io::Error::other("nope"));
            }
            self.recorder.push(ids, true);
            Ok(())
        }
    }

    fn healthy(recorder: &Recorder) -> Box<dyn MenuSurface> {
        Box::new(Fake {
            recorder: recorder.clone(),
            failures_left: 0,
            panics: false,
        })
    }

    fn flaky(recorder: &Recorder, failures: usize, panics: bool) -> Box<dyn MenuSurface> {
        Box::new(Fake {
            recorder: recorder.clone(),
            failures_left: failures,
            panics,
        })
    }

    fn targets(ids: &[&str]) -> Arc<[Target]> {
        ids.iter()
            .map(|id| Target {
                device_id: (*id).to_string(),
                name: (*id).to_string(),
                platform: "linux".into(),
            })
            .collect::<Vec<_>>()
            .into()
    }

    #[tokio::test]
    async fn it_renders_at_startup_then_on_every_change_and_empties_at_the_end() {
        let recorder = Recorder::default();
        let (tx, rx) = watch::channel(targets(&[]));
        let applier = tokio::spawn(run(vec![healthy(&recorder)], rx));

        // Wait for the startup render before touching the list: this test is
        // about the sequence, not about coalescing (covered below), and without
        // the wait the applier's first look could already see `d_1`.
        wait_for(&recorder, 1).await;
        tx.send(targets(&["d_1"])).expect("send");
        wait_for(&recorder, 2).await;
        tx.send(targets(&["d_1", "d_2"])).expect("send");
        wait_for(&recorder, 3).await;

        drop(tx);
        applier.await.expect("applier");

        assert_eq!(
            recorder.shown(),
            vec![
                Vec::<String>::new(),
                vec!["d_1".to_string()],
                vec!["d_1".to_string(), "d_2".to_string()],
                // The stop: no manager, no entry.
                Vec::<String>::new(),
            ]
        );
    }

    #[tokio::test]
    async fn a_burst_collapses_to_the_latest_list() {
        let recorder = Recorder::default();
        let (tx, rx) = watch::channel(targets(&[]));
        let applier = tokio::spawn(run(vec![healthy(&recorder)], rx));

        // Parked on `changed()` after its startup render…
        wait_for(&recorder, 1).await;
        // …then three lists without yielding: it can only observe the last one.
        for ids in [&["d_1"][..], &["d_1", "d_2"][..], &["d_9"][..]] {
            tx.send(targets(ids)).expect("send");
        }
        drop(tx);
        applier.await.expect("applier");

        let shown = recorder.shown();
        assert!(
            shown.contains(&vec!["d_9".to_string()]),
            "the latest list must be rendered: {shown:?}"
        );
        assert!(
            !shown.contains(&vec!["d_1".to_string()]),
            "an intermediate list must not be: {shown:?}"
        );
        assert_eq!(shown.last(), Some(&Vec::<String>::new()));
    }

    /// The orchestrator never re-publishes a list it has already sent, so if a
    /// transient failure were not retried here the menu would stay wrong until
    /// the target list happened to change again.
    #[tokio::test]
    async fn a_list_a_surface_refused_is_retried_until_it_shows() {
        let recorder = Recorder::default();
        let (tx, rx) = watch::channel(targets(&[]));
        // Fails its first two applies (startup, then the real list), then works.
        let applier = tokio::spawn(run(vec![flaky(&recorder, 2, false)], rx));

        wait_for(&recorder, 1).await;
        tx.send(targets(&["d_1"])).expect("send");

        // Without a retry this would never appear: nothing re-sends `[d_1]`.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while !recorder.shown().contains(&vec!["d_1".to_string()]) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "never shown: {:?}",
                recorder.attempts()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        drop(tx);
        applier.await.expect("applier");
    }

    /// A fresh list supersedes the one that failed: retrying the old one would
    /// briefly show something the manager no longer vouches for.
    #[tokio::test]
    async fn a_new_list_supersedes_the_one_being_retried() {
        let recorder = Recorder::default();
        let (tx, rx) = watch::channel(targets(&[]));
        let applier = tokio::spawn(run(vec![flaky(&recorder, 1, false)], rx));

        // The startup apply fails and enters the backoff.
        wait_for(&recorder, 1).await;
        tx.send(targets(&["d_9"])).expect("send");
        wait_for(&recorder, 2).await;

        drop(tx);
        applier.await.expect("applier");

        let shown = recorder.shown();
        assert_eq!(
            shown.first(),
            Some(&vec!["d_9".to_string()]),
            "the retry must pick up the NEW list: {:?}",
            recorder.attempts()
        );
    }

    #[tokio::test]
    async fn one_panicking_surface_does_not_stop_the_others() {
        let recorder = Recorder::default();
        let (tx, rx) = watch::channel(targets(&[]));
        let surfaces: Vec<Box<dyn MenuSurface>> = vec![
            flaky(&recorder, 1, true),
            flaky(&recorder, 1, false),
            healthy(&recorder),
        ];
        let applier = tokio::spawn(run(surfaces, rx));

        // Startup: the panicking one, the failing one, and the healthy one are
        // all driven — the panic must not swallow the two behind it.
        wait_for(&recorder, 3).await;
        tx.send(targets(&["d_1"])).expect("send");

        // All three eventually show the real list, the two flaky ones having
        // recovered on the retry.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while recorder
            .shown()
            .iter()
            .filter(|ids| *ids == &vec!["d_1".to_string()])
            .count()
            < 3
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "not all surfaces recovered: {:?}",
                recorder.attempts()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        drop(tx);
        applier.await.expect("applier");

        // And every surface was given the final empty list: the last three
        // attempts are the shutdown sweep, one per surface, all accepted.
        let attempts = recorder.attempts();
        assert_eq!(
            &attempts[attempts.len() - 3..],
            &[
                (Vec::<String>::new(), true),
                (Vec::<String>::new(), true),
                (Vec::<String>::new(), true)
            ],
            "the stop must clear every surface: {attempts:?}"
        );
    }

    async fn wait_for(recorder: &Recorder, count: usize) {
        for _ in 0..500 {
            if recorder.count() >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("only {} attempts", recorder.count());
    }
}
