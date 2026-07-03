use std::fs;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use e2v_core::{CommitOptions, InitOptions, ManifestStoreApi, RepositoryFacade};
use e2v_store::{
    BlobStore, DirectLayoutObjectStore, LayoutRootStore, ListedRef, MemoryBackend,
    OpendalMemoryBackend, RefStore, RefToken, RemoteBackend, S3CompatibleMockBackend, StoredRef,
    WebdavAlistMockBackend,
};
use tempfile::tempdir;

use e2v_sync::{
    CloneOptions, EnableObliviousLayoutOptions, FetchOptions, HistoricalRewriteOptions,
    PushOptions, clone_remote, enable_oblivious_layout, fetch_remote, historical_rewrite_remote,
    push_head,
};

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

fn keyring_pointer_ref_token(repo_root: &std::path::Path) -> RefToken {
    RefToken::new(format!(
        "keyring/{}",
        e2v_core::sync_support::read_repo_id(repo_root).unwrap()
    ))
}

#[derive(Clone, Debug)]
struct KeyringPointerDisappearsAfterRefReadRemote {
    inner: MemoryBackend,
    keyring_pointer_deleted: Arc<AtomicBool>,
}

impl KeyringPointerDisappearsAfterRefReadRemote {
    fn new(inner: MemoryBackend) -> Self {
        Self {
            inner,
            keyring_pointer_deleted: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl BlobStore for KeyringPointerDisappearsAfterRefReadRemote {
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

impl RefStore for KeyringPointerDisappearsAfterRefReadRemote {
    fn read_ref(&self, token: &RefToken) -> anyhow::Result<Option<StoredRef>> {
        let stored = self.inner.read_ref(token)?;
        if stored.is_some() && !self.keyring_pointer_deleted.swap(true, Ordering::SeqCst) {
            let pointer_token = self
                .inner
                .list_refs()?
                .into_iter()
                .find(|listed| listed.token.value.starts_with("keyring/"))
                .map(|listed| listed.stored.value.bytes)
                .or_else(|| {
                    self.inner
                        .get_physical("control/keyring/keyring.current")
                        .ok()
                });
            if let Some(pointer_bytes) = pointer_token {
                let pointer: serde_json::Value = serde_json::from_slice(&pointer_bytes).unwrap();
                let current = pointer["current"].as_str().unwrap();
                self.inner
                    .delete_physical(&format!("control/keyring/{current}"))?;
            }
        }
        Ok(stored)
    }

    fn list_refs(&self) -> anyhow::Result<Vec<ListedRef>> {
        self.inner.list_refs()
    }

    fn compare_and_swap_ref(
        &self,
        token: &RefToken,
        expected: Option<e2v_store::RefVersion>,
        next: e2v_store::EncryptedRef,
    ) -> anyhow::Result<e2v_store::CasResult> {
        self.inner.compare_and_swap_ref(token, expected, next)
    }
}

impl LayoutRootStore for KeyringPointerDisappearsAfterRefReadRemote {
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

impl RemoteBackend for KeyringPointerDisappearsAfterRefReadRemote {
    fn capability(&self) -> &e2v_store::BackendCapability {
        self.inner.capability()
    }
}

#[derive(Clone, Debug)]
struct KeyringPointerHiddenFromListRemote {
    inner: MemoryBackend,
    hidden_paths: Vec<String>,
}

impl KeyringPointerHiddenFromListRemote {
    fn new(inner: MemoryBackend) -> Self {
        Self {
            inner,
            hidden_paths: vec!["control/keyring/keyring.current".to_string()],
        }
    }

    fn with_hidden_paths(inner: MemoryBackend, hidden_paths: Vec<String>) -> Self {
        Self {
            inner,
            hidden_paths,
        }
    }
}

impl BlobStore for KeyringPointerHiddenFromListRemote {
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
        let mut listed = self.inner.list_physical(prefix)?;
        if prefix == "control/keyring/" {
            listed.retain(|path| !self.hidden_paths.iter().any(|hidden| hidden == path));
        }
        Ok(listed)
    }
}

impl RefStore for KeyringPointerHiddenFromListRemote {
    fn read_ref(&self, token: &RefToken) -> anyhow::Result<Option<StoredRef>> {
        self.inner.read_ref(token)
    }

    fn list_refs(&self) -> anyhow::Result<Vec<ListedRef>> {
        self.inner.list_refs()
    }

    fn compare_and_swap_ref(
        &self,
        token: &RefToken,
        expected: Option<e2v_store::RefVersion>,
        next: e2v_store::EncryptedRef,
    ) -> anyhow::Result<e2v_store::CasResult> {
        self.inner.compare_and_swap_ref(token, expected, next)
    }
}

impl LayoutRootStore for KeyringPointerHiddenFromListRemote {
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

impl RemoteBackend for KeyringPointerHiddenFromListRemote {
    fn capability(&self) -> &e2v_store::BackendCapability {
        self.inner.capability()
    }
}

#[derive(Clone, Debug)]
struct KeyringPointerRefHiddenRemote {
    inner: MemoryBackend,
}

impl KeyringPointerRefHiddenRemote {
    fn new(inner: MemoryBackend) -> Self {
        Self { inner }
    }
}

impl BlobStore for KeyringPointerRefHiddenRemote {
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

impl RefStore for KeyringPointerRefHiddenRemote {
    fn read_ref(&self, token: &RefToken) -> anyhow::Result<Option<StoredRef>> {
        if token.value.starts_with("keyring/") {
            return Ok(None);
        }
        self.inner.read_ref(token)
    }

    fn list_refs(&self) -> anyhow::Result<Vec<ListedRef>> {
        Ok(self
            .inner
            .list_refs()?
            .into_iter()
            .filter(|listed| !listed.token.value.starts_with("keyring/"))
            .collect())
    }

    fn compare_and_swap_ref(
        &self,
        token: &RefToken,
        expected: Option<e2v_store::RefVersion>,
        next: e2v_store::EncryptedRef,
    ) -> anyhow::Result<e2v_store::CasResult> {
        self.inner.compare_and_swap_ref(token, expected, next)
    }
}

impl LayoutRootStore for KeyringPointerRefHiddenRemote {
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

impl RemoteBackend for KeyringPointerRefHiddenRemote {
    fn capability(&self) -> &e2v_store::BackendCapability {
        self.inner.capability()
    }
}

#[derive(Clone, Debug)]
struct PackIndexListingForbiddenRemote {
    inner: MemoryBackend,
}

impl PackIndexListingForbiddenRemote {
    fn new(inner: MemoryBackend) -> Self {
        Self { inner }
    }
}

impl BlobStore for PackIndexListingForbiddenRemote {
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
        if prefix == "packs/index/" {
            anyhow::bail!("pack index listing disabled for test");
        }
        self.inner.list_physical(prefix)
    }
}

impl RefStore for PackIndexListingForbiddenRemote {
    fn read_ref(&self, token: &RefToken) -> anyhow::Result<Option<StoredRef>> {
        self.inner.read_ref(token)
    }

    fn list_refs(&self) -> anyhow::Result<Vec<ListedRef>> {
        self.inner.list_refs()
    }

    fn compare_and_swap_ref(
        &self,
        token: &RefToken,
        expected: Option<e2v_store::RefVersion>,
        next: e2v_store::EncryptedRef,
    ) -> anyhow::Result<e2v_store::CasResult> {
        self.inner.compare_and_swap_ref(token, expected, next)
    }
}

impl LayoutRootStore for PackIndexListingForbiddenRemote {
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

impl RemoteBackend for PackIndexListingForbiddenRemote {
    fn capability(&self) -> &e2v_store::BackendCapability {
        self.inner.capability()
    }
}

#[derive(Clone, Debug)]
struct PackDataRangeReadForbiddenRemote {
    inner: MemoryBackend,
}

impl PackDataRangeReadForbiddenRemote {
    fn new(inner: MemoryBackend) -> Self {
        Self { inner }
    }
}

impl BlobStore for PackDataRangeReadForbiddenRemote {
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
        if relative_path.starts_with("packs/data/") {
            anyhow::bail!("pack data range reads disabled for test");
        }
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

impl RefStore for PackDataRangeReadForbiddenRemote {
    fn read_ref(&self, token: &RefToken) -> anyhow::Result<Option<StoredRef>> {
        self.inner.read_ref(token)
    }

    fn list_refs(&self) -> anyhow::Result<Vec<ListedRef>> {
        self.inner.list_refs()
    }

    fn compare_and_swap_ref(
        &self,
        token: &RefToken,
        expected: Option<e2v_store::RefVersion>,
        next: e2v_store::EncryptedRef,
    ) -> anyhow::Result<e2v_store::CasResult> {
        self.inner.compare_and_swap_ref(token, expected, next)
    }
}

impl LayoutRootStore for PackDataRangeReadForbiddenRemote {
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

impl RemoteBackend for PackDataRangeReadForbiddenRemote {
    fn capability(&self) -> &e2v_store::BackendCapability {
        self.inner.capability()
    }
}

#[derive(Clone, Debug)]
struct GetTrackingBackend {
    inner: MemoryBackend,
    fetched_paths: Arc<Mutex<Vec<String>>>,
    range_read_paths: Arc<Mutex<Vec<String>>>,
    list_paths: Arc<Mutex<Vec<String>>>,
}

impl GetTrackingBackend {
    fn new(inner: MemoryBackend) -> Self {
        Self {
            inner,
            fetched_paths: Arc::new(Mutex::new(Vec::new())),
            range_read_paths: Arc::new(Mutex::new(Vec::new())),
            list_paths: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn reset_gets(&self) {
        self.fetched_paths.lock().unwrap().clear();
        self.range_read_paths.lock().unwrap().clear();
        self.list_paths.lock().unwrap().clear();
    }

    fn fetched_paths(&self) -> Vec<String> {
        self.fetched_paths.lock().unwrap().clone()
    }

    fn range_read_paths(&self) -> Vec<String> {
        self.range_read_paths.lock().unwrap().clone()
    }

    fn list_paths(&self) -> Vec<String> {
        self.list_paths.lock().unwrap().clone()
    }
}

impl BlobStore for GetTrackingBackend {
    fn put_physical(&self, relative_path: &str, bytes: &[u8]) -> anyhow::Result<()> {
        self.inner.put_physical(relative_path, bytes)
    }

    fn put_physical_if_absent(&self, relative_path: &str, bytes: &[u8]) -> anyhow::Result<bool> {
        self.inner.put_physical_if_absent(relative_path, bytes)
    }

    fn get_physical(&self, relative_path: &str) -> anyhow::Result<Vec<u8>> {
        self.fetched_paths
            .lock()
            .unwrap()
            .push(relative_path.to_string());
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
        self.list_paths.lock().unwrap().push(prefix.to_string());
        self.inner.list_physical(prefix)
    }
}

impl RefStore for GetTrackingBackend {
    fn read_ref(&self, token: &RefToken) -> anyhow::Result<Option<StoredRef>> {
        self.inner.read_ref(token)
    }

    fn list_refs(&self) -> anyhow::Result<Vec<ListedRef>> {
        self.inner.list_refs()
    }

    fn compare_and_swap_ref(
        &self,
        token: &RefToken,
        expected: Option<e2v_store::RefVersion>,
        next: e2v_store::EncryptedRef,
    ) -> anyhow::Result<e2v_store::CasResult> {
        self.inner.compare_and_swap_ref(token, expected, next)
    }
}

impl LayoutRootStore for GetTrackingBackend {
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

impl RemoteBackend for GetTrackingBackend {
    fn capability(&self) -> &e2v_store::BackendCapability {
        self.inner.capability()
    }
}

fn add_unreachable_remote_chunk_object(
    remote: &MemoryBackend,
    repo_root: &std::path::Path,
) -> String {
    let control_dir = repo_root.join(".e2v");
    let secrets = e2v_core::sync_support::open_repo_secrets_for_sync(&control_dir).unwrap();
    let object_store = DirectLayoutObjectStore::new(&control_dir, secrets);
    let stray_object_id = object_store
        .put_object("chunk", b"unreachable remote object")
        .unwrap();
    let stray_bytes = fs::read(
        control_dir
            .join("objects")
            .join(format!("{stray_object_id}.json")),
    )
    .unwrap();
    remote
        .put_physical(&format!("objects/{stray_object_id}.json"), &stray_bytes)
        .unwrap();
    stray_object_id
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
    assert!(
        remote
            .read_ref(&keyring_pointer_ref_token(&target_repo_root))
            .unwrap()
            .is_none(),
        "fresh local repository should not share the remote keyring ref token"
    );

    let result = fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
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
    assert!(
        target_repo_root
            .join(".e2v")
            .join("objects")
            .read_dir()
            .unwrap()
            .count()
            > 0
    );
}

#[test]
fn fetch_does_not_download_unreachable_remote_loose_objects_on_initial_sync() {
    let (temp, _facade, source_repo_root, branch_token, remote) = seed_remote();
    let stray_object_id = add_unreachable_remote_chunk_object(&remote, &source_repo_root);
    let tracked_remote = GetTrackingBackend::new(remote);
    let target_repo_root = temp.path().join("fetch-target");
    fs::create_dir_all(&target_repo_root).unwrap();
    RepositoryFacade::new()
        .init(InitOptions {
            repo_root: target_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();

    let fetched = fetch_remote(
        &tracked_remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token,
        },
    )
    .unwrap();

    assert!(fetched.downloaded_objects > 0);
    assert!(
        !target_repo_root
            .join(".e2v")
            .join("objects")
            .join(format!("{stray_object_id}.json"))
            .is_file(),
        "fetch should not materialize unreachable remote objects locally"
    );
    assert!(
        !tracked_remote
            .fetched_paths()
            .iter()
            .any(|path| path == &format!("objects/{stray_object_id}.json")),
        "fetch should not read unreachable remote objects from the backend"
    );
}

#[test]
fn fetch_does_not_read_unreachable_remote_loose_objects_when_repo_is_already_unlocked() {
    let (temp, _facade, source_repo_root, branch_token, remote) = seed_remote();
    let tracked_remote = GetTrackingBackend::new(remote);
    let clone_repo_root = temp.path().join("clone-target");

    clone_remote(
        &tracked_remote,
        CloneOptions {
            repo_root: clone_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_token: branch_token.clone(),
        },
    )
    .unwrap();

    let stray_object_id =
        add_unreachable_remote_chunk_object(&tracked_remote.inner, &source_repo_root);
    tracked_remote.reset_gets();

    let fetched = fetch_remote(
        &tracked_remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: clone_repo_root.clone(),
            branch_token,
        },
    )
    .unwrap();

    assert_eq!(fetched.downloaded_objects, 0);
    assert!(
        !tracked_remote
            .fetched_paths()
            .iter()
            .any(|path| path == &format!("objects/{stray_object_id}.json")),
        "fetch should not re-read unreachable remote objects while validating an unlocked repository"
    );
    assert!(
        !clone_repo_root
            .join(".e2v")
            .join("objects")
            .join(format!("{stray_object_id}.json"))
            .is_file(),
        "fetch should not import unreachable remote objects into an unlocked repository"
    );
}

#[test]
fn fetch_noop_for_unlocked_repo_lists_remote_loose_objects_at_most_once() {
    let (temp, _facade, _source_repo_root, branch_token, remote) = seed_remote();
    let tracked_remote = GetTrackingBackend::new(remote);
    let clone_repo_root = temp.path().join("clone-target");

    clone_remote(
        &tracked_remote,
        CloneOptions {
            repo_root: clone_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_token: branch_token.clone(),
        },
    )
    .unwrap();

    tracked_remote.reset_gets();

    let fetched = fetch_remote(
        &tracked_remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: clone_repo_root,
            branch_token,
        },
    )
    .unwrap();

    assert_eq!(fetched.downloaded_objects, 0);
    let object_list_calls = tracked_remote
        .list_paths()
        .into_iter()
        .filter(|path| path == "objects/")
        .count();
    assert!(
        object_list_calls <= 1,
        "expected no-op unlocked fetch to avoid relisting remote loose objects, saw {object_list_calls}"
    );
}

#[test]
fn fetch_restores_packed_objects_without_repeating_pack_range_reads_per_object() {
    let _guard = e2v_sync::testing::override_small_object_pack_threshold_for_test(1);
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
    for index in 0..24usize {
        fs::write(
            source_repo_root.join(format!("file-{index:02}.txt")),
            format!("payload-{index:02}"),
        )
        .unwrap();
    }
    facade
        .commit(CommitOptions {
            repo_root: source_repo_root.clone(),
            message: "packed-fetch-range-scaling".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    let pushed = push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: source_repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "packed-fetch-range-scaling-op".to_string(),
        },
    )
    .unwrap();
    assert!(pushed.uploaded_objects > 0);
    assert!(remote.list_physical("objects/").unwrap().is_empty());

    let tracked_remote = GetTrackingBackend::new(remote);
    let target_repo_root = temp.path().join("fetch-target");
    fs::create_dir_all(&target_repo_root).unwrap();
    RepositoryFacade::new()
        .init(InitOptions {
            repo_root: target_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();

    let fetched = fetch_remote(
        &tracked_remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
        },
    )
    .unwrap();

    assert!(fetched.downloaded_objects > 0);
    let range_read_paths = tracked_remote.range_read_paths();
    assert!(
        !range_read_paths.is_empty(),
        "expected packed fetch to use pack range reads"
    );
    let distinct_pack_paths = range_read_paths
        .iter()
        .filter(|path| path.starts_with("packs/data/"))
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    assert!(
        range_read_paths.len() <= distinct_pack_paths.len() + 2,
        "expected fetch to avoid repeated per-object pack reads, saw {:?}",
        range_read_paths
    );
}

#[test]
fn fetch_restores_missing_packed_objects_without_relisting_remote_pack_indexes() {
    let _guard = e2v_sync::testing::override_small_object_pack_threshold_for_test(1);
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
    for index in 0..24usize {
        fs::write(
            source_repo_root.join(format!("cached-{index:02}.txt")),
            format!("cached-payload-{index:02}"),
        )
        .unwrap();
    }
    facade
        .commit(CommitOptions {
            repo_root: source_repo_root.clone(),
            message: "packed-fetch-cache".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    let pushed = push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: source_repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "packed-fetch-cache-op".to_string(),
        },
    )
    .unwrap();
    assert!(pushed.uploaded_objects > 0);
    assert!(remote.list_physical("objects/").unwrap().is_empty());
    assert!(!remote.list_physical("packs/index/").unwrap().is_empty());

    let target_repo_root = temp.path().join("fetch-target");
    fs::create_dir_all(&target_repo_root).unwrap();
    RepositoryFacade::new()
        .init(InitOptions {
            repo_root: target_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();

    fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
        },
    )
    .unwrap();

    let deleted_object_path = fs::read_dir(target_repo_root.join(".e2v").join("objects"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .next()
        .unwrap();
    fs::remove_file(&deleted_object_path).unwrap();
    assert!(!deleted_object_path.exists());

    let guarded_remote = PackIndexListingForbiddenRemote::new(remote);
    let fetched = fetch_remote(
        &guarded_remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
        },
    )
    .unwrap();

    assert!(fetched.downloaded_objects > 0);
    assert!(deleted_object_path.is_file());
}

#[test]
fn fetch_allows_read_service_to_fallback_to_cached_pack_data_when_local_chunk_goes_missing() {
    let _guard = e2v_sync::testing::override_small_object_pack_threshold_for_test(1);
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
    for index in 0..24usize {
        fs::write(
            source_repo_root.join(format!("cached-{index:02}.txt")),
            format!("cached-payload-{index:02}"),
        )
        .unwrap();
    }
    facade
        .commit(CommitOptions {
            repo_root: source_repo_root.clone(),
            message: "packed-fetch-read-fallback".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    let pushed = push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: source_repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "packed-fetch-read-fallback-op".to_string(),
        },
    )
    .unwrap();
    assert!(pushed.uploaded_objects > 0);
    assert!(remote.list_physical("objects/").unwrap().is_empty());
    assert!(!remote.list_physical("packs/data/").unwrap().is_empty());

    let target_repo_root = temp.path().join("fetch-target");
    fs::create_dir_all(&target_repo_root).unwrap();
    RepositoryFacade::new()
        .init(InitOptions {
            repo_root: target_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();

    let fetched = fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
        },
    )
    .unwrap();
    assert!(fetched.downloaded_objects > 0);

    let cache_pack_data_root = target_repo_root
        .join(".e2v")
        .join("cache")
        .join("pack-data");
    RepositoryFacade::new()
        .unlock(&target_repo_root, "correct horse battery staple")
        .unwrap();
    let read_service = RepositoryFacade::new()
        .read_service(&target_repo_root)
        .unwrap();
    let snapshot = read_service
        .resolve_branch(&state.branch.token_hex)
        .unwrap();
    let file = read_service.open_file(&snapshot, "cached-00.txt").unwrap();
    let chunk_id = file.debug_chunk_ids()[0].clone();
    let chunk_path = target_repo_root
        .join(".e2v")
        .join("objects")
        .join(format!("{chunk_id}.json"));
    fs::remove_file(&chunk_path).unwrap();
    assert!(!chunk_path.exists());

    let bytes = read_service
        .read_range(&file, 0, "cached-payload-00".len())
        .unwrap();

    assert_eq!(String::from_utf8(bytes).unwrap(), "cached-payload-00");
    assert!(cache_pack_data_root.is_dir());
    assert!(
        fs::read_dir(cache_pack_data_root.join("packs").join("data"))
            .unwrap()
            .next()
            .is_some()
    );
}

#[test]
fn fetch_reuses_local_pack_data_cache_when_remote_pack_ranges_are_unavailable() {
    let _guard = e2v_sync::testing::override_small_object_pack_threshold_for_test(1);
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
    for index in 0..24usize {
        fs::write(
            source_repo_root.join(format!("cache-reuse-{index:02}.txt")),
            format!("cache-reuse-payload-{index:02}"),
        )
        .unwrap();
    }
    facade
        .commit(CommitOptions {
            repo_root: source_repo_root.clone(),
            message: "packed-fetch-cache-reuse".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    let pushed = push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: source_repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "packed-fetch-cache-reuse-op".to_string(),
        },
    )
    .unwrap();
    assert!(pushed.uploaded_objects > 0);
    assert!(remote.list_physical("objects/").unwrap().is_empty());
    assert!(!remote.list_physical("packs/data/").unwrap().is_empty());

    let target_repo_root = temp.path().join("fetch-target");
    fs::create_dir_all(&target_repo_root).unwrap();
    RepositoryFacade::new()
        .init(InitOptions {
            repo_root: target_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();

    fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
        },
    )
    .unwrap();

    let cache_pack_data_root = target_repo_root
        .join(".e2v")
        .join("cache")
        .join("pack-data")
        .join("packs")
        .join("data");
    assert!(cache_pack_data_root.is_dir());
    assert!(
        fs::read_dir(&cache_pack_data_root)
            .unwrap()
            .next()
            .is_some()
    );

    let deleted_object_path = fs::read_dir(target_repo_root.join(".e2v").join("objects"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .next()
        .unwrap();
    fs::remove_file(&deleted_object_path).unwrap();
    assert!(!deleted_object_path.exists());

    let guarded_remote = PackDataRangeReadForbiddenRemote::new(remote);
    let fetched = fetch_remote(
        &guarded_remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
        },
    )
    .unwrap();

    assert!(fetched.downloaded_objects > 0);
    assert!(deleted_object_path.is_file());
}

#[test]
fn fetch_rejects_invalid_pack_index_root_instead_of_falling_back_to_segment_listing() {
    let _guard = e2v_sync::testing::override_small_object_pack_threshold_for_test(1);
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
    fs::write(source_repo_root.join("hello.txt"), b"hello packed").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: source_repo_root.clone(),
            message: "invalid-pack-index-root".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: source_repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "invalid-pack-index-root-op".to_string(),
        },
    )
    .unwrap();

    let mut root = e2v_sync::testing::decode_pack_index_root_value_for_test(
        &source_repo_root.join(".e2v"),
        &remote.get_physical("pack-index/root.json").unwrap(),
    )
    .unwrap();
    root["schema_version"] = serde_json::Value::from(99u64);
    remote
        .put_physical(
            "pack-index/root.json",
            &e2v_sync::testing::encode_pack_index_root_value_for_test(
                &source_repo_root.join(".e2v"),
                &root,
            )
            .unwrap(),
        )
        .unwrap();

    let target_repo_root = temp.path().join("fetch-target");
    fs::create_dir_all(&target_repo_root).unwrap();
    RepositoryFacade::new()
        .init(InitOptions {
            repo_root: target_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();

    let error = fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root,
            branch_token: state.branch.token_hex.clone(),
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("pack index root")
            || error.to_string().contains("schema version"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn clone_and_fetch_restore_large_files_backed_by_file_shards() {
    let (_chunk_guard, _file_shard_guard) = e2v_core::testing::force_large_file_sharding_for_test();
    let _pack_guard = e2v_sync::testing::override_small_object_pack_threshold_for_test(1);
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

    let mut large_bytes = Vec::with_capacity(24 * 1024 * 1024);
    for block in 0..24usize {
        let fill = b'A' + (block as u8 % 26);
        large_bytes.extend(std::iter::repeat_n(fill, 1024 * 1024));
    }
    fs::write(source_repo_root.join("large.bin"), &large_bytes).unwrap();

    facade
        .commit(CommitOptions {
            repo_root: source_repo_root.clone(),
            message: "file-shard-sync".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: source_repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "file-shard-sync-op".to_string(),
        },
    )
    .unwrap();

    let clone_repo_root = temp.path().join("clone-target");
    clone_remote(
        &remote,
        CloneOptions {
            repo_root: clone_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_token: state.branch.token_hex.clone(),
        },
    )
    .unwrap();
    fs::create_dir_all(clone_repo_root.join("checkout")).unwrap();
    let clone_facade = RepositoryFacade::new();
    clone_facade
        .unlock(&clone_repo_root, "correct horse battery staple")
        .unwrap();
    clone_facade
        .checkout(e2v_core::CheckoutOptions {
            repo_root: clone_repo_root.clone(),
            snapshot_id: clone_facade.snapshots(&clone_repo_root).unwrap()[0]
                .snapshot_id
                .clone(),
            target_dir: clone_repo_root.join("checkout"),
        })
        .unwrap();
    assert_eq!(
        fs::read(clone_repo_root.join("checkout").join("large.bin")).unwrap(),
        large_bytes
    );

    let fetch_repo_root = temp.path().join("fetch-target");
    fs::create_dir_all(&fetch_repo_root).unwrap();
    RepositoryFacade::new()
        .init(InitOptions {
            repo_root: fetch_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
    fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: fetch_repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
        },
    )
    .unwrap();
    fs::create_dir_all(fetch_repo_root.join("checkout")).unwrap();
    let fetch_facade = RepositoryFacade::new();
    fetch_facade
        .unlock(&fetch_repo_root, "correct horse battery staple")
        .unwrap();
    fetch_facade
        .checkout(e2v_core::CheckoutOptions {
            repo_root: fetch_repo_root.clone(),
            snapshot_id: fetch_facade.snapshots(&fetch_repo_root).unwrap()[0]
                .snapshot_id
                .clone(),
            target_dir: fetch_repo_root.join("checkout"),
        })
        .unwrap();
    assert_eq!(
        fs::read(fetch_repo_root.join("checkout").join("large.bin")).unwrap(),
        large_bytes
    );
}

#[test]
fn fetch_reuses_pack_reads_when_restoring_objects_after_remote_keyring_pointer_changes() {
    let _guard = e2v_sync::testing::override_small_object_pack_threshold_for_test(1);
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
    for index in 0..24usize {
        fs::write(
            source_repo_root.join(format!("base-{index:02}.txt")),
            format!("base-payload-{index:02}"),
        )
        .unwrap();
    }
    facade
        .commit(CommitOptions {
            repo_root: source_repo_root.clone(),
            message: "packed-base".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: source_repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "packed-pointer-change-base".to_string(),
        },
    )
    .unwrap();

    let clone_repo_root = temp.path().join("clone-target");
    clone_remote(
        &remote,
        CloneOptions {
            repo_root: clone_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_token: state.branch.token_hex.clone(),
        },
    )
    .unwrap();

    facade
        .change_password(
            &source_repo_root,
            "correct horse battery staple",
            "new horse battery staple",
        )
        .unwrap();
    for index in 0..24usize {
        fs::write(
            source_repo_root.join(format!("next-{index:02}.txt")),
            format!("next-payload-{index:02}"),
        )
        .unwrap();
    }
    let committed = facade
        .commit(CommitOptions {
            repo_root: source_repo_root.clone(),
            message: "packed-pointer-change-next".to_string(),
        })
        .unwrap();
    let pushed = push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: source_repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "packed-pointer-change-next".to_string(),
        },
    )
    .unwrap();
    assert!(pushed.uploaded_objects > 0);

    let tracked_remote = GetTrackingBackend::new(remote);
    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&clone_repo_root.join(".e2v"));
    tracked_remote.reset_gets();

    let fetched = fetch_remote(
        &tracked_remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: clone_repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
        },
    )
    .unwrap();

    assert!(fetched.downloaded_objects > 0);
    assert!(
        clone_repo_root
            .join(".e2v")
            .join("objects")
            .join(format!("{}.json", committed.snapshot_id))
            .is_file()
    );
    let pack_range_read_paths = tracked_remote
        .range_read_paths()
        .into_iter()
        .filter(|path| path.starts_with("packs/data/"))
        .collect::<Vec<_>>();
    assert!(
        !pack_range_read_paths.is_empty(),
        "expected fetch to read packed object data after the remote keyring pointer changed"
    );
    let distinct_pack_paths = pack_range_read_paths
        .iter()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    assert!(
        pack_range_read_paths.len() <= distinct_pack_paths.len() + 1,
        "expected fetch to reuse pack reads after the remote keyring pointer changed, saw {:?}",
        pack_range_read_paths
    );

    RepositoryFacade::new()
        .unlock(&clone_repo_root, "new horse battery staple")
        .unwrap();
}

#[test]
fn fetch_restores_objects_after_remote_enables_oblivious_layout() {
    let temp = tempdir().unwrap();
    let source_root = temp.path().join("source");
    let clone_root = temp.path().join("clone");
    fs::create_dir_all(&source_root).unwrap();
    fs::create_dir_all(&clone_root).unwrap();

    let facade = RepositoryFacade::new();
    let state = facade
        .init(InitOptions {
            repo_root: source_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
    fs::write(source_root.join("hello.txt"), b"hello oram").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: source_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: source_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "fetch-oram-bootstrap".to_string(),
        },
    )
    .unwrap();
    enable_oblivious_layout(
        &remote,
        EnableObliviousLayoutOptions {
            repo_root: source_root.clone(),
            policy_profile: "balanced".to_string(),
        },
    )
    .unwrap();

    clone_remote(
        &remote,
        CloneOptions {
            repo_root: clone_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            password: "correct horse battery staple".to_string(),
        },
    )
    .unwrap();

    let object_count_before = fs::read_dir(clone_root.join(".e2v").join("objects"))
        .unwrap()
        .count();
    let first_object = fs::read_dir(clone_root.join(".e2v").join("objects"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::remove_file(&first_object).unwrap();

    let fetched = fetch_remote(
        &remote,
        FetchOptions {
            repo_root: clone_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            password: Some("correct horse battery staple".to_string()),
        },
    )
    .unwrap();

    let read = facade.read_service(&clone_root).unwrap();
    let snapshot = read.resolve_branch(&state.branch.token_hex).unwrap();
    let file = read.open_file(&snapshot, "hello.txt").unwrap();
    let bytes = read.read_range(&file, 0, 32).unwrap();

    assert!(fetched.downloaded_objects >= 1);
    assert_eq!(String::from_utf8(bytes).unwrap(), "hello oram");
    assert_eq!(
        fs::read_dir(clone_root.join(".e2v").join("objects"))
            .unwrap()
            .count(),
        object_count_before
    );
}

#[test]
fn fetch_after_oram_enablement_uses_oblivious_root_when_pack_index_root_is_missing() {
    let temp = tempdir().unwrap();
    let source_root = temp.path().join("source");
    let clone_root = temp.path().join("clone");
    fs::create_dir_all(&source_root).unwrap();
    fs::create_dir_all(&clone_root).unwrap();

    let facade = RepositoryFacade::new();
    let state = facade
        .init(InitOptions {
            repo_root: source_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
    fs::write(source_root.join("hello.txt"), b"hello oram").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: source_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: source_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "fetch-oram-routing-bootstrap".to_string(),
        },
    )
    .unwrap();
    enable_oblivious_layout(
        &remote,
        EnableObliviousLayoutOptions {
            repo_root: source_root.clone(),
            policy_profile: "balanced".to_string(),
        },
    )
    .unwrap();

    clone_remote(
        &remote,
        CloneOptions {
            repo_root: clone_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            password: "correct horse battery staple".to_string(),
        },
    )
    .unwrap();

    for path in remote.list_physical("objects/").unwrap() {
        remote.delete_physical(&path).unwrap();
    }
    remote.delete_physical("pack-index/root.json").unwrap();

    let first_object = fs::read_dir(clone_root.join(".e2v").join("objects"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::remove_file(&first_object).unwrap();

    let fetched = fetch_remote(
        &remote,
        FetchOptions {
            repo_root: clone_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            password: Some("correct horse battery staple".to_string()),
        },
    )
    .unwrap();

    let read = facade.read_service(&clone_root).unwrap();
    let snapshot = read.resolve_branch(&state.branch.token_hex).unwrap();
    let file = read.open_file(&snapshot, "hello.txt").unwrap();
    let bytes = read.read_range(&file, 0, 32).unwrap();

    assert!(fetched.downloaded_objects >= 1);
    assert_eq!(String::from_utf8(bytes).unwrap(), "hello oram");
}

#[test]
fn fetch_does_not_publish_new_keyring_pointer_before_new_generation_file_is_written() {
    let (temp, facade, source_repo_root, branch_token, remote) = seed_remote();
    let target_repo_root = temp.path().join("clone-target");

    clone_remote(
        &remote,
        CloneOptions {
            repo_root: target_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_token: branch_token.clone(),
        },
    )
    .unwrap();

    facade
        .change_password(
            &source_repo_root,
            "correct horse battery staple",
            "new horse battery staple",
        )
        .unwrap();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: source_repo_root,
            branch_token: branch_token.clone(),
            operation_id: "fetch-pointer-publish-order".to_string(),
        },
    )
    .unwrap();

    let original_pointer = fs::read(
        target_repo_root
            .join(".e2v")
            .join("keyring")
            .join("keyring.current"),
    )
    .unwrap();
    let original_generation_one = fs::read(
        target_repo_root
            .join(".e2v")
            .join("keyring")
            .join("keyring.1"),
    )
    .unwrap();
    fs::create_dir_all(
        target_repo_root
            .join(".e2v")
            .join("keyring")
            .join("keyring.2.tmp"),
    )
    .unwrap();

    let error = fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token: branch_token.clone(),
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("keyring.2")
            || error.to_string().contains("rename")
            || error.to_string().contains("write"),
        "unexpected error: {error:#}"
    );
    assert_eq!(
        fs::read(
            target_repo_root
                .join(".e2v")
                .join("keyring")
                .join("keyring.current")
        )
        .unwrap(),
        original_pointer
    );
    assert_eq!(
        fs::read(
            target_repo_root
                .join(".e2v")
                .join("keyring")
                .join("keyring.1")
        )
        .unwrap(),
        original_generation_one
    );
}

#[test]
fn fetch_replaces_corrupted_local_object_file_instead_of_skipping_existing_path() {
    let (temp, _facade, source_repo_root, branch_token, remote) = seed_remote();
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

    fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token: branch_token.clone(),
        },
    )
    .unwrap();

    let head_snapshot_id = RepositoryFacade::new()
        .snapshots(&source_repo_root)
        .unwrap()
        .first()
        .unwrap()
        .snapshot_id
        .clone();
    let source_object_path = e2v_core::sync_support::list_local_object_files(&source_repo_root)
        .unwrap()
        .into_iter()
        .find(|path| {
            let object_id = path.file_stem().unwrap().to_string_lossy().to_string();
            object_id != head_snapshot_id
        })
        .unwrap();
    let object_file_name = source_object_path.file_name().unwrap().to_owned();
    let source_object_bytes = fs::read(&source_object_path).unwrap();
    let target_object_path = target_repo_root
        .join(".e2v")
        .join("objects")
        .join(&object_file_name);
    fs::write(&target_object_path, b"truncated-object").unwrap();

    fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token,
        },
    )
    .unwrap();

    assert_eq!(fs::read(&target_object_path).unwrap(), source_object_bytes);
    RepositoryFacade::new()
        .unlock(&target_repo_root, "correct horse battery staple")
        .unwrap();
    RepositoryFacade::new()
        .verify_ref(&target_repo_root)
        .unwrap();
}

#[test]
fn fetch_requires_unlock_before_repairing_tampered_local_object_when_only_locked_checks_are_available()
 {
    let (temp, _facade, source_repo_root, branch_token, remote) = seed_remote();
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

    fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token: branch_token.clone(),
        },
    )
    .unwrap();

    let source_object_path = e2v_core::sync_support::list_local_object_files(&source_repo_root)
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let object_file_name = source_object_path.file_name().unwrap().to_owned();
    let source_object_bytes = fs::read(&source_object_path).unwrap();
    let target_object_path = target_repo_root
        .join(".e2v")
        .join("objects")
        .join(&object_file_name);
    let mut tampered_bytes = fs::read(&target_object_path).unwrap();
    let last_index = tampered_bytes.len() - 1;
    tampered_bytes[last_index] ^= 0x01;
    fs::write(&target_object_path, &tampered_bytes).unwrap();

    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&target_repo_root.join(".e2v"));

    let error = fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token: branch_token.clone(),
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("unlock")
            || error.to_string().contains("locked")
            || error.to_string().contains("repair"),
        "unexpected error: {error:#}"
    );
    assert_eq!(fs::read(&target_object_path).unwrap(), tampered_bytes);
    RepositoryFacade::new()
        .unlock(&target_repo_root, "correct horse battery staple")
        .unwrap();
    fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token: branch_token.clone(),
        },
    )
    .unwrap();
    assert_eq!(fs::read(&target_object_path).unwrap(), source_object_bytes);
    RepositoryFacade::new()
        .unlock(&target_repo_root, "correct horse battery staple")
        .unwrap();
    RepositoryFacade::new()
        .verify_ref(&target_repo_root)
        .unwrap();
}

#[test]
fn fetch_does_not_redownload_healthy_objects_just_because_local_repository_is_locked() {
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

    let first = fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token: branch_token.clone(),
        },
    )
    .unwrap();
    assert!(first.downloaded_objects > 0);

    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&target_repo_root.join(".e2v"));

