use std::{
    collections::{HashMap, HashSet},
    fmt, io,
    path::{Path, PathBuf},
};

use anyhow::Result;
use clap::ValueEnum;
use gloop_core::{
    FinalStatus, Graph, IssueSeverity, NodeKind, NodeStatus, PromptSpec, RunEvent, RunSummary,
    ValidationIssue,
};
use gloop_provider::{
    AdapterCapability, AdapterError, CatalogFamily, CatalogModel, ConfigError, ModelDiscovery,
    PROJECT_CONFIG_PATH, Profile, ProfileKind, ProfileStore, ProviderRegistry,
    catalog_family_for_argv0, discover_models_for_argv0, merge_profile_models,
};
use gloop_runtime::{
    JournalError, JournalRead, LiveRunReport, NodeFailureClass, ProgressEvent, ReplayError,
    ReplayReport, RunError, RunOptions, Runtime, inspect_run, live_run_status, node_failure_class,
    read_journal, replay_events, replay_journal,
};
use schemars::schema_for;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::{
    fs, signal,
    sync::{Semaphore, mpsc},
};
use tokio_util::sync::CancellationToken;
use toml::{Value as TomlValue, map::Map as TomlMap};

use crate::atomic_write::{write_text_atomic, write_text_no_replace};
use crate::gui::{self, GuiTarget};
use crate::i18n::Language;
use crate::templates;
use crate::wizard;

const MAX_PROFILE_TOML_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ExitCode {
    Success = 0,
    BlockedOrHumanGate = 2,
    VerificationFailed = 3,
    AdapterUnavailable = 4,
    BudgetExhausted = 5,
    InvalidGraph = 6,
    UnresolvedProfile = 7,
    Cancelled = 130,
    Internal = 1,
}

impl ExitCode {
    pub const fn as_i32(self) -> i32 {
        self as i32
    }
}

pub struct CommandResult {
    pub code: ExitCode,
    pub output: Option<Value>,
    pub text: Option<String>,
}

impl CommandResult {
    fn success_text(text: impl Into<String>) -> Self {
        Self {
            code: ExitCode::Success,
            output: None,
            text: Some(text.into()),
        }
    }

    fn success_json(value: Value) -> Self {
        Self {
            code: ExitCode::Success,
            output: Some(value),
            text: None,
        }
    }

    fn failure_json(code: ExitCode, message: impl Into<String>, details: Option<Value>) -> Self {
        let mut payload = serde_json::Map::new();
        payload.insert("success".to_owned(), Value::Bool(false));
        payload.insert("error".to_owned(), Value::String(message.into()));
        payload.insert("code".to_owned(), Value::from(code.as_i32()));
        if let Some(details) = details {
            payload.insert("details".to_owned(), details);
        }
        Self {
            code,
            output: Some(Value::Object(payload)),
            text: None,
        }
    }

    pub(crate) fn failure_text(code: ExitCode, message: impl Into<String>) -> Self {
        Self {
            code,
            output: None,
            text: Some(message.into()),
        }
    }
}

#[derive(ValueEnum, Clone, Copy, PartialEq, Eq)]
pub enum GraphTemplateArg {
    Direct,
    PlanImplementVerify,
    ParallelResearchReduce,
    ReviewFixLoop,
    DesignWallBounce,
    Council,
    DecomposeFanoutReduce,
    ImplementTestLoop,
}

impl GraphTemplateArg {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "direct" => Some(Self::Direct),
            "plan-implement-verify" => Some(Self::PlanImplementVerify),
            "parallel-research-reduce" => Some(Self::ParallelResearchReduce),
            "review-fix-loop" => Some(Self::ReviewFixLoop),
            "design-wall-bounce" => Some(Self::DesignWallBounce),
            "council" => Some(Self::Council),
            "decompose-fanout-reduce" => Some(Self::DecomposeFanoutReduce),
            "implement-test-loop" => Some(Self::ImplementTestLoop),
            _ => None,
        }
    }

    fn to_template(self) -> wizard::GraphTemplate {
        match self {
            Self::Direct => wizard::GraphTemplate::Direct,
            Self::PlanImplementVerify => wizard::GraphTemplate::PlanImplementVerify,
            Self::ParallelResearchReduce => wizard::GraphTemplate::ParallelResearchReduce,
            Self::ReviewFixLoop => wizard::GraphTemplate::ReviewFixLoop,
            Self::DesignWallBounce => wizard::GraphTemplate::DesignWallBounce,
            Self::Council => wizard::GraphTemplate::Council,
            Self::DecomposeFanoutReduce => wizard::GraphTemplate::DecomposeFanoutReduce,
            Self::ImplementTestLoop => wizard::GraphTemplate::ImplementTestLoop,
        }
    }
}

#[derive(ValueEnum, Clone)]
pub enum RenderFormat {
    Mermaid,
    Yaml,
    Dot,
}

impl RenderFormat {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Mermaid => "mermaid",
            Self::Yaml => "yaml",
            Self::Dot => "dot",
        }
    }
}

fn split_validation(issues: &[ValidationIssue]) -> (Vec<ValidationIssue>, Vec<ValidationIssue>) {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    for issue in issues {
        match issue.severity {
            IssueSeverity::Error => errors.push(issue.clone()),
            IssueSeverity::Warning => warnings.push(issue.clone()),
        }
    }

    (errors, warnings)
}

fn status_exit_code(final_status: FinalStatus) -> ExitCode {
    match final_status {
        FinalStatus::Blocked => ExitCode::BlockedOrHumanGate,
        FinalStatus::VerificationFailed | FinalStatus::Failed => ExitCode::VerificationFailed,
        FinalStatus::BudgetExhausted => ExitCode::BudgetExhausted,
        FinalStatus::Cancelled => ExitCode::Cancelled,
        FinalStatus::ReadyForHuman => ExitCode::Success,
    }
}

fn summary_exit_code(summary: &RunSummary) -> ExitCode {
    if summary.status != FinalStatus::Failed {
        return status_exit_code(summary.status);
    }

    for node in summary.nodes.values() {
        if node.status != NodeStatus::Failed {
            continue;
        }

        let Some(class) = node_failure_class(node) else {
            return ExitCode::VerificationFailed;
        };

        if matches!(
            class,
            NodeFailureClass::Cancelled | NodeFailureClass::ProviderCancelled
        ) {
            continue;
        }

        match class {
            NodeFailureClass::ProviderProfileNotFound | NodeFailureClass::ProviderCapability => {
                return ExitCode::UnresolvedProfile;
            }
            NodeFailureClass::ProviderUnavailable
            | NodeFailureClass::ProviderAuthentication
            | NodeFailureClass::ProviderRateLimit
            | NodeFailureClass::ProviderTransient
            | NodeFailureClass::ProviderTimeout
            | NodeFailureClass::ProviderContextLength
            | NodeFailureClass::ProviderProtocol
            | NodeFailureClass::ProviderConfiguration
            | NodeFailureClass::ProviderProcess => return ExitCode::AdapterUnavailable,
            NodeFailureClass::HumanGate => return ExitCode::BlockedOrHumanGate,
            NodeFailureClass::Budget => return ExitCode::BudgetExhausted,
            NodeFailureClass::Execution | NodeFailureClass::Verification => {
                return ExitCode::VerificationFailed;
            }
            NodeFailureClass::Cancelled | NodeFailureClass::ProviderCancelled => {}
        }
    }

    ExitCode::VerificationFailed
}

async fn validate_journal_file_path(journal_path: &std::path::Path) -> io::Result<()> {
    let run_dir = journal_path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "journal path has no run directory: {}",
                journal_path.display()
            ),
        )
    })?;
    let mut protected_directories = vec![run_dir];
    if let Some(runs_dir) = run_dir
        .parent()
        .filter(|path| path.file_name().and_then(|name| name.to_str()) == Some("runs"))
    {
        protected_directories.push(runs_dir);
        if let Some(gloop_dir) = runs_dir
            .parent()
            .filter(|path| path.file_name().and_then(|name| name.to_str()) == Some(".gloop"))
        {
            protected_directories.push(gloop_dir);
        }
    }
    for directory in protected_directories {
        let metadata = fs::symlink_metadata(directory).await?;
        if metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("journal directory is a symlink: {}", directory.display()),
            ));
        }
        if !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("journal parent is not a directory: {}", directory.display()),
            ));
        }
    }

    let metadata = fs::symlink_metadata(journal_path).await?;
    if metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("journal path is a symlink: {}", journal_path.display()),
        ));
    }
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "journal path is not a regular file: {}",
                journal_path.display()
            ),
        ));
    }
    Ok(())
}

pub(crate) fn apply_profile_to_agent_nodes(graph: &mut Graph, profile: &str) {
    for node in &mut graph.spec.nodes {
        match &mut node.kind {
            NodeKind::Agent {
                profile: existing, ..
            }
            | NodeKind::Reduce {
                profile: existing, ..
            }
            | NodeKind::Synthesize {
                profile: existing, ..
            } => {
                *existing = Some(profile.to_owned());
            }
            _ => {}
        }
    }
}

pub(crate) fn apply_model_to_agent_nodes(graph: &mut Graph, model: &str) {
    for node in &mut graph.spec.nodes {
        match &mut node.kind {
            NodeKind::Agent {
                model: existing, ..
            }
            | NodeKind::Reduce {
                model: existing, ..
            }
            | NodeKind::Synthesize {
                model: existing, ..
            } => *existing = Some(model.to_owned()),
            _ => {}
        }
    }
}

pub(crate) fn clear_model_on_agent_nodes(graph: &mut Graph) {
    for node in &mut graph.spec.nodes {
        match &mut node.kind {
            NodeKind::Agent {
                model: existing, ..
            }
            | NodeKind::Reduce {
                model: existing, ..
            }
            | NodeKind::Synthesize {
                model: existing, ..
            } => *existing = None,
            _ => {}
        }
    }
}

fn provider_store_error(message: impl fmt::Display) -> CommandResult {
    CommandResult::failure_json(
        ExitCode::Internal,
        "failed to load provider config",
        Some(json!({"error": message.to_string()})),
    )
}

fn load_profiles_for_repo(
    repo: &Path,
    trust_project_profiles: bool,
) -> Result<ProfileStore, ConfigError> {
    if trust_project_profiles {
        ProfileStore::load_trusted_project(repo)
    } else {
        ProfileStore::load(repo)
    }
}

fn profile_names_in_file(path: &Path) -> HashSet<String> {
    let Ok(source) = std::fs::read_to_string(path) else {
        return HashSet::new();
    };
    let Ok(value) = source.parse::<TomlValue>() else {
        return HashSet::new();
    };
    value
        .get("profiles")
        .and_then(TomlValue::as_table)
        .map(|table| table.keys().cloned().collect())
        .unwrap_or_default()
}

fn profile_kind_name(kind: &ProfileKind) -> &'static str {
    match kind {
        ProfileKind::Command(_) => "command",
        ProfileKind::OpenAi(_) => "openai",
        ProfileKind::Anthropic(_) => "anthropic",
    }
}

fn profile_default_model(kind: &ProfileKind) -> Option<String> {
    match kind {
        ProfileKind::OpenAi(profile) => Some(profile.model.clone()),
        ProfileKind::Anthropic(profile) => Some(profile.model.clone()),
        ProfileKind::Command(_) => None,
    }
}

pub fn build_profile_choices(
    repo: &Path,
    trust_project_profiles: bool,
) -> Result<Vec<wizard::ProfileChoice>, ConfigError> {
    let store = load_profiles_for_repo(repo, trust_project_profiles)?;
    let project_names = if trust_project_profiles {
        profile_names_in_file(&repo.join(PROJECT_CONFIG_PATH))
    } else {
        HashSet::new()
    };
    let user_names = ProfileStore::user_config_path()
        .as_deref()
        .map(profile_names_in_file)
        .unwrap_or_default();

    let mut choices = store
        .iter()
        .map(|(name, profile)| {
            let source = if project_names.contains(name) {
                wizard::ProfileSource::Project
            } else if user_names.contains(name) {
                wizard::ProfileSource::User
            } else {
                wizard::ProfileSource::Builtin
            };
            wizard::ProfileChoice {
                name: name.to_owned(),
                kind: profile_kind_name(&profile.kind).to_owned(),
                source,
                enabled: profile.enabled,
                default_model: profile_default_model(&profile.kind),
            }
        })
        .collect::<Vec<_>>();
    choices.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(choices)
}

