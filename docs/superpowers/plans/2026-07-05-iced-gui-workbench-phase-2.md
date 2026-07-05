# Iced GUI Workbench Phase 2 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn the current state-only GUI shell into a visible, interactive desktop workbench and add the `Search`, `Sharing`, and `Preview` pages with reusable live service controllers.

**Architecture:** Phase 2 keeps `crates/e2v-gui` as a composition layer over library crates, but closes the current rendering gap by moving page view composition into focused widget/page modules instead of keeping `app::view` as a title-only placeholder. Search stays `RepositoryFacade`-backed, sharing stays `Sdk`-backed, and preview introduces explicit controller wrappers over `e2v_sync::ServeHandle` and `e2v_vfs::MountedFilesystem` so live resources have clear lifetimes in app state.

**Tech Stack:** Rust workspace, `iced 0.14`, `serde`, `serde_json`, `base64`, `tokio`, `e2v-api::Sdk`, `e2v_core::RepositoryFacade`, `e2v_sync::{ServeHandle, ServeOptions, serve_local_web}`, `e2v_vfs::{MountedFilesystem, MountLaunchSummary, start_live_branch_mount, start_snapshot_mount}`, focused crate tests in `crates/e2v-gui/tests`.

## Global Constraints

- The GUI is a standalone desktop application, not a browser UI and not an embedded CLI wrapper.
- The first screen is a multi-repository home, not a single-repo dashboard.
- Dangerous and low-frequency operations must be hidden by default and grouped into an advanced/maintenance area.
- Stability and safety take priority over visual cleverness or aggressive optimization.
- The GUI should model the current product surface only. It must not add new first-class concepts solely to preserve obsolete or compatibility-only interfaces.
- The GUI must call Rust interfaces directly for normal operations. It must not shell out to the CLI as its primary execution model.
- Phase 2 must make the existing Phase 1 shell actually visible and interactive instead of leaving `app::view` as a title-only placeholder.
- The `Preview` page should favor VFS-backed local preview workflows for repository files.
- The GUI should not maintain a second unrelated preview cache for repository files when a mounted VFS path already provides local access.
- Phase 2 covers `Search`, `Sharing`, `Preview`, and reusable live service controllers; `Advanced` remains Phase 3 work.

---

## File Structure

### Workspace

- Modify: `crates/e2v-gui/Cargo.toml`
  - add `base64.workspace = true`
  - add `e2v-core = { path = "../e2v-core" }`
  - add `e2v-sync = { path = "../e2v-sync" }`
  - add `e2v-vfs = { path = "../e2v-vfs" }`

### Existing GUI files

- Modify: `crates/e2v-gui/src/app.rs`
  - route new page messages
  - compose real home/workbench views
  - manage preview/share confirmation outcomes
  - own live controller cleanup on page transitions and app shutdown
- Modify: `crates/e2v-gui/src/domain.rs`
  - add `Search`, `Sharing`, `Preview`, and `Advanced` page identifiers
  - add share/search/preview view-model types
  - expand `PendingConfirmation`
- Modify: `crates/e2v-gui/src/lib.rs`
  - export new services, pages, and widgets
- Modify: `crates/e2v-gui/src/testing.rs`
  - add fake search, sharing, preview, and host-shell services
  - add richer harness builders for page-specific tests
- Modify: `crates/e2v-gui/src/services/mod.rs`
  - register repository, search, sharing, preview, and host-shell services in `AppServices`
- Modify: `crates/e2v-gui/src/pages/home.rs`
  - add real home-page view helpers
- Modify: `crates/e2v-gui/src/pages/overview.rs`
  - add real overview-page view helpers
- Modify: `crates/e2v-gui/src/pages/history.rs`
  - add real history-page view helpers
- Modify: `crates/e2v-gui/src/pages/branches.rs`
  - add real branches-page view helpers
- Modify: `crates/e2v-gui/src/pages/sync.rs`
  - add real sync-page view helpers
- Modify: `crates/e2v-gui/src/pages/workbench.rs`
  - extend state to carry search, sharing, preview, and advanced navigation
- Modify: `crates/e2v-gui/src/widgets/job_drawer.rs`
  - render the running/completed job list instead of only exposing helper counts
- Modify: `crates/e2v-gui/src/widgets/confirmation_sheet.rs`
  - render share-revoke and sync-risk confirmation sheets

### New services

- Create: `crates/e2v-gui/src/services/search_service.rs`
  - `RepositoryFacade`-backed filename/metadata search adapter
- Create: `crates/e2v-gui/src/services/sharing_service.rs`
  - `Sdk`-backed share roster and invite/revoke adapter
- Create: `crates/e2v-gui/src/services/preview_service.rs`
  - local web / VFS mount adapter and controller wrappers
- Create: `crates/e2v-gui/src/services/host_shell_service.rs`
  - thin host launcher for opening URLs and local paths

### New pages

- Create: `crates/e2v-gui/src/pages/search.rs`
  - search state, messages, and rendered results list
- Create: `crates/e2v-gui/src/pages/sharing.rs`
  - sharing roster, invite, accept, and revoke UI state
- Create: `crates/e2v-gui/src/pages/preview.rs`
  - local web panel, mount panel, and controller status UI

### New widgets

- Create: `crates/e2v-gui/src/widgets/home_screen.rs`
  - home view composition and card/form rendering
- Create: `crates/e2v-gui/src/widgets/workbench_shell.rs`
  - left-nav rail, repository header, page body switch, and drawer layout
- Create: `crates/e2v-gui/src/widgets/status_badge.rs`
  - shared badge rendering for status chips such as branch, remote, and mount mode

### New GUI tests

- Create: `crates/e2v-gui/tests/render_shell.rs`
- Create: `crates/e2v-gui/tests/search.rs`
- Create: `crates/e2v-gui/tests/sharing.rs`
- Create: `crates/e2v-gui/tests/preview.rs`

## Task 1: Render The Existing Home And Workbench Shell

**Files:**
- Modify: `crates/e2v-gui/Cargo.toml`
- Modify: `crates/e2v-gui/src/app.rs`
- Modify: `crates/e2v-gui/src/domain.rs`
- Modify: `crates/e2v-gui/src/lib.rs`
- Modify: `crates/e2v-gui/src/pages/home.rs`
- Modify: `crates/e2v-gui/src/pages/overview.rs`
- Modify: `crates/e2v-gui/src/pages/history.rs`
- Modify: `crates/e2v-gui/src/pages/branches.rs`
- Modify: `crates/e2v-gui/src/pages/sync.rs`
- Modify: `crates/e2v-gui/src/pages/workbench.rs`
- Modify: `crates/e2v-gui/src/widgets/job_drawer.rs`
- Create: `crates/e2v-gui/src/widgets/home_screen.rs`
- Create: `crates/e2v-gui/src/widgets/workbench_shell.rs`
- Create: `crates/e2v-gui/src/widgets/status_badge.rs`
- Create: `crates/e2v-gui/tests/render_shell.rs`

**Interfaces:**
- Produces: `pub struct HomeScreenModel`
- Produces: `pub struct WorkbenchShellModel`
- Produces: `pub fn build_home_screen_model(app: &PsycGuiApp) -> HomeScreenModel`
- Produces: `pub fn build_workbench_shell_model(app: &PsycGuiApp) -> WorkbenchShellModel`
- Produces: `pub fn view_home(app: &PsycGuiApp) -> iced::Element<'_, Message>`
- Produces: `pub fn view_workbench(app: &PsycGuiApp) -> iced::Element<'_, Message>`
- Produces: `pub enum WorkbenchPage { Overview, History, Branches, Sync, Search, Sharing, Preview, Advanced }`

- [ ] **Step 1: Write the failing shell-render tests**

```rust
use e2v_gui::testing::FakeRepositoryService;
use e2v_gui::widgets::home_screen::build_home_screen_model;
use e2v_gui::widgets::workbench_shell::build_workbench_shell_model;

#[test]
fn home_screen_model_includes_known_repository_cards_and_validation_errors() {
    let service = FakeRepositoryService::with_open_result("D:/repos/demo", "main", Some("snap-1"));
    let mut harness = e2v_gui::testing::boot_with_service(service.clone());
    harness.app.home.cards.push(
        service
            .load_repository_summary("D:/repos/demo".into())
            .unwrap(),
    );
    harness.app.home.validation_error = Some("Bad path".into());

    let model = build_home_screen_model(&harness.app);

    assert_eq!(model.cards[0].display_name, "demo");
    assert_eq!(model.cards[0].branch_name, "main");
    assert_eq!(model.validation_error.as_deref(), Some("Bad path"));
}

#[test]
fn workbench_shell_model_exposes_navigation_for_phase_two_pages() {
    let service = FakeRepositoryService::with_branch_table("D:/repos/demo", vec!["main"]);
    let harness = e2v_gui::testing::boot_into_workbench(service, "D:/repos/demo");

    let model = build_workbench_shell_model(&harness.app);

    assert!(model.nav_items.iter().any(|item| item.label == "Search"));
    assert!(model.nav_items.iter().any(|item| item.label == "Sharing"));
    assert!(model.nav_items.iter().any(|item| item.label == "Preview"));
}
```

- [ ] **Step 2: Run the focused shell tests to verify RED**

Run:

