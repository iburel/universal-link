// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The Windows family-A surfaces: a cascading entry in the classic shortcut menu,
//! and one shortcut per device in "Send to".
//!
//! - the **cascade** ([`Cascade`]) — a verb under `HKCU\Software\Classes\*\shell`
//!   (files) and `…\Directory\shell` (folders), whose submenu holds one child verb
//!   per target. Explorer reads these keys while it builds the menu, so a rewrite
//!   takes effect on the next right click: nothing to notify, no cache to flush.
//! - **Send to** ([`SendTo`]) — one `.lnk` per target in the user's `SendTo`
//!   folder. A flat surface, so the entries carry the product's name themselves
//!   (`PC A (1Device)`, decision 3 of the plan) rather than sitting in a
//!   submenu.
//!
//! Both are per-user (`HKEY_CURRENT_USER`, `%APPDATA%`): nothing here needs
//! administrator rights, which is what makes them family A at all.
//!
//! # What the shell does with a click, and why it forced a change upstream
//!
//! The classic menu invokes a verb **once per selected item** — one process per
//! file — unless the verb asks for the `Player` multi-select model. We ask for it,
//! but that is not enough to rely on: the shell still splits a selection too long
//! for one command line, and "Send to" is a drop, with a command-line limit of its
//! own. So the manager coalesces (see `clicks`); this surface only has to make sure
//! that whatever the shell decides, no file is left out.
//!
//! # Facts this is built on, read off a real Windows rather than remembered
//!
//! There is no source to read for the shell, so each of these was taken from what
//! shipping software does on a live machine (`HKLM\Software\Classes`), which is the
//! closest thing to a specification available:
//! - a cascade needs `ExtendedSubCommandsKey` naming a key (relative to
//!   `HKEY_CLASSES_ROOT`) that holds a `shell` subkey of child verbs. Microsoft's
//!   own `efscore.dll` entry (`*\shell\UpdateEncryptionSettingsWork`) points that
//!   value at ITSELF and keeps its children under `…\Shell\…`, which is the shape
//!   used here — one root's cascade is then entirely self-contained. The other
//!   documented form, a semicolon-separated `SubCommands` list, resolves its verbs
//!   from a `CommandStore` under `HKEY_LOCAL_MACHINE`, so it is out of reach for a
//!   per-user, unprivileged component.
//! - `MultiSelectModel=Player` is what VLC's shipping "add to playlist" verb uses
//!   with a plain `"%1"` command line to receive a whole selection at once.
//! - **`&` in a displayed label is a mnemonic marker**: Git's shipping entry
//!   spells its label `Open Git &GUI here` to underline the G. So a device name
//!   containing one must have it doubled, or half the name disappears. (The
//!   opposite of KIO, which escapes the ampersand itself — see `os::linux`.)
//! - a label starting with `@` is read as an indirect string
//!   (`@shell32.dll,-51608`), and that has no escape — the only answer is to drop
//!   the sigil.
//!
//! # Absolute, and never destructive
//!
//! Same two rules as the Linux surfaces. Every artifact carries [`MARKER`] — a
//! value on the cascade's own key, the description of a shortcut — and **nothing
//! without it is ever deleted**, because pruning means enumerating a key and a
//! folder that belong to the user. And a write that would not change anything is
//! skipped: the applier re-applies the current list at startup and after any
//! failure, and there is no reason to touch a key or rewrite a shortcut for that.

mod cascade;
mod sendto;

use std::io;
use std::path::{Path, PathBuf};

pub use cascade::Cascade;
pub use cascade::command_line as verb_command_line;
pub use sendto::SendTo;
pub use sendto::folder as send_to_folder;

use crate::surface::{HelperCommand, MenuSurface};

/// Marks an artifact as ours. Load-bearing: both surfaces enumerate a place the
/// user owns (a registry key, the `SendTo` folder) and delete what is no longer
/// wanted, and mistaking something else for a stale entry of ours would destroy
/// someone's work.
pub(crate) const MARKER: &str = "1device-menu:generated";

/// Where per-user class registrations live. `HKEY_CLASSES_ROOT` is the merged view
/// of this and the machine-wide one, with this taking precedence — so a verb
/// written here is what Explorer sees, and no administrator is involved.
const CLASSES: &str = r"Software\Classes";

/// The verb name our cascade takes under each class's `shell` key. Also the name
/// the pruning looks for, so it must stay stable across versions.
const VERB: &str = "1Device";

