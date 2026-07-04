use std::fs;
use std::io::Write;

use anyhow::Result;
use e2v_core::testing::{
    StableReadPolicy, TestSnapshotReader, with_snapshot_reader_and_policy_for_test,
    with_snapshot_reader_for_test, with_stable_read_policy_for_test,
};
use e2v_core::{
    CheckoutOptions, CommitOptions, InitOptions, ManifestObject, ManifestSnapshotObject,
    ManifestStore, ManifestStoreApi, ManifestTreeObject, RepositoryFacade, TreeWalkEntry,
};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tempfile::tempdir;

fn init_options(repo_root: &std::path::Path) -> InitOptions {
    InitOptions {
        repo_root: repo_root.to_path_buf(),
        password: "correct horse battery staple".to_string(),
        branch_name: "main".to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RefRecordMirror {
    branch_name: String,
    ref_token_hex: String,
    head_snapshot_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CheckoutPathMappingMirror {
    snapshot_path: String,
    local_path: String,
}

trait StreamingTreeWalkTestExt {
    fn walk_tree_streaming_for_test(
        &self,
        snapshot_id: &str,
    ) -> Result<Box<dyn Iterator<Item = Result<TreeWalkEntry>>>>;
}

impl StreamingTreeWalkTestExt for ManifestStore {
    fn walk_tree_streaming_for_test(
        &self,
        snapshot_id: &str,
    ) -> Result<Box<dyn Iterator<Item = Result<TreeWalkEntry>>>> {
        Ok(Box::new(self.walk_tree_iter(snapshot_id)?))
    }
}

#[test]
fn facade_test_helpers_are_not_exposed_as_public_api_functions() {
    let source = fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("facade.rs"),
    )
    .unwrap();

    for legacy_helper in [
        "pub fn override_max_file_chunks_per_object_for_test",
        "pub fn rotate_active_epoch_for_test",
        "pub fn unlock_with_local_device_for_test",
    ] {
        assert!(
            !source.contains(legacy_helper),
            "test-only facade helper should not remain public: {legacy_helper}"
        );
    }
}

#[test]
fn facade_module_is_not_exposed_as_a_public_crate_module() {
    let source = fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("lib.rs"),
    )
    .unwrap();

    assert!(
        !source.contains("pub mod facade;"),
        "e2v-core should expose stable root re-exports instead of a public facade module"
    );
}

#[test]
fn facade_exposes_history_rewrite_api_for_p3_a() {
    let source = fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("facade.rs"),
    )
    .unwrap();

    assert!(
        source.contains("pub struct HistoryRewriteLocalResult"),
        "expected facade to expose a local history rewrite result type for P3-A"
    );
    assert!(
        source.contains("pub fn rewrite_history_to_active_epoch("),
        "expected facade to expose a history rewrite entrypoint for P3-A"
    );
}

#[test]
fn testing_module_does_not_reexport_core_facade_types_or_sync_reconcile_directly() {
    let source = fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("lib.rs"),
    )
    .unwrap();

    for legacy_reexport in [
        "pub use crate::facade::RepositoryFacade;",
        "pub use crate::facade::reconcile_remote_keyring_for_sync;",
    ] {
        assert!(
            !source.contains(legacy_reexport),
            "e2v-core::testing should not directly re-export internal facade items: {legacy_reexport}"
        );
    }
}

#[test]
fn testing_module_wraps_chunker_and_keyring_test_helpers_instead_of_reexporting_them() {
    let source = fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("lib.rs"),
    )
    .unwrap();

    for legacy_reexport in [
        "pub use crate::chunker::override_fixed_span_bytes_for_test;",
        "pub use crate::keyring::clear_unlocked_keyring_cache_for_test;",
    ] {
        assert!(
            !source.contains(legacy_reexport),
            "e2v-core::testing should wrap internal test helpers instead of directly re-exporting them: {legacy_reexport}"
        );
    }
}

#[test]
fn sync_support_module_is_kept_doc_hidden_as_an_internal_sync_boundary() {
    let source = fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("lib.rs"),
    )
    .unwrap();
    let lines = source.lines().collect::<Vec<_>>();
    let sync_support_line = lines
        .iter()
        .position(|line| line.trim() == "pub mod sync_support {")
        .expect("expected sync_support module declaration");
    let previous_non_empty = lines[..sync_support_line]
        .iter()
        .rev()
        .find(|line| !line.trim().is_empty())
        .copied();

    assert_eq!(
        previous_non_empty,
        Some("#[doc(hidden)]"),
        "e2v-core::sync_support should stay doc-hidden because it is an internal sync boundary rather than the stable public read/write API"
    );
}

#[test]
fn manifest_store_source_does_not_expect_last_entry_after_non_empty_check() {
    let source = fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("manifest_store.rs"),
    )
    .unwrap();

    assert!(
        !source.contains("expect(\"first entry implies last entry\")"),
        "manifest store should validate shard range metadata without relying on panic-based last-entry assumptions"
    );
}

#[test]
fn init_creates_control_plane_files_for_local_direct_layout() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    let state = facade.init(init_options(&repo_root)).unwrap();

    assert_eq!(state.layout_generation, 1);
    assert_eq!(state.branch.name, "main");
    assert!(!state.branch.token_hex.is_empty());

    let e2v_dir = repo_root.join(".e2v");
    assert!(e2v_dir.exists(), "expected control directory to exist");
    assert!(
        e2v_dir.join("objects").is_dir(),
        "expected objects directory"
    );
    assert!(
        e2v_dir.join("journal").is_dir(),
        "expected journal directory"
    );
    assert!(
        e2v_dir.join("layout_root.json").is_file(),
        "expected layout root file"
    );
    assert!(
        e2v_dir.join("refs").join("default.json").is_file(),
        "expected default ref file"
    );
    assert!(
        e2v_dir.join("keyring").join("keyring.current").is_file(),
        "expected current keyring pointer"
    );
    assert!(
        e2v_dir.join("keyring").join("keyring.1").is_file(),
        "expected first keyring generation"
    );
    let leftover_temps = fs::read_dir(e2v_dir.join("keyring"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
        .filter(|name| name.ends_with(".tmp"))
        .collect::<Vec<_>>();
    assert!(
        leftover_temps.is_empty(),
        "expected no leftover temp files, found {leftover_temps:?}"
    );

    let control_plane_leftovers = fs::read_dir(&e2v_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
        .filter(|name| name.ends_with(".tmp"))
        .collect::<Vec<_>>();
    assert!(
        control_plane_leftovers.is_empty(),
        "expected no leftover control-plane temp files, found {control_plane_leftovers:?}"
    );
}

#[test]
fn init_persists_openable_repository_state() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    let created = facade.init(init_options(&repo_root)).unwrap();
    let opened = facade.open(&repo_root).unwrap();

    assert_eq!(opened, created);
}

#[test]
fn legacy_config_file_is_absent_after_init() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();

    assert!(
        !repo_root.join(".e2v").join("config.json").exists(),
        "legacy config.json should not be created"
    );
}

#[test]
fn init_does_not_create_redundant_config_file() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();

    assert!(
        !repo_root.join(".e2v").join("config.json").exists(),
        "init should not create redundant config.json once ref/layout/keyring are the only control-plane sources of truth"
    );
}

#[test]
fn open_rejects_default_ref_without_branch_name_instead_of_falling_back_to_config() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();

    let control_dir = repo_root.join(".e2v");
    let secrets = e2v_core::sync_support::open_repo_secrets_for_sync(&control_dir).unwrap();
    let default_ref_path = control_dir.join("refs").join("default.json");
    let default_ref_bytes = fs::read(&default_ref_path).unwrap();
    let plaintext = e2v_core::sync_support::decrypt_control_record_for_sync(
        &secrets,
        "default",
        "ref",
        &default_ref_bytes,
    )
    .unwrap();
    let mut record: RefRecordMirror = postcard::from_bytes(&plaintext).unwrap();
    record.branch_name.clear();
    let rewritten_plaintext = postcard::to_stdvec(&record).unwrap();
    let rewritten_bytes = e2v_core::sync_support::encrypt_control_record_for_sync(
        &secrets,
        "default",
        "ref",
        &rewritten_plaintext,
    )
    .unwrap();
    fs::write(&default_ref_path, rewritten_bytes).unwrap();

    let error = facade.open(&repo_root).unwrap_err();

    assert!(
        error.to_string().contains("branch")
            || error.to_string().contains("default ref")
            || error.to_string().contains("ref record"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn keyring_generation_does_not_store_plaintext_repo_keys() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();

    let keyring_bytes = fs::read(repo_root.join(".e2v").join("keyring").join("keyring.1")).unwrap();
    let keyring_text = String::from_utf8_lossy(&keyring_bytes);

    assert!(!keyring_text.contains("repo_dedup_key_hex"));
    assert!(!keyring_text.contains("repo_ref_key_hex"));
    assert!(!keyring_text.contains("repo_manifest_enc_key_hex"));
    assert!(!keyring_text.contains("repo_nonce_key_hex"));
    assert!(!keyring_text.contains("repo_path_index_key_hex"));
}

#[test]
fn init_persists_active_epoch_key_maps_in_latest_keyring_format() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    e2v_core::testing::rotate_active_epoch_for_test(&repo_root, "correct horse battery staple")
        .unwrap();

    let keyring = read_current_keyring_json(&repo_root);
    let active_epoch = keyring["active_epoch"].as_u64().unwrap().to_string();
    let epochs = keyring["epochs"].as_array().unwrap();

    assert_eq!(keyring["generation"].as_u64(), Some(2));
    assert!(epochs.iter().any(|epoch| {
        epoch["epoch"].as_u64() == Some(1) && epoch["status"].as_str() == Some("retired")
    }));
    assert!(epochs.iter().any(|epoch| {
        epoch["epoch"].as_u64() == Some(2) && epoch["status"].as_str() == Some("active")
    }));

    let secrets = e2v_core::sync_support::unlock_repo_secrets_for_sync(
        repo_root.join(".e2v"),
        "correct horse battery staple",
    )
    .unwrap();
    assert!(
        secrets.epoch_keys.contains_key(&1),
        "latest keyring should retain prior epoch keys after rotation"
    );
    assert!(
        secrets.epoch_keys.contains_key(
            &active_epoch
                .parse::<u32>()
                .expect("active epoch should fit in u32")
        ),
        "latest keyring should include active epoch keys"
    );
}

#[test]
fn wrong_password_cannot_unlock_keyring() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();

    let error = facade
        .unlock(&repo_root, "totally wrong password")
        .unwrap_err();

    assert!(
        error.to_string().contains("password")
            || error.to_string().contains("unlock")
            || error.to_string().contains("keyring"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn change_password_rotates_keyring_generation_and_requires_new_password() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    let created = facade.init(init_options(&repo_root)).unwrap();

    facade
        .change_password(
            &repo_root,
            "correct horse battery staple",
            "new horse battery staple",
        )
        .unwrap();

    let keyring_dir = repo_root.join(".e2v").join("keyring");
    assert!(keyring_dir.join("keyring.1").is_file());
    assert!(keyring_dir.join("keyring.2").is_file());
    let pointer: serde_json::Value =
        serde_json::from_slice(&fs::read(keyring_dir.join("keyring.current")).unwrap()).unwrap();
    assert_eq!(pointer["current"].as_str(), Some("keyring.2"));

    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&repo_root.join(".e2v"));
    let old_error = facade
        .unlock(&repo_root, "correct horse battery staple")
        .unwrap_err();
    assert!(
        old_error.to_string().contains("wrong password")
            || old_error.to_string().contains("unlock")
            || old_error.to_string().contains("keyring"),
        "unexpected error: {old_error:#}"
    );

    let reopened = facade
        .unlock(&repo_root, "new horse battery staple")
        .unwrap();

    assert_eq!(reopened.repo_root, created.repo_root);
    assert_eq!(reopened.branch.name, created.branch.name);
}

#[test]
fn repo_path_index_key_survives_keyring_round_trip() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();

    let control_dir = repo_root.join(".e2v");
    let original = e2v_core::sync_support::open_repo_secrets_for_sync(&control_dir).unwrap();
    assert_ne!(original.repo_path_index_key, [0u8; 32]);

    facade
        .change_password(
            &repo_root,
            "correct horse battery staple",
            "new horse battery staple",
        )
        .unwrap();

    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&control_dir);
    let reopened = facade
        .unlock(&repo_root, "new horse battery staple")
        .unwrap();
    let reopened_secrets =
        e2v_core::sync_support::open_repo_secrets_for_sync(&control_dir).unwrap();

    assert_eq!(reopened.branch.name, "main");
    assert_eq!(
        reopened_secrets.repo_path_index_key,
        original.repo_path_index_key
    );
}

#[test]
fn change_password_rejects_wrong_old_password_without_side_effects() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();

    let error = facade
        .change_password(
            &repo_root,
            "definitely wrong password",
            "new horse battery staple",
        )
        .unwrap_err();

    assert!(
        error.to_string().contains("wrong password")
            || error.to_string().contains("unlock")
            || error.to_string().contains("keyring"),
        "unexpected error: {error:#}"
    );
    assert!(
        !repo_root
            .join(".e2v")
            .join("keyring")
            .join("keyring.2")
            .exists()
    );
    assert!(
        !repo_root
            .join(".e2v")
            .join("journal")
            .join("keyring-update.json")
            .exists()
    );
}

#[test]
fn unlock_recovers_interrupted_keyring_pointer_publish() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();

    let keyring_current = repo_root
        .join(".e2v")
        .join("keyring")
        .join("keyring.current");
    let journal_path = repo_root
        .join(".e2v")
        .join("journal")
        .join("keyring-update.json");
    let pointer_one: serde_json::Value =
        serde_json::from_slice(&fs::read(&keyring_current).unwrap()).unwrap();
    assert_eq!(pointer_one["current"].as_str(), Some("keyring.1"));
    let facade_for_change = RepositoryFacade::new();
    facade_for_change
        .change_password(
            &repo_root,
            "correct horse battery staple",
            "new horse battery staple",
        )
        .unwrap();
    fs::write(
        &keyring_current,
        serde_json::to_vec_pretty(&serde_json::json!({
            "generation": 1u64,
            "current": "keyring.1"
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        &journal_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "generation": 2u64,
            "current": "keyring.2",
            "stage": "writing_pointer"
        }))
        .unwrap(),
    )
    .unwrap();

    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&repo_root.join(".e2v"));

    let reopened = facade
        .unlock(&repo_root, "new horse battery staple")
        .unwrap();

    let pointer: serde_json::Value =
        serde_json::from_slice(&fs::read(&keyring_current).unwrap()).unwrap();
    assert_eq!(pointer["current"].as_str(), Some("keyring.2"));
    assert!(!journal_path.exists());
    assert_eq!(reopened.branch.name, "main");
}

#[test]
fn unlock_recovers_when_generation_exists_but_journal_is_still_writing_generation() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();

    let keyring_current = repo_root
        .join(".e2v")
        .join("keyring")
        .join("keyring.current");
    let journal_path = repo_root
        .join(".e2v")
        .join("journal")
        .join("keyring-update.json");
    let pointer_one: serde_json::Value =
        serde_json::from_slice(&fs::read(&keyring_current).unwrap()).unwrap();
    assert_eq!(pointer_one["current"].as_str(), Some("keyring.1"));

    RepositoryFacade::new()
        .change_password(
            &repo_root,
            "correct horse battery staple",
            "new horse battery staple",
        )
        .unwrap();

    fs::write(
        &keyring_current,
        serde_json::to_vec_pretty(&serde_json::json!({
            "generation": 1u64,
            "current": "keyring.1"
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        &journal_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "generation": 2u64,
            "current": "keyring.2",
            "stage": "writing_generation"
        }))
        .unwrap(),
    )
    .unwrap();

    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&repo_root.join(".e2v"));

    let reopened = facade
        .unlock(&repo_root, "new horse battery staple")
        .unwrap();

    let pointer: serde_json::Value =
        serde_json::from_slice(&fs::read(&keyring_current).unwrap()).unwrap();
    assert_eq!(pointer["current"].as_str(), Some("keyring.2"));
    assert!(!journal_path.exists());
    assert_eq!(reopened.branch.name, "main");
}

#[test]
fn unlock_recovery_uses_keyring_file_generation_instead_of_tampered_journal_generation() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();

    let keyring_current = repo_root
        .join(".e2v")
        .join("keyring")
        .join("keyring.current");
    let journal_path = repo_root
        .join(".e2v")
        .join("journal")
        .join("keyring-update.json");

    RepositoryFacade::new()
        .change_password(
            &repo_root,
            "correct horse battery staple",
            "new horse battery staple",
        )
        .unwrap();

    fs::write(
        &keyring_current,
        serde_json::to_vec_pretty(&serde_json::json!({
            "generation": 1u64,
            "current": "keyring.1"
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        &journal_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "generation": 999u64,
            "current": "keyring.2",
            "stage": "writing_pointer"
        }))
        .unwrap(),
    )
    .unwrap();

    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&repo_root.join(".e2v"));

    let reopened = facade
        .unlock(&repo_root, "new horse battery staple")
        .unwrap();

    let pointer: serde_json::Value =
        serde_json::from_slice(&fs::read(&keyring_current).unwrap()).unwrap();
    assert_eq!(pointer["current"].as_str(), Some("keyring.2"));
    assert_eq!(
        pointer["generation"].as_u64(),
        Some(2),
        "recovery should rewrite the pointer with the actual recovered keyring generation"
    );
    assert!(!journal_path.exists());
    assert_eq!(reopened.branch.name, "main");

    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&repo_root.join(".e2v"));
    let reopened_again = facade
        .unlock(&repo_root, "new horse battery staple")
        .unwrap();
    assert_eq!(reopened_again.branch.name, "main");
}

#[test]
fn open_restores_access_after_fresh_process_lock_state_via_local_device() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    let created = facade.init(init_options(&repo_root)).unwrap();
    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&repo_root.join(".e2v"));

    let reopened = facade.open(&repo_root).unwrap();
    assert_eq!(reopened, created);

    let unlocked = facade
        .unlock(&repo_root, "correct horse battery staple")
        .unwrap();
    assert_eq!(unlocked, created);
}

#[test]
fn local_device_unlock_and_open_restore_access_without_password_after_cache_clear() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    let created = facade.init(init_options(&repo_root)).unwrap();
    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&repo_root.join(".e2v"));

    let reopened = facade.open(&repo_root).unwrap();
    assert_eq!(reopened, created);

    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&repo_root.join(".e2v"));
    let unlocked = e2v_core::testing::unlock_with_local_device_for_test(&repo_root).unwrap();
    assert_eq!(unlocked, created);
}

#[test]
fn local_device_unlock_survives_password_rotation() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    let created = facade.init(init_options(&repo_root)).unwrap();

    facade
        .change_password(
            &repo_root,
            "correct horse battery staple",
            "new horse battery staple",
        )
        .unwrap();

    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&repo_root.join(".e2v"));
    let reopened = e2v_core::testing::unlock_with_local_device_for_test(&repo_root).unwrap();

    assert_eq!(reopened, created);
}

#[test]
fn local_device_unlock_survives_epoch_rotation() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    let created = facade.init(init_options(&repo_root)).unwrap();

    e2v_core::testing::rotate_active_epoch_for_test(&repo_root, "correct horse battery staple")
        .unwrap();

    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&repo_root.join(".e2v"));
    let reopened = e2v_core::testing::unlock_with_local_device_for_test(&repo_root).unwrap();

    assert_eq!(reopened, created);
}

#[test]
fn commit_restores_access_via_local_device_after_cache_clear() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("tracked.txt"), "alpha").unwrap();
    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&repo_root.join(".e2v"));

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "fresh-process commit".to_string(),
        })
        .unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(&commit.snapshot_id).unwrap();
    let file = read_service.open_file(&snapshot, "tracked.txt").unwrap();
    let bytes = read_service.read_range(&file, 0, 64).unwrap();

    assert_eq!(String::from_utf8(bytes).unwrap(), "alpha");
}

#[test]
fn read_service_restores_access_via_local_device_after_cache_clear_between_steps() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("tracked.txt"), "alpha").unwrap();
    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "fresh-process read service".to_string(),
        })
        .unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&repo_root.join(".e2v"));
    let snapshot = read_service.open_snapshot(&commit.snapshot_id).unwrap();

    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&repo_root.join(".e2v"));
    let entries = read_service.read_dir(&snapshot, "").unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "tracked.txt");

    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&repo_root.join(".e2v"));
    let file = read_service.open_file(&snapshot, "tracked.txt").unwrap();

    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&repo_root.join(".e2v"));
    let bytes = read_service.read_range(&file, 0, 64).unwrap();
    assert_eq!(String::from_utf8(bytes).unwrap(), "alpha");
}

