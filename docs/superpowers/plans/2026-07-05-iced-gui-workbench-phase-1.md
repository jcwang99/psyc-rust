# Iced GUI Workbench Phase 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add the first usable standalone Iced desktop application for this repository system, including a multi-repository home, a repository workbench shell, and the `Overview`, `History`, `Branches`, and `Sync` pages with background jobs and confirmation flows.

**Architecture:** Phase 1 adds a new `crates/e2v-gui` workspace member that acts as a composition shell over `e2v-api::Sdk`, `e2v_core::RepositoryFacade`, and existing repository formats without shelling out to the CLI. The GUI is split into a pure state/update layer, thin service adapters, GUI-only repository registry persistence, and focused page modules so we can land the shell first, then daily workflows, then branch/history/sync surfaces without collapsing everything into one file.

**Tech Stack:** Rust workspace, `iced 0.14`, `serde`, `serde_json`, existing `anyhow`/`tokio`/`futures` ecosystem only where needed for background tasks, focused crate tests in `crates/e2v-gui/tests`, existing `e2v-api` and `e2v-core` service boundaries.

## Global Constraints

- The GUI is a standalone desktop application, not a browser UI and not an embedded CLI wrapper.
- The first screen is a multi-repository home, not a single-repo dashboard.
- Dangerous and low-frequency operations must be hidden by default and grouped into an advanced/maintenance area.
- Stability and safety take priority over visual cleverness or aggressive optimization.
- The GUI should model the current product surface only. It must not add new first-class concepts solely to preserve obsolete or compatibility-only interfaces.
- The GUI must call Rust interfaces directly for normal operations. It must not shell out to the CLI as its primary execution model.
- The first implementation plan derived from this design should target Phase 1 only.
- Each phase after Phase 1 should become its own implementation plan rather than extending the first plan indefinitely.

---

## File Structure

### Workspace

- Modify: `Cargo.toml`
  - add the `crates/e2v-gui` workspace member
  - add the shared GUI dependencies needed by the new crate

### New GUI crate

- Create: `crates/e2v-gui/Cargo.toml`
  - GUI crate manifest
- Create: `crates/e2v-gui/src/main.rs`
  - native app entrypoint
- Create: `crates/e2v-gui/src/lib.rs`
  - public re-exports for tests and app boot
- Create: `crates/e2v-gui/src/app.rs`
  - top-level application state, boot, update, and view composition
- Create: `crates/e2v-gui/src/domain.rs`
  - shared GUI types: app errors, repository cards, page ids, form state, job state
- Create: `crates/e2v-gui/src/jobs.rs`
  - background-job launch helpers and job-state transitions
- Create: `crates/e2v-gui/src/testing.rs`
  - test harness exports used by integration tests
- Create: `crates/e2v-gui/src/services/mod.rs`
  - service module wiring
- Create: `crates/e2v-gui/src/services/repository_service.rs`
  - thin adapters over `e2v-api::Sdk` and `e2v_core::RepositoryFacade`
- Create: `crates/e2v-gui/src/services/registry_store.rs`
  - GUI-only recent/pinned repository persistence
- Create: `crates/e2v-gui/src/pages/home.rs`
  - home screen state and rendering
- Create: `crates/e2v-gui/src/pages/workbench.rs`
  - workbench shell and left-navigation composition
- Create: `crates/e2v-gui/src/pages/overview.rs`
  - repository overview page and commit entrypoint
- Create: `crates/e2v-gui/src/pages/history.rs`
  - snapshot list and checkout flow
- Create: `crates/e2v-gui/src/pages/branches.rs`
  - branch list/create/checkout/delete flow
- Create: `crates/e2v-gui/src/pages/sync.rs`
  - remote add, push, fetch, and pull flow
- Create: `crates/e2v-gui/src/widgets/job_drawer.rs`
  - reusable drawer for running/completed jobs
- Create: `crates/e2v-gui/src/widgets/confirmation_sheet.rs`
  - reusable confirmation surface for risky sync actions

### New GUI tests

- Create: `crates/e2v-gui/tests/smoke.rs`
- Create: `crates/e2v-gui/tests/registry.rs`
- Create: `crates/e2v-gui/tests/home.rs`
- Create: `crates/e2v-gui/tests/overview.rs`
- Create: `crates/e2v-gui/tests/history_branches.rs`
- Create: `crates/e2v-gui/tests/sync.rs`

## Task 1: Scaffold `e2v-gui` And Boot A Native Iced Shell

**Files:**
- Modify: `Cargo.toml`
- Create: `crates/e2v-gui/Cargo.toml`
- Create: `crates/e2v-gui/src/main.rs`
- Create: `crates/e2v-gui/src/lib.rs`
- Create: `crates/e2v-gui/src/app.rs`
- Create: `crates/e2v-gui/src/domain.rs`
- Create: `crates/e2v-gui/tests/smoke.rs`

**Interfaces:**
- Produces: `pub fn run() -> iced::Result`
- Produces: `pub fn boot() -> (PsycGuiApp, iced::Task<Message>)`
- Produces: `pub enum Screen { Home, Workbench }`
- Produces: `pub enum Message { NoOp }`
- Produces: `pub struct PsycGuiApp { pub screen: Screen }`

- [ ] **Step 1: Write the failing smoke test**

```rust
use e2v_gui::{boot, Screen};

#[test]
fn boot_starts_on_home_screen_without_a_selected_repository() {
    let (app, task) = boot();

    assert_eq!(app.screen, Screen::Home);
    assert!(app.selected_repository.is_none());
    assert_eq!(task.units(), 0);
}
```

- [ ] **Step 2: Run the focused smoke test to verify it fails**

Run:

```powershell
cargo test -p e2v-gui boot_starts_on_home_screen_without_a_selected_repository
```

Expected: FAIL because the `e2v-gui` crate does not exist yet.

- [ ] **Step 3: Add the minimal crate, boot state, and native entrypoint**

```rust
// crates/e2v-gui/src/domain.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Home,
    Workbench,
}

#[derive(Debug, Clone)]
pub enum Message {
    NoOp,
}

// crates/e2v-gui/src/app.rs
use iced::{Element, Task};
use iced::widget::{container, text};

use crate::domain::{Message, Screen};

#[derive(Debug)]
pub struct PsycGuiApp {
    pub screen: Screen,
    pub selected_repository: Option<std::path::PathBuf>,
}

pub fn boot() -> (PsycGuiApp, Task<Message>) {
    (
        PsycGuiApp {
            screen: Screen::Home,
            selected_repository: None,
        },
        Task::none(),
    )
}

pub fn update(_app: &mut PsycGuiApp, _message: Message) -> Task<Message> {
    Task::none()
}

pub fn view(app: &PsycGuiApp) -> Element<'_, Message> {
    let title = match app.screen {
        Screen::Home => "Repositories",
        Screen::Workbench => "Workbench",
    };

    container(text(title).size(32)).into()
}

// crates/e2v-gui/src/lib.rs
pub mod app;
pub mod domain;

pub use app::{boot, run, PsycGuiApp};
pub use domain::{Message, Screen};

pub fn run() -> iced::Result {
    iced::application("psyc-rust", app::update, app::view)
        .window_size(iced::Size::new(1200.0, 800.0))
        .run_with(app::boot)
}
```

