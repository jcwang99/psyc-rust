use std::pin::Pin;

use axum::body::Body;
use e2v_sync::{ServeOptions, build_local_web_router, serve_local_web};
use e2v_core::ManifestStoreApi;
use futures_util::future::poll_fn;
use axum::body::HttpBody as _;
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
async fn snapshot_file_api_streams_large_full_downloads_in_multiple_frames() {
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
    let large_bytes = vec![b'x'; (1024 * 1024) + 257];
    std::fs::write(repo_root.join("large.bin"), &large_bytes).unwrap();
    let commit = facade
        .commit(e2v_core::CommitOptions {
            repo_root: repo_root.clone(),
            message: "large-file".to_string(),
        })
        .unwrap();

    let app = build_local_web_router(repo_root.clone());
    let response = app
        .oneshot(
            http::Request::builder()
                .uri(format!(
                    "/api/snapshots/{}/file?path=large.bin",
                    commit.snapshot_id
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), http::StatusCode::OK);
    let mut body = response.into_body();
    let mut frames = 0usize;
    let mut rebuilt = Vec::new();
    while let Some(frame) = poll_fn(|cx| Pin::new(&mut body).poll_frame(cx))
        .await
        .transpose()
        .unwrap()
    {
        if let Ok(bytes) = frame.into_data() {
            frames += 1;
            rebuilt.extend_from_slice(&bytes);
        }
    }

    assert_eq!(rebuilt, large_bytes);
    assert!(
        frames >= 2,
        "expected large full download to stream in multiple frames, saw {frames}"
    );
}

#[tokio::test]
async fn snapshot_file_api_downloads_empty_file() {
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
    std::fs::write(repo_root.join("empty.txt"), []).unwrap();
    let commit = facade
        .commit(e2v_core::CommitOptions {
            repo_root: repo_root.clone(),
            message: "empty-file".to_string(),
        })
        .unwrap();

    let app = build_local_web_router(repo_root.clone());
    let response = app
        .oneshot(
            http::Request::builder()
                .uri(format!(
                    "/api/snapshots/{}/file?path=empty.txt",
                    commit.snapshot_id
                ))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), http::StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(http::header::CONTENT_LENGTH)
            .unwrap(),
        "0"
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(body.is_empty());
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
async fn snapshot_file_api_honors_suffix_byte_range_requests() {
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
            message: "suffix-range".to_string(),
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
                .header(http::header::RANGE, "bytes=-5")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), http::StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        response.headers().get(http::header::CONTENT_RANGE).unwrap(),
        "bytes 11-15/16"
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(body.as_ref(), b"child");
}

#[tokio::test]
async fn snapshot_file_api_honors_open_ended_byte_range_requests() {
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
            message: "open-ended-range".to_string(),
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
                .header(http::header::RANGE, "bytes=6-")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), http::StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        response.headers().get(http::header::CONTENT_RANGE).unwrap(),
        "bytes 6-15/16"
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(body.as_ref(), b"from child");
}

#[tokio::test]
async fn snapshot_file_api_returns_416_for_empty_file_range_requests() {
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
    std::fs::write(repo_root.join("empty.txt"), []).unwrap();
    let commit = facade
        .commit(e2v_core::CommitOptions {
            repo_root: repo_root.clone(),
            message: "empty-range".to_string(),
        })
        .unwrap();

    let app = build_local_web_router(repo_root.clone());
    let response = app
        .oneshot(
            http::Request::builder()
                .uri(format!(
                    "/api/snapshots/{}/file?path=empty.txt",
                    commit.snapshot_id
                ))
                .header(http::header::RANGE, "bytes=0-0")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), http::StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(
        response.headers().get(http::header::CONTENT_RANGE).unwrap(),
        "bytes */0"
    );
}