#[test]
fn explicit_snapshot_reads_do_not_require_the_default_ref_to_exist() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("tracked.txt"), "alpha").unwrap();
    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "explicit snapshot".to_string(),
        })
        .unwrap();

    let default_ref_path = repo_root.join(".e2v").join("refs").join("default.json");
    fs::remove_file(default_ref_path).unwrap();
    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&repo_root.join(".e2v"));

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(&commit.snapshot_id).unwrap();
    let entries = read_service.read_dir(&snapshot, "").unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "tracked.txt");

    let file = read_service.open_file(&snapshot, "tracked.txt").unwrap();
    let bytes = read_service.read_range(&file, 0, 64).unwrap();
    assert_eq!(String::from_utf8(bytes).unwrap(), "alpha");
}

#[test]
fn explicit_branch_resolution_does_not_require_the_default_ref_to_exist() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    let created = facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("tracked.txt"), "alpha").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "explicit branch".to_string(),
        })
        .unwrap();

    let default_ref_path = repo_root.join(".e2v").join("refs").join("default.json");
    fs::remove_file(default_ref_path).unwrap();
    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&repo_root.join(".e2v"));

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service
        .resolve_branch(&created.branch.token_hex)
        .unwrap();
    let file = read_service.open_file(&snapshot, "tracked.txt").unwrap();
    let bytes = read_service.read_range(&file, 0, 64).unwrap();
    assert_eq!(String::from_utf8(bytes).unwrap(), "alpha");
}

#[test]
fn create_branch_restores_access_via_local_device_after_cache_clear() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&repo_root.join(".e2v"));

    let branch = facade.create_branch(&repo_root, "feature/latest").unwrap();
    let branches = facade.list_branches(&repo_root).unwrap();

    assert_eq!(branch.name, "feature/latest");
    assert!(
        branches
            .iter()
            .any(|entry| entry.name == "feature/latest" && !entry.is_current)
    );
}

#[test]
fn list_checkout_and_delete_branch_restore_access_via_local_device_after_cache_clear() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    facade.create_branch(&repo_root, "feature/latest").unwrap();

    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&repo_root.join(".e2v"));
    let listed = facade.list_branches(&repo_root).unwrap();
    assert!(listed.iter().any(|entry| entry.name == "feature/latest"));

    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&repo_root.join(".e2v"));
    let checked_out = facade
        .checkout_branch(&repo_root, "feature/latest")
        .unwrap();
    assert_eq!(checked_out.branch.name, "feature/latest");

    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&repo_root.join(".e2v"));
    facade.checkout_branch(&repo_root, "main").unwrap();
    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&repo_root.join(".e2v"));
    facade.delete_branch(&repo_root, "feature/latest").unwrap();

    let remaining = facade.list_branches(&repo_root).unwrap();
    assert!(!remaining.iter().any(|entry| entry.name == "feature/latest"));
}

#[test]
fn commit_prefers_snapshot_reader_for_large_file_input() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let snapshot_bytes = vec![b'S'; 9 * 1024 * 1024];
    let disk_bytes = vec![b'D'; 9 * 1024 * 1024];
    let calls = Arc::new(Mutex::new(Vec::new()));
    let facade = with_snapshot_reader_for_test(Arc::new(TestSnapshotReader {
        result: Ok(snapshot_bytes.clone()),
        calls: Arc::clone(&calls),
    }));
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("large.txt"), &disk_bytes).unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "snapshot-preferred".to_string(),
        })
        .unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(&commit.snapshot_id).unwrap();
    let file = read_service.open_file(&snapshot, "large.txt").unwrap();
    let content = read_service
        .read_range(&file, 0, snapshot_bytes.len())
        .unwrap();

    assert_eq!(content, snapshot_bytes);
    assert_eq!(calls.lock().unwrap().len(), 1);
}

#[test]
fn commit_falls_back_to_disk_when_snapshot_reader_fails() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let snapshot_bytes = vec![b'S'; 9 * 1024 * 1024];
    let disk_bytes = vec![b'D'; 9 * 1024 * 1024];
    let calls = Arc::new(Mutex::new(Vec::new()));
    let facade = with_snapshot_reader_for_test(Arc::new(TestSnapshotReader {
        result: Err("snapshot unavailable".to_string()),
        calls: Arc::clone(&calls),
    }));
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("large.txt"), &disk_bytes).unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "snapshot-fallback".to_string(),
        })
        .unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(&commit.snapshot_id).unwrap();
    let file = read_service.open_file(&snapshot, "large.txt").unwrap();
    let content = read_service.read_range(&file, 0, disk_bytes.len()).unwrap();

    assert_eq!(content, disk_bytes);
    assert_ne!(content, snapshot_bytes);
    assert_eq!(calls.lock().unwrap().len(), 1);
}

#[test]
fn commit_honors_custom_volatile_retry_budget_on_real_unstable_file() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = with_stable_read_policy_for_test(StableReadPolicy {
        metadata_retry_attempts: 2,
        volatile_retry_attempts: 1,
    });
    facade.init(init_options(&repo_root)).unwrap();

    let file_path = repo_root.join("large.txt");
    fs::write(&file_path, vec![b'A'; 9 * 1024 * 1024]).unwrap();
    let keep_writing = Arc::new(AtomicBool::new(true));
    let writer_started = Arc::new(AtomicBool::new(false));
    let writer_flag = Arc::clone(&keep_writing);
    let started_flag = Arc::clone(&writer_started);
    let writer_path = file_path.clone();
    let writer = std::thread::spawn(move || {
        use std::io::{Seek, SeekFrom};

        let mut block_index = 0usize;
        let block_size = 64 * 1024usize;
        let block_count = (9 * 1024 * 1024) / block_size;
        while writer_flag.load(Ordering::SeqCst) {
            let fill = if block_index.is_multiple_of(2) {
                b'B'
            } else {
                b'C'
            };
            if let Ok(mut file) = fs::OpenOptions::new().write(true).open(&writer_path) {
                let offset = ((block_index % block_count) * block_size) as u64;
                if file.seek(SeekFrom::Start(offset)).is_err() {
                    break;
                }
                let chunk = vec![fill; block_size];
                if file.write_all(&chunk).is_err() {
                    break;
                }
                if file.flush().is_err() {
                    break;
                }
                started_flag.store(true, Ordering::SeqCst);
                block_index += 1;
                std::thread::sleep(Duration::from_millis(1));
            } else {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    while !writer_started.load(Ordering::SeqCst) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        writer_started.load(Ordering::SeqCst),
        "background writer never started mutating the file"
    );

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "volatile-budget".to_string(),
        })
        .unwrap();

    keep_writing.store(false, Ordering::SeqCst);
    let _ = writer.join();

    assert_eq!(commit.committed_files, 0);
    assert_eq!(commit.warnings.len(), 1);
    assert!(commit.warnings[0].contains("large.txt"));
    assert!(
        commit.warnings[0].contains("unstable") || commit.warnings[0].contains("skipped"),
        "unexpected warning: {}",
        commit.warnings[0]
    );
}

#[test]
fn commit_can_use_snapshot_reader_and_custom_stable_read_policy_together() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let snapshot_bytes = vec![b'S'; 9 * 1024 * 1024];
    let disk_bytes = vec![b'D'; 9 * 1024 * 1024];
    let calls = Arc::new(Mutex::new(Vec::new()));
    let facade = with_snapshot_reader_and_policy_for_test(
        Arc::new(TestSnapshotReader {
            result: Ok(snapshot_bytes.clone()),
            calls: Arc::clone(&calls),
        }),
        StableReadPolicy {
            metadata_retry_attempts: 5,
            volatile_retry_attempts: 1,
        },
    );
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("large.txt"), &disk_bytes).unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "combined-facade-options".to_string(),
        })
        .unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(&commit.snapshot_id).unwrap();
    let file = read_service.open_file(&snapshot, "large.txt").unwrap();
    let content = read_service
        .read_range(&file, 0, snapshot_bytes.len())
        .unwrap();

    assert_eq!(content, snapshot_bytes);
    assert_eq!(calls.lock().unwrap().len(), 1);
}

#[test]
fn clearing_test_unlock_cache_only_evicts_the_requested_repo() {
    let temp = tempdir().unwrap();
    let repo_root_a = temp.path().join("repo-a");
    let repo_root_b = temp.path().join("repo-b");
    fs::create_dir_all(&repo_root_a).unwrap();
    fs::create_dir_all(&repo_root_b).unwrap();

    let facade = RepositoryFacade::new();
    let created_a = facade.init(init_options(&repo_root_a)).unwrap();
    let created_b = facade.init(init_options(&repo_root_b)).unwrap();

    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&repo_root_a.join(".e2v"));

    let opened_a = facade.open(&repo_root_a).unwrap();
    let opened_b = facade.open(&repo_root_b).unwrap();

    assert_eq!(opened_a, created_a);
    assert_eq!(opened_b, created_b);
    let reopened_a = facade
        .unlock(&repo_root_a, "correct horse battery staple")
        .unwrap();
    assert_eq!(reopened_a, created_a);
}

#[test]
fn init_uses_the_requested_default_branch_for_ref_state() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    let state = facade
        .init(InitOptions {
            repo_root: repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "trunk".to_string(),
        })
        .unwrap();

    assert_eq!(state.branch.name, "trunk");
    let opened = facade.open(&repo_root).unwrap();
    assert_eq!(opened.branch.name, "trunk");
}

#[test]
fn init_rejects_non_empty_repository_directory() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();
    fs::write(repo_root.join("hello.txt"), "world").unwrap();

    let facade = RepositoryFacade::new();
    let error = facade.init(init_options(&repo_root)).unwrap_err();

    assert!(
        error.to_string().contains("empty"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn commit_writes_snapshot_and_updates_default_ref() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("hello.txt"), "hello world").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "initial snapshot".to_string(),
        })
        .unwrap();

    assert!(!commit.snapshot_id.is_empty());
    assert_eq!(commit.committed_files, 1);
    assert_eq!(commit.new_bytes, "hello world".len() as u64);
    assert_eq!(commit.reused_bytes, 0);

    let objects_dir = repo_root.join(".e2v").join("objects");
    let object_files = fs::read_dir(&objects_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert!(
        object_files.len() >= 4,
        "expected chunk/file/tree/snapshot objects, found {object_files:?}"
    );

    let snapshots = facade.snapshots(&repo_root).unwrap();
    assert_eq!(snapshots[0].snapshot_id, commit.snapshot_id);
}

#[test]
fn commit_ignores_control_plane_directory_contents() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("tracked.txt"), "tracked").unwrap();
    fs::write(repo_root.join(".e2v").join("ignore-me.txt"), "control").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "ignore control plane".to_string(),
        })
        .unwrap();

    assert_eq!(commit.committed_files, 1);
}

#[test]
fn second_commit_links_to_previous_snapshot_as_parent() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();

    fs::write(repo_root.join("tracked.txt"), "v1").unwrap();
    let first = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "first".to_string(),
        })
        .unwrap();

    fs::write(repo_root.join("tracked.txt"), "v2").unwrap();
    let second = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "second".to_string(),
        })
        .unwrap();

    let snapshots = facade.snapshots(&repo_root).unwrap();

    assert_eq!(snapshots[0].snapshot_id, second.snapshot_id);
    assert_eq!(
        snapshots[0].parent_snapshot_id.as_deref(),
        Some(first.snapshot_id.as_str())
    );
}

#[test]
fn repeated_identical_commit_reports_reused_bytes() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();

    fs::write(repo_root.join("tracked.txt"), "same-content").unwrap();
    let first = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "first".to_string(),
        })
        .unwrap();
    let second = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "second".to_string(),
        })
        .unwrap();

    assert_eq!(first.reused_bytes, 0);
    assert_eq!(first.new_bytes, "same-content".len() as u64);
    assert_eq!(second.new_bytes, 0);
    assert_eq!(second.reused_bytes, "same-content".len() as u64);
}

#[test]
fn snapshots_lists_latest_commit_first() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();

    fs::write(repo_root.join("tracked.txt"), "v1").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "first".to_string(),
        })
        .unwrap();

    fs::write(repo_root.join("tracked.txt"), "v2").unwrap();
    let second = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "second".to_string(),
        })
        .unwrap();

    let snapshots = facade.snapshots(&repo_root).unwrap();

    assert_eq!(snapshots.len(), 2);
    assert_eq!(snapshots[0].snapshot_id, second.snapshot_id);
    assert_eq!(snapshots[0].message, "second");
    assert_eq!(snapshots[1].message, "first");
}

#[test]
fn branch_create_and_list_tracks_current_and_existing_heads() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    let created = facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("tracked.txt"), "base").unwrap();
    let first = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "base".to_string(),
        })
        .unwrap();

    let before_objects = fs::read_dir(repo_root.join(".e2v").join("objects"))
        .unwrap()
        .count();

    let feature = facade.create_branch(&repo_root, "feature").unwrap();
    let branches = facade.list_branches(&repo_root).unwrap();
    let after_objects = fs::read_dir(repo_root.join(".e2v").join("objects"))
        .unwrap()
        .count();

    assert_eq!(before_objects, after_objects);
    assert_eq!(created.branch.name, "main");
    assert_eq!(feature.name, "feature");
    assert_ne!(feature.token_hex, created.branch.token_hex);
    assert_eq!(branches.len(), 2);
    assert_eq!(branches[0].name, "feature");
    assert_eq!(
        branches[0].head_snapshot_id.as_deref(),
        Some(first.snapshot_id.as_str())
    );
    assert!(!branches[0].is_current);
    assert_eq!(branches[1].name, "main");
    assert_eq!(
        branches[1].head_snapshot_id.as_deref(),
        Some(first.snapshot_id.as_str())
    );
    assert!(branches[1].is_current);
}

#[test]
fn branch_checkout_switches_active_branch_and_commit_only_advances_that_branch() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    let main = facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("tracked.txt"), "base").unwrap();
    let base = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "base".to_string(),
        })
        .unwrap();
    let feature = facade.create_branch(&repo_root, "feature").unwrap();

    let reopened = facade.checkout_branch(&repo_root, "feature").unwrap();
    assert_eq!(reopened.branch.name, "feature");
    assert_eq!(
        fs::read_to_string(repo_root.join("tracked.txt")).unwrap(),
        "base"
    );

    fs::write(repo_root.join("tracked.txt"), "feature-v2").unwrap();
    let feature_commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "feature".to_string(),
        })
        .unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let feature_snapshot = read_service.resolve_branch(&feature.token_hex).unwrap();
    let main_snapshot = read_service.resolve_branch(&main.branch.token_hex).unwrap();
    let current_snapshots = facade.snapshots(&repo_root).unwrap();

    assert_eq!(feature_snapshot.snapshot_id, feature_commit.snapshot_id);
    assert_eq!(main_snapshot.snapshot_id, base.snapshot_id);
    assert_eq!(current_snapshots[0].snapshot_id, feature_commit.snapshot_id);
}

#[test]
fn branch_delete_rejects_current_branch_and_allows_non_current_branch() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    facade.create_branch(&repo_root, "feature").unwrap();

    let current_error = facade.delete_branch(&repo_root, "main").unwrap_err();
    assert!(
        current_error.to_string().contains("current")
            || current_error.to_string().contains("active"),
        "unexpected error: {current_error:#}"
    );

    facade.delete_branch(&repo_root, "feature").unwrap();
    let branches = facade.list_branches(&repo_root).unwrap();

    assert_eq!(branches.len(), 1);
    assert_eq!(branches[0].name, "main");
}

#[test]
fn read_service_reads_directory_and_file_content_from_snapshot() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("hello.txt"), "hello world").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "first".to_string(),
        })
        .unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(&commit.snapshot_id).unwrap();
    let entries = read_service.read_dir(&snapshot, "").unwrap();

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "hello.txt");
    assert_eq!(entries[0].kind, "file");

    let file = read_service.open_file(&snapshot, "hello.txt").unwrap();
    assert_eq!(file.layout_generation(), snapshot.layout_generation);
    assert_eq!(file.crypto_suite(), "xchacha20poly1305");
    assert_eq!(file.key_epoch(), 1);
    assert_eq!(file.chunker_id(), "fastcdc");
    let content = read_service.read_range(&file, 0, 5).unwrap();

    assert_eq!(String::from_utf8(content).unwrap(), "hello");
}

#[test]
fn read_service_can_be_constructed_without_repository_facade() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("hello.txt"), "hello world").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "first".to_string(),
        })
        .unwrap();

    let read_service = e2v_core::ReadService::new(&repo_root);
    let snapshot = read_service.open_snapshot(&commit.snapshot_id).unwrap();
    let file = read_service.open_file(&snapshot, "hello.txt").unwrap();
    let content = read_service.read_range(&file, 0, 5).unwrap();

    assert_eq!(String::from_utf8(content).unwrap(), "hello");
}

#[test]
fn read_service_reads_empty_files_from_snapshot() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("empty.txt"), []).unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "empty".to_string(),
        })
        .unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(&commit.snapshot_id).unwrap();
    let file = read_service.open_file(&snapshot, "empty.txt").unwrap();
    let content = read_service.read_range(&file, 0, 0).unwrap();

    assert!(content.is_empty());
    assert_eq!(file.file_size(), 0);
    assert_eq!(file.chunk_count(), 0);
}

#[test]
fn read_service_clamps_ranges_that_extend_past_end_of_file() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("hello.txt"), "hello world").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "first".to_string(),
        })
        .unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(&commit.snapshot_id).unwrap();
    let file = read_service.open_file(&snapshot, "hello.txt").unwrap();
    let content = read_service.read_range(&file, 6, 99).unwrap();

    assert_eq!(String::from_utf8(content).unwrap(), "world");
}

#[test]
fn read_service_can_browse_nested_directories_and_files() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::create_dir_all(repo_root.join("nested")).unwrap();
    fs::write(repo_root.join("nested").join("hello.txt"), "hello nested").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "nested".to_string(),
        })
        .unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(&commit.snapshot_id).unwrap();
    let root_entries = read_service.read_dir(&snapshot, "").unwrap();

    assert_eq!(root_entries.len(), 1);
    assert_eq!(root_entries[0].name, "nested");
    assert_eq!(root_entries[0].kind, "tree");

    let nested_entries = read_service.read_dir(&snapshot, "nested").unwrap();
    assert_eq!(nested_entries.len(), 1);
    assert_eq!(nested_entries[0].name, "hello.txt");
    assert_eq!(nested_entries[0].kind, "file");

    let file = read_service
        .open_file(&snapshot, "nested/hello.txt")
        .unwrap();
    let content = read_service.read_range(&file, 0, 99).unwrap();
    assert_eq!(String::from_utf8(content).unwrap(), "hello nested");
}

#[test]
fn read_service_normalizes_decomposed_unicode_paths() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    let decomposed = "e\u{301}.txt".to_string();
    fs::write(repo_root.join(&decomposed), "hello unicode").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "unicode".to_string(),
        })
        .unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(&commit.snapshot_id).unwrap();
    let file = read_service.open_file(&snapshot, &decomposed).unwrap();
    let content = read_service.read_range(&file, 0, 99).unwrap();

    assert_eq!(String::from_utf8(content).unwrap(), "hello unicode");
}

#[test]
fn unicode_name_scan_checkout_scan_is_idempotent() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    let decomposed = "e\u{301}.txt".to_string();
    fs::write(repo_root.join(&decomposed), "hello unicode").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "unicode".to_string(),
        })
        .unwrap();

    let checkout_target = temp.path().join("checkout");
    fs::create_dir_all(&checkout_target).unwrap();
    facade
        .checkout(CheckoutOptions {
            repo_root: repo_root.clone(),
            snapshot_id: commit.snapshot_id,
            target_dir: checkout_target.clone(),
        })
        .unwrap();

    let checkout_names = std::fs::read_dir(&checkout_target)
        .unwrap()
        .filter_map(|entry| {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.file_name().and_then(|name| name.to_str()) == Some(".e2v-checkout-mapping.json")
            {
                None
            } else {
                Some(entry.file_name().to_string_lossy().to_string())
            }
        })
        .collect::<Vec<_>>();

    assert_eq!(checkout_names.len(), 1);

    let working_tree = e2v_core::testing::new_working_tree_for_test(&checkout_target);
    let scanned = working_tree.scan_dir(&checkout_target, true).unwrap();
    let scanned_names = scanned
        .into_iter()
        .map(|entry| entry.name)
        .collect::<Vec<_>>();

    assert_eq!(scanned_names, vec!["\u{e9}.txt".to_string()]);
}

