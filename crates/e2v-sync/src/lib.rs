mod clone;
mod fetch;
mod journal;
mod object_type;
mod oram;
mod pack;
mod pack_cache;
mod pack_index;
mod publisher;
mod push;
mod remote_maintenance;
mod remote_markers;
mod remote_diagnostics;
mod remote_spec;
mod transaction;
mod trusted_state;
mod web;

pub use clone::{CloneOptions, CloneResult, clone_remote};
pub use fetch::{FetchOptions, FetchResult, fetch_remote};
pub use journal::{
    ObjectUploadRecord, ObjectUploadState, OperationId, OperationJournal, OperationMetadata,
    OperationType,
};
pub use oram::{
    EnableObliviousLayoutOptions, ObliviousLayoutPlan, ObliviousLayoutStatus,
    ReshuffleObliviousLayoutOptions, enable_oblivious_layout, plan_oblivious_layout,
    reshuffle_oblivious_layout, status_oblivious_layout,
};
pub use push::{
    PushOptions, PushResult, ResumeOptions, ResumeResult, push_head,
    push_head_with_single_writer_risk, resume_push,
};
pub use remote_maintenance::{
    GcDryRunOptions, GcDryRunReport, GcExecuteCapabilityStatus, GcExecuteOptions, GcExecuteResult,
    HistoricalRewriteOptions, HistoricalRewritePlan, HistoricalRewritePlanOptions,
    HistoricalRewriteResult, RepairRemoteOptions, RepairRemoteResult, VerifyRemoteOptions,
    VerifyRemoteResult, force_accept_remote_rollback, gc_dry_run, gc_execute,
    gc_execute_capability_status, historical_rewrite_remote, plan_historical_rewrite,
    repair_remote, verify_remote,
};
pub use remote_diagnostics::{
    RemoteDiagnosticsOptions, RemoteDiagnosticsPhaseReport, RemoteDiagnosticsReport,
    RemoteDiagnosticsScenario, run_remote_diagnostics,
};
pub use remote_spec::{RemoteBackendRef, RemoteSpec};
pub use trusted_state::TrustedRemoteState;
pub use web::{ServeHandle, ServeOptions, build_local_web_router, serve_local_web};

pub fn load_trusted_remote_state_for_repo(
    repo_id: &str,
) -> anyhow::Result<Option<TrustedRemoteState>> {
    trusted_state::load_trusted_remote_state(repo_id)
}

#[doc(hidden)]
pub mod testing {
    use anyhow::Result;
    use serde_json::Value;
    use std::path::{Path, PathBuf};

    pub fn override_small_object_pack_threshold_for_test(
        threshold: usize,
    ) -> crate::push::SmallObjectPackThresholdGuard {
        crate::push::override_small_object_pack_threshold_for_test(threshold)
    }

    pub fn decode_pack_index_root_value_for_test(
        control_dir: &Path,
        bytes: &[u8],
    ) -> Result<Value> {
        crate::pack_index::decode_pack_index_root_value_for_test(control_dir, bytes)
    }

    pub fn encode_pack_index_root_value_for_test(
        control_dir: &Path,
        value: &Value,
    ) -> Result<Vec<u8>> {
        crate::pack_index::encode_pack_index_root_value_for_test(control_dir, value)
    }

    pub fn decode_pack_index_segment_value_for_test(
        control_dir: &Path,
        segment_path: &str,
        bytes: &[u8],
    ) -> Result<Value> {
        crate::pack_index::decode_pack_index_segment_value_for_test(
            control_dir,
            segment_path,
            bytes,
        )
    }

    pub fn encode_pack_index_segment_value_for_test(
        control_dir: &Path,
        segment_path: &str,
        value: &Value,
    ) -> Result<Vec<u8>> {
        crate::pack_index::encode_pack_index_segment_value_for_test(
            control_dir,
            segment_path,
            value,
        )
    }

    pub fn override_trusted_state_dir_for_test(
        path: PathBuf,
    ) -> crate::trusted_state::TrustedStateDirGuard {
        crate::trusted_state::override_trusted_state_dir_for_test(path)
    }
}
