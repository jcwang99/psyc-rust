use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use e2v_store::{DirectLayoutObjectStore, LogicalObjectStore, validate_object_id_value};
use postcard::from_bytes as postcard_from_bytes;
use serde::{Deserialize, Serialize};

use crate::keyring::open_repo_secrets;

const CONTROL_DIR: &str = ".e2v";
const REPO_FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeWalkEntry {
    pub path: String,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestTreeEntry {
    pub name: String,
    pub kind: String,
    pub object_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestTreeObject {
    pub schema_version: u32,
    pub entries: Vec<ManifestTreeEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestFileObject {
    pub schema_version: u32,
    pub entry_name: String,
    pub file_size: u64,
    pub modified_unix_ms: u64,
    pub chunker_id: String,
    pub chunker_config_id: String,
    pub chunk_count: u64,
    pub chunks: Vec<String>,
    pub chunk_lengths: Vec<u64>,
    pub shard_ids: Vec<String>,
    pub shard_byte_lengths: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestSnapshotObject {
    pub schema_version: u32,
    pub message: String,
    pub root_tree_id: String,
    pub parent_snapshot_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestObject {
    Snapshot(ManifestSnapshotObject),
    Tree(ManifestTreeObject),
    File(ManifestFileObject),
}

pub trait ManifestStoreApi {
    fn get_snapshot(&self, id: &str) -> Result<ManifestSnapshotObject>;
    fn get_tree_node(&self, id: &str) -> Result<ManifestTreeObject>;
    fn get_file(&self, id: &str) -> Result<ManifestFileObject>;
    fn get_file_metadata(&self, id: &str) -> Result<ManifestFileObject>;
    fn get_many(&self, ids: &[(&str, &str)]) -> Result<Vec<ManifestObject>>;
    fn walk_tree(&self, snapshot_id: &str) -> Result<Vec<TreeWalkEntry>>;
    fn walk_tree_iter(&self, snapshot_id: &str) -> Result<TreeWalkIter>;
    fn collect_reachable_object_ids(&self, snapshot_id: &str) -> Result<Vec<String>>;
}

#[derive(Debug, Clone)]
pub struct ManifestStore {
    repo_root: PathBuf,
}

impl ManifestStoreApi for ManifestStore {
    fn get_snapshot(&self, id: &str) -> Result<ManifestSnapshotObject> {
        let control_dir = self.repo_root.join(CONTROL_DIR);
        let object_store = open_object_store(&control_dir)?;
        let snapshot = read_snapshot_object(&object_store, id)?;
        Ok(ManifestSnapshotObject {
            schema_version: snapshot.schema_version,
            message: snapshot.message,
            root_tree_id: snapshot.root_tree_id,
            parent_snapshot_id: snapshot.parent_snapshot_id,
        })
    }

    fn get_tree_node(&self, id: &str) -> Result<ManifestTreeObject> {
        let control_dir = self.repo_root.join(CONTROL_DIR);
        let object_store = open_object_store(&control_dir)?;
        let entries = read_directory_entries(&object_store, id)?;
        Ok(ManifestTreeObject {
            schema_version: REPO_FORMAT_VERSION,
            entries: entries
                .into_iter()
                .map(|entry| ManifestTreeEntry {
                    name: entry.name,
                    kind: entry.kind,
                    object_id: entry.object_id,
                })
                .collect(),
        })
    }

    fn get_file(&self, id: &str) -> Result<ManifestFileObject> {
        let control_dir = self.repo_root.join(CONTROL_DIR);
        let object_store = open_object_store(&control_dir)?;
        let file = read_file_object_flattened(&object_store, id)?;
        Ok(manifest_file_from_file_object(file))
    }

    fn get_file_metadata(&self, id: &str) -> Result<ManifestFileObject> {
        let control_dir = self.repo_root.join(CONTROL_DIR);
        let object_store = open_object_store(&control_dir)?;
        let file = read_file_object(&object_store, id)?;
        Ok(manifest_file_from_file_object(file))
    }

    fn get_many(&self, ids: &[(&str, &str)]) -> Result<Vec<ManifestObject>> {
        let mut objects = Vec::with_capacity(ids.len());
        for (id, object_type) in ids {
            let object = match *object_type {
                "snapshot" => ManifestObject::Snapshot(self.get_snapshot(id)?),
                "tree" => ManifestObject::Tree(self.get_tree_node(id)?),
                "file" => ManifestObject::File(self.get_file(id)?),
                other => anyhow::bail!("manifest store get_many does not support type {other}"),
            };
            objects.push(object);
        }
        Ok(objects)
    }

    fn walk_tree(&self, snapshot_id: &str) -> Result<Vec<TreeWalkEntry>> {
        self.walk_tree_iter(snapshot_id)?.collect()
    }

    fn walk_tree_iter(&self, snapshot_id: &str) -> Result<TreeWalkIter> {
        let control_dir = self.repo_root.join(CONTROL_DIR);
        let object_store = Arc::new(open_object_store(&control_dir)?);
        let snapshot = read_snapshot_object(object_store.as_ref(), snapshot_id)?;
        TreeWalkIter::new(object_store, snapshot.root_tree_id)
    }

    fn collect_reachable_object_ids(&self, snapshot_id: &str) -> Result<Vec<String>> {
        let control_dir = self.repo_root.join(CONTROL_DIR);
        let object_store = open_object_store(&control_dir)?;
        let mut reachable = Vec::new();
        let mut seen = HashSet::new();
        let snapshot = read_snapshot_object(&object_store, snapshot_id)?;
        push_reachable_id(&mut reachable, &mut seen, snapshot_id.to_string());
        collect_tree_object_ids(
            &object_store,
            &snapshot.root_tree_id,
            &mut reachable,
            &mut seen,
        )?;
        Ok(reachable)
    }
}

fn manifest_file_from_file_object(file: FileObject) -> ManifestFileObject {
    ManifestFileObject {
        schema_version: file.schema_version,
        entry_name: file.entry_name,
        file_size: file.file_size,
        modified_unix_ms: file.modified_unix_ms,
        chunker_id: file.chunker_id,
        chunker_config_id: file.chunker_config_id,
        chunk_count: file.chunk_count,
        chunks: file.chunks,
        chunk_lengths: file.chunk_lengths,
        shard_ids: file.shard_ids,
        shard_byte_lengths: file.shard_byte_lengths,
    }
}

impl ManifestStore {
    pub fn new(repo_root: impl AsRef<Path>) -> Self {
        Self {
            repo_root: repo_root.as_ref().to_path_buf(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FileObject {
    pub schema_version: u32,
    pub entry_name: String,
    pub file_size: u64,
    pub modified_unix_ms: u64,
    pub chunker_id: String,
    pub chunker_config_id: String,
    pub chunk_count: u64,
    pub chunks: Vec<String>,
    pub chunk_lengths: Vec<u64>,
    pub shard_ids: Vec<String>,
    pub shard_byte_lengths: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FileShardObject {
    pub schema_version: u32,
    pub chunks: Vec<String>,
    pub chunk_lengths: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TreeEntry {
    pub name: String,
    pub kind: String,
    pub object_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TreeObject {
    pub schema_version: u32,
    pub entries: Vec<TreeEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SnapshotObject {
    pub schema_version: u32,
    pub message: String,
    pub root_tree_id: String,
    pub parent_snapshot_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct DirectoryRootObject {
    pub schema_version: u32,
    pub fanout: u32,
    pub shards: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(dead_code)]
struct TreeShardObject {
    pub schema_version: u32,
    pub range_start: String,
    pub range_end: String,
    pub entries: Vec<TreeEntry>,
}

#[derive(Debug, Clone)]
struct PendingDirectory {
    prefix: String,
    entries: Vec<TreeEntry>,
    next_index: usize,
}

pub struct TreeWalkIter {
    object_store: Arc<dyn LogicalObjectStore>,
    stack: Vec<PendingDirectory>,
}

impl TreeWalkIter {
    fn new(object_store: Arc<dyn LogicalObjectStore>, root_tree_id: String) -> Result<Self> {
        let root_entries = read_directory_entries(object_store.as_ref(), &root_tree_id)?;
        Ok(Self {
            object_store,
            stack: vec![PendingDirectory {
                prefix: String::new(),
                entries: root_entries,
                next_index: 0,
            }],
        })
    }
}

impl Iterator for TreeWalkIter {
    type Item = Result<TreeWalkEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let current = self.stack.last_mut()?;
            if current.next_index >= current.entries.len() {
                self.stack.pop();
                continue;
            }

            let entry = current.entries[current.next_index].clone();
            current.next_index += 1;

            let path = if current.prefix.is_empty() {
                entry.name.clone()
            } else {
                format!("{}/{}", current.prefix, entry.name)
            };

            match entry.kind.as_str() {
                "tree" => {
                    match read_directory_entries(self.object_store.as_ref(), &entry.object_id) {
                        Ok(entries) => {
                            self.stack.push(PendingDirectory {
                                prefix: path,
                                entries,
                                next_index: 0,
                            });
                        }
                        Err(error) => return Some(Err(error)),
                    }
                }
                "file" => match read_file_object(self.object_store.as_ref(), &entry.object_id) {
                    Ok(_) => {
                        return Some(Ok(TreeWalkEntry {
                            path,
                            kind: "file".to_string(),
                        }));
                    }
                    Err(error) => return Some(Err(error)),
                },
                _ => {
                    return Some(Err(anyhow::anyhow!(
                        "manifest store encountered unknown tree entry kind {}",
                        entry.kind
                    )));
                }
            }
        }
    }
}

fn open_object_store(control_dir: &Path) -> Result<DirectLayoutObjectStore> {
    let secrets = open_repo_secrets(control_dir)?;
    Ok(DirectLayoutObjectStore::new(control_dir, secrets))
}

fn read_snapshot_object(
    object_store: &dyn LogicalObjectStore,
    object_id: &str,
) -> Result<SnapshotObject> {
    let snapshot: SnapshotObject = read_stored_object(object_store, object_id, "snapshot")?;
    validate_manifest_schema_version("snapshot", snapshot.schema_version)?;
    Ok(snapshot)
}

fn read_directory_entries(
    object_store: &dyn LogicalObjectStore,
    object_id: &str,
) -> Result<Vec<TreeEntry>> {
    let loaded = object_store.get_typed_object(object_id)?;
    match loaded.object_type.as_str() {
        "tree" => {
            let tree: TreeObject = postcard_from_bytes(&loaded.plaintext)
                .context("failed to decode object plaintext")?;
            validate_manifest_schema_version("tree", tree.schema_version)?;
            return Ok(tree.entries);
        }
        "directory_root" => {}
        other => {
            anyhow::bail!("unsupported directory object type {other}");
        }
    }

    let directory_root: DirectoryRootObject =
        postcard_from_bytes(&loaded.plaintext).context("failed to decode object plaintext")?;
    validate_manifest_schema_version("directory_root", directory_root.schema_version)?;
    validate_directory_root_object(&directory_root)?;
    let mut entries = Vec::new();
    for shard_id in directory_root.shards {
        let shard = read_tree_shard_object(object_store, &shard_id)?;
        entries.extend(shard.entries);
    }
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(entries)
}

fn read_file_object(object_store: &dyn LogicalObjectStore, object_id: &str) -> Result<FileObject> {
    let file: FileObject = read_stored_object(object_store, object_id, "file")?;
    validate_manifest_schema_version("file", file.schema_version)?;
    Ok(file)
}

fn read_file_shard_object(
    object_store: &dyn LogicalObjectStore,
    object_id: &str,
) -> Result<FileShardObject> {
    let file_shard: FileShardObject = read_stored_object(object_store, object_id, "file_shard")?;
    validate_manifest_schema_version("file_shard", file_shard.schema_version)?;
    Ok(file_shard)
}

fn read_file_object_flattened(
    object_store: &dyn LogicalObjectStore,
    object_id: &str,
) -> Result<FileObject> {
    let mut file = read_file_object(object_store, object_id)?;
    if file.shard_ids.is_empty() {
        return Ok(file);
    }

    let mut chunk_ids = Vec::new();
    let mut chunk_lengths = Vec::new();
    for shard_id in &file.shard_ids {
        let shard = read_file_shard_object(object_store, shard_id)?;
        ensure!(
            shard.chunks.len() == shard.chunk_lengths.len(),
            "file shard metadata is inconsistent"
        );
        chunk_ids.extend(shard.chunks);
        chunk_lengths.extend(shard.chunk_lengths);
    }
    file.chunks = chunk_ids;
    file.chunk_lengths = chunk_lengths;
    Ok(file)
}

fn read_tree_shard_object(
    object_store: &dyn LogicalObjectStore,
    object_id: &str,
) -> Result<TreeShardObject> {
    let tree_shard: TreeShardObject = read_stored_object(object_store, object_id, "tree_shard")?;
    validate_manifest_schema_version("tree_shard", tree_shard.schema_version)?;
    validate_tree_shard_object(&tree_shard)?;
    Ok(tree_shard)
}

fn collect_tree_object_ids(
    object_store: &dyn LogicalObjectStore,
    tree_id: &str,
    reachable: &mut Vec<String>,
    seen: &mut HashSet<String>,
) -> Result<()> {
    if !push_reachable_id(reachable, seen, tree_id.to_string()) {
        return Ok(());
    }

    let loaded = object_store.get_typed_object(tree_id)?;
    match loaded.object_type.as_str() {
        "tree" => {
            let tree: TreeObject = postcard_from_bytes(&loaded.plaintext)
                .context("failed to decode object plaintext")?;
            validate_manifest_schema_version("tree", tree.schema_version)?;
            collect_tree_entries(object_store, tree.entries, reachable, seen)
        }
        "directory_root" => {
            let directory_root: DirectoryRootObject = postcard_from_bytes(&loaded.plaintext)
                .context("failed to decode object plaintext")?;
            validate_manifest_schema_version("directory_root", directory_root.schema_version)?;
            validate_directory_root_object(&directory_root)?;
            for shard_id in directory_root.shards {
                if !push_reachable_id(reachable, seen, shard_id.clone()) {
                    continue;
                }
                let shard = read_tree_shard_object(object_store, &shard_id)?;
                collect_tree_entries(object_store, shard.entries, reachable, seen)?;
            }
            Ok(())
        }
        other => anyhow::bail!("unsupported directory object type {other}"),
    }
}

fn collect_tree_entries(
    object_store: &dyn LogicalObjectStore,
    entries: Vec<TreeEntry>,
    reachable: &mut Vec<String>,
    seen: &mut HashSet<String>,
) -> Result<()> {
    for entry in entries {
        match entry.kind.as_str() {
            "tree" => collect_tree_object_ids(object_store, &entry.object_id, reachable, seen)?,
            "file" => {
                if push_reachable_id(reachable, seen, entry.object_id.clone()) {
                    let file = read_file_object_flattened(object_store, &entry.object_id)?;
                    for chunk_id in file.chunks {
                        validate_object_id_value(&chunk_id).with_context(|| {
                            format!("invalid chunk id in file {}", entry.object_id)
                        })?;
                        push_reachable_id(reachable, seen, chunk_id);
                    }
                    for shard_id in file.shard_ids {
                        validate_object_id_value(&shard_id).with_context(|| {
                            format!("invalid file shard id in file {}", entry.object_id)
                        })?;
                        push_reachable_id(reachable, seen, shard_id);
                    }
                }
            }
            other => {
                anyhow::bail!("manifest store encountered unknown tree entry kind {other}");
            }
        }
    }
    Ok(())
}

fn validate_directory_root_object(directory_root: &DirectoryRootObject) -> Result<()> {
    ensure!(
        directory_root.fanout as usize == directory_root.shards.len(),
        "directory_root fanout does not match shard count"
    );
    Ok(())
}

fn validate_tree_shard_object(tree_shard: &TreeShardObject) -> Result<()> {
    let Some(first) = tree_shard.entries.first() else {
        ensure!(
            tree_shard.range_start.is_empty() && tree_shard.range_end.is_empty(),
            "tree_shard range metadata must be empty when the shard has no entries"
        );
        return Ok(());
    };
    let last = &tree_shard.entries[tree_shard.entries.len() - 1];
    ensure!(
        tree_shard.range_start == first.name,
        "tree_shard range_start does not match the first entry name"
    );
    ensure!(
        tree_shard.range_end == last.name,
        "tree_shard range_end does not match the last entry name"
    );
    Ok(())
}

fn push_reachable_id(
    reachable: &mut Vec<String>,
    seen: &mut HashSet<String>,
    object_id: String,
) -> bool {
    if seen.insert(object_id.clone()) {
        reachable.push(object_id);
        true
    } else {
        false
    }
}

fn read_stored_object<T: for<'de> Deserialize<'de>>(
    object_store: &dyn LogicalObjectStore,
    object_id: &str,
    expected_type: &str,
) -> Result<T> {
    let plaintext = object_store.get_object(object_id, expected_type)?;
    postcard_from_bytes(&plaintext).context("failed to decode object plaintext")
}

fn validate_manifest_schema_version(object_type: &str, schema_version: u32) -> Result<()> {
    ensure!(
        schema_version == REPO_FORMAT_VERSION,
        "unsupported manifest schema version for {object_type}: {schema_version}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;

    use super::*;
    use crate::keyring::{
        KeyringPointer, KeyringState, seal_repo_secrets, unlock_repo_secrets_uncached,
    };
    use e2v_store::LogicalObjectStore;
    use e2v_store::{EpochSecrets, RepoSecrets};

    fn store_for_tests() -> DirectLayoutObjectStore {
        let temp = tempdir().unwrap();
        let repo_root = temp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();
        let control_dir = repo_root.join(CONTROL_DIR);
        fs::create_dir_all(control_dir.join("objects")).unwrap();
        let secrets = RepoSecrets {
            repo_id: "repo".to_string(),
            active_epoch: 1,
            repo_dedup_key: [1u8; 32],
            repo_ref_key: [4u8; 32],
            repo_manifest_enc_key: [2u8; 32],
            repo_nonce_key: [3u8; 32],
            repo_path_index_key: [5u8; 32],
            epoch_keys: std::collections::BTreeMap::from([(
                1,
                EpochSecrets {
                    manifest_enc_key: [2u8; 32],
                    nonce_key: [3u8; 32],
                },
            )]),
        };
        DirectLayoutObjectStore::new(control_dir, secrets)
    }

    #[derive(Clone)]
    struct CountingObjectStore {
        inner: DirectLayoutObjectStore,
        loads: Arc<Mutex<HashMap<String, usize>>>,
    }

    impl CountingObjectStore {
        fn new(inner: DirectLayoutObjectStore) -> Self {
            Self {
                inner,
                loads: Arc::new(Mutex::new(HashMap::new())),
            }
        }

        fn record_load(&self, object_id: &str) {
            let mut loads = self.loads.lock().unwrap();
            *loads.entry(object_id.to_string()).or_insert(0) += 1;
        }

        fn get_load_count(&self, object_id: &str) -> usize {
            self.loads
                .lock()
                .unwrap()
                .get(object_id)
                .copied()
                .unwrap_or(0)
        }
    }

    impl LogicalObjectStore for CountingObjectStore {
        fn put_object(&self, object_type: &str, plaintext: &[u8]) -> Result<String> {
            self.inner.put_object(object_type, plaintext)
        }

        fn get_typed_object(&self, object_id: &str) -> Result<e2v_store::LoadedObject> {
            self.record_load(object_id);
            self.inner.get_typed_object(object_id)
        }

        fn get_object(&self, object_id: &str, expected_type: &str) -> Result<Vec<u8>> {
            let _ = expected_type;
            self.record_load(object_id);
            self.inner.get_object(object_id, expected_type)
        }

        fn get_object_range(
            &self,
            object_id: &str,
            expected_type: &str,
            offset: usize,
            length: usize,
        ) -> Result<Vec<u8>> {
            self.inner
                .get_object_range(object_id, expected_type, offset, length)
        }

        fn exists_object(&self, object_id: &str) -> bool {
            self.inner.exists_object(object_id)
        }

        fn resolve_object(&self, object_id: &str) -> Result<e2v_store::PhysicalObjectRef> {
            self.inner.resolve_object(object_id)
        }
    }

    #[test]
    fn read_tree_shard_object_round_trips_when_supported() {
        let store = store_for_tests();
        let object_id = store
            .put_object(
                "tree_shard",
                &postcard::to_stdvec(&TreeShardObject {
                    schema_version: REPO_FORMAT_VERSION,
                    range_start: String::new(),
                    range_end: String::new(),
                    entries: vec![],
                })
                .unwrap(),
            )
            .unwrap();

        let object = read_tree_shard_object(&store, &object_id).unwrap();
        assert_eq!(object.range_start, "");
    }

    #[test]
    fn manifest_decoding_helpers_work_through_logical_object_store_trait() {
        let store = store_for_tests();
        let object_id = store
            .put_object(
                "tree_shard",
                &postcard::to_stdvec(&TreeShardObject {
                    schema_version: REPO_FORMAT_VERSION,
                    range_start: String::new(),
                    range_end: String::new(),
                    entries: vec![],
                })
                .unwrap(),
            )
            .unwrap();
        let trait_store: &dyn LogicalObjectStore = &store;

        let object = read_tree_shard_object(trait_store, &object_id).unwrap();

        assert_eq!(object.range_end, "");
    }

    #[test]
    fn read_directory_entries_only_reads_directory_root_once() {
        let base_store = store_for_tests();
        let shard_id = base_store
            .put_object(
                "tree_shard",
                &postcard::to_stdvec(&TreeShardObject {
                    schema_version: REPO_FORMAT_VERSION,
                    range_start: "nested.txt".to_string(),
                    range_end: "nested.txt".to_string(),
                    entries: vec![TreeEntry {
                        name: "nested.txt".to_string(),
                        kind: "file".to_string(),
                        object_id: "child-file".to_string(),
                    }],
                })
                .unwrap(),
            )
            .unwrap();
        let directory_root_id = base_store
            .put_object(
                "directory_root",
                &postcard::to_stdvec(&DirectoryRootObject {
                    schema_version: REPO_FORMAT_VERSION,
                    fanout: 1,
                    shards: vec![shard_id],
                })
                .unwrap(),
            )
            .unwrap();
        let counting_store = CountingObjectStore::new(base_store);

        let entries = read_directory_entries(&counting_store, &directory_root_id).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "nested.txt");
        assert_eq!(counting_store.get_load_count(&directory_root_id), 1);
    }

    #[test]
    fn read_directory_entries_rejects_directory_root_with_mismatched_fanout() {
        let store = store_for_tests();
        let shard_id = store
            .put_object(
                "tree_shard",
                &postcard::to_stdvec(&TreeShardObject {
                    schema_version: REPO_FORMAT_VERSION,
                    range_start: "a".to_string(),
                    range_end: "z".to_string(),
                    entries: vec![],
                })
                .unwrap(),
            )
            .unwrap();
        let directory_root_id = store
            .put_object(
                "directory_root",
                &postcard::to_stdvec(&DirectoryRootObject {
                    schema_version: REPO_FORMAT_VERSION,
                    fanout: 2,
                    shards: vec![shard_id],
                })
                .unwrap(),
            )
            .unwrap();

        let error = read_directory_entries(&store, &directory_root_id).unwrap_err();
        assert!(
            error.to_string().contains("fanout"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn read_tree_shard_object_rejects_range_metadata_mismatch() {
        let store = store_for_tests();
        let object_id = store
            .put_object(
                "tree_shard",
                &postcard::to_stdvec(&TreeShardObject {
                    schema_version: REPO_FORMAT_VERSION,
                    range_start: "b".to_string(),
                    range_end: "y".to_string(),
                    entries: vec![TreeEntry {
                        name: "a".to_string(),
                        kind: "file".to_string(),
                        object_id: "child".to_string(),
                    }],
                })
                .unwrap(),
            )
            .unwrap();

        let error = read_tree_shard_object(&store, &object_id).unwrap_err();
        assert!(
            error.to_string().contains("range"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn read_file_object_rejects_missing_required_fields() {
        let store = store_for_tests();
        let object_id = store
            .put_object(
                "file",
                &postcard::to_stdvec(&serde_json::json!({
                    "schema_version": REPO_FORMAT_VERSION,
                    "entry_name": "hello.txt",
                    "file_size": 5u64
                }))
                .unwrap(),
            )
            .unwrap();

        let error = read_file_object(&store, &object_id).unwrap_err();

        assert!(
            error.to_string().contains("decode")
                || error.to_string().contains("failed")
                || error.to_string().contains("unexpected end"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn open_repo_secrets_follows_current_keyring_pointer() {
        let temp = tempdir().unwrap();
        let repo_root = temp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();
        let control_dir = repo_root.join(CONTROL_DIR);
        fs::create_dir_all(control_dir.join("keyring")).unwrap();

        let secrets_one = RepoSecrets {
            repo_id: "repo".to_string(),
            active_epoch: 1,
            repo_dedup_key: [1u8; 32],
            repo_ref_key: [4u8; 32],
            repo_manifest_enc_key: [2u8; 32],
            repo_nonce_key: [3u8; 32],
            repo_path_index_key: [5u8; 32],
            epoch_keys: std::collections::BTreeMap::from([(
                1,
                EpochSecrets {
                    manifest_enc_key: [2u8; 32],
                    nonce_key: [3u8; 32],
                },
            )]),
        };
        let secrets_two = RepoSecrets {
            repo_id: "repo".to_string(),
            active_epoch: 1,
            repo_dedup_key: [1u8; 32],
            repo_ref_key: [4u8; 32],
            repo_manifest_enc_key: [8u8; 32],
            repo_nonce_key: [3u8; 32],
            repo_path_index_key: [5u8; 32],
            epoch_keys: std::collections::BTreeMap::from([(
                1,
                EpochSecrets {
                    manifest_enc_key: [8u8; 32],
                    nonce_key: [3u8; 32],
                },
            )]),
        };
        let keyring_one = KeyringState {
            format_version: REPO_FORMAT_VERSION,
            generation: 1,
            repo_id: "repo".to_string(),
            active_epoch: 1,
            crypto_suite: "xchacha20poly1305".to_string(),
            kdf: "argon2id".to_string(),
            actors: vec![],
            devices: vec![],
            epochs: vec![],
            envelopes: vec![
                seal_repo_secrets("repo", 1, "password", &secrets_one, "len:8".to_string())
                    .unwrap(),
            ],
        };
        let mut keyring_two = keyring_one.clone();
        keyring_two.generation = 2;
        keyring_two.envelopes = vec![
            seal_repo_secrets("repo", 1, "password", &secrets_two, "len:8".to_string()).unwrap(),
        ];

        fs::write(
            control_dir.join("keyring").join("keyring.1"),
            serde_json::to_vec_pretty(&keyring_one).unwrap(),
        )
        .unwrap();
        fs::write(
            control_dir.join("keyring").join("keyring.2"),
            serde_json::to_vec_pretty(&keyring_two).unwrap(),
        )
        .unwrap();
        fs::write(
            control_dir.join("keyring").join("keyring.current"),
            serde_json::to_vec_pretty(&KeyringPointer {
                generation: 2,
                current: "keyring.2".to_string(),
            })
            .unwrap(),
        )
        .unwrap();

        let secrets = unlock_repo_secrets_uncached(&control_dir, "password").unwrap();

        assert_eq!(secrets.repo_manifest_enc_key, [8u8; 32]);
    }

    #[test]
    fn open_repo_secrets_rejects_pointer_generation_mismatch() {
        let temp = tempdir().unwrap();
        let repo_root = temp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();
        let control_dir = repo_root.join(CONTROL_DIR);
        fs::create_dir_all(control_dir.join("keyring")).unwrap();

        let secrets = RepoSecrets {
            repo_id: "repo".to_string(),
            active_epoch: 1,
            repo_dedup_key: [1u8; 32],
            repo_ref_key: [4u8; 32],
            repo_manifest_enc_key: [2u8; 32],
            repo_nonce_key: [3u8; 32],
            repo_path_index_key: [5u8; 32],
            epoch_keys: std::collections::BTreeMap::from([(
                1,
                EpochSecrets {
                    manifest_enc_key: [2u8; 32],
                    nonce_key: [3u8; 32],
                },
            )]),
        };
        let keyring = KeyringState {
            format_version: REPO_FORMAT_VERSION,
            generation: 1,
            repo_id: "repo".to_string(),
            active_epoch: 1,
            crypto_suite: "xchacha20poly1305".to_string(),
            kdf: "argon2id".to_string(),
            actors: vec![],
            devices: vec![],
            epochs: vec![],
            envelopes: vec![
                seal_repo_secrets("repo", 1, "password", &secrets, "len:8".to_string()).unwrap(),
            ],
        };

        fs::write(
            control_dir.join("keyring").join("keyring.2"),
            serde_json::to_vec_pretty(&keyring).unwrap(),
        )
        .unwrap();
        fs::write(
            control_dir.join("keyring").join("keyring.current"),
            serde_json::to_vec_pretty(&KeyringPointer {
                generation: 2,
                current: "keyring.2".to_string(),
            })
            .unwrap(),
        )
        .unwrap();

        let error = unlock_repo_secrets_uncached(&control_dir, "password").unwrap_err();

        assert!(
            error.to_string().contains("generation") || error.to_string().contains("mismatch"),
            "unexpected error: {error:#}"
        );
    }
}
