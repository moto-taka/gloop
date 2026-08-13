use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use gloop_core::{
    FinalStatus, Graph, Node, NodeStatus, RunSummary,
    graph::{Edge, EdgeCondition, EdgeKind, LoopCondition, NodeKind, OutputFormat, OutputSpec},
};
use gloop_provider::{
    AdapterCapabilities, AdapterError, AdapterOutput, AdapterRequest, AdapterResponse, ModelOrigin,
    SelectionOrigin, TokenUsage,
};
use gloop_runtime::{
    NodeFailureClass, ProviderInvocation, ProviderInvoker, RunOptions, Runtime, inspect_run,
    node_failure_class, read_events, read_journal, replay_events,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use tempfile::tempdir;
use tokio::{
    fs,
    sync::{Mutex, Notify},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
enum Invocation {
    Ok {
        profile: String,
        output: AdapterOutput,
        model: Option<String>,
    },
    Err(AdapterError),
}

#[derive(Debug)]
struct ScriptedInvocation {
    expect_prompt_fragment: Option<String>,
    wait_for_release: bool,
    invocation: Invocation,
}

#[derive(Debug, Clone)]
struct Call {
    profile: Option<String>,
}

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
            fail_if_unplanned: false,
        })
    }

    async fn wait_for_calls(&self, target: usize) {
        loop {
            let count = self.calls.lock().await.len();
            if count >= target {
                return;
            }
            self.started.notified().await;
        }
    }

    fn max_active(&self) -> usize {
        self.max_active.load(Ordering::SeqCst)
    }

    async fn call_count(&self) -> usize {
        self.calls.lock().await.len()
    }

    fn release_blocked(&self) {
        self.release.notify_waiters();
    }

    async fn call_profiles(&self) -> Vec<String> {
        let calls = self.calls.lock().await;
        calls
            .iter()
            .filter_map(|call| call.profile.clone())
            .collect()
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
        let prompt = request.prompt.clone();

        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        let mut observed = self.max_active.load(Ordering::SeqCst);
        while active > observed {
            match self.max_active.compare_exchange(
                observed,
                active,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(current) => observed = current,
            }
        }

        let plan = {
            let mut lock = self.planned.lock().await;
            if self.fail_if_unplanned && lock.is_empty() {
                return Err(AdapterError::InvalidRequest {
                    profile: preferred_profile.unwrap_or("default").to_owned(),
                    message: "unexpected provider invocation".to_owned(),
                });
            }

            let matching = lock
                .iter()
                .enumerate()
                .filter_map(|(index, entry)| {
                    entry
                        .expect_prompt_fragment
                        .as_ref()
                        .is_some_and(|fragment| request.prompt.contains(fragment))
                        .then_some(index)
                })
                .collect::<Vec<_>>();

            assert!(
                matching.len() <= 1 || active <= 1,
                "ambiguous provider invocation match for prompt: {prompt:?} matched {} planned entries",
                matching.len()
            );

            if let Some(index) = matching.first().copied() {
                lock.remove(index).expect("provider invocation planned")
            } else {
                lock.pop_front().expect("provider invocation planned")
            }
        };

        if let Some(fragment) = plan.expect_prompt_fragment {
            assert!(
                request.prompt.contains(&fragment),
                "provider call prompt mismatch: {prompt:?} does not contain {fragment:?}"
            );
        }

        self.calls.lock().await.push(Call {
            profile: preferred_profile.map(ToOwned::to_owned),
        });
        self.started.notify_waiters();

        if plan.wait_for_release {
            self.release.notified().await;
        }

        let result = match plan.invocation {
            Invocation::Ok {
                profile,
                output,
                model,
            } => {
                let response = AdapterResponse {
                    output,
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: Some(0),
                    reported_model: model.clone(),
                    reported_model_informational: false,
                    usage: Some(TokenUsage::default()),
                };
                Ok(ProviderInvocation {
                    profile,
                    selected_model: model,
                    selection_origin: if preferred_profile.is_some() {
                        SelectionOrigin::Explicit
                    } else {
                        SelectionOrigin::Capability
                    },
                    model_origin: ModelOrigin::ProviderDefault,
                    response,
                })
            }
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
        output.format = OutputFormat::Json;
    }
}

fn failing_command() -> Vec<String> {
    let executable = ["/usr/bin/false", "/bin/false"]
        .into_iter()
        .find(|candidate| std::path::Path::new(candidate).is_file())
        .expect("the test platform provides a false executable");
    vec![executable.to_owned()]
}

