#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotRow {
    pub snapshot_id: String,
    pub message: String,
}

#[derive(Debug, Clone, Default)]
pub struct HistoryState {
    pub rows: Vec<SnapshotRow>,
    pub selected_snapshot_id: Option<String>,
    pub checkout_target_dir: String,
    pub validation_error: Option<String>,
}

#[derive(Debug, Clone)]
pub enum HistoryMessage {
    SelectSnapshot(String),
    SetCheckoutTargetDir(String),
    SubmitCheckout,
}

pub fn update_history(
    app: &mut crate::app::PsycGuiApp,
    message: HistoryMessage,
) -> iced::Task<crate::domain::Message> {
    match message {
        HistoryMessage::SelectSnapshot(snapshot_id) => {
            app.workbench.history.selected_snapshot_id = Some(snapshot_id);
            iced::Task::none()
        }
        HistoryMessage::SetCheckoutTargetDir(value) => {
            app.workbench.history.checkout_target_dir = value;
            iced::Task::none()
        }
        HistoryMessage::SubmitCheckout => {
            let Some(snapshot_id) = app.workbench.history.selected_snapshot_id.clone() else {
                app.workbench.history.validation_error =
                    Some("Snapshot selection is required".into());
                return iced::Task::none();
            };

            if app.workbench.history.checkout_target_dir.trim().is_empty() {
                app.workbench.history.validation_error =
                    Some("Checkout target directory is required".into());
                return iced::Task::none();
            }

            let Some(repo_root) = app.selected_repository.clone() else {
                return iced::Task::none();
            };

            let target_dir =
                std::path::PathBuf::from(app.workbench.history.checkout_target_dir.trim());
            match app
                .services
                .repository
                .checkout_snapshot(repo_root, snapshot_id, target_dir)
            {
                Ok(()) => {
                    app.workbench.history.validation_error = None;
                    iced::Task::none()
                }
                Err(error) => {
                    app.workbench.history.validation_error = Some(error.message);
                    iced::Task::none()
                }
            }
        }
    }
}

pub fn view_history(app: &crate::app::PsycGuiApp) -> iced::Element<'_, crate::domain::Message> {
    use iced::widget::{button, column, container, text, text_input};

    let selected_snapshot = app.workbench.history.selected_snapshot_id.as_deref();
    let rows = if app.workbench.history.rows.is_empty() {
        column![text("No snapshots loaded yet.")]
    } else {
        app.workbench
            .history
            .rows
            .iter()
            .fold(column![].spacing(8), |column, snapshot| {
                let label = if selected_snapshot == Some(snapshot.snapshot_id.as_str()) {
                    format!("> {} - {}", snapshot.snapshot_id, snapshot.message)
                } else {
                    format!("{} - {}", snapshot.snapshot_id, snapshot.message)
                };
                column.push(
                    button(text(label))
                        .on_press(HistoryMessage::SelectSnapshot(snapshot.snapshot_id.clone())),
                )
            })
    };

    let content = {
        let base = column![
            text("History").size(28),
            rows,
            text_input(
                "Checkout target directory",
                &app.workbench.history.checkout_target_dir
            )
            .on_input(HistoryMessage::SetCheckoutTargetDir)
            .padding(10),
            button("Checkout snapshot").on_press(HistoryMessage::SubmitCheckout),
        ]
        .spacing(12);

        if let Some(error) = app.workbench.history.validation_error.as_ref() {
            base.push(text(error))
        } else {
            base
        }
    };

    let page: iced::Element<'_, HistoryMessage> = container(content).padding(20).into();
    page.map(crate::domain::Message::from)
}
