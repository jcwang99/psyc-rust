# Iced GUI Workbench Design

## Goal

Build a standalone Iced desktop application that wraps the repository system's current controls in a user-facing GUI, with a multi-repository home screen, a daily-use-first workbench for each repository, and a clearly separated advanced/maintenance area for risky operations.

## Current State

- The workspace currently contains six crates: `e2v-api`, `e2v-cli`, `e2v-core`, `e2v-store`, `e2v-sync`, and `e2v-vfs`.
- There is no GUI crate or reusable desktop shell today.
- The CLI is the only complete user surface and currently exposes these command families:
  - repository lifecycle: `init`, `commit`, `snapshots`, `checkout`
  - sync and remotes: `push`, `fetch`, `pull`, `clone`, `remote add`
  - collaboration: `share ...`
  - browsing: `branch ...`, `search`
  - verification and repair: `verify ...`, `repair`, `doctor`, `diagnose`
  - maintenance: `gc ...`, `historical-rewrite ...`, `oram ...`
  - live services: `serve`, `mount snapshot`, `mount branch`
- `e2v-api::Sdk` already owns many mutating operations and should remain the preferred high-level boundary for GUI-triggered actions.
- `e2v_core::RepositoryFacade` is still used for local browse/search-style reads.
- `e2v_sync` already owns the local web serving entrypoint logic, and `e2v_vfs` already owns mount launching logic, but the current user-facing lifecycle still lives behind CLI-specific process entrypoints.

## Locked Product Decisions

- The GUI is a standalone desktop application, not a browser UI and not an embedded CLI wrapper.
- The primary user is the ordinary repository user, not the maintenance expert.
- The first screen is a multi-repository home, not a single-repo dashboard.
- Dangerous and low-frequency operations must be hidden by default and grouped into an advanced/maintenance area.
- Stability and safety take priority over visual cleverness or aggressive optimization.
- The GUI should model the current product surface only. It must not add new first-class concepts solely to preserve obsolete or compatibility-only interfaces.
- The GUI must call Rust interfaces directly for normal operations. It must not shell out to the CLI as its primary execution model.

## Approaches Considered

### 1. Repository Workbench

Use a multi-repository home page as the app entrypoint, then open each repository into a persistent workbench with stable navigation for history, sync, sharing, search, preview, and maintenance.

Pros:

- Best match for the chosen multi-repo entry model.
- Gives daily workflows a stable home without losing room for advanced controls later.
- Scales cleanly as more controls are added, because navigation is organized by user intent instead of by command count.

Cons:

- Requires deliberate information architecture up front.
- Needs stronger state management than a wizard-first UI.

### 2. Task-Led Home

Put the whole product around a task launcher: commit, sync, clone, mount, repair, share, and so on. Repositories become secondary context chosen inside each task flow.

Pros:

- Very easy for first-time users to understand.
- Works well for low-frequency workflows.

Cons:

- Starts simple but becomes crowded as every command family asks for a tile, modal, or wizard.
- Makes cross-feature repository context weak, which hurts long-term usability.

### 3. Dual-Shell Power UI

Split the app into a launcher shell and a second, denser IDE-like repository shell with many panels visible at once.

Pros:

- High information density for expert users.
- Plenty of room for logs, jobs, previews, and diagnostics.

Cons:

- Too heavy for the chosen primary persona.
- Highest implementation and onboarding cost.

## Recommended Direction

Use **Approach 1: Repository Workbench**.

The workbench is the best fit for the product decisions already made: multi-repository home, ordinary-user-first priorities, and hidden advanced operations. It also gives us the cleanest path to the real long-term goal: every current control must eventually have a coherent GUI home, but those controls do not all deserve equal prominence. The workbench lets us keep daily operations visible while still providing a safe place for destructive workflows.

We should still borrow one strength from Approach 2: the home screen and repository overview should expose a small set of obvious quick actions so the workbench does not feel heavy for common tasks.

## Scope Of This Design

This design defines the standalone GUI shell, command-to-screen mapping, shared interaction patterns, and the service/lifecycle boundaries needed for later feature implementation. It is intentionally focused on the architecture and user surface that every later GUI screen will sit on top of.