/// The suffix that names us in a flat surface, appended to the device label.
const SUFFIX: &str = " (1Device)";

/// Every Windows surface. The two cascades are independent registrations of the
/// same list — one for files, one for folders — and "Send to" is a third: a broken
/// one does not cost the others their entries.
pub fn surfaces(helper: HelperCommand) -> Vec<Box<dyn MenuSurface>> {
    let mut surfaces: Vec<Box<dyn MenuSurface>> = vec![
        Box::new(Cascade::files(CLASSES, helper.clone())),
        Box::new(Cascade::folders(CLASSES, helper.clone())),
    ];
    // Resolved once, here: the folder is a known folder and can be redirected, so
    // it is asked for rather than assumed. Without it there is simply no Send to
    // surface — the cascade still works, so this is reported and not fatal.
    match sendto::folder() {
        Ok(dir) => surfaces.push(Box::new(SendTo::new(&dir, helper))),
        Err(e) => eprintln!("[1device-menu] no Send to folder ({e}): that entry is skipped"),
    }
    surfaces
}

/// The label a MENU shows for `name`: trimmed, and with the two characters a
/// displayed shell string reads as syntax made harmless.
///
/// `&` is doubled because a menu label underlines the character after a single one
/// (Git's own entry relies on it), and a leading `@` is dropped because it makes
/// the shell treat the whole value as a resource reference — for which no escape
/// exists, so the alternative would be an entry showing nothing at all. Control
/// characters become spaces: they cannot be displayed, and a NUL would truncate the
/// value where the registry's counted string ends.
///
/// Nothing else is touched. A registry value is a counted string, so unlike a
/// `.desktop` line there is no quoting to get right and no way for a name to become
/// syntax — only these two ways for it to be misread.
pub(crate) fn menu_label(name: &str) -> String {
    let mut label = String::with_capacity(name.len());
    for c in name.trim().trim_start_matches('@').chars() {
        match c {
            '&' => label.push_str("&&"),
            c if c.is_control() => label.push(' '),
            c => label.push(c),
        }
    }
    label.trim().to_string()
}

/// The label a FILE NAME can carry for `name` — the other answer, on the same
/// platform: "Send to" takes each entry's name from its file, so there is nothing
/// to escape and everything to sanitize.
///
/// The characters Win32 forbids in a name become dashes, control characters become
/// spaces, and a trailing dot or space is dropped because the file system silently
/// drops it too: keeping one would mean the name we compute is never the name on
/// disk, and every render would rewrite the shortcut it had just written.
///
/// Deliberately NOT doubling `&` here, unlike [`menu_label`]: this string is a file
/// name first, and it is the shell that decides how to show it. A name is far more
/// likely to be seen in the folder than to hold an ampersand.
pub(crate) fn file_label(name: &str) -> String {
    let mut label = String::with_capacity(name.len());
    for c in name.trim().chars() {
        match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => label.push('-'),
            c if c.is_control() => label.push(' '),
            c => label.push(c),
        }
    }
    label.trim_end_matches(['.', ' ']).trim_start().to_string()
}

/// Quotes one argument of a command line the way `CommandLineToArgvW` reads it
/// back, which is the parser every program the shell starts uses.
///
/// The rules that matter: an argument holding a space or a quote must be wrapped in
/// quotes, an embedded quote is `\"`, and a run of backslashes is only special
/// immediately before a quote — where each of them must be doubled, or the last one
/// escapes the closing quote instead. That last case is not hypothetical: it is
/// every path that ends in a separator.
pub(crate) fn quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"', '\\']) {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => {
                backslashes += 1;
                out.push('\\');
            }
            '"' => {
                // The run before a quote is doubled, then the quote is escaped.
                out.push_str(&"\\".repeat(backslashes));
                backslashes = 0;
                out.push_str("\\\"");
            }
            c => {
                backslashes = 0;
                out.push(c);
            }
        }
    }
    // Same reason, for the closing quote we are about to add ourselves.
    out.push_str(&"\\".repeat(backslashes));
    out.push('"');
    out
}

