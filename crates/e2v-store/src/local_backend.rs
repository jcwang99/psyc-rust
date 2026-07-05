use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, ensure};

use crate::capability::{BackendCapability, ConsistencyClass};
use crate::layout::LayoutRoot;
use crate::layout_root_store::{LayoutRootStore, LayoutRootVersion};
use crate::opendal_backend::RemoteBackend;
use crate::ref_store::{CasResult, ListedRef, RefStore, RefToken, RefVersion, StoredRef};
use crate::remote_telemetry::{RemoteOperationKind, RemoteTelemetryHandle};

pub trait BlobStore {
    fn put_physical(&self, relative_path: &str, bytes: &[u8]) -> Result<()>;
    fn put_physical_if_absent(&self, relative_path: &str, bytes: &[u8]) -> Result<bool>;
    fn get_physical(&self, relative_path: &str) -> Result<Vec<u8>>;
    fn get_physical_range(
        &self,
        relative_path: &str,
        offset: usize,
        length: usize,
    ) -> Result<Vec<u8>>;
    fn delete_physical(&self, relative_path: &str) -> Result<()>;
    fn exists_physical(&self, relative_path: &str) -> bool;
    fn stat_physical(&self, relative_path: &str) -> Result<ObjectStat>;
    fn list_physical(&self, prefix: &str) -> Result<Vec<String>>;
}

pub fn is_missing_physical_object_error(error: &anyhow::Error) -> bool {
    if error.to_string().contains("missing physical object") {
        return true;
    }

    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .map(|io_error| io_error.kind() == std::io::ErrorKind::NotFound)
            .unwrap_or(false)
            || cause
                .downcast_ref::<opendal::Error>()
                .map(|opendal_error| opendal_error.kind() == opendal::ErrorKind::NotFound)
                .unwrap_or(false)
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectStat {
    pub length: u64,
    pub modified_at: Option<std::time::SystemTime>,
}

#[derive(Debug, Clone)]
pub struct LocalFolderBackend {
    repo_root: PathBuf,
    telemetry: Option<RemoteTelemetryHandle>,
}

impl LocalFolderBackend {
    pub fn new(repo_root: impl AsRef<Path>) -> Self {
        Self::new_with_optional_telemetry(repo_root, None)
    }

    pub fn new_with_telemetry(
        repo_root: impl AsRef<Path>,
        telemetry: RemoteTelemetryHandle,
    ) -> Self {
        Self::new_with_optional_telemetry(repo_root, Some(telemetry))
    }

    fn new_with_optional_telemetry(
        repo_root: impl AsRef<Path>,
        telemetry: Option<RemoteTelemetryHandle>,
    ) -> Self {
        Self {
            repo_root: repo_root.as_ref().to_path_buf(),
            telemetry,
        }
    }

    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    fn capability() -> BackendCapability {
        BackendCapability {
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
            supports_layout_root_cas: false,
            supports_oblivious_access_schedule: true,
        }
    }

    fn resolve_relative_path(&self, relative_path: &str) -> Result<PathBuf> {
        let path = Path::new(relative_path);
        ensure!(!relative_path.is_empty(), "path must not be empty");
        ensure!(!path.is_absolute(), "path must be relative to repo root");
        ensure!(
            path.components()
                .all(|component| matches!(component, Component::Normal(_))),
            "path traversal outside repo root is not allowed"
        );
        Ok(self.repo_root.join(path))
    }

    pub fn put_object(&self, relative_path: &str, bytes: &[u8]) -> Result<()> {
        let full_path = self.resolve_relative_path(relative_path)?;
        if let Some(parent) = full_path.parent() {
            ensure_directory_path(parent)?;
        }
        match fs::write(&full_path, bytes) {
            Ok(()) => {}
            Err(_) => {
                remove_path_if_exists(&full_path)?;
                fs::write(&full_path, bytes)
                    .with_context(|| format!("failed to write object {}", full_path.display()))?;
            }
        }
        Ok(())
    }

    pub fn get_object(&self, relative_path: &str) -> Result<Vec<u8>> {
        let full_path = self.resolve_relative_path(relative_path)?;
        fs::read(&full_path)
            .with_context(|| format!("failed to read object {}", full_path.display()))
    }

    pub fn exists_object(&self, relative_path: &str) -> bool {
        self.resolve_relative_path(relative_path)
            .map(|path| path.is_file())
            .unwrap_or(false)
    }

    fn ref_path(token: &RefToken) -> String {
        format!("control/refs/by-token/{}.json", token.value)
    }

    fn default_layout_root() -> LayoutRoot {
        LayoutRoot::direct_default()
    }

    fn layout_history_path(generation: u64) -> String {
        format!("control/layout-roots/{generation:020}.json")
    }

    pub(crate) fn override_physical_modified_time_for_test(
        &self,
        relative_path: &str,
        modified_at: std::time::SystemTime,
    ) -> Result<()> {
        let full_path = self.resolve_relative_path(relative_path)?;
        anyhow::ensure!(
            full_path.is_file(),
            "cannot override modified time for missing physical object {relative_path}"
        );
        let file = std::fs::File::options().write(true).open(&full_path)?;
        file.set_times(std::fs::FileTimes::new().set_modified(modified_at))?;
        Ok(())
    }

    fn record_result<T>(
        &self,
        kind: RemoteOperationKind,
        relative_path: &str,
        bytes_sent: u64,
        action: impl FnOnce() -> Result<T>,
        summarize: impl FnOnce(&T) -> (u64, u64),
    ) -> Result<T> {
        let start = Instant::now();
        let result = action();
        if let Some(telemetry) = &self.telemetry {
            let (bytes_received, listed_entries) = match result.as_ref() {
                Ok(value) => summarize(value),
                Err(_) => (0, 0),
            };
            telemetry.record(
                kind,
                relative_path,
                start.elapsed(),
                bytes_sent,
                bytes_received,
                listed_entries,
                result.is_ok(),
            );
        }
        result
    }

    fn record_bool(
        &self,
        kind: RemoteOperationKind,
        relative_path: &str,
        action: impl FnOnce() -> bool,
    ) -> bool {
        let start = Instant::now();
        let result = action();
        if let Some(telemetry) = &self.telemetry {
            telemetry.record(kind, relative_path, start.elapsed(), 0, 0, 0, true);
        }
        result
    }
}

impl PartialEq for LocalFolderBackend {
    fn eq(&self, other: &Self) -> bool {
        self.repo_root == other.repo_root
    }
}

impl Eq for LocalFolderBackend {}

fn ensure_directory_path(path: &Path) -> Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    if let Some(parent) = path.parent()
        && parent != path
    {
        ensure_directory_path(parent)?;
    }
    remove_path_if_exists(path)?;
    fs::create_dir_all(path)
        .with_context(|| format!("failed to create object parent {}", path.display()))?;
    Ok(())
}

