use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use async_trait::async_trait;
use gloop_core::{
    FinalStatus, Graph, Node, NodeKind, NodeStatus,
    graph::{Edge, EdgeKind, OutputFormat, PromptSpec, WorkspaceSpec},
};
use gloop_provider::{
    AdapterCapabilities, AdapterError, AdapterOutput, AdapterRequest, AdapterResponse, ModelOrigin,
    SelectionOrigin, TokenUsage,
};
use gloop_runtime::{ProviderInvocation, ProviderInvoker, RunOptions, Runtime};
use indexmap::IndexMap;
use serde_json::json;
use tempfile::tempdir;
use tokio::sync::{Mutex, Notify};

#[derive(Debug)]
struct ScriptedInvocation {
    expect_prompt_fragment: &'static str,
    output: AdapterOutput,
}

#[derive(Debug)]
struct TestInvoker {
    planned: Mutex<VecDeque<ScriptedInvocation>>,
    started_count: Mutex<usize>,
    started_notify: Notify,
}

impl TestInvoker {
    fn new(planned: Vec<ScriptedInvocation>) -> Arc<Self> {
        Arc::new(Self {
            planned: Mutex::new(planned.into()),
            started_count: Mutex::new(0),
            started_notify: Notify::new(),
        })
    }

    async fn wait_for_first_call(&self) {
        loop {
            let count = *self.started_count.lock().await;
            if count >= 1 {
                return;
            }
            self.started_notify.notified().await;
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
        _cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<ProviderInvocation, AdapterError> {
        let mut planned = self.planned.lock().await;
        let invocation = planned
            .pop_front()
            .ok_or_else(|| AdapterError::InvalidRequest {
                profile: preferred_profile.unwrap_or("fake").to_owned(),
                message: "unexpected provider invocation".to_owned(),
            })?;

        {
            let mut count = self.started_count.lock().await;
            *count += 1;
            self.started_notify.notify_waiters();
        }

        assert!(
            request.prompt.contains(invocation.expect_prompt_fragment),
            "provider prompt mismatch: {request:?}"
        );

        Ok(ProviderInvocation {
            profile: "fake".to_owned(),
            selected_model: None,
            selection_origin: SelectionOrigin::Explicit,
            model_origin: ModelOrigin::ProviderDefault,
            response: AdapterResponse {
                output: invocation.output,
                stdout: String::new(),
                stderr: String::new(),
                exit_code: Some(0),
                reported_model: None,
                reported_model_informational: false,
                usage: Some(TokenUsage::default()),
            },
        })
    }
}

#[derive(Debug)]
struct MutatingInvoker {
    delegate: Arc<TestInvoker>,
    current_dir: PathBuf,
    escape_dir: PathBuf,
    mutated: AtomicBool,
}

impl MutatingInvoker {
    fn new(delegate: Arc<TestInvoker>, current_dir: PathBuf, escape_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            delegate,
            current_dir,
            escape_dir,
            mutated: AtomicBool::new(false),
        })
    }

    async fn mutate_workspace(&self) -> std::io::Result<()> {
        let _ = tokio::fs::remove_dir_all(&self.current_dir).await;
        tokio::fs::create_dir_all(&self.escape_dir).await?;
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&self.escape_dir, &self.current_dir)?;
        }
        Ok(())
    }
}

