use std::fs;
use std::path::Path;

use e2v_api::{
    CheckoutSnapshotOptions, CloneRequest, CommitRepositoryOptions, EnableObliviousLayoutRequest,
    FetchRequest, HistoricalRewriteExecuteRequest, HistoricalRewritePlanRequest,
    InitRepositoryOptions, ObliviousLayoutPlanRequest, PullRequest, PushRequest,
    ReshuffleObliviousLayoutRequest, Sdk, SdkErrorCode, parse_remote_spec,
};
use e2v_core::RepositoryFacade;

fn file_remote_spec(path: &Path) -> String {
    format!("file://{}", path.to_string_lossy().replace('\\', "/"))
}

#[test]
fn sdk_can_init_commit_and_read_repository_without_internal_crates() {
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let sdk = Sdk::new();
    sdk.init_repository(InitRepositoryOptions {
        repo_root: repo_root.clone(),
        password: "correct horse battery staple".to_string(),
        branch_name: "main".to_string(),
    })
    .unwrap();

    fs::write(repo_root.join("notes.txt"), "hello sdk").unwrap();
    let commit = sdk
        .commit_repository(CommitRepositoryOptions {
            repo_root: repo_root.clone(),
            message: "first".to_string(),
        })
        .unwrap();

    let read = sdk.open_read_handle(&repo_root).unwrap();
    let snapshot = read.open_snapshot(&commit.snapshot_id).unwrap();
    let file = read.open_file(&snapshot, "notes.txt").unwrap();
    let bytes = read.read_range(&file, 0, 32).unwrap();

    assert_eq!(String::from_utf8(bytes).unwrap(), "hello sdk");
}

#[test]
fn sdk_open_missing_repository_returns_not_found() {
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("missing-repo");

    let error = Sdk::new().open_repository(&repo_root).unwrap_err();

    assert_eq!(error.code(), SdkErrorCode::NotFound);
}

#[test]
fn sdk_open_missing_snapshot_returns_not_found() {
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let sdk = Sdk::new();
    sdk.init_repository(InitRepositoryOptions {
        repo_root: repo_root.clone(),
        password: "correct horse battery staple".to_string(),
        branch_name: "main".to_string(),
    })
    .unwrap();

    let read = sdk.open_read_handle(&repo_root).unwrap();
    let error = read.open_snapshot("missing").unwrap_err();

    assert_eq!(error.code(), SdkErrorCode::NotFound);
}

#[test]
fn sdk_remote_parse_rejects_unsupported_scheme_with_invalid_argument() {
    let error = parse_remote_spec("ftp://example.com/repo").unwrap_err();
    assert_eq!(error.code(), SdkErrorCode::InvalidArgument);
}

#[test]
fn sdk_remote_parse_accepts_s3_remote_specs() {
    let parsed = parse_remote_spec(
        "s3+https://alice:secret@s3.example.com/example-bucket/sync-root?region=us-east-1",
    )
    .unwrap();

    assert_eq!(
        parsed.as_str(),
        "s3+https://alice:secret@s3.example.com/example-bucket/sync-root?region=us-east-1"
    );
}

