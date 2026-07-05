use e2v_gui::pages::sharing::SharingMessage;
use e2v_gui::testing::{
    FakeRepositoryService, FakeSharingService, TestServices, advance,
    boot_into_workbench_with_services,
};

#[test]
fn member_invite_stores_a_copyable_base64_bundle() {
    let repository =
        FakeRepositoryService::with_open_result("D:/repos/demo", "main", Some("snap-1"));
    let sharing = FakeSharingService::with_member_invite_bundle(b"member-bundle".to_vec());
    let mut harness = boot_into_workbench_with_services(
        TestServices::new(repository).with_sharing(sharing),
        "D:/repos/demo",
    );
    harness.app.workbench.sharing.invite_member_display_name = "Alice".into();

    let _ = advance(&mut harness.app, SharingMessage::SubmitInviteMember.into());

    assert_eq!(
        harness
            .app
            .workbench
            .sharing
            .last_invite_bundle_base64
            .as_deref(),
        Some("bWVtYmVyLWJ1bmRsZQ==")
    );
}

#[test]
fn accept_member_requires_bundle_and_device_label() {
    let repository =
        FakeRepositoryService::with_open_result("D:/repos/demo", "main", Some("snap-1"));
    let sharing = FakeSharingService::default();
    let mut harness = boot_into_workbench_with_services(
        TestServices::new(repository).with_sharing(sharing),
        "D:/repos/demo",
    );

    let task = advance(&mut harness.app, SharingMessage::SubmitAcceptMember.into());

    assert_eq!(task.units(), 0);
    assert_eq!(
        harness.app.workbench.sharing.validation_error.as_deref(),
        Some("Invite bundle and local device label are required")
    );
}

#[test]
fn revoke_member_requires_confirmation_before_dispatch() {
    let repository =
        FakeRepositoryService::with_open_result("D:/repos/demo", "main", Some("snap-1"));
    let sharing = FakeSharingService::with_roster(vec![("actor-1", "Alice", "owner")], vec![]);
    let mut harness = boot_into_workbench_with_services(
        TestServices::new(repository).with_sharing(sharing),
        "D:/repos/demo",
    );

    let task = advance(
        &mut harness.app,
        SharingMessage::RequestRevokeMember {
            actor_id: "actor-1".into(),
            password: "secret".into(),
        }
        .into(),
    );

    assert_eq!(task.units(), 0);
    assert!(matches!(
        harness.app.pending_confirmation,
        Some(e2v_gui::PendingConfirmation::RevokeMember { .. })
    ));
}

#[test]
fn confirmed_device_revoke_refreshes_the_roster() {
    let repository =
        FakeRepositoryService::with_open_result("D:/repos/demo", "main", Some("snap-1"));
    let sharing = FakeSharingService::with_revocable_device("device-1");
    let mut harness = boot_into_workbench_with_services(
        TestServices::new(repository).with_sharing(sharing),
        "D:/repos/demo",
    );
    harness.app.pending_confirmation = Some(e2v_gui::PendingConfirmation::RevokeDevice {
        repo_root: "D:/repos/demo".into(),
        device_id: "device-1".into(),
        password: "secret".into(),
    });

    let _ = advance(
        &mut harness.app,
        SharingMessage::ConfirmPendingAction.into(),
    );

    assert!(
        harness
            .app
            .workbench
            .sharing
            .roster
            .devices
            .iter()
            .all(|device| device.device_id != "device-1")
    );
}
