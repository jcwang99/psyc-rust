mod capability;
mod layout;
mod layout_root_store;
mod local_backend;
mod logical_object_store;
mod memory_backend;
mod oblivious_layout;
mod opendal_backend;
mod ref_store;
mod storage_layout;

pub use capability::{BackendCapability, ConsistencyClass, WriterMode};
pub use layout::{
    DedupMode, LayoutCostPolicy, LayoutMode, LayoutRoot, LayoutSchedulePolicy, LayoutTrafficPolicy,
};
pub use layout_root_store::{LayoutRootStore, LayoutRootVersion};
#[doc(hidden)]
pub use local_backend::is_missing_physical_object_error;
pub use local_backend::{BlobStore, LocalFolderBackend, ObjectStat};
pub use logical_object_store::{
    DirectLayoutObjectStore, EpochSecrets, LoadedObject, LogicalObjectStore, PhysicalObjectRef,
    RepoSecrets, validate_object_id_value,
};
pub use memory_backend::MemoryBackend;
pub use oblivious_layout::{ObliviousObjectPlacement, ObliviousStorageLayout};
pub use opendal_backend::{
    OpendalMemoryBackend, OpendalS3Backend, OpendalWebdavBackend, RemoteBackend,
    S3CompatibleMockBackend, S3RemoteConfig, WebdavAlistMockBackend, WebdavFlavor,
    WebdavRemoteConfig, WebdavVerifiedCapabilities,
};
pub use ref_store::{
    CasResult, EncryptedRef, ListedRef, RefStore, RefToken, RefVersion, StoredRef,
    validate_ref_token_value,
};
pub use storage_layout::{
    DirectStorageLayout, LayoutObjectLocation, LogicalReadRequest, LogicalResponseWindow,
    PackStorageLayout, PhysicalReadKind, PhysicalReadOp, ReadPlan, StorageLayout,
    TrafficExecutionHint,
};

#[doc(hidden)]
pub mod testing {
    use std::time::SystemTime;

    use anyhow::Result;

    use crate::{LocalFolderBackend, MemoryBackend};

    pub fn override_local_backend_modified_time(
        backend: &LocalFolderBackend,
        relative_path: &str,
        modified_at: SystemTime,
    ) -> Result<()> {
        backend.override_physical_modified_time_for_test(relative_path, modified_at)
    }

    pub fn override_memory_backend_modified_time(
        backend: &MemoryBackend,
        relative_path: &str,
        modified_at: SystemTime,
    ) -> Result<()> {
        backend.override_physical_modified_time_for_test(relative_path, modified_at)
    }
}
