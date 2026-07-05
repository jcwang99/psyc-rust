use e2v_gui::services::RepositoryService;
use e2v_gui::testing::FakeRepositoryService;
use e2v_gui::widgets::home_screen::build_home_screen_model;
use e2v_gui::widgets::workbench_shell::build_workbench_shell_model;

#[test]
fn home_screen_model_includes_known_repository_cards_and_validation_errors() {
    let service = FakeRepositoryService::with_open_result("D:/repos/demo", "main", Some("snap-1"));
    let mut harness = e2v_gui::testing::boot_with_service(service.clone());
    harness.app.home.cards.push(
        service
            .load_repository_summary("D:/repos/demo".into())
            .unwrap(),
    );
    harness.app.home.validation_error = Some("Bad path".into());

    let model = build_home_screen_model(&harness.app);

    assert_eq!(model.cards[0].display_name, "demo");
    assert_eq!(model.cards[0].branch_name, "main");
    assert_eq!(model.validation_error.as_deref(), Some("Bad path"));
}

#[test]
fn workbench_shell_model_exposes_navigation_for_phase_two_pages() {
    let service = FakeRepositoryService::with_branch_table("D:/repos/demo", vec!["main"]);
    let harness = e2v_gui::testing::boot_into_workbench(service, "D:/repos/demo");

    let model = build_workbench_shell_model(&harness.app);

    assert!(model.nav_items.iter().any(|item| item.label == "Search"));
    assert!(model.nav_items.iter().any(|item| item.label == "Sharing"));
    assert!(model.nav_items.iter().any(|item| item.label == "Preview"));
}
