#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use gloop_core::{
    FinalStatus, Graph, Node, NodeStatus, RunEventKind,
    graph::{Edge, LoopCondition, NodeKind, OutputFormat, WorkspaceSpec},
};
use gloop_provider::ProviderRegistry;
use gloop_runtime::{
    RunError, RunOptions, Runtime, WorktreeError, WorktreeManifest, inspect_run, read_events,
};
use serde_json::json;
use tempfile::TempDir;

#[derive(Debug)]
struct TestRepository {
    _temporary: TempDir,
    path: PathBuf,
    base_commit: String,
}

impl TestRepository {
    fn new() -> Self {
        let temporary = tempfile::tempdir().expect("temporary repository");
        let path = temporary.path().to_path_buf();
        git_ok(&path, &["init", "--quiet"]);
        fs::write(path.join("tracked.txt"), "base\n").expect("write tracked file");
        git_ok(&path, &["add", "--", "tracked.txt"]);
        git_ok(
            &path,
            &[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "--no-verify",
                "--no-gpg-sign",
                "-m",
                "initial",
            ],
        );
        let path = fs::canonicalize(path).expect("canonical repository");
        let base_commit = git_text(&path, &["rev-parse", "HEAD"]);
        Self {
            _temporary: temporary,
            path,
            base_commit,
        }
    }

    fn run_root(&self, run_id: &str) -> PathBuf {
        self.path.join(".gloop/runs").join(run_id)
    }
}

fn git(repository: &Path, arguments: &[&str]) -> Output {
    let mut command = Command::new("git");
    command
        .env_clear()
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .arg("-C")
        .arg(repository)
        .args(arguments);
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    command.output().expect("run test Git command")
}

fn git_ok(repository: &Path, arguments: &[&str]) -> Output {
    let output = git(repository, arguments);
    assert!(
        output.status.success(),
        "git {arguments:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn git_text(repository: &Path, arguments: &[&str]) -> String {
    String::from_utf8(git_ok(repository, arguments).stdout)
        .expect("UTF-8 Git output")
        .trim()
        .to_owned()
}

fn shell_node(id: &str, script: &str) -> Node {
    Node::command(
        id,
        vec!["/bin/sh".to_owned(), "-c".to_owned(), script.to_owned()],
    )
}

fn worktree_node(id: &str, script: &str, auto_commit: bool) -> Node {
    let mut node = shell_node(id, script);
    node.workspace = WorkspaceSpec::Worktree {
        base: None,
        auto_commit,
    };
    node
}

async fn run_graph(
    repository: &TestRepository,
    run_id: &str,
    graph: &Graph,
) -> gloop_core::RunSummary {
    let runtime = Runtime::new(
        ProviderRegistry::default(),
        repository.path.join(".gloop/runs"),
    );
    runtime
        .run(
            graph,
            RunOptions {
                run_id: Some(run_id.to_owned()),
                current_dir: repository.path.clone(),
                ..RunOptions::default()
            },
        )
        .await
        .expect("runtime run")
}

fn load_manifest(run_root: &Path, summary: &gloop_core::RunSummary) -> WorktreeManifest {
    let artifact = summary
        .artifacts
        .iter()
        .find(|artifact| artifact.kind == "worktree_manifest")
        .expect("worktree manifest artifact");
    assert_eq!(artifact.path, "worktree-manifest.json");
    assert!(artifact.size.is_some_and(|size| size > 0));
    assert!(
        artifact
            .sha256
            .as_ref()
            .is_some_and(|hash| hash.len() == 64)
    );
    let bytes = fs::read(run_root.join(&artifact.path)).expect("read worktree manifest");
    serde_json::from_slice(&bytes).expect("decode worktree manifest")
}

#[tokio::test]
async fn current_only_non_git_run_does_not_construct_worktree_manager() {
    let temporary = tempfile::tempdir().expect("temporary non-Git workspace");
    let graph = Graph::new(
        "current-only",
        "run without Git",
        vec![shell_node("current", "printf current")],
    );
    let runtime = Runtime::new(ProviderRegistry::default(), temporary.path().join("runs"));
    let summary = runtime
        .run(
            &graph,
            RunOptions {
                run_id: Some("current-only".to_owned()),
                current_dir: temporary.path().to_path_buf(),
                ..RunOptions::default()
            },
        )
        .await
        .expect("current-only run succeeds without Git");

    assert_eq!(summary.status, FinalStatus::ReadyForHuman);
    assert!(summary.provenance.base_commit.is_none());
    assert!(
        summary
            .artifacts
            .iter()
            .all(|artifact| artifact.kind != "worktree_manifest")
    );
    inspect_run(temporary.path().join("runs/current-only"))
        .await
        .expect("current-only run remains inspectable");
}

#[tokio::test]
async fn readonly_workspace_remains_explicitly_unsupported() {
    let temporary = tempfile::tempdir().expect("temporary non-Git workspace");
    let mut node = shell_node("readonly", "printf must-not-run > mutation.txt");
    node.workspace = WorkspaceSpec::Readonly;
    let graph = Graph::new("readonly", "reject unenforced readonly mode", vec![node]);
    let runtime = Runtime::new(ProviderRegistry::default(), temporary.path().join("runs"));
    let summary = runtime
        .run(
            &graph,
            RunOptions {
                run_id: Some("readonly-run".to_owned()),
                current_dir: temporary.path().to_path_buf(),
                ..RunOptions::default()
            },
        )
        .await
        .expect("unsupported readonly mode is reported in the run summary");

    assert_eq!(summary.status, FinalStatus::Failed);
    assert_eq!(summary.nodes["readonly"].status, NodeStatus::Failed);
    assert!(
        summary.nodes["readonly"]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("readonly workspace mode is unavailable"))
    );
    assert!(!temporary.path().join("mutation.txt").exists());
    assert!(summary.provenance.base_commit.is_none());
    inspect_run(temporary.path().join("runs/readonly-run"))
        .await
        .expect("readonly rejection remains inspectable");
}

