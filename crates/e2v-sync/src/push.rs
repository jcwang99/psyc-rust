use std::path::PathBuf;

use anyhow::Result;

use e2v_core::{sync_support, ManifestStore, ManifestStoreApi, RepositoryFacade};
use e2v_store::{
    EncryptedRef, LayoutRoot, RefToken, RemoteBackend,
};

use crate::journal::{OperationId, OperationJournal, OperationMetadata};
use crate::publisher::{SimpleTransactionPublisher, TransactionPublisher};
use crate::transaction::{PublishPlan, PublishedObject};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushOptions {
    pub repo_root: PathBuf,
    pub branch_token: String,
    pub operation_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushResult {
    pub published_snapshot_id: String,
    pub uploaded_objects: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeOptions {
    pub repo_root: PathBuf,
    pub branch_token: String,
    pub operation_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeResult {
    pub published_snapshot_id: String,
    pub skipped_uploaded_objects: usize,
}

pub fn push_head<R: RemoteBackend + Clone>(
    facade: &RepositoryFacade,
    remote: &R,
    options: PushOptions,
) -> Result<PushResult> {
    let (_state, snapshot) = sync_support::export_head_snapshot(facade, &options.repo_root)?;
    let expected_ref_version = match remote.read_ref(&RefToken::new(options.branch_token.clone()))? {
        Some(stored_ref) => {
            let remote_head_snapshot_id =
                sync_support::decode_ref_head_snapshot_id(&options.repo_root, &stored_ref.value.bytes)?;
            let can_fast_forward = match remote_head_snapshot_id.as_deref() {
                Some(remote_head) => snapshot.ancestor_snapshot_ids.iter().any(|ancestor| ancestor == remote_head),
                None => snapshot.parent_snapshot_id.is_none(),
            };
            if !can_fast_forward {
                anyhow::bail!("push requires needs-rebase recovery");
            }
            Some(stored_ref.version.value)
        }
        None => None,
    };
    for ancestor_snapshot_id in &snapshot.ancestor_snapshot_ids {
        let ancestor_object_path = format!("objects/{ancestor_snapshot_id}.json");
        if !remote.exists_physical(&ancestor_object_path) {
            anyhow::bail!(
                "push rejected: ancestor closure incomplete, missing remote snapshot {ancestor_snapshot_id}"
            );
        }
    }
    let journal = OperationJournal::new(options.repo_root.join(".e2v").join("journal").join("sync"))?;
    let operation_id = OperationId::new(options.operation_id);
    journal.begin_operation(
        &operation_id,
        OperationMetadata::push(options.branch_token.clone(), expected_ref_version),
    )?;
    let publisher = SimpleTransactionPublisher::new(
        remote.capability().clone(),
        journal.clone(),
        remote.clone(),
    );
    let layout_root_bytes = sync_support::read_layout_root_bytes(&options.repo_root)?;
    let layout_root: LayoutRoot = serde_json::from_slice(&layout_root_bytes)?;
    let session = publisher.begin(PublishPlan {
        operation_id: operation_id.clone(),
        target_branch_token: options.branch_token.clone(),
        expected_ref_version,
        writer_mode: remote.capability().writer_mode(),
    })?;
    let session = crate::transaction::PublishSession {
        next_layout_root: Some(layout_root.clone()),
        next_layout_root_bytes: Some(layout_root_bytes.clone()),
        ..session
    };

    let manifest_store = ManifestStore::new(&options.repo_root);
    let reachable_object_ids = manifest_store.collect_reachable_object_ids(&snapshot.snapshot_id)?;
    for object_id in &reachable_object_ids {
        journal.plan_object(&operation_id, object_id, "object")?;
    }
    for object_id in &reachable_object_ids {
        let object_name = format!("{object_id}.json");
        let relative_path = format!("objects/{object_name}");
        let bytes = std::fs::read(
            options
                .repo_root
                .join(".e2v")
                .join("objects")
                .join(&object_name),
        )?;
        remote.put_physical(&relative_path, &bytes)?;
        publisher.record_uploaded(
            &session,
            PublishedObject {
                object_id: object_id.clone(),
                object_type: "object".to_string(),
            },
        )?;
        journal.record_verified(
            &operation_id,
            object_id,
            "object",
        )?;
    }

    let config_bytes = sync_support::read_config_bytes(&options.repo_root)?;
    remote.put_physical("control/config.json", &config_bytes)?;
    for keyring_file in sync_support::list_keyring_files(&options.repo_root)? {
        let file_name = keyring_file
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow::anyhow!("invalid keyring path {}", keyring_file.display()))?;
        let bytes = std::fs::read(&keyring_file)?;
        remote.put_physical(&format!("control/keyring/{file_name}"), &bytes)?;
    }
    publisher.publish_layout_if_needed(&session)?;
    publisher.pre_commit_verify(&session)?;

    let default_ref_bytes = sync_support::read_default_ref_bytes(&options.repo_root)?;
    remote.put_physical("control/refs/default.json", &default_ref_bytes)?;
    let publish_result = publisher.publish_ref(&session, EncryptedRef::new(default_ref_bytes))?;
    if !publish_result.applied {
        anyhow::bail!("push requires needs-rebase recovery");
    }
    publisher.complete(session)?;

    Ok(PushResult {
        published_snapshot_id: snapshot.snapshot_id,
        uploaded_objects: reachable_object_ids.len(),
    })
}

pub fn resume_push<R: RemoteBackend + Clone>(
    facade: &RepositoryFacade,
    remote: &R,
    options: ResumeOptions,
) -> Result<ResumeResult> {
    let (_state, snapshot) = sync_support::export_head_snapshot(facade, &options.repo_root)?;
    let manifest_store = ManifestStore::new(&options.repo_root);
    let reachable_object_ids = manifest_store.collect_reachable_object_ids(&snapshot.snapshot_id)?;
    let journal = OperationJournal::new(options.repo_root.join(".e2v").join("journal").join("sync"))?;
    let operation_id = OperationId::new(options.operation_id);
    let branch_token = options.branch_token;
    let pending = journal.pending_objects(&operation_id)?;
    let skipped_uploaded_objects = pending
        .iter()
        .filter(|record| matches!(record.state, crate::journal::ObjectUploadState::Uploaded | crate::journal::ObjectUploadState::Verified))
        .count();
    for object_id in &reachable_object_ids {
        let relative_path = format!("objects/{object_id}.json");
        if remote.exists_physical(&relative_path) {
            continue;
        }
        let object_path = options
            .repo_root
            .join(".e2v")
            .join("objects")
            .join(format!("{object_id}.json"));
        let bytes = std::fs::read(&object_path)?;
        remote.put_physical(&relative_path, &bytes)?;
        journal.record_verified(&operation_id, object_id, "object")?;
    }

    let config_bytes = sync_support::read_config_bytes(&options.repo_root)?;
    let keyring_files = sync_support::list_keyring_files(&options.repo_root)?;
    let layout_root_bytes = sync_support::read_layout_root_bytes(&options.repo_root)?;
    let layout_root: LayoutRoot = serde_json::from_slice(&layout_root_bytes)?;
    let default_ref_bytes = sync_support::read_default_ref_bytes(&options.repo_root)?;
    let current_remote_ref = remote.read_ref(&RefToken::new(branch_token.clone()))?;
    if current_remote_ref
        .as_ref()
        .map(|stored_ref| stored_ref.value.bytes.as_slice() == default_ref_bytes.as_slice())
        .unwrap_or(false)
    {
        return Ok(ResumeResult {
            published_snapshot_id: snapshot.snapshot_id,
            skipped_uploaded_objects,
        });
    }
    let expected_ref_version = journal
        .operation_metadata(&operation_id)?
        .and_then(|metadata| metadata.expected_ref_version);
    let publisher = SimpleTransactionPublisher::new(
        remote.capability().clone(),
        journal.clone(),
        remote.clone(),
    );
    let session = publisher.begin(PublishPlan {
        operation_id: operation_id.clone(),
        target_branch_token: branch_token.clone(),
        expected_ref_version,
        writer_mode: remote.capability().writer_mode(),
    })?;
    let session = crate::transaction::PublishSession {
        next_layout_root: Some(layout_root.clone()),
        next_layout_root_bytes: Some(layout_root_bytes.clone()),
        ..session
    };
    remote.put_physical("control/config.json", &config_bytes)?;
    for keyring_file in keyring_files {
        let file_name = keyring_file
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow::anyhow!("invalid keyring path {}", keyring_file.display()))?;
        let bytes = std::fs::read(&keyring_file)?;
        remote.put_physical(&format!("control/keyring/{file_name}"), &bytes)?;
    }
    publisher.publish_layout_if_needed(&session)?;
    publisher.pre_commit_verify(&session)?;
    remote.put_physical("control/refs/default.json", &default_ref_bytes)?;
    let publish_result = publisher.publish_ref(&session, EncryptedRef::new(default_ref_bytes))?;
    if !publish_result.applied {
        anyhow::bail!("push requires needs-rebase recovery");
    }
    publisher.complete(session)?;

    Ok(ResumeResult {
        published_snapshot_id: snapshot.snapshot_id,
        skipped_uploaded_objects,
    })
}
