use std::{collections::VecDeque, sync::Arc};

use async_trait::async_trait;
use gloop_core::{FinalStatus, Graph, Node, NodeKind};
use gloop_provider::{
    AdapterCapabilities, AdapterError, AdapterOutput, AdapterRequest, AdapterResponse, ModelOrigin,
    SelectionOrigin, TokenUsage,
};
use gloop_runtime::{ProviderInvocation, ProviderInvoker, RunOptions, Runtime};
use tempfile::tempdir;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
struct FakeInvocation {
    expect_prompt_fragment: &'static str,
    model: Option<String>,
}

#[derive(Debug)]
struct FakeProviderInvoker {
    calls: Mutex<Vec<AdapterRequest>>,
    planned: Mutex<VecDeque<FakeInvocation>>,
    fail_if_unplanned: bool,
}

impl FakeProviderInvoker {
    fn new(planned: Vec<FakeInvocation>) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            planned: Mutex::new(planned.into()),
            fail_if_unplanned: true,
        })
    }

    async fn call_models(&self) -> Vec<Option<String>> {
        self.calls
            .lock()
            .await
            .iter()
            .map(|call| call.model.clone())
            .collect()
    }

    async fn call_prompts(&self) -> Vec<String> {
        self.calls
            .lock()
            .await
            .iter()
            .map(|call| call.prompt.clone())
            .collect()
    }

    async fn wait_for_calls(&self, expected: usize) {
        loop {
            let ready = self.calls.lock().await.len();
            if ready >= expected {
                return;
            }
            tokio::task::yield_now().await;
        }
    }
}

#[async_trait]
impl ProviderInvoker for FakeProviderInvoker {
    async fn execute(
        &self,
        preferred_profile: Option<&str>,
        _required: &AdapterCapabilities,
        request: AdapterRequest,
        _cancellation: CancellationToken,
    ) -> Result<ProviderInvocation, AdapterError> {
        self.calls.lock().await.push(request.clone());

        let mut planned = self.planned.lock().await;
        if self.fail_if_unplanned && planned.is_empty() {
            return Err(AdapterError::InvalidRequest {
                profile: preferred_profile.unwrap_or("fake").to_owned(),
                message: "unexpected provider invocation".to_owned(),
            });
        }
        let scripted = if let Some(index) = planned
            .iter()
            .position(|inv| request.prompt.contains(inv.expect_prompt_fragment))
        {
            planned.remove(index).expect("provider invocation prepared")
        } else {
            planned.pop_front().expect("provider invocation prepared")
        };

        assert!(
            request.prompt.contains(scripted.expect_prompt_fragment),
            "provider call prompt mismatch: {:?}",
            request.prompt
        );
        assert_eq!(request.model, scripted.model);

        Ok(ProviderInvocation {
            profile: "fake".to_owned(),
            selected_model: scripted.model.clone(),
            selection_origin: SelectionOrigin::Explicit,
            model_origin: ModelOrigin::ProviderDefault,
            response: AdapterResponse {
                output: AdapterOutput::Text("ok".to_owned()),
                stdout: String::new(),
                stderr: String::new(),
                exit_code: Some(0),
                reported_model: scripted.model,
                usage: Some(TokenUsage::default()),
            },
        })
    }
}

fn runtime_with(
    invoker: Arc<FakeProviderInvoker>,
    temp: &tempfile::TempDir,
) -> (Runtime, RunOptions) {
    let runtime = Runtime::from_invoker(invoker, temp.path().join("runs"));
    let options = RunOptions {
        current_dir: temp.path().to_path_buf(),
        ..RunOptions::default()
    };
    (runtime, options)
}

fn agent_node_with_model(id: &str, prompt: &str, model: Option<&str>) -> Node {
    let mut node = Node::agent(id.to_owned(), prompt);
    if let NodeKind::Agent {
        model: node_model, ..
    } = &mut node.kind
    {
        *node_model = model.map(ToOwned::to_owned);
    }
    node
}

