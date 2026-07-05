#[derive(Debug)]
pub struct LocalWebController {
    pub local_url: String,
    pub(crate) _handle: Option<e2v_sync::ServeHandle>,
}

#[derive(Debug)]
pub struct MountController {
    pub summary: e2v_vfs::MountLaunchSummary,
    pub(crate) _filesystem: Option<e2v_vfs::MountedFilesystem>,
}

pub trait PreviewService: Send + Sync + std::fmt::Debug + 'static {
    fn start_local_web(
        &self,
        repo_root: std::path::PathBuf,
    ) -> Result<LocalWebController, crate::domain::AppError>;
    fn stop_local_web(&self, controller: LocalWebController)
    -> Result<(), crate::domain::AppError>;
    fn start_snapshot_mount(
        &self,
        repo_root: std::path::PathBuf,
        snapshot_id: String,
        mount_point: std::path::PathBuf,
    ) -> Result<MountController, crate::domain::AppError>;
    fn start_live_branch_mount(
        &self,
        repo_root: std::path::PathBuf,
        branch_token: String,
        mount_point: std::path::PathBuf,
    ) -> Result<MountController, crate::domain::AppError>;
    fn stop_mount(&self, controller: MountController) -> Result<(), crate::domain::AppError>;
}

#[derive(Debug, Default)]
pub struct RealPreviewService;

impl PreviewService for RealPreviewService {
    fn start_local_web(
        &self,
        repo_root: std::path::PathBuf,
    ) -> Result<LocalWebController, crate::domain::AppError> {
        let handle = tokio::runtime::Handle::current()
            .block_on(e2v_sync::serve_local_web(e2v_sync::ServeOptions {
                repo_root,
            }))
            .map_err(|error| crate::domain::AppError::internal(error.to_string()))?;
        Ok(LocalWebController {
            local_url: format!("http://{}", handle.local_addr()),
            _handle: Some(handle),
        })
    }

    fn stop_local_web(
        &self,
        controller: LocalWebController,
    ) -> Result<(), crate::domain::AppError> {
        drop(controller);
        Ok(())
    }

    fn start_snapshot_mount(
        &self,
        repo_root: std::path::PathBuf,
        snapshot_id: String,
        mount_point: std::path::PathBuf,
    ) -> Result<MountController, crate::domain::AppError> {
        let filesystem = e2v_vfs::start_snapshot_mount(repo_root, snapshot_id, mount_point)
            .map_err(|error| crate::domain::AppError::internal(error.to_string()))?;
        Ok(MountController {
            summary: filesystem.summary().clone(),
            _filesystem: Some(filesystem),
        })
    }

    fn start_live_branch_mount(
        &self,
        repo_root: std::path::PathBuf,
        branch_token: String,
        mount_point: std::path::PathBuf,
    ) -> Result<MountController, crate::domain::AppError> {
        let filesystem = e2v_vfs::start_live_branch_mount(repo_root, branch_token, mount_point)
            .map_err(|error| crate::domain::AppError::internal(error.to_string()))?;
        Ok(MountController {
            summary: filesystem.summary().clone(),
            _filesystem: Some(filesystem),
        })
    }

    fn stop_mount(&self, controller: MountController) -> Result<(), crate::domain::AppError> {
        drop(controller);
        Ok(())
    }
}
