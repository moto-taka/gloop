use std::{collections::BTreeSet, path::PathBuf, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;
pub(crate) const MAX_REPORTED_MODEL_BYTES: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterCapability {
    TextOutput,
    JsonOutput,
    JsonLinesOutput,
    Streaming,
    SystemPrompt,
    ModelSelection,
    WorkingDirectory,
    RepositoryRead,
    RepositoryWrite,
    ToolExecution,
    ResumeSession,
    UsageReporting,
    NativeSandbox,
    PermissionControl,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AdapterCapabilities(BTreeSet<AdapterCapability>);

impl AdapterCapabilities {
    pub fn new(capabilities: impl IntoIterator<Item = AdapterCapability>) -> Self {
        Self(capabilities.into_iter().collect())
    }

    pub fn text() -> Self {
        Self::new([AdapterCapability::TextOutput])
    }

    pub fn contains(&self, capability: AdapterCapability) -> bool {
        self.0.contains(&capability)
    }

    pub fn supports(&self, required: &Self) -> bool {
        required.0.is_subset(&self.0)
    }

    pub fn missing(&self, required: &Self) -> Vec<AdapterCapability> {
        required.0.difference(&self.0).copied().collect()
    }

    pub fn insert(&mut self, capability: AdapterCapability) -> bool {
        self.0.insert(capability)
    }

    pub fn extend(&mut self, capabilities: &Self) {
        self.0.extend(capabilities.0.iter().copied());
    }

    pub fn iter(&self) -> impl Iterator<Item = AdapterCapability> + '_ {
        self.0.iter().copied()
    }
}

impl Default for AdapterCapabilities {
    fn default() -> Self {
        Self::text()
    }
}

impl FromIterator<AdapterCapability> for AdapterCapabilities {
    fn from_iter<T: IntoIterator<Item = AdapterCapability>>(iter: T) -> Self {
        Self::new(iter)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
    #[serde(rename = "jsonl")]
    JsonLines,
}

impl OutputFormat {
    pub const fn capability(self) -> AdapterCapability {
        match self {
            Self::Text => AdapterCapability::TextOutput,
            Self::Json => AdapterCapability::JsonOutput,
            Self::JsonLines => AdapterCapability::JsonLinesOutput,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "format", content = "value", rename_all = "snake_case")]
pub enum AdapterOutput {
    Text(String),
    Json(Value),
    #[serde(rename = "jsonl")]
    JsonLines(Vec<Value>),
}

impl AdapterOutput {
    pub fn into_value(self) -> Value {
        match self {
            Self::Text(text) => Value::String(text),
            Self::Json(value) => value,
            Self::JsonLines(values) => Value::Array(values),
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            Self::Json(_) | Self::JsonLines(_) => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AdapterRequest {
    pub prompt: String,
    pub system_prompt: Option<String>,
    pub model: Option<String>,
    pub working_directory: Option<PathBuf>,
    pub output_format: OutputFormat,
    pub timeout: Option<Duration>,
    pub max_prompt_bytes: usize,
    pub max_output_bytes: usize,
}

impl AdapterRequest {
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            system_prompt: None,
            model: None,
            working_directory: None,
            output_format: OutputFormat::Text,
            timeout: None,
            max_prompt_bytes: 1024 * 1024,
            max_output_bytes: 1024 * 1024,
        }
    }

    pub fn required_capabilities(&self) -> AdapterCapabilities {
        let mut required = AdapterCapabilities::new([self.output_format.capability()]);
        if self.system_prompt.is_some() {
            required.insert(AdapterCapability::SystemPrompt);
        }
        if self.model.is_some() {
            required.insert(AdapterCapability::ModelSelection);
        }
        required
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterEventKind {
    Started,
    OutputDelta,
    Diagnostic,
    Usage,
    Finished,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdapterEvent {
    pub timestamp: DateTime<Utc>,
    pub kind: AdapterEventKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub data: Value,
}

impl AdapterEvent {
    pub fn new(kind: AdapterEventKind) -> Self {
        Self {
            timestamp: Utc::now(),
            kind,
            message: None,
            data: Value::Null,
        }
    }

    pub fn message(kind: AdapterEventKind, message: impl Into<String>) -> Self {
        Self {
            timestamp: Utc::now(),
            kind,
            message: Some(message.into()),
            data: Value::Null,
        }
    }

    pub fn data(kind: AdapterEventKind, data: Value) -> Self {
        Self {
            timestamp: Utc::now(),
            kind,
            message: None,
            data,
        }
    }
}

pub type AdapterEventSender = mpsc::UnboundedSender<AdapterEvent>;

pub(crate) fn emit(events: Option<&AdapterEventSender>, event: AdapterEvent) {
    if let Some(events) = events {
        let _receiver_may_have_closed = events.send(event);
    }
}

pub(crate) fn validate_request_limits(
    profile: &str,
    request: &AdapterRequest,
) -> Result<(), AdapterError> {
    if request.max_prompt_bytes == 0 {
        return Err(AdapterError::InvalidRequest {
            profile: profile.to_owned(),
            message: "max_prompt_bytes must be greater than zero".to_owned(),
        });
    }
    if request.max_output_bytes == 0 {
        return Err(AdapterError::InvalidRequest {
            profile: profile.to_owned(),
            message: "max_output_bytes must be greater than zero".to_owned(),
        });
    }
    if request.max_prompt_bytes > MAX_REQUEST_BYTES {
        return Err(AdapterError::InvalidRequest {
            profile: profile.to_owned(),
            message: format!("max_prompt_bytes must not exceed {MAX_REQUEST_BYTES} bytes"),
        });
    }
    if request.max_output_bytes > MAX_REQUEST_BYTES {
        return Err(AdapterError::InvalidRequest {
            profile: profile.to_owned(),
            message: format!("max_output_bytes must not exceed {MAX_REQUEST_BYTES} bytes"),
        });
    }
    let prompt_bytes = request.prompt.len() + request.system_prompt.as_ref().map_or(0, String::len);
    if prompt_bytes > request.max_prompt_bytes {
        return Err(AdapterError::OutputTooLarge {
            profile: profile.to_owned(),
            stream: "prompt",
            limit: request.max_prompt_bytes,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_request_limits_rejects_large_max_prompt_bytes() {
        let request = AdapterRequest {
            max_prompt_bytes: MAX_REQUEST_BYTES + 1,
            ..AdapterRequest::new("hello")
        };
        let error = validate_request_limits("provider", &request).unwrap_err();
        assert!(matches!(error, AdapterError::InvalidRequest { .. }));
    }

    #[test]
    fn validate_request_limits_rejects_large_max_output_bytes() {
        let request = AdapterRequest {
            max_output_bytes: MAX_REQUEST_BYTES + 1,
            ..AdapterRequest::new("hello")
        };
        let error = validate_request_limits("provider", &request).unwrap_err();
        assert!(matches!(error, AdapterError::InvalidRequest { .. }));
    }

    #[test]
    fn validate_request_limits_allows_64mb_limits() {
        let request = AdapterRequest {
            max_prompt_bytes: MAX_REQUEST_BYTES,
            max_output_bytes: MAX_REQUEST_BYTES,
            ..AdapterRequest::new("hello")
        };
        validate_request_limits("provider", &request).expect("limits should be valid");
    }

    #[test]
    fn http_429_status_is_retryable() {
        let error = AdapterError::HttpStatus {
            profile: "retry-profile".to_owned(),
            status: 429,
            error_type: None,
            error_code: None,
        };
        assert_eq!(error.class(), AdapterErrorClass::RateLimit);
        assert!(error.is_retryable());

        let context_limit = AdapterError::HttpStatus {
            profile: "retry-profile".to_owned(),
            status: 429,
            error_type: Some("context_length_exceeded".to_owned()),
            error_code: None,
        };
        assert_eq!(context_limit.class(), AdapterErrorClass::ContextLength);
        assert!(!context_limit.is_retryable());
    }

    #[test]
    fn http_ambiguous_statuses_are_not_retryable() {
        let statuses = [408, 409, 425, 500, 503];
        for status in statuses {
            let error = AdapterError::HttpStatus {
                profile: "retry-profile".to_owned(),
                status,
                error_type: None,
                error_code: None,
            };
            assert_eq!(error.class(), AdapterErrorClass::Transient);
            assert!(
                !error.is_retryable(),
                "status {status} must not be auto-retried"
            );
        }
    }

    #[test]
    fn transport_timeout_is_not_retryable_without_explicit_flag() {
        let error = AdapterError::transport_timeout("retry-profile", 1234);
        assert!(!error.is_retryable());
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdapterResponse {
    pub output: AdapterOutput,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stdout: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stderr: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_model: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reported_model_informational: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
}

#[derive(Debug, Error)]
pub enum AdapterError {
    #[error("provider profile {0:?} was not found")]
    ProfileNotFound(String),
    #[error("provider profile {profile:?} lacks required capabilities: {missing:?}")]
    CapabilityMismatch {
        profile: String,
        missing: Vec<AdapterCapability>,
    },
    #[error("no available provider profile supports {required:?}")]
    NoMatchingProfile { required: Vec<AdapterCapability> },
    #[error("provider profile {profile:?} is disabled")]
    Disabled { profile: String },
    #[error("provider profile {profile:?} requires environment variable {env_var}")]
    MissingCredential { profile: String, env_var: String },
    #[error("provider profile {profile:?} is unavailable: {reason}")]
    Unavailable { profile: String, reason: String },
    #[error("invalid request for provider profile {profile:?}: {message}")]
    InvalidRequest { profile: String, message: String },
    #[error("failed to start executable {executable:?}: {source}")]
    Spawn {
        executable: String,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "provider process {executable:?} for profile {profile:?} failed with exit code {code:?}"
    )]
    ProcessFailed {
        profile: String,
        executable: String,
        code: Option<i32>,
        stdout: String,
        stderr: String,
    },
    #[error("provider profile {profile:?} I/O failed during {operation}: {source}")]
    Io {
        profile: String,
        operation: &'static str,
        #[source]
        source: std::io::Error,
        retryable: bool,
    },
    #[error("provider profile {profile:?} timed out after {timeout_ms} ms")]
    Timeout {
        profile: String,
        timeout_ms: u64,
        retryable: bool,
    },
    #[error("provider profile {profile:?} was cancelled")]
    Cancelled { profile: String },
    #[error("provider profile {profile:?} returned more than {limit} bytes on {stream}")]
    OutputTooLarge {
        profile: String,
        stream: &'static str,
        limit: usize,
    },
    #[error("provider profile {profile:?} returned invalid {format:?}: {message}")]
    InvalidOutput {
        profile: String,
        format: OutputFormat,
        message: String,
    },
    #[error("HTTP request for provider profile {profile:?} failed: {source}")]
    HttpTransport {
        profile: String,
        #[source]
        source: reqwest::Error,
    },
    #[error(
        "provider profile {profile:?} returned HTTP {status}{error_suffix}",
        error_suffix = format_http_error_suffix(.error_type.as_deref(), .error_code.as_deref())
    )]
    HttpStatus {
        profile: String,
        status: u16,
        error_type: Option<String>,
        error_code: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterErrorClass {
    RateLimit,
    Transient,
    Authentication,
    ContextLength,
    Protocol,
    Configuration,
    Process,
    Timeout,
    Cancelled,
}

impl AdapterError {
    pub(crate) fn command_io(
        profile: impl Into<String>,
        operation: &'static str,
        source: std::io::Error,
    ) -> Self {
        Self::Io {
            profile: profile.into(),
            operation,
            source,
            retryable: false,
        }
    }

    pub(crate) fn command_timeout(profile: impl Into<String>, timeout_ms: u64) -> Self {
        Self::Timeout {
            profile: profile.into(),
            timeout_ms,
            retryable: false,
        }
    }

    pub(crate) fn probe_io(
        profile: impl Into<String>,
        operation: &'static str,
        source: std::io::Error,
    ) -> Self {
        Self::Io {
            profile: profile.into(),
            operation,
            source,
            retryable: true,
        }
    }

    pub(crate) fn transport_timeout(profile: impl Into<String>, timeout_ms: u64) -> Self {
        Self::Timeout {
            profile: profile.into(),
            timeout_ms,
            retryable: false,
        }
    }

    pub fn class(&self) -> AdapterErrorClass {
        match self {
            Self::ProfileNotFound(_)
            | Self::CapabilityMismatch { .. }
            | Self::NoMatchingProfile { .. }
            | Self::Disabled { .. }
            | Self::MissingCredential { .. }
            | Self::InvalidRequest { .. }
            | Self::Spawn { .. }
            | Self::Unavailable { .. } => AdapterErrorClass::Configuration,
            Self::ProcessFailed { .. } => AdapterErrorClass::Process,
            Self::Io { .. } | Self::HttpTransport { .. } => AdapterErrorClass::Transient,
            Self::Timeout { .. } => AdapterErrorClass::Timeout,
            Self::Cancelled { .. } => AdapterErrorClass::Cancelled,
            Self::OutputTooLarge { .. } => AdapterErrorClass::ContextLength,
            Self::InvalidOutput { .. } => AdapterErrorClass::Protocol,
            Self::HttpStatus {
                status,
                error_type,
                error_code,
                ..
            } => classify_http_status(*status, error_type.as_deref(), error_code.as_deref()),
        }
    }

    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Io { retryable, .. } | Self::Timeout { retryable, .. } => *retryable,
            Self::HttpStatus { .. } => self.class() == AdapterErrorClass::RateLimit,
            _ => false,
        }
    }
}

fn classify_http_status(
    status: u16,
    error_type: Option<&str>,
    error_code: Option<&str>,
) -> AdapterErrorClass {
    let identifier = format!(
        "{} {}",
        error_type.unwrap_or_default(),
        error_code.unwrap_or_default()
    )
    .to_ascii_lowercase();
    if identifier.contains("context") || identifier.contains("token_limit") || status == 413 {
        AdapterErrorClass::ContextLength
    } else if status == 429 {
        AdapterErrorClass::RateLimit
    } else if matches!(status, 401 | 403) {
        AdapterErrorClass::Authentication
    } else if matches!(status, 408 | 409 | 425) || status >= 500 {
        // These responses are transient in nature but not safe to replay: the
        // provider may already have accepted or completed the original call.
        AdapterErrorClass::Transient
    } else {
        AdapterErrorClass::Protocol
    }
}

fn format_http_error_suffix(error_type: Option<&str>, error_code: Option<&str>) -> String {
    match (error_type, error_code) {
        (None, None) => String::new(),
        (kind, code) => format!(
            " (type={}, code={})",
            kind.unwrap_or("unknown"),
            code.unwrap_or("unknown")
        ),
    }
}

#[async_trait]
pub trait ProviderAdapter: Send + Sync + std::fmt::Debug {
    fn profile_name(&self) -> &str;

    fn capabilities(&self) -> &AdapterCapabilities;

    async fn execute(
        &self,
        request: AdapterRequest,
        cancellation: CancellationToken,
        events: Option<AdapterEventSender>,
    ) -> Result<AdapterResponse, AdapterError>;
}
