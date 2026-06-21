use std::path::PathBuf;

use anyhow::Result;

use e2v_store::{RefToken, RemoteBackend};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchOptions {
    pub repo_root: PathBuf,
    pub branch_token: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchResult {
    pub downloaded_objects: usize,
}

pub fn fetch_remote<R: RemoteBackend>(remote: &R, options: FetchOptions) -> Result<FetchResult> {
    let objects_dir = options.repo_root.join(".e2v").join("objects");
    std::fs::create_dir_all(&objects_dir)?;

    let listed = remote.list_physical("objects/")?;
    let mut downloaded_objects = 0usize;
    for relative_path in listed {
        let file_name = relative_path
            .strip_prefix("objects/")
            .ok_or_else(|| anyhow::anyhow!("invalid remote object path {relative_path}"))?;
        let target_path = objects_dir.join(file_name);
        if !target_path.exists() {
            let bytes = remote.get_physical(&relative_path)?;
            std::fs::write(&target_path, bytes)?;
            downloaded_objects += 1;
        }
    }

    let ref_token = RefToken::new(options.branch_token);
    if let Some(stored_ref) = remote.read_ref(&ref_token)? {
        std::fs::write(
            options.repo_root.join(".e2v").join("refs").join("default.json"),
            stored_ref.value.bytes,
        )?;
    }

    let layout_root = remote.read_layout_root()?;
    std::fs::write(
        options.repo_root.join(".e2v").join("layout_root.json"),
        serde_json::to_vec_pretty(&layout_root)?,
    )?;

    Ok(FetchResult { downloaded_objects })
}