#[tokio::test]
async fn snapshot_file_api_rejects_empty_suffix_range_form() {
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
            message: "bad-empty-suffix-range".to_string(),
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
                .header(http::header::RANGE, "bytes=-")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn snapshot_file_api_rejects_range_headers_with_internal_whitespace() {
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
            message: "range-whitespace".to_string(),
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
                .header(http::header::RANGE, "bytes= 0-4")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), http::StatusCode::BAD_REQUEST);
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
async fn snapshot_tree_api_returns_bad_request_for_invalid_snapshot_id_path_traversal() {
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
                .uri("/api/snapshots/%2E%2E/tree?path=")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn snapshot_file_api_returns_bad_request_for_invalid_snapshot_id_path_traversal() {
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
                .uri("/api/snapshots/%2E%2E/file?path=hello.txt")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), http::StatusCode::BAD_REQUEST);
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
async fn tampered_snapshot_tree_api_returns_internal_server_error() {
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
            message: "tampered-snapshot".to_string(),
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
                .uri(format!("/api/snapshots/{}/tree?path=", commit.snapshot_id))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), http::StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn tampered_snapshot_file_api_returns_internal_server_error_instead_of_not_found() {
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
            message: "tampered-file-object".to_string(),
        })
        .unwrap();

    let object_ids = e2v_core::sync_support::list_local_object_files(&repo_root).unwrap();
    let file_object_path = object_ids
        .into_iter()
        .find(|path| {
            let id = path.file_stem().unwrap().to_string_lossy().to_string();
            e2v_core::RepositoryFacade::new()
                .verify_object(&repo_root, &id, "file")
                .is_ok()
        })
        .unwrap();
    let mut bytes = std::fs::read(&file_object_path).unwrap();
    let last_index = bytes.len() - 1;
    bytes[last_index] ^= 0x01;
    std::fs::write(&file_object_path, bytes).unwrap();

    let app = build_local_web_router(repo_root.clone());
    let response = app
        .oneshot(
            http::Request::builder()
                .uri(format!(
                    "/api/snapshots/{}/file?path=hello.txt",
                    commit.snapshot_id
                ))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), http::StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn snapshot_file_api_returns_internal_server_error_for_authenticated_file_with_empty_chunks() {
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
            message: "empty-chunks".to_string(),
        })
        .unwrap();

    let manifest_store = e2v_core::ManifestStore::new(&repo_root);
    let snapshot = manifest_store.get_snapshot(&commit.snapshot_id).unwrap();
    let root_tree = manifest_store.get_tree_node(&snapshot.root_tree_id).unwrap();
    let file_entry = root_tree
        .entries
        .iter()
        .find(|entry| entry.name == "hello.txt")
        .unwrap()
        .clone();
    let mut file_manifest = manifest_store.get_file(&file_entry.object_id).unwrap();
    file_manifest.chunks.clear();

    let control_dir = repo_root.join(".e2v");
    let secrets = e2v_core::sync_support::open_repo_secrets_for_sync(&control_dir).unwrap();
    let object_store = e2v_store::DirectLayoutObjectStore::new(&control_dir, secrets);
    let tampered_file_id = object_store
        .put_object("file", &postcard::to_stdvec(&file_manifest).unwrap())
        .unwrap();

    let mut tampered_tree = root_tree.clone();
    tampered_tree
        .entries
        .iter_mut()
        .find(|entry| entry.name == "hello.txt")
        .unwrap()
        .object_id = tampered_file_id;
    let tampered_tree_id = object_store
        .put_object("tree", &postcard::to_stdvec(&tampered_tree).unwrap())
        .unwrap();

    let mut tampered_snapshot = snapshot.clone();
    tampered_snapshot.root_tree_id = tampered_tree_id;
    let tampered_snapshot_id = object_store
        .put_object("snapshot", &postcard::to_stdvec(&tampered_snapshot).unwrap())
        .unwrap();

    let app = build_local_web_router(repo_root.clone());
    let response = app
        .oneshot(
            http::Request::builder()
                .uri(format!(
                    "/api/snapshots/{tampered_snapshot_id}/file?path=hello.txt"
                ))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), http::StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn tampered_branch_tree_api_returns_internal_server_error_instead_of_not_found() {
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
            message: "tampered-branch-ref".to_string(),
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
                .uri(format!(
                    "/api/branches/{}/tree?path=",
                    state.branch.token_hex
                ))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), http::StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn snapshot_file_api_returns_bad_request_for_empty_file_path() {
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
            message: "invalid-logical-path".to_string(),
        })
        .unwrap();

    let app = build_local_web_router(repo_root.clone());
    let response = app
        .oneshot(
            http::Request::builder()
                .uri(format!("/api/snapshots/{}/file?path=", commit.snapshot_id))
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

#[tokio::test]
async fn branch_tree_api_returns_bad_request_for_invalid_branch_token_path_traversal() {
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
                .uri("/api/branches/%2E%2E/tree?path=")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn branch_file_api_returns_bad_request_for_invalid_branch_token_path_traversal() {
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
                .uri("/api/branches/%2E%2E/file?path=hello.txt")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), http::StatusCode::BAD_REQUEST);
}
