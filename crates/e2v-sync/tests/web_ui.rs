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
async fn snapshot_page_url_encodes_file_links_for_special_character_names() {
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
    std::fs::write(repo_root.join("report&notes#1+.txt"), b"hello web").unwrap();
    let commit = facade
        .commit(e2v_core::CommitOptions {
            repo_root: repo_root.clone(),
            message: "special-file".to_string(),
        })
        .unwrap();

    let app = build_local_web_router(repo_root.clone());
    let response = app
        .oneshot(
            http::Request::builder()
                .uri(format!("/snapshots/{}?path=", commit.snapshot_id))
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
    assert!(text.contains("report&amp;notes#1+.txt"));
    assert!(text.contains("path=report%26notes%231%2B.txt"));
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
async fn branch_page_url_encodes_directory_links_for_special_character_names() {
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
    std::fs::create_dir_all(repo_root.join("dir&notes#1+")).unwrap();
    std::fs::write(
        repo_root.join("dir&notes#1+").join("child.txt"),
        b"hello from child",
    )
    .unwrap();
    facade
        .commit(e2v_core::CommitOptions {
            repo_root: repo_root.clone(),
            message: "branch-special-dir".to_string(),
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
    assert!(text.contains("dir&amp;notes#1+"));
    assert!(text.contains("path=dir%26notes%231%2B"));
}

#[tokio::test]
async fn snapshot_page_escapes_single_quotes_in_visible_names() {
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
    std::fs::write(repo_root.join("report'oops.txt"), b"hello web").unwrap();
    let commit = facade
        .commit(e2v_core::CommitOptions {
            repo_root: repo_root.clone(),
            message: "single-quote-file".to_string(),
        })
        .unwrap();

    let app = build_local_web_router(repo_root.clone());
    let response = app
        .oneshot(
            http::Request::builder()
                .uri(format!("/snapshots/{}?path=", commit.snapshot_id))
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
    assert!(text.contains("report&#39;oops.txt"));
    assert!(!text.contains("report'oops.txt"));
}

#[tokio::test]
async fn missing_snapshot_page_escapes_single_quotes_in_error_message_context() {
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("repo'root");
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
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(!text.contains("repo'root"));
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

#[tokio::test]
async fn invalid_snapshot_page_returns_bad_request_html_instead_of_internal_server_error() {
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
                .uri("/snapshots/%2E%2E?path=")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), http::StatusCode::BAD_REQUEST);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("Bad Request"));
    assert!(text.contains("Invalid snapshot id"));
}

#[tokio::test]
async fn tampered_snapshot_page_renders_internal_server_error_html() {
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
            message: "tampered-page".to_string(),
        })
        .unwrap();

    let snapshot_path = repo_root
        .join(".e2v")
        .join("objects")
        .join(format!("{}.json", commit.snapshot_id));
    let mut bytes = std::fs::read(&snapshot_path).unwrap();
    let last_index = bytes.len() - 1;
    bytes[last_index] ^= 0x01;
    std::fs::write(&snapshot_path, bytes).unwrap();

    let app = build_local_web_router(repo_root.clone());
    let response = app
        .oneshot(
            http::Request::builder()
                .uri(format!("/snapshots/{}?path=", commit.snapshot_id))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), http::StatusCode::INTERNAL_SERVER_ERROR);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("<html"));
    assert!(text.contains("Internal Server Error"));
}

#[tokio::test]
async fn tampered_branch_page_renders_internal_server_error_html() {
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
    std::fs::write(repo_root.join("hello.txt"), b"hello web").unwrap();
    facade
        .commit(e2v_core::CommitOptions {
            repo_root: repo_root.clone(),
            message: "tampered-branch-page".to_string(),
        })
        .unwrap();

    let ref_path = repo_root.join(".e2v").join("refs").join("default.json");
    let mut bytes = std::fs::read(&ref_path).unwrap();
    let last_index = bytes.len() - 1;
    bytes[last_index] ^= 0x01;
    std::fs::write(&ref_path, bytes).unwrap();

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

    assert_eq!(response.status(), http::StatusCode::INTERNAL_SERVER_ERROR);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("<html"));
    assert!(text.contains("Internal Server Error"));
}

#[tokio::test]
async fn invalid_branch_page_returns_bad_request_html_instead_of_internal_server_error() {
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
                .uri("/branches/%2E%2E?path=")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), http::StatusCode::BAD_REQUEST);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("Bad Request"));
    assert!(text.contains("Invalid branch token"));
}