```powershell
cargo test -p e2v-gui home_screen_model_includes_known_repository_cards_and_validation_errors
cargo test -p e2v-gui workbench_shell_model_exposes_navigation_for_phase_two_pages
```

Expected: FAIL because the real shell widgets and expanded navigation do not exist yet.

- [ ] **Step 3: Implement the real home/workbench shell and page views**

```rust
// crates/e2v-gui/src/domain.rs
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
```

```rust
// crates/e2v-gui/src/widgets/home_screen.rs
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeCardModel {
    pub display_name: String,
    pub repo_root: std::path::PathBuf,
    pub branch_name: String,
    pub head_snapshot_id: Option<String>,
    pub remote_configured: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeScreenModel {
    pub cards: Vec<HomeCardModel>,
    pub validation_error: Option<String>,
}

pub fn build_home_screen_model(app: &crate::app::PsycGuiApp) -> HomeScreenModel {
    HomeScreenModel {
        cards: app
            .home
            .cards
            .iter()
            .map(|card| HomeCardModel {
                display_name: card.display_name.clone(),
                repo_root: card.repo_root.clone(),
                branch_name: card.branch_name.clone(),
                head_snapshot_id: card.head_snapshot_id.clone(),
                remote_configured: card.remote_configured,
            })
            .collect(),
        validation_error: app.home.validation_error.clone(),
    }
}

pub fn view_home(app: &crate::app::PsycGuiApp) -> iced::Element<'_, crate::domain::Message> {
    use iced::widget::{button, column, container, row, scrollable, text, text_input};

    let cards = build_home_screen_model(app)
        .cards
        .into_iter()
        .fold(column![text("Repositories").size(32)].spacing(16), |column, card| {
            column.push(
                button(
                    column![
                        text(card.display_name).size(22),
                        text(card.repo_root.display().to_string()),
                        row![
                            text(format!("Branch: {}", card.branch_name)),
                            text(format!(
                                "Head: {}",
                                card.head_snapshot_id.unwrap_or_else(|| "none".into())
                            )),
                            text(if card.remote_configured { "Remote: configured" } else { "Remote: missing" }),
                        ]
                        .spacing(12),
                    ]
                    .spacing(6),
                )
                .on_press(crate::pages::home::HomeMessage::SelectRepository(card.repo_root).into()),
            )
        });

    let forms = column![
        text_input("Open repository path", &app.home.open_repository_path)
            .on_input(crate::pages::home::HomeMessage::SetOpenRepositoryPath)
            .padding(10),
        button("Open repository").on_press(crate::pages::home::HomeMessage::SubmitOpenRepository.into()),
        text_input("Create repository path", &app.home.new_repository.repo_root_text)
            .on_input(crate::pages::home::HomeMessage::SetNewRepositoryPath)
            .padding(10),
        text_input("Password", &app.home.new_repository.password_text)
            .on_input(crate::pages::home::HomeMessage::SetNewRepositoryPassword)
            .padding(10),
        text_input("Branch", &app.home.new_repository.branch_name_text)
            .on_input(crate::pages::home::HomeMessage::SetNewRepositoryBranch)
            .padding(10),
        button("Create repository")
            .on_press(crate::pages::home::HomeMessage::SubmitCreateRepository.into()),
    ]
    .spacing(12);

    let content = if let Some(error) = app.home.validation_error.as_ref() {
        row![
            scrollable(cards).width(iced::Length::FillPortion(2)),
            column![text(error), forms]
                .spacing(12)
                .width(iced::Length::FillPortion(1)),
        ]
        .spacing(20)
    } else {
        row![
            scrollable(cards).width(iced::Length::FillPortion(2)),
            forms.width(iced::Length::FillPortion(1)),
        ]
        .spacing(20)
    };

    container(content).padding(24).into()
}
```

```rust
// crates/e2v-gui/src/widgets/workbench_shell.rs
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkbenchNavItem {
    pub label: &'static str,
    pub page: crate::domain::WorkbenchPage,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkbenchShellModel {
    pub repo_title: String,
    pub nav_items: Vec<WorkbenchNavItem>,
}

pub fn build_workbench_shell_model(app: &crate::app::PsycGuiApp) -> WorkbenchShellModel {
    let active_page = app.workbench.active_page;
    let nav = [
        (crate::domain::WorkbenchPage::Overview, "Overview"),
        (crate::domain::WorkbenchPage::History, "History"),
        (crate::domain::WorkbenchPage::Branches, "Branches"),
        (crate::domain::WorkbenchPage::Sync, "Sync"),
        (crate::domain::WorkbenchPage::Search, "Search"),
        (crate::domain::WorkbenchPage::Sharing, "Sharing"),
        (crate::domain::WorkbenchPage::Preview, "Preview"),
        (crate::domain::WorkbenchPage::Advanced, "Advanced"),
    ];

    WorkbenchShellModel {
        repo_title: app
            .selected_repository
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "No repository".into()),
        nav_items: nav
            .into_iter()
            .map(|(page, label)| WorkbenchNavItem {
                label,
                page,
                active: page == active_page,
            })
            .collect(),
    }
}

pub fn view_workbench(
    app: &crate::app::PsycGuiApp,
) -> iced::Element<'_, crate::domain::Message> {
    use iced::widget::{button, column, container, row, text};

    let model = build_workbench_shell_model(app);
    let nav = model.nav_items.into_iter().fold(
        column![text(model.repo_title).size(24)].spacing(10),
        |column, item| {
            let label = if item.active {
                format!("> {}", item.label)
            } else {
                item.label.to_string()
            };
            column.push(
                button(text(label)).on_press(crate::pages::workbench::WorkbenchMessage::SelectPage(item.page).into()),
            )
        },
    );

    let body = crate::pages::workbench::view_active_page(app);
    let drawer = crate::widgets::job_drawer::view_job_drawer(&app.jobs);

    container(
        row![
            container(nav).width(220).padding(16),
            column![body, drawer].width(iced::Length::Fill).spacing(16),
        ]
        .spacing(20),
    )
    .padding(20)
    .into()
}
```

```rust
// crates/e2v-gui/src/pages/workbench.rs
#[derive(Debug, Clone)]
pub enum WorkbenchMessage {
    SelectPage(crate::domain::WorkbenchPage),
}

impl From<WorkbenchMessage> for crate::domain::Message {
    fn from(message: WorkbenchMessage) -> Self {
        crate::domain::Message::Workbench(message)
    }
}

pub fn view_active_page(
    app: &crate::app::PsycGuiApp,
) -> iced::Element<'_, crate::domain::Message> {
    match app.workbench.active_page {
        crate::domain::WorkbenchPage::Overview => crate::pages::overview::view_overview(app),
        crate::domain::WorkbenchPage::History => crate::pages::history::view_history(app),
        crate::domain::WorkbenchPage::Branches => crate::pages::branches::view_branches(app),
        crate::domain::WorkbenchPage::Sync => crate::pages::sync::view_sync(app),
        crate::domain::WorkbenchPage::Search => crate::pages::search::view_search(app),
        crate::domain::WorkbenchPage::Sharing => crate::pages::sharing::view_sharing(app),
        crate::domain::WorkbenchPage::Preview => crate::pages::preview::view_preview(app),
        crate::domain::WorkbenchPage::Advanced => {
            iced::widget::container(iced::widget::text("Advanced (Phase 3)")).into()
        }
    }
}
```

```rust
// crates/e2v-gui/src/app.rs
pub fn update(app: &mut PsycGuiApp, message: Message) -> Task<Message> {
    match message {
        Message::Branches(message) => crate::pages::branches::update_branches(app, message),
        Message::History(message) => crate::pages::history::update_history(app, message),
        Message::Home(message) => crate::pages::home::update_home(app, message),
        Message::HomeJobFinished(result) => {
            handle_home_job_result(app, result);
            Task::none()
        }
        Message::Overview(message) => crate::pages::overview::update_overview(app, message),
        Message::OverviewJobFinished(result) => {
            handle_overview_job_result(app, result);
            Task::none()
        }
        Message::Preview(message) => crate::pages::preview::update_preview(app, message),
        Message::Search(message) => crate::pages::search::update_search(app, message),
        Message::Sharing(message) => crate::pages::sharing::update_sharing(app, message),
        Message::Sync(message) => crate::pages::sync::update_sync(app, message),
        Message::SyncJobFinished(result) => {
            handle_sync_job_result(app, result);
            Task::none()
        }
        Message::Workbench(crate::pages::workbench::WorkbenchMessage::SelectPage(page)) => {
            app.workbench.active_page = page;
            Task::none()
        }
        Message::NoOp => Task::none(),
    }
}

pub fn view(app: &PsycGuiApp) -> Element<'_, Message> {
    match app.screen {
        Screen::Home => crate::widgets::home_screen::view_home(app),
        Screen::Workbench => crate::widgets::workbench_shell::view_workbench(app),
    }
}
```

- [ ] **Step 4: Re-run the shell tests to verify GREEN**

Run:

```powershell
cargo test -p e2v-gui home_screen_model_includes_known_repository_cards_and_validation_errors
cargo test -p e2v-gui workbench_shell_model_exposes_navigation_for_phase_two_pages
```

Expected: PASS

- [ ] **Step 5: Commit the rendered shell foundation**

