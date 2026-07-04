use std::collections::BTreeSet;
use std::ffi::{CString, c_void};
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use e2v_core::DirectoryEntry;

use crate::{
    CachePolicy, MountLaunchSummary, MountMode, MountRequest, MountedFilesystem, OpenedFile,
    ReadOnlyVfs, RefreshOutcome, VfsMountConfig, VfsNodeMetadata, WritableVfs,
};

const DEFAULT_SECTOR_SIZE: u32 = 4096;
const DEFAULT_TOTAL_BYTES: u64 = 1 << 40;
const WINDOWS_ADAPTER_STATUS: &str =
    "winfsp adapter boundary ready; windows adapter not implemented yet";
const STATUS_SUCCESS: i32 = 0;
const STATUS_ACCESS_DENIED: i32 = 0xC000_0022u32 as i32;
const STATUS_BUFFER_OVERFLOW: i32 = 0x8000_0005u32 as i32;
const STATUS_INVALID_DEVICE_REQUEST: i32 = 0xC000_0010u32 as i32;
const STATUS_MEDIA_WRITE_PROTECTED: i32 = 0xC000_00A2u32 as i32;
const STATUS_NOT_A_DIRECTORY: i32 = 0xC000_0103u32 as i32;
const STATUS_OBJECT_NAME_NOT_FOUND: i32 = 0xC000_0034u32 as i32;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
const FILE_NON_DIRECTORY_FILE: u32 = 0x0000_0040;
const FILE_WRITE_DATA: u32 = 0x0000_0002;
const FILE_APPEND_DATA: u32 = 0x0000_0004;
const FILE_WRITE_ATTRIBUTES: u32 = 0x0000_0100;
const SECURITY_DESCRIPTOR_REVISION: u32 = 1;
const READ_ONLY_SECURITY_DESCRIPTOR_SDDL: &str = "O:BAG:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FR;;;WD)";
const WRITABLE_SECURITY_DESCRIPTOR_SDDL: &str = "O:BAG:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;WD)";
const VOLUME_PARAMS_ALWAYS_USE_DOUBLE_BUFFERING_BIT: u32 = 1 << 12;
const VOLUME_PARAMS_VOLUME_INFO_TIMEOUT_VALID_BIT: u32 = 1 << 0;
const VOLUME_PARAMS_DIR_INFO_TIMEOUT_VALID_BIT: u32 = 1 << 1;
const FSP_FILE_SYSTEM_OPERATION_GUARD_STRATEGY_FINE: i32 = 0;
const E2V_WINFSP_DEBUG_ENV: &str = "E2V_WINFSP_DEBUG";

static READ_ONLY_SECURITY_DESCRIPTOR_BYTES: LazyLock<Result<Arc<SecurityDescriptorBytes>>> =
    LazyLock::new(|| {
        SecurityDescriptorBytes::from_sddl(READ_ONLY_SECURITY_DESCRIPTOR_SDDL)
            .map(Arc::new)
            .map_err(|error| error.context("failed to parse WinFSP read-only security descriptor"))
    });

static WRITABLE_SECURITY_DESCRIPTOR_BYTES: LazyLock<Result<Arc<SecurityDescriptorBytes>>> =
    LazyLock::new(|| {
        SecurityDescriptorBytes::from_sddl(WRITABLE_SECURITY_DESCRIPTOR_SDDL)
            .map(Arc::new)
            .map_err(|error| error.context("failed to parse WinFSP writable security descriptor"))
    });

#[derive(Debug)]
pub struct WindowsMountedFilesystemHost {
    native_filesystem: *mut NativeFspFileSystem,
    runtime: WinfspRuntimeLibrary,
    _interface: Box<NativeWinfspInterface>,
    _mount_context: Box<WinfspMountContext>,
    stop_state: Arc<HostStopState>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::panic::{self, AssertUnwindSafe};

    use e2v_core::{CommitOptions, InitOptions, RepositoryFacade};
    use tempfile::tempdir;

    fn init_repo(repo_root: &std::path::Path) {
        RepositoryFacade::new()
            .init(InitOptions {
                repo_root: repo_root.to_path_buf(),
                password: "correct horse battery staple".to_string(),
                branch_name: "main".to_string(),
            })
            .unwrap();
    }

    fn commit_message(repo_root: &std::path::Path, message: &str, body: &str) -> String {
        fs::write(repo_root.join("tracked.txt"), body).unwrap();
        RepositoryFacade::new()
            .commit(CommitOptions {
                repo_root: repo_root.to_path_buf(),
                message: message.to_string(),
            })
            .unwrap()
            .snapshot_id
    }

    fn live_branch_context(repo_root: &std::path::Path) -> WinfspMountContext {
        let branch_token = RepositoryFacade::new()
            .open(repo_root)
            .unwrap()
            .branch
            .token_hex;
        WinfspMountContext::from_request(MountRequest::live_branch(
            repo_root.to_path_buf(),
            branch_token,
            PathBuf::from("M:"),
        ))
        .unwrap()
    }

    fn poison_mutex<T>(mutex: &Arc<Mutex<T>>) {
        let poisoned = Arc::clone(mutex);
        let _ = panic::catch_unwind(AssertUnwindSafe(move || {
            let _guard = poisoned.lock().unwrap();
            panic!("poison test mutex");
        }));
    }

    fn poison_writable_vfs(context: &WinfspMountContext) {
        let WinfspVfs::Writable(vfs) = &context.vfs else {
            panic!("expected writable WinFSP context");
        };
        poison_mutex(vfs);
    }

    #[test]
    fn cached_namespace_identity_survives_poisoned_writable_vfs_lock() {
        let temp = tempdir().unwrap();
        let repo_root = temp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();
        init_repo(&repo_root);
        commit_message(&repo_root, "first", "alpha");
        let context = live_branch_context(&repo_root);

        let snapshot_id = context.namespace_snapshot_id();
        poison_writable_vfs(&context);

        let overlay = context.new_overlay_file_handle("created.txt", true);
        assert_eq!(context.namespace_snapshot_id(), snapshot_id);
        assert_eq!(overlay.snapshot_id(), snapshot_id);
        assert!(overlay.layout_generation() > 0);
    }

    #[test]
    fn open_handle_returns_error_after_poisoned_writable_vfs_lock() {
        let temp = tempdir().unwrap();
        let repo_root = temp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();
        init_repo(&repo_root);
        commit_message(&repo_root, "first", "alpha");
        let context = live_branch_context(&repo_root);

        poison_writable_vfs(&context);

        let error = context.open_handle("tracked.txt").unwrap_err();
        assert!(error.to_string().contains("poison"));
    }

    #[test]
    fn refresh_namespace_returns_error_after_poisoned_writable_vfs_lock() {
        let temp = tempdir().unwrap();
        let repo_root = temp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();
        init_repo(&repo_root);
        commit_message(&repo_root, "first", "alpha");
        let mut context = live_branch_context(&repo_root);

        poison_writable_vfs(&context);

        let error = context.refresh_namespace().unwrap_err();
        assert!(error.to_string().contains("poison"));
    }

    #[test]
    fn build_invalidation_plan_recovers_from_poisoned_observer_sets() {
        let temp = tempdir().unwrap();
        let repo_root = temp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();
        init_repo(&repo_root);
        let snapshot_id = commit_message(&repo_root, "first", "alpha");
        let context = WinfspMountContext::from_request(MountRequest::snapshot(
            repo_root,
            snapshot_id,
            PathBuf::from("M:"),
        ))
        .unwrap();

        context.remember_inode(7);
        context.remember_directory_path("nested");
        poison_mutex(&context.observed_inode_ids);
        poison_mutex(&context.observed_directory_paths);

        let plan = context
            .build_invalidation_plan(&RefreshOutcome {
                namespace_changed: true,
                requires_invalidation: true,
            })
            .expect("expected invalidation plan");
        assert_eq!(plan.inode_ids, vec![7]);
        assert_eq!(plan.directory_paths, vec!["nested".to_string()]);
    }
}

impl Drop for WindowsMountedFilesystemHost {
    fn drop(&mut self) {
        self.stop_state.request_stop();
        let stop = self.runtime.stop_dispatcher_fn();
        let remove_mount_point = self.runtime.remove_mount_point_fn();
        unsafe {
            stop(self.native_filesystem);
            remove_mount_point(self.native_filesystem);
        }
        let delete = self.runtime.delete_fn();
        unsafe {
            delete(self.native_filesystem.cast());
        }
    }
}

