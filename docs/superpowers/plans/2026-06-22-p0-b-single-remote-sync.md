# P0-B Single Remote Sync Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the P0-B single-remote sync layer on top of the existing P0-A repository engine, including remote backend capability boundaries, transaction publishing, push/fetch/clone flows, operation journal recovery, and upload resume.

**Architecture:** Keep `e2v-core` focused on repository, snapshot, and working-tree behavior. Add P0-B storage and sync boundaries in `e2v-store` and a new `e2v-sync` crate so remote publication, ref/layout CAS, write intents, and recovery do not leak into CLI-facing core APIs. Start with a testable in-memory/S3-compatible style backend and append-only operation journal, then extend into end-to-end push/fetch/clone flows.

**Tech Stack:** Rust workspace crates, `serde`, `serde_json`, `postcard`, `blake3`, existing encrypted object store, append-only WAL journal files, test-first integration tests with `tempfile`.

---

### Task 1: Add Remote Capability And Store Boundaries

**Files:**
- Create: `crates/e2v-store/src/capability.rs`
- Create: `crates/e2v-store/src/ref_store.rs`
- Create: `crates/e2v-store/src/layout_root_store.rs`
- Create: `crates/e2v-store/src/memory_backend.rs`
- Modify: `crates/e2v-store/src/lib.rs`
- Test: `crates/e2v-store/src/memory_backend.rs`

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn compare_and_swap_ref_rejects_stale_version() {
    let backend = MemoryBackend::new();
    let token = RefToken::new("branch-token".to_string());
    let first = EncryptedRef::new(vec![1, 2, 3]);
    let second = EncryptedRef::new(vec![4, 5, 6]);

    let initial = backend
        .compare_and_swap_ref(&token, None, first.clone())
        .unwrap();
    assert!(initial.applied);

    let stale = backend
        .compare_and_swap_ref(&token, None, second)
        .unwrap();
    assert!(!stale.applied);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p e2v-store compare_and_swap_ref_rejects_stale_version -- --nocapture`
Expected: FAIL because `MemoryBackend`, `RefToken`, `EncryptedRef`, or `compare_and_swap_ref` do not exist yet.

- [ ] **Step 3: Write minimal implementation**

```rust
pub struct BackendCapability {
    pub supports_conditional_put: bool,
    pub supports_range_read: bool,
    pub supports_atomic_rename: bool,
    pub supports_paged_list: bool,
    pub consistency_class: ConsistencyClass,
    pub supports_remote_lock_or_lease: bool,
    pub supports_transaction_markers: bool,
    pub supports_reliable_remote_time: bool,
    pub supports_object_generation_or_etag: bool,
    pub supports_layout_root_cas: bool,
    pub supports_oblivious_access_schedule: bool,
}

pub trait RefStore {
    fn read_ref(&self, token: &RefToken) -> Result<Option<StoredRef>>;
    fn compare_and_swap_ref(
        &self,
        token: &RefToken,
        expected: Option<RefVersion>,
        next: EncryptedRef,
    ) -> Result<CasResult>;
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p e2v-store memory_backend -- --nocapture`
Expected: PASS with the new CAS/layout-root capability tests green.

- [ ] **Step 5: Commit**

```bash
git add crates/e2v-store/src/capability.rs crates/e2v-store/src/ref_store.rs crates/e2v-store/src/layout_root_store.rs crates/e2v-store/src/memory_backend.rs crates/e2v-store/src/lib.rs
git commit -m "feat: add remote store capability boundaries"
```

### Task 2: Add Operation Journal And Transaction Publisher Skeleton

**Files:**
- Create: `crates/e2v-sync/Cargo.toml`
- Create: `crates/e2v-sync/src/lib.rs`
- Create: `crates/e2v-sync/src/journal.rs`
- Create: `crates/e2v-sync/src/transaction.rs`
- Create: `crates/e2v-sync/src/publisher.rs`
- Modify: `Cargo.toml`
- Test: `crates/e2v-sync/src/journal.rs`
- Test: `crates/e2v-sync/src/publisher.rs`

- [ ] **Step 1: Write the failing journal test**

```rust
#[test]
fn journal_replays_pending_uploaded_objects_in_order() {
    let temp = tempfile::tempdir().unwrap();
    let journal = OperationJournal::new(temp.path()).unwrap();
    let operation_id = OperationId::new("op-1".to_string());

    journal.begin_operation(&operation_id, OperationMetadata::push()).unwrap();
    journal.plan_object(&operation_id, "chunk-1", "chunk").unwrap();
    journal.record_uploaded(&operation_id, "chunk-1", "chunk").unwrap();

    let replay = journal.pending_objects(&operation_id).unwrap();
    assert_eq!(replay.len(), 1);
    assert_eq!(replay[0].state, ObjectUploadState::Uploaded);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p e2v-sync journal_replays_pending_uploaded_objects_in_order -- --nocapture`
Expected: FAIL because `e2v-sync` and `OperationJournal` do not exist yet.

- [ ] **Step 3: Write minimal implementation**

```rust
pub trait TransactionPublisher {
    fn begin(&self, plan: PublishPlan) -> Result<PublishSession>;
    fn record_uploaded(&self, session: &PublishSession, object: PublishedObject) -> Result<()>;
    fn publish_layout_if_needed(&self, session: &PublishSession) -> Result<LayoutRootVersion>;
    fn pre_commit_verify(&self, session: &PublishSession) -> Result<()>;
    fn publish_ref(&self, session: &PublishSession, next: EncryptedRef) -> Result<CasResult>;
    fn complete(&self, session: PublishSession) -> Result<()>;
    fn recover(&self, operation_id: &OperationId) -> Result<RecoveryAction>;
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p e2v-sync -- --nocapture`
Expected: PASS with WAL journal replay and publisher mode-selection tests green.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml crates/e2v-sync/Cargo.toml crates/e2v-sync/src/lib.rs crates/e2v-sync/src/journal.rs crates/e2v-sync/src/transaction.rs crates/e2v-sync/src/publisher.rs
git commit -m "feat: add sync journal and publisher skeleton"
```

### Task 3: Implement Push Happy Path With Resume Hooks

**Files:**
- Create: `crates/e2v-sync/src/push.rs`
- Modify: `crates/e2v-core/src/lib.rs`
- Modify: `crates/e2v-core/src/manifest_store.rs`
- Test: `crates/e2v-sync/tests/push_remote.rs`

- [ ] **Step 1: Write the failing push test**

```rust
#[test]
fn push_uploads_reachable_objects_and_publishes_remote_ref() {
    let harness = PushHarness::new().unwrap();
    let commit = harness.commit_file("hello.txt", b"hello remote");

    let result = harness.push_head().unwrap();

    assert_eq!(result.published_snapshot_id, commit.snapshot_id);
    assert!(harness.remote_has_ref());
    assert!(harness.remote_has_layout_root());
    assert!(harness.remote_object_count() > 0);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p e2v-sync push_uploads_reachable_objects_and_publishes_remote_ref -- --nocapture`
Expected: FAIL because the sync engine and remote publish flow are not implemented yet.

- [ ] **Step 3: Write minimal implementation**

```rust
pub fn push_head(
    repository: &RepositoryFacade,
    remote: &dyn RemoteBackend,
    options: PushOptions,
) -> Result<PushResult> {
    let plan = PublishPlan::from_local_head(repository, &options.repo_root, &options.branch_token)?;
    let session = remote.publisher().begin(plan)?;
    upload_missing_objects(repository, remote, &session)?;
    remote.publisher().publish_layout_if_needed(&session)?;
    remote.publisher().pre_commit_verify(&session)?;
    let result = publish_head_ref(repository, remote, &session)?;
    remote.publisher().complete(session)?;
    Ok(result)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p e2v-sync push_ -- --nocapture`
Expected: PASS for push happy-path tests with journal entries and remote ref publication verified.

- [ ] **Step 5: Commit**

```bash
git add crates/e2v-sync/src/push.rs crates/e2v-sync/tests/push_remote.rs crates/e2v-core/src/lib.rs crates/e2v-core/src/manifest_store.rs
git commit -m "feat: add remote push publication flow"
```

### Task 4: Implement Fetch And Clone Foundations

**Files:**
- Create: `crates/e2v-sync/src/fetch.rs`
- Create: `crates/e2v-sync/src/clone.rs`
- Test: `crates/e2v-sync/tests/fetch_clone.rs`

- [ ] **Step 1: Write the failing fetch/clone tests**

```rust
#[test]
fn fetch_downloads_remote_ref_and_missing_objects_without_touching_worktree() {
    let harness = SyncHarness::seed_remote().unwrap();
    let result = harness.fetch().unwrap();
    assert!(result.downloaded_objects > 0);
    assert!(!harness.working_tree_changed());
}

#[test]
fn clone_bootstraps_local_repository_from_remote_head() {
    let harness = SyncHarness::seed_remote().unwrap();
    let clone = harness.clone_into_fresh_repo().unwrap();
    assert!(clone.head_snapshot_id.is_some());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p e2v-sync fetch_ clone_ -- --nocapture`
Expected: FAIL because fetch/clone are not implemented yet.

- [ ] **Step 3: Write minimal implementation**

```rust
pub fn fetch(remote: &dyn RemoteBackend, local: &LocalSyncTarget) -> Result<FetchResult> {
    let remote_ref = remote.read_default_ref()?;
    let remote_layout = remote.read_layout_root()?;
    copy_missing_objects(remote, local, &remote_ref, &remote_layout)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p e2v-sync fetch_clone -- --nocapture`
Expected: PASS for fetch and clone happy-path tests.

- [ ] **Step 5: Commit**

```bash
git add crates/e2v-sync/src/fetch.rs crates/e2v-sync/src/clone.rs crates/e2v-sync/tests/fetch_clone.rs
git commit -m "feat: add fetch and clone foundations"
```

### Task 5: Add Resume, Recovery, And Conflict Handling

**Files:**
- Modify: `crates/e2v-sync/src/journal.rs`
- Modify: `crates/e2v-sync/src/publisher.rs`
- Modify: `crates/e2v-sync/src/push.rs`
- Test: `crates/e2v-sync/tests/push_remote.rs`

- [ ] **Step 1: Write the failing recovery tests**

```rust
#[test]
fn resume_skips_verified_objects_and_republishes_missing_ref() {
    let harness = PushHarness::with_interrupted_upload().unwrap();
    let resumed = harness.resume().unwrap();
    assert!(resumed.skipped_uploaded_objects > 0);
    assert!(harness.remote_has_ref());
}

#[test]
fn stale_remote_head_marks_push_needs_rebase() {
    let harness = PushHarness::with_remote_conflict().unwrap();
    let error = harness.push_head().unwrap_err();
    assert!(error.to_string().contains("needs-rebase"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p e2v-sync resume_ needs_rebase -- --nocapture`
Expected: FAIL because recovery and conflict handling are not implemented yet.

- [ ] **Step 3: Write minimal implementation**

```rust
if !remote_head_matches_expected {
    journal.mark_needs_rebase(&session.operation_id)?;
    anyhow::bail!("push requires needs-rebase recovery");
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p e2v-sync push_remote -- --nocapture`
Expected: PASS for interrupted-upload resume and stale-head conflict tests.

- [ ] **Step 5: Commit**

```bash
git add crates/e2v-sync/src/journal.rs crates/e2v-sync/src/publisher.rs crates/e2v-sync/src/push.rs crates/e2v-sync/tests/push_remote.rs
git commit -m "feat: add upload resume and recovery handling"
```
