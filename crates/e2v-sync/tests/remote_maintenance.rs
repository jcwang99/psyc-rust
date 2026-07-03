use std::fs;
use std::path::Path;

use e2v_core::{CommitOptions, InitOptions, ManifestStore, ManifestStoreApi, RepositoryFacade};
use e2v_store::{
    BackendCapability, ConsistencyClass, EncryptedRef, LayoutRootStore, RefStore, RefToken,
    StoredRef,
};
use e2v_store::{BlobStore, MemoryBackend};
use tempfile::tempdir;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;

use e2v_sync::{
    CloneOptions, EnableObliviousLayoutOptions, GcDryRunOptions, GcExecuteOptions,
    HistoricalRewriteOptions, HistoricalRewritePlanOptions, PushOptions, RepairRemoteOptions,
    ReshuffleObliviousLayoutOptions, VerifyRemoteOptions, clone_remote, enable_oblivious_layout,
    fetch_remote, force_accept_remote_rollback, gc_dry_run, gc_execute, historical_rewrite_remote,
    plan_historical_rewrite, plan_oblivious_layout, push_head, repair_remote,
    reshuffle_oblivious_layout, status_oblivious_layout, verify_remote,
};

enum UndeletableCacheEntryGuard {
    #[cfg(unix)]
    Permissions { path: PathBuf, original_mode: u32 },
    #[cfg(windows)]
    Locked { _file: std::fs::File },
    #[cfg(not(any(unix, windows)))]
    Noop,
}

impl Drop for UndeletableCacheEntryGuard {
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

fn make_undeletable_cache_entry(path: &Path) -> UndeletableCacheEntryGuard {
    #[cfg(unix)]
    {
        fs::write(path, b"foreign").unwrap();
        let metadata = fs::metadata(path).unwrap();
        let original_mode = metadata.permissions().mode();
        let mut permissions = metadata.permissions();
        permissions.set_mode(0);
        fs::set_permissions(path, permissions).unwrap();
        UndeletableCacheEntryGuard::Permissions {
            path: path.to_path_buf(),
            original_mode,
        }
    }

    #[cfg(windows)]
    {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .share_mode(0)
            .open(path)
            .unwrap();
        UndeletableCacheEntryGuard::Locked { _file: file }
    }

    #[cfg(not(any(unix, windows)))]
    {
        fs::write(path, b"foreign").unwrap();
        UndeletableCacheEntryGuard::Noop
    }
}

#[test]
fn sync_exposes_historical_rewrite_plan_and_execute_api_for_p3_a() {
    let lib_source = fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("lib.rs"),
    )
    .unwrap();
    let maintenance_source = fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("remote_maintenance.rs"),
    )
    .unwrap();

    for required_export in [
        "HistoricalRewritePlan",
        "HistoricalRewritePlanOptions",
        "HistoricalRewriteOptions",
        "HistoricalRewriteResult",
        "historical_rewrite_remote",
        "plan_historical_rewrite",
    ] {
        assert!(
            lib_source.contains(required_export) || maintenance_source.contains(required_export),
            "expected P3-A historical rewrite surface to include {required_export}"
        );
    }
}

#[test]
fn sync_exposes_oblivious_layout_api_for_p3_b() {
    let lib_source = fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("lib.rs"),
    )
    .unwrap();

    for required_export in [
        "ObliviousLayoutPlan",
        "ObliviousLayoutStatus",
        "EnableObliviousLayoutOptions",
        "ReshuffleObliviousLayoutOptions",
        "plan_oblivious_layout",
        "status_oblivious_layout",
        "enable_oblivious_layout",
        "reshuffle_oblivious_layout",
    ] {
        assert!(
            lib_source.contains(required_export),
            "expected P3-B oblivious layout surface to include {required_export}"
        );
    }
}

#[test]
fn historical_rewrite_remote_does_not_expect_initialized_rewrite_state() {
    let maintenance_source = fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("remote_maintenance.rs"),
    )
    .unwrap();

    assert!(
        !maintenance_source.contains("expect(\"rewrite state initialized\")"),
        "historical rewrite should surface checkpoint initialization failures as errors instead of panicking"
    );
}

#[test]
fn plan_oblivious_layout_reports_amplification_and_advisories() {
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
    fs::write(repo_root.join("tracked.txt"), b"hello remote").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-oram-plan".to_string(),
        },
    )
    .unwrap();

    let plan = plan_oblivious_layout(&remote, &repo_root).unwrap();

    assert!(plan.estimated_real_reads_per_request >= 1);
    assert!(plan.estimated_cover_reads_per_request >= 1);
    assert!(plan.estimated_bytes_per_request > 0);
    assert!(plan.requires_layout_root_rewrite);
    assert!(
        plan.advisory_messages
            .iter()
            .any(|message: &String| message.contains("ORAM") || message.contains("oblivious")),
        "expected ORAM advisory copy, saw {:?}",
        plan.advisory_messages
    );
}

#[test]
fn enable_and_reshuffle_oblivious_layout_publish_new_generations() {
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
    fs::write(repo_root.join("tracked.txt"), b"hello remote").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-oram-enable".to_string(),
        },
    )
    .unwrap();

    let enabled = enable_oblivious_layout(
        &remote,
        EnableObliviousLayoutOptions {
            repo_root: repo_root.clone(),
            policy_profile: "balanced".to_string(),
        },
    )
    .unwrap();
    let status_after_enable = status_oblivious_layout(&remote, &repo_root).unwrap();
    let reshuffled = reshuffle_oblivious_layout(
        &remote,
        ReshuffleObliviousLayoutOptions {
            repo_root: repo_root.clone(),
            policy_profile: "balanced".to_string(),
        },
    )
    .unwrap();

    assert_eq!(enabled.layout_mode, "oblivious");
    assert_eq!(enabled.dedup_mode, "generation-scoped-randomized");
    assert!(remote.exists_physical("oblivious/root.json"));
    assert!(status_after_enable.oblivious_generation.is_some());
    assert!(reshuffled.layout_generation > enabled.layout_generation);
    assert!(reshuffled.oblivious_generation.unwrap() > enabled.oblivious_generation.unwrap());
}

#[test]
fn gc_under_oblivious_layout_does_not_require_pack_index_root() {
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
    fs::write(repo_root.join("tracked.txt"), b"hello remote").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-oram-gc".to_string(),
        },
    )
    .unwrap();
    enable_oblivious_layout(
        &remote,
        EnableObliviousLayoutOptions {
            repo_root: repo_root.clone(),
            policy_profile: "balanced".to_string(),
        },
    )
    .unwrap();

    for path in remote.list_physical("objects/").unwrap() {
        remote.delete_physical(&path).unwrap();
    }
    remote.delete_physical("pack-index/root.json").unwrap();
    let stray_object_path =
        "objects/abababababababababababababababababababababababababababababababab.json";
    remote
        .put_physical(stray_object_path, br#"{"garbage":true}"#)
        .unwrap();

    let report = gc_dry_run(
        &remote,
        GcDryRunOptions {
            repo_root: repo_root.clone(),
        },
    )
    .unwrap();

    assert!(
        report
            .unreachable_physical_refs
            .contains(&stray_object_path.to_string()),
        "gc dry-run should still classify stray physical refs under oblivious layout"
    );
}

#[test]
fn verify_remote_sample_repairs_tampered_local_copy_when_remote_object_authenticates() {
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
    let snapshot = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-maintenance".to_string(),
        },
    )
    .unwrap();
    let reachable_object_ids = ManifestStore::new(&repo_root)
        .collect_reachable_object_ids(&snapshot.snapshot_id)
        .unwrap();

    let local_snapshot_path = repo_root
        .join(".e2v")
        .join("objects")
        .join(format!("{}.json", snapshot.snapshot_id));
    let original_bytes = fs::read(&local_snapshot_path).unwrap();
    fs::write(&local_snapshot_path, br#"{"tampered":true}"#).unwrap();

    let verified = verify_remote(
        &remote,
        VerifyRemoteOptions {
            repo_root: repo_root.clone(),
            sample_percent: 100,
        },
    )
    .unwrap();
    let repaired_bytes = fs::read(&local_snapshot_path).unwrap();

    assert_eq!(verified.sampled_objects, reachable_object_ids.len());
    assert_eq!(verified.repaired_local_objects, 1);
    assert_eq!(repaired_bytes, original_bytes);
}

#[test]
fn plan_historical_rewrite_reports_old_epochs_reachable_objects_and_guidance() {
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
    fs::write(repo_root.join("tracked.txt"), b"hello remote").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap();
    e2v_core::testing::rotate_active_epoch_for_test(&repo_root, "correct horse battery staple")
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-history-plan".to_string(),
        },
    )
    .unwrap();

    let plan = plan_historical_rewrite(
        &remote,
        HistoricalRewritePlanOptions {
            repo_root: repo_root.clone(),
        },
    )
    .unwrap();

    assert!(
        plan.reachable_object_count > 0,
        "expected history rewrite planning to find reachable objects"
    );
    assert_eq!(plan.old_epoch_count, 1);
    assert!(plan.requires_remote_credential_revocation_guidance);
    assert!(
        plan.advisory_messages.iter().any(|message| {
            message.contains("remote storage credentials") && message.contains("large repositories")
        }),
        "expected remote credential revocation guidance, saw {:?}",
        plan.advisory_messages
    );
}

#[test]
fn repair_remote_restores_missing_local_object_from_remote_head() {
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
    let snapshot = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap();

    let reachable_object_ids = ManifestStore::new(&repo_root)
        .collect_reachable_object_ids(&snapshot.snapshot_id)
        .unwrap();
    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-repair".to_string(),
        },
    )
    .unwrap();

    let missing_object_id = reachable_object_ids
        .last()
        .cloned()
        .expect("reachable object id");
    let missing_object_path = repo_root
        .join(".e2v")
        .join("objects")
        .join(format!("{missing_object_id}.json"));
    let original_bytes = fs::read(&missing_object_path).unwrap();
    fs::remove_file(&missing_object_path).unwrap();

    let repaired = repair_remote(
        &remote,
        RepairRemoteOptions {
            repo_root: repo_root.clone(),
        },
    )
    .unwrap();

    assert_eq!(repaired.repaired_objects, 1);
    assert_eq!(fs::read(&missing_object_path).unwrap(), original_bytes);
}

#[test]
fn historical_rewrite_remote_retires_local_old_epochs_before_remote_publish() {
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
    fs::write(repo_root.join("tracked.txt"), b"hello remote").unwrap();
    let first_commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap();
    e2v_core::testing::rotate_active_epoch_for_test(&repo_root, "correct horse battery staple")
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-history-exec".to_string(),
        },
    )
    .unwrap();

    let result = historical_rewrite_remote(
        &remote,
        HistoricalRewriteOptions {
            repo_root: repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            confirm_full_reencryption: true,
        },
    )
    .unwrap();

    assert_eq!(result.retired_epoch_count, 1);

    let keyring_dir = repo_root.join(".e2v").join("keyring");
    let pointer: serde_json::Value =
        serde_json::from_slice(&fs::read(keyring_dir.join("keyring.current")).unwrap()).unwrap();
    let current = pointer["current"].as_str().unwrap();
    let keyring: serde_json::Value =
        serde_json::from_slice(&fs::read(keyring_dir.join(current)).unwrap()).unwrap();
    assert_eq!(keyring["epochs"].as_array().unwrap().len(), 1);
    assert_eq!(keyring["active_epoch"].as_u64(), Some(2));

    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&repo_root.join(".e2v"));
    facade.open(&repo_root).unwrap();
    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service
        .open_snapshot(&first_commit.snapshot_id)
        .unwrap();
    let file = read_service.open_file(&snapshot, "tracked.txt").unwrap();
    let bytes = read_service.read_range(&file, 0, 64).unwrap();

    assert_eq!(bytes, b"hello remote");
}

#[test]
fn historical_rewrite_remote_clears_checkpoint_after_successful_publish() {
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
    fs::write(repo_root.join("tracked.txt"), b"hello remote").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap();
    e2v_core::testing::rotate_active_epoch_for_test(&repo_root, "correct horse battery staple")
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-history-checkpoint".to_string(),
        },
    )
    .unwrap();

    let result = historical_rewrite_remote(
        &remote,
        HistoricalRewriteOptions {
            repo_root: repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            confirm_full_reencryption: true,
        },
    )
    .unwrap();

    let journal =
        e2v_sync::OperationJournal::new(repo_root.join(".e2v").join("journal").join("sync"))
            .unwrap();
    let operation_id = e2v_sync::OperationId::new("history-rewrite".to_string()).unwrap();

    assert_eq!(result.next_layout_generation, 2);
    assert!(journal.read_rewrite_state(&operation_id).unwrap().is_none());
    assert!(journal.operation_metadata(&operation_id).unwrap().is_none());
    assert!(journal.pending_objects(&operation_id).unwrap().is_empty());
}

#[test]
fn historical_rewrite_remote_reconcile_keeps_password_unlockable_old_epochs() {
    let temp = tempdir().unwrap();
    let owner_root = temp.path().join("owner");
    let recipient_root = temp.path().join("recipient");
    fs::create_dir_all(&owner_root).unwrap();
    fs::create_dir_all(&recipient_root).unwrap();

    let facade = RepositoryFacade::new();
    let state = facade
        .init(InitOptions {
            repo_root: owner_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
    fs::write(owner_root.join("hello.txt"), b"epoch-one").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: owner_root.clone(),
            message: "epoch-one".to_string(),
        })
        .unwrap();
    let invite = facade
        .share_invite_member(
            &owner_root,
            e2v_core::ShareInviteMemberOptions {
                display_name: "Alice".to_string(),
            },
        )
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: owner_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "history-rewrite-reconcile-bootstrap".to_string(),
        },
    )
    .unwrap();

    facade
        .share_accept_member(
            &recipient_root,
            e2v_core::ShareAcceptMemberOptions {
                invite_bytes: invite.bundle_bytes,
                local_device_label: "alice-laptop".to_string(),
            },
        )
        .unwrap();
    fetch_remote(
        &remote,
        e2v_sync::FetchOptions {
            repo_root: recipient_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            password: None,
        },
    )
    .unwrap();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: recipient_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "history-rewrite-reconcile-recipient-publish".to_string(),
        },
    )
    .unwrap();
    e2v_core::testing::rotate_active_epoch_for_test(&owner_root, "correct horse battery staple")
        .unwrap();

    let remote_keyring_bytes = remote
        .get_physical(&format!(
            "control/keyring/{}",
            serde_json::from_slice::<serde_json::Value>(
                &remote
                    .read_ref(&RefToken::new(format!(
                        "keyring/{}",
                        e2v_core::sync_support::read_repo_id(&owner_root).unwrap()
                    )))
                    .unwrap()
                    .unwrap()
                    .value
                    .bytes
            )
            .unwrap()["current"]
                .as_str()
                .unwrap()
        ))
        .unwrap();
    e2v_core::testing::reconcile_remote_keyring_for_test(&owner_root, &remote_keyring_bytes)
        .unwrap();

    let secrets = e2v_core::sync_support::unlock_repo_secrets_for_sync(
        owner_root.join(".e2v"),
        "correct horse battery staple",
    )
    .unwrap();

    assert!(
        secrets.epoch_keys.contains_key(&1),
        "password unlock should still retain pre-rewrite epoch keys after remote reconcile"
    );
    assert!(secrets.epoch_keys.contains_key(&2));
}