fn reduce_node_with_model(id: &str, prompt: &str, model: Option<&str>) -> Node {
    let mut node = Node::agent(id.to_owned(), prompt);
    node.kind = match node.kind {
        NodeKind::Agent {
            prompt,
            profile,
            model: _,
            fan_out: _,
            output,
        } => NodeKind::Reduce {
            prompt,
            profile,
            model: model.map(ToOwned::to_owned),
            output,
        },
        _ => panic!("NodeKind unexpectedly changed while constructing reduce node"),
    };
    node
}

fn synthesize_node_with_model(id: &str, prompt: &str, model: Option<&str>) -> Node {
    let mut node = Node::agent(id.to_owned(), prompt);
    node.kind = match node.kind {
        NodeKind::Agent {
            prompt,
            profile,
            model: _,
            fan_out: _,
            output,
        } => NodeKind::Synthesize {
            prompt,
            profile,
            model: model.map(ToOwned::to_owned),
            output,
        },
        _ => panic!("NodeKind unexpectedly changed while constructing synthesize node"),
    };
    node
}

#[tokio::test]
async fn runtime_forwards_node_model_to_adapter_request_for_all_model_capable_kinds() {
    let temp = tempdir().expect("temp");
    let invoker = FakeProviderInvoker::new(vec![
        FakeInvocation {
            expect_prompt_fragment: "AGENT_NODE",
            model: Some("agent-model".to_owned()),
        },
        FakeInvocation {
            expect_prompt_fragment: "REDUCE_NODE",
            model: Some("reduce-model".to_owned()),
        },
        FakeInvocation {
            expect_prompt_fragment: "SYNTH_NODE",
            model: Some("synthesize-model".to_owned()),
        },
    ]);

    let graph = Graph::new(
        "model-binding",
        "model binding",
        vec![
            agent_node_with_model("agent", "AGENT_NODE", Some("agent-model")),
            reduce_node_with_model("reduce", "REDUCE_NODE", Some("reduce-model")),
            synthesize_node_with_model("synthesize", "SYNTH_NODE", Some("synthesize-model")),
        ],
    );

    let (runtime, options) = runtime_with(Arc::clone(&invoker), &temp);
    let summary = runtime.run(&graph, options).await.expect("run");
    invoker.wait_for_calls(3).await;
    assert_eq!(summary.status, FinalStatus::ReadyForHuman);

    let models = invoker.call_models().await;
    assert_eq!(models.len(), 3);
    let mut expected = vec![
        Some("agent-model".to_owned()),
        Some("reduce-model".to_owned()),
        Some("synthesize-model".to_owned()),
    ];
    expected.sort();
    let mut actual = models;
    actual.sort();
    assert_eq!(expected, actual);
}

#[tokio::test]
async fn runtime_sends_none_model_for_nodes_without_model() {
    let temp = tempdir().expect("temp");
    let invoker = FakeProviderInvoker::new(vec![
        FakeInvocation {
            expect_prompt_fragment: "AGENT_NODE",
            model: None,
        },
        FakeInvocation {
            expect_prompt_fragment: "REDUCE_NODE",
            model: None,
        },
        FakeInvocation {
            expect_prompt_fragment: "SYNTH_NODE",
            model: None,
        },
    ]);

    let graph = Graph::new(
        "model-binding-none",
        "model binding none",
        vec![
            agent_node_with_model("agent", "AGENT_NODE", None),
            reduce_node_with_model("reduce", "REDUCE_NODE", None),
            synthesize_node_with_model("synthesize", "SYNTH_NODE", None),
        ],
    );

    let (runtime, options) = runtime_with(Arc::clone(&invoker), &temp);
    let summary = runtime.run(&graph, options).await.expect("run");
    invoker.wait_for_calls(3).await;
    assert_eq!(summary.status, FinalStatus::ReadyForHuman);

    let prompts = invoker.call_prompts().await;
    assert!(prompts.iter().any(|prompt| prompt.contains("AGENT_NODE")));
    assert!(prompts.iter().any(|prompt| prompt.contains("REDUCE_NODE")));
    assert!(prompts.iter().any(|prompt| prompt.contains("SYNTH_NODE")));
    let all_none = invoker
        .call_models()
        .await
        .into_iter()
        .all(|model| model.is_none());
    assert!(all_none);
}
