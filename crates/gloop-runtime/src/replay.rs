use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use gloop_core::{FinalStatus, Graph, NodeOutcome, NodeStatus, RunEvent, RunEventKind, RunSummary};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::fs;

use crate::SUMMARY_SCHEMA_VERSION;
use crate::journal::{EVENT_SCHEMA_VERSION, JournalError, read_journal};

const MAX_SUMMARY_BYTES: u64 = 128 * 1024 * 1024;
const MAX_GRAPH_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_RUN_EVENT_COUNT: usize = 1_000_000;
const MAX_JOURNAL_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReplayReport {
    pub run_id: String,
    pub graph_hash: Option<String>,
    pub event_count: usize,
    pub last_sequence: u64,
    pub finished: bool,
    pub truncated_tail: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_status: Option<FinalStatus>,
    pub nodes: IndexMap<String, NodeOutcome>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunInspection {
    pub summary: RunSummary,
    pub replay: ReplayReport,
}

pub async fn replay_journal(path: impl AsRef<Path>) -> Result<ReplayReport, ReplayError> {
    Ok(replay_journal_with_artifacts(path).await?.0)
}

async fn replay_journal_with_artifacts(
    path: impl AsRef<Path>,
) -> Result<(ReplayReport, Vec<String>), ReplayError> {
    ensure_file_not_symlink(path.as_ref())?;
    enforce_file_size_limit(path.as_ref(), MAX_JOURNAL_BYTES).await?;
    let journal = read_journal(path).await?;
    if journal.truncated_tail {
        return Err(JournalError::IncompleteTail.into());
    }
    let mut report = replay_events(&journal.events)?;
    report.truncated_tail = false;
    Ok((report, collect_journal_artifacts(&journal.events)))
}

#[allow(clippy::too_many_lines)]
pub fn replay_events(events: &[RunEvent]) -> Result<ReplayReport, ReplayError> {
    if events.len() > MAX_RUN_EVENT_COUNT {
        return Err(ReplayError::TooManyEvents {
            count: events.len(),
            limit: MAX_RUN_EVENT_COUNT,
        });
    }

    let first = events.first().ok_or(ReplayError::EmptyJournal)?;
    if first.kind != RunEventKind::RunStarted {
        return Err(ReplayError::RunDidNotStart);
    }
    let run_id = first.run_id.clone();
    let mut nodes = IndexMap::<String, NodeOutcome>::new();
    let mut graph_hash = None;
    let mut final_status = None;
    let mut finished = false;
    let mut root_nodes = BTreeSet::<String>::new();

    for (offset, event) in events.iter().enumerate() {
        let expected = u64::try_from(offset)
            .ok()
            .and_then(|value| value.checked_add(1))
            .ok_or(ReplayError::SequenceOverflow)?;
        if event.sequence != expected {
            return Err(ReplayError::Sequence {
                expected,
                actual: event.sequence,
            });
        }
        if event.schema_version != EVENT_SCHEMA_VERSION {
            return Err(ReplayError::SchemaVersion {
                sequence: event.sequence,
                value: event.schema_version.clone(),
            });
        }
        if event.run_id != run_id {
            return Err(ReplayError::MixedRun {
                sequence: event.sequence,
                expected: run_id.clone(),
                actual: event.run_id.clone(),
            });
        }
        if finished {
            return Err(ReplayError::EventAfterFinish(event.sequence));
        }

        match event.kind {
            RunEventKind::RunStarted => {
                if event.sequence != 1 {
                    return Err(ReplayError::DuplicateStart(event.sequence));
                }
                if let Some(value) = event
                    .data
                    .get("graph_hash")
                    .and_then(serde_json::Value::as_str)
                {
                    graph_hash = Some(value.to_owned());
                }
                if let Some(node_ids) = event.data.get("nodes").and_then(|value| value.as_array()) {
                    for node_id in node_ids {
                        let node_id = node_id.as_str().ok_or(ReplayError::InvalidStartNodes)?;
                        nodes.entry(node_id.to_owned()).or_default();
                        root_nodes.insert(node_id.to_owned());
                    }
                }
            }
            RunEventKind::NodeReady => {
                require_declared_root(event, &nodes, &root_nodes)?;
                let outcome = outcome_for(event, &mut nodes)?;
                require_status(event, outcome.status, &[NodeStatus::Pending], "ready")?;
                outcome.status = NodeStatus::Ready;
            }
            RunEventKind::NodeStarted => {
                require_declared_root(event, &nodes, &root_nodes)?;
                let attempt = event
                    .attempt
                    .ok_or(ReplayError::MissingAttempt(event.sequence))?;
                let outcome = outcome_for(event, &mut nodes)?;
                require_status(
                    event,
                    outcome.status,
                    &[NodeStatus::Ready, NodeStatus::Failed],
                    "start",
                )?;
                outcome.status = NodeStatus::Running;
                outcome.attempts = outcome.attempts.max(attempt);
                outcome.started_at.get_or_insert(event.timestamp);
            }
            RunEventKind::NodeOutput => {
                require_declared_root(event, &nodes, &root_nodes)?;
                let outcome = outcome_for(event, &mut nodes)?;
                require_status(event, outcome.status, &[NodeStatus::Running], "emit output")?;
                outcome.output = Some(
                    event
                        .data
                        .get("output")
                        .cloned()
                        .unwrap_or_else(|| event.data.clone()),
                );
                if let Some(path) = event.data.get("output_artifact").and_then(|v| v.as_str()) {
                    outcome.output_artifact = Some(path.to_owned());
                }
                if let Some(path) = event.data.get("stdout_artifact").and_then(|v| v.as_str()) {
                    outcome.stdout_artifact = Some(path.to_owned());
                }
                if let Some(path) = event.data.get("stderr_artifact").and_then(|v| v.as_str()) {
                    outcome.stderr_artifact = Some(path.to_owned());
                }
                outcome.profile = event
                    .data
                    .get("profile")
                    .and_then(|value| value.as_str())
                    .map(ToOwned::to_owned);
                outcome.model = event
                    .data
                    .get("model")
                    .and_then(|value| value.as_str())
                    .map(ToOwned::to_owned);
                outcome.workspace = event
                    .data
                    .get("workspace")
                    .and_then(|value| value.as_str())
                    .map(ToOwned::to_owned);
            }
            RunEventKind::NodeSucceeded => {
                require_declared_root(event, &nodes, &root_nodes)?;
                let outcome = terminal_node(event, &mut nodes, NodeStatus::Succeeded)?;
                outcome.exit_code = event
                    .data
                    .get("exit_code")
                    .and_then(serde_json::Value::as_i64)
                    .and_then(|value| i32::try_from(value).ok());
            }
            RunEventKind::NodeFailed => {
                require_declared_root(event, &nodes, &root_nodes)?;
                let outcome = terminal_node(event, &mut nodes, NodeStatus::Failed)?;
                outcome.error.clone_from(&event.message);
                apply_artifact_data(outcome, event);
                outcome.exit_code = event
                    .data
                    .get("exit_code")
                    .and_then(serde_json::Value::as_i64)
                    .and_then(|value| i32::try_from(value).ok());
            }
            RunEventKind::NodeSkipped => {
                require_declared_root(event, &nodes, &root_nodes)?;
                terminal_node(event, &mut nodes, NodeStatus::Skipped)?;
            }
            RunEventKind::NodeBlocked => {
                require_declared_root(event, &nodes, &root_nodes)?;
                let status = if event.data.get("status").and_then(|value| value.as_str())
                    == Some("cancelled")
                {
                    NodeStatus::Cancelled
                } else {
                    NodeStatus::Blocked
                };
                let outcome = terminal_node(event, &mut nodes, status)?;
                outcome.error.clone_from(&event.message);
                apply_artifact_data(outcome, event);
            }
            RunEventKind::RetryScheduled => {
                require_declared_root(event, &nodes, &root_nodes)?;
                let outcome = outcome_for(event, &mut nodes)?;
                require_status(
                    event,
                    outcome.status,
                    &[NodeStatus::Failed],
                    "schedule retry",
                )?;
            }
            RunEventKind::RunCancelled => {
                // run cancellation is run-level control and may include nodes without a matching
                // start event. keep behavior unchanged and rely on replay terminal checks.
                for outcome in nodes.values_mut().filter(|outcome| {
                    matches!(
                        outcome.status,
                        NodeStatus::Pending | NodeStatus::Ready | NodeStatus::Running
                    )
                }) {
                    outcome.status = NodeStatus::Cancelled;
                    outcome.finished_at = Some(event.timestamp);
                    outcome.error.clone_from(&event.message);
                }
            }
            RunEventKind::RunFinished => {
                let active_nodes = nodes
                    .iter()
                    .filter_map(|(node, outcome)| {
                        (!outcome.status.is_terminal()).then_some(node.clone())
                    })
                    .collect::<Vec<_>>();
                if !active_nodes.is_empty() {
                    return Err(ReplayError::RunFinishedWithActiveNodes {
                        sequence: event.sequence,
                        nodes: active_nodes,
                    });
                }
                let raw_status =
                    event
                        .data
                        .get("status")
                        .ok_or(ReplayError::MissingFinalStatus {
                            sequence: event.sequence,
                        })?;
                final_status = Some(serde_json::from_value(raw_status.clone()).map_err(
                    |source| ReplayError::InvalidFinalStatus {
                        sequence: event.sequence,
                        source,
                    },
                )?);
                finished = true;
            }
            RunEventKind::LoopStarted
            | RunEventKind::LoopIterationStarted
            | RunEventKind::LoopIterationFinished
            | RunEventKind::LoopFinished => {}
        }
    }

    if graph_hash.is_none() {
        return Err(ReplayError::MissingRunStartedGraphHash);
    }

    if !finished {
        return Err(ReplayError::RunDidNotFinish);
    }

    Ok(ReplayReport {
        run_id,
        graph_hash,
        event_count: events.len(),
        last_sequence: events.last().map_or(0, |event| event.sequence),
        finished,
        truncated_tail: false,
        final_status,
        nodes,
    })
}

pub async fn inspect_run(root: impl AsRef<Path>) -> Result<RunInspection, ReplayError> {
    let root = std::fs::canonicalize(root.as_ref())?;
    ensure_no_inroot_symlink(&root, &root)?;
    let summary_root = root.clone();
    let summary_path = root.join("summary.json");
    ensure_no_inroot_symlink(&summary_path, &summary_root)?;
    enforce_file_size_limit(&summary_path, MAX_SUMMARY_BYTES).await?;
    let summary_bytes = fs::read(&summary_path).await?;
    let summary: RunSummary =
        serde_json::from_slice(&summary_bytes).map_err(|source| ReplayError::InvalidSummary {
            path: summary_path,
            source,
        })?;
    if summary.schema_version != SUMMARY_SCHEMA_VERSION {
        return Err(ReplayError::UnsupportedSummarySchemaVersion {
            summary: summary.schema_version.clone(),
            supported: SUMMARY_SCHEMA_VERSION.to_owned(),
        });
    }
    verify_summary_artifact_refs(&summary)?;
    verify_summary_node_artifacts(&summary)?;
    preflight_summary_artifacts(&summary_root, &summary.artifacts)?;

    let (replay, replay_artifacts) =
        replay_journal_with_artifacts(root.join("journal.jsonl")).await?;
    if summary.run_id != replay.run_id {
        return Err(ReplayError::SummaryRunMismatch {
            summary: summary.run_id,
            journal: replay.run_id,
        });
    }
    if Some(&summary.provenance.graph_hash) != replay.graph_hash.as_ref() {
        return Err(ReplayError::GraphHashMismatch {
            expected: summary.provenance.graph_hash.clone(),
            actual: replay.graph_hash.unwrap_or_default(),
        });
    }
    verify_summary_artifact_coverage(&summary, &replay_artifacts)?;
    verify_summary_graph_artifact(&summary_root, &summary).await?;
    verify_summary_artifacts(&summary_root, &summary.artifacts).await?;
    if replay.final_status != Some(summary.status) {
        return Err(ReplayError::SummaryStatusMismatch {
            summary: summary.status,
            journal: replay.final_status,
        });
    }
    verify_terminal_states(&summary, &replay)?;
    Ok(RunInspection { summary, replay })
}

fn outcome_for<'a>(
    event: &RunEvent,
    nodes: &'a mut IndexMap<String, NodeOutcome>,
) -> Result<&'a mut NodeOutcome, ReplayError> {
    let node_id = event
        .node_id
        .as_deref()
        .ok_or(ReplayError::MissingNode(event.sequence))?;
    Ok(nodes.entry(node_id.to_owned()).or_default())
}

