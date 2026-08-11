use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use gloop_core::{
    ContextSpec, Graph, Node, NodeKind, NodeStatus, OutputFormat, RetryPolicy, RunEventKind,
    RunSummary, WorkspaceSpec, graph,
};
use gloop_provider::{
    AdapterCapabilities, AdapterError, AdapterOutput, AdapterRequest, AdapterResponse, ModelOrigin,
    SelectionOrigin, TokenUsage,
};
use gloop_runtime::{
    ProviderInvocation, ProviderInvoker, ReplayError, RunOptions, Runtime, inspect_run, read_events,
};
use serde_json::json;
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::os::unix::fs::symlink;
use tempfile::tempdir;
use tokio::{fs, fs::OpenOptions, sync::Notify};
use tokio_util::sync::CancellationToken;

const MAX_SUMMARY_BYTES: usize = 128 * 1024 * 1024;
const MAX_GRAPH_BYTES: usize = 64 * 1024 * 1024;
const MAX_ARTIFACT_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug)]
struct TestInvoker {
    called: AtomicUsize,
    started: Notify,
}

impl TestInvoker {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            called: AtomicUsize::new(0),
            started: Notify::new(),
        })
    }
}

#[async_trait]
impl ProviderInvoker for TestInvoker {
    async fn execute(
        &self,
        preferred_profile: Option<&str>,
        _required: &AdapterCapabilities,
        _request: AdapterRequest,
        _cancellation: CancellationToken,
    ) -> Result<ProviderInvocation, AdapterError> {
        self.called.fetch_add(1, Ordering::SeqCst);
        self.started.notify_waiters();
        let _ = preferred_profile;

        Ok(ProviderInvocation {
            profile: "test".to_owned(),
            selected_model: Some("model".to_owned()),
            selection_origin: SelectionOrigin::Explicit,
            model_origin: ModelOrigin::ProviderDefault,
            response: AdapterResponse {
                output: AdapterOutput::Json(json!({"value": 1})),
                stdout: String::new(),
                stderr: String::new(),
                exit_code: Some(0),
                reported_model: Some("model".to_owned()),
                usage: Some(TokenUsage::default()),
            },
        })
    }
}

#[derive(Debug)]
struct RetryInvoker {
    calls: AtomicUsize,
}

impl RetryInvoker {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl ProviderInvoker for RetryInvoker {
    async fn execute(
        &self,
        _preferred_profile: Option<&str>,
        _required: &AdapterCapabilities,
        _request: AdapterRequest,
        _cancellation: CancellationToken,
    ) -> Result<ProviderInvocation, AdapterError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(AdapterError::HttpStatus {
                profile: "test".to_owned(),
                status: 429,
                error_type: Some("rate_limit".to_owned()),
                error_code: None,
            });
        }
        Ok(ProviderInvocation {
            profile: "test".to_owned(),
            selected_model: Some("model".to_owned()),
            selection_origin: SelectionOrigin::Explicit,
            model_origin: ModelOrigin::ProviderDefault,
            response: AdapterResponse {
                output: AdapterOutput::Text("retry succeeded".to_owned()),
                stdout: String::new(),
                stderr: String::new(),
                exit_code: Some(0),
                reported_model: Some("model".to_owned()),
                usage: Some(TokenUsage::default()),
            },
        })
    }
}

fn runtime_with(invoker: Arc<TestInvoker>, temp: &tempfile::TempDir) -> (Runtime, RunOptions) {
    let runtime = Runtime::from_invoker(invoker, temp.path().join("runs"));
    let options = RunOptions {
        current_dir: temp.path().to_path_buf(),
        ..RunOptions::default()
    };
    (runtime, options)
}

fn json_output(node: &mut Node) {
    if let NodeKind::Agent { output, .. } = &mut node.kind {
        output.format = OutputFormat::Json;
    }
}

#[tokio::test]
async fn inspect_rejects_terminal_state_mismatch_between_summary_and_replay() {
    let temp = tempdir().expect("temp");
    let mut node = Node::agent("agent", "agent");
    json_output(&mut node);
    let graph = Graph::new("mismatch", "mismatch", vec![node]);

    let invoker = TestInvoker::new();
    let (runtime, mut options) = runtime_with(Arc::clone(&invoker), &temp);
    options.run_id = Some("integrity_mismatch".to_owned());
    let summary = runtime.run(&graph, options).await.expect("run");

    let root = temp.path().join("runs").join("integrity_mismatch");
    let mut tampered = summary;
    tampered.nodes.get_mut("agent").expect("node").status = NodeStatus::Failed;

    fs::write(
        root.join("summary.json"),
        serde_json::to_vec_pretty(&tampered).expect("serialize"),
    )
    .await
    .expect("rewrite summary");

    let error = inspect_run(&root)
        .await
        .expect_err("terminal mismatch should fail");
    assert!(matches!(
        error,
        ReplayError::SummaryReplayNodeMismatch { .. }
    ));
}

