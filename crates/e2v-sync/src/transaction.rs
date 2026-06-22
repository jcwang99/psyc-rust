use serde::{Deserialize, Serialize};

use e2v_store::LayoutRoot;
use e2v_store::WriterMode;

use crate::journal::OperationId;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishPlan {
    pub operation_id: OperationId,
    pub target_branch_token: String,
    pub expected_ref_version: Option<u64>,
    pub writer_mode: WriterMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishSession {
    pub operation_id: OperationId,
    pub target_branch_token: String,
    pub expected_ref_version: Option<u64>,
    pub writer_mode: WriterMode,
    pub next_layout_root: Option<LayoutRoot>,
    pub next_layout_root_bytes: Option<Vec<u8>>,
    pub active_intent_path: String,
    pub lease_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishedObject {
    pub object_id: String,
    pub object_type: String,
}
