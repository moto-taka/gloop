use assert_cmd::prelude::*;
use predicates::prelude::predicate;
use serde_json::Value;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::Command;
use tempfile::tempdir;

fn gloop_cmd() -> Command {
    Command::cargo_bin("gloop").expect("gloop binary build is available")
}

fn parse_json_output(bytes: &[u8]) -> Value {
    let text = std::str::from_utf8(bytes).expect("stdout is utf8").trim();
    serde_json::from_str(text).expect("stdout should be a single JSON object")
}

fn write_script(path: &Path, body: &str) -> String {
    let contents = format!("#!/usr/bin/env sh\n{body}\n");
    fs::write(path, contents).expect("write fake command");
    let metadata = fs::metadata(path).expect("script metadata");
    #[cfg(unix)]
    {
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).expect("make executable");
    }
    path.to_string_lossy().into_owned()
}

fn write_graph(path: &Path, script_path: &str, name: &str) -> String {
    let contents = format!(
        r#"
apiVersion: gloop.dev/v1alpha1
kind: Graph
metadata:
  name: {name}
  version: "1.0.0"
spec:
  goal: Execute command graph
  nodes:
    - id: run
      kind: command
      argv:
        - "{script_path}"
      output:
        format: text
"#
    );
    fs::write(path, contents).expect("write graph");
    path.to_string_lossy().into_owned()
}

fn write_fake_profile_config(repo: &Path, profile_name: &str, command_path: &str) {
    let profile_dir = repo.join(".gloop");
    fs::create_dir_all(&profile_dir).expect("create .gloop");
    let contents = format!(
        r#"
[profiles.{profile_name}]
kind = "command"
argv = ["{command_path}"]
prompt_mode = "argument"
prompt_args = ["{{prompt}}"]
output = "jsonl"
output_pointer = "/message"
"#
    );
    fs::write(profile_dir.join("profiles.toml"), contents).expect("write profiles.toml");
}

fn write_fake_profile_config_with_model_args(
    repo: &Path,
    profile_name: &str,
    command_path: &str,
    model_arg: &str,
    model_template: &str,
) {
    let profile_dir = repo.join(".gloop");
    fs::create_dir_all(&profile_dir).expect("create .gloop");
    let contents = format!(
        r#"
[profiles.{profile_name}]
kind = "command"
argv = ["{command_path}"]
prompt_mode = "argument"
prompt_args = ["{{prompt}}"]
model_args = ["{model_arg}", "{model_template}"]
output = "jsonl"
output_pointer = "/message"
"#
    );
    fs::write(profile_dir.join("profiles.toml"), contents).expect("write profiles.toml");
}

fn run_summary_has_profile(summary: &Value, expected: &str) -> bool {
    if summary
        .get("profiles_used")
        .and_then(Value::as_array)
        .is_some_and(|profiles| {
            profiles
                .iter()
                .any(|value| value.as_str() == Some(expected))
        })
    {
        return true;
    }
    summary
        .get("nodes")
        .and_then(Value::as_object)
        .is_some_and(|nodes| {
            nodes.values().any(|node| {
                node.get("profile")
                    .and_then(Value::as_str)
                    .is_some_and(|name| name == expected)
            })
        })
}

fn run_summary_has_model(summary: &Value, expected: &str) -> bool {
    summary
        .get("models_used")
        .and_then(Value::as_array)
        .is_some_and(|models| {
            models.iter().any(|entry| {
                entry
                    .get("reported_model")
                    .and_then(Value::as_str)
                    .is_some_and(|model| model == expected)
            })
        })
}

fn write_parallel_graph(
    path: &Path,
    max_parallel: usize,
    quick_script: &str,
    slow_script: &str,
) -> String {
    let contents = format!(
        r#"
apiVersion: gloop.dev/v1alpha1
kind: Graph
metadata:
  name: parallel-graph
  version: "1.0.0"
spec:
  goal: Parallel command failure behavior
  policies:
    max_parallel: {max_parallel}
  nodes:
    - id: quick
      kind: command
      argv:
        - "{quick_script}"
      output:
        format: text
    - id: slow
      kind: command
      argv:
        - "{slow_script}"
      output:
        format: text
"#
    );
    fs::write(path, contents).expect("write graph");
    path.to_string_lossy().into_owned()
}

fn write_incomplete_journal(path: &Path) {
    fs::write(path.join("journal.jsonl"), b"{\"event\"").expect("write incomplete journal");
}

fn write_tampered_journal(path: &Path) {
    fs::write(path.join("journal.jsonl"), b"not-json-line\n").expect("write tampered journal");
}