fn lock_or_recover<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lock_writable_vfs(
    vfs: &Arc<Mutex<WritableVfs>>,
) -> Result<std::sync::MutexGuard<'_, WritableVfs>> {
    vfs.lock()
        .map_err(|_| anyhow::anyhow!("WinFSP writable VFS state is poisoned"))
}

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
    pub(crate) fn from_request(request: &MountRequest) -> Self {
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
        let read_only = context.volume_summary().read_only;
        host.launch(self, context)?;
        Ok(MountLaunchSummary {
            mount_mode: self.mount_mode_label().to_string(),
            mount_point: self.mount_point().clone(),
            cache_policy,
            read_only,
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
    let context = WinfspMountContext::from_request(request)?;
    let mut host = SummaryOnlyHostLauncher;
    launcher.launch_with_host_and_describe(context, &mut host)
}

pub(crate) fn start_snapshot_mount(
    config: VfsMountConfig,
    mount_point: PathBuf,
) -> Result<MountedFilesystem> {
    start_mount_request(MountRequest::from_config(config, mount_point))
}

pub(crate) fn start_live_branch_mount(
    config: VfsMountConfig,
    mount_point: PathBuf,
) -> Result<MountedFilesystem> {
    start_mount_request(MountRequest::from_config(config, mount_point))
}

fn start_mount_request(request: MountRequest) -> Result<MountedFilesystem> {
    let mount_mode = request.mount_mode_label().to_string();
    let mount_point = normalize_windows_mount_point(request.mount_point());
    let raw_mount_point = request.mount_point().clone();
    let cache_policy = match &request.config.mode {
        MountMode::SnapshotPinned { .. } => CachePolicy::KernelCacheWithInvalidation,
        MountMode::LiveBranch { .. } => {
            if request
                .config
                .platform_capabilities
                .supports_live_branch_kernel_cache()
            {
                CachePolicy::KernelCacheWithInvalidation
            } else {
                CachePolicy::DirectIoFallback
            }
        }
    };
    let launcher = WindowsMountLauncher::from_request(&request);
    let mut mount_context = Box::new(WinfspMountContext::from_request(request)?);
    let host_config = launcher.host_config(&mount_context);
    let volume_params = WinfspVolumeParams::from_host_config(&host_config);
    let runtime_paths = WinfspRuntimePaths::from_candidate_roots_for_test(
        &[
            PathBuf::from(r"C:\Program Files (x86)\WinFsp"),
            PathBuf::from(r"C:\Program Files\WinFsp"),
        ],
        "x86_64",
    )?;
    let runtime = WinfspRuntimeLibrary::load(&runtime_paths)?;
    let stop_state = Arc::new(HostStopState::default());
    let interface = Box::new(NativeWinfspInterface::read_only_host());
    mount_context.set_add_dir_info_fn(runtime.add_dir_info_fn());
    let mut native_volume_params = NativeWinfspVolumeParams::from_volume_params(&volume_params);
    let device_path = widestr_from_str("WinFsp.Disk");
    let mut native_filesystem = std::ptr::null_mut::<c_void>();
    let create = runtime.create_fn();
    let status = unsafe {
        create(
            device_path.as_ptr() as *mut u16,
            native_volume_params.as_raw_mut(),
            interface.as_raw(),
            &mut native_filesystem,
        )
    };
    debug_winfsp("create_status", &format!("{status:#x}"));
    if status != STATUS_SUCCESS {
        anyhow::bail!("FspFileSystemCreate failed with NTSTATUS {status:#x}");
    }
    if native_filesystem.is_null() {
        anyhow::bail!("FspFileSystemCreate returned a null filesystem handle");
    }
    unsafe {
        native_filesystem
            .cast::<NativeFspFileSystem>()
            .as_mut()
            .unwrap()
            .set_user_context((&mut *mount_context as *mut WinfspMountContext).cast());
    }
    let set_operation_guard_strategy = runtime.set_operation_guard_strategy_fn();
    unsafe {
        set_operation_guard_strategy(
            native_filesystem.cast::<NativeFspFileSystem>(),
            FSP_FILE_SYSTEM_OPERATION_GUARD_STRATEGY_FINE,
        );
    }
    let mut mount_point_wide = widestr_from_os_path(&raw_mount_point);
    let set_mount_point = runtime.set_mount_point_fn();
    let status = unsafe {
        set_mount_point(
            native_filesystem.cast::<NativeFspFileSystem>(),
            mount_point_wide.as_mut_ptr(),
        )
    };
    debug_winfsp("set_mount_point_status", &format!("{status:#x}"));
    if status != STATUS_SUCCESS {
        let delete = runtime.delete_fn();
        unsafe {
            delete(native_filesystem.cast());
        }
        anyhow::bail!("FspFileSystemSetMountPoint failed with NTSTATUS {status:#x}");
    }
    let start_dispatcher = runtime.start_dispatcher_fn();
    let status = unsafe { start_dispatcher(native_filesystem.cast::<NativeFspFileSystem>(), 0) };
    debug_winfsp("start_dispatcher_status", &format!("{status:#x}"));
    if status != STATUS_SUCCESS {
        let remove_mount_point = runtime.remove_mount_point_fn();
        let delete = runtime.delete_fn();
        unsafe {
            remove_mount_point(native_filesystem.cast::<NativeFspFileSystem>());
            delete(native_filesystem.cast());
        }
        anyhow::bail!("FspFileSystemStartDispatcher failed with NTSTATUS {status:#x}");
    }
    let summary = MountLaunchSummary {
        mount_mode,
        mount_point,
        cache_policy,
        read_only: host_config.read_only,
        stream_only: true,
        status_message: "winfsp host mount active".to_string(),
    };
    Ok(MountedFilesystem::with_windows_host(
        summary,
        WindowsMountedFilesystemHost {
            native_filesystem: native_filesystem.cast::<NativeFspFileSystem>(),
            runtime,
            _interface: interface,
            _mount_context: mount_context,
            stop_state,
        },
    ))
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
    pub persistent_acls: bool,
    pub read_only_volume: bool,
    pub um_file_context_is_user_context2: bool,
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
            persistent_acls: true,
            read_only_volume: config.read_only,
            um_file_context_is_user_context2: true,
            um_file_context_is_full_context: false,
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
    mount_exports: WinfspMountExports,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WinfspMountExports {
    pub create: usize,
    pub remove_mount_point: usize,
    pub set_operation_guard_strategy: usize,
    pub set_mount_point: usize,
    pub start_dispatcher: usize,
    pub stop_dispatcher: usize,
    pub delete_filesystem: usize,
    pub add_dir_info: usize,
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
        let mount_exports = match Self::resolve_mount_exports(handle) {
            Ok(exports) => exports,
            Err(error) => {
                unsafe {
                    FreeLibrary(handle);
                }
                return Err(error);
            }
        };
        Ok(Self {
            dll_path: paths.dll_path.clone(),
            handle,
            mount_exports,
        })
    }

    pub fn dll_path(&self) -> &PathBuf {
        &self.dll_path
    }

    pub(crate) fn get_symbol_address_for_test(&self, symbol_name: &str) -> Result<usize> {
        Self::get_symbol_address(self.handle, symbol_name)
    }

    fn get_symbol_address(handle: *mut c_void, symbol_name: &str) -> Result<usize> {
        let symbol_name = CString::new(symbol_name)?;
        let address = unsafe { GetProcAddress(handle, symbol_name.as_ptr()) };
        if address.is_null() {
            anyhow::bail!("failed to resolve WinFSP export: {symbol_name:?}");
        }
        Ok(address as usize)
    }

    fn resolve_mount_exports(handle: *mut c_void) -> Result<WinfspMountExports> {
        Ok(WinfspMountExports {
            create: Self::get_symbol_address(handle, "FspFileSystemCreate")?,
            remove_mount_point: Self::get_symbol_address(handle, "FspFileSystemRemoveMountPoint")?,
            set_operation_guard_strategy: Self::get_symbol_address(
                handle,
                "FspFileSystemSetOperationGuardStrategyF",
            )?,
            set_mount_point: Self::get_symbol_address(handle, "FspFileSystemSetMountPoint")?,
            start_dispatcher: Self::get_symbol_address(handle, "FspFileSystemStartDispatcher")?,
            stop_dispatcher: Self::get_symbol_address(handle, "FspFileSystemStopDispatcher")?,
            delete_filesystem: Self::get_symbol_address(handle, "FspFileSystemDelete")?,
            add_dir_info: Self::get_symbol_address(handle, "FspFileSystemAddDirInfo")?,
        })
    }

    pub(crate) fn resolve_mount_exports_for_test(&self) -> Result<WinfspMountExports> {
        Ok(self.mount_exports)
    }

    fn create_fn(&self) -> FspFileSystemCreateFn {
        self.mount_exports.create_fn()
    }

    fn set_mount_point_fn(&self) -> FspFileSystemSetMountPointFn {
        self.mount_exports.set_mount_point_fn()
    }

    fn set_operation_guard_strategy_fn(&self) -> FspFileSystemSetOperationGuardStrategyFn {
        self.mount_exports.set_operation_guard_strategy_fn()
    }

    fn remove_mount_point_fn(&self) -> FspFileSystemRemoveMountPointFn {
        self.mount_exports.remove_mount_point_fn()
    }

    fn start_dispatcher_fn(&self) -> FspFileSystemStartDispatcherFn {
        self.mount_exports.start_dispatcher_fn()
    }

    fn stop_dispatcher_fn(&self) -> FspFileSystemStopDispatcherFn {
        self.mount_exports.stop_dispatcher_fn()
    }

    fn delete_fn(&self) -> FspFileSystemDeleteFn {
        self.mount_exports.delete_fn()
    }

    fn add_dir_info_fn(&self) -> FspFileSystemAddDirInfoFn {
        self.mount_exports.add_dir_info_fn()
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

type FspFileSystemSetMountPointFn =
    unsafe extern "C" fn(filesystem: *mut NativeFspFileSystem, mount_point: *mut u16) -> i32;
type FspFileSystemSetOperationGuardStrategyFn =
    unsafe extern "C" fn(filesystem: *mut NativeFspFileSystem, guard_strategy: i32);
type FspFileSystemRemoveMountPointFn = unsafe extern "C" fn(filesystem: *mut NativeFspFileSystem);
type FspFileSystemStartDispatcherFn =
    unsafe extern "C" fn(filesystem: *mut NativeFspFileSystem, thread_count: u32) -> i32;
type FspFileSystemStopDispatcherFn = unsafe extern "C" fn(filesystem: *mut NativeFspFileSystem);
type FspFileSystemDeleteFn = unsafe extern "C" fn(filesystem: *mut c_void);
type FspFileSystemAddDirInfoFn = unsafe extern "C" fn(
    dir_info: *mut NativeFspFsctlDirInfo,
    buffer: *mut c_void,
    length: u32,
    bytes_transferred: *mut u32,
) -> u8;

impl WinfspMountExports {
    fn create_fn(&self) -> FspFileSystemCreateFn {
        unsafe { std::mem::transmute(self.create) }
    }

    fn set_mount_point_fn(&self) -> FspFileSystemSetMountPointFn {
        unsafe { std::mem::transmute(self.set_mount_point) }
    }

    fn set_operation_guard_strategy_fn(&self) -> FspFileSystemSetOperationGuardStrategyFn {
        unsafe { std::mem::transmute(self.set_operation_guard_strategy) }
    }

    fn remove_mount_point_fn(&self) -> FspFileSystemRemoveMountPointFn {
        unsafe { std::mem::transmute(self.remove_mount_point) }
    }

    fn start_dispatcher_fn(&self) -> FspFileSystemStartDispatcherFn {
        unsafe { std::mem::transmute(self.start_dispatcher) }
    }

    fn stop_dispatcher_fn(&self) -> FspFileSystemStopDispatcherFn {
        unsafe { std::mem::transmute(self.stop_dispatcher) }
    }

    fn delete_fn(&self) -> FspFileSystemDeleteFn {
        unsafe { std::mem::transmute(self.delete_filesystem) }
    }

    fn add_dir_info_fn(&self) -> FspFileSystemAddDirInfoFn {
        unsafe { std::mem::transmute(self.add_dir_info) }
    }
}

unsafe extern "system" {
    fn LoadLibraryW(lp_lib_file_name: *const u16) -> *mut c_void;
    fn GetProcAddress(h_module: *mut c_void, lp_proc_name: *const i8) -> *mut c_void;
    fn FreeLibrary(h_lib_module: *mut c_void) -> i32;
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        string_security_descriptor: *const u16,
        string_sd_revision: u32,
        security_descriptor: *mut *mut c_void,
        security_descriptor_size: *mut u32,
    ) -> i32;
    fn LocalFree(memory: *mut c_void) -> *mut c_void;
}

#[repr(C)]
struct NativeFspFileSystem {
    _version: u16,
    _padding: [u8; 6],
    user_context: *mut c_void,
}

impl NativeFspFileSystem {
    unsafe fn set_user_context(&mut self, user_context: *mut c_void) {
        self.user_context = user_context;
    }
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct NativeFspFsctlVolumeInfo {
    total_size: u64,
    free_size: u64,
    volume_label_length: u16,
    volume_label: [u16; 32],
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct NativeFspFsctlFileInfo {
    file_attributes: u32,
    reparse_tag: u32,
    allocation_size: u64,
    file_size: u64,
    creation_time: u64,
    last_access_time: u64,
    last_write_time: u64,
    change_time: u64,
    index_number: u64,
    hard_links: u32,
    ea_size: u32,
}

#[repr(C)]
struct NativeFspFsctlDirInfo {
    size: u16,
    _padding0: [u8; 6],
    file_info: NativeFspFsctlFileInfo,
    _padding1: [u8; 24],
    file_name_buf: [u16; 1],
}

#[derive(Debug, Default)]
struct HostStopState {
    stop_requested: Mutex<bool>,
}

impl HostStopState {
    fn request_stop(&self) {
        *lock_or_recover(&self.stop_requested) = true;
    }
}

#[derive(Debug)]
struct SecurityDescriptorBytes {
    bytes: Vec<u8>,
}

impl SecurityDescriptorBytes {
    fn from_sddl(sddl: &str) -> Result<Self> {
        let mut security_descriptor = std::ptr::null_mut();
        let mut security_descriptor_size = 0u32;
        let wide = widestr_from_str(sddl);
        let converted = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SECURITY_DESCRIPTOR_REVISION,
                &mut security_descriptor,
                &mut security_descriptor_size,
            )
        };
        if converted == 0 {
            anyhow::bail!("failed to convert SDDL to Windows security descriptor");
        }
        let bytes = unsafe {
            let slice = std::slice::from_raw_parts(
                security_descriptor.cast::<u8>(),
                security_descriptor_size as usize,
            );
            let bytes = slice.to_vec();
            let _ = LocalFree(security_descriptor);
            bytes
        };
        Ok(Self { bytes })
    }
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
    additional_flags: u32,
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
        if params.persistent_acls {
            bitfield_1 |= 1 << 3;
        }
        bitfield_1 |= VOLUME_PARAMS_ALWAYS_USE_DOUBLE_BUFFERING_BIT;
        if params.um_file_context_is_user_context2 {
            bitfield_1 |= 1 << 16;
        }
        if params.um_file_context_is_full_context {
            bitfield_1 |= 1 << 17;
        }
        if params.read_only_volume {
            bitfield_1 |= 1 << 9;
        }
        let mut additional_flags = 0u32;
        additional_flags |= VOLUME_PARAMS_VOLUME_INFO_TIMEOUT_VALID_BIT;
        additional_flags |= VOLUME_PARAMS_DIR_INFO_TIMEOUT_VALID_BIT;
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
            additional_flags,
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

type WinfspCreateExFn = unsafe extern "C" fn(
    *mut c_void,
    *mut u16,
    u32,
    u32,
    u32,
    *mut c_void,
    u64,
    *mut c_void,
    u32,
    u8,
    *mut *mut c_void,
    *mut c_void,
) -> i32;

#[repr(C)]
#[derive(Debug, Default)]
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
    resolve_reparse_points: Option<
        unsafe extern "C" fn(
            *mut c_void,
            *mut u16,
            u32,
            u8,
            *mut c_void,
            *mut c_void,
            *mut usize,
        ) -> i32,
    >,
    get_reparse_point: Option<
        unsafe extern "C" fn(*mut c_void, *mut c_void, *mut u16, *mut c_void, *mut usize) -> i32,
    >,
    set_reparse_point:
        Option<unsafe extern "C" fn(*mut c_void, *mut c_void, *mut u16, *mut c_void, usize) -> i32>,
    delete_reparse_point:
        Option<unsafe extern "C" fn(*mut c_void, *mut c_void, *mut u16, *mut c_void, usize) -> i32>,
    get_stream_info:
        Option<unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, u32, *mut u32) -> i32>,
    get_dir_info_by_name: Option<
        unsafe extern "C" fn(*mut c_void, *mut c_void, *mut u16, *mut NativeFspFsctlDirInfo) -> i32,
    >,
    control: Option<
        unsafe extern "C" fn(
            *mut c_void,
            *mut c_void,
            u32,
            *mut c_void,
            u32,
            *mut c_void,
            u32,
            *mut u32,
        ) -> i32,
    >,
    set_delete: Option<unsafe extern "C" fn(*mut c_void, *mut c_void, *mut u16, u8) -> i32>,
    create_ex: Option<WinfspCreateExFn>,
    overwrite_ex: Option<
        unsafe extern "C" fn(
            *mut c_void,
            *mut c_void,
            u32,
            u8,
            u64,
            *mut c_void,
            u32,
            *mut c_void,
        ) -> i32,
    >,
    get_ea:
        Option<unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, u32, *mut u32) -> i32>,
    set_ea: Option<
        unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, u32, *mut c_void) -> i32,
    >,
    obsolete0: Option<unsafe extern "C" fn() -> i32>,
    dispatcher_stopped: Option<unsafe extern "C" fn(*mut c_void, u8)>,
    reserved: [Option<unsafe extern "C" fn() -> i32>; 31],
}

