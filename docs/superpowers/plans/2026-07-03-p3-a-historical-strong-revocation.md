# P3-A Historical Strong Revocation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Complete P3-A by adding repository-wide historical strong revocation with optional full re-encryption, rewrite planning, old-epoch retirement, encrypted rewrite journaling, layout-root rewrite publication, and operator guidance for large repositories.

**Architecture:** Reuse the existing P2 epoch model, P0/P1 transaction publisher, and P1 pack/index transport instead of inventing a separate P3 storage plane. The implementation rewrites all reachable local and remote objects plus control records onto the current active epoch, publishes a fresh remote layout generation and pack-index generation through the existing transaction boundary, retires old epoch secrets only after the rewrite is locally consistent, and records resumable progress in a dedicated rewrite journal.

**Tech Stack:** Rust workspace crates `e2v-core`, `e2v-sync`, `e2v-api`, `e2v-cli`; existing `rusqlite`, `serde`, `tempfile`, transaction publisher, pack/index helpers, and repository facade.

## Global Constraints

- P2 revocation remains “future access only”; P3-A must be a distinct explicit maintenance flow and CLI copy must not blur the two.
- Historical strong revocation must not overwrite old **remote** physical refs in place; it must publish a new layout generation and new remote physical refs before the old ones are purged.
- Old epoch keys may be retired only after all locally readable objects and control records have been rewritten onto the current active epoch.
- P3-A must reuse the existing transaction/recovery model and layout-root publish boundary instead of bypassing `TransactionPublisher`.
- P3-A must emit large-repository advisory text that recommends revoking remote storage credentials first and frames full repository re-encryption as an expensive maintenance action.
- The implementation must remain compatible with the current fetch/clone/read stack; if stale loose-object duplicates would make the current reader choose the wrong epoch, the rewrite flow must purge those stale remote loose refs during finalization rather than leaving ambiguous duplicate carriers behind.
- Any confirmed behavior change must follow TDD: failing test first, verify red, minimal implementation, verify green, then refactor.
- Run `cargo fmt --all` before edits and keep formatting-only churn out of behavior commits.

---

### Task 1: Add Local Rewrite And Old-Epoch Retirement Primitives

**Files:**
- Modify: `crates/e2v-core/src/facade.rs`
- Modify: `crates/e2v-core/src/keyring.rs`
- Modify: `crates/e2v-core/src/lib.rs`
- Test: `crates/e2v-core/tests/init_repository.rs`

**Interfaces:**
- Consumes: existing `RepositoryFacade`, `RepoSecrets`, `KeyringState`, local control-record helpers, and object envelope parsing.
- Produces:
  - `RepositoryFacade::rewrite_history_to_active_epoch(repo_root: impl AsRef<Path>, password: &str) -> Result<HistoryRewriteLocalResult>`
  - `pub struct HistoryRewriteLocalResult { pub rewritten_object_ids: Vec<String>, pub rewritten_control_records: Vec<String>, pub retired_epoch_count: usize, pub active_epoch: u32 }`
  - `sync_support::local_object_envelope_key_epoch(repo_root: impl AsRef<Path>, object_id: &str) -> Result<u32>`

- [ ] **Step 1: Write the failing local rewrite tests**

Add tests that prove:
- after `share_revoke_member` or `share_revoke_device`, old snapshots remain readable before P3-A;
- after `rewrite_history_to_active_epoch`, every local object envelope and encrypted control record uses the current active epoch;
- retired old epochs are no longer present in the latest keyring state;
- `verify_ref`, `open`, `snapshots`, and `read_file` still work after retirement.

- [ ] **Step 2: Run the focused core tests to verify they fail**

Run: `cargo test -p e2v-core historical_rewrite -- --nocapture`

Expected: FAIL because the new facade entrypoint, result type, or epoch assertions do not exist yet.

- [ ] **Step 3: Implement the minimal local rewrite path**

Implement local helpers that:
- enumerate reachable local object ids from the current branch head and branch refs;
- decrypt and re-encrypt local object bytes under `RepoSecrets.active_epoch` without changing logical object ids;
- rewrite local encrypted control records (`default ref`, branch refs, `layout_root.json`) onto the active epoch;
- rebuild the latest keyring generation with only the active epoch retained after the rewrite succeeds;
- clear local unlocked-key caches and preserve local-device/password unlock behavior.

- [ ] **Step 4: Re-run the focused core tests**