```powershell
git add crates/e2v-gui/Cargo.toml crates/e2v-gui/src/app.rs crates/e2v-gui/src/domain.rs crates/e2v-gui/src/lib.rs crates/e2v-gui/src/pages/home.rs crates/e2v-gui/src/pages/overview.rs crates/e2v-gui/src/pages/history.rs crates/e2v-gui/src/pages/branches.rs crates/e2v-gui/src/pages/sync.rs crates/e2v-gui/src/pages/workbench.rs crates/e2v-gui/src/widgets/home_screen.rs crates/e2v-gui/src/widgets/workbench_shell.rs crates/e2v-gui/src/widgets/status_badge.rs crates/e2v-gui/src/widgets/job_drawer.rs crates/e2v-gui/tests/render_shell.rs
git commit -m "feat: render the gui home and workbench shell"
```

## Task 2: Add The Search Service And Search Page

**Files:**
- Modify: `crates/e2v-gui/Cargo.toml`
- Modify: `crates/e2v-gui/src/app.rs`
- Modify: `crates/e2v-gui/src/domain.rs`
- Modify: `crates/e2v-gui/src/lib.rs`
- Modify: `crates/e2v-gui/src/testing.rs`
- Modify: `crates/e2v-gui/src/services/mod.rs`
- Modify: `crates/e2v-gui/src/pages/workbench.rs`
- Create: `crates/e2v-gui/src/services/search_service.rs`
- Create: `crates/e2v-gui/src/pages/search.rs`
- Create: `crates/e2v-gui/tests/search.rs`

**Interfaces:**
- Produces: `pub struct SearchQuery`
- Produces: `pub struct SearchResultRow`
- Produces: `pub struct SearchState`
- Produces: `pub enum SearchMessage`
- Produces: `pub trait SearchService`
- Produces: `fn search(&self, repo_root: PathBuf, branch_token: String, head_snapshot_id: Option<String>, query: SearchQuery) -> Result<Vec<SearchResultRow>, AppError>`

- [ ] **Step 1: Write the failing search tests**

```rust
use e2v_gui::pages::search::SearchMessage;
use e2v_gui::testing::{FakeRepositoryService, FakeSearchService, TestServices, advance};

#[test]
fn search_requires_non_empty_query_before_dispatch() {
    let repository = FakeRepositoryService::with_open_result("D:/repos/demo", "main", Some("snap-1"));
    let search = FakeSearchService::default();
    let mut harness = e2v_gui::testing::boot_into_workbench_with_services(
        TestServices::new(repository).with_search(search),
        "D:/repos/demo",
    );

    let task = advance(&mut harness.app, SearchMessage::SubmitSearch.into());

    assert_eq!(task.units(), 0);
    assert_eq!(
        harness.app.workbench.search.validation_error.as_deref(),
        Some("Search query is required")
    );
}

#[test]
fn successful_filename_search_populates_result_rows() {
    let repository = FakeRepositoryService::with_open_result("D:/repos/demo", "main", Some("snap-1"));
    let search = FakeSearchService::with_rows(vec![
        e2v_gui::pages::search::SearchResultRow {
            path: "images/cat.png".into(),
            source: "filename".into(),
            file_object_id: "obj-1".into(),
        },
        e2v_gui::pages::search::SearchResultRow {
            path: "notes/todo.txt".into(),
            source: "filename".into(),
            file_object_id: "obj-2".into(),
        },
    ]);
    let mut harness = e2v_gui::testing::boot_into_workbench_with_services(
        TestServices::new(repository).with_search(search),
        "D:/repos/demo",
    );
    harness.app.workbench.search.query_text = "cat".into();

    let _ = advance(&mut harness.app, SearchMessage::SubmitSearch.into());

    assert_eq!(harness.app.workbench.search.results.len(), 2);
    assert_eq!(harness.app.workbench.search.results[0].path, "images/cat.png");
}
```

- [ ] **Step 2: Run the focused search tests to verify RED**

Run:

```powershell
cargo test -p e2v-gui search_requires_non_empty_query_before_dispatch
cargo test -p e2v-gui successful_filename_search_populates_result_rows
```

Expected: FAIL because the search service and page do not exist yet.

- [ ] **Step 3: Implement the search service, state, and page**

```rust
// crates/e2v-gui/src/services/search_service.rs
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchQuery {
    pub query_text: String,
    pub path_prefix: Option<String>,
}

pub trait SearchService: Send + Sync + std::fmt::Debug + 'static {
    fn search(
        &self,
        repo_root: std::path::PathBuf,
        branch_token: String,
        head_snapshot_id: Option<String>,
        query: SearchQuery,
    ) -> Result<Vec<crate::pages::search::SearchResultRow>, crate::domain::AppError>;
}

#[derive(Debug, Default)]
pub struct RealSearchService {
    facade: e2v_core::RepositoryFacade,
}

impl SearchService for RealSearchService {
    fn search(
        &self,
        repo_root: std::path::PathBuf,
        branch_token: String,
        head_snapshot_id: Option<String>,
        query: SearchQuery,
    ) -> Result<Vec<crate::pages::search::SearchResultRow>, crate::domain::AppError> {
        let filename_hits = self
            .facade
            .search_filenames(&repo_root, &branch_token, head_snapshot_id.as_deref(), &query.query_text)
            .map_err(crate::domain::AppError::internal)?;
        if !filename_hits.is_empty() {
            return Ok(filename_hits
                .into_iter()
                .map(|row| crate::pages::search::SearchResultRow {
                    path: row.path,
                    source: "filename".into(),
                    file_object_id: row.file_object_id,
                })
                .collect());
        }

        let metadata_hits = self
            .facade
            .search_metadata(
                &repo_root,
                &branch_token,
                head_snapshot_id.as_deref(),
                &e2v_core::MetadataSearchQuery {
                    extension: Some(query.query_text),
                    path_prefix: query.path_prefix,
                    min_size: None,
                    max_size: None,
                },
            )
            .map_err(crate::domain::AppError::internal)?;

        Ok(metadata_hits
            .into_iter()
            .map(|row| crate::pages::search::SearchResultRow {
                path: row.path,
                source: "metadata".into(),
                file_object_id: row.file_object_id,
            })
            .collect())
    }
}
```

```rust
// crates/e2v-gui/src/pages/search.rs
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResultRow {
    pub path: String,
    pub source: String,
    pub file_object_id: String,
}

#[derive(Debug, Clone, Default)]
pub struct SearchState {
    pub query_text: String,
    pub path_prefix_text: String,
    pub validation_error: Option<String>,
    pub results: Vec<SearchResultRow>,
}

#[derive(Debug, Clone)]
pub enum SearchMessage {
    SetQueryText(String),
    SetPathPrefixText(String),
    SubmitSearch,
}

pub fn update_search(
    app: &mut crate::app::PsycGuiApp,
    message: SearchMessage,
) -> iced::Task<crate::domain::Message> {
    match message {
        SearchMessage::SetQueryText(value) => {
            app.workbench.search.query_text = value;
            iced::Task::none()
        }
        SearchMessage::SetPathPrefixText(value) => {
            app.workbench.search.path_prefix_text = value;
            iced::Task::none()
        }
        SearchMessage::SubmitSearch => {
            if app.workbench.search.query_text.trim().is_empty() {
                app.workbench.search.validation_error = Some("Search query is required".into());
                return iced::Task::none();
            }
            let repo_root = app.selected_repository.clone().expect("selected repository");
            let rows = app
                .services
                .search
                .search(
                    repo_root,
                    app.workbench.branch_token.clone(),
                    app.workbench.overview.head_snapshot_id.clone(),
                    crate::services::search_service::SearchQuery {
                        query_text: app.workbench.search.query_text.trim().to_owned(),
                        path_prefix: (!app.workbench.search.path_prefix_text.trim().is_empty())
                            .then(|| app.workbench.search.path_prefix_text.trim().to_owned()),
                    },
                )
                .unwrap_or_default();
            app.workbench.search.validation_error = None;
            app.workbench.search.results = rows;
            iced::Task::none()
        }
    }
}
```

```rust
// crates/e2v-gui/src/services/mod.rs
#[derive(Debug, Clone)]
pub struct AppServices {
    pub repository: std::sync::Arc<dyn repository_service::RepositoryService>,
    pub search: std::sync::Arc<dyn search_service::SearchService>,
}

impl AppServices {
    pub fn real() -> Self {
        Self {
            repository: std::sync::Arc::new(repository_service::RealRepositoryService::default()),
            search: std::sync::Arc::new(search_service::RealSearchService::default()),
        }
    }
}
```

- [ ] **Step 4: Re-run the search tests to verify GREEN**

Run:

```powershell
cargo test -p e2v-gui search_requires_non_empty_query_before_dispatch
cargo test -p e2v-gui successful_filename_search_populates_result_rows
```

Expected: PASS

- [ ] **Step 5: Commit the search page**

```powershell
git add crates/e2v-gui/Cargo.toml crates/e2v-gui/src/app.rs crates/e2v-gui/src/domain.rs crates/e2v-gui/src/lib.rs crates/e2v-gui/src/testing.rs crates/e2v-gui/src/services/mod.rs crates/e2v-gui/src/services/search_service.rs crates/e2v-gui/src/pages/workbench.rs crates/e2v-gui/src/pages/search.rs crates/e2v-gui/tests/search.rs
git commit -m "feat: add gui search page"
```

