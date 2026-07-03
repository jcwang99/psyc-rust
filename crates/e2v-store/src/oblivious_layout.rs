use crate::{
    LayoutRoot, LogicalReadRequest, LogicalResponseWindow, PhysicalReadKind, PhysicalReadOp,
    ReadPlan, StorageLayout, TrafficExecutionHint,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObliviousObjectPlacement {
    pub object_id: String,
    pub bucket_path: String,
    pub slot_offset: u64,
    pub slot_length: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObliviousStorageLayout {
    root: LayoutRoot,
    placements: Vec<ObliviousObjectPlacement>,
}

impl ObliviousStorageLayout {
    pub fn new(root: LayoutRoot, placements: Vec<ObliviousObjectPlacement>) -> Self {
        Self { root, placements }
    }

    fn placement_for(&self, object_id: &str) -> anyhow::Result<&ObliviousObjectPlacement> {
        self.placements
            .iter()
            .find(|placement| placement.object_id == object_id)
            .ok_or_else(|| anyhow::anyhow!("missing oblivious placement for object {object_id}"))
    }

    fn bucket_ref(&self, bucket_path: &str) -> crate::PhysicalObjectRef {
        crate::PhysicalObjectRef::pack(
            bucket_path.to_string(),
            0,
            self.root.schedule_policy.bucket_bytes as u64,
        )
    }

    fn cover_bucket_ref(&self, cover_index: usize) -> crate::PhysicalObjectRef {
        let generation = self.root.oblivious_generation.unwrap_or(0);
        self.bucket_ref(&format!(
            "oblivious/data/{:020}/cover-{cover_index:02}.bin",
            generation
        ))
    }
}

impl StorageLayout for ObliviousStorageLayout {
    fn resolve(
        &self,
        _location: crate::LayoutObjectLocation<'_>,
    ) -> anyhow::Result<crate::PhysicalObjectRef> {
        anyhow::bail!("oblivious storage layout does not support direct resolve shortcuts")
    }

    fn plan_logical_read(&self, request: LogicalReadRequest<'_>) -> anyhow::Result<ReadPlan> {
        let placement = self.placement_for(request.object_id)?;
        let mut operations = vec![PhysicalReadOp {
            physical_ref: self.bucket_ref(&placement.bucket_path),
            kind: PhysicalReadKind::Real,
        }];
        for cover_index in 0..self.root.schedule_policy.cover_reads_per_request as usize {
            operations.push(PhysicalReadOp {
                physical_ref: self.cover_bucket_ref(cover_index),
                kind: PhysicalReadKind::Cover,
            });
        }
        while operations.len() < self.root.schedule_policy.min_total_reads as usize {
            let cover_index = operations.len();
            operations.push(PhysicalReadOp {
                physical_ref: self.cover_bucket_ref(cover_index),
                kind: PhysicalReadKind::Cover,
            });
        }

        Ok(ReadPlan {
            layout_id: self.root.layout_id.clone(),
            generation: self.root.generation,
            operations,
            response_window: LogicalResponseWindow {
                logical_offset: request.offset,
                logical_length: request.length,
            },
            traffic_hint: TrafficExecutionHint {
                max_parallel_reads: self.root.traffic_policy.max_parallel_reads,
                inter_read_delay_ms: self.root.traffic_policy.inter_read_delay_ms,
                burst_budget_bytes: self.root.traffic_policy.burst_budget_bytes,
                target_request_window_ms: self.root.traffic_policy.target_request_window_ms,
            },
        })
    }

    fn enumerate_reachable_physical_refs(&self) -> anyhow::Result<Vec<crate::PhysicalObjectRef>> {
        Ok(self
            .placements
            .iter()
            .map(|placement| self.bucket_ref(&placement.bucket_path))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        BackendCapability, ConsistencyClass, DedupMode, LayoutCostPolicy, LayoutMode,
        LayoutSchedulePolicy, LayoutTrafficPolicy, WriterMode,
    };

    const TEST_OBJECT_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn seeded_layout_root(oblivious_generation: u64) -> LayoutRoot {
        LayoutRoot {
            schema_version: 2,
            layout_id: "oram-v1".to_string(),
            generation: 11,
            mode: LayoutMode::Oblivious,
            mapping_policy: "bucketed-randomized".to_string(),
            dedup_mode: DedupMode::GenerationScopedRandomized,
            oblivious_generation: Some(oblivious_generation),
            schedule_policy: LayoutSchedulePolicy {
                bucket_bytes: 4096,
                min_total_reads: 3,
                cover_reads_per_request: 2,
                reshuffle_after_generations: 5,
            },
            traffic_policy: LayoutTrafficPolicy {
                max_parallel_reads: 2,
                inter_read_delay_ms: 15,
                burst_budget_bytes: 16384,
                target_request_window_ms: 90,
            },
            cost_policy: LayoutCostPolicy {
                profile: "balanced".to_string(),
                max_expected_read_amplification: 3,
                max_expected_write_amplification: 4,
            },
        }
    }

    fn seeded_oblivious_layout() -> ObliviousStorageLayout {
        ObliviousStorageLayout::new(
            seeded_layout_root(4),
            vec![ObliviousObjectPlacement {
                object_id: TEST_OBJECT_ID.to_string(),
                bucket_path: "oblivious/data/00000000000000000011/bucket-a.bin".to_string(),
                slot_offset: 512,
                slot_length: 2048,
            }],
        )
    }

    #[test]
    fn oblivious_layout_adds_cover_reads_and_bucket_sized_windows() {
        let layout = seeded_oblivious_layout();

        let plan = layout
            .plan_logical_read(LogicalReadRequest {
                object_id: TEST_OBJECT_ID,
                offset: 7,
                length: 19,
            })
            .unwrap();

        assert_eq!(plan.layout_id, "oram-v1");
        assert_eq!(plan.generation, 11);
        assert_eq!(plan.response_window.logical_offset, 7);
        assert_eq!(plan.response_window.logical_length, 19);
        assert!(plan.operations.len() >= 3);
        assert!(
            plan.operations
                .iter()
                .any(|op| matches!(op.kind, PhysicalReadKind::Cover))
        );
        assert!(
            plan.operations
                .iter()
                .all(|op| op.physical_ref.length == 4096)
        );
    }

    #[test]
    fn reshuffle_can_change_bucket_paths_without_changing_logical_object_id() {
        let before = ObliviousStorageLayout::new(
            seeded_layout_root(4),
            vec![ObliviousObjectPlacement {
                object_id: TEST_OBJECT_ID.to_string(),
                bucket_path: "oblivious/data/00000000000000000011/bucket-a.bin".to_string(),
                slot_offset: 0,
                slot_length: 2048,
            }],
        );
        let after = ObliviousStorageLayout::new(
            seeded_layout_root(5),
            vec![ObliviousObjectPlacement {
                object_id: TEST_OBJECT_ID.to_string(),
                bucket_path: "oblivious/data/00000000000000000012/bucket-z.bin".to_string(),
                slot_offset: 0,
                slot_length: 2048,
            }],
        );

        let before_plan = before
            .plan_logical_read(LogicalReadRequest {
                object_id: TEST_OBJECT_ID,
                offset: 0,
                length: 128,
            })
            .unwrap();
        let after_plan = after
            .plan_logical_read(LogicalReadRequest {
                object_id: TEST_OBJECT_ID,
                offset: 0,
                length: 128,
            })
            .unwrap();

        assert_ne!(
            before_plan.operations[0].physical_ref.container_id,
            after_plan.operations[0].physical_ref.container_id
        );
    }

    #[test]
    fn capability_gate_rejects_backends_without_oblivious_schedule_support() {
        let capability = BackendCapability {
            supports_conditional_put: true,
            supports_range_read: true,
            supports_atomic_rename: true,
            supports_paged_list: true,
            consistency_class: ConsistencyClass::StrongWhitelisted,
            supports_remote_lock_or_lease: true,
            supports_atomic_create_if_absent: true,
            supports_transaction_markers: true,
            supports_reliable_remote_time: true,
            supports_object_generation_or_etag: true,
            supports_layout_root_cas: true,
            supports_oblivious_access_schedule: false,
        };

        assert!(!capability.supports_oblivious_layout_updates());
        assert_eq!(capability.writer_mode(), WriterMode::MultiWriter);
    }
}