#[tokio::test]
async fn runtime_max_parallel_is_honored_for_independent_nodes() {
    let temp = tempdir().expect("temp");
    let invoker = TestInvoker::new(vec![
        ScriptedInvocation {
            expect_prompt_fragment: Some("a".to_owned()),
            wait_for_release: true,
            invocation: Invocation::Ok {
                profile: "primary".to_owned(),
                output: AdapterOutput::Text("a".to_owned()),
                model: Some("m".to_owned()),
            },
        },
        ScriptedInvocation {
            expect_prompt_fragment: Some("b".to_owned()),
            wait_for_release: true,
            invocation: Invocation::Ok {
                profile: "primary".to_owned(),
                output: AdapterOutput::Text("b".to_owned()),
                model: Some("m".to_owned()),
            },
        },
        ScriptedInvocation {
            expect_prompt_fragment: Some("c".to_owned()),
            wait_for_release: false,
            invocation: Invocation::Ok {
                profile: "primary".to_owned(),
                output: AdapterOutput::Text("c".to_owned()),
                model: Some("m".to_owned()),
            },
        },
    ]);

    let (runtime, mut options) = runtime_with(Arc::clone(&invoker), &temp);
    options.max_parallel = Some(2);
    let graph = Graph::new(
        "max_parallel",
        "parallel scheduling",
        vec![
            Node::agent("node-a", "a"),
            Node::agent("node-b", "b"),
            Node::agent("node-c", "c"),
        ],
    );

    let run = tokio::spawn(async move { runtime.run(&graph, options).await.expect("run") });
    invoker.wait_for_calls(2).await;
    assert_eq!(invoker.max_active(), 2);
    invoker.release_blocked();

    let summary = run.await.expect("run task");
    assert_eq!(summary.status, FinalStatus::ReadyForHuman);
}

#[tokio::test]
async fn runtime_serializes_nodes_with_shared_resources() {
    let temp = tempdir().expect("temp");
    let invoker = TestInvoker::new(vec![
        ScriptedInvocation {
            expect_prompt_fragment: Some("first".to_owned()),
            wait_for_release: false,
            invocation: Invocation::Ok {
                profile: "p".to_owned(),
                output: AdapterOutput::Text("first".to_owned()),
                model: Some("m".to_owned()),
            },
        },
        ScriptedInvocation {
            expect_prompt_fragment: Some("second".to_owned()),
            wait_for_release: false,
            invocation: Invocation::Ok {
                profile: "p".to_owned(),
                output: AdapterOutput::Text("second".to_owned()),
                model: Some("m".to_owned()),
            },
        },
    ]);

    let (runtime, mut options) = runtime_with(Arc::clone(&invoker), &temp);
    options.max_parallel = Some(4);

    let mut first = Node::agent("first", "first");
    first.resources.push("workspace".to_owned());
    let mut second = Node::agent("second", "second");
    second.resources.push("workspace".to_owned());
    let graph = Graph::new("resources", "resource serialization", vec![first, second]);

    let summary = runtime.run(&graph, options).await.expect("run");
    assert_eq!(summary.status, FinalStatus::ReadyForHuman);
    assert_eq!(invoker.max_active(), 1);
}

#[tokio::test]
async fn runtime_maintains_fan_out_output_order() {
    let temp = tempdir().expect("temp");
    let mut node = Node::agent("fan", "{{node_id}}");
    if let NodeKind::Agent {
        fan_out, output, ..
    } = &mut node.kind
    {
        *fan_out = 3;
        output.format = OutputFormat::Json;
    }

    let invoker = TestInvoker::new(vec![
        ScriptedInvocation {
            expect_prompt_fragment: Some("Fan-out candidate: 1/3".to_owned()),
            wait_for_release: false,
            invocation: Invocation::Ok {
                profile: "p".to_owned(),
                output: AdapterOutput::Json(json!("first")),
                model: Some("m".to_owned()),
            },
        },
        ScriptedInvocation {
            expect_prompt_fragment: Some("Fan-out candidate: 2/3".to_owned()),
            wait_for_release: true,
            invocation: Invocation::Ok {
                profile: "p".to_owned(),
                output: AdapterOutput::Json(json!("second")),
                model: Some("m".to_owned()),
            },
        },
        ScriptedInvocation {
            expect_prompt_fragment: Some("Fan-out candidate: 3/3".to_owned()),
            wait_for_release: false,
            invocation: Invocation::Ok {
                profile: "p".to_owned(),
                output: AdapterOutput::Json(json!("third")),
                model: Some("m".to_owned()),
            },
        },
    ]);

    let (runtime, mut options) = runtime_with(Arc::clone(&invoker), &temp);
    options.max_parallel = Some(3);
    let graph = Graph::new("fan", "fan out", vec![node]);

    let handle: JoinHandle<RunSummary> =
        tokio::spawn(async move { runtime.run(&graph, options).await.expect("run") });
    invoker.wait_for_calls(2).await;
    invoker.release_blocked();
    let summary = handle.await.expect("run complete");

    let output = summary.nodes["fan"].output.as_ref().expect("fan output");
    let values = output.as_array().expect("fan output should be an array");
    assert_eq!(
        values,
        &vec![json!("first"), json!("second"), json!("third")]
    );
}

