// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The root watcher (doc/sync-engine.md, section 8): `notify` behind a
//! plain thread that DEBOUNCES - a change starts a quiescence window, more
//! changes extend it, and only the quiet end of a burst reports upward.
//! Propagation is immediate whenever devices can talk (conflicts rare by
//! speed), and the reporting unit is the whole set: the rescan is the one
//! reconciler, so the watcher only ever says "look again".
//!
//! Overflow (a queue that dropped events) reports the same way - a rescan
//! covers whatever was missed. A watcher that cannot be INSTALLED at all
//! is the caller's problem to surface (degraded periodic scanning, a card
//! problem): this module just says so, loudly typed.

use std::path::Path;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use notify::Watcher as _;

use crate::records::SetKind;

/// Quiescence before a burst of changes reports (the letter: ~2 s).
pub const DEBOUNCE: Duration = Duration::from_secs(2);

/// A living watch on one set's root. Dropping it stops the watcher and,
/// in time, its thread.
pub struct WatchHandle {
    /// Kept alive: dropping the watcher closes the OS watch, which ends
    /// the event stream, which ends the debounce thread.
    _watcher: notify::RecommendedWatcher,
}

/// Starts watching `root` for the set `set_id`. Every quiet end of a burst
/// sends `set_id` on `quiesced` (a blocking send from the debounce thread;
/// the channel should be comfortably buffered). A single-file set watches
/// its PARENT, non-recursively (editors replace-by-rename on save), and
/// reports on any event there: the rescan filters to the one name.
pub fn watch(
    set_id: &str,
    root: &Path,
    kind: SetKind,
    debounce: Duration,
    quiesced: tokio::sync::mpsc::Sender<String>,
) -> Result<WatchHandle, String> {
    let (event_tx, event_rx) = std_mpsc::channel::<()>();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        // An error event usually means an overflow: a rescan covers it, so
        // every outcome is the same nudge.
        match &event {
            Ok(e) if !matters(e) => return,
            _ => {}
        }
        let _ = event_tx.send(());
    })
    .map_err(|e| format!("cannot create the watcher: {e}"))?;

    let (target, mode) = match kind {
        SetKind::Dir => (root.to_path_buf(), notify::RecursiveMode::Recursive),
        // The parent, filtered by the rescan rather than here: a save that
        // replaces-by-rename never fires on the file's own path.
        SetKind::File => (
            root.parent()
                .ok_or_else(|| "a single-file root needs a parent".to_string())?
                .to_path_buf(),
            notify::RecursiveMode::NonRecursive,
        ),
    };
    watcher
        .watch(&target, mode)
        .map_err(|e| format!("cannot install the watch: {e}"))?;

    let set_id = set_id.to_string();
    std::thread::spawn(move || {
        // The debounce loop: wait for a first event, then extend the
        // window while more arrive; report on quiescence. The thread ends
        // when the watcher (and with it the sender) is dropped.
        while event_rx.recv().is_ok() {
            while event_rx.recv_timeout(debounce).is_ok() {}
            if quiesced.blocking_send(set_id.clone()).is_err() {
                return;
            }
        }
    });

    Ok(WatchHandle { _watcher: watcher })
}

/// Which events deserve a rescan: anything that touches data or the
/// namespace. Pure accesses stay silent; everything unknown nudges (the
/// rescan is idempotent, a spurious nudge costs a walk).
fn matters(event: &notify::Event) -> bool {
    use notify::EventKind;
    !matches!(event.kind, EventKind::Access(_))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One quiet report per burst, none without one.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_burst_reports_once_after_quiescence() {
        let root = tempfile::tempdir().expect("root");
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let _handle = watch(
            "SETSETSETSETSETSETSETS",
            root.path(),
            SetKind::Dir,
            Duration::from_millis(100),
            tx,
        )
        .expect("watch");

        // A burst of writes...
        for i in 0..5 {
            std::fs::write(root.path().join(format!("f{i}.txt")), "x").expect("write");
        }
        // ...reports exactly once, after the quiet.
        let set = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("a report")
            .expect("channel open");
        assert_eq!(set, "SETSETSETSETSETSETSETS");
        // And silence follows silence.
        let extra = tokio::time::timeout(Duration::from_millis(400), rx.recv()).await;
        assert!(extra.is_err(), "no report without a change");

        // A later change reports again.
        std::fs::write(root.path().join("late.txt"), "y").expect("write");
        tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("a second report")
            .expect("channel open");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_single_file_set_watches_its_parent() {
        let dir = tempfile::tempdir().expect("dir");
        let file = dir.path().join("notes.txt");
        std::fs::write(&file, "n").expect("write");
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let _handle = watch(
            "SETSETSETSETSETSETSETS",
            &file,
            SetKind::File,
            Duration::from_millis(100),
            tx,
        )
        .expect("watch");

        // The editor's save dance: write a temp, rename over.
        let tmp = dir.path().join(".notes.swp");
        std::fs::write(&tmp, "n2").expect("write");
        std::fs::rename(&tmp, &file).expect("replace");
        tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("a report")
            .expect("channel open");
    }

    #[test]
    fn a_missing_root_refuses_loudly() {
        let root = tempfile::tempdir().expect("root");
        let gone = root.path().join("never");
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        assert!(watch("SETSETSETSETSETSETSETS", &gone, SetKind::Dir, DEBOUNCE, tx).is_err());
    }
}