#[tokio::test]
async fn inspect_rejects_unsupported_summary_schema_version() {
    let temp = tempdir().expect("temp");
    let mut node = Node::agent("agent", "agent");
    json_output(&mut node);
    let graph = Graph::new("schema_version", "schema version", vec![node]);

    let invoker = TestInvoker::new();
    let (runtime, mut options) = runtime_with(Arc::clone(&invoker), &temp);
    options.run_id = Some("integrity_schema_version".to_owned());
    let mut summary = runtime.run(&graph, options).await.expect("run");

    let root = temp.path().join("runs").join("integrity_schema_version");
    summary.schema_version = "gloop.run-summary/v0beta".to_owned();
    fs::write(
        root.join("summary.json"),
        serde_json::to_vec_pretty(&summary).expect("serialize"),
    )
    .await
    .expect("rewrite");

    let error = inspect_run(&root)
        .await
        .expect_err("unsupported version should fail");
    assert!(matches!(
        error,
        ReplayError::UnsupportedSummarySchemaVersion { .. }
    ));
}

#[tokio::test]
async fn inspect_rejects_summary_terminal_attempt_error_and_profile_mismatch() {
    let temp = tempdir().expect("temp");
    let mut node = Node::agent("agent", "agent");
    json_output(&mut node);
    let graph = Graph::new("attempts", "attempts", vec![node]);

    let invoker = TestInvoker::new();
    let (runtime, mut options) = runtime_with(Arc::clone(&invoker), &temp);
    options.run_id = Some("integrity_terminal_fields".to_owned());
    let summary = runtime.run(&graph, options).await.expect("run");

    let root = temp.path().join("runs").join("integrity_terminal_fields");
    let agent_attempts = summary
        .nodes
        .get("agent")
        .expect("summary includes agent node");

    let mut tampered = summary.clone();
    tampered
        .nodes
        .get_mut("agent")
        .expect("summary includes agent node")
        .attempts = agent_attempts.attempts + 1;
    fs::write(
        root.join("summary.json"),
        serde_json::to_vec_pretty(&tampered).expect("serialize"),
    )
    .await
    .expect("rewrite");
    assert!(matches!(
        inspect_run(&root)
            .await
            .expect_err("tampered attempts should fail"),
        ReplayError::SummaryReplayNodeFieldMismatch {
            field: "attempts",
            ..
        }
    ));

    let mut tampered = summary.clone();
    tampered
        .nodes
        .get_mut("agent")
        .expect("summary includes agent node")
        .error = Some("tampered error".to_owned());
    fs::write(
        root.join("summary.json"),
        serde_json::to_vec_pretty(&tampered).expect("serialize"),
    )
    .await
    .expect("rewrite");
    assert!(matches!(
        inspect_run(&root)
            .await
            .expect_err("tampered error should fail"),
        ReplayError::SummaryReplayNodeFieldMismatch { field: "error", .. }
    ));

    let mut tampered = summary.clone();
    tampered
        .nodes
        .get_mut("agent")
        .expect("summary includes agent node")
        .profile = Some("tampered_profile".to_owned());
    fs::write(
        root.join("summary.json"),
        serde_json::to_vec_pretty(&tampered).expect("serialize"),
    )
    .await
    .expect("rewrite");
    assert!(matches!(
        inspect_run(&root)
            .await
            .expect_err("tampered profile should fail"),
        ReplayError::SummaryReplayNodeFieldMismatch {
            field: "profile",
            ..
        }
    ));
}

