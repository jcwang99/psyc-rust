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
