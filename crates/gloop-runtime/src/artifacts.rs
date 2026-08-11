#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;

use gloop_core::{Graph, RunSummary, state::ArtifactRef};
use serde::Serialize;
use sha2::{Digest, Sha256};
#[cfg(all(test, unix))]
use std::os::unix::fs::symlink;
use thiserror::Error;
use tokio::fs;

use crate::worktree::WorktreeManifest;

const NODE_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

const GRAPH_FILE: &str = "graph.json";
const JOURNAL_FILE: &str = "journal.jsonl";
const SUMMARY_FILE: &str = "summary.json";
const WORKTREE_MANIFEST_FILE: &str = "worktree-manifest.json";
const MAX_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunPaths {
    pub root: PathBuf,
    pub graph: PathBuf,
    pub journal: PathBuf,
    pub summary: PathBuf,
    pub nodes: PathBuf,
}

impl RunPaths {
    pub fn new(base: impl AsRef<Path>, run_id: &str) -> Result<Self, ArtifactError> {
        validate_component(run_id)?;
        let root = base.as_ref().join(run_id);
        Ok(Self {
            graph: root.join(GRAPH_FILE),
            journal: root.join(JOURNAL_FILE),
            summary: root.join(SUMMARY_FILE),
            nodes: root.join("nodes"),
            root,
        })
    }

    pub fn from_root(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            graph: root.join(GRAPH_FILE),
            journal: root.join(JOURNAL_FILE),
            summary: root.join(SUMMARY_FILE),
            nodes: root.join("nodes"),
            root,
        }
    }

    pub fn attempt_dir(
        &self,
        qualified_node_id: &str,
        attempt: u32,
    ) -> Result<PathBuf, ArtifactError> {
        if attempt == 0 {
            return Err(ArtifactError::InvalidComponent(
                "attempt number must be at least one".to_owned(),
            ));
        }
        Ok(self
            .nodes
            .join(encoded_node_path(qualified_node_id)?)
            .join(format!("attempt-{attempt}")))
    }
}

#[derive(Debug, Clone)]
pub struct ArtifactStore {
    paths: RunPaths,
}

