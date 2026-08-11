use assert_cmd::prelude::*;
use predicates::prelude::predicate;
use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
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

fn cmd_in_isolation(project: &Path, home: &Path, path_dir: &Path) -> Command {
    let mut command = gloop_cmd();
    command.current_dir(project);
    command.env("HOME", home);
    command.env("XDG_CONFIG_HOME", home.join(".config"));
    let original_path = std::env::var("PATH").expect("PATH exists");
    command.env(
        "PATH",
        format!("{}:{}", path_dir.to_string_lossy(), original_path),
    );
    command
}

fn write_fake_executable(path_dir: &Path, name: &str, stdout: &str) {
    let script = format!("#!/bin/sh\n{stdout}\n");
    let path = path_dir.join(name);
    fs::write(&path, script).expect("write fake executable");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("mark executable");
}

fn write_project_profiles(project: &Path, source: &str) {
    let path = project.join(".gloop");
    fs::create_dir_all(&path).expect("create .gloop directory");
    fs::write(path.join("profiles.toml"), source).expect("write test profiles");
}

fn write_openai_like_builtins(project: &Path) {
    write_project_profiles(
        project,
        r#"
[profiles.codex]
kind = "openai"
model = "gpt-test"
api_key_env = "DOCTOR_TEST_TOKEN"

[profiles.claude]
kind = "openai"
model = "claude-instant"
api_key_env = "DOCTOR_TEST_TOKEN"

[profiles.qwen]
kind = "openai"
model = "qwen-test"
api_key_env = "DOCTOR_TEST_TOKEN"

[profiles.cursor-agent]
kind = "openai"
model = "cursor-test"
api_key_env = "DOCTOR_TEST_TOKEN"

[profiles.pi]
kind = "openai"
model = "pi-test"
api_key_env = "DOCTOR_TEST_TOKEN"
"#,
    );
}

