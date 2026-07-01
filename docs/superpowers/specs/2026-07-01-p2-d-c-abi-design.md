# P2-D C-ABI Design

## Goal

Complete P2-D by publishing a stable C-ABI for the existing `e2v-api` Rust SDK, with opaque C handles, a C-compatible error and result ownership model, and panic-safe FFI boundaries.

## Current State

The workspace already has the Rust-side pieces needed to support a C boundary:

- `crates/e2v-api/src/lib.rs` provides a stable Rust SDK surface for repository, read, sync, and maintenance flows.
- `SdkErrorCode` and `SdkError` already define a stable Rust-facing error contract.
- `ReadHandle`, `SnapshotView`, and `FileView` already represent the stable read-path abstractions that Web and VFS also rely on indirectly through `ReadService`.

The missing P2-D pieces are:

- no `extern "C"` surface exists today
- no opaque-handle model exists for C callers
- no C-compatible ownership and free contract exists for returned strings, byte buffers, handles, or errors
- no panic boundary protection exists at the C export surface
- no generated C header or ABI-level contract tests exist

## Constraints From `plan.md`

P2-D must satisfy these explicit rules:

- publish a stable C-ABI after the P2-C Rust API is stable
- use opaque C handles
- use a C-compatible error and handle model
- wrap every exported function in `catch_unwind` so panic never crosses the FFI boundary
- keep the API layered on top of the same orchestration boundary rather than bypassing into storage primitives

The existing workspace dependency rules still apply:

- `e2v-api` remains the top-level API boundary
- C-ABI must not bypass `RepositoryFacade`, `ReadService`, or the stable Rust SDK surface
- the ABI must stay insulated from direct-layout, pack-layout, and future storage layout evolution

## Approaches Considered

### 1. Export internal crates directly through FFI

Pros:

- shortest path to some exported symbols

Cons:

- leaks internal crate structure into the C contract
- duplicates stabilization work already done in `e2v-api`
- makes the ABI brittle when internals evolve

### 2. Build the C-ABI on top of `e2v-api`

Pros:

- keeps one supported public contract stack: internal crates -> stable Rust SDK -> stable C-ABI
- reuses the stable error model and read/repository semantics from P2-C
- minimizes ABI churn by translating only stable Rust SDK types
- matches the architecture intent in `plan.md`

Cons:

- requires explicit handle wrappers and ownership helpers
- adds ABI-specific tests and header generation work

### 3. Generate a header from the current Rust SDK without a dedicated FFI layer

Pros:

- low initial effort

Cons:

- Rust-native types like `String`, `Vec`, `PathBuf`, and result-returning methods are not ABI-safe
- does not solve panic boundaries or ownership rules
- would produce an unusable or misleading C contract

## Recommended Direction

Use approach 2.

Keep `crates/e2v-api` as the only outward-facing API package and add a dedicated `c_abi` module plus a generated public header. The C-ABI wraps the stable Rust SDK instead of wrapping internal crates.

## Architecture

### 1. ABI boundary placement

The ABI lives inside `crates/e2v-api`:

- `src/lib.rs` continues to own the stable Rust SDK
- `src/c_abi.rs` owns FFI-safe exported types and functions
- `include/e2v_api.h` becomes the generated or checked-in public header consumed by C callers

`Cargo.toml` for `e2v-api` should publish a C-loadable library artifact through `cdylib`, while still supporting the existing Rust library use case.

### 2. Opaque handle model

The ABI uses opaque structs with no public fields:

- `e2v_sdk_t`
- `e2v_read_handle_t`
- `e2v_snapshot_view_t`
- `e2v_file_view_t`
- `e2v_error_t`

Each handle internally owns the corresponding stable Rust SDK value:

- `e2v_sdk_t` owns `Sdk`
- `e2v_read_handle_t` owns `ReadHandle`
- `e2v_snapshot_view_t` owns `SnapshotView`
- `e2v_file_view_t` owns `FileView`
- `e2v_error_t` owns `SdkError`

Creation and destruction are explicit. Every owning handle has a matching `*_free` function. Double-free remains caller UB as with normal C ownership contracts, so the ABI must document that ownership transfers exactly once.

### 3. C-compatible value model

The ABI cannot expose Rust-native collections or strings directly. It uses simple `#[repr(C)]` value containers:

- `e2v_string_t`
  - `const char *ptr`
  - `size_t len`
- `e2v_bytes_t`
  - `const uint8_t *ptr`
  - `size_t len`