#[test]
fn historical_rewrite_remote_preserves_remote_active_device_envelopes() {
    let temp = tempdir().unwrap();
    let owner_root = temp.path().join("owner");
    let recipient_root = temp.path().join("recipient");
    fs::create_dir_all(&owner_root).unwrap();
    fs::create_dir_all(&recipient_root).unwrap();

    let facade = RepositoryFacade::new();
    let state = facade
        .init(InitOptions {
            repo_root: owner_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
    fs::write(owner_root.join("hello.txt"), b"epoch-one").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: owner_root.clone(),
            message: "epoch-one".to_string(),
        })
        .unwrap();
    let invite = facade
        .share_invite_member(
            &owner_root,
            e2v_core::ShareInviteMemberOptions {
                display_name: "Alice".to_string(),
            },
        )
        .unwrap();
    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: owner_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "history-rewrite-envelope-bootstrap".to_string(),
        },
    )
    .unwrap();

    let accepted = facade
        .share_accept_member(
            &recipient_root,
            e2v_core::ShareAcceptMemberOptions {
                invite_bytes: invite.bundle_bytes,
                local_device_label: "alice-laptop".to_string(),
            },
        )
        .unwrap();
    fetch_remote(
        &remote,
        e2v_sync::FetchOptions {
            repo_root: recipient_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            password: None,
        },
    )
    .unwrap();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: recipient_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "history-rewrite-envelope-recipient-publish".to_string(),
        },
    )
    .unwrap();

    e2v_core::testing::rotate_active_epoch_for_test(&owner_root, "correct horse battery staple")
        .unwrap();
    fs::write(owner_root.join("future.txt"), b"epoch-two").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: owner_root.clone(),
            message: "epoch-two".to_string(),
        })
        .unwrap();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: owner_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "history-rewrite-envelope-owner-rotate".to_string(),
        },
    )
    .unwrap();
    historical_rewrite_remote(
        &remote,
        HistoricalRewriteOptions {
            repo_root: owner_root.clone(),
            password: "correct horse battery staple".to_string(),
            confirm_full_reencryption: true,
        },
    )
    .unwrap();

    let repo_id = e2v_core::sync_support::read_repo_id(&owner_root).unwrap();
    let pointer = remote
        .read_ref(&RefToken::new(format!("keyring/{repo_id}")))
        .unwrap()
        .unwrap();
    let pointer_json: serde_json::Value = serde_json::from_slice(&pointer.value.bytes).unwrap();
    let current = pointer_json["current"].as_str().unwrap();
    let keyring: serde_json::Value = serde_json::from_slice(
        &remote
            .get_physical(&format!("control/keyring/{current}"))
            .unwrap(),
    )
    .unwrap();
    let device_actor_ids = keyring["envelopes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|envelope| envelope["kind"].as_str() == Some("device"))
        .filter_map(|envelope| envelope["actor_id"].as_str())
        .collect::<Vec<_>>();

    assert!(
        device_actor_ids.contains(&accepted.actor_id.as_str()),
        "remote historical rewrite should preserve active shared device envelopes, saw {:?}",
        device_actor_ids
    );
}

#[test]
fn historical_rewrite_remote_publishes_new_pack_view_and_purges_stale_loose_refs() {
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
    fs::write(repo_root.join("tracked.txt"), b"hello rewrite").unwrap();
    let first_commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap();

    let reachable_object_ids = ManifestStore::new(&repo_root)
        .collect_reachable_object_ids(&first_commit.snapshot_id)
        .unwrap();
    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-history-remote-view".to_string(),
        },
    )
    .unwrap();

    let mut old_loose_paths = reachable_object_ids
        .iter()
        .map(|object_id| format!("objects/{object_id}.json"))
        .collect::<Vec<_>>();
    old_loose_paths.sort();
    for path in &old_loose_paths {
        assert!(
            remote.exists_physical(path),
            "expected seed push to publish loose object {path}"
        );
    }

    e2v_core::testing::rotate_active_epoch_for_test(&repo_root, "correct horse battery staple")
        .unwrap();
    let _pack_guard = e2v_sync::testing::override_small_object_pack_threshold_for_test(1);

    let result = historical_rewrite_remote(
        &remote,
        HistoricalRewriteOptions {
            repo_root: repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            confirm_full_reencryption: true,
        },
    )
    .unwrap();

    assert_eq!(result.next_layout_generation, 2);
    assert_eq!(result.deleted_stale_remote_refs, old_loose_paths);
    assert!(
        !repo_root.join(".e2v").join("index.sqlite3").exists(),
        "historical rewrite should invalidate the local sqlite index after publication"
    );

    let pack_root_bytes = remote
        .get_physical("pack-index/root.json")
        .expect("historical rewrite should publish a pack index root");
    let pack_root = e2v_sync::testing::decode_pack_index_root_value_for_test(
        &repo_root.join(".e2v"),
        &pack_root_bytes,
    )
    .unwrap();
    let segments = pack_root["segments"]
        .as_array()
        .expect("segments array")
        .iter()
        .filter_map(|value| value.as_str())
        .map(str::to_string)
        .collect::<Vec<_>>();
    assert!(
        !segments.is_empty(),
        "historical rewrite should publish new pack index segments"
    );
    assert!(
        segments
            .iter()
            .all(|path| path.starts_with("packs/index/history-rewrite-")),
        "historical rewrite should replace the current pack view with rewrite-owned segments: {:?}",
        segments
    );

    for path in &old_loose_paths {
        assert!(
            !remote.exists_physical(path),
            "historical rewrite should purge stale loose object carrier {path}"
        );
    }

    let fetched_root = temp.path().join("fetched");
    clone_remote(
        &remote,
        CloneOptions {
            repo_root: fetched_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_token: state.branch.token_hex.clone(),
        },
    )
    .unwrap();
    let read_service = facade.read_service(&fetched_root).unwrap();
    let snapshot = read_service
        .open_snapshot(&first_commit.snapshot_id)
        .unwrap();
    let file = read_service.open_file(&snapshot, "tracked.txt").unwrap();
    let bytes = read_service.read_range(&file, 0, 64).unwrap();
    assert_eq!(bytes, b"hello rewrite");
}

#[test]
fn historical_rewrite_remote_resumes_after_stale_loose_purge_interruption() {
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
    fs::write(repo_root.join("tracked.txt"), b"hello resume").unwrap();
    let first_commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap();

    let reachable_object_ids = ManifestStore::new(&repo_root)
        .collect_reachable_object_ids(&first_commit.snapshot_id)
        .unwrap();
    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-history-resume".to_string(),
        },
    )
    .unwrap();

    e2v_core::testing::rotate_active_epoch_for_test(&repo_root, "correct horse battery staple")
        .unwrap();
    let _pack_guard = e2v_sync::testing::override_small_object_pack_threshold_for_test(1);

    let mut stale_loose_paths = reachable_object_ids
        .iter()
        .map(|object_id| format!("objects/{object_id}.json"))
        .collect::<Vec<_>>();
    stale_loose_paths.sort();
    let interrupted_remote =
        FailOnceOnDeleteBackend::new(remote.clone(), stale_loose_paths[0].clone());

    let first_error = historical_rewrite_remote(
        &interrupted_remote,
        HistoricalRewriteOptions {
            repo_root: repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            confirm_full_reencryption: true,
        },
    )
    .unwrap_err();
    assert!(
        first_error.to_string().contains("injected delete failure"),
        "unexpected interruption error: {first_error:#}"
    );

    let layout_after_interruption = remote.read_layout_root().unwrap();
    assert_eq!(layout_after_interruption.generation, 2);
    let ref_version_after_interruption = remote
        .read_ref(&RefToken::new(state.branch.token_hex.clone()))
        .unwrap()
        .expect("remote branch ref after interruption")
        .version
        .value;

    let resumed = historical_rewrite_remote(
        &remote,
        HistoricalRewriteOptions {
            repo_root: repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            confirm_full_reencryption: true,
        },
    )
    .unwrap();

    assert_eq!(resumed.next_layout_generation, 2);
    assert_eq!(resumed.deleted_stale_remote_refs, stale_loose_paths);
    assert_eq!(remote.read_layout_root().unwrap().generation, 2);
    assert_eq!(
        remote
            .read_ref(&RefToken::new(state.branch.token_hex.clone()))
            .unwrap()
            .expect("remote branch ref after resume")
            .version
            .value,
        ref_version_after_interruption,
        "resume should not republish the current branch ref once the rewrite view is already published"
    );
    for path in &stale_loose_paths {
        assert!(
            !remote.exists_physical(path),
            "resumed historical rewrite should eventually purge stale loose ref {path}"
        );
    }
}

#[test]
fn historical_rewrite_remote_stores_rewrite_checkpoint_without_plaintext_object_ids_or_stage() {
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
    fs::write(repo_root.join("tracked.txt"), b"hello encrypted checkpoint").unwrap();
    let first_commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap();

    let reachable_object_ids = ManifestStore::new(&repo_root)
        .collect_reachable_object_ids(&first_commit.snapshot_id)
        .unwrap();
    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-history-encrypted-checkpoint".to_string(),
        },
    )
    .unwrap();

    e2v_core::testing::rotate_active_epoch_for_test(&repo_root, "correct horse battery staple")
        .unwrap();
    let _pack_guard = e2v_sync::testing::override_small_object_pack_threshold_for_test(1);
    let stale_path = format!("objects/{}.json", reachable_object_ids[0]);
    let interrupted_remote = FailOnceOnDeleteBackend::new(remote.clone(), stale_path);

    let first_error = historical_rewrite_remote(
        &interrupted_remote,
        HistoricalRewriteOptions {
            repo_root: repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            confirm_full_reencryption: true,
        },
    )
    .unwrap_err();
    assert!(
        first_error.to_string().contains("injected delete failure"),
        "unexpected interruption error: {first_error:#}"
    );

    let checkpoint_bytes = fs::read(
        repo_root
            .join(".e2v")
            .join("journal")
            .join("sync")
            .join("history-rewrite.checkpoint"),
    )
    .unwrap();
    let checkpoint_text = String::from_utf8_lossy(&checkpoint_bytes);
    let sqlite_bytes = fs::read(
        repo_root
            .join(".e2v")
            .join("journal")
            .join("sync")
            .join("operations.sqlite"),
    )
    .unwrap();
    let sqlite_path = repo_root
        .join(".e2v")
        .join("journal")
        .join("sync")
        .join("operations.sqlite");
    let sqlite = rusqlite::Connection::open(&sqlite_path).unwrap();
    let history_rewrite_metadata_rows: i64 = sqlite
        .query_row(
            "SELECT COUNT(*) FROM operation_metadata WHERE operation_id = ?1",
            rusqlite::params!["history-rewrite"],
            |row| row.get(0),
        )
        .unwrap();
    let history_rewrite_object_rows: i64 = sqlite
        .query_row(
            "SELECT COUNT(*) FROM object_states WHERE operation_id = ?1",
            rusqlite::params!["history-rewrite"],
            |row| row.get(0),
        )
        .unwrap();
    let history_rewrite_state_rows: i64 = sqlite
        .query_row(
            "SELECT COUNT(*) FROM rewrite_state WHERE operation_id = ?1",
            rusqlite::params!["history-rewrite"],
            |row| row.get(0),
        )
        .unwrap();

    assert!(
        !checkpoint_text.contains("local_rewrite_completed"),
        "rewrite checkpoint should not leak plaintext stage names"
    );
    for object_id in &reachable_object_ids {
        assert!(
            !checkpoint_text.contains(object_id),
            "rewrite checkpoint should not leak plaintext object id {object_id}"
        );
    }
    assert!(
        !String::from_utf8_lossy(&sqlite_bytes).contains("local_rewrite_completed"),
        "historical rewrite journaling should not leak plaintext stage names into operations sqlite"
    );
    assert_eq!(history_rewrite_metadata_rows, 0);
    assert_eq!(history_rewrite_object_rows, 0);
    assert_eq!(history_rewrite_state_rows, 0);
}

#[test]
fn historical_rewrite_remote_resumes_after_pack_segment_upload_interruption() {
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
    for index in 0..300usize {
        fs::write(
            repo_root.join(format!("tracked-{index:03}.txt")),
            format!("payload-{index:03}"),
        )
        .unwrap();
    }
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-history-pack-resume".to_string(),
        },
    )
    .unwrap();

    e2v_core::testing::rotate_active_epoch_for_test(&repo_root, "correct horse battery staple")
        .unwrap();
    let _pack_guard = e2v_sync::testing::override_small_object_pack_threshold_for_test(1);
    let interrupted_remote =
        FailOnceOnPutBackend::for_history_rewrite_index_batch(remote.clone(), 1);

    let first_error = historical_rewrite_remote(
        &interrupted_remote,
        HistoricalRewriteOptions {
            repo_root: repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            confirm_full_reencryption: true,
        },
    )
    .unwrap_err();
    assert!(
        first_error
            .to_string()
            .contains("injected put failure for history rewrite batch"),
        "unexpected interruption error: {first_error:#}"
    );
    let expected_segment_count = plan_historical_rewrite(
        &remote,
        HistoricalRewritePlanOptions {
            repo_root: repo_root.clone(),
        },
    )
    .unwrap()
    .reachable_object_count
    .div_ceil(256);
    let interrupted_segments = remote
        .list_physical("packs/index/")
        .unwrap()
        .into_iter()
        .filter(|path| path.starts_with("packs/index/history-rewrite-"))
        .collect::<Vec<_>>();
    assert!(
        interrupted_segments.len() < expected_segment_count,
        "interrupted rewrite should leave fewer published segments than the complete rewrite would require"
    );

    let resumed = historical_rewrite_remote(
        &remote,
        HistoricalRewriteOptions {
            repo_root: repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            confirm_full_reencryption: true,
        },
    )
    .unwrap();
    assert!(resumed.rewritten_objects > 256);

    let pack_root_bytes = remote.get_physical("pack-index/root.json").unwrap();
    let pack_root = e2v_sync::testing::decode_pack_index_root_value_for_test(
        &repo_root.join(".e2v"),
        &pack_root_bytes,
    )
    .unwrap();
    let segments = pack_root["segments"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|value| value.as_str())
        .collect::<Vec<_>>();

    assert_eq!(
        segments.len(),
        expected_segment_count,
        "resumed historical rewrite should publish every rewrite-owned segment after a later-batch upload interruption"
    );
    assert!(
        segments
            .iter()
            .all(|path| path.starts_with("packs/index/history-rewrite-")),
        "unexpected rewrite pack segments: {segments:?}"
    );
}

#[test]
fn historical_rewrite_remote_clears_local_rewrite_journal_after_success() {
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
    fs::write(repo_root.join("tracked.txt"), b"hello cleanup").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-history-cleanup".to_string(),
        },
    )
    .unwrap();

    e2v_core::testing::rotate_active_epoch_for_test(&repo_root, "correct horse battery staple")
        .unwrap();
    historical_rewrite_remote(
        &remote,
        HistoricalRewriteOptions {
            repo_root: repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            confirm_full_reencryption: true,
        },
    )
    .unwrap();

    let journal =
        e2v_sync::OperationJournal::new(repo_root.join(".e2v").join("journal").join("sync"))
            .unwrap();
    let operation_id = e2v_sync::OperationId::new("history-rewrite".to_string()).unwrap();

    assert!(journal.read_rewrite_state(&operation_id).unwrap().is_none());
    assert!(journal.operation_metadata(&operation_id).unwrap().is_none());
    assert!(journal.pending_objects(&operation_id).unwrap().is_empty());
}

#[test]
fn force_accept_remote_rollback_rebuilds_local_fact_view_from_remote_head() {
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
    fs::write(repo_root.join("hello.txt"), b"remote base").unwrap();
    let remote_snapshot = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "remote-base".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-rollback-remote-base".to_string(),
        },
    )
    .unwrap();

    fs::write(repo_root.join("hello.txt"), b"local ahead").unwrap();
    let local_snapshot = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "local-ahead".to_string(),
        })
        .unwrap();
    assert_ne!(local_snapshot.snapshot_id, remote_snapshot.snapshot_id);

    let _repaired = force_accept_remote_rollback(
        &remote,
        RepairRemoteOptions {
            repo_root: repo_root.clone(),
        },
        "correct horse battery staple",
    )
    .unwrap();

    let snapshots = facade.snapshots(&repo_root).unwrap();
    assert_eq!(
        snapshots.first().unwrap().snapshot_id,
        remote_snapshot.snapshot_id
    );
    assert_ne!(
        snapshots.first().unwrap().snapshot_id,
        local_snapshot.snapshot_id
    );
    facade.verify_ref(&repo_root).unwrap();
}

