use std::fmt;
use std::path::{Path, PathBuf};

pub mod c_abi;

use e2v_core::{
    BranchSummary, CheckoutOptions, CommitOptions, CommitResult, DirectoryEntry, FileHandle,
    InitOptions, ReadService, RepositoryFacade, RepositoryState, ShareAcceptDeviceOptions,
    ShareAcceptMemberOptions, ShareAcceptResult, ShareInviteBundle, ShareInviteDeviceOptions,
    ShareInviteMemberOptions, ShareListResult, ShareRevokeDeviceOptions, ShareRevokeMemberOptions,
    SnapshotHandle, SnapshotSummary,
};
use e2v_sync::{
    CloneOptions, EnableObliviousLayoutOptions, FetchOptions, GcDryRunOptions, GcExecuteOptions,
    HistoricalRewriteOptions, HistoricalRewritePlanOptions, PushOptions, RemoteSpec,
    RepairRemoteOptions, ReshuffleObliviousLayoutOptions, VerifyRemoteOptions,
};
use serde::{Deserialize, Serialize};

const REMOTES_DIR: &str = ".e2v/remotes";
const DEFAULT_REMOTE_PATH: &str = ".e2v/remotes/default.json";

macro_rules! with_remote_backend {
    ($remote_spec:expr, |$backend:ident| $body:expr) => {
        $remote_spec.with_backend(|remote| match remote {
            e2v_sync::RemoteBackendRef::LocalFolder($backend) => $body,
            e2v_sync::RemoteBackendRef::S3($backend) => $body,
            e2v_sync::RemoteBackendRef::Webdav($backend) => $body,
        })
    };
}

