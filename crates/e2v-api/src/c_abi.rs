#![allow(non_camel_case_types)]
#![allow(non_upper_case_globals)]
#![allow(clippy::missing_safety_doc)]

use std::ffi::{CStr, CString, c_char};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;

use serde::Serialize;

use crate::{
    CloneRequest, CommitRepositoryOptions, FetchRequest, GcExecuteRequest, InitRepositoryOptions,
    PushRequest, ReadHandle, Sdk, SdkError, SdkErrorCode, ShareAcceptDeviceRequest,
    ShareAcceptMemberRequest, ShareInviteDeviceRequest,
    ShareInviteMemberRequest, ShareRevokeDeviceRequest, ShareRevokeMemberRequest, SnapshotView,
    VerifyRemoteRequest,
};

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum e2v_error_code_t {
    E2V_OK = 0,
    E2V_INVALID_ARGUMENT = 1,
    E2V_NOT_FOUND = 2,
    E2V_ALREADY_EXISTS = 3,
    E2V_PERMISSION_DENIED = 4,
    E2V_AUTHENTICATION_REQUIRED = 5,
    E2V_CONFLICT = 6,
    E2V_NEEDS_REBASE = 7,
    E2V_ROLLBACK_DETECTED = 8,
    E2V_UNSUPPORTED = 9,
    E2V_CORRUPT_STATE = 10,
    E2V_IO = 11,
    E2V_INTERNAL = 12,
    E2V_INTERNAL_PANIC = 255,
}

pub const E2V_OK: e2v_error_code_t = e2v_error_code_t::E2V_OK;
pub const E2V_INVALID_ARGUMENT: e2v_error_code_t = e2v_error_code_t::E2V_INVALID_ARGUMENT;
pub const E2V_NOT_FOUND: e2v_error_code_t = e2v_error_code_t::E2V_NOT_FOUND;
pub const E2V_ALREADY_EXISTS: e2v_error_code_t = e2v_error_code_t::E2V_ALREADY_EXISTS;
pub const E2V_PERMISSION_DENIED: e2v_error_code_t = e2v_error_code_t::E2V_PERMISSION_DENIED;
pub const E2V_AUTHENTICATION_REQUIRED: e2v_error_code_t =
    e2v_error_code_t::E2V_AUTHENTICATION_REQUIRED;
pub const E2V_CONFLICT: e2v_error_code_t = e2v_error_code_t::E2V_CONFLICT;
pub const E2V_NEEDS_REBASE: e2v_error_code_t = e2v_error_code_t::E2V_NEEDS_REBASE;
pub const E2V_ROLLBACK_DETECTED: e2v_error_code_t =
    e2v_error_code_t::E2V_ROLLBACK_DETECTED;
pub const E2V_UNSUPPORTED: e2v_error_code_t = e2v_error_code_t::E2V_UNSUPPORTED;
pub const E2V_CORRUPT_STATE: e2v_error_code_t = e2v_error_code_t::E2V_CORRUPT_STATE;
pub const E2V_IO: e2v_error_code_t = e2v_error_code_t::E2V_IO;
pub const E2V_INTERNAL: e2v_error_code_t = e2v_error_code_t::E2V_INTERNAL;
pub const E2V_INTERNAL_PANIC: e2v_error_code_t = e2v_error_code_t::E2V_INTERNAL_PANIC;

#[repr(C)]
#[derive(Debug, Default)]
pub struct e2v_string_t {
    pub ptr: *const c_char,
    pub len: usize,
}

#[repr(C)]
#[derive(Debug, Default)]
pub struct e2v_bytes_t {
    pub ptr: *const u8,
    pub len: usize,
}

pub struct e2v_sdk_t {
    inner: Sdk,
}

pub struct e2v_read_handle_t {
    inner: ReadHandle,
}

pub struct e2v_snapshot_view_t {
    inner: SnapshotView,
}

pub struct e2v_file_view_t {
    inner: crate::FileView,
}

pub struct e2v_error_t {
    code: e2v_error_code_t,
    message: String,
}

