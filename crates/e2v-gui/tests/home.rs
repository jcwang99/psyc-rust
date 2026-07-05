use e2v_gui::pages::home::{HomeJobResult, HomeMessage, NewRepositoryForm};
use e2v_gui::services::RepositoryService;
use e2v_gui::testing::{FakeRepositoryService, advance};

#[test]
fn submit_create_repository_requires_a_password_before_dispatch() {
    let mut harness = e2v_gui::testing::boot_with_service(FakeRepositoryService::default());
    harness.app.home.new_repository = NewRepositoryForm {
        repo_root_text: "D:/repos/demo".into(),
        password_text: String::new(),
        branch_name_text: "main".into(),
    };

    let task = advance(&mut harness.app, HomeMessage::SubmitCreateRepository.into());

    assert_eq!(task.units(), 0);
    assert_eq!(
        harness.app.home.validation_error.as_deref(),
        Some("Password is required")
    );
}

#[test]
fn successful_open_repository_enters_the_workbench_and_updates_recent_registry() {
    let service = FakeRepositoryService::with_open_result("D:/repos/demo", "main", Some("snap-1"));
    let mut harness = e2v_gui::testing::boot_with_service(service);
    harness.app.home.open_repository_path = "D:/repos/demo".into();

    let _ = advance(&mut harness.app, HomeMessage::SubmitOpenRepository.into());
    let _ = advance(
        &mut harness.app,
        e2v_gui::Message::HomeJobFinished(Ok(HomeJobResult::RepositoryLoaded(
            harness
                .service
                .load_repository_summary("D:/repos/demo".into())
                .unwrap(),
        ))),
    );

    assert_eq!(
        harness.app.selected_repository.as_deref(),
        Some(std::path::Path::new("D:/repos/demo"))
    );
    assert!(matches!(harness.app.screen, e2v_gui::Screen::Workbench));
    assert_eq!(
        harness.app.registry.recent[0].repo_root,
        std::path::PathBuf::from("D:/repos/demo")
    );
}
