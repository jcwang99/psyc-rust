use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

use crate::bundle::{BundledObjectLocation, load_remote_bundle_locations, read_bundled_object};
use e2v_core::{clear_unlocked_keyring_cache, validate_layout_root_value};
use e2v_store::{RefToken, RemoteBackend};

const KEYRING_LOCK_FILE: &str = "keyring.lock";

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct KeyringPointer {
    generation: u64,
    current: String,
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

struct RemoteControlPlane {
    config_bytes: Vec<u8>,
    keyring_pointer_bytes: Vec<u8>,
    keyring_pointer: KeyringPointer,
    keyring_files: Vec<(String, Vec<u8>)>,
    layout_root_bytes: Vec<u8>,
    default_ref_bytes: Vec<u8>,
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
    let control_plane = read_remote_control_plane(remote, stored_ref.value.bytes)?;
    ensure!(
        control_plane.keyring_pointer.generation > 0
            && !control_plane.keyring_pointer.current.trim().is_empty(),
        "invalid remote keyring pointer"
    );

    let listed = remote.list_physical("objects/")?;
    let bundle_locations = load_remote_bundle_locations(remote)?;
    if matches!(
        sync_mode,
        RepositorySyncMode::SameRepositoryPointerUnchanged
            | RepositorySyncMode::SameRepositoryPointerChanged
    ) {
        validate_remote_ref_consistency_if_locally_unlocked(
            remote,
            &options.repo_root,
            &requested_branch_token,
            &stored_ref_bytes,
            &bundle_locations,
        )?;
    }
    let mut validated_remote_objects = Vec::with_capacity(listed.len());
    for relative_path in listed {
        let file_name = relative_path
            .strip_prefix("objects/")
            .ok_or_else(|| anyhow::anyhow!("invalid remote object path {relative_path}"))?;
        validate_remote_relative_name(file_name).map_err(|error| {
            anyhow::anyhow!("invalid remote object path {relative_path}: {error}")
        })?;
        let file_name = file_name.to_string();
        validated_remote_objects.push((relative_path, file_name));
    }
    let mut downloaded_objects = 0usize;
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
    for object_id in bundle_locations.keys() {
        let target_path = objects_dir.join(format!("{object_id}.json"));
        if target_path.exists() {
            match classify_local_object_health(&options.repo_root, object_id) {
                LocalObjectHealth::Verified => continue,
                LocalObjectHealth::LockedByteComparable => {
                    if let Some(bytes) = read_bundled_object(remote, &bundle_locations, object_id)?
                    {
                        if !local_object_matches_bytes(&options.repo_root, object_id, &bytes) {
                            anyhow::bail!(
                                "cannot replace locked local object {object_id} with unverified remote bytes; unlock repository first"
                            );
                        }
                    }
                    continue;
                }
                LocalObjectHealth::LockedEnvelopeInvalid => {}
                LocalObjectHealth::Unhealthy => {}
            }
        }
        if let Some(bytes) = read_bundled_object(remote, &bundle_locations, object_id)? {
            std::fs::write(&target_path, bytes)?;
            downloaded_objects += 1;
        }
    }
    if matches!(sync_mode, RepositorySyncMode::ReplaceLocalState) {
        verify_replace_local_state_with_password(
            &options.repo_root,
            &objects_dir,
            &control_plane,
            options.password.as_deref(),
        )?;
    }

    atomic_write_bytes(control_dir.join("config.json"), &control_plane.config_bytes)?;
    for (file_name, bytes) in &control_plane.keyring_files {
        atomic_write_bytes(control_dir.join("keyring").join(file_name), bytes)?;
    }
    atomic_write_bytes(
        control_dir.join("refs").join("default.json"),
        &control_plane.default_ref_bytes,
    )?;
    atomic_write_bytes(
        control_dir.join("layout_root.json"),
        &control_plane.layout_root_bytes,
    )?;
    atomic_write_bytes(
        control_dir.join("keyring").join("keyring.current"),
        &control_plane.keyring_pointer_bytes,
    )?;
    if !matches!(
        sync_mode,
        RepositorySyncMode::SameRepositoryPointerUnchanged
    ) {
        clear_unlocked_keyring_cache(&control_dir);
    }

    Ok(FetchResult { downloaded_objects })
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
    let remote_pointer_bytes = remote.get_physical("control/keyring/keyring.current")?;
    let remote_pointer: KeyringPointer = serde_json::from_slice(&remote_pointer_bytes)
        .context("failed to decode remote keyring pointer")?;

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
    for object_type in [
        "chunk",
        "snapshot",
        "tree",
        "file",
        "directory_root",
        "tree_shard",
    ] {
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
    bundle_locations: &BTreeMap<String, BundledObjectLocation>,
    object_id: &str,
) -> Result<Option<Vec<u8>>> {
    if remote.exists_physical(&format!("objects/{object_id}.json")) {
        return Ok(Some(
            remote.get_physical(&format!("objects/{object_id}.json"))?,
        ));
    }
    read_bundled_object(remote, bundle_locations, object_id)
}

fn remote_object_authenticates_for_repo<R: RemoteBackend>(
    repo_root: &Path,
    remote: &R,
    bundle_locations: &BTreeMap<String, BundledObjectLocation>,
    object_id: &str,
    expected_type: &str,
) -> bool {
    let control_dir = repo_root.join(".e2v");
    let secrets = match e2v_core::sync_support::open_repo_secrets_for_sync(&control_dir) {
        Ok(secrets) => secrets,
        Err(_) => return false,
    };
    let validation_root = match next_validation_root(repo_root) {
        Ok(path) => path,
        Err(_) => return false,
    };
    let validation_control = validation_root.join(".e2v");
    if std::fs::create_dir_all(validation_control.join("objects")).is_err() {
        return false;
    }
    if std::fs::create_dir_all(validation_control.join("keyring")).is_err() {
        let _ = std::fs::remove_dir_all(&validation_root);
        return false;
    }
    if std::fs::create_dir_all(validation_control.join("refs")).is_err() {
        let _ = std::fs::remove_dir_all(&validation_root);
        return false;
    }
    let _ = std::fs::copy(
        control_dir.join("config.json"),
        validation_control.join("config.json"),
    );
    let _ = std::fs::copy(
        control_dir.join("layout_root.json"),
        validation_control.join("layout_root.json"),
    );
    let _ = std::fs::copy(
        control_dir.join("refs").join("default.json"),
        validation_control.join("refs").join("default.json"),
    );
    if let Ok(pointer_bytes) = std::fs::read(control_dir.join("keyring").join("keyring.current")) {
        let _ = std::fs::write(
            validation_control.join("keyring").join("keyring.current"),
            pointer_bytes,
        );
    }
    let store = e2v_store::DirectLayoutObjectStore::new(&validation_control, secrets);
    let bytes = match remote_object_bytes(remote, bundle_locations, object_id) {
        Ok(Some(bytes)) => bytes,
        _ => return false,
    };
    let target_path = validation_root
        .join(".e2v")
        .join("objects")
        .join(format!("{object_id}.json"));
    if std::fs::write(&target_path, &bytes).is_err() {
        let _ = std::fs::remove_dir_all(&validation_root);
        return false;
    }
    let verified = store.get_object(object_id, expected_type).is_ok();
    let _ = std::fs::remove_dir_all(&validation_root);
    verified
}

fn remote_snapshot_graph_authenticates_for_repo<R: RemoteBackend>(
    repo_root: &Path,
    remote: &R,
    bundle_locations: &BTreeMap<String, BundledObjectLocation>,
    snapshot_id: &str,
) -> bool {
    if !remote_object_authenticates_for_repo(
        repo_root,
        remote,
        bundle_locations,
        snapshot_id,
        "snapshot",
    ) {
        return false;
    }

    let control_dir = repo_root.join(".e2v");
    let secrets = match e2v_core::sync_support::open_repo_secrets_for_sync(&control_dir) {
        Ok(secrets) => secrets,
        Err(_) => return false,
    };
    let validation_root = match next_validation_root(repo_root) {
        Ok(path) => path,
        Err(_) => return false,
    };
    let validation_control = validation_root.join(".e2v");
    if std::fs::create_dir_all(validation_control.join("objects")).is_err() {
        return false;
    }
    if std::fs::create_dir_all(validation_control.join("keyring")).is_err() {
        let _ = std::fs::remove_dir_all(&validation_root);
        return false;
    }
    if std::fs::create_dir_all(validation_control.join("refs")).is_err() {
        let _ = std::fs::remove_dir_all(&validation_root);
        return false;
    }
    if std::fs::copy(
        control_dir.join("config.json"),
        validation_control.join("config.json"),
    )
    .is_err()
    {
        let _ = std::fs::remove_dir_all(&validation_root);
        return false;
    }
    if std::fs::copy(
        control_dir.join("layout_root.json"),
        validation_control.join("layout_root.json"),
    )
    .is_err()
    {
        let _ = std::fs::remove_dir_all(&validation_root);
        return false;
    }
    if std::fs::copy(
        control_dir.join("refs").join("default.json"),
        validation_control.join("refs").join("default.json"),
    )
    .is_err()
    {
        let _ = std::fs::remove_dir_all(&validation_root);
        return false;
    }
    if let Ok(pointer_bytes) = std::fs::read(control_dir.join("keyring").join("keyring.current")) {
        if std::fs::write(
            validation_control.join("keyring").join("keyring.current"),
            pointer_bytes,
        )
        .is_err()
        {
            let _ = std::fs::remove_dir_all(&validation_root);
            return false;
        }
    }
    let listed = match remote.list_physical("objects/") {
        Ok(listed) => listed,
        Err(_) => {
            let _ = std::fs::remove_dir_all(&validation_root);
            return false;
        }
    };
    for (relative_path, file_name) in listed.into_iter().filter_map(|relative_path| {
        let file_name = relative_path.strip_prefix("objects/")?.to_string();
        Some((relative_path, file_name))
    }) {
        let target_path = validation_control.join("objects").join(file_name);
        let bytes = match remote.get_physical(&relative_path) {
            Ok(bytes) => bytes,
            Err(_) => {
                let _ = std::fs::remove_dir_all(&validation_root);
                return false;
            }
        };
        if std::fs::write(target_path, bytes).is_err() {
            let _ = std::fs::remove_dir_all(&validation_root);
            return false;
        }
    }
    for object_id in bundle_locations.keys() {
        let bytes = match read_bundled_object(remote, bundle_locations, object_id) {
            Ok(Some(bytes)) => bytes,
            _ => {
                let _ = std::fs::remove_dir_all(&validation_root);
                return false;
            }
        };
        if std::fs::write(
            validation_control
                .join("objects")
                .join(format!("{object_id}.json")),
            bytes,
        )
        .is_err()
        {
            let _ = std::fs::remove_dir_all(&validation_root);
            return false;
        }
    }
    let verified = e2v_core::sync_support::verify_snapshot_with_secrets_for_sync(
        &validation_root,
        secrets,
        snapshot_id,
    )
    .is_ok();
    let _ = std::fs::remove_dir_all(&validation_root);
    verified
}

fn read_remote_control_plane<R: RemoteBackend>(
    remote: &R,
    default_ref_bytes: Vec<u8>,
) -> Result<RemoteControlPlane> {
    let config_bytes = remote.get_physical("control/config.json")?;
    let keyring_pointer_bytes = remote.get_physical("control/keyring/keyring.current")?;
    let keyring_pointer: KeyringPointer = serde_json::from_slice(&keyring_pointer_bytes)
        .context("failed to decode remote keyring pointer")?;
    validate_remote_relative_name(&keyring_pointer.current).map_err(|error| {
        anyhow::anyhow!(
            "invalid remote keyring path {}: {error}",
            keyring_pointer.current
        )
    })?;
    let mut keyring_files = Vec::new();
    let pointed_keyring_path = format!("control/keyring/{}", keyring_pointer.current);
    let pointed_keyring_bytes = remote.get_physical(&pointed_keyring_path)?;
    let pointed_keyring_state: RemoteKeyringStateValidation =
        serde_json::from_slice(&pointed_keyring_bytes).with_context(|| {
            format!(
                "failed to decode remote keyring state {}",
                keyring_pointer.current
            )
        })?;
    keyring_files.push((keyring_pointer.current.clone(), pointed_keyring_bytes));
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
    let layout_root_bytes = serde_json::to_vec_pretty(&layout_root)?;

    Ok(RemoteControlPlane {
        config_bytes,
        keyring_pointer_bytes,
        keyring_pointer,
        keyring_files,
        layout_root_bytes,
        default_ref_bytes,
    })
}

pub(crate) fn validate_remote_branch_control_plane<R: RemoteBackend>(
    remote: &R,
    branch_token: &str,
) -> Result<()> {
    let stored_ref = remote
        .read_ref(&RefToken::new(branch_token.to_string()))?
        .ok_or_else(|| anyhow::anyhow!("remote branch ref not found"))?;
    let _ = read_remote_control_plane(remote, stored_ref.value.bytes)?;
    Ok(())
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

fn validate_remote_ref_consistency_if_locally_unlocked<R: RemoteBackend>(
    _remote: &R,
    repo_root: &Path,
    requested_branch_token: &str,
    default_ref_bytes: &[u8],
    bundle_locations: &BTreeMap<String, BundledObjectLocation>,
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
        ensure!(
            _remote.exists_physical(&format!("objects/{head_snapshot_id}.json"))
                || bundle_locations.contains_key(&head_snapshot_id),
            "remote ref points to missing head snapshot {head_snapshot_id}"
        );
        ensure!(
            remote_snapshot_graph_authenticates_for_repo(
                repo_root,
                _remote,
                bundle_locations,
                &head_snapshot_id,
            ),
            "remote ref points to unreadable head snapshot graph {head_snapshot_id}"
        );
    }

    Ok(())
}

fn verify_replace_local_state_with_password(
    repo_root: &Path,
    objects_dir: &Path,
    control_plane: &RemoteControlPlane,
    password: Option<&str>,
) -> Result<()> {
    let password = password
        .context("fetch into replacement repository requires password-based verification")?;
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
        validation_control.join("config.json"),
        &control_plane.config_bytes,
    )?;
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
    let result = facade
        .unlock(&validation_root, password)
        .and_then(|_| facade.verify_ref(&validation_root))
        .context("remote head snapshot graph failed validation");
    e2v_core::clear_unlocked_keyring_cache(&validation_control);
    let _ = std::fs::remove_dir_all(&validation_root);
    result
}

fn next_validation_root(repo_root: &Path) -> Result<PathBuf> {
    for attempt in 0..1024usize {
        let candidate = repo_root.join(format!(".e2v-fetch-validate-{attempt}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    anyhow::bail!("failed to allocate temporary fetch validation directory")
}