pub(crate) async fn resolve_profile_options(
    repo: &Path,
    trust_project_profiles: bool,
    choices: &[wizard::ProfileChoice],
) -> Result<Vec<gui::ProfileOption>, ConfigError> {
    let store = load_profiles_for_repo(repo, trust_project_profiles)?;
    let runtime_default = store
        .iter()
        .filter(|(_, profile)| {
            profile.enabled && profile.capabilities.contains(AdapterCapability::TextOutput)
        })
        .fold(
            None,
            |selected: Option<(&str, i32)>, (name, profile)| match selected {
                Some((_, priority)) if priority >= profile.priority => selected,
                _ => Some((name, profile.priority)),
            },
        )
        .map(|(name, _)| name.to_owned());
    let mut cache: HashMap<(CatalogFamily, String), ModelDiscovery> = HashMap::new();
    let mut jobs = Vec::new();
    for choice in choices {
        let Some(profile) = store.get(&choice.name) else {
            continue;
        };
        if !choice.enabled {
            continue;
        }
        let ProfileKind::Command(command) = &profile.kind else {
            continue;
        };
        if command.model_args.is_empty() {
            continue;
        }
        let Some(argv0) = command.argv.first() else {
            continue;
        };
        let Some(family) = catalog_family_for_argv0(argv0) else {
            continue;
        };
        let key = (family, argv0.clone());
        if !cache.contains_key(&key) && !jobs.iter().any(|job| job == &key) {
            jobs.push(key);
        }
    }
    let mut pending_discoveries = Vec::new();
    for (family, argv0) in jobs {
        pending_discoveries.push(tokio::spawn(async move {
            let discovery = discover_models_for_argv0(&argv0).await;
            ((family, argv0), discovery)
        }));
    }
    for handle in pending_discoveries {
        let (key, discovery) = handle.await.map_err(|error| ConfigError::InvalidProfile {
            profile: "model-discovery".to_owned(),
            message: format!("task failed: {error}"),
        })?;
        cache.insert(key, discovery);
    }

    Ok(choices
        .iter()
        .map(|choice| {
            let default_model = choice.default_model.clone();
            let (models, discovery, discovery_error) = match store.get(&choice.name) {
                Some(profile) => profile_model_catalog(profile, &cache, default_model.as_deref()),
                None => (
                    merge_profile_models(default_model.as_deref(), &[], &[]),
                    "unsupported".to_owned(),
                    None,
                ),
            };
            gui::ProfileOption {
                name: choice.name.clone(),
                kind: choice.kind.clone(),
                enabled: choice.enabled,
                runtime_default: runtime_default.as_deref() == Some(choice.name.as_str()),
                default_model,
                models,
                discovery,
                discovery_error,
            }
        })
        .collect())
}

fn profile_model_catalog(
    profile: &Profile,
    cache: &HashMap<(CatalogFamily, String), ModelDiscovery>,
    default_model: Option<&str>,
) -> (Vec<CatalogModel>, String, Option<String>) {
    match &profile.kind {
        ProfileKind::OpenAi(openai) => (
            merge_profile_models(Some(&openai.model), &[], &[]),
            "unsupported".to_owned(),
            None,
        ),
        ProfileKind::Anthropic(anthropic) => (
            merge_profile_models(Some(&anthropic.model), &[], &[]),
            "unsupported".to_owned(),
            None,
        ),
        ProfileKind::Command(command) => {
            if command.model_args.is_empty() {
                return (
                    merge_profile_models(default_model, &[], &[]),
                    "unsupported".to_owned(),
                    None,
                );
            }
            let Some(argv0) = command.argv.first() else {
                return (
                    merge_profile_models(default_model, &[], &[]),
                    "unsupported".to_owned(),
                    None,
                );
            };
            let Some(family) = catalog_family_for_argv0(argv0) else {
                return (
                    merge_profile_models(default_model, &[], &[]),
                    "unsupported".to_owned(),
                    None,
                );
            };
            match cache.get(&(family, argv0.clone())) {
                Some(ModelDiscovery::Listed(discovered)) => (
                    merge_profile_models(default_model, discovered, &[]),
                    "listed".to_owned(),
                    None,
                ),
                Some(ModelDiscovery::Failed { reason }) => (
                    merge_profile_models(default_model, &[], &[]),
                    "failed".to_owned(),
                    Some(reason.clone()),
                ),
                Some(ModelDiscovery::Unsupported) | None => (
                    merge_profile_models(default_model, &[], &[]),
                    "unsupported".to_owned(),
                    None,
                ),
            }
        }
    }
}

fn run_error_code(error: &RunError) -> ExitCode {
    match error {
        RunError::InvalidParallelism
        | RunError::CurrentDirectory { .. }
        | RunError::CurrentDirectoryNotDirectory(_)
        | RunError::Graph(_) => ExitCode::InvalidGraph,
        RunError::SchedulerInvariant(_)
        | RunError::Worktree(_)
        | RunError::WorktreeManagerUnavailable { .. }
        | RunError::Artifact(_)
        | RunError::Journal(_)
        | RunError::Io(_) => ExitCode::Internal,
    }
}

fn provider_probe_exit_code(error: &AdapterError) -> ExitCode {
    match error {
        AdapterError::ProfileNotFound(_)
        | AdapterError::NoMatchingProfile { .. }
        | AdapterError::CapabilityMismatch { .. } => ExitCode::UnresolvedProfile,
        AdapterError::Disabled { .. }
        | AdapterError::MissingCredential { .. }
        | AdapterError::Unavailable { .. }
        | AdapterError::Spawn { .. }
        | AdapterError::ProcessFailed { .. }
        | AdapterError::Timeout { .. } => ExitCode::AdapterUnavailable,
        _ => ExitCode::Internal,
    }
}

fn inspect_error_code(error: &ReplayError) -> ExitCode {
    use gloop_runtime::replay::ReplayError;
    match error {
        ReplayError::SummaryStatusMismatch { .. } => ExitCode::VerificationFailed,
        ReplayError::Io(_) | ReplayError::Journal(JournalError::Io(_)) => ExitCode::Internal,
        _ => ExitCode::InvalidGraph,
    }
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::fn_params_excessive_bools
)]
pub async fn run_foreground(
    goal: Option<String>,
    graph_path: Option<PathBuf>,
    profile: Option<String>,
    model: Option<String>,
    repo: PathBuf,
    json_mode: bool,
    dry_run: bool,
    non_interactive: bool,
    max_parallel: Option<usize>,
    trust_project_profiles: bool,
    interactive: bool,
    run_id: Option<String>,
) -> CommandResult {
    let mut generated = false;
    let graph = if let Some(path) = graph_path {
        match Graph::from_path(&path) {
            Ok(graph) => graph,
            Err(error) => {
                return CommandResult::failure_json(
                    ExitCode::InvalidGraph,
                    "failed to load graph",
                    Some(json!({"path": path, "error": error.to_string()})),
                );
            }
        }
    } else {
        generated = true;
        if interactive {
            let profiles = match build_profile_choices(&repo, trust_project_profiles) {
                Ok(profiles) => profiles,
                Err(error) => return provider_store_error(error),
            };
            match wizard::interactive_graph(&profiles) {
                Ok(graph) => graph,
                Err(error) => {
                    return CommandResult::failure_json(
                        ExitCode::Internal,
                        "interactive graph construction failed",
                        Some(json!({"error": error.to_string()})),
                    );
                }
            }
        } else if let Some(goal) = goal {
            wizard::request_graph("run", goal.clone(), goal)
        } else if non_interactive {
            return CommandResult::failure_json(
                ExitCode::InvalidGraph,
                "run requires a goal or --graph when --non-interactive is set",
                None,
            );
        } else {
            let profiles = match build_profile_choices(&repo, trust_project_profiles) {
                Ok(profiles) => profiles,
                Err(error) => return provider_store_error(error),
            };
            match wizard::interactive_graph(&profiles) {
                Ok(graph) => graph,
                Err(error) => {
                    return CommandResult::failure_json(
                        ExitCode::Internal,
                        "interactive graph construction failed",
                        Some(json!({"error": error.to_string()})),
                    );
                }
            }
        }
    };

    let mut graph = graph;
    let cli_max_parallel = if let Some(max_parallel) = max_parallel {
        let max_parallel_limit = Semaphore::MAX_PERMITS;
        if max_parallel == 0 {
            return CommandResult::failure_json(
                ExitCode::InvalidGraph,
                "max-parallel must be at least one",
                None,
            );
        }
        if max_parallel > max_parallel_limit {
            return CommandResult::failure_json(
                ExitCode::InvalidGraph,
                "max-parallel exceeds runtime limit",
                Some(json!({
                    "max_parallel": max_parallel,
                    "max_limit": max_parallel_limit,
                })),
            );
        }
        Some(max_parallel)
    } else {
        None
    };
    let effective_max_parallel = cli_max_parallel
        .map_or(graph.spec.policies.max_parallel, |value| {
            graph.spec.policies.max_parallel.min(value)
        });

    if generated && let Some(profile) = profile.as_deref() {
        apply_profile_to_agent_nodes(&mut graph, profile);
    }
    if generated && let Some(model) = model.as_deref() {
        apply_model_to_agent_nodes(&mut graph, model);
    }

    let issues = graph.validate();
    let (errors, warnings) = split_validation(&issues);
    if !errors.is_empty() {
        return CommandResult::failure_json(
            ExitCode::InvalidGraph,
            "graph validation failed",
            Some(json!({"errors": errors, "warnings": warnings})),
        );
    }

    if dry_run {
        let serialized = match graph.to_yaml() {
            Ok(text) => text,
            Err(error) => {
                return CommandResult::failure_json(
                    ExitCode::Internal,
                    "failed to serialize graph",
                    Some(json!({"error": error.to_string()})),
                );
            }
        };

        if json_mode {
            return CommandResult::success_json(json!({
                "success": true,
                "dry_run": true,
                "goal": graph.spec.goal,
                "profile": profile.unwrap_or_else(|| "default".to_owned()),
                "model": model,
                "max_parallel": graph.spec.policies.max_parallel,
                "effective_max_parallel": effective_max_parallel,
                "graph": graph.metadata.name,
                "yaml": serialized,
            }));
        }

        return CommandResult::success_text(format!(
            "dry-run ready\ngraph: {}\nmax_parallel: {}\neffective_max_parallel: {}\nprofile: {}\nmodel: {}",
            graph.metadata.name,
            graph.spec.policies.max_parallel,
            effective_max_parallel,
            profile.unwrap_or_else(|| "default".to_owned()),
            model.unwrap_or_else(|| "provider default".to_owned()),
        ));
    }

    let registry = match load_profiles_for_repo(&repo, trust_project_profiles) {
        Ok(store) => ProviderRegistry::new(store),
        Err(error) => return provider_store_error(error),
    };

    let artifact_root = repo.join(PROJECT_CONFIG_PATH).with_file_name("runs");
    let run_id = match run_id {
        Some(id) => {
            if id.is_empty()
                || id == "."
                || id == ".."
                || id.contains(['/', '\\'])
                || id.len() > 128
            {
                return CommandResult::failure_json(
                    ExitCode::InvalidGraph,
                    "invalid run id",
                    Some(json!({"run_id": id})),
                );
            }
            id
        }
        None => ulid::Ulid::new().to_string().to_ascii_lowercase(),
    };
    if !json_mode {
        eprintln!(
            "run: {run_id}\nrun dir: {}",
            artifact_root.join(&run_id).display()
        );
    }
    let runtime = Runtime::new(registry, artifact_root);

    let show_progress = !json_mode;
    let (progress, progress_task) = if show_progress {
        let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<ProgressEvent>();
        let task = tokio::spawn(async move {
            while let Some(event) = progress_rx.recv().await {
                eprintln!("{}", format_progress_event(&event));
            }
        });
        (Some(progress_tx), Some(task))
    } else {
        (None, None)
    };

    let cancellation = CancellationToken::new();
    let signal_task = tokio::spawn({
        let cancellation = cancellation.clone();
        async move {
            let _ = signal::ctrl_c().await;
            cancellation.cancel();
        }
    });

    let options = RunOptions {
        current_dir: repo,
        max_parallel: cli_max_parallel,
        cancellation,
        progress,
        run_id: Some(run_id),
        ..RunOptions::default()
    };

    let summary = runtime.run(&graph, options).await;
    signal_task.abort();
    if let Some(task) = progress_task {
        let _ = task.await;
    }

    match summary {
        Ok(summary) => {
            let exit_code = summary_exit_code(&summary);
            if json_mode {
                CommandResult {
                    code: exit_code,
                    output: Some(json!({
                        "success": summary.status == FinalStatus::ReadyForHuman,
                        "summary": summary,
                    })),
                    text: None,
                }
            } else {
                if !warnings.is_empty() {
                    eprintln!("warnings: {}", warnings.len());
                    for issue in warnings {
                        eprintln!("warning: {}", issue.message);
                    }
                }

                CommandResult {
                    code: exit_code,
                    output: None,
                    text: Some(format_run_summary(&summary)),
                }
            }
        }
        Err(error) => {
            let code = run_error_code(&error);
            CommandResult::failure_json(
                code,
                "run execution failed",
                Some(json!({"error": error.to_string()})),
            )
        }
    }
}

