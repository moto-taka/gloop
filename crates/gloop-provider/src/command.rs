use std::{env, process::Stdio, time::Duration};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    task::JoinHandle,
    time,
};
use tokio_util::sync::CancellationToken;

use crate::{
    adapter::{
        AdapterCapabilities, AdapterError, AdapterEvent, AdapterEventKind, AdapterEventSender,
        AdapterOutput, AdapterRequest, AdapterResponse, MAX_REPORTED_MODEL_BYTES, OutputFormat,
        ProviderAdapter, TokenUsage, emit, validate_request_limits,
    },
    config::{CommandProfile, CommandPromptMode},
};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
#[cfg(windows)]
pub(crate) const COMMAND_ENVIRONMENT_ALLOWLIST: &[&str] = &[
    "COMSPEC",
    "PATH",
    "SYSTEMROOT",
    "TEMP",
    "TMP",
    "USERPROFILE",
    "USERNAME",
];
#[cfg(not(windows))]
pub(crate) const COMMAND_ENVIRONMENT_ALLOWLIST: &[&str] =
    &["HOME", "LANG", "PATH", "TMP", "TMPDIR", "USER"];

enum WaitOutcome {
    Finished(std::io::Result<std::process::ExitStatus>),
    Cancelled,
    TimedOut,
}

#[derive(Debug, Clone)]
pub(crate) struct CommandAdapter {
    name: String,
    timeout: Option<Duration>,
    capabilities: AdapterCapabilities,
    command: CommandProfile,
}

impl CommandAdapter {
    pub(crate) fn new(
        name: impl Into<String>,
        timeout: Option<Duration>,
        capabilities: AdapterCapabilities,
        command: CommandProfile,
    ) -> Self {
        Self {
            name: name.into(),
            timeout,
            capabilities,
            command,
        }
    }

    fn render_command(
        &self,
        request: &AdapterRequest,
    ) -> Result<(String, Vec<String>, Option<String>), AdapterError> {
        let executable = self
            .command
            .argv
            .first()
            .expect("validated command has an executable")
            .clone();
        let mut arguments = self
            .command
            .argv
            .iter()
            .skip(1)
            .cloned()
            .collect::<Vec<_>>();

        if let Some(model) = &request.model {
            if self.command.model_args.is_empty() {
                return Err(AdapterError::InvalidRequest {
                    profile: self.name.clone(),
                    message: "this command profile does not define model_args".to_owned(),
                });
            }
            arguments.extend(render_arguments(
                &self.command.model_args,
                "{model}",
                model,
                &self.name,
                "model_args",
            )?);
        }

        let mut prompt = request.prompt.clone();
        if let Some(system_prompt) = &request.system_prompt {
            if self.command.system_prompt_args.is_empty() {
                prompt = format!("{system_prompt}\n\n{prompt}");
            } else {
                arguments.extend(render_arguments(
                    &self.command.system_prompt_args,
                    "{system_prompt}",
                    system_prompt,
                    &self.name,
                    "system_prompt_args",
                )?);
            }
        }

        let stdin = match self.command.prompt_mode {
            CommandPromptMode::Argument => {
                arguments.extend(render_arguments(
                    &self.command.prompt_args,
                    "{prompt}",
                    &prompt,
                    &self.name,
                    "prompt_args",
                )?);
                None
            }
            CommandPromptMode::Stdin => {
                arguments.extend(self.command.prompt_args.iter().cloned());
                Some(prompt)
            }
        };
        Ok((executable, arguments, stdin))
    }
}

pub(crate) fn apply_isolated_environment(command: &mut Command) {
    command.env_clear();
    for variable in COMMAND_ENVIRONMENT_ALLOWLIST {
        if let Ok(value) = env::var(variable)
            && !value.is_empty()
        {
            command.env(variable, value);
        }
    }
}

#[async_trait]
impl ProviderAdapter for CommandAdapter {
    fn profile_name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> &AdapterCapabilities {
        &self.capabilities
    }

