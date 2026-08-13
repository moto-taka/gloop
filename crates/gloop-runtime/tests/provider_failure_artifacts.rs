use std::sync::{Arc, atomic::AtomicUsize, atomic::Ordering};

use async_trait::async_trait;
use gloop_core::graph::OutputFormat;
use gloop_core::{FinalStatus, Node, NodeKind, NodeStatus, RunEventKind};
use gloop_provider::{
    AdapterCapabilities, AdapterError, AdapterOutput, AdapterRequest, AdapterResponse, ModelOrigin,
    SelectionOrigin, TokenUsage,
};
use gloop_runtime::{
    ProviderInvocation, ProviderInvoker, RunOptions, Runtime, inspect_run, read_events,
};
use serde_json::json;
use tempfile::tempdir;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
struct TestInvoker {
    planned: Mutex<Vec<Result<AdapterResponse, AdapterError>>>,
    call_count: AtomicUsize,
}

impl TestInvoker {
    fn new(planned: Vec<AdapterResponse>) -> Arc<Self> {
        Arc::new(Self {
            planned: Mutex::new(planned.into_iter().map(Ok).collect()),
            call_count: AtomicUsize::new(0),
        })
    }

    fn failing(error: AdapterError) -> Arc<Self> {
        Arc::new(Self {
            planned: Mutex::new(vec![Err(error)]),
            call_count: AtomicUsize::new(0),
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
        self.call_count.fetch_add(1, Ordering::SeqCst);
        let response = {
            let mut lock = self.planned.lock().await;
            lock.pop().expect("provider invocation planned")
        }?;
        Ok(ProviderInvocation {
            profile: preferred_profile.unwrap_or("fallback").to_owned(),
            selected_model: response.reported_model.clone(),
            selection_origin: if preferred_profile.is_some() {
                SelectionOrigin::Explicit
            } else {
                SelectionOrigin::Capability
            },
            model_origin: ModelOrigin::ProviderDefault,
            response,
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

fn build_graph_and_bad_response() -> (gloop_core::Graph, serde_json::Value, AdapterResponse) {
    let mut node = Node::agent("schema_node", "schema_node");
    if let NodeKind::Agent { output, .. } = &mut node.kind {
        output.format = OutputFormat::Json;
        output.inline_schema = Some(
            json!({"type":"object","required":["value"],"properties":{"value":{"type":"string"}}}),
        );
    }
    let graph = gloop_core::Graph::new("provider_failure_artifacts", "schema failure", vec![node]);
    let bad_output = json!({"answer": 123});
    let response = AdapterResponse {
        output: AdapterOutput::Json(bad_output.clone()),
        stdout: "provider stdout sample\n".to_owned(),
        stderr: "provider stderr sample\n".to_owned(),
        exit_code: Some(0),
        reported_model: Some("provider-model".to_owned()),
        reported_model_informational: false,
        usage: Some(TokenUsage::default()),
    };
    (graph, bad_output, response)
}

fn assert_artifact_contents(run_dir: &std::path::Path, path: &str, expected: &str) -> Vec<u8> {
    let bytes = std::fs::read(run_dir.join(path)).expect("artifact");
    assert_eq!(
        std::str::from_utf8(&bytes).expect("artifact utf8"),
        expected
    );
    bytes
}

#[tokio::test]
async fn provider_output_and_streams_are_written_as_artifacts_on_schema_validation_failure() {
    let temp = tempdir().expect("tempdir");
    let (graph, bad_output, response) = build_graph_and_bad_response();
    let invoker = TestInvoker::new(vec![response]);

    let (runtime, mut options) = runtime_with(Arc::clone(&invoker), &temp);
    options.run_id = Some("provider-schema-fail".to_owned());
    let summary = runtime.run(&graph, options).await.expect("run");

    let run_dir = temp.path().join("runs").join("provider-schema-fail");
    let inspected = inspect_run(&run_dir).await.expect("inspect run");
    let events = read_events(run_dir.join("journal.jsonl"))
        .await
        .expect("read events");
    let failure_event = events
        .into_iter()
        .find(|event| {
            event.kind == RunEventKind::NodeFailed
                && event.node_id.as_deref() == Some("schema_node")
        })
        .expect("failure event");

    assert_eq!(summary.status, FinalStatus::Failed);
    let outcome = &summary.nodes["schema_node"];
    assert_eq!(outcome.status, NodeStatus::Failed);

    let output_artifact = outcome.output_artifact.as_deref().unwrap_or_default();
    let stdout_artifact = outcome.stdout_artifact.as_deref().unwrap_or_default();
    let stderr_artifact = outcome.stderr_artifact.as_deref().unwrap_or_default();
    assert!(!output_artifact.is_empty());
    assert!(!stdout_artifact.is_empty());
    assert!(!stderr_artifact.is_empty());

    assert_artifact_contents(
        &run_dir,
        output_artifact,
        &serde_json::to_string(&bad_output).expect("serialize"),
    );
    assert_artifact_contents(&run_dir, stdout_artifact, "provider stdout sample\n");
    assert_artifact_contents(&run_dir, stderr_artifact, "provider stderr sample\n");

    assert_eq!(
        failure_event
            .data
            .get("output_artifact")
            .and_then(|value| value.as_str()),
        Some(output_artifact),
    );
    assert_eq!(
        failure_event
            .data
            .get("stdout_artifact")
            .and_then(|value| value.as_str()),
        Some(stdout_artifact),
    );
    assert_eq!(
        failure_event
            .data
            .get("stderr_artifact")
            .and_then(|value| value.as_str()),
        Some(stderr_artifact),
    );

    assert_eq!(
        inspected.summary.nodes["schema_node"].output_artifact,
        outcome.output_artifact
    );
    assert_eq!(
        inspected.summary.nodes["schema_node"].stdout_artifact,
        outcome.stdout_artifact
    );
    assert_eq!(
        inspected.summary.nodes["schema_node"].stderr_artifact,
        outcome.stderr_artifact
    );
    assert_eq!(
        inspected.summary.nodes["schema_node"].status,
        NodeStatus::Failed
    );
    assert_eq!(inspected.summary.status, FinalStatus::Failed);
    assert_eq!(inspected.summary.status, summary.status);
    assert_eq!(invoker.call_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn provider_process_failure_streams_are_written_as_sanitized_artifacts() {
    let temp = tempdir().expect("tempdir");
    let graph = gloop_core::Graph::new(
        "provider_process_failure_artifacts",
        "provider process failure",
        vec![Node::agent("process_node", "process_node")],
    );
    let invoker = TestInvoker::failing(AdapterError::ProcessFailed {
        profile: "failing-profile".to_owned(),
        executable: "provider-command".to_owned(),
        code: Some(17),
        stdout: "sanitized stdout: [REDACTED]\n".to_owned(),
        stderr: "sanitized stderr: [REDACTED]\n".to_owned(),
    });

    let (runtime, mut options) = runtime_with(Arc::clone(&invoker), &temp);
    options.run_id = Some("provider-process-fail".to_owned());
    let summary = runtime.run(&graph, options).await.expect("run");

    let run_dir = temp.path().join("runs").join("provider-process-fail");
    let inspected = inspect_run(&run_dir).await.expect("inspect run");
    let events = read_events(run_dir.join("journal.jsonl"))
        .await
        .expect("read events");
    let failure_event = events
        .into_iter()
        .find(|event| {
            event.kind == RunEventKind::NodeFailed
                && event.node_id.as_deref() == Some("process_node")
        })
        .expect("failure event");

    assert_eq!(summary.status, FinalStatus::Failed);
    let outcome = &summary.nodes["process_node"];
    assert_eq!(outcome.status, NodeStatus::Failed);
    assert_eq!(outcome.profile.as_deref(), Some("failing-profile"));
    assert_eq!(outcome.exit_code, Some(17));
    assert!(
        outcome
            .error
            .as_deref()
            .is_some_and(|error| error.starts_with("[gloop:provider_process]"))
    );

    let output_artifact = outcome.output_artifact.as_deref().unwrap_or_default();
    let stdout_artifact = outcome.stdout_artifact.as_deref().unwrap_or_default();
    let stderr_artifact = outcome.stderr_artifact.as_deref().unwrap_or_default();
    assert!(!output_artifact.is_empty());
    assert!(!stdout_artifact.is_empty());
    assert!(!stderr_artifact.is_empty());

    assert_artifact_contents(&run_dir, output_artifact, "");
    assert_artifact_contents(&run_dir, stdout_artifact, "sanitized stdout: [REDACTED]\n");
    assert_artifact_contents(&run_dir, stderr_artifact, "sanitized stderr: [REDACTED]\n");

    assert_eq!(
        failure_event
            .data
            .get("error_class")
            .and_then(|value| value.as_str()),
        Some("provider_process"),
    );
    assert_eq!(
        failure_event
            .data
            .get("exit_code")
            .and_then(serde_json::Value::as_i64),
        Some(17),
    );
    for (key, expected) in [
        ("output_artifact", output_artifact),
        ("stdout_artifact", stdout_artifact),
        ("stderr_artifact", stderr_artifact),
    ] {
        assert_eq!(
            failure_event.data.get(key).and_then(|value| value.as_str()),
            Some(expected),
        );
    }

    let inspected_outcome = &inspected.summary.nodes["process_node"];
    assert_eq!(inspected_outcome.profile, outcome.profile);
    assert_eq!(inspected_outcome.exit_code, outcome.exit_code);
    assert_eq!(inspected_outcome.output_artifact, outcome.output_artifact);
    assert_eq!(inspected_outcome.stdout_artifact, outcome.stdout_artifact);
    assert_eq!(inspected_outcome.stderr_artifact, outcome.stderr_artifact);
    assert_eq!(invoker.call_count.load(Ordering::SeqCst), 1);
}