#[test]
fn force_accept_remote_rollback_rewrites_current_branch_mirror_to_remote_head() {
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
    fs::write(repo_root.join("hello.txt"), b"remote base").unwrap();
    let remote_snapshot = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "remote-base".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-rollback-branch-mirror".to_string(),
        },
    )
    .unwrap();

    fs::write(repo_root.join("hello.txt"), b"local ahead").unwrap();
    let local_snapshot = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "local-ahead".to_string(),
        })
        .unwrap();
    assert_ne!(local_snapshot.snapshot_id, remote_snapshot.snapshot_id);

    let branches_before = facade.list_branches(&repo_root).unwrap();
    let current_before = branches_before
        .iter()
        .find(|branch| branch.is_current)
        .unwrap();
    assert_eq!(
        current_before.head_snapshot_id.as_deref(),
        Some(local_snapshot.snapshot_id.as_str())
    );

    force_accept_remote_rollback(
        &remote,
        RepairRemoteOptions {
            repo_root: repo_root.clone(),
        },
        "correct horse battery staple",
    )
    .unwrap();

    let branches_after = facade.list_branches(&repo_root).unwrap();
    let current_after = branches_after
        .iter()
        .find(|branch| branch.is_current)
        .unwrap();
    assert_eq!(
        current_after.head_snapshot_id.as_deref(),
        Some(remote_snapshot.snapshot_id.as_str())
    );
    assert_ne!(
        current_after.head_snapshot_id.as_deref(),
        Some(local_snapshot.snapshot_id.as_str())
    );
}

#[test]
fn force_accept_remote_rollback_can_reset_local_high_water_after_explicit_acceptance() {
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
    fs::write(repo_root.join("hello.txt"), b"remote base").unwrap();
    let remote_snapshot = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "remote-base".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-rollback-high-water-reset".to_string(),
        },
    )
    .unwrap();

    let trusted_state_root = temp.path().join("trusted-state");
    fs::create_dir_all(&trusted_state_root).unwrap();
    let _trusted_state_guard =
        e2v_sync::testing::override_trusted_state_dir_for_test(trusted_state_root.clone());
    let remote_keyring_path = repo_root.join(".e2v").join("keyring").join("keyring.1");
    let remote_keyring: serde_json::Value =
        serde_json::from_slice(&fs::read(&remote_keyring_path).unwrap()).unwrap();
    let repo_id = remote_keyring["repo_id"]
        .as_str()
        .expect("remote keyring should contain repo_id");
    fs::write(
        trusted_state_root.join(format!("{repo_id}.json")),
        serde_json::to_vec_pretty(&serde_json::json!({
            "repo_id": repo_id,
            "min_layout_generation": 9u64,
            "min_keyring_generation": 1u64,
            "min_ref_generation": 1u64
        }))
        .unwrap(),
    )
    .unwrap();

    let repaired = force_accept_remote_rollback(
        &remote,
        RepairRemoteOptions {
            repo_root: repo_root.clone(),
        },
        "correct horse battery staple",
    )
    .unwrap();

    assert_eq!(repaired.repaired_objects, 0);
    let snapshots = facade.snapshots(&repo_root).unwrap();
    assert_eq!(
        snapshots.first().unwrap().snapshot_id,
        remote_snapshot.snapshot_id
    );
}

#[test]
fn gc_dry_run_reports_unreachable_remote_loose_object() {
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
            message: "seed".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-gc-dry-run".to_string(),
        },
    )
    .unwrap();

    let stray_object_path =
        "objects/ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff.json";
    remote
        .put_physical(stray_object_path, br#"{"garbage":true}"#)
        .unwrap();

    let report = gc_dry_run(
        &remote,
        GcDryRunOptions {
            repo_root: repo_root.clone(),
        },
    )
    .unwrap();

    assert_eq!(
        report.unreachable_physical_refs,
        vec![stray_object_path.to_string()]
    );
    assert!(report.active_intent_paths.is_empty());
}

#[test]
fn gc_execute_rejects_when_active_intent_exists() {
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
            message: "seed".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-gc-execute".to_string(),
        },
    )
    .unwrap();
    remote
        .put_physical(
            "transactions/active/op-blocking.intent",
            br#"{"operation_id":"op-blocking","target_branch_token":"branch"}"#,
        )
        .unwrap();

    let error = gc_execute(
        &remote,
        GcExecuteOptions {
            repo_root: repo_root.clone(),
            grace_period_days: 30,
            allow_single_writer_maintenance_window: false,
        },
    )
    .unwrap_err();

    assert!(error.to_string().contains("active intent"));
}

#[test]
fn gc_execute_ignores_expired_active_intent_outside_intent_expiry_window() {
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
            message: "seed".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-gc-expired-intent".to_string(),
        },
    )
    .unwrap();
    let stray_object_path =
        "objects/edededededededededededededededededededededededededededededededed.json";
    remote
        .put_physical(stray_object_path, br#"{"garbage":true}"#)
        .unwrap();
    let old_time = std::time::SystemTime::now() - std::time::Duration::from_secs(31 * 24 * 60 * 60);
    e2v_store::testing::override_memory_backend_modified_time(&remote, stray_object_path, old_time)
        .unwrap();
    remote
        .put_physical(
            "transactions/active/op-expired.intent",
            br#"{"operation_id":"op-expired","writer_id":"writer:op-expired","started_at_remote_unix_ms":1,"heartbeat":{"remote_observed_at_unix_ms":1,"sequence":1},"expected_ref_version":null,"target_branch_token":"main","planned_snapshot_id":null,"client_version":"test"}"#,
        )
        .unwrap();
    e2v_store::testing::override_memory_backend_modified_time(
        &remote,
        "transactions/active/op-expired.intent",
        std::time::SystemTime::now() - std::time::Duration::from_secs(73 * 60 * 60),
    )
    .unwrap();

    let result = gc_execute(
        &remote,
        GcExecuteOptions {
            repo_root: repo_root.clone(),
            grace_period_days: 30,
            allow_single_writer_maintenance_window: false,
        },
    )
    .unwrap();

    assert_eq!(
        result.deleted_physical_refs,
        vec![stray_object_path.to_string()]
    );
    assert!(!remote.exists_physical(stray_object_path));
}

#[test]
fn gc_execute_rejects_when_writer_lease_exists() {
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
            message: "seed".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-gc-execute-lease".to_string(),
        },
    )
    .unwrap();
    remote
        .put_physical(
            "leases/main.lock",
            br#"{"operation_id":"op-lease","target_branch_token":"main"}"#,
        )
        .unwrap();

    let error = gc_execute(
        &remote,
        GcExecuteOptions {
            repo_root: repo_root.clone(),
            grace_period_days: 30,
            allow_single_writer_maintenance_window: false,
        },
    )
    .unwrap_err();

    assert!(error.to_string().contains("lease"));
}

#[derive(Clone, Debug)]
struct WeakGcBackend {
    inner: MemoryBackend,
    capability: BackendCapability,
}

impl WeakGcBackend {
    fn new(inner: MemoryBackend) -> Self {
        Self {
            inner,
            capability: BackendCapability {
                supports_conditional_put: false,
                supports_range_read: true,
                supports_atomic_rename: false,
                supports_paged_list: false,
                consistency_class: ConsistencyClass::UnknownOrEventual,
                supports_remote_lock_or_lease: false,
                supports_atomic_create_if_absent: false,
                supports_transaction_markers: false,
                supports_reliable_remote_time: false,
                supports_object_generation_or_etag: false,
                supports_layout_root_cas: false,
                supports_oblivious_access_schedule: false,
            },
        }
    }
}

impl BlobStore for WeakGcBackend {
    fn put_physical(&self, relative_path: &str, bytes: &[u8]) -> anyhow::Result<()> {
        self.inner.put_physical(relative_path, bytes)
    }

    fn put_physical_if_absent(&self, relative_path: &str, bytes: &[u8]) -> anyhow::Result<bool> {
        self.inner.put_physical_if_absent(relative_path, bytes)
    }

    fn get_physical(&self, relative_path: &str) -> anyhow::Result<Vec<u8>> {
        self.inner.get_physical(relative_path)
    }

    fn get_physical_range(
        &self,
        relative_path: &str,
        offset: usize,
        length: usize,
    ) -> anyhow::Result<Vec<u8>> {
        self.inner.get_physical_range(relative_path, offset, length)
    }

    fn delete_physical(&self, relative_path: &str) -> anyhow::Result<()> {
        self.inner.delete_physical(relative_path)
    }

    fn exists_physical(&self, relative_path: &str) -> bool {
        self.inner.exists_physical(relative_path)
    }

    fn stat_physical(&self, relative_path: &str) -> anyhow::Result<e2v_store::ObjectStat> {
        self.inner.stat_physical(relative_path)
    }

    fn list_physical(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
        self.inner.list_physical(prefix)
    }
}

impl RefStore for WeakGcBackend {
    fn read_ref(&self, token: &RefToken) -> anyhow::Result<Option<StoredRef>> {
        self.inner.read_ref(token)
    }

    fn list_refs(&self) -> anyhow::Result<Vec<e2v_store::ListedRef>> {
        self.inner.list_refs()
    }

    fn compare_and_swap_ref(
        &self,
        token: &RefToken,
        expected: Option<e2v_store::RefVersion>,
        next: EncryptedRef,
    ) -> anyhow::Result<e2v_store::CasResult> {
        self.inner.compare_and_swap_ref(token, expected, next)
    }
}

impl LayoutRootStore for WeakGcBackend {
    fn read_layout_root(&self) -> anyhow::Result<e2v_store::LayoutRoot> {
        self.inner.read_layout_root()
    }

    fn compare_and_swap_layout_root(
        &self,
        expected: e2v_store::LayoutRootVersion,
        next: e2v_store::LayoutRoot,
    ) -> anyhow::Result<e2v_store::CasResult> {
        self.inner.compare_and_swap_layout_root(expected, next)
    }

    fn list_retained_layout_roots(&self) -> anyhow::Result<Vec<e2v_store::LayoutRoot>> {
        self.inner.list_retained_layout_roots()
    }
}

impl e2v_store::RemoteBackend for WeakGcBackend {
    fn capability(&self) -> &BackendCapability {
        &self.capability
    }
}

#[derive(Clone, Debug)]
struct IntentAppearsBeforeDeleteBackend {
    inner: MemoryBackend,
    capability: BackendCapability,
    listed_active_once: std::sync::Arc<std::sync::atomic::AtomicBool>,
    injected: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl IntentAppearsBeforeDeleteBackend {
    fn new(inner: MemoryBackend) -> Self {
        Self {
            capability: inner.capability().clone(),
            inner,
            listed_active_once: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            injected: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }
}

impl BlobStore for IntentAppearsBeforeDeleteBackend {
    fn put_physical(&self, relative_path: &str, bytes: &[u8]) -> anyhow::Result<()> {
        self.inner.put_physical(relative_path, bytes)
    }

    fn put_physical_if_absent(&self, relative_path: &str, bytes: &[u8]) -> anyhow::Result<bool> {
        self.inner.put_physical_if_absent(relative_path, bytes)
    }

    fn get_physical(&self, relative_path: &str) -> anyhow::Result<Vec<u8>> {
        self.inner.get_physical(relative_path)
    }

    fn get_physical_range(
        &self,
        relative_path: &str,
        offset: usize,
        length: usize,
    ) -> anyhow::Result<Vec<u8>> {
        self.inner.get_physical_range(relative_path, offset, length)
    }

    fn delete_physical(&self, relative_path: &str) -> anyhow::Result<()> {
        self.inner.delete_physical(relative_path)
    }

    fn exists_physical(&self, relative_path: &str) -> bool {
        self.inner.exists_physical(relative_path)
    }

    fn stat_physical(&self, relative_path: &str) -> anyhow::Result<e2v_store::ObjectStat> {
        self.inner.stat_physical(relative_path)
    }

    fn list_physical(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
        if prefix == "transactions/active/" {
            let saw_first = self
                .listed_active_once
                .swap(true, std::sync::atomic::Ordering::SeqCst);
            if saw_first
                && !self
                    .injected
                    .swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                self.inner.put_physical(
                    "transactions/active/op-raced.intent",
                    br#"{"operation_id":"op-raced","target_branch_token":"branch"}"#,
                )?;
            }
        }
        self.inner.list_physical(prefix)
    }
}

impl RefStore for IntentAppearsBeforeDeleteBackend {
    fn read_ref(&self, token: &RefToken) -> anyhow::Result<Option<StoredRef>> {
        self.inner.read_ref(token)
    }

    fn list_refs(&self) -> anyhow::Result<Vec<e2v_store::ListedRef>> {
        self.inner.list_refs()
    }

    fn compare_and_swap_ref(
        &self,
        token: &RefToken,
        expected: Option<e2v_store::RefVersion>,
        next: EncryptedRef,
    ) -> anyhow::Result<e2v_store::CasResult> {
        self.inner.compare_and_swap_ref(token, expected, next)
    }
}

impl LayoutRootStore for IntentAppearsBeforeDeleteBackend {
    fn read_layout_root(&self) -> anyhow::Result<e2v_store::LayoutRoot> {
        self.inner.read_layout_root()
    }

    fn compare_and_swap_layout_root(
        &self,
        expected: e2v_store::LayoutRootVersion,
        next: e2v_store::LayoutRoot,
    ) -> anyhow::Result<e2v_store::CasResult> {
        self.inner.compare_and_swap_layout_root(expected, next)
    }

    fn list_retained_layout_roots(&self) -> anyhow::Result<Vec<e2v_store::LayoutRoot>> {
        self.inner.list_retained_layout_roots()
    }
}

impl e2v_store::RemoteBackend for IntentAppearsBeforeDeleteBackend {
    fn capability(&self) -> &BackendCapability {
        &self.capability
    }
}

#[derive(Clone, Debug)]
struct LeaseAppearsBeforeDeleteBackend {
    inner: MemoryBackend,
    capability: BackendCapability,
    listed_leases_once: std::sync::Arc<std::sync::atomic::AtomicBool>,
    injected: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl LeaseAppearsBeforeDeleteBackend {
    fn new(inner: MemoryBackend) -> Self {
        Self {
            capability: inner.capability().clone(),
            inner,
            listed_leases_once: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            injected: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }
}

impl BlobStore for LeaseAppearsBeforeDeleteBackend {
    fn put_physical(&self, relative_path: &str, bytes: &[u8]) -> anyhow::Result<()> {
        self.inner.put_physical(relative_path, bytes)
    }

    fn put_physical_if_absent(&self, relative_path: &str, bytes: &[u8]) -> anyhow::Result<bool> {
        self.inner.put_physical_if_absent(relative_path, bytes)
    }

    fn get_physical(&self, relative_path: &str) -> anyhow::Result<Vec<u8>> {
        self.inner.get_physical(relative_path)
    }

    fn get_physical_range(
        &self,
        relative_path: &str,
        offset: usize,
        length: usize,
    ) -> anyhow::Result<Vec<u8>> {
        self.inner.get_physical_range(relative_path, offset, length)
    }

    fn delete_physical(&self, relative_path: &str) -> anyhow::Result<()> {
        self.inner.delete_physical(relative_path)
    }

    fn exists_physical(&self, relative_path: &str) -> bool {
        self.inner.exists_physical(relative_path)
    }

    fn stat_physical(&self, relative_path: &str) -> anyhow::Result<e2v_store::ObjectStat> {
        self.inner.stat_physical(relative_path)
    }

    fn list_physical(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
        if prefix == "leases/" {
            let saw_first = self
                .listed_leases_once
                .swap(true, std::sync::atomic::Ordering::SeqCst);
            if saw_first
                && !self
                    .injected
                    .swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                self.inner.put_physical(
                    "leases/branch.lock",
                    br#"{"operation_id":"op-lease","target_branch_token":"branch"}"#,
                )?;
            }
        }
        self.inner.list_physical(prefix)
    }
}

impl RefStore for LeaseAppearsBeforeDeleteBackend {
    fn read_ref(&self, token: &RefToken) -> anyhow::Result<Option<StoredRef>> {
        self.inner.read_ref(token)
    }

