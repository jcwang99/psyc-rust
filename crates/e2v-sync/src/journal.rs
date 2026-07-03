use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationId {
    pub value: String,
}

impl OperationId {
    pub fn new(value: String) -> Result<Self> {
        validate_sync_identifier("operation id", &value)?;
        Ok(Self { value })
    }
}

pub(crate) fn validate_sync_identifier(label: &str, value: &str) -> Result<()> {
    ensure!(!value.trim().is_empty(), "{label} must not be empty");
    ensure!(
        !value.contains('/') && !value.contains('\\'),
        "{label} must not contain path separators"
    );
    ensure!(
        value != "." && value != "..",
        "{label} must not contain path traversal"
    );
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OperationType {
    Push,
    Fetch,
    Clone,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationMetadata {
    pub operation_type: OperationType,
    pub target_branch_token: String,
    pub expected_ref_version: Option<u64>,
}

impl OperationMetadata {
    pub fn push(target_branch_token: impl Into<String>, expected_ref_version: Option<u64>) -> Self {
        Self {
            operation_type: OperationType::Push,
            target_branch_token: target_branch_token.into(),
            expected_ref_version,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObjectUploadState {
    Planned,
    Uploaded,
    Verified,
    Failed,
}

impl ObjectUploadState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Uploaded => "uploaded",
            Self::Verified => "verified",
            Self::Failed => "failed",
        }
    }

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "planned" => Ok(Self::Planned),
            "uploaded" => Ok(Self::Uploaded),
            "verified" => Ok(Self::Verified),
            "failed" => Ok(Self::Failed),
            other => anyhow::bail!("unknown object upload state {other}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectUploadRecord {
    pub object_id: String,
    pub object_type: String,
    pub state: ObjectUploadState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectStateBatch {
    pub records: Vec<ObjectUploadRecord>,
    pub next_cursor: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewriteJournalState {
    pub stage: String,
    pub target_layout_generation: u64,
    pub rewritten_object_ids: Vec<String>,
    pub retired_epoch_count: usize,
}

#[derive(Debug, Clone)]
pub struct OperationJournal {
    root: PathBuf,
}

impl OperationJournal {
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        ensure_directory_path(&root)
            .with_context(|| format!("failed to create journal dir {}", root.display()))?;
        let journal = Self { root };
        journal.ensure_schema()?;
        Ok(journal)
    }

    pub fn begin_operation(
        &self,
        operation_id: &OperationId,
        metadata: OperationMetadata,
    ) -> Result<()> {
        let bytes = serde_json::to_vec(&metadata).context("failed to encode operation metadata")?;
        let connection = self.open_connection()?;
        connection
            .execute(
                "INSERT INTO operation_metadata(operation_id, metadata_json)
                 VALUES (?1, ?2)
                 ON CONFLICT(operation_id) DO UPDATE SET metadata_json = excluded.metadata_json",
                params![operation_id.value, bytes],
            )
            .context("failed to upsert operation metadata")?;
        Ok(())
    }

    pub fn plan_object(
        &self,
        operation_id: &OperationId,
        object_id: &str,
        object_type: &str,
    ) -> Result<()> {
        self.upsert_object_state(
            operation_id,
            object_id,
            object_type,
            ObjectUploadState::Planned,
        )
    }

    pub fn record_uploaded(
        &self,
        operation_id: &OperationId,
        object_id: &str,
        object_type: &str,
    ) -> Result<()> {
        self.upsert_object_state(
            operation_id,
            object_id,
            object_type,
            ObjectUploadState::Uploaded,
        )
    }

    pub fn record_verified(
        &self,
        operation_id: &OperationId,
        object_id: &str,
        object_type: &str,
    ) -> Result<()> {
        self.upsert_object_state(
            operation_id,
            object_id,
            object_type,
            ObjectUploadState::Verified,
        )
    }

    pub fn pending_objects(&self, operation_id: &OperationId) -> Result<Vec<ObjectUploadRecord>> {
        self.latest_records(operation_id)
    }

    pub fn pending_object_batch(
        &self,
        operation_id: &OperationId,
        start: usize,
        limit: usize,
    ) -> Result<ObjectStateBatch> {
        anyhow::ensure!(limit > 0, "object state batch size must be positive");
        let connection = self.open_connection()?;
        let mut stmt = connection
            .prepare(
                "SELECT object_id, object_type, state
                 FROM object_states
                 WHERE operation_id = ?1 AND state != ?2
                 ORDER BY object_id
                 LIMIT ?3 OFFSET ?4",
            )
            .context("failed to prepare pending object batch query")?;
        let rows = stmt
            .query_map(
                params![
                    operation_id.value,
                    ObjectUploadState::Verified.as_str(),
                    limit as i64,
                    start as i64
                ],
                |row| {
                    Ok(ObjectUploadRecord {
                        object_id: row.get(0)?,
                        object_type: row.get(1)?,
                        state: ObjectUploadState::from_str(&row.get::<_, String>(2)?).map_err(
                            |error| rusqlite::Error::ToSqlConversionFailure(error.into()),
                        )?,
                    })
                },
            )
            .context("failed to query pending object batch")?;
        let records = rows
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("failed to decode pending object batch")?;
        let total = self.count_objects_in_states(
            operation_id,
            &[
                ObjectUploadState::Planned,
                ObjectUploadState::Uploaded,
                ObjectUploadState::Failed,
            ],
        )?;
        let next_cursor = if start + records.len() < total {
            Some(start + records.len())
        } else {
            None
        };
        Ok(ObjectStateBatch {
            records,
            next_cursor,
        })
    }

    pub fn object_state_batch(
        &self,
        operation_id: &OperationId,
        start: usize,
        limit: usize,
    ) -> Result<ObjectStateBatch> {
        anyhow::ensure!(limit > 0, "object state batch size must be positive");
        let connection = self.open_connection()?;
        let mut stmt = connection
            .prepare(
                "SELECT object_id, object_type, state
                 FROM object_states
                 WHERE operation_id = ?1
                 ORDER BY object_id
                 LIMIT ?2 OFFSET ?3",
            )
            .context("failed to prepare object state batch query")?;
        let rows = stmt
            .query_map(
                params![operation_id.value, limit as i64, start as i64],
                |row| {
                    Ok(ObjectUploadRecord {
                        object_id: row.get(0)?,
                        object_type: row.get(1)?,
                        state: ObjectUploadState::from_str(&row.get::<_, String>(2)?).map_err(
                            |error| rusqlite::Error::ToSqlConversionFailure(error.into()),
                        )?,
                    })
                },
            )
            .context("failed to query object state batch")?;
        let records = rows
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("failed to decode object state batch")?;
        let total = self.total_object_count(operation_id)?;
        let next_cursor = if start + records.len() < total {
            Some(start + records.len())
        } else {
            None
        };
        Ok(ObjectStateBatch {
            records,
            next_cursor,
        })
    }

    pub fn count_objects_in_states(
        &self,
        operation_id: &OperationId,
        states: &[ObjectUploadState],
    ) -> Result<usize> {
        if states.is_empty() {
            return Ok(0);
        }
        let connection = self.open_connection()?;
        let placeholders = (0..states.len())
            .map(|index| format!("?{}", index + 2))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT COUNT(*) FROM object_states WHERE operation_id = ?1 AND state IN ({placeholders})"
        );
        let mut stmt = connection
            .prepare(&sql)
            .context("failed to prepare object state count query")?;
        let mut params_vec = Vec::with_capacity(states.len() + 1);
        params_vec.push(rusqlite::types::Value::Text(operation_id.value.clone()));
        params_vec.extend(
            states
                .iter()
                .map(|state| rusqlite::types::Value::Text(state.as_str().to_string())),
        );
        let count: i64 = stmt
            .query_row(rusqlite::params_from_iter(params_vec), |row| row.get(0))
            .context("failed to count object states")?;
        Ok(count as usize)
    }

    pub fn operation_metadata(
        &self,
        operation_id: &OperationId,
    ) -> Result<Option<OperationMetadata>> {
        let connection = self.open_connection()?;
        let bytes: Option<Vec<u8>> = connection
            .query_row(
                "SELECT metadata_json FROM operation_metadata WHERE operation_id = ?1",
                params![operation_id.value],
                |row| row.get(0),
            )
            .optional()
            .context("failed to read operation metadata")?;
        bytes
            .map(|bytes| {
                serde_json::from_slice(&bytes).context("failed to decode operation metadata")
            })
            .transpose()
    }

    pub fn write_rewrite_state(
        &self,
        operation_id: &OperationId,
        state: &RewriteJournalState,
    ) -> Result<()> {
        let bytes = serde_json::to_vec(state).context("failed to encode rewrite journal state")?;
        let connection = self.open_connection()?;
        connection
            .execute(
                "INSERT INTO rewrite_state(operation_id, state_json)
                 VALUES (?1, ?2)
                 ON CONFLICT(operation_id) DO UPDATE SET state_json = excluded.state_json",
                params![operation_id.value, bytes],
            )
            .context("failed to upsert rewrite journal state")?;
        Ok(())
    }

    pub fn read_rewrite_state(
        &self,
        operation_id: &OperationId,
    ) -> Result<Option<RewriteJournalState>> {
        let connection = self.open_connection()?;
        let bytes: Option<Vec<u8>> = connection
            .query_row(
                "SELECT state_json FROM rewrite_state WHERE operation_id = ?1",
                params![operation_id.value],
                |row| row.get(0),
            )
            .optional()
            .context("failed to read rewrite journal state")?;
        bytes
            .map(|bytes| {
                serde_json::from_slice(&bytes).context("failed to decode rewrite journal state")
            })
            .transpose()
    }

    pub fn clear_operation(&self, operation_id: &OperationId) -> Result<()> {
        let connection = self.open_connection()?;
        connection
            .execute(
                "DELETE FROM rewrite_state WHERE operation_id = ?1",
                params![operation_id.value],
            )
            .context("failed to delete rewrite journal state")?;
        connection
            .execute(
                "DELETE FROM object_states WHERE operation_id = ?1",
                params![operation_id.value],
            )
            .context("failed to delete object upload states")?;
        connection
            .execute(
                "DELETE FROM operation_metadata WHERE operation_id = ?1",
                params![operation_id.value],
            )
            .context("failed to delete operation metadata")?;
        Ok(())
    }

    fn latest_records(&self, operation_id: &OperationId) -> Result<Vec<ObjectUploadRecord>> {
        let connection = self.open_connection()?;
        let mut stmt = connection
            .prepare(
                "SELECT object_id, object_type, state
                 FROM object_states
                 WHERE operation_id = ?1
                 ORDER BY object_id",
            )
            .context("failed to prepare pending objects query")?;
        let rows = stmt
            .query_map(params![operation_id.value], |row| {
                Ok(ObjectUploadRecord {
                    object_id: row.get(0)?,
                    object_type: row.get(1)?,
                    state: ObjectUploadState::from_str(&row.get::<_, String>(2)?)
                        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(error.into()))?,
                })
            })
            .context("failed to read pending objects")?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .context("failed to decode pending objects")
    }

    fn total_object_count(&self, operation_id: &OperationId) -> Result<usize> {
        let connection = self.open_connection()?;
        let count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM object_states WHERE operation_id = ?1",
                params![operation_id.value],
                |row| row.get(0),
            )
            .context("failed to count objects")?;
        Ok(count as usize)
    }

    fn upsert_object_state(
        &self,
        operation_id: &OperationId,
        object_id: &str,
        object_type: &str,
        state: ObjectUploadState,
    ) -> Result<()> {
        let connection = self.open_connection()?;
        connection
            .execute(
                "INSERT INTO object_states(operation_id, object_id, object_type, state)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(operation_id, object_id)
                 DO UPDATE SET object_type = excluded.object_type, state = excluded.state",
                params![operation_id.value, object_id, object_type, state.as_str()],
            )
            .context("failed to upsert object state")?;
        Ok(())
    }

    fn ensure_schema(&self) -> Result<()> {
        let _connection = self.open_connection()?;
        Ok(())
    }

    fn open_connection(&self) -> Result<Connection> {
        let sqlite_path = self.sqlite_path();
        ensure_directory_path(&self.root)
            .with_context(|| format!("failed to create journal dir {}", self.root.display()))?;
        let mut reset_attempted = false;
        loop {
            let result = Connection::open(&sqlite_path)
                .with_context(|| format!("failed to open journal sqlite {}", sqlite_path.display()))
                .and_then(|connection| {
                    connection
                        .execute_batch(
                            "CREATE TABLE IF NOT EXISTS operation_metadata (
                                operation_id TEXT PRIMARY KEY,
                                metadata_json BLOB NOT NULL
                             );
                             CREATE TABLE IF NOT EXISTS object_states (
                                operation_id TEXT NOT NULL,
                                object_id TEXT NOT NULL,
                                object_type TEXT NOT NULL,
                                state TEXT NOT NULL,
                                PRIMARY KEY(operation_id, object_id)
                             );
                             CREATE TABLE IF NOT EXISTS rewrite_state (
                                operation_id TEXT PRIMARY KEY,
                                state_json BLOB NOT NULL
                             );
                             CREATE INDEX IF NOT EXISTS idx_object_states_operation
                                ON object_states(operation_id, object_id);",
                        )
                        .context("failed to initialize journal schema")?;
                    Ok(connection)
                });
            match result {
                Ok(connection) => return Ok(connection),
                Err(error) if !reset_attempted && is_recoverable_journal_error(&error) => {
                    reset_attempted = true;
                    reset_sqlite_path(&sqlite_path)?;
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn sqlite_path(&self) -> PathBuf {
        self.root.join("operations.sqlite")
    }
}

fn is_recoverable_journal_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(sql_error, _))
                if matches!(
                    sql_error.code,
                    rusqlite::ffi::ErrorCode::CannotOpen
                        | rusqlite::ffi::ErrorCode::DatabaseCorrupt
                        | rusqlite::ffi::ErrorCode::NotADatabase
                )
        )
    })
}

