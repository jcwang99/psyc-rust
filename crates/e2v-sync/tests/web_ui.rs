use e2v_sync::build_local_web_router;
use tower::util::ServiceExt;

#[tokio::test]
async fn home_page_lists_snapshots_and_links() {
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    std::fs::create_dir_all(&repo_root).unwrap();

    let facade = e2v_core::RepositoryFacade::new();
    facade
        .init(e2v_core::InitOptions {
            repo_root: repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
    std::fs::write(repo_root.join("hello.txt"), b"hello web").unwrap();
    let commit = facade
        .commit(e2v_core::CommitOptions {
            repo_root: repo_root.clone(),
            message: "first".to_string(),
        })
        .unwrap();

    let app = build_local_web_router(repo_root.clone());
    let response = app
        .oneshot(
            http::Request::builder()
                .uri("/")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), http::StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("Snapshots"));
    assert!(text.contains(repo_root.display().to_string().as_str()));
    assert!(text.contains(commit.snapshot_id.as_str()));
    assert!(text.contains("/snapshots/"));
}

#[tokio::test]
async fn snapshot_page_renders_directory_entries_as_links() {
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    std::fs::create_dir_all(&repo_root).unwrap();

    let facade = e2v_core::RepositoryFacade::new();
    facade
        .init(e2v_core::InitOptions {
            repo_root: repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
    std::fs::create_dir_all(repo_root.join("nested")).unwrap();
    std::fs::write(
        repo_root.join("nested").join("child.txt"),
        b"hello from child",
    )
    .unwrap();
    let commit = facade
        .commit(e2v_core::CommitOptions {
            repo_root: repo_root.clone(),
            message: "tree".to_string(),
        })
        .unwrap();

    let app = build_local_web_router(repo_root.clone());
    let response = app
        .oneshot(
            http::Request::builder()
                .uri(format!("/snapshots/{}?path=nested", commit.snapshot_id))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), http::StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("child.txt"));
    assert!(text.contains("/api/snapshots/"));
}

#[tokio::test]
async fn branch_page_renders_root_entries_as_links() {
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    std::fs::create_dir_all(&repo_root).unwrap();

    let facade = e2v_core::RepositoryFacade::new();
    let state = facade
        .init(e2v_core::InitOptions {
            repo_root: repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
    std::fs::create_dir_all(repo_root.join("nested")).unwrap();
    std::fs::write(
        repo_root.join("nested").join("child.txt"),
        b"hello from child",
    )
    .unwrap();
    facade
        .commit(e2v_core::CommitOptions {
            repo_root: repo_root.clone(),
            message: "branch".to_string(),
        })
        .unwrap();

    let app = build_local_web_router(repo_root.clone());
    let response = app
        .oneshot(
            http::Request::builder()
                .uri(format!("/branches/{}?path=", state.branch.token_hex))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), http::StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("nested"));
    assert!(text.contains("/branches/"));
}

#[tokio::test]
async fn missing_snapshot_page_renders_readable_error_html() {
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    std::fs::create_dir_all(&repo_root).unwrap();

    let facade = e2v_core::RepositoryFacade::new();
    facade
        .init(e2v_core::InitOptions {
            repo_root: repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();

    let app = build_local_web_router(repo_root.clone());
    let response = app
        .oneshot(
            http::Request::builder()
                .uri("/snapshots/does-not-exist?path=")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), http::StatusCode::NOT_FOUND);
    assert_eq!(
        response.headers().get(http::header::CONTENT_TYPE).unwrap(),
        "text/html; charset=utf-8"
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("<html"));
    assert!(text.contains("Not Found"));
}