    fn list_refs(&self) -> anyhow::Result<Vec<e2v_store::ListedRef>> {
        self.inner.list_refs()
    }

    fn compare_and_swap_ref(
        &self,
        token: &RefToken,
        expected: Option<e2v_store::RefVersion>,
        next: EncryptedRef,
    ) -> anyhow::Result<e2v_store::CasResult> {
        self.inner.compare_and_swap_ref(token, expected, next)
    }
}

impl LayoutRootStore for LeaseAppearsBeforeDeleteBackend {
    fn read_layout_root(&self) -> anyhow::Result<e2v_store::LayoutRoot> {
        self.inner.read_layout_root()
    }

    fn compare_and_swap_layout_root(
        &self,
        expected: e2v_store::LayoutRootVersion,
        next: e2v_store::LayoutRoot,
    ) -> anyhow::Result<e2v_store::CasResult> {
        self.inner.compare_and_swap_layout_root(expected, next)
    }

    fn list_retained_layout_roots(&self) -> anyhow::Result<Vec<e2v_store::LayoutRoot>> {
        self.inner.list_retained_layout_roots()
    }
}

impl e2v_store::RemoteBackend for LeaseAppearsBeforeDeleteBackend {
    fn capability(&self) -> &BackendCapability {
        &self.capability
    }
}

#[derive(Clone, Debug)]
struct PackIndexRootChangesBeforeDeleteBackend {
    inner: MemoryBackend,
    capability: BackendCapability,
    listed_active_once: std::sync::Arc<std::sync::atomic::AtomicBool>,
    mutated: std::sync::Arc<std::sync::atomic::AtomicBool>,
    replacement_root_bytes: Vec<u8>,
}

impl PackIndexRootChangesBeforeDeleteBackend {
    fn new(inner: MemoryBackend, replacement_root_bytes: Vec<u8>) -> Self {
        Self {
            capability: inner.capability().clone(),
            inner,
            listed_active_once: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            mutated: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            replacement_root_bytes,
        }
    }
}

impl BlobStore for PackIndexRootChangesBeforeDeleteBackend {
    fn put_physical(&self, relative_path: &str, bytes: &[u8]) -> anyhow::Result<()> {
        self.inner.put_physical(relative_path, bytes)
    }

    fn put_physical_if_absent(&self, relative_path: &str, bytes: &[u8]) -> anyhow::Result<bool> {
        self.inner.put_physical_if_absent(relative_path, bytes)
    }

    fn get_physical(&self, relative_path: &str) -> anyhow::Result<Vec<u8>> {
        self.inner.get_physical(relative_path)
    }

    fn get_physical_range(
        &self,
        relative_path: &str,
        offset: usize,
        length: usize,
    ) -> anyhow::Result<Vec<u8>> {
        self.inner.get_physical_range(relative_path, offset, length)
    }

    fn delete_physical(&self, relative_path: &str) -> anyhow::Result<()> {
        self.inner.delete_physical(relative_path)
    }

    fn exists_physical(&self, relative_path: &str) -> bool {
        self.inner.exists_physical(relative_path)
    }

    fn stat_physical(&self, relative_path: &str) -> anyhow::Result<e2v_store::ObjectStat> {
        self.inner.stat_physical(relative_path)
    }

    fn list_physical(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
        if prefix == "transactions/active/" {
            let saw_first = self
                .listed_active_once
                .swap(true, std::sync::atomic::Ordering::SeqCst);
            if saw_first && !self.mutated.swap(true, std::sync::atomic::Ordering::SeqCst) {
                self.inner
                    .put_physical("pack-index/root.json", &self.replacement_root_bytes)?;
            }
        }
        self.inner.list_physical(prefix)
    }
}

impl RefStore for PackIndexRootChangesBeforeDeleteBackend {
    fn read_ref(&self, token: &RefToken) -> anyhow::Result<Option<StoredRef>> {
        self.inner.read_ref(token)
    }

    fn list_refs(&self) -> anyhow::Result<Vec<e2v_store::ListedRef>> {
        self.inner.list_refs()
    }

    fn compare_and_swap_ref(
        &self,
        token: &RefToken,
        expected: Option<e2v_store::RefVersion>,
        next: EncryptedRef,
    ) -> anyhow::Result<e2v_store::CasResult> {
        self.inner.compare_and_swap_ref(token, expected, next)
    }
}

impl LayoutRootStore for PackIndexRootChangesBeforeDeleteBackend {
    fn read_layout_root(&self) -> anyhow::Result<e2v_store::LayoutRoot> {
        self.inner.read_layout_root()
    }

    fn compare_and_swap_layout_root(
        &self,
        expected: e2v_store::LayoutRootVersion,
        next: e2v_store::LayoutRoot,
    ) -> anyhow::Result<e2v_store::CasResult> {
        self.inner.compare_and_swap_layout_root(expected, next)
    }