impl NativeWinfspInterface {
    fn read_only_stub() -> Self {
        Self::default()
    }

    fn read_only_host() -> Self {
        Self {
            get_volume_info: Some(host_get_volume_info),
            set_volume_label_w: Some(host_set_volume_label_w),
            get_security_by_name: Some(host_get_security_by_name),
            control: Some(host_control),
            overwrite_ex: Some(host_overwrite_ex),
            create_ex: Some(host_create_ex),
            open: Some(host_open),
            cleanup: Some(host_cleanup),
            close: Some(host_close),
            read: Some(host_read),
            write: Some(host_write),
            flush: Some(host_flush),
            get_file_info: Some(host_get_file_info),
            set_basic_info: Some(host_set_basic_info),
            set_file_size: Some(host_set_file_size),
            get_security: Some(host_get_security),
            set_security: Some(host_set_security),
            read_directory: Some(host_read_directory),
            get_reparse_point: Some(host_get_reparse_point),
            set_reparse_point: Some(host_set_reparse_point),
            delete_reparse_point: Some(host_delete_reparse_point),
            resolve_reparse_points: Some(host_resolve_reparse_points),
            get_stream_info: Some(host_get_stream_info),
            set_delete: Some(host_set_delete),
            rename: Some(host_rename),
            get_ea: Some(host_get_ea),
            set_ea: Some(host_set_ea),
            dispatcher_stopped: Some(host_dispatcher_stopped),
            ..Self::default()
        }
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

fn widestr_from_os_path(value: &std::path::Path) -> Vec<u16> {
    value
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn normalize_windows_mount_point(path: &std::path::Path) -> PathBuf {
    let raw = path.as_os_str().to_string_lossy();
    if raw.len() == 2 && raw.ends_with(':') {
        return PathBuf::from(format!(r"{raw}\"));
    }
    path.to_path_buf()
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

fn context_from_filesystem(filesystem: *mut c_void) -> &'static WinfspMountContext {
    let filesystem = filesystem.cast::<NativeFspFileSystem>();
    unsafe { &*((*filesystem).user_context.cast::<WinfspMountContext>()) }
}

fn volume_info_from_summary(summary: &ReadOnlyVolumeSummary) -> NativeFspFsctlVolumeInfo {
    let mut volume_info = NativeFspFsctlVolumeInfo {
        total_size: summary.total_bytes,
        free_size: summary.free_bytes,
        ..Default::default()
    };
    let label = summary
        .volume_label
        .encode_utf16()
        .take(volume_info.volume_label.len())
        .collect::<Vec<_>>();
    for (index, value) in label.iter().enumerate() {
        volume_info.volume_label[index] = *value;
    }
    volume_info.volume_label_length = (label.len() * std::mem::size_of::<u16>()) as u16;
    volume_info
}

fn normalized_logical_path_from_windows_path(file_name: *const u16) -> String {
    if file_name.is_null() {
        return String::new();
    }
    let mut length = 0usize;
    unsafe {
        while *file_name.add(length) != 0 {
            length += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(file_name, length))
    }
    .trim_start_matches('\\')
    .replace('\\', "/")
}

fn debug_winfsp(event: &str, detail: &str) {
    static WINFSP_DEBUG_ENABLED: LazyLock<bool> = LazyLock::new(|| {
        std::env::var_os(E2V_WINFSP_DEBUG_ENV)
            .map(|value| {
                let value = value.to_string_lossy();
                !value.is_empty() && value != "0" && !value.eq_ignore_ascii_case("false")
            })
            .unwrap_or(false)
    });

    if *WINFSP_DEBUG_ENABLED {
        eprintln!("[e2v-winfsp] {event}: {detail}");
    }
}

fn unsupported_winfsp_status(event: &str, status: i32) -> i32 {
    debug_winfsp(event, &format!("unsupported {status:#x}"));
    status
}

fn copy_security_descriptor_bytes(
    security_descriptor_bytes: &[u8],
    security_descriptor: *mut c_void,
    security_descriptor_size: *mut usize,
) -> i32 {
    if security_descriptor_size.is_null() {
        return STATUS_ACCESS_DENIED;
    }
    let requested = unsafe { *security_descriptor_size };
    unsafe {
        *security_descriptor_size = security_descriptor_bytes.len();
    }
    if requested < security_descriptor_bytes.len() {
        return STATUS_BUFFER_OVERFLOW;
    }
    if !security_descriptor.is_null() {
        unsafe {
            std::ptr::copy_nonoverlapping(
                security_descriptor_bytes.as_ptr(),
                security_descriptor.cast::<u8>(),
                security_descriptor_bytes.len(),
            );
        }
    }
    STATUS_SUCCESS
}

fn fill_file_info_from_metadata(
    file_info: &mut NativeFspFsctlFileInfo,
    metadata: &VfsNodeMetadata,
) {
    file_info.file_attributes = match metadata.kind {
        crate::VfsNodeKind::Directory => FILE_ATTRIBUTE_DIRECTORY,
        crate::VfsNodeKind::File => FILE_ATTRIBUTE_NORMAL,
    };
    file_info.reparse_tag = 0;
    file_info.allocation_size = metadata.size_bytes;
    file_info.file_size = metadata.size_bytes;
    file_info.creation_time = 0;
    file_info.last_access_time = 0;
    file_info.last_write_time = 0;
    file_info.change_time = 0;
    file_info.index_number = metadata.inode_id;
    file_info.hard_links = 1;
    file_info.ea_size = 0;
}

fn fill_file_info_from_open_handle(
    file_info: &mut NativeFspFsctlFileInfo,
    handle: &WinfspOpenHandle,
    file_size: u64,
) {
    file_info.file_attributes = FILE_ATTRIBUTE_NORMAL;
    file_info.reparse_tag = 0;
    file_info.allocation_size = file_size;
    file_info.file_size = file_size;
    file_info.creation_time = 0;
    file_info.last_access_time = 0;
    file_info.last_write_time = 0;
    file_info.change_time = 0;
    file_info.index_number = handle.inode_id();
    file_info.hard_links = 1;
    file_info.ea_size = 0;
}

fn fill_file_info_from_handle(file_info: &mut NativeFspFsctlFileInfo, handle: &WinfspOpenHandle) {
    if handle.is_directory() {
        file_info.file_attributes = FILE_ATTRIBUTE_DIRECTORY;
        file_info.reparse_tag = 0;
        file_info.allocation_size = 0;
        file_info.file_size = 0;
        file_info.creation_time = 0;
        file_info.last_access_time = 0;
        file_info.last_write_time = 0;
        file_info.change_time = 0;
        file_info.index_number = handle.inode_id();
        file_info.hard_links = 1;
        file_info.ea_size = 0;
    } else {
        fill_file_info_from_open_handle(file_info, handle, handle.current_file_size());
    }
}

fn granted_access_requests_write(granted_access: u32) -> bool {
    granted_access & (FILE_WRITE_DATA | FILE_APPEND_DATA | FILE_WRITE_ATTRIBUTES) != 0
}

unsafe extern "C" fn host_get_volume_info(
    filesystem: *mut c_void,
    volume_info: *mut c_void,
) -> i32 {
    let context = context_from_filesystem(filesystem);
    debug_winfsp("get_volume_info", context.mount_mode_label());
    let info = volume_info_from_summary(&context.volume_summary());
    unsafe {
        *(volume_info.cast::<NativeFspFsctlVolumeInfo>()) = info;
    }
    STATUS_SUCCESS
}

unsafe extern "C" fn host_get_security_by_name(
    filesystem: *mut c_void,
    file_name: *mut u16,
    file_attributes: *mut u32,
    security_descriptor: *mut c_void,
    security_descriptor_size: *mut usize,
) -> i32 {
    let context = context_from_filesystem(filesystem);
    let logical_path = normalized_logical_path_from_windows_path(file_name);
    debug_winfsp("get_security_by_name", &logical_path);
    let metadata = match context.stat_path(&logical_path) {
        Ok(metadata) => metadata,
        Err(error) => {
            debug_winfsp("get_security_by_name_err", &error.to_string());
            return STATUS_OBJECT_NAME_NOT_FOUND;
        }
    };
    if !file_attributes.is_null() {
        unsafe {
            *file_attributes = match metadata.kind {
                crate::VfsNodeKind::Directory => FILE_ATTRIBUTE_DIRECTORY,
                crate::VfsNodeKind::File => FILE_ATTRIBUTE_NORMAL,
            };
        }
    }
    copy_security_descriptor_bytes(
        context.security_descriptor_bytes(),
        security_descriptor,
        security_descriptor_size,
    )
}

unsafe extern "C" fn host_set_volume_label_w(
    _filesystem: *mut c_void,
    _volume_label: *mut u16,
    _volume_info: *mut c_void,
) -> i32 {
    unsupported_winfsp_status("set_volume_label_w", STATUS_MEDIA_WRITE_PROTECTED)
}

unsafe extern "C" fn host_open(
    filesystem: *mut c_void,
    file_name: *mut u16,
    _create_options: u32,
    granted_access: u32,
    file_context: *mut *mut c_void,
    file_info: *mut c_void,
) -> i32 {
    let context = context_from_filesystem(filesystem);
    let logical_path = normalized_logical_path_from_windows_path(file_name);
    debug_winfsp("open", &logical_path);
    let metadata = match context.stat_path(&logical_path) {
        Ok(metadata) => metadata,
        Err(error) => {
            debug_winfsp("open_stat_err", &error.to_string());
            return STATUS_OBJECT_NAME_NOT_FOUND;
        }
    };
    if metadata.kind == crate::VfsNodeKind::Directory {
        let handle = Box::new(WinfspOpenHandle::from_opened_file_stub(
            metadata.snapshot_id.clone(),
            metadata.layout_generation,
            metadata.logical_path.clone(),
            "__dir__".to_string(),
            metadata.inode_id,
        ));
        unsafe {
            *file_context = Box::into_raw(handle).cast();
            fill_file_info_from_metadata(
                &mut *file_info.cast::<NativeFspFsctlFileInfo>(),
                &metadata,
            );
        }
        return STATUS_SUCCESS;
    }
    let handle = match context
        .open_handle_with_options(&logical_path, granted_access_requests_write(granted_access))
    {
        Ok(handle) => handle,
        Err(error) => {
            debug_winfsp("open_handle_err", &error.to_string());
            return STATUS_OBJECT_NAME_NOT_FOUND;
        }
    };
    unsafe {
        *file_context = Box::into_raw(Box::new(handle.clone())).cast();
        fill_file_info_from_handle(&mut *file_info.cast::<NativeFspFsctlFileInfo>(), &handle);
    }
    STATUS_SUCCESS
}

unsafe extern "C" fn host_create_ex(
    filesystem: *mut c_void,
    file_name: *mut u16,
    create_options: u32,
    granted_access: u32,
    _file_attributes: u32,
    _security_descriptor: *mut c_void,
    _allocation_size: u64,
    _extra_buffer: *mut c_void,
    _extra_length: u32,
    _extra_buffer_is_reparse_point: u8,
    file_context: *mut *mut c_void,
    file_info: *mut c_void,
) -> i32 {
    let context = context_from_filesystem(filesystem);
    let logical_path = normalized_logical_path_from_windows_path(file_name);
    debug_winfsp("create_ex", &logical_path);
    let metadata = match context.stat_path(&logical_path) {
        Ok(metadata) => metadata,
        Err(error) => {
            debug_winfsp("create_ex_stat_err", &error.to_string());
            if !context.mount_is_writable() {
                return STATUS_MEDIA_WRITE_PROTECTED;
            }
            if (create_options & FILE_DIRECTORY_FILE) != 0 {
                match context.create_directory_and_commit(&logical_path) {
                    Ok(handle) => {
                        unsafe {
                            *file_context = Box::into_raw(Box::new(handle.clone())).cast();
                            fill_file_info_from_handle(
                                &mut *file_info.cast::<NativeFspFsctlFileInfo>(),
                                &handle,
                            );
                        }
                        return STATUS_SUCCESS;
                    }
                    Err(create_error) => {
                        debug_winfsp("create_ex_mkdir_err", &create_error.to_string());
                        return STATUS_MEDIA_WRITE_PROTECTED;
                    }
                }
            }
            let handle = context.new_overlay_file_handle(
                &logical_path,
                granted_access_requests_write(granted_access),
            );
            unsafe {
                *file_context = Box::into_raw(Box::new(handle.clone())).cast();
                fill_file_info_from_handle(
                    &mut *file_info.cast::<NativeFspFsctlFileInfo>(),
                    &handle,
                );
            }
            return STATUS_SUCCESS;
        }
    };
    let wants_directory = (create_options & FILE_DIRECTORY_FILE) != 0;
    let wants_non_directory = (create_options & FILE_NON_DIRECTORY_FILE) != 0;
    match metadata.kind {
        crate::VfsNodeKind::Directory if wants_non_directory => STATUS_NOT_A_DIRECTORY,
        crate::VfsNodeKind::File if wants_directory => STATUS_NOT_A_DIRECTORY,
        crate::VfsNodeKind::Directory => {
            let handle = Box::new(WinfspOpenHandle::from_opened_file_stub(
                metadata.snapshot_id.clone(),
                metadata.layout_generation,
                metadata.logical_path.clone(),
                "__dir__".to_string(),
                metadata.inode_id,
            ));
            unsafe {
                *file_context = Box::into_raw(handle).cast();
                fill_file_info_from_metadata(
                    &mut *file_info.cast::<NativeFspFsctlFileInfo>(),
                    &metadata,
                );
            }
            STATUS_SUCCESS
        }
        crate::VfsNodeKind::File => {
            let handle = match context.open_handle_with_options(
                &logical_path,
                granted_access_requests_write(granted_access),
            ) {
                Ok(handle) => handle,
                Err(error) => {
                    debug_winfsp("create_ex_open_handle_err", &error.to_string());
                    return STATUS_OBJECT_NAME_NOT_FOUND;
                }
            };
            unsafe {
                *file_context = Box::into_raw(Box::new(handle.clone())).cast();
                fill_file_info_from_handle(
                    &mut *file_info.cast::<NativeFspFsctlFileInfo>(),
                    &handle,
                );
            }
            STATUS_SUCCESS
        }
    }
}

unsafe extern "C" fn host_control(
    _filesystem: *mut c_void,
    _file_context: *mut c_void,
    _control_code: u32,
    _input_buffer: *mut c_void,
    _input_buffer_length: u32,
    _output_buffer: *mut c_void,
    _output_buffer_length: u32,
    _bytes_transferred: *mut u32,
) -> i32 {
    unsupported_winfsp_status("control", STATUS_INVALID_DEVICE_REQUEST)
}

unsafe extern "C" fn host_overwrite_ex(
    filesystem: *mut c_void,
    file_context: *mut c_void,
    _file_attributes: u32,
    _replace_file_attributes: u8,
    _allocation_size: u64,
    _ea: *mut c_void,
    _ea_length: u32,
    file_info: *mut c_void,
) -> i32 {
    let context = context_from_filesystem(filesystem);
    let handle = unsafe { &mut *file_context.cast::<WinfspOpenHandle>() };
    if !context.mount_is_writable() || !handle.writable() || handle.is_directory() {
        return unsupported_winfsp_status("overwrite_ex", STATUS_MEDIA_WRITE_PROTECTED);
    }
    let new_size = match usize::try_from(_allocation_size) {
        Ok(size) => size,
        Err(_) => return STATUS_ACCESS_DENIED,
    };
    if let Err(error) = context.resize_handle_bytes(handle, new_size) {
        debug_winfsp("overwrite_ex_err", &error.to_string());
        return STATUS_ACCESS_DENIED;
    }
    if !file_info.is_null() {
        unsafe {
            fill_file_info_from_handle(&mut *file_info.cast::<NativeFspFsctlFileInfo>(), handle);
        }
    }
    STATUS_SUCCESS
}

unsafe extern "C" fn host_cleanup(
    _filesystem: *mut c_void,
    _file_context: *mut c_void,
    _file_name: *mut u16,
    _flags: u32,
) {
    debug_winfsp("cleanup", "");
}

unsafe extern "C" fn host_close(filesystem: *mut c_void, file_context: *mut c_void) {
    debug_winfsp("close", "");
    if !file_context.is_null() {
        let context = context_from_filesystem(filesystem);
        unsafe {
            let mut handle = Box::from_raw(file_context.cast::<WinfspOpenHandle>());
            if let Err(error) = context.flush_handle(&mut handle) {
                debug_winfsp("close_flush_err", &error.to_string());
            }
            drop(handle);
        }
    }
}

unsafe extern "C" fn host_read(
    filesystem: *mut c_void,
    file_context: *mut c_void,
    buffer: *mut c_void,
    offset: u64,
    length: u32,
    bytes_transferred: *mut u32,
) -> i32 {
    let context = context_from_filesystem(filesystem);
    let handle = unsafe { &*file_context.cast::<WinfspOpenHandle>() };
    debug_winfsp(
        "read",
        &format!("{}@{}+{}", handle.logical_path(), offset, length),
    );
    let bytes = match context.read_handle(handle, offset as usize, length as usize) {
        Ok(bytes) => bytes,
        Err(error) => {
            debug_winfsp("read_err", &error.to_string());
            return STATUS_OBJECT_NAME_NOT_FOUND;
        }
    };
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer.cast::<u8>(), bytes.len());
        *bytes_transferred = bytes.len() as u32;
    }
    STATUS_SUCCESS
}

unsafe extern "C" fn host_write(
    filesystem: *mut c_void,
    file_context: *mut c_void,
    buffer: *mut c_void,
    offset: u64,
    length: u32,
    write_to_end_of_file: u8,
    _constrained_io: u8,
    bytes_transferred: *mut u32,
    file_info: *mut c_void,
) -> i32 {
    let context = context_from_filesystem(filesystem);
    let handle = unsafe { &mut *file_context.cast::<WinfspOpenHandle>() };
    if !context.mount_is_writable() || !handle.writable() || handle.is_directory() {
        return unsupported_winfsp_status("write", STATUS_MEDIA_WRITE_PROTECTED);
    }
    let write_len = length as usize;
    let write_bytes = unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), write_len) };
    let written = match context.write_handle_bytes(
        handle,
        offset as usize,
        write_to_end_of_file != 0,
        write_bytes,
    ) {
        Ok(written) => written,
        Err(error) => {
            debug_winfsp("write_err", &error.to_string());
            return STATUS_ACCESS_DENIED;
        }
    };
    unsafe {
        *bytes_transferred = written as u32;
        if !file_info.is_null() {
            fill_file_info_from_handle(&mut *file_info.cast::<NativeFspFsctlFileInfo>(), handle);
        }
    }
    STATUS_SUCCESS
}

