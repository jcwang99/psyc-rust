use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use iced::{Element, Task};

use crate::domain::{JobRecord, JobState, Message, PendingConfirmation, Screen};
use crate::pages::home::{HomeJobResult, HomeState};
use crate::pages::overview::{OverviewJobResult, OverviewState};
use crate::pages::sync::SyncJobResult;
use crate::pages::workbench::WorkbenchState;
use crate::services::AppServices;

#[derive(Debug)]
pub struct PsycGuiApp {
    pub screen: Screen,
    pub selected_repository: Option<PathBuf>,
    pub registry: crate::domain::RepositoryRegistry,
    pub services: AppServices,
    pub home: HomeState,
    pub workbench: WorkbenchState,
    pub jobs: Vec<JobRecord>,
    pub pending_confirmation: Option<PendingConfirmation>,
    next_job_id: u64,
}

pub fn boot() -> (PsycGuiApp, Task<Message>) {
    boot_with_services(AppServices::real())
}

pub fn boot_with_services(services: AppServices) -> (PsycGuiApp, Task<Message>) {
    (
        PsycGuiApp {
            screen: Screen::Home,
            selected_repository: None,
            registry: crate::domain::RepositoryRegistry::default(),
            services,
            home: HomeState::default(),
            workbench: WorkbenchState::default(),
            jobs: Vec::new(),
            pending_confirmation: None,
            next_job_id: 1,
        },
        Task::none(),
    )
}

pub fn update(app: &mut PsycGuiApp, message: Message) -> Task<Message> {
    match message {
        Message::Branches(message) => crate::pages::branches::update_branches(app, message),
        Message::Home(message) => crate::pages::home::update_home(app, message),
        Message::HomeJobFinished(result) => {
            handle_home_job_result(app, result);
            Task::none()
        }
        Message::History(message) => crate::pages::history::update_history(app, message),
        Message::Overview(message) => crate::pages::overview::update_overview(app, message),
        Message::OverviewJobFinished(result) => {
            handle_overview_job_result(app, result);
            Task::none()
        }
        Message::Search(message) => crate::pages::search::update_search(app, message),
        Message::Sync(message) => crate::pages::sync::update_sync(app, message),
        Message::SyncJobFinished(result) => {
            handle_sync_job_result(app, result);
            Task::none()
        }
        Message::Workbench(message) => crate::pages::workbench::update_workbench(app, message),
        Message::NoOp => Task::none(),
    }
}

pub fn view(app: &PsycGuiApp) -> Element<'_, Message> {
    match app.screen {
        Screen::Home => crate::widgets::home_screen::view_home(app),
        Screen::Workbench => crate::widgets::workbench_shell::view_workbench(app),
    }
}

fn handle_home_job_result(
    app: &mut PsycGuiApp,
    result: Result<HomeJobResult, crate::domain::AppError>,
) {
    match result {
        Ok(HomeJobResult::RepositoryLoaded(card)) => {
            app.home.validation_error = None;
            activate_repository(app, card);
        }
        Err(error) => {
            app.home.validation_error = Some(error.message);
        }
    }
}

fn handle_overview_job_result(
    app: &mut PsycGuiApp,
    result: Result<OverviewJobResult, crate::domain::AppError>,
) {
    match result {
        Ok(OverviewJobResult::Committed {
            repo_root,
            head_snapshot_id,
            last_message: _,
        }) => {
            app.workbench.overview.head_snapshot_id = Some(head_snapshot_id.clone());
            app.workbench.overview.commit_message.clear();

            if let Some(card) = app
                .home
                .cards
                .iter_mut()
                .find(|existing| existing.repo_root == repo_root)
            {
                card.head_snapshot_id = Some(head_snapshot_id.clone());
            }

            if let Some(job) = app.jobs.iter_mut().rev().find(|job| {
                job.repo_root.as_ref() == Some(&repo_root) && matches!(job.state, JobState::Running)
            }) {
                job.state = JobState::Succeeded;
            }
        }
        Err(error) => {
            if let Some(job) = app
                .jobs
                .iter_mut()
                .rev()
                .find(|job| matches!(job.state, JobState::Running))
            {
                job.state = JobState::Failed(error.message.clone());
            }
        }
    }
}

pub(crate) fn activate_repository(app: &mut PsycGuiApp, card: crate::domain::RepositoryHomeCard) {
    upsert_home_card(&mut app.home.cards, card.clone());
    app.registry
        .touch_recent(card.repo_root.clone(), current_unix_ms());
    app.selected_repository = Some(card.repo_root.clone());
    sync_workbench_from_card(&mut app.workbench.overview, &card);
    app.workbench.branch_token = card.branch_token.clone();
    if let Ok(rows) = app
        .services
        .repository
        .list_snapshots(card.repo_root.clone())
    {
        app.workbench.history.rows = rows;
    }
    if let Ok(rows) = app
        .services
        .repository
        .list_branches(card.repo_root.clone())
    {
        app.workbench.branches.rows = rows;
    }
    app.screen = Screen::Workbench;
}

fn upsert_home_card(
    cards: &mut Vec<crate::domain::RepositoryHomeCard>,
    card: crate::domain::RepositoryHomeCard,
) {
    cards.retain(|existing| existing.repo_root != card.repo_root);
    cards.insert(0, card);
}

fn sync_workbench_from_card(
    overview: &mut OverviewState,
    card: &crate::domain::RepositoryHomeCard,
) {
    overview.branch_name = card.branch_name.clone();
    overview.head_snapshot_id = card.head_snapshot_id.clone();
}

fn handle_sync_job_result(
    app: &mut PsycGuiApp,
    result: Result<SyncJobResult, crate::domain::AppError>,
) {
    match result {
        Ok(SyncJobResult::RemoteAdded { repo_root }) => {
            mark_latest_job_succeeded(app, &repo_root);
            if let Some(card) = app
                .home
                .cards
                .iter_mut()
                .find(|existing| existing.repo_root == repo_root)
            {
                card.remote_configured = true;
            }
            app.workbench.sync.validation_error = None;
        }
        Ok(SyncJobResult::Pushed { repo_root, .. })
        | Ok(SyncJobResult::Fetched { repo_root, .. }) => {
            mark_latest_job_succeeded(app, &repo_root);
            app.workbench.sync.validation_error = None;
        }
        Ok(SyncJobResult::Pulled {
            repo_root,
            snapshot_id,
        }) => {
            mark_latest_job_succeeded(app, &repo_root);
            app.workbench.sync.validation_error = None;
            app.workbench.overview.head_snapshot_id = Some(snapshot_id.clone());
            if let Some(card) = app
                .home
                .cards
                .iter_mut()
                .find(|existing| existing.repo_root == repo_root)
            {
                card.head_snapshot_id = Some(snapshot_id);
            }
        }
        Err(error) => {
            if let Some(job) = app
                .jobs
                .iter_mut()
                .rev()
                .find(|job| matches!(job.state, JobState::Running))
            {
                job.state = JobState::Failed(error.message.clone());
            }
            app.workbench.sync.validation_error = Some(error.message);
        }
    }
}

fn mark_latest_job_succeeded(app: &mut PsycGuiApp, repo_root: &std::path::Path) {
    if let Some(job) = app.jobs.iter_mut().rev().find(|job| {
        job.repo_root.as_deref() == Some(repo_root) && matches!(job.state, JobState::Running)
    }) {
        job.state = JobState::Succeeded;
    }
}

fn current_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

impl PsycGuiApp {
    pub fn allocate_job_id(&mut self) -> u64 {
        let id = self.next_job_id;
        self.next_job_id += 1;
        id
    }
}
