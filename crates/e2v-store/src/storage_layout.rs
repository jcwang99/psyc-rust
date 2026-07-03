use anyhow::{Result, ensure};

use crate::logical_object_store::{PhysicalObjectRef, validate_object_id_value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogicalReadRequest<'a> {
    pub object_id: &'a str,
    pub offset: u64,
    pub length: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhysicalReadKind {
    Real,
    Cover,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhysicalReadOp {
    pub physical_ref: PhysicalObjectRef,
    pub kind: PhysicalReadKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogicalResponseWindow {
    pub logical_offset: u64,
    pub logical_length: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrafficExecutionHint {
    pub max_parallel_reads: u8,
    pub inter_read_delay_ms: u16,
    pub burst_budget_bytes: u64,
    pub target_request_window_ms: u32,
}

impl TrafficExecutionHint {
    fn no_shaping() -> Self {
        Self {
            max_parallel_reads: 0,
            inter_read_delay_ms: 0,
            burst_budget_bytes: 0,
            target_request_window_ms: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadPlan {
    pub layout_id: String,
    pub generation: u64,
    pub operations: Vec<PhysicalReadOp>,
    pub response_window: LogicalResponseWindow,
    pub traffic_hint: TrafficExecutionHint,
}

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
    fn plan_logical_read(&self, request: LogicalReadRequest<'_>) -> Result<ReadPlan>;
    fn enumerate_reachable_physical_refs(&self) -> Result<Vec<PhysicalObjectRef>>;
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

    fn plan_logical_read(&self, request: LogicalReadRequest<'_>) -> Result<ReadPlan> {
        let physical_ref = self.resolve(LayoutObjectLocation::LooseObject {
            object_id: request.object_id,
            stored_len: request.length,
        })?;
        Ok(ReadPlan {
            layout_id: "direct".to_string(),
            generation: 1,
            operations: vec![PhysicalReadOp {
                physical_ref,
                kind: PhysicalReadKind::Real,
            }],
            response_window: LogicalResponseWindow {
                logical_offset: request.offset,
                logical_length: request.length,
            },
            traffic_hint: TrafficExecutionHint::no_shaping(),
        })
    }

    fn enumerate_reachable_physical_refs(&self) -> Result<Vec<PhysicalObjectRef>> {
        Ok(Vec::new())
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

    fn plan_logical_read(&self, request: LogicalReadRequest<'_>) -> Result<ReadPlan> {
        let physical_ref = self.resolve(LayoutObjectLocation::PackedObject {
            container_id: "packs/data/test-pack.bin",
            offset: request.offset,
            length: request.length,
        })?;
        Ok(ReadPlan {
            layout_id: "pack".to_string(),
            generation: 1,
            operations: vec![PhysicalReadOp {
                physical_ref,
                kind: PhysicalReadKind::Real,
            }],
            response_window: LogicalResponseWindow {
                logical_offset: request.offset,
                logical_length: request.length,
            },
            traffic_hint: TrafficExecutionHint::no_shaping(),
        })
    }

    fn enumerate_reachable_physical_refs(&self) -> Result<Vec<PhysicalObjectRef>> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DirectStorageLayout, LayoutObjectLocation, LogicalReadRequest, PackStorageLayout,
        PhysicalReadKind, StorageLayout,
    };

    #[test]
    fn direct_storage_layout_resolves_loose_object_path() {
        let layout = DirectStorageLayout;

        let reference = layout
            .resolve(LayoutObjectLocation::LooseObject {
                object_id: "abc123",
                stored_len: 42,
            })
            .unwrap();

        assert_eq!(reference.layout_id, "direct");
        assert_eq!(reference.container_id, "objects/abc123.json");
        assert_eq!(reference.offset, None);
        assert_eq!(reference.length, 42);
    }

    #[test]
    fn pack_storage_layout_resolves_pack_container_offset_and_length() {
        let layout = PackStorageLayout;

        let reference = layout
            .resolve(LayoutObjectLocation::PackedObject {
                container_id: "packs/data/pack-01.bin",
                offset: 128,
                length: 4096,
            })
            .unwrap();

        assert_eq!(reference.layout_id, "pack");
        assert_eq!(reference.container_id, "packs/data/pack-01.bin");
        assert_eq!(reference.offset, Some(128));
        assert_eq!(reference.length, 4096);
    }

    #[test]
    fn direct_storage_layout_plans_one_real_read_without_cover_reads() {
        let layout = DirectStorageLayout;

        let plan = layout
            .plan_logical_read(LogicalReadRequest {
                object_id: "abc123",
                offset: 7,
                length: 19,
            })
            .unwrap();

        assert_eq!(plan.layout_id, "direct");
        assert_eq!(plan.generation, 1);
        assert_eq!(plan.operations.len(), 1);
        assert!(matches!(plan.operations[0].kind, PhysicalReadKind::Real));
        assert_eq!(
            plan.operations[0].physical_ref.container_id,
            "objects/abc123.json"
        );
        assert_eq!(plan.response_window.logical_offset, 7);
        assert_eq!(plan.response_window.logical_length, 19);
    }

    #[test]
    fn pack_storage_layout_plans_one_real_read_without_cover_reads() {
        let layout = PackStorageLayout;

        let plan = layout
            .plan_logical_read(LogicalReadRequest {
                object_id: "ignored-for-pack-plan",
                offset: 128,
                length: 64,
            })
            .unwrap();

        assert_eq!(plan.layout_id, "pack");
        assert_eq!(plan.generation, 1);
        assert_eq!(plan.operations.len(), 1);
        assert!(matches!(plan.operations[0].kind, PhysicalReadKind::Real));
        assert_eq!(
            plan.operations[0].physical_ref.container_id,
            "packs/data/test-pack.bin"
        );
        assert_eq!(plan.operations[0].physical_ref.offset, Some(128));
        assert_eq!(plan.operations[0].physical_ref.length, 64);
        assert_eq!(plan.response_window.logical_offset, 128);
        assert_eq!(plan.response_window.logical_length, 64);
    }
}
