// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Turning the couriers of ONE gesture into ONE transfer.
//!
//! # Why this exists
//!
//! Windows is the reason. The classic shortcut menu invokes a verb **once per
//! selected item** — a separate process per file — unless the verb opts into the
//! `Player` multi-select model, and even then the shell splits a selection too
//! long for one command line into several invocations. Ten selected files would
//! otherwise become ten `files.send` calls: ten transfers, ten notifications on the
//! receiving PC, ten entries in its history, for one gesture the user made once.
//!
//! So a courier's request is not sent through immediately: it joins a batch keyed
//! by the target device, and the batch is sent when the burst goes quiet. Every
//! courier of the batch then gets the same answer — the same transfer id, or the
//! same refusal.
//!
//! # Best effort, never lossy
//!
//! The guarantee is deliberately weak: **as few transfers as possible, and never a
//! file left behind**. Nothing tells us how many processes the shell is about to
//! start, so a batch closes on a quiet window rather than on a count; a machine
//! slow enough to spread its process launches wider than [`COALESCE_WINDOW`] simply
//! gets two transfers instead of one. What must never happen is the opposite —
//! waiting for a courier that will never come, which is what [`MAX_COALESCE_DELAY`]
//! bounds.
//!
//! # One window for every platform
//!
//! A Linux or macOS click already carries the whole selection (Dolphin's `%F`, a
//! Nautilus script's argv), so there the window normally merges nothing and only
//! delays the send by a quarter of a second — imperceptible next to starting a
//! process and opening a transfer. It is kept anyway, and identical everywhere: a
//! Windows-only code path would be exercised by no CI job that runs this suite, and
//! the coalescing is precisely the part that must not be wrong.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::channel::{Response, error};

/// Quiet window that closes a batch: no further courier for this device in that
/// long, and it goes. A quarter of a second is far longer than the gap between two
/// processes the shell starts back to back, and short enough that a single click —
/// the normal case everywhere but a Windows multi-select — still feels immediate.
pub const COALESCE_WINDOW: Duration = Duration::from_millis(250);
/// Longest a batch may be held. The quiet window is extended by every courier, so
/// this is what stops a very large selection (or a user clicking repeatedly) from
/// postponing the send indefinitely.
pub const MAX_COALESCE_DELAY: Duration = Duration::from_secs(2);
/// Paths in one batch, beyond which it is sent at once instead of waiting out the
/// window. Bounds what a burst can accumulate in memory, and a batch this size has
/// nothing to gain from waiting.
pub const MAX_BATCH_PATHS: usize = 4096;
/// Couriers waiting to join a batch. They arrive at the rate the shell can start
/// processes; this only stops an unbounded queue from forming behind a batching
/// task that is busy issuing a send.
const QUEUE: usize = 256;

/// Issues one `files.send`. A seam, so the batching can be tested with virtual
/// time and no Core at all.
pub type SendFiles = Arc<
    dyn Fn(String, Vec<PathBuf>) -> Pin<Box<dyn Future<Output = Response> + Send>> + Send + Sync,
>;

/// Handle a courier task uses to submit a click.
#[derive(Clone)]
pub struct Clicks {
    tx: mpsc::Sender<Click>,
}

struct Click {
    device_id: String,
    paths: Vec<PathBuf>,
    reply: oneshot::Sender<Response>,
}

/// Clicks for one device, waiting to be sent together.
struct Batch {
    paths: Vec<PathBuf>,
    /// Same paths, for the deduplication. Two couriers of the same gesture never
    /// name the same file, but a user clicking the same entry twice does — and
    /// sending one file twice in one transfer means nothing.
    seen: HashSet<PathBuf>,
    waiters: Vec<oneshot::Sender<Response>>,
    /// When this batch goes, unless another courier extends it.
    due: Instant,
    /// And the deadline that extension can never push past.
    hard: Instant,
}

impl Clicks {
    /// Starts the batching task with the production timings.
    pub fn spawn(send: SendFiles) -> (Clicks, JoinHandle<()>) {
        Clicks::spawn_with(COALESCE_WINDOW, MAX_COALESCE_DELAY, send)
    }

    /// Same, with the timings injected — how the batching is tested.
    pub fn spawn_with(
        window: Duration,
        cap: Duration,
        send: SendFiles,
    ) -> (Clicks, JoinHandle<()>) {
        let (tx, rx) = mpsc::channel(QUEUE);
        let task = tokio::spawn(batch(rx, window, cap, send));
        (Clicks { tx }, task)
    }

