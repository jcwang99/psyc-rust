use std::collections::HashMap;
use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use blake3::Hasher;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use e2v_core::{
    BranchOverlayChange, BranchWritebackOptions, BranchWritebackOutcome, DirectoryEntry,
    FileHandle, ReadService, RepositoryFacade, SnapshotHandle,
};
use getrandom::fill as getrandom_fill;
use unicode_normalization::UnicodeNormalization;

mod platform;
mod windows;

use crate::platform::{
    try_mount_live_branch_on_current_platform, try_mount_snapshot_on_current_platform,
};

const DEFAULT_PLAINTEXT_MEMORY_CACHE_BUDGET_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CachePolicy {
    KernelCacheWithInvalidation,
    DirectIoFallback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VfsSemantic {
    ByteRangeLocks,
    MemoryMappedWrites,
    WritableHandles,
    WritebackCaching,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VfsNodeKind {
    File,
    Directory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlatformCapabilities {
    pub supports_directory_entry_invalidation: bool,
    pub supports_inode_attribute_invalidation: bool,
    pub supports_page_cache_invalidation: bool,
}

impl PlatformCapabilities {
    pub fn no_reliable_invalidation() -> Self {
        Self {
            supports_directory_entry_invalidation: false,
            supports_inode_attribute_invalidation: false,
            supports_page_cache_invalidation: false,
        }
    }

    pub fn reliable_invalidation() -> Self {
        Self {
            supports_directory_entry_invalidation: true,
            supports_inode_attribute_invalidation: true,
            supports_page_cache_invalidation: true,
        }
    }

    pub fn without_page_cache_invalidation(mut self) -> Self {
        self.supports_page_cache_invalidation = false;
        self
    }

    pub fn without_directory_entry_invalidation(mut self) -> Self {
        self.supports_directory_entry_invalidation = false;
        self
    }

    pub fn without_inode_attribute_invalidation(mut self) -> Self {
        self.supports_inode_attribute_invalidation = false;
        self
    }

    fn supports_live_branch_kernel_cache(self) -> bool {
        self.supports_directory_entry_invalidation
            && self.supports_inode_attribute_invalidation
            && self.supports_page_cache_invalidation
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VfsMountConfig {
    repo_root: PathBuf,
    mode: MountMode,
    platform_capabilities: PlatformCapabilities,
    encrypted_disk_cache_dir: Option<PathBuf>,
    plaintext_memory_cache_budget_bytes: usize,
}

impl VfsMountConfig {
    pub fn snapshot(repo_root: PathBuf, snapshot_id: String) -> Self {
        Self {
            repo_root,
            mode: MountMode::SnapshotPinned { snapshot_id },
            platform_capabilities: PlatformCapabilities::reliable_invalidation(),
            encrypted_disk_cache_dir: None,
            plaintext_memory_cache_budget_bytes: DEFAULT_PLAINTEXT_MEMORY_CACHE_BUDGET_BYTES,
        }
    }

    pub fn live_branch(repo_root: PathBuf, branch_token_hex: String) -> Self {
        Self {
            repo_root,
            mode: MountMode::LiveBranch { branch_token_hex },
            platform_capabilities: PlatformCapabilities::reliable_invalidation(),
            encrypted_disk_cache_dir: None,
            plaintext_memory_cache_budget_bytes: DEFAULT_PLAINTEXT_MEMORY_CACHE_BUDGET_BYTES,
        }
    }

    pub fn with_platform_capabilities(
        mut self,
        platform_capabilities: PlatformCapabilities,
    ) -> Self {
        self.platform_capabilities = platform_capabilities;
        self
    }

    pub fn with_encrypted_disk_cache_dir(mut self, encrypted_disk_cache_dir: PathBuf) -> Self {
        self.encrypted_disk_cache_dir = Some(encrypted_disk_cache_dir);
        self
    }

    pub fn with_plaintext_memory_cache_budget_bytes(
        mut self,
        plaintext_memory_cache_budget_bytes: usize,
    ) -> Self {
        self.plaintext_memory_cache_budget_bytes = plaintext_memory_cache_budget_bytes;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MountMode {
    SnapshotPinned { snapshot_id: String },
    LiveBranch { branch_token_hex: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MountRequest {
    config: VfsMountConfig,
    mount_point: PathBuf,
}

impl MountRequest {
    pub(crate) fn from_config(config: VfsMountConfig, mount_point: PathBuf) -> Self {
        Self {
            config,
            mount_point,
        }
    }

    pub(crate) fn snapshot(repo_root: PathBuf, snapshot_id: String, mount_point: PathBuf) -> Self {
        Self::from_config(
            VfsMountConfig::snapshot(repo_root, snapshot_id),
            mount_point,
        )
    }

    pub(crate) fn live_branch(
        repo_root: PathBuf,
        branch_token_hex: String,
        mount_point: PathBuf,
    ) -> Self {
        Self::from_config(
            VfsMountConfig::live_branch(repo_root, branch_token_hex),
            mount_point,
        )
    }

    pub(crate) fn mount_point(&self) -> &PathBuf {
        &self.mount_point
    }

    pub(crate) fn mount_mode_label(&self) -> &'static str {
        match self.config.mode {
            MountMode::SnapshotPinned { .. } => "snapshot-pinned",
            MountMode::LiveBranch { .. } => "live-branch",
        }
    }
}

#[derive(Debug, Clone)]
pub struct OpenedFile {
    inode_id: u64,
    logical_path: String,
    plaintext_cache: Arc<Mutex<Option<CachedRange>>>,
    overlay_bytes: Option<Arc<Vec<u8>>>,
    backing: OpenedFileBacking,
}

#[derive(Debug, Clone)]
enum OpenedFileBacking {
    Snapshot(FileHandle),
    Overlay {
        snapshot_id: String,
        layout_generation: u64,
        file_object_id: String,
        file_size: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshOutcome {
    pub namespace_changed: bool,
    pub requires_invalidation: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VfsNodeMetadata {
    pub inode_id: u64,
    pub logical_path: String,
    pub kind: VfsNodeKind,
    pub size_bytes: u64,
    pub snapshot_id: String,
    pub layout_generation: u64,
}

#[derive(Debug, Clone)]
pub struct ReadOnlyVfs {
    mode: MountMode,
    cache_policy: CachePolicy,
    plaintext_cache: Arc<Mutex<PlaintextCache>>,
    plaintext_memory_cache_budget_bytes: usize,
    encrypted_range_cache: Option<EncryptedRangeCache>,
    read_service: ReadService,
    namespace_snapshot: SnapshotHandle,
}

#[derive(Debug, Clone, Default)]
struct WritableOverlay {
    created_directories: Vec<String>,
    upserted_files: HashMap<String, Vec<u8>>,
}

impl WritableOverlay {
    fn as_changes(&self) -> Vec<BranchOverlayChange> {
        let mut changes = self
            .created_directories
            .iter()
            .cloned()
            .map(|path| BranchOverlayChange::CreateDirectory { path })
            .collect::<Vec<_>>();
        let mut files = self
            .upserted_files
            .iter()
            .map(|(path, bytes)| BranchOverlayChange::UpsertFile {
                path: path.clone(),
                bytes: bytes.clone(),
            })
            .collect::<Vec<_>>();
        changes.append(&mut files);
        changes
    }

    fn created_directories_under(&self, parent: &str) -> Vec<String> {
        self.created_directories
            .iter()
            .filter_map(|path| immediate_child_name(parent, path))
            .collect()
    }

    fn upserted_files_under(&self, parent: &str) -> Vec<String> {
        self.upserted_files
            .keys()
            .filter_map(|path| immediate_child_name(parent, path))
            .collect()
    }
}

fn lock_or_recover<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Debug, Clone)]
pub struct WritableVfs {
    config: VfsMountConfig,
    inner: ReadOnlyVfs,
    overlay: WritableOverlay,
}

type PlaintextCacheKey = (String, String, u64);

#[derive(Debug)]
struct PlaintextCache {
    budget_bytes: usize,
    total_bytes: usize,
    next_access_order: u64,
    entries: HashMap<PlaintextCacheKey, PlaintextCacheEntry>,
}

#[derive(Debug, Clone)]
struct PlaintextCacheEntry {
    bytes: Vec<u8>,
    last_access_order: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CachedRange {
    offset: usize,
    bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountLaunchState {
    SummaryOnly,
    HostActive,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountLaunchSummary {
    pub mount_mode: String,
    pub mount_point: PathBuf,
    pub cache_policy: CachePolicy,
    pub read_only: bool,
    pub stream_only: bool,
    pub launch_state: MountLaunchState,
    pub status_message: String,
}

#[derive(Debug)]
pub struct MountedFilesystem {
    summary: MountLaunchSummary,
    #[cfg(windows)]
    _windows_host: Option<crate::windows::WindowsMountedFilesystemHost>,
}

impl MountedFilesystem {
    pub fn summary(&self) -> &MountLaunchSummary {
        &self.summary
    }

    #[cfg(not(windows))]
    pub(crate) fn from_summary(summary: MountLaunchSummary) -> Self {
        Self {
            summary,
            #[cfg(windows)]
            _windows_host: None,
        }
    }

    #[cfg(windows)]
    pub(crate) fn with_windows_host(
        summary: MountLaunchSummary,
        windows_host: crate::windows::WindowsMountedFilesystemHost,
    ) -> Self {
        Self {
            summary,
            _windows_host: Some(windows_host),
        }
    }
}

impl ReadOnlyVfs {
    pub fn mount_snapshot(config: VfsMountConfig) -> Result<Self> {
        match config.mode {
            MountMode::SnapshotPinned { .. } => Self::mount(config),
            MountMode::LiveBranch { .. } => {
                anyhow::bail!("snapshot mounts require snapshot mode configuration")
            }
        }
    }

    pub fn mount_live_branch(config: VfsMountConfig) -> Result<Self> {
        match config.mode {
            MountMode::LiveBranch { .. } => Self::mount(config),
            MountMode::SnapshotPinned { .. } => {
                anyhow::bail!("live branch mounts require live branch mode configuration")
            }
        }
    }

    fn mount(config: VfsMountConfig) -> Result<Self> {
        let mode = config.mode.clone();
        let read_service = ReadService::new(&config.repo_root);
        let encrypted_range_cache = config
            .encrypted_disk_cache_dir
            .as_ref()
            .map(|cache_dir| EncryptedRangeCache::new(cache_dir.clone(), &read_service))
            .transpose()?;
        let cache_policy = match (
            &mode,
            config
                .platform_capabilities
                .supports_live_branch_kernel_cache(),
        ) {
            (MountMode::LiveBranch { .. }, false) => CachePolicy::DirectIoFallback,
            _ => CachePolicy::KernelCacheWithInvalidation,
        };
        let namespace_snapshot = match config.mode {
            MountMode::LiveBranch { branch_token_hex } => {
                read_service.resolve_branch(&branch_token_hex)?
            }
            MountMode::SnapshotPinned { snapshot_id } => {
                read_service.open_snapshot(&snapshot_id)?
            }
        };
        Ok(Self {
            mode,
            cache_policy,
            plaintext_cache: Arc::new(Mutex::new(PlaintextCache::new(
                config.plaintext_memory_cache_budget_bytes,
            ))),
            plaintext_memory_cache_budget_bytes: config.plaintext_memory_cache_budget_bytes,
            encrypted_range_cache,
            read_service,
            namespace_snapshot,
        })
    }

    pub fn namespace_snapshot_id(&self) -> String {
        self.namespace_snapshot.snapshot_id.clone()
    }

    pub fn namespace_layout_generation(&self) -> u64 {
        self.namespace_snapshot.layout_generation
    }

    pub fn open_file(&self, path: &str) -> Result<OpenedFile> {
        let normalized = normalize_vfs_path(path);
        let file = self
            .read_service
            .open_file(&self.namespace_snapshot, &normalized)?;
        Ok(OpenedFile {
            inode_id: stable_inode_id(&file.snapshot_id, &normalized, &file.file_object_id),
            logical_path: normalized,
            plaintext_cache: Arc::new(Mutex::new(None)),
            overlay_bytes: None,
            backing: OpenedFileBacking::Snapshot(file),
        })
    }

    pub fn read_dir(&self, path: &str) -> Result<Vec<DirectoryEntry>> {
        let normalized = normalize_vfs_path(path);
        self.read_service
            .read_dir(&self.namespace_snapshot, &normalized)
    }

    pub fn stat_path(&self, path: &str) -> Result<VfsNodeMetadata> {
        let normalized = normalize_vfs_path(path);
        if normalized.is_empty() {
            return Ok(VfsNodeMetadata {
                inode_id: stable_directory_inode_id(&self.namespace_snapshot.snapshot_id, ""),
                logical_path: String::new(),
                kind: VfsNodeKind::Directory,
                size_bytes: 0,
                snapshot_id: self.namespace_snapshot.snapshot_id.clone(),
                layout_generation: self.namespace_snapshot.layout_generation,
            });
        }

        if let Ok(file) = self.open_file(&normalized) {
            return Ok(VfsNodeMetadata {
                inode_id: file.inode_id(),
                logical_path: normalized.to_string(),
                kind: VfsNodeKind::File,
                size_bytes: file.file_size(),
                snapshot_id: file.snapshot_id().to_string(),
                layout_generation: file.layout_generation(),
            });
        }

        self.read_dir(&normalized)?;
        Ok(VfsNodeMetadata {
            inode_id: stable_directory_inode_id(&self.namespace_snapshot.snapshot_id, &normalized),
            logical_path: normalized.to_string(),
            kind: VfsNodeKind::Directory,
            size_bytes: 0,
            snapshot_id: self.namespace_snapshot.snapshot_id.clone(),
            layout_generation: self.namespace_snapshot.layout_generation,
        })
    }

    pub fn cache_policy(&self) -> CachePolicy {
        self.cache_policy
    }

    pub fn read(&self, opened_file: &OpenedFile, offset: usize, length: usize) -> Result<Vec<u8>> {
        if let Some(overlay_bytes) = &opened_file.overlay_bytes {
            anyhow::ensure!(offset <= overlay_bytes.len(), "range offset out of bounds");
            let end = offset.saturating_add(length).min(overlay_bytes.len());
            return Ok(overlay_bytes[offset..end].to_vec());
        }
        let cache_key = (
            opened_file.snapshot_id().to_string(),
            opened_file.file_object_id().to_string(),
            opened_file.layout_generation(),
        );
        let file_size = opened_file.file_size() as usize;
        anyhow::ensure!(offset <= file_size, "range offset out of bounds");

        if let Some(cached) = opened_file.cached_range_bytes(offset, length) {
            return Ok(cached);
        }

        if let Some(requested) = self
            .plaintext_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get_range(&cache_key, offset, length)
        {
            self.replace_open_file_cache(
                opened_file,
                self.cacheable_plaintext_range(offset, requested.clone()),
            );
            return Ok(requested);
        }

        if let Some(cache) = &self.encrypted_range_cache
            && let Some(cached) = cache.read_range(
                opened_file.snapshot_id(),
                opened_file.file_object_id(),
                opened_file.layout_generation(),
                offset,
                length,
            )?
        {
            self.replace_open_file_cache(
                opened_file,
                Some(CachedRange {
                    offset,
                    bytes: cached.clone(),
                }),
            );
            return Ok(cached);
        }

        let OpenedFileBacking::Snapshot(file) = &opened_file.backing else {
            anyhow::bail!("overlay-backed files must be read from overlay bytes");
        };
        let bytes = self.read_service.read_range(file, offset, length)?;
        if let Some(cache) = &self.encrypted_range_cache {
            let _ = cache.write_range(
                opened_file.snapshot_id(),
                opened_file.file_object_id(),
                opened_file.layout_generation(),
                offset,
                &bytes,
            );
        }
        if offset == 0 && offset.saturating_add(bytes.len()) >= file_size {
            let cacheable = self.cacheable_plaintext_range(0, bytes.clone());
            self.replace_open_file_cache(opened_file, cacheable.clone());
            if let Some(cacheable) = cacheable {
                self.plaintext_cache
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(cache_key, cacheable.bytes);
            }
        } else {
            self.replace_open_file_cache(
                opened_file,
                self.cacheable_plaintext_range(offset, bytes.clone()),
            );
        }
        Ok(bytes)
    }

    pub fn refresh_live_branch(&mut self) -> Result<RefreshOutcome> {
        let MountMode::LiveBranch { branch_token_hex } = &self.mode else {
            return Ok(RefreshOutcome {
                namespace_changed: false,
                requires_invalidation: false,
            });
        };
        let refreshed = self.read_service.resolve_branch(branch_token_hex)?;
        let namespace_changed = refreshed.snapshot_id != self.namespace_snapshot.snapshot_id;
        if namespace_changed {
            self.namespace_snapshot = refreshed;
        }
        Ok(RefreshOutcome {
            namespace_changed,
            requires_invalidation: namespace_changed
                && self.cache_policy == CachePolicy::KernelCacheWithInvalidation,
        })
    }

    pub fn require_semantic(&self, semantic: VfsSemantic) -> Result<()> {
        match semantic {
            VfsSemantic::ByteRangeLocks => {
                anyhow::bail!("unsupported VFS semantic: byte-range locks")
            }
            VfsSemantic::MemoryMappedWrites => {
                anyhow::bail!("unsupported VFS semantic: memory-mapped writes")
            }
            VfsSemantic::WritableHandles => {
                anyhow::bail!("unsupported VFS semantic: writable handles")
            }
            VfsSemantic::WritebackCaching => {
                anyhow::bail!("unsupported VFS semantic: writeback caching")
            }
        }
    }

    fn cacheable_plaintext_range(&self, offset: usize, bytes: Vec<u8>) -> Option<CachedRange> {
        (bytes.len() <= self.plaintext_memory_cache_budget_bytes)
            .then_some(CachedRange { offset, bytes })
    }

    fn replace_open_file_cache(&self, opened_file: &OpenedFile, cached_range: Option<CachedRange>) {
        *lock_or_recover(&opened_file.plaintext_cache) = cached_range;
    }
}

impl WritableVfs {
    pub fn mount_live_branch(config: VfsMountConfig) -> Result<Self> {
        let inner = ReadOnlyVfs::mount_live_branch(config.clone())?;
        Ok(Self {
            config,
            inner,
            overlay: WritableOverlay::default(),
        })
    }

    pub fn namespace_snapshot_id(&self) -> String {
        self.inner.namespace_snapshot_id()
    }

    pub fn cache_policy(&self) -> CachePolicy {
        self.inner.cache_policy()
    }

    pub fn namespace_layout_generation(&self) -> u64 {
        self.inner.namespace_layout_generation()
    }

    pub fn require_semantic(&self, semantic: VfsSemantic) -> Result<()> {
        match semantic {
            VfsSemantic::WritableHandles => Ok(()),
            VfsSemantic::ByteRangeLocks => {
                anyhow::bail!("unsupported VFS semantic: byte-range locks")
            }
            VfsSemantic::MemoryMappedWrites => {
                anyhow::bail!("unsupported VFS semantic: memory-mapped writes")
            }
            VfsSemantic::WritebackCaching => {
                anyhow::bail!("unsupported VFS semantic: writeback caching")
            }
        }
    }

    pub fn open_file(&self, path: &str) -> Result<OpenedFile> {
        let normalized = normalize_vfs_path(path);
        if let Some(bytes) = self.overlay.upserted_files.get(&normalized) {
            return Ok(OpenedFile {
                inode_id: stable_inode_id(
                    &self.inner.namespace_snapshot.snapshot_id,
                    &normalized,
                    &format!("overlay:{normalized}"),
                ),
                logical_path: normalized.clone(),
                plaintext_cache: Arc::new(Mutex::new(None)),
                overlay_bytes: Some(Arc::new(bytes.clone())),
                backing: OpenedFileBacking::Overlay {
                    snapshot_id: self.inner.namespace_snapshot.snapshot_id.clone(),
                    layout_generation: self.inner.namespace_snapshot.layout_generation,
                    file_object_id: format!("overlay:{normalized}"),
                    file_size: bytes.len() as u64,
                },
            });
        }
        self.inner.open_file(&normalized)
    }

    pub fn read(&self, opened_file: &OpenedFile, offset: usize, length: usize) -> Result<Vec<u8>> {
        self.inner.read(opened_file, offset, length)
    }

    pub fn stat_path(&self, path: &str) -> Result<VfsNodeMetadata> {
        let normalized = normalize_vfs_path(path);
        if normalized.is_empty() {
            return Ok(VfsNodeMetadata {
                inode_id: stable_directory_inode_id(&self.inner.namespace_snapshot.snapshot_id, ""),
                logical_path: String::new(),
                kind: VfsNodeKind::Directory,
                size_bytes: 0,
                snapshot_id: self.inner.namespace_snapshot.snapshot_id.clone(),
                layout_generation: self.inner.namespace_snapshot.layout_generation,
            });
        }
        if let Some(bytes) = self.overlay.upserted_files.get(&normalized) {
            return Ok(VfsNodeMetadata {
                inode_id: stable_inode_id(
                    &self.inner.namespace_snapshot.snapshot_id,
                    &normalized,
                    &format!("overlay:{normalized}"),
                ),
                logical_path: normalized,
                kind: VfsNodeKind::File,
                size_bytes: bytes.len() as u64,
                snapshot_id: self.inner.namespace_snapshot.snapshot_id.clone(),
                layout_generation: self.inner.namespace_snapshot.layout_generation,
            });
        }
        if self.overlay_directory_exists(&normalized) {
            return Ok(VfsNodeMetadata {
                inode_id: stable_directory_inode_id(
                    &self.inner.namespace_snapshot.snapshot_id,
                    &normalized,
                ),
                logical_path: normalized,
                kind: VfsNodeKind::Directory,
                size_bytes: 0,
                snapshot_id: self.inner.namespace_snapshot.snapshot_id.clone(),
                layout_generation: self.inner.namespace_snapshot.layout_generation,
            });
        }
        self.inner.stat_path(path)
    }

    pub fn read_dir(&self, path: &str) -> Result<Vec<DirectoryEntry>> {
        let normalized = normalize_vfs_path(path);
        let mut entries = match self.inner.read_dir(&normalized) {
            Ok(entries) => entries,
            Err(error) if self.overlay_directory_exists(&normalized) => {
                let _ = error;
                Vec::new()
            }
            Err(error) => return Err(error),
        };

        let mut by_name = entries
            .drain(..)
            .map(|entry| (entry.name.clone(), entry))
            .collect::<HashMap<_, _>>();
        for directory in self.overlay.created_directories_under(&normalized) {
            by_name.insert(
                directory.clone(),
                DirectoryEntry {
                    name: directory,
                    kind: "tree".to_string(),
                },
            );
        }
        for file in self.overlay.upserted_files_under(&normalized) {
            by_name.insert(
                file.clone(),
                DirectoryEntry {
                    name: file,
                    kind: "file".to_string(),
                },
            );
        }

        let mut entries = by_name.into_values().collect::<Vec<_>>();
        entries.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(entries)
    }

    pub fn create_directory(&mut self, path: &str) -> Result<()> {
        let normalized = normalize_vfs_path(path);
        anyhow::ensure!(!normalized.is_empty(), "directory path must not be empty");
        let parent = parent_vfs_path(&normalized);
        anyhow::ensure!(
            self.directory_exists(&parent),
            "parent directory does not exist: {parent}"
        );
        if let Ok(metadata) = self.inner.stat_path(&normalized) {
            anyhow::ensure!(
                metadata.kind == VfsNodeKind::Directory,
                "path already exists as file: {normalized}"
            );
            return Ok(());
        }
        if self.overlay.upserted_files.contains_key(&normalized) {
            anyhow::bail!("path already exists as file: {normalized}");
        }
        if !self.overlay.created_directories.contains(&normalized) {
            self.overlay.created_directories.push(normalized);
        }
        Ok(())
    }

    pub fn write_file(&mut self, path: &str, bytes: Vec<u8>) -> Result<()> {
        let normalized = normalize_vfs_path(path);
        self.write_file_normalized(normalized, bytes)
    }

    fn write_file_normalized(&mut self, normalized: String, bytes: Vec<u8>) -> Result<()> {
        anyhow::ensure!(!normalized.is_empty(), "file path must not be empty");
        let parent = parent_vfs_path(&normalized);
        anyhow::ensure!(
            self.directory_exists(&parent),
            "parent directory does not exist: {parent}"
        );
        if let Ok(metadata) = self.inner.stat_path(&normalized) {
            anyhow::ensure!(
                metadata.kind != VfsNodeKind::Directory,
                "path already exists as directory: {normalized}"
            );
        }
        if self.overlay_directory_exists(&normalized) {
            anyhow::bail!("path already exists as directory: {normalized}");
        }
        self.overlay.upserted_files.insert(normalized, bytes);
        Ok(())
    }

    pub(crate) fn take_overlay_file_bytes(&mut self, path: &str) -> Option<Vec<u8>> {
        self.overlay.upserted_files.remove(path)
    }

    pub fn writeback(&mut self, message: &str) -> Result<RefreshOutcome> {
        self.writeback_with_changes(Vec::new(), message)
    }

    pub fn create_directory_and_writeback(
        &mut self,
        path: &str,
        message: &str,
    ) -> Result<RefreshOutcome> {
        self.create_directory(path)?;
        self.writeback(message)
    }

    pub fn delete_file_and_writeback(
        &mut self,
        path: &str,
        message: &str,
    ) -> Result<RefreshOutcome> {
        self.writeback_with_changes(
            vec![BranchOverlayChange::DeleteFile {
                path: normalize_vfs_path(path),
            }],
            message,
        )
    }

    pub fn delete_directory_and_writeback(
        &mut self,
        path: &str,
        message: &str,
    ) -> Result<RefreshOutcome> {
        self.writeback_with_changes(
            vec![BranchOverlayChange::DeleteDirectory {
                path: normalize_vfs_path(path),
            }],
            message,
        )
    }

    pub fn rename_and_writeback(
        &mut self,
        from: &str,
        to: &str,
        message: &str,
    ) -> Result<RefreshOutcome> {
        self.writeback_with_changes(
            vec![BranchOverlayChange::Rename {
                from: normalize_vfs_path(from),
                to: normalize_vfs_path(to),
            }],
            message,
        )
    }

    fn writeback_with_changes(
        &mut self,
        mut extra_changes: Vec<BranchOverlayChange>,
        message: &str,
    ) -> Result<RefreshOutcome> {
        let MountMode::LiveBranch { branch_token_hex } = &self.config.mode else {
            anyhow::bail!("writable VFS requires a live branch mount");
        };
        let facade = RepositoryFacade::new();
        let expected_head_snapshot_id = Some(self.inner.namespace_snapshot.snapshot_id.clone());
        let mut changes = self.overlay.as_changes();
        changes.append(&mut extra_changes);
        let outcome = facade.write_branch_overlay(BranchWritebackOptions {
            repo_root: self.config.repo_root.clone(),
            ref_token_hex: branch_token_hex.clone(),
            expected_head_snapshot_id,
            message: message.to_string(),
            changes,
        })?;

        match outcome {
            BranchWritebackOutcome::Noop(_) => {
                return Ok(RefreshOutcome {
                    namespace_changed: false,
                    requires_invalidation: false,
                });
            }
            BranchWritebackOutcome::Committed(_) => {}
            BranchWritebackOutcome::Conflicted(conflict) => {
                anyhow::bail!(
                    "branch overlay writeback reported a conflict: {}",
                    conflict
                        .conflicts
                        .iter()
                        .map(|entry| entry.path.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
        let refreshed = self
            .inner
            .read_service
            .resolve_branch(branch_token_hex.as_str())?;
        let namespace_changed = refreshed.snapshot_id != self.inner.namespace_snapshot.snapshot_id;
        self.inner.namespace_snapshot = refreshed;
        self.overlay = WritableOverlay::default();
        *lock_or_recover(&self.inner.plaintext_cache) =
            PlaintextCache::new(self.inner.plaintext_memory_cache_budget_bytes);
        Ok(RefreshOutcome {
            namespace_changed,
            requires_invalidation: namespace_changed
                && self.inner.cache_policy == CachePolicy::KernelCacheWithInvalidation,
        })
    }

    pub fn refresh_live_branch(&mut self) -> Result<RefreshOutcome> {
        let MountMode::LiveBranch { branch_token_hex } = &self.config.mode else {
            return Ok(RefreshOutcome {
                namespace_changed: false,
                requires_invalidation: false,
            });
        };
        let refreshed = self
            .inner
            .read_service
            .resolve_branch(branch_token_hex.as_str())?;
        let namespace_changed = refreshed.snapshot_id != self.inner.namespace_snapshot.snapshot_id;
        if namespace_changed {
            self.inner.namespace_snapshot = refreshed;
        }
        Ok(RefreshOutcome {
            namespace_changed,
            requires_invalidation: namespace_changed
                && self.inner.cache_policy == CachePolicy::KernelCacheWithInvalidation,
        })
    }

    fn directory_exists(&self, path: &str) -> bool {
        path.is_empty()
            || self.overlay_directory_exists(path)
            || matches!(
                self.inner.stat_path(path),
                Ok(VfsNodeMetadata {
                    kind: VfsNodeKind::Directory,
                    ..
                })
            )
    }

    fn overlay_directory_exists(&self, path: &str) -> bool {
        self.overlay
            .created_directories
            .iter()
            .any(|value| value == path)
            || self
                .overlay
                .created_directories
                .iter()
                .any(|value| value.starts_with(&format!("{path}/")))
            || self
                .overlay
                .upserted_files
                .keys()
                .any(|value| parent_vfs_path(value) == path)
    }
}

#[derive(Debug, Clone)]
struct EncryptedRangeCache {
    cache_dir: PathBuf,
    cipher_key: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RangeIndexEntry {
    offset: usize,
    length: usize,
}

impl EncryptedRangeCache {
    fn new(cache_dir: PathBuf, read_service: &ReadService) -> Result<Self> {
        fs::create_dir_all(&cache_dir)?;
        let cipher_key = read_service.derive_vfs_cache_key()?;
        Ok(Self {
            cache_dir,
            cipher_key,
        })
    }

    fn read_range(
        &self,
        snapshot_id: &str,
        file_object_id: &str,
        layout_generation: u64,
        offset: usize,
        length: usize,
    ) -> Result<Option<Vec<u8>>> {
        let cache_path = self.cache_path(
            snapshot_id,
            file_object_id,
            layout_generation,
            offset,
            length,
        );
        let requested_entry = RangeIndexEntry { offset, length };
        let bytes = match fs::read(&cache_path) {
            Ok(bytes) => bytes,
            Err(error) if Self::should_treat_as_cache_miss(&error) => {
                self.best_effort_prune_range_entries(
                    snapshot_id,
                    file_object_id,
                    layout_generation,
                    std::slice::from_ref(&requested_entry),
                    None,
                );
                return self.read_covering_range(
                    snapshot_id,
                    file_object_id,
                    layout_generation,
                    offset,
                    length,
                );
            }
            Err(error) => return Err(error.into()),
        };
        let (cached_offset, plaintext) = match self.decrypt_blob(&bytes) {
            Ok(decoded) => decoded,
            Err(_) => {
                self.best_effort_prune_range_entries(
                    snapshot_id,
                    file_object_id,
                    layout_generation,
                    std::slice::from_ref(&requested_entry),
                    None,
                );
                return self.read_covering_range(
                    snapshot_id,
                    file_object_id,
                    layout_generation,
                    offset,
                    length,
                );
            }
        };
        if cached_offset != offset || plaintext.len() != length {
            self.best_effort_prune_range_entries(
                snapshot_id,
                file_object_id,
                layout_generation,
                std::slice::from_ref(&requested_entry),
                None,
            );
            return self.read_covering_range(
                snapshot_id,
                file_object_id,
                layout_generation,
                offset,
                length,
            );
        }
        Ok(Some(plaintext))
    }

    fn should_treat_as_cache_miss(error: &std::io::Error) -> bool {
        matches!(
            error.kind(),
            ErrorKind::NotFound
                | ErrorKind::PermissionDenied
                | ErrorKind::InvalidData
                | ErrorKind::IsADirectory
        )
    }

    fn write_range(
        &self,
        snapshot_id: &str,
        file_object_id: &str,
        layout_generation: u64,
        offset: usize,
        plaintext: &[u8],
    ) -> Result<()> {
        let cache_path = self.cache_path(
            snapshot_id,
            file_object_id,
            layout_generation,
            offset,
            plaintext.len(),
        );
        let ciphertext = self.encrypt_blob(offset, plaintext)?;
        if let Some(parent) = cache_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(cache_path, ciphertext)?;
        let _ = self.record_range_index_entry(
            snapshot_id,
            file_object_id,
            layout_generation,
            RangeIndexEntry {
                offset,
                length: plaintext.len(),
            },
        );
        Ok(())
    }

    fn cache_path(
        &self,
        snapshot_id: &str,
        file_object_id: &str,
        layout_generation: u64,
        offset: usize,
        length: usize,
    ) -> PathBuf {
        let prefix = self.cache_entry_key(
            snapshot_id,
            file_object_id,
            layout_generation,
            offset,
            length,
        );
        self.cache_dir.join(format!("{prefix}.bin"))
    }

    fn cache_key_prefix(
        &self,
        snapshot_id: &str,
        file_object_id: &str,
        layout_generation: u64,
    ) -> String {
        let mut hasher = Hasher::new();
        hasher.update(snapshot_id.as_bytes());
        hasher.update(&[0]);
        hasher.update(file_object_id.as_bytes());
        hasher.update(&[0]);
        hasher.update(&layout_generation.to_le_bytes());
        hex::encode(hasher.finalize().as_bytes())
    }

    fn cache_entry_key(
        &self,
        snapshot_id: &str,
        file_object_id: &str,
        layout_generation: u64,
        offset: usize,
        length: usize,
    ) -> String {
        let prefix = self.cache_key_prefix(snapshot_id, file_object_id, layout_generation);
        let mut hasher = Hasher::new();
        hasher.update(prefix.as_bytes());
        hasher.update(&offset.to_le_bytes());
        hasher.update(&length.to_le_bytes());
        hex::encode(hasher.finalize().as_bytes())
    }

    fn read_covering_range(
        &self,
        snapshot_id: &str,
        file_object_id: &str,
        layout_generation: u64,
        offset: usize,
        length: usize,
    ) -> Result<Option<Vec<u8>>> {
        let mut candidates =
            self.range_index_entries(snapshot_id, file_object_id, layout_generation)?;
        let mut stale_entries = Vec::new();
        for candidate in candidates.iter().cloned() {
            let path = self.cache_path(
                snapshot_id,
                file_object_id,
                layout_generation,
                candidate.offset,
                candidate.length,
            );
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) if Self::should_treat_as_cache_miss(&error) => {
                    stale_entries.push(candidate);
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            let (cached_offset, plaintext) = match self.decrypt_blob(&bytes) {
                Ok(decoded) => decoded,
                Err(_) => {
                    stale_entries.push(candidate);
                    continue;
                }
            };
            let cached_length = plaintext.len();
            if cached_offset != candidate.offset || cached_length != candidate.length {
                stale_entries.push(candidate);
                continue;
            }
            let cached_end = cached_offset.saturating_add(cached_length);
            let requested_end = offset.saturating_add(length);
            if cached_offset > offset || cached_end < requested_end {
                continue;
            }
            if !stale_entries.is_empty() {
                candidates.retain(|entry| !stale_entries.contains(entry));
                self.best_effort_prune_range_entries(
                    snapshot_id,
                    file_object_id,
                    layout_generation,
                    &stale_entries,
                    Some(&candidates),
                );
            }
            let start = offset - cached_offset;
            let end = start + length;
            return Ok(Some(plaintext[start..end].to_vec()));
        }
        if !stale_entries.is_empty() {
            candidates.retain(|entry| !stale_entries.contains(entry));
            self.best_effort_prune_range_entries(
                snapshot_id,
                file_object_id,
                layout_generation,
                &stale_entries,
                Some(&candidates),
            );
        }
        Ok(None)
    }

    fn range_index_path(
        &self,
        snapshot_id: &str,
        file_object_id: &str,
        layout_generation: u64,
    ) -> PathBuf {
        let prefix = self.cache_key_prefix(snapshot_id, file_object_id, layout_generation);
        self.cache_dir.join(format!("{prefix}.ranges"))
    }

    fn range_index_entries(
        &self,
        snapshot_id: &str,
        file_object_id: &str,
        layout_generation: u64,
    ) -> Result<Vec<RangeIndexEntry>> {
        let path = self.range_index_path(snapshot_id, file_object_id, layout_generation);
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if Self::should_treat_as_cache_miss(&error) => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let (_, plaintext) = match self.decrypt_blob(&bytes) {
            Ok(decoded) => decoded,
            Err(_) => return Ok(Vec::new()),
        };
        Self::decode_range_index_entries(&plaintext)
    }

    fn record_range_index_entry(
        &self,
        snapshot_id: &str,
        file_object_id: &str,
        layout_generation: u64,
        new_entry: RangeIndexEntry,
    ) -> Result<()> {
        let mut entries =
            self.range_index_entries(snapshot_id, file_object_id, layout_generation)?;
        if !entries.contains(&new_entry) {
            entries.push(new_entry);
        }
        self.write_range_index_entries(snapshot_id, file_object_id, layout_generation, &entries)
    }

    fn write_range_index_entries(
        &self,
        snapshot_id: &str,
        file_object_id: &str,
        layout_generation: u64,
        entries: &[RangeIndexEntry],
    ) -> Result<()> {
        let path = self.range_index_path(snapshot_id, file_object_id, layout_generation);
        let encoded = Self::encode_range_index_entries(entries)?;
        let ciphertext = self.encrypt_blob(0, &encoded)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, ciphertext)?;
        Ok(())
    }

    fn best_effort_prune_range_entries(
        &self,
        snapshot_id: &str,
        file_object_id: &str,
        layout_generation: u64,
        stale_entries: &[RangeIndexEntry],
        retained_entries: Option<&[RangeIndexEntry]>,
    ) {
        for entry in stale_entries {
            let path = self.cache_path(
                snapshot_id,
                file_object_id,
                layout_generation,
                entry.offset,
                entry.length,
            );
            let _ = fs::remove_file(path);
        }

        let remaining = if let Some(entries) = retained_entries {
            entries.to_vec()
        } else {
            let Ok(mut entries) =
                self.range_index_entries(snapshot_id, file_object_id, layout_generation)
            else {
                return;
            };
            entries.retain(|entry| !stale_entries.contains(entry));
            entries
        };

        let index_path = self.range_index_path(snapshot_id, file_object_id, layout_generation);
        if remaining.is_empty() {
            let _ = fs::remove_file(index_path);
        } else {
            let _ = self.write_range_index_entries(
                snapshot_id,
                file_object_id,
                layout_generation,
                &remaining,
            );
        }
    }

    fn encode_range_index_entries(entries: &[RangeIndexEntry]) -> Result<Vec<u8>> {
        let mut bytes = Vec::with_capacity(8 + entries.len() * 16);
        let count = u64::try_from(entries.len())
            .map_err(|_| anyhow::anyhow!("too many VFS cache range index entries"))?;
        bytes.extend_from_slice(&count.to_le_bytes());
        for entry in entries {
            let offset = u64::try_from(entry.offset)
                .map_err(|_| anyhow::anyhow!("VFS cache range index offset is too large"))?;
            let length = u64::try_from(entry.length)
                .map_err(|_| anyhow::anyhow!("VFS cache range index length is too large"))?;
            bytes.extend_from_slice(&offset.to_le_bytes());
            bytes.extend_from_slice(&length.to_le_bytes());
        }
        Ok(bytes)
    }

    fn decode_range_index_entries(bytes: &[u8]) -> Result<Vec<RangeIndexEntry>> {
        anyhow::ensure!(
            bytes.len() >= 8,
            "encrypted VFS disk cache range index is truncated"
        );
        let mut count_bytes = [0u8; 8];
        count_bytes.copy_from_slice(&bytes[..8]);
        let count = usize::try_from(u64::from_le_bytes(count_bytes)).map_err(|_| {
            anyhow::anyhow!("encrypted VFS disk cache range index count is too large")
        })?;
        let expected_len = 8usize
            .checked_add(count.checked_mul(16).ok_or_else(|| {
                anyhow::anyhow!("encrypted VFS disk cache range index is too large")
            })?)
            .ok_or_else(|| anyhow::anyhow!("encrypted VFS disk cache range index is too large"))?;
        anyhow::ensure!(
            bytes.len() == expected_len,
            "encrypted VFS disk cache range index is malformed"
        );
        let mut entries = Vec::with_capacity(count);
        let mut cursor = 8;
        for _ in 0..count {
            let mut offset_bytes = [0u8; 8];
            offset_bytes.copy_from_slice(&bytes[cursor..cursor + 8]);
            cursor += 8;
            let mut length_bytes = [0u8; 8];
            length_bytes.copy_from_slice(&bytes[cursor..cursor + 8]);
            cursor += 8;
            entries.push(RangeIndexEntry {
                offset: usize::try_from(u64::from_le_bytes(offset_bytes)).map_err(|_| {
                    anyhow::anyhow!("encrypted VFS disk cache range index offset is too large")
                })?,
                length: usize::try_from(u64::from_le_bytes(length_bytes)).map_err(|_| {
                    anyhow::anyhow!("encrypted VFS disk cache range index length is too large")
                })?,
            });
        }
        Ok(entries)
    }

    fn encrypt_blob(&self, offset: usize, plaintext: &[u8]) -> Result<Vec<u8>> {
        let cipher = XChaCha20Poly1305::new((&self.cipher_key).into());
        let mut nonce = [0u8; 24];
        getrandom_fill(&mut nonce)
            .map_err(|_| anyhow::anyhow!("failed to obtain encrypted cache nonce"))?;
        let offset = u64::try_from(offset)
            .map_err(|_| anyhow::anyhow!("failed to encode encrypted cache offset"))?;
        let mut framed_plaintext = offset.to_le_bytes().to_vec();
        framed_plaintext.extend_from_slice(plaintext);
        let ciphertext = cipher
            .encrypt(XNonce::from_slice(&nonce), framed_plaintext.as_slice())
            .map_err(|_| anyhow::anyhow!("failed to encrypt VFS disk cache entry"))?;
        let mut output = nonce.to_vec();
        output.extend_from_slice(&ciphertext);
        Ok(output)
    }

    fn decrypt_blob(&self, bytes: &[u8]) -> Result<(usize, Vec<u8>)> {
        anyhow::ensure!(
            bytes.len() >= 24,
            "encrypted VFS disk cache entry is truncated"
        );
        let (nonce, ciphertext) = bytes.split_at(24);
        let cipher = XChaCha20Poly1305::new((&self.cipher_key).into());
        let plaintext = cipher
            .decrypt(XNonce::from_slice(nonce), ciphertext)
            .map_err(|_| anyhow::anyhow!("failed to decrypt VFS disk cache entry"))?;
        anyhow::ensure!(
            plaintext.len() >= std::mem::size_of::<u64>(),
            "encrypted VFS disk cache entry is truncated"
        );
        let mut offset_bytes = [0u8; 8];
        offset_bytes.copy_from_slice(&plaintext[..8]);
        let offset = usize::try_from(u64::from_le_bytes(offset_bytes))
            .map_err(|_| anyhow::anyhow!("encrypted VFS disk cache offset is too large"))?;
        Ok((offset, plaintext[8..].to_vec()))
    }
}

impl PlaintextCache {
    fn new(budget_bytes: usize) -> Self {
        Self {
            budget_bytes,
            total_bytes: 0,
            next_access_order: 0,
            entries: HashMap::new(),
        }
    }

    fn get_range(
        &mut self,
        key: &PlaintextCacheKey,
        offset: usize,
        length: usize,
    ) -> Option<Vec<u8>> {
        let access_order = self.next_access_order();
        let entry = self.entries.get_mut(key)?;
        entry.last_access_order = access_order;
        let end = offset.saturating_add(length).min(entry.bytes.len());
        Some(entry.bytes.get(offset..end)?.to_vec())
    }

    fn insert(&mut self, key: PlaintextCacheKey, bytes: Vec<u8>) {
        if self.budget_bytes == 0 || bytes.len() > self.budget_bytes {
            return;
        }

        if let Some(previous) = self.entries.remove(&key) {
            self.total_bytes = self.total_bytes.saturating_sub(previous.bytes.len());
        }

        let len = bytes.len();
        let access_order = self.next_access_order();
        self.entries.insert(
            key,
            PlaintextCacheEntry {
                bytes,
                last_access_order: access_order,
            },
        );
        self.total_bytes = self.total_bytes.saturating_add(len);
        self.evict_if_needed();
    }

    fn evict_if_needed(&mut self) {
        while self.total_bytes > self.budget_bytes {
            let Some((oldest_key, oldest_len)) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_access_order)
                .map(|(key, entry)| (key.clone(), entry.bytes.len()))
            else {
                break;
            };
            self.entries.remove(&oldest_key);
            self.total_bytes = self.total_bytes.saturating_sub(oldest_len);
        }
    }

    fn next_access_order(&mut self) -> u64 {
        let current = self.next_access_order;
        self.next_access_order = self.next_access_order.saturating_add(1);
        current
    }
}

impl OpenedFile {
    fn cached_range_bytes(&self, offset: usize, length: usize) -> Option<Vec<u8>> {
        let cached = lock_or_recover(&self.plaintext_cache);
        let cached = cached.as_ref()?;
        let cache_end = cached.offset.saturating_add(cached.bytes.len());
        let request_end = offset.saturating_add(length);
        if offset < cached.offset || request_end > cache_end {
            return None;
        }
        let start = offset - cached.offset;
        let end = start + length;
        Some(cached.bytes[start..end].to_vec())
    }

    pub fn snapshot_id(&self) -> &str {
        match &self.backing {
            OpenedFileBacking::Snapshot(file) => &file.snapshot_id,
            OpenedFileBacking::Overlay { snapshot_id, .. } => snapshot_id,
        }
    }

    pub fn logical_path(&self) -> &str {
        &self.logical_path
    }

    pub fn file_object_id(&self) -> &str {
        match &self.backing {
            OpenedFileBacking::Snapshot(file) => &file.file_object_id,
            OpenedFileBacking::Overlay { file_object_id, .. } => file_object_id,
        }
    }

    pub fn layout_generation(&self) -> u64 {
        match &self.backing {
            OpenedFileBacking::Snapshot(file) => file.layout_generation(),
            OpenedFileBacking::Overlay {
                layout_generation, ..
            } => *layout_generation,
        }
    }

    pub fn inode_id(&self) -> u64 {
        self.inode_id
    }

    pub fn file_size(&self) -> u64 {
        match &self.backing {
            OpenedFileBacking::Snapshot(file) => file.file_size(),
            OpenedFileBacking::Overlay { file_size, .. } => *file_size,
        }
    }
}

fn stable_inode_id(snapshot_id: &str, logical_path: &str, file_object_id: &str) -> u64 {
    let mut hasher = Hasher::new();
    hasher.update(snapshot_id.as_bytes());
    hasher.update(&[0]);
    hasher.update(logical_path.as_bytes());
    hasher.update(&[0]);
    hasher.update(file_object_id.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest.as_bytes()[..8]);
    let value = u64::from_le_bytes(bytes);
    if value == 0 { 1 } else { value }
}

fn stable_directory_inode_id(snapshot_id: &str, logical_path: &str) -> u64 {
    stable_inode_id(snapshot_id, logical_path, "__dir__")
}

fn normalize_vfs_path(path: &str) -> String {
    path.trim_matches('/')
        .split('/')
        .map(|component| component.nfc().collect::<String>())
        .collect::<Vec<_>>()
        .join("/")
}

fn parent_vfs_path(path: &str) -> String {
    path.rsplit_once('/')
        .map(|(parent, _)| parent.to_string())
        .unwrap_or_default()
}

fn immediate_child_name(parent: &str, path: &str) -> Option<String> {
    let remainder = if parent.is_empty() {
        path
    } else {
        path.strip_prefix(&format!("{parent}/"))?
    };
    if remainder.is_empty() {
        return None;
    }
    let name = remainder.split('/').next()?.to_string();
    Some(name)
}

pub fn mount_snapshot(
    repo_root: PathBuf,
    snapshot_id: String,
    mount_point: PathBuf,
) -> Result<MountLaunchSummary> {
    try_mount_snapshot_on_current_platform(
        VfsMountConfig::snapshot(repo_root, snapshot_id),
        mount_point,
    )
}

pub fn mount_live_branch(
    repo_root: PathBuf,
    branch_token_hex: String,
    mount_point: PathBuf,
) -> Result<MountLaunchSummary> {
    try_mount_live_branch_on_current_platform(
        VfsMountConfig::live_branch(repo_root, branch_token_hex),
        mount_point,
    )
}

pub fn start_snapshot_mount(
    repo_root: PathBuf,
    snapshot_id: String,
    mount_point: PathBuf,
) -> Result<MountedFilesystem> {
    #[cfg(windows)]
    {
        crate::windows::start_snapshot_mount(
            VfsMountConfig::snapshot(repo_root, snapshot_id),
            mount_point,
        )
    }
    #[cfg(not(windows))]
    {
        let summary = try_mount_snapshot_on_current_platform(
            VfsMountConfig::snapshot(repo_root, snapshot_id),
            mount_point,
        )?;
        Ok(MountedFilesystem::from_summary(summary))
    }
}

pub fn start_live_branch_mount(
    repo_root: PathBuf,
    branch_token_hex: String,
    mount_point: PathBuf,
) -> Result<MountedFilesystem> {
    #[cfg(windows)]
    {
        crate::windows::start_live_branch_mount(
            VfsMountConfig::live_branch(repo_root, branch_token_hex),
            mount_point,
        )
    }
    #[cfg(not(windows))]
    {
        let summary = try_mount_live_branch_on_current_platform(
            VfsMountConfig::live_branch(repo_root, branch_token_hex),
            mount_point,
        )?;
        Ok(MountedFilesystem::from_summary(summary))
    }
}

#[doc(hidden)]
pub mod testing {
    use std::path::PathBuf;

    use anyhow::Result;

    use super::{MountLaunchSummary, OpenedFile, VfsMountConfig};
    use crate::windows::{WinfspMountExports, WinfspNativeCreateRequest, WinfspRuntimePaths};

    pub use crate::platform::{LinuxMountAdapter, MacosMountAdapter, PlatformFamily};
    pub use crate::windows::{
        WindowsMountLauncher, WinfspHostConfig, WinfspHostDriver, WinfspHostLauncher,
        WinfspHostSession, WinfspInvalidator, WinfspMountContext, WinfspOpenRequest,
        WinfspRuntimeLibrary, WinfspVolumeParams,
    };

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct MountRequest(super::MountRequest);

    impl MountRequest {
        pub fn from_config(config: VfsMountConfig, mount_point: PathBuf) -> Self {
            Self(super::MountRequest::from_config(config, mount_point))
        }

        pub fn snapshot(repo_root: PathBuf, snapshot_id: String, mount_point: PathBuf) -> Self {
            Self(super::MountRequest::snapshot(
                repo_root,
                snapshot_id,
                mount_point,
            ))
        }

        pub fn live_branch(
            repo_root: PathBuf,
            branch_token_hex: String,
            mount_point: PathBuf,
        ) -> Self {
            Self(super::MountRequest::live_branch(
                repo_root,
                branch_token_hex,
                mount_point,
            ))
        }

        pub fn mount_point(&self) -> &PathBuf {
            self.0.mount_point()
        }

        pub fn mount_mode_label(&self) -> &'static str {
            self.0.mount_mode_label()
        }

        fn as_inner(&self) -> &super::MountRequest {
            &self.0
        }

        fn into_inner(self) -> super::MountRequest {
            self.0
        }
    }

    pub trait VfsHostLauncher {
        fn launch(&mut self, request: &MountRequest) -> Result<()>;
    }

    impl WindowsMountLauncher {
        pub fn from_test_request(request: &MountRequest) -> Self {
            Self::from_request(request.as_inner())
        }
    }

    impl WinfspMountContext {
        pub fn from_test_request(request: MountRequest) -> anyhow::Result<Self> {
            Self::from_request(request.into_inner())
        }
    }

    impl LinuxMountAdapter {
        pub fn platform_family_for_test(self) -> PlatformFamily {
            crate::platform::PlatformMountAdapter::platform_family(&self)
        }

        pub fn launch_test_request(self, request: MountRequest) -> Result<MountLaunchSummary> {
            crate::platform::PlatformMountAdapter::launch(&self, request.into_inner())
        }
    }

    impl MacosMountAdapter {
        pub fn platform_family_for_test(self) -> PlatformFamily {
            crate::platform::PlatformMountAdapter::platform_family(&self)
        }

        pub fn launch_test_request(self, request: MountRequest) -> Result<MountLaunchSummary> {
            crate::platform::PlatformMountAdapter::launch(&self, request.into_inner())
        }
    }

    pub fn try_mount_snapshot_on_current_platform_for_test(
        config: VfsMountConfig,
        mount_point: PathBuf,
    ) -> Result<MountLaunchSummary> {
        crate::platform::try_mount_snapshot_on_current_platform(config, mount_point)
    }

    pub fn try_mount_live_branch_on_current_platform_for_test(
        config: VfsMountConfig,
        mount_point: PathBuf,
    ) -> Result<MountLaunchSummary> {
        crate::platform::try_mount_live_branch_on_current_platform(config, mount_point)
    }

    pub fn opened_file_cached_plaintext(opened_file: &OpenedFile) -> Option<Vec<u8>> {
        opened_file
            .plaintext_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .map(|cached| cached.bytes.clone())
    }

    pub fn winfsp_runtime_paths_from_install_root(
        install_root: PathBuf,
        arch: &str,
    ) -> Result<WinfspRuntimePaths> {
        WinfspRuntimePaths::from_install_root(install_root, arch)
    }

    pub fn winfsp_runtime_paths_from_candidate_roots(
        candidate_roots: &[PathBuf],
        arch: &str,
    ) -> Result<WinfspRuntimePaths> {
        WinfspRuntimePaths::from_candidate_roots(candidate_roots, arch)
    }

    pub fn winfsp_runtime_get_symbol_address(
        runtime: &WinfspRuntimeLibrary,
        symbol_name: &str,
    ) -> Result<usize> {
        runtime.get_symbol_address_public(symbol_name)
    }

    pub fn winfsp_runtime_resolve_mount_exports(
        runtime: &WinfspRuntimeLibrary,
    ) -> Result<WinfspMountExports> {
        runtime.resolve_mount_exports_public()
    }

    pub fn winfsp_host_session_new(
        runtime: WinfspRuntimeLibrary,
        host_config: WinfspHostConfig,
        volume_params: WinfspVolumeParams,
    ) -> Result<WinfspHostSession> {
        WinfspHostSession::new(runtime, host_config, volume_params)
    }

    pub fn winfsp_session_is_mounted(session: &WinfspHostSession) -> bool {
        session.is_mounted()
    }

    pub fn winfsp_session_build_native_create_request(
        session: &WinfspHostSession,
    ) -> Result<WinfspNativeCreateRequest> {
        session.build_native_create_request()
    }

    pub fn winfsp_session_create_filesystem_handle(session: &mut WinfspHostSession) -> Result<()> {
        session.create_filesystem_handle()
    }

    pub fn winfsp_session_has_native_filesystem_handle(session: &WinfspHostSession) -> bool {
        session.has_native_filesystem_handle()
    }

    pub fn winfsp_session_destroy_filesystem_handle(session: &mut WinfspHostSession) {
        session.destroy_filesystem_handle();
    }

    pub fn winfsp_session_run_mount_lifecycle(
        session: &mut WinfspHostSession,
        driver: &impl WinfspHostDriver,
        mount_point: PathBuf,
        thread_count: u32,
    ) -> Result<()> {
        session.run_mount_lifecycle(driver, mount_point, thread_count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{self, AssertUnwindSafe};
    use std::sync::Arc;

    use e2v_core::{CommitOptions, InitOptions, RepositoryFacade};
    use tempfile::tempdir;

    fn init_repo(repo_root: &std::path::Path) {
        RepositoryFacade::new()
            .init(InitOptions {
                repo_root: repo_root.to_path_buf(),
                password: "correct horse battery staple".to_string(),
                branch_name: "main".to_string(),
            })
            .unwrap();
    }

    fn commit_message(repo_root: &std::path::Path, message: &str, body: &str) -> String {
        fs::write(repo_root.join("tracked.txt"), body).unwrap();
        RepositoryFacade::new()
            .commit(CommitOptions {
                repo_root: repo_root.to_path_buf(),
                message: message.to_string(),
            })
            .unwrap()
            .snapshot_id
    }

    fn poison_mutex<T>(mutex: &Arc<Mutex<T>>) {
        let poisoned = Arc::clone(mutex);
        let _ = panic::catch_unwind(AssertUnwindSafe(move || {
            let _guard = poisoned.lock().unwrap();
            panic!("poison test mutex");
        }));
    }

    #[test]
    fn plaintext_cache_returns_only_the_requested_slice() {
        let mut cache = PlaintextCache::new(1024);
        let key = ("snapshot".to_string(), "file".to_string(), 7);
        cache.insert(key.clone(), b"alpha".to_vec());

        assert_eq!(cache.get_range(&key, 1, 3).unwrap(), b"lph".to_vec());
    }

    #[test]
    fn read_recovers_from_poisoned_open_file_cache_and_repopulates_it() {
        let temp = tempdir().unwrap();
        let repo_root = temp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();
        init_repo(&repo_root);

        let snapshot_id = commit_message(&repo_root, "first", "alpha");
        let vfs =
            ReadOnlyVfs::mount_snapshot(VfsMountConfig::snapshot(repo_root, snapshot_id)).unwrap();

        let handle = vfs.open_file("tracked.txt").unwrap();
        poison_mutex(&handle.plaintext_cache);

        let bytes = vfs.read(&handle, 0, 5).unwrap();
        assert_eq!(bytes, b"alpha");

        let cached = handle
            .plaintext_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(
            cached.as_ref().map(|range| range.bytes.clone()),
            Some(b"alpha".to_vec())
        );
    }

    #[test]
    fn read_recovers_from_poisoned_global_plaintext_cache_and_repopulates_it() {
        let temp = tempdir().unwrap();
        let repo_root = temp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();
        init_repo(&repo_root);

        let snapshot_id = commit_message(&repo_root, "first", "alpha");
        let vfs =
            ReadOnlyVfs::mount_snapshot(VfsMountConfig::snapshot(repo_root, snapshot_id.clone()))
                .unwrap();

        poison_mutex(&vfs.plaintext_cache);

        let handle = vfs.open_file("tracked.txt").unwrap();
        let bytes = vfs.read(&handle, 0, 5).unwrap();
        assert_eq!(bytes, b"alpha");

        let mut cache = vfs
            .plaintext_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let key = (
            snapshot_id,
            handle.file_object_id().to_string(),
            handle.layout_generation(),
        );
        assert_eq!(cache.get_range(&key, 0, 5), Some(b"alpha".to_vec()));
    }

    #[test]
    fn writable_writeback_recovers_from_poisoned_plaintext_cache_reset() {
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
        let mut vfs =
            WritableVfs::mount_live_branch(VfsMountConfig::live_branch(repo_root, branch_token))
                .unwrap();

        poison_mutex(&vfs.inner.plaintext_cache);
        vfs.write_file("created.txt", b"beta".to_vec()).unwrap();

        let refresh = vfs.writeback("second").unwrap();
        assert!(refresh.namespace_changed);
    }
}