/// Same, for an argument going into a **verb's command line** — where the shell
/// reads the string first and substitutes its field codes (`%1` is the selection,
/// `%V` the folder, `%*` the lot) before any program sees it.
///
/// So a percent sign we mean literally is doubled. That is the only escape the
/// shell offers here, and it is the escape a `%` in our own installation path
/// needs: without it, `C:\100%\…` could reach `CreateProcess` as a path that does
/// not exist, and every click would fail with nothing to show for it.
///
/// The `"%1"` a surface appends afterwards is the field code itself, and is never
/// passed through this.
pub(crate) fn quote_for_verb(arg: &str) -> String {
    quote(arg).replace('%', "%%")
}

/// The fixed part of a verb's command line for `target`, without the shell's path
/// placeholder: our own program, then the courier's arguments, each quoted.
pub(crate) fn command_prefix(helper: &HelperCommand, target: &crate::surface::Target) -> String {
    let program = helper.program.to_string_lossy().into_owned();
    let mut line = quote_for_verb(&program);
    for arg in helper.args_for(target) {
        line.push(' ');
        line.push_str(&quote_for_verb(&arg));
    }
    line
}

/// Truncates `s` to `max` UTF-16 code units — what Windows counts a file name in —
/// without splitting a character.
pub(crate) fn fit_utf16(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut units = 0;
    for c in s.chars() {
        units += c.len_utf16();
        if units > max {
            break;
        }
        out.push(c);
    }
    out
}

/// Whether the file at `path` is a regular file we may consider (a directory in
/// the `SendTo` folder is a user's own, and never ours).
pub(crate) fn is_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file())
}

