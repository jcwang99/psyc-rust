# P2-C SDK / API Stabilization Design

## Goal

Complete P2-C by turning the existing reusable core and sync building blocks into a stable Rust SDK surface for third-party callers, while keeping C-ABI publication deferred to P2-D.

## Current State

The workspace already contains most of the underlying behavior needed by a public SDK:

- `e2v-core::RepositoryFacade` already centralizes repository orchestration for init, unlock, open, commit, checkout, branch, sharing, and verification flows.
- `e2v-core::ReadService` already centralizes snapshot/branch resolution, directory listing, file open, and range reads for Web and VFS.
- `e2v-sync` already exposes reusable push, fetch, clone, remote verification, repair, and GC orchestration.
- `e2v-vfs` already depends on the shared read/browse path rather than reimplementing path traversal or range logic.

The main P2-C gaps are:

- there is no dedicated stable SDK crate that acts as the public Rust API boundary
- callers still need to know internal crate/module layout such as `facade`, `sync_support`, and CLI-only remote registry helpers
- public errors are still mostly ad-hoc `anyhow` strings rather than a stable, matchable SDK error model
- there are no contract tests that prove the intended public API is sufficient for third-party callers

## Constraints From `plan.md`

P2-C must satisfy these rules:

- Rust public API becomes stable in P2-C, while C-ABI remains a P2-D deliverable
- SDK callers must use the same orchestration layer as CLI/Web/VFS rather than bypassing into storage primitives
- SDK read access must flow through `ReadService`
- SDK must not directly access `BlobStore`, decode chunks, or construct physical paths
- upper-layer API must remain stable across direct layout, pack layout, and future storage layout evolution
- `e2v-api` must not bypass the orchestration layer

## Approaches Considered

### 1. Keep exporting `e2v-core` and `e2v-sync` directly

Pros:

- smallest immediate code diff
- no new crate

Cons:

- callers remain coupled to internal module layout
- error behavior stays unstable
- impossible to clearly separate supported API from internal helper surfaces

### 2. Add a dedicated `e2v-api` crate as the stable Rust SDK boundary

Pros:

- creates a single public contract for repository, read, and sync flows
- lets internal crates keep evolving behind a stable facade
- provides a clean place to normalize errors, handles, and remote configuration
- lines up directly with the `e2v-api/ rust_api.rs / c_abi.rs` intent in `plan.md`

Cons:

- requires new wrapper types and API contract tests
- some internal helper code must move or be rewrapped

### 3. Publish only documentation and examples for the current crates

Pros:

- lowest implementation cost

Cons:

- does not actually stabilize the API
- leaves third-party callers exposed to internal modules and drifting error strings

## Recommended Direction

Use approach 2.

Introduce a new `e2v-api` crate that becomes the only supported Rust SDK surface. It wraps `e2v-core::RepositoryFacade`, `e2v-core::ReadService`, and `e2v-sync` orchestration functions behind stable SDK types, stable error codes, and contract tests. Internal crates remain reusable implementation crates but are no longer the public contract promised by P2-C.

## Architecture

### 1. Public crate boundary

Add `crates/e2v-api` and make it the supported SDK entry point.

It exposes three stable sub-surfaces:

- repository API
  - init/open/unlock/change_password/commit/checkout/list_snapshots/branch/share
- read API
  - open_snapshot/resolve_branch/read_dir/open_file/read_range
- sync API
  - parse remote spec/add_remote/load default remote/push/fetch/clone/verify_remote/repair_remote/gc

`e2v-api` may depend on `e2v-core`, `e2v-sync`, and `e2v-store`, but SDK callers should not need to import those crates for normal usage.

### 2. Stable SDK types

`e2v-api` defines its own public option/result/handle types instead of re-exporting the internal ones wholesale.

Examples:

- `Sdk`
- `RepositoryHandle`
- `ReadHandle`
- `SnapshotView`
- `FileView`
- `RemoteRegistration`
- `InitRepositoryOptions`
- `CommitRepositoryOptions`
- `CheckoutSnapshotOptions`
- `PushRequest`
- `FetchRequest`
- `CloneRequest`

Internally these types translate to `e2v-core` and `e2v-sync` options. This keeps the outward contract stable even if internal option structs evolve.

### 3. Stable error model

Add a dedicated SDK error model:

- `SdkError`
- `SdkErrorCode`

`SdkErrorCode` is the matchable contract. Initial codes:

- `InvalidArgument`
- `NotFound`
- `AlreadyExists`
- `PermissionDenied`
- `AuthenticationRequired`
- `Conflict`
- `NeedsRebase`
- `RollbackDetected`
- `Unsupported`
- `CorruptState`
- `Io`
- `Internal`

Internal crates may continue returning `anyhow::Error`, but `e2v-api` must map them to stable codes at the public boundary. New code paths may improve internal typing later, but P2-C only requires the SDK boundary to be stable and predictable.

### 4. Remote configuration surface

Remote configuration should no longer be CLI-private.

Move the registry behavior currently trapped in `e2v-cli::remote_registry` into `e2v-api`, or promote it into a reusable shared implementation wrapped by `e2v-api`.

Supported SDK flows:

- validate and store a named default remote
- load the default remote registration
- run sync commands through either the stored default remote or an explicit remote spec

The SDK surface should not require the caller to duplicate the CLI registry file format.

### 5. Read/browse stability contract

The SDK read surface must preserve current cross-surface behavior:

- snapshot and branch reads use the same `ReadService`
- path normalization and traversal rejection remain shared behavior
- file handles stay snapshot-pinned
- range reads continue to work across direct and packed storage without exposing storage layout details

This keeps Web, VFS, and SDK on the same read contract.

## Testing Strategy

Add contract tests in `crates/e2v-api/tests` that verify:

- repositories can be initialized, opened, unlocked, committed, checked out, and listed through `e2v-api` alone
- snapshot and branch reads can be performed through `e2v-api` alone
- remote registration and default-remote sync flows can be performed through `e2v-api` alone
- stable error codes are returned for representative failures such as path traversal, missing repositories, invalid remote specs, and rebase/rollback conditions

The tests should avoid reaching into `e2v-core::facade`, CLI remote registry helpers, or `sync_support` unless they are explicitly validating that the SDK encapsulates those details.

## Non-Goals

P2-C does not include:

- publishing a stable C-ABI
- supporting unwind-safe FFI entry points
- replacing all internal `anyhow` usage with typed errors
- deprecating internal crates for workspace-internal use

Those items remain future work, with C-ABI specifically reserved for P2-D.
