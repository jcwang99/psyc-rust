use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use e2v_store::{BlobStore, RemoteBackend};

use crate::journal::OperationId;
use crate::transaction::PublishPlan;

pub(crate) const INTENT_EXPIRY_HOURS: u64 = 72;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct RemoteMarkerHeartbeat {
    pub remote_observed_at_unix_ms: u64,
    pub sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct RemoteWriteIntentMarker {
    pub operation_id: String,
    pub writer_id: String,
    pub started_at_remote_unix_ms: u64,
    pub heartbeat: RemoteMarkerHeartbeat,
    pub expected_ref_version: Option<u64>,
    pub target_branch_token: String,
    pub planned_snapshot_id: Option<String>,
    pub client_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct RemoteWriterLeaseMarker {
    pub writer_id: String,
    pub operation_id: String,
    pub target_branch_token: String,
    pub remote_observed_at_unix_ms: u64,
    pub lease_generation: u64,
    pub heartbeat: RemoteMarkerHeartbeat,
}

pub(crate) fn writer_id_for_operation(operation_id: &OperationId) -> String {
    format!("writer:{}", operation_id.value)
}

pub(crate) fn build_write_intent_marker(
    plan: &PublishPlan,
    remote_observed_at_unix_ms: u64,
    heartbeat_sequence: u64,
) -> RemoteWriteIntentMarker {
    RemoteWriteIntentMarker {
        operation_id: plan.operation_id.value.clone(),
        writer_id: writer_id_for_operation(&plan.operation_id),
        started_at_remote_unix_ms: remote_observed_at_unix_ms,
        heartbeat: RemoteMarkerHeartbeat {
            remote_observed_at_unix_ms,
            sequence: heartbeat_sequence,
        },
        expected_ref_version: plan.expected_ref_version,
        target_branch_token: plan.target_branch_token.clone(),
        planned_snapshot_id: plan.planned_snapshot_id.clone(),
        client_version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

pub(crate) fn build_writer_lease_marker(
    plan: &PublishPlan,
    remote_observed_at_unix_ms: u64,
    lease_generation: u64,
    heartbeat_sequence: u64,
) -> RemoteWriterLeaseMarker {
    RemoteWriterLeaseMarker {
        writer_id: writer_id_for_operation(&plan.operation_id),
        operation_id: plan.operation_id.value.clone(),
        target_branch_token: plan.target_branch_token.clone(),
        remote_observed_at_unix_ms,
        lease_generation,
        heartbeat: RemoteMarkerHeartbeat {
            remote_observed_at_unix_ms,
            sequence: heartbeat_sequence,
        },
    }
}

pub(crate) fn renew_write_intent_marker(
    existing: &RemoteWriteIntentMarker,
    remote_observed_at_unix_ms: u64,
) -> RemoteWriteIntentMarker {
    RemoteWriteIntentMarker {
        operation_id: existing.operation_id.clone(),
        writer_id: existing.writer_id.clone(),
        started_at_remote_unix_ms: existing.started_at_remote_unix_ms,
        heartbeat: RemoteMarkerHeartbeat {
            remote_observed_at_unix_ms,
            sequence: existing.heartbeat.sequence.saturating_add(1),
        },
        expected_ref_version: existing.expected_ref_version,
        target_branch_token: existing.target_branch_token.clone(),
        planned_snapshot_id: existing.planned_snapshot_id.clone(),
        client_version: existing.client_version.clone(),
    }
}

pub(crate) fn renew_writer_lease_marker(
    existing: &RemoteWriterLeaseMarker,
    remote_observed_at_unix_ms: u64,
) -> RemoteWriterLeaseMarker {
    RemoteWriterLeaseMarker {
        writer_id: existing.writer_id.clone(),
        operation_id: existing.operation_id.clone(),
        target_branch_token: existing.target_branch_token.clone(),
        remote_observed_at_unix_ms,
        lease_generation: existing.lease_generation.saturating_add(1),
        heartbeat: RemoteMarkerHeartbeat {
            remote_observed_at_unix_ms,
            sequence: existing.heartbeat.sequence.saturating_add(1),
        },
    }
}

pub(crate) fn system_time_to_unix_ms(value: SystemTime) -> Result<u64> {
    value
        .duration_since(UNIX_EPOCH)
        .context("system clock is before unix epoch")?
        .as_millis()
        .try_into()
        .context("remote observed timestamp overflow")
}

pub(crate) fn remote_observed_at_unix_ms<R: RemoteBackend>(remote: &R, path: &str) -> Result<u64> {
    let modified_at = remote.stat_physical(path)?.modified_at.ok_or_else(|| {
        anyhow::anyhow!("remote marker {path} has no reliable remote modification time")
    })?;
    system_time_to_unix_ms(modified_at)
}

pub(crate) fn observe_remote_now_with_probe<R: RemoteBackend>(
    remote: &R,
    probe_path: &str,
) -> Result<SystemTime> {
    remote.put_physical(probe_path, br#"{"probe":true}"#)?;
    let observed = remote
        .stat_physical(probe_path)?
        .modified_at
        .ok_or_else(|| {
            anyhow::anyhow!("remote probe {probe_path} has no reliable remote modification time")
        })?;
    remote.delete_physical(probe_path)?;
    Ok(observed)
}

pub(crate) fn marker_is_fresh_at<R: BlobStore>(
    remote: &R,
    path: &str,
    observed_now: SystemTime,
    expiry_hours: u64,
) -> Result<bool> {
    let modified_at = remote.stat_physical(path)?.modified_at.ok_or_else(|| {
        anyhow::anyhow!("remote marker {path} has no reliable remote modification time")
    })?;
    let expiry = Duration::from_secs(expiry_hours.saturating_mul(60 * 60));
    let age = observed_now.duration_since(modified_at).unwrap_or_default();
    Ok(age <= expiry)
}