#[tokio::test]
async fn runtime_conditional_edges_pick_the_expected_branch() {
    let temp = tempdir().expect("temp");
    let mut source = Node::agent("source", "{{node_id}}");
    json_output(&mut source);

    let mut yes = Node::agent("yes", "yes");
    json_output(&mut yes);
    let mut no = Node::agent("no", "no");
    json_output(&mut no);

    let mut graph = Graph::new(
        "conditions",
        "conditionals",
        vec![source.clone(), yes.clone(), no.clone()],
    );
    graph.spec.edges = vec![
        Edge {
            from: "source".to_owned(),
            to: "yes".to_owned(),
            kind: EdgeKind::Conditional,
            when: Some(EdgeCondition {
                status: Some(NodeStatus::Succeeded),
                output_contains: None,
                json_pointer: Some("/route".to_owned()),
                equals: Some(json!("yes")),
            }),
        },
        Edge {
            from: "source".to_owned(),
            to: "no".to_owned(),
            kind: EdgeKind::Conditional,
            when: Some(EdgeCondition {
                status: Some(NodeStatus::Succeeded),
                output_contains: None,
                json_pointer: Some("/route".to_owned()),
                equals: Some(json!("no")),
            }),
        },
    ];

    let invoker = TestInvoker::new(vec![
        ScriptedInvocation {
            expect_prompt_fragment: Some("source".to_owned()),
            wait_for_release: false,
            invocation: Invocation::Ok {
                profile: "p".to_owned(),
                output: AdapterOutput::Json(json!({"route": "yes"})),
                model: Some("m".to_owned()),
            },
        },
        ScriptedInvocation {
            expect_prompt_fragment: Some("yes".to_owned()),
            wait_for_release: false,
            invocation: Invocation::Ok {
                profile: "p".to_owned(),
                output: AdapterOutput::Json(json!("ok")),
                model: Some("m".to_owned()),
            },
        },
        ScriptedInvocation {
            expect_prompt_fragment: Some("no".to_owned()),
            wait_for_release: false,
            invocation: Invocation::Ok {
                profile: "p".to_owned(),
                output: AdapterOutput::Json(json!("bad")),
                model: Some("m".to_owned()),
            },
        },
    ]);

    let (runtime, options) = runtime_with(Arc::clone(&invoker), &temp);
    let summary = runtime.run(&graph, options).await.expect("run");
    assert_eq!(summary.status, FinalStatus::ReadyForHuman);
    assert_eq!(summary.nodes["yes"].status, NodeStatus::Succeeded);
    assert_eq!(summary.nodes["no"].status, NodeStatus::Skipped);
}

#[tokio::test]
async fn runtime_failure_edges_run_when_source_fails() {
    let temp = tempdir().expect("temp");
    let fail_node = Node::agent("source", "source");
    let mut fallback = Node::agent("fallback", "fallback");
    json_output(&mut fallback);
    let mut skipped = Node::agent("skip", "skip");
    json_output(&mut skipped);
    let mut graph = Graph::new(
        "failure",
        "failure edge",
        vec![fail_node, fallback, skipped],
    );
    graph.spec.edges = vec![
        Edge {
            from: "source".to_owned(),
            to: "skip".to_owned(),
            kind: EdgeKind::Data,
            when: None,
        },
        Edge {
            from: "source".to_owned(),
            to: "fallback".to_owned(),
            kind: EdgeKind::Failure,
            when: None,
        },
    ];

    let invoker = TestInvoker::new(vec![
        ScriptedInvocation {
            expect_prompt_fragment: Some("source".to_owned()),
            wait_for_release: false,
            invocation: Invocation::Err(AdapterError::Timeout {
                profile: "p".to_owned(),
                timeout_ms: 1,
                retryable: false,
            }),
        },
        ScriptedInvocation {
            expect_prompt_fragment: Some("\"status\": \"failed\"".to_owned()),
            wait_for_release: false,
            invocation: Invocation::Ok {
                profile: "p".to_owned(),
                output: AdapterOutput::Json(json!("ok")),
                model: Some("m".to_owned()),
            },
        },
        ScriptedInvocation {
            expect_prompt_fragment: Some("skip".to_owned()),
            wait_for_release: false,
            invocation: Invocation::Ok {
                profile: "p".to_owned(),
                output: AdapterOutput::Text("unexpected".to_owned()),
                model: Some("m".to_owned()),
            },
        },
    ]);

    let (runtime, mut options) = runtime_with(Arc::clone(&invoker), &temp);
    options.run_id = Some("failure-edges".to_owned());
    let summary = runtime.run(&graph, options).await.expect("run");
    assert_eq!(summary.status, FinalStatus::Failed);
    assert_eq!(summary.nodes["fallback"].status, NodeStatus::Succeeded);
    assert_eq!(summary.nodes["skip"].status, NodeStatus::Skipped);
}