fn sqlite_sidecar_path(db_path: &Path, suffix: &str) -> PathBuf {
    let mut os_string = db_path.as_os_str().to_os_string();
    os_string.push(suffix);
    PathBuf::from(os_string)
}

fn reset_sqlite_path(path: &Path) -> Result<()> {
    remove_path_if_exists(path)?;
    remove_path_if_exists(&sqlite_sidecar_path(path, "-wal"))?;
    remove_path_if_exists(&sqlite_sidecar_path(path, "-shm"))?;
    Ok(())
}

fn remove_path_if_exists(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path)?,
        Ok(_) => std::fs::remove_file(path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn ensure_directory_path(path: &Path) -> Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    if let Some(parent) = path.parent()
        && parent != path
    {
        ensure_directory_path(parent)?;
    }
    remove_path_if_exists(path)?;
    std::fs::create_dir_all(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn journal_replays_pending_uploaded_objects_in_order() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let operation_id = OperationId::new("op-1".to_string()).unwrap();

        journal
            .begin_operation(&operation_id, OperationMetadata::push("branch-token", None))
            .unwrap();
        journal
            .plan_object(&operation_id, "chunk-1", "chunk")
            .unwrap();
        journal
            .record_uploaded(&operation_id, "chunk-1", "chunk")
            .unwrap();

        let replay = journal.pending_objects(&operation_id).unwrap();
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].state, ObjectUploadState::Uploaded);
    }

    #[test]
    fn journal_reads_latest_object_states_in_bounded_batches() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let operation_id = OperationId::new("op-batch".to_string()).unwrap();

        journal
            .begin_operation(&operation_id, OperationMetadata::push("branch-token", None))
            .unwrap();
        journal
            .plan_object(&operation_id, "chunk-1", "chunk")
            .unwrap();
        journal
            .plan_object(&operation_id, "chunk-2", "chunk")
            .unwrap();
        journal
            .plan_object(&operation_id, "chunk-3", "chunk")
            .unwrap();
        journal
            .record_uploaded(&operation_id, "chunk-1", "chunk")
            .unwrap();
        journal
            .record_verified(&operation_id, "chunk-1", "chunk")
            .unwrap();
        journal
            .record_uploaded(&operation_id, "chunk-2", "chunk")
            .unwrap();

        let first = journal.object_state_batch(&operation_id, 0, 2).unwrap();
        assert_eq!(
            first.records,
            vec![
                ObjectUploadRecord {
                    object_id: "chunk-1".to_string(),
                    object_type: "chunk".to_string(),
                    state: ObjectUploadState::Verified,
                },
                ObjectUploadRecord {
                    object_id: "chunk-2".to_string(),
                    object_type: "chunk".to_string(),
                    state: ObjectUploadState::Uploaded,
                },
            ]
        );
        assert!(first.next_cursor.is_some());

        let second = journal
            .object_state_batch(&operation_id, first.next_cursor.unwrap(), 2)
            .unwrap();
        assert_eq!(
            second.records,
            vec![ObjectUploadRecord {
                object_id: "chunk-3".to_string(),
                object_type: "chunk".to_string(),
                state: ObjectUploadState::Planned,
            }]
        );
        assert!(second.next_cursor.is_none());
    }

    #[test]
    fn journal_pages_only_non_verified_states_for_resume() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let operation_id = OperationId::new("op-resume-batch".to_string()).unwrap();

        journal
            .begin_operation(&operation_id, OperationMetadata::push("branch-token", None))
            .unwrap();
        journal
            .plan_object(&operation_id, "chunk-1", "chunk")
            .unwrap();
        journal
            .plan_object(&operation_id, "chunk-2", "chunk")
            .unwrap();
        journal
            .plan_object(&operation_id, "chunk-3", "chunk")
            .unwrap();
        journal
            .record_verified(&operation_id, "chunk-1", "chunk")
            .unwrap();
        journal
            .record_uploaded(&operation_id, "chunk-2", "chunk")
            .unwrap();

        let batch = journal.pending_object_batch(&operation_id, 0, 8).unwrap();

        assert_eq!(
            batch.records,
            vec![
                ObjectUploadRecord {
                    object_id: "chunk-2".to_string(),
                    object_type: "chunk".to_string(),
                    state: ObjectUploadState::Uploaded,
                },
                ObjectUploadRecord {
                    object_id: "chunk-3".to_string(),
                    object_type: "chunk".to_string(),
                    state: ObjectUploadState::Planned,
                },
            ]
        );
        assert!(batch.next_cursor.is_none());
    }

    #[test]
    fn journal_counts_latest_states_by_kind() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let operation_id = OperationId::new("op-count".to_string()).unwrap();

        journal
            .begin_operation(&operation_id, OperationMetadata::push("branch-token", None))
            .unwrap();
        journal
            .plan_object(&operation_id, "chunk-1", "chunk")
            .unwrap();
        journal
            .plan_object(&operation_id, "chunk-2", "chunk")
            .unwrap();
        journal
            .record_uploaded(&operation_id, "chunk-1", "chunk")
            .unwrap();
        journal
            .record_verified(&operation_id, "chunk-1", "chunk")
            .unwrap();
        journal
            .record_uploaded(&operation_id, "chunk-2", "chunk")
            .unwrap();

        let count = journal
            .count_objects_in_states(
                &operation_id,
                &[ObjectUploadState::Uploaded, ObjectUploadState::Verified],
            )
            .unwrap();

        assert_eq!(count, 2);
    }

    #[test]
    fn journal_persists_object_states_in_sqlite_index() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let operation_id = OperationId::new("op-sqlite".to_string()).unwrap();

        journal
            .begin_operation(&operation_id, OperationMetadata::push("branch-token", None))
            .unwrap();
        journal
            .plan_object(&operation_id, "chunk-1", "chunk")
            .unwrap();

        assert!(temp.path().join("operations.sqlite").is_file());
    }

    #[test]
    fn journal_recovers_when_sqlite_path_is_a_directory() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let operation_id = OperationId::new("op-dir-conflict".to_string()).unwrap();
        journal
            .begin_operation(&operation_id, OperationMetadata::push("branch-token", None))
            .unwrap();

        let sqlite_path = temp.path().join("operations.sqlite");
        std::fs::remove_file(&sqlite_path).unwrap();
        std::fs::create_dir(&sqlite_path).unwrap();

        let reopened = OperationJournal::new(temp.path()).unwrap();
        reopened
            .begin_operation(
                &operation_id,
                OperationMetadata::push("branch-token", Some(7)),
            )
            .unwrap();

        assert!(sqlite_path.is_file());
    }

    #[test]
    fn journal_rebuilds_corrupted_sqlite_database() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let operation_id = OperationId::new("op-corrupt-sqlite".to_string()).unwrap();
        journal
            .begin_operation(&operation_id, OperationMetadata::push("branch-token", None))
            .unwrap();

        let sqlite_path = temp.path().join("operations.sqlite");
        std::fs::write(&sqlite_path, b"not-a-sqlite-database").unwrap();

        let reopened = OperationJournal::new(temp.path()).unwrap();
        reopened
            .begin_operation(
                &operation_id,
                OperationMetadata::push("branch-token", Some(9)),
            )
            .unwrap();

        assert_ne!(
            std::fs::read(&sqlite_path).unwrap(),
            b"not-a-sqlite-database"
        );
    }

    #[test]
    fn journal_stores_operation_metadata_as_compact_json() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let operation_id = OperationId::new("op-metadata-json".to_string()).unwrap();
        let metadata = OperationMetadata::push("branch-token", Some(17));

        journal
            .begin_operation(&operation_id, metadata.clone())
            .unwrap();

        let bytes: Vec<u8> = journal
            .open_connection()
            .unwrap()
            .query_row(
                "SELECT metadata_json FROM operation_metadata WHERE operation_id = ?1",
                params![operation_id.value],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(
            bytes,
            serde_json::to_vec(&metadata).unwrap(),
            "operation journal should not store pretty-printed JSON whitespace in sqlite metadata blobs"
        );
    }

    #[test]
    fn journal_persists_rewrite_state_as_compact_json() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let operation_id = OperationId::new("op-rewrite".to_string()).unwrap();
        journal
            .begin_operation(
                &operation_id,
                OperationMetadata::push("branch-token", Some(17)),
            )
            .unwrap();
        let state = RewriteJournalState {
            stage: "rewrite_objects".to_string(),
            target_layout_generation: 9,
            rewritten_object_ids: vec!["abc".to_string(), "def".to_string()],
            retired_epoch_count: 2,
        };

        journal.write_rewrite_state(&operation_id, &state).unwrap();

        let bytes: Vec<u8> = journal
            .open_connection()
            .unwrap()
            .query_row(
                "SELECT state_json FROM rewrite_state WHERE operation_id = ?1",
                params![operation_id.value],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(
            journal.read_rewrite_state(&operation_id).unwrap(),
            Some(state)
        );
        assert_eq!(
            bytes,
            serde_json::to_vec(&RewriteJournalState {
                stage: "rewrite_objects".to_string(),
                target_layout_generation: 9,
                rewritten_object_ids: vec!["abc".to_string(), "def".to_string()],
                retired_epoch_count: 2,
            })
            .unwrap(),
            "rewrite journal state should not store pretty-printed JSON whitespace"
        );
    }

    #[test]
    fn clear_operation_removes_rewrite_state_metadata_and_object_rows() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let operation_id = OperationId::new("op-rewrite".to_string()).unwrap();
        journal
            .begin_operation(
                &operation_id,
                OperationMetadata::push("branch-token", Some(17)),
            )
            .unwrap();
        journal.plan_object(&operation_id, "abc", "object").unwrap();
        journal
            .write_rewrite_state(
                &operation_id,
                &RewriteJournalState {
                    stage: "rewrite_objects".to_string(),
                    target_layout_generation: 9,
                    rewritten_object_ids: vec!["abc".to_string()],
                    retired_epoch_count: 1,
                },
            )
            .unwrap();

        journal.clear_operation(&operation_id).unwrap();

        assert!(journal.operation_metadata(&operation_id).unwrap().is_none());
        assert!(journal.read_rewrite_state(&operation_id).unwrap().is_none());
        assert!(journal.pending_objects(&operation_id).unwrap().is_empty());
    }

    #[test]
    fn sync_identifier_rejects_path_separators_and_traversal_segments() {
        let separator_error = validate_sync_identifier("operation id", "../evil").unwrap_err();
        let traversal_error = validate_sync_identifier("operation id", "..").unwrap_err();

        assert!(separator_error.to_string().contains("path separators"));
        assert!(traversal_error.to_string().contains("path traversal"));
    }

    #[test]
    fn sync_identifier_allows_non_path_traversal_dots_inside_names() {
        validate_sync_identifier("operation id", "resume..batch").unwrap();
    }
}
