use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use e2v_core::{CommitOptions, InitOptions, RepositoryFacade};
use tempfile::tempdir;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;

use e2v_vfs::{
    CachePolicy, MountRequest, PlatformCapabilities, ReadOnlyVfs, VfsMountConfig, VfsNodeKind,
    VfsSemantic,
    testing::{
        LinuxMountAdapter, MacosMountAdapter, PlatformFamily, PlatformMountAdapter,
        VfsHostLauncher, WindowsMountLauncher, WinfspHostConfig, WinfspHostDriver,
        WinfspHostLauncher, WinfspHostSession, WinfspInvalidator, WinfspMountContext,
        WinfspOpenRequest, WinfspRuntimeLibrary, WinfspVolumeParams, opened_file_cached_plaintext,
        winfsp_host_session_new, winfsp_runtime_get_symbol_address,
        winfsp_runtime_paths_from_candidate_roots, winfsp_runtime_paths_from_install_root,
        winfsp_runtime_resolve_mount_exports, winfsp_session_build_native_create_request,
        winfsp_session_create_filesystem_handle, winfsp_session_destroy_filesystem_handle,
        winfsp_session_has_native_filesystem_handle, winfsp_session_is_mounted,
        winfsp_session_run_mount_lifecycle,
    },
    try_mount_snapshot_on_current_platform,
};

enum UnreadableCacheEntryGuard {
    #[cfg(unix)]
    Permissions { path: PathBuf, original_mode: u32 },
    #[cfg(windows)]
    Locked { _file: std::fs::File },
    #[cfg(not(any(unix, windows)))]
    Noop,
}

impl Drop for UnreadableCacheEntryGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Self::Permissions {
            path,
            original_mode,
        } = self
        {
            let mut permissions = fs::metadata(&path).unwrap().permissions();
            permissions.set_mode(*original_mode);
            fs::set_permissions(&path, permissions).unwrap();
        }
    }
}

fn make_unreadable_cache_entry(path: &Path) -> UnreadableCacheEntryGuard {
    #[cfg(unix)]
    {
        fs::write(path, b"foreign").unwrap();
        let metadata = fs::metadata(path).unwrap();
        let original_mode = metadata.permissions().mode();
        let mut permissions = metadata.permissions();
        permissions.set_mode(0);
        fs::set_permissions(path, permissions).unwrap();
        UnreadableCacheEntryGuard::Permissions {
            path: path.to_path_buf(),
            original_mode,
        }
    }

    #[cfg(windows)]
    {
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .share_mode(0)
            .open(path)
            .unwrap();
        file.write_all(b"foreign").unwrap();
        file.flush().unwrap();
        UnreadableCacheEntryGuard::Locked { _file: file }
    }

    #[cfg(not(any(unix, windows)))]
    {
        fs::write(path, b"foreign").unwrap();
        UnreadableCacheEntryGuard::Noop
    }
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

fn commit_message(repo_root: &Path, message: &str, body: &str) -> String {
    fs::write(repo_root.join("tracked.txt"), body).unwrap();
    RepositoryFacade::new()
        .commit(CommitOptions {
            repo_root: repo_root.to_path_buf(),
            message: message.to_string(),
        })
        .unwrap()
        .snapshot_id
}

#[test]
fn snapshot_pinned_mount_keeps_original_snapshot_after_branch_head_moves() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let first_snapshot_id = commit_message(&repo_root, "first", "alpha");
    let vfs = ReadOnlyVfs::mount_snapshot(VfsMountConfig::snapshot(
        repo_root.clone(),
        first_snapshot_id.clone(),
    ))
    .unwrap();

    let handle = vfs.open_file("tracked.txt").unwrap();

    commit_message(&repo_root, "second", "beta");

    let bytes = vfs.read(&handle, 0, 32).unwrap();
    assert_eq!(String::from_utf8(bytes).unwrap(), "alpha");
    assert_eq!(vfs.namespace_snapshot_id(), first_snapshot_id);
}

#[test]
fn opening_same_file_in_same_snapshot_reuses_stable_inode_id() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let first_snapshot_id = commit_message(&repo_root, "first", "alpha");
    let vfs = ReadOnlyVfs::mount_snapshot(VfsMountConfig::snapshot(repo_root, first_snapshot_id))
        .unwrap();

    let first = vfs.open_file("tracked.txt").unwrap();
    let second = vfs.open_file("tracked.txt").unwrap();

    assert!(first.inode_id() > 0);
    assert_eq!(first.inode_id(), second.inode_id());
}

#[test]
fn mount_snapshot_rejects_live_branch_mode_config() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    commit_message(&repo_root, "first", "alpha");
    let branch_token = RepositoryFacade::new()
        .open(&repo_root)
        .unwrap()
        .branch
        .token_hex;

    let error = ReadOnlyVfs::mount_snapshot(VfsMountConfig::live_branch(repo_root, branch_token))
        .unwrap_err();
    assert!(error.to_string().contains("snapshot"));
}

#[test]
fn mount_live_branch_rejects_snapshot_mode_config() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let snapshot_id = commit_message(&repo_root, "first", "alpha");

    let error = ReadOnlyVfs::mount_live_branch(VfsMountConfig::snapshot(repo_root, snapshot_id))
        .unwrap_err();
    assert!(error.to_string().contains("live branch"));
}

#[test]
fn repeated_reads_can_use_plaintext_memory_cache_after_local_objects_become_unreadable() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let first_snapshot_id = commit_message(&repo_root, "first", "alpha");
    let vfs = ReadOnlyVfs::mount_snapshot(VfsMountConfig::snapshot(
        repo_root.clone(),
        first_snapshot_id,
    ))
    .unwrap();

    let handle = vfs.open_file("tracked.txt").unwrap();
    let first = vfs.read(&handle, 0, 5).unwrap();
    assert_eq!(String::from_utf8(first).unwrap(), "alpha");

    let cached = opened_file_cached_plaintext(&handle).unwrap();
    assert_eq!(String::from_utf8(cached).unwrap(), "alpha");

    let objects_dir = repo_root.join(".e2v").join("objects");
    for entry in fs::read_dir(&objects_dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) == Some("json") {
            fs::write(path, b"corrupt").unwrap();
        }
    }

    let second = vfs.read(&handle, 0, 5).unwrap();
    assert_eq!(String::from_utf8(second).unwrap(), "alpha");
}

#[test]
fn repeated_range_reads_can_use_encrypted_disk_cache_after_local_objects_become_unreadable() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let cache_dir = temp.path().join("encrypted-cache");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let first_snapshot_id = commit_message(&repo_root, "first", "alpha");
    let vfs = ReadOnlyVfs::mount_snapshot(
        VfsMountConfig::snapshot(repo_root.clone(), first_snapshot_id)
            .with_encrypted_disk_cache_dir(cache_dir.clone()),
    )
    .unwrap();

    let handle = vfs.open_file("tracked.txt").unwrap();
    let first = vfs.read(&handle, 1, 3).unwrap();
    assert_eq!(String::from_utf8(first).unwrap(), "lph");

    let cache_entries = fs::read_dir(&cache_dir).unwrap().count();
    assert!(
        cache_entries > 0,
        "expected encrypted cache files in {cache_dir:?}"
    );

    let read_service = RepositoryFacade::new().read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(handle.snapshot_id()).unwrap();
    let file = read_service.open_file(&snapshot, "tracked.txt").unwrap();
    for chunk_id in file.debug_chunk_ids() {
        let chunk_path = repo_root
            .join(".e2v")
            .join("objects")
            .join(format!("{chunk_id}.json"));
        let mut bytes = fs::read(&chunk_path).unwrap();
        let last_index = bytes.len() - 1;
        bytes[last_index] ^= 0x01;
        fs::write(chunk_path, bytes).unwrap();
    }

    let reopened = vfs.open_file("tracked.txt").unwrap();
    let second = vfs.read(&reopened, 1, 3).unwrap();
    assert_eq!(String::from_utf8(second).unwrap(), "lph");
}

#[test]
fn encrypted_disk_cache_hits_are_promoted_into_open_file_memory_cache() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let cache_dir = temp.path().join("encrypted-cache");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let snapshot_id = commit_message(&repo_root, "first", "alpha");
    let vfs = ReadOnlyVfs::mount_snapshot(
        VfsMountConfig::snapshot(repo_root.clone(), snapshot_id)
            .with_encrypted_disk_cache_dir(cache_dir.clone()),
    )
    .unwrap();

    let warm_handle = vfs.open_file("tracked.txt").unwrap();
    let first = vfs.read(&warm_handle, 1, 3).unwrap();
    assert_eq!(String::from_utf8(first).unwrap(), "lph");

    let read_service = RepositoryFacade::new().read_service(&repo_root).unwrap();
    let snapshot = read_service
        .open_snapshot(warm_handle.snapshot_id())
        .unwrap();
    let file = read_service.open_file(&snapshot, "tracked.txt").unwrap();
    for chunk_id in file.debug_chunk_ids() {
        let chunk_path = repo_root
            .join(".e2v")
            .join("objects")
            .join(format!("{chunk_id}.json"));
        let mut bytes = fs::read(&chunk_path).unwrap();
        let last_index = bytes.len() - 1;
        bytes[last_index] ^= 0x01;
        fs::write(chunk_path, bytes).unwrap();
    }

    let reopened = vfs.open_file("tracked.txt").unwrap();
    let second = vfs.read(&reopened, 1, 3).unwrap();
    assert_eq!(String::from_utf8(second).unwrap(), "lph");
    assert_eq!(
        String::from_utf8(opened_file_cached_plaintext(&reopened).unwrap()).unwrap(),
        "lph"
    );

    fs::remove_dir_all(&cache_dir).unwrap();

    let third = vfs.read(&reopened, 1, 3).unwrap();
    assert_eq!(String::from_utf8(third).unwrap(), "lph");
}

#[test]
fn encrypted_disk_cache_remains_readable_after_repo_moves_to_a_new_path() {
    let temp = tempdir().unwrap();
    let original_repo_root = temp.path().join("repo-original");
    let moved_repo_root = temp.path().join("repo-moved");
    let cache_dir = temp.path().join("encrypted-cache");
    fs::create_dir_all(&original_repo_root).unwrap();
    init_repo(&original_repo_root);

    let snapshot_id = commit_message(&original_repo_root, "first", "alpha");
    {
        let vfs = ReadOnlyVfs::mount_snapshot(
            VfsMountConfig::snapshot(original_repo_root.clone(), snapshot_id.clone())
                .with_encrypted_disk_cache_dir(cache_dir.clone()),
        )
        .unwrap();
        let handle = vfs.open_file("tracked.txt").unwrap();
        let first = vfs.read(&handle, 1, 3).unwrap();
        assert_eq!(String::from_utf8(first).unwrap(), "lph");
    }

    fs::rename(&original_repo_root, &moved_repo_root).unwrap();

    let read_service = RepositoryFacade::new()
        .read_service(&moved_repo_root)
        .unwrap();
    let snapshot = read_service.open_snapshot(&snapshot_id).unwrap();
    let file = read_service.open_file(&snapshot, "tracked.txt").unwrap();
    for chunk_id in file.debug_chunk_ids() {
        let chunk_path = moved_repo_root
            .join(".e2v")
            .join("objects")
            .join(format!("{chunk_id}.json"));
        let mut bytes = fs::read(&chunk_path).unwrap();
        let last_index = bytes.len() - 1;
        bytes[last_index] ^= 0x01;
        fs::write(chunk_path, bytes).unwrap();
    }

    let moved_vfs = ReadOnlyVfs::mount_snapshot(
        VfsMountConfig::snapshot(moved_repo_root, snapshot_id)
            .with_encrypted_disk_cache_dir(cache_dir),
    )
    .unwrap();
    let reopened = moved_vfs.open_file("tracked.txt").unwrap();
    let bytes = moved_vfs.read(&reopened, 1, 3).unwrap();
    assert_eq!(String::from_utf8(bytes).unwrap(), "lph");
}

