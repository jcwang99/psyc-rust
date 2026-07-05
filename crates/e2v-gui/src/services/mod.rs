mod registry_store;
mod repository_service;
mod search_service;

use std::sync::Arc;

pub use registry_store::FsRepositoryRegistryStore;
pub use repository_service::{RealRepositoryService, RepositoryService};
pub use search_service::{RealSearchService, SearchQuery, SearchService};

#[derive(Debug, Clone)]
pub struct AppServices {
    pub repository: Arc<dyn RepositoryService>,
    pub search: Arc<dyn SearchService>,
}

impl AppServices {
    pub fn new(repository: Arc<dyn RepositoryService>) -> Self {
        Self {
            repository,
            search: Arc::new(RealSearchService::default()),
        }
    }

    pub fn with_search(mut self, search: Arc<dyn SearchService>) -> Self {
        self.search = search;
        self
    }

    pub fn real() -> Self {
        Self::new(Arc::new(RealRepositoryService::default()))
    }
}
