use std::path::PathBuf;

use anyhow::Result;
use e2v_store::{
    LocalFolderBackend, OpendalS3Backend, OpendalWebdavBackend, RemoteTelemetryHandle,
    S3RemoteConfig, WebdavFlavor, WebdavRemoteConfig, WebdavVerifiedCapabilities,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteSpec {
    LocalFolder(PathBuf),
    S3(S3RemoteConfig),
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
        if let Some(raw_url) = value.strip_prefix("s3+") {
            return parse_s3_url(raw_url);
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
        let root = normalize_webdav_root(flavor, parsed.path());
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
        self.with_backend_telemetry(None, handler)
    }

    pub fn with_backend_telemetry<T>(
        &self,
        telemetry: Option<RemoteTelemetryHandle>,
        handler: impl FnOnce(RemoteBackendRef<'_>) -> Result<T>,
    ) -> Result<T> {
        match self {
            Self::LocalFolder(path) => {
                let backend = match telemetry.clone() {
                    Some(telemetry) => LocalFolderBackend::new_with_telemetry(path, telemetry),
                    None => LocalFolderBackend::new(path),
                };
                handler(RemoteBackendRef::LocalFolder(&backend))
            }
            Self::S3(config) => {
                let backend = match telemetry.clone() {
                    Some(telemetry) => {
                        OpendalS3Backend::new_with_telemetry(config.clone(), telemetry)?
                    }
                    None => OpendalS3Backend::new(config.clone())?,
                };
                handler(RemoteBackendRef::S3(&backend))
            }
            Self::Webdav(config) => {
                let backend = match telemetry {
                    Some(telemetry) => {
                        OpendalWebdavBackend::new_with_telemetry(config.clone(), telemetry)?
                    }
                    None => OpendalWebdavBackend::new(config.clone())?,
                };
                handler(RemoteBackendRef::Webdav(&backend))
            }
        }
    }
}

fn normalize_webdav_root(flavor: WebdavFlavor, raw_path: &str) -> String {
    let path = if raw_path.is_empty() { "/" } else { raw_path };
    if flavor != WebdavFlavor::Alist {
        return path.to_string();
    }

    if path == "/" {
        return "/dav".to_string();
    }
    if path == "/dav" || path.starts_with("/dav/") {
        return path.to_string();
    }
    let trimmed = path.trim_start_matches('/');
    format!("/dav/{trimmed}")
}

pub enum RemoteBackendRef<'a> {
    LocalFolder(&'a LocalFolderBackend),
    S3(&'a OpendalS3Backend),
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

fn parse_s3_url(raw_url: &str) -> Result<RemoteSpec> {
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
    let mut segments = parsed
        .path_segments()
        .ok_or_else(|| anyhow::anyhow!("s3 remote url must include a bucket path"))?;
    let bucket = segments
        .next()
        .filter(|segment| !segment.is_empty())
        .ok_or_else(|| anyhow::anyhow!("s3 remote url must include a bucket path"))?
        .to_string();
    let remainder = segments.collect::<Vec<_>>();
    let root = if remainder.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", remainder.join("/"))
    };

    Ok(RemoteSpec::S3(S3RemoteConfig {
        endpoint,
        bucket,
        root,
        region: parsed
            .query_pairs()
            .find(|(key, _)| key == "region")
            .map(|(_, value)| value.to_string()),
        access_key_id: if parsed.username().is_empty() {
            None
        } else {
            Some(parsed.username().to_string())
        },
        secret_access_key: parsed.password().map(ToString::to_string),
        session_token: parsed
            .query_pairs()
            .find(|(key, _)| key == "session_token")
            .map(|(_, value)| value.to_string()),
        disable_config_load: true,
    }))
}