    #[allow(clippy::too_many_lines)]
    async fn execute(
        &self,
        request: AdapterRequest,
        cancellation: CancellationToken,
        events: Option<AdapterEventSender>,
    ) -> Result<AdapterResponse, AdapterError> {
        if cancellation.is_cancelled() {
            return Err(AdapterError::Cancelled {
                profile: self.name.clone(),
            });
        }
        let required = request.required_capabilities();
        if !self.capabilities.supports(&required) {
            return Err(AdapterError::CapabilityMismatch {
                profile: self.name.clone(),
                missing: self.capabilities.missing(&required),
            });
        }
        validate_request_limits(&self.name, &request)?;

        let (executable, arguments, stdin) = self.render_command(&request)?;
        let mut command = Command::new(&executable);
        #[cfg(unix)]
        command.process_group(0);
        apply_isolated_environment(&mut command);
        command
            .args(&arguments)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(directory) = &request.working_directory {
            command.current_dir(directory);
        }
        let mut redactions = Vec::new();
        for (target, source) in &self.command.env_from {
            let value = std::env::var(source).map_err(|_| AdapterError::MissingCredential {
                profile: self.name.clone(),
                env_var: source.clone(),
            })?;
            command.env(target, &value);
            if !value.is_empty() {
                redactions.push(value);
            }
        }

        let mut child = command.spawn().map_err(|source| AdapterError::Spawn {
            executable: executable.clone(),
            source,
        })?;
        let process_group = child.id();
        emit(
            events.as_ref(),
            AdapterEvent::data(AdapterEventKind::Started, json!({ "profile": self.name })),
        );

        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");
        let stream_events = if redactions.is_empty() {
            events.clone()
        } else {
            None
        };
        let (overflow_sender, mut overflow_receiver) =
            tokio::sync::mpsc::unbounded_channel::<&'static str>();
        let stdout_task = tokio::spawn(collect_stream(
            stdout,
            request.max_output_bytes,
            stream_events.clone(),
            AdapterEventKind::OutputDelta,
            overflow_sender.clone(),
            "stdout",
        ));
        let stderr_task = tokio::spawn(collect_stream(
            stderr,
            request.max_output_bytes,
            stream_events,
            AdapterEventKind::Diagnostic,
            overflow_sender.clone(),
            "stderr",
        ));
        let mut overflowed_stream = None;

        let stdin_task = stdin.map(|input| {
            let mut child_stdin = child.stdin.take().expect("stdin was piped");
            tokio::spawn(async move {
                child_stdin.write_all(input.as_bytes()).await?;
                child_stdin.shutdown().await
            })
        });

        let timeout = request.timeout.or(self.timeout).unwrap_or(DEFAULT_TIMEOUT);
        let outcome = {
            let wait = child.wait();
            tokio::pin!(wait);
            tokio::select! {
                biased;
                () = cancellation.cancelled() => WaitOutcome::Cancelled,
                result = &mut wait => WaitOutcome::Finished(result),
                overflowed = overflow_receiver.recv() => {
                    overflowed_stream = overflowed;
                    WaitOutcome::Cancelled
                }
                () = tokio::time::sleep(timeout) => WaitOutcome::TimedOut,
            }
        };

        let status = match outcome {
            WaitOutcome::Finished(result) => result.map_err(|source| {
                AdapterError::command_io(self.name.clone(), "waiting for process", source)
            })?,
            WaitOutcome::Cancelled if overflowed_stream.is_some() => {
                terminate(&mut child, process_group).await;
                abort_background(stdin_task.as_ref(), &stdout_task, &stderr_task);
                return Err(AdapterError::OutputTooLarge {
                    profile: self.name.clone(),
                    stream: overflowed_stream.expect("overflow stream is present"),
                    limit: request.max_output_bytes,
                });
            }
            WaitOutcome::Cancelled => {
                terminate(&mut child, process_group).await;
                abort_background(stdin_task.as_ref(), &stdout_task, &stderr_task);
                return Err(AdapterError::Cancelled {
                    profile: self.name.clone(),
                });
            }
            WaitOutcome::TimedOut => {
                terminate(&mut child, process_group).await;
                abort_background(stdin_task.as_ref(), &stdout_task, &stderr_task);
                return Err(AdapterError::command_timeout(
                    self.name.clone(),
                    duration_millis(timeout),
                ));
            }
        };

        let drain_budget = request.timeout.or(self.timeout).unwrap_or(DEFAULT_TIMEOUT);
        let timeout_result = time::timeout(drain_budget, async {
            tokio::join!(
                await_stdin(stdin_task, &self.name),
                await_reader(stdout_task, &self.name, "reading stdout"),
                await_reader(stderr_task, &self.name, "reading stderr")
            )
        })
        .await;
        let Ok((stdin_result, stdout, stderr)) = timeout_result else {
            terminate(&mut child, process_group).await;
            return Err(AdapterError::command_timeout(
                self.name.clone(),
                duration_millis(drain_budget),
            ));
        };
        let stdin_error = stdin_result.err();
        let stdout = stdout?;
        let stderr = stderr?;
        if stdout.overflow {
            return Err(AdapterError::OutputTooLarge {
                profile: self.name.clone(),
                stream: "stdout",
                limit: request.max_output_bytes,
            });
        }
        if stderr.overflow {
            return Err(AdapterError::OutputTooLarge {
                profile: self.name.clone(),
                stream: "stderr",
                limit: request.max_output_bytes,
            });
        }
        if stdin_error
            .as_ref()
            .is_some_and(|error| !is_benign_stdin_error(error))
        {
            return Err(stdin_error.expect("known to be present"));
        }

        let stdout =
            String::from_utf8(stdout.bytes).map_err(|error| AdapterError::InvalidOutput {
                profile: self.name.clone(),
                format: self.command.output,
                message: format!("stdout is not UTF-8: {error}"),
            })?;
        let stderr =
            String::from_utf8(stderr.bytes).map_err(|error| AdapterError::InvalidOutput {
                profile: self.name.clone(),
                format: OutputFormat::Text,
                message: format!("stderr is not UTF-8: {error}"),
            })?;
        let stdout = redact(&stdout, &redactions);
        let stderr = redact(&stderr, &redactions);

        if !status.success() {
            return Err(AdapterError::ProcessFailed {
                profile: self.name.clone(),
                executable,
                code: status.code(),
                stdout,
                stderr,
            });
        }

        let output = parse_output(
            &self.name,
            &stdout,
            self.command.output,
            self.command.output_pointer.as_deref(),
            request.output_format,
        )?;
        let (usage, reported_model) = extract_metadata(&self.name, &stdout, self.command.output)?;
        if let Some(usage) = &usage {
            emit(
                events.as_ref(),
                AdapterEvent::data(
                    AdapterEventKind::Usage,
                    serde_json::to_value(usage).expect("token usage serializes"),
                ),
            );
        }
        emit(
            events.as_ref(),
            AdapterEvent::data(
                AdapterEventKind::Finished,
                json!({ "profile": self.name, "exit_code": status.code() }),
            ),
        );

        Ok(AdapterResponse {
            output,
            stdout,
            stderr,
            exit_code: status.code(),
            reported_model,
            usage,
        })
    }
}

