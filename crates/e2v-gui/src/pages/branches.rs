#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchRow {
    pub name: String,
    pub head_snapshot_id: Option<String>,
    pub is_current: bool,
}

#[derive(Debug, Clone, Default)]
pub struct BranchesState {
    pub rows: Vec<BranchRow>,
    pub create_branch_name: String,
}

#[derive(Debug, Clone)]
pub enum BranchesMessage {
    CreateBranch(String),
    CheckoutBranch(String),
    DeleteBranch(String),
}

pub fn update_branches(
    app: &mut crate::app::PsycGuiApp,
    message: BranchesMessage,
) -> iced::Task<crate::domain::Message> {
    let Some(repo_root) = app.selected_repository.clone() else {
        return iced::Task::none();
    };

    match message {
        BranchesMessage::CreateBranch(name) => {
            if app
                .services
                .repository
                .create_branch(repo_root.clone(), name)
                .is_ok()
            {
                refresh_branch_rows(app, repo_root);
            }
            iced::Task::none()
        }
        BranchesMessage::CheckoutBranch(name) => {
            if let Ok(card) = app
                .services
                .repository
                .checkout_branch(repo_root.clone(), name)
            {
                crate::app::activate_repository(app, card);
                refresh_branch_rows(app, repo_root);
            }
            iced::Task::none()
        }
        BranchesMessage::DeleteBranch(name) => {
            if app
                .services
                .repository
                .delete_branch(repo_root.clone(), name)
                .is_ok()
            {
                refresh_branch_rows(app, repo_root);
            }
            iced::Task::none()
        }
    }
}

fn refresh_branch_rows(app: &mut crate::app::PsycGuiApp, repo_root: std::path::PathBuf) {
    if let Ok(rows) = app.services.repository.list_branches(repo_root) {
        app.workbench.branches.rows = rows;
    }
}