## Task 3: Add The Sharing Roster And Bundle-Based Invite Flows

**Files:**
- Modify: `crates/e2v-gui/Cargo.toml`
- Modify: `crates/e2v-gui/src/app.rs`
- Modify: `crates/e2v-gui/src/domain.rs`
- Modify: `crates/e2v-gui/src/lib.rs`
- Modify: `crates/e2v-gui/src/testing.rs`
- Modify: `crates/e2v-gui/src/services/mod.rs`
- Modify: `crates/e2v-gui/src/pages/workbench.rs`
- Create: `crates/e2v-gui/src/services/sharing_service.rs`
- Create: `crates/e2v-gui/src/pages/sharing.rs`
- Create: `crates/e2v-gui/tests/sharing.rs`

**Interfaces:**
- Produces: `pub struct SharingRoster`
- Produces: `pub struct ShareActorRow`
- Produces: `pub struct ShareDeviceRow`
- Produces: `pub struct SharingState`
- Produces: `pub enum SharingMessage`
- Produces: `pub trait SharingService`
- Produces: `fn load_roster(&self, repo_root: PathBuf) -> Result<SharingRoster, AppError>`
- Produces: `fn invite_member(&self, repo_root: PathBuf, display_name: String) -> Result<String, AppError>`
- Produces: `fn accept_member(&self, repo_root: PathBuf, invite_bundle_base64: String, local_device_label: String) -> Result<SharingRoster, AppError>`
- Produces: `fn invite_device(&self, repo_root: PathBuf, actor_id: String, device_label: String) -> Result<String, AppError>`
- Produces: `fn accept_device(&self, repo_root: PathBuf, invite_bundle_base64: String, local_device_label: String) -> Result<SharingRoster, AppError>`

- [ ] **Step 1: Write the failing sharing tests**

```rust
use e2v_gui::pages::sharing::SharingMessage;
use e2v_gui::testing::{FakeRepositoryService, FakeSharingService, TestServices, advance};

#[test]
fn member_invite_stores_a_copyable_base64_bundle() {
    let repository = FakeRepositoryService::with_open_result("D:/repos/demo", "main", Some("snap-1"));
    let sharing = FakeSharingService::with_member_invite_bundle(b"member-bundle".to_vec());
    let mut harness = e2v_gui::testing::boot_into_workbench_with_services(
        TestServices::new(repository).with_sharing(sharing),
        "D:/repos/demo",
    );
    harness.app.workbench.sharing.invite_member_display_name = "Alice".into();

    let _ = advance(&mut harness.app, SharingMessage::SubmitInviteMember.into());

    assert_eq!(
        harness.app.workbench.sharing.last_invite_bundle_base64.as_deref(),
        Some("bWVtYmVyLWJ1bmRsZQ==")
    );
}

#[test]
fn accept_member_requires_bundle_and_device_label() {
    let repository = FakeRepositoryService::with_open_result("D:/repos/demo", "main", Some("snap-1"));
    let sharing = FakeSharingService::default();
    let mut harness = e2v_gui::testing::boot_into_workbench_with_services(
        TestServices::new(repository).with_sharing(sharing),
        "D:/repos/demo",
    );

    let task = advance(&mut harness.app, SharingMessage::SubmitAcceptMember.into());

    assert_eq!(task.units(), 0);
    assert_eq!(
        harness.app.workbench.sharing.validation_error.as_deref(),
        Some("Invite bundle and local device label are required")
    );
}
```

- [ ] **Step 2: Run the focused sharing tests to verify RED**

Run:

```powershell
cargo test -p e2v-gui member_invite_stores_a_copyable_base64_bundle
cargo test -p e2v-gui accept_member_requires_bundle_and_device_label
```

Expected: FAIL because the sharing service and page do not exist yet.

- [ ] **Step 3: Implement the sharing roster and invite/accept flows**

```rust
// crates/e2v-gui/src/services/sharing_service.rs
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
    fn load_roster(&self, repo_root: std::path::PathBuf) -> Result<SharingRoster, crate::domain::AppError>;
    fn invite_member(&self, repo_root: std::path::PathBuf, display_name: String) -> Result<String, crate::domain::AppError>;
    fn accept_member(&self, repo_root: std::path::PathBuf, invite_bundle_base64: String, local_device_label: String) -> Result<SharingRoster, crate::domain::AppError>;
    fn invite_device(&self, repo_root: std::path::PathBuf, actor_id: String, device_label: String) -> Result<String, crate::domain::AppError>;
    fn accept_device(&self, repo_root: std::path::PathBuf, invite_bundle_base64: String, local_device_label: String) -> Result<SharingRoster, crate::domain::AppError>;
}

#[derive(Debug, Default)]
pub struct RealSharingService {
    sdk: e2v_api::Sdk,
}

impl RealSharingService {
    fn roster_from_info(info: e2v_api::ShareListInfo) -> SharingRoster {
        SharingRoster {
            actors: info.actors.into_iter().map(|actor| ShareActorRow {
                actor_id: actor.actor_id,
                display_name: actor.display_name,
                role: actor.role,
            }).collect(),
            devices: info.devices.into_iter().map(|device| ShareDeviceRow {
                device_id: device.device_id,
                actor_id: device.actor_id,
                label: device.label,
                status: device.status,
            }).collect(),
        }
    }
}
```

```rust
// crates/e2v-gui/src/pages/sharing.rs
#[derive(Debug, Clone, Default)]
pub struct SharingState {
    pub roster: crate::services::sharing_service::SharingRoster,
    pub invite_member_display_name: String,
    pub accept_bundle_base64: String,
    pub accept_local_device_label: String,
    pub invite_device_actor_id: String,
    pub invite_device_label: String,
    pub validation_error: Option<String>,
    pub last_invite_bundle_base64: Option<String>,
}

#[derive(Debug, Clone)]
pub enum SharingMessage {
    SetInviteMemberDisplayName(String),
    SetAcceptBundleBase64(String),
    SetAcceptLocalDeviceLabel(String),
    SetInviteDeviceActorId(String),
    SetInviteDeviceLabel(String),
    SubmitInviteMember,
    SubmitAcceptMember,
    SubmitInviteDevice,
    SubmitAcceptDevice,
}

pub fn update_sharing(
    app: &mut crate::app::PsycGuiApp,
    message: SharingMessage,
) -> iced::Task<crate::domain::Message> {
    let repo_root = app.selected_repository.clone().expect("selected repository");
    match message {
        SharingMessage::SetInviteMemberDisplayName(value) => {
            app.workbench.sharing.invite_member_display_name = value;
            iced::Task::none()
        }
        SharingMessage::SetAcceptBundleBase64(value) => {
            app.workbench.sharing.accept_bundle_base64 = value;
            iced::Task::none()
        }
        SharingMessage::SetAcceptLocalDeviceLabel(value) => {
            app.workbench.sharing.accept_local_device_label = value;
            iced::Task::none()
        }
        SharingMessage::SetInviteDeviceActorId(value) => {
            app.workbench.sharing.invite_device_actor_id = value;
            iced::Task::none()
        }
        SharingMessage::SetInviteDeviceLabel(value) => {
            app.workbench.sharing.invite_device_label = value;
            iced::Task::none()
        }
        SharingMessage::SubmitInviteMember => {
            if app.workbench.sharing.invite_member_display_name.trim().is_empty() {
                app.workbench.sharing.validation_error =
                    Some("Member display name is required".into());
                return iced::Task::none();
            }
            let base64_bundle = app
                .services
                .sharing
                .invite_member(
                    repo_root,
                    app.workbench.sharing.invite_member_display_name.trim().to_owned(),
                )
                .unwrap();
            app.workbench.sharing.last_invite_bundle_base64 = Some(base64_bundle);
            app.workbench.sharing.validation_error = None;
            iced::Task::none()
        }
        SharingMessage::SubmitAcceptMember => {
            if app.workbench.sharing.accept_bundle_base64.trim().is_empty()
                || app.workbench.sharing.accept_local_device_label.trim().is_empty()
            {
                app.workbench.sharing.validation_error =
                    Some("Invite bundle and local device label are required".into());
                return iced::Task::none();
            }
            app.workbench.sharing.roster = app
                .services
                .sharing
                .accept_member(
                    repo_root,
                    app.workbench.sharing.accept_bundle_base64.trim().to_owned(),
                    app.workbench
                        .sharing
                        .accept_local_device_label
                        .trim()
                        .to_owned(),
                )
                .unwrap();
            app.workbench.sharing.validation_error = None;
            iced::Task::none()
        }
        SharingMessage::SubmitInviteDevice => {
            if app.workbench.sharing.invite_device_actor_id.trim().is_empty()
                || app.workbench.sharing.invite_device_label.trim().is_empty()
            {
                app.workbench.sharing.validation_error =
                    Some("Actor id and device label are required".into());
                return iced::Task::none();
            }
            let base64_bundle = app
                .services
                .sharing
                .invite_device(
                    repo_root,
                    app.workbench.sharing.invite_device_actor_id.trim().to_owned(),
                    app.workbench.sharing.invite_device_label.trim().to_owned(),
                )
                .unwrap();
            app.workbench.sharing.last_invite_bundle_base64 = Some(base64_bundle);
            app.workbench.sharing.validation_error = None;
            iced::Task::none()
        }
        SharingMessage::SubmitAcceptDevice => {
            if app.workbench.sharing.accept_bundle_base64.trim().is_empty()
                || app.workbench.sharing.accept_local_device_label.trim().is_empty()
            {
                app.workbench.sharing.validation_error =
                    Some("Invite bundle and local device label are required".into());
                return iced::Task::none();
            }
            app.workbench.sharing.roster = app
                .services
                .sharing
                .accept_device(
                    repo_root,
                    app.workbench.sharing.accept_bundle_base64.trim().to_owned(),
                    app.workbench
                        .sharing
                        .accept_local_device_label
                        .trim()
                        .to_owned(),
                )
                .unwrap();
            app.workbench.sharing.validation_error = None;
            iced::Task::none()
        }
    }
}
```

