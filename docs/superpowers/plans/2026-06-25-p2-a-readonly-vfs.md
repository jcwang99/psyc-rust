# P2-A Read-Only VFS Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a new `e2v-vfs` crate that owns the platform-independent read-only VFS core, route a CLI mount entry through it, and land the first Windows-first adapter boundary without bypassing `ReadService`.

**Architecture:** The implementation introduces a standalone `e2v-vfs` workspace member that depends only on `e2v-core::ReadService`. The first TDD slices build snapshot-pinned and live-branch core semantics, then add cache-policy and unsupported-semantics guards, then wire a CLI mount surface and Windows adapter boundary around that tested core.

**Tech Stack:** Rust workspace, `anyhow`, `serde` when needed, unit/integration tests in `crates/e2v-vfs/tests` and `crates/e2v-cli/tests`, synchronous `e2v-core::ReadService`.

---

### Task 1: Workspace And Snapshot-Pinned Core

**Files:**
- Modify: `Cargo.toml`
- Create: `crates/e2v-vfs/Cargo.toml`
- Create: `crates/e2v-vfs/src/lib.rs`
- Create: `crates/e2v-vfs/tests/read_only_core.rs`

- [ ] **Step 1: Write the failing snapshot-pinned test**

```rust
#[test]
fn snapshot_pinned_mount_keeps_original_snapshot_after_branch_head_moves() {
    let (repo_root, first_snapshot_id) = seeded_repo_with_one_commit("alpha");
    let vfs = ReadOnlyVfs::mount_snapshot(
        VfsMountConfig::snapshot(repo_root.clone(), first_snapshot_id.clone()),
    )
    .unwrap();

    let handle = vfs.open_file("tracked.txt").unwrap();

    commit_new_head(&repo_root, "beta");

    let bytes = vfs.read(&handle, 0, 32).unwrap();
    assert_eq!(String::from_utf8(bytes).unwrap(), "alpha");
    assert_eq!(vfs.namespace_snapshot_id(), first_snapshot_id);
}
```

- [ ] **Step 2: Run the focused test to verify RED**

Run: `cargo test -p e2v-vfs snapshot_pinned_mount_keeps_original_snapshot_after_branch_head_moves`
Expected: FAIL because `e2v-vfs` and `ReadOnlyVfs::mount_snapshot` do not exist yet.

- [ ] **Step 3: Write the minimal snapshot-pinned implementation**

```rust
pub struct ReadOnlyVfs {
    read_service: ReadService,
    namespace_snapshot: SnapshotHandle,
}

impl ReadOnlyVfs {
    pub fn mount_snapshot(config: VfsMountConfig) -> Result<Self> { /* resolve snapshot once */ }
    pub fn namespace_snapshot_id(&self) -> &str { /* return pinned id */ }
    pub fn open_file(&self, path: &str) -> Result<OpenedFile> { /* use pinned snapshot */ }
    pub fn read(&self, file: &OpenedFile, offset: usize, length: usize) -> Result<Vec<u8>> { /* delegate to ReadService */ }
}
```

- [ ] **Step 4: Re-run the focused test to verify GREEN**

Run: `cargo test -p e2v-vfs snapshot_pinned_mount_keeps_original_snapshot_after_branch_head_moves`
Expected: PASS

### Task 2: Live-Branch Refresh And Old-Handle Stability

**Files:**
- Modify: `crates/e2v-vfs/src/lib.rs`
- Modify: `crates/e2v-vfs/tests/read_only_core.rs`

- [ ] **Step 1: Write the failing live-branch test**

```rust
#[test]
fn live_branch_refresh_updates_new_opens_without_changing_old_handles() {
    let (repo_root, branch_token) = seeded_repo_with_branch_head("alpha");
    let vfs = ReadOnlyVfs::mount_live_branch(
        VfsMountConfig::live_branch(repo_root.clone(), branch_token.clone()),
    )
    .unwrap();

    let old_handle = vfs.open_file("tracked.txt").unwrap();
    commit_new_head(&repo_root, "beta");

    assert!(vfs.refresh_live_branch().unwrap().namespace_changed);

    let new_handle = vfs.open_file("tracked.txt").unwrap();
    assert_eq!(String::from_utf8(vfs.read(&old_handle, 0, 32).unwrap()).unwrap(), "alpha");
    assert_eq!(String::from_utf8(vfs.read(&new_handle, 0, 32).unwrap()).unwrap(), "beta");
}
```

- [ ] **Step 2: Run the focused test to verify RED**

Run: `cargo test -p e2v-vfs live_branch_refresh_updates_new_opens_without_changing_old_handles`
Expected: FAIL because live-branch refresh behavior is missing.

- [ ] **Step 3: Implement the minimal live-branch namespace state**

```rust
enum MountModeState {
    SnapshotPinned,
    LiveBranch { branch_token_hex: String },
}

pub struct RefreshOutcome {
    pub namespace_changed: bool,
}

pub fn refresh_live_branch(&mut self) -> Result<RefreshOutcome> { /* re-resolve branch and swap namespace snapshot if changed */ }
```

- [ ] **Step 4: Re-run the focused test to verify GREEN**

Run: `cargo test -p e2v-vfs live_branch_refresh_updates_new_opens_without_changing_old_handles`
Expected: PASS

### Task 3: Cache Policy And Unsupported Semantics

**Files:**
- Modify: `crates/e2v-vfs/src/lib.rs`
- Modify: `crates/e2v-vfs/tests/read_only_core.rs`

- [ ] **Step 1: Write the failing policy tests**

