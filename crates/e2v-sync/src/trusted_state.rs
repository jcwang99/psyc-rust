use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

static TRUSTED_STATE_DIR_OVERRIDE: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TrustedRemoteState {
    pub repo_id: String,
    pub min_layout_generation: u64,
    pub min_keyring_generation: u64,
    pub min_ref_generation: u64,
}

#[doc(hidden)]
pub fn override_trusted_state_dir_for_test(path: PathBuf) -> TrustedStateDirGuard {
    let lock = TRUSTED_STATE_DIR_OVERRIDE.get_or_init(|| Mutex::new(None));
    let mut slot = lock.lock().unwrap();
    let previous = slot.replace(path);
    TrustedStateDirGuard { previous }
}

#[doc(hidden)]
pub struct TrustedStateDirGuard {
    previous: Option<PathBuf>,
}

impl Drop for TrustedStateDirGuard {
    fn drop(&mut self) {
        let lock = TRUSTED_STATE_DIR_OVERRIDE.get_or_init(|| Mutex::new(None));
        *lock.lock().unwrap() = self.previous.take();
    }
}

pub(crate) fn load_trusted_remote_state(repo_id: &str) -> Result<Option<TrustedRemoteState>> {
    let path = trusted_state_file_path(repo_id)?;
    if !path.is_file() {
        return Ok(None);
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
    std::fs::write(&path, bytes)
        .with_context(|| format!("failed to write trusted state {}", path.display()))?;
    Ok(())
}

fn trusted_state_file_path(repo_id: &str) -> Result<PathBuf> {
    anyhow::ensure!(
        !repo_id.trim().is_empty(),
        "trusted state repo_id must not be empty"
    );
    Ok(trusted_state_root()?.join(format!("{repo_id}.json")))
}

fn trusted_state_root() -> Result<PathBuf> {
    if let Some(path) = TRUSTED_STATE_DIR_OVERRIDE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap()
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
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn trusted_state_is_stored_as_compact_json() {
        let temp = tempdir().unwrap();
        let _guard = override_trusted_state_dir_for_test(temp.path().to_path_buf());
        let state = TrustedRemoteState {
            repo_id: "repo-123".to_string(),
            min_layout_generation: 7,
            min_keyring_generation: 11,
            min_ref_generation: 13,
        };

        store_trusted_remote_state(&state).unwrap();

        let bytes = std::fs::read(temp.path().join("repo-123.json")).unwrap();
        assert_eq!(
            bytes,
            serde_json::to_vec(&state).unwrap(),
            "trusted state files should not store pretty-printed JSON whitespace"
        );
    }
}
