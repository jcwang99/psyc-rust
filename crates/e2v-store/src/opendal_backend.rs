use anyhow::Result;

use crate::{
    BackendCapability, BlobStore, CasResult, ConsistencyClass, EncryptedRef, LayoutRoot,
    LayoutRootStore, LayoutRootVersion, ObjectStat, RefStore, RefToken, RefVersion, StoredRef,
};

pub trait RemoteBackend: BlobStore + RefStore + LayoutRootStore + Send + Sync {
    fn capability(&self) -> &BackendCapability;
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

impl RemoteBackend for S3CompatibleMockBackend {
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
}
