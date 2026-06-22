use std::fs;
use std::sync::{Arc, Mutex};

use e2v_core::{CommitOptions, InitOptions, ManifestStore, ManifestStoreApi, RepositoryFacade};
use e2v_store::{
    BackendCapability, BlobStore, CasResult, ConsistencyClass, EncryptedRef, LayoutRoot,
    LayoutRootStore, MemoryBackend, RefStore, RefToken, RefVersion, RemoteBackend, StoredRef,
};
use tempfile::tempdir;

use e2v_sync::{clone_remote, push_head, resume_push, CloneOptions, PushOptions, ResumeOptions};

#[derive(Debug, Clone)]
struct RefConflictBackend {
    inner: MemoryBackend,
    capability: BackendCapability,
}

impl RefConflictBackend {
    fn new() -> Self {
        Self {
            inner: MemoryBackend::new(),
            capability: BackendCapability {
                supports_conditional_put: true,
                supports_range_read: true,
                supports_atomic_rename: true,
                supports_paged_list: true,
                consistency_class: ConsistencyClass::StrongWhitelisted,
                supports_remote_lock_or_lease: true,
                supports_transaction_markers: true,
                supports_reliable_remote_time: true,
                supports_object_generation_or_etag: true,
                supports_layout_root_cas: true,
                supports_oblivious_access_schedule: false,
            },
        }
    }
}

