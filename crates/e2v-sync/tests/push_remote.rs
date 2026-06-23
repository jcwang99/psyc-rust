use std::fs;
use std::sync::{Arc, Mutex};

use e2v_core::{CommitOptions, InitOptions, ManifestStore, ManifestStoreApi, RepositoryFacade};
use e2v_store::{
    BackendCapability, BlobStore, CasResult, ConsistencyClass, EncryptedRef, LayoutRoot,
    LayoutRootStore, MemoryBackend, RefStore, RefToken, RefVersion, RemoteBackend, StoredRef,
};
use tempfile::tempdir;

use e2v_sync::{CloneOptions, PushOptions, ResumeOptions, clone_remote, push_head, resume_push};

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
struct ExistsCountingBackend {
    inner: MemoryBackend,
    capability: BackendCapability,
    exists_calls: Arc<Mutex<usize>>,
    list_calls: Arc<Mutex<usize>>,
    object_put_calls: Arc<Mutex<usize>>,
    range_read_paths: Arc<Mutex<Vec<String>>>,
}

impl ExistsCountingBackend {
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
            exists_calls: Arc::new(Mutex::new(0)),
            list_calls: Arc::new(Mutex::new(0)),
            object_put_calls: Arc::new(Mutex::new(0)),
            range_read_paths: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn exists_call_count(&self) -> usize {
        *self.exists_calls.lock().unwrap()
    }

    fn list_call_count(&self) -> usize {
        *self.list_calls.lock().unwrap()
    }

    fn reset_counts(&self) {
        *self.exists_calls.lock().unwrap() = 0;
        *self.list_calls.lock().unwrap() = 0;
        *self.object_put_calls.lock().unwrap() = 0;
        self.range_read_paths.lock().unwrap().clear();
    }

    fn object_put_call_count(&self) -> usize {
        *self.object_put_calls.lock().unwrap()
    }

    fn range_read_paths(&self) -> Vec<String> {
        self.range_read_paths.lock().unwrap().clone()
    }
}

impl BlobStore for ExistsCountingBackend {
    fn put_physical(&self, relative_path: &str, bytes: &[u8]) -> anyhow::Result<()> {
        if relative_path.starts_with("objects/")
            || relative_path.starts_with("bundles/data/")
            || relative_path.starts_with("bundles/index/")
        {
            *self.object_put_calls.lock().unwrap() += 1;
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
        *self.exists_calls.lock().unwrap() += 1;
        self.inner.exists_physical(relative_path)
    }

    fn stat_physical(&self, relative_path: &str) -> anyhow::Result<e2v_store::ObjectStat> {
        self.inner.stat_physical(relative_path)
    }

    fn list_physical(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
        *self.list_calls.lock().unwrap() += 1;
        self.inner.list_physical(prefix)
    }
}

impl RefStore for ExistsCountingBackend {
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

impl LayoutRootStore for ExistsCountingBackend {
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

impl RemoteBackend for ExistsCountingBackend {
    fn capability(&self) -> &BackendCapability {
        &self.capability
    }
}

#[derive(Debug, Clone)]
struct InventoryListingForbiddenBackend {
    inner: MemoryBackend,
    capability: BackendCapability,
}

impl InventoryListingForbiddenBackend {
    fn new(inner: MemoryBackend) -> Self {
        Self {
            inner,
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

impl BlobStore for InventoryListingForbiddenBackend {
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
        if prefix == "objects/" || prefix == "bundles/index/" {
            anyhow::bail!("remote inventory listing is not allowed during resume for {prefix}");
        }
        self.inner.list_physical(prefix)
    }
}

impl RefStore for InventoryListingForbiddenBackend {
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

impl LayoutRootStore for InventoryListingForbiddenBackend {
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

impl RemoteBackend for InventoryListingForbiddenBackend {
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

#[derive(Debug, Clone)]
struct SingleWriterMemoryBackend {
    inner: MemoryBackend,
    capability: BackendCapability,
}

impl SingleWriterMemoryBackend {
    fn new() -> Self {
        Self {
            inner: MemoryBackend::new(),
            capability: BackendCapability {
                supports_conditional_put: false,
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

impl BlobStore for SingleWriterMemoryBackend {
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

impl RefStore for SingleWriterMemoryBackend {
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

impl LayoutRootStore for SingleWriterMemoryBackend {
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

impl RemoteBackend for SingleWriterMemoryBackend {
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
fn push_avoids_per_object_remote_exists_checks_for_missing_objects() {
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
    for index in 0..32usize {
        fs::write(
            repo_root.join(format!("file-{index:02}.txt")),
            format!("payload-{index:02}"),
        )
        .unwrap();
    }
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "exists-check-scaling".to_string(),
        })
        .unwrap();

    let remote = ExistsCountingBackend::new();

    let result = push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "exists-check-scaling-op".to_string(),
        },
    )
    .unwrap();

    assert!(result.uploaded_objects > 0);
    assert!(
        remote.list_call_count() >= 1,
        "expected push to inspect remote object listings"
    );
    assert!(
        remote.exists_call_count() <= 8,
        "expected push to avoid per-object remote exists checks, saw {} exists calls",
        remote.exists_call_count()
    );
}

#[test]
fn push_avoids_per_object_remote_exists_checks_when_validating_remote_ancestor_graph() {
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
    for index in 0..32usize {
        fs::write(
            repo_root.join(format!("file-{index:02}.txt")),
            format!("payload-v1-{index:02}"),
        )
        .unwrap();
    }
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "ancestor-exists-v1".to_string(),
        })
        .unwrap();

    let remote = ExistsCountingBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "ancestor-exists-v1-op".to_string(),
        },
    )
    .unwrap();

    fs::write(repo_root.join("file-00.txt"), "payload-v2-00").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "ancestor-exists-v2".to_string(),
        })
        .unwrap();

    remote.reset_counts();

    let result = push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "ancestor-exists-v2-op".to_string(),
        },
    )
    .unwrap();

    assert!(result.uploaded_objects > 0);
    assert!(
        remote.list_call_count() >= 1,
        "expected push to inspect remote inventory for ancestor validation"
    );
    assert!(
        remote.exists_call_count() <= 8,
        "expected push to avoid per-object remote exists checks while validating ancestors, saw {} exists calls",
        remote.exists_call_count()
    );
}

#[test]
fn resume_avoids_per_object_remote_exists_checks_for_pending_objects() {
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
    for index in 0..32usize {
        fs::write(
            repo_root.join(format!("file-{index:02}.txt")),
            format!("payload-{index:02}"),
        )
        .unwrap();
    }
    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "resume-exists-scaling".to_string(),
        })
        .unwrap();