    fn list_retained_layout_roots(&self) -> anyhow::Result<Vec<e2v_store::LayoutRoot>> {
        self.inner.list_retained_layout_roots()
    }
}

impl e2v_store::RemoteBackend for PackIndexRootChangesBeforeDeleteBackend {
    fn capability(&self) -> &BackendCapability {
        &self.capability
    }
}

#[derive(Clone, Debug)]
struct PackIndexRootDisappearsDuringFenceBackend {
    inner: MemoryBackend,
    capability: BackendCapability,
    listed_active_once: std::sync::Arc<std::sync::atomic::AtomicBool>,
    delete_on_next_pack_index_root_get: std::sync::Arc<std::sync::atomic::AtomicBool>,
    deleted_once: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl PackIndexRootDisappearsDuringFenceBackend {
    fn new(inner: MemoryBackend) -> Self {
        Self {
            capability: inner.capability().clone(),
            inner,
            listed_active_once: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            delete_on_next_pack_index_root_get: std::sync::Arc::new(
                std::sync::atomic::AtomicBool::new(false),
            ),
            deleted_once: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }
}

impl BlobStore for PackIndexRootDisappearsDuringFenceBackend {
    fn put_physical(&self, relative_path: &str, bytes: &[u8]) -> anyhow::Result<()> {
        self.inner.put_physical(relative_path, bytes)
    }

    fn put_physical_if_absent(&self, relative_path: &str, bytes: &[u8]) -> anyhow::Result<bool> {
        self.inner.put_physical_if_absent(relative_path, bytes)
    }

    fn get_physical(&self, relative_path: &str) -> anyhow::Result<Vec<u8>> {
        if relative_path == "pack-index/root.json"
            && self
                .delete_on_next_pack_index_root_get
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            && !self
                .deleted_once
                .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            self.inner.delete_physical(relative_path)?;
        }
        self.inner.get_physical(relative_path)
    }

    fn get_physical_range(
        &self,
        relative_path: &str,
        offset: usize,
        length: usize,
    ) -> anyhow::Result<Vec<u8>> {
        self.inner.get_physical_range(relative_path, offset, length)
    }

    fn delete_physical(&self, relative_path: &str) -> anyhow::Result<()> {
        self.inner.delete_physical(relative_path)
    }

    fn exists_physical(&self, relative_path: &str) -> bool {
        self.inner.exists_physical(relative_path)
    }

    fn stat_physical(&self, relative_path: &str) -> anyhow::Result<e2v_store::ObjectStat> {
        self.inner.stat_physical(relative_path)
    }

    fn list_physical(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
        if prefix == "transactions/active/" {
            let saw_first = self
                .listed_active_once
                .swap(true, std::sync::atomic::Ordering::SeqCst);
            if saw_first {
                self.delete_on_next_pack_index_root_get
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        self.inner.list_physical(prefix)
    }
}

impl RefStore for PackIndexRootDisappearsDuringFenceBackend {
    fn read_ref(&self, token: &RefToken) -> anyhow::Result<Option<StoredRef>> {
        self.inner.read_ref(token)
    }

    fn list_refs(&self) -> anyhow::Result<Vec<e2v_store::ListedRef>> {
        self.inner.list_refs()
    }

    fn compare_and_swap_ref(
        &self,
        token: &RefToken,
        expected: Option<e2v_store::RefVersion>,
        next: EncryptedRef,
    ) -> anyhow::Result<e2v_store::CasResult> {
        self.inner.compare_and_swap_ref(token, expected, next)
    }
}

impl LayoutRootStore for PackIndexRootDisappearsDuringFenceBackend {
    fn read_layout_root(&self) -> anyhow::Result<e2v_store::LayoutRoot> {
        self.inner.read_layout_root()
    }

    fn compare_and_swap_layout_root(
        &self,
        expected: e2v_store::LayoutRootVersion,
        next: e2v_store::LayoutRoot,
    ) -> anyhow::Result<e2v_store::CasResult> {
        self.inner.compare_and_swap_layout_root(expected, next)
    }

    fn list_retained_layout_roots(&self) -> anyhow::Result<Vec<e2v_store::LayoutRoot>> {
        self.inner.list_retained_layout_roots()
    }
}

impl e2v_store::RemoteBackend for PackIndexRootDisappearsDuringFenceBackend {
    fn capability(&self) -> &BackendCapability {
        &self.capability
    }
}

#[derive(Clone, Debug)]
struct LayoutRootBytesChangeBeforeDeleteBackend {
    inner: MemoryBackend,
    capability: BackendCapability,
    listed_active_once: std::sync::Arc<std::sync::atomic::AtomicBool>,
    mutated: std::sync::Arc<std::sync::atomic::AtomicBool>,
    replacement_layout_root_bytes: Vec<u8>,
}

impl LayoutRootBytesChangeBeforeDeleteBackend {
    fn new(inner: MemoryBackend, replacement_layout_root_bytes: Vec<u8>) -> Self {
        Self {
            capability: inner.capability().clone(),
            inner,
            listed_active_once: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            mutated: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            replacement_layout_root_bytes,
        }
    }
}

impl BlobStore for LayoutRootBytesChangeBeforeDeleteBackend {
    fn put_physical(&self, relative_path: &str, bytes: &[u8]) -> anyhow::Result<()> {
        self.inner.put_physical(relative_path, bytes)
    }

    fn put_physical_if_absent(&self, relative_path: &str, bytes: &[u8]) -> anyhow::Result<bool> {
        self.inner.put_physical_if_absent(relative_path, bytes)
    }

    fn get_physical(&self, relative_path: &str) -> anyhow::Result<Vec<u8>> {
        self.inner.get_physical(relative_path)
    }

    fn get_physical_range(
        &self,
        relative_path: &str,
        offset: usize,
        length: usize,
    ) -> anyhow::Result<Vec<u8>> {
        self.inner.get_physical_range(relative_path, offset, length)
    }

    fn delete_physical(&self, relative_path: &str) -> anyhow::Result<()> {
        self.inner.delete_physical(relative_path)
    }

    fn exists_physical(&self, relative_path: &str) -> bool {
        self.inner.exists_physical(relative_path)
    }

    fn stat_physical(&self, relative_path: &str) -> anyhow::Result<e2v_store::ObjectStat> {
        self.inner.stat_physical(relative_path)
    }

    fn list_physical(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
        if prefix == "transactions/active/" {
            let saw_first = self
                .listed_active_once
                .swap(true, std::sync::atomic::Ordering::SeqCst);
            if saw_first && !self.mutated.swap(true, std::sync::atomic::Ordering::SeqCst) {
                self.inner
                    .put_physical("layout_root.json", &self.replacement_layout_root_bytes)?;
            }
        }
        self.inner.list_physical(prefix)
    }
}

impl RefStore for LayoutRootBytesChangeBeforeDeleteBackend {
    fn read_ref(&self, token: &RefToken) -> anyhow::Result<Option<StoredRef>> {
        self.inner.read_ref(token)
    }

    fn list_refs(&self) -> anyhow::Result<Vec<e2v_store::ListedRef>> {
        self.inner.list_refs()
    }

    fn compare_and_swap_ref(
        &self,
        token: &RefToken,
        expected: Option<e2v_store::RefVersion>,
        next: EncryptedRef,
    ) -> anyhow::Result<e2v_store::CasResult> {
        self.inner.compare_and_swap_ref(token, expected, next)
    }
}

impl LayoutRootStore for LayoutRootBytesChangeBeforeDeleteBackend {
    fn read_layout_root(&self) -> anyhow::Result<e2v_store::LayoutRoot> {
        self.inner.read_layout_root()
    }

    fn compare_and_swap_layout_root(
        &self,
        expected: e2v_store::LayoutRootVersion,
        next: e2v_store::LayoutRoot,
    ) -> anyhow::Result<e2v_store::CasResult> {
        self.inner.compare_and_swap_layout_root(expected, next)
    }

    fn list_retained_layout_roots(&self) -> anyhow::Result<Vec<e2v_store::LayoutRoot>> {
        self.inner.list_retained_layout_roots()
    }
}

impl e2v_store::RemoteBackend for LayoutRootBytesChangeBeforeDeleteBackend {
    fn capability(&self) -> &BackendCapability {
        &self.capability
    }
}

#[derive(Clone, Debug)]
struct FailOnceOnDeleteBackend {
    inner: MemoryBackend,
    capability: BackendCapability,
    target_path: String,
    failed_once: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl FailOnceOnDeleteBackend {
    fn new(inner: MemoryBackend, target_path: impl Into<String>) -> Self {
        Self {
            capability: inner.capability().clone(),
            inner,
            target_path: target_path.into(),
            failed_once: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }
}

impl BlobStore for FailOnceOnDeleteBackend {
    fn put_physical(&self, relative_path: &str, bytes: &[u8]) -> anyhow::Result<()> {
        self.inner.put_physical(relative_path, bytes)
    }

    fn put_physical_if_absent(&self, relative_path: &str, bytes: &[u8]) -> anyhow::Result<bool> {
        self.inner.put_physical_if_absent(relative_path, bytes)
    }

    fn get_physical(&self, relative_path: &str) -> anyhow::Result<Vec<u8>> {
        self.inner.get_physical(relative_path)
    }

    fn get_physical_range(
        &self,
        relative_path: &str,
        offset: usize,
        length: usize,
    ) -> anyhow::Result<Vec<u8>> {
        self.inner.get_physical_range(relative_path, offset, length)
    }

    fn delete_physical(&self, relative_path: &str) -> anyhow::Result<()> {
        if relative_path == self.target_path
            && !self
                .failed_once
                .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            anyhow::bail!("injected delete failure for {relative_path}");
        }
        self.inner.delete_physical(relative_path)
    }

    fn exists_physical(&self, relative_path: &str) -> bool {
        self.inner.exists_physical(relative_path)
    }

    fn stat_physical(&self, relative_path: &str) -> anyhow::Result<e2v_store::ObjectStat> {
        self.inner.stat_physical(relative_path)
    }

    fn list_physical(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
        self.inner.list_physical(prefix)
    }
}

impl RefStore for FailOnceOnDeleteBackend {
    fn read_ref(&self, token: &RefToken) -> anyhow::Result<Option<StoredRef>> {
        self.inner.read_ref(token)
    }

    fn list_refs(&self) -> anyhow::Result<Vec<e2v_store::ListedRef>> {
        self.inner.list_refs()
    }

    fn compare_and_swap_ref(
        &self,
        token: &RefToken,
        expected: Option<e2v_store::RefVersion>,
        next: EncryptedRef,
    ) -> anyhow::Result<e2v_store::CasResult> {
        self.inner.compare_and_swap_ref(token, expected, next)
    }
}

impl LayoutRootStore for FailOnceOnDeleteBackend {
    fn read_layout_root(&self) -> anyhow::Result<e2v_store::LayoutRoot> {
        self.inner.read_layout_root()
    }

    fn compare_and_swap_layout_root(
        &self,
        expected: e2v_store::LayoutRootVersion,
        next: e2v_store::LayoutRoot,
    ) -> anyhow::Result<e2v_store::CasResult> {
        self.inner.compare_and_swap_layout_root(expected, next)
    }

    fn list_retained_layout_roots(&self) -> anyhow::Result<Vec<e2v_store::LayoutRoot>> {
        self.inner.list_retained_layout_roots()
    }
}

impl e2v_store::RemoteBackend for FailOnceOnDeleteBackend {
    fn capability(&self) -> &BackendCapability {
        &self.capability
    }
}

#[derive(Clone, Debug)]
struct FailOnceOnPutBackend {
    inner: MemoryBackend,
    capability: BackendCapability,
    target_path: String,
    failed_once: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl FailOnceOnPutBackend {
    fn for_history_rewrite_index_batch(inner: MemoryBackend, batch_index: usize) -> Self {
        Self {
            capability: inner.capability().clone(),
            inner,
            target_path: format!("packs/index/history-rewrite-{batch_index:08}.json"),
            failed_once: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }
}

impl BlobStore for FailOnceOnPutBackend {
    fn put_physical(&self, relative_path: &str, bytes: &[u8]) -> anyhow::Result<()> {
        if relative_path == self.target_path
            && !self
                .failed_once
                .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            anyhow::bail!("injected put failure for history rewrite batch {relative_path}");
        }
        self.inner.put_physical(relative_path, bytes)
    }

    fn put_physical_if_absent(&self, relative_path: &str, bytes: &[u8]) -> anyhow::Result<bool> {
        self.inner.put_physical_if_absent(relative_path, bytes)
    }

    fn get_physical(&self, relative_path: &str) -> anyhow::Result<Vec<u8>> {
        self.inner.get_physical(relative_path)
    }

    fn get_physical_range(
        &self,
        relative_path: &str,
        offset: usize,
        length: usize,
    ) -> anyhow::Result<Vec<u8>> {
        self.inner.get_physical_range(relative_path, offset, length)
    }

    fn delete_physical(&self, relative_path: &str) -> anyhow::Result<()> {
        self.inner.delete_physical(relative_path)
    }

    fn exists_physical(&self, relative_path: &str) -> bool {
        self.inner.exists_physical(relative_path)
    }

    fn stat_physical(&self, relative_path: &str) -> anyhow::Result<e2v_store::ObjectStat> {
        self.inner.stat_physical(relative_path)
    }

    fn list_physical(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
        self.inner.list_physical(prefix)
    }
}

impl RefStore for FailOnceOnPutBackend {
    fn read_ref(&self, token: &RefToken) -> anyhow::Result<Option<StoredRef>> {
        self.inner.read_ref(token)
    }

    fn list_refs(&self) -> anyhow::Result<Vec<e2v_store::ListedRef>> {
        self.inner.list_refs()
    }

    fn compare_and_swap_ref(
        &self,
        token: &RefToken,
        expected: Option<e2v_store::RefVersion>,
        next: EncryptedRef,
    ) -> anyhow::Result<e2v_store::CasResult> {
        self.inner.compare_and_swap_ref(token, expected, next)
    }
}

impl LayoutRootStore for FailOnceOnPutBackend {
    fn read_layout_root(&self) -> anyhow::Result<e2v_store::LayoutRoot> {
        self.inner.read_layout_root()
    }

    fn compare_and_swap_layout_root(
        &self,
        expected: e2v_store::LayoutRootVersion,
        next: e2v_store::LayoutRoot,
    ) -> anyhow::Result<e2v_store::CasResult> {
        self.inner.compare_and_swap_layout_root(expected, next)
    }

    fn list_retained_layout_roots(&self) -> anyhow::Result<Vec<e2v_store::LayoutRoot>> {
        self.inner.list_retained_layout_roots()
    }
}

impl e2v_store::RemoteBackend for FailOnceOnPutBackend {
    fn capability(&self) -> &BackendCapability {
        &self.capability
    }
}

#[derive(Clone, Debug)]
struct DisappearBeforeDeleteBackend {
    inner: MemoryBackend,
    capability: BackendCapability,
    target_path: String,
    deleted_on_stat: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl DisappearBeforeDeleteBackend {
    fn new(inner: MemoryBackend, target_path: impl Into<String>) -> Self {
        Self {
            capability: inner.capability().clone(),
            inner,
            target_path: target_path.into(),
            deleted_on_stat: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }
}

impl BlobStore for DisappearBeforeDeleteBackend {
    fn put_physical(&self, relative_path: &str, bytes: &[u8]) -> anyhow::Result<()> {
        self.inner.put_physical(relative_path, bytes)
    }

    fn put_physical_if_absent(&self, relative_path: &str, bytes: &[u8]) -> anyhow::Result<bool> {
        self.inner.put_physical_if_absent(relative_path, bytes)
    }

    fn get_physical(&self, relative_path: &str) -> anyhow::Result<Vec<u8>> {
        self.inner.get_physical(relative_path)
    }

    fn get_physical_range(
        &self,
        relative_path: &str,
        offset: usize,
        length: usize,
    ) -> anyhow::Result<Vec<u8>> {
        self.inner.get_physical_range(relative_path, offset, length)
    }

    fn delete_physical(&self, relative_path: &str) -> anyhow::Result<()> {
        self.inner.delete_physical(relative_path)
    }

    fn exists_physical(&self, relative_path: &str) -> bool {
        self.inner.exists_physical(relative_path)
    }

    fn stat_physical(&self, relative_path: &str) -> anyhow::Result<e2v_store::ObjectStat> {
        if relative_path == self.target_path
            && !self
                .deleted_on_stat
                .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            self.inner.delete_physical(relative_path)?;
        }
        self.inner.stat_physical(relative_path)
    }

    fn list_physical(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
        self.inner.list_physical(prefix)
    }
}

impl RefStore for DisappearBeforeDeleteBackend {
    fn read_ref(&self, token: &RefToken) -> anyhow::Result<Option<StoredRef>> {
        self.inner.read_ref(token)
    }

    fn list_refs(&self) -> anyhow::Result<Vec<e2v_store::ListedRef>> {
        self.inner.list_refs()
    }

    fn compare_and_swap_ref(
        &self,
        token: &RefToken,
        expected: Option<e2v_store::RefVersion>,
        next: EncryptedRef,
    ) -> anyhow::Result<e2v_store::CasResult> {
        self.inner.compare_and_swap_ref(token, expected, next)
    }
}

impl LayoutRootStore for DisappearBeforeDeleteBackend {
    fn read_layout_root(&self) -> anyhow::Result<e2v_store::LayoutRoot> {
        self.inner.read_layout_root()
    }

    fn compare_and_swap_layout_root(
        &self,
        expected: e2v_store::LayoutRootVersion,
        next: e2v_store::LayoutRoot,
    ) -> anyhow::Result<e2v_store::CasResult> {
        self.inner.compare_and_swap_layout_root(expected, next)
    }

    fn list_retained_layout_roots(&self) -> anyhow::Result<Vec<e2v_store::LayoutRoot>> {
        self.inner.list_retained_layout_roots()
    }
}

impl e2v_store::RemoteBackend for DisappearBeforeDeleteBackend {
    fn capability(&self) -> &BackendCapability {
        &self.capability
    }
}

#[derive(Clone, Debug)]
struct DisappearDuringDeleteBackend {
    inner: MemoryBackend,
    capability: BackendCapability,
    target_path: String,
    deleted_once: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl DisappearDuringDeleteBackend {
    fn new(inner: MemoryBackend, target_path: impl Into<String>) -> Self {
        Self {
            capability: inner.capability().clone(),
            inner,
            target_path: target_path.into(),
            deleted_once: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }
}

impl BlobStore for DisappearDuringDeleteBackend {
    fn put_physical(&self, relative_path: &str, bytes: &[u8]) -> anyhow::Result<()> {
        self.inner.put_physical(relative_path, bytes)
    }

    fn put_physical_if_absent(&self, relative_path: &str, bytes: &[u8]) -> anyhow::Result<bool> {
        self.inner.put_physical_if_absent(relative_path, bytes)
    }

    fn get_physical(&self, relative_path: &str) -> anyhow::Result<Vec<u8>> {
        self.inner.get_physical(relative_path)
    }

    fn get_physical_range(
        &self,
        relative_path: &str,
        offset: usize,
        length: usize,
    ) -> anyhow::Result<Vec<u8>> {
        self.inner.get_physical_range(relative_path, offset, length)
    }

    fn delete_physical(&self, relative_path: &str) -> anyhow::Result<()> {
        if relative_path == self.target_path
            && !self
                .deleted_once
                .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            self.inner.delete_physical(relative_path)?;
            return Err(anyhow::Error::new(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("simulated delete race for {relative_path}"),
            )));
        }
        self.inner.delete_physical(relative_path)
    }

    fn exists_physical(&self, relative_path: &str) -> bool {
        self.inner.exists_physical(relative_path)
    }

    fn stat_physical(&self, relative_path: &str) -> anyhow::Result<e2v_store::ObjectStat> {
        self.inner.stat_physical(relative_path)
    }

    fn list_physical(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
        self.inner.list_physical(prefix)
    }
}

impl RefStore for DisappearDuringDeleteBackend {
    fn read_ref(&self, token: &RefToken) -> anyhow::Result<Option<StoredRef>> {
        self.inner.read_ref(token)
    }

    fn list_refs(&self) -> anyhow::Result<Vec<e2v_store::ListedRef>> {
        self.inner.list_refs()
    }

    fn compare_and_swap_ref(
        &self,
        token: &RefToken,
        expected: Option<e2v_store::RefVersion>,
        next: EncryptedRef,
    ) -> anyhow::Result<e2v_store::CasResult> {
        self.inner.compare_and_swap_ref(token, expected, next)
    }
}

impl LayoutRootStore for DisappearDuringDeleteBackend {
    fn read_layout_root(&self) -> anyhow::Result<e2v_store::LayoutRoot> {
        self.inner.read_layout_root()
    }

    fn compare_and_swap_layout_root(
        &self,
        expected: e2v_store::LayoutRootVersion,
        next: e2v_store::LayoutRoot,
    ) -> anyhow::Result<e2v_store::CasResult> {
        self.inner.compare_and_swap_layout_root(expected, next)
    }

    fn list_retained_layout_roots(&self) -> anyhow::Result<Vec<e2v_store::LayoutRoot>> {
        self.inner.list_retained_layout_roots()
    }
}

impl e2v_store::RemoteBackend for DisappearDuringDeleteBackend {
    fn capability(&self) -> &BackendCapability {
        &self.capability
    }
}

#[derive(Clone, Debug)]
struct RangeReadTrackingBackend {
    inner: MemoryBackend,
    capability: BackendCapability,
    range_read_paths: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl RangeReadTrackingBackend {
    fn new(inner: MemoryBackend) -> Self {
        Self {
            capability: inner.capability().clone(),
            inner,
            range_read_paths: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    fn range_read_paths(&self) -> Vec<String> {
        self.range_read_paths.lock().unwrap().clone()
    }

    fn reset_range_reads(&self) {
        self.range_read_paths.lock().unwrap().clear();
    }
}

impl BlobStore for RangeReadTrackingBackend {
    fn put_physical(&self, relative_path: &str, bytes: &[u8]) -> anyhow::Result<()> {
        self.inner.put_physical(relative_path, bytes)
    }

    fn put_physical_if_absent(&self, relative_path: &str, bytes: &[u8]) -> anyhow::Result<bool> {
        self.inner.put_physical_if_absent(relative_path, bytes)
    }

    fn get_physical(&self, relative_path: &str) -> anyhow::Result<Vec<u8>> {
        self.inner.get_physical(relative_path)
    }

    fn get_physical_range(
        &self,
        relative_path: &str,
        offset: usize,
        length: usize,
    ) -> anyhow::Result<Vec<u8>> {
        self.range_read_paths
            .lock()
            .unwrap()
            .push(relative_path.to_string());
        self.inner.get_physical_range(relative_path, offset, length)
    }

    fn delete_physical(&self, relative_path: &str) -> anyhow::Result<()> {
        self.inner.delete_physical(relative_path)
    }

    fn exists_physical(&self, relative_path: &str) -> bool {
        self.inner.exists_physical(relative_path)
    }

    fn stat_physical(&self, relative_path: &str) -> anyhow::Result<e2v_store::ObjectStat> {
        self.inner.stat_physical(relative_path)
    }

    fn list_physical(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
        self.inner.list_physical(prefix)
    }
}

impl RefStore for RangeReadTrackingBackend {
    fn read_ref(&self, token: &RefToken) -> anyhow::Result<Option<StoredRef>> {
        self.inner.read_ref(token)
    }

    fn list_refs(&self) -> anyhow::Result<Vec<e2v_store::ListedRef>> {
        self.inner.list_refs()
    }

    fn compare_and_swap_ref(
        &self,
        token: &RefToken,
        expected: Option<e2v_store::RefVersion>,
        next: EncryptedRef,
    ) -> anyhow::Result<e2v_store::CasResult> {
        self.inner.compare_and_swap_ref(token, expected, next)
    }
}

impl LayoutRootStore for RangeReadTrackingBackend {
    fn read_layout_root(&self) -> anyhow::Result<e2v_store::LayoutRoot> {
        self.inner.read_layout_root()
    }

    fn compare_and_swap_layout_root(
        &self,
        expected: e2v_store::LayoutRootVersion,
        next: e2v_store::LayoutRoot,
    ) -> anyhow::Result<e2v_store::CasResult> {
        self.inner.compare_and_swap_layout_root(expected, next)
    }

    fn list_retained_layout_roots(&self) -> anyhow::Result<Vec<e2v_store::LayoutRoot>> {
        self.inner.list_retained_layout_roots()
    }
}

impl e2v_store::RemoteBackend for RangeReadTrackingBackend {
    fn capability(&self) -> &BackendCapability {
        &self.capability
    }
}

#[test]
fn gc_execute_rejects_weak_backend_capabilities() {
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
            message: "seed".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-gc-weak".to_string(),
        },
    )
    .unwrap();
    let weak_remote = WeakGcBackend::new(remote);

    let error = gc_execute(
        &weak_remote,
        GcExecuteOptions {
            repo_root: repo_root.clone(),
            grace_period_days: 30,
            allow_single_writer_maintenance_window: true,
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("capability") || error.to_string().contains("weak"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn gc_execute_rejects_backend_without_lease_or_transaction_markers() {
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
            message: "seed".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-gc-missing-fencing".to_string(),
        },
    )
    .unwrap();
    let weak_remote = WeakGcBackend {
        inner: remote,
        capability: BackendCapability {
            supports_conditional_put: true,
            supports_range_read: true,
            supports_atomic_rename: true,
            supports_paged_list: true,
            consistency_class: ConsistencyClass::StrongWhitelisted,
            supports_remote_lock_or_lease: false,
            supports_atomic_create_if_absent: false,
            supports_transaction_markers: false,
            supports_reliable_remote_time: true,
            supports_object_generation_or_etag: true,
            supports_layout_root_cas: true,
            supports_oblivious_access_schedule: false,
        },
    };

    let error = gc_execute(
        &weak_remote,
        GcExecuteOptions {
            repo_root: repo_root.clone(),
            grace_period_days: 30,
            allow_single_writer_maintenance_window: false,
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("lease")
            || error.to_string().contains("transaction")
            || error.to_string().contains("capability"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn gc_execute_rejects_single_writer_backend_without_explicit_maintenance_window() {
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
            message: "seed".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-gc-single-writer-window".to_string(),
        },
    )
    .unwrap();
    let single_writer_remote = WeakGcBackend {
        inner: remote,
        capability: BackendCapability {
            supports_conditional_put: false,
            supports_range_read: true,
            supports_atomic_rename: true,
            supports_paged_list: true,
            consistency_class: ConsistencyClass::StrongWhitelisted,
            supports_remote_lock_or_lease: true,
            supports_atomic_create_if_absent: true,
            supports_transaction_markers: true,
            supports_reliable_remote_time: true,
            supports_object_generation_or_etag: true,
            supports_layout_root_cas: true,
            supports_oblivious_access_schedule: false,
        },
    };

    let error = gc_execute(
        &single_writer_remote,
        GcExecuteOptions {
            repo_root: repo_root.clone(),
            grace_period_days: 30,
            allow_single_writer_maintenance_window: false,
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("maintenance window"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn gc_execute_deletes_unreachable_remote_loose_object_when_safe() {
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
            message: "seed".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-gc-execute-delete".to_string(),
        },
    )
    .unwrap();

    let stray_object_path =
        "objects/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee.json";
    remote
        .put_physical(stray_object_path, br#"{"garbage":true}"#)
        .unwrap();
    e2v_store::testing::override_memory_backend_modified_time(
        &remote,
        stray_object_path,
        std::time::SystemTime::now() - std::time::Duration::from_secs(31 * 24 * 60 * 60),
    )
    .unwrap();

    let result = gc_execute(
        &remote,
        GcExecuteOptions {
            repo_root: repo_root.clone(),
            grace_period_days: 30,
            allow_single_writer_maintenance_window: true,
        },
    )
    .unwrap();

    assert_eq!(
        result.deleted_physical_refs,
        vec![stray_object_path.to_string()]
    );
    assert!(!remote.exists_physical(stray_object_path));
}

#[test]
fn gc_execute_does_not_probe_candidate_existence_before_statting_it() {
    #[derive(Clone, Debug)]
    struct CandidateExistsProbeCountingBackend {
        capability: BackendCapability,
        inner: MemoryBackend,
        target_path: &'static str,
        target_exists_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        target_stat_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl CandidateExistsProbeCountingBackend {
        fn new(inner: MemoryBackend, target_path: &'static str) -> Self {
            Self {
                capability: inner.capability().clone(),
                inner,
                target_path,
                target_exists_calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                target_stat_calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
        }

        fn target_exists_calls(&self) -> usize {
            self.target_exists_calls
                .load(std::sync::atomic::Ordering::SeqCst)
        }

        fn target_stat_calls(&self) -> usize {
            self.target_stat_calls
                .load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl BlobStore for CandidateExistsProbeCountingBackend {
        fn put_physical(&self, relative_path: &str, bytes: &[u8]) -> anyhow::Result<()> {
            self.inner.put_physical(relative_path, bytes)
        }

        fn put_physical_if_absent(
            &self,
            relative_path: &str,
            bytes: &[u8],
        ) -> anyhow::Result<bool> {
            self.inner.put_physical_if_absent(relative_path, bytes)
        }

        fn get_physical(&self, relative_path: &str) -> anyhow::Result<Vec<u8>> {
            self.inner.get_physical(relative_path)
        }

        fn get_physical_range(
            &self,
            relative_path: &str,
            offset: usize,
            length: usize,
        ) -> anyhow::Result<Vec<u8>> {
            self.inner.get_physical_range(relative_path, offset, length)
        }

        fn delete_physical(&self, relative_path: &str) -> anyhow::Result<()> {
            self.inner.delete_physical(relative_path)
        }

        fn exists_physical(&self, relative_path: &str) -> bool {
            if relative_path == self.target_path {
                self.target_exists_calls
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            self.inner.exists_physical(relative_path)
        }

        fn stat_physical(&self, relative_path: &str) -> anyhow::Result<e2v_store::ObjectStat> {
            if relative_path == self.target_path {
                self.target_stat_calls
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            self.inner.stat_physical(relative_path)
        }

        fn list_physical(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
            self.inner.list_physical(prefix)
        }
    }

    impl RefStore for CandidateExistsProbeCountingBackend {
        fn read_ref(&self, token: &RefToken) -> anyhow::Result<Option<StoredRef>> {
            self.inner.read_ref(token)
        }

        fn list_refs(&self) -> anyhow::Result<Vec<e2v_store::ListedRef>> {
            self.inner.list_refs()
        }

        fn compare_and_swap_ref(
            &self,
            token: &RefToken,
            expected: Option<e2v_store::RefVersion>,
            next: EncryptedRef,
        ) -> anyhow::Result<e2v_store::CasResult> {
            self.inner.compare_and_swap_ref(token, expected, next)
        }
    }

    impl LayoutRootStore for CandidateExistsProbeCountingBackend {
        fn read_layout_root(&self) -> anyhow::Result<e2v_store::LayoutRoot> {
            self.inner.read_layout_root()
        }

        fn compare_and_swap_layout_root(
            &self,
            expected: e2v_store::LayoutRootVersion,
            next: e2v_store::LayoutRoot,
        ) -> anyhow::Result<e2v_store::CasResult> {
            self.inner.compare_and_swap_layout_root(expected, next)
        }

        fn list_retained_layout_roots(&self) -> anyhow::Result<Vec<e2v_store::LayoutRoot>> {
            self.inner.list_retained_layout_roots()
        }
    }

    impl e2v_store::RemoteBackend for CandidateExistsProbeCountingBackend {
        fn capability(&self) -> &BackendCapability {
            &self.capability
        }
    }

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
            message: "seed".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-gc-candidate-probe".to_string(),
        },
    )
    .unwrap();

    let stray_object_path =
        "objects/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee.json";
    remote
        .put_physical(stray_object_path, br#"{"garbage":true}"#)
        .unwrap();
    e2v_store::testing::override_memory_backend_modified_time(
        &remote,
        stray_object_path,
        std::time::SystemTime::now() - std::time::Duration::from_secs(31 * 24 * 60 * 60),
    )
    .unwrap();

    let counted = CandidateExistsProbeCountingBackend::new(remote.clone(), stray_object_path);

    let result = gc_execute(
        &counted,
        GcExecuteOptions {
            repo_root: repo_root.clone(),
            grace_period_days: 30,
            allow_single_writer_maintenance_window: true,
        },
    )
    .unwrap();

    assert_eq!(
        result.deleted_physical_refs,
        vec![stray_object_path.to_string()]
    );
    assert!(
        counted.target_stat_calls() > 0,
        "gc execute should still stat a candidate before deleting it"
    );
    assert_eq!(
        counted.target_exists_calls(),
        0,
        "gc execute should not probe candidate existence before statting it"
    );
}

#[test]
fn gc_execute_keeps_recent_unreachable_object_within_grace_period() {
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
            message: "seed".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-gc-execute-grace".to_string(),
        },
    )
    .unwrap();

    let stray_object_path =
        "objects/dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd.json";
    remote
        .put_physical(stray_object_path, br#"{"garbage":true}"#)
        .unwrap();

    let result = gc_execute(
        &remote,
        GcExecuteOptions {
            repo_root: repo_root.clone(),
            grace_period_days: 30,
            allow_single_writer_maintenance_window: true,
        },
    )
    .unwrap();

    assert!(result.deleted_physical_refs.is_empty());
    assert!(remote.exists_physical(stray_object_path));
}

#[test]
fn gc_execute_aborts_when_active_intent_appears_after_dry_run() {
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
            message: "seed".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-gc-raced-intent".to_string(),
        },
    )
    .unwrap();
    let stray_object_path =
        "objects/cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc.json";
    remote
        .put_physical(stray_object_path, br#"{"garbage":true}"#)
        .unwrap();
    e2v_store::testing::override_memory_backend_modified_time(
        &remote,
        stray_object_path,
        std::time::SystemTime::now() - std::time::Duration::from_secs(31 * 24 * 60 * 60),
    )
    .unwrap();

    let raced_remote = IntentAppearsBeforeDeleteBackend::new(remote.clone());

    let error = gc_execute(
        &raced_remote,
        GcExecuteOptions {
            repo_root: repo_root.clone(),
            grace_period_days: 30,
            allow_single_writer_maintenance_window: true,
        },
    )
    .unwrap_err();

    assert!(error.to_string().contains("active intent"));
    assert!(remote.exists_physical(stray_object_path));
}

#[test]
fn gc_execute_aborts_when_writer_lease_appears_after_dry_run() {
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
            message: "seed".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-gc-raced-lease".to_string(),
        },
    )
    .unwrap();
    let stray_object_path =
        "objects/1212121212121212121212121212121212121212121212121212121212121212.json";
    remote
        .put_physical(stray_object_path, br#"{"garbage":true}"#)
        .unwrap();
    e2v_store::testing::override_memory_backend_modified_time(
        &remote,
        stray_object_path,
        std::time::SystemTime::now() - std::time::Duration::from_secs(31 * 24 * 60 * 60),
    )
    .unwrap();

    let raced_remote = LeaseAppearsBeforeDeleteBackend::new(remote.clone());

    let error = gc_execute(
        &raced_remote,
        GcExecuteOptions {
            repo_root: repo_root.clone(),
            grace_period_days: 30,
            allow_single_writer_maintenance_window: true,
        },
    )
    .unwrap_err();

    assert!(error.to_string().contains("lease") || error.to_string().contains("changed"));
    assert!(remote.exists_physical(stray_object_path));
}

#[test]
fn gc_execute_resumes_from_local_deletion_journal_after_partial_failure() {
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
            message: "seed".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-gc-delete-journal".to_string(),
        },
    )
    .unwrap();

    let stray_one = "objects/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.json";
    let stray_two = "objects/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb.json";
    remote.put_physical(stray_one, br#"{"garbage":1}"#).unwrap();
    remote.put_physical(stray_two, br#"{"garbage":2}"#).unwrap();
    let old_time = std::time::SystemTime::now() - std::time::Duration::from_secs(31 * 24 * 60 * 60);
    e2v_store::testing::override_memory_backend_modified_time(&remote, stray_one, old_time)
        .unwrap();
    e2v_store::testing::override_memory_backend_modified_time(&remote, stray_two, old_time)
        .unwrap();

    let flaky_remote = FailOnceOnDeleteBackend::new(remote.clone(), stray_two);
    let first_error = gc_execute(
        &flaky_remote,
        GcExecuteOptions {
            repo_root: repo_root.clone(),
            grace_period_days: 30,
            allow_single_writer_maintenance_window: true,
        },
    )
    .unwrap_err();
    assert!(first_error.to_string().contains("delete failure"));
    assert!(!remote.exists_physical(stray_one));
    assert!(remote.exists_physical(stray_two));

    let journal_path = repo_root
        .join(".e2v")
        .join("journal")
        .join("gc")
        .join("gc-execute.json");
    assert!(
        journal_path.is_file(),
        "gc delete journal should be retained after partial failure"
    );

    let resumed = gc_execute(
        &remote,
        GcExecuteOptions {
            repo_root: repo_root.clone(),
            grace_period_days: 30,
            allow_single_writer_maintenance_window: true,
        },
    )
    .unwrap();
    assert_eq!(resumed.deleted_physical_refs, vec![stray_two.to_string()]);
    assert!(!remote.exists_physical(stray_two));
    assert!(
        !journal_path.exists(),
        "gc delete journal should be removed after successful resume"
    );
}

#[test]
fn gc_execute_ignores_candidate_that_disappears_before_delete() {
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
            message: "seed".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-gc-disappearing-candidate".to_string(),
        },
    )
    .unwrap();

    let stray_object_path =
        "objects/cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc.json";
    remote
        .put_physical(stray_object_path, br#"{"garbage":true}"#)
        .unwrap();
    e2v_store::testing::override_memory_backend_modified_time(
        &remote,
        stray_object_path,
        std::time::SystemTime::now() - std::time::Duration::from_secs(31 * 24 * 60 * 60),
    )
    .unwrap();

    let raced_remote = DisappearBeforeDeleteBackend::new(remote.clone(), stray_object_path);

    let result = gc_execute(
        &raced_remote,
        GcExecuteOptions {
            repo_root: repo_root.clone(),
            grace_period_days: 30,
            allow_single_writer_maintenance_window: true,
        },
    );

    assert!(
        result.is_ok(),
        "unexpected error when candidate disappeared before delete: {result:#?}"
    );
    assert!(
        !remote.exists_physical(stray_object_path),
        "disappearing candidate should remain absent after gc execute"
    );
}

#[test]
fn gc_execute_ignores_candidate_that_disappears_during_delete() {
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
            message: "seed".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-gc-disappearing-during-delete".to_string(),
        },
    )
    .unwrap();

    let stray_object_path =
        "objects/dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd.json";
    remote
        .put_physical(stray_object_path, br#"{"garbage":true}"#)
        .unwrap();
    e2v_store::testing::override_memory_backend_modified_time(
        &remote,
        stray_object_path,
        std::time::SystemTime::now() - std::time::Duration::from_secs(31 * 24 * 60 * 60),
    )
    .unwrap();

    let raced_remote = DisappearDuringDeleteBackend::new(remote.clone(), stray_object_path);

    let result = gc_execute(
        &raced_remote,
        GcExecuteOptions {
            repo_root: repo_root.clone(),
            grace_period_days: 30,
            allow_single_writer_maintenance_window: true,
        },
    );

    assert!(
        result.is_ok(),
        "unexpected error when candidate disappeared during delete: {result:#?}"
    );
    assert!(
        !remote.exists_physical(stray_object_path),
        "candidate should remain absent after disappearing during delete"
    );
}

#[test]
fn gc_dry_run_keeps_objects_reachable_from_other_remote_branch_refs() {
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
    fs::write(repo_root.join("base.txt"), b"base").unwrap();
    let base = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "base".to_string(),
        })
        .unwrap();
    facade.create_branch(&repo_root, "feature").unwrap();
    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-gc-all-refs-first".to_string(),
        },
    )
    .unwrap();

    let feature_checkout = facade.checkout_branch(&repo_root, "feature").unwrap();
    fs::write(repo_root.join("feature.txt"), b"feature only").unwrap();
    let feature = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "feature".to_string(),
        })
        .unwrap();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: feature_checkout.branch.token_hex.clone(),
            operation_id: "push-op-gc-all-refs-feature".to_string(),
        },
    )
    .unwrap();

    facade.checkout_branch(&repo_root, "main").unwrap();
    fs::write(repo_root.join("main.txt"), b"main only").unwrap();
    let main = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "main".to_string(),
        })
        .unwrap();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-gc-all-refs-main".to_string(),
        },
    )
    .unwrap();

    let base_reachable = ManifestStore::new(&repo_root)
        .collect_reachable_object_ids(&base.snapshot_id)
        .unwrap();
    let main_reachable = ManifestStore::new(&repo_root)
        .collect_reachable_object_ids(&main.snapshot_id)
        .unwrap();
    let feature_reachable = ManifestStore::new(&repo_root)
        .collect_reachable_object_ids(&feature.snapshot_id)
        .unwrap();
    let feature_only_object_id = feature_reachable
        .iter()
        .find(|object_id| {
            !main_reachable.contains(*object_id) && !base_reachable.contains(*object_id)
        })
        .cloned()
        .expect("object only reachable from feature branch");

    let report = gc_dry_run(
        &remote,
        GcDryRunOptions {
            repo_root: repo_root.clone(),
        },
    )
    .unwrap();

    assert!(
        !report
            .unreachable_physical_refs
            .contains(&format!("objects/{feature_only_object_id}.json")),
        "gc dry-run should respect all remote refs, not just the local default branch"
    );
}

#[test]
fn gc_dry_run_keeps_recent_unpublished_local_snapshot_chain_and_ancestors() {
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
    fs::write(repo_root.join("base.txt"), b"base").unwrap();
    let base = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "base".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-gc-unpublished-base".to_string(),
        },
    )
    .unwrap();

