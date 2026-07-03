use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use e2v_core::sync_support::{
    decrypt_control_record_for_sync, encrypt_control_record_for_sync,
    open_or_unlock_repo_secrets_for_sync,
};
use e2v_store::{BlobStore, RepoSecrets, is_missing_physical_object_error};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::journal::validate_sync_identifier;
use crate::pack::{
    ObjectPackIndex, PackedObjectLocation, REMOTE_PACK_INDEX_PREFIX,
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
    let root_bytes = encode_pack_index_root_bytes(secrets, &root)?;
    if remote
        .get_physical(PACK_INDEX_ROOT_PATH)
        .map(|existing| existing == root_bytes)
        .unwrap_or(false)
    {
        return Ok(());
    }
    remote.put_physical(PACK_INDEX_ROOT_PATH, &root_bytes)
}

pub fn load_remote_pack_locations_with_local_cache<B: BlobStore>(
    remote: &B,
    control_dir: &Path,
    secrets: Option<&RepoSecrets>,
) -> Result<BTreeMap<String, PackedObjectLocation>> {
    let cache_dir = pack_index_cache_dir(control_dir);
    ensure_pack_index_cache_layout(control_dir)?;

    let Some(root_bytes) = load_remote_pack_index_root_bytes(remote)? else {
        prune_pack_index_cache(&cache_dir)?;
        return Ok(BTreeMap::new());
    };
    let root = decode_pack_index_root_bytes(&root_bytes, secrets)?;
    validate_pack_index_root(&root)?;
    prune_stale_cached_segments(&cache_dir, &root.segments)?;

    let mut locations = BTreeMap::new();
    append_remote_pack_locations_from_segment_paths(
        remote,
        &cache_dir,
        secrets,
        &root.segments,
        &mut locations,
    )?;
    persist_root_cache_if_changed(&cache_dir.join("root.json"), &root_bytes)?;
    Ok(locations)
}

