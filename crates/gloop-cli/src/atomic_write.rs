use std::{
    ffi::OsString,
    fs,
    fs::OpenOptions,
    io,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

// The write primitives are synchronous: they are small local file operations,
// and the interactive wizard calls them from a non-async context where
// spinning up (or blocking) a Tokio runtime would panic inside the CLI's own
// runtime. Async call sites use the thin `spawn_blocking` wrappers below so
// both paths share one implementation.

pub(crate) fn write_text_no_replace_sync(path: &Path, content: &str) -> io::Result<()> {
    let path = canonical_write_path(path)?;
    write_text_new(path.as_path(), content.as_bytes())
}

pub(crate) async fn write_text_no_replace(path: &Path, content: &str) -> io::Result<()> {
    let path = path.to_path_buf();
    let content = content.to_owned();
    tokio::task::spawn_blocking(move || write_text_no_replace_sync(&path, &content))
        .await
        .map_err(io::Error::other)?
}

pub(crate) async fn write_text_atomic(path: &Path, content: &str) -> io::Result<()> {
    let path = path.to_path_buf();
    let content = content.to_owned();
    tokio::task::spawn_blocking(move || write_text_atomic_sync(&path, &content))
        .await
        .map_err(io::Error::other)?
}

pub(crate) fn write_text_atomic_sync(path: &Path, content: &str) -> io::Result<()> {
    let path = canonical_write_path(path)?;

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let mut attempts = 0u32;
    let tmp_path = loop {
        let attempt_suffix = if attempts == 0 {
            String::new()
        } else {
            format!(".{attempts}")
        };
        let candidate = path.with_file_name(format!(".{file_name}.tmp.{nanos}{attempt_suffix}"));
        match write_text_new(candidate.as_path(), content.as_bytes()) {
            Ok(()) => break candidate,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists && attempts < 32 => {
                attempts += 1;
            }
            Err(error) => return Err(error),
        }
    };
    match fs::rename(&tmp_path, &path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&tmp_path);
            Err(error)
        }
    }
}

fn canonical_write_path(path: &Path) -> io::Result<PathBuf> {
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let canonical_parent = canonical_parent_for_write(parent)?;
    let resolved = canonical_parent.join(file_name);

    match fs::symlink_metadata(&resolved) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("output path is a symlink: {}", resolved.display()),
        )),
        Ok(_) => Ok(resolved),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(resolved),
        Err(error) => Err(error),
    }
}

fn canonical_parent_for_write(parent: &Path) -> io::Result<PathBuf> {
    let mut candidate = if parent.is_absolute() {
        parent.to_path_buf()
    } else {
        std::env::current_dir()?.join(parent)
    };
    let mut missing = Vec::<OsString>::new();

    loop {
        match fs::metadata(&candidate) {
            Ok(metadata) => {
                if !metadata.is_dir() {
                    return Err(io::Error::new(
                        io::ErrorKind::NotADirectory,
                        format!("output parent is not a directory: {}", candidate.display()),
                    ));
                }
                let mut resolved = fs::canonicalize(&candidate)?;
                for component in missing.iter().rev() {
                    resolved.push(component);
                    match fs::create_dir(&resolved) {
                        Ok(()) => {}
                        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                            let metadata = fs::symlink_metadata(&resolved)?;
                            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                                return Err(io::Error::new(
                                    io::ErrorKind::InvalidInput,
                                    format!(
                                        "unsafe output parent created concurrently: {}",
                                        resolved.display()
                                    ),
                                ));
                            }
                        }
                        Err(error) => return Err(error),
                    }
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let component = candidate.file_name().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("cannot resolve output parent: {}", parent.display()),
                    )
                })?;
                missing.push(component.to_os_string());
                candidate = candidate
                    .parent()
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("cannot resolve output parent: {}", parent.display()),
                        )
                    })?
                    .to_path_buf();
            }
            Err(error) => return Err(error),
        }
    }
}

fn write_text_new(path: &Path, content: &[u8]) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path)?;
    #[cfg(test)]
    if simulate_write_failure_for_test(path) {
        let _ = fs::remove_file(path);
        return Err(io::Error::other("simulated write failure"));
    }
    if let Err(error) = file.write_all(content) {
        let _ = fs::remove_file(path);
        return Err(error);
    }
    if let Err(error) = file.flush() {
        let _ = fs::remove_file(path);
        return Err(error);
    }
    if let Err(error) = file.sync_all() {
        let _ = fs::remove_file(path);
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
fn simulate_write_failure_for_test(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("simulate-fail-"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[cfg(unix)]
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[tokio::test]
    async fn write_text_no_replace_rejects_existing_destination() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let canonical_root =
            std::fs::canonicalize(temporary.path()).expect("canonicalize temporary directory");
        let destination = canonical_root.join("graph.yml");
        write_text_no_replace(&destination, "first")
            .await
            .expect("first write should succeed");
        let error = write_text_no_replace(&destination, "second")
            .await
            .expect_err("second write must fail");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read_to_string(destination).expect("read destination"),
            "first"
        );
    }

    #[tokio::test]
    async fn write_text_atomic_writes_file_without_symlinks() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let canonical_root =
            std::fs::canonicalize(temporary.path()).expect("canonicalize temporary directory");
        let destination = canonical_root.join("graph.yml");
        write_text_atomic(&destination, "ok")
            .await
            .expect("write should succeed");
        assert_eq!(
            fs::read_to_string(destination).expect("read destination"),
            "ok"
        );
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(temporary.path().join("graph.yml"))
                .expect("destination metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_text_atomic_writes_through_canonicalized_symlink_parent() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let link = temporary.path().join("graphs");
        let target = tempfile::tempdir().expect("target directory");
        symlink(target.path(), &link).expect("symlink parent directory");
        let destination = link.join("graph.yml");
        write_text_atomic(&destination, "payload")
            .await
            .expect("canonical parent write should succeed");
        assert_eq!(
            fs::read_to_string(target.path().join("graph.yml")).expect("read canonical target"),
            "payload"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_text_atomic_rejects_symlink_destination() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let target = temporary.path().join("target.yml");
        fs::write(&target, "unchanged").expect("write target");
        let destination = temporary.path().join("graph.yml");
        symlink(&target, &destination).expect("symlink destination");

        let error = write_text_atomic(&destination, "payload")
            .await
            .expect_err("symlink destination must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(
            fs::read_to_string(target).expect("read target"),
            "unchanged"
        );
    }

    #[tokio::test]
    async fn write_text_no_replace_removes_partial_file_on_write_failure() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let canonical_root =
            std::fs::canonicalize(temporary.path()).expect("canonicalize temporary directory");
        let destination = canonical_root.join("simulate-fail-graph.yml");

        let error = write_text_no_replace(&destination, "partial write should fail")
            .await
            .expect_err("write must fail");
        assert_eq!(error.to_string(), "simulated write failure");
        assert!(
            !destination.exists(),
            "failed non-force write must not leave a partial destination file"
        );
    }

    #[tokio::test]
    async fn sync_writers_are_safe_inside_a_current_thread_runtime() {
        // Regression companion: the sync primitives must never touch the
        // Tokio runtime, so calling them from async contexts cannot panic.
        let dir = tempfile::tempdir().expect("tempdir");
        let destination = dir.path().join("sync.txt");
        write_text_no_replace_sync(&destination, "one").expect("create");
        write_text_atomic_sync(&destination, "two").expect("replace");
        assert_eq!(fs::read_to_string(&destination).expect("read"), "two");
    }
}
