use anyhow::{Result, ensure};

use crate::logical_object_store::{PhysicalObjectRef, validate_object_id_value};

pub enum LayoutObjectLocation<'a> {
    LooseObject {
        object_id: &'a str,
        stored_len: u64,
    },
    PackedObject {
        container_id: &'a str,
        offset: u64,
        length: u64,
    },
}

pub trait StorageLayout {
    fn resolve(&self, location: LayoutObjectLocation<'_>) -> Result<PhysicalObjectRef>;
}

pub struct DirectStorageLayout;

impl StorageLayout for DirectStorageLayout {
    fn resolve(&self, location: LayoutObjectLocation<'_>) -> Result<PhysicalObjectRef> {
        match location {
            LayoutObjectLocation::LooseObject {
                object_id,
                stored_len,
            } => {
                validate_object_id_value(object_id)?;
                Ok(PhysicalObjectRef {
                    layout_id: "direct".to_string(),
                    container_id: format!("objects/{object_id}.json"),
                    offset: None,
                    length: stored_len,
                })
            }
            LayoutObjectLocation::PackedObject { .. } => {
                anyhow::bail!("direct storage layout cannot resolve packed objects")
            }
        }
    }
}

pub struct PackStorageLayout;

impl StorageLayout for PackStorageLayout {
    fn resolve(&self, location: LayoutObjectLocation<'_>) -> Result<PhysicalObjectRef> {
        match location {
            LayoutObjectLocation::PackedObject {
                container_id,
                offset,
                length,
            } => {
                ensure!(
                    container_id.starts_with("packs/data/"),
                    "pack storage layout requires pack data container ids"
                );
                Ok(PhysicalObjectRef::pack(
                    container_id.to_string(),
                    offset,
                    length,
                ))
            }
            LayoutObjectLocation::LooseObject { .. } => {
                anyhow::bail!("pack storage layout cannot resolve loose objects")
            }
        }
    }
}
