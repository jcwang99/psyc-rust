use std::ffi::{CStr, CString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;

use e2v_api::{
    BranchSummaryInfo, CommitInfo, RemoteRegistration, RepositoryInfo, ShareAcceptInfo,
    ShareInviteInfo, ShareListInfo, SnapshotInfo, c_abi,
};

fn read_owned_string(value: &mut c_abi::e2v_string_t) -> String {
    assert!(!value.ptr.is_null());
    let text = unsafe { CStr::from_ptr(value.ptr) }
        .to_str()
        .unwrap()
        .to_string();
    unsafe {
        c_abi::e2v_string_free(value);
    }
    text
}

fn read_owned_bytes(value: &mut c_abi::e2v_bytes_t) -> Vec<u8> {
    assert!(!value.ptr.is_null());
    let bytes = unsafe { std::slice::from_raw_parts(value.ptr, value.len) }.to_vec();
    unsafe {
        c_abi::e2v_bytes_free(value);
    }
    bytes
}

fn c_string(path: impl AsRef<str>) -> CString {
    CString::new(path.as_ref()).unwrap()
}

fn path_c_string(path: &std::path::Path) -> CString {
    CString::new(path.to_string_lossy().as_bytes()).unwrap()
}

fn read_header_file() -> String {
    fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("include")
            .join("e2v_api.h"),
    )
    .unwrap()
}

fn new_sdk_handle() -> *mut c_abi::e2v_sdk_t {
    let mut sdk = std::ptr::null_mut();
    let mut error = std::ptr::null_mut();

    let code = unsafe { c_abi::e2v_sdk_new(&mut sdk, &mut error) };

    assert_eq!(code, c_abi::E2V_OK);
    assert!(!sdk.is_null());
    assert!(error.is_null());
    sdk
}

fn error_message_from_ptr(error: *mut c_abi::e2v_error_t) -> String {
    assert!(!error.is_null());
    let mut message = c_abi::e2v_string_t::default();
    let code = unsafe { c_abi::e2v_error_message(error, &mut message) };
    assert_eq!(code, c_abi::E2V_OK);
    read_owned_string(&mut message)
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

fn unique_ffi_smoke_suffix() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

fn compile_ffi_smoke_program(workspace_root: &Path) -> PathBuf {
    let build_status = Command::new("cargo")
        .args(["build", "-p", "e2v-api", "--release"])
        .current_dir(workspace_root)
        .status()
        .unwrap();
    assert!(build_status.success());

    let suffix = unique_ffi_smoke_suffix();
    let compile_script = workspace_root
        .join("target")
        .join(format!("ffi-smoke-build-{suffix}.cmd"));
    let object_name = format!("ffi_smoke-{suffix}.obj");
    let exe_name = format!("ffi-smoke-{suffix}.exe");
    fs::write(
        &compile_script,
        format!(
            "@echo off\r\ncall \"C:\\Program Files\\Microsoft Visual Studio\\18\\Community\\VC\\Auxiliary\\Build\\vcvars64.bat\" >nul\r\ncl /nologo /MD /Fo:target\\{object_name} /I crates\\e2v-api\\include crates\\e2v-api\\tests\\ffi_smoke.c /link /OUT:target\\{exe_name} target\\release\\e2v_api.dll.lib\r\n"
        ),
    )
    .unwrap();

    let compile_status = Command::new("cmd")
        .args(["/c", compile_script.to_string_lossy().as_ref()])
        .current_dir(workspace_root)
        .status()
        .unwrap();
    let _ = fs::remove_file(&compile_script);
    assert!(compile_status.success());

    workspace_root.join("target").join(exe_name)
}

fn remove_legacy_ffi_smoke_roots() {
    let temp_root = std::env::temp_dir();
    if let Ok(entries) = fs::read_dir(&temp_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
                continue;
            };
            if name.starts_with("e2v-ffi-smoke-") {
                let _ = fs::remove_dir_all(path);
            }
        }
    }
}

fn remove_dir_if_exists(path: &Path) {
    if !path.exists() {
        return;
    }
    for _ in 0..5 {
        match fs::remove_dir_all(path) {
            Ok(()) => return,
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(50)),
        }
    }
    fs::remove_dir_all(path).unwrap();
}

#[test]
fn c_abi_can_create_and_free_sdk_handle() {
    let sdk = new_sdk_handle();
    unsafe {
        c_abi::e2v_sdk_free(sdk);
    }
}

#[test]
fn c_abi_rejects_null_out_parameter_with_invalid_argument() {
    let mut error = std::ptr::null_mut();

    let code = unsafe { c_abi::e2v_sdk_new(std::ptr::null_mut(), &mut error) };

    assert_eq!(code, c_abi::E2V_INVALID_ARGUMENT);
    assert!(!error.is_null());
    assert_eq!(
        unsafe { c_abi::e2v_error_code(error) },
        c_abi::E2V_INVALID_ARGUMENT
    );
    unsafe {
        c_abi::e2v_error_free(error);
    }
}