#[tokio::test]
async fn worktree_inherit_keeps_exact_owner_workspace_and_commits_repairs() {
    let repository = TestRepository::new();
    let writer = worktree_node("writer", "printf writer > writer.txt; printf writer", true);
    let mut repair = shell_node(
        "repair",
        "test -f writer.txt && printf repair > repair.txt; printf repair",
    );
    repair.workspace = WorkspaceSpec::Inherit {
        node: "writer".to_owned(),
    };
    let mut finalize = shell_node(
        "finalize",
        "test -f repair.txt && printf final > final.txt; printf final",
    );
    finalize.workspace = WorkspaceSpec::Inherit {
        node: "repair".to_owned(),
    };
    let mut graph = Graph::new(
        "inherit",
        "inherit owner workspace",
        vec![writer, repair, finalize],
    );
    graph.spec.edges.push(Edge::data("writer", "repair"));
    graph.spec.edges.push(Edge::data("repair", "finalize"));

    let summary = run_graph(&repository, "inherit-run", &graph).await;
    assert_eq!(summary.status, FinalStatus::ReadyForHuman);
    assert_eq!(
        summary.nodes["writer"].workspace,
        summary.nodes["repair"].workspace
    );
    assert_eq!(
        summary.nodes["repair"].workspace,
        summary.nodes["finalize"].workspace
    );
    let manifest = load_manifest(&repository.run_root("inherit-run"), &summary);
    assert_eq!(manifest.base_commit, repository.base_commit);
    assert_eq!(
        summary.provenance.base_commit,
        Some(manifest.base_commit.clone())
    );
    assert_eq!(manifest.records.len(), 1);
    let record = &manifest.records[0];
    assert_eq!(record.node, "writer");
    assert!(record.path.join("writer.txt").is_file());
    assert!(record.path.join("repair.txt").is_file());
    assert!(record.path.join("final.txt").is_file());
    assert!(!record.dirty);
    assert_ne!(record.commit, record.base_commit);
    let commit_range = format!("{}..{}", record.base_commit, record.commit);
    assert_eq!(
        git_text(&record.path, &["rev-list", "--count", &commit_range]),
        "3"
    );
    assert!(!repository.path.join("writer.txt").exists());
    assert!(!repository.path.join("repair.txt").exists());
    assert!(!repository.path.join("final.txt").exists());

    let manifest_metadata = fs::metadata(
        repository
            .run_root("inherit-run")
            .join("worktree-manifest.json"),
    )
    .expect("manifest metadata");
    assert_eq!(manifest_metadata.permissions().mode() & 0o777, 0o600);
    let events = read_events(repository.run_root("inherit-run").join("journal.jsonl"))
        .await
        .expect("read journal");
    let finished = events
        .iter()
        .find(|event| event.kind == RunEventKind::RunFinished)
        .expect("run finished event");
    assert_eq!(
        finished
            .data
            .get("worktree_manifest_artifact")
            .and_then(serde_json::Value::as_str),
        Some("worktree-manifest.json")
    );
    inspect_run(repository.run_root("inherit-run"))
        .await
        .expect("manifest artifact is covered by replay and inspect");
}

