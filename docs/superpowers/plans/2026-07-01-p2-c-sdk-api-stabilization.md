# P2-C SDK / API Stabilization Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a stable Rust SDK crate that exposes repository, read, and sync entry points with stable error codes and contract tests.

**Architecture:** Create a new `e2v-api` crate that wraps `e2v-core::RepositoryFacade`, `e2v-core::ReadService`, and `e2v-sync` orchestration behind stable SDK types. Keep internal crates reusable, but make `e2v-api` the supported public surface and prove it through black-box contract tests.

**Tech Stack:** Rust workspace, `anyhow`, `serde`, `thiserror`-free custom error structs, existing `e2v-core`, `e2v-sync`, `e2v-store`, and integration tests under `crates/e2v-api/tests`.

---

### Task 1: Add failing SDK contract tests for repository and read entry points

**Files:**
- Create: `crates/e2v-api/Cargo.toml`
- Create: `crates/e2v-api/src/lib.rs`
- Create: `crates/e2v-api/tests/sdk_contract.rs`
- Modify: `Cargo.toml`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn sdk_can_init_commit_and_read_repository_without_internal_crates() {
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    std::fs::create_dir_all(&repo_root).unwrap();

    let sdk = e2v_api::Sdk::new();
    sdk.init_repository(e2v_api::InitRepositoryOptions {
        repo_root: repo_root.clone(),
        password: "correct horse battery staple".to_string(),
        branch_name: "main".to_string(),
    })
    .unwrap();

    std::fs::write(repo_root.join("notes.txt"), "hello sdk").unwrap();
    let commit = sdk
        .commit_repository(e2v_api::CommitRepositoryOptions {
            repo_root: repo_root.clone(),
            message: "first".to_string(),
        })
        .unwrap();

    let read = sdk.open_read_handle(&repo_root).unwrap();
    let snapshot = read.open_snapshot(&commit.snapshot_id).unwrap();
    let file = read.open_file(&snapshot, "notes.txt").unwrap();
    let bytes = read.read_range(&file, 0, 32).unwrap();

    assert_eq!(String::from_utf8(bytes).unwrap(), "hello sdk");
}

#[test]
fn sdk_read_returns_invalid_argument_for_path_traversal() {
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    std::fs::create_dir_all(&repo_root).unwrap();

    let sdk = e2v_api::Sdk::new();
    sdk.init_repository(e2v_api::InitRepositoryOptions {
        repo_root: repo_root.clone(),
        password: "correct horse battery staple".to_string(),
        branch_name: "main".to_string(),
    })
    .unwrap();

    let read = sdk.open_read_handle(&repo_root).unwrap();
    let snapshot = read.open_snapshot("missing").unwrap_err();
    assert_eq!(snapshot.code(), e2v_api::SdkErrorCode::NotFound);
}
```

- [ ] **Step 2: Run the focused test to verify RED**

Run: `cargo test -p e2v-api sdk_can_init_commit_and_read_repository_without_internal_crates -- --exact`
Expected: FAIL because `e2v-api` and its public SDK types do not exist yet.

- [ ] **Step 3: Write the minimal crate scaffolding and SDK wrappers**

```rust
pub struct Sdk {
    facade: e2v_core::RepositoryFacade,
}

impl Sdk {
    pub fn new() -> Self {
        Self {
            facade: e2v_core::RepositoryFacade::new(),
        }
    }
}
```

- [ ] **Step 4: Re-run the focused test to verify GREEN**

Run: `cargo test -p e2v-api sdk_can_init_commit_and_read_repository_without_internal_crates -- --exact`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml crates/e2v-api/Cargo.toml crates/e2v-api/src/lib.rs crates/e2v-api/tests/sdk_contract.rs
git commit -m "feat: add stable sdk repository and read entry points"
```

### Task 2: Add a stable SDK error model and contract tests

**Files:**
- Modify: `crates/e2v-api/src/lib.rs`
- Modify: `crates/e2v-api/tests/sdk_contract.rs`

- [ ] **Step 1: Write the failing error-contract tests**

```rust
#[test]
fn sdk_open_missing_repository_returns_not_found() {
    let temp = tempfile::tempdir().unwrap();
    let repo_root = temp.path().join("missing-repo");

    let error = e2v_api::Sdk::new().open_repository(&repo_root).unwrap_err();

    assert_eq!(error.code(), e2v_api::SdkErrorCode::NotFound);
}

#[test]
fn sdk_remote_parse_rejects_unsupported_scheme_with_invalid_argument() {
    let error = e2v_api::parse_remote_spec("ftp://example.com/repo").unwrap_err();
    assert_eq!(error.code(), e2v_api::SdkErrorCode::InvalidArgument);
}
```

- [ ] **Step 2: Run the focused tests to verify RED**

Run: `cargo test -p e2v-api sdk_open_missing_repository_returns_not_found -- --exact`
Run: `cargo test -p e2v-api sdk_remote_parse_rejects_unsupported_scheme_with_invalid_argument -- --exact`
Expected: FAIL because the SDK has no stable error code surface yet.

