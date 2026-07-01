use std::fs;

use e2v_core::{CommitOptions, InitOptions, ManifestStore, ManifestStoreApi, RepositoryFacade};
use e2v_store::{
    BackendCapability, ConsistencyClass, EncryptedRef, LayoutRootStore, RefStore, RefToken,
    StoredRef,
};
use e2v_store::{BlobStore, MemoryBackend};
use tempfile::tempdir;

use e2v_sync::{
    GcDryRunOptions, GcExecuteOptions, PushOptions, RepairRemoteOptions, VerifyRemoteOptions,
    force_accept_remote_rollback, gc_dry_run, gc_execute, push_head, repair_remote, verify_remote,
};

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
    remote
        .override_physical_modified_time_for_test(stray_object_path, old_time)
        .unwrap();
    remote
        .put_physical(
            "transactions/active/op-expired.intent",
            br#"{"operation_id":"op-expired","writer_id":"writer:op-expired","started_at_remote_unix_ms":1,"heartbeat":{"remote_observed_at_unix_ms":1,"sequence":1},"expected_ref_version":null,"target_branch_token":"main","planned_snapshot_id":null,"client_version":"test"}"#,
        )
        .unwrap();
    remote
        .override_physical_modified_time_for_test(
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
            if saw_first
                && !self
                    .mutated
                    .swap(true, std::sync::atomic::Ordering::SeqCst)
            {
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
    remote
        .override_physical_modified_time_for_test(
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
    remote
        .override_physical_modified_time_for_test(
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
    remote
        .override_physical_modified_time_for_test(
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
    remote
        .override_physical_modified_time_for_test(stray_one, old_time)
        .unwrap();
    remote
        .override_physical_modified_time_for_test(stray_two, old_time)
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
        remote
            .override_physical_modified_time_for_test(
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
        remote
            .override_physical_modified_time_for_test(
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
        remote
            .override_physical_modified_time_for_test(segment_path, old_time)
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
    remote
        .override_physical_modified_time_for_test(&resurrected_segment, old_time)
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