unsafe extern "C" fn host_flush(
    filesystem: *mut c_void,
    file_context: *mut c_void,
    file_info: *mut c_void,
) -> i32 {
    if file_context.is_null() {
        return STATUS_SUCCESS;
    }
    let context = context_from_filesystem(filesystem);
    let handle = unsafe { &mut *file_context.cast::<WinfspOpenHandle>() };
    if let Err(error) = context.flush_handle(handle) {
        debug_winfsp("flush_err", &error.to_string());
        return STATUS_ACCESS_DENIED;
    }
    unsafe {
        if !file_info.is_null() {
            fill_file_info_from_handle(&mut *file_info.cast::<NativeFspFsctlFileInfo>(), handle);
        }
    }
    STATUS_SUCCESS
}

unsafe extern "C" fn host_get_file_info(
    filesystem: *mut c_void,
    file_context: *mut c_void,
    file_info: *mut c_void,
) -> i32 {
    let context = context_from_filesystem(filesystem);
    let handle = unsafe { &*file_context.cast::<WinfspOpenHandle>() };
    debug_winfsp("get_file_info", handle.logical_path());
    let metadata = match context.stat_path(handle.logical_path()) {
        Ok(metadata) => metadata,
        Err(error) => {
            debug_winfsp("get_file_info_err", &error.to_string());
            return STATUS_OBJECT_NAME_NOT_FOUND;
        }
    };
    unsafe {
        fill_file_info_from_metadata(&mut *file_info.cast::<NativeFspFsctlFileInfo>(), &metadata);
    }
    STATUS_SUCCESS
}