    let manifest_store = ManifestStore::new(&repo_root);
    let reachable_object_ids = manifest_store
        .collect_reachable_object_ids(&commit.snapshot_id)
        .unwrap();

    let journal =
        e2v_sync::OperationJournal::new(repo_root.join(".e2v").join("journal").join("sync"))
            .unwrap();
    let operation_id =
        e2v_sync::OperationId::new("resume-exists-scaling-op".to_string()).unwrap();
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

    let remote = ExistsCountingBackend::new();

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
    assert!(
        remote.exists_call_count() <= 8,
        "expected resume to avoid per-object remote exists checks, saw {} exists calls",
        remote.exists_call_count()
    );
    assert_eq!(
        remote.list_call_count(),
        0,
        "expected resume to avoid loading remote object inventory while replaying journal batches"
    );
}

#[test]
fn resume_reuses_journal_recorded_bundle_locations_without_loading_remote_inventory() {
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
    for index in 0..8usize {
        fs::write(
            repo_root.join(format!("file-{index:02}.txt")),
            format!("payload-{index:02}"),
        )
        .unwrap();
    }
    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "resume-bundled-journal-locations".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "resume-bundled-journal-locations-op".to_string(),
        },
    )
    .unwrap();

    let manifest_store = ManifestStore::new(&repo_root);
    let reachable_object_ids = manifest_store
        .collect_reachable_object_ids(&commit.snapshot_id)
        .unwrap();
    let journal =
        e2v_sync::OperationJournal::new(repo_root.join(".e2v").join("journal").join("sync"))
            .unwrap();
    let operation_id =
        e2v_sync::OperationId::new("resume-bundled-journal-locations-op".to_string()).unwrap();
    for object_id in &reachable_object_ids {
        journal
            .record_uploaded(&operation_id, object_id, "object")
            .unwrap();
    }

    let guarded_remote = InventoryListingForbiddenBackend::new(remote.clone());
    let resumed = resume_push(
        &facade,
        &guarded_remote,
        ResumeOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: operation_id.value.clone(),
        },
    )
    .unwrap();

    assert_eq!(resumed.published_snapshot_id, commit.snapshot_id);
    assert!(resumed.skipped_uploaded_objects > 0);
}

#[test]
fn resume_skips_object_reupload_when_remote_state_is_already_complete() {
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
    for index in 0..16usize {
        fs::write(
            repo_root.join(format!("file-{index:02}.txt")),
            format!("payload-{index:02}"),
        )
        .unwrap();
    }
    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "resume-complete-remote".to_string(),
        })
        .unwrap();

    let remote = ExistsCountingBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "resume-complete-seed-op".to_string(),
        },
    )
    .unwrap();

    remote.reset_counts();

    let resumed = resume_push(
        &facade,
        &remote,
        ResumeOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "resume-complete-op".to_string(),
        },
    )
    .unwrap();

    assert_eq!(resumed.published_snapshot_id, commit.snapshot_id);
    assert_eq!(
        remote.object_put_call_count(),
        0,
        "expected resume to skip object reupload when remote is already complete"
    );
}

