use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use gloop_core::{FinalStatus, Node, NodeKind, NodeStatus};
use gloop_provider::{
    AdapterCapabilities, AdapterError, AdapterOutput, AdapterRequest, AdapterResponse, ModelOrigin,
    SelectionOrigin, TokenUsage,
};
use gloop_runtime::{ProviderInvocation, ProviderInvoker, RunOptions, Runtime};
use tempfile::tempdir;
use tokio::sync::{Mutex, Semaphore};
use tokio::time::timeout;
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

#[derive(Debug)]
struct TestInvoker {
    planned: Mutex<Vec<ScriptedInvocation>>,
    call_count: AtomicUsize,
    max_active: AtomicUsize,
    active: AtomicUsize,
    canceled_remaining: AtomicUsize,
}

impl TestInvoker {
    fn new(planned: Vec<ScriptedInvocation>) -> Arc<Self> {
        Arc::new(Self {
            planned: Mutex::new(planned),
            call_count: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            canceled_remaining: AtomicUsize::new(0),
        })
    }

    fn call_count(&self) -> usize {
        self.call_count.load(Ordering::SeqCst)
    }

    fn max_active(&self) -> usize {
        self.max_active.load(Ordering::SeqCst)
    }

    fn active(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }

    fn canceled_remaining(&self) -> usize {
        self.canceled_remaining.load(Ordering::SeqCst)
    }

    async fn wait_for_call_count(&self, target: usize) {
        loop {
            if self.call_count() >= target {
                return;
            }
            tokio::task::yield_now().await;
        }
    }
}

#[derive(Debug)]
struct SerializedStartInvoker {
    planned: Mutex<Vec<ScriptedInvocation>>,
    start_permits: Semaphore,
    finish_permits: Semaphore,
    call_count: AtomicUsize,
    max_active: AtomicUsize,
    active: AtomicUsize,
}

impl SerializedStartInvoker {
    fn new(planned: Vec<ScriptedInvocation>) -> Arc<Self> {
        Arc::new(Self {
            planned: Mutex::new(planned),
            start_permits: Semaphore::new(0),
            finish_permits: Semaphore::new(0),
            call_count: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
        })
    }

    fn call_count(&self) -> usize {
        self.call_count.load(Ordering::SeqCst)
    }

    fn max_active(&self) -> usize {
        self.max_active.load(Ordering::SeqCst)
    }

    fn release_next_start(&self) {
        self.start_permits.add_permits(1);
    }

    fn release_running_call(&self) {
        self.finish_permits.add_permits(1);
    }

    async fn wait_for_call_count(&self, target: usize) {
        loop {
            if self.call_count() >= target {
                return;
            }
            tokio::task::yield_now().await;
        }
    }
}

#[async_trait]
impl ProviderInvoker for SerializedStartInvoker {
    async fn execute(
        &self,
        preferred_profile: Option<&str>,
        _required: &AdapterCapabilities,
        request: AdapterRequest,
        _cancellation: CancellationToken,
    ) -> Result<ProviderInvocation, AdapterError> {
        let _start = self.start_permits.acquire().await.expect("start permits");
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        let _active = ActiveCall {
            active: &self.active,
            max_active: &self.max_active,
        };
        _active.update_max(active);
        self.call_count.fetch_add(1, Ordering::SeqCst);

        let mut planned = self.planned.lock().await;
        let plan = planned.pop().expect("provider invocation planned");
        drop(planned);

        if let Some(fragment) = plan.expect_prompt_fragment {
            assert!(
                request.prompt.contains(&fragment),
                "prompt mismatch: {fragment:?}"
            );
        }

        if plan.wait_for_release {
            let _ = self.finish_permits.acquire().await.expect("finish permits");
        }

        match plan.invocation {
            Invocation::Err(error) => Err(error),
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
                    reported_model: model,
                    reported_model_informational: false,
                    usage: Some(TokenUsage::default()),
                };
                Ok(ProviderInvocation {
                    profile,
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
    }
}

#[async_trait]
impl ProviderInvoker for TestInvoker {
    async fn execute(
        &self,
        preferred_profile: Option<&str>,
        _required: &AdapterCapabilities,
        request: AdapterRequest,
        cancellation: CancellationToken,
    ) -> Result<ProviderInvocation, AdapterError> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        let _active = ActiveCall {
            active: &self.active,
            max_active: &self.max_active,
        };
        _active.update_max(active);
        self.call_count.fetch_add(1, Ordering::SeqCst);

        let mut planned = self.planned.lock().await;
        let plan = planned.pop().expect("provider invocation planned");
        drop(planned);

        if let Some(fragment) = plan.expect_prompt_fragment {
            assert!(
                request.prompt.contains(&fragment),
                "prompt mismatch: {fragment:?}"
            );
        }

        if plan.wait_for_release {
            tokio::select! {
                () = cancellation.cancelled() => {
                    self.canceled_remaining.fetch_add(1, Ordering::SeqCst);
                    return Err(AdapterError::Timeout {
                        profile: preferred_profile.unwrap_or("default").to_owned(),
                        timeout_ms: 0,
                        retryable: false,
                    });
                }
                () = tokio::time::sleep(Duration::from_secs(30)) => {
                    return Err(AdapterError::Timeout {
                        profile: preferred_profile.unwrap_or("default").to_owned(),
                        timeout_ms: 30_000,
                        retryable: false,
                    });
                }
            }
        }

        match plan.invocation {
            Invocation::Err(error) => {
                while self.call_count() < 3 {
                    tokio::task::yield_now().await;
                }
                Err(error)
            }
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
                    reported_model: model,
                    reported_model_informational: false,
                    usage: Some(TokenUsage::default()),
                };
                Ok(ProviderInvocation {
                    profile,
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
    }
}

struct ActiveCall<'a> {
    active: &'a AtomicUsize,
    max_active: &'a AtomicUsize,
}

impl Drop for ActiveCall<'_> {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
    }
}

