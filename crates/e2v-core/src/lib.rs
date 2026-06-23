mod chunker;
pub mod facade;
mod keyring;
mod local_index;
mod manifest_store;
mod working_tree;

#[doc(hidden)]
pub mod testing {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use anyhow::Result;

    pub use crate::facade::RepositoryFacade;
    pub use crate::keyring::clear_unlocked_keyring_cache_for_test;
    pub use crate::working_tree::WorkingTree as TestWorkingTree;
    pub use crate::working_tree::{SnapshotReader, StableReadPolicy};

    pub fn new_working_tree_for_test(repo_root: impl AsRef<std::path::Path>) -> TestWorkingTree {
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
    BranchState, BranchSummary, CheckoutOptions, CommitOptions, CommitResult, DirectoryEntry, FileHandle,
    InitOptions, ReadService, RepositoryFacade, RepositoryState, SnapshotSummary,
    validate_layout_root_value,
};
pub use keyring::clear_unlocked_keyring_cache;
pub use local_index::{
    FilenameSearchResult, MetadataSearchQuery, MetadataSearchResult,
};
pub use manifest_store::{
    ManifestFileObject, ManifestObject, ManifestSnapshotObject, ManifestStore, ManifestStoreApi,
    ManifestTreeEntry, ManifestTreeObject, TreeWalkEntry,
};
pub use working_tree::{SnapshotReader, StableReadPolicy};

pub mod sync_support {
    use std::path::{Path, PathBuf};

