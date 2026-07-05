#[derive(Debug, Clone, Default)]
pub struct OverviewState {
    pub branch_name: String,
    pub head_snapshot_id: Option<String>,
    pub commit_message: String,
}

#[derive(Debug, Clone)]
pub enum OverviewMessage {
    SetCommitMessage(String),
    SubmitCommit,
}

#[derive(Debug, Clone)]
pub enum OverviewJobResult {
    Committed {
        repo_root: std::path::PathBuf,
        head_snapshot_id: String,
        last_message: String,
    },
}

pub fn update_overview(
    app: &mut crate::app::PsycGuiApp,
    message: OverviewMessage,
) -> iced::Task<crate::domain::Message> {
    match message {
        OverviewMessage::SetCommitMessage(value) => {
            app.workbench.overview.commit_message = value;
            iced::Task::none()
        }
        OverviewMessage::SubmitCommit => submit_commit(app),
    }
}

pub fn submit_commit(app: &mut crate::app::PsycGuiApp) -> iced::Task<crate::domain::Message> {
    let Some(repo_root) = app.selected_repository.clone() else {
        return iced::Task::none();
    };

    let commit_message = app.workbench.overview.commit_message.clone();
    let service = app.services.repository.clone();
    let job_id = app.allocate_job_id();
    let commit_repo_root = repo_root.clone();
    let commit_message_for_work = commit_message.clone();
    app.jobs.push(crate::domain::JobRecord {
        id: job_id,
        label: "Commit repository".into(),
        repo_root: Some(repo_root.clone()),
        state: crate::domain::JobState::Running,
    });

    crate::jobs::spawn_blocking_job(
        move || {
            service.commit_repository(commit_repo_root.clone(), commit_message_for_work.clone())
        },
        move |result| {
            crate::domain::Message::OverviewJobFinished(result.map(|commit| {
                OverviewJobResult::Committed {
                    repo_root,
                    head_snapshot_id: commit.snapshot_id,
                    last_message: commit_message,
                }
            }))
        },
    )
}

pub fn view_overview(app: &crate::app::PsycGuiApp) -> iced::Element<'_, crate::domain::Message> {
    use iced::widget::{button, column, container, row, text, text_input};

    let branch = if app.workbench.overview.branch_name.is_empty() {
        "unknown".to_owned()
    } else {
        app.workbench.overview.branch_name.clone()
    };
    let head = app
        .workbench
        .overview
        .head_snapshot_id
        .clone()
        .unwrap_or_else(|| "none".into());

    let page: iced::Element<'_, OverviewMessage> = container(
        column![
            text("Overview").size(28),
            row![
                crate::widgets::status_badge::view_status_badge(format!("Branch: {branch}")),
                crate::widgets::status_badge::view_status_badge(format!("Head: {head}")),
            ]
            .spacing(8),
            text_input("Commit message", &app.workbench.overview.commit_message)
                .on_input(OverviewMessage::SetCommitMessage)
                .padding(10),
            button("Commit repository").on_press(OverviewMessage::SubmitCommit),
        ]
        .spacing(16),
    )
    .padding(20)
    .into();
    page.map(crate::domain::Message::from)
}
