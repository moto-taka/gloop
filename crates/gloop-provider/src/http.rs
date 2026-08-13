use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::{
    Client, RequestBuilder, StatusCode,
    header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue},
};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::{
    adapter::{
        AdapterCapabilities, AdapterError, AdapterEvent, AdapterEventKind, AdapterEventSender,
        AdapterOutput, AdapterRequest, AdapterResponse, MAX_REPORTED_MODEL_BYTES, OutputFormat,
        ProviderAdapter, TokenUsage, emit, validate_request_limits,
    },
    config::{AnthropicProfile, OpenAiProfile, SecretRef},
};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Practical upper bound (in bytes) for serialized outbound request JSON.
///
/// Provider profiles can include large arbitrary parameters. We cap the request
/// payload here to avoid serializing and transmitting unbounded request bodies
/// across many fan-out providers.
const MAX_SERIALIZED_REQUEST_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone)]
pub(crate) struct OpenAiAdapter {
    name: String,
    timeout: Option<Duration>,
    capabilities: AdapterCapabilities,
    profile: OpenAiProfile,
    client: Client,
}

impl OpenAiAdapter {
    pub(crate) fn new(
        name: impl Into<String>,
        timeout: Option<Duration>,
        capabilities: AdapterCapabilities,
        profile: OpenAiProfile,
        client: Client,
    ) -> Self {
        Self {
            name: name.into(),
            timeout,
            capabilities,
            profile,
            client,
        }
    }
}

#[async_trait]
impl ProviderAdapter for OpenAiAdapter {
    fn profile_name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> &AdapterCapabilities {
        &self.capabilities
    }

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
        validate_http_request(&self.name, &self.capabilities, &request)?;
        let model = request
            .model
            .clone()
            .unwrap_or_else(|| self.profile.model.clone());
        let endpoint = endpoint(&self.profile.base_url, "chat/completions", &self.name)?;
        let mut messages = Vec::new();
        if let Some(system) = &request.system_prompt {
            messages.push(json!({ "role": "system", "content": system }));
        }
        messages.push(json!({ "role": "user", "content": request.prompt }));

        let mut body = request_body_from_parameters(&self.profile.parameters);
        body.insert("model".to_owned(), Value::String(model));
        body.insert("messages".to_owned(), Value::Array(messages));
        body.insert("stream".to_owned(), Value::Bool(false));
        let body = serialize_request_body(&self.name, body, MAX_SERIALIZED_REQUEST_BYTES)?;

        let (mut headers, mut redactions) = secret_headers(&self.name, &self.profile.headers_from)?;
        if let Some(reference) = &self.profile.api_key_env {
            let credential = reference
                .resolve()
                .map_err(|_| missing_credential(&self.name, reference))?;
            let mut authorization = HeaderValue::from_str(&format!("Bearer {credential}"))
                .map_err(|_| AdapterError::InvalidRequest {
                    profile: self.name.clone(),
                    message: format!(
                        "environment variable {} is not a valid HTTP credential",
                        reference.env_var()
                    ),
                })?;
            authorization.set_sensitive(true);
            headers.insert(AUTHORIZATION, authorization);
            redactions.push(credential);
        }
        if let Some(organization_env) = &self.profile.organization_env {
            let organization =
                std::env::var(organization_env).map_err(|_| AdapterError::MissingCredential {
                    profile: self.name.clone(),
                    env_var: organization_env.clone(),
                })?;
            insert_sensitive_header(
                &self.name,
                &mut headers,
                "openai-organization",
                &organization,
            )?;
            redactions.push(organization);
        }
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        let builder = self.client.post(endpoint).headers(headers).body(body);
        let raw = send_json(
            &self.name,
            builder,
            request.timeout.or(self.timeout).unwrap_or(DEFAULT_TIMEOUT),
            request.max_output_bytes,
            &redactions,
            cancellation,
            events.as_ref(),
        )
        .await?;
        parse_openai_response(&self.name, &raw, request.output_format, events.as_ref())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct AnthropicAdapter {
    name: String,
    timeout: Option<Duration>,
    capabilities: AdapterCapabilities,
    profile: AnthropicProfile,
    client: Client,
}

impl AnthropicAdapter {
    pub(crate) fn new(
        name: impl Into<String>,
        timeout: Option<Duration>,
        capabilities: AdapterCapabilities,
        profile: AnthropicProfile,
        client: Client,
    ) -> Self {
        Self {
            name: name.into(),
            timeout,
            capabilities,
            profile,
            client,
        }
    }
}

#[async_trait]
impl ProviderAdapter for AnthropicAdapter {
    fn profile_name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> &AdapterCapabilities {
        &self.capabilities
    }

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
        validate_http_request(&self.name, &self.capabilities, &request)?;
        let model = request
            .model
            .clone()
            .unwrap_or_else(|| self.profile.model.clone());
        let endpoint = endpoint(&self.profile.base_url, "messages", &self.name)?;
        let mut body = request_body_from_parameters(&self.profile.parameters);
        body.insert("model".to_owned(), Value::String(model));
        body.insert(
            "max_tokens".to_owned(),
            Value::Number(self.profile.max_tokens.into()),
        );
        body.insert(
            "messages".to_owned(),
            json!([{ "role": "user", "content": request.prompt }]),
        );
        body.insert("stream".to_owned(), Value::Bool(false));
        if let Some(system) = &request.system_prompt {
            body.insert("system".to_owned(), Value::String(system.clone()));
        }
        let body = serialize_request_body(&self.name, body, MAX_SERIALIZED_REQUEST_BYTES)?;