#[test]
fn oversized_full_file_reads_are_not_retained_in_plaintext_memory_cache() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let large_bytes = vec![b'a'; 2 * 1024 * 1024];
    fs::write(repo_root.join("large.bin"), &large_bytes).unwrap();

    let snapshot_id = RepositoryFacade::new()
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "large".to_string(),
        })
        .unwrap()
        .snapshot_id;
    let vfs = ReadOnlyVfs::mount_snapshot(
        VfsMountConfig::snapshot(repo_root.clone(), snapshot_id)
            .with_plaintext_memory_cache_budget_bytes(1024 * 1024),
    )
    .unwrap();

    let handle = vfs.open_file("large.bin").unwrap();
    let first = vfs.read(&handle, 0, large_bytes.len()).unwrap();
    assert_eq!(first, large_bytes);
    assert!(
        opened_file_cached_plaintext(&handle).is_none(),
        "oversized full-file read should not remain in plaintext handle cache"
    );

    let read_service = RepositoryFacade::new().read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(handle.snapshot_id()).unwrap();
    let file = read_service.open_file(&snapshot, "large.bin").unwrap();
    for chunk_id in file.debug_chunk_ids() {
        let chunk_path = repo_root
            .join(".e2v")
            .join("objects")
            .join(format!("{chunk_id}.json"));
        let mut bytes = fs::read(&chunk_path).unwrap();
        let last_index = bytes.len() - 1;
        bytes[last_index] ^= 0x01;
        fs::write(chunk_path, bytes).unwrap();
    }

    let reopened = vfs.open_file("large.bin").unwrap();
    let error = vfs.read(&reopened, 0, 64 * 1024).unwrap_err();
    assert!(
        error.to_string().contains("authentication failed"),
        "expected oversized read to miss plaintext cache and surface object corruption, got: {error:#}"
    );
}

#[test]
fn cached_full_file_reads_still_reject_out_of_bounds_offsets() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let snapshot_id = commit_message(&repo_root, "first", "alpha");
    let vfs =
        ReadOnlyVfs::mount_snapshot(VfsMountConfig::snapshot(repo_root, snapshot_id)).unwrap();

    let handle = vfs.open_file("tracked.txt").unwrap();
    let warm = vfs.read(&handle, 0, 32).unwrap();
    assert_eq!(String::from_utf8(warm).unwrap(), "alpha");

    let error = vfs.read(&handle, 6, 1).unwrap_err();
    assert!(
        error.to_string().contains("range offset out of bounds"),
        "expected cached read to preserve out-of-bounds error semantics, got: {error:#}"
    );
}

#[test]
fn oversized_first_read_that_reaches_eof_populates_cache_without_a_second_origin_read() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let snapshot_id = commit_message(&repo_root, "first", "alpha");
    let vfs = ReadOnlyVfs::mount_snapshot(VfsMountConfig::snapshot(repo_root.clone(), snapshot_id))
        .unwrap();

    let handle = vfs.open_file("tracked.txt").unwrap();
    let first = vfs.read(&handle, 0, 32).unwrap();
    assert_eq!(String::from_utf8(first).unwrap(), "alpha");
    assert_eq!(
        String::from_utf8(opened_file_cached_plaintext(&handle).unwrap()).unwrap(),
        "alpha"
    );

    let read_service = RepositoryFacade::new().read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(handle.snapshot_id()).unwrap();
    let file = read_service.open_file(&snapshot, "tracked.txt").unwrap();
    for chunk_id in file.debug_chunk_ids() {
        let chunk_path = repo_root
            .join(".e2v")
            .join("objects")
            .join(format!("{chunk_id}.json"));
        let mut bytes = fs::read(&chunk_path).unwrap();
        let last_index = bytes.len() - 1;
        bytes[last_index] ^= 0x01;
        fs::write(chunk_path, bytes).unwrap();
    }

    let cached = vfs.read(&handle, 0, 32).unwrap();
    assert_eq!(String::from_utf8(cached).unwrap(), "alpha");
}

#[test]
fn plaintext_memory_cache_evicts_older_entries_when_budget_is_exceeded() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let first_bytes = vec![b'a'; 700 * 1024];
    let second_bytes = vec![b'b'; 700 * 1024];
    fs::write(repo_root.join("first.bin"), &first_bytes).unwrap();
    fs::write(repo_root.join("second.bin"), &second_bytes).unwrap();

    let snapshot_id = RepositoryFacade::new()
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap()
        .snapshot_id;
    let vfs = ReadOnlyVfs::mount_snapshot(
        VfsMountConfig::snapshot(repo_root.clone(), snapshot_id)
            .with_plaintext_memory_cache_budget_bytes(1024 * 1024),
    )
    .unwrap();

    let first_handle = vfs.open_file("first.bin").unwrap();
    let first = vfs.read(&first_handle, 0, first_bytes.len()).unwrap();
    assert_eq!(first, first_bytes);

    let second_handle = vfs.open_file("second.bin").unwrap();
    let second = vfs.read(&second_handle, 0, second_bytes.len()).unwrap();
    assert_eq!(second, second_bytes);

    let read_service = RepositoryFacade::new().read_service(&repo_root).unwrap();
    let snapshot = read_service
        .open_snapshot(first_handle.snapshot_id())
        .unwrap();
    for (path, expected_name) in [("first.bin", b'a'), ("second.bin", b'b')] {
        let file = read_service.open_file(&snapshot, path).unwrap();
        for chunk_id in file.debug_chunk_ids() {
            let chunk_path = repo_root
                .join(".e2v")
                .join("objects")
                .join(format!("{chunk_id}.json"));
            let mut bytes = fs::read(&chunk_path).unwrap();
            let last_index = bytes.len() - 1;
            bytes[last_index] ^= 0x01;
            fs::write(chunk_path, bytes).unwrap();
        }
        assert!(expected_name == b'a' || expected_name == b'b');
    }

    let reopened_first = vfs.open_file("first.bin").unwrap();
    let first_error = vfs.read(&reopened_first, 0, 64 * 1024).unwrap_err();
    assert!(
        first_error.to_string().contains("authentication failed"),
        "expected first cached file to be evicted once budget is exceeded, got: {first_error:#}"
    );

    let reopened_second = vfs.open_file("second.bin").unwrap();
    let second_bytes_after = vfs.read(&reopened_second, 0, second_bytes.len()).unwrap();
    assert_eq!(second_bytes_after, second_bytes);
}

#[test]
fn global_plaintext_cache_hits_only_promote_the_requested_slice_into_handle_cache() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let snapshot_id = commit_message(&repo_root, "first", "alpha");
    let vfs = ReadOnlyVfs::mount_snapshot(VfsMountConfig::snapshot(repo_root.clone(), snapshot_id))
        .unwrap();

    let warm_handle = vfs.open_file("tracked.txt").unwrap();
    let first = vfs.read(&warm_handle, 0, 5).unwrap();
    assert_eq!(String::from_utf8(first).unwrap(), "alpha");

    let read_service = RepositoryFacade::new().read_service(&repo_root).unwrap();
    let snapshot = read_service
        .open_snapshot(warm_handle.snapshot_id())
        .unwrap();
    let file = read_service.open_file(&snapshot, "tracked.txt").unwrap();
    for chunk_id in file.debug_chunk_ids() {
        let chunk_path = repo_root
            .join(".e2v")
            .join("objects")
            .join(format!("{chunk_id}.json"));
        let mut bytes = fs::read(&chunk_path).unwrap();
        let last_index = bytes.len() - 1;
        bytes[last_index] ^= 0x01;
        fs::write(chunk_path, bytes).unwrap();
    }

    let reopened = vfs.open_file("tracked.txt").unwrap();
    let second = vfs.read(&reopened, 1, 3).unwrap();
    assert_eq!(String::from_utf8(second).unwrap(), "lph");
    assert_eq!(
        String::from_utf8(opened_file_cached_plaintext(&reopened).unwrap()).unwrap(),
        "lph"
    );
}

#[test]
fn encrypted_disk_cache_full_file_entry_can_serve_later_subrange_reads() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let cache_dir = temp.path().join("encrypted-cache");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let snapshot_id = commit_message(&repo_root, "first", "alpha");
    let vfs = ReadOnlyVfs::mount_snapshot(
        VfsMountConfig::snapshot(repo_root.clone(), snapshot_id)
            .with_encrypted_disk_cache_dir(cache_dir.clone())
            .with_plaintext_memory_cache_budget_bytes(0),
    )
    .unwrap();

    let warm_handle = vfs.open_file("tracked.txt").unwrap();
    let first = vfs.read(&warm_handle, 0, 5).unwrap();
    assert_eq!(String::from_utf8(first).unwrap(), "alpha");
    assert!(opened_file_cached_plaintext(&warm_handle).is_none());

    let read_service = RepositoryFacade::new().read_service(&repo_root).unwrap();
    let snapshot = read_service
        .open_snapshot(warm_handle.snapshot_id())
        .unwrap();
    let file = read_service.open_file(&snapshot, "tracked.txt").unwrap();
    for chunk_id in file.debug_chunk_ids() {
        let chunk_path = repo_root
            .join(".e2v")
            .join("objects")
            .join(format!("{chunk_id}.json"));
        let mut bytes = fs::read(&chunk_path).unwrap();
        let last_index = bytes.len() - 1;
        bytes[last_index] ^= 0x01;
        fs::write(chunk_path, bytes).unwrap();
    }

    let reopened = vfs.open_file("tracked.txt").unwrap();
    let second = vfs.read(&reopened, 1, 3).unwrap();
    assert_eq!(String::from_utf8(second).unwrap(), "lph");
}

#[test]
fn encrypted_disk_cache_file_names_do_not_expose_requested_offsets() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let cache_dir = temp.path().join("encrypted-cache");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let snapshot_id = commit_message(&repo_root, "first", "alpha");
    let vfs = ReadOnlyVfs::mount_snapshot(
        VfsMountConfig::snapshot(repo_root, snapshot_id)
            .with_encrypted_disk_cache_dir(cache_dir.clone())
            .with_plaintext_memory_cache_budget_bytes(0),
    )
    .unwrap();

    let handle = vfs.open_file("tracked.txt").unwrap();
    let bytes = vfs.read(&handle, 1, 3).unwrap();
    assert_eq!(String::from_utf8(bytes).unwrap(), "lph");

    let cache_file_names = fs::read_dir(&cache_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
        .collect::<Vec<_>>();
    assert!(
        cache_file_names.iter().all(|name| !name.contains("-1-3")),
        "encrypted cache file names should not expose requested offsets/lengths: {cache_file_names:?}"
    );
}

#[test]
fn encrypted_disk_cache_range_indexes_do_not_store_plaintext_offsets() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let cache_dir = temp.path().join("encrypted-cache");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let snapshot_id = commit_message(&repo_root, "first", "alpha");
    let vfs = ReadOnlyVfs::mount_snapshot(
        VfsMountConfig::snapshot(repo_root, snapshot_id)
            .with_encrypted_disk_cache_dir(cache_dir.clone())
            .with_plaintext_memory_cache_budget_bytes(0),
    )
    .unwrap();

    let handle = vfs.open_file("tracked.txt").unwrap();
    let bytes = vfs.read(&handle, 1, 3).unwrap();
    assert_eq!(String::from_utf8(bytes).unwrap(), "lph");

    let range_index_path = fs::read_dir(&cache_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().and_then(|value| value.to_str()) == Some("ranges"))
        .expect("expected encrypted cache range index");
    let stored = fs::read(&range_index_path).unwrap();

    let mut expected_plaintext = Vec::new();
    expected_plaintext.extend_from_slice(&1u64.to_le_bytes());
    expected_plaintext.extend_from_slice(&1u64.to_le_bytes());
    expected_plaintext.extend_from_slice(&3u64.to_le_bytes());

    assert_ne!(
        stored, expected_plaintext,
        "range index should not store plaintext offset/length tuples: {range_index_path:?}"
    );
    assert!(
        stored.len() > expected_plaintext.len(),
        "encrypted range index should include confidentiality overhead"
    );
}

