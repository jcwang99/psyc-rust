# psyc-rust User Manual

This manual is written for real users of `psyc-rust`. It explains how to initialize a repository, create snapshots, configure remotes, synchronize state, manage sharing, and use advanced maintenance features from Windows PowerShell.

The document covers the stable CLI workflows that are already implemented and tested, plus a small set of advanced operational commands that should be used with care: `gc`, `historical-rewrite`, `oram`, and `serve`. All examples assume you are running the CLI from the workspace with `cargo run -p e2v-cli -- ...`.

## 1. What This Project Is

`psyc-rust` is a local-first repository system with snapshots, branches, remote synchronization, and sharing workflows. It takes files from a working directory, stores repository state in an encrypted internal format, and records history as immutable snapshots.

From a user perspective, you can think of it as a toolset that lets you:

- create a protected repository with `init`
- save the current working tree as a snapshot with `commit`
- inspect and restore history with `snapshots`, `checkout`, and `branch`
- synchronize with remotes using `remote`, `push`, `fetch`, `pull`, and `clone`
- manage shared members and devices with `share`
- inspect and repair state with `verify`, `repair`, `doctor`, and `gc`
- use stronger security and access-pattern workflows with `historical-rewrite` and `oram`
- browse repository contents through a local read-only web server with `serve`

## 2. Scope of This Manual

This manual assumes:

- environment: Windows PowerShell
- interface: CLI
- audience: repository owners, normal sync users, shared collaborators, operators
- depth: stable features plus advanced maintenance workflows

This manual does not focus on:

- Rust crate-level architecture
- secondary development with the SDK or C ABI
- the full platform-specific `mount` and VFS workflow

## 3. Directory Layout and Core Concepts

### 3.1 Repository Directory

After repository initialization, a hidden control directory appears under the repository root:

```text
<repo>\
  .e2v\
```

`.e2v` stores control-plane state, objects, key-related records, default remote registration, and other internal metadata. In normal usage, you should not edit these files manually. If the state becomes corrupted, prefer `verify`, `repair`, or `doctor` instead of editing JSON by hand.

### 3.2 Snapshots

Each `commit` creates an immutable snapshot of the current working tree. A snapshot has:

- a unique `snapshot_id`
- a commit message
- the full recorded file tree state

Snapshots are the basis for `checkout`, synchronization, integrity verification, and historical operations.

### 3.3 Branches

Every repository has at least one current branch. The default branch name is `main`. Internally, a branch also has a `branch token`, and many synchronization flows depend on that token to identify the tracked branch state.

### 3.4 Default Remote

After `remote add`, the repository stores a default remote registration. Commands such as `push`, `fetch`, `pull`, `verify remote`, `repair`, `gc`, `historical-rewrite`, `oram`, and `doctor` use that default remote automatically.

### 3.5 Local-First Workflow

The normal workflow is:

1. edit files in the local working directory
2. commit them into local snapshot history
3. synchronize with the remote afterward

That means:

- `commit` comes before `push`
- `fetch` downloads remote state into the local repository
- `pull` attempts to move the local current branch forward to the remote state

## 4. Environment Setup

### 4.1 Prerequisites

This repository is a Rust workspace. The CLI lives in `crates/e2v-cli`. If you are using it from source, the most direct invocation pattern is:

```powershell
cargo run -p e2v-cli -- --help
```

The current workspace uses:

- Rust edition: `2024`
- CLI parsing: `clap`
- local web serving: `axum`
- remote backend support: local folder, S3, WebDAV, and Alist

### 4.2 Recommended Invocation

In a development or test environment, use `cargo run`:

```powershell
cargo run -p e2v-cli -- init .\my-repo --password "correct horse battery staple"
```

If you have already built the CLI separately, you can also run `target\debug\e2v-cli.exe` or a release binary directly.

### 4.3 Getting Help

Top-level help:

```powershell
cargo run -p e2v-cli -- --help
```

Subcommand help:

```powershell
cargo run -p e2v-cli -- push --help
cargo run -p e2v-cli -- verify --help
cargo run -p e2v-cli -- historical-rewrite --help
```

## 5. Quick Start

This is a minimal first-run workflow.

### 5.1 Create a Repository

```powershell
New-Item -ItemType Directory -Path .\demo-repo | Out-Null
cargo run -p e2v-cli -- init .\demo-repo --password "correct horse battery staple"
```

Expected result:

- the repository is initialized
- the default branch is `main`
- the `.e2v` control directory is created

### 5.2 Add Content and Commit

```powershell
Set-Content -Path .\demo-repo\notes.txt -Value "hello psyc-rust"
cargo run -p e2v-cli -- commit --repo .\demo-repo --message "first snapshot"
```

Expected result:

- the CLI prints `committed ...`
- a new snapshot ID is created

### 5.3 List Snapshots

```powershell
cargo run -p e2v-cli -- snapshots --repo .\demo-repo
```

Expected result:

- you can see the new `snapshot_id`
- you can see the message `first snapshot`

### 5.4 Check Out a Snapshot Into a Separate Directory

```powershell
New-Item -ItemType Directory -Path .\checkout-out | Out-Null
cargo run -p e2v-cli -- checkout --repo .\demo-repo --snapshot <SNAPSHOT_ID> --target .\checkout-out
```

This materializes the selected snapshot into `.\checkout-out` without replacing your original repository root.

## 6. Initialization and Local Versioning

### 6.1 `init`

Usage:

```powershell
cargo run -p e2v-cli -- init <REPO> --password <PASSWORD> [--branch <BRANCH>]
```

Examples:

```powershell
cargo run -p e2v-cli -- init .\repo-a --password "correct horse battery staple"
cargo run -p e2v-cli -- init .\repo-b --password "another secret" --branch trunk
```

Notes:

- `REPO` is the repository root.
- `--password` is required.
- `--branch` is optional and defaults to `main`.

Recommendations:

- treat the initialization password as a high-value secret
- do not hardcode it into shared scripts or logs
- if you plan to use sharing and multiple devices, keep password handling disciplined

### 6.2 `commit`

Usage:

```powershell
cargo run -p e2v-cli -- commit --repo <REPO> --message <MESSAGE>
```

Example:

```powershell
cargo run -p e2v-cli -- commit --repo .\demo-repo --message "seed"
```

Behavior:

- reads the current repository working tree
- creates a new snapshot
- prints a shortened snapshot identifier

Best practice:

- use meaningful commit messages
- commit before pushing so local changes are explicitly captured in snapshot history

### 6.3 `snapshots`

Usage:

```powershell
cargo run -p e2v-cli -- snapshots --repo <REPO>
```

Typical output looks like:

```text
<snapshot_id> first snapshot
<snapshot_id> seed
```

Use it to:

- inspect repository history
- retrieve a `snapshot_id` for `checkout` or verification

### 6.4 `checkout`

Usage:

```powershell
cargo run -p e2v-cli -- checkout --repo <REPO> --snapshot <SNAPSHOT> --target <TARGET_DIR>
```

Notes:

- `checkout` restores a selected snapshot into a target directory
- it behaves more like "export this snapshot" than "switch my live working tree to old history"

Best practice:

- use a fresh directory for every checkout
- if the target directory already contains data, back up anything important first

## 7. Branch Management

### 7.1 List Branches

```powershell
cargo run -p e2v-cli -- branch --repo .\demo-repo list
```

Output characteristics:

- the current branch is marked with `*`
- branch names may be shown with their head snapshot IDs

### 7.2 Create a Branch

```powershell
cargo run -p e2v-cli -- branch --repo .\demo-repo create feature
```

### 7.3 Check Out a Branch

```powershell
cargo run -p e2v-cli -- branch --repo .\demo-repo checkout feature
```

### 7.4 Delete a Branch