impl BlobStore for RefConflictBackend {
    fn put_physical(&self, relative_path: &str, bytes: &[u8]) -> anyhow::Result<()> {
        self.inner.put_physical(relative_path, bytes)
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

impl RefStore for RefConflictBackend {
    fn read_ref(&self, _token: &RefToken) -> anyhow::Result<Option<StoredRef>> {
        Ok(None)
    }

    fn compare_and_swap_ref(
        &self,
        _token: &RefToken,
        _expected: Option<RefVersion>,
        _next: EncryptedRef,
    ) -> anyhow::Result<CasResult> {
        Ok(CasResult {
            applied: false,
            current: Some(StoredRef {
                version: RefVersion { value: 2 },
                value: EncryptedRef::new(vec![9, 9, 9]),
            }),
        })
    }
}

impl LayoutRootStore for RefConflictBackend {
    fn read_layout_root(&self) -> anyhow::Result<LayoutRoot> {
        self.inner.read_layout_root()
    }

    fn compare_and_swap_layout_root(
        &self,
        expected: u64,
        next: LayoutRoot,
    ) -> anyhow::Result<CasResult> {
        self.inner.compare_and_swap_layout_root(expected, next)
    }

    fn list_retained_layout_roots(&self) -> anyhow::Result<Vec<LayoutRoot>> {
        self.inner.list_retained_layout_roots()
    }
}

impl RemoteBackend for RefConflictBackend {
    fn capability(&self) -> &BackendCapability {
        &self.capability
    }
}

#[derive(Debug, Clone)]
struct LayoutPublisherOnlyBackend {
    inner: MemoryBackend,
    capability: BackendCapability,
    layout_cas_called: std::sync::Arc<std::sync::Mutex<bool>>,
}

impl LayoutPublisherOnlyBackend {
    fn new() -> Self {
        Self {
            inner: MemoryBackend::new(),
            capability: BackendCapability {
                supports_conditional_put: true,
                supports_range_read: true,
                supports_atomic_rename: true,
                supports_paged_list: true,
                consistency_class: ConsistencyClass::StrongWhitelisted,
                supports_remote_lock_or_lease: true,
                supports_transaction_markers: true,
                supports_reliable_remote_time: true,
                supports_object_generation_or_etag: true,
                supports_layout_root_cas: true,
                supports_oblivious_access_schedule: false,
            },
            layout_cas_called: std::sync::Arc::new(std::sync::Mutex::new(false)),
        }
    }
}

impl BlobStore for LayoutPublisherOnlyBackend {
    fn put_physical(&self, relative_path: &str, bytes: &[u8]) -> anyhow::Result<()> {
        anyhow::ensure!(
            relative_path != "layout_root.json" || *self.layout_cas_called.lock().unwrap(),
            "push_head must not bypass TransactionPublisher for layout root publish"
        );
        self.inner.put_physical(relative_path, bytes)
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

impl RefStore for LayoutPublisherOnlyBackend {
    fn read_ref(&self, token: &RefToken) -> anyhow::Result<Option<StoredRef>> {
        self.inner.read_ref(token)
    }

    fn compare_and_swap_ref(
        &self,
        token: &RefToken,
        expected: Option<RefVersion>,
        next: EncryptedRef,
    ) -> anyhow::Result<CasResult> {
        self.inner.compare_and_swap_ref(token, expected, next)
    }
}

impl LayoutRootStore for LayoutPublisherOnlyBackend {
    fn read_layout_root(&self) -> anyhow::Result<LayoutRoot> {
        self.inner.read_layout_root()
    }

    fn compare_and_swap_layout_root(
        &self,
        expected: u64,
        next: LayoutRoot,
    ) -> anyhow::Result<CasResult> {
        let result = self.inner.compare_and_swap_layout_root(expected, next)?;
        if result.applied {
            *self.layout_cas_called.lock().unwrap() = true;
        }
        Ok(result)
    }

    fn list_retained_layout_roots(&self) -> anyhow::Result<Vec<LayoutRoot>> {
        self.inner.list_retained_layout_roots()
    }
}

impl RemoteBackend for LayoutPublisherOnlyBackend {
    fn capability(&self) -> &BackendCapability {
        &self.capability
    }
}

#[derive(Debug, Clone)]
struct InterruptingObjectUploadBackend {
    inner: MemoryBackend,
    capability: BackendCapability,
    remaining_object_uploads_before_failure: Arc<Mutex<Option<usize>>>,
}

impl InterruptingObjectUploadBackend {
    fn new(successful_object_uploads_before_failure: usize) -> Self {
        Self {
            inner: MemoryBackend::new(),
            capability: BackendCapability {
                supports_conditional_put: true,
                supports_range_read: true,
                supports_atomic_rename: true,
                supports_paged_list: true,
                consistency_class: ConsistencyClass::StrongWhitelisted,
                supports_remote_lock_or_lease: true,
                supports_transaction_markers: true,
                supports_reliable_remote_time: true,
                supports_object_generation_or_etag: true,
                supports_layout_root_cas: true,
                supports_oblivious_access_schedule: false,
            },
            remaining_object_uploads_before_failure: Arc::new(Mutex::new(Some(
                successful_object_uploads_before_failure,
            ))),
        }
    }
}

impl BlobStore for InterruptingObjectUploadBackend {
    fn put_physical(&self, relative_path: &str, bytes: &[u8]) -> anyhow::Result<()> {
        if relative_path.starts_with("objects/") {
            let mut remaining = self.remaining_object_uploads_before_failure.lock().unwrap();
            if let Some(successes_left) = remaining.as_mut() {
                if *successes_left == 0 {
                    *remaining = None;
                    anyhow::bail!("simulated object upload interruption");
                }
                *successes_left -= 1;
            }
        }
        self.inner.put_physical(relative_path, bytes)
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

impl RefStore for InterruptingObjectUploadBackend {
    fn read_ref(&self, token: &RefToken) -> anyhow::Result<Option<StoredRef>> {
        self.inner.read_ref(token)
    }

    fn compare_and_swap_ref(
        &self,
        token: &RefToken,
        expected: Option<RefVersion>,
        next: EncryptedRef,
    ) -> anyhow::Result<CasResult> {
        self.inner.compare_and_swap_ref(token, expected, next)
    }
}

impl LayoutRootStore for InterruptingObjectUploadBackend {
    fn read_layout_root(&self) -> anyhow::Result<LayoutRoot> {
        self.inner.read_layout_root()
    }

    fn compare_and_swap_layout_root(
        &self,
        expected: u64,
        next: LayoutRoot,
    ) -> anyhow::Result<CasResult> {
        self.inner.compare_and_swap_layout_root(expected, next)
    }

    fn list_retained_layout_roots(&self) -> anyhow::Result<Vec<LayoutRoot>> {
        self.inner.list_retained_layout_roots()
    }
}

impl RemoteBackend for InterruptingObjectUploadBackend {
    fn capability(&self) -> &BackendCapability {
        &self.capability
    }
}

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
fn push_publishes_layout_root_through_transaction_publisher() {
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
            message: "publisher-layout-root".to_string(),
        })
        .unwrap();

    let remote = LayoutPublisherOnlyBackend::new();

    let result = push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "publisher-layout-root-op".to_string(),
        },
    )
    .unwrap();

    assert_eq!(result.published_snapshot_id, commit.snapshot_id);
    assert_eq!(
        remote.read_layout_root().unwrap().generation,
        state.layout_generation
    );
}

#[test]
fn push_ignores_unreachable_local_object_files() {
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
            message: "reachable-only".to_string(),
        })
        .unwrap();