    fs::write(repo_root.join("second.txt"), b"second only").unwrap();
    let second = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "second".to_string(),
        })
        .unwrap();
    fs::write(repo_root.join("third.txt"), b"third only").unwrap();
    let third = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "third".to_string(),
        })
        .unwrap();

    let manifest_store = ManifestStore::new(&repo_root);
    let base_reachable = manifest_store
        .collect_reachable_object_ids(&base.snapshot_id)
        .unwrap();
    let second_reachable = manifest_store
        .collect_reachable_object_ids(&second.snapshot_id)
        .unwrap();
    let third_reachable = manifest_store
        .collect_reachable_object_ids(&third.snapshot_id)
        .unwrap();
    let second_snapshot = manifest_store.get_snapshot(&second.snapshot_id).unwrap();
    let third_snapshot = manifest_store.get_snapshot(&third.snapshot_id).unwrap();

    upload_local_objects_to_remote(&remote, &repo_root, &second_reachable);
    let old_time = std::time::SystemTime::now() - std::time::Duration::from_secs(31 * 24 * 60 * 60);
    for object_id in &second_reachable {
        e2v_store::testing::override_memory_backend_modified_time(
            &remote,
            &format!("objects/{object_id}.json"),
            old_time,
        )
        .unwrap();
    }
    upload_local_objects_to_remote(&remote, &repo_root, &third_reachable);

    let second_only_object_id = second_reachable
        .iter()
        .find(|object_id| {
            !base_reachable.contains(*object_id) && !third_reachable.contains(*object_id)
        })
        .cloned()
        .expect("object only reachable from the unpublished ancestor snapshot");
    let third_only_object_id = third_reachable
        .iter()
        .find(|object_id| !second_reachable.contains(*object_id))
        .cloned()
        .expect("object only reachable from the recent unpublished head snapshot");

    let report = gc_dry_run(
        &remote,
        GcDryRunOptions {
            repo_root: repo_root.clone(),
        },
    )
    .unwrap();

    assert!(
        !report
            .unreachable_physical_refs
            .contains(&format!("objects/{}.json", second.snapshot_id)),
        "gc dry-run should keep an unpublished ancestor snapshot while a recent local descendant still depends on it"
    );
    assert!(
        !report
            .unreachable_physical_refs
            .contains(&format!("objects/{}.json", second_snapshot.root_tree_id)),
        "gc dry-run should keep ancestor tree objects needed by a recent unpublished descendant"
    );
    assert!(
        !report
            .unreachable_physical_refs
            .contains(&format!("objects/{second_only_object_id}.json")),
        "gc dry-run should not report objects that are only reachable from an unpublished ancestor chain"
    );
    assert!(
        !report
            .unreachable_physical_refs
            .contains(&format!("objects/{}.json", third.snapshot_id)),
        "gc dry-run should keep a recent unpublished head snapshot"
    );
    assert_eq!(
        third_snapshot.parent_snapshot_id.as_deref(),
        Some(second.snapshot_id.as_str())
    );
    assert!(
        !report
            .unreachable_physical_refs
            .contains(&format!("objects/{third_only_object_id}.json")),
        "gc dry-run should not report objects that are only reachable from a recent unpublished head snapshot"
    );
}

