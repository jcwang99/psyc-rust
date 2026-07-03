use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, ensure};
use postcard::from_bytes as postcard_from_bytes;
use serde::Deserialize;

use crate::journal::validate_sync_identifier;
use crate::object_type::candidate_object_types;
use crate::oram::load_remote_active_pack_locations_with_local_cache;
use crate::pack::{PackedObjectLocation, read_packed_object};
use crate::trusted_state::{
    TrustedRemoteState, load_trusted_remote_state, store_trusted_remote_state,
};
use e2v_core::{
    RepositoryFacade, clear_unlocked_keyring_cache,
    sync_support::{
        unlock_repo_secrets_for_sync, unlock_repo_secrets_from_keyring_bytes_for_sync,
        unlock_repo_secrets_from_keyring_bytes_with_local_device_for_sync,
    },
    validate_layout_root_value,
};
use e2v_store::{
    DirectLayoutObjectStore, RefToken, RemoteBackend, RepoSecrets, validate_object_id_value,
};

const KEYRING_LOCK_FILE: &str = "keyring.lock";
const REPO_FORMAT_VERSION: u32 = 1;

#[derive(Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct KeyringPointer {
    generation: u64,
    current: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RemoteKeyringPointerSummary {
    pub(crate) generation: u64,
}

#[derive(Debug, Deserialize)]
struct KeyringStateSummary {
    repo_id: String,
}

#[derive(Debug, Deserialize)]
struct KeyringEnvelopeSummary {
    kind: String,
}

