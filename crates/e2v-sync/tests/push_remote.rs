use std::fs;

use e2v_core::{CommitOptions, InitOptions, RepositoryFacade};
use e2v_store::{BlobStore, LayoutRootStore, MemoryBackend, RefStore, RefToken};
use tempfile::tempdir;

use e2v_sync::{push_head, resume_push, PushOptions, ResumeOptions};

#[test]
fn push_uploads_reachable_objects_and_publishes_remote_ref() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    let state = facade
        .init(InitOptions {
            repo_root: repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
    fs::write(repo_root.join("hello.txt"), b"hello remote").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "push-happy-path".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();

    let result = push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-1".to_string(),
        },
    )
    .unwrap();

    assert_eq!(result.published_snapshot_id, commit.snapshot_id);
    let stored_ref = remote
        .read_ref(&RefToken::new(state.branch.token_hex.clone()))
        .unwrap()
        .unwrap();
    assert!(!stored_ref.value.bytes.is_empty());
    assert!(remote.list_physical("objects/").unwrap().len() > 0);
    assert_eq!(
        remote.read_layout_root().unwrap().generation,
        state.layout_generation
    );
}

#[test]
fn resume_skips_uploaded_objects_and_republishes_missing_ref() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    let state = facade
        .init(InitOptions {
            repo_root: repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
    fs::write(repo_root.join("hello.txt"), b"hello remote").unwrap();
    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "resume-push".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    let result = push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "resume-op".to_string(),
        },
    )
    .unwrap();
    assert_eq!(result.published_snapshot_id, commit.snapshot_id);

    remote
        .compare_and_swap_ref(
            &RefToken::new(state.branch.token_hex.clone()),
            Some(e2v_store::RefVersion { value: 1 }),
            e2v_store::EncryptedRef::new(vec![9, 9, 9]),
        )
        .unwrap();
    let resumed = resume_push(
        &facade,
        &remote,
        ResumeOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "resume-op".to_string(),
        },
    )
    .unwrap();

    assert!(resumed.skipped_uploaded_objects > 0);
    assert_eq!(resumed.published_snapshot_id, commit.snapshot_id);
}

#[test]
fn stale_remote_head_marks_push_needs_rebase() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    let state = facade
        .init(InitOptions {
            repo_root: repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
    fs::write(repo_root.join("hello.txt"), b"hello remote").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "needs-rebase".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    remote
        .compare_and_swap_ref(
            &RefToken::new(state.branch.token_hex.clone()),
            None,
            e2v_store::EncryptedRef::new(vec![7, 7, 7]),
        )
        .unwrap();

    let error = push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "needs-rebase-op".to_string(),
        },
    )
    .unwrap_err();

    assert!(error.to_string().contains("needs-rebase"));
}

#[test]
fn push_rejects_missing_remote_parent_chain() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    let state = facade
        .init(InitOptions {
            repo_root: repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();

    fs::write(repo_root.join("hello.txt"), b"first").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "first".to_string(),
        })
        .unwrap();

    fs::write(repo_root.join("hello.txt"), b"second").unwrap();
    let second = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "second".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();

    let error = push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "missing-parent-op".to_string(),
        },
    )
    .unwrap_err();

    assert!(error.to_string().contains("ancestor"));
    assert!(
        remote
            .read_ref(&RefToken::new(state.branch.token_hex.clone()))
            .unwrap()
            .is_none()
    );
    assert!(second.snapshot_id.len() > 10);
}