This design does **not** attempt to fully specify every dialog copy string, every pixel value, or every final visual refinement. It defines the structure, safety model, and component boundaries required to implement the GUI in slices without painting the project into a corner.

The first implementation plan derived from this design should target **Phase 1 only**. Phases 2 and 3 should be written as follow-up implementation plans on top of the same shell once the foundation is in place.

## Architecture

### 1. Crate Boundary

Add a new workspace crate: `crates/e2v-gui`.

`e2v-gui` owns:

- the Iced application entrypoint
- the top-level application model and message routing
- reusable panels, forms, and confirmation components
- repository registry and persisted GUI-only state
- background job orchestration for long-running operations
- screen modules for home, repository workbench pages, and maintenance flows

`e2v-gui` should consume existing crates through explicit service adapters:

- `e2v-api::Sdk` for repository mutations, sync, sharing, verification, repair, GC, historical rewrite, and ORAM actions
- `e2v_core::RepositoryFacade` for local read/search-style queries that do not yet have stable SDK wrappers
- `e2v_sync` for local web serving and diagnostics launch helpers
- `e2v_vfs` for mount launching and mount lifecycle handles

The GUI must not depend on CLI output text parsing. If a needed action is only available through a CLI-only helper today, we should extract that behavior into a library-facing function in the owning crate and call that function from both CLI and GUI.

### 2. Shell Structure

The app has two navigation levels:

1. **Multi-repository home**
   - recent repositories
   - pinned repositories
   - create / open / clone actions
   - recent job results
   - high-signal quick actions

2. **Per-repository workbench**
   - a stable left navigation rail
   - a top repository header with branch, snapshot, remote, and job indicators
   - a main content area for the active screen
   - a collapsible job/log drawer for long-running operations

The workbench navigation should be:

- `Overview`
- `History`
- `Branches`
- `Sync`
- `Search`
- `Sharing`
- `Preview`
- `Advanced`

This keeps daily operations near the top and quarantines risky controls without making them unreachable.

### 3. Repository Home

The home screen is not just a file picker. It is the user's control tower for all repositories known to the desktop app.

It should show:

- repository name and local path
- last-opened time
- current branch name when cheaply available
- current head snapshot summary when cheaply available
- whether a default remote is configured
- whether there are recent failed or in-progress jobs for that repository

The home screen should support these primary actions:

- create repository
- open existing repository folder
- clone remote repository
- reopen recent repository
- jump directly into `Commit`, `Push`, or `Pull` for the most recently used repository

The home screen needs its own GUI-only registry persisted outside repository storage. That registry stores pinned repositories, recent repositories, last-opened timestamps, and lightweight UI preferences. It must not mutate repository format or remote state.

### 4. Workbench Page Responsibilities

Each screen should have one clear purpose.

#### `Overview`

The repository summary and daily-launch screen.

Owns:

- branch and head snapshot summary
- latest snapshot message list or short history snippet
- quick actions for `commit`, `push`, `fetch`, `pull`
- clear status tiles for remote configuration, sharing state, and service availability

#### `History`

Owns:

- `snapshots`
- snapshot detail viewing
- `checkout` into a chosen target directory
- `verify snapshot`
- snapshot verification results or drill-down where that context is natural

#### `Branches`

Owns:

- `branch list`
- `branch create`
- `branch checkout`
- `branch delete`

#### `Sync`

Owns:

- `push`
- `fetch`
- `pull`
- `remote add`
- remote verification entrypoints
- sync-specific warnings such as single-writer-risk confirmations

#### `Search`

Owns:

- filename search
- metadata-backed extension search
- result navigation into snapshot or preview flows

#### `Sharing`

Owns:

- `share list`
- member invite / accept / revoke
- device invite / accept / revoke
- explicit display of actor/device identity and role impact before revocation

#### `Preview`

Owns:

- `serve`
- `mount snapshot`
- `mount branch`
- visible service state such as local URL, mount point, mode, and stop/restart controls

#### `Advanced`

Owns low-frequency and high-risk operations:

- `verify object`
- `repair`
- `doctor`
- `diagnose`
- `gc dry-run`
- `gc execute`
- `historical-rewrite plan`
- `historical-rewrite execute`
- `oram plan`
- `oram status`
- `oram enable`
- `oram reshuffle`