unsafe extern "C" fn host_get_security(
    filesystem: *mut c_void,
    _file_context: *mut c_void,
    security_descriptor: *mut c_void,
    security_descriptor_size: *mut usize,
) -> i32 {
    debug_winfsp("get_security", "");
    let context = context_from_filesystem(filesystem);
    copy_security_descriptor_bytes(
        context.security_descriptor_bytes(),
        security_descriptor,
        security_descriptor_size,
    )
}

unsafe extern "C" fn host_set_basic_info(
    _filesystem: *mut c_void,
    _file_context: *mut c_void,
    _file_attributes: u32,
    _creation_time: u64,
    _last_access_time: u64,
    _last_write_time: u64,
    _change_time: u64,
    _file_info: *mut c_void,
) -> i32 {
    unsupported_winfsp_status("set_basic_info", STATUS_MEDIA_WRITE_PROTECTED)
}

unsafe extern "C" fn host_set_file_size(
    filesystem: *mut c_void,
    file_context: *mut c_void,
    new_size: u64,
    _set_allocation_size: u8,
    file_info: *mut c_void,
) -> i32 {
    let context = context_from_filesystem(filesystem);
    let handle = unsafe { &mut *file_context.cast::<WinfspOpenHandle>() };
    if !context.mount_is_writable() || !handle.writable() || handle.is_directory() {
        return unsupported_winfsp_status("set_file_size", STATUS_MEDIA_WRITE_PROTECTED);
    }
    let new_size = match usize::try_from(new_size) {
        Ok(size) => size,
        Err(_) => return STATUS_ACCESS_DENIED,
    };
    if let Err(error) = context.resize_handle_bytes(handle, new_size) {
        debug_winfsp("set_file_size_err", &error.to_string());
        return STATUS_ACCESS_DENIED;
    }
    unsafe {
        if !file_info.is_null() {
            fill_file_info_from_handle(&mut *file_info.cast::<NativeFspFsctlFileInfo>(), handle);
        }
    }
    STATUS_SUCCESS
}

