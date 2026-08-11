use assert_cmd::prelude::*;
use gloop_core::Graph;
use predicates::prelude::PredicateBooleanExt;
use predicates::prelude::predicate;
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::tempdir;

fn gloop_cmd() -> Command {
    Command::cargo_bin("gloop").expect("gloop binary build is available")
}

fn parse_json_output(bytes: &[u8]) -> Value {
    let text = std::str::from_utf8(bytes).expect("stdout is utf8").trim();
    assert!(text.starts_with('{') && text.ends_with('}'));
    serde_json::from_str(text).expect("stdout should be a JSON object")
}

fn write_graph(path: &Path) -> String {
    let out = gloop_cmd()
        .args([
            "graph",
            "new",
            path.to_str().expect("temp path"),
            "--name",
            "sample",
            "--goal",
            "validate test",
            "--force",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let value = parse_json_output(&out);
    assert_eq!(value["success"].as_bool(), Some(true));
    assert_eq!(
        value["written"].as_str(),
        Some(path.to_string_lossy().as_ref())
    );

    fs::read_to_string(path).expect("graph should be written")
}

#[test]
fn graph_help_contains_only_supported_commands() {
    let output = gloop_cmd()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("graph"))
        .stdout(predicate::str::contains("provider"))
        .stdout(predicate::str::contains("inspect"))
        .stdout(predicate::str::contains("daemon").not())
        .stdout(predicate::str::contains("queue").not())
        .get_output()
        .stdout
        .clone();

    let text = std::str::from_utf8(&output).expect("stdout is utf8");
    assert!(text.contains("gloop"));
    assert!(text.contains("Usage:"));
}

#[test]
fn graph_version_smoke_test() {
    let output = gloop_cmd()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::is_empty().not())
        .stderr(predicate::str::is_empty())
        .get_output()
        .stdout
        .clone();

    let text = std::str::from_utf8(&output).expect("stdout is utf8");
    assert!(text.trim().starts_with("gloop "));
}

#[test]
fn graph_new_writes_file_and_blocks_overwrite_without_force() {
    let dir = tempdir().expect("create tempdir");
    let path = dir.path().join("sample_graph.yml");

    let first = write_graph(&path);
    assert!(first.contains("apiVersion"));

    gloop_cmd()
        .args([
            "graph",
            "new",
            path.to_str().expect("temp path"),
            "--name",
            "sample",
            "--goal",
            "validate test",
        ])
        .assert()
        .code(6);

    write_graph(&path);
}

#[cfg(unix)]
#[test]
fn graph_new_writes_through_canonicalized_symlink_parent_directory() {
    use std::os::unix::fs::symlink;

    let dir = tempdir().expect("create tempdir");
    let target = tempdir().expect("create symlink target directory");
    let link = dir.path().join("graphs");
    symlink(target.path(), &link).expect("create symlinked parent");

    gloop_cmd()
        .args([
            "graph",
            "new",
            link.join("sample_graph.yml")
                .to_str()
                .expect("temp symlink path"),
            "--name",
            "sample",
            "--goal",
            "validate test",
            "--force",
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"success\": true"))
        .stderr(predicate::str::is_empty());

    let written = target.path().join("sample_graph.yml");
    let graph = Graph::from_path(&written).expect("graph written to canonical parent");
    assert_eq!(graph.metadata.name, "sample");
}

#[cfg(unix)]
#[test]
fn graph_new_rejects_symlink_destination() {
    use std::os::unix::fs::symlink;

    let dir = tempdir().expect("create tempdir");
    let target = dir.path().join("target.yml");
    fs::write(&target, "unchanged").expect("write target");
    let link = dir.path().join("graph.yml");
    symlink(&target, &link).expect("create symlink destination");

    let output = gloop_cmd()
        .args([
            "graph",
            "new",
            link.to_str().expect("temp symlink path"),
            "--name",
            "sample",
            "--goal",
            "validate test",
            "--force",
            "--json",
        ])
        .output()
        .expect("run graph new");
    assert_eq!(output.status.code(), Some(1));
    let payload = parse_json_output(&output.stdout);
    assert_eq!(payload["success"], false);
    assert!(
        payload["details"]["error"]
            .as_str()
            .is_some_and(|error| error.contains("output path is a symlink"))
    );
    assert_eq!(
        fs::read_to_string(target).expect("read target"),
        "unchanged"
    );
}

#[test]
fn graph_validate_success_and_failure_exit_codes() {
    let dir = tempdir().expect("create tempdir");
    let valid = dir.path().join("valid.yml");
    write_graph(&valid);

    gloop_cmd()
        .args(["graph", "validate", valid.to_str().expect("temp path")])
        .assert()
        .success()
        .stdout(predicate::str::contains("is valid"));

    let invalid = dir.path().join("invalid.yml");
    fs::write(
        &invalid,
        "apiVersion: gloop.dev/v1alpha1\nkind: Graph\nmetadata:\n  name: invalid\nspec:\n  goal: \"\"\n  policies: {}\n  budgets: {}\n  nodes: []\n",
    )
    .expect("write invalid graph");

    gloop_cmd()
        .args(["graph", "validate", invalid.to_str().expect("temp path")])
        .assert()
        .code(6)
        .stderr(predicate::str::is_empty())
        .stdout(predicate::str::is_empty().not());
}

#[test]
fn graph_explain_and_render_mermaid_and_dot() {
    let dir = tempdir().expect("create tempdir");
    let path = dir.path().join("explain_render.yml");
    write_graph(&path);

    gloop_cmd()
        .args(["graph", "explain", path.to_str().expect("temp path")])
        .assert()
        .success()
        .stdout(predicate::str::contains("Graph:"));

    gloop_cmd()
        .args([
            "graph",
            "render",
            path.to_str().expect("temp path"),
            "--format",
            "mermaid",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("flowchart TD"));

    gloop_cmd()
        .args([
            "graph",
            "render",
            path.to_str().expect("temp path"),
            "--format",
            "dot",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("digraph gloop {"));
}

#[test]
fn graph_schema_is_valid_json() {
    let output = gloop_cmd()
        .args(["graph", "schema", "--json"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .get_output()
        .stdout
        .clone();

    let value = parse_json_output(&output);
    assert!(value.is_object());
    assert!(value.get("$schema").is_some());
}

#[test]
fn run_dry_run_supports_text_and_json_without_progress_output() {
    let dir = tempdir().expect("create tempdir");
    let graph_path = dir.path().join("run.yml");
    write_graph(&graph_path);

    gloop_cmd()
        .args([
            "run",
            "--dry-run",
            "--graph",
            graph_path.to_str().expect("temp path"),
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .stdout(predicate::str::contains("dry-run ready"));

    let json_out = gloop_cmd()
        .args([
            "run",
            "--dry-run",
            "--json",
            "--graph",
            graph_path.to_str().expect("temp path"),
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .get_output()
        .stdout
        .clone();

    let value = parse_json_output(&json_out);
    assert_eq!(value["success"].as_bool(), Some(true));
}