Run: `cargo test -p e2v-core historical_rewrite -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Run adjacent local regression coverage**

Run: `cargo test -p e2v-core share_revoke_member_advances_active_epoch_and_removes_member_envelope -- --exact`

Run: `cargo test -p e2v-core share_revoke_device_advances_epoch_and_only_revokes_target_device -- --exact`

Run: `cargo test -p e2v-core rotated_active_epoch_keeps_old_snapshot_readable -- --exact`

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/e2v-core/src/facade.rs crates/e2v-core/src/keyring.rs crates/e2v-core/src/lib.rs crates/e2v-core/tests/init_repository.rs
git commit -m "Add local history rewrite and epoch retirement primitives"
```

### Task 2: Add Rewrite Planning And Encrypted Rewrite Journal Recovery

**Files:**
- Create: `crates/e2v-sync/src/history_rewrite.rs`
- Modify: `crates/e2v-sync/src/lib.rs`
- Modify: `crates/e2v-sync/src/journal.rs`
- Test: `crates/e2v-sync/tests/remote_maintenance.rs`

**Interfaces:**
- Consumes: existing pack/index helpers, `OperationJournal`, `TransactionPublisher`, maintenance object traversal, and core local rewrite result.
- Produces:
  - `pub struct HistoricalRewritePlan { pub reachable_object_count: usize, pub remote_loose_object_count: usize, pub remote_pack_object_count: usize, pub old_epoch_count: usize, pub large_repo_advisory: Option<String>, pub requires_remote_credential_revocation_guidance: bool }`
  - `pub struct HistoricalRewriteOptions { pub repo_root: PathBuf, pub password: String, pub confirm_full_reencryption: bool }`
  - `pub fn plan_historical_rewrite<R: RemoteBackend>(remote: &R, options: HistoricalRewritePlanOptions) -> Result<HistoricalRewritePlan>`
  - encrypted rewrite journal rows that can resume object rewrite, publish, and stale-ref purge stages.

- [ ] **Step 1: Write failing sync tests for plan and journal behavior**

Add tests that prove:
- a planning call reports old-epoch presence and emits large-repo credential-revocation guidance;
- the rewrite journal stores compact encrypted progress metadata;
- a partially written rewrite can resume without re-uploading already rewritten objects.

- [ ] **Step 2: Run the focused sync tests to verify they fail**

Run: `cargo test -p e2v-sync historical_rewrite_plan -- --nocapture`

Expected: FAIL because the plan types and rewrite journal do not exist yet.

- [ ] **Step 3: Implement plan/report types and journal schema**

Implement:
- a dedicated history-rewrite module with plan-only analysis;
- an encrypted or authenticated rewrite journal that records stages, target layout generation, pending object ids, published pack segments, and stale-ref purge candidates;
- compact JSON or sqlite-backed state storage consistent with existing maintenance journaling.

- [ ] **Step 4: Re-run the focused sync tests**

Run: `cargo test -p e2v-sync historical_rewrite_plan -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/e2v-sync/src/history_rewrite.rs crates/e2v-sync/src/lib.rs crates/e2v-sync/src/journal.rs crates/e2v-sync/tests/remote_maintenance.rs
git commit -m "Add historical rewrite planning and journal recovery"
```

### Task 3: Implement Remote Historical Rewrite Publication And Stale-Ref Purge

**Files:**
- Modify: `crates/e2v-sync/src/history_rewrite.rs`
- Modify: `crates/e2v-sync/src/push.rs`
- Modify: `crates/e2v-sync/src/pack_index.rs`
- Modify: `crates/e2v-sync/src/remote_maintenance.rs`
- Test: `crates/e2v-sync/tests/remote_maintenance.rs`
- Test: `crates/e2v-sync/tests/fetch_clone.rs`

**Interfaces:**
- Consumes: `plan_historical_rewrite`, `RepositoryFacade::rewrite_history_to_active_epoch`, pack writer/index, publisher begin/heartbeat/pre_commit/publish_ref/complete, and remote control-plane readers.
- Produces:
  - `pub struct HistoricalRewriteResult { pub rewritten_objects: usize, pub retired_epoch_count: usize, pub deleted_stale_remote_refs: Vec<String>, pub next_layout_generation: u64 }`
  - `pub fn historical_rewrite_remote<R: RemoteBackend>(remote: &R, options: HistoricalRewriteOptions) -> Result<HistoricalRewriteResult>`

- [ ] **Step 1: Write failing end-to-end rewrite tests**

Add tests that prove:
- after member/device revocation plus historical rewrite, fetch/clone with the retained credentials still works;
- old epoch keys can no longer decrypt current local or remote objects;
- the remote current view uses newly written physical refs and a new layout generation;
- stale loose-object refs from the old epoch are purged so fetch does not accidentally prefer them over rewritten packed refs;
- interrupted rewrite can resume from the journal and finish publication exactly once.

- [ ] **Step 2: Run the focused rewrite tests to verify they fail**

