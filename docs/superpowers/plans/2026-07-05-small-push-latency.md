# Small-Push Latency Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make small multi-object pushes on high-latency remotes use the existing pack path based on the current missing upload set instead of the total repository size alone.

**Architecture:** Add a shared adaptive packing decision helper in `crates/e2v-sync/src/push.rs`, keep the existing `100000` large-repository threshold as a compatibility guard, and wire the new helper through fresh push plus both resume branches. Land the change with TDD-first unit and integration coverage, then verify it against the live AList diagnostic path.

**Tech Stack:** Rust workspace, `cargo test`, integration tests in `crates/e2v-sync/tests`, sync logic in `crates/e2v-sync/src/push.rs`, live diagnostics through `e2v-cli diagnose`

## Global Constraints

- Preserve the existing publish safety model: do not change transaction markers, journal semantics, layout-root publication, ref publication, large-object handling, or ORAM segment-only upload behavior.
- Keep the existing large-repository small-object pack threshold behavior as a compatibility path.
- The new fast path must depend on the current missing upload set and default to packing when at least `2` pack-eligible missing objects exist.
- `1` pack-eligible missing object must remain loose by default.
- Any new threshold override introduced for tests must stay under `#[doc(hidden)] pub mod testing` and must not become a public runtime tuning surface.
- Do not commit credentials or remote tokens into the repository; use environment variables for the live AList verification command.
- Use TDD for each behavior change: write the failing test first, run it to confirm failure, implement the minimal fix, then re-run the focused test.

---

### Task 1: Add the adaptive packing decision helper and lock its behavior with unit tests

**Files:**
- Modify: `crates/e2v-sync/src/push.rs`
- Modify: `crates/e2v-sync/src/lib.rs`

**Interfaces:**
- Consumes: `local_object_path(repo_root: &Path, object_id: &str) -> PathBuf`
- Produces: `fn should_pack_current_upload_set(repo_root: &Path, missing_object_ids: &[String], total_object_count_hint: usize) -> Result<bool>`
- Produces: `pub fn small_push_pack_threshold() -> usize`
- Produces: `pub(crate) fn override_small_push_pack_threshold_for_test(threshold: usize) -> SmallPushPackThresholdGuard`

- [ ] **Step 1: Write the failing unit tests in `crates/e2v-sync/src/push.rs`**

```rust
#[test]
fn current_upload_set_enables_packing_for_two_small_missing_objects() {
    let _large_guard = override_small_object_pack_threshold_for_test(usize::MAX);
    let _small_guard = override_small_push_pack_threshold_for_test(2);
    let temp = tempdir().unwrap();
    let repo_root = temp.path();
    let objects_dir = repo_root.join(".e2v").join("objects");
    std::fs::create_dir_all(&objects_dir).unwrap();

    let first = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
    let second = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string();
    std::fs::write(local_object_path(repo_root, &first), vec![1u8; 32]).unwrap();
    std::fs::write(local_object_path(repo_root, &second), vec![2u8; 32]).unwrap();

    assert!(should_pack_current_upload_set(repo_root, &[first, second], 2).unwrap());
}

#[test]
fn current_upload_set_keeps_single_small_missing_object_loose_by_default() {
    let _large_guard = override_small_object_pack_threshold_for_test(usize::MAX);
    let _small_guard = override_small_push_pack_threshold_for_test(2);
    let temp = tempdir().unwrap();
    let repo_root = temp.path();
    let objects_dir = repo_root.join(".e2v").join("objects");
    std::fs::create_dir_all(&objects_dir).unwrap();

    let object_id =
        "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc".to_string();
    std::fs::write(local_object_path(repo_root, &object_id), vec![3u8; 32]).unwrap();

    assert!(!should_pack_current_upload_set(repo_root, &[object_id], 1).unwrap());
}

#[test]
fn large_repository_threshold_still_enables_packing_with_one_missing_object() {
    let _large_guard = override_small_object_pack_threshold_for_test(3);
    let _small_guard = override_small_push_pack_threshold_for_test(usize::MAX);
    let temp = tempdir().unwrap();
    let repo_root = temp.path();
    let objects_dir = repo_root.join(".e2v").join("objects");
    std::fs::create_dir_all(&objects_dir).unwrap();

    let object_id =
        "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd".to_string();
    std::fs::write(local_object_path(repo_root, &object_id), vec![4u8; 32]).unwrap();

    assert!(should_pack_current_upload_set(repo_root, &[object_id], 3).unwrap());
}
```

