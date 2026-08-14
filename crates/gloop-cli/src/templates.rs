//! Project graph template naming, discovery, resolution, and persistence.

use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

use gloop_core::{Graph, GraphError, IssueSeverity};

pub const TEMPLATES_DIR: &str = ".gloop/templates";
pub const GRAPHS_DIR: &str = ".gloop/graphs";
pub const MAX_TEMPLATE_NAME_LEN: usize = 64;
pub const DEFAULT_TEMPLATE_GOAL: &str = "work";

pub const BUILTIN_TEMPLATE_NAMES: [&str; 5] = [
    "direct",
    "plan-implement-verify",
    "parallel-research-reduce",
    "review-fix-loop",
    "design-wall-bounce",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateSource {
    Builtin,
    Project,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateEntry {
    pub name: String,
    pub source: TemplateSource,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedTemplate {
    Builtin(&'static str),
    Project(Box<Graph>),
}

#[derive(Debug)]
pub enum TemplateResolveError {
    Unknown {
        name: String,
        available: Vec<String>,
    },
    InvalidName {
        name: String,
        error: String,
    },
    PathEscape {
        path: PathBuf,
        error: String,
    },
    Read {
        path: PathBuf,
        error: String,
    },
    Validation {
        path: PathBuf,
        error: String,
    },
}

impl TemplateResolveError {
    pub fn message(&self) -> String {
        match self {
            Self::Unknown { name, available } => {
                format!(
                    "unknown graph template '{name}'; available templates: {}",
                    available.join(", ")
                )
            }
            Self::InvalidName { name, error } => {
                format!("invalid graph template name '{name}': {error}")
            }
            Self::PathEscape { path, error } => {
                format!(
                    "project template {} escapes the templates directory: {error}",
                    path.display()
                )
            }
            Self::Read { path, error } => {
                format!(
                    "failed to read project template {}: {error}",
                    path.display()
                )
            }
            Self::Validation { path, error } => {
                format!("project template {} is invalid: {error}", path.display())
            }
        }
    }
}

pub fn is_builtin_template_name(name: &str) -> bool {
    BUILTIN_TEMPLATE_NAMES.contains(&name)
}

pub fn is_valid_kebab_name(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_TEMPLATE_NAME_LEN {
        return false;
    }

    let mut parts = name.split('-');
    let first = parts.next().expect("non-empty name");
    if first.is_empty()
        || !first
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit())
    {
        return false;
    }

    for part in parts {
        if part.is_empty()
            || !part
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit())
        {
            return false;
        }
    }

    true
}

pub fn validate_init_template_name(name: &str) -> Result<(), String> {
    if !is_valid_kebab_name(name) {
        return Err(format!(
            "template name must be kebab-case ([a-z0-9]+(-[a-z0-9]+)*) with at most {MAX_TEMPLATE_NAME_LEN} characters"
        ));
    }
    if is_builtin_template_name(name) {
        return Err(format!(
            "template name '{name}' collides with a built-in template"
        ));
    }
    Ok(())
}

pub fn templates_dir(repo: &Path) -> PathBuf {
    repo.join(TEMPLATES_DIR)
}

pub fn template_path(repo: &Path, name: &str) -> PathBuf {
    templates_dir(repo).join(format!("{name}.yaml"))
}

pub fn graphs_dir(repo: &Path) -> PathBuf {
    repo.join(GRAPHS_DIR)
}

pub fn graph_path(repo: &Path, name: &str) -> PathBuf {
    graphs_dir(repo).join(format!("{name}.yaml"))
}

/// Reject symlinked parents for files managed by gloop.
///
/// Arbitrary graph files may live behind symlinked directories, but gloop's
/// own `.gloop` data must never be redirected outside the project.
pub fn ensure_managed_directory(repo: &Path, relative: &Path) -> std::io::Result<()> {
    let mut current = repo.to_path_buf();
    for component in relative.components() {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("managed directory is a symlink: {}", current.display()),
                ));
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotADirectory,
                    format!("managed path is not a directory: {}", current.display()),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Find graph-shaped YAML files without descending into generated or dependency directories.
/// Symlinks are skipped so a project cannot make a read-only catalog walk outside its tree.
pub fn list_graph_files(repo: &Path) -> Result<Vec<PathBuf>, String> {
    if !repo.is_dir() {
        return Err(format!(
            "repository path is not a directory: {}",
            repo.display()
        ));
    }

    let mut files = Vec::new();
    collect_graph_files(repo, repo, 0, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_graph_files(
    repo: &Path,
    directory: &Path,
    depth: usize,
    files: &mut Vec<PathBuf>,
) -> Result<(), String> {
    if depth > 32 {
        return Err(format!(
            "graph search exceeded the maximum directory depth at {}",
            directory.display()
        ));
    }

    for entry in std::fs::read_dir(directory)
        .map_err(|error| format!("failed to read {}: {error}", directory.display()))?
    {
        let entry = entry.map_err(|error| {
            format!(
                "failed to read directory entry in {}: {error}",
                directory.display()
            )
        })?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| format!("failed to inspect {}: {error}", path.display()))?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        let relative = path
            .strip_prefix(repo)
            .map_err(|error| format!("failed to relativize {}: {error}", path.display()))?;
        if metadata.is_dir() {
            if should_skip_graph_directory(relative) {
                continue;
            }
            collect_graph_files(repo, &path, depth + 1, files)?;
        } else if metadata.is_file()
            && path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| matches!(extension, "yaml" | "yml"))
            && (looks_like_graph(&path) || is_managed_graph_file(relative))
        {
            files.push(path);
            if files.len() > 2_048 {
                return Err("graph search found more than 2048 graph files".to_owned());
            }
        }
    }
    Ok(())
}

fn should_skip_graph_directory(relative: &Path) -> bool {
    let mut components = relative.components();
    let Some(first) = components
        .next()
        .and_then(|component| component.as_os_str().to_str())
    else {
        return false;
    };
    if matches!(first, ".git" | "target" | "node_modules") {
        return true;
    }
    if first != ".gloop" {
        return false;
    }
    matches!(
        components
            .next()
            .and_then(|component| component.as_os_str().to_str()),
        Some("templates" | "runs" | "worktrees" | "provider-e2e" | "provider-e2e-final")
    )
}

fn is_managed_graph_file(relative: &Path) -> bool {
    let mut components = relative.components();
    matches!(
        (
            components
                .next()
                .and_then(|component| component.as_os_str().to_str()),
            components
                .next()
                .and_then(|component| component.as_os_str().to_str())
        ),
        (Some(".gloop"), Some("graphs"))
    )
}

fn looks_like_graph(path: &Path) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    let mut source = String::new();
    if file.take(128 * 1024).read_to_string(&mut source).is_err() {
        return false;
    }
    source.contains("apiVersion: gloop.dev/") || source.contains("kind: Graph")
}

pub fn validate_template_lookup_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("template name must not be empty".to_owned());
    }
    if Path::new(name).is_absolute() {
        return Err("template name must not be an absolute path".to_owned());
    }
    if name.contains('/') || name.contains('\\') {
        return Err("template name must not contain path separators".to_owned());
    }
    if name.contains("..") {
        return Err("template name must not contain '..'".to_owned());
    }
    if !is_valid_kebab_name(name) {
        return Err(format!(
            "template name must be kebab-case ([a-z0-9]+(-[a-z0-9]+)*) with at most {MAX_TEMPLATE_NAME_LEN} characters"
        ));
    }
    Ok(())
}