#[async_trait]
impl ProviderInvoker for MutatingInvoker {
    async fn execute(
        &self,
        preferred_profile: Option<&str>,
        required: &AdapterCapabilities,
        request: AdapterRequest,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<ProviderInvocation, AdapterError> {
        let response = self
            .delegate
            .execute(preferred_profile, required, request, cancellation)
            .await?;
        if !self.mutated.swap(true, Ordering::SeqCst) {
            self.mutate_workspace()
                .await
                .map_err(|error| AdapterError::InvalidRequest {
                    profile: preferred_profile.unwrap_or("fake").to_owned(),
                    message: format!("workspace mutation failed: {error}"),
                })?;
        }
        Ok(response)
    }
}

fn runtime_with(
    invoker: Arc<dyn gloop_runtime::ProviderInvoker>,
    artifact_root: &tempfile::TempDir,
    run_dir: PathBuf,
) -> (Runtime, RunOptions) {
    let runtime = Runtime::from_invoker(invoker, artifact_root.path().join("runs"));
    let options = RunOptions {
        current_dir: run_dir,
        ..RunOptions::default()
    };
    (runtime, options)
}

#[tokio::test]
async fn runtime_rejects_prompt_context_and_schema_inputs_when_limits_are_exceeded() {
    let temp = tempdir().expect("temp");
    let run_dir = temp.path().to_owned();

    let mut prompt_node = Node::agent("prompt_node", "{{node_id}}");
    if let NodeKind::Agent { prompt, .. } = &mut prompt_node.kind {
        let prompt_file = run_dir.join("prompt-package.txt");
        tokio::fs::write(&prompt_file, vec![b'a'; 256 * 1024 + 1])
            .await
            .expect("write oversized prompt package");
        *prompt = PromptSpec::Package {
            file: prompt_file
                .strip_prefix(&run_dir)
                .expect("prompt file is in run dir")
                .to_path_buf(),
            version: None,
            variables: IndexMap::new(),
        };
    }

    let mut context_node = Node::agent("context_node", "{{node_id}}");
    context_node.context.files.push("context.txt".into());
    tokio::fs::write(run_dir.join("context.txt"), vec![b'b'; 256 * 1024 + 1])
        .await
        .expect("write oversized context file");

    let mut schema_node = Node::agent("schema_node", "{{node_id}}");
    if let NodeKind::Agent { output, .. } = &mut schema_node.kind {
        output.format = OutputFormat::Json;
        output.schema = Some("schema.json".into());
    }
    tokio::fs::write(run_dir.join("schema.json"), vec![b'c'; 4 * 1024 * 1024 + 1])
        .await
        .expect("write oversized schema file");

    let cases = [
        (prompt_node, "prompt_node", "prompt_node"),
        (context_node, "context_node", "context_node"),
        (schema_node, "schema_node", "schema_node"),
    ];

    for (node, node_id, expectation) in cases {
        let output = if node_id == "schema_node" {
            AdapterOutput::Json(json!({"ok": true}))
        } else {
            AdapterOutput::Text("ok".to_owned())
        };
        let invoker = TestInvoker::new(vec![ScriptedInvocation {
            expect_prompt_fragment: expectation,
            output,
        }]);
        let graph = Graph::new(node_id, format!("overflow {node_id}"), vec![node]);
        let (runtime, options) = runtime_with(invoker, &temp, run_dir.clone());
        let summary = runtime.run(&graph, options).await.expect("runtime run");
        assert_eq!(summary.status, FinalStatus::Failed);
        let failure = summary.nodes[node_id].error.as_deref().unwrap_or_default();
        assert!(
            failure.contains("exceeded the"),
            "unexpected error for {node_id}: {failure}"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn runtime_rejects_inherited_workspace_after_identity_is_replaced() {
    use tokio::time::{Duration, sleep};

    let temp = tempdir().expect("temp");
    let run_dir = temp.path().join("run-workspace");
    std::fs::create_dir_all(&run_dir).expect("workspace dir");

    let first = Node::agent("first", "first");
    let mut second = Node::agent("second", "second");
    second.workspace = WorkspaceSpec::Inherit {
        node: "first".to_owned(),
    };
    let mut graph = Graph::new("inherit", "workspace identity", vec![first, second]);
    graph.spec.edges.push(Edge {
        from: "first".to_owned(),
        to: "second".to_owned(),
        kind: EdgeKind::Data,
        when: None,
    });

    let base = TestInvoker::new(vec![ScriptedInvocation {
        expect_prompt_fragment: "first",
        output: AdapterOutput::Text("ok".to_owned()),
    }]);
    let mutator = MutatingInvoker::new(
        Arc::clone(&base),
        run_dir.clone(),
        temp.path().join("outside-workspace"),
    );
    let (runtime, options) = runtime_with(
        mutator as Arc<dyn gloop_runtime::ProviderInvoker>,
        &temp,
        run_dir,
    );
    let run = tokio::spawn({
        let options = options;
        async move { runtime.run(&graph, options).await }
    });

    base.wait_for_first_call().await;
    sleep(Duration::from_millis(20)).await;

    let summary = run.await.expect("run task").expect("run");
    assert_eq!(summary.status, FinalStatus::Failed);
    assert_eq!(summary.nodes["second"].status, NodeStatus::Failed);
    let failure = summary.nodes["second"].error.as_deref().unwrap_or_default();
    assert!(
        failure.contains("current workspace identity changed"),
        "unexpected error: {failure}"
    );
}