#[tokio::test]
async fn runtime_retries_rejected_rate_limit_and_stops_for_permanent_error() {
    let temp = tempdir().expect("temp");
    let mut transient = Node::agent("retry", "retry");
    transient.retry.max_attempts = 2;
    let mut graph = Graph::new("retry", "retry", vec![transient]);
    graph.spec.nodes[0].retry.max_attempts = 2;

    let invoker = TestInvoker::new(vec![
        ScriptedInvocation {
            expect_prompt_fragment: Some("retry".to_owned()),
            wait_for_release: false,
            invocation: Invocation::Err(AdapterError::HttpStatus {
                profile: "p".to_owned(),
                status: 429,
                error_type: Some("rate_limit".to_owned()),
                error_code: None,
            }),
        },
        ScriptedInvocation {
            expect_prompt_fragment: Some("retry".to_owned()),
            wait_for_release: false,
            invocation: Invocation::Ok {
                profile: "p".to_owned(),
                output: AdapterOutput::Text("ok".to_owned()),
                model: Some("m".to_owned()),
            },
        },
    ]);

    let (runtime, options) = runtime_with(Arc::clone(&invoker), &temp);
    let summary = runtime.run(&graph, options).await.expect("run");
    assert_eq!(summary.nodes["retry"].attempts, 2);
    assert_eq!(summary.nodes["retry"].status, NodeStatus::Succeeded);
    assert_eq!(summary.nodes["retry"].profile.as_deref(), Some("p"));

    let mut permanent = Node::agent("permanent", "permanent");
    permanent.retry.max_attempts = 1;
    let perm_graph = Graph::new("permanent", "permanent", vec![permanent]);
    let permanent_invoker = TestInvoker::new(vec![ScriptedInvocation {
        expect_prompt_fragment: Some("permanent".to_owned()),
        wait_for_release: false,
        invocation: Invocation::Err(AdapterError::InvalidRequest {
            profile: "p".to_owned(),
            message: "permanent".to_owned(),
        }),
    }]);

    let (runtime, options) = runtime_with(Arc::clone(&permanent_invoker), &temp);
    let summary = runtime.run(&perm_graph, options).await.expect("run");
    assert_eq!(summary.nodes["permanent"].attempts, 1);
    assert_eq!(summary.nodes["permanent"].status, NodeStatus::Failed);
    assert_eq!(
        node_failure_class(&summary.nodes["permanent"]),
        Some(NodeFailureClass::ProviderConfiguration)
    );
}

#[tokio::test]
async fn runtime_rebind_profiles_follow_expected_order() {
    let temp = tempdir().expect("temp");
    let mut node = Node::agent("rebind", "rebind");
    node.retry.max_attempts = 3;
    node.retry.rebind_profiles = vec!["secondary".to_owned(), "tertiary".to_owned()];
    if let NodeKind::Agent { profile, .. } = &mut node.kind {
        *profile = Some("primary".to_owned());
    }

    let graph = Graph::new("rebind", "profile", vec![node]);
    let invoker = TestInvoker::new(vec![
        ScriptedInvocation {
            expect_prompt_fragment: Some("rebind".to_owned()),
            wait_for_release: false,
            invocation: Invocation::Err(AdapterError::ProfileNotFound("primary".to_owned())),
        },
        ScriptedInvocation {
            expect_prompt_fragment: Some("rebind".to_owned()),
            wait_for_release: false,
            invocation: Invocation::Err(AdapterError::ProfileNotFound("secondary".to_owned())),
        },
        ScriptedInvocation {
            expect_prompt_fragment: Some("rebind".to_owned()),
            wait_for_release: false,
            invocation: Invocation::Ok {
                profile: "tertiary".to_owned(),
                output: AdapterOutput::Text("done".to_owned()),
                model: Some("m".to_owned()),
            },
        },
    ]);

    let (runtime, options) = runtime_with(Arc::clone(&invoker), &temp);
    let summary = runtime.run(&graph, options).await.expect("run");
    let calls = invoker.call_profiles().await;
    assert_eq!(calls, vec!["primary", "secondary", "tertiary"]);
    assert_eq!(summary.nodes["rebind"].attempts, 3);
    assert_eq!(summary.nodes["rebind"].status, NodeStatus::Succeeded);
    assert_eq!(summary.nodes["rebind"].profile.as_deref(), Some("tertiary"));
}