This page should default to an informational overview with grouped cards. Destructive actions should live behind explicit expansion or secondary dialogs, not on the first visible surface.

### 5. Command Coverage Matrix

The GUI must eventually cover the current CLI surface through this mapping:

| Command family | GUI home |
| --- | --- |
| `init`, `clone` | multi-repository home |
| `commit`, `push`, `fetch`, `pull` | `Overview` + `Sync` |
| `snapshots`, `checkout`, `verify snapshot` | `History` |
| `branch ...` | `Branches` |
| `search` | `Search` |
| `share ...` | `Sharing` |
| `remote add`, `verify remote`, sync safety warnings | `Sync` |
| `serve`, `mount ...` | `Preview` |
| `verify object`, `repair`, `doctor`, `diagnose` | `Advanced` |
| `gc ...`, `historical-rewrite ...`, `oram ...` | `Advanced` |

This matrix is important because the GUI goal is not "a nice front end for a subset." It is "a coherent GUI home for every current control." New work should be checked against this matrix so we do not quietly leave command families behind.

### 6. Shared Interaction Patterns

The GUI should use the same interaction grammar everywhere:

- **Primary action forms** for common operations such as commit, clone, push, pull, and mount
- **Secondary detail panels** for read-heavy surfaces such as history, verification summaries, and doctor output
- **Job drawer** for in-progress operations, recent results, warnings, and copyable logs
- **Inline validation** for missing passwords, invalid paths, malformed remote specs, and required confirmations
- **Confirmation sheets** for risky operations

Risky operations must use escalation that matches real consequences:

- `push --force-single-writer-risk`
- remote rollback acceptance
- member/device revocation
- `gc --execute`
- `historical-rewrite execute`
- ORAM enable/reshuffle

Those flows should require:

- an explicit explanation of the impact
- the exact repository and remote target being affected
- the relevant password/token fields when required
- a second deliberate user action before the job begins

### 7. Background Jobs And Responsiveness

The GUI must stay responsive while repository operations run. The app should therefore separate:

- UI state
- user intent messages
- background job execution
- job result delivery

The concrete implementation should use Iced's normal update/task/subscription model, but with a typed job layer inside `e2v-gui` so every long-running action has:

- a stable job identifier
- an operation kind
- a repository context
- a start time
- a visible state: queued, running, succeeded, failed, needs-confirmation follow-up
- structured result data where available

Long-running jobs should not block navigation. The user must be able to switch repositories or inspect another screen while a push, diagnosis run, mount, or historical rewrite is executing.

### 8. Service Lifecycle Controllers

`serve` and `mount` are special because they are not one-shot mutations; they create live processes or live handles.

The GUI should model them as managed controllers:

- **Serve controller**
  - start local web service
  - surface bound address and URL
  - stop and restart cleanly
  - keep the runtime handle alive without parking the whole GUI process

- **Mount controller**
  - start snapshot or branch mount
  - retain the mount handle while active
  - show mount mode, cache policy, access mode, and mount point
  - unmount or dispose cleanly from the GUI

This means some CLI-only lifecycle logic will need to move into reusable library-facing helpers. The GUI should never fake these flows by spawning a forever-running CLI subprocess and scraping stdout.

### 9. API Boundary Adjustments

The GUI should prefer stable, intention-level calls rather than rebuilding behavior from low-level internals. Where the current crates do not yet expose the right shape, we should add thin library helpers instead of letting `e2v-gui` reach deep into unrelated modules.

That likely means:

- keeping mutations and repository-changing flows centered on `e2v-api::Sdk`
- adding small adapter helpers for summary-level reads needed by the home screen and overview screen
- extracting non-CLI-specific service launch helpers where the current entrypoint is too terminal-shaped

The goal is not to move all logic into `e2v-gui`. The goal is to keep `e2v-gui` as a composition layer over clear domain interfaces.

## Data Flow

### Home and Repository Opening

1. User opens the app.
2. GUI loads the local repository registry.
3. Home screen renders recent/pinned repositories and lightweight summaries.
4. User creates, opens, or clones a repository.
5. The selected repository becomes the active workbench context.

