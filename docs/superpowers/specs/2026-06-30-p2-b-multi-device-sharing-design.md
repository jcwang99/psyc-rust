# P2-B Multi-Device Keys And Repository Sharing Design

## Goal

Complete P2-B by adding multi-device repository access, owner-admin controlled repository sharing, epoch-based future-access revocation, keyring reconciliation, and rollback-safe remote publication without breaking the existing encrypted snapshot and sync model.

## Current State

The repository already has a solid single-user keyring and sync foundation:

- `e2v-core::keyring` stores versioned keyring generations and supports local password unlock.
- `e2v-core::facade` already supports atomic local keyring generation updates with a lock and recovery journal.
- `e2v-sync::push` and `e2v-sync::fetch` already publish and fetch retained keyring generation files.
- `e2v-sync::trusted_state` already tracks high-water generations and rejects remote rollback.
- Objects and control records already carry a `key_epoch` field in their authenticated envelope format.

The main P2-B gaps are:

- only a single password envelope exists
- there is no device or member identity model
- there is no repository sharing workflow
- revocation does not rotate future-access keys
- object decryption still relies on the current active epoch instead of the recorded object epoch
- remote keyring publication has no true compare-and-swap reconciliation surface

## Constraints From plan.md

P2-B must satisfy these rules:

- multi-device lands before or together with repository sharing
- revocation means envelope removal plus `active_epoch` advance plus future objects using the new epoch
- P2 revocation blocks future access only; historical strong revocation is deferred to P3 rewrite/re-encryption
- keyring updates must use local lock + journal + atomic publish and keep recent generations
- remote publication must retain prior generations
- after keyring CAS failure, clients must fetch latest state, reconcile, and retry
- revocation/removal beats add/renew/privilege upgrade
- irreconcilable conflicts must fail with an explicit manual-resolution error
- rollback protection must treat lower remote keyring/ref/layout generations as `CRITICAL_ROLLBACK_DETECTED`
- sharing scope for this slice is repository-wide only

## Approaches Considered

### 1. Extend the existing password-only envelope in place

Pros:

- smallest schema diff
- quickest path for local password rotation

Cons:

- no clean way to represent devices and members
- revocation semantics stay bolted on
- remote reconciliation becomes ambiguous because the actor model is missing

### 2. Introduce a unified recipient + epoch-key keyring model

Pros:

- supports password recovery, device access, and share access with one structure
- matches the required revocation model
- gives reconciliation explicit records to merge
- lets existing object formats continue working with recorded `key_epoch`

Cons:

- larger schema evolution
- requires refactoring `RepoSecrets` and decryption paths

### 3. Add online identity/accounts now

Pros:

- cleaner long-term user experience

Cons:

- out of scope for repository-local P2-B
- introduces infrastructure and trust assumptions not present elsewhere in the system

## Recommended Direction

Use approach 2.

Add a unified keyring state that separates:

- stable repository keys that do not rotate during P2 revocation
- per-epoch read keys that gate future object and control-plane access
- recipient records that carry per-device or per-share access to selected epochs

Sharing remains repository-wide and offline. Owners/admins generate invitation bundles out of band. Recipients import them locally and become full repository writers, but they cannot delegate further access.

## Architecture

### 1. Key Model

P2-B keeps two classes of secrets:

- stable repository keys
  - `repo_dedup_key`
  - `repo_ref_key`
  - `repo_path_index_key`
- epoch-scoped read keys
  - `epoch_manifest_enc_key`
  - `epoch_nonce_key`

The stable keys preserve current dedup and branch-token behavior across revocations. The epoch-scoped keys control future object and control-record readability. When revoking a device or member, the system advances `active_epoch` and generates a fresh epoch key pair for the next epoch. New objects and new encrypted control records use the new epoch keys. Old objects remain readable by principals that still hold the old epoch keys, which is the intended P2 behavior.

### 2. Repository Secrets Shape

`RepoSecrets` evolves from “one active bundle” to “stable keys + known epoch keys”:

- `repo_id`
- `active_epoch`
- `repo_dedup_key`
- `repo_ref_key`
- `repo_path_index_key`
- `epoch_keys: BTreeMap<u32, EpochSecrets>`

`EpochSecrets` contains:

- `manifest_enc_key`
- `nonce_key`

Code that encrypts new objects or refs uses `active_epoch`. Code that decrypts existing objects or refs must select keys by the authenticated `key_epoch` stored in the envelope, not by the current active epoch.

This is the minimum change that makes “future revocation only” real on top of the current object format.

### 3. Keyring Schema

`KeyringState` grows into an explicit authorization document:

- `format_version`
- `generation`
- `repo_id`
- `active_epoch`
- `crypto_suite`
- `kdf`
- `actors: Vec<ActorRecord>`
- `devices: Vec<DeviceRecord>`
- `epochs: Vec<EpochDescriptor>`
- `recipient_envelopes: Vec<RecipientEnvelope>`

`ActorRecord`:

- `actor_id`
- `display_name`
- `role`

Roles:

- `owner_admin`
- `writer_member`

`DeviceRecord`:

- `device_id`
- `actor_id`
- `label`
- `device_pubkey_hex`
- `status`

Statuses:

- `active`
- `revoked`

`EpochDescriptor`:

- `epoch`
- `status`

Statuses:

- `active`
- `retired`

`RecipientEnvelope` variants:

- `PasswordRecovery`
  - password-derived envelope used for password unlock and recovery
- `DeviceAccess`
  - device public key recipient for a concrete device
- `ShareInvite`
  - one-time offline invitation envelope for a not-yet-imported member device

Each envelope decrypts an `AccessibleKeyBundle`:

- stable keys
- `epoch_keys: Vec<(epoch, EpochSecrets)>`
- `granted_role`
- `actor_id`
- `device_id`

Top-level keyring JSON never stores raw stable keys or raw epoch keys in plaintext.

### 4. Authorization Model

Authorization is repository-wide for the first version.

Owners/admins can:

- list actors and devices
- issue device invites
- issue member invites
- revoke devices
- revoke members
- reconcile conflicting keyring updates

Writer members can:

- unlock
- fetch
- clone
- commit
- push

Writer members cannot:

- create invites
- change roles
- revoke others

There is no path ACL, branch ACL, or read-only member in P2-B.

### 5. Invite And Acceptance Workflow

The first version uses offline invitation bundles.

#### Add a new device for an existing owner/admin

1. Existing unlocked owner/admin device generates a new X25519 keypair locally.
2. The current device creates an invite bundle containing:
   - target actor identity
   - target device metadata
   - one or more recipient envelopes granting the active epochs that the new device should read
   - repository identifier and replay guards
3. The bundle is transferred out of band.
4. The receiving device imports it, writes a new local keyring generation, and can then fetch/clone/unlock using its device key.

#### Add a new shared member

1. Owner/admin creates a new actor with role `writer_member`.
2. Owner/admin generates a `ShareInvite` bundle for the recipient’s first device.
3. Recipient imports the bundle, materializes its local device identity, converts the invite into a durable `DeviceAccess` recipient envelope, and publishes a new keyring generation.

The invite bundle is repository-scoped and one-shot. It is not a standing capability store.

### 6. Revocation Semantics

Device revocation:

- mark device revoked
- remove its durable recipient envelope
- generate a new epoch
- distribute the new active epoch only to retained active recipients

Member revocation:

- revoke all active devices belonging to that actor
- remove their durable recipient envelopes
- generate a new epoch
- distribute the new active epoch only to retained active recipients

This blocks future access because all future encrypted objects and control-plane records move to the new epoch keys. Historical objects remain readable to anyone who already possesses historical epoch keys, which matches the approved P2 scope.

### 7. Object And Control-Plane Crypto Changes

Current object envelopes and control-record envelopes already record `key_epoch`. P2-B changes the decryption path so that:

- encryption uses keys for `active_epoch`
- decryption parses the envelope first, reads `key_epoch`, and then selects the matching epoch secrets
- if the epoch is unknown to the unlocked recipient, decryption fails explicitly

This applies to:

- object decryption in `e2v-store::DirectLayoutObjectStore`
- control-record decryption in `e2v-core::facade`
- sync-side decode helpers in `e2v-core::sync_support`

Without this change, revocation would only bump metadata and would not actually preserve old-object readability after rotation.

### 8. Local Keyring Update Protocol

