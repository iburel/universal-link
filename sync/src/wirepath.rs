// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The wire-path gate (doc/sync-engine.md, section 4): the fail-closed
//! validation every path arriving in ANY dialect message passes BEFORE
//! absorption, and the rule local names are measured against too (a name
//! that cannot cross the wire is indexed as ignored, section 8).
//!
//! A hostile sibling's engine gets no write and no unlink outside the
//! root, ever: deletes, applies, resolves and fills operate only on gated
//! paths.

use unicode_normalization::{UnicodeNormalization, is_nfc};

/// Reserved staging directory at each set root (doc/sync-engine.md,
/// section 6): excluded from watching, indexing and publishing, refused on
/// the wire.
pub const STAGING_DIR: &str = ".1device.tmp";

/// Suffix of the EXDEV fallback temp names (`.<name>.1dtmp`), on the same
/// exclusion list.
pub const STAGING_SUFFIX: &str = ".1dtmp";

/// Budget per component, UTF-8 bytes: every major filesystem can create it,
/// with room for the conflict-copy suffix.
const COMPONENT_MAX_BYTES: usize = 200;

/// Bound on a whole wire path: far above any real tree, a cap on abuse.
const PATH_MAX_BYTES: usize = 4096;

/// Validates one wire path. `Ok` means: relative, `/`-separated, NFC, every
/// component within the gate. The reason is for the log and the ignored
/// list.
pub fn check_wire_path(path: &str) -> Result<(), &'static str> {
    if path.is_empty() {
        return Err("empty path");
    }
    if path.len() > PATH_MAX_BYTES {
        return Err("path too long");
    }
    if path.starts_with('/') || path.ends_with('/') {
        return Err("not a relative path");
    }
    for component in path.split('/') {
        check_component(component)?;
    }
    Ok(())
}

/// Validates one path component (also the whole name of a single-file set,
/// and what local names are measured against).
pub fn check_component(name: &str) -> Result<(), &'static str> {
    if name.is_empty() || name == "." || name == ".." {
        return Err("dot or empty component");
    }
    if name.len() > COMPONENT_MAX_BYTES {
        return Err("component over the byte budget");
    }
    if name.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Err("control character");
    }
    if name.contains('\\') || name.contains(':') {
        return Err("separator character");
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Err("trailing dot or space");
    }
    // NFC everywhere: two byte spellings of one name must not both cross.
    if !is_nfc(name) {
        return Err("not NFC");
    }
    if name == STAGING_DIR || (name.starts_with('.') && name.ends_with(STAGING_SUFFIX)) {
        return Err("reserved staging name");
    }
    if windows_reserved(name) {
        return Err("reserved device name");
    }
    Ok(())
}

/// The classic Win32 reserved device names, which Windows reserves with ANY
/// extension and case-insensitively: `CON`, `PRN`, `AUX`, `NUL`, `COM1-9`,
/// `LPT1-9`.
fn windows_reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name);
    let stem = stem.to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit()
            && stem.as_bytes()[3] != b'0')
}

/// Windows only: the extended-length form of an absolute path
/// (`\\?\C:\...`), which lifts the 260-character MAX_PATH limit for every
/// call that takes it. A deep tree under a long root must not wedge, and
/// the prefix is invisible to the rest of the code (elsewhere, and for a
/// path that already carries it, this is the identity).
pub fn long_path(path: &std::path::Path) -> std::path::PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        if path.is_absolute() && !text.starts_with(r"\\") {
            return std::path::PathBuf::from(format!(r"\\?\{text}"));
        }
    }
    path.to_path_buf()
}

/// The NFC form of a local name, when mapping is this platform's job: on
/// macOS the disk hands back NFD and the filesystem opens either spelling,
/// so the wire form is the NFC mapping. Elsewhere the disk name IS the
/// name, and a non-NFC one fails the gate instead (fail-visible).
pub fn wire_name(disk_name: &str) -> Option<String> {
    if is_nfc(disk_name) {
        return Some(disk_name.to_string());
    }
    if cfg!(target_os = "macos") {
        return Some(disk_name.nfc().collect());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gate_admits_ordinary_paths() {
        for good in [
            "a.txt",
            "dir/sub/file.rs",
            "caf\u{00E9}/menu.md",
            "spaces are fine/really.txt",
            ".hidden/config",
            "comport/COM10.log",
            "console.d/CONF",
        ] {
            assert_eq!(check_wire_path(good), Ok(()), "should pass: {good}");
        }
    }

    #[test]
    fn the_gate_refuses_everything_hostile() {
        for (bad, why) in [
            ("", "empty path"),
            ("/etc/passwd", "not a relative path"),
            ("dir/", "not a relative path"),
            ("a//b", "dot or empty component"),
            ("../up", "dot or empty component"),
            ("a/./b", "dot or empty component"),
            ("a\\b", "separator character"),
            ("c:stream", "separator character"),
            ("bell\u{0007}", "control character"),
            ("line\nbreak", "control character"),
            ("del\u{007f}", "control character"),
            ("trailing.", "trailing dot or space"),
            ("trailing ", "trailing dot or space"),
            ("CON", "reserved device name"),
            ("con.txt", "reserved device name"),
            ("Prn.tar.gz", "reserved device name"),
            ("COM1", "reserved device name"),
            ("lpt9.log", "reserved device name"),
            ("cafe\u{0301}/nfd.txt", "not NFC"),
            (".1device.tmp/anything", "reserved staging name"),
            ("sub/.1device.tmp", "reserved staging name"),
            (".partial.1dtmp", "reserved staging name"),
        ] {
            assert_eq!(check_wire_path(bad), Err(why), "for {bad:?}");
        }
        // COM0/LPT0 are NOT reserved; neither is a long component under the
        // budget or a name merely containing a reserved stem.
        assert_eq!(check_wire_path("COM0"), Ok(()));
        assert_eq!(check_wire_path("CONSOLE"), Ok(()));
        assert_eq!(check_component(&"x".repeat(200)), Ok(()));
        assert_eq!(
            check_component(&"x".repeat(201)),
            Err("component over the byte budget")
        );
    }

    #[test]
    fn wire_names_map_nfd_only_where_the_disk_does() {
        assert_eq!(wire_name("caf\u{00E9}"), Some("caf\u{00E9}".to_string()));
        let mapped = wire_name("cafe\u{0301}");
        if cfg!(target_os = "macos") {
            assert_eq!(mapped, Some("caf\u{00E9}".to_string()));
        } else {
            assert_eq!(mapped, None);
        }
    }
}