### Common Mutation Flow

1. User fills a form on `Overview`, `Sync`, `Sharing`, `Preview`, or `Advanced`.
2. GUI validates local inputs immediately.
3. If the action is risky, the GUI shows a confirmation sheet with impact summary.
4. The action is submitted into the background job layer.
5. The appropriate adapter calls `Sdk`, `RepositoryFacade`, `e2v_sync`, or `e2v_vfs`.
6. The job drawer receives progress and final status.
7. Screens that depend on the changed data invalidate and refresh their summaries.

### Live Service Flow

1. User starts `serve` or `mount`.
2. GUI creates a managed controller and stores the handle in application state.
3. `Preview` shows active status and connection details.
4. On stop, repository switch, or app shutdown, the GUI disposes the handle cleanly.

## Error Handling

The GUI should present errors in user terms, not in CLI terms.

Required categories:

- **Input errors**
  - bad path
  - missing password
  - invalid percent/grace period
  - malformed remote spec

- **Safety blocks**
  - missing required confirmation
  - risky mode not explicitly enabled
  - rollback acceptance not confirmed

- **Repository/domain errors**
  - branch not found
  - remote not configured
  - snapshot/object not found
  - sharing actor/device mismatch

- **Service lifecycle errors**
  - mount failed to start
  - serve port binding failure
  - active controller already exists

- **Long-running operation failures**
  - push/fetch/pull failure
  - diagnostics failure
  - verification/repair failure
  - historical rewrite failure

Every failure shown in the GUI should include:

- a concise title
- a plain-language summary
- the repository or remote context
- a copyable detail section for the full technical error
- the next best user action when one is obvious

## Testing Strategy

### 1. App-State Reducer Tests

Test pure GUI state transitions without real repository I/O:

- navigation changes
- repository selection
- job creation and completion
- risky confirmation gating
- service controller registration and cleanup

### 2. Adapter Contract Tests

Define traits around the service layer so `e2v-gui` can be tested with fakes. Verify that GUI actions call the expected high-level domain operations with the right parameters.

This is especially important for:

- push/pull/fetch parameter construction
- share invite/accept/revoke requests
- GC and historical rewrite confirmations
- ORAM policy actions
- mount and serve start/stop requests

### 3. Repository Registry Tests

Test GUI-only persistence:

- recent repository ordering
- pin/unpin behavior
- stale path cleanup rules
- active-repository restoration

### 4. Integration Smoke Tests

Add focused integration coverage for the first implemented screens so we can prove:

- home can create/open/clone and enter a workbench
- workbench actions dispatch jobs without blocking the UI state model
- risky flows stay hidden until intentionally opened
- live service controllers remain stable across refreshes and navigation

### 5. Manual Windows Validation

Windows is a first-class target for this GUI. Every slice that touches preview or lifecycle control should include manual validation for:

- snapshot mount
- branch mount
- local web serve start/stop
- copying URLs and mount paths from the GUI
- closing the app while live controllers exist

## Implementation Phases

This architecture should be implemented in phases, but all phases must stay inside the same workbench shell.

### Phase 1: Shell And Daily Workbench

- add `e2v-gui`
- build multi-repository home
- build workbench shell
- implement `Overview`, `History`, `Branches`, and `Sync`
- add job drawer and confirmation sheet foundations

### Phase 2: Collaboration And Preview

- implement `Search`
- implement `Sharing`
- implement `Preview`
- introduce reusable live service controllers

### Phase 3: Maintenance And Power Features

- implement `Advanced`
- add verification, repair, doctor, diagnose, GC, historical rewrite, and ORAM flows
- complete risky-operation escalation patterns

This sequencing keeps common workflows usable early while still preserving a path to full command coverage.

Each phase after Phase 1 should become its own implementation plan rather than extending the first plan indefinitely.

## Non-Goals

- Building a web UI instead of a desktop app
- Preserving CLI-shaped interaction patterns where a clearer GUI pattern exists
- Re-implementing repository logic inside the GUI crate
- Adding legacy-only top-level concepts for deprecated or compatibility-only internals
- Defining final visual polish for every component before the shell and job model exist