And the manifest:

```toml
# crates/e2v-gui/Cargo.toml
[package]
name = "e2v-gui"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
iced = "0.14"
serde.workspace = true
serde_json.workspace = true
anyhow.workspace = true
```

- [ ] **Step 4: Re-run the focused smoke test to verify it passes**

Run:

```powershell
cargo test -p e2v-gui boot_starts_on_home_screen_without_a_selected_repository
```

Expected: PASS

- [ ] **Step 5: Commit the scaffold**

```powershell
git add Cargo.toml crates/e2v-gui
git commit -m "feat: scaffold iced gui crate"
```

## Task 2: Add Repository Registry Persistence And Real Service Adapters

**Files:**
- Modify: `crates/e2v-gui/src/app.rs`
- Modify: `crates/e2v-gui/src/lib.rs`
- Modify: `crates/e2v-gui/src/domain.rs`
- Create: `crates/e2v-gui/src/testing.rs`
- Create: `crates/e2v-gui/src/services/mod.rs`
- Create: `crates/e2v-gui/src/services/repository_service.rs`
- Create: `crates/e2v-gui/src/services/registry_store.rs`
- Create: `crates/e2v-gui/tests/registry.rs`

**Interfaces:**
- Produces: `pub struct RepositoryHomeCard`
- Produces: `pub struct RepositoryRegistry`
- Produces: `pub struct AppError`
- Produces: `pub struct AppServices`
- Produces: `pub trait RepositoryService`
- Produces: `pub struct RealRepositoryService`
- Produces: `pub struct FakeRepositoryService`
- Produces: `pub struct FsRepositoryRegistryStore`
- Produces: `fn load_repository_summary(&self, repo_root: PathBuf) -> Result<RepositoryHomeCard, AppError>`
- Produces: `fn load(&self) -> Result<RepositoryRegistry, AppError>`
- Produces: `fn save(&self, registry: &RepositoryRegistry) -> Result<(), AppError>`

- [ ] **Step 1: Write the failing registry and summary tests**

```rust
use e2v_gui::services::{FsRepositoryRegistryStore, RealRepositoryService, RepositoryService};
use e2v_gui::RepositoryRegistry;
use tempfile::tempdir;

#[test]
fn registry_round_trips_pinned_and_recent_entries_in_mru_order() {
    let temp = tempdir().unwrap();
    let store = FsRepositoryRegistryStore::new(temp.path().join("gui-state.json"));

    let mut registry = RepositoryRegistry::default();
    registry.touch_recent("D:/repos/alpha".into(), 300);
    registry.touch_recent("D:/repos/beta".into(), 400);
    registry.toggle_pin("D:/repos/alpha".into());
    store.save(&registry).unwrap();

    let loaded = store.load().unwrap();
    assert_eq!(loaded.recent[0].repo_root, std::path::PathBuf::from("D:/repos/beta"));
    assert_eq!(loaded.recent[1].repo_root, std::path::PathBuf::from("D:/repos/alpha"));
    assert_eq!(loaded.pinned, vec![std::path::PathBuf::from("D:/repos/alpha")]);
}

#[test]
fn real_service_loads_branch_head_and_default_remote_flags() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let service = RealRepositoryService::default();

    let card = service
        .init_repository(repo_root.clone(), "secret".into(), "main".into())
        .unwrap();

    assert_eq!(card.branch_name, "main");
    assert_eq!(card.repo_root, repo_root);
    assert!(!card.remote_configured);
}
```

- [ ] **Step 2: Run the focused tests to verify RED**

Run:

```powershell
cargo test -p e2v-gui registry_round_trips_pinned_and_recent_entries_in_mru_order
cargo test -p e2v-gui real_service_loads_branch_head_and_default_remote_flags
```

Expected: FAIL because the registry store and repository service do not exist yet.

- [ ] **Step 3: Implement the registry types and the first real service adapter**

```rust
// crates/e2v-gui/src/domain.rs
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

    pub fn from_sdk(error: e2v_api::SdkError) -> Self {
        Self {
            code: "sdk",
            message: error.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RecentRepositoryEntry {
    pub repo_root: std::path::PathBuf,
    pub last_opened_unix_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RepositoryRegistry {
    pub pinned: Vec<std::path::PathBuf>,
    pub recent: Vec<RecentRepositoryEntry>,
}

impl RepositoryRegistry {
    pub fn touch_recent(&mut self, repo_root: std::path::PathBuf, last_opened_unix_ms: u64) {
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

    pub fn toggle_pin(&mut self, repo_root: std::path::PathBuf) {
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
    pub repo_root: std::path::PathBuf,
    pub display_name: String,
    pub branch_name: String,
    pub head_snapshot_id: Option<String>,
    pub remote_configured: bool,
}
```

