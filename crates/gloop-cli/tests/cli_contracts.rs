use assert_cmd::prelude::*;
use predicates::prelude::predicate;
use std::process::Command;
use tempfile::tempdir;

fn gloop_cmd() -> Command {
    Command::cargo_bin("gloop").expect("gloop binary build is available")
}

#[test]
fn run_goal_and_graph_are_mutually_exclusive() {
    gloop_cmd()
        .args(["run", "Draft a tiny plan", "--graph", "graph.yml"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("cannot be used with"));
}

#[test]
fn run_graph_and_profile_are_mutually_exclusive() {
    gloop_cmd()
        .args(["run", "--graph", "graph.yml", "--profile", "default"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("cannot be used with"));
}

#[test]
fn run_graph_and_model_are_mutually_exclusive() {
    gloop_cmd()
        .args(["run", "--graph", "graph.yml", "--model", "gpt-4o"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("cannot be used with"));
}

#[test]
fn run_graph_and_interactive_are_mutually_exclusive() {
    gloop_cmd()
        .args(["run", "--graph", "graph.yml", "--interactive"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("cannot be used with"));
}

#[test]
fn run_interactive_and_non_interactive_are_mutually_exclusive() {
    gloop_cmd()
        .args(["run", "--interactive", "--non-interactive"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("cannot be used with"));
}

#[test]
fn run_goal_and_interactive_are_mutually_exclusive() {
    gloop_cmd()
        .args(["run", "draft this change", "--interactive"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("cannot be used with"));
}

#[test]
fn graph_new_interactive_rejects_template_shaping_flags() {
    for flag in [
        ["--template", "review-fix-loop"],
        ["--request", "seed request"],
        ["--provider-profiles", "one,two"],
        ["--loop-cap", "4"],
    ] {
        gloop_cmd()
            .args(["graph", "new", "--interactive"])
            .args(flag)
            .assert()
            .failure()
            .code(2)
            .stderr(predicate::str::contains("cannot be used with"));
    }
}

#[test]
fn graph_repo_is_shared_by_parent_and_subcommand_positions() {
    let dir = tempdir().expect("create tempdir");
    let repo = dir.path().to_str().expect("temp path");

    for args in [
        vec!["graph", "--repo", repo, "list", "--json"],
        vec!["graph", "list", "--repo", repo, "--json"],
    ] {
        gloop_cmd()
            .args(args)
            .assert()
            .success()
            .stdout(predicate::str::contains("\"success\": true"));
    }
}
