use std::fs;
use std::path::{Path, PathBuf};

use crate::domain::{AppError, RepositoryRegistry};

#[derive(Debug, Clone)]
pub struct FsRepositoryRegistryStore {
    path: PathBuf,
}

impl FsRepositoryRegistryStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<RepositoryRegistry, AppError> {
        match fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
                AppError::invalid_state(format!("invalid registry json: {error}"))
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(RepositoryRegistry::default())
            }
            Err(error) => Err(AppError::io(format!(
                "failed to read registry store {}: {error}",
                self.path.display()
            ))),
        }
    }

    pub fn save(&self, registry: &RepositoryRegistry) -> Result<(), AppError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                AppError::io(format!(
                    "failed to create registry store directory {}: {error}",
                    parent.display()
                ))
            })?;
        }

        let bytes = serde_json::to_vec_pretty(registry).map_err(|error| {
            AppError::internal(format!("failed to serialize registry store: {error}"))
        })?;

        fs::write(&self.path, bytes).map_err(|error| {
            AppError::io(format!(
                "failed to write registry store {}: {error}",
                self.path.display()
            ))
        })
    }
}
