use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use e2v_core::{CommitOptions, InitOptions, RepositoryFacade};
use e2v_store::{
    BackendCapability, BlobStore, CasResult, ConsistencyClass, EncryptedRef, LayoutRoot,
    LayoutRootStore, ListedRef, MemoryBackend, RefStore, RefToken, RefVersion, RemoteBackend,
    StoredRef,
};
use e2v_sync::benchmarking::override_small_object_pack_threshold_for_test;
use e2v_sync::{CloneOptions, FetchOptions, PushOptions, clone_remote, fetch_remote, push_head};
use tempfile::tempdir;

fn main() -> Result<()> {
    let report = run_benchmark()?;
    println!("{report}");
    Ok(())
}

fn run_benchmark() -> Result<String> {
    let _guard = override_small_object_pack_threshold_for_test(1);
    let file_count = bench_env_usize("E2V_P1_C_BENCH_FILE_COUNT", 256)?;
    let max_compaction_versions = bench_env_usize("E2V_P1_C_BENCH_MAX_COMPACTION_VERSIONS", 16)?;
    let temp = tempdir()?;
    let source_repo_root = temp.path().join("source");
    fs::create_dir_all(&source_repo_root)?;

    let facade = RepositoryFacade::new();
    let state = facade.init(InitOptions {
        repo_root: source_repo_root.clone(),
        password: "correct horse battery staple".to_string(),
        branch_name: "main".to_string(),
    })?;

    write_small_files(&source_repo_root, file_count, "v1")?;
    let committed = facade.commit(CommitOptions {
        repo_root: source_repo_root.clone(),
        message: "bench-pack-v1".to_string(),
    })?;

    let remote = MemoryBackend::new();

    let push_start = Instant::now();
    push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: source_repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "bench-pack-v1-op".to_string(),
        },
    )?;
    let packed_push_ms = push_start.elapsed().as_millis();
    let pack_remote_written = !remote.list_physical("packs/data/")?.is_empty()
        && !remote.list_physical("packs/index/")?.is_empty()
        && remote.exists_physical("pack-index/root.json");

    let clone_repo_root = temp.path().join("clone");
    let clone_start = Instant::now();
    let cloned = clone_remote(
        &remote,
        CloneOptions {
            repo_root: clone_repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_token: state.branch.token_hex.clone(),
        },
    )?;
    let packed_clone_ms = clone_start.elapsed().as_millis();
    let clone_head_verified = cloned.head_snapshot_id.as_deref()
        == Some(committed.snapshot_id.as_str())
        && clone_repo_root
            .join(".e2v")
            .join("objects")
            .join(format!("{}.json", committed.snapshot_id))
            .is_file();

    let fetch_target_root = temp.path().join("fetch-target");
    fs::create_dir_all(&fetch_target_root)?;
    RepositoryFacade::new().init(InitOptions {
        repo_root: fetch_target_root.clone(),
        password: "correct horse battery staple".to_string(),
        branch_name: "main".to_string(),
    })?;

    let warmup_start = Instant::now();
    let tracked_remote = RangeReadTrackingRemote::new(remote.clone());
    fetch_remote(
        &tracked_remote,
        FetchOptions {
            repo_root: fetch_target_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            password: Some("correct horse battery staple".to_string()),
        },
    )?;
    let pack_index_warmup_ms = warmup_start.elapsed().as_millis();
    let pack_range_read_paths = tracked_remote.pack_range_read_paths();
    let distinct_pack_paths = pack_range_read_paths
        .iter()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let pack_range_reads = pack_range_read_paths.len();
    let distinct_pack_path_count = distinct_pack_paths.len();
    let pack_range_reuse_verified =
        pack_range_reads > 0 && pack_range_reads <= distinct_pack_path_count + 2;

    let deleted_object_path =
        first_local_object_path(&fetch_target_root.join(".e2v").join("objects"))
            .context("benchmark expected fetch warmup to materialize at least one local object")?;
    fs::remove_file(&deleted_object_path)?;
    let cached_fetch_start = Instant::now();
    let cache_reuse_remote = PackIndexSegmentReadForbiddenRemote::new(remote.clone());
    fetch_remote(
        &cache_reuse_remote,
        FetchOptions {
            repo_root: fetch_target_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            password: Some("correct horse battery staple".to_string()),
        },
    )?;
    let cached_fetch_ms = cached_fetch_start.elapsed().as_millis();
    let cache_reuse_verified = deleted_object_path.is_file();
    let cached_fetch_without_remote_segments = cache_reuse_remote.blocked_segment_reads() == 0;

    let compact_start = Instant::now();
    let mut compaction_triggered = false;
    let mut last_root_segment_count;
    for version in 2..=max_compaction_versions {
        write_small_files(&source_repo_root, file_count, &format!("v{version}"))?;
        facade.commit(CommitOptions {
            repo_root: source_repo_root.clone(),
            message: format!("bench-pack-v{version}"),
        })?;
        push_head(
            &facade,
            &remote,
            PushOptions {
                repo_root: source_repo_root.clone(),
                branch_token: state.branch.token_hex.clone(),
                operation_id: format!("bench-pack-v{version}-op"),
            },
        )?;
        last_root_segment_count = read_root_segment_count(&source_repo_root.join(".e2v"), &remote)?;
        if remote.exists_physical("pack-index/segments/compact-00000000000000000005.json")
            || has_compacted_segments(&remote)?
        {
            compaction_triggered = true;
        }
        if compaction_triggered && last_root_segment_count <= 4 {
            break;
        }
    }
    let compaction_path_ms = compact_start.elapsed().as_millis();

    let segment_count = read_root_segment_count(&source_repo_root.join(".e2v"), &remote)?;

    Ok(format!(
        "p1-c-pack-bench packed_push_ms={packed_push_ms} packed_clone_ms={packed_clone_ms} pack_index_warmup_ms={pack_index_warmup_ms} cached_fetch_ms={cached_fetch_ms} compaction_path_ms={compaction_path_ms} pack_range_reads={pack_range_reads} distinct_pack_paths={distinct_pack_path_count} pack_range_reuse_verified={pack_range_reuse_verified} pack_remote_written={pack_remote_written} clone_head_verified={clone_head_verified} cached_fetch_without_remote_segments={cached_fetch_without_remote_segments} root_segments={segment_count} compaction_triggered={compaction_triggered} cache_reuse_verified={cache_reuse_verified}"
    ))
}