- [ ] **Step 4: Re-run the sharing tests to verify GREEN**

Run:

```powershell
cargo test -p e2v-gui member_invite_stores_a_copyable_base64_bundle
cargo test -p e2v-gui accept_member_requires_bundle_and_device_label
```

Expected: PASS

- [ ] **Step 5: Commit the sharing roster and bundle flows**

```powershell
git add crates/e2v-gui/Cargo.toml crates/e2v-gui/src/app.rs crates/e2v-gui/src/domain.rs crates/e2v-gui/src/lib.rs crates/e2v-gui/src/testing.rs crates/e2v-gui/src/services/mod.rs crates/e2v-gui/src/services/sharing_service.rs crates/e2v-gui/src/pages/workbench.rs crates/e2v-gui/src/pages/sharing.rs crates/e2v-gui/tests/sharing.rs
git commit -m "feat: add gui sharing roster and invite flows"
```

## Task 4: Add Sharing Revocation Confirmations

**Files:**
- Modify: `crates/e2v-gui/src/app.rs`
- Modify: `crates/e2v-gui/src/domain.rs`
- Modify: `crates/e2v-gui/src/testing.rs`
- Modify: `crates/e2v-gui/src/services/sharing_service.rs`
- Modify: `crates/e2v-gui/src/pages/sharing.rs`
- Modify: `crates/e2v-gui/src/widgets/confirmation_sheet.rs`
- Modify: `crates/e2v-gui/tests/sharing.rs`

**Interfaces:**
- Produces: `fn revoke_member(&self, repo_root: PathBuf, actor_id: String, password: String) -> Result<SharingRoster, AppError>`
- Produces: `fn revoke_device(&self, repo_root: PathBuf, device_id: String, password: String) -> Result<SharingRoster, AppError>`
- Produces: `pub enum PendingConfirmation { SingleWriterRiskPush { .. }, RevokeMember { .. }, RevokeDevice { .. } }`

- [ ] **Step 1: Write the failing revoke-confirmation tests**

```rust
use e2v_gui::pages::sharing::SharingMessage;
use e2v_gui::testing::{FakeRepositoryService, FakeSharingService, TestServices, advance};

#[test]
fn revoke_member_requires_confirmation_before_dispatch() {
    let repository = FakeRepositoryService::with_open_result("D:/repos/demo", "main", Some("snap-1"));
    let sharing = FakeSharingService::with_roster(vec![("actor-1", "Alice", "owner")], vec![]);
    let mut harness = e2v_gui::testing::boot_into_workbench_with_services(
        TestServices::new(repository).with_sharing(sharing),
        "D:/repos/demo",
    );

    let task = advance(
        &mut harness.app,
        SharingMessage::RequestRevokeMember {
            actor_id: "actor-1".into(),
            password: "secret".into(),
        }
        .into(),
    );

    assert_eq!(task.units(), 0);
    assert!(matches!(
        harness.app.pending_confirmation,
        Some(e2v_gui::PendingConfirmation::RevokeMember { .. })
    ));
}

#[test]
fn confirmed_device_revoke_refreshes_the_roster() {
    let repository = FakeRepositoryService::with_open_result("D:/repos/demo", "main", Some("snap-1"));
    let sharing = FakeSharingService::with_revocable_device("device-1");
    let mut harness = e2v_gui::testing::boot_into_workbench_with_services(
        TestServices::new(repository).with_sharing(sharing),
        "D:/repos/demo",
    );
    harness.app.pending_confirmation = Some(e2v_gui::PendingConfirmation::RevokeDevice {
        repo_root: "D:/repos/demo".into(),
        device_id: "device-1".into(),
        password: "secret".into(),
    });

    let _ = advance(&mut harness.app, SharingMessage::ConfirmPendingAction.into());

    assert!(harness
        .app
        .workbench
        .sharing
        .roster
        .devices
        .iter()
        .all(|device| device.device_id != "device-1"));
}
```

- [ ] **Step 2: Run the focused revoke-confirmation tests to verify RED**

Run:

```powershell
cargo test -p e2v-gui revoke_member_requires_confirmation_before_dispatch
cargo test -p e2v-gui confirmed_device_revoke_refreshes_the_roster
```

Expected: FAIL because revoke confirmations and confirm handlers do not exist yet.

- [ ] **Step 3: Implement revoke confirmations and roster refresh**

```rust
// crates/e2v-gui/src/domain.rs
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingConfirmation {
    SingleWriterRiskPush {
        repo_root: std::path::PathBuf,
        branch_token: String,
    },
    RevokeMember {
        repo_root: std::path::PathBuf,
        actor_id: String,
        password: String,
    },
    RevokeDevice {
        repo_root: std::path::PathBuf,
        device_id: String,
        password: String,
    },
}
```

```rust
// crates/e2v-gui/src/pages/sharing.rs
#[derive(Debug, Clone)]
pub enum SharingMessage {
    SetInviteMemberDisplayName(String),
    SetAcceptBundleBase64(String),
    SetAcceptLocalDeviceLabel(String),
    SetInviteDeviceActorId(String),
    SetInviteDeviceLabel(String),
    SubmitInviteMember,
    SubmitAcceptMember,
    SubmitInviteDevice,
    SubmitAcceptDevice,
    RequestRevokeMember { actor_id: String, password: String },
    RequestRevokeDevice { device_id: String, password: String },
    ConfirmPendingAction,
    CancelPendingAction,
}

pub fn update_sharing(
    app: &mut crate::app::PsycGuiApp,
    message: SharingMessage,
) -> iced::Task<crate::domain::Message> {
    let repo_root = app.selected_repository.clone().expect("selected repository");
    match message {
        SharingMessage::SetInviteMemberDisplayName(value) => {
            app.workbench.sharing.invite_member_display_name = value;
            iced::Task::none()
        }
        SharingMessage::SetAcceptBundleBase64(value) => {
            app.workbench.sharing.accept_bundle_base64 = value;
            iced::Task::none()
        }
        SharingMessage::SetAcceptLocalDeviceLabel(value) => {
            app.workbench.sharing.accept_local_device_label = value;
            iced::Task::none()
        }
        SharingMessage::SetInviteDeviceActorId(value) => {
            app.workbench.sharing.invite_device_actor_id = value;
            iced::Task::none()
        }
        SharingMessage::SetInviteDeviceLabel(value) => {
            app.workbench.sharing.invite_device_label = value;
            iced::Task::none()
        }
        SharingMessage::SubmitInviteMember => {
            if app.workbench.sharing.invite_member_display_name.trim().is_empty() {
                app.workbench.sharing.validation_error =
                    Some("Member display name is required".into());
                return iced::Task::none();
            }
            let base64_bundle = app
                .services
                .sharing
                .invite_member(
                    repo_root,
                    app.workbench.sharing.invite_member_display_name.trim().to_owned(),
                )
                .unwrap();
            app.workbench.sharing.last_invite_bundle_base64 = Some(base64_bundle);
            app.workbench.sharing.validation_error = None;
            iced::Task::none()
        }
        SharingMessage::SubmitAcceptMember => {
            if app.workbench.sharing.accept_bundle_base64.trim().is_empty()
                || app.workbench.sharing.accept_local_device_label.trim().is_empty()
            {
                app.workbench.sharing.validation_error =
                    Some("Invite bundle and local device label are required".into());
                return iced::Task::none();
            }
            app.workbench.sharing.roster = app
                .services
                .sharing
                .accept_member(
                    repo_root,
                    app.workbench.sharing.accept_bundle_base64.trim().to_owned(),
                    app.workbench
                        .sharing
                        .accept_local_device_label
                        .trim()
                        .to_owned(),
                )
                .unwrap();
            app.workbench.sharing.validation_error = None;
            iced::Task::none()
        }
        SharingMessage::SubmitInviteDevice => {
            if app.workbench.sharing.invite_device_actor_id.trim().is_empty()
                || app.workbench.sharing.invite_device_label.trim().is_empty()
            {
                app.workbench.sharing.validation_error =
                    Some("Actor id and device label are required".into());
                return iced::Task::none();
            }
            let base64_bundle = app
                .services
                .sharing
                .invite_device(
                    repo_root,
                    app.workbench.sharing.invite_device_actor_id.trim().to_owned(),
                    app.workbench.sharing.invite_device_label.trim().to_owned(),
                )
                .unwrap();
            app.workbench.sharing.last_invite_bundle_base64 = Some(base64_bundle);
            app.workbench.sharing.validation_error = None;
            iced::Task::none()
        }
        SharingMessage::SubmitAcceptDevice => {
            if app.workbench.sharing.accept_bundle_base64.trim().is_empty()
                || app.workbench.sharing.accept_local_device_label.trim().is_empty()
            {
                app.workbench.sharing.validation_error =
                    Some("Invite bundle and local device label are required".into());
                return iced::Task::none();
            }
            app.workbench.sharing.roster = app
                .services
                .sharing
                .accept_device(
                    repo_root,
                    app.workbench.sharing.accept_bundle_base64.trim().to_owned(),
                    app.workbench
                        .sharing
                        .accept_local_device_label
                        .trim()
                        .to_owned(),
                )
                .unwrap();
            app.workbench.sharing.validation_error = None;
            iced::Task::none()
        }
        SharingMessage::RequestRevokeMember { actor_id, password } => {
            app.pending_confirmation = Some(crate::domain::PendingConfirmation::RevokeMember {
                repo_root: repo_root.clone(),
                actor_id,
                password,
            });
            iced::Task::none()
        }
        SharingMessage::RequestRevokeDevice { device_id, password } => {
            app.pending_confirmation = Some(crate::domain::PendingConfirmation::RevokeDevice {
                repo_root,
                device_id,
                password,
            });
            iced::Task::none()
        }
        SharingMessage::ConfirmPendingAction => {
            match app.pending_confirmation.take() {
                Some(crate::domain::PendingConfirmation::RevokeMember { repo_root, actor_id, password }) => {
                    app.workbench.sharing.roster =
                        app.services.sharing.revoke_member(repo_root, actor_id, password).unwrap();
                }
                Some(crate::domain::PendingConfirmation::RevokeDevice { repo_root, device_id, password }) => {
                    app.workbench.sharing.roster =
                        app.services.sharing.revoke_device(repo_root, device_id, password).unwrap();
                }
                other => app.pending_confirmation = other,
            }
            iced::Task::none()
        }
        SharingMessage::CancelPendingAction => {
            app.pending_confirmation = None;
            iced::Task::none()
        }
    }
}
```

