# P3-B Access Pattern Hiding Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Complete P3-B by adding an ORAM-style storage layout with padded access schedules, traffic shaping, cost-aware policy control, an authenticated oblivious layout root, and explicit disabling of long-lived physical dedup semantics in ORAM mode.

**Architecture:** Upgrade `e2v-store` from a single-ref layout boundary into a layout-metadata plus read-plan boundary, then add an oblivious layout implementation that publishes encrypted bucket metadata and generation-scoped randomized placement through the existing `TransactionPublisher` and layout-root rewrite flow in `e2v-sync`. Keep `e2v-core` logical object IDs, manifests, and snapshots unchanged; all remote physical access must flow through layout read plans and layout enumeration instead of direct physical-path reconstruction.

**Tech Stack:** Rust workspace crates `e2v-store`, `e2v-sync`, `e2v-api`, `e2v-cli`; existing `serde`, `rusqlite`, `tempfile`, `TransactionPublisher`, pack/index helpers, layout-root CAS or lease publication, and maintenance journaling.

## Global Constraints

- P3-B must preserve the existing logical object model, manifest graph, and snapshot DAG; only physical placement, access scheduling, and remote layout metadata may change.
- `StorageLayout` is the only boundary allowed to explain how logical objects map to physical access; `fetch`, `verify`, `repair`, `gc`, and reshuffle code must not reconstruct stable physical refs directly from object IDs.
- `LayoutRoot` must become the authenticated source of truth for layout mode, oblivious generation, schedule policy, traffic policy, cost policy, and dedup mode.
- ORAM mode must explicitly disable long-lived stable physical dedup semantics by switching to a generation-scoped randomized physical placement policy.
- Layout publication and reshuffle must continue to use generation plus CAS or safe lease-backed publication through `TransactionPublisher`; backends without sufficient capability must reject ORAM enablement.
- Read plans in ORAM mode must be able to express real reads, cover reads, response windows, and traffic-shaping hints.
- Any confirmed behavior change must follow TDD: failing test first, verify red, minimal implementation, verify green, then refactor.
- Run `cargo fmt --all` before edits and keep formatting-only churn isolated from behavior commits.
- Target only the latest layout-root format; do not preserve compatibility shims for the pre-P3-B flat layout-root schema.

---

### Task 1: Upgrade Layout Metadata And The StorageLayout Contract

**Files:**
- Modify: `crates/e2v-store/src/layout.rs`
- Modify: `crates/e2v-store/src/storage_layout.rs`
- Modify: `crates/e2v-store/src/lib.rs`
- Modify: `crates/e2v-store/src/memory_backend.rs`
- Modify: `crates/e2v-store/src/local_backend.rs`
- Modify: `crates/e2v-store/src/opendal_backend.rs`
- Test: `crates/e2v-store/src/storage_layout.rs`
- Test: `crates/e2v-store/src/memory_backend.rs`

