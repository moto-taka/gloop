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
        vec!["--repo", repo, "graph", "list", "--json"],
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

#[test]
fn startup_options_work_without_a_subcommand_and_with_the_published_alias() {
    let repo = tempdir().expect("create tempdir");
    for prefix in [vec![], vec!["tui"]] {
        gloop_cmd()
            .args(prefix)
            .args(["--lang", "ja", "--repo"])
            .arg(repo.path())
            .assert()
            .failure()
            .code(1)
            .stderr(predicate::str::contains("The TUI needs a terminal"));
    }
    assert!(!repo.path().join(".gloop").exists());
}

#[test]
fn help_shows_the_canonical_commands_and_a_direct_start_example() {
    let output = gloop_cmd()
        .arg("--help")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let help = String::from_utf8(output).expect("help is UTF-8");
    let commands = help
        .split("Commands:\n")
        .nth(1)
        .expect("commands")
        .split("\nOptions:")
        .next()
        .expect("command section");
    for hidden in ["tui", "status", "inspect", "logs", "replay", "task-worker"] {
        assert!(
            !commands
                .lines()
                .any(|line| line.split_whitespace().next() == Some(hidden)),
            "{hidden} is a compatibility or internal command"
        );
    }
    assert!(commands.contains("debug"));
    assert!(help.contains("gloop --lang ja"));
}

#[test]
fn language_is_forwarded_before_or_after_graph_subcommands() {
    let repo = tempdir().expect("create tempdir");
    let mut outputs = Vec::new();
    for args in [
        vec!["--lang", "ja", "graph", "list"],
        vec!["graph", "--lang", "ja", "list"],
        vec!["graph", "list", "--language", "ja"],
    ] {
        outputs.push(
            gloop_cmd()
                .args(args)
                .arg("--repo")
                .arg(repo.path())
                .assert()
                .success()
                .get_output()
                .stdout
                .clone(),
        );
    }
    assert!(outputs.windows(2).all(|pair| pair[0] == pair[1]));
    let english = gloop_cmd()
        .args(["graph", "list", "--lang", "en", "--repo"])
        .arg(repo.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_ne!(outputs[0], english);
}
