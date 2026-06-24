use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use e2v_core::{ManifestStore, ManifestStoreApi, RepositoryFacade, sync_support};
use e2v_store::{EncryptedRef, LayoutRoot, RefToken, RemoteBackend, validate_object_id_value};

use crate::bundle::{
    BundledObjectLocation, ObjectBundleBuilder, bundle_paths, load_remote_bundle_locations,
    load_remote_operation_bundle_locations,
};
use crate::journal::{OperationId, OperationJournal, OperationMetadata, validate_sync_identifier};
use crate::object_type::infer_object_type_from_hint;
use crate::publisher::{SimpleTransactionPublisher, TransactionPublisher};
use crate::transaction::{PublishPlan, PublishedObject};

const KEYRING_LOCK_FILE: &str = "keyring.lock";

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

const RESUME_OBJECT_BATCH_SIZE: usize = 128;
const SMALL_OBJECT_BUNDLE_BATCH_SIZE: usize = 256;
const SMALL_OBJECT_MAX_BYTES: usize = 1024 * 1024;
const DEFAULT_SMALL_OBJECT_BUNDLE_THRESHOLD: usize = 100_000;
thread_local! {
    static SMALL_OBJECT_BUNDLE_THRESHOLD_OVERRIDE: Cell<Option<usize>> = const { Cell::new(None) };
}

pub fn small_object_bundle_threshold() -> usize {
    SMALL_OBJECT_BUNDLE_THRESHOLD_OVERRIDE.with(|override_cell| {
        override_cell
            .get()
            .unwrap_or(DEFAULT_SMALL_OBJECT_BUNDLE_THRESHOLD)
    })
}

fn should_bundle_small_objects(object_count: usize) -> bool {
    object_count >= small_object_bundle_threshold()
}

pub fn override_small_object_bundle_threshold_for_test(
    threshold: usize,
) -> SmallObjectBundleThresholdGuard {
    let previous = SMALL_OBJECT_BUNDLE_THRESHOLD_OVERRIDE.with(|override_cell| {
        let previous = override_cell.get();
        override_cell.set(Some(threshold));
        previous
    });
    SmallObjectBundleThresholdGuard { previous }
}

pub struct SmallObjectBundleThresholdGuard {
    previous: Option<usize>,
}

impl Drop for SmallObjectBundleThresholdGuard {
    fn drop(&mut self) {
        SMALL_OBJECT_BUNDLE_THRESHOLD_OVERRIDE.with(|override_cell| {
            override_cell.set(self.previous);
        });
    }
}

fn local_object_path(repo_root: &Path, object_id: &str) -> PathBuf {
    repo_root
        .join(".e2v")
        .join("objects")
        .join(format!("{object_id}.json"))
}

fn inventory_has_object(
    loose_object_ids: &BTreeSet<String>,
    bundle_locations: &BTreeMap<String, BundledObjectLocation>,
    object_id: &str,
) -> bool {
    loose_object_ids.contains(object_id) || bundle_locations.contains_key(object_id)
}

fn load_remote_loose_object_ids<R: RemoteBackend>(remote: &R) -> Result<BTreeSet<String>> {
    let mut object_ids = BTreeSet::new();
    for relative_path in remote.list_physical("objects/")? {
        let Some(object_id) = relative_path
            .strip_prefix("objects/")
            .and_then(|value| value.strip_suffix(".json"))
        else {
            continue;
        };
        if validate_object_id_value(object_id).is_ok() {
            object_ids.insert(object_id.to_string());
        }
    }
    Ok(object_ids)
}

fn load_remote_object_inventory<R: RemoteBackend>(
    remote: &R,
) -> Result<(BTreeSet<String>, BTreeMap<String, BundledObjectLocation>)> {
    Ok((
        load_remote_loose_object_ids(remote)?,
        load_remote_bundle_locations(remote)?,
    ))
}

fn load_remote_resume_bundle_locations<R: RemoteBackend>(
    remote: &R,
    operation_id: &OperationId,
) -> Result<BTreeMap<String, BundledObjectLocation>> {
    load_remote_operation_bundle_locations(remote, &operation_id.value)
}

