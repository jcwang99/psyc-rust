use std::collections::BTreeMap;
use std::fs;

use anyhow::Result;
use e2v_core::{CommitOptions, InitOptions, RepositoryFacade};
use e2v_store::{RemoteBackend, RemoteTelemetryHandle, RemoteTelemetrySnapshot};
use serde::{Deserialize, Serialize};
use tempfile::tempdir;

use crate::{
    CloneOptions, CloneResult, FetchOptions, FetchResult, PushOptions, PushResult,
    RemoteBackendRef, RemoteSpec, clone_remote, fetch_remote, push_head,
    push_head_with_single_writer_risk,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteDiagnosticsScenario {
    Full,
    Push,
    Clone,
    Fetch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteDiagnosticsOptions {
    pub remote_spec: String,
    pub scenario: RemoteDiagnosticsScenario,
    pub password: String,
    pub file_count: usize,
    pub payload_bytes: usize,
    pub force_single_writer_risk: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteDiagnosticsPhaseReport {
    pub name: String,
    pub summary: String,
    pub elapsed_ms: u128,
    pub metrics: RemoteTelemetrySnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteDiagnosticsReport {
    pub scenario: RemoteDiagnosticsScenario,
    pub remote_kind: String,
    pub remote_spec_redacted: String,
    pub file_count: usize,
    pub payload_bytes: usize,
    pub elapsed_ms: u128,
    pub phases: Vec<RemoteDiagnosticsPhaseReport>,
    pub total_metrics: RemoteTelemetrySnapshot,
}

impl RemoteDiagnosticsReport {
    pub fn render_summary(&self) -> String {
        let mut output = String::new();
        output.push_str("remote diagnostics\n");
        output.push_str(&format!(
            "scenario: {:?}\nremote: {}\nrequests: {}\nbytes_sent: {}\nbytes_received: {}\nunique_paths: {}\n",
            self.scenario,
            self.remote_kind,
            self.total_metrics.total_requests,
            self.total_metrics.bytes_sent,
            self.total_metrics.bytes_received,
            self.total_metrics.unique_path_count()
        ));
        for phase in &self.phases {
            output.push_str(&format!(
                "{}: elapsed_ms={} requests={} bytes_sent={} bytes_received={} unique_paths={} {}\n",
                phase.name,
                phase.elapsed_ms,
                phase.metrics.total_requests,
                phase.metrics.bytes_sent,
                phase.metrics.bytes_received,
                phase.metrics.unique_path_count(),
                phase.summary
            ));
        }
        output
    }
}

pub fn run_remote_diagnostics(
    options: RemoteDiagnosticsOptions,
) -> Result<RemoteDiagnosticsReport> {
    anyhow::ensure!(
        options.file_count > 0,
        "file_count must be greater than zero"
    );
    anyhow::ensure!(
        options.payload_bytes > 0,
        "payload_bytes must be greater than zero"
    );

    let start = std::time::Instant::now();
    let remote_spec = RemoteSpec::parse(&options.remote_spec)?;
    let temp = tempdir()?;
    let source_repo_root = temp.path().join("source");
    let clone_repo_root = temp.path().join("clone");
    fs::create_dir_all(&source_repo_root)?;

    let facade = RepositoryFacade::new();
    let state = facade.init(InitOptions {
        repo_root: source_repo_root.clone(),
        password: options.password.clone(),
        branch_name: "main".to_string(),
    })?;
    write_payload_files(
        &source_repo_root,
        options.file_count,
        "v1",
        options.payload_bytes,
    )?;
    let first_commit = facade.commit(CommitOptions {
        repo_root: source_repo_root.clone(),
        message: "diagnostics-v1".to_string(),
    })?;

    with_instrumented_remote_backend(&remote_spec, |remote| match remote {
        RemoteBackendRef::LocalFolder(remote) => ensure_remote_root_is_empty(remote),
        RemoteBackendRef::S3(remote) => ensure_remote_root_is_empty(remote),
        RemoteBackendRef::Webdav(remote) => ensure_remote_root_is_empty(remote),
    })?;

    let mut phases = Vec::new();
    match options.scenario {
        RemoteDiagnosticsScenario::Full => {
            let (push_v1, push_v1_phase) = measure_push_phase(
                &remote_spec,
                &facade,
                PushOptions {
                    repo_root: source_repo_root.clone(),
                    branch_token: state.branch.token_hex.clone(),
                    operation_id: "remote-diagnostics-push-v1".to_string(),
                },
                options.force_single_writer_risk,
                "push_v1",
            )?;
            phases.push(push_v1_phase);

            let (_cloned, clone_phase) = measure_clone_phase(
                &remote_spec,
                CloneOptions {
                    repo_root: clone_repo_root.clone(),
                    password: options.password.clone(),
                    branch_token: state.branch.token_hex.clone(),
                },
                "clone",
            )?;
            phases.push(clone_phase);

            write_payload_files(
                &source_repo_root,
                options.file_count,
                "v2",
                options.payload_bytes,
            )?;
            let second_commit = facade.commit(CommitOptions {
                repo_root: source_repo_root.clone(),
                message: "diagnostics-v2".to_string(),
            })?;
            anyhow::ensure!(
                second_commit.snapshot_id != first_commit.snapshot_id,
                "expected diagnostics v2 commit to produce a new snapshot"
            );

            let (_push_v2, push_v2_phase) = measure_push_phase(
                &remote_spec,
                &facade,
                PushOptions {
                    repo_root: source_repo_root.clone(),
                    branch_token: state.branch.token_hex.clone(),
                    operation_id: "remote-diagnostics-push-v2".to_string(),
                },
                options.force_single_writer_risk,
                "push_v2",
            )?;
            phases.push(push_v2_phase);

            let (_fetch, fetch_phase) = measure_fetch_phase(
                &remote_spec,
                FetchOptions {
                    repo_root: clone_repo_root.clone(),
                    branch_token: state.branch.token_hex.clone(),
                    password: Some(options.password.clone()),
                },
                "fetch",
            )?;
            phases.push(fetch_phase);

            let _ = push_v1;
        }
        RemoteDiagnosticsScenario::Push => {
            let (_, phase) = measure_push_phase(
                &remote_spec,
                &facade,
                PushOptions {
                    repo_root: source_repo_root.clone(),
                    branch_token: state.branch.token_hex.clone(),
                    operation_id: "remote-diagnostics-push".to_string(),
                },
                options.force_single_writer_risk,
                "push",
            )?;
            phases.push(phase);
        }
        RemoteDiagnosticsScenario::Clone => {
            with_plain_remote_backend(&remote_spec, |remote| match remote {
                RemoteBackendRef::LocalFolder(remote) => run_push(
                    &facade,
                    remote,
                    PushOptions {
                        repo_root: source_repo_root.clone(),
                        branch_token: state.branch.token_hex.clone(),
                        operation_id: "remote-diagnostics-clone-seed".to_string(),
                    },
                    options.force_single_writer_risk,
                ),
                RemoteBackendRef::S3(remote) => run_push(
                    &facade,
                    remote,
                    PushOptions {
                        repo_root: source_repo_root.clone(),
                        branch_token: state.branch.token_hex.clone(),
                        operation_id: "remote-diagnostics-clone-seed".to_string(),
                    },
                    options.force_single_writer_risk,
                ),
                RemoteBackendRef::Webdav(remote) => run_push(
                    &facade,
                    remote,
                    PushOptions {
                        repo_root: source_repo_root.clone(),
                        branch_token: state.branch.token_hex.clone(),
                        operation_id: "remote-diagnostics-clone-seed".to_string(),
                    },
                    options.force_single_writer_risk,
                ),
            })?;
            let (_, phase) = measure_clone_phase(
                &remote_spec,
                CloneOptions {
                    repo_root: clone_repo_root.clone(),
                    password: options.password.clone(),
                    branch_token: state.branch.token_hex.clone(),
                },
                "clone",
            )?;
            phases.push(phase);
        }
        RemoteDiagnosticsScenario::Fetch => {
            with_plain_remote_backend(&remote_spec, |remote| match remote {
                RemoteBackendRef::LocalFolder(remote) => run_push(
                    &facade,
                    remote,
                    PushOptions {
                        repo_root: source_repo_root.clone(),
                        branch_token: state.branch.token_hex.clone(),
                        operation_id: "remote-diagnostics-fetch-seed".to_string(),
                    },
                    options.force_single_writer_risk,
                ),
                RemoteBackendRef::S3(remote) => run_push(
                    &facade,
                    remote,
                    PushOptions {
                        repo_root: source_repo_root.clone(),
                        branch_token: state.branch.token_hex.clone(),
                        operation_id: "remote-diagnostics-fetch-seed".to_string(),
                    },
                    options.force_single_writer_risk,
                ),
                RemoteBackendRef::Webdav(remote) => run_push(
                    &facade,
                    remote,
                    PushOptions {
                        repo_root: source_repo_root.clone(),
                        branch_token: state.branch.token_hex.clone(),
                        operation_id: "remote-diagnostics-fetch-seed".to_string(),
                    },
                    options.force_single_writer_risk,
                ),
            })?;
            with_plain_remote_backend(&remote_spec, |remote| match remote {
                RemoteBackendRef::LocalFolder(remote) => clone_remote(
                    remote,
                    CloneOptions {
                        repo_root: clone_repo_root.clone(),
                        password: options.password.clone(),
                        branch_token: state.branch.token_hex.clone(),
                    },
                ),
                RemoteBackendRef::S3(remote) => clone_remote(
                    remote,
                    CloneOptions {
                        repo_root: clone_repo_root.clone(),
                        password: options.password.clone(),
                        branch_token: state.branch.token_hex.clone(),
                    },
                ),
                RemoteBackendRef::Webdav(remote) => clone_remote(
                    remote,
                    CloneOptions {
                        repo_root: clone_repo_root.clone(),
                        password: options.password.clone(),
                        branch_token: state.branch.token_hex.clone(),
                    },
                ),
            })?;

            write_payload_files(
                &source_repo_root,
                options.file_count,
                "v2",
                options.payload_bytes,
            )?;
            facade.commit(CommitOptions {
                repo_root: source_repo_root.clone(),
                message: "diagnostics-fetch-v2".to_string(),
            })?;
            with_plain_remote_backend(&remote_spec, |remote| match remote {
                RemoteBackendRef::LocalFolder(remote) => run_push(
                    &facade,
                    remote,
                    PushOptions {
                        repo_root: source_repo_root.clone(),
                        branch_token: state.branch.token_hex.clone(),
                        operation_id: "remote-diagnostics-fetch-update".to_string(),
                    },
                    options.force_single_writer_risk,
                ),
                RemoteBackendRef::S3(remote) => run_push(
                    &facade,
                    remote,
                    PushOptions {
                        repo_root: source_repo_root.clone(),
                        branch_token: state.branch.token_hex.clone(),
                        operation_id: "remote-diagnostics-fetch-update".to_string(),
                    },
                    options.force_single_writer_risk,
                ),
                RemoteBackendRef::Webdav(remote) => run_push(
                    &facade,
                    remote,
                    PushOptions {
                        repo_root: source_repo_root.clone(),
                        branch_token: state.branch.token_hex.clone(),
                        operation_id: "remote-diagnostics-fetch-update".to_string(),
                    },
                    options.force_single_writer_risk,
                ),
            })?;

            let (_, phase) = measure_fetch_phase(
                &remote_spec,
                FetchOptions {
                    repo_root: clone_repo_root.clone(),
                    branch_token: state.branch.token_hex.clone(),
                    password: Some(options.password.clone()),
                },
                "fetch",
            )?;
            phases.push(phase);
        }
    }

    Ok(RemoteDiagnosticsReport {
        scenario: options.scenario,
        remote_kind: remote_kind(&remote_spec).to_string(),
        remote_spec_redacted: redact_remote_spec(&options.remote_spec),
        file_count: options.file_count,
        payload_bytes: options.payload_bytes,
        elapsed_ms: start.elapsed().as_millis(),
        total_metrics: sum_phase_metrics(&phases),
        phases,
    })
}

fn measure_push_phase(
    remote_spec: &RemoteSpec,
    facade: &RepositoryFacade,
    push_options: PushOptions,
    force_single_writer_risk: bool,
    phase_name: &str,
) -> Result<(PushResult, RemoteDiagnosticsPhaseReport)> {
    let started = std::time::Instant::now();
    let push_options_local = push_options.clone();
    let (result, metrics) = with_instrumented_remote_backend(remote_spec, |remote| match remote {
        RemoteBackendRef::LocalFolder(remote) => run_push(
            facade,
            remote,
            push_options_local.clone(),
            force_single_writer_risk,
        ),
        RemoteBackendRef::S3(remote) => run_push(
            facade,
            remote,
            push_options_local.clone(),
            force_single_writer_risk,
        ),
        RemoteBackendRef::Webdav(remote) => run_push(
            facade,
            remote,
            push_options_local.clone(),
            force_single_writer_risk,
        ),
    })?;
    Ok((
        result.clone(),
        RemoteDiagnosticsPhaseReport {
            name: phase_name.to_string(),
            summary: format!(
                "pushed {} uploaded_objects={}",
                short_id(&result.published_snapshot_id),
                result.uploaded_objects
            ),
            elapsed_ms: started.elapsed().as_millis(),
            metrics,
        },
    ))
}

fn measure_clone_phase(
    remote_spec: &RemoteSpec,
    clone_options: CloneOptions,
    phase_name: &str,
) -> Result<(CloneResult, RemoteDiagnosticsPhaseReport)> {
    let started = std::time::Instant::now();
    let clone_options_local = clone_options.clone();
    let (result, metrics) = with_instrumented_remote_backend(remote_spec, |remote| match remote {
        RemoteBackendRef::LocalFolder(remote) => clone_remote(remote, clone_options_local.clone()),
        RemoteBackendRef::S3(remote) => clone_remote(remote, clone_options_local.clone()),
        RemoteBackendRef::Webdav(remote) => clone_remote(remote, clone_options_local.clone()),
    })?;
    Ok((
        result.clone(),
        RemoteDiagnosticsPhaseReport {
            name: phase_name.to_string(),
            summary: format!(
                "cloned {}",
                result
                    .head_snapshot_id
                    .as_deref()
                    .map(short_id)
                    .unwrap_or("no-head")
            ),
            elapsed_ms: started.elapsed().as_millis(),
            metrics,
        },
    ))
}

fn measure_fetch_phase(
    remote_spec: &RemoteSpec,
    fetch_options: FetchOptions,
    phase_name: &str,
) -> Result<(FetchResult, RemoteDiagnosticsPhaseReport)> {
    let started = std::time::Instant::now();
    let fetch_options_local = fetch_options.clone();
    let (result, metrics) = with_instrumented_remote_backend(remote_spec, |remote| match remote {
        RemoteBackendRef::LocalFolder(remote) => fetch_remote(remote, fetch_options_local.clone()),
        RemoteBackendRef::S3(remote) => fetch_remote(remote, fetch_options_local.clone()),
        RemoteBackendRef::Webdav(remote) => fetch_remote(remote, fetch_options_local.clone()),
    })?;
    Ok((
        result.clone(),
        RemoteDiagnosticsPhaseReport {
            name: phase_name.to_string(),
            summary: format!("downloaded_objects={}", result.downloaded_objects),
            elapsed_ms: started.elapsed().as_millis(),
            metrics,
        },
    ))
}

fn run_push<R: RemoteBackend + Clone>(
    facade: &RepositoryFacade,
    remote: &R,
    push_options: PushOptions,
    force_single_writer_risk: bool,
) -> Result<PushResult> {
    if force_single_writer_risk {
        push_head_with_single_writer_risk(facade, remote, push_options)
    } else {
        push_head(facade, remote, push_options)
    }
}

fn with_plain_remote_backend<T>(
    remote_spec: &RemoteSpec,
    handler: impl FnOnce(RemoteBackendRef<'_>) -> Result<T>,
) -> Result<T> {
    remote_spec.with_backend(handler)
}

fn with_instrumented_remote_backend<T>(
    remote_spec: &RemoteSpec,
    handler: impl FnOnce(RemoteBackendRef<'_>) -> Result<T>,
) -> Result<(T, RemoteTelemetrySnapshot)> {
    let telemetry = RemoteTelemetryHandle::new();
    let result = remote_spec.with_backend_telemetry(Some(telemetry.clone()), handler)?;
    Ok((result, telemetry.snapshot()))
}

fn write_payload_files(
    repo_root: &std::path::Path,
    file_count: usize,
    version: &str,
    payload_bytes: usize,
) -> Result<()> {
    for index in 0..file_count {
        let prefix = format!("{version}-payload-{index:04}-");
        let repeated = prefix.repeat((payload_bytes / prefix.len()).max(1));
        let payload = repeated.chars().take(payload_bytes).collect::<String>();
        fs::write(
            repo_root.join(format!("diagnostic-{index:04}.txt")),
            payload,
        )?;
    }
    Ok(())
}

fn ensure_remote_root_is_empty<R: RemoteBackend>(remote: &R) -> Result<()> {
    anyhow::ensure!(
        remote.list_refs()?.is_empty(),
        "diagnostics remote root must be empty before running"
    );
    for prefix in [
        "objects/",
        "packs/",
        "pack-index/",
        "control/keyring/",
        "control/refs/",
        "transactions/",
        "leases/",
    ] {
        anyhow::ensure!(
            remote.list_physical(prefix)?.is_empty(),
            "diagnostics remote root must be empty before running"
        );
    }
    Ok(())
}

fn sum_phase_metrics(phases: &[RemoteDiagnosticsPhaseReport]) -> RemoteTelemetrySnapshot {
    let mut totals = RemoteTelemetrySnapshot::default();
    for phase in phases {
        totals.total_requests = totals
            .total_requests
            .saturating_add(phase.metrics.total_requests);
        totals.failed_requests = totals
            .failed_requests
            .saturating_add(phase.metrics.failed_requests);
        totals.duration_ms = totals.duration_ms.saturating_add(phase.metrics.duration_ms);
        totals.bytes_sent = totals.bytes_sent.saturating_add(phase.metrics.bytes_sent);
        totals.bytes_received = totals
            .bytes_received
            .saturating_add(phase.metrics.bytes_received);
        totals.listed_entries = totals
            .listed_entries
            .saturating_add(phase.metrics.listed_entries);
        merge_operation_stats(&mut totals.operations, &phase.metrics.operations);
        merge_path_stats(&mut totals.paths, &phase.metrics.paths);
    }
    totals
}

fn merge_operation_stats(
    target: &mut BTreeMap<e2v_store::RemoteOperationKind, e2v_store::RemoteOperationStats>,
    source: &BTreeMap<e2v_store::RemoteOperationKind, e2v_store::RemoteOperationStats>,
) {
    for (kind, stats) in source {
        let entry = target.entry(*kind).or_default();
        entry.requests = entry.requests.saturating_add(stats.requests);
        entry.failed_requests = entry.failed_requests.saturating_add(stats.failed_requests);
        entry.duration_ms = entry.duration_ms.saturating_add(stats.duration_ms);
        entry.bytes_sent = entry.bytes_sent.saturating_add(stats.bytes_sent);
        entry.bytes_received = entry.bytes_received.saturating_add(stats.bytes_received);
        entry.listed_entries = entry.listed_entries.saturating_add(stats.listed_entries);
    }
}

fn merge_path_stats(
    target: &mut BTreeMap<String, e2v_store::RemotePathStats>,
    source: &BTreeMap<String, e2v_store::RemotePathStats>,
) {
    for (path, stats) in source {
        let entry = target.entry(path.clone()).or_default();
        entry.requests = entry.requests.saturating_add(stats.requests);
        entry.failed_requests = entry.failed_requests.saturating_add(stats.failed_requests);
        entry.duration_ms = entry.duration_ms.saturating_add(stats.duration_ms);
        entry.bytes_sent = entry.bytes_sent.saturating_add(stats.bytes_sent);
        entry.bytes_received = entry.bytes_received.saturating_add(stats.bytes_received);
        entry.listed_entries = entry.listed_entries.saturating_add(stats.listed_entries);
        merge_operation_stats(&mut entry.operations, &stats.operations);
    }
}

fn remote_kind(remote_spec: &RemoteSpec) -> &'static str {
    match remote_spec {
        RemoteSpec::LocalFolder(_) => "local-folder",
        RemoteSpec::S3(_) => "s3",
        RemoteSpec::Webdav(_) => "webdav",
    }
}

fn redact_remote_spec(spec: &str) -> String {
    if spec.starts_with("file://") || spec.starts_with("file+") {
        return "file://<redacted>".to_string();
    }
    if let Some((scheme, _)) = spec.split_once("://") {
        return format!("{scheme}://<redacted>");
    }
    "<redacted>".to_string()
}

fn short_id(value: &str) -> &str {
    &value[..value.len().min(8)]
}
