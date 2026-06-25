# P1-A Branches And Local Index Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Complete P1-A by adding local branch management plus a SQLite-backed current-branch index that supports metadata search and filename search.

**Architecture:** Keep branch management and search entry points on `e2v_core::RepositoryFacade` so future CLI/Web/SDK surfaces can reuse one stable orchestration layer. Store the active branch in the existing `refs/default.json` control file, add per-branch local ref records under `.e2v/refs/branches/`, extend file manifests with persistent modified-time metadata, and build a rebuildable SQLite cache that tracks the currently checked-out branch head.

**Tech Stack:** Rust workspace crates, `rusqlite` with bundled SQLite/FTS5, existing encrypted control-plane records, existing `ManifestStore` tree walking, `tempfile` integration tests.

---

### Task 1: Add Repository Secrets And File Metadata Needed By The Local Index

**Files:**
- Modify: `crates/e2v-store/src/logical_object_store.rs`
- Modify: `crates/e2v-core/src/keyring.rs`
- Modify: `crates/e2v-core/src/facade.rs`
- Modify: `crates/e2v-core/src/manifest_store.rs`
- Test: `crates/e2v-core/tests/init_repository.rs`

- [ ] **Step 1: Write the failing metadata persistence tests**

Add tests that prove:
- repo secrets survive keyring round-trip with a new `repo_path_index_key`
- committed file manifests persist a positive `modified_unix_ms`

- [ ] **Step 2: Run the focused tests to verify they fail**

Run: `cargo test -p e2v-core repo_path_index_key file_manifest_records_modified_time -- --nocapture`
Expected: FAIL because the secret and manifest field do not exist yet.

- [ ] **Step 3: Write the minimal implementation**

Implement:
- `RepoSecrets.repo_path_index_key`
- keyring seal/unseal support for the new key
- `FileObject.modified_unix_ms` and `ManifestFileObject.modified_unix_ms`
- commit-time capture of file modified time in Unix milliseconds

- [ ] **Step 4: Run the focused tests to verify they pass**

Run: `cargo test -p e2v-core repo_path_index_key file_manifest_records_modified_time -- --nocapture`
Expected: PASS.

### Task 2: Add Local Branch Create/List/Delete/Checkout

**Files:**
- Modify: `crates/e2v-core/src/facade.rs`
- Modify: `crates/e2v-core/src/lib.rs`
- Test: `crates/e2v-core/tests/init_repository.rs`

- [ ] **Step 1: Write the failing branch workflow tests**

Add tests that prove:
- branch creation starts from the current head without copying objects
- branch checkout switches the active branch without rewriting the working tree
- commits after branch checkout advance only that branch
- listing shows all local branches and marks the current one
- deleting the current branch is rejected, but deleting a non-current branch succeeds

- [ ] **Step 2: Run the focused tests to verify they fail**

Run: `cargo test -p e2v-core branch_ --test init_repository -- --nocapture`
Expected: FAIL because the branch APIs and local ref storage do not exist yet.

- [ ] **Step 3: Write the minimal implementation**

Implement:
- local branch ref files under `.e2v/refs/branches/<branch-token>.json`
- facade APIs for `create_branch`, `list_branches`, `delete_branch`, and `checkout_branch`
- current-branch updates through `refs/default.json`
- `ReadService::resolve_branch` lookup for non-current local branches
- commit updates for both the active branch ref file and `default.json`

- [ ] **Step 4: Run the focused tests to verify they pass**

Run: `cargo test -p e2v-core branch_ --test init_repository -- --nocapture`
Expected: PASS.

### Task 3: Add A SQLite Current-Branch Index And Metadata Search

**Files:**
- Modify: `crates/e2v-core/Cargo.toml`
- Create: `crates/e2v-core/src/local_index.rs`
- Modify: `crates/e2v-core/src/lib.rs`
- Modify: `crates/e2v-core/src/facade.rs`
- Test: `crates/e2v-core/tests/init_repository.rs`

