use std::{
    fs,
    path::{Path, PathBuf},
};

use directories::ProjectDirs;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use thiserror::Error;
use url::Url;

use crate::adapter::{AdapterCapabilities, AdapterCapability, OutputFormat};

pub const PROJECT_CONFIG_PATH: &str = ".gloop/profiles.toml";
pub const USER_CONFIG_FILE: &str = "profiles.toml";
const MAX_TIMEOUT_SECONDS: u64 = 31_536_000;
const MAX_PROFILE_NAME_BYTES: usize = 64;
const MAX_PROFILE_FIELD_BYTES: usize = 4_096;
const MAX_HTTP_PARAMETER_ENTRIES: usize = 256;
const MAX_HTTP_PARAMETERS_BYTES: usize = 256 * 1024;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretRef(String);

impl SecretRef {
    pub fn environment(variable: impl Into<String>) -> Self {
        Self(variable.into())
    }

    pub fn env_var(&self) -> &str {
        &self.0
    }

    pub(crate) fn resolve(&self) -> Result<String, std::env::VarError> {
        let value = std::env::var(&self.0)?;
        if value.is_empty() {
            Err(std::env::VarError::NotPresent)
        } else {
            Ok(value)
        }
    }
}

impl std::fmt::Debug for SecretRef {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("SecretRef")
            .field(&format_args!("env:{}", self.0))
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Profile {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub priority: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
    #[serde(default)]
    pub capabilities: AdapterCapabilities,
    #[serde(flatten)]
    pub kind: ProfileKind,
}

impl<'de> Deserialize<'de> for Profile {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as DeError;
        let value = toml::Value::deserialize(deserializer)?;
        let table = value
            .as_table()
            .ok_or_else(|| DeError::custom("provider profile must be a table"))?;

        let kind_name = profile_kind_name::<D::Error>(table)?;
        validate_profile_fields::<D::Error>(kind_name, table)?;

        let common = deserialize_common::<D>(table)?;
        let kind = deserialize_kind::<D>(kind_name, table)?;
        let capabilities = common
            .capabilities
            .unwrap_or_else(|| inferred_capabilities(&kind));

        Ok(Self {
            enabled: common.enabled,
            priority: common.priority,
            timeout_seconds: common.timeout_seconds,
            capabilities,
            kind,
        })
    }
}

const COMMON_KEYS: [&str; 4] = ["enabled", "priority", "timeout_seconds", "capabilities"];
const COMMAND_KEYS: [&str; 9] = [
    "argv",
    "prompt_mode",
    "prompt_args",
    "model_args",
    "system_prompt_args",
    "version_args",
    "output",
    "output_pointer",
    "env_from",
];
const OPENAI_KEYS: [&str; 6] = [
    "base_url",
    "model",
    "api_key_env",
    "organization_env",
    "headers_from",
    "parameters",
];
const ANTHROPIC_KEYS: [&str; 7] = [
    "base_url",
    "model",
    "api_key_env",
    "api_version",
    "max_tokens",
    "headers_from",
    "parameters",
];

fn profile_kind_name<E>(table: &toml::Table) -> Result<&str, E>
where
    E: serde::de::Error,
{
    table
        .get("kind")
        .and_then(toml::Value::as_str)
        .ok_or_else(|| E::missing_field("kind"))
}

fn allowed_kind_keys(kind: &str) -> &[&str] {
    match kind {
        "command" => COMMAND_KEYS.as_slice(),
        "openai" => OPENAI_KEYS.as_slice(),
        "anthropic" => ANTHROPIC_KEYS.as_slice(),
        _ => &[],
    }
}

fn validate_profile_fields<E>(kind_name: &str, table: &toml::Table) -> Result<(), E>
where
    E: serde::de::Error,
{
    let allowed = allowed_kind_keys(kind_name);
    if allowed.is_empty() {
        return Err(E::custom(format!("unknown profile kind {kind_name:?}")));
    }

    for key in table.keys() {
        if COMMON_KEYS.contains(&key.as_str()) || allowed.contains(&key.as_str()) || key == "kind" {
            continue;
        }
        return Err(E::custom(format!("unknown field {key}")));
    }
    Ok(())
}

fn collect_fields(table: &toml::Table, allowed: &[&str]) -> toml::Table {
    let mut fields = toml::Table::new();
    for (key, value) in table {
        if allowed.contains(&key.as_str()) {
            fields.insert(key.clone(), value.clone());
        }
    }
    fields
}

#[derive(Deserialize)]
struct RawCommon {
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    priority: i32,
    #[serde(default)]
    timeout_seconds: Option<u64>,
    #[serde(default)]
    capabilities: Option<AdapterCapabilities>,
}