**Interfaces:**
- Consumes: existing `LayoutRoot`, `PhysicalObjectRef`, `StorageLayout`, layout-root store history, and backend capability structs.
- Produces:
  - `pub enum LayoutMode { Direct, Pack, Rewrite, Oblivious }`
  - `pub enum DedupMode { StablePhysical, GenerationScopedRandomized }`
  - `pub struct LayoutSchedulePolicy { pub bucket_bytes: u32, pub min_total_reads: u8, pub cover_reads_per_request: u8, pub reshuffle_after_generations: u32 }`
  - `pub struct LayoutTrafficPolicy { pub max_parallel_reads: u8, pub inter_read_delay_ms: u16, pub burst_budget_bytes: u64, pub target_request_window_ms: u32 }`
  - `pub struct LayoutCostPolicy { pub profile: String, pub max_expected_read_amplification: u8, pub max_expected_write_amplification: u8 }`
  - `pub struct LogicalReadRequest<'a> { pub object_id: &'a str, pub offset: u64, pub length: u64 }`
  - `pub enum PhysicalReadKind { Real, Cover }`
  - `pub struct PhysicalReadOp { pub physical_ref: PhysicalObjectRef, pub kind: PhysicalReadKind }`
  - `pub struct LogicalResponseWindow { pub logical_offset: u64, pub logical_length: u64 }`
  - `pub struct TrafficExecutionHint { pub max_parallel_reads: u8, pub inter_read_delay_ms: u16, pub burst_budget_bytes: u64, pub target_request_window_ms: u32 }`
  - `pub struct ReadPlan { pub layout_id: String, pub generation: u64, pub operations: Vec<PhysicalReadOp>, pub response_window: LogicalResponseWindow, pub traffic_hint: TrafficExecutionHint }`
  - `pub trait StorageLayout { fn plan_logical_read(&self, request: LogicalReadRequest<'_>) -> Result<ReadPlan>; fn enumerate_reachable_physical_refs(&self) -> Result<Vec<PhysicalObjectRef>>; }`

- [ ] **Step 1: Write the failing layout-root and read-plan tests**

Add unit tests that prove:
- `LayoutRoot` round-trips with `mode`, `dedup_mode`, `oblivious_generation`, `schedule_policy`, `traffic_policy`, and `cost_policy`;
- `DirectStorageLayout` produces one `Real` read with no cover reads;
- `PackStorageLayout` produces one `Real` read with no cover reads;
- memory and local backends persist the richer layout-root JSON and retained history.

Use concrete test shapes like:

```rust
#[test]
fn direct_storage_layout_plans_one_real_read_without_cover_reads() {
    let plan = DirectStorageLayout
        .plan_logical_read(LogicalReadRequest {
            object_id: "a".repeat(64).as_str(),
            offset: 0,
            length: 32,
        })
        .unwrap();

    assert_eq!(plan.operations.len(), 1);
    assert!(matches!(plan.operations[0].kind, PhysicalReadKind::Real));
}
```

- [ ] **Step 2: Run the focused store tests to verify they fail**

Run: `cargo test -p e2v-store storage_layout -- --nocapture`

Expected: FAIL because the richer `LayoutRoot` fields and `plan_logical_read` contract do not exist yet.

- [ ] **Step 3: Implement the minimal metadata and contract upgrade**

Implement the new layout metadata structs and extend the direct/pack implementations so they satisfy the new trait without changing existing direct or pack behavior.

The minimal shape in `layout.rs` should look like:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutRoot {
    pub schema_version: u32,
    pub layout_id: String,
    pub generation: u64,
    pub mode: LayoutMode,
    pub mapping_policy: String,
    pub dedup_mode: DedupMode,
    pub oblivious_generation: Option<u64>,
    pub schedule_policy: LayoutSchedulePolicy,
    pub traffic_policy: LayoutTrafficPolicy,
    pub cost_policy: LayoutCostPolicy,
}
```

Direct and pack plans should remain trivial:

```rust
ReadPlan {
    layout_id: "direct".to_string(),
    generation: 1,
    operations: vec![PhysicalReadOp {
        physical_ref,
        kind: PhysicalReadKind::Real,
    }],
    response_window: LogicalResponseWindow {
        logical_offset: request.offset,
        logical_length: request.length,
    },
    traffic_hint: TrafficExecutionHint::no_shaping(),
}
```

- [ ] **Step 4: Re-run the focused store tests**

Run: `cargo test -p e2v-store storage_layout -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Run adjacent backend regression coverage**

Run: `cargo test -p e2v-store memory_backend -- --nocapture`

