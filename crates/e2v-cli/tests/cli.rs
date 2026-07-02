use std::{
    fs,
    io::{Read, Write},
    net::TcpStream,
    process::{Child, Command, Stdio},
    thread,
    time::Duration,
};

use e2v_core::{CommitOptions, InitOptions, RepositoryFacade};
use e2v_store::BlobStore;
use e2v_store::LocalFolderBackend;
use e2v_sync::{push_head, PushOptions};
use tempfile::tempdir;

fn cli_binary_path() -> std::path::PathBuf {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_e2v-cli") {
        return std::path::PathBuf::from(path);
    }

    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("debug")
        .join(if cfg!(windows) {
            "e2v-cli.exe"
        } else {
            "e2v-cli"
        })
}

struct ChildGuard {
    child: Child,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self { child }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn file_remote_spec(path: &std::path::Path) -> String {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap().join(path)
    };
    let mut normalized = absolute.to_string_lossy().replace('\\', "/");
    if let Some(stripped) = normalized.strip_prefix("//?/") {
        normalized = stripped.to_string();
    }
    format!("file:///{normalized}")
}

fn init_repo(repo_root: &std::path::Path) {
    RepositoryFacade::new()
        .init(InitOptions {
            repo_root: repo_root.to_path_buf(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
}

fn cli_lib_source_without_whitespace() -> String {
    fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("lib.rs"),
    )
    .unwrap()
    .chars()
    .filter(|ch| !ch.is_whitespace())
    .collect()
}

fn init_shared_repo(repo_root: &std::path::Path) -> (RepositoryFacade, String) {
    init_repo(repo_root);
    let facade = RepositoryFacade::new();
    let owner_credential_bytes = fs::read(
        repo_root
            .join(".e2v")
            .join("device")
            .join("local-device.json"),
    )
    .unwrap();
    let invite = facade
        .share_invite_member(
            repo_root,
            e2v_core::ShareInviteMemberOptions {
                display_name: "Alice".to_string(),
            },
        )
        .unwrap();
    let actor_id = invite.actor_id.clone();
    facade
        .share_accept_member(
            repo_root,
            e2v_core::ShareAcceptMemberOptions {
                invite_bytes: invite.bundle_bytes,
                local_device_label: "alice-laptop".to_string(),
            },
        )
        .unwrap();
    fs::write(
        repo_root
            .join(".e2v")
            .join("device")
            .join("local-device.json"),
        owner_credential_bytes,
    )
    .unwrap();
    (facade, actor_id)
}

#[test]
fn branch_commands_create_list_and_checkout() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);
    fs::write(repo_root.join("tracked.txt"), "base").unwrap();
    RepositoryFacade::new()
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "base".to_string(),
        })
        .unwrap();

    let create_output = e2v_cli::run_cli_for_test([
        "e2v",
        "branch",
        "--repo",
        repo_root.to_str().unwrap(),
        "create",
        "feature",
    ])
    .unwrap();
    assert!(create_output.contains("feature"));

    let list_output = e2v_cli::run_cli_for_test([
        "e2v",
        "branch",
        "--repo",
        repo_root.to_str().unwrap(),
        "list",
    ])
    .unwrap();
    assert!(list_output.contains("* main"));
    assert!(list_output.contains("feature"));

    let checkout_output = e2v_cli::run_cli_for_test([
        "e2v",
        "branch",
        "--repo",
        repo_root.to_str().unwrap(),
        "checkout",
        "feature",
    ])
    .unwrap();
    assert!(checkout_output.contains("feature"));

    let list_after = e2v_cli::run_cli_for_test([
        "e2v",
        "branch",
        "--repo",
        repo_root.to_str().unwrap(),
        "list",
    ])
    .unwrap();
    assert!(list_after.contains("* feature"));
}

#[test]
fn search_command_prints_filename_matches() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);
    fs::write(repo_root.join("alpha-notes.txt"), "alpha").unwrap();
    fs::write(repo_root.join("beta.txt"), "beta").unwrap();
    RepositoryFacade::new()
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap();

    let output = e2v_cli::run_cli_for_test([
        "e2v",
        "search",
        "notes",
        "--repo",
        repo_root.to_str().unwrap(),
    ])
    .unwrap();

    assert!(output.contains("alpha-notes.txt"));
    assert!(!output.contains("beta.txt"));
}

