use std::fs;

use e2v_core::{
    BranchOverlayChange, BranchWritebackOptions, BranchWritebackOutcome, CommitOptions,
    InitOptions, ManifestStore, ManifestStoreApi, RepositoryFacade,
};
use tempfile::tempdir;

fn init_repo(repo_root: &std::path::Path) {
    RepositoryFacade::new()
        .init(InitOptions {
            repo_root: repo_root.to_path_buf(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
}

fn commit_message(repo_root: &std::path::Path, message: &str, path: &str, body: &str) -> String {
    let full_path = repo_root.join(path);
    if let Some(parent) = full_path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(full_path, body).unwrap();
    RepositoryFacade::new()
        .commit(CommitOptions {
            repo_root: repo_root.to_path_buf(),
            message: message.to_string(),
        })
        .unwrap()
        .snapshot_id
}

fn read_branch_file(repo_root: &std::path::Path, branch_token: &str, path: &str) -> String {
    let facade = RepositoryFacade::new();
    let read_service = facade.read_service(repo_root).unwrap();
    let snapshot = read_service.resolve_branch(branch_token).unwrap();
    let file = read_service.open_file(&snapshot, path).unwrap();
    String::from_utf8(read_service.read_range(&file, 0, usize::MAX).unwrap()).unwrap()
}

#[test]
fn branch_writeback_commits_overlay_bytes_to_the_target_branch() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let base_snapshot = commit_message(&repo_root, "base", "tracked.txt", "alpha");
    let facade = RepositoryFacade::new();
    let branch_token = facade.open(&repo_root).unwrap().branch.token_hex;

    let outcome = facade
        .write_branch_overlay(BranchWritebackOptions {
            repo_root: repo_root.clone(),
            ref_token_hex: branch_token.clone(),
            expected_head_snapshot_id: Some(base_snapshot.clone()),
            message: "overlay writeback".to_string(),
            changes: vec![
                BranchOverlayChange::CreateDirectory {
                    path: "nested".to_string(),
                },
                BranchOverlayChange::UpsertFile {
                    path: "nested/new.txt".to_string(),
                    bytes: b"beta".to_vec(),
                },
            ],
        })
        .unwrap();

    let BranchWritebackOutcome::Committed(success) = outcome else {
        panic!("expected overlay writeback to commit successfully");
    };
    assert_eq!(
        success.previous_head_snapshot_id.as_deref(),
        Some(base_snapshot.as_str())
    );
    assert!(!success.rebased);

    assert_eq!(
        read_branch_file(&repo_root, &branch_token, "tracked.txt"),
        "alpha"
    );
    assert_eq!(
        read_branch_file(&repo_root, &branch_token, "nested/new.txt"),
        "beta"
    );

    let snapshot = ManifestStore::new(&repo_root)
        .get_snapshot(&success.snapshot_id)
        .unwrap();
    assert_eq!(snapshot.parent_snapshot_id, Some(base_snapshot));
}

#[test]
fn branch_writeback_can_create_nested_directories_before_staging_a_file() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let base_snapshot = commit_message(&repo_root, "base", "tracked.txt", "alpha");
    let facade = RepositoryFacade::new();
    let branch_token = facade.open(&repo_root).unwrap().branch.token_hex;

    let outcome = facade
        .write_branch_overlay(BranchWritebackOptions {
            repo_root: repo_root.clone(),
            ref_token_hex: branch_token.clone(),
            expected_head_snapshot_id: Some(base_snapshot),
            message: "nested overlay writeback".to_string(),
            changes: vec![
                BranchOverlayChange::CreateDirectory {
                    path: "nested".to_string(),
                },
                BranchOverlayChange::CreateDirectory {
                    path: "nested/deeper".to_string(),
                },
                BranchOverlayChange::UpsertFile {
                    path: "nested/deeper/new.txt".to_string(),
                    bytes: b"payload".to_vec(),
                },
            ],
        })
        .unwrap();

    let BranchWritebackOutcome::Committed(_) = outcome else {
        panic!("expected nested overlay directory creation to commit successfully");
    };
    assert_eq!(
        read_branch_file(&repo_root, &branch_token, "nested/deeper/new.txt"),
        "payload"
    );
}

#[test]
fn branch_writeback_rebases_non_overlapping_overlay_changes_onto_a_newer_head() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let base_snapshot = commit_message(&repo_root, "base", "tracked.txt", "alpha");
    let facade = RepositoryFacade::new();
    let branch_token = facade.open(&repo_root).unwrap().branch.token_hex;
    let advanced_snapshot = commit_message(&repo_root, "remote", "remote.txt", "from-remote");

    let outcome = facade
        .write_branch_overlay(BranchWritebackOptions {
            repo_root: repo_root.clone(),
            ref_token_hex: branch_token.clone(),
            expected_head_snapshot_id: Some(base_snapshot),
            message: "rebased overlay".to_string(),
            changes: vec![BranchOverlayChange::UpsertFile {
                path: "local.txt".to_string(),
                bytes: b"from-local".to_vec(),
            }],
        })
        .unwrap();

    let BranchWritebackOutcome::Committed(success) = outcome else {
        panic!("expected non-overlapping overlay writeback to commit successfully");
    };
    assert!(success.rebased);
    assert_eq!(
        success.previous_head_snapshot_id.as_deref(),
        Some(advanced_snapshot.as_str())
    );
    assert_eq!(
        read_branch_file(&repo_root, &branch_token, "tracked.txt"),
        "alpha"
    );
    assert_eq!(
        read_branch_file(&repo_root, &branch_token, "remote.txt"),
        "from-remote"
    );
    assert_eq!(
        read_branch_file(&repo_root, &branch_token, "local.txt"),
        "from-local"
    );

    let snapshot = ManifestStore::new(&repo_root)
        .get_snapshot(&success.snapshot_id)
        .unwrap();
    assert_eq!(snapshot.parent_snapshot_id, Some(advanced_snapshot));
}

#[test]
fn branch_writeback_reports_conflicts_when_the_branch_changed_a_dirty_path() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let base_snapshot = commit_message(&repo_root, "base", "tracked.txt", "alpha");
    let facade = RepositoryFacade::new();
    let branch_token = facade.open(&repo_root).unwrap().branch.token_hex;
    let advanced_snapshot = commit_message(&repo_root, "remote", "tracked.txt", "from-remote");

    let outcome = facade
        .write_branch_overlay(BranchWritebackOptions {
            repo_root: repo_root.clone(),
            ref_token_hex: branch_token.clone(),
            expected_head_snapshot_id: Some(base_snapshot),
            message: "conflicting overlay".to_string(),
            changes: vec![BranchOverlayChange::UpsertFile {
                path: "tracked.txt".to_string(),
                bytes: b"from-local".to_vec(),
            }],
        })
        .unwrap();

    let BranchWritebackOutcome::Conflicted(conflict) = outcome else {
        panic!("expected conflicting overlay writeback to report a conflict");
    };
    assert_eq!(
        conflict.current_head_snapshot_id.as_deref(),
        Some(advanced_snapshot.as_str())
    );
    assert!(
        conflict
            .conflicts
            .iter()
            .any(|entry| entry.path == "tracked.txt"),
        "expected conflict list to mention tracked.txt, got {:?}",
        conflict.conflicts
    );
    assert_eq!(
        read_branch_file(&repo_root, &branch_token, "tracked.txt"),
        "from-remote"
    );
}