    let second = fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token,
        },
    )
    .unwrap();

    assert_eq!(second.downloaded_objects, 0);
}

#[test]
fn local_object_envelope_static_health_check_accepts_a_healthy_downloaded_object() {
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

    fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token,
        },
    )
    .unwrap();

    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&target_repo_root.join(".e2v"));

    let object_id = e2v_core::sync_support::list_local_object_files(&target_repo_root)
        .unwrap()
        .into_iter()
        .next()
        .unwrap()
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .to_string();

    assert!(
        e2v_core::sync_support::local_object_envelope_looks_valid(&target_repo_root, &object_id)
            .unwrap(),
        "healthy local object should pass static envelope validation"
    );
}

#[test]
fn fetch_does_not_overwrite_locked_local_object_with_corrupted_remote_bytes() {
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

    fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token: branch_token.clone(),
        },
    )
    .unwrap();

    let target_object_path = e2v_core::sync_support::list_local_object_files(&target_repo_root)
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let original_local_bytes = fs::read(&target_object_path).unwrap();
    let object_id = target_object_path
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .to_string();

    let mut corrupted_remote_bytes = original_local_bytes.clone();
    let flip_index = corrupted_remote_bytes.len() / 2;
    corrupted_remote_bytes[flip_index] ^= 0x01;
    remote
        .put_physical(
            &format!("objects/{object_id}.json"),
            &corrupted_remote_bytes,
        )
        .unwrap();

    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&target_repo_root.join(".e2v"));

    let error = fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token,
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("object")
            || error.to_string().contains("authentication")
            || error.to_string().contains("corrupt"),
        "unexpected error: {error:#}"
    );
    assert_eq!(fs::read(&target_object_path).unwrap(), original_local_bytes);
}

