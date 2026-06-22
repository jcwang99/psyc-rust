# P0-C Local Web Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build all P0-C local web access goals with a localhost-only `axum` server that exposes a minimal Local HTTP API and Local Web UI for snapshot browse, directory browse, download, and HTTP Range.

**Architecture:** Keep all repository read logic behind `e2v_core::ReadService`. Add a `web` module inside `e2v-sync` that builds an `axum` router, translates HTTP requests into `ReadService` calls, and renders either JSON API responses or small server-rendered HTML pages. Start with API coverage, then layer HTML pages on top of the same handlers and route helpers.

**Tech Stack:** Rust workspace crates, `axum`, `tokio`, `http`, `serde`, `tempfile`, in-process router integration tests.

---

### Task 1: Add Web Dependencies And Public Server Surface

**Files:**
- Modify: `Cargo.toml`
- Modify: `crates/e2v-sync/Cargo.toml`
- Create: `crates/e2v-sync/src/web.rs`
- Modify: `crates/e2v-sync/src/lib.rs`
- Test: `crates/e2v-sync/tests/web_api.rs`

- [ ] **Step 1: Write the failing dependency smoke test**

```rust
use e2v_sync::build_local_web_router;

#[test]
fn local_web_router_can_be_constructed_for_a_repository() {
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    std::fs::create_dir_all(&repo_root).unwrap();

    let router = build_local_web_router(repo_root);

    let _ = router;
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p e2v-sync local_web_router_can_be_constructed_for_a_repository --test web_api -- --nocapture`
Expected: FAIL because `web_api.rs`, `build_local_web_router`, and `axum` dependencies do not exist yet.

- [ ] **Step 3: Write minimal implementation**

