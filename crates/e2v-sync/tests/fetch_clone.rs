use std::fs;

use e2v_core::{CommitOptions, InitOptions, RepositoryFacade};
use e2v_store::{
    BlobStore, LayoutRootStore, MemoryBackend, OpendalMemoryBackend, RefStore, RefToken,
    S3CompatibleMockBackend,
};
use tempfile::tempdir;

use e2v_sync::{clone_remote, fetch_remote, push_head, CloneOptions, FetchOptions, PushOptions};

fn seed_remote() -> (
    tempfile::TempDir,
    RepositoryFacade,
    std::path::PathBuf,
    String,
    MemoryBackend,
) {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("source");
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
            message: "push-happy-path".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-seed".to_string(),
        },
    )
    .unwrap();

    (temp, facade, repo_root, state.branch.token_hex, remote)
}

#[test]
fn fetch_downloads_remote_ref_and_missing_objects_without_touching_worktree() {
    let (temp, _facade, _source_repo_root, branch_token, remote) = seed_remote();
    let target_repo_root = temp.path().join("fetch-target");
    fs::create_dir_all(&target_repo_root).unwrap();
    let local = RepositoryFacade::new();
    local
        .init(InitOptions {
            repo_root: target_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
    fs::write(target_repo_root.join("local-only.txt"), b"leave me alone").unwrap();

    let result = fetch_remote(
        &remote,
        FetchOptions {
            repo_root: target_repo_root.clone(),
            branch_token: branch_token.clone(),
        },
    )
    .unwrap();

    assert!(result.downloaded_objects > 0);
    assert_eq!(
        fs::read(target_repo_root.join("local-only.txt")).unwrap(),
        b"leave me alone"
    );
    assert!(target_repo_root.join(".e2v").join("objects").read_dir().unwrap().count() > 0);
}

#[test]
fn clone_bootstraps_local_repository_from_remote_head() {
    let (temp, _facade, _source_repo_root, branch_token, remote) = seed_remote();
    let clone_repo_root = temp.path().join("clone-target");

    let cloned = clone_remote(
        &remote,
        CloneOptions {
            repo_root: clone_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
            branch_token,
        },
    )
    .unwrap();

    assert!(cloned.head_snapshot_id.is_some());
    assert!(clone_repo_root.join(".e2v").join("objects").is_dir());
    assert!(remote.read_layout_root().unwrap().generation >= 1);
    assert!(
        remote
            .read_ref(&RefToken::new(cloned.branch_token.clone()))
            .unwrap()
            .is_some()
    );
    assert!(remote.list_physical("objects/").unwrap().len() > 0);
}

#[test]
fn sync_flows_work_with_s3_compatible_backend_adapter() {
    let temp = tempdir().unwrap();
    let source_repo_root = temp.path().join("source");
    fs::create_dir_all(&source_repo_root).unwrap();

    let facade = RepositoryFacade::new();
    let state = facade
        .init(InitOptions {
            repo_root: source_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
    fs::write(source_repo_root.join("hello.txt"), b"hello remote").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: source_repo_root.clone(),
            message: "adapter".to_string(),
        })
        .unwrap();

    let remote = S3CompatibleMockBackend::new();
    let pushed = push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: source_repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "adapter-push".to_string(),
        },
    )
    .unwrap();
    assert!(pushed.uploaded_objects > 0);

    let fetch_repo_root = temp.path().join("fetch-target");
    fs::create_dir_all(&fetch_repo_root).unwrap();
    let local = RepositoryFacade::new();
    local
        .init(InitOptions {
            repo_root: fetch_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
    let fetched = fetch_remote(
        &remote,
        FetchOptions {
            repo_root: fetch_repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
        },
    )
    .unwrap();
    assert!(fetched.downloaded_objects > 0);

    let clone_repo_root = temp.path().join("clone-target");
    let cloned = clone_remote(
        &remote,
        CloneOptions {
            repo_root: clone_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
            branch_token: state.branch.token_hex.clone(),
        },
    )
    .unwrap();
    assert!(cloned.head_snapshot_id.is_some());
}

#[test]
fn sync_flows_work_with_opendal_memory_backend_adapter() {
    let temp = tempdir().unwrap();
    let source_repo_root = temp.path().join("source");
    fs::create_dir_all(&source_repo_root).unwrap();

    let facade = RepositoryFacade::new();
    let state = facade
        .init(InitOptions {
            repo_root: source_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
    fs::write(source_repo_root.join("hello.txt"), b"hello opendal").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: source_repo_root.clone(),
            message: "adapter-opendal".to_string(),
        })
        .unwrap();

    let remote = OpendalMemoryBackend::new().unwrap();
    let push_error = push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: source_repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "adapter-opendal-push".to_string(),
        },
    )
    .unwrap_err();
    assert!(push_error.to_string().contains("read-only"));

    for relative_path in e2v_core::sync_support::list_local_object_files(&source_repo_root)
        .unwrap()
        .into_iter()
        .map(|path| {
            let file_name = path.file_name().unwrap().to_str().unwrap().to_string();
            let bytes = fs::read(&path).unwrap();
            (format!("objects/{file_name}"), bytes)
        })
    {
        remote.put_physical(&relative_path.0, &relative_path.1).unwrap();
    }
    remote
        .put_physical(
            "control/config.json",
            &e2v_core::sync_support::read_config_bytes(&source_repo_root).unwrap(),
        )
        .unwrap();
    remote
        .put_physical(
            "control/refs/default.json",
            &e2v_core::sync_support::read_default_ref_bytes(&source_repo_root).unwrap(),
        )
        .unwrap();
    for keyring_file in e2v_core::sync_support::list_keyring_files(&source_repo_root).unwrap() {
        let file_name = keyring_file.file_name().unwrap().to_str().unwrap();
        remote
            .put_physical(
                &format!("control/keyring/{file_name}"),
                &fs::read(&keyring_file).unwrap(),
            )
            .unwrap();
    }
    let layout_root = remote.read_layout_root().unwrap();
    let next_layout_root: e2v_store::LayoutRoot =
        serde_json::from_slice(&e2v_core::sync_support::read_layout_root_bytes(&source_repo_root).unwrap())
            .unwrap();
    remote
        .compare_and_swap_layout_root(layout_root.generation, next_layout_root)
        .unwrap();
    remote
        .compare_and_swap_ref(
            &RefToken::new(state.branch.token_hex.clone()),
            None,
            e2v_store::EncryptedRef::new(
                e2v_core::sync_support::read_default_ref_bytes(&source_repo_root).unwrap(),
            ),
        )
        .unwrap();

    let fetch_repo_root = temp.path().join("fetch-target");
    fs::create_dir_all(&fetch_repo_root).unwrap();
    let local = RepositoryFacade::new();
    local
        .init(InitOptions {
            repo_root: fetch_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
    let fetched = fetch_remote(
        &remote,
        FetchOptions {
            repo_root: fetch_repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
        },
    )
    .unwrap();
    assert!(fetched.downloaded_objects > 0);

    let clone_repo_root = temp.path().join("clone-target");
    let cloned = clone_remote(
        &remote,
        CloneOptions {
            repo_root: clone_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
            branch_token: state.branch.token_hex.clone(),
        },
    )
    .unwrap();
    assert!(cloned.head_snapshot_id.is_some());
}
