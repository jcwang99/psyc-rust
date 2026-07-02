mod clone;
mod fetch;
mod journal;
mod object_type;
mod pack;
mod pack_index;
mod publisher;
mod push;
mod remote_maintenance;
mod remote_markers;
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
pub use publisher::{RecoveryAction, SimpleTransactionPublisher, TransactionPublisher};
pub use push::{PushOptions, PushResult, ResumeOptions, ResumeResult, push_head, resume_push};
pub use remote_maintenance::{
    GcDryRunOptions, GcDryRunReport, GcExecuteCapabilityStatus, GcExecuteOptions, GcExecuteResult,
    RepairRemoteOptions, RepairRemoteResult, VerifyRemoteOptions, VerifyRemoteResult,
    force_accept_remote_rollback, gc_dry_run, gc_execute, gc_execute_capability_status,
    repair_remote, verify_remote,
};
pub use remote_spec::{RemoteBackendRef, RemoteSpec};
pub use transaction::{PublishPlan, PublishSession, PublishedObject};
pub use trusted_state::TrustedRemoteState;
pub use web::{ServeHandle, ServeOptions, build_local_web_router, serve_local_web};

pub fn load_trusted_remote_state_for_repo(
    repo_id: &str,
) -> anyhow::Result<Option<TrustedRemoteState>> {
    trusted_state::load_trusted_remote_state(repo_id)
}

#[doc(hidden)]
pub mod testing {
    pub use crate::pack_index::{
        decode_pack_index_root_value_for_test, decode_pack_index_segment_value_for_test,
        encode_pack_index_root_value_for_test, encode_pack_index_segment_value_for_test,
        load_cached_pack_physical_ref_for_object_id,
    };
    pub use crate::trusted_state::override_trusted_state_dir_for_test;

    pub fn override_small_object_pack_threshold_for_test(
        threshold: usize,
    ) -> crate::push::SmallObjectPackThresholdGuard {
        crate::push::override_small_object_pack_threshold_for_test(threshold)
    }
}

#[doc(hidden)]
pub mod benchmarking {
    pub use crate::pack_index::decode_pack_index_root_value_for_test;

    pub fn override_small_object_pack_threshold_for_test(
        threshold: usize,
    ) -> crate::push::SmallObjectPackThresholdGuard {
        crate::push::override_small_object_pack_threshold_for_test(threshold)
    }
}
