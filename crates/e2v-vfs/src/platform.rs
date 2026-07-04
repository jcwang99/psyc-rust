use std::path::PathBuf;

use anyhow::Result;

use crate::{MountLaunchState, MountLaunchSummary, MountRequest, VfsMountConfig};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformFamily {
    WindowsWinfsp,
    LinuxFuse,
    MacosFuse,
}

pub trait PlatformMountAdapter {
    fn platform_family(&self) -> PlatformFamily;
    fn launch(&self, request: MountRequest) -> Result<MountLaunchSummary>;
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LinuxMountAdapter;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MacosMountAdapter;

impl PlatformMountAdapter for LinuxMountAdapter {
    fn platform_family(&self) -> PlatformFamily {
        PlatformFamily::LinuxFuse
    }

    fn launch(&self, request: MountRequest) -> Result<MountLaunchSummary> {
        let vfs = match request.mount_mode_label() {
            "snapshot-pinned" => crate::ReadOnlyVfs::mount_snapshot(request.config.clone())?,
            "live-branch" => crate::ReadOnlyVfs::mount_live_branch(request.config.clone())?,
            other => anyhow::bail!("unsupported mount mode: {other}"),
        };
        Ok(MountLaunchSummary {
            mount_mode: request.mount_mode_label().to_string(),
            mount_point: request.mount_point().clone(),
            cache_policy: vfs.cache_policy(),
            read_only: true,
            stream_only: true,
            launch_state: MountLaunchState::SummaryOnly,
            status_message: "linux adapter not implemented yet".to_string(),
        })
    }
}

impl PlatformMountAdapter for MacosMountAdapter {
    fn platform_family(&self) -> PlatformFamily {
        PlatformFamily::MacosFuse
    }

    fn launch(&self, request: MountRequest) -> Result<MountLaunchSummary> {
        let vfs = match request.mount_mode_label() {
            "snapshot-pinned" => crate::ReadOnlyVfs::mount_snapshot(request.config.clone())?,
            "live-branch" => crate::ReadOnlyVfs::mount_live_branch(request.config.clone())?,
            other => anyhow::bail!("unsupported mount mode: {other}"),
        };
        Ok(MountLaunchSummary {
            mount_mode: request.mount_mode_label().to_string(),
            mount_point: request.mount_point().clone(),
            cache_policy: vfs.cache_policy(),
            read_only: true,
            stream_only: true,
            launch_state: MountLaunchState::SummaryOnly,
            status_message: "macos adapter not implemented yet".to_string(),
        })
    }
}

pub fn try_mount_snapshot_on_current_platform(
    config: VfsMountConfig,
    mount_point: PathBuf,
) -> Result<MountLaunchSummary> {
    #[cfg(windows)]
    {
        windows::mount_snapshot(config, mount_point)
    }
    #[cfg(not(windows))]
    {
        let vfs = crate::ReadOnlyVfs::mount_snapshot(config)?;
        Ok(MountLaunchSummary {
            mount_mode: "snapshot-pinned".to_string(),
            mount_point,
            cache_policy: vfs.cache_policy(),
            read_only: true,
            stream_only: true,
            launch_state: MountLaunchState::SummaryOnly,
            status_message: "not supported on this platform yet".to_string(),
        })
    }
}

pub fn try_mount_live_branch_on_current_platform(
    config: VfsMountConfig,
    mount_point: PathBuf,
) -> Result<MountLaunchSummary> {
    #[cfg(windows)]
    {
        windows::mount_live_branch(config, mount_point)
    }
    #[cfg(not(windows))]
    {
        let vfs = crate::ReadOnlyVfs::mount_live_branch(config)?;
        Ok(MountLaunchSummary {
            mount_mode: "live-branch".to_string(),
            mount_point,
            cache_policy: vfs.cache_policy(),
            read_only: true,
            stream_only: true,
            launch_state: MountLaunchState::SummaryOnly,
            status_message: "not supported on this platform yet".to_string(),
        })
    }
}

#[cfg(windows)]
mod windows {
    use std::path::PathBuf;

    use anyhow::Result;

    use crate::{MountLaunchSummary, VfsMountConfig};

    pub fn mount_snapshot(
        config: VfsMountConfig,
        mount_point: PathBuf,
    ) -> Result<MountLaunchSummary> {
        crate::windows::mount_snapshot(config, mount_point)
    }

    pub fn mount_live_branch(
        config: VfsMountConfig,
        mount_point: PathBuf,
    ) -> Result<MountLaunchSummary> {
        crate::windows::mount_live_branch(config, mount_point)
    }
}
