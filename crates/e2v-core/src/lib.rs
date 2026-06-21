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