#[tokio::test]
async fn runtime_loop_stops_on_success_or_stagnation_or_bound() {
    let temp = tempdir().expect("temp");

    let mut good_condition_node = Node::agent("probe", "probe");
    json_output(&mut good_condition_node);
    let success_nested = Graph::new("nested", "nested", vec![good_condition_node.clone()]);

    let mut loop_node = Node::agent("loop", "loop");
    let loop_condition = LoopCondition {
        node: "probe".to_owned(),
        status: NodeStatus::Succeeded,
        output_contains: None,
        json_pointer: Some("/done".to_owned()),
        equals: Some(json!(true)),
    };
    loop_node.kind = NodeKind::Loop {
        graph: Box::new(success_nested.clone()),
        until: loop_condition,
        max_iterations: 3,
        stagnation_after: 2,
    };

    let invoker = TestInvoker::new(vec![
        ScriptedInvocation {
            expect_prompt_fragment: Some("probe".to_owned()),
            wait_for_release: false,
            invocation: Invocation::Ok {
                profile: "p".to_owned(),
                output: AdapterOutput::Json(json!({"done": false})),
                model: Some("m".to_owned()),
            },
        },
        ScriptedInvocation {
            expect_prompt_fragment: Some("probe".to_owned()),
            wait_for_release: false,
            invocation: Invocation::Ok {
                profile: "p".to_owned(),
                output: AdapterOutput::Json(json!({"done": true})),
                model: Some("m".to_owned()),
            },
        },
        ScriptedInvocation {
            expect_prompt_fragment: Some("probe".to_owned()),
            wait_for_release: false,
            invocation: Invocation::Ok {
                profile: "p".to_owned(),
                output: AdapterOutput::Json(json!({"done": false})),
                model: Some("m".to_owned()),
            },
        },
        ScriptedInvocation {
            expect_prompt_fragment: Some("probe".to_owned()),
            wait_for_release: false,
            invocation: Invocation::Ok {
                profile: "p".to_owned(),
                output: AdapterOutput::Json(json!({"done": false})),
                model: Some("m".to_owned()),
            },
        },
    ]);
    let (runtime, mut options) = runtime_with(Arc::clone(&invoker), &temp);
    options.run_id = Some("loop-success".to_owned());
    let graph = Graph::new("loop_success", "loop", vec![loop_node.clone()]);
    let success_summary = runtime.run(&graph, options.clone()).await.expect("run");
    assert_eq!(success_summary.status, FinalStatus::ReadyForHuman);
    let success_output = success_summary.nodes["loop"]
        .output
        .as_ref()
        .expect("loop output");
    assert_eq!(success_output.get("iterations"), Some(&json!(2)));

    let mut fail_loop_node = loop_node;
    fail_loop_node.kind = NodeKind::Loop {
        graph: Box::new(success_nested.clone()),
        until: LoopCondition {
            node: "probe".to_owned(),
            status: NodeStatus::Succeeded,
            output_contains: None,
            json_pointer: Some("/done".to_owned()),
            equals: Some(json!(true)),
        },
        max_iterations: 2,
        stagnation_after: 2,
    };

    let fail_graph = Graph::new("loop_fail", "loop", vec![fail_loop_node]);

    let (runtime, mut options2) = runtime_with(Arc::clone(&invoker), &temp);
    options2.run_id = Some("loop-fail".to_owned());
    let fail_summary = runtime.run(&fail_graph, options2).await.expect("run");
    assert_eq!(fail_summary.nodes["loop"].status, NodeStatus::Failed);
    assert_eq!(fail_summary.status, FinalStatus::Failed);
}

