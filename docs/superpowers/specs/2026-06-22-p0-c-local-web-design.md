# P0-C Local Web And HTTP Design

## Goal

Deliver all P0-C access-plane goals on top of the existing P0-A/P0-B repository engine:

- local `axum` server
- snapshot browse
- directory browse
- download
- HTTP Range
- Local HTTP API reused by the Local Web UI

This phase must not duplicate manifest traversal, snapshot resolution, file opening, decryption, or authenticated reads. Those behaviors stay behind `e2v_core::ReadService`.

## Current State

The repository already exposes the internal read primitives needed for P0-C:

- `ReadService::open_snapshot`
- `ReadService::resolve_branch`
- `ReadService::read_dir`
- `ReadService::open_file`
- `ReadService::read_range`

There is currently no HTTP server crate, no `axum` dependency, no web routes, and no local `serve` entrypoint. The access plane therefore stops at library APIs today.

## Design Choice

### Option A: New `e2v-web` workspace crate

Pros:

- cleanest long-term separation for UI/server work
- easiest to scale into richer frontend assets later

Cons:

- larger P0-C surface area
- more workspace churn before any user-facing path works

### Option B: Put web server inside `e2v-core`

Pros:

- quickest path to first endpoint

Cons:

- mixes access plane with core repository domain
- conflicts with the existing architecture boundary in `plan.md`

### Option C: Add local web/http module to `e2v-sync`

Pros:

- smallest change that still keeps access-plane concerns outside `e2v-core`
- reuses the existing `ReadService` and sync-facing crate layout
- keeps room to split into a dedicated crate later if the UI grows

Cons:

- `e2v-sync` becomes the temporary host for both sync and local web access

### Decision

Use **Option C** for P0-C.

`e2v-sync` will gain a local `axum` server module that hosts:

- Local HTTP API
- minimal Local Web UI built directly from server-rendered HTML

## Scope

P0-C will implement a localhost-only, read-only browser for repository contents. It will support:

- listing snapshots
- browsing snapshot directories
- browsing branch directories through branch token resolution
- downloading files
- single-range HTTP byte requests for file downloads and preview

P0-C will not yet implement:

- authentication token flow beyond localhost binding
- rich frontend assets or build tooling
- write operations
- multi-range responses
- thumbnailing, syntax highlighting, or specialized previews

## Architecture

### Module Placement

Add the following module in `crates/e2v-sync`:

- `src/web.rs`

Expose the following types and functions from `crates/e2v-sync/src/lib.rs`:

- `ServeOptions`
- `ServeHandle` or equivalent server binding result
- `serve_local_web(...)`

### Dependencies

Add minimal web dependencies to `crates/e2v-sync/Cargo.toml`:

- `axum`
- `tokio` features needed for networking/macros
- `http`
- `tower` or `tower-http` only if required for a very small helper

Do not add a frontend toolchain.

### Boundary Rule

The web layer may:

- construct `ReadService`
- map request parameters into `ReadService` calls
- translate domain errors into HTTP responses
- render minimal HTML

The web layer may not:

- walk manifests directly
- open object files directly
- decode snapshots or trees itself
- bypass authenticated reads

## HTTP API

All endpoints bind to `127.0.0.1` only.

### Snapshot Endpoints

- `GET /api/snapshots`
  - returns snapshot summaries
- `GET /api/snapshots/:snapshot_id/tree?path=<repo-path>`
  - returns directory entries for a snapshot-relative path
- `GET /api/snapshots/:snapshot_id/file?path=<repo-path>`
  - returns file bytes for full download or `Range` request

### Branch Endpoints

- `GET /api/branches/:branch_token/tree?path=<repo-path>`
  - resolves branch through `ReadService::resolve_branch`
  - returns directory entries
- `GET /api/branches/:branch_token/file?path=<repo-path>`
  - resolves branch then returns file bytes or ranged bytes

### Response Shape

Keep JSON simple and explicit:

```json
{
  "snapshot_id": "....",
  "path": "nested",
  "entries": [
    { "name": "hello.txt", "kind": "file" }
  ]
}
```

For file responses:

- `200 OK` for full download
- `206 Partial Content` for valid single-range request
- `416 Range Not Satisfiable` when the requested range is outside the file

## Local Web UI

The UI is server-rendered HTML with no asset pipeline.

### Pages

- `GET /`
  - landing page with repository path and latest snapshots
- `GET /snapshots/:snapshot_id`
  - renders directory listing for snapshot root or nested path via query parameter
- `GET /branches/:branch_token`
  - renders directory listing for branch root or nested path via query parameter

### Behavior

- directories render as clickable links
- files render as download links
- file downloads use the same API endpoints as direct HTTP clients
- HTML is intentionally plain and functional for P0-C

## Path Handling

All request paths are repository-relative logical paths.

Rules:

- empty path means repository root
- no absolute filesystem paths in HTTP parameters
- invalid path, missing file, or missing snapshot returns client-facing error response
- the server never reveals local object ids or physical object file paths

## Range Handling

P0-C supports single-range requests only.

Accepted form:

- `Range: bytes=start-end`
- `Range: bytes=start-`

Behavior:

- parse and validate the range header in the web layer
- call `ReadService::read_range(file, offset, length)`
- set `Content-Range`, `Content-Length`, and `Accept-Ranges: bytes`
- reject malformed and multi-range requests with a clear HTTP error

Important note:

`ReadService::read_range` currently reconstructs the full file from chunks before slicing the requested bytes. This is acceptable for P0-C correctness, but it does not satisfy the future performance target implied by VFS and large-file preview. That optimization remains follow-up work, not a blocker for P0-C completion.

## Error Handling

Map errors into a small stable HTTP surface:

- `400 Bad Request`
  - malformed query or bad range syntax
- `404 Not Found`
  - missing snapshot, branch, path, or file
- `416 Range Not Satisfiable`
  - invalid byte range
- `500 Internal Server Error`
  - unexpected repository or server failure

HTML pages should render readable error pages instead of raw debug output.

## Testing Strategy

P0-C must be implemented with TDD.

### Red-Green Sequence

1. failing API test for listing snapshots
2. failing API test for browsing a directory by snapshot
3. failing API test for downloading a file
4. failing API test for `206 Partial Content`
5. failing HTML route test for root page and navigation links
6. failing error-path tests for missing snapshot/path and invalid range

### Test Style

Prefer integration tests in `crates/e2v-sync/tests/` that:

- create a temporary repository
- write commits through `RepositoryFacade`
- build an `axum` router from the P0-C server module
- send in-process HTTP requests to the router

This keeps tests fast and avoids browser dependence.

## Deliverables

P0-C is complete when the current workspace proves all of the following:

- `e2v-sync` exposes a localhost-only `axum` server entrypoint
- Local HTTP API routes exist for snapshots, directories, downloads, and range reads
- Local Web UI pages exist for root, snapshot browse, and branch browse
- all browsing and file reads flow through `ReadService`
- single-range HTTP reads return correct `206` behavior
- tests cover success and error cases for the routes above

## Non-Goals For This Phase

- auth token issuance
- JS-heavy UI
- editable web actions
- multi-range streaming
- VFS-grade sub-chunk authenticated range optimization
- public network exposure beyond localhost

## Implementation Notes

- Start with the HTTP API because it is the narrower surface and the Web UI can layer on top.
- Keep HTML generation inline or through tiny string helpers; do not introduce templates unless tests show duplication is getting in the way.
- If a `serve` CLI entrypoint does not yet exist in this workspace, P0-C may first expose a library server API plus tests. The CLI integration can then be wired immediately after, still within this phase, once the HTTP surface is proven.