fn deserialize_common<'de, D>(table: &toml::Table) -> Result<RawCommon, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as DeError;

    let mut common = toml::Table::new();
    for key in COMMON_KEYS {
        if let Some(value) = table.get(key) {
            common.insert(key.to_string(), value.clone());
        }
    }
    toml::Value::Table(common)
        .try_into()
        .map_err(DeError::custom)
}

fn deserialize_kind<'de, D>(kind: &str, table: &toml::Table) -> Result<ProfileKind, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as DeError;

    match kind {
        "command" => toml::Value::Table(collect_fields(table, COMMAND_KEYS.as_slice()))
            .try_into()
            .map_err(DeError::custom)
            .map(ProfileKind::Command),
        "openai" => toml::Value::Table(collect_fields(table, OPENAI_KEYS.as_slice()))
            .try_into()
            .map_err(DeError::custom)
            .map(ProfileKind::OpenAi),
        "anthropic" => toml::Value::Table(collect_fields(table, ANTHROPIC_KEYS.as_slice()))
            .try_into()
            .map_err(DeError::custom)
            .map(ProfileKind::Anthropic),
        _ => unreachable!(),
    }
}

impl Profile {
    pub fn command(command: CommandProfile) -> Self {
        Self {
            enabled: true,
            priority: 0,
            timeout_seconds: None,
            capabilities: AdapterCapabilities::text(),
            kind: ProfileKind::Command(command),
        }
    }

    pub fn validate(&self, name: &str) -> Result<(), ConfigError> {
        let allow_builtin_codex_sandbox = name == "codex"
            && builtin_profiles()
                .get("codex")
                .is_some_and(|builtin| self.kind == builtin.kind);
        self.validate_inner(name, allow_builtin_codex_sandbox)
    }

    fn validate_inner(
        &self,
        name: &str,
        allow_builtin_codex_sandbox: bool,
    ) -> Result<(), ConfigError> {
        if name.trim().is_empty() {
            return Err(ConfigError::InvalidProfile {
                profile: name.to_owned(),
                message: "profile name must not be empty".to_owned(),
            });
        }
        if name.len() > MAX_PROFILE_NAME_BYTES {
            return Err(ConfigError::InvalidProfile {
                profile: name.to_owned(),
                message: format!("profile name must not exceed {MAX_PROFILE_NAME_BYTES} bytes"),
            });
        }
        if self.capabilities.contains(AdapterCapability::NativeSandbox)
            && !allow_builtin_codex_sandbox
        {
            return Err(ConfigError::InvalidProfile {
                profile: name.to_owned(),
                message: "only the unmodified built-in codex command may declare native_sandbox"
                    .to_owned(),
            });
        }
        if self.timeout_seconds == Some(0) {
            return Err(ConfigError::InvalidProfile {
                profile: name.to_owned(),
                message: "timeout_seconds must be greater than zero".to_owned(),
            });
        }
        if self
            .timeout_seconds
            .is_some_and(|value| value > MAX_TIMEOUT_SECONDS)
        {
            return Err(ConfigError::InvalidProfile {
                profile: name.to_owned(),
                message: "timeout_seconds must not exceed 31_536_000".to_owned(),
            });
        }

        match &self.kind {
            ProfileKind::Command(command) => command.validate(name),
            ProfileKind::OpenAi(profile) => profile.validate(name),
            ProfileKind::Anthropic(profile) => profile.validate(name),
        }
    }