#[tokio::test]
async fn runtime_loop_stops_on_cancellation_before_max_iterations() {
    let temp = tempdir().expect("temp");
    let cancellation_token = CancellationToken::new();
    let invoker = TestInvoker::new(vec![
        ScriptedInvocation {
            expect_prompt_fragment: Some("probe".to_owned()),
            wait_for_release: true,
            invocation: Invocation::Ok {
                profile: "p".to_owned(),
                output: AdapterOutput::Json(json!({"done": false})),
                model: Some("m".to_owned()),
            },
        },
        ScriptedInvocation {
            expect_prompt_fragment: Some("probe".to_owned()),
            wait_for_release: false,
            invocation: Invocation::Ok {
                profile: "p".to_owned(),
                output: AdapterOutput::Json(json!({"done": false})),
                model: Some("m".to_owned()),
            },
        },
        ScriptedInvocation {
            expect_prompt_fragment: Some("probe".to_owned()),
            wait_for_release: false,
            invocation: Invocation::Ok {
                profile: "p".to_owned(),
                output: AdapterOutput::Json(json!({"done": false})),
                model: Some("m".to_owned()),
            },
        },
    ]);

    let (runtime, mut options) = runtime_with(Arc::clone(&invoker), &temp);
    options.run_id = Some("loop-cancel".to_owned());
    options.cancellation = cancellation_token.clone();
    let mut body_probe = Node::agent("probe", "probe");
    if let NodeKind::Agent { output, .. } = &mut body_probe.kind {
        output.format = OutputFormat::Json;
        output.inline_schema =
            Some(json!({"type": "object", "properties": {"done": {"type": "boolean"}}}));
    }
    let loop_body = Graph::new("loop-body", "loop body", vec![body_probe]);
    let loop_node = Node::agent("loop", "loop");
    let mut loop_node = loop_node;
    loop_node.kind = NodeKind::Loop {
        graph: Box::new(loop_body),
        until: LoopCondition {
            node: "probe".to_owned(),
            status: NodeStatus::Succeeded,
            output_contains: None,
            json_pointer: Some("/done".to_owned()),
            equals: Some(json!(true)),
        },
        max_iterations: 3,
        stagnation_after: 3,
    };

    let graph = Graph::new("loop-cancel", "loop cancel", vec![loop_node]);
    let run = tokio::spawn(async move { runtime.run(&graph, options).await.expect("run") });
    invoker.wait_for_calls(1).await;
    cancellation_token.cancel();
    invoker.release_blocked();
    let summary = run.await.expect("run task");

    assert_eq!(summary.status, FinalStatus::Cancelled);
    assert_eq!(summary.nodes["loop"].status, NodeStatus::Cancelled);
    assert_eq!(
        node_failure_class(&summary.nodes["loop"]),
        Some(NodeFailureClass::Cancelled)
    );
    assert_eq!(invoker.call_count().await, 1);
}

#[tokio::test]
async fn runtime_maps_command_and_verify_failures_to_expected_status() {
    let temp = tempdir().expect("temp");

    let command_fail = Node::command("cmd", failing_command());
    let command_graph = Graph::new("command", "fail", vec![command_fail]);
    let command_invoker = TestInvoker::new(vec![]);
    let (runtime, options) = runtime_with(Arc::clone(&command_invoker), &temp);
    let command_summary = runtime.run(&command_graph, options).await.expect("run");
    assert_eq!(command_summary.status, FinalStatus::Failed);

    let mut verify_fail = Node::command("v", vec!["/definitely-missing-verify-command".to_owned()]);
    verify_fail.kind = NodeKind::Verify {
        argv: vec!["/definitely-missing-verify-command".to_owned()],
        env: indexmap::IndexMap::new(),
        output: OutputSpec::default(),
    };
    let verify_graph = Graph::new("verify", "fail", vec![verify_fail]);
    let verify_invoker = TestInvoker::new(vec![]);
    let (runtime, options) = runtime_with(Arc::clone(&verify_invoker), &temp);
    let verify_summary = runtime.run(&verify_graph, options).await.expect("run");
    assert_eq!(verify_summary.status, FinalStatus::VerificationFailed);
    assert_eq!(
        node_failure_class(&verify_summary.nodes["v"]),
        Some(NodeFailureClass::Verification)
    );

    let mut nested_verify = Node::command("nested_verify", failing_command());
    nested_verify.kind = NodeKind::Verify {
        argv: failing_command(),
        env: indexmap::IndexMap::new(),
        output: OutputSpec::default(),
    };
    let nested_graph = Graph::new("nested", "nested verify", vec![nested_verify]);
    let mut subgraph = Node::agent("subgraph", "subgraph");
    subgraph.kind = NodeKind::Subgraph {
        graph: Box::new(nested_graph),
    };
    let subgraph_graph = Graph::new("nested-verify", "nested verify", vec![subgraph]);
    let nested_invoker = TestInvoker::new(vec![]);
    let (runtime, options) = runtime_with(Arc::clone(&nested_invoker), &temp);
    let nested_summary = runtime
        .run(&subgraph_graph, options)
        .await
        .expect("run nested verify");
    assert_eq!(nested_summary.status, FinalStatus::VerificationFailed);
    assert_eq!(
        node_failure_class(&nested_summary.nodes["subgraph"]),
        Some(NodeFailureClass::Verification)
    );

    let mut loop_verify = Node::command("loop_verify", failing_command());
    loop_verify.kind = NodeKind::Verify {
        argv: failing_command(),
        env: indexmap::IndexMap::new(),
        output: OutputSpec::default(),
    };
    let loop_body = Graph::new("loop-body", "loop verify", vec![loop_verify]);
    let mut loop_node = Node::agent("loop", "loop");
    loop_node.kind = NodeKind::Loop {
        graph: Box::new(loop_body),
        until: LoopCondition {
            node: "loop_verify".to_owned(),
            status: NodeStatus::Succeeded,
            output_contains: None,
            json_pointer: None,
            equals: None,
        },
        max_iterations: 1,
        stagnation_after: 1,
    };
    let loop_graph = Graph::new("loop-verify", "loop verify", vec![loop_node]);
    let loop_invoker = TestInvoker::new(vec![]);
    let (runtime, options) = runtime_with(Arc::clone(&loop_invoker), &temp);
    let loop_summary = runtime
        .run(&loop_graph, options)
        .await
        .expect("run loop verify");
    assert_eq!(loop_summary.status, FinalStatus::VerificationFailed);
    assert_eq!(
        node_failure_class(&loop_summary.nodes["loop"]),
        Some(NodeFailureClass::Verification)
    );
}