- [ ] **Step 4: Re-run the revoke-confirmation tests to verify GREEN**

Run:

```powershell
cargo test -p e2v-gui revoke_member_requires_confirmation_before_dispatch
cargo test -p e2v-gui confirmed_device_revoke_refreshes_the_roster
```

Expected: PASS

- [ ] **Step 5: Commit the sharing confirmation flows**

```powershell
git add crates/e2v-gui/src/app.rs crates/e2v-gui/src/domain.rs crates/e2v-gui/src/testing.rs crates/e2v-gui/src/services/sharing_service.rs crates/e2v-gui/src/pages/sharing.rs crates/e2v-gui/src/widgets/confirmation_sheet.rs crates/e2v-gui/tests/sharing.rs
git commit -m "feat: add gui sharing revocation confirmations"
```

## Task 5: Add Preview Controllers And The Local Web Panel

**Files:**
- Modify: `crates/e2v-gui/Cargo.toml`
- Modify: `crates/e2v-gui/src/app.rs`
- Modify: `crates/e2v-gui/src/domain.rs`
- Modify: `crates/e2v-gui/src/lib.rs`
- Modify: `crates/e2v-gui/src/testing.rs`
- Modify: `crates/e2v-gui/src/services/mod.rs`
- Modify: `crates/e2v-gui/src/pages/workbench.rs`
- Create: `crates/e2v-gui/src/services/preview_service.rs`
- Create: `crates/e2v-gui/src/pages/preview.rs`
- Create: `crates/e2v-gui/tests/preview.rs`

**Interfaces:**
- Produces: `pub struct LocalWebController`
- Produces: `pub struct MountController`
- Produces: `pub struct PreviewState`
- Produces: `pub enum PreviewMessage`
- Produces: `pub enum PreviewJobResult`
- Produces: `pub trait PreviewService`
- Produces: `fn start_local_web(&self, repo_root: PathBuf) -> Result<LocalWebController, AppError>`
- Produces: `fn stop_local_web(&self, controller: LocalWebController) -> Result<(), AppError>`

- [ ] **Step 1: Write the failing preview-controller tests**

```rust
use e2v_gui::pages::preview::PreviewMessage;
use e2v_gui::testing::{FakePreviewService, FakeRepositoryService, TestServices, advance};

#[test]
fn starting_local_web_registers_a_controller_and_local_url() {
    let repository = FakeRepositoryService::with_open_result("D:/repos/demo", "main", Some("snap-1"));
    let preview = FakePreviewService::with_local_web_url("http://127.0.0.1:44551".into());
    let mut harness = e2v_gui::testing::boot_into_workbench_with_services(
        TestServices::new(repository).with_preview(preview),
        "D:/repos/demo",
    );

    let _ = advance(&mut harness.app, PreviewMessage::StartLocalWeb.into());

    assert_eq!(
        harness.app.workbench.preview.local_web_url.as_deref(),
        Some("http://127.0.0.1:44551")
    );
    assert!(harness.app.workbench.preview.local_web_running);
}

#[test]
fn stopping_local_web_clears_the_preview_controller_state() {
    let repository = FakeRepositoryService::with_open_result("D:/repos/demo", "main", Some("snap-1"));
    let preview = FakePreviewService::with_local_web_url("http://127.0.0.1:44551".into());
    let mut harness = e2v_gui::testing::boot_into_workbench_with_services(
        TestServices::new(repository).with_preview(preview),
        "D:/repos/demo",
    );
    harness.app.workbench.preview.local_web_url = Some("http://127.0.0.1:44551".into());
    harness.app.workbench.preview.local_web_running = true;

    let _ = advance(&mut harness.app, PreviewMessage::StopLocalWeb.into());

    assert_eq!(harness.app.workbench.preview.local_web_url, None);
    assert!(!harness.app.workbench.preview.local_web_running);
}
```

- [ ] **Step 2: Run the focused preview-controller tests to verify RED**

Run:

```powershell
cargo test -p e2v-gui starting_local_web_registers_a_controller_and_local_url
cargo test -p e2v-gui stopping_local_web_clears_the_preview_controller_state
```

Expected: FAIL because preview state and controller services do not exist yet.

- [ ] **Step 3: Implement preview controllers and the local web panel**

```rust
// crates/e2v-gui/src/services/preview_service.rs
#[derive(Debug)]
pub struct LocalWebController {
    pub local_url: String,
    handle: e2v_sync::ServeHandle,
}

#[derive(Debug)]
pub struct MountController {
    pub summary: e2v_vfs::MountLaunchSummary,
    filesystem: e2v_vfs::MountedFilesystem,
}

pub trait PreviewService: Send + Sync + std::fmt::Debug + 'static {
    fn start_local_web(&self, repo_root: std::path::PathBuf) -> Result<LocalWebController, crate::domain::AppError>;
    fn stop_local_web(&self, controller: LocalWebController) -> Result<(), crate::domain::AppError>;
}

#[derive(Debug, Default)]
pub struct RealPreviewService;

impl PreviewService for RealPreviewService {
    fn start_local_web(&self, repo_root: std::path::PathBuf) -> Result<LocalWebController, crate::domain::AppError> {
        let runtime = tokio::runtime::Handle::current();
        let handle = runtime
            .block_on(e2v_sync::serve_local_web(e2v_sync::ServeOptions { repo_root }))
            .map_err(|error| crate::domain::AppError::internal(error.to_string()))?;
        Ok(LocalWebController {
            local_url: format!("http://{}", handle.local_addr()),
            handle,
        })
    }

    fn stop_local_web(&self, controller: LocalWebController) -> Result<(), crate::domain::AppError> {
        drop(controller);
        Ok(())
    }
}
```

```rust
// crates/e2v-gui/src/pages/preview.rs
#[derive(Debug)]
pub struct PreviewState {
    pub local_web_url: Option<String>,
    pub local_web_running: bool,
    pub mount_summary: Option<e2v_vfs::MountLaunchSummary>,
    pub selected_snapshot_id: Option<String>,
    pub mount_point_text: String,
    pub writable_live_branch: bool,
    pub validation_error: Option<String>,
    pub local_web_controller: Option<crate::services::preview_service::LocalWebController>,
    pub mount_controller: Option<crate::services::preview_service::MountController>,
    pub focused_path: Option<String>,
}

impl Default for PreviewState {
    fn default() -> Self {
        Self {
            local_web_url: None,
            local_web_running: false,
            mount_summary: None,
            selected_snapshot_id: None,
            mount_point_text: String::new(),
            writable_live_branch: false,
            validation_error: None,
            local_web_controller: None,
            mount_controller: None,
            focused_path: None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum PreviewMessage {
    StartLocalWeb,
    StopLocalWeb,
    StartSnapshotMount,
    StartLiveBranchMount,
    StopMount,
    SetMountPoint(String),
    SetSelectedSnapshot(String),
    SetWritableLiveBranch(bool),
}
```