```powershell
cargo run -p e2v-cli -- branch --repo .\demo-repo delete feature
```

Things to watch:

- do not delete a branch you still need
- make sure you are no longer on that branch before deleting it

Recommended workflow:

1. prepare base state on `main`
2. run `branch create <name>`
3. run `branch checkout <name>`
4. continue committing on that branch

## 8. Search and Read-Only Browsing

### 8.1 `search`

Usage:

```powershell
cargo run -p e2v-cli -- search <QUERY> --repo <REPO>
```

Example:

```powershell
cargo run -p e2v-cli -- search notes --repo .\demo-repo
```

Current behavior:

- it first searches by filename
- if there is no filename hit, it falls back to a metadata-style search path

Good for:

- quickly locating files by name
- lightweight lookup in the local snapshot index

### 8.2 `serve`

Usage:

```powershell
cargo run -p e2v-cli -- serve --repo <REPO>
```

Purpose:

- starts a local read-only web server
- prints a local address, typically a `localhost` or `127.0.0.1` URL

Example:

```powershell
cargo run -p e2v-cli -- serve --repo .\demo-repo
```

Useful for:

- browsing snapshots and directory trees in a browser
- temporary local read-only inspection

Notes:

- this is a local service, not a hosted public sharing surface
- the process remains running until you stop it manually

## 9. Remote Configuration and Synchronization

### 9.1 Remote Types Overview

At the user level, the most common remote types are:

- local folder remote: `file:///...`
- S3-compatible remote: `s3+https://...`
- WebDAV remote: `webdav+https://...`
- Alist remote: `alist+https://...`

Based on tested parsing behavior, examples include:

Local folder:

```text
file:///C:/e2v-remote
```

S3:

```text
s3+https://alice:secret@s3.example.com/example-bucket/sync-root?region=us-east-1
```

WebDAV:

```text
webdav+https://alice:secret@example.com/repo
```

Alist:

```text
alist+https://token@example.com/remote-root
```

Important:

- the credentials in these examples are placeholders only
- never keep real credentials in public scripts, chat logs, or tickets

### 9.2 Add a Default Remote

Usage:

```powershell
cargo run -p e2v-cli -- remote --repo <REPO> add <NAME> <URL>
```

Example:

```powershell
New-Item -ItemType Directory -Path .\remote-store | Out-Null
cargo run -p e2v-cli -- remote --repo .\demo-repo add origin file:///C:/remote-store
```

Notes:

- `NAME` is typically `origin`
- `URL` is a remote spec string
- once this is configured, the repository has a default remote

### 9.3 `push`

Usage:

```powershell
cargo run -p e2v-cli -- push --repo <REPO>
```

Prerequisites:

- the repository already has a default remote
- the current branch already has at least one local snapshot

Behavior:

- publishes the current branch head to the default remote
- prints the published snapshot prefix

Typical workflow:

```powershell
Set-Content -Path .\demo-repo\notes.txt -Value "version 2"
cargo run -p e2v-cli -- commit --repo .\demo-repo --message "update notes"
cargo run -p e2v-cli -- push --repo .\demo-repo
```

### 9.4 `fetch`

Usage:

```powershell
cargo run -p e2v-cli -- fetch --repo <REPO> [--password <PASSWORD>]
```

Notes:

- `fetch` downloads objects and branch state from the default remote into the local repository
- unlike `pull`, it does not directly move the current branch to the remote head
- some flows may need `--password` for unlock or key-state handling

Example:

```powershell
cargo run -p e2v-cli -- fetch --repo .\demo-repo --password "correct horse battery staple"
```

### 9.5 `pull`

Usage:

```powershell
cargo run -p e2v-cli -- pull --repo <REPO> [--password <PASSWORD>]
```

Notes:

- `pull` tries to bring the current local branch forward to the remote state
- fast-forward style updates normally succeed
- diverged local and remote history is not silently overwritten

Example:

```powershell
cargo run -p e2v-cli -- pull --repo .\demo-repo --password "correct horse battery staple"
```

Important behavior:

- if the local repository has new history and the remote also moved in an incompatible way, `pull` fails with a diverged or conflict-style error
- do not assume the system will auto-merge for you

### 9.6 `clone`

Usage:

```powershell
cargo run -p e2v-cli -- clone <REMOTE_SPEC> <TARGET_REPO_ROOT> --password <PASSWORD> --branch-token <BRANCH_TOKEN>
```

Example:

```powershell
cargo run -p e2v-cli -- clone file:///C:/remote-store .\demo-clone --password "correct horse battery staple" --branch-token <BRANCH_TOKEN>
```

Notes:

- `clone` requires an explicit remote
- it also requires the target repository path
- and it requires the `branch-token` to follow

If you do not know the branch token:

- obtain it from the repository owner or trusted peer
- the current CLI sync model uses it as a core identifier for the tracked branch

## 10. Verification, Diagnostics, and Repair

### 10.1 `verify snapshot`

Usage:

```powershell
cargo run -p e2v-cli -- verify --repo <REPO> snapshot <SNAPSHOT_ID>
```

Purpose:

- verifies that the selected snapshot graph is intact, readable, and not corrupted

### 10.2 `verify object`

Usage:

```powershell
cargo run -p e2v-cli -- verify --repo <REPO> object <EXPECTED_TYPE> <OBJECT_ID>
```

Example:

```powershell
cargo run -p e2v-cli -- verify --repo .\demo-repo object snapshot <SNAPSHOT_ID>
```

Good for:

- investigating a specific corrupted object
- doing precise low-level verification during troubleshooting

### 10.3 `verify remote`

Usage:

```powershell
cargo run -p e2v-cli -- verify --repo <REPO> remote --sample <SAMPLE_PERCENT>
```

Example:

```powershell
cargo run -p e2v-cli -- verify --repo .\demo-repo remote --sample 100%
```

Notes:

- performs sampled verification against the default remote
- `100%` is the heaviest but most complete option
- output typically includes the number of sampled objects and local repair statistics

Recommendation:

- use `100%` on small repositories
- on larger repositories, start with a smaller sample and reserve full verification for maintenance windows

### 10.4 `repair`

Usage:

```powershell
cargo run -p e2v-cli -- repair --repo <REPO>
```

Or, for dangerous rollback acceptance:

```powershell
cargo run -p e2v-cli -- repair --repo <REPO> --force-accept-remote-rollback --confirm-remote-rollback --password <PASSWORD>
```

Normal repair mode:

- attempts to repair missing or corrupted local objects from the default remote

Dangerous rollback mode:

- explicitly accepts remote rollback
- rebuilds the local fact view from remote state

Dangerous mode requirements:

- `--force-accept-remote-rollback`
- `--confirm-remote-rollback`
- `--password`

If the second confirmation flag is missing, the command refuses to run.

### 10.5 `doctor`

Usage:

```powershell
cargo run -p e2v-cli -- doctor --repo <REPO>
```

Generate a diagnostic bundle:

```powershell
cargo run -p e2v-cli -- doctor --repo <REPO> --bundle .\doctor-bundle
```

Purpose:

- prints a summary of remote and trusted-state information
- reports whether GC execution is supported
- writes a structured diagnostic bundle when `--bundle` is used

Tested bundle outputs include:

- `doctor-summary.json`
- `trusted-state.json`

Security behavior:

- diagnostic bundles redact local paths, `file:///` remote paths, S3 credentials, and related sensitive values

Good for:

- troubleshooting
- sharing structured diagnostics with teammates
- preserving a sanitized state snapshot for investigation

## 11. Sharing and Collaboration

Sharing lets you manage authorization at both member and device level.

### 11.1 List Current Sharing State

```powershell
cargo run -p e2v-cli -- share --repo .\demo-repo list
```

The output typically includes:

- actor records
- device records

### 11.2 Invite a Member

