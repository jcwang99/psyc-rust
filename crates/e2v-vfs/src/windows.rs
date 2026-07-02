use std::collections::BTreeSet;
use std::ffi::{CString, c_void};
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use e2v_core::DirectoryEntry;

use crate::{
    CachePolicy, MountLaunchSummary, MountMode, MountRequest, OpenedFile, ReadOnlyVfs,
    RefreshOutcome, VfsMountConfig, VfsNodeMetadata,
};

const DEFAULT_SECTOR_SIZE: u32 = 4096;
const DEFAULT_TOTAL_BYTES: u64 = 1 << 40;
const WINDOWS_ADAPTER_STATUS: &str =
    "winfsp adapter boundary ready; windows adapter not implemented yet";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsMountLauncher {
    request: MountRequest,
}

pub trait WinfspHostLauncher {
    fn launch(
        &mut self,
        launcher: &WindowsMountLauncher,
        context: WinfspMountContext,
    ) -> Result<()>;
}

impl WindowsMountLauncher {
    pub fn from_request(request: &MountRequest) -> Self {
        Self {
            request: request.clone(),
        }
    }

    pub fn mount_mode_label(&self) -> &'static str {
        self.request.mount_mode_label()
    }

    pub fn mount_point(&self) -> &PathBuf {
        self.request.mount_point()
    }

    pub fn host_config(&self, context: &WinfspMountContext) -> WinfspHostConfig {
        WinfspHostConfig::from_context(context)
    }

    pub fn launch_with_host(
        &self,
        context: WinfspMountContext,
        host: &mut impl WinfspHostLauncher,
    ) -> Result<()> {
        host.launch(self, context)
    }

    pub fn launch_with_host_and_describe(
        &self,
        context: WinfspMountContext,
        host: &mut impl WinfspHostLauncher,
    ) -> Result<MountLaunchSummary> {
        let cache_policy = context.cache_policy();
        host.launch(self, context)?;
        Ok(MountLaunchSummary {
            mount_mode: self.mount_mode_label().to_string(),
            mount_point: self.mount_point().clone(),
            cache_policy,
            read_only: true,
            stream_only: true,
            status_message: WINDOWS_ADAPTER_STATUS.to_string(),
        })
    }
}

pub(crate) fn mount_snapshot(
    config: VfsMountConfig,
    mount_point: PathBuf,
) -> Result<MountLaunchSummary> {
    mount_request(MountRequest::from_config(config, mount_point))
}

pub(crate) fn mount_live_branch(
    config: VfsMountConfig,
    mount_point: PathBuf,
) -> Result<MountLaunchSummary> {
    mount_request(MountRequest::from_config(config, mount_point))
}

fn mount_request(request: MountRequest) -> Result<MountLaunchSummary> {
    let launcher = WindowsMountLauncher::from_request(&request);
    let context = WinfspMountContext::from_request(request);
    let mut host = SummaryOnlyHostLauncher;
    launcher.launch_with_host_and_describe(context, &mut host)
}

struct SummaryOnlyHostLauncher;

