use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};

use e2v_core::{ManifestStore, ManifestStoreApi, RepositoryFacade, sync_support};
use e2v_store::{
    EncryptedRef, LayoutMode, LayoutRoot, RefToken, RemoteBackend,
    is_missing_physical_object_error, validate_object_id_value,
};

use crate::fetch::validate_remote_relative_name;
use crate::journal::{OperationId, OperationJournal, OperationMetadata, validate_sync_identifier};
use crate::object_type::infer_object_type_from_hint;
use crate::oram::load_remote_active_pack_locations_with_local_cache;
use crate::pack::{ObjectPackBuilder, PackedObjectLocation, pack_paths};
use crate::pack_index::{
    encode_pack_index_segment_bytes_for_sync, load_remote_operation_pack_locations_with_secrets,
    next_pack_index_segment_paths, publish_pack_index_root,
};
use crate::publisher::{SimpleTransactionPublisher, TransactionPublisher};
use crate::remote_maintenance::load_remote_loose_object_ids;
use crate::remote_markers::{RemoteWriteIntentMarker, RemoteWriterLeaseMarker};
use crate::transaction::{PublishPlan, PublishedObject};

const KEYRING_LOCK_FILE: &str = "keyring.lock";

#[derive(Debug, serde::Deserialize)]
struct RemoteKeyringPointerSummary {
    generation: u64,
    current: String,
}