#[derive(Debug, Deserialize)]
struct RemoteKeyringStateValidation {
    generation: u64,
    envelopes: Vec<KeyringEnvelopeSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RepositorySyncMode {
    SameRepositoryPointerUnchanged,
    SameRepositoryPointerChanged,
    ReplaceLocalState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalObjectHealth {
    Verified,
    LockedByteComparable,
    LockedEnvelopeInvalid,
    Unhealthy,
}

pub(crate) struct RemoteControlPlane {
    pub(crate) repo_id: String,
    pub(crate) keyring_pointer_bytes: Vec<u8>,
    pub(crate) keyring_pointer: KeyringPointer,
    pub(crate) current_keyring_bytes: Vec<u8>,
    pub(crate) keyring_files: Vec<(String, Vec<u8>)>,
    pub(crate) layout_root_bytes: Vec<u8>,
    pub(crate) default_ref_bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct RemoteSnapshotObject {
    schema_version: u32,
    message: String,
    root_tree_id: String,
    parent_snapshot_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct RemoteTreeEntry {
    name: String,
    kind: String,
    object_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct RemoteTreeObject {
    schema_version: u32,
    entries: Vec<RemoteTreeEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct RemoteDirectoryRootObject {
    schema_version: u32,
    fanout: u32,
    shards: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct RemoteTreeShardObject {
    schema_version: u32,
    range_start: String,
    range_end: String,
    entries: Vec<RemoteTreeEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct RemoteFileObject {
    schema_version: u32,
    entry_name: String,
    file_size: u64,
    modified_unix_ms: u64,
    chunker_id: String,
    chunker_config_id: String,
    chunk_count: u64,
    chunks: Vec<String>,
    chunk_lengths: Vec<u64>,
    shard_ids: Vec<String>,
    shard_byte_lengths: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct RemoteFileShardObject {
    schema_version: u32,
    chunks: Vec<String>,
    chunk_lengths: Vec<u64>,
}

struct RemoteReachabilityContext<'a, R: RemoteBackend> {
    remote: &'a R,
    remote_loose_object_ids: &'a BTreeSet<String>,
    pack_locations: &'a BTreeMap<String, PackedObjectLocation>,
    validation_root: &'a RemoteValidationRoot,
    object_store: &'a DirectLayoutObjectStore,
    pack_cache: &'a mut BTreeMap<String, Vec<u8>>,
    recorder: &'a mut dyn RemoteReachabilityRecorder,
}

pub(crate) trait RemoteReachabilityRecorder {
    fn record(&mut self, object_id: &str) -> Result<bool>;
}

struct VecReachabilityRecorder<'a> {
    reachable_object_ids: &'a mut Vec<String>,
    seen: HashSet<String>,
}

impl RemoteReachabilityRecorder for VecReachabilityRecorder<'_> {
    fn record(&mut self, object_id: &str) -> Result<bool> {
        if self.seen.insert(object_id.to_string()) {
            self.reachable_object_ids.push(object_id.to_string());
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

pub(crate) struct RemoteReachabilityTraversal<'a, R: RemoteBackend> {
    pub(crate) remote: &'a R,
    pub(crate) remote_loose_object_ids: &'a BTreeSet<String>,
    pub(crate) pack_locations: &'a BTreeMap<String, PackedObjectLocation>,
    pub(crate) validation_root: &'a RemoteValidationRoot,
    pub(crate) validation_secrets: &'a RepoSecrets,
    pub(crate) pack_cache: &'a mut BTreeMap<String, Vec<u8>>,
}

struct RemoteFetchValidationInputs<'a> {
    repo_root: &'a Path,
    control_plane: &'a RemoteControlPlane,
    password: Option<&'a str>,
    device_validation_secrets: Option<&'a RepoSecrets>,
    default_ref_bytes: &'a [u8],
    remote_loose_object_ids: &'a BTreeSet<String>,
    pack_locations: &'a BTreeMap<String, PackedObjectLocation>,
}

#[derive(Debug)]
pub(crate) struct RemoteValidationRoot {
    pub(crate) path: PathBuf,
}

impl RemoteValidationRoot {
    pub(crate) fn control_dir(&self) -> PathBuf {
        self.path.join(".e2v")
    }

    pub(crate) fn object_path(&self, object_id: &str) -> PathBuf {
        self.control_dir()
            .join("objects")
            .join(format!("{object_id}.json"))
    }
}

impl Drop for RemoteValidationRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[derive(Debug)]
struct RemoteFetchPlan {
    validation_root: RemoteValidationRoot,
    reachable_object_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchOptions {
    pub repo_root: PathBuf,
    pub branch_token: String,
    pub password: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchResult {
    pub downloaded_objects: usize,
}

pub fn fetch_remote<R: RemoteBackend>(remote: &R, options: FetchOptions) -> Result<FetchResult> {
    validate_sync_identifier("branch token", &options.branch_token)?;
    let control_dir = options.repo_root.join(".e2v");
    let objects_dir = options.repo_root.join(".e2v").join("objects");
    std::fs::create_dir_all(control_dir.join("keyring"))?;
    std::fs::create_dir_all(control_dir.join("refs"))?;
    std::fs::create_dir_all(&objects_dir)?;

    let sync_mode = classify_repository_sync_mode(remote, &control_dir)?;
    let requested_branch_token = options.branch_token.clone();
    let ref_token = RefToken::new(options.branch_token);
    let stored_ref = remote
        .read_ref(&ref_token)?
        .ok_or_else(|| anyhow::anyhow!("remote branch ref not found"))?;
    let stored_ref_bytes = stored_ref.value.bytes.clone();
    let control_plane = read_remote_control_plane(remote, stored_ref.value.bytes.clone())?;
    let preserve_newer_local_keyring =
        local_keyring_is_newer_than_remote(&control_dir, &control_plane)?;
    ensure!(
        control_plane.keyring_pointer.generation > 0
            && !control_plane.keyring_pointer.current.trim().is_empty(),
        "invalid remote keyring pointer"
    );
    assert_remote_generations_meet_local_floor(&stored_ref, &control_plane)?;
    let local_repo_secrets = e2v_core::sync_support::open_repo_secrets_for_sync(&control_dir).ok();
    let remote_current_device_secrets = if matches!(
        sync_mode,
        RepositorySyncMode::SameRepositoryPointerUnchanged
            | RepositorySyncMode::SameRepositoryPointerChanged
    ) {
        match unlock_repo_secrets_from_keyring_bytes_with_local_device_for_sync(
            &control_dir,
            &control_plane.current_keyring_bytes,
        ) {
            Ok(secrets) => Some(secrets),
            Err(error) if options.password.is_none() && !preserve_newer_local_keyring => {
                return Err(error).context("current local device cannot unlock remote keyring");
            }
            Err(_) => None,
        }
    } else {
        None
    };
    let pack_index_secrets = match sync_mode {
        RepositorySyncMode::ReplaceLocalState => options.password.as_deref().and_then(|password| {
            unlock_repo_secrets_from_keyring_bytes_for_sync(
                &control_plane.current_keyring_bytes,
                password,
            )
            .ok()
        }),
        RepositorySyncMode::SameRepositoryPointerUnchanged
        | RepositorySyncMode::SameRepositoryPointerChanged => remote_current_device_secrets
            .clone()
            .or_else(|| local_repo_secrets.clone())
            .or_else(|| {
                options
                    .password
                    .as_deref()
                    .and_then(|password| unlock_repo_secrets_for_sync(&control_dir, password).ok())
            })
            .or_else(|| {
                options.password.as_deref().and_then(|password| {
                    unlock_repo_secrets_from_keyring_bytes_for_sync(
                        &control_plane.current_keyring_bytes,
                        password,
                    )
                    .ok()
                })
            }),
    };

    let listed = remote.list_physical("objects/")?;
    let pack_locations = match pack_index_secrets.as_ref() {
        Some(secrets) => {
            load_remote_active_pack_locations_with_local_cache(remote, &control_dir, secrets)?
        }
        None => BTreeMap::new(),
    };
    let mut validated_remote_objects = Vec::with_capacity(listed.len());
    let mut remote_loose_object_ids = BTreeSet::new();
    for relative_path in listed {
        let file_name = relative_path
            .strip_prefix("objects/")
            .ok_or_else(|| anyhow::anyhow!("invalid remote object path {relative_path}"))?;
        validate_remote_relative_name(file_name).map_err(|error| {
            anyhow::anyhow!("invalid remote object path {relative_path}: {error}")
        })?;
        let file_name = file_name.to_string();
        let object_id = file_name
            .strip_suffix(".json")
            .unwrap_or(&file_name)
            .to_string();
        validate_object_id_value(&object_id).map_err(|error| {
            anyhow::anyhow!("invalid remote object path {relative_path}: {error}")
        })?;
        remote_loose_object_ids.insert(object_id);
        validated_remote_objects.push((relative_path, file_name));
    }

    let remote_fetch_plan = if options.password.is_some()
        && matches!(
            sync_mode,
            RepositorySyncMode::ReplaceLocalState
                | RepositorySyncMode::SameRepositoryPointerUnchanged
        ) {
        Some(prepare_remote_fetch_plan(
            remote,
            RemoteFetchValidationInputs {
                repo_root: &options.repo_root,
                control_plane: &control_plane,
                password: options.password.as_deref(),
                device_validation_secrets: remote_current_device_secrets.as_ref(),
                default_ref_bytes: &stored_ref_bytes,
                remote_loose_object_ids: &remote_loose_object_ids,
                pack_locations: &pack_locations,
            },
        )?)
    } else {
        None
    };
    if remote_fetch_plan.is_none()
        && matches!(
            sync_mode,
            RepositorySyncMode::SameRepositoryPointerUnchanged
                | RepositorySyncMode::SameRepositoryPointerChanged
        )
    {
        validate_remote_ref_consistency_if_locally_unlocked(
            remote,
            &options.repo_root,
            &requested_branch_token,
            &stored_ref_bytes,
            Some(&remote_loose_object_ids),
            &pack_locations,
        )?;
    }
    if matches!(
        sync_mode,
        RepositorySyncMode::SameRepositoryPointerUnchanged
    ) && local_control_plane_matches_remote(&control_dir, &control_plane)?
        && RepositoryFacade::new()
            .verify_ref(&options.repo_root)
            .is_ok()
    {
        return Ok(FetchResult {
            downloaded_objects: 0,
        });
    }
    let mut downloaded_objects = 0usize;
    if let Some(remote_fetch_plan) = &remote_fetch_plan {
        for object_id in &remote_fetch_plan.reachable_object_ids {
            let target_path = objects_dir.join(format!("{object_id}.json"));
            let bytes = std::fs::read(remote_fetch_plan.validation_root.object_path(object_id))?;
            if !target_path.exists() {
                std::fs::write(&target_path, bytes)?;
                downloaded_objects += 1;
                continue;
            }

            match classify_local_object_health(&options.repo_root, object_id) {
                LocalObjectHealth::Verified => {}
                LocalObjectHealth::LockedByteComparable => {
                    if !local_object_matches_bytes(&options.repo_root, object_id, &bytes) {
                        anyhow::bail!(
                            "cannot replace locked local object {object_id} with unverified remote bytes; unlock repository first"
                        );
                    }
                }
                LocalObjectHealth::LockedEnvelopeInvalid | LocalObjectHealth::Unhealthy => {
                    std::fs::write(&target_path, bytes)?;
                    downloaded_objects += 1;
                }
            }
        }
    } else {
        let mut pack_cache = BTreeMap::new();
        for (relative_path, file_name) in validated_remote_objects {
            let object_id = file_name
                .strip_suffix(".json")
                .unwrap_or(&file_name)
                .to_string();
            let target_path = objects_dir.join(file_name);
            if !target_path.exists() {
                let bytes = remote.get_physical(&relative_path)?;
                std::fs::write(&target_path, bytes)?;
                downloaded_objects += 1;
                continue;
            }

            match classify_local_object_health(&options.repo_root, &object_id) {
                LocalObjectHealth::Verified => {}
                LocalObjectHealth::LockedByteComparable => {
                    let bytes = remote.get_physical(&relative_path)?;
                    if !local_object_matches_bytes(&options.repo_root, &object_id, &bytes) {
                        anyhow::bail!(
                            "cannot replace locked local object {object_id} with unverified remote bytes; unlock repository first"
                        );
                    }
                }
                LocalObjectHealth::LockedEnvelopeInvalid => {
                    let bytes = remote.get_physical(&relative_path)?;
                    std::fs::write(&target_path, bytes)?;
                    downloaded_objects += 1;
                }
                LocalObjectHealth::Unhealthy => {
                    let bytes = remote.get_physical(&relative_path)?;
                    std::fs::write(&target_path, bytes)?;
                    downloaded_objects += 1;
                }
            }
        }
        for object_id in pack_locations.keys() {
            let target_path = objects_dir.join(format!("{object_id}.json"));
            if target_path.exists() {
                match classify_local_object_health(&options.repo_root, object_id) {
                    LocalObjectHealth::Verified => continue,
                    LocalObjectHealth::LockedByteComparable => {
                        if let Some(bytes) = remote_object_bytes_with_pack_cache(
                            remote,
                            &remote_loose_object_ids,
                            &pack_locations,
                            &mut pack_cache,
                            Some(&control_dir),
                            object_id,
                        )? && !local_object_matches_bytes(&options.repo_root, object_id, &bytes)
                        {
                            anyhow::bail!(
                                "cannot replace locked local object {object_id} with unverified remote bytes; unlock repository first"
                            );
                        }
                        continue;
                    }
                    LocalObjectHealth::LockedEnvelopeInvalid => {}
                    LocalObjectHealth::Unhealthy => {}
                }
            }
            if let Some(bytes) = remote_object_bytes_with_pack_cache(
                remote,
                &remote_loose_object_ids,
                &pack_locations,
                &mut pack_cache,
                Some(&control_dir),
                object_id,
            )? {
                std::fs::write(&target_path, bytes)?;
                downloaded_objects += 1;
            }
        }
    }
    if matches!(sync_mode, RepositorySyncMode::ReplaceLocalState) {
        verify_replace_local_state(
            &options.repo_root,
            &objects_dir,
            &control_plane,
            options.password.as_deref(),
            remote_current_device_secrets.as_ref(),
        )?;
    }

    if !preserve_newer_local_keyring {
        for (file_name, bytes) in &control_plane.keyring_files {
            atomic_write_bytes(control_dir.join("keyring").join(file_name), bytes)?;
        }
    }
    atomic_write_bytes(
        control_dir.join("refs").join("default.json"),
        &control_plane.default_ref_bytes,
    )?;
    atomic_write_bytes(
        control_dir.join("layout_root.json"),
        &control_plane.layout_root_bytes,
    )?;
    if !preserve_newer_local_keyring {
        atomic_write_bytes(
            control_dir.join("keyring").join("keyring.current"),
            &control_plane.keyring_pointer_bytes,
        )?;
    }
    if !matches!(
        sync_mode,
        RepositorySyncMode::SameRepositoryPointerUnchanged
    ) {
        clear_unlocked_keyring_cache(&control_dir);
    }
    update_trusted_remote_state_from_control_plane(&stored_ref, &control_plane)?;

    Ok(FetchResult { downloaded_objects })
}

fn local_keyring_is_newer_than_remote(
    control_dir: &Path,
    control_plane: &RemoteControlPlane,
) -> Result<bool> {
    let local_pointer_path = control_dir.join("keyring").join("keyring.current");
    if !local_pointer_path.is_file() {
        return Ok(false);
    }
    let local_pointer: KeyringPointer =
        serde_json::from_slice(&std::fs::read(&local_pointer_path)?)
            .with_context(|| format!("failed to decode {}", local_pointer_path.display()))?;
    if local_pointer.generation <= control_plane.keyring_pointer.generation {
        return Ok(false);
    }
    let local_keyring_path = control_dir.join("keyring").join(&local_pointer.current);
    let local_keyring_bytes = match std::fs::read(&local_keyring_path) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(false),
    };
    let _local_keyring: KeyringStateSummary = match serde_json::from_slice(&local_keyring_bytes) {
        Ok(keyring) => keyring,
        Err(_) => return Ok(false),
    };
    if e2v_core::sync_support::open_repo_secrets_for_sync(control_dir).is_err()
        && e2v_core::sync_support::unlock_repo_secrets_from_keyring_bytes_with_local_device_for_sync(
            control_dir,
            &local_keyring_bytes,
        )
        .is_err()
    {
        return Ok(false);
    }
    Ok(true)
}

fn prepare_remote_fetch_plan<R: RemoteBackend>(
    remote: &R,
    inputs: RemoteFetchValidationInputs<'_>,
) -> Result<RemoteFetchPlan> {
    let validation_root = RemoteValidationRoot {
        path: next_validation_root(inputs.repo_root)?,
    };
    write_remote_control_plane_to_validation_root(&validation_root, inputs.control_plane)?;
    let validation_secrets = load_remote_validation_secrets(
        &validation_root,
        inputs.password,
        inputs.device_validation_secrets,
    )?;
    let mut pack_cache = BTreeMap::new();
    let (_, head_snapshot_id) = e2v_core::sync_support::decode_default_ref_record(
        &validation_root.path,
        inputs.default_ref_bytes,
    )?;
    let reachable_object_ids = match head_snapshot_id.as_deref() {
        Some(head_snapshot_id) => {
            let reachable_object_ids = collect_remote_reachable_object_ids(
                remote,
                inputs.remote_loose_object_ids,
                inputs.pack_locations,
                &validation_root,
                &validation_secrets,
                &mut pack_cache,
                head_snapshot_id,
            )?;
            materialize_remote_objects_into_validation_root(
                remote,
                inputs.remote_loose_object_ids,
                inputs.pack_locations,
                &validation_root,
                &mut pack_cache,
                &reachable_object_ids,
            )?;
            e2v_core::sync_support::verify_snapshot_with_secrets_for_sync(
                &validation_root.path,
                validation_secrets,
                head_snapshot_id,
            )?;
            persist_cached_pack_data(inputs.repo_root.join(".e2v"), &pack_cache)?;
            reachable_object_ids
        }
        None => Vec::new(),
    };
    Ok(RemoteFetchPlan {
        validation_root,
        reachable_object_ids,
    })
}

fn persist_cached_pack_data(
    control_dir: PathBuf,
    pack_cache: &BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    for (container_id, pack_bytes) in pack_cache {
        cache_pack_data_bytes(&control_dir, container_id, pack_bytes)?;
    }
    Ok(())
}

fn classify_repository_sync_mode<R: RemoteBackend>(
    remote: &R,
    control_dir: &std::path::Path,
) -> Result<RepositorySyncMode> {
    let has_local_objects = local_objects_dir_has_entries(control_dir)?;
    let local_pointer_path = control_dir.join("keyring").join("keyring.current");
    if !local_pointer_path.is_file() {
        if has_local_objects {
            anyhow::bail!(
                "repository identity mismatch: local keyring pointer is missing while local history exists"
            );
        }
        return Ok(RepositorySyncMode::ReplaceLocalState);
    }

    let local_pointer_bytes = std::fs::read(&local_pointer_path)
        .with_context(|| format!("failed to read {}", local_pointer_path.display()))?;
    let local_pointer: KeyringPointer = match serde_json::from_slice(&local_pointer_bytes) {
        Ok(pointer) => pointer,
        Err(error) if !has_local_objects => {
            return Ok(RepositorySyncMode::ReplaceLocalState);
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to decode {}", local_pointer_path.display()));
        }
    };
    let local_keyring_path = control_dir.join("keyring").join(&local_pointer.current);
    let local_state_bytes = match std::fs::read(&local_keyring_path) {
        Ok(bytes) => bytes,
        Err(error) if !has_local_objects => {
            return Ok(RepositorySyncMode::ReplaceLocalState);
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read {}", local_keyring_path.display()));
        }
    };
    let local_state: KeyringStateSummary = match serde_json::from_slice(&local_state_bytes) {
        Ok(state) => state,
        Err(error) if !has_local_objects => {
            return Ok(RepositorySyncMode::ReplaceLocalState);
        }
        Err(error) => return Err(error).context("failed to decode local keyring state"),
    };
    let remote_pointer_bytes = read_remote_keyring_pointer_bytes(remote)?;
    let remote_pointer: KeyringPointer = serde_json::from_slice(&remote_pointer_bytes)
        .context("failed to decode remote keyring pointer")?;
    let remote_state: KeyringStateSummary = serde_json::from_slice(
        &remote.get_physical(&format!("control/keyring/{}", remote_pointer.current))?,
    )
    .context("failed to decode remote keyring state")?;

    if local_state.repo_id == remote_state.repo_id {
        return Ok(if local_pointer == remote_pointer {
            RepositorySyncMode::SameRepositoryPointerUnchanged
        } else {
            RepositorySyncMode::SameRepositoryPointerChanged
        });
    }

    if !has_local_objects {
        return Ok(RepositorySyncMode::ReplaceLocalState);
    }

    anyhow::bail!(
        "repository identity mismatch: remote repository does not match local repository"
    );
}

fn local_objects_dir_has_entries(control_dir: &std::path::Path) -> Result<bool> {
    let objects_dir = control_dir.join("objects");
    Ok(std::fs::read_dir(&objects_dir)
        .with_context(|| format!("failed to read {}", objects_dir.display()))?
        .next()
        .transpose()
        .with_context(|| format!("failed to scan {}", objects_dir.display()))?
        .is_some())
}

fn local_control_plane_matches_remote(
    control_dir: &Path,
    control_plane: &RemoteControlPlane,
) -> Result<bool> {
    Ok(read_if_exists(control_dir.join("layout_root.json"))
        == Some(control_plane.layout_root_bytes.clone())
        && read_if_exists(control_dir.join("refs").join("default.json"))
            == Some(control_plane.default_ref_bytes.clone())
        && read_if_exists(control_dir.join("keyring").join("keyring.current"))
            == Some(control_plane.keyring_pointer_bytes.clone()))
}

fn read_if_exists(path: PathBuf) -> Option<Vec<u8>> {
    std::fs::read(path).ok()
}

fn atomic_write_bytes(path: PathBuf, bytes: &[u8]) -> Result<()> {
    let temp_path = path.with_extension(format!(
        "{}.tmp",
        path.extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("tmp")
    ));
    std::fs::write(&temp_path, bytes)
        .with_context(|| format!("failed to write {}", temp_path.display()))?;
    std::fs::rename(&temp_path, &path)
        .with_context(|| format!("failed to publish {}", path.display()))?;
    Ok(())
}

fn classify_local_object_health(repo_root: &Path, object_id: &str) -> LocalObjectHealth {
    let facade = e2v_core::RepositoryFacade::new();
    let mut saw_locked_error = false;
    let hint = e2v_core::sync_support::read_local_object_type_hint(repo_root, object_id).ok();
    for object_type in candidate_object_types(hint.as_deref()) {
        match facade.verify_object(repo_root, object_id, object_type) {
            Ok(()) => return LocalObjectHealth::Verified,
            Err(error) => {
                let error_text = error.to_string();
                if error_text.contains("repository keyring is locked")
                    || error_text.contains("unlock with a password first")
                {
                    saw_locked_error = true;
                }
            }
        }
    }

    if saw_locked_error {
        if e2v_core::sync_support::local_object_envelope_looks_valid(repo_root, object_id)
            .unwrap_or(false)
        {
            return LocalObjectHealth::LockedByteComparable;
        }
        return LocalObjectHealth::LockedEnvelopeInvalid;
    }

    LocalObjectHealth::Unhealthy
}

fn local_object_matches_bytes(repo_root: &Path, object_id: &str, expected_bytes: &[u8]) -> bool {
    e2v_core::sync_support::read_local_object_bytes(repo_root, object_id)
        .map(|bytes| bytes == expected_bytes)
        .unwrap_or(false)
}

fn remote_object_bytes<R: RemoteBackend>(
    remote: &R,
    loose_object_ids: &BTreeSet<String>,
    pack_locations: &BTreeMap<String, PackedObjectLocation>,
    object_id: &str,
) -> Result<Option<Vec<u8>>> {
    if loose_object_ids.contains(object_id) {
        return Ok(Some(
            remote.get_physical(&format!("objects/{object_id}.json"))?,
        ));
    }
    read_packed_object(remote, pack_locations, object_id)
}

pub(crate) fn remote_object_bytes_with_pack_cache<R: RemoteBackend>(
    remote: &R,
    loose_object_ids: &BTreeSet<String>,
    pack_locations: &BTreeMap<String, PackedObjectLocation>,
    pack_cache: &mut BTreeMap<String, Vec<u8>>,
    control_dir: Option<&Path>,
    object_id: &str,
) -> Result<Option<Vec<u8>>> {
    if loose_object_ids.contains(object_id) {
        return Ok(Some(
            remote.get_physical(&format!("objects/{object_id}.json"))?,
        ));
    }

    let Some(location) = pack_locations.get(object_id) else {
        return Ok(None);
    };
    let physical_ref = location.physical_ref();
    if !pack_cache.contains_key(&physical_ref.container_id) {
        let pack_len: usize = remote
            .stat_physical(&physical_ref.container_id)?
            .length
            .try_into()
            .map_err(|_| anyhow::anyhow!("pack is too large to read on this platform"))?;
        let pack_bytes = remote.get_physical_range(&physical_ref.container_id, 0, pack_len)?;
        if let Some(control_dir) = control_dir {
            cache_pack_data_bytes(control_dir, &physical_ref.container_id, &pack_bytes)?;
        }
        pack_cache.insert(physical_ref.container_id.clone(), pack_bytes);
    }
    let pack_bytes = pack_cache.get(&physical_ref.container_id).unwrap();
    let offset = usize::try_from(physical_ref.offset.unwrap_or(0))
        .map_err(|_| anyhow::anyhow!("pack offset is too large to read on this platform"))?;
    let length = usize::try_from(physical_ref.length)
        .map_err(|_| anyhow::anyhow!("pack length is too large to read on this platform"))?;
    let end = offset.saturating_add(length);
    ensure!(
        end <= pack_bytes.len(),
        "packed object range out of bounds for {object_id}"
    );
    Ok(Some(pack_bytes[offset..end].to_vec()))
}

fn cache_pack_data_bytes(control_dir: &Path, container_id: &str, pack_bytes: &[u8]) -> Result<()> {
    validate_remote_relative_name(container_id)?;
    let cache_path = control_dir
        .join("cache")
        .join("pack-data")
        .join(container_id);
    if cache_path.is_file() {
        return Ok(());
    }
    if let Some(parent) = cache_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    atomic_write_bytes(cache_path, pack_bytes)
}

fn remote_object_authenticates_for_repo<R: RemoteBackend>(
    repo_root: &Path,
    remote: &R,
    loose_object_ids: &BTreeSet<String>,
    pack_locations: &BTreeMap<String, PackedObjectLocation>,
    object_id: &str,
    expected_type: &str,
) -> Result<()> {
    let control_dir = repo_root.join(".e2v");
    let secrets = e2v_core::sync_support::open_repo_secrets_for_sync(&control_dir)?;
    let validation_root = RemoteValidationRoot {
        path: next_validation_root(repo_root)?,
    };
    let validation_control = validation_root.control_dir();
    (|| -> Result<()> {
        std::fs::create_dir_all(validation_control.join("objects"))?;
        std::fs::create_dir_all(validation_control.join("keyring"))?;
        std::fs::create_dir_all(validation_control.join("refs"))?;
        std::fs::copy(
            control_dir.join("layout_root.json"),
            validation_control.join("layout_root.json"),
        )?;
        std::fs::copy(
            control_dir.join("refs").join("default.json"),
            validation_control.join("refs").join("default.json"),
        )?;
        let pointer_bytes = std::fs::read(control_dir.join("keyring").join("keyring.current"))?;
        std::fs::write(
            validation_control.join("keyring").join("keyring.current"),
            pointer_bytes,
        )?;

        let store = e2v_store::DirectLayoutObjectStore::new(&validation_control, secrets);
        let bytes = remote_object_bytes(remote, loose_object_ids, pack_locations, object_id)?
            .with_context(|| format!("missing remote object {object_id}"))?;
        let object_file_name = validation_object_file_name(object_id)?;
        let target_path = validation_control.join("objects").join(object_file_name);
        std::fs::write(&target_path, &bytes)?;
        let _ = store.get_object(object_id, expected_type)?;
        Ok(())
    })()
}

fn remote_snapshot_graph_authenticates_for_repo<R: RemoteBackend>(
    repo_root: &Path,
    remote: &R,
    loose_object_ids: &BTreeSet<String>,
    pack_locations: &BTreeMap<String, PackedObjectLocation>,
    snapshot_id: &str,
) -> Result<()> {
    remote_object_authenticates_for_repo(
        repo_root,
        remote,
        loose_object_ids,
        pack_locations,
        snapshot_id,
        "snapshot",
    )
    .context("failed to authenticate remote head snapshot object")?;

    let control_dir = repo_root.join(".e2v");
    let secrets = e2v_core::sync_support::open_repo_secrets_for_sync(&control_dir)?;
    let validation_root = RemoteValidationRoot {
        path: next_validation_root(repo_root)?,
    };
    let validation_control = validation_root.control_dir();
    let mut pack_cache: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    (|| -> Result<()> {
        std::fs::create_dir_all(validation_control.join("objects"))?;
        std::fs::create_dir_all(validation_control.join("keyring"))?;
        std::fs::create_dir_all(validation_control.join("refs"))?;
        std::fs::copy(
            control_dir.join("layout_root.json"),
            validation_control.join("layout_root.json"),
        )?;
        std::fs::copy(
            control_dir.join("refs").join("default.json"),
            validation_control.join("refs").join("default.json"),
        )?;
        let pointer_bytes = std::fs::read(control_dir.join("keyring").join("keyring.current"))?;
        std::fs::write(
            validation_control.join("keyring").join("keyring.current"),
            pointer_bytes,
        )?;

        let reachable_object_ids = collect_remote_reachable_object_ids(
            remote,
            loose_object_ids,
            pack_locations,
            &validation_root,
            &secrets,
            &mut pack_cache,
            snapshot_id,
        )?;
        materialize_remote_objects_into_validation_root(
            remote,
            loose_object_ids,
            pack_locations,
            &validation_root,
            &mut pack_cache,
            &reachable_object_ids,
        )?;
        e2v_core::sync_support::verify_snapshot_with_secrets_for_sync(
            &validation_root.path,
            secrets,
            snapshot_id,
        )?;
        Ok(())
    })()
}

pub(crate) fn read_remote_control_plane<R: RemoteBackend>(
    remote: &R,
    default_ref_bytes: Vec<u8>,
) -> Result<RemoteControlPlane> {
    let keyring_pointer_bytes = read_remote_keyring_pointer_bytes(remote)?;
    let keyring_pointer: KeyringPointer = serde_json::from_slice(&keyring_pointer_bytes)
        .context("failed to decode remote keyring pointer")?;
    validate_remote_relative_name(&keyring_pointer.current).map_err(|error| {
        anyhow::anyhow!(
            "invalid remote keyring path {}: {error}",
            keyring_pointer.current
        )
    })?;
    let pointed_keyring_path = format!("control/keyring/{}", keyring_pointer.current);
    let pointed_keyring_bytes = remote.get_physical(&pointed_keyring_path)?;
    let pointed_keyring_state: RemoteKeyringStateValidation =
        serde_json::from_slice(&pointed_keyring_bytes).with_context(|| {
            format!(
                "failed to decode remote keyring state {}",
                keyring_pointer.current
            )
        })?;
    let repo_id = serde_json::from_slice::<KeyringStateSummary>(&pointed_keyring_bytes)
        .context("failed to decode remote keyring state summary")?
        .repo_id;
    let mut keyring_files = Vec::new();
    keyring_files.push((
        keyring_pointer.current.clone(),
        pointed_keyring_bytes.clone(),
    ));
    for relative_path in remote.list_physical("control/keyring/")? {
        let file_name = relative_path
            .strip_prefix("control/keyring/")
            .ok_or_else(|| anyhow::anyhow!("invalid remote keyring path {relative_path}"))?
            .to_string();
        validate_remote_relative_name(&file_name).map_err(|error| {
            anyhow::anyhow!("invalid remote keyring path {relative_path}: {error}")
        })?;
        if file_name == keyring_pointer.current
            || file_name == "keyring.current"
            || file_name == KEYRING_LOCK_FILE
        {
            continue;
        }
        let bytes = remote.get_physical(&relative_path)?;
        keyring_files.push((file_name, bytes));
    }
    ensure!(
        pointed_keyring_state.generation == keyring_pointer.generation,
        "remote keyring pointer generation mismatch"
    );
    ensure!(
        pointed_keyring_state
            .envelopes
            .iter()
            .any(|envelope| envelope.kind == "password"),
        "remote keyring state has no password envelope"
    );
    let layout_root = remote.read_layout_root()?;
    validate_layout_root_value(&layout_root)?;
    let layout_root_bytes = serde_json::to_vec(&layout_root)?;

    Ok(RemoteControlPlane {
        repo_id,
        keyring_pointer_bytes,
        keyring_pointer,
        current_keyring_bytes: pointed_keyring_bytes.clone(),
        keyring_files,
        layout_root_bytes,
        default_ref_bytes,
    })
}

fn read_remote_keyring_pointer_bytes<R: RemoteBackend>(remote: &R) -> Result<Vec<u8>> {
    let keyring_refs = remote
        .list_refs()?
        .into_iter()
        .filter(|listed| listed.token.value.starts_with("keyring/"))
        .collect::<Vec<_>>();
    if keyring_refs.len() == 1 {
        return Ok(keyring_refs[0].stored.value.bytes.clone());
    }
    if keyring_refs.len() > 1 {
        anyhow::bail!("ambiguous remote keyring pointer refs");
    }
    anyhow::bail!("missing remote keyring pointer ref");
}

pub(crate) fn validate_remote_branch_control_plane<R: RemoteBackend>(
    remote: &R,
    _repo_root: &Path,
    branch_token: &str,
) -> Result<()> {
    validate_sync_identifier("branch token", branch_token)?;
    let stored_ref = remote
        .read_ref(&RefToken::new(branch_token.to_string()))?
        .ok_or_else(|| anyhow::anyhow!("remote branch ref not found"))?;
    let control_plane = read_remote_control_plane(remote, stored_ref.value.bytes.clone())?;
    assert_remote_generations_meet_local_floor(&stored_ref, &control_plane)?;
    Ok(())
}

pub(crate) fn assert_remote_generations_meet_local_floor(
    remote_default_ref: &e2v_store::StoredRef,
    control_plane: &RemoteControlPlane,
) -> Result<()> {
    let remote_layout_root: e2v_store::LayoutRoot =
        serde_json::from_slice(&control_plane.layout_root_bytes)
            .context("failed to decode remote layout root while checking rollback floor")?;
    let remote_keyring_pointer: RemoteKeyringPointerSummary =
        serde_json::from_slice(&control_plane.keyring_pointer_bytes)
            .context("failed to decode remote keyring pointer while checking rollback floor")?;
    let trusted_state = load_trusted_remote_state(&control_plane.repo_id)?;

    ensure!(
        remote_keyring_pointer.generation >= 1,
        "CRITICAL_ROLLBACK_DETECTED: remote keyring generation is invalid"
    );
    ensure!(
        remote_default_ref.version.value >= 1,
        "CRITICAL_ROLLBACK_DETECTED: remote ref generation is invalid"
    );
    if let Some(trusted_state) = trusted_state {
        ensure!(
            remote_layout_root.generation >= trusted_state.min_layout_generation,
            "CRITICAL_ROLLBACK_DETECTED: remote layout generation rollback detected"
        );
        ensure!(
            remote_keyring_pointer.generation >= trusted_state.min_keyring_generation,
            "CRITICAL_ROLLBACK_DETECTED: remote keyring generation rollback detected"
        );
        ensure!(
            remote_default_ref.version.value >= trusted_state.min_ref_generation,
            "CRITICAL_ROLLBACK_DETECTED: remote ref generation rollback detected"
        );
    }
    Ok(())
}

pub(crate) fn update_trusted_remote_state_from_control_plane(
    remote_default_ref: &e2v_store::StoredRef,
    control_plane: &RemoteControlPlane,
) -> Result<()> {
    let remote_layout_root: e2v_store::LayoutRoot =
        serde_json::from_slice(&control_plane.layout_root_bytes)
            .context("failed to decode remote layout root while updating trusted state")?;
    let remote_keyring_pointer: RemoteKeyringPointerSummary =
        serde_json::from_slice(&control_plane.keyring_pointer_bytes)
            .context("failed to decode remote keyring pointer while updating trusted state")?;
    let next = TrustedRemoteState {
        repo_id: control_plane.repo_id.clone(),
        min_layout_generation: remote_layout_root.generation,
        min_keyring_generation: remote_keyring_pointer.generation,
        min_ref_generation: remote_default_ref.version.value,
    };
    match load_trusted_remote_state(&next.repo_id)? {
        Some(current) => store_trusted_remote_state(&TrustedRemoteState {
            repo_id: next.repo_id.clone(),
            min_layout_generation: current
                .min_layout_generation
                .max(next.min_layout_generation),
            min_keyring_generation: current
                .min_keyring_generation
                .max(next.min_keyring_generation),
            min_ref_generation: current.min_ref_generation.max(next.min_ref_generation),
        }),
        None => store_trusted_remote_state(&next),
    }
}

fn validate_remote_relative_name(value: &str) -> Result<()> {
    let path = Path::new(value);
    ensure!(!value.is_empty(), "empty relative path");
    ensure!(!path.is_absolute(), "path escapes target directory");
    ensure!(
        path.components()
            .all(|component| matches!(component, Component::Normal(_))),
        "path traversal is not allowed"
    );
    Ok(())
}

fn validate_remote_object_file_name(value: &str) -> Result<()> {
    validate_remote_relative_name(value)?;
    ensure!(
        Path::new(value).components().count() == 1,
        "nested remote object paths are not allowed"
    );
    ensure!(
        value.ends_with(".json"),
        "remote object path must end with .json"
    );
    Ok(())
}

fn validation_object_file_name(object_id: &str) -> Result<String> {
    let file_name = format!("{object_id}.json");
    validate_remote_object_file_name(&file_name)?;
    Ok(file_name)
}

fn validate_remote_ref_consistency_if_locally_unlocked<R: RemoteBackend>(
    _remote: &R,
    repo_root: &Path,
    requested_branch_token: &str,
    default_ref_bytes: &[u8],
    remote_loose_object_ids: Option<&BTreeSet<String>>,
    pack_locations: &BTreeMap<String, PackedObjectLocation>,
) -> Result<()> {
    let (decoded_ref_token_hex, head_snapshot_id) =
        match e2v_core::sync_support::decode_default_ref_record(repo_root, default_ref_bytes) {
            Ok(decoded) => decoded,
            Err(error)
                if error.to_string().contains("locked")
                    || error.to_string().contains("unlock")
                    || error.to_string().contains("keyring") =>
            {
                return Ok(());
            }
            Err(error) => return Err(error).context("failed to decode remote branch ref"),
        };

    ensure!(
        decoded_ref_token_hex == requested_branch_token,
        "remote ref token mismatch: requested {requested_branch_token}, decoded {decoded_ref_token_hex}"
    );

    if let Some(head_snapshot_id) = head_snapshot_id {
        let owned_loose_object_ids;
        let loose_object_ids = if let Some(remote_loose_object_ids) = remote_loose_object_ids {
            remote_loose_object_ids
        } else {
            let listed = _remote.list_physical("objects/")?;
            let mut derived_loose_object_ids = BTreeSet::new();
            for relative_path in listed {
                let Some(object_id) = relative_path
                    .strip_prefix("objects/")
                    .and_then(|value| value.strip_suffix(".json"))
                else {
                    continue;
                };
                if validate_object_id_value(object_id).is_ok() {
                    derived_loose_object_ids.insert(object_id.to_string());
                }
            }
            owned_loose_object_ids = derived_loose_object_ids;
            &owned_loose_object_ids
        };
        ensure!(
            loose_object_ids.contains(&head_snapshot_id)
                || pack_locations.contains_key(&head_snapshot_id),
            "remote ref points to missing head snapshot {head_snapshot_id}"
        );
        remote_snapshot_graph_authenticates_for_repo(
            repo_root,
            _remote,
            loose_object_ids,
            pack_locations,
            &head_snapshot_id,
        )
        .with_context(|| {
            format!("remote ref points to unreadable head snapshot graph {head_snapshot_id}")
        })?;
    }

    Ok(())
}

fn verify_replace_local_state(
    repo_root: &Path,
    objects_dir: &Path,
    control_plane: &RemoteControlPlane,
    password: Option<&str>,
    device_validation_secrets: Option<&RepoSecrets>,
) -> Result<()> {
    let validation_root = next_validation_root(repo_root)?;
    let validation_control = validation_root.join(".e2v");
    std::fs::create_dir_all(validation_control.join("objects"))?;
    std::fs::create_dir_all(validation_control.join("keyring"))?;
    std::fs::create_dir_all(validation_control.join("refs"))?;

    for entry in std::fs::read_dir(objects_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        std::fs::copy(
            entry.path(),
            validation_control.join("objects").join(entry.file_name()),
        )?;
    }
    std::fs::write(
        validation_control.join("layout_root.json"),
        &control_plane.layout_root_bytes,
    )?;
    std::fs::write(
        validation_control.join("refs").join("default.json"),
        &control_plane.default_ref_bytes,
    )?;
    std::fs::write(
        validation_control.join("keyring").join("keyring.current"),
        &control_plane.keyring_pointer_bytes,
    )?;
    for (file_name, bytes) in &control_plane.keyring_files {
        std::fs::write(validation_control.join("keyring").join(file_name), bytes)?;
    }

    let facade = e2v_core::RepositoryFacade::new();
    let result = if let Some(password) = password {
        facade
            .unlock(&validation_root, password)
            .and_then(|_| facade.verify_ref(&validation_root))
            .context("remote head snapshot graph failed validation")
    } else if device_validation_secrets.is_some() {
        facade
            .open(&validation_root)
            .and_then(|_| facade.verify_ref(&validation_root))
            .context("remote head snapshot graph failed validation")
    } else {
        anyhow::bail!(
            "fetch into replacement repository requires password or local device verification"
        );
    };
    e2v_core::clear_unlocked_keyring_cache(&validation_control);
    let _ = std::fs::remove_dir_all(&validation_root);
    result
}

pub(crate) fn next_validation_root(repo_root: &Path) -> Result<PathBuf> {
    for attempt in 0..1024usize {
        let candidate = repo_root.join(format!(".e2v-fetch-validate-{attempt}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    anyhow::bail!("failed to allocate temporary fetch validation directory")
}

pub(crate) fn write_remote_control_plane_to_validation_root(
    validation_root: &RemoteValidationRoot,
    control_plane: &RemoteControlPlane,
) -> Result<()> {
    let validation_control = validation_root.control_dir();
    std::fs::create_dir_all(validation_control.join("objects"))?;
    std::fs::create_dir_all(validation_control.join("keyring"))?;
    std::fs::create_dir_all(validation_control.join("refs"))?;
    std::fs::write(
        validation_control.join("layout_root.json"),
        &control_plane.layout_root_bytes,
    )?;
    std::fs::write(
        validation_control.join("refs").join("default.json"),
        &control_plane.default_ref_bytes,
    )?;
    std::fs::write(
        validation_control.join("keyring").join("keyring.current"),
        &control_plane.keyring_pointer_bytes,
    )?;
    for (file_name, bytes) in &control_plane.keyring_files {
        std::fs::write(validation_control.join("keyring").join(file_name), bytes)?;
    }
    Ok(())
}

fn load_remote_validation_secrets(
    validation_root: &RemoteValidationRoot,
    password: Option<&str>,
    device_validation_secrets: Option<&RepoSecrets>,
) -> Result<RepoSecrets> {
    if let Some(password) = password {
        RepositoryFacade::new().unlock(&validation_root.path, password)?;
        return e2v_core::sync_support::open_repo_secrets_for_sync(validation_root.control_dir());
    }
    if let Some(secrets) = device_validation_secrets {
        return Ok(secrets.clone());
    }
    anyhow::bail!("fetch requires password or local device remote validation")
}

pub(crate) fn collect_remote_reachable_object_ids<R: RemoteBackend>(
    remote: &R,
    remote_loose_object_ids: &BTreeSet<String>,
    pack_locations: &BTreeMap<String, PackedObjectLocation>,
    validation_root: &RemoteValidationRoot,
    validation_secrets: &RepoSecrets,
    pack_cache: &mut BTreeMap<String, Vec<u8>>,
    head_snapshot_id: &str,
) -> Result<Vec<String>> {
    let mut traversal = RemoteReachabilityTraversal {
        remote,
        remote_loose_object_ids,
        pack_locations,
        validation_root,
        validation_secrets,
        pack_cache,
    };
    let mut reachable_object_ids = Vec::new();
    let mut recorder = VecReachabilityRecorder {
        reachable_object_ids: &mut reachable_object_ids,
        seen: HashSet::new(),
    };
    collect_remote_reachable_object_ids_with_recorder(
        &mut traversal,
        head_snapshot_id,
        &mut recorder,
    )?;
    Ok(reachable_object_ids)
}

pub(crate) fn collect_remote_reachable_object_ids_with_recorder<R: RemoteBackend>(
    traversal: &mut RemoteReachabilityTraversal<'_, R>,
    head_snapshot_id: &str,
    recorder: &mut dyn RemoteReachabilityRecorder,
) -> Result<()> {
    let object_store = DirectLayoutObjectStore::new(
        traversal.validation_root.control_dir(),
        traversal.validation_secrets.clone(),
    );
    let mut context = RemoteReachabilityContext {
        remote: traversal.remote,
        remote_loose_object_ids: traversal.remote_loose_object_ids,
        pack_locations: traversal.pack_locations,
        validation_root: traversal.validation_root,
        object_store: &object_store,
        pack_cache: traversal.pack_cache,
        recorder,
    };
    collect_remote_reachable_object_ids_with_context(&mut context, head_snapshot_id)
}

fn collect_remote_reachable_object_ids_with_context<R: RemoteBackend>(
    context: &mut RemoteReachabilityContext<'_, R>,
    head_snapshot_id: &str,
) -> Result<()> {
    let mut pending_snapshots = vec![head_snapshot_id.to_string()];

    while let Some(snapshot_id) = pending_snapshots.pop() {
        fetch_remote_object_into_validation_root(
            context.remote,
            context.remote_loose_object_ids,
            context.pack_locations,
            context.validation_root,
            context.pack_cache,
            &snapshot_id,
        )?;
        if !record_reachable_id(context.recorder, &snapshot_id)? {
            continue;
        }
        let snapshot: RemoteSnapshotObject =
            read_remote_validation_object(context.object_store, &snapshot_id, "snapshot")?;
        validate_manifest_schema_version("snapshot", snapshot.schema_version)?;
        collect_remote_tree_object_ids(context, &snapshot.root_tree_id)?;
        if let Some(parent_snapshot_id) = snapshot.parent_snapshot_id {
            validate_object_id_value(&parent_snapshot_id)
                .with_context(|| format!("invalid parent snapshot id in snapshot {snapshot_id}"))?;
            pending_snapshots.push(parent_snapshot_id);
        }
    }

    Ok(())
}

fn collect_remote_tree_object_ids<R: RemoteBackend>(
    context: &mut RemoteReachabilityContext<'_, R>,
    tree_id: &str,
) -> Result<()> {
    fetch_remote_object_into_validation_root(
        context.remote,
        context.remote_loose_object_ids,
        context.pack_locations,
        context.validation_root,
        context.pack_cache,
        tree_id,
    )?;
    if !record_reachable_id(context.recorder, tree_id)? {
        return Ok(());
    }

    match read_remote_validation_object::<RemoteTreeObject>(context.object_store, tree_id, "tree") {
        Ok(tree) => {
            validate_manifest_schema_version("tree", tree.schema_version)?;
            collect_remote_tree_entries(context, tree.entries)
        }
        Err(error) => {
            if !error.to_string().contains("object type mismatch") {
                return Err(error);
            }
            let directory_root: RemoteDirectoryRootObject =
                read_remote_validation_object(context.object_store, tree_id, "directory_root")?;
            validate_manifest_schema_version("directory_root", directory_root.schema_version)?;
            for shard_id in directory_root.shards {
                validate_object_id_value(&shard_id)
                    .with_context(|| format!("invalid shard id in directory root {tree_id}"))?;
                fetch_remote_object_into_validation_root(
                    context.remote,
                    context.remote_loose_object_ids,
                    context.pack_locations,
                    context.validation_root,
                    context.pack_cache,
                    &shard_id,
                )?;
                if !record_reachable_id(context.recorder, &shard_id)? {
                    continue;
                }
                let shard: RemoteTreeShardObject =
                    read_remote_validation_object(context.object_store, &shard_id, "tree_shard")?;
                validate_manifest_schema_version("tree_shard", shard.schema_version)?;
                collect_remote_tree_entries(context, shard.entries)?;
            }
            Ok(())
        }
    }
}

fn collect_remote_tree_entries<R: RemoteBackend>(
    context: &mut RemoteReachabilityContext<'_, R>,
    entries: Vec<RemoteTreeEntry>,
) -> Result<()> {
    for entry in entries {
        validate_object_id_value(&entry.object_id).with_context(|| {
            format!(
                "invalid {} object id in tree entry {}",
                entry.kind, entry.name
            )
        })?;
        match entry.kind.as_str() {
            "tree" => collect_remote_tree_object_ids(context, &entry.object_id)?,
            "file" => collect_remote_file_object_ids(context, &entry.object_id)?,
            other => anyhow::bail!("unknown remote tree entry kind {other}"),
        }
    }
    Ok(())
}

fn collect_remote_file_object_ids<R: RemoteBackend>(
    context: &mut RemoteReachabilityContext<'_, R>,
    file_id: &str,
) -> Result<()> {
    fetch_remote_object_into_validation_root(
        context.remote,
        context.remote_loose_object_ids,
        context.pack_locations,
        context.validation_root,
        context.pack_cache,
        file_id,
    )?;
    if !record_reachable_id(context.recorder, file_id)? {
        return Ok(());
    }

    let file: RemoteFileObject =
        read_remote_validation_object(context.object_store, file_id, "file")?;
    validate_manifest_schema_version("file", file.schema_version)?;
    for chunk_id in file.chunks {
        validate_object_id_value(&chunk_id)
            .with_context(|| format!("invalid chunk id in file {file_id}"))?;
        let _ = record_reachable_id(context.recorder, &chunk_id)?;
    }
    for shard_id in file.shard_ids {
        validate_object_id_value(&shard_id)
            .with_context(|| format!("invalid file shard id in file {file_id}"))?;
        fetch_remote_object_into_validation_root(
            context.remote,
            context.remote_loose_object_ids,
            context.pack_locations,
            context.validation_root,
            context.pack_cache,
            &shard_id,
        )?;
        if !record_reachable_id(context.recorder, &shard_id)? {
            continue;
        }
        let shard: RemoteFileShardObject =
            read_remote_validation_object(context.object_store, &shard_id, "file_shard")?;
        validate_manifest_schema_version("file_shard", shard.schema_version)?;
        for chunk_id in shard.chunks {
            validate_object_id_value(&chunk_id)
                .with_context(|| format!("invalid chunk id in file shard for {file_id}"))?;
            let _ = record_reachable_id(context.recorder, &chunk_id)?;
        }
    }
    Ok(())
}

fn materialize_remote_objects_into_validation_root<R: RemoteBackend>(
    remote: &R,
    remote_loose_object_ids: &BTreeSet<String>,
    pack_locations: &BTreeMap<String, PackedObjectLocation>,
    validation_root: &RemoteValidationRoot,
    pack_cache: &mut BTreeMap<String, Vec<u8>>,
    reachable_object_ids: &[String],
) -> Result<()> {
    for object_id in reachable_object_ids {
        fetch_remote_object_into_validation_root(
            remote,
            remote_loose_object_ids,
            pack_locations,
            validation_root,
            pack_cache,
            object_id,
        )?;
    }
    Ok(())
}

fn fetch_remote_object_into_validation_root<R: RemoteBackend>(
    remote: &R,
    remote_loose_object_ids: &BTreeSet<String>,
    pack_locations: &BTreeMap<String, PackedObjectLocation>,
    validation_root: &RemoteValidationRoot,
    pack_cache: &mut BTreeMap<String, Vec<u8>>,
    object_id: &str,
) -> Result<()> {
    validate_object_id_value(object_id)?;
    let target_path = validation_root.object_path(object_id);
    if target_path.exists() {
        return Ok(());
    }
    let bytes = remote_object_bytes_with_pack_cache(
        remote,
        remote_loose_object_ids,
        pack_locations,
        pack_cache,
        None,
        object_id,
    )?
    .with_context(|| format!("missing remote object {object_id}"))?;
    std::fs::write(target_path, bytes)?;
    Ok(())
}

pub(crate) fn read_remote_validation_object<T: for<'de> Deserialize<'de>>(
    object_store: &DirectLayoutObjectStore,
    object_id: &str,
    expected_type: &str,
) -> Result<T> {
    let plaintext = object_store.get_object(object_id, expected_type)?;
    postcard_from_bytes(&plaintext).context("failed to decode remote object plaintext")
}

fn record_reachable_id(
    recorder: &mut dyn RemoteReachabilityRecorder,
    object_id: &str,
) -> Result<bool> {
    recorder.record(object_id)
}

fn validate_manifest_schema_version(object_type: &str, schema_version: u32) -> Result<()> {
    ensure!(
        schema_version == REPO_FORMAT_VERSION,
        "unsupported {object_type} schema version {schema_version}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;

    use e2v_core::{CommitOptions, InitOptions, ManifestStore, ManifestStoreApi, RepositoryFacade};
    use e2v_store::{BlobStore, MemoryBackend, RefStore};
    use tempfile::tempdir;

    use super::*;
    use crate::oram::load_remote_active_pack_locations_with_local_cache;
    use crate::push::{PushOptions, push_head};

    #[derive(Default)]
    struct SetRecorder {
        seen: BTreeSet<String>,
    }

    impl RemoteReachabilityRecorder for SetRecorder {
        fn record(&mut self, object_id: &str) -> Result<bool> {
            Ok(self.seen.insert(object_id.to_string()))
        }
    }

    fn remote_loose_object_ids(remote: &MemoryBackend) -> BTreeSet<String> {
        remote
            .list_physical("objects/")
            .unwrap()
            .into_iter()
            .filter_map(|relative_path| {
                relative_path
                    .strip_prefix("objects/")
                    .and_then(|value| value.strip_suffix(".json"))
                    .map(str::to_string)
            })
            .collect()
    }

    #[test]
    fn collect_remote_reachable_object_ids_can_stream_into_custom_recorder() {
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
        let snapshot = facade
            .commit(CommitOptions {
                repo_root: repo_root.clone(),
                message: "seed".to_string(),
            })
            .unwrap();

        let remote = MemoryBackend::new();
        push_head(
            &facade,
            &remote,
            PushOptions {
                repo_root: repo_root.clone(),
                branch_token: state.branch.token_hex.clone(),
                operation_id: "push-op-fetch-recorder".to_string(),
            },
        )
        .unwrap();

        let default_ref = remote
            .read_ref(&e2v_store::RefToken::new(state.branch.token_hex.clone()))
            .unwrap()
            .unwrap();
        let control_dir = repo_root.join(".e2v");
        let secrets = e2v_core::sync_support::open_repo_secrets_for_sync(&control_dir).unwrap();
        let pack_locations =
            load_remote_active_pack_locations_with_local_cache(&remote, &control_dir, &secrets)
                .unwrap();
        let control_plane =
            read_remote_control_plane(&remote, default_ref.value.bytes.clone()).unwrap();
        let validation_root = RemoteValidationRoot {
            path: next_validation_root(&repo_root).unwrap(),
        };
        write_remote_control_plane_to_validation_root(&validation_root, &control_plane).unwrap();
        let remote_loose_object_ids = remote_loose_object_ids(&remote);
        let mut pack_cache = BTreeMap::new();
        let mut traversal = RemoteReachabilityTraversal {
            remote: &remote,
            remote_loose_object_ids: &remote_loose_object_ids,
            pack_locations: &pack_locations,
            validation_root: &validation_root,
            validation_secrets: &secrets,
            pack_cache: &mut pack_cache,
        };
        let mut recorder = SetRecorder::default();

        collect_remote_reachable_object_ids_with_recorder(
            &mut traversal,
            &snapshot.snapshot_id,
            &mut recorder,
        )
        .unwrap();

        let expected = ManifestStore::new(&repo_root)
            .collect_reachable_object_ids(&snapshot.snapshot_id)
            .unwrap()
            .into_iter()
            .collect::<BTreeSet<_>>();

        assert_eq!(recorder.seen, expected);
    }
}
