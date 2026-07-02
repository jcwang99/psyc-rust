mod chunker;
mod facade;
mod keyring;
mod local_index;
mod manifest_store;
mod working_tree;

#[doc(hidden)]
pub mod testing {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use anyhow::Result;

    pub use crate::chunker::override_fixed_span_bytes_for_test;
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
    ) -> crate::RepositoryFacade {
        crate::RepositoryFacade::with_snapshot_reader(snapshot_reader)
    }

    pub fn with_stable_read_policy_for_test(
        stable_read_policy: StableReadPolicy,
    ) -> crate::RepositoryFacade {
        crate::RepositoryFacade::with_stable_read_policy(stable_read_policy)
    }

    pub fn with_snapshot_reader_and_policy_for_test(
        snapshot_reader: Arc<dyn SnapshotReader>,
        stable_read_policy: StableReadPolicy,
    ) -> crate::RepositoryFacade {
        crate::RepositoryFacade::with_snapshot_reader_and_policy(
            snapshot_reader,
            stable_read_policy,
        )
    }

    pub fn override_max_file_chunks_per_object_for_test(
        max_chunks: usize,
    ) -> crate::facade::MaxFileChunksPerObjectGuard {
        crate::facade::override_max_file_chunks_per_object_for_test(max_chunks)
    }

    pub fn rotate_active_epoch_for_test(
        repo_root: impl AsRef<std::path::Path>,
        password: &str,
    ) -> Result<()> {
        crate::facade::rotate_active_epoch_for_test(repo_root, password)
    }

    pub fn unlock_with_local_device_for_test(
        repo_root: impl AsRef<std::path::Path>,
    ) -> Result<crate::RepositoryState> {
        crate::facade::unlock_with_local_device_for_test(repo_root)
    }

    pub fn force_large_file_sharding_for_test() -> (
        crate::chunker::FixedSpanBytesGuard,
        crate::facade::MaxFileChunksPerObjectGuard,
    ) {
        (
            crate::chunker::override_fixed_span_bytes_for_test(1024 * 1024),
            crate::facade::override_max_file_chunks_per_object_for_test(2),
        )
    }

    pub fn reconcile_remote_keyring_for_test(
        repo_root: impl AsRef<std::path::Path>,
        remote_keyring_bytes: &[u8],
    ) -> Result<bool> {
        crate::facade::reconcile_remote_keyring_for_sync(repo_root, remote_keyring_bytes)
    }
}

pub use facade::{
    BranchState, BranchSummary, CheckoutOptions, CommitOptions, CommitResult, DirectoryEntry,
    FileHandle, InitOptions, ReadService, RepositoryFacade, RepositoryState,
    ShareAcceptDeviceOptions, ShareAcceptMemberOptions, ShareAcceptResult, ShareInviteBundle,
    ShareInviteDeviceOptions, ShareInviteMemberOptions, ShareListResult, ShareRevokeDeviceOptions,
    ShareRevokeMemberOptions, SnapshotHandle, SnapshotSummary, validate_layout_root_value,
};
pub use keyring::clear_unlocked_keyring_cache;
pub use local_index::{FilenameSearchResult, MetadataSearchQuery, MetadataSearchResult};
pub use manifest_store::{
    ManifestFileObject, ManifestObject, ManifestSnapshotObject, ManifestStore, ManifestStoreApi,
    ManifestTreeEntry, ManifestTreeObject, TreeWalkEntry,
};
pub use working_tree::{SnapshotReader, StableReadPolicy};

pub mod sync_support {
    use std::path::Component;
    use std::path::{Path, PathBuf};

    use anyhow::{Context, Result, ensure};
    use chacha20poly1305::aead::{AeadInPlace, KeyInit};
    use chacha20poly1305::{Tag, XChaCha20Poly1305, XNonce};
    use e2v_store::{
        LayoutObjectLocation, PackStorageLayout, PhysicalObjectRef, RepoSecrets, StorageLayout,
        validate_object_id_value,
    };
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

