# P1-C Packfile And Performance Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add the missing P1-C pack/index/cache/compaction/benchmark behavior on top of the current packed remote object path.

**Architecture:** Reuse the existing packed upload and packed-range-read behavior as the first pack implementation, then add a published pack-index root, persistent local cache, bounded L0 compaction, and a layout-facing resolution boundary. Land the work in TDD slices that keep the current push/fetch semantics stable.

**Tech Stack:** Rust workspace, `serde`, `serde_json`, integration tests in `crates/e2v-sync/tests`, object-store code in `crates/e2v-store`, benchmark harness via Cargo bench/bin.

---

### Task 1: Pack Index Root And Local Cache

**Files:**
- Create: `crates/e2v-sync/src/pack_index.rs`
- Modify: `crates/e2v-sync/src/lib.rs`
- Modify: `crates/e2v-sync/src/fetch.rs`
- Modify: `crates/e2v-sync/src/push.rs`
- Test: `crates/e2v-sync/tests/fetch_clone.rs`

- [ ] Step 1: Write a failing fetch test that performs one packed fetch, then blocks remote `packs/index/` listing on the next fetch and expects the second fetch to succeed from the cached pack index.
- [ ] Step 2: Run the focused fetch test and confirm it fails because fetch still depends on remote pack-index listing.
- [ ] Step 3: Add `pack_index.rs` with pack-index root types, cache-path helpers, remote-root loading, local-cache update logic, and empty-pack-inventory behavior when the root is absent.
- [ ] Step 4: Wire fetch and push inventory loading to use the pack-index root/cache flow instead of direct pack-index prefix discovery when the root exists.
- [ ] Step 5: Re-run the focused fetch test and confirm it passes.

### Task 2: Bounded L0 Safety Compaction

**Files:**
- Modify: `crates/e2v-sync/src/pack_index.rs`
- Modify: `crates/e2v-sync/src/push.rs`
- Test: `crates/e2v-sync/tests/push_remote.rs`

- [ ] Step 1: Write a failing push test that forces multiple packed uploads, hits the configured segment bound, and expects the published root to shrink back to a bounded segment count.
- [ ] Step 2: Run the focused push test and confirm it fails because L0 segments only accumulate.
- [ ] Step 3: Implement minimal aggregate-segment compaction and root republish logic.
- [ ] Step 4: Re-run the focused push test and confirm it passes.
- [ ] Step 5: Re-run the existing packed push/fetch regression tests to ensure compaction does not break the current packed object path.

### Task 3: Layout-Facing Resolution

**Files:**
- Modify: `crates/e2v-store/src/logical_object_store.rs`
- Modify: `crates/e2v-store/src/lib.rs`
- Modify: `crates/e2v-sync/src/pack_index.rs`
- Test: `crates/e2v-store/src/logical_object_store.rs`

- [ ] Step 1: Write a failing unit test that resolves a packed object entry into a `PhysicalObjectRef` with a pack container path and byte range.
- [ ] Step 2: Run the focused unit test and confirm it fails because only direct layout resolution exists.
- [ ] Step 3: Add the minimal layout-facing type/helper needed to represent pack-backed physical refs without breaking direct layout users.
- [ ] Step 4: Re-run the focused unit test and confirm it passes.

### Task 4: Benchmark Harness

**Files:**
- Create: `crates/e2v-sync/benches/p1_c_pack_paths.rs` or `crates/e2v-sync/src/bin/p1_c_pack_bench.rs`
- Modify: `crates/e2v-sync/Cargo.toml`
- Test: benchmark smoke command in workspace root

- [ ] Step 1: Write a failing smoke test or verification hook that expects the benchmark target to be invokable by Cargo.
- [ ] Step 2: Run the benchmark command and confirm it fails because the target does not exist yet.
- [ ] Step 3: Add a runnable harness that exercises packed push, cached fetch, and compaction-trigger paths with deterministic fixture generation.
- [ ] Step 4: Run the benchmark command and confirm it completes successfully.

### Task 5: Final Verification

**Files:**
- Modify: any touched files from prior tasks

- [ ] Step 1: Run focused P1-C tests for fetch, push, and object-store resolution.
- [ ] Step 2: Run the benchmark harness smoke command.
- [ ] Step 3: Run `cargo test --workspace`.
- [ ] Step 4: Review the diff for accidental churn and keep only intentional P1-C changes.
