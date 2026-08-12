use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::{FutureExt, StreamExt, stream::FuturesUnordered};
use gloop_core::{
    CompiledGraph, FinalStatus, Graph, Node, NodeKind, NodeOutcome, NodeStatus, RunEvent,
    RunEventKind, RunSummary,
    graph::{
        ContextSpec, Edge, EdgeCondition, EdgeKind, FailurePolicy, GateDefault, LoopCondition,
        OutputFormat as GraphOutputFormat, OutputSpec, PromptSpec, WorkspaceSpec,
    },
    state::{ArtifactRef, CheckResult, ModelUsage, Provenance},
};
use gloop_provider::{
    AdapterCapabilities, AdapterCapability, AdapterError, AdapterErrorClass, AdapterOutput,
    AdapterRequest, AdapterResponse, ModelOrigin, OutputFormat as ProviderOutputFormat,
    ProviderRegistry, SelectionOrigin,
};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{
    fs,
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::{Mutex, Semaphore, mpsc, watch},
    time,
};
use tokio_util::sync::CancellationToken;
use ulid::Ulid;

use crate::{
    artifacts::{ArtifactError, ArtifactStore, AttemptArtifacts},
    journal::{Journal, JournalError},
    worktree::{GitWorktreeManager, WorktreeError, WorktreeWorkspace},
};

pub const SUMMARY_SCHEMA_VERSION: &str = "gloop.run-summary/v1alpha1";
const CANCELLATION_GRACE: Duration = Duration::from_secs(5);
const COMMAND_PIPE_DRAIN_GRACE: Duration = Duration::from_millis(500);
const MAX_SCHEMA_BYTES: usize = 4 * 1024 * 1024;
const RETAINED_OUTPUT_BYTES_BUDGET: usize = 64 * 1024 * 1024;
const SUMMARY_SNAPSHOT_FLUSH_EVERY: usize = 64;

#[cfg(unix)]
const PORTABLE_COMMAND_ENV_ALLOWLIST: [&str; 6] = ["HOME", "LANG", "PATH", "TMP", "TMPDIR", "USER"];
#[cfg(windows)]
const PORTABLE_COMMAND_ENV_ALLOWLIST: [&str; 7] = [
    "COMSPEC",
    "PATH",
    "SYSTEMROOT",
    "TEMP",
    "TMP",
    "USERPROFILE",
    "USERNAME",
];
#[cfg(not(any(unix, windows)))]
const PORTABLE_COMMAND_ENV_ALLOWLIST: [&str; 3] = ["HOME", "PATH", "TMPDIR"];

fn command_environment(env: &IndexMap<String, String>) -> IndexMap<String, String> {
    let mut command_env: IndexMap<String, String> = PORTABLE_COMMAND_ENV_ALLOWLIST
        .into_iter()
        .filter_map(|key| std::env::var(key).ok().map(|value| (key.to_owned(), value)))
        .collect();
    command_env.extend(env.iter().map(|(key, value)| (key.clone(), value.clone())));
    command_env
}

#[cfg(unix)]
async fn terminate_process_group(process_group: u32) {
    let process_group = format!("-{process_group}");
    let _ = tokio::process::Command::new("/bin/kill")
        .arg("-TERM")
        .arg(&process_group)
        .status()
        .await;
    time::sleep(Duration::from_millis(100)).await;
    let _ = tokio::process::Command::new("/bin/kill")
        .arg("-KILL")
        .arg(&process_group)
        .status()
        .await;
}

