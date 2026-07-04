use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

static TRUSTED_STATE_DIR_OVERRIDE: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
static TRUSTED_STATE_OVERRIDE_GUARD: OnceLock<Mutex<()>> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TrustedRemoteState {
    pub repo_id: String,
    pub min_layout_generation: u64,
    pub min_keyring_generation: u64,
    #[serde(default)]
    pub min_ref_generations: BTreeMap<String, u64>,
}

#[doc(hidden)]
pub(crate) fn override_trusted_state_dir_for_test(path: PathBuf) -> TrustedStateDirGuard {
    let usage_lock = TRUSTED_STATE_OVERRIDE_GUARD
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let lock = TRUSTED_STATE_DIR_OVERRIDE.get_or_init(|| Mutex::new(None));
    let mut slot = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let previous = slot.replace(path);
    TrustedStateDirGuard {
        previous,
        _usage_lock: Some(usage_lock),
    }
}

#[doc(hidden)]
pub struct TrustedStateDirGuard {
    previous: Option<PathBuf>,
    _usage_lock: Option<std::sync::MutexGuard<'static, ()>>,
}

impl Drop for TrustedStateDirGuard {
    fn drop(&mut self) {
        let lock = TRUSTED_STATE_DIR_OVERRIDE.get_or_init(|| Mutex::new(None));
        *lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = self.previous.take();
    }
}

pub(crate) fn load_trusted_remote_state(repo_id: &str) -> Result<Option<TrustedRemoteState>> {
    let path = trusted_state_file_path(repo_id)?;
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_dir() => {
            anyhow::bail!("failed to read trusted state {}", path.display());
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let bytes = std::fs::read(&path)
        .with_context(|| format!("failed to read trusted state {}", path.display()))?;
    let state: TrustedRemoteState = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to decode trusted state {}", path.display()))?;
    Ok(Some(state))
}

pub(crate) fn store_trusted_remote_state(state: &TrustedRemoteState) -> Result<()> {
    let parent = trusted_state_root()?;
    std::fs::create_dir_all(&parent)
        .with_context(|| format!("failed to create trusted state dir {}", parent.display()))?;
    let path = trusted_state_file_path(&state.repo_id)?;
    let bytes = serde_json::to_vec(state)?;
    atomic_write_bytes(&path, &bytes)
        .with_context(|| format!("failed to write trusted state {}", path.display()))?;
    Ok(())
}

fn atomic_write_bytes(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    let temp_path = path.with_extension(format!(
        "{}.tmp",
        path.extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("tmp")
    ));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    remove_path_if_exists(&temp_path)?;
    std::fs::write(&temp_path, bytes)
        .with_context(|| format!("failed to write {}", temp_path.display()))?;
    match std::fs::rename(&temp_path, path) {
        Ok(()) => Ok(()),
        Err(error) if cfg!(windows) => {
            remove_path_if_exists(path)?;
            std::fs::rename(&temp_path, path)
                .with_context(|| format!("failed to publish {}", path.display()))?;
            Ok(())
        }
        Err(error) => Err(error).with_context(|| format!("failed to publish {}", path.display())),
    }
}

fn remove_path_if_exists(path: &std::path::Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path)?,
        Ok(_) => std::fs::remove_file(path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn trusted_state_file_path(repo_id: &str) -> Result<PathBuf> {
    let path = Path::new(repo_id);
    anyhow::ensure!(
        !repo_id.trim().is_empty(),
        "trusted state repo_id must not be empty"
    );
    anyhow::ensure!(!path.is_absolute(), "trusted state repo_id must be relative");
    anyhow::ensure!(
        path.components()
            .all(|component| matches!(component, Component::Normal(_))),
        "trusted state repo_id path traversal is not allowed"
    );
    Ok(trusted_state_root()?.join(format!("{repo_id}.json")))
}

fn trusted_state_root() -> Result<PathBuf> {
    if let Some(path) = TRUSTED_STATE_DIR_OVERRIDE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
    {
        return Ok(path);
    }

    if let Ok(path) = std::env::var("E2V_TRUSTED_STATE_DIR") {
        return Ok(PathBuf::from(path));
    }

    let base = dirs_base_dir()
        .context("failed to resolve default trusted state directory for this platform")?;
    Ok(base.join("e2v").join("trusted-state"))
}

#[cfg(windows)]
fn dirs_base_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("APPDATA").map(PathBuf::from))
}

#[cfg(not(windows))]
fn dirs_base_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| Path::new(&home).join(".local").join("state"))
        })
}

#[cfg(test)]
mod tests {
    use std::mem::drop;
    use std::panic::{self, AssertUnwindSafe};
    use tempfile::tempdir;

    use super::*;

    fn poison_override_lock() {
        let lock = TRUSTED_STATE_DIR_OVERRIDE.get_or_init(|| Mutex::new(None));
        let _ = panic::catch_unwind(AssertUnwindSafe(|| {
            let _guard = lock.lock().unwrap();
            panic!("poison trusted state override lock");
        }));
    }

