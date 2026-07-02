use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use e2v_core::sync_support::{
    decrypt_control_record_for_sync, encrypt_control_record_for_sync,
    open_or_unlock_repo_secrets_for_sync,
};
use e2v_store::local_backend::is_missing_physical_object_error;
use e2v_store::{BlobStore, PhysicalObjectRef, RepoSecrets};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::journal::validate_sync_identifier;
use crate::pack::{
    ObjectPackIndex, PackedObjectLocation, REMOTE_PACK_DATA_PREFIX, REMOTE_PACK_INDEX_PREFIX,
    append_pack_index_locations_from_bytes, pack_paths,
};

const PACK_INDEX_ROOT_SCHEMA_VERSION: u32 = 1;
const PACK_INDEX_ROOT_PATH: &str = "pack-index/root.json";
const PACK_INDEX_SEGMENT_BOUND: usize = 4;
const PACK_INDEX_COMPACTED_SEGMENT_PREFIX: &str = "pack-index/segments/";
const PACK_INDEX_ROOT_STABLE_NAME: &str = "pack-index-root";
const PACK_INDEX_ROOT_OBJECT_TYPE: &str = "pack-index-root";
const PACK_INDEX_SEGMENT_OBJECT_TYPE: &str = "pack-index-segment";
const COMPACTED_SEGMENT_STABLE_NAME_PREFIX: &str = "pack-index-segment:";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackIndexRoot {
    pub schema_version: u32,
    pub layout_id: String,
    pub layout_generation: u64,
    pub generation: u64,
    pub segments: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct AggregatePackIndexSegment {
    schema_version: u32,
    entries: Vec<AggregatePackIndexEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct AggregatePackIndexEntry {
    object_id: String,
    data_path: String,
    offset: u64,
    length: u64,
}

pub fn publish_pack_index_root<B: BlobStore>(
    remote: &B,
    secrets: &RepoSecrets,
    layout_id: &str,
    layout_generation: u64,
    segment_paths: Vec<String>,
) -> Result<()> {
    let compacted_segment_paths =
        compact_segment_paths_if_needed(remote, secrets, layout_generation, segment_paths)?;
    let root = PackIndexRoot {
        schema_version: PACK_INDEX_ROOT_SCHEMA_VERSION,
        layout_id: layout_id.to_string(),
        layout_generation,
        generation: layout_generation,
        segments: compacted_segment_paths,
    };
    remote.put_physical(
        PACK_INDEX_ROOT_PATH,
        &encode_pack_index_root_bytes(secrets, &root)?,
    )
}

pub fn load_remote_pack_locations_with_local_cache<B: BlobStore>(
    remote: &B,
    control_dir: &Path,
    secrets: Option<&RepoSecrets>,
) -> Result<BTreeMap<String, PackedObjectLocation>> {
    let cache_dir = pack_index_cache_dir(control_dir);
    std::fs::create_dir_all(segment_cache_dir(control_dir))?;

    let Some(root_bytes) = load_remote_pack_index_root_bytes(remote)? else {
        return Ok(BTreeMap::new());
    };
    let root = decode_pack_index_root_bytes(&root_bytes, secrets)?;
    validate_pack_index_root(&root)?;

    let mut locations = BTreeMap::new();
    for segment_path in &root.segments {
        validate_segment_path(segment_path)?;
        let cache_path = cached_segment_path(&cache_dir, segment_path);
        let segment_bytes =
            load_segment_bytes_with_cache_recovery(remote, segment_path, &cache_path, secrets)?;
        append_segment_locations(
            remote,
            segment_path,
            &segment_bytes,
            &mut locations,
            secrets,
        )?;
    }

    std::fs::write(cache_dir.join("root.json"), root_bytes)?;
    Ok(locations)
}

pub fn load_remote_pack_index_root_bytes<B: BlobStore>(remote: &B) -> Result<Option<Vec<u8>>> {
    match remote.get_physical(PACK_INDEX_ROOT_PATH) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if is_missing_physical_object_error(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

pub(crate) fn load_remote_pack_index_segment_paths<B: BlobStore>(
    remote: &B,
    secrets: Option<&RepoSecrets>,
) -> Result<Vec<String>> {
    let Some(root_bytes) = load_remote_pack_index_root_bytes(remote)? else {
        return Ok(Vec::new());
    };
    let root = decode_pack_index_root_bytes(&root_bytes, secrets)?;
    validate_pack_index_root(&root)?;
    Ok(root.segments)
}

pub fn next_pack_index_segment_paths<B: BlobStore>(
    remote: &B,
    newly_published_segments: &[String],
    secrets: Option<&RepoSecrets>,
) -> Result<Vec<String>> {
    let mut segment_paths = if let Some(root_bytes) = load_remote_pack_index_root_bytes(remote)? {
        let root = decode_pack_index_root_bytes(&root_bytes, secrets)?;
        validate_pack_index_root(&root)?;
        root.segments
    } else {
        Vec::new()
    };
    for segment_path in newly_published_segments {
        validate_segment_path(segment_path)?;
        if !segment_paths.contains(segment_path) {
            segment_paths.push(segment_path.clone());
        }
    }
    Ok(segment_paths)
}

#[cfg(test)]
fn list_remote_pack_index_segments<B: BlobStore>(remote: &B) -> Result<Vec<String>> {
    let mut segments = remote
        .list_physical(REMOTE_PACK_INDEX_PREFIX)?
        .into_iter()
        .chain(remote.list_physical(PACK_INDEX_COMPACTED_SEGMENT_PREFIX)?)
        .collect::<Vec<_>>();
    segments.sort();
    Ok(segments)
}

pub fn load_remote_operation_pack_locations_with_secrets<B: BlobStore>(
    remote: &B,
    operation_id: &str,
    secrets: &RepoSecrets,
) -> Result<BTreeMap<String, PackedObjectLocation>> {
    validate_sync_identifier("operation id", operation_id)?;
    let mut locations = BTreeMap::new();
    let mut batch_index = 0usize;
    loop {
        let (_, _, index_path) = pack_paths(operation_id, batch_index)?;
        let bytes = match remote.get_physical(&index_path) {
            Ok(bytes) => bytes,
            Err(error) if is_missing_physical_object_error(&error) => break,
            Err(error) => return Err(error),
        };
        append_segment_locations(remote, &index_path, &bytes, &mut locations, Some(secrets))?;
        batch_index += 1;
    }
    Ok(locations)
}

pub fn load_cached_pack_physical_ref_for_object_id(
    control_dir: &Path,
    object_id: &str,
) -> Result<PhysicalObjectRef> {
    let repo_root = control_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("control_dir has no parent repository root"))?;
    e2v_core::sync_support::load_cached_pack_physical_ref_for_object_id(repo_root, object_id)
}

#[doc(hidden)]
pub fn decode_pack_index_root_value_for_test(control_dir: &Path, bytes: &[u8]) -> Result<Value> {
    let secrets = open_or_unlock_repo_secrets_for_sync(control_dir)?;
    let root = decode_pack_index_root_bytes(bytes, Some(&secrets))?;
    serde_json::to_value(root).map_err(Into::into)
}

#[doc(hidden)]
pub fn encode_pack_index_root_value_for_test(control_dir: &Path, value: &Value) -> Result<Vec<u8>> {
    let secrets = open_or_unlock_repo_secrets_for_sync(control_dir)?;
    let root: PackIndexRoot = serde_json::from_value(value.clone())?;
    encode_pack_index_root_bytes(&secrets, &root)
}

#[doc(hidden)]
pub fn decode_pack_index_segment_value_for_test(
    control_dir: &Path,
    segment_path: &str,
    bytes: &[u8],
) -> Result<Value> {
    let secrets = open_or_unlock_repo_secrets_for_sync(control_dir)?;
    let plaintext = decode_pack_index_segment_plaintext(segment_path, bytes, Some(&secrets))?;
    let index: ObjectPackIndex = serde_json::from_slice(&plaintext)?;
    serde_json::to_value(index).map_err(Into::into)
}

#[doc(hidden)]
pub fn encode_pack_index_segment_value_for_test(
    control_dir: &Path,
    segment_path: &str,
    value: &Value,
) -> Result<Vec<u8>> {
    let secrets = open_or_unlock_repo_secrets_for_sync(control_dir)?;
    let index: ObjectPackIndex = serde_json::from_value(value.clone())?;
    encode_pack_index_segment_bytes(&secrets, segment_path, &serde_json::to_vec(&index)?)
}

#[doc(hidden)]
pub fn encode_pack_index_segment_bytes_for_sync(
    secrets: &RepoSecrets,
    segment_path: &str,
    plaintext: &[u8],
) -> Result<Vec<u8>> {
    encode_pack_index_segment_bytes(secrets, segment_path, plaintext)
}

fn pack_index_cache_dir(control_dir: &Path) -> PathBuf {
    control_dir.join("cache").join("pack-index")
}

fn segment_cache_dir(control_dir: &Path) -> PathBuf {
    pack_index_cache_dir(control_dir).join("segments")
}

fn cached_segment_path(cache_dir: &Path, segment_path: &str) -> PathBuf {
    cache_dir
        .join("segments")
        .join(segment_path.replace('/', "__"))
}

fn load_segment_bytes_with_cache_recovery<B: BlobStore>(
    remote: &B,
    segment_path: &str,
    cache_path: &Path,
    secrets: Option<&RepoSecrets>,
) -> Result<Vec<u8>> {
    if cache_path.is_file() {
        let cached_bytes = std::fs::read(cache_path)?;
        if read_segment_entries(segment_path, &cached_bytes, secrets).is_ok() {
            return Ok(cached_bytes);
        }
        let _ = std::fs::remove_file(cache_path);
    }

    let bytes = remote.get_physical(segment_path)?;
    if let Some(parent) = cache_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(cache_path, &bytes)?;
    Ok(bytes)
}

fn encode_pack_index_root_bytes(secrets: &RepoSecrets, root: &PackIndexRoot) -> Result<Vec<u8>> {
    encrypt_control_record_for_sync(
        secrets,
        PACK_INDEX_ROOT_STABLE_NAME,
        PACK_INDEX_ROOT_OBJECT_TYPE,
        &serde_json::to_vec(root)?,
    )
}

fn decode_pack_index_root_bytes(
    bytes: &[u8],
    secrets: Option<&RepoSecrets>,
) -> Result<PackIndexRoot> {
    let secrets = secrets.context("pack index root requires unlocked repository or password")?;
    let plaintext = decrypt_control_record_for_sync(
        secrets,
        PACK_INDEX_ROOT_STABLE_NAME,
        PACK_INDEX_ROOT_OBJECT_TYPE,
        bytes,
    )?;
    serde_json::from_slice(&plaintext).context("failed to decode authenticated pack index root")
}

fn pack_index_segment_stable_name(segment_path: &str) -> String {
    format!("{COMPACTED_SEGMENT_STABLE_NAME_PREFIX}{segment_path}")
}

fn encode_pack_index_segment_bytes(
    secrets: &RepoSecrets,
    segment_path: &str,
    plaintext: &[u8],
) -> Result<Vec<u8>> {
    encrypt_control_record_for_sync(
        secrets,
        &pack_index_segment_stable_name(segment_path),
        PACK_INDEX_SEGMENT_OBJECT_TYPE,
        plaintext,
    )
}

fn decode_pack_index_segment_plaintext(
    segment_path: &str,
    bytes: &[u8],
    secrets: Option<&RepoSecrets>,
) -> Result<Vec<u8>> {
    let secrets = secrets.context("pack index segment requires unlocked repository or password")?;
    decrypt_control_record_for_sync(
        secrets,
        &pack_index_segment_stable_name(segment_path),
        PACK_INDEX_SEGMENT_OBJECT_TYPE,
        bytes,
    )
    .with_context(|| format!("failed to decrypt authenticated pack index segment {segment_path}"))
}

fn validate_pack_index_root(root: &PackIndexRoot) -> Result<()> {
    ensure!(
        root.schema_version == PACK_INDEX_ROOT_SCHEMA_VERSION,
        "unsupported pack index root schema version {}",
        root.schema_version
    );
    ensure!(
        !root.layout_id.trim().is_empty(),
        "pack index root layout id must not be empty"
    );
    Ok(())
}

fn validate_segment_path(segment_path: &str) -> Result<()> {
    ensure!(
        segment_path.starts_with(REMOTE_PACK_INDEX_PREFIX)
            || segment_path.starts_with(PACK_INDEX_COMPACTED_SEGMENT_PREFIX),
        "invalid pack index segment path {}",
        segment_path
    );
    Ok(())
}

fn compact_segment_paths_if_needed<B: BlobStore>(
    remote: &B,
    secrets: &RepoSecrets,
    layout_generation: u64,
    segment_paths: Vec<String>,
) -> Result<Vec<String>> {
    if segment_paths.len() <= PACK_INDEX_SEGMENT_BOUND {
        return Ok(segment_paths);
    }

    let keep_tail = PACK_INDEX_SEGMENT_BOUND.saturating_sub(1);
    let split_at = segment_paths.len().saturating_sub(keep_tail);
    let older_segments = segment_paths[..split_at].to_vec();
    let newer_segments = segment_paths[split_at..].to_vec();
    let compacted_path =
        format!("{PACK_INDEX_COMPACTED_SEGMENT_PREFIX}compact-{layout_generation:020}.json");

    let compacted_bytes =
        build_compacted_segment_bytes(remote, &older_segments, &compacted_path, secrets)?;
    remote.put_physical(&compacted_path, &compacted_bytes)?;

    let mut compacted_segments = vec![compacted_path];
    compacted_segments.extend(newer_segments);
    Ok(compacted_segments)
}

fn build_compacted_segment_bytes<B: BlobStore>(
    remote: &B,
    segment_paths: &[String],
    compacted_path: &str,
    secrets: &RepoSecrets,
) -> Result<Vec<u8>> {
    let mut merged_entries = BTreeMap::<String, AggregatePackIndexEntry>::new();
    for segment_path in segment_paths {
        let bytes = remote.get_physical(segment_path)?;
        for entry in read_segment_entries(segment_path, &bytes, Some(secrets))? {
            merged_entries.insert(entry.object_id.clone(), entry);
        }
    }
    let compacted_index = AggregatePackIndexSegment {
        schema_version: PACK_INDEX_ROOT_SCHEMA_VERSION,
        entries: merged_entries.into_values().collect(),
    };
    encode_pack_index_segment_bytes(
        secrets,
        compacted_path,
        &serde_json::to_vec(&compacted_index)?,
    )
}

fn append_segment_locations<B: BlobStore>(
    remote: &B,
    segment_path: &str,
    segment_bytes: &[u8],
    locations: &mut BTreeMap<String, PackedObjectLocation>,
    secrets: Option<&RepoSecrets>,
) -> Result<()> {
    if segment_path.starts_with(REMOTE_PACK_INDEX_PREFIX) {
        let plaintext = decode_pack_index_segment_plaintext(segment_path, segment_bytes, secrets)?;
        return append_pack_index_locations_from_bytes(remote, &plaintext, locations);
    }

    let plaintext = decode_pack_index_segment_plaintext(segment_path, segment_bytes, secrets)?;
    let aggregate: AggregatePackIndexSegment = serde_json::from_slice(&plaintext)?;
    ensure!(
        aggregate.schema_version == PACK_INDEX_ROOT_SCHEMA_VERSION,
        "unsupported aggregate pack index schema version {}",
        aggregate.schema_version
    );
    for entry in aggregate.entries {
        ensure!(
            entry.data_path.starts_with(REMOTE_PACK_DATA_PREFIX),
            "invalid aggregate pack data path {}",
            entry.data_path
        );
        ensure!(
            !locations.contains_key(&entry.object_id),
            "duplicate packed object id {}",
            entry.object_id
        );
        locations.insert(
            entry.object_id,
            PackedObjectLocation {
                data_path: entry.data_path,
                offset: entry.offset as usize,
                length: entry.length as usize,
            },
        );
    }
    Ok(())
}

fn read_segment_entries(
    segment_path: &str,
    segment_bytes: &[u8],
    secrets: Option<&RepoSecrets>,
) -> Result<Vec<AggregatePackIndexEntry>> {
    if segment_path.starts_with(REMOTE_PACK_INDEX_PREFIX) {
        let plaintext = decode_pack_index_segment_plaintext(segment_path, segment_bytes, secrets)?;
        let index: ObjectPackIndex = serde_json::from_slice(&plaintext)?;
        return Ok(index
            .entries
            .into_iter()
            .map(|entry| AggregatePackIndexEntry {
                object_id: entry.object_id,
                data_path: index.data_path.clone(),
                offset: entry.offset,
                length: entry.length,
            })
            .collect());
    }

    let plaintext = decode_pack_index_segment_plaintext(segment_path, segment_bytes, secrets)?;
    let aggregate: AggregatePackIndexSegment = serde_json::from_slice(&plaintext)?;
    ensure!(
        aggregate.schema_version == PACK_INDEX_ROOT_SCHEMA_VERSION,
        "unsupported aggregate pack index schema version {}",
        aggregate.schema_version
    );
    Ok(aggregate.entries)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use e2v_core::sync_support::open_repo_secrets_for_sync;
    use e2v_core::{InitOptions, RepositoryFacade};
    use e2v_store::{MemoryBackend, ObjectStat};
    use tempfile::tempdir;

    use super::*;
    use crate::pack::build_pack;

    #[derive(Debug, Clone)]
    struct RootProbeBackend {
        inner: MemoryBackend,
        root_exists_calls: Arc<Mutex<usize>>,
        root_get_calls: Arc<Mutex<usize>>,
    }

    impl RootProbeBackend {
        fn new() -> Self {
            Self {
                inner: MemoryBackend::new(),
                root_exists_calls: Arc::new(Mutex::new(0)),
                root_get_calls: Arc::new(Mutex::new(0)),
            }
        }

        fn root_exists_call_count(&self) -> usize {
            *self.root_exists_calls.lock().unwrap()
        }

        fn root_get_call_count(&self) -> usize {
            *self.root_get_calls.lock().unwrap()
        }
    }

    #[derive(Debug, Clone)]
    struct OperationSegmentProbeBackend {
        inner: MemoryBackend,
        segment_exists_calls: Arc<Mutex<usize>>,
        segment_get_calls: Arc<Mutex<usize>>,
    }

    impl OperationSegmentProbeBackend {
        fn new() -> Self {
            Self {
                inner: MemoryBackend::new(),
                segment_exists_calls: Arc::new(Mutex::new(0)),
                segment_get_calls: Arc::new(Mutex::new(0)),
            }
        }

        fn segment_exists_call_count(&self) -> usize {
            *self.segment_exists_calls.lock().unwrap()
        }

        fn segment_get_call_count(&self) -> usize {
            *self.segment_get_calls.lock().unwrap()
        }
    }

    impl BlobStore for RootProbeBackend {
        fn put_physical(&self, relative_path: &str, bytes: &[u8]) -> Result<()> {
            self.inner.put_physical(relative_path, bytes)
        }

        fn put_physical_if_absent(&self, relative_path: &str, bytes: &[u8]) -> Result<bool> {
            self.inner.put_physical_if_absent(relative_path, bytes)
        }

        fn get_physical(&self, relative_path: &str) -> Result<Vec<u8>> {
            if relative_path == PACK_INDEX_ROOT_PATH {
                *self.root_get_calls.lock().unwrap() += 1;
            }
            self.inner.get_physical(relative_path)
        }

        fn get_physical_range(
            &self,
            relative_path: &str,
            offset: usize,
            length: usize,
        ) -> Result<Vec<u8>> {
            self.inner.get_physical_range(relative_path, offset, length)
        }

        fn delete_physical(&self, relative_path: &str) -> Result<()> {
            self.inner.delete_physical(relative_path)
        }

        fn exists_physical(&self, relative_path: &str) -> bool {
            if relative_path == PACK_INDEX_ROOT_PATH {
                *self.root_exists_calls.lock().unwrap() += 1;
            }
            self.inner.exists_physical(relative_path)
        }

        fn stat_physical(&self, relative_path: &str) -> Result<ObjectStat> {
            self.inner.stat_physical(relative_path)
        }

        fn list_physical(&self, prefix: &str) -> Result<Vec<String>> {
            self.inner.list_physical(prefix)
        }
    }

    impl BlobStore for OperationSegmentProbeBackend {
        fn put_physical(&self, relative_path: &str, bytes: &[u8]) -> Result<()> {
            self.inner.put_physical(relative_path, bytes)
        }

        fn put_physical_if_absent(&self, relative_path: &str, bytes: &[u8]) -> Result<bool> {
            self.inner.put_physical_if_absent(relative_path, bytes)
        }

        fn get_physical(&self, relative_path: &str) -> Result<Vec<u8>> {
            if relative_path.starts_with(REMOTE_PACK_INDEX_PREFIX) {
                *self.segment_get_calls.lock().unwrap() += 1;
            }
            self.inner.get_physical(relative_path)
        }

        fn get_physical_range(
            &self,
            relative_path: &str,
            offset: usize,
            length: usize,
        ) -> Result<Vec<u8>> {
            self.inner.get_physical_range(relative_path, offset, length)
        }

        fn delete_physical(&self, relative_path: &str) -> Result<()> {
            self.inner.delete_physical(relative_path)
        }

        fn exists_physical(&self, relative_path: &str) -> bool {
            if relative_path.starts_with(REMOTE_PACK_INDEX_PREFIX) {
                *self.segment_exists_calls.lock().unwrap() += 1;
            }
            self.inner.exists_physical(relative_path)
        }

        fn stat_physical(&self, relative_path: &str) -> Result<ObjectStat> {
            self.inner.stat_physical(relative_path)
        }

        fn list_physical(&self, prefix: &str) -> Result<Vec<String>> {
            self.inner.list_physical(prefix)
        }
    }

    fn seeded_control_dir() -> (tempfile::TempDir, PathBuf, RepoSecrets) {
        let temp = tempdir().unwrap();
        let repo_root = temp.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        RepositoryFacade::new()
            .init(InitOptions {
                repo_root: repo_root.clone(),
                password: "correct horse battery staple".to_string(),
                branch_name: "main".to_string(),
            })
            .unwrap();
        let control_dir = repo_root.join(".e2v");
        let secrets = open_repo_secrets_for_sync(&control_dir).unwrap();
        (temp, control_dir, secrets)
    }

    #[test]
    fn cached_pack_index_segments_restore_locations_without_remote_listing() {
        let (_temp, control_dir, secrets) = seeded_control_dir();
        let remote = MemoryBackend::new();
        let (index, payload) =
            build_pack("op", 0, &[("abc".to_string(), b"hello".to_vec())]).unwrap();
        remote.put_physical(&index.data_path, &payload).unwrap();
        let segment_path = "packs/index/op-00000000.json";
        remote
            .put_physical(
                segment_path,
                &encode_pack_index_segment_bytes(
                    &secrets,
                    segment_path,
                    &serde_json::to_vec(&index).unwrap(),
                )
                .unwrap(),
            )
            .unwrap();
        publish_pack_index_root(
            &remote,
            &secrets,
            "direct",
            1,
            vec![segment_path.to_string()],
        )
        .unwrap();

        let locations =
            load_remote_pack_locations_with_local_cache(&remote, &control_dir, Some(&secrets))
                .unwrap();
        assert!(locations.contains_key("abc"));

        remote.delete_physical(segment_path).unwrap();
        let cached =
            load_remote_pack_locations_with_local_cache(&remote, &control_dir, Some(&secrets))
                .unwrap();
        assert!(cached.contains_key("abc"));
    }

    #[test]
    fn cached_pack_index_segments_can_restore_a_physical_ref_for_object_id() {
        let (_temp, control_dir, secrets) = seeded_control_dir();
        let remote = MemoryBackend::new();
        let (index, payload) =
            build_pack("op", 0, &[("abc".to_string(), b"hello".to_vec())]).unwrap();
        remote.put_physical(&index.data_path, &payload).unwrap();
        let segment_path = "packs/index/op-00000000.json";
        remote
            .put_physical(
                segment_path,
                &encode_pack_index_segment_bytes(
                    &secrets,
                    segment_path,
                    &serde_json::to_vec(&index).unwrap(),
                )
                .unwrap(),
            )
            .unwrap();
        publish_pack_index_root(
            &remote,
            &secrets,
            "direct",
            1,
            vec![segment_path.to_string()],
        )
        .unwrap();

        load_remote_pack_locations_with_local_cache(&remote, &control_dir, Some(&secrets)).unwrap();
        remote.delete_physical(segment_path).unwrap();

        let physical_ref =
            load_cached_pack_physical_ref_for_object_id(&control_dir, "abc").unwrap();

        assert_eq!(physical_ref.layout_id, "pack");
        assert_eq!(physical_ref.container_id, "packs/data/op-00000000.bin");
        assert_eq!(physical_ref.offset, Some(0));
        assert_eq!(physical_ref.length, payload.len() as u64);
    }

    #[test]
    fn missing_pack_index_root_returns_empty_locations_without_segment_listing_fallback() {
        let (_temp, control_dir, secrets) = seeded_control_dir();
        let remote = MemoryBackend::new();
        let (index, payload) =
            build_pack("op", 0, &[("abc".to_string(), b"hello".to_vec())]).unwrap();
        remote.put_physical(&index.data_path, &payload).unwrap();
        let segment_path = "packs/index/op-00000000.json";
        remote
            .put_physical(
                segment_path,
                &encode_pack_index_segment_bytes(
                    &secrets,
                    segment_path,
                    &serde_json::to_vec(&index).unwrap(),
                )
                .unwrap(),
            )
            .unwrap();

        let locations =
            load_remote_pack_locations_with_local_cache(&remote, &control_dir, Some(&secrets))
                .unwrap();

        assert!(
            locations.is_empty(),
            "expected missing root to suppress segment listing discovery"
        );
    }

    #[test]
    fn missing_pack_index_root_uses_single_get_without_exists_probe() {
        let remote = RootProbeBackend::new();

        let root_bytes = load_remote_pack_index_root_bytes(&remote).unwrap();

        assert!(root_bytes.is_none());
        assert_eq!(
            remote.root_exists_call_count(),
            0,
            "missing root should not trigger a separate exists probe"
        );
        assert_eq!(
            remote.root_get_call_count(),
            1,
            "missing root should be determined from a single get attempt"
        );
    }

    #[test]
    fn operation_pack_location_restore_uses_segment_gets_without_exists_probe_per_segment() {
        let (_temp, control_dir, secrets) = seeded_control_dir();
        let remote = OperationSegmentProbeBackend::new();
        let operation_id = "resume-pack-locations";
        let segment_path = "packs/index/resume-pack-locations-00000000.json";
        let (index, payload) =
            build_pack(operation_id, 0, &[("abc".to_string(), b"hello".to_vec())]).unwrap();
        remote.put_physical(&index.data_path, &payload).unwrap();
        remote
            .put_physical(
                segment_path,
                &encode_pack_index_segment_bytes(
                    &secrets,
                    segment_path,
                    &serde_json::to_vec(&index).unwrap(),
                )
                .unwrap(),
            )
            .unwrap();

        let locations =
            load_remote_operation_pack_locations_with_secrets(&remote, operation_id, &secrets)
                .unwrap();

        assert!(locations.contains_key("abc"));
        assert_eq!(
            remote.segment_exists_call_count(),
            0,
            "operation pack location restore should not probe segment existence before reading"
        );
        assert_eq!(
            remote.segment_get_call_count(),
            2,
            "operation pack location restore should read the present segment and then one missing sentinel path"
        );
        let _ = control_dir;
    }

    #[test]
    fn pack_index_root_rejects_plaintext_json_bytes() {
        let (_temp, control_dir, _secrets) = seeded_control_dir();
        let remote = MemoryBackend::new();
        remote
            .put_physical(
                PACK_INDEX_ROOT_PATH,
                br#"{"schema_version":1,"layout_id":"pack","layout_generation":1,"generation":1,"segments":[]}"#,
            )
            .unwrap();

        let error =
            load_remote_pack_locations_with_local_cache(&remote, &control_dir, None).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("pack index root requires unlocked repository or password")
                || error
                    .to_string()
                    .contains("failed to decode authenticated pack index root")
                || error.to_string().contains("ref authentication failed")
                || error.to_string().contains("authentication"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn pack_index_segment_rejects_plaintext_json_bytes() {
        let (_temp, control_dir, secrets) = seeded_control_dir();
        let remote = MemoryBackend::new();
        remote
            .put_physical("packs/data/op-00000000.bin", b"")
            .unwrap();
        remote
            .put_physical(
                PACK_INDEX_ROOT_PATH,
                &encode_pack_index_root_bytes(
                    &secrets,
                    &PackIndexRoot {
                        schema_version: PACK_INDEX_ROOT_SCHEMA_VERSION,
                        layout_id: "pack".to_string(),
                        layout_generation: 1,
                        generation: 1,
                        segments: vec!["packs/index/op-00000000.json".to_string()],
                    },
                )
                .unwrap(),
            )
            .unwrap();
        remote
            .put_physical(
                "packs/index/op-00000000.json",
                br#"{"schema_version":1,"pack_id":"op-00000000","data_path":"packs/data/op-00000000.bin","entries":[]}"#,
            )
            .unwrap();

        let error =
            load_remote_pack_locations_with_local_cache(&remote, &control_dir, Some(&secrets))
                .unwrap_err();

        assert!(
            error.to_string().contains("authentication")
                || error.to_string().contains("pack entry range")
                || error
                    .to_string()
                    .contains("failed to decrypt authenticated pack index segment"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn corrupted_cached_pack_index_segment_is_refetched_once() {
        let (_temp, control_dir, secrets) = seeded_control_dir();
        let remote = MemoryBackend::new();
        let (index, payload) =
            build_pack("op", 0, &[("abc".to_string(), b"hello".to_vec())]).unwrap();
        remote.put_physical(&index.data_path, &payload).unwrap();
        let segment_path = "packs/index/op-00000000.json";
        let segment_plaintext = serde_json::to_vec(&index).unwrap();
        let segment_bytes =
            encode_pack_index_segment_bytes(&secrets, segment_path, &segment_plaintext).unwrap();
        remote.put_physical(segment_path, &segment_bytes).unwrap();
        publish_pack_index_root(
            &remote,
            &secrets,
            "direct",
            1,
            vec![segment_path.to_string()],
        )
        .unwrap();

        let first =
            load_remote_pack_locations_with_local_cache(&remote, &control_dir, Some(&secrets))
                .unwrap();
        assert!(first.contains_key("abc"));

        let cache_path = cached_segment_path(&pack_index_cache_dir(&control_dir), segment_path);
        std::fs::write(&cache_path, b"not valid json").unwrap();

        let refreshed =
            load_remote_pack_locations_with_local_cache(&remote, &control_dir, Some(&secrets))
                .unwrap();
        assert!(refreshed.contains_key("abc"));
        assert_eq!(std::fs::read(cache_path).unwrap(), segment_bytes);
    }

    #[test]
    fn next_pack_index_segment_paths_builds_from_current_root_without_readding_compacted_history() {
        let (_temp, _control_dir, secrets) = seeded_control_dir();
        let remote = MemoryBackend::new();
        for index in 0..6usize {
            let (pack_index, payload) = build_pack(
                &format!("op-{index}"),
                0,
                &[(
                    format!("{index:064x}"),
                    format!("payload-{index}").into_bytes(),
                )],
            )
            .unwrap();
            remote
                .put_physical(&pack_index.data_path, &payload)
                .unwrap();
            let segment_path = format!("packs/index/op-{index}-00000000.json");
            remote
                .put_physical(
                    &segment_path,
                    &encode_pack_index_segment_bytes(
                        &secrets,
                        &segment_path,
                        &serde_json::to_vec(&pack_index).unwrap(),
                    )
                    .unwrap(),
                )
                .unwrap();
        }

        publish_pack_index_root(
            &remote,
            &secrets,
            "direct",
            6,
            list_remote_pack_index_segments(&remote).unwrap(),
        )
        .unwrap();

        let next_segment_path = "packs/index/op-6-00000000.json".to_string();
        let segment_paths = next_pack_index_segment_paths(
            &remote,
            std::slice::from_ref(&next_segment_path),
            Some(&secrets),
        )
        .unwrap();

        assert!(segment_paths.contains(&next_segment_path));
        let compacted_count = segment_paths
            .iter()
            .filter(|path| path.starts_with(PACK_INDEX_COMPACTED_SEGMENT_PREFIX))
            .count();
        assert_eq!(compacted_count, 1);
    }

    #[test]
    fn publish_pack_index_root_compacts_segments_to_bound() {
        let (_temp, _control_dir, secrets) = seeded_control_dir();
        let remote = MemoryBackend::new();
        for index in 0..6usize {
            let (pack_index, payload) = build_pack(
                &format!("op-{index}"),
                0,
                &[(
                    format!("{index:064x}"),
                    format!("payload-{index}").into_bytes(),
                )],
            )
            .unwrap();
            remote
                .put_physical(&pack_index.data_path, &payload)
                .unwrap();
            let segment_path = format!("packs/index/op-{index}-00000000.json");
            remote
                .put_physical(
                    &segment_path,
                    &encode_pack_index_segment_bytes(
                        &secrets,
                        &segment_path,
                        &serde_json::to_vec(&pack_index).unwrap(),
                    )
                    .unwrap(),
                )
                .unwrap();
        }

        publish_pack_index_root(
            &remote,
            &secrets,
            "direct",
            6,
            list_remote_pack_index_segments(&remote).unwrap(),
        )
        .unwrap();

        let root = decode_pack_index_root_bytes(
            &remote.get_physical(PACK_INDEX_ROOT_PATH).unwrap(),
            Some(&secrets),
        )
        .unwrap();
        assert!(root.segments.len() <= PACK_INDEX_SEGMENT_BOUND);
        assert!(
            root.segments
                .iter()
                .any(|path| path.starts_with(PACK_INDEX_COMPACTED_SEGMENT_PREFIX))
        );
    }

    #[test]
    fn pack_index_root_ciphertext_uses_compact_json_plaintext() {
        let (_temp, _control_dir, secrets) = seeded_control_dir();
        let root = PackIndexRoot {
            schema_version: PACK_INDEX_ROOT_SCHEMA_VERSION,
            layout_id: "direct".to_string(),
            layout_generation: 7,
            generation: 7,
            segments: vec![
                "packs/index/op-00000000.json".to_string(),
                "pack-index/segments/compact-00000000000000000005.json".to_string(),
            ],
        };

        let bytes = encode_pack_index_root_bytes(&secrets, &root).unwrap();
        let plaintext = decrypt_control_record_for_sync(
            &secrets,
            PACK_INDEX_ROOT_STABLE_NAME,
            PACK_INDEX_ROOT_OBJECT_TYPE,
            &bytes,
        )
        .unwrap();

        assert_eq!(
            plaintext,
            serde_json::to_vec(&root).unwrap(),
            "pack-index root should not spend ciphertext budget on pretty-printed JSON whitespace"
        );
    }

    #[test]
    fn compacted_pack_index_segment_ciphertext_uses_compact_json_plaintext() {
        let (_temp, _control_dir, secrets) = seeded_control_dir();
        let remote = MemoryBackend::new();
        let mut segment_paths = Vec::new();
        for index in 0..2usize {
            let (pack_index, payload) = build_pack(
                &format!("op-{index}"),
                0,
                &[(
                    format!("{index:064x}"),
                    format!("payload-{index}").into_bytes(),
                )],
            )
            .unwrap();
            remote
                .put_physical(&pack_index.data_path, &payload)
                .unwrap();
            let segment_path = format!("packs/index/op-{index}-00000000.json");
            remote
                .put_physical(
                    &segment_path,
                    &encode_pack_index_segment_bytes(
                        &secrets,
                        &segment_path,
                        &serde_json::to_vec(&pack_index).unwrap(),
                    )
                    .unwrap(),
                )
                .unwrap();
            segment_paths.push(segment_path);
        }

        let compacted_path = "pack-index/segments/compact-00000000000000000002.json";
        let compacted_bytes =
            build_compacted_segment_bytes(&remote, &segment_paths, compacted_path, &secrets)
                .unwrap();
        let plaintext =
            decode_pack_index_segment_plaintext(compacted_path, &compacted_bytes, Some(&secrets))
                .unwrap();
        let aggregate: AggregatePackIndexSegment = serde_json::from_slice(&plaintext).unwrap();
        let compact_json = serde_json::to_vec(&aggregate).unwrap();

        assert_eq!(
            plaintext, compact_json,
            "compacted pack-index segments should not spend ciphertext budget on pretty-printed JSON whitespace"
        );
    }
}