- [ ] **Step 2: Run the focused unit tests and confirm failure because the helper does not exist yet**

Run:

```powershell
cargo test -p e2v-sync --lib current_upload_set_enables_packing_for_two_small_missing_objects
```

Expected: FAIL with a compile error for `should_pack_current_upload_set` and the small-push threshold override symbols being undefined.

- [ ] **Step 3: Implement the minimal helper and test seam in `crates/e2v-sync/src/push.rs` and `crates/e2v-sync/src/lib.rs`**

```rust
const DEFAULT_SMALL_PUSH_PACK_THRESHOLD: usize = 2;
thread_local! {
    static SMALL_PUSH_PACK_THRESHOLD_OVERRIDE: Cell<Option<usize>> = const { Cell::new(None) };
}

pub fn small_push_pack_threshold() -> usize {
    SMALL_PUSH_PACK_THRESHOLD_OVERRIDE.with(|override_cell| {
        override_cell
            .get()
            .unwrap_or(DEFAULT_SMALL_PUSH_PACK_THRESHOLD)
    })
}

pub(crate) fn override_small_push_pack_threshold_for_test(
    threshold: usize,
) -> SmallPushPackThresholdGuard {
    let previous = SMALL_PUSH_PACK_THRESHOLD_OVERRIDE.with(|override_cell| {
        let previous = override_cell.get();
        override_cell.set(Some(threshold));
        previous
    });
    SmallPushPackThresholdGuard { previous }
}

pub struct SmallPushPackThresholdGuard {
    previous: Option<usize>,
}

impl Drop for SmallPushPackThresholdGuard {
    fn drop(&mut self) {
        SMALL_PUSH_PACK_THRESHOLD_OVERRIDE.with(|override_cell| {
            override_cell.set(self.previous);
        });
    }
}

fn should_pack_current_upload_set(
    repo_root: &Path,
    missing_object_ids: &[String],
    total_object_count_hint: usize,
) -> Result<bool> {
    if should_pack_small_objects(total_object_count_hint) {
        return Ok(true);
    }

    let mut pack_eligible_count = 0usize;
    for object_id in missing_object_ids {
        validate_object_id_value(object_id)?;
        let bytes = std::fs::read(local_object_path(repo_root, object_id))?;
        if bytes.len() <= SMALL_OBJECT_MAX_BYTES {
            pack_eligible_count += 1;
            if pack_eligible_count >= small_push_pack_threshold() {
                return Ok(true);
            }
        }
    }

    Ok(false)
}
```

And export the test seam:

```rust
pub fn override_small_push_pack_threshold_for_test(
    threshold: usize,
) -> crate::push::SmallPushPackThresholdGuard {
    crate::push::override_small_push_pack_threshold_for_test(threshold)
}
```

- [ ] **Step 4: Re-run the focused unit tests and confirm pass**

Run:

```powershell
cargo test -p e2v-sync --lib current_upload_set_
```

Expected: PASS for the three new decision tests.

- [ ] **Step 5: Commit the helper-only slice**

```powershell
git add crates/e2v-sync/src/push.rs crates/e2v-sync/src/lib.rs
git commit -m "perf: add adaptive small-push pack decision"
```

### Task 2: Use the adaptive helper in fresh push and journal-driven resume

**Files:**
- Modify: `crates/e2v-sync/tests/push_remote.rs`
- Modify: `crates/e2v-sync/src/push.rs`

**Interfaces:**
- Consumes: `should_pack_current_upload_set(repo_root: &Path, missing_object_ids: &[String], total_object_count_hint: usize) -> Result<bool>`
- Produces: fresh push and journal-driven resume paths both call the helper instead of directly comparing totals to `small_object_pack_threshold()`

- [ ] **Step 1: Add failing integration tests for fresh push and journal-driven resume**