Both are owned return values that must be released with matching `e2v_string_free` and `e2v_bytes_free`.

For structured data, the ABI uses one of two patterns:

- output parameters for primitive/scalar fields
- owned JSON strings for list-like or nested results that would otherwise require many ABI-specific mirror structs

For P2-D, the first release should prefer JSON strings for complex structured outputs such as snapshot lists and directory listings. This keeps the ABI stable while preserving the richer Rust-side shapes.

### 4. Error model

The ABI error contract mirrors the stable Rust `SdkErrorCode` with a `#[repr(C)]` enum:

- `E2V_OK = 0`
- `E2V_INVALID_ARGUMENT`
- `E2V_NOT_FOUND`
- `E2V_ALREADY_EXISTS`
- `E2V_PERMISSION_DENIED`
- `E2V_AUTHENTICATION_REQUIRED`
- `E2V_CONFLICT`
- `E2V_NEEDS_REBASE`
- `E2V_ROLLBACK_DETECTED`
- `E2V_UNSUPPORTED`
- `E2V_CORRUPT_STATE`
- `E2V_IO`
- `E2V_INTERNAL`
- `E2V_INTERNAL_PANIC`

Every fallible ABI function returns an `e2v_error_code_t`. On failure it also writes an owned `e2v_error_t*` through an out-parameter. Callers can inspect:

- `e2v_error_code(error)`
- `e2v_error_message(error, out_string)`

This gives C callers both a stable matchable code and a human-readable message without embedding unstable string matching into the API contract.

### 5. Panic boundary

Every exported function must be wrapped in a single shared helper that:

- validates non-null pointer preconditions
- runs the Rust body in `std::panic::catch_unwind`
- maps Rust success/failure into `e2v_error_code_t`
- converts panics into `E2V_INTERNAL_PANIC`
- never allows unwind to cross the ABI boundary

This is a non-negotiable safety requirement from `plan.md`.

### 6. Initial surface area

P2-D should cover all stable Rust SDK functionality required to make the C-ABI genuinely usable, not just a toy subset.

The published first-pass ABI surface should include:

- lifecycle
  - create/free sdk
  - free read/snapshot/file/error/string/bytes handles
- repository flows
  - init repository
  - open repository
  - unlock repository
  - commit repository
  - checkout snapshot
  - list snapshots
  - verify snapshot
  - change password
  - create/list/checkout/delete branch
- read flows
  - open read handle
  - open snapshot
  - resolve branch
  - read directory
  - open file
  - read range
- remote/sync flows
  - parse remote spec
  - add remote
  - load default remote
  - push default remote
  - fetch default remote
  - clone remote
  - verify default remote
  - repair default remote
  - force accept default remote rollback
  - gc default remote dry-run
  - gc default remote execute
- share flows
  - share list
  - invite/accept member
  - invite/accept device
  - revoke member/device

To avoid exploding the ABI shape, complex return values should be serialized as stable JSON payloads whose schema is derived from the already-stable `serde`-serializable SDK structs.

### 7. Header contract

The public header must:

- define all exported enums, opaque typedefs, and owned value containers
- document ownership transfer and matching free functions
- declare nullability expectations for every argument
- be stable enough for downstream C or C++ compilation tests

The header should be generated from the Rust declarations, then checked into the repo so the contract is reviewable and diffable.

## Testing Strategy

P2-D needs ABI-level tests, not just Rust unit coverage.

### Rust-side contract tests

Add tests that prove:

- handle creation and destruction work
- null out-parameters return `E2V_INVALID_ARGUMENT`
- Rust SDK errors map to the expected ABI error codes
- panic inside an exported wrapper returns `E2V_INTERNAL_PANIC`
- owned strings and byte buffers round-trip and free correctly

### C consumer smoke test

Add at least one integration test that compiles and runs a tiny C program against the generated header and produced library. That program should:

- create a repository through the ABI
- commit a file
- open a read handle
- open a snapshot or resolve a branch
- read file bytes
- destroy all owned resources cleanly

This proves the header and produced symbols are actually consumable from C rather than merely type-checking inside Rust.

### Verification

Completion evidence should include:

- `cargo test -p e2v-api`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- focused ABI smoke test output proving a compiled C caller can link and run

## Non-Goals

P2-D does not include:

- redesigning the stable Rust SDK surface from P2-C
- exposing internal crates directly through FFI
- adding async ABI entry points
- adding writable VFS behavior
- replacing JSON-based complex outputs with many hand-written ABI mirror structs when JSON is sufficient to preserve stability