#[test]
fn resume_reuploads_only_missing_remote_objects_without_journal() {
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
    for index in 0..16usize {
        fs::write(
            repo_root.join(format!("file-{index:02}.txt")),
            format!("payload-{index:02}"),
        )
        .unwrap();
    }
    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "resume-single-missing-remote-object".to_string(),
        })
        .unwrap();

    let remote = ExistsCountingBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "resume-single-missing-remote-object-seed-op".to_string(),
        },
    )
    .unwrap();

    let missing_remote_object = remote
        .list_physical("objects/")
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let local_object_bytes = std::fs::read(
        repo_root
            .join(".e2v")
            .join("objects")
            .join(missing_remote_object.strip_prefix("objects/").unwrap()),
    )
    .unwrap();
    remote.delete_physical(&missing_remote_object).unwrap();
    remote.reset_counts();

    let resumed = resume_push(
        &facade,
        &remote,
        ResumeOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "resume-single-missing-remote-object-op".to_string(),
        },
    )
    .unwrap();

    assert_eq!(resumed.published_snapshot_id, commit.snapshot_id);
    assert_eq!(
        remote.object_put_call_count(),
        1,
        "expected resume to upload only the missing remote object when no journal records exist"
    );
    assert_eq!(
        remote.get_physical(&missing_remote_object).unwrap(),
        local_object_bytes
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
fn manifest_store_reachable_set_rejects_tampered_chunk_id_before_push_can_upload_it() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    let outside = temp.path().join("outside.json");
    fs::write(&outside, b"outside").unwrap();

    let facade = RepositoryFacade::new();
    facade
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
            message: "tampered-chunk-id".to_string(),
        })
        .unwrap();

    let manifest_store = ManifestStore::new(&repo_root);
    let snapshot = manifest_store.get_snapshot(&commit.snapshot_id).unwrap();
    let root_tree = manifest_store.get_tree_node(&snapshot.root_tree_id).unwrap();
    let file_entry = root_tree
        .entries
        .iter()
        .find(|entry| entry.name == "hello.txt")
        .unwrap()
        .clone();
    let mut file_manifest = manifest_store.get_file(&file_entry.object_id).unwrap();
    file_manifest.chunks = vec!["..\\evil".to_string()];

    let control_dir = repo_root.join(".e2v");
    let secrets = e2v_core::sync_support::open_repo_secrets_for_sync(&control_dir).unwrap();
    let object_store = e2v_store::DirectLayoutObjectStore::new(&control_dir, secrets);
    let tampered_file_id = object_store
        .put_object("file", &postcard::to_stdvec(&file_manifest).unwrap())
        .unwrap();

    let mut tampered_tree = root_tree.clone();
    tampered_tree
        .entries
        .iter_mut()
        .find(|entry| entry.name == "hello.txt")
        .unwrap()
        .object_id = tampered_file_id;
    let tampered_tree_id = object_store
        .put_object("tree", &postcard::to_stdvec(&tampered_tree).unwrap())
        .unwrap();

    let mut tampered_snapshot = snapshot.clone();
    tampered_snapshot.root_tree_id = tampered_tree_id;
    let tampered_snapshot_id = object_store
        .put_object("snapshot", &postcard::to_stdvec(&tampered_snapshot).unwrap())
        .unwrap();

    let error = manifest_store
        .collect_reachable_object_ids(&tampered_snapshot_id)
        .unwrap_err();

    assert!(
        error.to_string().contains("chunk")
            || error.to_string().contains("object id")
            || error.to_string().contains("path"),
        "unexpected error: {error:#}"
    );
    assert_eq!(fs::read(&outside).unwrap(), b"outside");
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
    assert!(
        rebuilt
            .read_ref(&RefToken::new(state.branch.token_hex.clone()))
            .unwrap()
            .is_some()
    );
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
fn resume_repairs_corrupted_existing_remote_object_instead_of_marking_it_verified() {
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
            message: "resume-corrupted-remote-object".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "resume-corrupted-remote-object-seed-op".to_string(),
        },
    )
    .unwrap();

    let manifest_store = ManifestStore::new(&repo_root);
    let reachable_object_ids = manifest_store
        .collect_reachable_object_ids(&commit.snapshot_id)
        .unwrap();
    let object_id = reachable_object_ids
        .iter()
        .find(|id| **id != commit.snapshot_id)
        .unwrap()
        .clone();
    let remote_object_path = format!("objects/{object_id}.json");
    let local_object_bytes = std::fs::read(
        repo_root
            .join(".e2v")
            .join("objects")
            .join(format!("{object_id}.json")),
    )
    .unwrap();
    let mut corrupted_bytes = remote.get_physical(&remote_object_path).unwrap();
    let flip_index = corrupted_bytes.len() / 2;
    corrupted_bytes[flip_index] ^= 0x01;
    remote
        .put_physical(&remote_object_path, &corrupted_bytes)
        .unwrap();

    let journal =
        e2v_sync::OperationJournal::new(repo_root.join(".e2v").join("journal").join("sync"))
            .unwrap();
    let operation_id =
        e2v_sync::OperationId::new("resume-corrupted-remote-object-op".to_string()).unwrap();
    journal
        .begin_operation(
            &operation_id,
            e2v_sync::OperationMetadata::push(state.branch.token_hex.clone(), None),
        )
        .unwrap();
    journal
        .plan_object(&operation_id, &object_id, "object")
        .unwrap();

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
        remote.get_physical(&remote_object_path).unwrap(),
        local_object_bytes,
        "resume should heal a corrupted remote object rather than marking it verified as-is"
    );
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
    assert!(
        push_error
            .to_string()
            .contains("simulated object upload interruption")
    );

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
    let operation_id =
        e2v_sync::OperationId::new("resume-batched-count-op".to_string()).unwrap();
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
fn resume_restores_control_ref_mirror_even_when_remote_ref_already_matches_local_head() {
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
            message: "resume-control-ref-mirror".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "resume-control-ref-mirror-op".to_string(),
        },
    )
    .unwrap();

    remote.delete_physical("control/refs/default.json").unwrap();
    remote
        .put_physical(
            "transactions/active/resume-control-ref-mirror-op.intent",
            br#"{"operation_id":"resume-control-ref-mirror-op","target_branch_token":"main"}"#,
        )
        .unwrap();

    let resumed = resume_push(
        &facade,
        &remote,
        ResumeOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "resume-control-ref-mirror-op".to_string(),
        },
    )
    .unwrap();

    assert_eq!(resumed.published_snapshot_id, commit.snapshot_id);
    assert_eq!(
        remote.get_physical("control/refs/default.json").unwrap(),
        e2v_core::sync_support::read_default_ref_bytes(&repo_root).unwrap()
    );
    assert!(
        !remote.exists_physical("transactions/active/resume-control-ref-mirror-op.intent"),
        "resume should clean up the active intent once the control-plane mirror is restored"
    );
}

