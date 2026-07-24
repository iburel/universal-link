// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! A minimal logcat sink. The Core speaks `tracing`; on Android there is no
//! terminal, and native stdout/stderr goes to `/dev/null` by default — so we
//! forward `tracing` events to `__android_log_write` (liblog), the only place
//! `adb logcat` will show them. Kept dependency-free on purpose (one C symbol,
//! linked in `build.rs`), rather than pulling a logging crate.

use std::io;

const TAG: &str = "ULCore";

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

/// Emit a single line to logcat under our tag.
pub fn line(msg: &str) {
    sys::write(TAG, msg);
}

/// `MakeWriter` that sends every formatted `tracing` line to logcat.
pub struct MakeLogcat;

pub struct LogcatWriter;

impl io::Write for LogcatWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let text = String::from_utf8_lossy(buf);
        let trimmed = text.trim_end_matches(['\r', '\n']);
        if !trimmed.is_empty() {
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
