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
    pub supports_atomic_create_if_absent: bool,
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

    pub fn supports_safe_single_writer_push(&self) -> bool {
        self.supports_remote_lock_or_lease
            && self.supports_atomic_create_if_absent
            && self.supports_transaction_markers
            && (self.supports_reliable_remote_time || self.supports_object_generation_or_etag)
    }

    pub fn push_write_mode(&self) -> WriterMode {
        if self.supports_conditional_put {
            WriterMode::MultiWriter
        } else if self.supports_safe_single_writer_push() {
            WriterMode::SingleWriter
        } else {
            WriterMode::ReadOnly
        }
    }

    pub fn supports_oblivious_layout_updates(&self) -> bool {
        self.supports_oblivious_access_schedule
            && self.supports_range_read
            && (self.supports_layout_root_cas || self.supports_safe_single_writer_push())
            && self.supports_transaction_markers
            && (self.supports_reliable_remote_time || self.supports_object_generation_or_etag)
    }
}

#[cfg(test)]
mod tests {
    use super::{BackendCapability, ConsistencyClass, WriterMode};

    #[test]
    fn oblivious_layout_updates_require_explicit_schedule_support() {
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

        assert_eq!(capability.writer_mode(), WriterMode::MultiWriter);
        assert!(!capability.supports_oblivious_layout_updates());
    }
}