#[tokio::test]
async fn runtime_validates_json_output_against_schema() {
    let temp = tempdir().expect("temp");
    let mut valid_node = Node::agent("validator", "validator");
    if let NodeKind::Agent { output, .. } = &mut valid_node.kind {
        output.format = OutputFormat::Json;
        output.inline_schema = Some(
            json!({"type": "object", "required": ["value"], "properties": {"value": {"type": "string"}}}),
        );
    }

    let schema_graph = Graph::new("schema", "schema", vec![valid_node.clone()]);
    let invoker = TestInvoker::new(vec![ScriptedInvocation {
        expect_prompt_fragment: Some("validator".to_owned()),
        wait_for_release: false,
        invocation: Invocation::Ok {
            profile: "p".to_owned(),
            output: AdapterOutput::Json(json!({"value": "ok"})),
            model: Some("m".to_owned()),
        },
    }]);
    let (runtime, options) = runtime_with(Arc::clone(&invoker), &temp);
    let summary = runtime.run(&schema_graph, options).await.expect("run");
    assert_eq!(summary.status, FinalStatus::ReadyForHuman);

    let mut invalid_node = valid_node;
    if let NodeKind::Agent { output, .. } = &mut invalid_node.kind {
        output.schema = None;
    }
    let invalid_graph = Graph::new("schema_invalid", "schema", vec![invalid_node]);
    let invalid_invoker = TestInvoker::new(vec![ScriptedInvocation {
        expect_prompt_fragment: Some("validator".to_owned()),
        wait_for_release: false,
        invocation: Invocation::Ok {
            profile: "p".to_owned(),
            output: AdapterOutput::Json(json!({"value": 123})),
            model: Some("m".to_owned()),
        },
    }]);

    let (runtime, options) = runtime_with(Arc::clone(&invalid_invoker), &temp);
    let summary = runtime.run(&invalid_graph, options).await.expect("run");
    assert_eq!(summary.status, FinalStatus::Failed);
    assert_eq!(summary.nodes["validator"].status, NodeStatus::Failed);
}

