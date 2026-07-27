// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The seam between the OS-agnostic orchestrator and a platform menu surface,
//! plus the target list the surfaces render.
//!
//! Unlike the clipboard's seam, this one is **downcall-only**: a click never
//! comes back through the surface. Every family-A surface (see
//! doc/architecture.md, "Two families of backends") registers a *command line*,
//! so the OS answers a click by starting a fresh process — the `--send` helper —
//! which reaches the manager over the local channel. The surface therefore has
//! exactly one job: make the OS show exactly this list of targets, and nothing
//! else.

use std::path::PathBuf;

/// A device a menu entry can send to. Only what a surface needs: the label to
/// show, and the id to bake into the command line. The two are deliberately
/// distinct — a rename must not break an already-written entry's meaning, and a
/// label is not an identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub device_id: String,
    pub name: String,
    /// `linux` / `macos` / `windows`. Not used for filtering here (the
    /// orchestrator already did that) — surfaces may use it for an icon.
    pub platform: String,
}

/// How a surface must spell "invoke the click helper". Held by construction so
/// that a live test can point generated artifacts at its own channel, while
/// production emits the plain form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelperCommand {
    /// Absolute path to our own executable — an entry written into the registry
    /// or a `.desktop` file outlives our process, so it cannot rely on `PATH`.
    pub program: PathBuf,
    /// Inserted before `--send`. Empty in production; a test passes
    /// `--channel <path>` here.
    pub extra_args: Vec<String>,
}

impl HelperCommand {
    /// The fixed part of the command line for `target`, in order, WITHOUT the
    /// selected paths. Every surface must append its platform's path
    /// placeholder after these — the trailing `--` is what makes a file named
    /// `-r` a path and not a flag.
    pub fn args_for(&self, target: &Target) -> Vec<String> {
        let mut args = self.extra_args.clone();
        args.push("--send".into());
        args.push(target.device_id.clone());
        args.push("--".into());
        args
    }
}

/// A platform menu surface: `HKCU\…\shell` on Windows, a KDE ServiceMenu or a
/// Nautilus script on Linux, a Quick Action on macOS.
///
/// `apply` is BLOCKING (it writes files or registry keys) and is called from a
/// blocking thread pool, never on the async reactor. It is also **absolute, not
/// incremental**: after it returns, the surface must show exactly `targets` —
/// so `apply(&[])` is how every entry is removed, and it is what runs both at
/// startup (stale artifacts from a previous run) and at graceful shutdown (no
/// manager, no entry: the fail-closed rule of doc/architecture.md).
pub trait MenuSurface: Send + 'static {
    /// For logs. Several surfaces coexist per OS, and a failure must name one.
    fn name(&self) -> &'static str;

    /// Make the OS show exactly `targets`. Must be idempotent: the orchestrator
    /// re-applies the same list whenever it cannot prove nothing changed.
    fn apply(&mut self, targets: &[Target]) -> std::io::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(id: &str) -> Target {
        Target {
            device_id: id.into(),
            name: "PC".into(),
            platform: "linux".into(),
        }
    }

    #[test]
    fn helper_args_carry_the_id_and_end_with_a_separator() {
        let helper = HelperCommand {
            program: PathBuf::from("/opt/universallink-menu"),
            extra_args: vec![],
        };
        assert_eq!(helper.args_for(&target("d_1")), ["--send", "d_1", "--"]);
    }

    #[test]
    fn helper_args_keep_the_injected_prefix_first() {
        // A live test points the generated artifacts at its own channel; the
        // override has to precede the mode so parsing stays positional-free.
        let helper = HelperCommand {
            program: PathBuf::from("/opt/universallink-menu"),
            extra_args: vec!["--channel".into(), "/tmp/t.sock".into()],
        };
        assert_eq!(
            helper.args_for(&target("d_2")),
            ["--channel", "/tmp/t.sock", "--send", "d_2", "--"]
        );
    }
}