```rust
// crates/e2v-gui/src/services/repository_service.rs
pub trait RepositoryService: Send + Sync + 'static {
    fn init_repository(
        &self,
        repo_root: std::path::PathBuf,
        password: String,
        branch_name: String,
    ) -> Result<crate::domain::RepositoryHomeCard, crate::domain::AppError>;

    fn open_repository(
        &self,
        repo_root: std::path::PathBuf,
    ) -> Result<crate::domain::RepositoryHomeCard, crate::domain::AppError>;

    fn clone_repository(
        &self,
        remote_spec: String,
        target_repo_root: std::path::PathBuf,
        password: String,
        branch_token: String,
    ) -> Result<crate::domain::RepositoryHomeCard, crate::domain::AppError>;

    fn load_repository_summary(
        &self,
        repo_root: std::path::PathBuf,
    ) -> Result<crate::domain::RepositoryHomeCard, crate::domain::AppError>;
}

#[derive(Default)]
pub struct RealRepositoryService {
    sdk: e2v_api::Sdk,
}

impl RepositoryService for RealRepositoryService {
    fn init_repository(
        &self,
        repo_root: std::path::PathBuf,
        password: String,
        branch_name: String,
    ) -> Result<crate::domain::RepositoryHomeCard, crate::domain::AppError> {
        self.sdk
            .init_repository(e2v_api::InitRepositoryOptions {
                repo_root: repo_root.clone(),
                password,
                branch_name,
            })
            .map_err(crate::domain::AppError::from_sdk)?;

        self.load_repository_summary(repo_root)
    }

    fn open_repository(
        &self,
        repo_root: std::path::PathBuf,
    ) -> Result<crate::domain::RepositoryHomeCard, crate::domain::AppError> {
        self.load_repository_summary(repo_root)
    }

    fn clone_repository(
        &self,
        remote_spec: String,
        target_repo_root: std::path::PathBuf,
        password: String,
        branch_token: String,
    ) -> Result<crate::domain::RepositoryHomeCard, crate::domain::AppError> {
        self.sdk
            .clone_remote(e2v_api::CloneRequest {
                remote_spec,
                target_repo_root: target_repo_root.clone(),
                password,
                branch_token,
            })
            .map_err(crate::domain::AppError::from_sdk)?;

        self.load_repository_summary(target_repo_root)
    }

    fn load_repository_summary(
        &self,
        repo_root: std::path::PathBuf,
    ) -> Result<crate::domain::RepositoryHomeCard, crate::domain::AppError> {
        let repo = self
            .sdk
            .open_repository(&repo_root)
            .map_err(crate::domain::AppError::from_sdk)?;
        let snapshots = self
            .sdk
            .list_snapshots(&repo_root)
            .map_err(crate::domain::AppError::from_sdk)?;
        let remote_configured = self.sdk.load_default_remote(&repo_root).is_ok();

        Ok(crate::domain::RepositoryHomeCard {
            display_name: repo_root
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            repo_root,
            branch_name: repo.branch.name,
            head_snapshot_id: snapshots.first().map(|snapshot| snapshot.snapshot_id.clone()),
            remote_configured,
        })
    }
}
```

```rust
// crates/e2v-gui/src/testing.rs
#[derive(Default, Clone)]
pub struct FakeRepositoryService {
    opened: std::collections::HashMap<std::path::PathBuf, crate::domain::RepositoryHomeCard>,
}

impl FakeRepositoryService {
    pub fn with_open_result(
        repo_root: &str,
        branch_name: &str,
        head_snapshot_id: Option<&str>,
    ) -> Self {
        let mut service = Self::default();
        service.opened.insert(
            std::path::PathBuf::from(repo_root),
            crate::domain::RepositoryHomeCard {
                repo_root: repo_root.into(),
                display_name: std::path::Path::new(repo_root)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                branch_name: branch_name.into(),
                head_snapshot_id: head_snapshot_id.map(str::to_string),
                remote_configured: false,
            },
        );
        service
    }
}

impl crate::services::RepositoryService for FakeRepositoryService {
    fn init_repository(
        &self,
        repo_root: std::path::PathBuf,
        _password: String,
        branch_name: String,
    ) -> Result<crate::domain::RepositoryHomeCard, crate::domain::AppError> {
        Ok(crate::domain::RepositoryHomeCard {
            display_name: repo_root.file_name().unwrap_or_default().to_string_lossy().into_owned(),
            repo_root,
            branch_name,
            head_snapshot_id: None,
            remote_configured: false,
        })
    }

    fn open_repository(
        &self,
        repo_root: std::path::PathBuf,
    ) -> Result<crate::domain::RepositoryHomeCard, crate::domain::AppError> {
        self.load_repository_summary(repo_root)
    }

    fn clone_repository(
        &self,
        _remote_spec: String,
        target_repo_root: std::path::PathBuf,
        _password: String,
        _branch_token: String,
    ) -> Result<crate::domain::RepositoryHomeCard, crate::domain::AppError> {
        self.load_repository_summary(target_repo_root)
    }

    fn load_repository_summary(
        &self,
        repo_root: std::path::PathBuf,
    ) -> Result<crate::domain::RepositoryHomeCard, crate::domain::AppError> {
        self.opened
            .get(&repo_root)
            .cloned()
            .ok_or_else(|| crate::domain::AppError::internal("missing fake repository summary"))
    }
}
```

```rust
// crates/e2v-gui/src/services/mod.rs
#[derive(Clone)]
pub struct AppServices {
    pub repository: std::sync::Arc<dyn repository_service::RepositoryService>,
}

impl AppServices {
    pub fn real() -> Self {
        Self {
            repository: std::sync::Arc::new(repository_service::RealRepositoryService::default()),
        }
    }
}
```

```rust
// crates/e2v-gui/src/app.rs
pub struct PsycGuiApp {
    pub screen: crate::domain::Screen,
    pub selected_repository: Option<std::path::PathBuf>,
    pub registry: crate::domain::RepositoryRegistry,
    pub services: crate::services::AppServices,
}

pub fn boot() -> (PsycGuiApp, iced::Task<crate::domain::Message>) {
    (
        PsycGuiApp {
            screen: crate::domain::Screen::Home,
            selected_repository: None,
            registry: crate::domain::RepositoryRegistry::default(),
            services: crate::services::AppServices::real(),
        },
        iced::Task::none(),
    )
}
```

- [ ] **Step 4: Re-run the focused tests to verify GREEN**

Run:

```powershell
cargo test -p e2v-gui registry_round_trips_pinned_and_recent_entries_in_mru_order
cargo test -p e2v-gui real_service_loads_branch_head_and_default_remote_flags
```

Expected: PASS

- [ ] **Step 5: Commit the persistence and service foundation**

```powershell
 git add crates/e2v-gui/src/app.rs crates/e2v-gui/src/domain.rs crates/e2v-gui/src/testing.rs crates/e2v-gui/src/services crates/e2v-gui/tests/registry.rs crates/e2v-gui/src/lib.rs
git commit -m "feat: add gui registry and repository services"
```

## Task 3: Build The Multi-Repository Home And Background Home Jobs

**Files:**
- Modify: `Cargo.toml`
- Modify: `crates/e2v-gui/Cargo.toml`
- Modify: `crates/e2v-gui/src/app.rs`
- Modify: `crates/e2v-gui/src/domain.rs`
- Modify: `crates/e2v-gui/src/testing.rs`
- Create: `crates/e2v-gui/src/jobs.rs`
- Create: `crates/e2v-gui/src/pages/home.rs`
- Create: `crates/e2v-gui/tests/home.rs`

**Interfaces:**
- Produces: `pub struct HomeState`
- Produces: `pub enum HomeMessage`
- Produces: `pub enum HomeJobResult`
- Produces: `pub fn spawn_blocking_job<T: Send + 'static>(...) -> iced::Task<Message>`
- Produces: `pub fn boot_with_service(service: FakeRepositoryService) -> AppHarness`
- Produces: `pub fn advance(app: &mut PsycGuiApp, message: Message) -> iced::Task<Message>`
- Produces: selecting a home card transitions the app into `Screen::Workbench`

