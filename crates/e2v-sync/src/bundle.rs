use std::collections::BTreeMap;
use std::path::{Component, Path};

use anyhow::{Result, ensure};
use e2v_store::BlobStore;
use serde::{Deserialize, Serialize};

use crate::journal::validate_sync_identifier;

const BUNDLE_SCHEMA_VERSION: u32 = 1;
pub const REMOTE_BUNDLE_DATA_PREFIX: &str = "bundles/data/";
pub const REMOTE_BUNDLE_INDEX_PREFIX: &str = "bundles/index/";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectBundleIndex {
    pub schema_version: u32,
    pub bundle_id: String,
    pub data_path: String,
    pub entries: Vec<ObjectBundleEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectBundleEntry {
    pub object_id: String,
    pub offset: u64,
    pub length: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundledObjectLocation {
    pub data_path: String,
    pub offset: usize,
    pub length: usize,
}

pub struct ObjectBundleBuilder {
    bundle_id: String,
    data_path: String,
    entries: Vec<ObjectBundleEntry>,
    payload: Vec<u8>,
}

impl ObjectBundleBuilder {
    pub fn new(operation_id: &str, batch_index: usize) -> Result<Self> {
        let (bundle_id, data_path, _index_path) = bundle_paths(operation_id, batch_index)?;
        Ok(Self {
            bundle_id,
            data_path,
            entries: Vec::new(),
            payload: Vec::new(),
        })
    }

    pub fn push_object(&mut self, object_id: String, bytes: &[u8]) {
        self.entries.push(ObjectBundleEntry {
            object_id,
            offset: self.payload.len() as u64,
            length: bytes.len() as u64,
        });
        self.payload.extend_from_slice(bytes);
    }

    pub fn finish(self) -> (ObjectBundleIndex, Vec<u8>) {
        (
            ObjectBundleIndex {
                schema_version: BUNDLE_SCHEMA_VERSION,
                bundle_id: self.bundle_id,
                data_path: self.data_path,
                entries: self.entries,
            },
            self.payload,
        )
    }
}

pub fn bundle_paths(operation_id: &str, batch_index: usize) -> Result<(String, String, String)> {
    validate_sync_identifier("operation id", operation_id)?;
    let bundle_id = format!("{operation_id}-{batch_index:08}");
    let data_path = format!("{REMOTE_BUNDLE_DATA_PREFIX}{bundle_id}.bin");
    let index_path = format!("{REMOTE_BUNDLE_INDEX_PREFIX}{bundle_id}.json");
    Ok((bundle_id, data_path, index_path))
}

#[cfg(test)]
pub fn build_bundle(
    operation_id: &str,
    batch_index: usize,
    objects: &[(String, Vec<u8>)],
) -> Result<(ObjectBundleIndex, Vec<u8>)> {
    let mut builder = ObjectBundleBuilder::new(operation_id, batch_index)?;
    for (object_id, bytes) in objects {
        builder.push_object(object_id.clone(), bytes);
    }
    Ok(builder.finish())
}

pub fn load_remote_bundle_locations<B: BlobStore>(
    remote: &B,
) -> Result<BTreeMap<String, BundledObjectLocation>> {
    let mut locations = BTreeMap::new();
    for index_path in remote.list_physical(REMOTE_BUNDLE_INDEX_PREFIX)? {
        let index: ObjectBundleIndex = serde_json::from_slice(&remote.get_physical(&index_path)?)?;
        append_bundle_index_locations(remote, index, &mut locations)?;
    }
    Ok(locations)
}

pub fn load_remote_operation_bundle_locations<B: BlobStore>(
    remote: &B,
    operation_id: &str,
) -> Result<BTreeMap<String, BundledObjectLocation>> {
    validate_sync_identifier("operation id", operation_id)?;
    let mut locations = BTreeMap::new();
    let mut batch_index = 0usize;
    loop {
        let (_, _, index_path) = bundle_paths(operation_id, batch_index)?;
        if !remote.exists_physical(&index_path) {
            break;
        }
        let index: ObjectBundleIndex = serde_json::from_slice(&remote.get_physical(&index_path)?)?;
        append_bundle_index_locations(remote, index, &mut locations)?;
        batch_index += 1;
    }
    Ok(locations)
}

pub fn read_bundled_object<B: BlobStore>(
    remote: &B,
    locations: &BTreeMap<String, BundledObjectLocation>,
    object_id: &str,
) -> Result<Option<Vec<u8>>> {
    let Some(location) = locations.get(object_id) else {
        return Ok(None);
    };
    Ok(Some(remote.get_physical_range(
        &location.data_path,
        location.offset,
        location.length,
    )?))
}

fn validate_bundle_relative_path(value: &str) -> Result<()> {
    let path = Path::new(value);
    ensure!(!value.is_empty(), "empty bundle data path");
    ensure!(
        !path.is_absolute(),
        "bundle data path escapes target directory"
    );
    ensure!(
        path.components()
            .all(|component| matches!(component, Component::Normal(_))),
        "bundle data path traversal is not allowed"
    );
    Ok(())
}

fn append_bundle_index_locations<B: BlobStore>(
    remote: &B,
    index: ObjectBundleIndex,
    locations: &mut BTreeMap<String, BundledObjectLocation>,
) -> Result<()> {
    ensure!(
        index.schema_version == BUNDLE_SCHEMA_VERSION,
        "unsupported bundle index schema version {}",
        index.schema_version
    );
    ensure!(
        index.data_path.starts_with(REMOTE_BUNDLE_DATA_PREFIX),
        "invalid bundle data path {}",
        index.data_path
    );
    validate_bundle_relative_path(
        index
            .data_path
            .strip_prefix(REMOTE_BUNDLE_DATA_PREFIX)
            .unwrap_or_default(),
    )?;
    let bundle_len = remote.stat_physical(&index.data_path)?.length;
    let mut previous_end = 0u64;
    for entry in index.entries {
        ensure!(
            !entry.object_id.is_empty()
                && entry
                    .object_id
                    .chars()
                    .all(|character| character.is_ascii_hexdigit()),
            "invalid bundled object id {}",
            entry.object_id
        );
        let entry_end = entry
            .offset
            .checked_add(entry.length)
            .ok_or_else(|| anyhow::anyhow!("invalid bundle entry range for {}", entry.object_id))?;
        ensure!(
            entry.length > 0,
            "invalid bundle entry range for {}",
            entry.object_id
        );
        ensure!(
            entry.offset >= previous_end,
            "bundle entry overlap detected for {}",
            entry.object_id
        );
        ensure!(
            entry_end <= bundle_len,
            "bundle entry range out of bounds for {}",
            entry.object_id
        );
        let object_id = entry.object_id;
        ensure!(
            !locations.contains_key(&object_id),
            "duplicate bundled object id {}",
            object_id
        );
        locations.insert(
            object_id,
            BundledObjectLocation {
                data_path: index.data_path.clone(),
                offset: entry.offset as usize,
                length: entry.length as usize,
            },
        );
        previous_end = entry_end;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use e2v_store::{BlobStore, MemoryBackend};

    use super::*;

    #[test]
    fn bundle_builder_matches_one_shot_bundle_layout() {
        let objects = vec![
            ("a".to_string(), b"hello".to_vec()),
            ("b".to_string(), b"world".to_vec()),
        ];
        let (expected_index, expected_payload) = build_bundle("op", 7, &objects).unwrap();

        let mut builder = ObjectBundleBuilder::new("op", 7).unwrap();
        for (object_id, bytes) in &objects {
            builder.push_object(object_id.clone(), bytes);
        }
        let (index, payload) = builder.finish();

        assert_eq!(index, expected_index);
        assert_eq!(payload, expected_payload);
    }

    #[test]
    fn bundled_object_round_trips_through_index_and_range_reads() {
        let objects = vec![
            ("a".to_string(), b"hello".to_vec()),
            ("b".to_string(), b"world".to_vec()),
        ];
        let (index, payload) = build_bundle("op", 0, &objects).unwrap();
        let (_bundle_id, data_path, index_path) = bundle_paths("op", 0).unwrap();
        let remote = MemoryBackend::new();
        remote.put_physical(&data_path, &payload).unwrap();
        remote
            .put_physical(&index_path, &serde_json::to_vec(&index).unwrap())
            .unwrap();

        let locations = load_remote_bundle_locations(&remote).unwrap();

        assert_eq!(
            read_bundled_object(&remote, &locations, "a")
                .unwrap()
                .unwrap(),
            b"hello"
        );
        assert_eq!(
            read_bundled_object(&remote, &locations, "b")
                .unwrap()
                .unwrap(),
            b"world"
        );
    }

    #[test]
    fn bundle_paths_reject_operation_id_with_path_traversal() {
        let error = bundle_paths("../evil", 0).unwrap_err();

        assert!(error.to_string().contains("operation"));
    }
}
