# P2-B Multi-Device Sharing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add P2-B multi-device repository access, repository-wide writer-member sharing, epoch-based future revocation, and rollback-safe keyring publication while preserving the existing encrypted snapshot and sync behavior.

**Architecture:** The implementation upgrades the keyring into a recipient-and-epoch authorization document, refactors repository secrets into stable keys plus per-epoch read keys, fixes object/control decryption to honor the recorded key epoch, and then layers invite/revoke workflows plus remote keyring CAS and reconciliation on top of the existing sync foundation.

**Tech Stack:** Rust workspace, `anyhow`, `serde`, `serde_json`, existing `blake3` and `chacha20poly1305` crypto, existing ref CAS store, unit tests in `crates/e2v-core/tests`, `crates/e2v-cli/tests`, and sync integration tests in `crates/e2v-sync/tests`.

---

### Task 1: Add a failing epoch-rotation regression test

**Files:**
- Modify: `crates/e2v-core/tests/init_repository.rs`
- Test: `crates/e2v-core/tests/init_repository.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn rotated_active_epoch_keeps_old_snapshot_readable() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    let facade = RepositoryFacade::new();
    facade
        .init(InitOptions {
            repo_root: repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "main".to_string(),
        })
        .unwrap();

    fs::write(repo_root.join("tracked.txt"), "alpha").unwrap();
    let first = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "first".to_string(),
        })
        .unwrap();

    facade
        .testing_rotate_active_epoch_for_test(&repo_root)
        .unwrap();

    fs::write(repo_root.join("tracked.txt"), "beta").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "second".to_string(),
        })
        .unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(&first.snapshot_id).unwrap();
    let file = read_service.open_file(&snapshot, "tracked.txt").unwrap();
    let bytes = read_service.read_file_range(&file, 0, 64).unwrap();

    assert_eq!(String::from_utf8(bytes).unwrap(), "alpha");
}
```

- [ ] **Step 2: Run the focused test to verify RED**

Run: `cargo test -p e2v-core rotated_active_epoch_keeps_old_snapshot_readable -- --exact`
Expected: FAIL because epoch rotation helpers and epoch-aware decryption do not exist yet.

- [ ] **Step 3: Do not write production code yet**

```text
Wait until the failure clearly shows that decryption still depends on the current active epoch.
```

- [ ] **Step 4: Commit**

```bash
git add crates/e2v-core/tests/init_repository.rs
git commit -m "test: capture epoch rotation read regression"
```

### Task 2: Refactor `RepoSecrets` and object/control decryption to honor recorded key epochs

**Files:**
- Modify: `crates/e2v-store/src/logical_object_store.rs`
- Modify: `crates/e2v-core/src/facade.rs`
- Modify: `crates/e2v-core/src/lib.rs`
- Modify: `crates/e2v-core/src/manifest_store.rs`
- Modify: `crates/e2v-core/tests/init_repository.rs`
- Test: `crates/e2v-core/tests/init_repository.rs`

- [ ] **Step 1: Write the failing focused decryption tests**

```rust
#[test]
fn control_record_decryption_uses_envelope_key_epoch() {
    let (control_dir, old_ref_bytes, rotated_secrets) = seeded_rotated_control_record_fixture();
    let head = decode_default_ref_bytes_with_secrets_for_test(&control_dir, &rotated_secrets, &old_ref_bytes)
        .unwrap()
        .head_snapshot_id;
    assert!(head.is_some());
}

#[test]
fn object_decryption_uses_envelope_key_epoch() {
    let (store, old_chunk_id) = seeded_rotated_object_fixture();
    let bytes = store.get_object(&old_chunk_id, "chunk").unwrap();
    assert_eq!(bytes, b"alpha");
}
```

- [ ] **Step 2: Run the focused tests to verify RED**

Run: `cargo test -p e2v-core control_record_decryption_uses_envelope_key_epoch -- --exact`
Run: `cargo test -p e2v-store object_decryption_uses_envelope_key_epoch -- --exact`
Expected: FAIL because both paths still bind associated data to the current active epoch.