- [ ] **Step 1: Write the failing home-flow tests**

```rust
use e2v_gui::pages::home::{HomeMessage, NewRepositoryForm};
use e2v_gui::testing::{advance, FakeRepositoryService};

#[test]
fn submit_create_repository_requires_a_password_before_dispatch() {
    let mut harness = e2v_gui::testing::boot_with_service(FakeRepositoryService::default());
    harness.app.home.new_repository = NewRepositoryForm {
        repo_root_text: "D:/repos/demo".into(),
        password_text: String::new(),
        branch_name_text: "main".into(),
    };

    let task = advance(&mut harness.app, HomeMessage::SubmitCreateRepository.into());

    assert_eq!(task.units(), 0);
    assert_eq!(
        harness.app.home.validation_error.as_deref(),
        Some("Password is required")
    );
}

#[test]
fn successful_open_repository_enters_the_workbench_and_updates_recent_registry() {
    let service = FakeRepositoryService::with_open_result("D:/repos/demo", "main", Some("snap-1"));
    let mut harness = e2v_gui::testing::boot_with_service(service);
    harness.app.home.open_repository_path = "D:/repos/demo".into();

    let _ = advance(&mut harness.app, HomeMessage::SubmitOpenRepository.into());
    let _ = advance(&mut harness.app, e2v_gui::Message::HomeJobFinished(Ok(
        e2v_gui::pages::home::HomeJobResult::RepositoryLoaded(
            harness.service.load_repository_summary("D:/repos/demo".into()).unwrap(),
        ),
    )));

    assert_eq!(harness.app.selected_repository.as_deref(), Some(std::path::Path::new("D:/repos/demo")));
    assert!(matches!(harness.app.screen, e2v_gui::Screen::Workbench));
    assert_eq!(harness.app.registry.recent[0].repo_root, std::path::PathBuf::from("D:/repos/demo"));
}
```

- [ ] **Step 2: Run the focused home tests to verify RED**

Run:

```powershell
cargo test -p e2v-gui submit_create_repository_requires_a_password_before_dispatch
cargo test -p e2v-gui successful_open_repository_enters_the_workbench_and_updates_recent_registry
```

Expected: FAIL because the home page, home forms, and background job wiring do not exist yet.

- [ ] **Step 3: Implement the home state, job launcher, and workbench transition**

```rust
// crates/e2v-gui/src/jobs.rs
pub fn spawn_blocking_job<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, crate::domain::AppError> + Send + 'static,
    map: impl FnOnce(Result<T, crate::domain::AppError>) -> crate::domain::Message + Send + 'static,
) -> iced::Task<crate::domain::Message> {
    iced::Task::perform(
        async move {
            let (sender, receiver) = futures::channel::oneshot::channel();
            std::thread::spawn(move || {
                let _ = sender.send(work());
            });
            receiver
                .await
                .unwrap_or_else(|_| Err(crate::domain::AppError::internal("background job dropped")))
        },
        map,
    )
}
```

```rust
// crates/e2v-gui/src/testing.rs
pub struct AppHarness {
    pub app: crate::app::PsycGuiApp,
    pub service: FakeRepositoryService,
}

pub fn boot_with_service(service: FakeRepositoryService) -> AppHarness {
    let (mut app, _) = crate::boot();
    app.services.repository = std::sync::Arc::new(service.clone());
    AppHarness { app, service }
}

pub fn advance(
    app: &mut crate::app::PsycGuiApp,
    message: crate::domain::Message,
) -> iced::Task<crate::domain::Message> {
    crate::app::update(app, message)
}
```

```rust
// crates/e2v-gui/src/app.rs
pub struct PsycGuiApp {
    pub screen: crate::domain::Screen,
    pub selected_repository: Option<std::path::PathBuf>,
    pub registry: crate::domain::RepositoryRegistry,
    pub services: crate::services::AppServices,
    pub home: crate::pages::home::HomeState,
}
```

```rust
// crates/e2v-gui/src/pages/home.rs
#[derive(Debug, Clone, Default)]
pub struct NewRepositoryForm {
    pub repo_root_text: String,
    pub password_text: String,
    pub branch_name_text: String,
}

#[derive(Debug, Clone, Default)]
pub struct HomeState {
    pub cards: Vec<crate::domain::RepositoryHomeCard>,
    pub open_repository_path: String,
    pub validation_error: Option<String>,
    pub new_repository: NewRepositoryForm,
}

#[derive(Debug, Clone)]
pub enum HomeMessage {
    SubmitCreateRepository,
    SubmitOpenRepository,
    SelectRepository(std::path::PathBuf),
}

#[derive(Debug, Clone)]
pub enum HomeJobResult {
    RepositoryLoaded(crate::domain::RepositoryHomeCard),
}

pub fn update_home(
    app: &mut crate::app::PsycGuiApp,
    message: HomeMessage,
) -> iced::Task<crate::domain::Message> {
    match message {
        HomeMessage::SubmitCreateRepository => {
            if app.home.new_repository.password_text.trim().is_empty() {
                app.home.validation_error = Some("Password is required".into());
                return iced::Task::none();
            }
            let repo_root = std::path::PathBuf::from(app.home.new_repository.repo_root_text.trim());
            let password = app.home.new_repository.password_text.clone();
            let branch_name = app.home.new_repository.branch_name_text.clone();
            let service = app.services.repository.clone();
            crate::jobs::spawn_blocking_job(
                move || service.init_repository(repo_root, password, branch_name),
                |result| crate::domain::Message::HomeJobFinished(result.map(HomeJobResult::RepositoryLoaded)),
            )
        }
        HomeMessage::SubmitOpenRepository => {
            let repo_root = std::path::PathBuf::from(app.home.open_repository_path.trim());
            let service = app.services.repository.clone();
            crate::jobs::spawn_blocking_job(
                move || service.open_repository(repo_root),
                |result| crate::domain::Message::HomeJobFinished(result.map(HomeJobResult::RepositoryLoaded)),
            )
        }
        HomeMessage::SelectRepository(repo_root) => {
            app.selected_repository = Some(repo_root);
            app.screen = crate::domain::Screen::Workbench;
            iced::Task::none()
        }
    }
}
```

- [ ] **Step 4: Re-run the focused home tests to verify GREEN**

Run:

```powershell
cargo test -p e2v-gui submit_create_repository_requires_a_password_before_dispatch
cargo test -p e2v-gui successful_open_repository_enters_the_workbench_and_updates_recent_registry
```

Expected: PASS

- [ ] **Step 5: Commit the home and background-job foundation**

