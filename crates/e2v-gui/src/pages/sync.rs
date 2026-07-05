#[derive(Debug, Clone, Default)]
pub struct SyncState {
    pub remote_name: String,
    pub remote_spec: String,
    pub validation_error: Option<String>,
}

#[derive(Debug, Clone)]
pub enum SyncMessage {
    SetRemoteName(String),
    SetRemoteSpec(String),
    SubmitAddRemote,
    SubmitPush,
    SubmitPushWithSingleWriterRisk,
    SubmitFetch,
    SubmitPull,
    ConfirmPendingAction,
    CancelPendingAction,
}

#[derive(Debug, Clone)]
pub enum SyncJobResult {
    RemoteAdded {
        repo_root: std::path::PathBuf,
    },
    Pushed {
        repo_root: std::path::PathBuf,
        published_snapshot_id: String,
    },
    Fetched {
        repo_root: std::path::PathBuf,
        downloaded_objects: usize,
    },
    Pulled {
        repo_root: std::path::PathBuf,
        snapshot_id: String,
    },
}

pub fn update_sync(
    app: &mut crate::app::PsycGuiApp,
    message: SyncMessage,
) -> iced::Task<crate::domain::Message> {
    match message {
        SyncMessage::SetRemoteName(value) => {
            app.workbench.sync.remote_name = value;
            iced::Task::none()
        }
        SyncMessage::SetRemoteSpec(value) => {
            app.workbench.sync.remote_spec = value;
            iced::Task::none()
        }
        SyncMessage::SubmitAddRemote => submit_add_remote(app),
        SyncMessage::SubmitPush => submit_push(app),
        SyncMessage::SubmitPushWithSingleWriterRisk => {
            let Some(repo_root) = app.selected_repository.clone() else {
                return iced::Task::none();
            };
            app.pending_confirmation =
                Some(crate::domain::PendingConfirmation::SingleWriterRiskPush {
                    repo_root,
                    branch_token: app.workbench.branch_token.clone(),
                });
            iced::Task::none()
        }
        SyncMessage::SubmitFetch => submit_fetch(app),
        SyncMessage::SubmitPull => submit_pull(app),
        SyncMessage::ConfirmPendingAction => confirm_pending_action(app),
        SyncMessage::CancelPendingAction => {
            app.pending_confirmation = None;
            iced::Task::none()
        }
    }
}

fn submit_add_remote(app: &mut crate::app::PsycGuiApp) -> iced::Task<crate::domain::Message> {
    if app.workbench.sync.remote_name.trim().is_empty()
        || app.workbench.sync.remote_spec.trim().is_empty()
    {
        app.workbench.sync.validation_error = Some("Remote name and spec are required".into());
        return iced::Task::none();
    }

    let Some(repo_root) = app.selected_repository.clone() else {
        return iced::Task::none();
    };

    app.workbench.sync.validation_error = None;
    let remote_name = app.workbench.sync.remote_name.trim().to_owned();
    let remote_spec = app.workbench.sync.remote_spec.trim().to_owned();
    let service = app.services.repository.clone();
    let add_repo_root = repo_root.clone();
    let job_id = app.allocate_job_id();
    app.jobs.push(crate::domain::JobRecord {
        id: job_id,
        label: "Add remote".into(),
        repo_root: Some(repo_root.clone()),
        state: crate::domain::JobState::Running,
    });

    crate::jobs::spawn_blocking_job(
        move || service.add_remote(add_repo_root, remote_name, remote_spec),
        move |result| {
            crate::domain::Message::SyncJobFinished(
                result.map(|()| SyncJobResult::RemoteAdded { repo_root }),
            )
        },
    )
}

fn submit_push(app: &mut crate::app::PsycGuiApp) -> iced::Task<crate::domain::Message> {
    let Some(repo_root) = app.selected_repository.clone() else {
        return iced::Task::none();
    };
    let branch_token = app.workbench.branch_token.clone();
    let service = app.services.repository.clone();
    let push_repo_root = repo_root.clone();
    push_job(app, "Push remote", Some(repo_root.clone()));
    crate::jobs::spawn_blocking_job(
        move || service.push_default_remote(push_repo_root, branch_token),
        move |result| {
            crate::domain::Message::SyncJobFinished(result.map(|response| SyncJobResult::Pushed {
                repo_root,
                published_snapshot_id: response.published_snapshot_id,
            }))
        },
    )
}