#[test]
fn fetch_still_skips_locked_local_object_when_remote_bytes_match_exactly() {
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

    let first = fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token: branch_token.clone(),
        },
    )
    .unwrap();
    assert!(first.downloaded_objects > 0);

    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&target_repo_root.join(".e2v"));

    let second = fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token,
        },
    )
    .unwrap();

    assert_eq!(second.downloaded_objects, 0);
}

#[test]
fn fetch_updates_keyring_pointer_even_when_remote_keyring_listing_omits_pointer_file() {
    let (temp, facade, source_repo_root, branch_token, remote) = seed_remote();
    let target_repo_root = temp.path().join("clone-target");

    clone_remote(
        &remote,
        CloneOptions {
            repo_root: target_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_token: branch_token.clone(),
        },
    )
    .unwrap();

    facade
        .change_password(
            &source_repo_root,
            "correct horse battery staple",
            "new horse battery staple",
        )
        .unwrap();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: source_repo_root,
            branch_token: branch_token.clone(),
            operation_id: "rotate-password-with-hidden-pointer-file".to_string(),
        },
    )
    .unwrap();

    let list_hidden_remote = KeyringPointerHiddenFromListRemote::new(remote.clone());
    fetch_remote(
        &list_hidden_remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token,
        },
    )
    .unwrap();

    assert_eq!(
        fs::read(
            target_repo_root
                .join(".e2v")
                .join("keyring")
                .join("keyring.current")
        )
        .unwrap(),
        remote
            .get_physical("control/keyring/keyring.current")
            .unwrap()
    );

    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&target_repo_root.join(".e2v"));
    let clone_facade = RepositoryFacade::new();
    let old_password_error = clone_facade
        .unlock(&target_repo_root, "correct horse battery staple")
        .unwrap_err();
    assert!(
        old_password_error.to_string().contains("wrong password")
            || old_password_error.to_string().contains("unlock")
            || old_password_error.to_string().contains("keyring"),
        "unexpected old password error: {old_password_error:#}"
    );
    clone_facade
        .unlock(&target_repo_root, "new horse battery staple")
        .unwrap();
}

#[test]
fn fetch_uses_keyring_pointer_ref_when_physical_pointer_file_is_missing() {
    let (temp, facade, source_repo_root, branch_token, remote) = seed_remote();
    let target_repo_root = temp.path().join("clone-target");

    clone_remote(
        &remote,
        CloneOptions {
            repo_root: target_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_token: branch_token.clone(),
        },
    )
    .unwrap();

    facade
        .change_password(
            &source_repo_root,
            "correct horse battery staple",
            "new horse battery staple",
        )
        .unwrap();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: source_repo_root.clone(),
            branch_token: branch_token.clone(),
            operation_id: "rotate-password-with-pointer-ref".to_string(),
        },
    )
    .unwrap();

    let pointer_bytes = remote
        .get_physical("control/keyring/keyring.current")
        .unwrap();
    let expected_version = remote
        .read_ref(&keyring_pointer_ref_token(&source_repo_root))
        .unwrap()
        .map(|stored| stored.version);
    remote
        .compare_and_swap_ref(
            &keyring_pointer_ref_token(&source_repo_root),
            expected_version,
            e2v_store::EncryptedRef::new(pointer_bytes.clone()),
        )
        .unwrap();
    remote
        .delete_physical("control/keyring/keyring.current")
        .unwrap();

    fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token,
        },
    )
    .unwrap();

    assert_eq!(
        fs::read(
            target_repo_root
                .join(".e2v")
                .join("keyring")
                .join("keyring.current")
        )
        .unwrap(),
        pointer_bytes
    );
}

