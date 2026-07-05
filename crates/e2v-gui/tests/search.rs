use e2v_gui::pages::search::{SearchMessage, SearchResultRow};
use e2v_gui::testing::{
    FakeRepositoryService, FakeSearchService, TestServices, advance,
    boot_into_workbench_with_services,
};

#[test]
fn search_requires_non_empty_query_before_dispatch() {
    let repository =
        FakeRepositoryService::with_open_result("D:/repos/demo", "main", Some("snap-1"));
    let search = FakeSearchService::default();
    let mut harness = boot_into_workbench_with_services(
        TestServices::new(repository).with_search(search.clone()),
        "D:/repos/demo",
    );

    let task = advance(&mut harness.app, SearchMessage::SubmitSearch.into());

    assert_eq!(task.units(), 0);
    assert_eq!(
        harness.app.workbench.search.validation_error.as_deref(),
        Some("Search query is required")
    );
    assert_eq!(search.call_count(), 0);
}

#[test]
fn successful_filename_search_populates_result_rows() {
    let repository =
        FakeRepositoryService::with_open_result("D:/repos/demo", "main", Some("snap-1"));
    let search = FakeSearchService::with_rows(vec![SearchResultRow {
        path: "notes/todo.txt".into(),
        source: "filename".into(),
        file_object_id: "obj-1".into(),
    }]);
    let mut harness = boot_into_workbench_with_services(
        TestServices::new(repository).with_search(search.clone()),
        "D:/repos/demo",
    );
    harness.app.workbench.search.query_text = "todo".into();

    let _ = advance(&mut harness.app, SearchMessage::SubmitSearch.into());

    assert_eq!(search.call_count(), 1);
    assert_eq!(harness.app.workbench.search.results.len(), 1);
    assert_eq!(
        harness.app.workbench.search.results[0].path,
        "notes/todo.txt"
    );
    assert_eq!(
        search.last_query().unwrap(),
        e2v_gui::services::SearchQuery {
            query_text: "todo".into(),
            path_prefix: None,
        }
    );
}