/// Removes `path` if it is there. Absent is success: an empty list is applied at
/// every startup, when there is usually nothing left to remove.
pub(crate) fn remove_if_present(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Everything in `dir`, or nothing if it does not exist (which is not a failure:
/// the folder is created by the shell, and an empty list has nothing to prune).
pub(crate) fn entries(dir: &Path) -> io::Result<Vec<PathBuf>> {
    match std::fs::read_dir(dir) {
        Ok(reader) => Ok(reader.filter_map(Result::ok).map(|e| e.path()).collect()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------------------
// The registry, just enough of it.
// ---------------------------------------------------------------------------

/// A `HKEY_CURRENT_USER` subkey, closed when dropped.
///
/// Public for the test suites: the integration one reads back the command line a
/// cascade wrote and hands it to the shell's own parser, and it needs to sweep the
/// root it wrote under. Nothing outside this module uses it in production.
pub mod registry {
    use std::io;

    use windows_sys::Win32::Foundation::{
        ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS,
    };
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
        RegCreateKeyExW, RegDeleteTreeW, RegEnumKeyExW, RegOpenKeyExW, RegQueryValueExW,
        RegSetValueExW,
    };

    /// Longest a registry key name can be, plus its terminator.
    const MAX_KEY_NAME: usize = 256;

    pub struct Key(HKEY);

    // The handle is not tied to a thread: an `apply` runs on whichever blocking
    // thread the applier had free.
    unsafe impl Send for Key {}

    impl Drop for Key {
        fn drop(&mut self) {
            // SAFETY: opened by RegCreateKeyExW/RegOpenKeyExW and never closed
            // twice — `Key` is not `Copy` and owns the handle.
            unsafe { RegCloseKey(self.0) };
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn result(code: u32) -> io::Result<()> {
        if code == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(code as i32))
        }
    }

    impl Key {
        /// Opens `path` under `HKEY_CURRENT_USER`, creating it and its parents.
        pub fn create(path: &str) -> io::Result<Key> {
            let mut handle: HKEY = std::ptr::null_mut();
            // SAFETY: a NUL-terminated path, a valid out pointer, no security
            // attributes (the default DACL of a HKCU key is this user's).
            let code = unsafe {
                RegCreateKeyExW(
                    HKEY_CURRENT_USER,
                    wide(path).as_ptr(),
                    0,
                    std::ptr::null(),
                    REG_OPTION_NON_VOLATILE,
                    KEY_READ | KEY_WRITE,
                    std::ptr::null(),
                    &mut handle,
                    std::ptr::null_mut(),
                )
            };
            result(code)?;
            Ok(Key(handle))
        }

        /// Opens `path` for reading and writing, or `None` if it does not exist.
        pub fn open(path: &str) -> io::Result<Option<Key>> {
            let mut handle: HKEY = std::ptr::null_mut();
            // SAFETY: as above, without creation.
            let code = unsafe {
                RegOpenKeyExW(
                    HKEY_CURRENT_USER,
                    wide(path).as_ptr(),
                    0,
                    KEY_READ | KEY_WRITE,
                    &mut handle,
                )
            };
            match code {
                ERROR_SUCCESS => Ok(Some(Key(handle))),
                ERROR_FILE_NOT_FOUND => Ok(None),
                code => Err(io::Error::from_raw_os_error(code as i32)),
            }
        }

        /// A string value, or `None` if it is absent or not a string. `name` empty
        /// is the key's default value.
        pub fn string(&self, name: &str) -> Option<String> {
            let name = wide(name);
            let mut kind = 0u32;
            let mut len = 0u32;
            // SAFETY: a size query — no buffer is written.
            let code = unsafe {
                RegQueryValueExW(
                    self.0,
                    name.as_ptr(),
                    std::ptr::null(),
                    &mut kind,
                    std::ptr::null_mut(),
                    &mut len,
                )
            };
            if code != ERROR_SUCCESS || kind != REG_SZ || len == 0 {
                return None;
            }
            let mut buf = vec![0u16; (len as usize).div_ceil(2)];
            let mut len = (buf.len() * 2) as u32;
            // SAFETY: `buf` holds `len` bytes, which is what is announced.
            let code = unsafe {
                RegQueryValueExW(
                    self.0,
                    name.as_ptr(),
                    std::ptr::null(),
                    &mut kind,
                    buf.as_mut_ptr() as *mut u8,
                    &mut len,
                )
            };
            if code != ERROR_SUCCESS {
                return None;
            }
            let chars = (len as usize / 2).min(buf.len());
            let value = &buf[..chars];
            // The stored form is NUL-terminated; the value is what precedes it.
            let value = match value.iter().position(|&c| c == 0) {
                Some(end) => &value[..end],
                None => value,
            };
            Some(String::from_utf16_lossy(value))
        }

        /// Sets a string value, unless it already holds exactly that. Returns
        /// whether it had to be written.
        ///
        /// The skip is not an optimization for its own sake: the applier
        /// re-applies the list it already rendered whenever it cannot prove
        /// nothing changed, and a key nobody had to touch is one Explorer cannot
        /// see change under it. The returned flag is what lets a test prove the
        /// skip really happens.
        pub fn set_string(&self, name: &str, value: &str) -> io::Result<bool> {
            if self.string(name).as_deref() == Some(value) {
                return Ok(false);
            }
            let data = wide(value);
            // SAFETY: `data` is a NUL-terminated UTF-16 buffer and the length
            // announced is its size in bytes, terminator included — what REG_SZ
            // expects.
            let code = unsafe {
                RegSetValueExW(
                    self.0,
                    wide(name).as_ptr(),
                    0,
                    REG_SZ,
                    data.as_ptr() as *const u8,
                    (data.len() * 2) as u32,
                )
            };
            result(code)?;
            Ok(true)
        }

        /// The names of this key's immediate subkeys.
        ///
        /// Collected before anything is deleted, on purpose: deleting during an
        /// enumeration shifts the indices of the entries that follow, and half of
        /// them would be skipped.
        pub fn subkeys(&self) -> io::Result<Vec<String>> {
            let mut names = Vec::new();
            let mut index = 0u32;
            loop {
                let mut buf = [0u16; MAX_KEY_NAME];
                let mut len = buf.len() as u32;
                // SAFETY: `buf` holds `len` UTF-16 units, which is what is
                // announced; every other argument is optional and passed null.
                let code = unsafe {
                    RegEnumKeyExW(
                        self.0,
                        index,
                        buf.as_mut_ptr(),
                        &mut len,
                        std::ptr::null(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                };
                match code {
                    ERROR_SUCCESS => {
                        names.push(String::from_utf16_lossy(&buf[..len as usize]));
                        index += 1;
                    }
                    ERROR_NO_MORE_ITEMS => return Ok(names),
                    // A name longer than the ceiling cannot be one of ours, and
                    // must not stop the enumeration either.
                    ERROR_MORE_DATA => index += 1,
                    code => return Err(io::Error::from_raw_os_error(code as i32)),
                }
            }
        }

        /// Deletes `name` and everything under it. Absent is success.
        pub fn delete_subtree(&self, name: &str) -> io::Result<()> {
            // SAFETY: a NUL-terminated subkey name under a live handle.
            let code = unsafe { RegDeleteTreeW(self.0, wide(name).as_ptr()) };
            match code {
                ERROR_SUCCESS | ERROR_FILE_NOT_FOUND => Ok(()),
                code => Err(io::Error::from_raw_os_error(code as i32)),
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::surface::Target;

    /// A registry subtree of this test's own, removed when it goes out of scope.
    /// The surfaces write to a real registry — that is the point — but never to the
    /// developer's live menus.
    pub(crate) struct TestRoot {
        parent: String,
        name: String,
        classes: String,
    }

    impl TestRoot {
        pub(crate) fn new(tag: &str) -> TestRoot {
            let parent = r"Software\1Device-menu-tests".to_string();
            // One process per test under nextest, several threads under plain
            // `cargo test`: the tag keeps them apart either way.
            let name = format!("{tag}-{}", std::process::id());
            let classes = format!(r"{parent}\{name}\Classes");
            TestRoot {
                parent,
                name,
                classes,
            }
        }

        /// Stands in for `Software\Classes`.
        pub(crate) fn classes(&self) -> &str {
            &self.classes
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            if let Ok(Some(parent)) = registry::Key::open(&self.parent) {
                let _ = parent.delete_subtree(&self.name);
            }
        }
    }

    pub(crate) fn helper() -> HelperCommand {
        HelperCommand {
            program: PathBuf::from(r"C:\Program Files\UL\1device-menu.exe"),
            extra_args: vec![],
        }
    }

    pub(crate) fn target(device_id: &str, name: &str) -> Target {
        Target {
            device_id: device_id.into(),
            name: name.into(),
            platform: "windows".into(),
        }
    }

    /// Splits a command line with the REAL parser every program the shell starts
    /// uses. The strongest oracle available for the quoting: not a second
    /// implementation of the rules, but the implementation.
    pub(crate) fn parse_command_line(line: &str) -> Vec<String> {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::UI::Shell::CommandLineToArgvW;

        let wide: Vec<u16> = line.encode_utf16().chain(std::iter::once(0)).collect();
        let mut count = 0i32;
        // SAFETY: a NUL-terminated command line and a valid out pointer; the
        // returned array is freed below, as the API requires.
        unsafe {
            let argv = CommandLineToArgvW(wide.as_ptr(), &mut count);
            assert!(!argv.is_null(), "CommandLineToArgvW refused: {line}");
            let mut out = Vec::with_capacity(count as usize);
            for i in 0..count as usize {
                let arg = *argv.add(i);
                let mut end = arg;
                while *end != 0 {
                    end = end.add(1);
                }
                out.push(String::from_utf16_lossy(std::slice::from_raw_parts(
                    arg,
                    end.offset_from(arg) as usize,
                )));
            }
            LocalFree(argv as *mut std::ffi::c_void);
            out
        }
    }

    /// A menu label is a displayed shell string: the two characters it reads as
    /// syntax have to stop being syntax, and the rest must survive untouched.
    #[test]
    fn a_menu_label_keeps_its_meaning_when_the_shell_reads_it() {
        assert_eq!(menu_label("  PC A \t"), "PC A");
        // A single ampersand would underline the next character and vanish.
        assert_eq!(menu_label("Iwan & Co"), "Iwan && Co");
        assert_eq!(menu_label("A&&B"), "A&&&&B");
        // A leading @ makes the whole value a resource reference, and there is no
        // escape for it.
        assert_eq!(menu_label("@shell32.dll,-1"), "shell32.dll,-1");
        assert_eq!(menu_label("mail@host"), "mail@host");
        // A NUL would end the counted string early; a newline cannot be shown.
        assert_eq!(menu_label("PC\0A"), "PC A");
        assert_eq!(menu_label("PC\nA"), "PC A");
        // Everything else is the user's name and stays exactly as it is.
        assert_eq!(menu_label("Bureau d'Iwan — 100%"), "Bureau d'Iwan — 100%");
    }

    /// A file name, on the other hand, has characters the file system refuses and
    /// characters it silently drops — and the second kind is the dangerous one: a
    /// name we can never read back is a shortcut rewritten on every render.
    #[test]
    fn a_file_label_is_a_name_windows_will_really_give_the_file() {
        assert_eq!(file_label("  PC A \t"), "PC A");
        assert_eq!(file_label(r"C:/PC?A|B*"), "C--PC-A-B-");
        assert_eq!(file_label("PC\0A"), "PC A");
        // Win32 drops these, so we drop them first.
        assert_eq!(file_label("PC A..."), "PC A");
        assert_eq!(file_label("PC A. . "), "PC A");
        // An ampersand is left alone: this is a name, not a menu string.
        assert_eq!(file_label("Iwan & Co"), "Iwan & Co");
    }

    /// The rules `CommandLineToArgvW` applies, read backwards: what we write must
    /// parse back to the argument we meant. Proven against the real parser too, in
    /// the round-trip tests below.
    #[test]
    fn arguments_are_quoted_the_way_the_parser_reads_them() {
        assert_eq!(quote("--send"), "--send");
        assert_eq!(
            quote(r"C:\Program Files\ul.exe"),
            r#""C:\Program Files\ul.exe""#
        );
        // A trailing separator is the case that bites: without doubling, its
        // backslash would escape the closing quote and swallow the next argument.
        assert_eq!(quote(r"C:\dir\"), r#""C:\dir\\""#);
        assert_eq!(quote(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(quote(""), r#""""#);
        // A percent is left alone in a shortcut's arguments, and doubled in a
        // verb's command line — where the shell reads it before anyone else.
        assert_eq!(quote("100%"), "100%");
        assert_eq!(quote_for_verb("100%"), "100%%");
        assert_eq!(quote_for_verb(r"C:\100%\a b"), r#""C:\100%%\a b""#);
    }

    /// The real parser, on the real quoting: whatever we write, argv must come back
    /// as the list we meant. The cases that break naive quoting are the trailing
    /// separator and the embedded quote.
    #[test]
    fn every_argument_survives_a_round_trip_through_the_real_parser() {
        for arg in [
            r"C:\Program Files\UL\1device-menu.exe",
            r"C:\dir\",
            r"C:\a b\c\\",
            r#"say "hi""#,
            "--send",
            "d_3a4424d810ba6c27",
            "--",
            "a b",
        ] {
            // argv[0] is parsed by different rules, so the argument under test is
            // never in that position.
            let line = format!("prog {}", quote(arg));
            assert_eq!(
                parse_command_line(&line),
                ["prog", arg],
                "{arg:?} did not come back as itself from {line:?}"
            );
        }
    }

    /// A file name is counted in UTF-16 units by Windows, and a cut must not land
    /// inside a character.
    #[test]
    fn a_long_label_is_cut_on_a_character_boundary() {
        assert_eq!(fit_utf16("abcdef", 3), "abc");
        assert_eq!(fit_utf16("abc", 10), "abc");
        // One unit each, so here the count is the character count.
        assert_eq!(fit_utf16("ééé", 2), "éé");
        // Two units each: a cut can never land between the halves of one.
        assert_eq!(fit_utf16("𝄞𝄞", 3), "𝄞");
        // And what is promised is a CHARACTER boundary, not a grapheme one: a flag
        // is two regional indicators of two units each, so a four-unit budget keeps
        // one whole flag and a three-unit budget keeps half of it. Ugly, and still a
        // valid file name — which is all a counted budget can promise without a
        // segmentation library.
        assert_eq!(fit_utf16("🇫🇷🇫🇷", 4), "🇫🇷");
        assert_eq!(fit_utf16("🇫🇷🇫🇷", 3), "🇫");
    }

    /// And the whole prefix, read back by the real parser: what a click will
    /// actually receive as its argv.
    #[test]
    fn the_command_prefix_names_our_program_then_the_courier_arguments() {
        let line = command_prefix(&helper(), &target("d_1", "PC"));
        assert_eq!(
            line,
            r#""C:\Program Files\UL\1device-menu.exe" --send d_1 --"#
        );
        assert_eq!(
            parse_command_line(&line),
            [
                r"C:\Program Files\UL\1device-menu.exe",
                "--send",
                "d_1",
                "--"
            ]
        );
    }

    /// The channel override a live test injects goes through the same quoting, and
    /// a pipe name is nothing but backslashes.
    #[test]
    fn an_injected_channel_survives_the_command_line() {
        let helper = HelperCommand {
            program: PathBuf::from(r"C:\ul\menu.exe"),
            extra_args: vec!["--channel".into(), r"\\.\pipe\1device-menu-test".into()],
        };
        let line = command_prefix(&helper, &target("d_1", "PC"));
        assert_eq!(
            parse_command_line(&line),
            [
                r"C:\ul\menu.exe",
                "--channel",
                r"\\.\pipe\1device-menu-test",
                "--send",
                "d_1",
                "--"
            ]
        );
    }
}