#[test]
fn checkout_restores_snapshot_into_target_directory() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::create_dir_all(repo_root.join("nested")).unwrap();
    fs::write(repo_root.join("root.txt"), "root").unwrap();
    fs::write(repo_root.join("nested").join("hello.txt"), "hello nested").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "snapshot".to_string(),
        })
        .unwrap();

    let checkout_target = temp.path().join("checkout");
    fs::create_dir_all(&checkout_target).unwrap();

    facade
        .checkout(CheckoutOptions {
            repo_root: repo_root.clone(),
            snapshot_id: commit.snapshot_id,
            target_dir: checkout_target.clone(),
        })
        .unwrap();

    assert_eq!(
        fs::read_to_string(checkout_target.join("root.txt")).unwrap(),
        "root"
    );
    assert_eq!(
        fs::read_to_string(checkout_target.join("nested").join("hello.txt")).unwrap(),
        "hello nested"
    );
    let mapping_path = checkout_target.join(".e2v-checkout-mapping.json");
    let mapping = fs::read_to_string(mapping_path).unwrap();
    assert!(mapping.contains("root.txt"));
    assert!(mapping.contains("nested/hello.txt"));
}

#[test]
fn repeated_checkout_does_not_duplicate_local_checkout_mapping_entries() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::create_dir_all(repo_root.join("nested")).unwrap();
    fs::write(repo_root.join("root.txt"), "root").unwrap();
    fs::write(repo_root.join("nested").join("hello.txt"), "hello nested").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "snapshot".to_string(),
        })
        .unwrap();

    let checkout_target = temp.path().join("checkout");
    fs::create_dir_all(&checkout_target).unwrap();

    facade
        .checkout(CheckoutOptions {
            repo_root: repo_root.clone(),
            snapshot_id: commit.snapshot_id.clone(),
            target_dir: checkout_target.clone(),
        })
        .unwrap();
    facade
        .checkout(CheckoutOptions {
            repo_root: repo_root.clone(),
            snapshot_id: commit.snapshot_id,
            target_dir: checkout_target.clone(),
        })
        .unwrap();

    let mapping_path = checkout_target.join(".e2v-checkout-mapping.json");
    let mappings: Vec<CheckoutPathMappingMirror> =
        serde_json::from_slice(&fs::read(mapping_path).unwrap()).unwrap();
    let snapshot_paths = mappings
        .iter()
        .map(|entry| entry.snapshot_path.as_str())
        .collect::<Vec<_>>();

    assert_eq!(mappings.len(), 2);
    assert_eq!(snapshot_paths, vec!["nested/hello.txt", "root.txt"]);
}

#[test]
fn checkout_mapping_is_stored_as_compact_json() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("root.txt"), "root").unwrap();
    fs::create_dir_all(repo_root.join("nested")).unwrap();
    fs::write(repo_root.join("nested").join("hello.txt"), "hello nested").unwrap();
    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "seed".into(),
        })
        .unwrap();

    let checkout_target = temp.path().join("checkout");
    fs::create_dir_all(&checkout_target).unwrap();
    facade
        .checkout(CheckoutOptions {
            repo_root: repo_root.clone(),
            snapshot_id: commit.snapshot_id,
            target_dir: checkout_target.clone(),
        })
        .unwrap();

    let mapping = fs::read_to_string(checkout_target.join(".e2v-checkout-mapping.json")).unwrap();
    assert!(
        !mapping.contains('\n'),
        "expected compact checkout mapping json without pretty-printed newlines"
    );
}

#[test]
fn checkout_rewrites_local_checkout_mapping_to_current_snapshot_entries() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("root.txt"), "root").unwrap();
    let first = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "first".to_string(),
        })
        .unwrap();

    fs::create_dir_all(repo_root.join("nested")).unwrap();
    fs::write(repo_root.join("nested").join("hello.txt"), "hello nested").unwrap();
    let second = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "second".to_string(),
        })
        .unwrap();

    let checkout_target = temp.path().join("checkout");
    fs::create_dir_all(&checkout_target).unwrap();

    facade
        .checkout(CheckoutOptions {
            repo_root: repo_root.clone(),
            snapshot_id: second.snapshot_id,
            target_dir: checkout_target.clone(),
        })
        .unwrap();
    facade
        .checkout(CheckoutOptions {
            repo_root: repo_root.clone(),
            snapshot_id: first.snapshot_id,
            target_dir: checkout_target.clone(),
        })
        .unwrap();

    let mapping_path = checkout_target.join(".e2v-checkout-mapping.json");
    let mappings: Vec<CheckoutPathMappingMirror> =
        serde_json::from_slice(&fs::read(mapping_path).unwrap()).unwrap();
    let snapshot_paths = mappings
        .iter()
        .map(|entry| entry.snapshot_path.as_str())
        .collect::<Vec<_>>();

    assert_eq!(mappings.len(), 1);
    assert_eq!(snapshot_paths, vec!["root.txt"]);
}

#[test]
fn checkout_removes_previously_materialized_files_missing_from_new_snapshot() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("root.txt"), "root").unwrap();
    let first = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "first".to_string(),
        })
        .unwrap();

    fs::create_dir_all(repo_root.join("nested")).unwrap();
    fs::write(repo_root.join("nested").join("hello.txt"), "hello nested").unwrap();
    let second = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "second".to_string(),
        })
        .unwrap();

    let checkout_target = temp.path().join("checkout");
    fs::create_dir_all(&checkout_target).unwrap();

    facade
        .checkout(CheckoutOptions {
            repo_root: repo_root.clone(),
            snapshot_id: second.snapshot_id,
            target_dir: checkout_target.clone(),
        })
        .unwrap();
    assert_eq!(
        fs::read_to_string(checkout_target.join("nested").join("hello.txt")).unwrap(),
        "hello nested"
    );

    facade
        .checkout(CheckoutOptions {
            repo_root: repo_root.clone(),
            snapshot_id: first.snapshot_id,
            target_dir: checkout_target.clone(),
        })
        .unwrap();

    assert_eq!(
        fs::read_to_string(checkout_target.join("root.txt")).unwrap(),
        "root"
    );
    assert!(
        !checkout_target.join("nested").join("hello.txt").exists(),
        "checkout should remove previously materialized files that are absent from the new snapshot"
    );
}

#[test]
fn commit_ignores_local_checkout_mapping_artifact() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("root.txt"), "root").unwrap();
    fs::write(
        repo_root.join(".e2v-checkout-mapping.json"),
        r#"[{"snapshot_path":"root.txt","local_path":"C:\\demo\\root.txt"}]"#,
    )
    .unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "first".to_string(),
        })
        .unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(&commit.snapshot_id).unwrap();
    let entries = read_service.read_dir(&snapshot, "").unwrap();

    assert!(entries.iter().any(|entry| entry.name == "root.txt"));
    assert!(
        !entries
            .iter()
            .any(|entry| entry.name == ".e2v-checkout-mapping.json")
    );
}

#[test]
fn checkout_does_not_restore_control_plane_directory() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("root.txt"), "root").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "snapshot".to_string(),
        })
        .unwrap();

    let checkout_target = temp.path().join("checkout");
    fs::create_dir_all(&checkout_target).unwrap();

    facade
        .checkout(CheckoutOptions {
            repo_root,
            snapshot_id: commit.snapshot_id,
            target_dir: checkout_target.clone(),
        })
        .unwrap();

    assert!(!checkout_target.join(".e2v").exists());
}

#[test]
fn checkout_rejects_conflicts_with_existing_directories() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("root.txt"), "root").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "snapshot".to_string(),
        })
        .unwrap();

    let checkout_target = temp.path().join("checkout");
    fs::create_dir_all(checkout_target.join("root.txt")).unwrap();

    let error = facade
        .checkout(CheckoutOptions {
            repo_root,
            snapshot_id: commit.snapshot_id,
            target_dir: checkout_target,
        })
        .unwrap_err();

    assert!(
        error.to_string().contains("checkout conflict"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn checkout_preflights_all_paths_before_writing_any_files() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::create_dir_all(repo_root.join("z-nested")).unwrap();
    fs::write(repo_root.join("a-root.txt"), "root").unwrap();
    fs::write(repo_root.join("z-nested").join("hello.txt"), "hello nested").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "snapshot".to_string(),
        })
        .unwrap();

    let checkout_target = temp.path().join("checkout");
    fs::create_dir_all(checkout_target.join("z-nested").join("hello.txt")).unwrap();

    let error = facade
        .checkout(CheckoutOptions {
            repo_root,
            snapshot_id: commit.snapshot_id,
            target_dir: checkout_target.clone(),
        })
        .unwrap_err();

    assert!(
        error.to_string().contains("checkout conflict"),
        "unexpected error: {error:#}"
    );
    assert!(
        !checkout_target.join("a-root.txt").exists(),
        "checkout wrote files before preflight completed"
    );
}

#[test]
fn checkout_rejects_parent_path_file_conflicts_before_writing_any_files() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::create_dir_all(repo_root.join("nested")).unwrap();
    fs::write(repo_root.join("nested").join("hello.txt"), "hello nested").unwrap();
    fs::write(repo_root.join("root.txt"), "root").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "snapshot".to_string(),
        })
        .unwrap();

    let checkout_target = temp.path().join("checkout");
    fs::create_dir_all(&checkout_target).unwrap();
    fs::write(checkout_target.join("nested"), "blocking-file").unwrap();

    let error = facade
        .checkout(CheckoutOptions {
            repo_root,
            snapshot_id: commit.snapshot_id,
            target_dir: checkout_target.clone(),
        })
        .unwrap_err();

    assert!(
        error.to_string().contains("checkout conflict"),
        "unexpected error: {error:#}"
    );
    assert!(
        !checkout_target.join("root.txt").exists(),
        "checkout wrote files before parent-path preflight completed"
    );
    assert!(
        !checkout_target.join("nested").join("hello.txt").exists(),
        "checkout wrote nested files despite parent path conflict"
    );
}

#[test]
fn checkout_does_not_publish_any_files_until_all_reads_verify() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("a-first.txt"), "first").unwrap();
    fs::write(repo_root.join("z-second.txt"), "second").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "snapshot".to_string(),
        })
        .unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(&commit.snapshot_id).unwrap();
    let bad_file = read_service.open_file(&snapshot, "z-second.txt").unwrap();
    let bad_chunk_id = bad_file.debug_chunk_ids()[0].clone();
    let bad_chunk_path = repo_root
        .join(".e2v")
        .join("objects")
        .join(format!("{bad_chunk_id}.json"));
    let mut bytes = fs::read(&bad_chunk_path).unwrap();
    let last_index = bytes.len() - 1;
    bytes[last_index] ^= 0x01;
    fs::write(&bad_chunk_path, bytes).unwrap();

    let checkout_target = temp.path().join("checkout");
    fs::create_dir_all(&checkout_target).unwrap();

    let error = facade
        .checkout(CheckoutOptions {
            repo_root,
            snapshot_id: commit.snapshot_id,
            target_dir: checkout_target.clone(),
        })
        .unwrap_err();

    assert!(
        error.to_string().contains("authentication"),
        "unexpected error: {error:#}"
    );
    assert!(
        !checkout_target.join("a-first.txt").exists(),
        "checkout published early files before all reads verified"
    );
    let leftover_temps = fs::read_dir(&checkout_target)
        .unwrap()
        .flat_map(|entry| {
            let path = entry.unwrap().path();
            if path.is_file() {
                vec![path]
            } else {
                fs::read_dir(path)
                    .unwrap()
                    .map(|child| child.unwrap().path())
                    .collect::<Vec<_>>()
            }
        })
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("e2v-tmp"))
        .collect::<Vec<_>>();
    assert!(
        leftover_temps.is_empty(),
        "expected no leftover temp files, found {leftover_temps:?}"
    );
}

#[test]
fn commit_uses_repo_scoped_snapshot_ids_for_identical_content() {
    let temp = tempdir().unwrap();
    let repo_a = temp.path().join("repo-a");
    let repo_b = temp.path().join("repo-b");
    fs::create_dir_all(&repo_a).unwrap();
    fs::create_dir_all(&repo_b).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_a)).unwrap();
    facade.init(init_options(&repo_b)).unwrap();

    fs::write(repo_a.join("hello.txt"), "same-content").unwrap();
    fs::write(repo_b.join("hello.txt"), "same-content").unwrap();

    let left = facade
        .commit(CommitOptions {
            repo_root: repo_a,
            message: "same-message".to_string(),
        })
        .unwrap();
    let right = facade
        .commit(CommitOptions {
            repo_root: repo_b,
            message: "same-message".to_string(),
        })
        .unwrap();

    assert_ne!(left.snapshot_id, right.snapshot_id);
}

#[test]
fn commit_uses_repo_scoped_chunk_ids_for_identical_content() {
    let temp = tempdir().unwrap();
    let repo_a = temp.path().join("repo-a");
    let repo_b = temp.path().join("repo-b");
    fs::create_dir_all(&repo_a).unwrap();
    fs::create_dir_all(&repo_b).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_a)).unwrap();
    facade.init(init_options(&repo_b)).unwrap();

    fs::write(repo_a.join("hello.txt"), "same-content").unwrap();
    fs::write(repo_b.join("hello.txt"), "same-content").unwrap();

    let left = facade
        .commit(CommitOptions {
            repo_root: repo_a.clone(),
            message: "same-message".to_string(),
        })
        .unwrap();
    let right = facade
        .commit(CommitOptions {
            repo_root: repo_b.clone(),
            message: "same-message".to_string(),
        })
        .unwrap();

    let left_read_service = facade.read_service(&repo_a).unwrap();
    let left_snapshot = left_read_service.open_snapshot(&left.snapshot_id).unwrap();
    let left_file = left_read_service
        .open_file(&left_snapshot, "hello.txt")
        .unwrap();

    let right_read_service = facade.read_service(&repo_b).unwrap();
    let right_snapshot = right_read_service
        .open_snapshot(&right.snapshot_id)
        .unwrap();
    let right_file = right_read_service
        .open_file(&right_snapshot, "hello.txt")
        .unwrap();

    assert_ne!(
        left_file.debug_chunk_ids()[0],
        right_file.debug_chunk_ids()[0]
    );
}

#[test]
fn committed_objects_do_not_store_plaintext_file_or_path_bytes() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("hello.txt"), "hello world").unwrap();

    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "plain-check".to_string(),
        })
        .unwrap();

    let object_bytes = fs::read_dir(repo_root.join(".e2v").join("objects"))
        .unwrap()
        .map(|entry| fs::read(entry.unwrap().path()).unwrap())
        .collect::<Vec<_>>();

    for bytes in object_bytes {
        assert!(
            !bytes
                .windows(b"hello world".len())
                .any(|window| window == b"hello world"),
            "object bytes leaked file content"
        );
        assert!(
            !bytes
                .windows(b"hello.txt".len())
                .any(|window| window == b"hello.txt"),
            "object bytes leaked file name"
        );
    }
}

#[test]
fn read_service_rejects_tampered_snapshot_objects() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("hello.txt"), "hello world").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "tamper".to_string(),
        })
        .unwrap();

    let snapshot_path = repo_root
        .join(".e2v")
        .join("objects")
        .join(format!("{}.json", commit.snapshot_id));
    let mut bytes = fs::read(&snapshot_path).unwrap();
    let last_index = bytes.len() - 1;
    bytes[last_index] ^= 0x01;
    fs::write(&snapshot_path, bytes).unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let error = read_service.open_snapshot(&commit.snapshot_id).unwrap_err();

    assert!(
        error.to_string().contains("authentication"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn local_default_ref_does_not_store_plaintext_branch_or_snapshot() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade
        .init(InitOptions {
            repo_root: repo_root.clone(),
            password: "correct horse battery staple".to_string(),
            branch_name: "trunk".to_string(),
        })
        .unwrap();
    fs::write(repo_root.join("hello.txt"), "hello world").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "first".to_string(),
        })
        .unwrap();

    let ref_bytes = fs::read(repo_root.join(".e2v").join("refs").join("default.json")).unwrap();
    assert!(
        !ref_bytes
            .windows(b"trunk".len())
            .any(|window| window == b"trunk"),
        "ref bytes leaked branch name"
    );
    assert!(
        !ref_bytes
            .windows(commit.snapshot_id.len())
            .any(|window| window == commit.snapshot_id.as_bytes()),
        "ref bytes leaked snapshot id"
    );
}

#[test]
fn read_service_can_resolve_current_snapshot_from_branch_token() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    let state = facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("hello.txt"), "hello world").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "first".to_string(),
        })
        .unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service
        .resolve_branch(&state.branch.token_hex)
        .unwrap();

    assert_eq!(snapshot.snapshot_id, commit.snapshot_id);
}

#[test]
fn read_service_rejects_snapshot_id_path_traversal() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let error = read_service.open_snapshot("../evil").unwrap_err();

    assert!(
        error.to_string().contains("snapshot id")
            || error.to_string().contains("object id")
            || error.to_string().contains("path traversal"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn read_service_rejects_parent_dir_segments_in_directory_paths() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::create_dir_all(repo_root.join("nested")).unwrap();
    fs::write(repo_root.join("nested").join("hello.txt"), "hello nested").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "nested".to_string(),
        })
        .unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(&commit.snapshot_id).unwrap();

    let error = read_service.read_dir(&snapshot, "../nested").unwrap_err();

    assert!(
        error.to_string().contains("invalid snapshot path"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn read_service_rejects_parent_dir_segments_in_file_paths() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    std::fs::create_dir_all(repo_root.join("nested")).unwrap();
    std::fs::write(repo_root.join("nested").join("hello.txt"), "hello nested").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "nested".to_string(),
        })
        .unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(&commit.snapshot_id).unwrap();

    let error = read_service
        .open_file(&snapshot, "nested/../hello.txt")
        .unwrap_err();

    assert!(
        error.to_string().contains("invalid snapshot path"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn large_files_are_split_into_multiple_chunks() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();

    let mut large_bytes = Vec::with_capacity(24 * 1024 * 1024);
    for block in 0..24usize {
        let fill = b'A' + (block as u8 % 26);
        large_bytes.extend(std::iter::repeat_n(fill, 1024 * 1024));
    }
    fs::write(repo_root.join("large.txt"), &large_bytes).unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "large-file".to_string(),
        })
        .unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(&commit.snapshot_id).unwrap();
    let file = read_service.open_file(&snapshot, "large.txt").unwrap();

    assert!(
        file.chunk_count() > 1,
        "expected large file to span multiple chunks, got {}",
        file.chunk_count()
    );

    let read_back = read_service
        .read_range(&file, 0, large_bytes.len())
        .unwrap();
    assert_eq!(read_back, large_bytes);
}

#[test]
fn oversized_file_chunk_lists_are_sharded_without_breaking_read_service_or_manifest_store() {
    let _guard = e2v_core::testing::override_max_file_chunks_per_object_for_test(2);
    let _chunk_guard = e2v_core::testing::override_fixed_span_bytes_for_test(1024 * 1024);
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();

    let mut large_bytes = Vec::with_capacity(9_000_000);
    for i in 0..9_000_000usize {
        large_bytes.push((i % 251) as u8);
    }
    fs::write(repo_root.join("large.txt"), &large_bytes).unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "large-file-sharded".to_string(),
        })
        .unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(&commit.snapshot_id).unwrap();
    let file = read_service.open_file(&snapshot, "large.txt").unwrap();
    let manifest_store = ManifestStore::new(&repo_root);
    let snapshot_manifest = manifest_store.get_snapshot(&commit.snapshot_id).unwrap();
    let tree = manifest_store
        .get_tree_node(&snapshot_manifest.root_tree_id)
        .unwrap();
    let file_entry = tree
        .entries
        .iter()
        .find(|entry| entry.name == "large.txt" && entry.kind == "file")
        .unwrap();
    let file_manifest = manifest_store.get_file(&file_entry.object_id).unwrap();
    let local_object_files = e2v_core::sync_support::list_local_object_files(&repo_root).unwrap();
    let file_shard_object_count = local_object_files
        .iter()
        .filter_map(|path| {
            let object_id = path.file_stem()?.to_str()?;
            e2v_core::sync_support::read_local_object_type_hint(&repo_root, object_id).ok()
        })
        .filter(|object_type| object_type == "file_shard")
        .count();

    assert!(
        file.chunk_count() > 2,
        "expected large file to span more than the test chunk limit"
    );
    assert!(
        file_shard_object_count > 0,
        "expected oversized file to publish file_shard objects"
    );
    assert!(
        !file_manifest.chunk_lengths.is_empty(),
        "expected manifest store to expose chunk lengths"
    );
    assert_eq!(
        read_service
            .read_range(&file, 0, large_bytes.len())
            .unwrap(),
        large_bytes
    );
}