        let (mut headers, mut redactions) = secret_headers(&self.name, &self.profile.headers_from)?;
        let credential = self
            .profile
            .api_key_env
            .resolve()
            .map_err(|_| missing_credential(&self.name, &self.profile.api_key_env))?;
        insert_sensitive_header(&self.name, &mut headers, "x-api-key", &credential)?;
        redactions.push(credential);
        insert_header(
            &self.name,
            &mut headers,
            "anthropic-version",
            &self.profile.api_version,
        )?;
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        let builder = self.client.post(endpoint).headers(headers).body(body);
        let raw = send_json(
            &self.name,
            builder,
            request.timeout.or(self.timeout).unwrap_or(DEFAULT_TIMEOUT),
            request.max_output_bytes,
            &redactions,
            cancellation,
            events.as_ref(),
        )
        .await?;
        parse_anthropic_response(&self.name, &raw, request.output_format, events.as_ref())
    }
}

fn validate_http_request(
    profile: &str,
    capabilities: &AdapterCapabilities,
    request: &AdapterRequest,
) -> Result<(), AdapterError> {
    validate_request_limits(profile, request)?;
    let required = request.required_capabilities();
    if !capabilities.supports(&required) {
        return Err(AdapterError::CapabilityMismatch {
            profile: profile.to_owned(),
            missing: capabilities.missing(&required),
        });
    }
    Ok(())
}

fn request_body_from_parameters(
    parameters: &indexmap::IndexMap<String, Value>,
) -> Map<String, Value> {
    let mut body = Map::with_capacity(parameters.len());
    for (name, value) in parameters {
        body.insert(name.clone(), value.clone());
    }
    body
}

fn serialize_request_body(
    profile: &str,
    body: Map<String, Value>,
    max_serialized_bytes: usize,
) -> Result<Vec<u8>, AdapterError> {
    let body = Value::Object(body);
    let serialized = serde_json::to_vec(&body).map_err(|source| AdapterError::InvalidRequest {
        profile: profile.to_owned(),
        message: format!("failed to serialize request payload: {source}"),
    })?;
    if serialized.len() > max_serialized_bytes {
        return Err(AdapterError::OutputTooLarge {
            profile: profile.to_owned(),
            stream: "HTTP request",
            limit: max_serialized_bytes,
        });
    }
    Ok(serialized)
}

async fn send_json(
    profile: &str,
    builder: RequestBuilder,
    timeout: Duration,
    max_output_bytes: usize,
    redactions: &[String],
    cancellation: CancellationToken,
    events: Option<&AdapterEventSender>,
) -> Result<Value, AdapterError> {
    emit(
        events,
        AdapterEvent::data(AdapterEventKind::Started, json!({ "profile": profile })),
    );
    let operation = async {
        let response = builder
            .send()
            .await
            .map_err(|source| AdapterError::HttpTransport {
                profile: profile.to_owned(),
                source,
            })?;
        let status = response.status();
        if response
            .content_length()
            .is_some_and(|length| length > u64::try_from(max_output_bytes).unwrap_or(u64::MAX))
        {
            return Err(AdapterError::OutputTooLarge {
                profile: profile.to_owned(),
                stream: "HTTP response",
                limit: max_output_bytes,
            });
        }
        let mut bytes = Vec::with_capacity(max_output_bytes.min(16 * 1024));
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|source| AdapterError::HttpTransport {
                profile: profile.to_owned(),
                source,
            })?;
            if bytes.len().saturating_add(chunk.len()) > max_output_bytes {
                return Err(AdapterError::OutputTooLarge {
                    profile: profile.to_owned(),
                    stream: "HTTP response",
                    limit: max_output_bytes,
                });
            }
            bytes.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            let mut value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            redact_json(&mut value, redactions);
            return Err(http_status_error(profile, status, &value));
        }
        let mut value: Value =
            serde_json::from_slice(&bytes).map_err(|error| AdapterError::InvalidOutput {
                profile: profile.to_owned(),
                format: OutputFormat::Json,
                message: error.to_string(),
            })?;
        redact_json(&mut value, redactions);
        Ok(value)
    };
    tokio::pin!(operation);
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(AdapterError::Cancelled {
            profile: profile.to_owned(),
        }),
        result = &mut operation => result,
        () = tokio::time::sleep(timeout) => {
            Err(AdapterError::transport_timeout(
                profile.to_owned(),
                duration_millis(timeout),
            ))
        }
    }
}