#[test]
fn mount_snapshot_command_delegates_to_e2v_vfs() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);
    fs::write(repo_root.join("tracked.txt"), "alpha").unwrap();
    let snapshot_id = RepositoryFacade::new()
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap()
        .snapshot_id;

    let output = e2v_cli::run_cli_for_test([
        "e2v",
        "mount",
        "--repo",
        repo_root.to_str().unwrap(),
        "snapshot",
        "--snapshot",
        &snapshot_id,
        "--mount-point",
        "X:",
    ])
    .unwrap();

    assert!(output.contains("snapshot-pinned"));
    assert!(output.contains("X:"));
    assert!(output.contains("KernelCacheWithInvalidation"));
    assert!(output.contains("read-only"));
    assert!(output.contains("stream-only"));
}

#[test]
fn mount_branch_command_delegates_to_e2v_vfs() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);
    fs::write(repo_root.join("tracked.txt"), "alpha").unwrap();
    RepositoryFacade::new()
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap();
    let branch_token = RepositoryFacade::new()
        .open(&repo_root)
        .unwrap()
        .branch
        .token_hex;

    let output = e2v_cli::run_cli_for_test([
        "e2v",
        "mount",
        "--repo",
        repo_root.to_str().unwrap(),
        "branch",
        "--branch-token",
        &branch_token,
        "--mount-point",
        "Y:",
    ])
    .unwrap();

    assert!(output.contains("live-branch"));
    assert!(output.contains("Y:"));
    assert!(output.contains("KernelCacheWithInvalidation"));
    assert!(output.contains("read-only"));
    assert!(output.contains("stream-only"));
}

#[test]
fn serve_command_starts_local_web_server_and_prints_localhost_url() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);
    fs::write(repo_root.join("tracked.txt"), "alpha").unwrap();
    RepositoryFacade::new()
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap();

    let bin = cli_binary_path();
    let child = Command::new(bin)
        .args(["serve", "--repo", repo_root.to_str().unwrap()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut child = ChildGuard::new(child);

    let mut stdout = child.child.stdout.take().unwrap();
    let mut buffer = String::new();
    for _ in 0..40 {
        let mut chunk = [0u8; 256];
        let read = stdout.read(&mut chunk).unwrap_or(0);
        if read > 0 {
            buffer.push_str(&String::from_utf8_lossy(&chunk[..read]));
            if buffer.contains('\n') {
                break;
            }
        }
        thread::sleep(Duration::from_millis(100));
    }

    let first_line = buffer.lines().next().unwrap_or_default().trim().to_string();
    assert!(
        first_line.starts_with("serving http://127.0.0.1:"),
        "serve command should print localhost address, got stdout: {buffer:?}"
    );

    let authority = first_line
        .strip_prefix("serving http://")
        .expect("serve command should print serving URL")
        .trim_end_matches('/');
    let mut stream = TcpStream::connect(authority).unwrap();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "serve command should return 200 for home page, got: {response}"
    );
    assert!(response.contains("Snapshots"));
}

#[test]
fn remote_add_persists_default_remote_spec() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(&repo_root).unwrap();
    fs::create_dir_all(&remote_root).unwrap();
    init_repo(&repo_root);
    let spec = file_remote_spec(&remote_root);

    let output = e2v_cli::run_cli_for_test([
        "e2v",
        "remote",
        "--repo",
        repo_root.to_str().unwrap(),
        "add",
        "origin",
        &spec,
    ])
    .unwrap();

    assert!(output.contains("origin"));
    let config_path = repo_root.join(".e2v").join("remotes").join("default.json");
    let stored = fs::read_to_string(config_path).unwrap();
    assert!(stored.contains("\"name\":\"origin\""));
    assert!(stored.contains(&format!("\"spec\":\"{spec}\"")));
}

#[test]
fn share_list_prints_member_and_device_records() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_shared_repo(&repo_root);

    let output = e2v_cli::run_cli_for_test([
        "e2v",
        "share",
        "--repo",
        repo_root.to_str().unwrap(),
        "list",
    ])
    .unwrap();

    assert!(output.contains("owner_admin"));
    assert!(output.contains("writer_member"));
    assert!(output.contains("alice-laptop"));
}

