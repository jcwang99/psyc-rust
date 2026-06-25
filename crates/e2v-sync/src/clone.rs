use std::path::PathBuf;

use anyhow::{Result, ensure};

use e2v_core::RepositoryFacade;
use e2v_store::RemoteBackend;

use crate::fetch::{FetchOptions, fetch_remote, validate_remote_branch_control_plane};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloneOptions {
    pub repo_root: PathBuf,
    pub password: String,
    pub branch_token: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloneResult {
    pub branch_token: String,
    pub head_snapshot_id: Option<String>,
}

pub fn clone_remote<R: RemoteBackend>(remote: &R, options: CloneOptions) -> Result<CloneResult> {
    if options.repo_root.exists() {
        ensure!(
            std::fs::read_dir(&options.repo_root)?
                .next()
                .transpose()?
                .is_none(),
            "clone target directory must be empty"
        );
    } else {
        std::fs::create_dir_all(&options.repo_root)?;
    }
    validate_remote_branch_control_plane(remote, &options.repo_root, &options.branch_token)?;
    std::fs::create_dir_all(options.repo_root.join(".e2v").join("objects"))?;
    std::fs::create_dir_all(options.repo_root.join(".e2v").join("journal"))?;
    std::fs::create_dir_all(options.repo_root.join(".e2v").join("refs"))?;
    std::fs::create_dir_all(options.repo_root.join(".e2v").join("keyring"))?;

    let facade = RepositoryFacade::new();

    let fetch_result = fetch_remote(
        remote,
        FetchOptions {
            repo_root: options.repo_root.clone(),
            branch_token: options.branch_token.clone(),
            password: Some(options.password.clone()),
        },
    );
    if let Err(error) = fetch_result {
        let _ = std::fs::remove_dir_all(options.repo_root.join(".e2v"));
        return Err(error);
    }
    let reopened = match facade.unlock(&options.repo_root, &options.password) {
        Ok(reopened) => reopened,
        Err(error) => {
            let _ = std::fs::remove_dir_all(options.repo_root.join(".e2v"));
            return Err(error);
        }
    };
    if let Err(error) = facade.verify_ref(&options.repo_root) {
        let _ = std::fs::remove_dir_all(options.repo_root.join(".e2v"));
        return Err(error);
    }
    let head_snapshot_id = match facade.snapshots(&options.repo_root) {
        Ok(snapshots) => snapshots
            .first()
            .map(|snapshot| snapshot.snapshot_id.clone()),
        Err(error) => {
            let _ = std::fs::remove_dir_all(options.repo_root.join(".e2v"));
            return Err(error);
        }
    };

    Ok(CloneResult {
        branch_token: reopened.branch.token_hex,
        head_snapshot_id,
    })
}