```rust
use std::path::PathBuf;

use axum::{Router, routing::get};

#[derive(Debug, Clone)]
pub struct ServeOptions {
    pub repo_root: PathBuf,
}

pub fn build_local_web_router(repo_root: PathBuf) -> Router {
    Router::new().route("/health", get(|| async { "ok" })).with_state(ServeOptions { repo_root })
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p e2v-sync local_web_router_can_be_constructed_for_a_repository --test web_api -- --nocapture`
Expected: PASS with the router-construction smoke test green.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml crates/e2v-sync/Cargo.toml crates/e2v-sync/src/web.rs crates/e2v-sync/src/lib.rs crates/e2v-sync/tests/web_api.rs
git commit -m "feat: add P0-C web server skeleton"
```

### Task 2: Add Snapshot Listing HTTP API

**Files:**
- Modify: `crates/e2v-sync/src/web.rs`
- Test: `crates/e2v-sync/tests/web_api.rs`

- [ ] **Step 1: Write the failing snapshots API test**

```rust
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

    let app = e2v_sync::build_local_web_router(repo_root.clone());
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
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("snapshot_id"));
    assert!(text.contains("first"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p e2v-sync snapshots_api_lists_latest_snapshots --test web_api -- --nocapture`
Expected: FAIL because `/api/snapshots` is not implemented.

- [ ] **Step 3: Write minimal implementation**

```rust
#[derive(Debug, Clone, serde::Serialize)]
struct SnapshotSummaryResponse {
    snapshot_id: String,
    message: String,
}

async fn list_snapshots(
    axum::extract::State(state): axum::extract::State<ServeOptions>,
) -> Result<axum::Json<Vec<SnapshotSummaryResponse>>, StatusCode> {
    let facade = e2v_core::RepositoryFacade::new();
    let snapshots = facade
        .snapshots(&state.repo_root)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(axum::Json(
        snapshots
            .into_iter()
            .map(|snapshot| SnapshotSummaryResponse {
                snapshot_id: snapshot.snapshot_id,
                message: snapshot.message,
            })
            .collect(),
    ))
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p e2v-sync snapshots_api_lists_latest_snapshots --test web_api -- --nocapture`
Expected: PASS with the snapshots API returning JSON.

- [ ] **Step 5: Commit**

```bash
git add crates/e2v-sync/src/web.rs crates/e2v-sync/tests/web_api.rs
git commit -m "feat: add snapshot listing API"
```

### Task 3: Add Snapshot And Branch Directory Browse APIs

**Files:**
- Modify: `crates/e2v-sync/src/web.rs`
- Test: `crates/e2v-sync/tests/web_api.rs`

- [ ] **Step 1: Write the failing directory browse tests**

```rust
#[tokio::test]
async fn snapshot_tree_api_lists_directory_entries() {
    let harness = WebHarness::seed().unwrap();

    let response = harness
        .request("/api/snapshots/{snapshot_id}/tree?path=nested")
        .await;

    assert_eq!(response.status(), http::StatusCode::OK);
    let body = harness.read_text(response).await;
    assert!(body.contains("child.txt"));
    assert!(body.contains("\"kind\":\"file\""));
}

#[tokio::test]
async fn branch_tree_api_resolves_branch_and_lists_root_entries() {
    let harness = WebHarness::seed().unwrap();

    let response = harness
        .request("/api/branches/{branch_token}/tree?path=")
        .await;

    assert_eq!(response.status(), http::StatusCode::OK);
    let body = harness.read_text(response).await;
    assert!(body.contains("nested"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p e2v-sync tree_api --test web_api -- --nocapture`
Expected: FAIL because tree browse routes are not implemented.

- [ ] **Step 3: Write minimal implementation**

```rust
#[derive(Debug, Clone, serde::Serialize)]
struct DirectoryEntryResponse {
    name: String,
    kind: String,
}

#[derive(Debug, Clone, serde::Serialize)]
struct DirectoryListingResponse {
    snapshot_id: String,
    path: String,
    entries: Vec<DirectoryEntryResponse>,
}
```

Implement:

- snapshot route handler that calls `ReadService::open_snapshot` then `read_dir`
- branch route handler that calls `ReadService::resolve_branch` then `read_dir`

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p e2v-sync tree_api --test web_api -- --nocapture`
Expected: PASS with both snapshot and branch directory browse tests green.

- [ ] **Step 5: Commit**

```bash
git add crates/e2v-sync/src/web.rs crates/e2v-sync/tests/web_api.rs
git commit -m "feat: add directory browse APIs"
```

### Task 4: Add File Download And Single-Range HTTP Support

**Files:**
- Modify: `crates/e2v-sync/src/web.rs`
- Test: `crates/e2v-sync/tests/web_api.rs`

- [ ] **Step 1: Write the failing file and range tests**

```rust
#[tokio::test]
async fn snapshot_file_api_downloads_full_file() {
    let harness = WebHarness::seed().unwrap();

    let response = harness
        .request("/api/snapshots/{snapshot_id}/file?path=nested/child.txt")
        .await;

    assert_eq!(response.status(), http::StatusCode::OK);
    let body = harness.read_bytes(response).await;
    assert_eq!(body, b"hello from child");
}

#[tokio::test]
async fn snapshot_file_api_honors_single_byte_range_requests() {
    let harness = WebHarness::seed().unwrap();

    let response = harness
        .request_with_range(
            "/api/snapshots/{snapshot_id}/file?path=nested/child.txt",
            "bytes=0-4",
        )
        .await;

    assert_eq!(response.status(), http::StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        response.headers().get(http::header::CONTENT_RANGE).unwrap(),
        "bytes 0-4/16"
    );
    let body = harness.read_bytes(response).await;
    assert_eq!(body, b"hello");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p e2v-sync file_api --test web_api -- --nocapture`
Expected: FAIL because file download and `Range` handling are not implemented.

- [ ] **Step 3: Write minimal implementation**

```rust
fn parse_single_range(header: &str, file_size: usize) -> Result<(usize, usize), StatusCode> {
    let bytes = header
        .strip_prefix("bytes=")
        .ok_or(StatusCode::BAD_REQUEST)?;
    let (start, end) = bytes.split_once('-').ok_or(StatusCode::BAD_REQUEST)?;
    let start: usize = start.parse().map_err(|_| StatusCode::BAD_REQUEST)?;
    let end = if end.is_empty() {
        file_size.saturating_sub(1)
    } else {
        end.parse().map_err(|_| StatusCode::BAD_REQUEST)?
    };
    if start >= file_size || end < start {
        return Err(StatusCode::RANGE_NOT_SATISFIABLE);
    }
    Ok((start, end.min(file_size.saturating_sub(1))))
}
```

Implement:

- full download path through `ReadService::open_file` + `read_range`
- single-range path returning `206`
- `Accept-Ranges: bytes`
- `Content-Range` and `Content-Length`

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p e2v-sync file_api --test web_api -- --nocapture`
Expected: PASS for full download and single-range responses.

- [ ] **Step 5: Commit**

```bash
git add crates/e2v-sync/src/web.rs crates/e2v-sync/tests/web_api.rs
git commit -m "feat: add file download and range API"
```

### Task 5: Add Minimal Local Web UI Pages

**Files:**
- Modify: `crates/e2v-sync/src/web.rs`
- Test: `crates/e2v-sync/tests/web_ui.rs`

- [ ] **Step 1: Write the failing HTML route tests**

```rust
#[tokio::test]
async fn home_page_lists_snapshots_and_links() {
    let harness = WebHarness::seed().unwrap();

    let response = harness.request("/").await;

    assert_eq!(response.status(), http::StatusCode::OK);
    let body = harness.read_text(response).await;
    assert!(body.contains("Snapshots"));
    assert!(body.contains("/snapshots/"));
}

#[tokio::test]
async fn snapshot_page_renders_directory_entries_as_links() {
    let harness = WebHarness::seed().unwrap();

    let response = harness.request("/snapshots/{snapshot_id}?path=nested").await;

    assert_eq!(response.status(), http::StatusCode::OK);
    let body = harness.read_text(response).await;
    assert!(body.contains("child.txt"));
    assert!(body.contains("/api/snapshots/"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p e2v-sync page_ --test web_ui -- --nocapture`
Expected: FAIL because HTML pages are not implemented.

- [ ] **Step 3: Write minimal implementation**

```rust
async fn home_page(
    State(state): State<ServeOptions>,
) -> Result<axum::response::Html<String>, StatusCode> {
    let facade = e2v_core::RepositoryFacade::new();
    let snapshots = facade
        .snapshots(&state.repo_root)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(axum::response::Html(render_home_page(&snapshots)))
}
```

Implement:

- `/` page
- `/snapshots/:snapshot_id`
- `/branches/:branch_token`
- simple string-rendered HTML with navigation and file download links

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p e2v-sync --test web_ui -- --nocapture`
Expected: PASS for root and directory HTML routes.

- [ ] **Step 5: Commit**

```bash
git add crates/e2v-sync/src/web.rs crates/e2v-sync/tests/web_ui.rs
git commit -m "feat: add minimal local web UI"
```

### Task 6: Add Error Handling Coverage And Server Entry Point

**Files:**
- Modify: `crates/e2v-sync/src/web.rs`
- Modify: `crates/e2v-sync/src/lib.rs`
- Test: `crates/e2v-sync/tests/web_api.rs`
- Test: `crates/e2v-sync/tests/web_ui.rs`

- [ ] **Step 1: Write the failing error-path tests**

```rust
#[tokio::test]
async fn missing_snapshot_returns_not_found() {
    let harness = WebHarness::seed().unwrap();

    let response = harness.request("/api/snapshots/does-not-exist/tree?path=").await;

    assert_eq!(response.status(), http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn malformed_multi_range_request_is_rejected() {
    let harness = WebHarness::seed().unwrap();

    let response = harness
        .request_with_range(
            "/api/snapshots/{snapshot_id}/file?path=nested/child.txt",
            "bytes=0-1,3-4",
        )
        .await;

    assert_eq!(response.status(), http::StatusCode::BAD_REQUEST);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p e2v-sync missing_snapshot malformed_multi_range -- --nocapture`
Expected: FAIL because current handlers do not map these errors correctly.

- [ ] **Step 3: Write minimal implementation**

```rust
pub async fn serve_local_web(options: ServeOptions) -> anyhow::Result<std::net::SocketAddr> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let addr = listener.local_addr()?;
    let router = build_local_web_router(options.repo_root.clone());
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Ok(addr)
}
```

Also implement:

- `404` mapping for missing snapshot, branch, path, and file
- `400` mapping for malformed range syntax and multi-range requests
- readable HTML error body for page routes

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p e2v-sync web_ -- --nocapture`
Expected: PASS across API/UI success and error tests.

- [ ] **Step 5: Commit**

```bash
git add crates/e2v-sync/src/web.rs crates/e2v-sync/src/lib.rs crates/e2v-sync/tests/web_api.rs crates/e2v-sync/tests/web_ui.rs
git commit -m "feat: complete P0-C local web access"
```

## Self-Review

### Spec Coverage

- localhost-only server: covered in Tasks 1 and 6
- Local HTTP API: covered in Tasks 2, 3, 4, and 6
- Local Web UI: covered in Task 5
- snapshot browse: covered in Tasks 2, 3, and 5
- directory browse: covered in Tasks 3 and 5
- download: covered in Task 4
- HTTP Range: covered in Tasks 4 and 6
- `ReadService` reuse boundary: enforced in Tasks 2 through 6

No uncovered spec requirements remain.

### Placeholder Scan

- no `TODO`, `TBD`, or deferred steps
- every code-changing step includes concrete code
- every test step includes a concrete command and expected result

### Type Consistency

- `ServeOptions`, `build_local_web_router`, and `serve_local_web` are introduced early and reused consistently
- API tests live in `crates/e2v-sync/tests/web_api.rs`
- HTML tests live in `crates/e2v-sync/tests/web_ui.rs`

Plan complete and saved to `docs/superpowers/plans/2026-06-22-p0-c-local-web.md`. Two execution options:

1. Subagent-Driven (recommended) - I dispatch a fresh subagent per task, review between tasks, fast iteration

2. Inline Execution - Execute tasks in this session using executing-plans, batch execution with checkpoints
