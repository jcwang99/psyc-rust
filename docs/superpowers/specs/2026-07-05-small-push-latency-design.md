# Small-Push Latency Design

## Goal

Reduce small-push latency on high-latency remotes such as AList/WebDAV without weakening the existing publish safety model or regressing local and S3-oriented workflows.

## Current State

The repository already has two recent latency improvements in place:

- fresh heartbeat renewals are throttled instead of being forced after every upload loop step
- fresh pre-commit verification skips unnecessary remote time probes

Those changes improved the live AList diagnostic run from:

- `184442 ms / 151 requests`

to:

- `131512 ms / 103 requests`

The remaining bottleneck is still request shaping during small pushes.

Current upload behavior is decided in `crates/e2v-sync/src/push.rs` by `should_pack_small_objects(...)`, which uses:

- total reachable object count for fresh push
- total tracked journal object count for resume

with a default threshold of `100000`.

That rule works for very large repositories, but it misses the user-visible latency case we just measured:

- a small commit can still expand into several encrypted repository objects
- those objects remain below `SMALL_OBJECT_MAX_BYTES`
- the repository is far below `100000` total objects
- the push still takes the loose-object path, producing avoidable remote round trips on AList/WebDAV

In other words, the current heuristic is tied to repository size, not to the actual shape of the upload that is about to happen.

## Approaches Considered

### 1. Lower the global small-object pack threshold

Pros:

- smallest code change
- improves more cases immediately

Cons:

- changes behavior for every backend, including low-latency local and S3-like paths
- risks turning trivial one-object uploads into unnecessary pack/index work
- does not distinguish between repository size and upload shape

### 2. Decide packing from the current upload set

Pros:

- directly targets the measured AList/WebDAV pain point
- keeps the existing large-repository threshold as a compatibility guard
- can be applied consistently to fresh push and resume
- reduces request count without changing publish safety semantics

Cons:

- needs a new shared decision helper and more explicit tests
- requires careful alignment across push and resume branches

### 3. Add parallel loose-object uploads

Pros:

- could improve latency further after request-count reduction

Cons:

- much higher regression risk on conservative WebDAV backends
- interacts with retry, ordering, journal semantics, and single-writer safety
- solves transport overlap before fixing the more obvious request-shaping mistake

## Recommended Direction

Use approach 2.

The next optimization slice should make the pack decision depend on the current missing upload set rather than on the total repository size alone. This keeps the existing protocol intact while letting small but multi-object pushes use the already-tested pack path.

## Design

### 1. Shared upload packing decision

Add a shared decision helper in the push path that answers one question:

- should this upload set use small-object packing?

The helper should be used by:

- fresh push
- journal-driven resume
- resume fallback when there is no persisted object journal state

This removes the current split where fresh push and resume each derive `pack_enabled` from different totals.

### 2. Adaptive small-push rule

Keep the existing large-repository rule, but add a current-upload fast path.

Packing should be enabled when either of these is true:

1. the current upload set contains at least a small configured number of pack-eligible missing objects
2. the existing large-repository threshold is reached

For this slice, "pack-eligible" means:

- the object is currently missing on the remote
- the local object envelope size is `<= SMALL_OBJECT_MAX_BYTES`

The fast-path threshold should default to `2`.

That gives the intended behavior:

- `0` missing objects: no upload, no pack
- `1` pack-eligible missing object: keep the loose-object path by default
- `2+` pack-eligible missing objects: prefer the pack path even in a small repository
- very large repositories: continue to use the existing threshold behavior

This matches the observed AList case, where a tiny working-tree change still fans out into multiple small encrypted objects.

### 3. Keep safety and object semantics unchanged

This slice must not change:

- transaction markers
- journal semantics
- layout-root publication
- ref publication
- large-object handling
- ORAM segment-only upload behavior

Only the decision of whether small missing objects travel as loose objects or as pack payloads should change.

Resume must remain fully compatible with fresh push:

- objects already recorded as uploaded or verified still follow the current journal logic
- missing objects selected during resume use the same adaptive packing rule as fresh push
- fallback resume without journal object rows must not silently use a different heuristic

### 4. Test seams

Keep the existing test-only override for the large pack threshold and add an equivalent test seam for the small-push fast-path threshold if needed.

The seam stays test-only. It must not become a new public runtime tuning surface in this slice.

## Error Handling

- If the adaptive rule selects packing and pack upload fails, the existing journal and resume path remain authoritative.
- If an object is larger than `SMALL_OBJECT_MAX_BYTES`, it must continue to use the loose-object path even when packing is otherwise enabled.
- If the current upload set is empty, the helper must short-circuit cleanly without creating pack metadata.
- If resume has already published some pack segments for the current operation, the new heuristic must still preserve resumability and avoid changing object verification semantics.

## Testing Strategy

Use TDD in small slices.

Required automated coverage:

1. decision tests proving the fast-path threshold triggers for a small current upload set even when the repository is far below `100000`
2. decision tests proving a single missing small object still stays loose by default
3. compatibility tests proving the existing large threshold still enables packing
4. push tests proving a small repository push can publish pack data and pack index segments because of the missing-object heuristic
5. resume tests proving the same heuristic applies to journal-driven missing objects

Required verification after implementation:

- targeted sync tests for the new decision helper and push/resume behavior
- `cargo test --workspace`
- a fresh live AList diagnostic run against the same small-push scenario, comparing total elapsed time plus request counts with the `131512 ms / 103 requests` baseline

## Non-Goals

- adding parallel loose-object uploads
- introducing backend-specific transport tuning knobs
- changing pack index format or layout-root structure
- changing the logical repository object model