fn ensure_no_inroot_symlink(path: &Path, root: &Path) -> Result<(), ReplayError> {
    let canonical_root = std::fs::canonicalize(root).map_err(ReplayError::Io)?;
    let path = path
        .strip_prefix(&canonical_root)
        .map_err(|_| ReplayError::SymlinkPath {
            path: path.to_path_buf(),
        })?;
    let mut cursor = canonical_root;
    for component in path.components() {
        if matches!(component, Component::ParentDir) {
            return Err(ReplayError::InvalidArtifactPath {
                path: path.display().to_string(),
                reason: "artifact path must not contain parent directory component".to_owned(),
            });
        }
        if component == Component::CurDir {
            continue;
        }
        cursor = cursor.join(component);
        let metadata = match std::fs::symlink_metadata(&cursor) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(ReplayError::Io(error)),
        };
        if metadata.file_type().is_symlink() {
            return Err(ReplayError::SymlinkPath { path: cursor });
        }
    }
    Ok(())
}

fn ensure_file_not_symlink(path: &Path) -> Result<(), ReplayError> {
    let metadata = std::fs::symlink_metadata(path).map_err(ReplayError::Io)?;
    if metadata.file_type().is_symlink() {
        Err(ReplayError::SymlinkPath {
            path: path.to_path_buf(),
        })
    } else {
        Ok(())
    }
}