fn remote_object_authenticates_for_repo<R: RemoteBackend>(
    repo_root: &Path,
    remote: &R,
    bundle_locations: &BTreeMap<String, BundledObjectLocation>,
    bundle_cache: &mut BTreeMap<String, Vec<u8>>,
    object_id: &str,
    expected_type: &str,
) -> bool {
    let control_dir = repo_root.join(".e2v");
    let secrets = match e2v_core::sync_support::open_repo_secrets_for_sync(&control_dir) {
        Ok(secrets) => secrets,
        Err(_) => return false,
    };
    let store = e2v_store::DirectLayoutObjectStore::new(&control_dir, secrets);

    if let Ok(bytes) = remote.get_physical(&format!("objects/{object_id}.json")) {
        let target_path = local_object_path(repo_root, object_id);
        let original = std::fs::read(&target_path).ok();
        if std::fs::write(&target_path, &bytes).is_err() {
            return false;
        }
        let verified = store.get_object(object_id, expected_type).is_ok();
        match original {
            Some(original_bytes) => {
                let _ = std::fs::write(&target_path, original_bytes);
            }
            None => {
                let _ = std::fs::remove_file(&target_path);
            }
        }
        return verified;
    }

    if let Some(location) = bundle_locations.get(object_id) {
        if !bundle_cache.contains_key(&location.data_path) {
            let bundle_len = match remote.stat_physical(&location.data_path) {
                Ok(stat) => match usize::try_from(stat.length) {
                    Ok(length) => length,
                    Err(_) => return false,
                },
                Err(_) => return false,
            };
            let bundle_bytes = match remote.get_physical_range(&location.data_path, 0, bundle_len) {
                Ok(bytes) => bytes,
                Err(_) => return false,
            };
            bundle_cache.insert(location.data_path.clone(), bundle_bytes);
        }
        let Some(bundle_bytes) = bundle_cache.get(&location.data_path) else {
            return false;
        };
        let end = location.offset.saturating_add(location.length);
        if end > bundle_bytes.len() {
            return false;
        }
        let bytes = bundle_bytes[location.offset..end].to_vec();
        let target_path = local_object_path(repo_root, object_id);
        let original = std::fs::read(&target_path).ok();
        if std::fs::write(&target_path, &bytes).is_err() {
            return false;
        }
        let verified = store.get_object(object_id, expected_type).is_ok();
        match original {
            Some(original_bytes) => {
                let _ = std::fs::write(&target_path, original_bytes);
            }
            None => {
                let _ = std::fs::remove_file(&target_path);
            }
        }
        return verified;
    }

    false
}

fn remote_snapshot_graph_authenticates_for_repo<R: RemoteBackend>(
    repo_root: &Path,
    remote: &R,
    loose_object_ids: &BTreeSet<String>,
    bundle_locations: &BTreeMap<String, BundledObjectLocation>,
    bundle_cache: &mut BTreeMap<String, Vec<u8>>,
    snapshot_id: &str,
) -> bool {
    if !remote_object_authenticates_for_repo(
        repo_root,
        remote,
        bundle_locations,
        bundle_cache,
        snapshot_id,
        "snapshot",
    ) {
        return false;
    }

    let manifest_store = ManifestStore::new(repo_root);
    let reachable_object_ids = match manifest_store.collect_reachable_object_ids(snapshot_id) {
        Ok(ids) => ids,
        Err(_) => return false,
    };

    for object_id in reachable_object_ids {
        if object_id == snapshot_id {
            continue;
        }
        let object_type = infer_object_type_for_resume_candidate(repo_root, &object_id);
        if !inventory_has_object(loose_object_ids, bundle_locations, &object_id)
            || !remote_object_authenticates_for_repo(
                repo_root,
                remote,
                bundle_locations,
                bundle_cache,
                &object_id,
                object_type,
            )
        {
            return false;
        }
    }

    true
}

fn ensure_remote_object_inventory_loaded<'a, R: RemoteBackend>(
    remote: &'a R,
    inventory: &'a mut Option<(BTreeSet<String>, BTreeMap<String, BundledObjectLocation>)>,
) -> Result<&'a (BTreeSet<String>, BTreeMap<String, BundledObjectLocation>)> {
    if inventory.is_none() {
        *inventory = Some(load_remote_object_inventory(remote)?);
    }
    Ok(inventory.as_ref().expect("inventory initialized"))
}

fn remote_object_authenticates_for_resume<R: RemoteBackend>(
    repo_root: &Path,
    remote: &R,
    current_operation_bundle_locations: &BTreeMap<String, BundledObjectLocation>,
    allow_inventory_lookup: bool,
    fallback_inventory: &mut Option<(BTreeSet<String>, BTreeMap<String, BundledObjectLocation>)>,
    bundle_cache: &mut BTreeMap<String, Vec<u8>>,
    object_id: &str,
    expected_type: &str,
) -> Result<bool> {
    if remote_object_authenticates_for_repo(
        repo_root,
        remote,
        current_operation_bundle_locations,
        bundle_cache,
        object_id,
        expected_type,
    ) {
        return Ok(true);
    }

    if !allow_inventory_lookup {
        return Ok(false);
    }

    let (remote_loose_object_ids, remote_bundle_locations) =
        ensure_remote_object_inventory_loaded(remote, fallback_inventory)?;
    Ok(
        inventory_has_object(remote_loose_object_ids, remote_bundle_locations, object_id)
            && remote_object_authenticates_for_repo(
                repo_root,
                remote,
                remote_bundle_locations,
                bundle_cache,
                object_id,
                expected_type,
            ),
    )
}

