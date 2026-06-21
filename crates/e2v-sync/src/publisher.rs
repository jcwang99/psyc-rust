use anyhow::Result;

use e2v_store::{BackendCapability, CasResult, EncryptedRef, LayoutRootVersion, RemoteBackend};

use crate::journal::{ObjectUploadState, OperationId, OperationJournal};
use crate::transaction::{PublishPlan, PublishSession, PublishedObject};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryAction {
    ResumePendingObjects(Vec<String>),
    NothingToDo,
}

pub trait TransactionPublisher {
    fn begin(&self, plan: PublishPlan) -> Result<PublishSession>;
    fn record_uploaded(&self, session: &PublishSession, object: PublishedObject) -> Result<()>;
    fn publish_layout_if_needed(&self, session: &PublishSession) -> Result<LayoutRootVersion>;
    fn pre_commit_verify(&self, session: &PublishSession) -> Result<()>;
    fn publish_ref(&self, session: &PublishSession, next: EncryptedRef) -> Result<CasResult>;
    fn complete(&self, session: PublishSession) -> Result<()>;
    fn recover(&self, operation_id: &OperationId) -> Result<RecoveryAction>;
}

#[derive(Clone)]
pub struct SimpleTransactionPublisher<R: RemoteBackend> {
    capability: BackendCapability,
    journal: OperationJournal,
    remote_backend: R,
}

impl<R: RemoteBackend> SimpleTransactionPublisher<R> {
    pub fn new(
        capability: BackendCapability,
        journal: OperationJournal,
        remote_backend: R,
    ) -> Self {
        Self { capability, journal, remote_backend }
    }

    pub fn remote_backend(&self) -> &R {
        &self.remote_backend
    }
}

impl<R: RemoteBackend> TransactionPublisher for SimpleTransactionPublisher<R> {
    fn begin(&self, plan: PublishPlan) -> Result<PublishSession> {
        let writer_mode = self.capability.writer_mode();
        let lease_path = if writer_mode == e2v_store::WriterMode::SingleWriter {
            let lease_path = format!("leases/{}.lock", plan.target_branch_token);
            if self.remote_backend.exists_physical(&lease_path) {
                anyhow::bail!("writer lease acquisition failed for {}", plan.target_branch_token);
            }
            self.remote_backend.put_physical(
                &lease_path,
                serde_json::to_vec(&serde_json::json!({
                    "operation_id": plan.operation_id.value,
                    "target_branch_token": plan.target_branch_token,
                }))?
                .as_slice(),
            )?;
            Some(lease_path)
        } else {
            None
        };
        let active_intent_path = format!("transactions/active/{}.intent", plan.operation_id.value);
        self.remote_backend.put_physical(
            &active_intent_path,
            serde_json::to_vec(&serde_json::json!({
                "operation_id": plan.operation_id.value,
                "target_branch_token": plan.target_branch_token,
            }))?
            .as_slice(),
        )?;
        Ok(PublishSession {
            operation_id: plan.operation_id,
            target_branch_token: plan.target_branch_token,
            writer_mode,
            active_intent_path,
            lease_path,
        })
    }

    fn record_uploaded(&self, session: &PublishSession, object: PublishedObject) -> Result<()> {
        self.journal.record_uploaded(
            &session.operation_id,
            &object.object_id,
            &object.object_type,
        )
    }

    fn publish_layout_if_needed(&self, _session: &PublishSession) -> Result<LayoutRootVersion> {
        Ok(1)
    }

    fn pre_commit_verify(&self, session: &PublishSession) -> Result<()> {
        let bytes = self.remote_backend.get_physical(&session.active_intent_path)?;
        anyhow::ensure!(
            !bytes.is_empty(),
            "pre-commit verify failed: active intent missing or empty"
        );
        if let Some(lease_path) = &session.lease_path {
            let lease = self.remote_backend.get_physical(lease_path)?;
            anyhow::ensure!(
                !lease.is_empty(),
                "pre-commit verify failed: writer lease missing or empty"
            );
        }
        Ok(())
    }

    fn publish_ref(&self, _session: &PublishSession, next: EncryptedRef) -> Result<CasResult> {
        Ok(CasResult {
            applied: true,
            current: Some(e2v_store::StoredRef {
                version: e2v_store::RefVersion { value: 1 },
                value: next,
            }),
        })
    }

    fn complete(&self, session: PublishSession) -> Result<()> {
        if let Some(lease_path) = session.lease_path {
            self.remote_backend.put_physical(&lease_path, b"released")?;
        }
        Ok(())
    }