#[test]
fn encrypted_disk_cache_retains_multiple_private_ranges_for_one_file() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let cache_dir = temp.path().join("encrypted-cache");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let snapshot_id = commit_message(&repo_root, "first", "alpha");
    let vfs = ReadOnlyVfs::mount_snapshot(
        VfsMountConfig::snapshot(repo_root.clone(), snapshot_id)
            .with_encrypted_disk_cache_dir(cache_dir)
            .with_plaintext_memory_cache_budget_bytes(0),
    )
    .unwrap();

    let handle = vfs.open_file("tracked.txt").unwrap();
    let first = vfs.read(&handle, 1, 3).unwrap();
    assert_eq!(String::from_utf8(first).unwrap(), "lph");
    let second = vfs.read(&handle, 0, 1).unwrap();
    assert_eq!(String::from_utf8(second).unwrap(), "a");

    let read_service = RepositoryFacade::new().read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(handle.snapshot_id()).unwrap();
    let file = read_service.open_file(&snapshot, "tracked.txt").unwrap();
    for chunk_id in file.debug_chunk_ids() {
        let chunk_path = repo_root
            .join(".e2v")
            .join("objects")
            .join(format!("{chunk_id}.json"));
        let mut bytes = fs::read(&chunk_path).unwrap();
        let last_index = bytes.len() - 1;
        bytes[last_index] ^= 0x01;
        fs::write(chunk_path, bytes).unwrap();
    }

    let reopened = vfs.open_file("tracked.txt").unwrap();
    let reread = vfs.read(&reopened, 1, 3).unwrap();
    assert_eq!(String::from_utf8(reread).unwrap(), "lph");
}

#[test]
fn encrypted_disk_cache_ignores_unrelated_subdirectories_when_cold() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let cache_dir = temp.path().join("encrypted-cache");
    fs::create_dir_all(&repo_root).unwrap();
    fs::create_dir_all(cache_dir.join("scratch")).unwrap();
    init_repo(&repo_root);

    let snapshot_id = commit_message(&repo_root, "first", "alpha");
    let vfs = ReadOnlyVfs::mount_snapshot(
        VfsMountConfig::snapshot(repo_root, snapshot_id)
            .with_encrypted_disk_cache_dir(cache_dir)
            .with_plaintext_memory_cache_budget_bytes(0),
    )
    .unwrap();

    let handle = vfs.open_file("tracked.txt").unwrap();
    let bytes = vfs.read(&handle, 1, 3).unwrap();

    assert_eq!(String::from_utf8(bytes).unwrap(), "lph");
}

#[test]
fn encrypted_disk_cache_ignores_unrelated_unreadable_files_when_cold() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let cache_dir = temp.path().join("encrypted-cache");
    fs::create_dir_all(&repo_root).unwrap();
    fs::create_dir_all(&cache_dir).unwrap();
    let _guard = make_unreadable_cache_entry(&cache_dir.join("foreign.bin"));
    init_repo(&repo_root);

    let snapshot_id = commit_message(&repo_root, "first", "alpha");
    let vfs = ReadOnlyVfs::mount_snapshot(
        VfsMountConfig::snapshot(repo_root, snapshot_id)
            .with_encrypted_disk_cache_dir(cache_dir)
            .with_plaintext_memory_cache_budget_bytes(0),
    )
    .unwrap();

    let handle = vfs.open_file("tracked.txt").unwrap();
    let bytes = vfs.read(&handle, 1, 3).unwrap();

    assert_eq!(String::from_utf8(bytes).unwrap(), "lph");
}

#[test]
fn encrypted_disk_cache_path_conflicts_do_not_break_repository_reads() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let cache_dir = temp.path().join("encrypted-cache");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let snapshot_id = commit_message(&repo_root, "first", "alpha");
    {
        let vfs = ReadOnlyVfs::mount_snapshot(
            VfsMountConfig::snapshot(repo_root.clone(), snapshot_id.clone())
                .with_encrypted_disk_cache_dir(cache_dir.clone())
                .with_plaintext_memory_cache_budget_bytes(0),
        )
        .unwrap();
        let handle = vfs.open_file("tracked.txt").unwrap();
        let first = vfs.read(&handle, 1, 3).unwrap();
        assert_eq!(String::from_utf8(first).unwrap(), "lph");
    }

    let cache_entry = fs::read_dir(&cache_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::remove_file(&cache_entry).unwrap();
    fs::create_dir(&cache_entry).unwrap();

    let reopened_vfs = ReadOnlyVfs::mount_snapshot(
        VfsMountConfig::snapshot(repo_root, snapshot_id)
            .with_encrypted_disk_cache_dir(cache_dir)
            .with_plaintext_memory_cache_budget_bytes(0),
    )
    .unwrap();
    let reopened = reopened_vfs.open_file("tracked.txt").unwrap();
    let reread = reopened_vfs.read(&reopened, 1, 3).unwrap();

    assert_eq!(String::from_utf8(reread).unwrap(), "lph");
}

#[test]
fn encrypted_disk_cache_prunes_stale_range_index_entries_after_cover_hits() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let cache_dir = temp.path().join("encrypted-cache");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let snapshot_id = commit_message(&repo_root, "first", "alpha");
    let vfs = ReadOnlyVfs::mount_snapshot(
        VfsMountConfig::snapshot(repo_root.clone(), snapshot_id)
            .with_encrypted_disk_cache_dir(cache_dir.clone())
            .with_plaintext_memory_cache_budget_bytes(0),
    )
    .unwrap();

    let handle = vfs.open_file("tracked.txt").unwrap();
    let first = vfs.read(&handle, 1, 3).unwrap();
    assert_eq!(String::from_utf8(first).unwrap(), "lph");
    let stale_range_path = fs::read_dir(&cache_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().and_then(|value| value.to_str()) == Some("bin"))
        .expect("expected first cached range file");

    let full = vfs.read(&handle, 0, 5).unwrap();
    assert_eq!(String::from_utf8(full).unwrap(), "alpha");

    let range_index_path = fs::read_dir(&cache_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().and_then(|value| value.to_str()) == Some("ranges"))
        .expect("expected encrypted cache range index");
    let before_len = fs::metadata(&range_index_path).unwrap().len();

    fs::remove_file(&stale_range_path).unwrap();

    let read_service = RepositoryFacade::new().read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(handle.snapshot_id()).unwrap();
    let file = read_service.open_file(&snapshot, "tracked.txt").unwrap();
    for chunk_id in file.debug_chunk_ids() {
        let chunk_path = repo_root
            .join(".e2v")
            .join("objects")
            .join(format!("{chunk_id}.json"));
        let mut bytes = fs::read(&chunk_path).unwrap();
        let last_index = bytes.len() - 1;
        bytes[last_index] ^= 0x01;
        fs::write(chunk_path, bytes).unwrap();
    }

    let reopened = vfs.open_file("tracked.txt").unwrap();
    let reread = vfs.read(&reopened, 1, 3).unwrap();
    assert_eq!(String::from_utf8(reread).unwrap(), "lph");

    let after_len = fs::metadata(&range_index_path).unwrap().len();
    assert!(
        after_len < before_len,
        "expected stale range index entry to be pruned after cover hit: before={before_len} after={after_len}"
    );
}

#[test]
fn encrypted_disk_cache_deletes_corrupted_range_files_after_cover_hits() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let cache_dir = temp.path().join("encrypted-cache");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let snapshot_id = commit_message(&repo_root, "first", "alpha");
    let vfs = ReadOnlyVfs::mount_snapshot(
        VfsMountConfig::snapshot(repo_root.clone(), snapshot_id)
            .with_encrypted_disk_cache_dir(cache_dir.clone())
            .with_plaintext_memory_cache_budget_bytes(0),
    )
    .unwrap();

    let handle = vfs.open_file("tracked.txt").unwrap();
    let first = vfs.read(&handle, 1, 3).unwrap();
    assert_eq!(String::from_utf8(first).unwrap(), "lph");
    let stale_range_path = fs::read_dir(&cache_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().and_then(|value| value.to_str()) == Some("bin"))
        .expect("expected first cached range file");

    let full = vfs.read(&handle, 0, 5).unwrap();
    assert_eq!(String::from_utf8(full).unwrap(), "alpha");

    let mut stale_bytes = fs::read(&stale_range_path).unwrap();
    stale_bytes[0] ^= 0x01;
    fs::write(&stale_range_path, stale_bytes).unwrap();

    let read_service = RepositoryFacade::new().read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(handle.snapshot_id()).unwrap();
    let file = read_service.open_file(&snapshot, "tracked.txt").unwrap();
    for chunk_id in file.debug_chunk_ids() {
        let chunk_path = repo_root
            .join(".e2v")
            .join("objects")
            .join(format!("{chunk_id}.json"));
        let mut bytes = fs::read(&chunk_path).unwrap();
        let last_index = bytes.len() - 1;
        bytes[last_index] ^= 0x01;
        fs::write(chunk_path, bytes).unwrap();
    }

    let reopened = vfs.open_file("tracked.txt").unwrap();
    let reread = vfs.read(&reopened, 1, 3).unwrap();
    assert_eq!(String::from_utf8(reread).unwrap(), "lph");
    assert!(
        !stale_range_path.exists(),
        "expected corrupted stale cache entry to be deleted after cover hit: {stale_range_path:?}"
    );
}

#[test]
fn prefix_reads_do_not_require_unrelated_later_chunks_to_authenticate() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let mut content = Vec::with_capacity(16 * 1024 * 1024);
    for index in 0..(16 * 1024 * 1024) {
        content.push((index % 251) as u8);
    }
    fs::write(repo_root.join("large.bin"), &content).unwrap();

    let snapshot_id = RepositoryFacade::new()
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "large".to_string(),
        })
        .unwrap()
        .snapshot_id;
    let vfs = ReadOnlyVfs::mount_snapshot(VfsMountConfig::snapshot(repo_root.clone(), snapshot_id))
        .unwrap();
    let handle = vfs.open_file("large.bin").unwrap();

    let read_service = RepositoryFacade::new().read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(handle.snapshot_id()).unwrap();
    let file = read_service.open_file(&snapshot, "large.bin").unwrap();
    let later_chunk_id = file.debug_chunk_ids().last().unwrap().clone();
    let later_chunk_path = repo_root
        .join(".e2v")
        .join("objects")
        .join(format!("{later_chunk_id}.json"));
    let mut bytes = fs::read(&later_chunk_path).unwrap();
    let last_index = bytes.len() - 1;
    bytes[last_index] ^= 0x01;
    fs::write(later_chunk_path, bytes).unwrap();

    let prefix = vfs.read(&handle, 0, 64 * 1024).unwrap();
    assert_eq!(prefix, content[..64 * 1024].to_vec());
}

#[test]
fn snapshot_pinned_read_dir_keeps_original_namespace_after_branch_head_moves() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    fs::create_dir_all(repo_root.join("nested")).unwrap();
    fs::write(repo_root.join("nested").join("base.txt"), "alpha").unwrap();
    let first_snapshot_id = RepositoryFacade::new()
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "first".to_string(),
        })
        .unwrap()
        .snapshot_id;

    let vfs = ReadOnlyVfs::mount_snapshot(VfsMountConfig::snapshot(
        repo_root.clone(),
        first_snapshot_id,
    ))
    .unwrap();

    fs::write(repo_root.join("nested").join("second.txt"), "beta").unwrap();
    RepositoryFacade::new()
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "second".to_string(),
        })
        .unwrap();

    let entries = vfs.read_dir("nested").unwrap();
    let names = entries
        .into_iter()
        .map(|entry| entry.name)
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["base.txt".to_string()]);
}

#[test]
fn snapshot_vfs_can_stat_files_and_directories() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    fs::create_dir_all(repo_root.join("nested")).unwrap();
    fs::write(repo_root.join("nested").join("base.txt"), "alpha").unwrap();
    fs::write(repo_root.join("tracked.txt"), "root-file").unwrap();
    let snapshot_id = RepositoryFacade::new()
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap()
        .snapshot_id;

    let vfs =
        ReadOnlyVfs::mount_snapshot(VfsMountConfig::snapshot(repo_root, snapshot_id)).unwrap();

    let root = vfs.stat_path("").unwrap();
    assert_eq!(root.kind, VfsNodeKind::Directory);
    assert_eq!(root.logical_path, "");
    assert_eq!(root.size_bytes, 0);
    assert!(root.inode_id > 0);

    let nested = vfs.stat_path("nested").unwrap();
    assert_eq!(nested.kind, VfsNodeKind::Directory);
    assert_eq!(nested.logical_path, "nested");
    assert_eq!(nested.size_bytes, 0);
    assert!(nested.inode_id > 0);
    assert_ne!(root.inode_id, nested.inode_id);

    let tracked = vfs.stat_path("tracked.txt").unwrap();
    assert_eq!(tracked.kind, VfsNodeKind::File);
    assert_eq!(tracked.logical_path, "tracked.txt");
    assert_eq!(tracked.size_bytes, "root-file".len() as u64);
    assert!(tracked.inode_id > 0);
    assert_eq!(tracked.snapshot_id, vfs.namespace_snapshot_id());
    assert!(tracked.layout_generation > 0);
}

