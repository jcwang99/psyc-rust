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
    SubmitCheckout,
}

pub fn update_history(
    app: &mut crate::app::PsycGuiApp,
    message: HistoryMessage,
) -> iced::Task<crate::domain::Message> {
    match message {
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
