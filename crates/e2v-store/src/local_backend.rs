use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, ensure};

use crate::capability::{BackendCapability, ConsistencyClass};
use crate::layout::LayoutRoot;
use crate::layout_root_store::{LayoutRootStore, LayoutRootVersion};
use crate::opendal_backend::RemoteBackend;
use crate::ref_store::{CasResult, ListedRef, RefStore, RefToken, RefVersion, StoredRef};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalFolderBackend {
    repo_root: PathBuf,
}

impl LocalFolderBackend {
    pub fn new(repo_root: impl AsRef<Path>) -> Self {
        Self {
            repo_root: repo_root.as_ref().to_path_buf(),
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
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create object parent {}", parent.display()))?;
        }
        fs::write(&full_path, bytes)
            .with_context(|| format!("failed to write object {}", full_path.display()))?;
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
}

impl BlobStore for LocalFolderBackend {
    fn put_physical(&self, relative_path: &str, bytes: &[u8]) -> Result<()> {
        self.put_object(relative_path, bytes)
    }

    fn put_physical_if_absent(&self, relative_path: &str, bytes: &[u8]) -> Result<bool> {
        let full_path = self.resolve_relative_path(relative_path)?;
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create object parent {}", parent.display()))?;
        }
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&full_path)
        {
            Ok(mut file) => {
                use std::io::Write;
                file.write_all(bytes)
                    .with_context(|| format!("failed to write object {}", full_path.display()))?;
                Ok(true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(error) => Err(error)
                .with_context(|| format!("failed to create object {}", full_path.display())),
        }
    }

    fn get_physical(&self, relative_path: &str) -> Result<Vec<u8>> {
        self.get_object(relative_path)
    }

    fn get_physical_range(
        &self,
        relative_path: &str,
        offset: usize,
        length: usize,
    ) -> Result<Vec<u8>> {
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
        file.read_exact(&mut bytes)
            .with_context(|| format!("failed to read object range {}", full_path.display()))?;
        Ok(bytes)
    }

    fn delete_physical(&self, relative_path: &str) -> Result<()> {
        let full_path = self.resolve_relative_path(relative_path)?;
        match fs::remove_file(&full_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error)
                .with_context(|| format!("failed to delete object {}", full_path.display())),
        }
    }

    fn exists_physical(&self, relative_path: &str) -> bool {
        self.exists_object(relative_path)
    }

    fn stat_physical(&self, relative_path: &str) -> Result<ObjectStat> {
        let full_path = self.resolve_relative_path(relative_path)?;
        let metadata = fs::metadata(&full_path)
            .with_context(|| format!("failed to stat object {}", full_path.display()))?;
        Ok(ObjectStat {
            length: metadata.len(),
            modified_at: metadata.modified().ok(),
        })
    }

    fn list_physical(&self, prefix: &str) -> Result<Vec<String>> {
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
            for entry in fs::read_dir(current)
                .with_context(|| format!("failed to list objects under {}", current.display()))?
            {
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
    }
}

impl RefStore for LocalFolderBackend {
    fn read_ref(&self, token: &RefToken) -> Result<Option<StoredRef>> {
        token.validate()?;
        let path = Self::ref_path(token);
        match self.get_physical(&path) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
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
                Ok(ListedRef {
                    token: RefToken::new(token),
                    stored: serde_json::from_slice(&self.get_physical(&path)?)?,
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
    use crate::ref_store::{EncryptedRef, RefStore, RefToken};

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
}