#[test]
fn opened_file_test_probe_is_not_exposed_as_a_public_api_method() {
    let source = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("lib.rs"),
    )
    .unwrap();

    assert!(
        !source.contains("pub fn cached_plaintext_for_test"),
        "test-only cache probes should not remain in the public VFS API surface"
    );
}

#[test]
fn winfsp_test_probes_are_not_exposed_as_public_api_methods() {
    let source = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("windows.rs"),
    )
    .unwrap();

    for signature in [
        "pub fn from_install_root_for_test",
        "pub fn from_candidate_roots_for_test",
        "pub fn get_symbol_address_for_test",
        "pub fn resolve_mount_exports_for_test",
        "pub fn new_for_test",
        "pub fn is_mounted_for_test",
        "pub fn build_native_create_request_for_test",
        "pub fn create_filesystem_handle_for_test",
        "pub fn has_native_filesystem_handle_for_test",
        "pub fn destroy_filesystem_handle_for_test",
        "pub fn run_mount_lifecycle_for_test",
    ] {
        assert!(
            !source.contains(signature),
            "test-only WinFSP probe should not remain public: {signature}"
        );
    }
}

#[cfg(windows)]
#[test]
fn winfsp_security_callbacks_reuse_a_single_cached_descriptor_parse() {
    let source = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("windows.rs"),
    )
    .unwrap();

    let parse_count = source
        .matches("SecurityDescriptorBytes::from_sddl(")
        .count();
    assert_eq!(
        parse_count, 1,
        "WinFSP host should parse the read-only security descriptor once and reuse cached bytes, found {parse_count} parses"
    );
}

#[cfg(windows)]
#[test]
fn winfsp_directory_callbacks_do_not_reload_the_runtime_library() {
    let source = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("windows.rs"),
    )
    .unwrap();

    let load_count = source.matches("WinfspRuntimeLibrary::load(").count();
    assert_eq!(
        load_count, 1,
        "WinFSP host should load the runtime once during mount startup and reuse cached callback helpers, found {load_count} runtime loads"
    );
}

#[test]
fn vfs_root_does_not_reexport_winfsp_host_internals() {
    let source = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("lib.rs"),
    )
    .unwrap();

    if let Some(start) = source.find("pub use windows::{") {
        let export_block = &source[start..]
            .split_once("};")
            .map(|(block, _)| block)
            .unwrap_or(&source[start..]);
        for symbol in [
            "ReadOnlyVolumeSummary",
            "WindowsMountLauncher",
            "WinfspHostConfig",
            "WinfspHostDriver",
            "WinfspHostLauncher",
            "WinfspHostSession",
            "WinfspInvalidationPlan",
            "WinfspInvalidator",
            "WinfspMountContext",
            "WinfspOpenHandle",
            "WinfspOpenRequest",
            "WinfspRuntimeLibrary",
            "WinfspRuntimePaths",
            "WinfspVolumeParams",
        ] {
            assert!(
                !export_block.contains(symbol),
                "crate root should not publicly re-export WinFSP host internals: {symbol}"
            );
        }
    }
}

#[test]
fn vfs_root_does_not_reexport_platform_adapter_test_seams() {
    let source = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("lib.rs"),
    )
    .unwrap();

    if let Some(start) = source.find("pub use platform::{") {
        let export_block = &source[start..]
            .split_once("};")
            .map(|(block, _)| block)
            .unwrap_or(&source[start..]);
        for symbol in [
            "PlatformFamily",
            "PlatformMountAdapter",
            "LinuxMountAdapter",
            "MacosMountAdapter",
            "WindowsMountAdapter",
        ] {
            assert!(
                !export_block.contains(symbol),
                "crate root should not publicly re-export platform adapter test seams: {symbol}"
            );
        }
    }
}

#[test]
fn vfs_root_does_not_expose_host_launcher_test_trait() {
    let source = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("lib.rs"),
    )
    .unwrap();
    let public_surface = source.split("#[doc(hidden)]").next().unwrap_or(&source);

    assert!(
        !public_surface.contains("pub trait VfsHostLauncher"),
        "crate root should not expose the test-only VfsHostLauncher trait"
    );
}

#[test]
fn snapshot_vfs_accepts_rooted_and_trailing_slash_paths() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    fs::create_dir_all(repo_root.join("nested")).unwrap();
    fs::write(repo_root.join("nested").join("base.txt"), "alpha").unwrap();
    fs::write(repo_root.join("tracked.txt"), "root-file").unwrap();
    let snapshot_id = RepositoryFacade::new()
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap()
        .snapshot_id;

    let vfs =
        ReadOnlyVfs::mount_snapshot(VfsMountConfig::snapshot(repo_root, snapshot_id)).unwrap();

    let rooted = vfs.open_file("/tracked.txt").unwrap();
    let bytes = vfs.read(&rooted, 0, 32).unwrap();
    assert_eq!(String::from_utf8(bytes).unwrap(), "root-file");

    let nested = vfs.read_dir("/nested/").unwrap();
    assert_eq!(nested.len(), 1);
    assert_eq!(nested[0].name, "base.txt");

    let metadata = vfs.stat_path("/nested/").unwrap();
    assert_eq!(metadata.kind, VfsNodeKind::Directory);
    assert_eq!(metadata.logical_path, "nested");
}

#[test]
fn snapshot_vfs_normalizes_equivalent_file_paths_to_one_logical_identity() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    fs::write(repo_root.join("tracked.txt"), "root-file").unwrap();
    let snapshot_id = RepositoryFacade::new()
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap()
        .snapshot_id;

    let vfs =
        ReadOnlyVfs::mount_snapshot(VfsMountConfig::snapshot(repo_root, snapshot_id)).unwrap();

    let plain = vfs.open_file("tracked.txt").unwrap();
    let rooted = vfs.open_file("/tracked.txt").unwrap();

    assert_eq!(plain.logical_path(), "tracked.txt");
    assert_eq!(rooted.logical_path(), "tracked.txt");
    assert_eq!(plain.inode_id(), rooted.inode_id());
}

#[test]
fn snapshot_vfs_normalizes_decomposed_unicode_file_paths_to_one_logical_identity() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let decomposed = "e\u{301}.txt".to_string();
    fs::write(repo_root.join(&decomposed), "hello unicode").unwrap();
    let snapshot_id = RepositoryFacade::new()
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap()
        .snapshot_id;

    let vfs =
        ReadOnlyVfs::mount_snapshot(VfsMountConfig::snapshot(repo_root, snapshot_id)).unwrap();

    let composed = vfs.open_file("é.txt").unwrap();
    let decomposed_handle = vfs.open_file(&decomposed).unwrap();

    assert_eq!(composed.logical_path(), "é.txt");
    assert_eq!(decomposed_handle.logical_path(), "é.txt");
    assert_eq!(composed.inode_id(), decomposed_handle.inode_id());
}

#[test]
fn live_branch_refresh_updates_new_opens_without_changing_old_handles() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    commit_message(&repo_root, "first", "alpha");
    let branch_token = RepositoryFacade::new()
        .open(&repo_root)
        .unwrap()
        .branch
        .token_hex;

    let mut vfs = ReadOnlyVfs::mount_live_branch(VfsMountConfig::live_branch(
        repo_root.clone(),
        branch_token,
    ))
    .unwrap();

    let old_handle = vfs.open_file("tracked.txt").unwrap();
    commit_message(&repo_root, "second", "beta");

    let refresh = vfs.refresh_live_branch().unwrap();
    assert!(refresh.namespace_changed);
    assert!(refresh.requires_invalidation);

    let new_handle = vfs.open_file("tracked.txt").unwrap();

    assert_eq!(old_handle.snapshot_id(), vfs_before_snapshot_id(&repo_root));
    assert_ne!(old_handle.snapshot_id(), new_handle.snapshot_id());
    assert_ne!(old_handle.inode_id(), new_handle.inode_id());
    assert_eq!(old_handle.logical_path(), "tracked.txt");
    assert_eq!(new_handle.logical_path(), "tracked.txt");
    assert!(old_handle.layout_generation() > 0);
    assert_eq!(
        old_handle.layout_generation(),
        new_handle.layout_generation()
    );

    assert_eq!(
        String::from_utf8(vfs.read(&old_handle, 0, 32).unwrap()).unwrap(),
        "alpha"
    );
    assert_eq!(
        String::from_utf8(vfs.read(&new_handle, 0, 32).unwrap()).unwrap(),
        "beta"
    );
}

fn vfs_before_snapshot_id(repo_root: &std::path::Path) -> String {
    RepositoryFacade::new()
        .snapshots(repo_root)
        .unwrap()
        .get(1)
        .unwrap()
        .snapshot_id
        .clone()
}

#[test]
fn live_branch_refresh_updates_directory_entries_for_new_namespace_reads() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    fs::create_dir_all(repo_root.join("nested")).unwrap();
    fs::write(repo_root.join("nested").join("base.txt"), "alpha").unwrap();
    RepositoryFacade::new()
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "first".to_string(),
        })
        .unwrap();
    let branch_token = RepositoryFacade::new()
        .open(&repo_root)
        .unwrap()
        .branch
        .token_hex;

    let mut vfs = ReadOnlyVfs::mount_live_branch(VfsMountConfig::live_branch(
        repo_root.clone(),
        branch_token,
    ))
    .unwrap();

    let before = vfs
        .read_dir("nested")
        .unwrap()
        .into_iter()
        .map(|entry| entry.name)
        .collect::<Vec<_>>();
    assert_eq!(before, vec!["base.txt".to_string()]);

    fs::write(repo_root.join("nested").join("second.txt"), "beta").unwrap();
    RepositoryFacade::new()
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "second".to_string(),
        })
        .unwrap();

    let refresh = vfs.refresh_live_branch().unwrap();
    assert!(refresh.namespace_changed);
    assert!(refresh.requires_invalidation);

    let after = vfs
        .read_dir("nested")
        .unwrap()
        .into_iter()
        .map(|entry| entry.name)
        .collect::<Vec<_>>();
    assert_eq!(
        after,
        vec!["base.txt".to_string(), "second.txt".to_string()]
    );
}

#[test]
fn live_branch_refresh_skips_invalidation_signal_when_direct_io_policy_is_active() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    commit_message(&repo_root, "first", "alpha");
    let branch_token = RepositoryFacade::new()
        .open(&repo_root)
        .unwrap()
        .branch
        .token_hex;

    let mut vfs = ReadOnlyVfs::mount_live_branch(
        VfsMountConfig::live_branch(repo_root.clone(), branch_token)
            .with_platform_capabilities(PlatformCapabilities::no_reliable_invalidation()),
    )
    .unwrap();

    commit_message(&repo_root, "second", "beta");

    let refresh = vfs.refresh_live_branch().unwrap();
    assert!(refresh.namespace_changed);
    assert!(!refresh.requires_invalidation);
    assert_eq!(vfs.cache_policy(), CachePolicy::DirectIoFallback);
}

#[test]
fn live_branch_mount_without_reliable_invalidation_uses_direct_io_policy() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    commit_message(&repo_root, "first", "alpha");
    let branch_token = RepositoryFacade::new()
        .open(&repo_root)
        .unwrap()
        .branch
        .token_hex;

    let vfs = ReadOnlyVfs::mount_live_branch(
        VfsMountConfig::live_branch(repo_root, branch_token)
            .with_platform_capabilities(PlatformCapabilities::no_reliable_invalidation()),
    )
    .unwrap();

    assert_eq!(vfs.cache_policy(), CachePolicy::DirectIoFallback);
}

#[test]
fn live_branch_mount_without_page_cache_invalidation_uses_direct_io_policy() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    commit_message(&repo_root, "first", "alpha");
    let branch_token = RepositoryFacade::new()
        .open(&repo_root)
        .unwrap()
        .branch
        .token_hex;

    let vfs = ReadOnlyVfs::mount_live_branch(
        VfsMountConfig::live_branch(repo_root, branch_token).with_platform_capabilities(
            PlatformCapabilities::reliable_invalidation().without_page_cache_invalidation(),
        ),
    )
    .unwrap();

    assert_eq!(vfs.cache_policy(), CachePolicy::DirectIoFallback);
}