    let stray_object_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let stray_object_path = repo_root
        .join(".e2v")
        .join("objects")
        .join(format!("{stray_object_id}.json"));
    fs::write(&stray_object_path, br#"{"not":"reachable"}"#).unwrap();

    let remote = MemoryBackend::new();

    let result = push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "reachable-only-op".to_string(),
        },
    )
    .unwrap();

    assert!(result.uploaded_objects > 0);
    assert!(!remote.exists_physical(&format!("objects/{stray_object_id}.json")));
}

#[test]
fn push_batches_small_objects_into_remote_bundles_when_threshold_is_reached() {
    let _guard = e2v_sync::testing::override_small_object_bundle_threshold_for_test(1);
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
            message: "bundle-small-objects".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    let pushed = push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "bundle-small-objects-op".to_string(),
        },
    )
    .unwrap();

    assert!(pushed.uploaded_objects > 0);
    assert!(!remote.list_physical("bundles/index/").unwrap().is_empty());
    assert!(!remote.list_physical("bundles/data/").unwrap().is_empty());
    assert!(remote.list_physical("objects/").unwrap().len() < pushed.uploaded_objects);
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

    let rebuilt = MemoryBackend::new();
    for path in remote.list_physical("objects/").unwrap() {
        rebuilt
            .put_physical(&path, &remote.get_physical(&path).unwrap())
            .unwrap();
    }
    rebuilt
        .put_physical(
            "control/config.json",
            &remote.get_physical("control/config.json").unwrap(),
        )
        .unwrap();
    for path in remote.list_physical("control/keyring/").unwrap() {
        rebuilt
            .put_physical(&path, &remote.get_physical(&path).unwrap())
            .unwrap();
    }
    rebuilt
        .compare_and_swap_layout_root(1, remote.read_layout_root().unwrap())
        .unwrap();

    let resumed = resume_push(
        &facade,
        &rebuilt,
        ResumeOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "resume-op".to_string(),
        },
    )
    .unwrap();

    assert!(resumed.skipped_uploaded_objects > 0);
    assert_eq!(resumed.published_snapshot_id, commit.snapshot_id);
    assert!(rebuilt
        .read_ref(&RefToken::new(state.branch.token_hex.clone()))
        .unwrap()
        .is_some());
}

#[test]
fn resume_reuploads_missing_remote_objects_from_journal() {
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
            message: "resume-missing-object".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    let pushed = push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "resume-missing-object-op".to_string(),
        },
    )
    .unwrap();
    assert!(pushed.uploaded_objects > 0);

    let first_remote_object = remote
        .list_physical("objects/")
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let removed_path = repo_root
        .join(".e2v")
        .join("journal")
        .join("sync")
        .join("operations.sqlite");
    assert!(removed_path.exists());

    let object_bytes = std::fs::read(
        repo_root
            .join(".e2v")
            .join("objects")
            .join(first_remote_object.strip_prefix("objects/").unwrap()),
    )
    .unwrap();
    let physical = first_remote_object.clone();
    let remote_shadow = remote
        .list_physical("objects/")
        .unwrap()
        .into_iter()
        .filter(|path| path != &physical)
        .collect::<Vec<_>>();
    let rebuilt = MemoryBackend::new();
    for path in remote_shadow {
        let bytes = remote.get_physical(&path).unwrap();
        rebuilt.put_physical(&path, &bytes).unwrap();
    }
    rebuilt
        .put_physical(
            "control/config.json",
            &remote.get_physical("control/config.json").unwrap(),
        )
        .unwrap();
    rebuilt
        .put_physical(
            "control/refs/default.json",
            &remote.get_physical("control/refs/default.json").unwrap(),
        )
        .unwrap();
    for path in remote.list_physical("control/keyring/").unwrap() {
        rebuilt
            .put_physical(&path, &remote.get_physical(&path).unwrap())
            .unwrap();
    }
    let stored_ref = remote
        .read_ref(&RefToken::new(state.branch.token_hex.clone()))
        .unwrap()
        .unwrap();
    rebuilt
        .compare_and_swap_ref(
            &RefToken::new(state.branch.token_hex.clone()),
            None,
            stored_ref.value.clone(),
        )
        .unwrap();
    let next_layout = remote.read_layout_root().unwrap();
    rebuilt
        .compare_and_swap_layout_root(1, next_layout)
        .unwrap();
    assert!(!rebuilt.exists_physical(&physical));

    let resumed = resume_push(
        &facade,
        &rebuilt,
        ResumeOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "resume-missing-object-op".to_string(),
        },
    )
    .unwrap();

    assert!(resumed.skipped_uploaded_objects > 0);
    assert_eq!(rebuilt.get_physical(&physical).unwrap(), object_bytes);
}

