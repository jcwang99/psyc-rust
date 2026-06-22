use std::{net::SocketAddr, path::PathBuf};

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, Response, StatusCode},
    response::{Html, IntoResponse},
    routing::get,
};

#[derive(Debug, Clone)]
pub struct ServeOptions {
    pub repo_root: PathBuf,
}

#[derive(Debug)]
pub struct ServeHandle {
    local_addr: SocketAddr,
    server_task: tokio::task::JoinHandle<()>,
}

impl ServeHandle {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

impl Drop for ServeHandle {
    fn drop(&mut self) {
        self.server_task.abort();
    }
}

#[derive(Debug, Clone, serde::Serialize)]
struct SnapshotSummaryResponse {
    snapshot_id: String,
    message: String,
}

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

#[derive(Debug, Clone, serde::Deserialize)]
struct TreeQuery {
    path: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct FileQuery {
    path: String,
}

#[derive(Debug)]
struct PageError {
    status: StatusCode,
    message: String,
}

impl PageError {
    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }
}

impl IntoResponse for PageError {
    fn into_response(self) -> axum::response::Response {
        let title = self.status.canonical_reason().unwrap_or("Error");
        (
            self.status,
            Html(format!(
                "<html><body><h1>{title}</h1><p>{message}</p></body></html>",
                title = escape_html(title),
                message = escape_html(&self.message)
            )),
        )
            .into_response()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RangeError {
    BadRequest,
    Unsatisfiable,
}

pub fn build_local_web_router(repo_root: PathBuf) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/", get(home_page))
        .route("/branches/{branch_token}", get(branch_page))
        .route("/snapshots/{snapshot_id}", get(snapshot_page))
        .route("/api/snapshots", get(list_snapshots))
        .route("/api/snapshots/{snapshot_id}/tree", get(snapshot_tree))
        .route("/api/snapshots/{snapshot_id}/file", get(snapshot_file))
        .route("/api/branches/{branch_token}/tree", get(branch_tree))
        .route("/api/branches/{branch_token}/file", get(branch_file))
        .with_state(ServeOptions { repo_root })
}

pub async fn serve_local_web(options: ServeOptions) -> anyhow::Result<ServeHandle> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let local_addr = listener.local_addr()?;
    let router = build_local_web_router(options.repo_root);
    let server_task = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    Ok(ServeHandle {
        local_addr,
        server_task,
    })
}

async fn home_page(State(state): State<ServeOptions>) -> Result<Html<String>, PageError> {
    let facade = e2v_core::RepositoryFacade::new();
    let snapshots = facade
        .snapshots(&state.repo_root)
        .map_err(|_| PageError::internal("Failed to load snapshots"))?;

    let mut html = format!(
        "<html><body><h1>Snapshots</h1><p>Repository: {repo_root}</p><ul>",
        repo_root = escape_html(&state.repo_root.display().to_string())
    );
    for snapshot in snapshots {
        html.push_str(&format!(
            "<li><a href=\"/snapshots/{id}\">{id}</a> {message}</li>",
            id = escape_html(&snapshot.snapshot_id),
            message = escape_html(&snapshot.message)
        ));
    }
    html.push_str("</ul></body></html>");

    Ok(Html(html))
}

async fn snapshot_page(
    State(state): State<ServeOptions>,
    Path(snapshot_id): Path<String>,
    Query(query): Query<TreeQuery>,
) -> Result<Html<String>, PageError> {
    let facade = e2v_core::RepositoryFacade::new();
    let read_service = facade
        .read_service(&state.repo_root)
        .map_err(|_| PageError::internal("Failed to open repository read service"))?;
    let snapshot = read_service
        .open_snapshot(&snapshot_id)
        .map_err(|_| PageError::not_found("Snapshot not found"))?;
    let path = query.path.unwrap_or_default();
    let entries = read_service
        .read_dir(&snapshot, &path)
        .map_err(|_| PageError::not_found("Directory not found"))?;

    let mut html = format!(
        "<html><body><h1>Snapshot {snapshot_id}</h1><ul>",
        snapshot_id = escape_html(&snapshot_id)
    );
    for entry in entries {
        let entry_path = if path.is_empty() {
            entry.name.clone()
        } else {
            format!("{path}/{}", entry.name)
        };
        if entry.kind == "file" {
            html.push_str(&format!(
                "<li><a href=\"/api/snapshots/{snapshot_id}/file?path={entry_path}\">{name}</a></li>",
                snapshot_id = escape_html(&snapshot_id),
                entry_path = escape_html(&entry_path),
                name = escape_html(&entry.name)
            ));
        } else {
            html.push_str(&format!(
                "<li><a href=\"/snapshots/{snapshot_id}?path={entry_path}\">{name}</a></li>",
                snapshot_id = escape_html(&snapshot_id),
                entry_path = escape_html(&entry_path),
                name = escape_html(&entry.name)
            ));
        }
    }
    html.push_str("</ul></body></html>");

    Ok(Html(html))
}

async fn branch_page(
    State(state): State<ServeOptions>,
    Path(branch_token): Path<String>,
    Query(query): Query<TreeQuery>,
) -> Result<Html<String>, PageError> {
    let facade = e2v_core::RepositoryFacade::new();
    let read_service = facade
        .read_service(&state.repo_root)
        .map_err(|_| PageError::internal("Failed to open repository read service"))?;
    let snapshot = read_service
        .resolve_branch(&branch_token)
        .map_err(|_| PageError::not_found("Branch not found"))?;
    let path = query.path.unwrap_or_default();
    let entries = read_service
        .read_dir(&snapshot, &path)
        .map_err(|_| PageError::not_found("Directory not found"))?;

    let mut html = format!(
        "<html><body><h1>Branch {branch_token}</h1><ul>",
        branch_token = escape_html(&branch_token)
    );
    for entry in entries {
        let entry_path = if path.is_empty() {
            entry.name.clone()
        } else {
            format!("{path}/{}", entry.name)
        };
        if entry.kind == "file" {
            html.push_str(&format!(
                "<li><a href=\"/api/branches/{branch_token}/file?path={entry_path}\">{name}</a></li>",
                branch_token = escape_html(&branch_token),
                entry_path = escape_html(&entry_path),
                name = escape_html(&entry.name)
            ));
        } else {
            html.push_str(&format!(
                "<li><a href=\"/branches/{branch_token}?path={entry_path}\">{name}</a></li>",
                branch_token = escape_html(&branch_token),
                entry_path = escape_html(&entry_path),
                name = escape_html(&entry.name)
            ));
        }
    }
    html.push_str("</ul></body></html>");

    Ok(Html(html))
}

async fn list_snapshots(
    State(state): State<ServeOptions>,
) -> Result<Json<Vec<SnapshotSummaryResponse>>, StatusCode> {
    let facade = e2v_core::RepositoryFacade::new();
    let snapshots = facade
        .snapshots(&state.repo_root)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(
        snapshots
            .into_iter()
            .map(|snapshot| SnapshotSummaryResponse {
                snapshot_id: snapshot.snapshot_id,
                message: snapshot.message,
            })
            .collect(),
    ))
}