```rust
#[test]
fn push_uses_pack_uploads_for_small_repository_when_multiple_small_missing_objects_exist() {
    let _large_guard = e2v_sync::testing::override_small_object_pack_threshold_for_test(usize::MAX);
    let _small_guard = e2v_sync::testing::override_small_push_pack_threshold_for_test(2);
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    let state = facade
        .init(InitOptions {
            repo_root: repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
    fs::write(repo_root.join("hello.txt"), b"hello remote").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "adaptive-pack-small-push".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    let pushed = push_head(
        &facade,
        &remote,
        PushOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "adaptive-pack-small-push-op".to_string(),
        },
    )
    .unwrap();

    assert!(pushed.uploaded_objects > 0);
    assert!(!remote.list_physical("packs/index/").unwrap().is_empty());
    assert!(!remote.list_physical("packs/data/").unwrap().is_empty());
}

#[test]
fn resume_uses_pack_uploads_for_small_repository_when_journal_missing_set_contains_multiple_small_objects() {
    let _large_guard = e2v_sync::testing::override_small_object_pack_threshold_for_test(usize::MAX);
    let _small_guard = e2v_sync::testing::override_small_push_pack_threshold_for_test(2);
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    let state = facade
        .init(InitOptions {
            repo_root: repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
    fs::write(repo_root.join("hello.txt"), b"hello remote").unwrap();
    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "adaptive-pack-resume".to_string(),
        })
        .unwrap();

    let manifest_store = ManifestStore::new(&repo_root);
    let reachable_object_ids = manifest_store
        .collect_reachable_object_ids(&commit.snapshot_id)
        .unwrap();
    let journal =
        e2v_sync::OperationJournal::new(repo_root.join(".e2v").join("journal").join("sync"))
            .unwrap();
    let operation_id = e2v_sync::OperationId::new("adaptive-pack-resume-op".to_string()).unwrap();
    journal
        .begin_operation(
            &operation_id,
            e2v_sync::OperationMetadata::push(state.branch.token_hex.clone(), None),
        )
        .unwrap();
    for object_id in &reachable_object_ids {
        journal.plan_object(&operation_id, object_id, "object").unwrap();
    }

    let remote = MemoryBackend::new();
    let resumed = resume_push(
        &facade,
        &remote,
        ResumeOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: operation_id.value.clone(),
        },
    )
    .unwrap();

    assert_eq!(resumed.published_snapshot_id, commit.snapshot_id);
    assert!(!remote.list_physical("packs/index/").unwrap().is_empty());
    assert!(!remote.list_physical("packs/data/").unwrap().is_empty());
}
```

- [ ] **Step 2: Run the focused integration tests and confirm failure because fresh push and resume still use total-count heuristics**

Run:

```powershell
cargo test -p e2v-sync --test push_remote push_uses_pack_uploads_for_small_repository_when_multiple_small_missing_objects_exist
cargo test -p e2v-sync --test push_remote resume_uses_pack_uploads_for_small_repository_when_journal_missing_set_contains_multiple_small_objects
```

Expected: FAIL because the current code keeps `pack_enabled` false when the large threshold is overridden to `usize::MAX`.

- [ ] **Step 3: Replace the direct `should_pack_small_objects(...)` calls in fresh push and journal-driven resume**

```rust
let pack_enabled = should_pack_current_upload_set(
    &options.repo_root,
    &missing_object_ids,
    reachable_object_ids.len(),
)?;
```

and:

```rust
let pack_enabled = should_pack_current_upload_set(
    &options.repo_root,
    &missing_object_ids,
    total_tracked_objects,
)?;
```

The implementation detail to preserve is:

- compute `missing_object_ids` first
- pass the current missing upload set into the helper
- leave `segment_only_uploads` logic untouched
- leave `upload_objects_with_policy(...)` and `upload_objects_as_pack_segments(...)` signatures unchanged unless a focused refactor is required for compilation

- [ ] **Step 4: Re-run the focused integration tests and confirm pass**

Run:

```powershell
cargo test -p e2v-sync --test push_remote adaptive-pack
```

Expected: PASS for the two new tests, with pack index and pack data present on the remote.

- [ ] **Step 5: Commit the fresh-push and journal-resume wiring**

```powershell
git add crates/e2v-sync/src/push.rs crates/e2v-sync/tests/push_remote.rs
git commit -m "perf: pack small multi-object push uploads"
```

### Task 3: Align the no-journal resume fallback with the same heuristic

**Files:**
- Modify: `crates/e2v-sync/tests/push_remote.rs`
- Modify: `crates/e2v-sync/src/push.rs`

**Interfaces:**
- Consumes: `should_pack_current_upload_set(repo_root: &Path, missing_object_ids: &[String], total_object_count_hint: usize) -> Result<bool>`
- Produces: the no-journal resume fallback branch uses the same helper as fresh push and journal-driven resume

- [ ] **Step 1: Add a failing fallback-resume regression test**

