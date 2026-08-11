use std::{
    collections::BTreeMap,
    ffi::OsString,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    process::ExitStatus,
    sync::OnceLock,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{
    fs,
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::Mutex,
    time,
};

const GIT_OUTPUT_LIMIT: usize = 1024 * 1024;
const GIT_TIMEOUT: Duration = Duration::from_secs(120);
const INITIAL_ID_PREFIX_LEN: usize = 24;
const MAX_RUN_ID_LEN: usize = 128;
const MAX_NODE_ID_LEN: usize = 16 * 1024;
const MAX_BASE_LEN: usize = 4 * 1024;

static GIT_METADATA_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn git_metadata_lock() -> &'static Mutex<()> {
    GIT_METADATA_LOCK.get_or_init(|| Mutex::new(()))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorktreeRecord {
    pub node: String,
    pub path: PathBuf,
    pub branch: String,
    pub base_commit: String,
    pub commit: String,
    pub dirty: bool,
    pub auto_commit: bool,
}

impl WorktreeRecord {
    pub fn workspace(&self) -> WorktreeWorkspace {
        WorktreeWorkspace {
            owner_node: self.node.clone(),
            path: self.path.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorktreeWorkspace {
    pub owner_node: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorktreeManifest {
    pub run_id: String,
    pub repository: PathBuf,
    pub root: PathBuf,
    pub base_commit: String,
    pub records: Vec<WorktreeRecord>,
}

#[derive(Debug)]
struct ManagedRecord {
    requested_base: Option<String>,
    record: WorktreeRecord,
}

#[derive(Debug, Default)]
struct ManagerState {
    records: BTreeMap<String, ManagedRecord>,
}

/// Owns retained Git worktrees for one foreground gloop run.
///
/// Construction performs the repository preflight exactly once. All later
/// methods keep the captured base commit and never re-check source-tree
/// cleanliness, so retries remain stable even after nodes start editing their
/// isolated worktrees.
#[derive(Debug)]
pub struct GitWorktreeManager {
    repository: PathBuf,
    git_common_dir: PathBuf,
    run_id: String,
    root: PathBuf,
    base_commit: String,
    state: Mutex<ManagerState>,
}

impl GitWorktreeManager {
    pub async fn new(
        repository: impl AsRef<Path>,
        run_id: impl Into<String>,
    ) -> Result<Self, WorktreeError> {
        let run_id = run_id.into();
        validate_run_id(&run_id)?;

        let _git_guard = git_metadata_lock().lock().await;
        let requested_repository = repository.as_ref().to_path_buf();
        let repository = fs::canonicalize(&requested_repository)
            .await
            .map_err(|source| WorktreeError::RepositoryPath {
                path: requested_repository,
                source,
            })?;
        if !fs::metadata(&repository)
            .await
            .map_err(|source| WorktreeError::RepositoryPath {
                path: repository.clone(),
                source,
            })?
            .is_dir()
        {
            return Err(WorktreeError::RepositoryNotDirectory(repository));
        }

        let top_level = successful_git(&repository, "locate repository top level", |command| {
            command.args(["rev-parse", "--show-toplevel"]);
        })
        .await?;
        let reported_top_level = path_from_git_output(&top_level.stdout, "repository top level")?;
        let canonical_top_level =
            fs::canonicalize(&reported_top_level)
                .await
                .map_err(|source| WorktreeError::RepositoryPath {
                    path: reported_top_level,
                    source,
                })?;
        if canonical_top_level != repository {
            return Err(WorktreeError::RepositoryTopLevelMismatch {
                requested: repository,
                reported: canonical_top_level,
            });
        }

        reject_external_git_drivers(&repository).await?;

        let status = successful_git(&repository, "check source tree cleanliness", |command| {
            command.args([
                "status",
                "--porcelain=v1",
                "--untracked-files=all",
                "--ignore-submodules=none",
                "--",
                ".",
                ":(exclude).gloop",
                ":(exclude).gloop/**",
            ]);
        })
        .await?;
        if !status.stdout.is_empty() {
            return Err(WorktreeError::DirtyRepository {
                repository: repository.clone(),
                status: bounded_text(&status.stdout),
            });
        }

        let base_commit = resolve_commit(&repository, "HEAD").await?;
        let common_dir = successful_git(&repository, "locate common Git directory", |command| {
            command.args(["rev-parse", "--git-common-dir"]);
        })
        .await?;
        let common_dir = path_from_git_output(&common_dir.stdout, "common Git directory")?;
        let common_dir = if common_dir.is_absolute() {
            common_dir
        } else {
            repository.join(common_dir)
        };
        let git_common_dir = fs::canonicalize(&common_dir).await.map_err(|source| {
            WorktreeError::RepositoryPath {
                path: common_dir,
                source,
            }
        })?;

        let gloop_dir = repository.join(".gloop");
        let worktrees_dir = gloop_dir.join("worktrees");
        ensure_directory(&gloop_dir).await?;
        ensure_directory(&worktrees_dir).await?;
        let root = worktrees_dir.join(&run_id);
        reject_existing_run_root(&root).await?;
        fs::create_dir(&root)
            .await
            .map_err(|source| WorktreeError::CreateDirectory {
                path: root.clone(),
                source,
            })?;
        let canonical_root =
            fs::canonicalize(&root)
                .await
                .map_err(|source| WorktreeError::RepositoryPath {
                    path: root.clone(),
                    source,
                })?;
        if canonical_root != root || !canonical_root.starts_with(&repository) {
            return Err(WorktreeError::UnsafePath {
                path: canonical_root,
                reason: "run worktree root escaped its canonical repository".to_owned(),
            });
        }

        Ok(Self {
            repository,
            git_common_dir,
            run_id,
            root,
            base_commit,
            state: Mutex::new(ManagerState::default()),
        })
    }

    pub fn repository(&self) -> &Path {
        &self.repository
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn base_commit(&self) -> &str {
        &self.base_commit
    }

    pub async fn record_for(&self, qualified_node_id: &str) -> Option<WorktreeRecord> {
        self.state
            .lock()
            .await
            .records
            .get(qualified_node_id)
            .map(|managed| managed.record.clone())
    }

    pub async fn workspace_for_node(
        &self,
        qualified_node_id: &str,
        base: Option<&str>,
        auto_commit: bool,
    ) -> Result<WorktreeRecord, WorktreeError> {
        validate_node_id(qualified_node_id)?;
        validate_base(base)?;

        let _git_guard = git_metadata_lock().lock().await;
        let mut state = self.state.lock().await;
        if let Some(existing) = state.records.get(qualified_node_id) {
            if existing.requested_base.as_deref() != base
                || existing.record.auto_commit != auto_commit
            {
                return Err(WorktreeError::NodeConfigurationChanged {
                    node: qualified_node_id.to_owned(),
                });
            }
            self.validate_owned_worktree(&existing.record).await?;
            return Ok(existing.record.clone());
        }

        self.validate_manager_root().await?;
        reject_external_git_drivers(&self.repository).await?;
        let base_commit = match base {
            Some(base) => resolve_commit(&self.repository, base).await?,
            None => self.base_commit.clone(),
        };
        let digest = hex::encode(Sha256::digest(qualified_node_id.as_bytes()));
        let (prefix, branch, path) = self.available_identity(&digest).await?;
        let branch_check = run_git(
            &self.repository,
            "validate generated branch name",
            |command| {
                command.args(["check-ref-format", "--branch", &branch]);
            },
        )
        .await?;
        if !branch_check.status.success() {
            return Err(command_failed(
                "validate generated branch name",
                &branch_check,
            ));
        }
        reject_existing_worktree_path(&path).await?;

        let add = run_git(&self.repository, "create node worktree", |command| {
            command
                .arg("worktree")
                .arg("add")
                .arg("-b")
                .arg(&branch)
                .arg(&path)
                .arg(&base_commit);
        })
        .await?;
        if !add.status.success() {
            return Err(command_failed("create node worktree", &add));
        }

        let canonical_path =
            fs::canonicalize(&path)
                .await
                .map_err(|source| WorktreeError::RepositoryPath {
                    path: path.clone(),
                    source,
                })?;
        if canonical_path != path || canonical_path.parent() != Some(self.root.as_path()) {
            return Err(WorktreeError::UnsafePath {
                path: canonical_path,
                reason: "created worktree is outside the manager-owned root".to_owned(),
            });
        }

        let record = WorktreeRecord {
            node: qualified_node_id.to_owned(),
            path: canonical_path,
            branch,
            base_commit: base_commit.clone(),
            commit: base_commit,
            dirty: false,
            auto_commit,
        };
        self.validate_owned_worktree(&record).await?;
        state.records.insert(
            qualified_node_id.to_owned(),
            ManagedRecord {
                requested_base: base.map(str::to_owned),
                record: record.clone(),
            },
        );
        debug_assert!(record.path.ends_with(&prefix));
        Ok(record)
    }

    pub async fn inherit_workspace(
        &self,
        source_node: &str,
        inherited_path: impl AsRef<Path>,
    ) -> Result<WorktreeRecord, WorktreeError> {
        let _git_guard = git_metadata_lock().lock().await;
        let state = self.state.lock().await;
        let existing = state
            .records
            .get(source_node)
            .ok_or_else(|| WorktreeError::UnknownWorktreeNode(source_node.to_owned()))?;
        if inherited_path.as_ref().as_os_str() != existing.record.path.as_os_str() {
            return Err(WorktreeError::InheritedPathMismatch {
                node: source_node.to_owned(),
                expected: existing.record.path.clone(),
                actual: inherited_path.as_ref().to_path_buf(),
            });
        }
        self.validate_owned_worktree(&existing.record).await?;
        Ok(existing.record.clone())
    }

    pub async fn finish_node_success(
        &self,
        qualified_node_id: &str,
    ) -> Result<WorktreeRecord, WorktreeError> {
        let _git_guard = git_metadata_lock().lock().await;
        let mut state = self.state.lock().await;
        let managed = state
            .records
            .get_mut(qualified_node_id)
            .ok_or_else(|| WorktreeError::UnknownWorktreeNode(qualified_node_id.to_owned()))?;
        self.validate_owned_worktree(&managed.record).await?;
        reject_external_git_drivers(&managed.record.path).await?;

        let mut dirty = worktree_is_dirty(&managed.record.path).await?;
        if managed.record.auto_commit && dirty {
            successful_git(
                &managed.record.path,
                "stage node worktree changes",
                |command| {
                    command.args(["add", "--all", "--", "."]);
                },
            )
            .await?;
            let staged = run_git(
                &managed.record.path,
                "inspect staged node changes",
                |command| {
                    command.args([
                        "diff",
                        "--cached",
                        "--quiet",
                        "--exit-code",
                        "--no-ext-diff",
                        "--no-textconv",
                    ]);
                },
            )
            .await?;
            match staged.status.code() {
                Some(1) => {}
                Some(0) => {
                    return Err(WorktreeError::DirtyButNothingStaged {
                        node: qualified_node_id.to_owned(),
                    });
                }
                _ => return Err(command_failed("inspect staged node changes", &staged)),
            }

            let commit_message = format!(
                "gloop: auto-commit {}",
                &hex::encode(Sha256::digest(qualified_node_id.as_bytes()))[..INITIAL_ID_PREFIX_LEN]
            );
            successful_git(
                &managed.record.path,
                "commit node worktree changes",
                |command| {
                    command
                        .arg("-c")
                        .arg("user.name=gloop")
                        .arg("-c")
                        .arg("user.email=gloop@localhost")
                        .arg("-c")
                        .arg("commit.gpgsign=false")
                        .arg("commit")
                        .arg("--no-verify")
                        .arg("--no-gpg-sign")
                        .arg("-m")
                        .arg(commit_message)
                        .env("GIT_AUTHOR_NAME", "gloop")
                        .env("GIT_AUTHOR_EMAIL", "gloop@localhost")
                        .env("GIT_COMMITTER_NAME", "gloop")
                        .env("GIT_COMMITTER_EMAIL", "gloop@localhost");
                },
            )
            .await?;
            dirty = worktree_is_dirty(&managed.record.path).await?;
            if dirty {
                managed.record.dirty = true;
                managed.record.commit = resolve_commit(&managed.record.path, "HEAD").await?;
                return Err(WorktreeError::WorktreeStillDirty {
                    node: qualified_node_id.to_owned(),
                });
            }
        }

        managed.record.commit = resolve_commit(&managed.record.path, "HEAD").await?;
        managed.record.dirty = dirty;
        Ok(managed.record.clone())
    }

    pub async fn finish_workspace_success(
        &self,
        workspace: &WorktreeWorkspace,
    ) -> Result<WorktreeRecord, WorktreeError> {
        self.inherit_workspace(&workspace.owner_node, &workspace.path)
            .await?;
        self.finish_node_success(&workspace.owner_node).await
    }

    pub async fn manifest(&self) -> Result<WorktreeManifest, WorktreeError> {
        let _git_guard = git_metadata_lock().lock().await;
        let mut state = self.state.lock().await;
        self.validate_manager_root().await?;
        reject_external_git_drivers(&self.repository).await?;
        for managed in state.records.values_mut() {
            self.validate_owned_worktree(&managed.record).await?;
            managed.record.commit = resolve_commit(&managed.record.path, "HEAD").await?;
            managed.record.dirty = worktree_is_dirty(&managed.record.path).await?;
        }
        Ok(WorktreeManifest {
            run_id: self.run_id.clone(),
            repository: self.repository.clone(),
            root: self.root.clone(),
            base_commit: self.base_commit.clone(),
            records: state
                .records
                .values()
                .map(|managed| managed.record.clone())
                .collect(),
        })
    }

    async fn available_identity(
        &self,
        digest: &str,
    ) -> Result<(String, String, PathBuf), WorktreeError> {
        for length in (INITIAL_ID_PREFIX_LEN..=digest.len()).step_by(8) {
            let prefix = digest[..length].to_owned();
            let branch = format!("gloop/{}/{prefix}", self.run_id);
            let reference = format!("refs/heads/{branch}");
            let exists = run_git(
                &self.repository,
                "check generated branch availability",
                |command| {
                    command.args(["show-ref", "--verify", "--quiet", &reference]);
                },
            )
            .await?;
            if exists.status.code() == Some(1) {
                return Ok((prefix.clone(), branch, self.root.join(prefix)));
            }
            if exists.status.code() != Some(0) {
                return Err(command_failed(
                    "check generated branch availability",
                    &exists,
                ));
            }
        }
        Err(WorktreeError::BranchNamespaceExhausted {
            node_digest: digest.to_owned(),
        })
    }

    async fn validate_owned_worktree(&self, record: &WorktreeRecord) -> Result<(), WorktreeError> {
        self.validate_manager_root().await?;
        if record.path.parent() != Some(self.root.as_path()) {
            return Err(WorktreeError::UnsafePath {
                path: record.path.clone(),
                reason: "recorded worktree is outside the manager-owned root".to_owned(),
            });
        }
        let metadata = fs::symlink_metadata(&record.path).await.map_err(|source| {
            WorktreeError::RepositoryPath {
                path: record.path.clone(),
                source,
            }
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(WorktreeError::UnsafePath {
                path: record.path.clone(),
                reason: "recorded worktree is not a real directory".to_owned(),
            });
        }
        let canonical = fs::canonicalize(&record.path).await.map_err(|source| {
            WorktreeError::RepositoryPath {
                path: record.path.clone(),
                source,
            }
        })?;
        if canonical != record.path {
            return Err(WorktreeError::UnsafePath {
                path: record.path.clone(),
                reason: "recorded worktree path is not canonical".to_owned(),
            });
        }

        let top_level = successful_git(
            &record.path,
            "validate node worktree top level",
            |command| {
                command.args(["rev-parse", "--show-toplevel"]);
            },
        )
        .await?;
        let top_level = path_from_git_output(&top_level.stdout, "node worktree top level")?;
        let top_level =
            fs::canonicalize(&top_level)
                .await
                .map_err(|source| WorktreeError::RepositoryPath {
                    path: top_level,
                    source,
                })?;
        if top_level != record.path {
            return Err(WorktreeError::UnsafePath {
                path: record.path.clone(),
                reason: "recorded directory is not its Git worktree top level".to_owned(),
            });
        }

        let common_dir = successful_git(&record.path, "validate node Git directory", |command| {
            command.args(["rev-parse", "--git-common-dir"]);
        })
        .await?;
        let common_dir = path_from_git_output(&common_dir.stdout, "node common Git directory")?;
        let common_dir = if common_dir.is_absolute() {
            common_dir
        } else {
            record.path.join(common_dir)
        };
        let common_dir = fs::canonicalize(&common_dir).await.map_err(|source| {
            WorktreeError::RepositoryPath {
                path: common_dir,
                source,
            }
        })?;
        if common_dir != self.git_common_dir {
            return Err(WorktreeError::UnsafePath {
                path: record.path.clone(),
                reason: "recorded worktree belongs to a different Git repository".to_owned(),
            });
        }

        let branch = successful_git(&record.path, "validate node worktree branch", |command| {
            command.args(["symbolic-ref", "--quiet", "--short", "HEAD"]);
        })
        .await?;
        let branch = text_from_git_output(&branch.stdout, "node worktree branch")?;
        if branch != record.branch {
            return Err(WorktreeError::WorktreeBranchChanged {
                node: record.node.clone(),
                expected: record.branch.clone(),
                actual: branch,
            });
        }
        Ok(())
    }

    async fn validate_manager_root(&self) -> Result<(), WorktreeError> {
        for path in [
            self.repository.join(".gloop"),
            self.repository.join(".gloop/worktrees"),
            self.root.clone(),
        ] {
            let metadata = fs::symlink_metadata(&path).await.map_err(|source| {
                WorktreeError::RepositoryPath {
                    path: path.clone(),
                    source,
                }
            })?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(WorktreeError::UnsafePath {
                    path,
                    reason: "manager-owned path component is not a real directory".to_owned(),
                });
            }
        }
        let canonical_root =
            fs::canonicalize(&self.root)
                .await
                .map_err(|source| WorktreeError::RepositoryPath {
                    path: self.root.clone(),
                    source,
                })?;
        if canonical_root != self.root {
            return Err(WorktreeError::UnsafePath {
                path: self.root.clone(),
                reason: "manager-owned root is no longer canonical".to_owned(),
            });
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum WorktreeError {
    #[error("invalid worktree run id {0:?}")]
    InvalidRunId(String),
    #[error("invalid qualified node id {0:?}")]
    InvalidNodeId(String),
    #[error("invalid worktree base {0:?}")]
    InvalidBase(String),
    #[error("cannot access repository path {path}: {source}")]
    RepositoryPath {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("repository path is not a directory: {0}")]
    RepositoryNotDirectory(PathBuf),
    #[error("requested canonical repository {requested} does not equal Git top level {reported}")]
    RepositoryTopLevelMismatch {
        requested: PathBuf,
        reported: PathBuf,
    },
    #[error("source repository is dirty (excluding .gloop): {repository}: {status}")]
    DirtyRepository { repository: PathBuf, status: String },
    #[error("unsafe worktree path {path}: {reason}")]
    UnsafePath { path: PathBuf, reason: String },
    #[error("worktree run root already exists: {0}")]
    RunRootAlreadyExists(PathBuf),
    #[error("node worktree path already exists: {0}")]
    WorktreePathAlreadyExists(PathBuf),
    #[error("failed to create directory {path}: {source}")]
    CreateDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to start Git operation {operation}: {source}")]
    GitSpawn {
        operation: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("Git operation {operation} timed out after {} seconds", GIT_TIMEOUT.as_secs())]
    GitTimeout { operation: &'static str },
    #[error("failed to wait for Git operation {operation}: {source}")]
    GitWait {
        operation: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("Git operation {operation} exceeded the {stream} output limit of {limit} bytes")]
    GitOutputTooLarge {
        operation: &'static str,
        stream: &'static str,
        limit: usize,
    },
    #[error("Git operation {operation} failed with status {status:?}: {stderr}")]
    GitCommand {
        operation: &'static str,
        status: Option<i32>,
        stderr: String,
    },
    #[error("repository-local external Git filter/diff drivers are unsupported in worktree mode")]
    UnsafeGitDriverConfiguration,
    #[error("Git returned invalid {what}: {details}")]
    InvalidGitOutput { what: &'static str, details: String },
    #[error("worktree branch namespace is exhausted for node digest {node_digest}")]
    BranchNamespaceExhausted { node_digest: String },
    #[error("worktree node {node:?} was requested with a different base or auto_commit setting")]
    NodeConfigurationChanged { node: String },
    #[error("no manager-owned worktree exists for node {0:?}")]
    UnknownWorktreeNode(String),
    #[error("inherited path for node {node:?} must exactly match {expected}, received {actual}")]
    InheritedPathMismatch {
        node: String,
        expected: PathBuf,
        actual: PathBuf,
    },
    #[error(
        "manager-owned worktree for node {node:?} changed branch from {expected:?} to {actual:?}"
    )]
    WorktreeBranchChanged {
        node: String,
        expected: String,
        actual: String,
    },
    #[error("worktree for node {node:?} is dirty but has no stageable changes")]
    DirtyButNothingStaged { node: String },
    #[error("worktree for node {node:?} remained dirty after auto-commit")]
    WorktreeStillDirty { node: String },
}

#[derive(Debug)]
struct GitOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

#[derive(Debug)]
struct BoundedOutput {
    bytes: Vec<u8>,
    exceeded: bool,
}

type GitFuture<'a> = Pin<Box<dyn Future<Output = Result<GitOutput, WorktreeError>> + Send + 'a>>;

fn run_git<'a>(
    cwd: &'a Path,
    operation: &'static str,
    configure: impl FnOnce(&mut Command) + Send + 'a,
) -> GitFuture<'a> {
    Box::pin(async move {
        let mut command = Command::new("git");
        command
            .env_clear()
            .env("LC_ALL", "C")
            .env("LANG", "C")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", null_device())
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_PAGER", "cat")
            .env("PAGER", "cat")
            // Strengthen execution hardening for every Git invocation to prevent
            // repository hooks and repository-provided fsmonitor extensions.
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env("GIT_ATTR_GLOBAL", null_device())
            .arg("-C")
            .arg(cwd)
            .arg("-c")
            .arg(format!(
                "core.hooksPath={}",
                null_device().to_string_lossy()
            ))
            .arg("-c")
            .arg("core.fsmonitor=false")
            .arg("-c")
            .arg("diff.external=")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        if let Some(path) = std::env::var_os("PATH") {
            command.env("PATH", path);
        }
        configure(&mut command);

        let mut child = command
            .spawn()
            .map_err(|source| WorktreeError::GitSpawn { operation, source })?;
        let process_group = child.id();
        let stdout = child.stdout.take().expect("stdout is configured as piped");
        let stderr = child.stderr.take().expect("stderr is configured as piped");
        let completed = Box::pin(time::timeout(GIT_TIMEOUT, async {
            tokio::join!(
                child.wait(),
                read_bounded(stdout, GIT_OUTPUT_LIMIT),
                read_bounded(stderr, GIT_OUTPUT_LIMIT)
            )
        }))
        .await;
        let Ok((status, stdout, stderr)) = completed else {
            terminate_git_process(&mut child, process_group).await;
            return Err(WorktreeError::GitTimeout { operation });
        };
        let status = status.map_err(|source| WorktreeError::GitWait { operation, source })?;
        let stdout = stdout.map_err(|source| WorktreeError::GitWait { operation, source })?;
        let stderr = stderr.map_err(|source| WorktreeError::GitWait { operation, source })?;
        if stdout.exceeded {
            return Err(WorktreeError::GitOutputTooLarge {
                operation,
                stream: "stdout",
                limit: GIT_OUTPUT_LIMIT,
            });
        }
        if stderr.exceeded {
            return Err(WorktreeError::GitOutputTooLarge {
                operation,
                stream: "stderr",
                limit: GIT_OUTPUT_LIMIT,
            });
        }
        Ok(GitOutput {
            status,
            stdout: stdout.bytes,
            stderr: stderr.bytes,
        })
    })
}

async fn terminate_git_process(child: &mut tokio::process::Child, process_group: Option<u32>) {
    #[cfg(not(unix))]
    let _ = process_group;
    #[cfg(unix)]
    if let Some(process_group) = process_group {
        let process_group = format!("-{process_group}");
        let _ = tokio::process::Command::new("/bin/kill")
            .arg("-TERM")
            .arg(&process_group)
            .status()
            .await;
        let _ = tokio::process::Command::new("/bin/kill")
            .arg("-KILL")
            .arg(&process_group)
            .status()
            .await;
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}

async fn reject_external_git_drivers(repository: &Path) -> Result<(), WorktreeError> {
    let configured = run_git(
        repository,
        "inspect repository-local Git drivers",
        |command| {
            command.args([
                "config",
                "--local",
                "--name-only",
                "--get-regexp",
                r"^(filter\..*\.(clean|smudge|process)|diff\..*\.(command|textconv)|diff\.external)$",
            ]);
        },
    )
    .await?;
    match configured.status.code() {
        Some(1) => Ok(()),
        Some(0) => Err(WorktreeError::UnsafeGitDriverConfiguration),
        _ => Err(command_failed(
            "inspect repository-local Git drivers",
            &configured,
        )),
    }
}

async fn successful_git(
    cwd: &Path,
    operation: &'static str,
    configure: impl FnOnce(&mut Command) + Send,
) -> Result<GitOutput, WorktreeError> {
    let output = run_git(cwd, operation, configure).await?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(command_failed(operation, &output))
    }
}

async fn read_bounded(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
) -> Result<BoundedOutput, std::io::Error> {
    let mut bytes = Vec::new();
    let mut exceeded = false;
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(bytes.len());
        let retained = read.min(remaining);
        bytes.extend_from_slice(&buffer[..retained]);
        if retained != read {
            exceeded = true;
        }
    }
    Ok(BoundedOutput { bytes, exceeded })
}

fn command_failed(operation: &'static str, output: &GitOutput) -> WorktreeError {
    WorktreeError::GitCommand {
        operation,
        status: output.status.code(),
        stderr: bounded_text(&output.stderr),
    }
}

async fn resolve_commit(repository: &Path, base: &str) -> Result<String, WorktreeError> {
    let revision = format!("{base}^{{commit}}");
    let output = successful_git(repository, "resolve worktree base commit", |command| {
        command.args(["rev-parse", "--verify", "--end-of-options", &revision]);
    })
    .await?;
    parse_object_id(&output.stdout, "commit object id")
}

async fn worktree_is_dirty(path: &Path) -> Result<bool, WorktreeError> {
    let output = successful_git(path, "inspect node worktree status", |command| {
        command.args([
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ]);
    })
    .await?;
    Ok(!output.stdout.is_empty())
}

fn parse_object_id(bytes: &[u8], what: &'static str) -> Result<String, WorktreeError> {
    let value = text_from_git_output(bytes, what)?;
    if !matches!(value.len(), 40 | 64) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(WorktreeError::InvalidGitOutput {
            what,
            details: "expected a 40- or 64-character hexadecimal object id".to_owned(),
        });
    }
    Ok(value.to_ascii_lowercase())
}

fn path_from_git_output(bytes: &[u8], what: &'static str) -> Result<PathBuf, WorktreeError> {
    text_from_git_output(bytes, what).map(PathBuf::from)
}

fn text_from_git_output(bytes: &[u8], what: &'static str) -> Result<String, WorktreeError> {
    let value = std::str::from_utf8(bytes).map_err(|_| WorktreeError::InvalidGitOutput {
        what,
        details: "output is not UTF-8".to_owned(),
    })?;
    let value = value.trim_end_matches(['\r', '\n']);
    if value.is_empty() || value.contains(['\r', '\n', '\0']) {
        return Err(WorktreeError::InvalidGitOutput {
            what,
            details: "output is empty or contains multiple lines".to_owned(),
        });
    }
    Ok(value.to_owned())
}

fn bounded_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .trim_end_matches(['\r', '\n'])
        .to_owned()
}

fn validate_run_id(run_id: &str) -> Result<(), WorktreeError> {
    let mut chars = run_id.chars();
    let starts_valid = chars
        .next()
        .is_some_and(|character| character.is_ascii_alphanumeric());
    if !starts_valid
        || run_id.len() > MAX_RUN_ID_LEN
        || !chars
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(WorktreeError::InvalidRunId(run_id.to_owned()));
    }
    Ok(())
}

fn validate_node_id(node: &str) -> Result<(), WorktreeError> {
    if node.is_empty() || node.len() > MAX_NODE_ID_LEN || node.contains('\0') {
        return Err(WorktreeError::InvalidNodeId(node.to_owned()));
    }
    Ok(())
}

fn validate_base(base: Option<&str>) -> Result<(), WorktreeError> {
    if let Some(base) = base
        && (base.trim().is_empty()
            || base.len() > MAX_BASE_LEN
            || base.contains(['\0', '\r', '\n']))
    {
        return Err(WorktreeError::InvalidBase(base.to_owned()));
    }
    Ok(())
}

async fn ensure_directory(path: &Path) -> Result<(), WorktreeError> {
    match fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(WorktreeError::UnsafePath {
            path: path.to_path_buf(),
            reason: "path component is a symlink".to_owned(),
        }),
        Ok(metadata) if !metadata.is_dir() => Err(WorktreeError::UnsafePath {
            path: path.to_path_buf(),
            reason: "path component is not a directory".to_owned(),
        }),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match fs::create_dir(path).await {
                Ok(()) => Ok(()),
                Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
                    let metadata = fs::symlink_metadata(path).await.map_err(|source| {
                        WorktreeError::CreateDirectory {
                            path: path.to_path_buf(),
                            source,
                        }
                    })?;
                    if metadata.file_type().is_symlink() || !metadata.is_dir() {
                        Err(WorktreeError::UnsafePath {
                            path: path.to_path_buf(),
                            reason: "concurrently-created path is not a real directory".to_owned(),
                        })
                    } else {
                        Ok(())
                    }
                }
                Err(source) => Err(WorktreeError::CreateDirectory {
                    path: path.to_path_buf(),
                    source,
                }),
            }
        }
        Err(source) => Err(WorktreeError::CreateDirectory {
            path: path.to_path_buf(),
            source,
        }),
    }
}