- [ ] **Step 1: Write the failing metadata index tests**

Add tests that prove:
- metadata search can filter current visible files by extension
- metadata search can filter by size bounds and path prefix
- search returns the manifest-backed modified time and object linkage

- [ ] **Step 2: Run the focused tests to verify they fail**

Run: `cargo test -p e2v-core metadata_search --test init_repository -- --nocapture`
Expected: FAIL because the SQLite index and metadata search API do not exist yet.

- [ ] **Step 3: Write the minimal implementation**

Implement:
- SQLite database bootstrap with `secure_delete`
- rebuildable current-head file table keyed by normalized path and keyed `path_token`
- metadata cache row storing active branch token and indexed head snapshot id
- `RepositoryFacade::search_metadata(...)` plus automatic index refresh when the active head changes

- [ ] **Step 4: Run the focused tests to verify they pass**

Run: `cargo test -p e2v-core metadata_search --test init_repository -- --nocapture`
Expected: PASS.

### Task 4: Add Filename FTS Search And Head-Switch Refresh Behavior

**Files:**
- Modify: `crates/e2v-core/src/local_index.rs`
- Modify: `crates/e2v-core/src/facade.rs`
- Test: `crates/e2v-core/tests/init_repository.rs`

- [ ] **Step 1: Write the failing filename-search tests**

Add tests that prove:
- filename search finds visible files through FTS
- search results update after a new commit on the active branch
- search results switch to the checked-out branch view after `checkout_branch`

- [ ] **Step 2: Run the focused tests to verify they fail**

Run: `cargo test -p e2v-core filename_search --test init_repository -- --nocapture`
Expected: FAIL because no filename FTS surface exists and branch-head refresh is not implemented.

- [ ] **Step 3: Write the minimal implementation**

Implement:
- FTS5-backed filename search table
- branch-head aware rebuild-on-demand logic
- small query normalization for user text

- [ ] **Step 4: Run the focused tests to verify they pass**

Run: `cargo test -p e2v-core filename_search --test init_repository -- --nocapture`
Expected: PASS.

### Task 5: Verify End-To-End Behavior And Apply Small Optimizations

**Files:**
- Modify: `crates/e2v-core/src/local_index.rs`
- Modify: `crates/e2v-core/src/facade.rs`
- Test: `crates/e2v-core/tests/init_repository.rs`

- [ ] **Step 1: Add the final regression tests**

Add tests that prove:
- empty-head repositories return empty search results without errors
- rebuilding the index is idempotent
- branch list remains correct even when only `default.json` exists for the current branch

- [ ] **Step 2: Run the focused tests to verify they fail**

Run: `cargo test -p e2v-core local_index_ --test init_repository -- --nocapture`
Expected: FAIL until the edge-case handling is finished.

- [ ] **Step 3: Write the minimal implementation and cleanup**

Implement:
- graceful empty-head handling
- explicit validation that the current branch ref exists under `refs/branches/`
- index transaction cleanup and deterministic ordering

- [ ] **Step 4: Run the full relevant verification**

Run: `cargo test -p e2v-core --test init_repository -- --nocapture`
Expected: PASS.

Run: `cargo test --workspace -- --nocapture`
Expected: PASS with no regressions in `e2v-sync` or `e2v-store`.

## Self-Review

### Spec Coverage

- `branch create/list/delete`: covered in Task 2
- `checkout branch`: covered in Task 2
- `SQLite index`: covered in Tasks 3 and 4
- `metadata search`: covered in Task 3
- `filename search`: covered in Task 4
- local index cache, not fact source: enforced by rebuild-on-demand and current-head metadata in Tasks 3 through 5

### Placeholder Scan

- no deferred `TODO`/`TBD` items
- each task has a failing test, red run, minimal implementation, and green run
- verification commands are concrete

### Type Consistency

- branch APIs stay on `RepositoryFacade`
- SQLite implementation lives in `crates/e2v-core/src/local_index.rs`
- tests remain in `crates/e2v-core/tests/init_repository.rs`
