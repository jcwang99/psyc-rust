#[derive(Debug, Clone, Default)]
pub struct SharingState {
    pub roster: crate::services::SharingRoster,
    pub invite_member_display_name: String,
    pub accept_bundle_base64: String,
    pub accept_local_device_label: String,
    pub invite_device_actor_id: String,
    pub invite_device_label: String,
    pub revoke_password: String,
    pub validation_error: Option<String>,
    pub last_invite_bundle_base64: Option<String>,
}

#[derive(Debug, Clone)]
pub enum SharingMessage {
    SetInviteMemberDisplayName(String),
    SetAcceptBundleBase64(String),
    SetAcceptLocalDeviceLabel(String),
    SetInviteDeviceActorId(String),
    SetInviteDeviceLabel(String),
    SetRevokePassword(String),
    SubmitInviteMember,
    SubmitAcceptMember,
    SubmitInviteDevice,
    SubmitAcceptDevice,
    RequestRevokeMember { actor_id: String, password: String },
    RequestRevokeDevice { device_id: String, password: String },
    ConfirmPendingAction,
    CancelPendingAction,
}

pub fn update_sharing(
    app: &mut crate::app::PsycGuiApp,
    message: SharingMessage,
) -> iced::Task<crate::domain::Message> {
    let repo_root = app.selected_repository.clone().unwrap_or_default();
    match message {
        SharingMessage::SetInviteMemberDisplayName(value) => {
            app.workbench.sharing.invite_member_display_name = value;
            iced::Task::none()
        }
        SharingMessage::SetAcceptBundleBase64(value) => {
            app.workbench.sharing.accept_bundle_base64 = value;
            iced::Task::none()
        }
        SharingMessage::SetAcceptLocalDeviceLabel(value) => {
            app.workbench.sharing.accept_local_device_label = value;
            iced::Task::none()
        }
        SharingMessage::SetInviteDeviceActorId(value) => {
            app.workbench.sharing.invite_device_actor_id = value;
            iced::Task::none()
        }
        SharingMessage::SetInviteDeviceLabel(value) => {
            app.workbench.sharing.invite_device_label = value;
            iced::Task::none()
        }
        SharingMessage::SetRevokePassword(value) => {
            app.workbench.sharing.revoke_password = value;
            iced::Task::none()
        }
        SharingMessage::SubmitInviteMember => {
            if app
                .workbench
                .sharing
                .invite_member_display_name
                .trim()
                .is_empty()
            {
                app.workbench.sharing.validation_error =
                    Some("Member display name is required".into());
                return iced::Task::none();
            }
            let base64_bundle = app
                .services
                .sharing
                .invite_member(
                    repo_root,
                    app.workbench
                        .sharing
                        .invite_member_display_name
                        .trim()
                        .to_owned(),
                )
                .unwrap();
            app.workbench.sharing.last_invite_bundle_base64 = Some(base64_bundle);
            app.workbench.sharing.validation_error = None;
            iced::Task::none()
        }
        SharingMessage::SubmitAcceptMember => {
            if app.workbench.sharing.accept_bundle_base64.trim().is_empty()
                || app
                    .workbench
                    .sharing
                    .accept_local_device_label
                    .trim()
                    .is_empty()
            {
                app.workbench.sharing.validation_error =
                    Some("Invite bundle and local device label are required".into());
                return iced::Task::none();
            }
            app.workbench.sharing.roster = app
                .services
                .sharing
                .accept_member(
                    repo_root,
                    app.workbench.sharing.accept_bundle_base64.trim().to_owned(),
                    app.workbench
                        .sharing
                        .accept_local_device_label
                        .trim()
                        .to_owned(),
                )
                .unwrap();
            app.workbench.sharing.validation_error = None;
            iced::Task::none()
        }
        SharingMessage::SubmitInviteDevice => {
            if app
                .workbench
                .sharing
                .invite_device_actor_id
                .trim()
                .is_empty()
                || app.workbench.sharing.invite_device_label.trim().is_empty()
            {
                app.workbench.sharing.validation_error =
                    Some("Actor id and device label are required".into());
                return iced::Task::none();
            }
            let base64_bundle = app
                .services
                .sharing
                .invite_device(
                    repo_root,
                    app.workbench
                        .sharing
                        .invite_device_actor_id
                        .trim()
                        .to_owned(),
                    app.workbench.sharing.invite_device_label.trim().to_owned(),
                )
                .unwrap();
            app.workbench.sharing.last_invite_bundle_base64 = Some(base64_bundle);
            app.workbench.sharing.validation_error = None;
            iced::Task::none()
        }
        SharingMessage::SubmitAcceptDevice => {
            if app.workbench.sharing.accept_bundle_base64.trim().is_empty()
                || app
                    .workbench
                    .sharing
                    .accept_local_device_label
                    .trim()
                    .is_empty()
            {
                app.workbench.sharing.validation_error =
                    Some("Invite bundle and local device label are required".into());
                return iced::Task::none();
            }
            app.workbench.sharing.roster = app
                .services
                .sharing
                .accept_device(
                    repo_root,
                    app.workbench.sharing.accept_bundle_base64.trim().to_owned(),
                    app.workbench
                        .sharing
                        .accept_local_device_label
                        .trim()
                        .to_owned(),
                )
                .unwrap();
            app.workbench.sharing.validation_error = None;
            iced::Task::none()
        }
        SharingMessage::RequestRevokeMember { actor_id, password } => {
            app.pending_confirmation = Some(crate::domain::PendingConfirmation::RevokeMember {
                repo_root,
                actor_id,
                password,
            });
            iced::Task::none()
        }
        SharingMessage::RequestRevokeDevice {
            device_id,
            password,
        } => {
            app.pending_confirmation = Some(crate::domain::PendingConfirmation::RevokeDevice {
                repo_root,
                device_id,
                password,
            });
            iced::Task::none()
        }
        SharingMessage::ConfirmPendingAction => {
            match app.pending_confirmation.take() {
                Some(crate::domain::PendingConfirmation::RevokeMember {
                    repo_root,
                    actor_id,
                    password,
                }) => {
                    app.workbench.sharing.roster = app
                        .services
                        .sharing
                        .revoke_member(repo_root, actor_id, password)
                        .unwrap();
                }
                Some(crate::domain::PendingConfirmation::RevokeDevice {
                    repo_root,
                    device_id,
                    password,
                }) => {
                    app.workbench.sharing.roster = app
                        .services
                        .sharing
                        .revoke_device(repo_root, device_id, password)
                        .unwrap();
                }
                other => app.pending_confirmation = other,
            }
            iced::Task::none()
        }
        SharingMessage::CancelPendingAction => {
            app.pending_confirmation = None;
            iced::Task::none()
        }
    }
}