#[test]
fn fetch_rejects_remote_without_keyring_pointer_ref_even_if_physical_pointer_file_exists() {
    let (temp, facade, source_repo_root, branch_token, remote) = seed_remote();
    let target_repo_root = temp.path().join("clone-target");

    clone_remote(
        &remote,
        CloneOptions {
            repo_root: target_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_token: branch_token.clone(),
        },
    )
    .unwrap();

    facade
        .change_password(
            &source_repo_root,
            "correct horse battery staple",
            "new horse battery staple",
        )
        .unwrap();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: source_repo_root.clone(),
            branch_token: branch_token.clone(),
            operation_id: "rotate-password-without-pointer-ref".to_string(),
        },
    )
    .unwrap();
    let hidden_ref_remote = KeyringPointerRefHiddenRemote::new(remote.clone());

    let error = fetch_remote(
        &hidden_ref_remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token,
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("keyring pointer ref")
            || error.to_string().contains("ambiguous")
            || error.to_string().contains("missing"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn fetch_updates_current_keyring_generation_even_when_remote_keyring_listing_omits_it() {
    let (temp, facade, source_repo_root, branch_token, remote) = seed_remote();
    let target_repo_root = temp.path().join("clone-target");

    clone_remote(
        &remote,
        CloneOptions {
            repo_root: target_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_token: branch_token.clone(),
        },
    )
    .unwrap();

    facade
        .change_password(
            &source_repo_root,
            "correct horse battery staple",
            "new horse battery staple",
        )
        .unwrap();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: source_repo_root,
            branch_token: branch_token.clone(),
            operation_id: "rotate-password-with-hidden-current-generation".to_string(),
        },
    )
    .unwrap();

    let remote_pointer: serde_json::Value = serde_json::from_slice(
        &remote
            .get_physical("control/keyring/keyring.current")
            .unwrap(),
    )
    .unwrap();
    let current_file_name = remote_pointer["current"].as_str().unwrap().to_string();
    let filtered_remote = KeyringPointerHiddenFromListRemote::with_hidden_paths(
        remote.clone(),
        vec![format!("control/keyring/{current_file_name}")],
    );

    fetch_remote(
        &filtered_remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token,
        },
    )
    .unwrap();

    assert_eq!(
        fs::read(
            target_repo_root
                .join(".e2v")
                .join("keyring")
                .join(&current_file_name)
        )
        .unwrap(),
        remote
            .get_physical(&format!("control/keyring/{current_file_name}"))
            .unwrap()
    );

    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&target_repo_root.join(".e2v"));
    let clone_facade = RepositoryFacade::new();
    let old_password_error = clone_facade
        .unlock(&target_repo_root, "correct horse battery staple")
        .unwrap_err();
    assert!(
        old_password_error.to_string().contains("wrong password")
            || old_password_error.to_string().contains("unlock")
            || old_password_error.to_string().contains("keyring"),
        "unexpected old password error: {old_password_error:#}"
    );
    clone_facade
        .unlock(&target_repo_root, "new horse battery staple")
        .unwrap();
}

#[test]
fn fetch_rejects_remote_keyring_pointer_path_traversal_even_when_listing_omits_pointed_file() {
    let (temp, _facade, source_repo_root, branch_token, remote) = seed_remote();
    let pointed_path = "control/keyring/../../evil.json";
    let malicious_pointer = serde_json::to_vec(&serde_json::json!({
        "generation": 7u64,
        "current": "../../evil.json"
    }))
    .unwrap();
    let expected_version = remote
        .read_ref(&keyring_pointer_ref_token(&source_repo_root))
        .unwrap()
        .map(|stored| stored.version);
    remote
        .compare_and_swap_ref(
            &keyring_pointer_ref_token(&source_repo_root),
            expected_version,
            e2v_store::EncryptedRef::new(malicious_pointer.clone()),
        )
        .unwrap();
    remote
        .put_physical("control/keyring/keyring.current", &malicious_pointer)
        .unwrap();
    remote
        .put_physical(
            pointed_path,
            &remote.get_physical("control/keyring/keyring.1").unwrap(),
        )
        .unwrap();

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
    let original_pointer = fs::read(
        target_repo_root
            .join(".e2v")
            .join("keyring")
            .join("keyring.current"),
    )
    .unwrap();

    let filtered_remote = KeyringPointerHiddenFromListRemote::with_hidden_paths(
        remote.clone(),
        vec![pointed_path.to_string()],
    );
    let error = fetch_remote(
        &filtered_remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token,
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("invalid remote keyring path")
            || error.to_string().contains("path escapes")
            || error.to_string().contains("path traversal"),
        "unexpected error: {error:#}"
    );
    assert_eq!(
        fs::read(
            target_repo_root
                .join(".e2v")
                .join("keyring")
                .join("keyring.current")
        )
        .unwrap(),
        original_pointer
    );
}

#[test]
fn fetch_preserves_unlocked_access_when_remote_keyring_pointer_is_unchanged() {
    let (temp, _facade, _source_repo_root, branch_token, remote) = seed_remote();
    let clone_repo_root = temp.path().join("clone-target");

    clone_remote(
        &remote,
        CloneOptions {
            repo_root: clone_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_token: branch_token.clone(),
        },
    )
    .unwrap();

    let before = RepositoryFacade::new().snapshots(&clone_repo_root).unwrap();

    fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: clone_repo_root.clone(),
            branch_token,
        },
    )
    .unwrap();

    let after = RepositoryFacade::new().snapshots(&clone_repo_root).unwrap();
    assert_eq!(after, before);
}

#[test]
fn fetch_clears_stale_keyring_cache_when_replacing_an_empty_local_repository() {
    let (temp, facade, source_repo_root, branch_token, remote) = seed_remote();
    let expected_snapshot_id = facade
        .snapshots(&source_repo_root)
        .unwrap()
        .first()
        .unwrap()
        .snapshot_id
        .clone();
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

    fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token: branch_token.clone(),
        },
    )
    .unwrap();

    let stale_cache_error = RepositoryFacade::new()
        .snapshots(&target_repo_root)
        .unwrap_err();
    assert!(
        stale_cache_error.to_string().contains("locked")
            || stale_cache_error.to_string().contains("unlock")
            || stale_cache_error.to_string().contains("keyring"),
        "unexpected error: {stale_cache_error:#}"
    );

    RepositoryFacade::new()
        .unlock(&target_repo_root, "correct horse battery staple")
        .unwrap();
    let snapshots = RepositoryFacade::new()
        .snapshots(&target_repo_root)
        .unwrap();
    assert_eq!(
        snapshots
            .first()
            .map(|snapshot| snapshot.snapshot_id.as_str()),
        Some(expected_snapshot_id.as_str())
    );
}

#[test]
fn fetch_clears_stale_keyring_cache_when_replacing_empty_repository_with_missing_pointer() {
    let (temp, facade, source_repo_root, branch_token, remote) = seed_remote();
    let expected_snapshot_id = facade
        .snapshots(&source_repo_root)
        .unwrap()
        .first()
        .unwrap()
        .snapshot_id
        .clone();
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

    fs::remove_file(
        target_repo_root
            .join(".e2v")
            .join("keyring")
            .join("keyring.current"),
    )
    .unwrap();

    fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token: branch_token.clone(),
        },
    )
    .unwrap();

    let stale_cache_error = RepositoryFacade::new()
        .snapshots(&target_repo_root)
        .unwrap_err();
    assert!(
        stale_cache_error.to_string().contains("locked")
            || stale_cache_error.to_string().contains("unlock")
            || stale_cache_error.to_string().contains("keyring"),
        "unexpected error: {stale_cache_error:#}"
    );

    RepositoryFacade::new()
        .unlock(&target_repo_root, "correct horse battery staple")
        .unwrap();
    let snapshots = RepositoryFacade::new()
        .snapshots(&target_repo_root)
        .unwrap();
    assert_eq!(
        snapshots
            .first()
            .map(|snapshot| snapshot.snapshot_id.as_str()),
        Some(expected_snapshot_id.as_str())
    );
}

#[test]
fn fetch_rejects_missing_remote_branch_ref_without_mutating_local_state() {
    let (temp, _facade, _source_repo_root, _branch_token, remote) = seed_remote();
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

    let original_ref_bytes = fs::read(
        target_repo_root
            .join(".e2v")
            .join("refs")
            .join("default.json"),
    )
    .unwrap();
    let original_layout_bytes =
        fs::read(target_repo_root.join(".e2v").join("layout_root.json")).unwrap();

    let error = fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token: "missing-branch-token".to_string(),
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("remote branch ref not found")
            || error.to_string().contains("branch ref"),
        "unexpected error: {error:#}"
    );
    assert_eq!(
        fs::read(
            target_repo_root
                .join(".e2v")
                .join("refs")
                .join("default.json")
        )
        .unwrap(),
        original_ref_bytes
    );
    assert_eq!(
        fs::read(target_repo_root.join(".e2v").join("layout_root.json")).unwrap(),
        original_layout_bytes
    );
}

#[test]
fn fetch_succeeds_without_remote_config_file() {
    let (temp, _facade, _source_repo_root, branch_token, remote) = seed_remote();
    if remote.exists_physical("control/config.json") {
        remote.delete_physical("control/config.json").unwrap();
    }

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

    let original_ref_bytes = fs::read(
        target_repo_root
            .join(".e2v")
            .join("refs")
            .join("default.json"),
    )
    .unwrap();
    let original_layout_bytes =
        fs::read(target_repo_root.join(".e2v").join("layout_root.json")).unwrap();

    let fetched = fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token,
        },
    )
    .unwrap();

    assert!(fetched.downloaded_objects > 0);
    assert_ne!(
        fs::read(
            target_repo_root
                .join(".e2v")
                .join("refs")
                .join("default.json")
        )
        .unwrap(),
        original_ref_bytes
    );
    assert_eq!(
        fs::read(target_repo_root.join(".e2v").join("layout_root.json")).unwrap(),
        original_layout_bytes,
        "fetch without remote config should still preserve the valid layout root"
    );
}

#[test]
fn fetch_rejects_remote_object_paths_that_escape_the_objects_directory() {
    let (temp, _facade, _source_repo_root, branch_token, remote) = seed_remote();
    remote
        .put_physical("objects/../../keep.txt", b"malicious overwrite")
        .unwrap();

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
    fs::write(target_repo_root.join("keep.txt"), b"safe").unwrap();

    let error = fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token,
        },
    )
    .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("remote ref points to unreadable head snapshot graph")
            || error.to_string().contains("invalid remote object path")
            || error.to_string().contains("path escapes")
            || error.to_string().contains("path traversal"),
        "unexpected error: {error:#}"
    );
    assert_eq!(
        fs::read(target_repo_root.join("keep.txt")).unwrap(),
        b"safe"
    );
    assert_eq!(
        target_repo_root
            .join(".e2v")
            .join("objects")
            .read_dir()
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn fetch_does_not_allow_unlocked_head_validation_to_write_outside_validation_root() {
    let (temp, _facade, _source_repo_root, branch_token, remote) = seed_remote();
    let target_repo_root = temp.path().join("fetch-target");

    clone_remote(
        &remote,
        CloneOptions {
            repo_root: target_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_token: branch_token.clone(),
        },
    )
    .unwrap();

    fs::write(target_repo_root.join("keep.txt"), b"safe").unwrap();
    remote
        .put_physical("objects/../../../keep.txt", b"malicious overwrite")
        .unwrap();

    let error = fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token,
        },
    )
    .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("remote ref points to unreadable head snapshot graph")
            || error.to_string().contains("invalid remote object path")
            || error.to_string().contains("path escapes")
            || error.to_string().contains("path traversal"),
        "unexpected error: {error:#}"
    );
    assert_eq!(
        fs::read(target_repo_root.join("keep.txt")).unwrap(),
        b"safe"
    );
}

#[test]
fn fetch_leaves_no_partial_objects_when_a_later_remote_object_path_is_invalid() {
    let (temp, _facade, _source_repo_root, branch_token, remote) = seed_remote();
    remote
        .put_physical(
            "objects/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz.json",
            b"benign object",
        )
        .unwrap();
    remote
        .put_physical("objects/../../keep.txt", b"malicious overwrite")
        .unwrap();

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
    fs::write(target_repo_root.join("keep.txt"), b"safe").unwrap();

    let error = fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token,
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("invalid remote object path")
            || error.to_string().contains("path escapes")
            || error.to_string().contains("path traversal"),
        "unexpected error: {error:#}"
    );
    assert_eq!(
        fs::read(target_repo_root.join("keep.txt")).unwrap(),
        b"safe"
    );
    assert_eq!(
        target_repo_root
            .join(".e2v")
            .join("objects")
            .read_dir()
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn fetch_leaves_no_partial_objects_when_invalid_remote_object_path_sorts_after_valid_objects() {
    let (temp, _facade, _source_repo_root, branch_token, remote) = seed_remote();
    remote
        .put_physical(
            "objects/ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff.json",
            b"benign object",
        )
        .unwrap();
    remote
        .put_physical("objects/z/../../keep.txt", b"malicious overwrite")
        .unwrap();

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
    fs::write(target_repo_root.join("keep.txt"), b"safe").unwrap();

    let error = fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token,
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("invalid remote object path")
            || error.to_string().contains("path escapes")
            || error.to_string().contains("path traversal"),
        "unexpected error: {error:#}"
    );
    assert_eq!(
        fs::read(target_repo_root.join("keep.txt")).unwrap(),
        b"safe"
    );
    assert_eq!(
        target_repo_root
            .join(".e2v")
            .join("objects")
            .read_dir()
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn fetch_rejects_pack_indexes_with_data_paths_outside_pack_data_prefix() {
    let _guard = e2v_sync::testing::override_small_object_pack_threshold_for_test(1);
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
    fs::write(source_repo_root.join("hello.txt"), b"hello packed").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: source_repo_root.clone(),
            message: "packed-sync".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: source_repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "packed-path-escape".to_string(),
        },
    )
    .unwrap();

    let index_path = remote
        .list_physical("packs/index/")
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let mut index = e2v_sync::testing::decode_pack_index_segment_value_for_test(
        &source_repo_root.join(".e2v"),
        &index_path,
        &remote.get_physical(&index_path).unwrap(),
    )
    .unwrap();
    index["data_path"] = serde_json::Value::String("../keep.txt".to_string());
    remote
        .put_physical(
            &index_path,
            &e2v_sync::testing::encode_pack_index_segment_value_for_test(
                &source_repo_root.join(".e2v"),
                &index_path,
                &index,
            )
            .unwrap(),
        )
        .unwrap();

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
    fs::write(target_repo_root.join("keep.txt"), b"safe").unwrap();

    let error = fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("pack data path")
            || error.to_string().contains("packs/data/")
            || error.to_string().contains("path traversal"),
        "unexpected error: {error:#}"
    );
    assert_eq!(
        fs::read(target_repo_root.join("keep.txt")).unwrap(),
        b"safe"
    );
}

#[test]
fn fetch_rejects_pack_indexes_with_object_ids_that_escape_the_objects_directory() {
    let _guard = e2v_sync::testing::override_small_object_pack_threshold_for_test(1);
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
    fs::write(source_repo_root.join("hello.txt"), b"hello packed").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: source_repo_root.clone(),
            message: "packed-sync".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: source_repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "packed-object-id-escape".to_string(),
        },
    )
    .unwrap();

    let index_path = remote
        .list_physical("packs/index/")
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let mut index = e2v_sync::testing::decode_pack_index_segment_value_for_test(
        &source_repo_root.join(".e2v"),
        &index_path,
        &remote.get_physical(&index_path).unwrap(),
    )
    .unwrap();
    index["entries"][0]["object_id"] = serde_json::Value::String("../keep".to_string());
    remote
        .put_physical(
            &index_path,
            &e2v_sync::testing::encode_pack_index_segment_value_for_test(
                &source_repo_root.join(".e2v"),
                &index_path,
                &index,
            )
            .unwrap(),
        )
        .unwrap();

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
    fs::write(target_repo_root.join("keep.json"), b"safe").unwrap();

    let error = fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("invalid packed object id")
            || error.to_string().contains("path traversal")
            || error.to_string().contains("object id"),
        "unexpected error: {error:#}"
    );
    assert_eq!(
        fs::read(target_repo_root.join("keep.json")).unwrap(),
        b"safe"
    );
}

