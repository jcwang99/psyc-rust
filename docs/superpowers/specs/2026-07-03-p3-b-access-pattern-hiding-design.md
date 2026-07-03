# P3-B Access Pattern Hiding Design

## Goal

Complete the P3-B slice by introducing an ORAM-style remote storage layout that hides stable physical access patterns through padded access schedules, traffic shaping, cost-aware policy control, and an authenticated oblivious layout root, while preserving the existing logical object model, manifest graph, and snapshot DAG.

## Current State

The repository already has several boundaries that P3-B can extend instead of replacing:

- `crates/e2v-store/src/storage_layout.rs` defines `StorageLayout`, but today it only resolves a logical location into one `PhysicalObjectRef`.
- `crates/e2v-store/src/layout.rs` defines `LayoutRoot`, but it currently only stores `schema_version`, `layout_id`, `generation`, and `mapping_policy`.
- `crates/e2v-store/src/capability.rs` already carries `supports_oblivious_access_schedule`, but every current backend reports `false` and no caller enforces or uses it.
- `crates/e2v-sync/src/pack_index.rs`, `crates/e2v-sync/src/publisher.rs`, and `crates/e2v-sync/src/remote_maintenance.rs` already prove that encrypted layout metadata, generationed layout publication, retained layout history, rewrite-style publication, and generation-aware cleanup can be expressed through the current sync boundary.
- `crates/e2v-core` still treats logical object identity, manifests, snapshots, and branches as stable logical concepts independent from remote physical placement.

The main gaps against P3-B are:

- there is no authenticated oblivious layout metadata model
- there is no read-plan abstraction that can express real reads, cover reads, and pacing
- there is no generation-scoped randomized remote layout
- there is no policy surface for traffic, cost, or dedup mode
- there is no ORAM-mode publication or reshuffle workflow
- there is no proof today that `fetch`, `verify`, `repair`, `gc`, and future remote logical reads can operate without directly depending on stable physical placement

## Constraints From `plan.md`

P3-B must satisfy these architecture rules:

- `StorageLayout` remains the boundary that maps logical objects to physical storage layout, including P0 direct layout, P1 pack layout, P3 rewrite layout, and P3 ORAM layout.
- P3 rewrite and ORAM layouts may change physical object refs, access scheduling, and remote positions, but must not change the `Object Model`, `ManifestStore`, or `Snapshot DAG`.
- ORAM layout must manage access plans, padding schedules, and reshuffle generation as layout metadata.
- `BlobStore` must not be asked to understand `ObjectId` semantics or ORAM buckets directly.
- `GC` may only reason about physical refs through `StorageLayout`; it must not infer reachability by directly scanning backend prefixes.
- layout root updates must continue to use generation plus CAS or lease publication, and backends without layout-root CAS or lease support must not run ORAM updates.
- P3-B must deliver:
  - ORAM-style storage layout
  - padded access schedule
  - traffic shaping
  - cost model and policy control
  - oblivious layout root
  - ORAM mode disables long-lived physical dedup semantics

This repository is explicitly targeting the latest format only. P3-B does not preserve wire compatibility with pre-P3-B layout-root schema; all tests and tooling may assume the new layout-root format as the authoritative one.

## Approaches Considered

### 1. Keep `StorageLayout` as a simple physical locator and add ORAM behavior around it

Pros:

- smallest immediate edit set
- fewer new types

Cons:

- forces `fetch`, `verify`, `repair`, and `gc` to learn ORAM-specific rules out-of-band
- cannot cleanly represent cover reads, pacing, or policy-driven read amplification
- leaves `LayoutRoot` too weak to authenticate the active ORAM policy

### 2. Upgrade `StorageLayout` into a layout-metadata and read-plan boundary

Pros:

- keeps ORAM logic behind the storage boundary described in `plan.md`
- lets direct, pack, rewrite, and oblivious layouts share one consumer contract
- gives `gc`, `verify`, `repair`, `fetch`, and future remote readers one place to obtain physical read and enumeration plans
- keeps logical object identity stable while allowing randomized physical placement per layout generation

Cons:

- touches the layout API and all callers that currently assume one `PhysicalObjectRef`
- requires a new authenticated layout metadata model

### 3. Implement a full proof-oriented Path ORAM style tree immediately