#[test]
fn c_abi_maps_missing_repo_to_not_found() {
    let sdk = new_sdk_handle();
    let repo_root = c_string("missing-repo");
    let mut json = c_abi::e2v_string_t::default();
    let mut error = std::ptr::null_mut();

    let code =
        unsafe { c_abi::e2v_open_repository_json(sdk, repo_root.as_ptr(), &mut json, &mut error) };

    assert_eq!(code, c_abi::E2V_NOT_FOUND);
    assert!(json.ptr.is_null());
    assert!(!error.is_null());
    assert_eq!(
        unsafe { c_abi::e2v_error_code(error) },
        c_abi::E2V_NOT_FOUND
    );

    let mut message = c_abi::e2v_string_t::default();
    let message_code = unsafe { c_abi::e2v_error_message(error, &mut message) };
    assert_eq!(message_code, c_abi::E2V_OK);
    let text = read_owned_string(&mut message);
    assert!(text.contains("not found") || text.contains("missing"));

    unsafe {
        c_abi::e2v_error_free(error);
        c_abi::e2v_sdk_free(sdk);
    }
}

#[test]
fn c_abi_catches_panics_and_returns_internal_panic() {
    let mut error = std::ptr::null_mut();

    let code = unsafe { c_abi::e2v_test_only_force_panic(&mut error) };

    assert_eq!(code, c_abi::E2V_INTERNAL_PANIC);
    assert!(!error.is_null());
    assert_eq!(
        unsafe { c_abi::e2v_error_code(error) },
        c_abi::E2V_INTERNAL_PANIC
    );
    unsafe {
        c_abi::e2v_error_free(error);
    }
}

#[test]
fn c_abi_can_init_commit_and_read_file_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let sdk = new_sdk_handle();
    let repo_root_c = path_c_string(&repo_root);
    let password = c_string("correct horse battery staple");
    let branch_name = c_string("main");

    let mut init_json = c_abi::e2v_string_t::default();
    let mut error = std::ptr::null_mut();
    let init_code = unsafe {
        c_abi::e2v_init_repository_json(
            sdk,
            repo_root_c.as_ptr(),
            password.as_ptr(),
            branch_name.as_ptr(),
            &mut init_json,
            &mut error,
        )
    };
    assert_eq!(init_code, c_abi::E2V_OK);
    assert!(error.is_null());

    let repo: RepositoryInfo = serde_json::from_str(&read_owned_string(&mut init_json)).unwrap();
    fs::write(repo_root.join("hello.txt"), "hello ffi").unwrap();

    let message = c_string("seed");
    let mut commit_json = c_abi::e2v_string_t::default();
    let commit_code = unsafe {
        c_abi::e2v_commit_repository_json(
            sdk,
            repo_root_c.as_ptr(),
            message.as_ptr(),
            &mut commit_json,
            &mut error,
        )
    };
    assert_eq!(commit_code, c_abi::E2V_OK);
    assert!(error.is_null());
    let commit: CommitInfo = serde_json::from_str(&read_owned_string(&mut commit_json)).unwrap();
    assert_eq!(commit.committed_files, 1);

    let mut read_handle = std::ptr::null_mut();
    let read_code = unsafe {
        c_abi::e2v_open_read_handle(sdk, repo_root_c.as_ptr(), &mut read_handle, &mut error)
    };
    assert_eq!(read_code, c_abi::E2V_OK);
    assert!(error.is_null());
    assert!(!read_handle.is_null());

    let branch_token = CString::new(repo.branch.token_hex).unwrap();
    let mut snapshot = std::ptr::null_mut();
    let resolve_code = unsafe {
        c_abi::e2v_resolve_branch(
            read_handle,
            branch_token.as_ptr(),
            &mut snapshot,
            &mut error,
        )
    };
    assert_eq!(resolve_code, c_abi::E2V_OK);
    assert!(error.is_null());
    assert!(!snapshot.is_null());

    let path = c_string("hello.txt");
    let mut file = std::ptr::null_mut();
    let open_code = unsafe {
        c_abi::e2v_open_file(read_handle, snapshot, path.as_ptr(), &mut file, &mut error)
    };
    assert_eq!(open_code, c_abi::E2V_OK);
    assert!(error.is_null());
    assert!(!file.is_null());

    let mut dir_json = c_abi::e2v_string_t::default();
    let read_dir_code = unsafe {
        c_abi::e2v_read_dir_json(
            read_handle,
            snapshot,
            c_string("").as_ptr(),
            &mut dir_json,
            &mut error,
        )
    };
    assert_eq!(read_dir_code, c_abi::E2V_OK);
    assert!(error.is_null());
    let entries: Vec<e2v_api::DirectoryEntryInfo> =
        serde_json::from_str(&read_owned_string(&mut dir_json)).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "hello.txt");

    let mut bytes = c_abi::e2v_bytes_t::default();
    let read_bytes_code =
        unsafe { c_abi::e2v_read_range(read_handle, file, 0, 32, &mut bytes, &mut error) };
    assert_eq!(read_bytes_code, c_abi::E2V_OK);
    assert!(error.is_null());
    assert_eq!(
        String::from_utf8(read_owned_bytes(&mut bytes)).unwrap(),
        "hello ffi"
    );

    let snapshot_id = c_string(&commit.snapshot_id);
    let mut opened_snapshot = std::ptr::null_mut();
    let open_snapshot_code = unsafe {
        c_abi::e2v_open_snapshot(
            read_handle,
            snapshot_id.as_ptr(),
            &mut opened_snapshot,
            &mut error,
        )
    };
    assert_eq!(open_snapshot_code, c_abi::E2V_OK);
    assert!(error.is_null());
    assert!(!opened_snapshot.is_null());

    unsafe {
        c_abi::e2v_snapshot_view_free(opened_snapshot);
        c_abi::e2v_file_view_free(file);
        c_abi::e2v_snapshot_view_free(snapshot);
        c_abi::e2v_read_handle_free(read_handle);
        c_abi::e2v_sdk_free(sdk);
    }
}