#[test]
fn middle_insertion_only_adds_local_chunks() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();

    let mut original_bytes = Vec::with_capacity(20_000_000);
    for i in 0..20_000_000usize {
        original_bytes.push((i % 251) as u8);
    }
    fs::write(repo_root.join("large.txt"), &original_bytes).unwrap();

    let first = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "large-file-v1".to_string(),
        })
        .unwrap();

    let middle = original_bytes.len() / 2;
    let mut updated_bytes = original_bytes[..middle].to_vec();
    updated_bytes.extend(std::iter::repeat_n(b'Z', 4096));
    updated_bytes.extend_from_slice(&original_bytes[middle..]);
    fs::write(repo_root.join("large.txt"), &updated_bytes).unwrap();

    let second = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "large-file-v2".to_string(),
        })
        .unwrap();

    assert!(second.reused_bytes > 0);
    assert!(
        second.new_bytes < first.new_bytes,
        "expected local chunk reuse, first new_bytes={}, second new_bytes={}",
        first.new_bytes,
        second.new_bytes
    );
    assert!(
        second.reused_bytes >= 8 * 1024 * 1024,
        "expected meaningful chunk reuse, reused_bytes={}",
        second.reused_bytes
    );
}

#[test]
fn verify_snapshot_accepts_a_healthy_local_snapshot() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("hello.txt"), "hello world").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "verify".to_string(),
        })
        .unwrap();

    facade
        .verify_snapshot(&repo_root, &commit.snapshot_id)
        .unwrap();
}

#[test]
fn open_rejects_unsupported_layout_root_schema_version() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();

    let layout_root_path = repo_root.join(".e2v").join("layout_root.json");
    let mut layout_root: serde_json::Value =
        serde_json::from_slice(&fs::read(&layout_root_path).unwrap()).unwrap();
    layout_root["schema_version"] = serde_json::Value::from(99u64);
    fs::write(
        &layout_root_path,
        serde_json::to_vec_pretty(&layout_root).unwrap(),
    )
    .unwrap();

    let error = facade.open(&repo_root).unwrap_err();

    assert!(
        error.to_string().contains("schema")
            || error.to_string().contains("layout")
            || error.to_string().contains("unsupported"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn open_accepts_oblivious_layout_root_metadata_when_schema_is_supported() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();

    let layout_root_path = repo_root.join(".e2v").join("layout_root.json");
    let mut layout_root: serde_json::Value =
        serde_json::from_slice(&fs::read(&layout_root_path).unwrap()).unwrap();
    layout_root["layout_id"] = serde_json::Value::String("oram-v1".to_string());
    layout_root["mode"] = serde_json::Value::String("Oblivious".to_string());
    layout_root["mapping_policy"] = serde_json::Value::String("bucketed-randomized".to_string());
    layout_root["dedup_mode"] = serde_json::Value::String("GenerationScopedRandomized".to_string());
    layout_root["oblivious_generation"] = serde_json::Value::from(3u64);
    layout_root["schedule_policy"] = serde_json::json!({
        "bucket_bytes": 4096u64,
        "min_total_reads": 3u64,
        "cover_reads_per_request": 2u64,
        "reshuffle_after_generations": 5u64
    });
    layout_root["traffic_policy"] = serde_json::json!({
        "max_parallel_reads": 2u64,
        "inter_read_delay_ms": 15u64,
        "burst_budget_bytes": 16384u64,
        "target_request_window_ms": 90u64
    });
    layout_root["cost_policy"] = serde_json::json!({
        "profile": "balanced",
        "max_expected_read_amplification": 3u64,
        "max_expected_write_amplification": 4u64
    });
    fs::write(
        &layout_root_path,
        serde_json::to_vec_pretty(&layout_root).unwrap(),
    )
    .unwrap();

    let state = facade.open(&repo_root).unwrap();

    assert_eq!(state.layout_generation, 1);
}

#[test]
fn verify_snapshot_requires_layout_root_view_to_be_present() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("hello.txt"), "hello world").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "verify-layout".to_string(),
        })
        .unwrap();

    fs::remove_file(repo_root.join(".e2v").join("layout_root.json")).unwrap();

    let error = facade
        .verify_snapshot(&repo_root, &commit.snapshot_id)
        .unwrap_err();

    assert!(
        error.to_string().contains("layout_root.json") || error.to_string().contains("layout root"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn verify_snapshot_rejects_unsupported_layout_root_schema_version() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("hello.txt"), "hello world").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "verify-layout-schema".to_string(),
        })
        .unwrap();

    let layout_root_path = repo_root.join(".e2v").join("layout_root.json");
    let mut layout_root: serde_json::Value =
        serde_json::from_slice(&fs::read(&layout_root_path).unwrap()).unwrap();
    layout_root["schema_version"] = serde_json::Value::from(99u64);
    fs::write(
        &layout_root_path,
        serde_json::to_vec_pretty(&layout_root).unwrap(),
    )
    .unwrap();

    let error = facade
        .verify_snapshot(&repo_root, &commit.snapshot_id)
        .unwrap_err();

    assert!(
        error.to_string().contains("schema")
            || error.to_string().contains("layout")
            || error.to_string().contains("unsupported"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn verify_ref_accepts_a_healthy_local_default_ref() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("hello.txt"), "hello world").unwrap();

    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "verify-ref".to_string(),
        })
        .unwrap();

    facade.verify_ref(&repo_root).unwrap();
}

#[test]
fn verify_ref_requires_layout_root_view_to_be_present() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("hello.txt"), "hello world").unwrap();

    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "verify-layout".to_string(),
        })
        .unwrap();

    fs::remove_file(repo_root.join(".e2v").join("layout_root.json")).unwrap();

    let error = facade.verify_ref(&repo_root).unwrap_err();

    assert!(
        error.to_string().contains("layout_root.json") || error.to_string().contains("layout root"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn verify_ref_rejects_unsupported_layout_root_schema_version() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("hello.txt"), "hello world").unwrap();

    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "verify-ref-layout-schema".to_string(),
        })
        .unwrap();

    let layout_root_path = repo_root.join(".e2v").join("layout_root.json");
    let mut layout_root: serde_json::Value =
        serde_json::from_slice(&fs::read(&layout_root_path).unwrap()).unwrap();
    layout_root["schema_version"] = serde_json::Value::from(99u64);
    fs::write(
        &layout_root_path,
        serde_json::to_vec_pretty(&layout_root).unwrap(),
    )
    .unwrap();

    let error = facade.verify_ref(&repo_root).unwrap_err();

    assert!(
        error.to_string().contains("schema")
            || error.to_string().contains("layout")
            || error.to_string().contains("unsupported"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn verify_ref_rejects_tampered_local_default_ref() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("hello.txt"), "hello world").unwrap();

    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "verify-ref".to_string(),
        })
        .unwrap();

    let ref_path = repo_root.join(".e2v").join("refs").join("default.json");
    let mut bytes = fs::read(&ref_path).unwrap();
    let last_index = bytes.len() - 1;
    bytes[last_index] ^= 0x01;
    fs::write(&ref_path, bytes).unwrap();

    let error = facade.verify_ref(&repo_root).unwrap_err();
    assert!(
        error.to_string().contains("authentication")
            || error.to_string().contains("format")
            || error.to_string().contains("ref"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn verify_ref_rejects_when_head_snapshot_is_tampered() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("hello.txt"), "hello world").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "verify-ref-head".to_string(),
        })
        .unwrap();

    let snapshot_path = repo_root
        .join(".e2v")
        .join("objects")
        .join(format!("{}.json", commit.snapshot_id));
    let mut bytes = fs::read(&snapshot_path).unwrap();
    let last_index = bytes.len() - 1;
    bytes[last_index] ^= 0x01;
    fs::write(&snapshot_path, bytes).unwrap();

    let error = facade.verify_ref(&repo_root).unwrap_err();
    assert!(
        error.to_string().contains("authentication"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn verify_snapshot_rejects_tampered_reachable_chunk() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("hello.txt"), "hello world").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "verify".to_string(),
        })
        .unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(&commit.snapshot_id).unwrap();
    let file = read_service.open_file(&snapshot, "hello.txt").unwrap();
    let chunk_id = file.debug_chunk_ids()[0].clone();
    let chunk_path = repo_root
        .join(".e2v")
        .join("objects")
        .join(format!("{chunk_id}.json"));
    let mut bytes = fs::read(&chunk_path).unwrap();
    let last_index = bytes.len() - 1;
    bytes[last_index] ^= 0x01;
    fs::write(&chunk_path, bytes).unwrap();

    let error = facade
        .verify_snapshot(&repo_root, &commit.snapshot_id)
        .unwrap_err();
    assert!(
        error.to_string().contains("authentication"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn verify_snapshot_rejects_snapshot_with_invalid_parent_snapshot_id() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("hello.txt"), "hello world").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "verify-parent".to_string(),
        })
        .unwrap();

    let manifest_store = ManifestStore::new(&repo_root);
    let snapshot = manifest_store.get_snapshot(&commit.snapshot_id).unwrap();

    let control_dir = repo_root.join(".e2v");
    let secrets = e2v_core::sync_support::open_repo_secrets_for_sync(&control_dir).unwrap();
    let object_store = e2v_store::DirectLayoutObjectStore::new(&control_dir, secrets);
    let tampered_snapshot = ManifestSnapshotObject {
        parent_snapshot_id: Some("..\\evil".to_string()),
        ..snapshot
    };
    let tampered_snapshot_id = object_store
        .put_object(
            "snapshot",
            &postcard::to_stdvec(&tampered_snapshot).unwrap(),
        )
        .unwrap();

    let error = facade
        .verify_snapshot(&repo_root, &tampered_snapshot_id)
        .unwrap_err();

    assert!(
        error.to_string().contains("snapshot")
            || error.to_string().contains("parent")
            || error.to_string().contains("object id")
            || error.to_string().contains("path"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn checkout_rejects_tampered_chunk_and_leaves_no_dirty_files() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("hello.txt"), "hello world").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "checkout-tamper".to_string(),
        })
        .unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(&commit.snapshot_id).unwrap();
    let file = read_service.open_file(&snapshot, "hello.txt").unwrap();
    let chunk_id = file.debug_chunk_ids()[0].clone();
    let chunk_path = repo_root
        .join(".e2v")
        .join("objects")
        .join(format!("{chunk_id}.json"));
    let mut bytes = fs::read(&chunk_path).unwrap();
    let last_index = bytes.len() - 1;
    bytes[last_index] ^= 0x01;
    fs::write(&chunk_path, bytes).unwrap();

    let checkout_target = temp.path().join("checkout");
    fs::create_dir_all(&checkout_target).unwrap();

    let error = facade
        .checkout(CheckoutOptions {
            repo_root: repo_root.clone(),
            snapshot_id: commit.snapshot_id,
            target_dir: checkout_target.clone(),
        })
        .unwrap_err();

    assert!(
        error.to_string().contains("authentication"),
        "unexpected error: {error:#}"
    );
    assert!(!checkout_target.join("hello.txt").exists());
    let leftover_temps = fs::read_dir(&checkout_target)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("e2v-tmp"))
        .collect::<Vec<_>>();
    assert!(
        leftover_temps.is_empty(),
        "expected no leftover temp files, found {leftover_temps:?}"
    );
}

#[test]
fn read_range_rejects_tampered_chunk_before_returning_bytes() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("hello.txt"), "hello world").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "range-tamper".to_string(),
        })
        .unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(&commit.snapshot_id).unwrap();
    let file = read_service.open_file(&snapshot, "hello.txt").unwrap();
    let chunk_id = file.debug_chunk_ids()[0].clone();
    let chunk_path = repo_root
        .join(".e2v")
        .join("objects")
        .join(format!("{chunk_id}.json"));
    let mut bytes = fs::read(&chunk_path).unwrap();
    let last_index = bytes.len() - 1;
    bytes[last_index] ^= 0x01;
    fs::write(&chunk_path, bytes).unwrap();

    let error = read_service.read_range(&file, 0, 5).unwrap_err();

    assert!(
        error.to_string().contains("authentication"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn read_range_reports_missing_physical_chunk_with_layout_generation_context() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("hello.txt"), "hello world").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "missing-physical-chunk".to_string(),
        })
        .unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(&commit.snapshot_id).unwrap();
    let file = read_service.open_file(&snapshot, "hello.txt").unwrap();
    let chunk_id = file.debug_chunk_ids()[0].clone();
    let chunk_path = repo_root
        .join(".e2v")
        .join("objects")
        .join(format!("{chunk_id}.json"));
    fs::remove_file(&chunk_path).unwrap();

    let error = read_service.read_range(&file, 0, 5).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("stale-layout fallback unavailable"),
        "unexpected error: {error:#}"
    );
    assert!(
        error
            .to_string()
            .contains(&format!("layout generation {}", file.layout_generation())),
        "unexpected error: {error:#}"
    );
    assert!(
        error.to_string().contains(&chunk_id),
        "unexpected error: {error:#}"
    );
}

#[test]
fn read_range_still_reads_healthy_chunks_after_missing_chunk_error_improvement() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("hello.txt"), "hello world").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "healthy-after-missing-error-improvement".to_string(),
        })
        .unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(&commit.snapshot_id).unwrap();
    let file = read_service.open_file(&snapshot, "hello.txt").unwrap();

    let bytes = read_service.read_range(&file, 0, 5).unwrap();

    assert_eq!(bytes, b"hello");
}

#[test]
fn sync_support_can_resolve_cached_pack_physical_ref_for_object_id() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();

    let control_dir = repo_root.join(".e2v");
    let secrets = e2v_core::sync_support::open_repo_secrets_for_sync(&control_dir).unwrap();
    let cache_dir = control_dir.join("cache").join("pack-index");
    fs::create_dir_all(cache_dir.join("segments")).unwrap();

    let root_plaintext = serde_json::json!({
        "schema_version": 1u32,
        "layout_id": "pack",
        "layout_generation": 7u64,
        "generation": 7u64,
        "segments": ["packs/index/op-00000000.json"],
    });
    let root_bytes = e2v_core::sync_support::encrypt_control_record_for_sync(
        &secrets,
        "pack-index-root",
        "pack-index-root",
        &serde_json::to_vec(&root_plaintext).unwrap(),
    )
    .unwrap();
    fs::write(cache_dir.join("root.json"), root_bytes).unwrap();

    let segment_plaintext = serde_json::json!({
        "schema_version": 1u32,
        "pack_id": "op-00000000",
        "data_path": "packs/data/op-00000000.bin",
        "entries": [
            {
                "object_id": "abc",
                "offset": 3u64,
                "length": 5u64,
            }
        ],
    });
    let segment_bytes = e2v_core::sync_support::encrypt_control_record_for_sync(
        &secrets,
        "pack-index-segment:packs/index/op-00000000.json",
        "pack-index-segment",
        &serde_json::to_vec(&segment_plaintext).unwrap(),
    )
    .unwrap();
    fs::write(
        cache_dir
            .join("segments")
            .join("packs__index__op-00000000.json"),
        segment_bytes,
    )
    .unwrap();

    let physical_ref =
        e2v_core::sync_support::load_cached_pack_physical_ref_for_object_id(&repo_root, "abc")
            .unwrap();

    assert_eq!(physical_ref.layout_id, "pack");
    assert_eq!(physical_ref.container_id, "packs/data/op-00000000.bin");
    assert_eq!(physical_ref.offset, Some(3));
    assert_eq!(physical_ref.length, 5);
}

#[test]
fn sync_support_reports_missing_cached_pack_entry_for_object_id() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();

    let control_dir = repo_root.join(".e2v");
    let secrets = e2v_core::sync_support::open_repo_secrets_for_sync(&control_dir).unwrap();
    let cache_dir = control_dir.join("cache").join("pack-index");
    fs::create_dir_all(cache_dir.join("segments")).unwrap();

    let root_plaintext = serde_json::json!({
        "schema_version": 1u32,
        "layout_id": "pack",
        "layout_generation": 7u64,
        "generation": 7u64,
        "segments": ["packs/index/op-00000000.json"],
    });
    let root_bytes = e2v_core::sync_support::encrypt_control_record_for_sync(
        &secrets,
        "pack-index-root",
        "pack-index-root",
        &serde_json::to_vec(&root_plaintext).unwrap(),
    )
    .unwrap();
    fs::write(cache_dir.join("root.json"), root_bytes).unwrap();

    let segment_plaintext = serde_json::json!({
        "schema_version": 1u32,
        "pack_id": "op-00000000",
        "data_path": "packs/data/op-00000000.bin",
        "entries": [],
    });
    let segment_bytes = e2v_core::sync_support::encrypt_control_record_for_sync(
        &secrets,
        "pack-index-segment:packs/index/op-00000000.json",
        "pack-index-segment",
        &serde_json::to_vec(&segment_plaintext).unwrap(),
    )
    .unwrap();
    fs::write(
        cache_dir
            .join("segments")
            .join("packs__index__op-00000000.json"),
        segment_bytes,
    )
    .unwrap();

    let error =
        e2v_core::sync_support::load_cached_pack_physical_ref_for_object_id(&repo_root, "abc")
            .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("cached pack index has no entry for object abc"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn sync_support_stops_scanning_cached_segments_after_resolving_target_object() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();

    let control_dir = repo_root.join(".e2v");
    let secrets = e2v_core::sync_support::open_repo_secrets_for_sync(&control_dir).unwrap();
    let cache_dir = control_dir.join("cache").join("pack-index");
    fs::create_dir_all(cache_dir.join("segments")).unwrap();

    let root_plaintext = serde_json::json!({
        "schema_version": 1u32,
        "layout_id": "pack",
        "layout_generation": 7u64,
        "generation": 7u64,
        "segments": [
            "packs/index/op-00000000.json",
            "packs/index/op-00000001.json",
        ],
    });
    let root_bytes = e2v_core::sync_support::encrypt_control_record_for_sync(
        &secrets,
        "pack-index-root",
        "pack-index-root",
        &serde_json::to_vec(&root_plaintext).unwrap(),
    )
    .unwrap();
    fs::write(cache_dir.join("root.json"), root_bytes).unwrap();

    let first_segment_plaintext = serde_json::json!({
        "schema_version": 1u32,
        "pack_id": "op-00000000",
        "data_path": "packs/data/op-00000000.bin",
        "entries": [
            {
                "object_id": "abc",
                "offset": 3u64,
                "length": 5u64,
            }
        ],
    });
    let first_segment_bytes = e2v_core::sync_support::encrypt_control_record_for_sync(
        &secrets,
        "pack-index-segment:packs/index/op-00000000.json",
        "pack-index-segment",
        &serde_json::to_vec(&first_segment_plaintext).unwrap(),
    )
    .unwrap();
    fs::write(
        cache_dir
            .join("segments")
            .join("packs__index__op-00000000.json"),
        first_segment_bytes,
    )
    .unwrap();

    fs::write(
        cache_dir
            .join("segments")
            .join("packs__index__op-00000001.json"),
        br#"{"tampered":true}"#,
    )
    .unwrap();

    let physical_ref =
        e2v_core::sync_support::load_cached_pack_physical_ref_for_object_id(&repo_root, "abc")
            .unwrap();

    assert_eq!(physical_ref.layout_id, "pack");
    assert_eq!(physical_ref.container_id, "packs/data/op-00000000.bin");
    assert_eq!(physical_ref.offset, Some(3));
    assert_eq!(physical_ref.length, 5);
}