fn infer_object_type_for_resume_candidate(repo_root: &Path, object_id: &str) -> &'static str {
    let facade = RepositoryFacade::new();
    let hint = e2v_core::sync_support::read_local_object_type_hint(repo_root, object_id).ok();
    infer_object_type_from_hint(hint.as_deref(), |object_type| {
        facade
            .verify_object(repo_root, object_id, object_type)
            .is_ok()
    })
}

fn verify_remote_reachable_objects(
    loose_object_ids: &BTreeSet<String>,
    bundle_locations: &BTreeMap<String, BundledObjectLocation>,
    object_ids: &[String],
) -> Result<()> {
    for object_id in object_ids {
        anyhow::ensure!(
            inventory_has_object(loose_object_ids, bundle_locations, object_id),
            "pre-commit verify failed: reachable object missing from remote: {object_id}"
        );
    }
    Ok(())
}

fn verify_remote_reachable_objects_for_resume<R: RemoteBackend>(
    repo_root: &Path,
    remote: &R,
    current_operation_bundle_locations: &BTreeMap<String, BundledObjectLocation>,
    allow_inventory_lookup: bool,
    fallback_inventory: &mut Option<(BTreeSet<String>, BTreeMap<String, BundledObjectLocation>)>,
    bundle_cache: &mut BTreeMap<String, Vec<u8>>,
    snapshot_id: &str,
    object_ids: &[String],
) -> Result<()> {
    for object_id in object_ids {
        let expected_type = if object_id == snapshot_id {
            "snapshot"
        } else {
            infer_object_type_for_resume_candidate(repo_root, object_id)
        };
        anyhow::ensure!(
            remote_object_authenticates_for_resume(
                repo_root,
                remote,
                current_operation_bundle_locations,
                allow_inventory_lookup,
                fallback_inventory,
                bundle_cache,
                object_id,
                expected_type,
            )?,
            "pre-commit verify failed: reachable object missing from remote: {object_id}"
        );
    }
    Ok(())
}

fn next_bundle_batch_index<R: RemoteBackend>(
    remote: &R,
    operation_id: &OperationId,
) -> Result<usize> {
    let prefix = format!("bundles/index/{}-", operation_id.value);
    Ok(remote
        .list_physical("bundles/index/")?
        .into_iter()
        .filter(|path| path.starts_with(&prefix))
        .count())
}

fn upload_object_batch<R, F>(
    remote: &R,
    repo_root: &Path,
    operation_id: &OperationId,
    object_ids: &[String],
    bundle_enabled: bool,
    bundle_batch_index: &mut usize,
    mut on_uploaded: F,
) -> Result<()>
where
    R: RemoteBackend,
    F: FnMut(&str) -> Result<()>,
{
    let mut bundle_builder = if bundle_enabled {
        Some(ObjectBundleBuilder::new(
            &operation_id.value,
            *bundle_batch_index,
        )?)
    } else {
        None
    };
    let mut bundled_object_ids = Vec::new();
    for object_id in object_ids {
        validate_object_id_value(object_id)?;
        let bytes = std::fs::read(local_object_path(repo_root, object_id))?;
        if let Some(builder) = bundle_builder.as_mut() {
            if bytes.len() <= SMALL_OBJECT_MAX_BYTES {
                builder.push_object(object_id.clone(), &bytes);
                bundled_object_ids.push(object_id.clone());
                continue;
            }
        }

        remote.put_physical(&format!("objects/{object_id}.json"), &bytes)?;
        on_uploaded(object_id)?;
    }

    if let Some(builder) = bundle_builder {
        if !bundled_object_ids.is_empty() {
            let (index, payload) = builder.finish();
            let (_, data_path, index_path) =
                bundle_paths(&operation_id.value, *bundle_batch_index)?;
            remote.put_physical(&data_path, &payload)?;
            remote.put_physical(&index_path, &serde_json::to_vec_pretty(&index)?)?;
            *bundle_batch_index += 1;
            for object_id in &bundled_object_ids {
                on_uploaded(object_id)?;
            }
        }
    }

    Ok(())
}

fn upload_objects_with_policy<R, F>(
    remote: &R,
    repo_root: &Path,
    operation_id: &OperationId,
    object_ids: &[String],
    bundle_enabled: bool,
    mut on_uploaded: F,
) -> Result<()>
where
    R: RemoteBackend,
    F: FnMut(&str) -> Result<()>,
{
    if object_ids.is_empty() {
        return Ok(());
    }
    let mut bundle_batch_index = if bundle_enabled {
        next_bundle_batch_index(remote, operation_id)?
    } else {
        0
    };
    for object_batch in object_ids.chunks(SMALL_OBJECT_BUNDLE_BATCH_SIZE) {
        upload_object_batch(
            remote,
            repo_root,
            operation_id,
            object_batch,
            bundle_enabled,
            &mut bundle_batch_index,
            &mut on_uploaded,
        )?;
    }
    Ok(())
}

