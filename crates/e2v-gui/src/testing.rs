use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use e2v_api::{CommitInfo, FetchResponse, PullResponse, PushResponse};
use e2v_vfs::{CachePolicy, MountLaunchState, MountLaunchSummary};

use crate::domain::{AppError, RepositoryHomeCard};
use crate::services::{
    HostShellService, LocalWebController, MountController, PreviewService, RepositoryService,
    SearchQuery, SearchService, ShareActorRow, ShareDeviceRow, SharingRoster, SharingService,
};

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
                branch_token: "branch-token".into(),
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
                branch_token: "branch-token".into(),
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
    pub host_shell: FakeHostShellService,
    pub preview: FakePreviewService,
    pub service: FakeRepositoryService,
    pub search: FakeSearchService,
    pub sharing: FakeSharingService,
}

#[derive(Debug, Clone)]
pub struct TestServices {
    pub host_shell: FakeHostShellService,
    pub preview: FakePreviewService,
    pub repository: FakeRepositoryService,
    pub search: FakeSearchService,
    pub sharing: FakeSharingService,
}

impl TestServices {
    pub fn new(repository: FakeRepositoryService) -> Self {
        Self {
            host_shell: FakeHostShellService::default(),
            preview: FakePreviewService::default(),
            repository,
            search: FakeSearchService::default(),
            sharing: FakeSharingService::default(),
        }
    }

    pub fn with_search(mut self, search: FakeSearchService) -> Self {
        self.search = search;
        self
    }

    pub fn with_sharing(mut self, sharing: FakeSharingService) -> Self {
        self.sharing = sharing;
        self
    }

    pub fn with_preview(mut self, preview: FakePreviewService) -> Self {
        self.preview = preview;
        self
    }

    pub fn with_host_shell(mut self, host_shell: FakeHostShellService) -> Self {
        self.host_shell = host_shell;
        self
    }
}

pub fn boot_with_service(service: FakeRepositoryService) -> AppHarness {
    boot_with_test_services(TestServices::new(service))
}

pub fn boot_with_test_services(services: TestServices) -> AppHarness {
    let (app, _) = crate::boot_with_services(
        crate::services::AppServices::new(Arc::new(services.repository.clone()))
            .with_host_shell(Arc::new(services.host_shell.clone()))
            .with_preview(Arc::new(services.preview.clone()))
            .with_search(Arc::new(services.search.clone()))
            .with_sharing(Arc::new(services.sharing.clone())),
    );

    AppHarness {
        app,
        host_shell: services.host_shell,
        preview: services.preview,
        service: services.repository,
        search: services.search,
        sharing: services.sharing,
    }
}

pub fn boot_into_workbench(service: FakeRepositoryService, repo_root: &str) -> AppHarness {
    let mut harness = boot_with_service(service.clone());
    let card = service
        .load_repository_summary(PathBuf::from(repo_root))
        .expect("fake repository summary");
    crate::app::activate_repository(&mut harness.app, card);
    harness
}

pub fn boot_into_workbench_with_services(services: TestServices, repo_root: &str) -> AppHarness {
    let mut harness = boot_with_test_services(services);
    let card = harness
        .service
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
            branch_token: "branch-token".into(),
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
                    branch_token: "branch-token".into(),
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
        summary.branch_token = "branch-token".into();
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

    fn add_remote(&self, repo_root: PathBuf, _name: String, _spec: String) -> Result<(), AppError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| AppError::internal("fake repository service state poisoned"))?;
        if let Some(summary) = state.summaries.get_mut(&repo_root) {
            summary.remote_configured = true;
        }
        Ok(())
    }

    fn push_default_remote(
        &self,
        _repo_root: PathBuf,
        _branch_token: String,
    ) -> Result<PushResponse, AppError> {
        Ok(PushResponse {
            published_snapshot_id: "push-snapshot".into(),
            uploaded_objects: 1,
        })
    }

    fn push_default_remote_allowing_single_writer_risk(
        &self,
        repo_root: PathBuf,
        branch_token: String,
    ) -> Result<PushResponse, AppError> {
        self.push_default_remote(repo_root, branch_token)
    }

    fn fetch_default_remote(
        &self,
        _repo_root: PathBuf,
        _branch_token: String,
    ) -> Result<FetchResponse, AppError> {
        Ok(FetchResponse {
            downloaded_objects: 1,
        })
    }

    fn pull_default_remote(
        &self,
        _repo_root: PathBuf,
        _branch_token: String,
    ) -> Result<PullResponse, AppError> {
        Ok(PullResponse {
            snapshot_id: "pulled-snapshot".into(),
            fast_forward_applied: true,
        })
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

#[derive(Debug, Default, Clone)]
pub struct FakeSearchService {
    state: Arc<Mutex<FakeSearchServiceState>>,
}

#[derive(Debug, Default)]
struct FakeSearchServiceState {
    rows: Vec<crate::pages::search::SearchResultRow>,
    call_count: usize,
    last_query: Option<SearchQuery>,
}

impl FakeSearchService {
    pub fn with_rows(rows: Vec<crate::pages::search::SearchResultRow>) -> Self {
        Self {
            state: Arc::new(Mutex::new(FakeSearchServiceState {
                rows,
                call_count: 0,
                last_query: None,
            })),
        }
    }

    pub fn call_count(&self) -> usize {
        self.state
            .lock()
            .expect("fake search service state")
            .call_count
    }

    pub fn last_query(&self) -> Option<SearchQuery> {
        self.state
            .lock()
            .expect("fake search service state")
            .last_query
            .clone()
    }
}

impl SearchService for FakeSearchService {
    fn search(
        &self,
        _repo_root: PathBuf,
        _branch_token: String,
        _head_snapshot_id: Option<String>,
        query: SearchQuery,
    ) -> Result<Vec<crate::pages::search::SearchResultRow>, AppError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| AppError::internal("fake search service state poisoned"))?;
        state.call_count += 1;
        state.last_query = Some(query);
        Ok(state.rows.clone())
    }
}

