mod host_shell_service;
mod preview_service;
mod registry_store;
mod repository_service;
mod search_service;
mod sharing_service;

use std::sync::Arc;

pub use host_shell_service::{HostShellService, RealHostShellService};
pub use preview_service::{
    LocalWebController, MountController, PreviewService, RealPreviewService,
};
pub use registry_store::FsRepositoryRegistryStore;
pub use repository_service::{RealRepositoryService, RepositoryService};
pub use search_service::{RealSearchService, SearchQuery, SearchService};
pub use sharing_service::{
    RealSharingService, ShareActorRow, ShareDeviceRow, SharingRoster, SharingService,
};

#[derive(Debug, Clone)]
pub struct AppServices {
    pub host_shell: Arc<dyn HostShellService>,
    pub preview: Arc<dyn PreviewService>,
    pub repository: Arc<dyn RepositoryService>,
    pub search: Arc<dyn SearchService>,
    pub sharing: Arc<dyn SharingService>,
}

impl AppServices {
    pub fn new(repository: Arc<dyn RepositoryService>) -> Self {
        Self {
            host_shell: Arc::new(RealHostShellService::default()),
            preview: Arc::new(RealPreviewService::default()),
            repository,
            search: Arc::new(RealSearchService::default()),
            sharing: Arc::new(RealSharingService::default()),
        }
    }

    pub fn with_search(mut self, search: Arc<dyn SearchService>) -> Self {
        self.search = search;
        self
    }

    pub fn with_preview(mut self, preview: Arc<dyn PreviewService>) -> Self {
        self.preview = preview;
        self
    }

    pub fn with_sharing(mut self, sharing: Arc<dyn SharingService>) -> Self {
        self.sharing = sharing;
        self
    }

    pub fn with_host_shell(mut self, host_shell: Arc<dyn HostShellService>) -> Self {
        self.host_shell = host_shell;
        self
    }

    pub fn real() -> Self {
        Self::new(Arc::new(RealRepositoryService::default()))
    }
}