#[cfg(not(unix))]
async fn terminate_process_group(_: u32) {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessCancellationScope {
    DirectChild,
    ProcessGroup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeFailureClass {
    Execution,
    Verification,
    HumanGate,
    Cancelled,
    Budget,
    ProviderProfileNotFound,
    ProviderCapability,
    ProviderUnavailable,
    ProviderAuthentication,
    ProviderRateLimit,
    ProviderTransient,
    ProviderTimeout,
    ProviderContextLength,
    ProviderProtocol,
    ProviderConfiguration,
    ProviderProcess,
    ProviderCancelled,
}

impl NodeFailureClass {
    const fn code(self) -> &'static str {
        match self {
            Self::Execution => "execution",
            Self::Verification => "verification",
            Self::HumanGate => "human_gate",
            Self::Cancelled => "cancelled",
            Self::Budget => "budget",
            Self::ProviderProfileNotFound => "provider_profile_not_found",
            Self::ProviderCapability => "provider_capability",
            Self::ProviderUnavailable => "provider_unavailable",
            Self::ProviderAuthentication => "provider_authentication",
            Self::ProviderRateLimit => "provider_rate_limit",
            Self::ProviderTransient => "provider_transient",
            Self::ProviderTimeout => "provider_timeout",
            Self::ProviderContextLength => "provider_context_length",
            Self::ProviderProtocol => "provider_protocol",
            Self::ProviderConfiguration => "provider_configuration",
            Self::ProviderProcess => "provider_process",
            Self::ProviderCancelled => "provider_cancelled",
        }
    }

    fn from_code(code: &str) -> Option<Self> {
        Some(match code {
            "execution" => Self::Execution,
            "verification" => Self::Verification,
            "human_gate" => Self::HumanGate,
            "cancelled" => Self::Cancelled,
            "budget" => Self::Budget,
            "provider_profile_not_found" => Self::ProviderProfileNotFound,
            "provider_capability" => Self::ProviderCapability,
            "provider_unavailable" => Self::ProviderUnavailable,
            "provider_authentication" => Self::ProviderAuthentication,
            "provider_rate_limit" => Self::ProviderRateLimit,
            "provider_transient" => Self::ProviderTransient,
            "provider_timeout" => Self::ProviderTimeout,
            "provider_context_length" => Self::ProviderContextLength,
            "provider_protocol" => Self::ProviderProtocol,
            "provider_configuration" => Self::ProviderConfiguration,
            "provider_process" => Self::ProviderProcess,
            "provider_cancelled" => Self::ProviderCancelled,
            _ => return None,
        })
    }
}

pub fn node_failure_class(outcome: &NodeOutcome) -> Option<NodeFailureClass> {
    let error = outcome.error.as_deref()?;
    let code = error.strip_prefix("[gloop:")?.split_once(']')?.0;
    NodeFailureClass::from_code(code)
}

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub run_id: Option<String>,
    pub current_dir: PathBuf,
    pub max_parallel: Option<usize>,
    pub wall_time: Option<Duration>,
    pub model_calls: Option<u32>,
    pub cancellation: CancellationToken,
    pub progress: Option<mpsc::UnboundedSender<ProgressEvent>>,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            run_id: None,
            current_dir: PathBuf::from("."),
            max_parallel: None,
            wall_time: None,
            model_calls: None,
            cancellation: CancellationToken::new(),
            progress: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProgressEvent {
    pub sequence: u64,
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    pub kind: RunEventKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl From<&RunEvent> for ProgressEvent {
    fn from(event: &RunEvent) -> Self {
        Self {
            sequence: event.sequence,
            run_id: event.run_id.clone(),
            node_id: event.node_id.clone(),
            kind: event.kind,
            message: event.message.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateDecision {
    Approve,
    Reject,
}

#[derive(Debug, Clone)]
pub struct GateRequest {
    pub run_id: String,
    pub node_id: String,
    pub message: String,
    pub default: GateDecision,
}

#[async_trait]
pub trait HumanGate: Send + Sync + fmt::Debug {
    async fn decide(&self, request: GateRequest) -> Result<GateDecision, String>;
}

#[derive(Debug, Default)]
pub struct DefaultHumanGate;

#[async_trait]
impl HumanGate for DefaultHumanGate {
    async fn decide(&self, request: GateRequest) -> Result<GateDecision, String> {
        Ok(request.default)
    }
}

#[async_trait]
pub trait ProviderInvoker: Send + Sync + fmt::Debug {
    async fn execute(
        &self,
        preferred_profile: Option<&str>,
        required: &AdapterCapabilities,
        request: AdapterRequest,
        cancellation: CancellationToken,
    ) -> Result<ProviderInvocation, AdapterError>;
}

#[derive(Debug, Clone)]
pub struct ProviderInvocation {
    pub profile: String,
    pub selected_model: Option<String>,
    pub selection_origin: SelectionOrigin,
    pub model_origin: ModelOrigin,
    pub response: AdapterResponse,
}

struct RegistryInvoker {
    registry: ProviderRegistry,
}

impl fmt::Debug for RegistryInvoker {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RegistryInvoker")
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl ProviderInvoker for RegistryInvoker {
    async fn execute(
        &self,
        preferred_profile: Option<&str>,
        required: &AdapterCapabilities,
        request: AdapterRequest,
        cancellation: CancellationToken,
    ) -> Result<ProviderInvocation, AdapterError> {
        let result = self
            .registry
            .execute_with_capabilities(preferred_profile, required, request, cancellation, None)
            .await?;
        Ok(ProviderInvocation {
            profile: result.selection.profile,
            selected_model: result.selection.model,
            selection_origin: result.selection.origin,
            model_origin: result.selection.model_origin,
            response: result.response,
        })
    }
}

#[derive(Clone)]
pub struct Runtime {
    providers: Arc<dyn ProviderInvoker>,
    artifact_root: PathBuf,
    gate: Arc<dyn HumanGate>,
}

impl fmt::Debug for Runtime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Runtime")
            .field("artifact_root", &self.artifact_root)
            .field("providers", &self.providers)
            .field("gate", &self.gate)
            .finish()
    }
}

impl Runtime {
    pub fn new(registry: ProviderRegistry, artifact_root: impl Into<PathBuf>) -> Self {
        Self {
            providers: Arc::new(RegistryInvoker { registry }),
            artifact_root: artifact_root.into(),
            gate: Arc::new(DefaultHumanGate),
        }
    }

    pub fn from_invoker(
        providers: Arc<dyn ProviderInvoker>,
        artifact_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            providers,
            artifact_root: artifact_root.into(),
            gate: Arc::new(DefaultHumanGate),
        }
    }

    #[must_use]
    pub fn with_human_gate(mut self, gate: Arc<dyn HumanGate>) -> Self {
        self.gate = gate;
        self
    }

    pub const fn process_cancellation_scope(&self) -> ProcessCancellationScope {
        if cfg!(unix) {
            ProcessCancellationScope::ProcessGroup
        } else {
            ProcessCancellationScope::DirectChild
        }
    }

    #[allow(clippy::too_many_lines)]
    pub async fn run(&self, graph: &Graph, options: RunOptions) -> Result<RunSummary, RunError> {
        let compiled = graph.compile()?;
        let max_parallel = options
            .max_parallel
            .map_or(graph.spec.policies.max_parallel, |limit| {
                limit.min(graph.spec.policies.max_parallel)
            });
        if graph.spec.policies.max_parallel == 0
            || graph.spec.policies.max_parallel > Semaphore::MAX_PERMITS
            || options
                .max_parallel
                .is_some_and(|limit| limit == 0 || limit > Semaphore::MAX_PERMITS)
            || max_parallel == 0
            || max_parallel > Semaphore::MAX_PERMITS
        {
            return Err(RunError::InvalidParallelism);
        }
        let current_dir = fs::canonicalize(&options.current_dir)
            .await
            .map_err(|source| RunError::CurrentDirectory {
                path: options.current_dir.clone(),
                source,
            })?;
        if !fs::metadata(&current_dir).await?.is_dir() {
            return Err(RunError::CurrentDirectoryNotDirectory(current_dir));
        }
        let run_id = options
            .run_id
            .unwrap_or_else(|| Ulid::new().to_string().to_ascii_lowercase());
        let store = ArtifactStore::create(&self.artifact_root, &run_id).await?;
        let graph_artifact = store.write_graph(graph).await?;
        let journal = Arc::new(Journal::create(&store.paths().journal, &run_id).await?);
        let worktree_manager = if graph_requires_worktree(graph) {
            Some(Arc::new(
                GitWorktreeManager::new(&current_dir, &run_id).await?,
            ))
        } else {
            None
        };
        let base_commit = worktree_manager
            .as_ref()
            .map(|manager| manager.base_commit().to_owned());
        let root_workspace = ResolvedWorkspace {
            path: current_dir.clone(),
            owner: None,
        };
        let started_at = Utc::now();
        let started = Instant::now();
        let graph_hash = graph.hash()?;
        let wall_time = minimum_duration(
            options.wall_time,
            graph
                .spec
                .budgets
                .wall_time_seconds
                .map(Duration::from_secs),
        );
        let model_calls = minimum_u32(options.model_calls, graph.spec.budgets.model_calls);
        let context = Arc::new(RunContext {
            run_id: run_id.clone(),
            store,
            journal,
            external_cancellation: options.cancellation,
            budget_cancellation: CancellationToken::new(),
            deadline: wall_time.map(|duration| started + duration),
            model_call_limit: model_calls,
            model_calls: AtomicU32::new(0),
            retained_output_bytes: AtomicUsize::new(0),
            snapshot_terminal_count: AtomicUsize::new(0),
            parallelism: Semaphore::new(max_parallel),
            resource_locks: ResourceLocks::new(),
            worktree_manager,
            worktree_workspaces: Mutex::new(HashMap::new()),
            progress: options.progress,
            snapshot: Mutex::new(IndexMap::new()),
            usage: Mutex::new(Vec::new()),
            artifacts: Mutex::new(vec![graph_artifact]),
        });

        context
            .emit(
                RunEventKind::RunStarted,
                None,
                None,
                None,
                json!({
                    "graph_hash": graph_hash,
                    "graph_name": graph.metadata.name,
                    "nodes": graph.spec.nodes.iter().map(|node| &node.id).collect::<Vec<_>>(),
                    "max_parallel": max_parallel,
                    "wall_time_seconds": wall_time.map(|value| value.as_secs()),
                    "model_calls": model_calls,
                }),
            )
            .await?;

        let execution = self
            .clone()
            .execute_graph(
                compiled,
                Arc::clone(&context),
                String::new(),
                max_parallel,
                CancellationToken::new(),
                root_workspace,
            )
            .await?;

        let global_cancel = context.global_cancel_reason();
        if let Some(reason) = global_cancel {
            context
                .emit(
                    RunEventKind::RunCancelled,
                    None,
                    None,
                    Some(reason.message().to_owned()),
                    json!({"reason": reason.as_str()}),
                )
                .await?;
        }

        let status = final_status(graph, &execution.outcomes, global_cancel);
        let finished_at = Utc::now();
        let duration_ms = duration_millis(started.elapsed());
        let usage = context.usage.lock().await.clone();
        let profiles_used = deduplicated_profiles(&usage);
        let models_used = deduplicated_models(&usage);
        let checks = verification_checks(graph, &execution.outcomes);
        let blocking_findings = blocking_findings(&execution.outcomes);
        let unresolved = unresolved_nodes(&execution.outcomes);

        context.flush_summary_snapshot().await?;

        let worktree_manifest_artifact = if let Some(manager) = &context.worktree_manager {
            let manifest = manager.manifest().await?;
            let artifact = context.store.write_worktree_manifest(&manifest).await?;
            context.artifacts.lock().await.push(artifact.clone());
            Some(artifact)
        } else {
            None
        };

        context
            .emit(
                RunEventKind::RunFinished,
                None,
                None,
                None,
                json!({
                    "status": status,
                    "worktree_manifest_artifact": worktree_manifest_artifact
                        .as_ref()
                        .map(|artifact| artifact.path.as_str()),
                }),
            )
            .await?;

        let journal_artifact = context
            .store
            .reference(&context.store.paths().journal, "journal")
            .await?;
        context.artifacts.lock().await.push(journal_artifact);
        let artifacts = context.artifacts.lock().await.clone();
        let summary = RunSummary {
            schema_version: SUMMARY_SCHEMA_VERSION.to_owned(),
            run_id,
            status,
            graph_name: graph.metadata.name.clone(),
            goal: graph.spec.goal.clone(),
            summary: format_summary(status, graph, &execution.outcomes),
            started_at,
            finished_at,
            duration_ms,
            nodes: execution.outcomes,
            profiles_used,
            models_used,
            checks,
            blocking_findings,
            unresolved,
            artifacts,
            provenance: Provenance {
                graph_hash,
                base_commit,
                runtime_version: env!("CARGO_PKG_VERSION").to_owned(),
            },
        };
        context.store.write_summary(&summary).await?;
        Ok(summary)
    }

    #[allow(clippy::too_many_lines)]
    fn execute_graph(
        self,
        compiled: CompiledGraph,
        context: Arc<RunContext>,
        namespace: String,
        inherited_parallel_limit: usize,
        parent_cancellation: CancellationToken,
        default_workspace: ResolvedWorkspace,
    ) -> Pin<Box<dyn Future<Output = Result<GraphExecution, RunError>> + Send>> {
        Box::pin(async move {
            let max_parallel = inherited_parallel_limit
                .min(compiled.graph.spec.policies.max_parallel)
                .clamp(1, Semaphore::MAX_PERMITS);
            let scheduler_cancellation = parent_cancellation.child_token();
            let mut resource_changes = context.resource_locks.subscribe();
            let mut outcomes: IndexMap<String, NodeOutcome> = compiled
                .graph
                .spec
                .nodes
                .iter()
                .map(|node| (node.id.clone(), NodeOutcome::default()))
                .collect();
            let mut running = FuturesUnordered::<NodeFuture>::new();
            let mut resources = BTreeSet::<String>::new();

            loop {
                context.enforce_wall_budget();
                let global_cancel = context.global_cancel_reason();
                if global_cancel.is_none() && !scheduler_cancellation.is_cancelled() {
                    for id in &compiled.order {
                        if outcomes[id].status != NodeStatus::Pending {
                            continue;
                        }
                        match dependency_decision(&compiled, id, &outcomes) {
                            DependencyDecision::Wait => {}
                            DependencyDecision::Ready => {
                                outcomes.get_mut(id).expect("compiled node exists").status =
                                    NodeStatus::Ready;
                                let qualified = qualify(&namespace, id);
                                context
                                    .emit(
                                        RunEventKind::NodeReady,
                                        Some(&qualified),
                                        None,
                                        None,
                                        Value::Null,
                                    )
                                    .await?;
                            }
                            DependencyDecision::Skip(reason) => {
                                let now = Utc::now();
                                let outcome = outcomes.get_mut(id).expect("compiled node exists");
                                outcome.status = NodeStatus::Skipped;
                                outcome.error = Some(reason.clone());
                                outcome.finished_at = Some(now);
                                outcome.duration_ms = Some(0);
                                let qualified = qualify(&namespace, id);
                                context
                                    .emit(
                                        RunEventKind::NodeSkipped,
                                        Some(&qualified),
                                        None,
                                        Some(reason),
                                        Value::Null,
                                    )
                                    .await?;
                                context.record_outcome(&qualified, outcome.clone()).await?;
                            }
                        }
                    }

                    for id in &compiled.order {
                        if running.len() >= max_parallel || outcomes[id].status != NodeStatus::Ready
                        {
                            continue;
                        }
                        let node = compiled.node(id).expect("compiled node exists");
                        if node
                            .resources
                            .iter()
                            .any(|resource| resources.contains(resource))
                        {
                            continue;
                        }
                        let qualified_id = qualify(&namespace, id);
                        if !context
                            .resource_locks
                            .try_claim(&qualified_id, &node.resources)
                            .await
                        {
                            continue;
                        }
                        resources.extend(node.resources.iter().cloned());
                        outcomes.get_mut(id).expect("compiled node exists").status =
                            NodeStatus::Running;
                        let input = NodeInput {
                            node: node.clone(),
                            qualified_id,
                            dependencies: dependency_outputs(&compiled, id, &outcomes),
                            local_outcomes: outcomes.clone(),
                            parallel_limit: max_parallel,
                            namespace: namespace.clone(),
                            default_workspace: default_workspace.clone(),
                        };
                        let runtime = self.clone();
                        let context = Arc::clone(&context);
                        let scheduler_cancellation = scheduler_cancellation.clone();
                        let id = id.clone();
                        let claimed = node.resources.clone();
                        running.push(
                            async move {
                                let result = runtime
                                    .execute_node(input, context, scheduler_cancellation)
                                    .await;
                                (id, claimed, result)
                            }
                            .boxed(),
                        );
                    }
                }

                if running.is_empty() {
                    if outcomes
                        .values()
                        .all(|outcome| outcome.status.is_terminal())
                    {
                        break;
                    }
                    if let Some(reason) = global_cancel {
                        cancel_remaining(
                            &compiled,
                            &namespace,
                            &context,
                            &mut outcomes,
                            reason.message(),
                            false,
                        )
                        .await?;
                        break;
                    }
                    if scheduler_cancellation.is_cancelled() {
                        cancel_remaining(
                            &compiled,
                            &namespace,
                            &context,
                            &mut outcomes,
                            "cancelled by fail-fast policy",
                            true,
                        )
                        .await?;
                        break;
                    }
                    if outcomes
                        .values()
                        .any(|outcome| outcome.status == NodeStatus::Ready)
                    {
                        tokio::select! {
                            change = resource_changes.changed() => {
                                if change.is_err() {
                                    return Err(RunError::SchedulerInvariant(
                                        "resource lock notifier closed".into(),
                                    ));
                                }
                            }
                            _ = wait_for_cancellation(&context, &scheduler_cancellation) => {}
                        }
                        continue;
                    }
                    let stalled = outcomes
                        .iter()
                        .filter_map(|(id, outcome)| {
                            (!outcome.status.is_terminal()).then_some(id.clone())
                        })
                        .collect::<Vec<_>>();
                    for id in stalled {
                        let now = Utc::now();
                        let outcome = outcomes.get_mut(&id).expect("node exists");
                        outcome.status = NodeStatus::Blocked;
                        outcome.error =
                            Some("scheduler stalled with no runnable predecessor".into());
                        outcome.finished_at = Some(now);
                        let qualified = qualify(&namespace, &id);
                        context
                            .emit(
                                RunEventKind::NodeBlocked,
                                Some(&qualified),
                                None,
                                outcome.error.clone(),
                                json!({"status": "blocked"}),
                            )
                            .await?;
                        context.record_outcome(&qualified, outcome.clone()).await?;
                    }
                    break;
                }

                let Some((id, claimed, result)) = running.next().await else {
                    return Err(RunError::SchedulerInvariant(
                        "running node set ended unexpectedly".into(),
                    ));
                };
                for resource in claimed {
                    resources.remove(&resource);
                }
                context
                    .resource_locks
                    .release(
                        &qualify(&namespace, &id),
                        compiled
                            .node(&id)
                            .expect("node exists")
                            .resources
                            .as_slice(),
                    )
                    .await;
                let outcome = result?;
                let failed = outcome.status == NodeStatus::Failed;
                let continue_on_failure = compiled
                    .node(&id)
                    .expect("compiled node exists")
                    .continue_on_failure;
                let has_failure_handler = compiled
                    .outgoing_edges(&id)
                    .any(|edge| edge.kind == EdgeKind::Failure);
                let qualified = qualify(&namespace, &id);
                outcomes.insert(id, outcome.clone());
                context.record_outcome(&qualified, outcome).await?;
                if failed
                    && compiled.graph.spec.policies.failure == FailurePolicy::FailFast
                    && !continue_on_failure
                    && !has_failure_handler
                {
                    scheduler_cancellation.cancel();
                }
            }

            Ok(GraphExecution { outcomes })
        })
    }
}

#[derive(Debug)]
struct RunContext {
    run_id: String,
    store: ArtifactStore,
    journal: Arc<Journal>,
    external_cancellation: CancellationToken,
    budget_cancellation: CancellationToken,
    deadline: Option<Instant>,
    model_call_limit: Option<u32>,
    model_calls: AtomicU32,
    retained_output_bytes: AtomicUsize,
    snapshot_terminal_count: AtomicUsize,
    parallelism: Semaphore,
    resource_locks: ResourceLocks,
    worktree_manager: Option<Arc<GitWorktreeManager>>,
    worktree_workspaces: Mutex<HashMap<String, WorktreeWorkspace>>,
    progress: Option<mpsc::UnboundedSender<ProgressEvent>>,
    snapshot: Mutex<IndexMap<String, NodeOutcome>>,
    usage: Mutex<Vec<ProviderUsage>>,
    artifacts: Mutex<Vec<ArtifactRef>>,
}

#[derive(Debug, Clone)]
struct ResolvedWorkspace {
    path: PathBuf,
    owner: Option<WorktreeWorkspace>,
}

#[derive(Debug)]
struct ResourceLocks {
    claims: Mutex<HashMap<String, BTreeSet<String>>>,
    generation: watch::Sender<u64>,
}

impl ResourceLocks {
    fn new() -> Self {
        let (generation, _receiver) = watch::channel(0);
        Self {
            claims: Mutex::new(HashMap::new()),
            generation,
        }
    }

    fn subscribe(&self) -> watch::Receiver<u64> {
        self.generation.subscribe()
    }

    async fn try_claim(&self, owner: &str, resources: &[String]) -> bool {
        let mut claims = self.claims.lock().await;
        if resources.iter().any(|resource| {
            claims.get(resource).is_some_and(|owners| {
                owners
                    .iter()
                    .any(|existing| !owners_are_related(existing, owner))
            })
        }) {
            return false;
        }
        for resource in resources {
            claims
                .entry(resource.clone())
                .or_default()
                .insert(owner.to_owned());
        }
        true
    }

    async fn release(&self, owner: &str, resources: &[String]) {
        let mut claims = self.claims.lock().await;
        for resource in resources {
            let remove_resource = claims.get_mut(resource).is_some_and(|owners| {
                owners.remove(owner);
                owners.is_empty()
            });
            if remove_resource {
                claims.remove(resource);
            }
        }
        drop(claims);
        self.generation.send_modify(|generation| {
            *generation = generation.wrapping_add(1);
        });
    }
}

fn owners_are_related(left: &str, right: &str) -> bool {
    left == right
        || right
            .strip_prefix(left)
            .is_some_and(|suffix| suffix.starts_with('.'))
        || left
            .strip_prefix(right)
            .is_some_and(|suffix| suffix.starts_with('.'))
}

impl RunContext {
    async fn record_worktree_workspace(
        &self,
        qualified_id: &str,
        workspace: &WorktreeWorkspace,
    ) -> Result<(), RunError> {
        let mut workspaces = self.worktree_workspaces.lock().await;
        if let Some(existing) = workspaces.get(qualified_id) {
            if existing != workspace {
                return Err(RunError::SchedulerInvariant(format!(
                    "workspace identity changed for node {qualified_id:?}"
                )));
            }
            return Ok(());
        }
        workspaces.insert(qualified_id.to_owned(), workspace.clone());
        Ok(())
    }

    async fn worktree_workspace_for(&self, qualified_id: &str) -> Option<WorktreeWorkspace> {
        self.worktree_workspaces
            .lock()
            .await
            .get(qualified_id)
            .cloned()
    }

    async fn emit(
        &self,
        kind: RunEventKind,
        node_id: Option<&str>,
        attempt: Option<u32>,
        message: Option<String>,
        data: Value,
    ) -> Result<RunEvent, RunError> {
        let event = self
            .journal
            .append(kind, node_id, attempt, message, data)
            .await?;
        if let Some(progress) = &self.progress {
            let _receiver_may_have_closed = progress.send(ProgressEvent::from(&event));
        }
        Ok(event)
    }

    fn enforce_wall_budget(&self) {
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.budget_cancellation.cancel();
        }
    }

    fn global_cancel_reason(&self) -> Option<GlobalCancelReason> {
        self.enforce_wall_budget();
        if self.budget_cancellation.is_cancelled() {
            Some(GlobalCancelReason::Budget)
        } else if self.external_cancellation.is_cancelled() {
            Some(GlobalCancelReason::External)
        } else {
            None
        }
    }

    fn reserve_model_calls(&self, count: usize) -> Result<(), AttemptFailure> {
        let count = u32::try_from(count)
            .map_err(|_| AttemptFailure::normal("fan_out exceeds the model-call counter"))?;
        loop {
            let current = self.model_calls.load(Ordering::Acquire);
            let Some(next) = current.checked_add(count) else {
                self.budget_cancellation.cancel();
                return Err(AttemptFailure::global_cancelled(
                    "model-call budget counter overflowed",
                ));
            };
            if self.model_call_limit.is_some_and(|limit| next > limit) {
                self.budget_cancellation.cancel();
                return Err(AttemptFailure::global_cancelled(
                    "model-call budget exhausted",
                ));
            }
            if self
                .model_calls
                .compare_exchange(current, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Ok(());
            }
        }
    }

    fn reserve_retained_output_bytes(
        &self,
        node: &str,
        bytes: usize,
    ) -> Result<(), AttemptFailure> {
        loop {
            let current = self.retained_output_bytes.load(Ordering::Acquire);
            let Some(next) = current.checked_add(bytes) else {
                self.budget_cancellation.cancel();
                return Err(AttemptFailure::global_cancelled(format!(
                    "run retained output budget overflowed for node {node}"
                )));
            };
            if next > RETAINED_OUTPUT_BYTES_BUDGET {
                self.budget_cancellation.cancel();
                return Err(AttemptFailure::global_cancelled(format!(
                    "run retained output budget exceeded for node {node}"
                )));
            }
            if self
                .retained_output_bytes
                .compare_exchange(current, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Ok(());
            }
        }
    }

    fn reserve_attempt_artifact_bytes(
        &self,
        node: &str,
        stdout: &[u8],
        stderr: &[u8],
        output: &[u8],
    ) -> Result<(), AttemptFailure> {
        let Some(bytes) = stdout
            .len()
            .checked_add(stderr.len())
            .and_then(|bytes| bytes.checked_add(output.len()))
        else {
            self.budget_cancellation.cancel();
            return Err(AttemptFailure::global_cancelled(format!(
                "run retained attempt artifact size overflowed for node {node}"
            )));
        };
        self.reserve_retained_output_bytes(node, bytes)
    }

    async fn record_outcome(
        &self,
        qualified_id: &str,
        outcome: NodeOutcome,
    ) -> Result<(), RunError> {
        let mut snapshot = self.snapshot.lock().await;
        let snapshot_terminal = outcome.status.is_terminal();
        snapshot.insert(qualified_id.to_owned(), outcome);
        let flush = snapshot_terminal && self.should_snapshot_flush();
        drop(snapshot);
        if flush {
            self.flush_summary_snapshot().await?;
        }
        Ok(())
    }

    async fn flush_summary_snapshot(&self) -> Result<(), RunError> {
        let snapshot = self.snapshot.lock().await;
        let value = summary_snapshot(&self.run_id, &snapshot);
        self.store.write_summary_snapshot(&value).await?;
        Ok(())
    }

    fn should_snapshot_flush(&self) -> bool {
        let terminal_count = self.snapshot_terminal_count.fetch_add(1, Ordering::AcqRel);
        (terminal_count + 1).is_multiple_of(SUMMARY_SNAPSHOT_FLUSH_EVERY)
    }

    async fn record_usage(&self, profile: String, reported_model: Option<String>, verified: bool) {
        self.usage.lock().await.push(ProviderUsage {
            profile,
            reported_model,
            verified,
        });
    }

    async fn store_attempt(
        &self,
        node_id: &str,
        attempt: u32,
        stdout: &[u8],
        stderr: &[u8],
        output: &[u8],
        output_is_json: bool,
    ) -> Result<AttemptArtifacts, RunError> {
        let written = self
            .store
            .write_attempt(node_id, attempt, stdout, stderr, output, output_is_json)
            .await?;
        let root = &self.store.paths().root;
        let references = [
            ("stdout", &written.stdout),
            ("stderr", &written.stderr),
            ("output", &written.output),
        ];
        let mut artifacts = self.artifacts.lock().await;
        for (kind, relative) in references {
            artifacts.push(self.store.reference(root.join(relative), kind).await?);
        }
        Ok(written)
    }
}

#[derive(Debug, Serialize)]
struct SummarySnapshot<'a> {
    schema_version: &'static str,
    run_id: &'a str,
    updated_at: DateTime<Utc>,
    nodes: IndexMap<String, NodeOutcome>,
}

fn summary_snapshot<'a>(
    run_id: &'a str,
    snapshot: &IndexMap<String, NodeOutcome>,
) -> SummarySnapshot<'a> {
    let mut compact = IndexMap::new();
    for (id, outcome) in snapshot {
        let mut compact_outcome = outcome.clone();
        compact_outcome.output = None;
        compact.insert(id.clone(), compact_outcome);
    }
    SummarySnapshot {
        schema_version: SUMMARY_SCHEMA_VERSION,
        run_id,
        updated_at: Utc::now(),
        nodes: compact,
    }
}