fn has_unsupported_interactive_seeds(
    template: &str,
    request: Option<&str>,
    provider_profiles: Option<&str>,
    loop_cap: Option<u32>,
) -> bool {
    template != "direct" || request.is_some() || provider_profiles.is_some() || loop_cap.is_some()
}

fn parse_provider_profiles(provider_profiles: Option<String>) -> Option<Vec<String>> {
    provider_profiles.map(|value| {
        value
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>()
    })
}

fn graph_from_resolved_template(
    resolved: templates::ResolvedTemplate,
    name: String,
    goal: String,
    request: Option<String>,
    provider_profiles: Option<String>,
    loop_cap: Option<u32>,
) -> Result<Graph, CommandResult> {
    match resolved {
        templates::ResolvedTemplate::Builtin(builtin_name) => {
            let template = GraphTemplateArg::from_name(builtin_name).expect("builtin template");
            Ok(wizard::template_graph(
                name,
                goal,
                template.to_template(),
                request,
                parse_provider_profiles(provider_profiles),
                loop_cap,
            ))
        }
        templates::ResolvedTemplate::Project(mut graph) => {
            if request.is_some() {
                return Err(CommandResult::failure_json(
                    ExitCode::InvalidGraph,
                    "--request is not supported for saved project templates; edit the template YAML or use a built-in template",
                    None,
                ));
            }
            if provider_profiles.is_some() {
                return Err(CommandResult::failure_json(
                    ExitCode::InvalidGraph,
                    "--provider-profiles is not supported for saved project templates; edit the template YAML or use a built-in template",
                    None,
                ));
            }
            if loop_cap.is_some() {
                return Err(CommandResult::failure_json(
                    ExitCode::InvalidGraph,
                    "--loop-cap is not supported for saved project templates; edit the template YAML or use a built-in template",
                    None,
                ));
            }
            templates::apply_new_overrides(&mut graph, &name, &goal);
            Ok(*graph)
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::fn_params_excessive_bools
)]
pub async fn graph_new(
    name: String,
    goal: String,
    template: String,
    repo: PathBuf,
    request: Option<String>,
    provider_profiles: Option<String>,
    loop_cap: Option<u32>,
    interactive: bool,
    path: Option<PathBuf>,
    force: bool,
    json_mode: bool,
    trust_project_profiles: bool,
) -> CommandResult {
    if interactive
        && has_unsupported_interactive_seeds(
            &template,
            request.as_deref(),
            provider_profiles.as_deref(),
            loop_cap,
        )
    {
        return CommandResult::failure_json(
            ExitCode::InvalidGraph,
            "--interactive does not accept --template, --request, --provider-profiles, or --loop-cap; name and goal are used as prompt defaults",
            None,
        );
    }

    let graph = if interactive {
        let profiles = match build_profile_choices(&repo, trust_project_profiles) {
            Ok(profiles) => profiles,
            Err(error) => return provider_store_error(error),
        };
        let persist = path
            .as_ref()
            .map_or(wizard::EditorPersistTarget::None, |output| {
                wizard::EditorPersistTarget::GraphFile {
                    path: output.clone(),
                    force,
                }
            });
        match wizard::interactive_graph_with_seed(
            Some(name.as_str()),
            Some(goal.as_str()),
            &profiles,
            &persist,
        ) {
            Ok(graph) => graph,
            Err(error) => {
                return CommandResult::failure_json(
                    ExitCode::Internal,
                    "interactive graph construction failed",
                    Some(json!({"error": error.to_string()})),
                );
            }
        }
    } else {
        let resolved = match templates::resolve_template(&template, &repo) {
            Ok(resolved) => resolved,
            Err(error) => {
                return CommandResult::failure_json(ExitCode::InvalidGraph, error.message(), None);
            }
        };

        match graph_from_resolved_template(
            resolved,
            name,
            goal,
            request,
            provider_profiles,
            loop_cap,
        ) {
            Ok(graph) => graph,
            Err(result) => return result,
        }
    };

    let issues = graph.validate();
    let (errors, warnings) = split_validation(&issues);
    if !errors.is_empty() {
        return CommandResult::failure_json(
            ExitCode::InvalidGraph,
            "generated graph validation failed",
            Some(json!({"errors": errors, "warnings": warnings})),
        );
    }

    let yaml = match graph.to_yaml() {
        Ok(yaml) => yaml,
        Err(error) => {
            return CommandResult::failure_json(
                ExitCode::Internal,
                "failed serialize generated graph",
                Some(json!({"error": error.to_string()})),
            );
        }
    };

    if let Some(path) = path {
        if path.exists() && !force && !interactive {
            return CommandResult::failure_json(
                ExitCode::InvalidGraph,
                "output path exists. use --force to overwrite",
                Some(json!({"path": path})),
            );
        }

        if !interactive {
            let write_result = if force {
                write_text_atomic(path.as_path(), &yaml).await
            } else {
                write_text_no_replace(path.as_path(), &yaml).await
            };
            if let Err(error) = write_result {
                if !force && error.kind() == io::ErrorKind::AlreadyExists {
                    return CommandResult::failure_json(
                        ExitCode::InvalidGraph,
                        "output path exists. use --force to overwrite",
                        Some(json!({"path": path})),
                    );
                }
                return CommandResult::failure_json(
                    ExitCode::Internal,
                    "failed to write graph",
                    Some(json!({"path": path, "error": error.to_string()})),
                );
            }
        }

        if json_mode {
            CommandResult::success_json(json!({
                "success": true,
                "written": path,
                "warnings": warnings,
                "validation": issues,
            }))
        } else {
            if !warnings.is_empty() {
                eprintln!("warnings: {}", warnings.len());
            }
            CommandResult::success_text(format!("wrote graph to {}", path.display()))
        }
    } else if json_mode {
        CommandResult::success_json(json!({
            "success": true,
            "yaml": yaml,
            "warnings": warnings,
            "validation": issues,
        }))
    } else {
        if !warnings.is_empty() {
            eprintln!("warnings: {}", warnings.len());
        }
        CommandResult::success_text(yaml)
    }
}

fn gui_init_starts_blank(name: Option<&str>, from: Option<&str>, request: Option<&str>) -> bool {
    name.is_none() && from.is_none() && request.is_none()
}

fn gui_init_goal(request: Option<&str>, starts_blank: bool) -> String {
    if starts_blank {
        String::new()
    } else {
        request
            .unwrap_or(templates::DEFAULT_TEMPLATE_GOAL)
            .to_owned()
    }
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::fn_params_excessive_bools
)]
pub async fn graph_init(
    name: Option<String>,
    from: Option<String>,
    description: Option<String>,
    request: Option<String>,
    provider_profiles: Option<String>,
    loop_cap: Option<u32>,
    list: bool,
    force: bool,
    repo: PathBuf,
    json_mode: bool,
    trust_project_profiles: bool,
    gui_mode: bool,
    language: Language,
) -> CommandResult {
    if gui_mode {
        if name.is_some() != from.is_some() {
            return CommandResult::failure_json(
                ExitCode::InvalidGraph,
                "--gui graph init requires both --name and --from, or neither",
                None,
            );
        }
        let profiles = match build_profile_choices(&repo, trust_project_profiles) {
            Ok(profiles) => profiles,
            Err(error) => return provider_store_error(error),
        };
        let initial_name = name.clone().unwrap_or_else(|| "my-workflow".to_owned());
        let initial_template = from
            .as_deref()
            .and_then(GraphTemplateArg::from_name)
            .unwrap_or(GraphTemplateArg::Direct);
        if from.is_some()
            && GraphTemplateArg::from_name(from.as_deref().unwrap_or_default()).is_none()
        {
            return CommandResult::failure_json(
                ExitCode::InvalidGraph,
                format!(
                    "unknown built-in template base '{}'",
                    from.as_deref().unwrap_or_default()
                ),
                None,
            );
        }
        let starts_blank =
            gui_init_starts_blank(name.as_deref(), from.as_deref(), request.as_deref());
        let initial_goal = gui_init_goal(request.as_deref(), starts_blank);
        let mut graph = wizard::template_graph(
            initial_name,
            initial_goal,
            initial_template.to_template(),
            request,
            parse_provider_profiles(provider_profiles),
            loop_cap,
        );
        if starts_blank && let Some(node) = graph.spec.nodes.first_mut() {
            node.label = Some(match language {
                Language::En => "Configure this step".to_owned(),
                Language::Ja => "処理内容を設定".to_owned(),
            });
            if let NodeKind::Agent { prompt, .. } = &mut node.kind {
                *prompt = PromptSpec::Inline(String::new());
            }
        }
        if let Some(description) = description {
            graph.metadata.description = Some(description);
        }
        let profile_options =
            match resolve_profile_options(&repo, trust_project_profiles, &profiles).await {
                Ok(options) => options,
                Err(error) => return provider_store_error(error),
            };
        return graph_gui(
            graph,
            profile_options,
            GuiTarget::ProjectTemplate {
                repo,
                force,
                saved_name: None,
                expected_sha256: None,
            },
            language,
            json_mode,
        )
        .await;
    }

    if list {
        let entries = match templates::list_all_templates(&repo) {
            Ok(entries) => entries,
            Err(error) => {
                return CommandResult::failure_json(
                    ExitCode::Internal,
                    "failed to list graph templates",
                    Some(json!({"error": error})),
                );
            }
        };

        if json_mode {
            let payload = entries
                .iter()
                .map(|entry| {
                    json!({
                        "name": entry.name,
                        "source": match entry.source {
                            templates::TemplateSource::Builtin => "builtin",
                            templates::TemplateSource::Project => "project",
                        },
                        "description": entry.description,
                    })
                })
                .collect::<Vec<_>>();
            return CommandResult::success_json(json!({
                "success": true,
                "templates": payload,
            }));
        }

        let mut lines = Vec::new();
        for entry in &entries {
            let source = match (language, entry.source) {
                (Language::En, templates::TemplateSource::Builtin) => "builtin",
                (Language::En, templates::TemplateSource::Project) => "project",
                (Language::Ja, templates::TemplateSource::Builtin) => "組み込み",
                (Language::Ja, templates::TemplateSource::Project) => "プロジェクト",
            };
            if let Some(description) = localized_template_description(entry, language) {
                lines.push(format!("{source} {name}: {description}", name = entry.name));
            } else {
                lines.push(format!("{source} {name}", name = entry.name));
            }
        }
        return CommandResult::success_text(lines.join("\n"));
    }

    if language == Language::Ja && name.is_none() && from.is_none() {
        eprintln!(
            "TUI is English-only. Use --gui for the Japanese editor. / TUIは英語のみです。日本語エディタは --gui を使ってください。"
        );
    }
    if !(name.is_some() && from.is_some())
        && (description.is_some()
            || request.is_some()
            || provider_profiles.is_some()
            || loop_cap.is_some())
    {
        return CommandResult::failure_json(
            ExitCode::InvalidGraph,
            "interactive graph init does not accept --description, --request, --provider-profiles, or --loop-cap; provide both --name and --from for non-interactive authoring",
            None,
        );
    }

    let interactive_authoring = name.is_none() && from.is_none();

    let graph = if let (Some(template_name), Some(base)) = (name.as_ref(), from.as_ref()) {
        if let Err(error) = templates::validate_init_template_name(template_name) {
            return CommandResult::failure_json(ExitCode::InvalidGraph, error, None);
        }

        let Some(builtin) = GraphTemplateArg::from_name(base) else {
            return CommandResult::failure_json(
                ExitCode::InvalidGraph,
                format!("unknown built-in template base '{base}'"),
                None,
            );
        };

        let mut graph = wizard::template_graph(
            template_name,
            templates::DEFAULT_TEMPLATE_GOAL,
            builtin.to_template(),
            request,
            parse_provider_profiles(provider_profiles),
            loop_cap,
        );
        if let Some(description) = description {
            graph.metadata.description = Some(description);
        }
        graph
    } else if name.is_some() || from.is_some() {
        return CommandResult::failure_json(
            ExitCode::InvalidGraph,
            "non-interactive graph init requires both --name and --from",
            None,
        );
    } else {
        let profiles = match build_profile_choices(&repo, trust_project_profiles) {
            Ok(profiles) => profiles,
            Err(error) => return provider_store_error(error),
        };
        match wizard::interactive_template_init(&profiles, &repo, force) {
            Ok(graph) => graph,
            Err(error) => {
                return CommandResult::failure_json(
                    ExitCode::Internal,
                    "interactive template authoring failed",
                    Some(json!({"error": error.to_string()})),
                );
            }
        }
    };

    let template_name = if let Some(name) = name {
        name
    } else {
        graph.metadata.name.clone()
    };

    if let Err(error) =
        templates::ensure_managed_directory(&repo, Path::new(templates::TEMPLATES_DIR))
    {
        return CommandResult::failure_json(
            ExitCode::InvalidGraph,
            "project template directory is not safe",
            Some(json!({"error": error.to_string()})),
        );
    }
    let destination = templates::template_path(&repo, &template_name);

    let issues = graph.validate();
    let (errors, warnings) = split_validation(&issues);
    if !errors.is_empty() {
        return CommandResult::failure_json(
            ExitCode::InvalidGraph,
            "template graph validation failed",
            Some(json!({"errors": errors, "warnings": warnings})),
        );
    }

    if !interactive_authoring {
        let yaml = match graph.to_yaml() {
            Ok(yaml) => yaml,
            Err(error) => {
                return CommandResult::failure_json(
                    ExitCode::Internal,
                    "failed serialize template graph",
                    Some(json!({"error": error.to_string()})),
                );
            }
        };

        let write_result = if force {
            write_text_atomic(destination.as_path(), &yaml).await
        } else {
            write_text_no_replace(destination.as_path(), &yaml).await
        };
        if let Err(error) = write_result {
            if !force && error.kind() == io::ErrorKind::AlreadyExists {
                return CommandResult::failure_json(
                    ExitCode::InvalidGraph,
                    "template path exists. use --force to overwrite",
                    Some(json!({"path": destination})),
                );
            }
            return CommandResult::failure_json(
                ExitCode::Internal,
                "failed to write template",
                Some(json!({"path": destination, "error": error.to_string()})),
            );
        }
    }

    let usage = format!("gloop graph new workflow.yaml --template {template_name}");

    if json_mode {
        CommandResult::success_json(json!({
            "success": true,
            "written": destination,
            "usage": usage,
            "warnings": warnings,
            "validation": issues,
        }))
    } else {
        if !warnings.is_empty() {
            eprintln!("warnings: {}", warnings.len());
        }
        CommandResult::success_text(format!(
            "saved template to {}\nusage: {}",
            destination.display(),
            usage
        ))
    }
}