- [ ] **Step 3: Write the minimal implementation**

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpochSecrets {
    pub manifest_enc_key: [u8; 32],
    pub nonce_key: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoSecrets {
    pub repo_id: String,
    pub active_epoch: u32,
    pub repo_dedup_key: [u8; 32],
    pub repo_ref_key: [u8; 32],
    pub repo_path_index_key: [u8; 32],
    pub epoch_keys: BTreeMap<u32, EpochSecrets>,
}

impl RepoSecrets {
    pub fn active_epoch_keys(&self) -> Result<&EpochSecrets> { /* lookup active epoch */ }
    pub fn epoch_keys(&self, epoch: u32) -> Result<&EpochSecrets> { /* lookup explicit epoch */ }
}
```

- [ ] **Step 4: Update crypto callers to use active keys for encryption and envelope `key_epoch` for decryption**

```rust
let epoch_keys = self.secrets.active_epoch_keys()?;
let decrypt_keys = self.secrets.epoch_keys(envelope.key_epoch)?;
```

- [ ] **Step 5: Re-run the focused tests to verify GREEN**

Run: `cargo test -p e2v-core control_record_decryption_uses_envelope_key_epoch -- --exact`
Run: `cargo test -p e2v-store object_decryption_uses_envelope_key_epoch -- --exact`
Run: `cargo test -p e2v-core rotated_active_epoch_keeps_old_snapshot_readable -- --exact`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add crates/e2v-store/src/logical_object_store.rs crates/e2v-core/src/facade.rs crates/e2v-core/src/lib.rs crates/e2v-core/src/manifest_store.rs crates/e2v-core/tests/init_repository.rs
git commit -m "refactor: make repo decryption epoch-aware"
```

### Task 3: Upgrade the keyring schema with recipient and epoch records

**Files:**
- Modify: `crates/e2v-core/src/keyring.rs`
- Modify: `crates/e2v-core/src/facade.rs`
- Modify: `crates/e2v-core/tests/init_repository.rs`
- Modify: `crates/e2v-core/src/manifest_store.rs`
- Test: `crates/e2v-core/tests/init_repository.rs`

- [ ] **Step 1: Write the failing keyring compatibility tests**

```rust
#[test]
fn password_only_keyring_upgrades_to_epoch_keyring_on_password_rotation() {
    let repo_root = seeded_repo();
    RepositoryFacade::new()
        .change_password(
            &repo_root,
            "correct horse battery staple",
            "new horse battery staple",
        )
        .unwrap();

    let keyring: serde_json::Value = read_current_keyring_json(&repo_root);
    assert!(keyring["epochs"].is_array());
    assert!(keyring["recipient_envelopes"].is_array());
}

#[test]
fn unlocked_keyring_keeps_historical_epoch_keys_after_rotation() {
    let repo_root = seeded_repo();
    RepositoryFacade::new()
        .testing_rotate_active_epoch_for_test(&repo_root)
        .unwrap();
    let secrets = e2v_core::sync_support::unlock_repo_secrets_for_sync(
        repo_root.join(".e2v"),
        "correct horse battery staple",
    )
    .unwrap();
    assert!(secrets.epoch_keys.contains_key(&1));
    assert!(secrets.epoch_keys.contains_key(&2));
}
```

- [ ] **Step 2: Run the focused tests to verify RED**

Run: `cargo test -p e2v-core password_only_keyring_upgrades_to_epoch_keyring_on_password_rotation -- --exact`
Run: `cargo test -p e2v-core unlocked_keyring_keeps_historical_epoch_keys_after_rotation -- --exact`
Expected: FAIL because the keyring schema and sealed payload are still password-only.

- [ ] **Step 3: Write the minimal keyring schema implementation**

```rust
pub struct ActorRecord { /* actor_id, display_name, role */ }
pub struct DeviceRecord { /* device_id, actor_id, label, device_pubkey_hex, status */ }
pub struct EpochDescriptor { /* epoch, status */ }
pub enum RecipientEnvelope { /* password, device, invite */ }
pub struct AccessibleKeyBundle { /* stable keys + epoch keys + role + actor/device ids */ }
```

- [ ] **Step 4: Preserve backward compatibility when reading older password-only generations**

```rust
fn unlock_repo_secrets_from_state(keyring: &KeyringState, password: &str) -> Result<RepoSecrets> {
    if keyring.epochs.is_empty() {
        return unlock_legacy_password_only_state(keyring, password);
    }
    /* new schema path */
}
```

- [ ] **Step 5: Re-run the focused tests to verify GREEN**

Run: `cargo test -p e2v-core password_only_keyring_upgrades_to_epoch_keyring_on_password_rotation -- --exact`
Run: `cargo test -p e2v-core unlocked_keyring_keeps_historical_epoch_keys_after_rotation -- --exact`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add crates/e2v-core/src/keyring.rs crates/e2v-core/src/facade.rs crates/e2v-core/tests/init_repository.rs crates/e2v-core/src/manifest_store.rs
git commit -m "feat: add epoch-aware keyring schema"
```

### Task 4: Add device and member authorization operations in `e2v-core`

**Files:**
- Modify: `crates/e2v-core/src/facade.rs`
- Modify: `crates/e2v-core/src/keyring.rs`
- Modify: `crates/e2v-core/src/lib.rs`
- Modify: `crates/e2v-core/tests/init_repository.rs`
- Test: `crates/e2v-core/tests/init_repository.rs`

- [ ] **Step 1: Write the failing authorization tests**

```rust
#[test]
fn invite_member_accept_member_creates_writer_member_device() {
    let repo_root = seeded_repo();
    let facade = RepositoryFacade::new();

    let invite = facade
        .share_invite_member(&repo_root, ShareInviteMemberOptions {
            display_name: "Alice".to_string(),
        })
        .unwrap();

    let accepted = facade
        .share_accept_member(&repo_root, ShareAcceptMemberOptions {
            invite_bytes: invite.bundle_bytes.clone(),
            local_device_label: "alice-laptop".to_string(),
        })
        .unwrap();

    assert_eq!(accepted.role, "writer_member");
    assert!(!accepted.device_id.is_empty());
}

#[test]
fn revoke_member_advances_active_epoch_and_removes_member_envelope() {
    let repo_root = seeded_shared_repo();
    let before = read_current_keyring_json(&repo_root)["active_epoch"].as_u64().unwrap();

    RepositoryFacade::new()
        .share_revoke_member(&repo_root, "member-actor-id")
        .unwrap();

    let after = read_current_keyring_json(&repo_root);
    assert_eq!(after["active_epoch"].as_u64(), Some(before + 1));
    assert!(!keyring_contains_actor_envelope(&after, "member-actor-id"));
}
```

- [ ] **Step 2: Run the focused tests to verify RED**

Run: `cargo test -p e2v-core invite_member_accept_member_creates_writer_member_device -- --exact`
Run: `cargo test -p e2v-core revoke_member_advances_active_epoch_and_removes_member_envelope -- --exact`
Expected: FAIL because share operations do not exist.

- [ ] **Step 3: Implement the minimal core APIs**

```rust
pub struct ShareInviteMemberOptions { pub display_name: String }
pub struct ShareAcceptMemberOptions { pub invite_bytes: Vec<u8>, pub local_device_label: String }
pub struct ShareInviteDeviceOptions { pub actor_id: String, pub device_label: String }

impl RepositoryFacade {
    pub fn share_invite_member(&self, repo_root: impl AsRef<Path>, options: ShareInviteMemberOptions) -> Result<ShareInviteBundle> { /* generate bundle */ }
    pub fn share_accept_member(&self, repo_root: impl AsRef<Path>, options: ShareAcceptMemberOptions) -> Result<AcceptedShare> { /* import bundle */ }
    pub fn share_revoke_member(&self, repo_root: impl AsRef<Path>, actor_id: &str) -> Result<()> { /* remove envelopes + rotate epoch */ }
}
```

- [ ] **Step 4: Re-run the focused tests to verify GREEN**

Run: `cargo test -p e2v-core invite_member_accept_member_creates_writer_member_device -- --exact`
Run: `cargo test -p e2v-core revoke_member_advances_active_epoch_and_removes_member_envelope -- --exact`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/e2v-core/src/facade.rs crates/e2v-core/src/keyring.rs crates/e2v-core/src/lib.rs crates/e2v-core/tests/init_repository.rs
git commit -m "feat: add core device and member sharing flows"
```

### Task 5: Add remote keyring CAS and reconciliation behavior

**Files:**
- Modify: `crates/e2v-sync/src/push.rs`
- Modify: `crates/e2v-sync/src/fetch.rs`
- Modify: `crates/e2v-sync/src/lib.rs`
- Modify: `crates/e2v-sync/tests/fetch_clone.rs`
- Modify: `crates/e2v-sync/tests/push_remote.rs`
- Test: `crates/e2v-sync/tests/push_remote.rs`
- Test: `crates/e2v-sync/tests/fetch_clone.rs`

- [ ] **Step 1: Write the failing remote CAS tests**

```rust
#[test]
fn concurrent_keyring_publish_retries_after_remote_pointer_cas_failure() {
    let fixture = seeded_remote_with_two_admins();
    let result = push_share_mutation_after_concurrent_remote_keyring_change(&fixture).unwrap();
    assert!(result.keyring_retried);
}

#[test]
fn revoke_wins_over_concurrent_member_add_during_reconciliation() {
    let fixture = seeded_remote_reconcile_conflict();
    let error = push_conflicting_share_mutation(&fixture).unwrap_err();
    assert!(error.to_string().contains("manual resolution"));
}
```

- [ ] **Step 2: Run the focused tests to verify RED**

Run: `cargo test -p e2v-sync concurrent_keyring_publish_retries_after_remote_pointer_cas_failure -- --exact`
Run: `cargo test -p e2v-sync revoke_wins_over_concurrent_member_add_during_reconciliation -- --exact`
Expected: FAIL because keyring publication still writes the raw pointer file directly.

- [ ] **Step 3: Implement the minimal remote keyring pointer ref**

```rust
fn keyring_pointer_ref_token(control_dir: &Path) -> Result<RefToken> { /* deterministic repository-stable token */ }

fn publish_remote_keyring_pointer_via_ref_cas<R: RemoteBackend>(
    remote: &R,
    token: &RefToken,
    expected: Option<RefVersion>,
    pointer_bytes: Vec<u8>,
) -> Result<CasResult> { /* use compare_and_swap_ref */ }
```

- [ ] **Step 4: Add reconcile-and-retry loop**

```rust
for _attempt in 0..MAX_KEYRING_RETRIES {
    match try_publish_keyring_update(...) {
        Ok(outcome) => return Ok(outcome),
        Err(KeyringPublishError::Conflict(remote_latest)) => {
            local_state = reconcile_keyring_states(local_state, remote_latest)?;
            continue;
        }
    }
}
```

- [ ] **Step 5: Re-run the focused tests to verify GREEN**

Run: `cargo test -p e2v-sync concurrent_keyring_publish_retries_after_remote_pointer_cas_failure -- --exact`
Run: `cargo test -p e2v-sync revoke_wins_over_concurrent_member_add_during_reconciliation -- --exact`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add crates/e2v-sync/src/push.rs crates/e2v-sync/src/fetch.rs crates/e2v-sync/src/lib.rs crates/e2v-sync/tests/fetch_clone.rs crates/e2v-sync/tests/push_remote.rs
git commit -m "feat: add remote keyring cas and reconciliation"
```

### Task 6: Add CLI share commands

**Files:**
- Modify: `crates/e2v-cli/src/lib.rs`
- Modify: `crates/e2v-cli/tests/cli.rs`
- Test: `crates/e2v-cli/tests/cli.rs`

- [ ] **Step 1: Write the failing CLI tests**

```rust
#[test]
fn share_list_prints_member_and_device_records() {
    let repo_root = seeded_shared_repo();
    let output = e2v_cli::run_cli_for_test([
        "e2v",
        "share",
        "--repo",
        repo_root.to_str().unwrap(),
        "list",
    ])
    .unwrap();
    assert!(output.contains("owner_admin"));
}

#[test]
fn share_invite_member_and_accept_member_round_trip_via_bundle_file() {
    let fixture = seeded_cli_share_repo();
    let bundle_path = fixture.temp.path().join("invite.e2vshare");
    let invite_output = e2v_cli::run_cli_for_test([
        "e2v",
        "share",
        "--repo",
        fixture.repo_root.to_str().unwrap(),
        "invite-member",
        "--name",
        "Alice",
        "--out",
        bundle_path.to_str().unwrap(),
    ])
    .unwrap();
    assert!(invite_output.contains("invite"));
    assert!(bundle_path.is_file());
}
```

- [ ] **Step 2: Run the focused tests to verify RED**

Run: `cargo test -p e2v-cli share_list_prints_member_and_device_records -- --exact`
Run: `cargo test -p e2v-cli share_invite_member_and_accept_member_round_trip_via_bundle_file -- --exact`
Expected: FAIL because the `share` command group does not exist.

- [ ] **Step 3: Implement the minimal CLI routing**

```rust
enum Command {
    Share { #[command(subcommand)] command: ShareCommand, #[arg(long)] repo: PathBuf },
}

enum ShareCommand {
    List,
    InviteMember { #[arg(long = "name")] name: String, #[arg(long = "out")] out: PathBuf },
    AcceptMember { #[arg(long = "bundle")] bundle: PathBuf, #[arg(long = "label")] label: String },
    RevokeMember { #[arg(long = "actor")] actor: String },
}
```

- [ ] **Step 4: Re-run the focused tests to verify GREEN**

Run: `cargo test -p e2v-cli share_list_prints_member_and_device_records -- --exact`
Run: `cargo test -p e2v-cli share_invite_member_and_accept_member_round_trip_via_bundle_file -- --exact`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/e2v-cli/src/lib.rs crates/e2v-cli/tests/cli.rs
git commit -m "feat: add share cli commands"
```

### Task 7: Add rollback and revoked-future-access sync coverage

**Files:**
- Modify: `crates/e2v-sync/tests/fetch_clone.rs`
- Modify: `crates/e2v-sync/src/fetch.rs`
- Test: `crates/e2v-sync/tests/fetch_clone.rs`

- [ ] **Step 1: Write the failing sync tests**

```rust
#[test]
fn revoked_member_cannot_decrypt_future_remote_head_after_fetch() {
    let fixture = seeded_revoked_member_remote();
    let error = fetch_remote(
        &fixture.remote,
        FetchOptions {
            repo_root: fixture.revoked_clone_root.clone(),
            branch_token: fixture.branch_token.clone(),
            password: None,
        },
    )
    .unwrap_err();

    assert!(error.to_string().contains("missing epoch keys"));
}

#[test]
fn remote_keyring_generation_rollback_via_pointer_ref_is_rejected() {
    let fixture = seeded_keyring_pointer_rollback_remote();
    let error = clone_remote(&fixture.remote, fixture.clone_options()).unwrap_err();
    assert!(error.to_string().contains("CRITICAL_ROLLBACK_DETECTED"));
}
```

- [ ] **Step 2: Run the focused tests to verify RED**

Run: `cargo test -p e2v-sync revoked_member_cannot_decrypt_future_remote_head_after_fetch -- --exact`
Run: `cargo test -p e2v-sync remote_keyring_generation_rollback_via_pointer_ref_is_rejected -- --exact`
Expected: FAIL because revoked device/member epoch denial and pointer-ref rollback checks are incomplete.

- [ ] **Step 3: Implement the minimal sync checks**

```rust
ensure!(
    secrets.epoch_keys.contains_key(&required_epoch),
    "missing epoch keys for remote generation"
);
```

- [ ] **Step 4: Re-run the focused tests to verify GREEN**

Run: `cargo test -p e2v-sync revoked_member_cannot_decrypt_future_remote_head_after_fetch -- --exact`
Run: `cargo test -p e2v-sync remote_keyring_generation_rollback_via_pointer_ref_is_rejected -- --exact`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/e2v-sync/tests/fetch_clone.rs crates/e2v-sync/src/fetch.rs
git commit -m "test: cover revoked future access and keyring rollback"
```

### Task 8: Final verification

**Files:**
- Modify: any touched files from prior tasks

- [ ] **Step 1: Run focused core tests**

Run: `cargo test -p e2v-core`
Expected: PASS

- [ ] **Step 2: Run focused sync tests**

Run: `cargo test -p e2v-sync`
Expected: PASS

- [ ] **Step 3: Run focused CLI tests**

Run: `cargo test -p e2v-cli`
Expected: PASS

- [ ] **Step 4: Run workspace verification**

Run: `cargo test --workspace`
Expected: PASS

- [ ] **Step 5: Review the diff for accidental churn and keep only intentional P2-B changes**
