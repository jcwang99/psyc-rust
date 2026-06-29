# P2-A Read-Only VFS Design

## Goal

Complete the P2-A read-only VFS slice by introducing a platform-independent VFS core that reuses `e2v-core::ReadService` for all repository reads and exposes thin platform adapters for Windows WinFSP first, with Linux FUSE and macOS macFUSE boundaries prepared for follow-up implementation.

## Current State

The repository already has the core read path needed by a VFS:

- `e2v-core::ReadService` can open snapshots, resolve branches, list directories, open files, and read authenticated byte ranges.
- `e2v-sync` already proves that Web and HTTP access can reuse `ReadService` without touching lower storage layers directly.
- The workspace does not yet contain an `e2v-vfs` crate.
- `e2v-cli` has no mount command or VFS-facing surface.

The main gaps against P2-A are:

- there is no VFS crate or platform adapter boundary
- there is no snapshot-pinned versus live-branch mount model
- there is no VFS file-handle model that pins reads to `snapshot_id + file_object_id + layout_generation`
- there is no explicit stream-only / unsupported-semantics boundary
- there is no direct-I/O fallback policy for platforms without reliable invalidation

## Constraints From plan.md

P2-A must satisfy these architecture rules:

- `e2v-vfs` only depends on stable read/browse APIs.
- VFS must use `ReadService`, not `ManifestStore`, `LogicalObjectStore`, `StorageLayout`, `BlobStore`, or physical paths directly.
- VFS must distinguish `snapshot-pinned` and `live-branch` mounts.
- Open file handles must stay pinned to the snapshot and file object visible at open time.
- Live mounts must invalidate kernel caches after branch-head changes, or fall back to direct I/O / disabled page cache if reliable invalidation is not available.
- The default positioning is stream-only access; unsupported semantics such as `mmap`-style expectations or byte-range locks must fail explicitly rather than pretending to behave like a normal disk.

## Approaches Considered

### 1. Put VFS logic in `e2v-cli` or `e2v-sync`

Pros:

- fastest to start
- fewer new workspace files

Cons:

- mixes platform callbacks with repository-read logic
- makes Linux/macOS follow-up harder
- violates the intended `e2v-vfs` boundary in `plan.md`

### 2. Add a dedicated `e2v-vfs` crate with thin platform adapters

Pros:

- matches the architecture described in `plan.md`
- keeps VFS core testable without a mounted kernel filesystem
- allows Windows-first delivery without locking Linux/macOS into WinFSP-specific shapes

Cons:

- requires a new crate and a small amount of scaffolding
- CLI needs a delegation boundary instead of owning the logic directly

### 3. Implement Windows WinFSP directly first and extract later

Pros:

- quickest route to a first mounted filesystem on the current platform

Cons:

- high risk of platform-specific logic leaking into the core
- almost guarantees rework for Linux/macOS
- makes TDD harder because the only verification surface becomes kernel integration

## Recommended Direction

Use approach 2.

Add a new `crates/e2v-vfs` workspace member that owns all platform-independent read-only VFS behavior and speaks only through `e2v-core::ReadService`. The first concrete platform adapter targets Windows WinFSP, while Linux FUSE and macOS macFUSE are represented by traits and test boundaries that keep the core stable for later implementation.

## Architecture

### 1. Crate Boundaries

Add `crates/e2v-vfs` with these responsibilities:

- mount configuration and mount-mode parsing
- snapshot-pinned versus live-branch namespace resolution
- inode/path bookkeeping
- open-file-handle binding to `snapshot_id + file_object_id + layout_generation`
- read-only policy enforcement
- stream-only capability reporting
- invalidation-policy decisions
- platform-adapter traits and the Windows adapter entrypoint

The crate must not:

- read manifests directly
- resolve physical object refs directly
- call storage backends directly
- persist or cache remote physical paths

`e2v-cli` only parses a mount request and delegates to `e2v-vfs`.

### 2. Core Types

The first iteration centers on a small synchronous core:

