#[derive(Debug)]
pub struct PreviewState {
    pub local_web_url: Option<String>,
    pub local_web_running: bool,
    pub mount_summary: Option<e2v_vfs::MountLaunchSummary>,
    pub selected_snapshot_id: Option<String>,
    pub mount_point_text: String,
    pub writable_live_branch: bool,
    pub validation_error: Option<String>,
    pub local_web_controller: Option<crate::services::LocalWebController>,
    pub mount_controller: Option<crate::services::MountController>,
    pub focused_path: Option<String>,
}

impl Default for PreviewState {
    fn default() -> Self {
        Self {
            local_web_url: None,
            local_web_running: false,
            mount_summary: None,
            selected_snapshot_id: None,
            mount_point_text: String::new(),
            writable_live_branch: false,
            validation_error: None,
            local_web_controller: None,
            mount_controller: None,
            focused_path: None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum PreviewMessage {
    StartLocalWeb,
    StopLocalWeb,
    StartSnapshotMount,
    StartLiveBranchMount,
    StopMount,
    OpenLocalWebInBrowser,
    OpenMountedPath,
    SetMountPoint(String),
    SetSelectedSnapshot(String),
    SetWritableLiveBranch(bool),
}

pub fn update_preview(
    app: &mut crate::app::PsycGuiApp,
    message: PreviewMessage,
) -> iced::Task<crate::domain::Message> {
    let repo_root = app.selected_repository.clone().unwrap_or_default();
    match message {
        PreviewMessage::StartLocalWeb => {
            let controller = app.services.preview.start_local_web(repo_root).unwrap();
            app.workbench.preview.local_web_url = Some(controller.local_url.clone());
            app.workbench.preview.local_web_running = true;
            app.workbench.preview.local_web_controller = Some(controller);
            iced::Task::none()
        }
        PreviewMessage::StopLocalWeb => {
            if let Some(controller) = app.workbench.preview.local_web_controller.take() {
                let _ = app.services.preview.stop_local_web(controller);
            }
            app.workbench.preview.local_web_running = false;
            app.workbench.preview.local_web_url = None;
            iced::Task::none()
        }
        PreviewMessage::StartSnapshotMount => {
            let Some(snapshot_id) = app.workbench.preview.selected_snapshot_id.clone() else {
                app.workbench.preview.validation_error =
                    Some("Snapshot selection is required".into());
                return iced::Task::none();
            };
            if app.workbench.preview.mount_point_text.trim().is_empty() {
                app.workbench.preview.validation_error = Some("Mount point is required".into());
                return iced::Task::none();
            }
            let mount_point =
                std::path::PathBuf::from(app.workbench.preview.mount_point_text.trim());
            let controller = app
                .services
                .preview
                .start_snapshot_mount(repo_root, snapshot_id, mount_point)
                .unwrap();
            app.workbench.preview.mount_summary = Some(controller.summary.clone());
            app.workbench.preview.mount_controller = Some(controller);
            app.workbench.preview.validation_error = None;
            iced::Task::none()
        }
        PreviewMessage::StartLiveBranchMount => {
            if app.workbench.preview.mount_point_text.trim().is_empty() {
                app.workbench.preview.validation_error = Some("Mount point is required".into());
                return iced::Task::none();
            }
            let mount_point =
                std::path::PathBuf::from(app.workbench.preview.mount_point_text.trim());
            let controller = app
                .services
                .preview
                .start_live_branch_mount(repo_root, app.workbench.branch_token.clone(), mount_point)
                .unwrap();
            app.workbench.preview.mount_summary = Some(controller.summary.clone());
            app.workbench.preview.mount_controller = Some(controller);
            app.workbench.preview.validation_error = None;
            iced::Task::none()
        }
        PreviewMessage::StopMount => {
            if let Some(controller) = app.workbench.preview.mount_controller.take() {
                let _ = app.services.preview.stop_mount(controller);
            }
            app.workbench.preview.mount_summary = None;
            iced::Task::none()
        }
        PreviewMessage::OpenMountedPath => {
            if let Some(summary) = app.workbench.preview.mount_summary.as_ref() {
                let _ = app.services.host_shell.open_path(&summary.mount_point);
            }
            iced::Task::none()
        }
        PreviewMessage::OpenLocalWebInBrowser => {
            if let Some(url) = app.workbench.preview.local_web_url.as_ref() {
                let _ = app.services.host_shell.open_url(url);
            }
            iced::Task::none()
        }
        PreviewMessage::SetMountPoint(value) => {
            app.workbench.preview.mount_point_text = value;
            iced::Task::none()
        }
        PreviewMessage::SetSelectedSnapshot(value) => {
            app.workbench.preview.selected_snapshot_id = Some(value);
            iced::Task::none()
        }
        PreviewMessage::SetWritableLiveBranch(value) => {
            app.workbench.preview.writable_live_branch = value;
            iced::Task::none()
        }
    }
}

pub fn view_preview(app: &crate::app::PsycGuiApp) -> iced::Element<'_, crate::domain::Message> {
    use iced::widget::{button, column, container, row, text, text_input};

    let snapshots = if app.workbench.history.rows.is_empty() {
        column![text("No snapshots available yet.")]
    } else {
        app.workbench
            .history
            .rows
            .iter()
            .fold(column![].spacing(8), |column, snapshot| {
                let label = if app.workbench.preview.selected_snapshot_id.as_deref()
                    == Some(snapshot.snapshot_id.as_str())
                {
                    format!("> {}", snapshot.snapshot_id)
                } else {
                    snapshot.snapshot_id.clone()
                };
                column.push(
                    button(text(label)).on_press(PreviewMessage::SetSelectedSnapshot(
                        snapshot.snapshot_id.clone(),
                    )),
                )
            })
    };

    let base = column![
        text("Preview").size(28),
        row![
            button("Start local web").on_press(PreviewMessage::StartLocalWeb),
            button("Stop local web").on_press(PreviewMessage::StopLocalWeb),
            button("Open local web").on_press(PreviewMessage::OpenLocalWebInBrowser),
        ]
        .spacing(8),
        text(
            app.workbench
                .preview
                .local_web_url
                .clone()
                .unwrap_or_else(|| "Local web not running".into())
        ),
        text("Snapshots").size(22),
        snapshots,
        text_input("Mount point", &app.workbench.preview.mount_point_text)
            .on_input(PreviewMessage::SetMountPoint)
            .padding(10),
        button(if app.workbench.preview.writable_live_branch {
            "Writable live branch: on"
        } else {
            "Writable live branch: off"
        })
        .on_press(PreviewMessage::SetWritableLiveBranch(
            !app.workbench.preview.writable_live_branch,
        )),
        row![
            button("Mount snapshot").on_press(PreviewMessage::StartSnapshotMount),
            button("Mount live branch").on_press(PreviewMessage::StartLiveBranchMount),
            button("Stop mount").on_press(PreviewMessage::StopMount),
            button("Open mount").on_press(PreviewMessage::OpenMountedPath),
        ]
        .spacing(8),
    ]
    .spacing(12);

    let base = if let Some(summary) = app.workbench.preview.mount_summary.as_ref() {
        base.push(text(format!(
            "Mount: {} at {} ({})",
            summary.mount_mode,
            summary.mount_point.display(),
            summary.status_message
        )))
    } else {
        base
    };

    let base = if let Some(path) = app.workbench.preview.focused_path.as_ref() {
        base.push(text(format!("Focused path: {path}")))
    } else {
        base
    };

    let content = if let Some(error) = app.workbench.preview.validation_error.as_ref() {
        base.push(text(error))
    } else {
        base
    };

    let page: iced::Element<'_, PreviewMessage> = container(content).padding(20).into();
    page.map(crate::domain::Message::from)
}