#[test]
fn resume_restores_missing_config_when_remote_ref_and_control_ref_mirror_match() {
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
            message: "resume-missing-config".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "resume-missing-config-op".to_string(),
        },
    )
    .unwrap();

    remote.delete_physical("control/config.json").unwrap();

    let resumed = resume_push(
        &facade,
        &remote,
        ResumeOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "resume-missing-config-op".to_string(),
        },
    )
    .unwrap();

    assert_eq!(resumed.published_snapshot_id, commit.snapshot_id);
    assert_eq!(
        remote.get_physical("control/config.json").unwrap(),
        e2v_core::sync_support::read_config_bytes(&repo_root).unwrap()
    );
}

#[test]
fn resume_repairs_stale_config_when_remote_ref_and_control_ref_mirror_match() {
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
            message: "resume-stale-config".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "resume-stale-config-op".to_string(),
        },
    )
    .unwrap();

    remote
        .put_physical("control/config.json", br#"{"stale":true}"#)
        .unwrap();

    let resumed = resume_push(
        &facade,
        &remote,
        ResumeOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "resume-stale-config-op".to_string(),
        },
    )
    .unwrap();

    assert_eq!(resumed.published_snapshot_id, commit.snapshot_id);
    assert_eq!(
        remote.get_physical("control/config.json").unwrap(),
        e2v_core::sync_support::read_config_bytes(&repo_root).unwrap()
    );
}

#[test]
fn resume_restores_missing_layout_root_when_remote_ref_and_control_ref_mirror_match() {
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
            message: "resume-missing-layout-root".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "resume-missing-layout-root-op".to_string(),
        },
    )
    .unwrap();

    remote.delete_physical("layout_root.json").unwrap();

    let resumed = resume_push(
        &facade,
        &remote,
        ResumeOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "resume-missing-layout-root-op".to_string(),
        },
    )
    .unwrap();

    assert_eq!(resumed.published_snapshot_id, commit.snapshot_id);
    assert_eq!(
        remote.get_physical("layout_root.json").unwrap(),
        e2v_core::sync_support::read_layout_root_bytes(&repo_root).unwrap()
    );
}

#[test]
fn resume_repairs_stale_layout_root_when_remote_ref_and_control_ref_mirror_match() {
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
            message: "resume-stale-layout-root".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "resume-stale-layout-root-op".to_string(),
        },
    )
    .unwrap();

    remote
        .put_physical("layout_root.json", br#"{"stale":true}"#)
        .unwrap();

    let resumed = resume_push(
        &facade,
        &remote,
        ResumeOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "resume-stale-layout-root-op".to_string(),
        },
    )
    .unwrap();

    assert_eq!(resumed.published_snapshot_id, commit.snapshot_id);
    assert_eq!(
        remote.get_physical("layout_root.json").unwrap(),
        e2v_core::sync_support::read_layout_root_bytes(&repo_root).unwrap()
    );
}

#[test]
fn resume_restores_missing_keyring_pointer_when_remote_ref_and_control_ref_mirror_match() {
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
            message: "resume-missing-keyring-pointer".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "resume-missing-keyring-pointer-op".to_string(),
        },
    )
    .unwrap();

    remote
        .delete_physical("control/keyring/keyring.current")
        .unwrap();

    let resumed = resume_push(
        &facade,
        &remote,
        ResumeOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "resume-missing-keyring-pointer-op".to_string(),
        },
    )
    .unwrap();

    assert_eq!(resumed.published_snapshot_id, commit.snapshot_id);
    assert_eq!(
        remote
            .get_physical("control/keyring/keyring.current")
            .unwrap(),
        std::fs::read(
            repo_root
                .join(".e2v")
                .join("keyring")
                .join("keyring.current")
        )
        .unwrap()
    );
}