Pros:

- strongest academic framing
- fewer future conceptual upgrades if the final target is formal ORAM

Cons:

- much larger implementation surface
- significantly higher write amplification and maintenance complexity
- would force a large rewrite of the current pack and layout publication pipeline before proving the rest of the repository can consume the new boundary

## Recommended Direction

Use approach 2.

P3-B should upgrade the storage boundary so that all remote physical access is expressed as a layout-directed plan rather than as a stable physical path lookup. The first delivery is an engineering ORAM-style layout:

- fixed-size encrypted bucket containers
- generation-scoped randomized physical placement
- deterministic cover reads per logical request
- traffic shaping controlled by explicit policy
- cost reporting before enablement or reshuffle
- no long-lived stable physical dedup semantics in ORAM mode

This is intentionally an ORAM-style system rather than a proof-oriented full ORAM tree. It satisfies the plan requirements while preserving the existing logical object model and reusing the current rewrite/publication infrastructure.

## Architecture

### 1. Crate Boundaries

#### `crates/e2v-store`

Owns:

- authenticated layout-root types
- storage-layout traits and read-plan types
- direct, pack, rewrite, and oblivious layout implementations
- capability gating for ORAM-safe backends

Must not:

- embed sync publication policy
- persist operation journals
- perform remote reshuffle orchestration

#### `crates/e2v-sync`

Owns:

- ORAM enablement
- ORAM reshuffle
- remote oblivious index and bucket publication
- journaled recovery and resume
- layout-root publication through `TransactionPublisher`
- generation-aware GC, verify, repair, fetch, and clone integration

Must not:

- bypass the storage-layout read-plan boundary by reconstructing stable physical placement directly

#### `crates/e2v-core`

Continues to own:

- logical object model
- manifests
- snapshots
- refs
- local repository workflows

P3-B must not force `e2v-core` to adopt ORAM bucket semantics or remote path knowledge.

#### `crates/e2v-cli` and `crates/e2v-api`

Expose:

- plan/status/enable/reshuffle user surfaces
- policy configuration
- cost reports

They delegate to `e2v-sync` and must not implement ORAM layout rules themselves.

### 2. New Layout Types

`LayoutRoot` becomes a richer authenticated control-plane record. The root still carries `schema_version`, `layout_id`, and `generation`, but it also gains explicit mode and policy metadata.

The new root shape is:

```rust
pub struct LayoutRoot {
    pub schema_version: u32,
    pub layout_id: String,
    pub generation: u64,
    pub mode: LayoutMode,
    pub mapping_policy: String,
    pub dedup_mode: DedupMode,
    pub oblivious_generation: Option<u64>,
    pub schedule_policy: LayoutSchedulePolicy,
    pub traffic_policy: LayoutTrafficPolicy,
    pub cost_policy: LayoutCostPolicy,
}

pub enum LayoutMode {
    Direct,
    Pack,
    Rewrite,
    Oblivious,
}

pub enum DedupMode {
    StablePhysical,
    GenerationScopedRandomized,
}
```

The root intentionally remains small. Detailed oblivious placement state lives in a separate encrypted control-plane object family, published similarly to pack-index metadata:

- `oblivious/root.json`
- `oblivious/segments/*.json`

The main `layout_root.json` authenticates the active mode and the currently active policy. The oblivious root and segments authenticate the generation-specific physical bucket mapping and reshuffle state.

### 3. StorageLayout Becomes A Read-Plan Boundary

`StorageLayout` can no longer stop at a single `PhysicalObjectRef`. P3-B needs a layout-driven access plan that consumers can execute without learning layout internals.

The new boundary is:

```rust
pub struct LogicalReadRequest<'a> {
    pub object_id: &'a str,
    pub offset: u64,
    pub length: u64,
}

pub struct PhysicalReadOp {
    pub physical_ref: PhysicalObjectRef,
    pub kind: PhysicalReadKind,
}

pub enum PhysicalReadKind {
    Real,
    Cover,
}

pub struct ReadPlan {
    pub layout_id: String,
    pub generation: u64,
    pub operations: Vec<PhysicalReadOp>,
    pub response_window: LogicalResponseWindow,
    pub traffic_hint: TrafficExecutionHint,
}

pub trait StorageLayout {
    fn plan_logical_read(&self, request: LogicalReadRequest<'_>) -> Result<ReadPlan>;
    fn enumerate_reachable_physical_refs(&self) -> Result<Vec<PhysicalObjectRef>>;
}
```