#[test]
fn share_member_round_trip_via_bundle_file() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);
    let owner_credential_bytes = fs::read(
        repo_root
            .join(".e2v")
            .join("device")
            .join("local-device.json"),
    )
    .unwrap();
    let bundle_path = temp.path().join("invite.e2vshare");

    let invite_output = e2v_cli::run_cli_for_test([
        "e2v",
        "share",
        "--repo",
        repo_root.to_str().unwrap(),
        "invite-member",
        "--name",
        "Alice",
        "--out",
        bundle_path.to_str().unwrap(),
    ])
    .unwrap();
    assert!(invite_output.contains("invite"));
    assert!(bundle_path.is_file());

    let accept_output = e2v_cli::run_cli_for_test([
        "e2v",
        "share",
        "--repo",
        repo_root.to_str().unwrap(),
        "accept-member",
        "--bundle",
        bundle_path.to_str().unwrap(),
        "--label",
        "alice-laptop",
    ])
    .unwrap();
    assert!(accept_output.contains("writer_member"));

    let listing = e2v_cli::run_cli_for_test([
        "e2v",
        "share",
        "--repo",
        repo_root.to_str().unwrap(),
        "list",
    ])
    .unwrap();
    assert!(listing.contains("Alice"));
    assert!(listing.contains("alice-laptop"));
    fs::write(
        repo_root
            .join(".e2v")
            .join("device")
            .join("local-device.json"),
        owner_credential_bytes,
    )
    .unwrap();

    let actor_id = listing
        .lines()
        .find_map(|line| {
            if line.contains("writer_member") {
                line.split_whitespace()
                    .next()
                    .map(|value| value.to_string())
            } else {
                None
            }
        })
        .unwrap();
    let revoke_output = e2v_cli::run_cli_for_test([
        "e2v",
        "share",
        "--repo",
        repo_root.to_str().unwrap(),
        "revoke-member",
        "--actor",
        &actor_id,
        "--password",
        "correct horse battery staple",
    ])
    .unwrap();
    assert!(revoke_output.contains("revoked"));
}

#[test]
fn share_device_round_trip_via_bundle_file() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    let (_facade, actor_id) = init_shared_repo(&repo_root);
    let owner_credential_bytes = fs::read(
        repo_root
            .join(".e2v")
            .join("device")
            .join("local-device.json"),
    )
    .unwrap();
    let bundle_path = temp.path().join("device.e2vshare");

    let invite_output = e2v_cli::run_cli_for_test([
        "e2v",
        "share",
        "--repo",
        repo_root.to_str().unwrap(),
        "invite-device",
        "--actor",
        &actor_id,
        "--label",
        "alice-phone",
        "--out",
        bundle_path.to_str().unwrap(),
    ])
    .unwrap();
    assert!(invite_output.contains("invite"));
    assert!(bundle_path.is_file());

    let accept_output = e2v_cli::run_cli_for_test([
        "e2v",
        "share",
        "--repo",
        repo_root.to_str().unwrap(),
        "accept-device",
        "--bundle",
        bundle_path.to_str().unwrap(),
        "--label",
        "alice-phone",
    ])
    .unwrap();
    assert!(accept_output.contains("accepted device"));
    fs::write(
        repo_root
            .join(".e2v")
            .join("device")
            .join("local-device.json"),
        owner_credential_bytes,
    )
    .unwrap();

    let listing = e2v_cli::run_cli_for_test([
        "e2v",
        "share",
        "--repo",
        repo_root.to_str().unwrap(),
        "list",
    ])
    .unwrap();
    assert!(listing.contains("alice-phone"));

    let device_id = listing
        .lines()
        .filter(|line| line.contains(" active "))
        .find_map(|line| {
            if line.contains("alice-phone") {
                line.split_whitespace()
                    .next()
                    .map(|value| value.to_string())
            } else {
                None
            }
        })
        .unwrap();
    let revoke_output = e2v_cli::run_cli_for_test([
        "e2v",
        "share",
        "--repo",
        repo_root.to_str().unwrap(),
        "revoke-device",
        "--device",
        &device_id,
        "--password",
        "correct horse battery staple",
    ])
    .unwrap();
    assert!(revoke_output.contains("revoked"));
}

