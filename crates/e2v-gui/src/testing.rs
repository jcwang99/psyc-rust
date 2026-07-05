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
    branch_rows: HashMap<PathBuf, Vec<crate::pages::branches::BranchRow>>,
    summaries: HashMap<PathBuf, RepositoryHomeCard>,
    commit_results: HashMap<PathBuf, CommitInfo>,
    snapshot_rows: HashMap<PathBuf, Vec<crate::pages::history::SnapshotRow>>,
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
                branch_rows: HashMap::new(),
                summaries,
                commit_results: HashMap::new(),
                snapshot_rows: HashMap::new(),
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
                branch_rows: HashMap::new(),
                summaries,
                commit_results,
                snapshot_rows: HashMap::new(),
            })),
        }
    }

    pub fn with_snapshot_list(repo_root: impl Into<PathBuf>, snapshot_ids: Vec<&str>) -> Self {
        let repo_root = repo_root.into();
        let service =
            Self::with_open_result(repo_root.clone(), "main", snapshot_ids.first().copied());
        let rows = snapshot_ids
            .into_iter()
            .map(|snapshot_id| crate::pages::history::SnapshotRow {
                snapshot_id: snapshot_id.to_owned(),
                message: format!("message-{snapshot_id}"),
            })
            .collect::<Vec<_>>();
        service
            .state
            .lock()
            .expect("fake repository service state")
            .snapshot_rows
            .insert(repo_root, rows);
        service
    }

    pub fn with_branch_table(repo_root: impl Into<PathBuf>, branch_names: Vec<&str>) -> Self {
        let repo_root = repo_root.into();
        let current_branch = branch_names.first().copied().unwrap_or("main");
        let service = Self::with_open_result(repo_root.clone(), current_branch, Some("snap-1"));
        let rows = branch_names
            .into_iter()
            .enumerate()
            .map(|(index, name)| crate::pages::branches::BranchRow {
                name: name.to_owned(),
                head_snapshot_id: Some(format!("snap-{}", index + 1)),
                is_current: index == 0,
            })
            .collect::<Vec<_>>();
        service
            .state
            .lock()
            .expect("fake repository service state")
            .branch_rows
            .insert(repo_root, rows);
        service
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

    fn list_snapshots(
        &self,
        repo_root: PathBuf,
    ) -> Result<Vec<crate::pages::history::SnapshotRow>, AppError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| AppError::internal("fake repository service state poisoned"))?
            .snapshot_rows
            .get(&repo_root)
            .cloned()
            .unwrap_or_default())
    }

    fn checkout_snapshot(
        &self,
        _repo_root: PathBuf,
        _snapshot_id: String,
        _target_dir: PathBuf,
    ) -> Result<(), AppError> {
        Ok(())
    }

    fn list_branches(
        &self,
        repo_root: PathBuf,
    ) -> Result<Vec<crate::pages::branches::BranchRow>, AppError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| AppError::internal("fake repository service state poisoned"))?
            .branch_rows
            .get(&repo_root)
            .cloned()
            .unwrap_or_default())
    }

    fn create_branch(
        &self,
        repo_root: PathBuf,
        name: String,
    ) -> Result<crate::pages::branches::BranchRow, AppError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| AppError::internal("fake repository service state poisoned"))?;
        let rows = state.branch_rows.entry(repo_root).or_default();
        let row = crate::pages::branches::BranchRow {
            name,
            head_snapshot_id: rows
                .iter()
                .find(|branch| branch.is_current)
                .and_then(|branch| branch.head_snapshot_id.clone()),
            is_current: false,
        };
        rows.insert(0, row.clone());
        Ok(row)
    }

    fn checkout_branch(
        &self,
        repo_root: PathBuf,
        name: String,
    ) -> Result<RepositoryHomeCard, AppError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| AppError::internal("fake repository service state poisoned"))?;
        let rows = state.branch_rows.entry(repo_root.clone()).or_default();
        let mut head_snapshot_id = None;
        for row in rows.iter_mut() {
            let is_current = row.name == name;
            row.is_current = is_current;
            if is_current {
                head_snapshot_id = row.head_snapshot_id.clone();
            }
        }

        let summary = state
            .summaries
            .get_mut(&repo_root)
            .ok_or_else(|| AppError::internal("missing fake repository summary"))?;
        summary.branch_name = name;
        summary.head_snapshot_id = head_snapshot_id;
        Ok(summary.clone())
    }

    fn delete_branch(&self, repo_root: PathBuf, name: String) -> Result<(), AppError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| AppError::internal("fake repository service state poisoned"))?;
        if let Some(rows) = state.branch_rows.get_mut(&repo_root) {
            rows.retain(|row| row.name != name);
        }
        Ok(())
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