impl ActiveCall<'_> {
    fn update_max(&self, current: usize) {
        let mut observed = self.max_active.load(Ordering::SeqCst);
        while current > observed {
            match self.max_active.compare_exchange(
                observed,
                current,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(latest) => observed = latest,
            }
        }
    }
}

#[tokio::test]
async fn fanout_cancellation_notifies_remaining_invocations_and_finishes_quickly() {
    let temp = tempdir().expect("temp");
    let mut node = Node::agent("fanout", "fanout");
    if let NodeKind::Agent { fan_out, .. } = &mut node.kind {
        *fan_out = 3;
    }
    node.retry.max_attempts = 2;
    node.retry.rebind_profiles = vec!["fallback".to_owned()];

    let invoker = TestInvoker::new(vec![
        ScriptedInvocation {
            expect_prompt_fragment: Some("fanout".to_owned()),
            wait_for_release: true,
            invocation: Invocation::Ok {
                profile: "default".to_owned(),
                output: AdapterOutput::Text("third".to_owned()),
                model: Some("m".to_owned()),
            },
        },
        ScriptedInvocation {
            expect_prompt_fragment: Some("fanout".to_owned()),
            wait_for_release: true,
            invocation: Invocation::Ok {
                profile: "default".to_owned(),
                output: AdapterOutput::Text("second".to_owned()),
                model: Some("m".to_owned()),
            },
        },
        ScriptedInvocation {
            expect_prompt_fragment: Some("fanout".to_owned()),
            wait_for_release: false,
            invocation: Invocation::Err(AdapterError::Timeout {
                profile: "default".to_owned(),
                timeout_ms: 1,
                retryable: false,
            }),
        },
    ]);

    let invoker_for_runtime: Arc<dyn ProviderInvoker> = Arc::clone(&invoker) as _;
    let runtime = Runtime::from_invoker(invoker_for_runtime, temp.path().join("runs"));
    let options = RunOptions {
        current_dir: temp.path().to_path_buf(),
        max_parallel: Some(3),
        ..RunOptions::default()
    };
    let graph = gloop_core::Graph::new("fanout_cancel", "fanout cancel", vec![node]);

    let run = tokio::spawn(async move { runtime.run(&graph, options).await.expect("run") });

    timeout(Duration::from_millis(200), invoker.wait_for_call_count(3))
        .await
        .expect("fanout calls should be scheduled");

    let summary = timeout(Duration::from_millis(800), run)
        .await
        .expect("run should finish quickly when remaining fanout calls cancel")
        .unwrap();

    assert_eq!(summary.nodes["fanout"].status, NodeStatus::Failed);
    assert_eq!(summary.status, FinalStatus::Failed);
    assert_eq!(invoker.call_count(), 3);
    assert_eq!(invoker.max_active(), 3);
    assert_eq!(invoker.active(), 0);
    assert!(
        invoker.canceled_remaining() >= 2,
        "remaining fanout invocations must observe cancellation"
    );
}

