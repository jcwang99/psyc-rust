use std::collections::HashMap;
use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use blake3::Hasher;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use e2v_core::facade::SnapshotHandle;
use e2v_core::{DirectoryEntry, FileHandle, ReadService};
use getrandom::fill as getrandom_fill;
use unicode_normalization::UnicodeNormalization;

mod platform;
mod windows;

pub use platform::{
    LinuxMountAdapter, MacosMountAdapter, PlatformFamily, PlatformMountAdapter,
    WindowsMountAdapter, try_mount_live_branch_on_current_platform,
    try_mount_snapshot_on_current_platform,
};
pub use windows::{
    ReadOnlyVolumeSummary, WindowsMountLauncher, WinfspHostConfig, WinfspHostDriver,
    WinfspHostLauncher, WinfspHostSession, WinfspInvalidationPlan, WinfspInvalidator,
    WinfspMountContext, WinfspOpenHandle, WinfspOpenRequest, WinfspRuntimeLibrary,
    WinfspRuntimePaths, WinfspVolumeParams,
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
pub struct MountRequest {
    config: VfsMountConfig,
    mount_point: PathBuf,
}

pub trait VfsHostLauncher {
    fn launch(&mut self, request: &MountRequest) -> Result<()>;
}

impl MountRequest {
    pub fn from_config(config: VfsMountConfig, mount_point: PathBuf) -> Self {
        Self {
            config,
            mount_point,
        }
    }

    pub fn snapshot(repo_root: PathBuf, snapshot_id: String, mount_point: PathBuf) -> Self {
        Self::from_config(
            VfsMountConfig::snapshot(repo_root, snapshot_id),
            mount_point,
        )
    }

    pub fn live_branch(repo_root: PathBuf, branch_token_hex: String, mount_point: PathBuf) -> Self {
        Self::from_config(
            VfsMountConfig::live_branch(repo_root, branch_token_hex),
            mount_point,
        )
    }

    pub fn mount_point(&self) -> &PathBuf {
        &self.mount_point
    }

    pub fn mount_mode_label(&self) -> &'static str {
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
    file: FileHandle,
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
pub struct MountLaunchSummary {
    pub mount_mode: String,
    pub mount_point: PathBuf,
    pub cache_policy: CachePolicy,
    pub read_only: bool,
    pub stream_only: bool,
    pub status_message: String,
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

    pub fn open_file(&self, path: &str) -> Result<OpenedFile> {
        let normalized = normalize_vfs_path(path);
        let file = self
            .read_service
            .open_file(&self.namespace_snapshot, &normalized)?;
        Ok(OpenedFile {
            inode_id: stable_inode_id(&file.snapshot_id, &normalized, &file.file_object_id),
            logical_path: normalized,
            plaintext_cache: Arc::new(Mutex::new(None)),
            file,
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
                size_bytes: file.file.file_size(),
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
        let cache_key = (
            opened_file.snapshot_id().to_string(),
            opened_file.file_object_id().to_string(),
            opened_file.layout_generation(),
        );

        if let Some(cached) = opened_file.cached_range_bytes(offset, length) {
            return Ok(cached);
        }

        if let Some(cached) = self
            .plaintext_cache
            .lock()
            .unwrap()
            .get(&cache_key)
        {
            let start = offset.min(cached.len());
            let end = offset.saturating_add(length).min(cached.len());
            let requested = cached[start..end].to_vec();
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

        let bytes = self
            .read_service
            .read_range(&opened_file.file, offset, length)?;
        if let Some(cache) = &self.encrypted_range_cache {
            cache.write_range(
                opened_file.snapshot_id(),
                opened_file.file_object_id(),
                opened_file.layout_generation(),
                offset,
                &bytes,
            )?;
        }
        let file_size = opened_file.file.file_size() as usize;
        if offset == 0 && offset.saturating_add(bytes.len()) >= file_size {
            let cacheable = self.cacheable_plaintext_range(0, bytes.clone());
            self.replace_open_file_cache(opened_file, cacheable.clone());
            if let Some(cacheable) = cacheable {
                self.plaintext_cache
                    .lock()
                    .unwrap()
                    .insert(cache_key, cacheable.bytes);
            }
        } else if offset == 0 && offset.saturating_add(length) >= file_size {
            let full = self
                .read_service
                .read_range(&opened_file.file, 0, file_size)?;
            let cacheable = self.cacheable_plaintext_range(0, full);
            if let Some(cacheable) = cacheable.clone() {
                self.plaintext_cache
                    .lock()
                    .unwrap()
                    .insert(cache_key, cacheable.bytes.clone());
            }
            self.replace_open_file_cache(opened_file, cacheable);
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
        (bytes.len() <= self.plaintext_memory_cache_budget_bytes).then_some(CachedRange {
            offset,
            bytes,
        })
    }

    fn replace_open_file_cache(&self, opened_file: &OpenedFile, cached_range: Option<CachedRange>) {
        *opened_file.plaintext_cache.lock().unwrap() = cached_range;
    }
}

#[derive(Debug, Clone)]
struct EncryptedRangeCache {
    cache_dir: PathBuf,
    cipher_key: [u8; 32],
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
        let bytes = match fs::read(&cache_path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        Ok(Some(self.decrypt_blob(&bytes)?))
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
        let ciphertext = self.encrypt_blob(plaintext)?;
        fs::write(cache_path, ciphertext)?;
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
        let mut hasher = Hasher::new();
        hasher.update(snapshot_id.as_bytes());
        hasher.update(&[0]);
        hasher.update(file_object_id.as_bytes());
        hasher.update(&[0]);
        hasher.update(&layout_generation.to_le_bytes());
        hasher.update(&offset.to_le_bytes());
        hasher.update(&length.to_le_bytes());
        self.cache_dir
            .join(format!("{}.bin", hex::encode(hasher.finalize().as_bytes())))
    }

    fn encrypt_blob(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let cipher = XChaCha20Poly1305::new((&self.cipher_key).into());
        let mut nonce = [0u8; 24];
        getrandom_fill(&mut nonce)
            .map_err(|_| anyhow::anyhow!("failed to obtain encrypted cache nonce"))?;
        let ciphertext = cipher
            .encrypt(XNonce::from_slice(&nonce), plaintext)
            .map_err(|_| anyhow::anyhow!("failed to encrypt VFS disk cache entry"))?;
        let mut output = nonce.to_vec();
        output.extend_from_slice(&ciphertext);
        Ok(output)
    }

    fn decrypt_blob(&self, bytes: &[u8]) -> Result<Vec<u8>> {
        anyhow::ensure!(
            bytes.len() >= 24,
            "encrypted VFS disk cache entry is truncated"
        );
        let (nonce, ciphertext) = bytes.split_at(24);
        let cipher = XChaCha20Poly1305::new((&self.cipher_key).into());
        cipher
            .decrypt(XNonce::from_slice(nonce), ciphertext)
            .map_err(|_| anyhow::anyhow!("failed to decrypt VFS disk cache entry"))
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

    fn get(&mut self, key: &PlaintextCacheKey) -> Option<Vec<u8>> {
        let access_order = self.next_access_order();
        let entry = self.entries.get_mut(key)?;
        entry.last_access_order = access_order;
        Some(entry.bytes.clone())
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
        let cached = self.plaintext_cache.lock().unwrap();
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
        &self.file.snapshot_id
    }

    pub fn logical_path(&self) -> &str {
        &self.logical_path
    }

    pub fn file_object_id(&self) -> &str {
        &self.file.file_object_id
    }

    pub fn layout_generation(&self) -> u64 {
        self.file.layout_generation()
    }

    pub fn inode_id(&self) -> u64 {
        self.inode_id
    }

    pub fn cached_plaintext_for_test(&self) -> Option<Vec<u8>> {
        self.plaintext_cache
            .lock()
            .unwrap()
            .as_ref()
            .map(|cached| cached.bytes.clone())
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