The existing local protocol stays and is extended:

- acquire local keyring lock
- write recovery journal
- write retained generation file
- atomically publish local pointer
- remove journal

Additional local rules:

- new generations never overwrite prior files
- a bounded recent generation history is retained
- local reconciliation always starts from the current pointer generation on disk

### 9. Remote Publication And Reconciliation

P2-B introduces a remote keyring pointer ref token, for example `keyring/<repo-id>` or another repository-stable control token derived from `repo_ref_key`.

Remote publication becomes:

1. upload new retained keyring generation files under `control/keyring/`
2. publish keyring pointer bytes through ref CAS using the keyring pointer ref
3. after successful CAS, mirror the same pointer bytes to `control/keyring/keyring.current` for compatibility and inspection

Why this is needed:

- the existing raw pointer file has no compare-and-swap semantics
- P2-B requires retryable reconciliation after concurrent updates
- the ref store already provides a versioned CAS surface

Reconciliation policy:

- fetch latest remote keyring state after CAS failure
- merge local pending change with remote latest
- retry only if merge is deterministic

Precedence rules:

- revoke device beats add device
- revoke member beats add member
- remove envelope beats renew envelope
- role downgrade beats role upgrade

Irreconcilable examples that must stop with explicit error:

- simultaneous conflicting display-name or label rewrites for the same logical record
- simultaneous actor removal and actor reassignment
- incompatible device ownership changes

### 10. Rollback Protection

The existing trusted-state mechanism remains authoritative and now treats the remote keyring pointer ref generation and mirrored pointer generation as the same logical keyring generation floor.

Rules:

- local client stores highest seen layout, keyring, and branch ref generations outside the repository
- lower remote layout generation fails
- lower remote keyring generation fails
- lower remote branch ref generation fails
- failure uses `CRITICAL_ROLLBACK_DETECTED`

No P2-B admin operation is allowed to bypass this guard implicitly.

### 11. CLI Surface

Add a new `share` command group in `e2v-cli`:

- `e2v share list --repo <repo>`
- `e2v share invite-device --repo <repo> --actor <actor-id> --label <device-label> --out <bundle>`
- `e2v share accept-device --repo <repo> --bundle <bundle> --label <local-label>`
- `e2v share invite-member --repo <repo> --name <display-name> --out <bundle>`
- `e2v share accept-member --repo <repo> --bundle <bundle> --label <local-label>`
- `e2v share revoke-device --repo <repo> --device <device-id>`
- `e2v share revoke-member --repo <repo> --actor <actor-id>`

For this slice:

- the repository still supports password unlock for owner recovery
- device/member unlock is primarily consumed by fetch/clone and later local unlock flows through imported bundles
- CLI owns argument parsing and bundle file I/O only
- authorization, reconciliation, and keyring mutation stay in `e2v-core`

### 12. Compatibility Rules

The first P2-B format version can reject pre-P2-B keyring JSON unless upgraded in place during first mutating operation. The chosen direction is:

- reading an old password-only keyring is supported
- the first share/device mutation upgrades it to the new schema in a new generation
- plain password rotation also upgrades the schema lazily if needed

This keeps existing repositories usable while avoiding a mandatory migration command before P2-B work.

## Testing Strategy

Use TDD in narrow slices:

1. keyring schema upgrade and password compatibility
2. epoch-keyed object/control decryption
3. device invite and acceptance
4. member invite and acceptance
5. revocation with epoch advance and future-access denial
6. remote keyring CAS and reconciliation
7. rollback floor enforcement
8. CLI share command routing

Required evidence:

- after epoch rotation, old snapshots remain readable to retained recipients that hold old epoch keys
- after revocation, future objects and future refs are not decryptable by the revoked device/member
- concurrent keyring publication reconciles deterministic add/add cases
- conflicting revoke/add or incompatible ownership changes stop with explicit merge errors
- lower remote keyring generation triggers `CRITICAL_ROLLBACK_DETECTED`
- offline invite bundles can create usable writer members and devices without server-side identity infrastructure

## Non-Goals For This Slice

- historical strong revocation through repository-wide rewrite
- branch-level or path-level sharing
- read-only members
- online account systems or hosted identity
- automatic network delivery of invitations
- writable VFS changes
