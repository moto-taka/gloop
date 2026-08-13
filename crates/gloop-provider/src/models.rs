//! Bounded CLI model-list discovery for command profiles.

use std::{path::Path, process::Stdio, time::Duration};

use serde::{Deserialize, Serialize};
use tokio::{process::Command, time};

use crate::command::apply_isolated_environment;

const MODEL_LIST_TIMEOUT: Duration = Duration::from_secs(4);
const MAX_MODEL_LIST_OUTPUT: usize = 256 * 1024;
const MAX_DISCOVERED_MODELS: usize = 512;
const MAX_MODEL_ID_BYTES: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogModel {
    pub id: String,
    pub label: String,
}

impl CatalogModel {
    #[must_use]
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        let id = id.into();
        let label = label.into();
        Self { id, label }
    }

    #[must_use]
    pub fn uniform(id: impl Into<String>) -> Self {
        let id = id.into();
        Self {
            label: id.clone(),
            id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelDiscovery {
    Listed(Vec<CatalogModel>),
    Unsupported,
    Failed { reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CatalogFamily {
    CursorAgent,
    Pi,
    OpenCode,
}

pub fn executable_basename(argv0: &str) -> Option<&'static str> {
    let normalized = argv0.replace('\\', "/");
    let name = Path::new(&normalized)
        .file_name()
        .and_then(|value| value.to_str())?;
    let lower = name.to_ascii_lowercase();
    let stem = lower
        .strip_suffix(".exe")
        .or_else(|| lower.strip_suffix(".cmd"))
        .unwrap_or(lower.as_str());
    match stem {
        "cursor-agent" => Some("cursor-agent"),
        "pi" => Some("pi"),
        "opencode" => Some("opencode"),
        _ => None,
    }
}

pub fn catalog_family_for_argv0(argv0: &str) -> Option<CatalogFamily> {
    match executable_basename(argv0)? {
        "cursor-agent" => Some(CatalogFamily::CursorAgent),
        "pi" => Some(CatalogFamily::Pi),
        "opencode" => Some(CatalogFamily::OpenCode),
        _ => None,
    }
}

fn listing_args(family: CatalogFamily) -> &'static [&'static str] {
    match family {
        CatalogFamily::CursorAgent | CatalogFamily::Pi => &["--list-models"],
        CatalogFamily::OpenCode => &["models"],
    }
}

pub async fn discover_models_for_argv0(argv0: &str) -> ModelDiscovery {
    let Some(family) = catalog_family_for_argv0(argv0) else {
        return ModelDiscovery::Unsupported;
    };
    let deadline = time::Instant::now() + MODEL_LIST_TIMEOUT;
    let mut command = Command::new(argv0);
    #[cfg(unix)]
    command.process_group(0);
    apply_isolated_environment(&mut command);
    command
        .args(listing_args(family))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ModelDiscovery::Failed {
                reason: "not installed".to_owned(),
            };
        }
        Err(source) => {
            return ModelDiscovery::Failed {
                reason: format!("spawn failed: {source}"),
            };
        }
    };
    let process_group = child.id();
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    let mut stdout_task =
        tokio::spawn(crate::registry::drain_capped(stdout, MAX_MODEL_LIST_OUTPUT));
    let mut stderr_task =
        tokio::spawn(crate::registry::drain_capped(stderr, MAX_MODEL_LIST_OUTPUT));

    let wait_outcome = time::timeout_at(deadline, child.wait()).await;
    let status = match wait_outcome {
        Ok(Ok(status)) => status,
        Ok(Err(source)) => {
            abort_drains(&mut stdout_task, &mut stderr_task).await;
            return ModelDiscovery::Failed {
                reason: format!("wait failed: {source}"),
            };
        }
        Err(_) => {
            terminate_child(&mut child, process_group).await;
            abort_drains(&mut stdout_task, &mut stderr_task).await;
            return ModelDiscovery::Failed {
                reason: "timed out".to_owned(),
            };
        }
    };
    if !status.success() {
        terminate_child(&mut child, process_group).await;
    }

    let drain_outcome = time::timeout_at(deadline, async {
        tokio::join!(join_drain(&mut stdout_task), join_drain(&mut stderr_task))
    })
    .await;
    let (stdout_bytes, stderr_bytes) = match drain_outcome {
        Ok((Ok(stdout), Ok(stderr))) => (stdout, stderr),
        Ok((Err(reason), _) | (_, Err(reason))) => {
            terminate_child(&mut child, process_group).await;
            abort_drains(&mut stdout_task, &mut stderr_task).await;
            return ModelDiscovery::Failed { reason };
        }
        Err(_) => {
            terminate_child(&mut child, process_group).await;
            abort_drains(&mut stdout_task, &mut stderr_task).await;
            return ModelDiscovery::Failed {
                reason: "timed out".to_owned(),
            };
        }
    };

    if !status.success() {
        let snippet = String::from_utf8_lossy(&stderr_bytes.bytes);
        let reason = snippet
            .lines()
            .find(|line| !line.trim().is_empty())
            .map_or_else(
                || format!("exit code {}", status.code().unwrap_or(-1)),
                |line| line.trim().to_owned(),
            );
        return ModelDiscovery::Failed { reason };
    }
    if stdout_bytes.overflow || stderr_bytes.overflow {
        return ModelDiscovery::Failed {
            reason: "model list output was truncated".to_owned(),
        };
    }
    parse_model_list(family, &stdout_bytes.bytes)
}