#[test]
fn fetch_rejects_pack_indexes_with_out_of_bounds_entry_ranges() {
    let _guard = e2v_sync::testing::override_small_object_pack_threshold_for_test(1);
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
    fs::write(source_repo_root.join("hello.txt"), b"hello packed").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: source_repo_root.clone(),
            message: "packed-sync".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: source_repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "packed-range-oob".to_string(),
        },
    )
    .unwrap();

    let index_path = remote
        .list_physical("packs/index/")
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let data_path = remote
        .list_physical("packs/data/")
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let data_len = remote.get_physical(&data_path).unwrap().len() as u64;
    let mut index = e2v_sync::testing::decode_pack_index_segment_value_for_test(
        &source_repo_root.join(".e2v"),
        &index_path,
        &remote.get_physical(&index_path).unwrap(),
    )
    .unwrap();
    index["entries"][0]["offset"] = serde_json::Value::from(data_len + 1);
    index["entries"][0]["length"] = serde_json::Value::from(10u64);
    remote
        .put_physical(
            &index_path,
            &e2v_sync::testing::encode_pack_index_segment_value_for_test(
                &source_repo_root.join(".e2v"),
                &index_path,
                &index,
            )
            .unwrap(),
        )
        .unwrap();

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

    let error = fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("pack entry range")
            || error.to_string().contains("out of bounds"),
        "unexpected error: {error:#}"
    );
    assert_eq!(
        target_repo_root
            .join(".e2v")
            .join("objects")
            .read_dir()
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn fetch_rejects_pack_indexes_with_entry_lengths_that_span_multiple_objects() {
    let _guard = e2v_sync::testing::override_small_object_pack_threshold_for_test(1);
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
    fs::write(source_repo_root.join("hello.txt"), b"hello packed").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: source_repo_root.clone(),
            message: "packed-sync".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: source_repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "packed-range-overlap".to_string(),
        },
    )
    .unwrap();

    let index_path = remote
        .list_physical("packs/index/")
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let data_path = remote
        .list_physical("packs/data/")
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let data_len = remote.get_physical(&data_path).unwrap().len() as u64;
    let mut index = e2v_sync::testing::decode_pack_index_segment_value_for_test(
        &source_repo_root.join(".e2v"),
        &index_path,
        &remote.get_physical(&index_path).unwrap(),
    )
    .unwrap();
    index["entries"][0]["offset"] = serde_json::Value::from(0u64);
    index["entries"][0]["length"] = serde_json::Value::from(data_len);
    remote
        .put_physical(
            &index_path,
            &e2v_sync::testing::encode_pack_index_segment_value_for_test(
                &source_repo_root.join(".e2v"),
                &index_path,
                &index,
            )
            .unwrap(),
        )
        .unwrap();

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

    let error = fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("pack entry range") || error.to_string().contains("overlap"),
        "unexpected error: {error:#}"
    );
    assert_eq!(
        target_repo_root
            .join(".e2v")
            .join("objects")
            .read_dir()
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn fetch_rejects_pack_indexes_with_duplicate_object_ids() {
    let _guard = e2v_sync::testing::override_small_object_pack_threshold_for_test(1);
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
    fs::write(source_repo_root.join("hello.txt"), b"hello packed").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: source_repo_root.clone(),
            message: "packed-sync".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: source_repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "packed-duplicate-object".to_string(),
        },
    )
    .unwrap();

    let index_path = remote
        .list_physical("packs/index/")
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let mut index = e2v_sync::testing::decode_pack_index_segment_value_for_test(
        &source_repo_root.join(".e2v"),
        &index_path,
        &remote.get_physical(&index_path).unwrap(),
    )
    .unwrap();
    let duplicate_id = index["entries"][0]["object_id"]
        .as_str()
        .unwrap()
        .to_string();
    index["entries"][1]["object_id"] = serde_json::Value::String(duplicate_id);
    remote
        .put_physical(
            &index_path,
            &e2v_sync::testing::encode_pack_index_segment_value_for_test(
                &source_repo_root.join(".e2v"),
                &index_path,
                &index,
            )
            .unwrap(),
        )
        .unwrap();

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

    let error = fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("duplicate packed object id")
            || error.to_string().contains("duplicate object id")
            || error.to_string().contains("pack"),
        "unexpected error: {error:#}"
    );
    assert_eq!(
        target_repo_root
            .join(".e2v")
            .join("objects")
            .read_dir()
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn fetch_rejects_pack_indexes_with_unknown_schema_version() {
    let _guard = e2v_sync::testing::override_small_object_pack_threshold_for_test(1);
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
    fs::write(source_repo_root.join("hello.txt"), b"hello packed").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: source_repo_root.clone(),
            message: "packed-schema-version".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: source_repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "packed-schema-version".to_string(),
        },
    )
    .unwrap();

    let index_path = remote
        .list_physical("packs/index/")
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let mut index = e2v_sync::testing::decode_pack_index_segment_value_for_test(
        &source_repo_root.join(".e2v"),
        &index_path,
        &remote.get_physical(&index_path).unwrap(),
    )
    .unwrap();
    index["schema_version"] = serde_json::Value::from(99u64);
    remote
        .put_physical(
            &index_path,
            &e2v_sync::testing::encode_pack_index_segment_value_for_test(
                &source_repo_root.join(".e2v"),
                &index_path,
                &index,
            )
            .unwrap(),
        )
        .unwrap();

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

    let error = fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("schema version")
            || error.to_string().contains("unsupported")
            || error.to_string().contains("pack"),
        "unexpected error: {error:#}"
    );
    assert_eq!(
        target_repo_root
            .join(".e2v")
            .join("objects")
            .read_dir()
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn fetch_rejects_remote_state_for_a_different_local_repository() {
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
    fs::write(target_repo_root.join("local.txt"), b"local history").unwrap();
    local
        .commit(CommitOptions {
            repo_root: target_repo_root.clone(),
            message: "local-only-history".to_string(),
        })
        .unwrap();

    let original_ref_bytes = fs::read(
        target_repo_root
            .join(".e2v")
            .join("refs")
            .join("default.json"),
    )
    .unwrap();
    let original_layout_bytes =
        fs::read(target_repo_root.join(".e2v").join("layout_root.json")).unwrap();

    let error = fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token,
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("repository identity mismatch"),
        "unexpected error: {error:#}"
    );
    assert_eq!(
        fs::read(
            target_repo_root
                .join(".e2v")
                .join("refs")
                .join("default.json")
        )
        .unwrap(),
        original_ref_bytes
    );
    assert_eq!(
        fs::read(target_repo_root.join(".e2v").join("layout_root.json")).unwrap(),
        original_layout_bytes
    );
}

#[test]
fn fetch_rejects_different_remote_even_when_local_keyring_pointer_is_missing() {
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
    fs::write(target_repo_root.join("local.txt"), b"local history").unwrap();
    local
        .commit(CommitOptions {
            repo_root: target_repo_root.clone(),
            message: "local-only-history".to_string(),
        })
        .unwrap();

    let original_ref_bytes = fs::read(
        target_repo_root
            .join(".e2v")
            .join("refs")
            .join("default.json"),
    )
    .unwrap();
    let original_layout_bytes =
        fs::read(target_repo_root.join(".e2v").join("layout_root.json")).unwrap();
    fs::remove_file(
        target_repo_root
            .join(".e2v")
            .join("keyring")
            .join("keyring.current"),
    )
    .unwrap();

    let error = fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token,
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("repository identity mismatch"),
        "unexpected error: {error:#}"
    );
    assert_eq!(
        fs::read(
            target_repo_root
                .join(".e2v")
                .join("refs")
                .join("default.json")
        )
        .unwrap(),
        original_ref_bytes
    );
    assert_eq!(
        fs::read(target_repo_root.join(".e2v").join("layout_root.json")).unwrap(),
        original_layout_bytes
    );
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
    assert!(
        !remote.list_physical("objects/").unwrap().is_empty()
            || !remote.list_physical("packs/index/").unwrap().is_empty()
    );
}

#[test]
fn clone_writes_keyring_pointer_even_when_remote_keyring_listing_omits_pointer_file() {
    let (temp, _facade, _source_repo_root, branch_token, remote) = seed_remote();
    let clone_repo_root = temp.path().join("clone-target");
    let list_hidden_remote = KeyringPointerHiddenFromListRemote::new(remote.clone());

    clone_remote(
        &list_hidden_remote,
        CloneOptions {
            repo_root: clone_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_token,
        },
    )
    .unwrap();

    assert_eq!(
        fs::read(
            clone_repo_root
                .join(".e2v")
                .join("keyring")
                .join("keyring.current")
        )
        .unwrap(),
        remote
            .get_physical("control/keyring/keyring.current")
            .unwrap()
    );
}

#[test]
fn clone_uses_keyring_pointer_ref_when_physical_pointer_file_is_missing() {
    let (temp, _facade, source_repo_root, branch_token, remote) = seed_remote();
    let pointer_bytes = remote
        .get_physical("control/keyring/keyring.current")
        .unwrap();
    remote
        .compare_and_swap_ref(
            &keyring_pointer_ref_token(&source_repo_root),
            None,
            e2v_store::EncryptedRef::new(pointer_bytes.clone()),
        )
        .unwrap();
    remote
        .delete_physical("control/keyring/keyring.current")
        .unwrap();
    let clone_repo_root = temp.path().join("clone-target");

    let cloned = clone_remote(
        &remote,
        CloneOptions {
            repo_root: clone_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_token,
        },
    )
    .unwrap();

    assert!(cloned.head_snapshot_id.is_some());
    assert_eq!(
        fs::read(
            clone_repo_root
                .join(".e2v")
                .join("keyring")
                .join("keyring.current")
        )
        .unwrap(),
        pointer_bytes
    );
}