#[derive(Debug, Default, Clone)]
pub struct FakeSharingService {
    state: Arc<Mutex<FakeSharingServiceState>>,
}

#[derive(Debug, Default)]
struct FakeSharingServiceState {
    roster: SharingRoster,
    member_invite_bundle: Vec<u8>,
    device_invite_bundle: Vec<u8>,
}

impl FakeSharingService {
    pub fn with_member_invite_bundle(bundle: Vec<u8>) -> Self {
        Self {
            state: Arc::new(Mutex::new(FakeSharingServiceState {
                roster: SharingRoster::default(),
                member_invite_bundle: bundle,
                device_invite_bundle: Vec::new(),
            })),
        }
    }

    pub fn with_roster(
        actors: Vec<(&str, &str, &str)>,
        devices: Vec<(&str, &str, &str, &str)>,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(FakeSharingServiceState {
                roster: SharingRoster {
                    actors: actors
                        .into_iter()
                        .map(|(actor_id, display_name, role)| ShareActorRow {
                            actor_id: actor_id.into(),
                            display_name: display_name.into(),
                            role: role.into(),
                        })
                        .collect(),
                    devices: devices
                        .into_iter()
                        .map(|(device_id, actor_id, label, status)| ShareDeviceRow {
                            device_id: device_id.into(),
                            actor_id: actor_id.into(),
                            label: label.into(),
                            status: status.into(),
                        })
                        .collect(),
                },
                member_invite_bundle: Vec::new(),
                device_invite_bundle: Vec::new(),
            })),
        }
    }

    pub fn with_revocable_device(device_id: &str) -> Self {
        Self::with_roster(
            vec![("actor-1", "Alice", "owner")],
            vec![(device_id, "actor-1", "Laptop", "active")],
        )
    }
}

impl SharingService for FakeSharingService {
    fn load_roster(&self, _repo_root: PathBuf) -> Result<SharingRoster, AppError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| AppError::internal("fake sharing service state poisoned"))?
            .roster
            .clone())
    }

    fn invite_member(
        &self,
        _repo_root: PathBuf,
        _display_name: String,
    ) -> Result<String, AppError> {
        use base64::Engine as _;

        let state = self
            .state
            .lock()
            .map_err(|_| AppError::internal("fake sharing service state poisoned"))?;
        Ok(base64::engine::general_purpose::STANDARD.encode(&state.member_invite_bundle))
    }

    fn accept_member(
        &self,
        _repo_root: PathBuf,
        _invite_bundle_base64: String,
        _local_device_label: String,
    ) -> Result<SharingRoster, AppError> {
        self.load_roster(PathBuf::new())
    }

    fn invite_device(
        &self,
        _repo_root: PathBuf,
        _actor_id: String,
        _device_label: String,
    ) -> Result<String, AppError> {
        use base64::Engine as _;

        let state = self
            .state
            .lock()
            .map_err(|_| AppError::internal("fake sharing service state poisoned"))?;
        Ok(base64::engine::general_purpose::STANDARD.encode(&state.device_invite_bundle))
    }

    fn accept_device(
        &self,
        _repo_root: PathBuf,
        _invite_bundle_base64: String,
        _local_device_label: String,
    ) -> Result<SharingRoster, AppError> {
        self.load_roster(PathBuf::new())
    }

    fn revoke_member(
        &self,
        _repo_root: PathBuf,
        actor_id: String,
        _password: String,
    ) -> Result<SharingRoster, AppError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| AppError::internal("fake sharing service state poisoned"))?;
        state
            .roster
            .actors
            .retain(|actor| actor.actor_id != actor_id);
        state
            .roster
            .devices
            .retain(|device| device.actor_id != actor_id);
        Ok(state.roster.clone())
    }

    fn revoke_device(
        &self,
        _repo_root: PathBuf,
        device_id: String,
        _password: String,
    ) -> Result<SharingRoster, AppError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| AppError::internal("fake sharing service state poisoned"))?;
        state
            .roster
            .devices
            .retain(|device| device.device_id != device_id);
        Ok(state.roster.clone())
    }
}

#[derive(Debug, Default, Clone)]
pub struct FakePreviewService {
    state: Arc<Mutex<FakePreviewServiceState>>,
}