async fn abort_drains(
    stdout_task: &mut tokio::task::JoinHandle<std::io::Result<crate::registry::CappedBytes>>,
    stderr_task: &mut tokio::task::JoinHandle<std::io::Result<crate::registry::CappedBytes>>,
) {
    stdout_task.abort();
    stderr_task.abort();
    let _ = stdout_task.await;
    let _ = stderr_task.await;
}

async fn join_drain(
    task: &mut tokio::task::JoinHandle<std::io::Result<crate::registry::CappedBytes>>,
) -> Result<crate::registry::CappedBytes, String> {
    task.await
        .map_err(|error| format!("reader failed: {error}"))?
        .map_err(|source| format!("read failed: {source}"))
}

async fn terminate_child(child: &mut tokio::process::Child, process_group: Option<u32>) {
    #[cfg(unix)]
    if let Some(pgid) = process_group {
        let pgid = format!("-{pgid}");
        let _ = std::process::Command::new("/bin/kill")
            .arg("-TERM")
            .arg(&pgid)
            .status();
        let _ = std::process::Command::new("/bin/kill")
            .arg("-KILL")
            .arg(&pgid)
            .status();
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}

pub fn parse_model_list(family: CatalogFamily, stdout: &[u8]) -> ModelDiscovery {
    let text = match std::str::from_utf8(stdout) {
        Ok(value) => strip_ansi(value),
        Err(_) => return failed("invalid utf-8 output"),
    };
    let models = parse_json_models(&text).or_else(|| parse_line_models(family, &text));
    let Some(models) = models else {
        return failed("no models in output");
    };
    let models = dedupe_models(models);
    if models.is_empty() {
        return failed("no models in output");
    }
    ModelDiscovery::Listed(models)
}

fn failed(reason: &str) -> ModelDiscovery {
    ModelDiscovery::Failed {
        reason: reason.to_owned(),
    }
}

fn strip_ansi(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        output.push(character);
    }
    output
}

