use std::{
    collections::{HashSet, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use gloop_core::{
    FinalStatus, Graph, Node, NodeKind, NodeStatus, RunEvent, RunEventKind,
    graph::{Edge, EdgeKind},
};
use gloop_provider::{
    AdapterCapabilities, AdapterError, AdapterOutput, AdapterRequest, AdapterResponse, ModelOrigin,
    SelectionOrigin, TokenUsage,
};
use gloop_runtime::{
    NodeFailureClass, ProviderInvocation, ProviderInvoker, RunOptions, Runtime, node_failure_class,
    read_events, read_journal, replay_events, replay_journal,
};
use serde_json::{Value, json};
use tempfile::tempdir;
use tokio::{
    sync::{Mutex, Notify},
    time::sleep,
};
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
enum Invocation {
    Ok {
        profile: String,
        output: AdapterOutput,
        model: Option<String>,
    },
    Raw {
        profile: String,
        output: AdapterOutput,
        selected_model: Option<String>,
        reported_model: Option<String>,
        reported_model_informational: bool,
        stdout: String,
        stderr: String,
    },
    Err(AdapterError),
}

#[derive(Debug)]
struct ScriptedInvocation {
    expect_prompt_fragment: Option<String>,
    wait_for_release: bool,
    invocation: Invocation,
}

#[derive(Debug)]
struct Call;

#[derive(Debug)]
struct TestInvoker {
    planned: Mutex<VecDeque<ScriptedInvocation>>,
    calls: Mutex<Vec<Call>>,
    max_active: AtomicUsize,
    active: AtomicUsize,
    started: Notify,
    release: Notify,
    fail_if_unplanned: bool,
}

impl TestInvoker {
    fn new(planned: Vec<ScriptedInvocation>) -> Arc<Self> {
        Arc::new(Self {
            planned: Mutex::new(planned.into()),
            calls: Mutex::new(Vec::new()),
            max_active: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            started: Notify::new(),
            release: Notify::new(),
            fail_if_unplanned: true,
        })
    }

    fn max_active(&self) -> usize {
        self.max_active.load(Ordering::SeqCst)
    }

    async fn call_count(&self) -> usize {
        self.calls.lock().await.len()
    }

    async fn wait_for_calls(&self, count: usize) {
        loop {
            let calls = self.calls.lock().await.len();
            if calls >= count {
                return;
            }
            tokio::task::yield_now().await;
        }
    }

    fn release_blocked(&self) {
        self.release.notify_waiters();
    }
}

#[async_trait]
impl ProviderInvoker for TestInvoker {
    async fn execute(
        &self,
        preferred_profile: Option<&str>,
        _required: &AdapterCapabilities,
        request: AdapterRequest,
        _cancellation: CancellationToken,
    ) -> Result<ProviderInvocation, AdapterError> {
        self.calls.lock().await.push(Call);

        let current_active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(current_active, Ordering::SeqCst);
        self.started.notify_waiters();

        let mut lock = self.planned.lock().await;
        if self.fail_if_unplanned && lock.is_empty() {
            self.active.fetch_sub(1, Ordering::SeqCst);
            return Err(AdapterError::InvalidRequest {
                profile: preferred_profile.unwrap_or("fake").to_owned(),
                message: "unexpected provider invocation".to_owned(),
            });
        }

        let plan = if let Some(index) = lock.iter().position(|entry| {
            entry
                .expect_prompt_fragment
                .as_ref()
                .is_some_and(|fragment| request.prompt.contains(fragment))
        }) {
            lock.remove(index).expect("provider invocation planned")
        } else {
            lock.pop_front().expect("provider invocation planned")
        };
        if let Some(fragment) = &plan.expect_prompt_fragment {
            assert!(
                request.prompt.contains(fragment),
                "provider call prompt mismatch: {:?}",
                request.prompt
            );
        }
        let wait = plan.wait_for_release;
        let invocation = plan.invocation;
        drop(lock);

        if wait {
            self.release.notified().await;
        }

        let result = match invocation {
            Invocation::Ok {
                profile,
                output,
                model,
            } => Ok(ProviderInvocation {
                profile,
                selected_model: model,
                selection_origin: if preferred_profile.is_some() {
                    SelectionOrigin::Explicit
                } else {
                    SelectionOrigin::Capability
                },
                model_origin: ModelOrigin::ProviderDefault,
                response: AdapterResponse {
                    output,
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: Some(0),
                    reported_model: None,
                    reported_model_informational: false,
                    usage: Some(TokenUsage::default()),
                },
            }),
            Invocation::Raw {
                profile,
                output,
                selected_model,
                reported_model,
                reported_model_informational,
                stdout,
                stderr,
            } => Ok(ProviderInvocation {
                profile,
                selected_model,
                selection_origin: if preferred_profile.is_some() {
                    SelectionOrigin::Explicit
                } else {
                    SelectionOrigin::Capability
                },
                model_origin: ModelOrigin::Request,
                response: AdapterResponse {
                    output,
                    stdout,
                    stderr,
                    exit_code: Some(0),
                    reported_model,
                    reported_model_informational,
                    usage: Some(TokenUsage::default()),
                },
            }),
            Invocation::Err(error) => Err(error),
        };

        self.active.fetch_sub(1, Ordering::SeqCst);
        result
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
        output.format = gloop_core::graph::OutputFormat::Json;
    }
}

#[tokio::test]
async fn loop_with_succeeded_condition_still_fails_when_inner_node_fails() {
    let temp = tempdir().expect("temp");
    let condition = {
        let mut condition = Node::agent("condition", "condition");
        json_output(&mut condition);
        condition
    };
    let failing_inner = Node::agent("failing", "failing");
    let outer_loop_graph = Graph::new("inner", "inner", vec![condition, failing_inner]);
    // Use a successful condition target and a separate failing inner node.
    let mut loop_node = Node::agent("outer_loop", "outer_loop");
    loop_node.kind = NodeKind::Loop {
        graph: Box::new(outer_loop_graph),
        until: gloop_core::graph::LoopCondition {
            node: "condition".to_owned(),
            status: NodeStatus::Succeeded,
            output_contains: None,
            json_pointer: None,
            equals: None,
        },
        max_iterations: 1,
        stagnation_after: 1,
    };
    let graph = Graph::new("loop_outer", "loop outer", vec![loop_node]);

    let invoker = TestInvoker::new(vec![
        ScriptedInvocation {
            expect_prompt_fragment: Some("condition".to_owned()),
            wait_for_release: false,
            invocation: Invocation::Ok {
                profile: "p".to_owned(),
                output: AdapterOutput::Json(json!(true)),
                model: Some("m".to_owned()),
            },
        },
        ScriptedInvocation {
            expect_prompt_fragment: Some("failing".to_owned()),
            wait_for_release: false,
            invocation: Invocation::Err(AdapterError::Timeout {
                profile: "p".to_owned(),
                timeout_ms: 10,
                retryable: false,
            }),
        },
    ]);

    let (runtime, options) = runtime_with(invoker, &temp);
    let summary = runtime.run(&graph, options).await.expect("run");

    assert_eq!(summary.nodes["outer_loop"].status, NodeStatus::Failed);
    assert_eq!(summary.status, FinalStatus::Failed);
}

fn probe_attempt_ids_and_output_artifacts(
    events: &[RunEvent],
) -> (HashSet<String>, HashSet<String>) {
    let mut output_artifacts = HashSet::new();
    let mut qualified_ids = HashSet::new();
    for event in events {
        if event.kind != RunEventKind::NodeOutput {
            continue;
        }
        let Some(node_id) = event.node_id.as_deref() else {
            continue;
        };
        if node_id.starts_with("outer.") && node_id.strip_suffix(".probe").is_some() {
            qualified_ids.insert(node_id.to_owned());
            if let Some(path) = event
                .data
                .get("output_artifact")
                .and_then(|value| value.as_str())
            {
                output_artifacts.insert(path.to_owned());
            }
        }
    }
    (qualified_ids, output_artifacts)
}

#[tokio::test]
async fn nested_subgraph_rate_limit_does_not_repeat_successful_inner_calls() {
    let temp = tempdir().expect("temp");
    let inner_probe = {
        let mut inner_node = Node::agent("probe", "probe");
        json_output(&mut inner_node);
        inner_node
    };
    let mut inner_fail = Node::agent("failer", "failer");
    json_output(&mut inner_fail);
    let mut inner = Graph::new("inner_nested", "inner", vec![inner_probe, inner_fail]);
    inner.spec.edges = vec![Edge {
        from: "probe".to_owned(),
        to: "failer".to_owned(),
        kind: EdgeKind::Data,
        when: None,
    }];
    let mut outer = Node::agent("outer", "outer");
    outer.kind = NodeKind::Subgraph {
        graph: Box::new(inner),
    };
    outer.retry.max_attempts = 2;
    let graph = Graph::new("nested_retry", "nested", vec![outer]);

    let invoker = TestInvoker::new(vec![
        ScriptedInvocation {
            expect_prompt_fragment: Some("probe".to_owned()),
            wait_for_release: false,
            invocation: Invocation::Ok {
                profile: "p".to_owned(),
                output: AdapterOutput::Json(json!({"probe": 1})),
                model: Some("m".to_owned()),
            },
        },
        ScriptedInvocation {
            expect_prompt_fragment: Some("failer".to_owned()),
            wait_for_release: false,
            invocation: Invocation::Err(AdapterError::HttpStatus {
                profile: "p".to_owned(),
                status: 429,
                error_type: Some("rate_limit".to_owned()),
                error_code: None,
            }),
        },
    ]);

    let run_id = "nested_retry_subgraph".to_owned();
    let (runtime, mut options) = runtime_with(Arc::clone(&invoker), &temp);
    options.run_id = Some(run_id.clone());
    let summary = runtime.run(&graph, options).await.expect("run");

    assert_eq!(summary.status, FinalStatus::Failed);
    assert_eq!(summary.nodes["outer"].status, NodeStatus::Failed);
    assert_eq!(summary.nodes["outer"].attempts, 1);
    assert_eq!(invoker.call_count().await, 2);

    let events: Vec<RunEvent> =
        read_events(temp.path().join("runs").join(&run_id).join("journal.jsonl"))
            .await
            .expect("read journal");
    let (qualified_ids, output_artifacts) = probe_attempt_ids_and_output_artifacts(&events);
    assert!(!output_artifacts.is_empty());
    assert!(qualified_ids.contains("outer.attempt-1.probe"));
    assert!(!qualified_ids.contains("outer.attempt-2.probe"));

    let journal = read_journal(temp.path().join("runs").join(&run_id).join("journal.jsonl"))
        .await
        .expect("read journal");
    let replay = replay_events(&journal.events).expect("replay journal");
    assert_eq!(replay.final_status, Some(FinalStatus::Failed));
    assert!(replay.nodes.contains_key("outer.attempt-1.probe"));
    assert!(!replay.nodes.contains_key("outer.attempt-2.probe"));
}

#[tokio::test]
async fn outer_max_parallel_policy_propagates_to_nested_subgraph() {
    let temp = tempdir().expect("temp");
    let inner_a = Node::agent("inner_a", "inner a");
    let inner_b = Node::agent("inner_b", "inner b");
    let inner = Graph::new("inner_parallel", "inner", vec![inner_a, inner_b]);
    let mut outer = Node::agent("outer", "outer");
    outer.kind = NodeKind::Subgraph {
        graph: Box::new(inner),
    };
    let graph = Graph::new("nested_policy", "nested", vec![outer]);

    let invoker = TestInvoker::new(vec![
        ScriptedInvocation {
            expect_prompt_fragment: Some("inner a".to_owned()),
            wait_for_release: true,
            invocation: Invocation::Ok {
                profile: "p".to_owned(),
                output: AdapterOutput::Text("ok".to_owned()),
                model: Some("m".to_owned()),
            },
        },
        ScriptedInvocation {
            expect_prompt_fragment: Some("inner b".to_owned()),
            wait_for_release: true,
            invocation: Invocation::Ok {
                profile: "p".to_owned(),
                output: AdapterOutput::Text("ok".to_owned()),
                model: Some("m".to_owned()),
            },
        },
    ]);

    let (runtime, mut options) = runtime_with(Arc::clone(&invoker), &temp);
    options.max_parallel = Some(1);
    let run = tokio::spawn(async move { runtime.run(&graph, options).await.expect("run") });

    invoker.wait_for_calls(1).await;
    sleep(std::time::Duration::from_millis(50)).await;
    let first_peak = invoker.max_active();
    invoker.release_blocked();
    invoker.wait_for_calls(2).await;
    invoker.release_blocked();
    let summary = run.await.expect("run finished");
    let second_peak = invoker.max_active();

    assert_eq!(summary.status, FinalStatus::ReadyForHuman);
    assert!(
        first_peak <= 1,
        "nested execution must honor max_parallel=1"
    );
    assert!(
        second_peak <= 1,
        "nested execution must honor max_parallel=1"
    );
    assert_eq!(invoker.call_count().await, 2);
}

#[tokio::test]
async fn retry_backoff_cancellation_matches_summary_and_replay_status() {
    let temp = tempdir().expect("temp");
    let mut node = Node::agent("retry", "retry");
    node.retry.max_attempts = 2;
    node.retry.backoff_seconds = 30;
    let graph = Graph::new("retry_cancel", "retry", vec![node]);

    let invoker = TestInvoker::new(vec![ScriptedInvocation {
        expect_prompt_fragment: Some("retry".to_owned()),
        wait_for_release: false,
        invocation: Invocation::Err(AdapterError::HttpStatus {
            profile: "p".to_owned(),
            status: 429,
            error_type: Some("rate_limit".to_owned()),
            error_code: None,
        }),
    }]);

    let cancellation = CancellationToken::new();
    let (runtime, mut options) = runtime_with(Arc::clone(&invoker), &temp);
    options.cancellation = cancellation.clone();
    options.run_id = Some("retry-cancel".to_owned());

    let handle = tokio::spawn(async move { runtime.run(&graph, options).await.expect("run") });
    invoker.wait_for_calls(1).await;
    sleep(std::time::Duration::from_millis(10)).await;
    cancellation.cancel();
    let summary = handle.await.expect("task");

    let root = temp.path().join("runs").join("retry-cancel");
    let report = replay_events(
        &read_events(root.join("journal.jsonl"))
            .await
            .expect("read events"),
    )
    .expect("replay");
    assert_eq!(summary.status, FinalStatus::Cancelled);
    assert_eq!(summary.nodes["retry"].status, NodeStatus::Cancelled);
    assert_eq!(report.final_status, Some(FinalStatus::Cancelled));
    assert_eq!(
        report
            .nodes
            .get("retry")
            .expect("retry node in replay")
            .status,
        NodeStatus::Cancelled
    );
    assert!(replay_journal(root.join("journal.jsonl")).await.is_ok());
}

#[tokio::test]
async fn mismatched_reported_model_fails_as_non_retryable_provider_protocol() {
    let temp = tempdir().expect("temp");
    let mut node = Node::agent("model_mismatch", "model mismatch");
    if let NodeKind::Agent { model, .. } = &mut node.kind {
        *model = Some("requested-model".to_owned());
    }
    node.retry.max_attempts = 3;
    node.retry.rebind_profiles = vec!["fallback-a".to_owned(), "fallback-b".to_owned()];
    let graph = Graph::new("model_mismatch", "model mismatch", vec![node]);
    let invoker = TestInvoker::new(vec![ScriptedInvocation {
        expect_prompt_fragment: Some("model mismatch".to_owned()),
        wait_for_release: false,
        invocation: Invocation::Raw {
            profile: "primary".to_owned(),
            output: AdapterOutput::Text("valid output".to_owned()),
            selected_model: Some("requested-model".to_owned()),
            reported_model: Some("different-model".to_owned()),
            reported_model_informational: false,
            stdout: String::new(),
            stderr: String::new(),
        },
    }]);

    let (runtime, options) = runtime_with(Arc::clone(&invoker), &temp);
    let summary = runtime.run(&graph, options).await.expect("run");
    let outcome = &summary.nodes["model_mismatch"];

    assert_eq!(outcome.status, NodeStatus::Failed);
    assert_eq!(outcome.attempts, 1);
    assert_eq!(invoker.call_count().await, 1);
    assert_eq!(
        node_failure_class(outcome),
        Some(NodeFailureClass::ProviderProtocol)
    );
    assert!(summary.models_used.iter().all(|usage| !usage.verified));
}

#[tokio::test]
async fn informational_cursor_reported_model_does_not_fail_provider_protocol() {
    let temp = tempdir().expect("temp");
    let mut node = Node::agent("cursor_model", "cursor model");
    if let NodeKind::Agent { model, .. } = &mut node.kind {
        *model = Some("gpt-5.6-luna-xhigh".to_owned());
    }
    let graph = Graph::new("cursor_model", "cursor model", vec![node]);
    let invoker = TestInvoker::new(vec![ScriptedInvocation {
        expect_prompt_fragment: Some("cursor model".to_owned()),
        wait_for_release: false,
        invocation: Invocation::Raw {
            profile: "cursor-agent".to_owned(),
            output: AdapterOutput::Text("valid output".to_owned()),
            selected_model: Some("gpt-5.6-luna-xhigh".to_owned()),
            reported_model: Some("GPT-5.6 Luna 272K Extra High".to_owned()),
            reported_model_informational: true,
            stdout: String::new(),
            stderr: String::new(),
        },
    }]);

    let (runtime, options) = runtime_with(Arc::clone(&invoker), &temp);
    let summary = runtime.run(&graph, options).await.expect("run");
    let outcome = &summary.nodes["cursor_model"];

    assert_eq!(outcome.status, NodeStatus::Succeeded);
    assert_eq!(invoker.call_count().await, 1);
    assert_eq!(summary.models_used.len(), 1);
    assert_eq!(summary.models_used[0].profile, "cursor-agent");
    assert_eq!(
        summary.models_used[0].reported_model.as_deref(),
        Some("gpt-5.6-luna-xhigh")
    );
    assert!(!summary.models_used[0].verified);
}

#[tokio::test]
async fn blank_provider_text_fails_once_even_with_rebinds() {
    let temp = tempdir().expect("temp");
    let mut node = Node::agent("blank", "blank response");
    node.retry.max_attempts = 3;
    node.retry.rebind_profiles = vec!["fallback-a".to_owned(), "fallback-b".to_owned()];
    let graph = Graph::new("blank", "blank", vec![node]);
    let invoker = TestInvoker::new(vec![ScriptedInvocation {
        expect_prompt_fragment: Some("blank response".to_owned()),
        wait_for_release: false,
        invocation: Invocation::Raw {
            profile: "primary".to_owned(),
            output: AdapterOutput::Text("  \n\t".to_owned()),
            selected_model: Some("model".to_owned()),
            reported_model: Some("model".to_owned()),
            reported_model_informational: false,
            stdout: String::new(),
            stderr: String::new(),
        },
    }]);

    let (runtime, options) = runtime_with(Arc::clone(&invoker), &temp);
    let summary = runtime.run(&graph, options).await.expect("run");
    let outcome = &summary.nodes["blank"];

    assert_eq!(outcome.status, NodeStatus::Failed);
    assert_eq!(outcome.attempts, 1);
    assert_eq!(invoker.call_count().await, 1);
    assert_eq!(
        node_failure_class(outcome),
        Some(NodeFailureClass::ProviderProtocol)
    );
}

#[tokio::test]
async fn provider_schema_validation_failure_is_non_retryable() {
    let temp = tempdir().expect("temp");
    let mut node = Node::agent("schema", "schema response");
    if let NodeKind::Agent { output, .. } = &mut node.kind {
        output.format = gloop_core::graph::OutputFormat::Json;
        output.inline_schema = Some(json!({"type": "integer"}));
    }
    node.retry.max_attempts = 3;
    node.retry.rebind_profiles = vec!["fallback-a".to_owned(), "fallback-b".to_owned()];
    let graph = Graph::new("schema", "schema", vec![node]);
    let invoker = TestInvoker::new(vec![ScriptedInvocation {
        expect_prompt_fragment: Some("schema response".to_owned()),
        wait_for_release: false,
        invocation: Invocation::Raw {
            profile: "primary".to_owned(),
            output: AdapterOutput::Json(json!("not an integer")),
            selected_model: Some("model".to_owned()),
            reported_model: Some("model".to_owned()),
            reported_model_informational: false,
            stdout: String::new(),
            stderr: String::new(),
        },
    }]);

    let (runtime, options) = runtime_with(Arc::clone(&invoker), &temp);
    let summary = runtime.run(&graph, options).await.expect("run");
    let outcome = &summary.nodes["schema"];

    assert_eq!(outcome.status, NodeStatus::Failed);
    assert_eq!(outcome.attempts, 1);
    assert_eq!(invoker.call_count().await, 1);
    assert_eq!(
        node_failure_class(outcome),
        Some(NodeFailureClass::ProviderProtocol)
    );
}

#[tokio::test]
async fn nested_subgraph_preserves_provider_failure_class() {
    let temp = tempdir().expect("temp");
    let mut inner = Node::agent("inner", "missing provider");
    if let NodeKind::Agent { profile, .. } = &mut inner.kind {
        *profile = Some("missing".to_owned());
    }
    let inner_graph = Graph::new("inner", "inner", vec![inner]);
    let mut outer = Node::agent("outer", "outer");
    outer.kind = NodeKind::Subgraph {
        graph: Box::new(inner_graph),
    };
    let graph = Graph::new("outer", "outer", vec![outer]);
    let invoker = TestInvoker::new(vec![ScriptedInvocation {
        expect_prompt_fragment: Some("missing provider".to_owned()),
        wait_for_release: false,
        invocation: Invocation::Err(AdapterError::ProfileNotFound("missing".to_owned())),
    }]);

    let (runtime, options) = runtime_with(invoker, &temp);
    let summary = runtime.run(&graph, options).await.expect("run");

    assert_eq!(summary.nodes["outer"].status, NodeStatus::Failed);
    assert_eq!(
        node_failure_class(&summary.nodes["outer"]),
        Some(NodeFailureClass::ProviderProfileNotFound)
    );
}

#[tokio::test]
async fn nested_subgraph_prefers_provider_failure_over_cancelled_sibling() {
    let temp = tempdir().expect("temp");
    let running = Node::command(
        "running",
        vec![
            "/bin/sh".to_owned(),
            "-lc".to_owned(),
            "sleep 20".to_owned(),
        ],
    );
    let mut failing = Node::agent("failing", "provider missing");
    if let NodeKind::Agent { profile, .. } = &mut failing.kind {
        *profile = Some("missing".to_owned());
    }
    let mut inner_graph = Graph::new("inner_cancel", "inner cancel", vec![running, failing]);
    inner_graph.spec.policies.max_parallel = 2;
    let mut outer = Node::agent("outer", "outer");
    outer.kind = NodeKind::Subgraph {
        graph: Box::new(inner_graph),
    };
    let graph = Graph::new(
        "outer_cancel_preference",
        "outer cancel preference",
        vec![outer],
    );
    let invoker = TestInvoker::new(vec![ScriptedInvocation {
        expect_prompt_fragment: Some("provider missing".to_owned()),
        wait_for_release: false,
        invocation: Invocation::Err(AdapterError::ProfileNotFound("missing".to_owned())),
    }]);

    let (runtime, options) = runtime_with(invoker, &temp);
    let summary = runtime.run(&graph, options).await.expect("run");

    assert_eq!(summary.nodes["outer"].status, NodeStatus::Failed);
    assert_eq!(
        node_failure_class(&summary.nodes["outer"]),
        Some(NodeFailureClass::ProviderProfileNotFound)
    );
}

#[tokio::test]
async fn retained_output_budget_is_global_across_multiple_succeeded_nodes() {
    let temp = tempdir().expect("temp");
    let large = "x".repeat(24 * 1024 * 1024);

    let mut first = Node::agent("first", "first");
    if let NodeKind::Agent { output, .. } = &mut first.kind {
        output.max_bytes = 30 * 1024 * 1024;
    }
    let mut second = Node::agent("second", "second");
    if let NodeKind::Agent { output, .. } = &mut second.kind {
        output.max_bytes = 30 * 1024 * 1024;
    }
    let mut third = Node::agent("third", "third");
    if let NodeKind::Agent { output, .. } = &mut third.kind {
        output.max_bytes = 30 * 1024 * 1024;
    }

    let graph = Graph::new(
        "retained_budget",
        "retained budget",
        vec![first, second, third],
    );

    let invoker = TestInvoker::new(vec![
        ScriptedInvocation {
            expect_prompt_fragment: Some("first".to_owned()),
            wait_for_release: false,
            invocation: Invocation::Raw {
                profile: "p".to_owned(),
                output: AdapterOutput::Text("ok".to_owned()),
                selected_model: Some("m".to_owned()),
                reported_model: None,
                reported_model_informational: false,
                stdout: large.clone(),
                stderr: String::new(),
            },
        },
        ScriptedInvocation {
            expect_prompt_fragment: Some("second".to_owned()),
            wait_for_release: false,
            invocation: Invocation::Raw {
                profile: "p".to_owned(),
                output: AdapterOutput::Text("ok".to_owned()),
                selected_model: Some("m".to_owned()),
                reported_model: None,
                reported_model_informational: false,
                stdout: String::new(),
                stderr: large.clone(),
            },
        },
        ScriptedInvocation {
            expect_prompt_fragment: Some("third".to_owned()),
            wait_for_release: false,
            invocation: Invocation::Raw {
                profile: "p".to_owned(),
                output: AdapterOutput::Text("ok".to_owned()),
                selected_model: Some("m".to_owned()),
                reported_model: None,
                reported_model_informational: false,
                stdout: large,
                stderr: String::new(),
            },
        },
    ]);

    let mut graph = graph;
    graph.spec.policies.max_parallel = 1;
    let (runtime, options) = runtime_with(Arc::clone(&invoker), &temp);
    let mut options = options;
    options.max_parallel = Some(1);
    let summary = runtime.run(&graph, options).await.expect("run");

    assert_eq!(summary.status, FinalStatus::BudgetExhausted);
    assert_eq!(summary.nodes["first"].status, NodeStatus::Succeeded);
    assert_eq!(summary.nodes["second"].status, NodeStatus::Succeeded);
    assert_eq!(summary.nodes["third"].status, NodeStatus::Cancelled);
    assert_eq!(summary.nodes["third"].attempts, 1);
    assert!(summary.nodes["third"].output.is_none());
    assert_eq!(
        node_failure_class(&summary.nodes["third"]),
        Some(NodeFailureClass::Budget)
    );
    assert!(summary.nodes["third"].output_artifact.is_none());
}

#[tokio::test]
async fn retained_output_budget_counts_failed_attempt_artifacts() {
    let temp = tempdir().expect("temp");
    let large_invalid_json = "x".repeat(34 * 1024 * 1024);

    let mut first = Node::agent("first_failure", "first failure");
    let mut second = Node::agent("second_failure", "second failure");
    for node in [&mut first, &mut second] {
        if let NodeKind::Agent { output, .. } = &mut node.kind {
            output.format = gloop_core::graph::OutputFormat::Json;
            output.max_bytes = 40 * 1024 * 1024;
        }
    }

    let mut graph = Graph::new(
        "failed_retained_budget",
        "failed retained budget",
        vec![first, second],
    );
    graph.spec.policies.max_parallel = 1;
    graph.spec.policies.failure = gloop_core::graph::FailurePolicy::Continue;

    let invoker = TestInvoker::new(vec![
        ScriptedInvocation {
            expect_prompt_fragment: Some("first failure".to_owned()),
            wait_for_release: false,
            invocation: Invocation::Ok {
                profile: "p".to_owned(),
                output: AdapterOutput::Text(large_invalid_json.clone()),
                model: Some("m".to_owned()),
            },
        },
        ScriptedInvocation {
            expect_prompt_fragment: Some("second failure".to_owned()),
            wait_for_release: false,
            invocation: Invocation::Ok {
                profile: "p".to_owned(),
                output: AdapterOutput::Text(large_invalid_json),
                model: Some("m".to_owned()),
            },
        },
    ]);

    let (runtime, mut options) = runtime_with(Arc::clone(&invoker), &temp);
    options.max_parallel = Some(1);
    let summary = runtime.run(&graph, options).await.expect("run");

    assert_eq!(summary.status, FinalStatus::BudgetExhausted);
    assert_eq!(summary.nodes["first_failure"].status, NodeStatus::Failed);
    assert!(summary.nodes["first_failure"].output_artifact.is_some());
    assert_eq!(
        summary.nodes["second_failure"].status,
        NodeStatus::Cancelled
    );
    assert_eq!(
        node_failure_class(&summary.nodes["second_failure"]),
        Some(NodeFailureClass::Budget)
    );
    assert!(summary.nodes["second_failure"].output_artifact.is_none());
}

#[tokio::test]
async fn summary_snapshot_is_compact_and_includes_final_node_status() {
    let temp = tempdir().expect("temp");
    let mut nodes = Vec::new();
    let mut invocations = Vec::new();
    for index in 0..65 {
        let id = format!("node_{index}");
        let mut node = Node::agent(&id, format!("snapshot {index}"));
        if let NodeKind::Agent { output, .. } = &mut node.kind {
            output.max_bytes = 1024;
        }
        nodes.push(node);
        invocations.push(ScriptedInvocation {
            expect_prompt_fragment: Some(format!("snapshot {index}")),
            wait_for_release: false,
            invocation: Invocation::Ok {
                profile: "p".to_owned(),
                output: AdapterOutput::Text("ok".to_owned()),
                model: Some("m".to_owned()),
            },
        });
    }

    let graph = Graph::new("snapshot_compact", "snapshot compact", nodes);
    let invoker = TestInvoker::new(invocations);
    let (runtime, mut options) = runtime_with(Arc::clone(&invoker), &temp);
    options.run_id = Some("snapshot-compact".to_owned());
    let summary = runtime.run(&graph, options).await.expect("run");

    assert_eq!(summary.status, FinalStatus::ReadyForHuman);
    assert_eq!(summary.nodes["node_64"].status, NodeStatus::Succeeded);

    let snapshot_path = temp
        .path()
        .join("runs")
        .join("snapshot-compact")
        .join("summary.snapshot.json");
    let snapshot = std::fs::read(snapshot_path).expect("snapshot file");
    let snapshot: Value = serde_json::from_slice(&snapshot).expect("snapshot json");

    let nodes = snapshot
        .get("nodes")
        .and_then(Value::as_object)
        .expect("snapshot has nodes");
    let final_node = nodes.get("node_64").expect("final node in snapshot");
    assert_eq!(
        final_node.get("status").and_then(Value::as_str),
        Some("succeeded")
    );
    assert!(final_node.get("output").is_none());
    assert!(final_node.get("output_artifact").is_some());
    assert_eq!(nodes.len(), 65);
}

#[tokio::test]
async fn provider_usage_summaries_are_sorted() {
    let temp = tempdir().expect("temp");
    let first = Node::agent("first", "first usage");
    let second = Node::agent("second", "second usage");
    let mut graph = Graph::new("usage_sort", "usage", vec![first, second]);
    graph.spec.edges = vec![Edge {
        from: "first".to_owned(),
        to: "second".to_owned(),
        kind: EdgeKind::Control,
        when: None,
    }];
    let invoker = TestInvoker::new(vec![
        ScriptedInvocation {
            expect_prompt_fragment: Some("first usage".to_owned()),
            wait_for_release: false,
            invocation: Invocation::Ok {
                profile: "zeta".to_owned(),
                output: AdapterOutput::Text("first".to_owned()),
                model: Some("model-z".to_owned()),
            },
        },
        ScriptedInvocation {
            expect_prompt_fragment: Some("second usage".to_owned()),
            wait_for_release: false,
            invocation: Invocation::Ok {
                profile: "alpha".to_owned(),
                output: AdapterOutput::Text("second".to_owned()),
                model: Some("model-a".to_owned()),
            },
        },
    ]);

    let (runtime, options) = runtime_with(invoker, &temp);
    let summary = runtime.run(&graph, options).await.expect("run");

    assert_eq!(summary.profiles_used, vec!["alpha", "zeta"]);
    assert_eq!(summary.models_used[0].profile, "alpha");
    assert_eq!(summary.models_used[1].profile, "zeta");
}

#[tokio::test]
async fn oversized_programmatic_parallelism_returns_an_error_without_panicking() {
    let temp = tempdir().expect("temp");
    let mut graph = Graph::new(
        "oversized_parallelism",
        "oversized parallelism",
        vec![Node::agent("node", "node")],
    );
    graph.spec.policies.max_parallel = usize::MAX;
    let invoker = TestInvoker::new(Vec::new());
    let (runtime, options) = runtime_with(invoker, &temp);

    assert!(runtime.run(&graph, options).await.is_err());
}

#[tokio::test]
async fn aggregate_fanout_prompt_limit_fails_before_provider_calls() {
    let temp = tempdir().expect("temp");
    let mut node = Node::agent("fanout_prompt", "x".repeat(60));
    node.context.max_bytes = 100;
    if let NodeKind::Agent { fan_out, .. } = &mut node.kind {
        *fan_out = 2;
    }
    node.retry.max_attempts = 2;
    node.retry.rebind_profiles = vec!["fallback".to_owned()];
    let graph = Graph::new("fanout_prompt", "fanout prompt", vec![node]);
    let invoker = TestInvoker::new(Vec::new());
    let (runtime, options) = runtime_with(Arc::clone(&invoker), &temp);

    let summary = runtime.run(&graph, options).await.expect("run");
    let outcome = &summary.nodes["fanout_prompt"];
    assert_eq!(outcome.status, NodeStatus::Failed);
    assert_eq!(outcome.attempts, 1);
    assert_eq!(invoker.call_count().await, 0);
    assert_eq!(
        node_failure_class(outcome),
        Some(NodeFailureClass::ProviderContextLength)
    );
}
