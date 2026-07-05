#[derive(Debug, Clone)]
pub enum WorkbenchMessage {
    SelectPage(crate::domain::WorkbenchPage),
}

pub fn update_workbench(
    app: &mut crate::app::PsycGuiApp,
    message: WorkbenchMessage,
) -> iced::Task<crate::domain::Message> {
    match message {
        WorkbenchMessage::SelectPage(page) => {
            if matches!(page, crate::domain::WorkbenchPage::Sharing) {
                if let Some(repo_root) = app.selected_repository.clone() {
                    match app.services.sharing.load_roster(repo_root) {
                        Ok(roster) => {
                            app.workbench.sharing.roster = roster;
                            app.workbench.sharing.validation_error = None;
                        }
                        Err(error) => {
                            app.workbench.sharing.validation_error = Some(error.message);
                        }
                    }
                }
            }
            app.workbench.active_page = page;
            iced::Task::none()
        }
    }
}

pub fn view_active_page(app: &crate::app::PsycGuiApp) -> iced::Element<'_, crate::domain::Message> {
    use iced::widget::{container, text};

    match app.workbench.active_page {
        crate::domain::WorkbenchPage::Overview => crate::pages::overview::view_overview(app),
        crate::domain::WorkbenchPage::History => crate::pages::history::view_history(app),
        crate::domain::WorkbenchPage::Branches => crate::pages::branches::view_branches(app),
        crate::domain::WorkbenchPage::Sync => crate::pages::sync::view_sync(app),
        crate::domain::WorkbenchPage::Search => crate::pages::search::view_search(app),
        crate::domain::WorkbenchPage::Sharing => crate::pages::sharing::view_sharing(app),
        crate::domain::WorkbenchPage::Preview => crate::pages::preview::view_preview(app),
        crate::domain::WorkbenchPage::Advanced => container(text("Advanced (Phase 3)")).into(),
    }
}

#[derive(Debug)]
pub struct WorkbenchState {
    pub active_page: crate::domain::WorkbenchPage,
    pub branches: crate::pages::branches::BranchesState,
    pub branch_token: String,
    pub history: crate::pages::history::HistoryState,
    pub overview: crate::pages::overview::OverviewState,
    pub preview: crate::pages::preview::PreviewState,
    pub search: crate::pages::search::SearchState,
    pub sharing: crate::pages::sharing::SharingState,
    pub sync: crate::pages::sync::SyncState,
}

impl Default for WorkbenchState {
    fn default() -> Self {
        Self {
            active_page: crate::domain::WorkbenchPage::Overview,
            branches: crate::pages::branches::BranchesState::default(),
            branch_token: String::new(),
            history: crate::pages::history::HistoryState::default(),
            overview: crate::pages::overview::OverviewState::default(),
            preview: crate::pages::preview::PreviewState::default(),
            search: crate::pages::search::SearchState::default(),
            sharing: crate::pages::sharing::SharingState::default(),
            sync: crate::pages::sync::SyncState::default(),
        }
    }
}
