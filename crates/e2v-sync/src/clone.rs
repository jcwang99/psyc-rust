use std::path::PathBuf;

use anyhow::Result;

use e2v_core::RepositoryFacade;
use e2v_store::{RefToken, RemoteBackend};

use crate::fetch::{fetch_remote, FetchOptions};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloneOptions {
    pub repo_root: PathBuf,
    pub password: String,
    pub branch_name: String,
    pub branch_token: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloneResult {
    pub branch_token: String,
    pub head_snapshot_id: Option<String>,
}

pub fn clone_remote<R: RemoteBackend>(remote: &R, options: CloneOptions) -> Result<CloneResult> {
    std::fs::create_dir_all(options.repo_root.join(".e2v").join("objects"))?;
    std::fs::create_dir_all(options.repo_root.join(".e2v").join("journal"))?;
    std::fs::create_dir_all(options.repo_root.join(".e2v").join("refs"))?;
    std::fs::create_dir_all(options.repo_root.join(".e2v").join("keyring"))?;

    let config_bytes = remote.get_physical("control/config.json")?;
    std::fs::write(
        options.repo_root.join(".e2v").join("config.json"),
        config_bytes,
    )?;
    for relative_path in remote.list_physical("control/keyring/")? {
        let file_name = relative_path
            .strip_prefix("control/keyring/")
            .ok_or_else(|| anyhow::anyhow!("invalid remote keyring path {relative_path}"))?;
        let bytes = remote.get_physical(&relative_path)?;
        std::fs::write(
            options.repo_root.join(".e2v").join("keyring").join(file_name),
            bytes,
        )?;
    }
    std::fs::write(
        options.repo_root.join(".e2v").join("layout_root.json"),
        serde_json::to_vec_pretty(&remote.read_layout_root()?)?,
    )?;
    let remote_ref = remote
        .read_ref(&RefToken::new(options.branch_token.clone()))?
        .ok_or_else(|| anyhow::anyhow!("remote branch ref not found"))?;
    std::fs::write(
        options.repo_root.join(".e2v").join("refs").join("default.json"),
        remote_ref.value.bytes,
    )?;

    let facade = RepositoryFacade::new();

    fetch_remote(
        remote,
        FetchOptions {
            repo_root: options.repo_root.clone(),
            branch_token: options.branch_token.clone(),
        },
    )?;
    let reopened = facade.unlock(&options.repo_root, &options.password)?;
    let head_snapshot_id = facade
        .snapshots(&options.repo_root)?
        .first()
        .map(|snapshot| snapshot.snapshot_id.clone());

    Ok(CloneResult {
        branch_token: reopened.branch.token_hex,
        head_snapshot_id,
    })
}
