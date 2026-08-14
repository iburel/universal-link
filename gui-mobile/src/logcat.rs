// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! A minimal logcat sink. The Core speaks `tracing`; on Android there is no
//! terminal, and native stdout/stderr goes to `/dev/null` by default — so we
//! forward `tracing` events to `__android_log_write` (liblog), the only place
//! `adb logcat` will show them. Kept dependency-free on purpose (one C symbol,
//! linked in `build.rs`), rather than pulling a logging crate.
//!
//! Unlike the desktop's log, this one has no file behind it: logcat is a ring
//! buffer shared with the whole system, and an app that repeats itself pushes
//! out what came before. So a repeating line is rate-limited per line here
//! (`RepeatFilter`) — measured need, see its documentation. Whatever is held
//! back is counted out loud; nothing is dropped in silence.

use std::io;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use onedevice_daemon::logging::RepeatFilter;

const TAG: &str = "ULCore";

/// Long enough that a burst costs two lines a minute instead of a hundred,
/// short enough that a line which starts repeating is still news while it is
/// happening.
const REPEAT_WINDOW: Duration = Duration::from_secs(60);

/// One filter for the whole process: `make_writer` hands out a fresh writer per
/// event, so the state cannot live in the writer.
static REPEATS: Mutex<Option<RepeatFilter>> = Mutex::new(None);

#[cfg(target_os = "android")]
mod sys {
    use std::ffi::CString;
    use std::os::raw::{c_char, c_int};

    // liblog. ANDROID_LOG_INFO == 4.
    const ANDROID_LOG_INFO: c_int = 4;

    unsafe extern "C" {
        fn __android_log_write(prio: c_int, tag: *const c_char, text: *const c_char) -> c_int;
    }

    pub(super) fn write(tag: &str, msg: &str) {
        // Interior NULs would truncate the C string: replace them defensively.
        let msg = msg.replace('\0', "\u{fffd}");
        if let (Ok(tag), Ok(text)) = (CString::new(tag), CString::new(msg)) {
            // SAFETY: both pointers are valid, NUL-terminated C strings that
            // outlive the call.
            unsafe {
                __android_log_write(ANDROID_LOG_INFO, tag.as_ptr(), text.as_ptr());
            }
        }
    }
}

#[cfg(not(target_os = "android"))]
mod sys {
    pub(super) fn write(tag: &str, msg: &str) {
        eprintln!("[{tag}] {msg}");
    }
}

/// `MakeWriter` that sends every formatted `tracing` line to logcat.
pub struct MakeLogcat;

pub struct LogcatWriter;

impl io::Write for LogcatWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let text = String::from_utf8_lossy(buf);
        // Leading space: with the timestamp gone the formatter still writes its
        // separator, and logcat would show it — twice over inside a summary,
        // which quotes the line.
        let trimmed = text.trim_end_matches(['\r', '\n']).trim_start_matches(' ');
        if trimmed.is_empty() {
            return Ok(buf.len());
        }
        // The lock is held across the writes so a summary cannot be separated
        // from the line it introduces, and it survives a poisoned mutex: a log
        // that panicked once must not stop logging.
        let mut guard = REPEATS.lock().unwrap_or_else(PoisonError::into_inner);
        let filter = guard.get_or_insert_with(|| RepeatFilter::new(REPEAT_WINDOW));
        let verdict = filter.observe(trimmed, Instant::now());
        for summary in &verdict.summaries {
            sys::write(TAG, summary);
        }
        if verdict.write_line {
            sys::write(TAG, trimmed);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for MakeLogcat {
    type Writer = LogcatWriter;

    fn make_writer(&'a self) -> Self::Writer {
        LogcatWriter
    }
}
