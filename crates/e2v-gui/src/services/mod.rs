mod registry_store;
mod repository_service;

use std::sync::Arc;

pub use registry_store::FsRepositoryRegistryStore;
pub use repository_service::{RealRepositoryService, RepositoryService};

#[derive(Debug, Clone)]
pub struct AppServices {
    pub repository: Arc<dyn RepositoryService>,
}

impl AppServices {
    pub fn new(repository: Arc<dyn RepositoryService>) -> Self {
        Self { repository }
    }

    pub fn real() -> Self {
        Self::new(Arc::new(RealRepositoryService::default()))
    }
}
