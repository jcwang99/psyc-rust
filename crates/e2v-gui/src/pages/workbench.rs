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
        crate::domain::WorkbenchPage::Search => container(text("Search (Phase 2)")).into(),
        crate::domain::WorkbenchPage::Sharing => container(text("Sharing (Phase 2)")).into(),
        crate::domain::WorkbenchPage::Preview => container(text("Preview (Phase 2)")).into(),
        crate::domain::WorkbenchPage::Advanced => container(text("Advanced (Phase 3)")).into(),
    }
}

#[derive(Debug, Clone)]
pub struct WorkbenchState {
    pub active_page: crate::domain::WorkbenchPage,
    pub branches: crate::pages::branches::BranchesState,
    pub branch_token: String,
    pub history: crate::pages::history::HistoryState,
    pub overview: crate::pages::overview::OverviewState,
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
            sync: crate::pages::sync::SyncState::default(),
        }
    }
}