#[tokio::test]
async fn retry_reuses_one_retained_worktree() {
    let repository = TestRepository::new();
    let mut probe = shell_node(
        "probe",
        "if test -f retry-marker; then printf success > retry-success; printf '{\"done\":true}'; else printf first > retry-marker; printf '{\"done\":false}'; fi",
    );
    if let NodeKind::Command { output, .. } = &mut probe.kind {
        output.format = OutputFormat::Json;
    }
    let nested = Graph::new("retry-attempt", "probe retry workspace", vec![probe]);
    let mut node = worktree_node("retry", "printf unused", false);
    node.kind = NodeKind::Loop {
        graph: Box::new(nested),
        until: LoopCondition {
            node: "probe".to_owned(),
            status: NodeStatus::Succeeded,
            output_contains: None,
            json_pointer: Some("/done".to_owned()),
            equals: Some(json!(true)),
        },
        max_iterations: 1,
        stagnation_after: 1,
    };
    node.retry.max_attempts = 2;
    let graph = Graph::new("retry", "reuse worktree", vec![node]);

    let summary = run_graph(&repository, "retry-run", &graph).await;
    assert_eq!(summary.status, FinalStatus::ReadyForHuman);
    assert_eq!(summary.nodes["retry"].attempts, 2);
    let manifest = load_manifest(&repository.run_root("retry-run"), &summary);
    assert_eq!(manifest.records.len(), 1);
    let record = &manifest.records[0];
    assert!(record.path.join("retry-marker").is_file());
    assert!(record.path.join("retry-success").is_file());
    assert!(record.dirty);
    let inspection = inspect_run(repository.run_root("retry-run"))
        .await
        .expect("inspect retry run");
    assert_eq!(
        inspection.replay.nodes["retry.attempt-1.iteration-1.probe"].workspace,
        inspection.replay.nodes["retry.attempt-2.iteration-1.probe"].workspace
    );
    assert_eq!(
        inspection.replay.nodes["retry.attempt-2.iteration-1.probe"].workspace,
        summary.nodes["retry"].workspace
    );
}