    fn recover(&self, operation_id: &OperationId) -> Result<RecoveryAction> {
        let pending = self
            .journal
            .pending_objects(operation_id)?
            .into_iter()
            .filter(|record| record.state == ObjectUploadState::Uploaded)
            .map(|record| record.object_id)
            .collect::<Vec<_>>();
        if pending.is_empty() {
            Ok(RecoveryAction::NothingToDo)
        } else {
            Ok(RecoveryAction::ResumePendingObjects(pending))
        }
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use e2v_store::{BlobStore, ConsistencyClass, WriterMode};

    use super::*;

    #[test]
    fn begin_selects_multi_writer_mode_when_capability_supports_conditional_put() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let publisher = SimpleTransactionPublisher::new(
            BackendCapability {
                supports_conditional_put: true,
                supports_range_read: true,
                supports_atomic_rename: true,
                supports_paged_list: true,
                consistency_class: ConsistencyClass::StrongWhitelisted,
                supports_remote_lock_or_lease: true,
                supports_transaction_markers: true,
                supports_reliable_remote_time: true,
                supports_object_generation_or_etag: true,
                supports_layout_root_cas: true,
                supports_oblivious_access_schedule: false,
            },
            journal,
            e2v_store::MemoryBackend::new(),
        );

        let session = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-1".to_string()),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: None,
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap();

        assert_eq!(session.writer_mode, WriterMode::MultiWriter);
    }

    #[test]
    fn begin_writes_active_intent_marker() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let publisher = SimpleTransactionPublisher::new(
            BackendCapability {
                supports_conditional_put: true,
                supports_range_read: true,
                supports_atomic_rename: true,
                supports_paged_list: true,
                consistency_class: ConsistencyClass::StrongWhitelisted,
                supports_remote_lock_or_lease: true,
                supports_transaction_markers: true,
                supports_reliable_remote_time: true,
                supports_object_generation_or_etag: true,
                supports_layout_root_cas: true,
                supports_oblivious_access_schedule: false,
            },
            journal,
            e2v_store::MemoryBackend::new(),
        );

        let session = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-intent".to_string()),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: None,
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap();

        let active_intent = publisher
            .remote_backend()
            .get_physical("transactions/active/op-intent.intent")
            .unwrap();
        assert!(!active_intent.is_empty());
        assert_eq!(session.operation_id.value, "op-intent");
    }

    #[test]
    fn pre_commit_verify_rejects_missing_active_intent() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let publisher = SimpleTransactionPublisher::new(
            BackendCapability {
                supports_conditional_put: true,
                supports_range_read: true,
                supports_atomic_rename: true,
                supports_paged_list: true,
                consistency_class: ConsistencyClass::StrongWhitelisted,
                supports_remote_lock_or_lease: true,
                supports_transaction_markers: true,
                supports_reliable_remote_time: true,
                supports_object_generation_or_etag: true,
                supports_layout_root_cas: true,
                supports_oblivious_access_schedule: false,
            },
            journal,
            e2v_store::MemoryBackend::new(),
        );

        let session = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-precommit".to_string()),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: None,
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap();
        publisher
            .remote_backend()
            .put_physical("transactions/active/op-precommit.intent", b"")
            .unwrap();

        let error = publisher.pre_commit_verify(&session).unwrap_err();

        assert!(error.to_string().contains("intent"));
    }

    #[test]
    fn begin_uses_single_writer_mode_when_cas_is_unavailable_but_lease_exists() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let publisher = SimpleTransactionPublisher::new(
            BackendCapability {
                supports_conditional_put: false,
                supports_range_read: true,
                supports_atomic_rename: true,
                supports_paged_list: true,
                consistency_class: ConsistencyClass::UnknownOrEventual,
                supports_remote_lock_or_lease: true,
                supports_transaction_markers: true,
                supports_reliable_remote_time: true,
                supports_object_generation_or_etag: true,
                supports_layout_root_cas: true,
                supports_oblivious_access_schedule: false,
            },
            journal,
            e2v_store::MemoryBackend::new(),
        );

        let session = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-single-writer".to_string()),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: None,
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap();

        assert_eq!(session.writer_mode, WriterMode::SingleWriter);
    }

    #[test]
    fn begin_rejects_single_writer_when_lease_cannot_be_acquired() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let remote = e2v_store::MemoryBackend::new();
        remote
            .put_physical("leases/branch-token.lock", b"held-by-other-writer")
            .unwrap();
        let publisher = SimpleTransactionPublisher::new(
            BackendCapability {
                supports_conditional_put: false,
                supports_range_read: true,
                supports_atomic_rename: true,
                supports_paged_list: true,
                consistency_class: ConsistencyClass::UnknownOrEventual,
                supports_remote_lock_or_lease: true,
                supports_transaction_markers: true,
                supports_reliable_remote_time: true,
                supports_object_generation_or_etag: true,
                supports_layout_root_cas: true,
                supports_oblivious_access_schedule: false,
            },
            journal,
            remote,
        );

        let error = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-single-writer-fail".to_string()),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: None,
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap_err();

        assert!(error.to_string().contains("lease"));
    }
}