- `MountMode`
  - `SnapshotPinned { snapshot_id }`
  - `LiveBranch { branch_token_hex }`
- `CachePolicy`
  - `KernelCacheWithInvalidation`
  - `DirectIoFallback`
- `ReadOnlyVfs`
  - owns `repo_root`, `ReadService`, mount mode, cache policy, and namespace state
- `NamespaceView`
  - tracks the current root snapshot for directory lookups
- `OpenedFile`
  - stores the pinned `SnapshotHandle`, `FileHandle`, canonical path, and stable inode id

The core API remains platform-neutral and testable without kernel callbacks:

- create a mount
- inspect current namespace snapshot
- read directory entries by logical path
- open a file by logical path
- read an opened file by range
- refresh a live-branch namespace and report whether invalidation is required

### 3. Mount Modes

#### Snapshot-Pinned

- resolve the target snapshot once at mount creation
- never change the namespace snapshot afterward
- keep all later reads pinned to that snapshot even if the repository head changes

#### Live-Branch

- resolve the branch token at mount creation for the initial namespace view
- allow an explicit refresh operation to re-resolve the branch head
- old opened handles stay pinned to the snapshot they were opened from
- new namespace lookups and new opened handles use the refreshed snapshot

The platform adapter owns how refresh is triggered. The core only decides the new snapshot view and whether the namespace changed.

### 4. Invalidation And Cache Policy

The core accepts a platform capability declaration:

- reliable directory-entry invalidation
- reliable inode/attribute invalidation
- reliable page-cache invalidation

If all required invalidations are available, live-branch mounts use kernel caching with explicit invalidation requests on refresh.

If they are not available, the mount must switch to direct-I/O / disabled page cache policy instead of risking mixed-version reads.

This policy decision is made by the core so every platform adapter follows the same safety rules.

### 5. Stream-Only Semantics

The first read-only VFS does not claim normal local-disk semantics.

The core publishes explicit unsupported capabilities for:

- writable handles
- byte-range locks
- writeback caching
- memory-mapped-write assumptions

Platform adapters must map these to platform-specific unsupported/not-implemented errors rather than faking success.

### 6. Windows Adapter

The first concrete adapter is a Windows-only module in `e2v-vfs`:

- compiled behind `cfg(windows)`
- built on a thin adapter boundary so tests can exercise the core without WinFSP
- responsible only for translating WinFSP callbacks into core operations

Linux/macOS support is represented now by:

- a shared platform-adapter trait
- non-Windows stubs that return “not supported on this platform yet”
- tests proving CLI and crate routing choose the correct adapter boundary

### 7. CLI Surface

Add a `mount` command to `e2v-cli`:

- `mount snapshot --repo <repo> --snapshot <id> --mount-point <path>`
- `mount branch --repo <repo> --branch-token <token> --mount-point <path>`

The CLI:

- builds a VFS mount request
- delegates to `e2v-vfs`
- reports the selected cache policy and mount mode
- does not embed VFS namespace, read, or invalidation logic

## Testing Strategy

Use TDD in slices that prove the core before platform integration:

1. snapshot-pinned core semantics
2. live-branch refresh semantics with old-handle stability
3. cache-policy downgrade when invalidation is unavailable
4. unsupported-semantics reporting
5. CLI routing to the VFS crate
6. Windows adapter wiring behind `cfg(windows)`

Required evidence:

- a snapshot-pinned mount keeps returning the original bytes after the branch head advances
- a live-branch mount refresh updates new lookups while old handles still read the old snapshot
- lack of reliable invalidation forces direct-I/O policy
- unsupported semantics return explicit errors instead of silent success
- CLI mount commands delegate into `e2v-vfs` instead of reimplementing reads locally

## Non-Goals For This Slice

- writable VFS behavior
- background watcher/IPC implementation beyond the core refresh boundary
- direct manifest or storage-layer access from VFS
- full Linux/macOS kernel-adapter implementations in the first Windows-first increment
