#[derive(Debug, Clone)]
pub struct WorkbenchState {
    pub active_page: crate::domain::WorkbenchPage,
    pub overview: crate::pages::overview::OverviewState,
}

impl Default for WorkbenchState {
    fn default() -> Self {
        Self {
            active_page: crate::domain::WorkbenchPage::Overview,
            overview: crate::pages::overview::OverviewState::default(),
        }
    }
}