fn error_code_from_sdk(code: SdkErrorCode) -> e2v_error_code_t {
    match code {
        SdkErrorCode::InvalidArgument => E2V_INVALID_ARGUMENT,
        SdkErrorCode::NotFound => E2V_NOT_FOUND,
        SdkErrorCode::AlreadyExists => E2V_ALREADY_EXISTS,
        SdkErrorCode::PermissionDenied => E2V_PERMISSION_DENIED,
        SdkErrorCode::AuthenticationRequired => E2V_AUTHENTICATION_REQUIRED,
        SdkErrorCode::Conflict => E2V_CONFLICT,
        SdkErrorCode::NeedsRebase => E2V_NEEDS_REBASE,
        SdkErrorCode::RollbackDetected => E2V_ROLLBACK_DETECTED,
        SdkErrorCode::Unsupported => E2V_UNSUPPORTED,
        SdkErrorCode::CorruptState => E2V_CORRUPT_STATE,
        SdkErrorCode::Io => E2V_IO,
        SdkErrorCode::Internal => E2V_INTERNAL,
    }
}

fn invalid_argument(message: impl Into<String>) -> SdkError {
    SdkError::new(SdkErrorCode::InvalidArgument, message)
}

fn panic_error() -> SdkError {
    SdkError::new(SdkErrorCode::Internal, "panic crossed ffi boundary")
}

fn clear_error_out(error_out: *mut *mut e2v_error_t) {
    if !error_out.is_null() {
        unsafe {
            *error_out = ptr::null_mut();
        }
    }
}

fn set_error_out(error_out: *mut *mut e2v_error_t, error: SdkError) -> e2v_error_code_t {
    let code = error_code_from_sdk(error.code());
    if !error_out.is_null() {
        unsafe {
            *error_out = Box::into_raw(Box::new(e2v_error_t {
                code,
                message: error.message().to_string(),
            }));
        }
    }
    code
}

fn set_panic_out(error_out: *mut *mut e2v_error_t) -> e2v_error_code_t {
    if !error_out.is_null() {
        unsafe {
            *error_out = Box::into_raw(Box::new(e2v_error_t {
                code: E2V_INTERNAL_PANIC,
                message: panic_error().message().to_string(),
            }));
        }
    }
    E2V_INTERNAL_PANIC
}

fn ffi_call<F>(error_out: *mut *mut e2v_error_t, f: F) -> e2v_error_code_t
where
    F: FnOnce() -> crate::SdkResult<()>,
{
    clear_error_out(error_out);
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(())) => E2V_OK,
        Ok(Err(error)) => set_error_out(error_out, error),
        Err(_) => set_panic_out(error_out),
    }
}

fn ffi_call_with_json<T, F>(
    json_out: *mut e2v_string_t,
    error_out: *mut *mut e2v_error_t,
    f: F,
) -> e2v_error_code_t
where
    T: Serialize,
    F: FnOnce() -> crate::SdkResult<T>,
{
    if json_out.is_null() {
        return set_error_out(
            error_out,
            invalid_argument("json output pointer must not be null"),
        );
    }
    unsafe {
        *json_out = e2v_string_t::default();
    }

    ffi_call(error_out, || {
        let value = f()?;
        let json = serde_json::to_string(&value)
            .map_err(anyhow::Error::from)
            .map_err(crate::map_error)?;
        write_owned_string(json_out, json)
    })
}

fn ffi_call_with_handle<T, H, F>(
    handle_out: *mut *mut H,
    error_out: *mut *mut e2v_error_t,
    wrap: impl FnOnce(T) -> H,
    f: F,
) -> e2v_error_code_t
where
    F: FnOnce() -> crate::SdkResult<T>,
{
    if handle_out.is_null() {
        return set_error_out(
            error_out,
            invalid_argument("handle output pointer must not be null"),
        );
    }
    unsafe {
        *handle_out = ptr::null_mut();
    }

    ffi_call(error_out, || {
        let value = f()?;
        unsafe {
            *handle_out = Box::into_raw(Box::new(wrap(value)));
        }
        Ok(())
    })
}

fn ffi_read_c_string(value: *const c_char, argument_name: &str) -> crate::SdkResult<String> {
    if value.is_null() {
        return Err(invalid_argument(format!("{argument_name} must not be null")));
    }
    let text = unsafe { CStr::from_ptr(value) }
        .to_str()
        .map_err(|_| invalid_argument(format!("{argument_name} must be valid utf-8")))?;
    Ok(text.to_string())
}

fn write_owned_string(target: *mut e2v_string_t, value: String) -> crate::SdkResult<()> {
    let value = CString::new(value)
        .map_err(|_| invalid_argument("string output must not contain interior nul bytes"))?;
    let len = value.as_bytes().len();
    let ptr = value.into_raw();
    unsafe {
        *target = e2v_string_t { ptr, len };
    }
    Ok(())
}