fn parse_openai_response(
    profile: &str,
    raw: &Value,
    format: OutputFormat,
    events: Option<&AdapterEventSender>,
) -> Result<AdapterResponse, AdapterError> {
    let content = raw
        .pointer("/choices/0/message/content")
        .ok_or_else(|| invalid_response(profile, "missing choices[0].message.content"))?;
    let text = content_text(content)
        .ok_or_else(|| invalid_response(profile, "assistant content does not contain text"))?;
    let text = validate_assistant_text(profile, text)?;
    let output = response_output(profile, &text, format)?;
    let usage = usage_from_pointers(raw, "/usage/prompt_tokens", "/usage/completion_tokens");
    let reported_model = parse_reported_model(profile, raw)?;
    emit_http_completion(events, profile, &text, usage.as_ref());
    Ok(AdapterResponse {
        output,
        stdout: String::new(),
        stderr: String::new(),
        exit_code: None,
        reported_model,
        reported_model_informational: false,
        usage,
    })
}

fn parse_anthropic_response(
    profile: &str,
    raw: &Value,
    format: OutputFormat,
    events: Option<&AdapterEventSender>,
) -> Result<AdapterResponse, AdapterError> {
    let content = raw
        .get("content")
        .ok_or_else(|| invalid_response(profile, "missing content"))?;
    let text = content_text(content)
        .ok_or_else(|| invalid_response(profile, "content does not contain text"))?;
    let text = validate_assistant_text(profile, text)?;
    let output = response_output(profile, &text, format)?;
    let usage = usage_from_pointers(raw, "/usage/input_tokens", "/usage/output_tokens");
    let reported_model = parse_reported_model(profile, raw)?;
    emit_http_completion(events, profile, &text, usage.as_ref());
    Ok(AdapterResponse {
        output,
        stdout: String::new(),
        stderr: String::new(),
        exit_code: None,
        reported_model,
        reported_model_informational: false,
        usage,
    })
}

fn response_output(
    profile: &str,
    text: &str,
    format: OutputFormat,
) -> Result<AdapterOutput, AdapterError> {
    match format {
        OutputFormat::Text => Ok(AdapterOutput::Text(text.to_owned())),
        OutputFormat::Json => {
            serde_json::from_str(text)
                .map(AdapterOutput::Json)
                .map_err(|error| AdapterError::InvalidOutput {
                    profile: profile.to_owned(),
                    format,
                    message: error.to_string(),
                })
        }
        OutputFormat::JsonLines => Err(AdapterError::CapabilityMismatch {
            profile: profile.to_owned(),
            missing: vec![crate::adapter::AdapterCapability::JsonLinesOutput],
        }),
    }
}

fn validate_assistant_text(profile: &str, text: String) -> Result<String, AdapterError> {
    if text.trim().is_empty() {
        return Err(invalid_response(profile, "assistant content is empty"));
    }
    Ok(text)
}

fn content_text(content: &Value) -> Option<String> {
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => {
            let text = parts
                .iter()
                .filter_map(|part| {
                    part.get("text")
                        .and_then(Value::as_str)
                        .or_else(|| part.get("content").and_then(Value::as_str))
                })
                .collect::<Vec<_>>()
                .join("");
            (!text.is_empty()).then_some(text)
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::Object(_) => None,
    }
}