#[derive(Debug, Serialize)]
struct GraphCatalogEntry {
    name: String,
    graph_name: Option<String>,
    source: String,
    path: Option<String>,
    description: Option<String>,
    goal: Option<String>,
    node_count: Option<usize>,
    edge_count: Option<usize>,
    status: String,
    errors: Vec<String>,
    warnings: Vec<String>,
}

pub async fn graph_list(repo: PathBuf, language: Language, json_mode: bool) -> CommandResult {
    let templates = match templates::list_all_templates(&repo) {
        Ok(entries) => entries,
        Err(error) => {
            return CommandResult::failure_json(
                ExitCode::Internal,
                "failed to list graph templates",
                Some(json!({"error": error})),
            );
        }
    };
    let graph_files = match templates::list_graph_files(&repo) {
        Ok(paths) => paths,
        Err(error) => {
            return CommandResult::failure_json(
                ExitCode::Internal,
                "failed to search graph files",
                Some(json!({"error": error})),
            );
        }
    };

    let template_entries = graph_template_catalog_entries(&repo, templates, language, json_mode);
    let graph_entries = graph_files
        .iter()
        .map(|path| catalog_entry_from_path(repo.as_path(), path, "file".to_owned(), None))
        .collect::<Vec<_>>();

    if json_mode {
        return CommandResult::success_json(json!({
            "success": true,
            "repo": repo,
            "templates": template_entries,
            "graphs": graph_entries,
        }));
    }

    let mut lines = vec![match language {
        Language::En => "Templates (use a name with 'graph edit NAME --gui'):".to_owned(),
        Language::Ja => "テンプレート（名前を 'graph edit 名前 --gui' に渡せます）:".to_owned(),
    }];
    if template_entries.is_empty() {
        lines.push(match language {
            Language::En => "  (none)".to_owned(),
            Language::Ja => "  （なし）".to_owned(),
        });
    } else {
        lines.extend(
            template_entries
                .iter()
                .map(|entry| format_catalog_entry(entry, language)),
        );
    }
    lines.push(String::new());
    lines.push(match language {
        Language::En => "Saved graph files:".to_owned(),
        Language::Ja => "保存済みグラフファイル:".to_owned(),
    });
    if graph_entries.is_empty() {
        lines.push(match language {
            Language::En => "  (none — try 'gloop graph init --gui')".to_owned(),
            Language::Ja => "  （なし — 'gloop graph init --gui' を試してください）".to_owned(),
        });
    } else {
        lines.extend(
            graph_entries
                .iter()
                .map(|entry| format_catalog_entry(entry, language)),
        );
    }
    CommandResult::success_text(lines.join("\n"))
}

fn graph_template_catalog_entries(
    repo: &Path,
    entries: Vec<templates::TemplateEntry>,
    language: Language,
    json_mode: bool,
) -> Vec<GraphCatalogEntry> {
    entries
        .into_iter()
        .map(|entry| {
            let description = if json_mode {
                entry.description.clone()
            } else {
                localized_template_description(&entry, language)
            };
            match entry.source {
                templates::TemplateSource::Builtin => {
                    let graph = GraphTemplateArg::from_name(&entry.name).map(|template| {
                        wizard::template_graph(
                            entry.name.clone(),
                            templates::DEFAULT_TEMPLATE_GOAL,
                            template.to_template(),
                            None,
                            None,
                            None,
                        )
                    });
                    catalog_entry_from_graph(
                        graph,
                        entry.name,
                        "builtin".to_owned(),
                        None,
                        description,
                    )
                }
                templates::TemplateSource::Project => catalog_entry_from_path(
                    repo,
                    &templates::template_path(repo, &entry.name),
                    "template".to_owned(),
                    description,
                ),
            }
        })
        .collect()
}

fn catalog_entry_from_path(
    repo: &Path,
    path: &Path,
    source: String,
    description: Option<String>,
) -> GraphCatalogEntry {
    let name = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("unknown")
        .to_owned();
    let display_path = Some(display_catalog_path(repo, path));
    match Graph::from_path(path) {
        Ok(graph) => catalog_entry_from_graph(Some(graph), name, source, display_path, description),
        Err(error) => GraphCatalogEntry {
            name,
            graph_name: None,
            source,
            path: display_path,
            description,
            goal: None,
            node_count: None,
            edge_count: None,
            status: "invalid".to_owned(),
            errors: vec![error.to_string()],
            warnings: Vec::new(),
        },
    }
}

fn catalog_entry_from_graph(
    graph: Option<Graph>,
    fallback_name: String,
    source: String,
    path: Option<String>,
    description: Option<String>,
) -> GraphCatalogEntry {
    let Some(graph) = graph else {
        return GraphCatalogEntry {
            name: fallback_name,
            graph_name: None,
            source,
            path,
            description,
            goal: None,
            node_count: None,
            edge_count: None,
            status: "invalid".to_owned(),
            errors: vec!["unknown built-in graph template".to_owned()],
            warnings: Vec::new(),
        };
    };
    let issues = graph.validate();
    let errors = issues
        .iter()
        .filter(|issue| issue.severity == IssueSeverity::Error)
        .map(|issue| format!("[{}] {}", issue.code, issue.message))
        .collect::<Vec<_>>();
    let warnings = issues
        .iter()
        .filter(|issue| issue.severity == IssueSeverity::Warning)
        .map(|issue| format!("[{}] {}", issue.code, issue.message))
        .collect::<Vec<_>>();
    let graph_name = graph.metadata.name;
    GraphCatalogEntry {
        name: graph_name.clone(),
        graph_name: Some(graph_name.clone()),
        source,
        path,
        description: description.or(graph.metadata.description),
        goal: Some(graph.spec.goal),
        node_count: Some(graph.spec.nodes.len()),
        edge_count: Some(graph.spec.edges.len()),
        status: if errors.is_empty() {
            "valid".to_owned()
        } else {
            "invalid".to_owned()
        },
        errors,
        warnings,
    }
}

fn display_catalog_path(repo: &Path, path: &Path) -> String {
    path.strip_prefix(repo).map_or_else(
        |_| path.display().to_string(),
        |relative| relative.display().to_string(),
    )
}

fn format_catalog_entry(entry: &GraphCatalogEntry, language: Language) -> String {
    let status = match (language, entry.status.as_str()) {
        (Language::En, "valid") => "ok",
        (Language::En, _) => "check",
        (Language::Ja, "valid") => "OK",
        (Language::Ja, _) => "要確認",
    };
    let source = match (language, entry.source.as_str()) {
        (Language::En, "builtin") => "built-in",
        (Language::En, "template") => "project template",
        (Language::En, _) => "file",
        (Language::Ja, "builtin") => "組み込み",
        (Language::Ja, "template") => "プロジェクトテンプレート",
        (Language::Ja, _) => "ファイル",
    };
    let location = entry
        .path
        .as_deref()
        .map_or_else(String::new, |path| format!(" — {path}"));
    let description = entry
        .description
        .as_deref()
        .map_or_else(String::new, |description| match language {
            Language::En => format!(" — {description}"),
            Language::Ja => format!(" — 説明: {description}"),
        });
    let goal = entry
        .goal
        .as_deref()
        .map_or_else(String::new, |goal| match language {
            Language::En => format!(" — goal: {goal}"),
            Language::Ja => format!(" — 目的: {goal}"),
        });
    let shape = match (entry.node_count, entry.edge_count, language) {
        (Some(nodes), Some(edges), Language::En) => format!(" — {nodes} nodes, {edges} edges"),
        (Some(nodes), Some(edges), Language::Ja) => format!(" — ノード{nodes}、エッジ{edges}"),
        _ => String::new(),
    };
    let detail = entry
        .errors
        .first()
        .map_or_else(String::new, |error| format!(" — {error}"));
    let metadata_name = entry
        .graph_name
        .as_deref()
        .filter(|name| *name != entry.name)
        .map_or_else(String::new, |name| match language {
            Language::En => format!(" — saved name: {name}"),
            Language::Ja => format!(" — 保存名: {name}"),
        });
    format!(
        "  [{status}] {source} {name}{location}{metadata_name}{description}{goal}{shape}{detail}",
        name = entry.name,
    )
}