async fn enforce_file_size_limit(path: &Path, limit: u64) -> Result<(), ReplayError> {
    let metadata = fs::metadata(path).await?;
    if metadata.len() > limit {
        return Err(ReplayError::FileTooLarge {
            path: path.to_path_buf(),
            size: metadata.len(),
            limit,
        });
    }
    Ok(())
}

fn verify_summary_artifact_refs(summary: &RunSummary) -> Result<(), ReplayError> {
    for artifact in &summary.artifacts {
        if artifact.size.is_none() {
            return Err(ReplayError::MissingArtifactSize {
                path: artifact.path.clone(),
            });
        }
        if artifact.sha256.is_none() {
            return Err(ReplayError::MissingArtifactHash {
                path: artifact.path.clone(),
            });
        }
    }
    Ok(())
}

fn apply_artifact_data(outcome: &mut NodeOutcome, event: &RunEvent) {
    outcome.output_artifact = event
        .data
        .get("output_artifact")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned);
    outcome.stdout_artifact = event
        .data
        .get("stdout_artifact")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned);
    outcome.stderr_artifact = event
        .data
        .get("stderr_artifact")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned);
    outcome.profile = event
        .data
        .get("profile")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned);
    outcome.model = event
        .data
        .get("model")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned);
    outcome.workspace = event
        .data
        .get("workspace")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned);
    outcome.exit_code = event
        .data
        .get("exit_code")
        .and_then(serde_json::Value::as_i64)
        .and_then(|value| i32::try_from(value).ok());
}