Run: `cargo test -p e2v-sync historical_rewrite_remote -- --nocapture`

Expected: FAIL because the rewrite executor and remote publication logic do not exist yet.

- [ ] **Step 3: Implement the minimal remote rewrite executor**

Implement:
- remote reachable-object traversal using current branch refs and pack inventory;
- re-encryption of each reachable remote object onto the current active epoch and upload into fresh pack files / pack-index segments;
- new layout-root generation publication through the existing transaction publisher;
- ref/keyring pointer republish on the active epoch;
- stale old loose-object deletion after successful publish so the current reader cannot select the wrong epoch carrier;
- local index purge by deleting or invalidating `.e2v/index.sqlite3` after successful rewrite.

- [ ] **Step 4: Re-run the focused rewrite tests**

Run: `cargo test -p e2v-sync historical_rewrite_remote -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Run adjacent maintenance and fetch/push regressions**

Run: `cargo test -p e2v-sync remote_maintenance -- --nocapture`

Run: `cargo test -p e2v-sync fetch_clone -- --nocapture`

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/e2v-sync/src/history_rewrite.rs crates/e2v-sync/src/push.rs crates/e2v-sync/src/pack_index.rs crates/e2v-sync/src/remote_maintenance.rs crates/e2v-sync/tests/remote_maintenance.rs crates/e2v-sync/tests/fetch_clone.rs
git commit -m "Implement remote historical rewrite publication"
```

### Task 4: Expose P3-A Through SDK And CLI With Explicit Safety Copy

**Files:**
- Modify: `crates/e2v-api/src/lib.rs`
- Modify: `crates/e2v-api/tests/sdk_contract.rs`
- Modify: `crates/e2v-cli/src/lib.rs`
- Modify: `crates/e2v-cli/tests/cli.rs`

**Interfaces:**
- Consumes: `plan_historical_rewrite`, `historical_rewrite_remote`.
- Produces:
  - SDK request/response types for rewrite plan and execute.
  - CLI commands for plan and execute, with explicit `--confirm-full-reencryption`.

- [ ] **Step 1: Write failing API and CLI tests**

Add tests that prove:
- SDK can request a rewrite plan and execute historical rewrite against an explicit or default remote;
- CLI copy clearly distinguishes future-only revocation from historical strong revocation;
- CLI refuses execution without `--confirm-full-reencryption`;
- plan output includes large-repo guidance and remote credential revocation advice.

- [ ] **Step 2: Run the focused API and CLI tests to verify they fail**

Run: `cargo test -p e2v-api historical_rewrite -- --nocapture`

Run: `cargo test -p e2v-cli historical_rewrite -- --nocapture`

Expected: FAIL because the new SDK/CLI surface does not exist yet.

- [ ] **Step 3: Implement SDK and CLI wiring**

Implement:
- SDK request/response structs and façade methods that use the same default-remote workflow contract as verify/repair/gc;
- CLI subcommands under the maintenance surface with JSON and human-readable summaries;
- explicit warning/advisory strings for large repositories and storage-credential revocation.

- [ ] **Step 4: Re-run the focused API and CLI tests**

Run: `cargo test -p e2v-api historical_rewrite -- --nocapture`

Run: `cargo test -p e2v-cli historical_rewrite -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/e2v-api/src/lib.rs crates/e2v-api/tests/sdk_contract.rs crates/e2v-cli/src/lib.rs crates/e2v-cli/tests/cli.rs
git commit -m "Expose historical strong revocation through sdk and cli"
```

### Task 5: Verify, Optimize, And Close The Branch

**Files:**
- Review: `crates/e2v-core`, `crates/e2v-sync`, `crates/e2v-api`, `crates/e2v-cli`

**Interfaces:**
- Consumes: all prior tasks.
- Produces: verified P3-A branch with a clean worktree and evidence-backed final status.

- [ ] **Step 1: Run formatting and linting**

Run: `cargo fmt --all`

Run: `cargo clippy --workspace --all-targets -- -D warnings`

Expected: PASS.

- [ ] **Step 2: Run full regression coverage**

Run: `cargo test --workspace`

Expected: PASS.

- [ ] **Step 3: Run any focused performance or maintenance checks made relevant by the rewrite flow**

Run: `cargo run --release -p e2v-sync --bin p1_c_pack_bench`

Expected: PASS and no obvious regression in pack-path execution viability.

- [ ] **Step 4: Review diff for accidental churn**

Run: `git status --short`

Run: `git diff --stat master...HEAD`

Expected: only intentional P3-A files and tests.

- [ ] **Step 5: Complete development**

Use the required `superpowers:finishing-a-development-branch` skill after all verification is green.