#[tokio::test]
async fn inspect_rejects_summary_terminal_model_workspace_output_mismatch() {
    let temp = tempdir().expect("temp");
    let mut node = Node::agent("agent", "agent");
    json_output(&mut node);
    let graph = Graph::new("terminal_fields_2", "terminal fields", vec![node]);

    let invoker = TestInvoker::new();
    let (runtime, mut options) = runtime_with(Arc::clone(&invoker), &temp);
    options.run_id = Some("integrity_terminal_fields_2".to_owned());
    let summary = runtime.run(&graph, options).await.expect("run");

    let root = temp.path().join("runs").join("integrity_terminal_fields_2");

    let mut tampered = summary.clone();
    tampered
        .nodes
        .get_mut("agent")
        .expect("summary includes agent node")
        .model = Some("tampered-model".to_owned());
    fs::write(
        root.join("summary.json"),
        serde_json::to_vec_pretty(&tampered).expect("serialize"),
    )
    .await
    .expect("rewrite");
    assert!(matches!(
        inspect_run(&root)
            .await
            .expect_err("tampered model should fail"),
        ReplayError::SummaryReplayNodeFieldMismatch { field: "model", .. }
    ));

    let mut tampered = summary.clone();
    tampered
        .nodes
        .get_mut("agent")
        .expect("summary includes agent node")
        .workspace = Some("tampered-workspace".to_owned());
    fs::write(
        root.join("summary.json"),
        serde_json::to_vec_pretty(&tampered).expect("serialize"),
    )
    .await
    .expect("rewrite");
    assert!(matches!(
        inspect_run(&root)
            .await
            .expect_err("tampered workspace should fail"),
        ReplayError::SummaryReplayNodeFieldMismatch {
            field: "workspace",
            ..
        }
    ));

    let mut tampered = summary.clone();
    tampered
        .nodes
        .get_mut("agent")
        .expect("summary includes agent node")
        .output = Some(json!({"output": "tampered"}));
    fs::write(
        root.join("summary.json"),
        serde_json::to_vec_pretty(&tampered).expect("serialize"),
    )
    .await
    .expect("rewrite");
    assert!(matches!(
        inspect_run(&root)
            .await
            .expect_err("tampered output should fail"),
        ReplayError::SummaryReplayNodeFieldMismatch {
            field: "output",
            ..
        }
    ));
}

#[tokio::test]
async fn inspect_rejects_artifact_path_traversal_outside_run() {
    let temp = tempdir().expect("temp");
    let mut node = Node::agent("agent", "agent");
    json_output(&mut node);
    let graph = Graph::new("traversal", "traversal", vec![node]);

    let invoker = TestInvoker::new();
    let (runtime, mut options) = runtime_with(Arc::clone(&invoker), &temp);
    options.run_id = Some("integrity_traversal".to_owned());
    let mut summary = runtime.run(&graph, options).await.expect("run");
    let root = temp.path().join("runs").join("integrity_traversal");

    summary.artifacts.push(gloop_core::state::ArtifactRef {
        kind: "malicious".to_owned(),
        path: "../outside.json".to_owned(),
        size: Some(0),
        sha256: Some(hex::encode(Sha256::digest([]))),
    });
    fs::write(
        root.join("summary.json"),
        serde_json::to_vec_pretty(&summary).expect("serialize"),
    )
    .await
    .expect("rewrite summary");

    let error = inspect_run(&root).await.expect_err("bad path should fail");
    assert!(matches!(
        error,
        ReplayError::InvalidArtifactPath { .. }
            | ReplayError::ArtifactOutsideRun { .. }
            | ReplayError::SymlinkPath { .. }
    ));
}

#[tokio::test]
async fn inspect_rejects_artifact_hash_tamper() {
    let temp = tempdir().expect("temp");
    let mut node = Node::agent("agent", "agent");
    json_output(&mut node);
    let graph = Graph::new("hash", "hash", vec![node]);

    let invoker = TestInvoker::new();
    let (runtime, mut options) = runtime_with(Arc::clone(&invoker), &temp);
    options.run_id = Some("integrity_hash".to_owned());
    let _ = runtime.run(&graph, options).await.expect("run");

    let root = temp.path().join("runs").join("integrity_hash");
    let events = read_events(root.join("journal.jsonl"))
        .await
        .expect("read events");
    let mut artifact_path = String::new();
    for event in events {
        if event.kind == RunEventKind::NodeOutput
            && let Some(value) = event
                .data
                .get("output_artifact")
                .and_then(|value| value.as_str())
        {
            artifact_path = value.to_owned();
            break;
        }
    }
    assert!(!artifact_path.is_empty(), "found output artifact path");

    let absolute = root.join(&artifact_path);
    let mut bytes = fs::read(&absolute).await.expect("read artifact");
    if !bytes.is_empty() {
        let index = bytes.len() / 2;
        bytes[index] ^= 0xFF;
        fs::write(&absolute, &bytes)
            .await
            .expect("rewrite artifact");
    }

    let error = inspect_run(&root)
        .await
        .expect_err("hash mismatch should fail");
    assert!(matches!(error, ReplayError::ArtifactHashMismatch { .. }));
}