async fn reject_existing_run_root(path: &Path) -> Result<(), WorktreeError> {
    match fs::symlink_metadata(path).await {
        Ok(_) => Err(WorktreeError::RunRootAlreadyExists(path.to_path_buf())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(WorktreeError::RepositoryPath {
            path: path.to_path_buf(),
            source,
        }),
    }
}

async fn reject_existing_worktree_path(path: &Path) -> Result<(), WorktreeError> {
    match fs::symlink_metadata(path).await {
        Ok(_) => Err(WorktreeError::WorktreePathAlreadyExists(path.to_path_buf())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(WorktreeError::RepositoryPath {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn null_device() -> OsString {
    #[cfg(windows)]
    {
        OsString::from("NUL")
    }
    #[cfg(not(windows))]
    {
        OsString::from("/dev/null")
    }
}

#[cfg(test)]
mod tests {
    use std::{fs as std_fs, process::Command as ProcessCommand};

    #[cfg(unix)]
    use std::os::unix::fs::{PermissionsExt, symlink};
    use tempfile::TempDir;

    use super::*;

    #[derive(Debug)]
    struct TestRepository {
        _temporary: TempDir,
        path: PathBuf,
    }

    impl TestRepository {
        fn new() -> Self {
            let temporary = tempfile::tempdir().expect("temporary repository");
            let path = temporary.path().to_path_buf();
            test_git_ok(&path, &["init", "--quiet"]);
            std_fs::write(path.join("tracked.txt"), "base\n").expect("write tracked file");
            std_fs::write(path.join("delete-me.txt"), "delete me\n")
                .expect("write file for deletion test");
            test_git_ok(&path, &["add", "--", "tracked.txt", "delete-me.txt"]);
            test_git_ok(
                &path,
                &[
                    "-c",
                    "user.name=test",
                    "-c",
                    "user.email=test@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "--quiet",
                    "--no-verify",
                    "--no-gpg-sign",
                    "-m",
                    "initial",
                ],
            );
            let path = std_fs::canonicalize(&path).expect("canonical repository");
            Self {
                _temporary: temporary,
                path,
            }
        }

        fn head(&self) -> String {
            test_git_text(&self.path, &["rev-parse", "HEAD"])
        }
    }

    fn test_git(repository: &Path, arguments: &[&str]) -> std::process::Output {
        let mut command = ProcessCommand::new("git");
        command
            .env_clear()
            .env("LC_ALL", "C")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", null_device())
            .env("GIT_TERMINAL_PROMPT", "0")
            .arg("-C")
            .arg(repository)
            .args(arguments);
        if let Some(path) = std::env::var_os("PATH") {
            command.env("PATH", path);
        }
        command.output().expect("run test Git command")
    }

    fn test_git_ok(repository: &Path, arguments: &[&str]) -> std::process::Output {
        let output = test_git(repository, arguments);
        assert!(
            output.status.success(),
            "git {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn test_git_text(repository: &Path, arguments: &[&str]) -> String {
        let output = test_git_ok(repository, arguments);
        String::from_utf8(output.stdout)
            .expect("UTF-8 Git output")
            .trim()
            .to_owned()
    }

    #[tokio::test]
    async fn rejects_non_repository_and_non_top_level_paths() {
        let non_repository = tempfile::tempdir().expect("temporary non-repository");
        let result = GitWorktreeManager::new(non_repository.path(), "run1").await;
        assert!(matches!(result, Err(WorktreeError::GitCommand { .. })));

        let repository = TestRepository::new();
        let subdirectory = repository.path.join("subdir");
        std_fs::create_dir(&subdirectory).expect("create subdirectory");
        let result = GitWorktreeManager::new(&subdirectory, "run2").await;
        assert!(matches!(
            result,
            Err(WorktreeError::RepositoryTopLevelMismatch { .. })
        ));
    }

    #[tokio::test]
    async fn preflight_rejects_dirty_source_but_excludes_gloop() {
        let repository = TestRepository::new();
        std_fs::write(repository.path.join("tracked.txt"), "dirty\n").expect("dirty tracked file");
        let result = GitWorktreeManager::new(&repository.path, "dirty").await;
        assert!(matches!(result, Err(WorktreeError::DirtyRepository { .. })));

        std_fs::write(repository.path.join("tracked.txt"), "base\n").expect("restore tracked file");
        std_fs::create_dir(repository.path.join(".gloop")).expect("create .gloop");
        std_fs::write(repository.path.join(".gloop/existing-artifact"), "retained")
            .expect("write retained artifact");
        GitWorktreeManager::new(&repository.path, "clean")
            .await
            .expect(".gloop is excluded from preflight");
    }

    #[tokio::test]
    async fn rejects_invalid_run_node_and_base_values() {
        let repository = TestRepository::new();
        for invalid in ["", "../escape", "-leading", "contains/slash"] {
            let result = GitWorktreeManager::new(&repository.path, invalid).await;
            assert!(matches!(result, Err(WorktreeError::InvalidRunId(_))));
        }

        let manager = GitWorktreeManager::new(&repository.path, "valid")
            .await
            .expect("create manager");
        let result = manager.workspace_for_node("", None, false).await;
        assert!(matches!(result, Err(WorktreeError::InvalidNodeId(_))));
        let result = manager.workspace_for_node("node", Some("   "), false).await;
        assert!(matches!(result, Err(WorktreeError::InvalidBase(_))));
        let result = manager
            .workspace_for_node("node", Some("missing-base"), false)
            .await;
        assert!(matches!(result, Err(WorktreeError::GitCommand { .. })));
    }

    #[tokio::test]
    async fn sibling_worktrees_are_isolated_and_leave_source_untouched() {
        let repository = TestRepository::new();
        let source_head = repository.head();
        let manager = GitWorktreeManager::new(&repository.path, "siblings")
            .await
            .expect("create manager");
        let first = manager
            .workspace_for_node("outer.first", None, false)
            .await
            .expect("first worktree");
        let second = manager
            .workspace_for_node("outer.second", None, false)
            .await
            .expect("second worktree");
        assert_ne!(first.path, second.path);
        assert_ne!(first.branch, second.branch);
        assert_eq!(first.base_commit, source_head);
        assert_eq!(second.base_commit, source_head);

        std_fs::write(first.path.join("first-only.txt"), "first").expect("write first worktree");
        std_fs::write(second.path.join("second-only.txt"), "second")
            .expect("write second worktree");
        assert!(!second.path.join("first-only.txt").exists());
        assert!(!first.path.join("second-only.txt").exists());
        assert!(!repository.path.join("first-only.txt").exists());
        assert!(!repository.path.join("second-only.txt").exists());
        assert_eq!(repository.head(), source_head);

        let status = test_git_ok(
            &repository.path,
            &[
                "status",
                "--porcelain=v1",
                "--untracked-files=all",
                "--",
                ".",
                ":(exclude).gloop",
                ":(exclude).gloop/**",
            ],
        );
        assert!(status.stdout.is_empty());
    }

    #[tokio::test]
    async fn retry_reuses_worktree_and_rejects_configuration_changes() {
        let repository = TestRepository::new();
        let manager = GitWorktreeManager::new(&repository.path, "retry")
            .await
            .expect("create manager");
        std_fs::write(
            repository.path.join("tracked.txt"),
            "changed after preflight\n",
        )
        .expect("modify source after manager construction");
        let first = manager
            .workspace_for_node("nested.node", Some("HEAD"), false)
            .await
            .expect("create worktree");
        std_fs::write(first.path.join("retry-state.txt"), "preserved").expect("write retry state");
        let retry = manager
            .workspace_for_node("nested.node", Some("HEAD"), false)
            .await
            .expect("reuse worktree");
        assert_eq!(retry, first);
        assert_eq!(
            std_fs::read_to_string(retry.path.join("retry-state.txt")).expect("read retry state"),
            "preserved"
        );

        let changed_base = manager
            .workspace_for_node("nested.node", Some("HEAD~0"), false)
            .await;
        assert!(matches!(
            changed_base,
            Err(WorktreeError::NodeConfigurationChanged { .. })
        ));
        let changed_commit_mode = manager
            .workspace_for_node("nested.node", Some("HEAD"), true)
            .await;
        assert!(matches!(
            changed_commit_mode,
            Err(WorktreeError::NodeConfigurationChanged { .. })
        ));
        assert_eq!(manager.record_for("nested.node").await, Some(first));
    }

    #[tokio::test]
    async fn concurrent_requests_create_exactly_one_node_worktree() {
        let repository = TestRepository::new();
        let manager = GitWorktreeManager::new(&repository.path, "concurrent")
            .await
            .expect("create manager");
        let (first, second) = tokio::join!(
            manager.workspace_for_node("same.node", None, true),
            manager.workspace_for_node("same.node", None, true)
        );
        let first = first.expect("first concurrent request");
        let second = second.expect("second concurrent request");
        assert_eq!(first, second);
        let manifest = manager.manifest().await.expect("worktree manifest");
        assert_eq!(manifest.records, vec![first]);
        assert_eq!(
            test_git_text(
                &repository.path,
                &[
                    "branch",
                    "--list",
                    &second.branch,
                    "--format=%(refname:short)"
                ]
            ),
            second.branch
        );
    }

    #[tokio::test]
    async fn inherited_workspace_requires_exact_manager_owned_path() {
        let repository = TestRepository::new();
        let manager = GitWorktreeManager::new(&repository.path, "inherit")
            .await
            .expect("create manager");
        let record = manager
            .workspace_for_node("producer", None, false)
            .await
            .expect("producer worktree");
        let inherited = manager
            .inherit_workspace("producer", &record.path)
            .await
            .expect("inherit exact path");
        assert_eq!(inherited, record);

        let lexical_alias = record.path.join(".");
        let result = manager.inherit_workspace("producer", &lexical_alias).await;
        assert!(matches!(
            result,
            Err(WorktreeError::InheritedPathMismatch { .. })
        ));
        let result = manager
            .inherit_workspace("producer", &repository.path)
            .await;
        assert!(matches!(
            result,
            Err(WorktreeError::InheritedPathMismatch { .. })
        ));
        let result = manager.inherit_workspace("unknown", &record.path).await;
        assert!(matches!(result, Err(WorktreeError::UnknownWorktreeNode(_))));
    }

    #[tokio::test]
    async fn inherited_mutation_commits_on_the_worktree_owner_branch() {
        let repository = TestRepository::new();
        let manager = GitWorktreeManager::new(&repository.path, "inherit-commit")
            .await
            .expect("create manager");
        let producer = manager
            .workspace_for_node("producer", None, true)
            .await
            .expect("producer worktree");
        std_fs::write(producer.path.join("implementation.txt"), "implementation\n")
            .expect("write implementation");
        let producer_finished = manager
            .finish_workspace_success(&producer.workspace())
            .await
            .expect("commit producer changes");

        let inherited = manager
            .inherit_workspace("producer", &producer.path)
            .await
            .expect("inherit producer worktree");
        let inherited_workspace = inherited.workspace();
        assert_eq!(inherited_workspace.owner_node, "producer");
        assert_eq!(inherited_workspace.path, producer.path);
        let workspace_json = serde_json::to_vec(&inherited_workspace)
            .expect("serialize inherited workspace identity");
        let decoded_workspace: WorktreeWorkspace = serde_json::from_slice(&workspace_json)
            .expect("deserialize inherited workspace identity");
        assert_eq!(decoded_workspace, inherited_workspace);
        std_fs::write(inherited_workspace.path.join("repair.txt"), "repair\n")
            .expect("write inherited repair");
        let repair_finished = manager
            .finish_workspace_success(&inherited_workspace)
            .await
            .expect("commit inherited repair on owner branch");

        assert_eq!(repair_finished.node, "producer");
        assert_eq!(repair_finished.branch, producer.branch);
        assert_ne!(repair_finished.commit, producer_finished.commit);
        assert!(!repair_finished.dirty);
        assert_eq!(
            test_git_text(&repair_finished.path, &["show", "HEAD:implementation.txt"]),
            "implementation"
        );
        assert_eq!(
            test_git_text(&repair_finished.path, &["show", "HEAD:repair.txt"]),
            "repair"
        );
    }

    #[tokio::test]
    async fn auto_commit_stages_everything_without_hooks_or_signing() {
        let repository = TestRepository::new();
        test_git_ok(&repository.path, &["config", "commit.gpgsign", "true"]);
        #[cfg(unix)]
        {
            let hook = repository.path.join(".git/hooks/pre-commit");
            std_fs::write(&hook, "#!/bin/sh\nexit 91\n").expect("write rejecting hook");
            let mut permissions = std_fs::metadata(&hook)
                .expect("hook metadata")
                .permissions();
            permissions.set_mode(0o755);
            std_fs::set_permissions(&hook, permissions).expect("make hook executable");
        }

        let manager = GitWorktreeManager::new(&repository.path, "autocommit")
            .await
            .expect("create manager");
        let base_commit = manager.base_commit().to_owned();
        let record = manager
            .workspace_for_node("writer", None, true)
            .await
            .expect("writer worktree");
        std_fs::write(record.path.join("created.txt"), "created\n").expect("create untracked file");
        std_fs::write(record.path.join("tracked.txt"), "updated\n").expect("update tracked file");
        std_fs::remove_file(record.path.join("delete-me.txt")).expect("delete tracked file");
        let finished = manager
            .finish_node_success("writer")
            .await
            .expect("auto-commit changes");
        assert!(!finished.dirty);
        assert_ne!(finished.commit, base_commit);
        assert_eq!(
            test_git_text(&finished.path, &["show", "HEAD:created.txt"]),
            "created"
        );
        assert_eq!(
            test_git_text(&finished.path, &["show", "HEAD:tracked.txt"]),
            "updated"
        );
        assert!(
            !test_git(&finished.path, &["cat-file", "-e", "HEAD:delete-me.txt"])
                .status
                .success()
        );
        assert_eq!(
            test_git_text(&finished.path, &["log", "-1", "--format=%an|%ae|%cn|%ce"]),
            "gloop|gloop@localhost|gloop|gloop@localhost"
        );

        let repeated = manager
            .finish_node_success("writer")
            .await
            .expect("finishing an already clean worktree is idempotent");
        assert_eq!(repeated.commit, finished.commit);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn worktree_add_does_not_execute_post_checkout_hook() {
        let repository = TestRepository::new();
        let marker = repository.path.join(".gloop/post-checkout-ran");
        let hook = repository.path.join(".git/hooks/post-checkout");
        std_fs::write(
            &hook,
            "#!/bin/sh\nprintf 'ran' > \"$GIT_DIR/../.gloop/post-checkout-ran\"\n",
        )
        .expect("write post-checkout hook");
        let mut permissions = std_fs::metadata(&hook)
            .expect("post-checkout hook metadata")
            .permissions();
        permissions.set_mode(0o755);
        std_fs::set_permissions(&hook, permissions).expect("make hook executable");

        let manager = GitWorktreeManager::new(&repository.path, "post-checkout")
            .await
            .expect("create manager");
        let _ = manager
            .workspace_for_node("node", None, false)
            .await
            .expect("create node worktree");
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn worktree_mode_rejects_repository_local_external_filters() {
        let repository = TestRepository::new();
        let marker = repository.path.join("filter-ran");
        test_git_ok(
            &repository.path,
            &[
                "config",
                "--local",
                "filter.gloop-test.smudge",
                &format!("touch {}", marker.display()),
            ],
        );

        let error = GitWorktreeManager::new(&repository.path, "external-filter")
            .await
            .expect_err("external filters must fail closed");
        assert!(matches!(error, WorktreeError::UnsafeGitDriverConfiguration));
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn worktree_mode_rejects_repository_local_textconv_drivers() {
        let repository = TestRepository::new();
        test_git_ok(
            &repository.path,
            &["config", "--local", "diff.gloop-test.textconv", "false"],
        );

        let error = GitWorktreeManager::new(&repository.path, "external-textconv")
            .await
            .expect_err("external textconv drivers must fail closed");
        assert!(matches!(error, WorktreeError::UnsafeGitDriverConfiguration));
    }

    #[tokio::test]
    async fn auto_commit_does_not_create_empty_commit() {
        let repository = TestRepository::new();
        let manager = GitWorktreeManager::new(&repository.path, "nochange")
            .await
            .expect("create manager");
        let record = manager
            .workspace_for_node("reader", None, true)
            .await
            .expect("reader worktree");
        let count_before = test_git_text(&record.path, &["rev-list", "--count", "HEAD"]);
        let finished = manager
            .finish_node_success("reader")
            .await
            .expect("finish unchanged worktree");
        assert_eq!(finished.commit, finished.base_commit);
        assert!(!finished.dirty);
        assert_eq!(
            test_git_text(&record.path, &["rev-list", "--count", "HEAD"]),
            count_before
        );
    }

    #[tokio::test]
    async fn auto_commit_false_retains_dirty_changes_and_manifest_reports_them() {
        let repository = TestRepository::new();
        let manager = GitWorktreeManager::new(&repository.path, "manual")
            .await
            .expect("create manager");
        let record = manager
            .workspace_for_node("writer", None, false)
            .await
            .expect("manual worktree");
        std_fs::write(record.path.join("uncommitted.txt"), "retained")
            .expect("write uncommitted file");
        let finished = manager
            .finish_node_success("writer")
            .await
            .expect("finish without commit");
        assert!(finished.dirty);
        assert_eq!(finished.commit, finished.base_commit);
        assert!(
            !test_git(&finished.path, &["cat-file", "-e", "HEAD:uncommitted.txt"])
                .status
                .success()
        );

        let manifest = manager.manifest().await.expect("worktree manifest");
        assert_eq!(manifest.base_commit, repository.head());
        assert_eq!(manifest.records, vec![finished]);
        let encoded = serde_json::to_vec(&manifest).expect("serialize manifest");
        let decoded: WorktreeManifest =
            serde_json::from_slice(&encoded).expect("deserialize manifest");
        assert_eq!(decoded, manifest);
    }

    #[tokio::test]
    async fn rejects_preexisting_run_and_node_paths() {
        let repository = TestRepository::new();
        let existing_root = repository.path.join(".gloop/worktrees/existing");
        std_fs::create_dir_all(&existing_root).expect("create preexisting run root");
        let result = GitWorktreeManager::new(&repository.path, "existing").await;
        assert!(matches!(
            result,
            Err(WorktreeError::RunRootAlreadyExists(_))
        ));

        let manager = GitWorktreeManager::new(&repository.path, "nodepath")
            .await
            .expect("create manager");
        let digest = hex::encode(Sha256::digest(b"node"));
        std_fs::create_dir(manager.root().join(&digest[..INITIAL_ID_PREFIX_LEN]))
            .expect("create preexisting node path");
        let result = manager.workspace_for_node("node", None, false).await;
        assert!(matches!(
            result,
            Err(WorktreeError::WorktreePathAlreadyExists(_))
        ));
    }

    #[tokio::test]
    async fn extends_hash_prefix_when_generated_branch_already_exists() {
        let repository = TestRepository::new();
        let manager = GitWorktreeManager::new(&repository.path, "collision")
            .await
            .expect("create manager");
        let digest = hex::encode(Sha256::digest(b"node"));
        let occupied = format!("gloop/collision/{}", &digest[..INITIAL_ID_PREFIX_LEN]);
        test_git_ok(&repository.path, &["branch", &occupied, "HEAD"]);

        let record = manager
            .workspace_for_node("node", None, false)
            .await
            .expect("select a longer unique prefix");
        assert_eq!(
            record.branch,
            format!("gloop/collision/{}", &digest[..INITIAL_ID_PREFIX_LEN + 8])
        );
        assert!(record.path.ends_with(&digest[..INITIAL_ID_PREFIX_LEN + 8]));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlinked_worktree_roots_and_parents() {
        let repository = TestRepository::new();
        let target = tempfile::tempdir().expect("symlink target");
        let worktrees = repository.path.join(".gloop/worktrees");
        std_fs::create_dir_all(&worktrees).expect("create worktree parent");
        symlink(target.path(), worktrees.join("linked")).expect("symlink run root");
        let result = GitWorktreeManager::new(&repository.path, "linked").await;
        assert!(matches!(
            result,
            Err(WorktreeError::RunRootAlreadyExists(_))
        ));

        let second = TestRepository::new();
        std_fs::create_dir(second.path.join(".gloop")).expect("create .gloop");
        symlink(target.path(), second.path.join(".gloop/worktrees"))
            .expect("symlink worktrees parent");
        let result = GitWorktreeManager::new(&second.path, "linked-parent").await;
        assert!(matches!(result, Err(WorktreeError::UnsafePath { .. })));

        let third = TestRepository::new();
        let manager = GitWorktreeManager::new(&third.path, "swapped")
            .await
            .expect("create manager");
        let original_root = manager.root().with_file_name("swapped-original");
        std_fs::rename(manager.root(), &original_root).expect("move owned root");
        symlink(target.path(), manager.root()).expect("replace root with symlink");
        let result = manager.workspace_for_node("node", None, false).await;
        assert!(matches!(result, Err(WorktreeError::UnsafePath { .. })));
    }
}