impl ArtifactStore {
    pub async fn create(base: impl AsRef<Path>, run_id: &str) -> Result<Self, ArtifactError> {
        let paths = RunPaths::new(base, run_id)?;
        verify_no_symlink(paths.root.as_path()).await?;
        fs::create_dir_all(
            paths
                .root
                .parent()
                .ok_or_else(|| ArtifactError::InvalidComponent("run root has no parent".into()))?,
        )
        .await?;
        match fs::create_dir(&paths.root).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(ArtifactError::RunAlreadyExists(paths.root));
            }
            Err(error) => return Err(error.into()),
        }
        secure_set_permissions(&paths.root, NODE_MODE).await?;
        fs::create_dir(&paths.nodes).await?;
        secure_set_permissions(&paths.nodes, NODE_MODE).await?;
        Ok(Self { paths })
    }

    pub fn open(root: impl Into<PathBuf>) -> Self {
        Self {
            paths: RunPaths::from_root(root),
        }
    }

    pub fn paths(&self) -> &RunPaths {
        &self.paths
    }

    pub async fn write_graph(&self, graph: &Graph) -> Result<ArtifactRef, ArtifactError> {
        self.write_json_atomic(&self.paths.graph, graph).await?;
        self.reference(&self.paths.graph, "graph").await
    }

    pub async fn write_summary(&self, summary: &RunSummary) -> Result<(), ArtifactError> {
        self.write_json_atomic(&self.paths.summary, summary).await
    }

    pub async fn write_summary_snapshot<T: Serialize>(
        &self,
        snapshot: &T,
    ) -> Result<(), ArtifactError> {
        self.write_json_atomic(&self.paths.root.join("summary.snapshot.json"), snapshot)
            .await
    }

    pub async fn write_worktree_manifest(
        &self,
        manifest: &WorktreeManifest,
    ) -> Result<ArtifactRef, ArtifactError> {
        let path = self.paths.root.join(WORKTREE_MANIFEST_FILE);
        self.write_json_atomic(&path, manifest).await?;
        self.reference(path, "worktree_manifest").await
    }

    pub async fn write_attempt(
        &self,
        qualified_node_id: &str,
        attempt: u32,
        stdout: &[u8],
        stderr: &[u8],
        output: &[u8],
        output_is_json: bool,
    ) -> Result<AttemptArtifacts, ArtifactError> {
        let directory = self.paths.attempt_dir(qualified_node_id, attempt)?;
        verify_no_symlink(&directory).await?;
        verify_no_symlink(&self.paths.nodes).await?;
        if fs::metadata(&directory).await.is_ok() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("attempt directory already exists: {}", directory.display()),
            )
            .into());
        }
        if let Some(node_dir) = directory.parent()
            && fs::metadata(node_dir).await.is_err()
        {
            fs::create_dir(node_dir).await?;
            secure_set_permissions(node_dir, NODE_MODE).await?;
        }
        fs::create_dir(&directory).await?;
        secure_set_permissions(&directory, NODE_MODE).await?;
        let stdout_path = directory.join("stdout.txt");
        let stderr_path = directory.join("stderr.txt");
        let output_path = directory.join(if output_is_json {
            "output.json"
        } else {
            "output.txt"
        });
        write_file_new(&stdout_path, stdout, FILE_MODE).await?;
        write_file_new(&stderr_path, stderr, FILE_MODE).await?;
        write_file_new(&output_path, output, FILE_MODE).await?;
        Ok(AttemptArtifacts {
            stdout: self.relative_string(&stdout_path)?,
            stderr: self.relative_string(&stderr_path)?,
            output: self.relative_string(&output_path)?,
        })
    }

    pub async fn reference(
        &self,
        path: impl AsRef<Path>,
        kind: impl Into<String>,
    ) -> Result<ArtifactRef, ArtifactError> {
        let path = path.as_ref();
        let relative_path = self.relative_string(path)?;
        verify_no_inrun_symlinks(path, &self.paths.root).await?;
        let metadata = fs::metadata(path).await?;
        if !metadata.file_type().is_file() {
            return Err(ArtifactError::InvalidComponent(format!(
                "artifact path is not a file: {}",
                path.to_string_lossy()
            )));
        }
        if metadata.len() > MAX_ARTIFACT_BYTES {
            return Err(ArtifactError::InvalidComponent(format!(
                "artifact file too large: {} > {}",
                metadata.len(),
                MAX_ARTIFACT_BYTES
            )));
        }
        let bytes = fs::read(path).await?;
        Ok(ArtifactRef {
            kind: kind.into(),
            path: relative_path,
            size: Some(metadata.len()),
            sha256: Some(hex::encode(Sha256::digest(bytes))),
        })
    }

    fn relative_string(&self, path: &Path) -> Result<String, ArtifactError> {
        path.strip_prefix(&self.paths.root)
            .map(|relative| relative.to_string_lossy().into_owned())
            .map_err(|_| ArtifactError::OutsideRun(path.to_path_buf()))
    }

    async fn write_json_atomic<T: Serialize>(
        &self,
        destination: &Path,
        value: &T,
    ) -> Result<(), ArtifactError> {
        verify_no_symlink(destination).await?;
        let bytes = serde_json::to_vec_pretty(value)?;
        let file_name = destination
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| ArtifactError::InvalidComponent("invalid artifact filename".into()))?;
        let mut attempts = 0u32;
        let temp_base = random_suffix();
        let temporary = loop {
            let temp_path = if attempts == 0 {
                destination.with_file_name(format!(".{file_name}.tmp.{temp_base}"))
            } else {
                destination.with_file_name(format!(".{file_name}.tmp.{temp_base}.{attempts}"))
            };
            match write_file_new(temp_path.as_path(), &bytes, FILE_MODE).await {
                Ok(_file) => {
                    break temp_path;
                }
                Err(ArtifactError::Io(error))
                    if error.kind() == std::io::ErrorKind::AlreadyExists && attempts < 32 =>
                {
                    attempts += 1;
                }
                Err(error) => return Err(error),
            }
        };
        match fs::rename(&temporary, destination).await {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = fs::remove_file(&temporary).await;
                Err(error.into())
            }
        }
    }
}

