use std::path::PathBuf;

use anyhow::Result;

use e2v_core::{sync_support, RepositoryFacade};
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
    if remote
        .read_ref(&RefToken::new(options.branch_token.clone()))?
        .is_some()
    {
        anyhow::bail!("push requires needs-rebase recovery");
    }

    let (_state, snapshot) = sync_support::export_head_snapshot(facade, &options.repo_root)?;
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
        OperationMetadata::push(options.branch_token.clone()),
    )?;
    let publisher = SimpleTransactionPublisher::new(
        remote.capability().clone(),
        journal.clone(),
        remote.clone(),
    );
    let session = publisher.begin(PublishPlan {
        operation_id: operation_id.clone(),
        target_branch_token: options.branch_token.clone(),
        expected_ref_version: None,
        writer_mode: remote.capability().writer_mode(),
    })?;

    let object_files = sync_support::list_local_object_files(&options.repo_root)?;
    for object_path in &object_files {
        let object_name = object_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow::anyhow!("invalid object path {}", object_path.display()))?;
        let relative_path = format!("objects/{object_name}");
        let bytes = std::fs::read(object_path)?;
        remote.put_physical(&relative_path, &bytes)?;
        publisher.record_uploaded(
            &session,
            PublishedObject {
                object_id: object_name.trim_end_matches(".json").to_string(),
                object_type: "object".to_string(),
            },
        )?;
        journal.record_verified(
            &operation_id,
            object_name.trim_end_matches(".json"),
            "object",
        )?;
    }

    let layout_root_bytes = sync_support::read_layout_root_bytes(&options.repo_root)?;
    let config_bytes = sync_support::read_config_bytes(&options.repo_root)?;
    let layout_root: LayoutRoot = serde_json::from_slice(&layout_root_bytes)?;
    let _ = remote.compare_and_swap_layout_root(1, layout_root.clone())?;
    remote.put_physical("layout_root.json", &layout_root_bytes)?;
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
    let publish_result = publisher.publish_ref(&session, EncryptedRef::new(default_ref_bytes.clone()))?;
    if publish_result.applied {
        let _ = remote.compare_and_swap_ref(
            &RefToken::new(options.branch_token.clone()),
            None,
            EncryptedRef::new(default_ref_bytes),
        )?;
    }
    publisher.complete(session)?;

    Ok(PushResult {
        published_snapshot_id: snapshot.snapshot_id,
        uploaded_objects: object_files.len(),
    })
}

pub fn resume_push<R: RemoteBackend>(
    facade: &RepositoryFacade,
    remote: &R,
    options: ResumeOptions,
) -> Result<ResumeResult> {
    let (_state, snapshot) = sync_support::export_head_snapshot(facade, &options.repo_root)?;
    let journal = OperationJournal::new(options.repo_root.join(".e2v").join("journal").join("sync"))?;
    let operation_id = OperationId::new(options.operation_id);
    let pending = journal.pending_objects(&operation_id)?;
    let skipped_uploaded_objects = pending
        .iter()
        .filter(|record| matches!(record.state, crate::journal::ObjectUploadState::Uploaded | crate::journal::ObjectUploadState::Verified))
        .count();
    let default_ref_bytes = sync_support::read_default_ref_bytes(&options.repo_root)?;
    let _ = remote.compare_and_swap_ref(
        &RefToken::new(options.branch_token),
        None,
        EncryptedRef::new(default_ref_bytes),
    )?;

    Ok(ResumeResult {
        published_snapshot_id: snapshot.snapshot_id,
        skipped_uploaded_objects,
    })
}