fn write_small_files(repo_root: &std::path::Path, count: usize, version: &str) -> Result<()> {
    for index in 0..count {
        fs::write(
            repo_root.join(format!("bench-{index:04}.txt")),
            format!("{version}-payload-{index:04}"),
        )?;
    }
    Ok(())
}

fn bench_env_usize(name: &str, default: usize) -> Result<usize> {
    match std::env::var(name) {
        Ok(value) => value
            .parse::<usize>()
            .with_context(|| format!("invalid {name} value {value}")),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(error.into()),
    }
}

fn first_local_object_path(objects_dir: &Path) -> Result<std::path::PathBuf> {
    fs::read_dir(objects_dir)?
        .map(|entry| entry.map(|entry| entry.path()))
        .find_map(|entry| entry.ok())
        .context("missing local object file")
}

fn read_root_segment_count(control_dir: &Path, remote: &MemoryBackend) -> Result<usize> {
    let root = e2v_sync::benchmarking::decode_pack_index_root_value_for_test(
        control_dir,
        &remote.get_physical("pack-index/root.json")?,
    )?;
    Ok(root["segments"]
        .as_array()
        .map(|items| items.len())
        .unwrap_or(0))
}

fn has_compacted_segments(remote: &MemoryBackend) -> Result<bool> {
    Ok(!remote.list_physical("pack-index/segments/")?.is_empty())
}

#[derive(Debug, Clone)]
struct RangeReadTrackingRemote {
    inner: MemoryBackend,
    pack_range_read_paths: Arc<Mutex<Vec<String>>>,
    capability: BackendCapability,
}

impl RangeReadTrackingRemote {
    fn new(inner: MemoryBackend) -> Self {
        Self {
            inner,
            pack_range_read_paths: Arc::new(Mutex::new(Vec::new())),
            capability: BackendCapability {
                supports_conditional_put: true,
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
        }
    }

    fn pack_range_read_paths(&self) -> Vec<String> {
        self.pack_range_read_paths.lock().unwrap().clone()
    }
}

impl BlobStore for RangeReadTrackingRemote {
    fn put_physical(&self, relative_path: &str, bytes: &[u8]) -> Result<()> {
        self.inner.put_physical(relative_path, bytes)
    }

    fn put_physical_if_absent(&self, relative_path: &str, bytes: &[u8]) -> Result<bool> {
        self.inner.put_physical_if_absent(relative_path, bytes)
    }

    fn get_physical(&self, relative_path: &str) -> Result<Vec<u8>> {
        self.inner.get_physical(relative_path)
    }

    fn get_physical_range(
        &self,
        relative_path: &str,
        offset: usize,
        length: usize,
    ) -> Result<Vec<u8>> {
        if relative_path.starts_with("packs/data/") {
            self.pack_range_read_paths
                .lock()
                .unwrap()
                .push(relative_path.to_string());
        }
        self.inner.get_physical_range(relative_path, offset, length)
    }

    fn delete_physical(&self, relative_path: &str) -> Result<()> {
        self.inner.delete_physical(relative_path)
    }

    fn exists_physical(&self, relative_path: &str) -> bool {
        self.inner.exists_physical(relative_path)
    }