#[derive(Debug, Default)]
struct FakePreviewServiceState {
    local_web_url: Option<String>,
    snapshot_mount_summary: Option<MountLaunchSummary>,
    live_branch_mount_summary: Option<MountLaunchSummary>,
}

impl FakePreviewService {
    pub fn with_local_web_url(url: &str) -> Self {
        Self {
            state: Arc::new(Mutex::new(FakePreviewServiceState {
                local_web_url: Some(url.into()),
                snapshot_mount_summary: None,
                live_branch_mount_summary: None,
            })),
        }
    }

    pub fn with_snapshot_mount_summary(
        mount_point: &str,
        read_only: bool,
        stream_only: bool,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(FakePreviewServiceState {
                local_web_url: None,
                snapshot_mount_summary: Some(fake_mount_summary(
                    "snapshot-pinned",
                    mount_point,
                    read_only,
                    stream_only,
                )),
                live_branch_mount_summary: None,
            })),
        }
    }
}

impl PreviewService for FakePreviewService {
    fn start_local_web(&self, _repo_root: PathBuf) -> Result<LocalWebController, AppError> {
        let url = self
            .state
            .lock()
            .map_err(|_| AppError::internal("fake preview service state poisoned"))?
            .local_web_url
            .clone()
            .unwrap_or_else(|| "http://127.0.0.1:0".into());
        Ok(fake_local_web_controller(&url))
    }

    fn stop_local_web(&self, controller: LocalWebController) -> Result<(), AppError> {
        drop(controller);
        Ok(())
    }

    fn start_snapshot_mount(
        &self,
        _repo_root: PathBuf,
        _snapshot_id: String,
        _mount_point: PathBuf,
    ) -> Result<MountController, AppError> {
        let summary = self
            .state
            .lock()
            .map_err(|_| AppError::internal("fake preview service state poisoned"))?
            .snapshot_mount_summary
            .clone()
            .unwrap_or_else(|| fake_snapshot_mount_summary("Q:/preview"));
        Ok(MountController {
            summary,
            _filesystem: None,
        })
    }

    fn start_live_branch_mount(
        &self,
        _repo_root: PathBuf,
        _branch_token: String,
        _mount_point: PathBuf,
    ) -> Result<MountController, AppError> {
        let summary = self
            .state
            .lock()
            .map_err(|_| AppError::internal("fake preview service state poisoned"))?
            .live_branch_mount_summary
            .clone()
            .unwrap_or_else(|| fake_live_branch_mount_summary("R:/preview"));
        Ok(MountController {
            summary,
            _filesystem: None,
        })
    }

    fn stop_mount(&self, controller: MountController) -> Result<(), AppError> {
        drop(controller);
        Ok(())
    }
}

#[derive(Debug, Default, Clone)]
pub struct FakeHostShellService {
    state: Arc<Mutex<FakeHostShellState>>,
}

#[derive(Debug, Default)]
struct FakeHostShellState {
    last_opened_path: Option<PathBuf>,
    last_opened_url: Option<String>,
}

impl FakeHostShellService {
    pub fn last_opened_path(&self) -> Option<PathBuf> {
        self.state
            .lock()
            .expect("fake host shell state")
            .last_opened_path
            .clone()
    }

    pub fn last_opened_url(&self) -> Option<String> {
        self.state
            .lock()
            .expect("fake host shell state")
            .last_opened_url
            .clone()
    }
}

impl HostShellService for FakeHostShellService {
    fn open_url(&self, url: &str) -> Result<(), AppError> {
        self.state
            .lock()
            .map_err(|_| AppError::internal("fake host shell state poisoned"))?
            .last_opened_url = Some(url.into());
        Ok(())
    }

    fn open_path(&self, path: &std::path::Path) -> Result<(), AppError> {
        self.state
            .lock()
            .map_err(|_| AppError::internal("fake host shell state poisoned"))?
            .last_opened_path = Some(path.to_path_buf());
        Ok(())
    }
}

pub fn fake_local_web_controller(url: &str) -> LocalWebController {
    LocalWebController {
        local_url: url.into(),
        _handle: None,
    }
}

pub fn fake_snapshot_mount_summary(mount_point: &str) -> MountLaunchSummary {
    fake_mount_summary("snapshot-pinned", mount_point, true, true)
}

pub fn fake_live_branch_mount_summary(mount_point: &str) -> MountLaunchSummary {
    fake_mount_summary("live-branch", mount_point, false, true)
}

fn fake_mount_summary(
    mount_mode: &str,
    mount_point: &str,
    read_only: bool,
    stream_only: bool,
) -> MountLaunchSummary {
    MountLaunchSummary {
        mount_mode: mount_mode.into(),
        mount_point: PathBuf::from(mount_point),
        cache_policy: CachePolicy::KernelCacheWithInvalidation,
        read_only,
        stream_only,
        launch_state: MountLaunchState::SummaryOnly,
        status_message: "ready".into(),
    }
}

fn display_name(repo_root: &Path) -> String {
    repo_root
        .file_name()
        .filter(|name| !name.is_empty())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| repo_root.display().to_string())
}
