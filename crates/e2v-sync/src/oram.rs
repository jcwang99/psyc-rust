use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use e2v_core::{RepositoryFacade, sync_support};
use e2v_store::{
    DedupMode, LayoutCostPolicy, LayoutMode, LayoutRoot, LayoutSchedulePolicy, LayoutTrafficPolicy,
    RemoteBackend, RepoSecrets, validate_object_id_value,
};

use crate::journal::{OperationId, OperationJournal};
use crate::pack::PackedObjectLocation;
use crate::pack_index::publish_pack_index_root;
use crate::pack_index::{
    load_remote_pack_index_segment_paths,
    load_remote_pack_locations_from_segment_paths_with_local_cache,
    load_remote_pack_locations_with_local_cache,
};
use crate::publisher::{SimpleTransactionPublisher, TransactionPublisher};
use crate::push::{
    cleanup_completed_operation_markers, list_operation_pack_index_segment_paths,
    mirror_remote_keyring_pointer, publish_remote_keyring_pointer_with_retry,
    reconcile_local_keyring_with_remote_if_needed, upload_objects_as_pack_segments,
    upload_remote_keyring_generations,
};
use crate::remote_maintenance::list_remote_branch_refs;
use crate::transaction::{PublishPlan, PublishSession};

pub(crate) const OBLIVIOUS_ROOT_PATH: &str = "oblivious/root.json";
const OBLIVIOUS_ROOT_STABLE_NAME: &str = "oblivious-root";
const OBLIVIOUS_ROOT_OBJECT_TYPE: &str = "oblivious-root";
const OBLIVIOUS_OBJECT_BATCH_SIZE: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObliviousLayoutPlan {
    pub estimated_real_reads_per_request: u8,
    pub estimated_cover_reads_per_request: u8,
    pub estimated_bytes_per_request: u64,
    pub estimated_write_amplification: u8,
    pub requires_layout_root_rewrite: bool,
    pub advisory_messages: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObliviousLayoutStatus {
    pub layout_mode: String,
    pub dedup_mode: String,
    pub layout_generation: u64,
    pub oblivious_generation: Option<u64>,
    pub policy_profile: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnableObliviousLayoutOptions {
    pub repo_root: PathBuf,
    pub policy_profile: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReshuffleObliviousLayoutOptions {
    pub repo_root: PathBuf,
    pub policy_profile: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ObliviousRootRecord {
    schema_version: u32,
    layout_id: String,
    layout_generation: u64,
    oblivious_generation: u64,
    policy_profile: String,
    bucket_bytes: u32,
    object_count: usize,
    segment_paths: Vec<String>,
}

pub fn plan_oblivious_layout<R: RemoteBackend>(
    remote: &R,
    repo_root: &Path,
) -> Result<ObliviousLayoutPlan> {
    let layout_root = remote.read_layout_root()?;
    let policy_profile = remote_oblivious_root(remote, repo_root)?
        .map(|root| root.policy_profile)
        .unwrap_or_else(|| "balanced".to_string());
    let policy = policy_for_profile(&policy_profile);

    Ok(ObliviousLayoutPlan {
        estimated_real_reads_per_request: 1,
        estimated_cover_reads_per_request: policy.cover_reads_per_request.max(1),
        estimated_bytes_per_request: u64::from(policy.bucket_bytes)
            * u64::from(policy.min_total_reads.max(1)),
        estimated_write_amplification: policy
            .cover_reads_per_request
            .saturating_add(2)
            .max(2),
        requires_layout_root_rewrite: !matches!(layout_root.mode, LayoutMode::Oblivious),
        advisory_messages: vec![
            format!(
                "ORAM enablement will publish oblivious metadata with {} cover reads per logical request.",
                policy.cover_reads_per_request.max(1)
            ),
            "Oblivious mode disables long-lived stable physical dedup and uses generation-scoped randomized placement."
                .to_string(),
        ],
    })
}

pub fn status_oblivious_layout<R: RemoteBackend>(
    remote: &R,
    repo_root: &Path,
) -> Result<ObliviousLayoutStatus> {
    let layout_root = remote.read_layout_root()?;
    let remote_root = remote_oblivious_root(remote, repo_root)?;
    Ok(status_from_layout_root(
        &layout_root,
        remote_root
            .as_ref()
            .map(|root| root.policy_profile.as_str())
            .unwrap_or(layout_root.cost_policy.profile.as_str()),
    ))
}

pub fn enable_oblivious_layout<R: RemoteBackend + Clone>(
    remote: &R,
    options: EnableObliviousLayoutOptions,
) -> Result<ObliviousLayoutStatus> {
    let current_layout_root = remote.read_layout_root()?;
    ensure!(
        remote.capability().supports_oblivious_layout_updates(),
        "backend does not support oblivious layout updates"
    );
    if matches!(current_layout_root.mode, LayoutMode::Oblivious) {
        persist_local_layout_root(&options.repo_root, &current_layout_root)?;
        return status_oblivious_layout(remote, &options.repo_root);
    }

    execute_oblivious_layout_publish(
        remote,
        &options.repo_root,
        &options.policy_profile,
        current_layout_root.generation + 1,
        current_layout_root.oblivious_generation.unwrap_or(0) + 1,
        "oblivious-enable",
    )
}

pub fn reshuffle_oblivious_layout<R: RemoteBackend + Clone>(
    remote: &R,
    options: ReshuffleObliviousLayoutOptions,
) -> Result<ObliviousLayoutStatus> {
    let current_layout_root = remote.read_layout_root()?;
    ensure!(
        remote.capability().supports_oblivious_layout_updates(),
        "backend does not support oblivious layout updates"
    );
    ensure!(
        matches!(current_layout_root.mode, LayoutMode::Oblivious),
        "oblivious layout must be enabled before reshuffle"
    );

    execute_oblivious_layout_publish(
        remote,
        &options.repo_root,
        &options.policy_profile,
        current_layout_root.generation + 1,
        current_layout_root.oblivious_generation.unwrap_or(0) + 1,
        "oblivious-reshuffle",
    )
}

pub(crate) fn load_remote_active_pack_locations_with_local_cache<R: RemoteBackend>(
    remote: &R,
    control_dir: &Path,
    secrets: &RepoSecrets,
) -> Result<BTreeMap<String, PackedObjectLocation>> {
    let layout_root = remote.read_layout_root()?;
    if !matches!(layout_root.mode, LayoutMode::Oblivious) {
        return load_remote_pack_locations_with_local_cache(remote, control_dir, Some(secrets));
    }
    let segment_paths = load_remote_active_pack_segment_paths(remote, secrets)?;
    load_remote_pack_locations_from_segment_paths_with_local_cache(
        remote,
        control_dir,
        Some(secrets),
        &segment_paths,
    )
}

pub(crate) fn load_remote_active_pack_segment_paths<R: RemoteBackend>(
    remote: &R,
    secrets: &RepoSecrets,
) -> Result<Vec<String>> {
    let layout_root = remote.read_layout_root()?;
    if matches!(layout_root.mode, LayoutMode::Oblivious) {
        let root = remote_oblivious_root_with_secrets(remote, secrets)?.context(
            "remote layout root declares oblivious mode but oblivious/root.json is missing",
        )?;
        return Ok(root.segment_paths);
    }
    load_remote_pack_index_segment_paths(remote, Some(secrets))
}

fn execute_oblivious_layout_publish<R: RemoteBackend + Clone>(
    remote: &R,
    repo_root: &Path,
    policy_profile: &str,
    next_layout_generation: u64,
    next_oblivious_generation: u64,
    operation_prefix: &str,
) -> Result<ObliviousLayoutStatus> {
    reconcile_local_keyring_with_remote_if_needed(remote, repo_root)?;
    let facade = RepositoryFacade::new();
    let repo_state = facade.open(repo_root)?;
    let current_ref = remote.read_ref(&e2v_store::RefToken::new(
        repo_state.branch.token_hex.clone(),
    ))?;
    let keyring_files = sync_support::list_keyring_files(repo_root)?;
    let default_ref_bytes = sync_support::read_default_ref_bytes(repo_root)?;
    let control_dir = repo_root.join(".e2v");
    let secrets = sync_support::open_or_unlock_repo_secrets_for_sync(&control_dir)?;
    let current_layout_root = remote.read_layout_root()?;
    let next_layout_root = build_next_oblivious_layout_root(
        &current_layout_root,
        next_layout_generation,
        next_oblivious_generation,
        policy_profile,
    );
    let next_layout_root_bytes = serde_json::to_vec(&next_layout_root)
        .context("failed to encode next oblivious layout root")?;
    let reachable_object_ids = collect_oram_publish_object_ids(remote, repo_root, &secrets)?;

    let operation_id =
        OperationId::new(format!("{operation_prefix}-{next_layout_generation:020}"))?;
    let journal = OperationJournal::new(control_dir.join("journal").join("sync"))?;
    let publisher = SimpleTransactionPublisher::new(
        remote.capability().clone(),
        journal.clone(),
        remote.clone(),
    );
    let session = publisher.begin(PublishPlan {
        operation_id: operation_id.clone(),
        target_branch_token: repo_state.branch.token_hex.clone(),
        expected_ref_version: current_ref.as_ref().map(|stored| stored.version.value),
        planned_snapshot_id: sync_support::decode_ref_head_snapshot_id(
            repo_root,
            &default_ref_bytes,
        )?,
        writer_mode: remote.capability().push_write_mode(),
    })?;
    let session = PublishSession {
        next_layout_root: Some(next_layout_root.clone()),
        next_layout_root_bytes: Some(next_layout_root_bytes),
        ..session
    };

    let published_segment_paths = publish_oblivious_segments(
        remote,
        repo_root,
        &operation_id,
        &reachable_object_ids,
        &publisher,
        &session,
    )?;
    let remote_root = ObliviousRootRecord {
        schema_version: 1,
        layout_id: next_layout_root.layout_id.clone(),
        layout_generation: next_layout_root.generation,
        oblivious_generation: next_oblivious_generation,
        policy_profile: policy_profile.to_string(),
        bucket_bytes: next_layout_root.schedule_policy.bucket_bytes,
        object_count: reachable_object_ids.len(),
        segment_paths: published_segment_paths.clone(),
    };
    let remote_root_bytes = encode_oblivious_root_bytes(&secrets, &remote_root)?;

    upload_remote_keyring_generations(remote, &keyring_files)?;
    remote.put_physical(OBLIVIOUS_ROOT_PATH, &remote_root_bytes)?;
    publisher.publish_layout_if_needed(&session)?;
    if !published_segment_paths.is_empty() {
        publish_pack_index_root(
            remote,
            &secrets,
            &next_layout_root.layout_id,
            next_layout_root.generation,
            published_segment_paths.clone(),
        )?;
    }
    publisher.pre_commit_verify(&session)?;
    let pointer_bytes = publish_remote_keyring_pointer_with_retry(remote, repo_root)?;
    if let Some(current_ref) = current_ref {
        let cas = publisher.publish_ref(&session, current_ref.value)?;
        ensure!(
            cas.applied,
            "oblivious layout publish failed: remote ref changed"
        );
    }
    mirror_remote_keyring_pointer(remote, &pointer_bytes)?;
    publisher.complete(session)?;
    cleanup_completed_operation_markers(remote, &operation_id, &repo_state.branch.token_hex)?;
    persist_local_layout_root(repo_root, &next_layout_root)?;

    Ok(status_from_layout_root(&next_layout_root, policy_profile))
}

fn publish_oblivious_segments<R: RemoteBackend + Clone>(
    remote: &R,
    repo_root: &Path,
    operation_id: &OperationId,
    reachable_object_ids: &[String],
    publisher: &SimpleTransactionPublisher<R>,
    session: &PublishSession,
) -> Result<Vec<String>> {
    let existing_paths = list_operation_pack_index_segment_paths(remote, operation_id)?;
    let total_batches = reachable_object_ids
        .len()
        .div_ceil(OBLIVIOUS_OBJECT_BATCH_SIZE);
    let mut published_paths = Vec::with_capacity(total_batches.max(existing_paths.len()));

    for batch_index in 0..total_batches {
        let batch_start = batch_index * OBLIVIOUS_OBJECT_BATCH_SIZE;
        let batch_end =
            ((batch_index + 1) * OBLIVIOUS_OBJECT_BATCH_SIZE).min(reachable_object_ids.len());
        let batch = &reachable_object_ids[batch_start..batch_end];
        let expected_index_path = crate::pack::pack_paths(&operation_id.value, batch_index)?.2;
        if existing_paths.contains(&expected_index_path) {
            published_paths.push(expected_index_path);
            continue;
        }
        let uploaded = upload_objects_as_pack_segments(
            remote,
            repo_root,
            operation_id,
            batch,
            |_object_id| {
                publisher.heartbeat(session)?;
                Ok(())
            },
        )?;
        published_paths.extend(uploaded);
    }
    published_paths.sort();
    Ok(published_paths)
}

fn remote_oblivious_root<R: RemoteBackend>(
    remote: &R,
    repo_root: &Path,
) -> Result<Option<ObliviousRootRecord>> {
    let secrets = sync_support::open_or_unlock_repo_secrets_for_sync(repo_root.join(".e2v"))?;
    remote_oblivious_root_with_secrets(remote, &secrets)
}

fn remote_oblivious_root_with_secrets<R: RemoteBackend>(
    remote: &R,
    secrets: &RepoSecrets,
) -> Result<Option<ObliviousRootRecord>> {
    let bytes = match remote.get_physical(OBLIVIOUS_ROOT_PATH) {
        Ok(bytes) => bytes,
        Err(error) if e2v_store::is_missing_physical_object_error(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    let plaintext = e2v_core::sync_support::decrypt_control_record_for_sync(
        secrets,
        OBLIVIOUS_ROOT_STABLE_NAME,
        OBLIVIOUS_ROOT_OBJECT_TYPE,
        &bytes,
    )?;
    let root: ObliviousRootRecord =
        serde_json::from_slice(&plaintext).context("failed to decode oblivious root")?;
    Ok(Some(root))
}

fn encode_oblivious_root_bytes(
    secrets: &e2v_store::RepoSecrets,
    root: &ObliviousRootRecord,
) -> Result<Vec<u8>> {
    e2v_core::sync_support::encrypt_control_record_for_sync(
        secrets,
        OBLIVIOUS_ROOT_STABLE_NAME,
        OBLIVIOUS_ROOT_OBJECT_TYPE,
        &serde_json::to_vec(root)?,
    )
}

fn persist_local_layout_root(repo_root: &Path, layout_root: &LayoutRoot) -> Result<()> {
    let control_dir = repo_root.join(".e2v");
    std::fs::create_dir_all(&control_dir)?;
    std::fs::write(
        control_dir.join("layout_root.json"),
        serde_json::to_vec(layout_root)?,
    )?;
    Ok(())
}

fn build_next_oblivious_layout_root(
    current: &LayoutRoot,
    next_layout_generation: u64,
    next_oblivious_generation: u64,
    policy_profile: &str,
) -> LayoutRoot {
    let policy = policy_for_profile(policy_profile);
    LayoutRoot {
        schema_version: current.schema_version,
        layout_id: format!("oram-v{next_oblivious_generation}"),
        generation: next_layout_generation,
        mode: LayoutMode::Oblivious,
        mapping_policy: "bucketed-randomized".to_string(),
        dedup_mode: DedupMode::GenerationScopedRandomized,
        oblivious_generation: Some(next_oblivious_generation),
        schedule_policy: policy.clone(),
        traffic_policy: traffic_for_profile(policy_profile),
        cost_policy: cost_for_profile(policy_profile),
    }
}

fn collect_oram_publish_object_ids<R: RemoteBackend>(
    remote: &R,
    repo_root: &Path,
    secrets: &RepoSecrets,
) -> Result<Vec<String>> {
    collect_and_hydrate_remote_reachable_object_ids_for_oram_publish(remote, repo_root, secrets)
}

fn collect_and_hydrate_remote_reachable_object_ids_for_oram_publish<R: RemoteBackend>(
    remote: &R,
    repo_root: &Path,
    secrets: &RepoSecrets,
) -> Result<Vec<String>> {
    let mut remote_branch_refs = list_remote_branch_refs(remote, repo_root)?;
    if remote_branch_refs.is_empty() {
        let local_default_ref_bytes = sync_support::read_default_ref_bytes(repo_root)?;
        let (default_branch_token, _) =
            sync_support::decode_default_ref_record(repo_root, &local_default_ref_bytes)?;
        let default_ref = remote
            .read_ref(&e2v_store::RefToken::new(default_branch_token.clone()))?
            .ok_or_else(|| {
                anyhow::anyhow!("remote branch ref not found for {default_branch_token}")
            })?;
        remote_branch_refs.push(e2v_store::ListedRef {
            token: e2v_store::RefToken::new(default_branch_token),
            stored: default_ref,
        });
    }

    let mut object_ids = Vec::new();
    let mut unique = BTreeSet::new();
    let control_dir = repo_root.join(".e2v");
    let remote_loose_object_ids = remote
        .list_physical("objects/")?
        .into_iter()
        .filter_map(|relative_path| {
            relative_path
                .strip_prefix("objects/")
                .and_then(|value| value.strip_suffix(".json"))
                .map(str::to_string)
        })
        .collect::<BTreeSet<_>>();
    let pack_locations =
        load_remote_active_pack_locations_with_local_cache(remote, &control_dir, secrets)?;
    let validation_root = crate::fetch::RemoteValidationRoot {
        path: crate::fetch::next_validation_root(repo_root)?,
        cache_control_dir: Some(control_dir.clone()),
    };
    let mut pack_cache = BTreeMap::new();

    for listed_ref in remote_branch_refs {
        let control_plane =
            crate::fetch::read_remote_control_plane(remote, listed_ref.stored.value.bytes.clone())?;
        crate::fetch::write_remote_control_plane_to_validation_root(
            &validation_root,
            &control_plane,
        )?;
        let (branch_token, head_snapshot_id) =
            sync_support::decode_default_ref_record(repo_root, &listed_ref.stored.value.bytes)?;
        if branch_token != listed_ref.token.value {
            anyhow::bail!(
                "remote ref token mismatch during ORAM publish: listed {}, decoded {}",
                listed_ref.token.value,
                branch_token
            );
        }
        let Some(head_snapshot_id) = head_snapshot_id else {
            continue;
        };
        let reachable = crate::fetch::collect_remote_reachable_object_ids(
            remote,
            &remote_loose_object_ids,
            &pack_locations,
            &validation_root,
            secrets,
            &mut pack_cache,
            &head_snapshot_id,
        )?;
        for object_id in reachable {
            validate_object_id_value(&object_id)?;
            crate::fetch::fetch_remote_object_into_validation_root(
                remote,
                &remote_loose_object_ids,
                &pack_locations,
                &validation_root,
                &mut pack_cache,
                &object_id,
            )?;
            let target = repo_root
                .join(".e2v")
                .join("objects")
                .join(format!("{object_id}.json"));
            if !target.exists() {
                let source = validation_root.object_path(&object_id);
                let bytes = std::fs::read(&source).with_context(|| {
                    format!(
                        "failed to read hydrated remote object {} from {}",
                        object_id,
                        source.display()
                    )
                })?;
                std::fs::write(&target, bytes)?;
            }
            if unique.insert(object_id.clone()) {
                object_ids.push(object_id);
            }
        }
    }

    Ok(object_ids)
}

fn policy_for_profile(profile: &str) -> LayoutSchedulePolicy {
    match profile {
        "throughput" => LayoutSchedulePolicy {
            bucket_bytes: 8192,
            min_total_reads: 2,
            cover_reads_per_request: 1,
            reshuffle_after_generations: 8,
        },
        "strict" => LayoutSchedulePolicy {
            bucket_bytes: 4096,
            min_total_reads: 4,
            cover_reads_per_request: 3,
            reshuffle_after_generations: 3,
        },
        _ => LayoutSchedulePolicy {
            bucket_bytes: 4096,
            min_total_reads: 3,
            cover_reads_per_request: 2,
            reshuffle_after_generations: 5,
        },
    }
}

fn traffic_for_profile(profile: &str) -> LayoutTrafficPolicy {
    match profile {
        "throughput" => LayoutTrafficPolicy {
            max_parallel_reads: 4,
            inter_read_delay_ms: 0,
            burst_budget_bytes: 65_536,
            target_request_window_ms: 40,
        },
        "strict" => LayoutTrafficPolicy {
            max_parallel_reads: 1,
            inter_read_delay_ms: 20,
            burst_budget_bytes: 8_192,
            target_request_window_ms: 120,
        },
        _ => LayoutTrafficPolicy {
            max_parallel_reads: 2,
            inter_read_delay_ms: 15,
            burst_budget_bytes: 16_384,
            target_request_window_ms: 90,
        },
    }
}

fn cost_for_profile(profile: &str) -> LayoutCostPolicy {
    match profile {
        "throughput" => LayoutCostPolicy {
            profile: profile.to_string(),
            max_expected_read_amplification: 2,
            max_expected_write_amplification: 3,
        },
        "strict" => LayoutCostPolicy {
            profile: profile.to_string(),
            max_expected_read_amplification: 4,
            max_expected_write_amplification: 5,
        },
        _ => LayoutCostPolicy {
            profile: profile.to_string(),
            max_expected_read_amplification: 3,
            max_expected_write_amplification: 4,
        },
    }
}

fn status_from_layout_root(
    layout_root: &LayoutRoot,
    policy_profile: &str,
) -> ObliviousLayoutStatus {
    ObliviousLayoutStatus {
        layout_mode: match layout_root.mode {
            LayoutMode::Direct => "direct",
            LayoutMode::Pack => "pack",
            LayoutMode::Rewrite => "rewrite",
            LayoutMode::Oblivious => "oblivious",
        }
        .to_string(),
        dedup_mode: match layout_root.dedup_mode {
            DedupMode::StablePhysical => "stable-physical",
            DedupMode::GenerationScopedRandomized => "generation-scoped-randomized",
        }
        .to_string(),
        layout_generation: layout_root.generation,
        oblivious_generation: layout_root.oblivious_generation,
        policy_profile: policy_profile.to_string(),
    }
}