unsafe extern "C" fn host_set_security(
    _filesystem: *mut c_void,
    _file_context: *mut c_void,
    _security_information: u32,
    _modification_descriptor: *mut c_void,
) -> i32 {
    unsupported_winfsp_status("set_security", STATUS_MEDIA_WRITE_PROTECTED)
}

unsafe extern "C" fn host_read_directory(
    filesystem: *mut c_void,
    file_context: *mut c_void,
    _pattern: *mut u16,
    marker: *mut u16,
    buffer: *mut c_void,
    length: u32,
    bytes_transferred: *mut u32,
) -> i32 {
    let context = context_from_filesystem(filesystem);
    let handle = unsafe { &*file_context.cast::<WinfspOpenHandle>() };
    debug_winfsp("read_directory", handle.logical_path());
    let metadata = match context.stat_path(handle.logical_path()) {
        Ok(metadata) => metadata,
        Err(error) => {
            debug_winfsp("read_directory_stat_err", &error.to_string());
            return STATUS_OBJECT_NAME_NOT_FOUND;
        }
    };
    if metadata.kind != crate::VfsNodeKind::Directory {
        return STATUS_NOT_A_DIRECTORY;
    }
    unsafe {
        *bytes_transferred = 0;
    }
    let entries = match context.read_directory_entries(handle.logical_path()) {
        Ok(entries) => entries,
        Err(error) => {
            debug_winfsp("read_directory_entries_err", &error.to_string());
            return STATUS_OBJECT_NAME_NOT_FOUND;
        }
    };
    let marker_name = normalized_logical_path_from_windows_path(marker);
    let Some(add_dir_info) = context.add_dir_info_fn() else {
        return STATUS_ACCESS_DENIED;
    };
    for entry in entries {
        if !marker_name.is_empty() && entry.name <= marker_name {
            continue;
        }
        let child_logical_path = if handle.logical_path().is_empty() {
            entry.name.clone()
        } else {
            format!("{}/{}", handle.logical_path(), entry.name)
        };
        let child_metadata = match context.stat_path(&child_logical_path) {
            Ok(metadata) => metadata,
            Err(_) => return STATUS_OBJECT_NAME_NOT_FOUND,
        };
        let file_name = entry.name.encode_utf16().collect::<Vec<_>>();
        let dir_info_size = std::mem::size_of::<NativeFspFsctlDirInfo>()
            + file_name.len().saturating_sub(1) * std::mem::size_of::<u16>();
        let mut dir_info = vec![0u8; dir_info_size];
        let dir_info_ptr = dir_info.as_mut_ptr().cast::<NativeFspFsctlDirInfo>();
        unsafe {
            (*dir_info_ptr).size = dir_info_size as u16;
            fill_file_info_from_metadata(&mut (*dir_info_ptr).file_info, &child_metadata);
            std::ptr::copy_nonoverlapping(
                file_name.as_ptr(),
                (*dir_info_ptr).file_name_buf.as_mut_ptr(),
                file_name.len(),
            );
            let added = add_dir_info(dir_info_ptr, buffer, length, bytes_transferred);
            if added == 0 {
                break;
            }
        }
    }
    unsafe {
        add_dir_info(std::ptr::null_mut(), buffer, length, bytes_transferred);
    }
    STATUS_SUCCESS
}

unsafe extern "C" fn host_get_reparse_point(
    _filesystem: *mut c_void,
    _file_context: *mut c_void,
    _file_name: *mut u16,
    _buffer: *mut c_void,
    _size: *mut usize,
) -> i32 {
    unsupported_winfsp_status("get_reparse_point", STATUS_INVALID_DEVICE_REQUEST)
}

unsafe extern "C" fn host_set_reparse_point(
    _filesystem: *mut c_void,
    _file_context: *mut c_void,
    _file_name: *mut u16,
    _buffer: *mut c_void,
    _size: usize,
) -> i32 {
    unsupported_winfsp_status("set_reparse_point", STATUS_MEDIA_WRITE_PROTECTED)
}

unsafe extern "C" fn host_delete_reparse_point(
    _filesystem: *mut c_void,
    _file_context: *mut c_void,
    _file_name: *mut u16,
    _buffer: *mut c_void,
    _size: usize,
) -> i32 {
    unsupported_winfsp_status("delete_reparse_point", STATUS_MEDIA_WRITE_PROTECTED)
}