    /// Submits one courier's click and waits for the outcome of the batch it lands
    /// in.
    pub async fn submit(&self, device_id: String, paths: Vec<PathBuf>) -> Response {
        let (reply, answer) = oneshot::channel();
        let click = Click {
            device_id,
            paths,
            reply,
        };
        if self.tx.send(click).await.is_err() {
            // The batching task is gone: the manager is stopping.
            return Response::failed(error::STOPPING);
        }
        match answer.await {
            Ok(response) => response,
            Err(_) => Response::failed(error::STOPPING),
        }
    }
}

/// The batching task: accumulate, then send when the burst goes quiet.
async fn batch(mut rx: mpsc::Receiver<Click>, window: Duration, cap: Duration, send: SendFiles) {
    let mut batches: HashMap<String, Batch> = HashMap::new();
    loop {
        let next = batches.values().map(|b| b.due).min();
        let timer = async {
            match next {
                Some(due) => tokio::time::sleep_until(due).await,
                // Nothing pending: only a courier can wake us.
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            // A due batch is flushed before a new courier is taken in, so a
            // steady stream of clicks cannot starve the batch it is extending —
            // the hard cap has to be able to fire.
            biased;

            _ = timer => flush_due(&mut batches, &send),

            click = rx.recv() => match click {
                // Every courier task, and the accept loop, are gone: nobody is
                // waiting on a batch any more.
                None => return,
                Some(click) => admit(&mut batches, click, window, cap, &send),
            },
        }
    }
}

/// Adds a click to its device's batch, creating it if this is the first.
fn admit(
    batches: &mut HashMap<String, Batch>,
    click: Click,
    window: Duration,
    cap: Duration,
    send: &SendFiles,
) {
    let now = Instant::now();
    let device_id = click.device_id;
    let batch = batches.entry(device_id.clone()).or_insert_with(|| Batch {
        paths: Vec::new(),
        seen: HashSet::new(),
        waiters: Vec::new(),
        due: now,
        hard: now + cap,
    });
    for path in click.paths {
        if batch.seen.insert(path.clone()) {
            batch.paths.push(path);
        }
    }
    batch.waiters.push(click.reply);
    batch.due = std::cmp::min(now + window, batch.hard);
    if batch.paths.len() >= MAX_BATCH_PATHS {
        flush(batches, &device_id, send);
    }
}

/// Sends every batch whose window has elapsed.
fn flush_due(batches: &mut HashMap<String, Batch>, send: &SendFiles) {
    let now = Instant::now();
    let due: Vec<String> = batches
        .iter()
        .filter(|(_, batch)| batch.due <= now)
        .map(|(device_id, _)| device_id.clone())
        .collect();
    for device_id in due {
        flush(batches, &device_id, send);
    }
}

/// Issues one batch's `files.send` and answers its couriers, in a task of its own:
/// a request takes up to the client's timeout, and another device's batch must not
/// wait behind it.
fn flush(batches: &mut HashMap<String, Batch>, device_id: &str, send: &SendFiles) {
    let Some(batch) = batches.remove(device_id) else {
        return;
    };
    let send = send.clone();
    let device_id = device_id.to_string();
    tokio::spawn(async move {
        let response = send(device_id, batch.paths).await;
        for waiter in batch.waiters {
            // A courier that hung up while waiting gets nothing, and that is
            // fine: the send happened either way.
            let _ = waiter.send(response.clone());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// One `files.send` the seam was asked for: the device, and the batch.
    type Sent = (String, Vec<PathBuf>);

    /// Records what was sent, and answers with whatever the test asked for.
    #[derive(Clone)]
    struct Recorder {
        sent: Arc<Mutex<Vec<Sent>>>,
        answer: Response,
    }

    impl Recorder {
        fn new(answer: Response) -> Recorder {
            Recorder {
                sent: Arc::new(Mutex::new(Vec::new())),
                answer,
            }
        }

        fn seam(&self) -> SendFiles {
            let me = self.clone();
            Arc::new(move |device_id, paths| {
                let me = me.clone();
                Box::pin(async move {
                    me.sent.lock().expect("lock").push((device_id, paths));
                    me.answer.clone()
                })
            })
        }

        fn sent(&self) -> Vec<Sent> {
            self.sent.lock().expect("lock").clone()
        }
    }

    fn accepted() -> Response {
        Response::Accepted {
            transfer_id: "t_1a2b".into(),
        }
    }

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    fn names(paths: &[PathBuf]) -> Vec<String> {
        paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect()
    }

    /// The whole point of the module: the Windows classic menu starts one process
    /// per selected file, and the user made ONE gesture. Ten couriers must become
    /// one transfer carrying the ten files — not ten transfers.
    #[tokio::test(start_paused = true)]
    async fn a_burst_of_couriers_becomes_one_transfer() {
        let recorder = Recorder::new(accepted());
        let (clicks, _task) = Clicks::spawn_with(
            Duration::from_millis(250),
            Duration::from_secs(2),
            recorder.seam(),
        );

        let mut couriers = tokio::task::JoinSet::new();
        for i in 0..10 {
            let clicks = clicks.clone();
            couriers.spawn(async move {
                clicks
                    .submit("d_1".into(), paths(&[&format!("/f{i}.txt")]))
                    .await
            });
            // The gap between two processes the shell starts.
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // Every courier gets the answer, and it is the same one.
        let answers = couriers.join_all().await;
        assert_eq!(answers.len(), 10);
        for answer in &answers {
            assert_eq!(answer, &accepted());
        }

        let sent = recorder.sent();
        assert_eq!(sent.len(), 1, "one gesture must be one transfer");
        assert_eq!(sent[0].0, "d_1");
        let mut got = names(&sent[0].1);
        got.sort();
        let mut expected: Vec<String> = (0..10).map(|i| format!("/f{i}.txt")).collect();
        expected.sort();
        assert_eq!(got, expected, "every selected file must be in the batch");
    }

    /// Two entries clicked in the same breath are two destinations: merging them
    /// would send each device the other's files.
    #[tokio::test(start_paused = true)]
    async fn two_devices_are_never_merged() {
        let recorder = Recorder::new(accepted());
        let (clicks, _task) = Clicks::spawn_with(
            Duration::from_millis(250),
            Duration::from_secs(2),
            recorder.seam(),
        );

        let a = tokio::spawn({
            let clicks = clicks.clone();
            async move { clicks.submit("d_a".into(), paths(&["/a.txt"])).await }
        });
        let b = tokio::spawn({
            let clicks = clicks.clone();
            async move { clicks.submit("d_b".into(), paths(&["/b.txt"])).await }
        });
        assert_eq!(a.await.expect("join"), accepted());
        assert_eq!(b.await.expect("join"), accepted());

        let mut sent = recorder.sent();
        sent.sort_by(|x, y| x.0.cmp(&y.0));
        assert_eq!(sent.len(), 2);
        assert_eq!(
            (sent[0].0.as_str(), names(&sent[0].1)),
            ("d_a", vec!["/a.txt".to_string()])
        );
        assert_eq!(
            (sent[1].0.as_str(), names(&sent[1].1)),
            ("d_b", vec!["/b.txt".to_string()])
        );
    }

    /// The window is extended by every courier, so without the cap a long enough
    /// stream of them would hold the transfer for ever. The user clicked: the files
    /// have to go.
    #[tokio::test(start_paused = true)]
    async fn a_stream_of_couriers_cannot_postpone_the_transfer_for_ever() {
        let recorder = Recorder::new(accepted());
        let (clicks, _task) = Clicks::spawn_with(
            Duration::from_millis(250),
            Duration::from_secs(2),
            recorder.seam(),
        );

        let mut couriers = tokio::task::JoinSet::new();
        // 40 couriers, one every 100 ms: the quiet window never elapses, so only
        // the hard cap can fire — at 2 s, i.e. after about 20 of them.
        for i in 0..40 {
            let clicks = clicks.clone();
            couriers.spawn(async move {
                clicks
                    .submit("d_1".into(), paths(&[&format!("/f{i}.txt")]))
                    .await
            });
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        couriers.join_all().await;

        let sent = recorder.sent();
        assert!(
            sent.len() >= 2,
            "the cap must have closed a batch mid-stream, got {} transfers",
            sent.len()
        );
        let first = sent[0].1.len();
        assert!(
            (15..=25).contains(&first),
            "the first batch should hold about 2 s worth of clicks, got {first}"
        );
        let total: usize = sent.iter().map(|(_, p)| p.len()).sum();
        assert_eq!(total, 40, "no click may be dropped by the cap");
    }

    /// A selection larger than the ceiling is sent at once rather than held: the
    /// batch has nothing left to gain from waiting, and the memory it holds is not
    /// unbounded.
    #[tokio::test(start_paused = true)]
    async fn a_full_batch_goes_without_waiting_out_the_window() {
        let recorder = Recorder::new(accepted());
        let (clicks, _task) = Clicks::spawn_with(
            Duration::from_secs(3600),
            Duration::from_secs(3600),
            recorder.seam(),
        );

        let big: Vec<PathBuf> = (0..MAX_BATCH_PATHS)
            .map(|i| PathBuf::from(format!("/f{i}.txt")))
            .collect();
        // The window is an hour, so nothing but the ceiling can have sent this.
        assert_eq!(clicks.submit("d_1".into(), big).await, accepted());
        assert_eq!(recorder.sent().len(), 1);
        assert_eq!(recorder.sent()[0].1.len(), MAX_BATCH_PATHS);
    }

    /// Clicking the same entry twice on the same selection must not offer the same
    /// file twice.
    #[tokio::test(start_paused = true)]
    async fn the_same_file_twice_is_offered_once() {
        let recorder = Recorder::new(accepted());
        let (clicks, _task) = Clicks::spawn_with(
            Duration::from_millis(250),
            Duration::from_secs(2),
            recorder.seam(),
        );

        let first = tokio::spawn({
            let clicks = clicks.clone();
            async move {
                clicks
                    .submit("d_1".into(), paths(&["/a.txt", "/b.txt"]))
                    .await
            }
        });
        let second = tokio::spawn({
            let clicks = clicks.clone();
            async move {
                clicks
                    .submit("d_1".into(), paths(&["/b.txt", "/c.txt"]))
                    .await
            }
        });
        first.await.expect("join");
        second.await.expect("join");

        let sent = recorder.sent();
        assert_eq!(sent.len(), 1);
        let mut got = names(&sent[0].1);
        got.sort();
        assert_eq!(got, ["/a.txt", "/b.txt", "/c.txt"]);
    }

    /// A refusal is the batch's answer too: every courier of the gesture must
    /// learn it, not just the one that happened to arrive first.
    #[tokio::test(start_paused = true)]
    async fn a_refusal_reaches_every_courier_of_the_batch() {
        let refused = Response::failed("DEVICE_OFFLINE");
        let recorder = Recorder::new(refused.clone());
        let (clicks, _task) = Clicks::spawn_with(
            Duration::from_millis(250),
            Duration::from_secs(2),
            recorder.seam(),
        );

        let mut couriers = tokio::task::JoinSet::new();
        for i in 0..3 {
            let clicks = clicks.clone();
            couriers.spawn(async move {
                clicks
                    .submit("d_1".into(), paths(&[&format!("/f{i}.txt")]))
                    .await
            });
        }
        for answer in couriers.join_all().await {
            assert_eq!(answer, refused);
        }
        assert_eq!(recorder.sent().len(), 1);
    }

    /// A second gesture is a second transfer: the batch that already went does not
    /// swallow it.
    #[tokio::test(start_paused = true)]
    async fn a_click_after_a_batch_has_gone_starts_a_new_one() {
        let recorder = Recorder::new(accepted());
        let (clicks, _task) = Clicks::spawn_with(
            Duration::from_millis(250),
            Duration::from_secs(2),
            recorder.seam(),
        );

        assert_eq!(
            clicks.submit("d_1".into(), paths(&["/a.txt"])).await,
            accepted()
        );
        assert_eq!(
            clicks.submit("d_1".into(), paths(&["/b.txt"])).await,
            accepted()
        );

        let sent = recorder.sent();
        assert_eq!(sent.len(), 2);
        assert_eq!(names(&sent[0].1), ["/a.txt"]);
        assert_eq!(names(&sent[1].1), ["/b.txt"]);
    }

    /// A courier that arrives while the manager is shutting down must be told, not
    /// left hanging until its own timeout.
    #[tokio::test(start_paused = true)]
    async fn a_click_arriving_after_the_manager_stopped_is_refused_at_once() {
        let recorder = Recorder::new(accepted());
        let (clicks, task) = Clicks::spawn_with(
            Duration::from_millis(250),
            Duration::from_secs(2),
            recorder.seam(),
        );
        task.abort();
        let _ = task.await;

        assert_eq!(
            clicks.submit("d_1".into(), paths(&["/a.txt"])).await,
            Response::failed(error::STOPPING)
        );
        assert!(recorder.sent().is_empty());
    }
}