`DirectStorageLayout` and `PackStorageLayout` implement this trivially:

- one real read
- no cover reads
- no pacing beyond a no-op hint

`ObliviousStorageLayout` implements it non-trivially:

- one or more real bucket reads
- deterministic cover bucket reads
- bucket-sized padded access windows
- explicit pacing and burst limits

### 4. ORAM-Style Physical Layout

P3-B uses a fixed-size bucket model instead of stable loose-object or stable pack-object placement.

Each ORAM generation publishes:

- bucket data objects under randomized paths such as `oblivious/data/<generation>/<random>.bin`
- encrypted bucket index segments mapping logical object IDs to bucket members
- an authenticated oblivious root containing generation metadata and policy checksums

Each bucket contains:

- one or more encrypted logical objects or logical object fragments
- fixed-size slot envelopes
- padding up to the configured bucket size

Properties:

- the same logical object may land in different bucket paths across ORAM generations
- repeated reshuffles must be allowed to rewrite the same logical object to a fresh randomized container even if the plaintext is unchanged
- ORAM mode therefore switches `dedup_mode` to `GenerationScopedRandomized`

This preserves logical dedup inside the repository graph while explicitly removing the promise that one logical object corresponds to one long-lived remote physical location.

### 5. Padded Access Schedule

Every logical read in ORAM mode becomes a padded schedule:

- the requested logical object resolves to one or more real bucket reads
- the schedule always executes at least `min_total_reads`
- if the real read count is smaller, the layout injects deterministic cover reads
- each read is rounded to a configured bucket or aligned window size

The schedule policy is explicit:

```rust
pub struct LayoutSchedulePolicy {
    pub bucket_bytes: u32,
    pub min_total_reads: u8,
    pub cover_reads_per_request: u8,
    pub reshuffle_after_generations: u32,
}
```

The cover-read chooser must be deterministic from authenticated local secrets plus the active oblivious generation so that:

- the same repository can reproduce valid schedules after restart
- no remote metadata needs to reveal which reads were real versus cover

### 6. Traffic Shaping

Traffic shaping is a client-side execution contract derived from the authenticated layout policy. It does not require backend-side cooperation beyond the storage capabilities already needed to execute the plan.

The shaping policy is:

```rust
pub struct LayoutTrafficPolicy {
    pub max_parallel_reads: u8,
    pub inter_read_delay_ms: u16,
    pub burst_budget_bytes: u64,
    pub target_request_window_ms: u32,
}
```

The sync/read executor must respect this policy for remote ORAM reads:

- do not exceed the configured burst budget
- do not collapse cover reads away
- do not fuse request windows so aggressively that the configured traffic envelope disappears

Local and in-memory backends may execute the same plan without real sleeping in unit tests, but the plan object must still carry the shaping metadata so behavior remains verifiable.

### 7. Cost Model And Policy Control

P3-B needs explicit user-visible policy control rather than a hidden hard-coded ORAM mode.

The first policy presets are:

```text
interactive
balanced
low-leakage
```

Each preset resolves to a concrete schedule policy plus traffic policy plus dedup mode.

P3-B also exposes a planning report:

```rust
pub struct ObliviousLayoutPlan {
    pub estimated_real_reads_per_request: u8,
    pub estimated_cover_reads_per_request: u8,
    pub estimated_bytes_per_request: u64,
    pub estimated_write_amplification: u8,
    pub requires_layout_root_rewrite: bool,
    pub advisory_messages: Vec<String>,
}
```

The report is used by CLI and SDK before enablement or reshuffle so the user can choose a policy knowingly.

### 8. Backend Capability Gating

`supports_oblivious_access_schedule` becomes a real gate.

A backend may only advertise `true` when it can safely host ORAM mode for this repository implementation. At minimum the backend must already support:

- range reads
- layout-root publication through CAS or safe lease-backed single-writer mode
- transaction markers
- reliable remote time or object generation/etag semantics sufficient for maintenance fencing

The practical initial policy is:

- `MemoryBackend`: true
- `LocalFolderBackend`: true
- S3-compatible remote backend: true when the current capability set already satisfies the gating rules
- WebDAV/Alist backends: remain false until they can prove safe oblivious publication and maintenance behavior

If `supports_oblivious_access_schedule` is false, the new ORAM user surface must refuse enablement and reshuffle instead of silently degrading to pack mode.

### 9. Publication And Recovery

ORAM enablement and reshuffle are both layout rewrites.

They reuse the current P3-A publication model:

1. compute the next oblivious generation locally
2. upload new bucket data objects
3. upload oblivious index segments
4. publish `oblivious/root.json`
5. publish the next `layout_root.json` generation through `TransactionPublisher`
6. retain previous layout generations until GC grace rules allow deletion

Recovery requirements:

- interrupted bucket uploads must resume from journal state
- interrupted oblivious-root publication must not leave a partially switched layout generation
- previous layout generations remain readable until the new generation is fully published
- reshuffle journals must not store plaintext logical object contents or unauthenticated placement metadata

### 10. Read And Maintenance Consumers

The following paths must move to the layout-plan boundary:

- remote logical reads during `fetch`
- pack-backed read repair during `verify` and `repair`
- remote reachability and candidate enumeration during `gc`
- ORAM reshuffle and old-generation cleanup during maintenance

The rule is simple:

- consumers may ask the layout for a read plan or reachable physical refs
- consumers may not reconstruct stable physical paths from object IDs on their own

`e2v-core`, `e2v-vfs`, local web, and SDK local-read flows remain logically unchanged because they still operate on the local repository snapshot model. Future remote-assisted local reads must use the same layout plan boundary instead of bypassing it.

## User Surface

P3-B adds an explicit ORAM-facing workflow.

CLI commands:

```text
e2v oram plan
e2v oram status
e2v oram enable --policy <interactive|balanced|low-leakage>
e2v oram reshuffle --policy <interactive|balanced|low-leakage>
```

Rules:

- `plan` reports the estimated amplification and advisory messages
- `status` reports the current layout mode, oblivious generation, dedup mode, and active policy
- `enable` rewrites the current remote layout into ORAM mode
- `reshuffle` republishes the same logical repository state into a fresh randomized oblivious generation

The SDK and C-ABI follow the same conceptual workflow with plan/status/execute request and response types.

## Testing Strategy

Use TDD in slices that prove the boundary before broad integration.

### 1. Layout root schema and policy coverage

Required evidence:

- layout roots round-trip with explicit `mode`, `dedup_mode`, `schedule_policy`, `traffic_policy`, and `cost_policy`
- old flat roots are no longer silently accepted as if they were ORAM-capable

### 2. Storage-layout read plans

Required evidence:

- direct layout still produces exactly one real read and no cover reads
- pack layout still produces exactly one real read and no cover reads
- oblivious layout produces bucket-sized read plans with deterministic cover reads and shaping hints

### 3. ORAM publication

Required evidence:

- enabling ORAM publishes `oblivious/root.json` and encrypted segment metadata
- layout publication advances the layout generation
- ORAM mode records `dedup_mode = GenerationScopedRandomized`

### 4. Stable physical dedup is disabled

Required evidence:

- the same logical object can resolve to different randomized bucket paths after reshuffle
- reshuffle does not require a logical object ID change to achieve fresh remote physical placement

### 5. Maintenance and consumer routing

Required evidence:

- `fetch`, `verify`, `repair`, and `gc` consume layout plans or layout enumeration instead of direct object-path assembly
- retained old ORAM generations remain reachable until grace-period deletion is allowed

### 6. Capability gating

Required evidence:

- ORAM enablement is rejected on backends with `supports_oblivious_access_schedule = false`
- ORAM enablement is accepted on test backends that satisfy the gate

### 7. Cost reporting and user surface

Required evidence:

- CLI and SDK planning/status flows expose policy, amplification, and advisory messages
- execute paths refuse unsupported backends or contradictory policies explicitly

## Non-Goals For This Slice

- a formal cryptographic proof of obliviousness against an unbounded adversary
- writable VFS changes
- changing the logical object model, manifest schema, or snapshot DAG
- preserving compatibility with pre-P3-B layout-root schema
- teaching `BlobStore` to interpret logical object IDs or ORAM member structure