pub fn confined_template_path(
    repo: &Path,
    name: &str,
) -> Result<Option<PathBuf>, TemplateResolveError> {
    validate_template_lookup_name(name).map_err(|error| TemplateResolveError::InvalidName {
        name: name.to_owned(),
        error,
    })?;

    let canonical_repo =
        std::fs::canonicalize(repo).map_err(|error| TemplateResolveError::Read {
            path: repo.to_path_buf(),
            error: error.to_string(),
        })?;

    let dir = templates_dir(repo);
    let candidate = dir.join(format!("{name}.yaml"));

    let canonical_dir = match std::fs::canonicalize(&dir) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(error) => {
            return Err(TemplateResolveError::Read {
                path: dir,
                error: error.to_string(),
            });
        }
    };

    if !canonical_dir.starts_with(&canonical_repo) {
        return Err(TemplateResolveError::PathEscape {
            path: candidate,
            error: "templates directory is outside the repository".to_owned(),
        });
    }

    let canonical_candidate = canonical_dir.join(format!("{name}.yaml"));
    match std::fs::symlink_metadata(&canonical_candidate) {
        Ok(_) => {
            let resolved = std::fs::canonicalize(&canonical_candidate).map_err(|error| {
                TemplateResolveError::Read {
                    path: canonical_candidate.clone(),
                    error: error.to_string(),
                }
            })?;
            if !resolved.starts_with(&canonical_dir) {
                return Err(TemplateResolveError::PathEscape {
                    path: canonical_candidate,
                    error: "resolved path is outside the templates directory".to_owned(),
                });
            }
            Ok(Some(resolved))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(TemplateResolveError::Read {
            path: canonical_candidate,
            error: error.to_string(),
        }),
    }
}

