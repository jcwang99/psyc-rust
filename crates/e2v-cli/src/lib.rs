use std::{
    io::Write,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use clap::{Parser, Subcommand};
use e2v_api::{
    CheckoutSnapshotOptions, CloneRequest, CommitRepositoryOptions, EnableObliviousLayoutRequest,
    FetchRequest, GcExecuteRequest, HistoricalRewriteExecuteRequest, HistoricalRewritePlanRequest,
    InitRepositoryOptions, ObliviousLayoutPlanRequest, PullRequest, PushRequest,
    ReshuffleObliviousLayoutRequest, Sdk, ShareAcceptDeviceRequest, ShareAcceptMemberRequest,
    ShareInviteDeviceRequest, ShareInviteMemberRequest, ShareRevokeDeviceRequest,
    ShareRevokeMemberRequest, VerifyRemoteRequest,
};
use e2v_core::{MetadataSearchQuery, RepositoryFacade};
use e2v_sync::{ServeOptions, serve_local_web};
use e2v_vfs::{
    MountLaunchState, MountLaunchSummary, start_live_branch_mount, start_snapshot_mount,
};
#[cfg(not(windows))]
use e2v_vfs::{mount_live_branch, mount_snapshot};

#[derive(Debug, Parser)]
#[command(name = "e2v")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Init {
        repo: PathBuf,
        #[arg(long)]
        password: String,
        #[arg(long, default_value = "main")]
        branch: String,
    },
    Commit {
        #[arg(long)]
        repo: PathBuf,
        #[arg(short = 'm', long = "message")]
        message: String,
    },
    Snapshots {
        #[arg(long)]
        repo: PathBuf,
    },
    Checkout {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        snapshot: String,
        #[arg(long = "target")]
        target_dir: PathBuf,
    },
    Push {
        #[arg(long)]
        repo: PathBuf,
    },
    Fetch {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        password: Option<String>,
    },
    Pull {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        password: Option<String>,
    },
    Clone {
        remote_spec: String,
        target_repo_root: PathBuf,
        #[arg(long)]
        password: String,
        #[arg(long = "branch-token")]
        branch_token: String,
    },
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
    HistoricalRewrite {
        #[command(subcommand)]
        command: HistoricalRewriteCommand,
        #[arg(long)]
        repo: PathBuf,
    },
    Oram {
        #[command(subcommand)]
        command: OramCommand,
        #[arg(long)]
        repo: PathBuf,
    },
    Doctor {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        bundle: Option<PathBuf>,
    },
    Serve {
        #[arg(long)]
        repo: PathBuf,
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
    Snapshot {
        snapshot_id: String,
    },
    Object {
        expected_type: String,
        object_id: String,
    },
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
enum HistoricalRewriteCommand {
    Plan,
    Execute {
        #[arg(long)]
        password: String,
        #[arg(long = "confirm-full-reencryption", default_value_t = false)]
        confirm_full_reencryption: bool,
    },
}

#[derive(Debug, Subcommand)]
enum OramCommand {
    Plan,
    Status,
    Enable {
        #[arg(long = "policy", default_value = "balanced")]
        policy: String,
    },
    Reshuffle {
        #[arg(long = "policy", default_value = "balanced")]
        policy: String,
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

pub fn run_from_env() -> Result<()> {
    match Cli::parse() {
        Cli {
            command: Command::Serve { repo },
        } => run_serve_command(repo),
        Cli {
            command: Command::Mount { repo, command },
        } => run_mount_command(repo, command),
        cli => {
            let output = execute(cli)?;
            print!("{output}");
            Ok(())
        }
    }
}

fn execute(cli: Cli) -> Result<String> {
    let facade = RepositoryFacade::new();
    let sdk = Sdk::new();
    match cli.command {
        Command::Init {
            repo,
            password,
            branch,
        } => {
            let repo = sdk.init_repository(InitRepositoryOptions {
                repo_root: repo,
                password,
                branch_name: branch,
            })?;
            Ok(format!("initialized repository {}\n", repo.branch.name))
        }
        Command::Commit { repo, message } => {
            let commit = sdk.commit_repository(CommitRepositoryOptions {
                repo_root: repo,
                message,
            })?;
            Ok(format!(
                "committed {}\n",
                &commit.snapshot_id[..commit.snapshot_id.len().min(8)]
            ))
        }
        Command::Snapshots { repo } => {
            let snapshots = sdk.list_snapshots(&repo)?;
            Ok(snapshots
                .into_iter()
                .map(|snapshot| format!("{} {}\n", snapshot.snapshot_id, snapshot.message))
                .collect())
        }
        Command::Checkout {
            repo,
            snapshot,
            target_dir,
        } => {
            sdk.checkout_snapshot(CheckoutSnapshotOptions {
                repo_root: repo,
                snapshot_id: snapshot.clone(),
                target_dir: target_dir.clone(),
            })?;
            Ok(format!(
                "checked out {} to {}\n",
                &snapshot[..snapshot.len().min(8)],
                target_dir.display()
            ))
        }
        Command::Push { repo } => {
            let branch_token = sdk.open_repository(&repo)?.branch.token_hex;
            let pushed = sdk.push_default_remote(PushRequest {
                repo_root: repo,
                branch_token,
                operation_id: cli_operation_id("push")?,
            })?;
            Ok(format!(
                "pushed {}\n",
                &pushed.published_snapshot_id[..pushed.published_snapshot_id.len().min(8)]
            ))
        }
        Command::Fetch { repo, password } => {
            let branch_token = sdk.open_repository(&repo)?.branch.token_hex;
            let fetched = sdk.fetch_default_remote(FetchRequest {
                repo_root: repo,
                branch_token,
                password,
            })?;
            Ok(format!(
                "downloaded {} objects\n",
                fetched.downloaded_objects
            ))
        }
        Command::Pull { repo, password } => {
            let branch_token = sdk.open_repository(&repo)?.branch.token_hex;
            let pulled = sdk.pull_default_remote(PullRequest {
                repo_root: repo,
                branch_token,
                password,
            })?;
            Ok(format!(
                "pulled {}\n",
                &pulled.snapshot_id[..pulled.snapshot_id.len().min(8)]
            ))
        }
        Command::Clone {
            remote_spec,
            target_repo_root,
            password,
            branch_token,
        } => {
            let cloned = sdk.clone_remote(CloneRequest {
                remote_spec,
                target_repo_root,
                password,
                branch_token,
            })?;
            Ok(format!(
                "cloned {}\n",
                cloned
                    .head_snapshot_id
                    .unwrap_or_else(|| "no-head".to_string())
            ))
        }
        Command::Branch { command, repo } => match command {
            BranchCommand::List => {
                let branches = sdk.list_branches(&repo)?;
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
                let branch = sdk.create_branch(&repo, &name)?;
                Ok(format!("created branch {}\n", branch.name))
            }
            BranchCommand::Checkout { name } => {
                let state = sdk.checkout_branch(&repo, &name)?;
                Ok(format!("checked out {}\n", state.branch.name))
            }
            BranchCommand::Delete { name } => {
                sdk.delete_branch(&repo, &name)?;
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
                let listing = sdk.share_list(&repo)?;
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
                let invite = sdk
                    .share_invite_member(&repo, ShareInviteMemberRequest { display_name: name })?;
                std::fs::write(&out, &invite.bundle_bytes)?;
                Ok(format!(
                    "wrote member invite {} to {}\n",
                    invite.actor_id,
                    out.display()
                ))
            }
            ShareCommand::AcceptMember { bundle, label } => {
                let invite_bytes = std::fs::read(&bundle)?;
                let accepted = sdk.share_accept_member(
                    &repo,
                    ShareAcceptMemberRequest {
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
                let invite = sdk.share_invite_device(
                    &repo,
                    ShareInviteDeviceRequest {
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
                let accepted = sdk.share_accept_device(
                    &repo,
                    ShareAcceptDeviceRequest {
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
                sdk.share_revoke_member(
                    &repo,
                    ShareRevokeMemberRequest {
                        actor_id: actor.clone(),
                        password,
                    },
                )?;
                Ok(format!("revoked member {actor}\n"))
            }
            ShareCommand::RevokeDevice { device, password } => {
                sdk.share_revoke_device(
                    &repo,
                    ShareRevokeDeviceRequest {
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
            VerifyCommand::Snapshot { snapshot_id } => {
                facade.verify_snapshot(&repo, &snapshot_id)?;
                Ok(format!(
                    "verified snapshot {}\n",
                    &snapshot_id[..snapshot_id.len().min(8)]
                ))
            }
            VerifyCommand::Object {
                expected_type,
                object_id,
            } => {
                facade.verify_object(&repo, &object_id, &expected_type)?;
                Ok(format!(
                    "verified object {expected_type} {}\n",
                    &object_id[..object_id.len().min(8)]
                ))
            }
            VerifyCommand::Remote { sample_percent } => {
                let sample_percent = parse_percent_arg(&sample_percent)?;
                let result = sdk.verify_default_remote(VerifyRemoteRequest {
                    repo_root: repo.clone(),
                    sample_percent,
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
            if force_accept_remote_rollback {
                anyhow::ensure!(
                    confirm_remote_rollback,
                    "--confirm-remote-rollback is required with --force-accept-remote-rollback"
                );
                let password = password.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("--password is required with --force-accept-remote-rollback")
                })?;
                let result = sdk.force_accept_default_remote_rollback(&repo, password)?;
                Ok(format!(
                    "accepted remote rollback and rebuilt local fact view from remote; repaired {} local objects\n",
                    result.repaired_objects
                ))
            } else {
                let result = sdk.repair_default_remote(&repo)?;
                Ok(format!(
                    "repaired {} local objects\n",
                    result.repaired_objects
                ))
            }
        }
        Command::Gc { command, repo } => match command {
            GcCommand::DryRun => {
                let report = sdk.gc_default_remote_dry_run(&repo)?;
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
                let result = sdk.gc_default_remote_execute(GcExecuteRequest {
                    repo_root: repo.clone(),
                    grace_period_days,
                    allow_single_writer_maintenance_window:
                        confirm_single_writer_maintenance_window,
                })?;
                Ok(format!(
                    "gc execute: deleted {} physical refs\n",
                    result.deleted_physical_refs.len()
                ))
            }
        },
        Command::HistoricalRewrite { command, repo } => match command {
            HistoricalRewriteCommand::Plan => {
                let plan =
                    sdk.historical_rewrite_default_remote_plan(HistoricalRewritePlanRequest {
                        repo_root: repo.clone(),
                    })?;
                let mut output = String::new();
                output.push_str("historical strong revocation plan\n");
                output.push_str(
                    "note: future-only revocation stops new access, while historical strong revocation rewrites reachable history onto the active epoch.\n",
                );
                output.push_str(&format!(
                    "reachable objects: {}\nremote loose objects: {}\nremote packed objects: {}\nold epochs: {}\n",
                    plan.reachable_object_count,
                    plan.remote_loose_object_count,
                    plan.remote_pack_object_count,
                    plan.old_epoch_count
                ));
                if let Some(advisory) = plan.large_repo_advisory {
                    output.push_str(&format!("advisory: {advisory}\n"));
                }
                if plan.requires_remote_credential_revocation_guidance {
                    output.push_str(
                        "advisory: revoke remote storage credentials first for previously shared backends or large repositories.\n",
                    );
                }
                Ok(output)
            }
            HistoricalRewriteCommand::Execute {
                password,
                confirm_full_reencryption,
            } => {
                anyhow::ensure!(
                    confirm_full_reencryption,
                    "--confirm-full-reencryption is required for historical strong revocation"
                );
                let result = sdk.historical_rewrite_default_remote_execute(
                    HistoricalRewriteExecuteRequest {
                        repo_root: repo,
                        password,
                        confirm_full_reencryption,
                    },
                )?;
                Ok(format!(
                    "historical strong revocation complete: rewritten {} objects, retired {} old epochs, deleted {} stale remote refs\n",
                    result.rewritten_objects,
                    result.retired_epoch_count,
                    result.deleted_stale_remote_refs.len()
                ))
            }
        },
        Command::Oram { command, repo } => match command {
            OramCommand::Plan => {
                let plan =
                    sdk.oblivious_layout_default_remote_plan(ObliviousLayoutPlanRequest {
                        repo_root: repo.clone(),
                    })?;
                let mut output = String::new();
                output.push_str("oblivious layout plan\n");
                output.push_str(&format!(
                    "real reads: {}\ncover reads: {}\nbytes/request: {}\nwrite amplification: {}\n",
                    plan.estimated_real_reads_per_request,
                    plan.estimated_cover_reads_per_request,
                    plan.estimated_bytes_per_request,
                    plan.estimated_write_amplification
                ));
                for advisory in plan.advisory_messages {
                    output.push_str(&format!("advisory: {advisory}\n"));
                }
                Ok(output)
            }
            OramCommand::Status => {
                let status = sdk.oblivious_layout_default_remote_status(&repo)?;
                Ok(format!(
                    "mode: {}\ndedup: {}\nlayout generation: {}\noblivious generation: {}\npolicy: {}\n",
                    status.layout_mode,
                    status.dedup_mode,
                    status.layout_generation,
                    status
                        .oblivious_generation
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "none".to_string()),
                    status.policy_profile
                ))
            }
            OramCommand::Enable { policy } => {
                let status =
                    sdk.enable_oblivious_layout_default_remote(EnableObliviousLayoutRequest {
                        repo_root: repo,
                        policy_profile: policy,
                    })?;
                Ok(format!(
                    "oblivious layout enabled: mode={}, dedup={}, layout_generation={}, oblivious_generation={}\n",
                    status.layout_mode,
                    status.dedup_mode,
                    status.layout_generation,
                    status
                        .oblivious_generation
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "none".to_string())
                ))
            }
            OramCommand::Reshuffle { policy } => {
                let status = sdk.reshuffle_oblivious_layout_default_remote(
                    ReshuffleObliviousLayoutRequest {
                        repo_root: repo,
                        policy_profile: policy,
                    },
                )?;
                Ok(format!(
                    "oblivious layout reshuffled: layout_generation={}, oblivious_generation={}\n",
                    status.layout_generation,
                    status
                        .oblivious_generation
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "none".to_string())
                ))
            }
        },
        Command::Doctor { repo, bundle } => {
            let summary = sdk.inspect_default_remote(&repo)?;
            if let Some(bundle_root) = bundle {
                sdk.write_doctor_bundle(&bundle_root, &summary)?;
                Ok(format!(
                    "{}\nbundle={}\n",
                    serde_json::to_string_pretty(&summary)?,
                    bundle_root.display()
                ))
            } else {
                Ok(format!("{}\n", serde_json::to_string_pretty(&summary)?))
            }
        }
        Command::Serve { .. } => anyhow::bail!("serve command requires the CLI process entrypoint"),
        Command::Mount { .. } => anyhow::bail!("mount command requires the CLI process entrypoint"),
    }
}

#[doc(hidden)]
pub mod testing {
    use anyhow::Result;
    use clap::Parser;

    use super::{Cli, execute};

    pub fn run_cli<I, S>(args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: Into<std::ffi::OsString> + Clone,
    {
        let cli = Cli::parse_from(args);
        execute(cli)
    }
}

fn run_serve_command(repo: PathBuf) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    let handle = runtime.block_on(serve_local_web(ServeOptions { repo_root: repo }))?;
    println!("serving http://{}/", handle.local_addr());
    std::io::stdout().flush()?;
    loop {
        std::thread::park();
    }
}

fn run_mount_command(repo: PathBuf, command: MountCommand) -> Result<()> {
    match command {
        MountCommand::Snapshot {
            snapshot,
            mount_point,
        } => {
            #[cfg(windows)]
            {
                let mounted = start_snapshot_mount(repo, snapshot, mount_point)?;
                print!("{}", format_mount_summary(mounted.summary()));
                std::io::stdout().flush()?;
                loop {
                    let _keep_mount_alive = &mounted;
                    std::thread::park();
                }
            }
            #[cfg(not(windows))]
            {
                let summary = mount_snapshot(repo, snapshot, mount_point)?;
                print!("{}", format_mount_summary(&summary));
                std::io::stdout().flush()?;
                Ok(())
            }
        }
        MountCommand::Branch {
            branch_token,
            mount_point,
        } => {
            #[cfg(windows)]
            {
                let mounted = start_live_branch_mount(repo, branch_token, mount_point)?;
                print!("{}", format_mount_summary(mounted.summary()));
                std::io::stdout().flush()?;
                loop {
                    let _keep_mount_alive = &mounted;
                    std::thread::park();
                }
            }
            #[cfg(not(windows))]
            {
                let summary = mount_live_branch(repo, branch_token, mount_point)?;
                print!("{}", format_mount_summary(&summary));
                std::io::stdout().flush()?;
                Ok(())
            }
        }
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

fn cli_operation_id(prefix: &str) -> Result<String> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| anyhow::anyhow!("system clock error: {error}"))?
        .as_millis();
    Ok(format!("cli-{prefix}-{millis}"))
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
    let launch_verb = match summary.launch_state {
        MountLaunchState::SummaryOnly => "prepared",
        MountLaunchState::HostActive => "mounted",
    };
    format!(
        "{} {} at {} with {:?}, {}, {} ({})\n",
        launch_verb,
        summary.mount_mode,
        summary.mount_point.display(),
        summary.cache_policy,
        access_mode,
        io_mode,
        summary.status_message
    )
}

#[cfg(test)]
mod tests {
    use super::format_mount_summary;
    use e2v_vfs::{CachePolicy, MountLaunchState, MountLaunchSummary};
    use std::path::PathBuf;

    #[test]
    fn summary_only_mount_output_does_not_claim_host_is_mounted() {
        let summary = MountLaunchSummary {
            mount_mode: "snapshot-pinned".to_string(),
            mount_point: PathBuf::from("Z:\\"),
            cache_policy: CachePolicy::DirectIoFallback,
            read_only: true,
            stream_only: true,
            launch_state: MountLaunchState::SummaryOnly,
            status_message: "host not started".to_string(),
        };

        let rendered = format_mount_summary(&summary);

        assert!(
            rendered.starts_with("prepared snapshot-pinned"),
            "summary-only launch should not claim an active mount, got: {rendered:?}"
        );
    }
}