    fn stat_physical(&self, relative_path: &str) -> Result<e2v_store::ObjectStat> {
        self.inner.stat_physical(relative_path)
    }

    fn list_physical(&self, prefix: &str) -> Result<Vec<String>> {
        self.inner.list_physical(prefix)
    }
}

impl RefStore for RangeReadTrackingRemote {
    fn read_ref(&self, token: &RefToken) -> Result<Option<StoredRef>> {
        self.inner.read_ref(token)
    }

    fn list_refs(&self) -> Result<Vec<ListedRef>> {
        self.inner.list_refs()
    }

    fn compare_and_swap_ref(
        &self,
        token: &RefToken,
        expected: Option<RefVersion>,
        next: EncryptedRef,
    ) -> Result<CasResult> {
        self.inner.compare_and_swap_ref(token, expected, next)
    }
}

impl LayoutRootStore for RangeReadTrackingRemote {
    fn read_layout_root(&self) -> Result<LayoutRoot> {
        self.inner.read_layout_root()
    }

    fn compare_and_swap_layout_root(&self, expected: u64, next: LayoutRoot) -> Result<CasResult> {
        self.inner.compare_and_swap_layout_root(expected, next)
    }

    fn list_retained_layout_roots(&self) -> Result<Vec<LayoutRoot>> {
        self.inner.list_retained_layout_roots()
    }
}

impl RemoteBackend for RangeReadTrackingRemote {
    fn capability(&self) -> &BackendCapability {
        &self.capability
    }
}

#[derive(Debug, Clone)]
struct PackIndexSegmentReadForbiddenRemote {
    inner: MemoryBackend,
    blocked_segment_reads: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    capability: BackendCapability,
}

impl PackIndexSegmentReadForbiddenRemote {
    fn new(inner: MemoryBackend) -> Self {
        Self {
            inner,
            blocked_segment_reads: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            capability: BackendCapability {
                supports_conditional_put: true,
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
        }
    }

    fn blocked_segment_reads(&self) -> usize {
        self.blocked_segment_reads
            .load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl BlobStore for PackIndexSegmentReadForbiddenRemote {
    fn put_physical(&self, relative_path: &str, bytes: &[u8]) -> Result<()> {
        self.inner.put_physical(relative_path, bytes)
    }

    fn put_physical_if_absent(&self, relative_path: &str, bytes: &[u8]) -> Result<bool> {
        self.inner.put_physical_if_absent(relative_path, bytes)
    }

    fn get_physical(&self, relative_path: &str) -> Result<Vec<u8>> {
        if relative_path.starts_with("packs/index/")
            || relative_path.starts_with("pack-index/segments/")
        {
            self.blocked_segment_reads
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            anyhow::bail!("pack index segment reads disabled for benchmark cache verification");
        }
        self.inner.get_physical(relative_path)
    }

    fn get_physical_range(
        &self,
        relative_path: &str,
        offset: usize,
        length: usize,
    ) -> Result<Vec<u8>> {
        self.inner.get_physical_range(relative_path, offset, length)
    }

    fn delete_physical(&self, relative_path: &str) -> Result<()> {
        self.inner.delete_physical(relative_path)
    }

    fn exists_physical(&self, relative_path: &str) -> bool {
        self.inner.exists_physical(relative_path)
    }

    fn stat_physical(&self, relative_path: &str) -> Result<e2v_store::ObjectStat> {
        self.inner.stat_physical(relative_path)
    }

    fn list_physical(&self, prefix: &str) -> Result<Vec<String>> {
        self.inner.list_physical(prefix)
    }
}

impl RefStore for PackIndexSegmentReadForbiddenRemote {
    fn read_ref(&self, token: &RefToken) -> Result<Option<StoredRef>> {
        self.inner.read_ref(token)
    }

    fn list_refs(&self) -> Result<Vec<ListedRef>> {
        self.inner.list_refs()
    }

    fn compare_and_swap_ref(
        &self,
        token: &RefToken,
        expected: Option<RefVersion>,
        next: EncryptedRef,
    ) -> Result<CasResult> {
        self.inner.compare_and_swap_ref(token, expected, next)
    }
}

impl LayoutRootStore for PackIndexSegmentReadForbiddenRemote {
    fn read_layout_root(&self) -> Result<LayoutRoot> {
        self.inner.read_layout_root()
    }

    fn compare_and_swap_layout_root(&self, expected: u64, next: LayoutRoot) -> Result<CasResult> {
        self.inner.compare_and_swap_layout_root(expected, next)
    }

    fn list_retained_layout_roots(&self) -> Result<Vec<LayoutRoot>> {
        self.inner.list_retained_layout_roots()
    }
}

impl RemoteBackend for PackIndexSegmentReadForbiddenRemote {
    fn capability(&self) -> &BackendCapability {
        &self.capability
    }
}