```rust
#[test]
fn resume_without_journal_uses_pack_uploads_for_small_repository_when_multiple_missing_objects_exist() {
    let _large_guard = e2v_sync::testing::override_small_object_pack_threshold_for_test(usize::MAX);
    let _small_guard = e2v_sync::testing::override_small_push_pack_threshold_for_test(2);
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    let state = facade
        .init(InitOptions {
            repo_root: repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();
    fs::write(repo_root.join("hello.txt"), b"hello remote").unwrap();
    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "adaptive-pack-resume-fallback".to_string(),
        })
        .unwrap();

    let remote = MemoryBackend::new();
    let resumed = resume_push(
        &facade,
        &remote,
        ResumeOptions {
            repo_root: repo_root.clone(),
            branch_token: state.branch.token_hex.clone(),
            operation_id: "adaptive-pack-resume-fallback-op".to_string(),
        },
    )
    .unwrap();

    assert_eq!(resumed.published_snapshot_id, commit.snapshot_id);
    assert!(!remote.list_physical("packs/index/").unwrap().is_empty());
    assert!(!remote.list_physical("packs/data/").unwrap().is_empty());
}
```

- [ ] **Step 2: Run the focused fallback-resume test and confirm failure**

Run:

```powershell
cargo test -p e2v-sync --test push_remote resume_without_journal_uses_pack_uploads_for_small_repository_when_multiple_missing_objects_exist
```

Expected: FAIL because the fallback branch still uses `reachable_object_ids.len() >= small_object_pack_threshold()`.

- [ ] **Step 3: Replace the fallback branch’s direct threshold comparison with the shared helper**

```rust
let pack_enabled = should_pack_current_upload_set(
    &options.repo_root,
    &missing_object_ids,
    reachable_object_ids.len(),
)?;

published_pack_index_segments.extend(upload_objects_with_policy(
    remote,
    &options.repo_root,
    &operation_id,
    &missing_object_ids,
    pack_enabled,
    |object_id| journal.record_verified(&operation_id, object_id, "object"),
)?);
```

- [ ] **Step 4: Re-run the focused fallback-resume test and confirm pass**

Run:

```powershell
cargo test -p e2v-sync --test push_remote resume_without_journal_uses_pack_uploads_for_small_repository_when_multiple_missing_objects_exist
```

Expected: PASS, with the fallback branch publishing pack data and pack index segments even when the large threshold is disabled.

- [ ] **Step 5: Commit the fallback alignment**

```powershell
git add crates/e2v-sync/src/push.rs crates/e2v-sync/tests/push_remote.rs
git commit -m "perf: align resume fallback pack heuristic"
```

### Task 4: Run the verification matrix and re-measure the live AList scenario

**Files:**
- Modify: any files touched by Tasks 1-3 if verification exposes a defect

**Interfaces:**
- Consumes: all behavior and tests from Tasks 1-3
- Produces: a verified implementation commit plus fresh live diagnostic evidence against the prior `131512 ms / 103 requests` baseline

- [ ] **Step 1: Run the focused sync tests**

Run:

```powershell
cargo test -p e2v-sync --lib current_upload_set_
cargo test -p e2v-sync --test push_remote adaptive-pack
cargo test -p e2v-sync --test push_remote resume_without_journal_uses_pack_uploads_for_small_repository_when_multiple_missing_objects_exist
```

Expected: PASS with no new warnings or unrelated failures.

- [ ] **Step 2: Run the full workspace test suite**

Run:

```powershell
cargo test --workspace
```

Expected: PASS across the workspace.

- [ ] **Step 3: Re-run the live AList diagnostic with environment-supplied remote credentials**

Run:

```powershell
$env:E2V_SMALL_PUSH_REMOTE = "alist+https://<token>@alist.991198.xyz/filen/test/diag-20260705-small-push"
cargo run -q -p e2v-cli -- diagnose $env:E2V_SMALL_PUSH_REMOTE --scenario full --file-count 1 --payload-bytes 32 --json --force-single-writer-risk
```

Expected: JSON output with elapsed time and request counts lower than the previous `131512 ms / 103 requests` measurement, or at minimum a clear request-count reduction that justifies the code change.

- [ ] **Step 4: Review the diff and keep only intentional small-push latency changes**

Run:

```powershell
git status --short
git diff --stat
git diff -- crates/e2v-sync/src/push.rs crates/e2v-sync/src/lib.rs crates/e2v-sync/tests/push_remote.rs
```

Expected: only the adaptive packing helper, test seam, and push/resume regression tests are included, with unrelated `.gitignore` changes left unstaged.

- [ ] **Step 5: Create the final implementation commit**

```powershell
git add crates/e2v-sync/src/push.rs crates/e2v-sync/src/lib.rs crates/e2v-sync/tests/push_remote.rs
git commit -m "perf: reduce small push latency on high-latency remotes"
```