pub fn view_sharing(app: &crate::app::PsycGuiApp) -> iced::Element<'_, crate::domain::Message> {
    use iced::widget::{button, column, container, row, text, text_input};

    let actors =
        if app.workbench.sharing.roster.actors.is_empty() {
            column![text("No shared actors yet.")]
        } else {
            app.workbench.sharing.roster.actors.iter().fold(
                column![].spacing(8),
                |column, actor| {
                    column.push(
                        row![
                            text(format!("{} ({})", actor.display_name, actor.role))
                                .width(iced::Length::Fill),
                            button("Revoke").on_press(SharingMessage::RequestRevokeMember {
                                actor_id: actor.actor_id.clone(),
                                password: app.workbench.sharing.revoke_password.clone(),
                            }),
                        ]
                        .spacing(8),
                    )
                },
            )
        };

    let devices =
        if app.workbench.sharing.roster.devices.is_empty() {
            column![text("No shared devices yet.")]
        } else {
            app.workbench.sharing.roster.devices.iter().fold(
                column![].spacing(8),
                |column, device| {
                    column.push(
                        row![
                            text(format!("{} [{}]", device.label, device.status))
                                .width(iced::Length::Fill),
                            button("Revoke").on_press(SharingMessage::RequestRevokeDevice {
                                device_id: device.device_id.clone(),
                                password: app.workbench.sharing.revoke_password.clone(),
                            }),
                        ]
                        .spacing(8),
                    )
                },
            )
        };

    let content = {
        let base = column![
            text("Sharing").size(28),
            text_input(
                "Member display name",
                &app.workbench.sharing.invite_member_display_name
            )
            .on_input(SharingMessage::SetInviteMemberDisplayName)
            .padding(10),
            button("Invite member").on_press(SharingMessage::SubmitInviteMember),
            text_input("Invite bundle", &app.workbench.sharing.accept_bundle_base64)
                .on_input(SharingMessage::SetAcceptBundleBase64)
                .padding(10),
            text_input(
                "Local device label",
                &app.workbench.sharing.accept_local_device_label
            )
            .on_input(SharingMessage::SetAcceptLocalDeviceLabel)
            .padding(10),
            row![
                button("Accept member invite").on_press(SharingMessage::SubmitAcceptMember),
                button("Accept device invite").on_press(SharingMessage::SubmitAcceptDevice),
            ]
            .spacing(8),
            text_input(
                "Invite device actor id",
                &app.workbench.sharing.invite_device_actor_id
            )
            .on_input(SharingMessage::SetInviteDeviceActorId)
            .padding(10),
            text_input(
                "Invite device label",
                &app.workbench.sharing.invite_device_label
            )
            .on_input(SharingMessage::SetInviteDeviceLabel)
            .padding(10),
            button("Invite device").on_press(SharingMessage::SubmitInviteDevice),
            text_input("Revoke password", &app.workbench.sharing.revoke_password)
                .on_input(SharingMessage::SetRevokePassword)
                .padding(10),
            text("Actors").size(22),
            actors,
            text("Devices").size(22),
            devices,
        ]
        .spacing(12);

        let base = if let Some(bundle) = app.workbench.sharing.last_invite_bundle_base64.as_ref() {
            base.push(text(format!("Latest invite bundle: {bundle}")))
        } else {
            base
        };

        if let Some(error) = app.workbench.sharing.validation_error.as_ref() {
            base.push(text(error))
        } else {
            base
        }
    };

    let page: iced::Element<'_, SharingMessage> = container(content).padding(20).into();
    page.map(crate::domain::Message::from)
}