#[derive(Debug, Clone)]
struct ProviderUsage {
    profile: String,
    reported_model: Option<String>,
    verified: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GlobalCancelReason {
    External,
    Budget,
}

impl GlobalCancelReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::External => "external",
            Self::Budget => "budget",
        }
    }

    const fn message(self) -> &'static str {
        match self {
            Self::External => "run cancelled",
            Self::Budget => "run budget exhausted",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CancellationSource {
    Global(GlobalCancelReason),
    Scheduler,
}

impl CancellationSource {
    const fn message(self) -> &'static str {
        match self {
            Self::Global(reason) => reason.message(),
            Self::Scheduler => "cancelled by fail-fast policy",
        }
    }
}

async fn wait_for_cancellation(
    context: &RunContext,
    scheduler: &CancellationToken,
) -> CancellationSource {
    if let Some(reason) = context.global_cancel_reason() {
        return CancellationSource::Global(reason);
    }
    tokio::select! {
        biased;
        () = context.budget_cancellation.cancelled() => CancellationSource::Global(GlobalCancelReason::Budget),
        () = context.external_cancellation.cancelled() => CancellationSource::Global(GlobalCancelReason::External),
        () = scheduler.cancelled() => CancellationSource::Scheduler,
        () = sleep_until_deadline(context.deadline), if context.deadline.is_some() => {
            context.budget_cancellation.cancel();
            CancellationSource::Global(GlobalCancelReason::Budget)
        }
    }
}

async fn sleep_until_deadline(deadline: Option<Instant>) {
    if let Some(deadline) = deadline {
        time::sleep_until(time::Instant::from_std(deadline)).await;
    } else {
        std::future::pending::<()>().await;
    }
}

#[derive(Debug)]
struct AttemptSuccess {
    value: Value,
    raw_output: Vec<u8>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    output_is_json: bool,
    exit_code: Option<i32>,
    profile: Option<String>,
    model: Option<String>,
    selection_origin: Option<String>,
    model_origin: Option<String>,
    workspace: Option<String>,
}

impl AttemptSuccess {
    fn json(value: Value, workspace: Option<String>) -> Self {
        Self {
            raw_output: serde_json::to_vec(&value).unwrap_or_default(),
            value,
            stdout: Vec::new(),
            stderr: Vec::new(),
            output_is_json: true,
            exit_code: Some(0),
            profile: None,
            model: None,
            selection_origin: None,
            model_origin: None,
            workspace,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttemptFailureKind {
    Normal,
    Blocked,
    GlobalCancelled,
    SchedulerCancelled,
}

#[derive(Debug)]
struct AttemptFailure {
    kind: AttemptFailureKind,
    class: NodeFailureClass,
    retryable: bool,
    retry_forbidden: bool,
    message: String,
    details: Box<AttemptFailureDetails>,
}

#[derive(Debug)]
enum AttemptExecutionError {
    Failure(AttemptFailure),
    Fatal(RunError),
}

impl From<AttemptFailure> for AttemptExecutionError {
    fn from(error: AttemptFailure) -> Self {
        Self::Failure(error)
    }
}

impl From<RunError> for AttemptExecutionError {
    fn from(error: RunError) -> Self {
        Self::Fatal(error)
    }
}

#[derive(Debug, Default)]
struct AttemptFailureDetails {
    raw_output: Vec<u8>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    output_is_json: bool,
    exit_code: Option<i32>,
    profile: Option<String>,
    model: Option<String>,
    workspace: Option<String>,
}

impl std::ops::Deref for AttemptFailure {
    type Target = AttemptFailureDetails;

    fn deref(&self) -> &Self::Target {
        &self.details
    }
}

impl std::ops::DerefMut for AttemptFailure {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.details
    }
}

impl AttemptFailure {
    fn normal(message: impl Into<String>) -> Self {
        Self::new(
            AttemptFailureKind::Normal,
            NodeFailureClass::Execution,
            true,
            false,
            message,
        )
    }

    fn deterministic(message: impl Into<String>) -> Self {
        Self::new(
            AttemptFailureKind::Normal,
            NodeFailureClass::Execution,
            false,
            true,
            message,
        )
    }

    fn provider_protocol(message: impl Into<String>) -> Self {
        Self::new(
            AttemptFailureKind::Normal,
            NodeFailureClass::ProviderProtocol,
            false,
            true,
            message,
        )
    }

    fn blocked(message: impl Into<String>) -> Self {
        Self::new(
            AttemptFailureKind::Blocked,
            NodeFailureClass::HumanGate,
            false,
            true,
            message,
        )
    }

    fn global_cancelled(message: impl Into<String>) -> Self {
        Self::new(
            AttemptFailureKind::GlobalCancelled,
            NodeFailureClass::Budget,
            false,
            true,
            message,
        )
    }

    fn provider(error: &AdapterError) -> Self {
        let retryable = error.is_retryable();
        let class = provider_failure_class(error);
        let mut failure = Self::new(
            AttemptFailureKind::Normal,
            class,
            retryable,
            provider_retry_forbidden(error),
            error.to_string(),
        );
        if let AdapterError::ProcessFailed {
            profile,
            code,
            stdout,
            stderr,
            ..
        } = error
        {
            failure.profile = Some(profile.clone());
            failure.exit_code = *code;
            failure.stdout = stdout.as_bytes().to_vec();
            failure.stderr = stderr.as_bytes().to_vec();
        }
        failure
    }

    fn cancelled(source: CancellationSource) -> Self {
        let kind = match source {
            CancellationSource::Global(_) => AttemptFailureKind::GlobalCancelled,
            CancellationSource::Scheduler => AttemptFailureKind::SchedulerCancelled,
        };
        Self::new(
            kind,
            NodeFailureClass::Cancelled,
            false,
            true,
            source.message(),
        )
    }

    fn new(
        kind: AttemptFailureKind,
        class: NodeFailureClass,
        retryable: bool,
        retry_forbidden: bool,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            class,
            retryable,
            retry_forbidden,
            message: message.into(),
            details: Box::default(),
        }
    }
}

fn provider_failure_class(error: &AdapterError) -> NodeFailureClass {
    match error {
        AdapterError::ProfileNotFound(_) => NodeFailureClass::ProviderProfileNotFound,
        AdapterError::CapabilityMismatch { .. } | AdapterError::NoMatchingProfile { .. } => {
            NodeFailureClass::ProviderCapability
        }
        AdapterError::Disabled { .. }
        | AdapterError::MissingCredential { .. }
        | AdapterError::Unavailable { .. } => NodeFailureClass::ProviderUnavailable,
        AdapterError::InvalidRequest { .. } => NodeFailureClass::ProviderConfiguration,
        AdapterError::Spawn { .. } | AdapterError::ProcessFailed { .. } => {
            NodeFailureClass::ProviderProcess
        }
        AdapterError::Io { .. } | AdapterError::HttpTransport { .. } => {
            NodeFailureClass::ProviderTransient
        }
        AdapterError::Timeout { .. } => NodeFailureClass::ProviderTimeout,
        AdapterError::Cancelled { .. } => NodeFailureClass::ProviderCancelled,
        AdapterError::OutputTooLarge { .. } => NodeFailureClass::ProviderContextLength,
        AdapterError::InvalidOutput { .. } => NodeFailureClass::ProviderProtocol,
        AdapterError::HttpStatus { .. } => match error.class() {
            AdapterErrorClass::RateLimit => NodeFailureClass::ProviderRateLimit,
            AdapterErrorClass::Transient => NodeFailureClass::ProviderTransient,
            AdapterErrorClass::Authentication => NodeFailureClass::ProviderAuthentication,
            AdapterErrorClass::ContextLength => NodeFailureClass::ProviderContextLength,
            AdapterErrorClass::Protocol => NodeFailureClass::ProviderProtocol,
            AdapterErrorClass::Configuration => NodeFailureClass::ProviderConfiguration,
            AdapterErrorClass::Process => NodeFailureClass::ProviderProcess,
            AdapterErrorClass::Timeout => NodeFailureClass::ProviderTimeout,
            AdapterErrorClass::Cancelled => NodeFailureClass::ProviderCancelled,
        },
    }
}

/// Returns true when the provider may already have accepted or completed the
/// invocation. Retrying these failures, including through an explicit profile
/// rebind, could duplicate billing or provider-side effects.
fn provider_retry_forbidden(error: &AdapterError) -> bool {
    match error {
        // These failures are detected before a provider invocation starts.
        AdapterError::ProfileNotFound(_)
        | AdapterError::CapabilityMismatch { .. }
        | AdapterError::NoMatchingProfile { .. }
        | AdapterError::Disabled { .. }
        | AdapterError::MissingCredential { .. }
        | AdapterError::Unavailable { .. }
        | AdapterError::InvalidRequest { .. }
        | AdapterError::Spawn { .. } => false,
        // A concrete 4xx response is a provider rejection. The ambiguous
        // timeout/conflict/early-data statuses are kept fail-closed, as are
        // server errors whose response may have been produced after handling.
        AdapterError::HttpStatus { status, .. } => {
            matches!(*status, 408 | 409 | 425) || *status >= 500
        }
        AdapterError::ProcessFailed { .. }
        | AdapterError::Io { .. }
        | AdapterError::Timeout { .. }
        | AdapterError::Cancelled { .. }
        | AdapterError::OutputTooLarge { .. }
        | AdapterError::InvalidOutput { .. }
        | AdapterError::HttpTransport { .. } => true,
    }
}

fn encoded_failure(failure: &AttemptFailure) -> String {
    format!("[gloop:{}] {}", failure.class.code(), failure.message)
}

#[derive(Debug)]
struct CommandOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit_code: Option<i32>,
}

#[derive(Debug)]
struct ValidatedOutput {
    value: Value,
    raw: Vec<u8>,
    is_json: bool,
}

#[derive(Debug)]
struct NormalizedProviderOutput {
    value: Value,
    raw_output: Vec<u8>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    model: Option<String>,
    model_verified: bool,
}

#[derive(Debug)]
struct ProviderCandidate {
    profile: String,
    selection_origin: SelectionOrigin,
    model_origin: ModelOrigin,
    normalized: NormalizedProviderOutput,
    encoded_value: Vec<u8>,
}

fn dependency_decision(
    compiled: &CompiledGraph,
    node_id: &str,
    outcomes: &IndexMap<String, NodeOutcome>,
) -> DependencyDecision {
    let incoming = compiled.incoming_edges(node_id).collect::<Vec<_>>();
    if incoming.is_empty() {
        return DependencyDecision::Ready;
    }
    for edge in &incoming {
        let Some(source) = outcomes.get(&edge.from) else {
            return DependencyDecision::Skip(format!(
                "dependency {:?} has no runtime outcome",
                edge.from
            ));
        };
        if !source.status.is_terminal() {
            return DependencyDecision::Wait;
        }
    }
    for edge in incoming {
        let source = &outcomes[&edge.from];
        if !edge_matches(edge, source) {
            return DependencyDecision::Skip(format!(
                "edge from {:?} did not match its {:?} condition",
                edge.from, edge.kind
            ));
        }
    }
    DependencyDecision::Ready
}

fn edge_matches(edge: &Edge, source: &NodeOutcome) -> bool {
    let expected_status = match edge.kind {
        EdgeKind::Failure => NodeStatus::Failed,
        EdgeKind::Data | EdgeKind::Control | EdgeKind::Resource | EdgeKind::Conditional => {
            NodeStatus::Succeeded
        }
    };
    if edge.when.is_none() && source.status != expected_status {
        return false;
    }
    edge.when
        .as_ref()
        .is_none_or(|condition| condition_matches(condition, source, Some(expected_status)))
}

fn condition_matches(
    condition: &EdgeCondition,
    outcome: &NodeOutcome,
    default_status: Option<NodeStatus>,
) -> bool {
    if outcome.status
        != condition
            .status
            .or(default_status)
            .unwrap_or(outcome.status)
    {
        return false;
    }
    output_predicate_matches(
        outcome.output.as_ref(),
        condition.output_contains.as_deref(),
        condition.json_pointer.as_deref(),
        condition.equals.as_ref(),
    )
}

fn loop_condition_matches(condition: &LoopCondition, outcome: &NodeOutcome) -> bool {
    outcome.status == condition.status
        && output_predicate_matches(
            outcome.output.as_ref(),
            condition.output_contains.as_deref(),
            condition.json_pointer.as_deref(),
            condition.equals.as_ref(),
        )
}

fn output_predicate_matches(
    output: Option<&Value>,
    contains: Option<&str>,
    pointer: Option<&str>,
    equals: Option<&Value>,
) -> bool {
    if contains.is_none() && pointer.is_none() && equals.is_none() {
        return true;
    }
    let Some(output) = output else {
        return false;
    };
    if let Some(needle) = contains {
        let text = output
            .as_str()
            .map_or_else(|| output.to_string(), ToOwned::to_owned);
        if !text.contains(needle) {
            return false;
        }
    }
    let selected = pointer.map_or(Some(output), |pointer| output.pointer(pointer));
    let Some(selected) = selected else {
        return false;
    };
    equals.is_none_or(|expected| selected == expected)
}

fn dependency_outputs(
    compiled: &CompiledGraph,
    node_id: &str,
    outcomes: &IndexMap<String, NodeOutcome>,
) -> IndexMap<String, Value> {
    compiled
        .incoming_edges(node_id)
        .filter_map(|edge| {
            outcomes.get(&edge.from).and_then(|outcome| {
                outcome
                    .output
                    .clone()
                    .or_else(|| failure_dependency_metadata(outcome))
                    .map(|output| (edge.from.clone(), output))
            })
        })
        .collect()
}

fn failure_dependency_metadata(outcome: &NodeOutcome) -> Option<Value> {
    if !matches!(
        outcome.status,
        NodeStatus::Failed | NodeStatus::Blocked | NodeStatus::Cancelled
    ) {
        return None;
    }

    let mut metadata = serde_json::Map::new();
    metadata.insert("status".to_owned(), json!(outcome.status));
    metadata.insert("attempts".to_owned(), json!(outcome.attempts));
    if let Some(class) = node_failure_class(outcome) {
        metadata.insert("error_class".to_owned(), json!(class));
    }
    if let Some(error) = &outcome.error {
        metadata.insert("error".to_owned(), json!(error));
    }
    if let Some(exit_code) = outcome.exit_code {
        metadata.insert("exit_code".to_owned(), json!(exit_code));
    }
    for (key, artifact) in [
        ("output_artifact", &outcome.output_artifact),
        ("stdout_artifact", &outcome.stdout_artifact),
        ("stderr_artifact", &outcome.stderr_artifact),
    ] {
        if let Some(artifact) = artifact {
            metadata.insert(key.to_owned(), json!(artifact));
        }
    }
    Some(Value::Object(metadata))
}

async fn cancel_remaining(
    compiled: &CompiledGraph,
    namespace: &str,
    context: &RunContext,
    outcomes: &mut IndexMap<String, NodeOutcome>,
    message: &str,
    emit_node_events: bool,
) -> Result<(), RunError> {
    for id in &compiled.order {
        let outcome = outcomes.get_mut(id).expect("compiled node exists");
        if outcome.status.is_terminal() {
            continue;
        }
        outcome.status = NodeStatus::Cancelled;
        outcome.error = Some(message.to_owned());
        outcome.finished_at = Some(Utc::now());
        let qualified = qualify(namespace, id);
        if emit_node_events {
            context
                .emit(
                    RunEventKind::NodeBlocked,
                    Some(&qualified),
                    None,
                    Some(message.to_owned()),
                    json!({"status": "cancelled"}),
                )
                .await?;
        }
        context.record_outcome(&qualified, outcome.clone()).await?;
    }
    Ok(())
}

fn qualify(namespace: &str, node_id: &str) -> String {
    if namespace.is_empty() {
        node_id.to_owned()
    } else {
        format!("{namespace}.{node_id}")
    }
}

fn profile_for_attempt(node: &Node, attempt: u32) -> Option<&str> {
    if attempt <= 1 {
        return node.profile();
    }
    usize::try_from(attempt - 2)
        .ok()
        .and_then(|index| node.retry.rebind_profiles.get(index))
        .map(String::as_str)
        .or_else(|| node.profile())
}

fn gate_default(default: GateDefault) -> GateDecision {
    match default {
        GateDefault::Approve => GateDecision::Approve,
        GateDefault::Reject => GateDecision::Reject,
    }
}

fn outcome_fingerprint(outcome: &NodeOutcome) -> String {
    let payload = serde_json::to_vec(&(outcome.status, &outcome.output)).unwrap_or_default();
    hex::encode(Sha256::digest(payload))
}

fn graph_failed(outcomes: &IndexMap<String, NodeOutcome>) -> bool {
    outcomes.values().any(|outcome| {
        matches!(
            outcome.status,
            NodeStatus::Failed | NodeStatus::Blocked | NodeStatus::Cancelled
        )
    })
}