    use anyhow::{Result, ensure};
    use e2v_store::{RepoSecrets, validate_object_id_value};
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
            repo_root
                .as_ref()
                .join(".e2v")
                .join("refs")
                .join("default.json"),
        )?)
    }

    pub fn read_local_object_bytes(
        repo_root: impl AsRef<Path>,
        object_id: &str,
    ) -> Result<Vec<u8>> {
        validate_object_id_value(object_id)?;
        Ok(std::fs::read(
            repo_root
                .as_ref()
                .join(".e2v")
                .join("objects")
                .join(format!("{object_id}.json")),
        )?)
    }

    pub fn local_object_envelope_looks_valid(
        repo_root: impl AsRef<Path>,
        object_id: &str,
    ) -> Result<bool> {
        parse_local_object_envelope_header(repo_root, object_id).map(|_| true)
    }

    pub fn read_local_object_type_hint(
        repo_root: impl AsRef<Path>,
        object_id: &str,
    ) -> Result<String> {
        Ok(parse_local_object_envelope_header(repo_root, object_id)?.object_type)
    }

    struct LocalObjectEnvelopeHeader {
        object_type: String,
    }

    fn parse_local_object_envelope_header(
        repo_root: impl AsRef<Path>,
        object_id: &str,
    ) -> Result<LocalObjectEnvelopeHeader> {
        const ENVELOPE_MAGIC: &[u8; 4] = b"E2V0";
        const ENVELOPE_FORMAT_VERSION: u32 = 1;
        const CRYPTO_SUITE: &str = "xchacha20poly1305";
        const NONCE_SIZE: usize = 24;
        const TAG_SIZE: usize = 16;

        let bytes = read_local_object_bytes(repo_root, object_id)?;
        ensure!(
            bytes.len() >= ENVELOPE_MAGIC.len(),
            "object authentication failed"
        );

        let mut cursor = 0usize;
        let magic = take_exact(&bytes, &mut cursor, ENVELOPE_MAGIC.len())?;
        ensure!(magic == ENVELOPE_MAGIC, "object authentication failed");

        let format_version = read_u32(&bytes, &mut cursor)?;
        ensure!(
            format_version == ENVELOPE_FORMAT_VERSION,
            "object authentication failed"
        );

        let object_type = read_string(&bytes, &mut cursor)?;
        ensure!(
            !object_type.trim().is_empty(),
            "object authentication failed"
        );

        let crypto_suite = read_string(&bytes, &mut cursor)?;
        ensure!(crypto_suite == CRYPTO_SUITE, "object authentication failed");

        let key_epoch = read_u32(&bytes, &mut cursor)?;
        ensure!(key_epoch > 0, "object authentication failed");

        let padding_policy = read_string(&bytes, &mut cursor)?;
        ensure!(
            !padding_policy.trim().is_empty(),
            "object authentication failed"
        );

        let stored_object_id = read_string(&bytes, &mut cursor)?;
        ensure!(
            stored_object_id == object_id,
            "object authentication failed"
        );

        let nonce_len = take_exact(&bytes, &mut cursor, 1)?[0] as usize;
        ensure!(nonce_len == NONCE_SIZE, "object authentication failed");
        let _nonce = take_exact(&bytes, &mut cursor, nonce_len)?;

        let ciphertext_len = read_u64(&bytes, &mut cursor)? as usize;
        ensure!(ciphertext_len > 0, "object authentication failed");
        let _ciphertext = take_exact(&bytes, &mut cursor, ciphertext_len)?;

        let auth_tag = take_exact(&bytes, &mut cursor, TAG_SIZE)?;
        ensure!(auth_tag.len() == TAG_SIZE, "object authentication failed");
        ensure!(cursor == bytes.len(), "object authentication failed");

        Ok(LocalObjectEnvelopeHeader { object_type })
    }

    fn take_exact<'a>(bytes: &'a [u8], cursor: &mut usize, len: usize) -> Result<&'a [u8]> {
        let end = cursor.saturating_add(len);
        ensure!(end <= bytes.len(), "object authentication failed");
        let slice = &bytes[*cursor..end];
        *cursor = end;
        Ok(slice)
    }

    fn read_u8(bytes: &[u8], cursor: &mut usize) -> Result<u8> {
        Ok(take_exact(bytes, cursor, 1)?[0])
    }

    fn read_u32(bytes: &[u8], cursor: &mut usize) -> Result<u32> {
        let mut value = [0u8; 4];
        value.copy_from_slice(take_exact(bytes, cursor, 4)?);
        Ok(u32::from_le_bytes(value))
    }

    fn read_u64(bytes: &[u8], cursor: &mut usize) -> Result<u64> {
        let mut value = [0u8; 8];
        value.copy_from_slice(take_exact(bytes, cursor, 8)?);
        Ok(u64::from_le_bytes(value))
    }

    fn read_string(bytes: &[u8], cursor: &mut usize) -> Result<String> {
        let length = read_u8(bytes, cursor)? as usize;
        let raw = take_exact(bytes, cursor, length)?;
        Ok(std::str::from_utf8(raw)
            .map_err(|_| anyhow::anyhow!("object authentication failed"))?
            .to_string())
    }

    pub fn decode_ref_head_snapshot_id(
        repo_root: impl AsRef<Path>,
        encrypted_ref_bytes: &[u8],
    ) -> Result<Option<String>> {
        let control_dir = repo_root.as_ref().join(".e2v");
        Ok(
            super::facade::decode_default_ref_bytes(&control_dir, encrypted_ref_bytes)?
                .head_snapshot_id,
        )
    }

    pub fn decode_default_ref_record(
        repo_root: impl AsRef<Path>,
        encrypted_ref_bytes: &[u8],
    ) -> Result<(String, Option<String>)> {
        let control_dir = repo_root.as_ref().join(".e2v");
        let record = super::facade::decode_default_ref_bytes(&control_dir, encrypted_ref_bytes)?;
        Ok((record.ref_token_hex, record.head_snapshot_id))
    }

    pub fn open_repo_secrets_for_sync(control_dir: impl AsRef<Path>) -> Result<RepoSecrets> {
        super::keyring::open_repo_secrets(control_dir.as_ref())
    }

    pub fn verify_snapshot_with_secrets_for_sync(
        repo_root: impl AsRef<Path>,
        secrets: RepoSecrets,
        snapshot_id: &str,
    ) -> Result<()> {
        super::facade::verify_snapshot_with_secrets_for_sync(repo_root, secrets, snapshot_id)
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
