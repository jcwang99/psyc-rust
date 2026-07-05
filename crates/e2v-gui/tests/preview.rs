use e2v_gui::pages::preview::PreviewMessage;
use e2v_gui::pages::search::SearchMessage;
use e2v_gui::testing::{
    FakeHostShellService, FakePreviewService, FakeRepositoryService, TestServices, advance,
    boot_into_workbench_with_services, fake_snapshot_mount_summary,
};

#[test]
fn starting_local_web_registers_a_controller_and_local_url() {
    let repository =
        FakeRepositoryService::with_open_result("D:/repos/demo", "main", Some("snap-1"));
    let preview = FakePreviewService::with_local_web_url("http://127.0.0.1:44551");
    let mut harness = boot_into_workbench_with_services(
        TestServices::new(repository).with_preview(preview),
        "D:/repos/demo",
    );

    let _ = advance(&mut harness.app, PreviewMessage::StartLocalWeb.into());

    assert_eq!(
        harness.app.workbench.preview.local_web_url.as_deref(),
        Some("http://127.0.0.1:44551")
    );
    assert!(harness.app.workbench.preview.local_web_running);
}

#[test]
fn stopping_local_web_clears_the_preview_controller_state() {
    let repository =
        FakeRepositoryService::with_open_result("D:/repos/demo", "main", Some("snap-1"));
    let preview = FakePreviewService::with_local_web_url("http://127.0.0.1:44551");
    let mut harness = boot_into_workbench_with_services(
        TestServices::new(repository).with_preview(preview),
        "D:/repos/demo",
    );
    harness.app.workbench.preview.local_web_url = Some("http://127.0.0.1:44551".into());
    harness.app.workbench.preview.local_web_running = true;
    harness.app.workbench.preview.local_web_controller = Some(
        e2v_gui::testing::fake_local_web_controller("http://127.0.0.1:44551"),
    );

    let _ = advance(&mut harness.app, PreviewMessage::StopLocalWeb.into());

    assert_eq!(harness.app.workbench.preview.local_web_url, None);
    assert!(!harness.app.workbench.preview.local_web_running);
}

#[test]
fn snapshot_mount_is_rendered_read_only_and_backed_by_launch_summary() {
    let repository =
        FakeRepositoryService::with_open_result("D:/repos/demo", "main", Some("snap-1"));
    let preview = FakePreviewService::with_snapshot_mount_summary("Q:/preview", true, true);
    let mut harness = boot_into_workbench_with_services(
        TestServices::new(repository).with_preview(preview),
        "D:/repos/demo",
    );
    harness.app.workbench.preview.selected_snapshot_id = Some("snap-1".into());
    harness.app.workbench.preview.mount_point_text = "Q:/preview".into();

    let _ = advance(&mut harness.app, PreviewMessage::StartSnapshotMount.into());

    let summary = harness
        .app
        .workbench
        .preview
        .mount_summary
        .as_ref()
        .unwrap();
    assert!(summary.read_only);
    assert!(summary.stream_only);
    assert_eq!(summary.mount_mode, "snapshot-pinned");
}

#[test]
fn search_result_can_switch_to_preview_with_a_focused_path() {
    let repository =
        FakeRepositoryService::with_open_result("D:/repos/demo", "main", Some("snap-1"));
    let mut harness =
        boot_into_workbench_with_services(TestServices::new(repository), "D:/repos/demo");

    let _ = advance(
        &mut harness.app,
        SearchMessage::OpenResultInPreview("images/cat.png".into()).into(),
    );

    assert!(matches!(
        harness.app.workbench.active_page,
        e2v_gui::WorkbenchPage::Preview
    ));
    assert_eq!(
        harness.app.workbench.preview.focused_path.as_deref(),
        Some("images/cat.png")
    );
}

#[test]
fn open_mount_in_explorer_uses_the_host_shell_service() {
    let repository =
        FakeRepositoryService::with_open_result("D:/repos/demo", "main", Some("snap-1"));
    let preview = FakePreviewService::with_snapshot_mount_summary("Q:/preview", true, true);
    let host_shell = FakeHostShellService::default();
    let mut harness = boot_into_workbench_with_services(
        TestServices::new(repository)
            .with_preview(preview)
            .with_host_shell(host_shell.clone()),
        "D:/repos/demo",
    );
    harness.app.workbench.preview.mount_summary = Some(fake_snapshot_mount_summary("Q:/preview"));

    let _ = advance(&mut harness.app, PreviewMessage::OpenMountedPath.into());

    assert_eq!(
        host_shell.last_opened_path(),
        Some(std::path::PathBuf::from("Q:/preview"))
    );
}