Run: `cargo test -p e2v-store local_folder_backend -- --nocapture`

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/e2v-store/src/layout.rs crates/e2v-store/src/storage_layout.rs crates/e2v-store/src/lib.rs crates/e2v-store/src/memory_backend.rs crates/e2v-store/src/local_backend.rs crates/e2v-store/src/opendal_backend.rs
git commit -m "Upgrade layout metadata and read-plan boundary"
```

### Task 2: Add Oblivious Layout Metadata, Randomized Placement, And Capability Gating

**Files:**
- Create: `crates/e2v-store/src/oblivious_layout.rs`
- Modify: `crates/e2v-store/src/storage_layout.rs`
- Modify: `crates/e2v-store/src/capability.rs`
- Modify: `crates/e2v-store/src/lib.rs`
- Modify: `crates/e2v-store/src/memory_backend.rs`
- Modify: `crates/e2v-store/src/local_backend.rs`
- Modify: `crates/e2v-store/src/opendal_backend.rs`
- Test: `crates/e2v-store/src/oblivious_layout.rs`
- Test: `crates/e2v-store/src/capability.rs`

**Interfaces:**
- Consumes: Task 1 layout metadata, `PhysicalObjectRef`, and backend capability declarations.
- Produces:
  - `pub struct ObliviousObjectPlacement { pub object_id: String, pub bucket_path: String, pub slot_offset: u64, pub slot_length: u64 }`
  - `pub struct ObliviousLayoutRoot { pub schema_version: u32, pub layout_generation: u64, pub oblivious_generation: u64, pub bucket_bytes: u32, pub segment_paths: Vec<String> }`
  - `pub struct ObliviousStorageLayout`
  - `impl StorageLayout for ObliviousStorageLayout`
  - `impl BackendCapability { pub fn supports_oblivious_layout_updates(&self) -> bool }`

- [ ] **Step 1: Write the failing oblivious-layout tests**

Add tests that prove:
- the same logical object resolves to bucket-sized read plans with deterministic cover reads;
- a reshuffle with a new `oblivious_generation` can map the same logical object to a different bucket path without changing its logical ID;
- capability gating rejects ORAM enablement on backends that report `supports_oblivious_access_schedule = false`.

Use concrete tests like:

```rust
#[test]
fn oblivious_layout_adds_cover_reads_and_bucket_sized_windows() {
    let layout = seeded_oblivious_layout();
    let plan = layout
        .plan_logical_read(LogicalReadRequest {
            object_id: TEST_OBJECT_ID,
            offset: 7,
            length: 19,
        })
        .unwrap();

    assert!(plan.operations.len() >= 2);
    assert!(plan.operations.iter().any(|op| matches!(op.kind, PhysicalReadKind::Cover)));
    assert!(plan.operations.iter().all(|op| op.physical_ref.length == 4096));
}
```

- [ ] **Step 2: Run the focused oblivious-layout tests to verify they fail**

Run: `cargo test -p e2v-store oblivious_layout -- --nocapture`

Expected: FAIL because the oblivious layout implementation does not exist yet.

- [ ] **Step 3: Implement the minimal oblivious-layout engine**

Implement:
- encrypted placement metadata types;
- deterministic cover-read selection derived from authenticated layout metadata plus `oblivious_generation`;
- generation-scoped randomized bucket paths;
- capability helpers that explicitly gate ORAM mode.

The minimal plan builder should follow this shape:

```rust
let mut operations = vec![PhysicalReadOp {
    physical_ref: real_bucket_ref,
    kind: PhysicalReadKind::Real,
}];
operations.extend(select_cover_bucket_refs(...).into_iter().map(|physical_ref| {
    PhysicalReadOp {
        physical_ref,
        kind: PhysicalReadKind::Cover,
    }
}));
```

- [ ] **Step 4: Re-run the focused oblivious-layout tests**

Run: `cargo test -p e2v-store oblivious_layout -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/e2v-store/src/oblivious_layout.rs crates/e2v-store/src/storage_layout.rs crates/e2v-store/src/capability.rs crates/e2v-store/src/lib.rs crates/e2v-store/src/memory_backend.rs crates/e2v-store/src/local_backend.rs crates/e2v-store/src/opendal_backend.rs
git commit -m "Add oblivious layout metadata and capability gating"
```

### Task 3: Implement ORAM Planning, Publication, And Reshuffle Recovery

**Files:**
- Create: `crates/e2v-sync/src/oram.rs`
- Modify: `crates/e2v-sync/src/lib.rs`
- Modify: `crates/e2v-sync/src/journal.rs`
- Modify: `crates/e2v-sync/src/publisher.rs`
- Modify: `crates/e2v-sync/src/remote_maintenance.rs`
- Modify: `crates/e2v-sync/src/transaction.rs`
- Test: `crates/e2v-sync/tests/remote_maintenance.rs`

**Interfaces:**
- Consumes: Task 1 and 2 store types, existing pack publication helpers, layout-root publication, and maintenance journal infrastructure.
- Produces:
  - `pub struct ObliviousLayoutPlan { pub estimated_real_reads_per_request: u8, pub estimated_cover_reads_per_request: u8, pub estimated_bytes_per_request: u64, pub estimated_write_amplification: u8, pub requires_layout_root_rewrite: bool, pub advisory_messages: Vec<String> }`
  - `pub struct ObliviousLayoutStatus { pub layout_mode: String, pub dedup_mode: String, pub layout_generation: u64, pub oblivious_generation: Option<u64>, pub policy_profile: String }`
  - `pub struct EnableObliviousLayoutOptions { pub repo_root: PathBuf, pub policy_profile: String }`
  - `pub struct ReshuffleObliviousLayoutOptions { pub repo_root: PathBuf, pub policy_profile: String }`
  - `pub fn plan_oblivious_layout<R: RemoteBackend>(remote: &R, repo_root: &Path) -> Result<ObliviousLayoutPlan>`
  - `pub fn status_oblivious_layout<R: RemoteBackend>(remote: &R, repo_root: &Path) -> Result<ObliviousLayoutStatus>`
  - `pub fn enable_oblivious_layout<R: RemoteBackend + Clone>(remote: &R, options: EnableObliviousLayoutOptions) -> Result<ObliviousLayoutStatus>`
  - `pub fn reshuffle_oblivious_layout<R: RemoteBackend + Clone>(remote: &R, options: ReshuffleObliviousLayoutOptions) -> Result<ObliviousLayoutStatus>`

- [ ] **Step 1: Write the failing ORAM planning and publication tests**

Add tests that prove:
- planning reports amplification and advisory messages;
- enablement publishes `oblivious/root.json`, encrypted segment metadata, and a new `layout_root.json` generation;
- reshuffle advances `oblivious_generation` and can place the same logical object on a fresh randomized bucket path;
- interrupted enablement or reshuffle resumes from journal state instead of republishing completed stages.

- [ ] **Step 2: Run the focused ORAM publication tests to verify they fail**

Run: `cargo test -p e2v-sync oblivious_layout -- --nocapture`

Expected: FAIL because the ORAM planning and execution surface does not exist yet.

- [ ] **Step 3: Implement the minimal enablement and reshuffle executor**

Implement:
- encrypted `oblivious/root.json` and segment publication;
- generation-scoped randomized bucket writes;
- journaled resume state for upload, segment publish, root publish, and layout-root CAS;
- `dedup_mode = GenerationScopedRandomized` in the published layout root.

The core publication path should follow the existing transaction boundary:

```rust
let session = publisher.begin(PublishPlan { ... })?;
let session = PublishSession {
    next_layout_root: Some(next_layout_root.clone()),
    next_layout_root_bytes: Some(next_layout_root_bytes.clone()),
    ..session
};
publisher.publish_layout_if_needed(&session)?;
publisher.pre_commit_verify(&session)?;
publisher.complete(session)?;
```

- [ ] **Step 4: Re-run the focused ORAM publication tests**

Run: `cargo test -p e2v-sync oblivious_layout -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/e2v-sync/src/oram.rs crates/e2v-sync/src/lib.rs crates/e2v-sync/src/journal.rs crates/e2v-sync/src/publisher.rs crates/e2v-sync/src/remote_maintenance.rs crates/e2v-sync/src/transaction.rs crates/e2v-sync/tests/remote_maintenance.rs
git commit -m "Implement oblivious layout planning and publication"
```

### Task 4: Route Maintenance And Remote Reads Through Layout Plans

**Files:**
- Modify: `crates/e2v-sync/src/fetch.rs`
- Modify: `crates/e2v-sync/src/pack_index.rs`
- Modify: `crates/e2v-sync/src/remote_maintenance.rs`
- Modify: `crates/e2v-sync/src/push.rs`
- Test: `crates/e2v-sync/tests/fetch_clone.rs`
- Test: `crates/e2v-sync/tests/remote_maintenance.rs`

**Interfaces:**
- Consumes: Task 1 to 3 layout plan and layout enumeration APIs, pack-index cache, and existing remote maintenance traversal.
- Produces:
  - remote read helpers that accept `ReadPlan` instead of one stable physical location
  - maintenance traversal helpers that enumerate physical refs through the active layout implementation

- [ ] **Step 1: Write the failing consumer-routing tests**

Add tests that prove:
- `fetch` can satisfy logical object reads from an oblivious layout plan without reconstructing stable object paths;
- `verify`, `repair`, and `gc` can enumerate reachable physical refs through the active layout metadata;
- retained prior ORAM generations remain protected from deletion until grace-period rules allow cleanup.

- [ ] **Step 2: Run the focused routing tests to verify they fail**

Run: `cargo test -p e2v-sync fetch_after_oram_enablement -- --nocapture`

Run: `cargo test -p e2v-sync gc_under_oblivious_layout -- --nocapture`

Expected: FAIL because current consumers still assume direct or pack physical lookup shortcuts.

- [ ] **Step 3: Implement minimal read-plan and enumeration routing**

Implement a shared executor that consumes `ReadPlan` operations in order, preserves cover reads, and applies shaping hints. Update maintenance code so physical-ref reachability comes from `enumerate_reachable_physical_refs`.

The central execution shape should look like:

```rust
for op in &plan.operations {
    maybe_apply_traffic_hint(&plan.traffic_hint, op)?;
    let _bytes = remote.read_physical_ref(&op.physical_ref)?;
    if matches!(op.kind, PhysicalReadKind::Real) {
        real_bytes.push(_bytes);
    }
}
```

- [ ] **Step 4: Re-run the focused routing tests**

Run: `cargo test -p e2v-sync fetch_after_oram_enablement -- --nocapture`

Run: `cargo test -p e2v-sync gc_under_oblivious_layout -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Run adjacent sync regressions**

