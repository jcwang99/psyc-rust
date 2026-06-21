use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutRoot {
    pub schema_version: u32,
    pub layout_id: String,
    pub generation: u64,
    pub mapping_policy: String,
}