fn parse_json_models(text: &str) -> Option<Vec<CatalogModel>> {
    let trimmed = text.trim();
    let value: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    let array = if let Some(array) = value.as_array() {
        array.clone()
    } else {
        value.get("models")?.as_array()?.clone()
    };
    let mut models = Vec::new();
    for entry in array {
        if let Some(model) = entry.as_str() {
            push_uniform_model(&mut models, model);
        } else if let Some(id) = entry.get("id").and_then(serde_json::Value::as_str) {
            let label = entry
                .get("name")
                .or_else(|| entry.get("label"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or(id);
            push_catalog_model(&mut models, id, label);
        } else if let Some(name) = entry.get("name").and_then(serde_json::Value::as_str) {
            push_uniform_model(&mut models, name);
        }
    }
    if models.is_empty() {
        None
    } else {
        Some(models)
    }
}

fn parse_line_models(family: CatalogFamily, text: &str) -> Option<Vec<CatalogModel>> {
    let mut models = Vec::new();
    for line in text.lines() {
        let mut trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        for prefix in ["- ", "* ", "• "] {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                trimmed = rest.trim();
            }
        }
        match family {
            CatalogFamily::CursorAgent => {
                let Some((id, display)) = trimmed.split_once(" - ") else {
                    continue;
                };
                let id = id.trim();
                let display = strip_model_annotations(display.trim());
                push_catalog_model(&mut models, id, display);
            }
            CatalogFamily::Pi => {
                let columns = trimmed.split_whitespace().collect::<Vec<_>>();
                if columns.len() < 2 || columns[0].eq_ignore_ascii_case("provider") {
                    continue;
                }
                let model_id = format!("{}/{}", columns[0], columns[1]);
                push_uniform_model(&mut models, &model_id);
            }
            CatalogFamily::OpenCode => {
                let token = trimmed
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .trim_end_matches(',');
                let Some((provider, model)) = token.split_once('/') else {
                    continue;
                };
                if provider.is_empty() || model.is_empty() {
                    continue;
                }
                push_uniform_model(&mut models, token);
            }
        }
    }
    if models.is_empty() {
        None
    } else {
        Some(models)
    }
}

fn strip_model_annotations(name: &str) -> &str {
    let name = name.trim();
    if let Some(index) = name.rfind(" (")
        && name.ends_with(')')
    {
        return name[..index].trim();
    }
    name
}

fn valid_model_id(candidate: &str) -> bool {
    let candidate = candidate.trim();
    if candidate.is_empty() || candidate.starts_with('-') {
        return false;
    }
    if candidate.len() > MAX_MODEL_ID_BYTES {
        return false;
    }
    candidate.bytes().all(|byte| {
        matches!(
            byte,
            b'a'..=b'z'
                | b'A'..=b'Z'
                | b'0'..=b'9'
                | b'.'
                | b'_'
                | b':'
                | b'/'
                | b'@'
                | b'-'
        )
    })
}

fn push_uniform_model(models: &mut Vec<CatalogModel>, candidate: &str) {
    let candidate = candidate.trim();
    if !valid_model_id(candidate) {
        return;
    }
    if models.len() < MAX_DISCOVERED_MODELS {
        models.push(CatalogModel::uniform(candidate));
    }
}

fn push_catalog_model(models: &mut Vec<CatalogModel>, id: &str, label: &str) {
    let id = id.trim();
    if !valid_model_id(id) {
        return;
    }
    let label = label.trim();
    if label.is_empty() {
        return;
    }
    if models.len() < MAX_DISCOVERED_MODELS {
        models.push(CatalogModel::new(id, label));
    }
}

fn dedupe_models(models: Vec<CatalogModel>) -> Vec<CatalogModel> {
    let mut seen = std::collections::BTreeSet::new();
    let mut output = Vec::new();
    for model in models {
        if seen.insert(model.id.clone()) {
            output.push(model);
        }
    }
    output.sort_by(|left, right| left.id.cmp(&right.id));
    output.truncate(MAX_DISCOVERED_MODELS);
    output
}

pub fn merge_profile_models(
    default_model: Option<&str>,
    discovered: &[CatalogModel],
    history: &[String],
) -> Vec<CatalogModel> {
    let mut models = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut push = |model: CatalogModel, validate_id: bool| {
        let id = model.id.trim();
        let label = model.label.trim();
        if id.is_empty()
            || label.is_empty()
            || (validate_id && !valid_model_id(id))
            || models.len() >= MAX_DISCOVERED_MODELS
            || !seen.insert(id.to_owned())
        {
            return;
        }
        models.push(CatalogModel::new(id, label));
    };
    if let Some(model) = default_model {
        push(CatalogModel::uniform(model), false);
    }
    for model in discovered {
        push(model.clone(), true);
    }
    for model in history {
        let model = model.trim();
        if model.is_empty() {
            continue;
        }
        push(CatalogModel::uniform(model), true);
    }
    models
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};

    #[test]
    fn parse_opencode_lines_preserves_provider_model_ids() {
        let output = "openai/gpt-4.1\nanthropic/claude-sonnet\n";
        let ModelDiscovery::Listed(models) =
            parse_model_list(CatalogFamily::OpenCode, output.as_bytes())
        else {
            panic!("expected listed models");
        };
        assert_eq!(
            models,
            vec![
                CatalogModel::uniform("anthropic/claude-sonnet"),
                CatalogModel::uniform("openai/gpt-4.1"),
            ]
        );
    }

    #[test]
    fn parse_opencode_skips_headers_and_incomplete_provider_rows() {
        let output = "Available providers:\nopenai/\n/openai\nopenai/gpt-4.1 (default)\n";
        let ModelDiscovery::Listed(models) =
            parse_model_list(CatalogFamily::OpenCode, output.as_bytes())
        else {
            panic!("expected listed models");
        };
        assert_eq!(models, vec![CatalogModel::uniform("openai/gpt-4.1")]);
    }

    #[test]
    fn merged_history_is_validated_and_bounded() {
        let history = vec![
            "valid-model".to_owned(),
            "model with spaces".to_owned(),
            "--not-a-model".to_owned(),
        ];
        let models = merge_profile_models(None, &[], &history);
        assert_eq!(
            models,
            vec![CatalogModel::uniform("valid-model")],
            "historical values must use the same model-id validation as discovery"
        );

        let history = (0..(MAX_DISCOVERED_MODELS + 32))
            .map(|index| format!("historical-{index}"))
            .collect::<Vec<_>>();
        let models = merge_profile_models(None, &[], &history);
        assert_eq!(models.len(), MAX_DISCOVERED_MODELS);
    }

    #[test]
    fn parse_model_ids_trims_whitespace_before_storing() {
        let output = " openai/gpt-4.1 \n";
        let ModelDiscovery::Listed(models) =
            parse_model_list(CatalogFamily::OpenCode, output.as_bytes())
        else {
            panic!("expected listed models");
        };
        assert_eq!(models, vec![CatalogModel::uniform("openai/gpt-4.1")]);
    }

    #[test]
    fn parse_json_models_deduplicates() {
        let output = r#"["gpt-4.1","gpt-4.1","claude"]"#;
        let ModelDiscovery::Listed(models) = parse_model_list(CatalogFamily::Pi, output.as_bytes())
        else {
            panic!("expected listed models");
        };
        assert_eq!(
            models,
            vec![
                CatalogModel::uniform("claude"),
                CatalogModel::uniform("gpt-4.1"),
            ]
        );
    }

    #[test]
    fn parse_cursor_preserves_id_and_label_and_strips_ansi() {
        let output = "\u{1b}[31mAvailable models\u{1b}[0m\ngpt-5.6-luna-xhigh - GPT-5.6 Luna 1M Extra High (current)\nclaude-opus-5-thinking-high - Opus 5 1M Thinking\n";
        let ModelDiscovery::Listed(models) =
            parse_model_list(CatalogFamily::CursorAgent, output.as_bytes())
        else {
            panic!("expected listed models");
        };
        assert_eq!(
            models,
            vec![
                CatalogModel::new("claude-opus-5-thinking-high", "Opus 5 1M Thinking"),
                CatalogModel::new("gpt-5.6-luna-xhigh", "GPT-5.6 Luna 1M Extra High"),
            ]
        );
    }

    #[test]
    fn parse_cursor_skips_header_lines_without_id_display_rows() {
        let output = "Available models\ngpt-5.6-luna-xhigh - GPT-5.6 Luna 1M Extra High\n";
        let ModelDiscovery::Listed(models) =
            parse_model_list(CatalogFamily::CursorAgent, output.as_bytes())
        else {
            panic!("expected listed models");
        };
        assert_eq!(
            models,
            vec![CatalogModel::new(
                "gpt-5.6-luna-xhigh",
                "GPT-5.6 Luna 1M Extra High"
            )]
        );
    }

    #[test]
    fn parse_pi_table_combines_provider_and_model() {
        let output = "provider model\nopenai gpt-4.1 extra\nanthropic claude-fable-5\n";
        let ModelDiscovery::Listed(models) = parse_model_list(CatalogFamily::Pi, output.as_bytes())
        else {
            panic!("expected listed models");
        };
        assert_eq!(
            models,
            vec![
                CatalogModel::uniform("anthropic/claude-fable-5"),
                CatalogModel::uniform("openai/gpt-4.1"),
            ]
        );
    }

    #[test]
    fn parse_rejects_invalid_and_oversized_ids() {
        let long = "a".repeat(129);
        let output = format!("provider model\nvalid model\n{long}\n-heading\n");
        let ModelDiscovery::Listed(models) = parse_model_list(CatalogFamily::Pi, output.as_bytes())
        else {
            panic!("expected listed models");
        };
        assert_eq!(models, vec![CatalogModel::uniform("valid/model")]);
    }

    #[test]
    fn basename_classifies_custom_paths_and_extensions() {
        assert_eq!(
            catalog_family_for_argv0("/opt/bin/cursor-agent"),
            Some(CatalogFamily::CursorAgent)
        );
        assert_eq!(
            catalog_family_for_argv0(r"C:\tools\opencode.exe"),
            Some(CatalogFamily::OpenCode)
        );
        assert_eq!(catalog_family_for_argv0("/opt/bin/codex"), None);
    }

    #[tokio::test]
    async fn discover_from_fake_pi_executable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("pi");
        fs::write(
            &path,
            "#!/bin/sh\necho 'provider model'\necho 'openai gpt-4.1'\n",
        )
        .expect("script");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
        let discovery = discover_models_for_argv0(path.to_str().expect("utf8 path")).await;
        assert_eq!(
            discovery,
            ModelDiscovery::Listed(vec![CatalogModel::uniform("openai/gpt-4.1")])
        );
    }

    #[tokio::test]
    async fn discover_uses_expected_listing_arguments_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cases = [
            (
                "cursor-agent",
                "--list-models",
                "gpt-5.6-luna-xhigh - GPT-5.6 Luna Extra High\n",
            ),
            ("pi", "--list-models", "provider model\nopenai gpt-4.1\n"),
            ("opencode", "models", "openai/gpt-4.1\n"),
        ];
        for (name, expected_arg, output) in cases {
            let path = dir.path().join(name);
            let args_path = dir.path().join(format!("{name}.args"));
            let script = format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nprintf '%s' '{}'\n",
                args_path.display(),
                output.replace('\'', "'\\''")
            );
            fs::write(&path, script).expect("script");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");

            let discovery = discover_models_for_argv0(path.to_str().expect("utf8 path")).await;
            assert!(matches!(discovery, ModelDiscovery::Listed(_)));
            assert_eq!(
                fs::read_to_string(&args_path).expect("args"),
                format!("{expected_arg}\n")
            );
        }
    }

    #[tokio::test]
    async fn discover_times_out_when_stdout_stays_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("pi");
        fs::write(
            &path,
            "#!/bin/sh\necho 'openai/gpt-4.1'\n(sleep 60) >&1 &\n",
        )
        .expect("script");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
        let discovery = discover_models_for_argv0(path.to_str().expect("utf8 path")).await;
        assert_eq!(
            discovery,
            ModelDiscovery::Failed {
                reason: "timed out".to_owned()
            }
        );
    }

    #[tokio::test]
    async fn discover_nonzero_exit_drains_stderr_within_deadline() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("pi");
        fs::write(
            &path,
            "#!/bin/sh\n(sleep 60) >&2 &\necho 'invalid model id' >&2\nexit 1\n",
        )
        .expect("script");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
        let discovery = discover_models_for_argv0(path.to_str().expect("utf8 path")).await;
        assert_eq!(
            discovery,
            ModelDiscovery::Failed {
                reason: "invalid model id".to_owned()
            }
        );
    }

    #[tokio::test]
    async fn discover_overflow_output_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("opencode");
        fs::write(&path, "#!/bin/sh\nyes openai/gpt-4.1 | head -c 300000\n").expect("script");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
        let discovery = discover_models_for_argv0(path.to_str().expect("utf8 path")).await;
        assert_eq!(
            discovery,
            ModelDiscovery::Failed {
                reason: "model list output was truncated".to_owned()
            }
        );
    }

    #[tokio::test]
    async fn discover_missing_executable_reports_not_installed() {
        let discovery = discover_models_for_argv0("/definitely/missing/pi").await;
        assert_eq!(
            discovery,
            ModelDiscovery::Failed {
                reason: "not installed".to_owned()
            }
        );
    }
}
