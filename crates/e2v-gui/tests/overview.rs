use e2v_gui::pages::overview::{OverviewJobResult, OverviewMessage};
use e2v_gui::testing::{FakeRepositoryService, advance};

#[test]
fn submitting_commit_adds_a_running_job_record() {
    let service = FakeRepositoryService::with_open_result("D:/repos/demo", "main", Some("snap-1"));
    let mut harness = e2v_gui::testing::boot_into_workbench(service, "D:/repos/demo");
    harness.app.workbench.overview.commit_message = "snapshot 2".into();

    let _ = advance(&mut harness.app, OverviewMessage::SubmitCommit.into());

    assert_eq!(harness.app.jobs.len(), 1);
    assert_eq!(harness.app.jobs[0].label, "Commit repository");
    assert!(matches!(
        harness.app.jobs[0].state,
        e2v_gui::JobState::Running
    ));
}

#[test]
fn successful_commit_refreshes_the_head_snapshot_in_overview() {
    let service =
        FakeRepositoryService::with_commit_result("D:/repos/demo", "main", "snap-2", "snapshot 2");
    let mut harness = e2v_gui::testing::boot_into_workbench(service, "D:/repos/demo");

    let _ = advance(
        &mut harness.app,
        e2v_gui::Message::OverviewJobFinished(Ok(OverviewJobResult::Committed {
            repo_root: "D:/repos/demo".into(),
            head_snapshot_id: "snap-2".into(),
            last_message: "snapshot 2".into(),
        })),
    );

    assert_eq!(
        harness.app.workbench.overview.head_snapshot_id.as_deref(),
        Some("snap-2")
    );
}