#[test]
fn gc_dry_run_allows_expired_unpublished_local_snapshot_chain_to_be_collected() {
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
    fs::write(repo_root.join("base.txt"), b"base").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "base".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-gc-expired-unpublished-base".to_string(),
        },
    )
    .unwrap();

    fs::write(repo_root.join("second.txt"), b"second only").unwrap();
    let second = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "second".to_string(),
        })
        .unwrap();
    fs::write(repo_root.join("third.txt"), b"third only").unwrap();
    let third = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "third".to_string(),
        })
        .unwrap();

    let manifest_store = ManifestStore::new(&repo_root);
    let second_reachable = manifest_store
        .collect_reachable_object_ids(&second.snapshot_id)
        .unwrap();
    let third_reachable = manifest_store
        .collect_reachable_object_ids(&third.snapshot_id)
        .unwrap();
    upload_local_objects_to_remote(&remote, &repo_root, &second_reachable);
    upload_local_objects_to_remote(&remote, &repo_root, &third_reachable);

    let old_time = std::time::SystemTime::now() - std::time::Duration::from_secs(31 * 24 * 60 * 60);
    for object_id in second_reachable.iter().chain(third_reachable.iter()) {
        e2v_store::testing::override_memory_backend_modified_time(
            &remote,
            &format!("objects/{object_id}.json"),
            old_time,
        )
        .unwrap();
    }

    let report = gc_dry_run(
        &remote,
        GcDryRunOptions {
            repo_root: repo_root.clone(),
        },
    )
    .unwrap();

    assert!(
        report
            .unreachable_physical_refs
            .contains(&format!("objects/{}.json", second.snapshot_id)),
        "gc dry-run should allow an expired unpublished ancestor snapshot to be collected"
    );
    assert!(
        report
            .unreachable_physical_refs
            .contains(&format!("objects/{}.json", third.snapshot_id)),
        "gc dry-run should allow an expired unpublished head snapshot to be collected"
    );
}

#[test]
fn gc_dry_run_reports_unreferenced_pack_index_segments_after_compaction() {
    let _guard = e2v_sync::testing::override_small_object_pack_threshold_for_test(1);
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
    let remote = MemoryBackend::new();

    for version in 0..5usize {
        fs::write(repo_root.join("rolling.txt"), format!("rolling-{version}")).unwrap();
        facade
            .commit(CommitOptions {
                repo_root: repo_root.clone(),
                message: format!("gc-pack-index-bound-{version}"),
            })
            .unwrap();
        push_head(
            &facade,
            &remote,
            PushOptions {
                repo_root: repo_root.clone(),
                branch_token: state.branch.token_hex.clone(),
                operation_id: format!("gc-pack-index-bound-op-{version}"),
            },
        )
        .unwrap();
    }

    let root = e2v_sync::testing::decode_pack_index_root_value_for_test(
        &repo_root.join(".e2v"),
        &remote.get_physical("pack-index/root.json").unwrap(),
    )
    .unwrap();
    let referenced_segments = root["segments"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|value| value.as_str())
        .map(str::to_string)
        .collect::<std::collections::BTreeSet<_>>();
    let unreferenced_segments = remote
        .list_physical("packs/index/")
        .unwrap()
        .into_iter()
        .chain(remote.list_physical("pack-index/segments/").unwrap())
        .filter(|path| !referenced_segments.contains(path))
        .collect::<Vec<_>>();
    assert!(
        !unreferenced_segments.is_empty(),
        "expected compaction to leave older pack index segments behind for gc to collect"
    );

    let report = gc_dry_run(
        &remote,
        GcDryRunOptions {
            repo_root: repo_root.clone(),
        },
    )
    .unwrap();

    for segment_path in &unreferenced_segments {
        assert!(
            report.unreachable_physical_refs.contains(segment_path),
            "gc dry-run should report unreferenced pack index segment {segment_path}, saw {:?}",
            report.unreachable_physical_refs
        );
    }
}

