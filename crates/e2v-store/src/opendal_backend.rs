use std::sync::LazyLock;

use anyhow::Result;

use crate::{
    BackendCapability, BlobStore, CasResult, ConsistencyClass, EncryptedRef, LayoutRoot,
    LayoutRootStore, LayoutRootVersion, ObjectStat, RefStore, RefToken, RefVersion, StoredRef,
};

pub trait RemoteBackend: BlobStore + RefStore + LayoutRootStore + Send + Sync {
    fn capability(&self) -> &BackendCapability;
}

static OPENDAL_RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build opendal runtime")
});

#[derive(Clone)]
pub struct OpendalMemoryBackend {
    operator: opendal::blocking::Operator,
    capability: BackendCapability,
}

impl OpendalMemoryBackend {
    pub fn new() -> Result<Self> {
        let _guard = OPENDAL_RUNTIME.enter();
        Ok(Self::from_operator(opendal::blocking::Operator::new(
            opendal::Operator::new(opendal::services::Memory::default())?.finish(),
        )?))
    }

    fn from_operator(operator: opendal::blocking::Operator) -> Self {
        Self {
            operator,
            capability: BackendCapability {
                supports_conditional_put: false,
                supports_range_read: true,
                supports_atomic_rename: false,
                supports_paged_list: true,
                consistency_class: ConsistencyClass::UnknownOrEventual,
                supports_remote_lock_or_lease: false,
                supports_transaction_markers: false,
                supports_reliable_remote_time: false,
                supports_object_generation_or_etag: false,
                supports_layout_root_cas: false,
                supports_oblivious_access_schedule: false,
            },
        }
    }

    fn default_layout_root() -> LayoutRoot {
        LayoutRoot {
            schema_version: 1,
            layout_id: "direct".to_string(),
            generation: 1,
            mapping_policy: "loose".to_string(),
        }
    }

    fn ref_path(token: &RefToken) -> String {
        format!("control/refs/by-token/{}.json", token.value)
    }

    fn layout_history_path(generation: u64) -> String {
        format!("control/layout-roots/{generation:020}.json")
    }
}

#[derive(Debug, Clone)]
pub struct S3CompatibleMockBackend {
    inner: crate::memory_backend::MemoryBackend,
    capability: BackendCapability,
}

impl S3CompatibleMockBackend {
    pub fn new() -> Self {
        Self {
            inner: crate::memory_backend::MemoryBackend::new(),
            capability: BackendCapability {
                supports_conditional_put: true,
                supports_range_read: true,
                supports_atomic_rename: false,
                supports_paged_list: true,
                consistency_class: ConsistencyClass::UnknownOrEventual,
                supports_remote_lock_or_lease: true,
                supports_transaction_markers: true,
                supports_reliable_remote_time: false,
                supports_object_generation_or_etag: true,
                supports_layout_root_cas: true,
                supports_oblivious_access_schedule: false,
            },
        }
    }
}

impl BlobStore for S3CompatibleMockBackend {
    fn put_physical(&self, relative_path: &str, bytes: &[u8]) -> Result<()> {
        self.inner.put_physical(relative_path, bytes)
    }

    fn get_physical(&self, relative_path: &str) -> Result<Vec<u8>> {
        self.inner.get_physical(relative_path)
    }

    fn get_physical_range(&self, relative_path: &str, offset: usize, length: usize) -> Result<Vec<u8>> {
        self.inner.get_physical_range(relative_path, offset, length)
    }

    fn delete_physical(&self, relative_path: &str) -> Result<()> {
        self.inner.delete_physical(relative_path)
    }

    fn exists_physical(&self, relative_path: &str) -> bool {
        self.inner.exists_physical(relative_path)
    }

    fn stat_physical(&self, relative_path: &str) -> Result<ObjectStat> {
        self.inner.stat_physical(relative_path)
    }

    fn list_physical(&self, prefix: &str) -> Result<Vec<String>> {
        self.inner.list_physical(prefix)
    }
}