fn random_suffix() -> u64 {
    TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
fn set_temp_suffix_for_test(value: u64) {
    TEMP_FILE_COUNTER.store(value, Ordering::SeqCst);
}

#[cfg(unix)]
async fn secure_set_permissions(path: &Path, mode: u32) -> Result<(), ArtifactError> {
    let permissions = std::fs::Permissions::from_mode(mode);
    fs::set_permissions(path, permissions).await?;
    Ok(())
}

#[cfg(not(unix))]
async fn secure_set_permissions(_path: &Path, _mode: u32) -> Result<(), ArtifactError> {
    Ok(())
}

async fn write_file_new(
    path: &Path,
    bytes: &[u8],
    mode: u32,
) -> Result<tokio::fs::File, ArtifactError> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .await?;
    if let Err(error) = set_file_permissions(&file, mode).await {
        let _ = fs::remove_file(path).await;
        return Err(error);
    }
    if let Err(error) = file.write_all(bytes).await {
        let _ = fs::remove_file(path).await;
        return Err(error.into());
    }
    if let Err(error) = file.flush().await {
        let _ = fs::remove_file(path).await;
        return Err(error.into());
    }
    if let Err(error) = file.sync_all().await {
        let _ = fs::remove_file(path).await;
        return Err(error.into());
    }
    Ok(file)
}

#[cfg(unix)]
async fn set_file_permissions(file: &tokio::fs::File, mode: u32) -> Result<(), ArtifactError> {
    file.set_permissions(std::fs::Permissions::from_mode(mode))
        .await
        .map_err(ArtifactError::Io)
}

#[cfg(not(unix))]
async fn set_file_permissions(_: &tokio::fs::File, _mode: u32) -> Result<(), ArtifactError> {
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptArtifacts {
    pub stdout: String,
    pub stderr: String,
    pub output: String,
}

#[derive(Debug, Error)]
pub enum ArtifactError {
    #[error("invalid artifact path component: {0}")]
    InvalidComponent(String),
    #[error("run artifact directory already exists: {0}")]
    RunAlreadyExists(PathBuf),
    #[error("artifact is outside the run directory: {0}")]
    OutsideRun(PathBuf),
    #[error("artifact I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("artifact serialization failed: {0}")]
    Json(#[from] serde_json::Error),
}

fn validate_component(value: &str) -> Result<(), ArtifactError> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(ArtifactError::InvalidComponent(value.to_owned()));
    }
    Ok(())
}

fn encoded_node_path(value: &str) -> Result<String, ArtifactError> {
    if value.is_empty() || value.contains('\0') {
        return Err(ArtifactError::InvalidComponent(value.to_owned()));
    }
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(encoded, "~{byte:02x}");
        }
    }
    Ok(encoded)
}