#[test]
fn share_revoke_member_still_works_after_cache_clear_when_password_is_explicit() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);
    let owner_credential_bytes = fs::read(
        repo_root
            .join(".e2v")
            .join("device")
            .join("local-device.json"),
    )
    .unwrap();
    let bundle_path = temp.path().join("invite.e2vshare");

    e2v_cli::run_cli_for_test([
        "e2v",
        "share",
        "--repo",
        repo_root.to_str().unwrap(),
        "invite-member",
        "--name",
        "Alice",
        "--out",
        bundle_path.to_str().unwrap(),
    ])
    .unwrap();
    e2v_cli::run_cli_for_test([
        "e2v",
        "share",
        "--repo",
        repo_root.to_str().unwrap(),
        "accept-member",
        "--bundle",
        bundle_path.to_str().unwrap(),
        "--label",
        "alice-laptop",
    ])
    .unwrap();
    fs::write(
        repo_root
            .join(".e2v")
            .join("device")
            .join("local-device.json"),
        owner_credential_bytes,
    )
    .unwrap();

    let listing = e2v_cli::run_cli_for_test([
        "e2v",
        "share",
        "--repo",
        repo_root.to_str().unwrap(),
        "list",
    ])
    .unwrap();
    let actor_id = listing
        .lines()
        .find_map(|line| {
            if line.contains("writer_member") {
                line.split_whitespace()
                    .next()
                    .map(|value| value.to_string())
            } else {
                None
            }
        })
        .unwrap();

    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&repo_root.join(".e2v"));
    let output = e2v_cli::run_cli_for_test([
        "e2v",
        "share",
        "--repo",
        repo_root.to_str().unwrap(),
        "revoke-member",
        "--actor",
        &actor_id,
        "--password",
        "correct horse battery staple",
    ])
    .unwrap();

    assert!(output.contains("revoked"));
}

#[test]
fn verify_remote_command_uses_default_file_remote() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(&repo_root).unwrap();
    fs::create_dir_all(&remote_root).unwrap();
    init_repo(&repo_root);
    fs::write(repo_root.join("tracked.txt"), "hello remote").unwrap();
    let facade = RepositoryFacade::new();
    let state = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap();
    let branch_state = facade.open(&repo_root).unwrap().branch;
    let remote = LocalFolderBackend::new(&remote_root);
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: branch_state.token_hex.clone(),
            operation_id: "cli-verify-remote".to_string(),
        },
    )
    .unwrap();

    e2v_cli::run_cli_for_test([
        "e2v",
        "remote",
        "--repo",
        repo_root.to_str().unwrap(),
        "add",
        "origin",
        &file_remote_spec(&remote_root),
    ])
    .unwrap();

    let output = e2v_cli::run_cli_for_test([
        "e2v",
        "verify",
        "--repo",
        repo_root.to_str().unwrap(),
        "remote",
        "--sample",
        "100%",
    ])
    .unwrap();

    assert!(output.contains("sampled"));
    assert!(output.contains(&state.snapshot_id[..8]));
}

#[test]
fn verify_snapshot_command_verifies_an_explicit_snapshot_id() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);
    fs::write(repo_root.join("tracked.txt"), "alpha").unwrap();
    let commit = RepositoryFacade::new()
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap();

    let output = e2v_cli::run_cli_for_test([
        "e2v",
        "verify",
        "--repo",
        repo_root.to_str().unwrap(),
        "snapshot",
        &commit.snapshot_id,
    ])
    .unwrap();

    assert!(output.contains("verified snapshot"));
    assert!(output.contains(&commit.snapshot_id[..8]));
}

#[test]
fn verify_object_command_verifies_an_explicit_object_id_and_type() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);
    fs::write(repo_root.join("tracked.txt"), "alpha").unwrap();
    let commit = RepositoryFacade::new()
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap();

    let output = e2v_cli::run_cli_for_test([
        "e2v",
        "verify",
        "--repo",
        repo_root.to_str().unwrap(),
        "object",
        "snapshot",
        &commit.snapshot_id,
    ])
    .unwrap();

    assert!(output.contains("verified object"));
    assert!(output.contains("snapshot"));
    assert!(output.contains(&commit.snapshot_id[..8]));
}

