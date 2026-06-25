use std::path::PathBuf;

use anyhow::Result;
use e2v_store::{
    LocalFolderBackend, OpendalWebdavBackend, WebdavFlavor, WebdavRemoteConfig,
    WebdavVerifiedCapabilities,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteSpec {
    LocalFolder(PathBuf),
    Webdav(WebdavRemoteConfig),
}

impl RemoteSpec {
    pub fn parse(value: &str) -> Result<Self> {
        if let Some(raw_url) = value.strip_prefix("file+") {
            return parse_file_url(raw_url);
        }
        if value.starts_with("file://") {
            return parse_file_url(value);
        }

        let (flavor, raw_url) = if let Some(url) = value.strip_prefix("webdav+") {
            (WebdavFlavor::Webdav, url)
        } else if let Some(url) = value.strip_prefix("alist+") {
            (WebdavFlavor::Alist, url)
        } else {
            anyhow::bail!("unsupported remote spec: {value}");
        };

        let parsed = url::Url::parse(raw_url)
            .map_err(|error| anyhow::anyhow!("invalid remote url {raw_url}: {error}"))?;
        anyhow::ensure!(
            parsed.scheme() == "https" || parsed.scheme() == "http",
            "unsupported remote url scheme: {}",
            parsed.scheme()
        );
        let host = parsed
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("remote url must include a host"))?;
        let endpoint = if let Some(port) = parsed.port() {
            format!("{}://{}:{port}", parsed.scheme(), host)
        } else {
            format!("{}://{}", parsed.scheme(), host)
        };
        let root = if parsed.path().is_empty() {
            "/".to_string()
        } else {
            parsed.path().to_string()
        };
        let username = if parsed.username().is_empty() {
            None
        } else if flavor == WebdavFlavor::Webdav {
            Some(parsed.username().to_string())
        } else {
            None
        };
        let password = if flavor == WebdavFlavor::Webdav {
            parsed.password().map(ToString::to_string)
        } else {
            None
        };
        let token = if flavor == WebdavFlavor::Alist && !parsed.username().is_empty() {
            Some(parsed.username().to_string())
        } else {
            None
        };

        Ok(Self::Webdav(WebdavRemoteConfig {
            flavor,
            endpoint,
            root,
            username,
            password,
            token,
            disable_create_dir: false,
            verified_capabilities: WebdavVerifiedCapabilities::default(),
        }))
    }

    pub fn with_backend<T>(
        &self,
        handler: impl FnOnce(RemoteBackendRef<'_>) -> Result<T>,
    ) -> Result<T> {
        match self {
            Self::LocalFolder(path) => {
                let backend = LocalFolderBackend::new(path);
                handler(RemoteBackendRef::LocalFolder(&backend))
            }
            Self::Webdav(config) => {
                let backend = OpendalWebdavBackend::new(config.clone())?;
                handler(RemoteBackendRef::Webdav(&backend))
            }
        }
    }
}

pub enum RemoteBackendRef<'a> {
    LocalFolder(&'a LocalFolderBackend),
    Webdav(&'a OpendalWebdavBackend),
}

fn parse_file_url(raw_url: &str) -> Result<RemoteSpec> {
    let parsed = url::Url::parse(raw_url)
        .map_err(|error| anyhow::anyhow!("invalid remote url {raw_url}: {error}"))?;
    anyhow::ensure!(
        parsed.scheme() == "file",
        "unsupported remote url scheme: {}",
        parsed.scheme()
    );
    let path = parsed
        .to_file_path()
        .map_err(|_| anyhow::anyhow!("file remote url must be an absolute filesystem path"))?;
    Ok(RemoteSpec::LocalFolder(path))
}
