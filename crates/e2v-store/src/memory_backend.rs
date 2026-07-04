use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use anyhow::Result;

use anyhow::Context;

use crate::capability::{BackendCapability, ConsistencyClass};
use crate::layout::LayoutRoot;
use crate::layout_root_store::{LayoutRootStore, LayoutRootVersion};
use crate::local_backend::{BlobStore, ObjectStat, is_missing_physical_object_error};
use crate::ref_store::{
    CasResult, EncryptedRef, ListedRef, RefStore, RefToken, RefVersion, StoredRef,
};

#[derive(Debug, Clone)]
pub struct MemoryBackend {
    physical: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    physical_modified_at: Arc<Mutex<HashMap<String, SystemTime>>>,
    layout_root: Arc<Mutex<LayoutRoot>>,
    capability: BackendCapability,
}

impl Default for MemoryBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryBackend {
    fn default_layout_root() -> LayoutRoot {
        LayoutRoot::direct_default()
    }

    fn layout_history_path(generation: u64) -> String {
        format!("control/layout-roots/{generation:020}.json")
    }

    fn ref_path(token: &RefToken) -> String {
        format!("control/refs/by-token/{}.json", token.value)
    }

    pub fn new() -> Self {
        let layout_root = Self::default_layout_root();
        Self {
            physical: Arc::new(Mutex::new(HashMap::new())),
            physical_modified_at: Arc::new(Mutex::new(HashMap::new())),
            layout_root: Arc::new(Mutex::new(layout_root)),
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
                supports_oblivious_access_schedule: true,
            },
        }
    }

    pub fn capability(&self) -> &BackendCapability {
        &self.capability
    }

    fn lock_or_recover<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn override_physical_modified_time_for_test(
        &self,
        relative_path: &str,
        modified_at: SystemTime,
    ) -> Result<()> {
        anyhow::ensure!(
            self.exists_physical(relative_path),
            "cannot override modified time for missing physical object {relative_path}"
        );
        self.physical_modified_at
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(relative_path.to_string(), modified_at);
        Ok(())
    }
}

impl RefStore for MemoryBackend {
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
        next: EncryptedRef,
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

        let next_version = RefVersion {
            value: current
                .as_ref()
                .map(|stored| stored.version.value + 1)
                .unwrap_or(1),
        };
        let stored = StoredRef {
            version: next_version,
            value: next,
        };
        self.put_physical(&Self::ref_path(token), &serde_json::to_vec(&stored)?)?;
        Ok(CasResult {
            applied: true,
            current: Some(stored),
        })
    }
}

impl BlobStore for MemoryBackend {
    fn put_physical(&self, relative_path: &str, bytes: &[u8]) -> Result<()> {
        self.physical
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(relative_path.to_string(), bytes.to_vec());
        self.physical_modified_at
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(relative_path.to_string(), SystemTime::now());
        Ok(())
    }

    fn put_physical_if_absent(&self, relative_path: &str, bytes: &[u8]) -> Result<bool> {
        let mut physical = Self::lock_or_recover(&self.physical);
        if physical.contains_key(relative_path) {
            return Ok(false);
        }
        physical.insert(relative_path.to_string(), bytes.to_vec());
        drop(physical);
        self.physical_modified_at
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(relative_path.to_string(), SystemTime::now());
        Ok(true)
    }

    fn get_physical(&self, relative_path: &str) -> Result<Vec<u8>> {
        self.physical
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(relative_path)
            .cloned()
            .with_context(|| format!("missing physical object {relative_path}"))
    }

    fn get_physical_range(
        &self,
        relative_path: &str,
        offset: usize,
        length: usize,
    ) -> Result<Vec<u8>> {
        let bytes = self.get_physical(relative_path)?;
        anyhow::ensure!(offset <= bytes.len(), "range offset out of bounds");
        let end = offset.saturating_add(length).min(bytes.len());
        Ok(bytes[offset..end].to_vec())
    }

    fn delete_physical(&self, relative_path: &str) -> Result<()> {
        Self::lock_or_recover(&self.physical).remove(relative_path);
        self.physical_modified_at
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(relative_path);
        Ok(())
    }

