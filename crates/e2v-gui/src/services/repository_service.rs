use std::path::{Path, PathBuf};

use e2v_api::{
    CloneRequest, CommitInfo, CommitRepositoryOptions, InitRepositoryOptions, Sdk, SdkErrorCode,
};

use crate::domain::{AppError, RepositoryHomeCard};

pub trait RepositoryService: Send + Sync + std::fmt::Debug + 'static {
    fn init_repository(
        &self,
        repo_root: PathBuf,
        password: String,
        branch_name: String,
    ) -> Result<RepositoryHomeCard, AppError>;

    fn open_repository(&self, repo_root: PathBuf) -> Result<RepositoryHomeCard, AppError>;

    fn clone_repository(
        &self,
        remote_spec: String,
        target_repo_root: PathBuf,
        password: String,
        branch_token: String,
    ) -> Result<RepositoryHomeCard, AppError>;

    fn commit_repository(
        &self,
        repo_root: PathBuf,
        message: String,
    ) -> Result<CommitInfo, AppError>;

    fn load_repository_summary(&self, repo_root: PathBuf) -> Result<RepositoryHomeCard, AppError>;
}

#[derive(Debug, Default)]
pub struct RealRepositoryService {
    sdk: Sdk,
}

impl RealRepositoryService {
    fn display_name(repo_root: &Path) -> String {
        repo_root
            .file_name()
            .filter(|name| !name.is_empty())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| repo_root.display().to_string())
    }
}

impl RepositoryService for RealRepositoryService {
    fn init_repository(
        &self,
        repo_root: PathBuf,
        password: String,
        branch_name: String,
    ) -> Result<RepositoryHomeCard, AppError> {
        self.sdk
            .init_repository(InitRepositoryOptions {
                repo_root: repo_root.clone(),
                password,
                branch_name,
            })
            .map_err(AppError::from_sdk)?;

        self.load_repository_summary(repo_root)
    }

    fn open_repository(&self, repo_root: PathBuf) -> Result<RepositoryHomeCard, AppError> {
        self.load_repository_summary(repo_root)
    }

    fn clone_repository(
        &self,
        remote_spec: String,
        target_repo_root: PathBuf,
        password: String,
        branch_token: String,
    ) -> Result<RepositoryHomeCard, AppError> {
        self.sdk
            .clone_remote(CloneRequest {
                remote_spec,
                target_repo_root: target_repo_root.clone(),
                password,
                branch_token,
            })
            .map_err(AppError::from_sdk)?;

        self.load_repository_summary(target_repo_root)
    }

    fn commit_repository(
        &self,
        repo_root: PathBuf,
        message: String,
    ) -> Result<CommitInfo, AppError> {
        self.sdk
            .commit_repository(CommitRepositoryOptions { repo_root, message })
            .map_err(AppError::from_sdk)
    }

    fn load_repository_summary(&self, repo_root: PathBuf) -> Result<RepositoryHomeCard, AppError> {
        let repository = self
            .sdk
            .open_repository(&repo_root)
            .map_err(AppError::from_sdk)?;
        let snapshots = self
            .sdk
            .list_snapshots(&repo_root)
            .map_err(AppError::from_sdk)?;
        let remote_configured = match self.sdk.load_default_remote(&repo_root) {
            Ok(_) => true,
            Err(error) if error.code() == SdkErrorCode::NotFound => false,
            Err(error) => return Err(AppError::from_sdk(error)),
        };

        Ok(RepositoryHomeCard {
            repo_root: repo_root.clone(),
            display_name: Self::display_name(&repo_root),
            branch_name: repository.branch.name,
            head_snapshot_id: snapshots
                .first()
                .map(|snapshot| snapshot.snapshot_id.clone()),
            remote_configured,
        })
    }
}