```powershell
git add Cargo.toml crates/e2v-gui/Cargo.toml crates/e2v-gui/src/app.rs crates/e2v-gui/src/domain.rs crates/e2v-gui/src/jobs.rs crates/e2v-gui/src/pages/home.rs crates/e2v-gui/tests/home.rs
git commit -m "feat: add gui home page and background jobs"
```

## Task 4: Add The Workbench Shell, Overview Page, And Job Drawer

**Files:**
- Modify: `crates/e2v-gui/src/app.rs`
- Modify: `crates/e2v-gui/src/domain.rs`
- Modify: `crates/e2v-gui/src/testing.rs`
- Modify: `crates/e2v-gui/src/services/repository_service.rs`
- Create: `crates/e2v-gui/src/pages/workbench.rs`
- Create: `crates/e2v-gui/src/pages/overview.rs`
- Create: `crates/e2v-gui/src/widgets/job_drawer.rs`
- Create: `crates/e2v-gui/tests/overview.rs`

**Interfaces:**
- Produces: `pub enum WorkbenchPage { Overview, History, Branches, Sync }`
- Produces: `pub struct WorkbenchState`
- Produces: `pub struct JobRecord`
- Produces: `pub fn boot_into_workbench(service: FakeRepositoryService, repo_root: &str) -> AppHarness`
- Produces: `fn commit_repository(&self, repo_root: PathBuf, message: String) -> Result<e2v_api::CommitInfo, AppError>`
- Produces: overview commit submits a background job and refreshes the selected repository card

- [ ] **Step 1: Write the failing overview tests**

```rust
use e2v_gui::pages::overview::OverviewMessage;
use e2v_gui::testing::{advance, FakeRepositoryService};

#[test]
fn submitting_commit_adds_a_running_job_record() {
    let service = FakeRepositoryService::with_open_result("D:/repos/demo", "main", Some("snap-1"));
    let mut harness = e2v_gui::testing::boot_into_workbench(service, "D:/repos/demo");
    harness.app.workbench.overview.commit_message = "snapshot 2".into();

    let _ = advance(&mut harness.app, OverviewMessage::SubmitCommit.into());

    assert_eq!(harness.app.jobs.len(), 1);
    assert_eq!(harness.app.jobs[0].label, "Commit repository");
    assert!(matches!(harness.app.jobs[0].state, e2v_gui::JobState::Running));
}

#[test]
fn successful_commit_refreshes_the_head_snapshot_in_overview() {
    let service = FakeRepositoryService::with_commit_result(
        "D:/repos/demo",
        "main",
        "snap-2",
        "snapshot 2",
    );
    let mut harness = e2v_gui::testing::boot_into_workbench(service, "D:/repos/demo");

    let _ = advance(&mut harness.app, e2v_gui::Message::OverviewJobFinished(Ok(
        e2v_gui::pages::overview::OverviewJobResult::Committed {
            repo_root: "D:/repos/demo".into(),
            head_snapshot_id: "snap-2".into(),
            last_message: "snapshot 2".into(),
        },
    )));

    assert_eq!(
        harness.app.workbench.overview.head_snapshot_id.as_deref(),
        Some("snap-2")
    );
}
```

- [ ] **Step 2: Run the focused overview tests to verify RED**

Run:

```powershell
cargo test -p e2v-gui submitting_commit_adds_a_running_job_record
cargo test -p e2v-gui successful_commit_refreshes_the_head_snapshot_in_overview
```

Expected: FAIL because the workbench shell, overview state, and job drawer do not exist yet.

- [ ] **Step 3: Implement the workbench state, overview commit flow, and drawer records**

```rust
// crates/e2v-gui/src/testing.rs
impl FakeRepositoryService {
    pub fn with_commit_result(
        repo_root: &str,
        branch_name: &str,
        head_snapshot_id: &str,
        _message: &str,
    ) -> Self {
        Self::with_open_result(repo_root, branch_name, Some(head_snapshot_id))
    }
}

pub fn boot_into_workbench(service: FakeRepositoryService, repo_root: &str) -> AppHarness {
    let mut harness = boot_with_service(service);
    harness.app.selected_repository = Some(repo_root.into());
    harness.app.screen = crate::domain::Screen::Workbench;
    harness
}
```

```rust
// crates/e2v-gui/src/domain.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkbenchPage {
    Overview,
    History,
    Branches,
    Sync,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobState {
    Running,
    Succeeded,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRecord {
    pub id: u64,
    pub label: String,
    pub repo_root: Option<std::path::PathBuf>,
    pub state: JobState,
}
```

```rust
// crates/e2v-gui/src/pages/workbench.rs
#[derive(Debug, Clone)]
pub struct WorkbenchState {
    pub active_page: crate::domain::WorkbenchPage,
    pub branch_token: String,
    pub overview: crate::pages::overview::OverviewState,
    pub history: crate::pages::history::HistoryState,
    pub branches: crate::pages::branches::BranchesState,
    pub sync: crate::pages::sync::SyncState,
}
```

```rust
// crates/e2v-gui/src/app.rs
pub struct PsycGuiApp {
    pub screen: crate::domain::Screen,
    pub selected_repository: Option<std::path::PathBuf>,
    pub registry: crate::domain::RepositoryRegistry,
    pub services: crate::services::AppServices,
    pub home: crate::pages::home::HomeState,
    pub workbench: crate::pages::workbench::WorkbenchState,
    pub jobs: Vec<crate::domain::JobRecord>,
}
```

```rust
// crates/e2v-gui/src/services/repository_service.rs
// extend the existing RepositoryService trait and the existing
// impl RepositoryService for RealRepositoryService with:
fn commit_repository(
    &self,
    repo_root: std::path::PathBuf,
    message: String,
) -> Result<e2v_api::CommitInfo, crate::domain::AppError> {
    self.sdk
        .commit_repository(e2v_api::CommitRepositoryOptions { repo_root, message })
        .map_err(crate::domain::AppError::from_sdk)
}
```