#[test]
fn sync_support_rejects_cached_compacted_segment_with_traversing_pack_data_path() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();

    let control_dir = repo_root.join(".e2v");
    let secrets = e2v_core::sync_support::open_repo_secrets_for_sync(&control_dir).unwrap();
    let cache_dir = control_dir.join("cache").join("pack-index");
    fs::create_dir_all(cache_dir.join("segments")).unwrap();

    let root_plaintext = serde_json::json!({
        "schema_version": 1u32,
        "layout_id": "pack",
        "layout_generation": 7u64,
        "generation": 7u64,
        "segments": ["pack-index/segments/compact-00000000000000000007.json"],
    });
    let root_bytes = e2v_core::sync_support::encrypt_control_record_for_sync(
        &secrets,
        "pack-index-root",
        "pack-index-root",
        &serde_json::to_vec(&root_plaintext).unwrap(),
    )
    .unwrap();
    fs::write(cache_dir.join("root.json"), root_bytes).unwrap();

    let segment_plaintext = serde_json::json!({
        "schema_version": 1u32,
        "entries": [
            {
                "object_id": "abc",
                "data_path": "packs/data/../escape.bin",
                "offset": 3u64,
                "length": 5u64,
            }
        ],
    });
    let segment_bytes = e2v_core::sync_support::encrypt_control_record_for_sync(
        &secrets,
        "pack-index-segment:pack-index/segments/compact-00000000000000000007.json",
        "pack-index-segment",
        &serde_json::to_vec(&segment_plaintext).unwrap(),
    )
    .unwrap();
    fs::write(
        cache_dir
            .join("segments")
            .join("pack-index__segments__compact-00000000000000000007.json"),
        segment_bytes,
    )
    .unwrap();

    let error =
        e2v_core::sync_support::load_cached_pack_physical_ref_for_object_id(&repo_root, "abc")
            .unwrap_err();

    assert!(
        error.to_string().contains("path traversal")
            || error
                .to_string()
                .contains("invalid aggregate pack data path"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn sync_support_prunes_corrupted_cached_pack_index_root_after_failed_lookup() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();

    let cache_dir = repo_root.join(".e2v").join("cache").join("pack-index");
    fs::create_dir_all(cache_dir.join("segments")).unwrap();
    let root_path = cache_dir.join("root.json");
    fs::write(&root_path, b"corrupt-pack-index-root").unwrap();

    let error =
        e2v_core::sync_support::load_cached_pack_physical_ref_for_object_id(&repo_root, "abc")
            .unwrap_err();

    assert!(
        !root_path.exists(),
        "corrupted cached pack-index root should be pruned after failed lookup: {root_path:?}"
    );
    let message = error.to_string();
    assert!(
        message.contains("pack index root")
            || message.contains("object authentication failed")
            || message.contains("unsupported"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn sync_support_prunes_corrupted_cached_pack_index_segment_after_failed_lookup() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();

    let control_dir = repo_root.join(".e2v");
    let secrets = e2v_core::sync_support::open_repo_secrets_for_sync(&control_dir).unwrap();
    let cache_dir = control_dir.join("cache").join("pack-index");
    fs::create_dir_all(cache_dir.join("segments")).unwrap();

    let root_plaintext = serde_json::json!({
        "schema_version": 1u32,
        "layout_id": "pack",
        "layout_generation": 7u64,
        "generation": 7u64,
        "segments": ["packs/index/op-00000000.json"],
    });
    let root_bytes = e2v_core::sync_support::encrypt_control_record_for_sync(
        &secrets,
        "pack-index-root",
        "pack-index-root",
        &serde_json::to_vec(&root_plaintext).unwrap(),
    )
    .unwrap();
    fs::write(cache_dir.join("root.json"), root_bytes).unwrap();

    let segment_path = cache_dir
        .join("segments")
        .join("packs__index__op-00000000.json");
    fs::write(&segment_path, b"corrupt-pack-index-segment").unwrap();

    let error =
        e2v_core::sync_support::load_cached_pack_physical_ref_for_object_id(&repo_root, "abc")
            .unwrap_err();

    assert!(
        !segment_path.exists(),
        "corrupted cached pack-index segment should be pruned after failed lookup: {segment_path:?}"
    );
    let message = error.to_string();
    assert!(
        message.contains("pack index segment")
            || message.contains("object authentication failed")
            || message.contains("unsupported"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn read_range_falls_back_to_cached_pack_data_when_loose_chunk_is_missing() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("hello.txt"), "hello world").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "cached-pack-fallback".to_string(),
        })
        .unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(&commit.snapshot_id).unwrap();
    let file = read_service.open_file(&snapshot, "hello.txt").unwrap();
    let chunk_id = file.debug_chunk_ids()[0].clone();
    let control_dir = repo_root.join(".e2v");
    let chunk_path = control_dir.join("objects").join(format!("{chunk_id}.json"));
    let chunk_bytes = fs::read(&chunk_path).unwrap();
    let secrets = e2v_core::sync_support::open_repo_secrets_for_sync(&control_dir).unwrap();
    let cache_dir = control_dir.join("cache").join("pack-index");
    fs::create_dir_all(cache_dir.join("segments")).unwrap();

    let root_plaintext = serde_json::json!({
        "schema_version": 1u32,
        "layout_id": "pack",
        "layout_generation": 7u64,
        "generation": 7u64,
        "segments": ["packs/index/op-00000000.json"],
    });
    let root_bytes = e2v_core::sync_support::encrypt_control_record_for_sync(
        &secrets,
        "pack-index-root",
        "pack-index-root",
        &serde_json::to_vec(&root_plaintext).unwrap(),
    )
    .unwrap();
    fs::write(cache_dir.join("root.json"), root_bytes).unwrap();

    let segment_plaintext = serde_json::json!({
        "schema_version": 1u32,
        "pack_id": "op-00000000",
        "data_path": "packs/data/op-00000000.bin",
        "entries": [
            {
                "object_id": chunk_id,
                "offset": 0u64,
                "length": chunk_bytes.len() as u64,
            }
        ],
    });
    let segment_bytes = e2v_core::sync_support::encrypt_control_record_for_sync(
        &secrets,
        "pack-index-segment:packs/index/op-00000000.json",
        "pack-index-segment",
        &serde_json::to_vec(&segment_plaintext).unwrap(),
    )
    .unwrap();
    fs::write(
        cache_dir
            .join("segments")
            .join("packs__index__op-00000000.json"),
        segment_bytes,
    )
    .unwrap();

    let cached_pack_path = control_dir
        .join("cache")
        .join("pack-data")
        .join("packs")
        .join("data")
        .join("op-00000000.bin");
    let cached_pack_hash_path = cached_pack_path.with_extension("bin.blake3");
    fs::create_dir_all(cached_pack_path.parent().unwrap()).unwrap();
    fs::write(&cached_pack_path, &chunk_bytes).unwrap();
    fs::write(
        &cached_pack_hash_path,
        blake3::hash(&chunk_bytes).to_hex().to_string(),
    )
    .unwrap();
    fs::remove_file(&chunk_path).unwrap();

    let bytes = read_service.read_range(&file, 0, 5).unwrap();

    assert_eq!(bytes, b"hello");
}

#[test]
fn read_range_reports_unavailable_fallback_when_cached_pack_data_is_missing() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("hello.txt"), "hello world").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "cached-pack-fallback-missing-pack".to_string(),
        })
        .unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(&commit.snapshot_id).unwrap();
    let file = read_service.open_file(&snapshot, "hello.txt").unwrap();
    let chunk_id = file.debug_chunk_ids()[0].clone();
    let control_dir = repo_root.join(".e2v");
    let chunk_path = control_dir.join("objects").join(format!("{chunk_id}.json"));
    let chunk_bytes = fs::read(&chunk_path).unwrap();
    let secrets = e2v_core::sync_support::open_repo_secrets_for_sync(&control_dir).unwrap();
    let cache_dir = control_dir.join("cache").join("pack-index");
    fs::create_dir_all(cache_dir.join("segments")).unwrap();

    let root_plaintext = serde_json::json!({
        "schema_version": 1u32,
        "layout_id": "pack",
        "layout_generation": 7u64,
        "generation": 7u64,
        "segments": ["packs/index/op-00000000.json"],
    });
    let root_bytes = e2v_core::sync_support::encrypt_control_record_for_sync(
        &secrets,
        "pack-index-root",
        "pack-index-root",
        &serde_json::to_vec(&root_plaintext).unwrap(),
    )
    .unwrap();
    fs::write(cache_dir.join("root.json"), root_bytes).unwrap();

    let segment_plaintext = serde_json::json!({
        "schema_version": 1u32,
        "pack_id": "op-00000000",
        "data_path": "packs/data/op-00000000.bin",
        "entries": [
            {
                "object_id": chunk_id,
                "offset": 0u64,
                "length": chunk_bytes.len() as u64,
            }
        ],
    });
    let segment_bytes = e2v_core::sync_support::encrypt_control_record_for_sync(
        &secrets,
        "pack-index-segment:packs/index/op-00000000.json",
        "pack-index-segment",
        &serde_json::to_vec(&segment_plaintext).unwrap(),
    )
    .unwrap();
    fs::write(
        cache_dir
            .join("segments")
            .join("packs__index__op-00000000.json"),
        segment_bytes,
    )
    .unwrap();

    fs::remove_file(&chunk_path).unwrap();

    let error = read_service.read_range(&file, 0, 5).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("stale-layout fallback unavailable"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn read_range_treats_corrupted_cached_pack_data_as_unavailable_fallback() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("hello.txt"), "hello world").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "cached-pack-fallback-corrupted-pack".to_string(),
        })
        .unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(&commit.snapshot_id).unwrap();
    let file = read_service.open_file(&snapshot, "hello.txt").unwrap();
    let chunk_id = file.debug_chunk_ids()[0].clone();
    let control_dir = repo_root.join(".e2v");
    let chunk_path = control_dir.join("objects").join(format!("{chunk_id}.json"));
    let chunk_bytes = fs::read(&chunk_path).unwrap();
    let secrets = e2v_core::sync_support::open_repo_secrets_for_sync(&control_dir).unwrap();
    let cache_dir = control_dir.join("cache").join("pack-index");
    fs::create_dir_all(cache_dir.join("segments")).unwrap();

    let root_plaintext = serde_json::json!({
        "schema_version": 1u32,
        "layout_id": "pack",
        "layout_generation": 7u64,
        "generation": 7u64,
        "segments": ["packs/index/op-00000000.json"],
    });
    let root_bytes = e2v_core::sync_support::encrypt_control_record_for_sync(
        &secrets,
        "pack-index-root",
        "pack-index-root",
        &serde_json::to_vec(&root_plaintext).unwrap(),
    )
    .unwrap();
    fs::write(cache_dir.join("root.json"), root_bytes).unwrap();

    let segment_plaintext = serde_json::json!({
        "schema_version": 1u32,
        "pack_id": "op-00000000",
        "data_path": "packs/data/op-00000000.bin",
        "entries": [
            {
                "object_id": chunk_id,
                "offset": 0u64,
                "length": chunk_bytes.len() as u64,
            }
        ],
    });
    let segment_bytes = e2v_core::sync_support::encrypt_control_record_for_sync(
        &secrets,
        "pack-index-segment:packs/index/op-00000000.json",
        "pack-index-segment",
        &serde_json::to_vec(&segment_plaintext).unwrap(),
    )
    .unwrap();
    fs::write(
        cache_dir
            .join("segments")
            .join("packs__index__op-00000000.json"),
        segment_bytes,
    )
    .unwrap();

    let cached_pack_path = control_dir
        .join("cache")
        .join("pack-data")
        .join("packs")
        .join("data")
        .join("op-00000000.bin");
    let cached_pack_hash_path = cached_pack_path.with_extension("bin.blake3");
    fs::create_dir_all(cached_pack_path.parent().unwrap()).unwrap();
    fs::write(&cached_pack_path, &chunk_bytes).unwrap();
    fs::write(
        &cached_pack_hash_path,
        blake3::hash(&chunk_bytes).to_hex().to_string(),
    )
    .unwrap();
    let mut corrupted_pack_bytes = fs::read(&cached_pack_path).unwrap();
    corrupted_pack_bytes[0] ^= 0x01;
    fs::write(&cached_pack_path, corrupted_pack_bytes).unwrap();
    fs::remove_file(&chunk_path).unwrap();

    let error = read_service.read_range(&file, 0, 5).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("stale-layout fallback unavailable"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn read_range_prunes_corrupted_cached_pack_data_after_failed_fallback() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("hello.txt"), "hello world").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "cached-pack-fallback-prune-corrupted-pack".to_string(),
        })
        .unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(&commit.snapshot_id).unwrap();
    let file = read_service.open_file(&snapshot, "hello.txt").unwrap();
    let chunk_id = file.debug_chunk_ids()[0].clone();
    let control_dir = repo_root.join(".e2v");
    let chunk_path = control_dir.join("objects").join(format!("{chunk_id}.json"));
    let chunk_bytes = fs::read(&chunk_path).unwrap();
    let secrets = e2v_core::sync_support::open_repo_secrets_for_sync(&control_dir).unwrap();
    let cache_dir = control_dir.join("cache").join("pack-index");
    fs::create_dir_all(cache_dir.join("segments")).unwrap();

    let root_plaintext = serde_json::json!({
        "schema_version": 1u32,
        "layout_id": "pack",
        "layout_generation": 7u64,
        "generation": 7u64,
        "segments": ["packs/index/op-00000000.json"],
    });
    let root_bytes = e2v_core::sync_support::encrypt_control_record_for_sync(
        &secrets,
        "pack-index-root",
        "pack-index-root",
        &serde_json::to_vec(&root_plaintext).unwrap(),
    )
    .unwrap();
    fs::write(cache_dir.join("root.json"), root_bytes).unwrap();

    let segment_plaintext = serde_json::json!({
        "schema_version": 1u32,
        "pack_id": "op-00000000",
        "data_path": "packs/data/op-00000000.bin",
        "entries": [
            {
                "object_id": chunk_id,
                "offset": 0u64,
                "length": chunk_bytes.len() as u64,
            }
        ],
    });
    let segment_bytes = e2v_core::sync_support::encrypt_control_record_for_sync(
        &secrets,
        "pack-index-segment:packs/index/op-00000000.json",
        "pack-index-segment",
        &serde_json::to_vec(&segment_plaintext).unwrap(),
    )
    .unwrap();
    fs::write(
        cache_dir
            .join("segments")
            .join("packs__index__op-00000000.json"),
        segment_bytes,
    )
    .unwrap();

    let cached_pack_path = control_dir
        .join("cache")
        .join("pack-data")
        .join("packs")
        .join("data")
        .join("op-00000000.bin");
    let cached_pack_hash_path = cached_pack_path.with_extension("bin.blake3");
    fs::create_dir_all(cached_pack_path.parent().unwrap()).unwrap();
    fs::write(&cached_pack_path, &chunk_bytes).unwrap();
    let mut corrupted_pack_bytes = fs::read(&cached_pack_path).unwrap();
    corrupted_pack_bytes[0] ^= 0x01;
    fs::write(&cached_pack_path, corrupted_pack_bytes).unwrap();
    fs::write(
        &cached_pack_hash_path,
        blake3::hash(&fs::read(&cached_pack_path).unwrap())
            .to_hex()
            .to_string(),
    )
    .unwrap();
    fs::remove_file(&chunk_path).unwrap();

    let _ = read_service.read_range(&file, 0, 5).unwrap_err();

    assert!(
        !cached_pack_path.exists(),
        "corrupted cached pack-data entry should be pruned after failed fallback: {cached_pack_path:?}"
    );
    assert!(
        !cached_pack_hash_path.exists(),
        "corrupted cached pack-data hash sidecar should be pruned with the data entry: {cached_pack_hash_path:?}"
    );
}

#[test]
fn read_range_prunes_unreadable_cached_pack_data_hash_sidecar_after_failed_fallback() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("hello.txt"), "hello world").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "cached-pack-fallback-prune-hash-sidecar-conflict".to_string(),
        })
        .unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(&commit.snapshot_id).unwrap();
    let file = read_service.open_file(&snapshot, "hello.txt").unwrap();
    let chunk_id = file.debug_chunk_ids()[0].clone();
    let control_dir = repo_root.join(".e2v");
    let chunk_path = control_dir.join("objects").join(format!("{chunk_id}.json"));
    let chunk_bytes = fs::read(&chunk_path).unwrap();
    let secrets = e2v_core::sync_support::open_repo_secrets_for_sync(&control_dir).unwrap();
    let cache_dir = control_dir.join("cache").join("pack-index");
    fs::create_dir_all(cache_dir.join("segments")).unwrap();

    let root_plaintext = serde_json::json!({
        "schema_version": 1u32,
        "layout_id": "pack",
        "layout_generation": 7u64,
        "generation": 7u64,
        "segments": ["packs/index/op-00000000.json"],
    });
    let root_bytes = e2v_core::sync_support::encrypt_control_record_for_sync(
        &secrets,
        "pack-index-root",
        "pack-index-root",
        &serde_json::to_vec(&root_plaintext).unwrap(),
    )
    .unwrap();
    fs::write(cache_dir.join("root.json"), root_bytes).unwrap();

    let segment_plaintext = serde_json::json!({
        "schema_version": 1u32,
        "pack_id": "op-00000000",
        "data_path": "packs/data/op-00000000.bin",
        "entries": [
            {
                "object_id": chunk_id,
                "offset": 0u64,
                "length": chunk_bytes.len() as u64,
            }
        ],
    });
    let segment_bytes = e2v_core::sync_support::encrypt_control_record_for_sync(
        &secrets,
        "pack-index-segment:packs/index/op-00000000.json",
        "pack-index-segment",
        &serde_json::to_vec(&segment_plaintext).unwrap(),
    )
    .unwrap();
    fs::write(
        cache_dir
            .join("segments")
            .join("packs__index__op-00000000.json"),
        segment_bytes,
    )
    .unwrap();

    let cached_pack_path = control_dir
        .join("cache")
        .join("pack-data")
        .join("packs")
        .join("data")
        .join("op-00000000.bin");
    let cached_pack_hash_path = cached_pack_path.with_extension("bin.blake3");
    fs::create_dir_all(cached_pack_path.parent().unwrap()).unwrap();
    fs::write(&cached_pack_path, &chunk_bytes).unwrap();
    fs::write(
        &cached_pack_hash_path,
        blake3::hash(&chunk_bytes).to_hex().to_string(),
    )
    .unwrap();
    fs::remove_file(&cached_pack_hash_path).unwrap();
    fs::create_dir(&cached_pack_hash_path).unwrap();
    fs::remove_file(&chunk_path).unwrap();

    let error = read_service.read_range(&file, 0, 5).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("stale-layout fallback unavailable"),
        "unexpected error: {error:#}"
    );
    assert!(
        !cached_pack_path.exists(),
        "cached pack-data entry should be pruned after hash sidecar path conflict: {cached_pack_path:?}"
    );
    assert!(
        !cached_pack_hash_path.exists(),
        "cached pack-data hash sidecar conflict should be pruned after failed fallback: {cached_pack_hash_path:?}"
    );
}

