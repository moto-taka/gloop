use std::{collections::HashMap, process::Stdio, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process,
    process::Command,
    time,
};
use tokio_util::sync::CancellationToken;

use crate::{
    adapter::{
        AdapterCapabilities, AdapterError, AdapterEventSender, AdapterRequest, AdapterResponse,
        ProviderAdapter,
    },
    command::{CommandAdapter, apply_isolated_environment},
    config::{Profile, ProfileKind, ProfileStore},
    http::{AnthropicAdapter, OpenAiAdapter},
};

const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_PROBE_OUTPUT_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionOrigin {
    Explicit,
    Capability,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelOrigin {
    Request,
    Profile,
    ProviderDefault,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderSelection {
    pub profile: String,
    pub origin: SelectionOrigin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub model_origin: ModelOrigin,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegistryResponse {
    pub selection: ProviderSelection,
    pub response: AdapterResponse,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeFailure {
    Disabled,
    ExecutableNotFound,
    VersionCommandFailed,
    MissingEnvironment { variable: String },
    TimedOut,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeResult {
    pub profile: String,
    pub available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<ProbeFailure>,
    pub checked_at: DateTime<Utc>,
}

#[derive(Debug)]
pub struct ResolvedAdapter {
    pub profile: String,
    pub origin: SelectionOrigin,
    pub probe: ProbeResult,
    pub adapter: Arc<dyn ProviderAdapter>,
}

#[derive(Debug, Clone)]
pub struct ProviderRegistry {
    profiles: ProfileStore,
    client: reqwest::Client,
    probe_timeout: Duration,
    probe_cache: Arc<tokio::sync::RwLock<HashMap<String, ProbeResult>>>,
}

impl ProviderRegistry {
    pub fn new(profiles: ProfileStore) -> Self {
        Self {
            profiles,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("provider HTTP client configuration is valid"),
            probe_timeout: DEFAULT_PROBE_TIMEOUT,
            probe_cache: Arc::<tokio::sync::RwLock<HashMap<String, ProbeResult>>>::default(),
        }
    }

    #[must_use]
    pub fn with_probe_timeout(mut self, timeout: Duration) -> Self {
        self.probe_timeout = timeout;
        self
    }

    pub const fn profiles(&self) -> &ProfileStore {
        &self.profiles
    }

    pub fn adapter(&self, name: &str) -> Result<Arc<dyn ProviderAdapter>, AdapterError> {
        let profile = self
            .profiles
            .get(name)
            .ok_or_else(|| AdapterError::ProfileNotFound(name.to_owned()))?;
        if !profile.enabled {
            return Err(AdapterError::Disabled {
                profile: name.to_owned(),
            });
        }
        Ok(self.build_adapter(name, profile))
    }

    pub async fn resolve(
        &self,
        preferred: Option<&str>,
        required: &AdapterCapabilities,
        cancellation: CancellationToken,
    ) -> Result<Arc<dyn ProviderAdapter>, AdapterError> {
        Ok(self
            .resolve_with_selection(preferred, required, cancellation)
            .await?
            .adapter)
    }

    pub async fn resolve_with_selection(
        &self,
        preferred: Option<&str>,
        required: &AdapterCapabilities,
        cancellation: CancellationToken,
    ) -> Result<ResolvedAdapter, AdapterError> {
        if let Some(name) = preferred {
            let profile = self
                .profiles
                .get(name)
                .ok_or_else(|| AdapterError::ProfileNotFound(name.to_owned()))?;
            if !profile.enabled {
                return Err(AdapterError::Disabled {
                    profile: name.to_owned(),
                });
            }
            ensure_capabilities(name, profile, required)?;
            let probe = self.probe(name, cancellation).await?;
            if !probe.available {
                return Err(probe_unavailable_error(&probe));
            }
            return Ok(ResolvedAdapter {
                profile: name.to_owned(),
                origin: SelectionOrigin::Explicit,
                probe,
                adapter: self.build_adapter(name, profile),
            });
        }

        let mut candidates = self
            .profiles
            .iter()
            .filter(|(_, profile)| profile.enabled && profile.capabilities.supports(required))
            .collect::<Vec<_>>();
        candidates.sort_by(|(_, left), (_, right)| right.priority.cmp(&left.priority));
        for (name, profile) in candidates {
            let probe = self.probe(name, cancellation.clone()).await?;
            if probe.available {
                return Ok(ResolvedAdapter {
                    profile: name.to_owned(),
                    origin: SelectionOrigin::Capability,
                    probe,
                    adapter: self.build_adapter(name, profile),
                });
            }
        }

        Err(AdapterError::NoMatchingProfile {
            required: required.iter().collect(),
        })
    }

    pub async fn execute(
        &self,
        preferred: Option<&str>,
        request: AdapterRequest,
        cancellation: CancellationToken,
        events: Option<AdapterEventSender>,
    ) -> Result<RegistryResponse, AdapterError> {
        let required = request.required_capabilities();
        self.execute_with_capabilities(preferred, &required, request, cancellation, events)
            .await
    }

    pub async fn execute_with_capabilities(
        &self,
        preferred: Option<&str>,
        required: &AdapterCapabilities,
        request: AdapterRequest,
        cancellation: CancellationToken,
        events: Option<AdapterEventSender>,
    ) -> Result<RegistryResponse, AdapterError> {
        let mut required = required.clone();
        required.extend(&request.required_capabilities());
        let requested_model = request.model.clone();
        let resolved = self
            .resolve_with_selection(preferred, &required, cancellation.clone())
            .await?;
        let (configured_model, model_origin) = match requested_model {
            Some(model) => (Some(model), ModelOrigin::Request),
            None => match self
                .profiles
                .get(&resolved.profile)
                .expect("resolved profile remains registered")
                .kind
            {
                ProfileKind::OpenAi(ref profile) => {
                    (Some(profile.model.clone()), ModelOrigin::Profile)
                }
                ProfileKind::Anthropic(ref profile) => {
                    (Some(profile.model.clone()), ModelOrigin::Profile)
                }
                ProfileKind::Command(_) => (None, ModelOrigin::ProviderDefault),
            },
        };
        let selection = ProviderSelection {
            profile: resolved.profile,
            origin: resolved.origin,
            model: configured_model,
            model_origin,
        };
        let response = resolved
            .adapter
            .execute(request, cancellation, events)
            .await?;
        Ok(RegistryResponse {
            selection,
            response,
        })
    }

    pub async fn probe(
        &self,
        name: &str,
        cancellation: CancellationToken,
    ) -> Result<ProbeResult, AdapterError> {
        if let Some(cached) = self.probe_cache.read().await.get(name).cloned() {
            return Ok(cached);
        }

        let result = self.probe_uncached(name, cancellation).await?;
        self.probe_cache
            .write()
            .await
            .insert(name.to_owned(), result.clone());
        Ok(result)
    }

    async fn probe_uncached(
        &self,
        name: &str,
        cancellation: CancellationToken,
    ) -> Result<ProbeResult, AdapterError> {
        if cancellation.is_cancelled() {
            return Err(AdapterError::Cancelled {
                profile: name.to_owned(),
            });
        }
        let profile = self
            .profiles
            .get(name)
            .ok_or_else(|| AdapterError::ProfileNotFound(name.to_owned()))?;
        if !profile.enabled {
            return Ok(unavailable_probe(name, ProbeFailure::Disabled, None));
        }
        match &profile.kind {
            ProfileKind::Command(command) => {
                for source in command.env_from.values() {
                    if std::env::var_os(source).is_none() {
                        return Ok(unavailable_probe(
                            name,
                            ProbeFailure::MissingEnvironment {
                                variable: source.clone(),
                            },
                            command.argv.first().cloned(),
                        ));
                    }
                }
                probe_command(name, command, self.probe_timeout, cancellation).await
            }
            ProfileKind::OpenAi(profile) => Ok(probe_http(
                name,
                profile
                    .api_key_env
                    .as_ref()
                    .map(crate::config::SecretRef::env_var),
            )),
            ProfileKind::Anthropic(profile) => {
                Ok(probe_http(name, Some(profile.api_key_env.env_var())))
            }
        }
    }

    fn build_adapter(&self, name: &str, profile: &Profile) -> Arc<dyn ProviderAdapter> {
        let timeout = profile.timeout_seconds.map(Duration::from_secs);
        match &profile.kind {
            ProfileKind::Command(command) => Arc::new(CommandAdapter::new(
                name,
                timeout,
                profile.capabilities.clone(),
                command.clone(),
            )),
            ProfileKind::OpenAi(http) => Arc::new(OpenAiAdapter::new(
                name,
                timeout,
                profile.capabilities.clone(),
                http.clone(),
                self.client.clone(),
            )),
            ProfileKind::Anthropic(http) => Arc::new(AnthropicAdapter::new(
                name,
                timeout,
                profile.capabilities.clone(),
                http.clone(),
                self.client.clone(),
            )),
        }
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new(ProfileStore::builtins())
    }
}

fn ensure_capabilities(
    name: &str,
    profile: &Profile,
    required: &AdapterCapabilities,
) -> Result<(), AdapterError> {
    if profile.capabilities.supports(required) {
        Ok(())
    } else {
        Err(AdapterError::CapabilityMismatch {
            profile: name.to_owned(),
            missing: profile.capabilities.missing(required),
        })
    }
}

#[allow(clippy::too_many_lines)]
async fn probe_command(
    name: &str,
    command_profile: &crate::config::CommandProfile,
    timeout: Duration,
    cancellation: CancellationToken,
) -> Result<ProbeResult, AdapterError> {
    let executable = command_profile
        .argv
        .first()
        .expect("validated command has executable")
        .clone();
    let mut command = Command::new(&executable);
    #[cfg(unix)]
    command.process_group(0);
    apply_isolated_environment(&mut command);
    command
        .args(&command_profile.version_args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for source in command_profile.env_from.values() {
        std::env::var(source).map_err(|_| AdapterError::MissingCredential {
            profile: name.to_owned(),
            env_var: source.clone(),
        })?;
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(unavailable_probe(
                name,
                ProbeFailure::ExecutableNotFound,
                Some(executable),
            ));
        }
        Err(source) => {
            return Err(AdapterError::Spawn { executable, source });
        }
    };
    let process_group = child.id();
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    let mut stdout_task = tokio::spawn(drain_capped(stdout, MAX_PROBE_OUTPUT_BYTES));
    let mut stderr_task = tokio::spawn(drain_capped(stderr, MAX_PROBE_OUTPUT_BYTES));

    let outcome = {
        let wait = child.wait();
        tokio::pin!(wait);
        tokio::select! {
            biased;
            () = cancellation.cancelled() => ProbeOutcome::Cancelled,
            result = &mut wait => ProbeOutcome::Finished(result),
            () = tokio::time::sleep(timeout) => ProbeOutcome::TimedOut,
        }
    };
    let status = match outcome {
        ProbeOutcome::Finished(result) => result.map_err(|source| {
            AdapterError::probe_io(name.to_owned(), "waiting for version probe", source)
        })?,
        ProbeOutcome::Cancelled => {
            terminate(&mut child, process_group).await;
            stdout_task.abort();
            stderr_task.abort();
            return Err(AdapterError::Cancelled {
                profile: name.to_owned(),
            });
        }
        ProbeOutcome::TimedOut => {
            terminate(&mut child, process_group).await;
            abort_probe_readers(&mut stdout_task, &mut stderr_task).await;
            return Ok(unavailable_probe(
                name,
                ProbeFailure::TimedOut,
                Some(executable),
            ));
        }
    };
    let timeout_result = time::timeout(timeout, async {
        tokio::try_join!(
            join_probe_reader(name, &mut stdout_task),
            join_probe_reader(name, &mut stderr_task)
        )
    })
    .await;
    let Ok(result) = timeout_result else {
        terminate(&mut child, process_group).await;
        abort_probe_readers(&mut stdout_task, &mut stderr_task).await;
        return Ok(unavailable_probe(
            name,
            ProbeFailure::TimedOut,
            Some(executable),
        ));
    };
    let (stdout, stderr) = result?;
    if !status.success() || stdout.overflow || stderr.overflow {
        return Ok(unavailable_probe(
            name,
            ProbeFailure::VersionCommandFailed,
            Some(executable),
        ));
    }
    let version = first_line(&stdout.bytes).or_else(|| first_line(&stderr.bytes));
    Ok(ProbeResult {
        profile: name.to_owned(),
        available: true,
        executable: Some(executable),
        version,
        failure: None,
        checked_at: Utc::now(),
    })
}

fn probe_http(name: &str, credential_env: Option<&str>) -> ProbeResult {
    if let Some(credential_env) = credential_env
        && std::env::var(credential_env).map_or(true, |value| value.is_empty())
    {
        unavailable_probe(
            name,
            ProbeFailure::MissingEnvironment {
                variable: credential_env.to_owned(),
            },
            None,
        )
    } else {
        ProbeResult {
            profile: name.to_owned(),
            available: true,
            executable: None,
            version: None,
            failure: None,
            checked_at: Utc::now(),
        }
    }
}

fn unavailable_probe(name: &str, failure: ProbeFailure, executable: Option<String>) -> ProbeResult {
    ProbeResult {
        profile: name.to_owned(),
        available: false,
        executable,
        version: None,
        failure: Some(failure),
        checked_at: Utc::now(),
    }
}

fn probe_unavailable_error(probe: &ProbeResult) -> AdapterError {
    AdapterError::Unavailable {
        profile: probe.profile.clone(),
        reason: match probe.failure.as_ref() {
            Some(ProbeFailure::Disabled) => "profile is disabled".to_owned(),
            Some(ProbeFailure::ExecutableNotFound) => "executable was not found".to_owned(),
            Some(ProbeFailure::VersionCommandFailed) => {
                "version probe did not complete successfully".to_owned()
            }
            Some(ProbeFailure::MissingEnvironment { variable }) => {
                format!("environment variable {variable} is not set")
            }
            Some(ProbeFailure::TimedOut) => "version probe timed out".to_owned(),
            None => "availability probe failed".to_owned(),
        },
    }
}

#[derive(Debug)]
struct CappedBytes {
    bytes: Vec<u8>,
    overflow: bool,
}

enum ProbeOutcome {
    Finished(std::io::Result<std::process::ExitStatus>),
    Cancelled,
    TimedOut,
}

async fn drain_capped(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
) -> std::io::Result<CappedBytes> {
    let mut bytes = Vec::with_capacity(limit);
    let mut overflow = false;
    let mut buffer = [0_u8; 1024];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let retained = limit.saturating_sub(bytes.len()).min(read);
        bytes.extend_from_slice(&buffer[..retained]);
        overflow |= retained < read;
    }
    Ok(CappedBytes { bytes, overflow })
}

async fn join_probe_reader(
    profile: &str,
    task: &mut tokio::task::JoinHandle<std::io::Result<CappedBytes>>,
) -> Result<CappedBytes, AdapterError> {
    task.await
        .map_err(|error| AdapterError::Unavailable {
            profile: profile.to_owned(),
            reason: format!("version probe reader failed: {error}"),
        })?
        .map_err(|source| {
            AdapterError::probe_io(profile.to_owned(), "reading version probe output", source)
        })
}

fn first_line(bytes: &[u8]) -> Option<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.trim().to_owned())
}

async fn terminate(child: &mut tokio::process::Child, process_group: Option<u32>) {
    #[cfg(unix)]
    {
        if let Some(pgid) = process_group {
            let pgid = format!("-{pgid}");
            let _ = process::Command::new("/bin/kill")
                .arg("-TERM")
                .arg(&pgid)
                .status()
                .await;
            let _ = process::Command::new("/bin/kill")
                .arg("-KILL")
                .arg(&pgid)
                .status()
                .await;
        }
    }
    let _kill_result = child.kill().await;
    let _wait_result = child.wait().await;
}

async fn abort_probe_readers(
    stdout: &mut tokio::task::JoinHandle<std::io::Result<CappedBytes>>,
    stderr: &mut tokio::task::JoinHandle<std::io::Result<CappedBytes>>,
) {
    stdout.abort();
    stderr.abort();
    let _ = stdout.await;
    let _ = stderr.await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        adapter::{AdapterCapability, AdapterOutput},
        config::{CommandProfile, CommandPromptMode},
    };

    fn probe_count(path: &std::path::Path) -> usize {
        std::fs::read_to_string(path)
            .map(|contents| contents.len())
            .unwrap_or(0)
    }

    fn test_profile(argv: Vec<String>, priority: i32) -> Profile {
        let mut command = CommandProfile::new(argv);
        command.prompt_mode = CommandPromptMode::Stdin;
        command.version_args = vec![String::new()];
        Profile {
            enabled: true,
            priority,
            timeout_seconds: None,
            capabilities: AdapterCapabilities::text(),
            kind: ProfileKind::Command(command),
        }
    }

    #[tokio::test]
    async fn explicit_unavailable_profile_never_falls_back() {
        let mut store = ProfileStore::default();
        store
            .insert(
                "missing",
                test_profile(vec!["gloop-command-that-does-not-exist".to_owned()], 100),
            )
            .expect("valid profile");
        store
            .insert(
                "available",
                test_profile(vec!["printf".to_owned(), "%s".to_owned()], 0),
            )
            .expect("valid profile");
        let registry = ProviderRegistry::new(store);
        let error = registry
            .resolve(
                Some("missing"),
                &AdapterCapabilities::text(),
                CancellationToken::new(),
            )
            .await
            .expect_err("explicit unavailable profile must fail");
        assert!(matches!(error, AdapterError::Unavailable { .. }));
    }

    #[tokio::test]
    async fn resolved_probe_results_are_cached_per_profile() {
        let workspace = tempfile::tempdir().expect("temporary workspace");
        let marker = workspace.path().join("probe-count.txt");

        let mut store = ProfileStore::default();
        let mut unavailable = CommandProfile::new(vec!["sh".to_owned()]);
        unavailable.version_args = vec![
            "-c".to_owned(),
            format!("printf fail >> {path}; exit 1", path = marker.display()),
        ];
        let mut available = CommandProfile::new(vec!["sh".to_owned()]);
        available.version_args = vec![
            "-c".to_owned(),
            format!("printf ok >> {path}; printf ok", path = marker.display()),
        ];

        store
            .insert(
                "unavailable",
                Profile {
                    enabled: true,
                    priority: 100,
                    timeout_seconds: None,
                    capabilities: AdapterCapabilities::text(),
                    kind: ProfileKind::Command(unavailable),
                },
            )
            .expect("valid profile");
        store
            .insert(
                "available",
                Profile {
                    enabled: true,
                    priority: 0,
                    timeout_seconds: None,
                    capabilities: AdapterCapabilities::text(),
                    kind: ProfileKind::Command(available),
                },
            )
            .expect("valid profile");

        let registry = ProviderRegistry::new(store);

        let result = registry
            .resolve_with_selection(None, &AdapterCapabilities::text(), CancellationToken::new())
            .await
            .expect("resolve succeeds");
        assert_eq!(result.profile, "available");
        assert_eq!(probe_count(&marker), 6);

        let result = registry
            .resolve_with_selection(None, &AdapterCapabilities::text(), CancellationToken::new())
            .await
            .expect("resolve succeeds");
        assert_eq!(result.profile, "available");
        assert_eq!(probe_count(&marker), 6);

        let error = registry
            .resolve(
                Some("unavailable"),
                &AdapterCapabilities::text(),
                CancellationToken::new(),
            )
            .await
            .expect_err("explicitly unavailable profile fails");
        assert!(matches!(error, AdapterError::Unavailable { .. }));
        assert_eq!(probe_count(&marker), 6);
    }

    #[tokio::test]
    async fn capability_selection_records_origin_and_executes() {
        let mut store = ProfileStore::default();
        store
            .insert(
                "lower",
                test_profile(vec!["printf".to_owned(), "lower".to_owned()], 0),
            )
            .expect("valid profile");
        store
            .insert(
                "higher",
                test_profile(vec!["printf".to_owned(), "higher".to_owned()], 10),
            )
            .expect("valid profile");
        let registry = ProviderRegistry::new(store);
        let result = registry
            .execute(
                None,
                AdapterRequest::new("ignored"),
                CancellationToken::new(),
                None,
            )
            .await
            .expect("provider executes");
        assert_eq!(result.selection.profile, "higher");
        assert_eq!(result.selection.origin, SelectionOrigin::Capability);
        assert_eq!(
            result.response.output,
            AdapterOutput::Text("higher".to_owned())
        );
    }

    #[tokio::test]
    async fn selection_hard_filters_capabilities() {
        let mut store = ProfileStore::default();
        store
            .insert(
                "text-only",
                test_profile(vec!["printf".to_owned(), "ok".to_owned()], 0),
            )
            .expect("valid profile");
        let registry = ProviderRegistry::new(store);
        let required = AdapterCapabilities::new([AdapterCapability::RepositoryWrite]);
        let error = registry
            .resolve(None, &required, CancellationToken::new())
            .await
            .expect_err("capability mismatch must fail");
        assert!(matches!(error, AdapterError::NoMatchingProfile { .. }));
    }

    #[tokio::test]
    async fn probes_executable_version() {
        let mut store = ProfileStore::default();
        let mut command = CommandProfile::new(vec!["rustc".to_owned()]);
        command.version_args = vec!["--version".to_owned()];
        store
            .insert(
                "rust",
                Profile {
                    enabled: true,
                    priority: 0,
                    timeout_seconds: None,
                    capabilities: AdapterCapabilities::text(),
                    kind: ProfileKind::Command(command),
                },
            )
            .expect("valid profile");
        let probe = ProviderRegistry::new(store)
            .probe("rust", CancellationToken::new())
            .await
            .expect("probe succeeds");
        assert!(probe.available);
        assert!(
            probe
                .version
                .is_some_and(|version| version.starts_with("rustc "))
        );
    }

    #[tokio::test]
    async fn probe_command_does_not_inherit_arbitrary_parent_environment() {
        if std::env::var_os("GLOOP_PROVIDER_TEST_CHILD_ENV").is_none() {
            let test_binary = std::env::current_exe().expect("current test binary");
            let result = tokio::process::Command::new(test_binary)
                .arg("probe_command_does_not_inherit_arbitrary_parent_environment_child")
                .arg("--nocapture")
                .env("GLOOP_PROVIDER_TEST_CHILD_ENV", "1")
                .env("GLOOP_TEST_PROBE_NOT_LEAKED", "1")
                .output()
                .await
                .expect("failed to spawn nested test");
            assert!(result.status.success());
            assert!(
                String::from_utf8_lossy(&result.stdout)
                    .contains("REGISTRY_ENVIRONMENT_ISOLATION_PROBE_OK"),
                "nested test marker missing from child output"
            );
            return;
        }
    }

    #[tokio::test]
    async fn probe_command_does_not_inherit_arbitrary_parent_environment_child() {
        let mut store = ProfileStore::default();
        let mut command = CommandProfile::new(vec!["sh".to_owned()]);
        command.version_args = vec![
            "-c".to_owned(),
            "if [ -z \"$GLOOP_TEST_PROBE_NOT_LEAKED\" ]; then printf ok; else printf blocked; fi"
                .to_owned(),
        ];
        let profile = Profile {
            enabled: true,
            priority: 0,
            timeout_seconds: None,
            capabilities: AdapterCapabilities::text(),
            kind: ProfileKind::Command(command),
        };
        store
            .insert("probe".to_owned(), profile)
            .expect("valid profile");
        let probe = ProviderRegistry::new(store)
            .probe("probe", CancellationToken::new())
            .await
            .expect("probe succeeds");
        assert!(probe.available);
        assert_eq!(probe.version.as_deref(), Some("ok"));
        if std::env::var_os("GLOOP_PROVIDER_TEST_CHILD_ENV").is_some() {
            println!("REGISTRY_ENVIRONMENT_ISOLATION_PROBE_OK");
        }
    }

    #[tokio::test]
    async fn probe_command_does_not_leak_mapped_environment_values_into_version() {
        let mut store = ProfileStore::default();
        let mut command = CommandProfile::new(vec!["sh".to_owned()]);
        command.version_args = vec!["-c".to_owned(), "printf \"$PROBE_PATH\"".to_owned()];
        command
            .env_from
            .insert("PROBE_PATH".to_owned(), "PATH".to_owned());
        let profile = Profile {
            enabled: true,
            priority: 0,
            timeout_seconds: None,
            capabilities: AdapterCapabilities::text(),
            kind: ProfileKind::Command(command),
        };
        store
            .insert("probe".to_owned(), profile)
            .expect("valid profile");
        let probe = ProviderRegistry::new(store)
            .probe("probe", CancellationToken::new())
            .await
            .expect("probe succeeds");
        assert!(probe.available);
        assert!(probe.version.is_none());
    }

    #[tokio::test]
    async fn probe_times_out_when_version_command_descendant_keeps_pipe_open() {
        let mut store = ProfileStore::default();
        let directory = tempfile::tempdir().expect("temporary directory");
        let marker = directory.path().join("descendant_pid");
        let marker_path = marker.to_string_lossy().into_owned();
        let mut command = CommandProfile::new(vec!["sh".to_owned()]);
        command.version_args = vec![
            "-c".to_owned(),
            format!("(sleep 2 &) ; echo $! > {marker_path}"),
        ];
        store
            .insert(
                "probe-background",
                Profile {
                    enabled: true,
                    priority: 0,
                    timeout_seconds: None,
                    capabilities: AdapterCapabilities::text(),
                    kind: ProfileKind::Command(command),
                },
            )
            .expect("valid profile");
        let probe = ProviderRegistry::new(store)
            .with_probe_timeout(Duration::from_millis(20))
            .probe("probe-background", CancellationToken::new())
            .await
            .expect("probe succeeds");
        assert!(!probe.available);
        assert!(matches!(probe.failure, Some(ProbeFailure::TimedOut)));
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
    async fn pre_cancelled_probe_never_spawns_version_command() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let marker = directory.path().join("must-not-exist");
        let mut command = CommandProfile::new(vec!["touch".to_owned()]);
        command.version_args = vec![marker.to_string_lossy().into_owned()];
        let mut store = ProfileStore::default();
        store
            .insert(
                "side-effect",
                Profile {
                    enabled: true,
                    priority: 0,
                    timeout_seconds: None,
                    capabilities: AdapterCapabilities::text(),
                    kind: ProfileKind::Command(command),
                },
            )
            .expect("valid profile");
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let error = ProviderRegistry::new(store)
            .probe("side-effect", cancellation)
            .await
            .expect_err("probe must be cancelled before spawn");
        assert!(matches!(error, AdapterError::Cancelled { .. }));
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn unauthenticated_openai_compatible_profile_is_available() {
        let store = ProfileStore::from_toml_str(
            r#"
[profiles.local]
kind = "openai"
base_url = "http://127.0.0.1:11434/v1/"
model = "local-model"
"#,
        )
        .expect("local profile parses");
        let probe = ProviderRegistry::new(store)
            .probe("local", CancellationToken::new())
            .await
            .expect("probe succeeds");
        assert!(probe.available);
        assert!(probe.failure.is_none());
    }

    #[test]
    fn http_error_classification_is_retry_aware() {
        let rate_limit = AdapterError::HttpStatus {
            profile: "test".to_owned(),
            status: 429,
            error_type: None,
            error_code: None,
        };
        assert_eq!(
            rate_limit.class(),
            crate::adapter::AdapterErrorClass::RateLimit
        );
        assert!(rate_limit.is_retryable());
        let authentication = AdapterError::HttpStatus {
            profile: "test".to_owned(),
            status: 401,
            error_type: None,
            error_code: None,
        };
        assert!(!authentication.is_retryable());
    }

    #[test]
    fn probe_io_error_is_retryable() {
        let error = AdapterError::probe_io(
            "probe-test",
            "waiting for version probe",
            std::io::Error::other("simulated probe I/O failure"),
        );
        assert!(error.is_retryable());
    }
}