    #[test]
    fn trusted_state_is_stored_as_compact_json() {
        let temp = tempdir().unwrap();
        let _guard = override_trusted_state_dir_for_test(temp.path().to_path_buf());
        let state = TrustedRemoteState {
            repo_id: "repo-123".to_string(),
            min_layout_generation: 7,
            min_keyring_generation: 11,
            min_ref_generations: BTreeMap::from([("branch-123".to_string(), 13)]),
        };

        store_trusted_remote_state(&state).unwrap();

        let bytes = std::fs::read(temp.path().join("repo-123.json")).unwrap();
        assert_eq!(
            bytes,
            serde_json::to_vec(&state).unwrap(),
            "trusted state files should not store pretty-printed JSON whitespace"
        );
    }

    #[test]
    fn trusted_state_store_leaves_no_temp_file_after_publish() {
        let temp = tempdir().unwrap();
        let _guard = override_trusted_state_dir_for_test(temp.path().to_path_buf());
        let state = TrustedRemoteState {
            repo_id: "repo-123".to_string(),
            min_layout_generation: 7,
            min_keyring_generation: 11,
            min_ref_generations: BTreeMap::from([("branch-123".to_string(), 13)]),
        };

        store_trusted_remote_state(&state).unwrap();

        let leftover_temps = std::fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .filter(|name| name.ends_with(".tmp"))
            .collect::<Vec<_>>();
        assert!(
            leftover_temps.is_empty(),
            "expected no leftover trusted-state temp files, found {leftover_temps:?}"
        );
    }

    #[test]
    fn trusted_state_store_recovers_from_temp_path_conflict() {
        let temp = tempdir().unwrap();
        let _guard = override_trusted_state_dir_for_test(temp.path().to_path_buf());
        let state = TrustedRemoteState {
            repo_id: "repo-123".to_string(),
            min_layout_generation: 7,
            min_keyring_generation: 11,
            min_ref_generations: BTreeMap::from([("branch-123".to_string(), 13)]),
        };
        let temp_path = temp.path().join("repo-123.json.tmp");
        std::fs::create_dir(&temp_path).unwrap();

        store_trusted_remote_state(&state).unwrap();

        assert!(temp.path().join("repo-123.json").is_file());
        assert!(
            !temp_path.exists(),
            "trusted state publish should replace temp-path conflicts with a real temp file during atomic write"
        );
    }

    #[test]
    fn trusted_state_store_uses_atomic_publish_path_instead_of_direct_final_write() {
        let source = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("src")
                .join("trusted_state.rs"),
        )
        .unwrap();
        let store_fn = source
            .split("pub(crate) fn store_trusted_remote_state")
            .nth(1)
            .and_then(|tail| tail.split("fn trusted_state_file_path").next())
            .expect("expected to isolate store_trusted_remote_state source");

        assert!(
            store_fn.contains("atomic_write_bytes(&path, &bytes)"),
            "trusted state persistence should publish through the shared atomic write helper"
        );
        assert!(
            !store_fn.contains("std::fs::write(&path, bytes)"),
            "trusted state persistence should not directly overwrite the final fact file"
        );
    }

    #[test]
    fn trusted_state_rejects_repo_id_path_traversal_before_touching_disk() {
        let temp = tempdir().unwrap();
        let _guard = override_trusted_state_dir_for_test(temp.path().to_path_buf());
        let state = TrustedRemoteState {
            repo_id: "../escape".to_string(),
            min_layout_generation: 7,
            min_keyring_generation: 11,
            min_ref_generations: BTreeMap::new(),
        };

        let error = store_trusted_remote_state(&state).unwrap_err();

        assert!(
            error.to_string().contains("path traversal")
                || error.to_string().contains("path separators")
                || error.to_string().contains("relative"),
            "unexpected error: {error:#}"
        );
        assert!(
            temp.path().read_dir().unwrap().next().is_none(),
            "trusted state store should reject traversal repo ids before creating any local files"
        );
    }

    #[test]
    fn trusted_state_load_rejects_path_conflicts_instead_of_treating_them_as_missing() {
        let temp = tempdir().unwrap();
        let _guard = override_trusted_state_dir_for_test(temp.path().to_path_buf());
        let path = temp.path().join("repo-123.json");
        std::fs::create_dir(&path).unwrap();

        let error = load_trusted_remote_state("repo-123").unwrap_err();

        assert!(
            error.to_string().contains("failed to read trusted state"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn trusted_state_root_recovers_from_poisoned_override_lock() {
        let temp = tempdir().unwrap();
        let expected = temp.path().to_path_buf();
        let _guard = override_trusted_state_dir_for_test(expected.clone());

        poison_override_lock();

        let result = panic::catch_unwind(AssertUnwindSafe(trusted_state_root));
        assert!(result.is_ok(), "trusted_state_root should not panic");
        assert_eq!(result.unwrap().unwrap(), expected);
    }

    #[test]
    fn trusted_state_guard_drop_recovers_from_poisoned_override_lock() {
        let temp_one = tempdir().unwrap();
        let temp_two = tempdir().unwrap();
        let _guard = override_trusted_state_dir_for_test(temp_two.path().to_path_buf());
        let guard = TrustedStateDirGuard {
            previous: Some(temp_one.path().to_path_buf()),
            _usage_lock: None,
        };

        poison_override_lock();

        let result = panic::catch_unwind(AssertUnwindSafe(|| drop(guard)));
        assert!(
            result.is_ok(),
            "TrustedStateDirGuard::drop should not panic"
        );
        assert_eq!(trusted_state_root().unwrap(), temp_one.path().to_path_buf());
    }
}
