use std::collections::BTreeMap;
use std::path::{Component, Path};

use anyhow::{Result, ensure};
use e2v_store::{
    BlobStore, LayoutObjectLocation, PackStorageLayout, PhysicalObjectRef, StorageLayout,
};
use serde::{Deserialize, Serialize};

use crate::journal::validate_sync_identifier;

const PACK_SCHEMA_VERSION: u32 = 1;
pub const REMOTE_PACK_DATA_PREFIX: &str = "packs/data/";
pub const REMOTE_PACK_INDEX_PREFIX: &str = "packs/index/";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectPackIndex {
    pub schema_version: u32,
    pub pack_id: String,
    pub data_path: String,
    pub entries: Vec<ObjectPackEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectPackEntry {
    pub object_id: String,
    pub offset: u64,
    pub length: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackedObjectLocation {
    pub data_path: String,
    pub offset: usize,
    pub length: usize,
}

impl PackedObjectLocation {
    pub fn physical_ref(&self) -> PhysicalObjectRef {
        PackStorageLayout
            .resolve(LayoutObjectLocation::PackedObject {
                container_id: &self.data_path,
                offset: self.offset as u64,
                length: self.length as u64,
            })
            .expect("packed object locations must resolve through pack storage layout")
    }
}

pub struct ObjectPackBuilder {
    pack_id: String,
    data_path: String,
    entries: Vec<ObjectPackEntry>,
    payload: Vec<u8>,
}

impl ObjectPackBuilder {
    pub fn new(operation_id: &str, batch_index: usize) -> Result<Self> {
        let (pack_id, data_path, _index_path) = pack_paths(operation_id, batch_index)?;
        Ok(Self {
            pack_id,
            data_path,
            entries: Vec::new(),
            payload: Vec::new(),
        })
    }

    pub fn push_object(&mut self, object_id: String, bytes: &[u8]) {
        self.entries.push(ObjectPackEntry {
            object_id,
            offset: self.payload.len() as u64,
            length: bytes.len() as u64,
        });
        self.payload.extend_from_slice(bytes);
    }

    pub fn finish(self) -> (ObjectPackIndex, Vec<u8>) {
        (
            ObjectPackIndex {
                schema_version: PACK_SCHEMA_VERSION,
                pack_id: self.pack_id,
                data_path: self.data_path,
                entries: self.entries,
            },
            self.payload,
        )
    }
}

pub fn pack_paths(operation_id: &str, batch_index: usize) -> Result<(String, String, String)> {
    validate_sync_identifier("operation id", operation_id)?;
    let pack_id = format!("{operation_id}-{batch_index:08}");
    let data_path = format!("{REMOTE_PACK_DATA_PREFIX}{pack_id}.bin");
    let index_path = format!("{REMOTE_PACK_INDEX_PREFIX}{pack_id}.json");
    Ok((pack_id, data_path, index_path))
}

#[cfg(test)]
pub fn build_pack(
    operation_id: &str,
    batch_index: usize,
    objects: &[(String, Vec<u8>)],
) -> Result<(ObjectPackIndex, Vec<u8>)> {
    let mut builder = ObjectPackBuilder::new(operation_id, batch_index)?;
    for (object_id, bytes) in objects {
        builder.push_object(object_id.clone(), bytes);
    }
    Ok(builder.finish())
}

#[cfg(test)]
pub fn load_remote_pack_locations<B: BlobStore>(
    remote: &B,
) -> Result<BTreeMap<String, PackedObjectLocation>> {
    let mut locations = BTreeMap::new();
    for index_path in remote.list_physical(REMOTE_PACK_INDEX_PREFIX)? {
        append_pack_index_locations_from_bytes(
            remote,
            &remote.get_physical(&index_path)?,
            &mut locations,
        )?;
    }
    Ok(locations)
}

pub fn read_packed_object<B: BlobStore>(
    remote: &B,
    locations: &BTreeMap<String, PackedObjectLocation>,
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

fn validate_pack_relative_path(value: &str) -> Result<()> {
    let path = Path::new(value);
    ensure!(!value.is_empty(), "empty pack data path");
    ensure!(
        !path.is_absolute(),
        "pack data path escapes target directory"
    );
    ensure!(
        path.components()
            .all(|component| matches!(component, Component::Normal(_))),
        "pack data path traversal is not allowed"
    );
    Ok(())
}

pub(crate) fn append_pack_index_locations_from_bytes<B: BlobStore>(
    remote: &B,
    index_bytes: &[u8],
    locations: &mut BTreeMap<String, PackedObjectLocation>,
) -> Result<()> {
    let index: ObjectPackIndex = serde_json::from_slice(index_bytes)?;
    append_pack_index_locations(remote, index, locations)
}

fn append_pack_index_locations<B: BlobStore>(
    remote: &B,
    index: ObjectPackIndex,
    locations: &mut BTreeMap<String, PackedObjectLocation>,
) -> Result<()> {
    ensure!(
        index.schema_version == PACK_SCHEMA_VERSION,
        "unsupported pack index schema version {}",
        index.schema_version
    );
    ensure!(
        index.data_path.starts_with(REMOTE_PACK_DATA_PREFIX),
        "invalid pack data path {}",
        index.data_path
    );
    validate_pack_relative_path(
        index
            .data_path
            .strip_prefix(REMOTE_PACK_DATA_PREFIX)
            .unwrap_or_default(),
    )?;
    let pack_len = remote.stat_physical(&index.data_path)?.length;
    let mut previous_end = 0u64;
    for entry in index.entries {
        ensure!(
            !entry.object_id.is_empty()
                && entry
                    .object_id
                    .chars()
                    .all(|character| character.is_ascii_hexdigit()),
            "invalid packed object id {}",
            entry.object_id
        );
        let entry_end = entry
            .offset
            .checked_add(entry.length)
            .ok_or_else(|| anyhow::anyhow!("invalid pack entry range for {}", entry.object_id))?;
        ensure!(
            entry.length > 0,
            "invalid pack entry range for {}",
            entry.object_id
        );
        ensure!(
            entry.offset >= previous_end,
            "pack entry overlap detected for {}",
            entry.object_id
        );
        ensure!(
            entry_end <= pack_len,
            "pack entry range out of bounds for {}",
            entry.object_id
        );
        let object_id = entry.object_id;
        ensure!(
            !locations.contains_key(&object_id),
            "duplicate packed object id {}",
            object_id
        );
        locations.insert(
            object_id,
            PackedObjectLocation {
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
    fn pack_builder_matches_one_shot_pack_layout() {
        let objects = vec![
            ("a".to_string(), b"hello".to_vec()),
            ("b".to_string(), b"world".to_vec()),
        ];
        let (expected_index, expected_payload) = build_pack("op", 7, &objects).unwrap();

        let mut builder = ObjectPackBuilder::new("op", 7).unwrap();
        for (object_id, bytes) in &objects {
            builder.push_object(object_id.clone(), bytes);
        }
        let (index, payload) = builder.finish();

        assert_eq!(index, expected_index);
        assert_eq!(payload, expected_payload);
    }

    #[test]
    fn packed_object_round_trips_through_index_and_range_reads() {
        let objects = vec![
            ("a".to_string(), b"hello".to_vec()),
            ("b".to_string(), b"world".to_vec()),
        ];
        let (index, payload) = build_pack("op", 0, &objects).unwrap();
        let (_pack_id, data_path, index_path) = pack_paths("op", 0).unwrap();
        let remote = MemoryBackend::new();
        remote.put_physical(&data_path, &payload).unwrap();
        remote
            .put_physical(&index_path, &serde_json::to_vec(&index).unwrap())
            .unwrap();

        let locations = load_remote_pack_locations(&remote).unwrap();

        assert_eq!(
            read_packed_object(&remote, &locations, "a")
                .unwrap()
                .unwrap(),
            b"hello"
        );
        assert_eq!(
            read_packed_object(&remote, &locations, "b")
                .unwrap()
                .unwrap(),
            b"world"
        );
    }

    #[test]
    fn pack_paths_reject_operation_id_with_path_traversal() {
        let error = pack_paths("../evil", 0).unwrap_err();

        assert!(error.to_string().contains("operation"));
    }

    #[test]
    fn packed_object_location_exposes_pack_physical_reference() {
        let location = PackedObjectLocation {
            data_path: "packs/data/op-00000000.bin".to_string(),
            offset: 128,
            length: 64,
        };

        let physical_ref = location.physical_ref();

        assert_eq!(physical_ref.layout_id, "pack");
        assert_eq!(physical_ref.container_id, "packs/data/op-00000000.bin");
        assert_eq!(physical_ref.offset, Some(128));
        assert_eq!(physical_ref.length, 64);
    }
}
