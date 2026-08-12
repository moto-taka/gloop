#![cfg(unix)]

use std::fmt::Write as _;
use std::path::PathBuf;

use gloop_core::{FinalStatus, Graph, Node, NodeKind, NodeStatus, RunEventKind};
use gloop_provider::{CommandProfile, Profile, ProfileKind, ProfileStore, ProviderRegistry};
use gloop_runtime::{NodeFailureClass, RunOptions, Runtime, node_failure_class, read_events};
use tempfile::tempdir;

#[tokio::test]
async fn command_profile_timeout_without_rebind_is_not_retried() {
    let temp = tempdir().expect("temp");

    let marker = temp.path().join("marker.log");
    let marker_for_profile = marker.to_string_lossy().into_owned();

    let runtime = build_runtime(temp.path().join("runs"), &marker_for_profile, true, false);

    let mut node = Node::agent("retryable", "run command provider");
    if let NodeKind::Agent { profile, .. } = &mut node.kind {
        *profile = Some("timeout".to_owned());
    }
    node.retry.max_attempts = 2;
    node.retry.backoff_seconds = 0;

    let graph = Graph::new(
        "command-provider-retry",
        "without explicit rebind",
        vec![node],
    );

    let options = RunOptions {
        run_id: Some("without-rebind".to_owned()),
        ..RunOptions::default()
    };
    let summary = runtime.run(&graph, options).await.expect("run graph");

    let log = tokio::fs::read_to_string(&marker).await.unwrap_or_default();

    assert_eq!(summary.status, FinalStatus::Failed);
    assert_eq!(summary.nodes["retryable"].status, NodeStatus::Failed);
    assert_eq!(summary.nodes["retryable"].attempts, 1);
    assert_eq!(
        node_failure_class(&summary.nodes["retryable"]),
        Some(NodeFailureClass::ProviderTimeout)
    );
    assert!(summary.nodes["retryable"].profile.is_none());
    assert_eq!(log, "timeout\n");
}

#[tokio::test]
async fn command_profile_timeout_with_rebind_does_not_duplicate_an_uncertain_attempt() {
    let temp = tempdir().expect("temp");

    let marker = temp.path().join("marker.log");
    let marker_for_profile = marker.to_string_lossy().into_owned();

    let runtime = build_runtime(temp.path().join("runs"), &marker_for_profile, true, true);

    let mut node = Node::agent("retryable", "run command provider");
    if let NodeKind::Agent { profile, .. } = &mut node.kind {
        *profile = Some("timeout".to_owned());
    }
    node.retry.max_attempts = 2;
    node.retry.backoff_seconds = 0;
    node.retry.rebind_profiles = vec!["safe".to_owned()];

    let graph = Graph::new("command-provider-retry", "with explicit rebind", vec![node]);
    let options = RunOptions {
        run_id: Some("with-rebind".to_owned()),
        ..RunOptions::default()
    };
    let summary = runtime.run(&graph, options).await.expect("run graph");

    let log = tokio::fs::read_to_string(&marker).await.unwrap_or_default();

    assert_eq!(summary.status, FinalStatus::Failed);
    assert_eq!(summary.nodes["retryable"].status, NodeStatus::Failed);
    assert_eq!(summary.nodes["retryable"].attempts, 1);
    assert_eq!(summary.nodes["retryable"].profile.as_deref(), None);
    assert_eq!(log, "timeout\n");

    let events = read_events(
        temp.path()
            .join("runs")
            .join("with-rebind")
            .join("journal.jsonl"),
    )
    .await
    .expect("read events");
    let retry_scheduled = events.into_iter().find(|event| {
        event.kind == RunEventKind::RetryScheduled
            && event.node_id.as_deref() == Some("retryable")
            && event.attempt == Some(2)
    });
    assert!(
        retry_scheduled.is_none(),
        "uncertain provider attempts must never be scheduled again"
    );
}

fn build_runtime(
    run_root: impl Into<PathBuf>,
    marker: &str,
    marker_on_timeout: bool,
    include_safe: bool,
) -> Runtime {
    let timeout_profile =
        build_profile("timeout", "timeout-profile 1.0", marker, marker_on_timeout);
    let mut timeout_profile = timeout_profile;
    timeout_profile.priority = 10;
    timeout_profile.timeout_seconds = Some(1);

    let mut store = ProfileStore::default();
    store
        .insert("timeout", timeout_profile)
        .expect("install timeout profile");

    if include_safe {
        let safe_profile = build_profile("safe", "safe-profile 1.0", marker, false);
        store
            .insert("safe", safe_profile)
            .expect("install safe profile");
    }

    Runtime::new(ProviderRegistry::new(store), run_root)
}

fn build_profile(name: &str, version: &str, marker: &str, timeout: bool) -> Profile {
    let mut script = String::new();
    script.push_str("if [ \"$1\" = --version ]; then\n");
    let _ = writeln!(&mut script, "  echo {}", sh_lit(version));
    script.push_str("  exit 0\nfi\n");
    let _ = writeln!(
        &mut script,
        "printf '%s\\n' {} >> {}",
        sh_lit(name),
        sh_quote(marker)
    );
    script.push('\n');
    if timeout {
        script.push_str("while :; do :; done\n");
    } else {
        script.push_str("printf \'ok\\n\'\n");
    }

    Profile {
        enabled: true,
        priority: 0,
        timeout_seconds: None,
        capabilities: gloop_provider::AdapterCapabilities::text(),
        kind: ProfileKind::Command(CommandProfile {
            version_args: vec!["-c".to_owned(), "exit 0".to_owned()],
            argv: vec!["sh".to_owned(), "-c".to_owned(), script],
            ..CommandProfile::new(vec!["noop".to_owned()])
        }),
    }
}

fn sh_quote(input: &str) -> String {
    format!("\"{}\"", input.replace('"', "\\\""))
}

fn sh_lit(input: &str) -> String {
    format!("'{}'", input.replace('\'', "'\\''"))
}
