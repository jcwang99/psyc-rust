use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use e2v_api::Sdk;
use e2v_core::sync_support::read_repo_id;
use e2v_core::{MetadataSearchQuery, RepositoryFacade};
use e2v_store::{BackendCapability, RemoteBackend};
use e2v_sync::{
    GcDryRunOptions, GcExecuteOptions, RepairRemoteOptions, VerifyRemoteOptions,
    force_accept_remote_rollback as force_accept_remote_rollback_sync, gc_dry_run, gc_execute,
    gc_execute_capability_status, load_trusted_remote_state_for_repo,
    remote_spec::RemoteBackendRef, repair_remote, verify_remote,
};
use e2v_vfs::{MountLaunchSummary, mount_live_branch, mount_snapshot};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(name = "e2v")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Branch {
        #[command(subcommand)]
        command: BranchCommand,
        #[arg(long)]
        repo: PathBuf,
    },
    Search {
        query: String,
        #[arg(long)]
        repo: PathBuf,
    },
    Share {
        #[command(subcommand)]
        command: ShareCommand,
        #[arg(long)]
        repo: PathBuf,
    },
    Remote {
        #[command(subcommand)]
        command: RemoteCommand,
        #[arg(long)]
        repo: PathBuf,
    },
    Verify {
        #[command(subcommand)]
        command: VerifyCommand,
        #[arg(long)]
        repo: PathBuf,
    },
    Repair {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long = "force-accept-remote-rollback", default_value_t = false)]
        force_accept_remote_rollback: bool,
        #[arg(long = "confirm-remote-rollback", default_value_t = false)]
        confirm_remote_rollback: bool,
        #[arg(long)]
        password: Option<String>,
    },
    Gc {
        #[command(subcommand)]
        command: GcCommand,
        #[arg(long)]
        repo: PathBuf,
    },
    Doctor {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        bundle: Option<PathBuf>,
    },
    Mount {
        #[command(subcommand)]
        command: MountCommand,
        #[arg(long)]
        repo: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum BranchCommand {
    List,
    Create { name: String },
    Checkout { name: String },
    Delete { name: String },
}

#[derive(Debug, Subcommand)]
enum ShareCommand {
    List,
    InviteMember {
        #[arg(long = "name")]
        name: String,
        #[arg(long = "out")]
        out: PathBuf,
    },
    AcceptMember {
        #[arg(long = "bundle")]
        bundle: PathBuf,
        #[arg(long = "label")]
        label: String,
    },
    InviteDevice {
        #[arg(long = "actor")]
        actor: String,
        #[arg(long = "label")]
        label: String,
        #[arg(long = "out")]
        out: PathBuf,
    },
    AcceptDevice {
        #[arg(long = "bundle")]
        bundle: PathBuf,
        #[arg(long = "label")]
        label: String,
    },
    RevokeMember {
        #[arg(long = "actor")]
        actor: String,
        #[arg(long = "password")]
        password: String,
    },
    RevokeDevice {
        #[arg(long = "device")]
        device: String,
        #[arg(long = "password")]
        password: String,
    },
}

#[derive(Debug, Subcommand)]
enum RemoteCommand {
    Add { name: String, url: String },
}

#[derive(Debug, Subcommand)]
enum VerifyCommand {
    Remote {
        #[arg(long = "sample")]
        sample_percent: String,
    },
}

#[derive(Debug, Subcommand)]
enum GcCommand {
    #[command(name = "--dry-run")]
    DryRun,
    #[command(name = "--execute")]
    Execute {
        #[arg(long = "grace-period")]
        grace_period_days: String,
        #[arg(
            long = "confirm-single-writer-maintenance-window",
            default_value_t = false
        )]
        confirm_single_writer_maintenance_window: bool,
    },
}

#[derive(Debug, Subcommand)]
enum MountCommand {
    Snapshot {
        #[arg(long)]
        snapshot: String,
        #[arg(long = "mount-point")]
        mount_point: PathBuf,
    },
    Branch {
        #[arg(long = "branch-token")]
        branch_token: String,
        #[arg(long = "mount-point")]
        mount_point: PathBuf,
    },
}