#[test]
fn clone_rejects_remote_without_keyring_pointer_ref_even_if_physical_pointer_file_exists() {
    let (temp, _facade, _source_repo_root, branch_token, remote) = seed_remote();
    let hidden_ref_remote = KeyringPointerRefHiddenRemote::new(remote.clone());
    let clone_repo_root = temp.path().join("clone-target");

    let error = clone_remote(
        &hidden_ref_remote,
        CloneOptions {
            repo_root: clone_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_token,
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("keyring pointer ref")
            || error.to_string().contains("ambiguous")
            || error.to_string().contains("missing"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn clone_does_not_restore_remote_keyring_lock_file() {
    let (temp, _facade, _source_repo_root, branch_token, remote) = seed_remote();
    remote
        .put_physical("control/keyring/keyring.lock", b"locked")
        .unwrap();
    let clone_repo_root = temp.path().join("clone-target");

    clone_remote(
        &remote,
        CloneOptions {
            repo_root: clone_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_token,
        },
    )
    .unwrap();

    assert!(
        !clone_repo_root
            .join(".e2v")
            .join("keyring")
            .join("keyring.lock")
            .exists(),
        "clone should not restore remote keyring lock files"
    );
    RepositoryFacade::new()
        .change_password(
            &clone_repo_root,
            "correct horse battery staple",
            "new horse battery staple",
        )
        .unwrap();
}

#[test]
fn clone_rejects_non_empty_target_directory() {
    let (temp, _facade, _source_repo_root, branch_token, remote) = seed_remote();
    let clone_repo_root = temp.path().join("clone-target");
    fs::create_dir_all(&clone_repo_root).unwrap();
    fs::write(clone_repo_root.join("keep.txt"), b"existing file").unwrap();

    let error = clone_remote(
        &remote,
        CloneOptions {
            repo_root: clone_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_token,
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("non-empty")
            || error.to_string().contains("not empty")
            || error.to_string().contains("existing")
            || error.to_string().contains("must be empty"),
        "unexpected error: {error:#}"
    );
    assert_eq!(
        fs::read(clone_repo_root.join("keep.txt")).unwrap(),
        b"existing file"
    );
    assert!(!clone_repo_root.join(".e2v").exists());
}

#[test]
fn clone_rejects_missing_remote_branch_ref_without_creating_control_dir() {
    let (temp, _facade, _source_repo_root, _branch_token, remote) = seed_remote();
    let clone_repo_root = temp.path().join("clone-target");

    let error = clone_remote(
        &remote,
        CloneOptions {
            repo_root: clone_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_token: "missing-branch-token".to_string(),
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("remote branch ref not found")
            || error.to_string().contains("branch ref"),
        "unexpected error: {error:#}"
    );
    assert!(!clone_repo_root.join(".e2v").exists());
}

#[test]
fn clone_succeeds_without_remote_config_file() {
    let (temp, _facade, _source_repo_root, branch_token, remote) = seed_remote();
    if remote.exists_physical("control/config.json") {
        remote.delete_physical("control/config.json").unwrap();
    }
    let clone_repo_root = temp.path().join("clone-target");

    let cloned = clone_remote(
        &remote,
        CloneOptions {
            repo_root: clone_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_token,
        },
    )
    .unwrap();

    assert!(cloned.head_snapshot_id.is_some());
    assert!(clone_repo_root.join(".e2v").exists());
}

#[test]
fn clone_rejects_remote_keyring_pointer_that_references_missing_generation() {
    let (temp, _facade, source_repo_root, branch_token, remote) = seed_remote();
    let pointer_bytes = serde_json::to_vec(&serde_json::json!({
        "generation": 2u64,
        "current": "keyring.2"
    }))
    .unwrap();
    let expected_version = remote
        .read_ref(&keyring_pointer_ref_token(&source_repo_root))
        .unwrap()
        .map(|stored| stored.version);
    remote
        .compare_and_swap_ref(
            &keyring_pointer_ref_token(&source_repo_root),
            expected_version,
            e2v_store::EncryptedRef::new(pointer_bytes.clone()),
        )
        .unwrap();
    remote
        .put_physical("control/keyring/keyring.current", &pointer_bytes)
        .unwrap();
    let clone_repo_root = temp.path().join("clone-target");

    let error = clone_remote(
        &remote,
        CloneOptions {
            repo_root: clone_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_token,
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("keyring.2")
            || error.to_string().contains("missing physical object")
            || error.to_string().contains("keyring"),
        "unexpected error: {error:#}"
    );
    assert!(!clone_repo_root.join(".e2v").exists());
}

#[test]
fn clone_rejects_remote_layout_root_with_unsupported_schema_version() {
    let (temp, _facade, _source_repo_root, branch_token, remote) = seed_remote();
    let current_layout = remote.read_layout_root().unwrap();
    remote
        .compare_and_swap_layout_root(
            current_layout.generation,
            e2v_store::LayoutRoot {
                schema_version: 99,
                ..current_layout.clone()
            },
        )
        .unwrap();
    let clone_repo_root = temp.path().join("clone-target");

    let error = clone_remote(
        &remote,
        CloneOptions {
            repo_root: clone_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_token,
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("layout")
            || error.to_string().contains("schema")
            || error.to_string().contains("unsupported"),
        "unexpected error: {error:#}"
    );
    assert!(!clone_repo_root.join(".e2v").exists());
}

#[test]
fn clone_cleans_up_control_dir_when_remote_head_snapshot_is_missing() {
    let (temp, facade, source_repo_root, branch_token, remote) = seed_remote();
    let clone_repo_root = temp.path().join("clone-target");
    let head_snapshot_id = facade
        .snapshots(&source_repo_root)
        .unwrap()
        .first()
        .unwrap()
        .snapshot_id
        .clone();
    remote
        .delete_physical(&format!("objects/{head_snapshot_id}.json"))
        .unwrap();

    let error = clone_remote(
        &remote,
        CloneOptions {
            repo_root: clone_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_token,
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains(&head_snapshot_id)
            || error.to_string().contains("snapshot")
            || error.to_string().contains("missing"),
        "unexpected error: {error:#}"
    );
    assert!(!clone_repo_root.join(".e2v").exists());
}

#[test]
fn clone_cleans_up_partial_control_dir_when_remote_breaks_after_preflight() {
    let (temp, _facade, _source_repo_root, branch_token, remote) = seed_remote();
    let clone_repo_root = temp.path().join("clone-target");
    let flaky_remote = KeyringPointerDisappearsAfterRefReadRemote::new(remote);

    let error = clone_remote(
        &flaky_remote,
        CloneOptions {
            repo_root: clone_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_token,
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("layout_root.json")
            || error.to_string().contains("keyring")
            || error.to_string().contains("missing physical object"),
        "unexpected error: {error:#}"
    );
    assert!(!clone_repo_root.join(".e2v").exists());
}

#[test]
fn clone_cleans_up_control_dir_when_password_is_wrong() {
    let (temp, _facade, _source_repo_root, branch_token, remote) = seed_remote();
    let clone_repo_root = temp.path().join("clone-target");

    let error = clone_remote(
        &remote,
        CloneOptions {
            repo_root: clone_repo_root.clone(),
            password: "definitely wrong password".to_string(),
            branch_token,
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("wrong password")
            || error.to_string().contains("unlock")
            || error.to_string().contains("keyring")
            || error.to_string().contains("validation"),
        "unexpected error: {error:#}"
    );
    assert!(!clone_repo_root.join(".e2v").exists());
}

#[test]
fn fetch_does_not_expand_layout_root_bytes_with_pretty_json_whitespace() {
    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("fetch.rs"),
    )
    .unwrap();

    assert!(
        !source.contains("serde_json::to_vec_pretty(&layout_root)?"),
        "fetch should preserve compact layout-root bytes instead of re-encoding them with pretty JSON whitespace"
    );
}

#[test]
fn fetch_preserves_local_device_credential_file() {
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

    let device_file = target_repo_root
        .join(".e2v")
        .join("device")
        .join("local-device.json");
    let original = fs::read(&device_file).unwrap();

    fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token,
        },
    )
    .unwrap();

    assert_eq!(fs::read(&device_file).unwrap(), original);
}

#[test]
fn fetch_rejects_remote_keyring_paths_that_escape_the_keyring_directory() {
    let (temp, _facade, _source_repo_root, branch_token, remote) = seed_remote();
    remote
        .put_physical("control/keyring/../../keep.txt", b"malicious overwrite")
        .unwrap();

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
    fs::write(target_repo_root.join("keep.txt"), b"safe").unwrap();

    let error = fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token,
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("invalid remote keyring path")
            || error.to_string().contains("path escapes")
            || error.to_string().contains("path traversal"),
        "unexpected error: {error:#}"
    );
    assert_eq!(
        fs::read(target_repo_root.join("keep.txt")).unwrap(),
        b"safe"
    );
}

#[test]
fn fetch_rejects_remote_keyring_pointer_that_references_missing_generation() {
    let (temp, _facade, source_repo_root, branch_token, remote) = seed_remote();
    let pointer_bytes = serde_json::to_vec(&serde_json::json!({
        "generation": 2u64,
        "current": "keyring.2"
    }))
    .unwrap();
    let expected_version = remote
        .read_ref(&keyring_pointer_ref_token(&source_repo_root))
        .unwrap()
        .map(|stored| stored.version);
    remote
        .compare_and_swap_ref(
            &keyring_pointer_ref_token(&source_repo_root),
            expected_version,
            e2v_store::EncryptedRef::new(pointer_bytes.clone()),
        )
        .unwrap();
    remote
        .put_physical("control/keyring/keyring.current", &pointer_bytes)
        .unwrap();

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

    let original_pointer = fs::read(
        target_repo_root
            .join(".e2v")
            .join("keyring")
            .join("keyring.current"),
    )
    .unwrap();

    let error = fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token,
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("keyring.2")
            || error.to_string().contains("missing physical object")
            || error.to_string().contains("keyring"),
        "unexpected error: {error:#}"
    );
    assert_eq!(
        fs::read(
            target_repo_root
                .join(".e2v")
                .join("keyring")
                .join("keyring.current")
        )
        .unwrap(),
        original_pointer
    );
}

#[test]
fn fetch_rejects_remote_keyring_pointer_generation_mismatch() {
    let (temp, _facade, _source_repo_root, branch_token, remote) = seed_remote();
    let keyring_one_path = "control/keyring/keyring.1";
    let mut keyring_one: serde_json::Value =
        serde_json::from_slice(&remote.get_physical(keyring_one_path).unwrap()).unwrap();
    keyring_one["generation"] = serde_json::Value::from(7u64);
    remote
        .put_physical(keyring_one_path, &serde_json::to_vec(&keyring_one).unwrap())
        .unwrap();

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

    let error = fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token,
        },
    )
    .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("keyring pointer generation mismatch")
            || error.to_string().contains("generation mismatch")
            || error.to_string().contains("keyring"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn fetch_rejects_invalid_remote_keyring_pointer_before_mutating_local_control_plane() {
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

    let original_pointer_bytes = fs::read(
        target_repo_root
            .join(".e2v")
            .join("keyring")
            .join("keyring.current"),
    )
    .unwrap();
    let original_ref_bytes = fs::read(
        target_repo_root
            .join(".e2v")
            .join("refs")
            .join("default.json"),
    )
    .unwrap();
    let original_layout_bytes =
        fs::read(target_repo_root.join(".e2v").join("layout_root.json")).unwrap();

    let keyring_one_path = "control/keyring/keyring.1";
    let mut keyring_one: serde_json::Value =
        serde_json::from_slice(&remote.get_physical(keyring_one_path).unwrap()).unwrap();
    keyring_one["generation"] = serde_json::Value::from(0u64);
    remote
        .put_physical(keyring_one_path, &serde_json::to_vec(&keyring_one).unwrap())
        .unwrap();
    remote
        .put_physical(
            "control/keyring/keyring.current",
            br#"{"generation":0,"current":"keyring.1"}"#,
        )
        .unwrap();

    let error = fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token,
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("invalid remote keyring pointer")
            || error.to_string().contains("keyring pointer"),
        "unexpected error: {error:#}"
    );
    assert_eq!(
        fs::read(
            target_repo_root
                .join(".e2v")
                .join("keyring")
                .join("keyring.current")
        )
        .unwrap(),
        original_pointer_bytes
    );
    assert_eq!(
        fs::read(
            target_repo_root
                .join(".e2v")
                .join("refs")
                .join("default.json")
        )
        .unwrap(),
        original_ref_bytes
    );
    assert_eq!(
        fs::read(target_repo_root.join(".e2v").join("layout_root.json")).unwrap(),
        original_layout_bytes
    );
}

#[test]
fn fetch_rejects_remote_keyring_generation_without_password_envelope() {
    let (temp, _facade, _source_repo_root, branch_token, remote) = seed_remote();
    let keyring_one_path = "control/keyring/keyring.1";
    let mut keyring_one: serde_json::Value =
        serde_json::from_slice(&remote.get_physical(keyring_one_path).unwrap()).unwrap();
    keyring_one["envelopes"] = serde_json::Value::Array(vec![]);
    remote
        .put_physical(keyring_one_path, &serde_json::to_vec(&keyring_one).unwrap())
        .unwrap();

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

    let error = fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token,
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("password envelope")
            || error.to_string().contains("keyring")
            || error.to_string().contains("envelopes"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn fetch_rejects_remote_layout_root_with_unsupported_schema_version() {
    let (temp, _facade, _source_repo_root, branch_token, remote) = seed_remote();
    let current_layout = remote.read_layout_root().unwrap();
    remote
        .compare_and_swap_layout_root(
            current_layout.generation,
            e2v_store::LayoutRoot {
                schema_version: 99,
                ..current_layout.clone()
            },
        )
        .unwrap();

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

    let original_ref_bytes = fs::read(
        target_repo_root
            .join(".e2v")
            .join("refs")
            .join("default.json"),
    )
    .unwrap();
    let original_layout_bytes =
        fs::read(target_repo_root.join(".e2v").join("layout_root.json")).unwrap();

    let error = fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token,
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("layout")
            || error.to_string().contains("schema")
            || error.to_string().contains("unsupported"),
        "unexpected error: {error:#}"
    );
    assert_eq!(
        fs::read(
            target_repo_root
                .join(".e2v")
                .join("refs")
                .join("default.json")
        )
        .unwrap(),
        original_ref_bytes
    );
    assert_eq!(
        fs::read(target_repo_root.join(".e2v").join("layout_root.json")).unwrap(),
        original_layout_bytes
    );
}

#[test]
fn fetch_rejects_remote_ref_that_points_to_missing_head_snapshot_when_local_ref_is_unlocked() {
    let (temp, _facade, _source_repo_root, branch_token, remote) = seed_remote();
    let target_repo_root = temp.path().join("fetch-target");

    clone_remote(
        &remote,
        CloneOptions {
            repo_root: target_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_token: branch_token.clone(),
        },
    )
    .unwrap();

    let original_ref_bytes = fs::read(
        target_repo_root
            .join(".e2v")
            .join("refs")
            .join("default.json"),
    )
    .unwrap();
    let original_layout_bytes =
        fs::read(target_repo_root.join(".e2v").join("layout_root.json")).unwrap();

    let head_snapshot_id = RepositoryFacade::new()
        .snapshots(&target_repo_root)
        .unwrap()
        .first()
        .unwrap()
        .snapshot_id
        .clone();
    remote
        .delete_physical(&format!("objects/{head_snapshot_id}.json"))
        .unwrap();

    let error = fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token,
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains(&head_snapshot_id)
            || error.to_string().contains("snapshot")
            || error.to_string().contains("missing"),
        "unexpected error: {error:#}"
    );
    assert_eq!(
        fs::read(
            target_repo_root
                .join(".e2v")
                .join("refs")
                .join("default.json")
        )
        .unwrap(),
        original_ref_bytes
    );
    assert_eq!(
        fs::read(target_repo_root.join(".e2v").join("layout_root.json")).unwrap(),
        original_layout_bytes
    );
}

#[test]
fn fetch_rejects_remote_ref_that_points_to_missing_head_snapshot_after_password_rotation() {
    let (temp, facade, source_repo_root, branch_token, remote) = seed_remote();
    let target_repo_root = temp.path().join("clone-target");

    clone_remote(
        &remote,
        CloneOptions {
            repo_root: target_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_token: branch_token.clone(),
        },
    )
    .unwrap();

    facade
        .change_password(
            &source_repo_root,
            "correct horse battery staple",
            "new horse battery staple",
        )
        .unwrap();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: source_repo_root,
            branch_token: branch_token.clone(),
            operation_id: "rotate-password-before-missing-head-fetch".to_string(),
        },
    )
    .unwrap();

    let original_ref_bytes = fs::read(
        target_repo_root
            .join(".e2v")
            .join("refs")
            .join("default.json"),
    )
    .unwrap();
    let original_layout_bytes =
        fs::read(target_repo_root.join(".e2v").join("layout_root.json")).unwrap();

    let head_snapshot_id = RepositoryFacade::new()
        .snapshots(&target_repo_root)
        .unwrap()
        .first()
        .unwrap()
        .snapshot_id
        .clone();
    remote
        .delete_physical(&format!("objects/{head_snapshot_id}.json"))
        .unwrap();

    let error = fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token,
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains(&head_snapshot_id)
            || error.to_string().contains("snapshot")
            || error.to_string().contains("missing"),
        "unexpected error: {error:#}"
    );
    assert_eq!(
        fs::read(
            target_repo_root
                .join(".e2v")
                .join("refs")
                .join("default.json")
        )
        .unwrap(),
        original_ref_bytes
    );
    assert_eq!(
        fs::read(target_repo_root.join(".e2v").join("layout_root.json")).unwrap(),
        original_layout_bytes
    );
}

#[test]
fn fetch_rejects_remote_head_snapshot_when_reachable_chunk_is_corrupted_for_unlocked_local_repo() {
    let (temp, _facade, source_repo_root, branch_token, remote) = seed_remote();
    let target_repo_root = temp.path().join("fetch-target");

    clone_remote(
        &remote,
        CloneOptions {
            repo_root: target_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_token: branch_token.clone(),
        },
    )
    .unwrap();

    let manifest_store = e2v_core::ManifestStore::new(&source_repo_root);
    let head_snapshot_id = RepositoryFacade::new()
        .snapshots(&source_repo_root)
        .unwrap()
        .first()
        .unwrap()
        .snapshot_id
        .clone();
    let reachable_ids = manifest_store
        .collect_reachable_object_ids(&head_snapshot_id)
        .unwrap();
    let chunk_id = reachable_ids
        .into_iter()
        .find(|object_id| {
            RepositoryFacade::new()
                .verify_object(&source_repo_root, object_id, "chunk")
                .is_ok()
        })
        .unwrap();
    let remote_chunk_path = format!("objects/{chunk_id}.json");
    let mut bytes = remote.get_physical(&remote_chunk_path).unwrap();
    let last_index = bytes.len() - 1;
    bytes[last_index] ^= 0x01;
    remote.put_physical(&remote_chunk_path, &bytes).unwrap();

    let original_ref_bytes = fs::read(
        target_repo_root
            .join(".e2v")
            .join("refs")
            .join("default.json"),
    )
    .unwrap();
    let original_layout_bytes =
        fs::read(target_repo_root.join(".e2v").join("layout_root.json")).unwrap();

    let error = fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token,
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("authentication")
            || error.to_string().contains("snapshot")
            || error.to_string().contains("failed"),
        "unexpected error: {error:#}"
    );
    assert_eq!(
        fs::read(
            target_repo_root
                .join(".e2v")
                .join("refs")
                .join("default.json")
        )
        .unwrap(),
        original_ref_bytes
    );
    assert_eq!(
        fs::read(target_repo_root.join(".e2v").join("layout_root.json")).unwrap(),
        original_layout_bytes
    );
}

#[test]
fn clone_rejects_remote_head_snapshot_when_reachable_chunk_is_corrupted() {
    let (temp, _facade, source_repo_root, branch_token, remote) = seed_remote();
    let manifest_store = e2v_core::ManifestStore::new(&source_repo_root);
    let head_snapshot_id = RepositoryFacade::new()
        .snapshots(&source_repo_root)
        .unwrap()
        .first()
        .unwrap()
        .snapshot_id
        .clone();
    let reachable_ids = manifest_store
        .collect_reachable_object_ids(&head_snapshot_id)
        .unwrap();
    let chunk_id = reachable_ids
        .into_iter()
        .find(|object_id| {
            RepositoryFacade::new()
                .verify_object(&source_repo_root, object_id, "chunk")
                .is_ok()
        })
        .unwrap();
    let remote_chunk_path = format!("objects/{chunk_id}.json");
    let mut bytes = remote.get_physical(&remote_chunk_path).unwrap();
    let last_index = bytes.len() - 1;
    bytes[last_index] ^= 0x01;
    remote.put_physical(&remote_chunk_path, &bytes).unwrap();

    let clone_repo_root = temp.path().join("clone-target");
    let error = clone_remote(
        &remote,
        CloneOptions {
            repo_root: clone_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_token,
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("authentication")
            || error.to_string().contains("snapshot")
            || error.to_string().contains("failed"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn fetch_rejects_remote_head_snapshot_when_reachable_chunk_is_corrupted_for_empty_local_repo() {
    let (temp, _facade, source_repo_root, branch_token, remote) = seed_remote();
    let manifest_store = e2v_core::ManifestStore::new(&source_repo_root);
    let head_snapshot_id = RepositoryFacade::new()
        .snapshots(&source_repo_root)
        .unwrap()
        .first()
        .unwrap()
        .snapshot_id
        .clone();
    let reachable_ids = manifest_store
        .collect_reachable_object_ids(&head_snapshot_id)
        .unwrap();
    let chunk_id = reachable_ids
        .into_iter()
        .find(|object_id| {
            RepositoryFacade::new()
                .verify_object(&source_repo_root, object_id, "chunk")
                .is_ok()
        })
        .unwrap();
    let remote_chunk_path = format!("objects/{chunk_id}.json");
    let mut bytes = remote.get_physical(&remote_chunk_path).unwrap();
    let last_index = bytes.len() - 1;
    bytes[last_index] ^= 0x01;
    remote.put_physical(&remote_chunk_path, &bytes).unwrap();

    let target_repo_root = temp.path().join("fetch-target");
    fs::create_dir_all(&target_repo_root).unwrap();
    RepositoryFacade::new()
        .init(InitOptions {
            repo_root: target_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();

    let error = fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token,
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("authentication")
            || error.to_string().contains("snapshot")
            || error.to_string().contains("failed"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn clone_rejects_remote_keyring_generation_without_password_envelope() {
    let (temp, _facade, _source_repo_root, branch_token, remote) = seed_remote();
    let keyring_one_path = "control/keyring/keyring.1";
    let mut keyring_one: serde_json::Value =
        serde_json::from_slice(&remote.get_physical(keyring_one_path).unwrap()).unwrap();
    keyring_one["envelopes"] = serde_json::Value::Array(vec![]);
    remote
        .put_physical(keyring_one_path, &serde_json::to_vec(&keyring_one).unwrap())
        .unwrap();

    let clone_repo_root = temp.path().join("clone-target");
    let error = clone_remote(
        &remote,
        CloneOptions {
            repo_root: clone_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_token,
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("password envelope")
            || error.to_string().contains("keyring")
            || error.to_string().contains("envelopes"),
        "unexpected error: {error:#}"
    );
    assert!(!clone_repo_root.join(".e2v").exists());
}

#[test]
fn fetch_updates_keyring_after_remote_password_rotation_without_new_snapshot() {
    let (temp, facade, source_repo_root, branch_token, remote) = seed_remote();
    let clone_repo_root = temp.path().join("clone-target");

    clone_remote(
        &remote,
        CloneOptions {
            repo_root: clone_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_token: branch_token.clone(),
        },
    )
    .unwrap();

    facade
        .change_password(
            &source_repo_root,
            "correct horse battery staple",
            "new horse battery staple",
        )
        .unwrap();

    let republished = push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: source_repo_root.clone(),
            branch_token: branch_token.clone(),
            operation_id: "rotate-password-push".to_string(),
        },
    )
    .unwrap();
    assert_eq!(republished.uploaded_objects, 0);

    let fetched = fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: clone_repo_root.clone(),
            branch_token: branch_token.clone(),
        },
    )
    .unwrap();
    assert_eq!(fetched.downloaded_objects, 0);

    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&clone_repo_root.join(".e2v"));
    let clone_facade = RepositoryFacade::new();
    let old_password_error = clone_facade
        .unlock(&clone_repo_root, "correct horse battery staple")
        .unwrap_err();
    assert!(
        old_password_error.to_string().contains("wrong password")
            || old_password_error.to_string().contains("unlock")
            || old_password_error.to_string().contains("keyring"),
        "unexpected old password error: {old_password_error:#}"
    );

    clone_facade
        .unlock(&clone_repo_root, "new horse battery staple")
        .unwrap();
}

#[test]
fn fetch_does_not_publish_rotated_keyring_pointer_before_default_ref_write_succeeds() {
    let (temp, facade, source_repo_root, branch_token, remote) = seed_remote();
    let clone_repo_root = temp.path().join("clone-target");

    clone_remote(
        &remote,
        CloneOptions {
            repo_root: clone_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_token: branch_token.clone(),
        },
    )
    .unwrap();

    facade
        .change_password(
            &source_repo_root,
            "correct horse battery staple",
            "new horse battery staple",
        )
        .unwrap();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: source_repo_root,
            branch_token: branch_token.clone(),
            operation_id: "fetch-rotate-password-pointer-order".to_string(),
        },
    )
    .unwrap();

    fs::create_dir_all(
        clone_repo_root
            .join(".e2v")
            .join("refs")
            .join("default.json.tmp"),
    )
    .unwrap();

    let error = fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: clone_repo_root.clone(),
            branch_token,
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("default.json")
            || error.to_string().contains("publish")
            || error.to_string().contains("write"),
        "unexpected error: {error:#}"
    );

    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&clone_repo_root.join(".e2v"));
    RepositoryFacade::new()
        .unlock(&clone_repo_root, "correct horse battery staple")
        .unwrap();
}

#[test]
fn fetch_repairs_empty_local_repository_when_local_keyring_pointer_is_corrupt() {
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

    fs::write(
        target_repo_root
            .join(".e2v")
            .join("keyring")
            .join("keyring.current"),
        br#"{"generation":999,"current":"missing.json"}"#,
    )
    .unwrap();

    let fetched = fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token: branch_token.clone(),
        },
    )
    .unwrap();

    assert!(fetched.downloaded_objects > 0);
    assert_eq!(
        fs::read(
            target_repo_root
                .join(".e2v")
                .join("keyring")
                .join("keyring.current")
        )
        .unwrap(),
        remote
            .get_physical("control/keyring/keyring.current")
            .unwrap()
    );
}