fn usage_from_pointers(raw: &Value, input: &str, output: &str) -> Option<TokenUsage> {
    let usage = TokenUsage {
        input_tokens: raw.pointer(input).and_then(Value::as_u64),
        output_tokens: raw.pointer(output).and_then(Value::as_u64),
    };
    (usage.input_tokens.is_some() || usage.output_tokens.is_some()).then_some(usage)
}

fn emit_http_completion(
    events: Option<&AdapterEventSender>,
    profile: &str,
    text: &str,
    usage: Option<&TokenUsage>,
) {
    emit(
        events,
        AdapterEvent::message(AdapterEventKind::OutputDelta, text),
    );
    if let Some(usage) = usage {
        emit(
            events,
            AdapterEvent::data(
                AdapterEventKind::Usage,
                serde_json::to_value(usage).expect("token usage serializes"),
            ),
        );
    }
    emit(
        events,
        AdapterEvent::data(AdapterEventKind::Finished, json!({ "profile": profile })),
    );
}

fn endpoint(base: &Url, suffix: &str, profile: &str) -> Result<Url, AdapterError> {
    if base.path().trim_end_matches('/').ends_with(suffix) {
        return Ok(base.clone());
    }
    let mut normalized = base.clone();
    if !normalized.path().ends_with('/') {
        let path = format!("{}/", normalized.path());
        normalized.set_path(&path);
    }
    normalized
        .join(suffix)
        .map_err(|error| AdapterError::InvalidRequest {
            profile: profile.to_owned(),
            message: format!("invalid base_url: {error}"),
        })
}

fn secret_headers(
    profile: &str,
    headers_from: &indexmap::IndexMap<String, String>,
) -> Result<(HeaderMap, Vec<String>), AdapterError> {
    let mut headers = HeaderMap::new();
    let mut redactions = Vec::new();
    for (header, env_var) in headers_from {
        let value = std::env::var(env_var).map_err(|_| AdapterError::MissingCredential {
            profile: profile.to_owned(),
            env_var: env_var.clone(),
        })?;
        insert_sensitive_header(profile, &mut headers, header, &value)?;
        if !value.is_empty() {
            redactions.push(value);
        }
    }
    Ok((headers, redactions))
}

fn insert_sensitive_header(
    profile: &str,
    headers: &mut HeaderMap,
    name: &str,
    value: &str,
) -> Result<(), AdapterError> {
    let mut value = HeaderValue::from_str(value).map_err(|_| AdapterError::InvalidRequest {
        profile: profile.to_owned(),
        message: format!("environment value for header {name:?} is invalid"),
    })?;
    value.set_sensitive(true);
    insert_parsed_header(profile, headers, name, value)
}

fn insert_header(
    profile: &str,
    headers: &mut HeaderMap,
    name: &str,
    value: &str,
) -> Result<(), AdapterError> {
    let value = HeaderValue::from_str(value).map_err(|_| AdapterError::InvalidRequest {
        profile: profile.to_owned(),
        message: format!("header {name:?} has an invalid value"),
    })?;
    insert_parsed_header(profile, headers, name, value)
}

fn insert_parsed_header(
    profile: &str,
    headers: &mut HeaderMap,
    name: &str,
    value: HeaderValue,
) -> Result<(), AdapterError> {
    let name =
        HeaderName::from_bytes(name.as_bytes()).map_err(|_| AdapterError::InvalidRequest {
            profile: profile.to_owned(),
            message: format!("invalid HTTP header name {name:?}"),
        })?;
    headers.insert(name, value);
    Ok(())
}

fn missing_credential(profile: &str, reference: &SecretRef) -> AdapterError {
    AdapterError::MissingCredential {
        profile: profile.to_owned(),
        env_var: reference.env_var().to_owned(),
    }
}

fn invalid_response(profile: &str, message: &str) -> AdapterError {
    AdapterError::InvalidOutput {
        profile: profile.to_owned(),
        format: OutputFormat::Json,
        message: message.to_owned(),
    }
}

fn http_status_error(profile: &str, status: StatusCode, value: &Value) -> AdapterError {
    AdapterError::HttpStatus {
        profile: profile.to_owned(),
        status: status.as_u16(),
        error_type: value
            .pointer("/error/type")
            .and_then(Value::as_str)
            .and_then(safe_error_identifier),
        error_code: value.pointer("/error/code").and_then(|code| match code {
            Value::String(code) => safe_error_identifier(code),
            Value::Number(code) => Some(code.to_string()),
            _ => None,
        }),
    }
}

fn safe_error_identifier(value: &str) -> Option<String> {
    (value.len() <= 128
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.' | '/' | ':')
        }))
    .then(|| value.to_owned())
}