#[test]
fn c_abi_can_list_snapshots_checkout_and_change_password() {
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let checkout_dir = temp.path().join("checkout");
    fs::create_dir_all(&repo_root).unwrap();
    fs::create_dir_all(&checkout_dir).unwrap();

    let sdk = new_sdk_handle();
    let repo_root_c = path_c_string(&repo_root);
    let checkout_dir_c = path_c_string(&checkout_dir);
    let old_password = c_string("old-password");
    let new_password = c_string("new-password");
    let branch_name = c_string("main");
    let mut error = std::ptr::null_mut();
    let mut init_json = c_abi::e2v_string_t::default();

    assert_eq!(
        unsafe {
            c_abi::e2v_init_repository_json(
                sdk,
                repo_root_c.as_ptr(),
                old_password.as_ptr(),
                branch_name.as_ptr(),
                &mut init_json,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let _repo: RepositoryInfo = serde_json::from_str(&read_owned_string(&mut init_json)).unwrap();

    fs::write(repo_root.join("tracked.txt"), "alpha").unwrap();
    let mut first_commit_json = c_abi::e2v_string_t::default();
    assert_eq!(
        unsafe {
            c_abi::e2v_commit_repository_json(
                sdk,
                repo_root_c.as_ptr(),
                c_string("first").as_ptr(),
                &mut first_commit_json,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let first: CommitInfo =
        serde_json::from_str(&read_owned_string(&mut first_commit_json)).unwrap();

    fs::write(repo_root.join("tracked.txt"), "beta").unwrap();
    let mut second_commit_json = c_abi::e2v_string_t::default();
    assert_eq!(
        unsafe {
            c_abi::e2v_commit_repository_json(
                sdk,
                repo_root_c.as_ptr(),
                c_string("second").as_ptr(),
                &mut second_commit_json,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let second: CommitInfo =
        serde_json::from_str(&read_owned_string(&mut second_commit_json)).unwrap();

    let mut snapshots_json = c_abi::e2v_string_t::default();
    let list_code = unsafe {
        c_abi::e2v_list_snapshots_json(sdk, repo_root_c.as_ptr(), &mut snapshots_json, &mut error)
    };
    assert_eq!(list_code, c_abi::E2V_OK);
    let snapshots: Vec<SnapshotInfo> =
        serde_json::from_str(&read_owned_string(&mut snapshots_json)).unwrap();
    assert_eq!(snapshots.len(), 2);
    assert_eq!(snapshots[0].snapshot_id, second.snapshot_id);
    assert_eq!(snapshots[1].snapshot_id, first.snapshot_id);

    let first_snapshot_c = c_string(&first.snapshot_id);
    assert_eq!(
        unsafe {
            c_abi::e2v_verify_snapshot(
                sdk,
                repo_root_c.as_ptr(),
                first_snapshot_c.as_ptr(),
                &mut error,
            )
        },
        c_abi::E2V_OK
    );

    assert_eq!(
        unsafe {
            c_abi::e2v_checkout_snapshot(
                sdk,
                repo_root_c.as_ptr(),
                first_snapshot_c.as_ptr(),
                checkout_dir_c.as_ptr(),
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    assert_eq!(
        fs::read_to_string(checkout_dir.join("tracked.txt")).unwrap(),
        "alpha"
    );

    assert_eq!(
        unsafe {
            c_abi::e2v_change_password(
                sdk,
                repo_root_c.as_ptr(),
                old_password.as_ptr(),
                new_password.as_ptr(),
                &mut error,
            )
        },
        c_abi::E2V_OK
    );

    let mut reopened_json = c_abi::e2v_string_t::default();
    assert_eq!(
        unsafe {
            c_abi::e2v_unlock_repository_json(
                sdk,
                repo_root_c.as_ptr(),
                new_password.as_ptr(),
                &mut reopened_json,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let reopened: RepositoryInfo =
        serde_json::from_str(&read_owned_string(&mut reopened_json)).unwrap();
    assert_eq!(reopened.branch.name, "main");

    unsafe {
        c_abi::e2v_sdk_free(sdk);
    }
}

#[test]
fn c_abi_can_list_checkout_and_delete_branches() {
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let sdk = new_sdk_handle();
    let repo_root_c = path_c_string(&repo_root);
    let mut error = std::ptr::null_mut();
    let mut init_json = c_abi::e2v_string_t::default();

    assert_eq!(
        unsafe {
            c_abi::e2v_init_repository_json(
                sdk,
                repo_root_c.as_ptr(),
                c_string("correct horse battery staple").as_ptr(),
                c_string("main").as_ptr(),
                &mut init_json,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let _repo: RepositoryInfo = serde_json::from_str(&read_owned_string(&mut init_json)).unwrap();

    let mut created_branch_json = c_abi::e2v_string_t::default();
    assert_eq!(
        unsafe {
            c_abi::e2v_create_branch_json(
                sdk,
                repo_root_c.as_ptr(),
                c_string("feature").as_ptr(),
                &mut created_branch_json,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let created: e2v_api::BranchInfo =
        serde_json::from_str(&read_owned_string(&mut created_branch_json)).unwrap();
    assert_eq!(created.name, "feature");

    let mut branches_json = c_abi::e2v_string_t::default();
    assert_eq!(
        unsafe {
            c_abi::e2v_list_branches_json(sdk, repo_root_c.as_ptr(), &mut branches_json, &mut error)
        },
        c_abi::E2V_OK
    );
    let branches: Vec<BranchSummaryInfo> =
        serde_json::from_str(&read_owned_string(&mut branches_json)).unwrap();
    assert!(branches.iter().any(|branch| branch.name == "feature"));

    let mut checkout_json = c_abi::e2v_string_t::default();
    assert_eq!(
        unsafe {
            c_abi::e2v_checkout_branch_json(
                sdk,
                repo_root_c.as_ptr(),
                c_string("feature").as_ptr(),
                &mut checkout_json,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let checked_out: RepositoryInfo =
        serde_json::from_str(&read_owned_string(&mut checkout_json)).unwrap();
    assert_eq!(checked_out.branch.name, "feature");

    let mut back_to_main_json = c_abi::e2v_string_t::default();
    assert_eq!(
        unsafe {
            c_abi::e2v_checkout_branch_json(
                sdk,
                repo_root_c.as_ptr(),
                c_string("main").as_ptr(),
                &mut back_to_main_json,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let _repo: RepositoryInfo =
        serde_json::from_str(&read_owned_string(&mut back_to_main_json)).unwrap();

    assert_eq!(
        unsafe {
            c_abi::e2v_delete_branch(
                sdk,
                repo_root_c.as_ptr(),
                c_string("feature").as_ptr(),
                &mut error,
            )
        },
        c_abi::E2V_OK
    );

    let mut remaining_json = c_abi::e2v_string_t::default();
    assert_eq!(
        unsafe {
            c_abi::e2v_list_branches_json(
                sdk,
                repo_root_c.as_ptr(),
                &mut remaining_json,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let remaining: Vec<BranchSummaryInfo> =
        serde_json::from_str(&read_owned_string(&mut remaining_json)).unwrap();
    assert!(!remaining.iter().any(|branch| branch.name == "feature"));

    unsafe {
        c_abi::e2v_sdk_free(sdk);
    }
}

#[test]
fn c_abi_can_list_and_manage_share_workflows() {
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let sdk = new_sdk_handle();
    let repo_root_c = path_c_string(&repo_root);
    let mut error = std::ptr::null_mut();
    let mut init_json = c_abi::e2v_string_t::default();

    assert_eq!(
        unsafe {
            c_abi::e2v_init_repository_json(
                sdk,
                repo_root_c.as_ptr(),
                c_string("correct horse battery staple").as_ptr(),
                c_string("main").as_ptr(),
                &mut init_json,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let _repo: RepositoryInfo = serde_json::from_str(&read_owned_string(&mut init_json)).unwrap();
    let owner_credential = fs::read(
        repo_root
            .join(".e2v")
            .join("device")
            .join("local-device.json"),
    )
    .unwrap();

    let mut list_json = c_abi::e2v_string_t::default();
    assert_eq!(
        unsafe {
            c_abi::e2v_share_list_json(sdk, repo_root_c.as_ptr(), &mut list_json, &mut error)
        },
        c_abi::E2V_OK
    );
    let listed: ShareListInfo = serde_json::from_str(&read_owned_string(&mut list_json)).unwrap();
    assert_eq!(listed.actors.len(), 1);
    assert_eq!(listed.devices.len(), 1);

    let mut invite_member_json = c_abi::e2v_string_t::default();
    assert_eq!(
        unsafe {
            c_abi::e2v_share_invite_member_json(
                sdk,
                repo_root_c.as_ptr(),
                c_string("Alice").as_ptr(),
                &mut invite_member_json,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let member_invite: ShareInviteInfo =
        serde_json::from_str(&read_owned_string(&mut invite_member_json)).unwrap();

    let mut accept_member_json = c_abi::e2v_string_t::default();
    assert_eq!(
        unsafe {
            c_abi::e2v_share_accept_member_json(
                sdk,
                repo_root_c.as_ptr(),
                member_invite.bundle_bytes.as_ptr(),
                member_invite.bundle_bytes.len(),
                c_string("alice-laptop").as_ptr(),
                &mut accept_member_json,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let accepted_member: ShareAcceptInfo =
        serde_json::from_str(&read_owned_string(&mut accept_member_json)).unwrap();
    assert_eq!(accepted_member.role, "writer_member");

    fs::write(
        repo_root
            .join(".e2v")
            .join("device")
            .join("local-device.json"),
        &owner_credential,
    )
    .unwrap();

    let mut invite_device_json = c_abi::e2v_string_t::default();
    assert_eq!(
        unsafe {
            c_abi::e2v_share_invite_device_json(
                sdk,
                repo_root_c.as_ptr(),
                c_string(&accepted_member.actor_id).as_ptr(),
                c_string("alice-phone").as_ptr(),
                &mut invite_device_json,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let device_invite: ShareInviteInfo =
        serde_json::from_str(&read_owned_string(&mut invite_device_json)).unwrap();

    let mut accept_device_json = c_abi::e2v_string_t::default();
    assert_eq!(
        unsafe {
            c_abi::e2v_share_accept_device_json(
                sdk,
                repo_root_c.as_ptr(),
                device_invite.bundle_bytes.as_ptr(),
                device_invite.bundle_bytes.len(),
                c_string("alice-phone").as_ptr(),
                &mut accept_device_json,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let accepted_device: ShareAcceptInfo =
        serde_json::from_str(&read_owned_string(&mut accept_device_json)).unwrap();
    assert_eq!(accepted_device.actor_id, accepted_member.actor_id);

    fs::write(
        repo_root
            .join(".e2v")
            .join("device")
            .join("local-device.json"),
        &owner_credential,
    )
    .unwrap();

    assert_eq!(
        unsafe {
            c_abi::e2v_share_revoke_device(
                sdk,
                repo_root_c.as_ptr(),
                c_string(&accepted_device.device_id).as_ptr(),
                c_string("correct horse battery staple").as_ptr(),
                &mut error,
            )
        },
        c_abi::E2V_OK
    );

    assert_eq!(
        unsafe {
            c_abi::e2v_share_revoke_member(
                sdk,
                repo_root_c.as_ptr(),
                c_string(&accepted_member.actor_id).as_ptr(),
                c_string("correct horse battery staple").as_ptr(),
                &mut error,
            )
        },
        c_abi::E2V_OK
    );

    unsafe {
        c_abi::e2v_sdk_free(sdk);
    }
}

#[test]
fn c_abi_can_register_remote_and_run_sync_maintenance_flows() {
    let temp = tempfile::tempdir().unwrap();
    let source_repo = temp.path().join("source");
    let clone_repo = temp.path().join("clone");
    let remote_repo = temp.path().join("remote");
    fs::create_dir_all(&source_repo).unwrap();
    fs::create_dir_all(&remote_repo).unwrap();

    let sdk = new_sdk_handle();
    let source_repo_c = path_c_string(&source_repo);
    let clone_repo_c = path_c_string(&clone_repo);
    let remote_spec = format!(
        "file://{}",
        remote_repo.to_string_lossy().replace('\\', "/")
    );
    let remote_spec_c = c_string(&remote_spec);
    let mut error = std::ptr::null_mut();
    let mut init_json = c_abi::e2v_string_t::default();

    assert_eq!(
        unsafe {
            c_abi::e2v_init_repository_json(
                sdk,
                source_repo_c.as_ptr(),
                c_string("correct horse battery staple").as_ptr(),
                c_string("main").as_ptr(),
                &mut init_json,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let repo: RepositoryInfo = serde_json::from_str(&read_owned_string(&mut init_json)).unwrap();

    fs::write(source_repo.join("tracked.txt"), "alpha").unwrap();
    let mut commit_json = c_abi::e2v_string_t::default();
    assert_eq!(
        unsafe {
            c_abi::e2v_commit_repository_json(
                sdk,
                source_repo_c.as_ptr(),
                c_string("seed").as_ptr(),
                &mut commit_json,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let _seed: CommitInfo = serde_json::from_str(&read_owned_string(&mut commit_json)).unwrap();

    let mut parsed_remote_json = c_abi::e2v_string_t::default();
    assert_eq!(
        unsafe {
            c_abi::e2v_parse_remote_spec_json(
                remote_spec_c.as_ptr(),
                &mut parsed_remote_json,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let _parsed: e2v_api::ParsedRemoteSpec =
        serde_json::from_str(&read_owned_string(&mut parsed_remote_json)).unwrap();

    let mut add_remote_json = c_abi::e2v_string_t::default();
    assert_eq!(
        unsafe {
            c_abi::e2v_add_remote_json(
                sdk,
                source_repo_c.as_ptr(),
                c_string("origin").as_ptr(),
                remote_spec_c.as_ptr(),
                &mut add_remote_json,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let added_remote: RemoteRegistration =
        serde_json::from_str(&read_owned_string(&mut add_remote_json)).unwrap();
    assert_eq!(added_remote.name, "origin");

    let mut default_remote_json = c_abi::e2v_string_t::default();
    assert_eq!(
        unsafe {
            c_abi::e2v_load_default_remote_json(
                sdk,
                source_repo_c.as_ptr(),
                &mut default_remote_json,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let default_remote: RemoteRegistration =
        serde_json::from_str(&read_owned_string(&mut default_remote_json)).unwrap();
    assert_eq!(default_remote.spec, remote_spec);

    let mut push_json = c_abi::e2v_string_t::default();
    assert_eq!(
        unsafe {
            c_abi::e2v_push_default_remote_json(
                sdk,
                source_repo_c.as_ptr(),
                c_string(&repo.branch.token_hex).as_ptr(),
                c_string("push-1").as_ptr(),
                &mut push_json,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let _push: e2v_api::PushResponse =
        serde_json::from_str(&read_owned_string(&mut push_json)).unwrap();

    let mut clone_json = c_abi::e2v_string_t::default();
    assert_eq!(
        unsafe {
            c_abi::e2v_clone_remote_json(
                sdk,
                remote_spec_c.as_ptr(),
                clone_repo_c.as_ptr(),
                c_string("correct horse battery staple").as_ptr(),
                c_string(&repo.branch.token_hex).as_ptr(),
                &mut clone_json,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let cloned: e2v_api::CloneResponse =
        serde_json::from_str(&read_owned_string(&mut clone_json)).unwrap();

    let mut clone_remote_reg_json = c_abi::e2v_string_t::default();
    assert_eq!(
        unsafe {
            c_abi::e2v_add_remote_json(
                sdk,
                clone_repo_c.as_ptr(),
                c_string("origin").as_ptr(),
                remote_spec_c.as_ptr(),
                &mut clone_remote_reg_json,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let _remote: RemoteRegistration =
        serde_json::from_str(&read_owned_string(&mut clone_remote_reg_json)).unwrap();

    fs::write(source_repo.join("tracked.txt"), "beta").unwrap();
    let mut second_push_commit_json = c_abi::e2v_string_t::default();
    assert_eq!(
        unsafe {
            c_abi::e2v_commit_repository_json(
                sdk,
                source_repo_c.as_ptr(),
                c_string("second").as_ptr(),
                &mut second_push_commit_json,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let _second: CommitInfo =
        serde_json::from_str(&read_owned_string(&mut second_push_commit_json)).unwrap();

    let mut second_push_json = c_abi::e2v_string_t::default();
    assert_eq!(
        unsafe {
            c_abi::e2v_push_default_remote_json(
                sdk,
                source_repo_c.as_ptr(),
                c_string(&repo.branch.token_hex).as_ptr(),
                c_string("push-2").as_ptr(),
                &mut second_push_json,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let _push: e2v_api::PushResponse =
        serde_json::from_str(&read_owned_string(&mut second_push_json)).unwrap();

    let mut fetch_json = c_abi::e2v_string_t::default();
    assert_eq!(
        unsafe {
            c_abi::e2v_fetch_default_remote_json(
                sdk,
                clone_repo_c.as_ptr(),
                c_string(&cloned.branch_token).as_ptr(),
                c_string("correct horse battery staple").as_ptr(),
                &mut fetch_json,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let _fetch: e2v_api::FetchResponse =
        serde_json::from_str(&read_owned_string(&mut fetch_json)).unwrap();

    let mut pull_json = c_abi::e2v_string_t::default();
    assert_eq!(
        unsafe {
            c_abi::e2v_pull_default_remote_json(
                sdk,
                clone_repo_c.as_ptr(),
                c_string(&cloned.branch_token).as_ptr(),
                c_string("correct horse battery staple").as_ptr(),
                &mut pull_json,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let pulled: e2v_api::PullResponse =
        serde_json::from_str(&read_owned_string(&mut pull_json)).unwrap();
    assert!(!pulled.snapshot_id.is_empty());

    let mut verify_json = c_abi::e2v_string_t::default();
    assert_eq!(
        unsafe {
            c_abi::e2v_verify_default_remote_json(
                sdk,
                source_repo_c.as_ptr(),
                100,
                &mut verify_json,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let verified: e2v_api::VerifyRemoteResponse =
        serde_json::from_str(&read_owned_string(&mut verify_json)).unwrap();
    assert!(verified.sampled_objects > 0);

    let object_path = fs::read_dir(source_repo.join(".e2v").join("objects"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::remove_file(&object_path).unwrap();

    let mut repair_json = c_abi::e2v_string_t::default();
    assert_eq!(
        unsafe {
            c_abi::e2v_repair_default_remote_json(
                sdk,
                source_repo_c.as_ptr(),
                &mut repair_json,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let repaired: e2v_api::RepairRemoteResponse =
        serde_json::from_str(&read_owned_string(&mut repair_json)).unwrap();
    assert!(repaired.repaired_objects > 0);

    fs::write(source_repo.join("tracked.txt"), "gamma").unwrap();
    let mut third_commit_json = c_abi::e2v_string_t::default();
    assert_eq!(
        unsafe {
            c_abi::e2v_commit_repository_json(
                sdk,
                source_repo_c.as_ptr(),
                c_string("local-ahead").as_ptr(),
                &mut third_commit_json,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let _third: CommitInfo =
        serde_json::from_str(&read_owned_string(&mut third_commit_json)).unwrap();

    let mut rollback_json = c_abi::e2v_string_t::default();
    assert_eq!(
        unsafe {
            c_abi::e2v_force_accept_default_remote_rollback_json(
                sdk,
                source_repo_c.as_ptr(),
                c_string("correct horse battery staple").as_ptr(),
                &mut rollback_json,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let _rollback: e2v_api::RepairRemoteResponse =
        serde_json::from_str(&read_owned_string(&mut rollback_json)).unwrap();

    let mut dry_run_json = c_abi::e2v_string_t::default();
    assert_eq!(
        unsafe {
            c_abi::e2v_gc_default_remote_dry_run_json(
                sdk,
                source_repo_c.as_ptr(),
                &mut dry_run_json,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let dry_run: e2v_api::GcDryRunResponse =
        serde_json::from_str(&read_owned_string(&mut dry_run_json)).unwrap();
    assert!(dry_run.unreachable_physical_refs.is_empty());

    let mut gc_execute_json = c_abi::e2v_string_t::default();
    assert_eq!(
        unsafe {
            c_abi::e2v_gc_default_remote_execute_json(
                sdk,
                source_repo_c.as_ptr(),
                30,
                false,
                &mut gc_execute_json,
                &mut error,
            )
        },
        c_abi::E2V_INVALID_ARGUMENT
    );
    assert_eq!(
        unsafe { c_abi::e2v_error_code(error) },
        c_abi::E2V_INVALID_ARGUMENT
    );
    let maintenance_error = error_message_from_ptr(error);
    assert!(
        maintenance_error.contains("maintenance window"),
        "unexpected error: {maintenance_error}"
    );
    unsafe {
        c_abi::e2v_error_free(error);
    }
    error = std::ptr::null_mut();

    assert_eq!(
        unsafe {
            c_abi::e2v_gc_default_remote_execute_json(
                sdk,
                source_repo_c.as_ptr(),
                30,
                true,
                &mut gc_execute_json,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let gc_execute: e2v_api::GcExecuteResponse =
        serde_json::from_str(&read_owned_string(&mut gc_execute_json)).unwrap();
    assert!(gc_execute.deleted_physical_refs.is_empty());

    unsafe {
        c_abi::e2v_sdk_free(sdk);
    }
}

#[test]
fn c_abi_pull_rejects_diverged_local_history_without_moving_the_current_branch() {
    let temp = tempfile::tempdir().unwrap();
    let source_repo = temp.path().join("source");
    let clone_repo = temp.path().join("clone");
    let remote_repo = temp.path().join("remote");
    fs::create_dir_all(&source_repo).unwrap();
    fs::create_dir_all(&remote_repo).unwrap();

    let sdk = new_sdk_handle();
    let source_repo_c = path_c_string(&source_repo);
    let clone_repo_c = path_c_string(&clone_repo);
    let remote_spec = format!(
        "file://{}",
        remote_repo.to_string_lossy().replace('\\', "/")
    );
    let remote_spec_c = c_string(&remote_spec);
    let mut error = std::ptr::null_mut();
    let mut init_json = c_abi::e2v_string_t::default();

    assert_eq!(
        unsafe {
            c_abi::e2v_init_repository_json(
                sdk,
                source_repo_c.as_ptr(),
                c_string("correct horse battery staple").as_ptr(),
                c_string("main").as_ptr(),
                &mut init_json,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let repo: RepositoryInfo = serde_json::from_str(&read_owned_string(&mut init_json)).unwrap();

    fs::write(source_repo.join("tracked.txt"), "alpha").unwrap();
    let mut first_commit_json = c_abi::e2v_string_t::default();
    assert_eq!(
        unsafe {
            c_abi::e2v_commit_repository_json(
                sdk,
                source_repo_c.as_ptr(),
                c_string("first").as_ptr(),
                &mut first_commit_json,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let _first: CommitInfo =
        serde_json::from_str(&read_owned_string(&mut first_commit_json)).unwrap();

    let mut remote_reg_json = c_abi::e2v_string_t::default();
    assert_eq!(
        unsafe {
            c_abi::e2v_add_remote_json(
                sdk,
                source_repo_c.as_ptr(),
                c_string("origin").as_ptr(),
                remote_spec_c.as_ptr(),
                &mut remote_reg_json,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let _remote: RemoteRegistration =
        serde_json::from_str(&read_owned_string(&mut remote_reg_json)).unwrap();

    let mut push_json = c_abi::e2v_string_t::default();
    assert_eq!(
        unsafe {
            c_abi::e2v_push_default_remote_json(
                sdk,
                source_repo_c.as_ptr(),
                c_string(&repo.branch.token_hex).as_ptr(),
                c_string("push-first").as_ptr(),
                &mut push_json,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let _push: e2v_api::PushResponse =
        serde_json::from_str(&read_owned_string(&mut push_json)).unwrap();

    let mut clone_json = c_abi::e2v_string_t::default();
    assert_eq!(
        unsafe {
            c_abi::e2v_clone_remote_json(
                sdk,
                remote_spec_c.as_ptr(),
                clone_repo_c.as_ptr(),
                c_string("correct horse battery staple").as_ptr(),
                c_string(&repo.branch.token_hex).as_ptr(),
                &mut clone_json,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let cloned: e2v_api::CloneResponse =
        serde_json::from_str(&read_owned_string(&mut clone_json)).unwrap();

    let mut clone_remote_reg_json = c_abi::e2v_string_t::default();
    assert_eq!(
        unsafe {
            c_abi::e2v_add_remote_json(
                sdk,
                clone_repo_c.as_ptr(),
                c_string("origin").as_ptr(),
                remote_spec_c.as_ptr(),
                &mut clone_remote_reg_json,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let _clone_remote: RemoteRegistration =
        serde_json::from_str(&read_owned_string(&mut clone_remote_reg_json)).unwrap();

    fs::write(clone_repo.join("tracked.txt"), "local-only").unwrap();
    let mut local_commit_json = c_abi::e2v_string_t::default();
    assert_eq!(
        unsafe {
            c_abi::e2v_commit_repository_json(
                sdk,
                clone_repo_c.as_ptr(),
                c_string("local-only").as_ptr(),
                &mut local_commit_json,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let _local_only: CommitInfo =
        serde_json::from_str(&read_owned_string(&mut local_commit_json)).unwrap();

    fs::write(source_repo.join("tracked.txt"), "remote-only").unwrap();
    let mut remote_commit_json = c_abi::e2v_string_t::default();
    assert_eq!(
        unsafe {
            c_abi::e2v_commit_repository_json(
                sdk,
                source_repo_c.as_ptr(),
                c_string("remote-only").as_ptr(),
                &mut remote_commit_json,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let _remote_only: CommitInfo =
        serde_json::from_str(&read_owned_string(&mut remote_commit_json)).unwrap();

    let mut second_push_json = c_abi::e2v_string_t::default();
    assert_eq!(
        unsafe {
            c_abi::e2v_push_default_remote_json(
                sdk,
                source_repo_c.as_ptr(),
                c_string(&repo.branch.token_hex).as_ptr(),
                c_string("push-remote-only").as_ptr(),
                &mut second_push_json,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let _second_push: e2v_api::PushResponse =
        serde_json::from_str(&read_owned_string(&mut second_push_json)).unwrap();

    let mut pull_json = c_abi::e2v_string_t::default();
    let pull_code = unsafe {
        c_abi::e2v_pull_default_remote_json(
            sdk,
            clone_repo_c.as_ptr(),
            c_string(&cloned.branch_token).as_ptr(),
            c_string("correct horse battery staple").as_ptr(),
            &mut pull_json,
            &mut error,
        )
    };
    assert_eq!(pull_code, c_abi::E2V_NEEDS_REBASE);

    let mut read_handle = std::ptr::null_mut();
    assert_eq!(
        unsafe {
            c_abi::e2v_open_read_handle(sdk, clone_repo_c.as_ptr(), &mut read_handle, &mut error)
        },
        c_abi::E2V_OK
    );
    let mut snapshot_handle = std::ptr::null_mut();
    assert_eq!(
        unsafe {
            c_abi::e2v_resolve_branch(
                read_handle,
                c_string(&cloned.branch_token).as_ptr(),
                &mut snapshot_handle,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let mut file_handle = std::ptr::null_mut();
    assert_eq!(
        unsafe {
            c_abi::e2v_open_file(
                read_handle,
                snapshot_handle,
                c_string("tracked.txt").as_ptr(),
                &mut file_handle,
                &mut error,
            )
        },
        c_abi::E2V_OK
    );
    let mut bytes = c_abi::e2v_bytes_t::default();
    assert_eq!(
        unsafe { c_abi::e2v_read_range(read_handle, file_handle, 0, 32, &mut bytes, &mut error) },
        c_abi::E2V_OK
    );
    assert_eq!(
        String::from_utf8(read_owned_bytes(&mut bytes)).unwrap(),
        "local-only"
    );

    unsafe {
        c_abi::e2v_file_view_free(file_handle);
        c_abi::e2v_snapshot_view_free(snapshot_handle);
        c_abi::e2v_read_handle_free(read_handle);
        c_abi::e2v_sdk_free(sdk);
    }
}

#[test]
fn c_abi_header_matches_generated_contract() {
    assert_eq!(read_header_file(), c_abi::header_text());
}

#[test]
fn c_abi_smoke_program_honors_forced_repo_root_override() {
    let workspace_root = workspace_root();
    let smoke_exe = compile_ffi_smoke_program(&workspace_root);
    let forced_root = workspace_root.join("target").join("ffi-smoke-forced-root");
    remove_dir_if_exists(&forced_root);
    fs::create_dir_all(&forced_root).unwrap();
    fs::write(forced_root.join("stale.txt"), "stale").unwrap();
    remove_legacy_ffi_smoke_roots();

    let run_status = Command::new(smoke_exe)
        .current_dir(&workspace_root)
        .env(
            "E2V_FFI_SMOKE_REPO_ROOT",
            forced_root.to_string_lossy().to_string(),
        )
        .status()
        .unwrap();

    assert!(run_status.success());
    assert!(forced_root.join(".e2v").exists());
}

#[test]
fn c_abi_smoke_program_compiles_links_and_runs() {
    let workspace_root = workspace_root();
    let smoke_exe = compile_ffi_smoke_program(&workspace_root);
    remove_legacy_ffi_smoke_roots();

    let run_status = Command::new(smoke_exe)
        .current_dir(&workspace_root)
        .status()
        .unwrap();
    assert!(run_status.success());
    assert!(
        !workspace_root.join("ffi_smoke.obj").exists(),
        "ffi smoke build should not leave ffi_smoke.obj in the workspace root"
    );
}

#[test]
fn c_abi_smoke_program_can_be_compiled_concurrently() {
    let workspace_root = workspace_root();
    let first_exe = compile_ffi_smoke_program(&workspace_root);
    let second_exe = compile_ffi_smoke_program(&workspace_root);

    assert_ne!(
        first_exe, second_exe,
        "ffi smoke builds must use unique output paths so parallel tests do not race on Windows"
    );

    let first_root = workspace_root.clone();
    let second_root = workspace_root.clone();
    let first = thread::spawn(move || compile_ffi_smoke_program(&first_root));
    let second = thread::spawn(move || compile_ffi_smoke_program(&second_root));

    let first_parallel_exe = first.join().unwrap();
    let second_parallel_exe = second.join().unwrap();

    assert!(first_parallel_exe.exists());
    assert!(second_parallel_exe.exists());
}
