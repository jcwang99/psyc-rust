use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::Engine;
use native_tls::TlsConnector;
use quick_xml::Reader as XmlReader;
use quick_xml::events::Event as XmlEvent;
use url::Url;

use crate::{
    BackendCapability, BlobStore, CasResult, ConsistencyClass, EncryptedRef, LayoutRoot,
    LayoutRootStore, LayoutRootVersion, ListedRef, ObjectStat, RefStore, RefToken, RefVersion,
    StoredRef, is_missing_physical_object_error,
    remote_telemetry::{RemoteOperationKind, RemoteTelemetryHandle},
};

pub trait RemoteBackend: BlobStore + RefStore + LayoutRootStore + Send + Sync {
    fn capability(&self) -> &BackendCapability;
}

static OPENDAL_RUNTIME: LazyLock<Result<tokio::runtime::Runtime>> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| anyhow::anyhow!(error).context("failed to build opendal runtime"))
});

fn opendal_runtime() -> Result<&'static tokio::runtime::Runtime> {
    OPENDAL_RUNTIME
        .as_ref()
        .map_err(|error| anyhow::anyhow!("{error:#}"))
}

#[derive(Clone)]
struct TelemetryOperator {
    inner: opendal::blocking::Operator,
    telemetry: Option<RemoteTelemetryHandle>,
}

impl TelemetryOperator {
    fn new(inner: opendal::blocking::Operator, telemetry: Option<RemoteTelemetryHandle>) -> Self {
        Self { inner, telemetry }
    }

    fn record_result<T>(
        &self,
        kind: RemoteOperationKind,
        path: &str,
        bytes_sent: u64,
        action: impl FnOnce(&opendal::blocking::Operator) -> opendal::Result<T>,
        summarize: impl FnOnce(&T) -> (u64, u64),
    ) -> opendal::Result<T> {
        let start = Instant::now();
        let result = action(&self.inner);
        if let Some(telemetry) = &self.telemetry {
            let (bytes_received, listed_entries) = match result.as_ref() {
                Ok(value) => summarize(value),
                Err(_) => (0, 0),
            };
            telemetry.record(
                kind,
                path,
                start.elapsed(),
                bytes_sent,
                bytes_received,
                listed_entries,
                result.is_ok(),
            );
        }
        result
    }

    fn write(&self, path: &str, bytes: Vec<u8>) -> opendal::Result<opendal::Metadata> {
        let bytes_sent = bytes.len() as u64;
        self.record_result(
            RemoteOperationKind::Write,
            path,
            bytes_sent,
            move |operator| operator.write(path, bytes),
            |_| (0, 0),
        )
    }

    fn write_options(
        &self,
        path: &str,
        bytes: Vec<u8>,
        opts: opendal::options::WriteOptions,
    ) -> opendal::Result<opendal::Metadata> {
        let bytes_sent = bytes.len() as u64;
        self.record_result(
            RemoteOperationKind::WriteIfAbsent,
            path,
            bytes_sent,
            move |operator| operator.write_options(path, bytes, opts),
            |_| (0, 0),
        )
    }

    fn read(&self, path: &str) -> opendal::Result<opendal::Buffer> {
        self.record_result(
            RemoteOperationKind::Read,
            path,
            0,
            |operator| operator.read(path),
            |buffer| (buffer.len() as u64, 0),
        )
    }

    fn read_options(
        &self,
        path: &str,
        opts: opendal::options::ReadOptions,
    ) -> opendal::Result<opendal::Buffer> {
        self.record_result(
            RemoteOperationKind::ReadRange,
            path,
            0,
            move |operator| operator.read_options(path, opts),
            |buffer| (buffer.len() as u64, 0),
        )
    }

    fn delete(&self, path: &str) -> opendal::Result<()> {
        self.record_result(
            RemoteOperationKind::Delete,
            path,
            0,
            |operator| operator.delete(path),
            |_| (0, 0),
        )
    }

    fn exists(&self, path: &str) -> opendal::Result<bool> {
        self.record_result(
            RemoteOperationKind::Exists,
            path,
            0,
            |operator| operator.exists(path),
            |_| (0, 0),
        )
    }

    fn stat(&self, path: &str) -> opendal::Result<opendal::Metadata> {
        self.record_result(
            RemoteOperationKind::Stat,
            path,
            0,
            |operator| operator.stat(path),
            |_| (0, 0),
        )
    }

    fn list(&self, path: &str) -> opendal::Result<Vec<opendal::Entry>> {
        self.record_result(
            RemoteOperationKind::List,
            path,
            0,
            |operator| operator.list(path),
            |entries| (0, entries.len() as u64),
        )
    }

    fn list_options(
        &self,
        path: &str,
        opts: opendal::options::ListOptions,
    ) -> opendal::Result<Vec<opendal::Entry>> {
        self.record_result(
            RemoteOperationKind::List,
            path,
            0,
            move |operator| operator.list_options(path, opts),
            |entries| (0, entries.len() as u64),
        )
    }
}

#[derive(Clone)]
pub struct OpendalMemoryBackend {
    operator: TelemetryOperator,
    capability: BackendCapability,
}

impl OpendalMemoryBackend {
    pub fn new() -> Result<Self> {
        Self::new_with_optional_telemetry(None)
    }

    pub fn new_with_telemetry(telemetry: RemoteTelemetryHandle) -> Result<Self> {
        Self::new_with_optional_telemetry(Some(telemetry))
    }

    fn new_with_optional_telemetry(telemetry: Option<RemoteTelemetryHandle>) -> Result<Self> {
        let _guard = opendal_runtime()?.enter();
        Ok(Self::from_operator_with_telemetry(
            opendal::blocking::Operator::new(
                opendal::Operator::new(opendal::services::Memory::default())?.finish(),
            )?,
            telemetry,
        ))
    }

    #[cfg(test)]
    fn from_operator(operator: opendal::blocking::Operator) -> Self {
        Self::from_operator_with_telemetry(operator, None)
    }

    fn from_operator_with_telemetry(
        operator: opendal::blocking::Operator,
        telemetry: Option<RemoteTelemetryHandle>,
    ) -> Self {
        Self {
            operator: TelemetryOperator::new(operator, telemetry),
            capability: BackendCapability {
                supports_conditional_put: false,
                supports_range_read: true,
                supports_atomic_rename: false,
                supports_paged_list: true,
                consistency_class: ConsistencyClass::UnknownOrEventual,
                supports_remote_lock_or_lease: false,
                supports_atomic_create_if_absent: true,
                supports_transaction_markers: false,
                supports_reliable_remote_time: false,
                supports_object_generation_or_etag: false,
                supports_layout_root_cas: false,
                supports_oblivious_access_schedule: false,
            },
        }
    }

    fn default_layout_root() -> LayoutRoot {
        LayoutRoot::direct_default()
    }

    fn ref_path(token: &RefToken) -> String {
        format!("control/refs/by-token/{}.json", token.value)
    }