fn parse_reported_model(profile: &str, raw: &Value) -> Result<Option<String>, AdapterError> {
    match raw.get("model") {
        Some(model) => {
            let model = model.as_str().ok_or_else(|| {
                invalid_response(profile, "model must be a nonblank string when present")
            })?;
            if model.trim().is_empty()
                || model.len() > MAX_REPORTED_MODEL_BYTES
                || model.chars().any(char::is_control)
            {
                return Err(invalid_response(
                    profile,
                    "model must be a nonblank, control-free string of at most 512 bytes",
                ));
            }
            Ok(Some(model.to_owned()))
        }
        None => Ok(None),
    }
}

fn redact_json(value: &mut Value, secrets: &[String]) {
    match value {
        Value::String(text) => {
            for secret in secrets {
                if !secret.is_empty() {
                    *text = text.replace(secret, "[REDACTED]");
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                redact_json(value, secrets);
            }
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                redact_json(value, secrets);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use indexmap::IndexMap;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use super::*;
    use crate::adapter::AdapterCapability;

    fn http_capabilities() -> AdapterCapabilities {
        AdapterCapabilities::new([
            AdapterCapability::TextOutput,
            AdapterCapability::JsonOutput,
            AdapterCapability::SystemPrompt,
            AdapterCapability::ModelSelection,
        ])
    }

    async fn server(
        status: u16,
        response: Value,
    ) -> (
        Url,
        tokio::sync::oneshot::Receiver<(String, HashMap<String, String>)>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let address = listener.local_addr().expect("test address");
        let (request_sender, request_receiver) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept request");
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let read = socket.read(&mut buffer).await.expect("read request");
                if read == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..read]);
                if let Some(header_end) = find_subslice(&bytes, b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&bytes[..header_end]);
                    let length = header
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(str::trim)
                                .and_then(|value| value.parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if bytes.len() >= header_end + 4 + length {
                        break;
                    }
                }
            }
            let text = String::from_utf8(bytes).expect("HTTP request is UTF-8");
            let (head, body) = text.split_once("\r\n\r\n").expect("request separator");
            let headers = head
                .lines()
                .skip(1)
                .filter_map(|line| line.split_once(':'))
                .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
                .collect();
            request_sender.send((body.to_owned(), headers)).ok();
            let response = response.to_string();
            let reason = if status == 200 { "OK" } else { "Error" };
            let message = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                response.len()
            );
            socket
                .write_all(message.as_bytes())
                .await
                .expect("write response");
        });
        (
            Url::parse(&format!("http://{address}/v1/")).expect("test URL"),
            request_receiver,
        )
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    async fn delayed_server() -> (Url, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind delayed test server");
        let address = listener.local_addr().expect("test address");
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept request");
            let mut buffer = [0_u8; 4096];
            let _read = socket.read(&mut buffer).await.expect("read request");
            tokio::time::sleep(Duration::from_millis(50)).await;
            let body = r#"{"choices":[{"message":{"content":"late"}}]}"#;
            let message = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _write_result = socket.write_all(message.as_bytes()).await;
        });
        (
            Url::parse(&format!("http://{address}/v1/")).expect("test URL"),
            task,
        )
    }

    #[tokio::test]
    async fn openai_adapter_sends_secret_from_environment_and_normalizes_response() {
        let (base_url, captured) = server(
            200,
            json!({
                "model": "test-model",
                "choices": [{ "message": { "content": "hello" } }],
                "usage": { "prompt_tokens": 2, "completion_tokens": 1 }
            }),
        )
        .await;
        let profile = OpenAiProfile {
            base_url,
            model: "test-model".to_owned(),
            api_key_env: Some(SecretRef::environment("PATH")),
            organization_env: None,
            headers_from: IndexMap::new(),
            parameters: IndexMap::new(),
        };
        let adapter = OpenAiAdapter::new(
            "openai-test",
            None,
            http_capabilities(),
            profile,
            Client::new(),
        );
        let response = adapter
            .execute(
                AdapterRequest::new("say hello"),
                CancellationToken::new(),
                None,
            )
            .await
            .expect("HTTP call succeeds");
        assert_eq!(response.output.as_text(), Some("hello"));
        assert_eq!(response.usage.expect("usage").output_tokens, Some(1));
        let (body, headers) = captured.await.expect("captured request");
        let expected_authorization = format!(
            "Bearer {}",
            std::env::var("PATH").expect("test process has PATH")
        );
        assert_eq!(
            headers.get("authorization").map(String::as_str),
            Some(expected_authorization.as_str())
        );
        assert_eq!(
            serde_json::from_str::<Value>(&body).expect("request JSON")["messages"][0]["content"],
            "say hello"
        );
    }

    #[tokio::test]
    async fn anthropic_adapter_extracts_json_content() {
        let (base_url, _captured) = server(
            200,
            json!({
                "model": "claude-test",
                "content": [{ "type": "text", "text": "{\"ok\":true}" }],
                "usage": { "input_tokens": 2, "output_tokens": 3 }
            }),
        )
        .await;
        let profile = AnthropicProfile {
            base_url,
            model: "claude-test".to_owned(),
            api_key_env: SecretRef::environment("PATH"),
            api_version: "2023-06-01".to_owned(),
            max_tokens: 100,
            headers_from: IndexMap::new(),
            parameters: IndexMap::new(),
        };
        let adapter = AnthropicAdapter::new(
            "anthropic-test",
            None,
            http_capabilities(),
            profile,
            Client::new(),
        );
        let mut request = AdapterRequest::new("return JSON");
        request.output_format = OutputFormat::Json;
        let response = adapter
            .execute(request, CancellationToken::new(), None)
            .await
            .expect("HTTP call succeeds");
        assert_eq!(response.output, AdapterOutput::Json(json!({ "ok": true })));
    }

    #[tokio::test]
    async fn error_does_not_include_remote_message_or_secret() {
        let (base_url, _captured) = server(
            401,
            json!({ "error": { "type": "authentication_error", "message": "remote-sensitive-text" } }),
        )
        .await;
        let profile = OpenAiProfile {
            base_url,
            model: "test-model".to_owned(),
            api_key_env: Some(SecretRef::environment("PATH")),
            organization_env: None,
            headers_from: IndexMap::new(),
            parameters: IndexMap::new(),
        };
        let adapter = OpenAiAdapter::new(
            "openai-test",
            None,
            http_capabilities(),
            profile,
            Client::new(),
        );
        let error = adapter
            .execute(AdapterRequest::new("hello"), CancellationToken::new(), None)
            .await
            .expect_err("HTTP error expected");
        let rendered = format!("{error:?} {error}");
        assert!(!rendered.contains("remote-sensitive-text"));
    }

    #[tokio::test]
    async fn echoed_http_credentials_are_redacted_from_success_output() {
        let credential = std::env::var("PATH").expect("test process has PATH");
        let (base_url, _captured) = server(
            200,
            json!({
                "choices": [{ "message": { "content": format!("echo {credential}") } }]
            }),
        )
        .await;
        let profile = OpenAiProfile {
            base_url,
            model: "test-model".to_owned(),
            api_key_env: Some(SecretRef::environment("PATH")),
            organization_env: None,
            headers_from: IndexMap::new(),
            parameters: IndexMap::new(),
        };
        let response = OpenAiAdapter::new(
            "openai-test",
            None,
            http_capabilities(),
            profile,
            Client::new(),
        )
        .execute(AdapterRequest::new("hello"), CancellationToken::new(), None)
        .await
        .expect("HTTP call succeeds");
        assert_eq!(response.output.as_text(), Some("echo [REDACTED]"));
    }

    #[tokio::test]
    async fn http_request_timeout_is_not_retryable() {
        let (base_url, server_task) = delayed_server().await;
        let profile = OpenAiProfile {
            base_url,
            model: "test-model".to_owned(),
            api_key_env: Some(SecretRef::environment("PATH")),
            organization_env: None,
            headers_from: IndexMap::new(),
            parameters: IndexMap::new(),
        };
        let adapter = OpenAiAdapter::new(
            "openai-test",
            None,
            http_capabilities(),
            profile,
            Client::new(),
        );
        let mut request = AdapterRequest::new("hello");
        request.timeout = Some(Duration::from_millis(5));
        let error = adapter
            .execute(request, CancellationToken::new(), None)
            .await
            .expect_err("HTTP request must time out");
        assert!(matches!(error, AdapterError::Timeout { .. }));
        assert!(!error.is_retryable());
        server_task.await.expect("server task finishes");
    }

    #[tokio::test]
    async fn http_transport_error_is_not_retryable() {
        let profile = OpenAiProfile {
            base_url: Url::parse("http://127.0.0.1:65534/v1/").expect("test URL"),
            model: "test-model".to_owned(),
            api_key_env: None,
            organization_env: None,
            headers_from: IndexMap::new(),
            parameters: IndexMap::new(),
        };
        let adapter = OpenAiAdapter::new(
            "openai-offline",
            None,
            http_capabilities(),
            profile,
            Client::new(),
        );
        let error = adapter
            .execute(AdapterRequest::new("hello"), CancellationToken::new(), None)
            .await
            .expect_err("HTTP transport error expected");
        assert!(matches!(error, AdapterError::HttpTransport { .. }));
        assert!(!error.is_retryable());
    }

    #[tokio::test]
    async fn http_response_is_bounded_before_parsing() {
        let (base_url, captured) = server(
            200,
            json!({
                "choices": [{ "message": { "content": "response larger than limit" } }]
            }),
        )
        .await;
        let profile = OpenAiProfile {
            base_url,
            model: "test-model".to_owned(),
            api_key_env: None,
            organization_env: None,
            headers_from: IndexMap::new(),
            parameters: IndexMap::new(),
        };
        let adapter = OpenAiAdapter::new(
            "openai-test",
            None,
            http_capabilities(),
            profile,
            Client::new(),
        );
        let mut request = AdapterRequest::new("hello");
        request.max_output_bytes = 8;
        let error = adapter
            .execute(request, CancellationToken::new(), None)
            .await
            .expect_err("oversized HTTP response must fail");
        assert!(matches!(error, AdapterError::OutputTooLarge { .. }));
        let (_body, headers) = captured.await.expect("captured request");
        assert!(!headers.contains_key("authorization"));
    }

    #[tokio::test]
    async fn openai_adapter_rejects_blank_assistant_content() {
        let (base_url, _captured) = server(
            200,
            json!({
                "choices": [{ "message": { "content": "   " } }],
                "usage": { "prompt_tokens": 2, "completion_tokens": 1 }
            }),
        )
        .await;
        let profile = OpenAiProfile {
            base_url,
            model: "test-model".to_owned(),
            api_key_env: None,
            organization_env: None,
            headers_from: IndexMap::new(),
            parameters: IndexMap::new(),
        };
        let adapter = OpenAiAdapter::new(
            "openai-test",
            None,
            http_capabilities(),
            profile,
            Client::new(),
        );
        let error = adapter
            .execute(AdapterRequest::new("hello"), CancellationToken::new(), None)
            .await
            .expect_err("blank assistant content should fail");
        assert!(matches!(error, AdapterError::InvalidOutput { .. }));
    }

    #[tokio::test]
    async fn anthropic_adapter_rejects_blank_assistant_content() {
        let (base_url, _captured) = server(
            200,
            json!({
                "content": [{ "type": "text", "text": "  " }],
                "usage": { "input_tokens": 2, "output_tokens": 3 }
            }),
        )
        .await;
        let profile = AnthropicProfile {
            base_url,
            model: "claude-test".to_owned(),
            api_key_env: SecretRef::environment("PATH"),
            api_version: "2023-06-01".to_owned(),
            max_tokens: 100,
            headers_from: IndexMap::new(),
            parameters: IndexMap::new(),
        };
        let adapter = AnthropicAdapter::new(
            "anthropic-test",
            None,
            http_capabilities(),
            profile,
            Client::new(),
        );
        let error = adapter
            .execute(
                AdapterRequest::new("return blank"),
                CancellationToken::new(),
                None,
            )
            .await
            .expect_err("blank assistant content should fail");
        assert!(matches!(error, AdapterError::InvalidOutput { .. }));
    }

    #[tokio::test]
    async fn openai_adapter_reports_model_only_when_provider_supplies_it() {
        let (base_url, _captured) = server(
            200,
            json!({
                "choices": [{ "message": { "content": "hello" } }],
                "usage": { "prompt_tokens": 2, "completion_tokens": 1 }
            }),
        )
        .await;
        let profile = OpenAiProfile {
            base_url,
            model: "test-model".to_owned(),
            api_key_env: None,
            organization_env: None,
            headers_from: IndexMap::new(),
            parameters: IndexMap::new(),
        };
        let adapter = OpenAiAdapter::new(
            "openai-test",
            None,
            http_capabilities(),
            profile,
            Client::new(),
        );
        let response = adapter
            .execute(AdapterRequest::new("hello"), CancellationToken::new(), None)
            .await
            .expect("HTTP call succeeds");
        assert_eq!(response.output.as_text(), Some("hello"));
        assert_eq!(response.reported_model, None);
    }

    #[tokio::test]
    async fn anthropic_adapter_reports_model_only_when_provider_supplies_it() {
        let (base_url, _captured) = server(
            200,
            json!({
                "content": [{ "type": "text", "text": "hello" }],
                "usage": { "input_tokens": 2, "output_tokens": 1 }
            }),
        )
        .await;
        let profile = AnthropicProfile {
            base_url,
            model: "test-model".to_owned(),
            api_key_env: SecretRef::environment("PATH"),
            api_version: "2023-06-01".to_owned(),
            max_tokens: 100,
            headers_from: IndexMap::new(),
            parameters: IndexMap::new(),
        };
        let adapter = AnthropicAdapter::new(
            "anthropic-test",
            None,
            http_capabilities(),
            profile,
            Client::new(),
        );
        let response = adapter
            .execute(AdapterRequest::new("hello"), CancellationToken::new(), None)
            .await
            .expect("HTTP call succeeds");
        assert_eq!(response.output.as_text(), Some("hello"));
        assert_eq!(response.reported_model, None);
    }

    #[test]
    fn response_parsers_reject_blank_assistant_text_without_network() {
        let openai = json!({"choices": [{"message": {"content": "  \n"}}]});
        let anthropic = json!({"content": [{"type": "text", "text": "\t"}]});

        assert!(matches!(
            parse_openai_response("openai-test", &openai, OutputFormat::Text, None),
            Err(AdapterError::InvalidOutput { .. })
        ));
        assert!(matches!(
            parse_anthropic_response("anthropic-test", &anthropic, OutputFormat::Text, None),
            Err(AdapterError::InvalidOutput { .. })
        ));
    }

    #[test]
    fn response_parsers_reject_non_string_model_field() {
        let openai = json!({"model": 123, "choices": [{"message": {"content": "ok"}}]});
        let anthropic = json!({"model": {"value": "x"}, "content": [{"type":"text","text":"ok"}]});

        assert!(matches!(
            parse_openai_response("openai-test", &openai, OutputFormat::Text, None),
            Err(AdapterError::InvalidOutput { .. })
        ));
        assert!(matches!(
            parse_anthropic_response("anthropic-test", &anthropic, OutputFormat::Text, None),
            Err(AdapterError::InvalidOutput { .. })
        ));
    }

    #[test]
    fn response_parsers_reject_blank_model_string() {
        let openai = json!({"model": "  ", "choices": [{"message": {"content": "ok"}}]});
        let anthropic = json!({"model": "", "content": [{"type":"text","text":"ok"}]});

        assert!(matches!(
            parse_openai_response("openai-test", &openai, OutputFormat::Text, None),
            Err(AdapterError::InvalidOutput { .. })
        ));
        assert!(matches!(
            parse_anthropic_response("anthropic-test", &anthropic, OutputFormat::Text, None),
            Err(AdapterError::InvalidOutput { .. })
        ));

        let oversized = "m".repeat(MAX_REPORTED_MODEL_BYTES + 1);
        let openai = json!({"model": oversized, "choices": [{"message": {"content": "ok"}}]});
        assert!(matches!(
            parse_openai_response("openai-test", &openai, OutputFormat::Text, None),
            Err(AdapterError::InvalidOutput { .. })
        ));
    }

    #[test]
    fn response_parsers_never_infer_a_requested_model() {
        let openai = json!({"choices": [{"message": {"content": "ok"}}]});
        let anthropic = json!({"content": [{"type": "text", "text": "ok"}]});

        let openai = parse_openai_response("openai-test", &openai, OutputFormat::Text, None)
            .expect("OpenAI response parses");
        let anthropic =
            parse_anthropic_response("anthropic-test", &anthropic, OutputFormat::Text, None)
                .expect("Anthropic response parses");
        assert_eq!(openai.reported_model, None);
        assert_eq!(anthropic.reported_model, None);
    }

    #[test]
    fn request_body_serialization_is_bounded_by_practical_limit() {
        let mut parameters = IndexMap::new();
        parameters.insert("prompt_hint".to_owned(), Value::String("x".repeat(2_048)));

        let mut body = request_body_from_parameters(&parameters);
        body.insert("model".to_owned(), Value::String("test-model".to_owned()));
        body.insert(
            "messages".to_owned(),
            json!([{ "role": "user", "content": "hello" }]),
        );
        body.insert("stream".to_owned(), Value::Bool(false));

        let result = serialize_request_body("openai-test", body, 1024);
        assert!(matches!(
            result,
            Err(AdapterError::OutputTooLarge {
                stream: "HTTP request",
                ..
            })
        ));
    }
}