```rust
#[test]
fn live_branch_mount_without_reliable_invalidation_uses_direct_io_policy() {
    let config = VfsMountConfig::live_branch(temp_repo(), branch_token())
        .with_platform_capabilities(PlatformCapabilities::no_reliable_invalidation());
    let vfs = ReadOnlyVfs::mount_live_branch(config).unwrap();
    assert_eq!(vfs.cache_policy(), CachePolicy::DirectIoFallback);
}

#[test]
fn stream_only_mount_rejects_byte_range_lock_semantics() {
    let vfs = mounted_snapshot_vfs();
    let error = vfs.require_semantic(VfsSemantic::ByteRangeLocks).unwrap_err();
    assert!(error.to_string().contains("unsupported"));
}
```

- [ ] **Step 2: Run the focused tests to verify RED**

Run: `cargo test -p e2v-vfs live_branch_mount_without_reliable_invalidation_uses_direct_io_policy`
Run: `cargo test -p e2v-vfs stream_only_mount_rejects_byte_range_lock_semantics`
Expected: FAIL because cache policy and semantic guards do not exist yet.

- [ ] **Step 3: Implement the minimal policy and semantic checks**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CachePolicy { KernelCacheWithInvalidation, DirectIoFallback }

pub fn require_semantic(&self, semantic: VfsSemantic) -> Result<()> { /* reject unsupported semantics */ }
```

- [ ] **Step 4: Re-run the focused tests to verify GREEN**

Run: `cargo test -p e2v-vfs live_branch_mount_without_reliable_invalidation_uses_direct_io_policy`
Run: `cargo test -p e2v-vfs stream_only_mount_rejects_byte_range_lock_semantics`
Expected: PASS

### Task 4: CLI Mount Entry And Adapter Boundary

**Files:**
- Modify: `crates/e2v-cli/Cargo.toml`
- Modify: `crates/e2v-cli/src/lib.rs`
- Modify: `crates/e2v-cli/tests/cli.rs`
- Modify: `crates/e2v-vfs/Cargo.toml`
- Modify: `crates/e2v-vfs/src/lib.rs`
- Create: `crates/e2v-vfs/src/platform.rs`

- [ ] **Step 1: Write the failing CLI mount test**

```rust
#[test]
fn mount_snapshot_command_delegates_to_e2v_vfs() {
    let repo_root = seeded_repo_root();
    let snapshot_id = head_snapshot_id(&repo_root);
    let output = e2v_cli::run_cli_for_test([
        "e2v",
        "mount",
        "--repo",
        repo_root.to_str().unwrap(),
        "snapshot",
        "--snapshot",
        &snapshot_id,
        "--mount-point",
        "X:",
    ])
    .unwrap();

    assert!(output.contains("snapshot-pinned"));
}
```

- [ ] **Step 2: Run the focused CLI test to verify RED**

Run: `cargo test -p e2v-cli mount_snapshot_command_delegates_to_e2v_vfs`
Expected: FAIL because the CLI mount command does not exist yet.

- [ ] **Step 3: Implement the minimal CLI delegation and platform boundary**

```rust
pub fn mount_snapshot(request: MountRequest) -> Result<MountLaunchSummary> { /* delegate to platform adapter */ }

#[cfg(not(windows))]
fn mount_platform(...) -> Result<MountLaunchSummary> { /* return unsupported-on-this-platform */ }
```

- [ ] **Step 4: Re-run the focused CLI test to verify GREEN**

Run: `cargo test -p e2v-cli mount_snapshot_command_delegates_to_e2v_vfs`
Expected: PASS

### Task 5: Windows-First Adapter Hook

**Files:**
- Modify: `crates/e2v-vfs/Cargo.toml`
- Create: `crates/e2v-vfs/src/windows.rs`
- Modify: `crates/e2v-vfs/src/platform.rs`
- Modify: `crates/e2v-vfs/tests/read_only_core.rs`

- [ ] **Step 1: Write the failing adapter-selection test**

```rust
#[test]
fn non_windows_mount_requests_stop_at_the_platform_boundary() {
    let summary = try_mount_on_current_platform(test_mount_request()).unwrap();
    assert!(summary.status_message.contains("not supported on this platform yet"));
}
```

- [ ] **Step 2: Run the focused test to verify RED**

Run: `cargo test -p e2v-vfs non_windows_mount_requests_stop_at_the_platform_boundary`
Expected: FAIL because the shared platform boundary does not exist yet.

- [ ] **Step 3: Implement the minimal Windows-first adapter routing**

```rust
pub fn try_mount_on_current_platform(request: MountRequest) -> Result<MountLaunchSummary> {
    #[cfg(windows)]
    { return windows::mount(request); }
    #[cfg(not(windows))]
    { return unsupported_platform(request); }
}
```

- [ ] **Step 4: Re-run the focused test to verify GREEN**

Run: `cargo test -p e2v-vfs non_windows_mount_requests_stop_at_the_platform_boundary`
Expected: PASS

### Task 6: Final Verification

**Files:**
- Modify: any touched files from prior tasks

- [ ] **Step 1: Run focused VFS core tests**

Run: `cargo test -p e2v-vfs`
Expected: PASS

- [ ] **Step 2: Run focused CLI mount tests**

Run: `cargo test -p e2v-cli mount_`
Expected: PASS

- [ ] **Step 3: Run workspace verification**

Run: `cargo test --workspace`
Expected: PASS

- [ ] **Step 4: Review the diff for accidental churn and keep only intentional P2-A changes**