fn write_owned_bytes(target: *mut e2v_bytes_t, value: Vec<u8>) -> crate::SdkResult<()> {
    if target.is_null() {
        return Err(invalid_argument("bytes output pointer must not be null"));
    }
    let len = value.len();
    let boxed = value.into_boxed_slice();
    let ptr = Box::into_raw(boxed) as *mut u8;
    unsafe {
        *target = e2v_bytes_t {
            ptr: ptr.cast_const(),
            len,
        };
    }
    Ok(())
}

fn ffi_read_bytes(
    ptr: *const u8,
    len: usize,
    argument_name: &str,
) -> crate::SdkResult<Vec<u8>> {
    if ptr.is_null() {
        return Err(invalid_argument(format!("{argument_name} must not be null")));
    }
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    Ok(bytes.to_vec())
}

unsafe fn sdk_ref<'a>(sdk: *mut e2v_sdk_t) -> crate::SdkResult<&'a Sdk> {
    unsafe { sdk.as_ref() }
        .map(|handle| &handle.inner)
        .ok_or_else(|| invalid_argument("sdk handle must not be null"))
}

unsafe fn read_handle_ref<'a>(handle: *mut e2v_read_handle_t) -> crate::SdkResult<&'a ReadHandle> {
    unsafe { handle.as_ref() }
        .map(|handle| &handle.inner)
        .ok_or_else(|| invalid_argument("read handle must not be null"))
}

unsafe fn snapshot_view_ref<'a>(
    handle: *mut e2v_snapshot_view_t,
) -> crate::SdkResult<&'a SnapshotView> {
    unsafe { handle.as_ref() }
        .map(|handle| &handle.inner)
        .ok_or_else(|| invalid_argument("snapshot view handle must not be null"))
}

