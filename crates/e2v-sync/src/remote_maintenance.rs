use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

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
    collect_remote_reachable_object_ids_with_recorder, fetch_remote_object_into_validation_root,
    next_validation_root, read_remote_control_plane,
    update_trusted_remote_state_from_control_plane, write_remote_control_plane_to_validation_root,
};
use crate::journal::{OperationId, OperationJournal, RewriteJournalState};
use crate::object_type::candidate_object_types;
use crate::oram::{
    load_remote_active_pack_locations_with_local_cache, load_remote_active_pack_segment_paths,
};
use crate::pack::PackedObjectLocation;
use crate::pack_cache::{
    cache_pack_data_bytes, prune_stale_cached_pack_data, remote_object_bytes_with_pack_cache,
};
use crate::pack_index::{next_pack_index_segment_paths, publish_pack_index_root};
use crate::publisher::{SimpleTransactionPublisher, TransactionPublisher};
use crate::push::{
    cleanup_completed_operation_markers, list_operation_pack_index_segment_paths,
    mirror_remote_keyring_pointer, publish_remote_keyring_pointer_with_retry,
    read_remote_current_keyring_bytes, reconcile_local_keyring_with_remote_if_needed,
    upload_objects_as_pack_segments, upload_remote_keyring_generations,
};
use crate::remote_markers::{
    INTENT_EXPIRY_HOURS, marker_is_fresh_at, observe_remote_now_with_probe,
};
use crate::transaction::{PublishPlan, PublishSession};
use e2v_store::DirectLayoutObjectStore;
use tempfile::TempDir;