unsafe extern "C" fn host_resolve_reparse_points(
    _filesystem: *mut c_void,
    _file_name: *mut u16,
    _reparse_point_index: u32,
    _resolve_last_path_component: u8,
    _io_status: *mut c_void,
    _buffer: *mut c_void,
    _size: *mut usize,
) -> i32 {
    unsupported_winfsp_status("resolve_reparse_points", STATUS_INVALID_DEVICE_REQUEST)
}

unsafe extern "C" fn host_get_stream_info(
    _filesystem: *mut c_void,
    _file_context: *mut c_void,
    _buffer: *mut c_void,
    _length: u32,
    _bytes_transferred: *mut u32,
) -> i32 {
    unsupported_winfsp_status("get_stream_info", STATUS_INVALID_DEVICE_REQUEST)
}

unsafe extern "C" fn host_set_delete(
    filesystem: *mut c_void,
    file_context: *mut c_void,
    _file_name: *mut u16,
    delete_file: u8,
) -> i32 {
    if delete_file == 0 {
        return STATUS_SUCCESS;
    }
    let context = context_from_filesystem(filesystem);
    if !context.mount_is_writable() {
        return unsupported_winfsp_status("set_delete", STATUS_MEDIA_WRITE_PROTECTED);
    }
    let handle = unsafe { &mut *file_context.cast::<WinfspOpenHandle>() };
    let WinfspVfs::Writable(vfs) = &context.vfs else {
        return unsupported_winfsp_status("set_delete", STATUS_MEDIA_WRITE_PROTECTED);
    };
    let mut vfs = match lock_writable_vfs(vfs) {
        Ok(vfs) => vfs,
        Err(error) => {
            debug_winfsp("set_delete_err", &error.to_string());
            return STATUS_ACCESS_DENIED;
        }
    };
    let result = if handle.is_directory() {
        vfs.delete_directory_and_writeback(handle.logical_path(), "winfsp rmdir")
    } else {
        vfs.delete_file_and_writeback(handle.logical_path(), "winfsp delete")
    };
    if let Err(error) = result {
        debug_winfsp("set_delete_err", &error.to_string());
        return STATUS_ACCESS_DENIED;
    }
    handle.opened_file = None;
    handle.pending_bytes = None;
    STATUS_SUCCESS
}

unsafe extern "C" fn host_rename(
    filesystem: *mut c_void,
    file_context: *mut c_void,
    _file_name: *mut u16,
    new_file_name: *mut u16,
    _replace_if_exists: u8,
) -> i32 {
    let context = context_from_filesystem(filesystem);
    if !context.mount_is_writable() {
        return unsupported_winfsp_status("rename", STATUS_MEDIA_WRITE_PROTECTED);
    }
    let handle = unsafe { &mut *file_context.cast::<WinfspOpenHandle>() };
    let target_logical_path = normalized_logical_path_from_windows_path(new_file_name);
    let WinfspVfs::Writable(vfs) = &context.vfs else {
        return unsupported_winfsp_status("rename", STATUS_MEDIA_WRITE_PROTECTED);
    };
    let mut vfs = match lock_writable_vfs(vfs) {
        Ok(vfs) => vfs,
        Err(error) => {
            debug_winfsp("rename_err", &error.to_string());
            return STATUS_ACCESS_DENIED;
        }
    };
    if let Err(error) =
        vfs.rename_and_writeback(handle.logical_path(), &target_logical_path, "winfsp rename")
    {
        debug_winfsp("rename_err", &error.to_string());
        return STATUS_ACCESS_DENIED;
    }
    handle.logical_path = target_logical_path.clone();
    handle.file_object_id = format!("overlay:{target_logical_path}");
    handle.pending_bytes = None;
    STATUS_SUCCESS
}

unsafe extern "C" fn host_get_ea(
    _filesystem: *mut c_void,
    _file_context: *mut c_void,
    _ea: *mut c_void,
    _ea_length: u32,
    _bytes_transferred: *mut u32,
) -> i32 {
    unsupported_winfsp_status("get_ea", STATUS_INVALID_DEVICE_REQUEST)
}

unsafe extern "C" fn host_set_ea(
    _filesystem: *mut c_void,
    _file_context: *mut c_void,
    _ea: *mut c_void,
    _ea_length: u32,
    _file_info: *mut c_void,
) -> i32 {
    unsupported_winfsp_status("set_ea", STATUS_MEDIA_WRITE_PROTECTED)
}

unsafe extern "C" fn host_dispatcher_stopped(_filesystem: *mut c_void, normally: u8) {
    debug_winfsp(
        "dispatcher_stopped",
        if normally == 0 { "abnormal" } else { "normal" },
    );
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
    namespace_snapshot_id: String,
    namespace_layout_generation: u64,
    observed_inode_ids: Arc<Mutex<BTreeSet<u64>>>,
    observed_directory_paths: Arc<Mutex<BTreeSet<String>>>,
    add_dir_info: Option<FspFileSystemAddDirInfoFn>,
    security_descriptor: Arc<SecurityDescriptorBytes>,
    vfs: WinfspVfs,
}

#[derive(Debug, Clone)]
enum WinfspVfs {
    ReadOnly(ReadOnlyVfs),
    Writable(Arc<Mutex<WritableVfs>>),
}

impl WinfspMountContext {
    pub(crate) fn from_request(request: MountRequest) -> Result<Self> {
        let vfs = match &request.config.mode {
            MountMode::SnapshotPinned { .. } => {
                WinfspVfs::ReadOnly(ReadOnlyVfs::mount_snapshot(request.config.clone())?)
            }
            MountMode::LiveBranch { .. } => WinfspVfs::Writable(Arc::new(Mutex::new(
                WritableVfs::mount_live_branch(request.config.clone())?,
            ))),
        };
        let cache_policy = match &vfs {
            WinfspVfs::ReadOnly(vfs) => vfs.cache_policy(),
            WinfspVfs::Writable(vfs) => lock_writable_vfs(vfs)?.cache_policy(),
        };
        let (namespace_snapshot_id, namespace_layout_generation) = match &vfs {
            WinfspVfs::ReadOnly(vfs) => (
                vfs.namespace_snapshot_id(),
                vfs.namespace_layout_generation(),
            ),
            WinfspVfs::Writable(vfs) => {
                let vfs = lock_writable_vfs(vfs)?;
                (
                    vfs.namespace_snapshot_id(),
                    vfs.namespace_layout_generation(),
                )
            }
        };
        let security_descriptor = if matches!(vfs, WinfspVfs::Writable(_)) {
            Arc::clone(
                WRITABLE_SECURITY_DESCRIPTOR_BYTES
                    .as_ref()
                    .map_err(|error| anyhow::anyhow!("{error:#}"))?,
            )
        } else {
            Arc::clone(
                READ_ONLY_SECURITY_DESCRIPTOR_BYTES
                    .as_ref()
                    .map_err(|error| anyhow::anyhow!("{error:#}"))?,
            )
        };
        Ok(Self {
            request,
            cache_policy,
            namespace_snapshot_id,
            namespace_layout_generation,
            observed_inode_ids: Arc::new(Mutex::new(BTreeSet::new())),
            observed_directory_paths: Arc::new(Mutex::new(BTreeSet::new())),
            add_dir_info: None,
            security_descriptor,
            vfs,
        })
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
        self.namespace_snapshot_id.clone()
    }

    pub fn refresh_namespace(&mut self) -> Result<RefreshOutcome> {
        match &mut self.vfs {
            WinfspVfs::ReadOnly(vfs) => vfs.refresh_live_branch(),
            WinfspVfs::Writable(vfs) => {
                let refresh = lock_writable_vfs(vfs)?.refresh_live_branch()?;
                if refresh.namespace_changed {
                    let vfs = lock_writable_vfs(vfs)?;
                    self.namespace_snapshot_id = vfs.namespace_snapshot_id();
                    self.namespace_layout_generation = vfs.namespace_layout_generation();
                }
                Ok(refresh)
            }
        }
    }

    fn set_add_dir_info_fn(&mut self, add_dir_info: FspFileSystemAddDirInfoFn) {
        self.add_dir_info = Some(add_dir_info);
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
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .copied()
            .collect::<Vec<_>>();
        let directory_paths = self
            .observed_directory_paths
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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
        self.open_handle_with_options(logical_path, false)
    }

    pub fn open_handle_with_options(
        &self,
        logical_path: &str,
        writable: bool,
    ) -> Result<WinfspOpenHandle> {
        let opened = match &self.vfs {
            WinfspVfs::ReadOnly(vfs) => vfs.open_file(logical_path)?,
            WinfspVfs::Writable(vfs) => lock_writable_vfs(vfs)?.open_file(logical_path)?,
        };
        let handle = WinfspOpenHandle::from_opened_file(opened).with_writable(writable);
        self.remember_inode(handle.inode_id());
        Ok(handle)
    }