unsafe fn file_view_ref<'a>(handle: *mut e2v_file_view_t) -> crate::SdkResult<&'a crate::FileView> {
    unsafe { handle.as_ref() }
        .map(|handle| &handle.inner)
        .ok_or_else(|| invalid_argument("file view handle must not be null"))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_sdk_new(
    sdk_out: *mut *mut e2v_sdk_t,
    error_out: *mut *mut e2v_error_t,
) -> e2v_error_code_t {
    if sdk_out.is_null() {
        return set_error_out(
            error_out,
            invalid_argument("sdk output pointer must not be null"),
        );
    }
    unsafe {
        *sdk_out = ptr::null_mut();
    }
    ffi_call(error_out, || {
        unsafe {
            *sdk_out = Box::into_raw(Box::new(e2v_sdk_t { inner: Sdk::new() }));
        }
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_sdk_free(handle: *mut e2v_sdk_t) {
    if !handle.is_null() {
        unsafe {
            drop(Box::from_raw(handle));
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_read_handle_free(handle: *mut e2v_read_handle_t) {
    if !handle.is_null() {
        unsafe {
            drop(Box::from_raw(handle));
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_snapshot_view_free(handle: *mut e2v_snapshot_view_t) {
    if !handle.is_null() {
        unsafe {
            drop(Box::from_raw(handle));
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_file_view_free(handle: *mut e2v_file_view_t) {
    if !handle.is_null() {
        unsafe {
            drop(Box::from_raw(handle));
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_error_free(handle: *mut e2v_error_t) {
    if !handle.is_null() {
        unsafe {
            drop(Box::from_raw(handle));
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_error_code(handle: *mut e2v_error_t) -> e2v_error_code_t {
    match unsafe { handle.as_ref() } {
        Some(error) => error.code,
        None => E2V_INVALID_ARGUMENT,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_error_message(
    handle: *mut e2v_error_t,
    message_out: *mut e2v_string_t,
) -> e2v_error_code_t {
    if message_out.is_null() {
        return E2V_INVALID_ARGUMENT;
    }
    unsafe {
        *message_out = e2v_string_t::default();
    }
    match unsafe { handle.as_ref() } {
        Some(error) => ffi_call(ptr::null_mut(), || {
            write_owned_string(message_out, error.message.clone())
        }),
        None => E2V_INVALID_ARGUMENT,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_string_free(value: *mut e2v_string_t) {
    if value.is_null() {
        return;
    }
    let owned = unsafe { &mut *value };
    if !owned.ptr.is_null() {
        unsafe {
            drop(CString::from_raw(owned.ptr.cast_mut()));
        }
    }
    *owned = e2v_string_t::default();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_bytes_free(value: *mut e2v_bytes_t) {
    if value.is_null() {
        return;
    }
    let owned = unsafe { &mut *value };
    if !owned.ptr.is_null() {
        let slice_ptr = ptr::slice_from_raw_parts_mut(owned.ptr.cast_mut(), owned.len);
        unsafe {
            drop(Box::from_raw(slice_ptr));
        }
    }
    *owned = e2v_bytes_t::default();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_test_only_force_panic(
    error_out: *mut *mut e2v_error_t,
) -> e2v_error_code_t {
    ffi_call(error_out, || -> crate::SdkResult<()> {
        panic!("forced ffi panic");
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_init_repository_json(
    sdk: *mut e2v_sdk_t,
    repo_root: *const c_char,
    password: *const c_char,
    branch_name: *const c_char,
    json_out: *mut e2v_string_t,
    error_out: *mut *mut e2v_error_t,
) -> e2v_error_code_t {
    ffi_call_with_json(json_out, error_out, || {
        let sdk = unsafe { sdk_ref(sdk)? };
        sdk.init_repository(InitRepositoryOptions {
            repo_root: ffi_read_c_string(repo_root, "repo_root")?.into(),
            password: ffi_read_c_string(password, "password")?,
            branch_name: ffi_read_c_string(branch_name, "branch_name")?,
        })
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_open_repository_json(
    sdk: *mut e2v_sdk_t,
    repo_root: *const c_char,
    json_out: *mut e2v_string_t,
    error_out: *mut *mut e2v_error_t,
) -> e2v_error_code_t {
    ffi_call_with_json(json_out, error_out, || {
        let sdk = unsafe { sdk_ref(sdk)? };
        sdk.open_repository(ffi_read_c_string(repo_root, "repo_root")?)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_commit_repository_json(
    sdk: *mut e2v_sdk_t,
    repo_root: *const c_char,
    message: *const c_char,
    json_out: *mut e2v_string_t,
    error_out: *mut *mut e2v_error_t,
) -> e2v_error_code_t {
    ffi_call_with_json(json_out, error_out, || {
        let sdk = unsafe { sdk_ref(sdk)? };
        sdk.commit_repository(CommitRepositoryOptions {
            repo_root: ffi_read_c_string(repo_root, "repo_root")?.into(),
            message: ffi_read_c_string(message, "message")?,
        })
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_open_read_handle(
    sdk: *mut e2v_sdk_t,
    repo_root: *const c_char,
    handle_out: *mut *mut e2v_read_handle_t,
    error_out: *mut *mut e2v_error_t,
) -> e2v_error_code_t {
    ffi_call_with_handle(
        handle_out,
        error_out,
        |inner| e2v_read_handle_t { inner },
        || {
            let sdk = unsafe { sdk_ref(sdk)? };
            sdk.open_read_handle(ffi_read_c_string(repo_root, "repo_root")?)
        },
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_unlock_repository_json(
    sdk: *mut e2v_sdk_t,
    repo_root: *const c_char,
    password: *const c_char,
    json_out: *mut e2v_string_t,
    error_out: *mut *mut e2v_error_t,
) -> e2v_error_code_t {
    ffi_call_with_json(json_out, error_out, || {
        let sdk = unsafe { sdk_ref(sdk)? };
        sdk.unlock_repository(
            ffi_read_c_string(repo_root, "repo_root")?,
            &ffi_read_c_string(password, "password")?,
        )
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_list_snapshots_json(
    sdk: *mut e2v_sdk_t,
    repo_root: *const c_char,
    json_out: *mut e2v_string_t,
    error_out: *mut *mut e2v_error_t,
) -> e2v_error_code_t {
    ffi_call_with_json(json_out, error_out, || {
        let sdk = unsafe { sdk_ref(sdk)? };
        sdk.list_snapshots(ffi_read_c_string(repo_root, "repo_root")?)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_verify_snapshot(
    sdk: *mut e2v_sdk_t,
    repo_root: *const c_char,
    snapshot_id: *const c_char,
    error_out: *mut *mut e2v_error_t,
) -> e2v_error_code_t {
    ffi_call(error_out, || {
        let sdk = unsafe { sdk_ref(sdk)? };
        sdk.verify_snapshot(
            ffi_read_c_string(repo_root, "repo_root")?,
            &ffi_read_c_string(snapshot_id, "snapshot_id")?,
        )
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_checkout_snapshot(
    sdk: *mut e2v_sdk_t,
    repo_root: *const c_char,
    snapshot_id: *const c_char,
    target_dir: *const c_char,
    error_out: *mut *mut e2v_error_t,
) -> e2v_error_code_t {
    ffi_call(error_out, || {
        let sdk = unsafe { sdk_ref(sdk)? };
        sdk.checkout_snapshot(crate::CheckoutSnapshotOptions {
            repo_root: ffi_read_c_string(repo_root, "repo_root")?.into(),
            snapshot_id: ffi_read_c_string(snapshot_id, "snapshot_id")?,
            target_dir: ffi_read_c_string(target_dir, "target_dir")?.into(),
        })
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_change_password(
    sdk: *mut e2v_sdk_t,
    repo_root: *const c_char,
    old_password: *const c_char,
    new_password: *const c_char,
    error_out: *mut *mut e2v_error_t,
) -> e2v_error_code_t {
    ffi_call(error_out, || {
        let sdk = unsafe { sdk_ref(sdk)? };
        sdk.change_password(
            ffi_read_c_string(repo_root, "repo_root")?,
            &ffi_read_c_string(old_password, "old_password")?,
            &ffi_read_c_string(new_password, "new_password")?,
        )
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_create_branch_json(
    sdk: *mut e2v_sdk_t,
    repo_root: *const c_char,
    branch_name: *const c_char,
    json_out: *mut e2v_string_t,
    error_out: *mut *mut e2v_error_t,
) -> e2v_error_code_t {
    ffi_call_with_json(json_out, error_out, || {
        let sdk = unsafe { sdk_ref(sdk)? };
        sdk.create_branch(
            ffi_read_c_string(repo_root, "repo_root")?,
            &ffi_read_c_string(branch_name, "branch_name")?,
        )
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_list_branches_json(
    sdk: *mut e2v_sdk_t,
    repo_root: *const c_char,
    json_out: *mut e2v_string_t,
    error_out: *mut *mut e2v_error_t,
) -> e2v_error_code_t {
    ffi_call_with_json(json_out, error_out, || {
        let sdk = unsafe { sdk_ref(sdk)? };
        sdk.list_branches(ffi_read_c_string(repo_root, "repo_root")?)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_checkout_branch_json(
    sdk: *mut e2v_sdk_t,
    repo_root: *const c_char,
    branch_name: *const c_char,
    json_out: *mut e2v_string_t,
    error_out: *mut *mut e2v_error_t,
) -> e2v_error_code_t {
    ffi_call_with_json(json_out, error_out, || {
        let sdk = unsafe { sdk_ref(sdk)? };
        sdk.checkout_branch(
            ffi_read_c_string(repo_root, "repo_root")?,
            &ffi_read_c_string(branch_name, "branch_name")?,
        )
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_delete_branch(
    sdk: *mut e2v_sdk_t,
    repo_root: *const c_char,
    branch_name: *const c_char,
    error_out: *mut *mut e2v_error_t,
) -> e2v_error_code_t {
    ffi_call(error_out, || {
        let sdk = unsafe { sdk_ref(sdk)? };
        sdk.delete_branch(
            ffi_read_c_string(repo_root, "repo_root")?,
            &ffi_read_c_string(branch_name, "branch_name")?,
        )
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_open_snapshot(
    read_handle: *mut e2v_read_handle_t,
    snapshot_id: *const c_char,
    handle_out: *mut *mut e2v_snapshot_view_t,
    error_out: *mut *mut e2v_error_t,
) -> e2v_error_code_t {
    ffi_call_with_handle(
        handle_out,
        error_out,
        |inner| e2v_snapshot_view_t { inner },
        || {
            let read = unsafe { read_handle_ref(read_handle)? };
            read.open_snapshot(&ffi_read_c_string(snapshot_id, "snapshot_id")?)
        },
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_resolve_branch(
    read_handle: *mut e2v_read_handle_t,
    branch_token: *const c_char,
    handle_out: *mut *mut e2v_snapshot_view_t,
    error_out: *mut *mut e2v_error_t,
) -> e2v_error_code_t {
    ffi_call_with_handle(
        handle_out,
        error_out,
        |inner| e2v_snapshot_view_t { inner },
        || {
            let read = unsafe { read_handle_ref(read_handle)? };
            read.resolve_branch(&ffi_read_c_string(branch_token, "branch_token")?)
        },
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_open_file(
    read_handle: *mut e2v_read_handle_t,
    snapshot: *mut e2v_snapshot_view_t,
    path: *const c_char,
    handle_out: *mut *mut e2v_file_view_t,
    error_out: *mut *mut e2v_error_t,
) -> e2v_error_code_t {
    ffi_call_with_handle(
        handle_out,
        error_out,
        |inner| e2v_file_view_t { inner },
        || {
            let read = unsafe { read_handle_ref(read_handle)? };
            let snapshot = unsafe { snapshot_view_ref(snapshot)? };
            read.open_file(snapshot, &ffi_read_c_string(path, "path")?)
        },
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_read_dir_json(
    read_handle: *mut e2v_read_handle_t,
    snapshot: *mut e2v_snapshot_view_t,
    path: *const c_char,
    json_out: *mut e2v_string_t,
    error_out: *mut *mut e2v_error_t,
) -> e2v_error_code_t {
    ffi_call_with_json(json_out, error_out, || {
        let read = unsafe { read_handle_ref(read_handle)? };
        let snapshot = unsafe { snapshot_view_ref(snapshot)? };
        read.read_dir(snapshot, &ffi_read_c_string(path, "path")?)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_read_range(
    read_handle: *mut e2v_read_handle_t,
    file: *mut e2v_file_view_t,
    offset: usize,
    length: usize,
    bytes_out: *mut e2v_bytes_t,
    error_out: *mut *mut e2v_error_t,
) -> e2v_error_code_t {
    if bytes_out.is_null() {
        return set_error_out(
            error_out,
            invalid_argument("bytes output pointer must not be null"),
        );
    }
    unsafe {
        *bytes_out = e2v_bytes_t::default();
    }

    ffi_call(error_out, || {
        let read = unsafe { read_handle_ref(read_handle)? };
        let file = unsafe { file_view_ref(file)? };
        let bytes = read.read_range(file, offset, length)?;
        write_owned_bytes(bytes_out, bytes)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_parse_remote_spec_json(
    spec: *const c_char,
    json_out: *mut e2v_string_t,
    error_out: *mut *mut e2v_error_t,
) -> e2v_error_code_t {
    ffi_call_with_json(json_out, error_out, || {
        crate::parse_remote_spec(&ffi_read_c_string(spec, "spec")?)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_share_list_json(
    sdk: *mut e2v_sdk_t,
    repo_root: *const c_char,
    json_out: *mut e2v_string_t,
    error_out: *mut *mut e2v_error_t,
) -> e2v_error_code_t {
    ffi_call_with_json(json_out, error_out, || {
        let sdk = unsafe { sdk_ref(sdk)? };
        sdk.share_list(ffi_read_c_string(repo_root, "repo_root")?)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_share_invite_member_json(
    sdk: *mut e2v_sdk_t,
    repo_root: *const c_char,
    display_name: *const c_char,
    json_out: *mut e2v_string_t,
    error_out: *mut *mut e2v_error_t,
) -> e2v_error_code_t {
    ffi_call_with_json(json_out, error_out, || {
        let sdk = unsafe { sdk_ref(sdk)? };
        sdk.share_invite_member(
            ffi_read_c_string(repo_root, "repo_root")?,
            ShareInviteMemberRequest {
                display_name: ffi_read_c_string(display_name, "display_name")?,
            },
        )
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_share_accept_member_json(
    sdk: *mut e2v_sdk_t,
    repo_root: *const c_char,
    invite_bytes: *const u8,
    invite_len: usize,
    local_device_label: *const c_char,
    json_out: *mut e2v_string_t,
    error_out: *mut *mut e2v_error_t,
) -> e2v_error_code_t {
    ffi_call_with_json(json_out, error_out, || {
        let sdk = unsafe { sdk_ref(sdk)? };
        sdk.share_accept_member(
            ffi_read_c_string(repo_root, "repo_root")?,
            ShareAcceptMemberRequest {
                invite_bytes: ffi_read_bytes(invite_bytes, invite_len, "invite_bytes")?,
                local_device_label: ffi_read_c_string(
                    local_device_label,
                    "local_device_label",
                )?,
            },
        )
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_share_invite_device_json(
    sdk: *mut e2v_sdk_t,
    repo_root: *const c_char,
    actor_id: *const c_char,
    device_label: *const c_char,
    json_out: *mut e2v_string_t,
    error_out: *mut *mut e2v_error_t,
) -> e2v_error_code_t {
    ffi_call_with_json(json_out, error_out, || {
        let sdk = unsafe { sdk_ref(sdk)? };
        sdk.share_invite_device(
            ffi_read_c_string(repo_root, "repo_root")?,
            ShareInviteDeviceRequest {
                actor_id: ffi_read_c_string(actor_id, "actor_id")?,
                device_label: ffi_read_c_string(device_label, "device_label")?,
            },
        )
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_share_accept_device_json(
    sdk: *mut e2v_sdk_t,
    repo_root: *const c_char,
    invite_bytes: *const u8,
    invite_len: usize,
    local_device_label: *const c_char,
    json_out: *mut e2v_string_t,
    error_out: *mut *mut e2v_error_t,
) -> e2v_error_code_t {
    ffi_call_with_json(json_out, error_out, || {
        let sdk = unsafe { sdk_ref(sdk)? };
        sdk.share_accept_device(
            ffi_read_c_string(repo_root, "repo_root")?,
            ShareAcceptDeviceRequest {
                invite_bytes: ffi_read_bytes(invite_bytes, invite_len, "invite_bytes")?,
                local_device_label: ffi_read_c_string(
                    local_device_label,
                    "local_device_label",
                )?,
            },
        )
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_share_revoke_device(
    sdk: *mut e2v_sdk_t,
    repo_root: *const c_char,
    device_id: *const c_char,
    password: *const c_char,
    error_out: *mut *mut e2v_error_t,
) -> e2v_error_code_t {
    ffi_call(error_out, || {
        let sdk = unsafe { sdk_ref(sdk)? };
        sdk.share_revoke_device(
            ffi_read_c_string(repo_root, "repo_root")?,
            ShareRevokeDeviceRequest {
                device_id: ffi_read_c_string(device_id, "device_id")?,
                password: ffi_read_c_string(password, "password")?,
            },
        )
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_share_revoke_member(
    sdk: *mut e2v_sdk_t,
    repo_root: *const c_char,
    actor_id: *const c_char,
    password: *const c_char,
    error_out: *mut *mut e2v_error_t,
) -> e2v_error_code_t {
    ffi_call(error_out, || {
        let sdk = unsafe { sdk_ref(sdk)? };
        sdk.share_revoke_member(
            ffi_read_c_string(repo_root, "repo_root")?,
            ShareRevokeMemberRequest {
                actor_id: ffi_read_c_string(actor_id, "actor_id")?,
                password: ffi_read_c_string(password, "password")?,
            },
        )
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_add_remote_json(
    sdk: *mut e2v_sdk_t,
    repo_root: *const c_char,
    name: *const c_char,
    spec: *const c_char,
    json_out: *mut e2v_string_t,
    error_out: *mut *mut e2v_error_t,
) -> e2v_error_code_t {
    ffi_call_with_json(json_out, error_out, || {
        let sdk = unsafe { sdk_ref(sdk)? };
        sdk.add_remote(
            ffi_read_c_string(repo_root, "repo_root")?,
            &ffi_read_c_string(name, "name")?,
            &ffi_read_c_string(spec, "spec")?,
        )
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_load_default_remote_json(
    sdk: *mut e2v_sdk_t,
    repo_root: *const c_char,
    json_out: *mut e2v_string_t,
    error_out: *mut *mut e2v_error_t,
) -> e2v_error_code_t {
    ffi_call_with_json(json_out, error_out, || {
        let sdk = unsafe { sdk_ref(sdk)? };
        sdk.load_default_remote(ffi_read_c_string(repo_root, "repo_root")?)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_push_default_remote_json(
    sdk: *mut e2v_sdk_t,
    repo_root: *const c_char,
    branch_token: *const c_char,
    operation_id: *const c_char,
    json_out: *mut e2v_string_t,
    error_out: *mut *mut e2v_error_t,
) -> e2v_error_code_t {
    ffi_call_with_json(json_out, error_out, || {
        let sdk = unsafe { sdk_ref(sdk)? };
        sdk.push_default_remote(PushRequest {
            repo_root: ffi_read_c_string(repo_root, "repo_root")?.into(),
            branch_token: ffi_read_c_string(branch_token, "branch_token")?,
            operation_id: ffi_read_c_string(operation_id, "operation_id")?,
        })
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_fetch_default_remote_json(
    sdk: *mut e2v_sdk_t,
    repo_root: *const c_char,
    branch_token: *const c_char,
    password: *const c_char,
    json_out: *mut e2v_string_t,
    error_out: *mut *mut e2v_error_t,
) -> e2v_error_code_t {
    ffi_call_with_json(json_out, error_out, || {
        let sdk = unsafe { sdk_ref(sdk)? };
        let password = if password.is_null() {
            None
        } else {
            Some(ffi_read_c_string(password, "password")?)
        };
        sdk.fetch_default_remote(FetchRequest {
            repo_root: ffi_read_c_string(repo_root, "repo_root")?.into(),
            branch_token: ffi_read_c_string(branch_token, "branch_token")?,
            password,
        })
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_clone_remote_json(
    sdk: *mut e2v_sdk_t,
    remote_spec: *const c_char,
    target_repo_root: *const c_char,
    password: *const c_char,
    branch_token: *const c_char,
    json_out: *mut e2v_string_t,
    error_out: *mut *mut e2v_error_t,
) -> e2v_error_code_t {
    ffi_call_with_json(json_out, error_out, || {
        let sdk = unsafe { sdk_ref(sdk)? };
        sdk.clone_remote(CloneRequest {
            remote_spec: ffi_read_c_string(remote_spec, "remote_spec")?,
            target_repo_root: ffi_read_c_string(target_repo_root, "target_repo_root")?.into(),
            password: ffi_read_c_string(password, "password")?,
            branch_token: ffi_read_c_string(branch_token, "branch_token")?,
        })
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_verify_default_remote_json(
    sdk: *mut e2v_sdk_t,
    repo_root: *const c_char,
    sample_percent: u8,
    json_out: *mut e2v_string_t,
    error_out: *mut *mut e2v_error_t,
) -> e2v_error_code_t {
    ffi_call_with_json(json_out, error_out, || {
        let sdk = unsafe { sdk_ref(sdk)? };
        sdk.verify_default_remote(VerifyRemoteRequest {
            repo_root: ffi_read_c_string(repo_root, "repo_root")?.into(),
            sample_percent,
        })
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_repair_default_remote_json(
    sdk: *mut e2v_sdk_t,
    repo_root: *const c_char,
    json_out: *mut e2v_string_t,
    error_out: *mut *mut e2v_error_t,
) -> e2v_error_code_t {
    ffi_call_with_json(json_out, error_out, || {
        let sdk = unsafe { sdk_ref(sdk)? };
        sdk.repair_default_remote(ffi_read_c_string(repo_root, "repo_root")?)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_force_accept_default_remote_rollback_json(
    sdk: *mut e2v_sdk_t,
    repo_root: *const c_char,
    password: *const c_char,
    json_out: *mut e2v_string_t,
    error_out: *mut *mut e2v_error_t,
) -> e2v_error_code_t {
    ffi_call_with_json(json_out, error_out, || {
        let sdk = unsafe { sdk_ref(sdk)? };
        sdk.force_accept_default_remote_rollback(
            ffi_read_c_string(repo_root, "repo_root")?,
            &ffi_read_c_string(password, "password")?,
        )
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_gc_default_remote_dry_run_json(
    sdk: *mut e2v_sdk_t,
    repo_root: *const c_char,
    json_out: *mut e2v_string_t,
    error_out: *mut *mut e2v_error_t,
) -> e2v_error_code_t {
    ffi_call_with_json(json_out, error_out, || {
        let sdk = unsafe { sdk_ref(sdk)? };
        sdk.gc_default_remote_dry_run(ffi_read_c_string(repo_root, "repo_root")?)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2v_gc_default_remote_execute_json(
    sdk: *mut e2v_sdk_t,
    repo_root: *const c_char,
    grace_period_days: u64,
    allow_single_writer_maintenance_window: bool,
    json_out: *mut e2v_string_t,
    error_out: *mut *mut e2v_error_t,
) -> e2v_error_code_t {
    ffi_call_with_json(json_out, error_out, || {
        let sdk = unsafe { sdk_ref(sdk)? };
        sdk.gc_default_remote_execute(GcExecuteRequest {
            repo_root: ffi_read_c_string(repo_root, "repo_root")?.into(),
            grace_period_days,
            allow_single_writer_maintenance_window,
        })
    })
}

pub fn header_text() -> String {
    include_str!("../include/e2v_api.h").to_string()
}
