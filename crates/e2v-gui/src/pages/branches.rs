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
    SetCreateBranchName(String),
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
        BranchesMessage::SetCreateBranchName(value) => {
            app.workbench.branches.create_branch_name = value;
            iced::Task::none()
        }
        BranchesMessage::CreateBranch(name) => {
            if name.trim().is_empty() {
                return iced::Task::none();
            }
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

pub fn view_branches(app: &crate::app::PsycGuiApp) -> iced::Element<'_, crate::domain::Message> {
    use iced::widget::{button, column, container, row, text, text_input};

    let rows = if app.workbench.branches.rows.is_empty() {
        column![text("No branches loaded yet.")]
    } else {
        app.workbench
            .branches
            .rows
            .iter()
            .fold(column![].spacing(8), |column, branch| {
                let title = if branch.is_current {
                    format!("* {}", branch.name)
                } else {
                    branch.name.clone()
                };
                let snapshot = branch
                    .head_snapshot_id
                    .clone()
                    .unwrap_or_else(|| "none".into());
                column.push(
                    row![
                        text(format!("{title} ({snapshot})")).width(iced::Length::Fill),
                        button("Checkout")
                            .on_press(BranchesMessage::CheckoutBranch(branch.name.clone())),
                        button("Delete")
                            .on_press(BranchesMessage::DeleteBranch(branch.name.clone())),
                    ]
                    .spacing(8),
                )
            })
    };

    let page: iced::Element<'_, BranchesMessage> = container(
        column![
            text("Branches").size(28),
            text_input(
                "New branch name",
                &app.workbench.branches.create_branch_name
            )
            .on_input(BranchesMessage::SetCreateBranchName)
            .padding(10),
            button("Create branch").on_press(BranchesMessage::CreateBranch(
                app.workbench.branches.create_branch_name.trim().to_owned(),
            )),
            rows,
        ]
        .spacing(12),
    )
    .padding(20)
    .into();
    page.map(crate::domain::Message::from)
}