#[derive(Debug, Serialize)]
struct DoctorSummary {
    repo_root: PathBuf,
    repo_id: String,
    trusted_state: Option<e2v_sync::TrustedRemoteState>,
    remote_spec: String,
    remote_kind: String,
    gc_execute_supported: bool,
    gc_execute_blockers: Vec<String>,
}

#[derive(Debug, Serialize)]
struct DoctorBundleSummary {
    repo_id: String,
    trusted_state_present: bool,
    remote_kind: String,
    remote_spec_redacted: String,
    gc_execute_supported: bool,
    gc_execute_blockers: Vec<String>,
}

pub fn run_cli_for_test<I, S>(args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: Into<std::ffi::OsString> + Clone,
{
    let cli = Cli::parse_from(args);
    execute(cli)
}

pub fn run_from_env() -> Result<()> {
    let output = execute(Cli::parse())?;
    print!("{output}");
    Ok(())
}

fn execute(cli: Cli) -> Result<String> {
    let facade = RepositoryFacade::new();
    let sdk = Sdk::new();
    match cli.command {
        Command::Branch { command, repo } => match command {
            BranchCommand::List => {
                let branches = facade.list_branches(repo)?;
                Ok(branches
                    .into_iter()
                    .map(|branch| {
                        let marker = if branch.is_current { "*" } else { " " };
                        match branch.head_snapshot_id {
                            Some(head) => format!("{marker} {} {head}\n", branch.name),
                            None => format!("{marker} {}\n", branch.name),
                        }
                    })
                    .collect())
            }
            BranchCommand::Create { name } => {
                let branch = facade.create_branch(repo, &name)?;
                Ok(format!("created branch {}\n", branch.name))
            }
            BranchCommand::Checkout { name } => {
                let state = facade.checkout_branch(repo, &name)?;
                Ok(format!("checked out {}\n", state.branch.name))
            }
            BranchCommand::Delete { name } => {
                facade.delete_branch(repo, &name)?;
                Ok(format!("deleted branch {name}\n"))
            }
        },
        Command::Search { query, repo } => {
            let results = facade.search_filenames(&repo, &query)?;
            if !results.is_empty() {
                return Ok(results
                    .into_iter()
                    .map(|result| format!("{}\n", result.path))
                    .collect());
            }
            let metadata = facade.search_metadata(
                &repo,
                MetadataSearchQuery {
                    extension: Some(query),
                    path_prefix: None,
                    min_size: None,
                    max_size: None,
                },
            )?;
            Ok(metadata
                .into_iter()
                .map(|result| format!("{}\n", result.path))
                .collect())
        }
        Command::Share { command, repo } => match command {
            ShareCommand::List => {
                let listing = facade.share_list(&repo)?;
                let mut output = String::new();
                for actor in listing.actors {
                    output.push_str(&format!(
                        "{} {} {}\n",
                        actor.actor_id, actor.role, actor.display_name
                    ));
                }
                for device in listing.devices {
                    output.push_str(&format!(
                        "{} {} {} {}\n",
                        device.device_id, device.actor_id, device.status, device.label
                    ));
                }
                Ok(output)
            }
            ShareCommand::InviteMember { name, out } => {
                let invite = facade.share_invite_member(
                    &repo,
                    e2v_core::ShareInviteMemberOptions { display_name: name },
                )?;
                std::fs::write(&out, &invite.bundle_bytes)?;
                Ok(format!(
                    "wrote member invite {} to {}\n",
                    invite.actor_id,
                    out.display()
                ))
            }
            ShareCommand::AcceptMember { bundle, label } => {
                let invite_bytes = std::fs::read(&bundle)?;
                let accepted = facade.share_accept_member(
                    &repo,
                    e2v_core::ShareAcceptMemberOptions {
                        invite_bytes,
                        local_device_label: label,
                    },
                )?;
                Ok(format!(
                    "accepted member {} {} {}\n",
                    accepted.actor_id, accepted.device_id, accepted.role
                ))
            }
            ShareCommand::InviteDevice { actor, label, out } => {
                let invite = facade.share_invite_device(
                    &repo,
                    e2v_core::ShareInviteDeviceOptions {
                        actor_id: actor,
                        device_label: label,
                    },
                )?;
                std::fs::write(&out, &invite.bundle_bytes)?;
                Ok(format!(
                    "wrote device invite {} to {}\n",
                    invite.actor_id,
                    out.display()
                ))
            }
            ShareCommand::AcceptDevice { bundle, label } => {
                let invite_bytes = std::fs::read(&bundle)?;
                let accepted = facade.share_accept_device(
                    &repo,
                    e2v_core::ShareAcceptDeviceOptions {
                        invite_bytes,
                        local_device_label: label,
                    },
                )?;
                Ok(format!(
                    "accepted device {} {} {}\n",
                    accepted.actor_id, accepted.device_id, accepted.role
                ))
            }
            ShareCommand::RevokeMember { actor, password } => {
                facade.share_revoke_member(
                    &repo,
                    e2v_core::ShareRevokeMemberOptions { actor_id: actor.clone(), password },
                )?;
                Ok(format!("revoked member {actor}\n"))
            }
            ShareCommand::RevokeDevice { device, password } => {
                facade.share_revoke_device(
                    &repo,
                    e2v_core::ShareRevokeDeviceOptions {
                        device_id: device.clone(),
                        password,
                    },
                )?;
                Ok(format!("revoked device {device}\n"))
            }
        },
        Command::Remote { command, repo } => match command {
            RemoteCommand::Add { name, url } => {
                let stored = sdk.add_remote(&repo, &name, &url)?;
                Ok(format!("added remote {} -> {}\n", stored.name, stored.spec))
            }
        },
        Command::Verify { command, repo } => match command {
            VerifyCommand::Remote { sample_percent } => {
                let sample_percent = parse_percent_arg(&sample_percent)?;
                let remote_spec = parse_default_remote_spec(&sdk, &repo)?;
                let result = remote_spec.with_backend(|remote| match remote {
                    RemoteBackendRef::LocalFolder(remote) => verify_remote(
                        remote,
                        VerifyRemoteOptions {
                            repo_root: repo.clone(),
                            sample_percent,
                        },
                    ),
                    RemoteBackendRef::Webdav(remote) => verify_remote(
                        remote,
                        VerifyRemoteOptions {
                            repo_root: repo.clone(),
                            sample_percent,
                        },
                    ),
                })?;
                let head_snapshot_id = facade
                    .snapshots(&repo)?
                    .first()
                    .map(|snapshot| snapshot.snapshot_id.clone())
                    .unwrap_or_else(|| "no-head".to_string());
                Ok(format!(
                    "verified remote head {}: sampled {} objects, repaired {} local objects\n",
                    &head_snapshot_id[..head_snapshot_id.len().min(8)],
                    result.sampled_objects,
                    result.repaired_local_objects
                ))
            }
        },
        Command::Repair {
            repo,
            force_accept_remote_rollback,
            confirm_remote_rollback,
            password,
        } => {
            let remote_spec = parse_default_remote_spec(&sdk, &repo)?;
            if force_accept_remote_rollback {
                anyhow::ensure!(
                    confirm_remote_rollback,
                    "--confirm-remote-rollback is required with --force-accept-remote-rollback"
                );
                let password = password.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("--password is required with --force-accept-remote-rollback")
                })?;
                let result = remote_spec.with_backend(|remote| match remote {
                    RemoteBackendRef::LocalFolder(remote) => force_accept_remote_rollback_sync(
                        remote,
                        RepairRemoteOptions {
                            repo_root: repo.clone(),
                        },
                        password,
                    ),
                    RemoteBackendRef::Webdav(remote) => force_accept_remote_rollback_sync(
                        remote,
                        RepairRemoteOptions {
                            repo_root: repo.clone(),
                        },
                        password,
                    ),
                })?;
                Ok(format!(
                    "accepted remote rollback and rebuilt local fact view from remote; repaired {} local objects\n",
                    result.repaired_objects
                ))
            } else {
                let result = remote_spec.with_backend(|remote| match remote {
                    RemoteBackendRef::LocalFolder(remote) => repair_remote(
                        remote,
                        RepairRemoteOptions {
                            repo_root: repo.clone(),
                        },
                    ),
                    RemoteBackendRef::Webdav(remote) => repair_remote(
                        remote,
                        RepairRemoteOptions {
                            repo_root: repo.clone(),
                        },
                    ),
                })?;
                Ok(format!(
                    "repaired {} local objects\n",
                    result.repaired_objects
                ))
            }
        }
        Command::Gc { command, repo } => {
            let remote_spec = parse_default_remote_spec(&sdk, &repo)?;
            match command {
                GcCommand::DryRun => {
                    let report = remote_spec.with_backend(|remote| match remote {
                        RemoteBackendRef::LocalFolder(remote) => gc_dry_run(
                            remote,
                            GcDryRunOptions {
                                repo_root: repo.clone(),
                            },
                        ),
                        RemoteBackendRef::Webdav(remote) => gc_dry_run(
                            remote,
                            GcDryRunOptions {
                                repo_root: repo.clone(),
                            },
                        ),
                    })?;
                    Ok(format!(
                        "gc dry-run: {} unreachable physical refs, {} active intents\n",
                        report.unreachable_physical_refs.len(),
                        report.active_intent_paths.len()
                    ))
                }
                GcCommand::Execute {
                    grace_period_days,
                    confirm_single_writer_maintenance_window,
                } => {
                    let grace_period_days = parse_grace_period_days(&grace_period_days)?;
                    let result = remote_spec.with_backend(|remote| match remote {
                        RemoteBackendRef::LocalFolder(remote) => gc_execute(
                            remote,
                            GcExecuteOptions {
                                repo_root: repo.clone(),
                                grace_period_days,
                                allow_single_writer_maintenance_window:
                                    confirm_single_writer_maintenance_window,
                            },
                        ),
                        RemoteBackendRef::Webdav(remote) => gc_execute(
                            remote,
                            GcExecuteOptions {
                                repo_root: repo.clone(),
                                grace_period_days,
                                allow_single_writer_maintenance_window:
                                    confirm_single_writer_maintenance_window,
                            },
                        ),
                    })?;
                    Ok(format!(
                        "gc execute: deleted {} physical refs\n",
                        result.deleted_physical_refs.len()
                    ))
                }
            }
        }
        Command::Doctor { repo, bundle } => {
            let stored_remote = sdk.load_default_remote(&repo)?;
            let repo_id = read_repo_id(&repo)?;
            let trusted_state = load_trusted_remote_state_for_repo(&repo_id)?;
            let remote_spec = parse_default_remote_spec(&sdk, &repo)?;
            let (remote_kind, gc_execute_supported, gc_execute_blockers) = remote_spec
                .with_backend(|remote| {
                    let capability = remote_capability(&remote);
                    let gc_status = gc_execute_capability_status(capability);
                    Ok((
                        remote_kind_label(&remote).to_string(),
                        gc_status.supported,
                        gc_status
                            .blockers
                            .into_iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>(),
                    ))
                })?;
            let summary = DoctorSummary {
                repo_root: repo.clone(),
                repo_id,
                trusted_state,
                remote_spec: stored_remote.spec,
                remote_kind,
                gc_execute_supported,
                gc_execute_blockers,
            };
            if let Some(bundle_root) = bundle {
                write_doctor_bundle(&bundle_root, &summary)?;
                Ok(format!(
                    "{}\nbundle={}\n",
                    serde_json::to_string_pretty(&summary)?,
                    bundle_root.display()
                ))
            } else {
                Ok(format!("{}\n", serde_json::to_string_pretty(&summary)?))
            }
        }
        Command::Mount { command, repo } => match command {
            MountCommand::Snapshot {
                snapshot,
                mount_point,
            } => {
                let summary = mount_snapshot(repo, snapshot, mount_point)?;
                Ok(format_mount_summary(&summary))
            }
            MountCommand::Branch {
                branch_token,
                mount_point,
            } => {
                let summary = mount_live_branch(repo, branch_token, mount_point)?;
                Ok(format_mount_summary(&summary))
            }
        },
    }
}

