use std::sync::LazyLock;

use anyhow::Result;

use crate::{
    BackendCapability, BlobStore, CasResult, ConsistencyClass, EncryptedRef, LayoutRoot,
    LayoutRootStore, LayoutRootVersion, ListedRef, ObjectStat, RefStore, RefToken, RefVersion,
    StoredRef,
};

pub trait RemoteBackend: BlobStore + RefStore + LayoutRootStore + Send + Sync {
    fn capability(&self) -> &BackendCapability;
}

static OPENDAL_RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build opendal runtime")
});

#[derive(Clone)]
pub struct OpendalMemoryBackend {
    operator: opendal::blocking::Operator,
    capability: BackendCapability,
}

impl OpendalMemoryBackend {
    pub fn new() -> Result<Self> {
        let _guard = OPENDAL_RUNTIME.enter();
        Ok(Self::from_operator(opendal::blocking::Operator::new(
            opendal::Operator::new(opendal::services::Memory::default())?.finish(),
        )?))
    }

    fn from_operator(operator: opendal::blocking::Operator) -> Self {
        Self {
            operator,
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
        LayoutRoot {
            schema_version: 1,
            layout_id: "direct".to_string(),
            generation: 1,
            mapping_policy: "loose".to_string(),
        }
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
    operator: opendal::blocking::Operator,
    capability: BackendCapability,
    flavor: WebdavFlavor,
}

#[derive(Clone)]
pub struct OpendalS3Backend {
    operator: opendal::blocking::Operator,
    capability: BackendCapability,
}

impl OpendalS3Backend {
    pub fn new(config: S3RemoteConfig) -> Result<Self> {
        anyhow::ensure!(!config.endpoint.trim().is_empty(), "s3 endpoint must not be empty");
        anyhow::ensure!(!config.bucket.trim().is_empty(), "s3 bucket must not be empty");
        anyhow::ensure!(!config.root.trim().is_empty(), "s3 root must not be empty");

        let _guard = OPENDAL_RUNTIME.enter();
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
            operator,
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
        anyhow::ensure!(
            !config.endpoint.trim().is_empty(),
            "webdav endpoint must not be empty"
        );
        anyhow::ensure!(
            !config.root.trim().is_empty(),
            "webdav root must not be empty"
        );

        let _guard = OPENDAL_RUNTIME.enter();
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
            operator,
            capability: webdav_capability(&config.verified_capabilities),
            flavor: config.flavor,
        })
    }

    pub fn flavor(&self) -> WebdavFlavor {
        self.flavor
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
        if self.exists_physical(relative_path) {
            self.operator.delete(relative_path)?;
        }
        Ok(())
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
        if self.exists_physical(relative_path) {
            self.operator.delete(relative_path)?;
        }
        Ok(())
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
        if !self.exists_physical(&path) {
            return Ok(None);
        }
        Ok(Some(serde_json::from_slice(&self.get_physical(&path)?)?))
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
                Ok(ListedRef {
                    token: RefToken::new(token),
                    stored: serde_json::from_slice(&self.get_physical(&path)?)?,
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
        self.put_physical(&Self::ref_path(token), &serde_json::to_vec_pretty(&stored)?)?;
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
        if !self.exists_physical(&path) {
            return Ok(None);
        }
        Ok(Some(serde_json::from_slice(&self.get_physical(&path)?)?))
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
                Ok(ListedRef {
                    token: RefToken::new(token),
                    stored: serde_json::from_slice(&self.get_physical(&path)?)?,
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
            &serde_json::to_vec_pretty(&stored)?,
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
        if !self.exists_physical("layout_root.json") {
            return Ok(Self::default_layout_root());
        }
        Ok(serde_json::from_slice(
            &self.get_physical("layout_root.json")?,
        )?)
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

        let bytes = serde_json::to_vec_pretty(&next)?;
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
        if !self.exists_physical("layout_root.json") {
            return Ok(OpendalMemoryBackend::default_layout_root());
        }
        Ok(serde_json::from_slice(
            &self.get_physical("layout_root.json")?,
        )?)
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

        let bytes = serde_json::to_vec_pretty(&next)?;
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
        if self.exists_physical(relative_path) {
            self.operator.delete(relative_path)?;
        }
        Ok(())
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
        if !self.exists_physical(&path) {
            return Ok(None);
        }
        Ok(Some(serde_json::from_slice(&self.get_physical(&path)?)?))
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
                Ok(ListedRef {
                    token: RefToken::new(token),
                    stored: serde_json::from_slice(&self.get_physical(&path)?)?,
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
            &serde_json::to_vec_pretty(&stored)?,
        )?;
        Ok(CasResult {
            applied: true,
            current: Some(stored),
        })
    }
}

impl LayoutRootStore for OpendalS3Backend {
    fn read_layout_root(&self) -> Result<LayoutRoot> {
        if !self.exists_physical("layout_root.json") {
            return Ok(OpendalMemoryBackend::default_layout_root());
        }
        Ok(serde_json::from_slice(
            &self.get_physical("layout_root.json")?,
        )?)
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

        let bytes = serde_json::to_vec_pretty(&next)?;
        self.put_physical("layout_root.json", &bytes)?;
        self.put_physical(&OpendalMemoryBackend::layout_history_path(next.generation), &bytes)?;
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
    use std::ops::Range;
    use std::sync::{Arc, Mutex};

    use opendal::raw::oio;
    use opendal::raw::{Access, AccessorInfo, OpRead, OpStat, RpRead, RpStat};
    use opendal::{Buffer, Capability, EntryMode, Metadata, Operator};

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

    fn shared_memory_operator() -> opendal::blocking::Operator {
        let _guard = OPENDAL_RUNTIME.enter();
        opendal::blocking::Operator::new(
            opendal::Operator::new(opendal::services::Memory::default())
                .unwrap()
                .finish(),
        )
        .unwrap()
    }

    #[test]
    fn independent_opendal_backends_share_ref_and_layout_state() {
        let operator = shared_memory_operator();
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
            schema_version: 1,
            layout_id: "direct".to_string(),
            generation: 2,
            mapping_policy: "loose".to_string(),
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
        let _guard = OPENDAL_RUNTIME.enter();
        let operator = opendal::blocking::Operator::new(
            Operator::from_inner(Arc::new(service.clone())),
        )
        .unwrap();
        let backend = OpendalMemoryBackend::from_operator(operator);

        let bytes = backend.get_physical_range("objects/demo.bin", 4, 5).unwrap();

        assert_eq!(bytes, vec![4, 5, 6, 7, 8]);
        assert_eq!(service.observed_ranges(), vec![4..9]);
    }

    #[test]
    fn opendal_memory_backend_lists_refs_with_nested_tokens() {
        let backend = OpendalMemoryBackend::new().unwrap();
        let token = RefToken::new("keyring/repo-123".to_string());
        let value = EncryptedRef::new(br#"{"generation":1,"current":"keyring.1"}"#.to_vec());

        let cas = backend.compare_and_swap_ref(&token, None, value.clone()).unwrap();

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
}
