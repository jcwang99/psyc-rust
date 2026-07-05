use e2v_gui::pages::branches::BranchesMessage;
use e2v_gui::pages::history::HistoryMessage;
use e2v_gui::testing::{FakeRepositoryService, advance};

#[test]
fn history_checkout_requires_a_target_directory() {
    let service = FakeRepositoryService::with_snapshot_list("D:/repos/demo", vec!["snap-1"]);
    let mut harness = e2v_gui::testing::boot_into_workbench(service, "D:/repos/demo");
    harness.app.workbench.history.selected_snapshot_id = Some("snap-1".into());
    harness.app.workbench.history.checkout_target_dir = String::new();

    let task = advance(&mut harness.app, HistoryMessage::SubmitCheckout.into());

    assert_eq!(task.units(), 0);
    assert_eq!(
        harness.app.workbench.history.validation_error.as_deref(),
        Some("Checkout target directory is required")
    );
}

#[test]
fn branch_create_checkout_and_delete_refresh_the_branch_table() {
    let service =
        FakeRepositoryService::with_branch_table("D:/repos/demo", vec!["main", "feature-a"]);
    let mut harness = e2v_gui::testing::boot_into_workbench(service, "D:/repos/demo");

    let _ = advance(
        &mut harness.app,
        BranchesMessage::CreateBranch("feature-b".into()).into(),
    );
    let _ = advance(
        &mut harness.app,
        BranchesMessage::CheckoutBranch("feature-b".into()).into(),
    );
    let _ = advance(
        &mut harness.app,
        BranchesMessage::DeleteBranch("feature-a".into()).into(),
    );

    assert_eq!(harness.app.workbench.branches.rows[0].name, "feature-b");
    assert_eq!(harness.app.workbench.overview.branch_name, "feature-b");
}