fn subgraph_failure(
    outcomes: &IndexMap<String, NodeOutcome>,
    workspace: Option<String>,
) -> AttemptFailure {
    let terminal_outcome = |outcome: &NodeOutcome| {
        matches!(
            outcome.status,
            NodeStatus::Failed | NodeStatus::Blocked | NodeStatus::Cancelled
        )
    };
    let provider_failure = |outcome: &NodeOutcome| {
        node_failure_class(outcome).is_some_and(|class| {
            matches!(
                class,
                NodeFailureClass::ProviderProfileNotFound
                    | NodeFailureClass::ProviderCapability
                    | NodeFailureClass::ProviderUnavailable
                    | NodeFailureClass::ProviderAuthentication
                    | NodeFailureClass::ProviderRateLimit
                    | NodeFailureClass::ProviderTransient
                    | NodeFailureClass::ProviderTimeout
                    | NodeFailureClass::ProviderContextLength
                    | NodeFailureClass::ProviderProtocol
                    | NodeFailureClass::ProviderConfiguration
                    | NodeFailureClass::ProviderProcess
                    | NodeFailureClass::ProviderCancelled
            )
        })
    };
    let underlying = failure_outcome(outcomes, terminal_outcome, provider_failure);
    let (kind, class, retryable, retry_forbidden, message, exit_code, profile, model) = underlying
        .map_or_else(
            || {
                (
                    AttemptFailureKind::Normal,
                    NodeFailureClass::Execution,
                    false,
                    true,
                    "subgraph did not complete successfully".to_owned(),
                    None,
                    None,
                    None,
                )
            },
            |(node_id, outcome)| {
                let class = node_failure_class(outcome).unwrap_or(NodeFailureClass::Execution);
                let kind = match outcome.status {
                    NodeStatus::Blocked => AttemptFailureKind::Blocked,
                    NodeStatus::Cancelled => AttemptFailureKind::SchedulerCancelled,
                    _ => AttemptFailureKind::Normal,
                };
                // A composite attempt may already contain successful provider
                // calls before a later child fails. Replaying the entire
                // subgraph/loop would duplicate those calls even when the last
                // failure itself (for example HTTP 429) is safe to retry.
                let retryable = false;
                let retry_forbidden = true;
                (
                    kind,
                    class,
                    retryable,
                    retry_forbidden,
                    format!(
                        "subgraph node {node_id:?} failed: {}",
                        outcome.error.as_deref().unwrap_or("unknown failure")
                    ),
                    outcome.exit_code,
                    outcome.profile.clone(),
                    outcome.model.clone(),
                )
            },
        );
    let mut failure = AttemptFailure::new(kind, class, retryable, retry_forbidden, message);
    failure.raw_output = serde_json::to_vec(outcomes).unwrap_or_default();
    failure.output_is_json = true;
    failure.exit_code = exit_code;
    failure.profile = profile;
    failure.model = model;
    failure.workspace = workspace;
    failure
}

fn failure_outcome<F, G>(
    outcomes: &IndexMap<String, NodeOutcome>,
    terminal_outcome: F,
    provider_failure: G,
) -> Option<(&String, &NodeOutcome)>
where
    F: Fn(&NodeOutcome) -> bool,
    G: Fn(&NodeOutcome) -> bool,
{
    outcomes
        .iter()
        .filter(|(_, outcome)| terminal_outcome(outcome))
        .find(|(_, outcome)| provider_failure(outcome))
        .or_else(|| {
            outcomes
                .iter()
                .filter(|(_, outcome)| terminal_outcome(outcome))
                .find(|(_, outcome)| {
                    node_failure_class(outcome)
                        .is_some_and(|class| class != NodeFailureClass::Execution)
                })
        })
        .or_else(|| {
            outcomes.iter().find(|(_, outcome)| {
                matches!(
                    outcome.status,
                    NodeStatus::Failed | NodeStatus::Blocked | NodeStatus::Cancelled
                )
            })
        })
}

fn apply_attempt_artifacts(outcome: &mut NodeOutcome, artifacts: &AttemptArtifacts) {
    outcome.stdout_artifact = Some(artifacts.stdout.clone());
    outcome.stderr_artifact = Some(artifacts.stderr.clone());
    outcome.output_artifact = Some(artifacts.output.clone());
}

fn artifact_event_data(
    artifacts: &AttemptArtifacts,
    outcome: &NodeOutcome,
    status: &str,
    class: NodeFailureClass,
) -> Value {
    json!({
        "status": status,
        "error_class": class,
        "exit_code": outcome.exit_code,
        "profile": outcome.profile,
        "model": outcome.model,
        "workspace": outcome.workspace,
        "output_artifact": artifacts.output,
        "stdout_artifact": artifacts.stdout,
        "stderr_artifact": artifacts.stderr,
    })
}

fn minimum_duration(first: Option<Duration>, second: Option<Duration>) -> Option<Duration> {
    match (first, second) {
        (Some(first), Some(second)) => Some(first.min(second)),
        (first, second) => first.or(second),
    }
}

fn minimum_u32(first: Option<u32>, second: Option<u32>) -> Option<u32> {
    match (first, second) {
        (Some(first), Some(second)) => Some(first.min(second)),
        (first, second) => first.or(second),
    }
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn final_status(
    graph: &Graph,
    outcomes: &IndexMap<String, NodeOutcome>,
    cancellation: Option<GlobalCancelReason>,
) -> FinalStatus {
    if cancellation == Some(GlobalCancelReason::Budget) {
        return FinalStatus::BudgetExhausted;
    }
    if cancellation == Some(GlobalCancelReason::External) {
        return FinalStatus::Cancelled;
    }
    if graph.spec.nodes.iter().any(|node| {
        matches!(node.kind, NodeKind::Verify { .. })
            && outcomes[&node.id].status == NodeStatus::Failed
    }) || outcomes.values().any(|outcome| {
        outcome.status == NodeStatus::Failed
            && node_failure_class(outcome) == Some(NodeFailureClass::Verification)
    }) {
        return FinalStatus::VerificationFailed;
    }
    if outcomes
        .values()
        .any(|outcome| outcome.status == NodeStatus::Failed)
    {
        return FinalStatus::Failed;
    }
    if outcomes
        .values()
        .any(|outcome| matches!(outcome.status, NodeStatus::Blocked | NodeStatus::Cancelled))
    {
        return FinalStatus::Blocked;
    }
    FinalStatus::ReadyForHuman
}

fn format_summary(
    status: FinalStatus,
    graph: &Graph,
    outcomes: &IndexMap<String, NodeOutcome>,
) -> String {
    const SUMMARY_LIMIT: usize = 32 * 1024;
    let output = graph
        .spec
        .nodes
        .iter()
        .rev()
        .filter(|node| !graph.spec.edges.iter().any(|edge| edge.from == node.id))
        .find_map(|node| summary_output(node, outcomes))
        .or_else(|| {
            graph
                .spec
                .nodes
                .iter()
                .rev()
                .find_map(|node| summary_output(node, outcomes))
        });
    if let Some(output) = output {
        let rendered = output
            .as_str()
            .map_or_else(|| output.to_string(), ToOwned::to_owned);
        return truncate_utf8(&rendered, SUMMARY_LIMIT);
    }
    let succeeded = outcomes
        .values()
        .filter(|outcome| outcome.status == NodeStatus::Succeeded)
        .count();
    let failed = outcomes
        .values()
        .filter(|outcome| outcome.status == NodeStatus::Failed)
        .count();
    let skipped = outcomes
        .values()
        .filter(|outcome| outcome.status == NodeStatus::Skipped)
        .count();
    format!(
        "run finished with {status:?}: {succeeded} succeeded, {failed} failed, {skipped} skipped"
    )
}

fn summary_output<'a>(
    node: &Node,
    outcomes: &'a IndexMap<String, NodeOutcome>,
) -> Option<&'a Value> {
    matches!(
        node.kind,
        NodeKind::Agent { .. } | NodeKind::Reduce { .. } | NodeKind::Synthesize { .. }
    )
    .then(|| outcomes.get(&node.id))
    .flatten()
    .filter(|outcome| outcome.status == NodeStatus::Succeeded)
    .and_then(|outcome| outcome.output.as_ref())
}

fn truncate_utf8(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_owned();
    }
    let mut boundary = limit;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    format!("{}…", &value[..boundary])
}