#[test]
fn live_branch_mount_without_directory_entry_invalidation_uses_direct_io_policy() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    commit_message(&repo_root, "first", "alpha");
    let branch_token = RepositoryFacade::new()
        .open(&repo_root)
        .unwrap()
        .branch
        .token_hex;

    let vfs = ReadOnlyVfs::mount_live_branch(
        VfsMountConfig::live_branch(repo_root, branch_token).with_platform_capabilities(
            PlatformCapabilities::reliable_invalidation().without_directory_entry_invalidation(),
        ),
    )
    .unwrap();

    assert_eq!(vfs.cache_policy(), CachePolicy::DirectIoFallback);
}

#[test]
fn live_branch_mount_without_inode_attribute_invalidation_uses_direct_io_policy() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    commit_message(&repo_root, "first", "alpha");
    let branch_token = RepositoryFacade::new()
        .open(&repo_root)
        .unwrap()
        .branch
        .token_hex;

    let vfs = ReadOnlyVfs::mount_live_branch(
        VfsMountConfig::live_branch(repo_root, branch_token).with_platform_capabilities(
            PlatformCapabilities::reliable_invalidation().without_inode_attribute_invalidation(),
        ),
    )
    .unwrap();

    assert_eq!(vfs.cache_policy(), CachePolicy::DirectIoFallback);
}

#[test]
fn stream_only_mount_rejects_byte_range_lock_semantics() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let first_snapshot_id = commit_message(&repo_root, "first", "alpha");
    let vfs = ReadOnlyVfs::mount_snapshot(VfsMountConfig::snapshot(repo_root, first_snapshot_id))
        .unwrap();

    let error = vfs
        .require_semantic(VfsSemantic::ByteRangeLocks)
        .unwrap_err();
    assert!(error.to_string().contains("unsupported"));
}

#[test]
fn stream_only_mount_rejects_memory_mapped_write_semantics() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let first_snapshot_id = commit_message(&repo_root, "first", "alpha");
    let vfs = ReadOnlyVfs::mount_snapshot(VfsMountConfig::snapshot(repo_root, first_snapshot_id))
        .unwrap();

    let error = vfs
        .require_semantic(VfsSemantic::MemoryMappedWrites)
        .unwrap_err();
    assert!(error.to_string().contains("memory-mapped"));
}

#[test]
fn stream_only_mount_rejects_writable_handle_semantics() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let first_snapshot_id = commit_message(&repo_root, "first", "alpha");
    let vfs = ReadOnlyVfs::mount_snapshot(VfsMountConfig::snapshot(repo_root, first_snapshot_id))
        .unwrap();

    let error = vfs
        .require_semantic(VfsSemantic::WritableHandles)
        .unwrap_err();
    assert!(error.to_string().contains("writable"));
}

#[test]
fn stream_only_mount_rejects_writeback_cache_semantics() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let first_snapshot_id = commit_message(&repo_root, "first", "alpha");
    let vfs = ReadOnlyVfs::mount_snapshot(VfsMountConfig::snapshot(repo_root, first_snapshot_id))
        .unwrap();

    let error = vfs
        .require_semantic(VfsSemantic::WritebackCaching)
        .unwrap_err();
    assert!(error.to_string().contains("writeback"));
}

#[test]
fn mount_requests_stop_at_the_platform_boundary_with_explicit_status() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let first_snapshot_id = commit_message(&repo_root, "first", "alpha");
    let summary = try_mount_snapshot_on_current_platform(
        VfsMountConfig::snapshot(repo_root, first_snapshot_id),
        PathBuf::from("X:"),
    )
    .unwrap();

    assert!(
        summary
            .status_message
            .contains("not supported on this platform yet")
            || summary
                .status_message
                .contains("windows adapter not implemented yet")
    );
}

#[cfg(windows)]
#[test]
fn current_platform_mount_uses_the_windows_adapter_boundary() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let snapshot_id = commit_message(&repo_root, "first", "alpha");
    let summary = try_mount_snapshot_on_current_platform(
        VfsMountConfig::snapshot(repo_root, snapshot_id),
        PathBuf::from("X:"),
    )
    .unwrap();

    assert!(
        summary
            .status_message
            .contains("winfsp adapter boundary ready"),
        "unexpected status: {}",
        summary.status_message
    );
}

#[test]
fn mount_request_constructors_preserve_mode_and_mount_point() {
    let request = MountRequest::snapshot(
        PathBuf::from("repo"),
        "snapshot-123".to_string(),
        PathBuf::from("X:"),
    );

    assert_eq!(request.mount_point(), &PathBuf::from("X:"));
    assert_eq!(request.mount_mode_label(), "snapshot-pinned");

    let branch_request = MountRequest::live_branch(
        PathBuf::from("repo"),
        "branch-token".to_string(),
        PathBuf::from("Y:"),
    );
    assert_eq!(branch_request.mount_point(), &PathBuf::from("Y:"));
    assert_eq!(branch_request.mount_mode_label(), "live-branch");
}

#[test]
fn mount_requests_can_be_presented_to_a_host_launcher() {
    #[derive(Default)]
    struct RecordingLauncher {
        seen_mode: Option<String>,
        seen_mount_point: Option<PathBuf>,
    }

    impl VfsHostLauncher for RecordingLauncher {
        fn launch(&mut self, request: &MountRequest) -> anyhow::Result<()> {
            self.seen_mode = Some(request.mount_mode_label().to_string());
            self.seen_mount_point = Some(request.mount_point().clone());
            Ok(())
        }
    }

    let request = MountRequest::snapshot(
        PathBuf::from("repo"),
        "snapshot-123".to_string(),
        PathBuf::from("X:"),
    );
    let mut launcher = RecordingLauncher::default();
    launcher.launch(&request).unwrap();

    assert_eq!(launcher.seen_mode.as_deref(), Some("snapshot-pinned"));
    assert_eq!(
        launcher.seen_mount_point.as_ref(),
        Some(&PathBuf::from("X:"))
    );
}

#[test]
fn windows_mount_launcher_records_mount_mode_and_mount_point() {
    let request = MountRequest::live_branch(
        PathBuf::from("repo"),
        "branch-token".to_string(),
        PathBuf::from("Z:"),
    );
    let launcher = WindowsMountLauncher::from_request(&request);

    assert_eq!(launcher.mount_mode_label(), "live-branch");
    assert_eq!(launcher.mount_point(), &PathBuf::from("Z:"));
}

#[test]
fn windows_mount_launcher_can_hand_request_and_context_to_a_winfsp_host() {
    #[derive(Default)]
    struct RecordingHost {
        seen_mode: Option<String>,
        seen_mount_point: Option<PathBuf>,
        seen_snapshot_id: Option<String>,
        seen_cache_policy: Option<CachePolicy>,
    }

    impl WinfspHostLauncher for RecordingHost {
        fn launch(
            &mut self,
            launcher: &WindowsMountLauncher,
            context: WinfspMountContext,
        ) -> anyhow::Result<()> {
            self.seen_mode = Some(launcher.mount_mode_label().to_string());
            self.seen_mount_point = Some(launcher.mount_point().clone());
            self.seen_snapshot_id = Some(context.namespace_snapshot_id());
            self.seen_cache_policy = Some(context.cache_policy());
            Ok(())
        }
    }

    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let snapshot_id = commit_message(&repo_root, "first", "alpha");
    let request = MountRequest::snapshot(repo_root, snapshot_id.clone(), PathBuf::from("W:"));
    let launcher = WindowsMountLauncher::from_request(&request);
    let context = WinfspMountContext::from_request(request);
    let mut host = RecordingHost::default();

    launcher.launch_with_host(context, &mut host).unwrap();

    assert_eq!(host.seen_mode.as_deref(), Some("snapshot-pinned"));
    assert_eq!(host.seen_mount_point.as_ref(), Some(&PathBuf::from("W:")));
    assert_eq!(host.seen_snapshot_id.as_deref(), Some(snapshot_id.as_str()));
    assert_eq!(
        host.seen_cache_policy,
        Some(CachePolicy::KernelCacheWithInvalidation)
    );
}

#[test]
fn windows_mount_launcher_can_launch_through_host_and_return_a_summary() {
    #[derive(Default)]
    struct RecordingHost {
        launches: usize,
        seen_snapshot_id: Option<String>,
    }

    impl WinfspHostLauncher for RecordingHost {
        fn launch(
            &mut self,
            _launcher: &WindowsMountLauncher,
            context: WinfspMountContext,
        ) -> anyhow::Result<()> {
            self.launches += 1;
            self.seen_snapshot_id = Some(context.namespace_snapshot_id());
            Ok(())
        }
    }

    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let snapshot_id = commit_message(&repo_root, "first", "alpha");
    let request = MountRequest::snapshot(repo_root, snapshot_id.clone(), PathBuf::from("W:"));
    let launcher = WindowsMountLauncher::from_request(&request);
    let context = WinfspMountContext::from_request(request);
    let mut host = RecordingHost::default();

    let summary = launcher
        .launch_with_host_and_describe(context, &mut host)
        .unwrap();

    assert_eq!(host.launches, 1);
    assert_eq!(host.seen_snapshot_id.as_deref(), Some(snapshot_id.as_str()));
    assert_eq!(summary.mount_mode, "snapshot-pinned");
    assert_eq!(summary.mount_point, PathBuf::from("W:"));
    assert_eq!(
        summary.cache_policy,
        CachePolicy::KernelCacheWithInvalidation
    );
    assert!(
        summary
            .status_message
            .contains("winfsp adapter boundary ready")
    );
}

#[test]
fn winfsp_host_config_reflects_a_snapshot_read_only_volume() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let snapshot_id = commit_message(&repo_root, "first", "alpha");
    let request = MountRequest::snapshot(repo_root, snapshot_id, PathBuf::from("W:"));
    let context = WinfspMountContext::from_request(request);

    let config = WinfspHostConfig::from_context(&context);

    assert_eq!(config.mount_point, PathBuf::from("W:"));
    assert_eq!(config.volume_label, "e2v snapshot-pinned");
    assert_eq!(config.filesystem_name, "e2v-ro");
    assert_eq!(config.sector_size, 4096);
    assert!(config.read_only);
    assert!(!config.enable_kernel_file_info_cache_bypass);
}

#[test]
fn winfsp_host_config_enables_cache_bypass_for_direct_io_mounts() {
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
    let request = MountRequest::from_config(
        VfsMountConfig::live_branch(repo_root, branch_token)
            .with_platform_capabilities(PlatformCapabilities::no_reliable_invalidation()),
        PathBuf::from("X:"),
    );
    let context = WinfspMountContext::from_request(request);

    let config = WinfspHostConfig::from_context(&context);

    assert_eq!(config.mount_point, PathBuf::from("X:"));
    assert!(config.read_only);
    assert!(config.enable_kernel_file_info_cache_bypass);
}

#[test]
fn windows_mount_launcher_can_hand_host_config_to_a_winfsp_host() {
    #[derive(Default)]
    struct RecordingHost {
        seen_mount_point: Option<PathBuf>,
        seen_read_only: Option<bool>,
        seen_cache_bypass: Option<bool>,
    }

    impl WinfspHostLauncher for RecordingHost {
        fn launch(
            &mut self,
            launcher: &WindowsMountLauncher,
            context: WinfspMountContext,
        ) -> anyhow::Result<()> {
            let config = launcher.host_config(&context);
            self.seen_mount_point = Some(config.mount_point);
            self.seen_read_only = Some(config.read_only);
            self.seen_cache_bypass = Some(config.enable_kernel_file_info_cache_bypass);
            Ok(())
        }
    }

    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let snapshot_id = commit_message(&repo_root, "first", "alpha");
    let request = MountRequest::snapshot(repo_root, snapshot_id, PathBuf::from("W:"));
    let launcher = WindowsMountLauncher::from_request(&request);
    let context = WinfspMountContext::from_request(request);
    let mut host = RecordingHost::default();

    launcher.launch_with_host(context, &mut host).unwrap();

    assert_eq!(host.seen_mount_point.as_ref(), Some(&PathBuf::from("W:")));
    assert_eq!(host.seen_read_only, Some(true));
    assert_eq!(host.seen_cache_bypass, Some(false));
}

