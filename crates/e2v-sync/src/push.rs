use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};

use e2v_core::{ManifestStore, ManifestStoreApi, RepositoryFacade, sync_support};
use e2v_store::{EncryptedRef, LayoutRoot, RefToken, RemoteBackend, validate_object_id_value};

use crate::journal::{OperationId, OperationJournal, OperationMetadata, validate_sync_identifier};
use crate::object_type::infer_object_type_from_hint;
use crate::pack::{ObjectPackBuilder, PackedObjectLocation, pack_paths};
use crate::pack_index::{
    encode_pack_index_segment_bytes_for_sync, load_remote_pack_locations_with_local_cache,
    next_pack_index_segment_paths, publish_pack_index_root,
};
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
const SMALL_OBJECT_PACK_BATCH_SIZE: usize = 256;
const SMALL_OBJECT_MAX_BYTES: usize = 1024 * 1024;
const DEFAULT_SMALL_OBJECT_PACK_THRESHOLD: usize = 100_000;
thread_local! {
    static SMALL_OBJECT_PACK_THRESHOLD_OVERRIDE: Cell<Option<usize>> = const { Cell::new(None) };
}

pub fn small_object_pack_threshold() -> usize {
    SMALL_OBJECT_PACK_THRESHOLD_OVERRIDE.with(|override_cell| {
        override_cell
            .get()
            .unwrap_or(DEFAULT_SMALL_OBJECT_PACK_THRESHOLD)
    })
}

fn should_pack_small_objects(object_count: usize) -> bool {
    object_count >= small_object_pack_threshold()
}

pub fn override_small_object_pack_threshold_for_test(
    threshold: usize,
) -> SmallObjectPackThresholdGuard {
    let previous = SMALL_OBJECT_PACK_THRESHOLD_OVERRIDE.with(|override_cell| {
        let previous = override_cell.get();
        override_cell.set(Some(threshold));
        previous
    });
    SmallObjectPackThresholdGuard { previous }
}

pub struct SmallObjectPackThresholdGuard {
    previous: Option<usize>,
}

