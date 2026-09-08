#![cfg(unix)]

use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

use assert_cmd::prelude::*;
use serde_json::Value;
use tempfile::{TempDir, tempdir};

fn cli(repo: &Path) -> Command {
    let mut command = Command::cargo_bin("gloop").expect("gloop binary");
    command.current_dir(repo).arg("--trust-project-profiles");
    command
}

fn json(command: &mut Command) -> Value {
    let output = command.output().expect("run CLI");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("single JSON value")
}

fn fixture(body: &str) -> TempDir {
    let repo = tempdir().expect("repo");
    fs::create_dir(repo.path().join(".gloop")).unwrap();
    let script = repo.path().join("fixture-provider");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nif [ \"$1\" = '--version' ]; then echo fixture-1; exit 0; fi\n{body}\n"
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let echo = repo.path().join("fixture-handoff");
    fs::write(&echo, "#!/bin/sh\nif [ \"$1\" = '--version' ]; then echo fixture-1; exit 0; fi\nprintf '%s' \"$*\"\n").unwrap();
    fs::set_permissions(&echo, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        repo.path().join(".gloop/profiles.toml"),
        format!(
            r#"
[profiles.fixture]
kind = "command"
argv = ["{}"]
prompt_mode = "argument"
prompt_args = ["{{prompt}}"]
output = "text"

[profiles.handoff]
kind = "command"
argv = ["{}"]
prompt_mode = "argument"
prompt_args = ["{{prompt}}"]
output = "text"
"#,
            script.display(),
            echo.display()
        ),
    )
    .unwrap();
    repo
}

fn start(repo: &Path, id: &str, goal: &str) -> Value {
    json(cli(repo).args([
        "start",
        goal,
        "--profile",
        "fixture",
        "--id",
        id,
        "--timeout",
        "10",
        "--json",
    ]))
}

fn wait(repo: &Path, id: &str) -> Value {
    json(cli(repo).args(["tasks", id, "--wait", "--timeout", "20", "--json"]))
}

#[test]
fn claude_native_alias_succeeds_and_records_the_concrete_model() {
    let repo = fixture("printf unused");
    let executable = repo.path().join("claude");
    fs::write(&executable, "#!/bin/sh\nif [ \"$1\" = '--version' ]; then echo fixture; exit 0; fi\nprintf '%s\\n' '{\"type\":\"result\",\"result\":\"ALIAS_OK\",\"model\":\"claude-haiku-4-5-20251001\",\"modelUsage\":{\"claude-haiku-4-5-20251001\":{\"inputTokens\":1,\"outputTokens\":1}}}'\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        repo.path().join(".gloop/profiles.toml"),
        format!(
            r#"
[profiles.alias_fixture]
kind = "command"
argv = ["{}"]
prompt_mode = "argument"
prompt_args = ["{{prompt}}"]
model_args = ["--model", "{{model}}"]
output = "jsonl"
"#,
            executable.display()
        ),
    )
    .unwrap();
    json(cli(repo.path()).args([
        "start",
        "Check alias",
        "--profile",
        "alias_fixture",
        "--model",
        "haiku",
        "--id",
        "alias",
        "--timeout",
        "10",
        "--json",
    ]));
    let result = wait(repo.path(), "alias");
    assert_eq!(result["status"], "completed");
    assert_eq!(
        result["nodes"]["work"]["model"],
        "claude-haiku-4-5-20251001"
    );
    assert!(
        result["nodes"]["work"]["output"]
            .to_string()
            .contains("ALIAS_OK")
    );
    assert_eq!(result["job"]["request"]["model"], "haiku");
    json(cli(repo.path()).args([
        "start",
        "Check exact pin",
        "--profile",
        "alias_fixture",
        "--model",
        "different-exact-model",
        "--id",
        "pinned",
        "--timeout",
        "10",
        "--json",
    ]));
    cli(repo.path())
        .args(["tasks", "pinned", "--wait", "--timeout", "20", "--json"])
        .assert()
        .failure();
    let failure = json(cli(repo.path()).args(["tasks", "pinned", "--json"]));
    assert_eq!(
        failure["nodes"]["work"]["failure_class"],
        "provider_protocol"
    );
}

#[test]
fn detached_task_survives_launcher_exit_and_submission_is_idempotent() {
    let repo = fixture("sleep 0.3\nprintf 'called\\n' >> calls.txt\nprintf 'INDEPENDENT_RESULT'");
    let started = start(repo.path(), "durable", "Investigate the feature");
    assert_eq!(started["id"], "durable");
    let finished = wait(repo.path(), "durable");
    assert_eq!(finished["status"], "completed");
    assert!(
        finished["nodes"]["work"]["output"]
            .to_string()
            .contains("INDEPENDENT_RESULT")
    );
    assert_eq!(
        start(repo.path(), "durable", "Investigate the feature")["status"],
        "completed"
    );
    assert_eq!(
        fs::read_to_string(repo.path().join("calls.txt")).unwrap(),
        "called\n"
    );
    cli(repo.path())
        .args([
            "start",
            "Different task",
            "--profile",
            "fixture",
            "--id",
            "durable",
            "--json",
        ])
        .assert()
        .failure();
    let listed = json(cli(repo.path()).args(["tasks", "--json"]));
    assert_eq!(listed["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(listed["errors"], serde_json::json!([]));
}

#[test]
fn another_model_receives_previous_result_without_replaying_the_conversation() {
    let repo = fixture("printf 'FIRST_MODEL_EVIDENCE'");
    start(repo.path(), "first", "Find the evidence");
    wait(repo.path(), "first");
    json(cli(repo.path()).args([
        "start",
        "Check this evidence",
        "--profile",
        "handoff",
        "--after",
        "first",
        "--id",
        "second",
        "--json",
    ]));
    let result = wait(repo.path(), "second");
    let output = result["nodes"]["work"]["output"].to_string();
    assert!(output.contains("FIRST_MODEL_EVIDENCE"), "{output}");
    assert!(output.contains("Check this evidence"), "{output}");
    assert_eq!(result["job"]["request"]["after"], "first");
    assert!(result["job"]["handoff_bytes"].as_u64().unwrap() > 0);
    assert_eq!(result["job"]["handoff_truncated"], false);
}

#[test]
fn second_opinion_runs_after_first_model_and_counts_two_calls() {
    let repo = fixture("printf 'REVIEW_THIS_RESULT'");
    json(cli(repo.path()).args([
        "start",
        "Assess the task",
        "--profile",
        "fixture",
        "--review-profile",
        "handoff",
        "--max-calls",
        "2",
        "--id",
        "review",
        "--json",
    ]));
    let result = wait(repo.path(), "review");
    assert_eq!(result["nodes"].as_object().unwrap().len(), 2);
    assert_eq!(result["nodes"]["work"]["profile"], "fixture");
    assert_eq!(result["nodes"]["review"]["profile"], "handoff");
    assert!(
        result["nodes"]["review"]["output"]
            .to_string()
            .contains("REVIEW_THIS_RESULT")
    );
}

#[test]
fn stop_cancels_the_worker_and_releases_the_project_lock() {
    let repo = fixture("sleep 15\nprintf 'should not finish'");
    start(repo.path(), "slow", "Wait for cancellation");
    json(cli(repo.path()).args(["stop", "slow", "--json"]));
    let output = cli(repo.path())
        .args(["tasks", "slow", "--wait", "--timeout", "15", "--json"])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(130),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let cancelled: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(cancelled["status"], "cancelled");
    json(cli(repo.path()).args([
        "start",
        "Fresh task",
        "--profile",
        "handoff",
        "--id",
        "fresh",
        "--json",
    ]));
    assert_eq!(wait(repo.path(), "fresh")["status"], "completed");
}

#[test]
fn invalid_submissions_and_unsafe_task_paths_never_start_a_provider() {
    let repo = fixture("printf 'should not be called' > unexpected.txt");
    for args in [
        vec!["start", "task", "--profile", "fixture", "--max-calls", "0"],
        vec!["start", "task", "--profile", "fixture", "--timeout", "0"],
        vec!["start", "task", "--profile", "missing"],
        vec!["start", "task", "--profile", "fixture", "--id", "../escape"],
        vec![
            "start",
            "task",
            "--profile",
            "fixture",
            "--after",
            "unknown",
        ],
        vec!["tasks", "../escape"],
        vec!["stop", "../escape"],
    ] {
        cli(repo.path()).args(args).arg("--json").assert().failure();
    }
    assert!(!repo.path().join("unexpected.txt").exists());
    assert!(!repo.path().join(".gloop/jobs").exists());
}

#[test]
fn saved_workflow_uses_the_existing_runtime_without_an_ai_coordinator() {
    let repo = tempdir().unwrap();
    let graph = gloop_core::Graph::new(
        "local",
        "Print a result",
        vec![gloop_core::Node::command(
            "hello",
            vec!["printf".to_owned(), "WORKFLOW_RESULT".to_owned()],
        )],
    );
    fs::write(repo.path().join("workflow.yaml"), graph.to_yaml().unwrap()).unwrap();
    json(cli(repo.path()).args([
        "start",
        "--graph",
        "workflow.yaml",
        "--id",
        "graph",
        "--json",
    ]));
    let result = wait(repo.path(), "graph");
    assert_eq!(result["status"], "completed");
    assert!(
        result["nodes"]["hello"]["output"]
            .to_string()
            .contains("WORKFLOW_RESULT")
    );
}

fn plan_fixture(cycle: bool) -> TempDir {
    let document = serde_json::json!({"title":"Two steps", "steps":[
        {"id":"first", "title":"First", "instructions":"Write the result", "completion_criteria":"Result is present", "owned_files":[], "depends_on":if cycle { vec!["second"] } else { vec![] }},
        {"id":"second", "title":"Second", "instructions":"Check the first result", "completion_criteria":"A verdict is present", "owned_files":[], "depends_on":["first"]}
    ]});
    fixture(&format!(
        "printf 'call\\n' >> calls.txt\nprintf '%s' '{document}'"
    ))
}

#[test]
fn planning_makes_one_call_and_saves_a_proposal_without_executing_its_steps() {
    let repo = plan_fixture(false);
    let started = json(cli(repo.path()).args([
        "start",
        "Create a result and check it",
        "--plan",
        "--profile",
        "fixture",
        "--id",
        "proposal",
        "--timeout",
        "10",
        "--json",
    ]));
    assert_eq!(started["job"]["request"]["kind"], "planning");
    assert_eq!(started["job"]["request"]["max_calls"], 1);
    let result = wait(repo.path(), "proposal");
    assert_eq!(result["status"], "completed");
    assert_eq!(result["nodes"].as_object().unwrap().len(), 1);
    let plan: Value =
        serde_json::from_str(result["nodes"]["plan"]["output"].as_str().unwrap()).unwrap();
    assert_eq!(plan["steps"].as_array().unwrap().len(), 2);
    assert_eq!(
        fs::read_to_string(repo.path().join("calls.txt")).unwrap(),
        "call\n"
    );
}

#[test]
fn invalid_plan_fails_without_replanning_or_executing_any_step() {
    let repo = plan_fixture(true);
    json(cli(repo.path()).args([
        "start",
        "Create a plan",
        "--plan",
        "--profile",
        "fixture",
        "--id",
        "cyclic",
        "--timeout",
        "10",
        "--json",
    ]));
    cli(repo.path())
        .args(["tasks", "cyclic", "--wait", "--timeout", "20", "--json"])
        .assert()
        .code(3);
    let result = json(cli(repo.path()).args(["tasks", "cyclic", "--json"]));
    assert_eq!(result["status"], "failed");
    assert!(result["error"].as_str().unwrap().contains("cycle"));
    assert_eq!(
        fs::read_to_string(repo.path().join("calls.txt")).unwrap(),
        "call\n"
    );
}

#[test]
fn active_tasks_cannot_overlap_writes_in_one_project() {
    let repo = fixture("sleep 15\nprintf 'finished'");
    start(repo.path(), "locked", "Own the project");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !repo.path().join(".gloop/jobs/locked/started.json").exists() {
        assert!(std::time::Instant::now() < deadline, "worker did not start");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let output = cli(repo.path())
        .args([
            "start",
            "Concurrent edit",
            "--profile",
            "handoff",
            "--id",
            "conflict",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("another task is using this project"));
    assert!(!repo.path().join(".gloop/jobs/conflict").exists());
    json(cli(repo.path()).args(["stop", "locked", "--json"]));
    cli(repo.path())
        .args(["tasks", "locked", "--wait", "--timeout", "15", "--json"])
        .assert()
        .code(130);
}

#[test]
fn handoff_is_bounded_at_utf8_boundaries_and_reports_truncation() {
    let repo = fixture(&format!("printf '{}'", "確認".repeat(9000)));
    start(repo.path(), "long", "Read the long response");
    wait(repo.path(), "long");
    json(cli(repo.path()).args([
        "start",
        "Make a short decision",
        "--profile",
        "handoff",
        "--after",
        "long",
        "--id",
        "bounded",
        "--json",
    ]));
    let result = wait(repo.path(), "bounded");
    assert!(result["job"]["handoff_bytes"].as_u64().unwrap() <= 24 * 1024);
    assert_eq!(result["job"]["handoff_truncated"], true);
    let output = result["nodes"]["work"]["output"].to_string();
    assert!(output.contains("Make a short decision"));
    assert!(!output.contains('\u{fffd}'));
}

#[test]
fn managed_jobs_cannot_be_redirected_outside_the_project() {
    let repo = fixture("printf 'should not run'");
    let outside = tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), repo.path().join(".gloop/jobs")).unwrap();
    cli(repo.path())
        .args(["start", "task", "--profile", "fixture", "--json"])
        .assert()
        .failure();
    cli(repo.path())
        .args(["tasks", "--json"])
        .assert()
        .failure();
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
}
