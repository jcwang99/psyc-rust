use e2v_gui::pages::sync::SyncMessage;
use e2v_gui::testing::{FakeRepositoryService, advance};

#[test]
fn remote_add_requires_a_name_and_spec_before_dispatch() {
    let service = FakeRepositoryService::with_open_result("D:/repos/demo", "main", Some("snap-1"));
    let mut harness = e2v_gui::testing::boot_into_workbench(service, "D:/repos/demo");

    let task = advance(&mut harness.app, SyncMessage::SubmitAddRemote.into());

    assert_eq!(task.units(), 0);
    assert_eq!(
        harness.app.workbench.sync.validation_error.as_deref(),
        Some("Remote name and spec are required")
    );
}

#[test]
fn single_writer_risk_push_requires_confirmation_before_launching_the_job() {
    let service = FakeRepositoryService::with_open_result("D:/repos/demo", "main", Some("snap-1"));
    let mut harness = e2v_gui::testing::boot_into_workbench(service, "D:/repos/demo");

    let task = advance(
        &mut harness.app,
        SyncMessage::SubmitPushWithSingleWriterRisk.into(),
    );

    assert_eq!(task.units(), 0);
    assert!(matches!(
        harness.app.pending_confirmation,
        Some(e2v_gui::domain::PendingConfirmation::SingleWriterRiskPush { .. })
    ));
    assert!(harness.app.jobs.is_empty());
}