#[test]
fn fetch_repairs_empty_local_repository_even_when_control_plane_directories_are_missing() {
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

    fs::remove_dir_all(target_repo_root.join(".e2v").join("keyring")).unwrap();
    fs::remove_dir_all(target_repo_root.join(".e2v").join("refs")).unwrap();

    let fetched = fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: target_repo_root.clone(),
            branch_token: branch_token.clone(),
        },
    )
    .unwrap();

    assert!(fetched.downloaded_objects > 0);
    assert_eq!(
        fs::read(
            target_repo_root
                .join(".e2v")
                .join("keyring")
                .join("keyring.current")
        )
        .unwrap(),
        remote
            .get_physical("control/keyring/keyring.current")
            .unwrap()
    );
    assert_eq!(
        fs::read(
            target_repo_root
                .join(".e2v")
                .join("refs")
                .join("default.json")
        )
        .unwrap(),
        remote
            .read_ref(&RefToken::new(branch_token))
            .unwrap()
            .unwrap()
            .value
            .bytes
    );
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
            password: Some("correct horse battery staple".to_string()),
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
        remote
            .put_physical(&relative_path.0, &relative_path.1)
            .unwrap();
    }
    for keyring_file in e2v_core::sync_support::list_keyring_files(&source_repo_root).unwrap() {
        let file_name = keyring_file.file_name().unwrap().to_str().unwrap();
        remote
            .put_physical(
                &format!("control/keyring/{file_name}"),
                &fs::read(&keyring_file).unwrap(),
            )
            .unwrap();
    }
    remote
        .compare_and_swap_ref(
            &keyring_pointer_ref_token(&source_repo_root),
            None,
            e2v_store::EncryptedRef::new(
                fs::read(
                    source_repo_root
                        .join(".e2v")
                        .join("keyring")
                        .join("keyring.current"),
                )
                .unwrap(),
            ),
        )
        .unwrap();
    let layout_root = remote.read_layout_root().unwrap();
    let next_layout_root: e2v_store::LayoutRoot = serde_json::from_slice(
        &e2v_core::sync_support::read_layout_root_bytes(&source_repo_root).unwrap(),
    )
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
            password: Some("correct horse battery staple".to_string()),
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
            branch_token: state.branch.token_hex.clone(),
        },
    )
    .unwrap();
    assert!(cloned.head_snapshot_id.is_some());
}

#[test]
fn fetch_and_clone_work_with_conservative_webdav_backend_adapter() {
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
    fs::write(
        source_repo_root.join("hello.txt"),
        b"hello conservative webdav",
    )
    .unwrap();
    facade
        .commit(CommitOptions {
            repo_root: source_repo_root.clone(),
            message: "adapter-webdav".to_string(),
        })
        .unwrap();

    let remote = WebdavAlistMockBackend::webdav();
    for relative_path in e2v_core::sync_support::list_local_object_files(&source_repo_root)
        .unwrap()
        .into_iter()
        .map(|path| {
            let file_name = path.file_name().unwrap().to_str().unwrap().to_string();
            let bytes = fs::read(&path).unwrap();
            (format!("objects/{file_name}"), bytes)
        })
    {
        remote
            .put_physical(&relative_path.0, &relative_path.1)
            .unwrap();
    }
    for keyring_file in e2v_core::sync_support::list_keyring_files(&source_repo_root).unwrap() {
        let file_name = keyring_file.file_name().unwrap().to_str().unwrap();
        remote
            .put_physical(
                &format!("control/keyring/{file_name}"),
                &fs::read(&keyring_file).unwrap(),
            )
            .unwrap();
    }
    remote
        .compare_and_swap_ref(
            &keyring_pointer_ref_token(&source_repo_root),
            None,
            e2v_store::EncryptedRef::new(
                fs::read(
                    source_repo_root
                        .join(".e2v")
                        .join("keyring")
                        .join("keyring.current"),
                )
                .unwrap(),
            ),
        )
        .unwrap();
    let layout_root = remote.read_layout_root().unwrap();
    let next_layout_root: e2v_store::LayoutRoot = serde_json::from_slice(
        &e2v_core::sync_support::read_layout_root_bytes(&source_repo_root).unwrap(),
    )
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
            password: Some("correct horse battery staple".to_string()),
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
            branch_token: state.branch.token_hex.clone(),
        },
    )
    .unwrap();
    assert!(cloned.head_snapshot_id.is_some());
}