#[test]
fn winfsp_volume_params_reflect_snapshot_volume_defaults() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let snapshot_id = commit_message(&repo_root, "first", "alpha");
    let request = MountRequest::snapshot(repo_root, snapshot_id, PathBuf::from("W:"));
    let context = WinfspMountContext::from_request(request);
    let host_config = WinfspHostConfig::from_context(&context);

    let params = WinfspVolumeParams::from_host_config(&host_config);

    assert_eq!(params.sector_size, 4096);
    assert_eq!(params.sectors_per_allocation_unit, 1);
    assert_eq!(params.file_info_timeout_ms, 1_000);
    assert_eq!(params.volume_info_timeout_ms, 1_000);
    assert_eq!(params.dir_info_timeout_ms, 1_000);
    assert!(params.case_sensitive_search);
    assert!(params.case_preserved_names);
    assert!(params.unicode_on_disk);
    assert!(params.read_only_volume);
    assert!(params.persistent_acls);
    assert!(!params.um_file_context_is_full_context);
    assert!(params.um_file_context_is_user_context2);
    assert_eq!(params.prefix, "");
    assert_eq!(params.filesystem_name, "e2v-ro");
}

#[test]
fn winfsp_volume_params_disable_kernel_timeouts_for_direct_io_mounts() {
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
    let request = MountRequest::from_config(
        VfsMountConfig::live_branch(repo_root, branch_token)
            .with_platform_capabilities(PlatformCapabilities::no_reliable_invalidation()),
        PathBuf::from("X:"),
    );
    let context = WinfspMountContext::from_request(request);
    let host_config = WinfspHostConfig::from_context(&context);

    let params = WinfspVolumeParams::from_host_config(&host_config);

    assert_eq!(params.file_info_timeout_ms, 0);
    assert_eq!(params.volume_info_timeout_ms, 0);
    assert_eq!(params.dir_info_timeout_ms, 0);
}

#[test]
fn winfsp_runtime_paths_choose_arch_specific_dll_from_install_root() {
    let temp = tempdir().unwrap();
    let install_root = temp.path().join("WinFsp");
    let bin_dir = install_root.join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    fs::write(bin_dir.join("winfsp-x64.dll"), b"dll").unwrap();

    let paths = winfsp_runtime_paths_from_install_root(install_root.clone(), "x86_64").unwrap();

    assert_eq!(paths.install_root, install_root);
    assert_eq!(paths.bin_dir, bin_dir);
    assert_eq!(paths.dll_path, bin_dir.join("winfsp-x64.dll"));
}

#[test]
fn winfsp_runtime_paths_reject_missing_arch_specific_dll() {
    let temp = tempdir().unwrap();
    let install_root = temp.path().join("WinFsp");
    fs::create_dir_all(install_root.join("bin")).unwrap();

    let error = winfsp_runtime_paths_from_install_root(install_root.clone(), "x86_64").unwrap_err();

    assert!(
        error.to_string().contains("winfsp-x64.dll"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn winfsp_runtime_paths_choose_first_existing_default_install_root() {
    let temp = tempdir().unwrap();
    let missing_root = temp.path().join("missing");
    let install_root = temp.path().join("WinFsp");
    let bin_dir = install_root.join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    fs::write(bin_dir.join("winfsp-x64.dll"), b"dll").unwrap();

    let paths =
        winfsp_runtime_paths_from_candidate_roots(&[missing_root, install_root.clone()], "x86_64")
            .unwrap();

    assert_eq!(paths.install_root, install_root);
    assert_eq!(paths.dll_path, bin_dir.join("winfsp-x64.dll"));
}

#[test]
fn winfsp_runtime_library_loads_the_resolved_runtime_dll() {
    let paths = winfsp_runtime_paths_from_candidate_roots(
        &[
            PathBuf::from(r"C:\Program Files (x86)\WinFsp"),
            PathBuf::from(r"C:\Program Files\WinFsp"),
        ],
        "x86_64",
    )
    .unwrap();

    let library = WinfspRuntimeLibrary::load(&paths).unwrap();

    assert_eq!(library.dll_path(), &paths.dll_path);
}

#[test]
fn winfsp_runtime_library_resolves_the_create_export() {
    let paths = winfsp_runtime_paths_from_candidate_roots(
        &[
            PathBuf::from(r"C:\Program Files (x86)\WinFsp"),
            PathBuf::from(r"C:\Program Files\WinFsp"),
        ],
        "x86_64",
    )
    .unwrap();
    let library = WinfspRuntimeLibrary::load(&paths).unwrap();

    let symbol = winfsp_runtime_get_symbol_address(&library, "FspFileSystemCreate").unwrap();

    assert_ne!(symbol, 0);
}

#[test]
fn winfsp_runtime_library_rejects_missing_exports() {
    let paths = winfsp_runtime_paths_from_candidate_roots(
        &[
            PathBuf::from(r"C:\Program Files (x86)\WinFsp"),
            PathBuf::from(r"C:\Program Files\WinFsp"),
        ],
        "x86_64",
    )
    .unwrap();
    let library = WinfspRuntimeLibrary::load(&paths).unwrap();

    let error =
        winfsp_runtime_get_symbol_address(&library, "DefinitelyMissingWinfspSymbol").unwrap_err();

    assert!(
        error
            .to_string()
            .contains("failed to resolve WinFSP export"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn winfsp_runtime_library_resolves_required_mount_exports() {
    let paths = winfsp_runtime_paths_from_candidate_roots(
        &[
            PathBuf::from(r"C:\Program Files (x86)\WinFsp"),
            PathBuf::from(r"C:\Program Files\WinFsp"),
        ],
        "x86_64",
    )
    .unwrap();
    let library = WinfspRuntimeLibrary::load(&paths).unwrap();

    let exports = winfsp_runtime_resolve_mount_exports(&library).unwrap();

    assert_ne!(exports.create, 0);
    assert_ne!(exports.set_mount_point, 0);
    assert_ne!(exports.start_dispatcher, 0);
    assert_ne!(exports.stop_dispatcher, 0);
    assert_ne!(exports.delete_filesystem, 0);
}

#[test]
fn winfsp_host_session_can_be_built_from_runtime_and_mount_context() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let snapshot_id = commit_message(&repo_root, "first", "alpha");
    let request = MountRequest::snapshot(repo_root, snapshot_id, PathBuf::from("W:"));
    let launcher = WindowsMountLauncher::from_request(&request);
    let context = WinfspMountContext::from_request(request);
    let host_config = launcher.host_config(&context);
    let volume_params = WinfspVolumeParams::from_host_config(&host_config);
    let runtime_paths = winfsp_runtime_paths_from_candidate_roots(
        &[
            PathBuf::from(r"C:\Program Files (x86)\WinFsp"),
            PathBuf::from(r"C:\Program Files\WinFsp"),
        ],
        "x86_64",
    )
    .unwrap();
    let runtime = WinfspRuntimeLibrary::load(&runtime_paths).unwrap();

    let session = winfsp_host_session_new(runtime, host_config, volume_params).unwrap();

    assert_eq!(session.mount_point(), &PathBuf::from("W:"));
    assert_eq!(session.filesystem_name(), "e2v-ro");
    assert_eq!(session.volume_label(), "e2v snapshot-pinned");
    assert!(session.has_required_mount_exports());
    assert!(!winfsp_session_is_mounted(&session));
}

#[test]
fn winfsp_host_session_builds_a_native_create_request() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let snapshot_id = commit_message(&repo_root, "first", "alpha");
    let request = MountRequest::snapshot(repo_root, snapshot_id, PathBuf::from("W:"));
    let launcher = WindowsMountLauncher::from_request(&request);
    let context = WinfspMountContext::from_request(request);
    let host_config = launcher.host_config(&context);
    let volume_params = WinfspVolumeParams::from_host_config(&host_config);
    let runtime_paths = winfsp_runtime_paths_from_candidate_roots(
        &[
            PathBuf::from(r"C:\Program Files (x86)\WinFsp"),
            PathBuf::from(r"C:\Program Files\WinFsp"),
        ],
        "x86_64",
    )
    .unwrap();
    let runtime = WinfspRuntimeLibrary::load(&runtime_paths).unwrap();
    let session = winfsp_host_session_new(runtime, host_config, volume_params).unwrap();

    let request = winfsp_session_build_native_create_request(&session).unwrap();

    assert_eq!(request.device_path, "WinFsp.Disk");
    assert_eq!(request.mount_point, PathBuf::from("W:"));
    assert_eq!(request.volume_params.sector_size, 4096);
    assert!(request.volume_params.read_only_volume);
    assert_eq!(request.volume_params.filesystem_name, "e2v-ro");
}

#[test]
fn winfsp_host_session_can_create_and_destroy_a_native_filesystem_handle() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let snapshot_id = commit_message(&repo_root, "first", "alpha");
    let request = MountRequest::snapshot(repo_root, snapshot_id, PathBuf::from("W:"));
    let launcher = WindowsMountLauncher::from_request(&request);
    let context = WinfspMountContext::from_request(request);
    let host_config = launcher.host_config(&context);
    let volume_params = WinfspVolumeParams::from_host_config(&host_config);
    let runtime_paths = winfsp_runtime_paths_from_candidate_roots(
        &[
            PathBuf::from(r"C:\Program Files (x86)\WinFsp"),
            PathBuf::from(r"C:\Program Files\WinFsp"),
        ],
        "x86_64",
    )
    .unwrap();
    let runtime = WinfspRuntimeLibrary::load(&runtime_paths).unwrap();
    let mut session = winfsp_host_session_new(runtime, host_config, volume_params).unwrap();

    winfsp_session_create_filesystem_handle(&mut session).unwrap();

    assert!(winfsp_session_has_native_filesystem_handle(&session));

    winfsp_session_destroy_filesystem_handle(&mut session);

    assert!(!winfsp_session_has_native_filesystem_handle(&session));
}

#[test]
fn winfsp_host_session_can_run_mount_lifecycle_through_a_host_driver() {
    #[derive(Clone, Debug, PartialEq, Eq)]
    enum HostCall {
        CreateFilesystemHandle,
        SetMountPoint(PathBuf),
        StartDispatcher(u32),
        StopDispatcher,
        DeleteFilesystemHandle,
    }

    #[derive(Clone)]
    struct RecordingHostDriver {
        calls: Arc<Mutex<Vec<HostCall>>>,
    }

    impl RecordingHostDriver {
        fn new() -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl WinfspHostDriver for RecordingHostDriver {
        fn create_filesystem_handle(&self, _session: &mut WinfspHostSession) -> anyhow::Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(HostCall::CreateFilesystemHandle);
            Ok(())
        }

        fn set_mount_point(&self, mount_point: &std::path::Path) -> anyhow::Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(HostCall::SetMountPoint(mount_point.to_path_buf()));
            Ok(())
        }

        fn start_dispatcher(&self, thread_count: u32) -> anyhow::Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(HostCall::StartDispatcher(thread_count));
            Ok(())
        }

        fn stop_dispatcher(&self) -> anyhow::Result<()> {
            self.calls.lock().unwrap().push(HostCall::StopDispatcher);
            Ok(())
        }

        fn delete_filesystem_handle(&self, _session: &mut WinfspHostSession) -> anyhow::Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(HostCall::DeleteFilesystemHandle);
            Ok(())
        }
    }

    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let snapshot_id = commit_message(&repo_root, "first", "alpha");
    let request = MountRequest::snapshot(repo_root, snapshot_id, PathBuf::from("W:"));
    let launcher = WindowsMountLauncher::from_request(&request);
    let context = WinfspMountContext::from_request(request);
    let host_config = launcher.host_config(&context);
    let volume_params = WinfspVolumeParams::from_host_config(&host_config);
    let runtime_paths = winfsp_runtime_paths_from_candidate_roots(
        &[
            PathBuf::from(r"C:\Program Files (x86)\WinFsp"),
            PathBuf::from(r"C:\Program Files\WinFsp"),
        ],
        "x86_64",
    )
    .unwrap();
    let runtime = WinfspRuntimeLibrary::load(&runtime_paths).unwrap();
    let mut session = winfsp_host_session_new(runtime, host_config, volume_params).unwrap();
    let driver = RecordingHostDriver::new();

    let mount_point = session.mount_point().clone();
    winfsp_session_run_mount_lifecycle(&mut session, &driver, mount_point, 0).unwrap();

    assert_eq!(
        *driver.calls.lock().unwrap(),
        vec![
            HostCall::CreateFilesystemHandle,
            HostCall::SetMountPoint(PathBuf::from("W:")),
            HostCall::StartDispatcher(0),
            HostCall::StopDispatcher,
            HostCall::DeleteFilesystemHandle,
        ]
    );
    assert!(winfsp_session_is_mounted(&session));
    assert!(!winfsp_session_has_native_filesystem_handle(&session));
}

