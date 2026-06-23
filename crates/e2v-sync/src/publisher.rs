use anyhow::{Context, Result};

use e2v_store::{BackendCapability, CasResult, EncryptedRef, LayoutRootVersion, RemoteBackend};

use crate::journal::{ObjectUploadState, OperationId, OperationJournal, validate_sync_identifier};
use crate::transaction::{PublishPlan, PublishSession, PublishedObject};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryAction {
    ResumePendingObjects(Vec<String>),
    NothingToDo,
}

#[derive(serde::Deserialize)]
struct RemoteOwnershipMarker {
    operation_id: String,
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
        Self {
            capability,
            journal,
            remote_backend,
        }
    }

    pub fn remote_backend(&self) -> &R {
        &self.remote_backend
    }
}

impl<R: RemoteBackend> TransactionPublisher for SimpleTransactionPublisher<R> {
    fn begin(&self, plan: PublishPlan) -> Result<PublishSession> {
        validate_sync_identifier("branch token", &plan.target_branch_token)?;
        let writer_mode = self.capability.writer_mode();
        anyhow::ensure!(
            writer_mode != e2v_store::WriterMode::ReadOnly,
            "read-only backend capabilities cannot publish"
        );
        let lease_path = if writer_mode == e2v_store::WriterMode::SingleWriter {
            let lease_path = format!("leases/{}.lock", plan.target_branch_token);
            if self.remote_backend.exists_physical(&lease_path) {
                anyhow::bail!(
                    "writer lease acquisition failed for {}",
                    plan.target_branch_token
                );
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
            expected_ref_version: plan.expected_ref_version,
            writer_mode,
            next_layout_root: None,
            next_layout_root_bytes: None,
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

    fn publish_layout_if_needed(&self, session: &PublishSession) -> Result<LayoutRootVersion> {
        if let Some(next_layout_root) = &session.next_layout_root {
            let current_layout_root = self.remote_backend.read_layout_root()?;
            let result = self.remote_backend.compare_and_swap_layout_root(
                current_layout_root.generation,
                next_layout_root.clone(),
            )?;
            anyhow::ensure!(
                result.applied,
                "layout root publish failed: generation changed before publish"
            );
            if let Some(bytes) = &session.next_layout_root_bytes {
                self.remote_backend
                    .put_physical("layout_root.json", bytes)?;
            }
            Ok(next_layout_root.generation)
        } else {
            Ok(self.remote_backend.read_layout_root()?.generation)
        }
    }

    fn pre_commit_verify(&self, session: &PublishSession) -> Result<()> {
        let bytes = self
            .remote_backend
            .get_physical(&session.active_intent_path)?;
        anyhow::ensure!(
            !bytes.is_empty(),
            "pre-commit verify failed: active intent missing or empty"
        );
        let intent: RemoteOwnershipMarker = serde_json::from_slice(&bytes)
            .context("pre-commit verify failed: invalid active intent marker")?;
        anyhow::ensure!(
            intent.operation_id == session.operation_id.value,
            "pre-commit verify failed: active intent belongs to another operation"
        );
        if let Some(expected_ref_version) = session.expected_ref_version {
            validate_sync_identifier("branch token", &session.target_branch_token)?;
            let current = self.remote_backend.read_ref(&e2v_store::RefToken::new(
                session.target_branch_token.clone(),
            ))?;
            let current_version = current
                .as_ref()
                .map(|stored_ref| stored_ref.version.value)
                .context("pre-commit verify failed: remote ref missing")?;
            anyhow::ensure!(
                current_version == expected_ref_version,
                "pre-commit verify failed: remote ref version changed"
            );
        }
        if let Some(lease_path) = &session.lease_path {
            let lease = self.remote_backend.get_physical(lease_path)?;
            anyhow::ensure!(
                !lease.is_empty(),
                "pre-commit verify failed: writer lease missing or empty"
            );
            let lease_marker: RemoteOwnershipMarker = serde_json::from_slice(&lease)
                .context("pre-commit verify failed: invalid writer lease marker")?;
            anyhow::ensure!(
                lease_marker.operation_id == session.operation_id.value,
                "pre-commit verify failed: writer lease belongs to another operation"
            );
        }
        Ok(())
    }

    fn publish_ref(&self, session: &PublishSession, next: EncryptedRef) -> Result<CasResult> {
        validate_sync_identifier("branch token", &session.target_branch_token)?;
        self.remote_backend.compare_and_swap_ref(
            &e2v_store::RefToken::new(session.target_branch_token.clone()),
            session
                .expected_ref_version
                .map(|value| e2v_store::RefVersion { value }),
            next,
        )
    }

    fn complete(&self, session: PublishSession) -> Result<()> {
        self.remote_backend
            .delete_physical(&session.active_intent_path)?;
        if let Some(lease_path) = session.lease_path {
            self.remote_backend.delete_physical(&lease_path)?;
        }
        Ok(())
    }

    fn recover(&self, operation_id: &OperationId) -> Result<RecoveryAction> {
        let mut pending = Vec::new();
        let mut cursor = Some(0usize);
        while let Some(start) = cursor {
            let batch = self
                .journal
                .pending_object_batch(operation_id, start, 128)?;
            for record in batch.records {
                if record.state == ObjectUploadState::Uploaded {
                    pending.push(record.object_id);
                }
            }
            cursor = batch.next_cursor;
        }
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

    use e2v_store::{BlobStore, ConsistencyClass, RefStore, WriterMode};

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
                operation_id: OperationId::new("op-1".to_string()).unwrap(),
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
                operation_id: OperationId::new("op-intent".to_string()).unwrap(),
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
                operation_id: OperationId::new("op-precommit".to_string()).unwrap(),
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
    fn pre_commit_verify_rejects_foreign_active_intent_marker() {
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
                operation_id: OperationId::new("op-intent-owner".to_string()).unwrap(),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: None,
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap();
        publisher
            .remote_backend()
            .put_physical(
                "transactions/active/op-intent-owner.intent",
                serde_json::to_vec(&serde_json::json!({
                    "operation_id": "other-operation",
                    "target_branch_token": "branch-token",
                }))
                .unwrap()
                .as_slice(),
            )
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
                operation_id: OperationId::new("op-single-writer".to_string()).unwrap(),
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
                operation_id: OperationId::new("op-single-writer-fail".to_string()).unwrap(),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: None,
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap_err();

        assert!(error.to_string().contains("lease"));
    }

    #[test]
    fn begin_rejects_read_only_backend_capabilities() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let publisher = SimpleTransactionPublisher::new(
            BackendCapability {
                supports_conditional_put: false,
                supports_range_read: true,
                supports_atomic_rename: false,
                supports_paged_list: true,
                consistency_class: ConsistencyClass::UnknownOrEventual,
                supports_remote_lock_or_lease: false,
                supports_transaction_markers: false,
                supports_reliable_remote_time: false,
                supports_object_generation_or_etag: false,
                supports_layout_root_cas: false,
                supports_oblivious_access_schedule: false,
            },
            journal,
            e2v_store::MemoryBackend::new(),
        );

        let error = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-read-only".to_string()).unwrap(),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: None,
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap_err();

        assert!(error.to_string().contains("read-only"));
    }

    #[test]
    fn pre_commit_verify_rejects_foreign_writer_lease_holder() {
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
                operation_id: OperationId::new("op-lease-owner".to_string()).unwrap(),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: None,
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap();
        publisher
            .remote_backend()
            .put_physical(
                "leases/branch-token.lock",
                serde_json::to_vec(&serde_json::json!({
                    "operation_id": "other-operation",
                    "target_branch_token": "branch-token",
                }))
                .unwrap()
                .as_slice(),
            )
            .unwrap();

        let error = publisher.pre_commit_verify(&session).unwrap_err();

        assert!(error.to_string().contains("lease"));
    }

    #[test]
    fn pre_commit_verify_rejects_stale_remote_ref_version() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let remote = e2v_store::MemoryBackend::new();
        let token = e2v_store::RefToken::new("branch-token".to_string());
        remote
            .compare_and_swap_ref(&token, None, e2v_store::EncryptedRef::new(vec![1, 2, 3]))
            .unwrap();
        let publisher =
            SimpleTransactionPublisher::new(remote.capability().clone(), journal, remote.clone());
        let session = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-stale-precommit".to_string()).unwrap(),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: Some(1),
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap();
        remote
            .compare_and_swap_ref(
                &token,
                Some(e2v_store::RefVersion { value: 1 }),
                e2v_store::EncryptedRef::new(vec![4, 5, 6]),
            )
            .unwrap();

        let error = publisher.pre_commit_verify(&session).unwrap_err();

        assert!(error.to_string().contains("ref"));
    }

    #[test]
    fn publish_ref_respects_expected_remote_ref_version() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let remote = e2v_store::MemoryBackend::new();
        let token = e2v_store::RefToken::new("branch-token".to_string());
        remote
            .compare_and_swap_ref(&token, None, e2v_store::EncryptedRef::new(vec![1, 2, 3]))
            .unwrap();
        let publisher =
            SimpleTransactionPublisher::new(remote.capability().clone(), journal, remote.clone());
        let session = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-publish-ref".to_string()).unwrap(),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: Some(1),
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap();

        let applied = publisher
            .publish_ref(&session, e2v_store::EncryptedRef::new(vec![4, 5, 6]))
            .unwrap();
        assert!(applied.applied);
        assert_eq!(applied.current.unwrap().version.value, 2);

        let stale = publisher
            .publish_ref(&session, e2v_store::EncryptedRef::new(vec![7, 8, 9]))
            .unwrap();
        assert!(!stale.applied);
        assert_eq!(stale.current.unwrap().version.value, 2);
    }
    #[test]
    fn complete_clears_active_intent_marker() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let remote = e2v_store::MemoryBackend::new();
        let publisher =
            SimpleTransactionPublisher::new(remote.capability().clone(), journal, remote.clone());
        let session = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-complete-intent".to_string()).unwrap(),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: None,
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap();

        publisher.complete(session).unwrap();

        assert!(!remote.exists_physical("transactions/active/op-complete-intent.intent"));
    }