```powershell
cargo run -p e2v-cli -- share --repo .\demo-repo invite-member --name Alice --out .\alice-member-invite.bin
```

Notes:

- creates an invitation bundle file
- this bundle should be delivered through a secure channel

### 11.3 Accept a Member Invite

```powershell
cargo run -p e2v-cli -- share --repo .\demo-repo accept-member --bundle .\alice-member-invite.bin --label alice-laptop
```

### 11.4 Invite a Device

```powershell
cargo run -p e2v-cli -- share --repo .\demo-repo invite-device --actor <ACTOR_ID> --label alice-phone --out .\alice-device-invite.bin
```

### 11.5 Accept a Device Invite

```powershell
cargo run -p e2v-cli -- share --repo .\demo-repo accept-device --bundle .\alice-device-invite.bin --label alice-phone
```

### 11.6 Revoke a Member

```powershell
cargo run -p e2v-cli -- share --repo .\demo-repo revoke-member --actor <ACTOR_ID> --password "correct horse battery staple"
```

### 11.7 Revoke a Device

```powershell
cargo run -p e2v-cli -- share --repo .\demo-repo revoke-device --device <DEVICE_ID> --password "correct horse battery staple"
```

Sharing security recommendations:

- treat invitation bundles as sensitive material
- do not send them through untrusted public channels
- keep an internal record for member and device revocations

## 12. Advanced Operations

This chapter covers stronger, heavier, and more sensitive operational commands.

### 12.1 `gc`

`gc` is used for remote garbage collection analysis and execution.

#### 12.1.1 Dry Run

```powershell
cargo run -p e2v-cli -- gc --repo .\demo-repo --dry-run
```

Purpose:

- reports unreachable remote physical references
- reveals active intent state and related cleanup conditions

Good for:

- estimating cleanup impact before deleting anything
- preparing a maintenance window

#### 12.1.2 Execute

```powershell
cargo run -p e2v-cli -- gc --repo .\demo-repo --execute --grace-period 30d --confirm-single-writer-maintenance-window
```

Notes:

- `--grace-period` is required
- the CLI accepts values such as `30d`
- for single-writer style remotes, you must explicitly confirm the maintenance window

Why confirmation is required:

- physical remote deletion affects recovery options
- the current model expects explicit operator awareness before destructive cleanup

If you omit `--confirm-single-writer-maintenance-window`, the command refuses to run and reports a maintenance-window-style error.

### 12.2 `historical-rewrite`

This is a high-risk security operation for historical strong revocation and full reachable-history rewriting.

#### 12.2.1 Review the Plan

```powershell
cargo run -p e2v-cli -- historical-rewrite --repo .\demo-repo plan
```

Current plan output includes fields such as:

- `historical strong revocation plan`
- reachable objects
- remote loose objects
- remote packed objects
- old epochs
- advisory messages

Use it when:

- a revoked member should no longer retain access through older reachable history
- you need a stronger historical protection workflow

#### 12.2.2 Execute the Rewrite

```powershell
cargo run -p e2v-cli -- historical-rewrite --repo .\demo-repo execute --password "correct horse battery staple" --confirm-full-reencryption
```

Requirements:

- `--password`
- `--confirm-full-reencryption`

Without the second confirmation flag, the command refuses to run.

Risk warning:

- this is not a routine sync command
- it may rewrite reachable history, retire old epochs, and leave stale remote references that should later be handled by GC
- always run `plan` first
- strongly consider making full local and remote backups before execution

### 12.3 `oram`

`oram` controls oblivious-layout style access-pattern-hiding workflows.

#### 12.3.1 View the Plan

```powershell
cargo run -p e2v-cli -- oram --repo .\demo-repo plan
```

Typical output includes:

- `oblivious layout plan`
- real reads
- cover reads
- bytes per request
- write amplification

#### 12.3.2 Check Status

```powershell
cargo run -p e2v-cli -- oram --repo .\demo-repo status
```