    pub fn credential_env(&self) -> Option<&str> {
        match &self.kind {
            ProfileKind::Command(_) => None,
            ProfileKind::OpenAi(profile) => profile.api_key_env.as_ref().map(SecretRef::env_var),
            ProfileKind::Anthropic(profile) => Some(profile.api_key_env.env_var()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProfileKind {
    Command(CommandProfile),
    #[serde(rename = "openai")]
    OpenAi(OpenAiProfile),
    Anthropic(AnthropicProfile),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandPromptMode {
    Argument,
    #[default]
    Stdin,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandProfile {
    pub argv: Vec<String>,
    #[serde(default)]
    pub prompt_mode: CommandPromptMode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prompt_args: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_args: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system_prompt_args: Vec<String>,
    #[serde(default = "default_version_args")]
    pub version_args: Vec<String>,
    #[serde(default)]
    pub output: OutputFormat,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_pointer: Option<String>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub env_from: IndexMap<String, String>,
}

impl CommandProfile {
    pub fn new(argv: Vec<String>) -> Self {
        Self {
            argv,
            prompt_mode: CommandPromptMode::Stdin,
            prompt_args: Vec::new(),
            model_args: Vec::new(),
            system_prompt_args: Vec::new(),
            version_args: default_version_args(),
            output: OutputFormat::Text,
            output_pointer: None,
            env_from: IndexMap::new(),
        }
    }

    fn validate(&self, name: &str) -> Result<(), ConfigError> {
        if self
            .argv
            .first()
            .is_none_or(|value| value.trim().is_empty())
        {
            return Err(ConfigError::InvalidProfile {
                profile: name.to_owned(),
                message: "command argv must contain a non-empty executable".to_owned(),
            });
        }
        if self
            .argv
            .iter()
            .chain(&self.prompt_args)
            .chain(&self.model_args)
            .chain(&self.system_prompt_args)
            .chain(&self.version_args)
            .any(|argument| argument.contains('\0'))
        {
            return Err(ConfigError::InvalidProfile {
                profile: name.to_owned(),
                message: "command arguments must not contain NUL bytes".to_owned(),
            });
        }
        if self
            .argv
            .iter()
            .chain(&self.prompt_args)
            .chain(&self.model_args)
            .chain(&self.system_prompt_args)
            .chain(&self.version_args)
            .any(|argument| argument.len() > MAX_PROFILE_FIELD_BYTES)
            || self
                .output_pointer
                .as_ref()
                .is_some_and(|pointer| pointer.len() > MAX_PROFILE_FIELD_BYTES)
        {
            return Err(ConfigError::InvalidProfile {
                profile: name.to_owned(),
                message: format!(
                    "command field values must not exceed {MAX_PROFILE_FIELD_BYTES} bytes",
                ),
            });
        }
        if self
            .output_pointer
            .as_ref()
            .is_some_and(|pointer| !pointer.is_empty() && !pointer.starts_with('/'))
        {
            return Err(ConfigError::InvalidProfile {
                profile: name.to_owned(),
                message: "output_pointer must be an RFC 6901 JSON pointer".to_owned(),
            });
        }
        if !self.model_args.is_empty()
            && !self
                .model_args
                .iter()
                .any(|argument| argument.contains("{model}"))
        {
            return Err(ConfigError::InvalidProfile {
                profile: name.to_owned(),
                message: "model_args must contain the {model} placeholder".to_owned(),
            });
        }
        if !self.system_prompt_args.is_empty()
            && !self
                .system_prompt_args
                .iter()
                .any(|argument| argument.contains("{system_prompt}"))
        {
            return Err(ConfigError::InvalidProfile {
                profile: name.to_owned(),
                message: "system_prompt_args must contain the {system_prompt} placeholder"
                    .to_owned(),
            });
        }
        if self.prompt_mode == CommandPromptMode::Argument
            && !self
                .prompt_args
                .iter()
                .any(|argument| argument.contains("{prompt}"))
        {
            return Err(ConfigError::InvalidProfile {
                profile: name.to_owned(),
                message: "argument prompt_args must contain the {prompt} placeholder".to_owned(),
            });
        }
        for (target, source) in &self.env_from {
            if !is_env_name(target) || !is_env_name(source) {
                return Err(ConfigError::InvalidProfile {
                    profile: name.to_owned(),
                    message: format!(
                        "env_from entries must map valid environment variable names ({target:?} -> {source:?})"
                    ),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenAiProfile {
    #[serde(default = "default_openai_base_url")]
    pub base_url: Url,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<SecretRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization_env: Option<String>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub headers_from: IndexMap<String, String>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub parameters: IndexMap<String, JsonValue>,
}

impl OpenAiProfile {
    fn validate(&self, name: &str) -> Result<(), ConfigError> {
        validate_http_profile(
            name,
            &self.base_url,
            &self.model,
            self.api_key_env.as_ref().map(SecretRef::env_var),
        )?;
        if self
            .organization_env
            .as_ref()
            .is_some_and(|value| !is_env_name(value))
        {
            return Err(ConfigError::InvalidProfile {
                profile: name.to_owned(),
                message: "organization_env must name an environment variable".to_owned(),
            });
        }
        validate_header_env(name, &self.headers_from)?;
        validate_http_parameters(name, &self.parameters)?;
        for reserved in ["model", "messages", "stream"] {
            if self.parameters.contains_key(reserved) {
                return Err(ConfigError::InvalidProfile {
                    profile: name.to_owned(),
                    message: format!("parameters must not override reserved field {reserved:?}"),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnthropicProfile {
    #[serde(default = "default_anthropic_base_url")]
    pub base_url: Url,
    pub model: String,
    #[serde(default = "default_anthropic_key_env")]
    pub api_key_env: SecretRef,
    #[serde(default = "default_anthropic_version")]
    pub api_version: String,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub headers_from: IndexMap<String, String>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub parameters: IndexMap<String, JsonValue>,
}

impl AnthropicProfile {
    fn validate(&self, name: &str) -> Result<(), ConfigError> {
        validate_http_profile(
            name,
            &self.base_url,
            &self.model,
            Some(self.api_key_env.env_var()),
        )?;
        if self.api_version.trim().is_empty() {
            return Err(ConfigError::InvalidProfile {
                profile: name.to_owned(),
                message: "api_version must not be empty".to_owned(),
            });
        }
        if self.max_tokens == 0 {
            return Err(ConfigError::InvalidProfile {
                profile: name.to_owned(),
                message: "max_tokens must be greater than zero".to_owned(),
            });
        }
        validate_header_env(name, &self.headers_from)?;
        validate_http_parameters(name, &self.parameters)?;
        for reserved in ["model", "messages", "system", "stream", "max_tokens"] {
            if self.parameters.contains_key(reserved) {
                return Err(ConfigError::InvalidProfile {
                    profile: name.to_owned(),
                    message: format!("parameters must not override reserved field {reserved:?}"),
                });
            }
        }
        Ok(())
    }
}

fn validate_http_parameters(
    name: &str,
    parameters: &IndexMap<String, JsonValue>,
) -> Result<(), ConfigError> {
    if parameters.len() > MAX_HTTP_PARAMETER_ENTRIES {
        return Err(ConfigError::InvalidProfile {
            profile: name.to_owned(),
            message: format!(
                "parameters must contain at most {MAX_HTTP_PARAMETER_ENTRIES} entries"
            ),
        });
    }
    let encoded = serde_json::to_vec(parameters).map_err(|error| ConfigError::InvalidProfile {
        profile: name.to_owned(),
        message: format!("parameters could not be serialized: {error}"),
    })?;
    if encoded.len() > MAX_HTTP_PARAMETERS_BYTES {
        return Err(ConfigError::InvalidProfile {
            profile: name.to_owned(),
            message: format!(
                "parameters must not exceed {MAX_HTTP_PARAMETERS_BYTES} serialized bytes"
            ),
        });
    }
    Ok(())
}

#[derive(Debug, Clone, Default)]
pub struct ProfileStore {
    profiles: IndexMap<String, Profile>,
}

impl ProfileStore {
    pub fn builtins() -> Self {
        Self {
            profiles: builtin_profiles(),
        }
    }

    pub fn load(_project_root: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let user = Self::user_config_path();
        Self::load_paths(user.as_deref(), None)
    }

    pub fn load_trusted_project(project_root: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let user = Self::user_config_path();
        let project = project_root.as_ref().join(PROJECT_CONFIG_PATH);
        Self::load_paths(user.as_deref(), Some(&project))
    }

    pub fn load_paths(
        user_path: Option<&Path>,
        project_path: Option<&Path>,
    ) -> Result<Self, ConfigError> {
        let mut values = builtin_profile_values();
        if let Some(path) = user_path {
            apply_file(path, &mut values)?;
        }
        if let Some(path) = project_path {
            apply_file(path, &mut values)?;
        }
        Self::from_values(values)
    }

    pub fn from_toml_str(source: &str) -> Result<Self, ConfigError> {
        let mut values = builtin_profile_values();
        let layer: ConfigLayer = toml::from_str(source).map_err(|source| ConfigError::Parse {
            path: PathBuf::from("<memory>"),
            source,
        })?;
        apply_layer(layer, &mut values);
        Self::from_values(values)
    }

    pub fn user_config_path() -> Option<PathBuf> {
        ProjectDirs::from("", "", "gloop")
            .map(|directories| directories.config_dir().join(USER_CONFIG_FILE))
    }

    pub fn get(&self, name: &str) -> Option<&Profile> {
        self.profiles.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Profile)> {
        self.profiles
            .iter()
            .map(|(name, profile)| (name.as_str(), profile))
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.profiles.keys().map(String::as_str)
    }

    pub fn insert(&mut self, name: impl Into<String>, profile: Profile) -> Result<(), ConfigError> {
        let name = name.into();
        profile.validate(&name)?;
        if self.profiles.contains_key(&name) {
            return Err(ConfigError::DuplicateProfile { profile: name });
        }
        self.profiles.insert(name, profile);
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.profiles.is_empty()
    }

    pub fn len(&self) -> usize {
        self.profiles.len()
    }

    fn from_values(values: IndexMap<String, toml::Value>) -> Result<Self, ConfigError> {
        let mut profiles = IndexMap::new();
        let builtin_codex_kind = builtin_profiles()
            .shift_remove("codex")
            .expect("Codex built-in exists")
            .kind;
        for (name, value) in values {
            let profile: Profile =
                value
                    .try_into()
                    .map_err(|source| ConfigError::ProfileDecode {
                        profile: name.clone(),
                        source,
                    })?;
            let allow_builtin_codex_sandbox = name == "codex" && profile.kind == builtin_codex_kind;
            profile.validate_inner(&name, allow_builtin_codex_sandbox)?;
            profiles.insert(name, profile);
        }
        Ok(Self { profiles })
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read provider config {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("provider config {path} is too large: {size} bytes (max 8_388_608)")]
    FileTooLarge { path: PathBuf, size: u64 },
    #[error("invalid provider config {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("invalid provider profile {profile:?}: {source}")]
    ProfileDecode {
        profile: String,
        #[source]
        source: toml::de::Error,
    },
    #[error("invalid provider profile {profile:?}: {message}")]
    InvalidProfile { profile: String, message: String },
    #[error("provider profile {profile:?} is already registered")]
    DuplicateProfile { profile: String },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigLayer {
    #[serde(default)]
    profiles: IndexMap<String, toml::Value>,
}

fn apply_file(
    path: &Path,
    profiles: &mut IndexMap<String, toml::Value>,
) -> Result<(), ConfigError> {
    const MAX_PROFILE_TOML_BYTES: u64 = 8 * 1024 * 1024;
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(ConfigError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    if metadata.len() > MAX_PROFILE_TOML_BYTES {
        return Err(ConfigError::FileTooLarge {
            path: path.to_path_buf(),
            size: metadata.len(),
        });
    }

    let source = match fs::read_to_string(path) {
        Ok(source) => source,
        Err(source) => {
            return Err(ConfigError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let layer: ConfigLayer = toml::from_str(&source).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    apply_layer(layer, profiles);
    Ok(())
}

fn apply_layer(layer: ConfigLayer, profiles: &mut IndexMap<String, toml::Value>) {
    for (name, overlay) in layer.profiles {
        if let Some(base) = profiles.get_mut(&name) {
            merge_toml(base, overlay);
        } else {
            profiles.insert(name, overlay);
        }
    }
}

fn merge_toml(base: &mut toml::Value, overlay: toml::Value) {
    match (base, overlay) {
        (toml::Value::Table(base), toml::Value::Table(overlay)) => {
            if overlay.get("kind").is_some_and(|overlay_kind| {
                base.get("kind")
                    .is_some_and(|base_kind| base_kind != overlay_kind)
            }) {
                *base = overlay;
                return;
            }
            for (key, value) in overlay {
                if let Some(base_value) = base.get_mut(&key) {
                    merge_toml(base_value, value);
                } else {
                    base.insert(key, value);
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

fn builtin_profile_values() -> IndexMap<String, toml::Value> {
    builtin_profiles()
        .into_iter()
        .map(|(name, profile)| {
            let value = toml::Value::try_from(profile).expect("built-in profile serializes");
            (name, value)
        })
        .collect()
}

fn builtin_profiles() -> IndexMap<String, Profile> {
    let capabilities = command_capabilities();
    let mut profiles: IndexMap<String, Profile> = [
        (
            "codex",
            builtin_command(
                vec!["codex", "exec", "--json", "--ephemeral"],
                CommandPromptMode::Stdin,
                vec!["-"],
                vec!["--model", "{model}"],
                OutputFormat::JsonLines,
                Some("/item/text"),
            ),
        ),
        (
            "claude",
            builtin_command(
                vec![
                    "claude",
                    "--print",
                    "--verbose",
                    "--output-format",
                    "stream-json",
                ],
                CommandPromptMode::Stdin,
                Vec::new(),
                vec!["--model", "{model}"],
                OutputFormat::JsonLines,
                Some("/result"),
            ),
        ),
        (
            "qwen",
            builtin_command(
                vec!["qwen", "--output-format", "stream-json"],
                CommandPromptMode::Argument,
                vec!["--prompt={prompt}"],
                vec!["--model", "{model}"],
                OutputFormat::JsonLines,
                Some("/result"),
            ),
        ),
        (
            "cursor-agent",
            builtin_command(
                vec!["cursor-agent", "-p", "--output-format", "stream-json"],
                CommandPromptMode::Argument,
                vec!["--", "{prompt}"],
                vec!["--model", "{model}"],
                OutputFormat::JsonLines,
                Some("/result"),
            ),
        ),
        (
            "pi",
            builtin_command(
                vec!["pi", "--print", "--mode", "json", "--no-session"],
                CommandPromptMode::Argument,
                vec!["--", "{prompt}"],
                vec!["--model", "{model}"],
                OutputFormat::JsonLines,
                Some("/message/content/0/text"),
            ),
        ),
        (
            "opencode",
            builtin_command(
                vec!["opencode", "run", "--format", "json"],
                CommandPromptMode::Argument,
                vec!["--", "{prompt}"],
                vec!["--model", "{model}"],
                OutputFormat::JsonLines,
                Some("/part/text"),
            ),
        ),
    ]
    .into_iter()
    .map(|(name, mut profile)| {
        profile.priority = -100;
        profile.capabilities.clone_from(&capabilities);
        (name.to_owned(), profile)
    })
    .collect();
    profiles
        .get_mut("codex")
        .expect("Codex built-in exists")
        .capabilities
        .insert(AdapterCapability::NativeSandbox);
    profiles
}

fn builtin_command(
    argv: Vec<&str>,
    prompt_mode: CommandPromptMode,
    prompt_args: Vec<&str>,
    model_args: Vec<&str>,
    output: OutputFormat,
    output_pointer: Option<&str>,
) -> Profile {
    Profile::command(CommandProfile {
        argv: argv.into_iter().map(str::to_owned).collect(),
        prompt_mode,
        prompt_args: prompt_args.into_iter().map(str::to_owned).collect(),
        model_args: model_args.into_iter().map(str::to_owned).collect(),
        system_prompt_args: Vec::new(),
        version_args: default_version_args(),
        output,
        output_pointer: output_pointer.map(str::to_owned),
        env_from: IndexMap::new(),
    })
}

fn command_capabilities() -> AdapterCapabilities {
    AdapterCapabilities::new([
        AdapterCapability::TextOutput,
        AdapterCapability::JsonOutput,
        AdapterCapability::JsonLinesOutput,
        AdapterCapability::Streaming,
        AdapterCapability::SystemPrompt,
        AdapterCapability::ModelSelection,
        AdapterCapability::WorkingDirectory,
        AdapterCapability::RepositoryRead,
        AdapterCapability::RepositoryWrite,
        AdapterCapability::ToolExecution,
        AdapterCapability::UsageReporting,
        AdapterCapability::PermissionControl,
    ])
}

fn inferred_capabilities(kind: &ProfileKind) -> AdapterCapabilities {
    match kind {
        ProfileKind::Command(command) => {
            let mut capabilities = AdapterCapabilities::new([
                AdapterCapability::TextOutput,
                AdapterCapability::SystemPrompt,
                AdapterCapability::WorkingDirectory,
            ]);
            if !command.model_args.is_empty() {
                capabilities.insert(AdapterCapability::ModelSelection);
            }
            match command.output {
                OutputFormat::Text => {}
                OutputFormat::Json => {
                    capabilities.insert(AdapterCapability::JsonOutput);
                }
                OutputFormat::JsonLines => {
                    capabilities.insert(AdapterCapability::JsonOutput);
                    capabilities.insert(AdapterCapability::JsonLinesOutput);
                    if command.env_from.is_empty() {
                        capabilities.insert(AdapterCapability::Streaming);
                    }
                }
            }
            capabilities
        }
        ProfileKind::OpenAi(_) | ProfileKind::Anthropic(_) => AdapterCapabilities::new([
            AdapterCapability::TextOutput,
            AdapterCapability::JsonOutput,
            AdapterCapability::SystemPrompt,
            AdapterCapability::ModelSelection,
            AdapterCapability::UsageReporting,
        ]),
    }
}

fn validate_http_profile(
    name: &str,
    base_url: &Url,
    model: &str,
    api_key_env: Option<&str>,
) -> Result<(), ConfigError> {
    if !matches!(base_url.scheme(), "http" | "https")
        || base_url.cannot_be_a_base()
        || !base_url.username().is_empty()
        || base_url.password().is_some()
        || base_url.query().is_some()
        || base_url.fragment().is_some()
    {
        return Err(ConfigError::InvalidProfile {
            profile: name.to_owned(),
            message: "base_url must be an HTTP(S) base URL without credentials, query, or fragment"
                .to_owned(),
        });
    }
    if model.trim().is_empty() {
        return Err(ConfigError::InvalidProfile {
            profile: name.to_owned(),
            message: "model must not be empty".to_owned(),
        });
    }
    if api_key_env.is_some_and(|api_key_env| !is_env_name(api_key_env)) {
        return Err(ConfigError::InvalidProfile {
            profile: name.to_owned(),
            message: "api_key_env must name an environment variable".to_owned(),
        });
    }
    Ok(())
}

fn validate_header_env(name: &str, headers: &IndexMap<String, String>) -> Result<(), ConfigError> {
    for (header, env_var) in headers {
        if header.trim().is_empty()
            || header.chars().any(|character| character.is_ascii_control())
            || !is_env_name(env_var)
        {
            return Err(ConfigError::InvalidProfile {
                profile: name.to_owned(),
                message: format!(
                    "headers_from must map valid header names to environment variable names ({header:?})"
                ),
            });
        }
    }
    Ok(())
}

fn is_env_name(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(first) if first == '_' || first.is_ascii_alphabetic())
        && chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

const fn default_true() -> bool {
    true
}

fn default_version_args() -> Vec<String> {
    vec!["--version".to_owned()]
}

fn default_openai_base_url() -> Url {
    Url::parse("https://api.openai.com/v1/").expect("static OpenAI URL is valid")
}

fn default_anthropic_base_url() -> Url {
    Url::parse("https://api.anthropic.com/v1/").expect("static Anthropic URL is valid")
}

fn default_anthropic_key_env() -> SecretRef {
    SecretRef::environment("ANTHROPIC_API_KEY")
}

fn default_anthropic_version() -> String {
    "2023-06-01".to_owned()
}

const fn default_max_tokens() -> u32 {
    4096
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn builtins_have_safe_argv_and_expected_names() {
        let store = ProfileStore::builtins();
        assert_eq!(
            store.names().collect::<Vec<_>>(),
            ["codex", "claude", "qwen", "cursor-agent", "pi", "opencode"]
        );
        for (name, profile) in store.iter() {
            profile.validate(name).expect("built-in profile is valid");
            let ProfileKind::Command(command) = &profile.kind else {
                panic!("built-in profile must be a command");
            };
            assert!(!command.argv.is_empty());
            assert!(
                !command
                    .argv
                    .iter()
                    .any(|argument| argument == "sh" || argument == "-c")
            );
        }
    }

    #[test]
    fn project_layer_overrides_user_and_can_patch_builtin() {
        let directory = tempdir().expect("temporary directory");
        let user = directory.path().join("user.toml");
        let project = directory.path().join("project.toml");
        fs::write(
            &user,
            r#"
[profiles.codex]
priority = 10
timeout_seconds = 30

[profiles.custom]
kind = "command"
argv = ["tool", "--headless"]
prompt_mode = "stdin"
"#,
        )
        .expect("write user config");
        fs::write(
            &project,
            r"
[profiles.codex]
priority = 20
enabled = false

[profiles.custom]
timeout_seconds = 5
",
        )
        .expect("write project config");

        let store = ProfileStore::load_paths(Some(&user), Some(&project)).expect("load profiles");
        let codex = store.get("codex").expect("codex profile");
        assert_eq!(codex.priority, 20);
        assert!(!codex.enabled);
        assert_eq!(codex.timeout_seconds, Some(30));
        let custom = store.get("custom").expect("custom profile");
        assert_eq!(custom.timeout_seconds, Some(5));
        let ProfileKind::Command(command) = &custom.kind else {
            panic!("custom command profile");
        };
        assert_eq!(command.argv, ["tool", "--headless"]);
    }

    #[test]
    fn rejects_timeout_seconds_above_graph_cap() {
        let source = r#"
[profiles.custom]
kind = "command"
argv = ["tool"]
timeout_seconds = 31_536_001
"#;

        let error =
            ProfileStore::from_toml_str(source).expect_err("timeout above graph cap must fail");
        assert!(error.to_string().contains("timeout_seconds"));
    }

    #[test]
    fn rejects_overly_large_profile_files() {
        let directory = tempdir().expect("temporary directory");
        let project = directory.path().join("project.toml");
        let payload = vec![b'a'; (8 * 1024 * 1024) + 1];
        fs::write(&project, payload).expect("write oversized profile");
        let error = ProfileStore::load_paths(None, Some(&project))
            .expect_err("oversized profile file must fail");
        assert!(matches!(error, ConfigError::FileTooLarge { .. }));
    }

    #[test]
    fn rejects_literal_or_invalid_credential_references() {
        let error = ProfileStore::from_toml_str(
            r#"
[profiles.remote]
kind = "openai"
model = "test"
api_key_env = "not a valid env name"
"#,
        )
        .expect_err("invalid env reference must fail");
        assert!(error.to_string().contains("api_key_env"));
    }

    #[test]
    fn rejects_unknown_config_fields() {
        let error = ProfileStore::from_toml_str(
            r#"
[profiles.custom]
kind = "command"
argv = ["tool"]
shell = true
"#,
        )
        .expect_err("unknown fields must fail");
        assert!(error.to_string().contains("shell"));
    }

    #[test]
    fn rejects_unknown_top_level_profile_fields_for_openai() {
        let error = ProfileStore::from_toml_str(
            r#"
[profiles.misspelled]
kind = "openai"
model = "test-model"
api_key_env_var = "OPENAI_API_KEY"
"#,
        )
        .expect_err("unknown top-level fields must fail");
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn rejects_unknown_top_level_profile_aliases_for_timeout() {
        let error = ProfileStore::from_toml_str(
            r#"
[profiles.misspelled]
kind = "openai"
model = "test-model"
timeout_sec = 10
"#,
        )
        .expect_err("unknown top-level fields must fail");
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn rejects_unknown_top_level_profile_capability_alias() {
        let error = ProfileStore::from_toml_str(
            r#"
[profiles.misspelled]
kind = "openai"
model = "test-model"
capability = ["text_output"]
"#,
        )
        .expect_err("unknown top-level fields must fail");
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn rejects_custom_command_native_sandbox_assertion() {
        let error = ProfileStore::from_toml_str(
            r#"
[profiles.custom]
kind = "command"
argv = ["tool"]
capabilities = ["native_sandbox"]
"#,
        )
        .expect_err("custom native_sandbox capability must fail");
        assert!(error.to_string().contains("native_sandbox"));
    }

    #[test]
    fn rejects_codex_overlay_that_changes_the_sandboxed_command() {
        let error = ProfileStore::from_toml_str(
            r#"
[profiles.codex]
argv = ["sh", "-c", "echo not-sandboxed"]
"#,
        )
        .expect_err("an overlaid Codex command must not inherit native_sandbox");
        assert!(error.to_string().contains("unmodified built-in codex"));
    }

    #[test]
    fn rejects_excessively_large_command_field_values() {
        let long_argument = "x".repeat(MAX_PROFILE_FIELD_BYTES + 1);
        let source = format!(
            r#"
[profiles.custom]
kind = "command"
argv = ["tool"]
prompt_args = ["{long_argument}"]
"#,
        );
        let error =
            ProfileStore::from_toml_str(&source).expect_err("large command field values must fail");
        assert!(error.to_string().contains("must not exceed"));
    }

    #[test]
    fn rejects_excessively_large_http_parameter_maps() {
        let large_value = "x".repeat(MAX_HTTP_PARAMETERS_BYTES + 1);
        let source = format!(
            r#"
[profiles.remote]
kind = "openai"
base_url = "https://example.invalid/v1"
model = "test"
parameters = {{ metadata = "{large_value}" }}
"#,
        );
        let error = ProfileStore::from_toml_str(&source)
            .expect_err("large HTTP parameter maps must fail validation");
        assert!(error.to_string().contains("serialized bytes"));
    }

    #[test]
    fn direct_registration_rejects_duplicate_profile_names() {
        let mut store = ProfileStore::default();
        let profile = Profile::command(CommandProfile::new(vec!["tool".to_owned()]));
        store
            .insert("custom", profile.clone())
            .expect("first registration succeeds");
        let error = store
            .insert("custom", profile)
            .expect_err("duplicate registration must fail");
        assert!(matches!(error, ConfigError::DuplicateProfile { .. }));
    }

    #[test]
    fn rejects_extremely_long_profile_names() {
        let name = "x".repeat(MAX_PROFILE_NAME_BYTES + 1);
        let source = format!(
            r#"
[profiles.{name}]
kind = "command"
argv = ["tool"]
"#,
        );
        let error = ProfileStore::from_toml_str(&source).expect_err("long profile names must fail");
        assert!(error.to_string().contains("must not exceed"));
    }

    #[test]
    fn changing_kind_replaces_lower_layer_instead_of_mixing_schemas() {
        let store = ProfileStore::from_toml_str(
            r#"
[profiles.codex]
kind = "openai"
model = "compatible-model"
api_key_env = "COMPATIBLE_API_KEY"
"#,
        )
        .expect("kind replacement is valid");
        let profile = store.get("codex").expect("overridden profile");
        assert!(matches!(profile.kind, ProfileKind::OpenAi(_)));
        assert!(
            profile
                .capabilities
                .contains(AdapterCapability::ModelSelection)
        );
    }

    #[test]
    fn rejects_command_templates_without_required_placeholders() {
        for (field, value, expected) in [
            ("model_args", "[\"--model\", \"fixed\"]", "{model}"),
            (
                "system_prompt_args",
                "[\"--system\", \"fixed\"]",
                "{system_prompt}",
            ),
        ] {
            let source = format!(
                r#"
[profiles.invalid]
kind = "command"
argv = ["tool"]
{field} = {value}
"#
            );
            let error = ProfileStore::from_toml_str(&source)
                .expect_err("missing placeholder must fail profile loading");
            assert!(error.to_string().contains(expected));
        }
    }
}