#[test]
fn maintenance_commands_share_the_same_default_remote_workflow_contract() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(&repo_root).unwrap();
    fs::create_dir_all(&remote_root).unwrap();
    init_repo(&repo_root);
    fs::write(repo_root.join("tracked.txt"), "alpha").unwrap();
    let facade = RepositoryFacade::new();
    let branch_state = facade.open(&repo_root).unwrap().branch;
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap();
    let remote = LocalFolderBackend::new(&remote_root);
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: branch_state.token_hex.clone(),
            operation_id: "cli-maintenance-contract".to_string(),
        },
    )
    .unwrap();

    let stray = "objects/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee.json";
    remote.put_physical(stray, br#"{"garbage":true}"#).unwrap();
    remote
        .override_physical_modified_time_for_test(
            stray,
            std::time::SystemTime::now() - std::time::Duration::from_secs(31 * 24 * 60 * 60),
        )
        .unwrap();

    e2v_cli::run_cli_for_test([
        "e2v",
        "remote",
        "--repo",
        repo_root.to_str().unwrap(),
        "add",
        "origin",
        &file_remote_spec(&remote_root),
    ])
    .unwrap();

    let verify_output = e2v_cli::run_cli_for_test([
        "e2v",
        "verify",
        "--repo",
        repo_root.to_str().unwrap(),
        "remote",
        "--sample",
        "100%",
    ])
    .unwrap();
    assert!(verify_output.contains("sampled"));

    let repair_output =
        e2v_cli::run_cli_for_test(["e2v", "repair", "--repo", repo_root.to_str().unwrap()])
            .unwrap();
    assert!(repair_output.contains("repaired 0"));

    let gc_error = e2v_cli::run_cli_for_test([
        "e2v",
        "gc",
        "--repo",
        repo_root.to_str().unwrap(),
        "--execute",
        "--grace-period",
        "30",
    ])
    .unwrap_err();
    assert!(
        gc_error.to_string().contains("maintenance window"),
        "unexpected error: {gc_error:#}"
    );

    let gc_output = e2v_cli::run_cli_for_test([
        "e2v",
        "gc",
        "--repo",
        repo_root.to_str().unwrap(),
        "--execute",
        "--confirm-single-writer-maintenance-window",
        "--grace-period",
        "30",
    ])
    .unwrap();
    assert!(gc_output.contains("deleted 1 physical refs"));
}

#[test]
fn maintenance_commands_delegate_through_the_sdk_boundary() {
    let source = cli_lib_source_without_whitespace();

    for legacy_call in [
        "verify_remote(",
        "repair_remote(",
        "gc_dry_run(",
        "gc_execute(",
        "force_accept_remote_rollback_sync(",
    ] {
        assert!(
            !source.contains(legacy_call),
            "expected CLI maintenance commands to delegate through e2v_api::Sdk instead of {legacy_call}"
        );
    }

    for sdk_call in [
        "sdk.verify_default_remote(",
        "sdk.repair_default_remote(",
        "sdk.force_accept_default_remote_rollback(",
        "sdk.gc_default_remote_dry_run(",
        "sdk.gc_default_remote_execute(",
    ] {
        assert!(
            source.contains(sdk_call),
            "expected CLI maintenance commands to use SDK call {sdk_call}"
        );
    }
}

#[test]
fn branch_and_share_commands_delegate_through_the_sdk_boundary() {
    let source = cli_lib_source_without_whitespace();

    for legacy_call in [
        "facade.list_branches(",
        "facade.create_branch(",
        "facade.checkout_branch(",
        "facade.delete_branch(",
        "facade.share_list(",
        "facade.share_invite_member(",
        "facade.share_accept_member(",
        "facade.share_invite_device(",
        "facade.share_accept_device(",
        "facade.share_revoke_member(",
        "facade.share_revoke_device(",
    ] {
        assert!(
            !source.contains(legacy_call),
            "expected CLI branch/share commands to delegate through e2v_api::Sdk instead of {legacy_call}"
        );
    }

    for sdk_call in [
        "sdk.list_branches(",
        "sdk.create_branch(",
        "sdk.checkout_branch(",
        "sdk.delete_branch(",
        "sdk.share_list(",
        "sdk.share_invite_member(",
        "sdk.share_accept_member(",
        "sdk.share_invite_device(",
        "sdk.share_accept_device(",
        "sdk.share_revoke_member(",
        "sdk.share_revoke_device(",
    ] {
        assert!(
            source.contains(sdk_call),
            "expected CLI branch/share commands to use SDK call {sdk_call}"
        );
    }
}