fn terminal_node<'a>(
    event: &RunEvent,
    nodes: &'a mut IndexMap<String, NodeOutcome>,
    status: NodeStatus,
) -> Result<&'a mut NodeOutcome, ReplayError> {
    let outcome = outcome_for(event, nodes)?;
    let expected = if status == NodeStatus::Cancelled {
        &[
            NodeStatus::Pending,
            NodeStatus::Ready,
            NodeStatus::Running,
            NodeStatus::Failed,
        ][..]
    } else {
        &[NodeStatus::Pending, NodeStatus::Ready, NodeStatus::Running][..]
    };
    require_status(event, outcome.status, expected, "finish")?;
    if status == NodeStatus::Succeeded {
        outcome.error = None;
    }
    outcome.status = status;
    outcome.finished_at = Some(event.timestamp);
    outcome.duration_ms = outcome.started_at.map(|started| {
        u64::try_from((event.timestamp - started).num_milliseconds().max(0)).unwrap_or(u64::MAX)
    });
    Ok(outcome)
}

fn require_declared_root(
    event: &RunEvent,
    nodes: &IndexMap<String, NodeOutcome>,
    allowed_roots: &BTreeSet<String>,
) -> Result<(), ReplayError> {
    let node_id = event
        .node_id
        .as_deref()
        .ok_or(ReplayError::MissingNode(event.sequence))?;
    let root_node = node_id.split('.').next().unwrap_or_default();
    if !allowed_roots.contains(root_node) {
        return Err(ReplayError::UndeclaredRootNode {
            sequence: event.sequence,
            node: node_id.to_owned(),
            declared: allowed_roots.iter().cloned().collect(),
        });
    }
    if !nodes.contains_key(node_id) {
        return Ok(()); // Node entry will be created in outcome_for for known root namespace.
    }
    Ok(())
}

fn require_status(
    event: &RunEvent,
    actual: NodeStatus,
    expected: &[NodeStatus],
    action: &'static str,
) -> Result<(), ReplayError> {
    if expected.contains(&actual) {
        Ok(())
    } else {
        Err(ReplayError::InvalidTransition {
            sequence: event.sequence,
            node: event.node_id.clone().unwrap_or_default(),
            actual,
            action,
        })
    }
}

fn verify_terminal_states(summary: &RunSummary, report: &ReplayReport) -> Result<(), ReplayError> {
    let summary_nodes = root_node_keys(&summary.nodes);
    let replay_nodes = root_node_keys(&report.nodes);
    if summary_nodes != replay_nodes {
        return Err(ReplayError::ReplayNodeSetMismatch {
            summary: summary_nodes,
            replay: replay_nodes,
        });
    }

    for (node_id, summary_node) in &summary.nodes {
        let replay_node =
            report
                .nodes
                .get(node_id)
                .ok_or_else(|| ReplayError::SummaryNodeMissing {
                    node: node_id.to_owned(),
                })?;
        if summary_node.status != replay_node.status {
            return Err(ReplayError::SummaryReplayNodeMismatch {
                node: node_id.to_owned(),
                summary: summary_node.status,
                replay: replay_node.status,
            });
        }
        if summary_node.error != replay_node.error {
            return Err(ReplayError::SummaryReplayNodeFieldMismatch {
                node: node_id.to_owned(),
                field: "error",
                summary: format!("{:?}", summary_node.error),
                replay: format!("{:?}", replay_node.error),
            });
        }
        if summary_node.attempts != replay_node.attempts {
            return Err(ReplayError::SummaryReplayNodeFieldMismatch {
                node: node_id.to_owned(),
                field: "attempts",
                summary: summary_node.attempts.to_string(),
                replay: replay_node.attempts.to_string(),
            });
        }
        if summary_node.profile != replay_node.profile {
            return Err(ReplayError::SummaryReplayNodeFieldMismatch {
                node: node_id.to_owned(),
                field: "profile",
                summary: summary_node.profile.clone().unwrap_or_default(),
                replay: replay_node.profile.clone().unwrap_or_default(),
            });
        }
        if summary_node.model != replay_node.model {
            return Err(ReplayError::SummaryReplayNodeFieldMismatch {
                node: node_id.to_owned(),
                field: "model",
                summary: summary_node.model.clone().unwrap_or_default(),
                replay: replay_node.model.clone().unwrap_or_default(),
            });
        }
        if summary_node.output != replay_node.output {
            return Err(ReplayError::SummaryReplayNodeFieldMismatch {
                node: node_id.to_owned(),
                field: "output",
                summary: summary_node
                    .output
                    .as_ref()
                    .map_or_else(String::new, |value| {
                        serde_json::to_string(value)
                            .unwrap_or_else(|error| format!("unserializable:{error}"))
                    }),
                replay: replay_node
                    .output
                    .as_ref()
                    .map_or_else(String::new, |value| {
                        serde_json::to_string(value)
                            .unwrap_or_else(|error| format!("unserializable:{error}"))
                    }),
            });
        }
        if summary_node.workspace != replay_node.workspace {
            return Err(ReplayError::SummaryReplayNodeFieldMismatch {
                node: node_id.to_owned(),
                field: "workspace",
                summary: summary_node.workspace.clone().unwrap_or_default(),
                replay: replay_node.workspace.clone().unwrap_or_default(),
            });
        }
    }

    Ok(())
}