fn deduplicated_profiles(usage: &[ProviderUsage]) -> Vec<String> {
    usage
        .iter()
        .map(|item| item.profile.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn deduplicated_models(usage: &[ProviderUsage]) -> Vec<ModelUsage> {
    let mut models = BTreeMap::<(String, Option<String>), bool>::new();
    for item in usage {
        models
            .entry((item.profile.clone(), item.reported_model.clone()))
            .and_modify(|verified| *verified &= item.verified)
            .or_insert(item.verified);
    }
    models
        .into_iter()
        .map(|((profile, reported_model), verified)| ModelUsage {
            profile,
            reported_model,
            verified,
        })
        .collect()
}

fn verification_checks(
    graph: &Graph,
    outcomes: &IndexMap<String, NodeOutcome>,
) -> Vec<CheckResult> {
    graph
        .spec
        .nodes
        .iter()
        .filter_map(|node| {
            let NodeKind::Verify { argv, .. } = &node.kind else {
                return None;
            };
            Some(CheckResult {
                node: node.id.clone(),
                status: outcomes[&node.id].status,
                command: argv.clone(),
            })
        })
        .collect()
}

fn blocking_findings(outcomes: &IndexMap<String, NodeOutcome>) -> Vec<String> {
    outcomes
        .iter()
        .filter(|(_, outcome)| matches!(outcome.status, NodeStatus::Failed | NodeStatus::Blocked))
        .map(|(id, outcome)| {
            format!(
                "{id}: {}",
                outcome.error.as_deref().unwrap_or("no error detail")
            )
        })
        .collect()
}

fn unresolved_nodes(outcomes: &IndexMap<String, NodeOutcome>) -> Vec<String> {
    outcomes
        .iter()
        .filter(|(_, outcome)| {
            matches!(outcome.status, NodeStatus::Skipped | NodeStatus::Cancelled)
        })
        .map(|(id, outcome)| {
            format!(
                "{id}: {}",
                outcome.error.as_deref().unwrap_or("not executed")
            )
        })
        .collect()
}

#[derive(Debug)]
enum WorkspaceResolutionError {
    Attempt(String),
    Fatal(RunError),
}

fn graph_requires_worktree(graph: &Graph) -> bool {
    graph.spec.nodes.iter().any(|node| {
        matches!(node.workspace, WorkspaceSpec::Worktree { .. })
            || match &node.kind {
                NodeKind::Loop { graph, .. } | NodeKind::Subgraph { graph } => {
                    graph_requires_worktree(graph)
                }
                _ => false,
            }
    })
}

async fn resolve_workspace(
    input: &NodeInput,
    context: &RunContext,
) -> Result<ResolvedWorkspace, WorkspaceResolutionError> {
    let resolved = match &input.node.workspace {
        WorkspaceSpec::Current => {
            let path = stable_workspace(&input.default_workspace.path)
                .await
                .map_err(WorkspaceResolutionError::Attempt)?;
            let owner = input.default_workspace.owner.clone();
            if let Some(owner) = &owner {
                let manager = context.worktree_manager.as_ref().ok_or_else(|| {
                    WorkspaceResolutionError::Fatal(RunError::WorktreeManagerUnavailable {
                        node: input.qualified_id.clone(),
                    })
                })?;
                manager
                    .inherit_workspace(&owner.owner_node, &path)
                    .await
                    .map_err(|error| WorkspaceResolutionError::Fatal(error.into()))?;
            }
            ResolvedWorkspace { path, owner }
        }
        WorkspaceSpec::Inherit { node: source } => {
            let source_outcome = input.local_outcomes.get(source).ok_or_else(|| {
                WorkspaceResolutionError::Attempt(format!(
                    "workspace source node {source:?} does not exist"
                ))
            })?;
            if source_outcome.status != NodeStatus::Succeeded {
                return Err(WorkspaceResolutionError::Attempt(format!(
                    "workspace source node {source:?} did not succeed"
                )));
            }
            let source_workspace = source_outcome.workspace.as_deref().ok_or_else(|| {
                WorkspaceResolutionError::Attempt(format!(
                    "workspace source node {source:?} did not record a workspace"
                ))
            })?;
            let source_qualified_id = qualify(&input.namespace, source);
            let owner = context.worktree_workspace_for(&source_qualified_id).await;
            let path = if let Some(owner) = &owner {
                if source_workspace != owner.path.to_string_lossy() {
                    return Err(WorkspaceResolutionError::Fatal(
                        RunError::SchedulerInvariant(format!(
                            "recorded workspace disagrees with owner identity for node {source_qualified_id:?}"
                        )),
                    ));
                }
                let manager = context.worktree_manager.as_ref().ok_or_else(|| {
                    WorkspaceResolutionError::Fatal(RunError::WorktreeManagerUnavailable {
                        node: input.qualified_id.clone(),
                    })
                })?;
                manager
                    .inherit_workspace(&owner.owner_node, &owner.path)
                    .await
                    .map_err(|error| WorkspaceResolutionError::Fatal(error.into()))?
                    .path
            } else {
                let source_path = PathBuf::from(source_workspace);
                stable_workspace(&source_path)
                    .await
                    .map_err(WorkspaceResolutionError::Attempt)?
            };
            ResolvedWorkspace { path, owner }
        }
        WorkspaceSpec::Readonly => {
            return Err(WorkspaceResolutionError::Attempt(
                "readonly workspace mode is unavailable without an enforced filesystem sandbox"
                    .into(),
            ));
        }
        WorkspaceSpec::Worktree { base, auto_commit } => {
            let manager = context.worktree_manager.as_ref().ok_or_else(|| {
                WorkspaceResolutionError::Fatal(RunError::WorktreeManagerUnavailable {
                    node: input.qualified_id.clone(),
                })
            })?;
            let record = manager
                .workspace_for_node(&input.qualified_id, base.as_deref(), *auto_commit)
                .await
                .map_err(|error| WorkspaceResolutionError::Fatal(error.into()))?;
            ResolvedWorkspace {
                path: record.path.clone(),
                owner: Some(record.workspace()),
            }
        }
    };

    if let Some(owner) = &resolved.owner {
        context
            .record_worktree_workspace(&input.qualified_id, owner)
            .await
            .map_err(WorkspaceResolutionError::Fatal)?;
    }
    Ok(resolved)
}

async fn stable_workspace(current_dir: &Path) -> Result<PathBuf, String> {
    let canonical = fs::canonicalize(current_dir)
        .await
        .map_err(|error| format!("current workspace is unavailable: {error}"))?;
    if canonical != current_dir {
        return Err(format!(
            "current workspace identity changed from {} to {}",
            current_dir.display(),
            canonical.display()
        ));
    }
    if !fs::metadata(&canonical)
        .await
        .map_err(|error| format!("cannot inspect current workspace: {error}"))?
        .is_dir()
    {
        return Err("current workspace is not a directory".into());
    }
    Ok(canonical)
}

async fn validate_resolved_workspace(
    workspace: &ResolvedWorkspace,
    context: &RunContext,
    qualified_id: &str,
) -> Result<(), AttemptExecutionError> {
    let canonical = stable_workspace(&workspace.path)
        .await
        .map_err(AttemptFailure::deterministic)?;
    let Some(owner) = &workspace.owner else {
        return Ok(());
    };
    if canonical != owner.path {
        return Err(RunError::SchedulerInvariant(format!(
            "workspace identity changed for node {qualified_id:?}"
        ))
        .into());
    }
    let manager = context.worktree_manager.as_ref().ok_or_else(|| {
        AttemptExecutionError::Fatal(RunError::WorktreeManagerUnavailable {
            node: qualified_id.to_owned(),
        })
    })?;
    manager
        .inherit_workspace(&owner.owner_node, &canonical)
        .await
        .map_err(|error| AttemptExecutionError::Fatal(error.into()))?;
    Ok(())
}

async fn render_prompt(
    prompt: &PromptSpec,
    context: &ContextSpec,
    dependencies: &IndexMap<String, Value>,
    workspace: &Path,
    node_id: &str,
) -> Result<String, String> {
    let (mut rendered, variables) = match prompt {
        PromptSpec::Inline(value) => (value.clone(), IndexMap::new()),
        PromptSpec::Package {
            file, variables, ..
        } => {
            let path = contained_file(workspace, file).await?;
            let bytes = read_bounded_file(&path, context.max_bytes, "prompt package").await?;
            let value = String::from_utf8(bytes)
                .map_err(|_| format!("prompt package {} is not UTF-8", path.display()))?;
            (value, variables.clone())
        }
    };
    for (name, value) in variables {
        rendered = rendered.replace(&format!("{{{{{name}}}}}"), &value);
    }
    rendered = rendered.replace("{{node_id}}", node_id);

    let dependency_text = serde_json::to_string_pretty(dependencies)
        .map_err(|error| format!("failed to render dependency context: {error}"))?;
    if rendered.contains("{{dependencies}}") {
        rendered = rendered.replace("{{dependencies}}", &dependency_text);
    } else if context.include_dependencies && !dependencies.is_empty() {
        rendered.push_str("\n\nDependency outputs (JSON):\n");
        rendered.push_str(&dependency_text);
    }

    if rendered.len() > context.max_bytes {
        return Err(format!(
            "rendered prompt and dependency context exceeded the {} byte limit",
            context.max_bytes
        ));
    }

    for file in &context.files {
        let path = contained_file(workspace, file).await?;
        let header = format!("\n\nContext file: {}\n", file.to_string_lossy());
        let used = rendered
            .len()
            .checked_add(header.len())
            .ok_or_else(|| "rendered context byte count overflowed".to_owned())?;
        let remaining = context.max_bytes.checked_sub(used).ok_or_else(|| {
            format!(
                "rendered prompt and context exceeded the {} byte limit",
                context.max_bytes
            )
        })?;
        let bytes = read_bounded_file(&path, remaining, "context file").await?;
        let text = String::from_utf8(bytes)
            .map_err(|_| format!("context file {} is not UTF-8", path.display()))?;
        rendered.push_str(&header);
        rendered.push_str(&text);
    }
    Ok(rendered)
}

async fn read_bounded_file(
    path: &Path,
    limit: usize,
    description: &str,
) -> Result<Vec<u8>, String> {
    let metadata = fs::metadata(path).await.map_err(|error| {
        format!(
            "failed to inspect {description} {}: {error}",
            path.display()
        )
    })?;
    let declared_len = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
    if declared_len > limit {
        return Err(format!(
            "{description} {} exceeded the {limit} byte limit",
            path.display()
        ));
    }
    let file = fs::File::open(path)
        .await
        .map_err(|error| format!("failed to read {description} {}: {error}", path.display()))?;
    let read_limit = u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
    let mut reader = file.take(read_limit);
    let mut bytes = Vec::with_capacity(declared_len);
    reader
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| format!("failed to read {description} {}: {error}", path.display()))?;
    if bytes.len() > limit {
        return Err(format!(
            "{description} {} exceeded the {limit} byte limit",
            path.display()
        ));
    }
    Ok(bytes)
}

async fn contained_file(workspace: &Path, requested: &Path) -> Result<PathBuf, String> {
    let candidate = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        workspace.join(requested)
    };
    let canonical = fs::canonicalize(&candidate)
        .await
        .map_err(|error| format!("failed to resolve {}: {error}", candidate.display()))?;
    if !canonical.starts_with(workspace) {
        return Err(format!(
            "path {} escapes workspace {}",
            candidate.display(),
            workspace.display()
        ));
    }
    let metadata = fs::metadata(&canonical)
        .await
        .map_err(|error| format!("failed to inspect {}: {error}", canonical.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", canonical.display()));
    }
    Ok(canonical)
}

fn required_capabilities(
    node: &Node,
    output: &OutputSpec,
) -> Result<AdapterCapabilities, AttemptFailure> {
    let mut capabilities = vec![match output.format {
        GraphOutputFormat::Text => AdapterCapability::TextOutput,
        GraphOutputFormat::Json => AdapterCapability::JsonOutput,
    }];
    for requirement in &node.requires {
        let capability = match requirement.as_str() {
            "text_output" => AdapterCapability::TextOutput,
            "json_output" => AdapterCapability::JsonOutput,
            "json_lines_output" | "jsonl_output" => AdapterCapability::JsonLinesOutput,
            "streaming" => AdapterCapability::Streaming,
            "system_prompt" => AdapterCapability::SystemPrompt,
            "model_selection" => AdapterCapability::ModelSelection,
            "working_directory" => AdapterCapability::WorkingDirectory,
            "repository_read" | "repository-read" => AdapterCapability::RepositoryRead,
            "repository_write" | "repository-write" => AdapterCapability::RepositoryWrite,
            "tool_execution" | "tool-execution" => AdapterCapability::ToolExecution,
            "resume_session" | "resume-session" | "resume" => AdapterCapability::ResumeSession,
            "usage_reporting" | "usage-reporting" => AdapterCapability::UsageReporting,
            "native_sandbox" | "native-sandbox" => AdapterCapability::NativeSandbox,
            "permission_control" | "permission-control" => AdapterCapability::PermissionControl,
            unknown => {
                return Err(AttemptFailure::deterministic(format!(
                    "unknown provider capability requirement {unknown:?}"
                )));
            }
        };
        capabilities.push(capability);
    }
    Ok(AdapterCapabilities::new(capabilities))
}

fn provider_output_format(format: GraphOutputFormat) -> ProviderOutputFormat {
    match format {
        GraphOutputFormat::Text => ProviderOutputFormat::Text,
        GraphOutputFormat::Json => ProviderOutputFormat::Json,
    }
}

fn selection_origin_code(origin: SelectionOrigin) -> String {
    match origin {
        SelectionOrigin::Explicit => "explicit",
        SelectionOrigin::Capability => "capability",
    }
    .to_owned()
}

fn model_origin_code(origin: ModelOrigin) -> String {
    match origin {
        ModelOrigin::Request => "request",
        ModelOrigin::Profile => "profile",
        ModelOrigin::ProviderDefault => "provider_default",
    }
    .to_owned()
}

#[allow(clippy::too_many_lines)]
async fn execute_command(
    argv: &[String],
    env: &IndexMap<String, String>,
    workspace: &Path,
    output_limit: usize,
    cancellation: CancellationToken,
) -> Result<CommandOutput, AttemptFailure> {
    let Some(executable) = argv.first() else {
        return Err(AttemptFailure::normal("command argv is empty"));
    };
    let mut command = Command::new(executable);
    command
        .args(&argv[1..])
        .env_clear()
        .envs(command_environment(env))
        .current_dir(workspace)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().map_err(|error| {
        AttemptFailure::deterministic(format!("failed to start command: {error}"))
    })?;
    let process_group = child.id();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AttemptFailure::normal("command stdout pipe was unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| AttemptFailure::normal("command stderr pipe was unavailable"))?;
    let stderr_limit = output_limit.max(1024 * 1024);
    let mut stdout_task = tokio::spawn(read_limited(stdout, output_limit, "stdout"));
    let mut stderr_task = tokio::spawn(read_limited(stderr, stderr_limit, "stderr"));
    let mut status = None;
    let mut stdout = None;
    let mut stderr = None;
    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut pipe_drain_deadline = None;

    while status.is_none() || stdout.is_none() || stderr.is_none() {
        tokio::select! {
            result = child.wait(), if status.is_none() => {
                match result {
                    Ok(exit_status) => {
                        status = Some(exit_status);
                        if stdout.is_none() || stderr.is_none() {
                            pipe_drain_deadline = Some(Instant::now() + COMMAND_PIPE_DRAIN_GRACE);
                        }
                    }
                    Err(error) => {
                        cleanup_command(
                            &mut child,
                            process_group,
                            false,
                            &mut stdout_task,
                            stdout_done,
                            &mut stderr_task,
                            stderr_done,
                        )
                        .await;
                            return Err(AttemptFailure::deterministic(format!(
                                "failed to wait for command: {error}"
                            )));
                    }
                }
            }
            result = &mut stdout_task, if !stdout_done => {
                stdout_done = true;
                match join_read(result, "stdout") {
                    Ok(bytes) => stdout = Some(bytes),
                    Err(error) => {
                        cleanup_command(
                            &mut child,
                            process_group,
                            status.is_some(),
                            &mut stdout_task,
                            stdout_done,
                            &mut stderr_task,
                            stderr_done,
                        )
                        .await;
                        return Err(error);
                    }
                }
            }
            result = &mut stderr_task, if !stderr_done => {
                stderr_done = true;
                match join_read(result, "stderr") {
                    Ok(bytes) => stderr = Some(bytes),
                    Err(error) => {
                        cleanup_command(
                            &mut child,
                            process_group,
                            status.is_some(),
                            &mut stdout_task,
                            stdout_done,
                            &mut stderr_task,
                            stderr_done,
                        )
                        .await;
                        return Err(error);
                    }
                }
            }
            () = cancellation.cancelled() => {
                cleanup_command(
                    &mut child,
                    process_group,
                    status.is_some(),
                    &mut stdout_task,
                    stdout_done,
                    &mut stderr_task,
                    stderr_done,
                )
                .await;
                return Err(AttemptFailure::deterministic("command cancelled"));
            }
            () = sleep_until_deadline(pipe_drain_deadline), if pipe_drain_deadline.is_some() => {
                let exit_code = status.as_ref().and_then(std::process::ExitStatus::code);
                cleanup_command(
                    &mut child,
                    process_group,
                    true,
                    &mut stdout_task,
                    stdout_done,
                    &mut stderr_task,
                    stderr_done,
                )
                .await;
                let mut failure = AttemptFailure::deterministic(
                    "command output pipes remained open after the process exited",
                );
                failure.exit_code = exit_code;
                return Err(failure);
            }
        }
    }

    Ok(CommandOutput {
        stdout: stdout.expect("command stdout was collected"),
        stderr: stderr.expect("command stderr was collected"),
        exit_code: status.expect("command status was collected").code(),
    })
}

async fn cleanup_command(
    child: &mut tokio::process::Child,
    process_group: Option<u32>,
    child_reaped: bool,
    stdout_task: &mut tokio::task::JoinHandle<Result<Vec<u8>, String>>,
    stdout_done: bool,
    stderr_task: &mut tokio::task::JoinHandle<Result<Vec<u8>, String>>,
    stderr_done: bool,
) {
    #[cfg(unix)]
    {
        if let Some(process_group) = process_group {
            terminate_process_group(process_group).await;
        } else {
            let _ = child.start_kill();
        }
    }
    #[cfg(not(unix))]
    let _ = child.start_kill();
    if !child_reaped {
        let _ = child.wait().await;
    }
    if !stdout_done {
        stdout_task.abort();
        let _ = stdout_task.await;
    }
    if !stderr_done {
        stderr_task.abort();
        let _ = stderr_task.await;
    }
}

async fn read_limited(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
    stream: &'static str,
) -> Result<Vec<u8>, String> {
    let mut output = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .map_err(|error| format!("failed to read command {stream}: {error}"))?;
        if read == 0 {
            return Ok(output);
        }
        let next = output
            .len()
            .checked_add(read)
            .ok_or_else(|| format!("command {stream} size overflowed"))?;
        if next > limit {
            return Err(format!("command {stream} exceeded {limit} bytes"));
        }
        output.extend_from_slice(&buffer[..read]);
    }
}

fn join_read(
    result: Result<Result<Vec<u8>, String>, tokio::task::JoinError>,
    stream: &'static str,
) -> Result<Vec<u8>, AttemptFailure> {
    result
        .map_err(|error| {
            AttemptFailure::deterministic(format!("{stream} reader task failed: {error}"))
        })?
        .map_err(AttemptFailure::deterministic)
}

#[allow(clippy::too_many_lines)]
async fn normalize_provider_response(
    response: AdapterResponse,
    selected_profile: String,
    effective_model: Option<String>,
    output: &OutputSpec,
    workspace: &Path,
) -> Result<NormalizedProviderOutput, AttemptFailure> {
    let AdapterResponse {
        output: adapter_output,
        stdout,
        stderr,
        exit_code,
        reported_model,
        ..
    } = response;
    let output_is_json = output.format == GraphOutputFormat::Json
        || !matches!(&adapter_output, AdapterOutput::Text(_));
    let raw_output =
        match &adapter_output {
            AdapterOutput::Text(text) => text.as_bytes().to_vec(),
            AdapterOutput::Json(value) => serde_json::to_vec(value)
                .map_err(|error| AttemptFailure::normal(error.to_string()))?,
            AdapterOutput::JsonLines(values) => serde_json::to_vec(values)
                .map_err(|error| AttemptFailure::normal(error.to_string()))?,
        };
    if exit_code.is_some_and(|code| code != 0) {
        return Err(provider_response_failure(
            format!(
                "provider process exited with status {}",
                exit_code.unwrap_or_default()
            ),
            &raw_output,
            output_is_json,
            &stdout,
            &stderr,
            exit_code,
            &selected_profile,
            reported_model.as_deref().or(effective_model.as_deref()),
            NodeFailureClass::ProviderProcess,
            false,
        ));
    }

    if let (Some(reported), Some(requested)) = (&reported_model, &effective_model)
        && reported != requested
    {
        return Err(provider_response_failure(
            format!(
                "provider reported model {reported:?}, but the effective requested model was {requested:?}"
            ),
            &raw_output,
            output_is_json,
            &stdout,
            &stderr,
            exit_code,
            &selected_profile,
            Some(reported),
            NodeFailureClass::ProviderProtocol,
            true,
        ));
    }

    if matches!(&adapter_output, AdapterOutput::Text(text) if text.trim().is_empty()) {
        return Err(provider_response_failure(
            "provider returned blank text output",
            &raw_output,
            output_is_json,
            &stdout,
            &stderr,
            exit_code,
            &selected_profile,
            reported_model.as_deref().or(effective_model.as_deref()),
            NodeFailureClass::ProviderProtocol,
            true,
        ));
    }

    let bytes = match (&adapter_output, output.format) {
        (AdapterOutput::Text(_), _)
        | (AdapterOutput::Json(_) | AdapterOutput::JsonLines(_), GraphOutputFormat::Json) => {
            raw_output.clone()
        }
        (AdapterOutput::Json(_) | AdapterOutput::JsonLines(_), GraphOutputFormat::Text) => {
            return Err(provider_response_failure(
                "provider returned structured output for a text node",
                &raw_output,
                output_is_json,
                &stdout,
                &stderr,
                exit_code,
                &selected_profile,
                reported_model.as_deref().or(effective_model.as_deref()),
                NodeFailureClass::ProviderProtocol,
                true,
            ));
        }
    };
    let validated = match validate_bytes(&bytes, output, workspace).await {
        Ok(validated) => validated,
        Err(error) => {
            return Err(provider_response_failure(
                error,
                &raw_output,
                output_is_json,
                &stdout,
                &stderr,
                exit_code,
                &selected_profile,
                reported_model.as_deref().or(effective_model.as_deref()),
                NodeFailureClass::ProviderProtocol,
                true,
            ));
        }
    };
    let model_verified = reported_model.is_some();
    Ok(NormalizedProviderOutput {
        value: validated.value,
        raw_output: validated.raw,
        stdout: stdout.into_bytes(),
        stderr: stderr.into_bytes(),
        model: reported_model.or(effective_model),
        model_verified,
    })
}

#[allow(clippy::too_many_arguments)]
fn provider_response_failure(
    message: impl Into<String>,
    raw_output: &[u8],
    output_is_json: bool,
    stdout: &str,
    stderr: &str,
    exit_code: Option<i32>,
    profile: &str,
    model: Option<&str>,
    class: NodeFailureClass,
    retry_forbidden: bool,
) -> AttemptFailure {
    let mut failure = AttemptFailure::new(
        AttemptFailureKind::Normal,
        class,
        false,
        retry_forbidden,
        message,
    );
    failure.raw_output = raw_output.to_vec();
    failure.stdout = stdout.as_bytes().to_vec();
    failure.stderr = stderr.as_bytes().to_vec();
    failure.output_is_json = output_is_json;
    failure.exit_code = exit_code;
    failure.profile = Some(profile.to_owned());
    failure.model = model.map(ToOwned::to_owned);
    failure
}

fn aggregate_provider_limit(
    current: usize,
    additional: usize,
    limit: usize,
    stream: &'static str,
) -> Result<usize, AttemptFailure> {
    let next = current.checked_add(additional).ok_or_else(|| {
        AttemptFailure::new(
            AttemptFailureKind::Normal,
            NodeFailureClass::ProviderContextLength,
            false,
            true,
            format!("combined provider {stream} size overflowed"),
        )
    })?;
    if next > limit {
        return Err(AttemptFailure::new(
            AttemptFailureKind::Normal,
            NodeFailureClass::ProviderContextLength,
            false,
            true,
            format!("combined provider {stream} exceeded {limit} bytes"),
        ));
    }
    Ok(next)
}

fn fanout_candidate_output_limit(
    aggregate_limit: usize,
    fan_out: usize,
    format: GraphOutputFormat,
) -> Result<usize, AttemptFailure> {
    if fan_out <= 1 {
        return Ok(aggregate_limit);
    }
    let array_bytes = fan_out
        .checked_add(1)
        .ok_or_else(|| AttemptFailure::deterministic("fan-out output structure size overflowed"))?;
    let quote_bytes = if format == GraphOutputFormat::Text {
        fan_out.checked_mul(2).ok_or_else(|| {
            AttemptFailure::deterministic("fan-out output structure size overflowed")
        })?
    } else {
        0
    };
    let structural_bytes = array_bytes
        .checked_add(quote_bytes)
        .ok_or_else(|| AttemptFailure::deterministic("fan-out output structure size overflowed"))?;
    let output_payload_bytes = aggregate_limit
        .checked_sub(structural_bytes)
        .ok_or_else(|| {
            AttemptFailure::new(
                AttemptFailureKind::Normal,
                NodeFailureClass::ProviderContextLength,
                false,
                true,
                format!("combined provider output exceeded {aggregate_limit} bytes"),
            )
        })?;
    let mut header_bytes = 0_usize;
    for index in 0..fan_out {
        header_bytes = header_bytes
            .checked_add(format!("--- candidate {} ---\n", index + 1).len())
            .ok_or_else(|| {
                AttemptFailure::deterministic("fan-out stream header size overflowed")
            })?;
    }
    let stream_payload_bytes = aggregate_limit.checked_sub(header_bytes).ok_or_else(|| {
        AttemptFailure::new(
            AttemptFailureKind::Normal,
            NodeFailureClass::ProviderContextLength,
            false,
            true,
            format!("combined provider streams exceeded {aggregate_limit} bytes"),
        )
    })?;
    let candidate_limit = output_payload_bytes.min(stream_payload_bytes) / fan_out;
    if candidate_limit == 0 {
        return Err(AttemptFailure::new(
            AttemptFailureKind::Normal,
            NodeFailureClass::ProviderContextLength,
            false,
            true,
            format!("combined provider output exceeded {aggregate_limit} bytes"),
        ));
    }
    Ok(candidate_limit)
}

fn enforce_fanout_prompt_limit(
    prompt: &str,
    fan_out: usize,
    aggregate_limit: usize,
) -> Result<(), AttemptFailure> {
    let mut aggregate_bytes = 0_usize;
    for index in 0..fan_out {
        aggregate_bytes = aggregate_bytes
            .checked_add(prompt.len())
            .ok_or_else(|| AttemptFailure::deterministic("fan-out prompt size overflowed"))?;
        if fan_out > 1 {
            let suffix = format!("\n\nFan-out candidate: {}/{}", index + 1, fan_out);
            aggregate_bytes = aggregate_bytes
                .checked_add(suffix.len())
                .ok_or_else(|| AttemptFailure::deterministic("fan-out prompt size overflowed"))?;
        }
        if aggregate_bytes > aggregate_limit {
            return Err(AttemptFailure::new(
                AttemptFailureKind::Normal,
                NodeFailureClass::ProviderContextLength,
                false,
                true,
                format!("combined fan-out prompts exceeded {aggregate_limit} bytes"),
            ));
        }
    }
    Ok(())
}

async fn validate_bytes(
    bytes: &[u8],
    output: &OutputSpec,
    workspace: &Path,
) -> Result<ValidatedOutput, String> {
    if bytes.len() > output.max_bytes {
        return Err(format!(
            "node output exceeded the {} byte limit",
            output.max_bytes
        ));
    }
    let (value, raw, is_json) = match output.format {
        GraphOutputFormat::Text => {
            let text = String::from_utf8(bytes.to_vec())
                .map_err(|_| "text output is not valid UTF-8".to_owned())?;
            (Value::String(text), bytes.to_vec(), false)
        }
        GraphOutputFormat::Json => {
            let value: Value = serde_json::from_slice(bytes)
                .map_err(|error| format!("output is not valid JSON: {error}"))?;
            let raw = serde_json::to_vec(&value)
                .map_err(|error| format!("failed to normalize JSON output: {error}"))?;
            (value, raw, true)
        }
    };
    validate_schema(&value, output, workspace).await?;
    Ok(ValidatedOutput {
        value,
        raw,
        is_json,
    })
}

async fn validate_schema(
    value: &Value,
    output: &OutputSpec,
    workspace: &Path,
) -> Result<(), String> {
    let schema = match (&output.inline_schema, &output.schema) {
        (None, None) => return Ok(()),
        (Some(_), Some(_)) => {
            return Err("output declares both inline_schema and schema file".into());
        }
        (Some(schema), None) => schema.clone(),
        (None, Some(path)) => {
            let path = contained_file(workspace, path).await?;
            let bytes = read_bounded_file(&path, MAX_SCHEMA_BYTES, "output schema").await?;
            serde_json::from_slice(&bytes)
                .map_err(|error| format!("schema {} is not valid JSON: {error}", path.display()))?
        }
    };
    let validator = jsonschema::validator_for(&schema)
        .map_err(|error| format!("invalid output JSON Schema: {error}"))?;
    let errors = validator
        .iter_errors(value)
        .take(4)
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "output failed JSON Schema validation: {}",
            errors.join("; ")
        ))
    }
}