impl Drop for SmallObjectPackThresholdGuard {
    fn drop(&mut self) {
        SMALL_OBJECT_PACK_THRESHOLD_OVERRIDE.with(|override_cell| {
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
    pack_locations: &BTreeMap<String, PackedObjectLocation>,
    object_id: &str,
) -> bool {
    loose_object_ids.contains(object_id) || pack_locations.contains_key(object_id)
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
    control_dir: &Path,
    remote: &R,
) -> Result<(BTreeSet<String>, BTreeMap<String, PackedObjectLocation>)> {
    let secrets = sync_support::open_or_unlock_repo_secrets_for_sync(control_dir)?;
    let pack_locations =
        load_remote_pack_locations_with_local_cache(remote, control_dir, Some(&secrets))?;
    Ok((load_remote_loose_object_ids(remote)?, pack_locations))
}

fn load_remote_resume_pack_locations<R: RemoteBackend>(
    control_dir: &Path,
    remote: &R,
    operation_id: &OperationId,
) -> Result<BTreeMap<String, PackedObjectLocation>> {
    let secrets = sync_support::open_or_unlock_repo_secrets_for_sync(control_dir)?;
    crate::pack_index::load_remote_operation_pack_locations_with_secrets(
        remote,
        &operation_id.value,
        &secrets,
    )
}

fn remote_object_authenticates_for_repo<R: RemoteBackend>(
    repo_root: &Path,
    remote: &R,
    pack_locations: &BTreeMap<String, PackedObjectLocation>,
    pack_cache: &mut BTreeMap<String, Vec<u8>>,
    object_id: &str,
    expected_type: &str,
) -> bool {
    if let Ok(bytes) = remote.get_physical(&format!("objects/{object_id}.json")) {
        return e2v_core::sync_support::decode_object_bytes_for_sync(
            repo_root,
            object_id,
            expected_type,
            &bytes,
        )
        .is_ok();
    }

    if let Some(location) = pack_locations.get(object_id) {
        let physical_ref = location.physical_ref();
        if !pack_cache.contains_key(&physical_ref.container_id) {
            let pack_len = match remote.stat_physical(&physical_ref.container_id) {
                Ok(stat) => match usize::try_from(stat.length) {
                    Ok(length) => length,
                    Err(_) => return false,
                },
                Err(_) => return false,
            };
            let pack_bytes =
                match remote.get_physical_range(&physical_ref.container_id, 0, pack_len) {
                    Ok(bytes) => bytes,
                    Err(_) => return false,
                };
            pack_cache.insert(physical_ref.container_id.clone(), pack_bytes);
        }
        let Some(pack_bytes) = pack_cache.get(&physical_ref.container_id) else {
            return false;
        };
        let offset = match usize::try_from(physical_ref.offset.unwrap_or(0)) {
            Ok(offset) => offset,
            Err(_) => return false,
        };
        let length = match usize::try_from(physical_ref.length) {
            Ok(length) => length,
            Err(_) => return false,
        };
        let end = offset.saturating_add(length);
        if end > pack_bytes.len() {
            return false;
        }
        let bytes = pack_bytes[offset..end].to_vec();
        return e2v_core::sync_support::decode_object_bytes_for_sync(
            repo_root,
            object_id,
            expected_type,
            &bytes,
        )
        .is_ok();
    }

    false
}

fn remote_snapshot_graph_authenticates_for_repo<R: RemoteBackend>(
    repo_root: &Path,
    remote: &R,
    loose_object_ids: &BTreeSet<String>,
    pack_locations: &BTreeMap<String, PackedObjectLocation>,
    pack_cache: &mut BTreeMap<String, Vec<u8>>,
    snapshot_id: &str,
) -> bool {
    if !remote_object_authenticates_for_repo(
        repo_root,
        remote,
        pack_locations,
        pack_cache,
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
        if !inventory_has_object(loose_object_ids, pack_locations, &object_id)
            || !remote_object_authenticates_for_repo(
                repo_root,
                remote,
                pack_locations,
                pack_cache,
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
    control_dir: &Path,
    remote: &'a R,
    inventory: &'a mut Option<(BTreeSet<String>, BTreeMap<String, PackedObjectLocation>)>,
) -> Result<&'a (BTreeSet<String>, BTreeMap<String, PackedObjectLocation>)> {
    if inventory.is_none() {
        *inventory = Some(load_remote_object_inventory(control_dir, remote)?);
    }
    Ok(inventory.as_ref().expect("inventory initialized"))
}

struct ResumeRemoteAuthContext<'a, R: RemoteBackend> {
    repo_root: &'a Path,
    remote: &'a R,
    current_operation_pack_locations: &'a BTreeMap<String, PackedObjectLocation>,
    allow_full_inventory_lookup: bool,
    remote_inventory_cache:
        &'a mut Option<(BTreeSet<String>, BTreeMap<String, PackedObjectLocation>)>,
    pack_cache: &'a mut BTreeMap<String, Vec<u8>>,
}

fn remote_object_authenticates_for_resume<R: RemoteBackend>(
    context: &mut ResumeRemoteAuthContext<'_, R>,
    object_id: &str,
    expected_type: &str,
) -> Result<bool> {
    if remote_object_authenticates_for_repo(
        context.repo_root,
        context.remote,
        context.current_operation_pack_locations,
        context.pack_cache,
        object_id,
        expected_type,
    ) {
        return Ok(true);
    }

    if !context.allow_full_inventory_lookup {
        return Ok(false);
    }

    let control_dir = context.repo_root.join(".e2v");
    let (remote_loose_object_ids, remote_pack_locations) =
        ensure_remote_object_inventory_loaded(
            &control_dir,
            context.remote,
            context.remote_inventory_cache,
        )?;
    Ok(
        inventory_has_object(remote_loose_object_ids, remote_pack_locations, object_id)
            && remote_object_authenticates_for_repo(
                context.repo_root,
                context.remote,
                remote_pack_locations,
                context.pack_cache,
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
    pack_locations: &BTreeMap<String, PackedObjectLocation>,
    object_ids: &[String],
) -> Result<()> {
    for object_id in object_ids {
        anyhow::ensure!(
            inventory_has_object(loose_object_ids, pack_locations, object_id),
            "pre-commit verify failed: reachable object missing from remote: {object_id}"
        );
    }
    Ok(())
}

fn verify_remote_reachable_objects_for_resume<R: RemoteBackend>(
    context: &mut ResumeRemoteAuthContext<'_, R>,
    snapshot_id: &str,
    object_ids: &[String],
) -> Result<()> {
    for object_id in object_ids {
        let expected_type = if object_id == snapshot_id {
            "snapshot"
        } else {
            infer_object_type_for_resume_candidate(context.repo_root, object_id)
        };
        anyhow::ensure!(
            remote_object_authenticates_for_resume(context, object_id, expected_type)?,
            "pre-commit verify failed: reachable object missing from remote: {object_id}"
        );
    }
    Ok(())
}

fn next_pack_batch_index<R: RemoteBackend>(
    remote: &R,
    operation_id: &OperationId,
) -> Result<usize> {
    let prefix = format!("packs/index/{}-", operation_id.value);
    Ok(remote
        .list_physical("packs/index/")?
        .into_iter()
        .filter(|path| path.starts_with(&prefix))
        .count())
}

fn upload_object_batch<R, F>(
    remote: &R,
    repo_root: &Path,
    operation_id: &OperationId,
    object_ids: &[String],
    pack_enabled: bool,
    pack_batch_index: &mut usize,
    mut on_uploaded: F,
) -> Result<Option<String>>
where
    R: RemoteBackend,
    F: FnMut(&str) -> Result<()>,
{
    let mut pack_builder = if pack_enabled {
        Some(ObjectPackBuilder::new(
            &operation_id.value,
            *pack_batch_index,
        )?)
    } else {
        None
    };
    let mut packed_object_ids = Vec::new();
    for object_id in object_ids {
        validate_object_id_value(object_id)?;
        let bytes = std::fs::read(local_object_path(repo_root, object_id))?;
        if let Some(builder) = pack_builder.as_mut()
            && bytes.len() <= SMALL_OBJECT_MAX_BYTES
        {
            builder.push_object(object_id.clone(), &bytes);
            packed_object_ids.push(object_id.clone());
            continue;
        }

        remote.put_physical(&format!("objects/{object_id}.json"), &bytes)?;
        on_uploaded(object_id)?;
    }

    if let Some(builder) = pack_builder
        && !packed_object_ids.is_empty()
    {
        let (index, payload) = builder.finish();
        let (_, data_path, index_path) = pack_paths(&operation_id.value, *pack_batch_index)?;
        let secrets = sync_support::open_or_unlock_repo_secrets_for_sync(repo_root.join(".e2v"))?;
        remote.put_physical(&data_path, &payload)?;
        remote.put_physical(
            &index_path,
            &encode_pack_index_segment_bytes_for_sync(
                &secrets,
                &index_path,
                &serde_json::to_vec_pretty(&index)?,
            )?,
        )?;
        *pack_batch_index += 1;
        for object_id in &packed_object_ids {
            on_uploaded(object_id)?;
        }
        return Ok(Some(index_path));
    }

    Ok(None)
}

fn upload_objects_with_policy<R, F>(
    remote: &R,
    repo_root: &Path,
    operation_id: &OperationId,
    object_ids: &[String],
    pack_enabled: bool,
    mut on_uploaded: F,
) -> Result<Vec<String>>
where
    R: RemoteBackend,
    F: FnMut(&str) -> Result<()>,
{
    if object_ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut pack_batch_index = if pack_enabled {
        next_pack_batch_index(remote, operation_id)?
    } else {
        0
    };
    let mut published_pack_index_segments = Vec::new();
    for object_batch in object_ids.chunks(SMALL_OBJECT_PACK_BATCH_SIZE) {
        if let Some(index_path) = upload_object_batch(
            remote,
            repo_root,
            operation_id,
            object_batch,
            pack_enabled,
            &mut pack_batch_index,
            &mut on_uploaded,
        )? {
            published_pack_index_segments.push(index_path);
        }
    }
    Ok(published_pack_index_segments)
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

fn remote_control_plane_matches<R: RemoteBackend>(remote: &R, layout_root_bytes: &[u8]) -> bool {
    remote_physical_matches(remote, "layout_root.json", layout_root_bytes)
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
) -> Result<Vec<u8>> {
    let pointer_file = keyring_files
        .iter()
        .find(|path| path.file_name().and_then(|name| name.to_str()) == Some("keyring.current"))
        .context("missing local keyring pointer file")?;
    let bytes = std::fs::read(pointer_file)?;
    let keyring_pointer: serde_json::Value =
        serde_json::from_slice(&bytes).context("failed to decode local keyring pointer")?;
    let generation = keyring_pointer["generation"]
        .as_u64()
        .context("invalid local keyring pointer generation")?;
    let current = keyring_pointer["current"]
        .as_str()
        .context("invalid local keyring pointer current file")?;
    let current_keyring = keyring_files
        .iter()
        .find(|path| path.file_name().and_then(|name| name.to_str()) == Some(current))
        .context("missing local current keyring generation file")?;
    let repo_id = serde_json::from_slice::<serde_json::Value>(&std::fs::read(current_keyring)?)
        .context("failed to decode local keyring state for pointer ref")?["repo_id"]
        .as_str()
        .context("local keyring state is missing repo_id")?
        .to_string();
    let pointer_token = RefToken::new(format!("keyring/{repo_id}"));
    if let Some(current) = remote.read_ref(&pointer_token)?
        && current.value.bytes == bytes
    {
        ensure!(
            generation >= 1,
            "invalid local keyring pointer generation {generation}"
        );
        return Ok(bytes);
    }
    let expected = remote
        .read_ref(&pointer_token)?
        .map(|stored| stored.version);
    let cas =
        remote.compare_and_swap_ref(&pointer_token, expected, EncryptedRef::new(bytes.clone()))?;
    ensure!(cas.applied, "keyring pointer publish conflict");
    ensure!(
        generation >= 1,
        "invalid local keyring pointer generation {generation}"
    );
    Ok(bytes)
}

fn mirror_remote_keyring_pointer<R: RemoteBackend>(remote: &R, pointer_bytes: &[u8]) -> Result<()> {
    remote.put_physical("control/keyring/keyring.current", pointer_bytes)?;
    Ok(())
}

fn read_remote_current_keyring_bytes<R: RemoteBackend>(
    remote: &R,
    repo_root: &Path,
) -> Result<Option<Vec<u8>>> {
    let repo_id = e2v_core::sync_support::read_repo_id(repo_root)?;
    let pointer_token = RefToken::new(format!("keyring/{repo_id}"));
    let pointer_bytes = match remote.read_ref(&pointer_token)? {
        Some(stored) => stored.value.bytes,
        None => {
            if !remote.exists_physical("control/keyring/keyring.current") {
                return Ok(None);
            }
            remote.get_physical("control/keyring/keyring.current")?
        }
    };
    let pointer: serde_json::Value = serde_json::from_slice(&pointer_bytes)
        .context("failed to decode remote keyring pointer")?;
    let current = pointer["current"]
        .as_str()
        .context("invalid remote keyring pointer current file")?;
    match remote.get_physical(&format!("control/keyring/{current}")) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.to_string().contains("missing physical object") => Ok(None),
        Err(error) => Err(error),
    }
}

fn reconcile_local_keyring_with_remote_if_needed<R: RemoteBackend>(
    remote: &R,
    repo_root: &Path,
) -> Result<()> {
    if let Some(remote_keyring_bytes) = read_remote_current_keyring_bytes(remote, repo_root)? {
        let _ = sync_support::reconcile_remote_keyring_for_sync(repo_root, &remote_keyring_bytes)?;
    }
    Ok(())
}

fn publish_remote_keyring_pointer_with_retry<R: RemoteBackend>(
    remote: &R,
    repo_root: &Path,
) -> Result<Vec<u8>> {
    let mut last_error = None;
    for attempt in 0..2 {
        let keyring_files = sync_support::list_keyring_files(repo_root)?;
        match publish_remote_keyring_pointer(remote, &keyring_files) {
            Ok(pointer_bytes) => return Ok(pointer_bytes),
            Err(error)
                if error
                    .to_string()
                    .contains("keyring pointer publish conflict")
                    && attempt == 0 =>
            {
                reconcile_local_keyring_with_remote_if_needed(remote, repo_root)?;
                last_error = Some(error);
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("keyring pointer publish conflict")))
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
    reconcile_local_keyring_with_remote_if_needed(remote, &options.repo_root)?;
    let (_state, snapshot) = sync_support::export_head_snapshot(facade, &options.repo_root)?;
    let layout_root_bytes = sync_support::read_layout_root_bytes(&options.repo_root)?;
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
                        && remote_control_plane_matches(remote, &layout_root_bytes)
                        && remote_keyring_matches(remote, &keyring_files)
                    {
                        return Ok(PushResult {
                            published_snapshot_id: snapshot.snapshot_id,
                            uploaded_objects: 0,
                        });
                    }
                    if remote_ref_matches_local {
                        upload_remote_keyring_generations(remote, &keyring_files)?;
                        remote.put_physical("layout_root.json", &layout_root_bytes)?;
                        let pointer_bytes =
                            publish_remote_keyring_pointer_with_retry(remote, &options.repo_root)?;
                        mirror_remote_keyring_pointer(remote, &pointer_bytes)?;
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
    let control_dir = options.repo_root.join(".e2v");
    let pack_index_secrets = sync_support::open_or_unlock_repo_secrets_for_sync(&control_dir)?;
    let (mut remote_loose_object_ids, mut remote_pack_locations) =
        load_remote_object_inventory(&control_dir, remote)?;
    let mut remote_pack_cache = BTreeMap::new();
    for ancestor_snapshot_id in &snapshot.ancestor_snapshot_ids {
        if !inventory_has_object(
            &remote_loose_object_ids,
            &remote_pack_locations,
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
                &remote_pack_locations,
                &mut remote_pack_cache,
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
        planned_snapshot_id: Some(snapshot.snapshot_id.clone()),
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
    let pack_enabled = should_pack_small_objects(reachable_object_ids.len());
    let missing_object_ids = reachable_object_ids
        .iter()
        .filter(|object_id| {
            !inventory_has_object(&remote_loose_object_ids, &remote_pack_locations, object_id)
        })
        .cloned()
        .collect::<Vec<_>>();
    for object_id in &reachable_object_ids {
        journal.plan_object(&operation_id, object_id, "object")?;
    }
    let published_pack_index_segments = upload_objects_with_policy(
        remote,
        &options.repo_root,
        &operation_id,
        &missing_object_ids,
        pack_enabled,
        |object_id| {
            publisher.record_uploaded(
                &session,
                PublishedObject {
                    object_id: object_id.to_string(),
                    object_type: "object".to_string(),
                },
            )?;
            publisher.heartbeat(&session)?;
            journal.record_verified(&operation_id, object_id, "object")
        },
    )?;

    upload_remote_keyring_generations(remote, &keyring_files)?;
    publisher.publish_layout_if_needed(&session)?;
    if !published_pack_index_segments.is_empty() {
        let segment_paths = next_pack_index_segment_paths(
            remote,
            &published_pack_index_segments,
            Some(&pack_index_secrets),
        )?;
        publish_pack_index_root(
            remote,
            &pack_index_secrets,
            &layout_root.layout_id,
            layout_root.generation,
            segment_paths,
        )?;
    }
    publisher.pre_commit_verify(&session)?;

    (remote_loose_object_ids, remote_pack_locations) =
        load_remote_object_inventory(&control_dir, remote)?;
    verify_remote_reachable_objects(
        &remote_loose_object_ids,
        &remote_pack_locations,
        &reachable_object_ids,
    )?;
    let pointer_bytes = publish_remote_keyring_pointer_with_retry(remote, &options.repo_root)?;
    let publish_result =
        publisher.publish_ref(&session, EncryptedRef::new(default_ref_bytes.clone()))?;
    if !publish_result.applied {
        anyhow::bail!("push requires needs-rebase recovery");
    }
    mirror_remote_keyring_pointer(remote, &pointer_bytes)?;
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
    reconcile_local_keyring_with_remote_if_needed(remote, &options.repo_root)?;
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
    let pack_enabled = should_pack_small_objects(total_tracked_objects);
    let skipped_uploaded_objects = journal.count_objects_in_states(
        &operation_id,
        &[
            crate::journal::ObjectUploadState::Uploaded,
            crate::journal::ObjectUploadState::Verified,
        ],
    )?;
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
    let control_dir = options.repo_root.join(".e2v");
    let pack_index_secrets = sync_support::open_or_unlock_repo_secrets_for_sync(&control_dir)?;
    let mut remote_pack_locations =
        load_remote_resume_pack_locations(&control_dir, remote, &operation_id)?;
    let mut remote_inventory = None;
    let mut remote_pack_cache = BTreeMap::new();
    let mut resume_remote_auth = ResumeRemoteAuthContext {
        repo_root: &options.repo_root,
        remote,
        current_operation_pack_locations: &remote_pack_locations,
        allow_full_inventory_lookup: false,
        remote_inventory_cache: &mut remote_inventory,
        pack_cache: &mut remote_pack_cache,
    };
    let mut published_pack_index_segments = Vec::new();
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
                &mut resume_remote_auth,
                &record.object_id,
                infer_object_type_for_resume_candidate(&options.repo_root, &record.object_id),
            )? {
                journal.record_verified(&operation_id, &record.object_id, "object")?;
                continue;
            }
            missing_object_ids.push(record.object_id.clone());
        }
        published_pack_index_segments.extend(upload_objects_with_policy(
            remote,
            &options.repo_root,
            &operation_id,
            &missing_object_ids,
            pack_enabled,
            |object_id| journal.record_verified(&operation_id, object_id, "object"),
        )?);
        remote_pack_locations =
            load_remote_resume_pack_locations(&control_dir, remote, &operation_id)?;
        remote_inventory = None;
        remote_pack_cache.clear();
        resume_remote_auth = ResumeRemoteAuthContext {
            repo_root: &options.repo_root,
            remote,
            current_operation_pack_locations: &remote_pack_locations,
            allow_full_inventory_lookup: false,
            remote_inventory_cache: &mut remote_inventory,
            pack_cache: &mut remote_pack_cache,
        };
        pending_cursor = batch.next_cursor;
    }

    if !saw_journal_objects {
        let (remote_loose_object_ids, remote_pack_locations) =
            load_remote_object_inventory(&control_dir, remote)?;
        let manifest_store = ManifestStore::new(&options.repo_root);
        let reachable_object_ids =
            manifest_store.collect_reachable_object_ids(&snapshot.snapshot_id)?;
        let remote_is_already_complete = remote_ref_matches_local
            && inventory_has_object(
                &remote_loose_object_ids,
                &remote_pack_locations,
                &snapshot.snapshot_id,
            )
            && remote_snapshot_graph_authenticates_for_repo(
                &options.repo_root,
                remote,
                &remote_loose_object_ids,
                &remote_pack_locations,
                &mut remote_pack_cache,
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
                if inventory_has_object(&remote_loose_object_ids, &remote_pack_locations, object_id)
                    && remote_object_authenticates_for_repo(
                        &options.repo_root,
                        remote,
                        &remote_pack_locations,
                        &mut remote_pack_cache,
                        object_id,
                        object_type,
                    )
                {
                    continue;
                }
                missing_object_ids.push(object_id.clone());
            }
            published_pack_index_segments.extend(upload_objects_with_policy(
                remote,
                &options.repo_root,
                &operation_id,
                &missing_object_ids,
                reachable_object_ids.len() >= small_object_pack_threshold(),
                |object_id| journal.record_verified(&operation_id, object_id, "object"),
            )?);
        }
    }

    if remote_ref_matches_local
        && remote_control_plane_matches(remote, &layout_root_bytes)
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
        planned_snapshot_id: Some(snapshot.snapshot_id.clone()),
        writer_mode: remote.capability().push_write_mode(),
    })?;
    let session = crate::transaction::PublishSession {
        next_layout_root: Some(layout_root.clone()),
        next_layout_root_bytes: Some(layout_root_bytes.clone()),
        ..session
    };
    if remote_ref_matches_local {
        upload_remote_keyring_generations(remote, &keyring_files)?;
        let pointer_bytes = publish_remote_keyring_pointer_with_retry(remote, &options.repo_root)?;
        mirror_remote_keyring_pointer(remote, &pointer_bytes)?;
        publisher.publish_layout_if_needed(&session)?;
        if !published_pack_index_segments.is_empty() {
            let segment_paths = next_pack_index_segment_paths(
                remote,
                &published_pack_index_segments,
                Some(&pack_index_secrets),
            )?;
            publish_pack_index_root(
                remote,
                &pack_index_secrets,
                &layout_root.layout_id,
                layout_root.generation,
                segment_paths,
            )?;
        }
        publisher.complete(session)?;
        return Ok(ResumeResult {
            published_snapshot_id: snapshot.snapshot_id,
            skipped_uploaded_objects,
        });
    }
    upload_remote_keyring_generations(remote, &keyring_files)?;
    publisher.publish_layout_if_needed(&session)?;
    if !published_pack_index_segments.is_empty() {
        let segment_paths = next_pack_index_segment_paths(
            remote,
            &published_pack_index_segments,
            Some(&pack_index_secrets),
        )?;
        publish_pack_index_root(
            remote,
            &pack_index_secrets,
            &layout_root.layout_id,
            layout_root.generation,
            segment_paths,
        )?;
    }
    publisher.pre_commit_verify(&session)?;
    let manifest_store = ManifestStore::new(&options.repo_root);
    let reachable_object_ids =
        manifest_store.collect_reachable_object_ids(&snapshot.snapshot_id)?;
    let mut precommit_remote_auth = ResumeRemoteAuthContext {
        repo_root: &options.repo_root,
        remote,
        current_operation_pack_locations: &remote_pack_locations,
        allow_full_inventory_lookup: false,
        remote_inventory_cache: &mut remote_inventory,
        pack_cache: &mut remote_pack_cache,
    };
    verify_remote_reachable_objects_for_resume(
        &mut precommit_remote_auth,
        &snapshot.snapshot_id,
        &reachable_object_ids,
    )?;
    let pointer_bytes = publish_remote_keyring_pointer_with_retry(remote, &options.repo_root)?;
    let publish_result =
        publisher.publish_ref(&session, EncryptedRef::new(default_ref_bytes.clone()))?;
    if !publish_result.applied {
        anyhow::bail!("push requires needs-rebase recovery");
    }
    mirror_remote_keyring_pointer(remote, &pointer_bytes)?;
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
        PackedObjectLocation, remote_object_authenticates_for_repo,
        remote_snapshot_graph_authenticates_for_repo, should_pack_small_objects,
        upload_object_batch,
    };
    use crate::journal::OperationId;

    #[test]
    fn default_small_object_pack_threshold_starts_at_100k() {
        assert!(!should_pack_small_objects(99_999));
        assert!(should_pack_small_objects(100_000));
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
            &std::collections::BTreeMap::<String, PackedObjectLocation>::new(),
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
            &std::collections::BTreeMap::<String, PackedObjectLocation>::new(),
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
            &std::collections::BTreeMap::<String, PackedObjectLocation>::new(),
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
        let mut pack_batch_index = 0usize;

        let error = upload_object_batch(
            &remote,
            &repo_root,
            &operation_id,
            &["..\\..\\..\\outside".to_string()],
            false,
            &mut pack_batch_index,
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