#[tokio::test]
async fn inspect_rejects_artifact_without_size_or_hash() {
    let temp = tempdir().expect("temp");
    let mut node = Node::agent("agent", "agent");
    json_output(&mut node);
    let graph = Graph::new("missing_metadata", "missing metadata", vec![node]);

    let invoker = TestInvoker::new();
    let (runtime, mut options) = runtime_with(Arc::clone(&invoker), &temp);
    options.run_id = Some("integrity_missing_artifact_metadata".to_owned());
    let summary = runtime.run(&graph, options).await.expect("run");

    let root = temp
        .path()
        .join("runs")
        .join("integrity_missing_artifact_metadata");
    let mut tampered = summary;
    let artifact = tampered
        .artifacts
        .iter_mut()
        .find(|artifact| artifact.kind != "graph")
        .expect("non-graph artifact");
    artifact.size = None;
    artifact.sha256 = None;
    fs::write(
        root.join("summary.json"),
        serde_json::to_vec_pretty(&tampered).expect("serialize"),
    )
    .await
    .expect("rewrite");

    let error = inspect_run(&root)
        .await
        .expect_err("missing artifact metadata should fail");
    assert!(matches!(error, ReplayError::MissingArtifactSize { .. }));
}

#[cfg(unix)]
#[tokio::test]
async fn inspect_rejects_artifact_path_symlink_escape_outside_run() {
    let temp = tempdir().expect("temp");
    let mut node = Node::agent("agent", "agent");
    json_output(&mut node);
    let graph = Graph::new("symlink_escape", "symlink_escape", vec![node]);

    let invoker = TestInvoker::new();
    let (runtime, mut options) = runtime_with(Arc::clone(&invoker), &temp);
    options.run_id = Some("integrity_symlink_escape".to_owned());
    let _ = runtime.run(&graph, options).await.expect("run");

    let root = temp.path().join("runs").join("integrity_symlink_escape");
    let outside = temp.path().join("outside.txt");
    fs::write(&outside, b"outside content")
        .await
        .expect("write outside file");

    let symlink_path = root.join("outside-link.json");
    symlink(&outside, &symlink_path).expect("create symlink");

    let mut summary: RunSummary = serde_json::from_slice(
        &fs::read(root.join("summary.json"))
            .await
            .expect("read summary"),
    )
    .expect("parse summary");
    summary
        .artifacts
        .iter_mut()
        .find(|artifact| artifact.kind != "graph")
        .expect("non-graph artifact")
        .path = "outside-link.json".to_owned();

    fs::write(
        root.join("summary.json"),
        serde_json::to_vec_pretty(&summary).expect("serialize"),
    )
    .await
    .expect("rewrite");

    let error = inspect_run(&root)
        .await
        .expect_err("symlink escape should fail");
    assert!(matches!(
        error,
        ReplayError::ArtifactOutsideRun { .. } | ReplayError::SymlinkPath { .. }
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn inspect_rejects_artifact_in_root_symlink_component() {
    let temp = tempdir().expect("temp");
    let mut node = Node::agent("agent", "agent");
    json_output(&mut node);
    let graph = Graph::new("artifact_root_symlink", "artifact root symlink", vec![node]);

    let invoker = TestInvoker::new();
    let (runtime, mut options) = runtime_with(Arc::clone(&invoker), &temp);
    options.run_id = Some("integrity_artifact_root_symlink".to_owned());
    let mut summary = runtime.run(&graph, options).await.expect("run");

    let root = temp
        .path()
        .join("runs")
        .join("integrity_artifact_root_symlink");
    let nested = root.join("nested");
    fs::create_dir(&nested).await.expect("create nested");
    let link_target = root.join("nested_target");
    symlink(&link_target, nested.join("link")).expect("create link");
    let artifact = summary
        .artifacts
        .iter_mut()
        .find(|artifact| artifact.kind != "graph")
        .expect("non-graph artifact");
    artifact.path = "nested/link/output.json".to_owned();
    fs::write(
        root.join("summary.json"),
        serde_json::to_vec_pretty(&summary).expect("serialize"),
    )
    .await
    .expect("rewrite");

    let error = inspect_run(&root)
        .await
        .expect_err("in-root symlink component should fail");
    assert!(matches!(error, ReplayError::SymlinkPath { .. }));
}

#[cfg(unix)]
#[tokio::test]
async fn inspect_rejects_summary_symlink() {
    let temp = tempdir().expect("temp");
    let mut node = Node::agent("agent", "agent");
    json_output(&mut node);
    let graph = Graph::new("summary_symlink", "summary symlink", vec![node]);

    let invoker = TestInvoker::new();
    let (runtime, mut options) = runtime_with(Arc::clone(&invoker), &temp);
    options.run_id = Some("integrity_summary_symlink".to_owned());
    let _ = runtime.run(&graph, options).await.expect("run");

    let root = temp.path().join("runs").join("integrity_summary_symlink");
    let outside = temp.path().join("outside_summary.txt");
    fs::write(&outside, b"outside summary")
        .await
        .expect("write outside");
    let summary = root.join("summary.json");
    let backup = root.join("summary.json.bak");
    fs::rename(&summary, &backup).await.expect("backup summary");
    symlink(&outside, &summary).expect("create summary symlink");

    let error = inspect_run(&root)
        .await
        .expect_err("summary symlink should fail");
    assert!(matches!(error, ReplayError::SymlinkPath { .. }));
}

#[cfg(unix)]
#[tokio::test]
async fn inspect_rejects_graph_symlink() {
    let temp = tempdir().expect("temp");
    let mut node = Node::agent("agent", "agent");
    json_output(&mut node);
    let graph = Graph::new("graph_symlink", "graph symlink", vec![node]);

    let invoker = TestInvoker::new();
    let (runtime, mut options) = runtime_with(Arc::clone(&invoker), &temp);
    options.run_id = Some("integrity_graph_symlink".to_owned());
    let _ = runtime.run(&graph, options).await.expect("run");

    let root = temp.path().join("runs").join("integrity_graph_symlink");
    let outside = temp.path().join("outside_graph.json");
    fs::write(&outside, b"outside graph")
        .await
        .expect("write outside");
    let graph_path = root.join("graph.json");
    let backup = root.join("graph.json.bak");
    fs::rename(&graph_path, &backup)
        .await
        .expect("backup graph");
    symlink(&outside, &graph_path).expect("create graph symlink");

    let error = inspect_run(&root)
        .await
        .expect_err("graph symlink should fail");
    assert!(matches!(error, ReplayError::SymlinkPath { .. }));
}

#[cfg(unix)]
#[tokio::test]
async fn inspect_rejects_journal_symlink() {
    let temp = tempdir().expect("temp");
    let mut node = Node::agent("agent", "agent");
    json_output(&mut node);
    let graph = Graph::new("journal_symlink", "journal symlink", vec![node]);

    let invoker = TestInvoker::new();
    let (runtime, mut options) = runtime_with(Arc::clone(&invoker), &temp);
    options.run_id = Some("integrity_journal_symlink".to_owned());
    let _ = runtime.run(&graph, options).await.expect("run");

    let root = temp.path().join("runs").join("integrity_journal_symlink");
    let outside = temp.path().join("outside_journal.jsonl");
    fs::write(&outside, b"outside journal")
        .await
        .expect("write outside");
    let journal = root.join("journal.jsonl");
    let backup = root.join("journal.jsonl.bak");
    fs::rename(&journal, &backup).await.expect("backup journal");
    symlink(&outside, &journal).expect("create journal symlink");

    let error = inspect_run(&root)
        .await
        .expect_err("journal symlink should fail");
    assert!(matches!(error, ReplayError::SymlinkPath { .. }));
}

#[tokio::test]
async fn inspect_rejects_artifact_size_mismatch() {
    let temp = tempdir().expect("temp");
    let mut node = Node::agent("agent", "agent");
    json_output(&mut node);
    let graph = Graph::new("size", "size", vec![node]);

    let invoker = TestInvoker::new();
    let (runtime, mut options) = runtime_with(Arc::clone(&invoker), &temp);
    options.run_id = Some("integrity_size".to_owned());
    let _ = runtime.run(&graph, options).await.expect("run");

    let root = temp.path().join("runs").join("integrity_size");
    let mut summary: RunSummary = serde_json::from_slice(
        &fs::read(root.join("summary.json"))
            .await
            .expect("read summary"),
    )
    .expect("parse summary");
    summary.artifacts[0].size = summary.artifacts[0].size.map(|value| value + 1);
    fs::write(
        root.join("summary.json"),
        serde_json::to_vec_pretty(&summary).expect("serialize"),
    )
    .await
    .expect("rewrite");

    let error = inspect_run(&root)
        .await
        .expect_err("size mismatch should fail");
    assert!(matches!(error, ReplayError::ArtifactSizeMismatch { .. }));
}

#[tokio::test]
async fn inspect_rejects_artifact_ref_mismatch() {
    let temp = tempdir().expect("temp");
    let mut node = Node::agent("agent", "agent");
    json_output(&mut node);
    let graph = Graph::new("artifact_ref", "artifact ref", vec![node]);

    let invoker = TestInvoker::new();
    let (runtime, mut options) = runtime_with(Arc::clone(&invoker), &temp);
    options.run_id = Some("integrity_artifact_ref_mismatch".to_owned());
    let mut summary = runtime.run(&graph, options).await.expect("run");

    let root = temp
        .path()
        .join("runs")
        .join("integrity_artifact_ref_mismatch");
    let artifact = summary
        .artifacts
        .iter_mut()
        .find(|artifact| artifact.kind != "graph")
        .expect("non-graph artifact");
    artifact.path = "changed.json".to_owned();
    fs::write(
        root.join("summary.json"),
        serde_json::to_vec_pretty(&summary).expect("serialize"),
    )
    .await
    .expect("rewrite");

    let error = inspect_run(&root)
        .await
        .expect_err("artifact ref set mismatch should fail");
    assert!(matches!(
        error,
        ReplayError::SummaryReplayArtifactMismatch { .. }
    ));
}

#[tokio::test]
async fn inspect_run_succeeds_for_retry_with_all_attempt_artifacts() {
    let temp = tempdir().expect("temp");
    let mut node = Node::agent("retry", "retry");
    node.retry.max_attempts = 2;
    let graph = Graph::new("retry_artifacts", "retry artifacts", vec![node]);
    let invoker: Arc<dyn ProviderInvoker> = RetryInvoker::new();
    let runtime = Runtime::from_invoker(Arc::clone(&invoker), temp.path().join("runs"));
    let mut options = RunOptions {
        current_dir: temp.path().to_path_buf(),
        ..RunOptions::default()
    };
    options.run_id = Some("integrity_retry_artifacts".to_owned());

    let summary = runtime.run(&graph, options).await.expect("run");
    let root = temp.path().join("runs").join("integrity_retry_artifacts");
    let inspected = inspect_run(&root)
        .await
        .expect("retry artifacts should be covered by replay");
    assert_eq!(inspected.summary.status, summary.status);
}

#[tokio::test]
async fn inspect_run_succeeds_for_subgraph_and_matches_root_and_qualified_statuses() {
    let temp = tempdir().expect("temp");

    let mut inner_node = Node::agent("inner", "inner");
    json_output(&mut inner_node);
    let inner = Graph::new("inner-graph", "inner goal", vec![inner_node]);

    let subgraph_node = Node {
        id: "subgraph".to_owned(),
        label: None,
        requires: Vec::new(),
        resources: Vec::new(),
        retry: RetryPolicy::default(),
        timeout_seconds: None,
        workspace: WorkspaceSpec::default(),
        context: ContextSpec::default(),
        continue_on_failure: false,
        kind: graph::NodeKind::Subgraph {
            graph: Box::new(inner),
        },
    };

    let graph = Graph::new(
        "inspect-subgraph-root",
        "inspect subgraph root",
        vec![subgraph_node],
    );

    let invoker = TestInvoker::new();
    let (runtime, mut options) = runtime_with(Arc::clone(&invoker), &temp);
    options.run_id = Some("integrity_subgraph".to_owned());

    let _ = runtime.run(&graph, options).await.expect("run");
    let root = temp.path().join("runs").join("integrity_subgraph");

    let inspected = inspect_run(&root).await.expect("inspect should succeed");

    assert_eq!(
        inspected.summary.nodes["subgraph"].status,
        inspected
            .replay
            .nodes
            .get("subgraph")
            .expect("replay should include subgraph node")
            .status
    );
    if !inspected
        .replay
        .nodes
        .keys()
        .any(|node_id| node_id.starts_with("subgraph.") && node_id != "subgraph")
    {
        panic!(
            "no qualified nodes under subgraph in replay: {:?}",
            inspected.replay.nodes.keys().collect::<Vec<_>>()
        );
    }
    assert!(inspected.summary.nodes.get("subgraph.inner").is_none());
    assert_eq!(
        inspected.summary.status,
        inspected.replay.final_status.unwrap()
    );
}

#[tokio::test]
async fn inspect_rejects_missing_graph_artifact_reference() {
    let temp = tempdir().expect("temp");
    let mut node = Node::agent("agent", "agent");
    json_output(&mut node);
    let graph = Graph::new(
        "missing_graph_artifact",
        "missing graph artifact",
        vec![node],
    );

    let invoker = TestInvoker::new();
    let (runtime, mut options) = runtime_with(Arc::clone(&invoker), &temp);
    options.run_id = Some("integrity_missing_graph_artifact".to_owned());
    let summary = runtime.run(&graph, options).await.expect("run");

    let root = temp
        .path()
        .join("runs")
        .join("integrity_missing_graph_artifact");
    let mut tampered = summary;
    tampered
        .artifacts
        .retain(|artifact| artifact.kind != "graph");

    fs::write(
        root.join("summary.json"),
        serde_json::to_vec_pretty(&tampered).expect("serialize"),
    )
    .await
    .expect("rewrite summary");

    let error = inspect_run(&root)
        .await
        .expect_err("missing graph artifact should fail");
    assert!(matches!(error, ReplayError::MissingGraphArtifact));
}

#[tokio::test]
async fn inspect_rejects_graph_hash_mismatch_between_graph_json_and_provenance() {
    let temp = tempdir().expect("temp");
    let mut node = Node::agent("agent", "agent");
    json_output(&mut node);
    let graph = Graph::new("tampered_graph", "tampered graph", vec![node]);

    let invoker = TestInvoker::new();
    let (runtime, mut options) = runtime_with(Arc::clone(&invoker), &temp);
    options.run_id = Some("integrity_tampered_graph_hash".to_owned());
    let _ = runtime.run(&graph, options).await.expect("run");

    let root = temp
        .path()
        .join("runs")
        .join("integrity_tampered_graph_hash");
    let mut graph_value: Graph =
        serde_json::from_slice(&fs::read(root.join("graph.json")).await.expect("read graph"))
            .expect("parse graph");
    graph_value.metadata.name = "tampered-graph-name".to_owned();
    fs::write(
        root.join("graph.json"),
        serde_json::to_vec_pretty(&graph_value).expect("serialize"),
    )
    .await
    .expect("rewrite graph");

    let error = inspect_run(&root)
        .await
        .expect_err("tampered graph hash should fail");
    assert!(matches!(error, ReplayError::GraphHashMismatch { .. }));
}

#[tokio::test]
async fn inspect_rejects_oversized_summary_file() {
    let temp = tempdir().expect("temp");
    let mut node = Node::agent("agent", "agent");
    json_output(&mut node);
    let graph = Graph::new("oversized_summary", "oversized summary", vec![node]);

    let invoker = TestInvoker::new();
    let (runtime, mut options) = runtime_with(Arc::clone(&invoker), &temp);
    options.run_id = Some("integrity_oversized_summary".to_owned());
    let _ = runtime.run(&graph, options).await.expect("run");

    let root = temp.path().join("runs").join("integrity_oversized_summary");
    let path = root.join("summary.json");
    let file = OpenOptions::new()
        .write(true)
        .open(&path)
        .await
        .expect("open summary");
    file.set_len(MAX_SUMMARY_BYTES as u64 + 1)
        .await
        .expect("resize summary");
    let error = inspect_run(&root)
        .await
        .expect_err("oversized summary should fail");
    assert!(matches!(error, ReplayError::FileTooLarge { .. }));
}

#[tokio::test]
async fn inspect_rejects_oversized_graph_file() {
    let temp = tempdir().expect("temp");
    let mut node = Node::agent("agent", "agent");
    json_output(&mut node);
    let graph = Graph::new("oversized_graph", "oversized graph", vec![node]);

    let invoker = TestInvoker::new();
    let (runtime, mut options) = runtime_with(Arc::clone(&invoker), &temp);
    options.run_id = Some("integrity_oversized_graph".to_owned());
    let _ = runtime.run(&graph, options).await.expect("run");

    let root = temp.path().join("runs").join("integrity_oversized_graph");
    let path = root.join("graph.json");
    let file = OpenOptions::new()
        .write(true)
        .open(&path)
        .await
        .expect("open graph");
    file.set_len(MAX_GRAPH_BYTES as u64 + 1)
        .await
        .expect("resize graph");
    let error = inspect_run(&root)
        .await
        .expect_err("oversized graph should fail");
    assert!(matches!(error, ReplayError::FileTooLarge { .. }));
}

#[tokio::test]
async fn inspect_rejects_oversized_artifact_file() {
    let temp = tempdir().expect("temp");
    let mut node = Node::agent("agent", "agent");
    json_output(&mut node);
    let graph = Graph::new("oversized_artifact", "oversized artifact", vec![node]);

    let invoker = TestInvoker::new();
    let (runtime, mut options) = runtime_with(Arc::clone(&invoker), &temp);
    options.run_id = Some("integrity_oversized_artifact".to_owned());
    let mut summary = runtime.run(&graph, options).await.expect("run");

    let root = temp
        .path()
        .join("runs")
        .join("integrity_oversized_artifact");
    let artifact = summary
        .artifacts
        .iter_mut()
        .find(|artifact| artifact.kind != "graph")
        .expect("non-graph artifact");
    let artifact_path = root.join(&artifact.path);
    let file = OpenOptions::new()
        .write(true)
        .open(&artifact_path)
        .await
        .expect("open artifact");
    file.set_len(MAX_ARTIFACT_BYTES as u64 + 1)
        .await
        .expect("resize artifact");
    let digest = Sha256::digest(vec![0u8; MAX_ARTIFACT_BYTES + 1]);
    artifact.size = Some(MAX_ARTIFACT_BYTES as u64 + 1);
    artifact.sha256 = Some(hex::encode(digest));
    fs::write(
        root.join("summary.json"),
        serde_json::to_vec_pretty(&summary).expect("serialize"),
    )
    .await
    .expect("rewrite");

    let error = inspect_run(&root)
        .await
        .expect_err("oversized artifact should fail");
    assert!(matches!(error, ReplayError::FileTooLarge { .. }));
}

#[tokio::test]
async fn inspect_allows_artifact_between_16_and_64_mib() {
    let temp = tempdir().expect("temp");
    let mut node = Node::agent("agent", "agent");
    json_output(&mut node);
    let graph = Graph::new("bounded_artifacts", "bounded artifacts", vec![node]);

    let invoker = TestInvoker::new();
    let (runtime, mut options) = runtime_with(Arc::clone(&invoker), &temp);
    options.run_id = Some("integrity_artifact_size_bounds".to_owned());
    let summary = runtime.run(&graph, options).await.expect("run");
    let root = temp
        .path()
        .join("runs")
        .join("integrity_artifact_size_bounds");

    let twelve = summary.clone();
    let _artifact_16mb = twelve
        .artifacts
        .iter()
        .find(|artifact| artifact.kind != "graph")
        .expect("non-graph artifact");
    let artifact = twelve
        .artifacts
        .iter()
        .find(|artifact| artifact.kind != "graph")
        .expect("non-graph artifact")
        .path
        .clone();
    let mut summary_16 = summary.clone();
    let artifact_16 = summary_16
        .artifacts
        .iter_mut()
        .find(|artifact| artifact.kind != "graph")
        .expect("non-graph artifact");
    let artifact_path = root.join(&artifact);
    let file = OpenOptions::new()
        .write(true)
        .open(&artifact_path)
        .await
        .expect("open artifact");
    let bytes_16 = vec![0u8; MAX_ARTIFACT_BYTES / 2];
    let digest_16 = Sha256::digest(&bytes_16);
    file.set_len(MAX_ARTIFACT_BYTES as u64 / 2)
        .await
        .expect("resize 16mb artifact");
    artifact_16.size = Some((MAX_ARTIFACT_BYTES / 2) as u64);
    artifact_16.sha256 = Some(hex::encode(digest_16));
    fs::write(
        root.join("summary.json"),
        serde_json::to_vec_pretty(&summary_16).expect("serialize"),
    )
    .await
    .expect("rewrite");
    inspect_run(&root)
        .await
        .expect("16 MiB artifact should be allowed");

    let mut summary_64 = summary_16;
    let artifact_64 = summary_64
        .artifacts
        .iter_mut()
        .find(|artifact| artifact.kind != "graph")
        .expect("non-graph artifact");
    let bytes_64 = vec![0u8; MAX_ARTIFACT_BYTES];
    let digest_64 = Sha256::digest(&bytes_64);
    file.set_len(MAX_ARTIFACT_BYTES as u64)
        .await
        .expect("resize 64mb artifact");
    artifact_64.size = Some(MAX_ARTIFACT_BYTES as u64);
    artifact_64.sha256 = Some(hex::encode(digest_64));
    fs::write(
        root.join("summary.json"),
        serde_json::to_vec_pretty(&summary_64).expect("serialize"),
    )
    .await
    .expect("rewrite");
    inspect_run(&root)
        .await
        .expect("64 MiB artifact should be allowed");
}

#[tokio::test]
async fn inspect_rejects_replay_without_terminal_event_when_final_rows_removed() {
    let temp = tempdir().expect("temp");
    let mut node = Node::agent("agent", "agent");
    json_output(&mut node);
    let graph = Graph::new("trimmed", "trimmed", vec![node]);

    let invoker = TestInvoker::new();
    let (runtime, mut options) = runtime_with(Arc::clone(&invoker), &temp);
    options.run_id = Some("integrity_missing_tail_rows".to_owned());
    let _ = runtime.run(&graph, options).await.expect("run");

    let root = temp.path().join("runs").join("integrity_missing_tail_rows");
    let journal_path = root.join("journal.jsonl");
    let bytes = fs::read(&journal_path).await.expect("read journal");
    let cut = bytes
        .iter()
        .position(|byte| *byte == b'\n')
        .expect("journal has newline")
        + 1;
    let truncated = bytes[..cut].to_vec();
    fs::write(&journal_path, truncated)
        .await
        .expect("rewrite truncated journal");

    let error = inspect_run(&root)
        .await
        .expect_err("missing terminal event should fail");
    assert!(matches!(error, ReplayError::RunDidNotFinish));
}
