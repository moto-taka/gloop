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

#[test]
fn graph_init_non_interactive_saves_template() {
    let dir = tempdir().expect("create tempdir");
    let repo = dir.path();

    let output = gloop_cmd()
        .args([
            "graph",
            "init",
            "--repo",
            repo.to_str().expect("repo path"),
            "--name",
            "my-flow",
            "--from",
            "direct",
            "--description",
            "saved direct flow",
            "--request",
            "do the thing",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let value = parse_json_output(&output);
    assert_eq!(value["success"].as_bool(), Some(true));
    let written = value["written"].as_str().expect("written path");
    assert!(written.ends_with(".gloop/templates/my-flow.yaml"));

    let yaml = fs::read_to_string(written).expect("read saved template");
    assert!(yaml.contains("my-flow"));
    assert!(yaml.contains("do the thing"));
    assert!(yaml.contains("saved direct flow"));
}

#[test]
fn graph_init_refuses_overwrite_without_force() {
    let dir = tempdir().expect("create tempdir");
    let repo = dir.path();
    let template_path = repo.join(".gloop/templates/my-flow.yaml");

    gloop_cmd()
        .args([
            "graph",
            "init",
            "--repo",
            repo.to_str().expect("repo path"),
            "--name",
            "my-flow",
            "--from",
            "direct",
            "--json",
        ])
        .assert()
        .success();

    gloop_cmd()
        .args([
            "graph",
            "init",
            "--repo",
            repo.to_str().expect("repo path"),
            "--name",
            "my-flow",
            "--from",
            "direct",
        ])
        .assert()
        .code(6);

    let first = fs::read_to_string(&template_path).expect("first template still present");
    gloop_cmd()
        .args([
            "graph",
            "init",
            "--repo",
            repo.to_str().expect("repo path"),
            "--name",
            "my-flow",
            "--from",
            "direct",
            "--request",
            "updated",
            "--force",
            "--json",
        ])
        .assert()
        .success();

    let second = fs::read_to_string(template_path).expect("updated template");
    assert_ne!(first, second);
    assert!(second.contains("updated"));
}

#[test]
fn graph_init_list_reports_builtin_and_project_templates() {
    let dir = tempdir().expect("create tempdir");
    let repo = dir.path();

    gloop_cmd()
        .args([
            "graph",
            "init",
            "--repo",
            repo.to_str().expect("repo path"),
            "--name",
            "listed-flow",
            "--from",
            "direct",
            "--json",
        ])
        .assert()
        .success();

    let output = gloop_cmd()
        .args([
            "graph",
            "init",
            "--repo",
            repo.to_str().expect("repo path"),
            "--list",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let value = parse_json_output(&output);
    let templates = value["templates"].as_array().expect("templates array");
    assert!(
        templates
            .iter()
            .any(|entry| entry["name"] == "direct" && entry["source"] == "builtin")
    );
    assert!(
        templates
            .iter()
            .any(|entry| entry["name"] == "listed-flow" && entry["source"] == "project")
    );
}

#[test]
fn graph_new_resolves_saved_project_template() {
    let dir = tempdir().expect("create tempdir");
    let repo = dir.path();
    let workflow = dir.path().join("workflow.yaml");

    gloop_cmd()
        .args([
            "graph",
            "init",
            "--repo",
            repo.to_str().expect("repo path"),
            "--name",
            "saved-flow",
            "--from",
            "direct",
            "--request",
            "template request",
            "--json",
        ])
        .assert()
        .success();

    let output = gloop_cmd()
        .args([
            "graph",
            "new",
            workflow.to_str().expect("workflow path"),
            "--repo",
            repo.to_str().expect("repo path"),
            "--template",
            "saved-flow",
            "--name",
            "run-name",
            "--goal",
            "run goal",
            "--force",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let value = parse_json_output(&output);
    assert_eq!(value["success"].as_bool(), Some(true));

    let graph = Graph::from_path(&workflow).expect("workflow written");
    assert_eq!(graph.metadata.name, "run-name");
    assert_eq!(graph.spec.goal, "run goal");
    assert!(graph
        .spec
        .nodes
        .iter()
        .any(|node| node.id == "request"));
}

#[test]
fn graph_init_rejects_builtin_name_collision() {
    let dir = tempdir().expect("create tempdir");
    let repo = dir.path();

    let output = gloop_cmd()
        .args([
            "graph",
            "init",
            "--repo",
            repo.to_str().expect("repo path"),
            "--name",
            "direct",
            "--from",
            "plan-implement-verify",
            "--json",
        ])
        .output()
        .expect("run graph init");

    assert_eq!(output.status.code(), Some(6));
    let value = parse_json_output(&output.stdout);
    assert_eq!(value["success"], false);
    assert!(
        value["error"]
            .as_str()
            .is_some_and(|error| error.contains("built-in"))
    );
}

#[test]
fn graph_new_rejects_invalid_saved_template_yaml() {
    let dir = tempdir().expect("create tempdir");
    let repo = dir.path();
    let templates = repo.join(".gloop/templates");
    fs::create_dir_all(&templates).expect("create templates dir");
    fs::write(
        templates.join("broken.yaml"),
        "apiVersion: gloop.dev/v1alpha1\nkind: Graph\nmetadata:\n  name: broken\nspec:\n  goal: \"\"\n  policies: {}\n  budgets: {}\n  nodes: []\n",
    )
    .expect("write invalid template");

    let output = gloop_cmd()
        .args([
            "graph",
            "new",
            "workflow.yaml",
            "--repo",
            repo.to_str().expect("repo path"),
            "--template",
            "broken",
            "--json",
        ])
        .output()
        .expect("run graph new");

    assert_eq!(output.status.code(), Some(6));
    let value = parse_json_output(&output.stdout);
    assert_eq!(value["success"], false);
    assert!(
        value["error"]
            .as_str()
            .is_some_and(|error| error.contains("invalid"))
    );
}

#[test]
fn graph_new_rejects_template_name_path_traversal() {
    let dir = tempdir().expect("create tempdir");
    let repo = dir.path();

    for template in ["../../../outside", "/etc/passwd", "foo/bar"] {
        let output = gloop_cmd()
            .args([
                "graph",
                "new",
                "workflow.yaml",
                "--repo",
                repo.to_str().expect("repo path"),
                "--template",
                template,
                "--json",
            ])
            .output()
            .expect("run graph new");

        assert_eq!(output.status.code(), Some(6), "template {template}");
        let value = parse_json_output(&output.stdout);
        assert_eq!(value["success"], false);
        assert!(
            value["error"]
                .as_str()
                .is_some_and(|error| error.contains("invalid graph template name")),
            "template {template}: {}",
            value["error"]
        );
    }
}

#[cfg(unix)]
#[test]
fn graph_new_rejects_symlink_template_escape() {
    use std::os::unix::fs::symlink;

    let dir = tempdir().expect("create tempdir");
    let repo = dir.path();
    let templates = repo.join(".gloop/templates");
    fs::create_dir_all(&templates).expect("create templates dir");
    let outside = dir.path().join("outside.yaml");
    fs::write(
        &outside,
        "apiVersion: gloop.dev/v1alpha1\nkind: Graph\nmetadata:\n  name: outside\nspec:\n  goal: work\n  policies: {}\n  budgets: {}\n  nodes: []\n",
    )
    .expect("write outside template");
    symlink(&outside, templates.join("escaped.yaml")).expect("create symlink escape");

    let output = gloop_cmd()
        .args([
            "graph",
            "new",
            "workflow.yaml",
            "--repo",
            repo.to_str().expect("repo path"),
            "--template",
            "escaped",
            "--json",
        ])
        .output()
        .expect("run graph new");

    assert_eq!(output.status.code(), Some(6));
    let value = parse_json_output(&output.stdout);
    assert_eq!(value["success"], false);
    assert!(
        value["error"]
            .as_str()
            .is_some_and(|error| error.contains("escapes the templates directory"))
    );
}

#[cfg(unix)]
#[test]
fn graph_new_rejects_symlinked_templates_directory_escape() {
    use std::os::unix::fs::symlink;

    let dir = tempdir().expect("create tempdir");
    let repo = dir.path();
    let outside = tempdir().expect("outside tempdir");
    fs::write(
        outside.path().join("evil.yaml"),
        "apiVersion: gloop.dev/v1alpha1\nkind: Graph\nmetadata:\n  name: evil\nspec:\n  goal: work\n  policies: {}\n  budgets: {}\n  nodes: []\n",
    )
    .expect("write outside template");
    fs::create_dir(repo.join(".gloop")).expect("create .gloop dir");
    symlink(outside.path(), repo.join(".gloop/templates")).expect("symlink templates");

    let output = gloop_cmd()
        .args([
            "graph",
            "new",
            "workflow.yaml",
            "--repo",
            repo.to_str().expect("repo path"),
            "--template",
            "evil",
            "--json",
        ])
        .output()
        .expect("run graph new");

    assert_eq!(output.status.code(), Some(6));
    let value = parse_json_output(&output.stdout);
    assert_eq!(value["success"], false);
    assert!(
        value["error"]
            .as_str()
            .is_some_and(|error| error.contains("escapes the templates directory"))
    );
}

#[cfg(unix)]
#[test]
fn graph_new_rejects_symlinked_gloop_directory_escape() {
    use std::os::unix::fs::symlink;

    let dir = tempdir().expect("create tempdir");
    let repo = dir.path();
    let outside = tempdir().expect("outside tempdir");
    fs::create_dir_all(outside.path().join("templates")).expect("create templates dir");
    fs::write(
        outside.path().join("templates/evil.yaml"),
        "apiVersion: gloop.dev/v1alpha1\nkind: Graph\nmetadata:\n  name: evil\nspec:\n  goal: work\n  policies: {}\n  budgets: {}\n  nodes: []\n",
    )
    .expect("write outside template");
    symlink(outside.path(), repo.join(".gloop")).expect("symlink .gloop");

    let output = gloop_cmd()
        .args([
            "graph",
            "new",
            "workflow.yaml",
            "--repo",
            repo.to_str().expect("repo path"),
            "--template",
            "evil",
            "--json",
        ])
        .output()
        .expect("run graph new");

    assert_eq!(output.status.code(), Some(6));
    let value = parse_json_output(&output.stdout);
    assert_eq!(value["success"], false);
    assert!(
        value["error"]
            .as_str()
            .is_some_and(|error| error.contains("escapes the templates directory"))
    );
}

#[test]
fn graph_new_unknown_template_without_templates_dir_lists_builtins() {
    let dir = tempdir().expect("create tempdir");
    let repo = dir.path();

    let output = gloop_cmd()
        .args([
            "graph",
            "new",
            "workflow.yaml",
            "--repo",
            repo.to_str().expect("repo path"),
            "--template",
            "missing",
            "--json",
        ])
        .output()
        .expect("run graph new");

    assert_eq!(output.status.code(), Some(6));
    let value = parse_json_output(&output.stdout);
    assert_eq!(value["success"], false);
    assert!(
        value["error"]
            .as_str()
            .is_some_and(|error| error.contains("unknown graph template 'missing'"))
    );
    assert!(
        value["error"]
            .as_str()
            .is_some_and(|error| error.contains("direct"))
    );
}

#[test]
fn graph_new_unknown_template_with_dangling_gloop_symlink_lists_builtins() {
    use std::os::unix::fs::symlink;

    let dir = tempdir().expect("create tempdir");
    let repo = dir.path();
    let outside = tempdir().expect("outside tempdir");
    symlink(outside.path(), repo.join(".gloop")).expect("symlink .gloop");

    let output = gloop_cmd()
        .args([
            "graph",
            "new",
            "workflow.yaml",
            "--repo",
            repo.to_str().expect("repo path"),
            "--template",
            "evil",
            "--json",
        ])
        .output()
        .expect("run graph new");

    assert_eq!(output.status.code(), Some(6));
    let value = parse_json_output(&output.stdout);
    assert_eq!(value["success"], false);
    assert!(
        value["error"]
            .as_str()
            .is_some_and(|error| error.contains("unknown graph template 'evil'"))
    );
    assert!(
        value["error"]
            .as_str()
            .is_some_and(|error| error.contains("direct"))
    );
}

#[test]
fn graph_new_rejects_invalid_saved_template_even_with_goal_override() {
    let dir = tempdir().expect("create tempdir");
    let repo = dir.path();
    let templates = repo.join(".gloop/templates");
    fs::create_dir_all(&templates).expect("create templates dir");
    fs::write(
        templates.join("broken.yaml"),
        "apiVersion: gloop.dev/v1alpha1\nkind: Graph\nmetadata:\n  name: broken\nspec:\n  goal: \"\"\n  policies: {}\n  budgets: {}\n  nodes: []\n",
    )
    .expect("write invalid template");

    let output = gloop_cmd()
        .args([
            "graph",
            "new",
            "workflow.yaml",
            "--repo",
            repo.to_str().expect("repo path"),
            "--template",
            "broken",
            "--name",
            "run",
            "--goal",
            "override goal",
            "--json",
        ])
        .output()
        .expect("run graph new");

    assert_eq!(output.status.code(), Some(6));
    let value = parse_json_output(&output.stdout);
    assert_eq!(value["success"], false);
    assert!(
        value["error"]
            .as_str()
            .is_some_and(|error| error.contains("invalid"))
    );
}

#[test]
fn graph_init_rejects_interactive_authoring_flags_without_name_and_from() {
    let dir = tempdir().expect("create tempdir");
    let repo = dir.path();

    let output = gloop_cmd()
        .args([
            "graph",
            "init",
            "--repo",
            repo.to_str().expect("repo path"),
            "--description",
            "saved flow",
            "--json",
        ])
        .output()
        .expect("run graph init");

    assert_eq!(output.status.code(), Some(6));
    let value = parse_json_output(&output.stdout);
    assert_eq!(value["success"], false);
    assert!(
        value["error"]
            .as_str()
            .is_some_and(|error| error.contains("interactive graph init does not accept"))
    );
}

#[test]
fn graph_new_rejects_provider_profiles_and_loop_cap_for_saved_templates() {
    let dir = tempdir().expect("create tempdir");
    let repo = dir.path();

    gloop_cmd()
        .args([
            "graph",
            "init",
            "--repo",
            repo.to_str().expect("repo path"),
            "--name",
            "saved-flow",
            "--from",
            "direct",
            "--json",
        ])
        .assert()
        .success();

    for (flag, value) in [
        ("--provider-profiles", "codex"),
        ("--loop-cap", "2"),
    ] {
        let output = gloop_cmd()
            .args([
                "graph",
                "new",
                "workflow.yaml",
                "--repo",
                repo.to_str().expect("repo path"),
                "--template",
                "saved-flow",
                "--json",
                flag,
                value,
            ])
            .output()
            .expect("run graph new");

        assert_eq!(output.status.code(), Some(6), "flag {flag}");
        let payload = parse_json_output(&output.stdout);
        assert_eq!(payload["success"], false);
        assert!(
            payload["error"]
                .as_str()
                .is_some_and(|error| error.contains("not supported for saved project templates")),
            "flag {flag}: {}",
            payload["error"]
        );
    }
}