fn remote_control_ref_mirror_matches<R: RemoteBackend>(
    remote: &R,
    expected_default_ref_bytes: &[u8],
) -> bool {
    remote_physical_matches(
        remote,
        "control/refs/default.json",
        expected_default_ref_bytes,
    )
}

fn remote_physical_matches<R: RemoteBackend>(
    remote: &R,
    relative_path: &str,
    expected_bytes: &[u8],
) -> bool {
    remote
        .get_physical(relative_path)
        .map(|bytes| bytes == expected_bytes)
        .unwrap_or(false)
}

fn remote_control_plane_matches<R: RemoteBackend>(
    remote: &R,
    config_bytes: &[u8],
    layout_root_bytes: &[u8],
) -> bool {
    remote_physical_matches(remote, "control/config.json", config_bytes)
        && remote_physical_matches(remote, "layout_root.json", layout_root_bytes)
}

fn remote_keyring_matches<R: RemoteBackend>(remote: &R, keyring_files: &[PathBuf]) -> bool {
    keyring_files.iter().all(|keyring_file| {
        let file_name = match keyring_file.file_name().and_then(|name| name.to_str()) {
            Some(name) => name,
            None => return false,
        };
        if file_name == KEYRING_LOCK_FILE {
            return true;
        }
        let relative_path = format!("control/keyring/{file_name}");
        let expected_bytes = match std::fs::read(keyring_file) {
            Ok(bytes) => bytes,
            Err(_) => return false,
        };
        remote_physical_matches(remote, &relative_path, &expected_bytes)
    })
}

fn upload_remote_keyring_generations<R: RemoteBackend>(
    remote: &R,
    keyring_files: &[PathBuf],
) -> Result<()> {
    for keyring_file in keyring_files {
        let file_name = keyring_file
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow::anyhow!("invalid keyring path {}", keyring_file.display()))?;
        if file_name == KEYRING_LOCK_FILE || file_name == "keyring.current" {
            continue;
        }
        let bytes = std::fs::read(keyring_file)?;
        remote.put_physical(&format!("control/keyring/{file_name}"), &bytes)?;
    }
    Ok(())
}

fn publish_remote_keyring_pointer<R: RemoteBackend>(
    remote: &R,
    keyring_files: &[PathBuf],
) -> Result<()> {
    let pointer_file = keyring_files
        .iter()
        .find(|path| path.file_name().and_then(|name| name.to_str()) == Some("keyring.current"))
        .context("missing local keyring pointer file")?;
    let bytes = std::fs::read(pointer_file)?;
    remote.put_physical("control/keyring/keyring.current", &bytes)?;
    Ok(())
}

fn cleanup_completed_operation_markers<R: RemoteBackend>(
    remote: &R,
    operation_id: &OperationId,
    branch_token: &str,
) -> Result<()> {
    let intent_path = format!("transactions/active/{}.intent", operation_id.value);
    if remote.exists_physical(&intent_path) {
        remote.delete_physical(&intent_path)?;
    }

    let lease_path = format!("leases/{branch_token}.lock");
    if remote.exists_physical(&lease_path) {
        let lease_bytes = remote.get_physical(&lease_path)?;
        let lease_marker: serde_json::Value = serde_json::from_slice(&lease_bytes)?;
        if lease_marker
            .get("operation_id")
            .and_then(|value| value.as_str())
            == Some(operation_id.value.as_str())
        {
            remote.delete_physical(&lease_path)?;
        }
    }

    Ok(())
}

