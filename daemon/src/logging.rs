// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The daemon's log. A Core launched at login has no terminal to shout at: the
//! file is authoritative, the error output is only added if someone is
//! watching (`stderr` attached to a terminal).
//!
//! The returned `WorkerGuard` must live until the end of `main`: it is its
//! `drop` that flushes the non-blocking writer's buffer. A shutdown that does
//! not go back through the return of `main` (SIGKILL) loses the last lines —
//! that is the price of non-blocking, and it is why graceful shutdown always
//! comes back into `main`.

use std::io::IsTerminal;
use std::time::{Duration, Instant};

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::filter::{EnvFilter, LevelFilter};
use tracing_subscriber::prelude::*;

/// Default level, and its override. `RUST_LOG` is too widely shared: a
/// developer who exports it for another tool must not make our daemon chatty.
const LOG_ENV: &str = "UNIVERSALLINK_LOG";

/// Installs the collector. Keep the guard, do not throw it away.
#[must_use]
pub fn init() -> Option<WorkerGuard> {
    let filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .with_env_var(LOG_ENV)
        .from_env_lossy();
    let Some((writer, guard)) = file_writer() else {
        // No usable log directory: we do not give up on logging, we make do
        // with what we have.
        tracing_subscriber::registry()
            .with(filter)
            .with(stderr_layer())
            .init();
        return None;
    };
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(writer),
        )
        .with(stderr_layer())
        .init();
    Some(guard)
}

/// The error output, only if someone is reading it. Generic over the layer
/// stack: its type depends on what it stacks onto, so it cannot be built once
/// for both branches of `init`.
fn stderr_layer<S>() -> Option<impl tracing_subscriber::Layer<S>>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    std::io::stderr().is_terminal().then(|| {
        tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_target(false)
    })
}

/// How much of a repeating line a summary quotes, so logcat is not handed a
/// second copy of a long one.
const QUOTE: usize = 100;

/// Distinct lines tracked at once. Past this the filter lets everything through
/// rather than choosing what to hide: a log that is too chatty beats a log that
/// silently drops the one line that mattered.
const TRACKED: usize = 16;

/// Rate-limits a line that keeps repeating, per line.
///
/// Where the log is a small ring buffer — Android's logcat — a repeating warning
/// pushes out everything worth reading; it cost me a diagnosis session. What it
/// actually looks like there, measured with the app backgrounded (the system
/// takes its network away, so every attempt at anything fails): 141 lines in
/// 45 s from THREE interleaved families — mDNS sends refused `EPERM`, relay
/// reconnections timing out, DNS lookups failing — and the runs are short, a
/// median of two lines. That measurement is why this counts per line over a
/// window instead of collapsing consecutive duplicates: with families taking
/// turns, consecutive-run collapsing removed barely half the lines and would
/// have announced "repeated 1 times" over and over.
///
/// Nothing is hidden: every distinct line is written the first time, and the
/// ones held back are counted out loud, quoting the line they belong to (with
/// three families interleaved, "the line above" would be a lie).
#[derive(Debug)]
pub struct RepeatFilter {
    window: Duration,
    /// Small enough that a linear scan is the right data structure.
    seen: Vec<Seen>,
}

#[derive(Debug)]
struct Seen {
    line: String,
    /// When this line was last written.
    written: Instant,
    /// Occurrences held back since then.
    held: u64,
}

/// What to write for the line just observed: summaries first, then the line
/// itself if it is not a repeat.
#[derive(Debug, PartialEq, Eq)]
pub struct Verdict {
    pub summaries: Vec<String>,
    pub write_line: bool,
}

impl RepeatFilter {
    pub fn new(window: Duration) -> Self {
        RepeatFilter {
            window,
            seen: Vec::new(),
        }
    }

    /// Registers `line` and says what to write for it. `now` is a parameter so
    /// the windows can be tested without sleeping.
    pub fn observe(&mut self, line: &str, now: Instant) -> Verdict {
        // Anything whose window has closed reports its count now, riding along
        // with whatever line is being written — otherwise a burst that stops
        // dead would take its final count with it. A retired entry is forgotten
        // entirely, so a line whose window just closed is written again below:
        // its next window starts from something a reader can see.
        let summaries = self.retire_expired(now);

        if let Some(seen) = self.seen.iter_mut().find(|s| s.line == line) {
            seen.held += 1;
            return Verdict {
                summaries,
                write_line: false,
            };
        }

        if self.seen.len() >= TRACKED {
            // Full of lines that are all still within their window: let this one
            // through untracked rather than evict someone else's count.
            return Verdict {
                summaries,
                write_line: true,
            };
        }
        self.seen.push(Seen {
            line: line.to_string(),
            written: now,
            held: 0,
        });
        Verdict {
            summaries,
            write_line: true,
        }
    }

    /// Forgets every entry whose window has closed, returning a summary for each
    /// one that held anything back.
    fn retire_expired(&mut self, now: Instant) -> Vec<String> {
        let mut summaries = Vec::new();
        let window = self.window;
        self.seen.retain(|seen| {
            if now.duration_since(seen.written) < window {
                return true;
            }
            if seen.held > 0 {
                summaries.push(summary(&seen.line, seen.held, window));
            }
            false
        });
        summaries
    }
}