const UNPUBLISHED_SNAPSHOT_GRACE_PERIOD_DAYS: u64 = 30;
const HISTORY_REWRITE_CHECKPOINT_FILE: &str = "history-rewrite.checkpoint";
const HISTORY_REWRITE_CHECKPOINT_BACKUP_FILE: &str = "history-rewrite.checkpoint.bak";

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
pub struct HistoricalRewritePlanOptions {
    pub repo_root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoricalRewriteOptions {
    pub repo_root: PathBuf,
    pub password: String,
    pub confirm_full_reencryption: bool,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoricalRewritePlan {
    pub reachable_object_count: usize,
    pub remote_loose_object_count: usize,
    pub remote_pack_object_count: usize,
    pub old_epoch_count: usize,
    pub large_repo_advisory: Option<String>,
    pub requires_remote_credential_revocation_guidance: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoricalRewriteResult {
    pub rewritten_objects: usize,
    pub retired_epoch_count: usize,
    pub pending_gc_stale_remote_refs: Vec<String>,
    pub next_layout_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct HistoricalRewriteCheckpoint {
    stage: String,
    target_layout_generation: u64,
    rewritten_object_ids: Vec<String>,
    retired_epoch_count: usize,
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

    fn object_count(&self) -> Result<usize> {
        let count = self
            .sqlite
            .query_row("SELECT COUNT(*) FROM reachable_objects", [], |row| {
                row.get::<_, i64>(0)
            })?;
        Ok(count.max(0) as usize)
    }
}

impl RemoteReachabilityRecorder for GcReachabilityStore {
    fn record(&mut self, object_id: &str) -> Result<bool> {
        self.insert_object_id(object_id)
    }
}

pub fn plan_historical_rewrite<R: RemoteBackend>(
    remote: &R,
    options: HistoricalRewritePlanOptions,
) -> Result<HistoricalRewritePlan> {
    let facade = RepositoryFacade::new();
    facade.open(&options.repo_root)?;
    let control_dir = options.repo_root.join(".e2v");
    let keyring_pointer: serde_json::Value = serde_json::from_slice(&std::fs::read(
        control_dir.join("keyring").join("keyring.current"),
    )?)
    .context("failed to decode current keyring pointer")?;
    let current_keyring_name = keyring_pointer["current"]
        .as_str()
        .context("keyring pointer missing current generation file")?;
    let keyring: serde_json::Value = serde_json::from_slice(&std::fs::read(
        control_dir.join("keyring").join(current_keyring_name),
    )?)
    .context("failed to decode current keyring state")?;
    let local_old_epoch_count =
        keyring_epoch_count(&keyring, "current local keyring state")?.saturating_sub(1);
    let remote_old_epoch_count =
        match crate::push::read_remote_current_keyring_bytes(remote, &options.repo_root)? {
            Some(bytes) => Some(
                keyring_epoch_count(
                    &serde_json::from_slice::<serde_json::Value>(&bytes)
                        .context("failed to decode remote current keyring state")?,
                    "remote current keyring state",
                )?
                .saturating_sub(1),
            ),
            None => None,
        };
    let old_epoch_count = remote_old_epoch_count
        .map(|remote_old_epoch_count| remote_old_epoch_count.max(local_old_epoch_count))
        .unwrap_or(local_old_epoch_count);
    let rewrite_checkpoint = load_historical_rewrite_checkpoint(&control_dir)?;
    let reachable_object_count = if let Some(rewrite_checkpoint) = rewrite_checkpoint {
        rewrite_checkpoint.rewritten_object_ids.len()
    } else {
        let remote_loose_object_ids = load_remote_loose_object_ids(remote)?;
        let secrets = sync_support::open_or_unlock_repo_secrets_for_sync(&control_dir)?;
        let pack_locations =
            load_remote_active_pack_locations_with_local_cache(remote, &control_dir, &secrets)?;
        let validation_root = RemoteValidationRoot {
            path: next_validation_root(&options.repo_root)?,
            cache_control_dir: Some(control_dir.clone()),
        };
        let mut pack_cache = BTreeMap::new();
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
        reachable_object_ids.object_count()?
    };
    let secrets = sync_support::open_or_unlock_repo_secrets_for_sync(&control_dir)?;
    let pack_locations =
        load_remote_active_pack_locations_with_local_cache(remote, &control_dir, &secrets)?;
    let remote_loose_object_count = load_remote_loose_object_ids(remote)?.len();
    let remote_pack_object_count = pack_locations.len();
    let large_repo_advisory = if reachable_object_count >= 10_000
        || remote_loose_object_count >= 10_000
        || remote_pack_object_count >= 10_000
    {
        Some(
            "Large repository detected; revoke remote storage credentials first and treat full repository re-encryption as an expensive maintenance action."
                .to_string(),
        )
    } else {
        None
    };
    let requires_remote_credential_revocation_guidance =
        old_epoch_count > 0 || large_repo_advisory.is_some();
    Ok(HistoricalRewritePlan {
        reachable_object_count,
        remote_loose_object_count,
        remote_pack_object_count,
        old_epoch_count,
        large_repo_advisory,
        requires_remote_credential_revocation_guidance,
    })
}

fn keyring_epoch_count(keyring: &serde_json::Value, context: &str) -> Result<usize> {
    keyring["epochs"]
        .as_array()
        .map(|epochs| epochs.len())
        .ok_or_else(|| anyhow::anyhow!("{context} is missing an epochs array"))
}

pub fn historical_rewrite_remote<R: RemoteBackend + Clone>(
    remote: &R,
    options: HistoricalRewriteOptions,
) -> Result<HistoricalRewriteResult> {
    anyhow::ensure!(
        options.confirm_full_reencryption,
        "historical rewrite requires explicit full re-encryption confirmation"
    );
    anyhow::ensure!(
        !options.password.trim().is_empty(),
        "historical rewrite password must not be empty"
    );
    let control_dir = options.repo_root.join(".e2v");
    let operation_id = OperationId::new("history-rewrite".to_string())?;
    let journal = OperationJournal::new(control_dir.join("journal").join("sync"))?;
    let facade = RepositoryFacade::new();
    reconcile_local_keyring_with_remote_if_needed(remote, &options.repo_root)?;
    let historical_segment_secrets = read_remote_current_keyring_bytes(remote, &options.repo_root)?
        .map(|bytes| {
            sync_support::unlock_repo_secrets_from_keyring_bytes_for_sync(&bytes, &options.password)
        })
        .transpose()?;
    let mut rewrite_state = load_historical_rewrite_checkpoint(&control_dir)?;
    let resuming_rewrite = rewrite_state.is_some();
    let current_repo_state = facade.open(&options.repo_root)?;
    if rewrite_state.is_none() {
        let current_layout_root_bytes = sync_support::read_layout_root_bytes(&options.repo_root)?;
        let current_default_ref_bytes = sync_support::read_default_ref_bytes(&options.repo_root)?;
        let current_plan = plan_historical_rewrite(
            remote,
            HistoricalRewritePlanOptions {
                repo_root: options.repo_root.clone(),
            },
        )?;
        let remote_layout_matches_local = remote
            .get_physical("layout_root.json")
            .map(|bytes| bytes == current_layout_root_bytes)
            .unwrap_or(false);
        let remote_ref_matches_local_before_rewrite = remote
            .read_ref(&RefToken::new(current_repo_state.branch.token_hex.clone()))?
            .map(|stored_ref| stored_ref.value.bytes == current_default_ref_bytes)
            .unwrap_or(false);
        if remote_layout_matches_local
            && remote_ref_matches_local_before_rewrite
            && current_plan.old_epoch_count == 0
        {
            let pending_gc_stale_remote_refs = gc_dry_run(
                remote,
                GcDryRunOptions {
                    repo_root: options.repo_root.clone(),
                },
            )
            .map(|report| {
                report
                    .unreachable_physical_refs
                    .into_iter()
                    .filter(|path| path.starts_with("objects/"))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
            remove_local_index_if_present(&options.repo_root)?;
            return Ok(HistoricalRewriteResult {
                rewritten_objects: current_plan.reachable_object_count,
                retired_epoch_count: 0,
                pending_gc_stale_remote_refs,
                next_layout_generation: current_repo_state.layout_generation,
            });
        }
    }
    if rewrite_state.is_none() {
        hydrate_remote_branch_history_for_rewrite(remote, &options.repo_root)?;
        let local_result =
            facade.rewrite_history_to_active_epoch(&options.repo_root, &options.password)?;
        let state = RewriteJournalState {
            stage: "local_rewrite_completed".to_string(),
            target_layout_generation: facade.open(&options.repo_root)?.layout_generation,
            rewritten_object_ids: local_result.rewritten_object_ids.clone(),
            retired_epoch_count: local_result.retired_epoch_count,
        };
        store_historical_rewrite_checkpoint(&control_dir, &state)?;
        rewrite_state = Some(state);
    }
    let rewrite_state = rewrite_state
        .ok_or_else(|| anyhow::anyhow!("historical rewrite checkpoint was not initialized"))?;
    let repo_state = facade.open(&options.repo_root)?;
    let keyring_files = sync_support::list_keyring_files(&options.repo_root)?;
    let layout_root_bytes = sync_support::read_layout_root_bytes(&options.repo_root)?;
    let layout_root: e2v_store::LayoutRoot = serde_json::from_slice(&layout_root_bytes)?;
    let default_ref_bytes = sync_support::read_default_ref_bytes(&options.repo_root)?;
    let current_remote_ref =
        remote.read_ref(&RefToken::new(repo_state.branch.token_hex.clone()))?;
    let remote_ref_matches_local = current_remote_ref
        .as_ref()
        .map(|stored_ref| stored_ref.value.bytes.as_slice() == default_ref_bytes.as_slice())
        .unwrap_or(false);
    let local_branch_refs = collect_local_branch_ref_bytes(&options.repo_root)?;
    let remote_branch_refs = list_remote_branch_refs(remote, &options.repo_root)?;

    let remote_loose_object_ids = load_remote_loose_object_ids(remote)?;
    let stale_loose_paths = rewrite_state
        .rewritten_object_ids
        .iter()
        .filter(|object_id| remote_loose_object_ids.contains(*object_id))
        .map(|object_id| format!("objects/{object_id}.json"))
        .collect::<Vec<_>>();
    let mut stale_loose_paths = stale_loose_paths;
    stale_loose_paths.sort();
    let pack_index_secrets = sync_support::open_or_unlock_repo_secrets_for_sync(&control_dir)?;
    let publisher = SimpleTransactionPublisher::new(
        remote.capability().clone(),
        journal.clone(),
        (*remote).clone(),
    );
    let session = publisher.begin(PublishPlan {
        operation_id: operation_id.clone(),
        target_branch_token: repo_state.branch.token_hex.clone(),
        expected_ref_version: current_remote_ref
            .as_ref()
            .map(|stored| stored.version.value),
        planned_snapshot_id: sync_support::decode_ref_head_snapshot_id(
            &options.repo_root,
            &default_ref_bytes,
        )?,
        writer_mode: remote.capability().push_write_mode(),
    })?;
    let session = PublishSession {
        next_layout_root: Some(layout_root.clone()),
        next_layout_root_bytes: Some(layout_root_bytes.clone()),
        ..session
    };

    let published_pack_index_segments = upload_rewrite_segments_with_resume(
        remote,
        &options.repo_root,
        &operation_id,
        &rewrite_state.rewritten_object_ids,
        &publisher,
        &session,
    )?;

    upload_remote_keyring_generations(remote, &keyring_files)?;
    publisher.publish_layout_if_needed(&session)?;
    if !published_pack_index_segments.is_empty() {
        let rewritten_object_ids = rewrite_state
            .rewritten_object_ids
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let retained_segments = retained_active_segment_paths_for_unrewritten_objects(
            remote,
            &control_dir,
            historical_segment_secrets.as_ref(),
            &rewritten_object_ids,
        )?;
        let mut active_segment_paths = retained_segments;
        for segment_path in &published_pack_index_segments {
            if !active_segment_paths.contains(segment_path) {
                active_segment_paths.push(segment_path.clone());
            }
        }
        if !resuming_rewrite && active_segment_paths.is_empty() {
            active_segment_paths = next_pack_index_segment_paths(
                remote,
                &published_pack_index_segments,
                Some(&pack_index_secrets),
            )?;
        }
        publish_pack_index_root(
            remote,
            &pack_index_secrets,
            &layout_root.layout_id,
            layout_root.generation,
            active_segment_paths,
        )?;
    }
    publisher.pre_commit_verify(&session)?;
    let pointer_bytes = publish_remote_keyring_pointer_with_retry(remote, &options.repo_root)?;
    if !remote_ref_matches_local {
        let publish_result =
            publisher.publish_ref(&session, e2v_store::EncryptedRef::new(default_ref_bytes))?;
        if !publish_result.applied {
            anyhow::bail!("historical rewrite requires needs-rebase recovery");
        }
    }
    publish_remote_branch_refs(
        remote,
        &local_branch_refs,
        &remote_branch_refs,
        &repo_state.branch.token_hex,
    )?;
    mirror_remote_keyring_pointer(remote, &pointer_bytes)?;
    publisher.complete(session)?;
    cleanup_completed_operation_markers(remote, &operation_id, &repo_state.branch.token_hex)?;
    clear_historical_rewrite_checkpoint(&control_dir)?;
    remove_local_index_if_present(&options.repo_root)?;
    let rewritten_objects = rewrite_state.rewritten_object_ids.len();
    let post_rewrite_plan = plan_historical_rewrite(
        remote,
        HistoricalRewritePlanOptions {
            repo_root: options.repo_root.clone(),
        },
    )
    .ok();
    Ok(HistoricalRewriteResult {
        rewritten_objects: post_rewrite_plan
            .map(|plan| rewritten_objects.max(plan.reachable_object_count))
            .unwrap_or(rewritten_objects),
        retired_epoch_count: rewrite_state.retired_epoch_count,
        pending_gc_stale_remote_refs: stale_loose_paths,
        next_layout_generation: repo_state.layout_generation,
    })
}

fn collect_local_branch_ref_bytes(repo_root: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    let branches = RepositoryFacade::new().list_branches(repo_root)?;
    let secrets = sync_support::open_or_unlock_repo_secrets_for_sync(repo_root.join(".e2v"))?;
    let branch_dir = repo_root.join(".e2v").join("refs").join("branches");
    let mut refs = BTreeMap::new();
    for branch in branches {
        let local_bytes = std::fs::read(branch_dir.join(format!("{}.json", branch.token_hex)))
            .with_context(|| format!("failed to read local branch ref {}", branch.token_hex))?;
        let plaintext = sync_support::decrypt_control_record_for_sync(
            &secrets,
            &format!("branch-ref:{}", branch.token_hex),
            "ref",
            &local_bytes,
        )?;
        let remote_bytes =
            sync_support::encrypt_control_record_for_sync(&secrets, "default", "ref", &plaintext)?;
        refs.insert(branch.token_hex, remote_bytes);
    }
    Ok(refs)
}

fn publish_remote_branch_refs<R: RemoteBackend>(
    remote: &R,
    local_branch_refs: &BTreeMap<String, Vec<u8>>,
    remote_branch_refs: &[e2v_store::ListedRef],
    current_branch_token: &str,
) -> Result<()> {
    for listed_ref in remote_branch_refs {
        if listed_ref.token.value == current_branch_token {
            continue;
        }
        let Some(next_bytes) = local_branch_refs.get(&listed_ref.token.value) else {
            continue;
        };
        if listed_ref.stored.value.bytes.as_slice() == next_bytes.as_slice() {
            continue;
        }
        let cas = remote.compare_and_swap_ref(
            &listed_ref.token,
            Some(listed_ref.stored.version.clone()),
            e2v_store::EncryptedRef::new(next_bytes.clone()),
        )?;
        anyhow::ensure!(
            cas.applied,
            "historical rewrite requires needs-rebase recovery for branch {}",
            listed_ref.token.value
        );
    }
    Ok(())
}

fn hydrate_remote_branch_history_for_rewrite<R: RemoteBackend>(
    remote: &R,
    repo_root: &Path,
) -> Result<()> {
    let remote_branch_refs = list_remote_branch_refs(remote, repo_root)?;
    if remote_branch_refs.is_empty() {
        return Ok(());
    }

    let control_dir = repo_root.join(".e2v");
    let secrets = sync_support::open_or_unlock_repo_secrets_for_sync(&control_dir)?;
    let remote_loose_object_ids = load_remote_loose_object_ids(remote)?;
    let pack_locations =
        load_remote_active_pack_locations_with_local_cache(remote, &control_dir, &secrets)?;
    prune_stale_cached_pack_data(&control_dir, &pack_locations)?;
    let validation_root = RemoteValidationRoot {
        path: next_validation_root(repo_root)?,
        cache_control_dir: Some(control_dir.clone()),
    };
    let mut pack_cache = BTreeMap::new();

    for listed_ref in remote_branch_refs {
        let control_plane =
            read_remote_control_plane(remote, listed_ref.stored.value.bytes.clone())?;
        assert_remote_generations_meet_local_floor(
            &listed_ref.token.value,
            &listed_ref.stored,
            &control_plane,
        )?;
        write_remote_control_plane_to_validation_root(&validation_root, &control_plane)?;
        let (decoded_branch_token, head_snapshot_id) =
            sync_support::decode_default_ref_record(repo_root, &listed_ref.stored.value.bytes)?;
        anyhow::ensure!(
            decoded_branch_token == listed_ref.token.value,
            "remote ref token mismatch: listed {}, decoded {}",
            listed_ref.token.value,
            decoded_branch_token
        );

        if let Some(head_snapshot_id) = head_snapshot_id {
            let reachable_object_ids = collect_remote_reachable_object_ids(
                remote,
                &remote_loose_object_ids,
                &pack_locations,
                &validation_root,
                &secrets,
                &mut pack_cache,
                &head_snapshot_id,
            )?;
            for object_id in reachable_object_ids {
                fetch_remote_object_into_validation_root(
                    remote,
                    &remote_loose_object_ids,
                    &pack_locations,
                    &validation_root,
                    &mut pack_cache,
                    &object_id,
                )?;
                let source = validation_root.object_path(&object_id);
                let target = repo_root
                    .join(".e2v")
                    .join("objects")
                    .join(format!("{object_id}.json"));
                if !target.exists() {
                    let bytes = std::fs::read(&source).with_context(|| {
                        format!(
                            "failed to read hydrated remote object {} from {}",
                            object_id,
                            source.display()
                        )
                    })?;
                    overwrite_local_object_bytes(&target, &bytes)?;
                }
            }
        }

        let branch_ref_path = control_dir
            .join("refs")
            .join("branches")
            .join(format!("{}.json", listed_ref.token.value));
        let plaintext = sync_support::decrypt_control_record_for_sync(
            &secrets,
            "default",
            "ref",
            &listed_ref.stored.value.bytes,
        )?;
        let local_branch_ref_bytes = sync_support::encrypt_control_record_for_sync(
            &secrets,
            &format!("branch-ref:{}", listed_ref.token.value),
            "ref",
            &plaintext,
        )?;
        overwrite_local_object_bytes(&branch_ref_path, &local_branch_ref_bytes)?;
    }

    cache_pack_data_from_map(&control_dir, &pack_cache)?;
    Ok(())
}

fn cache_pack_data_from_map(
    control_dir: &Path,
    pack_cache: &BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    for (container_id, pack_bytes) in pack_cache {
        cache_pack_data_bytes(control_dir, container_id, pack_bytes)?;
    }
    Ok(())
}

fn initialize_maintenance_pack_cache(
    control_dir: &Path,
    pack_locations: &BTreeMap<String, PackedObjectLocation>,
) -> Result<BTreeMap<String, Vec<u8>>> {
    prune_stale_cached_pack_data(control_dir, pack_locations)?;
    Ok(BTreeMap::new())
}

fn retained_active_segment_paths_for_unrewritten_objects<R: RemoteBackend>(
    remote: &R,
    control_dir: &Path,
    historical_segment_secrets: Option<&e2v_store::RepoSecrets>,
    rewritten_object_ids: &BTreeSet<String>,
) -> Result<Vec<String>> {
    let secrets = sync_support::open_or_unlock_repo_secrets_for_sync(control_dir)?;
    let active_segment_paths = load_remote_active_pack_segment_paths(remote, &secrets)?;
    let mut retained = Vec::new();
    for segment_path in active_segment_paths {
        let segment_bytes = remote.get_physical(&segment_path)?;
        let object_ids =
            crate::pack_index::read_segment_object_ids(&segment_path, &segment_bytes, &secrets)
                .or_else(|error| {
                    let Some(historical_segment_secrets) = historical_segment_secrets else {
                        return Err(error);
                    };
                    crate::pack_index::read_segment_object_ids(
                        &segment_path,
                        &segment_bytes,
                        historical_segment_secrets,
                    )
                })?;
        if object_ids
            .iter()
            .any(|object_id| !rewritten_object_ids.contains(object_id))
        {
            retained.push(segment_path);
        }
    }
    Ok(retained)
}

fn upload_rewrite_segments_with_resume<R: RemoteBackend + Clone>(
    remote: &R,
    repo_root: &Path,
    operation_id: &OperationId,
    rewritten_object_ids: &[String],
    publisher: &SimpleTransactionPublisher<R>,
    session: &PublishSession,
) -> Result<Vec<String>> {
    let existing_rewrite_segments = list_operation_pack_index_segment_paths(remote, operation_id)?;
    let total_batches = rewritten_object_ids.len().div_ceil(256);
    let mut published_segments =
        Vec::with_capacity(total_batches.max(existing_rewrite_segments.len()));

    for batch_index in 0..total_batches {
        let batch_start = batch_index * 256;
        let batch_end = ((batch_index + 1) * 256).min(rewritten_object_ids.len());
        let batch_object_ids = &rewritten_object_ids[batch_start..batch_end];
        let expected_index_path = crate::pack::pack_paths(&operation_id.value, batch_index)?.2;
        if existing_rewrite_segments.contains(&expected_index_path) {
            published_segments.push(expected_index_path);
            continue;
        }
        let uploaded = upload_objects_as_pack_segments(
            remote,
            repo_root,
            operation_id,
            batch_object_ids,
            |_object_id| {
                publisher.heartbeat(session)?;
                Ok(())
            },
        )?;
        published_segments.extend(uploaded);
    }
    published_segments.sort();
    Ok(published_segments)
}

fn historical_rewrite_checkpoint_path(control_dir: &Path) -> PathBuf {
    control_dir
        .join("journal")
        .join("sync")
        .join(HISTORY_REWRITE_CHECKPOINT_FILE)
}

fn historical_rewrite_checkpoint_backup_path(control_dir: &Path) -> PathBuf {
    control_dir
        .join("journal")
        .join("sync")
        .join(HISTORY_REWRITE_CHECKPOINT_BACKUP_FILE)
}

fn load_historical_rewrite_checkpoint(control_dir: &Path) -> Result<Option<RewriteJournalState>> {
    let path = historical_rewrite_checkpoint_path(control_dir);
    let backup_path = historical_rewrite_checkpoint_backup_path(control_dir);
    match load_historical_rewrite_checkpoint_from_path(control_dir, &path) {
        Ok(Some(state)) => Ok(Some(state)),
        Ok(None) => match load_historical_rewrite_checkpoint_from_path(control_dir, &backup_path) {
            Ok(Some(state)) => {
                store_historical_rewrite_checkpoint(control_dir, &state)?;
                Ok(Some(state))
            }
            Ok(None) => Ok(None),
            Err(error) => Err(error),
        },
        Err(primary_error) => {
            match load_historical_rewrite_checkpoint_from_path(control_dir, &backup_path) {
                Ok(Some(state)) => {
                    remove_path_if_exists(&path).with_context(|| {
                        format!(
                            "failed to remove corrupted historical rewrite checkpoint {}",
                            path.display()
                        )
                    })?;
                    store_historical_rewrite_checkpoint(control_dir, &state)?;
                    Ok(Some(state))
                }
                Ok(None) => Err(primary_error),
                Err(backup_error) => Err(primary_error.context(format!(
                    "; backup checkpoint at {} also failed: {backup_error:#}",
                    backup_path.display()
                ))),
            }
        }
    }
}

fn load_historical_rewrite_checkpoint_from_path(
    control_dir: &Path,
    path: &Path,
) -> Result<Option<RewriteJournalState>> {
    if !path.is_file() {
        return Ok(None);
    }
    let bytes = std::fs::read(path).with_context(|| {
        format!(
            "failed to read historical rewrite checkpoint {}",
            path.display()
        )
    })?;
    let secrets = sync_support::open_or_unlock_repo_secrets_for_sync(control_dir)?;
    let plaintext = sync_support::decrypt_control_record_for_sync(
        &secrets,
        "history-rewrite-checkpoint",
        "history_rewrite_checkpoint",
        &bytes,
    )?;
    let checkpoint: HistoricalRewriteCheckpoint = serde_json::from_slice(&plaintext)
        .context("failed to decode historical rewrite checkpoint")?;
    Ok(Some(RewriteJournalState {
        stage: checkpoint.stage,
        target_layout_generation: checkpoint.target_layout_generation,
        rewritten_object_ids: checkpoint.rewritten_object_ids,
        retired_epoch_count: checkpoint.retired_epoch_count,
    }))
}

fn store_historical_rewrite_checkpoint(
    control_dir: &Path,
    state: &RewriteJournalState,
) -> Result<()> {
    let secrets = sync_support::open_or_unlock_repo_secrets_for_sync(control_dir)?;
    let checkpoint = HistoricalRewriteCheckpoint {
        stage: state.stage.clone(),
        target_layout_generation: state.target_layout_generation,
        rewritten_object_ids: state.rewritten_object_ids.clone(),
        retired_epoch_count: state.retired_epoch_count,
    };
    let plaintext = serde_json::to_vec(&checkpoint)
        .context("failed to encode historical rewrite checkpoint")?;
    let bytes = sync_support::encrypt_control_record_for_sync(
        &secrets,
        "history-rewrite-checkpoint",
        "history_rewrite_checkpoint",
        &plaintext,
    )?;
    let path = historical_rewrite_checkpoint_path(control_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    atomic_write_bytes(path, &bytes)?;
    atomic_write_bytes(
        historical_rewrite_checkpoint_backup_path(control_dir),
        &bytes,
    )
}

fn clear_historical_rewrite_checkpoint(control_dir: &Path) -> Result<()> {
    let path = historical_rewrite_checkpoint_path(control_dir);
    remove_path_if_exists(&path).with_context(|| {
        format!(
            "failed to remove historical rewrite checkpoint {}",
            path.display()
        )
    })?;
    let backup_path = historical_rewrite_checkpoint_backup_path(control_dir);
    remove_path_if_exists(&backup_path).with_context(|| {
        format!(
            "failed to remove historical rewrite checkpoint {}",
            backup_path.display()
        )
    })
}

fn atomic_write_bytes(path: PathBuf, bytes: &[u8]) -> Result<()> {
    let temp_path = path.with_extension(format!(
        "{}.tmp",
        path.extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("tmp")
    ));
    if let Some(parent) = path.parent() {
        ensure_directory_path(parent)?;
    }
    remove_path_if_exists(&temp_path)?;
    std::fs::write(&temp_path, bytes)
        .with_context(|| format!("failed to write {}", temp_path.display()))?;
    match std::fs::rename(&temp_path, &path) {
        Ok(()) => {}
        Err(error) if cfg!(windows) => {
            remove_path_if_exists(&path)?;
            std::fs::rename(&temp_path, &path)
                .with_context(|| format!("failed to publish {}", path.display()))?;
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to publish {}", path.display()));
        }
    }
    Ok(())
}

fn remove_path_if_exists(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path)?,
        Ok(_) => std::fs::remove_file(path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn ensure_directory_path(path: &Path) -> Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    if let Some(parent) = path.parent()
        && parent != path
    {
        ensure_directory_path(parent)?;
    }
    remove_path_if_exists(path)?;
    std::fs::create_dir_all(path)?;
    Ok(())
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
        load_remote_active_pack_locations_with_local_cache(remote, &control_dir, &secrets)?;
    let mut pack_cache = initialize_maintenance_pack_cache(&control_dir, &pack_locations)?;
    let control_plane = read_remote_control_plane(remote, default_ref.value.bytes.clone())?;
    let validation_root = RemoteValidationRoot {
        path: next_validation_root(&options.repo_root)?,
        cache_control_dir: Some(control_dir.clone()),
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
            overwrite_local_object_bytes(&local_path, &bytes)?;
            repaired_objects += 1;
        }
    }

    rewrite_local_control_plane_from_remote(&control_dir, &control_plane)?;
    restore_local_branch_mirrors_from_remote(remote, &options.repo_root)?;
    update_trusted_remote_state_from_control_plane(&branch_token, &default_ref, &control_plane)?;
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
    let remote_loose_object_ids = load_remote_loose_object_ids(remote)?;
    let secrets = sync_support::open_repo_secrets_for_sync(&control_dir)?;
    let pack_locations =
        load_remote_active_pack_locations_with_local_cache(remote, &control_dir, &secrets)?;
    let mut pack_cache = initialize_maintenance_pack_cache(&control_dir, &pack_locations)?;
    let facade = RepositoryFacade::new();
    let mut repaired_local_objects = 0usize;

    let control_plane = read_remote_control_plane(remote, default_ref.value.bytes.clone())?;
    assert_remote_generations_meet_local_floor(&branch_token, &default_ref, &control_plane)?;
    let validation_root = RemoteValidationRoot {
        path: next_validation_root(&options.repo_root)?,
        cache_control_dir: Some(control_dir.clone()),
    };
    write_remote_control_plane_to_validation_root(&validation_root, &control_plane)?;
    let mut traversal = RemoteReachabilityTraversal {
        remote,
        remote_loose_object_ids: &remote_loose_object_ids,
        pack_locations: &pack_locations,
        validation_root: &validation_root,
        validation_secrets: &secrets,
        pack_cache: &mut pack_cache,
    };
    let reachable_store = GcReachabilityStore::new(&options.repo_root)?;
    let mut reachable_store = reachable_store;
    collect_all_remote_reachable_object_ids(
        &options.repo_root,
        &mut traversal,
        &mut reachable_store,
    )?;
    persist_cached_pack_data(&control_dir, &pack_cache)?;
    let mut statement = reachable_store
        .sqlite
        .prepare("SELECT object_id FROM reachable_objects ORDER BY object_id")?;
    let reachable_object_ids = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(statement);
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
        overwrite_local_object_bytes(&local_path, &bytes)?;
        match facade.verify_object(&options.repo_root, object_id, expected_type) {
            Ok(()) => {
                if original.as_deref() != Some(bytes.as_slice()) {
                    repaired_local_objects += 1;
                }
            }
            Err(error) => {
                if let Some(original_bytes) = original {
                    overwrite_local_object_bytes(&local_path, &original_bytes)?;
                } else {
                    let _ = remove_path_if_exists(&local_path);
                }
                return Err(error).with_context(|| {
                    format!("failed to authenticate sampled remote object {object_id}")
                });
            }
        }
    }

    update_trusted_remote_state_from_control_plane(&branch_token, &default_ref, &control_plane)?;
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
    let remote_loose_object_ids = load_remote_loose_object_ids(remote)?;
    let secrets = sync_support::open_repo_secrets_for_sync(&control_dir)?;
    let pack_locations =
        load_remote_active_pack_locations_with_local_cache(remote, &control_dir, &secrets)?;
    let mut pack_cache = initialize_maintenance_pack_cache(&control_dir, &pack_locations)?;
    let control_plane = read_remote_control_plane(remote, default_ref.value.bytes.clone())?;
    assert_remote_generations_meet_local_floor(&branch_token, &default_ref, &control_plane)?;
    let validation_root = RemoteValidationRoot {
        path: next_validation_root(&options.repo_root)?,
        cache_control_dir: Some(control_dir.clone()),
    };
    write_remote_control_plane_to_validation_root(&validation_root, &control_plane)?;
    let mut traversal = RemoteReachabilityTraversal {
        remote,
        remote_loose_object_ids: &remote_loose_object_ids,
        pack_locations: &pack_locations,
        validation_root: &validation_root,
        validation_secrets: &secrets,
        pack_cache: &mut pack_cache,
    };
    let reachable_store = GcReachabilityStore::new(&options.repo_root)?;
    let mut reachable_store = reachable_store;
    collect_all_remote_reachable_object_ids(
        &options.repo_root,
        &mut traversal,
        &mut reachable_store,
    )?;
    persist_cached_pack_data(&control_dir, &pack_cache)?;

    let mut repaired_objects = 0usize;
    let mut statement = reachable_store
        .sqlite
        .prepare("SELECT object_id FROM reachable_objects ORDER BY object_id")?;
    let reachable_object_ids = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(statement);
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
            overwrite_local_object_bytes(&local_path, &bytes)?;
            repaired_objects += 1;
        }
    }

    update_trusted_remote_state_from_control_plane(&branch_token, &default_ref, &control_plane)?;
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
        load_remote_active_pack_locations_with_local_cache(remote, &control_dir, &secrets)?;
    let control_plane = read_remote_control_plane(remote, default_ref.value.bytes.clone())?;
    assert_remote_generations_meet_local_floor(&branch_token, &default_ref, &control_plane)?;
    let validation_root = RemoteValidationRoot {
        path: next_validation_root(&options.repo_root)?,
        cache_control_dir: Some(control_dir.clone()),
    };
    write_remote_control_plane_to_validation_root(&validation_root, &control_plane)?;
    let mut pack_cache = initialize_maintenance_pack_cache(&control_dir, &pack_locations)?;
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
    let pack_index_segment_paths = load_remote_active_pack_segment_paths(remote, &secrets)?;
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
    let mut scored = object_ids
        .iter()
        .map(|object_id| {
            let digest = blake3::hash(object_id.as_bytes());
            (digest.as_bytes().to_vec(), object_id)
        })
        .collect::<Vec<_>>();
    scored.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(right.1)));
    scored
        .into_iter()
        .take(target.max(1))
        .map(|(_, object_id)| object_id.clone())
        .collect::<Vec<_>>()
}

pub(crate) fn load_remote_loose_object_ids<R: RemoteBackend>(
    remote: &R,
) -> Result<BTreeSet<String>> {
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

pub(crate) fn persist_cached_pack_data(
    control_dir: impl AsRef<Path>,
    pack_cache: &BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    let control_dir = control_dir.as_ref();
    for (container_id, pack_bytes) in pack_cache {
        cache_pack_data_bytes(control_dir, container_id, pack_bytes)?;
    }
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

fn restore_local_branch_mirrors_from_remote<R: RemoteBackend>(
    remote: &R,
    repo_root: &Path,
) -> Result<()> {
    let control_dir = repo_root.join(".e2v");
    let branch_dir = control_dir.join("refs").join("branches");
    std::fs::create_dir_all(&branch_dir)?;
    let secrets = sync_support::open_or_unlock_repo_secrets_for_sync(&control_dir)?;

    for listed_ref in list_remote_branch_refs(remote, repo_root)? {
        let plaintext = sync_support::decrypt_control_record_for_sync(
            &secrets,
            "default",
            "ref",
            &listed_ref.stored.value.bytes,
        )?;
        let local_branch_ref_bytes = sync_support::encrypt_control_record_for_sync(
            &secrets,
            &format!("branch-ref:{}", listed_ref.token.value),
            "ref",
            &plaintext,
        )?;
        overwrite_local_object_bytes(
            &branch_dir.join(format!("{}.json", listed_ref.token.value)),
            &local_branch_ref_bytes,
        )?;
    }
    Ok(())
}

fn remove_local_index_if_present(repo_root: &Path) -> Result<()> {
    let index_path = repo_root.join(".e2v").join("index.sqlite3");
    for path in [
        index_path.clone(),
        sqlite_sidecar_path(&index_path, "-wal"),
        sqlite_sidecar_path(&index_path, "-shm"),
    ] {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to remove local index {}", path.display()));
            }
        }
    }
    Ok(())
}

fn sqlite_sidecar_path(db_path: &Path, suffix: &str) -> PathBuf {
    let mut os_string = db_path.as_os_str().to_os_string();
    os_string.push(suffix);
    PathBuf::from(os_string)
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
    match serde_json::from_slice(&bytes) {
        Ok(journal) => Ok(Some(journal)),
        Err(_) => {
            remove_path_if_exists(path).with_context(|| {
                format!(
                    "failed to remove corrupted gc delete journal {}",
                    path.display()
                )
            })?;
            Ok(None)
        }
    }
}

fn store_gc_delete_journal(path: &std::path::Path, journal: &GcDeleteJournal) -> Result<()> {
    let _parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("gc delete journal path has no parent"))?;
    atomic_write_bytes(path.to_path_buf(), &serde_json::to_vec(journal)?)
        .with_context(|| format!("failed to write gc delete journal {}", path.display()))
}