#[test]
fn sdk_can_register_default_remote_and_push_fetch_through_it() {
    let temp = tempfile::tempdir().unwrap();
    let source_repo = temp.path().join("source");
    let clone_repo = temp.path().join("clone");
    let remote_repo = temp.path().join("remote");
    fs::create_dir_all(&source_repo).unwrap();
    fs::create_dir_all(&remote_repo).unwrap();

    let sdk = Sdk::new();
    let state = sdk
        .init_repository(InitRepositoryOptions {
            repo_root: source_repo.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
    fs::write(source_repo.join("notes.txt"), "hello sync").unwrap();
    sdk.commit_repository(CommitRepositoryOptions {
        repo_root: source_repo.clone(),
        message: "first".to_string(),
    })
    .unwrap();

    let remote_spec = format!(
        "file://{}",
        remote_repo.to_string_lossy().replace('\\', "/")
    );
    sdk.add_remote(&source_repo, "origin", &remote_spec)
        .unwrap();
    sdk.push_default_remote(PushRequest {
        repo_root: source_repo.clone(),
        branch_token: state.branch.token_hex.clone(),
        operation_id: "push-1".to_string(),
    })
    .unwrap();

    let cloned = sdk
        .clone_remote(CloneRequest {
            remote_spec,
            target_repo_root: clone_repo.clone(),
            password: "correct horse battery staple".to_string(),
            branch_token: state.branch.token_hex.clone(),
        })
        .unwrap();

    let read = sdk.open_read_handle(&clone_repo).unwrap();
    let snapshot = read.resolve_branch(&cloned.branch_token).unwrap();
    let file = read.open_file(&snapshot, "notes.txt").unwrap();
    let bytes = read.read_range(&file, 0, 32).unwrap();
    assert_eq!(String::from_utf8(bytes).unwrap(), "hello sync");
}

#[test]
fn sdk_branch_reads_are_snapshot_pinned() {
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let sdk = Sdk::new();
    let state = sdk
        .init_repository(InitRepositoryOptions {
            repo_root: repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();

    fs::write(repo_root.join("tracked.txt"), "alpha").unwrap();
    sdk.commit_repository(CommitRepositoryOptions {
        repo_root: repo_root.clone(),
        message: "first".to_string(),
    })
    .unwrap();

    let read = sdk.open_read_handle(&repo_root).unwrap();
    let first_snapshot = read.resolve_branch(&state.branch.token_hex).unwrap();
    let first_file = read.open_file(&first_snapshot, "tracked.txt").unwrap();

    fs::write(repo_root.join("tracked.txt"), "beta").unwrap();
    sdk.commit_repository(CommitRepositoryOptions {
        repo_root: repo_root.clone(),
        message: "second".to_string(),
    })
    .unwrap();

    let old_bytes = read.read_range(&first_file, 0, 32).unwrap();
    assert_eq!(String::from_utf8(old_bytes).unwrap(), "alpha");

    let second_snapshot = read.resolve_branch(&state.branch.token_hex).unwrap();
    let second_file = read.open_file(&second_snapshot, "tracked.txt").unwrap();
    let new_bytes = read.read_range(&second_file, 0, 32).unwrap();
    assert_eq!(String::from_utf8(new_bytes).unwrap(), "beta");
}

#[test]
fn sdk_can_list_snapshots_and_checkout_older_snapshot() {
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let checkout_dir = temp.path().join("checkout");
    fs::create_dir_all(&repo_root).unwrap();
    fs::create_dir_all(&checkout_dir).unwrap();

    let sdk = Sdk::new();
    sdk.init_repository(InitRepositoryOptions {
        repo_root: repo_root.clone(),
        password: "correct horse battery staple".to_string(),
        branch_name: "main".to_string(),
    })
    .unwrap();

    fs::write(repo_root.join("tracked.txt"), "alpha").unwrap();
    let first = sdk
        .commit_repository(CommitRepositoryOptions {
            repo_root: repo_root.clone(),
            message: "first".to_string(),
        })
        .unwrap();

    fs::write(repo_root.join("tracked.txt"), "beta").unwrap();
    let second = sdk
        .commit_repository(CommitRepositoryOptions {
            repo_root: repo_root.clone(),
            message: "second".to_string(),
        })
        .unwrap();

    let snapshots = sdk.list_snapshots(&repo_root).unwrap();
    assert_eq!(snapshots.len(), 2);
    assert_eq!(snapshots[0].snapshot_id, second.snapshot_id);
    assert_eq!(snapshots[1].snapshot_id, first.snapshot_id);

    sdk.checkout_snapshot(CheckoutSnapshotOptions {
        repo_root: repo_root.clone(),
        snapshot_id: first.snapshot_id,
        target_dir: checkout_dir.clone(),
    })
    .unwrap();

    let checked_out = fs::read_to_string(checkout_dir.join("tracked.txt")).unwrap();
    assert_eq!(checked_out, "alpha");
}

#[test]
fn sdk_can_read_directory_entries_through_public_read_api() {
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let sdk = Sdk::new();
    sdk.init_repository(InitRepositoryOptions {
        repo_root: repo_root.clone(),
        password: "correct horse battery staple".to_string(),
        branch_name: "main".to_string(),
    })
    .unwrap();

    fs::create_dir_all(repo_root.join("nested")).unwrap();
    fs::write(repo_root.join("nested").join("base.txt"), "alpha").unwrap();
    fs::write(repo_root.join("root.txt"), "root").unwrap();
    let commit = sdk
        .commit_repository(CommitRepositoryOptions {
            repo_root: repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap();

    let read = sdk.open_read_handle(&repo_root).unwrap();
    let snapshot = read.open_snapshot(&commit.snapshot_id).unwrap();
    let root_entries = read.read_dir(&snapshot, "").unwrap();
    let root_names = root_entries
        .into_iter()
        .map(|entry| entry.name)
        .collect::<Vec<_>>();
    assert_eq!(
        root_names,
        vec!["nested".to_string(), "root.txt".to_string()]
    );

    let nested_entries = read.read_dir(&snapshot, "nested").unwrap();
    let nested_names = nested_entries
        .into_iter()
        .map(|entry| entry.name)
        .collect::<Vec<_>>();
    assert_eq!(nested_names, vec!["base.txt".to_string()]);
}

#[test]
fn sdk_reports_missing_local_chunk_as_corrupt_state_instead_of_not_found() {
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let sdk = Sdk::new();
    sdk.init_repository(InitRepositoryOptions {
        repo_root: repo_root.clone(),
        password: "correct horse battery staple".to_string(),
        branch_name: "main".to_string(),
    })
    .unwrap();

    fs::write(repo_root.join("hello.txt"), "hello sdk").unwrap();
    let commit = sdk
        .commit_repository(CommitRepositoryOptions {
            repo_root: repo_root.clone(),
            message: "first".to_string(),
        })
        .unwrap();

    let read = sdk.open_read_handle(&repo_root).unwrap();
    let snapshot = read.open_snapshot(&commit.snapshot_id).unwrap();
    let file = read.open_file(&snapshot, "hello.txt").unwrap();

    let core_read = RepositoryFacade::new().read_service(&repo_root).unwrap();
    let core_snapshot = core_read.open_snapshot(&commit.snapshot_id).unwrap();
    let core_file = core_read.open_file(&core_snapshot, "hello.txt").unwrap();
    let chunk_id = core_file.debug_chunk_ids()[0].clone();
    let chunk_path = repo_root
        .join(".e2v")
        .join("objects")
        .join(format!("{chunk_id}.json"));
    fs::remove_file(&chunk_path).unwrap();

    let error = read.read_range(&file, 0, 64).unwrap_err();
    assert_eq!(error.code(), SdkErrorCode::CorruptState);
}

#[test]
fn sdk_can_verify_snapshot_through_public_api() {
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let sdk = Sdk::new();
    sdk.init_repository(InitRepositoryOptions {
        repo_root: repo_root.clone(),
        password: "correct horse battery staple".to_string(),
        branch_name: "main".to_string(),
    })
    .unwrap();

    fs::write(repo_root.join("tracked.txt"), "alpha").unwrap();
    let commit = sdk
        .commit_repository(CommitRepositoryOptions {
            repo_root: repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap();

    sdk.verify_snapshot(&repo_root, &commit.snapshot_id)
        .unwrap();
}

#[test]
fn sdk_can_fetch_updates_from_registered_default_remote() {
    let temp = tempfile::tempdir().unwrap();
    let source_repo = temp.path().join("source");
    let clone_repo = temp.path().join("clone");
    let remote_repo = temp.path().join("remote");
    fs::create_dir_all(&source_repo).unwrap();
    fs::create_dir_all(&remote_repo).unwrap();

    let sdk = Sdk::new();
    let source_state = sdk
        .init_repository(InitRepositoryOptions {
            repo_root: source_repo.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
    fs::write(source_repo.join("tracked.txt"), "alpha").unwrap();
    sdk.commit_repository(CommitRepositoryOptions {
        repo_root: source_repo.clone(),
        message: "first".to_string(),
    })
    .unwrap();

    let remote_spec = format!(
        "file://{}",
        remote_repo.to_string_lossy().replace('\\', "/")
    );
    sdk.add_remote(&source_repo, "origin", &remote_spec)
        .unwrap();
    sdk.push_default_remote(PushRequest {
        repo_root: source_repo.clone(),
        branch_token: source_state.branch.token_hex.clone(),
        operation_id: "push-1".to_string(),
    })
    .unwrap();

    let cloned = sdk
        .clone_remote(CloneRequest {
            remote_spec: remote_spec.clone(),
            target_repo_root: clone_repo.clone(),
            password: "correct horse battery staple".to_string(),
            branch_token: source_state.branch.token_hex.clone(),
        })
        .unwrap();
    sdk.add_remote(&clone_repo, "origin", &remote_spec).unwrap();

    fs::write(source_repo.join("tracked.txt"), "beta").unwrap();
    sdk.commit_repository(CommitRepositoryOptions {
        repo_root: source_repo.clone(),
        message: "second".to_string(),
    })
    .unwrap();
    sdk.push_default_remote(PushRequest {
        repo_root: source_repo.clone(),
        branch_token: source_state.branch.token_hex.clone(),
        operation_id: "push-2".to_string(),
    })
    .unwrap();

    sdk.fetch_default_remote(FetchRequest {
        repo_root: clone_repo.clone(),
        branch_token: cloned.branch_token.clone(),
        password: Some("correct horse battery staple".to_string()),
    })
    .unwrap();

    let read = sdk.open_read_handle(&clone_repo).unwrap();
    let snapshot = read.resolve_branch(&cloned.branch_token).unwrap();
    let file = read.open_file(&snapshot, "tracked.txt").unwrap();
    let bytes = read.read_range(&file, 0, 32).unwrap();
    assert_eq!(String::from_utf8(bytes).unwrap(), "beta");
}

#[test]
fn sdk_can_pull_fast_forward_updates_into_the_current_branch() {
    let temp = tempfile::tempdir().unwrap();
    let source_repo = temp.path().join("source");
    let clone_repo = temp.path().join("clone");
    let remote_repo = temp.path().join("remote");
    fs::create_dir_all(&source_repo).unwrap();
    fs::create_dir_all(&remote_repo).unwrap();

    let sdk = Sdk::new();
    let source_state = sdk
        .init_repository(InitRepositoryOptions {
            repo_root: source_repo.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
    fs::write(source_repo.join("tracked.txt"), "alpha").unwrap();
    let first = sdk
        .commit_repository(CommitRepositoryOptions {
            repo_root: source_repo.clone(),
            message: "first".to_string(),
        })
        .unwrap();

    let remote_spec = format!(
        "file://{}",
        remote_repo.to_string_lossy().replace('\\', "/")
    );
    sdk.add_remote(&source_repo, "origin", &remote_spec)
        .unwrap();
    sdk.push_default_remote(PushRequest {
        repo_root: source_repo.clone(),
        branch_token: source_state.branch.token_hex.clone(),
        operation_id: "push-first".to_string(),
    })
    .unwrap();

    sdk.clone_remote(CloneRequest {
        remote_spec: remote_spec.clone(),
        target_repo_root: clone_repo.clone(),
        password: "correct horse battery staple".to_string(),
        branch_token: source_state.branch.token_hex.clone(),
    })
    .unwrap();
    sdk.add_remote(&clone_repo, "origin", &remote_spec).unwrap();

    fs::write(source_repo.join("tracked.txt"), "beta").unwrap();
    let second = sdk
        .commit_repository(CommitRepositoryOptions {
            repo_root: source_repo.clone(),
            message: "second".to_string(),
        })
        .unwrap();
    sdk.push_default_remote(PushRequest {
        repo_root: source_repo.clone(),
        branch_token: source_state.branch.token_hex.clone(),
        operation_id: "push-second".to_string(),
    })
    .unwrap();

    let pulled = sdk
        .pull_default_remote(PullRequest {
            repo_root: clone_repo.clone(),
            branch_token: source_state.branch.token_hex.clone(),
            password: Some("correct horse battery staple".to_string()),
        })
        .unwrap();

    assert_eq!(pulled.snapshot_id, second.snapshot_id);
    assert!(pulled.fast_forward_applied);

    let read = sdk.open_read_handle(&clone_repo).unwrap();
    let snapshot = read.resolve_branch(&source_state.branch.token_hex).unwrap();
    assert_eq!(snapshot.snapshot_id, second.snapshot_id);
    assert_ne!(snapshot.snapshot_id, first.snapshot_id);
}

#[test]
fn sdk_pull_rejects_diverged_local_history_without_moving_the_current_branch() {
    let temp = tempfile::tempdir().unwrap();
    let source_repo = temp.path().join("source");
    let clone_repo = temp.path().join("clone");
    let remote_repo = temp.path().join("remote");
    fs::create_dir_all(&source_repo).unwrap();
    fs::create_dir_all(&remote_repo).unwrap();

    let sdk = Sdk::new();
    let source_state = sdk
        .init_repository(InitRepositoryOptions {
            repo_root: source_repo.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
    fs::write(source_repo.join("tracked.txt"), "alpha").unwrap();
    sdk.commit_repository(CommitRepositoryOptions {
        repo_root: source_repo.clone(),
        message: "first".to_string(),
    })
    .unwrap();

    let remote_spec = format!(
        "file://{}",
        remote_repo.to_string_lossy().replace('\\', "/")
    );
    sdk.add_remote(&source_repo, "origin", &remote_spec)
        .unwrap();
    sdk.push_default_remote(PushRequest {
        repo_root: source_repo.clone(),
        branch_token: source_state.branch.token_hex.clone(),
        operation_id: "push-first".to_string(),
    })
    .unwrap();

    sdk.clone_remote(CloneRequest {
        remote_spec: remote_spec.clone(),
        target_repo_root: clone_repo.clone(),
        password: "correct horse battery staple".to_string(),
        branch_token: source_state.branch.token_hex.clone(),
    })
    .unwrap();
    sdk.add_remote(&clone_repo, "origin", &remote_spec).unwrap();

    fs::write(clone_repo.join("tracked.txt"), "local-only").unwrap();
    let local_only = sdk
        .commit_repository(CommitRepositoryOptions {
            repo_root: clone_repo.clone(),
            message: "local-only".to_string(),
        })
        .unwrap();

    fs::write(source_repo.join("tracked.txt"), "remote-only").unwrap();
    sdk.commit_repository(CommitRepositoryOptions {
        repo_root: source_repo.clone(),
        message: "remote-only".to_string(),
    })
    .unwrap();
    sdk.push_default_remote(PushRequest {
        repo_root: source_repo.clone(),
        branch_token: source_state.branch.token_hex.clone(),
        operation_id: "push-remote-only".to_string(),
    })
    .unwrap();
    let default_ref_before =
        fs::read(clone_repo.join(".e2v").join("refs").join("default.json")).unwrap();

    let error = sdk
        .pull_default_remote(PullRequest {
            repo_root: clone_repo.clone(),
            branch_token: source_state.branch.token_hex.clone(),
            password: Some("correct horse battery staple".to_string()),
        })
        .unwrap_err();

    assert_eq!(error.code(), SdkErrorCode::NeedsRebase);

    let read = sdk.open_read_handle(&clone_repo).unwrap();
    let snapshot = read.resolve_branch(&source_state.branch.token_hex).unwrap();
    assert_eq!(snapshot.snapshot_id, local_only.snapshot_id);
    assert_eq!(
        fs::read(clone_repo.join(".e2v").join("refs").join("default.json"),).unwrap(),
        default_ref_before
    );
}

#[test]
fn sdk_can_push_and_fetch_with_explicit_remote_spec_without_default_remote_registration() {
    let temp = tempfile::tempdir().unwrap();
    let source_repo = temp.path().join("source");
    let clone_repo = temp.path().join("clone");
    let remote_repo = temp.path().join("remote");
    fs::create_dir_all(&source_repo).unwrap();
    fs::create_dir_all(&remote_repo).unwrap();

    let sdk = Sdk::new();
    let source_state = sdk
        .init_repository(InitRepositoryOptions {
            repo_root: source_repo.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
    fs::write(source_repo.join("tracked.txt"), "alpha").unwrap();
    sdk.commit_repository(CommitRepositoryOptions {
        repo_root: source_repo.clone(),
        message: "first".to_string(),
    })
    .unwrap();

    let remote_spec = file_remote_spec(&remote_repo);
    sdk.push_remote(
        &remote_spec,
        PushRequest {
            repo_root: source_repo.clone(),
            branch_token: source_state.branch.token_hex.clone(),
            operation_id: "push-explicit-1".to_string(),
        },
    )
    .unwrap();

    let cloned = sdk
        .clone_remote(CloneRequest {
            remote_spec: remote_spec.clone(),
            target_repo_root: clone_repo.clone(),
            password: "correct horse battery staple".to_string(),
            branch_token: source_state.branch.token_hex.clone(),
        })
        .unwrap();

    assert!(
        !source_repo
            .join(".e2v")
            .join("remotes")
            .join("default.json")
            .exists(),
        "explicit remote push should not require default remote registration"
    );
    assert!(
        !clone_repo
            .join(".e2v")
            .join("remotes")
            .join("default.json")
            .exists(),
        "explicit remote clone/fetch flow should not require default remote registration"
    );

    fs::write(source_repo.join("tracked.txt"), "beta").unwrap();
    sdk.commit_repository(CommitRepositoryOptions {
        repo_root: source_repo.clone(),
        message: "second".to_string(),
    })
    .unwrap();
    sdk.push_remote(
        &remote_spec,
        PushRequest {
            repo_root: source_repo.clone(),
            branch_token: source_state.branch.token_hex.clone(),
            operation_id: "push-explicit-2".to_string(),
        },
    )
    .unwrap();

    sdk.fetch_remote(
        &remote_spec,
        FetchRequest {
            repo_root: clone_repo.clone(),
            branch_token: cloned.branch_token.clone(),
            password: Some("correct horse battery staple".to_string()),
        },
    )
    .unwrap();

    let read = sdk.open_read_handle(&clone_repo).unwrap();
    let snapshot = read.resolve_branch(&cloned.branch_token).unwrap();
    let file = read.open_file(&snapshot, "tracked.txt").unwrap();
    let bytes = read.read_range(&file, 0, 32).unwrap();
    assert_eq!(String::from_utf8(bytes).unwrap(), "beta");
}

#[test]
fn sdk_public_workflows_can_be_typed_without_internal_crates() {
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(&repo_root).unwrap();
    fs::create_dir_all(&remote_root).unwrap();

    let sdk: e2v_api::Sdk = Sdk::new();
    let repo: e2v_api::RepositoryInfo = sdk
        .init_repository(InitRepositoryOptions {
            repo_root: repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();

    fs::write(repo_root.join("typed.txt"), "typed").unwrap();
    let commit: e2v_api::CommitInfo = sdk
        .commit_repository(CommitRepositoryOptions {
            repo_root: repo_root.clone(),
            message: "typed".to_string(),
        })
        .unwrap();

    let read: e2v_api::ReadHandle = sdk.open_read_handle(&repo_root).unwrap();
    let snapshot: e2v_api::SnapshotView = read.open_snapshot(&commit.snapshot_id).unwrap();
    let file: e2v_api::FileView = read.open_file(&snapshot, "typed.txt").unwrap();
    let entries: Vec<e2v_api::DirectoryEntryInfo> = read.read_dir(&snapshot, "").unwrap();
    let remote_spec: e2v_api::ParsedRemoteSpec = parse_remote_spec(&format!(
        "file://{}",
        remote_root.to_string_lossy().replace('\\', "/")
    ))
    .unwrap();
    let remote: e2v_api::RemoteRegistration = sdk
        .add_remote(&repo_root, "origin", remote_spec.as_str())
        .unwrap();

    assert_eq!(repo.branch.token_hex, repo.branch.token_hex.clone());
    assert_eq!(snapshot.snapshot_id, commit.snapshot_id);
    assert!(snapshot.branch_token.is_none());
    assert_eq!(
        String::from_utf8(read.read_range(&file, 0, 16).unwrap()).unwrap(),
        "typed"
    );
    assert_eq!(entries.len(), 1);
    assert_eq!(remote.name, "origin");
}

#[test]
fn sdk_snapshot_and_file_views_do_not_serialize_internal_handles() {
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let sdk = Sdk::new();
    sdk.init_repository(InitRepositoryOptions {
        repo_root: repo_root.clone(),
        password: "correct horse battery staple".to_string(),
        branch_name: "main".to_string(),
    })
    .unwrap();

    fs::write(repo_root.join("typed.txt"), "typed").unwrap();
    let commit = sdk
        .commit_repository(CommitRepositoryOptions {
            repo_root: repo_root.clone(),
            message: "typed".to_string(),
        })
        .unwrap();

    let read = sdk.open_read_handle(&repo_root).unwrap();
    let snapshot = read.open_snapshot(&commit.snapshot_id).unwrap();
    let file = read.open_file(&snapshot, "typed.txt").unwrap();

    let snapshot_json = serde_json::to_value(&snapshot).unwrap();
    let file_json = serde_json::to_value(&file).unwrap();

    let snapshot_object = snapshot_json.as_object().unwrap();
    let file_object = file_json.as_object().unwrap();

    assert!(!snapshot_object.contains_key("inner"));
    assert!(!snapshot_json.to_string().contains("root_tree_id"));

    assert!(!file_object.contains_key("inner"));
    assert!(!file_json.to_string().contains("chunk_ids"));
    assert!(!file_json.to_string().contains("chunk_lengths"));
    assert!(!file_json.to_string().contains("shard_ids"));
    assert!(!file_json.to_string().contains("shard_byte_lengths"));
}

#[test]
fn sdk_can_change_password_and_unlock_with_new_password() {
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let sdk = Sdk::new();
    sdk.init_repository(InitRepositoryOptions {
        repo_root: repo_root.clone(),
        password: "old-password".to_string(),
        branch_name: "main".to_string(),
    })
    .unwrap();

    sdk.change_password(&repo_root, "old-password", "new-password")
        .unwrap();
    let reopened = sdk.unlock_repository(&repo_root, "new-password").unwrap();
    assert_eq!(reopened.branch.name, "main");
}

#[test]
fn sdk_can_create_list_checkout_and_delete_branch() {
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let sdk = Sdk::new();
    sdk.init_repository(InitRepositoryOptions {
        repo_root: repo_root.clone(),
        password: "correct horse battery staple".to_string(),
        branch_name: "main".to_string(),
    })
    .unwrap();

    let created = sdk.create_branch(&repo_root, "feature").unwrap();
    assert_eq!(created.name, "feature");

    let branches = sdk.list_branches(&repo_root).unwrap();
    assert!(branches.iter().any(|branch| branch.name == "feature"));

    let checked_out = sdk.checkout_branch(&repo_root, "feature").unwrap();
    assert_eq!(checked_out.branch.name, "feature");

    sdk.checkout_branch(&repo_root, "main").unwrap();
    sdk.delete_branch(&repo_root, "feature").unwrap();

    let remaining = sdk.list_branches(&repo_root).unwrap();
    assert!(!remaining.iter().any(|branch| branch.name == "feature"));
}

#[test]
fn sdk_can_list_and_manage_share_workflows() {
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let sdk = Sdk::new();
    sdk.init_repository(InitRepositoryOptions {
        repo_root: repo_root.clone(),
        password: "correct horse battery staple".to_string(),
        branch_name: "main".to_string(),
    })
    .unwrap();
    let owner_credential = fs::read(
        repo_root
            .join(".e2v")
            .join("device")
            .join("local-device.json"),
    )
    .unwrap();

    let listed = sdk.share_list(&repo_root).unwrap();
    assert_eq!(listed.actors.len(), 1);
    assert_eq!(listed.devices.len(), 1);

    let member_invite = sdk
        .share_invite_member(
            &repo_root,
            e2v_api::ShareInviteMemberRequest {
                display_name: "Alice".to_string(),
            },
        )
        .unwrap();
    let accepted_member = sdk
        .share_accept_member(
            &repo_root,
            e2v_api::ShareAcceptMemberRequest {
                invite_bytes: member_invite.bundle_bytes.clone(),
                local_device_label: "alice-laptop".to_string(),
            },
        )
        .unwrap();
    assert_eq!(accepted_member.role, "writer_member");

    fs::write(
        repo_root
            .join(".e2v")
            .join("device")
            .join("local-device.json"),
        &owner_credential,
    )
    .unwrap();

    let device_invite = sdk
        .share_invite_device(
            &repo_root,
            e2v_api::ShareInviteDeviceRequest {
                actor_id: accepted_member.actor_id.clone(),
                device_label: "alice-phone".to_string(),
            },
        )
        .unwrap();
    let accepted_device = sdk
        .share_accept_device(
            &repo_root,
            e2v_api::ShareAcceptDeviceRequest {
                invite_bytes: device_invite.bundle_bytes.clone(),
                local_device_label: "alice-phone".to_string(),
            },
        )
        .unwrap();
    assert_eq!(accepted_device.actor_id, accepted_member.actor_id);

    fs::write(
        repo_root
            .join(".e2v")
            .join("device")
            .join("local-device.json"),
        &owner_credential,
    )
    .unwrap();

    sdk.share_revoke_device(
        &repo_root,
        e2v_api::ShareRevokeDeviceRequest {
            device_id: accepted_device.device_id,
            password: "correct horse battery staple".to_string(),
        },
    )
    .unwrap();

    sdk.share_revoke_member(
        &repo_root,
        e2v_api::ShareRevokeMemberRequest {
            actor_id: accepted_member.actor_id,
            password: "correct horse battery staple".to_string(),
        },
    )
    .unwrap();
}

#[test]
fn sdk_can_verify_repair_accept_rollback_and_gc_default_remote() {
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(&repo_root).unwrap();
    fs::create_dir_all(&remote_root).unwrap();

    let sdk = Sdk::new();
    let state = sdk
        .init_repository(InitRepositoryOptions {
            repo_root: repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
    fs::write(repo_root.join("tracked.txt"), "alpha").unwrap();
    sdk.commit_repository(CommitRepositoryOptions {
        repo_root: repo_root.clone(),
        message: "seed".to_string(),
    })
    .unwrap();

    let remote_spec = format!(
        "file://{}",
        remote_root.to_string_lossy().replace('\\', "/")
    );
    sdk.add_remote(&repo_root, "origin", &remote_spec).unwrap();
    sdk.push_default_remote(PushRequest {
        repo_root: repo_root.clone(),
        branch_token: state.branch.token_hex.clone(),
        operation_id: "push-maintenance-1".to_string(),
    })
    .unwrap();

    let verified = sdk
        .verify_default_remote(e2v_api::VerifyRemoteRequest {
            repo_root: repo_root.clone(),
            sample_percent: 100,
        })
        .unwrap();
    assert!(verified.sampled_objects > 0);

    let object_path = fs::read_dir(repo_root.join(".e2v").join("objects"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::remove_file(&object_path).unwrap();
    let repaired = sdk.repair_default_remote(&repo_root).unwrap();
    assert!(repaired.repaired_objects > 0);
    assert!(object_path.exists());

    fs::write(repo_root.join("tracked.txt"), "beta").unwrap();
    sdk.commit_repository(CommitRepositoryOptions {
        repo_root: repo_root.clone(),
        message: "local-ahead".to_string(),
    })
    .unwrap();
    let accepted = sdk
        .force_accept_default_remote_rollback(&repo_root, "correct horse battery staple")
        .unwrap();
    assert!(accepted.repaired_objects <= repaired.repaired_objects);

    let gc_report = sdk.gc_default_remote_dry_run(&repo_root).unwrap();
    assert!(gc_report.unreachable_physical_refs.is_empty());

    let gc_error = sdk
        .gc_default_remote_execute(e2v_api::GcExecuteRequest {
            repo_root: repo_root.clone(),
            grace_period_days: 30,
            allow_single_writer_maintenance_window: false,
        })
        .unwrap_err();
    assert_eq!(gc_error.code(), SdkErrorCode::InvalidArgument);
    assert!(
        gc_error.message().contains("maintenance window"),
        "unexpected error: {gc_error:?}"
    );

    let gc_execute = sdk
        .gc_default_remote_execute(e2v_api::GcExecuteRequest {
            repo_root: repo_root.clone(),
            grace_period_days: 30,
            allow_single_writer_maintenance_window: true,
        })
        .unwrap();
    assert!(gc_execute.deleted_physical_refs.is_empty());
}

#[test]
fn sdk_can_run_maintenance_with_explicit_remote_spec_without_default_remote_registration() {
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(&repo_root).unwrap();
    fs::create_dir_all(&remote_root).unwrap();

    let sdk = Sdk::new();
    let state = sdk
        .init_repository(InitRepositoryOptions {
            repo_root: repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
    fs::write(repo_root.join("tracked.txt"), "alpha").unwrap();
    sdk.commit_repository(CommitRepositoryOptions {
        repo_root: repo_root.clone(),
        message: "seed".to_string(),
    })
    .unwrap();

    let remote_spec = file_remote_spec(&remote_root);
    sdk.push_remote(
        &remote_spec,
        PushRequest {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-explicit-maintenance-1".to_string(),
        },
    )
    .unwrap();

    let verified = sdk
        .verify_remote(
            &remote_spec,
            e2v_api::VerifyRemoteRequest {
                repo_root: repo_root.clone(),
                sample_percent: 100,
            },
        )
        .unwrap();
    assert!(verified.sampled_objects > 0);

    let object_path = fs::read_dir(repo_root.join(".e2v").join("objects"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::remove_file(&object_path).unwrap();
    let repaired = sdk.repair_remote(&remote_spec, &repo_root).unwrap();
    assert!(repaired.repaired_objects > 0);
    assert!(object_path.exists());

    fs::write(repo_root.join("tracked.txt"), "beta").unwrap();
    sdk.commit_repository(CommitRepositoryOptions {
        repo_root: repo_root.clone(),
        message: "local-ahead".to_string(),
    })
    .unwrap();
    let accepted = sdk
        .force_accept_remote_rollback(&remote_spec, &repo_root, "correct horse battery staple")
        .unwrap();
    assert!(accepted.repaired_objects <= repaired.repaired_objects);

    let gc_report = sdk.gc_remote_dry_run(&remote_spec, &repo_root).unwrap();
    assert!(gc_report.unreachable_physical_refs.is_empty());

    let gc_error = sdk
        .gc_remote_execute(
            &remote_spec,
            e2v_api::GcExecuteRequest {
                repo_root: repo_root.clone(),
                grace_period_days: 30,
                allow_single_writer_maintenance_window: false,
            },
        )
        .unwrap_err();
    assert_eq!(gc_error.code(), SdkErrorCode::InvalidArgument);
    assert!(
        gc_error.message().contains("maintenance window"),
        "unexpected error: {gc_error:?}"
    );

    let gc_execute = sdk
        .gc_remote_execute(
            &remote_spec,
            e2v_api::GcExecuteRequest {
                repo_root: repo_root.clone(),
                grace_period_days: 30,
                allow_single_writer_maintenance_window: true,
            },
        )
        .unwrap();
    assert!(gc_execute.deleted_physical_refs.is_empty());

    assert!(
        !repo_root
            .join(".e2v")
            .join("remotes")
            .join("default.json")
            .exists(),
        "explicit remote maintenance should not require default remote registration"
    );
}

#[test]
fn sdk_verify_default_remote_reports_corrupt_state_for_invalid_remote_layout_root() {
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(&repo_root).unwrap();
    fs::create_dir_all(&remote_root).unwrap();

    let sdk = Sdk::new();
    let state = sdk
        .init_repository(InitRepositoryOptions {
            repo_root: repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
    fs::write(repo_root.join("tracked.txt"), "alpha").unwrap();
    sdk.commit_repository(CommitRepositoryOptions {
        repo_root: repo_root.clone(),
        message: "seed".to_string(),
    })
    .unwrap();

    let remote_spec = file_remote_spec(&remote_root);
    sdk.add_remote(&repo_root, "origin", &remote_spec).unwrap();
    sdk.push_default_remote(PushRequest {
        repo_root: repo_root.clone(),
        branch_token: state.branch.token_hex.clone(),
        operation_id: "push-corrupt-layout-root".to_string(),
    })
    .unwrap();

    fs::write(remote_root.join("layout_root.json"), br#"{"invalid":true"#).unwrap();

    let error = sdk
        .verify_default_remote(e2v_api::VerifyRemoteRequest {
            repo_root: repo_root.clone(),
            sample_percent: 100,
        })
        .unwrap_err();

    assert_eq!(error.code(), SdkErrorCode::CorruptState);
    assert!(
        error
            .message()
            .contains("failed to decode remote layout root"),
        "unexpected error: {error:?}"
    );
}

#[test]
fn sdk_verify_default_remote_reports_corrupt_state_for_remote_keyring_pointer_generation_mismatch()
{
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(&repo_root).unwrap();
    fs::create_dir_all(&remote_root).unwrap();

    let sdk = Sdk::new();
    let state = sdk
        .init_repository(InitRepositoryOptions {
            repo_root: repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
    fs::write(repo_root.join("tracked.txt"), "alpha").unwrap();
    sdk.commit_repository(CommitRepositoryOptions {
        repo_root: repo_root.clone(),
        message: "seed".to_string(),
    })
    .unwrap();

    let remote_spec = file_remote_spec(&remote_root);
    sdk.add_remote(&repo_root, "origin", &remote_spec).unwrap();
    sdk.push_default_remote(PushRequest {
        repo_root: repo_root.clone(),
        branch_token: state.branch.token_hex.clone(),
        operation_id: "push-invalid-remote-keyring-generation".to_string(),
    })
    .unwrap();

    let repo_id = e2v_core::sync_support::read_repo_id(&repo_root).unwrap();
    let pointer_path = remote_root
        .join("control")
        .join("refs")
        .join("by-token")
        .join("keyring")
        .join(format!("{repo_id}.json"));
    let pointer_bytes = fs::read(&pointer_path).unwrap();
    let mut pointer_record: serde_json::Value = serde_json::from_slice(&pointer_bytes).unwrap();
    let pointer_json = pointer_record["value"]["bytes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_u64().unwrap() as u8)
        .collect::<Vec<_>>();
    let mut pointer_json: serde_json::Value = serde_json::from_slice(&pointer_json).unwrap();
    pointer_json["generation"] = serde_json::json!(0);
    pointer_record["value"]["bytes"] = serde_json::Value::Array(
        serde_json::to_vec(&pointer_json)
            .unwrap()
            .into_iter()
            .map(serde_json::Value::from)
            .collect(),
    );
    fs::write(&pointer_path, serde_json::to_vec(&pointer_record).unwrap()).unwrap();

    let error = sdk
        .verify_default_remote(e2v_api::VerifyRemoteRequest {
            repo_root: repo_root.clone(),
            sample_percent: 100,
        })
        .unwrap_err();

    assert_eq!(error.code(), SdkErrorCode::CorruptState);
    assert!(
        error
            .message()
            .contains("remote keyring pointer generation mismatch"),
        "unexpected error: {error:?}"
    );
}

#[test]
fn sdk_verify_default_remote_reports_rollback_detected_for_remote_keyring_generation_below_floor() {
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(&repo_root).unwrap();
    fs::create_dir_all(&remote_root).unwrap();

    let sdk = Sdk::new();
    let state = sdk
        .init_repository(InitRepositoryOptions {
            repo_root: repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
    fs::write(repo_root.join("tracked.txt"), "alpha").unwrap();
    sdk.commit_repository(CommitRepositoryOptions {
        repo_root: repo_root.clone(),
        message: "seed".to_string(),
    })
    .unwrap();

    let remote_spec = file_remote_spec(&remote_root);
    sdk.add_remote(&repo_root, "origin", &remote_spec).unwrap();
    sdk.push_default_remote(PushRequest {
        repo_root: repo_root.clone(),
        branch_token: state.branch.token_hex.clone(),
        operation_id: "push-rollback-remote-keyring-generation".to_string(),
    })
    .unwrap();

    let pointer_file_path = remote_root
        .join("control")
        .join("keyring")
        .join("keyring.current");
    let pointer_file_bytes = fs::read(&pointer_file_path).unwrap();
    let mut pointer_file_json: serde_json::Value =
        serde_json::from_slice(&pointer_file_bytes).unwrap();
    let current_keyring = pointer_file_json["current"].as_str().unwrap().to_string();
    pointer_file_json["generation"] = serde_json::json!(0);
    fs::write(
        &pointer_file_path,
        serde_json::to_vec(&pointer_file_json).unwrap(),
    )
    .unwrap();

    let keyring_generation_path = remote_root
        .join("control")
        .join("keyring")
        .join(&current_keyring);
    let keyring_generation_bytes = fs::read(&keyring_generation_path).unwrap();
    let mut keyring_generation_json: serde_json::Value =
        serde_json::from_slice(&keyring_generation_bytes).unwrap();
    keyring_generation_json["generation"] = serde_json::json!(0);
    fs::write(
        &keyring_generation_path,
        serde_json::to_vec(&keyring_generation_json).unwrap(),
    )
    .unwrap();

    let repo_id = e2v_core::sync_support::read_repo_id(&repo_root).unwrap();
    let pointer_ref_path = remote_root
        .join("control")
        .join("refs")
        .join("by-token")
        .join("keyring")
        .join(format!("{repo_id}.json"));
    let pointer_ref_bytes = fs::read(&pointer_ref_path).unwrap();
    let mut pointer_ref_json: serde_json::Value =
        serde_json::from_slice(&pointer_ref_bytes).unwrap();
    pointer_ref_json["value"]["bytes"] = serde_json::Value::Array(
        serde_json::to_vec(&pointer_file_json)
            .unwrap()
            .into_iter()
            .map(serde_json::Value::from)
            .collect(),
    );
    fs::write(
        &pointer_ref_path,
        serde_json::to_vec(&pointer_ref_json).unwrap(),
    )
    .unwrap();

    let error = sdk
        .verify_default_remote(e2v_api::VerifyRemoteRequest {
            repo_root: repo_root.clone(),
            sample_percent: 100,
        })
        .unwrap_err();

    assert_eq!(error.code(), SdkErrorCode::RollbackDetected);
    assert!(
        error.message().contains("CRITICAL_ROLLBACK_DETECTED")
            || error
                .message()
                .contains("remote keyring generation is invalid"),
        "unexpected error: {error:?}"
    );
}

#[test]
fn sdk_verify_default_remote_reports_corrupt_state_for_invalid_remote_branch_ref_record() {
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(&repo_root).unwrap();
    fs::create_dir_all(&remote_root).unwrap();

    let sdk = Sdk::new();
    let state = sdk
        .init_repository(InitRepositoryOptions {
            repo_root: repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
    fs::write(repo_root.join("tracked.txt"), "alpha").unwrap();
    sdk.commit_repository(CommitRepositoryOptions {
        repo_root: repo_root.clone(),
        message: "seed".to_string(),
    })
    .unwrap();

    let remote_spec = file_remote_spec(&remote_root);
    sdk.add_remote(&repo_root, "origin", &remote_spec).unwrap();
    sdk.push_default_remote(PushRequest {
        repo_root: repo_root.clone(),
        branch_token: state.branch.token_hex.clone(),
        operation_id: "push-invalid-remote-branch-ref".to_string(),
    })
    .unwrap();

    let branch_ref_path = remote_root
        .join("control")
        .join("refs")
        .join("by-token")
        .join(format!("{}.json", state.branch.token_hex));
    fs::write(&branch_ref_path, br#"{"broken":true"#).unwrap();

    let error = sdk
        .verify_default_remote(e2v_api::VerifyRemoteRequest {
            repo_root: repo_root.clone(),
            sample_percent: 100,
        })
        .unwrap_err();

    assert_eq!(error.code(), SdkErrorCode::CorruptState);
    assert!(
        error
            .message()
            .contains("failed to decode remote branch ref"),
        "unexpected error: {error:?}"
    );
}

#[test]
fn sdk_verify_default_remote_reports_corrupt_state_for_invalid_keyring_pointer_ref_record() {
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(&repo_root).unwrap();
    fs::create_dir_all(&remote_root).unwrap();

    let sdk = Sdk::new();
    let state = sdk
        .init_repository(InitRepositoryOptions {
            repo_root: repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
    fs::write(repo_root.join("tracked.txt"), "alpha").unwrap();
    sdk.commit_repository(CommitRepositoryOptions {
        repo_root: repo_root.clone(),
        message: "seed".to_string(),
    })
    .unwrap();

    let remote_spec = file_remote_spec(&remote_root);
    sdk.add_remote(&repo_root, "origin", &remote_spec).unwrap();
    sdk.push_default_remote(PushRequest {
        repo_root: repo_root.clone(),
        branch_token: state.branch.token_hex.clone(),
        operation_id: "push-invalid-keyring-pointer-ref-record".to_string(),
    })
    .unwrap();

    let repo_id = e2v_core::sync_support::read_repo_id(&repo_root).unwrap();
    let keyring_ref_path = remote_root
        .join("control")
        .join("refs")
        .join("by-token")
        .join("keyring")
        .join(format!("{repo_id}.json"));
    fs::write(&keyring_ref_path, br#"{"broken":true"#).unwrap();

    let error = sdk
        .verify_default_remote(e2v_api::VerifyRemoteRequest {
            repo_root: repo_root.clone(),
            sample_percent: 100,
        })
        .unwrap_err();

    assert_eq!(error.code(), SdkErrorCode::CorruptState);
    assert!(
        error
            .message()
            .contains("failed to decode remote branch ref"),
        "unexpected error: {error:?}"
    );
}

#[test]
fn sdk_verify_default_remote_reports_corrupt_state_for_invalid_pack_index_segment() {
    let _guard = e2v_sync::testing::override_small_object_pack_threshold_for_test(1);
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(&repo_root).unwrap();
    fs::create_dir_all(&remote_root).unwrap();

    let sdk = Sdk::new();
    let state = sdk
        .init_repository(InitRepositoryOptions {
            repo_root: repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
    fs::write(repo_root.join("tracked.txt"), "alpha").unwrap();
    sdk.commit_repository(CommitRepositoryOptions {
        repo_root: repo_root.clone(),
        message: "seed".to_string(),
    })
    .unwrap();

    let remote_spec = file_remote_spec(&remote_root);
    sdk.add_remote(&repo_root, "origin", &remote_spec).unwrap();
    sdk.push_default_remote(PushRequest {
        repo_root: repo_root.clone(),
        branch_token: state.branch.token_hex.clone(),
        operation_id: "push-invalid-pack-index-segment".to_string(),
    })
    .unwrap();

    let pack_index_segment_path = fs::read_dir(remote_root.join("packs").join("index"))
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .find(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .expect("expected packed push to publish a pack index segment");
    fs::write(&pack_index_segment_path, br#"{"broken":true"#).unwrap();

    let error = sdk
        .verify_default_remote(e2v_api::VerifyRemoteRequest {
            repo_root: repo_root.clone(),
            sample_percent: 100,
        })
        .unwrap_err();

    assert_eq!(error.code(), SdkErrorCode::CorruptState);
    assert!(
        error
            .message()
            .contains("failed to decrypt authenticated pack index segment"),
        "unexpected error: {error:?}"
    );
}

#[test]
fn sdk_verify_default_remote_reports_corrupt_state_for_invalid_trusted_state_file() {
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let remote_root = temp.path().join("remote");
    let trusted_state_root = temp.path().join("trusted-state");
    fs::create_dir_all(&repo_root).unwrap();
    fs::create_dir_all(&remote_root).unwrap();
    fs::create_dir_all(&trusted_state_root).unwrap();

    let _guard = e2v_sync::testing::override_trusted_state_dir_for_test(trusted_state_root.clone());

    let sdk = Sdk::new();
    let state = sdk
        .init_repository(InitRepositoryOptions {
            repo_root: repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
    fs::write(repo_root.join("tracked.txt"), "alpha").unwrap();
    sdk.commit_repository(CommitRepositoryOptions {
        repo_root: repo_root.clone(),
        message: "seed".to_string(),
    })
    .unwrap();

    let remote_spec = file_remote_spec(&remote_root);
    sdk.add_remote(&repo_root, "origin", &remote_spec).unwrap();
    sdk.push_default_remote(PushRequest {
        repo_root: repo_root.clone(),
        branch_token: state.branch.token_hex.clone(),
        operation_id: "push-invalid-trusted-state".to_string(),
    })
    .unwrap();

    sdk.verify_default_remote(e2v_api::VerifyRemoteRequest {
        repo_root: repo_root.clone(),
        sample_percent: 100,
    })
    .unwrap();

    let repo_id = e2v_core::sync_support::read_repo_id(&repo_root).unwrap();
    fs::write(
        trusted_state_root.join(format!("{repo_id}.json")),
        br#"{"broken":true"#,
    )
    .unwrap();

    let error = sdk
        .verify_default_remote(e2v_api::VerifyRemoteRequest {
            repo_root: repo_root.clone(),
            sample_percent: 100,
        })
        .unwrap_err();

    assert_eq!(error.code(), SdkErrorCode::CorruptState);
    assert!(
        error.message().contains("failed to decode trusted state"),
        "unexpected error: {error:?}"
    );
}

#[test]
fn sdk_fetch_default_remote_reports_corrupt_state_for_invalid_local_keyring_pointer() {
    let temp = tempfile::tempdir().unwrap();
    let source_repo = temp.path().join("source");
    let clone_repo = temp.path().join("clone");
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(&source_repo).unwrap();
    fs::create_dir_all(&remote_root).unwrap();

    let sdk = Sdk::new();
    let state = sdk
        .init_repository(InitRepositoryOptions {
            repo_root: source_repo.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
    fs::write(source_repo.join("tracked.txt"), "alpha").unwrap();
    sdk.commit_repository(CommitRepositoryOptions {
        repo_root: source_repo.clone(),
        message: "seed".to_string(),
    })
    .unwrap();

    let remote_spec = file_remote_spec(&remote_root);
    sdk.add_remote(&source_repo, "origin", &remote_spec)
        .unwrap();
    sdk.push_default_remote(PushRequest {
        repo_root: source_repo.clone(),
        branch_token: state.branch.token_hex.clone(),
        operation_id: "push-invalid-local-keyring-pointer".to_string(),
    })
    .unwrap();

    let cloned = sdk
        .clone_remote(CloneRequest {
            remote_spec: remote_spec.clone(),
            target_repo_root: clone_repo.clone(),
            password: "correct horse battery staple".to_string(),
            branch_token: state.branch.token_hex.clone(),
        })
        .unwrap();
    sdk.add_remote(&clone_repo, "origin", &remote_spec).unwrap();

    fs::write(
        clone_repo
            .join(".e2v")
            .join("keyring")
            .join("keyring.current"),
        br#"{"broken":true"#,
    )
    .unwrap();

    let error = sdk
        .fetch_default_remote(FetchRequest {
            repo_root: clone_repo.clone(),
            branch_token: cloned.branch_token,
            password: Some("correct horse battery staple".to_string()),
        })
        .unwrap_err();

    assert_eq!(error.code(), SdkErrorCode::CorruptState);
    assert!(
        error
            .message()
            .contains("failed to decode local keyring pointer"),
        "unexpected error: {error:?}"
    );
}

#[test]
fn sdk_historical_rewrite_plan_reports_corrupt_state_for_invalid_current_keyring_pointer() {
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(&repo_root).unwrap();
    fs::create_dir_all(&remote_root).unwrap();

    let sdk = Sdk::new();
    let state = sdk
        .init_repository(InitRepositoryOptions {
            repo_root: repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
    fs::write(repo_root.join("tracked.txt"), "alpha").unwrap();
    sdk.commit_repository(CommitRepositoryOptions {
        repo_root: repo_root.clone(),
        message: "seed".to_string(),
    })
    .unwrap();

    let remote_spec = file_remote_spec(&remote_root);
    sdk.add_remote(&repo_root, "origin", &remote_spec).unwrap();
    sdk.push_default_remote(PushRequest {
        repo_root: repo_root.clone(),
        branch_token: state.branch.token_hex.clone(),
        operation_id: "push-invalid-current-keyring-pointer".to_string(),
    })
    .unwrap();

    fs::write(
        repo_root
            .join(".e2v")
            .join("keyring")
            .join("keyring.current"),
        br#"{"broken":true"#,
    )
    .unwrap();

    let error = sdk
        .historical_rewrite_default_remote_plan(HistoricalRewritePlanRequest {
            repo_root: repo_root.clone(),
        })
        .unwrap_err();

    assert_eq!(error.code(), SdkErrorCode::CorruptState);
    assert!(
        error
            .message()
            .contains("failed to decode current keyring pointer"),
        "unexpected error: {error:?}"
    );
}

#[test]
fn sdk_can_plan_and_execute_historical_rewrite_via_default_and_explicit_remote() {
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let remote_root = temp.path().join("remote");
    let clone_root = temp.path().join("clone");
    fs::create_dir_all(&repo_root).unwrap();
    fs::create_dir_all(&remote_root).unwrap();

    let sdk = Sdk::new();
    let state = sdk
        .init_repository(InitRepositoryOptions {
            repo_root: repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
    fs::write(repo_root.join("tracked.txt"), "alpha").unwrap();
    sdk.commit_repository(CommitRepositoryOptions {
        repo_root: repo_root.clone(),
        message: "seed".to_string(),
    })
    .unwrap();

    let remote_spec = file_remote_spec(&remote_root);
    sdk.add_remote(&repo_root, "origin", &remote_spec).unwrap();
    sdk.push_default_remote(PushRequest {
        repo_root: repo_root.clone(),
        branch_token: state.branch.token_hex.clone(),
        operation_id: "push-historical-default".to_string(),
    })
    .unwrap();

    e2v_core::testing::rotate_active_epoch_for_test(&repo_root, "correct horse battery staple")
        .unwrap();

    let default_plan = sdk
        .historical_rewrite_default_remote_plan(HistoricalRewritePlanRequest {
            repo_root: repo_root.clone(),
        })
        .unwrap();
    assert!(default_plan.reachable_object_count > 0);
    assert_eq!(default_plan.old_epoch_count, 1);
    assert!(default_plan.requires_remote_credential_revocation_guidance);
    assert!(default_plan.advisory_messages.iter().any(|message| {
        message.contains("remote storage credentials") && message.contains("large repositories")
    }));

    let default_execute = sdk
        .historical_rewrite_default_remote_execute(HistoricalRewriteExecuteRequest {
            repo_root: repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            confirm_full_reencryption: true,
        })
        .unwrap();
    assert!(default_execute.rewritten_objects > 0);
    assert_eq!(default_execute.retired_epoch_count, 1);
    assert!(default_execute.next_layout_generation >= 2);

    let cloned = sdk
        .clone_remote(CloneRequest {
            remote_spec: remote_spec.clone(),
            target_repo_root: clone_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_token: state.branch.token_hex.clone(),
        })
        .unwrap();
    let read = sdk.open_read_handle(&clone_root).unwrap();
    let snapshot = read.resolve_branch(&cloned.branch_token).unwrap();
    let file = read.open_file(&snapshot, "tracked.txt").unwrap();
    let bytes = read.read_range(&file, 0, 32).unwrap();
    assert_eq!(String::from_utf8(bytes).unwrap(), "alpha");

    let explicit_plan = sdk
        .historical_rewrite_remote_plan(
            &remote_spec,
            HistoricalRewritePlanRequest {
                repo_root: repo_root.clone(),
            },
        )
        .unwrap();
    assert_eq!(
        explicit_plan.reachable_object_count,
        default_plan.reachable_object_count
    );

    let explicit_error = sdk
        .historical_rewrite_remote_execute(
            &remote_spec,
            HistoricalRewriteExecuteRequest {
                repo_root: repo_root.clone(),
                password: "correct horse battery staple".to_string(),
                confirm_full_reencryption: false,
            },
        )
        .unwrap_err();
    assert_eq!(explicit_error.code(), SdkErrorCode::InvalidArgument);
    assert!(
        explicit_error
            .message()
            .contains("full re-encryption confirmation"),
        "unexpected error: {explicit_error:?}"
    );
}

#[test]
fn sdk_can_plan_status_enable_and_reshuffle_oblivious_layout() {
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let remote_root = temp.path().join("remote");
    fs::create_dir_all(&repo_root).unwrap();
    fs::create_dir_all(&remote_root).unwrap();

    let sdk = Sdk::new();
    let state = sdk
        .init_repository(InitRepositoryOptions {
            repo_root: repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
    fs::write(repo_root.join("tracked.txt"), "alpha").unwrap();
    sdk.commit_repository(CommitRepositoryOptions {
        repo_root: repo_root.clone(),
        message: "seed".to_string(),
    })
    .unwrap();

    let remote_spec = file_remote_spec(&remote_root);
    sdk.add_remote(&repo_root, "origin", &remote_spec).unwrap();
    sdk.push_default_remote(PushRequest {
        repo_root: repo_root.clone(),
        branch_token: state.branch.token_hex.clone(),
        operation_id: "push-oblivious-default".to_string(),
    })
    .unwrap();

    let plan = sdk
        .oblivious_layout_default_remote_plan(ObliviousLayoutPlanRequest {
            repo_root: repo_root.clone(),
        })
        .unwrap();
    assert!(plan.estimated_real_reads_per_request >= 1);
    assert!(plan.estimated_cover_reads_per_request >= 1);
    assert!(plan.estimated_bytes_per_request > 0);
    assert!(plan.requires_layout_root_rewrite);

    let enabled = sdk
        .enable_oblivious_layout_default_remote(EnableObliviousLayoutRequest {
            repo_root: repo_root.clone(),
            policy_profile: "balanced".to_string(),
        })
        .unwrap();
    assert_eq!(enabled.layout_mode, "oblivious");
    assert_eq!(enabled.dedup_mode, "generation-scoped-randomized");
    assert!(enabled.oblivious_generation.is_some());

    let status = sdk
        .oblivious_layout_default_remote_status(repo_root.clone())
        .unwrap();
    assert_eq!(status.layout_mode, "oblivious");
    assert_eq!(status.policy_profile, "balanced");

    let reshuffled = sdk
        .reshuffle_oblivious_layout_default_remote(ReshuffleObliviousLayoutRequest {
            repo_root: repo_root.clone(),
            policy_profile: "balanced".to_string(),
        })
        .unwrap();
    assert!(reshuffled.layout_generation > enabled.layout_generation);
    assert!(reshuffled.oblivious_generation > enabled.oblivious_generation);

    let explicit_status = sdk
        .oblivious_layout_remote_status(&remote_spec, repo_root.clone())
        .unwrap();
    assert_eq!(
        explicit_status.layout_generation,
        reshuffled.layout_generation
    );
}

#[test]
fn sdk_default_remote_workflows_also_support_webdav_specs() {
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let sdk = Sdk::new();
    sdk.init_repository(InitRepositoryOptions {
        repo_root: repo_root.clone(),
        password: "correct horse battery staple".to_string(),
        branch_name: "main".to_string(),
    })
    .unwrap();

    sdk.add_remote(
        &repo_root,
        "origin",
        "webdav+https://alice:secret@example.com/repo",
    )
    .unwrap();

    let loaded = sdk.load_default_remote(&repo_root).unwrap();
    assert_eq!(loaded.name, "origin");
    assert_eq!(loaded.spec, "webdav+https://alice:secret@example.com/repo");
}

#[test]
fn sdk_can_register_s3_remote_specs_as_default_remotes() {
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let sdk = Sdk::new();
    sdk.init_repository(InitRepositoryOptions {
        repo_root: repo_root.clone(),
        password: "correct horse battery staple".to_string(),
        branch_name: "main".to_string(),
    })
    .unwrap();

    let remote_spec =
        "s3+https://alice:secret@s3.example.com/example-bucket/sync-root?region=us-east-1";
    sdk.add_remote(&repo_root, "origin", remote_spec).unwrap();

    let loaded = sdk.load_default_remote(&repo_root).unwrap();
    assert_eq!(loaded.name, "origin");
    assert_eq!(loaded.spec, remote_spec);
}