fn render_arguments(
    template: &[String],
    placeholder: &str,
    value: &str,
    profile: &str,
    field: &str,
) -> Result<Vec<String>, AdapterError> {
    if !template
        .iter()
        .any(|argument| argument.contains(placeholder))
    {
        return Err(AdapterError::InvalidRequest {
            profile: profile.to_owned(),
            message: format!("{field} must contain placeholder {placeholder:?}"),
        });
    }
    Ok(template
        .iter()
        .map(|argument| argument.replace(placeholder, value))
        .collect())
}

#[derive(Debug)]
struct CollectedStream {
    bytes: Vec<u8>,
    overflow: bool,
}

async fn collect_stream(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
    events: Option<AdapterEventSender>,
    kind: AdapterEventKind,
    overflowed: tokio::sync::mpsc::UnboundedSender<&'static str>,
    stream: &'static str,
) -> std::io::Result<CollectedStream> {
    let mut collected = Vec::with_capacity(limit.min(16 * 1024));
    let mut overflow = false;
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let chunk = &buffer[..read];
        let remaining = limit.saturating_sub(collected.len());
        let retained = remaining.min(read);
        if retained > 0 {
            emit(
                events.as_ref(),
                AdapterEvent::message(kind, String::from_utf8_lossy(&chunk[..retained])),
            );
        }
        collected.extend_from_slice(&chunk[..retained]);
        overflow |= retained < read;
        if overflow {
            let _ = overflowed.send(stream);
            break;
        }
    }
    Ok(CollectedStream {
        bytes: collected,
        overflow,
    })
}

async fn await_reader(
    task: JoinHandle<std::io::Result<CollectedStream>>,
    profile: &str,
    operation: &'static str,
) -> Result<CollectedStream, AdapterError> {
    task.await
        .map_err(|error| AdapterError::InvalidOutput {
            profile: profile.to_owned(),
            format: OutputFormat::Text,
            message: format!("output reader task failed: {error}"),
        })?
        .map_err(|source| AdapterError::command_io(profile.to_owned(), operation, source))
}

async fn await_stdin(
    task: Option<JoinHandle<std::io::Result<()>>>,
    profile: &str,
) -> Result<(), AdapterError> {
    let Some(task) = task else {
        return Ok(());
    };
    task.await
        .map_err(|error| AdapterError::InvalidOutput {
            profile: profile.to_owned(),
            format: OutputFormat::Text,
            message: format!("stdin writer task failed: {error}"),
        })?
        .map_err(|source| {
            AdapterError::command_io(profile.to_owned(), "writing command stdin", source)
        })
}

fn abort_background(
    stdin: Option<&JoinHandle<std::io::Result<()>>>,
    stdout: &JoinHandle<std::io::Result<CollectedStream>>,
    stderr: &JoinHandle<std::io::Result<CollectedStream>>,
) {
    if let Some(stdin) = stdin {
        stdin.abort();
    }
    stdout.abort();
    stderr.abort();
}

async fn terminate(child: &mut tokio::process::Child, process_group: Option<u32>) {
    #[cfg(unix)]
    {
        if let Some(pgid) = process_group {
            let pgid = format!("-{pgid}");
            let _ = tokio::process::Command::new("/bin/kill")
                .arg("-TERM")
                .arg(&pgid)
                .status()
                .await;
            let _ = tokio::process::Command::new("/bin/kill")
                .arg("-KILL")
                .arg(&pgid)
                .status()
                .await;
        }
    }
    let _kill_result = child.kill().await;
    let _wait_result = child.wait().await;
}