```rust
// crates/e2v-gui/src/pages/preview.rs
pub fn update_preview(
    app: &mut crate::app::PsycGuiApp,
    message: PreviewMessage,
) -> iced::Task<crate::domain::Message> {
    let repo_root = app.selected_repository.clone().expect("selected repository");
    match message {
        PreviewMessage::StartLocalWeb => {
            let controller = app.services.preview.start_local_web(repo_root).unwrap();
            app.workbench.preview.local_web_url = Some(controller.local_url.clone());
            app.workbench.preview.local_web_running = true;
            app.workbench.preview.local_web_controller = Some(controller);
            iced::Task::none()
        }
        PreviewMessage::StopLocalWeb => {
            if let Some(controller) = app.workbench.preview.local_web_controller.take() {
                let _ = app.services.preview.stop_local_web(controller);
            }
            app.workbench.preview.local_web_running = false;
            app.workbench.preview.local_web_url = None;
            iced::Task::none()
        }
        _ => iced::Task::none(),
    }
}
```

- [ ] **Step 4: Re-run the preview-controller tests to verify GREEN**

Run:

```powershell
cargo test -p e2v-gui starting_local_web_registers_a_controller_and_local_url
cargo test -p e2v-gui stopping_local_web_clears_the_preview_controller_state
```

Expected: PASS

- [ ] **Step 5: Commit the preview controller foundation**

```powershell
git add crates/e2v-gui/Cargo.toml crates/e2v-gui/src/app.rs crates/e2v-gui/src/domain.rs crates/e2v-gui/src/lib.rs crates/e2v-gui/src/testing.rs crates/e2v-gui/src/services/mod.rs crates/e2v-gui/src/services/preview_service.rs crates/e2v-gui/src/pages/workbench.rs crates/e2v-gui/src/pages/preview.rs crates/e2v-gui/tests/preview.rs
git commit -m "feat: add gui preview service controllers"
```

## Task 6: Add Preview Mount Workflows And Host Launch Actions

**Files:**
- Modify: `crates/e2v-gui/src/app.rs`
- Modify: `crates/e2v-gui/src/domain.rs`
- Modify: `crates/e2v-gui/src/lib.rs`
- Modify: `crates/e2v-gui/src/testing.rs`
- Modify: `crates/e2v-gui/src/services/mod.rs`
- Modify: `crates/e2v-gui/src/services/preview_service.rs`
- Modify: `crates/e2v-gui/src/pages/search.rs`
- Modify: `crates/e2v-gui/src/pages/preview.rs`
- Create: `crates/e2v-gui/src/services/host_shell_service.rs`
- Modify: `crates/e2v-gui/tests/search.rs`
- Modify: `crates/e2v-gui/tests/preview.rs`

**Interfaces:**
- Produces: `fn start_snapshot_mount(&self, repo_root: PathBuf, snapshot_id: String, mount_point: PathBuf) -> Result<MountController, AppError>`
- Produces: `fn start_live_branch_mount(&self, repo_root: PathBuf, branch_token: String, mount_point: PathBuf) -> Result<MountController, AppError>`
- Produces: `fn stop_mount(&self, controller: MountController) -> Result<(), AppError>`
- Produces: `pub trait HostShellService`
- Produces: `fn open_url(&self, url: &str) -> Result<(), AppError>`
- Produces: `fn open_path(&self, path: &Path) -> Result<(), AppError>`
- Produces: `SearchMessage::OpenResultInPreview(String)`

- [ ] **Step 1: Write the failing mount/host-launch tests**

```rust
use e2v_gui::pages::preview::PreviewMessage;
use e2v_gui::pages::search::SearchMessage;
use e2v_gui::testing::{
    FakeHostShellService, FakePreviewService, FakeRepositoryService, FakeSearchService,
    TestServices, advance,
};

#[test]
fn snapshot_mount_is_rendered_read_only_and_backed_by_launch_summary() {
    let repository = FakeRepositoryService::with_open_result("D:/repos/demo", "main", Some("snap-1"));
    let preview = FakePreviewService::with_snapshot_mount_summary("Q:/preview", true, true);
    let mut harness = e2v_gui::testing::boot_into_workbench_with_services(
        TestServices::new(repository).with_preview(preview),
        "D:/repos/demo",
    );
    harness.app.workbench.preview.selected_snapshot_id = Some("snap-1".into());
    harness.app.workbench.preview.mount_point_text = "Q:/preview".into();

    let _ = advance(&mut harness.app, PreviewMessage::StartSnapshotMount.into());

    let summary = harness.app.workbench.preview.mount_summary.as_ref().unwrap();
    assert!(summary.read_only);
    assert!(summary.stream_only);
    assert_eq!(summary.mount_mode, "snapshot-pinned");
}

#[test]
fn search_result_can_switch_to_preview_with_a_focused_path() {
    let repository = FakeRepositoryService::with_open_result("D:/repos/demo", "main", Some("snap-1"));
    let search = FakeSearchService::with_rows(vec![e2v_gui::pages::search::SearchResultRow {
        path: "images/cat.png".into(),
        source: "filename".into(),
        file_object_id: "obj-1".into(),
    }]);
    let mut harness = e2v_gui::testing::boot_into_workbench_with_services(
        TestServices::new(repository).with_search(search),
        "D:/repos/demo",
    );

    let _ = advance(
        &mut harness.app,
        SearchMessage::OpenResultInPreview("images/cat.png".into()).into(),
    );

    assert!(matches!(
        harness.app.workbench.active_page,
        e2v_gui::WorkbenchPage::Preview
    ));
    assert_eq!(
        harness.app.workbench.preview.focused_path.as_deref(),
        Some("images/cat.png")
    );
}

#[test]
fn open_mount_in_explorer_uses_the_host_shell_service() {
    let repository = FakeRepositoryService::with_open_result("D:/repos/demo", "main", Some("snap-1"));
    let preview = FakePreviewService::with_snapshot_mount_summary("Q:/preview", true, true);
    let host_shell = FakeHostShellService::default();
    let mut harness = e2v_gui::testing::boot_into_workbench_with_services(
        TestServices::new(repository)
            .with_preview(preview)
            .with_host_shell(host_shell.clone()),
        "D:/repos/demo",
    );
    harness.app.workbench.preview.mount_summary = Some(e2v_gui::testing::fake_snapshot_mount_summary("Q:/preview"));

    let _ = advance(&mut harness.app, PreviewMessage::OpenMountedPath.into());

    assert_eq!(host_shell.last_opened_path(), Some(std::path::PathBuf::from("Q:/preview")));
}
```

- [ ] **Step 2: Run the focused mount/host-launch tests to verify RED**

Run:

```powershell
cargo test -p e2v-gui snapshot_mount_is_rendered_read_only_and_backed_by_launch_summary
cargo test -p e2v-gui search_result_can_switch_to_preview_with_a_focused_path
cargo test -p e2v-gui open_mount_in_explorer_uses_the_host_shell_service
```

Expected: FAIL because mount start/stop, search-preview routing, and host shell launching are not implemented yet.

- [ ] **Step 3: Implement mount workflows, search-preview routing, and host launches**

```rust
// crates/e2v-gui/src/services/preview_service.rs
pub trait PreviewService: Send + Sync + std::fmt::Debug + 'static {
    fn start_local_web(&self, repo_root: std::path::PathBuf) -> Result<LocalWebController, crate::domain::AppError>;
    fn stop_local_web(&self, controller: LocalWebController) -> Result<(), crate::domain::AppError>;
    fn start_snapshot_mount(&self, repo_root: std::path::PathBuf, snapshot_id: String, mount_point: std::path::PathBuf) -> Result<MountController, crate::domain::AppError>;
    fn start_live_branch_mount(&self, repo_root: std::path::PathBuf, branch_token: String, mount_point: std::path::PathBuf) -> Result<MountController, crate::domain::AppError>;
    fn stop_mount(&self, controller: MountController) -> Result<(), crate::domain::AppError>;
}

impl PreviewService for RealPreviewService {
    fn start_snapshot_mount(&self, repo_root: std::path::PathBuf, snapshot_id: String, mount_point: std::path::PathBuf) -> Result<MountController, crate::domain::AppError> {
        let filesystem = e2v_vfs::start_snapshot_mount(repo_root, snapshot_id, mount_point)
            .map_err(|error| crate::domain::AppError::internal(error.to_string()))?;
        Ok(MountController {
            summary: filesystem.summary().clone(),
            filesystem,
        })
    }

    fn start_live_branch_mount(&self, repo_root: std::path::PathBuf, branch_token: String, mount_point: std::path::PathBuf) -> Result<MountController, crate::domain::AppError> {
        let filesystem = e2v_vfs::start_live_branch_mount(repo_root, branch_token, mount_point)
            .map_err(|error| crate::domain::AppError::internal(error.to_string()))?;
        Ok(MountController {
            summary: filesystem.summary().clone(),
            filesystem,
        })
    }

    fn stop_mount(&self, controller: MountController) -> Result<(), crate::domain::AppError> {
        drop(controller);
        Ok(())
    }
}
```