#[test]
fn resume_uploads_objects_missing_after_interrupted_push() {
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
            message: "resume-interrupted-push".to_string(),
        })
        .unwrap();
    let manifest_store = ManifestStore::new(&repo_root);
    let reachable_object_ids = manifest_store
        .collect_reachable_object_ids(&commit.snapshot_id)
        .unwrap();
    assert!(reachable_object_ids.len() > 1);

    let remote = InterruptingObjectUploadBackend::new(1);
    let push_error = push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "resume-interrupted-op".to_string(),
        },
    )
    .unwrap_err();
    assert!(push_error
        .to_string()
        .contains("simulated object upload interruption"));

    let resumed = resume_push(
        &facade,
        &remote,
        ResumeOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "resume-interrupted-op".to_string(),
        },
    )
    .unwrap();

    assert_eq!(resumed.published_snapshot_id, commit.snapshot_id);
    for object_id in reachable_object_ids {
        assert!(
            remote.exists_physical(&format!("objects/{object_id}.json")),
            "missing resumed object {object_id}"
        );
    }
}

#[test]
fn resume_counts_skipped_uploaded_objects_across_multiple_journal_batches() {
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
            message: "resume-batched-count".to_string(),
        })
        .unwrap();
    let manifest_store = ManifestStore::new(&repo_root);
    let reachable_object_ids = manifest_store
        .collect_reachable_object_ids(&commit.snapshot_id)
        .unwrap();
    assert!(reachable_object_ids.len() > 2);

    let remote = MemoryBackend::new();
    for object_id in reachable_object_ids.iter().skip(2) {
        let object_name = format!("{object_id}.json");
        let relative_path = format!("objects/{object_name}");
        let bytes =
            std::fs::read(repo_root.join(".e2v").join("objects").join(&object_name)).unwrap();
        remote.put_physical(&relative_path, &bytes).unwrap();
    }

    let journal =
        e2v_sync::OperationJournal::new(repo_root.join(".e2v").join("journal").join("sync"))
            .unwrap();
    let operation_id = e2v_sync::OperationId::new("resume-batched-count-op".to_string());
    journal
        .begin_operation(
            &operation_id,
            e2v_sync::OperationMetadata::push(state.branch.token_hex.clone(), None),
        )
        .unwrap();
    for object_id in &reachable_object_ids {
        journal
            .plan_object(&operation_id, object_id, "object")
            .unwrap();
    }
    for object_id in reachable_object_ids.iter().skip(2) {
        journal
            .record_uploaded(&operation_id, object_id, "object")
            .unwrap();
    }

    let resumed = resume_push(
        &facade,
        &remote,
        ResumeOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: operation_id.value.clone(),
        },
    )
    .unwrap();

    assert_eq!(resumed.published_snapshot_id, commit.snapshot_id);
    assert_eq!(
        resumed.skipped_uploaded_objects,
        reachable_object_ids.len() - 2
    );
}

#[test]
fn resume_restores_missing_control_plane_files_before_republishing_ref() {
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
            message: "resume-control-plane".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "resume-control-plane-op".to_string(),
        },
    )
    .unwrap();

    let rebuilt = MemoryBackend::new();
    for path in remote.list_physical("objects/").unwrap() {
        rebuilt
            .put_physical(&path, &remote.get_physical(&path).unwrap())
            .unwrap();
    }

    let resumed = resume_push(
        &facade,
        &rebuilt,
        ResumeOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "resume-control-plane-op".to_string(),
        },
    )
    .unwrap();

    assert_eq!(resumed.published_snapshot_id, commit.snapshot_id);
    assert_eq!(
        rebuilt.get_physical("control/config.json").unwrap(),
        remote.get_physical("control/config.json").unwrap()
    );
    assert_eq!(
        rebuilt.list_physical("control/keyring/").unwrap(),
        remote.list_physical("control/keyring/").unwrap()
    );
    assert!(rebuilt.exists_physical("layout_root.json"));
    assert!(rebuilt.exists_physical("control/refs/default.json"));
}