#[test]
fn fetch_rejects_remote_layout_generation_rollback_below_local_high_water() {
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
    fs::write(source_repo_root.join("hello.txt"), b"hello rollback").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: source_repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    for relative_path in e2v_core::sync_support::list_local_object_files(&source_repo_root)
        .unwrap()
        .into_iter()
        .map(|path| {
            let file_name = path.file_name().unwrap().to_str().unwrap().to_string();
            let bytes = fs::read(&path).unwrap();
            (format!("objects/{file_name}"), bytes)
        })
    {
        remote
            .put_physical(&relative_path.0, &relative_path.1)
            .unwrap();
    }
    for keyring_file in e2v_core::sync_support::list_keyring_files(&source_repo_root).unwrap() {
        let file_name = keyring_file.file_name().unwrap().to_str().unwrap();
        remote
            .put_physical(
                &format!("control/keyring/{file_name}"),
                &fs::read(&keyring_file).unwrap(),
            )
            .unwrap();
    }
    remote
        .compare_and_swap_ref(
            &keyring_pointer_ref_token(&source_repo_root),
            None,
            e2v_store::EncryptedRef::new(
                fs::read(
                    source_repo_root
                        .join(".e2v")
                        .join("keyring")
                        .join("keyring.current"),
                )
                .unwrap(),
            ),
        )
        .unwrap();
    let layout_root = remote.read_layout_root().unwrap();
    let mut next_layout_root: e2v_store::LayoutRoot = serde_json::from_slice(
        &e2v_core::sync_support::read_layout_root_bytes(&source_repo_root).unwrap(),
    )
    .unwrap();
    next_layout_root.generation = 3;
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
    RepositoryFacade::new()
        .init(InitOptions {
            repo_root: fetch_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
    let trusted_state_root = temp.path().join("trusted-state-fetch");
    fs::create_dir_all(&trusted_state_root).unwrap();
    let _trusted_state_guard =
        e2v_sync::testing::override_trusted_state_dir_for_test(trusted_state_root.clone());
    let remote_keyring_path = source_repo_root
        .join(".e2v")
        .join("keyring")
        .join("keyring.1");
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

    let error = fetch_remote(
        &remote,
        FetchOptions {
            password: Some("correct horse battery staple".to_string()),
            repo_root: fetch_repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
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
fn clone_rejects_remote_layout_generation_rollback_below_external_high_water() {
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
    fs::write(source_repo_root.join("hello.txt"), b"hello rollback").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: source_repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    for relative_path in e2v_core::sync_support::list_local_object_files(&source_repo_root)
        .unwrap()
        .into_iter()
        .map(|path| {
            let file_name = path.file_name().unwrap().to_str().unwrap().to_string();
            let bytes = fs::read(&path).unwrap();
            (format!("objects/{file_name}"), bytes)
        })
    {
        remote
            .put_physical(&relative_path.0, &relative_path.1)
            .unwrap();
    }
    for keyring_file in e2v_core::sync_support::list_keyring_files(&source_repo_root).unwrap() {
        let file_name = keyring_file.file_name().unwrap().to_str().unwrap();
        remote
            .put_physical(
                &format!("control/keyring/{file_name}"),
                &fs::read(&keyring_file).unwrap(),
            )
            .unwrap();
    }
    remote
        .compare_and_swap_ref(
            &keyring_pointer_ref_token(&source_repo_root),
            None,
            e2v_store::EncryptedRef::new(
                fs::read(
                    source_repo_root
                        .join(".e2v")
                        .join("keyring")
                        .join("keyring.current"),
                )
                .unwrap(),
            ),
        )
        .unwrap();
    let layout_root = remote.read_layout_root().unwrap();
    let next_layout_root: e2v_store::LayoutRoot = serde_json::from_slice(
        &e2v_core::sync_support::read_layout_root_bytes(&source_repo_root).unwrap(),
    )
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

    let trusted_state_root = temp.path().join("trusted-state");
    fs::create_dir_all(&trusted_state_root).unwrap();
    let _trusted_state_guard =
        e2v_sync::testing::override_trusted_state_dir_for_test(trusted_state_root.clone());
    let remote_keyring_path = source_repo_root
        .join(".e2v")
        .join("keyring")
        .join("keyring.1");
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

    let clone_repo_root = temp.path().join("clone-target");
    let error = clone_remote(
        &remote,
        CloneOptions {
            repo_root: clone_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_token: state.branch.token_hex.clone(),
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
fn fetch_and_clone_restore_objects_from_remote_packs() {
    let _guard = e2v_sync::testing::override_small_object_pack_threshold_for_test(1);
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
    fs::write(source_repo_root.join("hello.txt"), b"hello packed").unwrap();
    let committed = facade
        .commit(CommitOptions {
            repo_root: source_repo_root.clone(),
            message: "packed-sync".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    let pushed = push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: source_repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "packed-sync-op".to_string(),
        },
    )
    .unwrap();
    assert!(pushed.uploaded_objects > 0);
    assert!(remote.list_physical("objects/").unwrap().is_empty());
    assert!(!remote.list_physical("packs/index/").unwrap().is_empty());

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
            password: Some("correct horse battery staple".to_string()),
            repo_root: fetch_repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
        },
    )
    .unwrap();
    assert!(fetched.downloaded_objects > 0);
    assert!(
        fetch_repo_root
            .join(".e2v")
            .join("objects")
            .join(format!("{}.json", committed.snapshot_id))
            .is_file()
    );

    let clone_repo_root = temp.path().join("clone-target");
    let cloned = clone_remote(
        &remote,
        CloneOptions {
            repo_root: clone_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_token: state.branch.token_hex.clone(),
        },
    )
    .unwrap();
    assert_eq!(
        cloned.head_snapshot_id.as_deref(),
        Some(committed.snapshot_id.as_str())
    );
}

#[test]
fn revoked_device_cannot_fetch_future_remote_head() {
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
    let owner_credential_bytes = fs::read(
        source_repo_root
            .join(".e2v")
            .join("device")
            .join("local-device.json"),
    )
    .unwrap();
    let member_invite = facade
        .share_invite_member(
            &source_repo_root,
            e2v_core::ShareInviteMemberOptions {
                display_name: "Alice".to_string(),
            },
        )
        .unwrap();
    let member = facade
        .share_accept_member(
            &source_repo_root,
            e2v_core::ShareAcceptMemberOptions {
                invite_bytes: member_invite.bundle_bytes.clone(),
                local_device_label: "alice-laptop".to_string(),
            },
        )
        .unwrap();
    fs::write(
        source_repo_root
            .join(".e2v")
            .join("device")
            .join("local-device.json"),
        owner_credential_bytes.clone(),
    )
    .unwrap();
    let device_invite = facade
        .share_invite_device(
            &source_repo_root,
            e2v_core::ShareInviteDeviceOptions {
                actor_id: member.actor_id.clone(),
                device_label: "alice-phone".to_string(),
            },
        )
        .unwrap();
    let device = facade
        .share_accept_device(
            &source_repo_root,
            e2v_core::ShareAcceptDeviceOptions {
                invite_bytes: device_invite.bundle_bytes,
                local_device_label: "alice-phone".to_string(),
            },
        )
        .unwrap();
    let revoked_device_credential_bytes = fs::read(
        source_repo_root
            .join(".e2v")
            .join("device")
            .join("local-device.json"),
    )
    .unwrap();
    fs::write(
        source_repo_root
            .join(".e2v")
            .join("device")
            .join("local-device.json"),
        owner_credential_bytes.clone(),
    )
    .unwrap();

    fs::write(source_repo_root.join("hello.txt"), b"before revoke").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: source_repo_root.clone(),
            message: "before-revoke".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: source_repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-before-device-revoke".to_string(),
        },
    )
    .unwrap();

    let revoked_clone_root = temp.path().join("revoked-clone");
    clone_remote(
        &remote,
        CloneOptions {
            repo_root: revoked_clone_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_token: state.branch.token_hex.clone(),
        },
    )
    .unwrap();

    facade
        .share_revoke_device(
            &source_repo_root,
            e2v_core::ShareRevokeDeviceOptions {
                device_id: device.device_id.clone(),
                password: "correct horse battery staple".to_string(),
            },
        )
        .unwrap();
    fs::write(source_repo_root.join("hello.txt"), b"after revoke").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: source_repo_root.clone(),
            message: "after-revoke".to_string(),
        })
        .unwrap();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: source_repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "push-after-device-revoke".to_string(),
        },
    )
    .unwrap();

    let member_device_credential =
        serde_json::from_slice::<serde_json::Value>(&revoked_device_credential_bytes).unwrap();
    assert_eq!(
        member_device_credential["device_id"].as_str(),
        Some(device.device_id.as_str())
    );
    let member_keyring_files = fs::read_dir(source_repo_root.join(".e2v").join("keyring"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    for path in member_keyring_files {
        let file_name = path.file_name().unwrap();
        fs::copy(
            &path,
            revoked_clone_root
                .join(".e2v")
                .join("keyring")
                .join(file_name),
        )
        .unwrap();
    }
    fs::create_dir_all(revoked_clone_root.join(".e2v").join("device")).unwrap();
    fs::write(
        revoked_clone_root
            .join(".e2v")
            .join("device")
            .join("local-device.json"),
        serde_json::to_vec_pretty(&member_device_credential).unwrap(),
    )
    .unwrap();
    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&revoked_clone_root.join(".e2v"));

    let error = fetch_remote(
        &remote,
        FetchOptions {
            repo_root: revoked_clone_root,
            branch_token: state.branch.token_hex.clone(),
            password: None,
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("missing epoch keys")
            || error.to_string().contains("matching local device envelope")
            || error.to_string().contains("device unlock failed")
            || error
                .to_string()
                .contains("current local device cannot unlock remote keyring"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn accepted_member_bootstrap_can_fetch_remote_without_password() {
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
    fs::write(owner_root.join("hello.txt"), b"hello shared").unwrap();
    let committed = facade
        .commit(CommitOptions {
            repo_root: owner_root.clone(),
            message: "shared-seed".to_string(),
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
            operation_id: "shared-seed-push".to_string(),
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

    let fetched = fetch_remote(
        &remote,
        FetchOptions {
            password: None,
            repo_root: recipient_root.clone(),
            branch_token: state.branch.token_hex.clone(),
        },
    )
    .unwrap();

    assert!(fetched.downloaded_objects > 0);
    let reopened = e2v_core::testing::unlock_with_local_device_for_test(&recipient_root).unwrap();
    assert_eq!(reopened.branch.token_hex, state.branch.token_hex);
    assert!(
        recipient_root
            .join(".e2v")
            .join("objects")
            .join(format!("{}.json", committed.snapshot_id))
            .is_file()
    );
}

#[test]
fn accepted_member_fetch_without_password_lists_remote_loose_objects_at_most_once() {
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
    fs::write(owner_root.join("hello.txt"), b"hello shared").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: owner_root.clone(),
            message: "shared-seed".to_string(),
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
            operation_id: "shared-seed-push-list-once".to_string(),
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

    let tracked_remote = GetTrackingBackend::new(remote);
    let fetched = fetch_remote(
        &tracked_remote,
        FetchOptions {
            password: None,
            repo_root: recipient_root,
            branch_token: state.branch.token_hex.clone(),
        },
    )
    .unwrap();

    assert!(fetched.downloaded_objects > 0);
    let object_list_calls = tracked_remote
        .list_paths()
        .into_iter()
        .filter(|path| path == "objects/")
        .count();
    assert!(
        object_list_calls <= 1,
        "expected accepted-member fetch to avoid relisting remote loose objects, saw {object_list_calls}"
    );
}

#[test]
fn accepted_member_published_keyring_can_fetch_future_epoch_after_owner_rotation() {
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
            operation_id: "future-epoch-bootstrap-seed".to_string(),
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
        FetchOptions {
            password: None,
            repo_root: recipient_root.clone(),
            branch_token: state.branch.token_hex.clone(),
        },
    )
    .unwrap();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: recipient_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "future-epoch-recipient-publish".to_string(),
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
            operation_id: "future-epoch-owner-rotate".to_string(),
        },
    )
    .unwrap();

    let fetched = fetch_remote(
        &remote,
        FetchOptions {
            password: None,
            repo_root: recipient_root.clone(),
            branch_token: state.branch.token_hex.clone(),
        },
    )
    .unwrap();

    assert!(fetched.downloaded_objects > 0);
    let reopened = e2v_core::testing::unlock_with_local_device_for_test(&recipient_root).unwrap();
    assert_eq!(reopened.branch.token_hex, state.branch.token_hex);
}

#[test]
fn accepted_member_can_fetch_after_historical_rewrite_remote() {
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
            operation_id: "historical-rewrite-bootstrap-seed".to_string(),
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
        FetchOptions {
            password: None,
            repo_root: recipient_root.clone(),
            branch_token: state.branch.token_hex.clone(),
        },
    )
    .unwrap();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: recipient_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "historical-rewrite-recipient-publish".to_string(),
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
            operation_id: "historical-rewrite-owner-rotate".to_string(),
        },
    )
    .unwrap();
    let owner_secrets = e2v_core::sync_support::unlock_repo_secrets_for_sync(
        owner_root.join(".e2v"),
        "correct horse battery staple",
    )
    .unwrap();
    assert!(owner_secrets.epoch_keys.contains_key(&1));
    assert!(owner_secrets.epoch_keys.contains_key(&2));
    let repo_id = e2v_core::sync_support::read_repo_id(&owner_root).unwrap();
    let remote_pointer = remote
        .read_ref(&e2v_store::RefToken::new(format!("keyring/{repo_id}")))
        .unwrap()
        .unwrap();
    let remote_pointer_json: serde_json::Value =
        serde_json::from_slice(&remote_pointer.value.bytes).unwrap();
    let remote_current = remote_pointer_json["current"].as_str().unwrap();
    let remote_keyring_bytes = remote
        .get_physical(&format!("control/keyring/{remote_current}"))
        .unwrap();
    e2v_core::testing::reconcile_remote_keyring_for_test(&owner_root, &remote_keyring_bytes)
        .unwrap();
    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&owner_root.join(".e2v"));
    let reconciled_owner_secrets = e2v_core::sync_support::unlock_repo_secrets_for_sync(
        owner_root.join(".e2v"),
        "correct horse battery staple",
    )
    .unwrap();
    assert!(reconciled_owner_secrets.epoch_keys.contains_key(&1));
    assert!(reconciled_owner_secrets.epoch_keys.contains_key(&2));
    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&owner_root.join(".e2v"));
    historical_rewrite_remote(
        &remote,
        HistoricalRewriteOptions {
            repo_root: owner_root.clone(),
            password: "correct horse battery staple".to_string(),
            confirm_full_reencryption: true,
        },
    )
    .unwrap();

    let fetched = fetch_remote(
        &remote,
        FetchOptions {
            password: None,
            repo_root: recipient_root.clone(),
            branch_token: state.branch.token_hex.clone(),
        },
    )
    .unwrap();

    assert!(fetched.downloaded_objects > 0);
    let reopened = e2v_core::testing::unlock_with_local_device_for_test(&recipient_root).unwrap();
    assert_eq!(reopened.branch.token_hex, state.branch.token_hex);
}

#[test]
fn fetch_prefers_remote_current_device_secrets_when_keyring_pointer_changes() {
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
            operation_id: "fetch-prefers-remote-device-secrets-bootstrap".to_string(),
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
        FetchOptions {
            password: None,
            repo_root: recipient_root.clone(),
            branch_token: state.branch.token_hex.clone(),
        },
    )
    .unwrap();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: recipient_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "fetch-prefers-remote-device-secrets-recipient-publish".to_string(),
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
            operation_id: "fetch-prefers-remote-device-secrets-owner-rotate".to_string(),
        },
    )
    .unwrap();

    historical_rewrite_remote(
        &remote,
        HistoricalRewriteOptions {
            repo_root: owner_root,
            password: "correct horse battery staple".to_string(),
            confirm_full_reencryption: true,
        },
    )
    .unwrap();

    let fetched = fetch_remote(
        &remote,
        FetchOptions {
            password: None,
            repo_root: recipient_root.clone(),
            branch_token: state.branch.token_hex.clone(),
        },
    )
    .unwrap();

    assert!(fetched.downloaded_objects > 0);
    let reopened = e2v_core::testing::unlock_with_local_device_for_test(&recipient_root).unwrap();
    assert_eq!(reopened.branch.token_hex, state.branch.token_hex);
}

#[test]
fn accepted_member_bootstrap_fetch_then_open_supports_clone_equivalent_readiness() {
    let temp = tempdir().unwrap();
    let owner_root = temp.path().join("owner");
    let bootstrap_root = temp.path().join("bootstrap");
    fs::create_dir_all(&owner_root).unwrap();
    fs::create_dir_all(&bootstrap_root).unwrap();

    let facade = RepositoryFacade::new();
    let state = facade
        .init(InitOptions {
            repo_root: owner_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
    fs::write(owner_root.join("hello.txt"), b"clone-shared").unwrap();
    let committed = facade
        .commit(CommitOptions {
            repo_root: owner_root.clone(),
            message: "clone-shared".to_string(),
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
            operation_id: "clone-shared-seed".to_string(),
        },
    )
    .unwrap();

    facade
        .share_accept_member(
            &bootstrap_root,
            e2v_core::ShareAcceptMemberOptions {
                invite_bytes: invite.bundle_bytes,
                local_device_label: "alice-laptop".to_string(),
            },
        )
        .unwrap();
    fetch_remote(
        &remote,
        FetchOptions {
            password: None,
            repo_root: bootstrap_root.clone(),
            branch_token: state.branch.token_hex.clone(),
        },
    )
    .unwrap();

    let reopened = e2v_core::testing::unlock_with_local_device_for_test(&bootstrap_root).unwrap();
    assert_eq!(reopened.branch.token_hex, state.branch.token_hex);
    assert!(
        bootstrap_root
            .join(".e2v")
            .join("objects")
            .join(format!("{}.json", committed.snapshot_id))
            .is_file()
    );
}

#[test]
fn remote_keyring_generation_rollback_via_pointer_ref_is_rejected() {
    let (temp, _facade, source_repo_root, branch_token, remote) = seed_remote();
    let trusted_state_root = temp.path().join("trusted-state");
    fs::create_dir_all(&trusted_state_root).unwrap();
    let _trusted_state_guard =
        e2v_sync::testing::override_trusted_state_dir_for_test(trusted_state_root.clone());

    let repo_id = e2v_core::sync_support::read_repo_id(&source_repo_root).unwrap();
    fs::write(
        trusted_state_root.join(format!("{repo_id}.json")),
        serde_json::to_vec_pretty(&serde_json::json!({
            "repo_id": repo_id,
            "min_layout_generation": 1u64,
            "min_keyring_generation": 2u64,
            "min_ref_generation": 1u64
        }))
        .unwrap(),
    )
    .unwrap();

    let current_pointer = remote
        .get_physical("control/keyring/keyring.current")
        .unwrap();
    remote
        .compare_and_swap_ref(
            &keyring_pointer_ref_token(&source_repo_root),
            None,
            e2v_store::EncryptedRef::new(current_pointer),
        )
        .unwrap();

    let clone_repo_root = temp.path().join("clone-target");
    let error = clone_remote(
        &remote,
        CloneOptions {
            repo_root: clone_repo_root,
            password: "correct horse battery staple".to_string(),
            branch_token,
        },
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("CRITICAL_ROLLBACK_DETECTED")
            || error.to_string().contains("critical rollback detected"),
        "expected rollback detection error, got: {error:#}"
    );
}