```rust
// crates/e2v-gui/src/pages/overview.rs
#[derive(Debug, Clone, Default)]
pub struct OverviewState {
    pub branch_name: String,
    pub head_snapshot_id: Option<String>,
    pub commit_message: String,
}

#[derive(Debug, Clone)]
pub enum OverviewMessage {
    SubmitCommit,
}

#[derive(Debug, Clone)]
pub enum OverviewJobResult {
    Committed {
        repo_root: std::path::PathBuf,
        head_snapshot_id: String,
        last_message: String,
    },
}

pub fn submit_commit(app: &mut crate::app::PsycGuiApp) -> iced::Task<crate::domain::Message> {
    let repo_root = app.selected_repository.clone().expect("selected repository");
    let commit_message = app.workbench.overview.commit_message.clone();
    let service = app.services.repository.clone();
    let job_id = app.allocate_job_id();
    app.jobs.push(crate::domain::JobRecord {
        id: job_id,
        label: "Commit repository".into(),
        repo_root: Some(repo_root.clone()),
        state: crate::domain::JobState::Running,
    });

    crate::jobs::spawn_blocking_job(
        move || service.commit_repository(repo_root.clone(), commit_message.clone()),
        move |result| crate::domain::Message::OverviewJobFinished(result.map(|commit| {
            OverviewJobResult::Committed {
                repo_root,
                head_snapshot_id: commit.snapshot_id,
                last_message: commit_message,
            }
        })),
    )
}
```

- [ ] **Step 4: Re-run the focused overview tests to verify GREEN**

Run:

```powershell
cargo test -p e2v-gui submitting_commit_adds_a_running_job_record
cargo test -p e2v-gui successful_commit_refreshes_the_head_snapshot_in_overview
```

Expected: PASS

- [ ] **Step 5: Commit the workbench and overview foundation**

```powershell
git add crates/e2v-gui/src/app.rs crates/e2v-gui/src/domain.rs crates/e2v-gui/src/services/repository_service.rs crates/e2v-gui/src/pages/workbench.rs crates/e2v-gui/src/pages/overview.rs crates/e2v-gui/src/widgets/job_drawer.rs crates/e2v-gui/tests/overview.rs
git commit -m "feat: add gui workbench and overview page"
```

## Task 5: Implement The History And Branches Pages

**Files:**
- Modify: `crates/e2v-gui/src/app.rs`
- Modify: `crates/e2v-gui/src/domain.rs`
- Modify: `crates/e2v-gui/src/testing.rs`
- Modify: `crates/e2v-gui/src/services/repository_service.rs`
- Create: `crates/e2v-gui/src/pages/history.rs`
- Create: `crates/e2v-gui/src/pages/branches.rs`
- Create: `crates/e2v-gui/tests/history_branches.rs`

**Interfaces:**
- Produces: `fn list_snapshots(&self, repo_root: PathBuf) -> Result<Vec<SnapshotRow>, AppError>`
- Produces: `fn checkout_snapshot(&self, repo_root: PathBuf, snapshot_id: String, target_dir: PathBuf) -> Result<(), AppError>`
- Produces: `fn list_branches(&self, repo_root: PathBuf) -> Result<Vec<BranchRow>, AppError>`
- Produces: `fn create_branch(&self, repo_root: PathBuf, name: String) -> Result<BranchRow, AppError>`
- Produces: `fn checkout_branch(&self, repo_root: PathBuf, name: String) -> Result<RepositoryHomeCard, AppError>`
- Produces: `fn delete_branch(&self, repo_root: PathBuf, name: String) -> Result<(), AppError>`

- [ ] **Step 1: Write the failing history and branch tests**

```rust
use e2v_gui::pages::branches::BranchesMessage;
use e2v_gui::pages::history::HistoryMessage;
use e2v_gui::testing::{advance, FakeRepositoryService};

#[test]
fn history_checkout_requires_a_target_directory() {
    let service = FakeRepositoryService::with_snapshot_list("D:/repos/demo", vec!["snap-1"]);
    let mut harness = e2v_gui::testing::boot_into_workbench(service, "D:/repos/demo");
    harness.app.workbench.history.selected_snapshot_id = Some("snap-1".into());
    harness.app.workbench.history.checkout_target_dir = String::new();

    let task = advance(&mut harness.app, HistoryMessage::SubmitCheckout.into());

    assert_eq!(task.units(), 0);
    assert_eq!(
        harness.app.workbench.history.validation_error.as_deref(),
        Some("Checkout target directory is required")
    );
}

#[test]
fn branch_create_checkout_and_delete_refresh_the_branch_table() {
    let service = FakeRepositoryService::with_branch_table(
        "D:/repos/demo",
        vec!["main", "feature-a"],
    );
    let mut harness = e2v_gui::testing::boot_into_workbench(service, "D:/repos/demo");

    let _ = advance(&mut harness.app, BranchesMessage::CreateBranch("feature-b".into()).into());
    let _ = advance(&mut harness.app, BranchesMessage::CheckoutBranch("feature-b".into()).into());
    let _ = advance(&mut harness.app, BranchesMessage::DeleteBranch("feature-a".into()).into());

    assert_eq!(harness.app.workbench.branches.rows[0].name, "feature-b");
    assert_eq!(harness.app.workbench.overview.branch_name, "feature-b");
}
```

- [ ] **Step 2: Run the focused tests to verify RED**

Run:

```powershell
cargo test -p e2v-gui history_checkout_requires_a_target_directory
cargo test -p e2v-gui branch_create_checkout_and_delete_refresh_the_branch_table
```

Expected: FAIL because the history and branches pages do not exist yet.

- [ ] **Step 3: Implement the history and branch service calls and page state**

```rust
// crates/e2v-gui/src/testing.rs
#[derive(Default, Clone)]
pub struct FakeRepositoryService {
    opened: std::collections::HashMap<std::path::PathBuf, crate::domain::RepositoryHomeCard>,
    snapshot_rows: Vec<crate::pages::history::SnapshotRow>,
    branch_rows: Vec<crate::pages::branches::BranchRow>,
}

impl FakeRepositoryService {
    pub fn with_snapshot_list(repo_root: &str, snapshot_ids: Vec<&str>) -> Self {
        let mut service = Self::with_open_result(repo_root, "main", snapshot_ids.first().copied());
        service.snapshot_rows = snapshot_ids
            .into_iter()
            .map(|snapshot_id| crate::pages::history::SnapshotRow {
                snapshot_id: snapshot_id.to_string(),
                message: format!("message-{snapshot_id}"),
            })
            .collect();
        service
    }

    pub fn with_branch_table(repo_root: &str, branch_names: Vec<&str>) -> Self {
        let mut service = Self::with_open_result(repo_root, branch_names[0], Some("snap-1"));
        service.branch_rows = branch_names
            .into_iter()
            .enumerate()
            .map(|(index, name)| crate::pages::branches::BranchRow {
                name: name.to_string(),
                head_snapshot_id: Some(format!("snap-{}", index + 1)),
                is_current: index == 0,
            })
            .collect();
        service
    }
}

// extend the existing impl RepositoryService for FakeRepositoryService with:
fn list_snapshots(
    &self,
    _repo_root: std::path::PathBuf,
) -> Result<Vec<crate::pages::history::SnapshotRow>, crate::domain::AppError> {
    Ok(self.snapshot_rows.clone())
}

fn list_branches(
    &self,
    _repo_root: std::path::PathBuf,
) -> Result<Vec<crate::pages::branches::BranchRow>, crate::domain::AppError> {
    Ok(self.branch_rows.clone())
}
```

