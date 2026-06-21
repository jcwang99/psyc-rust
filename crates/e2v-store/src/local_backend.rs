use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub trait BlobStore {
    fn put_physical(&self, relative_path: &str, bytes: &[u8]) -> Result<()>;
    fn get_physical(&self, relative_path: &str) -> Result<Vec<u8>>;
    fn get_physical_range(&self, relative_path: &str, offset: usize, length: usize) -> Result<Vec<u8>>;
    fn exists_physical(&self, relative_path: &str) -> bool;
    fn stat_physical(&self, relative_path: &str) -> Result<ObjectStat>;
    fn list_physical(&self, prefix: &str) -> Result<Vec<String>>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectStat {
    pub length: u64,
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

    pub fn put_object(&self, relative_path: &str, bytes: &[u8]) -> Result<()> {
        let full_path = self.repo_root.join(relative_path);
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create object parent {}", parent.display()))?;
        }
        fs::write(&full_path, bytes)
            .with_context(|| format!("failed to write object {}", full_path.display()))?;
        Ok(())
    }

    pub fn get_object(&self, relative_path: &str) -> Result<Vec<u8>> {
        let full_path = self.repo_root.join(relative_path);
        fs::read(&full_path)
            .with_context(|| format!("failed to read object {}", full_path.display()))
    }

    pub fn exists_object(&self, relative_path: &str) -> bool {
        self.repo_root.join(relative_path).is_file()
    }
}

impl BlobStore for LocalFolderBackend {
    fn put_physical(&self, relative_path: &str, bytes: &[u8]) -> Result<()> {
        self.put_object(relative_path, bytes)
    }

    fn get_physical(&self, relative_path: &str) -> Result<Vec<u8>> {
        self.get_object(relative_path)
    }

    fn get_physical_range(&self, relative_path: &str, offset: usize, length: usize) -> Result<Vec<u8>> {
        let bytes = self.get_object(relative_path)?;
        anyhow::ensure!(offset <= bytes.len(), "range offset out of bounds");
        let end = offset.saturating_add(length).min(bytes.len());
        Ok(bytes[offset..end].to_vec())
    }

    fn exists_physical(&self, relative_path: &str) -> bool {
        self.exists_object(relative_path)
    }

    fn stat_physical(&self, relative_path: &str) -> Result<ObjectStat> {
        let full_path = self.repo_root.join(relative_path);
        let metadata = fs::metadata(&full_path)
            .with_context(|| format!("failed to stat object {}", full_path.display()))?;
        Ok(ObjectStat {
            length: metadata.len(),
        })
    }

    fn list_physical(&self, prefix: &str) -> Result<Vec<String>> {
        let base = self.repo_root.join(prefix);
        let mut listed = Vec::new();
        if !base.exists() {
            return Ok(listed);
        }

        for entry in fs::read_dir(&base)
            .with_context(|| format!("failed to list objects under {}", base.display()))?
        {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                let relative = format!(
                    "{}{}",
                    prefix,
                    entry.file_name().to_string_lossy()
                );
                listed.push(relative);
            }
        }
        listed.sort();
        Ok(listed)
    }
}

#[cfg(test)]
mod tests {
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

        blob_store.put_physical("objects/sample.bin", b"hello world").unwrap();
        assert!(blob_store.exists_physical("objects/sample.bin"));
        assert_eq!(blob_store.get_physical("objects/sample.bin").unwrap(), b"hello world");
        assert_eq!(blob_store.get_physical_range("objects/sample.bin", 6, 5).unwrap(), b"world");
    }

    #[test]
    fn blob_store_stat_reports_object_size() {
        let temp = tempdir().unwrap();
        let backend = LocalFolderBackend::new(temp.path());
        let blob_store: &dyn BlobStore = &backend;
        blob_store.put_physical("objects/sample.bin", b"hello world").unwrap();

        let stat = blob_store.stat_physical("objects/sample.bin").unwrap();

        assert_eq!(stat.length, 11);
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
}
