// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The supervisor: launch, restart, stop — and leave nothing behind.

use std::path::PathBuf;
use std::time::Duration;

use crate::support::*;

#[tokio::test]
async fn a_spawned_component_introduces_itself_with_the_token_from_its_stdin() {
    let daemon = TestDaemon::start().await;
    let supervisor = daemon.supervise(spec(&daemon, "stay", &[]));

    let journal = daemon.wait_lines("hello ok", 1).await;
    let hello = journal.iter().find(|l| l.starts_with("hello ok")).unwrap();
    assert_eq!(
        hello, "hello ok session.read,devices.read",
        "the spawn token grants EXACTLY the spec's scopes, no more, no less"
    );

    supervisor.shutdown().await;
}

#[tokio::test]
async fn a_component_that_dies_is_restarted_with_a_fresh_token() {
    // The spawn token is single-use. If the supervisor replayed the one from
    // the previous launch, this second `hello` would answer INVALID_TOKEN.
    let daemon = TestDaemon::start().await;
    let supervisor = daemon.supervise(spec(&daemon, "exit", &[("exit", "3")]));

    let journal = daemon.wait_lines("hello ok", 2).await;
    assert!(
        !journal.iter().any(|line| line.starts_with("hello err")),
        "a restart was refused (token replayed?): {journal:?}"
    );

    supervisor.shutdown().await;
}

#[tokio::test]
async fn the_token_of_a_child_that_died_before_saying_hello_is_taken_back() {
    // Without reclamation, each restart would leave behind an activation token
    // alive until the Core stops.
    let daemon = TestDaemon::start().await;
    let supervisor = daemon.supervise(spec(&daemon, "leak", &[]));

    // Two launches: seeing the second proves the first was reaped, hence that
    // its token is already reclaimed.
    let journal = daemon.wait_lines("token ", 2).await;
    let orphan = journal
        .iter()
        .find_map(|l| l.strip_prefix("token "))
        .expect("token disclosed by the fixture")
        .to_string();

    let code = hello_with(&daemon.ipc_path(), &orphan).await;
    assert_eq!(
        code.as_deref(),
        Some("INVALID_TOKEN"),
        "the token of a child that died without introducing itself still opens the Core"
    );

    supervisor.shutdown().await;
}

#[tokio::test]
async fn shutting_down_closes_the_child_stdin_and_the_child_leaves_on_its_own() {
    // EOF on standard input is the only graceful-shutdown channel that exists
    // on all three OSes. Seeing it taken ("bye") proves we did not go through
    // force.
    let daemon = TestDaemon::start().await;
    let supervisor = daemon.supervise(spec(&daemon, "stay", &[]));
    daemon.wait_lines("hello ok", 1).await;
    daemon.wait_beating("tray").await;

    supervisor.shutdown().await;

    assert!(
        daemon.journal().iter().any(|line| line == "bye"),
        "the child did not see its standard input close: {:?}",
        daemon.journal()
    );
    daemon.assert_dead("tray").await;
}

#[tokio::test]
async fn the_descendants_of_a_component_die_with_it() {
    // A contextual-menu backend spawns shims. Leaving them alive after the
    // Core stops would be a process leak — and, for the shims, an OS
    // integration that answers into the void.
    let daemon = TestDaemon::start().await;
    let supervisor = daemon.supervise(spec(&daemon, "stay", &[("child", "1")]));
    daemon.wait_lines("hello ok", 1).await;
    daemon.wait_beating("grandchild").await;

    supervisor.shutdown().await;

    daemon.assert_dead("grandchild").await;
}

#[tokio::test]
async fn nothing_is_restarted_after_the_shutdown() {
    let daemon = TestDaemon::start().await;
    let supervisor = daemon.supervise(spec(&daemon, "exit", &[]));
    daemon.wait_lines("hello", 1).await;

    supervisor.shutdown().await;
    let after_shutdown = daemon.journal().len();

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        daemon.journal().len(),
        after_shutdown,
        "a component was restarted after the shutdown"
    );
}

#[tokio::test]
async fn the_spawn_token_never_reaches_the_child_environment() {
    // Threat-model invariant frozen by the harness: the token goes through
    // stdin, never through argv (readable by all) nor the environment
    // (inherited by all descendants). It is the child itself that attests what
    // it received — a regression that moved the token to the env would say so.
    let daemon = TestDaemon::start().await;
    let supervisor = daemon.supervise(spec(&daemon, "stay", &[]));

    let journal = daemon.wait_lines("leak ", 1).await;
    let line = journal.iter().find(|l| l.starts_with("leak ")).unwrap();
    assert_eq!(
        line, "leak env=false argv=false",
        "the spawn token leaked out of standard input"
    );

    supervisor.shutdown().await;
}

#[tokio::test]
async fn a_missing_executable_does_not_bring_the_supervisor_down() {
    // A missing official component (partial installation): the Core must keep
    // running, and the shutdown stay clean.
    let daemon = TestDaemon::start().await;
    let mut spec = spec(&daemon, "stay", &[]);
    spec.program = PathBuf::from("/this/binary/does/not/exist");
    let supervisor = daemon.supervise(spec);

    // A launch failure (and the token reclamation that follows) must not
    // poison the Core: it still answers, registry intact.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let token = daemon.core.mint_spawn_token("tray", &["session.read"]);
    assert_eq!(
        hello_with(&daemon.ipc_path(), &token).await,
        None,
        "the Core no longer responds after a component's launch failure"
    );

    tokio::time::timeout(DEADLINE, supervisor.shutdown())
        .await
        .expect("shutdown must not hang on a child that never existed");
}