fn localized_template_description(
    entry: &templates::TemplateEntry,
    language: Language,
) -> Option<String> {
    if entry.source == templates::TemplateSource::Builtin && language == Language::Ja {
        let description = match entry.name.as_str() {
            "direct" => "1つのエージェントに作業を頼む",
            "plan-implement-verify" => "計画 → 実装 → 検証",
            "parallel-research-reduce" => "並列で調べてからまとめる",
            "review-fix-loop" => "レビューと修正を回数制限つきで繰り返す",
            "design-wall-bounce" => "2人の設計者が互いの案を壁打ちして統合する",
            "council" => "ブラインド設計 → 統合 → 実装 → パネルレビュー → 統合判定",
            "decompose-fanout-reduce" => "タスク分解 → 軽量ワーカー並列実行 → 統合",
            "implement-test-loop" => "実装後、テストが通るまで検証/修正を回数限定で反復",
            _ => return entry.description.clone(),
        };
        return Some(description.to_owned());
    }
    entry.description.clone()
}

#[derive(Debug)]
struct GraphEditSource {
    graph: Graph,
    path: PathBuf,
    expected_sha256: Option<String>,
    create_only: bool,
}

fn resolve_graph_edit_source(
    target: &Path,
    repo: &Path,
    update_mode: bool,
) -> Result<GraphEditSource, String> {
    if !update_mode && target.is_file() {
        return Ok(GraphEditSource {
            graph: Graph::from_path(target).map_err(|error| error.to_string())?,
            path: target.to_path_buf(),
            expected_sha256: Some(
                gui::file_sha256(&target.to_path_buf()).map_err(|error| error.to_string())?,
            ),
            create_only: false,
        });
    }

    let name = target
        .to_str()
        .ok_or_else(|| "graph target must be valid UTF-8".to_owned())?;
    templates::validate_template_lookup_name(name)
        .map_err(|error| format!("'{name}' is not a graph file or template name: {error}"))?;

    if !templates::is_builtin_template_name(name) {
        let matches = templates::list_graph_files(repo)?
            .into_iter()
            .filter_map(|path| {
                Graph::from_path(&path)
                    .ok()
                    .filter(|graph| graph.metadata.name == name)
                    .map(|graph| (path, graph))
            })
            .collect::<Vec<_>>();
        if matches.len() > 1 {
            return Err(format!(
                "more than one saved graph has the name '{name}'; edit the file path shown by 'graph list'"
            ));
        }
        if let Some((path, graph)) = matches.into_iter().next() {
            return Ok(GraphEditSource {
                graph,
                expected_sha256: Some(gui::file_sha256(&path).map_err(|error| error.to_string())?),
                path,
                create_only: false,
            });
        }
    }

    let resolved = templates::resolve_template(name, repo).map_err(|error| error.message())?;
    match resolved {
        templates::ResolvedTemplate::Project(graph) => {
            let path = templates::confined_template_path(repo, name)
                .map_err(|error| error.message())?
                .ok_or_else(|| format!("project template '{name}' does not have a file"))?;
            Ok(GraphEditSource {
                graph: *graph,
                expected_sha256: Some(gui::file_sha256(&path).map_err(|error| error.to_string())?),
                path,
                create_only: false,
            })
        }
        templates::ResolvedTemplate::Builtin(builtin_name) => {
            if update_mode {
                return Err(format!(
                    "built-in template '{builtin_name}' is not a saved template; use 'graph edit {builtin_name}' to create an editable graph, or 'graph init --name NAME --from {builtin_name}' for a reusable template"
                ));
            }
            templates::ensure_managed_directory(repo, Path::new(templates::GRAPHS_DIR))
                .map_err(|error| error.to_string())?;
            let path = templates::graph_path(repo, builtin_name);
            if path.is_file() {
                return Ok(GraphEditSource {
                    graph: Graph::from_path(&path).map_err(|error| error.to_string())?,
                    expected_sha256: Some(
                        gui::file_sha256(&path).map_err(|error| error.to_string())?,
                    ),
                    path,
                    create_only: false,
                });
            }
            let template = GraphTemplateArg::from_name(builtin_name)
                .ok_or_else(|| format!("unknown built-in graph template '{builtin_name}'"))?;
            Ok(GraphEditSource {
                graph: wizard::template_graph(
                    builtin_name,
                    templates::DEFAULT_TEMPLATE_GOAL,
                    template.to_template(),
                    None,
                    None,
                    None,
                ),
                path,
                expected_sha256: None,
                create_only: true,
            })
        }
    }
}

#[allow(clippy::fn_params_excessive_bools, clippy::too_many_arguments)]
pub async fn graph_edit(
    target: PathBuf,
    repo: PathBuf,
    gui_mode: bool,
    language: Language,
    json_mode: bool,
    update_mode: bool,
    trust_project_profiles: bool,
) -> CommandResult {
    let source = match resolve_graph_edit_source(&target, &repo, update_mode) {
        Ok(source) => source,
        Err(error) => {
            let message = format!(
                "failed to load graph for editing: {error}\nTry 'gloop graph list' to see available names and files."
            );
            if !json_mode {
                return CommandResult::failure_text(ExitCode::InvalidGraph, message);
            }
            return CommandResult::failure_json(
                ExitCode::InvalidGraph,
                "failed to load graph for editing",
                Some(json!({
                    "target": target,
                    "error": error,
                    "hint": "use 'gloop graph list' to see available names and files",
                })),
            );
        }
    };
    let profiles = match build_profile_choices(&repo, trust_project_profiles) {
        Ok(profiles) => profiles,
        Err(error) => return provider_store_error(error),
    };

    if gui_mode {
        let profile_options =
            match resolve_profile_options(&repo, trust_project_profiles, &profiles).await {
                Ok(options) => options,
                Err(error) => return provider_store_error(error),
            };
        return graph_gui(
            source.graph,
            profile_options,
            GuiTarget::GraphFile {
                path: source.path,
                expected_sha256: source.expected_sha256,
                create_only: source.create_only,
            },
            language,
            json_mode,
        )
        .await;
    }

    if language == Language::Ja {
        eprintln!(
            "TUI is English-only. Use --gui for the Japanese editor. / TUIは英語のみです。日本語エディタは --gui を使ってください。"
        );
    }
    let persist = wizard::EditorPersistTarget::GraphFile {
        path: source.path.clone(),
        force: !source.create_only,
    };
    let edited = match wizard::interactive_edit_graph(source.graph, &profiles, &persist) {
        Ok(graph) => graph,
        Err(error) => {
            return CommandResult::failure_json(
                ExitCode::BlockedOrHumanGate,
                "interactive graph editing did not save",
                Some(json!({"error": error.to_string()})),
            );
        }
    };
    let issues = edited.validate();
    let (_, warnings) = split_validation(&issues);
    if json_mode {
        CommandResult::success_json(json!({
            "success": true,
            "written": source.path,
            "warnings": warnings,
            "validation": issues,
        }))
    } else {
        CommandResult::success_text(format!("saved graph to {}", source.path.display()))
    }
}

async fn graph_gui(
    graph: Graph,
    profiles: Vec<gui::ProfileOption>,
    target: GuiTarget,
    language: Language,
    json_mode: bool,
) -> CommandResult {
    let result =
        match tokio::task::spawn_blocking(move || gui::launch(graph, &profiles, target, language))
            .await
        {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => {
                return CommandResult::failure_json(
                    ExitCode::Internal,
                    "graph GUI session failed",
                    Some(json!({"error": error.to_string()})),
                );
            }
            Err(error) => {
                return CommandResult::failure_json(
                    ExitCode::Internal,
                    "graph GUI session stopped unexpectedly",
                    Some(json!({"error": error.to_string()})),
                );
            }
        };
    let issues = result.graph.validate();
    let (_, warnings) = split_validation(&issues);
    if json_mode {
        CommandResult::success_json(json!({
            "success": true,
            "saved": result.written.is_some(),
            "written": result.written,
            "warnings": warnings,
            "validation": issues,
        }))
    } else {
        match result.written {
            Some(path) => CommandResult::success_text(format!("saved graph to {}", path.display())),
            None => CommandResult::success_text("closed graph editor without saving"),
        }
    }
}

pub async fn graph_validate(path: PathBuf, json_mode: bool) -> CommandResult {
    let graph = match Graph::from_path(&path) {
        Ok(graph) => graph,
        Err(error) => {
            return CommandResult::failure_json(
                ExitCode::InvalidGraph,
                "failed to load graph",
                Some(json!({"path": path, "error": error.to_string()})),
            );
        }
    };

    let issues = graph.validate();
    let (errors, warnings) = split_validation(&issues);
    let success = errors.is_empty();

    if json_mode {
        let output = json!({
            "success": success,
            "graph": graph.metadata.name,
            "errors": errors,
            "warnings": warnings,
            "validation_count": issues.len(),
        });
        CommandResult {
            code: if success {
                ExitCode::Success
            } else {
                ExitCode::InvalidGraph
            },
            output: Some(output),
            text: None,
        }
    } else if success {
        CommandResult::success_text(format!("Graph {} is valid", graph.metadata.name))
    } else {
        let detail = format!("{} validation errors", issues.len());
        CommandResult::failure_text(ExitCode::InvalidGraph, detail)
    }
}

pub async fn graph_explain(path: PathBuf, json_mode: bool) -> CommandResult {
    let graph = match Graph::from_path(&path) {
        Ok(graph) => graph,
        Err(error) => {
            return CommandResult::failure_json(
                ExitCode::InvalidGraph,
                "failed to load graph",
                Some(json!({"path": path, "error": error.to_string()})),
            );
        }
    };

    let compiled = match graph.compile() {
        Ok(compiled) => compiled,
        Err(error) => {
            return CommandResult::failure_json(
                ExitCode::InvalidGraph,
                "graph compilation failed",
                Some(json!({"error": error.to_string()})),
            );
        }
    };

    let explanation = compiled.explain();

    if json_mode {
        CommandResult::success_json(json!({
            "success": true,
            "graph": graph.metadata.name,
            "explanation": explanation,
        }))
    } else {
        CommandResult::success_text(explanation)
    }
}

pub async fn graph_render(path: PathBuf, format: RenderFormat, json_mode: bool) -> CommandResult {
    let graph = match Graph::from_path(&path) {
        Ok(graph) => graph,
        Err(error) => {
            return CommandResult::failure_json(
                ExitCode::InvalidGraph,
                "failed to load graph",
                Some(json!({"path": path, "error": error.to_string()})),
            );
        }
    };

    let rendered = match format {
        RenderFormat::Yaml => match graph.to_yaml() {
            Ok(value) => value,
            Err(error) => {
                return CommandResult::failure_json(
                    ExitCode::Internal,
                    "failed render YAML",
                    Some(json!({"error": error.to_string()})),
                );
            }
        },
        RenderFormat::Mermaid => match graph.compile() {
            Ok(compiled) => compiled.render_mermaid(),
            Err(error) => {
                return CommandResult::failure_json(
                    ExitCode::InvalidGraph,
                    "graph compilation failed",
                    Some(json!({"error": error.to_string()})),
                );
            }
        },
        RenderFormat::Dot => match graph.compile() {
            Ok(compiled) => compiled.render_dot(),
            Err(error) => {
                return CommandResult::failure_json(
                    ExitCode::InvalidGraph,
                    "graph compilation failed",
                    Some(json!({"error": error.to_string()})),
                );
            }
        },
    };

    if json_mode {
        CommandResult::success_json(json!({
            "success": true,
            "graph": graph.metadata.name,
            "format": format.as_str(),
            "value": rendered,
        }))
    } else {
        CommandResult::success_text(rendered)
    }
}