fn remove_last_journal_row(path: &Path) {
    let journal_path = path.join("journal.jsonl");
    let content = fs::read_to_string(&journal_path).expect("read journal");
    let mut rows = content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    let _ = rows.pop();
    let new_content = if rows.is_empty() {
        String::new()
    } else {
        format!("{}\n", rows.join("\n"))
    };
    fs::write(&journal_path, new_content).expect("write truncated journal");
}

#[test]
fn run_json_is_single_final_object_and_artifacts_written() {
    let dir = tempdir().expect("create tempdir");
    let repo = dir.path();
    let script = write_script(&repo.join("success.sh"), "printf \"run-ok\\n\"");
    let graph = write_graph(&repo.join("run.yml"), &script, "run-json-artifacts");

    let output = gloop_cmd()
        .args([
            "run",
            "--json",
            "--non-interactive",
            "--graph",
            graph.as_str(),
            "--repo",
            repo.to_str().expect("repo path"),
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .get_output()
        .stdout
        .clone();

    let value = parse_json_output(&output);
    assert_eq!(value["success"].as_bool(), Some(true));
    let summary = &value["summary"];
    assert_eq!(summary["status"].as_str(), Some("ready_for_human"));
    let run_id = summary["run_id"].as_str().expect("summary includes run_id");
    let run_root = repo.join(".gloop").join("runs").join(run_id);
    assert!(run_root.join("journal.jsonl").exists());
    assert!(run_root.join("summary.json").exists());
    assert!(run_root.join("graph.json").exists());
    assert!(run_root.join("nodes").exists() || run_root.join("nodes").read_dir().is_ok());
}

#[test]
fn run_foreground_failed_command_returns_expected_exit_code() {
    let dir = tempdir().expect("create tempdir");
    let repo = dir.path();
    let script = write_script(&repo.join("fail.sh"), "printf \"run-fail\\n\" >&2\nexit 13");
    let graph = write_graph(&repo.join("run.yml"), &script, "run-failure");

    let output = gloop_cmd()
        .args([
            "run",
            "--json",
            "--non-interactive",
            "--graph",
            graph.as_str(),
            "--repo",
            repo.to_str().expect("repo path"),
        ])
        .assert()
        .code(3)
        .get_output()
        .stdout
        .clone();

    let value = parse_json_output(&output);
    assert_eq!(value["success"].as_bool(), Some(false));
    assert_eq!(value["summary"]["status"].as_str(), Some("failed"));
}

#[test]
fn run_json_stdout_is_exactly_one_json_object() {
    let dir = tempdir().expect("create tempdir");
    let repo = dir.path();
    let script = write_script(&repo.join("json.sh"), "printf \"json-check\\n\"");
    let graph = write_graph(&repo.join("run.yml"), &script, "run-json-object");

    let output = gloop_cmd()
        .args([
            "run",
            "--json",
            "--non-interactive",
            "--graph",
            graph.as_str(),
            "--repo",
            repo.to_str().expect("repo path"),
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .get_output()
        .stdout
        .clone();

    let text = std::str::from_utf8(&output).expect("stdout is utf8").trim();
    assert!(serde_json::from_str::<Value>(text).is_ok());
    let bytes = text.as_bytes();
    assert_eq!(bytes.first(), Some(&b'{'));
    assert_eq!(bytes.last(), Some(&b'}'));
}

#[test]
fn run_inline_with_fake_profile_records_profile_in_summary() {
    let dir = tempdir().expect("create tempdir");
    let repo = dir.path();
    let fake_profile = write_script(
        &repo.join("fake_profile.sh"),
        "printf '{\"message\":\"ok from fake profile\"}\\n'",
    );
    write_fake_profile_config(repo, "fake", &fake_profile);

    let output = gloop_cmd()
        .args([
            "run",
            "Draft and validate a tiny plan",
            "--profile",
            "fake",
            "--trust-project-profiles",
            "--non-interactive",
            "--json",
            "--repo",
            repo.to_str().expect("repo path"),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let value = parse_json_output(&output);
    assert_eq!(value["success"].as_bool(), Some(true));
    let summary = &value["summary"];
    assert_eq!(summary["status"].as_str(), Some("ready_for_human"));
    let run_id = summary["run_id"].as_str().expect("summary includes run_id");
    let run_root = repo.join(".gloop").join("runs").join(run_id);
    assert!(run_root.join("summary.json").exists());
    assert!(run_summary_has_profile(summary, "fake"));
}

#[test]
fn run_inline_with_fake_profile_and_model_records_model_override_in_summary() {
    let dir = tempdir().expect("create tempdir");
    let repo = dir.path();
    let fake_profile = write_script(
        &repo.join("fake_profile.sh"),
        r#"#!/usr/bin/env sh
if [ "$1" = "--version" ]; then
  echo "fake-profile v1.0.0"
  exit 0
fi
has_model=0
has_value=0
for arg in "$@"; do
  if [ "$arg" = "--model" ]; then
    has_model=1
  elif [ "$arg" = "test-model" ]; then
    has_value=1
  fi
done
if [ "$has_model" -ne 1 ] || [ "$has_value" -ne 1 ]; then
  printf 'missing model args\n' >&2
  exit 13
fi
printf '{"message":"ok from fake profile"}\n'
"#,
    );
    write_fake_profile_config_with_model_args(repo, "fake", &fake_profile, "--model", "{model}");

    let output = gloop_cmd()
        .args([
            "run",
            "Draft and validate a tiny plan",
            "--profile",
            "fake",
            "--model",
            "test-model",
            "--trust-project-profiles",
            "--non-interactive",
            "--json",
            "--repo",
            repo.to_str().expect("repo path"),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let value = parse_json_output(&output);
    assert_eq!(value["success"].as_bool(), Some(true));
    let summary = &value["summary"];
    assert_eq!(summary["status"].as_str(), Some("ready_for_human"));
    assert!(run_summary_has_profile(summary, "fake"));
    assert!(run_summary_has_model(summary, "test-model"));
}

#[test]
fn run_inline_missing_profile_should_exit_with_7() {
    let dir = tempdir().expect("create tempdir");
    let repo = dir.path();

    let status = gloop_cmd()
        .args([
            "run",
            "Draft a plan",
            "--profile",
            "missing-profile",
            "--non-interactive",
            "--json",
            "--repo",
            repo.to_str().expect("repo path"),
        ])
        .output()
        .expect("run missing-profile command");

    assert_eq!(status.status.code(), Some(7));
    let stdout = std::str::from_utf8(&status.stdout)
        .expect("stdout is utf8")
        .trim()
        .to_string();
    if !stdout.is_empty() {
        let value = parse_json_output(&status.stdout);
        assert_eq!(value["success"].as_bool(), Some(false));
        assert_eq!(value["summary"]["status"].as_str(), Some("failed"));
    }
}

#[test]
fn run_command_foreground_can_be_inspected_logged_and_replayed_with_ready_for_human() {
    let dir = tempdir().expect("create tempdir");
    let repo = dir.path();

    let script = write_script(&repo.join("success.sh"), "printf \"hello\n\" && exit 0");
    let graph = write_graph(&repo.join("run.yml"), &script, "run-replay-cycle");

    let run_output = gloop_cmd()
        .args([
            "run",
            "--json",
            "--non-interactive",
            "--graph",
            graph.as_str(),
            "--repo",
            repo.to_str().expect("repo path"),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let run_json = parse_json_output(&run_output);
    assert_eq!(run_json["success"].as_bool(), Some(true));
    let summary = &run_json["summary"];
    assert_eq!(summary["status"].as_str(), Some("ready_for_human"));
    let run_id = summary["run_id"].as_str().expect("summary includes run_id");
    let run_root = repo.join(".gloop").join("runs").join(run_id);

    let inspect_output = gloop_cmd()
        .args(["inspect", run_root.to_str().expect("run root"), "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let inspect_json = parse_json_output(&inspect_output);
    let inspect_status = inspect_json["inspection"]["summary"]["status"]
        .as_str()
        .or_else(|| inspect_json["inspection"]["summary"]["final_status"].as_str())
        .or_else(|| inspect_json["summary"]["status"].as_str())
        .or_else(|| inspect_json["success"].as_str());
    assert_eq!(inspect_status, Some("ready_for_human"));

    let logs_output = gloop_cmd()
        .args(["logs", run_root.to_str().expect("run root"), "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let logs_json = parse_json_output(&logs_output);
    let events = logs_json["events"].as_array().expect("logs has events");
    let has_started = events
        .iter()
        .any(|event| event["kind"].as_str() == Some("run_started"));
    let has_finished = events
        .iter()
        .any(|event| event["kind"].as_str() == Some("run_finished"));
    assert!(has_started);
    assert!(has_finished);

    let replay_output = gloop_cmd()
        .args(["replay", run_root.to_str().expect("run root"), "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let replay_json = parse_json_output(&replay_output);
    let replay_status = replay_json["replay"]["final_status"]
        .as_str()
        .or_else(|| replay_json["replay"]["summary"]["status"].as_str())
        .or_else(|| replay_json["summary"]["status"].as_str())
        .or_else(|| replay_json["success"].as_str());
    assert_eq!(replay_status, Some("ready_for_human"));
}

#[test]
fn run_with_max_parallel_cap_does_not_raise_graph_policy() {
    let dir = tempdir().expect("create tempdir");
    let repo = dir.path();
    let script = write_script(&repo.join("sleep.sh"), "sleep 0.3\n");
    let graph = write_parallel_graph(&repo.join("run.yml"), 1, &script, &script);

    let output = gloop_cmd()
        .args([
            "run",
            "--json",
            "--non-interactive",
            "--graph",
            graph.as_str(),
            "--repo",
            repo.to_str().expect("repo path"),
            "--max-parallel",
            "4",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let value = parse_json_output(&output);
    assert_eq!(value["success"].as_bool(), Some(true));
    assert_eq!(value["summary"]["status"].as_str(), Some("ready_for_human"));
    let run_id = value["summary"]["run_id"]
        .as_str()
        .expect("summary includes run_id");
    let run_root = repo.join(".gloop").join("runs").join(run_id);
    let authored_graph = fs::read_to_string(repo.join("run.yml")).expect("read authored graph");
    assert!(authored_graph.contains("max_parallel: 1"));

    let logs_output = gloop_cmd()
        .args(["logs", run_root.to_str().expect("run root"), "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let logs = parse_json_output(&logs_output);
    let start_event = logs["events"]
        .as_array()
        .and_then(|events| {
            events
                .iter()
                .find(|event| event["kind"].as_str() == Some("run_started"))
        })
        .expect("run_started in logs");
    assert_eq!(start_event["data"]["max_parallel"].as_u64(), Some(1));
}

#[test]
fn run_dry_run_rejects_zero_max_parallel() {
    let dir = tempdir().expect("create tempdir");
    let repo = dir.path();
    let script = write_script(&repo.join("sleep.sh"), "sleep 0.1\n");
    let graph = write_graph(&repo.join("run.yml"), &script, "run-dry-run-max-parallel");

    let output = gloop_cmd()
        .args([
            "run",
            "--json",
            "--dry-run",
            "--non-interactive",
            "--graph",
            graph.as_str(),
            "--repo",
            repo.to_str().expect("repo path"),
            "--max-parallel",
            "0",
        ])
        .output()
        .expect("run dry-run command");

    assert_eq!(output.status.code(), Some(6));
    let value = parse_json_output(&output.stdout);
    assert_eq!(value["success"].as_bool(), Some(false));
    assert_eq!(value["code"].as_i64(), Some(6));
    assert!(
        value["error"]
            .as_str()
            .is_some_and(|message| message.contains("max-parallel"))
    );
}

#[cfg(unix)]
#[test]
fn run_logs_rejects_symlinked_journal() {
    let dir = tempdir().expect("create tempdir");
    let run_root = dir.path();
    let target = run_root.join("journal-target.jsonl");
    fs::write(&target, b"{}").expect("write target");
    let link = run_root.join("journal.jsonl");
    symlink(&target, &link).expect("create symlinked journal");

    let output = gloop_cmd()
        .args(["logs", run_root.to_str().expect("run root"), "--json"])
        .output()
        .expect("logs command");

    assert_eq!(output.status.code(), Some(6));
    let value = parse_json_output(&output.stdout);
    assert_eq!(value["success"].as_bool(), Some(false));
    assert!(
        value["details"]["error"]
            .as_str()
            .is_some_and(|message| message.contains("symlink"))
    );
}

#[cfg(unix)]
#[test]
fn run_logs_rejects_symlinked_run_directory() {
    let dir = tempdir().expect("create tempdir");
    let actual_run = dir.path().join("actual-run");
    fs::create_dir(&actual_run).expect("create actual run directory");
    fs::write(actual_run.join("journal.jsonl"), b"{}").expect("write target journal");
    let linked_run = dir.path().join("linked-run");
    symlink(&actual_run, &linked_run).expect("create symlinked run directory");

    let output = gloop_cmd()
        .args(["logs", linked_run.to_str().expect("run root"), "--json"])
        .output()
        .expect("logs command");

    assert_eq!(output.status.code(), Some(6));
    let value = parse_json_output(&output.stdout);
    assert_eq!(value["success"].as_bool(), Some(false));
    assert!(
        value["details"]["error"]
            .as_str()
            .is_some_and(|message| message.contains("symlink"))
    );
}

#[test]
fn run_logs_rejects_non_regular_journal() {
    let dir = tempdir().expect("create tempdir");
    let run_root = dir.path();
    fs::create_dir(run_root.join("journal.jsonl")).expect("create non-regular journal path");

    let output = gloop_cmd()
        .args(["logs", run_root.to_str().expect("run root"), "--json"])
        .output()
        .expect("logs command");

    assert_eq!(output.status.code(), Some(6));
    let value = parse_json_output(&output.stdout);
    assert_eq!(value["success"].as_bool(), Some(false));
    assert!(
        value["details"]["error"]
            .as_str()
            .is_some_and(|message| message.contains("regular file"))
    );
}

#[test]
fn run_failed_node_exit_prefers_verification_code_over_cancelled_siblings() {
    let dir = tempdir().expect("create tempdir");
    let repo = dir.path();
    let fail_script = write_script(
        &repo.join("fail.sh"),
        "sleep 0.2\nprintf \"fail\\n\"\nexit 13\n",
    );
    let slow_script = write_script(&repo.join("slow.sh"), "sleep 1\nprintf \"slow\\n\"");
    let graph = write_parallel_graph(&repo.join("run.yml"), 2, &fail_script, &slow_script);

    let output = gloop_cmd()
        .args([
            "run",
            "--json",
            "--non-interactive",
            "--graph",
            graph.as_str(),
            "--repo",
            repo.to_str().expect("repo path"),
            "--max-parallel",
            "2",
        ])
        .assert()
        .code(3)
        .get_output()
        .stdout
        .clone();

    let value = parse_json_output(&output);
    assert_eq!(value["success"].as_bool(), Some(false));
    assert_eq!(value["summary"]["status"].as_str(), Some("failed"));
    let nodes = value["summary"]["nodes"]
        .as_object()
        .expect("summary has nodes");
    let has_failed = nodes
        .values()
        .any(|node| node["status"].as_str() == Some("failed"));
    let has_cancelled = nodes
        .values()
        .any(|node| node["status"].as_str() == Some("cancelled"));
    assert!(has_failed);
    assert!(has_cancelled);
}

#[test]
fn run_logs_incomplete_journal_is_a_local_cli_error() {
    let dir = tempdir().expect("create tempdir");
    write_incomplete_journal(dir.path());

    let output = gloop_cmd()
        .args(["logs", dir.path().to_str().expect("temp dir"), "--json"])
        .assert()
        .code(6)
        .get_output()
        .stdout
        .clone();

    let value = parse_json_output(&output);
    assert_eq!(value["success"].as_bool(), Some(false));
    assert!(
        value["error"]
            .as_str()
            .is_some_and(|message| message.contains("incomplete"))
    );
}

#[test]
fn run_logs_tampered_journal_is_a_local_cli_error() {
    let dir = tempdir().expect("create tempdir");
    write_tampered_journal(dir.path());

    let output = gloop_cmd()
        .args(["logs", dir.path().to_str().expect("temp dir"), "--json"])
        .assert()
        .code(6)
        .get_output()
        .stdout
        .clone();

    let value = parse_json_output(&output);
    assert_eq!(value["success"].as_bool(), Some(false));
    assert!(
        value["error"]
            .as_str()
            .is_some_and(|message| message.contains("corrupted"))
    );
}

#[test]
fn run_logs_missing_final_run_finished_row_is_a_local_cli_error() {
    let dir = tempdir().expect("create tempdir");
    let repo = dir.path();
    let script = write_script(&repo.join("success.sh"), "printf \"run-ok\\n\"");
    let graph = write_graph(&repo.join("run.yml"), &script, "run-missing-final-row");

    let output = gloop_cmd()
        .args([
            "run",
            "--json",
            "--non-interactive",
            "--graph",
            graph.as_str(),
            "--repo",
            repo.to_str().expect("repo path"),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let value = parse_json_output(&output);
    assert_eq!(value["success"].as_bool(), Some(true));
    let run_id = value["summary"]["run_id"]
        .as_str()
        .expect("summary includes run_id");
    let run_root = repo.join(".gloop").join("runs").join(run_id);
    remove_last_journal_row(&run_root);

    let logs_output = gloop_cmd()
        .args(["logs", run_root.to_str().expect("run root"), "--json"])
        .assert()
        .code(6)
        .get_output()
        .stdout
        .clone();

    let value = parse_json_output(&logs_output);
    assert_eq!(value["success"].as_bool(), Some(false));
    assert!(
        value["error"]
            .as_str()
            .is_some_and(|message| message.contains("incomplete"))
    );
}