#[tokio::test]
async fn auto_commit_true_false_and_no_change_have_distinct_final_states() {
    let repository = TestRepository::new();

    let committed = run_graph(
        &repository,
        "commit-run",
        &Graph::new(
            "commit",
            "auto commit",
            vec![worktree_node(
                "change",
                "printf committed > committed.txt; printf ok",
                true,
            )],
        ),
    )
    .await;
    let committed_manifest = load_manifest(&repository.run_root("commit-run"), &committed);
    let committed_record = &committed_manifest.records[0];
    assert!(!committed_record.dirty);
    assert_ne!(committed_record.commit, committed_record.base_commit);

    let uncommitted = run_graph(
        &repository,
        "no-commit-run",
        &Graph::new(
            "no-commit",
            "retain changes",
            vec![worktree_node(
                "change",
                "printf uncommitted > uncommitted.txt; printf ok",
                false,
            )],
        ),
    )
    .await;
    let uncommitted_manifest = load_manifest(&repository.run_root("no-commit-run"), &uncommitted);
    let uncommitted_record = &uncommitted_manifest.records[0];
    assert!(uncommitted_record.dirty);
    assert_eq!(uncommitted_record.commit, uncommitted_record.base_commit);

    let unchanged = run_graph(
        &repository,
        "no-change-run",
        &Graph::new(
            "no-change",
            "no changes to commit",
            vec![worktree_node("change", "printf unchanged", true)],
        ),
    )
    .await;
    let unchanged_manifest = load_manifest(&repository.run_root("no-change-run"), &unchanged);
    let unchanged_record = &unchanged_manifest.records[0];
    assert!(!unchanged_record.dirty);
    assert_eq!(unchanged_record.commit, unchanged_record.base_commit);

    assert_eq!(
        fs::read_to_string(repository.path.join("tracked.txt")).expect("read original checkout"),
        "base\n"
    );
    assert!(!repository.path.join("committed.txt").exists());
    assert!(!repository.path.join("uncommitted.txt").exists());
}

#[tokio::test]
async fn nested_subgraph_and_loop_current_nodes_use_parent_worktrees() {
    let repository = TestRepository::new();

    let nested_subgraph = Graph::new(
        "nested-subgraph",
        "nested current",
        vec![shell_node(
            "inner",
            "printf nested > nested-subgraph.txt; printf nested",
        )],
    );
    let mut subgraph = shell_node("subgraph", "printf unused");
    subgraph.kind = NodeKind::Subgraph {
        graph: Box::new(nested_subgraph),
    };
    subgraph.workspace = WorkspaceSpec::Worktree {
        base: None,
        auto_commit: false,
    };

    let mut probe = shell_node(
        "probe",
        "printf loop > nested-loop.txt; printf '{\"done\":true}'",
    );
    if let NodeKind::Command { output, .. } = &mut probe.kind {
        output.format = OutputFormat::Json;
    }
    let nested_loop = Graph::new("nested-loop", "loop current", vec![probe]);
    let mut loop_node = shell_node("loop", "printf unused");
    loop_node.kind = NodeKind::Loop {
        graph: Box::new(nested_loop),
        until: LoopCondition {
            node: "probe".to_owned(),
            status: NodeStatus::Succeeded,
            output_contains: None,
            json_pointer: Some("/done".to_owned()),
            equals: Some(json!(true)),
        },
        max_iterations: 1,
        stagnation_after: 1,
    };
    loop_node.workspace = WorkspaceSpec::Worktree {
        base: None,
        auto_commit: false,
    };

    let graph = Graph::new(
        "nested",
        "propagate parent worktrees",
        vec![subgraph, loop_node],
    );
    let summary = run_graph(&repository, "nested-run", &graph).await;
    assert_eq!(summary.status, FinalStatus::ReadyForHuman);
    let manifest = load_manifest(&repository.run_root("nested-run"), &summary);
    assert_eq!(manifest.records.len(), 2);
    let subgraph_record = manifest
        .records
        .iter()
        .find(|record| record.node == "subgraph")
        .expect("subgraph record");
    let loop_record = manifest
        .records
        .iter()
        .find(|record| record.node == "loop")
        .expect("loop record");
    assert!(subgraph_record.path.join("nested-subgraph.txt").is_file());
    assert!(loop_record.path.join("nested-loop.txt").is_file());
    assert!(!repository.path.join("nested-subgraph.txt").exists());
    assert!(!repository.path.join("nested-loop.txt").exists());

    let inspection = inspect_run(repository.run_root("nested-run"))
        .await
        .expect("inspect nested run");
    assert_eq!(
        inspection.replay.nodes["subgraph.attempt-1.inner"].workspace,
        summary.nodes["subgraph"].workspace
    );
    assert_eq!(
        inspection.replay.nodes["loop.attempt-1.iteration-1.probe"].workspace,
        summary.nodes["loop"].workspace
    );
}

