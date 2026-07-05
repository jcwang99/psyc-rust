use std::path::PathBuf;

use e2v_api::SdkErrorCode;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Home,
    Workbench,
}

#[derive(Debug, Clone)]
pub enum Message {
    Branches(crate::pages::branches::BranchesMessage),
    Home(crate::pages::home::HomeMessage),
    HomeJobFinished(Result<crate::pages::home::HomeJobResult, AppError>),
    History(crate::pages::history::HistoryMessage),
    Overview(crate::pages::overview::OverviewMessage),
    OverviewJobFinished(Result<crate::pages::overview::OverviewJobResult, AppError>),
    Search(crate::pages::search::SearchMessage),
    Sharing(crate::pages::sharing::SharingMessage),
    Sync(crate::pages::sync::SyncMessage),
    SyncJobFinished(Result<crate::pages::sync::SyncJobResult, AppError>),
    Workbench(crate::pages::workbench::WorkbenchMessage),
    NoOp,
}

impl From<crate::pages::branches::BranchesMessage> for Message {
    fn from(message: crate::pages::branches::BranchesMessage) -> Self {
        Self::Branches(message)
    }
}

impl From<crate::pages::home::HomeMessage> for Message {
    fn from(message: crate::pages::home::HomeMessage) -> Self {
        Self::Home(message)
    }
}

impl From<crate::pages::history::HistoryMessage> for Message {
    fn from(message: crate::pages::history::HistoryMessage) -> Self {
        Self::History(message)
    }
}

impl From<crate::pages::overview::OverviewMessage> for Message {
    fn from(message: crate::pages::overview::OverviewMessage) -> Self {
        Self::Overview(message)
    }
}

impl From<crate::pages::search::SearchMessage> for Message {
    fn from(message: crate::pages::search::SearchMessage) -> Self {
        Self::Search(message)
    }
}

impl From<crate::pages::sharing::SharingMessage> for Message {
    fn from(message: crate::pages::sharing::SharingMessage) -> Self {
        Self::Sharing(message)
    }
}

impl From<crate::pages::sync::SyncMessage> for Message {
    fn from(message: crate::pages::sync::SyncMessage) -> Self {
        Self::Sync(message)
    }
}

impl From<crate::pages::workbench::WorkbenchMessage> for Message {
    fn from(message: crate::pages::workbench::WorkbenchMessage) -> Self {
        Self::Workbench(message)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppError {
    pub code: &'static str,
    pub message: String,
}

impl AppError {
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            code: "internal",
            message: message.into(),
        }
    }

    pub fn io(message: impl Into<String>) -> Self {
        Self {
            code: "io",
            message: message.into(),
        }
    }

    pub fn invalid_state(message: impl Into<String>) -> Self {
        Self {
            code: "invalid_state",
            message: message.into(),
        }
    }

    pub fn from_sdk(error: e2v_api::SdkError) -> Self {
        let code = match error.code() {
            SdkErrorCode::InvalidArgument => "invalid_argument",
            SdkErrorCode::NotFound => "not_found",
            SdkErrorCode::AlreadyExists => "already_exists",
            SdkErrorCode::PermissionDenied => "permission_denied",
            SdkErrorCode::AuthenticationRequired => "authentication_required",
            SdkErrorCode::Conflict => "conflict",
            SdkErrorCode::NeedsRebase => "needs_rebase",
            SdkErrorCode::RollbackDetected => "rollback_detected",
            SdkErrorCode::Unsupported => "unsupported",
            SdkErrorCode::CorruptState => "corrupt_state",
            SdkErrorCode::Io => "io",
            SdkErrorCode::Internal => "internal",
            _ => "sdk",
        };

        Self {
            code,
            message: error.message().to_owned(),
        }
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for AppError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentRepositoryEntry {
    pub repo_root: PathBuf,
    pub last_opened_unix_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryRegistry {
    pub pinned: Vec<PathBuf>,
    pub recent: Vec<RecentRepositoryEntry>,
}

impl RepositoryRegistry {
    pub fn touch_recent(&mut self, repo_root: PathBuf, last_opened_unix_ms: u64) {
        self.recent.retain(|entry| entry.repo_root != repo_root);
        self.recent.insert(
            0,
            RecentRepositoryEntry {
                repo_root,
                last_opened_unix_ms,
            },
        );
        self.recent.truncate(20);
    }

    pub fn toggle_pin(&mut self, repo_root: PathBuf) {
        if let Some(index) = self.pinned.iter().position(|entry| entry == &repo_root) {
            self.pinned.remove(index);
        } else {
            self.pinned.push(repo_root);
            self.pinned.sort();
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryHomeCard {
    pub repo_root: PathBuf,
    pub display_name: String,
    pub branch_name: String,
    pub branch_token: String,
    pub head_snapshot_id: Option<String>,
    pub remote_configured: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkbenchPage {
    Overview,
    History,
    Branches,
    Sync,
    Search,
    Sharing,
    Preview,
    Advanced,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobState {
    Running,
    Succeeded,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingConfirmation {
    SingleWriterRiskPush {
        repo_root: PathBuf,
        branch_token: String,
    },
    RevokeMember {
        repo_root: PathBuf,
        actor_id: String,
        password: String,
    },
    RevokeDevice {
        repo_root: PathBuf,
        device_id: String,
        password: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRecord {
    pub id: u64,
    pub label: String,
    pub repo_root: Option<PathBuf>,
    pub state: JobState,
}