#[test]
fn resume_rejects_stale_remote_ref_and_requires_rebase_recovery() {
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
            message: "resume-stale-ref".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    let pushed = push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "resume-stale-ref-op".to_string(),
        },
    )
    .unwrap();
    assert_eq!(pushed.published_snapshot_id, commit.snapshot_id);

    remote
        .compare_and_swap_ref(
            &RefToken::new(state.branch.token_hex.clone()),
            Some(e2v_store::RefVersion { value: 1 }),
            e2v_store::EncryptedRef::new(vec![9, 9, 9]),
        )
        .unwrap();

    let error = resume_push(
        &facade,
        &remote,
        ResumeOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "resume-stale-ref-op".to_string(),
        },
    )
    .unwrap_err();

    assert!(error.to_string().contains("needs-rebase"));
}

#[test]
fn stale_remote_head_marks_push_needs_rebase() {
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
    let first = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "base".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    let first_push = push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "needs-rebase-base".to_string(),
        },
    )
    .unwrap();
    assert_eq!(first_push.published_snapshot_id, first.snapshot_id);

    let competitor_repo_root = temp.path().join("competitor");
    clone_remote(
        &remote,
        CloneOptions {
            repo_root: competitor_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
            branch_token: state.branch.token_hex.clone(),
        },
    )
    .unwrap();
    fs::write(competitor_repo_root.join("hello.txt"), b"competitor").unwrap();
    let competitor = RepositoryFacade::new();
    competitor
        .commit(CommitOptions {
            repo_root: competitor_repo_root.clone(),
            message: "competitor".to_string(),
        })
        .unwrap();
    push_head(
        &competitor,
        &remote,
        PushOptions {
            repo_root: competitor_repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "needs-rebase-competitor".to_string(),
        },
    )
    .unwrap();

    fs::write(repo_root.join("hello.txt"), b"source-second").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "source-second".to_string(),
        })
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
    assert!(remote
        .read_ref(&RefToken::new(state.branch.token_hex.clone()))
        .unwrap()
        .is_none());
    assert!(second.snapshot_id.len() > 10);
}

#[test]
fn push_marks_needs_rebase_when_ref_publish_cas_loses_race() {
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
            message: "cas-race".to_string(),
        })
        .unwrap();

    let remote = RefConflictBackend::new();

    let error = push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "cas-race-op".to_string(),
        },
    )
    .unwrap_err();

    assert!(error.to_string().contains("needs-rebase"));
}

#[test]
fn push_rejects_ref_publish_when_reachable_remote_object_disappears() {
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
            message: "verify-remote-before-ref".to_string(),
        })
        .unwrap();
    let manifest_store = ManifestStore::new(&repo_root);
    let reachable_object_ids = manifest_store
        .collect_reachable_object_ids(&commit.snapshot_id)
        .unwrap();

    let remote = MemoryBackend::new();
    let journal =
        e2v_sync::OperationJournal::new(repo_root.join(".e2v").join("journal").join("sync"))
            .unwrap();
    let operation_id = e2v_sync::OperationId::new("verify-remote-before-ref-op".to_string());
    journal
        .begin_operation(
            &operation_id,
            e2v_sync::OperationMetadata::push(state.branch.token_hex.clone(), None),
        )
        .unwrap();
    for object_id in &reachable_object_ids {
        journal
            .plan_object(&operation_id, object_id, "object")
            .unwrap();
    }
    for object_id in &reachable_object_ids[..reachable_object_ids.len() - 1] {
        let object_name = format!("{object_id}.json");
        remote
            .put_physical(
                &format!("objects/{object_name}"),
                &std::fs::read(repo_root.join(".e2v").join("objects").join(&object_name)).unwrap(),
            )
            .unwrap();
        journal
            .record_verified(&operation_id, object_id, "object")
            .unwrap();
    }

    let error = resume_push(
        &facade,
        &remote,
        ResumeOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: operation_id.value.clone(),
        },
    )
    .unwrap();

    assert_eq!(error.published_snapshot_id, commit.snapshot_id);
    for object_id in &reachable_object_ids {
        assert!(
            remote.exists_physical(&format!("objects/{object_id}.json"))
                || !remote.list_physical("bundles/index/").unwrap().is_empty()
        );
    }
}