#[derive(Debug, Error)]
pub enum RunError {
    #[error(transparent)]
    Graph(#[from] gloop_core::GraphError),
    #[error(transparent)]
    Artifact(#[from] ArtifactError),
    #[error(transparent)]
    Journal(#[from] JournalError),
    #[error(transparent)]
    Worktree(#[from] WorktreeError),
    #[error("run I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to open current directory {path}: {source}")]
    CurrentDirectory {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("current workspace is not a directory: {0}")]
    CurrentDirectoryNotDirectory(PathBuf),
    #[error("max_parallel is outside the runtime-supported range")]
    InvalidParallelism,
    #[error("worktree manager is unavailable for node {node:?}")]
    WorktreeManagerUnavailable { node: String },
    #[error("scheduler invariant failed: {0}")]
    SchedulerInvariant(String),
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Mutex as StdMutex,
        atomic::{AtomicBool, AtomicUsize},
    };

    use gloop_core::{Edge, Graph, Node, graph::EdgeCondition};
    use gloop_provider::TokenUsage;

    use super::*;

    #[derive(Debug, Clone, Copy)]
    enum FakeMode {
        Echo,
        FailFirst,
        AmbiguousFailure,
        Loop,
        Conditional,
    }

    #[derive(Debug)]
    struct FakeProvider {
        mode: FakeMode,
        delay: Duration,
        calls: AtomicUsize,
        active: AtomicUsize,
        max_active: AtomicUsize,
        failed_once: AtomicBool,
        preferred: StdMutex<Vec<Option<String>>>,
        records: StdMutex<Vec<CallRecord>>,
    }

    #[derive(Debug, Clone)]
    struct CallRecord {
        prompt: String,
        started: Instant,
        finished: Instant,
    }

    impl FakeProvider {
        fn new(mode: FakeMode, delay: Duration) -> Self {
            Self {
                mode,
                delay,
                calls: AtomicUsize::new(0),
                active: AtomicUsize::new(0),
                max_active: AtomicUsize::new(0),
                failed_once: AtomicBool::new(false),
                preferred: StdMutex::new(Vec::new()),
                records: StdMutex::new(Vec::new()),
            }
        }

        fn update_max(&self, active: usize) {
            let mut observed = self.max_active.load(Ordering::Relaxed);
            while active > observed {
                match self.max_active.compare_exchange_weak(
                    observed,
                    active,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return,
                    Err(actual) => observed = actual,
                }
            }
        }
    }

    #[async_trait]
    impl ProviderInvoker for FakeProvider {
        async fn execute(
            &self,
            preferred_profile: Option<&str>,
            _required: &AdapterCapabilities,
            request: AdapterRequest,
            _cancellation: CancellationToken,
        ) -> Result<ProviderInvocation, AdapterError> {
            self.preferred
                .lock()
                .expect("preferred lock")
                .push(preferred_profile.map(ToOwned::to_owned));
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.update_max(active);
            let started = Instant::now();
            time::sleep(self.delay).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            self.records.lock().expect("records lock").push(CallRecord {
                prompt: request.prompt.clone(),
                started,
                finished: Instant::now(),
            });
            if matches!(self.mode, FakeMode::FailFirst)
                && !self.failed_once.swap(true, Ordering::SeqCst)
            {
                return Err(AdapterError::InvalidRequest {
                    profile: preferred_profile.unwrap_or("default").to_owned(),
                    message: "planned failure".to_owned(),
                });
            }
            if matches!(self.mode, FakeMode::AmbiguousFailure)
                && !self.failed_once.swap(true, Ordering::SeqCst)
            {
                return Err(AdapterError::HttpStatus {
                    profile: preferred_profile.unwrap_or("default").to_owned(),
                    status: 503,
                    error_type: Some("upstream_error".to_owned()),
                    error_code: None,
                });
            }
            let output = match self.mode {
                FakeMode::Loop => AdapterOutput::Text(if call == 0 {
                    "keep going".to_owned()
                } else {
                    "done".to_owned()
                }),
                FakeMode::Conditional if request.output_format == ProviderOutputFormat::Json => {
                    AdapterOutput::Json(json!({"route": "yes"}))
                }
                FakeMode::Echo
                | FakeMode::FailFirst
                | FakeMode::AmbiguousFailure
                | FakeMode::Conditional => AdapterOutput::Text(request.prompt),
            };
            Ok(ProviderInvocation {
                profile: preferred_profile.unwrap_or("default").to_owned(),
                selected_model: Some("fake-model".to_owned()),
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
                    reported_model: Some("fake-model".to_owned()),
                    usage: Some(TokenUsage::default()),
                },
            })
        }
    }

    fn runtime(
        temporary: &tempfile::TempDir,
        provider: Arc<FakeProvider>,
    ) -> (Runtime, RunOptions) {
        (
            Runtime::from_invoker(provider, temporary.path().join("runs")),
            RunOptions {
                current_dir: temporary.path().to_path_buf(),
                ..RunOptions::default()
            },
        )
    }

    #[tokio::test]
    async fn caps_parallel_nodes_at_max_parallel() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let provider = Arc::new(FakeProvider::new(FakeMode::Echo, Duration::from_millis(40)));
        let (runtime, options) = runtime(&temporary, Arc::clone(&provider));
        let mut graph = Graph::new(
            "parallel",
            "parallel test",
            vec![
                Node::agent("one", "{{node_id}}"),
                Node::agent("two", "{{node_id}}"),
                Node::agent("three", "{{node_id}}"),
            ],
        );
        graph.spec.policies.max_parallel = 2;
        let summary = runtime.run(&graph, options).await.expect("run succeeds");
        assert_eq!(summary.status, FinalStatus::ReadyForHuman);
        assert_eq!(provider.max_active.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn serializes_nodes_claiming_the_same_resource() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let provider = Arc::new(FakeProvider::new(FakeMode::Echo, Duration::from_millis(35)));
        let (runtime, options) = runtime(&temporary, Arc::clone(&provider));
        let mut first = Node::agent("first", "{{node_id}}");
        first.resources.push("workspace".to_owned());
        let mut second = Node::agent("second", "{{node_id}}");
        second.resources.push("workspace".to_owned());
        let third = Node::agent("third", "{{node_id}}");
        let mut graph = Graph::new("resources", "resource test", vec![first, second, third]);
        graph.spec.policies.max_parallel = 2;
        runtime.run(&graph, options).await.expect("run succeeds");
        let records = provider.records.lock().expect("records lock");
        let first = records
            .iter()
            .find(|record| record.prompt == "first")
            .expect("first record");
        let second = records
            .iter()
            .find(|record| record.prompt == "second")
            .expect("second record");
        assert!(first.finished <= second.started || second.finished <= first.started);
    }

    #[tokio::test]
    async fn retries_with_an_explicit_rebound_profile() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let provider = Arc::new(FakeProvider::new(FakeMode::FailFirst, Duration::ZERO));
        let (runtime, options) = runtime(&temporary, Arc::clone(&provider));
        let mut node = Node::agent("retry", "try");
        let NodeKind::Agent { profile, .. } = &mut node.kind else {
            panic!("agent node")
        };
        *profile = Some("primary".to_owned());
        node.retry.max_attempts = 2;
        node.retry.rebind_profiles.push("backup".to_owned());
        let graph = Graph::new("retry", "retry test", vec![node]);
        let summary = runtime.run(&graph, options).await.expect("run completes");
        assert_eq!(summary.nodes["retry"].status, NodeStatus::Succeeded);
        assert_eq!(summary.nodes["retry"].attempts, 2);
        assert_eq!(summary.nodes["retry"].profile.as_deref(), Some("backup"));
        assert_eq!(
            *provider.preferred.lock().expect("preferred lock"),
            [Some("primary".to_owned()), Some("backup".to_owned())]
        );
    }

    #[tokio::test]
    async fn explicit_rebind_does_not_repeat_an_ambiguous_http_failure() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let provider = Arc::new(FakeProvider::new(
            FakeMode::AmbiguousFailure,
            Duration::ZERO,
        ));
        let (runtime, options) = runtime(&temporary, Arc::clone(&provider));
        let mut node = Node::agent("retry", "try");
        let NodeKind::Agent { profile, .. } = &mut node.kind else {
            panic!("agent node")
        };
        *profile = Some("primary".to_owned());
        node.retry.max_attempts = 2;
        node.retry.rebind_profiles.push("backup".to_owned());

        let graph = Graph::new("retry", "retry safety test", vec![node]);
        let summary = runtime.run(&graph, options).await.expect("run completes");

        assert_eq!(summary.nodes["retry"].status, NodeStatus::Failed);
        assert_eq!(summary.nodes["retry"].attempts, 1);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            *provider.preferred.lock().expect("preferred lock"),
            [Some("primary".to_owned())]
        );
    }

    #[tokio::test]
    async fn bounded_loop_stops_when_its_typed_condition_matches() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let provider = Arc::new(FakeProvider::new(FakeMode::Loop, Duration::ZERO));
        let (runtime, options) = runtime(&temporary, provider);
        let inner = Graph::new(
            "inner",
            "loop body",
            vec![Node::agent("result", "iteration")],
        );
        let mut node = Node::agent("loop", "unused");
        node.kind = NodeKind::Loop {
            graph: Box::new(inner),
            until: LoopCondition {
                node: "result".to_owned(),
                status: NodeStatus::Succeeded,
                output_contains: Some("done".to_owned()),
                json_pointer: None,
                equals: None,
            },
            max_iterations: 3,
            stagnation_after: 2,
        };
        let graph = Graph::new("looping", "loop test", vec![node]);
        let summary = runtime.run(&graph, options).await.expect("run completes");
        assert_eq!(summary.nodes["loop"].status, NodeStatus::Succeeded);
        assert_eq!(
            summary.nodes["loop"].output.as_ref().unwrap()["iterations"],
            2
        );
    }

    #[tokio::test]
    async fn typed_edge_condition_runs_only_the_matching_branch() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let provider = Arc::new(FakeProvider::new(FakeMode::Conditional, Duration::ZERO));
        let (runtime, options) = runtime(&temporary, provider);
        let mut source = Node::agent("source", "source");
        let NodeKind::Agent { output, .. } = &mut source.kind else {
            panic!("agent node")
        };
        output.format = GraphOutputFormat::Json;
        let yes = Node::agent("yes", "yes");
        let no = Node::agent("no", "no");
        let mut graph = Graph::new("conditions", "condition test", vec![source, yes, no]);
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
        let summary = runtime.run(&graph, options).await.expect("run completes");
        assert_eq!(summary.nodes["yes"].status, NodeStatus::Succeeded);
        assert_eq!(summary.nodes["no"].status, NodeStatus::Skipped);
    }

    #[tokio::test]
    async fn completed_run_replays_to_the_same_root_node_states() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let provider = Arc::new(FakeProvider::new(FakeMode::Echo, Duration::ZERO));
        let (runtime, mut options) = runtime(&temporary, provider);
        options.run_id = Some("replay-run".to_owned());
        let graph = Graph::new("replay", "replay test", vec![Node::agent("node", "output")]);
        let summary = runtime.run(&graph, options).await.expect("run completes");
        let replay = crate::replay_journal(temporary.path().join("runs/replay-run/journal.jsonl"))
            .await
            .expect("replay succeeds");
        assert_eq!(replay.final_status, Some(summary.status));
        assert_eq!(replay.nodes["node"].status, summary.nodes["node"].status);
        assert!(!replay.truncated_tail);
    }
}