Run: `cargo test -p e2v-sync fetch_clone -- --nocapture`

Run: `cargo test -p e2v-sync remote_maintenance -- --nocapture`

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/e2v-sync/src/fetch.rs crates/e2v-sync/src/pack_index.rs crates/e2v-sync/src/remote_maintenance.rs crates/e2v-sync/src/push.rs crates/e2v-sync/tests/fetch_clone.rs crates/e2v-sync/tests/remote_maintenance.rs
git commit -m "Route sync maintenance and reads through layout plans"
```

### Task 5: Expose ORAM Plan, Status, Enablement, And Reshuffle Through API And CLI

**Files:**
- Modify: `crates/e2v-api/src/lib.rs`
- Modify: `crates/e2v-api/src/c_abi.rs`
- Modify: `crates/e2v-api/include/e2v_api.h`
- Modify: `crates/e2v-api/tests/sdk_contract.rs`
- Modify: `crates/e2v-api/tests/c_abi_contract.rs`
- Modify: `crates/e2v-cli/src/lib.rs`
- Modify: `crates/e2v-cli/tests/cli.rs`

**Interfaces:**
- Consumes: Task 3 ORAM plan/status/execute helpers.
- Produces:
  - SDK request and response types for ORAM plan/status/enable/reshuffle
  - optional C-ABI JSON entrypoints mirroring the same workflow
  - CLI commands:
    - `e2v oram plan`
    - `e2v oram status`
    - `e2v oram enable --policy <...>`
    - `e2v oram reshuffle --policy <...>`

- [ ] **Step 1: Write the failing API and CLI tests**

Add tests that prove:
- SDK can plan, inspect status, enable, and reshuffle ORAM mode through explicit or default remotes;
- CLI reports the current policy and dedup mode;
- CLI rejects unsupported backends and contradictory policy inputs with explicit errors;
- enablement and reshuffle output include amplification and advisory messages.

- [ ] **Step 2: Run the focused API and CLI tests to verify they fail**

Run: `cargo test -p e2v-api oblivious_layout -- --nocapture`

Run: `cargo test -p e2v-cli oblivious_layout -- --nocapture`

Expected: FAIL because the public user surfaces do not exist yet.

- [ ] **Step 3: Implement the SDK, C-ABI, and CLI wiring**

Implement thin delegation that reuses the default-remote workflow contract already used by verify, repair, GC, and historical rewrite.

The SDK-facing shape should follow:

```rust
pub fn oblivious_layout_plan_default_remote(
    &self,
    request: ObliviousLayoutPlanRequest,
) -> Result<ObliviousLayoutPlanResponse> { ... }
```

and the CLI should render the policy and advisory fields directly from the returned struct.

- [ ] **Step 4: Re-run the focused API and CLI tests**

Run: `cargo test -p e2v-api oblivious_layout -- --nocapture`

Run: `cargo test -p e2v-cli oblivious_layout -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/e2v-api/src/lib.rs crates/e2v-api/src/c_abi.rs crates/e2v-api/include/e2v_api.h crates/e2v-api/tests/sdk_contract.rs crates/e2v-api/tests/c_abi_contract.rs crates/e2v-cli/src/lib.rs crates/e2v-cli/tests/cli.rs
git commit -m "Expose oblivious layout workflows through api and cli"
```

### Task 6: Verify, Optimize, And Close The Branch

**Files:**
- Review: `crates/e2v-store`, `crates/e2v-sync`, `crates/e2v-api`, `crates/e2v-cli`

**Interfaces:**
- Consumes: all prior tasks.
- Produces: verified P3-B branch with a clean worktree and evidence-backed final status.

- [ ] **Step 1: Run formatting and linting**

Run: `cargo fmt --all`

Run: `cargo clippy --workspace --all-targets -- -D warnings`

Expected: PASS.

- [ ] **Step 2: Run focused ORAM regressions**

Run: `cargo test -p e2v-store oblivious_layout -- --nocapture`

Run: `cargo test -p e2v-sync oblivious_layout -- --nocapture`

Run: `cargo test -p e2v-api oblivious_layout -- --nocapture`

Run: `cargo test -p e2v-cli oblivious_layout -- --nocapture`

Expected: PASS.

- [ ] **Step 3: Run full workspace verification**

Run: `cargo test --workspace -- --nocapture`

Expected: PASS.

- [ ] **Step 4: Run pack-path viability check**

Run: `cargo run --release -p e2v-sync --bin p1_c_pack_bench`

Expected: PASS and no obvious regression that breaks the existing pack/read pipeline after the layout-boundary change.

- [ ] **Step 5: Review diff for accidental churn**

Run: `git status --short`

Run: `git diff --stat master...HEAD`

Expected: only intentional P3-B files, tests, and reviewed spec/plan documents.

- [ ] **Step 6: Complete development**

Use the required `superpowers:finishing-a-development-branch` skill after all verification is green.