    pub fn read_repo_id(repo_root: impl AsRef<Path>) -> Result<String> {
        let keyring = crate::keyring::read_current_keyring_state(&repo_root.as_ref().join(".e2v"))?;
        Ok(keyring.repo_id)
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

    pub fn read_cached_pack_object_bytes(
        repo_root: impl AsRef<Path>,
        object_id: &str,
    ) -> Result<Vec<u8>> {
        fn cached_pack_data_path(repo_root: &Path, container_id: &str) -> Result<PathBuf> {
            let relative = Path::new(container_id);
            ensure!(
                !container_id.is_empty(),
                "cached pack container id must not be empty"
            );
            ensure!(
                !relative.is_absolute(),
                "cached pack container id must be relative"
            );
            ensure!(
                relative
                    .components()
                    .all(|component| matches!(component, Component::Normal(_))),
                "cached pack container path traversal is not allowed"
            );
            let mut path = repo_root.join(".e2v").join("cache").join("pack-data");
            for segment in relative.components() {
                let Component::Normal(segment) = segment else {
                    unreachable!("validated above")
                };
                path.push(segment);
            }
            Ok(path)
        }

        let physical_ref = load_cached_pack_physical_ref_for_object_id(&repo_root, object_id)?;
        let offset = usize::try_from(physical_ref.offset.unwrap_or(0))
            .map_err(|_| anyhow::anyhow!("cached pack offset is too large to read"))?;
        let length = usize::try_from(physical_ref.length)
            .map_err(|_| anyhow::anyhow!("cached pack length is too large to read"))?;
        let pack_path = cached_pack_data_path(repo_root.as_ref(), &physical_ref.container_id)?;
        let mut pack_file = std::fs::File::open(&pack_path).with_context(|| {
            format!(
                "cached pack data is missing for {}",
                physical_ref.container_id
            )
        })?;
        read_cached_pack_object_range(&mut pack_file, offset, length, object_id)
    }

    fn read_cached_pack_object_range<R: std::io::Read + std::io::Seek>(
        reader: &mut R,
        offset: usize,
        length: usize,
        object_id: &str,
    ) -> Result<Vec<u8>> {
        use std::io::SeekFrom;

        let offset_u64 = u64::try_from(offset)
            .map_err(|_| anyhow::anyhow!("cached pack offset is too large to seek"))?;
        reader
            .seek(SeekFrom::Start(offset_u64))
            .with_context(|| format!("cached pack object range out of bounds for {object_id}"))?;
        let mut bytes = vec![0u8; length];
        reader
            .read_exact(&mut bytes)
            .with_context(|| format!("cached pack object range out of bounds for {object_id}"))?;
        Ok(bytes)
    }

    pub fn decode_object_bytes_for_sync(
        repo_root: impl AsRef<Path>,
        object_id: &str,
        expected_type: &str,
        bytes: &[u8],
    ) -> Result<Vec<u8>> {
        const ENVELOPE_MAGIC: &[u8; 4] = b"E2V0";
        const ENVELOPE_FORMAT_VERSION: u32 = 1;
        const CRYPTO_SUITE: &str = "xchacha20poly1305";
        const PADDING_POLICY_NONE: &str = "none";

        let control_dir = repo_root.as_ref().join(".e2v");
        let secrets = open_repo_secrets_for_sync(&control_dir)?;
        let mut cursor = 0usize;

        let magic = take_exact(bytes, &mut cursor, ENVELOPE_MAGIC.len())?;
        ensure!(magic == ENVELOPE_MAGIC, "object authentication failed");

        let format_version = read_u32(bytes, &mut cursor)?;
        ensure!(
            format_version == ENVELOPE_FORMAT_VERSION,
            "unsupported object format version"
        );

        let object_type = read_string(bytes, &mut cursor)?;
        let crypto_suite = read_string(bytes, &mut cursor)?;
        let key_epoch = read_u32(bytes, &mut cursor)?;
        let padding_policy = read_string(bytes, &mut cursor)?;
        let stored_object_id = read_string(bytes, &mut cursor)?;
        let nonce_len = read_u8(bytes, &mut cursor)? as usize;
        ensure!(nonce_len == 24, "object authentication failed");
        let nonce = take_exact(bytes, &mut cursor, nonce_len)?.to_vec();
        let ciphertext_len = read_u64(bytes, &mut cursor)? as usize;
        let ciphertext = take_exact(bytes, &mut cursor, ciphertext_len)?.to_vec();
        let auth_tag = take_exact(bytes, &mut cursor, 16)?.to_vec();
        ensure!(cursor == bytes.len(), "object authentication failed");

        ensure!(
            object_type == expected_type,
            "object type mismatch: expected {expected_type}, got {object_type}"
        );
        ensure!(
            crypto_suite == CRYPTO_SUITE,
            "unsupported crypto suite: {crypto_suite}"
        );
        ensure!(
            stored_object_id == object_id,
            "object id mismatch in stored envelope"
        );
        ensure!(key_epoch > 0, "object authentication failed");

        let mut associated_data = Vec::new();
        associated_data.extend_from_slice(ENVELOPE_MAGIC);
        associated_data.extend_from_slice(&ENVELOPE_FORMAT_VERSION.to_le_bytes());
        associated_data.extend_from_slice(secrets.repo_id.as_bytes());
        associated_data.extend_from_slice(object_type.as_bytes());
        associated_data.extend_from_slice(stored_object_id.as_bytes());
        associated_data.extend_from_slice(CRYPTO_SUITE.as_bytes());
        associated_data.extend_from_slice(&key_epoch.to_le_bytes());
        associated_data.extend_from_slice(padding_policy.as_bytes());

        let epoch_keys = secrets.epoch_keys(key_epoch)?;
        let cipher = XChaCha20Poly1305::new((&epoch_keys.manifest_enc_key).into());
        let mut plaintext = ciphertext;
        cipher
            .decrypt_in_place_detached(
                XNonce::from_slice(&nonce),
                &associated_data,
                &mut plaintext,
                Tag::from_slice(&auth_tag),
            )
            .map_err(|_| anyhow::anyhow!("object authentication failed"))?;

        let plaintext = if padding_policy == PADDING_POLICY_NONE {
            plaintext
        } else {
            ensure!(plaintext.len() >= 4, "object authentication failed");
            let mut pad_len_bytes = [0u8; 4];
            pad_len_bytes.copy_from_slice(&plaintext[..4]);
            let pad_len = u32::from_le_bytes(pad_len_bytes) as usize;
            ensure!(
                plaintext.len() >= 4 + pad_len,
                "object authentication failed"
            );
            let end = plaintext.len() - pad_len;
            plaintext[4..end].to_vec()
        };

        let mut input = Vec::with_capacity(expected_type.len() + 8 + plaintext.len());
        input.extend_from_slice(expected_type.as_bytes());
        input.extend_from_slice(&(plaintext.len() as u64).to_le_bytes());
        input.extend_from_slice(&plaintext);
        let recomputed_id =
            hex::encode(blake3::keyed_hash(&secrets.repo_dedup_key, &input).as_bytes());
        ensure!(
            recomputed_id == object_id,
            "object authentication failed: object id mismatch"
        );

        Ok(plaintext)
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

    pub fn load_cached_pack_physical_ref_for_object_id(
        repo_root: impl AsRef<Path>,
        object_id: &str,
    ) -> Result<PhysicalObjectRef> {
        const PACK_INDEX_ROOT_SCHEMA_VERSION: u32 = 1;
        const PACK_INDEX_ROOT_STABLE_NAME: &str = "pack-index-root";
        const PACK_INDEX_ROOT_OBJECT_TYPE: &str = "pack-index-root";
        const PACK_INDEX_SEGMENT_OBJECT_TYPE: &str = "pack-index-segment";
        const PACK_INDEX_SEGMENT_STABLE_NAME_PREFIX: &str = "pack-index-segment:";
        const REMOTE_PACK_INDEX_PREFIX: &str = "packs/index/";
        const PACK_INDEX_COMPACTED_SEGMENT_PREFIX: &str = "pack-index/segments/";
        const REMOTE_PACK_DATA_PREFIX: &str = "packs/data/";

        #[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
        struct CachedPackIndexRoot {
            schema_version: u32,
            layout_id: String,
            segments: Vec<String>,
        }

        #[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
        struct CachedObjectPackIndex {
            schema_version: u32,
            data_path: String,
            entries: Vec<CachedObjectPackEntry>,
        }

        #[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
        struct CachedObjectPackEntry {
            object_id: String,
            offset: u64,
            length: u64,
        }

        #[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
        struct CachedAggregatePackIndexSegment {
            schema_version: u32,
            entries: Vec<CachedAggregatePackIndexEntry>,
        }

        #[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
        struct CachedAggregatePackIndexEntry {
            object_id: String,
            data_path: String,
            offset: u64,
            length: u64,
        }

        #[derive(Debug, Clone, PartialEq, Eq)]
        struct CachedPackedObjectLocation {
            data_path: String,
            offset: usize,
            length: usize,
        }

        impl CachedPackedObjectLocation {
            fn physical_ref(&self) -> Result<PhysicalObjectRef> {
                PackStorageLayout.resolve(LayoutObjectLocation::PackedObject {
                    container_id: &self.data_path,
                    offset: self.offset as u64,
                    length: self.length as u64,
                })
            }
        }

        fn cached_segment_path(cache_dir: &Path, segment_path: &str) -> PathBuf {
            cache_dir
                .join("segments")
                .join(segment_path.replace('/', "__"))
        }

        fn decode_root(bytes: &[u8], secrets: &RepoSecrets) -> Result<CachedPackIndexRoot> {
            let plaintext = decrypt_control_record_for_sync(
                secrets,
                PACK_INDEX_ROOT_STABLE_NAME,
                PACK_INDEX_ROOT_OBJECT_TYPE,
                bytes,
            )?;
            serde_json::from_slice(&plaintext)
                .context("failed to decode authenticated pack index root")
        }

        fn decode_segment(
            segment_path: &str,
            bytes: &[u8],
            secrets: &RepoSecrets,
        ) -> Result<Vec<CachedAggregatePackIndexEntry>> {
            let plaintext = decrypt_control_record_for_sync(
                secrets,
                &format!("{PACK_INDEX_SEGMENT_STABLE_NAME_PREFIX}{segment_path}"),
                PACK_INDEX_SEGMENT_OBJECT_TYPE,
                bytes,
            )
            .with_context(|| {
                format!("failed to decrypt authenticated pack index segment {segment_path}")
            })?;

            if segment_path.starts_with(REMOTE_PACK_INDEX_PREFIX) {
                let index: CachedObjectPackIndex = serde_json::from_slice(&plaintext)?;
                ensure!(
                    index.schema_version == PACK_INDEX_ROOT_SCHEMA_VERSION,
                    "unsupported pack index schema version {}",
                    index.schema_version
                );
                ensure!(
                    index.data_path.starts_with(REMOTE_PACK_DATA_PREFIX),
                    "invalid pack data path {}",
                    index.data_path
                );
                return Ok(index
                    .entries
                    .into_iter()
                    .map(|entry| CachedAggregatePackIndexEntry {
                        object_id: entry.object_id,
                        data_path: index.data_path.clone(),
                        offset: entry.offset,
                        length: entry.length,
                    })
                    .collect());
            }

            let aggregate: CachedAggregatePackIndexSegment = serde_json::from_slice(&plaintext)?;
            ensure!(
                aggregate.schema_version == PACK_INDEX_ROOT_SCHEMA_VERSION,
                "unsupported aggregate pack index schema version {}",
                aggregate.schema_version
            );
            Ok(aggregate.entries)
        }

        validate_object_id_value(object_id)?;
        let control_dir = repo_root.as_ref().join(".e2v");
        let secrets = open_repo_secrets_for_sync(&control_dir)?;
        let cache_dir = control_dir.join("cache").join("pack-index");
        let root_bytes = std::fs::read(cache_dir.join("root.json"))
            .context("cached pack index root is missing")?;
        let root = decode_root(&root_bytes, &secrets)?;
        ensure!(
            root.schema_version == PACK_INDEX_ROOT_SCHEMA_VERSION,
            "unsupported pack index root schema version {}",
            root.schema_version
        );
        ensure!(
            !root.layout_id.trim().is_empty(),
            "pack index root layout id must not be empty"
        );

        for segment_path in &root.segments {
            ensure!(
                segment_path.starts_with(REMOTE_PACK_INDEX_PREFIX)
                    || segment_path.starts_with(PACK_INDEX_COMPACTED_SEGMENT_PREFIX),
                "invalid pack index segment path {}",
                segment_path
            );
            let segment_bytes = std::fs::read(cached_segment_path(&cache_dir, segment_path))
                .with_context(|| {
                    format!("cached pack index segment is missing for {}", segment_path)
                })?;
            for entry in decode_segment(segment_path, &segment_bytes, &secrets)? {
                ensure!(
                    entry.data_path.starts_with(REMOTE_PACK_DATA_PREFIX),
                    "invalid aggregate pack data path {}",
                    entry.data_path
                );
                if entry.object_id == object_id {
                    return CachedPackedObjectLocation {
                        data_path: entry.data_path,
                        offset: entry.offset as usize,
                        length: entry.length as usize,
                    }
                    .physical_ref();
                }
            }
        }

        Err(anyhow::anyhow!(
            "cached pack index has no entry for object {object_id}"
        ))
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

    pub fn open_or_unlock_repo_secrets_for_sync(
        control_dir: impl AsRef<Path>,
    ) -> Result<RepoSecrets> {
        match super::keyring::open_repo_secrets(control_dir.as_ref()) {
            Ok(secrets) => Ok(secrets),
            Err(_) => super::keyring::unlock_repo_secrets_with_local_device(control_dir.as_ref()),
        }
    }

    pub fn unlock_repo_secrets_for_sync(
        control_dir: impl AsRef<Path>,
        password: &str,
    ) -> Result<RepoSecrets> {
        super::keyring::unlock_repo_secrets_uncached(control_dir.as_ref(), password)
    }

    pub fn unlock_repo_secrets_from_keyring_bytes_for_sync(
        keyring_bytes: &[u8],
        password: &str,
    ) -> Result<RepoSecrets> {
        super::keyring::unlock_repo_secrets_from_keyring_bytes(keyring_bytes, password)
    }

    pub fn unlock_repo_secrets_from_keyring_bytes_with_local_device_for_sync(
        control_dir: impl AsRef<Path>,
        keyring_bytes: &[u8],
    ) -> Result<RepoSecrets> {
        super::keyring::unlock_repo_secrets_from_keyring_bytes_with_local_device(
            control_dir.as_ref(),
            keyring_bytes,
        )
    }

    pub fn encrypt_control_record_for_sync(
        secrets: &RepoSecrets,
        stable_name: &str,
        object_type: &str,
        plaintext: &[u8],
    ) -> Result<Vec<u8>> {
        super::facade::encrypt_control_record(secrets, stable_name, object_type, plaintext)
    }

    pub fn decrypt_control_record_for_sync(
        secrets: &RepoSecrets,
        stable_name: &str,
        object_type: &str,
        bytes: &[u8],
    ) -> Result<Vec<u8>> {
        super::facade::decrypt_control_record(secrets, stable_name, object_type, bytes)
    }

    pub fn verify_snapshot_with_secrets_for_sync(
        repo_root: impl AsRef<Path>,
        secrets: RepoSecrets,
        snapshot_id: &str,
    ) -> Result<()> {
        super::facade::verify_snapshot_with_secrets_for_sync(repo_root, secrets, snapshot_id)
    }

    pub fn reconcile_remote_keyring_for_sync(
        repo_root: impl AsRef<Path>,
        remote_keyring_bytes: &[u8],
    ) -> Result<bool> {
        super::facade::reconcile_remote_keyring_for_sync(repo_root, remote_keyring_bytes)
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

    #[cfg(test)]
    mod tests {
        use std::io::Cursor;

        use super::*;

        #[test]
        fn cached_pack_object_reads_only_the_requested_range() {
            let object_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
            let mut short_reader = Cursor::new(b"34".to_vec());
            let error =
                read_cached_pack_object_range(&mut short_reader, 3, 5, object_id).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("cached pack object range out of bounds"),
                "unexpected error: {error:#}"
            );

            let mut reader = Cursor::new(b"01234567".to_vec());
            let bytes = read_cached_pack_object_range(&mut reader, 3, 5, object_id).unwrap();
            assert_eq!(bytes, b"34567");
        }
    }
}