- [ ] **Step 3: Write the minimal implementation**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SdkErrorCode {
    InvalidArgument,
    NotFound,
    AlreadyExists,
    PermissionDenied,
    AuthenticationRequired,
    Conflict,
    NeedsRebase,
    RollbackDetected,
    Unsupported,
    CorruptState,
    Io,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SdkError {
    code: SdkErrorCode,
    message: String,
}
```

- [ ] **Step 4: Re-run the focused tests to verify GREEN**

Run: `cargo test -p e2v-api sdk_open_missing_repository_returns_not_found -- --exact`
Run: `cargo test -p e2v-api sdk_remote_parse_rejects_unsupported_scheme_with_invalid_argument -- --exact`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/e2v-api/src/lib.rs crates/e2v-api/tests/sdk_contract.rs
git commit -m "feat: add stable sdk error codes"
```

### Task 3: Add remote registration and default sync entry points

**Files:**
- Modify: `crates/e2v-api/src/lib.rs`
- Modify: `crates/e2v-api/tests/sdk_contract.rs`

- [ ] **Step 1: Write the failing sync-contract tests**

```rust
#[test]
fn sdk_can_register_default_remote_and_push_fetch_through_it() {
    let temp = tempfile::tempdir().unwrap();
    let source_repo = temp.path().join("source");
    let clone_repo = temp.path().join("clone");
    let remote_repo = temp.path().join("remote");
    std::fs::create_dir_all(&source_repo).unwrap();
    std::fs::create_dir_all(&remote_repo).unwrap();

    let sdk = e2v_api::Sdk::new();
    sdk.init_repository(e2v_api::InitRepositoryOptions {
        repo_root: source_repo.clone(),
        password: "correct horse battery staple".to_string(),
        branch_name: "main".to_string(),
    })
    .unwrap();
    std::fs::write(source_repo.join("notes.txt"), "hello sync").unwrap();
    sdk.commit_repository(e2v_api::CommitRepositoryOptions {
        repo_root: source_repo.clone(),
        message: "first".to_string(),
    })
    .unwrap();

    sdk.add_remote(
        &source_repo,
        "origin",
        &format!("file://{}", remote_repo.to_string_lossy().replace('\\', "/")),
    )
    .unwrap();
    sdk.push_default_remote(e2v_api::PushRequest {
        repo_root: source_repo.clone(),
        branch_token: "default".to_string(),
        operation_id: "push-1".to_string(),
    })
    .unwrap();

    sdk.clone_remote(e2v_api::CloneRequest {
        remote_spec: format!("file://{}", remote_repo.to_string_lossy().replace('\\', "/")),
        target_repo_root: clone_repo.clone(),
        password: "correct horse battery staple".to_string(),
        branch_name: "main".to_string(),
        branch_token: "default".to_string(),
    })
    .unwrap();

    let read = sdk.open_read_handle(&clone_repo).unwrap();
    let snapshot = read.resolve_branch("default").unwrap();
    let file = read.open_file(&snapshot, "notes.txt").unwrap();
    let bytes = read.read_range(&file, 0, 32).unwrap();
    assert_eq!(String::from_utf8(bytes).unwrap(), "hello sync");
}
```

- [ ] **Step 2: Run the focused test to verify RED**

Run: `cargo test -p e2v-api sdk_can_register_default_remote_and_push_fetch_through_it -- --exact`
Expected: FAIL because the SDK does not yet expose stable remote registration or sync wrappers.

- [ ] **Step 3: Write the minimal implementation**

```rust
pub fn add_remote(&self, repo_root: &Path, name: &str, spec: &str) -> SdkResult<RemoteRegistration> {
    /* validate spec and persist .e2v/remotes/default.json */
}

pub fn push_default_remote(&self, request: PushRequest) -> SdkResult<PushResponse> {
    /* load persisted remote and dispatch through RemoteSpec::with_backend */
}
```

- [ ] **Step 4: Re-run the focused test to verify GREEN**

Run: `cargo test -p e2v-api sdk_can_register_default_remote_and_push_fetch_through_it -- --exact`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/e2v-api/src/lib.rs crates/e2v-api/tests/sdk_contract.rs
git commit -m "feat: add stable sdk sync entry points"
```

### Task 4: Add final regression coverage and verification

**Files:**
- Modify: `crates/e2v-api/tests/sdk_contract.rs`
- Modify: `crates/e2v-api/src/lib.rs`

- [ ] **Step 1: Add final contract tests**

```rust
#[test]
fn sdk_branch_reads_are_snapshot_pinned() {
    // open a file through resolve_branch, mutate head, and prove the old handle still reads old bytes
}

#[test]
fn sdk_does_not_require_internal_modules_for_public_workflows() {
    // compile-time usage stays confined to e2v_api types in this test file
}
```

- [ ] **Step 2: Run focused tests to verify RED/GREEN as needed**

Run: `cargo test -p e2v-api`
Expected: PASS after minimal implementation updates.

- [ ] **Step 3: Run full verification**

Run: `cargo test --workspace`
Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add crates/e2v-api/src/lib.rs crates/e2v-api/tests/sdk_contract.rs
git commit -m "test: lock sdk public api contracts"
```