#[test]
fn repair_command_uses_default_file_remote() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(&repo_root).unwrap();
    fs::create_dir_all(&remote_root).unwrap();
    init_repo(&repo_root);
    fs::write(repo_root.join("tracked.txt"), "hello remote").unwrap();
    let facade = RepositoryFacade::new();
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap();
    let branch_state = facade.open(&repo_root).unwrap().branch;
    let remote = LocalFolderBackend::new(&remote_root);
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: branch_state.token_hex.clone(),
            operation_id: "cli-repair-remote".to_string(),
        },
    )
    .unwrap();

    let object_path = fs::read_dir(repo_root.join(".e2v").join("objects"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::remove_file(&object_path).unwrap();

    e2v_cli::run_cli_for_test([
        "e2v",
        "remote",
        "--repo",
        repo_root.to_str().unwrap(),
        "add",
        "origin",
        &file_remote_spec(&remote_root),
    ])
    .unwrap();

    let output =
        e2v_cli::run_cli_for_test(["e2v", "repair", "--repo", repo_root.to_str().unwrap()])
            .unwrap();

    assert!(output.contains("repaired 1"));
    assert!(object_path.exists());
}

#[test]
fn repair_force_accept_remote_rollback_uses_explicit_dangerous_flag() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(&repo_root).unwrap();
    fs::create_dir_all(&remote_root).unwrap();
    init_repo(&repo_root);
    fs::write(repo_root.join("tracked.txt"), "remote base").unwrap();
    let facade = RepositoryFacade::new();
    let remote_snapshot = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "remote-base".to_string(),
        })
        .unwrap();
    let branch_state = facade.open(&repo_root).unwrap().branch;
    let remote = LocalFolderBackend::new(&remote_root);
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: branch_state.token_hex.clone(),
            operation_id: "cli-rollback-remote-base".to_string(),
        },
    )
    .unwrap();

    fs::write(repo_root.join("tracked.txt"), "local ahead").unwrap();
    let local_snapshot = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "local-ahead".to_string(),
        })
        .unwrap();
    assert_ne!(remote_snapshot.snapshot_id, local_snapshot.snapshot_id);

    e2v_cli::run_cli_for_test([
        "e2v",
        "remote",
        "--repo",
        repo_root.to_str().unwrap(),
        "add",
        "origin",
        &file_remote_spec(&remote_root),
    ])
    .unwrap();

    let output = e2v_cli::run_cli_for_test([
        "e2v",
        "repair",
        "--repo",
        repo_root.to_str().unwrap(),
        "--force-accept-remote-rollback",
        "--confirm-remote-rollback",
        "--password",
        "correct horse battery staple",
    ])
    .unwrap();

    assert!(output.contains("accepted remote rollback"));
    let snapshots = facade.snapshots(&repo_root).unwrap();
    assert_eq!(
        snapshots.first().unwrap().snapshot_id,
        remote_snapshot.snapshot_id
    );
    assert_ne!(
        snapshots.first().unwrap().snapshot_id,
        local_snapshot.snapshot_id
    );
}

#[test]
fn repair_force_accept_remote_rollback_requires_second_confirmation_flag() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(&repo_root).unwrap();
    fs::create_dir_all(&remote_root).unwrap();
    init_repo(&repo_root);
    fs::write(repo_root.join("tracked.txt"), "remote base").unwrap();
    let facade = RepositoryFacade::new();
    let branch_state = facade.open(&repo_root).unwrap().branch;
    let remote = LocalFolderBackend::new(&remote_root);
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "remote-base".to_string(),
        })
        .unwrap();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: branch_state.token_hex.clone(),
            operation_id: "cli-rollback-second-confirm-remote-base".to_string(),
        },
    )
    .unwrap();

    fs::write(repo_root.join("tracked.txt"), "local ahead").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "local-ahead".to_string(),
        })
        .unwrap();

    e2v_cli::run_cli_for_test([
        "e2v",
        "remote",
        "--repo",
        repo_root.to_str().unwrap(),
        "add",
        "origin",
        &file_remote_spec(&remote_root),
    ])
    .unwrap();

    let error = e2v_cli::run_cli_for_test([
        "e2v",
        "repair",
        "--repo",
        repo_root.to_str().unwrap(),
        "--force-accept-remote-rollback",
        "--password",
        "correct horse battery staple",
    ])
    .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("--confirm-remote-rollback is required"),
        "expected explicit second confirmation error, got: {error:#}"
    );
}

#[test]
fn gc_dry_run_command_uses_default_file_remote() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(&repo_root).unwrap();
    fs::create_dir_all(&remote_root).unwrap();
    init_repo(&repo_root);
    fs::write(repo_root.join("tracked.txt"), "hello remote").unwrap();
    let facade = RepositoryFacade::new();
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap();
    let branch_state = facade.open(&repo_root).unwrap().branch;
    let remote = LocalFolderBackend::new(&remote_root);
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: branch_state.token_hex.clone(),
            operation_id: "cli-gc-dry-run".to_string(),
        },
    )
    .unwrap();
    remote
        .put_physical(
            "objects/ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff.json",
            br#"{"garbage":true}"#,
        )
        .unwrap();

    e2v_cli::run_cli_for_test([
        "e2v",
        "remote",
        "--repo",
        repo_root.to_str().unwrap(),
        "add",
        "origin",
        &file_remote_spec(&remote_root),
    ])
    .unwrap();

    let output = e2v_cli::run_cli_for_test([
        "e2v",
        "gc",
        "--repo",
        repo_root.to_str().unwrap(),
        "--dry-run",
    ])
    .unwrap();

    assert!(output.contains("1 unreachable physical refs"));
}

