use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationId {
    pub value: String,
}

impl OperationId {
    pub fn new(value: String) -> Self {
        Self { value }
    }
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
}

impl OperationMetadata {
    pub fn push(target_branch_token: impl Into<String>) -> Self {
        Self {
            operation_type: OperationType::Push,
            target_branch_token: target_branch_token.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObjectUploadState {
    Planned,
    Uploaded,
    Verified,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectUploadRecord {
    pub object_id: String,
    pub object_type: String,
    pub state: ObjectUploadState,
}

#[derive(Debug, Clone)]
pub struct OperationJournal {
    root: PathBuf,
}

impl OperationJournal {
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root)
            .with_context(|| format!("failed to create journal dir {}", root.display()))?;
        Ok(Self { root })
    }

    pub fn begin_operation(
        &self,
        operation_id: &OperationId,
        metadata: OperationMetadata,
    ) -> Result<()> {
        let path = self.metadata_path(operation_id);
        let bytes = serde_json::to_vec_pretty(&metadata).context("failed to encode operation metadata")?;
        fs::write(&path, bytes)
            .with_context(|| format!("failed to write operation metadata {}", path.display()))?;
        Ok(())
    }

    pub fn plan_object(
        &self,
        operation_id: &OperationId,
        object_id: &str,
        object_type: &str,
    ) -> Result<()> {
        self.append_record(
            operation_id,
            ObjectUploadRecord {
                object_id: object_id.to_string(),
                object_type: object_type.to_string(),
                state: ObjectUploadState::Planned,
            },
        )
    }

    pub fn record_uploaded(
        &self,
        operation_id: &OperationId,
        object_id: &str,
        object_type: &str,
    ) -> Result<()> {
        self.append_record(
            operation_id,
            ObjectUploadRecord {
                object_id: object_id.to_string(),
                object_type: object_type.to_string(),
                state: ObjectUploadState::Uploaded,
            },
        )
    }

    pub fn record_verified(
        &self,
        operation_id: &OperationId,
        object_id: &str,
        object_type: &str,
    ) -> Result<()> {
        self.append_record(
            operation_id,
            ObjectUploadRecord {
                object_id: object_id.to_string(),
                object_type: object_type.to_string(),
                state: ObjectUploadState::Verified,
            },
        )
    }

    pub fn pending_objects(&self, operation_id: &OperationId) -> Result<Vec<ObjectUploadRecord>> {
        let path = self.wal_path(operation_id);
        if !path.exists() {
            return Ok(Vec::new());
        }

        let contents = fs::read_to_string(&path)
            .with_context(|| format!("failed to read journal wal {}", path.display()))?;
        let mut latest = std::collections::BTreeMap::new();
        for line in contents.lines().filter(|line| !line.trim().is_empty()) {
            let record: ObjectUploadRecord =
                serde_json::from_str(line).context("failed to decode journal wal record")?;
            latest.insert(record.object_id.clone(), record);
        }
        Ok(latest.into_values().collect())
    }

    fn append_record(&self, operation_id: &OperationId, record: ObjectUploadRecord) -> Result<()> {
        let path = self.wal_path(operation_id);
        let mut existing = if path.exists() {
            fs::read_to_string(&path)
                .with_context(|| format!("failed to read journal wal {}", path.display()))?
        } else {
            String::new()
        };
        existing.push_str(&serde_json::to_string(&record).context("failed to encode wal record")?);
        existing.push('\n');
        fs::write(&path, existing)
            .with_context(|| format!("failed to write journal wal {}", path.display()))?;
        Ok(())
    }

    fn metadata_path(&self, operation_id: &OperationId) -> PathBuf {
        self.root.join(format!("{}.meta.json", operation_id.value))
    }

    fn wal_path(&self, operation_id: &OperationId) -> PathBuf {
        self.root.join(format!("{}.wal", operation_id.value))
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn journal_replays_pending_uploaded_objects_in_order() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let operation_id = OperationId::new("op-1".to_string());

        journal
            .begin_operation(&operation_id, OperationMetadata::push("branch-token"))
            .unwrap();
        journal.plan_object(&operation_id, "chunk-1", "chunk").unwrap();
        journal.record_uploaded(&operation_id, "chunk-1", "chunk").unwrap();

        let replay = journal.pending_objects(&operation_id).unwrap();
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].state, ObjectUploadState::Uploaded);
    }
}
