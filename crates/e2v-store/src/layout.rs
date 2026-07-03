use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LayoutMode {
    Direct,
    Pack,
    Rewrite,
    Oblivious,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DedupMode {
    StablePhysical,
    GenerationScopedRandomized,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutSchedulePolicy {
    pub bucket_bytes: u32,
    pub min_total_reads: u8,
    pub cover_reads_per_request: u8,
    pub reshuffle_after_generations: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutTrafficPolicy {
    pub max_parallel_reads: u8,
    pub inter_read_delay_ms: u16,
    pub burst_budget_bytes: u64,
    pub target_request_window_ms: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutCostPolicy {
    pub profile: String,
    pub max_expected_read_amplification: u8,
    pub max_expected_write_amplification: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutRoot {
    pub schema_version: u32,
    pub layout_id: String,
    pub generation: u64,
    pub mode: LayoutMode,
    pub mapping_policy: String,
    pub dedup_mode: DedupMode,
    pub oblivious_generation: Option<u64>,
    pub schedule_policy: LayoutSchedulePolicy,
    pub traffic_policy: LayoutTrafficPolicy,
    pub cost_policy: LayoutCostPolicy,
}

impl LayoutRoot {
    pub fn direct_default() -> Self {
        Self {
            schema_version: 1,
            layout_id: "direct".to_string(),
            generation: 1,
            mode: LayoutMode::Direct,
            mapping_policy: "loose".to_string(),
            dedup_mode: DedupMode::StablePhysical,
            oblivious_generation: None,
            schedule_policy: LayoutSchedulePolicy {
                bucket_bytes: 0,
                min_total_reads: 1,
                cover_reads_per_request: 0,
                reshuffle_after_generations: 0,
            },
            traffic_policy: LayoutTrafficPolicy {
                max_parallel_reads: 0,
                inter_read_delay_ms: 0,
                burst_budget_bytes: 0,
                target_request_window_ms: 0,
            },
            cost_policy: LayoutCostPolicy {
                profile: "direct".to_string(),
                max_expected_read_amplification: 1,
                max_expected_write_amplification: 1,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DedupMode, LayoutCostPolicy, LayoutMode, LayoutRoot, LayoutSchedulePolicy,
        LayoutTrafficPolicy,
    };

    #[test]
    fn layout_root_round_trips_with_oblivious_metadata_fields() {
        let root = LayoutRoot {
            schema_version: 2,
            layout_id: "oram-v1".to_string(),
            generation: 9,
            mode: LayoutMode::Oblivious,
            mapping_policy: "bucketed-randomized".to_string(),
            dedup_mode: DedupMode::GenerationScopedRandomized,
            oblivious_generation: Some(4),
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
        };

        let bytes = serde_json::to_vec(&root).unwrap();
        let decoded: LayoutRoot = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(decoded, root);
    }
}
