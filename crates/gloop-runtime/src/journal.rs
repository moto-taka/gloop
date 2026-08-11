use std::path::{Path, PathBuf};

use chrono::Utc;
use gloop_core::{RunEvent, RunEventKind};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{
    fs::{self, File, OpenOptions},
    io::{AsyncWriteExt, BufWriter},
    sync::Mutex,
};

pub const EVENT_SCHEMA_VERSION: &str = "gloop.run-event/v1alpha1";
const MAX_JOURNAL_BYTES: u64 = 128 * 1024 * 1024;
const MAX_JOURNAL_EVENTS: usize = 1_000_000;

#[derive(Debug)]
pub struct Journal {
    path: PathBuf,
    run_id: String,
    state: Mutex<JournalState>,
}

#[derive(Debug)]
struct JournalState {
    writer: BufWriter<File>,
    next_sequence: u64,
    previous_hash: Option<String>,
    bytes_written: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct JournalRow {
    pub event: RunEvent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev_hash: Option<String>,
    pub event_hash: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct JournalRead {
    pub events: Vec<RunEvent>,
    pub truncated_tail: bool,
}

impl Journal {
    pub async fn create(
        path: impl Into<PathBuf>,
        run_id: impl Into<String>,
    ) -> Result<Self, JournalError> {
        let path = path.into();
        let mut options = OpenOptions::new();
        options.create_new(true);
        options.write(true);
        options.append(true);
        #[cfg(unix)]
        options.mode(0o600);
        let file = options.open(&path).await?;
        Ok(Self {
            path,
            run_id: run_id.into(),
            state: Mutex::new(JournalState {
                writer: BufWriter::new(file),
                next_sequence: 1,
                previous_hash: None,
                bytes_written: 0,
            }),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn append(
        &self,
        kind: RunEventKind,
        node_id: Option<&str>,
        attempt: Option<u32>,
        message: Option<String>,
        data: Value,
    ) -> Result<RunEvent, JournalError> {
        let mut state = self.state.lock().await;
        let attempted_count = usize::try_from(state.next_sequence).unwrap_or(usize::MAX);
        if attempted_count > MAX_JOURNAL_EVENTS {
            return Err(JournalError::TooManyEvents {
                count: attempted_count,
                limit: MAX_JOURNAL_EVENTS,
            });
        }
        let event = RunEvent {
            schema_version: EVENT_SCHEMA_VERSION.to_owned(),
            sequence: state.next_sequence,
            timestamp: Utc::now(),
            run_id: self.run_id.clone(),
            node_id: node_id.map(ToOwned::to_owned),
            attempt,
            kind,
            message,
            data,
        };
        let event_hash = hash_event(&event, state.previous_hash.as_deref())?;
        let row = JournalRow {
            event: event.clone(),
            prev_hash: state.previous_hash.clone(),
            event_hash: event_hash.clone(),
        };
        let mut encoded = serde_json::to_vec(&row)?;
        encoded.push(b'\n');
        let next_bytes = state
            .bytes_written
            .checked_add(u64::try_from(encoded.len()).map_err(|_| {
                JournalError::JournalTooLarge {
                    size: state.bytes_written,
                    limit: MAX_JOURNAL_BYTES,
                }
            })?)
            .ok_or(JournalError::JournalTooLarge {
                size: state.bytes_written,
                limit: MAX_JOURNAL_BYTES,
            })?;
        if next_bytes > MAX_JOURNAL_BYTES {
            return Err(JournalError::JournalTooLarge {
                size: next_bytes,
                limit: MAX_JOURNAL_BYTES,
            });
        }
        state.writer.write_all(&encoded).await?;
        state.writer.flush().await?;
        state.writer.get_ref().sync_data().await?;
        state.bytes_written = next_bytes;
        state.previous_hash = Some(event_hash);
        state.next_sequence = state
            .next_sequence
            .checked_add(1)
            .ok_or(JournalError::SequenceOverflow)?;
        Ok(event)
    }
}

pub async fn read_events(path: impl AsRef<Path>) -> Result<Vec<RunEvent>, JournalError> {
    let journal = read_journal(path).await?;
    if journal.truncated_tail {
        return Err(JournalError::IncompleteTail);
    }
    Ok(journal.events)
}

pub async fn read_journal(path: impl AsRef<Path>) -> Result<JournalRead, JournalError> {
    let metadata = fs::metadata(path.as_ref()).await?;
    enforce_journal_size(metadata.len())?;
    let bytes = fs::read(path.as_ref()).await?;
    let mut events = Vec::new();
    let mut previous_hash: Option<String> = None;
    let mut start = 0_usize;
    let mut line_number = 0_usize;
    for (newline, _) in bytes.iter().enumerate().filter(|(_, byte)| **byte == b'\n') {
        line_number += 1;
        let line = &bytes[start..newline];
        start = newline + 1;
        if line.iter().all(u8::is_ascii_whitespace) {
            return Err(JournalError::EmptyLine { line: line_number });
        }
        let row: JournalRow =
            serde_json::from_slice(line).map_err(|source| JournalError::InvalidLine {
                line: line_number,
                source,
            })?;
        verify_row(&row, previous_hash.as_deref(), line_number)?;
        previous_hash = Some(row.event_hash);
        events.push(row.event);
        if events.len() > MAX_JOURNAL_EVENTS {
            return Err(JournalError::TooManyEvents {
                count: events.len(),
                limit: MAX_JOURNAL_EVENTS,
            });
        }
    }
    let truncated_tail = start < bytes.len();
    Ok(JournalRead {
        events,
        truncated_tail,
    })
}

fn enforce_journal_size(size: u64) -> Result<(), JournalError> {
    if size > MAX_JOURNAL_BYTES {
        return Err(JournalError::JournalTooLarge {
            size,
            limit: MAX_JOURNAL_BYTES,
        });
    }
    Ok(())
}

fn hash_event(event: &RunEvent, previous_hash: Option<&str>) -> Result<String, serde_json::Error> {
    #[derive(Serialize)]
    struct HashPayload<'a> {
        event: &'a RunEvent,
        prev_hash: Option<&'a str>,
    }

    let payload = serde_json::to_vec(&HashPayload {
        event,
        prev_hash: previous_hash,
    })?;
    Ok(hex::encode(Sha256::digest(payload)))
}

fn verify_row(
    row: &JournalRow,
    expected_previous_hash: Option<&str>,
    line: usize,
) -> Result<(), JournalError> {
    if row.prev_hash.as_deref() != expected_previous_hash {
        return Err(JournalError::BrokenChain {
            line,
            expected: expected_previous_hash.map(ToOwned::to_owned),
            actual: row.prev_hash.clone(),
        });
    }
    let expected = hash_event(&row.event, expected_previous_hash)?;
    if row.event_hash != expected {
        return Err(JournalError::HashMismatch {
            line,
            expected,
            actual: row.event_hash.clone(),
        });
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum JournalError {
    #[error("journal I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("journal file too large: {size} bytes > {limit}")]
    JournalTooLarge { size: u64, limit: u64 },
    #[error("journal serialization failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("journal line {line} is invalid: {source}")]
    InvalidLine {
        line: usize,
        source: serde_json::Error,
    },
    #[error("journal contains an empty line at {line}")]
    EmptyLine { line: usize },
    #[error("journal contains too many events: {count} > {limit}")]
    TooManyEvents { count: usize, limit: usize },
    #[error("journal ends with an incomplete row")]
    IncompleteTail,
    #[error("journal hash chain is broken at line {line}: expected {expected:?}, found {actual:?}")]
    BrokenChain {
        line: usize,
        expected: Option<String>,
        actual: Option<String>,
    },
    #[error("journal hash mismatch at line {line}: expected {expected}, found {actual}")]
    HashMismatch {
        line: usize,
        expected: String,
        actual: String,
    },
    #[error("journal sequence overflow")]
    SequenceOverflow,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[tokio::test]
    async fn appends_strictly_sequenced_json_lines() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let path = temporary.path().join("events.jsonl");
        let journal = Journal::create(&path, "run").await.expect("create journal");
        journal
            .append(RunEventKind::RunStarted, None, None, None, json!({}))
            .await
            .expect("append first");
        journal
            .append(RunEventKind::RunFinished, None, None, None, json!({}))
            .await
            .expect("append second");
        let events = read_events(path).await.expect("read journal");
        assert_eq!(
            events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            [1, 2]
        );
    }

    #[tokio::test]
    async fn rejects_tampered_interior_rows() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let path = temporary.path().join("events.jsonl");
        let journal = Journal::create(&path, "run").await.expect("create journal");
        journal
            .append(RunEventKind::RunStarted, None, None, None, json!({}))
            .await
            .expect("append first");
        journal
            .append(RunEventKind::RunFinished, None, None, None, json!({}))
            .await
            .expect("append second");
        let bytes = fs::read(&path).await.expect("read journal");
        let altered =
            String::from_utf8(bytes)
                .expect("utf8")
                .replacen("run_started", "run_finished", 1);
        fs::write(&path, altered).await.expect("write tamper");
        assert!(matches!(
            read_events(path).await.expect_err("tamper rejected"),
            JournalError::HashMismatch { line: 1, .. }
        ));
    }

    #[tokio::test]
    async fn reports_an_incomplete_final_row() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let path = temporary.path().join("events.jsonl");
        let journal = Journal::create(&path, "run").await.expect("create journal");
        journal
            .append(RunEventKind::RunStarted, None, None, None, json!({}))
            .await
            .expect("append first");
        let mut bytes = fs::read(&path).await.expect("read journal");
        bytes.extend_from_slice(b"{\"event\":");
        fs::write(&path, bytes).await.expect("write partial row");
        let read = read_journal(path).await.expect("valid prefix is readable");
        assert!(read.truncated_tail);
        assert_eq!(read.events.len(), 1);
    }

    #[tokio::test]
    async fn rejects_oversized_journal_file() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let path = temporary.path().join("events.jsonl");
        let file = File::create(&path).await.expect("create oversized file");
        file.set_len(MAX_JOURNAL_BYTES + 1)
            .await
            .expect("set oversized length");
        assert!(matches!(
            read_journal(path)
                .await
                .expect_err("oversized journal should fail"),
            JournalError::JournalTooLarge { .. }
        ));
    }

    #[test]
    fn accepts_journal_at_maximum_size() {
        enforce_journal_size(MAX_JOURNAL_BYTES).expect("boundary size is accepted");
    }

    #[tokio::test]
    async fn rejects_too_many_journal_events() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let path = temporary.path().join("events.jsonl");
        let journal = Journal::create(&path, "run").await.expect("create journal");
        {
            let mut state = journal.state.lock().await;
            state.next_sequence = u64::try_from(MAX_JOURNAL_EVENTS).expect("limit fits") + 1;
        }
        assert!(matches!(
            journal
                .append(
                    RunEventKind::RunStarted,
                    None,
                    None,
                    None,
                    serde_json::json!({}),
                )
                .await
                .expect_err("event limit should fail"),
            JournalError::TooManyEvents { .. }
        ));
    }

    #[tokio::test]
    async fn rejects_append_over_size_limit() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let path = temporary.path().join("events.jsonl");
        let journal = Journal::create(&path, "run").await.expect("create journal");
        {
            let mut state = journal.state.lock().await;
            state.bytes_written = MAX_JOURNAL_BYTES;
        }
        let oversized = journal
            .append(
                RunEventKind::RunStarted,
                None,
                None,
                None,
                serde_json::json!({"payload": "x"}),
            )
            .await;
        assert!(matches!(
            oversized.expect_err("size limit should fail"),
            JournalError::JournalTooLarge { .. }
        ));

        {
            let mut state = journal.state.lock().await;
            assert_eq!(state.next_sequence, 1);
            assert!(state.previous_hash.is_none());
            state.bytes_written = 0;
        }
        let after_fail = journal
            .append(
                RunEventKind::RunStarted,
                None,
                None,
                None,
                serde_json::json!({ "payload": "x".repeat(16) }),
            )
            .await
            .expect("recovery append");
        assert_eq!(after_fail.sequence, 1);
    }

    #[tokio::test]
    async fn creates_journal_with_restricted_permissions() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let path = temporary.path().join("events.jsonl");
        let _ = Journal::create(&path, "run").await.expect("create journal");
        let metadata = fs::metadata(&path).await.expect("read journal metadata");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        }
    }
}