fn remove_gc_delete_journal(path: &std::path::Path) -> Result<()> {
    remove_path_if_exists(path)
        .with_context(|| format!("failed to remove gc delete journal {}", path.display()))?;
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
        let control_plane =
            read_remote_control_plane(traversal.remote, listed_ref.stored.value.bytes.clone())?;
        write_remote_control_plane_to_validation_root(traversal.validation_root, &control_plane)?;
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
        if reachable.contains_object_id(object_id)?
            && current_remote_physical_path_for_object(pack_locations, object_id).as_deref()
                == Some(format!("objects/{object_id}.json").as_str())
        {
            reachable.insert_physical_ref(&format!("objects/{object_id}.json"))?;
        }
    }
    for (object_id, location) in pack_locations {
        if reachable.contains_object_id(object_id)? {
            let physical_ref = location.physical_ref()?;
            reachable.insert_physical_ref(&physical_ref.container_id)?;
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

fn current_remote_physical_path_for_object(
    pack_locations: &BTreeMap<String, PackedObjectLocation>,
    object_id: &str,
) -> Option<String> {
    pack_locations
        .get(object_id)
        .map(|location| {
            location
                .physical_ref()
                .map(|physical_ref| physical_ref.container_id)
        })
        .transpose()
        .ok()
        .flatten()
        .or_else(|| Some(format!("objects/{object_id}.json")))
}

pub(crate) fn overwrite_local_object_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        ensure_directory_path(parent)?;
    }
    match std::fs::write(path, bytes) {
        Ok(()) => Ok(()),
        Err(_) => {
            remove_path_if_exists(path)?;
            std::fs::write(path, bytes)
                .with_context(|| format!("failed to write {}", path.display()))?;
            Ok(())
        }
    }
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
        .map(|location| {
            location
                .physical_ref()
                .map(|physical_ref| physical_ref.container_id)
        })
        .transpose()
        .ok()
        .flatten()
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

pub(crate) fn list_remote_branch_refs<R: RemoteBackend>(
    remote: &R,
    _repo_root: &std::path::Path,
) -> Result<Vec<e2v_store::ListedRef>> {
    let mut refs = Vec::new();
    for listed_ref in remote.list_refs()? {
        if listed_ref.token.value.starts_with("keyring/") {
            continue;
        }
        crate::journal::validate_sync_identifier("branch token", &listed_ref.token.value)
            .with_context(|| format!("invalid remote branch token {}", listed_ref.token.value))?;
        refs.push(listed_ref);
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

    #[test]
    fn maintenance_pack_cache_starts_empty_even_when_disk_cache_exists() {
        let temp = tempdir().unwrap();
        let control_dir = temp.path().join(".e2v");
        let container_id = "packs/data/pack-00000001.bin";
        let pack_locations = BTreeMap::from([(
            object_id('a'),
            PackedObjectLocation {
                data_path: container_id.to_string(),
                offset: 0,
                length: 13,
            },
        )]);

        cache_pack_data_bytes(&control_dir, container_id, b"packed-object").unwrap();

        let pack_cache = initialize_maintenance_pack_cache(&control_dir, &pack_locations).unwrap();

        assert!(
            pack_cache.is_empty(),
            "maintenance flows should lazily hydrate pack bytes instead of preloading all cached packs into memory"
        );
        assert!(
            control_dir
                .join("cache")
                .join("pack-data")
                .join("packs")
                .join("data")
                .join("pack-00000001.bin")
                .is_file(),
            "initializing the maintenance pack cache should preserve the on-disk cache for later lazy reuse"
        );
    }

    #[test]
    fn sample_object_ids_spreads_selection_instead_of_taking_prefix_only() {
        let object_ids = (1..=8u8)
            .map(|value| format!("{value:064x}"))
            .collect::<Vec<_>>();

        let sampled = sample_object_ids(&object_ids, 25);

        assert_eq!(sampled.len(), 2);
        assert_eq!(
            sampled,
            vec![format!("{:064x}", 5), format!("{:064x}", 3)],
            "verify_remote sampling should use a stable pseudo-random spread rather than the first N sorted object ids"
        );
    }
}