fn submit_fetch(app: &mut crate::app::PsycGuiApp) -> iced::Task<crate::domain::Message> {
    let Some(repo_root) = app.selected_repository.clone() else {
        return iced::Task::none();
    };
    let branch_token = app.workbench.branch_token.clone();
    let service = app.services.repository.clone();
    let fetch_repo_root = repo_root.clone();
    push_job(app, "Fetch remote", Some(repo_root.clone()));
    crate::jobs::spawn_blocking_job(
        move || service.fetch_default_remote(fetch_repo_root, branch_token),
        move |result| {
            crate::domain::Message::SyncJobFinished(result.map(|response| SyncJobResult::Fetched {
                repo_root,
                downloaded_objects: response.downloaded_objects,
            }))
        },
    )
}

fn submit_pull(app: &mut crate::app::PsycGuiApp) -> iced::Task<crate::domain::Message> {
    let Some(repo_root) = app.selected_repository.clone() else {
        return iced::Task::none();
    };
    let branch_token = app.workbench.branch_token.clone();
    let service = app.services.repository.clone();
    let pull_repo_root = repo_root.clone();
    push_job(app, "Pull remote", Some(repo_root.clone()));
    crate::jobs::spawn_blocking_job(
        move || service.pull_default_remote(pull_repo_root, branch_token),
        move |result| {
            crate::domain::Message::SyncJobFinished(result.map(|response| SyncJobResult::Pulled {
                repo_root,
                snapshot_id: response.snapshot_id,
            }))
        },
    )
}

fn confirm_pending_action(app: &mut crate::app::PsycGuiApp) -> iced::Task<crate::domain::Message> {
    let Some(crate::domain::PendingConfirmation::SingleWriterRiskPush {
        repo_root,
        branch_token,
    }) = app.pending_confirmation.take()
    else {
        return iced::Task::none();
    };
    let service = app.services.repository.clone();
    let push_repo_root = repo_root.clone();
    push_job(
        app,
        "Push remote (single-writer risk)",
        Some(repo_root.clone()),
    );
    crate::jobs::spawn_blocking_job(
        move || {
            service.push_default_remote_allowing_single_writer_risk(push_repo_root, branch_token)
        },
        move |result| {
            crate::domain::Message::SyncJobFinished(result.map(|response| SyncJobResult::Pushed {
                repo_root,
                published_snapshot_id: response.published_snapshot_id,
            }))
        },
    )
}

fn push_job(app: &mut crate::app::PsycGuiApp, label: &str, repo_root: Option<std::path::PathBuf>) {
    let job_id = app.allocate_job_id();
    app.jobs.push(crate::domain::JobRecord {
        id: job_id,
        label: label.into(),
        repo_root,
        state: crate::domain::JobState::Running,
    });
}

pub fn view_sync(app: &crate::app::PsycGuiApp) -> iced::Element<'_, crate::domain::Message> {
    use iced::widget::{button, column, container, row, text, text_input};

    let content = {
        let base = column![
            text("Sync").size(28),
            text_input("Remote name", &app.workbench.sync.remote_name)
                .on_input(SyncMessage::SetRemoteName)
                .padding(10),
            text_input("Remote spec", &app.workbench.sync.remote_spec)
                .on_input(SyncMessage::SetRemoteSpec)
                .padding(10),
            row![
                button("Add remote").on_press(SyncMessage::SubmitAddRemote),
                button("Push").on_press(SyncMessage::SubmitPush),
                button("Push (risk)").on_press(SyncMessage::SubmitPushWithSingleWriterRisk),
                button("Fetch").on_press(SyncMessage::SubmitFetch),
                button("Pull").on_press(SyncMessage::SubmitPull),
            ]
            .spacing(8),
        ]
        .spacing(12);

        if let Some(error) = app.workbench.sync.validation_error.as_ref() {
            base.push(text(error))
        } else {
            base
        }
    };

    let page: iced::Element<'_, SyncMessage> = container(content).padding(20).into();
    page.map(crate::domain::Message::from)
}