#[test]
fn gc_execute_command_uses_default_file_remote() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(&repo_root).unwrap();
    fs::create_dir_all(&remote_root).unwrap();
    init_repo(&repo_root);
    fs::write(repo_root.join("tracked.txt"), "hello remote").unwrap();
    let facade = RepositoryFacade::new();
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap();
    let branch_state = facade.open(&repo_root).unwrap().branch;
    let remote = LocalFolderBackend::new(&remote_root);
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: branch_state.token_hex.clone(),
            operation_id: "cli-gc-execute".to_string(),
        },
    )
    .unwrap();
    let stray = "objects/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee.json";
    remote.put_physical(stray, br#"{"garbage":true}"#).unwrap();
    remote
        .override_physical_modified_time_for_test(
            stray,
            std::time::SystemTime::now() - std::time::Duration::from_secs(31 * 24 * 60 * 60),
        )
        .unwrap();

    e2v_cli::run_cli_for_test([
        "e2v",
        "remote",
        "--repo",
        repo_root.to_str().unwrap(),
        "add",
        "origin",
        &file_remote_spec(&remote_root),
    ])
    .unwrap();

    let output = e2v_cli::run_cli_for_test([
        "e2v",
        "gc",
        "--repo",
        repo_root.to_str().unwrap(),
        "--execute",
        "--confirm-single-writer-maintenance-window",
        "--grace-period",
        "30d",
    ])
    .unwrap();

    assert!(output.contains("deleted 1 physical refs"));
    assert!(!remote.exists_physical(stray));
}

#[test]
fn gc_execute_command_requires_confirmation_for_single_writer_file_remote() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(&repo_root).unwrap();
    fs::create_dir_all(&remote_root).unwrap();
    init_repo(&repo_root);
    fs::write(repo_root.join("tracked.txt"), "hello remote").unwrap();
    let facade = RepositoryFacade::new();
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap();
    let branch_state = facade.open(&repo_root).unwrap().branch;
    let remote = LocalFolderBackend::new(&remote_root);
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: branch_state.token_hex.clone(),
            operation_id: "cli-gc-execute-window".to_string(),
        },
    )
    .unwrap();

    e2v_cli::run_cli_for_test([
        "e2v",
        "remote",
        "--repo",
        repo_root.to_str().unwrap(),
        "add",
        "origin",
        &file_remote_spec(&remote_root),
    ])
    .unwrap();

    let error = e2v_cli::run_cli_for_test([
        "e2v",
        "gc",
        "--repo",
        repo_root.to_str().unwrap(),
        "--execute",
        "--grace-period",
        "30d",
    ])
    .unwrap_err();

    assert!(
        error.to_string().contains("maintenance window"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn doctor_command_reports_trusted_state_and_remote_gc_capabilities() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let remote_root = temp.path().join("remote");
    let trusted_state_root = temp.path().join("trusted-state");
    fs::create_dir_all(&repo_root).unwrap();
    fs::create_dir_all(&remote_root).unwrap();
    fs::create_dir_all(&trusted_state_root).unwrap();
    init_repo(&repo_root);
    let _trusted_state_guard =
        e2v_sync::testing::override_trusted_state_dir_for_test(trusted_state_root.clone());

    fs::write(repo_root.join("tracked.txt"), "hello remote").unwrap();
    let facade = RepositoryFacade::new();
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap();
    let branch_state = facade.open(&repo_root).unwrap().branch;
    let remote = LocalFolderBackend::new(&remote_root);
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: branch_state.token_hex.clone(),
            operation_id: "cli-doctor".to_string(),
        },
    )
    .unwrap();

    e2v_cli::run_cli_for_test([
        "e2v",
        "remote",
        "--repo",
        repo_root.to_str().unwrap(),
        "add",
        "origin",
        &file_remote_spec(&remote_root),
    ])
    .unwrap();

    let output =
        e2v_cli::run_cli_for_test(["e2v", "doctor", "--repo", repo_root.to_str().unwrap()])
            .unwrap();

    assert!(output.contains("trusted_state"));
    assert!(output.contains("remote_spec"));
    assert!(output.contains("gc_execute_supported"));
}