```rust
// crates/e2v-gui/src/services/host_shell_service.rs
pub trait HostShellService: Send + Sync + std::fmt::Debug + 'static {
    fn open_url(&self, url: &str) -> Result<(), crate::domain::AppError>;
    fn open_path(&self, path: &std::path::Path) -> Result<(), crate::domain::AppError>;
}

#[derive(Debug, Default)]
pub struct RealHostShellService;

impl HostShellService for RealHostShellService {
    fn open_url(&self, url: &str) -> Result<(), crate::domain::AppError> {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn()
            .map(|_| ())
            .map_err(|error| crate::domain::AppError::internal(error.to_string()))
    }

    fn open_path(&self, path: &std::path::Path) -> Result<(), crate::domain::AppError> {
        std::process::Command::new("explorer")
            .arg(path)
            .spawn()
            .map(|_| ())
            .map_err(|error| crate::domain::AppError::internal(error.to_string()))
    }
}
```

```rust
// crates/e2v-gui/src/pages/search.rs
#[derive(Debug, Clone)]
pub enum SearchMessage {
    SetQueryText(String),
    SetPathPrefixText(String),
    SubmitSearch,
    OpenResultInPreview(String),
}

pub fn update_search(
    app: &mut crate::app::PsycGuiApp,
    message: SearchMessage,
) -> iced::Task<crate::domain::Message> {
    match message {
        SearchMessage::SetQueryText(value) => {
            app.workbench.search.query_text = value;
            iced::Task::none()
        }
        SearchMessage::SetPathPrefixText(value) => {
            app.workbench.search.path_prefix_text = value;
            iced::Task::none()
        }
        SearchMessage::SubmitSearch => {
            if app.workbench.search.query_text.trim().is_empty() {
                app.workbench.search.validation_error = Some("Search query is required".into());
                return iced::Task::none();
            }
            let repo_root = app.selected_repository.clone().expect("selected repository");
            let rows = app
                .services
                .search
                .search(
                    repo_root,
                    app.workbench.branch_token.clone(),
                    app.workbench.overview.head_snapshot_id.clone(),
                    crate::services::search_service::SearchQuery {
                        query_text: app.workbench.search.query_text.trim().to_owned(),
                        path_prefix: (!app.workbench.search.path_prefix_text.trim().is_empty())
                            .then(|| app.workbench.search.path_prefix_text.trim().to_owned()),
                    },
                )
                .unwrap_or_default();
            app.workbench.search.validation_error = None;
            app.workbench.search.results = rows;
            iced::Task::none()
        }
        SearchMessage::OpenResultInPreview(path) => {
            app.workbench.preview.focused_path = Some(path);
            app.workbench.active_page = crate::domain::WorkbenchPage::Preview;
            iced::Task::none()
        }
    }
}
```

```rust
// crates/e2v-gui/src/pages/preview.rs
#[derive(Debug, Clone)]
pub enum PreviewMessage {
    StartLocalWeb,
    StopLocalWeb,
    StartSnapshotMount,
    StartLiveBranchMount,
    StopMount,
    OpenLocalWebInBrowser,
    OpenMountedPath,
    SetMountPoint(String),
    SetSelectedSnapshot(String),
    SetWritableLiveBranch(bool),
}

pub fn update_preview(
    app: &mut crate::app::PsycGuiApp,
    message: PreviewMessage,
) -> iced::Task<crate::domain::Message> {
    let repo_root = app.selected_repository.clone().expect("selected repository");
    match message {
        PreviewMessage::StartLocalWeb => {
            let controller = app.services.preview.start_local_web(repo_root).unwrap();
            app.workbench.preview.local_web_url = Some(controller.local_url.clone());
            app.workbench.preview.local_web_running = true;
            app.workbench.preview.local_web_controller = Some(controller);
            iced::Task::none()
        }
        PreviewMessage::StopLocalWeb => {
            if let Some(controller) = app.workbench.preview.local_web_controller.take() {
                let _ = app.services.preview.stop_local_web(controller);
            }
            app.workbench.preview.local_web_running = false;
            app.workbench.preview.local_web_url = None;
            iced::Task::none()
        }
        PreviewMessage::StartSnapshotMount => {
            let snapshot_id = app.workbench.preview.selected_snapshot_id.clone().expect("selected snapshot");
            let mount_point = std::path::PathBuf::from(app.workbench.preview.mount_point_text.trim());
            let controller = app
                .services
                .preview
                .start_snapshot_mount(repo_root, snapshot_id, mount_point)
                .unwrap();
            app.workbench.preview.mount_summary = Some(controller.summary.clone());
            app.workbench.preview.mount_controller = Some(controller);
            iced::Task::none()
        }
        PreviewMessage::StartLiveBranchMount => {
            let mount_point = std::path::PathBuf::from(app.workbench.preview.mount_point_text.trim());
            let controller = app
                .services
                .preview
                .start_live_branch_mount(repo_root, app.workbench.branch_token.clone(), mount_point)
                .unwrap();
            app.workbench.preview.mount_summary = Some(controller.summary.clone());
            app.workbench.preview.mount_controller = Some(controller);
            iced::Task::none()
        }
        PreviewMessage::StopMount => {
            if let Some(controller) = app.workbench.preview.mount_controller.take() {
                let _ = app.services.preview.stop_mount(controller);
            }
            app.workbench.preview.mount_summary = None;
            iced::Task::none()
        }
        PreviewMessage::OpenMountedPath => {
            if let Some(summary) = app.workbench.preview.mount_summary.as_ref() {
                let _ = app.services.host_shell.open_path(&summary.mount_point);
            }
            iced::Task::none()
        }
        PreviewMessage::OpenLocalWebInBrowser => {
            if let Some(url) = app.workbench.preview.local_web_url.as_ref() {
                let _ = app.services.host_shell.open_url(url);
            }
            iced::Task::none()
        }
        PreviewMessage::SetMountPoint(value) => {
            app.workbench.preview.mount_point_text = value;
            iced::Task::none()
        }
        PreviewMessage::SetSelectedSnapshot(value) => {
            app.workbench.preview.selected_snapshot_id = Some(value);
            iced::Task::none()
        }
        PreviewMessage::SetWritableLiveBranch(value) => {
            app.workbench.preview.writable_live_branch = value;
            iced::Task::none()
        }
    }
}
```

- [ ] **Step 4: Re-run the mount/host-launch tests to verify GREEN**

Run:

```powershell
cargo test -p e2v-gui snapshot_mount_is_rendered_read_only_and_backed_by_launch_summary
cargo test -p e2v-gui search_result_can_switch_to_preview_with_a_focused_path
cargo test -p e2v-gui open_mount_in_explorer_uses_the_host_shell_service
```

Expected: PASS

- [ ] **Step 5: Commit the preview mount and host-launch flows**

```powershell
git add crates/e2v-gui/src/app.rs crates/e2v-gui/src/domain.rs crates/e2v-gui/src/lib.rs crates/e2v-gui/src/testing.rs crates/e2v-gui/src/services/mod.rs crates/e2v-gui/src/services/preview_service.rs crates/e2v-gui/src/services/host_shell_service.rs crates/e2v-gui/src/pages/search.rs crates/e2v-gui/src/pages/preview.rs crates/e2v-gui/tests/search.rs crates/e2v-gui/tests/preview.rs
git commit -m "feat: add gui preview mounts and host launch actions"
```

## Task 7: Run The Phase 2 Verification Matrix

**Files:**
- Modify: any files touched by Tasks 1-6 only if verification finds a real defect

**Interfaces:**
- Consumes: rendered home/workbench shell, search page, sharing page, preview page, controller services
- Produces: a verified Phase 2 branch with visible, interactive shell rendering and passing regression coverage

- [ ] **Step 1: Run the GUI crate tests**

Run:

```powershell
cargo test -p e2v-gui
```

Expected: PASS for `smoke`, `registry`, `home`, `overview`, `history_branches`, `sync`, `render_shell`, `search`, `sharing`, and `preview`.

- [ ] **Step 2: Run the workspace regression suite**

Run:

```powershell
cargo test --workspace
```

Expected: PASS with no regressions in `e2v-api`, `e2v-cli`, `e2v-core`, `e2v-store`, `e2v-sync`, and `e2v-vfs`.

- [ ] **Step 3: Run the GUI crate manually on the host**

Run:

```powershell
cargo run -p e2v-gui
```

Expected: the app opens to a populated home screen, workbench navigation is visible after selecting a repository, and the `Search`, `Sharing`, and `Preview` pages render real controls instead of a blank title-only shell.

- [ ] **Step 4: Perform the required Windows manual checks**

Run:

```powershell
cargo run -p e2v-gui
```

Expected manual outcomes:

- Home renders repository cards and create/open forms.
- Search returns filename hits and can route a result into the `Preview` page.
- Sharing can list actors/devices, produce a copyable base64 invite bundle, and gate member/device revocation behind confirmation.
- Local web preview can start, show a `http://127.0.0.1:<port>` URL, and stop cleanly.
- Snapshot mount renders as read-only with stream-only status.
- Live-branch mount renders the real mount mode and cache-policy summary from `MountLaunchSummary`.
- Opening the mount path from the GUI launches the host explorer.
- Mounted image/media browsing remains smooth enough for ordinary local preview workflows.

- [ ] **Step 5: Review the diff and create the final Phase 2 implementation commit**

Run:

```powershell
git status --short
git diff --stat
git diff -- crates/e2v-gui
git add crates/e2v-gui
git commit -m "feat: add phase 2 gui collaboration and preview flows"
```

Expected: only intentional GUI shell, search, sharing, preview, and controller changes remain staged.
