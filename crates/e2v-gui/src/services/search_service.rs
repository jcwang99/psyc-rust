use std::path::PathBuf;

use e2v_core::{MetadataSearchQuery, RepositoryFacade};

use crate::domain::AppError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchQuery {
    pub query_text: String,
    pub path_prefix: Option<String>,
}

pub trait SearchService: Send + Sync + std::fmt::Debug + 'static {
    fn search(
        &self,
        repo_root: PathBuf,
        branch_token: String,
        head_snapshot_id: Option<String>,
        query: SearchQuery,
    ) -> Result<Vec<crate::pages::search::SearchResultRow>, AppError>;
}

#[derive(Debug)]
pub struct RealSearchService {
    facade: RepositoryFacade,
}

impl Default for RealSearchService {
    fn default() -> Self {
        Self {
            facade: RepositoryFacade::new(),
        }
    }
}

impl SearchService for RealSearchService {
    fn search(
        &self,
        repo_root: PathBuf,
        _branch_token: String,
        _head_snapshot_id: Option<String>,
        query: SearchQuery,
    ) -> Result<Vec<crate::pages::search::SearchResultRow>, AppError> {
        let query_text = query.query_text.trim().to_owned();
        if query_text.is_empty() {
            return Ok(Vec::new());
        }

        let filename_hits = self
            .facade
            .search_filenames(&repo_root, &query_text)
            .map_err(|error| AppError::internal(error.to_string()))?;

        let filename_rows = filename_hits
            .into_iter()
            .filter(|row| matches_path_prefix(&row.path, query.path_prefix.as_deref()))
            .map(|row| crate::pages::search::SearchResultRow {
                path: row.path,
                source: "filename".into(),
                file_object_id: row.file_object_id,
            })
            .collect::<Vec<_>>();

        if !filename_rows.is_empty() {
            return Ok(filename_rows);
        }

        let metadata_hits = self
            .facade
            .search_metadata(
                &repo_root,
                MetadataSearchQuery {
                    extension: Some(query_text.trim_start_matches('.').to_lowercase()),
                    path_prefix: query.path_prefix.clone(),
                    min_size: None,
                    max_size: None,
                },
            )
            .map_err(|error| AppError::internal(error.to_string()))?;

        Ok(metadata_hits
            .into_iter()
            .map(|row| crate::pages::search::SearchResultRow {
                path: row.path,
                source: "metadata".into(),
                file_object_id: row.file_object_id,
            })
            .collect())
    }
}

fn matches_path_prefix(path: &str, path_prefix: Option<&str>) -> bool {
    let Some(path_prefix) = path_prefix.map(str::trim).filter(|value| !value.is_empty()) else {
        return true;
    };
    let normalized_prefix = path_prefix.trim_matches('/').replace('\\', "/");
    path == normalized_prefix || path.starts_with(&(normalized_prefix + "/"))
}