fn parse_percent_arg(value: &str) -> Result<u8> {
    let trimmed = value.trim();
    let number = trimmed.strip_suffix('%').unwrap_or(trimmed);
    let parsed: u8 = number.parse()?;
    anyhow::ensure!(
        (1..=100).contains(&parsed),
        "sample percent must be between 1 and 100"
    );
    Ok(parsed)
}

fn parse_grace_period_days(value: &str) -> Result<u64> {
    let trimmed = value.trim();
    let number = trimmed.strip_suffix('d').unwrap_or(trimmed);
    let parsed: u64 = number.parse()?;
    anyhow::ensure!(parsed > 0, "grace period must be greater than zero");
    Ok(parsed)
}

fn remote_capability<'a>(remote: &'a RemoteBackendRef<'a>) -> &'a BackendCapability {
    match remote {
        RemoteBackendRef::LocalFolder(remote) => remote.capability(),
        RemoteBackendRef::Webdav(remote) => remote.capability(),
    }
}

fn remote_kind_label(remote: &RemoteBackendRef<'_>) -> &'static str {
    match remote {
        RemoteBackendRef::LocalFolder(_) => "local-folder",
        RemoteBackendRef::Webdav(_) => "webdav",
    }
}

fn write_doctor_bundle(bundle_root: &std::path::Path, summary: &DoctorSummary) -> Result<()> {
    std::fs::create_dir_all(bundle_root)?;
    let bundle_summary = DoctorBundleSummary {
        repo_id: summary.repo_id.clone(),
        trusted_state_present: summary.trusted_state.is_some(),
        remote_kind: summary.remote_kind.clone(),
        remote_spec_redacted: redact_remote_spec(&summary.remote_spec),
        gc_execute_supported: summary.gc_execute_supported,
        gc_execute_blockers: summary.gc_execute_blockers.clone(),
    };
    std::fs::write(
        bundle_root.join("doctor-summary.json"),
        serde_json::to_vec_pretty(&bundle_summary)?,
    )?;
    let trusted_state_bytes = match &summary.trusted_state {
        Some(trusted_state) => serde_json::to_vec_pretty(trusted_state)?,
        None => serde_json::to_vec_pretty(&serde_json::Value::Null)?,
    };
    std::fs::write(bundle_root.join("trusted-state.json"), trusted_state_bytes)?;
    Ok(())
}

fn redact_remote_spec(spec: &str) -> String {
    if spec.starts_with("file://") || spec.starts_with("file+") {
        return "file://<redacted>".to_string();
    }
    if let Ok(url) = url::Url::parse(spec) {
        return format!("{}://<redacted>", url.scheme());
    }
    "<redacted>".to_string()
}

fn parse_default_remote_spec(sdk: &Sdk, repo_root: &std::path::Path) -> Result<e2v_sync::RemoteSpec> {
    let stored = sdk.load_default_remote(repo_root)?;
    e2v_sync::RemoteSpec::parse(&stored.spec)
}

fn format_mount_summary(summary: &MountLaunchSummary) -> String {
    let access_mode = if summary.read_only {
        "read-only"
    } else {
        "writable"
    };
    let io_mode = if summary.stream_only {
        "stream-only"
    } else {
        "disk-like"
    };
    format!(
        "mounted {} at {} with {:?}, {}, {} ({})\n",
        summary.mount_mode,
        summary.mount_point.display(),
        summary.cache_policy,
        access_mode,
        io_mode,
        summary.status_message
    )
}