pub fn graph_schema(json_mode: bool) -> CommandResult {
    let schema = schema_for!(Graph);
    if json_mode {
        CommandResult::success_json(json!(schema))
    } else {
        match serde_json::to_string_pretty(&schema) {
            Ok(text) => CommandResult::success_text(text),
            Err(error) => CommandResult::failure_text(
                ExitCode::Internal,
                format!("failed serialize graph schema: {error}"),
            ),
        }
    }
}

pub async fn provider_list(json_mode: bool, trust_project_profiles: bool) -> CommandResult {
    let profile_store = match load_profiles_for_repo(Path::new("."), trust_project_profiles) {
        Ok(store) => store,
        Err(error) => return provider_store_error(error),
    };

    let profiles = profile_store
        .iter()
        .map(|(name, profile)| {
            json!({
                "name": name,
                "enabled": profile.enabled,
                "priority": profile.priority,
                "capabilities": profile.capabilities,
            })
        })
        .collect::<Vec<_>>();

    if json_mode {
        CommandResult::success_json(json!({
            "success": true,
            "profiles": profiles,
            "count": profiles.len(),
        }))
    } else {
        let lines = profiles
            .iter()
            .map(|profile| profile["name"].as_str().unwrap_or("unknown"))
            .collect::<Vec<_>>()
            .join(", ");
        CommandResult::success_text(format!("profiles: {lines}"))
    }
}

pub async fn provider_probe(
    profile: String,
    json_mode: bool,
    trust_project_profiles: bool,
) -> CommandResult {
    if profile.trim().is_empty() {
        return CommandResult::failure_json(
            ExitCode::InvalidGraph,
            "provider profile name cannot be empty",
            None,
        );
    }

    let registry = match load_profiles_for_repo(Path::new("."), trust_project_profiles) {
        Ok(store) => ProviderRegistry::new(store),
        Err(error) => return provider_store_error(error),
    };

    let token = CancellationToken::new();
    match registry.probe(&profile, token).await {
        Ok(result) if result.available => {
            if json_mode {
                CommandResult::success_json(json!({
                    "success": true,
                    "profile": profile,
                    "probe": result,
                }))
            } else {
                CommandResult::success_text(format!(
                    "profile '{profile}' is available ({})",
                    result.version.clone().unwrap_or_else(|| "ok".to_owned())
                ))
            }
        }
        Ok(result) => CommandResult::failure_json(
            ExitCode::AdapterUnavailable,
            "provider adapter is unavailable",
            Some(json!({"profile": profile, "probe": result})),
        ),
        Err(error) => CommandResult::failure_json(
            provider_probe_exit_code(&error),
            "provider probe failed",
            Some(json!({"profile": profile, "error": error.to_string()})),
        ),
    }
}

#[allow(clippy::too_many_lines)]
pub async fn provider_add(
    profile: String,
    definition: String,
    json_mode: bool,
    trust_project_profiles: bool,
) -> CommandResult {
    if profile.trim().is_empty() {
        return CommandResult::failure_json(
            ExitCode::InvalidGraph,
            "provider profile name cannot be empty",
            None,
        );
    }
    if definition.trim().is_empty() {
        return CommandResult::failure_json(
            ExitCode::InvalidGraph,
            "provider definition cannot be empty",
            None,
        );
    }

    let parsed_profile: Profile = match toml::from_str(&definition) {
        Ok(profile) => profile,
        Err(error) => {
            return CommandResult::failure_json(
                ExitCode::InvalidGraph,
                "invalid provider definition",
                Some(json!({"profile": profile, "error": error.to_string()})),
            );
        }
    };

    if let Err(error) = parsed_profile.validate(&profile) {
        return CommandResult::failure_json(
            ExitCode::InvalidGraph,
            "invalid provider definition",
            Some(json!({"profile": profile, "error": error.to_string()})),
        );
    }

    let mut root = match load_profiles_file(".").await {
        Ok(root) => root,
        Err(error) => {
            return CommandResult::failure_json(
                ExitCode::Internal,
                "failed to read provider config",
                Some(json!({"path": PROJECT_CONFIG_PATH, "error": error.to_string()})),
            );
        }
    };

    let profile_value = match toml::Value::try_from(parsed_profile) {
        Ok(value) => value,
        Err(error) => {
            return CommandResult::failure_json(
                ExitCode::Internal,
                "invalid provider definition",
                Some(json!({"profile": profile, "error": error.to_string()})),
            );
        }
    };

    let Some(table) = root.as_table_mut() else {
        return CommandResult::failure_json(
            ExitCode::Internal,
            "profiles file format invalid",
            Some(json!({"path": PROJECT_CONFIG_PATH})),
        );
    };

    let profiles = table
        .entry("profiles".to_string())
        .or_insert_with(|| TomlValue::Table(TomlMap::new()));
    let Some(profiles) = profiles.as_table_mut() else {
        return CommandResult::failure_json(
            ExitCode::Internal,
            "profiles file format invalid",
            Some(json!({"path": PROJECT_CONFIG_PATH})),
        );
    };

    profiles.insert(profile.clone(), profile_value);

    let output = match toml::to_string_pretty(&root) {
        Ok(value) => value,
        Err(error) => {
            return CommandResult::failure_json(
                ExitCode::Internal,
                "failed serialize provider config",
                Some(json!({"path": PROJECT_CONFIG_PATH, "error": error.to_string()})),
            );
        }
    };

    let path = Path::new(".").join(PROJECT_CONFIG_PATH);
    if let Err(error) = write_text_atomic(&path, &output).await {
        return CommandResult::failure_json(
            ExitCode::Internal,
            "failed write provider config",
            Some(json!({"path": path, "error": error.to_string()})),
        );
    }

    if json_mode {
        CommandResult::success_json(json!({
            "success": true,
            "profile": profile,
            "path": PROJECT_CONFIG_PATH,
            "written": true,
            "project_profiles_enabled": trust_project_profiles,
        }))
    } else {
        let suffix = if trust_project_profiles {
            String::new()
        } else {
            " (project profiles are currently disabled; use --trust-project-profiles)".to_owned()
        };
        CommandResult::success_text(format!(
            "added profile '{profile}' to {PROJECT_CONFIG_PATH}{suffix}"
        ))
    }
}

pub async fn provider_doctor(json_mode: bool, trust_project_profiles: bool) -> CommandResult {
    let registry = match load_profiles_for_repo(Path::new("."), trust_project_profiles) {
        Ok(store) => ProviderRegistry::new(store),
        Err(error) => return provider_store_error(error),
    };

    let project_names = if trust_project_profiles {
        profile_names_in_file(Path::new(PROJECT_CONFIG_PATH))
    } else {
        HashSet::new()
    };
    let user_names = ProfileStore::user_config_path()
        .as_deref()
        .map(profile_names_in_file)
        .unwrap_or_default();
    let builtin_profiles = ProfileStore::builtins();
    let builtin_names = builtin_profiles.names().collect::<HashSet<_>>();

    let mut checks = Vec::new();
    let mut all_ok = true;
    let token = CancellationToken::new();

    for (name, _) in registry.profiles().iter() {
        match registry.probe(name, token.clone()).await {
            Ok(result) => {
                let configured = project_names.contains(name) || user_names.contains(name);
                let required = configured || !builtin_names.contains(name);
                if !result.available && required {
                    all_ok = false;
                }
                checks
                    .push(json!({"profile": name, "probe": result, "available": result.available}));
            }
            Err(error) => {
                all_ok = false;
                checks
                    .push(json!({"profile": name, "available": false, "error": error.to_string()}));
            }
        }
    }

    if json_mode {
        CommandResult::success_json(json!({
            "success": true,
            "healthy": all_ok,
            "checks": checks,
            "total": checks.len(),
        }))
    } else {
        let available = checks
            .iter()
            .filter(|check| check["available"].as_bool() == Some(true))
            .count();
        CommandResult::success_text(format!(
            "doctor checks: {} entries ({available} available, {} unavailable)",
            checks.len(),
            checks.len().saturating_sub(available),
        ))
    }
}

async fn load_profiles_file(path: &str) -> std::io::Result<TomlValue> {
    let full = Path::new(path).join(PROJECT_CONFIG_PATH);

    match fs::metadata(&full).await {
        Ok(metadata) if metadata.len() > MAX_PROFILE_TOML_BYTES => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "provider config file is too large: {} bytes (max {MAX_PROFILE_TOML_BYTES})",
                metadata.len()
            ),
        )),
        Ok(_) => match fs::read_to_string(&full).await {
            Ok(source) => toml::from_str::<TomlValue>(&source)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error)),
            Err(error) => Err(error),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(TomlValue::Table(TomlMap::new()))
        }
        Err(error) => Err(error),
    }
}

pub async fn inspect_run_at(path: PathBuf, json_mode: bool) -> CommandResult {
    match inspect_run(&path).await {
        Ok(inspection) => {
            let code = summary_exit_code(&inspection.summary);
            if json_mode {
                CommandResult {
                    code,
                    output: Some(json!({
                        "success": code == ExitCode::Success,
                        "inspection": inspection,
                    })),
                    text: None,
                }
            } else {
                CommandResult {
                    code,
                    output: None,
                    text: Some(format_inspection_summary(
                        &inspection.summary,
                        &inspection.replay,
                    )),
                }
            }
        }
        Err(error) => {
            let code = inspect_error_code(&error);
            if json_mode {
                CommandResult::failure_json(
                    code,
                    "failed inspect run",
                    Some(json!({"path": path, "error": error.to_string()})),
                )
            } else {
                CommandResult::failure_text(
                    code,
                    format!("failed inspect run at {}: {error}", path.display()),
                )
            }
        }
    }
}

pub async fn replay_run(path: PathBuf, json_mode: bool) -> CommandResult {
    let journal = path.join("journal.jsonl");
    match replay_journal(&journal).await {
        Ok(report) => {
            let code = report
                .final_status
                .map_or(ExitCode::Internal, status_exit_code);

            if json_mode {
                CommandResult {
                    code,
                    output: Some(json!({
                        "success": code == ExitCode::Success,
                        "replay": report,
                    })),
                    text: None,
                }
            } else {
                CommandResult {
                    code,
                    output: None,
                    text: Some(format_replay_report(&report)),
                }
            }
        }
        Err(error) => {
            if json_mode {
                CommandResult::failure_json(
                    ExitCode::InvalidGraph,
                    "failed to replay run journal",
                    Some(json!({"path": journal, "error": error.to_string()})),
                )
            } else {
                CommandResult::failure_text(
                    ExitCode::InvalidGraph,
                    format!(
                        "failed to replay run journal at '{}': {error}",
                        journal.display()
                    ),
                )
            }
        }
    }
}

