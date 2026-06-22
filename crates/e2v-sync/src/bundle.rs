use std::collections::BTreeMap;

use anyhow::Result;
use e2v_store::BlobStore;
use serde::{Deserialize, Serialize};

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

pub fn bundle_paths(operation_id: &str, batch_index: usize) -> (String, String, String) {
    let bundle_id = format!("{operation_id}-{batch_index:08}");
    let data_path = format!("{REMOTE_BUNDLE_DATA_PREFIX}{bundle_id}.bin");
    let index_path = format!("{REMOTE_BUNDLE_INDEX_PREFIX}{bundle_id}.json");
    (bundle_id, data_path, index_path)
}

pub fn build_bundle(
    operation_id: &str,
    batch_index: usize,
    objects: &[(String, Vec<u8>)],
) -> Result<(ObjectBundleIndex, Vec<u8>)> {
    let (bundle_id, data_path, _index_path) = bundle_paths(operation_id, batch_index);
    let mut offset = 0usize;
    let mut entries = Vec::with_capacity(objects.len());
    let mut payload = Vec::new();
    for (object_id, bytes) in objects {
        entries.push(ObjectBundleEntry {
            object_id: object_id.clone(),
            offset: offset as u64,
            length: bytes.len() as u64,
        });
        payload.extend_from_slice(bytes);
        offset += bytes.len();
    }
    Ok((
        ObjectBundleIndex {
            schema_version: BUNDLE_SCHEMA_VERSION,
            bundle_id,
            data_path,
            entries,
        },
        payload,
    ))
}

pub fn load_remote_bundle_locations<B: BlobStore>(
    remote: &B,
) -> Result<BTreeMap<String, BundledObjectLocation>> {
    let mut locations = BTreeMap::new();
    for index_path in remote.list_physical(REMOTE_BUNDLE_INDEX_PREFIX)? {
        let index: ObjectBundleIndex = serde_json::from_slice(&remote.get_physical(&index_path)?)?;
        for entry in index.entries {
            locations.insert(
                entry.object_id,
                BundledObjectLocation {
                    data_path: index.data_path.clone(),
                    offset: entry.offset as usize,
                    length: entry.length as usize,
                },
            );
        }
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

#[cfg(test)]
mod tests {
    use e2v_store::{BlobStore, MemoryBackend};

    use super::*;

    #[test]
    fn bundled_object_round_trips_through_index_and_range_reads() {
        let objects = vec![
            ("a".to_string(), b"hello".to_vec()),
            ("b".to_string(), b"world".to_vec()),
        ];
        let (index, payload) = build_bundle("op", 0, &objects).unwrap();
        let (_bundle_id, data_path, index_path) = bundle_paths("op", 0);
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
}
