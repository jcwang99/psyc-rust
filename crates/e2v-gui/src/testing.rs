use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use e2v_api::CommitInfo;

use crate::domain::{AppError, RepositoryHomeCard};
use crate::services::RepositoryService;

#[derive(Debug, Default, Clone)]
pub struct FakeRepositoryService {
    state: Arc<Mutex<FakeRepositoryServiceState>>,
}

#[derive(Debug, Default)]
struct FakeRepositoryServiceState {
    summaries: HashMap<PathBuf, RepositoryHomeCard>,
    commit_results: HashMap<PathBuf, CommitInfo>,
}

impl FakeRepositoryService {
    pub fn with_open_result(
        repo_root: impl Into<PathBuf>,
        branch_name: impl Into<String>,
        head_snapshot_id: Option<&str>,
    ) -> Self {
        let repo_root = repo_root.into();
        let mut summaries = HashMap::new();
        summaries.insert(
            repo_root.clone(),
            RepositoryHomeCard {
                display_name: display_name(&repo_root),
                repo_root,
                branch_name: branch_name.into(),
                head_snapshot_id: head_snapshot_id.map(str::to_owned),
                remote_configured: false,
            },
        );

        Self {
            state: Arc::new(Mutex::new(FakeRepositoryServiceState {
                summaries,
                commit_results: HashMap::new(),
            })),
        }
    }

    pub fn with_commit_result(
        repo_root: impl Into<PathBuf>,
        branch_name: impl Into<String>,
        head_snapshot_id: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        let repo_root = repo_root.into();
        let head_snapshot_id = head_snapshot_id.into();
        let mut summaries = HashMap::new();
        summaries.insert(
            repo_root.clone(),
            RepositoryHomeCard {
                display_name: display_name(&repo_root),
                repo_root: repo_root.clone(),
                branch_name: branch_name.into(),
                head_snapshot_id: Some(head_snapshot_id.clone()),
                remote_configured: false,
            },
        );
        let mut commit_results = HashMap::new();
        commit_results.insert(
            repo_root,
            CommitInfo {
                snapshot_id: head_snapshot_id,
                committed_files: 1,
                new_bytes: 0,
                reused_bytes: 0,
                warnings: vec![message.into()],
            },
        );

        Self {
            state: Arc::new(Mutex::new(FakeRepositoryServiceState {
                summaries,
                commit_results,
            })),
        }
    }
}

#[derive(Debug)]
pub struct AppHarness {
    pub app: crate::app::PsycGuiApp,
    pub service: FakeRepositoryService,
}

pub fn boot_with_service(service: FakeRepositoryService) -> AppHarness {
    let (app, _) =
        crate::boot_with_services(crate::services::AppServices::new(Arc::new(service.clone())));

    AppHarness { app, service }
}

pub fn boot_into_workbench(service: FakeRepositoryService, repo_root: &str) -> AppHarness {
    let mut harness = boot_with_service(service.clone());
    let card = service
        .load_repository_summary(PathBuf::from(repo_root))
        .expect("fake repository summary");
    crate::app::activate_repository(&mut harness.app, card);
    harness
}

pub fn advance(
    app: &mut crate::app::PsycGuiApp,
    message: crate::domain::Message,
) -> iced::Task<crate::domain::Message> {
    crate::app::update(app, message)
}

impl crate::services::RepositoryService for FakeRepositoryService {
    fn init_repository(
        &self,
        repo_root: PathBuf,
        _password: String,
        branch_name: String,
    ) -> Result<RepositoryHomeCard, AppError> {
        let card = RepositoryHomeCard {
            display_name: display_name(&repo_root),
            repo_root: repo_root.clone(),
            branch_name,
            head_snapshot_id: None,
            remote_configured: false,
        };

        self.state
            .lock()
            .map_err(|_| AppError::internal("fake repository service state poisoned"))?
            .summaries
            .insert(repo_root, card.clone());

        Ok(card)
    }

    fn open_repository(&self, repo_root: PathBuf) -> Result<RepositoryHomeCard, AppError> {
        self.load_repository_summary(repo_root)
    }

    fn clone_repository(
        &self,
        _remote_spec: String,
        target_repo_root: PathBuf,
        _password: String,
        _branch_token: String,
    ) -> Result<RepositoryHomeCard, AppError> {
        self.load_repository_summary(target_repo_root)
    }

    fn commit_repository(
        &self,
        repo_root: PathBuf,
        _message: String,
    ) -> Result<CommitInfo, AppError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| AppError::internal("fake repository service state poisoned"))?;
        let result = state
            .commit_results
            .get(&repo_root)
            .cloned()
            .unwrap_or(CommitInfo {
                snapshot_id: format!("generated-{}", state.commit_results.len() + 1),
                committed_files: 1,
                new_bytes: 0,
                reused_bytes: 0,
                warnings: Vec::new(),
            });

        if let Some(summary) = state.summaries.get_mut(&repo_root) {
            summary.head_snapshot_id = Some(result.snapshot_id.clone());
        } else {
            state.summaries.insert(
                repo_root.clone(),
                RepositoryHomeCard {
                    display_name: display_name(&repo_root),
                    repo_root,
                    branch_name: "main".into(),
                    head_snapshot_id: Some(result.snapshot_id.clone()),
                    remote_configured: false,
                },
            );
        }

        Ok(result)
    }

    fn load_repository_summary(&self, repo_root: PathBuf) -> Result<RepositoryHomeCard, AppError> {
        self.state
            .lock()
            .map_err(|_| AppError::internal("fake repository service state poisoned"))?
            .summaries
            .get(&repo_root)
            .cloned()
            .ok_or_else(|| AppError::internal("missing fake repository summary"))
    }
}

fn display_name(repo_root: &Path) -> String {
    repo_root
        .file_name()
        .filter(|name| !name.is_empty())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| repo_root.display().to_string())
}