fn parse_output(
    profile: &str,
    source: &str,
    raw_format: OutputFormat,
    pointer: Option<&str>,
    requested_format: OutputFormat,
) -> Result<AdapterOutput, AdapterError> {
    let values = match raw_format {
        OutputFormat::Text => {
            return parse_text_output(profile, source, requested_format);
        }
        OutputFormat::Json => vec![parse_json(profile, source, OutputFormat::Json)?],
        OutputFormat::JsonLines => parse_json_lines(profile, source)?,
    };

    if requested_format == OutputFormat::JsonLines {
        return Ok(AdapterOutput::JsonLines(values));
    }

    let selected = if let Some(pointer) = pointer {
        let selected = values
            .iter()
            .filter_map(|value| value.pointer(pointer).cloned())
            .collect::<Vec<_>>();
        if selected.is_empty() {
            return Err(AdapterError::InvalidOutput {
                profile: profile.to_owned(),
                format: raw_format,
                message: format!("no output value matched JSON pointer {pointer:?}"),
            });
        }
        selected
    } else {
        values
    };

    match requested_format {
        OutputFormat::Text => {
            let text = if pointer.is_some() {
                values_to_text(&selected[selected.len() - 1..])
            } else {
                values_to_text(&selected)
            };
            if text.trim().is_empty() {
                return Err(AdapterError::InvalidOutput {
                    profile: profile.to_owned(),
                    format: raw_format,
                    message: "response text is empty".to_owned(),
                });
            }
            Ok(AdapterOutput::Text(text))
        }
        OutputFormat::Json => {
            let value = selected
                .last()
                .expect("selected output is non-empty")
                .clone();
            if let Value::String(text) = value {
                Ok(AdapterOutput::Json(parse_json(
                    profile,
                    &text,
                    OutputFormat::Json,
                )?))
            } else {
                Ok(AdapterOutput::Json(value))
            }
        }
        OutputFormat::JsonLines => unreachable!("handled above"),
    }
}

fn parse_text_output(
    profile: &str,
    source: &str,
    requested_format: OutputFormat,
) -> Result<AdapterOutput, AdapterError> {
    let trimmed = source.trim_end_matches(['\r', '\n']);
    if requested_format == OutputFormat::Text && trimmed.trim().is_empty() {
        return Err(AdapterError::InvalidOutput {
            profile: profile.to_owned(),
            format: OutputFormat::Text,
            message: "response text is empty".to_owned(),
        });
    }
    match requested_format {
        OutputFormat::Text => Ok(AdapterOutput::Text(trimmed.to_owned())),
        OutputFormat::Json => Ok(AdapterOutput::Json(parse_json(
            profile,
            trimmed,
            OutputFormat::Json,
        )?)),
        OutputFormat::JsonLines => Ok(AdapterOutput::JsonLines(parse_json_lines(
            profile, trimmed,
        )?)),
    }
}

fn parse_json(profile: &str, source: &str, format: OutputFormat) -> Result<Value, AdapterError> {
    serde_json::from_str(source).map_err(|error| AdapterError::InvalidOutput {
        profile: profile.to_owned(),
        format,
        message: error.to_string(),
    })
}

fn parse_json_lines(profile: &str, source: &str) -> Result<Vec<Value>, AdapterError> {
    let mut values = Vec::new();
    for (index, line) in source.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        values.push(
            serde_json::from_str(line).map_err(|error| AdapterError::InvalidOutput {
                profile: profile.to_owned(),
                format: OutputFormat::JsonLines,
                message: format!("line {}: {error}", index + 1),
            })?,
        );
    }
    if values.is_empty() {
        return Err(AdapterError::InvalidOutput {
            profile: profile.to_owned(),
            format: OutputFormat::JsonLines,
            message: "output did not contain any JSON values".to_owned(),
        });
    }
    Ok(values)
}