fn summary(line: &str, held: u64, window: Duration) -> String {
    let quoted: String = line.chars().take(QUOTE).collect();
    let ellipsis = if quoted.chars().count() < line.chars().count() {
        "…"
    } else {
        ""
    };
    format!(
        "... {held} more in {}s of: {quoted}{ellipsis}",
        window.as_secs()
    )
}

fn file_writer() -> Option<(tracing_appender::non_blocking::NonBlocking, WorkerGuard)> {
    let dir = universallink_paths::log_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    // Daily rotation, seven files kept: enough to understand yesterday's
    // incident, not enough to fill a disk.
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("universallink")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&dir)
        .ok()?;
    Some(tracing_appender::non_blocking(appender))
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Duration = Duration::from_secs(60);

    /// The measured shape of the problem: three families taking turns, each
    /// repeating. Every distinct line is written ONCE, and the rest are held.
    #[test]
    fn interleaved_families_each_get_written_once() {
        let mut filter = RepeatFilter::new(WINDOW);
        let t = Instant::now();
        let lines = ["mdns EPERM", "relay timeout", "dns failed", "mdns EPERM"];
        let written: Vec<bool> = lines
            .iter()
            .map(|l| filter.observe(l, t).write_line)
            .collect();
        assert_eq!(written, vec![true, true, true, false]);

        // 40 more turns of the same three: nothing is written, nothing summarized
        // yet (the window is still open).
        for i in 0..40 {
            for line in ["mdns EPERM", "relay timeout", "dns failed"] {
                let verdict = filter.observe(line, t + Duration::from_millis(i * 300));
                assert!(!verdict.write_line, "{line} written again");
                assert!(verdict.summaries.is_empty(), "summarized too early");
            }
        }
    }

    #[test]
    fn a_closed_window_reports_its_count_and_writes_the_line_again() {
        let mut filter = RepeatFilter::new(WINDOW);
        let t = Instant::now();
        assert!(filter.observe("boom", t).write_line);
        for _ in 0..99 {
            assert!(!filter.observe("boom", t).write_line);
        }
        let verdict = filter.observe("boom", t + Duration::from_secs(61));
        assert_eq!(verdict.summaries.len(), 1);
        assert!(
            verdict.summaries[0].contains("99 more in 60s of: boom"),
            "{:?}",
            verdict.summaries
        );
        // Written again, so the next window opens on something visible.
        assert!(verdict.write_line);
    }

    /// A burst that stops dead still reports: the count rides along with the next
    /// line written, whatever it is.
    #[test]
    fn a_finished_burst_reports_on_the_next_line() {
        let mut filter = RepeatFilter::new(WINDOW);
        let t = Instant::now();
        filter.observe("burst", t);
        for _ in 0..5 {
            filter.observe("burst", t);
        }
        let verdict = filter.observe("something else", t + Duration::from_secs(61));
        assert!(verdict.write_line);
        assert_eq!(verdict.summaries.len(), 1);
        assert!(verdict.summaries[0].contains("5 more"), "{:?}", verdict);
    }

    /// A line seen once and never again costs no summary — silence is the right
    /// report for nothing held back.
    #[test]
    fn a_line_that_never_repeats_is_never_summarized() {
        let mut filter = RepeatFilter::new(WINDOW);
        let t = Instant::now();
        filter.observe("once", t);
        let verdict = filter.observe("later", t + Duration::from_secs(61));
        assert!(verdict.write_line);
        assert!(verdict.summaries.is_empty(), "{:?}", verdict.summaries);
    }

    /// Past the tracking capacity the filter stops holding anything back. The
    /// alternative would be evicting an entry and losing its count silently.
    #[test]
    fn too_many_distinct_lines_lets_everything_through() {
        let mut filter = RepeatFilter::new(WINDOW);
        let t = Instant::now();
        for i in 0..TRACKED {
            assert!(filter.observe(&format!("line {i}"), t).write_line);
        }
        // Untracked, so it is written…
        assert!(filter.observe("overflow", t).write_line);
        // …and so is its repeat, rather than being counted against a stranger.
        assert!(filter.observe("overflow", t).write_line);
        // A tracked line still behaves.
        assert!(!filter.observe("line 0", t).write_line);
    }

    #[test]
    fn a_long_line_is_quoted_not_repeated_whole() {
        let long = "x".repeat(QUOTE * 3);
        let text = summary(&long, 7, WINDOW);
        assert!(text.contains("7 more in 60s of: "), "{text}");
        assert!(text.ends_with('…'), "{text}");
        assert!(
            text.chars().count() < long.chars().count(),
            "the summary is not shorter than the line it quotes"
        );
    }

    /// An empty line is a line: the filter must not confuse it with "nothing
    /// seen yet" (the writer trims, but the type does not get to assume that).
    #[test]
    fn an_empty_line_is_tracked_like_any_other() {
        let mut filter = RepeatFilter::new(WINDOW);
        let t = Instant::now();
        assert!(filter.observe("", t).write_line);
        assert!(!filter.observe("", t).write_line);
    }
}