impl BlobStore for OpendalMemoryBackend {
    fn put_physical(&self, relative_path: &str, bytes: &[u8]) -> Result<()> {
        self.operator.write(relative_path, bytes.to_vec())?;
        Ok(())
    }

    fn get_physical(&self, relative_path: &str) -> Result<Vec<u8>> {
        Ok(self.operator.read(relative_path)?.to_vec())
    }

    fn get_physical_range(&self, relative_path: &str, offset: usize, length: usize) -> Result<Vec<u8>> {
        let bytes = self.get_physical(relative_path)?;
        anyhow::ensure!(offset <= bytes.len(), "range offset out of bounds");
        let end = offset.saturating_add(length).min(bytes.len());
        Ok(bytes[offset..end].to_vec())
    }

    fn delete_physical(&self, relative_path: &str) -> Result<()> {
        if self.exists_physical(relative_path) {
            self.operator.delete(relative_path)?;
        }
        Ok(())
    }

    fn exists_physical(&self, relative_path: &str) -> bool {
        self.operator.exists(relative_path).unwrap_or(false)
    }

    fn stat_physical(&self, relative_path: &str) -> Result<ObjectStat> {
        let metadata = self.operator.stat(relative_path)?;
        Ok(ObjectStat {
            length: metadata.content_length(),
        })
    }

    fn list_physical(&self, prefix: &str) -> Result<Vec<String>> {
        let mut listed = self
            .operator
            .list(prefix)?
            .into_iter()
            .filter_map(|entry: opendal::Entry| {
                let path = entry.path().to_string();
                if path == prefix || path.ends_with('/') {
                    None
                } else {
                    Some(path)
                }
            })
            .collect::<Vec<_>>();
        listed.sort();
        Ok(listed)
    }
}