This shows:

- current layout mode
- dedup mode
- layout generation
- oblivious generation
- policy

#### 12.3.3 Enable

```powershell
cargo run -p e2v-cli -- oram --repo .\demo-repo enable --policy balanced
```

#### 12.3.4 Reshuffle

```powershell
cargo run -p e2v-cli -- oram --repo .\demo-repo reshuffle --policy balanced
```

Recommendations:

- do not enable it casually without understanding the cost model
- run `plan` before `enable`
- on larger repositories, pay attention to read amplification, write amplification, and maintenance cost

## 13. Recommended Workflows

### 13.1 Single-Machine Daily Use

```powershell
cargo run -p e2v-cli -- init .\repo --password "correct horse battery staple"
Set-Content -Path .\repo\notes.txt -Value "v1"
cargo run -p e2v-cli -- commit --repo .\repo --message "seed"
cargo run -p e2v-cli -- snapshots --repo .\repo
```

### 13.2 Configure a Default Remote and Publish

```powershell
New-Item -ItemType Directory -Path C:\remote-store | Out-Null
cargo run -p e2v-cli -- remote --repo .\repo add origin file:///C:/remote-store
cargo run -p e2v-cli -- push --repo .\repo
```

### 13.3 Clone and Sync on a Second Machine

```powershell
cargo run -p e2v-cli -- clone file:///C:/remote-store .\repo-clone --password "correct horse battery staple" --branch-token <BRANCH_TOKEN>
cargo run -p e2v-cli -- remote --repo .\repo-clone add origin file:///C:/remote-store
cargo run -p e2v-cli -- fetch --repo .\repo-clone --password "correct horse battery staple"
cargo run -p e2v-cli -- pull --repo .\repo-clone --password "correct horse battery staple"
```

### 13.4 Periodic Health Checks

```powershell
cargo run -p e2v-cli -- verify --repo .\repo remote --sample 100%
cargo run -p e2v-cli -- doctor --repo .\repo --bundle .\doctor-out
cargo run -p e2v-cli -- gc --repo .\repo --dry-run
```

## 14. Common Problems and Troubleshooting

### 14.1 `pull` Fails With diverged or conflict

Meaning:

- local and remote both moved forward independently, and the system refused to overwrite one side silently

Recommended response:

- determine which side should be preserved
- avoid jumping directly to dangerous repair commands
- inspect state first with `verify` and `doctor`

### 14.2 `repair` Demands a Second Confirmation

This usually means you requested a dangerous action such as accepting remote rollback. The CLI is intentionally designed to require extra intent signaling before that kind of state transition.

### 14.3 `gc --execute` Reports maintenance window

Meaning:

- the current remote capability model requires explicit single-writer maintenance-window confirmation

Recommended response:

- run `--dry-run` first
- schedule a maintenance window
- then rerun with `--confirm-single-writer-maintenance-window`

### 14.4 `historical-rewrite execute` Refuses to Run

If `--confirm-full-reencryption` is missing, refusal is expected behavior.

### 14.5 Does a `doctor` Bundle Leak Paths or Credentials?

Based on current test coverage, the bundle redacts:

- local repository paths
- local-folder remote paths
- `file:///` URLs
- S3 credentials and bucket names

Even so, treat the diagnostic bundle as internal material.

## 15. Security Recommendations

- keep initialization passwords in a proper secret manager
- do not expose remote credentials in public scripts
- treat invitation bundles and diagnostic bundles as sensitive files
- for dangerous commands, run `plan`, `verify`, and `doctor` first when possible
- consider offline backups before `historical-rewrite`, `gc --execute`, or forced rollback acceptance

## 16. Command Cheat Sheet

### 16.1 Basic Commands