#[allow(clippy::too_many_lines)]
pub async fn run_logs(path: PathBuf, json_mode: bool) -> CommandResult {
    let journal_path = path.join("journal.jsonl");
    if let Err(error) = validate_journal_file_path(&journal_path).await {
        return if json_mode {
            CommandResult::failure_json(
                ExitCode::InvalidGraph,
                "failed to read journal",
                Some(json!({"path": journal_path, "error": error.to_string()})),
            )
        } else {
            CommandResult::failure_text(
                ExitCode::InvalidGraph,
                format!(
                    "failed to read journal at '{}': {error}",
                    journal_path.display()
                ),
            )
        };
    }
    match read_journal(&journal_path).await {
        Ok(JournalRead {
            events,
            truncated_tail,
        }) => {
            if truncated_tail {
                return CommandResult::failure_json(
                    ExitCode::InvalidGraph,
                    "journal is incomplete",
                    Some(json!({
                        "path": journal_path,
                        "error": "journal ends with an incomplete row",
                    })),
                );
            }

            if let Err(error) = replay_events(&events) {
                let message = match &error {
                    ReplayError::RunDidNotFinish => "journal is incomplete",
                    _ => "journal appears to be corrupted or tampered",
                };
                if json_mode {
                    return CommandResult::failure_json(
                        ExitCode::InvalidGraph,
                        message,
                        Some(json!({"path": journal_path, "error": error.to_string()})),
                    );
                }
                return CommandResult::failure_text(
                    ExitCode::InvalidGraph,
                    match &error {
                        ReplayError::RunDidNotFinish => {
                            format!(
                                "journal at '{}' appears incomplete: {error}",
                                journal_path.display()
                            )
                        }
                        _ => format!(
                            "journal at '{}' appears corrupted or tampered: {error}",
                            journal_path.display()
                        ),
                    },
                );
            }

            if json_mode {
                CommandResult::success_json(json!({
                    "success": true,
                    "events": events,
                    "count": events.len(),
                }))
            } else if events.is_empty() {
                CommandResult::success_text("no events")
            } else {
                let output = events.iter().map(event_line).collect::<Vec<_>>().join("\n");
                CommandResult::success_text(output)
            }
        }
        Err(error) => {
            let message = match &error {
                JournalError::InvalidLine { .. }
                | JournalError::EmptyLine { .. }
                | JournalError::BrokenChain { .. }
                | JournalError::HashMismatch { .. } => {
                    "journal appears to be corrupted or tampered"
                }
                _ => "failed to read journal",
            };
            if json_mode {
                CommandResult::failure_json(
                    ExitCode::InvalidGraph,
                    message,
                    Some(json!({"path": journal_path, "error": error.to_string()})),
                )
            } else {
                CommandResult::failure_text(
                    ExitCode::InvalidGraph,
                    match &error {
                        JournalError::InvalidLine { .. }
                        | JournalError::EmptyLine { .. }
                        | JournalError::BrokenChain { .. }
                        | JournalError::HashMismatch { .. } => {
                            format!(
                                "journal at '{}' appears corrupted: {error}",
                                journal_path.display()
                            )
                        }
                        _ => format!(
                            "failed to read journal at '{}': {error}",
                            journal_path.display()
                        ),
                    },
                )
            }
        }
    }
}

/// Resolve one run directory under `<repo>/.gloop/runs`. Without a run id
/// the directory with the newest modification time wins. Name order is not
/// usable: run ids may be user-chosen (`--run-id my-task`) and would sort
/// against the ULID default ids.
fn resolve_run_dir(runs_root: &Path, run_id: Option<&str>) -> Result<PathBuf, String> {
    let Some(id) = run_id else {
        let entries = std::fs::read_dir(runs_root)
            .map_err(|error| format!("failed to read {}: {error}", runs_root.display()))?;
        let newest = entries
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.file_name())
            .filter_map(|name| {
                let path = runs_root.join(&name);
                let metadata = path.symlink_metadata().ok()?;
                if !metadata.is_dir() {
                    return None;
                }
                let modified = metadata.modified().ok()?;
                Some((modified, name))
            })
            .max();
        return newest
            .map(|(_, name)| runs_root.join(name))
            .ok_or_else(|| format!("no runs found under {}", runs_root.display()));
    };
    if id.is_empty() || id == "." || id == ".." || id.contains(['/', '\\']) {
        return Err(format!("invalid run id: {id}"));
    }
    let dir = runs_root.join(id);
    let is_real_dir = dir
        .symlink_metadata()
        .map(|metadata| metadata.is_dir())
        .unwrap_or(false);
    if is_real_dir {
        Ok(dir)
    } else {
        Err(format!(
            "run '{id}' was not found under {}",
            runs_root.display()
        ))
    }
}

fn truncate_chars(text: &str, limit: usize) -> String {
    let count = text.chars().count();
    if count <= limit {
        return text.to_owned();
    }
    let kept: String = text.chars().take(limit).collect();
    format!("{kept}\u{2026}(+{} chars)", count - limit)
}

fn truncate_json_strings(value: Value, limit: usize) -> Value {
    match value {
        Value::String(text) => Value::String(truncate_chars(&text, limit)),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| truncate_json_strings(item, limit))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, item)| (key, truncate_json_strings(item, limit)))
                .collect(),
        ),
        other => other,
    }
}

fn status_payload(report: &LiveRunReport, run_dir: &Path) -> Value {
    let nodes: Vec<Value> = report
        .journal
        .nodes
        .iter()
        .map(|(id, outcome)| {
            json!({
                "id": id,
                "status": outcome.status,
                "attempts": outcome.attempts,
                "profile": outcome.profile,
                "model": outcome.model,
                "error": outcome.error,
                "duration_ms": outcome.duration_ms,
                "output": outcome
                    .output
                    .as_ref()
                    .map(|output| truncate_json_strings(output.clone(), 400)),
                "output_artifact": outcome.output_artifact,
            })
        })
        .collect();
    let tail: Vec<Value> = report
        .events_tail
        .iter()
        .map(|event| {
            json!({
                "sequence": event.sequence,
                "timestamp": event.timestamp,
                "kind": event.kind,
                "node": event.node_id,
                "attempt": event.attempt,
                "message": event.message.as_deref().map(|message| truncate_chars(message, 160)),
            })
        })
        .collect();
    json!({
        "run_id": report.run_id,
        "run_dir": run_dir,
        "phase": report.phase(),
        "finished": report.finished(),
        "final_status": report.final_status(),
        "graph_name": report.graph_name,
        "goal": report.goal,
        "started_at": report.started_at,
        "last_event_at": report.last_event_at,
        "last_event_age_ms": report.last_event_age_ms,
        "truncated_tail": report.truncated_tail,
        "event_count": report.journal.event_count,
        "nodes": nodes,
        "events_tail": tail,
        "summary": report.summary.as_ref().map(|summary| {
            truncate_json_strings(
                serde_json::to_value(summary).unwrap_or(Value::Null),
                400,
            )
        }),
    })
}

#[allow(clippy::too_many_lines)]
fn format_status_text(report: &LiveRunReport, run_dir: &Path, language: Language) -> String {
    let (phase_label, graph_label, events_label, nodes_label, tail_label, result_label, age_label) =
        match language {
            Language::En => (
                "phase",
                "graph",
                "events",
                "nodes",
                "tail",
                "result",
                "last event",
            ),
            Language::Ja => (
                "\u{9032}\u{884c}",
                "\u{30b0}\u{30e9}\u{30d5}",
                "\u{30a4}\u{30d9}\u{30f3}\u{30c8}",
                "\u{30ce}\u{30fc}\u{30c9}",
                "\u{76f4}\u{8fd1}",
                "\u{7d50}\u{679c}",
                "\u{6700}\u{7d42}\u{30a4}\u{30d9}\u{30f3}\u{30c8}",
            ),
        };
    let phase = if report.finished() {
        match report.final_status() {
            Some(status) => format!("finished ({status:?})"),
            None => "finished".to_owned(),
        }
    } else {
        report.phase().to_owned()
    };
    let mut out = String::new();
    let _ = fmt::Write::write_fmt(
        &mut out,
        format_args!(
            "run:     {}\n{}:   {}\ndir:     {}\n",
            report.run_id,
            phase_label,
            phase,
            run_dir.display()
        ),
    );
    if let (Some(name), Some(goal)) = (&report.graph_name, &report.goal) {
        let goal_line = goal.lines().next().unwrap_or_default();
        let _ = fmt::Write::write_fmt(
            &mut out,
            format_args!(
                "{}:  {} \u{00b7} {}\n",
                graph_label,
                name,
                truncate_chars(goal_line, 80)
            ),
        );
    }
    let truncated = if report.truncated_tail {
        match language {
            Language::En => " (truncated tail)",
            Language::Ja => " (\u{672b}\u{5c3e}\u{4e0d}\u{5b8c}\u{5168})",
        }
    } else {
        ""
    };
    let _ = fmt::Write::write_fmt(
        &mut out,
        format_args!(
            "{}: {}{}\n{}: {}ms ago\n{}:\n",
            events_label,
            report.journal.event_count,
            truncated,
            age_label,
            report.last_event_age_ms.unwrap_or(0),
            nodes_label
        ),
    );
    for (id, outcome) in &report.journal.nodes {
        let detail = outcome
            .error
            .as_deref()
            .map(|error| format!("  error: {}", truncate_chars(error, 80)))
            .or_else(|| {
                outcome
                    .output
                    .as_ref()
                    .map(|output| format!("  output: {}", truncate_chars(&output.to_string(), 80)))
            })
            .unwrap_or_default();
        let _ = fmt::Write::write_fmt(
            &mut out,
            format_args!(
                "  {:<20} {:<10?} attempts={}{}\n",
                id, outcome.status, outcome.attempts, detail
            ),
        );
    }
    if !report.events_tail.is_empty() {
        let _ = fmt::Write::write_fmt(&mut out, format_args!("{tail_label}:\n"));
        for event in &report.events_tail {
            let _ = fmt::Write::write_fmt(
                &mut out,
                format_args!(
                    "  {:>4} {:<18} {}\n",
                    event.sequence,
                    format!("{:?}", event.kind),
                    event
                        .message
                        .as_deref()
                        .map(|message| truncate_chars(message, 80))
                        .unwrap_or_default()
                ),
            );
        }
    }
    if let Some(summary) = &report.summary {
        let _ = fmt::Write::write_fmt(
            &mut out,
            format_args!(
                "{}: {:?} \u{00b7} {}ms \u{00b7} {}\n",
                result_label, summary.status, summary.duration_ms, summary.summary
            ),
        );
    }
    out
}

/// Pollable live status for one run. While the run is in flight the journal
/// prefix is reduced with the same rules as replay, so node states and
/// intermediate outputs are visible before `summary.json` exists.
///
/// Exit-code contract: a successful query exits 0 with or without `--wait`
/// (polling loops must be able to distinguish query errors from run state,
/// which they read from `phase`/`final_status`). With `--wait`, the command
/// blocks until the run finishes and then exits with the run's own status
/// code. `--wait` also retries the run-directory lookup and the first journal
/// reads for a grace period, so `gloop run --run-id x & gloop status x --wait`
/// does not race the runtime's directory creation.
#[allow(clippy::too_many_arguments)]
pub async fn run_status(
    run_id: Option<String>,
    repo: PathBuf,
    events: usize,
    wait: bool,
    interval_ms: u64,
    json_mode: bool,
    language: Language,
) -> CommandResult {
    let runs_root = repo.join(PROJECT_CONFIG_PATH).with_file_name("runs");
    let interval = std::time::Duration::from_millis(interval_ms.max(50));
    let grace = std::time::Duration::from_secs(30);
    let started = std::time::Instant::now();

    let mut run_dir: Option<PathBuf> = None;
    let report = loop {
        if run_dir.is_none() {
            match resolve_run_dir(&runs_root, run_id.as_deref()) {
                Ok(dir) => run_dir = Some(dir),
                Err(message) => {
                    if wait && started.elapsed() < grace {
                        tokio::time::sleep(interval).await;
                        continue;
                    }
                    return if json_mode {
                        CommandResult::failure_json(
                            ExitCode::InvalidGraph,
                            "run not found",
                            Some(json!({"error": message})),
                        )
                    } else {
                        CommandResult::failure_text(ExitCode::InvalidGraph, message)
                    };
                }
            }
        }
        let dir = run_dir.clone().expect("resolved above");
        match live_run_status(&dir, events).await {
            Ok(report) => {
                if !wait || report.finished() {
                    break report;
                }
            }
            Err(error) => {
                let retryable_startup = wait
                    && started.elapsed() < grace
                    && (matches!(&error, ReplayError::EmptyJournal)
                        || matches!(&error, ReplayError::Io(io_error)
                            if io_error.kind() == std::io::ErrorKind::NotFound));
                if retryable_startup {
                    tokio::time::sleep(interval).await;
                    continue;
                }
                return if json_mode {
                    CommandResult::failure_json(
                        ExitCode::InvalidGraph,
                        "failed to read run status",
                        Some(json!({"run_dir": dir, "error": error.to_string()})),
                    )
                } else {
                    CommandResult::failure_text(
                        ExitCode::InvalidGraph,
                        format!("failed to read run status at {}: {error}", dir.display()),
                    )
                };
            }
        }
        tokio::time::sleep(interval).await;
    };
    let run_dir = run_dir.expect("run dir resolved before the report loop exits");

    let exit_code = if wait && report.finished() {
        if let Some(summary) = &report.summary {
            summary_exit_code(summary)
        } else {
            report
                .final_status()
                .map_or(ExitCode::Internal, status_exit_code)
        }
    } else {
        ExitCode::Success
    };

    if json_mode {
        CommandResult {
            code: exit_code,
            output: Some(json!({
                "success": true,
                "run": status_payload(&report, &run_dir),
            })),
            text: None,
        }
    } else {
        CommandResult {
            code: exit_code,
            output: None,
            text: Some(format_status_text(&report, &run_dir, language)),
        }
    }
}