#[test]
fn read_range_prunes_cached_pack_data_path_conflict_after_failed_fallback() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("hello.txt"), "hello world").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "cached-pack-fallback-prune-pack-path-conflict".to_string(),
        })
        .unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(&commit.snapshot_id).unwrap();
    let file = read_service.open_file(&snapshot, "hello.txt").unwrap();
    let chunk_id = file.debug_chunk_ids()[0].clone();
    let control_dir = repo_root.join(".e2v");
    let chunk_path = control_dir.join("objects").join(format!("{chunk_id}.json"));
    let chunk_bytes = fs::read(&chunk_path).unwrap();
    let secrets = e2v_core::sync_support::open_repo_secrets_for_sync(&control_dir).unwrap();
    let cache_dir = control_dir.join("cache").join("pack-index");
    fs::create_dir_all(cache_dir.join("segments")).unwrap();

    let root_plaintext = serde_json::json!({
        "schema_version": 1u32,
        "layout_id": "pack",
        "layout_generation": 7u64,
        "generation": 7u64,
        "segments": ["packs/index/op-00000000.json"],
    });
    let root_bytes = e2v_core::sync_support::encrypt_control_record_for_sync(
        &secrets,
        "pack-index-root",
        "pack-index-root",
        &serde_json::to_vec(&root_plaintext).unwrap(),
    )
    .unwrap();
    fs::write(cache_dir.join("root.json"), root_bytes).unwrap();

    let segment_plaintext = serde_json::json!({
        "schema_version": 1u32,
        "pack_id": "op-00000000",
        "data_path": "packs/data/op-00000000.bin",
        "entries": [
            {
                "object_id": chunk_id,
                "offset": 0u64,
                "length": chunk_bytes.len() as u64,
            }
        ],
    });
    let segment_bytes = e2v_core::sync_support::encrypt_control_record_for_sync(
        &secrets,
        "pack-index-segment:packs/index/op-00000000.json",
        "pack-index-segment",
        &serde_json::to_vec(&segment_plaintext).unwrap(),
    )
    .unwrap();
    fs::write(
        cache_dir
            .join("segments")
            .join("packs__index__op-00000000.json"),
        segment_bytes,
    )
    .unwrap();

    let cached_pack_path = control_dir
        .join("cache")
        .join("pack-data")
        .join("packs")
        .join("data")
        .join("op-00000000.bin");
    let cached_pack_hash_path = cached_pack_path.with_extension("bin.blake3");
    fs::create_dir_all(cached_pack_path.parent().unwrap()).unwrap();
    fs::write(&cached_pack_path, &chunk_bytes).unwrap();
    fs::write(
        &cached_pack_hash_path,
        blake3::hash(&chunk_bytes).to_hex().to_string(),
    )
    .unwrap();
    fs::remove_file(&cached_pack_path).unwrap();
    fs::create_dir(&cached_pack_path).unwrap();
    fs::remove_file(&chunk_path).unwrap();

    let error = read_service.read_range(&file, 0, 5).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("stale-layout fallback unavailable"),
        "unexpected error: {error:#}"
    );
    assert!(
        !cached_pack_path.exists(),
        "cached pack-data path conflict should be pruned after failed fallback: {cached_pack_path:?}"
    );
    assert!(
        !cached_pack_hash_path.exists(),
        "cached pack-data hash sidecar should be pruned with the path conflict: {cached_pack_hash_path:?}"
    );
}

#[test]
fn read_range_still_prefers_healthy_loose_chunk_over_cached_pack_fallback() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("hello.txt"), "hello world").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "healthy-loose-preferred".to_string(),
        })
        .unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(&commit.snapshot_id).unwrap();
    let file = read_service.open_file(&snapshot, "hello.txt").unwrap();

    let bytes = read_service.read_range(&file, 0, 5).unwrap();

    assert_eq!(bytes, b"hello");
}

#[test]
fn read_range_only_requires_chunks_covering_the_requested_prefix() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    let mut content = Vec::with_capacity(16 * 1024 * 1024);
    for index in 0..(16 * 1024 * 1024) {
        content.push((index % 251) as u8);
    }
    fs::write(repo_root.join("large.bin"), &content).unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "prefix-range".to_string(),
        })
        .unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(&commit.snapshot_id).unwrap();
    let file = read_service.open_file(&snapshot, "large.bin").unwrap();
    assert!(
        file.chunk_count() >= 2,
        "expected multi-chunk file, got {} chunks",
        file.chunk_count()
    );

    let later_chunk_id = file.debug_chunk_ids().last().unwrap().clone();
    let later_chunk_path = repo_root
        .join(".e2v")
        .join("objects")
        .join(format!("{later_chunk_id}.json"));
    let mut bytes = fs::read(&later_chunk_path).unwrap();
    let last_index = bytes.len() - 1;
    bytes[last_index] ^= 0x01;
    fs::write(&later_chunk_path, bytes).unwrap();

    let prefix = read_service.read_range(&file, 0, 16).unwrap();

    assert_eq!(prefix, content[..16].to_vec());
}

#[test]
fn read_range_only_requires_chunks_covering_the_requested_suffix() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    let mut content = Vec::with_capacity(16 * 1024 * 1024);
    for index in 0..(16 * 1024 * 1024) {
        content.push((index % 251) as u8);
    }
    fs::write(repo_root.join("large.bin"), &content).unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "suffix-range".to_string(),
        })
        .unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(&commit.snapshot_id).unwrap();
    let file = read_service.open_file(&snapshot, "large.bin").unwrap();
    assert!(
        file.chunk_count() >= 2,
        "expected multi-chunk file, got {} chunks",
        file.chunk_count()
    );

    let first_chunk_id = file.debug_chunk_ids().first().unwrap().clone();
    let first_chunk_path = repo_root
        .join(".e2v")
        .join("objects")
        .join(format!("{first_chunk_id}.json"));
    let mut bytes = fs::read(&first_chunk_path).unwrap();
    let last_index = bytes.len() - 1;
    bytes[last_index] ^= 0x01;
    fs::write(&first_chunk_path, bytes).unwrap();

    let suffix_len = 64usize;
    let suffix_offset = content.len() - suffix_len;
    let suffix = read_service
        .read_range(&file, suffix_offset, suffix_len)
        .unwrap();

    assert_eq!(suffix, content[suffix_offset..].to_vec());
}

#[test]
fn read_range_prefix_does_not_require_unrelated_later_file_shard_metadata() {
    let _guard = e2v_core::testing::override_max_file_chunks_per_object_for_test(2);
    let _chunk_guard = e2v_core::testing::override_fixed_span_bytes_for_test(1024 * 1024);
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    let mut content = Vec::with_capacity(9_000_000);
    for index in 0..9_000_000usize {
        content.push((index % 251) as u8);
    }
    fs::write(repo_root.join("large.bin"), &content).unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "prefix-file-shard-range".to_string(),
        })
        .unwrap();

    let manifest_store = ManifestStore::new(&repo_root);
    let snapshot_manifest = manifest_store.get_snapshot(&commit.snapshot_id).unwrap();
    let tree = manifest_store
        .get_tree_node(&snapshot_manifest.root_tree_id)
        .unwrap();
    let file_entry = tree
        .entries
        .iter()
        .find(|entry| entry.name == "large.bin" && entry.kind == "file")
        .unwrap();
    let file_manifest = manifest_store.get_file(&file_entry.object_id).unwrap();
    assert!(
        file_manifest.shard_ids.len() >= 2,
        "expected multiple file_shard objects, got {}",
        file_manifest.shard_ids.len()
    );

    let later_shard_path = repo_root
        .join(".e2v")
        .join("objects")
        .join(format!("{}.json", file_manifest.shard_ids.last().unwrap()));
    let mut bytes = fs::read(&later_shard_path).unwrap();
    let last_index = bytes.len() - 1;
    bytes[last_index] ^= 0x01;
    fs::write(&later_shard_path, bytes).unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(&commit.snapshot_id).unwrap();
    let file = read_service.open_file(&snapshot, "large.bin").unwrap();
    let prefix = read_service.read_range(&file, 0, 16).unwrap();

    assert_eq!(prefix, content[..16].to_vec());
}

#[test]
fn read_range_suffix_does_not_require_unrelated_earlier_file_shard_metadata() {
    let _guard = e2v_core::testing::override_max_file_chunks_per_object_for_test(2);
    let _chunk_guard = e2v_core::testing::override_fixed_span_bytes_for_test(1024 * 1024);
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    let mut content = Vec::with_capacity(9_000_000);
    for index in 0..9_000_000usize {
        content.push((index % 251) as u8);
    }
    fs::write(repo_root.join("large.bin"), &content).unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "suffix-file-shard-range".to_string(),
        })
        .unwrap();

    let manifest_store = ManifestStore::new(&repo_root);
    let snapshot_manifest = manifest_store.get_snapshot(&commit.snapshot_id).unwrap();
    let tree = manifest_store
        .get_tree_node(&snapshot_manifest.root_tree_id)
        .unwrap();
    let file_entry = tree
        .entries
        .iter()
        .find(|entry| entry.name == "large.bin" && entry.kind == "file")
        .unwrap();
    let file_manifest = manifest_store.get_file(&file_entry.object_id).unwrap();
    assert!(
        file_manifest.shard_ids.len() >= 2,
        "expected multiple file_shard objects, got {}",
        file_manifest.shard_ids.len()
    );

    let first_shard_path = repo_root
        .join(".e2v")
        .join("objects")
        .join(format!("{}.json", file_manifest.shard_ids.first().unwrap()));
    let mut bytes = fs::read(&first_shard_path).unwrap();
    let last_index = bytes.len() - 1;
    bytes[last_index] ^= 0x01;
    fs::write(&first_shard_path, bytes).unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(&commit.snapshot_id).unwrap();
    let file = read_service.open_file(&snapshot, "large.bin").unwrap();
    let suffix_len = 64usize;
    let suffix_offset = content.len() - suffix_len;
    let suffix = read_service
        .read_range(&file, suffix_offset, suffix_len)
        .unwrap();

    assert_eq!(suffix, content[suffix_offset..].to_vec());
}

#[test]
fn read_range_rejects_authenticated_file_graph_with_incomplete_chunk_coverage() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("hello.txt"), "hello world").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "incomplete-coverage".to_string(),
        })
        .unwrap();

    let manifest_store = ManifestStore::new(&repo_root);
    let snapshot = manifest_store.get_snapshot(&commit.snapshot_id).unwrap();
    let root_tree = manifest_store
        .get_tree_node(&snapshot.root_tree_id)
        .unwrap();
    let file_entry = root_tree
        .entries
        .iter()
        .find(|entry| entry.name == "hello.txt")
        .unwrap()
        .clone();
    let mut file_manifest = manifest_store.get_file(&file_entry.object_id).unwrap();
    file_manifest.file_size += 5;

    let control_dir = repo_root.join(".e2v");
    let secrets = e2v_core::sync_support::open_repo_secrets_for_sync(&control_dir).unwrap();
    let object_store = e2v_store::DirectLayoutObjectStore::new(&control_dir, secrets);
    let tampered_file_id = object_store
        .put_object("file", &postcard::to_stdvec(&file_manifest).unwrap())
        .unwrap();

    let mut tampered_tree: ManifestTreeObject = root_tree.clone();
    tampered_tree
        .entries
        .iter_mut()
        .find(|entry| entry.name == "hello.txt")
        .unwrap()
        .object_id = tampered_file_id;
    let tampered_tree_id = object_store
        .put_object("tree", &postcard::to_stdvec(&tampered_tree).unwrap())
        .unwrap();

    let mut tampered_snapshot: ManifestSnapshotObject = snapshot.clone();
    tampered_snapshot.root_tree_id = tampered_tree_id;
    let tampered_snapshot_id = object_store
        .put_object(
            "snapshot",
            &postcard::to_stdvec(&tampered_snapshot).unwrap(),
        )
        .unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let tampered_snapshot_handle = read_service.open_snapshot(&tampered_snapshot_id).unwrap();
    let file = read_service
        .open_file(&tampered_snapshot_handle, "hello.txt")
        .unwrap();

    let error = read_service.read_range(&file, 0, usize::MAX).unwrap_err();

    assert!(
        error.to_string().contains("chunk")
            || error.to_string().contains("coverage")
            || error.to_string().contains("truncated"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn read_range_rejects_file_manifest_with_mismatched_chunk_length_metadata() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("hello.txt"), "hello world").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "mismatched-chunk-metadata".to_string(),
        })
        .unwrap();

    let manifest_store = ManifestStore::new(&repo_root);
    let snapshot = manifest_store.get_snapshot(&commit.snapshot_id).unwrap();
    let root_tree = manifest_store
        .get_tree_node(&snapshot.root_tree_id)
        .unwrap();
    let file_entry = root_tree
        .entries
        .iter()
        .find(|entry| entry.name == "hello.txt")
        .unwrap()
        .clone();
    let mut file_manifest = manifest_store.get_file(&file_entry.object_id).unwrap();
    file_manifest.chunk_lengths.clear();

    let control_dir = repo_root.join(".e2v");
    let secrets = e2v_core::sync_support::open_repo_secrets_for_sync(&control_dir).unwrap();
    let object_store = e2v_store::DirectLayoutObjectStore::new(&control_dir, secrets);
    let tampered_file_id = object_store
        .put_object("file", &postcard::to_stdvec(&file_manifest).unwrap())
        .unwrap();

    let mut tampered_tree: ManifestTreeObject = root_tree.clone();
    tampered_tree
        .entries
        .iter_mut()
        .find(|entry| entry.name == "hello.txt")
        .unwrap()
        .object_id = tampered_file_id;
    let tampered_tree_id = object_store
        .put_object("tree", &postcard::to_stdvec(&tampered_tree).unwrap())
        .unwrap();

    let mut tampered_snapshot: ManifestSnapshotObject = snapshot.clone();
    tampered_snapshot.root_tree_id = tampered_tree_id;
    let tampered_snapshot_id = object_store
        .put_object(
            "snapshot",
            &postcard::to_stdvec(&tampered_snapshot).unwrap(),
        )
        .unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let tampered_snapshot_handle = read_service.open_snapshot(&tampered_snapshot_id).unwrap();
    let file = read_service
        .open_file(&tampered_snapshot_handle, "hello.txt")
        .unwrap();

    let error = read_service.read_range(&file, 0, usize::MAX).unwrap_err();

    assert!(
        error.to_string().contains("metadata") || error.to_string().contains("chunk"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn manifest_store_rejects_authenticated_snapshot_graph_with_path_traversal_chunk_id() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("hello.txt"), "hello world").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "tampered-chunk-id".to_string(),
        })
        .unwrap();

    let manifest_store = ManifestStore::new(&repo_root);
    let snapshot = manifest_store.get_snapshot(&commit.snapshot_id).unwrap();
    let root_tree = manifest_store
        .get_tree_node(&snapshot.root_tree_id)
        .unwrap();
    let file_entry = root_tree
        .entries
        .iter()
        .find(|entry| entry.name == "hello.txt")
        .unwrap()
        .clone();
    let mut file_manifest = manifest_store.get_file(&file_entry.object_id).unwrap();
    file_manifest.chunks = vec!["..\\evil".to_string()];

    let control_dir = repo_root.join(".e2v");
    let secrets = e2v_core::sync_support::open_repo_secrets_for_sync(&control_dir).unwrap();
    let object_store = e2v_store::DirectLayoutObjectStore::new(&control_dir, secrets);
    let tampered_file_id = object_store
        .put_object("file", &postcard::to_stdvec(&file_manifest).unwrap())
        .unwrap();

    let mut tampered_tree: ManifestTreeObject = root_tree.clone();
    tampered_tree
        .entries
        .iter_mut()
        .find(|entry| entry.name == "hello.txt")
        .unwrap()
        .object_id = tampered_file_id;
    let tampered_tree_id = object_store
        .put_object("tree", &postcard::to_stdvec(&tampered_tree).unwrap())
        .unwrap();

    let mut tampered_snapshot: ManifestSnapshotObject = snapshot.clone();
    tampered_snapshot.root_tree_id = tampered_tree_id;
    let tampered_snapshot_id = object_store
        .put_object(
            "snapshot",
            &postcard::to_stdvec(&tampered_snapshot).unwrap(),
        )
        .unwrap();

    let error = manifest_store
        .collect_reachable_object_ids(&tampered_snapshot_id)
        .unwrap_err();

    assert!(
        error.to_string().contains("chunk")
            || error.to_string().contains("object id")
            || error.to_string().contains("path"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn traversal_rejects_tampered_tree_objects() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::create_dir_all(repo_root.join("nested")).unwrap();
    fs::write(repo_root.join("nested").join("hello.txt"), "hello nested").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "tamper-tree".to_string(),
        })
        .unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(&commit.snapshot_id).unwrap();
    let manifest_store = ManifestStore::new(&repo_root);
    let manifest_snapshot = manifest_store.get_snapshot(&commit.snapshot_id).unwrap();
    let root_tree_id = manifest_snapshot.root_tree_id;
    let tree_path = repo_root
        .join(".e2v")
        .join("objects")
        .join(format!("{root_tree_id}.json"));
    let mut bytes = fs::read(&tree_path).unwrap();
    let last_index = bytes.len() - 1;
    bytes[last_index] ^= 0x01;
    fs::write(&tree_path, bytes).unwrap();

    let error = read_service.read_dir(&snapshot, "").unwrap_err();

    assert!(
        error.to_string().contains("authentication"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn large_directories_are_committed_and_readable_via_tree_sharding() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();

    for index in 0..4100usize {
        fs::write(repo_root.join(format!("file-{index:04}.txt")), b"x").unwrap();
    }

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "sharded-directory".to_string(),
        })
        .unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(&commit.snapshot_id).unwrap();
    let entries = read_service.read_dir(&snapshot, "").unwrap();
    let walked = ManifestStore::new(&repo_root)
        .walk_tree(&commit.snapshot_id)
        .unwrap();

    assert!(
        entries.len() >= 4100,
        "expected sharded read_dir to expose all directory entries"
    );
    assert_eq!(walked.len(), 4100);
}

#[cfg(windows)]
#[test]
fn commit_skips_locked_files_and_reports_warning() {
    use std::os::windows::fs::OpenOptionsExt;

    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("good.txt"), "good").unwrap();
    fs::write(repo_root.join("locked.txt"), "locked").unwrap();

    let _locked = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(repo_root.join("locked.txt"))
        .unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "skip-locked".to_string(),
        })
        .unwrap();

    assert_eq!(commit.committed_files, 1);
    assert_eq!(commit.warnings.len(), 1);
    assert!(commit.warnings[0].contains("locked.txt"));

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(&commit.snapshot_id).unwrap();
    let entries = read_service.read_dir(&snapshot, "").unwrap();
    assert!(entries.iter().any(|entry| entry.name == "good.txt"));
    assert!(!entries.iter().any(|entry| entry.name == "locked.txt"));
}

#[test]
fn verify_object_accepts_a_healthy_chunk() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("hello.txt"), "hello world").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "verify-object".to_string(),
        })
        .unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(&commit.snapshot_id).unwrap();
    let file = read_service.open_file(&snapshot, "hello.txt").unwrap();
    let chunk_id = file.debug_chunk_ids()[0].clone();

    facade
        .verify_object(&repo_root, &chunk_id, "chunk")
        .unwrap();
}

#[test]
fn verify_object_rejects_tampered_chunk() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("hello.txt"), "hello world").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "verify-object".to_string(),
        })
        .unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(&commit.snapshot_id).unwrap();
    let file = read_service.open_file(&snapshot, "hello.txt").unwrap();
    let chunk_id = file.debug_chunk_ids()[0].clone();
    let chunk_path = repo_root
        .join(".e2v")
        .join("objects")
        .join(format!("{chunk_id}.json"));
    let mut bytes = fs::read(&chunk_path).unwrap();
    let last_index = bytes.len() - 1;
    bytes[last_index] ^= 0x01;
    fs::write(&chunk_path, bytes).unwrap();

    let error = facade
        .verify_object(&repo_root, &chunk_id, "chunk")
        .unwrap_err();
    assert!(
        error.to_string().contains("authentication"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn manifest_store_walks_nested_tree_entries() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::create_dir_all(repo_root.join("nested")).unwrap();
    fs::write(repo_root.join("nested").join("hello.txt"), "hello nested").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "manifest".to_string(),
        })
        .unwrap();

    let store = ManifestStore::new(&repo_root);
    let entries = store.walk_tree(&commit.snapshot_id).unwrap();

    assert!(
        entries
            .iter()
            .any(|entry| entry.path == "nested/hello.txt" && entry.kind == "file")
    );
}

#[test]
fn manifest_store_can_fetch_snapshot_tree_and_file_manifests() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::create_dir_all(repo_root.join("nested")).unwrap();
    fs::write(repo_root.join("nested").join("hello.txt"), "hello nested").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "manifest".to_string(),
        })
        .unwrap();

    let store = ManifestStore::new(&repo_root);
    let snapshot = store.get_snapshot(&commit.snapshot_id).unwrap();
    let tree = store.get_tree_node(&snapshot.root_tree_id).unwrap();
    let file_entry = tree
        .entries
        .iter()
        .find(|entry| entry.name == "nested" && entry.kind == "tree")
        .unwrap();
    let nested_tree = store.get_tree_node(&file_entry.object_id).unwrap();
    let nested_file_entry = nested_tree
        .entries
        .iter()
        .find(|entry| entry.name == "hello.txt" && entry.kind == "file")
        .unwrap();
    let file = store.get_file(&nested_file_entry.object_id).unwrap();

    assert_eq!(snapshot.message, "manifest");
    assert_eq!(file.entry_name, "hello.txt");
    assert_eq!(file.file_size, "hello nested".len() as u64);
    assert_eq!(file.chunker_id, "fastcdc");
    assert_eq!(file.chunker_config_id, "fastcdc-64k-1m-8m");
    assert_eq!(file.chunk_lengths, vec!["hello nested".len() as u64]);
}

