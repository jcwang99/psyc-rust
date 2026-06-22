use e2v_sync::{ServeOptions, build_local_web_router, serve_local_web};
use tower::util::ServiceExt;

#[test]
fn local_web_router_can_be_constructed_for_a_repository() {
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    std::fs::create_dir_all(&repo_root).unwrap();

    let router = build_local_web_router(repo_root);

    let _ = router;
}

#[tokio::test]
async fn snapshots_api_lists_latest_snapshots() {
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
    facade
        .commit(e2v_core::CommitOptions {
            repo_root: repo_root.clone(),
            message: "first".to_string(),
        })
        .unwrap();

    let app = build_local_web_router(repo_root.clone());
    let response = app
        .oneshot(
            http::Request::builder()
                .uri("/api/snapshots")
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
    assert!(text.contains("snapshot_id"));
    assert!(text.contains("first"));
}

#[tokio::test]
async fn snapshot_tree_api_lists_directory_entries() {
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
                .uri(format!(
                    "/api/snapshots/{}/tree?path=nested",
                    commit.snapshot_id
                ))
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
    assert!(text.contains("\"kind\":\"file\""));
}

#[tokio::test]
async fn branch_tree_api_resolves_branch_and_lists_root_entries() {
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
            message: "tree".to_string(),
        })
        .unwrap();

    let app = build_local_web_router(repo_root.clone());
    let response = app
        .oneshot(
            http::Request::builder()
                .uri(format!(
                    "/api/branches/{}/tree?path=",
                    state.branch.token_hex
                ))
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
}

#[tokio::test]
async fn snapshot_file_api_downloads_full_file() {
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
            message: "file".to_string(),
        })
        .unwrap();

    let app = build_local_web_router(repo_root.clone());
    let response = app
        .oneshot(
            http::Request::builder()
                .uri(format!(
                    "/api/snapshots/{}/file?path=nested/child.txt",
                    commit.snapshot_id
                ))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), http::StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(body.as_ref(), b"hello from child");
}

#[tokio::test]
async fn snapshot_file_api_honors_single_byte_range_requests() {
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
            message: "range".to_string(),
        })
        .unwrap();

    let app = build_local_web_router(repo_root.clone());
    let response = app
        .oneshot(
            http::Request::builder()
                .uri(format!(
                    "/api/snapshots/{}/file?path=nested/child.txt",
                    commit.snapshot_id
                ))
                .header(http::header::RANGE, "bytes=0-4")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), http::StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        response.headers().get(http::header::CONTENT_RANGE).unwrap(),
        "bytes 0-4/16"
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(body.as_ref(), b"hello");
}

#[tokio::test]
async fn branch_file_api_downloads_full_file() {
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
            message: "branch-file".to_string(),
        })
        .unwrap();

    let app = build_local_web_router(repo_root.clone());
    let response = app
        .oneshot(
            http::Request::builder()
                .uri(format!(
                    "/api/branches/{}/file?path=nested/child.txt",
                    state.branch.token_hex
                ))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), http::StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(body.as_ref(), b"hello from child");
}

#[tokio::test]
async fn snapshot_file_api_returns_unsatisfied_content_range_for_out_of_bounds_ranges() {
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
            message: "range-416".to_string(),
        })
        .unwrap();

    let app = build_local_web_router(repo_root.clone());
    let response = app
        .oneshot(
            http::Request::builder()
                .uri(format!(
                    "/api/snapshots/{}/file?path=nested/child.txt",
                    commit.snapshot_id
                ))
                .header(http::header::RANGE, "bytes=99-100")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), http::StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(
        response.headers().get(http::header::CONTENT_RANGE).unwrap(),
        "bytes */16"
    );
}

#[tokio::test]
async fn serve_local_web_binds_localhost_and_serves_home_page() {
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
            message: "serve".to_string(),
        })
        .unwrap();

    let handle = serve_local_web(ServeOptions {
        repo_root: repo_root.clone(),
    })
    .await
    .unwrap();
    assert!(handle.local_addr().ip().is_loopback());

    let addr = handle.local_addr();
    let response = tokio::task::spawn_blocking(move || {
        use std::{
            io::{Read, Write},
            net::TcpStream,
            thread,
            time::Duration,
        };

        for _ in 0..20 {
            match TcpStream::connect(addr) {
                Ok(mut stream) => {
                    stream
                        .write_all(
                            b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                        )
                        .unwrap();
                    let mut response = String::new();
                    stream.read_to_string(&mut response).unwrap();
                    return response;
                }
                Err(_) => thread::sleep(Duration::from_millis(25)),
            }
        }

        panic!("server did not accept connections in time");
    })
    .await
    .unwrap();

    assert!(response.starts_with("HTTP/1.1 200 OK"));
    assert!(response.contains("Snapshots"));
    assert!(response.contains(commit.snapshot_id.as_str()));
}

#[tokio::test]
async fn missing_snapshot_tree_api_returns_not_found() {
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
                .uri("/api/snapshots/does-not-exist/tree?path=")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn snapshot_file_api_rejects_malformed_multi_range_requests() {
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
            message: "bad-range".to_string(),
        })
        .unwrap();

    let app = build_local_web_router(repo_root.clone());
    let response = app
        .oneshot(
            http::Request::builder()
                .uri(format!(
                    "/api/snapshots/{}/file?path=nested/child.txt",
                    commit.snapshot_id
                ))
                .header(http::header::RANGE, "bytes=0-1,3-4")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn branch_file_api_honors_single_byte_range_requests() {
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
            message: "branch-range".to_string(),
        })
        .unwrap();

    let app = build_local_web_router(repo_root.clone());
    let response = app
        .oneshot(
            http::Request::builder()
                .uri(format!(
                    "/api/branches/{}/file?path=nested/child.txt",
                    state.branch.token_hex
                ))
                .header(http::header::RANGE, "bytes=6-9")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), http::StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        response.headers().get(http::header::CONTENT_RANGE).unwrap(),
        "bytes 6-9/16"
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(body.as_ref(), b"from");
}