impl RefStore for S3CompatibleMockBackend {
    fn read_ref(&self, token: &RefToken) -> Result<Option<StoredRef>> {
        self.inner.read_ref(token)
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

impl RefStore for OpendalMemoryBackend {
    fn read_ref(&self, token: &RefToken) -> Result<Option<StoredRef>> {
        let path = Self::ref_path(token);
        if !self.exists_physical(&path) {
            return Ok(None);
        }
        Ok(Some(serde_json::from_slice(&self.get_physical(&path)?)?))
    }

    fn compare_and_swap_ref(
        &self,
        token: &RefToken,
        expected: Option<RefVersion>,
        next: EncryptedRef,
    ) -> Result<CasResult> {
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
        self.put_physical(
            &Self::ref_path(token),
            &serde_json::to_vec_pretty(&stored)?,
        )?;
        Ok(CasResult {
            applied: true,
            current: Some(stored),
        })
    }
}

impl LayoutRootStore for S3CompatibleMockBackend {
    fn read_layout_root(&self) -> Result<LayoutRoot> {
        self.inner.read_layout_root()
    }

    fn compare_and_swap_layout_root(
        &self,
        expected: LayoutRootVersion,
        next: LayoutRoot,
    ) -> Result<CasResult> {
        self.inner.compare_and_swap_layout_root(expected, next)
    }

    fn list_retained_layout_roots(&self) -> Result<Vec<LayoutRoot>> {
        self.inner.list_retained_layout_roots()
    }
}

impl LayoutRootStore for OpendalMemoryBackend {
    fn read_layout_root(&self) -> Result<LayoutRoot> {
        if !self.exists_physical("layout_root.json") {
            return Ok(Self::default_layout_root());
        }
        Ok(serde_json::from_slice(&self.get_physical("layout_root.json")?)?)
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

        let bytes = serde_json::to_vec_pretty(&next)?;
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

impl RemoteBackend for S3CompatibleMockBackend {
    fn capability(&self) -> &BackendCapability {
        &self.capability
    }
}

impl RemoteBackend for OpendalMemoryBackend {
    fn capability(&self) -> &BackendCapability {
        &self.capability
    }
}

impl RemoteBackend for crate::memory_backend::MemoryBackend {
    fn capability(&self) -> &BackendCapability {
        self.capability()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s3_compatible_backend_defaults_to_unknown_or_eventual_consistency() {
        let backend = S3CompatibleMockBackend::new();

        assert_eq!(
            backend.capability().consistency_class,
            ConsistencyClass::UnknownOrEventual
        );
    }

    #[test]
    fn s3_compatible_backend_negative_ref_cas_fails() {
        let backend = S3CompatibleMockBackend::new();
        let token = RefToken::new("branch-token".to_string());
        let first = EncryptedRef::new(vec![1, 2, 3]);
        let second = EncryptedRef::new(vec![4, 5, 6]);

        let initial = backend.compare_and_swap_ref(&token, None, first).unwrap();
        assert!(initial.applied);

        let stale = backend
            .compare_and_swap_ref(&token, Some(RefVersion { value: 99 }), second)
            .unwrap();
        assert!(!stale.applied);
    }

    #[test]
    fn s3_compatible_backend_lists_prefixed_objects() {
        let backend = S3CompatibleMockBackend::new();
        backend.put_physical("objects/a.bin", b"a").unwrap();
        backend.put_physical("objects/b.bin", b"b").unwrap();
        backend.put_physical("other/c.bin", b"c").unwrap();

        let listed = backend.list_physical("objects/").unwrap();

        assert_eq!(listed, vec!["objects/a.bin".to_string(), "objects/b.bin".to_string()]);
    }

    #[test]
    fn opendal_memory_backend_supports_blob_round_trip() {
        let backend = OpendalMemoryBackend::new().unwrap();
        backend.put_physical("objects/a.bin", b"abcdef").unwrap();

        assert!(backend.exists_physical("objects/a.bin"));
        assert_eq!(backend.get_physical("objects/a.bin").unwrap(), b"abcdef");
        assert_eq!(
            backend.get_physical_range("objects/a.bin", 2, 3).unwrap(),
            b"cde"
        );
        assert_eq!(
            backend.list_physical("objects/").unwrap(),
            vec!["objects/a.bin".to_string()]
        );
        assert_eq!(backend.stat_physical("objects/a.bin").unwrap().length, 6);
    }

    #[test]
    fn opendal_memory_backend_deletes_blob() {
        let backend = OpendalMemoryBackend::new().unwrap();
        backend.put_physical("objects/a.bin", b"abcdef").unwrap();

        backend.delete_physical("objects/a.bin").unwrap();

        assert!(!backend.exists_physical("objects/a.bin"));
    }

    fn shared_memory_operator() -> opendal::blocking::Operator {
        let _guard = OPENDAL_RUNTIME.enter();
        opendal::blocking::Operator::new(
            opendal::Operator::new(opendal::services::Memory::default())
                .unwrap()
                .finish(),
        )
        .unwrap()
    }

    #[test]
    fn independent_opendal_backends_share_ref_and_layout_state() {
        let operator = shared_memory_operator();
        let writer = OpendalMemoryBackend::from_operator(operator.clone());
        let reader = OpendalMemoryBackend::from_operator(operator);
        let token = RefToken::new("branch-token".to_string());
        let next_ref = EncryptedRef::new(vec![1, 2, 3]);

        let ref_result = writer
            .compare_and_swap_ref(&token, None, next_ref.clone())
            .unwrap();
        assert!(ref_result.applied);
        assert_eq!(
            reader.read_ref(&token).unwrap().unwrap().value,
            next_ref
        );

        let next_layout = LayoutRoot {
            schema_version: 1,
            layout_id: "direct".to_string(),
            generation: 2,
            mapping_policy: "loose".to_string(),
        };
        let layout_result = writer
            .compare_and_swap_layout_root(1, next_layout.clone())
            .unwrap();
        assert!(layout_result.applied);
        assert_eq!(reader.read_layout_root().unwrap(), next_layout);
        assert_eq!(
            reader
                .list_retained_layout_roots()
                .unwrap()
                .last()
                .cloned()
                .unwrap(),
            next_layout
        );
    }
}