fn remove_path_if_exists(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path)?,
        Ok(_) => fs::remove_file(path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn path_conflict_exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

impl BlobStore for LocalFolderBackend {
    fn put_physical(&self, relative_path: &str, bytes: &[u8]) -> Result<()> {
        self.record_result(
            RemoteOperationKind::Write,
            relative_path,
            bytes.len() as u64,
            || self.put_object(relative_path, bytes),
            |_| (0, 0),
        )
    }

    fn put_physical_if_absent(&self, relative_path: &str, bytes: &[u8]) -> Result<bool> {
        self.record_result(
            RemoteOperationKind::WriteIfAbsent,
            relative_path,
            bytes.len() as u64,
            || {
                let full_path = self.resolve_relative_path(relative_path)?;
                if let Some(parent) = full_path.parent() {
                    match ensure_directory_path(parent) {
                        Ok(()) => {}
                        Err(error) if path_conflict_exists(parent) => return Ok(false),
                        Err(error) => return Err(error),
                    }
                }
                match std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&full_path)
                {
                    Ok(mut file) => {
                        use std::io::Write;
                        file.write_all(bytes).with_context(|| {
                            format!("failed to write object {}", full_path.display())
                        })?;
                        Ok(true)
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
                    Err(error) if path_conflict_exists(&full_path) => {
                        let _ = error;
                        Ok(false)
                    }
                    Err(error) => Err(error).with_context(|| {
                        format!("failed to create object {}", full_path.display())
                    }),
                }
            },
            |_| (0, 0),
        )
    }

    fn get_physical(&self, relative_path: &str) -> Result<Vec<u8>> {
        self.record_result(
            RemoteOperationKind::Read,
            relative_path,
            0,
            || self.get_object(relative_path),
            |bytes| (bytes.len() as u64, 0),
        )
    }

    fn get_physical_range(
        &self,
        relative_path: &str,
        offset: usize,
        length: usize,
    ) -> Result<Vec<u8>> {
        self.record_result(
            RemoteOperationKind::ReadRange,
            relative_path,
            0,
            || {
                let full_path = self.resolve_relative_path(relative_path)?;
                let mut file = fs::File::open(&full_path)
                    .with_context(|| format!("failed to read object {}", full_path.display()))?;
                let total_length: usize = file
                    .metadata()
                    .with_context(|| format!("failed to stat object {}", full_path.display()))?
                    .len()
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("object length does not fit in usize"))?;
                anyhow::ensure!(offset <= total_length, "range offset out of bounds");
                let end = offset.saturating_add(length).min(total_length);
                if offset == end {
                    return Ok(Vec::new());
                }
                use std::io::{Read, Seek, SeekFrom};
                file.seek(SeekFrom::Start(offset as u64))
                    .with_context(|| format!("failed to seek object {}", full_path.display()))?;
                let mut bytes = vec![0u8; end - offset];
                file.read_exact(&mut bytes).with_context(|| {
                    format!("failed to read object range {}", full_path.display())
                })?;
                Ok(bytes)
            },
            |bytes| (bytes.len() as u64, 0),
        )
    }

    fn delete_physical(&self, relative_path: &str) -> Result<()> {
        self.record_result(
            RemoteOperationKind::Delete,
            relative_path,
            0,
            || {
                let full_path = self.resolve_relative_path(relative_path)?;
                remove_path_if_exists(&full_path)
                    .with_context(|| format!("failed to delete object {}", full_path.display()))
            },
            |_| (0, 0),
        )
    }

    fn exists_physical(&self, relative_path: &str) -> bool {
        self.record_bool(RemoteOperationKind::Exists, relative_path, || {
            self.exists_object(relative_path)
        })
    }

    fn stat_physical(&self, relative_path: &str) -> Result<ObjectStat> {
        self.record_result(
            RemoteOperationKind::Stat,
            relative_path,
            0,
            || {
                let full_path = self.resolve_relative_path(relative_path)?;
                let metadata = fs::metadata(&full_path)
                    .with_context(|| format!("failed to stat object {}", full_path.display()))?;
                Ok(ObjectStat {
                    length: metadata.len(),
                    modified_at: metadata.modified().ok(),
                })
            },
            |_| (0, 0),
        )
    }

    fn list_physical(&self, prefix: &str) -> Result<Vec<String>> {
        self.record_result(
            RemoteOperationKind::List,
            prefix,
            0,
            || {
                let base = self.resolve_relative_path(prefix)?;
                let mut listed = Vec::new();
                if !base.exists() {
                    return Ok(listed);
                }

                fn visit(
                    base: &Path,
                    current: &Path,
                    prefix: &str,
                    listed: &mut Vec<String>,
                ) -> Result<()> {
                    for entry in fs::read_dir(current).with_context(|| {
                        format!("failed to list objects under {}", current.display())
                    })? {
                        let entry = entry?;
                        let path = entry.path();
                        let file_type = entry.file_type()?;
                        if file_type.is_dir() {
                            visit(base, &path, prefix, listed)?;
                            continue;
                        }
                        if !file_type.is_file() {
                            continue;
                        }
                        let relative = path
                            .strip_prefix(base)
                            .with_context(|| {
                                format!(
                                    "failed to strip base {} from listed object {}",
                                    base.display(),
                                    path.display()
                                )
                            })?
                            .to_string_lossy()
                            .replace('\\', "/");
                        listed.push(format!("{prefix}{relative}"));
                    }
                    Ok(())
                }

                visit(&base, &base, prefix, &mut listed)?;
                listed.sort();
                Ok(listed)
            },
            |listed| (0, listed.len() as u64),
        )
    }
}