#[tokio::test]
async fn sibling_worktrees_are_isolated_and_original_checkout_is_unchanged() {
    let repository = TestRepository::new();
    let graph = Graph::new(
        "siblings",
        "isolated sibling writers",
        vec![
            worktree_node("left", "printf left > tracked.txt; printf left", false),
            worktree_node("right", "printf right > tracked.txt; printf right", false),
        ],
    );

    let summary = run_graph(&repository, "siblings-run", &graph).await;
    assert_eq!(summary.status, FinalStatus::ReadyForHuman);
    let manifest = load_manifest(&repository.run_root("siblings-run"), &summary);
    let left = manifest
        .records
        .iter()
        .find(|record| record.node == "left")
        .expect("left record");
    let right = manifest
        .records
        .iter()
        .find(|record| record.node == "right")
        .expect("right record");
    assert_ne!(left.path, right.path);
    assert_eq!(
        fs::read_to_string(left.path.join("tracked.txt")).expect("left tracked file"),
        "left"
    );
    assert_eq!(
        fs::read_to_string(right.path.join("tracked.txt")).expect("right tracked file"),
        "right"
    );
    assert_eq!(
        fs::read_to_string(repository.path.join("tracked.txt")).expect("original tracked file"),
        "base\n"
    );
}

#[tokio::test]
async fn failed_dirty_worktree_is_retained_without_auto_commit() {
    let repository = TestRepository::new();
    let graph = Graph::new(
        "failure",
        "retain failed worktree",
        vec![worktree_node(
            "failure",
            "printf dirty > failed.txt; printf failure >&2; exit 9",
            true,
        )],
    );

    let summary = run_graph(&repository, "failure-run", &graph).await;
    assert_eq!(summary.status, FinalStatus::Failed);
    assert_eq!(summary.nodes["failure"].status, NodeStatus::Failed);
    let manifest = load_manifest(&repository.run_root("failure-run"), &summary);
    let record = &manifest.records[0];
    assert!(record.path.is_dir());
    assert!(record.path.join("failed.txt").is_file());
    assert!(record.dirty);
    assert!(record.auto_commit);
    assert_eq!(record.commit, record.base_commit);
    inspect_run(repository.run_root("failure-run"))
        .await
        .expect("failed retained worktree remains inspectable");
}

#[tokio::test]
async fn dirty_repository_fails_with_typed_worktree_error_without_fallback() {
    let repository = TestRepository::new();
    fs::write(repository.path.join("tracked.txt"), "dirty\n").expect("dirty tracked file");
    let graph = Graph::new(
        "dirty",
        "reject dirty repository",
        vec![worktree_node(
            "writer",
            "printf fallback > must-not-exist.txt; printf unexpected",
            false,
        )],
    );
    let runtime = Runtime::new(
        ProviderRegistry::default(),
        repository.path.join(".gloop/runs"),
    );

    let error = runtime
        .run(
            &graph,
            RunOptions {
                run_id: Some("dirty-run".to_owned()),
                current_dir: repository.path.clone(),
                ..RunOptions::default()
            },
        )
        .await
        .expect_err("dirty repository must fail closed");
    assert!(
        matches!(
            error,
            RunError::Worktree(WorktreeError::DirtyRepository { .. })
        ),
        "unexpected error: {error:?}"
    );
    assert!(!repository.path.join("must-not-exist.txt").exists());
}