async fn snapshot_tree(
    State(state): State<ServeOptions>,
    Path(snapshot_id): Path<String>,
    Query(query): Query<TreeQuery>,
) -> Result<Json<DirectoryListingResponse>, StatusCode> {
    let facade = e2v_core::RepositoryFacade::new();
    let read_service = facade
        .read_service(&state.repo_root)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let snapshot = read_service
        .open_snapshot(&snapshot_id)
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let path = query.path.unwrap_or_default();
    let entries = read_service
        .read_dir(&snapshot, &path)
        .map_err(|_| StatusCode::NOT_FOUND)?;

    Ok(Json(DirectoryListingResponse {
        snapshot_id,
        path,
        entries: entries
            .into_iter()
            .map(|entry| DirectoryEntryResponse {
                name: entry.name,
                kind: entry.kind,
            })
            .collect(),
    }))
}

async fn branch_tree(
    State(state): State<ServeOptions>,
    Path(branch_token): Path<String>,
    Query(query): Query<TreeQuery>,
) -> Result<Json<DirectoryListingResponse>, StatusCode> {
    let facade = e2v_core::RepositoryFacade::new();
    let read_service = facade
        .read_service(&state.repo_root)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let snapshot = read_service
        .resolve_branch(&branch_token)
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let path = query.path.unwrap_or_default();
    let entries = read_service
        .read_dir(&snapshot, &path)
        .map_err(|_| StatusCode::NOT_FOUND)?;

    Ok(Json(DirectoryListingResponse {
        snapshot_id: snapshot.snapshot_id,
        path,
        entries: entries
            .into_iter()
            .map(|entry| DirectoryEntryResponse {
                name: entry.name,
                kind: entry.kind,
            })
            .collect(),
    }))
}