#[tokio::test]
async fn runtime_respects_cancellation_wall_budget_and_model_call_budget() {
    let temp = tempdir().expect("temp");
    let cancellation_token = CancellationToken::new();
    let cancel_invoker = TestInvoker::new(vec![ScriptedInvocation {
        expect_prompt_fragment: Some("cancel".to_owned()),
        wait_for_release: true,
        invocation: Invocation::Ok {
            profile: "p".to_owned(),
            output: AdapterOutput::Text("done".to_owned()),
            model: Some("m".to_owned()),
        },
    }]);

    let (runtime, mut options) = runtime_with(Arc::clone(&cancel_invoker), &temp);
    options.run_id = Some("cancellation".to_owned());
    options.cancellation = cancellation_token.clone();
    let node = Node::agent("cancel", "cancel");
    let graph = Graph::new("cancel", "cancel", vec![node]);
    let run = tokio::spawn(async move { runtime.run(&graph, options).await.expect("run") });
    cancel_invoker.wait_for_calls(1).await;
    cancellation_token.cancel();
    cancel_invoker.release_blocked();
    let summary = run.await.expect("task");
    assert_eq!(summary.status, FinalStatus::Cancelled);

    let wall_invoker = TestInvoker::new(vec![ScriptedInvocation {
        expect_prompt_fragment: Some("wall".to_owned()),
        wait_for_release: false,
        invocation: Invocation::Ok {
            profile: "p".to_owned(),
            output: AdapterOutput::Text("ok".to_owned()),
            model: Some("m".to_owned()),
        },
    }]);
    let (runtime, mut options) = runtime_with(Arc::clone(&wall_invoker), &temp);
    options.run_id = Some("wall".to_owned());
    options.wall_time = Some(Duration::from_nanos(0));
    let wall_summary = runtime
        .run(
            &Graph::new("wall", "wall", vec![Node::agent("wall", "wall")]),
            options,
        )
        .await
        .expect("run");
    assert_eq!(wall_summary.status, FinalStatus::BudgetExhausted);
    assert_eq!(wall_invoker.call_count().await, 0);

    let budget_invoker = TestInvoker::new(vec![
        ScriptedInvocation {
            expect_prompt_fragment: None,
            wait_for_release: false,
            invocation: Invocation::Ok {
                profile: "p".to_owned(),
                output: AdapterOutput::Text("first".to_owned()),
                model: Some("m".to_owned()),
            },
        },
        ScriptedInvocation {
            expect_prompt_fragment: None,
            wait_for_release: false,
            invocation: Invocation::Ok {
                profile: "p".to_owned(),
                output: AdapterOutput::Text("second".to_owned()),
                model: Some("m".to_owned()),
            },
        },
    ]);
    let (runtime, mut options) = runtime_with(Arc::clone(&budget_invoker), &temp);
    options.run_id = Some("model-budget".to_owned());
    options.model_calls = Some(1);

    let budget_graph = Graph::new(
        "model_budget",
        "budget",
        vec![
            Node::agent("budget-a", "budget-a"),
            Node::agent("budget-b", "budget-b"),
        ],
    );
    let budget_summary = runtime.run(&budget_graph, options).await.expect("run");
    assert_eq!(budget_summary.status, FinalStatus::BudgetExhausted);
    assert_eq!(budget_invoker.call_count().await, 1);
}

#[tokio::test]
async fn runtime_artifacts_journal_and_summary_are_replayable_and_tamper_detectable() {
    let temp = tempdir().expect("temp");
    let run_id = "replayable";
    let invoker = TestInvoker::new(vec![ScriptedInvocation {
        expect_prompt_fragment: Some("artifact".to_owned()),
        wait_for_release: false,
        invocation: Invocation::Ok {
            profile: "p".to_owned(),
            output: AdapterOutput::Json(json!({"answer": 42})),
            model: Some("m".to_owned()),
        },
    }]);

    let (runtime, mut options) = runtime_with(Arc::clone(&invoker), &temp);
    options.run_id = Some(run_id.to_owned());
    let mut artifact_node = Node::agent("artifact", "artifact");
    json_output(&mut artifact_node);
    let graph = Graph::new("replayable", "replayable", vec![artifact_node]);
    let summary = runtime.run(&graph, options).await.expect("run");

    let root = temp.path().join("runs").join(run_id);
    let journal_path = root.join("journal.jsonl");
    let summary_path = root.join("summary.json");
    let graph_path = root.join("graph.json");

    assert!(journal_path.exists());
    assert!(summary_path.exists());
    assert!(graph_path.exists());

    let replay = replay_events(&read_events(journal_path.clone()).await.unwrap()).unwrap();
    assert_eq!(replay.run_id, run_id);
    assert!(replay.final_status.is_some());
    assert!(read_journal(journal_path.clone()).await.is_ok());

    let inspected = inspect_run(&root).await.expect("inspect");
    assert_eq!(inspected.summary.run_id, summary.run_id);
    assert_eq!(inspected.summary.status, FinalStatus::ReadyForHuman);

    for artifact in &summary.artifacts {
        let artifact_path = root.join(&artifact.path);
        let bytes = fs::read(&artifact_path).await.expect("artifact exists");
        if let Some(expected) = artifact.sha256.as_ref() {
            let actual = hex::encode(Sha256::digest(&bytes));
            assert_eq!(actual, *expected);
        }
        assert!(artifact_path.exists());
    }

    let mut tampered = fs::read(&journal_path).await.expect("read journal");
    let run_id_offset = tampered
        .windows(run_id.len())
        .position(|window| window == run_id.as_bytes())
        .expect("journal includes run id");
    tampered[run_id_offset] = b'R';
    let tampered_path = root.join("journal.tampered");
    fs::write(&tampered_path, tampered)
        .await
        .expect("write tampered");
    assert!(read_journal(tampered_path).await.is_err());
}