#[test]
fn resume_repairs_stale_keyring_pointer_when_remote_ref_and_control_ref_mirror_match() {
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
            message: "resume-stale-keyring-pointer".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "resume-stale-keyring-pointer-op".to_string(),
        },
    )
    .unwrap();

    remote
        .put_physical(
            "control/keyring/keyring.current",
            br#"{"generation":"stale"}"#,
        )
        .unwrap();

    let resumed = resume_push(
        &facade,
        &remote,
        ResumeOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "resume-stale-keyring-pointer-op".to_string(),
        },
    )
    .unwrap();

    assert_eq!(resumed.published_snapshot_id, commit.snapshot_id);
    assert_eq!(
        remote
            .get_physical("control/keyring/keyring.current")
            .unwrap(),
        std::fs::read(
            repo_root
                .join(".e2v")
                .join("keyring")
                .join("keyring.current")
        )
        .unwrap()
    );
}

#[test]
fn resume_restores_missing_keyring_generation_when_remote_ref_and_control_ref_mirror_match() {
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
            message: "resume-missing-keyring-generation".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "resume-missing-keyring-generation-op".to_string(),
        },
    )
    .unwrap();

    let pointer_bytes = remote
        .get_physical("control/keyring/keyring.current")
        .unwrap();
    let pointer: serde_json::Value = serde_json::from_slice(&pointer_bytes).unwrap();
    let current = pointer["current"].as_str().unwrap();
    remote
        .delete_physical(&format!("control/keyring/{current}"))
        .unwrap();

    let resumed = resume_push(
        &facade,
        &remote,
        ResumeOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "resume-missing-keyring-generation-op".to_string(),
        },
    )
    .unwrap();

    assert_eq!(resumed.published_snapshot_id, commit.snapshot_id);
    assert_eq!(
        remote
            .get_physical(&format!("control/keyring/{current}"))
            .unwrap(),
        std::fs::read(repo_root.join(".e2v").join("keyring").join(current)).unwrap()
    );
}

#[test]
fn resume_cleans_up_stale_active_intent_when_remote_state_is_already_complete() {
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
            message: "resume-cleanup-only".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "resume-cleanup-only-op".to_string(),
        },
    )
    .unwrap();

    remote
        .put_physical(
            "transactions/active/resume-cleanup-only-op.intent",
            br#"{"operation_id":"resume-cleanup-only-op","target_branch_token":"main"}"#,
        )
        .unwrap();

    let resumed = resume_push(
        &facade,
        &remote,
        ResumeOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "resume-cleanup-only-op".to_string(),
        },
    )
    .unwrap();

    assert_eq!(resumed.published_snapshot_id, commit.snapshot_id);
    assert!(
        !remote.exists_physical("transactions/active/resume-cleanup-only-op.intent"),
        "resume should clean up a stale active intent even when no control-plane repair is needed"
    );
}

#[test]
fn resume_cleans_up_stale_single_writer_lease_when_remote_state_is_already_complete() {
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
            message: "resume-cleanup-single-writer".to_string(),
        })
        .unwrap();

    let remote = SingleWriterMemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "resume-cleanup-single-writer-op".to_string(),
        },
    )
    .unwrap();

    remote
        .put_physical(
            &format!(
                "transactions/active/{}.intent",
                "resume-cleanup-single-writer-op"
            ),
            br#"{"operation_id":"resume-cleanup-single-writer-op","target_branch_token":"main"}"#,
        )
        .unwrap();
    remote
        .put_physical(
            &format!("leases/{}.lock", state.branch.token_hex),
            format!(
                r#"{{"operation_id":"{}","target_branch_token":"{}"}}"#,
                "resume-cleanup-single-writer-op", state.branch.token_hex
            )
            .as_bytes(),
        )
        .unwrap();

    let resumed = resume_push(
        &facade,
        &remote,
        ResumeOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "resume-cleanup-single-writer-op".to_string(),
        },
    )
    .unwrap();

    assert_eq!(resumed.published_snapshot_id, commit.snapshot_id);
    assert!(!remote.exists_physical("transactions/active/resume-cleanup-single-writer-op.intent"));
    assert!(!remote.exists_physical(&format!("leases/{}.lock", state.branch.token_hex)));
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
fn resume_does_not_publish_new_keyring_pointer_before_ref_cas_succeeds() {
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
            message: "resume-password-rotation".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "resume-password-rotation-seed-op".to_string(),
        },
    )
    .unwrap();
    let original_keyring_pointer = remote
        .get_physical("control/keyring/keyring.current")
        .unwrap();

    facade
        .change_password(
            &repo_root,
            "correct horse battery staple",
            "new horse battery staple",
        )
        .unwrap();

    let journal =
        e2v_sync::OperationJournal::new(repo_root.join(".e2v").join("journal").join("sync"))
            .unwrap();
    let operation_id =
        e2v_sync::OperationId::new("resume-password-rotation-op".to_string()).unwrap();
    journal
        .begin_operation(
            &operation_id,
            e2v_sync::OperationMetadata::push(state.branch.token_hex.clone(), None),
        )
        .unwrap();

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
            operation_id: operation_id.value.clone(),
        },
    )
    .unwrap_err();

    assert!(error.to_string().contains("needs-rebase"));
    assert_eq!(
        remote
            .get_physical("control/keyring/keyring.current")
            .unwrap(),
        original_keyring_pointer,
        "resume must not publish a new keyring pointer before ref CAS succeeds"
    );
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
    assert!(
        remote
            .read_ref(&RefToken::new(state.branch.token_hex.clone()))
            .unwrap()
            .is_none()
    );
    assert!(second.snapshot_id.len() > 10);
}