fn values_to_text(values: &[Value]) -> String {
    values
        .iter()
        .map(|value| match value {
            Value::String(text) => text.clone(),
            value => value.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn extract_metadata(
    profile: &str,
    source: &str,
    format: OutputFormat,
) -> Result<(Option<TokenUsage>, Option<String>), AdapterError> {
    let values: Vec<Value> = match format {
        OutputFormat::Text => return Ok((None, None)),
        OutputFormat::Json => serde_json::from_str(source).into_iter().collect(),
        OutputFormat::JsonLines => source
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect(),
    };
    let usage = values.iter().rev().find_map(|value| {
        let input_tokens = first_u64(
            value,
            &[
                "/usage/input_tokens",
                "/usage/prompt_tokens",
                "/message/usage/input_tokens",
            ],
        );
        let output_tokens = first_u64(
            value,
            &[
                "/usage/output_tokens",
                "/usage/completion_tokens",
                "/message/usage/output_tokens",
            ],
        );
        (input_tokens.is_some() || output_tokens.is_some()).then_some(TokenUsage {
            input_tokens,
            output_tokens,
        })
    });
    let model = extract_reported_model(&values).map_err(|message| AdapterError::InvalidOutput {
        profile: profile.to_owned(),
        format,
        message,
    })?;
    Ok((usage, model))
}

fn extract_reported_model(values: &[Value]) -> Result<Option<String>, String> {
    for value in values.iter().rev() {
        for pointer in ["/model", "/message/model", "/item/model"] {
            let Some(model) = value.pointer(pointer) else {
                continue;
            };
            let Some(model) = model.as_str() else {
                return Err(format!(
                    "reported model metadata at {pointer:?} must be a string"
                ));
            };
            let model = model.trim();
            if model.is_empty() {
                return Err(format!(
                    "reported model metadata at {pointer:?} must not be blank"
                ));
            }
            if model.len() > MAX_REPORTED_MODEL_BYTES || model.chars().any(char::is_control) {
                return Err(format!(
                    "reported model metadata at {pointer:?} must be control-free and at most {MAX_REPORTED_MODEL_BYTES} bytes"
                ));
            }
            return Ok(Some(model.to_owned()));
        }
    }
    Ok(None)
}

fn first_u64(value: &Value, pointers: &[&str]) -> Option<u64> {
    pointers
        .iter()
        .find_map(|pointer| value.pointer(pointer).and_then(Value::as_u64))
}

fn redact(value: &str, secrets: &[String]) -> String {
    secrets.iter().fold(value.to_owned(), |redacted, secret| {
        redacted.replace(secret, "[REDACTED]")
    })
}

fn is_benign_stdin_error(error: &AdapterError) -> bool {
    if let AdapterError::Io {
        operation, source, ..
    } = error
    {
        *operation == "writing command stdin" && source.kind() == std::io::ErrorKind::BrokenPipe
    } else {
        false
    }
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::adapter::AdapterCapability;
    use crate::config::{ProfileKind, ProfileStore};

    fn adapter(command: CommandProfile, capabilities: AdapterCapabilities) -> CommandAdapter {
        CommandAdapter::new("test", None, capabilities, command)
    }

    #[test]
    fn builtins_render_verified_headless_argv() {
        let cases = [
            (
                "codex",
                "codex",
                vec![
                    "exec",
                    "--json",
                    "--ephemeral",
                    "--model",
                    "test-model",
                    "-",
                ],
                Some("do work"),
            ),
            (
                "claude",
                "claude",
                vec![
                    "--print",
                    "--verbose",
                    "--output-format",
                    "stream-json",
                    "--model",
                    "test-model",
                ],
                Some("do work"),
            ),
            (
                "qwen",
                "qwen",
                vec![
                    "--output-format",
                    "stream-json",
                    "--model",
                    "test-model",
                    "--prompt=do work",
                ],
                None,
            ),
            (
                "cursor-agent",
                "cursor-agent",
                vec![
                    "-p",
                    "--output-format",
                    "stream-json",
                    "--model",
                    "test-model",
                    "--",
                    "do work",
                ],
                None,
            ),
            (
                "pi",
                "pi",
                vec![
                    "--print",
                    "--mode",
                    "json",
                    "--no-session",
                    "--model",
                    "test-model",
                    "--",
                    "do work",
                ],
                None,
            ),
            (
                "opencode",
                "opencode",
                vec![
                    "run",
                    "--format",
                    "json",
                    "--model",
                    "test-model",
                    "--",
                    "do work",
                ],
                None,
            ),
        ];
        let store = ProfileStore::builtins();
        for (name, expected_executable, expected_arguments, expected_stdin) in cases {
            let profile = store.get(name).expect("built-in profile exists");
            let ProfileKind::Command(command) = &profile.kind else {
                panic!("built-in must be a command");
            };
            let adapter =
                CommandAdapter::new(name, None, profile.capabilities.clone(), command.clone());
            let mut request = AdapterRequest::new("do work");
            request.model = Some("test-model".to_owned());
            let (executable, arguments, stdin) =
                adapter.render_command(&request).expect("render command");
            assert_eq!(executable, expected_executable, "{name}");
            assert_eq!(arguments, expected_arguments, "{name}");
            assert_eq!(stdin.as_deref(), expected_stdin, "{name}");
        }
    }

    #[tokio::test]
    async fn passes_prompt_as_one_argument_without_implicit_shell() {
        let mut command = CommandProfile::new(vec!["echo".to_owned()]);
        command.prompt_mode = CommandPromptMode::Argument;
        command.prompt_args = vec!["{prompt}".to_owned()];
        let adapter = adapter(command, AdapterCapabilities::text());
        let prompt = "$(printf injected); *";
        let response = adapter
            .execute(AdapterRequest::new(prompt), CancellationToken::new(), None)
            .await
            .expect("command succeeds");
        assert_eq!(response.output.as_text(), Some(prompt));
    }

    #[tokio::test]
    async fn extracts_text_from_json_lines_with_pointer() {
        let mut command = CommandProfile::new(vec![
            "printf".to_owned(),
            "%s".to_owned(),
            "{\"type\":\"start\"}\n{\"result\":\"done\",\"model\":\"test-model\",\"usage\":{\"input_tokens\":2,\"output_tokens\":3}}\n".to_owned(),
        ]);
        command.prompt_mode = CommandPromptMode::Stdin;
        command.output = OutputFormat::JsonLines;
        command.output_pointer = Some("/result".to_owned());
        let adapter = adapter(command, AdapterCapabilities::text());
        let response = adapter
            .execute(AdapterRequest::new(""), CancellationToken::new(), None)
            .await
            .expect("command succeeds");
        assert_eq!(response.output.as_text(), Some("done"));
        assert_eq!(response.reported_model.as_deref(), Some("test-model"));
        assert_eq!(
            response.usage.expect("usage is extracted"),
            TokenUsage {
                input_tokens: Some(2),
                output_tokens: Some(3),
            }
        );
    }

    #[tokio::test]
    async fn preserves_json_lines_when_requested() {
        let mut command = CommandProfile::new(vec![
            "printf".to_owned(),
            "%s".to_owned(),
            "{\"sequence\":1}\n{\"sequence\":2}\n".to_owned(),
        ]);
        command.prompt_mode = CommandPromptMode::Stdin;
        command.output = OutputFormat::JsonLines;
        let adapter = adapter(
            command,
            AdapterCapabilities::new([AdapterCapability::JsonLinesOutput]),
        );
        let mut request = AdapterRequest::new("");
        request.output_format = OutputFormat::JsonLines;
        let response = adapter
            .execute(request, CancellationToken::new(), None)
            .await
            .expect("JSONL command succeeds");
        assert_eq!(
            response.output,
            AdapterOutput::JsonLines(vec![json!({ "sequence": 1 }), json!({ "sequence": 2 })])
        );
    }

    #[test]
    fn pi_extraction_uses_final_matching_message() {
        let source = concat!(
            "{\"type\":\"message_end\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"input prompt\"}]}}\n",
            "{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"final answer\"}]}}\n"
        );
        let output = parse_output(
            "pi",
            source,
            OutputFormat::JsonLines,
            Some("/message/content/0/text"),
            OutputFormat::Text,
        )
        .expect("Pi output parses");
        assert_eq!(output, AdapterOutput::Text("final answer".to_owned()));
    }

    #[test]
    fn parse_output_rejects_blank_text() {
        let error = parse_output(
            "command-profile",
            "   \n",
            OutputFormat::Text,
            None,
            OutputFormat::Text,
        )
        .expect_err("blank output should be rejected");
        assert!(matches!(error, AdapterError::InvalidOutput { .. }));
    }

    #[tokio::test]
    async fn times_out_and_terminates_process() {
        let command = CommandProfile::new(vec![
            "sh".to_owned(),
            "-c".to_owned(),
            "sleep 5".to_owned(),
            "gloop-test".to_owned(),
        ]);
        let adapter = adapter(command, AdapterCapabilities::text());
        let mut request = AdapterRequest::new("ignored");
        request.timeout = Some(Duration::from_millis(30));
        let error = adapter
            .execute(request, CancellationToken::new(), None)
            .await
            .expect_err("command must time out");
        assert!(matches!(error, AdapterError::Timeout { .. }));
    }

    #[tokio::test]
    async fn timeout_errors_from_command_execution_are_not_retryable() {
        let command =
            CommandProfile::new(vec!["sh".to_owned(), "-c".to_owned(), "sleep 5".to_owned()]);
        let adapter = adapter(command, AdapterCapabilities::text());
        let mut request = AdapterRequest::new("ignored");
        request.timeout = Some(Duration::from_millis(30));
        let error = adapter
            .execute(request, CancellationToken::new(), None)
            .await
            .expect_err("command must time out");
        assert!(!error.is_retryable());
    }

    #[test]
    fn io_errors_from_command_adapter_are_not_retryable() {
        let error = AdapterError::command_io(
            "command-test",
            "waiting for process",
            std::io::Error::other("simulated command I/O failure"),
        );
        assert!(!error.is_retryable());
    }

    #[tokio::test]
    async fn timeout_bounded_when_child_exits_and_descendant_keeps_pipe_open() {
        let command = CommandProfile::new(vec![
            "sh".to_owned(),
            "-c".to_owned(),
            "sleep 1 &".to_owned(),
        ]);
        let adapter = adapter(command, AdapterCapabilities::text());
        let mut request = AdapterRequest::new("ignored");
        request.timeout = Some(Duration::from_millis(20));
        let error = adapter
            .execute(request, CancellationToken::new(), None)
            .await
            .expect_err("drain must be bounded");
        assert!(matches!(error, AdapterError::Timeout { .. }));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn descendant_process_is_killed_when_reader_drain_times_out() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let marker = directory.path().join("descendant_pid");
        let marker_path = marker.to_string_lossy().into_owned();
        let command = CommandProfile::new(vec![
            "sh".to_owned(),
            "-c".to_owned(),
            format!("(sleep 2 &) ; echo $! > {marker_path}"),
        ]);
        let adapter = adapter(command, AdapterCapabilities::text());
        let mut request = AdapterRequest::new("ignored");
        request.timeout = Some(Duration::from_millis(20));
        let error = adapter
            .execute(request, CancellationToken::new(), None)
            .await
            .expect_err("drain must terminate descendant process");
        assert!(matches!(error, AdapterError::Timeout { .. }));
        let pid = std::fs::read_to_string(marker).expect("descendant pid file");
        let status = tokio::process::Command::new("kill")
            .arg("-0")
            .arg(pid.trim())
            .status()
            .await
            .expect("kill -0 check succeeds");
        assert!(!status.success());
    }

    #[tokio::test]
    async fn output_too_large_wins_over_broken_stdin_pipe() {
        let mut command = CommandProfile::new(vec![
            "awk".to_owned(),
            "BEGIN { for(i = 0; i < 4096; i++) printf \"A\" }".to_owned(),
        ]);
        command.prompt_mode = CommandPromptMode::Stdin;
        let adapter = adapter(command, AdapterCapabilities::text());
        let mut request = AdapterRequest::new("x".repeat(64 * 1024));
        request.max_output_bytes = 1024;
        let error = adapter
            .execute(request, CancellationToken::new(), None)
            .await
            .expect_err("large command output should be rejected");
        assert!(matches!(
            error,
            AdapterError::OutputTooLarge {
                stream: "stdout",
                ..
            }
        ));
    }

    #[tokio::test]
    async fn infinite_output_exceeds_limit_prompts_early_termination() {
        let command = CommandProfile::new(vec![
            "sh".to_owned(),
            "-c".to_owned(),
            "while true; do echo data; sleep 0.001; done".to_owned(),
        ]);
        let adapter = adapter(command, AdapterCapabilities::text());
        let mut request = AdapterRequest::new("");
        request.max_output_bytes = 64;
        request.timeout = Some(Duration::from_secs(2));
        let start = std::time::Instant::now();
        let error = adapter
            .execute(request, CancellationToken::new(), None)
            .await
            .expect_err("limit should be exceeded and terminate");
        let elapsed = start.elapsed();
        assert!(matches!(
            error,
            AdapterError::OutputTooLarge {
                stream: "stdout" | "stderr",
                ..
            }
        ));
        assert!(elapsed < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn timeout_also_bounds_blocked_stdin_delivery() {
        let mut command = CommandProfile::new(vec![
            "sh".to_owned(),
            "-c".to_owned(),
            "sleep 5".to_owned(),
            "gloop-test".to_owned(),
        ]);
        command.prompt_mode = CommandPromptMode::Stdin;
        let adapter = adapter(command, AdapterCapabilities::text());
        let mut request = AdapterRequest::new("x".repeat(256 * 1024));
        request.timeout = Some(Duration::from_millis(30));
        let error = adapter
            .execute(request, CancellationToken::new(), None)
            .await
            .expect_err("blocked stdin must share the process timeout");
        assert!(matches!(error, AdapterError::Timeout { .. }));
    }

    #[tokio::test]
    async fn cancellation_terminates_process() {
        let command = CommandProfile::new(vec![
            "sh".to_owned(),
            "-c".to_owned(),
            "sleep 5".to_owned(),
            "gloop-test".to_owned(),
        ]);
        let adapter = adapter(command, AdapterCapabilities::text());
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let error = adapter
            .execute(AdapterRequest::new("ignored"), cancellation, None)
            .await
            .expect_err("command must be cancelled");
        assert!(matches!(error, AdapterError::Cancelled { .. }));
    }

    #[tokio::test]
    async fn pre_cancelled_request_never_spawns_command() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let marker = directory.path().join("must-not-exist");
        let mut command = CommandProfile::new(vec![
            "touch".to_owned(),
            marker.to_string_lossy().into_owned(),
        ]);
        command.prompt_mode = CommandPromptMode::Stdin;
        let adapter = adapter(command, AdapterCapabilities::text());
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let error = adapter
            .execute(AdapterRequest::new("ignored"), cancellation, None)
            .await
            .expect_err("request must be cancelled before spawn");
        assert!(matches!(error, AdapterError::Cancelled { .. }));
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn rejects_output_over_limit_without_retaining_it() {
        let command = CommandProfile::new(vec![
            "printf".to_owned(),
            "%s".to_owned(),
            "123456789".to_owned(),
        ]);
        let adapter = adapter(command, AdapterCapabilities::text());
        let mut request = AdapterRequest::new("ignored");
        request.max_output_bytes = 4;
        let error = adapter
            .execute(request, CancellationToken::new(), None)
            .await
            .expect_err("oversize output must fail");
        assert!(matches!(error, AdapterError::OutputTooLarge { .. }));
    }

    #[tokio::test]
    async fn rejects_prompt_over_limit_before_spawn() {
        let command = CommandProfile::new(vec!["printf".to_owned(), "%s".to_owned()]);
        let adapter = adapter(command, AdapterCapabilities::text());
        let mut request = AdapterRequest::new("too long");
        request.max_prompt_bytes = 3;
        let error = adapter
            .execute(request, CancellationToken::new(), None)
            .await
            .expect_err("oversize prompt must fail");
        assert!(matches!(
            error,
            AdapterError::OutputTooLarge {
                stream: "prompt",
                ..
            }
        ));
    }

    #[tokio::test]
    async fn environment_values_are_redacted_from_output_and_events() {
        let mut command = CommandProfile::new(vec![
            "sh".to_owned(),
            "-c".to_owned(),
            "printf %s \"$GLOOP_REDACTION_TEST\"".to_owned(),
            "gloop-test".to_owned(),
        ]);
        command
            .env_from
            .insert("GLOOP_REDACTION_TEST".to_owned(), "PATH".to_owned());
        let adapter = adapter(command, AdapterCapabilities::text());
        let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let response = adapter
            .execute(
                AdapterRequest::new("ignored"),
                CancellationToken::new(),
                Some(events),
            )
            .await
            .expect("command succeeds");
        assert_eq!(response.output.as_text(), Some("[REDACTED]"));
        assert_eq!(response.stdout, "[REDACTED]");
        let events = std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>();
        assert!(!events.iter().any(|event| {
            event.kind == AdapterEventKind::OutputDelta
                || event.kind == AdapterEventKind::Diagnostic
        }));
    }

    #[tokio::test]
    async fn failing_process_retains_only_redacted_streams() {
        let secret = std::env::var("PATH").expect("PATH must be set for the test");
        assert!(!secret.is_empty());
        let mut command = CommandProfile::new(vec![
            "sh".to_owned(),
            "-c".to_owned(),
            "printf %s \"$GLOOP_REDACTION_TEST\"; printf %s \"$GLOOP_REDACTION_TEST\" >&2; exit 7"
                .to_owned(),
            "gloop-test".to_owned(),
        ]);
        command
            .env_from
            .insert("GLOOP_REDACTION_TEST".to_owned(), "PATH".to_owned());
        let adapter = adapter(command, AdapterCapabilities::text());
        let error = adapter
            .execute(
                AdapterRequest::new("ignored"),
                CancellationToken::new(),
                None,
            )
            .await
            .expect_err("command must fail");

        match &error {
            AdapterError::ProcessFailed {
                profile,
                executable,
                code,
                stdout,
                stderr,
            } => {
                assert_eq!(profile, "test");
                assert_eq!(executable, "sh");
                assert_eq!(*code, Some(7));
                assert_eq!(stdout, "[REDACTED]");
                assert_eq!(stderr, "[REDACTED]");
            }
            _ => panic!("expected process failure"),
        }
        let display = error.to_string();
        assert!(!display.contains(&secret));
        assert!(!display.contains("[REDACTED]"));
        assert!(!format!("{error:?}").contains(&secret));
    }

    #[tokio::test]
    async fn executes_without_inheriting_arbitrary_parent_environment() {
        if std::env::var_os("GLOOP_PROVIDER_TEST_CHILD_ENV").is_none() {
            let test_binary = std::env::current_exe().expect("current test binary");
            let result = tokio::process::Command::new(test_binary)
                .arg("executes_without_inheriting_arbitrary_parent_environment_child")
                .arg("--nocapture")
                .env("GLOOP_PROVIDER_TEST_CHILD_ENV", "1")
                .env("GLOOP_TEST_CHILD_NOT_LEAKED_0x7F2E", "1")
                .output()
                .await
                .expect("failed to spawn nested test");
            assert!(result.status.success());
            assert!(
                String::from_utf8_lossy(&result.stdout).contains("ENVIRONMENT_ISOLATION_PROBE_OK"),
                "nested test marker missing from child output"
            );
        }
    }

    #[tokio::test]
    async fn executes_without_inheriting_arbitrary_parent_environment_child() {
        let command = CommandProfile::new(vec![
            "sh".to_owned(),
            "-c".to_owned(),
            "printf %s \"ok${GLOOP_TEST_CHILD_NOT_LEAKED_0x7F2E:+_blocked}\"".to_owned(),
        ]);
        let adapter = adapter(command, AdapterCapabilities::text());
        let response = adapter
            .execute(AdapterRequest::new(""), CancellationToken::new(), None)
            .await
            .expect("command succeeds");
        assert_eq!(response.output.as_text(), Some("ok"));
        println!("ENVIRONMENT_ISOLATION_PROBE_OK");
    }

    #[tokio::test]
    async fn reported_model_reports_only_provider_value() {
        let mut command = CommandProfile::new(vec!["cat".to_owned()]);
        command.prompt_mode = CommandPromptMode::Stdin;
        let capabilities = AdapterCapabilities::new([
            crate::adapter::AdapterCapability::TextOutput,
            crate::adapter::AdapterCapability::JsonLinesOutput,
            crate::adapter::AdapterCapability::ModelSelection,
        ]);
        command.output = OutputFormat::JsonLines;
        command.output_pointer = Some("/result".to_owned());
        let adapter = adapter(command, capabilities);
        let response = adapter
            .execute(
                AdapterRequest::new("{\"result\":\"done\"}"),
                CancellationToken::new(),
                None,
            )
            .await
            .expect("command succeeds");
        assert_eq!(response.output.as_text(), Some("done"));
        assert_eq!(response.reported_model, None);
    }

    #[tokio::test]
    async fn reported_model_must_be_string_and_non_empty() {
        let mut command = CommandProfile::new(vec![
            "printf".to_owned(),
            "%s".to_owned(),
            "{\"result\":\"done\",\"model\":123}".to_owned(),
        ]);
        command.prompt_mode = CommandPromptMode::Stdin;
        command.output = OutputFormat::Json;
        command.output_pointer = Some("/result".to_owned());
        let adapter = adapter(command, AdapterCapabilities::text());
        let error = adapter
            .execute(
                AdapterRequest::new("ignored"),
                CancellationToken::new(),
                None,
            )
            .await
            .expect_err("reported model should be rejected when malformed");
        assert!(matches!(error, AdapterError::InvalidOutput { .. }));

        assert!(
            extract_reported_model(&[json!({
                "model": "m".repeat(MAX_REPORTED_MODEL_BYTES + 1)
            })])
            .is_err()
        );
    }

    #[tokio::test]
    async fn execute_rejects_whitespace_only_command_text() {
        let command = CommandProfile::new(vec![
            "printf".to_owned(),
            "%s".to_owned(),
            "  \n".to_owned(),
        ]);
        let adapter = adapter(command, AdapterCapabilities::text());
        let error = adapter
            .execute(AdapterRequest::new(""), CancellationToken::new(), None)
            .await
            .expect_err("blank command output should fail");
        assert!(matches!(error, AdapterError::InvalidOutput { .. }));
    }
}
