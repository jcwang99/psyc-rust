pub mod facade;
mod chunker;
mod keyring;
mod manifest_store;
mod working_tree;

#[doc(hidden)]
pub mod testing {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use anyhow::Result;

    pub use crate::keyring::clear_unlocked_keyring_cache_for_test;
    pub use crate::working_tree::{SnapshotReader, StableReadPolicy};
    pub use crate::working_tree::WorkingTree as TestWorkingTree;
    pub use crate::facade::RepositoryFacade;

    pub fn new_working_tree_for_test(
        repo_root: impl AsRef<std::path::Path>,
    ) -> TestWorkingTree {
        TestWorkingTree::new(repo_root)
    }

    #[derive(Debug)]
    pub struct TestSnapshotReader {
        pub result: std::result::Result<Vec<u8>, String>,
        pub calls: Arc<Mutex<Vec<PathBuf>>>,
    }

    impl SnapshotReader for TestSnapshotReader {
        fn read(&self, path: &std::path::Path) -> Result<Vec<u8>> {
            self.calls.lock().unwrap().push(path.to_path_buf());
            self.result.clone().map_err(anyhow::Error::msg)
        }
    }

    pub fn with_snapshot_reader_for_test(
        snapshot_reader: Arc<dyn SnapshotReader>,
    ) -> RepositoryFacade {
        RepositoryFacade::with_snapshot_reader(snapshot_reader)
    }

    pub fn with_stable_read_policy_for_test(
        stable_read_policy: StableReadPolicy,
    ) -> RepositoryFacade {
        RepositoryFacade::with_stable_read_policy(stable_read_policy)
    }

    pub fn with_snapshot_reader_and_policy_for_test(
        snapshot_reader: Arc<dyn SnapshotReader>,
        stable_read_policy: StableReadPolicy,
    ) -> RepositoryFacade {
        RepositoryFacade::with_snapshot_reader_and_policy(snapshot_reader, stable_read_policy)
    }
}

pub use facade::{
    BranchState, CheckoutOptions, CommitOptions, CommitResult, DirectoryEntry, FileHandle,
    InitOptions, ReadService, RepositoryFacade, RepositoryState, SnapshotSummary,
};
pub use manifest_store::{
    ManifestFileObject, ManifestObject, ManifestSnapshotObject, ManifestStore, ManifestStoreApi,
    ManifestTreeEntry, ManifestTreeObject, TreeWalkEntry,
};
pub use working_tree::{SnapshotReader, StableReadPolicy};

pub mod sync_support {
    use std::path::{Path, PathBuf};

    use anyhow::Result;
    use serde::{Deserialize, Serialize};

    use crate::facade::{RepositoryState, SnapshotSummary};

    use super::facade::RepositoryFacade;
    use super::manifest_store::{ManifestStore, ManifestStoreApi};

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct SyncSnapshot {
        pub snapshot_id: String,
        pub parent_snapshot_id: Option<String>,
        pub root_tree_id: String,
        pub ancestor_snapshot_ids: Vec<String>,
    }

    pub fn export_head_snapshot(
        facade: &RepositoryFacade,
        repo_root: impl AsRef<Path>,
    ) -> Result<(RepositoryState, SyncSnapshot)> {
        let repo_root = repo_root.as_ref().to_path_buf();
        let state = facade.open(&repo_root)?;
        let snapshots = facade.snapshots(&repo_root)?;
        let head = snapshots
            .first()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("repository has no snapshots to push"))?;
        let manifest_store = ManifestStore::new(&repo_root);
        let snapshot = manifest_store.get_snapshot(&head.snapshot_id)?;
        let mut ancestor_snapshot_ids = Vec::new();
        let mut next_parent = snapshot.parent_snapshot_id.clone();
        while let Some(parent_snapshot_id) = next_parent {
            ancestor_snapshot_ids.push(parent_snapshot_id.clone());
            let parent = manifest_store.get_snapshot(&parent_snapshot_id)?;
            next_parent = parent.parent_snapshot_id;
        }
        Ok((
            state,
            SyncSnapshot {
                snapshot_id: head.snapshot_id,
                parent_snapshot_id: snapshot.parent_snapshot_id,
                root_tree_id: snapshot.root_tree_id,
                ancestor_snapshot_ids,
            },
        ))
    }

    pub fn list_local_object_files(repo_root: impl AsRef<Path>) -> Result<Vec<PathBuf>> {
        let objects_dir = repo_root.as_ref().join(".e2v").join("objects");
        let mut files = std::fs::read_dir(&objects_dir)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        files.sort();
        Ok(files)
    }

    pub fn read_layout_root_bytes(repo_root: impl AsRef<Path>) -> Result<Vec<u8>> {
        Ok(std::fs::read(
            repo_root.as_ref().join(".e2v").join("layout_root.json"),
        )?)
    }

    pub fn read_config_bytes(repo_root: impl AsRef<Path>) -> Result<Vec<u8>> {
        Ok(std::fs::read(
            repo_root.as_ref().join(".e2v").join("config.json"),
        )?)
    }

    pub fn read_default_ref_bytes(repo_root: impl AsRef<Path>) -> Result<Vec<u8>> {
        Ok(std::fs::read(
            repo_root.as_ref().join(".e2v").join("refs").join("default.json"),
        )?)
    }

    pub fn decode_ref_head_snapshot_id(
        repo_root: impl AsRef<Path>,
        encrypted_ref_bytes: &[u8],
    ) -> Result<Option<String>> {
        let control_dir = repo_root.as_ref().join(".e2v");
        Ok(super::facade::decode_default_ref_bytes(&control_dir, encrypted_ref_bytes)?.head_snapshot_id)
    }

    pub fn list_keyring_files(repo_root: impl AsRef<Path>) -> Result<Vec<PathBuf>> {
        let keyring_dir = repo_root.as_ref().join(".e2v").join("keyring");
        let mut files = std::fs::read_dir(&keyring_dir)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        files.sort();
        Ok(files)
    }

    pub fn list_head_summaries(
        facade: &RepositoryFacade,
        repo_root: impl AsRef<Path>,
    ) -> Result<Vec<SnapshotSummary>> {
        facade.snapshots(repo_root)
    }
}