#[test]
fn push_rejects_operation_id_with_path_traversal_before_mutating_remote_state() {
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
            message: "invalid-operation-id".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();

    let error = push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "../evil".to_string(),
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("operation")
            || error.to_string().contains("path traversal")
            || error.to_string().contains("invalid"),
        "unexpected error: {error:#}"
    );
    assert!(remote.list_physical("objects/").unwrap().is_empty());
    assert!(remote.list_physical("bundles/index/").unwrap().is_empty());
    assert!(remote.list_physical("bundles/data/").unwrap().is_empty());
    assert!(
        remote
            .read_ref(&RefToken::new(state.branch.token_hex.clone()))
            .unwrap()
            .is_none()
    );
    assert!(!remote.exists_physical("transactions/active/../evil.intent"));
}

#[test]
fn push_rejects_branch_token_with_path_traversal_before_mutating_remote_state() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade
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
            message: "invalid-branch-token".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();

    let error = push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: "../evil".to_string(),
            operation_id: "invalid-branch-token-op".to_string(),
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("branch")
            || error.to_string().contains("path traversal")
            || error.to_string().contains("invalid"),
        "unexpected error: {error:#}"
    );
    assert!(remote.list_physical("objects/").unwrap().is_empty());
    assert!(remote.list_physical("bundles/index/").unwrap().is_empty());
    assert!(remote.list_physical("bundles/data/").unwrap().is_empty());
    assert!(!remote.exists_physical("leases/../evil.lock"));
    assert!(!remote.exists_physical("transactions/active/invalid-branch-token-op.intent"));
}

#[test]
fn push_rejects_corrupted_remote_parent_snapshot_even_when_object_path_exists() {
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
    let first = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "first".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "corrupted-parent-seed-op".to_string(),
        },
    )
    .unwrap();

    fs::write(repo_root.join("hello.txt"), b"second").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "second".to_string(),
        })
        .unwrap();

    let remote_parent_path = format!("objects/{}.json", first.snapshot_id);
    let mut corrupted_parent_bytes = remote.get_physical(&remote_parent_path).unwrap();
    let flip_index = corrupted_parent_bytes.len() / 2;
    corrupted_parent_bytes[flip_index] ^= 0x01;
    remote
        .put_physical(&remote_parent_path, &corrupted_parent_bytes)
        .unwrap();

    let error = push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "corrupted-parent-op".to_string(),
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("ancestor")
            || error.to_string().contains("authentication")
            || error.to_string().contains("remote"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn push_accepts_healthy_remote_parent_snapshot_when_object_verifies() {
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
    let first = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "first".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "healthy-parent-seed-op".to_string(),
        },
    )
    .unwrap();

    fs::write(repo_root.join("hello.txt"), b"second").unwrap();
    let second = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "second".to_string(),
        })
        .unwrap();

    let pushed = push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "healthy-parent-op".to_string(),
        },
    )
    .unwrap();

    assert_eq!(pushed.published_snapshot_id, second.snapshot_id);
    assert_ne!(first.snapshot_id, second.snapshot_id);
}

#[test]
fn push_rejects_remote_parent_snapshot_when_reachable_chunk_is_corrupted() {
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
    let first = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "first".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "corrupted-parent-chunk-seed-op".to_string(),
        },
    )
    .unwrap();

    let manifest_store = ManifestStore::new(&repo_root);
    let reachable_ids = manifest_store
        .collect_reachable_object_ids(&first.snapshot_id)
        .unwrap();
    let chunk_id = reachable_ids
        .into_iter()
        .find(|object_id| facade.verify_object(&repo_root, object_id, "chunk").is_ok())
        .unwrap();
    let remote_chunk_path = format!("objects/{chunk_id}.json");
    let mut bytes = remote.get_physical(&remote_chunk_path).unwrap();
    let last_index = bytes.len() - 1;
    bytes[last_index] ^= 0x01;
    remote.put_physical(&remote_chunk_path, &bytes).unwrap();

    fs::write(repo_root.join("hello.txt"), b"second").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "second".to_string(),
        })
        .unwrap();

    let error = push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "corrupted-parent-chunk-op".to_string(),
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("ancestor")
            || error.to_string().contains("verification")
            || error.to_string().contains("failed"),
        "unexpected error: {error:#}"
    );
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
fn push_does_not_publish_control_ref_mirror_before_ref_cas_succeeds() {
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
    assert!(
        !remote.exists_physical("control/refs/default.json"),
        "control ref mirror must not be published before ref CAS succeeds"
    );
}

