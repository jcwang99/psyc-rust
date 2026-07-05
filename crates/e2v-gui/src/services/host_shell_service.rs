pub trait HostShellService: Send + Sync + std::fmt::Debug + 'static {
    fn open_url(&self, url: &str) -> Result<(), crate::domain::AppError>;
    fn open_path(&self, path: &std::path::Path) -> Result<(), crate::domain::AppError>;
}

#[derive(Debug, Default)]
pub struct RealHostShellService;

impl HostShellService for RealHostShellService {
    fn open_url(&self, url: &str) -> Result<(), crate::domain::AppError> {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn()
            .map(|_| ())
            .map_err(|error| crate::domain::AppError::internal(error.to_string()))
    }

    fn open_path(&self, path: &std::path::Path) -> Result<(), crate::domain::AppError> {
        std::process::Command::new("explorer")
            .arg(path)
            .spawn()
            .map(|_| ())
            .map_err(|error| crate::domain::AppError::internal(error.to_string()))
    }
}
