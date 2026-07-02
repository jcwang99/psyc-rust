use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use anyhow::Result;

use anyhow::Context;

use crate::capability::{BackendCapability, ConsistencyClass};
use crate::layout::LayoutRoot;
use crate::layout_root_store::{LayoutRootStore, LayoutRootVersion};
use crate::local_backend::{BlobStore, ObjectStat};
use crate::ref_store::{
    CasResult, EncryptedRef, ListedRef, RefStore, RefToken, RefVersion, StoredRef,
};

#[derive(Debug, Clone)]
pub struct MemoryBackend {
    physical: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    physical_modified_at: Arc<Mutex<HashMap<String, SystemTime>>>,
    refs: Arc<Mutex<HashMap<String, StoredRef>>>,
    layout_root: Arc<Mutex<LayoutRoot>>,
    retained_layout_roots: Arc<Mutex<Vec<LayoutRoot>>>,
    capability: BackendCapability,
}

impl Default for MemoryBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryBackend {
    pub fn new() -> Self {
        let layout_root = LayoutRoot {
            schema_version: 1,
            layout_id: "direct".to_string(),
            generation: 1,
            mapping_policy: "loose".to_string(),
        };
        Self {
            physical: Arc::new(Mutex::new(HashMap::new())),
            physical_modified_at: Arc::new(Mutex::new(HashMap::new())),
            refs: Arc::new(Mutex::new(HashMap::new())),
            layout_root: Arc::new(Mutex::new(layout_root.clone())),
            retained_layout_roots: Arc::new(Mutex::new(vec![layout_root])),
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

    pub fn capability(&self) -> &BackendCapability {
        &self.capability
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
            .unwrap()
            .insert(relative_path.to_string(), modified_at);
        Ok(())
    }
}

impl RefStore for MemoryBackend {
    fn read_ref(&self, token: &RefToken) -> Result<Option<StoredRef>> {
        token.validate()?;
        Ok(self.refs.lock().unwrap().get(&token.value).cloned())
    }

    fn list_refs(&self) -> Result<Vec<ListedRef>> {
        let mut listed = self
            .refs
            .lock()
            .unwrap()
            .iter()
            .map(|(token, stored)| ListedRef {
                token: RefToken::new(token.clone()),
                stored: stored.clone(),
            })
            .collect::<Vec<_>>();
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
        let mut refs = self.refs.lock().unwrap();
        let current = refs.get(&token.value).cloned();
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
        refs.insert(token.value.clone(), stored.clone());
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
            .unwrap()
            .insert(relative_path.to_string(), bytes.to_vec());
        self.physical_modified_at
            .lock()
            .unwrap()
            .insert(relative_path.to_string(), SystemTime::now());
        Ok(())
    }

    fn put_physical_if_absent(&self, relative_path: &str, bytes: &[u8]) -> Result<bool> {
        let mut physical = self.physical.lock().unwrap();
        if physical.contains_key(relative_path) {
            return Ok(false);
        }
        physical.insert(relative_path.to_string(), bytes.to_vec());
        drop(physical);
        self.physical_modified_at
            .lock()
            .unwrap()
            .insert(relative_path.to_string(), SystemTime::now());
        Ok(true)
    }

    fn get_physical(&self, relative_path: &str) -> Result<Vec<u8>> {
        self.physical
            .lock()
            .unwrap()
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
        self.physical.lock().unwrap().remove(relative_path);
        self.physical_modified_at
            .lock()
            .unwrap()
            .remove(relative_path);
        Ok(())
    }

    fn exists_physical(&self, relative_path: &str) -> bool {
        self.physical.lock().unwrap().contains_key(relative_path)
    }

    fn stat_physical(&self, relative_path: &str) -> Result<ObjectStat> {
        let bytes = self.get_physical(relative_path)?;
        Ok(ObjectStat {
            length: bytes.len() as u64,
            modified_at: self
                .physical_modified_at
                .lock()
                .unwrap()
                .get(relative_path)
                .copied(),
        })
    }

    fn list_physical(&self, prefix: &str) -> Result<Vec<String>> {
        let mut listed = self
            .physical
            .lock()
            .unwrap()
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
        if self.exists_physical("layout_root.json") {
            return Ok(serde_json::from_slice(
                &self.get_physical("layout_root.json")?,
            )?);
        }
        Ok(self.layout_root.lock().unwrap().clone())
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
        *self.layout_root.lock().unwrap() = next.clone();
        self.put_physical("layout_root.json", &serde_json::to_vec_pretty(&next)?)?;
        self.retained_layout_roots.lock().unwrap().push(next);
        Ok(CasResult {
            applied: true,
            current: None,
        })
    }

    fn list_retained_layout_roots(&self) -> Result<Vec<LayoutRoot>> {
        Ok(self.retained_layout_roots.lock().unwrap().clone())
    }
}

#[cfg(test)]
mod tests {
    use crate::capability::WriterMode;

    use super::*;

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
                    schema_version: 1,
                    layout_id: "direct".to_string(),
                    generation: 2,
                    mapping_policy: "loose".to_string(),
                },
            )
            .unwrap();

        assert!(!stale.applied);
        assert_eq!(backend.read_layout_root().unwrap().generation, 1);
    }

    #[test]
    fn compare_and_swap_layout_root_materializes_physical_layout_root_file() {
        let backend = MemoryBackend::new();
        let next = LayoutRoot {
            schema_version: 1,
            layout_id: "direct".to_string(),
            generation: 2,
            mapping_policy: "loose".to_string(),
        };

        let result = backend
            .compare_and_swap_layout_root(1, next.clone())
            .unwrap();

        assert!(result.applied);
        assert_eq!(
            backend.get_physical("layout_root.json").unwrap(),
            serde_json::to_vec_pretty(&next).unwrap()
        );
    }

    #[test]
    fn read_layout_root_uses_physical_layout_root_bytes_when_present() {
        let backend = MemoryBackend::new();
        let physical = LayoutRoot {
            schema_version: 1,
            layout_id: "direct".to_string(),
            generation: 7,
            mapping_policy: "loose".to_string(),
        };
        backend
            .put_physical(
                "layout_root.json",
                &serde_json::to_vec_pretty(&physical).unwrap(),
            )
            .unwrap();

        assert_eq!(backend.read_layout_root().unwrap(), physical);
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
}