#[test]
fn push_does_not_publish_new_keyring_pointer_before_ref_cas_succeeds() {
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
            message: "seed-password-rotation".to_string(),
        })
        .unwrap();

    let remote = RefConflictBackend::new();
    push_head(
        &facade,
        &remote.inner,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "seed-password-rotation-op".to_string(),
        },
    )
    .unwrap();
    let original_keyring_pointer = remote
        .get_physical("control/keyring/keyring.current")
        .unwrap();

    facade
        .change_password(
            &repo_root,
            "correct horse battery staple",
            "new horse battery staple",
        )
        .unwrap();

    let error = push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "cas-race-password-rotation-op".to_string(),
        },
    )
    .unwrap_err();

    assert!(error.to_string().contains("needs-rebase"));
    assert_eq!(
        remote
            .get_physical("control/keyring/keyring.current")
            .unwrap(),
        original_keyring_pointer,
        "keyring pointer must not be published before ref CAS succeeds"
    );
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
    let operation_id =
        e2v_sync::OperationId::new("verify-remote-before-ref-op".to_string()).unwrap();
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

#[test]
fn push_fast_forward_bundle_ancestor_validation_avoids_repeating_bundle_range_reads() {
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
    for index in 0..24usize {
        fs::write(
            source_repo_root.join(format!("file-{index:02}.txt")),
            format!("payload-v1-{index:02}"),
        )
        .unwrap();
    }
    source
        .commit(CommitOptions {
            repo_root: source_repo_root.clone(),
            message: "bundle-ancestor-v1".to_string(),
        })
        .unwrap();

    let remote = ExistsCountingBackend::new();
    push_head(
        &source,
        &remote,
        PushOptions {
            repo_root: source_repo_root.clone(),
            branch_token: source_state.branch.token_hex.clone(),
            operation_id: "bundle-ancestor-v1-op".to_string(),
        },
    )
    .unwrap();
    assert!(remote.list_physical("objects/").unwrap().is_empty());

    let clone_repo_root = temp.path().join("clone");
    e2v_sync::clone_remote(
        &remote,
        e2v_sync::CloneOptions {
            repo_root: clone_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
            branch_token: source_state.branch.token_hex.clone(),
        },
    )
    .unwrap();

    for index in 0..24usize {
        fs::write(
            clone_repo_root.join(format!("file-{index:02}.txt")),
            format!("payload-v2-{index:02}"),
        )
        .unwrap();
    }
    let clone_facade = RepositoryFacade::new();
    clone_facade
        .commit(CommitOptions {
            repo_root: clone_repo_root.clone(),
            message: "bundle-ancestor-v2".to_string(),
        })
        .unwrap();

    remote.reset_counts();

    let second_push = push_head(
        &clone_facade,
        &remote,
        PushOptions {
            repo_root: clone_repo_root.clone(),
            branch_token: source_state.branch.token_hex.clone(),
            operation_id: "bundle-ancestor-v2-op".to_string(),
        },
    )
    .unwrap();

    assert!(second_push.uploaded_objects > 0);
    let range_read_paths = remote.range_read_paths();
    let distinct_bundle_paths = range_read_paths
        .iter()
        .filter(|path| path.starts_with("bundles/data/"))
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    assert!(
        range_read_paths.len() <= distinct_bundle_paths.len() + 2,
        "expected ancestor validation to avoid repeated bundle range reads, saw {:?}",
        range_read_paths
    );
}

#[test]
fn push_deep_fast_forward_bundle_ancestor_validation_reuses_remote_bundle_reads_across_snapshots() {
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
    for index in 0..24usize {
        fs::write(
            source_repo_root.join(format!("file-{index:02}.txt")),
            format!("payload-v1-{index:02}"),
        )
        .unwrap();
    }
    source
        .commit(CommitOptions {
            repo_root: source_repo_root.clone(),
            message: "deep-bundle-ancestor-v1".to_string(),
        })
        .unwrap();

    let remote = ExistsCountingBackend::new();
    push_head(
        &source,
        &remote,
        PushOptions {
            repo_root: source_repo_root.clone(),
            branch_token: source_state.branch.token_hex.clone(),
            operation_id: "deep-bundle-ancestor-v1-op".to_string(),
        },
    )
    .unwrap();
    assert!(remote.list_physical("objects/").unwrap().is_empty());

    let clone_repo_root = temp.path().join("clone");
    e2v_sync::clone_remote(
        &remote,
        e2v_sync::CloneOptions {
            repo_root: clone_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
            branch_token: source_state.branch.token_hex.clone(),
        },
    )
    .unwrap();

    let clone_facade = RepositoryFacade::new();
    for version in 2..=5usize {
        fs::write(clone_repo_root.join("rolling.txt"), format!("rolling-{version}")).unwrap();
        clone_facade
            .commit(CommitOptions {
                repo_root: clone_repo_root.clone(),
                message: format!("deep-bundle-ancestor-v{version}"),
            })
            .unwrap();
        push_head(
            &clone_facade,
            &remote,
            PushOptions {
                repo_root: clone_repo_root.clone(),
                branch_token: source_state.branch.token_hex.clone(),
                operation_id: format!("deep-bundle-ancestor-v{version}-op"),
            },
        )
        .unwrap();
    }

    fs::write(clone_repo_root.join("rolling.txt"), "rolling-6").unwrap();
    clone_facade
        .commit(CommitOptions {
            repo_root: clone_repo_root.clone(),
            message: "deep-bundle-ancestor-v6".to_string(),
        })
        .unwrap();

    remote.reset_counts();

    let pushed = push_head(
        &clone_facade,
        &remote,
        PushOptions {
            repo_root: clone_repo_root.clone(),
            branch_token: source_state.branch.token_hex.clone(),
            operation_id: "deep-bundle-ancestor-v6-op".to_string(),
        },
    )
    .unwrap();

    assert!(pushed.uploaded_objects > 0);
    let range_read_paths = remote.range_read_paths();
    let distinct_bundle_paths = range_read_paths
        .iter()
        .filter(|path| path.starts_with("bundles/data/"))
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    assert!(
        range_read_paths.len() <= distinct_bundle_paths.len() + 6,
        "expected deep ancestor validation to reuse bundle reads across snapshots, saw {:?}",
        range_read_paths
    );
}

