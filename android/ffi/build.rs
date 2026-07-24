// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

fn main() {
    // liblog: for __android_log_write (our logcat sink). Android only — the
    // shim is otherwise host-checkable without the NDK.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android") {
        println!("cargo:rustc-link-lib=log");
    }
}