    pub fn open_handle_for_request(&self, request: &WinfspOpenRequest) -> Result<WinfspOpenHandle> {
        if request.writable {
            match &self.vfs {
                WinfspVfs::ReadOnly(vfs) => {
                    vfs.require_semantic(crate::VfsSemantic::WritableHandles)?
                }
                WinfspVfs::Writable(vfs) => {
                    lock_writable_vfs(vfs)?.require_semantic(crate::VfsSemantic::WritableHandles)?
                }
            }
        }
        self.open_handle_with_options(&request.logical_path, request.writable)
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
        if let Some(bytes) = &handle.pending_bytes {
            anyhow::ensure!(offset <= bytes.len(), "range offset out of bounds");
            let end = offset.saturating_add(length).min(bytes.len());
            return Ok(bytes[offset..end].to_vec());
        }
        match &self.vfs {
            WinfspVfs::ReadOnly(vfs) => vfs.read(opened_file, offset, length),
            WinfspVfs::Writable(vfs) => lock_writable_vfs(vfs)?.read(opened_file, offset, length),
        }
    }

    pub fn read_directory_entries(&self, logical_path: &str) -> Result<Vec<DirectoryEntry>> {
        let metadata = self.stat_path(logical_path)?;
        if metadata.kind == crate::VfsNodeKind::Directory {
            self.remember_inode(metadata.inode_id);
            self.remember_directory_path(&metadata.logical_path);
        }
        match &self.vfs {
            WinfspVfs::ReadOnly(vfs) => vfs.read_dir(logical_path),
            WinfspVfs::Writable(vfs) => lock_writable_vfs(vfs)?.read_dir(logical_path),
        }
    }

    pub fn stat_path(&self, logical_path: &str) -> Result<VfsNodeMetadata> {
        let metadata = match &self.vfs {
            WinfspVfs::ReadOnly(vfs) => vfs.stat_path(logical_path)?,
            WinfspVfs::Writable(vfs) => lock_writable_vfs(vfs)?.stat_path(logical_path)?,
        };
        self.remember_inode(metadata.inode_id);
        if metadata.kind == crate::VfsNodeKind::Directory {
            self.remember_directory_path(&metadata.logical_path);
        }
        Ok(metadata)
    }

    pub fn volume_summary(&self) -> ReadOnlyVolumeSummary {
        let writable = matches!(self.vfs, WinfspVfs::Writable(_));
        ReadOnlyVolumeSummary {
            volume_label: format!("e2v {}", self.mount_mode_label()),
            filesystem_name: if writable {
                "e2v-rw".to_string()
            } else {
                "e2v-ro".to_string()
            },
            total_bytes: DEFAULT_TOTAL_BYTES,
            free_bytes: DEFAULT_TOTAL_BYTES,
            sector_size: DEFAULT_SECTOR_SIZE,
            read_only: !writable,
        }
    }

    fn security_descriptor_bytes(&self) -> &[u8] {
        &self.security_descriptor.bytes
    }

    fn add_dir_info_fn(&self) -> Option<FspFileSystemAddDirInfoFn> {
        self.add_dir_info
    }

    fn remember_inode(&self, inode_id: u64) {
        lock_or_recover(&self.observed_inode_ids).insert(inode_id);
    }

    fn remember_directory_path(&self, logical_path: &str) {
        lock_or_recover(&self.observed_directory_paths).insert(logical_path.to_string());
    }

    fn mount_is_writable(&self) -> bool {
        matches!(self.vfs, WinfspVfs::Writable(_))
    }

    fn new_overlay_file_handle(&self, logical_path: &str, writable: bool) -> WinfspOpenHandle {
        let (snapshot_id, layout_generation) = match &self.vfs {
            WinfspVfs::ReadOnly(vfs) => (
                vfs.namespace_snapshot_id(),
                vfs.namespace_layout_generation(),
            ),
            WinfspVfs::Writable(_) => (
                self.namespace_snapshot_id.clone(),
                self.namespace_layout_generation,
            ),
        };
        WinfspOpenHandle {
            opened_file: None,
            snapshot_id: snapshot_id.clone(),
            layout_generation,
            logical_path: logical_path.to_string(),
            file_object_id: format!("overlay:{logical_path}"),
            inode_id: crate::stable_inode_id(
                &snapshot_id,
                logical_path,
                &format!("overlay:{logical_path}"),
            ),
            writable,
            is_directory: false,
            pending_bytes: Some(Vec::new()),
        }
    }

    fn create_directory_and_commit(&self, logical_path: &str) -> Result<WinfspOpenHandle> {
        let WinfspVfs::Writable(vfs) = &self.vfs else {
            anyhow::bail!("directory creation is not available for read-only mounts");
        };
        lock_writable_vfs(vfs)?.create_directory_and_writeback(logical_path, "winfsp mkdir")?;
        let metadata = self.stat_path(logical_path)?;
        Ok(WinfspOpenHandle::from_opened_file_stub(
            metadata.snapshot_id,
            metadata.layout_generation,
            metadata.logical_path,
            "__dir__".to_string(),
            metadata.inode_id,
        ))
    }

    fn ensure_pending_bytes<'a>(
        &self,
        handle: &'a mut WinfspOpenHandle,
    ) -> Result<&'a mut Vec<u8>> {
        if handle.pending_bytes.is_none() {
            let bytes = if let Some(opened_file) = &handle.opened_file {
                match &self.vfs {
                    WinfspVfs::ReadOnly(vfs) => {
                        vfs.read(opened_file, 0, opened_file.file_size() as usize)?
                    }
                    WinfspVfs::Writable(vfs) => lock_writable_vfs(vfs)?.read(
                        opened_file,
                        0,
                        opened_file.file_size() as usize,
                    )?,
                }
            } else {
                Vec::new()
            };
            handle.pending_bytes = Some(bytes);
        }
        Ok(handle.pending_bytes.as_mut().unwrap())
    }

    fn resize_handle_bytes(&self, handle: &mut WinfspOpenHandle, new_size: usize) -> Result<()> {
        let bytes = self.ensure_pending_bytes(handle)?;
        bytes.resize(new_size, 0);
        Ok(())
    }

    fn write_handle_bytes(
        &self,
        handle: &mut WinfspOpenHandle,
        offset: usize,
        write_to_end_of_file: bool,
        bytes_to_write: &[u8],
    ) -> Result<usize> {
        let bytes = self.ensure_pending_bytes(handle)?;
        let start = if write_to_end_of_file {
            bytes.len()
        } else {
            offset
        };
        if start > bytes.len() {
            bytes.resize(start, 0);
        }
        let end = start.saturating_add(bytes_to_write.len());
        if end > bytes.len() {
            bytes.resize(end, 0);
        }
        bytes[start..end].copy_from_slice(bytes_to_write);
        Ok(bytes_to_write.len())
    }

    fn stage_handle_bytes(&self, handle: &mut WinfspOpenHandle) -> Result<()> {
        let Some(bytes) = handle.pending_bytes.clone() else {
            return Ok(());
        };
        let WinfspVfs::Writable(vfs) = &self.vfs else {
            anyhow::bail!("write staging is not available for read-only mounts");
        };
        let mut vfs = lock_writable_vfs(vfs)?;
        vfs.write_file(handle.logical_path(), bytes)?;
        let _ = vfs.writeback("winfsp writeback")?;
        let reopened = vfs.open_file(handle.logical_path())?;
        handle.snapshot_id = reopened.snapshot_id().to_string();
        handle.layout_generation = reopened.layout_generation();
        handle.file_object_id = reopened.file_object_id().to_string();
        handle.inode_id = reopened.inode_id();
        handle.opened_file = Some(reopened);
        handle.pending_bytes = None;
        Ok(())
    }

    fn flush_handle(&self, handle: &mut WinfspOpenHandle) -> Result<u64> {
        if handle.writable() && !handle.is_directory() && handle.pending_bytes.is_some() {
            self.stage_handle_bytes(handle)?;
        }
        Ok(handle.current_file_size())
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
    writable: bool,
    is_directory: bool,
    pending_bytes: Option<Vec<u8>>,
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
            writable: false,
            is_directory: false,
            pending_bytes: None,
        }
    }

    pub fn from_opened_file_stub(
        snapshot_id: String,
        layout_generation: u64,
        logical_path: String,
        file_object_id: String,
        inode_id: u64,
    ) -> Self {
        let is_directory = file_object_id == "__dir__";
        Self {
            opened_file: None,
            snapshot_id,
            layout_generation,
            logical_path,
            file_object_id,
            inode_id,
            writable: false,
            is_directory,
            pending_bytes: None,
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

    fn with_writable(mut self, writable: bool) -> Self {
        self.writable = writable;
        self
    }

    fn writable(&self) -> bool {
        self.writable
    }

    fn is_directory(&self) -> bool {
        self.is_directory
    }

    fn current_file_size(&self) -> u64 {
        if let Some(bytes) = &self.pending_bytes {
            bytes.len() as u64
        } else {
            self.opened_file
                .as_ref()
                .map(|opened_file| opened_file.file_size())
                .unwrap_or(0)
        }
    }
}