```rust
// crates/e2v-gui/src/services/repository_service.rs
// extend the existing RepositoryService trait and the existing
// impl RepositoryService for RealRepositoryService with:
fn list_snapshots(
    &self,
    repo_root: std::path::PathBuf,
) -> Result<Vec<crate::pages::history::SnapshotRow>, crate::domain::AppError> {
    self.sdk
        .list_snapshots(&repo_root)
        .map(|items| {
            items
                .into_iter()
                .map(|snapshot| crate::pages::history::SnapshotRow {
                    snapshot_id: snapshot.snapshot_id,
                    message: snapshot.message,
                })
                .collect()
        })
        .map_err(crate::domain::AppError::from_sdk)
}

fn list_branches(
    &self,
    repo_root: std::path::PathBuf,
) -> Result<Vec<crate::pages::branches::BranchRow>, crate::domain::AppError> {
    self.sdk
        .list_branches(&repo_root)
        .map(|items| {
            items
                .into_iter()
                .map(|branch| crate::pages::branches::BranchRow {
                    name: branch.name,
                    head_snapshot_id: branch.head_snapshot_id,
                    is_current: branch.is_current,
                })
                .collect()
        })
        .map_err(crate::domain::AppError::from_sdk)
}
```

```rust
// crates/e2v-gui/src/pages/history.rs
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotRow {
    pub snapshot_id: String,
    pub message: String,
}

#[derive(Debug, Clone, Default)]
pub struct HistoryState {
    pub rows: Vec<SnapshotRow>,
    pub selected_snapshot_id: Option<String>,
    pub checkout_target_dir: String,
    pub validation_error: Option<String>,
}
```

```rust
// crates/e2v-gui/src/pages/branches.rs
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchRow {
    pub name: String,
    pub head_snapshot_id: Option<String>,
    pub is_current: bool,
}

#[derive(Debug, Clone, Default)]
pub struct BranchesState {
    pub rows: Vec<BranchRow>,
    pub create_branch_name: String,
}
```

- [ ] **Step 4: Re-run the focused tests to verify GREEN**

Run:

```powershell
cargo test -p e2v-gui history_checkout_requires_a_target_directory
cargo test -p e2v-gui branch_create_checkout_and_delete_refresh_the_branch_table
```

Expected: PASS

- [ ] **Step 5: Commit the history and branches pages**

```powershell
git add crates/e2v-gui/src/app.rs crates/e2v-gui/src/domain.rs crates/e2v-gui/src/services/repository_service.rs crates/e2v-gui/src/pages/history.rs crates/e2v-gui/src/pages/branches.rs crates/e2v-gui/tests/history_branches.rs
git commit -m "feat: add gui history and branches pages"
```

## Task 6: Implement The Sync Page And Safety Confirmation Flow

**Files:**
- Modify: `crates/e2v-gui/src/app.rs`
- Modify: `crates/e2v-gui/src/domain.rs`
- Modify: `crates/e2v-gui/src/services/repository_service.rs`
- Create: `crates/e2v-gui/src/pages/sync.rs`
- Create: `crates/e2v-gui/src/widgets/confirmation_sheet.rs`
- Create: `crates/e2v-gui/tests/sync.rs`

**Interfaces:**
- Produces: `fn add_remote(&self, repo_root: PathBuf, name: String, spec: String) -> Result<(), AppError>`
- Produces: `fn push_default_remote(&self, repo_root: PathBuf, branch_token: String) -> Result<e2v_api::PushResponse, AppError>`
- Produces: `fn push_default_remote_allowing_single_writer_risk(&self, repo_root: PathBuf, branch_token: String) -> Result<e2v_api::PushResponse, AppError>`
- Produces: `fn fetch_default_remote(&self, repo_root: PathBuf, branch_token: String) -> Result<e2v_api::FetchResponse, AppError>`
- Produces: `fn pull_default_remote(&self, repo_root: PathBuf, branch_token: String) -> Result<e2v_api::PullResponse, AppError>`
- Produces: `pub enum PendingConfirmation`
- Produces: single-writer-risk push launches only after explicit confirmation

- [ ] **Step 1: Write the failing sync tests**

```rust
use e2v_gui::pages::sync::SyncMessage;
use e2v_gui::testing::{advance, FakeRepositoryService};

#[test]
fn remote_add_requires_a_name_and_spec_before_dispatch() {
    let service = FakeRepositoryService::with_open_result("D:/repos/demo", "main", Some("snap-1"));
    let mut harness = e2v_gui::testing::boot_into_workbench(service, "D:/repos/demo");

    let task = advance(&mut harness.app, SyncMessage::SubmitAddRemote.into());

    assert_eq!(task.units(), 0);
    assert_eq!(
        harness.app.workbench.sync.validation_error.as_deref(),
        Some("Remote name and spec are required")
    );
}

#[test]
fn single_writer_risk_push_requires_confirmation_before_launching_the_job() {
    let service = FakeRepositoryService::with_open_result("D:/repos/demo", "main", Some("snap-1"));
    let mut harness = e2v_gui::testing::boot_into_workbench(service, "D:/repos/demo");

    let task = advance(&mut harness.app, SyncMessage::SubmitPushWithSingleWriterRisk.into());

    assert_eq!(task.units(), 0);
    assert!(matches!(
        harness.app.pending_confirmation,
        Some(e2v_gui::PendingConfirmation::SingleWriterRiskPush { .. })
    ));
    assert!(harness.app.jobs.is_empty());
}
```

- [ ] **Step 2: Run the focused sync tests to verify RED**

Run:

```powershell
cargo test -p e2v-gui remote_add_requires_a_name_and_spec_before_dispatch
cargo test -p e2v-gui single_writer_risk_push_requires_confirmation_before_launching_the_job
```

Expected: FAIL because the sync page and confirmation sheet do not exist yet.

- [ ] **Step 3: Implement the sync state, remote add/push/fetch/pull jobs, and confirmation model**

```rust
// crates/e2v-gui/src/domain.rs
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingConfirmation {
    SingleWriterRiskPush {
        repo_root: std::path::PathBuf,
        branch_token: String,
    },
}
```

```rust
// crates/e2v-gui/src/app.rs
pub struct PsycGuiApp {
    pub screen: crate::domain::Screen,
    pub selected_repository: Option<std::path::PathBuf>,
    pub registry: crate::domain::RepositoryRegistry,
    pub services: crate::services::AppServices,
    pub home: crate::pages::home::HomeState,
    pub workbench: crate::pages::workbench::WorkbenchState,
    pub jobs: Vec<crate::domain::JobRecord>,
    pub pending_confirmation: Option<crate::domain::PendingConfirmation>,
}
```