#[test]
fn doctor_bundle_writes_bundle_directory_with_summary_files() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let remote_root = temp.path().join("remote");
    let trusted_state_root = temp.path().join("trusted-state");
    let bundle_root = temp.path().join("bundle-out");
    fs::create_dir_all(&repo_root).unwrap();
    fs::create_dir_all(&remote_root).unwrap();
    fs::create_dir_all(&trusted_state_root).unwrap();
    init_repo(&repo_root);
    let _trusted_state_guard =
        e2v_sync::testing::override_trusted_state_dir_for_test(trusted_state_root.clone());

    fs::write(repo_root.join("tracked.txt"), "hello remote").unwrap();
    let facade = RepositoryFacade::new();
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap();
    let branch_state = facade.open(&repo_root).unwrap().branch;
    let remote = LocalFolderBackend::new(&remote_root);
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: branch_state.token_hex.clone(),
            operation_id: "cli-doctor-bundle".to_string(),
        },
    )
    .unwrap();

    e2v_cli::run_cli_for_test([
        "e2v",
        "remote",
        "--repo",
        repo_root.to_str().unwrap(),
        "add",
        "origin",
        &file_remote_spec(&remote_root),
    ])
    .unwrap();

    let output = e2v_cli::run_cli_for_test([
        "e2v",
        "doctor",
        "--repo",
        repo_root.to_str().unwrap(),
        "--bundle",
        bundle_root.to_str().unwrap(),
    ])
    .unwrap();

    assert!(output.contains("bundle"));
    assert!(bundle_root.join("doctor-summary.json").is_file());
    assert!(bundle_root.join("trusted-state.json").is_file());
}

#[test]
fn doctor_bundle_redacts_plaintext_repo_and_remote_paths() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let remote_root = temp.path().join("remote");
    let trusted_state_root = temp.path().join("trusted-state");
    let bundle_root = temp.path().join("bundle-out");
    fs::create_dir_all(&repo_root).unwrap();
    fs::create_dir_all(&remote_root).unwrap();
    fs::create_dir_all(&trusted_state_root).unwrap();
    init_repo(&repo_root);
    let _trusted_state_guard =
        e2v_sync::testing::override_trusted_state_dir_for_test(trusted_state_root.clone());

    fs::write(repo_root.join("tracked.txt"), "hello remote").unwrap();
    let facade = RepositoryFacade::new();
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap();
    let branch_state = facade.open(&repo_root).unwrap().branch;
    let remote = LocalFolderBackend::new(&remote_root);
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: branch_state.token_hex.clone(),
            operation_id: "cli-doctor-bundle-redaction".to_string(),
        },
    )
    .unwrap();

    e2v_cli::run_cli_for_test([
        "e2v",
        "remote",
        "--repo",
        repo_root.to_str().unwrap(),
        "add",
        "origin",
        &file_remote_spec(&remote_root),
    ])
    .unwrap();

    e2v_cli::run_cli_for_test([
        "e2v",
        "doctor",
        "--repo",
        repo_root.to_str().unwrap(),
        "--bundle",
        bundle_root.to_str().unwrap(),
    ])
    .unwrap();

    let summary = fs::read_to_string(bundle_root.join("doctor-summary.json")).unwrap();
    assert!(
        !summary.contains(&repo_root.to_string_lossy().to_string()),
        "bundle summary leaked repo path: {summary}"
    );
    assert!(
        !summary.contains(&remote_root.to_string_lossy().to_string()),
        "bundle summary leaked remote path: {summary}"
    );
    assert!(
        !summary.contains("file:///"),
        "bundle summary leaked file remote url: {summary}"
    );
}

#[test]
fn doctor_bundle_redacts_s3_remote_credentials() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let bundle_root = temp.path().join("bundle-out");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    e2v_cli::run_cli_for_test([
        "e2v",
        "remote",
        "--repo",
        repo_root.to_str().unwrap(),
        "add",
        "origin",
        "s3+https://alice:secret@s3.example.com/example-bucket/sync-root?region=us-east-1",
    ])
    .unwrap();

    e2v_cli::run_cli_for_test([
        "e2v",
        "doctor",
        "--repo",
        repo_root.to_str().unwrap(),
        "--bundle",
        bundle_root.to_str().unwrap(),
    ])
    .unwrap();

    let summary = fs::read_to_string(bundle_root.join("doctor-summary.json")).unwrap();
    assert!(
        !summary.contains("alice"),
        "bundle summary leaked s3 access key id: {summary}"
    );
    assert!(
        !summary.contains("secret"),
        "bundle summary leaked s3 secret key: {summary}"
    );
    assert!(
        !summary.contains("example-bucket"),
        "bundle summary leaked s3 bucket name: {summary}"
    );
}