type NodeFuture = Pin<
    Box<dyn Future<Output = (String, Vec<String>, Result<NodeOutcome, RunError>)> + Send + 'static>,
>;

#[derive(Debug)]
struct NodeInput {
    node: Node,
    qualified_id: String,
    dependencies: IndexMap<String, Value>,
    local_outcomes: IndexMap<String, NodeOutcome>,
    parallel_limit: usize,
    namespace: String,
    default_workspace: ResolvedWorkspace,
}

#[derive(Debug)]
struct GraphExecution {
    outcomes: IndexMap<String, NodeOutcome>,
}

#[derive(Debug)]
enum DependencyDecision {
    Wait,
    Ready,
    Skip(String),
}

impl Runtime {
    #[allow(clippy::too_many_lines)]
    async fn execute_node(
        &self,
        input: NodeInput,
        context: Arc<RunContext>,
        scheduler_cancellation: CancellationToken,
    ) -> Result<NodeOutcome, RunError> {
        let started_at = Utc::now();
        let started = Instant::now();
        let mut outcome = NodeOutcome {
            status: NodeStatus::Running,
            started_at: Some(started_at),
            ..NodeOutcome::default()
        };
        let max_attempts = input.node.retry.max_attempts;
        let resolved_workspace = match resolve_workspace(&input, &context).await {
            Ok(workspace) => Ok(workspace),
            Err(WorkspaceResolutionError::Attempt(message)) => Err(message),
            Err(WorkspaceResolutionError::Fatal(error)) => return Err(error),
        };

        for attempt in 1..=max_attempts {
            outcome.attempts = attempt;
            let requested_profile = profile_for_attempt(&input.node, attempt);
            context
                .emit(
                    RunEventKind::NodeStarted,
                    Some(&input.qualified_id),
                    Some(attempt),
                    None,
                    json!({
                        "profile": requested_profile,
                        "fan_out": input.node.fan_out(),
                    }),
                )
                .await?;

            let attempt_cancellation = CancellationToken::new();
            let attempt_future = async {
                match &resolved_workspace {
                    Ok(workspace) => {
                        validate_resolved_workspace(workspace, &context, &input.qualified_id)
                            .await?;
                        self.execute_attempt(
                            &input,
                            &context,
                            attempt,
                            requested_profile,
                            workspace,
                            attempt_cancellation.clone(),
                        )
                        .await
                    }
                    Err(message) => Err(AttemptExecutionError::from(
                        AttemptFailure::deterministic(message.clone()),
                    )),
                }
            };
            tokio::pin!(attempt_future);
            let result = if let Some(timeout_seconds) = input.node.timeout_seconds {
                tokio::select! {
                    biased;
                    source = wait_for_cancellation(&context, &scheduler_cancellation) => {
                        attempt_cancellation.cancel();
                        let _cleanup = time::timeout(CANCELLATION_GRACE, &mut attempt_future).await;
                        Err(AttemptFailure::cancelled(source).into())
                    }
                    timed = time::timeout(Duration::from_secs(timeout_seconds), &mut attempt_future) => {
                        if let Ok(result) = timed {
                            result
                        } else {
                            attempt_cancellation.cancel();
                            let _cleanup = time::timeout(CANCELLATION_GRACE, &mut attempt_future).await;
                            Err(AttemptFailure::deterministic(format!(
                                "node timed out after {timeout_seconds} seconds"
                            )).into())
                        }
                    }
                }
            } else {
                tokio::select! {
                    biased;
                    source = wait_for_cancellation(&context, &scheduler_cancellation) => {
                        attempt_cancellation.cancel();
                        let _cleanup = time::timeout(CANCELLATION_GRACE, &mut attempt_future).await;
                        Err(AttemptFailure::cancelled(source).into())
                    }
                    result = &mut attempt_future => result,
                }
            };

            match result {
                Ok(success) => {
                    if let Err(mut failure) = context.reserve_attempt_artifact_bytes(
                        &input.qualified_id,
                        &success.stdout,
                        &success.stderr,
                        &success.raw_output,
                    ) {
                        failure.profile = success.profile;
                        failure.model = success.model;
                        failure.exit_code = success.exit_code;
                        failure.workspace = success.workspace;
                        outcome.status = NodeStatus::Cancelled;
                        outcome.profile = failure.profile.clone();
                        outcome.model = failure.model.clone();
                        outcome.exit_code = failure.exit_code;
                        outcome.workspace = failure.workspace.clone();
                        outcome.error = Some(encoded_failure(&failure));
                        outcome.finished_at = Some(Utc::now());
                        outcome.duration_ms = Some(duration_millis(started.elapsed()));
                        context
                            .emit(
                                RunEventKind::NodeBlocked,
                                Some(&input.qualified_id),
                                Some(attempt),
                                Some(encoded_failure(&failure)),
                                json!({
                                    "status": "cancelled",
                                    "error_class": failure.class,
                                    "exit_code": failure.exit_code,
                                    "artifacts_retained": false,
                                }),
                            )
                            .await?;
                        return Ok(outcome);
                    }
                    let artifacts = context
                        .store_attempt(
                            &input.qualified_id,
                            attempt,
                            &success.stdout,
                            &success.stderr,
                            &success.raw_output,
                            success.output_is_json,
                        )
                        .await?;
                    apply_attempt_artifacts(&mut outcome, &artifacts);
                    if matches!(
                        &input.node.workspace,
                        WorkspaceSpec::Worktree { .. } | WorkspaceSpec::Inherit { .. }
                    ) && let Some(owner) = resolved_workspace
                        .as_ref()
                        .expect("successful attempt has a resolved workspace")
                        .owner
                        .as_ref()
                    {
                        let manager = context.worktree_manager.as_ref().ok_or_else(|| {
                            RunError::WorktreeManagerUnavailable {
                                node: input.qualified_id.clone(),
                            }
                        })?;
                        manager.finish_workspace_success(owner).await?;
                    }
                    outcome.status = NodeStatus::Succeeded;
                    outcome.profile = success.profile;
                    outcome.model = success.model;
                    outcome.output = Some(success.value.clone());
                    outcome.exit_code = success.exit_code;
                    outcome.error = None;
                    outcome.finished_at = Some(Utc::now());
                    outcome.duration_ms = Some(duration_millis(started.elapsed()));
                    outcome.workspace = success.workspace;
                    context
                        .emit(
                            RunEventKind::NodeOutput,
                            Some(&input.qualified_id),
                            Some(attempt),
                            None,
                            json!({
                                "output": success.value,
                                "output_artifact": artifacts.output,
                                "stdout_artifact": artifacts.stdout,
                                "stderr_artifact": artifacts.stderr,
                                "profile": outcome.profile,
                                "model": outcome.model,
                                "selection_origin": success.selection_origin,
                                "model_origin": success.model_origin,
                                "workspace": outcome.workspace,
                            }),
                        )
                        .await?;
                    context
                        .emit(
                            RunEventKind::NodeSucceeded,
                            Some(&input.qualified_id),
                            Some(attempt),
                            None,
                            json!({"exit_code": success.exit_code}),
                        )
                        .await?;
                    return Ok(outcome);
                }
                Err(AttemptExecutionError::Fatal(error)) => return Err(error),
                Err(AttemptExecutionError::Failure(mut failure)) => {
                    if matches!(&input.node.kind, NodeKind::Verify { .. })
                        && failure.kind == AttemptFailureKind::Normal
                        && failure.class == NodeFailureClass::Execution
                    {
                        failure.class = NodeFailureClass::Verification;
                    }
                    if failure.class == NodeFailureClass::ProviderCancelled {
                        let source = context
                            .global_cancel_reason()
                            .map(CancellationSource::Global)
                            .or_else(|| {
                                scheduler_cancellation
                                    .is_cancelled()
                                    .then_some(CancellationSource::Scheduler)
                            });
                        if let Some(source) = source {
                            failure = AttemptFailure::cancelled(source);
                        }
                    }
                    if let Err(mut budget_failure) = context.reserve_attempt_artifact_bytes(
                        &input.qualified_id,
                        &failure.stdout,
                        &failure.stderr,
                        &failure.raw_output,
                    ) {
                        budget_failure.profile.clone_from(&failure.profile);
                        budget_failure.model.clone_from(&failure.model);
                        budget_failure.exit_code = failure.exit_code;
                        budget_failure.workspace.clone_from(&failure.workspace);
                        outcome.status = NodeStatus::Cancelled;
                        outcome.profile = budget_failure.profile.clone();
                        outcome.model = budget_failure.model.clone();
                        outcome.exit_code = budget_failure.exit_code;
                        outcome.workspace = budget_failure.workspace.clone();
                        outcome.error = Some(encoded_failure(&budget_failure));
                        outcome.finished_at = Some(Utc::now());
                        outcome.duration_ms = Some(duration_millis(started.elapsed()));
                        context
                            .emit(
                                RunEventKind::NodeBlocked,
                                Some(&input.qualified_id),
                                Some(attempt),
                                Some(encoded_failure(&budget_failure)),
                                json!({
                                    "status": "cancelled",
                                    "error_class": budget_failure.class,
                                    "exit_code": budget_failure.exit_code,
                                    "artifacts_retained": false,
                                }),
                            )
                            .await?;
                        return Ok(outcome);
                    }
                    let artifacts = context
                        .store_attempt(
                            &input.qualified_id,
                            attempt,
                            &failure.stdout,
                            &failure.stderr,
                            &failure.raw_output,
                            failure.output_is_json,
                        )
                        .await?;
                    apply_attempt_artifacts(&mut outcome, &artifacts);
                    outcome.profile.clone_from(&failure.profile);
                    outcome.model.clone_from(&failure.model);
                    outcome.exit_code = failure.exit_code;
                    outcome.error = Some(encoded_failure(&failure));
                    outcome.workspace.clone_from(&failure.workspace);
                    outcome.finished_at = Some(Utc::now());
                    outcome.duration_ms = Some(duration_millis(started.elapsed()));

                    match failure.kind {
                        AttemptFailureKind::GlobalCancelled
                        | AttemptFailureKind::SchedulerCancelled => {
                            outcome.status = NodeStatus::Cancelled;
                            context
                                .emit(
                                    RunEventKind::NodeBlocked,
                                    Some(&input.qualified_id),
                                    Some(attempt),
                                    Some(encoded_failure(&failure)),
                                    artifact_event_data(
                                        &artifacts,
                                        &outcome,
                                        "cancelled",
                                        failure.class,
                                    ),
                                )
                                .await?;
                            return Ok(outcome);
                        }
                        AttemptFailureKind::Blocked => {
                            outcome.status = NodeStatus::Blocked;
                            context
                                .emit(
                                    RunEventKind::NodeBlocked,
                                    Some(&input.qualified_id),
                                    Some(attempt),
                                    Some(encoded_failure(&failure)),
                                    artifact_event_data(
                                        &artifacts,
                                        &outcome,
                                        "blocked",
                                        failure.class,
                                    ),
                                )
                                .await?;
                            return Ok(outcome);
                        }
                        AttemptFailureKind::Normal => {
                            outcome.status = NodeStatus::Failed;
                            context
                                .emit(
                                    RunEventKind::NodeFailed,
                                    Some(&input.qualified_id),
                                    Some(attempt),
                                    Some(encoded_failure(&failure)),
                                    artifact_event_data(
                                        &artifacts,
                                        &outcome,
                                        "failed",
                                        failure.class,
                                    ),
                                )
                                .await?;
                            let has_explicit_rebind =
                                usize::try_from(attempt - 1).ok().is_some_and(|index| {
                                    input.node.retry.rebind_profiles.get(index).is_some()
                                });
                            let uncertain_side_effects = has_explicit_rebind
                                && !failure.retryable
                                && matches!(
                                    failure.class,
                                    NodeFailureClass::ProviderProcess
                                        | NodeFailureClass::ProviderTimeout
                                        | NodeFailureClass::ProviderTransient
                                );
                            if attempt < max_attempts
                                && !failure.retry_forbidden
                                && (failure.retryable || has_explicit_rebind)
                            {
                                let next_profile = profile_for_attempt(&input.node, attempt + 1);
                                context
                                    .emit(
                                        RunEventKind::RetryScheduled,
                                        Some(&input.qualified_id),
                                        Some(attempt + 1),
                                        Some(failure.message.clone()),
                                        json!({
                                            "backoff_seconds": input.node.retry.backoff_seconds,
                                            "profile": next_profile,
                                            "uncertain_side_effects": uncertain_side_effects,
                                        }),
                                    )
                                    .await?;
                                if input.node.retry.backoff_seconds > 0 {
                                    tokio::select! {
                                        source = wait_for_cancellation(&context, &scheduler_cancellation) => {
                                            let cancellation = AttemptFailure::cancelled(source);
                                            outcome.status = NodeStatus::Cancelled;
                                            outcome.error = Some(encoded_failure(&cancellation));
                                            outcome.finished_at = Some(Utc::now());
                                            outcome.duration_ms = Some(duration_millis(started.elapsed()));
                                            context
                                                .emit(
                                                    RunEventKind::NodeBlocked,
                                                    Some(&input.qualified_id),
                                                    Some(attempt),
                                                    outcome.error.clone(),
                                                    artifact_event_data(
                                                        &artifacts,
                                                        &outcome,
                                                        "cancelled",
                                                        NodeFailureClass::Cancelled,
                                                    ),
                                                )
                                                .await?;
                                            return Ok(outcome);
                                        }
                                        () = time::sleep(Duration::from_secs(input.node.retry.backoff_seconds)) => {}
                                    }
                                }
                                continue;
                            }
                            return Ok(outcome);
                        }
                    }
                }
            }
        }

        Err(RunError::SchedulerInvariant(format!(
            "node {:?} exhausted an unreachable retry loop",
            input.qualified_id
        )))
    }