```powershell
cargo run -p e2v-cli -- init <REPO> --password <PASSWORD> [--branch <BRANCH>]
cargo run -p e2v-cli -- commit --repo <REPO> --message <MESSAGE>
cargo run -p e2v-cli -- snapshots --repo <REPO>
cargo run -p e2v-cli -- checkout --repo <REPO> --snapshot <SNAPSHOT> --target <TARGET_DIR>
cargo run -p e2v-cli -- branch --repo <REPO> list
cargo run -p e2v-cli -- branch --repo <REPO> create <NAME>
cargo run -p e2v-cli -- branch --repo <REPO> checkout <NAME>
cargo run -p e2v-cli -- branch --repo <REPO> delete <NAME>
cargo run -p e2v-cli -- search <QUERY> --repo <REPO>
```

### 16.2 Sync Commands

```powershell
cargo run -p e2v-cli -- remote --repo <REPO> add <NAME> <URL>
cargo run -p e2v-cli -- push --repo <REPO>
cargo run -p e2v-cli -- fetch --repo <REPO> [--password <PASSWORD>]
cargo run -p e2v-cli -- pull --repo <REPO> [--password <PASSWORD>]
cargo run -p e2v-cli -- clone <REMOTE_SPEC> <TARGET_REPO_ROOT> --password <PASSWORD> --branch-token <BRANCH_TOKEN>
```

### 16.3 Sharing Commands

```powershell
cargo run -p e2v-cli -- share --repo <REPO> list
cargo run -p e2v-cli -- share --repo <REPO> invite-member --name <NAME> --out <OUT>
cargo run -p e2v-cli -- share --repo <REPO> accept-member --bundle <BUNDLE> --label <LABEL>
cargo run -p e2v-cli -- share --repo <REPO> invite-device --actor <ACTOR> --label <LABEL> --out <OUT>
cargo run -p e2v-cli -- share --repo <REPO> accept-device --bundle <BUNDLE> --label <LABEL>
cargo run -p e2v-cli -- share --repo <REPO> revoke-member --actor <ACTOR> --password <PASSWORD>
cargo run -p e2v-cli -- share --repo <REPO> revoke-device --device <DEVICE> --password <PASSWORD>
```

### 16.4 Verification and Maintenance Commands

```powershell
cargo run -p e2v-cli -- verify --repo <REPO> snapshot <SNAPSHOT_ID>
cargo run -p e2v-cli -- verify --repo <REPO> object <EXPECTED_TYPE> <OBJECT_ID>
cargo run -p e2v-cli -- verify --repo <REPO> remote --sample <SAMPLE_PERCENT>
cargo run -p e2v-cli -- repair --repo <REPO>
cargo run -p e2v-cli -- repair --repo <REPO> --force-accept-remote-rollback --confirm-remote-rollback --password <PASSWORD>
cargo run -p e2v-cli -- doctor --repo <REPO> [--bundle <BUNDLE>]
cargo run -p e2v-cli -- gc --repo <REPO> --dry-run
cargo run -p e2v-cli -- gc --repo <REPO> --execute --grace-period <DAYS> --confirm-single-writer-maintenance-window
cargo run -p e2v-cli -- historical-rewrite --repo <REPO> plan
cargo run -p e2v-cli -- historical-rewrite --repo <REPO> execute --password <PASSWORD> --confirm-full-reencryption
cargo run -p e2v-cli -- oram --repo <REPO> plan
cargo run -p e2v-cli -- oram --repo <REPO> status
cargo run -p e2v-cli -- oram --repo <REPO> enable --policy balanced
cargo run -p e2v-cli -- oram --repo <REPO> reshuffle --policy balanced
cargo run -p e2v-cli -- serve --repo <REPO>
```

## 17. Final Advice

If you are new to this project, learn the system in this order:

1. `init`, `commit`, `snapshots`
2. `checkout`, `branch`
3. `remote add`, `push`, `fetch`, `pull`, `clone`
4. `verify`, `repair`, `doctor`
5. `gc`, `historical-rewrite`, `oram`

Leaving the advanced maintenance commands until you are already comfortable with normal synchronization and recovery workflows is the safest approach.
