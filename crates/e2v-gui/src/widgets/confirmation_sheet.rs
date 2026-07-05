pub fn has_pending_confirmation(pending: &Option<crate::domain::PendingConfirmation>) -> bool {
    pending.is_some()
}

pub fn view_confirmation_sheet(
    pending: &Option<crate::domain::PendingConfirmation>,
) -> Option<iced::Element<'_, crate::domain::Message>> {
    use iced::widget::{button, column, container, row, text};

    match pending {
        Some(crate::domain::PendingConfirmation::SingleWriterRiskPush { .. }) => Some(
            container(
                column![
                    text("Confirm single-writer-risk push").size(20),
                    text(
                        "This push allows the current device to overwrite remote state without coordination."
                    ),
                    row![
                        button("Cancel")
                            .on_press(crate::pages::sync::SyncMessage::CancelPendingAction.into()),
                        button("Continue")
                            .on_press(crate::pages::sync::SyncMessage::ConfirmPendingAction.into()),
                    ]
                    .spacing(8),
                ]
                .spacing(12),
            )
            .padding(16)
            .into(),
        ),
        None => None,
    }
}