    fn exists_physical(&self, relative_path: &str) -> bool {
        Self::lock_or_recover(&self.physical).contains_key(relative_path)
    }

    fn stat_physical(&self, relative_path: &str) -> Result<ObjectStat> {
        let bytes = self.get_physical(relative_path)?;
        Ok(ObjectStat {
            length: bytes.len() as u64,
            modified_at: self
                .physical_modified_at
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(relative_path)
                .copied(),
        })
    }

    fn list_physical(&self, prefix: &str) -> Result<Vec<String>> {
        let mut listed = self
            .physical
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .keys()
            .filter(|path| path.starts_with(prefix))
            .cloned()
            .collect::<Vec<_>>();
        listed.sort();
        Ok(listed)
    }
}

impl LayoutRootStore for MemoryBackend {
    fn read_layout_root(&self) -> Result<LayoutRoot> {
        match self.get_physical("layout_root.json") {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).context("failed to decode remote layout root")
            }
            Err(error) if is_missing_physical_object_error(&error) => {
                Ok(Self::lock_or_recover(&self.layout_root).clone())
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
        *Self::lock_or_recover(&self.layout_root) = next.clone();
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

#[cfg(test)]
mod tests {
    use std::panic::{self, AssertUnwindSafe};

    use crate::capability::WriterMode;

    use super::*;

    fn poison_mutex<T>(mutex: &Arc<Mutex<T>>) {
        let poisoned = Arc::clone(mutex);
        let _ = panic::catch_unwind(AssertUnwindSafe(move || {
            let _guard = poisoned.lock().unwrap();
            panic!("poison test mutex");
        }));
    }

    #[test]
    fn compare_and_swap_ref_rejects_stale_version() {
        let backend = MemoryBackend::new();
        let token = RefToken::new("branch-token".to_string());
        let first = EncryptedRef::new(vec![1, 2, 3]);
        let second = EncryptedRef::new(vec![4, 5, 6]);

        let initial = backend
            .compare_and_swap_ref(&token, None, first.clone())
            .unwrap();
        assert!(initial.applied);

        let stale = backend.compare_and_swap_ref(&token, None, second).unwrap();
        assert!(!stale.applied);
        assert_eq!(stale.current.unwrap().value, first);
    }

    #[test]
    fn compare_and_swap_layout_root_rejects_stale_generation() {
        let backend = MemoryBackend::new();
        let stale = backend
            .compare_and_swap_layout_root(
                99,
                LayoutRoot {
                    generation: 2,
                    ..LayoutRoot::direct_default()
                },
            )
            .unwrap();

        assert!(!stale.applied);
        assert_eq!(backend.read_layout_root().unwrap().generation, 1);
    }

    #[test]
    fn compare_and_swap_ref_materializes_physical_ref_file() {
        let backend = MemoryBackend::new();
        let token = RefToken::new("branches/main".to_string());
        let next = EncryptedRef::new(vec![1, 2, 3]);

        let result = backend
            .compare_and_swap_ref(&token, None, next.clone())
            .unwrap();

        assert!(result.applied);
        assert_eq!(
            backend
                .get_physical("control/refs/by-token/branches/main.json")
                .unwrap(),
            serde_json::to_vec(&StoredRef {
                version: RefVersion { value: 1 },
                value: next,
            })
            .unwrap()
        );
    }

    #[test]
    fn read_ref_uses_physical_ref_bytes_when_present() {
        let backend = MemoryBackend::new();
        let token = RefToken::new("branches/main".to_string());
        let stored = StoredRef {
            version: RefVersion { value: 7 },
            value: EncryptedRef::new(vec![9, 8, 7]),
        };
        backend
            .put_physical(
                "control/refs/by-token/branches/main.json",
                &serde_json::to_vec(&stored).unwrap(),
            )
            .unwrap();

        assert_eq!(backend.read_ref(&token).unwrap(), Some(stored));
    }

    #[test]
    fn list_refs_uses_physical_ref_files_when_present() {
        let backend = MemoryBackend::new();
        let alpha = StoredRef {
            version: RefVersion { value: 2 },
            value: EncryptedRef::new(vec![1]),
        };
        let zeta = StoredRef {
            version: RefVersion { value: 3 },
            value: EncryptedRef::new(vec![2]),
        };
        backend
            .put_physical(
                "control/refs/by-token/branches/zeta.json",
                &serde_json::to_vec(&zeta).unwrap(),
            )
            .unwrap();
        backend
            .put_physical(
                "control/refs/by-token/branches/alpha.json",
                &serde_json::to_vec(&alpha).unwrap(),
            )
            .unwrap();

        let listed = backend.list_refs().unwrap();

        assert_eq!(
            listed
                .into_iter()
                .map(|entry| (entry.token.value, entry.stored))
                .collect::<Vec<_>>(),
            vec![
                ("branches/alpha".to_string(), alpha),
                ("branches/zeta".to_string(), zeta),
            ]
        );
    }

    #[test]
    fn compare_and_swap_layout_root_materializes_physical_layout_root_file() {
        let backend = MemoryBackend::new();
        let next = LayoutRoot {
            generation: 2,
            ..LayoutRoot::direct_default()
        };

        let result = backend
            .compare_and_swap_layout_root(1, next.clone())
            .unwrap();

        assert!(result.applied);
        assert_eq!(
            backend.get_physical("layout_root.json").unwrap(),
            serde_json::to_vec(&next).unwrap()
        );
    }

    #[test]
    fn compare_and_swap_layout_root_materializes_physical_layout_root_history_file() {
        let backend = MemoryBackend::new();
        let next = LayoutRoot {
            generation: 2,
            ..LayoutRoot::direct_default()
        };

        let result = backend
            .compare_and_swap_layout_root(1, next.clone())
            .unwrap();

        assert!(result.applied);
        assert_eq!(
            backend
                .get_physical("control/layout-roots/00000000000000000002.json")
                .unwrap(),
            serde_json::to_vec(&next).unwrap()
        );
    }

    #[test]
    fn read_layout_root_uses_physical_layout_root_bytes_when_present() {
        let backend = MemoryBackend::new();
        let physical = LayoutRoot {
            generation: 7,
            ..LayoutRoot::direct_default()
        };
        backend
            .put_physical("layout_root.json", &serde_json::to_vec(&physical).unwrap())
            .unwrap();

        assert_eq!(backend.read_layout_root().unwrap(), physical);
    }

    #[test]
    fn list_retained_layout_roots_uses_physical_history_when_present() {
        let backend = MemoryBackend::new();
        let generation_two = LayoutRoot {
            generation: 2,
            ..LayoutRoot::direct_default()
        };
        let generation_three = LayoutRoot {
            generation: 3,
            ..LayoutRoot::direct_default()
        };
        backend
            .put_physical(
                "control/layout-roots/00000000000000000002.json",
                &serde_json::to_vec(&generation_two).unwrap(),
            )
            .unwrap();
        backend
            .put_physical(
                "control/layout-roots/00000000000000000003.json",
                &serde_json::to_vec(&generation_three).unwrap(),
            )
            .unwrap();

        assert_eq!(
            backend.list_retained_layout_roots().unwrap(),
            vec![generation_two, generation_three]
        );
    }

    #[test]
    fn list_retained_layout_roots_falls_back_to_physical_current_layout_root_when_history_is_missing()
     {
        let backend = MemoryBackend::new();
        let physical = LayoutRoot {
            generation: 7,
            ..LayoutRoot::direct_default()
        };
        backend
            .put_physical("layout_root.json", &serde_json::to_vec(&physical).unwrap())
            .unwrap();

        assert_eq!(
            backend.list_retained_layout_roots().unwrap(),
            vec![physical]
        );
    }

    #[test]
    fn backend_capability_prefers_multi_writer_when_conditional_put_exists() {
        let backend = MemoryBackend::new();

        assert_eq!(backend.capability().writer_mode(), WriterMode::MultiWriter);
    }

    #[test]
    fn list_refs_returns_tokens_in_sorted_order() {
        let backend = MemoryBackend::new();
        backend
            .compare_and_swap_ref(
                &RefToken::new("branches/zeta".to_string()),
                None,
                EncryptedRef::new(vec![1]),
            )
            .unwrap();
        backend
            .compare_and_swap_ref(
                &RefToken::new("branches/alpha".to_string()),
                None,
                EncryptedRef::new(vec![2]),
            )
            .unwrap();

        let listed = backend.list_refs().unwrap();

        assert_eq!(
            listed
                .into_iter()
                .map(|entry| entry.token.value)
                .collect::<Vec<_>>(),
            vec!["branches/alpha".to_string(), "branches/zeta".to_string()]
        );
    }

    #[test]
    fn list_refs_rejects_invalid_physical_ref_tokens() {
        let backend = MemoryBackend::new();
        let stored = StoredRef {
            version: RefVersion { value: 1 },
            value: EncryptedRef::new(vec![1, 2, 3]),
        };
        backend
            .put_physical(
                "control/refs/by-token/../evil.json",
                &serde_json::to_vec(&stored).unwrap(),
            )
            .unwrap();

        let error = backend.list_refs().unwrap_err();

        assert!(
            error.to_string().contains("ref token") || error.to_string().contains("path"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn delete_physical_removes_existing_object() {
        let backend = MemoryBackend::new();
        backend
            .put_physical("objects/sample.bin", b"hello world")
            .unwrap();

        backend.delete_physical("objects/sample.bin").unwrap();

        assert!(!backend.exists_physical("objects/sample.bin"));
    }

    #[test]
    fn compare_and_swap_ref_rejects_path_traversal_token() {
        let backend = MemoryBackend::new();
        let error = backend
            .compare_and_swap_ref(
                &RefToken::new("../evil".to_string()),
                None,
                EncryptedRef::new(vec![1, 2, 3]),
            )
            .unwrap_err();

        assert!(error.to_string().contains("path"));
        assert!(
            backend
                .read_ref(&RefToken::new("../evil".to_string()))
                .unwrap_err()
                .to_string()
                .contains("path")
        );
    }

    #[test]
    fn compare_and_swap_ref_rejects_backslash_traversal_token() {
        let backend = MemoryBackend::new();
        let error = backend
            .compare_and_swap_ref(
                &RefToken::new("..\\evil".to_string()),
                None,
                EncryptedRef::new(vec![1, 2, 3]),
            )
            .unwrap_err();

        assert!(error.to_string().contains("path"));
        assert!(
            backend
                .read_ref(&RefToken::new("..\\evil".to_string()))
                .unwrap_err()
                .to_string()
                .contains("path")
        );
    }

    #[test]
    fn physical_operations_recover_from_poisoned_storage_lock() {
        let backend = MemoryBackend::new();
        backend
            .put_physical("objects/sample.bin", b"hello")
            .unwrap();

        poison_mutex(&backend.physical);

        backend.put_physical("objects/next.bin", b"world").unwrap();
        assert_eq!(
            backend.get_physical("objects/sample.bin").unwrap(),
            b"hello"
        );
        assert!(backend.exists_physical("objects/next.bin"));
        assert_eq!(
            backend.list_physical("objects/").unwrap(),
            vec![
                "objects/next.bin".to_string(),
                "objects/sample.bin".to_string()
            ]
        );
        backend.delete_physical("objects/sample.bin").unwrap();
        assert!(!backend.exists_physical("objects/sample.bin"));
    }

    #[test]
    fn layout_root_operations_recover_from_poisoned_layout_lock() {
        let backend = MemoryBackend::new();
        poison_mutex(&backend.layout_root);

        let current = backend.read_layout_root().unwrap();
        assert_eq!(current.generation, 1);

        let next = LayoutRoot {
            generation: 2,
            ..LayoutRoot::direct_default()
        };
        let result = backend
            .compare_and_swap_layout_root(1, next.clone())
            .unwrap();
        assert!(result.applied);
        assert_eq!(backend.read_layout_root().unwrap(), next);
    }

    #[test]
    fn stat_recover_from_poisoned_modified_time_lock() {
        let backend = MemoryBackend::new();
        backend
            .put_physical("objects/sample.bin", b"hello")
            .unwrap();

        poison_mutex(&backend.physical_modified_at);

        let stat = backend.stat_physical("objects/sample.bin").unwrap();
        assert_eq!(stat.length, 5);
        assert!(stat.modified_at.is_some());
    }
}