async fn verify_no_symlink(path: &Path) -> Result<(), ArtifactError> {
    // The store owns `<repo>/.gloop/runs`, not the spelling of system-level
    // ancestors such as macOS `/var -> /private/var`. Reject symlinks at the
    // two writable store components while allowing a repository reached
    // through a legitimate symlinked system path.
    for candidate in [
        Some(path),
        path.parent(),
        path.parent().and_then(Path::parent),
    ]
    .into_iter()
    .flatten()
    {
        match fs::symlink_metadata(candidate).await {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(ArtifactError::InvalidComponent(format!(
                    "artifact path component is a symlink: {}",
                    candidate.to_string_lossy()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

async fn verify_no_inrun_symlinks(path: &Path, root: &Path) -> Result<(), ArtifactError> {
    let mut candidate = Some(path);
    while let Some(current) = candidate {
        if current == root {
            break;
        }
        let metadata = match fs::symlink_metadata(current).await {
            Ok(metadata) => metadata,
            Err(error) => return Err(error.into()),
        };
        if metadata.file_type().is_symlink() {
            return Err(ArtifactError::InvalidComponent(format!(
                "artifact path component is a symlink: {}",
                current.to_string_lossy()
            )));
        }
        candidate = current.parent();
    }
    if candidate.is_none() {
        return Err(ArtifactError::OutsideRun(path.to_path_buf()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn creates_stable_attempt_layout() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let store = ArtifactStore::create(temporary.path(), "run_1")
            .await
            .expect("create store");
        let written = store
            .write_attempt("outer.iteration-1/inner", 2, b"out", b"err", b"{}", true)
            .await
            .expect("write attempt");
        assert!(written.output.ends_with("attempt-2/output.json"));
        assert_eq!(
            fs::read(store.paths().root.join(&written.stdout))
                .await
                .expect("read stdout"),
            b"out"
        );
    }

    #[tokio::test]
    async fn refuses_to_reuse_a_run_directory() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        ArtifactStore::create(temporary.path(), "same")
            .await
            .expect("first create");
        let error = ArtifactStore::create(temporary.path(), "same")
            .await
            .expect_err("duplicate rejected");
        assert!(matches!(error, ArtifactError::RunAlreadyExists(_)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn create_refuses_symlink_base_directories() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let symlinked = temporary.path().join("runs-symlink");
        let target = tempfile::tempdir().expect("target directory");
        symlink(target.path(), &symlinked).expect("symlink base");

        let error = ArtifactStore::create(symlinked, "run").await;
        assert!(matches!(error, Err(ArtifactError::InvalidComponent(_))));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn create_refuses_symlink_store_parent() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let target = tempfile::tempdir().expect("target directory");
        let store_parent = temporary.path().join(".gloop");
        symlink(target.path(), &store_parent).expect("symlink store parent");

        let error = ArtifactStore::create(store_parent.join("runs"), "run").await;
        assert!(
            matches!(error, Err(ArtifactError::InvalidComponent(_))),
            "unexpected result: {error:?}"
        );
    }

    #[tokio::test]
    async fn write_attempt_refuses_existing_attempt_directory() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let store = ArtifactStore::create(temporary.path(), "run_1")
            .await
            .expect("create store");
        let attempt_dir = store
            .paths()
            .attempt_dir("outer.iteration-1/inner", 1)
            .expect("attempt dir");
        tokio::fs::create_dir_all(&attempt_dir)
            .await
            .expect("create attempt dir");
        assert!(
            store
                .write_attempt("outer.iteration-1/inner", 1, b"out", b"err", b"{}", false)
                .await
                .is_err(),
            "preexisting attempt path must be rejected"
        );
    }

    #[tokio::test]
    async fn write_json_atomic_tolerates_temp_collision() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let store = ArtifactStore::create(temporary.path(), "run_1")
            .await
            .expect("create store");
        let graph_path = store.paths().graph.clone();
        let file_name = graph_path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("graph file name");
        set_temp_suffix_for_test(0);
        let temp_path = graph_path.with_file_name(format!(".{file_name}.tmp.0"));
        tokio::fs::write(&temp_path, b"blocked")
            .await
            .expect("precreate temp collision");

        store
            .write_graph(&Graph::new("root", "gloop", vec![]))
            .await
            .expect("write graph");
        fs::metadata(&graph_path).await.expect("graph file exists");
    }

    #[tokio::test]
    async fn reference_rejects_paths_outside_run_root() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let store = ArtifactStore::create(temporary.path(), "run_1")
            .await
            .expect("create store");
        let outside = tempfile::tempdir().expect("outside root");
        let outside_file = outside.path().join("outside.txt");
        tokio::fs::write(&outside_file, b"outside artifact")
            .await
            .expect("write outside file");

        let error = store
            .reference(&outside_file, "outside")
            .await
            .expect_err("outside path rejected");
        assert!(
            matches!(error, ArtifactError::OutsideRun(_)),
            "unexpected error: {error:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reference_rejects_symlink_path_before_read() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let store = ArtifactStore::create(temporary.path(), "run_1")
            .await
            .expect("create store");
        let outside = tempfile::tempdir().expect("outside root");
        let outside_file = outside.path().join("outside.txt");
        tokio::fs::write(&outside_file, b"outside artifact")
            .await
            .expect("write outside file");
        let symlink_path = store.paths().root.join("linked");
        symlink(&outside_file, &symlink_path).expect("create symlink");

        let error = store
            .reference(&symlink_path, "symlinked")
            .await
            .expect_err("symlink path rejected");
        assert!(
            matches!(error, ArtifactError::InvalidComponent(_)),
            "unexpected error: {error:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_attempt_enforces_private_permissions() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let store = ArtifactStore::create(temporary.path(), "run_1")
            .await
            .expect("create store");

        store
            .write_attempt("node", 1, b"out", b"err", b"{}", false)
            .await
            .expect("write attempt");

        let node_dir = store
            .paths()
            .nodes
            .join(encoded_node_path("node").expect("node key"));
        let attempt_dir = node_dir.join("attempt-1");

        let node_mode = fs::metadata(&node_dir)
            .await
            .expect("node dir metadata")
            .permissions()
            .mode()
            & 0o7777;
        let attempt_mode = fs::metadata(&attempt_dir)
            .await
            .expect("attempt dir metadata")
            .permissions()
            .mode()
            & 0o7777;

        assert_eq!(node_mode, NODE_MODE);
        assert_eq!(attempt_mode, NODE_MODE);
    }
}