    #[allow(clippy::too_many_lines)]
    async fn execute_attempt(
        &self,
        input: &NodeInput,
        context: &Arc<RunContext>,
        attempt: u32,
        profile: Option<&str>,
        workspace: &ResolvedWorkspace,
        cancellation: CancellationToken,
    ) -> Result<AttemptSuccess, AttemptExecutionError> {
        let _parallel_permit = if matches!(
            &input.node.kind,
            NodeKind::Agent { .. }
                | NodeKind::Reduce { .. }
                | NodeKind::Synthesize { .. }
                | NodeKind::Loop { .. }
                | NodeKind::Subgraph { .. }
        ) {
            None
        } else {
            Some(
                context
                    .parallelism
                    .acquire()
                    .await
                    .map_err(|_| AttemptFailure::normal("run parallelism limiter closed"))?,
            )
        };
        let workspace_path = &workspace.path;
        let workspace_string = Some(workspace_path.to_string_lossy().into_owned());
        match &input.node.kind {
            NodeKind::Command { argv, env, output } | NodeKind::Verify { argv, env, output } => {
                let raw =
                    execute_command(argv, env, workspace_path, output.max_bytes, cancellation)
                        .await?;
                let mut failure = AttemptFailure::deterministic(String::new());
                failure.stdout.clone_from(&raw.stdout);
                failure.stderr.clone_from(&raw.stderr);
                failure.raw_output.clone_from(&raw.stdout);
                failure.exit_code = raw.exit_code;
                failure.workspace.clone_from(&workspace_string);
                if raw.exit_code != Some(0) {
                    failure.message = format!(
                        "command exited with status {}",
                        raw.exit_code
                            .map_or_else(|| "unknown".to_owned(), |code| code.to_string())
                    );
                    return Err(failure.into());
                }
                let validated = validate_bytes(&raw.stdout, output, workspace_path)
                    .await
                    .map_err(|message| {
                        failure.message = message;
                        failure
                    })?;
                Ok(AttemptSuccess {
                    value: validated.value,
                    raw_output: validated.raw,
                    stdout: raw.stdout,
                    stderr: raw.stderr,
                    output_is_json: validated.is_json,
                    exit_code: raw.exit_code,
                    profile: None,
                    model: None,
                    selection_origin: None,
                    model_origin: None,
                    workspace: workspace_string,
                })
            }
            NodeKind::Agent {
                prompt,
                model,
                fan_out,
                output,
                ..
            } => self
                .execute_provider_node(
                    input,
                    context,
                    prompt,
                    output,
                    profile,
                    model.as_deref(),
                    *fan_out,
                    workspace_path,
                    workspace_string,
                    cancellation,
                )
                .await
                .map_err(Into::into),
            NodeKind::Reduce {
                prompt,
                model,
                output,
                ..
            }
            | NodeKind::Synthesize {
                prompt,
                model,
                output,
                ..
            } => self
                .execute_provider_node(
                    input,
                    context,
                    prompt,
                    output,
                    profile,
                    model.as_deref(),
                    1,
                    workspace_path,
                    workspace_string,
                    cancellation,
                )
                .await
                .map_err(Into::into),
            NodeKind::Gate { message, default } => {
                let decision = self
                    .gate
                    .decide(GateRequest {
                        run_id: context.run_id.clone(),
                        node_id: input.qualified_id.clone(),
                        message: message.clone(),
                        default: gate_default(*default),
                    })
                    .await
                    .map_err(AttemptFailure::normal)?;
                if decision == GateDecision::Reject {
                    return Err(AttemptFailure::blocked("human gate rejected").into());
                }
                let value = json!({"decision": "approved"});
                Ok(AttemptSuccess::json(value, workspace_string))
            }
            NodeKind::Subgraph { graph } => {
                let compiled = graph
                    .compile()
                    .map_err(|error| AttemptFailure::deterministic(error.to_string()))?;
                let execution = self
                    .clone()
                    .execute_graph(
                        compiled,
                        Arc::clone(context),
                        format!("{}.attempt-{attempt}", input.qualified_id),
                        input.parallel_limit,
                        cancellation,
                        workspace.clone(),
                    )
                    .await
                    .map_err(AttemptExecutionError::Fatal)?;
                if graph_failed(&execution.outcomes) {
                    return Err(subgraph_failure(&execution.outcomes, workspace_string).into());
                }
                let value = serde_json::to_value(&execution.outcomes)
                    .map_err(|error| AttemptFailure::normal(error.to_string()))?;
                Ok(AttemptSuccess::json(value, workspace_string))
            }
            NodeKind::Loop {
                graph,
                until,
                max_iterations,
                stagnation_after,
            } => {
                self.execute_loop(
                    input,
                    context,
                    graph,
                    until,
                    *max_iterations,
                    *stagnation_after,
                    attempt,
                    workspace_string,
                    workspace.clone(),
                    cancellation,
                )
                .await
            }
        }
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    async fn execute_provider_node(
        &self,
        input: &NodeInput,
        context: &Arc<RunContext>,
        prompt_spec: &PromptSpec,
        output: &OutputSpec,
        preferred_profile: Option<&str>,
        requested_model: Option<&str>,
        fan_out: usize,
        workspace: &Path,
        workspace_string: Option<String>,
        cancellation: CancellationToken,
    ) -> Result<AttemptSuccess, AttemptFailure> {
        let candidate_output_limit =
            fanout_candidate_output_limit(output.max_bytes, fan_out, output.format)?;
        let prompt = render_prompt(
            prompt_spec,
            &input.node.context,
            &input.dependencies,
            workspace,
            &input.qualified_id,
        )
        .await
        .map_err(AttemptFailure::deterministic)?;
        enforce_fanout_prompt_limit(&prompt, fan_out, input.node.context.max_bytes)?;
        let required = required_capabilities(&input.node, output)?;
        context.reserve_model_calls(fan_out)?;
        let fanout_cancellation = cancellation.child_token();
        let fanout_parallelism = Arc::new(Semaphore::new(
            input.parallel_limit.clamp(1, Semaphore::MAX_PERMITS),
        ));
        let mut calls = FuturesUnordered::new();
        for index in 0..fan_out {
            let mut request = AdapterRequest::new(if fan_out == 1 {
                prompt.clone()
            } else {
                format!("{prompt}\n\nFan-out candidate: {}/{}", index + 1, fan_out)
            });
            request.working_directory = Some(workspace.to_path_buf());
            request.model = requested_model.map(ToOwned::to_owned);
            request.output_format = provider_output_format(output.format);
            request.timeout = input.node.timeout_seconds.map(Duration::from_secs);
            request.max_prompt_bytes = input.node.context.max_bytes;
            request.max_output_bytes = candidate_output_limit;
            let providers = Arc::clone(&self.providers);
            let required = required.clone();
            let preferred = preferred_profile.map(ToOwned::to_owned);
            let token = fanout_cancellation.child_token();
            let context = Arc::clone(context);
            let fanout_parallelism = Arc::clone(&fanout_parallelism);
            calls.push(async move {
                let local_permit = tokio::select! {
                    biased;
                    () = token.cancelled() => Err(AttemptFailure::new(
                        AttemptFailureKind::Normal,
                        NodeFailureClass::ProviderCancelled,
                        false,
                        true,
                        "fan-out provider call cancelled",
                    )),
                    permit = fanout_parallelism.acquire() => permit.map_err(|_| {
                        AttemptFailure::normal("node parallelism limiter closed")
                    }),
                };
                let result = match local_permit {
                    Err(error) => Err(error),
                    Ok(_local_permit) => tokio::select! {
                        biased;
                        () = token.cancelled() => Err(AttemptFailure::new(
                            AttemptFailureKind::Normal,
                            NodeFailureClass::ProviderCancelled,
                            false,
                            true,
                            "fan-out provider call cancelled",
                        )),
                        permit = context.parallelism.acquire() => match permit {
                        Ok(_permit) => providers
                            .execute(preferred.as_deref(), &required, request, token)
                            .await
                            .map_err(|error| AttemptFailure::provider(&error)),
                        Err(_) => Err(AttemptFailure::normal("run parallelism limiter closed")),
                        },
                    },
                };
                (index, result)
            });
        }

        let mut candidates = (0..fan_out)
            .map(|_| None)
            .collect::<Vec<Option<ProviderCandidate>>>();
        let mut aggregate_output_bytes = usize::from(fan_out > 1) * 2;
        let mut aggregate_stdout_bytes = 0;
        let mut aggregate_stderr_bytes = 0;
        let mut completed = 0;
        while let Some((index, result)) = calls.next().await {
            let invocation = match result {
                Ok(invocation) => invocation,
                Err(mut error) => {
                    fanout_cancellation.cancel();
                    let _ = time::timeout(CANCELLATION_GRACE, async {
                        while calls.next().await.is_some() {}
                    })
                    .await;
                    if fan_out > 1 {
                        error.retry_forbidden = true;
                    }
                    return Err(error);
                }
            };
            let effective_model = invocation
                .selected_model
                .clone()
                .or_else(|| requested_model.map(ToOwned::to_owned));
            let normalized = match normalize_provider_response(
                invocation.response,
                invocation.profile.clone(),
                effective_model,
                output,
                workspace,
            )
            .await
            {
                Ok(normalized) => normalized,
                Err(mut error) => {
                    fanout_cancellation.cancel();
                    let _ = time::timeout(CANCELLATION_GRACE, async {
                        while calls.next().await.is_some() {}
                    })
                    .await;
                    if fan_out > 1 {
                        error.retry_forbidden = true;
                    }
                    return Err(error);
                }
            };
            let header_bytes = if fan_out > 1 {
                format!("--- candidate {} ---\n", index + 1).len()
            } else {
                0
            };
            let aggregate = (|| -> Result<(Vec<u8>, usize, usize, usize), AttemptFailure> {
                let encoded_value = if fan_out == 1 {
                    Vec::new()
                } else {
                    serde_json::to_vec(&normalized.value)
                        .map_err(|error| AttemptFailure::provider_protocol(error.to_string()))?
                };
                let separator_bytes = usize::from(completed > 0 && fan_out > 1);
                let output_bytes = aggregate_provider_limit(
                    aggregate_output_bytes,
                    encoded_value.len().saturating_add(separator_bytes),
                    output.max_bytes,
                    "output",
                )?;
                let stdout_bytes = aggregate_provider_limit(
                    aggregate_stdout_bytes,
                    header_bytes.saturating_add(normalized.stdout.len()),
                    output.max_bytes,
                    "stdout",
                )?;
                let stderr_bytes = aggregate_provider_limit(
                    aggregate_stderr_bytes,
                    header_bytes.saturating_add(normalized.stderr.len()),
                    output.max_bytes,
                    "stderr",
                )?;
                Ok((encoded_value, output_bytes, stdout_bytes, stderr_bytes))
            })();
            let (encoded_value, output_bytes, stdout_bytes, stderr_bytes) = match aggregate {
                Ok(aggregate) => aggregate,
                Err(mut error) => {
                    fanout_cancellation.cancel();
                    let _ = time::timeout(CANCELLATION_GRACE, async {
                        while calls.next().await.is_some() {}
                    })
                    .await;
                    if fan_out > 1 {
                        error.retry_forbidden = true;
                    }
                    return Err(error);
                }
            };
            aggregate_output_bytes = output_bytes;
            aggregate_stdout_bytes = stdout_bytes;
            aggregate_stderr_bytes = stderr_bytes;
            context
                .record_usage(
                    invocation.profile.clone(),
                    normalized.model.clone(),
                    normalized.model_verified,
                )
                .await;
            candidates[index] = Some(ProviderCandidate {
                profile: invocation.profile,
                selection_origin: invocation.selection_origin,
                model_origin: invocation.model_origin,
                normalized,
                encoded_value,
            });
            completed += 1;
        }

        let mut values = Vec::with_capacity(fan_out);
        let mut stdout = Vec::with_capacity(aggregate_stdout_bytes);
        let mut stderr = Vec::with_capacity(aggregate_stderr_bytes);
        let mut raw_output = Vec::with_capacity(aggregate_output_bytes);
        if fan_out > 1 {
            raw_output.push(b'[');
        }
        let mut selected_profile = None;
        let mut selected_model = None;
        let mut selected_origin = None;
        let mut selected_model_origin = None;
        for (index, candidate) in candidates.into_iter().enumerate() {
            let candidate = candidate.expect("every fan-out candidate completed");
            if fan_out > 1 {
                let header = format!("--- candidate {} ---\n", index + 1);
                stdout.extend_from_slice(header.as_bytes());
                stderr.extend_from_slice(header.as_bytes());
                if index > 0 {
                    raw_output.push(b',');
                }
                raw_output.extend_from_slice(&candidate.encoded_value);
            } else {
                raw_output.extend_from_slice(&candidate.normalized.raw_output);
            }
            stdout.extend_from_slice(&candidate.normalized.stdout);
            stderr.extend_from_slice(&candidate.normalized.stderr);
            selected_profile.get_or_insert(candidate.profile);
            selected_origin.get_or_insert(selection_origin_code(candidate.selection_origin));
            selected_model_origin.get_or_insert(model_origin_code(candidate.model_origin));
            if selected_model.is_none() {
                selected_model.clone_from(&candidate.normalized.model);
            }
            values.push(candidate.normalized.value);
        }
        if fan_out > 1 {
            raw_output.push(b']');
        }
        let value = if fan_out == 1 {
            values.into_iter().next().unwrap_or(Value::Null)
        } else {
            Value::Array(values)
        };
        Ok(AttemptSuccess {
            value,
            raw_output,
            stdout,
            stderr,
            output_is_json: output.format == GraphOutputFormat::Json || fan_out > 1,
            exit_code: Some(0),
            profile: selected_profile,
            model: selected_model,
            selection_origin: selected_origin,
            model_origin: selected_model_origin,
            workspace: workspace_string,
        })
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    async fn execute_loop(
        &self,
        input: &NodeInput,
        context: &Arc<RunContext>,
        graph: &Graph,
        condition: &LoopCondition,
        max_iterations: u32,
        stagnation_after: u32,
        attempt: u32,
        workspace: Option<String>,
        default_workspace: ResolvedWorkspace,
        cancellation: CancellationToken,
    ) -> Result<AttemptSuccess, AttemptExecutionError> {
        context
            .emit(
                RunEventKind::LoopStarted,
                Some(&input.qualified_id),
                Some(attempt),
                None,
                json!({"max_iterations": max_iterations}),
            )
            .await
            .map_err(|error| AttemptFailure::normal(error.to_string()))?;
        let mut previous_fingerprint = None;
        let mut stagnant = 0_u32;
        let mut last_outcomes = IndexMap::new();
        for iteration in 1..=max_iterations {
            if let Some(source) = context
                .global_cancel_reason()
                .map(CancellationSource::Global)
                .or_else(|| {
                    cancellation
                        .is_cancelled()
                        .then_some(CancellationSource::Scheduler)
                })
            {
                return Err(AttemptFailure::cancelled(source).into());
            }

            context
                .emit(
                    RunEventKind::LoopIterationStarted,
                    Some(&input.qualified_id),
                    Some(attempt),
                    None,
                    json!({"iteration": iteration}),
                )
                .await
                .map_err(AttemptExecutionError::Fatal)?;
            let compiled = graph
                .compile()
                .map_err(|error| AttemptFailure::normal(error.to_string()))?;
            let namespace = format!(
                "{}.attempt-{attempt}.iteration-{iteration}",
                input.qualified_id
            );
            let iteration_token = cancellation.child_token();
            let execution = self
                .clone()
                .execute_graph(
                    compiled,
                    Arc::clone(context),
                    namespace,
                    input.parallel_limit,
                    iteration_token,
                    default_workspace.clone(),
                )
                .await
                .map_err(AttemptExecutionError::Fatal)?;
            last_outcomes = execution.outcomes;
            let target = last_outcomes.get(&condition.node).ok_or_else(|| {
                AttemptFailure::normal(format!(
                    "loop condition references missing node {:?}",
                    condition.node
                ))
            })?;
            let inner_failed = graph_failed(&last_outcomes);
            let satisfied = !inner_failed && loop_condition_matches(condition, target);
            context
                .emit(
                    RunEventKind::LoopIterationFinished,
                    Some(&input.qualified_id),
                    Some(attempt),
                    None,
                    json!({
                        "iteration": iteration,
                        "satisfied": satisfied,
                        "inner_failed": inner_failed,
                    }),
                )
                .await
                .map_err(|error| AttemptFailure::normal(error.to_string()))?;
            if satisfied {
                context
                    .emit(
                        RunEventKind::LoopFinished,
                        Some(&input.qualified_id),
                        Some(attempt),
                        None,
                        json!({"iterations": iteration, "reason": "condition_satisfied"}),
                    )
                    .await
                    .map_err(|error| AttemptFailure::normal(error.to_string()))?;
                return Ok(AttemptSuccess::json(
                    json!({"iterations": iteration, "nodes": last_outcomes}),
                    workspace,
                ));
            }
            let fingerprint = outcome_fingerprint(target);
            if previous_fingerprint.as_ref() == Some(&fingerprint) {
                stagnant = stagnant.saturating_add(1);
            } else {
                stagnant = 0;
                previous_fingerprint = Some(fingerprint);
            }
            if stagnant >= stagnation_after {
                context
                    .emit(
                        RunEventKind::LoopFinished,
                        Some(&input.qualified_id),
                        Some(attempt),
                        Some("loop output stagnated".into()),
                        json!({"iterations": iteration, "reason": "stagnation"}),
                    )
                    .await
                    .map_err(|error| AttemptFailure::normal(error.to_string()))?;
                let mut failure = if inner_failed {
                    let mut failure = subgraph_failure(&last_outcomes, workspace.clone());
                    failure.message = format!(
                        "loop stagnated for {stagnation_after} repeated iterations: {}",
                        failure.message
                    );
                    failure
                } else {
                    AttemptFailure::normal(format!(
                        "loop stagnated for {stagnation_after} repeated iterations"
                    ))
                };
                failure.raw_output = serde_json::to_vec(&last_outcomes).unwrap_or_default();
                failure.output_is_json = true;
                failure.workspace = workspace;
                return Err(failure.into());
            }
        }
        context
            .emit(
                RunEventKind::LoopFinished,
                Some(&input.qualified_id),
                Some(attempt),
                Some("loop reached its iteration bound".into()),
                json!({"iterations": max_iterations, "reason": "max_iterations"}),
            )
            .await
            .map_err(|error| AttemptFailure::normal(error.to_string()))?;
        let inner_failed = graph_failed(&last_outcomes);
        let mut failure = if inner_failed {
            let mut failure = subgraph_failure(&last_outcomes, workspace.clone());
            failure.message = format!(
                "loop condition was not satisfied within {max_iterations} iterations: {}",
                failure.message
            );
            failure
        } else {
            AttemptFailure::normal(format!(
                "loop condition was not satisfied within {max_iterations} iterations"
            ))
        };
        failure.raw_output = serde_json::to_vec(&last_outcomes).unwrap_or_default();
        failure.output_is_json = true;
        failure.workspace = workspace;
        Err(failure.into())
    }
}