#[test]
fn file_manifest_records_modified_time() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::create_dir_all(repo_root.join("nested")).unwrap();
    fs::write(repo_root.join("nested").join("hello.txt"), "hello nested").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "manifest".to_string(),
        })
        .unwrap();

    let store = ManifestStore::new(&repo_root);
    let snapshot = store.get_snapshot(&commit.snapshot_id).unwrap();
    let tree = store.get_tree_node(&snapshot.root_tree_id).unwrap();
    let dir_entry = tree
        .entries
        .iter()
        .find(|entry| entry.name == "nested" && entry.kind == "tree")
        .unwrap();
    let nested_tree = store.get_tree_node(&dir_entry.object_id).unwrap();
    let file_entry = nested_tree
        .entries
        .iter()
        .find(|entry| entry.name == "hello.txt" && entry.kind == "file")
        .unwrap();
    let file = store.get_file(&file_entry.object_id).unwrap();

    assert!(file.modified_unix_ms > 0);
}

#[test]
fn metadata_search_filters_by_extension_and_path_prefix() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::create_dir_all(repo_root.join("docs")).unwrap();
    fs::create_dir_all(repo_root.join("src")).unwrap();
    fs::write(repo_root.join("docs").join("guide.md"), "guide").unwrap();
    fs::write(repo_root.join("docs").join("notes.txt"), "notes").unwrap();
    fs::write(repo_root.join("src").join("guide.md"), "source guide").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap();

    let results = facade
        .search_metadata(
            &repo_root,
            e2v_core::MetadataSearchQuery {
                extension: Some("md".to_string()),
                path_prefix: Some("docs".to_string()),
                min_size: None,
                max_size: None,
            },
        )
        .unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].path, "docs/guide.md");
    assert_eq!(results[0].extension.as_deref(), Some("md"));
}

#[test]
fn metadata_search_filters_by_size_bounds() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("tiny.txt"), "a").unwrap();
    fs::write(repo_root.join("mid.txt"), "abcd").unwrap();
    fs::write(repo_root.join("large.txt"), "abcdefghij").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap();

    let results = facade
        .search_metadata(
            &repo_root,
            e2v_core::MetadataSearchQuery {
                extension: None,
                path_prefix: None,
                min_size: Some(2),
                max_size: Some(6),
            },
        )
        .unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].path, "mid.txt");
    assert_eq!(results[0].size_bytes, 4);
    assert!(results[0].modified_unix_ms > 0);
    assert!(!results[0].file_object_id.is_empty());
}

#[test]
fn metadata_search_does_not_require_unrelated_later_file_shard_metadata() {
    let _guard = e2v_core::testing::override_max_file_chunks_per_object_for_test(2);
    let _chunk_guard = e2v_core::testing::override_fixed_span_bytes_for_test(1024 * 1024);
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();

    let mut content = Vec::with_capacity(9_000_000);
    for index in 0..9_000_000usize {
        content.push((index % 251) as u8);
    }
    fs::write(repo_root.join("large.bin"), &content).unwrap();
    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "metadata-file-shard-index".to_string(),
        })
        .unwrap();

    let manifest_store = ManifestStore::new(&repo_root);
    let snapshot_manifest = manifest_store.get_snapshot(&commit.snapshot_id).unwrap();
    let tree = manifest_store
        .get_tree_node(&snapshot_manifest.root_tree_id)
        .unwrap();
    let file_entry = tree
        .entries
        .iter()
        .find(|entry| entry.name == "large.bin" && entry.kind == "file")
        .unwrap();
    let file_manifest = manifest_store.get_file(&file_entry.object_id).unwrap();
    assert!(
        file_manifest.shard_ids.len() >= 2,
        "expected multiple file_shard objects, got {}",
        file_manifest.shard_ids.len()
    );

    let later_shard_path = repo_root
        .join(".e2v")
        .join("objects")
        .join(format!("{}.json", file_manifest.shard_ids.last().unwrap()));
    let mut bytes = fs::read(&later_shard_path).unwrap();
    let last_index = bytes.len() - 1;
    bytes[last_index] ^= 0x01;
    fs::write(&later_shard_path, bytes).unwrap();

    let results = facade
        .search_metadata(
            &repo_root,
            e2v_core::MetadataSearchQuery {
                extension: Some("bin".to_string()),
                path_prefix: Some("large.bin".to_string()),
                min_size: None,
                max_size: None,
            },
        )
        .unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].path, "large.bin");
    assert_eq!(results[0].size_bytes, content.len() as u64);
}

#[test]
fn filename_search_finds_visible_files_and_refreshes_after_commit_and_branch_checkout() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("alpha-notes.txt"), "alpha").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "base".to_string(),
        })
        .unwrap();
    facade.create_branch(&repo_root, "feature").unwrap();

    let base_results = facade.search_filenames(&repo_root, "notes").unwrap();
    assert_eq!(base_results.len(), 1);
    assert_eq!(base_results[0].path, "alpha-notes.txt");

    fs::write(repo_root.join("notes-fresh.txt"), "fresh").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "main-update".to_string(),
        })
        .unwrap();
    let main_results = facade.search_filenames(&repo_root, "notes").unwrap();
    assert_eq!(main_results.len(), 2);

    facade.checkout_branch(&repo_root, "feature").unwrap();
    let feature_results = facade.search_filenames(&repo_root, "notes").unwrap();
    assert_eq!(feature_results.len(), 1);
    assert_eq!(feature_results[0].path, "alpha-notes.txt");
}

#[test]
fn metadata_search_refresh_preserves_unchanged_index_rows_across_head_updates() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("stable.txt"), "stable").unwrap();
    fs::write(repo_root.join("base.txt"), "base").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "base".to_string(),
        })
        .unwrap();

    facade
        .search_metadata(
            &repo_root,
            e2v_core::MetadataSearchQuery {
                extension: None,
                path_prefix: None,
                min_size: None,
                max_size: None,
            },
        )
        .unwrap();

    let index_db = repo_root.join(".e2v").join("index.sqlite3");
    let connection = Connection::open(&index_db).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE audit_log(kind TEXT NOT NULL, path TEXT NOT NULL);
             CREATE TRIGGER current_files_delete_audit
             AFTER DELETE ON current_files
             BEGIN
                 INSERT INTO audit_log(kind, path) VALUES('delete', old.path);
             END;",
        )
        .unwrap();
    drop(connection);

    fs::write(repo_root.join("added.txt"), "added").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "add-file".to_string(),
        })
        .unwrap();

    facade
        .search_metadata(
            &repo_root,
            e2v_core::MetadataSearchQuery {
                extension: None,
                path_prefix: None,
                min_size: None,
                max_size: None,
            },
        )
        .unwrap();

    let connection = Connection::open(&index_db).unwrap();
    let deleted_stable: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM audit_log WHERE kind = 'delete' AND path = 'stable.txt'",
            [],
            |row| row.get(0),
        )
        .unwrap();

    assert_eq!(
        deleted_stable, 0,
        "incremental index refresh should not delete unchanged rows"
    );
}

#[test]
fn metadata_search_rebuilds_corrupted_local_index_database() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("stable.txt"), "stable").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap();

    let first = facade
        .search_metadata(
            &repo_root,
            e2v_core::MetadataSearchQuery {
                extension: None,
                path_prefix: None,
                min_size: None,
                max_size: None,
            },
        )
        .unwrap();
    assert_eq!(first.len(), 1);

    let index_db = repo_root.join(".e2v").join("index.sqlite3");
    fs::write(&index_db, b"not-a-sqlite-database").unwrap();

    let rebuilt = facade
        .search_metadata(
            &repo_root,
            e2v_core::MetadataSearchQuery {
                extension: None,
                path_prefix: None,
                min_size: None,
                max_size: None,
            },
        )
        .unwrap();

    assert_eq!(rebuilt.len(), 1);
    assert_eq!(rebuilt[0].path, "stable.txt");
    assert_ne!(fs::read(&index_db).unwrap(), b"not-a-sqlite-database");
}

#[test]
fn filename_search_rebuilds_local_index_when_index_path_is_a_directory() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("alpha-notes.txt"), "alpha").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "seed".to_string(),
        })
        .unwrap();

    let first = facade.search_filenames(&repo_root, "notes").unwrap();
    assert_eq!(first.len(), 1);

    let index_db = repo_root.join(".e2v").join("index.sqlite3");
    fs::remove_file(&index_db).unwrap();
    fs::create_dir(&index_db).unwrap();

    let rebuilt = facade.search_filenames(&repo_root, "notes").unwrap();

    assert_eq!(rebuilt.len(), 1);
    assert_eq!(rebuilt[0].path, "alpha-notes.txt");
    assert!(index_db.is_file());
}

#[test]
fn metadata_and_filename_search_return_empty_results_for_uncommitted_repository() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();

    let metadata = facade
        .search_metadata(
            &repo_root,
            e2v_core::MetadataSearchQuery {
                extension: None,
                path_prefix: None,
                min_size: None,
                max_size: None,
            },
        )
        .unwrap();
    let filenames = facade.search_filenames(&repo_root, "anything").unwrap();

    assert!(metadata.is_empty());
    assert!(filenames.is_empty());
}

#[test]
fn branch_list_rejects_missing_current_branch_ref() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();

    let branch_ref_dir = repo_root.join(".e2v").join("refs").join("branches");
    if branch_ref_dir.exists() {
        fs::remove_dir_all(&branch_ref_dir).unwrap();
    }

    let error = facade.list_branches(&repo_root).unwrap_err();

    assert!(
        error.to_string().contains("current branch")
            || error.to_string().contains("missing")
            || error.to_string().contains("branch ref"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn collect_reachable_object_ids_preserves_graph_traversal_order() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    let store = ManifestStore::new(&repo_root);

    let mut found_mismatch = false;
    for iteration in 0..64usize {
        fs::write(
            repo_root.join("alpha.txt"),
            format!("alpha-payload-{iteration:02}"),
        )
        .unwrap();
        fs::write(
            repo_root.join("omega.txt"),
            format!("omega-payload-{:02}-tail", 63 - iteration),
        )
        .unwrap();

        let commit = facade
            .commit(CommitOptions {
                repo_root: repo_root.clone(),
                message: format!("reachable-order-{iteration:02}"),
            })
            .unwrap();

        let snapshot = store.get_snapshot(&commit.snapshot_id).unwrap();
        let root_tree = store.get_tree_node(&snapshot.root_tree_id).unwrap();

        let mut expected = vec![commit.snapshot_id.clone(), snapshot.root_tree_id.clone()];
        for entry in &root_tree.entries {
            assert_eq!(entry.kind, "file");
            expected.push(entry.object_id.clone());
            let file = store.get_file(&entry.object_id).unwrap();
            expected.extend(file.chunks);
        }

        let mut sorted = expected.clone();
        sorted.sort();
        sorted.dedup();
        if sorted == expected {
            continue;
        }

        let collected = store
            .collect_reachable_object_ids(&commit.snapshot_id)
            .unwrap();
        assert_eq!(collected, expected);
        found_mismatch = true;
        break;
    }

    assert!(
        found_mismatch,
        "failed to find a snapshot whose traversal order differs from lexical object-id order"
    );
}

#[test]
fn manifest_store_get_many_fetches_multiple_manifest_types() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("hello.txt"), "hello world").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "manifest-many".to_string(),
        })
        .unwrap();

    let store = ManifestStore::new(&repo_root);
    let snapshot = store.get_snapshot(&commit.snapshot_id).unwrap();
    let objects = store
        .get_many(&[
            (&commit.snapshot_id, "snapshot"),
            (&snapshot.root_tree_id, "tree"),
        ])
        .unwrap();

    assert_eq!(objects.len(), 2);
    assert!(matches!(objects[0], ManifestObject::Snapshot(_)));
    assert!(matches!(objects[1], ManifestObject::Tree(_)));
}

#[test]
fn manifest_store_trait_object_supports_snapshot_lookup() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("hello.txt"), "hello world").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "trait-store".to_string(),
        })
        .unwrap();

    let store = ManifestStore::new(&repo_root);
    let trait_store: &dyn ManifestStoreApi = &store;
    let snapshot = trait_store.get_snapshot(&commit.snapshot_id).unwrap();

    assert_eq!(snapshot.message, "trait-store");
}

#[test]
fn manifest_store_exposes_iterator_style_tree_walk() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::create_dir_all(repo_root.join("nested")).unwrap();
    fs::write(repo_root.join("nested").join("hello.txt"), "hello nested").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "manifest-iter".to_string(),
        })
        .unwrap();

    let store = ManifestStore::new(&repo_root);
    let entries = store
        .walk_tree_streaming_for_test(&commit.snapshot_id)
        .unwrap()
        .collect::<Result<Vec<_>>>()
        .unwrap();

    assert!(entries.iter().any(|entry| entry.path == "nested/hello.txt"));
}

#[test]
fn manifest_store_streaming_tree_walk_defers_deep_file_validation_until_iteration() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("a-first.txt"), "first").unwrap();
    fs::create_dir_all(repo_root.join("nested")).unwrap();
    fs::write(repo_root.join("nested").join("z-second.txt"), "second").unwrap();

    let commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "manifest-stream".to_string(),
        })
        .unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service.open_snapshot(&commit.snapshot_id).unwrap();
    let nested_file = read_service
        .open_file(&snapshot, "nested/z-second.txt")
        .unwrap();
    let file_object_path = repo_root
        .join(".e2v")
        .join("objects")
        .join(format!("{}.json", nested_file.file_object_id));
    let mut bytes = fs::read(&file_object_path).unwrap();
    let last_index = bytes.len() - 1;
    bytes[last_index] ^= 0x01;
    fs::write(&file_object_path, bytes).unwrap();

    let store = ManifestStore::new(&repo_root);
    let mut entries = store
        .walk_tree_streaming_for_test(&commit.snapshot_id)
        .unwrap();

    let first = entries.next().unwrap().unwrap();
    assert_eq!(first.path, "a-first.txt");

    let error = entries.next().unwrap().unwrap_err();
    assert!(
        error.to_string().contains("authentication"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn rotated_active_epoch_keeps_old_snapshot_readable() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();

    fs::write(repo_root.join("tracked.txt"), "alpha").unwrap();
    let first_commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "first".to_string(),
        })
        .unwrap();

    e2v_core::testing::rotate_active_epoch_for_test(&repo_root, "correct horse battery staple")
        .unwrap();

    fs::write(repo_root.join("tracked.txt"), "beta").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "second".to_string(),
        })
        .unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service
        .open_snapshot(&first_commit.snapshot_id)
        .unwrap();
    let file = read_service.open_file(&snapshot, "tracked.txt").unwrap();
    let bytes = read_service.read_range(&file, 0, 64).unwrap();

    assert_eq!(String::from_utf8(bytes).unwrap(), "alpha");
}

#[test]
fn rewrite_history_to_active_epoch_retires_old_epochs_after_rewriting_local_history() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();

    fs::write(repo_root.join("tracked.txt"), "alpha").unwrap();
    let first_commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "first".to_string(),
        })
        .unwrap();

    e2v_core::testing::rotate_active_epoch_for_test(&repo_root, "correct horse battery staple")
        .unwrap();

    let before = read_current_keyring_json(&repo_root);
    assert_eq!(before["active_epoch"].as_u64(), Some(2));
    assert_eq!(before["epochs"].as_array().unwrap().len(), 2);

    let result = facade
        .rewrite_history_to_active_epoch(&repo_root, "correct horse battery staple")
        .unwrap();

    let after = read_current_keyring_json(&repo_root);
    assert_eq!(result.active_epoch, 2);
    assert_eq!(result.retired_epoch_count, 1);
    assert!(
        !result.rewritten_object_ids.is_empty(),
        "expected history rewrite to rewrite at least one local object"
    );
    assert_eq!(after["active_epoch"].as_u64(), Some(2));
    assert_eq!(after["epochs"].as_array().unwrap().len(), 1);
    assert_eq!(after["epochs"][0]["epoch"].as_u64(), Some(2));

    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&repo_root.join(".e2v"));
    facade.open(&repo_root).unwrap();

    let read_service = facade.read_service(&repo_root).unwrap();
    let snapshot = read_service
        .open_snapshot(&first_commit.snapshot_id)
        .unwrap();
    let file = read_service.open_file(&snapshot, "tracked.txt").unwrap();
    let bytes = read_service.read_range(&file, 0, 64).unwrap();

    assert_eq!(String::from_utf8(bytes).unwrap(), "alpha");
}

#[test]
fn rewrite_history_to_active_epoch_bumps_layout_generation() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("tracked.txt"), "alpha").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "first".to_string(),
        })
        .unwrap();
    e2v_core::testing::rotate_active_epoch_for_test(&repo_root, "correct horse battery staple")
        .unwrap();

    let before: serde_json::Value =
        serde_json::from_slice(&fs::read(repo_root.join(".e2v").join("layout_root.json")).unwrap())
            .unwrap();
    let result = facade
        .rewrite_history_to_active_epoch(&repo_root, "correct horse battery staple")
        .unwrap();
    let after: serde_json::Value =
        serde_json::from_slice(&fs::read(repo_root.join(".e2v").join("layout_root.json")).unwrap())
            .unwrap();
    let reopened = facade.open(&repo_root).unwrap();

    assert_eq!(before["generation"].as_u64(), Some(1));
    assert_eq!(after["generation"].as_u64(), Some(2));
    assert_eq!(reopened.layout_generation, 2);
    assert!(
        result
            .rewritten_control_records
            .iter()
            .any(|path| path == "layout_root.json"),
        "expected layout_root.json to be reported as a rewritten control record"
    );
}

#[test]
fn rewrite_history_to_active_epoch_rewrites_parent_snapshot_chain_before_retiring_old_epochs() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("tracked.txt"), "v1").unwrap();
    let first_commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "first".to_string(),
        })
        .unwrap();
    fs::write(repo_root.join("tracked.txt"), "v2").unwrap();
    let second_commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "second".to_string(),
        })
        .unwrap();

    e2v_core::testing::rotate_active_epoch_for_test(&repo_root, "correct horse battery staple")
        .unwrap();
    facade
        .rewrite_history_to_active_epoch(&repo_root, "correct horse battery staple")
        .unwrap();

    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&repo_root.join(".e2v"));
    let snapshots = facade.snapshots(&repo_root).unwrap();

    assert_eq!(snapshots.len(), 2);
    assert_eq!(snapshots[0].snapshot_id, second_commit.snapshot_id);
    assert_eq!(snapshots[1].snapshot_id, first_commit.snapshot_id);
}

#[test]
fn rewrite_history_to_active_epoch_prevents_old_epoch_keys_from_decrypting_current_ref() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("tracked.txt"), "v1").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "first".to_string(),
        })
        .unwrap();

    let control_dir = repo_root.join(".e2v");
    let secrets_before_rotation = e2v_core::sync_support::unlock_repo_secrets_for_sync(
        &control_dir,
        "correct horse battery staple",
    )
    .unwrap();
    let old_epoch_keys = secrets_before_rotation.epoch_keys(1).unwrap().clone();

    e2v_core::testing::rotate_active_epoch_for_test(&repo_root, "correct horse battery staple")
        .unwrap();
    facade
        .rewrite_history_to_active_epoch(&repo_root, "correct horse battery staple")
        .unwrap();

    let current_ref_bytes = fs::read(control_dir.join("refs").join("default.json")).unwrap();
    let wrong_epoch_secrets = e2v_store::RepoSecrets {
        repo_id: secrets_before_rotation.repo_id.clone(),
        active_epoch: 1,
        repo_dedup_key: secrets_before_rotation.repo_dedup_key,
        repo_ref_key: secrets_before_rotation.repo_ref_key,
        repo_manifest_enc_key: old_epoch_keys.manifest_enc_key,
        repo_nonce_key: old_epoch_keys.nonce_key,
        repo_path_index_key: secrets_before_rotation.repo_path_index_key,
        epoch_keys: std::collections::BTreeMap::from([(1, old_epoch_keys)]),
    };

    let error = e2v_core::sync_support::decrypt_control_record_for_sync(
        &wrong_epoch_secrets,
        "default",
        "ref",
        &current_ref_bytes,
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("missing epoch keys")
            || error.to_string().contains("authentication failed")
            || error.to_string().contains("ref authentication failed"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn sync_support_decode_object_bytes_accepts_objects_from_previous_epoch() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();

    fs::write(repo_root.join("tracked.txt"), "alpha").unwrap();
    let first_commit = facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "first".to_string(),
        })
        .unwrap();

    e2v_core::testing::rotate_active_epoch_for_test(&repo_root, "correct horse battery staple")
        .unwrap();

    let snapshot_bytes =
        e2v_core::sync_support::read_local_object_bytes(&repo_root, &first_commit.snapshot_id)
            .unwrap();
    let decoded = e2v_core::sync_support::decode_object_bytes_for_sync(
        &repo_root,
        &first_commit.snapshot_id,
        "snapshot",
        &snapshot_bytes,
    )
    .unwrap();
    let snapshot: ManifestSnapshotObject = postcard::from_bytes(&decoded).unwrap();

    assert_eq!(snapshot.message, "first");
}