pub type SdkResult<T> = std::result::Result<T, SdkError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SdkErrorCode {
    InvalidArgument,
    NotFound,
    AlreadyExists,
    PermissionDenied,
    AuthenticationRequired,
    Conflict,
    NeedsRebase,
    RollbackDetected,
    Unsupported,
    CorruptState,
    Io,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SdkError {
    code: SdkErrorCode,
    message: String,
}

impl SdkError {
    pub fn new(code: SdkErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn code(&self) -> SdkErrorCode {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for SdkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for SdkError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitRepositoryOptions {
    pub repo_root: PathBuf,
    pub password: String,
    pub branch_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRepositoryOptions {
    pub repo_root: PathBuf,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutSnapshotOptions {
    pub repo_root: PathBuf,
    pub snapshot_id: String,
    pub target_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushRequest {
    pub repo_root: PathBuf,
    pub branch_token: String,
    pub operation_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchRequest {
    pub repo_root: PathBuf,
    pub branch_token: String,
    pub password: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloneRequest {
    pub remote_spec: String,
    pub target_repo_root: PathBuf,
    pub password: String,
    pub branch_token: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequest {
    pub repo_root: PathBuf,
    pub branch_token: String,
    pub password: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyRemoteRequest {
    pub repo_root: PathBuf,
    pub sample_percent: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoricalRewritePlanRequest {
    pub repo_root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoricalRewriteExecuteRequest {
    pub repo_root: PathBuf,
    pub password: String,
    pub confirm_full_reencryption: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObliviousLayoutPlanRequest {
    pub repo_root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnableObliviousLayoutRequest {
    pub repo_root: PathBuf,
    pub policy_profile: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReshuffleObliviousLayoutRequest {
    pub repo_root: PathBuf,
    pub policy_profile: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteRegistration {
    pub name: String,
    pub spec: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchInfo {
    pub name: String,
    pub token_hex: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryInfo {
    pub repo_root: PathBuf,
    pub branch: BranchInfo,
    pub layout_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitInfo {
    pub snapshot_id: String,
    pub committed_files: usize,
    pub new_bytes: u64,
    pub reused_bytes: u64,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotInfo {
    pub snapshot_id: String,
    pub message: String,
    pub parent_snapshot_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchSummaryInfo {
    pub name: String,
    pub token_hex: String,
    pub head_snapshot_id: Option<String>,
    pub is_current: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareActorInfo {
    pub actor_id: String,
    pub display_name: String,
    pub role: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareDeviceInfo {
    pub device_id: String,
    pub actor_id: String,
    pub label: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareListInfo {
    pub actors: Vec<ShareActorInfo>,
    pub devices: Vec<ShareDeviceInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareInviteMemberRequest {
    pub display_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareAcceptMemberRequest {
    pub invite_bytes: Vec<u8>,
    pub local_device_label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareInviteDeviceRequest {
    pub actor_id: String,
    pub device_label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareAcceptDeviceRequest {
    pub invite_bytes: Vec<u8>,
    pub local_device_label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareRevokeDeviceRequest {
    pub device_id: String,
    pub password: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareRevokeMemberRequest {
    pub actor_id: String,
    pub password: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareInviteInfo {
    pub actor_id: String,
    pub device_id: String,
    pub bundle_bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareAcceptInfo {
    pub actor_id: String,
    pub device_id: String,
    pub role: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryEntryInfo {
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SnapshotView {
    pub snapshot_id: String,
    pub layout_generation: u64,
    pub branch_token: Option<String>,
    #[serde(skip)]
    inner: SnapshotHandle,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileView {
    pub snapshot_id: String,
    pub file_object_id: String,
    pub file_size: u64,
    pub chunk_count: usize,
    pub layout_generation: u64,
    pub crypto_suite: String,
    pub key_epoch: u32,
    pub chunker_id: String,
    #[serde(skip)]
    inner: FileHandle,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushResponse {
    pub published_snapshot_id: String,
    pub uploaded_objects: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchResponse {
    pub downloaded_objects: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloneResponse {
    pub branch_token: String,
    pub head_snapshot_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullResponse {
    pub snapshot_id: String,
    pub fast_forward_applied: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyRemoteResponse {
    pub sampled_objects: usize,
    pub repaired_local_objects: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepairRemoteResponse {
    pub repaired_objects: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GcDryRunResponse {
    pub unreachable_physical_refs: Vec<String>,
    pub active_intent_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GcExecuteRequest {
    pub repo_root: PathBuf,
    pub grace_period_days: u64,
    pub allow_single_writer_maintenance_window: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GcExecuteResponse {
    pub deleted_physical_refs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoricalRewritePlanResponse {
    pub reachable_object_count: usize,
    pub remote_loose_object_count: usize,
    pub remote_pack_object_count: usize,
    pub old_epoch_count: usize,
    pub large_repo_advisory: Option<String>,
    pub requires_remote_credential_revocation_guidance: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoricalRewriteExecuteResponse {
    pub rewritten_objects: usize,
    pub retired_epoch_count: usize,
    pub deleted_stale_remote_refs: Vec<String>,
    pub next_layout_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObliviousLayoutPlanResponse {
    pub estimated_real_reads_per_request: u8,
    pub estimated_cover_reads_per_request: u8,
    pub estimated_bytes_per_request: u64,
    pub estimated_write_amplification: u8,
    pub requires_layout_root_rewrite: bool,
    pub advisory_messages: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObliviousLayoutStatusResponse {
    pub layout_mode: String,
    pub dedup_mode: String,
    pub layout_generation: u64,
    pub oblivious_generation: Option<u64>,
    pub policy_profile: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedRemoteSpec {
    raw: String,
}

impl ParsedRemoteSpec {
    pub fn as_str(&self) -> &str {
        &self.raw
    }
}

#[derive(Debug, Clone)]
pub struct Sdk {
    facade: RepositoryFacade,
}

impl Default for Sdk {
    fn default() -> Self {
        Self::new()
    }
}

impl Sdk {
    pub fn new() -> Self {
        Self {
            facade: RepositoryFacade::new(),
        }
    }

    pub fn init_repository(&self, options: InitRepositoryOptions) -> SdkResult<RepositoryInfo> {
        self.facade
            .init(InitOptions {
                repo_root: options.repo_root,
                password: options.password,
                branch_name: options.branch_name,
            })
            .map(repository_info_from_state)
            .map_err(map_error)
    }

    pub fn open_repository(&self, repo_root: impl AsRef<Path>) -> SdkResult<RepositoryInfo> {
        self.facade
            .open(repo_root)
            .map(repository_info_from_state)
            .map_err(map_error)
    }

    pub fn unlock_repository(
        &self,
        repo_root: impl AsRef<Path>,
        password: &str,
    ) -> SdkResult<RepositoryInfo> {
        self.facade
            .unlock(repo_root, password)
            .map(repository_info_from_state)
            .map_err(map_error)
    }

    pub fn commit_repository(&self, options: CommitRepositoryOptions) -> SdkResult<CommitInfo> {
        self.facade
            .commit(CommitOptions {
                repo_root: options.repo_root,
                message: options.message,
            })
            .map(commit_info_from_result)
            .map_err(map_error)
    }

    pub fn checkout_snapshot(&self, options: CheckoutSnapshotOptions) -> SdkResult<()> {
        self.facade
            .checkout(CheckoutOptions {
                repo_root: options.repo_root,
                snapshot_id: options.snapshot_id,
                target_dir: options.target_dir,
            })
            .map_err(map_error)
    }

    pub fn list_snapshots(&self, repo_root: impl AsRef<Path>) -> SdkResult<Vec<SnapshotInfo>> {
        self.facade
            .snapshots(repo_root)
            .map(|items| items.into_iter().map(snapshot_info_from_summary).collect())
            .map_err(map_error)
    }

    pub fn verify_snapshot(&self, repo_root: impl AsRef<Path>, snapshot_id: &str) -> SdkResult<()> {
        self.facade
            .verify_snapshot(repo_root, snapshot_id)
            .map_err(map_error)
    }

    pub fn change_password(
        &self,
        repo_root: impl AsRef<Path>,
        old_password: &str,
        new_password: &str,
    ) -> SdkResult<()> {
        self.facade
            .change_password(repo_root, old_password, new_password)
            .map_err(map_error)
    }

    pub fn create_branch(
        &self,
        repo_root: impl AsRef<Path>,
        branch_name: &str,
    ) -> SdkResult<BranchInfo> {
        self.facade
            .create_branch(repo_root, branch_name)
            .map(branch_info_from_state)
            .map_err(map_error)
    }

    pub fn list_branches(&self, repo_root: impl AsRef<Path>) -> SdkResult<Vec<BranchSummaryInfo>> {
        self.facade
            .list_branches(repo_root)
            .map(|items| {
                items
                    .into_iter()
                    .map(branch_summary_info_from_summary)
                    .collect()
            })
            .map_err(map_error)
    }

    pub fn checkout_branch(
        &self,
        repo_root: impl AsRef<Path>,
        branch_name: &str,
    ) -> SdkResult<RepositoryInfo> {
        self.facade
            .checkout_branch(repo_root, branch_name)
            .map(repository_info_from_state)
            .map_err(map_error)
    }

    pub fn delete_branch(&self, repo_root: impl AsRef<Path>, branch_name: &str) -> SdkResult<()> {
        self.facade
            .delete_branch(repo_root, branch_name)
            .map_err(map_error)
    }

    pub fn share_list(&self, repo_root: impl AsRef<Path>) -> SdkResult<ShareListInfo> {
        self.facade
            .share_list(repo_root)
            .map(share_list_info_from_result)
            .map_err(map_error)
    }

    pub fn share_invite_member(
        &self,
        repo_root: impl AsRef<Path>,
        request: ShareInviteMemberRequest,
    ) -> SdkResult<ShareInviteInfo> {
        self.facade
            .share_invite_member(
                repo_root,
                ShareInviteMemberOptions {
                    display_name: request.display_name,
                },
            )
            .map(share_invite_info_from_bundle)
            .map_err(map_error)
    }

    pub fn share_accept_member(
        &self,
        repo_root: impl AsRef<Path>,
        request: ShareAcceptMemberRequest,
    ) -> SdkResult<ShareAcceptInfo> {
        self.facade
            .share_accept_member(
                repo_root,
                ShareAcceptMemberOptions {
                    invite_bytes: request.invite_bytes,
                    local_device_label: request.local_device_label,
                },
            )
            .map(share_accept_info_from_result)
            .map_err(map_error)
    }

    pub fn share_invite_device(
        &self,
        repo_root: impl AsRef<Path>,
        request: ShareInviteDeviceRequest,
    ) -> SdkResult<ShareInviteInfo> {
        self.facade
            .share_invite_device(
                repo_root,
                ShareInviteDeviceOptions {
                    actor_id: request.actor_id,
                    device_label: request.device_label,
                },
            )
            .map(share_invite_info_from_bundle)
            .map_err(map_error)
    }

    pub fn share_accept_device(
        &self,
        repo_root: impl AsRef<Path>,
        request: ShareAcceptDeviceRequest,
    ) -> SdkResult<ShareAcceptInfo> {
        self.facade
            .share_accept_device(
                repo_root,
                ShareAcceptDeviceOptions {
                    invite_bytes: request.invite_bytes,
                    local_device_label: request.local_device_label,
                },
            )
            .map(share_accept_info_from_result)
            .map_err(map_error)
    }

    pub fn share_revoke_device(
        &self,
        repo_root: impl AsRef<Path>,
        request: ShareRevokeDeviceRequest,
    ) -> SdkResult<()> {
        self.facade
            .share_revoke_device(
                repo_root,
                ShareRevokeDeviceOptions {
                    device_id: request.device_id,
                    password: request.password,
                },
            )
            .map_err(map_error)
    }

    pub fn share_revoke_member(
        &self,
        repo_root: impl AsRef<Path>,
        request: ShareRevokeMemberRequest,
    ) -> SdkResult<()> {
        self.facade
            .share_revoke_member(
                repo_root,
                ShareRevokeMemberOptions {
                    actor_id: request.actor_id,
                    password: request.password,
                },
            )
            .map_err(map_error)
    }

    pub fn open_read_handle(&self, repo_root: impl AsRef<Path>) -> SdkResult<ReadHandle> {
        let read_service = self.facade.read_service(repo_root).map_err(map_error)?;
        Ok(ReadHandle { read_service })
    }

    pub fn add_remote(
        &self,
        repo_root: impl AsRef<Path>,
        name: &str,
        spec: &str,
    ) -> SdkResult<RemoteRegistration> {
        add_remote_registration(repo_root.as_ref(), name, spec)
    }

    pub fn load_default_remote(
        &self,
        repo_root: impl AsRef<Path>,
    ) -> SdkResult<RemoteRegistration> {
        load_default_remote_registration(repo_root.as_ref())
    }

    pub fn push_remote(&self, remote_spec: &str, request: PushRequest) -> SdkResult<PushResponse> {
        let remote_spec = RemoteSpec::parse(remote_spec).map_err(map_error)?;
        with_remote_backend!(&remote_spec, |backend| {
            e2v_sync::push_head(
                &self.facade,
                backend,
                PushOptions {
                    repo_root: request.repo_root,
                    branch_token: request.branch_token,
                    operation_id: request.operation_id,
                },
            )
        })
        .map(push_response_from_result)
        .map_err(map_error)
    }

    pub fn push_default_remote(&self, request: PushRequest) -> SdkResult<PushResponse> {
        let stored = self.load_default_remote(&request.repo_root)?;
        self.push_remote(&stored.spec, request)
    }

    pub fn fetch_remote(
        &self,
        remote_spec: &str,
        request: FetchRequest,
    ) -> SdkResult<FetchResponse> {
        let remote_spec = RemoteSpec::parse(remote_spec).map_err(map_error)?;
        with_remote_backend!(&remote_spec, |backend| {
            e2v_sync::fetch_remote(
                backend,
                FetchOptions {
                    repo_root: request.repo_root,
                    branch_token: request.branch_token,
                    password: request.password,
                },
            )
        })
        .map(fetch_response_from_result)
        .map_err(map_error)
    }

    pub fn fetch_default_remote(&self, request: FetchRequest) -> SdkResult<FetchResponse> {
        let stored = self.load_default_remote(&request.repo_root)?;
        self.fetch_remote(&stored.spec, request)
    }

    pub fn clone_remote(&self, request: CloneRequest) -> SdkResult<CloneResponse> {
        let remote_spec = RemoteSpec::parse(&request.remote_spec).map_err(map_error)?;
        with_remote_backend!(&remote_spec, |backend| {
            e2v_sync::clone_remote(
                backend,
                CloneOptions {
                    repo_root: request.target_repo_root,
                    password: request.password,
                    branch_token: request.branch_token,
                },
            )
        })
        .map(clone_response_from_result)
        .map_err(map_error)
    }

    pub fn pull_default_remote(&self, request: PullRequest) -> SdkResult<PullResponse> {
        let current_repo = self.facade.open(&request.repo_root).map_err(map_error)?;
        if current_repo.branch.token_hex != request.branch_token {
            return Err(map_error(anyhow::anyhow!(
                "current checked out branch does not match requested branch token"
            )));
        }
        let default_ref_path = request
            .repo_root
            .join(".e2v")
            .join("refs")
            .join("default.json");
        let branch_ref_path = request
            .repo_root
            .join(".e2v")
            .join("refs")
            .join("branches")
            .join(format!("{}.json", request.branch_token));
        let original_default_ref = match std::fs::read(&default_ref_path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(map_error(error.into())),
        };
        let original_branch_ref = match std::fs::read(&branch_ref_path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(map_error(error.into())),
        };
        let previous = self
            .facade
            .read_service(&request.repo_root)
            .map_err(map_error)?
            .resolve_branch(&request.branch_token)
            .ok()
            .map(|snapshot| snapshot.snapshot_id);

        self.fetch_default_remote(FetchRequest {
            repo_root: request.repo_root.clone(),
            branch_token: request.branch_token.clone(),
            password: request.password,
        })?;

        let fetched_snapshot_id = self
            .facade
            .read_service(&request.repo_root)
            .map_err(map_error)?
            .resolve_branch(&request.branch_token)
            .map(|snapshot| snapshot.snapshot_id)
            .map_err(map_error)?;

        restore_original_ref_file(&default_ref_path, original_default_ref.as_deref())
            .map_err(map_error)?;
        restore_original_ref_file(&branch_ref_path, original_branch_ref.as_deref())
            .map_err(map_error)?;

        if let Err(error) = self.facade.update_branch_head_if_fast_forward(
            &request.repo_root,
            &request.branch_token,
            Some(&fetched_snapshot_id),
        ) {
            let _ = restore_original_ref_file(&default_ref_path, original_default_ref.as_deref());
            let _ = restore_original_ref_file(&branch_ref_path, original_branch_ref.as_deref());
            let _ = self
                .facade
                .restore_default_ref_from_branch(&request.repo_root, &request.branch_token);
            return Err(map_error(error));
        }

        Ok(pull_response_from_snapshot(
            fetched_snapshot_id.clone(),
            previous.as_deref() != Some(fetched_snapshot_id.as_str()),
        ))
    }

    pub fn verify_remote(
        &self,
        remote_spec: &str,
        request: VerifyRemoteRequest,
    ) -> SdkResult<VerifyRemoteResponse> {
        let remote_spec = RemoteSpec::parse(remote_spec).map_err(map_error)?;
        with_remote_backend!(&remote_spec, |backend| {
            e2v_sync::verify_remote(
                backend,
                VerifyRemoteOptions {
                    repo_root: request.repo_root,
                    sample_percent: request.sample_percent,
                },
            )
        })
        .map(verify_remote_response_from_result)
        .map_err(map_error)
    }

    pub fn verify_default_remote(
        &self,
        request: VerifyRemoteRequest,
    ) -> SdkResult<VerifyRemoteResponse> {
        let stored = self.load_default_remote(&request.repo_root)?;
        self.verify_remote(&stored.spec, request)
    }

    pub fn historical_rewrite_remote_plan(
        &self,
        remote_spec: &str,
        request: HistoricalRewritePlanRequest,
    ) -> SdkResult<HistoricalRewritePlanResponse> {
        let remote_spec = RemoteSpec::parse(remote_spec).map_err(map_error)?;
        with_remote_backend!(&remote_spec, |backend| {
            e2v_sync::plan_historical_rewrite(
                backend,
                HistoricalRewritePlanOptions {
                    repo_root: request.repo_root.clone(),
                },
            )
        })
        .map(historical_rewrite_plan_response_from_result)
        .map_err(map_error)
    }

    pub fn historical_rewrite_default_remote_plan(
        &self,
        request: HistoricalRewritePlanRequest,
    ) -> SdkResult<HistoricalRewritePlanResponse> {
        let stored = self.load_default_remote(&request.repo_root)?;
        self.historical_rewrite_remote_plan(&stored.spec, request)
    }

    pub fn historical_rewrite_remote_execute(
        &self,
        remote_spec: &str,
        request: HistoricalRewriteExecuteRequest,
    ) -> SdkResult<HistoricalRewriteExecuteResponse> {
        let remote_spec = RemoteSpec::parse(remote_spec).map_err(map_error)?;
        with_remote_backend!(&remote_spec, |backend| {
            e2v_sync::historical_rewrite_remote(
                backend,
                HistoricalRewriteOptions {
                    repo_root: request.repo_root.clone(),
                    password: request.password.clone(),
                    confirm_full_reencryption: request.confirm_full_reencryption,
                },
            )
        })
        .map(historical_rewrite_execute_response_from_result)
        .map_err(map_error)
    }

    pub fn historical_rewrite_default_remote_execute(
        &self,
        request: HistoricalRewriteExecuteRequest,
    ) -> SdkResult<HistoricalRewriteExecuteResponse> {
        let stored = self.load_default_remote(&request.repo_root)?;
        self.historical_rewrite_remote_execute(&stored.spec, request)
    }

    pub fn oblivious_layout_remote_plan(
        &self,
        remote_spec: &str,
        request: ObliviousLayoutPlanRequest,
    ) -> SdkResult<ObliviousLayoutPlanResponse> {
        let remote_spec = RemoteSpec::parse(remote_spec).map_err(map_error)?;
        with_remote_backend!(&remote_spec, |backend| {
            e2v_sync::plan_oblivious_layout(backend, &request.repo_root)
        })
        .map(oblivious_layout_plan_response_from_result)
        .map_err(map_error)
    }

    pub fn oblivious_layout_default_remote_plan(
        &self,
        request: ObliviousLayoutPlanRequest,
    ) -> SdkResult<ObliviousLayoutPlanResponse> {
        let stored = self.load_default_remote(&request.repo_root)?;
        self.oblivious_layout_remote_plan(&stored.spec, request)
    }

    pub fn oblivious_layout_remote_status(
        &self,
        remote_spec: &str,
        repo_root: impl AsRef<Path>,
    ) -> SdkResult<ObliviousLayoutStatusResponse> {
        let repo_root = repo_root.as_ref().to_path_buf();
        let remote_spec = RemoteSpec::parse(remote_spec).map_err(map_error)?;
        with_remote_backend!(&remote_spec, |backend| {
            e2v_sync::status_oblivious_layout(backend, &repo_root)
        })
        .map(oblivious_layout_status_response_from_result)
        .map_err(map_error)
    }

    pub fn oblivious_layout_default_remote_status(
        &self,
        repo_root: impl AsRef<Path>,
    ) -> SdkResult<ObliviousLayoutStatusResponse> {
        let repo_root = repo_root.as_ref().to_path_buf();
        let stored = self.load_default_remote(&repo_root)?;
        self.oblivious_layout_remote_status(&stored.spec, repo_root)
    }

    pub fn enable_oblivious_layout_remote(
        &self,
        remote_spec: &str,
        request: EnableObliviousLayoutRequest,
    ) -> SdkResult<ObliviousLayoutStatusResponse> {
        let remote_spec = RemoteSpec::parse(remote_spec).map_err(map_error)?;
        with_remote_backend!(&remote_spec, |backend| {
            e2v_sync::enable_oblivious_layout(
                backend,
                EnableObliviousLayoutOptions {
                    repo_root: request.repo_root.clone(),
                    policy_profile: request.policy_profile.clone(),
                },
            )
        })
        .map(oblivious_layout_status_response_from_result)
        .map_err(map_error)
    }

    pub fn enable_oblivious_layout_default_remote(
        &self,
        request: EnableObliviousLayoutRequest,
    ) -> SdkResult<ObliviousLayoutStatusResponse> {
        let stored = self.load_default_remote(&request.repo_root)?;
        self.enable_oblivious_layout_remote(&stored.spec, request)
    }

    pub fn reshuffle_oblivious_layout_remote(
        &self,
        remote_spec: &str,
        request: ReshuffleObliviousLayoutRequest,
    ) -> SdkResult<ObliviousLayoutStatusResponse> {
        let remote_spec = RemoteSpec::parse(remote_spec).map_err(map_error)?;
        with_remote_backend!(&remote_spec, |backend| {
            e2v_sync::reshuffle_oblivious_layout(
                backend,
                ReshuffleObliviousLayoutOptions {
                    repo_root: request.repo_root.clone(),
                    policy_profile: request.policy_profile.clone(),
                },
            )
        })
        .map(oblivious_layout_status_response_from_result)
        .map_err(map_error)
    }

    pub fn reshuffle_oblivious_layout_default_remote(
        &self,
        request: ReshuffleObliviousLayoutRequest,
    ) -> SdkResult<ObliviousLayoutStatusResponse> {
        let stored = self.load_default_remote(&request.repo_root)?;
        self.reshuffle_oblivious_layout_remote(&stored.spec, request)
    }

    pub fn repair_remote(
        &self,
        remote_spec: &str,
        repo_root: impl AsRef<Path>,
    ) -> SdkResult<RepairRemoteResponse> {
        let repo_root = repo_root.as_ref().to_path_buf();
        let remote_spec = RemoteSpec::parse(remote_spec).map_err(map_error)?;
        with_remote_backend!(&remote_spec, |backend| {
            e2v_sync::repair_remote(
                backend,
                RepairRemoteOptions {
                    repo_root: repo_root.clone(),
                },
            )
        })
        .map(repair_remote_response_from_result)
        .map_err(map_error)
    }

    pub fn repair_default_remote(
        &self,
        repo_root: impl AsRef<Path>,
    ) -> SdkResult<RepairRemoteResponse> {
        let stored = self.load_default_remote(&repo_root)?;
        self.repair_remote(&stored.spec, repo_root)
    }

    pub fn force_accept_remote_rollback(
        &self,
        remote_spec: &str,
        repo_root: impl AsRef<Path>,
        password: &str,
    ) -> SdkResult<RepairRemoteResponse> {
        let repo_root = repo_root.as_ref().to_path_buf();
        let remote_spec = RemoteSpec::parse(remote_spec).map_err(map_error)?;
        with_remote_backend!(&remote_spec, |backend| {
            e2v_sync::force_accept_remote_rollback(
                backend,
                RepairRemoteOptions {
                    repo_root: repo_root.clone(),
                },
                password,
            )
        })
        .map(repair_remote_response_from_result)
        .map_err(map_error)
    }

    pub fn force_accept_default_remote_rollback(
        &self,
        repo_root: impl AsRef<Path>,
        password: &str,
    ) -> SdkResult<RepairRemoteResponse> {
        let stored = self.load_default_remote(&repo_root)?;
        self.force_accept_remote_rollback(&stored.spec, repo_root, password)
    }

    pub fn gc_remote_dry_run(
        &self,
        remote_spec: &str,
        repo_root: impl AsRef<Path>,
    ) -> SdkResult<GcDryRunResponse> {
        let repo_root = repo_root.as_ref().to_path_buf();
        let remote_spec = RemoteSpec::parse(remote_spec).map_err(map_error)?;
        with_remote_backend!(&remote_spec, |backend| {
            e2v_sync::gc_dry_run(
                backend,
                GcDryRunOptions {
                    repo_root: repo_root.clone(),
                },
            )
        })
        .map(gc_dry_run_response_from_result)
        .map_err(map_error)
    }

    pub fn gc_default_remote_dry_run(
        &self,
        repo_root: impl AsRef<Path>,
    ) -> SdkResult<GcDryRunResponse> {
        let stored = self.load_default_remote(&repo_root)?;
        self.gc_remote_dry_run(&stored.spec, repo_root)
    }

    pub fn gc_remote_execute(
        &self,
        remote_spec: &str,
        request: GcExecuteRequest,
    ) -> SdkResult<GcExecuteResponse> {
        let remote_spec = RemoteSpec::parse(remote_spec).map_err(map_error)?;
        with_remote_backend!(&remote_spec, |backend| {
            e2v_sync::gc_execute(
                backend,
                GcExecuteOptions {
                    repo_root: request.repo_root.clone(),
                    grace_period_days: request.grace_period_days,
                    allow_single_writer_maintenance_window: request
                        .allow_single_writer_maintenance_window,
                },
            )
        })
        .map(gc_execute_response_from_result)
        .map_err(map_error)
    }

    pub fn gc_default_remote_execute(
        &self,
        request: GcExecuteRequest,
    ) -> SdkResult<GcExecuteResponse> {
        let stored = self.load_default_remote(&request.repo_root)?;
        self.gc_remote_execute(&stored.spec, request)
    }
}

#[derive(Debug, Clone)]
pub struct ReadHandle {
    read_service: ReadService,
}

impl ReadHandle {
    pub fn open_snapshot(&self, snapshot_id: &str) -> SdkResult<SnapshotView> {
        self.read_service
            .open_snapshot(snapshot_id)
            .map(|snapshot| snapshot_view_from_handle(snapshot, None))
            .map_err(map_error)
    }

    pub fn resolve_branch(&self, branch_token: &str) -> SdkResult<SnapshotView> {
        self.read_service
            .resolve_branch(branch_token)
            .map(|snapshot| snapshot_view_from_handle(snapshot, Some(branch_token.to_string())))
            .map_err(map_error)
    }

    pub fn read_dir(
        &self,
        snapshot: &SnapshotView,
        path: &str,
    ) -> SdkResult<Vec<DirectoryEntryInfo>> {
        self.read_service
            .read_dir(&snapshot.inner, path)
            .map(|entries| {
                entries
                    .into_iter()
                    .map(directory_entry_info_from_entry)
                    .collect()
            })
            .map_err(map_error)
    }

    pub fn open_file(&self, snapshot: &SnapshotView, path: &str) -> SdkResult<FileView> {
        self.read_service
            .open_file(&snapshot.inner, path)
            .map(file_view_from_handle)
            .map_err(map_error)
    }

    pub fn read_range(&self, file: &FileView, offset: usize, length: usize) -> SdkResult<Vec<u8>> {
        self.read_service
            .read_range(&file.inner, offset, length)
            .map_err(map_error)
    }
}

pub fn parse_remote_spec(value: &str) -> SdkResult<ParsedRemoteSpec> {
    let _ = RemoteSpec::parse(value).map_err(map_error)?;
    Ok(ParsedRemoteSpec {
        raw: value.to_string(),
    })
}

fn add_remote_registration(
    repo_root: &Path,
    name: &str,
    spec: &str,
) -> SdkResult<RemoteRegistration> {
    if name.trim().is_empty() {
        return Err(SdkError::new(
            SdkErrorCode::InvalidArgument,
            "remote name must not be empty",
        ));
    }
    let _ = RemoteSpec::parse(spec).map_err(map_error)?;
    let stored = RemoteRegistration {
        name: name.to_string(),
        spec: spec.to_string(),
    };
    let path = remote_path(repo_root, name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(anyhow::Error::from)
            .map_err(map_error)?;
    }
    let bytes = serde_json::to_vec(&stored)
        .map_err(anyhow::Error::from)
        .map_err(map_error)?;
    std::fs::write(&path, &bytes)
        .map_err(anyhow::Error::from)
        .map_err(map_error)?;
    std::fs::write(default_remote_path(repo_root), bytes)
        .map_err(anyhow::Error::from)
        .map_err(map_error)?;
    Ok(stored)
}

fn load_default_remote_registration(repo_root: &Path) -> SdkResult<RemoteRegistration> {
    let path = default_remote_path(repo_root);
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_dir() => {
            return Err(SdkError::new(
                SdkErrorCode::CorruptState,
                format!("failed to read default remote: {}", path.display()),
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(SdkError::new(
                SdkErrorCode::NotFound,
                format!("default remote not found: {}", path.display()),
            ));
        }
        Err(error) => {
            return Err(anyhow::anyhow!("failed to read default remote: {error}"))
                .map_err(map_error);
        }
    }
    let bytes = std::fs::read(&path)
        .map_err(|error| anyhow::anyhow!("failed to read default remote: {error}"))
        .map_err(map_error)?;
    serde_json::from_slice(&bytes)
        .map_err(anyhow::Error::from)
        .map_err(|error| anyhow::anyhow!("failed to decode default remote: {error}"))
        .map_err(map_error)
}

fn remote_path(repo_root: &Path, name: &str) -> PathBuf {
    repo_root.join(REMOTES_DIR).join(format!("{name}.json"))
}

fn default_remote_path(repo_root: &Path) -> PathBuf {
    repo_root.join(DEFAULT_REMOTE_PATH)
}

fn repository_info_from_state(state: RepositoryState) -> RepositoryInfo {
    RepositoryInfo {
        repo_root: state.repo_root,
        branch: BranchInfo {
            name: state.branch.name,
            token_hex: state.branch.token_hex,
        },
        layout_generation: state.layout_generation,
    }
}

fn branch_info_from_state(state: e2v_core::BranchState) -> BranchInfo {
    BranchInfo {
        name: state.name,
        token_hex: state.token_hex,
    }
}

fn commit_info_from_result(result: CommitResult) -> CommitInfo {
    CommitInfo {
        snapshot_id: result.snapshot_id,
        committed_files: result.committed_files,
        new_bytes: result.new_bytes,
        reused_bytes: result.reused_bytes,
        warnings: result.warnings,
    }
}

fn snapshot_info_from_summary(summary: SnapshotSummary) -> SnapshotInfo {
    SnapshotInfo {
        snapshot_id: summary.snapshot_id,
        message: summary.message,
        parent_snapshot_id: summary.parent_snapshot_id,
    }
}

fn branch_summary_info_from_summary(summary: BranchSummary) -> BranchSummaryInfo {
    BranchSummaryInfo {
        name: summary.name,
        token_hex: summary.token_hex,
        head_snapshot_id: summary.head_snapshot_id,
        is_current: summary.is_current,
    }
}

fn share_list_info_from_result(result: ShareListResult) -> ShareListInfo {
    ShareListInfo {
        actors: result
            .actors
            .into_iter()
            .map(|actor| ShareActorInfo {
                actor_id: actor.actor_id,
                display_name: actor.display_name,
                role: actor.role,
            })
            .collect(),
        devices: result
            .devices
            .into_iter()
            .map(|device| ShareDeviceInfo {
                device_id: device.device_id,
                actor_id: device.actor_id,
                label: device.label,
                status: device.status,
            })
            .collect(),
    }
}

fn share_invite_info_from_bundle(bundle: ShareInviteBundle) -> ShareInviteInfo {
    ShareInviteInfo {
        actor_id: bundle.actor_id,
        device_id: bundle.device_id,
        bundle_bytes: bundle.bundle_bytes,
    }
}

fn share_accept_info_from_result(result: ShareAcceptResult) -> ShareAcceptInfo {
    ShareAcceptInfo {
        actor_id: result.actor_id,
        device_id: result.device_id,
        role: result.role,
    }
}

fn directory_entry_info_from_entry(entry: DirectoryEntry) -> DirectoryEntryInfo {
    DirectoryEntryInfo {
        name: entry.name,
        kind: entry.kind,
    }
}

fn snapshot_view_from_handle(handle: SnapshotHandle, branch_token: Option<String>) -> SnapshotView {
    SnapshotView {
        snapshot_id: handle.snapshot_id.clone(),
        layout_generation: handle.layout_generation,
        branch_token,
        inner: handle,
    }
}

fn file_view_from_handle(handle: FileHandle) -> FileView {
    FileView {
        snapshot_id: handle.snapshot_id.clone(),
        file_object_id: handle.file_object_id.clone(),
        file_size: handle.file_size(),
        chunk_count: handle.chunk_count(),
        layout_generation: handle.layout_generation(),
        crypto_suite: handle.crypto_suite().to_string(),
        key_epoch: handle.key_epoch(),
        chunker_id: handle.chunker_id().to_string(),
        inner: handle,
    }
}

fn push_response_from_result(result: e2v_sync::PushResult) -> PushResponse {
    PushResponse {
        published_snapshot_id: result.published_snapshot_id,
        uploaded_objects: result.uploaded_objects,
    }
}

fn fetch_response_from_result(result: e2v_sync::FetchResult) -> FetchResponse {
    FetchResponse {
        downloaded_objects: result.downloaded_objects,
    }
}

fn clone_response_from_result(result: e2v_sync::CloneResult) -> CloneResponse {
    CloneResponse {
        branch_token: result.branch_token,
        head_snapshot_id: result.head_snapshot_id,
    }
}

fn pull_response_from_snapshot(snapshot_id: String, fast_forward_applied: bool) -> PullResponse {
    PullResponse {
        snapshot_id,
        fast_forward_applied,
    }
}

fn restore_original_ref_file(path: &Path, original_bytes: Option<&[u8]>) -> anyhow::Result<()> {
    match original_bytes {
        Some(bytes) => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, bytes)?;
        }
        None => match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        },
    }
    Ok(())
}

fn verify_remote_response_from_result(
    result: e2v_sync::VerifyRemoteResult,
) -> VerifyRemoteResponse {
    VerifyRemoteResponse {
        sampled_objects: result.sampled_objects,
        repaired_local_objects: result.repaired_local_objects,
    }
}

fn historical_rewrite_plan_response_from_result(
    result: e2v_sync::HistoricalRewritePlan,
) -> HistoricalRewritePlanResponse {
    HistoricalRewritePlanResponse {
        reachable_object_count: result.reachable_object_count,
        remote_loose_object_count: result.remote_loose_object_count,
        remote_pack_object_count: result.remote_pack_object_count,
        old_epoch_count: result.old_epoch_count,
        large_repo_advisory: result.large_repo_advisory,
        requires_remote_credential_revocation_guidance: result
            .requires_remote_credential_revocation_guidance,
    }
}

fn historical_rewrite_execute_response_from_result(
    result: e2v_sync::HistoricalRewriteResult,
) -> HistoricalRewriteExecuteResponse {
    HistoricalRewriteExecuteResponse {
        rewritten_objects: result.rewritten_objects,
        retired_epoch_count: result.retired_epoch_count,
        deleted_stale_remote_refs: result.deleted_stale_remote_refs,
        next_layout_generation: result.next_layout_generation,
    }
}

fn oblivious_layout_plan_response_from_result(
    result: e2v_sync::ObliviousLayoutPlan,
) -> ObliviousLayoutPlanResponse {
    ObliviousLayoutPlanResponse {
        estimated_real_reads_per_request: result.estimated_real_reads_per_request,
        estimated_cover_reads_per_request: result.estimated_cover_reads_per_request,
        estimated_bytes_per_request: result.estimated_bytes_per_request,
        estimated_write_amplification: result.estimated_write_amplification,
        requires_layout_root_rewrite: result.requires_layout_root_rewrite,
        advisory_messages: result.advisory_messages,
    }
}

fn oblivious_layout_status_response_from_result(
    result: e2v_sync::ObliviousLayoutStatus,
) -> ObliviousLayoutStatusResponse {
    ObliviousLayoutStatusResponse {
        layout_mode: result.layout_mode,
        dedup_mode: result.dedup_mode,
        layout_generation: result.layout_generation,
        oblivious_generation: result.oblivious_generation,
        policy_profile: result.policy_profile,
    }
}

fn repair_remote_response_from_result(
    result: e2v_sync::RepairRemoteResult,
) -> RepairRemoteResponse {
    RepairRemoteResponse {
        repaired_objects: result.repaired_objects,
    }
}

fn gc_dry_run_response_from_result(result: e2v_sync::GcDryRunReport) -> GcDryRunResponse {
    GcDryRunResponse {
        unreachable_physical_refs: result.unreachable_physical_refs,
        active_intent_paths: result.active_intent_paths,
    }
}

fn gc_execute_response_from_result(result: e2v_sync::GcExecuteResult) -> GcExecuteResponse {
    GcExecuteResponse {
        deleted_physical_refs: result.deleted_physical_refs,
    }
}

pub(crate) fn map_error(error: anyhow::Error) -> SdkError {
    let message = error.to_string();
    let lower = message.to_ascii_lowercase();
    let normalized = lower.replace('\\', "/");

    let code = if lower.contains("critical_rollback_detected")
        || lower.contains(" rollback detected")
    {
        SdkErrorCode::RollbackDetected
    } else if lower.contains("authentication failed")
        || lower.contains("tampered")
        || lower.contains("stale-layout fallback unavailable")
        || lower.contains("failed to decode default remote")
        || lower.contains("failed to read checkout mapping")
        || lower.contains("failed to decode checkout mapping")
        || lower.contains("failed to decode gc delete journal")
        || lower.contains("failed to decode historical rewrite checkpoint")
        || lower.contains("failed to decode ref record")
        || lower.contains("failed to decode branch ref record")
        || lower.contains("failed to decode encrypted ref")
        || lower.contains("failed to decode remote layout root")
        || lower.contains("failed to decode remote keyring pointer")
        || lower.contains("failed to decode remote keyring state")
        || lower.contains("failed to decode remote branch ref")
        || lower.contains("failed to read local keyring pointer")
        || lower.contains("failed to decode local keyring pointer")
        || lower.contains("failed to decode local keyring state")
        || lower.contains("failed to decode local keyring state for pointer ref")
        || lower.contains("failed to decode current keyring pointer")
        || lower.contains("failed to decode current keyring state")
        || lower.contains("failed to read ") && normalized.contains("/.e2v/keyring/keyring.")
        || lower.contains("failed to read ")
            && normalized.contains("/.e2v/layout_root.json")
            && !lower.contains("not found")
            && !lower.contains("missing")
        || lower.contains("failed to decode ")
            && normalized.contains("/.e2v/keyring/keyring.current")
        || lower.contains("failed to decode ") && normalized.contains("/.e2v/keyring/keyring.")
        || lower.contains("failed to decode ") && normalized.contains("/.e2v/layout_root.json")
        || lower.contains("failed to decode authenticated pack index root")
        || lower.contains("failed to decrypt authenticated pack index root")
        || lower.contains("failed to decode authenticated pack index segment")
        || lower.contains("failed to decrypt authenticated pack index segment")
        || lower.contains("failed to read trusted state")
        || lower.contains("failed to decode trusted state")
        || lower.contains("remote keyring pointer generation mismatch")
        || lower.contains("missing physical chunk")
        || lower.contains("cached pack index has no entry")
        || lower.contains("corrupt")
    {
        SdkErrorCode::CorruptState
    } else if lower.contains("unsupported remote spec")
        || lower.contains("unsupported remote url scheme")
        || lower.contains("invalid remote url")
        || lower.contains("invalid snapshot id")
        || lower.contains("invalid snapshot path")
        || lower.contains("current checked out branch does not match requested branch token")
        || lower.contains("path traversal")
        || lower.contains("must not be empty")
        || lower.contains("sample percent must be between")
        || lower.contains("grace period must be greater than zero")
        || lower.contains("full re-encryption confirmation")
        || lower.contains("confirm-full-reencryption")
        || lower.contains("maintenance window")
        || lower.contains("bad request")
    {
        SdkErrorCode::InvalidArgument
    } else if lower.contains("not found")
        || lower.contains("missing")
        || lower.contains("has no snapshots")
    {
        SdkErrorCode::NotFound
    } else if lower.contains("already exists") || lower.contains("must be empty before init") {
        SdkErrorCode::AlreadyExists
    } else if lower.contains("permission denied") || lower.contains("owner-admin local device") {
        SdkErrorCode::PermissionDenied
    } else if lower.contains("wrong password")
        || lower.contains("unlock")
        || lower.contains("password")
    {
        SdkErrorCode::AuthenticationRequired
    } else if lower.contains("needs-rebase") || lower.contains("rebase") {
        SdkErrorCode::NeedsRebase
    } else if lower.contains("rollback") {
        SdkErrorCode::RollbackDetected
    } else if lower.contains("unsupported") {
        SdkErrorCode::Unsupported
    } else if lower.contains("conflict") {
        SdkErrorCode::Conflict
    } else if lower.contains("io error")
        || lower.contains("failed to read")
        || lower.contains("failed to create")
        || lower.contains("failed to write")
    {
        SdkErrorCode::Io
    } else {
        SdkErrorCode::Internal
    };

    SdkError::new(code, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_error_does_not_treat_corrupt_pack_index_segment_as_invalid_argument() {
        let error = anyhow::anyhow!(
            "failed to decode authenticated pack index segment packs/index/push-invalid-pack-index-segment-op/pack-index.000000.json"
        );

        let mapped = map_error(error);

        assert_eq!(mapped.code(), SdkErrorCode::CorruptState);
    }

    #[test]
    fn map_error_treats_corrupted_pack_index_root_as_corrupt_state() {
        let error = anyhow::anyhow!("failed to decrypt authenticated pack index root");

        let mapped = map_error(error);

        assert_eq!(mapped.code(), SdkErrorCode::CorruptState);
    }

    #[test]
    fn map_error_treats_corrupted_trusted_state_as_corrupt_state() {
        let error =
            anyhow::anyhow!("failed to decode trusted state C:/tmp/e2v/trusted-state/repo.json");

        let mapped = map_error(error);

        assert_eq!(mapped.code(), SdkErrorCode::CorruptState);
    }

    #[test]
    fn map_error_treats_unreadable_trusted_state_as_corrupt_state() {
        let error =
            anyhow::anyhow!("failed to read trusted state C:/tmp/e2v/trusted-state/repo.json");

        let mapped = map_error(error);

        assert_eq!(mapped.code(), SdkErrorCode::CorruptState);
    }

    #[test]
    fn map_error_treats_corrupted_local_keyring_pointer_as_corrupt_state() {
        let error = anyhow::anyhow!("failed to decode local keyring pointer");

        let mapped = map_error(error);

        assert_eq!(mapped.code(), SdkErrorCode::CorruptState);
    }

    #[test]
    fn map_error_treats_corrupted_current_keyring_pointer_as_corrupt_state() {
        let error = anyhow::anyhow!("failed to decode current keyring pointer");

        let mapped = map_error(error);

        assert_eq!(mapped.code(), SdkErrorCode::CorruptState);
    }

    #[test]
    fn map_error_treats_corrupted_historical_rewrite_checkpoint_as_corrupt_state() {
        let error = anyhow::anyhow!("failed to decode historical rewrite checkpoint");

        let mapped = map_error(error);

        assert_eq!(mapped.code(), SdkErrorCode::CorruptState);
    }

    #[test]
    fn map_error_treats_corrupted_gc_delete_journal_as_corrupt_state() {
        let error = anyhow::anyhow!(
            "failed to decode gc delete journal C:/tmp/repo/.e2v/journal/gc/gc-execute.json"
        );

        let mapped = map_error(error);

        assert_eq!(mapped.code(), SdkErrorCode::CorruptState);
    }

    #[test]
    fn map_error_treats_corrupted_checkout_mapping_as_corrupt_state() {
        let error = anyhow::anyhow!("failed to decode checkout mapping");

        let mapped = map_error(error);

        assert_eq!(mapped.code(), SdkErrorCode::CorruptState);
    }

    #[test]
    fn map_error_treats_corrupted_default_remote_as_corrupt_state() {
        let error = anyhow::anyhow!("failed to decode default remote: expected value at line 1");

        let mapped = map_error(error);

        assert_eq!(mapped.code(), SdkErrorCode::CorruptState);
    }

    #[test]
    fn map_error_treats_unreadable_local_keyring_pointer_as_corrupt_state() {
        let error = anyhow::anyhow!(
            "failed to read local keyring pointer C:/tmp/repo/.e2v/keyring/keyring.current"
        );

        let mapped = map_error(error);

        assert_eq!(mapped.code(), SdkErrorCode::CorruptState);
    }

    #[test]
    fn map_error_treats_unreadable_checkout_mapping_as_corrupt_state() {
        let error = anyhow::anyhow!(
            "failed to read checkout mapping C:/tmp/repo/.e2v-checkout-mapping.json"
        );

        let mapped = map_error(error);

        assert_eq!(mapped.code(), SdkErrorCode::CorruptState);
    }

    #[test]
    fn map_error_treats_corrupted_local_branch_ref_record_as_corrupt_state() {
        let error = anyhow::anyhow!("failed to decode branch ref record");

        let mapped = map_error(error);

        assert_eq!(mapped.code(), SdkErrorCode::CorruptState);
    }

    #[test]
    fn map_error_treats_corrupted_encrypted_ref_as_corrupt_state() {
        let error = anyhow::anyhow!("failed to decode encrypted ref");

        let mapped = map_error(error);

        assert_eq!(mapped.code(), SdkErrorCode::CorruptState);
    }

    #[test]
    fn map_error_treats_corrupted_local_keyring_generation_file_as_corrupt_state() {
        let error =
            anyhow::anyhow!("failed to decode C:\\repo\\.e2v\\keyring\\keyring.7-bootstrap-device");

        let mapped = map_error(error);

        assert_eq!(mapped.code(), SdkErrorCode::CorruptState);
    }

    #[test]
    fn map_error_treats_corrupted_local_keyring_pointer_file_as_corrupt_state() {
        let error = anyhow::anyhow!("failed to decode C:\\repo\\.e2v\\keyring\\keyring.current");

        let mapped = map_error(error);

        assert_eq!(mapped.code(), SdkErrorCode::CorruptState);
    }

    #[test]
    fn map_error_treats_corrupted_local_keyring_pointer_file_with_mixed_separators_as_corrupt_state()
     {
        let error = anyhow::anyhow!("failed to decode C:\\repo\\.e2v\\keyring/keyring.current");

        let mapped = map_error(error);

        assert_eq!(mapped.code(), SdkErrorCode::CorruptState);
    }

    #[test]
    fn map_error_treats_corrupted_local_layout_root_file_as_corrupt_state() {
        let error = anyhow::anyhow!("failed to decode C:\\repo\\.e2v\\layout_root.json");

        let mapped = map_error(error);

        assert_eq!(mapped.code(), SdkErrorCode::CorruptState);
    }

    #[test]
    fn map_error_treats_unreadable_local_layout_root_file_as_corrupt_state() {
        let error = anyhow::anyhow!("failed to read C:\\repo\\.e2v\\layout_root.json");

        let mapped = map_error(error);

        assert_eq!(mapped.code(), SdkErrorCode::CorruptState);
    }

    #[test]
    fn map_error_treats_unreadable_local_keyring_generation_file_as_corrupt_state() {
        let error = anyhow::anyhow!("failed to read C:\\repo\\.e2v\\keyring\\keyring.2");

        let mapped = map_error(error);

        assert_eq!(mapped.code(), SdkErrorCode::CorruptState);
    }
}