async fn verify_summary_artifacts(
    root: &Path,
    artifacts: &[gloop_core::state::ArtifactRef],
) -> Result<(), ReplayError> {
    for artifact in artifacts {
        verify_artifact(root, artifact).await?;
    }
    Ok(())
}

fn collect_journal_artifacts(events: &[RunEvent]) -> Vec<String> {
    // The journal is itself recorded in the run summary's artifact manifest.
    // Include every attempt's references: replay collapses repeated node IDs
    // to their latest outcome, while the summary retains artifacts for all
    // attempts.
    let mut refs = BTreeSet::from(["journal.jsonl".to_owned()]);
    for event in events {
        for key in ["output_artifact", "stdout_artifact", "stderr_artifact"] {
            if let Some(path) = event.data.get(key).and_then(|value| value.as_str()) {
                refs.insert(path.to_owned());
            }
        }
        if let Some(path) = event
            .data
            .get("worktree_manifest_artifact")
            .and_then(|value| value.as_str())
        {
            refs.insert(path.to_owned());
        }
    }
    refs.into_iter().collect()
}

fn verify_summary_node_artifacts(summary: &RunSummary) -> Result<(), ReplayError> {
    for artifact in summary
        .artifacts
        .iter()
        .filter(|artifact| artifact.kind != "graph")
    {
        if artifact.path.trim().is_empty() {
            return Err(ReplayError::InvalidArtifactPath {
                path: artifact.path.clone(),
                reason: "artifact path must be non-empty".to_owned(),
            });
        }
    }
    Ok(())
}

