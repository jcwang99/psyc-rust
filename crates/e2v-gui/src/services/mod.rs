mod registry_store;
mod repository_service;
mod search_service;
mod sharing_service;

use std::sync::Arc;

pub use registry_store::FsRepositoryRegistryStore;
pub use repository_service::{RealRepositoryService, RepositoryService};
pub use search_service::{RealSearchService, SearchQuery, SearchService};
pub use sharing_service::{
    RealSharingService, ShareActorRow, ShareDeviceRow, SharingRoster, SharingService,
};

#[derive(Debug, Clone)]
pub struct AppServices {
    pub repository: Arc<dyn RepositoryService>,
    pub search: Arc<dyn SearchService>,
    pub sharing: Arc<dyn SharingService>,
}

impl AppServices {
    pub fn new(repository: Arc<dyn RepositoryService>) -> Self {
        Self {
            repository,
            search: Arc::new(RealSearchService::default()),
            sharing: Arc::new(RealSharingService::default()),
        }
    }

    pub fn with_search(mut self, search: Arc<dyn SearchService>) -> Self {
        self.search = search;
        self
    }

    pub fn with_sharing(mut self, sharing: Arc<dyn SharingService>) -> Self {
        self.sharing = sharing;
        self
    }

    pub fn real() -> Self {
        Self::new(Arc::new(RealRepositoryService::default()))
    }
}
