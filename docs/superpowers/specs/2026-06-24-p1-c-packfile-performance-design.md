# P1-C Packfile And Performance Design

## Goal

Complete the P1-C storage-performance slice by evolving the existing small-object pack path into a unified pack-oriented remote object path with:

- immutable per-pack indexes
- a published pack-index root
- a persistent local pack-index cache
- bounded L0 safety compaction
- pack-backed range-read reuse
- a runnable benchmark harness for the critical read/write paths

The design keeps the current repository object model unchanged and treats pack/index as a storage-layout concern.

## Current State

The repository already has:

- loose-object storage behind `DirectLayoutObjectStore`
- remote small-object packing in `crates/e2v-sync/src/pack.rs`
- packed fetch/range-read reuse in `crates/e2v-sync/src/fetch.rs`
- packed push/resume behavior in `crates/e2v-sync/src/push.rs`
- tests proving packed upload and packed range-read reuse

The main gaps against P1-C are:

- pack discovery still depends on remote inventory state instead of a published index root
- there is no persistent local pack-index cache
- there is no bounded L0 compaction guard
- pack/index state is not modeled as a `StorageLayout` concern
- there is no runnable benchmark harness

## Approaches Considered

### 1. Replace the current pack path with a brand-new pack subsystem

Pros:

- cleanest naming and abstractions
- easiest long-term separation between direct/pack/rewrite layouts

Cons:

- large rewrite across sync, fetch, and object-store code
- high regression risk because current pack behavior is already heavily tested

### 2. Treat the current pack path as the first pack implementation

Pros:

- reuses working upload and range-read behavior
- allows P1-C to land incrementally behind new index-root/cache/layout boundaries
- smaller TDD slices

Cons:

- some naming cleanup still needs follow-through across tests and docs
- direct and pack responsibilities must be separated carefully

### 3. Add only a benchmark harness and defer the rest

Pros:

- smallest immediate change

Cons:

- does not satisfy the P1-C storage requirements
- benchmark results would not represent the intended architecture

## Recommended Direction

Use approach 2.

The current pack payload and immutable per-pack JSON index become the initial pack data path and per-pack index path. P1-C then adds a published pack-index root, local cache, compaction, and layout-facing resolution on top of that working base.

## Architecture

### 1. Pack Index Root

Add a remote `pack-index/root.json` control object that contains:

- schema version
- `layout_id`
- `layout_generation`
- root generation
- ordered list of immutable segment paths

In P1-C, the segment list may contain:

- immutable per-pack index files under `packs/index/`
- compacted aggregate segments under `pack-index/segments/`

The root is the authoritative entrypoint for pack lookup. Fetch and push must stop discovering pack segments by remote prefix scan when the root is present.

### 2. Local Pack-Index Cache

Add a persistent cache under `.e2v/cache/pack-index/`:

- `root.json`
- cached segment payloads

Lookup flow:

1. read remote pack-index root
2. compare it with cached root
3. fetch only missing segments
4. update the local cache
5. resolve object ids to `PhysicalObjectRef`

If the remote root is absent, the code treats remote pack inventory as empty and continues to support direct loose-object repositories without segment listing fallback.

### 3. Bounded L0 Safety Compaction

Treat each newly uploaded per-pack index as an L0 segment. When the root segment count reaches a configured maximum, push must compact before publishing the next root.

P1-C compaction rules:

- compaction creates a new immutable aggregate segment
- old segments are not modified in place
- the new root is published only after the compacted segment is readable
- on failure, the old root stays authoritative

This is a safety compaction, not a background optimization system.

### 4. StorageLayout And Resolution

Introduce storage-layout-facing pack resolution in `e2v-store` without rewriting the whole object-store stack.

P1-C target boundary:

- upper layers continue to address objects by `ObjectId`
- pack lookup resolves to `PhysicalObjectRef`
- direct layout remains available
- pack resolution logic stops leaking remote path rules into higher-level sync logic

The first step is a minimal `StorageLayout` abstraction plus pack-index aware helpers. Direct loose-object behavior remains the default for local repositories.

### 5. Benchmark Harness

Add a runnable benchmark harness that covers the critical P1-C paths:

- pack index cache warmup
- cached fetch/clone object restoration from packed data
- packed push path for many small objects
- bounded-L0 compaction trigger path

P1-C does not require a hard performance threshold yet. The requirement is a reproducible harness that can run locally and exercise the intended path.

## Data Flow

### Push

1. refresh remote layout root and pack-index root view
2. determine missing loose objects and pack-covered objects
3. upload new pack data and immutable per-pack indexes
4. compact L0 if the segment bound would be exceeded
5. publish the next pack-index root
6. continue through the existing transaction/ref publish flow

### Fetch / Clone

1. read remote pack-index root
2. update `.e2v/cache/pack-index`
3. resolve needed object ids through cached segments
4. read pack-backed ranges with in-process pack-byte reuse
5. materialize only the required object envelopes locally

## Error Handling

- Invalid pack-index root: fail fetch/push with an explicit root validation error.
- Invalid cached segment: discard the local cached copy and re-fetch the remote segment once.
- Missing referenced segment: fail fetch/push instead of silently falling back to blind listing.
- Compaction publish race: discard the new compacted output and retry from a fresh root view.
- Missing remote root: treat pack inventory as empty and do not fall back to segment prefix listing.

## Testing Strategy

Use TDD in small slices:

1. root + cache tests
2. compaction trigger tests
3. layout-facing resolution tests
4. benchmark harness smoke test

Required evidence:

- fetch can reuse cached pack-index segments without re-listing remote pack indexes
- push refreshes the root view before using cached knowledge
- compaction reduces published L0 segment count when the bound is reached
- packed object reads still reuse remote range reads efficiently
- benchmark binary runs successfully and exercises the critical paths

## Non-Goals For This Slice

- full rewrite/oram layout
- background or distributed compaction scheduling
- changing the logical repository object model
- rewriting unrelated subsystems beyond the pack/index boundary