pub(crate) fn load_remote_pack_locations_from_segment_paths_with_local_cache<B: BlobStore>(
    remote: &B,
    control_dir: &Path,
    secrets: Option<&RepoSecrets>,
    segment_paths: &[String],
) -> Result<BTreeMap<String, PackedObjectLocation>> {
    let cache_dir = pack_index_cache_dir(control_dir);
    ensure_pack_index_cache_layout(control_dir)?;
    prune_stale_cached_segments(&cache_dir, segment_paths)?;

    let mut locations = BTreeMap::new();
    append_remote_pack_locations_from_segment_paths(
        remote,
        &cache_dir,
        secrets,
        segment_paths,
        &mut locations,
    )?;
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

#[doc(hidden)]
pub(crate) fn decode_pack_index_root_value_for_test(
    control_dir: &Path,
    bytes: &[u8],
) -> Result<Value> {
    let secrets = open_or_unlock_repo_secrets_for_sync(control_dir)?;
    let root = decode_pack_index_root_bytes(bytes, Some(&secrets))?;
    serde_json::to_value(root).map_err(Into::into)
}

#[doc(hidden)]
pub(crate) fn encode_pack_index_root_value_for_test(
    control_dir: &Path,
    value: &Value,
) -> Result<Vec<u8>> {
    let secrets = open_or_unlock_repo_secrets_for_sync(control_dir)?;
    let root: PackIndexRoot = serde_json::from_value(value.clone())?;
    encode_pack_index_root_bytes(&secrets, &root)
}

#[doc(hidden)]
pub(crate) fn decode_pack_index_segment_value_for_test(
    control_dir: &Path,
    segment_path: &str,
    bytes: &[u8],
) -> Result<Value> {
    let secrets = open_or_unlock_repo_secrets_for_sync(control_dir)?;
    let plaintext = decode_pack_index_segment_plaintext(segment_path, bytes, Some(&secrets))?;
    let index: ObjectPackIndex = serde_json::from_slice(&plaintext).with_context(|| {
        format!("failed to decode authenticated pack index segment {segment_path}")
    })?;
    serde_json::to_value(index).map_err(Into::into)
}

#[doc(hidden)]
pub(crate) fn encode_pack_index_segment_value_for_test(
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

fn ensure_pack_index_cache_layout(control_dir: &Path) -> Result<()> {
    let cache_dir = pack_index_cache_dir(control_dir);
    ensure_directory_path(&cache_dir)?;
    ensure_directory_path(&segment_cache_dir(control_dir))
}

fn cached_segment_path(cache_dir: &Path, segment_path: &str) -> PathBuf {
    cache_dir
        .join("segments")
        .join(segment_path.replace('/', "__"))
}

fn remove_path_if_exists(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => {
            std::fs::remove_dir_all(path)?;
        }
        Ok(_) => {
            std::fs::remove_file(path)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn ensure_directory_path(path: &Path) -> Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    if let Some(parent) = path.parent()
        && parent != path
    {
        ensure_directory_path(parent)?;
    }
    remove_path_if_exists(path)?;
    std::fs::create_dir_all(path)?;
    Ok(())
}

fn prepare_cache_file_path(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        ensure_directory_path(parent)?;
    }
    remove_path_if_exists(path)
}

fn prune_stale_cached_segments(cache_dir: &Path, live_segment_paths: &[String]) -> Result<()> {
    let segment_dir = cache_dir.join("segments");
    if !segment_dir.is_dir() {
        return Ok(());
    }

    let live_paths = live_segment_paths
        .iter()
        .map(|segment_path| cached_segment_path(cache_dir, segment_path))
        .collect::<std::collections::BTreeSet<_>>();
    for entry in std::fs::read_dir(&segment_dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_file() && !live_paths.contains(&path) {
            let _ = std::fs::remove_file(path);
        }
    }
    Ok(())
}

fn prune_pack_index_cache(cache_dir: &Path) -> Result<()> {
    let segment_dir = cache_dir.join("segments");
    if segment_dir.is_dir() {
        for entry in std::fs::read_dir(&segment_dir)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    let root_cache_path = cache_dir.join("root.json");
    if root_cache_path.is_file() {
        let _ = std::fs::remove_file(root_cache_path);
    }

    Ok(())
}

fn persist_root_cache_if_changed(cache_path: &Path, root_bytes: &[u8]) -> Result<()> {
    if let Ok(existing_bytes) = std::fs::read(cache_path)
        && existing_bytes == root_bytes
    {
        return Ok(());
    }
    prepare_cache_file_path(cache_path)?;
    std::fs::write(cache_path, root_bytes)?;
    Ok(())
}

fn append_remote_pack_locations_from_segment_paths<B: BlobStore>(
    remote: &B,
    cache_dir: &Path,
    secrets: Option<&RepoSecrets>,
    segment_paths: &[String],
    locations: &mut BTreeMap<String, PackedObjectLocation>,
) -> Result<()> {
    for segment_path in segment_paths {
        validate_segment_path(segment_path)?;
        let cache_path = cached_segment_path(cache_dir, segment_path);
        let segment_bytes =
            load_segment_bytes_with_cache_recovery(remote, segment_path, &cache_path, secrets)?;
        append_segment_locations(remote, segment_path, &segment_bytes, locations, secrets)?;
    }
    Ok(())
}

fn load_segment_bytes_with_cache_recovery<B: BlobStore>(
    remote: &B,
    segment_path: &str,
    cache_path: &Path,
    secrets: Option<&RepoSecrets>,
) -> Result<Vec<u8>> {
    match std::fs::read(cache_path) {
        Ok(cached_bytes) => {
            if read_segment_entries(segment_path, &cached_bytes, secrets).is_ok() {
                return Ok(cached_bytes);
            }
            let _ = remove_path_if_exists(cache_path);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => {
            let _ = remove_path_if_exists(cache_path);
        }
    }

    let bytes = remote.get_physical(segment_path)?;
    prepare_cache_file_path(cache_path)?;
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
    if !remote
        .get_physical(&compacted_path)
        .map(|existing| existing == compacted_bytes)
        .unwrap_or(false)
    {
        remote.put_physical(&compacted_path, &compacted_bytes)?;
    }

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
    let aggregate: AggregatePackIndexSegment =
        serde_json::from_slice(&plaintext).with_context(|| {
            format!("failed to decode authenticated pack index segment {segment_path}")
        })?;
    ensure!(
        aggregate.schema_version == PACK_INDEX_ROOT_SCHEMA_VERSION,
        "unsupported aggregate pack index schema version {}",
        aggregate.schema_version
    );
    for entry in aggregate.entries {
        ensure!(
            !locations.contains_key(&entry.object_id),
            "duplicate packed object id {}",
            entry.object_id
        );
        let location = PackedObjectLocation {
            data_path: entry.data_path,
            offset: entry.offset as usize,
            length: entry.length as usize,
        };
        location.validate()?;
        locations.insert(entry.object_id, location);
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
        let index: ObjectPackIndex = serde_json::from_slice(&plaintext).with_context(|| {
            format!("failed to decode authenticated pack index segment {segment_path}")
        })?;
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
    let aggregate: AggregatePackIndexSegment =
        serde_json::from_slice(&plaintext).with_context(|| {
            format!("failed to decode authenticated pack index segment {segment_path}")
        })?;
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

    #[derive(Debug, Clone)]
    struct PackIndexRootWriteCountingBackend {
        inner: MemoryBackend,
        root_put_calls: Arc<Mutex<usize>>,
    }

    impl PackIndexRootWriteCountingBackend {
        fn new() -> Self {
            Self {
                inner: MemoryBackend::new(),
                root_put_calls: Arc::new(Mutex::new(0)),
            }
        }

        fn root_put_call_count(&self) -> usize {
            *self.root_put_calls.lock().unwrap()
        }
    }

    #[derive(Debug, Clone)]
    struct CompactedSegmentWriteCountingBackend {
        inner: MemoryBackend,
        compacted_segment_put_calls: Arc<Mutex<usize>>,
    }

    impl CompactedSegmentWriteCountingBackend {
        fn new() -> Self {
            Self {
                inner: MemoryBackend::new(),
                compacted_segment_put_calls: Arc::new(Mutex::new(0)),
            }
        }

        fn compacted_segment_put_call_count(&self) -> usize {
            *self.compacted_segment_put_calls.lock().unwrap()
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

    impl BlobStore for PackIndexRootWriteCountingBackend {
        fn put_physical(&self, relative_path: &str, bytes: &[u8]) -> Result<()> {
            if relative_path == PACK_INDEX_ROOT_PATH {
                *self.root_put_calls.lock().unwrap() += 1;
            }
            self.inner.put_physical(relative_path, bytes)
        }

        fn put_physical_if_absent(&self, relative_path: &str, bytes: &[u8]) -> Result<bool> {
            self.inner.put_physical_if_absent(relative_path, bytes)
        }

        fn get_physical(&self, relative_path: &str) -> Result<Vec<u8>> {
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
            self.inner.exists_physical(relative_path)
        }

        fn stat_physical(&self, relative_path: &str) -> Result<ObjectStat> {
            self.inner.stat_physical(relative_path)
        }

        fn list_physical(&self, prefix: &str) -> Result<Vec<String>> {
            self.inner.list_physical(prefix)
        }
    }

    impl BlobStore for CompactedSegmentWriteCountingBackend {
        fn put_physical(&self, relative_path: &str, bytes: &[u8]) -> Result<()> {
            if relative_path.starts_with(PACK_INDEX_COMPACTED_SEGMENT_PREFIX) {
                *self.compacted_segment_put_calls.lock().unwrap() += 1;
            }
            self.inner.put_physical(relative_path, bytes)
        }

        fn put_physical_if_absent(&self, relative_path: &str, bytes: &[u8]) -> Result<bool> {
            self.inner.put_physical_if_absent(relative_path, bytes)
        }

        fn get_physical(&self, relative_path: &str) -> Result<Vec<u8>> {
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

        let repo_root = control_dir.parent().unwrap();
        let physical_ref =
            e2v_core::sync_support::load_cached_pack_physical_ref_for_object_id(repo_root, "abc")
                .unwrap();

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
    fn authenticated_pack_index_segment_reports_decode_failure() {
        let (_temp, control_dir, secrets) = seeded_control_dir();
        let remote = MemoryBackend::new();
        remote
            .put_physical("packs/data/op-00000000.bin", b"payload")
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
                &encode_pack_index_segment_bytes(
                    &secrets,
                    "packs/index/op-00000000.json",
                    br#"{"broken":true"#,
                )
                .unwrap(),
            )
            .unwrap();

        let error =
            load_remote_pack_locations_with_local_cache(&remote, &control_dir, Some(&secrets))
                .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("failed to decode authenticated pack index segment"),
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
    fn cached_pack_index_segment_path_conflict_is_healed_before_refetch() {
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
        std::fs::remove_file(&cache_path).unwrap();
        std::fs::create_dir(&cache_path).unwrap();

        let refreshed =
            load_remote_pack_locations_with_local_cache(&remote, &control_dir, Some(&secrets))
                .unwrap();
        assert!(refreshed.contains_key("abc"));
        assert!(cache_path.is_file());
        assert_eq!(std::fs::read(cache_path).unwrap(), segment_bytes);
    }

    #[test]
    fn pack_index_segment_cache_directory_path_conflict_is_healed_before_refetch() {
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

        let cache_dir = pack_index_cache_dir(&control_dir);
        let segment_dir = segment_cache_dir(&control_dir);
        let cache_path = cached_segment_path(&cache_dir, segment_path);
        std::fs::remove_file(&cache_path).unwrap();
        std::fs::remove_dir(&segment_dir).unwrap();
        std::fs::write(&segment_dir, b"not-a-directory").unwrap();

        let refreshed =
            load_remote_pack_locations_with_local_cache(&remote, &control_dir, Some(&secrets))
                .unwrap();
        assert!(refreshed.contains_key("abc"));
        assert!(segment_dir.is_dir());
        assert!(cache_path.is_file());
        assert_eq!(std::fs::read(cache_path).unwrap(), segment_bytes);
    }

    #[test]
    fn pack_index_root_cache_path_conflict_is_healed_after_segment_restore() {
        let (_temp, control_dir, secrets) = seeded_control_dir();
        let remote = MemoryBackend::new();
        let (index, payload) =
            build_pack("op", 0, &[("abc".to_string(), b"hello".to_vec())]).unwrap();
        remote.put_physical(&index.data_path, &payload).unwrap();
        let segment_path = "packs/index/op-00000000.json";
        let segment_bytes = encode_pack_index_segment_bytes(
            &secrets,
            segment_path,
            &serde_json::to_vec(&index).unwrap(),
        )
        .unwrap();
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

        let root_cache_path = pack_index_cache_dir(&control_dir).join("root.json");
        let expected_root_bytes = remote.get_physical(PACK_INDEX_ROOT_PATH).unwrap();
        std::fs::remove_file(&root_cache_path).unwrap();
        std::fs::create_dir(&root_cache_path).unwrap();

        let refreshed =
            load_remote_pack_locations_with_local_cache(&remote, &control_dir, Some(&secrets))
                .unwrap();
        assert!(refreshed.contains_key("abc"));
        assert!(root_cache_path.is_file());
        assert_eq!(std::fs::read(root_cache_path).unwrap(), expected_root_bytes);
    }

    #[test]
    fn stale_cached_pack_index_segments_are_pruned_when_root_changes() {
        let (_temp, control_dir, secrets) = seeded_control_dir();
        let remote = MemoryBackend::new();

        let (index_one, payload_one) =
            build_pack("op-1", 0, &[("abc".to_string(), b"hello".to_vec())]).unwrap();
        remote
            .put_physical(&index_one.data_path, &payload_one)
            .unwrap();
        let segment_one = "packs/index/op-1-00000000.json";
        remote
            .put_physical(
                segment_one,
                &encode_pack_index_segment_bytes(
                    &secrets,
                    segment_one,
                    &serde_json::to_vec(&index_one).unwrap(),
                )
                .unwrap(),
            )
            .unwrap();
        publish_pack_index_root(
            &remote,
            &secrets,
            "direct",
            1,
            vec![segment_one.to_string()],
        )
        .unwrap();
        load_remote_pack_locations_with_local_cache(&remote, &control_dir, Some(&secrets)).unwrap();
        let stale_cache_path =
            cached_segment_path(&pack_index_cache_dir(&control_dir), segment_one);
        assert!(stale_cache_path.is_file());

        let (index_two, payload_two) =
            build_pack("op-2", 0, &[("def".to_string(), b"world".to_vec())]).unwrap();
        remote
            .put_physical(&index_two.data_path, &payload_two)
            .unwrap();
        let segment_two = "packs/index/op-2-00000000.json";
        remote
            .put_physical(
                segment_two,
                &encode_pack_index_segment_bytes(
                    &secrets,
                    segment_two,
                    &serde_json::to_vec(&index_two).unwrap(),
                )
                .unwrap(),
            )
            .unwrap();
        publish_pack_index_root(
            &remote,
            &secrets,
            "direct",
            2,
            vec![segment_two.to_string()],
        )
        .unwrap();

        let refreshed =
            load_remote_pack_locations_with_local_cache(&remote, &control_dir, Some(&secrets))
                .unwrap();

        assert!(refreshed.contains_key("def"));
        assert!(!refreshed.contains_key("abc"));
        assert!(
            !stale_cache_path.exists(),
            "pack-index cache should prune segment files that are no longer referenced by the current root"
        );
    }

    #[test]
    fn pack_index_cache_is_pruned_when_remote_root_disappears() {
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

        let cached =
            load_remote_pack_locations_with_local_cache(&remote, &control_dir, Some(&secrets))
                .unwrap();
        assert!(cached.contains_key("abc"));

        let cache_dir = pack_index_cache_dir(&control_dir);
        let cached_root_path = cache_dir.join("root.json");
        let cached_segment_path = cached_segment_path(&cache_dir, segment_path);
        assert!(cached_root_path.is_file());
        assert!(cached_segment_path.is_file());

        remote.delete_physical(PACK_INDEX_ROOT_PATH).unwrap();

        let locations =
            load_remote_pack_locations_with_local_cache(&remote, &control_dir, Some(&secrets))
                .unwrap();

        assert!(
            locations.is_empty(),
            "missing pack-index root should still suppress stale local cache restoration"
        );
        assert!(
            !cached_root_path.exists(),
            "pack-index root cache should be pruned when the remote root disappears"
        );
        assert!(
            !cached_segment_path.exists(),
            "pack-index segment cache should be pruned when the remote root disappears"
        );
    }

    #[test]
    fn unchanged_pack_index_root_cache_is_not_rewritten() {
        let (_temp, control_dir, secrets) = seeded_control_dir();
        let remote = MemoryBackend::new();
        let (index, payload) =
            build_pack("op", 0, &[("abc".to_string(), b"hello".to_vec())]).unwrap();
        remote.put_physical(&index.data_path, &payload).unwrap();
        let segment_path = "packs/index/op-00000000.json";
        let segment_bytes = encode_pack_index_segment_bytes(
            &secrets,
            segment_path,
            &serde_json::to_vec(&index).unwrap(),
        )
        .unwrap();
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

        let root_cache_path = pack_index_cache_dir(&control_dir).join("root.json");
        let original_root_bytes = std::fs::read(&root_cache_path).unwrap();
        std::fs::write(&root_cache_path, b"sentinel-root-cache").unwrap();

        let second =
            load_remote_pack_locations_with_local_cache(&remote, &control_dir, Some(&secrets))
                .unwrap();
        assert!(second.contains_key("abc"));
        assert_eq!(
            std::fs::read(&root_cache_path).unwrap(),
            original_root_bytes,
            "pack-index loader should refresh the cached root bytes when the cache file diverges"
        );

        std::fs::write(&root_cache_path, &original_root_bytes).unwrap();
        let unchanged_before = std::fs::metadata(&root_cache_path)
            .unwrap()
            .modified()
            .unwrap();
        let third =
            load_remote_pack_locations_with_local_cache(&remote, &control_dir, Some(&secrets))
                .unwrap();
        assert!(third.contains_key("abc"));
        let unchanged_after = std::fs::metadata(&root_cache_path)
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(
            unchanged_after, unchanged_before,
            "pack-index loader should avoid rewriting an unchanged cached root file"
        );
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
    fn unchanged_pack_index_root_is_not_rewritten_remotely() {
        let (_temp, _control_dir, secrets) = seeded_control_dir();
        let remote = PackIndexRootWriteCountingBackend::new();
        let segment_paths = vec!["packs/index/op-00000000.json".to_string()];

        publish_pack_index_root(&remote, &secrets, "direct", 1, segment_paths.clone()).unwrap();
        assert_eq!(remote.root_put_call_count(), 1);

        publish_pack_index_root(&remote, &secrets, "direct", 1, segment_paths).unwrap();

        assert_eq!(
            remote.root_put_call_count(),
            1,
            "publishing an unchanged pack-index root should not overwrite pack-index/root.json again"
        );
    }

    #[test]
    fn changed_pack_index_root_is_still_rewritten_remotely() {
        let (_temp, _control_dir, secrets) = seeded_control_dir();
        let remote = PackIndexRootWriteCountingBackend::new();

        publish_pack_index_root(
            &remote,
            &secrets,
            "direct",
            1,
            vec!["packs/index/op-00000000.json".to_string()],
        )
        .unwrap();
        assert_eq!(remote.root_put_call_count(), 1);

        publish_pack_index_root(
            &remote,
            &secrets,
            "direct",
            2,
            vec!["packs/index/op-00000001.json".to_string()],
        )
        .unwrap();

        assert_eq!(
            remote.root_put_call_count(),
            2,
            "publishing a changed pack-index root should still rewrite pack-index/root.json"
        );
    }

    #[test]
    fn unchanged_compacted_pack_index_segment_is_not_rewritten_remotely() {
        let (_temp, _control_dir, secrets) = seeded_control_dir();
        let remote = CompactedSegmentWriteCountingBackend::new();
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

        let segment_paths = list_remote_pack_index_segments(&remote).unwrap();
        publish_pack_index_root(&remote, &secrets, "direct", 6, segment_paths.clone()).unwrap();
        assert_eq!(remote.compacted_segment_put_call_count(), 1);

        publish_pack_index_root(&remote, &secrets, "direct", 6, segment_paths).unwrap();

        assert_eq!(
            remote.compacted_segment_put_call_count(),
            1,
            "publishing an unchanged compacted pack-index view should not overwrite the compacted segment again"
        );
    }

    #[test]
    fn changed_compacted_pack_index_segment_is_still_rewritten_remotely() {
        let (_temp, _control_dir, secrets) = seeded_control_dir();
        let remote = CompactedSegmentWriteCountingBackend::new();
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
        assert_eq!(remote.compacted_segment_put_call_count(), 1);

        let extra_segment_path = "packs/index/op-6-00000000.json";
        let (extra_index, extra_payload) =
            build_pack("op-6", 0, &[("f".repeat(64), b"payload-6".to_vec())]).unwrap();
        remote
            .put_physical(&extra_index.data_path, &extra_payload)
            .unwrap();
        remote
            .put_physical(
                extra_segment_path,
                &encode_pack_index_segment_bytes(
                    &secrets,
                    extra_segment_path,
                    &serde_json::to_vec(&extra_index).unwrap(),
                )
                .unwrap(),
            )
            .unwrap();

        publish_pack_index_root(
            &remote,
            &secrets,
            "direct",
            7,
            list_remote_pack_index_segments(&remote).unwrap(),
        )
        .unwrap();

        assert_eq!(
            remote.compacted_segment_put_call_count(),
            2,
            "publishing a changed compacted pack-index view should still rewrite the compacted segment"
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

    #[test]
    fn compacted_pack_index_segment_rejects_traversing_pack_data_paths() {
        let (_temp, control_dir, secrets) = seeded_control_dir();
        let remote = MemoryBackend::new();
        let segment_path = "pack-index/segments/compact-00000000000000000001.json";
        let aggregate = AggregatePackIndexSegment {
            schema_version: PACK_INDEX_ROOT_SCHEMA_VERSION,
            entries: vec![AggregatePackIndexEntry {
                object_id: "abc".to_string(),
                data_path: "packs/data/../escape.bin".to_string(),
                offset: 0,
                length: 5,
            }],
        };
        remote
            .put_physical(
                segment_path,
                &encode_pack_index_segment_bytes(
                    &secrets,
                    segment_path,
                    &serde_json::to_vec(&aggregate).unwrap(),
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

        let error =
            load_remote_pack_locations_with_local_cache(&remote, &control_dir, Some(&secrets))
                .unwrap_err();

        assert!(
            error.to_string().contains("path traversal")
                || error
                    .to_string()
                    .contains("invalid aggregate pack data path"),
            "unexpected error: {error:#}"
        );
    }
}
