use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use e2v_sync::RemoteSpec;

const REMOTES_DIR: &str = ".e2v/remotes";
const DEFAULT_REMOTE_PATH: &str = ".e2v/remotes/default.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredRemote {
    pub name: String,
    pub spec: String,
}

pub fn add_remote(repo_root: &Path, name: &str, spec: &str) -> Result<StoredRemote> {
    anyhow::ensure!(!name.trim().is_empty(), "remote name must not be empty");
    let _ = RemoteSpec::parse(spec)?;
    let stored = StoredRemote {
        name: name.to_string(),
        spec: spec.to_string(),
    };
    let path = remote_path(repo_root, name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_vec(&stored)?)?;
    std::fs::write(default_remote_path(repo_root), serde_json::to_vec(&stored)?)?;
    Ok(stored)
}

pub fn load_default_remote(repo_root: &Path) -> Result<StoredRemote> {
    let bytes = std::fs::read(default_remote_path(repo_root))
        .map_err(|error| anyhow::anyhow!("failed to read default remote: {error}"))?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub fn load_default_remote_spec(repo_root: &Path) -> Result<RemoteSpec> {
    let stored = load_default_remote(repo_root)?;
    RemoteSpec::parse(&stored.spec)
}

fn remote_path(repo_root: &Path, name: &str) -> PathBuf {
    repo_root.join(REMOTES_DIR).join(format!("{name}.json"))
}

fn default_remote_path(repo_root: &Path) -> PathBuf {
    repo_root.join(DEFAULT_REMOTE_PATH)
}