async fn snapshot_file(
    State(state): State<ServeOptions>,
    Path(snapshot_id): Path<String>,
    Query(query): Query<FileQuery>,
    headers: HeaderMap,
) -> Result<Response<Body>, StatusCode> {
    let facade = e2v_core::RepositoryFacade::new();
    let read_service = facade
        .read_service(&state.repo_root)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let snapshot = read_service
        .open_snapshot(&snapshot_id)
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let file = read_service
        .open_file(&snapshot, &query.path)
        .map_err(|_| StatusCode::NOT_FOUND)?;
    build_file_response(&read_service, &file, &headers)
}

async fn branch_file(
    State(state): State<ServeOptions>,
    Path(branch_token): Path<String>,
    Query(query): Query<FileQuery>,
    headers: HeaderMap,
) -> Result<Response<Body>, StatusCode> {
    let facade = e2v_core::RepositoryFacade::new();
    let read_service = facade
        .read_service(&state.repo_root)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let snapshot = read_service
        .resolve_branch(&branch_token)
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let file = read_service
        .open_file(&snapshot, &query.path)
        .map_err(|_| StatusCode::NOT_FOUND)?;
    build_file_response(&read_service, &file, &headers)
}

fn build_file_response(
    read_service: &e2v_core::ReadService,
    file: &e2v_core::FileHandle,
    headers: &HeaderMap,
) -> Result<Response<Body>, StatusCode> {
    let file_size: usize = file
        .file_size()
        .try_into()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if let Some(range_header) = headers.get(axum::http::header::RANGE) {
        let range_header = range_header.to_str().map_err(|_| StatusCode::BAD_REQUEST)?;
        let (start, end) = match parse_single_range(range_header, file_size) {
            Ok(range) => range,
            Err(RangeError::BadRequest) => return Err(StatusCode::BAD_REQUEST),
            Err(RangeError::Unsatisfiable) => {
                return Response::builder()
                    .status(StatusCode::RANGE_NOT_SATISFIABLE)
                    .header(
                        axum::http::header::CONTENT_RANGE,
                        format!("bytes */{file_size}"),
                    )
                    .body(Body::empty())
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR);
            }
        };
        let bytes = read_service
            .read_range(&file, start, end - start + 1)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        return Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header(axum::http::header::ACCEPT_RANGES, "bytes")
            .header(
                axum::http::header::CONTENT_RANGE,
                format!("bytes {start}-{end}/{file_size}"),
            )
            .header(axum::http::header::CONTENT_LENGTH, bytes.len().to_string())
            .body(Body::from(bytes))
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR);
    }

    let bytes = read_service
        .read_range(&file, 0, file_size)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::ACCEPT_RANGES, "bytes")
        .header(axum::http::header::CONTENT_LENGTH, bytes.len().to_string())
        .body(Body::from(bytes))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

fn parse_single_range(header: &str, file_size: usize) -> Result<(usize, usize), RangeError> {
    let bytes = header
        .strip_prefix("bytes=")
        .ok_or(RangeError::BadRequest)?;
    if bytes.contains(',') {
        return Err(RangeError::BadRequest);
    }
    let (start, end) = bytes.split_once('-').ok_or(RangeError::BadRequest)?;
    let start: usize = start.parse().map_err(|_| RangeError::BadRequest)?;
    let end = if end.is_empty() {
        file_size.saturating_sub(1)
    } else {
        end.parse().map_err(|_| RangeError::BadRequest)?
    };
    if start >= file_size || end < start {
        return Err(RangeError::Unsatisfiable);
    }
    Ok((start, end.min(file_size.saturating_sub(1))))
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