#[test]
fn push_is_idempotent_when_remote_ref_already_points_at_local_head() {
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
            message: "idempotent-push".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    let first = push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "idempotent-push-1".to_string(),
        },
    )
    .unwrap();
    assert_eq!(first.published_snapshot_id, commit.snapshot_id);

    let second = push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "idempotent-push-2".to_string(),
        },
    )
    .unwrap();

    assert_eq!(second.published_snapshot_id, commit.snapshot_id);
}

#[test]
fn push_idempotent_noop_avoids_remote_inventory_listing_when_ref_already_matches_head() {
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
            message: "idempotent-noop".to_string(),
        })
        .unwrap();

    let remote = ExistsCountingBackend::new();
    let first = push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "idempotent-noop-1".to_string(),
        },
    )
    .unwrap();
    assert_eq!(first.published_snapshot_id, commit.snapshot_id);

    remote.reset_counts();

    let second = push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "idempotent-noop-2".to_string(),
        },
    )
    .unwrap();

    assert_eq!(second.published_snapshot_id, commit.snapshot_id);
    assert_eq!(remote.object_put_call_count(), 0);
    assert_eq!(
        remote.list_call_count(),
        0,
        "idempotent noop push should not scan remote object inventory"
    );
}

#[test]
fn push_password_rotation_still_republishes_control_plane_even_when_head_is_unchanged() {
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
            message: "idempotent-password-rotation".to_string(),
        })
        .unwrap();

    let remote = ExistsCountingBackend::new();
    let first = push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "idempotent-password-rotation-1".to_string(),
        },
    )
    .unwrap();
    assert_eq!(first.published_snapshot_id, commit.snapshot_id);

    facade
        .change_password(
            &repo_root,
            "correct horse battery staple",
            "new horse battery staple",
        )
        .unwrap();
    remote.reset_counts();

    let second = push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "idempotent-password-rotation-2".to_string(),
        },
    )
    .unwrap();

    assert_eq!(second.published_snapshot_id, commit.snapshot_id);
    assert_eq!(second.uploaded_objects, 0);
    assert!(
        remote.exists_physical("control/keyring/keyring.2"),
        "password rotation should still publish the next keyring generation"
    );
}

#[test]
fn push_republishes_control_plane_when_password_rotates_without_new_snapshot() {
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
            message: "password-rotation".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    let first = push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "password-rotation-push-1".to_string(),
        },
    )
    .unwrap();
    assert_eq!(first.published_snapshot_id, commit.snapshot_id);

    let original_keyring_pointer = remote
        .get_physical("control/keyring/keyring.current")
        .unwrap();

    facade
        .change_password(
            &repo_root,
            "correct horse battery staple",
            "new horse battery staple",
        )
        .unwrap();

    let second = push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "password-rotation-push-2".to_string(),
        },
    )
    .unwrap();

    assert_eq!(second.published_snapshot_id, commit.snapshot_id);
    assert!(remote.exists_physical("control/keyring/keyring.2"));
    assert_ne!(
        remote
            .get_physical("control/keyring/keyring.current")
            .unwrap(),
        original_keyring_pointer
    );
}

#[test]
fn push_does_not_upload_local_keyring_lock_file_to_remote_control_plane() {
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
            message: "ignore-keyring-lock".to_string(),
        })
        .unwrap();
    fs::write(
        repo_root.join(".e2v").join("keyring").join("keyring.lock"),
        b"locked",
    )
    .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-ignore-keyring-lock".to_string(),
        },
    )
    .unwrap();

    assert!(
        !remote.exists_physical("control/keyring/keyring.lock"),
        "push should not upload local keyring lock files"
    );
}