pub fn strict_validate_project_template(graph: &Graph) -> Result<(), String> {
    let errors: Vec<String> = graph
        .validate()
        .into_iter()
        .filter(|issue| issue.severity == IssueSeverity::Error)
        .map(|issue| issue.message)
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

pub fn list_builtin_templates() -> Vec<TemplateEntry> {
    BUILTIN_TEMPLATE_NAMES
        .iter()
        .map(|name| TemplateEntry {
            name: (*name).to_owned(),
            source: TemplateSource::Builtin,
            description: Some(
                match *name {
                    "direct" => "one agent task",
                    "plan-implement-verify" => "plan, implement, then verify",
                    "parallel-research-reduce" => "research in parallel, then reduce",
                    "review-fix-loop" => "bounded review and fix loop",
                    "design-wall-bounce" => "two designers wall-bounce proposals and integrate",
                    _ => "built-in graph template",
                }
                .to_owned(),
            ),
        })
        .collect()
}

pub fn list_project_templates(repo: &Path) -> Result<Vec<TemplateEntry>, String> {
    let dir = templates_dir(repo);
    let canonical_repo = std::fs::canonicalize(repo)
        .map_err(|error| format!("failed to inspect {}: {error}", repo.display()))?;
    let dir = match std::fs::canonicalize(&dir) {
        Ok(canonical_dir) if canonical_dir.starts_with(&canonical_repo) => canonical_dir,
        Ok(canonical_dir) => {
            return Err(format!(
                "template directory escapes the repository: {}",
                canonical_dir.display()
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(format!("failed to inspect {}: {error}", dir.display()));
        }
    };

    let mut entries = Vec::new();
    for entry in std::fs::read_dir(&dir)
        .map_err(|error| format!("failed to read {}: {error}", dir.display()))?
    {
        let entry =
            entry.map_err(|error| format!("failed to read template directory entry: {error}"))?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| format!("failed to inspect {}: {error}", path.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("yaml") {
            continue;
        }
        let file_name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default();
        if file_name.is_empty() {
            continue;
        }
        let description = Graph::from_path(&path)
            .ok()
            .and_then(|graph| graph.metadata.description.clone());
        entries.push(TemplateEntry {
            name: file_name.to_owned(),
            source: TemplateSource::Project,
            description,
        });
    }
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(entries)
}

pub fn list_all_templates(repo: &Path) -> Result<Vec<TemplateEntry>, String> {
    let mut entries = list_builtin_templates();
    entries.extend(list_project_templates(repo)?);
    Ok(entries)
}

pub fn available_template_names(repo: &Path) -> Result<Vec<String>, String> {
    Ok(list_all_templates(repo)?
        .into_iter()
        .map(|entry| entry.name)
        .collect())
}

pub fn resolve_template(name: &str, repo: &Path) -> Result<ResolvedTemplate, TemplateResolveError> {
    if is_builtin_template_name(name) {
        return Ok(ResolvedTemplate::Builtin(
            BUILTIN_TEMPLATE_NAMES
                .iter()
                .copied()
                .find(|builtin| *builtin == name)
                .expect("builtin name"),
        ));
    }

    let path = confined_template_path(repo, name)?;
    let path = match path {
        Some(path) if path.is_file() => path,
        _ => {
            let available = available_template_names(repo).unwrap_or_default();
            return Err(TemplateResolveError::Unknown {
                name: name.to_owned(),
                available,
            });
        }
    };

    let graph = match Graph::from_path(&path) {
        Ok(graph) => graph,
        Err(GraphError::Read { path, source }) => {
            return Err(TemplateResolveError::Read {
                path,
                error: source.to_string(),
            });
        }
        Err(error) => {
            return Err(TemplateResolveError::Validation {
                path,
                error: error.to_string(),
            });
        }
    };

    if let Err(error) = strict_validate_project_template(&graph) {
        return Err(TemplateResolveError::Validation { path, error });
    }

    Ok(ResolvedTemplate::Project(Box::new(graph)))
}

pub fn apply_new_overrides(graph: &mut Graph, name: &str, goal: &str) {
    name.clone_into(&mut graph.metadata.name);
    goal.clone_into(&mut graph.spec.goal);
}

#[cfg(test)]
mod tests {
    use super::{
        BUILTIN_TEMPLATE_NAMES, DEFAULT_TEMPLATE_GOAL, MAX_TEMPLATE_NAME_LEN, ResolvedTemplate,
        TemplateResolveError, TemplateSource, apply_new_overrides, confined_template_path,
        is_builtin_template_name, is_valid_kebab_name, list_all_templates, list_project_templates,
        resolve_template, strict_validate_project_template, template_path, templates_dir,
        validate_init_template_name, validate_template_lookup_name,
    };
    use gloop_core::{
        ContextSpec, Graph, Node, NodeKind, OutputSpec, PromptSpec, RetryPolicy, WorkspaceSpec,
    };
    use std::fs;
    use tempfile::tempdir;

    fn sample_project_graph(name: &str) -> Graph {
        Graph::new(
            name,
            DEFAULT_TEMPLATE_GOAL,
            vec![Node {
                id: "work".to_owned(),
                label: None,
                requires: vec![],
                resources: vec![],
                retry: RetryPolicy::default(),
                timeout_seconds: None,
                workspace: WorkspaceSpec::default(),
                context: ContextSpec::default(),
                continue_on_failure: false,
                kind: NodeKind::Agent {
                    prompt: PromptSpec::Inline("do work".to_owned()),
                    profile: None,
                    model: None,
                    fan_out: 1,
                    output: OutputSpec::default(),
                },
            }],
        )
    }

    #[test]
    fn kebab_name_validation_accepts_and_rejects_expected_values() {
        assert!(is_valid_kebab_name("my-template"));
        assert!(is_valid_kebab_name("a1"));
        assert!(is_valid_kebab_name("review-fix-2"));
        assert!(!is_valid_kebab_name(""));
        assert!(!is_valid_kebab_name("-bad"));
        assert!(!is_valid_kebab_name("bad-"));
        assert!(!is_valid_kebab_name("Upper"));
        assert!(!is_valid_kebab_name("a--b"));
        assert!(!is_valid_kebab_name(&"a".repeat(MAX_TEMPLATE_NAME_LEN + 1)));
    }

    #[test]
    fn init_name_rejects_builtin_collisions() {
        for name in BUILTIN_TEMPLATE_NAMES {
            assert!(is_builtin_template_name(name));
            let error = validate_init_template_name(name).expect_err("builtin collision");
            assert!(error.contains("built-in"));
        }
        assert!(validate_init_template_name("my-flow").is_ok());
    }

    #[test]
    fn resolve_builtin_before_project_lookup() {
        let dir = tempdir().expect("tempdir");
        let path = template_path(dir.path(), "direct");
        fs::create_dir_all(path.parent().expect("parent")).expect("create templates dir");
        fs::write(
            &path,
            "apiVersion: gloop.dev/v1alpha1\nkind: Graph\nmetadata:\n  name: direct\nspec:\n  goal: override\n  policies: {}\n  budgets: {}\n  nodes: []\n",
        )
        .expect("write project direct template");

        let resolved = resolve_template("direct", dir.path()).expect("builtin wins");
        assert!(matches!(resolved, ResolvedTemplate::Builtin("direct")));
    }

    #[test]
    fn resolve_project_template_reads_yaml() {
        let dir = tempdir().expect("tempdir");
        let graph = sample_project_graph("saved");
        let yaml = graph.to_yaml().expect("serialize");
        let path = template_path(dir.path(), "saved");
        fs::create_dir_all(path.parent().expect("parent")).expect("create templates dir");
        fs::write(&path, yaml).expect("write template");

        let resolved = resolve_template("saved", dir.path()).expect("project template");
        match resolved {
            ResolvedTemplate::Project(graph) => assert_eq!(graph.metadata.name, "saved"),
            ResolvedTemplate::Builtin(_) => panic!("expected project template"),
        }
    }

    #[test]
    fn unknown_template_lists_available_names() {
        let dir = tempdir().expect("tempdir");
        let error = resolve_template("missing", dir.path()).expect_err("unknown");
        match error {
            super::TemplateResolveError::Unknown { name, available } => {
                assert_eq!(name, "missing");
                assert!(available.contains(&"direct".to_owned()));
            }
            _ => panic!("expected unknown error"),
        }
    }

    #[test]
    fn list_project_templates_scans_yaml_files() {
        let dir = tempdir().expect("tempdir");
        let templates = templates_dir(dir.path());
        fs::create_dir_all(&templates).expect("create templates dir");
        let graph = sample_project_graph("listed");
        fs::write(
            template_path(dir.path(), "listed"),
            graph.to_yaml().expect("yaml"),
        )
        .expect("write template");
        fs::write(templates.join("notes.txt"), "skip").expect("write non-template");

        let entries = list_project_templates(dir.path()).expect("list");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "listed");
        assert_eq!(entries[0].source, TemplateSource::Project);
    }

    #[test]
    fn list_all_templates_includes_builtin_and_project_entries() {
        let dir = tempdir().expect("tempdir");
        fs::create_dir_all(templates_dir(dir.path())).expect("create templates dir");
        let graph = sample_project_graph("custom");
        fs::write(
            template_path(dir.path(), "custom"),
            graph.to_yaml().expect("yaml"),
        )
        .expect("write template");

        let entries = list_all_templates(dir.path()).expect("list all");
        assert!(entries.iter().any(|entry| entry.name == "direct"));
        assert!(entries.iter().any(|entry| entry.name == "custom"));
    }

    #[test]
    fn apply_new_overrides_replaces_name_and_goal() {
        let mut graph = Graph::new("template", "template goal", vec![]);
        apply_new_overrides(&mut graph, "run", "actual goal");
        assert_eq!(graph.metadata.name, "run");
        assert_eq!(graph.spec.goal, "actual goal");
    }

    #[test]
    fn template_lookup_name_rejects_traversal_and_absolute_paths() {
        for name in [
            "../../../outside",
            "/etc/passwd",
            "foo/bar",
            "foo\\bar",
            "..",
            "foo..bar",
        ] {
            let error = validate_template_lookup_name(name).expect_err(name);
            assert!(
                error.contains("path")
                    || error.contains("kebab-case")
                    || error.contains("..")
                    || error.contains("absolute"),
                "unexpected error for {name}: {error}"
            );
        }
        assert!(validate_template_lookup_name("my-flow").is_ok());
    }

    #[test]
    fn confined_template_path_rejects_symlink_escape() {
        #[cfg(not(unix))]
        return;

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let dir = tempdir().expect("tempdir");
            let templates = templates_dir(dir.path());
            fs::create_dir_all(&templates).expect("create templates dir");
            let outside = dir.path().join("outside.yaml");
            fs::write(&outside, "outside").expect("write outside");
            let link = templates.join("escaped.yaml");
            symlink(&outside, &link).expect("create symlink escape");

            let error = confined_template_path(dir.path(), "escaped").expect_err("escape");
            assert!(matches!(error, TemplateResolveError::PathEscape { .. }));
        }
    }

    #[test]
    fn confined_template_path_rejects_symlinked_templates_directory() {
        #[cfg(not(unix))]
        return;

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            let outside = tempdir().expect("outside tempdir");
            let graph = sample_project_graph("evil");
            fs::write(
                outside.path().join("evil.yaml"),
                graph.to_yaml().expect("yaml"),
            )
            .expect("write outside template");
            fs::create_dir(repo.join(".gloop")).expect("create .gloop dir");
            symlink(outside.path(), repo.join(".gloop/templates")).expect("symlink templates");

            let error = confined_template_path(repo, "evil").expect_err("directory escape");
            assert!(matches!(error, TemplateResolveError::PathEscape { .. }));
        }
    }

    #[test]
    fn confined_template_path_rejects_symlinked_gloop_directory() {
        #[cfg(not(unix))]
        return;

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            let outside = tempdir().expect("outside tempdir");
            fs::create_dir_all(outside.path().join("templates")).expect("create templates dir");
            let graph = sample_project_graph("evil");
            fs::write(
                outside.path().join("templates/evil.yaml"),
                graph.to_yaml().expect("yaml"),
            )
            .expect("write outside template");
            symlink(outside.path(), repo.join(".gloop")).expect("symlink .gloop");

            let error = confined_template_path(repo, "evil").expect_err("gloop escape");
            assert!(matches!(error, TemplateResolveError::PathEscape { .. }));
        }
    }

    #[test]
    fn confined_template_path_dangling_gloop_symlink_returns_none() {
        #[cfg(not(unix))]
        return;

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            let outside = tempdir().expect("outside tempdir");
            symlink(outside.path(), repo.join(".gloop")).expect("symlink .gloop");

            let result = confined_template_path(repo, "evil").expect("no error");
            assert_eq!(result, None);
        }
    }

    #[test]
    fn confined_template_path_missing_templates_dir_yields_unknown_template() {
        let dir = tempdir().expect("tempdir");
        let error = resolve_template("missing", dir.path()).expect_err("unknown");
        match error {
            TemplateResolveError::Unknown { name, available } => {
                assert_eq!(name, "missing");
                assert!(available.contains(&"direct".to_owned()));
            }
            other => panic!("expected unknown error, got {other:?}"),
        }
    }

    #[test]
    fn strict_validate_project_template_rejects_empty_goal() {
        let graph = Graph::new("broken", "", vec![]);
        let error = strict_validate_project_template(&graph).expect_err("empty goal");
        assert!(error.contains("goal"));
    }

    #[test]
    fn resolve_rejects_invalid_project_template_before_overrides() {
        let dir = tempdir().expect("tempdir");
        let templates = templates_dir(dir.path());
        fs::create_dir_all(&templates).expect("create templates dir");
        fs::write(
            templates.join("broken.yaml"),
            "apiVersion: gloop.dev/v1alpha1\nkind: Graph\nmetadata:\n  name: broken\nspec:\n  goal: \"\"\n  policies: {}\n  budgets: {}\n  nodes: []\n",
        )
        .expect("write invalid template");

        let error = resolve_template("broken", dir.path()).expect_err("invalid template");
        match error {
            TemplateResolveError::Validation { error, .. } => {
                assert!(error.contains("goal"));
            }
            other => panic!("expected validation error, got {other:?}"),
        }
    }
}