#[tokio::test]
async fn aggregate_fanout_output_limit_fails_without_rerunning_candidates() {
    let temp = tempdir().expect("temp");
    let mut node = Node::agent("fanout", "aggregate output");
    if let NodeKind::Agent {
        fan_out, output, ..
    } = &mut node.kind
    {
        *fan_out = 2;
        output.max_bytes = 64;
    }
    node.retry.max_attempts = 3;
    node.retry.rebind_profiles = vec!["fallback-a".to_owned(), "fallback-b".to_owned()];
    let candidate = "x".repeat(30);
    let invoker = SerializedStartInvoker::new(vec![
        ScriptedInvocation {
            expect_prompt_fragment: Some("aggregate output".to_owned()),
            wait_for_release: false,
            invocation: Invocation::Ok {
                profile: "default".to_owned(),
                output: AdapterOutput::Text(candidate.clone()),
                model: Some("m".to_owned()),
            },
        },
        ScriptedInvocation {
            expect_prompt_fragment: Some("aggregate output".to_owned()),
            wait_for_release: false,
            invocation: Invocation::Ok {
                profile: "default".to_owned(),
                output: AdapterOutput::Text(candidate),
                model: Some("m".to_owned()),
            },
        },
    ]);
    let invoker_for_runtime: Arc<dyn ProviderInvoker> = Arc::clone(&invoker) as _;
    let runtime = Runtime::from_invoker(invoker_for_runtime, temp.path().join("runs"));
    let options = RunOptions {
        current_dir: temp.path().to_path_buf(),
        max_parallel: Some(2),
        ..RunOptions::default()
    };
    let graph = gloop_core::Graph::new("fanout_limit", "fanout limit", vec![node]);

    invoker.release_next_start();
    invoker.release_next_start();
    let summary = runtime.run(&graph, options).await.expect("run");

    assert_eq!(summary.nodes["fanout"].status, NodeStatus::Failed);
    assert_eq!(summary.nodes["fanout"].attempts, 1);
    assert_eq!(invoker.call_count(), 2);
}

#[tokio::test]
async fn nested_subgraph_effective_max_parallel_limits_fanout_execution() {
    let temp = tempdir().expect("temp");
    let mut inner = Node::agent("inner", "inner");
    if let NodeKind::Agent { fan_out, .. } = &mut inner.kind {
        *fan_out = 2;
    }
    let mut inner_graph = gloop_core::Graph::new("inner", "inner", vec![inner]);
    inner_graph.spec.policies.max_parallel = 1;

    let mut outer = Node::agent("outer", "outer");
    outer.kind = NodeKind::Subgraph {
        graph: Box::new(inner_graph),
    };
    let graph = gloop_core::Graph::new("nested", "nested", vec![outer]);

    let invoker = SerializedStartInvoker::new(vec![
        ScriptedInvocation {
            expect_prompt_fragment: Some("inner".to_owned()),
            wait_for_release: true,
            invocation: Invocation::Ok {
                profile: "default".to_owned(),
                output: AdapterOutput::Text("ok1".to_owned()),
                model: Some("m".to_owned()),
            },
        },
        ScriptedInvocation {
            expect_prompt_fragment: Some("inner".to_owned()),
            wait_for_release: true,
            invocation: Invocation::Ok {
                profile: "default".to_owned(),
                output: AdapterOutput::Text("ok2".to_owned()),
                model: Some("m".to_owned()),
            },
        },
    ]);

    let invoker_for_runtime: Arc<dyn ProviderInvoker> = Arc::clone(&invoker) as _;
    let runtime = Runtime::from_invoker(invoker_for_runtime, temp.path().join("runs"));
    let options = RunOptions {
        current_dir: temp.path().to_path_buf(),
        max_parallel: Some(4),
        ..RunOptions::default()
    };

    let run = tokio::spawn(async move { runtime.run(&graph, options).await.expect("run") });

    invoker.release_next_start();
    timeout(Duration::from_millis(200), invoker.wait_for_call_count(1))
        .await
        .expect("first fanout call should start");
    assert_eq!(invoker.call_count(), 1);

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        invoker.call_count(),
        1,
        "second fanout call must not start before permit"
    );

    invoker.release_running_call();

    tokio::time::sleep(Duration::from_millis(50)).await;
    invoker.release_next_start();

    timeout(Duration::from_millis(400), invoker.wait_for_call_count(2))
        .await
        .expect("second fanout call should start");

    invoker.release_running_call();
    let summary = timeout(Duration::from_millis(800), run)
        .await
        .expect("run should finish")
        .unwrap();

    assert_eq!(summary.nodes["outer"].status, NodeStatus::Succeeded);
    assert_eq!(summary.status, FinalStatus::ReadyForHuman);
    assert_eq!(invoker.call_count(), 2);
    assert_eq!(invoker.max_active(), 1);
}