    fn layout_history_path(generation: u64) -> String {
        format!("control/layout-roots/{generation:020}.json")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WebdavVerifiedCapabilities {
    pub supports_reliable_remote_time: bool,
    pub supports_object_generation_or_etag: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3RemoteConfig {
    pub endpoint: String,
    pub bucket: String,
    pub root: String,
    pub region: Option<String>,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
    pub session_token: Option<String>,
    pub disable_config_load: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebdavRemoteConfig {
    pub flavor: WebdavFlavor,
    pub endpoint: String,
    pub root: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub token: Option<String>,
    pub disable_create_dir: bool,
    pub verified_capabilities: WebdavVerifiedCapabilities,
}

#[derive(Clone)]
pub struct OpendalWebdavBackend {
    operator: TelemetryOperator,
    capability: BackendCapability,
    flavor: WebdavFlavor,
    compat_lister: Option<WebdavCompatLister>,
}

#[derive(Clone)]
pub struct OpendalS3Backend {
    operator: TelemetryOperator,
    capability: BackendCapability,
}

impl OpendalS3Backend {
    pub fn new(config: S3RemoteConfig) -> Result<Self> {
        Self::new_with_optional_telemetry(config, None)
    }

    pub fn new_with_telemetry(
        config: S3RemoteConfig,
        telemetry: RemoteTelemetryHandle,
    ) -> Result<Self> {
        Self::new_with_optional_telemetry(config, Some(telemetry))
    }

    fn new_with_optional_telemetry(
        config: S3RemoteConfig,
        telemetry: Option<RemoteTelemetryHandle>,
    ) -> Result<Self> {
        anyhow::ensure!(
            !config.endpoint.trim().is_empty(),
            "s3 endpoint must not be empty"
        );
        anyhow::ensure!(
            !config.bucket.trim().is_empty(),
            "s3 bucket must not be empty"
        );
        anyhow::ensure!(!config.root.trim().is_empty(), "s3 root must not be empty");

        let _guard = opendal_runtime()?.enter();
        let mut builder = opendal::services::S3::default()
            .endpoint(&config.endpoint)
            .bucket(&config.bucket)
            .root(&config.root);

        if let Some(region) = &config.region {
            builder = builder.region(region);
        }
        if let Some(access_key_id) = &config.access_key_id {
            builder = builder.access_key_id(access_key_id);
        }
        if let Some(secret_access_key) = &config.secret_access_key {
            builder = builder.secret_access_key(secret_access_key);
        }
        if let Some(session_token) = &config.session_token {
            builder = builder.session_token(session_token);
        }
        if config.disable_config_load {
            builder = builder.disable_config_load();
        }

        let operator = opendal::blocking::Operator::new(opendal::Operator::new(builder)?.finish())?;

        Ok(Self {
            operator: TelemetryOperator::new(operator, telemetry),
            capability: BackendCapability {
                supports_conditional_put: false,
                supports_range_read: true,
                supports_atomic_rename: false,
                supports_paged_list: true,
                consistency_class: ConsistencyClass::UnknownOrEventual,
                supports_remote_lock_or_lease: true,
                supports_atomic_create_if_absent: true,
                supports_transaction_markers: true,
                supports_reliable_remote_time: true,
                supports_object_generation_or_etag: true,
                supports_layout_root_cas: false,
                supports_oblivious_access_schedule: false,
            },
        })
    }
}

impl OpendalWebdavBackend {
    pub fn new(config: WebdavRemoteConfig) -> Result<Self> {
        Self::new_with_optional_telemetry(config, None)
    }

    pub fn new_with_telemetry(
        config: WebdavRemoteConfig,
        telemetry: RemoteTelemetryHandle,
    ) -> Result<Self> {
        Self::new_with_optional_telemetry(config, Some(telemetry))
    }

    fn new_with_optional_telemetry(
        config: WebdavRemoteConfig,
        telemetry: Option<RemoteTelemetryHandle>,
    ) -> Result<Self> {
        anyhow::ensure!(
            !config.endpoint.trim().is_empty(),
            "webdav endpoint must not be empty"
        );
        anyhow::ensure!(
            !config.root.trim().is_empty(),
            "webdav root must not be empty"
        );

        let _guard = opendal_runtime()?.enter();
        let mut builder = opendal::services::Webdav::default()
            .endpoint(&config.endpoint)
            .root(&config.root)
            .disable_create_dir(config.disable_create_dir);
        if let Some(username) = &config.username {
            builder = builder.username(username);
        }
        if let Some(password) = &config.password {
            builder = builder.password(password);
        }
        if let Some(token) = &config.token {
            builder = builder.token(token);
        }

        let operator = opendal::blocking::Operator::new(opendal::Operator::new(builder)?.finish())?;

        Ok(Self {
            operator: TelemetryOperator::new(operator, telemetry),
            capability: webdav_capability(&config.verified_capabilities),
            flavor: config.flavor,
            compat_lister: Some(WebdavCompatLister::new(&config)?),
        })
    }

    pub fn flavor(&self) -> WebdavFlavor {
        self.flavor
    }

    fn list_paths_via_shallow_walk(&self, root: &str) -> Result<Vec<String>> {
        let mut pending = vec![root.to_string()];
        let mut visited = HashSet::new();
        let mut files = Vec::new();

        while let Some(prefix) = pending.pop() {
            if !visited.insert(prefix.clone()) {
                continue;
            }

            let entries = self.list_entries_shallow(&prefix)?;

            for entry in entries {
                let path = entry.path;
                if path == prefix {
                    continue;
                }
                if entry.is_dir {
                    pending.push(path);
                } else {
                    files.push(path);
                }
            }
        }

        files.sort();
        Ok(files)
    }

    fn read_listed_refs_from_paths(&self, paths: Vec<String>) -> Result<Vec<ListedRef>> {
        let mut listed = paths
            .into_iter()
            .filter_map(|path| {
                let token = path
                    .strip_prefix("control/refs/by-token/")?
                    .strip_suffix(".json")?
                    .to_string();
                Some((path, token))
            })
            .map(|(path, token)| -> Result<ListedRef> {
                let token = RefToken::new(token);
                token.validate()?;
                Ok(ListedRef {
                    token,
                    stored: serde_json::from_slice(&self.get_physical(&path)?)
                        .context("failed to decode remote branch ref")?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        listed.sort_by(|left, right| left.token.value.cmp(&right.token.value));
        Ok(listed)
    }

    fn list_entries_shallow(&self, prefix: &str) -> Result<Vec<WebdavListEntry>> {
        if let Some(compat_lister) = &self.compat_lister {
            return compat_lister.list_one_level(prefix);
        }

        let entries = match self.operator.list(prefix) {
            Ok(entries) => entries,
            Err(error) if error.kind() == opendal::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };

        Ok(entries
            .into_iter()
            .map(|entry| WebdavListEntry {
                is_dir: entry.path().ends_with('/'),
                path: entry.path().to_string(),
            })
            .collect())
    }

    #[cfg(test)]
    fn from_operator(operator: opendal::blocking::Operator, flavor: WebdavFlavor) -> Self {
        Self {
            operator: TelemetryOperator::new(operator, None),
            capability: webdav_capability(&WebdavVerifiedCapabilities::default()),
            flavor,
            compat_lister: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WebdavListEntry {
    path: String,
    is_dir: bool,
}

impl WebdavListEntry {
    fn dir(path: &str) -> Self {
        Self {
            is_dir: true,
            path: path.to_string(),
        }
    }

    fn file(path: &str) -> Self {
        Self {
            is_dir: false,
            path: path.to_string(),
        }
    }
}

#[derive(Clone)]
struct WebdavCompatLister {
    endpoint: String,
    root: String,
    auth: WebdavCompatAuth,
}

#[derive(Clone)]
enum WebdavCompatAuth {
    None,
    Basic { password: String, username: String },
    Bearer(String),
}

impl WebdavCompatLister {
    fn new(config: &WebdavRemoteConfig) -> Result<Self> {
        let auth = match (&config.username, &config.password, &config.token) {
            (Some(username), Some(password), _) => WebdavCompatAuth::Basic {
                password: password.clone(),
                username: username.clone(),
            },
            (_, _, Some(token)) => WebdavCompatAuth::Bearer(token.clone()),
            _ => WebdavCompatAuth::None,
        };
        Ok(Self {
            endpoint: config.endpoint.clone(),
            root: config.root.clone(),
            auth,
        })
    }

    fn build_rooted_abs_path(root: &str, path: &str) -> String {
        let normalized_root = if root.is_empty() {
            "/"
        } else {
            root.trim_end_matches('/')
        };
        let normalized_path = path.trim_start_matches('/');
        if normalized_path.is_empty() {
            if normalized_root == "/" {
                "/".to_string()
            } else {
                format!("{normalized_root}/")
            }
        } else if normalized_root == "/" {
            format!("/{normalized_path}")
        } else {
            format!("{normalized_root}/{normalized_path}")
        }
    }

    fn build_host_header(url: &Url) -> Result<String> {
        let host = url
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("webdav endpoint must include a host"))?;
        let default_port = match url.scheme() {
            "http" => Some(80),
            "https" => Some(443),
            _ => None,
        };
        Ok(match url.port() {
            Some(port) if Some(port) != default_port => format!("{host}:{port}"),
            _ => host.to_string(),
        })
    }

    fn authorization_header(&self) -> Option<String> {
        match &self.auth {
            WebdavCompatAuth::None => None,
            WebdavCompatAuth::Basic { username, password } => Some(format!(
                "Authorization: Basic {}\r\n",
                base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"))
            )),
            WebdavCompatAuth::Bearer(token) => Some(format!("Authorization: Bearer {token}\r\n")),
        }
    }

    fn build_propfind_request(&self, url: &Url, rooted_path: &str) -> Result<Vec<u8>> {
        let host_header = Self::build_host_header(url)?;
        let authorization_header = self.authorization_header().unwrap_or_default();
        Ok(format!(
            "PROPFIND {rooted_path} HTTP/1.1\r\n\
Depth: 1\r\n\
Content-Length: 0\r\n\
Accept: */*\r\n\
Host: {host_header}\r\n\
{authorization_header}\
Connection: close\r\n\
\r\n"
        )
        .into_bytes())
    }

    fn exchange_http_bytes<S: Read + Write>(
        stream: &mut S,
        request_bytes: &[u8],
    ) -> Result<Vec<u8>> {
        stream
            .write_all(request_bytes)
            .context("failed to write webdav PROPFIND request")?;
        stream
            .flush()
            .context("failed to flush webdav PROPFIND request")?;
        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .context("failed to read webdav PROPFIND response")?;
        Ok(response)
    }

    fn send_propfind_request(url: &Url, request_bytes: &[u8]) -> Result<Vec<u8>> {
        let host = url
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("webdav endpoint must include a host"))?;
        let port = url
            .port_or_known_default()
            .ok_or_else(|| anyhow::anyhow!("webdav endpoint must include a known port"))?;
        let mut tcp = TcpStream::connect((host, port))
            .with_context(|| format!("failed to connect to webdav host {host}:{port}"))?;
        tcp.set_read_timeout(Some(Duration::from_secs(30)))
            .context("failed to configure webdav read timeout")?;
        tcp.set_write_timeout(Some(Duration::from_secs(30)))
            .context("failed to configure webdav write timeout")?;
        match url.scheme() {
            "http" => Self::exchange_http_bytes(&mut tcp, request_bytes),
            "https" => {
                let connector =
                    TlsConnector::new().context("failed to build webdav TLS connector")?;
                let mut tls = connector.connect(host, tcp).with_context(|| {
                    format!("failed to establish webdav TLS session for {host}")
                })?;
                Self::exchange_http_bytes(&mut tls, request_bytes)
            }
            scheme => Err(anyhow::anyhow!(
                "unsupported webdav endpoint scheme: {scheme}"
            )),
        }
    }

    fn parse_http_response(response_bytes: &[u8]) -> Result<(u16, Vec<u8>)> {
        let header_end = response_bytes
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .ok_or_else(|| anyhow::anyhow!("webdav PROPFIND response missing header terminator"))?;
        let header_text = std::str::from_utf8(&response_bytes[..header_end])
            .context("webdav PROPFIND response headers were not valid UTF-8")?;
        let status_line = header_text
            .lines()
            .next()
            .ok_or_else(|| anyhow::anyhow!("webdav PROPFIND response missing status line"))?;
        let status = status_line
            .split_whitespace()
            .nth(1)
            .ok_or_else(|| anyhow::anyhow!("webdav PROPFIND response missing status code"))?
            .parse::<u16>()
            .context("webdav PROPFIND status code was not numeric")?;
        Ok((status, response_bytes[header_end + 4..].to_vec()))
    }

    fn list_one_level(&self, path: &str) -> Result<Vec<WebdavListEntry>> {
        let rooted_path = Self::build_rooted_abs_path(&self.root, path);
        let url = Url::parse(&self.endpoint).context("invalid webdav endpoint for listing")?;
        let request_bytes = self.build_propfind_request(&url, &rooted_path)?;
        let response_bytes = Self::send_propfind_request(&url, &request_bytes)
            .with_context(|| format!("failed to list webdav path {rooted_path}"))?;
        let (status, body_bytes) = Self::parse_http_response(&response_bytes)?;
        match status {
            207 => {
                let xml = String::from_utf8(body_bytes).with_context(|| {
                    format!("failed to decode webdav listing body for {rooted_path}")
                })?;
                Self::parse_propfind_entries(&self.root, &rooted_path, &xml)
            }
            404 => Ok(Vec::new()),
            _ => Err(anyhow::anyhow!(
                "webdav shallow PROPFIND returned {} for {}",
                status,
                rooted_path
            )),
        }
    }

    fn parse_propfind_entries(
        root: &str,
        requested_rooted_path: &str,
        xml: &str,
    ) -> Result<Vec<WebdavListEntry>> {
        let mut reader = XmlReader::from_str(xml);
        reader.config_mut().trim_text(true);

        let mut buf = Vec::new();
        let mut entries = Vec::new();
        let mut current_href: Option<String> = None;
        let mut current_is_dir = false;
        let mut in_response = false;
        let mut in_href = false;

        loop {
            match reader.read_event_into(&mut buf) {
                Ok(XmlEvent::Start(event)) => match Self::local_name(event.name().as_ref()) {
                    b"response" => {
                        current_href = None;
                        current_is_dir = false;
                        in_response = true;
                        in_href = false;
                    }
                    b"href" if in_response => in_href = true,
                    b"collection" if in_response => current_is_dir = true,
                    _ => {}
                },
                Ok(XmlEvent::Empty(event)) => {
                    if Self::local_name(event.name().as_ref()) == b"collection" && in_response {
                        current_is_dir = true;
                    }
                }
                Ok(XmlEvent::Text(text)) if in_href => {
                    current_href = Some(
                        text.decode()
                            .context("failed to decode webdav href text")?
                            .into_owned(),
                    );
                }
                Ok(XmlEvent::CData(text)) if in_href => {
                    current_href = Some(
                        text.decode()
                            .context("failed to decode webdav href CDATA")?
                            .into_owned(),
                    );
                }
                Ok(XmlEvent::End(event)) => match Self::local_name(event.name().as_ref()) {
                    b"href" => in_href = false,
                    b"response" => {
                        if let Some(href) = current_href.take() {
                            if let Some(entry) = Self::entry_from_href(
                                root,
                                requested_rooted_path,
                                &href,
                                current_is_dir,
                            )? {
                                entries.push(entry);
                            }
                        }
                        current_is_dir = false;
                        in_response = false;
                        in_href = false;
                    }
                    _ => {}
                },
                Ok(XmlEvent::Eof) => break,
                Err(error) => {
                    return Err(
                        anyhow::anyhow!(error).context("failed to parse webdav PROPFIND response")
                    );
                }
                _ => {}
            }
            buf.clear();
        }

        Ok(entries)
    }

    fn entry_from_href(
        root: &str,
        requested_rooted_path: &str,
        href: &str,
        current_is_dir: bool,
    ) -> Result<Option<WebdavListEntry>> {
        let href_path = if let Ok(url) = Url::parse(href) {
            url.path().to_string()
        } else {
            href.to_string()
        };
        let is_dir = current_is_dir || href_path.ends_with('/');
        let normalized_href = Self::normalize_href_path(&href_path, is_dir);
        let normalized_requested = Self::normalize_href_path(requested_rooted_path, true);

        if Self::trim_trailing_slash(&normalized_href)
            == Self::trim_trailing_slash(&normalized_requested)
        {
            return Ok(None);
        }

        let relative = Self::path_relative_to_root(root, &normalized_href).ok_or_else(|| {
            anyhow::anyhow!(
                "webdav PROPFIND href {} is outside configured root {}",
                normalized_href,
                root
            )
        })?;

        Ok(Some(if is_dir {
            WebdavListEntry::dir(&relative)
        } else {
            WebdavListEntry::file(&relative)
        }))
    }

    fn normalize_href_path(path: &str, is_dir: bool) -> String {
        let mut normalized = if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{path}")
        };
        if is_dir && !normalized.ends_with('/') {
            normalized.push('/');
        }
        normalized
    }

    fn path_relative_to_root(root: &str, absolute_path: &str) -> Option<String> {
        let normalized_root = root.trim_end_matches('/');
        if normalized_root.is_empty() || normalized_root == "/" {
            return Some(absolute_path.trim_start_matches('/').to_string());
        }
        absolute_path
            .strip_prefix(normalized_root)
            .map(|rest| rest.trim_start_matches('/').to_string())
    }

    fn trim_trailing_slash(path: &str) -> &str {
        if path == "/" {
            return path;
        }
        path.trim_end_matches('/')
    }

    fn local_name(name: &[u8]) -> &[u8] {
        name.rsplit(|byte| *byte == b':').next().unwrap_or(name)
    }
}

#[derive(Debug, Clone)]
pub struct S3CompatibleMockBackend {
    inner: crate::memory_backend::MemoryBackend,
    capability: BackendCapability,
}

impl Default for S3CompatibleMockBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl S3CompatibleMockBackend {
    pub fn new() -> Self {
        Self {
            inner: crate::memory_backend::MemoryBackend::new(),
            capability: BackendCapability {
                supports_conditional_put: true,
                supports_range_read: true,
                supports_atomic_rename: false,
                supports_paged_list: true,
                consistency_class: ConsistencyClass::UnknownOrEventual,
                supports_remote_lock_or_lease: true,
                supports_atomic_create_if_absent: true,
                supports_transaction_markers: true,
                supports_reliable_remote_time: false,
                supports_object_generation_or_etag: true,
                supports_layout_root_cas: true,
                supports_oblivious_access_schedule: false,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebdavFlavor {
    Webdav,
    Alist,
}

#[derive(Debug, Clone)]
pub struct WebdavAlistMockBackend {
    inner: crate::memory_backend::MemoryBackend,
    capability: BackendCapability,
    flavor: WebdavFlavor,
}

impl WebdavAlistMockBackend {
    pub fn webdav() -> Self {
        Self::with_capability(WebdavFlavor::Webdav, false)
    }

    pub fn alist() -> Self {
        Self::with_capability(WebdavFlavor::Alist, false)
    }

    pub fn verified_single_writer(flavor: WebdavFlavor) -> Self {
        Self::with_capability(flavor, true)
    }

    pub fn flavor(&self) -> WebdavFlavor {
        self.flavor
    }

    fn with_capability(flavor: WebdavFlavor, verified_single_writer: bool) -> Self {
        Self {
            inner: crate::memory_backend::MemoryBackend::new(),
            capability: webdav_capability(&WebdavVerifiedCapabilities {
                supports_reliable_remote_time: verified_single_writer,
                supports_object_generation_or_etag: verified_single_writer,
            }),
            flavor,
        }
    }
}

fn webdav_capability(verified_capabilities: &WebdavVerifiedCapabilities) -> BackendCapability {
    BackendCapability {
        supports_conditional_put: false,
        supports_range_read: true,
        supports_atomic_rename: false,
        supports_paged_list: false,
        consistency_class: ConsistencyClass::UnknownOrEventual,
        supports_remote_lock_or_lease: true,
        supports_atomic_create_if_absent: false,
        supports_transaction_markers: true,
        supports_reliable_remote_time: verified_capabilities.supports_reliable_remote_time,
        supports_object_generation_or_etag: verified_capabilities
            .supports_object_generation_or_etag,
        supports_layout_root_cas: false,
        supports_oblivious_access_schedule: false,
    }
}

impl BlobStore for S3CompatibleMockBackend {
    fn put_physical(&self, relative_path: &str, bytes: &[u8]) -> Result<()> {
        self.inner.put_physical(relative_path, bytes)
    }

    fn put_physical_if_absent(&self, relative_path: &str, bytes: &[u8]) -> Result<bool> {
        self.inner.put_physical_if_absent(relative_path, bytes)
    }

    fn get_physical(&self, relative_path: &str) -> Result<Vec<u8>> {
        self.inner.get_physical(relative_path)
    }

    fn get_physical_range(
        &self,
        relative_path: &str,
        offset: usize,
        length: usize,
    ) -> Result<Vec<u8>> {
        self.inner.get_physical_range(relative_path, offset, length)
    }

    fn delete_physical(&self, relative_path: &str) -> Result<()> {
        self.inner.delete_physical(relative_path)
    }

    fn exists_physical(&self, relative_path: &str) -> bool {
        self.inner.exists_physical(relative_path)
    }

    fn stat_physical(&self, relative_path: &str) -> Result<ObjectStat> {
        self.inner.stat_physical(relative_path)
    }

    fn list_physical(&self, prefix: &str) -> Result<Vec<String>> {
        self.inner.list_physical(prefix)
    }
}

impl BlobStore for WebdavAlistMockBackend {
    fn put_physical(&self, relative_path: &str, bytes: &[u8]) -> Result<()> {
        self.inner.put_physical(relative_path, bytes)
    }

    fn put_physical_if_absent(&self, relative_path: &str, bytes: &[u8]) -> Result<bool> {
        self.inner.put_physical_if_absent(relative_path, bytes)
    }

    fn get_physical(&self, relative_path: &str) -> Result<Vec<u8>> {
        self.inner.get_physical(relative_path)
    }

    fn get_physical_range(
        &self,
        relative_path: &str,
        offset: usize,
        length: usize,
    ) -> Result<Vec<u8>> {
        self.inner.get_physical_range(relative_path, offset, length)
    }

    fn delete_physical(&self, relative_path: &str) -> Result<()> {
        self.inner.delete_physical(relative_path)
    }

    fn exists_physical(&self, relative_path: &str) -> bool {
        self.inner.exists_physical(relative_path)
    }

    fn stat_physical(&self, relative_path: &str) -> Result<ObjectStat> {
        self.inner.stat_physical(relative_path)
    }

    fn list_physical(&self, prefix: &str) -> Result<Vec<String>> {
        self.inner.list_physical(prefix)
    }
}

impl BlobStore for OpendalMemoryBackend {
    fn put_physical(&self, relative_path: &str, bytes: &[u8]) -> Result<()> {
        self.operator.write(relative_path, bytes.to_vec())?;
        Ok(())
    }

    fn put_physical_if_absent(&self, relative_path: &str, bytes: &[u8]) -> Result<bool> {
        let opts = opendal::options::WriteOptions {
            if_not_exists: true,
            ..Default::default()
        };
        match self
            .operator
            .write_options(relative_path, bytes.to_vec(), opts)
        {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == opendal::ErrorKind::ConditionNotMatch => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn get_physical(&self, relative_path: &str) -> Result<Vec<u8>> {
        Ok(self.operator.read(relative_path)?.to_vec())
    }

    fn get_physical_range(
        &self,
        relative_path: &str,
        offset: usize,
        length: usize,
    ) -> Result<Vec<u8>> {
        let total_length = self.stat_physical(relative_path)?.length as usize;
        anyhow::ensure!(offset <= total_length, "range offset out of bounds");
        let end = offset.saturating_add(length).min(total_length);
        Ok(self
            .operator
            .read_options(
                relative_path,
                opendal::options::ReadOptions {
                    range: (offset as u64..end as u64).into(),
                    ..Default::default()
                },
            )?
            .to_vec())
    }

    fn delete_physical(&self, relative_path: &str) -> Result<()> {
        match self.operator.delete(relative_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == opendal::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn exists_physical(&self, relative_path: &str) -> bool {
        self.operator.exists(relative_path).unwrap_or(false)
    }

    fn stat_physical(&self, relative_path: &str) -> Result<ObjectStat> {
        let metadata = self.operator.stat(relative_path)?;
        Ok(ObjectStat {
            length: metadata.content_length(),
            modified_at: metadata.last_modified().map(Into::into),
        })
    }

    fn list_physical(&self, prefix: &str) -> Result<Vec<String>> {
        let mut listed = self
            .operator
            .list(prefix)?
            .into_iter()
            .filter_map(|entry: opendal::Entry| {
                let path = entry.path().to_string();
                if path == prefix || path.ends_with('/') {
                    None
                } else {
                    Some(path)
                }
            })
            .collect::<Vec<_>>();
        listed.sort();
        Ok(listed)
    }
}

impl BlobStore for OpendalWebdavBackend {
    fn put_physical(&self, relative_path: &str, bytes: &[u8]) -> Result<()> {
        self.operator.write(relative_path, bytes.to_vec())?;
        Ok(())
    }

    fn put_physical_if_absent(&self, relative_path: &str, bytes: &[u8]) -> Result<bool> {
        let opts = opendal::options::WriteOptions {
            if_not_exists: true,
            ..Default::default()
        };
        match self
            .operator
            .write_options(relative_path, bytes.to_vec(), opts)
        {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == opendal::ErrorKind::ConditionNotMatch => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn get_physical(&self, relative_path: &str) -> Result<Vec<u8>> {
        Ok(self.operator.read(relative_path)?.to_vec())
    }

    fn get_physical_range(
        &self,
        relative_path: &str,
        offset: usize,
        length: usize,
    ) -> Result<Vec<u8>> {
        let total_length = self.stat_physical(relative_path)?.length as usize;
        anyhow::ensure!(offset <= total_length, "range offset out of bounds");
        let end = offset.saturating_add(length).min(total_length);
        Ok(self
            .operator
            .read_options(
                relative_path,
                opendal::options::ReadOptions {
                    range: (offset as u64..end as u64).into(),
                    ..Default::default()
                },
            )?
            .to_vec())
    }

    fn delete_physical(&self, relative_path: &str) -> Result<()> {
        match self.operator.delete(relative_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == opendal::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn exists_physical(&self, relative_path: &str) -> bool {
        self.operator.exists(relative_path).unwrap_or(false)
    }

    fn stat_physical(&self, relative_path: &str) -> Result<ObjectStat> {
        let metadata = self.operator.stat(relative_path)?;
        Ok(ObjectStat {
            length: metadata.content_length(),
            modified_at: metadata.last_modified().map(Into::into),
        })
    }

    fn list_physical(&self, prefix: &str) -> Result<Vec<String>> {
        let mut listed = self
            .list_entries_shallow(prefix)?
            .into_iter()
            .filter_map(|entry| {
                if entry.path == prefix || entry.is_dir {
                    None
                } else {
                    Some(entry.path)
                }
            })
            .collect::<Vec<_>>();
        listed.sort();
        Ok(listed)
    }
}

impl RefStore for S3CompatibleMockBackend {
    fn read_ref(&self, token: &RefToken) -> Result<Option<StoredRef>> {
        token.validate()?;
        self.inner.read_ref(token)
    }

    fn list_refs(&self) -> Result<Vec<ListedRef>> {
        self.inner.list_refs()
    }

    fn compare_and_swap_ref(
        &self,
        token: &RefToken,
        expected: Option<RefVersion>,
        next: EncryptedRef,
    ) -> Result<CasResult> {
        token.validate()?;
        self.inner.compare_and_swap_ref(token, expected, next)
    }
}

impl RefStore for WebdavAlistMockBackend {
    fn read_ref(&self, token: &RefToken) -> Result<Option<StoredRef>> {
        token.validate()?;
        self.inner.read_ref(token)
    }

    fn list_refs(&self) -> Result<Vec<ListedRef>> {
        self.inner.list_refs()
    }

    fn compare_and_swap_ref(
        &self,
        token: &RefToken,
        expected: Option<RefVersion>,
        next: EncryptedRef,
    ) -> Result<CasResult> {
        token.validate()?;
        self.inner.compare_and_swap_ref(token, expected, next)
    }
}

impl RefStore for OpendalMemoryBackend {
    fn read_ref(&self, token: &RefToken) -> Result<Option<StoredRef>> {
        token.validate()?;
        let path = Self::ref_path(token);
        match self.get_physical(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .context("failed to decode remote branch ref")
                .map(Some),
            Err(error) if is_missing_physical_object_error(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn list_refs(&self) -> Result<Vec<ListedRef>> {
        let mut listed = self
            .operator
            .list_options(
                "control/refs/by-token/",
                opendal::options::ListOptions {
                    recursive: true,
                    ..Default::default()
                },
            )?
            .into_iter()
            .filter_map(|entry: opendal::Entry| {
                let path = entry.path().to_string();
                let token = path
                    .strip_prefix("control/refs/by-token/")?
                    .strip_suffix(".json")?
                    .to_string();
                Some((path, token))
            })
            .map(|(path, token)| -> Result<ListedRef> {
                let token = RefToken::new(token);
                token.validate()?;
                Ok(ListedRef {
                    token,
                    stored: serde_json::from_slice(&self.get_physical(&path)?)
                        .context("failed to decode remote branch ref")?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        listed.sort_by(|left, right| left.token.value.cmp(&right.token.value));
        Ok(listed)
    }

    fn compare_and_swap_ref(
        &self,
        token: &RefToken,
        expected: Option<RefVersion>,
        next: EncryptedRef,
    ) -> Result<CasResult> {
        token.validate()?;
        let current = self.read_ref(token)?;
        let matches = match (&current, expected) {
            (None, None) => true,
            (Some(stored), Some(expected_version)) => stored.version == expected_version,
            _ => false,
        };
        if !matches {
            return Ok(CasResult {
                applied: false,
                current,
            });
        }

        let stored = StoredRef {
            version: RefVersion {
                value: current
                    .as_ref()
                    .map(|existing| existing.version.value + 1)
                    .unwrap_or(1),
            },
            value: next,
        };
        self.put_physical(&Self::ref_path(token), &serde_json::to_vec(&stored)?)?;
        Ok(CasResult {
            applied: true,
            current: Some(stored),
        })
    }
}

impl RefStore for OpendalWebdavBackend {
    fn read_ref(&self, token: &RefToken) -> Result<Option<StoredRef>> {
        token.validate()?;
        let path = OpendalMemoryBackend::ref_path(token);
        match self.get_physical(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .context("failed to decode remote branch ref")
                .map(Some),
            Err(error) if is_missing_physical_object_error(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn list_refs(&self) -> Result<Vec<ListedRef>> {
        let paths = self.list_paths_via_shallow_walk("control/refs/by-token/")?;
        self.read_listed_refs_from_paths(paths)
    }

    fn compare_and_swap_ref(
        &self,
        token: &RefToken,
        expected: Option<RefVersion>,
        next: EncryptedRef,
    ) -> Result<CasResult> {
        token.validate()?;
        let current = self.read_ref(token)?;
        let matches = match (&current, expected) {
            (None, None) => true,
            (Some(stored), Some(expected_version)) => stored.version == expected_version,
            _ => false,
        };
        if !matches {
            return Ok(CasResult {
                applied: false,
                current,
            });
        }

        let stored = StoredRef {
            version: RefVersion {
                value: current
                    .as_ref()
                    .map(|existing| existing.version.value + 1)
                    .unwrap_or(1),
            },
            value: next,
        };
        self.put_physical(
            &OpendalMemoryBackend::ref_path(token),
            &serde_json::to_vec(&stored)?,
        )?;
        Ok(CasResult {
            applied: true,
            current: Some(stored),
        })
    }
}

impl LayoutRootStore for S3CompatibleMockBackend {
    fn read_layout_root(&self) -> Result<LayoutRoot> {
        self.inner.read_layout_root()
    }

    fn compare_and_swap_layout_root(
        &self,
        expected: LayoutRootVersion,
        next: LayoutRoot,
    ) -> Result<CasResult> {
        self.inner.compare_and_swap_layout_root(expected, next)
    }

    fn list_retained_layout_roots(&self) -> Result<Vec<LayoutRoot>> {
        self.inner.list_retained_layout_roots()
    }
}

impl LayoutRootStore for WebdavAlistMockBackend {
    fn read_layout_root(&self) -> Result<LayoutRoot> {
        self.inner.read_layout_root()
    }

    fn compare_and_swap_layout_root(
        &self,
        expected: LayoutRootVersion,
        next: LayoutRoot,
    ) -> Result<CasResult> {
        self.inner.compare_and_swap_layout_root(expected, next)
    }

    fn list_retained_layout_roots(&self) -> Result<Vec<LayoutRoot>> {
        self.inner.list_retained_layout_roots()
    }
}

impl LayoutRootStore for OpendalMemoryBackend {
    fn read_layout_root(&self) -> Result<LayoutRoot> {
        match self.get_physical("layout_root.json") {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).context("failed to decode remote layout root")
            }
            Err(error) if is_missing_physical_object_error(&error) => {
                Ok(Self::default_layout_root())
            }
            Err(error) => Err(error),
        }
    }

    fn compare_and_swap_layout_root(
        &self,
        expected: LayoutRootVersion,
        next: LayoutRoot,
    ) -> Result<CasResult> {
        let current = self.read_layout_root()?;
        if current.generation != expected {
            return Ok(CasResult {
                applied: false,
                current: None,
            });
        }

        let bytes = serde_json::to_vec(&next)?;
        self.put_physical("layout_root.json", &bytes)?;
        self.put_physical(&Self::layout_history_path(next.generation), &bytes)?;
        Ok(CasResult {
            applied: true,
            current: None,
        })
    }

    fn list_retained_layout_roots(&self) -> Result<Vec<LayoutRoot>> {
        let retained_paths = self.list_physical("control/layout-roots/")?;
        if retained_paths.is_empty() {
            return Ok(vec![self.read_layout_root()?]);
        }

        retained_paths
            .into_iter()
            .map(|path| serde_json::from_slice(&self.get_physical(&path)?).map_err(Into::into))
            .collect()
    }
}

impl LayoutRootStore for OpendalWebdavBackend {
    fn read_layout_root(&self) -> Result<LayoutRoot> {
        match self.get_physical("layout_root.json") {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).context("failed to decode remote layout root")
            }
            Err(error) if is_missing_physical_object_error(&error) => {
                Ok(OpendalMemoryBackend::default_layout_root())
            }
            Err(error) => Err(error),
        }
    }

    fn compare_and_swap_layout_root(
        &self,
        expected: LayoutRootVersion,
        next: LayoutRoot,
    ) -> Result<CasResult> {
        let current = self.read_layout_root()?;
        if current.generation != expected {
            return Ok(CasResult {
                applied: false,
                current: None,
            });
        }

        let bytes = serde_json::to_vec(&next)?;
        self.put_physical("layout_root.json", &bytes)?;
        self.put_physical(
            &OpendalMemoryBackend::layout_history_path(next.generation),
            &bytes,
        )?;
        Ok(CasResult {
            applied: true,
            current: None,
        })
    }

    fn list_retained_layout_roots(&self) -> Result<Vec<LayoutRoot>> {
        let retained_paths = self.list_physical("control/layout-roots/")?;
        if retained_paths.is_empty() {
            return Ok(vec![self.read_layout_root()?]);
        }

        retained_paths
            .into_iter()
            .map(|path| serde_json::from_slice(&self.get_physical(&path)?).map_err(Into::into))
            .collect()
    }
}

impl RemoteBackend for S3CompatibleMockBackend {
    fn capability(&self) -> &BackendCapability {
        &self.capability
    }
}

impl RemoteBackend for WebdavAlistMockBackend {
    fn capability(&self) -> &BackendCapability {
        &self.capability
    }
}

impl RemoteBackend for OpendalMemoryBackend {
    fn capability(&self) -> &BackendCapability {
        &self.capability
    }
}

impl BlobStore for OpendalS3Backend {
    fn put_physical(&self, relative_path: &str, bytes: &[u8]) -> Result<()> {
        self.operator.write(relative_path, bytes.to_vec())?;
        Ok(())
    }

    fn put_physical_if_absent(&self, relative_path: &str, bytes: &[u8]) -> Result<bool> {
        let opts = opendal::options::WriteOptions {
            if_not_exists: true,
            ..Default::default()
        };
        match self
            .operator
            .write_options(relative_path, bytes.to_vec(), opts)
        {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == opendal::ErrorKind::ConditionNotMatch => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn get_physical(&self, relative_path: &str) -> Result<Vec<u8>> {
        Ok(self.operator.read(relative_path)?.to_vec())
    }

    fn get_physical_range(
        &self,
        relative_path: &str,
        offset: usize,
        length: usize,
    ) -> Result<Vec<u8>> {
        let total_length = self.stat_physical(relative_path)?.length as usize;
        anyhow::ensure!(offset <= total_length, "range offset out of bounds");
        let end = offset.saturating_add(length).min(total_length);
        Ok(self
            .operator
            .read_options(
                relative_path,
                opendal::options::ReadOptions {
                    range: (offset as u64..end as u64).into(),
                    ..Default::default()
                },
            )?
            .to_vec())
    }

    fn delete_physical(&self, relative_path: &str) -> Result<()> {
        match self.operator.delete(relative_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == opendal::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn exists_physical(&self, relative_path: &str) -> bool {
        self.operator.exists(relative_path).unwrap_or(false)
    }

    fn stat_physical(&self, relative_path: &str) -> Result<ObjectStat> {
        let metadata = self.operator.stat(relative_path)?;
        Ok(ObjectStat {
            length: metadata.content_length(),
            modified_at: metadata.last_modified().map(Into::into),
        })
    }

    fn list_physical(&self, prefix: &str) -> Result<Vec<String>> {
        let mut listed = self
            .operator
            .list(prefix)?
            .into_iter()
            .filter_map(|entry: opendal::Entry| {
                let path = entry.path().to_string();
                if path == prefix || path.ends_with('/') {
                    None
                } else {
                    Some(path)
                }
            })
            .collect::<Vec<_>>();
        listed.sort();
        Ok(listed)
    }
}

impl RefStore for OpendalS3Backend {
    fn read_ref(&self, token: &RefToken) -> Result<Option<StoredRef>> {
        token.validate()?;
        let path = OpendalMemoryBackend::ref_path(token);
        match self.get_physical(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .context("failed to decode remote branch ref")
                .map(Some),
            Err(error) if is_missing_physical_object_error(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn list_refs(&self) -> Result<Vec<ListedRef>> {
        let mut listed = self
            .operator
            .list_options(
                "control/refs/by-token/",
                opendal::options::ListOptions {
                    recursive: true,
                    ..Default::default()
                },
            )?
            .into_iter()
            .filter_map(|entry: opendal::Entry| {
                let path = entry.path().to_string();
                let token = path
                    .strip_prefix("control/refs/by-token/")?
                    .strip_suffix(".json")?
                    .to_string();
                Some((path, token))
            })
            .map(|(path, token)| -> Result<ListedRef> {
                let token = RefToken::new(token);
                token.validate()?;
                Ok(ListedRef {
                    token,
                    stored: serde_json::from_slice(&self.get_physical(&path)?)
                        .context("failed to decode remote branch ref")?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        listed.sort_by(|left, right| left.token.value.cmp(&right.token.value));
        Ok(listed)
    }

    fn compare_and_swap_ref(
        &self,
        token: &RefToken,
        expected: Option<RefVersion>,
        next: EncryptedRef,
    ) -> Result<CasResult> {
        token.validate()?;
        let current = self.read_ref(token)?;
        let matches = match (&current, expected) {
            (None, None) => true,
            (Some(stored), Some(expected_version)) => stored.version == expected_version,
            _ => false,
        };
        if !matches {
            return Ok(CasResult {
                applied: false,
                current,
            });
        }

        let stored = StoredRef {
            version: RefVersion {
                value: current
                    .as_ref()
                    .map(|existing| existing.version.value + 1)
                    .unwrap_or(1),
            },
            value: next,
        };
        self.put_physical(
            &OpendalMemoryBackend::ref_path(token),
            &serde_json::to_vec(&stored)?,
        )?;
        Ok(CasResult {
            applied: true,
            current: Some(stored),
        })
    }
}

impl LayoutRootStore for OpendalS3Backend {
    fn read_layout_root(&self) -> Result<LayoutRoot> {
        match self.get_physical("layout_root.json") {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).context("failed to decode remote layout root")
            }
            Err(error) if is_missing_physical_object_error(&error) => {
                Ok(OpendalMemoryBackend::default_layout_root())
            }
            Err(error) => Err(error),
        }
    }

    fn compare_and_swap_layout_root(
        &self,
        expected: LayoutRootVersion,
        next: LayoutRoot,
    ) -> Result<CasResult> {
        let current = self.read_layout_root()?;
        if current.generation != expected {
            return Ok(CasResult {
                applied: false,
                current: None,
            });
        }

        let bytes = serde_json::to_vec(&next)?;
        self.put_physical("layout_root.json", &bytes)?;
        self.put_physical(
            &OpendalMemoryBackend::layout_history_path(next.generation),
            &bytes,
        )?;
        Ok(CasResult {
            applied: true,
            current: None,
        })
    }

    fn list_retained_layout_roots(&self) -> Result<Vec<LayoutRoot>> {
        let retained_paths = self.list_physical("control/layout-roots/")?;
        if retained_paths.is_empty() {
            return Ok(vec![self.read_layout_root()?]);
        }

        retained_paths
            .into_iter()
            .map(|path| serde_json::from_slice(&self.get_physical(&path)?).map_err(Into::into))
            .collect()
    }
}

impl RemoteBackend for OpendalS3Backend {
    fn capability(&self) -> &BackendCapability {
        &self.capability
    }
}

impl RemoteBackend for OpendalWebdavBackend {
    fn capability(&self) -> &BackendCapability {
        &self.capability
    }
}

impl RemoteBackend for crate::memory_backend::MemoryBackend {
    fn capability(&self) -> &BackendCapability {
        self.capability()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::ops::Range;
    use std::sync::{Arc, Mutex};
    use std::thread;

    use opendal::raw::oio;
    use opendal::raw::{Access, AccessorInfo, OpList, OpRead, OpStat, RpList, RpRead, RpStat};
    use opendal::{Buffer, Capability, EntryMode, Error, ErrorKind, Metadata, Operator};

    use super::*;
    use crate::WriterMode;

    #[test]
    fn s3_compatible_backend_defaults_to_unknown_or_eventual_consistency() {
        let backend = S3CompatibleMockBackend::new();

        assert_eq!(
            backend.capability().consistency_class,
            ConsistencyClass::UnknownOrEventual
        );
    }

    #[test]
    fn s3_compatible_backend_negative_ref_cas_fails() {
        let backend = S3CompatibleMockBackend::new();
        let token = RefToken::new("branch-token".to_string());
        let first = EncryptedRef::new(vec![1, 2, 3]);
        let second = EncryptedRef::new(vec![4, 5, 6]);

        let initial = backend.compare_and_swap_ref(&token, None, first).unwrap();
        assert!(initial.applied);

        let stale = backend
            .compare_and_swap_ref(&token, Some(RefVersion { value: 99 }), second)
            .unwrap();
        assert!(!stale.applied);
    }

    #[test]
    fn s3_compatible_backend_lists_prefixed_objects() {
        let backend = S3CompatibleMockBackend::new();
        backend.put_physical("objects/a.bin", b"a").unwrap();
        backend.put_physical("objects/b.bin", b"b").unwrap();
        backend.put_physical("other/c.bin", b"c").unwrap();

        let listed = backend.list_physical("objects/").unwrap();

        assert_eq!(
            listed,
            vec!["objects/a.bin".to_string(), "objects/b.bin".to_string()]
        );
    }

    #[test]
    fn opendal_memory_backend_supports_blob_round_trip() {
        let backend = OpendalMemoryBackend::new().unwrap();
        backend.put_physical("objects/a.bin", b"abcdef").unwrap();

        assert!(backend.exists_physical("objects/a.bin"));
        assert_eq!(backend.get_physical("objects/a.bin").unwrap(), b"abcdef");
        assert_eq!(
            backend.get_physical_range("objects/a.bin", 2, 3).unwrap(),
            b"cde"
        );
        assert_eq!(
            backend.list_physical("objects/").unwrap(),
            vec!["objects/a.bin".to_string()]
        );
        assert_eq!(backend.stat_physical("objects/a.bin").unwrap().length, 6);
    }

    #[test]
    fn opendal_memory_backend_deletes_blob() {
        let backend = OpendalMemoryBackend::new().unwrap();
        backend.put_physical("objects/a.bin", b"abcdef").unwrap();

        backend.delete_physical("objects/a.bin").unwrap();

        assert!(!backend.exists_physical("objects/a.bin"));
    }

    fn shared_memory_operator() -> Result<opendal::blocking::Operator> {
        let _guard = opendal_runtime()?.enter();
        opendal::blocking::Operator::new(
            opendal::Operator::new(opendal::services::Memory::default())
                .unwrap()
                .finish(),
        )
        .map_err(Into::into)
    }

    #[test]
    fn independent_opendal_backends_share_ref_and_layout_state() {
        let operator = shared_memory_operator().unwrap();
        let writer = OpendalMemoryBackend::from_operator(operator.clone());
        let reader = OpendalMemoryBackend::from_operator(operator);
        let token = RefToken::new("branch-token".to_string());
        let next_ref = EncryptedRef::new(vec![1, 2, 3]);

        let ref_result = writer
            .compare_and_swap_ref(&token, None, next_ref.clone())
            .unwrap();
        assert!(ref_result.applied);
        assert_eq!(reader.read_ref(&token).unwrap().unwrap().value, next_ref);

        let next_layout = LayoutRoot {
            generation: 2,
            ..LayoutRoot::direct_default()
        };
        let layout_result = writer
            .compare_and_swap_layout_root(1, next_layout.clone())
            .unwrap();
        assert!(layout_result.applied);
        assert_eq!(reader.read_layout_root().unwrap(), next_layout);
        assert_eq!(
            reader
                .list_retained_layout_roots()
                .unwrap()
                .last()
                .cloned()
                .unwrap(),
            next_layout
        );
    }

    #[test]
    fn opendal_memory_backend_rejects_path_traversal_ref_token() {
        let backend = OpendalMemoryBackend::new().unwrap();
        let error = backend
            .compare_and_swap_ref(
                &RefToken::new("../evil".to_string()),
                None,
                EncryptedRef::new(vec![1, 2, 3]),
            )
            .unwrap_err();

        assert!(error.to_string().contains("path"));
        assert!(
            backend
                .read_ref(&RefToken::new("../evil".to_string()))
                .unwrap_err()
                .to_string()
                .contains("path")
        );
    }

    #[test]
    fn opendal_memory_backend_reports_corrupt_layout_root_bytes() {
        let backend = OpendalMemoryBackend::new().unwrap();
        backend
            .put_physical("layout_root.json", br#"{"invalid":true"#)
            .unwrap();

        let error = backend.read_layout_root().unwrap_err();

        assert!(
            error
                .to_string()
                .contains("failed to decode remote layout root"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn webdav_and_alist_backends_default_to_conservative_single_writer_capabilities() {
        for backend in [
            WebdavAlistMockBackend::webdav(),
            WebdavAlistMockBackend::alist(),
        ] {
            assert_eq!(
                backend.capability().consistency_class,
                ConsistencyClass::UnknownOrEventual
            );
            assert!(!backend.capability().supports_conditional_put);
            assert!(backend.capability().supports_remote_lock_or_lease);
            assert_eq!(backend.capability().writer_mode(), WriterMode::SingleWriter);
            assert_eq!(backend.capability().push_write_mode(), WriterMode::ReadOnly);
            assert!(!backend.capability().supports_safe_single_writer_push());
        }
    }

    #[test]
    fn verified_webdav_backend_still_requires_atomic_lease_support_for_safe_push() {
        let backend = WebdavAlistMockBackend::verified_single_writer(WebdavFlavor::Webdav);

        assert_eq!(backend.capability().writer_mode(), WriterMode::SingleWriter);
        assert_eq!(backend.capability().push_write_mode(), WriterMode::ReadOnly);
        assert!(!backend.capability().supports_safe_single_writer_push());
    }

    #[test]
    fn opendal_webdav_backend_defaults_to_conservative_capabilities() {
        let backend = OpendalWebdavBackend::new(WebdavRemoteConfig {
            flavor: WebdavFlavor::Webdav,
            endpoint: "https://example.com/dav".to_string(),
            root: "/repo".to_string(),
            username: Some("alice".to_string()),
            password: Some("secret".to_string()),
            token: None,
            disable_create_dir: false,
            verified_capabilities: WebdavVerifiedCapabilities::default(),
        })
        .unwrap();

        assert_eq!(backend.capability().writer_mode(), WriterMode::SingleWriter);
        assert_eq!(backend.capability().push_write_mode(), WriterMode::ReadOnly);
        assert!(!backend.capability().supports_safe_single_writer_push());
    }

    #[test]
    fn opendal_alist_backend_still_requires_atomic_lease_support_for_safe_push() {
        let backend = OpendalWebdavBackend::new(WebdavRemoteConfig {
            flavor: WebdavFlavor::Alist,
            endpoint: "https://example.com/alist/dav".to_string(),
            root: "/repo".to_string(),
            username: None,
            password: None,
            token: Some("bearer-token".to_string()),
            disable_create_dir: true,
            verified_capabilities: WebdavVerifiedCapabilities {
                supports_reliable_remote_time: true,
                supports_object_generation_or_etag: true,
            },
        })
        .unwrap();

        assert_eq!(backend.capability().writer_mode(), WriterMode::SingleWriter);
        assert_eq!(backend.capability().push_write_mode(), WriterMode::ReadOnly);
        assert!(!backend.capability().supports_safe_single_writer_push());
    }

    #[derive(Debug, Clone)]
    struct RangeRecordingService {
        content: Arc<Vec<u8>>,
        observed_ranges: Arc<Mutex<Vec<Range<u64>>>>,
        info: Arc<AccessorInfo>,
    }

    impl RangeRecordingService {
        fn new(content: Vec<u8>) -> Self {
            let info = AccessorInfo::default();
            info.set_scheme("memory");
            info.set_native_capability(Capability {
                read: true,
                stat: true,
                ..Default::default()
            });
            Self {
                content: Arc::new(content),
                observed_ranges: Arc::new(Mutex::new(Vec::new())),
                info: Arc::new(info),
            }
        }

        fn observed_ranges(&self) -> Vec<Range<u64>> {
            self.observed_ranges.lock().unwrap().clone()
        }
    }

    #[derive(Debug)]
    struct StaticLister {
        entries: VecDeque<oio::Entry>,
    }

    impl oio::List for StaticLister {
        async fn next(&mut self) -> opendal::Result<Option<oio::Entry>> {
            Ok(self.entries.pop_front())
        }
    }

    #[derive(Debug, Clone)]
    struct DepthOneOnlyListService {
        directories: Arc<HashMap<String, Vec<(String, EntryMode)>>>,
        files: Arc<HashMap<String, Vec<u8>>>,
        info: Arc<AccessorInfo>,
    }

    impl DepthOneOnlyListService {
        fn with_layout(
            directories: HashMap<String, Vec<(String, EntryMode)>>,
            files: HashMap<String, Vec<u8>>,
        ) -> Self {
            let info = AccessorInfo::default();
            info.set_scheme("memory");
            info.set_native_capability(Capability {
                read: true,
                list: true,
                ..Default::default()
            });
            Self {
                directories: Arc::new(directories),
                files: Arc::new(files),
                info: Arc::new(info),
            }
        }
    }

    impl Access for DepthOneOnlyListService {
        type Reader = oio::Reader;
        type Writer = oio::Writer;
        type Lister = oio::Lister;
        type Deleter = oio::Deleter;
        type Copier = oio::Copier;

        fn info(&self) -> Arc<AccessorInfo> {
            self.info.clone()
        }

        async fn read(&self, path: &str, _: OpRead) -> opendal::Result<(RpRead, Self::Reader)> {
            let content = self
                .files
                .get(path)
                .cloned()
                .ok_or_else(|| Error::new(ErrorKind::NotFound, "missing file"))?;
            let metadata = Metadata::new(EntryMode::FILE).with_content_length(content.len() as u64);
            Ok((RpRead::new(metadata), Box::new(Buffer::from(content))))
        }

        async fn list(&self, path: &str, args: OpList) -> opendal::Result<(RpList, Self::Lister)> {
            if args.recursive() {
                return Err(Error::new(
                    ErrorKind::Unexpected,
                    "recursive Depth: infinity listing unsupported",
                ));
            }

            let entries = self
                .directories
                .get(path)
                .cloned()
                .ok_or_else(|| Error::new(ErrorKind::NotFound, "missing directory"))?
                .into_iter()
                .map(|(path, mode)| oio::Entry::new(&path, Metadata::new(mode)))
                .collect::<VecDeque<_>>();
            Ok((RpList::default(), Box::new(StaticLister { entries })))
        }
    }

    impl Access for RangeRecordingService {
        type Reader = oio::Reader;
        type Writer = oio::Writer;
        type Lister = oio::Lister;
        type Deleter = oio::Deleter;
        type Copier = oio::Copier;

        fn info(&self) -> Arc<AccessorInfo> {
            self.info.clone()
        }

        async fn stat(&self, _: &str, _: OpStat) -> opendal::Result<RpStat> {
            Ok(RpStat::new(
                Metadata::new(EntryMode::FILE).with_content_length(self.content.len() as u64),
            ))
        }

        async fn read(&self, _: &str, args: OpRead) -> opendal::Result<(RpRead, Self::Reader)> {
            let requested = args.range();
            let start = requested.offset().min(self.content.len() as u64);
            let size = requested
                .size()
                .unwrap_or(self.content.len() as u64 - start)
                .min(self.content.len() as u64 - start);
            let end = start.saturating_add(size);
            self.observed_ranges.lock().unwrap().push(start..end);

            let slice = self.content[start as usize..end as usize].to_vec();
            let metadata =
                Metadata::new(EntryMode::FILE).with_content_length(self.content.len() as u64);
            Ok((RpRead::new(metadata), Box::new(Buffer::from(slice))))
        }
    }

    #[test]
    fn opendal_memory_backend_range_reads_use_backend_ranges_instead_of_full_object_reads() {
        let service = RangeRecordingService::new((0u8..=15).collect());
        let _guard = opendal_runtime().unwrap().enter();
        let operator =
            opendal::blocking::Operator::new(Operator::from_inner(Arc::new(service.clone())))
                .unwrap();
        let backend = OpendalMemoryBackend::from_operator(operator);

        let bytes = backend
            .get_physical_range("objects/demo.bin", 4, 5)
            .unwrap();

        assert_eq!(bytes, vec![4, 5, 6, 7, 8]);
        assert_eq!(service.observed_ranges(), vec![4..9]);
    }

    #[test]
    fn opendal_memory_backend_lists_refs_with_nested_tokens() {
        let backend = OpendalMemoryBackend::new().unwrap();
        let token = RefToken::new("keyring/repo-123".to_string());
        let value = EncryptedRef::new(br#"{"generation":1,"current":"keyring.1"}"#.to_vec());

        let cas = backend
            .compare_and_swap_ref(&token, None, value.clone())
            .unwrap();

        assert!(cas.applied);
        assert_eq!(backend.read_ref(&token).unwrap().unwrap().value, value);
        assert!(
            backend
                .list_refs()
                .unwrap()
                .iter()
                .any(|listed| listed.token == token),
            "nested ref token should be discoverable via list_refs"
        );
    }

    #[test]
    fn opendal_webdav_backend_lists_nested_refs_without_recursive_depth_infinity() {
        let token = RefToken::new("keyring/repo-123".to_string());
        let stored = StoredRef {
            version: RefVersion { value: 1 },
            value: EncryptedRef::new(br#"{"generation":1,"current":"keyring.1"}"#.to_vec()),
        };
        let mut directories = HashMap::new();
        directories.insert(
            "control/refs/by-token/".to_string(),
            vec![("control/refs/by-token/keyring/".to_string(), EntryMode::DIR)],
        );
        directories.insert(
            "control/refs/by-token/keyring/".to_string(),
            vec![(
                "control/refs/by-token/keyring/repo-123.json".to_string(),
                EntryMode::FILE,
            )],
        );
        let mut files = HashMap::new();
        files.insert(
            "control/refs/by-token/keyring/repo-123.json".to_string(),
            serde_json::to_vec(&stored).unwrap(),
        );
        let service = DepthOneOnlyListService::with_layout(directories, files);
        let _guard = opendal_runtime().unwrap().enter();
        let operator =
            opendal::blocking::Operator::new(Operator::from_inner(Arc::new(service))).unwrap();
        let backend = OpendalWebdavBackend::from_operator(operator, WebdavFlavor::Webdav);

        let listed = backend.list_refs().unwrap();

        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].token, token);
        assert_eq!(listed[0].stored, stored);
    }

    #[test]
    fn opendal_webdav_backend_treats_missing_ref_directory_as_empty() {
        let service = DepthOneOnlyListService::with_layout(HashMap::new(), HashMap::new());
        let _guard = opendal_runtime().unwrap().enter();
        let operator =
            opendal::blocking::Operator::new(Operator::from_inner(Arc::new(service))).unwrap();
        let backend = OpendalWebdavBackend::from_operator(operator, WebdavFlavor::Webdav);

        let listed = backend.list_refs().unwrap();

        assert!(listed.is_empty());
    }

    #[test]
    fn webdav_propfind_parser_extracts_relative_entries_from_depth_one_listing() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<D:multistatus xmlns:D="DAV:">
  <D:response>
    <D:href>/dav/repo/control/refs/by-token/</D:href>
    <D:propstat>
      <D:prop>
        <D:resourcetype><D:collection/></D:resourcetype>
      </D:prop>
    </D:propstat>
  </D:response>
  <D:response>
    <D:href>/dav/repo/control/refs/by-token/keyring/</D:href>
    <D:propstat>
      <D:prop>
        <D:resourcetype><D:collection/></D:resourcetype>
      </D:prop>
    </D:propstat>
  </D:response>
  <D:response>
    <D:href>/dav/repo/control/refs/by-token/default.json</D:href>
    <D:propstat>
      <D:prop>
        <D:resourcetype/>
      </D:prop>
    </D:propstat>
  </D:response>
</D:multistatus>"#;

        let entries = WebdavCompatLister::parse_propfind_entries(
            "/dav/repo",
            "/dav/repo/control/refs/by-token/",
            xml,
        )
        .unwrap();

        assert_eq!(
            entries,
            vec![
                WebdavListEntry::dir("control/refs/by-token/keyring/"),
                WebdavListEntry::file("control/refs/by-token/default.json"),
            ]
        );
    }

    #[test]
    fn webdav_compat_lister_sends_bodyless_depth_one_propfind_with_basic_auth() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut buf = [0_u8; 1024];
            loop {
                let read = stream.read(&mut buf).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            stream
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            String::from_utf8(request).unwrap()
        });

        let lister = WebdavCompatLister::new(&WebdavRemoteConfig {
            flavor: WebdavFlavor::Webdav,
            endpoint: format!("http://{addr}"),
            root: "/repo".to_string(),
            username: Some("alice".to_string()),
            password: Some("secret".to_string()),
            token: None,
            disable_create_dir: false,
            verified_capabilities: WebdavVerifiedCapabilities::default(),
        })
        .unwrap();

        let entries = lister.list_one_level("control/refs/by-token/").unwrap();
        let request = server.join().unwrap();
        let request_lower = request.to_ascii_lowercase();

        assert!(entries.is_empty());
        assert!(request.starts_with("PROPFIND /repo/control/refs/by-token/ HTTP/1.1\r\n"));
        assert!(request_lower.contains("\r\ndepth: 1\r\n"));
        assert!(request_lower.contains("\r\nauthorization: basic ywxpy2u6c2vjcmv0\r\n"));
        assert!(request_lower.contains("\r\ncontent-length: 0\r\n"));
        assert!(!request.contains("<?xml"));
        assert!(!request.contains("Content-Type:"));
    }

    #[test]
    fn opendal_memory_backend_rejects_invalid_physical_ref_tokens() {
        let backend = OpendalMemoryBackend::new().unwrap();
        let stored = StoredRef {
            version: RefVersion { value: 1 },
            value: EncryptedRef::new(vec![1, 2, 3]),
        };
        backend
            .put_physical(
                "control/refs/by-token/../evil.json",
                &serde_json::to_vec(&stored).unwrap(),
            )
            .unwrap();

        let error = backend.list_refs().unwrap_err();

        assert!(
            error.to_string().contains("ref token") || error.to_string().contains("path"),
            "unexpected error: {error:#}"
        );
    }
}