#[test]
fn gc_execute_with_pack_index_root_does_not_probe_root_existence_before_statting_it() {
    #[derive(Clone, Debug)]
    struct PackIndexRootExistsProbeCountingBackend {
        capability: BackendCapability,
        inner: MemoryBackend,
        pack_index_root_exists_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        pack_index_root_stat_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl PackIndexRootExistsProbeCountingBackend {
        fn new(inner: MemoryBackend) -> Self {
            Self {
                capability: inner.capability().clone(),
                inner,
                pack_index_root_exists_calls: std::sync::Arc::new(
                    std::sync::atomic::AtomicUsize::new(0),
                ),
                pack_index_root_stat_calls: std::sync::Arc::new(
                    std::sync::atomic::AtomicUsize::new(0),
                ),
            }
        }

        fn pack_index_root_exists_calls(&self) -> usize {
            self.pack_index_root_exists_calls
                .load(std::sync::atomic::Ordering::SeqCst)
        }

        fn pack_index_root_stat_calls(&self) -> usize {
            self.pack_index_root_stat_calls
                .load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl BlobStore for PackIndexRootExistsProbeCountingBackend {
        fn put_physical(&self, relative_path: &str, bytes: &[u8]) -> anyhow::Result<()> {
            self.inner.put_physical(relative_path, bytes)
        }

        fn put_physical_if_absent(
            &self,
            relative_path: &str,
            bytes: &[u8],
        ) -> anyhow::Result<bool> {
            self.inner.put_physical_if_absent(relative_path, bytes)
        }

        fn get_physical(&self, relative_path: &str) -> anyhow::Result<Vec<u8>> {
            self.inner.get_physical(relative_path)
        }

        fn get_physical_range(
            &self,
            relative_path: &str,
            offset: usize,
            length: usize,
        ) -> anyhow::Result<Vec<u8>> {
            self.inner.get_physical_range(relative_path, offset, length)
        }

        fn delete_physical(&self, relative_path: &str) -> anyhow::Result<()> {
            self.inner.delete_physical(relative_path)
        }

        fn exists_physical(&self, relative_path: &str) -> bool {
            if relative_path == "pack-index/root.json" {
                self.pack_index_root_exists_calls
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            self.inner.exists_physical(relative_path)
        }

        fn stat_physical(&self, relative_path: &str) -> anyhow::Result<e2v_store::ObjectStat> {
            if relative_path == "pack-index/root.json" {
                self.pack_index_root_stat_calls
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            self.inner.stat_physical(relative_path)
        }

        fn list_physical(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
            self.inner.list_physical(prefix)
        }
    }

    impl RefStore for PackIndexRootExistsProbeCountingBackend {
        fn read_ref(&self, token: &RefToken) -> anyhow::Result<Option<StoredRef>> {
            self.inner.read_ref(token)
        }

        fn list_refs(&self) -> anyhow::Result<Vec<e2v_store::ListedRef>> {
            self.inner.list_refs()
        }

        fn compare_and_swap_ref(
            &self,
            token: &RefToken,
            expected: Option<e2v_store::RefVersion>,
            next: EncryptedRef,
        ) -> anyhow::Result<e2v_store::CasResult> {
            self.inner.compare_and_swap_ref(token, expected, next)
        }
    }

    impl LayoutRootStore for PackIndexRootExistsProbeCountingBackend {
        fn read_layout_root(&self) -> anyhow::Result<e2v_store::LayoutRoot> {
            self.inner.read_layout_root()
        }

        fn compare_and_swap_layout_root(
            &self,
            expected: e2v_store::LayoutRootVersion,
            next: e2v_store::LayoutRoot,
        ) -> anyhow::Result<e2v_store::CasResult> {
            self.inner.compare_and_swap_layout_root(expected, next)
        }

        fn list_retained_layout_roots(&self) -> anyhow::Result<Vec<e2v_store::LayoutRoot>> {
            self.inner.list_retained_layout_roots()
        }
    }

    impl e2v_store::RemoteBackend for PackIndexRootExistsProbeCountingBackend {
        fn capability(&self) -> &BackendCapability {
            &self.capability
        }
    }

    let _guard = e2v_sync::testing::override_small_object_pack_threshold_for_test(1);
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
    fs::write(repo_root.join("rolling.txt"), "rolling-0").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "gc-pack-root-probe".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "gc-pack-root-probe-op".to_string(),
        },
    )
    .unwrap();
    assert!(remote.exists_physical("pack-index/root.json"));

    let counted = PackIndexRootExistsProbeCountingBackend::new(remote.clone());

    gc_execute(
        &counted,
        GcExecuteOptions {
            repo_root: repo_root.clone(),
            grace_period_days: 1,
            allow_single_writer_maintenance_window: false,
        },
    )
    .unwrap();

    assert!(
        counted.pack_index_root_stat_calls() > 0,
        "gc execute should still stat the pack index root when it exists"
    );
    assert_eq!(
        counted.pack_index_root_exists_calls(),
        0,
        "gc execute should not probe pack index root existence before statting it"
    );
}

#[test]
fn gc_execute_deletes_unreferenced_pack_index_segments_after_compaction() {
    let _guard = e2v_sync::testing::override_small_object_pack_threshold_for_test(1);
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
    let remote = MemoryBackend::new();

    for version in 0..5usize {
        fs::write(repo_root.join("rolling.txt"), format!("rolling-{version}")).unwrap();
        facade
            .commit(CommitOptions {
                repo_root: repo_root.clone(),
                message: format!("gc-pack-index-execute-{version}"),
            })
            .unwrap();
        push_head(
            &facade,
            &remote,
            PushOptions {
                repo_root: repo_root.clone(),
                branch_token: state.branch.token_hex.clone(),
                operation_id: format!("gc-pack-index-execute-op-{version}"),
            },
        )
        .unwrap();
    }

    let root = e2v_sync::testing::decode_pack_index_root_value_for_test(
        &repo_root.join(".e2v"),
        &remote.get_physical("pack-index/root.json").unwrap(),
    )
    .unwrap();
    let referenced_segments = root["segments"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|value| value.as_str())
        .map(str::to_string)
        .collect::<std::collections::BTreeSet<_>>();
    let unreferenced_segments = remote
        .list_physical("packs/index/")
        .unwrap()
        .into_iter()
        .chain(remote.list_physical("pack-index/segments/").unwrap())
        .filter(|path| !referenced_segments.contains(path))
        .collect::<Vec<_>>();
    assert!(
        !unreferenced_segments.is_empty(),
        "expected compaction to leave older pack index segments behind for gc execute to collect"
    );

    let old_time = std::time::SystemTime::now() - std::time::Duration::from_secs(31 * 24 * 60 * 60);
    for segment_path in &unreferenced_segments {
        e2v_store::testing::override_memory_backend_modified_time(&remote, segment_path, old_time)
            .unwrap();
    }

    let result = gc_execute(
        &remote,
        GcExecuteOptions {
            repo_root: repo_root.clone(),
            grace_period_days: 30,
            allow_single_writer_maintenance_window: true,
        },
    )
    .unwrap();

    for segment_path in &unreferenced_segments {
        assert!(
            result.deleted_physical_refs.contains(segment_path),
            "gc execute should delete unreferenced pack index segment {segment_path}, saw {:?}",
            result.deleted_physical_refs
        );
        assert!(
            !remote.exists_physical(segment_path),
            "gc execute should remove unreferenced pack index segment {segment_path}"
        );
    }
}

#[test]
fn gc_execute_aborts_when_pack_index_root_changes_after_dry_run() {
    let _guard = e2v_sync::testing::override_small_object_pack_threshold_for_test(1);
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
    let remote = MemoryBackend::new();

    for version in 0..5usize {
        fs::write(repo_root.join("rolling.txt"), format!("rolling-{version}")).unwrap();
        facade
            .commit(CommitOptions {
                repo_root: repo_root.clone(),
                message: format!("gc-pack-index-race-{version}"),
            })
            .unwrap();
        push_head(
            &facade,
            &remote,
            PushOptions {
                repo_root: repo_root.clone(),
                branch_token: state.branch.token_hex.clone(),
                operation_id: format!("gc-pack-index-race-op-{version}"),
            },
        )
        .unwrap();
    }

    let mut root = e2v_sync::testing::decode_pack_index_root_value_for_test(
        &repo_root.join(".e2v"),
        &remote.get_physical("pack-index/root.json").unwrap(),
    )
    .unwrap();
    let referenced_segments = root["segments"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|value| value.as_str())
        .map(str::to_string)
        .collect::<std::collections::BTreeSet<_>>();
    let unreferenced_segments = remote
        .list_physical("packs/index/")
        .unwrap()
        .into_iter()
        .chain(remote.list_physical("pack-index/segments/").unwrap())
        .filter(|path| !referenced_segments.contains(path))
        .collect::<Vec<_>>();
    let resurrected_segment = unreferenced_segments
        .first()
        .cloned()
        .expect("expected an obsolete pack index segment after compaction");

    let old_time = std::time::SystemTime::now() - std::time::Duration::from_secs(31 * 24 * 60 * 60);
    e2v_store::testing::override_memory_backend_modified_time(
        &remote,
        &resurrected_segment,
        old_time,
    )
    .unwrap();

    let root_segments = root["segments"].as_array_mut().unwrap();
    root_segments.push(serde_json::Value::String(resurrected_segment.clone()));
    let replacement_root_bytes =
        e2v_sync::testing::encode_pack_index_root_value_for_test(&repo_root.join(".e2v"), &root)
            .unwrap();
    let raced_remote =
        PackIndexRootChangesBeforeDeleteBackend::new(remote.clone(), replacement_root_bytes);

    let error = gc_execute(
        &raced_remote,
        GcExecuteOptions {
            repo_root: repo_root.clone(),
            grace_period_days: 30,
            allow_single_writer_maintenance_window: true,
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("changed during execution"),
        "unexpected error: {error:#}"
    );
    assert!(
        remote.exists_physical(&resurrected_segment),
        "gc execute should not delete a pack index segment that became reachable again"
    );
}

#[test]
fn gc_execute_treats_disappearing_pack_index_root_as_fence_change() {
    let _guard = e2v_sync::testing::override_small_object_pack_threshold_for_test(1);
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
    let remote = MemoryBackend::new();

    for version in 0..5usize {
        fs::write(repo_root.join("rolling.txt"), format!("rolling-{version}")).unwrap();
        facade
            .commit(CommitOptions {
                repo_root: repo_root.clone(),
                message: format!("gc-pack-index-disappear-{version}"),
            })
            .unwrap();
        push_head(
            &facade,
            &remote,
            PushOptions {
                repo_root: repo_root.clone(),
                branch_token: state.branch.token_hex.clone(),
                operation_id: format!("gc-pack-index-disappear-op-{version}"),
            },
        )
        .unwrap();
    }

    let root = e2v_sync::testing::decode_pack_index_root_value_for_test(
        &repo_root.join(".e2v"),
        &remote.get_physical("pack-index/root.json").unwrap(),
    )
    .unwrap();
    let referenced_segments = root["segments"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|value| value.as_str())
        .map(str::to_string)
        .collect::<std::collections::BTreeSet<_>>();
    let unreferenced_segments = remote
        .list_physical("packs/index/")
        .unwrap()
        .into_iter()
        .chain(remote.list_physical("pack-index/segments/").unwrap())
        .filter(|path| !referenced_segments.contains(path))
        .collect::<Vec<_>>();
    let resurrected_segment = unreferenced_segments
        .first()
        .cloned()
        .expect("expected an obsolete pack index segment after compaction");

    let old_time = std::time::SystemTime::now() - std::time::Duration::from_secs(31 * 24 * 60 * 60);
    e2v_store::testing::override_memory_backend_modified_time(
        &remote,
        &resurrected_segment,
        old_time,
    )
    .unwrap();

    let raced_remote = PackIndexRootDisappearsDuringFenceBackend::new(remote.clone());

    let error = gc_execute(
        &raced_remote,
        GcExecuteOptions {
            repo_root: repo_root.clone(),
            grace_period_days: 30,
            allow_single_writer_maintenance_window: true,
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("changed during execution"),
        "unexpected error: {error:#}"
    );
    assert!(
        remote.exists_physical(&resurrected_segment),
        "gc execute should not delete candidates after pack index root disappeared"
    );
}

#[test]
fn gc_execute_aborts_when_layout_root_bytes_change_after_dry_run() {
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
            message: "seed".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-gc-layout-root-race".to_string(),
        },
    )
    .unwrap();

    let stray_object_path =
        "objects/abababababababababababababababababababababababababababababababab.json";
    remote
        .put_physical(stray_object_path, br#"{"garbage":true}"#)
        .unwrap();
    e2v_store::testing::override_memory_backend_modified_time(
        &remote,
        stray_object_path,
        std::time::SystemTime::now() - std::time::Duration::from_secs(31 * 24 * 60 * 60),
    )
    .unwrap();

    let mut replacement_root: serde_json::Value =
        serde_json::from_slice(&remote.get_physical("layout_root.json").unwrap()).unwrap();
    replacement_root["mapping_policy"] = serde_json::Value::String("loose-mutated".to_string());
    let replacement_layout_root_bytes = serde_json::to_vec_pretty(&replacement_root).unwrap();
    let raced_remote = LayoutRootBytesChangeBeforeDeleteBackend::new(
        remote.clone(),
        replacement_layout_root_bytes,
    );

    let error = gc_execute(
        &raced_remote,
        GcExecuteOptions {
            repo_root: repo_root.clone(),
            grace_period_days: 30,
            allow_single_writer_maintenance_window: true,
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("changed during execution"),
        "unexpected error: {error:#}"
    );
    assert!(
        remote.exists_physical(stray_object_path),
        "gc execute should not delete candidates after layout root bytes changed"
    );
}

fn upload_local_objects_to_remote(
    remote: &MemoryBackend,
    repo_root: &std::path::Path,
    object_ids: &[String],
) {
    for object_id in object_ids {
        let bytes = fs::read(
            repo_root
                .join(".e2v")
                .join("objects")
                .join(format!("{object_id}.json")),
        )
        .unwrap();
        remote
            .put_physical(&format!("objects/{object_id}.json"), &bytes)
            .unwrap();
    }
}

#[test]
fn verify_remote_rejects_remote_layout_generation_rollback_below_local_high_water() {
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
            message: "seed".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-rollback-high-water".to_string(),
        },
    )
    .unwrap();

    let trusted_state_root = temp.path().join("trusted-state");
    fs::create_dir_all(&trusted_state_root).unwrap();
    let _trusted_state_guard =
        e2v_sync::testing::override_trusted_state_dir_for_test(trusted_state_root.clone());
    let remote_keyring_path = repo_root.join(".e2v").join("keyring").join("keyring.1");
    let remote_keyring: serde_json::Value =
        serde_json::from_slice(&fs::read(&remote_keyring_path).unwrap()).unwrap();
    let repo_id = remote_keyring["repo_id"]
        .as_str()
        .expect("remote keyring should contain repo_id");
    fs::write(
        trusted_state_root.join(format!("{repo_id}.json")),
        serde_json::to_vec_pretty(&serde_json::json!({
            "repo_id": repo_id,
            "min_layout_generation": 9u64,
            "min_keyring_generation": 1u64,
            "min_ref_generation": 1u64
        }))
        .unwrap(),
    )
    .unwrap();

    let error = verify_remote(
        &remote,
        VerifyRemoteOptions {
            repo_root: repo_root.clone(),
            sample_percent: 100,
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("CRITICAL_ROLLBACK_DETECTED")
            || error.to_string().contains("critical rollback detected"),
        "expected rollback detection error, got: {error:#}"
    );
}

#[test]
fn verify_remote_reuses_cached_pack_data_across_repeated_runs() {
    let _guard = e2v_sync::testing::override_small_object_pack_threshold_for_test(1);
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
    for index in 0..24usize {
        fs::write(
            repo_root.join(format!("packed-{index:02}.txt")),
            format!("packed-payload-{index:02}"),
        )
        .unwrap();
    }
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "packed-seed".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-maintenance-pack-cache".to_string(),
        },
    )
    .unwrap();

    let tracked_remote = RangeReadTrackingBackend::new(remote);
    let first = verify_remote(
        &tracked_remote,
        VerifyRemoteOptions {
            repo_root: repo_root.clone(),
            sample_percent: 100,
        },
    )
    .unwrap();
    assert!(first.sampled_objects > 0);

    let first_range_reads = tracked_remote
        .range_read_paths()
        .into_iter()
        .filter(|path| path.starts_with("packs/data/"))
        .collect::<Vec<_>>();
    assert!(
        !first_range_reads.is_empty(),
        "expected first verify_remote run to fetch packed data from remote"
    );

    tracked_remote.reset_range_reads();

    let second = verify_remote(
        &tracked_remote,
        VerifyRemoteOptions {
            repo_root: repo_root.clone(),
            sample_percent: 100,
        },
    )
    .unwrap();
    assert_eq!(second.sampled_objects, first.sampled_objects);

    let second_range_reads = tracked_remote
        .range_read_paths()
        .into_iter()
        .filter(|path| path.starts_with("packs/data/"))
        .collect::<Vec<_>>();
    assert!(
        second_range_reads.is_empty(),
        "expected second verify_remote run to reuse local pack-data cache, saw remote range reads: {:?}",
        second_range_reads
    );
}

#[test]
fn verify_remote_recovers_from_corrupted_local_pack_data_cache() {
    let _guard = e2v_sync::testing::override_small_object_pack_threshold_for_test(1);
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
    for index in 0..24usize {
        fs::write(
            repo_root.join(format!("recover-packed-{index:02}.txt")),
            format!("recover-packed-payload-{index:02}"),
        )
        .unwrap();
    }
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "packed-cache-corruption-recovery".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-maintenance-pack-cache-corruption".to_string(),
        },
    )
    .unwrap();

    let tracked_remote = RangeReadTrackingBackend::new(remote);
    let first = verify_remote(
        &tracked_remote,
        VerifyRemoteOptions {
            repo_root: repo_root.clone(),
            sample_percent: 100,
        },
    )
    .unwrap();
    assert!(first.sampled_objects > 0);

    let cache_root = repo_root
        .join(".e2v")
        .join("cache")
        .join("pack-data")
        .join("packs")
        .join("data");
    let cached_pack_path = fs::read_dir(&cache_root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .next()
        .expect("expected verify_remote to materialize pack-data cache");
    fs::write(&cached_pack_path, b"corrupt-pack-cache").unwrap();

    tracked_remote.reset_range_reads();

    let second = verify_remote(
        &tracked_remote,
        VerifyRemoteOptions {
            repo_root: repo_root.clone(),
            sample_percent: 100,
        },
    )
    .unwrap();
    assert_eq!(second.sampled_objects, first.sampled_objects);

    let second_range_reads = tracked_remote
        .range_read_paths()
        .into_iter()
        .filter(|path| path.starts_with("packs/data/"))
        .collect::<Vec<_>>();
    assert!(
        !second_range_reads.is_empty(),
        "expected corrupted local pack-data cache to trigger remote pack range reread"
    );
    assert_ne!(fs::read(&cached_pack_path).unwrap(), b"corrupt-pack-cache");
}

#[test]
fn verify_remote_recovers_from_tampered_local_pack_data_cache_with_same_length() {
    let _guard = e2v_sync::testing::override_small_object_pack_threshold_for_test(1);
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
    for index in 0..24usize {
        fs::write(
            repo_root.join(format!("tampered-packed-{index:02}.txt")),
            format!("tampered-packed-payload-{index:02}"),
        )
        .unwrap();
    }
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "packed-cache-tamper-recovery".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-maintenance-pack-cache-tamper".to_string(),
        },
    )
    .unwrap();

    let tracked_remote = RangeReadTrackingBackend::new(remote);
    let first = verify_remote(
        &tracked_remote,
        VerifyRemoteOptions {
            repo_root: repo_root.clone(),
            sample_percent: 100,
        },
    )
    .unwrap();
    assert!(first.sampled_objects > 0);

    let cache_root = repo_root
        .join(".e2v")
        .join("cache")
        .join("pack-data")
        .join("packs")
        .join("data");
    let cached_pack_path = fs::read_dir(&cache_root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .next()
        .expect("expected verify_remote to materialize pack-data cache");
    let mut tampered_cache_bytes = fs::read(&cached_pack_path).unwrap();
    let flip_index = tampered_cache_bytes.len() / 2;
    tampered_cache_bytes[flip_index] ^= 0x01;
    fs::write(&cached_pack_path, &tampered_cache_bytes).unwrap();

    tracked_remote.reset_range_reads();

    let second = verify_remote(
        &tracked_remote,
        VerifyRemoteOptions {
            repo_root: repo_root.clone(),
            sample_percent: 100,
        },
    )
    .unwrap();
    assert_eq!(second.sampled_objects, first.sampled_objects);

    let second_range_reads = tracked_remote
        .range_read_paths()
        .into_iter()
        .filter(|path| path.starts_with("packs/data/"))
        .collect::<Vec<_>>();
    assert!(
        !second_range_reads.is_empty(),
        "expected tampered local pack-data cache to trigger remote pack range reread"
    );
    assert_ne!(fs::read(&cached_pack_path).unwrap(), tampered_cache_bytes);
}

#[test]
fn verify_remote_prunes_stale_local_pack_data_cache_after_historical_rewrite() {
    let _guard = e2v_sync::testing::override_small_object_pack_threshold_for_test(1);
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
    for index in 0..24usize {
        fs::write(
            repo_root.join(format!("prune-packed-{index:02}.txt")),
            format!("prune-packed-payload-{index:02}"),
        )
        .unwrap();
    }
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "epoch-one".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-pack-cache-prune-seed".to_string(),
        },
    )
    .unwrap();

    let tracked_remote = RangeReadTrackingBackend::new(remote.clone());
    let first = verify_remote(
        &tracked_remote,
        VerifyRemoteOptions {
            repo_root: repo_root.clone(),
            sample_percent: 100,
        },
    )
    .unwrap();
    assert!(first.sampled_objects > 0);

    let cache_root = repo_root
        .join(".e2v")
        .join("cache")
        .join("pack-data")
        .join("packs")
        .join("data");
    let stale_cached_pack_paths = fs::read_dir(&cache_root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert!(
        !stale_cached_pack_paths.is_empty(),
        "expected initial verify_remote run to materialize pack-data cache"
    );

    e2v_core::testing::rotate_active_epoch_for_test(&repo_root, "correct horse battery staple")
        .unwrap();
    fs::write(repo_root.join("epoch-two.txt"), b"epoch-two").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "epoch-two".to_string(),
        })
        .unwrap();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-pack-cache-prune-rotate".to_string(),
        },
    )
    .unwrap();
    historical_rewrite_remote(
        &remote,
        HistoricalRewriteOptions {
            repo_root: repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            confirm_full_reencryption: true,
        },
    )
    .unwrap();

    tracked_remote.reset_range_reads();

    let second = verify_remote(
        &tracked_remote,
        VerifyRemoteOptions {
            repo_root: repo_root.clone(),
            sample_percent: 100,
        },
    )
    .unwrap();
    assert!(second.sampled_objects > 0);

    let refreshed_cached_pack_paths = fs::read_dir(&cache_root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert!(
        !refreshed_cached_pack_paths.is_empty(),
        "expected rewritten verify_remote run to retain active pack-data cache"
    );
    for stale_path in stale_cached_pack_paths {
        assert!(
            !stale_path.exists(),
            "expected verify_remote to prune stale local pack-data cache file {}",
            stale_path.display()
        );
        let stale_hash_path = stale_path.with_extension(format!(
            "{}.blake3",
            stale_path
                .extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or("cache")
        ));
        assert!(
            !stale_hash_path.exists(),
            "expected verify_remote to prune stale local pack-data cache hash {}",
            stale_hash_path.display()
        );
    }
}

#[test]
fn verify_remote_ignores_undeletable_stale_local_pack_data_cache_entries() {
    let _guard = e2v_sync::testing::override_small_object_pack_threshold_for_test(1);
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
    for index in 0..24usize {
        fs::write(
            repo_root.join(format!("undeletable-packed-{index:02}.txt")),
            format!("undeletable-packed-payload-{index:02}"),
        )
        .unwrap();
    }
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "epoch-one".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-pack-cache-undeletable-seed".to_string(),
        },
    )
    .unwrap();

    let tracked_remote = RangeReadTrackingBackend::new(remote.clone());
    let first = verify_remote(
        &tracked_remote,
        VerifyRemoteOptions {
            repo_root: repo_root.clone(),
            sample_percent: 100,
        },
    )
    .unwrap();
    assert!(first.sampled_objects > 0);

    let cache_root = repo_root
        .join(".e2v")
        .join("cache")
        .join("pack-data")
        .join("packs")
        .join("data");

    e2v_core::testing::rotate_active_epoch_for_test(&repo_root, "correct horse battery staple")
        .unwrap();
    fs::write(repo_root.join("epoch-two.txt"), b"epoch-two").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "epoch-two".to_string(),
        })
        .unwrap();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-op-pack-cache-undeletable-rotate".to_string(),
        },
    )
    .unwrap();
    historical_rewrite_remote(
        &remote,
        HistoricalRewriteOptions {
            repo_root: repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            confirm_full_reencryption: true,
        },
    )
    .unwrap();

    let stale_path = cache_root.join("foreign-stale-pack.bin");
    let _stale_guard = make_undeletable_cache_entry(&stale_path);
    fs::write(stale_path.with_extension("bin.blake3"), b"deadbeef").unwrap();

    let second = verify_remote(
        &tracked_remote,
        VerifyRemoteOptions {
            repo_root: repo_root.clone(),
            sample_percent: 100,
        },
    )
    .unwrap();

    assert!(second.sampled_objects > 0);
}