#[test]
fn provider_list_supports_text_and_json_outputs() {
    let home = tempdir().expect("create temp home");
    let project = tempdir().expect("create temp project");
    let bin = tempdir().expect("create fake bin");

    let json = parse_json_output(
        &cmd_in_isolation(project.path(), home.path(), bin.path())
            .args(["provider", "list", "--json"])
            .assert()
            .success()
            .stderr(predicate::str::is_empty())
            .get_output()
            .stdout
            .clone(),
    );

    assert_eq!(json["success"].as_bool(), Some(true));
    let profiles = json["profiles"].as_array().expect("profiles array");
    assert!(!profiles.is_empty());
    assert!(profiles.iter().any(|profile| profile["name"] == "codex"));
    assert!(
        profiles
            .iter()
            .all(|profile| profile["enabled"].is_boolean())
    );

    let text = cmd_in_isolation(project.path(), home.path(), bin.path())
        .args(["provider", "list"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .get_output()
        .stdout
        .clone();

    let text = std::str::from_utf8(&text).expect("stdout is utf8");
    assert!(text.contains("profiles:"));
}

#[test]
fn provider_probe_uses_fake_executable_and_supports_text_and_json() {
    let home = tempdir().expect("create temp home");
    let project = tempdir().expect("create temp project");
    let bin = tempdir().expect("create fake bin");
    write_fake_executable(bin.path(), "probe-provider", "echo probe-provider 1.2.3");

    write_project_profiles(
        project.path(),
        r#"
[profiles.probe_target]
kind = "command"
argv = ["probe-provider"]
"#,
    );

    let text = cmd_in_isolation(project.path(), home.path(), bin.path())
        .args([
            "provider",
            "probe",
            "probe_target",
            "--trust-project-profiles",
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .get_output()
        .stdout
        .clone();
    let text = std::str::from_utf8(&text).expect("stdout is utf8");
    assert!(text.contains("profile 'probe_target' is available"));

    let json = parse_json_output(
        &cmd_in_isolation(project.path(), home.path(), bin.path())
            .args([
                "provider",
                "probe",
                "probe_target",
                "--json",
                "--trust-project-profiles",
            ])
            .assert()
            .success()
            .stderr(predicate::str::is_empty())
            .get_output()
            .stdout
            .clone(),
    );

    assert_eq!(json["success"].as_bool(), Some(true));
    assert_eq!(json["profile"].as_str(), Some("probe_target"));
    let probe = &json["probe"];
    assert_eq!(probe["available"].as_bool(), Some(true));
    assert_eq!(probe["version"].as_str(), Some("probe-provider 1.2.3"));
}

#[test]
fn provider_probe_fails_with_unresolved_profile_as_exit_code_7_in_json() {
    let home = tempdir().expect("create temp home");
    let project = tempdir().expect("create temp project");
    let bin = tempdir().expect("create fake bin");

    let output = cmd_in_isolation(project.path(), home.path(), bin.path())
        .args(["provider", "probe", "missing", "--json"])
        .assert()
        .code(7)
        .get_output()
        .stdout
        .clone();

    let value = parse_json_output(&output);
    assert_eq!(value["success"].as_bool(), Some(false));
    assert_eq!(value["code"].as_i64(), Some(7));
}

#[test]
fn provider_probe_reports_unavailable_command_profile_as_exit_code_4_in_json() {
    let home = tempdir().expect("create temp home");
    let project = tempdir().expect("create temp project");
    let bin = tempdir().expect("create fake bin");
    write_project_profiles(
        project.path(),
        r#"
[profiles.missing_exec]
kind = "command"
argv = ["definitely-not-real-executable"]
"#,
    );

    let output = cmd_in_isolation(project.path(), home.path(), bin.path())
        .args([
            "provider",
            "probe",
            "missing_exec",
            "--json",
            "--trust-project-profiles",
        ])
        .assert()
        .code(4)
        .get_output()
        .stdout
        .clone();

    let value = parse_json_output(&output);
    assert_eq!(value["success"].as_bool(), Some(false));
    assert_eq!(value["code"].as_i64(), Some(4));
    assert_eq!(
        value["details"]["probe"]["available"].as_bool(),
        Some(false)
    );
    assert_eq!(
        value["details"]["probe"]["profile"].as_str(),
        Some("missing_exec")
    );
}

#[test]
fn provider_doctor_json_reports_healthy_when_optional_builtins_are_absent() {
    let home = tempdir().expect("create temp home");
    let project = tempdir().expect("create temp project");
    let bin = tempdir().expect("create fake bin");
    write_openai_like_builtins(project.path());

    let output = cmd_in_isolation(project.path(), home.path(), bin.path())
        .args(["provider", "doctor", "--json", "--trust-project-profiles"])
        .env("DOCTOR_TEST_TOKEN", "not-a-real-key")
        .assert()
        .code(0)
        .stderr(predicate::str::is_empty())
        .get_output()
        .stdout
        .clone();

    let value = parse_json_output(&output);
    assert_eq!(value["success"].as_bool(), Some(true));
    assert_eq!(value["healthy"].as_bool(), Some(true));
    assert!(value["checks"].as_array().is_some());
}

#[test]
fn provider_doctor_reports_checks_in_text_and_json_without_printing_secrets() {
    let home = tempdir().expect("create temp home");
    let project = tempdir().expect("create temp project");
    let bin = tempdir().expect("create fake bin");
    write_fake_executable(bin.path(), "doctor-provider", "echo doctor-provider 0.9");

    let secret = "definitely_not_a_secret";
    write_project_profiles(
        project.path(),
        r#"
[profiles.commanded]
kind = "command"
argv = ["doctor-provider"]

[profiles.secret]
kind = "openai"
model = "gpt-test"
api_key_env = "GLOOP_TEST_SECRET"
"#,
    );

    let text_output = cmd_in_isolation(project.path(), home.path(), bin.path())
        .args(["provider", "doctor", "--trust-project-profiles"])
        .env("GLOOP_TEST_SECRET", secret)
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .get_output()
        .stdout
        .clone();
    let text = std::str::from_utf8(&text_output).expect("stdout is utf8");
    assert!(text.contains("doctor checks:"));
    assert!(!text.contains(secret));

    let json_output = cmd_in_isolation(project.path(), home.path(), bin.path())
        .args(["provider", "doctor", "--json", "--trust-project-profiles"])
        .env("GLOOP_TEST_SECRET", secret)
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .get_output()
        .stdout
        .clone();
    let json = parse_json_output(&json_output);
    assert_eq!(json["success"].as_bool(), Some(true));
    assert!(json["checks"].as_array().is_some());
    let checks = json["checks"].as_array().expect("checks array");
    assert!(!checks.is_empty());
    let names: Vec<_> = checks
        .iter()
        .filter_map(|entry| entry["profile"].as_str())
        .collect();
    assert!(names.contains(&"commanded"));
    assert!(names.contains(&"secret"));
    let json_text = std::str::from_utf8(&json_output).expect("stdout is utf8");
    assert!(!json_text.contains(secret));
}

#[test]
fn provider_add_can_add_then_override_profile_without_duplicates_and_supports_json() {
    let home = tempdir().expect("create temp home");
    let project = tempdir().expect("create temp project");
    let bin = tempdir().expect("create fake bin");
    write_fake_executable(bin.path(), "probe-v1", "echo version-one");
    write_fake_executable(bin.path(), "probe-v2", "echo version-two");

    write_project_profiles(
        project.path(),
        r#"
[profiles.local]
kind = "command"
argv = ["probe-v1"]
"#,
    );

    let first_probe = parse_json_output(
        &cmd_in_isolation(project.path(), home.path(), bin.path())
            .args([
                "provider",
                "probe",
                "local",
                "--json",
                "--trust-project-profiles",
            ])
            .assert()
            .success()
            .stderr(predicate::str::is_empty())
            .get_output()
            .stdout
            .clone(),
    );
    assert_eq!(
        first_probe["probe"]["version"].as_str(),
        Some("version-one")
    );

    let override_output = cmd_in_isolation(project.path(), home.path(), bin.path())
        .args([
            "provider",
            "add",
            "local",
            r#"kind = "command"
argv = ["probe-v2"]"#,
            "--json",
            "--trust-project-profiles",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let override_json = parse_json_output(&override_output);
    assert_eq!(override_json["success"].as_bool(), Some(true));
    assert_eq!(override_json["profile"].as_str(), Some("local"));

    let second_probe = parse_json_output(
        &cmd_in_isolation(project.path(), home.path(), bin.path())
            .args([
                "provider",
                "probe",
                "local",
                "--json",
                "--trust-project-profiles",
            ])
            .assert()
            .success()
            .stderr(predicate::str::is_empty())
            .get_output()
            .stdout
            .clone(),
    );
    assert_eq!(
        second_probe["probe"]["version"].as_str(),
        Some("version-two")
    );

    let list = parse_json_output(
        &cmd_in_isolation(project.path(), home.path(), bin.path())
            .args(["provider", "list", "--json", "--trust-project-profiles"])
            .assert()
            .success()
            .stderr(predicate::str::is_empty())
            .get_output()
            .stdout
            .clone(),
    );
    let local_count = list["profiles"]
        .as_array()
        .expect("profiles")
        .iter()
        .filter(|entry| entry["name"] == "local")
        .count();
    assert_eq!(local_count, 1);
}

#[test]
fn provider_project_profiles_are_ignored_without_trust_option() {
    let home = tempdir().expect("create temp home");
    let project = tempdir().expect("create temp project");
    let bin = tempdir().expect("create fake bin");
    write_fake_executable(bin.path(), "probe-token", "echo $PROJECT_TOKEN");

    write_project_profiles(
        project.path(),
        r#"
[profiles.trusted_token]
kind = "command"
argv = ["probe-token"]
env_from = { PROJECT_TOKEN = "PROJECT_TOKEN_SECRET" }
"#,
    );

    let unresolved = cmd_in_isolation(project.path(), home.path(), bin.path())
        .args(["provider", "probe", "trusted_token", "--json"])
        .env("PROJECT_TOKEN_SECRET", "secret-value")
        .assert()
        .code(7)
        .stderr(predicate::str::is_empty())
        .get_output()
        .stdout
        .clone();
    let unresolved = parse_json_output(&unresolved);
    assert_eq!(unresolved["success"].as_bool(), Some(false));
    assert_eq!(unresolved["code"].as_i64(), Some(7));

    let trusted = parse_json_output(
        &cmd_in_isolation(project.path(), home.path(), bin.path())
            .args([
                "provider",
                "probe",
                "trusted_token",
                "--json",
                "--trust-project-profiles",
            ])
            .env("PROJECT_TOKEN_SECRET", "secret-value")
            .assert()
            .success()
            .stderr(predicate::str::is_empty())
            .get_output()
            .stdout
            .clone(),
    );
    assert_eq!(trusted["success"].as_bool(), Some(true));
    assert!(trusted["probe"]["version"].is_null());
}

#[test]
fn provider_add_warns_when_project_profiles_not_trusted() {
    let home = tempdir().expect("create temp home");
    let project = tempdir().expect("create temp project");
    let bin = tempdir().expect("create fake bin");

    let json = cmd_in_isolation(project.path(), home.path(), bin.path())
        .args([
            "provider",
            "add",
            "trusted",
            r#"kind = "command"
argv = ["provider-add-check"]"#,
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json_value = parse_json_output(&json);
    assert_eq!(json_value["success"].as_bool(), Some(true));
    assert_eq!(
        json_value["project_profiles_enabled"].as_bool(),
        Some(false)
    );

    let output = cmd_in_isolation(project.path(), home.path(), bin.path())
        .args([
            "provider",
            "add",
            "trusted_text",
            "kind = \"command\"\nargv = [\"provider-add-check\"]",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = std::str::from_utf8(&output).expect("stdout is utf8");
    assert!(text.contains("project profiles are currently disabled"));
}