impl RefStore for LocalFolderBackend {
    fn read_ref(&self, token: &RefToken) -> Result<Option<StoredRef>> {
        token.validate()?;
        let path = Self::ref_path(token);
        match self.get_physical(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .context("failed to decode remote branch ref")
                .map(Some),
            Err(error) if is_missing_physical_object_error(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn list_refs(&self) -> Result<Vec<ListedRef>> {
        let mut listed = self
            .list_physical("control/refs/by-token/")?
            .into_iter()
            .filter_map(|path| {
                let token = path
                    .strip_prefix("control/refs/by-token/")?
                    .strip_suffix(".json")?
                    .to_string();
                Some((path, token))
            })
            .map(|(path, token)| -> Result<ListedRef> {
                let token = RefToken::new(token);
                token.validate()?;
                Ok(ListedRef {
                    token,
                    stored: serde_json::from_slice(&self.get_physical(&path)?)
                        .context("failed to decode remote branch ref")?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        listed.sort_by(|left, right| left.token.value.cmp(&right.token.value));
        Ok(listed)
    }

    fn compare_and_swap_ref(
        &self,
        token: &RefToken,
        expected: Option<RefVersion>,
        next: crate::ref_store::EncryptedRef,
    ) -> Result<CasResult> {
        token.validate()?;
        let current = self.read_ref(token)?;
        let matches = match (&current, expected) {
            (None, None) => true,
            (Some(stored), Some(expected_version)) => stored.version == expected_version,
            _ => false,
        };
        if !matches {
            return Ok(CasResult {
                applied: false,
                current,
            });
        }

        let stored = StoredRef {
            version: RefVersion {
                value: current
                    .as_ref()
                    .map(|existing| existing.version.value + 1)
                    .unwrap_or(1),
            },
            value: next,
        };
        self.put_physical(&Self::ref_path(token), &serde_json::to_vec(&stored)?)?;
        Ok(CasResult {
            applied: true,
            current: Some(stored),
        })
    }
}

impl LayoutRootStore for LocalFolderBackend {
    fn read_layout_root(&self) -> Result<LayoutRoot> {
        match self.get_physical("layout_root.json") {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).context("failed to decode remote layout root")
            }
            Err(error) if is_missing_physical_object_error(&error) => {
                Ok(Self::default_layout_root())
            }
            Err(error) => Err(error),
        }
    }

    fn compare_and_swap_layout_root(
        &self,
        expected: LayoutRootVersion,
        next: LayoutRoot,
    ) -> Result<CasResult> {
        let current = self.read_layout_root()?;
        if current.generation != expected {
            return Ok(CasResult {
                applied: false,
                current: None,
            });
        }

        let bytes = serde_json::to_vec(&next)?;
        self.put_physical("layout_root.json", &bytes)?;
        self.put_physical(&Self::layout_history_path(next.generation), &bytes)?;
        Ok(CasResult {
            applied: true,
            current: None,
        })
    }

    fn list_retained_layout_roots(&self) -> Result<Vec<LayoutRoot>> {
        let retained_paths = self.list_physical("control/layout-roots/")?;
        if retained_paths.is_empty() {
            return Ok(vec![self.read_layout_root()?]);
        }

        retained_paths
            .into_iter()
            .map(|path| serde_json::from_slice(&self.get_physical(&path)?).map_err(Into::into))
            .collect()
    }
}

impl RemoteBackend for LocalFolderBackend {
    fn capability(&self) -> &BackendCapability {
        static CAPABILITY: std::sync::LazyLock<BackendCapability> =
            std::sync::LazyLock::new(LocalFolderBackend::capability);
        &CAPABILITY
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::capability::WriterMode;
    use crate::opendal_backend::RemoteBackend;
    use crate::ref_store::{EncryptedRef, RefStore, RefToken, RefVersion, StoredRef};

    use tempfile::tempdir;

    use super::{BlobStore, LocalFolderBackend};

    #[test]
    fn backend_points_at_repo_root() {
        let temp = tempdir().unwrap();
        let backend = LocalFolderBackend::new(temp.path());

        assert_eq!(backend.repo_root(), temp.path());
    }

    #[test]
    fn backend_root_can_host_object_files() {
        let temp = tempdir().unwrap();
        let backend = LocalFolderBackend::new(temp.path());
        backend.put_object("objects/sample.bin", b"sample").unwrap();

        assert!(backend.exists_object("objects/sample.bin"));
        assert_eq!(backend.get_object("objects/sample.bin").unwrap(), b"sample");
    }

    #[test]
    fn blob_store_trait_supports_physical_round_trip_and_range_read() {
        let temp = tempdir().unwrap();
        let backend = LocalFolderBackend::new(temp.path());
        let blob_store: &dyn BlobStore = &backend;

        blob_store
            .put_physical("objects/sample.bin", b"hello world")
            .unwrap();
        assert!(blob_store.exists_physical("objects/sample.bin"));
        assert_eq!(
            blob_store.get_physical("objects/sample.bin").unwrap(),
            b"hello world"
        );
        assert_eq!(
            blob_store
                .get_physical_range("objects/sample.bin", 6, 5)
                .unwrap(),
            b"world"
        );
    }

    #[test]
    fn blob_store_stat_reports_object_size() {
        let temp = tempdir().unwrap();
        let backend = LocalFolderBackend::new(temp.path());
        let blob_store: &dyn BlobStore = &backend;
        blob_store
            .put_physical("objects/sample.bin", b"hello world")
            .unwrap();

        let stat = blob_store.stat_physical("objects/sample.bin").unwrap();

        assert_eq!(stat.length, 11);
    }

    #[test]
    fn blob_store_delete_removes_existing_object() {
        let temp = tempdir().unwrap();
        let backend = LocalFolderBackend::new(temp.path());
        let blob_store: &dyn BlobStore = &backend;
        blob_store
            .put_physical("objects/sample.bin", b"hello world")
            .unwrap();

        blob_store.delete_physical("objects/sample.bin").unwrap();

        assert!(!blob_store.exists_physical("objects/sample.bin"));
    }

    #[test]
    fn blob_store_delete_removes_directory_path_conflict() {
        let temp = tempdir().unwrap();
        let backend = LocalFolderBackend::new(temp.path());
        let blob_store: &dyn BlobStore = &backend;
        let conflicted_path = temp.path().join("objects").join("sample.bin");
        fs::create_dir_all(&conflicted_path).unwrap();

        blob_store.delete_physical("objects/sample.bin").unwrap();

        assert!(!conflicted_path.exists());
    }

    #[test]
    fn blob_store_list_returns_prefixed_objects() {
        let temp = tempdir().unwrap();
        let backend = LocalFolderBackend::new(temp.path());
        let blob_store: &dyn BlobStore = &backend;
        blob_store.put_physical("objects/a.bin", b"a").unwrap();
        blob_store.put_physical("objects/b.bin", b"b").unwrap();
        blob_store.put_physical("other/c.bin", b"c").unwrap();

        let listed = blob_store.list_physical("objects/").unwrap();

        assert_eq!(listed.len(), 2);
        assert!(listed.iter().any(|path| path == "objects/a.bin"));
        assert!(listed.iter().any(|path| path == "objects/b.bin"));
    }

    #[test]
    fn put_object_rejects_parent_dir_traversal_outside_repo_root() {
        let temp = tempdir().unwrap();
        let repo_root = temp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();
        let outside = temp.path().join("keep.txt");
        fs::write(&outside, b"keep").unwrap();
        let backend = LocalFolderBackend::new(&repo_root);

        let error = backend.put_object("../keep.txt", b"overwrite").unwrap_err();

        assert!(
            error.to_string().contains("path traversal") || error.to_string().contains("repo root")
        );
        assert_eq!(fs::read(&outside).unwrap(), b"keep");
    }

    #[test]
    fn put_object_rejects_absolute_paths_outside_repo_root() {
        let temp = tempdir().unwrap();
        let repo_root = temp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();
        let outside = temp.path().join("keep.txt");
        fs::write(&outside, b"keep").unwrap();
        let backend = LocalFolderBackend::new(&repo_root);

        let error = backend
            .put_object(outside.to_str().unwrap(), b"overwrite")
            .unwrap_err();

        assert!(error.to_string().contains("relative") || error.to_string().contains("repo root"));
        assert_eq!(fs::read(&outside).unwrap(), b"keep");
    }

    #[test]
    fn put_object_rejects_backslash_traversal_segments() {
        let temp = tempdir().unwrap();
        let repo_root = temp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();
        let backend = LocalFolderBackend::new(&repo_root);

        let error = backend
            .put_object("objects\\..\\keep.txt", b"overwrite")
            .unwrap_err();

        assert!(
            error.to_string().contains("path traversal") || error.to_string().contains("separator"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn put_object_replaces_target_path_conflict_with_file_contents() {
        let temp = tempdir().unwrap();
        let backend = LocalFolderBackend::new(temp.path());
        let conflicted_path = temp.path().join("objects").join("sample.bin");
        fs::create_dir_all(&conflicted_path).unwrap();

        backend
            .put_object("objects/sample.bin", b"hello world")
            .unwrap();

        assert!(conflicted_path.is_file());
        assert_eq!(fs::read(&conflicted_path).unwrap(), b"hello world");
    }

    #[test]
    fn put_object_replaces_parent_path_conflict_with_directory_tree() {
        let temp = tempdir().unwrap();
        let backend = LocalFolderBackend::new(temp.path());
        let conflicted_parent = temp.path().join("objects");
        fs::write(&conflicted_parent, b"not-a-directory").unwrap();

        backend
            .put_object("objects/sample.bin", b"hello world")
            .unwrap();

        let target_path = conflicted_parent.join("sample.bin");
        assert!(conflicted_parent.is_dir());
        assert!(target_path.is_file());
        assert_eq!(fs::read(&target_path).unwrap(), b"hello world");
    }

    #[test]
    fn put_physical_if_absent_treats_target_directory_conflict_as_existing() {
        let temp = tempdir().unwrap();
        let backend = LocalFolderBackend::new(temp.path());
        let blob_store: &dyn BlobStore = &backend;
        let conflicted_path = temp.path().join("objects").join("sample.bin");
        fs::create_dir_all(&conflicted_path).unwrap();

        let created = blob_store
            .put_physical_if_absent("objects/sample.bin", b"hello world")
            .unwrap();

        assert!(!created);
        assert!(conflicted_path.is_dir());
    }

    #[test]
    fn put_physical_if_absent_heals_parent_path_conflict_before_create() {
        let temp = tempdir().unwrap();
        let backend = LocalFolderBackend::new(temp.path());
        let blob_store: &dyn BlobStore = &backend;
        let conflicted_parent = temp.path().join("objects");
        fs::write(&conflicted_parent, b"not-a-directory").unwrap();

        let created = blob_store
            .put_physical_if_absent("objects/sample.bin", b"hello world")
            .unwrap();

        let target_path = conflicted_parent.join("sample.bin");
        assert!(created);
        assert!(conflicted_parent.is_dir());
        assert!(target_path.is_file());
        assert_eq!(fs::read(&target_path).unwrap(), b"hello world");
    }

    #[test]
    fn local_folder_backend_defaults_to_safe_single_writer_capability() {
        let temp = tempdir().unwrap();
        let backend = LocalFolderBackend::new(temp.path());

        assert_eq!(backend.capability().writer_mode(), WriterMode::SingleWriter);
        assert_eq!(
            backend.capability().push_write_mode(),
            WriterMode::SingleWriter
        );
        assert!(backend.capability().supports_safe_single_writer_push());
    }

    #[test]
    fn list_refs_includes_nested_tokens() {
        let temp = tempdir().unwrap();
        let backend = LocalFolderBackend::new(temp.path());
        let token = RefToken::new("keyring/repo-123".to_string());
        let value = EncryptedRef::new(br#"{"generation":1,"current":"keyring.1"}"#.to_vec());

        let cas = backend
            .compare_and_swap_ref(&token, None, value.clone())
            .unwrap();

        assert!(cas.applied);
        assert_eq!(backend.read_ref(&token).unwrap().unwrap().value, value);
        assert!(
            backend
                .list_refs()
                .unwrap()
                .iter()
                .any(|listed| listed.token == token),
            "nested ref token should be discoverable via list_refs"
        );
    }

    #[test]
    fn list_refs_rejects_invalid_physical_ref_tokens() {
        let temp = tempdir().unwrap();
        let backend = LocalFolderBackend::new(temp.path());
        let stored = StoredRef {
            version: RefVersion { value: 1 },
            value: EncryptedRef::new(vec![1, 2, 3]),
        };
        let invalid_path = temp
            .path()
            .join("control")
            .join("refs")
            .join("by-token")
            .join(".json");
        fs::create_dir_all(invalid_path.parent().unwrap()).unwrap();
        fs::write(&invalid_path, serde_json::to_vec(&stored).unwrap()).unwrap();

        let error = backend.list_refs().unwrap_err();

        assert!(
            error.to_string().contains("ref token")
                || error.to_string().contains("path")
                || error.to_string().contains("empty"),
            "unexpected error: {error:#}"
        );
    }
}
