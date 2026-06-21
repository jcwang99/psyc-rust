use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConsistencyClass {
    StrongWhitelisted,
    UnknownOrEventual,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WriterMode {
    MultiWriter,
    SingleWriter,
    ReadOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendCapability {
    pub supports_conditional_put: bool,
    pub supports_range_read: bool,
    pub supports_atomic_rename: bool,
    pub supports_paged_list: bool,
    pub consistency_class: ConsistencyClass,
    pub supports_remote_lock_or_lease: bool,
    pub supports_transaction_markers: bool,
    pub supports_reliable_remote_time: bool,
    pub supports_object_generation_or_etag: bool,
    pub supports_layout_root_cas: bool,
    pub supports_oblivious_access_schedule: bool,
}

impl BackendCapability {
    pub fn writer_mode(&self) -> WriterMode {
        if self.supports_conditional_put {
            WriterMode::MultiWriter
        } else if self.supports_remote_lock_or_lease {
            WriterMode::SingleWriter
        } else {
            WriterMode::ReadOnly
        }
    }
}