```rust
// crates/e2v-gui/src/services/repository_service.rs
// extend the existing RepositoryService trait and the existing
// impl RepositoryService for RealRepositoryService with:
fn add_remote(
    &self,
    repo_root: std::path::PathBuf,
    name: String,
    spec: String,
) -> Result<(), crate::domain::AppError> {
    self.sdk
        .add_remote(&repo_root, &name, &spec)
        .map(|_| ())
        .map_err(crate::domain::AppError::from_sdk)
}

fn push_default_remote(
    &self,
    repo_root: std::path::PathBuf,
    branch_token: String,
) -> Result<e2v_api::PushResponse, crate::domain::AppError> {
    self.sdk
        .push_default_remote(e2v_api::PushRequest {
            repo_root,
            branch_token,
            operation_id: "gui-push".into(),
        })
        .map_err(crate::domain::AppError::from_sdk)
}

fn push_default_remote_allowing_single_writer_risk(
    &self,
    repo_root: std::path::PathBuf,
    branch_token: String,
) -> Result<e2v_api::PushResponse, crate::domain::AppError> {
    self.sdk
        .push_default_remote_allowing_single_writer_risk(e2v_api::PushRequest {
            repo_root,
            branch_token,
            operation_id: "gui-push".into(),
        })
        .map_err(crate::domain::AppError::from_sdk)
}

fn fetch_default_remote(
    &self,
    repo_root: std::path::PathBuf,
    branch_token: String,
) -> Result<e2v_api::FetchResponse, crate::domain::AppError> {
    self.sdk
        .fetch_default_remote(e2v_api::FetchRequest {
            repo_root,
            branch_token,
            password: None,
        })
        .map_err(crate::domain::AppError::from_sdk)
}

fn pull_default_remote(
    &self,
    repo_root: std::path::PathBuf,
    branch_token: String,
) -> Result<e2v_api::PullResponse, crate::domain::AppError> {
    self.sdk
        .pull_default_remote(e2v_api::PullRequest {
            repo_root,
            branch_token,
            password: None,
        })
        .map_err(crate::domain::AppError::from_sdk)
}
```

```rust
// crates/e2v-gui/src/pages/sync.rs
#[derive(Debug, Clone, Default)]
pub struct SyncState {
    pub remote_name: String,
    pub remote_spec: String,
    pub validation_error: Option<String>,
}

#[derive(Debug, Clone)]
pub enum SyncMessage {
    SubmitAddRemote,
    SubmitPush,
    SubmitPushWithSingleWriterRisk,
    SubmitFetch,
    SubmitPull,
    ConfirmPendingAction,
    CancelPendingAction,
}

pub fn update_sync(
    app: &mut crate::app::PsycGuiApp,
    message: SyncMessage,
) -> iced::Task<crate::domain::Message> {
    match message {
        SyncMessage::SubmitAddRemote => {
            if app.workbench.sync.remote_name.trim().is_empty()
                || app.workbench.sync.remote_spec.trim().is_empty()
            {
                app.workbench.sync.validation_error =
                    Some("Remote name and spec are required".into());
                return iced::Task::none();
            }
            // dispatch add-remote job
            iced::Task::none()
        }
        SyncMessage::SubmitPushWithSingleWriterRisk => {
            app.pending_confirmation = Some(crate::domain::PendingConfirmation::SingleWriterRiskPush {
                repo_root: app.selected_repository.clone().expect("selected repository"),
                branch_token: app.workbench.branch_token.clone(),
            });
            iced::Task::none()
        }
        SyncMessage::ConfirmPendingAction => {
            let Some(crate::domain::PendingConfirmation::SingleWriterRiskPush {
                repo_root,
                branch_token,
            }) = app.pending_confirmation.take() else {
                return iced::Task::none();
            };
            let service = app.services.repository.clone();
            crate::jobs::spawn_blocking_job(
                move || service.push_default_remote_allowing_single_writer_risk(repo_root, branch_token),
                crate::domain::Message::SyncJobFinished,
            )
        }
        SyncMessage::CancelPendingAction => {
            app.pending_confirmation = None;
            iced::Task::none()
        }
        _ => iced::Task::none(),
    }
}
```

- [ ] **Step 4: Re-run the focused sync tests to verify GREEN**

Run:

```powershell
cargo test -p e2v-gui remote_add_requires_a_name_and_spec_before_dispatch
cargo test -p e2v-gui single_writer_risk_push_requires_confirmation_before_launching_the_job
```

Expected: PASS

- [ ] **Step 5: Commit the sync page and confirmation flow**

```powershell
git add crates/e2v-gui/src/app.rs crates/e2v-gui/src/domain.rs crates/e2v-gui/src/services/repository_service.rs crates/e2v-gui/src/pages/sync.rs crates/e2v-gui/src/widgets/confirmation_sheet.rs crates/e2v-gui/tests/sync.rs
git commit -m "feat: add gui sync page and confirmations"
```

## Task 7: Run The Phase 1 Verification Matrix

**Files:**
- Modify: any files touched by Tasks 1-6 only if verification finds a real defect

**Interfaces:**
- Consumes: all GUI state, service, and page modules from Tasks 1-6
- Produces: a verified Phase 1 implementation branch with passing crate tests and a clean diff limited to GUI work

- [ ] **Step 1: Run the focused GUI crate tests**

Run:

```powershell
cargo test -p e2v-gui
```

Expected: PASS for `smoke`, `registry`, `home`, `overview`, `history_branches`, and `sync`.

- [ ] **Step 2: Run the workspace test suite**

Run:

```powershell
cargo test --workspace
```

Expected: PASS with no regressions in the existing crates.

- [ ] **Step 3: Run the GUI crate once manually on the host**

Run:

```powershell
cargo run -p e2v-gui
```

Expected: the native window opens to the multi-repository home, workbench navigation is visible after selecting a repository, and submitting sync/commit actions does not freeze the window while jobs are running.

- [ ] **Step 4: Review the diff and keep only intentional Phase 1 GUI changes**

Run:

```powershell
git status --short
git diff --stat
git diff -- crates/e2v-gui Cargo.toml
```

Expected: only the new GUI crate, workspace registration, and any intentionally shared helper changes needed for Phase 1 are present. Unrelated `.gitignore` or downloaded VFS preview artifacts remain unstaged.

- [ ] **Step 5: Create the final Phase 1 implementation commit**

```powershell
git add Cargo.toml crates/e2v-gui
git commit -m "feat: add phase 1 iced gui workbench"
```