#[test]
fn winfsp_host_session_mount_lifecycle_cleans_up_a_real_native_handle() {
    struct NativeHandleLifecycleDriver;

    impl WinfspHostDriver for NativeHandleLifecycleDriver {
        fn create_filesystem_handle(&self, session: &mut WinfspHostSession) -> anyhow::Result<()> {
            winfsp_session_create_filesystem_handle(session)
        }

        fn set_mount_point(&self, _mount_point: &std::path::Path) -> anyhow::Result<()> {
            Ok(())
        }

        fn start_dispatcher(&self, _thread_count: u32) -> anyhow::Result<()> {
            Ok(())
        }

        fn stop_dispatcher(&self) -> anyhow::Result<()> {
            Ok(())
        }

        fn delete_filesystem_handle(&self, session: &mut WinfspHostSession) -> anyhow::Result<()> {
            winfsp_session_destroy_filesystem_handle(session);
            Ok(())
        }
    }

    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let snapshot_id = commit_message(&repo_root, "first", "alpha");
    let request = MountRequest::snapshot(repo_root, snapshot_id, PathBuf::from("W:"));
    let launcher = WindowsMountLauncher::from_request(&request);
    let context = WinfspMountContext::from_request(request);
    let host_config = launcher.host_config(&context);
    let volume_params = WinfspVolumeParams::from_host_config(&host_config);
    let runtime_paths = winfsp_runtime_paths_from_candidate_roots(
        &[
            PathBuf::from(r"C:\Program Files (x86)\WinFsp"),
            PathBuf::from(r"C:\Program Files\WinFsp"),
        ],
        "x86_64",
    )
    .unwrap();
    let runtime = WinfspRuntimeLibrary::load(&runtime_paths).unwrap();
    let mut session = winfsp_host_session_new(runtime, host_config, volume_params).unwrap();

    winfsp_session_run_mount_lifecycle(
        &mut session,
        &NativeHandleLifecycleDriver,
        PathBuf::from("W:"),
        0,
    )
    .unwrap();

    assert!(winfsp_session_is_mounted(&session));
    assert!(!winfsp_session_has_native_filesystem_handle(&session));
}

#[test]
fn winfsp_mount_context_captures_mount_request_and_cache_policy() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);
    let snapshot_id = commit_message(&repo_root, "first", "alpha");
    let request = MountRequest::snapshot(repo_root, snapshot_id, PathBuf::from("M:"));
    let context = WinfspMountContext::from_request(request.clone());

    assert_eq!(context.mount_mode_label(), "snapshot-pinned");
    assert_eq!(context.mount_point(), &PathBuf::from("M:"));
    assert_eq!(
        context.cache_policy(),
        CachePolicy::KernelCacheWithInvalidation
    );
}

#[test]
fn winfsp_mount_context_can_open_a_file_handle_from_the_vfs() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let snapshot_id = commit_message(&repo_root, "first", "alpha");
    let request = MountRequest::snapshot(repo_root.clone(), snapshot_id, PathBuf::from("M:"));
    let context = WinfspMountContext::from_request(request);
    let handle = context.open_handle("tracked.txt").unwrap();

    assert_eq!(context.mount_mode_label(), "snapshot-pinned");
    assert_eq!(handle.logical_path(), "tracked.txt");
    assert!(!handle.file_object_id().is_empty());
    assert_eq!(handle.snapshot_id(), context.namespace_snapshot_id());
    assert!(handle.layout_generation() > 0);
    assert!(handle.inode_id() > 0);
}

#[test]
fn winfsp_mount_context_accepts_explicit_read_only_open_requests() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let snapshot_id = commit_message(&repo_root, "first", "alpha");
    let request = MountRequest::snapshot(repo_root.clone(), snapshot_id, PathBuf::from("M:"));
    let context = WinfspMountContext::from_request(request);
    let handle = context
        .open_handle_for_request(&WinfspOpenRequest::read_only("tracked.txt"))
        .unwrap();

    assert_eq!(handle.logical_path(), "tracked.txt");
    assert_eq!(handle.snapshot_id(), context.namespace_snapshot_id());
}

#[test]
fn winfsp_mount_context_rejects_writable_open_requests() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let snapshot_id = commit_message(&repo_root, "first", "alpha");
    let request = MountRequest::snapshot(repo_root.clone(), snapshot_id, PathBuf::from("M:"));
    let context = WinfspMountContext::from_request(request);
    let error = context
        .open_handle_for_request(&WinfspOpenRequest::writable("tracked.txt"))
        .unwrap_err();

    assert!(error.to_string().contains("writable"));
}

#[test]
fn winfsp_mount_context_can_read_bytes_from_an_open_handle() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let snapshot_id = commit_message(&repo_root, "first", "alpha");
    let request = MountRequest::snapshot(repo_root, snapshot_id, PathBuf::from("M:"));
    let context = WinfspMountContext::from_request(request);
    let handle = context.open_handle("tracked.txt").unwrap();

    let bytes = context.read_handle(&handle, 0, 5).unwrap();
    assert_eq!(String::from_utf8(bytes).unwrap(), "alpha");
}

#[test]
fn winfsp_mount_context_can_stat_paths_from_the_vfs() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    fs::create_dir_all(repo_root.join("nested")).unwrap();
    fs::write(repo_root.join("nested").join("base.txt"), "alpha").unwrap();
    fs::write(repo_root.join("tracked.txt"), "root-file").unwrap();
    let snapshot_id = RepositoryFacade::new()
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap()
        .snapshot_id;

    let request = MountRequest::snapshot(repo_root, snapshot_id, PathBuf::from("M:"));
    let context = WinfspMountContext::from_request(request);

    let nested = context.stat_path("nested").unwrap();
    assert_eq!(nested.kind, VfsNodeKind::Directory);
    assert_eq!(nested.size_bytes, 0);
    assert!(nested.inode_id > 0);

    let tracked = context.stat_path("tracked.txt").unwrap();
    assert_eq!(tracked.kind, VfsNodeKind::File);
    assert_eq!(tracked.size_bytes, "root-file".len() as u64);
    assert!(tracked.inode_id > 0);
    assert_ne!(nested.inode_id, tracked.inode_id);
}

#[test]
fn winfsp_mount_context_can_list_directory_entries_from_the_vfs() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    fs::create_dir_all(repo_root.join("nested")).unwrap();
    fs::write(repo_root.join("nested").join("base.txt"), "alpha").unwrap();
    fs::write(repo_root.join("tracked.txt"), "root-file").unwrap();
    let snapshot_id = RepositoryFacade::new()
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap()
        .snapshot_id;

    let request = MountRequest::snapshot(repo_root, snapshot_id, PathBuf::from("M:"));
    let context = WinfspMountContext::from_request(request);

    let root_entries = context.read_directory_entries("").unwrap();
    let root_names = root_entries
        .iter()
        .map(|entry| (entry.name.as_str(), entry.kind.as_str()))
        .collect::<Vec<_>>();
    assert!(root_names.contains(&("nested", "tree")));
    assert!(root_names.contains(&("tracked.txt", "file")));

    let nested_entries = context.read_directory_entries("nested").unwrap();
    assert_eq!(nested_entries.len(), 1);
    assert_eq!(nested_entries[0].name, "base.txt");
    assert_eq!(nested_entries[0].kind, "file");
}

#[test]
fn winfsp_mount_context_exposes_a_read_only_volume_summary() {
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

    let request = MountRequest::snapshot(repo_root, snapshot_id, PathBuf::from("M:"));
    let context = WinfspMountContext::from_request(request);
    let summary = context.volume_summary();

    assert!(summary.volume_label.contains("e2v"));
    assert!(summary.filesystem_name.contains("e2v"));
    assert!(summary.total_bytes >= summary.free_bytes);
    assert!(summary.read_only);
    assert_eq!(summary.sector_size, 4096);
}

#[test]
fn winfsp_mount_context_derives_cache_policy_from_mount_request() {
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

    let request = MountRequest::from_config(
        VfsMountConfig::live_branch(repo_root, branch_token)
            .with_platform_capabilities(PlatformCapabilities::no_reliable_invalidation()),
        PathBuf::from("M:"),
    );
    let context = WinfspMountContext::from_request(request);

    assert_eq!(context.mount_mode_label(), "live-branch");
    assert_eq!(context.cache_policy(), CachePolicy::DirectIoFallback);
}

#[test]
fn winfsp_mount_context_refreshes_live_branch_and_reports_invalidation_need() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    commit_message(&repo_root, "first", "alpha");
    let branch_token = RepositoryFacade::new()
        .open(&repo_root)
        .unwrap()
        .branch
        .token_hex;
    let request = MountRequest::live_branch(repo_root.clone(), branch_token, PathBuf::from("M:"));
    let mut context = WinfspMountContext::from_request(request);

    let old_handle = context.open_handle("tracked.txt").unwrap();
    commit_message(&repo_root, "second", "beta");

    let refresh = context.refresh_namespace().unwrap();
    assert!(refresh.namespace_changed);
    assert!(refresh.requires_invalidation);
    let invalidation = context
        .build_invalidation_plan(&refresh)
        .expect("live branch refresh should produce invalidation work");
    assert!(invalidation.invalidate_directory_entries);
    assert!(invalidation.invalidate_attributes);
    assert!(invalidation.invalidate_page_cache);
    assert!(invalidation.inode_ids.contains(&old_handle.inode_id()));

    let new_handle = context.open_handle("tracked.txt").unwrap();
    let old_bytes = context.read_handle(&old_handle, 0, 32).unwrap();
    let new_bytes = context.read_handle(&new_handle, 0, 32).unwrap();

    assert_eq!(String::from_utf8(old_bytes).unwrap(), "alpha");
    assert_eq!(String::from_utf8(new_bytes).unwrap(), "beta");
}

#[test]
fn winfsp_mount_context_snapshot_refresh_is_a_noop() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let snapshot_id = commit_message(&repo_root, "first", "alpha");
    let request = MountRequest::snapshot(repo_root, snapshot_id, PathBuf::from("M:"));
    let mut context = WinfspMountContext::from_request(request);

    let refresh = context.refresh_namespace().unwrap();
    assert!(!refresh.namespace_changed);
    assert!(!refresh.requires_invalidation);
    assert!(context.build_invalidation_plan(&refresh).is_none());
}

#[test]
fn winfsp_direct_io_refresh_skips_kernel_invalidation_plan() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    commit_message(&repo_root, "first", "alpha");
    let branch_token = RepositoryFacade::new()
        .open(&repo_root)
        .unwrap()
        .branch
        .token_hex;
    let request = MountRequest::from_config(
        VfsMountConfig::live_branch(repo_root.clone(), branch_token)
            .with_platform_capabilities(PlatformCapabilities::no_reliable_invalidation()),
        PathBuf::from("M:"),
    );
    let mut context = WinfspMountContext::from_request(request);
    let handle = context.open_handle("tracked.txt").unwrap();

    commit_message(&repo_root, "second", "beta");

    let refresh = context.refresh_namespace().unwrap();
    assert!(refresh.namespace_changed);
    assert!(!refresh.requires_invalidation);
    assert!(context.build_invalidation_plan(&refresh).is_none());
    assert!(handle.inode_id() > 0);
}