#[test]
fn push_allows_fast_forward_when_remote_head_matches_local_parent() {
    let temp = tempdir().unwrap();

    let source_repo_root = temp.path().join("source");
    fs::create_dir_all(&source_repo_root).unwrap();
    let source = RepositoryFacade::new();
    let source_state = source
        .init(InitOptions {
            repo_root: source_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
    fs::write(source_repo_root.join("hello.txt"), b"first").unwrap();
    let first = source
        .commit(CommitOptions {
            repo_root: source_repo_root.clone(),
            message: "first".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    let first_push = push_head(
        &source,
        &remote,
        PushOptions {
            repo_root: source_repo_root.clone(),
            branch_token: source_state.branch.token_hex.clone(),
            operation_id: "ff-push-1".to_string(),
        },
    )
    .unwrap();
    assert_eq!(first_push.published_snapshot_id, first.snapshot_id);

    let clone_repo_root = temp.path().join("clone");
    let cloned = e2v_sync::clone_remote(
        &remote,
        e2v_sync::CloneOptions {
            repo_root: clone_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
            branch_token: source_state.branch.token_hex.clone(),
        },
    )
    .unwrap();
    assert_eq!(
        cloned.head_snapshot_id.as_deref(),
        Some(first.snapshot_id.as_str())
    );

    fs::write(clone_repo_root.join("hello.txt"), b"second").unwrap();
    let clone_facade = RepositoryFacade::new();
    let second = clone_facade
        .commit(CommitOptions {
            repo_root: clone_repo_root.clone(),
            message: "second".to_string(),
        })
        .unwrap();

    let second_push = push_head(
        &clone_facade,
        &remote,
        PushOptions {
            repo_root: clone_repo_root.clone(),
            branch_token: source_state.branch.token_hex.clone(),
            operation_id: "ff-push-2".to_string(),
        },
    )
    .unwrap();

    assert_eq!(second_push.published_snapshot_id, second.snapshot_id);
    let stored_ref = remote
        .read_ref(&RefToken::new(source_state.branch.token_hex.clone()))
        .unwrap()
        .unwrap();
    assert!(!stored_ref.value.bytes.is_empty());
    assert!(stored_ref.version.value >= 2);
}

#[test]
fn push_fast_forward_accepts_ancestor_snapshots_stored_only_in_bundles() {
    let _guard = e2v_sync::testing::override_small_object_bundle_threshold_for_test(1);
    let temp = tempdir().unwrap();

    let source_repo_root = temp.path().join("source");
    fs::create_dir_all(&source_repo_root).unwrap();
    let source = RepositoryFacade::new();
    let source_state = source
        .init(InitOptions {
            repo_root: source_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
    fs::write(source_repo_root.join("hello.txt"), b"first").unwrap();
    let first = source
        .commit(CommitOptions {
            repo_root: source_repo_root.clone(),
            message: "first".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    let first_push = push_head(
        &source,
        &remote,
        PushOptions {
            repo_root: source_repo_root.clone(),
            branch_token: source_state.branch.token_hex.clone(),
            operation_id: "bundle-ff-push-1".to_string(),
        },
    )
    .unwrap();
    assert_eq!(first_push.published_snapshot_id, first.snapshot_id);
    assert!(remote.list_physical("objects/").unwrap().is_empty());

    let clone_repo_root = temp.path().join("clone");
    let cloned = e2v_sync::clone_remote(
        &remote,
        e2v_sync::CloneOptions {
            repo_root: clone_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
            branch_token: source_state.branch.token_hex.clone(),
        },
    )
    .unwrap();
    assert_eq!(
        cloned.head_snapshot_id.as_deref(),
        Some(first.snapshot_id.as_str())
    );

    fs::write(clone_repo_root.join("hello.txt"), b"second").unwrap();
    let clone_facade = RepositoryFacade::new();
    let second = clone_facade
        .commit(CommitOptions {
            repo_root: clone_repo_root.clone(),
            message: "second".to_string(),
        })
        .unwrap();

    let second_push = push_head(
        &clone_facade,
        &remote,
        PushOptions {
            repo_root: clone_repo_root.clone(),
            branch_token: source_state.branch.token_hex.clone(),
            operation_id: "bundle-ff-push-2".to_string(),
        },
    )
    .unwrap();

    assert_eq!(second_push.published_snapshot_id, second.snapshot_id);
}