    #[test]
    fn complete_releases_single_writer_lease_for_next_publish() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let remote = e2v_store::MemoryBackend::new();
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
            remote.clone(),
        );
        let first = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-complete-lease-1".to_string()).unwrap(),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: None,
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap();

        publisher.complete(first).unwrap();

        let second = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-complete-lease-2".to_string()).unwrap(),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: None,
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap();
        let lease_bytes = remote.get_physical("leases/branch-token.lock").unwrap();
        let lease_marker: serde_json::Value = serde_json::from_slice(&lease_bytes).unwrap();

        assert_eq!(second.writer_mode, WriterMode::SingleWriter);
        assert_eq!(lease_marker["operation_id"], "op-complete-lease-2");
    }

    #[test]
    fn begin_rejects_branch_token_with_path_traversal_before_writing_markers() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let remote = e2v_store::MemoryBackend::new();
        let publisher =
            SimpleTransactionPublisher::new(remote.capability().clone(), journal, remote.clone());

        let error = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-invalid-branch".to_string()).unwrap(),
                target_branch_token: "../evil".to_string(),
                expected_ref_version: None,
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap_err();

        assert!(error.to_string().contains("branch"));
        assert!(!remote.exists_physical("transactions/active/op-invalid-branch.intent"));
        assert!(!remote.exists_physical("leases/../evil.lock"));
    }

    #[test]
    fn publish_ref_rejects_branch_token_with_path_traversal_in_session() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let remote = e2v_store::MemoryBackend::new();
        let publisher =
            SimpleTransactionPublisher::new(remote.capability().clone(), journal, remote);
        let session = PublishSession {
            operation_id: OperationId::new("op-invalid-session".to_string()).unwrap(),
            target_branch_token: "../evil".to_string(),
            expected_ref_version: None,
            writer_mode: WriterMode::ReadOnly,
            next_layout_root: None,
            next_layout_root_bytes: None,
            active_intent_path: "transactions/active/op-invalid-session.intent".to_string(),
            lease_path: None,
        };

        let error = publisher
            .publish_ref(&session, EncryptedRef::new(vec![1, 2, 3]))
            .unwrap_err();

        assert!(error.to_string().contains("branch"));
    }
}