fn format_run_summary(summary: &RunSummary) -> String {
    format!(
        "Run {} ({})\nGraph: {}\nGoal: {}\nStatus: {}\nDuration: {}ms\nNodes: {}\nBlocking findings: {}\n",
        summary.run_id,
        summary.started_at,
        summary.graph_name,
        summary.goal,
        status_to_readable(summary.status),
        summary.duration_ms,
        summary.nodes.len(),
        summary.blocking_findings.len(),
    )
}

fn format_inspection_summary(summary: &RunSummary, replay: &ReplayReport) -> String {
    let final_status = replay.final_status.unwrap_or(summary.status);
    let mut out = String::new();
    let _ = fmt::Write::write_fmt(
        &mut out,
        format_args!(
            "Run {} ({})\nStatus: {} -> {}\nGraph: {}\nGoal: {}\nDuration: {}ms\nNodes: {}\nFinal status: {}\nBlocking findings: {}\n",
            summary.run_id,
            summary.started_at,
            status_to_readable(summary.status),
            status_to_readable(final_status),
            summary.graph_name,
            summary.goal,
            summary.duration_ms,
            summary.nodes.len(),
            status_to_readable(final_status),
            summary.blocking_findings.len(),
        ),
    );
    if !summary.unresolved.is_empty() {
        let _ = fmt::Write::write_fmt(
            &mut out,
            format_args!("Unresolved: {}\n", summary.unresolved.len()),
        );
    }
    out
}

fn status_to_readable(status: FinalStatus) -> &'static str {
    match status {
        FinalStatus::VerificationFailed => "verification_failed",
        FinalStatus::BudgetExhausted => "budget_exhausted",
        FinalStatus::ReadyForHuman => "ready_for_human",
        FinalStatus::Failed => "failed",
        FinalStatus::Blocked => "blocked",
        FinalStatus::Cancelled => "cancelled",
    }
}

fn format_replay_report(report: &ReplayReport) -> String {
    let mut out = String::new();
    let final_status = report.final_status.map_or("unknown", status_to_readable);
    let _ = fmt::Write::write_fmt(
        &mut out,
        format_args!(
            "Replay for {}\nEvents: {}\nLast sequence: {}\nFinished: {}\nFinal status: {}\n",
            report.run_id, report.event_count, report.last_sequence, report.finished, final_status,
        ),
    );
    for (node, outcome) in &report.nodes {
        let _ = fmt::Write::write_fmt(&mut out, format_args!("- {node}: {:?}\n", outcome.status));
    }
    out
}

fn format_progress_event(event: &ProgressEvent) -> String {
    let node = event.node_id.as_deref().unwrap_or("-");
    let kind = format!("{:?}", event.kind);
    format!(
        "{:>6} {:<16} {:>10} {}",
        event.sequence,
        kind,
        node,
        event.message.as_deref().unwrap_or_default()
    )
}

fn event_line(event: &RunEvent) -> String {
    let node = event.node_id.as_deref().unwrap_or("-");
    let attempt = event
        .attempt
        .map_or("-".to_owned(), |value| value.to_string());
    let message = event.message.as_deref().unwrap_or_default();
    let kind = format!("{:?}", event.kind);

    format!(
        "{:>6} {:<16} {:>10} {:>8} {}",
        event.sequence, kind, node, attempt, message,
    )
}

fn to_json_mode_output(result: &CommandResult, json_mode: bool) -> Value {
    if json_mode {
        result.output.clone().unwrap_or_else(|| {
            json!({
                "success": result.code == ExitCode::Success,
                "message": result.text.clone().unwrap_or_default(),
                "code": result.code.as_i32()
            })
        })
    } else {
        json!(null)
    }
}

pub fn present(result: CommandResult, json_mode: bool) -> Result<i32> {
    let code = result.code.as_i32();

    if json_mode {
        let output = to_json_mode_output(&result, json_mode);
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else if let Some(text) = result.text {
        println!("{text}");
    } else if let Some(output) = result.output {
        if let Some(error) = output.get("error").and_then(Value::as_str) {
            eprintln!("{error}");
        } else {
            println!("{output}");
        }
    }

    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gloop_core::Graph;
    use std::path::PathBuf;
    use tempfile::tempdir;

    #[test]
    fn gui_init_is_blank_only_without_a_name_template_or_request() {
        assert!(gui_init_starts_blank(None, None, None));
        assert!(!gui_init_starts_blank(None, None, Some("review this")));
        assert!(!gui_init_starts_blank(
            Some("review-flow"),
            Some("plan-implement-verify"),
            None,
        ));
    }

    #[test]
    fn gui_init_preserves_explicit_and_template_default_goals() {
        assert_eq!(gui_init_goal(None, true), "");
        assert_eq!(gui_init_goal(Some("review this"), false), "review this");
        assert_eq!(gui_init_goal(None, false), templates::DEFAULT_TEMPLATE_GOAL,);
    }

    #[tokio::test]
    async fn interactive_graph_new_rejects_unconsumed_template_seeds() {
        let result = graph_new(
            "seed-name".to_owned(),
            "seed goal".to_owned(),
            "review-fix-loop".to_owned(),
            PathBuf::from("."),
            None,
            None,
            None,
            true,
            None,
            false,
            true,
            false,
        )
        .await;

        assert_eq!(result.code, ExitCode::InvalidGraph);
        let error = result
            .output
            .as_ref()
            .and_then(|output| output.get("error"))
            .and_then(Value::as_str)
            .expect("structured error");
        assert!(error.contains("--interactive does not accept"));
    }

    #[test]
    fn edit_resolves_builtin_template_to_safe_new_graph_path() {
        let repo = tempdir().expect("temp repo");
        let source = resolve_graph_edit_source(
            PathBuf::from("plan-implement-verify").as_path(),
            repo.path(),
            false,
        )
        .expect("built-in template should resolve");

        assert!(source.create_only);
        assert!(source.expected_sha256.is_none());
        assert_eq!(
            source.path,
            repo.path().join(".gloop/graphs/plan-implement-verify.yaml")
        );
        assert_eq!(source.graph.spec.nodes.len(), 3);
    }

    #[test]
    fn edit_reuses_materialized_builtin_graph_after_first_save() {
        let repo = tempdir().expect("temp repo");
        let path = templates::graph_path(repo.path(), "direct");
        let graph = Graph::new("direct", "saved goal", vec![]);
        std::fs::create_dir_all(path.parent().expect("graph parent")).expect("create graph parent");
        std::fs::write(&path, graph.to_yaml().expect("serialize graph")).expect("write graph");

        let source =
            resolve_graph_edit_source(PathBuf::from("direct").as_path(), repo.path(), false)
                .expect("materialized graph should resolve");

        assert!(!source.create_only);
        assert!(source.expected_sha256.is_some());
        assert_eq!(source.graph.spec.goal, "saved goal");
    }

    #[test]
    fn edit_resolves_saved_graph_by_metadata_name() {
        let repo = tempdir().expect("temp repo");
        let path = repo.path().join("workflow.yaml");
        let graph = Graph::new("friendly-name", "saved goal", vec![]);
        std::fs::write(&path, graph.to_yaml().expect("serialize graph")).expect("write graph");

        let source =
            resolve_graph_edit_source(PathBuf::from("friendly-name").as_path(), repo.path(), false)
                .expect("metadata name should resolve");

        assert_eq!(source.path, path);
        assert_eq!(source.graph.metadata.name, "friendly-name");
        assert!(source.expected_sha256.is_some());
    }

    #[test]
    fn update_keeps_builtins_read_only() {
        let repo = tempdir().expect("temp repo");
        let error = resolve_graph_edit_source(
            PathBuf::from("plan-implement-verify").as_path(),
            repo.path(),
            true,
        )
        .expect_err("update must not materialize a built-in");

        assert!(error.contains("use 'graph edit plan-implement-verify'"));
    }

    #[test]
    fn profile_model_catalog_scopes_models_per_profile() {
        use gloop_provider::{CatalogFamily, CommandProfile, ModelDiscovery};

        let mut cache = HashMap::new();
        cache.insert(
            (CatalogFamily::Pi, "/usr/bin/pi".to_owned()),
            ModelDiscovery::Listed(vec![CatalogModel::uniform("openai/gpt-4.1")]),
        );
        cache.insert(
            (CatalogFamily::OpenCode, "/usr/bin/opencode".to_owned()),
            ModelDiscovery::Listed(vec![CatalogModel::uniform("anthropic/claude-fable-5")]),
        );
        let pi_profile = Profile {
            enabled: true,
            priority: 0,
            timeout_seconds: None,
            capabilities: gloop_provider::AdapterCapabilities::default(),
            kind: ProfileKind::Command({
                let mut profile = CommandProfile::new(vec!["/usr/bin/pi".to_owned()]);
                profile.model_args = vec!["--model".to_owned(), "{model}".to_owned()];
                profile
            }),
        };
        let opencode_profile = Profile {
            enabled: true,
            priority: 0,
            timeout_seconds: None,
            capabilities: gloop_provider::AdapterCapabilities::default(),
            kind: ProfileKind::Command({
                let mut profile = CommandProfile::new(vec!["/usr/bin/opencode".to_owned()]);
                profile.model_args = vec!["--model".to_owned(), "{model}".to_owned()];
                profile
            }),
        };
        let (pi_models, _, _) = profile_model_catalog(&pi_profile, &cache, None);
        let (opencode_models, _, _) = profile_model_catalog(&opencode_profile, &cache, None);
        assert!(pi_models.iter().any(|model| model.id == "openai/gpt-4.1"));
        assert!(
            !pi_models
                .iter()
                .any(|model| model.id == "anthropic/claude-fable-5")
        );
        assert!(
            opencode_models
                .iter()
                .any(|model| model.id == "anthropic/claude-fable-5")
        );
        assert!(
            !opencode_models
                .iter()
                .any(|model| model.id == "openai/gpt-4.1")
        );
    }

    #[test]
    fn http_profiles_mark_discovery_unsupported() {
        let store = ProfileStore::from_toml_str(
            r#"
[profiles.openai]
kind = "openai"
model = "gpt-5"

[profiles.anthropic]
kind = "anthropic"
model = "claude-opus-4"
"#,
        )
        .expect("http profiles");
        let openai = store.get("openai").expect("openai profile");
        let anthropic = store.get("anthropic").expect("anthropic profile");
        let cache = HashMap::new();
        let (openai_models, openai_discovery, _) = profile_model_catalog(openai, &cache, None);
        let (anthropic_models, anthropic_discovery, _) =
            profile_model_catalog(anthropic, &cache, None);
        assert_eq!(openai_discovery, "unsupported");
        assert_eq!(anthropic_discovery, "unsupported");
        assert!(openai_models.iter().any(|model| model.id == "gpt-5"));
        assert!(!openai_models.iter().any(|model| model.id == "custom"));
        assert!(
            anthropic_models
                .iter()
                .any(|model| model.id == "claude-opus-4")
        );
    }
}
