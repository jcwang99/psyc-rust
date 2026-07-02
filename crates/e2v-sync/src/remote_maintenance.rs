use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use e2v_core::{RepositoryFacade, sync_support};
use e2v_store::{
    BackendCapability, RefToken, RemoteBackend, is_missing_physical_object_error,
    validate_object_id_value,
};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

use crate::fetch::{
    RemoteReachabilityRecorder, RemoteReachabilityTraversal, RemoteValidationRoot,
    assert_remote_generations_meet_local_floor, collect_remote_reachable_object_ids,
    collect_remote_reachable_object_ids_with_recorder, next_validation_root,
    read_remote_control_plane, update_trusted_remote_state_from_control_plane,
    write_remote_control_plane_to_validation_root,
};
use crate::object_type::candidate_object_types;
use crate::pack::PackedObjectLocation;
use crate::pack_index::{
    load_remote_pack_index_segment_paths, load_remote_pack_locations_with_local_cache,
};
use crate::remote_markers::{
    INTENT_EXPIRY_HOURS, marker_is_fresh_at, observe_remote_now_with_probe,
};
use e2v_store::DirectLayoutObjectStore;
use tempfile::TempDir;

const UNPUBLISHED_SNAPSHOT_GRACE_PERIOD_DAYS: u64 = 30;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyRemoteOptions {
    pub repo_root: PathBuf,
    pub sample_percent: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairRemoteOptions {
    pub repo_root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcDryRunOptions {
    pub repo_root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcExecuteOptions {
    pub repo_root: PathBuf,
    pub grace_period_days: u64,
    pub allow_single_writer_maintenance_window: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyRemoteResult {
    pub sampled_objects: usize,
    pub repaired_local_objects: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairRemoteResult {
    pub repaired_objects: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcDryRunReport {
    pub unreachable_physical_refs: Vec<String>,
    pub active_intent_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcExecuteResult {
    pub deleted_physical_refs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcExecuteCapabilityStatus {
    pub supported: bool,
    pub blockers: Vec<&'static str>,
}

struct MaintenanceReachabilityContext<'a, R: RemoteBackend> {
    remote: &'a R,
    repo_root: &'a std::path::Path,
    remote_loose_object_ids: &'a BTreeSet<String>,
    pack_locations: &'a BTreeMap<String, PackedObjectLocation>,
    validation_root: &'a RemoteValidationRoot,
    validation_secrets: &'a e2v_store::RepoSecrets,
    pack_cache: &'a mut BTreeMap<String, Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct GcDeleteJournal {
    grace_period_days: u64,
    fence_state: GcFenceState,
    pending_paths: Vec<String>,
}

struct GcReachabilityStore {
    sqlite: rusqlite::Connection,
    _temp_dir: TempDir,
}

impl GcReachabilityStore {
    fn new(repo_root: &Path) -> Result<Self> {
        let temp_dir = tempfile::Builder::new()
            .prefix("e2v-gc-reachable-")
            .tempdir_in(repo_root)
            .or_else(|_| {
                tempfile::Builder::new()
                    .prefix("e2v-gc-reachable-")
                    .tempdir()
            })?;
        let sqlite_path = temp_dir.path().join("reachable.sqlite3");
        let sqlite = rusqlite::Connection::open(&sqlite_path).with_context(|| {
            format!(
                "failed to open gc reachability sqlite {}",
                sqlite_path.display()
            )
        })?;
        sqlite.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS reachable_objects (
                object_id TEXT PRIMARY KEY NOT NULL
            );
            CREATE TABLE IF NOT EXISTS reachable_physical_refs (
                path TEXT PRIMARY KEY NOT NULL
            );
            ",
        )?;
        Ok(Self {
            sqlite,
            _temp_dir: temp_dir,
        })
    }

    fn insert_object_id(&self, object_id: &str) -> Result<bool> {
        let changed = self.sqlite.execute(
            "INSERT OR IGNORE INTO reachable_objects(object_id) VALUES (?1)",
            rusqlite::params![object_id],
        )?;
        Ok(changed > 0)
    }

    fn contains_object_id(&self, object_id: &str) -> Result<bool> {
        let exists = self
            .sqlite
            .query_row(
                "SELECT 1 FROM reachable_objects WHERE object_id = ?1 LIMIT 1",
                rusqlite::params![object_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        Ok(exists)
    }

    fn insert_physical_ref(&self, path: &str) -> Result<bool> {
        let changed = self.sqlite.execute(
            "INSERT OR IGNORE INTO reachable_physical_refs(path) VALUES (?1)",
            rusqlite::params![path],
        )?;
        Ok(changed > 0)
    }

    fn contains_physical_ref(&self, path: &str) -> Result<bool> {
        let exists = self
            .sqlite
            .query_row(
                "SELECT 1 FROM reachable_physical_refs WHERE path = ?1 LIMIT 1",
                rusqlite::params![path],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        Ok(exists)
    }

    fn clear_physical_refs(&self) -> Result<()> {
        self.sqlite
            .execute("DELETE FROM reachable_physical_refs", [])?;
        Ok(())
    }
}

impl RemoteReachabilityRecorder for GcReachabilityStore {
    fn record(&mut self, object_id: &str) -> Result<bool> {
        self.insert_object_id(object_id)
    }
}

pub fn force_accept_remote_rollback<R: RemoteBackend>(
    remote: &R,
    options: RepairRemoteOptions,
    password: &str,
) -> Result<RepairRemoteResult> {
    anyhow::ensure!(
        !password.trim().is_empty(),
        "force-accept-remote-rollback requires repository password"
    );
    let control_dir = options.repo_root.join(".e2v");
    let local_default_ref_bytes = sync_support::read_default_ref_bytes(&options.repo_root)?;
    let (branch_token, _) =
        sync_support::decode_default_ref_record(&options.repo_root, &local_default_ref_bytes)?;
    let default_ref = remote
        .read_ref(&RefToken::new(branch_token.clone()))?
        .ok_or_else(|| anyhow::anyhow!("remote branch ref not found for {branch_token}"))?;
    let (_, head_snapshot_id) =
        sync_support::decode_default_ref_record(&options.repo_root, &default_ref.value.bytes)?;

    let remote_loose_object_ids = load_remote_loose_object_ids(remote)?;
    let secrets = sync_support::open_repo_secrets_for_sync(&control_dir)?;
    let pack_locations =
        load_remote_pack_locations_with_local_cache(remote, &control_dir, Some(&secrets))?;
    let mut pack_cache = BTreeMap::new();
    preload_cached_pack_data(&control_dir, &pack_locations, &mut pack_cache)?;
    let control_plane = read_remote_control_plane(remote, default_ref.value.bytes.clone())?;
    let validation_root = RemoteValidationRoot {
        path: next_validation_root(&options.repo_root)?,
    };
    write_remote_control_plane_to_validation_root(&validation_root, &control_plane)?;
    let reachable_object_ids = match head_snapshot_id.as_deref() {
        Some(head_snapshot_id) => collect_remote_reachable_object_ids(
            remote,
            &remote_loose_object_ids,
            &pack_locations,
            &validation_root,
            &secrets,
            &mut pack_cache,
            head_snapshot_id,
        )?,
        None => Vec::new(),
    };
    persist_cached_pack_data(&control_dir, &pack_cache)?;

    let mut repaired_objects = 0usize;
    let objects_dir = control_dir.join("objects");
    std::fs::create_dir_all(&objects_dir)?;
    for object_id in &reachable_object_ids {
        let bytes = remote_object_bytes_with_pack_cache(
            remote,
            &remote_loose_object_ids,
            &pack_locations,
            &mut pack_cache,
            Some(&control_dir),
            object_id,
        )?
        .with_context(|| format!("missing remote object {object_id}"))?;
        let local_path = objects_dir.join(format!("{object_id}.json"));
        let needs_repair = std::fs::read(&local_path)
            .map(|existing| existing != bytes)
            .unwrap_or(true);
        if needs_repair {
            std::fs::write(&local_path, &bytes)?;
            repaired_objects += 1;
        }
    }

    rewrite_local_control_plane_from_remote(&control_dir, &control_plane)?;
    update_trusted_remote_state_from_control_plane(&default_ref, &control_plane)?;
    RepositoryFacade::new().unlock(&options.repo_root, password)?;
    RepositoryFacade::new().verify_ref(&options.repo_root)?;
    Ok(RepairRemoteResult { repaired_objects })
}

pub fn verify_remote<R: RemoteBackend>(
    remote: &R,
    options: VerifyRemoteOptions,
) -> Result<VerifyRemoteResult> {
    anyhow::ensure!(
        (1..=100).contains(&options.sample_percent),
        "sample percent must be between 1 and 100"
    );

    let control_dir = options.repo_root.join(".e2v");
    let local_default_ref_bytes = sync_support::read_default_ref_bytes(&options.repo_root)?;
    let (branch_token, _) =
        sync_support::decode_default_ref_record(&options.repo_root, &local_default_ref_bytes)?;
    let default_ref = remote
        .read_ref(&RefToken::new(branch_token.clone()))?
        .ok_or_else(|| anyhow::anyhow!("remote branch ref not found for {branch_token}"))?;
    let (_, head_snapshot_id) =
        sync_support::decode_default_ref_record(&options.repo_root, &default_ref.value.bytes)?;
    let Some(head_snapshot_id) = head_snapshot_id else {
        return Ok(VerifyRemoteResult {
            sampled_objects: 0,
            repaired_local_objects: 0,
        });
    };

    let remote_loose_object_ids = load_remote_loose_object_ids(remote)?;
    let secrets = sync_support::open_repo_secrets_for_sync(&control_dir)?;
    let pack_locations =
        load_remote_pack_locations_with_local_cache(remote, &control_dir, Some(&secrets))?;
    let mut pack_cache = BTreeMap::new();
    preload_cached_pack_data(&control_dir, &pack_locations, &mut pack_cache)?;
    let facade = RepositoryFacade::new();
    let mut repaired_local_objects = 0usize;

    let control_plane = read_remote_control_plane(remote, default_ref.value.bytes.clone())?;
    assert_remote_generations_meet_local_floor(&default_ref, &control_plane)?;
    let validation_root = RemoteValidationRoot {
        path: next_validation_root(&options.repo_root)?,
    };
    write_remote_control_plane_to_validation_root(&validation_root, &control_plane)?;
    let reachable_object_ids = collect_remote_reachable_object_ids(
        remote,
        &remote_loose_object_ids,
        &pack_locations,
        &validation_root,
        &secrets,
        &mut pack_cache,
        &head_snapshot_id,
    )?;
    persist_cached_pack_data(&control_dir, &pack_cache)?;
    let sampled_object_ids = sample_object_ids(&reachable_object_ids, options.sample_percent);
    let validation_object_store =
        DirectLayoutObjectStore::new(validation_root.control_dir(), secrets.clone());

    for object_id in &sampled_object_ids {
        let bytes = remote_object_bytes_with_pack_cache(
            remote,
            &remote_loose_object_ids,
            &pack_locations,
            &mut pack_cache,
            Some(&control_dir),
            object_id,
        )?
        .with_context(|| format!("missing sampled remote object {object_id}"))?;
        std::fs::write(validation_root.object_path(object_id), &bytes)?;
        let expected_type = infer_remote_object_type(&validation_object_store, object_id)?;

        let local_path = control_dir
            .join("objects")
            .join(format!("{object_id}.json"));
        let original = std::fs::read(&local_path).ok();
        std::fs::write(&local_path, &bytes)?;
        match facade.verify_object(&options.repo_root, object_id, expected_type) {
            Ok(()) => {
                if original.as_deref() != Some(bytes.as_slice()) {
                    repaired_local_objects += 1;
                }
            }
            Err(error) => {
                if let Some(original_bytes) = original {
                    std::fs::write(&local_path, original_bytes)?;
                } else {
                    let _ = std::fs::remove_file(&local_path);
                }
                return Err(error).with_context(|| {
                    format!("failed to authenticate sampled remote object {object_id}")
                });
            }
        }
    }

    update_trusted_remote_state_from_control_plane(&default_ref, &control_plane)?;
    Ok(VerifyRemoteResult {
        sampled_objects: sampled_object_ids.len(),
        repaired_local_objects,
    })
}

pub fn repair_remote<R: RemoteBackend>(
    remote: &R,
    options: RepairRemoteOptions,
) -> Result<RepairRemoteResult> {
    let control_dir = options.repo_root.join(".e2v");
    let local_default_ref_bytes = sync_support::read_default_ref_bytes(&options.repo_root)?;
    let (branch_token, _) =
        sync_support::decode_default_ref_record(&options.repo_root, &local_default_ref_bytes)?;
    let default_ref = remote
        .read_ref(&RefToken::new(branch_token.clone()))?
        .ok_or_else(|| anyhow::anyhow!("remote branch ref not found for {branch_token}"))?;
    let (_, head_snapshot_id) =
        sync_support::decode_default_ref_record(&options.repo_root, &default_ref.value.bytes)?;
    let Some(head_snapshot_id) = head_snapshot_id else {
        return Ok(RepairRemoteResult {
            repaired_objects: 0,
        });
    };

    let remote_loose_object_ids = load_remote_loose_object_ids(remote)?;
    let secrets = sync_support::open_repo_secrets_for_sync(&control_dir)?;
    let pack_locations =
        load_remote_pack_locations_with_local_cache(remote, &control_dir, Some(&secrets))?;
    let mut pack_cache = BTreeMap::new();
    preload_cached_pack_data(&control_dir, &pack_locations, &mut pack_cache)?;
    let control_plane = read_remote_control_plane(remote, default_ref.value.bytes.clone())?;
    assert_remote_generations_meet_local_floor(&default_ref, &control_plane)?;
    let validation_root = RemoteValidationRoot {
        path: next_validation_root(&options.repo_root)?,
    };
    write_remote_control_plane_to_validation_root(&validation_root, &control_plane)?;
    let reachable_object_ids = collect_remote_reachable_object_ids(
        remote,
        &remote_loose_object_ids,
        &pack_locations,
        &validation_root,
        &secrets,
        &mut pack_cache,
        &head_snapshot_id,
    )?;
    persist_cached_pack_data(&control_dir, &pack_cache)?;

    let mut repaired_objects = 0usize;
    for object_id in &reachable_object_ids {
        let bytes = remote_object_bytes_with_pack_cache(
            remote,
            &remote_loose_object_ids,
            &pack_locations,
            &mut pack_cache,
            Some(&control_dir),
            object_id,
        )?
        .with_context(|| format!("missing remote object {object_id}"))?;
        let local_path = control_dir
            .join("objects")
            .join(format!("{object_id}.json"));
        let needs_repair = std::fs::read(&local_path)
            .map(|existing| existing != bytes)
            .unwrap_or(true);
        if needs_repair {
            std::fs::write(&local_path, &bytes)?;
            repaired_objects += 1;
        }
    }

    update_trusted_remote_state_from_control_plane(&default_ref, &control_plane)?;
    Ok(RepairRemoteResult { repaired_objects })
}

pub fn gc_dry_run<R: RemoteBackend>(
    remote: &R,
    options: GcDryRunOptions,
) -> Result<GcDryRunReport> {
    let control_dir = options.repo_root.join(".e2v");
    let local_default_ref_bytes = sync_support::read_default_ref_bytes(&options.repo_root)?;
    let (branch_token, _) =
        sync_support::decode_default_ref_record(&options.repo_root, &local_default_ref_bytes)?;
    let default_ref = remote
        .read_ref(&RefToken::new(branch_token.clone()))?
        .ok_or_else(|| anyhow::anyhow!("remote branch ref not found for {branch_token}"))?;

    let remote_loose_object_ids = load_remote_loose_object_ids(remote)?;
    let secrets = sync_support::open_repo_secrets_for_sync(&control_dir)?;
    let pack_locations =
        load_remote_pack_locations_with_local_cache(remote, &control_dir, Some(&secrets))?;
    let control_plane = read_remote_control_plane(remote, default_ref.value.bytes.clone())?;
    assert_remote_generations_meet_local_floor(&default_ref, &control_plane)?;
    let validation_root = RemoteValidationRoot {
        path: next_validation_root(&options.repo_root)?,
    };
    write_remote_control_plane_to_validation_root(&validation_root, &control_plane)?;
    let mut pack_cache = BTreeMap::new();
    preload_cached_pack_data(&control_dir, &pack_locations, &mut pack_cache)?;
    let mut traversal = RemoteReachabilityTraversal {
        remote,
        remote_loose_object_ids: &remote_loose_object_ids,
        pack_locations: &pack_locations,
        validation_root: &validation_root,
        validation_secrets: &secrets,
        pack_cache: &mut pack_cache,
    };
    let mut reachable_object_ids = GcReachabilityStore::new(&options.repo_root)?;
    collect_all_remote_reachable_object_ids(
        &options.repo_root,
        &mut traversal,
        &mut reachable_object_ids,
    )?;
    let mut maintenance_context = MaintenanceReachabilityContext {
        remote,
        repo_root: &options.repo_root,
        remote_loose_object_ids: &remote_loose_object_ids,
        pack_locations: &pack_locations,
        validation_root: &validation_root,
        validation_secrets: &secrets,
        pack_cache: &mut pack_cache,
    };
    collect_recent_unpublished_local_reachable_object_ids(
        &mut maintenance_context,
        &mut reachable_object_ids,
    )?;
    persist_cached_pack_data(&control_dir, &pack_cache)?;
    let pack_index_segment_paths = load_remote_pack_index_segment_paths(remote, Some(&secrets))?;
    let active_intent_paths = list_active_intent_paths(remote)?;
    let mut unreachable_physical_refs = collect_unreachable_physical_refs_with_spill(
        remote,
        &remote_loose_object_ids,
        &pack_locations,
        &reachable_object_ids,
        &pack_index_segment_paths,
    )?;
    unreachable_physical_refs.sort();

    Ok(GcDryRunReport {
        unreachable_physical_refs,
        active_intent_paths,
    })
}

pub fn gc_execute<R: RemoteBackend>(
    remote: &R,
    options: GcExecuteOptions,
) -> Result<GcExecuteResult> {
    anyhow::ensure!(
        options.grace_period_days > 0,
        "grace period must be greater than zero"
    );
    ensure_gc_execute_capability(remote.capability())?;
    anyhow::ensure!(
        remote.capability().writer_mode() != e2v_store::WriterMode::SingleWriter
            || options.allow_single_writer_maintenance_window,
        "gc execute aborted: single-writer backends require an explicit offline maintenance window confirmation"
    );
    let journal_path = gc_delete_journal_path(&options.repo_root);
    let (before_fence, candidate_paths) = match load_gc_delete_journal(&journal_path)? {
        Some(journal) => {
            anyhow::ensure!(
                journal.grace_period_days == options.grace_period_days,
                "gc execute aborted: existing delete journal was created with a different grace period"
            );
            (journal.fence_state, journal.pending_paths)
        }
        None => {
            let before_fence = capture_gc_fence_state(remote)?;
            let report = gc_dry_run(
                remote,
                GcDryRunOptions {
                    repo_root: options.repo_root.clone(),
                },
            )?;
            anyhow::ensure!(
                report.active_intent_paths.is_empty(),
                "gc execute aborted: active intent exists"
            );
            anyhow::ensure!(
                before_fence.active_lease_paths.is_empty(),
                "gc execute aborted: writer lease exists"
            );
            store_gc_delete_journal(
                &journal_path,
                &GcDeleteJournal {
                    grace_period_days: options.grace_period_days,
                    fence_state: before_fence.clone(),
                    pending_paths: report.unreachable_physical_refs.clone(),
                },
            )?;
            (before_fence, report.unreachable_physical_refs)
        }
    };
    let after_fence = capture_gc_fence_state(remote)?;
    anyhow::ensure!(
        before_fence == after_fence,
        "gc execute aborted: remote refs, intents, or layout root changed during execution"
    );
    anyhow::ensure!(
        after_fence.active_lease_paths.is_empty(),
        "gc execute aborted: writer lease exists"
    );

    let safe_horizon =
        observed_remote_safe_horizon(remote, options.grace_period_days, &candidate_paths)?;
    let mut deleted_physical_refs = Vec::new();
    for (index, path) in candidate_paths.iter().enumerate() {
        if !physical_ref_is_older_than_horizon(remote, path, safe_horizon)? {
            continue;
        }
        if let Err(error) = remote.delete_physical(path) {
            if is_missing_physical_object_error(&error) {
                continue;
            }
            let pending_paths = candidate_paths[index..]
                .iter()
                .filter(|pending_path| remote.exists_physical(pending_path))
                .cloned()
                .collect::<Vec<_>>();
            if !pending_paths.is_empty() {
                store_gc_delete_journal(
                    &journal_path,
                    &GcDeleteJournal {
                        grace_period_days: options.grace_period_days,
                        fence_state: before_fence.clone(),
                        pending_paths,
                    },
                )?;
            }
            return Err(error);
        }
        deleted_physical_refs.push(path.clone());
    }
    remove_gc_delete_journal(&journal_path)?;

    Ok(GcExecuteResult {
        deleted_physical_refs,
    })
}

pub fn gc_execute_capability_status(capability: &BackendCapability) -> GcExecuteCapabilityStatus {
    let mut blockers = Vec::new();
    if !capability.supports_paged_list {
        blockers.push("requires reliable paged listing");
    }
    if !capability.supports_reliable_remote_time {
        blockers.push("requires reliable remote time");
    }
    if !capability.supports_remote_lock_or_lease {
        blockers.push("requires remote lock or lease");
    }
    if !capability.supports_transaction_markers {
        blockers.push("requires transaction markers");
    }
    GcExecuteCapabilityStatus {
        supported: blockers.is_empty(),
        blockers,
    }
}

fn ensure_gc_execute_capability(capability: &BackendCapability) -> Result<()> {
    let status = gc_execute_capability_status(capability);
    anyhow::ensure!(
        status.supported,
        "gc execute aborted: backend capability does not satisfy destructive gc safety requirements ({})",
        status.blockers.join(", ")
    );
    Ok(())
}

fn sample_object_ids(object_ids: &[String], sample_percent: u8) -> Vec<String> {
    if object_ids.is_empty() {
        return Vec::new();
    }
    let target = ((object_ids.len() * usize::from(sample_percent)).saturating_add(99)) / 100;
    object_ids
        .iter()
        .take(target.max(1))
        .cloned()
        .collect::<Vec<_>>()
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

fn remote_object_bytes_with_pack_cache<R: RemoteBackend>(
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
        let pack_bytes = if let Some(control_dir) = control_dir {
            if let Some(cached) =
                read_cached_pack_data_bytes(control_dir, &physical_ref.container_id)?
            {
                cached
            } else {
                let pack_len: usize = remote
                    .stat_physical(&physical_ref.container_id)?
                    .length
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("pack is too large to read on this platform"))?;
                let pack_bytes =
                    remote.get_physical_range(&physical_ref.container_id, 0, pack_len)?;
                cache_pack_data_bytes(control_dir, &physical_ref.container_id, &pack_bytes)?;
                pack_bytes
            }
        } else {
            let pack_len: usize = remote
                .stat_physical(&physical_ref.container_id)?
                .length
                .try_into()
                .map_err(|_| anyhow::anyhow!("pack is too large to read on this platform"))?;
            remote.get_physical_range(&physical_ref.container_id, 0, pack_len)?
        };
        pack_cache.insert(physical_ref.container_id.clone(), pack_bytes);
    }
    let pack_bytes = pack_cache.get(&physical_ref.container_id).unwrap();
    let offset = usize::try_from(physical_ref.offset.unwrap_or(0))
        .map_err(|_| anyhow::anyhow!("pack offset is too large to read on this platform"))?;
    let length = usize::try_from(physical_ref.length)
        .map_err(|_| anyhow::anyhow!("pack length is too large to read on this platform"))?;
    let end = offset.saturating_add(length);
    anyhow::ensure!(
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
    std::fs::write(cache_path, pack_bytes)?;
    Ok(())
}

fn read_cached_pack_data_bytes(control_dir: &Path, container_id: &str) -> Result<Option<Vec<u8>>> {
    validate_remote_relative_name(container_id)?;
    let cache_path = control_dir
        .join("cache")
        .join("pack-data")
        .join(container_id);
    match std::fs::read(cache_path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn persist_cached_pack_data(
    control_dir: &Path,
    pack_cache: &BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    for (container_id, pack_bytes) in pack_cache {
        cache_pack_data_bytes(control_dir, container_id, pack_bytes)?;
    }
    Ok(())
}

fn preload_cached_pack_data(
    control_dir: &Path,
    pack_locations: &BTreeMap<String, PackedObjectLocation>,
    pack_cache: &mut BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    for location in pack_locations.values() {
        let container_id = &location.physical_ref().container_id;
        if pack_cache.contains_key(container_id) {
            continue;
        }
        if let Some(bytes) = read_cached_pack_data_bytes(control_dir, container_id)? {
            pack_cache.insert(container_id.clone(), bytes);
        }
    }
    Ok(())
}

fn validate_remote_relative_name(value: &str) -> Result<()> {
    let path = Path::new(value);
    anyhow::ensure!(!value.is_empty(), "empty relative path");
    anyhow::ensure!(!path.is_absolute(), "path escapes target directory");
    anyhow::ensure!(
        path.components()
            .all(|component| matches!(component, Component::Normal(_))),
        "path traversal is not allowed"
    );
    Ok(())
}

fn infer_remote_object_type(
    object_store: &DirectLayoutObjectStore,
    object_id: &str,
) -> Result<&'static str> {
    for object_type in candidate_object_types(None) {
        if object_store.get_object(object_id, object_type).is_ok() {
            return Ok(object_type);
        }
    }
    anyhow::bail!("failed to infer remote object type for {object_id}")
}

fn list_active_intent_paths<R: RemoteBackend>(remote: &R) -> Result<Vec<String>> {
    list_fresh_marker_paths(
        remote,
        "transactions/active/",
        ".intent",
        INTENT_EXPIRY_HOURS,
    )
}

fn list_active_lease_paths<R: RemoteBackend>(remote: &R) -> Result<Vec<String>> {
    list_fresh_marker_paths(remote, "leases/", ".lock", INTENT_EXPIRY_HOURS)
}

fn list_fresh_marker_paths<R: RemoteBackend>(
    remote: &R,
    prefix: &str,
    suffix: &str,
    expiry_hours: u64,
) -> Result<Vec<String>> {
    let observed_now = observe_remote_now_with_probe(remote, ".e2v/gc-remote-time.probe")?;
    let mut intent_paths = remote
        .list_physical(prefix)?
        .into_iter()
        .filter(|path| path.ends_with(suffix))
        .filter(|path| {
            marker_is_fresh_at(remote, path, observed_now, expiry_hours).unwrap_or(false)
        })
        .collect::<Vec<_>>();
    intent_paths.sort();
    Ok(intent_paths)
}

fn rewrite_local_control_plane_from_remote(
    control_dir: &std::path::Path,
    control_plane: &crate::fetch::RemoteControlPlane,
) -> Result<()> {
    std::fs::create_dir_all(control_dir.join("keyring"))?;
    std::fs::create_dir_all(control_dir.join("refs"))?;
    std::fs::create_dir_all(control_dir.join("refs").join("branches"))?;
    for (file_name, bytes) in &control_plane.keyring_files {
        std::fs::write(control_dir.join("keyring").join(file_name), bytes)?;
    }
    std::fs::write(
        control_dir.join("refs").join("default.json"),
        &control_plane.default_ref_bytes,
    )?;
    let repo_root = control_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("control directory has no repository root"))?;
    let secrets = sync_support::open_repo_secrets_for_sync(control_dir)?;
    let (branch_token, _) =
        sync_support::decode_default_ref_record(repo_root, &control_plane.default_ref_bytes)?;
    let branch_plaintext = sync_support::decrypt_control_record_for_sync(
        &secrets,
        "default",
        "ref",
        &control_plane.default_ref_bytes,
    )?;
    let branch_ref_bytes = sync_support::encrypt_control_record_for_sync(
        &secrets,
        &format!("branch-ref:{branch_token}"),
        "ref",
        &branch_plaintext,
    )?;
    let branch_ref_path = control_dir
        .join("refs")
        .join("branches")
        .join(format!("{branch_token}.json"));
    std::fs::write(branch_ref_path, branch_ref_bytes)?;
    std::fs::write(
        control_dir.join("layout_root.json"),
        &control_plane.layout_root_bytes,
    )?;
    std::fs::write(
        control_dir.join("keyring").join("keyring.current"),
        &control_plane.keyring_pointer_bytes,
    )?;
    e2v_core::clear_unlocked_keyring_cache(control_dir);
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct GcFenceState {
    refs: Vec<(String, u64, Vec<u8>)>,
    active_intent_paths: Vec<String>,
    active_lease_paths: Vec<String>,
    layout_root_generation: u64,
    layout_root_bytes: Vec<u8>,
    pack_index_root_bytes: Option<Vec<u8>>,
}

fn gc_delete_journal_path(repo_root: &std::path::Path) -> PathBuf {
    repo_root
        .join(".e2v")
        .join("journal")
        .join("gc")
        .join("gc-execute.json")
}

fn load_gc_delete_journal(path: &std::path::Path) -> Result<Option<GcDeleteJournal>> {
    if !path.is_file() {
        return Ok(None);
    }
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read gc delete journal {}", path.display()))?;
    let journal = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to decode gc delete journal {}", path.display()))?;
    Ok(Some(journal))
}

fn store_gc_delete_journal(path: &std::path::Path, journal: &GcDeleteJournal) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("gc delete journal path has no parent"))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create gc journal dir {}", parent.display()))?;
    std::fs::write(path, serde_json::to_vec(journal)?)
        .with_context(|| format!("failed to write gc delete journal {}", path.display()))?;
    Ok(())
}

fn remove_gc_delete_journal(path: &std::path::Path) -> Result<()> {
    if path.exists() {
        std::fs::remove_file(path)
            .with_context(|| format!("failed to remove gc delete journal {}", path.display()))?;
    }
    Ok(())
}

fn collect_all_remote_reachable_object_ids<R: RemoteBackend>(
    repo_root: &std::path::Path,
    traversal: &mut RemoteReachabilityTraversal<'_, R>,
    reachable: &mut dyn RemoteReachabilityRecorder,
) -> Result<()> {
    let local_default_ref_bytes = sync_support::read_default_ref_bytes(repo_root)?;
    let (default_branch_token, _) =
        sync_support::decode_default_ref_record(repo_root, &local_default_ref_bytes)?;
    let mut saw_head_snapshot = false;
    for listed_ref in list_remote_branch_refs(traversal.remote, repo_root)? {
        let (decoded_branch_token, head_snapshot_id) =
            sync_support::decode_default_ref_record(repo_root, &listed_ref.stored.value.bytes)
                .with_context(|| {
                    format!(
                        "failed to decode remote branch ref {}",
                        listed_ref.token.value
                    )
                })?;
        anyhow::ensure!(
            decoded_branch_token == listed_ref.token.value,
            "remote ref token mismatch: listed {}, decoded {}",
            listed_ref.token.value,
            decoded_branch_token
        );
        let Some(head_snapshot_id) = head_snapshot_id else {
            continue;
        };
        saw_head_snapshot = true;
        collect_remote_reachable_object_ids_with_recorder(traversal, &head_snapshot_id, reachable)?;
    }

    if !saw_head_snapshot {
        let default_ref = traversal
            .remote
            .read_ref(&RefToken::new(default_branch_token.clone()))?
            .ok_or_else(|| {
                anyhow::anyhow!("remote branch ref not found for {default_branch_token}")
            })?;
        let (_, head_snapshot_id) =
            sync_support::decode_default_ref_record(repo_root, &default_ref.value.bytes)?;
        if let Some(head_snapshot_id) = head_snapshot_id {
            collect_remote_reachable_object_ids_with_recorder(
                traversal,
                &head_snapshot_id,
                reachable,
            )?;
        }
    }

    Ok(())
}

fn collect_recent_unpublished_local_reachable_object_ids<R: RemoteBackend>(
    context: &mut MaintenanceReachabilityContext<'_, R>,
    reachable_object_ids: &mut GcReachabilityStore,
) -> Result<()> {
    let facade = RepositoryFacade::new();
    let mut candidate_head_snapshot_ids = BTreeSet::new();
    for branch in facade.list_branches(context.repo_root)? {
        let Some(snapshot_id) = branch.head_snapshot_id else {
            continue;
        };
        if !reachable_object_ids.contains_object_id(&snapshot_id)? {
            candidate_head_snapshot_ids.insert(snapshot_id);
        }
    }
    if candidate_head_snapshot_ids.is_empty() {
        return Ok(());
    }

    let candidate_paths = candidate_head_snapshot_ids
        .iter()
        .filter_map(|snapshot_id| {
            remote_physical_path_for_object(
                context.remote_loose_object_ids,
                context.pack_locations,
                snapshot_id,
            )
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if candidate_paths.is_empty() {
        return Ok(());
    }

    let unpublished_safe_horizon = observed_remote_safe_horizon(
        context.remote,
        UNPUBLISHED_SNAPSHOT_GRACE_PERIOD_DAYS,
        &candidate_paths,
    )?;
    candidate_head_snapshot_ids.retain(|snapshot_id| {
        let Some(path) = remote_physical_path_for_object(
            context.remote_loose_object_ids,
            context.pack_locations,
            snapshot_id,
        ) else {
            return false;
        };
        !physical_ref_is_older_than_horizon(context.remote, &path, unpublished_safe_horizon)
            .unwrap_or(false)
    });
    if candidate_head_snapshot_ids.is_empty() {
        return Ok(());
    }

    for snapshot_id in candidate_head_snapshot_ids {
        let mut traversal = RemoteReachabilityTraversal {
            remote: context.remote,
            remote_loose_object_ids: context.remote_loose_object_ids,
            pack_locations: context.pack_locations,
            validation_root: context.validation_root,
            validation_secrets: context.validation_secrets,
            pack_cache: context.pack_cache,
        };
        collect_remote_reachable_object_ids_with_recorder(
            &mut traversal,
            &snapshot_id,
            reachable_object_ids,
        )
        .with_context(|| {
            format!(
                "failed to collect remote graph for recent unpublished local snapshot {snapshot_id}"
            )
        })?;
    }
    Ok(())
}

fn collect_unreachable_physical_refs_with_spill<R: RemoteBackend>(
    remote: &R,
    remote_loose_object_ids: &BTreeSet<String>,
    pack_locations: &BTreeMap<String, PackedObjectLocation>,
    reachable: &GcReachabilityStore,
    pack_index_segment_paths: &[String],
) -> Result<Vec<String>> {
    reachable.clear_physical_refs()?;
    for object_id in remote_loose_object_ids {
        if reachable.contains_object_id(object_id)? {
            reachable.insert_physical_ref(&format!("objects/{object_id}.json"))?;
        }
    }
    for (object_id, location) in pack_locations {
        if reachable.contains_object_id(object_id)? {
            reachable.insert_physical_ref(&location.physical_ref().container_id)?;
        }
    }
    for segment_path in pack_index_segment_paths {
        reachable.insert_physical_ref(segment_path)?;
    }

    let mut candidates = remote
        .list_physical("objects/")?
        .into_iter()
        .filter(|path| {
            path.strip_prefix("objects/")
                .and_then(|value| value.strip_suffix(".json"))
                .map(|object_id| validate_object_id_value(object_id).is_ok())
                .unwrap_or(false)
        })
        .collect::<Vec<_>>();
    candidates.extend(remote.list_physical("packs/data/")?);
    candidates.extend(remote.list_physical("packs/index/")?);
    candidates.extend(remote.list_physical("pack-index/segments/")?);

    let mut unreachable = Vec::new();
    for path in candidates {
        if !reachable.contains_physical_ref(&path)? {
            unreachable.push(path);
        }
    }
    Ok(unreachable)
}

fn remote_physical_path_for_object(
    remote_loose_object_ids: &BTreeSet<String>,
    pack_locations: &BTreeMap<String, PackedObjectLocation>,
    object_id: &str,
) -> Option<String> {
    if remote_loose_object_ids.contains(object_id) {
        return Some(format!("objects/{object_id}.json"));
    }
    pack_locations
        .get(object_id)
        .map(|location| location.physical_ref().container_id)
}

fn capture_gc_fence_state<R: RemoteBackend>(remote: &R) -> Result<GcFenceState> {
    let refs = remote
        .list_refs()?
        .into_iter()
        .filter(|entry| !entry.token.value.starts_with("keyring/"))
        .map(|entry| {
            (
                entry.token.value,
                entry.stored.version.value,
                entry.stored.value.bytes,
            )
        })
        .collect::<Vec<_>>();
    let active_intent_paths = list_active_intent_paths(remote)?;
    let active_lease_paths = list_active_lease_paths(remote)?;
    let layout_root_bytes = remote.get_physical("layout_root.json")?;
    let layout_root_generation = remote.read_layout_root()?.generation;
    let pack_index_root_bytes = match remote.get_physical("pack-index/root.json") {
        Ok(bytes) => Some(bytes),
        Err(error) if is_missing_physical_object_error(&error) => None,
        Err(error) => return Err(error),
    };
    Ok(GcFenceState {
        refs,
        active_intent_paths,
        active_lease_paths,
        layout_root_generation,
        layout_root_bytes,
        pack_index_root_bytes,
    })
}

fn list_remote_branch_refs<R: RemoteBackend>(
    remote: &R,
    repo_root: &std::path::Path,
) -> Result<Vec<e2v_store::ListedRef>> {
    let mut refs = Vec::new();
    for listed_ref in remote.list_refs()? {
        if listed_ref.token.value.starts_with("keyring/") {
            continue;
        }
        if sync_support::decode_default_ref_record(repo_root, &listed_ref.stored.value.bytes)
            .is_ok()
        {
            refs.push(listed_ref);
        }
    }
    Ok(refs)
}

fn observed_remote_safe_horizon<R: RemoteBackend>(
    remote: &R,
    grace_period_days: u64,
    candidate_paths: &[String],
) -> Result<std::time::SystemTime> {
    let observed_now = observed_remote_latest_time(remote, candidate_paths)?;
    let grace_period =
        std::time::Duration::from_secs(grace_period_days.saturating_mul(24 * 60 * 60));
    observed_now
        .checked_sub(grace_period)
        .ok_or_else(|| anyhow::anyhow!("gc execute aborted: invalid grace period horizon"))
}

fn observed_remote_latest_time<R: RemoteBackend>(
    remote: &R,
    candidate_paths: &[String],
) -> Result<std::time::SystemTime> {
    let mut observed_times = Vec::new();
    observed_times.push(
        remote
            .stat_physical("layout_root.json")?
            .modified_at
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "gc execute aborted: layout root has no reliable remote modification time"
                )
            })?,
    );
    match remote.stat_physical("pack-index/root.json") {
        Ok(stat) => observed_times.push(stat.modified_at.ok_or_else(|| {
            anyhow::anyhow!(
                "gc execute aborted: pack index root has no reliable remote modification time"
            )
        })?),
        Err(error) if is_missing_physical_object_error(&error) => {}
        Err(error) => return Err(error),
    }
    for path in remote.list_physical("transactions/active/")? {
        if !path.ends_with(".intent") {
            continue;
        }
        observed_times.push(remote.stat_physical(&path)?.modified_at.ok_or_else(|| {
            anyhow::anyhow!(
                "gc execute aborted: active intent {path} has no reliable remote modification time"
            )
        })?);
    }
    for path in remote.list_physical("leases/")? {
        if !path.ends_with(".lock") {
            continue;
        }
        observed_times.push(remote.stat_physical(&path)?.modified_at.ok_or_else(|| {
            anyhow::anyhow!(
                "gc execute aborted: writer lease {path} has no reliable remote modification time"
            )
        })?);
    }
    for path in candidate_paths {
        let modified_at = match remote.stat_physical(path) {
            Ok(stat) => stat.modified_at.ok_or_else(|| {
                anyhow::anyhow!(
                    "gc execute aborted: physical ref {path} has no reliable remote modification time"
                )
            })?,
            Err(error) if is_missing_physical_object_error(&error) => continue,
            Err(error) => return Err(error),
        };
        observed_times.push(modified_at);
    }
    let observed_now = observed_times
        .into_iter()
        .max()
        .ok_or_else(|| anyhow::anyhow!("gc execute aborted: failed to observe remote clock"))?;
    Ok(observed_now)
}

fn physical_ref_is_older_than_horizon<R: RemoteBackend>(
    remote: &R,
    path: &str,
    safe_horizon: std::time::SystemTime,
) -> Result<bool> {
    let modified_at = match remote.stat_physical(path) {
        Ok(stat) => stat.modified_at.ok_or_else(|| {
            anyhow::anyhow!(
                "gc execute aborted: physical ref {path} has no reliable remote modification time"
            )
        })?,
        Err(error) if is_missing_physical_object_error(&error) => return Ok(false),
        Err(error) => return Err(error),
    };
    Ok(modified_at <= safe_horizon)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use tempfile::tempdir;

    use super::*;
    use e2v_store::{BlobStore, MemoryBackend};

    fn object_id(fill: char) -> String {
        std::iter::repeat_n(fill, 64).collect()
    }

    #[test]
    fn gc_reachability_store_deduplicates_object_and_physical_ids() {
        let temp = tempdir().unwrap();
        let store = GcReachabilityStore::new(temp.path()).unwrap();

        assert!(store.insert_object_id(&object_id('a')).unwrap());
        assert!(!store.insert_object_id(&object_id('a')).unwrap());
        assert!(store.contains_object_id(&object_id('a')).unwrap());
        assert!(!store.contains_object_id(&object_id('b')).unwrap());

        assert!(store.insert_physical_ref("objects/test.json").unwrap());
        assert!(!store.insert_physical_ref("objects/test.json").unwrap());
        assert!(store.contains_physical_ref("objects/test.json").unwrap());

        store.clear_physical_refs().unwrap();
        assert!(!store.contains_physical_ref("objects/test.json").unwrap());
    }

    #[test]
    fn spilled_reachable_ids_can_drive_unreachable_physical_ref_collection() {
        let temp = tempdir().unwrap();
        let store = GcReachabilityStore::new(temp.path()).unwrap();
        let remote = MemoryBackend::new();
        let reachable_loose = object_id('a');
        let unreachable_loose = object_id('b');
        let packed_object = object_id('c');
        let pack_data_path = "packs/data/pack-00000001.bin".to_string();

        remote
            .put_physical(
                &format!("objects/{reachable_loose}.json"),
                br#"{"reachable":true}"#,
            )
            .unwrap();
        remote
            .put_physical(
                &format!("objects/{unreachable_loose}.json"),
                br#"{"reachable":false}"#,
            )
            .unwrap();
        remote
            .put_physical(&pack_data_path, b"packed-object")
            .unwrap();

        let remote_loose_object_ids =
            BTreeSet::from([reachable_loose.clone(), unreachable_loose.clone()]);
        let pack_locations = BTreeMap::from([(
            packed_object.clone(),
            PackedObjectLocation {
                data_path: pack_data_path.clone(),
                offset: 0,
                length: 13,
            },
        )]);

        store.insert_object_id(&reachable_loose).unwrap();
        store.insert_object_id(&packed_object).unwrap();

        let unreachable = collect_unreachable_physical_refs_with_spill(
            &remote,
            &remote_loose_object_ids,
            &pack_locations,
            &store,
            &[],
        )
        .unwrap();

        assert_eq!(
            unreachable,
            vec![format!("objects/{unreachable_loose}.json")]
        );
    }

    #[test]
    fn gc_delete_journal_is_stored_as_compact_json() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("gc-execute.json");
        let journal = GcDeleteJournal {
            grace_period_days: 30,
            fence_state: GcFenceState {
                refs: vec![("branch/main".to_string(), 7, b"ref-bytes".to_vec())],
                active_intent_paths: vec!["intents/intent-1.json".to_string()],
                active_lease_paths: vec!["leases/writer-1.json".to_string()],
                layout_root_generation: 11,
                layout_root_bytes: br#"{"layout":"root"}"#.to_vec(),
                pack_index_root_bytes: Some(
                    br#"{"segments":["packs/index/op-00000000.json"]}"#.to_vec(),
                ),
            },
            pending_paths: vec![
                "objects/a.json".to_string(),
                "packs/index/op-00000000.json".to_string(),
            ],
        };

        store_gc_delete_journal(&path, &journal).unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(
            bytes,
            serde_json::to_vec(&journal).unwrap(),
            "gc delete journal should not store pretty-printed JSON whitespace"
        );
    }
}