#[derive(Debug, serde::Deserialize)]
struct RemoteKeyringStateSummary {
    generation: u64,
}

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
const DEFAULT_SMALL_PUSH_PACK_THRESHOLD: usize = 2;
thread_local! {
    static SMALL_OBJECT_PACK_THRESHOLD_OVERRIDE: Cell<Option<usize>> = const { Cell::new(None) };
    static SMALL_PUSH_PACK_THRESHOLD_OVERRIDE: Cell<Option<usize>> = const { Cell::new(None) };
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

pub(crate) fn override_small_object_pack_threshold_for_test(
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

pub fn small_push_pack_threshold() -> usize {
    SMALL_PUSH_PACK_THRESHOLD_OVERRIDE.with(|override_cell| {
        override_cell
            .get()
            .unwrap_or(DEFAULT_SMALL_PUSH_PACK_THRESHOLD)
    })
}

pub(crate) fn override_small_push_pack_threshold_for_test(
    threshold: usize,
) -> SmallPushPackThresholdGuard {
    let previous = SMALL_PUSH_PACK_THRESHOLD_OVERRIDE.with(|override_cell| {
        let previous = override_cell.get();
        override_cell.set(Some(threshold));
        previous
    });
    SmallPushPackThresholdGuard { previous }
}

pub struct SmallPushPackThresholdGuard {
    previous: Option<usize>,
}

impl Drop for SmallPushPackThresholdGuard {
    fn drop(&mut self) {
        SMALL_PUSH_PACK_THRESHOLD_OVERRIDE.with(|override_cell| {
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

fn should_pack_current_upload_set(
    repo_root: &Path,
    missing_object_ids: &[String],
    total_object_count_hint: usize,
) -> Result<bool> {
    if should_pack_small_objects(total_object_count_hint) {
        return Ok(true);
    }

    let mut pack_eligible_count = 0usize;
    for object_id in missing_object_ids {
        validate_object_id_value(object_id)?;
        let bytes = std::fs::read(local_object_path(repo_root, object_id))?;
        if bytes.len() <= SMALL_OBJECT_MAX_BYTES {
            pack_eligible_count += 1;
            if pack_eligible_count >= small_push_pack_threshold() {
                return Ok(true);
            }
        }
    }

    Ok(false)
}

fn inventory_has_object(
    loose_object_ids: &BTreeSet<String>,
    pack_locations: &BTreeMap<String, PackedObjectLocation>,
    object_id: &str,
) -> bool {
    loose_object_ids.contains(object_id) || pack_locations.contains_key(object_id)
}

fn load_remote_object_inventory<R: RemoteBackend>(
    control_dir: &Path,
    remote: &R,
) -> Result<(BTreeSet<String>, BTreeMap<String, PackedObjectLocation>)> {
    let secrets = sync_support::open_or_unlock_repo_secrets_for_sync(control_dir)?;
    let pack_locations =
        load_remote_active_pack_locations_with_local_cache(remote, control_dir, &secrets)?;
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
    if let Some(location) = pack_locations.get(object_id) {
        let physical_ref = match location.physical_ref() {
            Ok(physical_ref) => physical_ref,
            Err(_) => return false,
        };
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

    if let Ok(bytes) = remote.get_physical(&format!("objects/{object_id}.json")) {
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
    match inventory {
        Some(inventory) => Ok(inventory),
        None => Ok(inventory.insert(load_remote_object_inventory(control_dir, remote)?)),
    }
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
    let (remote_loose_object_ids, remote_pack_locations) = ensure_remote_object_inventory_loaded(
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
                &serde_json::to_vec(&index)?,
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

pub(crate) fn upload_objects_with_policy<R, F>(
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

fn use_segment_only_uploads(layout_root: &LayoutRoot) -> bool {
    matches!(layout_root.mode, LayoutMode::Oblivious)
}

pub(crate) fn upload_objects_as_pack_segments<R, F>(
    remote: &R,
    repo_root: &Path,
    operation_id: &OperationId,
    object_ids: &[String],
    mut on_uploaded: F,
) -> Result<Vec<String>>
where
    R: RemoteBackend,
    F: FnMut(&str) -> Result<()>,
{
    if object_ids.is_empty() {
        return Ok(Vec::new());
    }

    let control_dir = repo_root.join(".e2v");
    let secrets = sync_support::open_or_unlock_repo_secrets_for_sync(&control_dir)?;
    let mut pack_batch_index = next_pack_batch_index(remote, operation_id)?;
    let mut published_pack_index_segments = Vec::new();
    for object_batch in object_ids.chunks(SMALL_OBJECT_PACK_BATCH_SIZE) {
        let mut pack_builder = ObjectPackBuilder::new(&operation_id.value, pack_batch_index)?;
        for object_id in object_batch {
            validate_object_id_value(object_id)?;
            let bytes = std::fs::read(local_object_path(repo_root, object_id))?;
            pack_builder.push_object(object_id.clone(), &bytes);
        }
        let (index, payload) = pack_builder.finish();
        let (_, data_path, index_path) = pack_paths(&operation_id.value, pack_batch_index)?;
        remote.put_physical(&data_path, &payload)?;
        remote.put_physical(
            &index_path,
            &encode_pack_index_segment_bytes_for_sync(
                &secrets,
                &index_path,
                &serde_json::to_vec(&index)?,
            )?,
        )?;
        pack_batch_index += 1;
        for object_id in object_batch {
            on_uploaded(object_id)?;
        }
        published_pack_index_segments.push(index_path);
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

pub(crate) fn remote_control_plane_matches<R: RemoteBackend>(
    remote: &R,
    layout_root_bytes: &[u8],
) -> bool {
    remote_physical_matches(remote, "layout_root.json", layout_root_bytes)
}

pub(crate) fn remote_keyring_matches<R: RemoteBackend>(
    remote: &R,
    keyring_files: &[PathBuf],
) -> bool {
    let pointer_file = match keyring_files
        .iter()
        .find(|path| path.file_name().and_then(|name| name.to_str()) == Some("keyring.current"))
    {
        Some(path) => path,
        None => return false,
    };
    let pointer_bytes = match std::fs::read(pointer_file) {
        Ok(bytes) => bytes,
        Err(_) => return false,
    };
    let pointer: serde_json::Value = match serde_json::from_slice(&pointer_bytes) {
        Ok(pointer) => pointer,
        Err(_) => return false,
    };
    let current = match pointer["current"].as_str() {
        Some(current) => current,
        None => return false,
    };
    let current_keyring = match keyring_files
        .iter()
        .find(|path| path.file_name().and_then(|name| name.to_str()) == Some(current))
    {
        Some(path) => path,
        None => return false,
    };
    let repo_id =
        match serde_json::from_slice::<serde_json::Value>(&match std::fs::read(current_keyring) {
            Ok(bytes) => bytes,
            Err(_) => return false,
        }) {
            Ok(keyring) => match keyring["repo_id"].as_str() {
                Some(repo_id) => repo_id.to_string(),
                None => return false,
            },
            Err(_) => return false,
        };
    let pointer_token = RefToken::new(format!("keyring/{repo_id}"));
    let remote_pointer = match remote.read_ref(&pointer_token) {
        Ok(Some(stored)) => stored.value.bytes,
        _ => return false,
    };
    if remote_pointer != pointer_bytes {
        return false;
    }

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

fn ensure_remote_root_matches_local_repository<R: RemoteBackend>(
    remote: &R,
    repo_root: &Path,
) -> Result<()> {
    let local_repo_id = sync_support::read_repo_id(repo_root)?;
    let expected_keyring_token = format!("keyring/{local_repo_id}");
    let foreign_keyring_tokens = remote
        .list_refs()?
        .into_iter()
        .filter_map(|listed| {
            let token = listed.token.value;
            if token.starts_with("keyring/") && token != expected_keyring_token {
                Some(token)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    ensure!(
        foreign_keyring_tokens.is_empty(),
        "remote root already contains keyring refs for a different repository"
    );
    Ok(())
}

pub(crate) fn upload_remote_keyring_generations<R: RemoteBackend>(
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
        let relative_path = format!("control/keyring/{file_name}");
        if remote_physical_matches(remote, &relative_path, &bytes) {
            continue;
        }
        remote.put_physical(&relative_path, &bytes)?;
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
    let current_keyring_generation =
        serde_json::from_slice::<RemoteKeyringStateSummary>(&std::fs::read(current_keyring)?)
            .context("failed to decode local current keyring generation state")?
            .generation;
    ensure!(
        current_keyring_generation == generation,
        "local keyring pointer generation mismatch"
    );
    let pointer_token = RefToken::new(format!("keyring/{repo_id}"));
    let current_pointer = remote.read_ref(&pointer_token)?;
    if let Some(current) = &current_pointer
        && current.value.bytes == bytes
    {
        ensure!(
            generation >= 1,
            "invalid local keyring pointer generation {generation}"
        );
        return Ok(bytes);
    }
    let expected = current_pointer.map(|stored| stored.version);
    let cas =
        remote.compare_and_swap_ref(&pointer_token, expected, EncryptedRef::new(bytes.clone()))?;
    ensure!(cas.applied, "keyring pointer publish conflict");
    ensure!(
        generation >= 1,
        "invalid local keyring pointer generation {generation}"
    );
    Ok(bytes)
}

pub(crate) fn mirror_remote_keyring_pointer<R: RemoteBackend>(
    remote: &R,
    pointer_bytes: &[u8],
) -> Result<()> {
    if remote
        .get_physical("control/keyring/keyring.current")
        .map(|bytes| bytes == pointer_bytes)
        .unwrap_or(false)
    {
        return Ok(());
    }
    remote.put_physical("control/keyring/keyring.current", pointer_bytes)?;
    Ok(())
}

pub(crate) fn read_remote_current_keyring_bytes<R: RemoteBackend>(
    remote: &R,
    repo_root: &Path,
) -> Result<Option<Vec<u8>>> {
    let repo_id = e2v_core::sync_support::read_repo_id(repo_root)?;
    let pointer_token = RefToken::new(format!("keyring/{repo_id}"));
    let pointer_bytes = match remote.read_ref(&pointer_token)? {
        Some(stored) => stored.value.bytes,
        None => return Ok(None),
    };
    let pointer: RemoteKeyringPointerSummary = serde_json::from_slice(&pointer_bytes)
        .context("failed to decode remote keyring pointer")?;
    let current = pointer.current.as_str();
    validate_remote_relative_name(current)
        .map_err(|error| anyhow::anyhow!("invalid remote keyring path {current}: {error}"))?;
    match remote.get_physical(&format!("control/keyring/{current}")) {
        Ok(bytes) => {
            let keyring_state: RemoteKeyringStateSummary = serde_json::from_slice(&bytes)
                .context("failed to decode remote current keyring state")?;
            anyhow::ensure!(
                keyring_state.generation == pointer.generation,
                "remote keyring pointer generation mismatch"
            );
            Ok(Some(bytes))
        }
        Err(error) if is_missing_physical_object_error(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

pub(crate) fn reconcile_local_keyring_with_remote_if_needed<R: RemoteBackend>(
    remote: &R,
    repo_root: &Path,
) -> Result<()> {
    if let Some(remote_keyring_bytes) = read_remote_current_keyring_bytes(remote, repo_root)? {
        let _ = sync_support::reconcile_remote_keyring_for_sync(repo_root, &remote_keyring_bytes)?;
    }
    Ok(())
}

pub(crate) fn publish_remote_keyring_pointer_with_retry<R: RemoteBackend>(
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

pub(crate) fn cleanup_completed_operation_markers<R: RemoteBackend>(
    remote: &R,
    operation_id: &OperationId,
    branch_token: &str,
) -> Result<()> {
    let intent_path = format!("transactions/active/{}.intent", operation_id.value);
    let had_current_intent = remote.exists_physical(&intent_path);
    match remote.delete_physical(&intent_path) {
        Ok(()) => {}
        Err(error) if is_missing_physical_object_error(&error) => {}
        Err(error) => return Err(error),
    }

    let lease_path = format!("leases/{branch_token}.lock");
    match remote.get_physical(&lease_path) {
        Ok(lease_bytes) => {
            let should_remove =
                match serde_json::from_slice::<RemoteWriterLeaseMarker>(&lease_bytes) {
                    Ok(lease_marker) => {
                        lease_marker.operation_id == operation_id.value
                            && lease_marker.target_branch_token == branch_token
                    }
                    Err(_) if had_current_intent => !other_active_intent_may_own_branch_lease(
                        remote,
                        &intent_path,
                        branch_token,
                    )?,
                    Err(_) => false,
                };
            if should_remove {
                match remote.delete_physical(&lease_path) {
                    Ok(()) => {}
                    Err(error) if is_missing_physical_object_error(&error) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        Err(error) if is_missing_physical_object_error(&error) => {}
        Err(error) => return Err(error),
    }

    Ok(())
}

fn other_active_intent_may_own_branch_lease<R: RemoteBackend>(
    remote: &R,
    current_intent_path: &str,
    branch_token: &str,
) -> Result<bool> {
    for intent_path in remote.list_physical("transactions/active/")? {
        if intent_path == current_intent_path {
            continue;
        }
        let intent_bytes = match remote.get_physical(&intent_path) {
            Ok(bytes) => bytes,
            Err(error) if is_missing_physical_object_error(&error) => continue,
            Err(error) => return Err(error),
        };
        match serde_json::from_slice::<RemoteWriteIntentMarker>(&intent_bytes) {
            Ok(intent_marker) => {
                if intent_marker.target_branch_token == branch_token {
                    return Ok(true);
                }
            }
            Err(_) => return Ok(true),
        }
    }
    Ok(false)
}

pub(crate) fn list_operation_pack_index_segment_paths<R: RemoteBackend>(
    remote: &R,
    operation_id: &OperationId,
) -> Result<Vec<String>> {
    let prefix = format!("packs/index/{}-", operation_id.value);
    let mut paths = remote
        .list_physical("packs/index/")?
        .into_iter()
        .filter(|path| path.starts_with(&prefix))
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
}

pub fn push_head<R: RemoteBackend + Clone>(
    facade: &RepositoryFacade,
    remote: &R,
    options: PushOptions,
) -> Result<PushResult> {
    push_head_inner(facade, remote, options, false)
}

pub fn push_head_with_single_writer_risk<R: RemoteBackend + Clone>(
    facade: &RepositoryFacade,
    remote: &R,
    options: PushOptions,
) -> Result<PushResult> {
    push_head_inner(facade, remote, options, true)
}

fn push_head_inner<R: RemoteBackend + Clone>(
    facade: &RepositoryFacade,
    remote: &R,
    options: PushOptions,
    allow_risky_single_writer: bool,
) -> Result<PushResult> {
    validate_sync_identifier("branch token", &options.branch_token)?;
    validate_sync_identifier("operation id", &options.operation_id)?;
    ensure_remote_root_matches_local_repository(remote, &options.repo_root)?;
    reconcile_local_keyring_with_remote_if_needed(remote, &options.repo_root)?;
    let current_state = facade.open(&options.repo_root)?;
    ensure!(
        current_state.branch.token_hex == options.branch_token,
        "current checked out branch does not match requested branch token"
    );
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
                        if !remote_control_plane_matches(remote, &layout_root_bytes) {
                            remote.put_physical("layout_root.json", &layout_root_bytes)?;
                        }
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
    let session = publisher.begin_allowing_risky_single_writer(
        PublishPlan {
            operation_id: operation_id.clone(),
            target_branch_token: options.branch_token.clone(),
            expected_ref_version,
            planned_snapshot_id: Some(snapshot.snapshot_id.clone()),
            writer_mode: remote.capability().push_write_mode(),
        },
        allow_risky_single_writer,
    )?;
    let session = crate::transaction::PublishSession {
        next_layout_root: Some(layout_root.clone()),
        next_layout_root_bytes: Some(layout_root_bytes.clone()),
        ..session
    };

    let manifest_store = ManifestStore::new(&options.repo_root);
    let reachable_object_ids =
        manifest_store.collect_reachable_object_ids(&snapshot.snapshot_id)?;
    let segment_only_uploads = use_segment_only_uploads(&layout_root);
    let missing_object_ids = reachable_object_ids
        .iter()
        .filter(|object_id| {
            !inventory_has_object(&remote_loose_object_ids, &remote_pack_locations, object_id)
        })
        .cloned()
        .collect::<Vec<_>>();
    let pack_enabled = should_pack_current_upload_set(
        &options.repo_root,
        &missing_object_ids,
        reachable_object_ids.len(),
    )?;
    for object_id in &reachable_object_ids {
        journal.plan_object(&operation_id, object_id, "object")?;
    }
    let published_pack_index_segments = if segment_only_uploads {
        upload_objects_as_pack_segments(
            remote,
            &options.repo_root,
            &operation_id,
            &missing_object_ids,
            |object_id| {
                publisher.record_uploaded(
                    &session,
                    PublishedObject {
                        object_id: object_id.to_string(),
                        object_type: "object".to_string(),
                    },
                )?;
                publisher.heartbeat_if_needed(&session)?;
                journal.record_verified(&operation_id, object_id, "object")
            },
        )?
    } else {
        upload_objects_with_policy(
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
                publisher.heartbeat_if_needed(&session)?;
                journal.record_verified(&operation_id, object_id, "object")
            },
        )?
    };

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

    if !missing_object_ids.is_empty() {
        let current_operation_pack_locations = if pack_enabled {
            load_remote_operation_pack_locations_with_secrets(
                remote,
                &operation_id.value,
                &pack_index_secrets,
            )?
        } else {
            BTreeMap::new()
        };
        for object_id in &missing_object_ids {
            if !current_operation_pack_locations.contains_key(object_id) {
                remote_loose_object_ids.insert(object_id.clone());
            }
        }
        remote_pack_locations.extend(current_operation_pack_locations);
    }
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
    ensure_remote_root_matches_local_repository(remote, &options.repo_root)?;
    reconcile_local_keyring_with_remote_if_needed(remote, &options.repo_root)?;
    let current_state = facade.open(&options.repo_root)?;
    ensure!(
        current_state.branch.token_hex == options.branch_token,
        "current checked out branch does not match requested branch token"
    );
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
    let segment_only_uploads = use_segment_only_uploads(&layout_root);
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
        let pack_enabled = should_pack_current_upload_set(
            &options.repo_root,
            &missing_object_ids,
            total_tracked_objects,
        )?;
        if segment_only_uploads {
            published_pack_index_segments.extend(upload_objects_as_pack_segments(
                remote,
                &options.repo_root,
                &operation_id,
                &missing_object_ids,
                |object_id| journal.record_verified(&operation_id, object_id, "object"),
            )?);
        } else {
            published_pack_index_segments.extend(upload_objects_with_policy(
                remote,
                &options.repo_root,
                &operation_id,
                &missing_object_ids,
                pack_enabled,
                |object_id| journal.record_verified(&operation_id, object_id, "object"),
            )?);
        }
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
            if segment_only_uploads {
                published_pack_index_segments.extend(upload_objects_as_pack_segments(
                    remote,
                    &options.repo_root,
                    &operation_id,
                    &missing_object_ids,
                    |object_id| journal.record_verified(&operation_id, object_id, "object"),
                )?);
            } else {
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
    use std::sync::{Arc, Mutex};

    use e2v_core::{CommitOptions, InitOptions, RepositoryFacade};
    use e2v_store::{
        BackendCapability, BlobStore, CasResult, ConsistencyClass, EncryptedRef, LayoutRoot,
        LayoutRootStore, ListedRef, LocalFolderBackend, MemoryBackend, RefStore, RefToken,
        RefVersion, RemoteBackend, StoredRef,
    };
    use tempfile::tempdir;

    use super::{
        PackedObjectLocation, PushOptions, cleanup_completed_operation_markers, local_object_path,
        override_small_object_pack_threshold_for_test, override_small_push_pack_threshold_for_test,
        publish_remote_keyring_pointer, push_head, read_remote_current_keyring_bytes,
        remote_object_authenticates_for_repo, remote_snapshot_graph_authenticates_for_repo,
        should_pack_current_upload_set, should_pack_small_objects, upload_object_batch,
    };
    use crate::journal::OperationId;

    #[derive(Debug, Clone)]
    struct KeyringPointerReadCountingBackend {
        inner: MemoryBackend,
        capability: BackendCapability,
        keyring_pointer_ref_reads: Arc<Mutex<usize>>,
    }

    impl KeyringPointerReadCountingBackend {
        fn new() -> Self {
            Self {
                inner: MemoryBackend::new(),
                capability: BackendCapability {
                    supports_conditional_put: true,
                    supports_range_read: true,
                    supports_atomic_rename: true,
                    supports_paged_list: true,
                    consistency_class: ConsistencyClass::StrongWhitelisted,
                    supports_remote_lock_or_lease: true,
                    supports_atomic_create_if_absent: true,
                    supports_transaction_markers: true,
                    supports_reliable_remote_time: true,
                    supports_object_generation_or_etag: true,
                    supports_layout_root_cas: true,
                    supports_oblivious_access_schedule: false,
                },
                keyring_pointer_ref_reads: Arc::new(Mutex::new(0)),
            }
        }

        fn keyring_pointer_ref_read_count(&self) -> usize {
            *self.keyring_pointer_ref_reads.lock().unwrap()
        }
    }

    impl BlobStore for KeyringPointerReadCountingBackend {
        fn put_physical(&self, relative_path: &str, bytes: &[u8]) -> anyhow::Result<()> {
            self.inner.put_physical(relative_path, bytes)
        }

        fn put_physical_if_absent(
            &self,
            relative_path: &str,
            bytes: &[u8],
        ) -> anyhow::Result<bool> {
            self.inner.put_physical_if_absent(relative_path, bytes)
        }

        fn get_physical(&self, relative_path: &str) -> anyhow::Result<Vec<u8>> {
            self.inner.get_physical(relative_path)
        }

        fn get_physical_range(
            &self,
            relative_path: &str,
            offset: usize,
            length: usize,
        ) -> anyhow::Result<Vec<u8>> {
            self.inner.get_physical_range(relative_path, offset, length)
        }

        fn delete_physical(&self, relative_path: &str) -> anyhow::Result<()> {
            self.inner.delete_physical(relative_path)
        }

        fn exists_physical(&self, relative_path: &str) -> bool {
            self.inner.exists_physical(relative_path)
        }

        fn stat_physical(&self, relative_path: &str) -> anyhow::Result<e2v_store::ObjectStat> {
            self.inner.stat_physical(relative_path)
        }

        fn list_physical(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
            self.inner.list_physical(prefix)
        }
    }

    impl RefStore for KeyringPointerReadCountingBackend {
        fn read_ref(&self, token: &RefToken) -> anyhow::Result<Option<StoredRef>> {
            if token.value.starts_with("keyring/") {
                *self.keyring_pointer_ref_reads.lock().unwrap() += 1;
            }
            self.inner.read_ref(token)
        }

        fn list_refs(&self) -> anyhow::Result<Vec<ListedRef>> {
            self.inner.list_refs()
        }

        fn compare_and_swap_ref(
            &self,
            token: &RefToken,
            expected: Option<RefVersion>,
            next: EncryptedRef,
        ) -> anyhow::Result<CasResult> {
            self.inner.compare_and_swap_ref(token, expected, next)
        }
    }

    impl LayoutRootStore for KeyringPointerReadCountingBackend {
        fn read_layout_root(&self) -> anyhow::Result<LayoutRoot> {
            self.inner.read_layout_root()
        }

        fn compare_and_swap_layout_root(
            &self,
            expected: u64,
            next: LayoutRoot,
        ) -> anyhow::Result<CasResult> {
            self.inner.compare_and_swap_layout_root(expected, next)
        }

        fn list_retained_layout_roots(&self) -> anyhow::Result<Vec<LayoutRoot>> {
            self.inner.list_retained_layout_roots()
        }
    }

    impl RemoteBackend for KeyringPointerReadCountingBackend {
        fn capability(&self) -> &BackendCapability {
            &self.capability
        }
    }

    #[test]
    fn default_small_object_pack_threshold_starts_at_100k() {
        assert!(!should_pack_small_objects(99_999));
        assert!(should_pack_small_objects(100_000));
    }

    #[test]
    fn current_upload_set_enables_packing_for_two_small_missing_objects() {
        let _large_guard = override_small_object_pack_threshold_for_test(usize::MAX);
        let _small_guard = override_small_push_pack_threshold_for_test(2);
        let temp = tempdir().unwrap();
        let repo_root = temp.path();
        let objects_dir = repo_root.join(".e2v").join("objects");
        std::fs::create_dir_all(&objects_dir).unwrap();

        let first = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
        let second = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string();
        std::fs::write(local_object_path(repo_root, &first), vec![1u8; 32]).unwrap();
        std::fs::write(local_object_path(repo_root, &second), vec![2u8; 32]).unwrap();

        assert!(should_pack_current_upload_set(repo_root, &[first, second], 2).unwrap());
    }

    #[test]
    fn current_upload_set_keeps_single_small_missing_object_loose_by_default() {
        let _large_guard = override_small_object_pack_threshold_for_test(usize::MAX);
        let _small_guard = override_small_push_pack_threshold_for_test(2);
        let temp = tempdir().unwrap();
        let repo_root = temp.path();
        let objects_dir = repo_root.join(".e2v").join("objects");
        std::fs::create_dir_all(&objects_dir).unwrap();

        let object_id =
            "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc".to_string();
        std::fs::write(local_object_path(repo_root, &object_id), vec![3u8; 32]).unwrap();

        assert!(!should_pack_current_upload_set(repo_root, &[object_id], 1).unwrap());
    }

    #[test]
    fn large_repository_threshold_still_enables_packing_with_one_missing_object() {
        let _large_guard = override_small_object_pack_threshold_for_test(3);
        let _small_guard = override_small_push_pack_threshold_for_test(usize::MAX);
        let temp = tempdir().unwrap();
        let repo_root = temp.path();
        let objects_dir = repo_root.join(".e2v").join("objects");
        std::fs::create_dir_all(&objects_dir).unwrap();

        let object_id =
            "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd".to_string();
        std::fs::write(local_object_path(repo_root, &object_id), vec![4u8; 32]).unwrap();

        assert!(should_pack_current_upload_set(repo_root, &[object_id], 3).unwrap());
    }

    #[test]
    fn cleanup_completed_operation_markers_removes_corrupted_current_lease_marker() {
        let remote = MemoryBackend::new();
        let operation_id = OperationId::new("cleanup-corrupted-lease".to_string()).unwrap();
        remote
            .put_physical(
                &format!("transactions/active/{}.intent", operation_id.value),
                br#"{"stale":true}"#,
            )
            .unwrap();
        remote
            .put_physical("leases/branch-token.lock", br#"{"broken":true"#)
            .unwrap();

        cleanup_completed_operation_markers(&remote, &operation_id, "branch-token").unwrap();

        assert!(!remote.exists_physical("leases/branch-token.lock"));
        assert!(!remote.exists_physical(&format!(
            "transactions/active/{}.intent",
            operation_id.value
        )));
    }

    #[test]
    fn cleanup_completed_operation_markers_keeps_corrupted_lease_without_current_intent() {
        let remote = MemoryBackend::new();
        let operation_id =
            OperationId::new("cleanup-corrupted-lease-no-intent".to_string()).unwrap();
        remote
            .put_physical("leases/branch-token.lock", br#"{"broken":true"#)
            .unwrap();

        cleanup_completed_operation_markers(&remote, &operation_id, "branch-token").unwrap();

        assert!(remote.exists_physical("leases/branch-token.lock"));
    }

    #[test]
    fn cleanup_completed_operation_markers_keeps_corrupted_lease_when_current_intent_is_foreign() {
        let remote = MemoryBackend::new();
        let operation_id = OperationId::new("cleanup-current-operation".to_string()).unwrap();
        let foreign_operation = OperationId::new("cleanup-foreign-operation".to_string()).unwrap();
        remote
            .put_physical(
                &format!("transactions/active/{}.intent", operation_id.value),
                br#"{"stale":true}"#,
            )
            .unwrap();
        remote
            .put_physical(
                &format!("transactions/active/{}.intent", foreign_operation.value),
                br#"{"active":true}"#,
            )
            .unwrap();
        remote
            .put_physical("leases/branch-token.lock", br#"{"broken":true"#)
            .unwrap();

        cleanup_completed_operation_markers(&remote, &operation_id, "branch-token").unwrap();

        assert!(
            remote.exists_physical("leases/branch-token.lock"),
            "cleanup for one completed operation must not delete a corrupted branch lease that could belong to another active operation"
        );
        assert!(!remote.exists_physical(&format!(
            "transactions/active/{}.intent",
            operation_id.value
        )));
        assert!(remote.exists_physical(&format!(
            "transactions/active/{}.intent",
            foreign_operation.value
        )));
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

    #[test]
    fn read_remote_current_keyring_bytes_treats_not_found_generation_as_missing() {
        let temp = tempdir().unwrap();
        let repo_root = temp.path().join("repo");
        let remote_root = temp.path().join("remote");
        fs::create_dir_all(&repo_root).unwrap();
        fs::create_dir_all(&remote_root).unwrap();

        let facade = RepositoryFacade::new();
        let state = facade
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
                message: "unit-missing-remote-keyring-generation".to_string(),
            })
            .unwrap();

        let remote = LocalFolderBackend::new(&remote_root);
        push_head(
            &facade,
            &remote,
            PushOptions {
                repo_root: repo_root.clone(),
                branch_token: state.branch.token_hex.clone(),
                operation_id: "unit-missing-remote-keyring-generation-op".to_string(),
            },
        )
        .unwrap();

        let pointer_bytes = remote
            .get_physical("control/keyring/keyring.current")
            .unwrap();
        let pointer: serde_json::Value = serde_json::from_slice(&pointer_bytes).unwrap();
        let current = pointer["current"].as_str().unwrap();
        remote
            .delete_physical(&format!("control/keyring/{current}"))
            .unwrap();

        let remote_keyring_bytes = read_remote_current_keyring_bytes(&remote, &repo_root).unwrap();

        assert!(
            remote_keyring_bytes.is_none(),
            "missing remote keyring generation should be treated as absent state"
        );
    }

    #[test]
    fn read_remote_current_keyring_bytes_rejects_pointer_path_traversal() {
        let temp = tempdir().unwrap();
        let repo_root = temp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();

        let facade = RepositoryFacade::new();
        let state = facade
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
                message: "unit-malicious-remote-keyring-pointer".to_string(),
            })
            .unwrap();

        let remote = MemoryBackend::new();
        push_head(
            &facade,
            &remote,
            PushOptions {
                repo_root: repo_root.clone(),
                branch_token: state.branch.token_hex.clone(),
                operation_id: "unit-malicious-remote-keyring-pointer-op".to_string(),
            },
        )
        .unwrap();

        let malicious_pointer = serde_json::to_vec(&serde_json::json!({
            "generation": 1u64,
            "current": "../../evil.json"
        }))
        .unwrap();
        let pointer_token = RefToken::new(format!(
            "keyring/{}",
            e2v_core::sync_support::read_repo_id(&repo_root).unwrap()
        ));
        let expected_version = remote
            .read_ref(&pointer_token)
            .unwrap()
            .map(|stored| stored.version);
        remote
            .compare_and_swap_ref(
                &pointer_token,
                expected_version,
                EncryptedRef::new(malicious_pointer.clone()),
            )
            .unwrap();
        remote
            .put_physical("control/keyring/keyring.current", &malicious_pointer)
            .unwrap();
        remote
            .put_physical(
                "control/keyring/../../evil.json",
                &remote.get_physical("control/keyring/keyring.1").unwrap(),
            )
            .unwrap();

        let error = read_remote_current_keyring_bytes(&remote, &repo_root).unwrap_err();

        assert!(
            error.to_string().contains("invalid remote keyring path")
                || error.to_string().contains("path traversal")
                || error.to_string().contains("path escapes"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn read_remote_current_keyring_bytes_rejects_pointer_generation_mismatch() {
        let temp = tempdir().unwrap();
        let repo_root = temp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();

        let facade = RepositoryFacade::new();
        let state = facade
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
                message: "unit-malicious-remote-keyring-generation".to_string(),
            })
            .unwrap();

        let remote = MemoryBackend::new();
        push_head(
            &facade,
            &remote,
            PushOptions {
                repo_root: repo_root.clone(),
                branch_token: state.branch.token_hex.clone(),
                operation_id: "unit-malicious-remote-keyring-generation-op".to_string(),
            },
        )
        .unwrap();

        let pointer_token = RefToken::new(format!(
            "keyring/{}",
            e2v_core::sync_support::read_repo_id(&repo_root).unwrap()
        ));
        let expected_version = remote
            .read_ref(&pointer_token)
            .unwrap()
            .map(|stored| stored.version);
        let mismatched_pointer = serde_json::to_vec(&serde_json::json!({
            "generation": 99u64,
            "current": "keyring.1"
        }))
        .unwrap();
        remote
            .compare_and_swap_ref(
                &pointer_token,
                expected_version,
                EncryptedRef::new(mismatched_pointer.clone()),
            )
            .unwrap();
        remote
            .put_physical("control/keyring/keyring.current", &mismatched_pointer)
            .unwrap();

        let error = read_remote_current_keyring_bytes(&remote, &repo_root).unwrap_err();

        assert!(
            error.to_string().contains("generation mismatch")
                || error
                    .to_string()
                    .contains("remote keyring pointer generation mismatch"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn publish_remote_keyring_pointer_reads_remote_ref_once_when_pointer_differs() {
        let temp = tempdir().unwrap();
        let repo_root = temp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();

        let facade = RepositoryFacade::new();
        facade
            .init(InitOptions {
                repo_root: repo_root.clone(),
                password: "correct horse battery staple".to_string(),
                branch_name: "main".to_string(),
            })
            .unwrap();

        let keyring_files = e2v_core::sync_support::list_keyring_files(&repo_root).unwrap();
        let repo_id = e2v_core::sync_support::read_repo_id(&repo_root).unwrap();
        let remote = KeyringPointerReadCountingBackend::new();
        remote
            .compare_and_swap_ref(
                &RefToken::new(format!("keyring/{repo_id}")),
                None,
                EncryptedRef::new(br#"{"generation":999,"current":"keyring.999"}"#.to_vec()),
            )
            .unwrap();

        publish_remote_keyring_pointer(&remote, &keyring_files).unwrap();

        assert_eq!(
            remote.keyring_pointer_ref_read_count(),
            1,
            "publish_remote_keyring_pointer should reuse the first remote ref read instead of re-reading the same keyring pointer ref before CAS"
        );
    }

    #[test]
    fn publish_remote_keyring_pointer_rejects_local_pointer_generation_mismatch() {
        let temp = tempdir().unwrap();
        let repo_root = temp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();

        let facade = RepositoryFacade::new();
        facade
            .init(InitOptions {
                repo_root: repo_root.clone(),
                password: "correct horse battery staple".to_string(),
                branch_name: "main".to_string(),
            })
            .unwrap();

        let keyring_dir = repo_root.join(".e2v").join("keyring");
        std::fs::write(
            keyring_dir.join("keyring.current"),
            serde_json::to_vec(&serde_json::json!({
                "generation": 99u64,
                "current": "keyring.1"
            }))
            .unwrap(),
        )
        .unwrap();

        let keyring_files = e2v_core::sync_support::list_keyring_files(&repo_root).unwrap();
        let remote = MemoryBackend::new();

        let error = publish_remote_keyring_pointer(&remote, &keyring_files).unwrap_err();

        assert!(
            error.to_string().contains("generation mismatch")
                || error
                    .to_string()
                    .contains("local keyring pointer generation mismatch"),
            "unexpected error: {error:#}"
        );
    }
}