#[test]
fn winfsp_mount_context_can_apply_kernel_invalidation_after_live_refresh() {
    #[derive(Default)]
    struct RecordingInvalidator {
        directory_entry_invalidations: Vec<String>,
        inode_invalidations: Vec<u64>,
        attribute_invalidations: Vec<u64>,
        page_cache_invalidations: Vec<u64>,
    }

    impl WinfspInvalidator for RecordingInvalidator {
        fn invalidate_directory_entries(&mut self, logical_path: &str) -> anyhow::Result<()> {
            self.directory_entry_invalidations
                .push(logical_path.to_string());
            Ok(())
        }

        fn invalidate_inode(&mut self, inode_id: u64) -> anyhow::Result<()> {
            self.inode_invalidations.push(inode_id);
            Ok(())
        }

        fn invalidate_attributes(&mut self, inode_id: u64) -> anyhow::Result<()> {
            self.attribute_invalidations.push(inode_id);
            Ok(())
        }

        fn invalidate_page_cache(&mut self, inode_id: u64) -> anyhow::Result<()> {
            self.page_cache_invalidations.push(inode_id);
            Ok(())
        }
    }

    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    commit_message(&repo_root, "first", "alpha");
    let branch_token = RepositoryFacade::new()
        .open(&repo_root)
        .unwrap()
        .branch
        .token_hex;
    let request = MountRequest::live_branch(repo_root.clone(), branch_token, PathBuf::from("M:"));
    let mut context = WinfspMountContext::from_request(request);
    context.stat_path("").unwrap();
    let old_handle = context.open_handle("tracked.txt").unwrap();

    commit_message(&repo_root, "second", "beta");

    let refresh = context.refresh_namespace().unwrap();
    let mut invalidator = RecordingInvalidator::default();
    let applied = context
        .apply_invalidation(&refresh, &mut invalidator)
        .unwrap();

    assert!(applied);
    assert_eq!(
        invalidator.directory_entry_invalidations,
        vec![String::new()]
    );
    assert!(
        invalidator
            .inode_invalidations
            .contains(&old_handle.inode_id())
    );
    assert!(
        invalidator
            .attribute_invalidations
            .contains(&old_handle.inode_id())
    );
    assert!(
        invalidator
            .page_cache_invalidations
            .contains(&old_handle.inode_id())
    );
}

#[test]
fn winfsp_mount_context_invalidation_plan_includes_observed_directory_inodes() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    fs::create_dir_all(repo_root.join("nested")).unwrap();
    fs::write(repo_root.join("nested").join("base.txt"), "alpha").unwrap();
    RepositoryFacade::new()
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "first".to_string(),
        })
        .unwrap();
    let branch_token = RepositoryFacade::new()
        .open(&repo_root)
        .unwrap()
        .branch
        .token_hex;
    let request = MountRequest::live_branch(repo_root.clone(), branch_token, PathBuf::from("M:"));
    let mut context = WinfspMountContext::from_request(request);

    let root = context.stat_path("").unwrap();
    let nested = context.stat_path("nested").unwrap();
    let entries = context.read_directory_entries("nested").unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "base.txt");

    fs::write(repo_root.join("nested").join("second.txt"), "beta").unwrap();
    RepositoryFacade::new()
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "second".to_string(),
        })
        .unwrap();

    let refresh = context.refresh_namespace().unwrap();
    let invalidation = context
        .build_invalidation_plan(&refresh)
        .expect("live branch refresh should produce invalidation work");

    assert!(invalidation.inode_ids.contains(&root.inode_id));
    assert!(invalidation.inode_ids.contains(&nested.inode_id));
    assert!(invalidation.directory_paths.contains(&String::new()));
    assert!(invalidation.directory_paths.contains(&"nested".to_string()));
}

#[test]
fn winfsp_mount_context_records_normalized_directory_paths_for_invalidation() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    fs::create_dir_all(repo_root.join("nested")).unwrap();
    fs::write(repo_root.join("nested").join("base.txt"), "alpha").unwrap();
    RepositoryFacade::new()
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "first".to_string(),
        })
        .unwrap();
    let branch_token = RepositoryFacade::new()
        .open(&repo_root)
        .unwrap()
        .branch
        .token_hex;
    let request = MountRequest::live_branch(repo_root.clone(), branch_token, PathBuf::from("M:"));
    let mut context = WinfspMountContext::from_request(request);

    let metadata = context.stat_path("/nested/").unwrap();
    assert_eq!(metadata.logical_path, "nested");

    fs::write(repo_root.join("nested").join("second.txt"), "beta").unwrap();
    RepositoryFacade::new()
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "second".to_string(),
        })
        .unwrap();

    let refresh = context.refresh_namespace().unwrap();
    let invalidation = context
        .build_invalidation_plan(&refresh)
        .expect("live branch refresh should produce invalidation work");

    assert!(invalidation.directory_paths.contains(&"nested".to_string()));
    assert!(
        !invalidation
            .directory_paths
            .contains(&"/nested/".to_string())
    );
}

#[test]
fn winfsp_mount_context_applies_directory_entry_invalidation_per_observed_directory() {
    #[derive(Default)]
    struct RecordingInvalidator {
        directory_entry_invalidations: Vec<String>,
        inode_invalidations: Vec<u64>,
        attribute_invalidations: Vec<u64>,
        page_cache_invalidations: Vec<u64>,
    }

    impl WinfspInvalidator for RecordingInvalidator {
        fn invalidate_directory_entries(&mut self, logical_path: &str) -> anyhow::Result<()> {
            self.directory_entry_invalidations
                .push(logical_path.to_string());
            Ok(())
        }

        fn invalidate_inode(&mut self, inode_id: u64) -> anyhow::Result<()> {
            self.inode_invalidations.push(inode_id);
            Ok(())
        }

        fn invalidate_attributes(&mut self, inode_id: u64) -> anyhow::Result<()> {
            self.attribute_invalidations.push(inode_id);
            Ok(())
        }

        fn invalidate_page_cache(&mut self, inode_id: u64) -> anyhow::Result<()> {
            self.page_cache_invalidations.push(inode_id);
            Ok(())
        }
    }

    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    fs::create_dir_all(repo_root.join("nested")).unwrap();
    fs::write(repo_root.join("nested").join("base.txt"), "alpha").unwrap();
    RepositoryFacade::new()
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "first".to_string(),
        })
        .unwrap();
    let branch_token = RepositoryFacade::new()
        .open(&repo_root)
        .unwrap()
        .branch
        .token_hex;
    let request = MountRequest::live_branch(repo_root.clone(), branch_token, PathBuf::from("M:"));
    let mut context = WinfspMountContext::from_request(request);

    context.stat_path("").unwrap();
    context.stat_path("nested").unwrap();

    fs::write(repo_root.join("nested").join("second.txt"), "beta").unwrap();
    RepositoryFacade::new()
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "second".to_string(),
        })
        .unwrap();

    let refresh = context.refresh_namespace().unwrap();
    let mut invalidator = RecordingInvalidator::default();
    let applied = context
        .apply_invalidation(&refresh, &mut invalidator)
        .unwrap();

    assert!(applied);
    assert_eq!(
        invalidator.directory_entry_invalidations,
        vec![String::new(), "nested".to_string()]
    );
}

#[test]
fn winfsp_mount_context_skips_kernel_invalidation_application_when_not_required() {
    #[derive(Default)]
    struct RecordingInvalidator {
        calls: usize,
    }

    impl WinfspInvalidator for RecordingInvalidator {
        fn invalidate_directory_entries(&mut self, _logical_path: &str) -> anyhow::Result<()> {
            self.calls += 1;
            Ok(())
        }

        fn invalidate_inode(&mut self, _inode_id: u64) -> anyhow::Result<()> {
            self.calls += 1;
            Ok(())
        }

        fn invalidate_attributes(&mut self, _inode_id: u64) -> anyhow::Result<()> {
            self.calls += 1;
            Ok(())
        }

        fn invalidate_page_cache(&mut self, _inode_id: u64) -> anyhow::Result<()> {
            self.calls += 1;
            Ok(())
        }
    }

    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    commit_message(&repo_root, "first", "alpha");
    let branch_token = RepositoryFacade::new()
        .open(&repo_root)
        .unwrap()
        .branch
        .token_hex;
    let request = MountRequest::from_config(
        VfsMountConfig::live_branch(repo_root.clone(), branch_token)
            .with_platform_capabilities(PlatformCapabilities::no_reliable_invalidation()),
        PathBuf::from("M:"),
    );
    let mut context = WinfspMountContext::from_request(request);

    commit_message(&repo_root, "second", "beta");

    let refresh = context.refresh_namespace().unwrap();
    let mut invalidator = RecordingInvalidator::default();
    let applied = context
        .apply_invalidation(&refresh, &mut invalidator)
        .unwrap();

    assert!(!applied);
    assert_eq!(invalidator.calls, 0);
}

#[test]
fn mount_snapshot_returns_a_launcher_summary_that_matches_the_request() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let snapshot_id = commit_message(&repo_root, "first", "alpha");
    let summary = try_mount_snapshot_on_current_platform(
        VfsMountConfig::snapshot(repo_root, snapshot_id),
        PathBuf::from("Q:"),
    )
    .unwrap();

    assert_eq!(summary.mount_mode, "snapshot-pinned");
    assert_eq!(summary.mount_point, PathBuf::from("Q:"));
    assert_eq!(
        summary.cache_policy,
        CachePolicy::KernelCacheWithInvalidation
    );
}

#[test]
fn mount_live_branch_summary_reports_direct_io_when_invalidation_is_unreliable() {
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

    let summary = e2v_vfs::try_mount_live_branch_on_current_platform(
        VfsMountConfig::live_branch(repo_root, branch_token)
            .with_platform_capabilities(PlatformCapabilities::no_reliable_invalidation()),
        PathBuf::from("R:"),
    )
    .unwrap();

    assert_eq!(summary.mount_mode, "live-branch");
    assert_eq!(summary.mount_point, PathBuf::from("R:"));
    assert_eq!(summary.cache_policy, CachePolicy::DirectIoFallback);
}

#[test]
fn linux_mount_adapter_exposes_a_future_fuse_boundary() {
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

    let request = MountRequest::from_config(
        VfsMountConfig::live_branch(repo_root, branch_token)
            .with_platform_capabilities(PlatformCapabilities::no_reliable_invalidation()),
        PathBuf::from("/mnt/e2v"),
    );
    let adapter = LinuxMountAdapter;
    let summary = adapter.launch(request).unwrap();

    assert_eq!(adapter.platform_family(), PlatformFamily::LinuxFuse);
    assert_eq!(summary.mount_mode, "live-branch");
    assert_eq!(summary.cache_policy, CachePolicy::DirectIoFallback);
    assert!(
        summary
            .status_message
            .contains("linux adapter not implemented yet")
    );
}

#[test]
fn macos_mount_adapter_exposes_a_future_fuse_boundary() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let snapshot_id = commit_message(&repo_root, "first", "alpha");
    let request = MountRequest::from_config(
        VfsMountConfig::snapshot(repo_root, snapshot_id),
        PathBuf::from("/Volumes/e2v"),
    );
    let adapter = MacosMountAdapter;
    let summary = adapter.launch(request).unwrap();

    assert_eq!(adapter.platform_family(), PlatformFamily::MacosFuse);
    assert_eq!(summary.mount_mode, "snapshot-pinned");
    assert_eq!(
        summary.cache_policy,
        CachePolicy::KernelCacheWithInvalidation
    );
    assert!(
        summary
            .status_message
            .contains("macos adapter not implemented yet")
    );
}

#[cfg(windows)]
#[test]
fn windows_snapshot_mount_reads_repository_file_through_real_winfsp_mount() {
    use std::time::{Duration, Instant};

    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    init_repo(&repo_root);

    let snapshot_id = commit_message(&repo_root, "first", "alpha");
    let mount_point = PathBuf::from("Q:");

    let mounted =
        e2v_vfs::start_snapshot_mount(repo_root, snapshot_id, mount_point.clone()).unwrap();
    let summary = mounted.summary();
    let rooted_mount_point = PathBuf::from(r"Q:\");

    assert_eq!(summary.mount_mode, "snapshot-pinned");
    assert_eq!(summary.mount_point, rooted_mount_point);
    assert!(summary.status_message.contains("winfsp host mount active"));
    let dir_output = std::process::Command::new("cmd")
        .args(["/c", "dir", r"Q:\"])
        .output()
        .unwrap();
    println!(
        "dir status={:?}\nstdout={}\nstderr={}",
        dir_output.status.code(),
        String::from_utf8_lossy(&dir_output.stdout),
        String::from_utf8_lossy(&dir_output.stderr)
    );
    println!("metadata={:?}", fs::metadata(&summary.mount_point));
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match fs::read_to_string(summary.mount_point.join("tracked.txt")) {
            Ok(contents) => {
                assert_eq!(contents, "alpha");
                break;
            }
            Err(error) if Instant::now() < deadline => {
                println!("retry read error={error:?}");
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(error) => panic!("mounted file never became readable: {error:?}"),
        }
    }
}