fn preflight_summary_artifacts(
    root: &Path,
    artifacts: &[gloop_core::state::ArtifactRef],
) -> Result<(), ReplayError> {
    for artifact in artifacts {
        match resolve_artifact_path(root, &artifact.path) {
            Ok(_) => {}
            Err(ReplayError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                preflight_missing_artifact_path(root, &artifact.path)?;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn verify_summary_artifact_coverage(
    summary: &RunSummary,
    replay_artifacts: &[String],
) -> Result<(), ReplayError> {
    let graph_paths: Vec<_> = summary
        .artifacts
        .iter()
        .filter(|artifact| artifact.kind == "graph")
        .map(|artifact| artifact.path.as_str())
        .collect();
    if graph_paths.is_empty() {
        return Err(ReplayError::MissingGraphArtifact);
    }
    if graph_paths.len() > 1 {
        return Err(ReplayError::MultipleGraphArtifacts {
            count: graph_paths.len(),
        });
    }

    let mut summary_artifacts: Vec<String> = summary
        .artifacts
        .iter()
        .filter(|artifact| artifact.kind != "graph")
        .map(|artifact| artifact.path.clone())
        .collect();
    summary_artifacts.sort_unstable();
    let replay_artifacts = replay_artifacts.to_vec();
    if summary_artifacts != replay_artifacts {
        return Err(ReplayError::SummaryReplayArtifactMismatch {
            summary: summary_artifacts,
            replay: replay_artifacts,
        });
    }
    Ok(())
}

async fn verify_summary_graph_artifact(
    root: &Path,
    summary: &RunSummary,
) -> Result<(), ReplayError> {
    let graph_path = root.join("graph.json");
    ensure_no_inroot_symlink(&graph_path, root)?;
    enforce_file_size_limit(&graph_path, MAX_GRAPH_BYTES).await?;
    let graph_artifacts: Vec<_> = summary
        .artifacts
        .iter()
        .filter(|artifact| artifact.kind == "graph")
        .collect();
    if graph_artifacts.is_empty() {
        return Err(ReplayError::MissingGraphArtifact);
    }
    if graph_artifacts.len() > 1 {
        return Err(ReplayError::MultipleGraphArtifacts {
            count: graph_artifacts.len(),
        });
    }

    let bytes = fs::read(&graph_path).await?;
    let graph: Graph =
        serde_json::from_slice(&bytes).map_err(|source| ReplayError::InvalidGraph {
            path: graph_path.clone(),
            source,
        })?;
    let actual = graph
        .hash()
        .map_err(|source| ReplayError::InvalidGraphHash {
            path: graph_path,
            source,
        })?;
    if actual != summary.provenance.graph_hash {
        return Err(ReplayError::GraphHashMismatch {
            expected: summary.provenance.graph_hash.clone(),
            actual,
        });
    }

    let artifact = graph_artifacts[0];
    if artifact.path != "graph.json" {
        return Err(ReplayError::InvalidArtifactPath {
            path: artifact.path.clone(),
            reason: "graph artifact must be graph.json".to_owned(),
        });
    }

    verify_artifact(root, artifact).await
}

async fn verify_artifact(
    root: &Path,
    artifact: &gloop_core::state::ArtifactRef,
) -> Result<(), ReplayError> {
    let artifact_path = resolve_artifact_path(root, &artifact.path)?;
    ensure_no_inroot_symlink(&artifact_path, root)?;
    enforce_file_size_limit(&artifact_path, MAX_ARTIFACT_BYTES).await?;
    let bytes = fs::read(&artifact_path).await?;
    let metadata = fs::metadata(&artifact_path).await?;
    if let Some(expected_size) = artifact.size
        && metadata.len() != expected_size
    {
        return Err(ReplayError::ArtifactSizeMismatch {
            path: artifact.path.clone(),
            expected: expected_size,
            actual: metadata.len(),
        });
    }
    let actual_sha = hex::encode(Sha256::digest(&bytes));
    if let Some(expected_sha) = artifact.sha256.as_deref()
        && !expected_sha.eq(&actual_sha)
    {
        return Err(ReplayError::ArtifactHashMismatch {
            path: artifact.path.clone(),
            expected: expected_sha.to_owned(),
            actual: actual_sha,
        });
    }
    Ok(())
}

fn root_node_keys(nodes: &IndexMap<String, NodeOutcome>) -> Vec<String> {
    let mut ids = nodes
        .keys()
        .map(|node_id| node_id.split('.').next().unwrap_or_default().to_owned())
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids.dedup();
    ids
}

fn resolve_artifact_path(root: &Path, path: &str) -> Result<PathBuf, ReplayError> {
    let candidate = resolve_artifact_path_nocanon(root, path)?;
    let canonical_candidate = candidate.canonicalize().map_err(ReplayError::Io)?;
    if !canonical_candidate.starts_with(root) {
        return Err(ReplayError::ArtifactOutsideRun {
            path: path.to_owned(),
            root: root.to_path_buf(),
        });
    }
    Ok(canonical_candidate)
}

fn preflight_missing_artifact_path(root: &Path, path: &str) -> Result<(), ReplayError> {
    let candidate = root.join(path);
    if !candidate.starts_with(root) {
        return Err(ReplayError::ArtifactOutsideRun {
            path: path.to_owned(),
            root: root.to_path_buf(),
        });
    }

    if candidate.is_dir() {
        return Err(ReplayError::InvalidArtifactPath {
            path: path.to_owned(),
            reason: "artifact path must refer to a file".to_owned(),
        });
    }

    if let Some(parent) = candidate.parent() {
        ensure_no_inroot_symlink(parent, root)?;
    }

    let metadata = match std::fs::symlink_metadata(&candidate) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(ReplayError::Io(error)),
    };
    if metadata.file_type().is_symlink() {
        return Err(ReplayError::SymlinkPath { path: candidate });
    }

    Ok(())
}

fn resolve_artifact_path_nocanon(root: &Path, path: &str) -> Result<PathBuf, ReplayError> {
    if path.is_empty() || path == "." {
        return Err(ReplayError::InvalidArtifactPath {
            path: path.to_owned(),
            reason: "artifact path must be non-empty and not current directory".to_owned(),
        });
    }

    if Path::new(path).is_absolute() {
        return Err(ReplayError::InvalidArtifactPath {
            path: path.to_owned(),
            reason: "artifact path must be relative".to_owned(),
        });
    }

    if Path::new(path)
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(ReplayError::InvalidArtifactPath {
            path: path.to_owned(),
            reason: "artifact path must not contain parent directory component".to_owned(),
        });
    }

    let candidate = root.join(path);
    ensure_no_inroot_symlink(&candidate, root)?;
    Ok(candidate)
}

#[derive(Debug, Error)]
pub enum ReplayError {
    #[error(transparent)]
    Journal(#[from] JournalError),
    #[error("run artifact I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("journal is empty")]
    EmptyJournal,
    #[error("journal does not begin with run_started")]
    RunDidNotStart,
    #[error("journal sequence overflow")]
    SequenceOverflow,
    #[error("expected journal sequence {expected}, found {actual}")]
    Sequence { expected: u64, actual: u64 },
    #[error("event {sequence} uses unsupported schema version {value:?}")]
    SchemaVersion { sequence: u64, value: String },
    #[error("event {sequence} belongs to run {actual:?}, expected {expected:?}")]
    MixedRun {
        sequence: u64,
        expected: String,
        actual: String,
    },
    #[error("event {0} appears after run_finished")]
    EventAfterFinish(u64),
    #[error("event {0} starts the run more than once")]
    DuplicateStart(u64),
    #[error("run_finished event {sequence} leaves active nodes: {nodes:?}")]
    RunFinishedWithActiveNodes { sequence: u64, nodes: Vec<String> },
    #[error("run_started contains a non-string node id")]
    InvalidStartNodes,
    #[error("event {0} is missing a node id")]
    MissingNode(u64),
    #[error("event {0} is missing an attempt number")]
    MissingAttempt(u64),
    #[error("event {sequence} cannot {action} node {node:?} from status {actual:?}")]
    InvalidTransition {
        sequence: u64,
        node: String,
        actual: NodeStatus,
        action: &'static str,
    },
    #[error("event {sequence} contains an invalid final status: {source}")]
    InvalidFinalStatus {
        sequence: u64,
        source: serde_json::Error,
    },
    #[error("event {sequence} is missing terminal run status")]
    MissingFinalStatus { sequence: u64 },
    #[error("too many events: {count} > {limit}")]
    TooManyEvents { count: usize, limit: usize },
    #[error("invalid summary {path}: {source}")]
    InvalidSummary {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("invalid graph.json {path}: {source}")]
    InvalidGraph {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("graph hash calculation failed for {path}: {source}")]
    InvalidGraphHash {
        path: PathBuf,
        source: gloop_core::graph::GraphError,
    },
    #[error("missing graph artifact reference")]
    MissingGraphArtifact,
    #[error("multiple graph artifacts found: {count}")]
    MultipleGraphArtifacts { count: usize },
    #[error("graph hash mismatch: expected {expected}, actual {actual}")]
    GraphHashMismatch { expected: String, actual: String },
    #[error("run_started event is missing graph_hash")]
    MissingRunStartedGraphHash,
    #[error("journal has no terminal run_finished event")]
    RunDidNotFinish,
    #[error("summary artifact references must include both size and sha256: {path:?}")]
    MissingArtifactSize { path: String },
    #[error("summary artifact references must include both size and sha256: {path:?}")]
    MissingArtifactHash { path: String },
    #[error("summary run {summary:?} does not match journal run {journal:?}")]
    SummaryRunMismatch { summary: String, journal: String },
    #[error("summary status {summary:?} does not match journal status {journal:?}")]
    SummaryStatusMismatch {
        summary: FinalStatus,
        journal: Option<FinalStatus>,
    },
    #[error("summary node {node:?} is missing from replay terminal states")]
    SummaryNodeMissing { node: String },
    #[error("summary has node {node:?} not present in replay")]
    ReplayNodeMissing { node: String },
    #[error("summary↔replay root node sets differ: summary={summary:?}, replay={replay:?}")]
    ReplayNodeSetMismatch {
        summary: Vec<String>,
        replay: Vec<String>,
    },
    #[error("terminal status mismatch for node {node:?}: summary {summary:?}, replay {replay:?}")]
    SummaryReplayNodeMismatch {
        node: String,
        summary: NodeStatus,
        replay: NodeStatus,
    },
    #[error(
        "terminal field mismatch for node {node:?} ({field}): summary {summary:?}, replay {replay:?}"
    )]
    SummaryReplayNodeFieldMismatch {
        node: String,
        field: &'static str,
        summary: String,
        replay: String,
    },
    #[error("artifact path is invalid: {path:?} ({reason})")]
    InvalidArtifactPath { path: String, reason: String },
    #[error("artifact path {path:?} is outside run directory {root:?}")]
    ArtifactOutsideRun { path: String, root: PathBuf },
    #[error("artifact path {path:?} is a symbolic link")]
    SymlinkPath { path: PathBuf },
    #[error("artifact {path:?} hash mismatch: expected {expected:?}, actual {actual:?}")]
    ArtifactHashMismatch {
        path: String,
        expected: String,
        actual: String,
    },
    #[error("artifact {path:?} size mismatch: expected {expected}, actual {actual}")]
    ArtifactSizeMismatch {
        path: String,
        expected: u64,
        actual: u64,
    },
    #[error("run artifact sets differ: summary={summary:?}, replay={replay:?}")]
    SummaryReplayArtifactMismatch {
        summary: Vec<String>,
        replay: Vec<String>,
    },
    #[error("event {sequence} references undeclared root node {node:?}; declared={declared:?}")]
    UndeclaredRootNode {
        sequence: u64,
        node: String,
        declared: Vec<String>,
    },
    #[error("file {path:?} exceeds allowed size {limit}: {size}")]
    FileTooLarge {
        path: PathBuf,
        size: u64,
        limit: u64,
    },
    #[error("unsupported summary schema version {summary:?}; expected {supported:?}")]
    UnsupportedSummarySchemaVersion { summary: String, supported: String },
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use serde_json::{Value, json};

    use super::*;
    use crate::journal::Journal;

    fn event(sequence: u64, kind: RunEventKind, node_id: Option<&str>) -> RunEvent {
        RunEvent {
            schema_version: EVENT_SCHEMA_VERSION.to_owned(),
            sequence,
            timestamp: Utc::now(),
            run_id: "run".to_owned(),
            node_id: node_id.map(ToOwned::to_owned),
            attempt: (kind == RunEventKind::NodeStarted).then_some(1),
            kind,
            message: None,
            data: Value::Null,
        }
    }

    #[test]
    fn reconstructs_node_state_and_output() {
        let mut started = event(1, RunEventKind::RunStarted, None);
        started.data = json!({"graph_hash": "test-graph-hash", "nodes": ["node"]});
        let mut output = event(4, RunEventKind::NodeOutput, Some("node"));
        output.data = json!({"output": {"ok": true}});
        let mut finished = event(6, RunEventKind::RunFinished, None);
        finished.data = json!({"status": "ready_for_human"});
        let report = replay_events(&[
            started,
            event(2, RunEventKind::NodeReady, Some("node")),
            event(3, RunEventKind::NodeStarted, Some("node")),
            output,
            event(5, RunEventKind::NodeSucceeded, Some("node")),
            finished,
        ])
        .expect("replay succeeds");
        assert_eq!(report.final_status, Some(FinalStatus::ReadyForHuman));
        assert_eq!(report.nodes["node"].output, Some(json!({"ok": true})));
    }

    #[test]
    fn rejects_sequence_gaps() {
        let error = replay_events(&[
            event(1, RunEventKind::RunStarted, None),
            event(3, RunEventKind::RunFinished, None),
        ])
        .expect_err("gap rejected");
        assert!(matches!(error, ReplayError::Sequence { expected: 2, .. }));
    }

    #[test]
    fn rejects_run_finished_with_a_running_node() {
        let mut started = event(1, RunEventKind::RunStarted, None);
        started.data = json!({"graph_hash": "test-graph-hash", "nodes": ["node"]});
        let mut finished = event(4, RunEventKind::RunFinished, None);
        finished.data = json!({"status": "ready_for_human"});
        let error = replay_events(&[
            started,
            event(2, RunEventKind::NodeReady, Some("node")),
            event(3, RunEventKind::NodeStarted, Some("node")),
            finished,
        ])
        .expect_err("active node must prevent run completion");
        assert!(matches!(
            error,
            ReplayError::RunFinishedWithActiveNodes { sequence: 4, .. }
        ));
    }

    #[test]
    fn rejects_run_finished_without_status() {
        let mut started = event(1, RunEventKind::RunStarted, None);
        started.data = json!({"graph_hash": "test-graph-hash", "nodes": ["node"]});
        let error = replay_events(&[
            started,
            event(2, RunEventKind::NodeReady, Some("node")),
            event(3, RunEventKind::NodeStarted, Some("node")),
            event(4, RunEventKind::NodeSucceeded, Some("node")),
            event(5, RunEventKind::RunFinished, None),
        ])
        .expect_err("missing status must fail");
        assert!(matches!(
            error,
            ReplayError::MissingFinalStatus { sequence: 5, .. }
        ));
    }

    #[test]
    fn rejects_node_event_for_undeclared_root() {
        let mut started = event(1, RunEventKind::RunStarted, None);
        started.data = json!({"graph_hash": "test-graph-hash", "nodes": ["root"]});
        let error = replay_events(&[
            started,
            event(2, RunEventKind::NodeReady, Some("forged.inner")),
            event(3, RunEventKind::NodeStarted, Some("forged.inner")),
            event(4, RunEventKind::RunFinished, None),
        ])
        .expect_err("undeclared root should fail");
        assert!(matches!(error, ReplayError::UndeclaredRootNode { .. }));
    }

    #[test]
    fn rejects_too_many_events() {
        let mut events = Vec::with_capacity(1_000_002);
        events.push(event(1, RunEventKind::RunStarted, None));
        for sequence in 2..=1_000_001 {
            events.push(event(sequence, RunEventKind::NodeReady, Some("node")));
        }
        events.push(event(1_000_002, RunEventKind::RunFinished, None));
        let error = replay_events(&events).expect_err("too many events");
        assert!(matches!(error, ReplayError::TooManyEvents { .. }));
    }

    #[tokio::test]
    async fn rejects_truncated_journal_tail() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let path = temporary.path().join("journal.jsonl");
        let journal = Journal::create(&path, "run").await.expect("create journal");
        journal
            .append(
                RunEventKind::RunStarted,
                None,
                None,
                None,
                json!({"nodes": []}),
            )
            .await
            .expect("append start");
        journal
            .append(
                RunEventKind::RunFinished,
                None,
                None,
                None,
                json!({"status": "ready_for_human"}),
            )
            .await
            .expect("append finish");
        let mut bytes = fs::read(&path).await.expect("read journal");
        bytes.extend_from_slice(b"{\"incomplete\":");
        fs::write(&path, bytes).await.expect("write truncated tail");

        assert!(matches!(
            replay_journal(&path)
                .await
                .expect_err("truncated tail must fail replay"),
            ReplayError::Journal(JournalError::IncompleteTail)
        ));
    }
}