#[test]
fn control_record_decryption_uses_envelope_key_epoch_after_rotation() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    fs::write(repo_root.join("tracked.txt"), "alpha").unwrap();
    facade
        .commit(CommitOptions {
            repo_root: repo_root.clone(),
            message: "first".to_string(),
        })
        .unwrap();

    let control_dir = repo_root.join(".e2v");
    let default_ref_before_rotation =
        fs::read(control_dir.join("refs").join("default.json")).unwrap();

    e2v_core::testing::rotate_active_epoch_for_test(&repo_root, "correct horse battery staple")
        .unwrap();

    let secrets_after_rotation = e2v_core::sync_support::unlock_repo_secrets_for_sync(
        &control_dir,
        "correct horse battery staple",
    )
    .unwrap();
    let plaintext = e2v_core::sync_support::decrypt_control_record_for_sync(
        &secrets_after_rotation,
        "default",
        "ref",
        &default_ref_before_rotation,
    )
    .unwrap();
    let record: RefRecordMirror = postcard::from_bytes(&plaintext).unwrap();

    assert_eq!(record.branch_name, "main");
}

#[test]
fn share_list_exposes_owner_actor_and_local_device() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();

    let listing = facade.share_list(&repo_root).unwrap();

    assert_eq!(listing.actors.len(), 1);
    assert_eq!(listing.actors[0].role, "owner_admin");
    assert_eq!(listing.devices.len(), 1);
    assert_eq!(listing.devices[0].status, "active");
}

#[test]
fn share_invite_member_creates_repository_scoped_bundle() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();

    let invite = facade
        .share_invite_member(
            &repo_root,
            e2v_core::ShareInviteMemberOptions {
                display_name: "Alice".to_string(),
            },
        )
        .unwrap();

    assert!(!invite.actor_id.is_empty());
    assert!(!invite.device_id.is_empty());
    assert!(!invite.bundle_bytes.is_empty());
}

fn read_current_keyring_json(repo_root: &std::path::Path) -> serde_json::Value {
    let keyring_dir = repo_root.join(".e2v").join("keyring");
    let pointer: serde_json::Value =
        serde_json::from_slice(&fs::read(keyring_dir.join("keyring.current")).unwrap()).unwrap();
    let current = pointer["current"].as_str().unwrap();
    serde_json::from_slice(&fs::read(keyring_dir.join(current)).unwrap()).unwrap()
}

fn keyring_contains_actor_envelope(keyring: &serde_json::Value, actor_id: &str) -> bool {
    keyring["envelopes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|envelope| envelope["actor_id"].as_str() == Some(actor_id))
}

#[test]
fn share_accept_member_creates_writer_member_device() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();

    let invite = facade
        .share_invite_member(
            &repo_root,
            e2v_core::ShareInviteMemberOptions {
                display_name: "Alice".to_string(),
            },
        )
        .unwrap();

    let accepted = facade
        .share_accept_member(
            &repo_root,
            e2v_core::ShareAcceptMemberOptions {
                invite_bytes: invite.bundle_bytes.clone(),
                local_device_label: "alice-laptop".to_string(),
            },
        )
        .unwrap();

    assert_eq!(accepted.actor_id, invite.actor_id);
    assert_eq!(accepted.role, "writer_member");
    assert!(!accepted.device_id.is_empty());

    let keyring = read_current_keyring_json(&repo_root);
    assert_eq!(keyring["generation"].as_u64(), Some(2));
    assert!(
        keyring["actors"]
            .as_array()
            .unwrap()
            .iter()
            .any(
                |actor| actor["actor_id"].as_str() == Some(invite.actor_id.as_str())
                    && actor["role"].as_str() == Some("writer_member")
            )
    );
    assert!(keyring["devices"].as_array().unwrap().iter().any(
        |device| device["actor_id"].as_str() == Some(invite.actor_id.as_str())
            && device["label"].as_str() == Some("alice-laptop")
            && device["status"].as_str() == Some("active")
    ));
    assert!(keyring_contains_actor_envelope(&keyring, &invite.actor_id));
}

#[test]
fn share_accept_member_bootstraps_empty_recipient_repo() {
    let temp = tempdir().unwrap();
    let owner_root = temp.path().join("owner");
    let recipient_root = temp.path().join("recipient");
    fs::create_dir_all(&owner_root).unwrap();
    fs::create_dir_all(&recipient_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&owner_root)).unwrap();

    let invite = facade
        .share_invite_member(
            &owner_root,
            e2v_core::ShareInviteMemberOptions {
                display_name: "Alice".to_string(),
            },
        )
        .unwrap();

    let accepted = facade
        .share_accept_member(
            &recipient_root,
            e2v_core::ShareAcceptMemberOptions {
                invite_bytes: invite.bundle_bytes.clone(),
                local_device_label: "alice-laptop".to_string(),
            },
        )
        .unwrap();

    assert_eq!(accepted.actor_id, invite.actor_id);
    assert_eq!(accepted.role, "writer_member");
    assert!(!accepted.device_id.is_empty());

    let recipient_control = recipient_root.join(".e2v");
    assert!(
        recipient_control
            .join("keyring")
            .join("keyring.current")
            .is_file()
    );
    assert!(
        recipient_control
            .join("device")
            .join("local-device.json")
            .is_file()
    );

    let pointer: serde_json::Value = serde_json::from_slice(
        &fs::read(recipient_control.join("keyring").join("keyring.current")).unwrap(),
    )
    .unwrap();
    let current = pointer["current"].as_str().unwrap();
    let keyring: serde_json::Value =
        serde_json::from_slice(&fs::read(recipient_control.join("keyring").join(current)).unwrap())
            .unwrap();

    assert_eq!(
        keyring["repo_id"].as_str(),
        read_current_keyring_json(&owner_root)["repo_id"].as_str()
    );
    assert!(
        keyring["actors"]
            .as_array()
            .unwrap()
            .iter()
            .any(
                |actor| actor["actor_id"].as_str() == Some(invite.actor_id.as_str())
                    && actor["role"].as_str() == Some("writer_member")
            )
    );
    assert!(keyring["devices"].as_array().unwrap().iter().any(
        |device| device["device_id"].as_str() == Some(accepted.device_id.as_str())
            && device["label"].as_str() == Some("alice-laptop")
            && device["status"].as_str() == Some("active")
    ));
}

#[test]
fn share_accept_member_rejects_bootstrap_pointer_path_traversal_before_writing_outside_repo() {
    let temp = tempdir().unwrap();
    let owner_root = temp.path().join("owner");
    let recipient_root = temp.path().join("recipient");
    fs::create_dir_all(&owner_root).unwrap();
    fs::create_dir_all(&recipient_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&owner_root)).unwrap();

    let invite = facade
        .share_invite_member(
            &owner_root,
            e2v_core::ShareInviteMemberOptions {
                display_name: "Alice".to_string(),
            },
        )
        .unwrap();
    let mut tampered_invite: serde_json::Value =
        serde_json::from_slice(&invite.bundle_bytes).unwrap();
    tampered_invite["bootstrap_keyring_pointer"]["current"] =
        serde_json::Value::String("../../../outside.json".to_string());

    let error = facade
        .share_accept_member(
            &recipient_root,
            e2v_core::ShareAcceptMemberOptions {
                invite_bytes: serde_json::to_vec(&tampered_invite).unwrap(),
                local_device_label: "alice-laptop".to_string(),
            },
        )
        .unwrap_err();

    assert!(
        error.to_string().contains("invalid current keyring path")
            || error.to_string().contains("invalid keyring")
            || error.to_string().contains("path traversal")
            || error.to_string().contains("single path segment"),
        "unexpected error: {error:#}"
    );
    assert!(
        !temp.path().join("outside.json").exists(),
        "tampered invite should not write keyring data outside the recipient repository"
    );
}

#[test]
fn share_revoke_member_advances_active_epoch_and_removes_member_envelope() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    let owner_credential_bytes = fs::read(
        repo_root
            .join(".e2v")
            .join("device")
            .join("local-device.json"),
    )
    .unwrap();
    let invite = facade
        .share_invite_member(
            &repo_root,
            e2v_core::ShareInviteMemberOptions {
                display_name: "Alice".to_string(),
            },
        )
        .unwrap();
    facade
        .share_accept_member(
            &repo_root,
            e2v_core::ShareAcceptMemberOptions {
                invite_bytes: invite.bundle_bytes.clone(),
                local_device_label: "alice-laptop".to_string(),
            },
        )
        .unwrap();

    let before = read_current_keyring_json(&repo_root);
    let before_epoch = before["active_epoch"].as_u64().unwrap();
    fs::write(
        repo_root
            .join(".e2v")
            .join("device")
            .join("local-device.json"),
        owner_credential_bytes,
    )
    .unwrap();

    facade
        .share_revoke_member(
            &repo_root,
            e2v_core::ShareRevokeMemberOptions {
                actor_id: invite.actor_id.clone(),
                password: "correct horse battery staple".to_string(),
            },
        )
        .unwrap();

    let after = read_current_keyring_json(&repo_root);
    assert_eq!(after["generation"].as_u64(), Some(3));
    assert_eq!(after["active_epoch"].as_u64(), Some(before_epoch + 1));
    assert!(
        after["actors"]
            .as_array()
            .unwrap()
            .iter()
            .any(
                |actor| actor["actor_id"].as_str() == Some(invite.actor_id.as_str())
                    && actor["role"].as_str() == Some("writer_member")
            )
    );
    assert!(
        after["devices"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|device| device["actor_id"].as_str() == Some(invite.actor_id.as_str()))
            .all(|device| device["status"].as_str() == Some("revoked"))
    );
    assert!(!keyring_contains_actor_envelope(&after, &invite.actor_id));
}

#[test]
fn share_revoke_member_accepts_explicit_password_after_cache_clear_in_latest_flow() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    let owner_credential_bytes = fs::read(
        repo_root
            .join(".e2v")
            .join("device")
            .join("local-device.json"),
    )
    .unwrap();
    let invite = facade
        .share_invite_member(
            &repo_root,
            e2v_core::ShareInviteMemberOptions {
                display_name: "Alice".to_string(),
            },
        )
        .unwrap();
    facade
        .share_accept_member(
            &repo_root,
            e2v_core::ShareAcceptMemberOptions {
                invite_bytes: invite.bundle_bytes.clone(),
                local_device_label: "alice-laptop".to_string(),
            },
        )
        .unwrap();

    fs::write(
        repo_root
            .join(".e2v")
            .join("device")
            .join("local-device.json"),
        owner_credential_bytes,
    )
    .unwrap();
    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&repo_root.join(".e2v"));

    facade
        .share_revoke_member(
            &repo_root,
            e2v_core::ShareRevokeMemberOptions {
                actor_id: invite.actor_id.clone(),
                password: "correct horse battery staple".to_string(),
            },
        )
        .unwrap();

    let after = read_current_keyring_json(&repo_root);
    assert!(
        after["devices"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|device| device["actor_id"].as_str() == Some(invite.actor_id.as_str()))
            .all(|device| device["status"].as_str() == Some("revoked"))
    );
}

#[test]
fn share_revoke_member_blocks_local_device_unlock_after_revocation() {
    let temp = tempdir().unwrap();
    let owner_root = temp.path().join("owner-repo");
    fs::create_dir_all(&owner_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&owner_root)).unwrap();
    let owner_credential_bytes = fs::read(
        owner_root
            .join(".e2v")
            .join("device")
            .join("local-device.json"),
    )
    .unwrap();
    let invite = facade
        .share_invite_member(
            &owner_root,
            e2v_core::ShareInviteMemberOptions {
                display_name: "Alice".to_string(),
            },
        )
        .unwrap();
    let accepted = facade
        .share_accept_member(
            &owner_root,
            e2v_core::ShareAcceptMemberOptions {
                invite_bytes: invite.bundle_bytes.clone(),
                local_device_label: "alice-laptop".to_string(),
            },
        )
        .unwrap();

    let member_credential: serde_json::Value = serde_json::from_slice(
        &fs::read(
            owner_root
                .join(".e2v")
                .join("device")
                .join("local-device.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        member_credential["actor_id"].as_str(),
        Some(accepted.actor_id.as_str())
    );

    fs::write(
        owner_root
            .join(".e2v")
            .join("device")
            .join("local-device.json"),
        owner_credential_bytes,
    )
    .unwrap();

    facade
        .share_revoke_member(
            &owner_root,
            e2v_core::ShareRevokeMemberOptions {
                actor_id: invite.actor_id.clone(),
                password: "correct horse battery staple".to_string(),
            },
        )
        .unwrap();

    fs::write(
        owner_root
            .join(".e2v")
            .join("device")
            .join("local-device.json"),
        serde_json::to_vec_pretty(&member_credential).unwrap(),
    )
    .unwrap();
    e2v_core::testing::clear_unlocked_keyring_cache_for_test(&owner_root.join(".e2v"));
    let error = e2v_core::testing::unlock_with_local_device_for_test(&owner_root).unwrap_err();
    assert!(
        error.to_string().contains("matching local device envelope")
            || error.to_string().contains("device unlock failed")
            || error.to_string().contains("locked"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn init_writes_local_device_credential_as_compact_json() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    RepositoryFacade::new()
        .init(init_options(&repo_root))
        .unwrap();

    let bytes = fs::read(
        repo_root
            .join(".e2v")
            .join("device")
            .join("local-device.json"),
    )
    .unwrap();
    let text = String::from_utf8(bytes).unwrap();

    assert!(
        !text.contains('\n'),
        "expected compact local device credential json without pretty-printed newlines"
    );
}

#[test]
fn share_invite_device_and_accept_device_adds_second_active_device_for_actor() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    let owner_credential_bytes = fs::read(
        repo_root
            .join(".e2v")
            .join("device")
            .join("local-device.json"),
    )
    .unwrap();
    let member_invite = facade
        .share_invite_member(
            &repo_root,
            e2v_core::ShareInviteMemberOptions {
                display_name: "Alice".to_string(),
            },
        )
        .unwrap();
    let accepted_member = facade
        .share_accept_member(
            &repo_root,
            e2v_core::ShareAcceptMemberOptions {
                invite_bytes: member_invite.bundle_bytes.clone(),
                local_device_label: "alice-laptop".to_string(),
            },
        )
        .unwrap();

    fs::write(
        repo_root
            .join(".e2v")
            .join("device")
            .join("local-device.json"),
        owner_credential_bytes,
    )
    .unwrap();
    let device_invite = facade
        .share_invite_device(
            &repo_root,
            e2v_core::ShareInviteDeviceOptions {
                actor_id: accepted_member.actor_id.clone(),
                device_label: "alice-phone".to_string(),
            },
        )
        .unwrap();
    let accepted_device = facade
        .share_accept_device(
            &repo_root,
            e2v_core::ShareAcceptDeviceOptions {
                invite_bytes: device_invite.bundle_bytes.clone(),
                local_device_label: "alice-phone".to_string(),
            },
        )
        .unwrap();

    assert_eq!(accepted_device.actor_id, accepted_member.actor_id);
    assert_eq!(accepted_device.role, "writer_member");
    assert_ne!(accepted_device.device_id, accepted_member.device_id);

    let keyring = read_current_keyring_json(&repo_root);
    let actor_devices = keyring["devices"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|device| device["actor_id"].as_str() == Some(accepted_member.actor_id.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(actor_devices.len(), 2);
    assert!(
        actor_devices
            .iter()
            .all(|device| device["status"].as_str() == Some("active"))
    );
    assert!(
        keyring["envelopes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|envelope| envelope["device_id"].as_str()
                == Some(accepted_device.device_id.as_str()))
    );
}

#[test]
fn share_revoke_device_advances_epoch_and_only_revokes_target_device() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    let owner_credential_bytes = fs::read(
        repo_root
            .join(".e2v")
            .join("device")
            .join("local-device.json"),
    )
    .unwrap();
    let member_invite = facade
        .share_invite_member(
            &repo_root,
            e2v_core::ShareInviteMemberOptions {
                display_name: "Alice".to_string(),
            },
        )
        .unwrap();
    let accepted_member = facade
        .share_accept_member(
            &repo_root,
            e2v_core::ShareAcceptMemberOptions {
                invite_bytes: member_invite.bundle_bytes.clone(),
                local_device_label: "alice-laptop".to_string(),
            },
        )
        .unwrap();
    fs::write(
        repo_root
            .join(".e2v")
            .join("device")
            .join("local-device.json"),
        owner_credential_bytes.clone(),
    )
    .unwrap();
    let device_invite = facade
        .share_invite_device(
            &repo_root,
            e2v_core::ShareInviteDeviceOptions {
                actor_id: accepted_member.actor_id.clone(),
                device_label: "alice-phone".to_string(),
            },
        )
        .unwrap();
    let accepted_device = facade
        .share_accept_device(
            &repo_root,
            e2v_core::ShareAcceptDeviceOptions {
                invite_bytes: device_invite.bundle_bytes.clone(),
                local_device_label: "alice-phone".to_string(),
            },
        )
        .unwrap();

    let before = read_current_keyring_json(&repo_root);
    let before_epoch = before["active_epoch"].as_u64().unwrap();
    fs::write(
        repo_root
            .join(".e2v")
            .join("device")
            .join("local-device.json"),
        owner_credential_bytes,
    )
    .unwrap();

    facade
        .share_revoke_device(
            &repo_root,
            e2v_core::ShareRevokeDeviceOptions {
                device_id: accepted_device.device_id.clone(),
                password: "correct horse battery staple".to_string(),
            },
        )
        .unwrap();

    let after = read_current_keyring_json(&repo_root);
    assert_eq!(after["active_epoch"].as_u64(), Some(before_epoch + 1));
    assert!(
        after["devices"]
            .as_array()
            .unwrap()
            .iter()
            .any(
                |device| device["device_id"].as_str() == Some(accepted_device.device_id.as_str())
                    && device["status"].as_str() == Some("revoked")
            )
    );
    assert!(
        after["devices"]
            .as_array()
            .unwrap()
            .iter()
            .any(
                |device| device["device_id"].as_str() == Some(accepted_member.device_id.as_str())
                    && device["status"].as_str() == Some("active")
            )
    );
    assert!(
        !after["envelopes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|envelope| envelope["device_id"].as_str()
                == Some(accepted_device.device_id.as_str()))
    );
}

#[test]
fn writer_member_cannot_issue_share_admin_operations() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root).unwrap();

    let facade = RepositoryFacade::new();
    facade.init(init_options(&repo_root)).unwrap();
    let member_invite = facade
        .share_invite_member(
            &repo_root,
            e2v_core::ShareInviteMemberOptions {
                display_name: "Alice".to_string(),
            },
        )
        .unwrap();
    let accepted_member = facade
        .share_accept_member(
            &repo_root,
            e2v_core::ShareAcceptMemberOptions {
                invite_bytes: member_invite.bundle_bytes.clone(),
                local_device_label: "alice-laptop".to_string(),
            },
        )
        .unwrap();

    let invite_error = facade
        .share_invite_member(
            &repo_root,
            e2v_core::ShareInviteMemberOptions {
                display_name: "Bob".to_string(),
            },
        )
        .unwrap_err();
    assert!(
        invite_error.to_string().contains("owner-admin")
            || invite_error.to_string().contains("not authorized")
            || invite_error.to_string().contains("share admin"),
        "unexpected invite error: {invite_error:#}"
    );

    let revoke_error = facade
        .share_revoke_member(
            &repo_root,
            e2v_core::ShareRevokeMemberOptions {
                actor_id: accepted_member.actor_id.clone(),
                password: "correct horse battery staple".to_string(),
            },
        )
        .unwrap_err();
    assert!(
        revoke_error.to_string().contains("owner-admin")
            || revoke_error.to_string().contains("not authorized")
            || revoke_error.to_string().contains("share admin"),
        "unexpected revoke error: {revoke_error:#}"
    );
}