pub fn push_head<R: RemoteBackend + Clone>(
    facade: &RepositoryFacade,
    remote: &R,
    options: PushOptions,
) -> Result<PushResult> {
    validate_sync_identifier("branch token", &options.branch_token)?;
    validate_sync_identifier("operation id", &options.operation_id)?;
    let (_state, snapshot) = sync_support::export_head_snapshot(facade, &options.repo_root)?;
    let layout_root_bytes = sync_support::read_layout_root_bytes(&options.repo_root)?;
    let config_bytes = sync_support::read_config_bytes(&options.repo_root)?;
    let keyring_files = sync_support::list_keyring_files(&options.repo_root)?;
    let default_ref_bytes = sync_support::read_default_ref_bytes(&options.repo_root)?;
    let expected_ref_version =
        match remote.read_ref(&RefToken::new(options.branch_token.clone()))? {
            Some(stored_ref) => {
                let remote_head_snapshot_id = sync_support::decode_ref_head_snapshot_id(
                    &options.repo_root,
                    &stored_ref.value.bytes,
                )?;
                if remote_head_snapshot_id.as_deref() == Some(snapshot.snapshot_id.as_str()) {
                    let remote_ref_matches_local =
                        stored_ref.value.bytes.as_slice() == default_ref_bytes.as_slice();
                    if remote_ref_matches_local
                        && remote_control_ref_mirror_matches(remote, &default_ref_bytes)
                        && remote_control_plane_matches(remote, &config_bytes, &layout_root_bytes)
                        && remote_keyring_matches(remote, &keyring_files)
                    {
                        return Ok(PushResult {
                            published_snapshot_id: snapshot.snapshot_id,
                            uploaded_objects: 0,
                        });
                    }
                    if remote_ref_matches_local {
                        remote.put_physical("control/config.json", &config_bytes)?;
                        upload_remote_keyring_generations(remote, &keyring_files)?;
                        remote.put_physical("layout_root.json", &layout_root_bytes)?;
                        publish_remote_keyring_pointer(remote, &keyring_files)?;
                        remote.put_physical("control/refs/default.json", &default_ref_bytes)?;
                        return Ok(PushResult {
                            published_snapshot_id: snapshot.snapshot_id,
                            uploaded_objects: 0,
                        });
                    }
                }
                let can_fast_forward = match remote_head_snapshot_id.as_deref() {
                    Some(remote_head) => snapshot
                        .ancestor_snapshot_ids
                        .iter()
                        .any(|ancestor| ancestor == remote_head),
                    None => snapshot.parent_snapshot_id.is_none(),
                };
                if !can_fast_forward {
                    anyhow::bail!("push requires needs-rebase recovery");
                }
                Some(stored_ref.version.value)
            }
            None => None,
        };
    let (mut remote_loose_object_ids, mut remote_bundle_locations) =
        load_remote_object_inventory(remote)?;
    let mut remote_bundle_cache = BTreeMap::new();
    for ancestor_snapshot_id in &snapshot.ancestor_snapshot_ids {
        if !inventory_has_object(
            &remote_loose_object_ids,
            &remote_bundle_locations,
            ancestor_snapshot_id,
        ) {
            anyhow::bail!(
                "push rejected: ancestor closure incomplete, missing remote snapshot {ancestor_snapshot_id}"
            );
        }
        anyhow::ensure!(
            remote_snapshot_graph_authenticates_for_repo(
                &options.repo_root,
                remote,
                &remote_loose_object_ids,
                &remote_bundle_locations,
                &mut remote_bundle_cache,
                ancestor_snapshot_id,
            ),
            "push rejected: reachable remote snapshot failed verification: {ancestor_snapshot_id}"
        );
    }
    let journal =
        OperationJournal::new(options.repo_root.join(".e2v").join("journal").join("sync"))?;
    let operation_id = OperationId::new(options.operation_id)?;
    journal.begin_operation(
        &operation_id,
        OperationMetadata::push(options.branch_token.clone(), expected_ref_version),
    )?;
    let publisher = SimpleTransactionPublisher::new(
        remote.capability().clone(),
        journal.clone(),
        remote.clone(),
    );
    let layout_root: LayoutRoot = serde_json::from_slice(&layout_root_bytes)?;
    let session = publisher.begin(PublishPlan {
        operation_id: operation_id.clone(),
        target_branch_token: options.branch_token.clone(),
        expected_ref_version,
        writer_mode: remote.capability().push_write_mode(),
    })?;
    let session = crate::transaction::PublishSession {
        next_layout_root: Some(layout_root.clone()),
        next_layout_root_bytes: Some(layout_root_bytes.clone()),
        ..session
    };

    let manifest_store = ManifestStore::new(&options.repo_root);
    let reachable_object_ids =
        manifest_store.collect_reachable_object_ids(&snapshot.snapshot_id)?;
    let bundle_enabled = should_bundle_small_objects(reachable_object_ids.len());
    let missing_object_ids = reachable_object_ids
        .iter()
        .filter(|object_id| {
            !inventory_has_object(
                &remote_loose_object_ids,
                &remote_bundle_locations,
                object_id,
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    for object_id in &reachable_object_ids {
        journal.plan_object(&operation_id, object_id, "object")?;
    }
    upload_objects_with_policy(
        remote,
        &options.repo_root,
        &operation_id,
        &missing_object_ids,
        bundle_enabled,
        |object_id| {
            publisher.record_uploaded(
                &session,
                PublishedObject {
                    object_id: object_id.to_string(),
                    object_type: "object".to_string(),
                },
            )?;
            journal.record_verified(&operation_id, object_id, "object")
        },
    )?;

    remote.put_physical("control/config.json", &config_bytes)?;
    upload_remote_keyring_generations(remote, &keyring_files)?;
    publisher.publish_layout_if_needed(&session)?;
    publisher.pre_commit_verify(&session)?;

    (remote_loose_object_ids, remote_bundle_locations) = load_remote_object_inventory(remote)?;
    verify_remote_reachable_objects(
        &remote_loose_object_ids,
        &remote_bundle_locations,
        &reachable_object_ids,
    )?;
    let publish_result =
        publisher.publish_ref(&session, EncryptedRef::new(default_ref_bytes.clone()))?;
    if !publish_result.applied {
        anyhow::bail!("push requires needs-rebase recovery");
    }
    publish_remote_keyring_pointer(remote, &keyring_files)?;
    remote.put_physical("control/refs/default.json", &default_ref_bytes)?;
    publisher.complete(session)?;

    Ok(PushResult {
        published_snapshot_id: snapshot.snapshot_id,
        uploaded_objects: missing_object_ids.len(),
    })
}

pub fn resume_push<R: RemoteBackend + Clone>(
    facade: &RepositoryFacade,
    remote: &R,
    options: ResumeOptions,
) -> Result<ResumeResult> {
    validate_sync_identifier("branch token", &options.branch_token)?;
    validate_sync_identifier("operation id", &options.operation_id)?;
    let (_state, snapshot) = sync_support::export_head_snapshot(facade, &options.repo_root)?;
    let journal =
        OperationJournal::new(options.repo_root.join(".e2v").join("journal").join("sync"))?;
    let operation_id = OperationId::new(options.operation_id)?;
    let branch_token = options.branch_token;
    let total_tracked_objects = journal.count_objects_in_states(
        &operation_id,
        &[
            crate::journal::ObjectUploadState::Planned,
            crate::journal::ObjectUploadState::Uploaded,
            crate::journal::ObjectUploadState::Verified,
            crate::journal::ObjectUploadState::Failed,
        ],
    )?;
    let bundle_enabled = should_bundle_small_objects(total_tracked_objects);
    let skipped_uploaded_objects = journal.count_objects_in_states(
        &operation_id,
        &[
            crate::journal::ObjectUploadState::Uploaded,
            crate::journal::ObjectUploadState::Verified,
        ],
    )?;
    let config_bytes = sync_support::read_config_bytes(&options.repo_root)?;
    let keyring_files = sync_support::list_keyring_files(&options.repo_root)?;
    let layout_root_bytes = sync_support::read_layout_root_bytes(&options.repo_root)?;
    let layout_root: LayoutRoot = serde_json::from_slice(&layout_root_bytes)?;
    let default_ref_bytes = sync_support::read_default_ref_bytes(&options.repo_root)?;
    let expected_ref_version = journal
        .operation_metadata(&operation_id)?
        .and_then(|metadata| metadata.expected_ref_version);
    let current_remote_ref = remote.read_ref(&RefToken::new(branch_token.clone()))?;
    let remote_ref_matches_local = current_remote_ref
        .as_ref()
        .map(|stored_ref| stored_ref.value.bytes.as_slice() == default_ref_bytes.as_slice())
        .unwrap_or(false);
    let mut saw_journal_objects = false;
    let mut remote_bundle_locations = load_remote_resume_bundle_locations(remote, &operation_id)?;
    let mut remote_inventory = None;
    let mut remote_bundle_cache = BTreeMap::new();
    let mut pending_cursor = Some(0usize);
    while let Some(cursor) = pending_cursor {
        let batch =
            journal.pending_object_batch(&operation_id, cursor, RESUME_OBJECT_BATCH_SIZE)?;
        let mut missing_object_ids = Vec::new();
        for record in &batch.records {
            if record.object_type != "object" {
                continue;
            }
            saw_journal_objects = true;
            if remote_object_authenticates_for_resume(
                &options.repo_root,
                remote,
                &remote_bundle_locations,
                false,
                &mut remote_inventory,
                &mut remote_bundle_cache,
                &record.object_id,
                infer_object_type_for_resume_candidate(&options.repo_root, &record.object_id),
            )? {
                journal.record_verified(&operation_id, &record.object_id, "object")?;
                continue;
            }
            missing_object_ids.push(record.object_id.clone());
        }
        upload_objects_with_policy(
            remote,
            &options.repo_root,
            &operation_id,
            &missing_object_ids,
            bundle_enabled,
            |object_id| journal.record_verified(&operation_id, object_id, "object"),
        )?;
        remote_bundle_locations = load_remote_resume_bundle_locations(remote, &operation_id)?;
        remote_inventory = None;
        remote_bundle_cache.clear();
        pending_cursor = batch.next_cursor;
    }

    if !saw_journal_objects {
        let (remote_loose_object_ids, remote_bundle_locations) =
            load_remote_object_inventory(remote)?;
        let manifest_store = ManifestStore::new(&options.repo_root);
        let reachable_object_ids =
            manifest_store.collect_reachable_object_ids(&snapshot.snapshot_id)?;
        let remote_is_already_complete = remote_ref_matches_local
            && inventory_has_object(
                &remote_loose_object_ids,
                &remote_bundle_locations,
                &snapshot.snapshot_id,
            )
            && remote_snapshot_graph_authenticates_for_repo(
                &options.repo_root,
                remote,
                &remote_loose_object_ids,
                &remote_bundle_locations,
                &mut remote_bundle_cache,
                &snapshot.snapshot_id,
            );
        if !remote_is_already_complete {
            let mut missing_object_ids = Vec::new();
            for object_id in &reachable_object_ids {
                let object_type = if object_id == &snapshot.snapshot_id {
                    "snapshot"
                } else {
                    infer_object_type_for_resume_candidate(&options.repo_root, object_id)
                };
                if inventory_has_object(
                    &remote_loose_object_ids,
                    &remote_bundle_locations,
                    object_id,
                ) && remote_object_authenticates_for_repo(
                    &options.repo_root,
                    remote,
                    &remote_bundle_locations,
                    &mut remote_bundle_cache,
                    object_id,
                    object_type,
                ) {
                    continue;
                }
                missing_object_ids.push(object_id.clone());
            }
            upload_objects_with_policy(
                remote,
                &options.repo_root,
                &operation_id,
                &missing_object_ids,
                reachable_object_ids.len() >= small_object_bundle_threshold(),
                |object_id| journal.record_verified(&operation_id, object_id, "object"),
            )?;
        }
    }

    if remote_ref_matches_local
        && remote_control_ref_mirror_matches(remote, &default_ref_bytes)
        && remote_control_plane_matches(remote, &config_bytes, &layout_root_bytes)
        && remote_keyring_matches(remote, &keyring_files)
    {
        cleanup_completed_operation_markers(remote, &operation_id, &branch_token)?;
        return Ok(ResumeResult {
            published_snapshot_id: snapshot.snapshot_id,
            skipped_uploaded_objects,
        });
    }
    let publisher = SimpleTransactionPublisher::new(
        remote.capability().clone(),
        journal.clone(),
        remote.clone(),
    );
    let session = publisher.begin(PublishPlan {
        operation_id: operation_id.clone(),
        target_branch_token: branch_token.clone(),
        expected_ref_version,
        writer_mode: remote.capability().push_write_mode(),
    })?;
    let session = crate::transaction::PublishSession {
        next_layout_root: Some(layout_root.clone()),
        next_layout_root_bytes: Some(layout_root_bytes.clone()),
        ..session
    };
    if remote_ref_matches_local {
        remote.put_physical("control/config.json", &config_bytes)?;
        upload_remote_keyring_generations(remote, &keyring_files)?;
        publish_remote_keyring_pointer(remote, &keyring_files)?;
        publisher.publish_layout_if_needed(&session)?;
        remote.put_physical("control/refs/default.json", &default_ref_bytes)?;
        publisher.complete(session)?;
        return Ok(ResumeResult {
            published_snapshot_id: snapshot.snapshot_id,
            skipped_uploaded_objects,
        });
    }
    remote.put_physical("control/config.json", &config_bytes)?;
    upload_remote_keyring_generations(remote, &keyring_files)?;
    publisher.publish_layout_if_needed(&session)?;
    publisher.pre_commit_verify(&session)?;
    let manifest_store = ManifestStore::new(&options.repo_root);
    let reachable_object_ids =
        manifest_store.collect_reachable_object_ids(&snapshot.snapshot_id)?;
    verify_remote_reachable_objects_for_resume(
        &options.repo_root,
        remote,
        &remote_bundle_locations,
        false,
        &mut remote_inventory,
        &mut remote_bundle_cache,
        &snapshot.snapshot_id,
        &reachable_object_ids,
    )?;
    let publish_result =
        publisher.publish_ref(&session, EncryptedRef::new(default_ref_bytes.clone()))?;
    if !publish_result.applied {
        anyhow::bail!("push requires needs-rebase recovery");
    }
    publish_remote_keyring_pointer(remote, &keyring_files)?;
    remote.put_physical("control/refs/default.json", &default_ref_bytes)?;
    publisher.complete(session)?;

    Ok(ResumeResult {
        published_snapshot_id: snapshot.snapshot_id,
        skipped_uploaded_objects,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use e2v_core::{CommitOptions, InitOptions, RepositoryFacade};
    use e2v_store::{BlobStore, MemoryBackend};
    use tempfile::tempdir;

    use super::{
        BundledObjectLocation, remote_object_authenticates_for_repo,
        remote_snapshot_graph_authenticates_for_repo, should_bundle_small_objects,
        upload_object_batch,
    };
    use crate::journal::OperationId;

    #[test]
    fn default_small_object_bundling_threshold_starts_at_100k() {
        assert!(!should_bundle_small_objects(99_999));
        assert!(should_bundle_small_objects(100_000));
    }

    #[test]
    fn remote_object_authentication_rejects_path_traversal_object_id_without_writing_outside_repo()
    {
        let temp = tempdir().unwrap();
        let repo_root = temp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();
        let outside = temp.path().join("outside.json");
        fs::write(&outside, b"keep").unwrap();

        let facade = RepositoryFacade::new();
        facade
            .init(InitOptions {
                repo_root: repo_root.clone(),
                password: "correct horse battery staple".to_string(),
                branch_name: "main".to_string(),
            })
            .unwrap();

        let remote = MemoryBackend::new();
        remote
            .put_physical("objects/..\\..\\..\\outside.json", b"evil")
            .unwrap();

        let verified = remote_object_authenticates_for_repo(
            &repo_root,
            &remote,
            &std::collections::BTreeMap::<String, BundledObjectLocation>::new(),
            &mut std::collections::BTreeMap::<String, Vec<u8>>::new(),
            "..\\..\\..\\outside",
            "snapshot",
        );

        assert!(!verified);
        assert_eq!(fs::read(&outside).unwrap(), b"keep");
    }

    #[test]
    fn remote_object_authentication_rejects_forward_slash_path_traversal_without_writing_outside_repo()
     {
        let temp = tempdir().unwrap();
        let repo_root = temp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();
        let outside = temp.path().join("outside.json");
        fs::write(&outside, b"keep").unwrap();

        let facade = RepositoryFacade::new();
        facade
            .init(InitOptions {
                repo_root: repo_root.clone(),
                password: "correct horse battery staple".to_string(),
                branch_name: "main".to_string(),
            })
            .unwrap();

        let remote = MemoryBackend::new();
        remote
            .put_physical("objects/../../../outside.json", b"evil")
            .unwrap();

        let verified = remote_object_authenticates_for_repo(
            &repo_root,
            &remote,
            &std::collections::BTreeMap::<String, BundledObjectLocation>::new(),
            &mut std::collections::BTreeMap::<String, Vec<u8>>::new(),
            "../../../outside",
            "snapshot",
        );

        assert!(!verified);
        assert_eq!(fs::read(&outside).unwrap(), b"keep");
    }

    #[test]
    fn remote_snapshot_graph_authentication_rejects_path_traversal_snapshot_id_without_writing_outside_repo()
     {
        let temp = tempdir().unwrap();
        let repo_root = temp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();
        let outside = temp.path().join("outside.json");
        fs::write(&outside, b"keep").unwrap();

        let facade = RepositoryFacade::new();
        facade
            .init(InitOptions {
                repo_root: repo_root.clone(),
                password: "correct horse battery staple".to_string(),
                branch_name: "main".to_string(),
            })
            .unwrap();
        fs::write(repo_root.join("hello.txt"), b"hello remote").unwrap();
        facade
            .commit(CommitOptions {
                repo_root: repo_root.clone(),
                message: "seed".to_string(),
            })
            .unwrap();

        let remote = MemoryBackend::new();
        remote
            .put_physical("objects/..\\..\\..\\outside.json", b"evil")
            .unwrap();

        let verified = remote_snapshot_graph_authenticates_for_repo(
            &repo_root,
            &remote,
            &std::collections::BTreeSet::<String>::new(),
            &std::collections::BTreeMap::<String, BundledObjectLocation>::new(),
            &mut std::collections::BTreeMap::<String, Vec<u8>>::new(),
            "..\\..\\..\\outside",
        );

        assert!(!verified);
        assert_eq!(fs::read(&outside).unwrap(), b"keep");
    }

    #[test]
    fn upload_object_batch_rejects_path_traversal_object_id_before_reading_outside_repo() {
        let temp = tempdir().unwrap();
        let repo_root = temp.path().join("repo");
        fs::create_dir_all(repo_root.join(".e2v").join("objects")).unwrap();
        let outside = temp.path().join("outside.json");
        fs::write(&outside, b"leak").unwrap();

        let remote = MemoryBackend::new();
        let operation_id = OperationId::new("upload-batch-op".to_string()).unwrap();
        let mut bundle_batch_index = 0usize;

        let error = upload_object_batch(
            &remote,
            &repo_root,
            &operation_id,
            &["..\\..\\..\\outside".to_string()],
            false,
            &mut bundle_batch_index,
            |_| Ok(()),
        )
        .unwrap_err();

        assert!(
            error.to_string().contains("object id")
                || error.to_string().contains("path")
                || error.to_string().contains("relative"),
            "unexpected error: {error:#}"
        );
        assert_eq!(fs::read(&outside).unwrap(), b"leak");
        assert!(!remote.exists_physical("objects/..\\..\\..\\outside.json"));
    }
}