impl WinfspHostLauncher for SummaryOnlyHostLauncher {
    fn launch(
        &mut self,
        _launcher: &WindowsMountLauncher,
        _context: WinfspMountContext,
    ) -> Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadOnlyVolumeSummary {
    pub volume_label: String,
    pub filesystem_name: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub sector_size: u32,
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WinfspHostConfig {
    pub mount_point: PathBuf,
    pub volume_label: String,
    pub filesystem_name: String,
    pub sector_size: u32,
    pub read_only: bool,
    pub enable_kernel_file_info_cache_bypass: bool,
}

impl WinfspHostConfig {
    pub fn from_context(context: &WinfspMountContext) -> Self {
        let volume = context.volume_summary();
        Self {
            mount_point: context.mount_point().clone(),
            volume_label: volume.volume_label,
            filesystem_name: volume.filesystem_name,
            sector_size: volume.sector_size,
            read_only: volume.read_only,
            enable_kernel_file_info_cache_bypass: context.cache_policy()
                == CachePolicy::DirectIoFallback,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WinfspVolumeParams {
    pub sector_size: u32,
    pub sectors_per_allocation_unit: u16,
    pub file_info_timeout_ms: u32,
    pub volume_info_timeout_ms: u32,
    pub dir_info_timeout_ms: u32,
    pub case_sensitive_search: bool,
    pub case_preserved_names: bool,
    pub unicode_on_disk: bool,
    pub read_only_volume: bool,
    pub um_file_context_is_full_context: bool,
    pub prefix: String,
    pub filesystem_name: String,
}

impl WinfspVolumeParams {
    pub fn from_host_config(config: &WinfspHostConfig) -> Self {
        let cache_timeout_ms = if config.enable_kernel_file_info_cache_bypass {
            0
        } else {
            1_000
        };
        Self {
            sector_size: config.sector_size,
            sectors_per_allocation_unit: 1,
            file_info_timeout_ms: cache_timeout_ms,
            volume_info_timeout_ms: cache_timeout_ms,
            dir_info_timeout_ms: cache_timeout_ms,
            case_sensitive_search: true,
            case_preserved_names: true,
            unicode_on_disk: true,
            read_only_volume: config.read_only,
            um_file_context_is_full_context: true,
            prefix: String::new(),
            filesystem_name: config.filesystem_name.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WinfspRuntimePaths {
    pub install_root: PathBuf,
    pub bin_dir: PathBuf,
    pub dll_path: PathBuf,
}

impl WinfspRuntimePaths {
    pub(crate) fn from_install_root_for_test(install_root: PathBuf, arch: &str) -> Result<Self> {
        let dll_name = match arch {
            "x86_64" => "winfsp-x64.dll",
            "x86" => "winfsp-x86.dll",
            "aarch64" => "winfsp-a64.dll",
            other => anyhow::bail!("unsupported WinFSP architecture: {other}"),
        };
        let bin_dir = install_root.join("bin");
        let dll_path = bin_dir.join(dll_name);
        if !dll_path.is_file() {
            anyhow::bail!("WinFSP runtime DLL not found: {}", dll_path.display());
        }
        Ok(Self {
            install_root,
            bin_dir,
            dll_path,
        })
    }

    pub(crate) fn from_candidate_roots_for_test(
        candidate_roots: &[PathBuf],
        arch: &str,
    ) -> Result<Self> {
        for root in candidate_roots {
            if let Ok(paths) = Self::from_install_root_for_test(root.clone(), arch) {
                return Ok(paths);
            }
        }
        anyhow::bail!("no usable WinFSP runtime installation found")
    }
}

#[derive(Debug)]
pub struct WinfspRuntimeLibrary {
    dll_path: PathBuf,
    handle: *mut c_void,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WinfspMountExports {
    pub create: usize,
    pub set_mount_point: usize,
    pub start_dispatcher: usize,
    pub stop_dispatcher: usize,
    pub delete_filesystem: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WinfspNativeCreateVolumeParams {
    pub sector_size: u32,
    pub read_only_volume: bool,
    pub filesystem_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WinfspNativeCreateRequest {
    pub device_path: String,
    pub mount_point: PathBuf,
    pub volume_params: WinfspNativeCreateVolumeParams,
}

#[derive(Debug)]
pub struct WinfspHostSession {
    runtime: WinfspRuntimeLibrary,
    host_config: WinfspHostConfig,
    volume_params: WinfspVolumeParams,
    exports: WinfspMountExports,
    mounted: bool,
    native_filesystem: Option<*mut c_void>,
}

pub trait WinfspHostDriver {
    fn create_filesystem_handle(&self, session: &mut WinfspHostSession) -> Result<()>;
    fn set_mount_point(&self, mount_point: &std::path::Path) -> Result<()>;
    fn start_dispatcher(&self, thread_count: u32) -> Result<()>;
    fn stop_dispatcher(&self) -> Result<()>;
    fn delete_filesystem_handle(&self, session: &mut WinfspHostSession) -> Result<()>;
}

impl WinfspRuntimeLibrary {
    pub fn load(paths: &WinfspRuntimePaths) -> Result<Self> {
        let wide_path = paths
            .dll_path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let handle = unsafe { LoadLibraryW(wide_path.as_ptr()) };
        if handle.is_null() {
            anyhow::bail!(
                "failed to load WinFSP runtime DLL: {}",
                paths.dll_path.display()
            );
        }
        Ok(Self {
            dll_path: paths.dll_path.clone(),
            handle,
        })
    }

    pub fn dll_path(&self) -> &PathBuf {
        &self.dll_path
    }

    pub(crate) fn get_symbol_address_for_test(&self, symbol_name: &str) -> Result<usize> {
        let symbol_name = CString::new(symbol_name)?;
        let address = unsafe { GetProcAddress(self.handle, symbol_name.as_ptr()) };
        if address.is_null() {
            anyhow::bail!("failed to resolve WinFSP export: {symbol_name:?}");
        }
        Ok(address as usize)
    }

    pub(crate) fn resolve_mount_exports_for_test(&self) -> Result<WinfspMountExports> {
        Ok(WinfspMountExports {
            create: self.get_symbol_address_for_test("FspFileSystemCreate")?,
            set_mount_point: self.get_symbol_address_for_test("FspFileSystemSetMountPoint")?,
            start_dispatcher: self.get_symbol_address_for_test("FspFileSystemStartDispatcher")?,
            stop_dispatcher: self.get_symbol_address_for_test("FspFileSystemStopDispatcher")?,
            delete_filesystem: self.get_symbol_address_for_test("FspFileSystemDelete")?,
        })
    }
}

impl WinfspHostSession {
    pub(crate) fn new_for_test(
        runtime: WinfspRuntimeLibrary,
        host_config: WinfspHostConfig,
        volume_params: WinfspVolumeParams,
    ) -> Result<Self> {
        let exports = runtime.resolve_mount_exports_for_test()?;
        Ok(Self {
            runtime,
            host_config,
            volume_params,
            exports,
            mounted: false,
            native_filesystem: None,
        })
    }

    pub fn mount_point(&self) -> &PathBuf {
        &self.host_config.mount_point
    }

    pub fn filesystem_name(&self) -> &str {
        &self.volume_params.filesystem_name
    }

    pub fn volume_label(&self) -> &str {
        &self.host_config.volume_label
    }

    pub fn has_required_mount_exports(&self) -> bool {
        self.exports.create != 0
            && self.exports.set_mount_point != 0
            && self.exports.start_dispatcher != 0
            && self.exports.stop_dispatcher != 0
            && self.exports.delete_filesystem != 0
            && !self.runtime.dll_path().as_os_str().is_empty()
    }

    pub(crate) fn is_mounted_for_test(&self) -> bool {
        self.mounted
    }

    pub(crate) fn build_native_create_request_for_test(&self) -> Result<WinfspNativeCreateRequest> {
        Ok(WinfspNativeCreateRequest {
            device_path: "WinFsp.Disk".to_string(),
            mount_point: self.host_config.mount_point.clone(),
            volume_params: WinfspNativeCreateVolumeParams {
                sector_size: self.volume_params.sector_size,
                read_only_volume: self.volume_params.read_only_volume,
                filesystem_name: self.volume_params.filesystem_name.clone(),
            },
        })
    }

    pub(crate) fn create_filesystem_handle_for_test(&mut self) -> Result<()> {
        if self.native_filesystem.is_some() {
            return Ok(());
        }

        let mut native_volume_params =
            NativeWinfspVolumeParams::from_volume_params(&self.volume_params);
        let interface = NativeWinfspInterface::read_only_stub();
        let device_path = widestr_from_str("WinFsp.Disk");
        let mut filesystem = std::ptr::null_mut();
        let create = self.exports.create_fn();
        let status = unsafe {
            create(
                device_path.as_ptr() as *mut u16,
                native_volume_params.as_raw_mut(),
                interface.as_raw(),
                &mut filesystem,
            )
        };
        if status != 0 {
            anyhow::bail!("FspFileSystemCreate failed with NTSTATUS {status:#x}");
        }
        if filesystem.is_null() {
            anyhow::bail!("FspFileSystemCreate returned a null filesystem handle");
        }
        self.native_filesystem = Some(filesystem.cast());
        Ok(())
    }

    pub(crate) fn has_native_filesystem_handle_for_test(&self) -> bool {
        self.native_filesystem.is_some()
    }

    pub(crate) fn destroy_filesystem_handle_for_test(&mut self) {
        if let Some(filesystem) = self.native_filesystem.take() {
            let delete = self.exports.delete_fn();
            unsafe {
                delete(filesystem.cast());
            }
        }
    }

    pub(crate) fn run_mount_lifecycle_for_test(
        &mut self,
        driver: &impl WinfspHostDriver,
        mount_point: PathBuf,
        thread_count: u32,
    ) -> Result<()> {
        driver.create_filesystem_handle(self)?;
        driver.set_mount_point(&mount_point)?;
        driver.start_dispatcher(thread_count)?;
        self.mounted = true;
        driver.stop_dispatcher()?;
        driver.delete_filesystem_handle(self)?;
        Ok(())
    }
}

impl Drop for WinfspRuntimeLibrary {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                FreeLibrary(self.handle);
            }
        }
    }
}

type FspFileSystemCreateFn = unsafe extern "C" fn(
    device_path: *mut u16,
    volume_params: *const NativeWinfspVolumeParams,
    interface: *const NativeWinfspInterface,
    filesystem: *mut *mut c_void,
) -> i32;

type FspFileSystemDeleteFn = unsafe extern "C" fn(filesystem: *mut c_void);

impl WinfspMountExports {
    fn create_fn(&self) -> FspFileSystemCreateFn {
        unsafe { std::mem::transmute(self.create) }
    }

    fn delete_fn(&self) -> FspFileSystemDeleteFn {
        unsafe { std::mem::transmute(self.delete_filesystem) }
    }
}

unsafe extern "system" {
    fn LoadLibraryW(lp_lib_file_name: *const u16) -> *mut c_void;
    fn GetProcAddress(h_module: *mut c_void, lp_proc_name: *const i8) -> *mut c_void;
    fn FreeLibrary(h_lib_module: *mut c_void) -> i32;
}

#[repr(C)]
struct NativeWinfspVolumeParams {
    version: u16,
    sector_size: u16,
    sectors_per_allocation_unit: u16,
    max_component_length: u16,
    volume_creation_time: u64,
    volume_serial_number: u32,
    transact_timeout: u32,
    irp_timeout: u32,
    irp_capacity: u32,
    file_info_timeout: u32,
    bitfield_1: u32,
    prefix: [u16; 192],
    filesystem_name: [u16; 16],
    bitfield_2: u32,
    volume_info_timeout: u32,
    dir_info_timeout: u32,
    security_timeout: u32,
    stream_info_timeout: u32,
    ea_timeout: u32,
    fsext_control_code: u32,
    reserved32: [u32; 1],
    reserved64: [u64; 2],
}

impl NativeWinfspVolumeParams {
    fn from_volume_params(params: &WinfspVolumeParams) -> Self {
        let mut prefix = [0u16; 192];
        let mut filesystem_name = [0u16; 16];
        copy_wide_z(&params.prefix, &mut prefix);
        copy_wide_z(&params.filesystem_name, &mut filesystem_name);
        let mut bitfield_1 = 0u32;
        if params.case_sensitive_search {
            bitfield_1 |= 1 << 0;
        }
        if params.case_preserved_names {
            bitfield_1 |= 1 << 1;
        }
        if params.unicode_on_disk {
            bitfield_1 |= 1 << 2;
        }
        if params.read_only_volume {
            bitfield_1 |= 1 << 24;
        }
        let mut bitfield_2 = 0u32;
        if params.um_file_context_is_full_context {
            bitfield_2 |= 1 << 13;
        }
        Self {
            version: 0,
            sector_size: params.sector_size as u16,
            sectors_per_allocation_unit: params.sectors_per_allocation_unit,
            max_component_length: 255,
            volume_creation_time: 0,
            volume_serial_number: 0xE2E0,
            transact_timeout: 0,
            irp_timeout: 0,
            irp_capacity: 0,
            file_info_timeout: params.file_info_timeout_ms,
            bitfield_1,
            prefix,
            filesystem_name,
            bitfield_2,
            volume_info_timeout: params.volume_info_timeout_ms,
            dir_info_timeout: params.dir_info_timeout_ms,
            security_timeout: 0,
            stream_info_timeout: 0,
            ea_timeout: 0,
            fsext_control_code: 0,
            reserved32: [0],
            reserved64: [0, 0],
        }
    }

    fn as_raw_mut(&mut self) -> *mut NativeWinfspVolumeParams {
        self
    }
}

#[repr(C)]
#[derive(Default)]
struct NativeWinfspInterface {
    get_volume_info: Option<unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32>,
    set_volume_label_w: Option<unsafe extern "C" fn(*mut c_void, *mut u16, *mut c_void) -> i32>,
    get_security_by_name: Option<
        unsafe extern "C" fn(*mut c_void, *mut u16, *mut u32, *mut c_void, *mut usize) -> i32,
    >,
    create: Option<
        unsafe extern "C" fn(
            *mut c_void,
            *mut u16,
            u32,
            u32,
            u32,
            *mut c_void,
            u64,
            *mut *mut c_void,
            *mut c_void,
        ) -> i32,
    >,
    open: Option<
        unsafe extern "C" fn(*mut c_void, *mut u16, u32, u32, *mut *mut c_void, *mut c_void) -> i32,
    >,
    overwrite:
        Option<unsafe extern "C" fn(*mut c_void, *mut c_void, u32, u8, u64, *mut c_void) -> i32>,
    cleanup: Option<unsafe extern "C" fn(*mut c_void, *mut c_void, *mut u16, u32)>,
    close: Option<unsafe extern "C" fn(*mut c_void, *mut c_void)>,
    read: Option<
        unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, u64, u32, *mut u32) -> i32,
    >,
    write: Option<
        unsafe extern "C" fn(
            *mut c_void,
            *mut c_void,
            *mut c_void,
            u64,
            u32,
            u8,
            u8,
            *mut u32,
            *mut c_void,
        ) -> i32,
    >,
    flush: Option<unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> i32>,
    get_file_info: Option<unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> i32>,
    set_basic_info: Option<
        unsafe extern "C" fn(*mut c_void, *mut c_void, u32, u64, u64, u64, u64, *mut c_void) -> i32,
    >,
    set_file_size:
        Option<unsafe extern "C" fn(*mut c_void, *mut c_void, u64, u8, *mut c_void) -> i32>,
    can_delete: Option<unsafe extern "C" fn(*mut c_void, *mut c_void, *mut u16) -> i32>,
    rename: Option<unsafe extern "C" fn(*mut c_void, *mut c_void, *mut u16, *mut u16, u8) -> i32>,
    get_security:
        Option<unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut usize) -> i32>,
    set_security: Option<unsafe extern "C" fn(*mut c_void, *mut c_void, u32, *mut c_void) -> i32>,
    read_directory: Option<
        unsafe extern "C" fn(
            *mut c_void,
            *mut c_void,
            *mut u16,
            *mut u16,
            *mut c_void,
            u32,
            *mut u32,
        ) -> i32,
    >,
    reserved: [usize; 15],
}

impl NativeWinfspInterface {
    fn read_only_stub() -> Self {
        Self::default()
    }

    fn as_raw(&self) -> *const NativeWinfspInterface {
        self
    }
}

fn widestr_from_str(value: &str) -> Vec<u16> {
    std::ffi::OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn copy_wide_z(source: &str, target: &mut [u16]) {
    let encoded = std::ffi::OsStr::new(source)
        .encode_wide()
        .take(target.len().saturating_sub(1))
        .collect::<Vec<_>>();
    for (index, value) in encoded.into_iter().enumerate() {
        target[index] = value;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WinfspInvalidationPlan {
    pub inode_ids: Vec<u64>,
    pub directory_paths: Vec<String>,
    pub invalidate_directory_entries: bool,
    pub invalidate_attributes: bool,
    pub invalidate_page_cache: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WinfspOpenRequest {
    logical_path: String,
    writable: bool,
}

impl WinfspOpenRequest {
    pub fn read_only(logical_path: impl Into<String>) -> Self {
        Self {
            logical_path: logical_path.into(),
            writable: false,
        }
    }

    pub fn writable(logical_path: impl Into<String>) -> Self {
        Self {
            logical_path: logical_path.into(),
            writable: true,
        }
    }
}

pub trait WinfspInvalidator {
    fn invalidate_directory_entries(&mut self, logical_path: &str) -> Result<()>;
    fn invalidate_inode(&mut self, inode_id: u64) -> Result<()>;
    fn invalidate_attributes(&mut self, inode_id: u64) -> Result<()>;
    fn invalidate_page_cache(&mut self, inode_id: u64) -> Result<()>;
}

#[derive(Debug, Clone)]
pub struct WinfspMountContext {
    request: MountRequest,
    cache_policy: CachePolicy,
    observed_inode_ids: Arc<Mutex<BTreeSet<u64>>>,
    observed_directory_paths: Arc<Mutex<BTreeSet<String>>>,
    vfs: ReadOnlyVfs,
}

impl WinfspMountContext {
    pub fn from_request(request: MountRequest) -> Self {
        let vfs = match &request.config.mode {
            MountMode::SnapshotPinned { .. } => ReadOnlyVfs::mount_snapshot(request.config.clone()),
            MountMode::LiveBranch { .. } => ReadOnlyVfs::mount_live_branch(request.config.clone()),
        }
        .expect("mount request should resolve to a readable VFS context");
        let cache_policy = vfs.cache_policy();
        Self {
            request,
            cache_policy,
            observed_inode_ids: Arc::new(Mutex::new(BTreeSet::new())),
            observed_directory_paths: Arc::new(Mutex::new(BTreeSet::new())),
            vfs,
        }
    }

    pub fn mount_mode_label(&self) -> &'static str {
        self.request.mount_mode_label()
    }

    pub fn mount_point(&self) -> &PathBuf {
        self.request.mount_point()
    }

    pub fn cache_policy(&self) -> CachePolicy {
        self.cache_policy
    }

    pub fn namespace_snapshot_id(&self) -> String {
        self.vfs.namespace_snapshot_id()
    }

    pub fn refresh_namespace(&mut self) -> Result<RefreshOutcome> {
        self.vfs.refresh_live_branch()
    }

    pub fn build_invalidation_plan(
        &self,
        refresh: &RefreshOutcome,
    ) -> Option<WinfspInvalidationPlan> {
        if !refresh.requires_invalidation {
            return None;
        }

        let inode_ids = self
            .observed_inode_ids
            .lock()
            .unwrap()
            .iter()
            .copied()
            .collect::<Vec<_>>();
        let directory_paths = self
            .observed_directory_paths
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        Some(WinfspInvalidationPlan {
            inode_ids,
            directory_paths,
            invalidate_directory_entries: true,
            invalidate_attributes: true,
            invalidate_page_cache: true,
        })
    }

    pub fn apply_invalidation(
        &self,
        refresh: &RefreshOutcome,
        invalidator: &mut impl WinfspInvalidator,
    ) -> Result<bool> {
        let Some(plan) = self.build_invalidation_plan(refresh) else {
            return Ok(false);
        };

        if plan.invalidate_directory_entries {
            for logical_path in &plan.directory_paths {
                invalidator.invalidate_directory_entries(logical_path)?;
            }
        }

        for inode_id in &plan.inode_ids {
            invalidator.invalidate_inode(*inode_id)?;
            if plan.invalidate_attributes {
                invalidator.invalidate_attributes(*inode_id)?;
            }
            if plan.invalidate_page_cache {
                invalidator.invalidate_page_cache(*inode_id)?;
            }
        }

        Ok(true)
    }

    pub fn open_handle(&self, logical_path: &str) -> Result<WinfspOpenHandle> {
        let opened = self.vfs.open_file(logical_path)?;
        let handle = WinfspOpenHandle::from_opened_file(opened);
        self.remember_inode(handle.inode_id());
        Ok(handle)
    }

    pub fn open_handle_for_request(&self, request: &WinfspOpenRequest) -> Result<WinfspOpenHandle> {
        if request.writable {
            self.vfs
                .require_semantic(crate::VfsSemantic::WritableHandles)?;
        }
        self.open_handle(&request.logical_path)
    }

    pub fn read_handle(
        &self,
        handle: &WinfspOpenHandle,
        offset: usize,
        length: usize,
    ) -> Result<Vec<u8>> {
        let opened_file = handle
            .opened_file
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("WinFSP handle does not carry readable file state"))?;
        self.vfs.read(opened_file, offset, length)
    }

    pub fn read_directory_entries(&self, logical_path: &str) -> Result<Vec<DirectoryEntry>> {
        let metadata = self.vfs.stat_path(logical_path)?;
        if metadata.kind == crate::VfsNodeKind::Directory {
            self.remember_inode(metadata.inode_id);
            self.remember_directory_path(&metadata.logical_path);
        }
        self.vfs.read_dir(logical_path)
    }

    pub fn stat_path(&self, logical_path: &str) -> Result<VfsNodeMetadata> {
        let metadata = self.vfs.stat_path(logical_path)?;
        self.remember_inode(metadata.inode_id);
        if metadata.kind == crate::VfsNodeKind::Directory {
            self.remember_directory_path(&metadata.logical_path);
        }
        Ok(metadata)
    }

    pub fn volume_summary(&self) -> ReadOnlyVolumeSummary {
        ReadOnlyVolumeSummary {
            volume_label: format!("e2v {}", self.mount_mode_label()),
            filesystem_name: "e2v-ro".to_string(),
            total_bytes: DEFAULT_TOTAL_BYTES,
            free_bytes: DEFAULT_TOTAL_BYTES,
            sector_size: DEFAULT_SECTOR_SIZE,
            read_only: true,
        }
    }

    fn remember_inode(&self, inode_id: u64) {
        self.observed_inode_ids.lock().unwrap().insert(inode_id);
    }

    fn remember_directory_path(&self, logical_path: &str) {
        self.observed_directory_paths
            .lock()
            .unwrap()
            .insert(logical_path.to_string());
    }
}

#[derive(Debug, Clone)]
pub struct WinfspOpenHandle {
    opened_file: Option<OpenedFile>,
    snapshot_id: String,
    layout_generation: u64,
    logical_path: String,
    file_object_id: String,
    inode_id: u64,
}

impl WinfspOpenHandle {
    fn from_opened_file(opened_file: OpenedFile) -> Self {
        Self {
            snapshot_id: opened_file.snapshot_id().to_string(),
            layout_generation: opened_file.layout_generation(),
            logical_path: opened_file.logical_path().to_string(),
            file_object_id: opened_file.file_object_id().to_string(),
            inode_id: opened_file.inode_id(),
            opened_file: Some(opened_file),
        }
    }

    pub fn from_opened_file_stub(
        snapshot_id: String,
        layout_generation: u64,
        logical_path: String,
        file_object_id: String,
        inode_id: u64,
    ) -> Self {
        Self {
            opened_file: None,
            snapshot_id,
            layout_generation,
            logical_path,
            file_object_id,
            inode_id,
        }
    }

    pub fn snapshot_id(&self) -> &str {
        &self.snapshot_id
    }

    pub fn layout_generation(&self) -> u64 {
        self.layout_generation
    }

    pub fn logical_path(&self) -> &str {
        &self.logical_path
    }

    pub fn file_object_id(&self) -> &str {
        &self.file_object_id
    }

    pub fn inode_id(&self) -> u64 {
        self.inode_id
    }
}
