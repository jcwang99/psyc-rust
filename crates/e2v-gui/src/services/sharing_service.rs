use std::path::PathBuf;

use base64::Engine as _;
use e2v_api::{
    Sdk, ShareAcceptDeviceRequest, ShareAcceptMemberRequest, ShareInviteDeviceRequest,
    ShareInviteMemberRequest, ShareListInfo, ShareRevokeDeviceRequest, ShareRevokeMemberRequest,
};

use crate::domain::AppError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareActorRow {
    pub actor_id: String,
    pub display_name: String,
    pub role: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareDeviceRow {
    pub device_id: String,
    pub actor_id: String,
    pub label: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SharingRoster {
    pub actors: Vec<ShareActorRow>,
    pub devices: Vec<ShareDeviceRow>,
}

pub trait SharingService: Send + Sync + std::fmt::Debug + 'static {
    fn load_roster(&self, repo_root: PathBuf) -> Result<SharingRoster, AppError>;
    fn invite_member(&self, repo_root: PathBuf, display_name: String) -> Result<String, AppError>;
    fn accept_member(
        &self,
        repo_root: PathBuf,
        invite_bundle_base64: String,
        local_device_label: String,
    ) -> Result<SharingRoster, AppError>;
    fn invite_device(
        &self,
        repo_root: PathBuf,
        actor_id: String,
        device_label: String,
    ) -> Result<String, AppError>;
    fn accept_device(
        &self,
        repo_root: PathBuf,
        invite_bundle_base64: String,
        local_device_label: String,
    ) -> Result<SharingRoster, AppError>;
    fn revoke_member(
        &self,
        repo_root: PathBuf,
        actor_id: String,
        password: String,
    ) -> Result<SharingRoster, AppError>;
    fn revoke_device(
        &self,
        repo_root: PathBuf,
        device_id: String,
        password: String,
    ) -> Result<SharingRoster, AppError>;
}

#[derive(Debug, Default)]
pub struct RealSharingService {
    sdk: Sdk,
}

impl RealSharingService {
    fn roster_from_info(info: ShareListInfo) -> SharingRoster {
        SharingRoster {
            actors: info
                .actors
                .into_iter()
                .map(|actor| ShareActorRow {
                    actor_id: actor.actor_id,
                    display_name: actor.display_name,
                    role: actor.role,
                })
                .collect(),
            devices: info
                .devices
                .into_iter()
                .map(|device| ShareDeviceRow {
                    device_id: device.device_id,
                    actor_id: device.actor_id,
                    label: device.label,
                    status: device.status,
                })
                .collect(),
        }
    }
}

impl SharingService for RealSharingService {
    fn load_roster(&self, repo_root: PathBuf) -> Result<SharingRoster, AppError> {
        self.sdk
            .share_list(&repo_root)
            .map(Self::roster_from_info)
            .map_err(AppError::from_sdk)
    }

    fn invite_member(&self, repo_root: PathBuf, display_name: String) -> Result<String, AppError> {
        self.sdk
            .share_invite_member(&repo_root, ShareInviteMemberRequest { display_name })
            .map(|info| base64::engine::general_purpose::STANDARD.encode(info.bundle_bytes))
            .map_err(AppError::from_sdk)
    }

    fn accept_member(
        &self,
        repo_root: PathBuf,
        invite_bundle_base64: String,
        local_device_label: String,
    ) -> Result<SharingRoster, AppError> {
        let invite_bytes = base64::engine::general_purpose::STANDARD
            .decode(invite_bundle_base64.trim())
            .map_err(|error| AppError::invalid_state(error.to_string()))?;
        self.sdk
            .share_accept_member(
                &repo_root,
                ShareAcceptMemberRequest {
                    invite_bytes,
                    local_device_label,
                },
            )
            .map_err(AppError::from_sdk)?;
        self.load_roster(repo_root)
    }

    fn invite_device(
        &self,
        repo_root: PathBuf,
        actor_id: String,
        device_label: String,
    ) -> Result<String, AppError> {
        self.sdk
            .share_invite_device(
                &repo_root,
                ShareInviteDeviceRequest {
                    actor_id,
                    device_label,
                },
            )
            .map(|info| base64::engine::general_purpose::STANDARD.encode(info.bundle_bytes))
            .map_err(AppError::from_sdk)
    }

    fn accept_device(
        &self,
        repo_root: PathBuf,
        invite_bundle_base64: String,
        local_device_label: String,
    ) -> Result<SharingRoster, AppError> {
        let invite_bytes = base64::engine::general_purpose::STANDARD
            .decode(invite_bundle_base64.trim())
            .map_err(|error| AppError::invalid_state(error.to_string()))?;
        self.sdk
            .share_accept_device(
                &repo_root,
                ShareAcceptDeviceRequest {
                    invite_bytes,
                    local_device_label,
                },
            )
            .map_err(AppError::from_sdk)?;
        self.load_roster(repo_root)
    }

    fn revoke_member(
        &self,
        repo_root: PathBuf,
        actor_id: String,
        password: String,
    ) -> Result<SharingRoster, AppError> {
        self.sdk
            .share_revoke_member(&repo_root, ShareRevokeMemberRequest { actor_id, password })
            .map_err(AppError::from_sdk)?;
        self.load_roster(repo_root)
    }

    fn revoke_device(
        &self,
        repo_root: PathBuf,
        device_id: String,
        password: String,
    ) -> Result<SharingRoster, AppError> {
        self.sdk
            .share_revoke_device(
                &repo_root,
                ShareRevokeDeviceRequest {
                    device_id,
                    password,
                },
            )
            .map_err(AppError::from_sdk)?;
        self.load_roster(repo_root)
    }
}
