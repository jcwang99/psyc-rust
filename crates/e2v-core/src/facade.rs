use std::cell::Cell;
use std::fs;
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{ensure, Context, Result};
use blake3::Hasher;
use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{Tag, XChaCha20Poly1305, XNonce};
use e2v_store::{
    validate_object_id_value, validate_ref_token_value, DirectLayoutObjectStore, EpochSecrets,
    LayoutRoot, RepoSecrets,
};
use postcard::{from_bytes as postcard_from_bytes, to_stdvec as postcard_to_vec};
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::chunker::FastCdcChunker;
use crate::keyring::{
    cache_unlocked_password, cache_unlocked_secrets, generate_local_device_credential,
    open_repo_secrets, read_current_keyring_state, read_local_device_credential, seal_repo_secrets,
    seal_repo_secrets_for_device, unlock_repo_secrets, unlock_repo_secrets_from_generation_file,
    unlock_repo_secrets_from_keyring_bytes_with_local_device, unlock_repo_secrets_uncached,
    unlock_repo_secrets_with_local_device, write_local_device_credential, KeyringPointer,
    KeyringState, KEYRING_CURRENT_FILE, KEYRING_DIR,
};
use crate::local_index::{FilenameSearchResult, MetadataSearchQuery, MetadataSearchResult};
use crate::manifest_store::{ManifestStore as LocalManifestStore, ManifestStoreApi};
use crate::working_tree::{SnapshotReader, StableReadPolicy, WorkingTree, WorkingTreeEntry};

const CONTROL_DIR: &str = ".e2v";
const LAYOUT_ROOT_FILE: &str = "layout_root.json";
const DEFAULT_REF_FILE: &str = "refs/default.json";
const BRANCH_REFS_DIR: &str = "refs/branches";
const OBJECTS_DIR: &str = "objects";
const JOURNAL_DIR: &str = "journal";
const DIRECT_LAYOUT_ID: &str = "direct";
const DIRECT_MAPPING_POLICY: &str = "loose";
const REPO_FORMAT_VERSION: u32 = 1;
const DEFAULT_ACTIVE_EPOCH: u32 = 1;
const DEFAULT_REF_TOKEN: &str = "default";
const CONTROL_REF_MAGIC: &[u8; 4] = b"E2RF";
const CONTROL_REF_FORMAT_VERSION: u32 = 1;
#[allow(dead_code)]
const RESERVED_MANIFEST_TYPES: &[&str] = &["directory_root", "tree_shard", "file_shard"];
const MAX_TREE_ENTRIES_PER_OBJECT: usize = 4096;
const CHECKOUT_STREAM_CHUNK_BYTES: usize = 256 * 1024;
const DEFAULT_MAX_FILE_CHUNKS_PER_OBJECT: usize = 8192;
thread_local! {
    static MAX_FILE_CHUNKS_PER_OBJECT_OVERRIDE: Cell<Option<usize>> = const { Cell::new(None) };
}

pub fn override_max_file_chunks_per_object_for_test(
    max_chunks: usize,
) -> MaxFileChunksPerObjectGuard {
    let previous = MAX_FILE_CHUNKS_PER_OBJECT_OVERRIDE.with(|cell| {
        let previous = cell.get();
        cell.set(Some(max_chunks));
        previous
    });
    MaxFileChunksPerObjectGuard { previous }
}

pub struct MaxFileChunksPerObjectGuard {
    previous: Option<usize>,
}

impl Drop for MaxFileChunksPerObjectGuard {
    fn drop(&mut self) {
        MAX_FILE_CHUNKS_PER_OBJECT_OVERRIDE.with(|cell| {
            cell.set(self.previous);
        });
    }
}

fn max_file_chunks_per_object() -> usize {
    MAX_FILE_CHUNKS_PER_OBJECT_OVERRIDE
        .with(|cell| cell.get().unwrap_or(DEFAULT_MAX_FILE_CHUNKS_PER_OBJECT))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitOptions {
    pub repo_root: PathBuf,
    pub password: String,
    pub branch_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitOptions {
    pub repo_root: PathBuf,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutOptions {
    pub repo_root: PathBuf,
    pub snapshot_id: String,
    pub target_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitResult {
    pub snapshot_id: String,
    pub committed_files: usize,
    pub new_bytes: u64,
    pub reused_bytes: u64,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotSummary {
    pub snapshot_id: String,
    pub message: String,
    pub parent_snapshot_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryEntry {
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotHandle {
    pub snapshot_id: String,
    pub layout_generation: u64,
    root_tree_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileHandle {
    pub snapshot_id: String,
    pub file_object_id: String,
    file_size: u64,
    chunk_count: usize,
    chunk_ids: Vec<String>,
    chunk_lengths: Vec<u64>,
    shard_ids: Vec<String>,
    shard_byte_lengths: Vec<u64>,
    layout_generation: u64,
    crypto_suite: String,
    key_epoch: u32,
    chunker_id: String,
}

impl FileHandle {
    pub fn chunk_count(&self) -> usize {
        self.chunk_count
    }

    pub fn file_size(&self) -> u64 {
        self.file_size
    }

    pub fn layout_generation(&self) -> u64 {
        self.layout_generation
    }

    pub fn crypto_suite(&self) -> &str {
        &self.crypto_suite
    }

    pub fn key_epoch(&self) -> u32 {
        self.key_epoch
    }

    pub fn chunker_id(&self) -> &str {
        &self.chunker_id
    }

    pub fn debug_chunk_ids(&self) -> &[String] {
        &self.chunk_ids
    }

    pub fn debug_chunk_lengths(&self) -> &[u64] {
        &self.chunk_lengths
    }
}

#[derive(Debug, Clone)]
pub struct ReadService {
    repo_root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryState {
    pub repo_root: PathBuf,
    pub branch: BranchState,
    pub layout_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchState {
    pub name: String,
    pub token_hex: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchSummary {
    pub name: String,
    pub token_hex: String,
    pub head_snapshot_id: Option<String>,
    pub is_current: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareActorSummary {
    pub actor_id: String,
    pub display_name: String,
    pub role: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareDeviceSummary {
    pub device_id: String,
    pub actor_id: String,
    pub label: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareListResult {
    pub actors: Vec<ShareActorSummary>,
    pub devices: Vec<ShareDeviceSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareInviteMemberOptions {
    pub display_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareAcceptMemberOptions {
    pub invite_bytes: Vec<u8>,
    pub local_device_label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareInviteDeviceOptions {
    pub actor_id: String,
    pub device_label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareAcceptDeviceOptions {
    pub invite_bytes: Vec<u8>,
    pub local_device_label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareRevokeDeviceOptions {
    pub device_id: String,
    pub password: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareRevokeMemberOptions {
    pub actor_id: String,
    pub password: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareInviteBundle {
    pub actor_id: String,
    pub device_id: String,
    pub bundle_bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareAcceptResult {
    pub actor_id: String,
    pub device_id: String,
    pub role: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ShareInvitePayload {
    repo_id: String,
    actor_id: String,
    device_id: String,
    display_name: String,
    role: String,
    invite_kind: String,
    active_epoch: u32,
    branch_name: String,
    branch_token_hex: String,
    default_ref_bytes_hex: String,
    layout_root_bytes_hex: String,
    bootstrap_credential: crate::keyring::LocalDeviceCredential,
    bootstrap_keyring_state: KeyringState,
    bootstrap_keyring_pointer: KeyringPointer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RefRecord {
    pub branch_name: String,
    pub ref_token_hex: String,
    pub head_snapshot_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct EncryptedControlRecord {
    magic: [u8; 4],
    format_version: u32,
    object_type: String,
    crypto_suite: String,
    key_epoch: u32,
    object_id: String,
    nonce: Vec<u8>,
    ciphertext: Vec<u8>,
    auth_tag: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ChunkObject {
    pub plaintext_length: usize,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FileObject {
    pub schema_version: u32,
    pub entry_name: String,
    pub file_size: u64,
    pub modified_unix_ms: u64,
    pub chunker_id: String,
    pub chunker_config_id: String,
    pub chunk_count: u64,
    pub chunks: Vec<String>,
    pub chunk_lengths: Vec<u64>,
    pub shard_ids: Vec<String>,
    pub shard_byte_lengths: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FileShardObject {
    pub schema_version: u32,
    pub chunks: Vec<String>,
    pub chunk_lengths: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TreeEntry {
    pub name: String,
    pub kind: String,
    pub object_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TreeObject {
    pub schema_version: u32,
    pub entries: Vec<TreeEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(dead_code)]
struct DirectoryRootObject {
    pub schema_version: u32,
    pub fanout: u32,
    pub shards: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(dead_code)]
struct TreeShardObject {
    pub schema_version: u32,
    pub range_start: String,
    pub range_end: String,
    pub entries: Vec<TreeEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SnapshotObject {
    pub schema_version: u32,
    pub message: String,
    pub root_tree_id: String,
    pub parent_snapshot_id: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct RepositoryFacade {
    snapshot_reader: Option<Arc<dyn SnapshotReader>>,
    stable_read_policy: Option<StableReadPolicy>,
}

impl RepositoryFacade {
    pub fn new() -> Self {
        Self {
            snapshot_reader: None,
            stable_read_policy: None,
        }
    }

    pub fn with_snapshot_reader(snapshot_reader: Arc<dyn SnapshotReader>) -> Self {
        Self {
            snapshot_reader: Some(snapshot_reader),
            stable_read_policy: None,
        }
    }

    pub fn with_stable_read_policy(stable_read_policy: StableReadPolicy) -> Self {
        Self {
            snapshot_reader: None,
            stable_read_policy: Some(stable_read_policy),
        }
    }

    pub fn with_snapshot_reader_and_policy(
        snapshot_reader: Arc<dyn SnapshotReader>,
        stable_read_policy: StableReadPolicy,
    ) -> Self {
        Self {
            snapshot_reader: Some(snapshot_reader),
            stable_read_policy: Some(stable_read_policy),
        }
    }

    pub fn init(&self, options: InitOptions) -> Result<RepositoryState> {
        ensure!(
            !options.password.is_empty(),
            "repository password must not be empty"
        );
        ensure!(
            !options.branch_name.trim().is_empty(),
            "branch name must not be empty"
        );

        let repo_root = options.repo_root;
        fs::create_dir_all(&repo_root)
            .with_context(|| format!("failed to create repo root at {}", repo_root.display()))?;

        ensure!(
            directory_is_empty(&repo_root)?,
            "repository root must be empty before init"
        );

        let control_dir = repo_root.join(CONTROL_DIR);
        fs::create_dir_all(control_dir.join(OBJECTS_DIR)).with_context(|| {
            format!(
                "failed to create objects directory at {}",
                control_dir.display()
            )
        })?;
        fs::create_dir_all(control_dir.join(JOURNAL_DIR)).with_context(|| {
            format!(
                "failed to create journal directory at {}",
                control_dir.display()
            )
        })?;
        fs::create_dir_all(control_dir.join("refs")).with_context(|| {
            format!(
                "failed to create refs directory at {}",
                control_dir.display()
            )
        })?;
        fs::create_dir_all(control_dir.join(BRANCH_REFS_DIR)).with_context(|| {
            format!(
                "failed to create branch refs directory at {}",
                control_dir.display()
            )
        })?;
        fs::create_dir_all(control_dir.join(KEYRING_DIR)).with_context(|| {
            format!(
                "failed to create keyring directory at {}",
                control_dir.display()
            )
        })?;

        let branch_name = options.branch_name;
        let repo_id = derive_repo_id(&repo_root);
        let repo_secrets = generate_repo_secrets(&repo_id)?;
        let branch = BranchState {
            name: branch_name.clone(),
            token_hex: derive_branch_token(&repo_secrets.repo_ref_key, &branch_name),
        };
        let local_device_credential = generate_local_device_credential(
            "owner-admin".to_string(),
            "device-local".to_string(),
            "local-device".to_string(),
        )?;
        let layout_root = LayoutRoot {
            schema_version: REPO_FORMAT_VERSION,
            layout_id: DIRECT_LAYOUT_ID.to_string(),
            generation: 1,
            mapping_policy: DIRECT_MAPPING_POLICY.to_string(),
        };
        let default_ref = RefRecord {
            branch_name: branch.name.clone(),
            ref_token_hex: branch.token_hex.clone(),
            head_snapshot_id: None,
        };
        let keyring_state = KeyringState {
            format_version: REPO_FORMAT_VERSION,
            generation: 1,
            repo_id: repo_id.clone(),
            active_epoch: DEFAULT_ACTIVE_EPOCH,
            crypto_suite: "xchacha20poly1305".to_string(),
            kdf: "argon2id".to_string(),
            actors: vec![crate::keyring::ActorRecord {
                actor_id: local_device_credential.actor_id.clone(),
                display_name: "Local Owner".to_string(),
                role: "owner_admin".to_string(),
            }],
            devices: vec![crate::keyring::DeviceRecord {
                device_id: local_device_credential.device_id.clone(),
                actor_id: local_device_credential.actor_id.clone(),
                label: local_device_credential.label.clone(),
                device_pubkey_hex: local_device_credential.public_key_hex.clone(),
                status: "active".to_string(),
            }],
            epochs: vec![crate::keyring::EpochDescriptor {
                epoch: DEFAULT_ACTIVE_EPOCH,
                status: "active".to_string(),
            }],
            envelopes: vec![
                seal_repo_secrets(
                    &repo_id,
                    DEFAULT_ACTIVE_EPOCH,
                    &options.password,
                    &repo_secrets,
                    redact_password_hint(&options.password),
                )?,
                seal_repo_secrets_for_device(
                    &repo_id,
                    DEFAULT_ACTIVE_EPOCH,
                    &local_device_credential.public_key_hex,
                    &repo_secrets,
                    &local_device_credential.actor_id,
                    &local_device_credential.device_id,
                )?,
            ],
        };
        let keyring_pointer = KeyringPointer {
            generation: 1,
            current: "keyring.1".to_string(),
        };

        atomic_write_json(control_dir.join(LAYOUT_ROOT_FILE), &layout_root)?;
        write_keyring_generation_and_pointer(
            &control_dir,
            "keyring.1",
            &keyring_state,
            &keyring_pointer,
        )?;
        write_local_device_credential(&control_dir, &local_device_credential)?;
        write_default_ref(&control_dir, &repo_secrets, &default_ref)?;
        write_branch_ref(&control_dir, &repo_secrets, &default_ref)?;
        cache_unlocked_secrets(&control_dir, &repo_secrets);
        cache_unlocked_password(&control_dir, &options.password);

        Ok(RepositoryState {
            repo_root,
            branch,
            layout_generation: layout_root.generation,
        })
    }

    pub fn unlock(&self, repo_root: impl AsRef<Path>, password: &str) -> Result<RepositoryState> {
        let repo_root = repo_root.as_ref().to_path_buf();
        let control_dir = repo_root.join(CONTROL_DIR);
        let journal_path = control_dir.join(JOURNAL_DIR).join("keyring-update.json");
        let unlock_result = unlock_repo_secrets(&control_dir, password);
        let _ = match unlock_result {
            Ok(secrets) => Ok(secrets),
            Err(primary_error) => {
                if journal_path.is_file() {
                    let journal: serde_json::Value = read_json(journal_path.clone())?;
                    let current = journal["current"]
                        .as_str()
                        .context("invalid keyring recovery journal current")?;
                    let generation = journal["generation"]
                        .as_u64()
                        .context("invalid keyring recovery journal generation")?;
                    let generation_path = control_dir.join(KEYRING_DIR).join(current);
                    if generation_path.is_file() {
                        let secrets = unlock_repo_secrets_from_generation_file(
                            &control_dir,
                            current,
                            password,
                        )?;
                        atomic_write_json(
                            control_dir.join(KEYRING_CURRENT_FILE),
                            &KeyringPointer {
                                generation,
                                current: current.to_string(),
                            },
                        )?;
                        let _ = fs::remove_file(&journal_path);
                        cache_unlocked_secrets(&control_dir, &secrets);
                        cache_unlocked_password(&control_dir, password);
                        Ok(secrets)
                    } else {
                        Err(primary_error)
                    }
                } else {
                    Err(primary_error)
                }
            }
        }?;
        self.open(&repo_root)
    }

    pub fn change_password(
        &self,
        repo_root: impl AsRef<Path>,
        old_password: &str,
        new_password: &str,
    ) -> Result<()> {
        let repo_root = repo_root.as_ref().to_path_buf();
        let control_dir = repo_root.join(CONTROL_DIR);
        let repo_secrets = unlock_repo_secrets_uncached(&control_dir, old_password)?;
        let current_pointer: KeyringPointer = read_json(control_dir.join(KEYRING_CURRENT_FILE))?;
        let current_state: KeyringState =
            read_json(control_dir.join(KEYRING_DIR).join(&current_pointer.current))?;
        let next_generation = current_state.generation + 1;
        let next_file_name = format!("keyring.{next_generation}");
        let next_state = KeyringState {
            generation: next_generation,
            envelopes: rebuild_local_device_and_password_envelopes(
                &current_state,
                &repo_secrets,
                new_password,
            )?,
            ..current_state
        };
        let next_pointer = KeyringPointer {
            generation: next_generation,
            current: next_file_name.clone(),
        };
        write_keyring_generation_and_pointer(
            &control_dir,
            &next_file_name,
            &next_state,
            &next_pointer,
        )?;
        cache_unlocked_secrets(&control_dir, &repo_secrets);
        cache_unlocked_password(&control_dir, new_password);
        Ok(())
    }

    pub fn open(&self, repo_root: impl AsRef<Path>) -> Result<RepositoryState> {
        let repo_root = repo_root.as_ref().to_path_buf();
        let control_dir = repo_root.join(CONTROL_DIR);

        let layout_root = validate_layout_root(&control_dir)?;
        let repo_secrets = open_or_unlock_repo_secrets_with_local_device(&control_dir)?;
        let default_ref = read_default_ref(&control_dir, &repo_secrets)?;
        ensure!(
            !default_ref.branch_name.trim().is_empty(),
            "default ref record is missing branch name"
        );

        Ok(RepositoryState {
            repo_root,
            branch: BranchState {
                name: default_ref.branch_name,
                token_hex: default_ref.ref_token_hex,
            },
            layout_generation: layout_root.generation,
        })
    }

    pub fn commit(&self, options: CommitOptions) -> Result<CommitResult> {
        let repo_root = options.repo_root;
        ensure!(
            !options.message.trim().is_empty(),
            "commit message must not be empty"
        );

        let control_dir = repo_root.join(CONTROL_DIR);
        ensure!(
            control_dir.is_dir(),
            "repository is not initialized at {}",
            repo_root.display()
        );

        let repo_secrets = open_or_unlock_repo_secrets_with_local_device(&control_dir)?;
        let mut default_ref = read_current_ref(&control_dir)?;
        let mut committed_files = 0usize;
        let mut new_bytes = 0u64;
        let mut reused_bytes = 0u64;
        let mut warnings = Vec::new();
        let object_store = open_object_store_with_secrets(&control_dir, repo_secrets.clone());
        let stable_read_policy = self.stable_read_policy.clone().unwrap_or_default();
        let working_tree = match &self.snapshot_reader {
            Some(snapshot_reader) => WorkingTree::new_with_snapshot_reader(
                &repo_root,
                stable_read_policy,
                Arc::clone(snapshot_reader),
            ),
            None => WorkingTree::new_with_policy(&repo_root, stable_read_policy),
        };
        let tree_id = build_tree_object(
            &object_store,
            &working_tree,
            &repo_root,
            &mut committed_files,
            &mut new_bytes,
            &mut reused_bytes,
            &mut warnings,
        )?;

        let snapshot_object = SnapshotObject {
            schema_version: REPO_FORMAT_VERSION,
            message: options.message,
            root_tree_id: tree_id,
            parent_snapshot_id: default_ref.head_snapshot_id.clone(),
        };
        let snapshot_id = write_object(&object_store, "snapshot", &snapshot_object)?;

        default_ref.head_snapshot_id = Some(snapshot_id.clone());
        write_default_ref(&control_dir, &repo_secrets, &default_ref)?;
        write_branch_ref(&control_dir, &repo_secrets, &default_ref)?;

        Ok(CommitResult {
            snapshot_id,
            committed_files,
            new_bytes,
            reused_bytes,
            warnings,
        })
    }

    pub fn snapshots(&self, repo_root: impl AsRef<Path>) -> Result<Vec<SnapshotSummary>> {
        let repo_root = repo_root.as_ref().to_path_buf();
        let control_dir = repo_root.join(CONTROL_DIR);
        let repo_secrets = open_or_unlock_repo_secrets_with_local_device(&control_dir)?;
        let object_store = open_object_store_with_secrets(&control_dir, repo_secrets);
        let default_ref = read_current_ref(&control_dir)?;
        let mut next_snapshot_id = default_ref.head_snapshot_id;
        let mut snapshots = Vec::new();

        while let Some(snapshot_id) = next_snapshot_id {
            let snapshot = read_snapshot_object(&object_store, &snapshot_id)?;
            next_snapshot_id = snapshot.parent_snapshot_id.clone();
            snapshots.push(SnapshotSummary {
                snapshot_id,
                message: snapshot.message,
                parent_snapshot_id: snapshot.parent_snapshot_id,
            });
        }

        Ok(snapshots)
    }

    pub fn read_service(&self, repo_root: impl AsRef<Path>) -> Result<ReadService> {
        Ok(ReadService {
            repo_root: repo_root.as_ref().to_path_buf(),
        })
    }

    pub fn search_metadata(
        &self,
        repo_root: impl AsRef<Path>,
        query: MetadataSearchQuery,
    ) -> Result<Vec<MetadataSearchResult>> {
        let repo_root = repo_root.as_ref().to_path_buf();
        let control_dir = repo_root.join(CONTROL_DIR);
        let current_ref = read_current_ref(&control_dir)?;
        crate::local_index::search_metadata(
            &repo_root,
            &current_ref.ref_token_hex,
            current_ref.head_snapshot_id.as_deref(),
            &query,
        )
    }

    pub fn search_filenames(
        &self,
        repo_root: impl AsRef<Path>,
        query_text: &str,
    ) -> Result<Vec<FilenameSearchResult>> {
        let repo_root = repo_root.as_ref().to_path_buf();
        let control_dir = repo_root.join(CONTROL_DIR);
        let current_ref = read_current_ref(&control_dir)?;
        crate::local_index::search_filenames(
            &repo_root,
            &current_ref.ref_token_hex,
            current_ref.head_snapshot_id.as_deref(),
            query_text,
        )
    }

    pub fn create_branch(
        &self,
        repo_root: impl AsRef<Path>,
        branch_name: &str,
    ) -> Result<BranchState> {
        ensure!(
            !branch_name.trim().is_empty(),
            "branch name must not be empty"
        );
        let repo_root = repo_root.as_ref().to_path_buf();
        let control_dir = repo_root.join(CONTROL_DIR);
        let repo_secrets = open_or_unlock_repo_secrets_with_local_device(&control_dir)?;
        let current_ref = read_current_ref(&control_dir)?;
        let token_hex = derive_branch_token(&repo_secrets.repo_ref_key, branch_name);
        if current_ref.ref_token_hex == token_hex {
            anyhow::bail!("branch already exists: {branch_name}");
        }
        if read_branch_ref_if_exists(&control_dir, &repo_secrets, &token_hex)?.is_some() {
            anyhow::bail!("branch already exists: {branch_name}");
        }

        let branch_ref = RefRecord {
            branch_name: branch_name.to_string(),
            ref_token_hex: token_hex.clone(),
            head_snapshot_id: current_ref.head_snapshot_id,
        };
        write_branch_ref(&control_dir, &repo_secrets, &branch_ref)?;

        Ok(BranchState {
            name: branch_name.to_string(),
            token_hex,
        })
    }

    pub fn list_branches(&self, repo_root: impl AsRef<Path>) -> Result<Vec<BranchSummary>> {
        let repo_root = repo_root.as_ref().to_path_buf();
        let control_dir = repo_root.join(CONTROL_DIR);
        let repo_secrets = open_or_unlock_repo_secrets_with_local_device(&control_dir)?;
        let current_ref = read_current_ref(&control_dir)?;
        let mut branches = read_all_branch_refs(&control_dir, &repo_secrets)?;
        ensure!(
            branches
                .iter()
                .any(|branch| branch.ref_token_hex == current_ref.ref_token_hex),
            "current branch ref is missing from refs/branches"
        );
        branches.sort_by(|left, right| left.branch_name.cmp(&right.branch_name));

        Ok(branches
            .into_iter()
            .map(|branch| BranchSummary {
                name: branch.branch_name,
                token_hex: branch.ref_token_hex.clone(),
                head_snapshot_id: branch.head_snapshot_id,
                is_current: branch.ref_token_hex == current_ref.ref_token_hex,
            })
            .collect())
    }

    pub fn share_list(&self, repo_root: impl AsRef<Path>) -> Result<ShareListResult> {
        let repo_root = repo_root.as_ref().to_path_buf();
        let control_dir = repo_root.join(CONTROL_DIR);
        let keyring = read_current_keyring_state(&control_dir)?;
        Ok(ShareListResult {
            actors: keyring
                .actors
                .into_iter()
                .map(|actor| ShareActorSummary {
                    actor_id: actor.actor_id,
                    display_name: actor.display_name,
                    role: actor.role,
                })
                .collect(),
            devices: keyring
                .devices
                .into_iter()
                .map(|device| ShareDeviceSummary {
                    device_id: device.device_id,
                    actor_id: device.actor_id,
                    label: device.label,
                    status: device.status,
                })
                .collect(),
        })
    }

    pub fn share_invite_member(
        &self,
        repo_root: impl AsRef<Path>,
        options: ShareInviteMemberOptions,
    ) -> Result<ShareInviteBundle> {
        ensure!(
            !options.display_name.trim().is_empty(),
            "share invite display name must not be empty"
        );
        let repo_root = repo_root.as_ref().to_path_buf();
        let control_dir = repo_root.join(CONTROL_DIR);
        ensure_local_share_admin(&control_dir)?;
        let keyring = read_current_keyring_state(&control_dir)?;
        let actor_id = format!("member-{}", random_hex_identifier()?);
        let bootstrap_credential = generate_local_device_credential(
            actor_id.clone(),
            format!("device-{}", random_hex_identifier()?),
            "bootstrap-device".to_string(),
        )?;
        let branch = self.open(&repo_root)?.branch;
        let repo_secrets = open_or_unlock_repo_secrets_with_local_device(&control_dir)?;
        let bootstrap_generation = keyring.generation + 1;
        let bootstrap_generation_file = format!(
            "keyring.{}.bootstrap-{}",
            bootstrap_generation, bootstrap_credential.device_id
        );
        let bootstrap_keyring_state = KeyringState {
            format_version: keyring.format_version,
            generation: bootstrap_generation,
            repo_id: keyring.repo_id.clone(),
            active_epoch: keyring.active_epoch,
            crypto_suite: keyring.crypto_suite.clone(),
            kdf: keyring.kdf.clone(),
            actors: vec![crate::keyring::ActorRecord {
                actor_id: actor_id.clone(),
                display_name: options.display_name.clone(),
                role: "writer_member".to_string(),
            }],
            devices: vec![crate::keyring::DeviceRecord {
                device_id: bootstrap_credential.device_id.clone(),
                actor_id: actor_id.clone(),
                label: bootstrap_credential.label.clone(),
                device_pubkey_hex: bootstrap_credential.public_key_hex.clone(),
                status: "active".to_string(),
            }],
            epochs: keyring.epochs.clone(),
            envelopes: vec![seal_repo_secrets_for_device(
                &keyring.repo_id,
                keyring.active_epoch,
                &bootstrap_credential.public_key_hex,
                &repo_secrets,
                &actor_id,
                &bootstrap_credential.device_id,
            )?],
        };
        let bootstrap_keyring_pointer = KeyringPointer {
            generation: bootstrap_generation,
            current: bootstrap_generation_file,
        };
        let bundle_bytes = serde_json::to_vec_pretty(&serde_json::json!({
            "repo_id": keyring.repo_id,
            "actor_id": actor_id,
            "device_id": bootstrap_credential.device_id,
            "display_name": options.display_name,
            "role": "writer_member",
            "invite_kind": "member",
            "active_epoch": keyring.active_epoch,
            "branch_name": branch.name,
            "branch_token_hex": branch.token_hex,
            "default_ref_bytes_hex": hex::encode(std::fs::read(control_dir.join(DEFAULT_REF_FILE))?),
            "layout_root_bytes_hex": hex::encode(std::fs::read(control_dir.join(LAYOUT_ROOT_FILE))?),
            "bootstrap_credential": bootstrap_credential,
            "bootstrap_keyring_state": bootstrap_keyring_state,
            "bootstrap_keyring_pointer": bootstrap_keyring_pointer
        }))
        .context("failed to encode share invite bundle")?;
        Ok(ShareInviteBundle {
            actor_id,
            device_id: bootstrap_keyring_state.devices[0].device_id.clone(),
            bundle_bytes,
        })
    }

    pub fn share_accept_member(
        &self,
        repo_root: impl AsRef<Path>,
        options: ShareAcceptMemberOptions,
    ) -> Result<ShareAcceptResult> {
        let invite: ShareInvitePayload = serde_json::from_slice(&options.invite_bytes)
            .context("failed to decode share invite bundle")?;
        ensure!(
            invite.invite_kind == "member",
            "share invite bundle is not a member invite"
        );
        self.accept_share_device(repo_root, &invite, &options.local_device_label, true)
    }

    pub fn share_invite_device(
        &self,
        repo_root: impl AsRef<Path>,
        options: ShareInviteDeviceOptions,
    ) -> Result<ShareInviteBundle> {
        ensure!(
            !options.actor_id.trim().is_empty(),
            "share invite actor id must not be empty"
        );
        ensure!(
            !options.device_label.trim().is_empty(),
            "share invite device label must not be empty"
        );
        let repo_root = repo_root.as_ref().to_path_buf();
        let control_dir = repo_root.join(CONTROL_DIR);
        ensure_local_share_admin(&control_dir)?;
        let keyring = read_current_keyring_state(&control_dir)?;
        let actor = keyring
            .actors
            .iter()
            .find(|actor| actor.actor_id == options.actor_id)
            .with_context(|| format!("share actor not found: {}", options.actor_id))?;
        let bootstrap_credential = generate_local_device_credential(
            actor.actor_id.clone(),
            format!("device-{}", random_hex_identifier()?),
            options.device_label.clone(),
        )?;
        let branch = self.open(&repo_root)?.branch;
        let repo_secrets = open_or_unlock_repo_secrets_with_local_device(&control_dir)?;
        let bootstrap_generation = keyring.generation + 1;
        let bootstrap_generation_file = format!(
            "keyring.{}.bootstrap-{}",
            bootstrap_generation, bootstrap_credential.device_id
        );
        let bootstrap_keyring_state = KeyringState {
            format_version: keyring.format_version,
            generation: bootstrap_generation,
            repo_id: keyring.repo_id.clone(),
            active_epoch: keyring.active_epoch,
            crypto_suite: keyring.crypto_suite.clone(),
            kdf: keyring.kdf.clone(),
            actors: vec![actor.clone()],
            devices: vec![crate::keyring::DeviceRecord {
                device_id: bootstrap_credential.device_id.clone(),
                actor_id: actor.actor_id.clone(),
                label: bootstrap_credential.label.clone(),
                device_pubkey_hex: bootstrap_credential.public_key_hex.clone(),
                status: "active".to_string(),
            }],
            epochs: keyring.epochs.clone(),
            envelopes: vec![seal_repo_secrets_for_device(
                &keyring.repo_id,
                keyring.active_epoch,
                &bootstrap_credential.public_key_hex,
                &repo_secrets,
                &actor.actor_id,
                &bootstrap_credential.device_id,
            )?],
        };
        let bootstrap_keyring_pointer = KeyringPointer {
            generation: bootstrap_generation,
            current: bootstrap_generation_file,
        };
        let bundle_bytes = serde_json::to_vec_pretty(&serde_json::json!({
            "repo_id": keyring.repo_id,
            "actor_id": actor.actor_id,
            "device_id": bootstrap_credential.device_id,
            "display_name": actor.display_name,
            "role": actor.role,
            "invite_kind": "device",
            "active_epoch": keyring.active_epoch,
            "branch_name": branch.name,
            "branch_token_hex": branch.token_hex,
            "default_ref_bytes_hex": hex::encode(std::fs::read(control_dir.join(DEFAULT_REF_FILE))?),
            "layout_root_bytes_hex": hex::encode(std::fs::read(control_dir.join(LAYOUT_ROOT_FILE))?),
            "bootstrap_credential": bootstrap_credential,
            "bootstrap_keyring_state": bootstrap_keyring_state,
            "bootstrap_keyring_pointer": bootstrap_keyring_pointer
        }))
        .context("failed to encode share device invite bundle")?;
        Ok(ShareInviteBundle {
            actor_id: actor.actor_id.clone(),
            device_id: bootstrap_keyring_state.devices[0].device_id.clone(),
            bundle_bytes,
        })
    }

    pub fn share_accept_device(
        &self,
        repo_root: impl AsRef<Path>,
        options: ShareAcceptDeviceOptions,
    ) -> Result<ShareAcceptResult> {
        let invite: ShareInvitePayload = serde_json::from_slice(&options.invite_bytes)
            .context("failed to decode share device invite bundle")?;
        ensure!(
            invite.invite_kind == "device",
            "share invite bundle is not a device invite"
        );
        self.accept_share_device(repo_root, &invite, &options.local_device_label, false)
    }

    pub fn share_revoke_device(
        &self,
        repo_root: impl AsRef<Path>,
        options: ShareRevokeDeviceOptions,
    ) -> Result<()> {
        ensure!(
            !options.device_id.trim().is_empty(),
            "share device id must not be empty"
        );
        ensure!(
            !options.password.trim().is_empty(),
            "share revoke device password must not be empty"
        );
        let repo_root = repo_root.as_ref().to_path_buf();
        let control_dir = repo_root.join(CONTROL_DIR);
        ensure_local_share_admin(&control_dir)?;
        let mut repo_secrets = open_or_unlock_repo_secrets_with_local_device(&control_dir)?;
        let password = options.password;
        let current_pointer: KeyringPointer = read_json(control_dir.join(KEYRING_CURRENT_FILE))?;
        let current_state: KeyringState =
            read_json(control_dir.join(KEYRING_DIR).join(&current_pointer.current))?;
        let target_device = current_state
            .devices
            .iter()
            .find(|device| device.device_id == options.device_id)
            .with_context(|| format!("share device not found: {}", options.device_id))?;
        ensure!(
            current_state.actors.iter().all(
                |actor| actor.actor_id != target_device.actor_id || actor.role != "owner_admin"
            ),
            "cannot revoke owner-admin device"
        );

        let next_generation = current_state.generation + 1;
        let next_file_name = format!("keyring.{next_generation}");
        repo_secrets.active_epoch += 1;
        repo_secrets.repo_manifest_enc_key = random_key_material()?;
        repo_secrets.repo_nonce_key = random_key_material()?;
        repo_secrets.epoch_keys.insert(
            repo_secrets.active_epoch,
            EpochSecrets {
                manifest_enc_key: repo_secrets.repo_manifest_enc_key,
                nonce_key: repo_secrets.repo_nonce_key,
            },
        );

        let devices = current_state
            .devices
            .iter()
            .cloned()
            .map(|mut device| {
                if device.device_id == options.device_id {
                    device.status = "revoked".to_string();
                }
                device
            })
            .collect::<Vec<_>>();
        let epochs = build_epoch_descriptors(&repo_secrets);
        let mut next_state = KeyringState {
            generation: next_generation,
            active_epoch: repo_secrets.active_epoch,
            devices,
            epochs,
            envelopes: rebuild_local_device_and_password_envelopes(
                &current_state,
                &repo_secrets,
                &password,
            )?,
            ..current_state
        };
        next_state
            .envelopes
            .retain(|envelope| envelope.device_id != options.device_id);

        let next_pointer = KeyringPointer {
            generation: next_generation,
            current: next_file_name.clone(),
        };
        write_keyring_generation_and_pointer(
            &control_dir,
            &next_file_name,
            &next_state,
            &next_pointer,
        )?;
        cache_unlocked_secrets(&control_dir, &repo_secrets);
        cache_unlocked_password(&control_dir, &password);
        Ok(())
    }

    pub fn share_revoke_member(
        &self,
        repo_root: impl AsRef<Path>,
        options: ShareRevokeMemberOptions,
    ) -> Result<()> {
        ensure!(
            !options.actor_id.trim().is_empty(),
            "share actor id must not be empty"
        );
        ensure!(
            !options.password.trim().is_empty(),
            "share revoke member password must not be empty"
        );
        let repo_root = repo_root.as_ref().to_path_buf();
        let control_dir = repo_root.join(CONTROL_DIR);
        ensure_local_share_admin(&control_dir)?;
        let mut repo_secrets = open_or_unlock_repo_secrets_with_local_device(&control_dir)?;
        let password = options.password;
        let current_pointer: KeyringPointer = read_json(control_dir.join(KEYRING_CURRENT_FILE))?;
        let current_state: KeyringState =
            read_json(control_dir.join(KEYRING_DIR).join(&current_pointer.current))?;
        ensure!(
            current_state
                .actors
                .iter()
                .any(|actor| actor.actor_id == options.actor_id),
            "share actor not found: {}",
            options.actor_id
        );
        ensure!(
            current_state
                .actors
                .iter()
                .all(|actor| actor.actor_id != options.actor_id || actor.role != "owner_admin"),
            "cannot revoke owner-admin actor"
        );

        let next_generation = current_state.generation + 1;
        let next_file_name = format!("keyring.{next_generation}");
        repo_secrets.active_epoch += 1;
        repo_secrets.repo_manifest_enc_key = random_key_material()?;
        repo_secrets.repo_nonce_key = random_key_material()?;
        repo_secrets.epoch_keys.insert(
            repo_secrets.active_epoch,
            EpochSecrets {
                manifest_enc_key: repo_secrets.repo_manifest_enc_key,
                nonce_key: repo_secrets.repo_nonce_key,
            },
        );

        let devices = current_state
            .devices
            .iter()
            .cloned()
            .map(|mut device| {
                if device.actor_id == options.actor_id {
                    device.status = "revoked".to_string();
                }
                device
            })
            .collect::<Vec<_>>();

        let epochs = build_epoch_descriptors(&repo_secrets);
        let mut next_state = KeyringState {
            generation: next_generation,
            active_epoch: repo_secrets.active_epoch,
            devices,
            epochs,
            envelopes: rebuild_local_device_and_password_envelopes(
                &current_state,
                &repo_secrets,
                &password,
            )?,
            ..current_state
        };
        next_state
            .envelopes
            .retain(|envelope| envelope.actor_id != options.actor_id);

        let next_pointer = KeyringPointer {
            generation: next_generation,
            current: next_file_name.clone(),
        };
        write_keyring_generation_and_pointer(
            &control_dir,
            &next_file_name,
            &next_state,
            &next_pointer,
        )?;
        cache_unlocked_secrets(&control_dir, &repo_secrets);
        cache_unlocked_password(&control_dir, &password);
        Ok(())
    }

    fn accept_share_device(
        &self,
        repo_root: impl AsRef<Path>,
        invite: &ShareInvitePayload,
        local_device_label: &str,
        create_actor: bool,
    ) -> Result<ShareAcceptResult> {
        ensure!(
            !local_device_label.trim().is_empty(),
            "share accept local device label must not be empty"
        );
        let repo_root = repo_root.as_ref().to_path_buf();
        let control_dir = repo_root.join(CONTROL_DIR);
        if !control_dir.exists() {
            bootstrap_recipient_repository(&repo_root, invite, local_device_label)?;
            return Ok(ShareAcceptResult {
                actor_id: invite.actor_id.clone(),
                device_id: invite.bootstrap_credential.device_id.clone(),
                role: invite.role.clone(),
            });
        }

        let repo_secrets = open_or_unlock_repo_secrets_with_local_device(&control_dir)?;
        ensure!(
            repo_secrets.repo_id == invite.repo_id,
            "share invite bundle targets a different repository"
        );

        let current_pointer: KeyringPointer = read_json(control_dir.join(KEYRING_CURRENT_FILE))?;
        let current_state: KeyringState =
            read_json(control_dir.join(KEYRING_DIR).join(&current_pointer.current))?;
        if create_actor {
            ensure!(
                !current_state
                    .actors
                    .iter()
                    .any(|actor| actor.actor_id == invite.actor_id),
                "share actor already exists"
            );
        } else {
            ensure!(
                current_state
                    .actors
                    .iter()
                    .any(|actor| actor.actor_id == invite.actor_id),
                "share actor not found: {}",
                invite.actor_id
            );
        }

        let next_generation = current_state.generation + 1;
        let next_file_name = format!("keyring.{next_generation}");
        let local_device = generate_local_device_credential(
            invite.actor_id.clone(),
            format!("device-{}", random_hex_identifier()?),
            local_device_label.to_string(),
        )?;

        let mut actors = current_state.actors.clone();
        if create_actor {
            actors.push(crate::keyring::ActorRecord {
                actor_id: invite.actor_id.clone(),
                display_name: invite.display_name.clone(),
                role: invite.role.clone(),
            });
        }

        let mut devices = current_state.devices.clone();
        devices.push(crate::keyring::DeviceRecord {
            device_id: local_device.device_id.clone(),
            actor_id: invite.actor_id.clone(),
            label: local_device.label.clone(),
            device_pubkey_hex: local_device.public_key_hex.clone(),
            status: "active".to_string(),
        });

        let mut envelopes = current_state.envelopes.clone();
        envelopes.push(seal_repo_secrets_for_device(
            &repo_secrets.repo_id,
            repo_secrets.active_epoch,
            &local_device.public_key_hex,
            &repo_secrets,
            &invite.actor_id,
            &local_device.device_id,
        )?);

        let next_state = KeyringState {
            generation: next_generation,
            actors,
            devices,
            envelopes,
            ..current_state
        };
        let next_pointer = KeyringPointer {
            generation: next_generation,
            current: next_file_name.clone(),
        };
        write_keyring_generation_and_pointer(
            &control_dir,
            &next_file_name,
            &next_state,
            &next_pointer,
        )?;
        write_local_device_credential(&control_dir, &local_device)?;
        cache_unlocked_secrets(&control_dir, &repo_secrets);

        Ok(ShareAcceptResult {
            actor_id: invite.actor_id.clone(),
            device_id: local_device.device_id,
            role: invite.role.clone(),
        })
    }

    pub fn delete_branch(&self, repo_root: impl AsRef<Path>, branch_name: &str) -> Result<()> {
        ensure!(
            !branch_name.trim().is_empty(),
            "branch name must not be empty"
        );
        let repo_root = repo_root.as_ref().to_path_buf();
        let control_dir = repo_root.join(CONTROL_DIR);
        let repo_secrets = open_or_unlock_repo_secrets_with_local_device(&control_dir)?;
        let current_ref = read_current_ref(&control_dir)?;
        let token_hex = derive_branch_token(&repo_secrets.repo_ref_key, branch_name);
        if current_ref.ref_token_hex == token_hex {
            anyhow::bail!("cannot delete the current branch");
        }
        let path = branch_ref_path(&control_dir, &token_hex)?;
        ensure!(path.is_file(), "branch not found: {branch_name}");
        fs::remove_file(&path)
            .with_context(|| format!("failed to delete branch ref {}", path.display()))?;
        Ok(())
    }

    pub fn checkout_branch(
        &self,
        repo_root: impl AsRef<Path>,
        branch_name: &str,
    ) -> Result<RepositoryState> {
        ensure!(
            !branch_name.trim().is_empty(),
            "branch name must not be empty"
        );
        let repo_root = repo_root.as_ref().to_path_buf();
        let control_dir = repo_root.join(CONTROL_DIR);
        let repo_secrets = open_or_unlock_repo_secrets_with_local_device(&control_dir)?;
        let current_ref = read_current_ref(&control_dir)?;
        let token_hex = derive_branch_token(&repo_secrets.repo_ref_key, branch_name);
        let target_ref = if current_ref.ref_token_hex == token_hex {
            current_ref
        } else {
            read_branch_ref_if_exists(&control_dir, &repo_secrets, &token_hex)?
                .with_context(|| format!("branch not found: {branch_name}"))?
        };
        write_default_ref(&control_dir, &repo_secrets, &target_ref)?;
        self.open(&repo_root)
    }

    pub fn checkout(&self, _options: CheckoutOptions) -> Result<()> {
        let read_service = self.read_service(&_options.repo_root)?;
        let snapshot = read_service.open_snapshot(&_options.snapshot_id)?;
        let working_tree = WorkingTree::new(&_options.target_dir);

        ensure!(
            _options.target_dir.is_dir(),
            "checkout target must already exist: {}",
            _options.target_dir.display()
        );

        let planned_files = collect_checkout_file_paths(&read_service, &snapshot, "")?;
        let relative_paths = planned_files
            .iter()
            .map(|(snapshot_path, _)| snapshot_path.clone())
            .collect::<Vec<_>>();
        let required_bytes = planned_files.iter().fold(0u64, |total, (_, file)| {
            total.saturating_add(file.file_size())
        });
        let final_paths = working_tree.preflight_checkout_paths_for_bytes(
            &_options.target_dir,
            &relative_paths,
            required_bytes,
        )?;
        let mut staged = Vec::with_capacity(planned_files.len());

        for ((_snapshot_path, file), final_path) in
            planned_files.into_iter().zip(final_paths.into_iter())
        {
            let stage_result: Result<()> = (|| {
                let temp_path =
                    checkout_file_to_temp(&working_tree, &file, &final_path, |offset, length| {
                        read_service.read_range(&file, offset, length)
                    })?;
                staged.push((_snapshot_path, temp_path, final_path));
                Ok(())
            })();

            if let Err(error) = stage_result {
                for (_, temp_path, _) in &staged {
                    let _ = fs::remove_file(temp_path);
                }
                return Err(error);
            }
        }

        for (snapshot_path, temp_path, final_path) in staged {
            working_tree.publish_checkout_temp(&temp_path, &final_path)?;
            let observed_name = working_tree.observed_checkout_name(&final_path)?;
            let expected_name = snapshot_path.split('/').next_back().unwrap_or_default();
            working_tree.validate_checkout_read_back(expected_name, &observed_name)?;
            working_tree.record_platform_name_mapping(&snapshot_path, &final_path)?;
        }

        Ok(())
    }

    pub fn verify_snapshot(&self, _repo_root: impl AsRef<Path>, _snapshot_id: &str) -> Result<()> {
        let repo_root = _repo_root.as_ref().to_path_buf();
        let control_dir = repo_root.join(CONTROL_DIR);
        let _repo_state = self.open(&repo_root)?;
        let object_store = open_object_store(&control_dir)?;
        let manifest_store = LocalManifestStore::new(&repo_root);
        verify_snapshot_graph(&manifest_store, &object_store, _snapshot_id)
    }

    pub fn verify_object(
        &self,
        _repo_root: impl AsRef<Path>,
        _object_id: &str,
        _expected_type: &str,
    ) -> Result<()> {
        let repo_root = _repo_root.as_ref().to_path_buf();
        let control_dir = repo_root.join(CONTROL_DIR);
        let object_store = open_object_store(&control_dir)?;
        let _ = object_store.get_object(_object_id, _expected_type)?;
        Ok(())
    }

    pub fn verify_ref(&self, _repo_root: impl AsRef<Path>) -> Result<()> {
        let repo_root = _repo_root.as_ref().to_path_buf();
        let control_dir = repo_root.join(CONTROL_DIR);
        let _repo_state = self.open(&repo_root)?;
        let repo_secrets = open_or_unlock_repo_secrets_with_local_device(&control_dir)?;
        let default_ref = read_default_ref(&control_dir, &repo_secrets)?;
        if let Some(snapshot_id) = default_ref.head_snapshot_id.as_deref() {
            let object_store = open_object_store(&control_dir)?;
            let manifest_store = LocalManifestStore::new(&repo_root);
            verify_snapshot_graph(&manifest_store, &object_store, snapshot_id)?;
        }
        Ok(())
    }

    pub fn default_head_snapshot_id(&self, repo_root: impl AsRef<Path>) -> Result<Option<String>> {
        let repo_root = repo_root.as_ref().to_path_buf();
        let control_dir = repo_root.join(CONTROL_DIR);
        let repo_secrets = open_or_unlock_repo_secrets_with_local_device(&control_dir)?;
        Ok(read_default_ref(&control_dir, &repo_secrets)?.head_snapshot_id)
    }

    pub fn update_branch_head_if_fast_forward(
        &self,
        repo_root: impl AsRef<Path>,
        ref_token_hex: &str,
        next_head_snapshot_id: Option<&str>,
    ) -> Result<Option<String>> {
        validate_ref_token_value(ref_token_hex)?;
        if let Some(snapshot_id) = next_head_snapshot_id {
            validate_object_id_value(snapshot_id).context("invalid snapshot id")?;
        }
        let repo_root = repo_root.as_ref().to_path_buf();
        let control_dir = repo_root.join(CONTROL_DIR);
        let repo_secrets = open_or_unlock_repo_secrets_with_local_device(&control_dir)?;
        let current_default_ref = read_default_ref(&control_dir, &repo_secrets)?;
        let mut branch_ref = if let Some(branch_ref) =
            read_branch_ref_if_exists(&control_dir, &repo_secrets, ref_token_hex)?
        {
            branch_ref
        } else {
            ensure!(
                current_default_ref.ref_token_hex == ref_token_hex,
                "branch ref not found for token"
            );
            current_default_ref.clone()
        };
        let previous_head_snapshot_id = branch_ref.head_snapshot_id.clone();

        if previous_head_snapshot_id.as_deref() == next_head_snapshot_id {
            return Ok(previous_head_snapshot_id);
        }

        ensure!(
            branch_fast_forward_allowed(
                &repo_root,
                previous_head_snapshot_id.as_deref(),
                next_head_snapshot_id
            )?,
            "pull diverged: local branch requires rebase"
        );

        branch_ref.head_snapshot_id = next_head_snapshot_id.map(ToString::to_string);
        write_branch_ref(&control_dir, &repo_secrets, &branch_ref)?;
        if current_default_ref.ref_token_hex == ref_token_hex {
            write_default_ref(&control_dir, &repo_secrets, &branch_ref)?;
        }
        Ok(previous_head_snapshot_id)
    }
}

impl ReadService {
    pub fn new(repo_root: impl AsRef<Path>) -> Self {
        Self {
            repo_root: repo_root.as_ref().to_path_buf(),
        }
    }

    pub fn derive_vfs_cache_key(&self) -> Result<[u8; 32]> {
        let control_dir = self.repo_root.join(CONTROL_DIR);
        let repo_secrets = open_or_unlock_repo_secrets_with_local_device(&control_dir)?;
        let mut hasher = Hasher::new();
        hasher.update(&repo_secrets.repo_path_index_key);
        hasher.update(&[0]);
        hasher.update(repo_secrets.repo_id.as_bytes());
        hasher.update(b"\0e2v-vfs-range-cache-key");
        let mut key = [0u8; 32];
        key.copy_from_slice(&hasher.finalize().as_bytes()[..32]);
        Ok(key)
    }

    pub fn open_snapshot(&self, snapshot_id: &str) -> Result<SnapshotHandle> {
        validate_object_id_value(snapshot_id).context("invalid snapshot id")?;
        let control_dir = self.repo_root.join(CONTROL_DIR);
        let repo_secrets = open_or_unlock_repo_secrets_with_local_device(&control_dir)?;
        let object_store = open_object_store_with_secrets(&control_dir, repo_secrets);
        let layout_root = validate_layout_root(&control_dir)?;
        let snapshot = read_snapshot_object(&object_store, snapshot_id)?;

        Ok(SnapshotHandle {
            snapshot_id: snapshot_id.to_string(),
            layout_generation: layout_root.generation,
            root_tree_id: snapshot.root_tree_id,
        })
    }

    pub fn resolve_branch(&self, ref_token_hex: &str) -> Result<SnapshotHandle> {
        validate_ref_token_value(ref_token_hex)?;
        let control_dir = self.repo_root.join(CONTROL_DIR);
        let repo_secrets = open_or_unlock_repo_secrets_with_local_device(&control_dir)?;
        let layout_root = validate_layout_root(&control_dir)?;
        let branch_ref = if let Some(branch_ref) =
            read_branch_ref_if_exists(&control_dir, &repo_secrets, ref_token_hex)?
        {
            branch_ref
        } else {
            let current_ref = read_default_ref(&control_dir, &repo_secrets)?;
            anyhow::ensure!(
                current_ref.ref_token_hex == ref_token_hex,
                "branch ref not found for token"
            );
            current_ref
        };
        let snapshot_id = branch_ref
            .head_snapshot_id
            .context("branch ref does not point to a snapshot")?;
        let manifest_store = LocalManifestStore::new(&self.repo_root);
        let snapshot = manifest_store.get_snapshot(&snapshot_id)?;

        Ok(SnapshotHandle {
            snapshot_id,
            layout_generation: layout_root.generation,
            root_tree_id: snapshot.root_tree_id,
        })
    }

    pub fn read_dir(&self, snapshot: &SnapshotHandle, path: &str) -> Result<Vec<DirectoryEntry>> {
        let control_dir = self.repo_root.join(CONTROL_DIR);
        let _repo_secrets = open_or_unlock_repo_secrets_with_local_device(&control_dir)?;
        let manifest_store = LocalManifestStore::new(&self.repo_root);
        let tree_id = resolve_tree_for_path(&manifest_store, &snapshot.root_tree_id, path)?;
        let tree = manifest_store.get_tree_node(&tree_id)?;

        Ok(tree
            .entries
            .into_iter()
            .map(|entry| DirectoryEntry {
                name: entry.name,
                kind: entry.kind,
            })
            .collect())
    }

    pub fn open_file(&self, snapshot: &SnapshotHandle, path: &str) -> Result<FileHandle> {
        let control_dir = self.repo_root.join(CONTROL_DIR);
        let repo_secrets = open_or_unlock_repo_secrets_with_local_device(&control_dir)?;
        let manifest_store = LocalManifestStore::new(&self.repo_root);
        let (parent_path, file_name) = split_parent_and_name(path)?;
        let tree_id = resolve_tree_for_path(&manifest_store, &snapshot.root_tree_id, &parent_path)?;
        let tree = manifest_store.get_tree_node(&tree_id)?;
        let entry = tree
            .entries
            .into_iter()
            .find(|entry| entry.name == file_name && entry.kind == "file");
        let entry = entry.with_context(|| format!("file not found in snapshot: {path}"))?;
        let object_store = open_object_store_with_secrets(&control_dir, repo_secrets);
        let file = read_file_object(&object_store, &entry.object_id)?;

        Ok(FileHandle {
            snapshot_id: snapshot.snapshot_id.clone(),
            file_object_id: entry.object_id,
            file_size: file.file_size,
            chunk_count: file.chunk_count as usize,
            chunk_ids: file.chunks,
            chunk_lengths: file.chunk_lengths,
            shard_ids: file.shard_ids,
            shard_byte_lengths: file.shard_byte_lengths,
            layout_generation: snapshot.layout_generation,
            crypto_suite: "xchacha20poly1305".to_string(),
            key_epoch: DEFAULT_ACTIVE_EPOCH,
            chunker_id: file.chunker_id,
        })
    }

    pub fn read_range(&self, file: &FileHandle, offset: usize, length: usize) -> Result<Vec<u8>> {
        let control_dir = self.repo_root.join(CONTROL_DIR);
        let repo_secrets = open_or_unlock_repo_secrets_with_local_device(&control_dir)?;
        let object_store = open_object_store_with_secrets(&control_dir, repo_secrets);
        let file_size: usize = file
            .file_size
            .try_into()
            .map_err(|_| anyhow::anyhow!("file size does not fit in usize"))?;
        ensure!(offset <= file_size, "range offset out of bounds");
        let end = offset.saturating_add(length).min(file_size);
        if offset == end {
            return Ok(Vec::new());
        }
        let mut output = Vec::with_capacity(end - offset);
        let mut chunk_start = 0usize;
        if !file.chunk_ids.is_empty() {
            ensure!(
                file.chunk_ids.len() == file.chunk_lengths.len(),
                "file chunk metadata is inconsistent"
            );
            let mut chunk_read_context = ChunkReadContext {
                output: &mut output,
                object_store: &object_store,
                repo_root: &self.repo_root,
                file,
                offset,
                end,
                chunk_start: &mut chunk_start,
            };
            append_requested_bytes_from_chunks(
                &mut chunk_read_context,
                &file.chunk_ids,
                &file.chunk_lengths,
            )?;
        } else {
            ensure!(!file.shard_ids.is_empty(), "file has no chunks");
            ensure!(
                file.shard_ids.len() == file.shard_byte_lengths.len(),
                "file shard metadata is inconsistent"
            );
            let mut shard_byte_start = 0usize;
            for (shard_id, shard_byte_length) in
                file.shard_ids.iter().zip(file.shard_byte_lengths.iter())
            {
                let shard_len: usize = (*shard_byte_length)
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("file shard length does not fit in usize"))?;
                let shard_byte_end = shard_byte_start.saturating_add(shard_len);
                if output.len() >= end - offset {
                    break;
                }
                if shard_byte_end <= offset {
                    chunk_start = shard_byte_end;
                    shard_byte_start = shard_byte_end;
                    continue;
                }
                if shard_byte_start >= end {
                    break;
                }
                let shard: FileShardObject =
                    read_stored_object(&object_store, shard_id, "file_shard")?;
                validate_manifest_schema_version_local("file_shard", shard.schema_version)?;
                ensure!(
                    shard.chunks.len() == shard.chunk_lengths.len(),
                    "file shard metadata is inconsistent"
                );
                let mut chunk_read_context = ChunkReadContext {
                    output: &mut output,
                    object_store: &object_store,
                    repo_root: &self.repo_root,
                    file,
                    offset,
                    end,
                    chunk_start: &mut chunk_start,
                };
                append_requested_bytes_from_chunks(
                    &mut chunk_read_context,
                    &shard.chunks,
                    &shard.chunk_lengths,
                )?;
                shard_byte_start = shard_byte_end;
            }
        }

        ensure!(
            output.len() == end - offset,
            "file chunk coverage incomplete for requested range"
        );

        Ok(output)
    }
}

fn checkout_file_to_temp<F>(
    working_tree: &WorkingTree,
    file: &FileHandle,
    final_path: &Path,
    mut read_range: F,
) -> Result<PathBuf>
where
    F: FnMut(usize, usize) -> Result<Vec<u8>>,
{
    let parent = final_path
        .parent()
        .with_context(|| format!("checkout target has no parent: {}", final_path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create checkout directory {}", parent.display()))?;
    working_tree.ensure_checkout_target_is_clear(final_path)?;

    let temp_path = final_path.with_extension("e2v-tmp");
    let result = (|| -> Result<()> {
        let mut temp_file = fs::File::create(&temp_path).with_context(|| {
            format!("failed to write checkout temp file {}", temp_path.display())
        })?;
        let file_size: usize = file
            .file_size()
            .try_into()
            .map_err(|_| anyhow::anyhow!("file size does not fit in usize"))?;
        let mut offset = 0usize;
        while offset < file_size {
            let chunk_len = (file_size - offset).min(CHECKOUT_STREAM_CHUNK_BYTES);
            let bytes = read_range(offset, chunk_len)?;
            ensure!(
                bytes.len() == chunk_len,
                "checkout read returned truncated data"
            );
            use std::io::Write as _;
            temp_file.write_all(&bytes).with_context(|| {
                format!("failed to write checkout temp file {}", temp_path.display())
            })?;
            offset += chunk_len;
        }
        Ok(())
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    Ok(temp_path)
}

fn directory_is_empty(path: &Path) -> Result<bool> {
    let mut entries = fs::read_dir(path)
        .with_context(|| format!("failed to read directory {}", path.display()))?;
    Ok(entries.next().transpose()?.is_none())
}

fn derive_repo_id(repo_root: &Path) -> String {
    let mut hasher = Hasher::new();
    hasher.update(repo_root.to_string_lossy().as_bytes());
    hex::encode(hasher.finalize().as_bytes())
}

fn derive_branch_token(repo_ref_key: &[u8; 32], branch_name: &str) -> String {
    let mut input = Vec::new();
    input.extend_from_slice(b"branch:");
    input.extend_from_slice(branch_name.as_bytes());
    hex::encode(blake3::keyed_hash(repo_ref_key, &input).as_bytes())
}

fn redact_password_hint(password: &str) -> String {
    format!("len:{}", password.chars().count())
}

fn generate_repo_secrets(repo_id: &str) -> Result<RepoSecrets> {
    let repo_manifest_enc_key = random_key_material()?;
    let repo_nonce_key = random_key_material()?;
    let active_epoch = DEFAULT_ACTIVE_EPOCH;
    Ok(RepoSecrets {
        repo_id: repo_id.to_string(),
        active_epoch,
        repo_dedup_key: random_key_material()?,
        repo_ref_key: random_key_material()?,
        repo_manifest_enc_key,
        repo_nonce_key,
        repo_path_index_key: random_key_material()?,
        epoch_keys: std::collections::BTreeMap::from([(
            active_epoch,
            EpochSecrets {
                manifest_enc_key: repo_manifest_enc_key,
                nonce_key: repo_nonce_key,
            },
        )]),
    })
}

fn random_key_material() -> Result<[u8; 32]> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("failed to obtain repository key material"))?;
    Ok(bytes)
}

fn random_hex_identifier() -> Result<String> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("failed to obtain random identifier"))?;
    Ok(hex::encode(bytes))
}

pub fn rotate_active_epoch_for_test(repo_root: impl AsRef<Path>, password: &str) -> Result<()> {
    let repo_root = repo_root.as_ref().to_path_buf();
    let control_dir = repo_root.join(CONTROL_DIR);
    let mut repo_secrets = unlock_repo_secrets_uncached(&control_dir, password)?;
    let current_pointer: KeyringPointer = read_json(control_dir.join(KEYRING_CURRENT_FILE))?;
    let current_state: KeyringState =
        read_json(control_dir.join(KEYRING_DIR).join(&current_pointer.current))?;
    let next_generation = current_state.generation + 1;
    let next_file_name = format!("keyring.{next_generation}");

    repo_secrets.active_epoch += 1;
    repo_secrets.repo_manifest_enc_key = random_key_material()?;
    repo_secrets.repo_nonce_key = random_key_material()?;
    repo_secrets.epoch_keys.insert(
        repo_secrets.active_epoch,
        EpochSecrets {
            manifest_enc_key: repo_secrets.repo_manifest_enc_key,
            nonce_key: repo_secrets.repo_nonce_key,
        },
    );

    let next_state = KeyringState {
        generation: next_generation,
        active_epoch: repo_secrets.active_epoch,
        epochs: build_epoch_descriptors(&repo_secrets),
        envelopes: rebuild_local_device_and_password_envelopes(
            &current_state,
            &repo_secrets,
            password,
        )?,
        ..current_state
    };
    let next_pointer = KeyringPointer {
        generation: next_generation,
        current: next_file_name.clone(),
    };
    write_keyring_generation_and_pointer(
        &control_dir,
        &next_file_name,
        &next_state,
        &next_pointer,
    )?;
    cache_unlocked_secrets(&control_dir, &repo_secrets);
    Ok(())
}

pub fn unlock_with_local_device_for_test(repo_root: impl AsRef<Path>) -> Result<RepositoryState> {
    let repo_root = repo_root.as_ref().to_path_buf();
    let control_dir = repo_root.join(CONTROL_DIR);
    let secrets = unlock_repo_secrets_with_local_device(&control_dir)?;
    cache_unlocked_secrets(&control_dir, &secrets);
    RepositoryFacade::new().open(&repo_root)
}

pub fn reconcile_remote_keyring_for_sync(
    repo_root: impl AsRef<Path>,
    remote_keyring_bytes: &[u8],
) -> Result<bool> {
    let repo_root = repo_root.as_ref().to_path_buf();
    let control_dir = repo_root.join(CONTROL_DIR);
    let current_pointer: KeyringPointer = read_json(control_dir.join(KEYRING_CURRENT_FILE))?;
    let local_state: KeyringState =
        read_json(control_dir.join(KEYRING_DIR).join(&current_pointer.current))?;
    let remote_state: KeyringState = serde_json::from_slice(remote_keyring_bytes)
        .context("failed to decode remote keyring state")?;
    ensure!(
        local_state.repo_id == remote_state.repo_id,
        "remote keyring targets a different repository"
    );

    let local_secrets = match open_repo_secrets(&control_dir) {
        Ok(secrets) => secrets,
        Err(_) => unlock_repo_secrets_with_local_device(&control_dir)?,
    };
    let merged_active_epoch = local_state.active_epoch.max(remote_state.active_epoch);
    let authoritative_secrets = if local_secrets.active_epoch == merged_active_epoch {
        local_secrets.clone()
    } else {
        unlock_repo_secrets_from_keyring_bytes_with_local_device(
            &control_dir,
            remote_keyring_bytes,
        )?
    };

    let mut merged_actors = merge_actor_records(&local_state, &remote_state)?;
    merged_actors.sort_by(|left, right| left.actor_id.cmp(&right.actor_id));
    let mut merged_devices = merge_device_records(&local_state, &remote_state)?;
    merged_devices.sort_by(|left, right| left.device_id.cmp(&right.device_id));
    let merged_epochs =
        merge_epoch_descriptors(&local_state, &remote_state, &authoritative_secrets);
    let merged_state_without_envelopes = KeyringState {
        format_version: local_state.format_version.max(remote_state.format_version),
        generation: 0,
        repo_id: local_state.repo_id.clone(),
        active_epoch: merged_active_epoch,
        crypto_suite: local_state.crypto_suite.clone(),
        kdf: local_state.kdf.clone(),
        actors: merged_actors,
        devices: merged_devices,
        epochs: merged_epochs,
        envelopes: Vec::new(),
    };

    let local_comparable = comparable_keyring_state(&local_state);
    let remote_comparable = comparable_keyring_state(&remote_state);
    let merged_comparable = merged_state_without_envelopes.clone();
    if merged_comparable == local_comparable || merged_comparable == remote_comparable {
        return Ok(false);
    }

    let mut merged_envelopes = Vec::new();
    if let Some(password_envelope) =
        select_password_envelope_for_active_epoch(&local_state, &remote_state, merged_active_epoch)?
    {
        merged_envelopes.push(password_envelope);
    }
    merged_envelopes.extend(build_device_envelopes_for_active_devices(
        &local_state.repo_id,
        &merged_state_without_envelopes.devices,
        merged_active_epoch,
        &authoritative_secrets,
    )?);
    merged_envelopes.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then(left.actor_id.cmp(&right.actor_id))
            .then(left.device_id.cmp(&right.device_id))
            .then(left.envelope_id.cmp(&right.envelope_id))
    });

    let merged_state = KeyringState {
        generation: local_state.generation.max(remote_state.generation) + 1,
        envelopes: merged_envelopes,
        ..merged_state_without_envelopes
    };

    let local_content = comparable_keyring_state(&local_state);
    let merged_content = comparable_keyring_state(&merged_state);
    if local_content == merged_content {
        return Ok(false);
    }
    let next_file_name = format!("keyring.{}", merged_state.generation);
    let next_pointer = KeyringPointer {
        generation: merged_state.generation,
        current: next_file_name.clone(),
    };
    write_keyring_generation_and_pointer(
        &control_dir,
        &next_file_name,
        &merged_state,
        &next_pointer,
    )?;
    cache_unlocked_secrets(&control_dir, &authoritative_secrets);
    Ok(true)
}

fn rebuild_local_device_and_password_envelopes(
    current_state: &KeyringState,
    repo_secrets: &RepoSecrets,
    password: &str,
) -> Result<Vec<crate::keyring::KeyringEnvelope>> {
    let mut envelopes = Vec::new();
    envelopes.push(seal_repo_secrets(
        &repo_secrets.repo_id,
        repo_secrets.active_epoch,
        password,
        repo_secrets,
        redact_password_hint(password),
    )?);
    for envelope in &current_state.envelopes {
        if envelope.kind != "device" || envelope.recipient_pubkey_hex.is_empty() {
            continue;
        }
        envelopes.push(seal_repo_secrets_for_device(
            &repo_secrets.repo_id,
            repo_secrets.active_epoch,
            &envelope.recipient_pubkey_hex,
            repo_secrets,
            &envelope.actor_id,
            &envelope.device_id,
        )?);
    }
    Ok(envelopes)
}

fn build_epoch_descriptors(repo_secrets: &RepoSecrets) -> Vec<crate::keyring::EpochDescriptor> {
    repo_secrets
        .epoch_keys
        .keys()
        .copied()
        .map(|epoch| crate::keyring::EpochDescriptor {
            epoch,
            status: if epoch == repo_secrets.active_epoch {
                "active".to_string()
            } else {
                "retired".to_string()
            },
        })
        .collect()
}

fn ensure_local_share_admin(control_dir: &Path) -> Result<()> {
    let credential = read_local_device_credential(control_dir)?;
    let keyring = read_current_keyring_state(control_dir)?;
    let actor = keyring
        .actors
        .iter()
        .find(|actor| actor.actor_id == credential.actor_id)
        .with_context(|| format!("share actor not found: {}", credential.actor_id))?;
    ensure!(
        actor.role == "owner_admin",
        "share admin operations require an owner-admin local device"
    );
    Ok(())
}

fn comparable_keyring_state(state: &KeyringState) -> KeyringState {
    let mut comparable = state.clone();
    comparable.generation = 0;
    comparable.envelopes.clear();
    comparable
}

fn merge_actor_records(
    local_state: &KeyringState,
    remote_state: &KeyringState,
) -> Result<Vec<crate::keyring::ActorRecord>> {
    let mut merged: std::collections::BTreeMap<String, crate::keyring::ActorRecord> =
        std::collections::BTreeMap::new();
    for actor in local_state.actors.iter().chain(remote_state.actors.iter()) {
        match merged.get(&actor.actor_id) {
            Some(existing)
                if existing.display_name != actor.display_name || existing.role != actor.role =>
            {
                anyhow::bail!("manual resolution required for actor {}", actor.actor_id);
            }
            Some(_) => {}
            None => {
                merged.insert(actor.actor_id.clone(), actor.clone());
            }
        }
    }
    Ok(merged.into_values().collect())
}

fn merge_device_records(
    local_state: &KeyringState,
    remote_state: &KeyringState,
) -> Result<Vec<crate::keyring::DeviceRecord>> {
    let mut merged: std::collections::BTreeMap<String, crate::keyring::DeviceRecord> =
        std::collections::BTreeMap::new();
    for device in local_state
        .devices
        .iter()
        .chain(remote_state.devices.iter())
    {
        match merged.get(&device.device_id) {
            Some(existing)
                if existing.actor_id != device.actor_id
                    || existing.device_pubkey_hex != device.device_pubkey_hex =>
            {
                anyhow::bail!("manual resolution required for device {}", device.device_id);
            }
            Some(existing) => {
                if existing.label != device.label
                    && existing.status == device.status
                    && existing.status != "revoked"
                {
                    anyhow::bail!("manual resolution required for device {}", device.device_id);
                }
                let merged_status = if existing.status == "revoked" || device.status == "revoked" {
                    "revoked".to_string()
                } else {
                    existing.status.clone()
                };
                let merged_label = if merged_status == "revoked" {
                    existing.label.clone()
                } else {
                    device.label.clone()
                };
                merged.insert(
                    device.device_id.clone(),
                    crate::keyring::DeviceRecord {
                        device_id: device.device_id.clone(),
                        actor_id: device.actor_id.clone(),
                        label: merged_label,
                        device_pubkey_hex: device.device_pubkey_hex.clone(),
                        status: merged_status,
                    },
                );
            }
            None => {
                merged.insert(device.device_id.clone(), device.clone());
            }
        }
    }
    Ok(merged.into_values().collect())
}

fn merge_epoch_descriptors(
    local_state: &KeyringState,
    remote_state: &KeyringState,
    repo_secrets: &RepoSecrets,
) -> Vec<crate::keyring::EpochDescriptor> {
    let mut epochs = std::collections::BTreeSet::new();
    epochs.extend(local_state.epochs.iter().map(|epoch| epoch.epoch));
    epochs.extend(remote_state.epochs.iter().map(|epoch| epoch.epoch));
    epochs.extend(repo_secrets.epoch_keys.keys().copied());
    epochs
        .into_iter()
        .map(|epoch| crate::keyring::EpochDescriptor {
            epoch,
            status: if epoch == repo_secrets.active_epoch {
                "active".to_string()
            } else {
                "retired".to_string()
            },
        })
        .collect()
}

fn select_password_envelope_for_active_epoch(
    local_state: &KeyringState,
    remote_state: &KeyringState,
    active_epoch: u32,
) -> Result<Option<crate::keyring::KeyringEnvelope>> {
    let local_password = local_state
        .envelopes
        .iter()
        .find(|envelope| envelope.kind == "password")
        .cloned();
    let remote_password = remote_state
        .envelopes
        .iter()
        .find(|envelope| envelope.kind == "password")
        .cloned();
    if local_state.active_epoch == active_epoch && local_password.is_some() {
        return Ok(local_password);
    }
    if remote_state.active_epoch == active_epoch && remote_password.is_some() {
        return Ok(remote_password);
    }
    if local_password.is_some() {
        return Ok(local_password);
    }
    Ok(remote_password)
}

fn build_device_envelopes_for_active_devices(
    repo_id: &str,
    devices: &[crate::keyring::DeviceRecord],
    active_epoch: u32,
    repo_secrets: &RepoSecrets,
) -> Result<Vec<crate::keyring::KeyringEnvelope>> {
    let mut envelopes = Vec::new();
    for device in devices {
        if device.status != "active" {
            continue;
        }
        envelopes.push(seal_repo_secrets_for_device(
            repo_id,
            active_epoch,
            &device.device_pubkey_hex,
            repo_secrets,
            &device.actor_id,
            &device.device_id,
        )?);
    }
    Ok(envelopes)
}

fn bootstrap_recipient_repository(
    repo_root: &Path,
    invite: &ShareInvitePayload,
    local_device_label: &str,
) -> Result<()> {
    ensure!(
        !repo_root.join(CONTROL_DIR).exists(),
        "share recipient repository is already initialized"
    );
    let control_dir = repo_root.join(CONTROL_DIR);
    fs::create_dir_all(control_dir.join("objects")).with_context(|| {
        format!(
            "failed to create bootstrap objects directory at {}",
            control_dir.join("objects").display()
        )
    })?;
    fs::create_dir_all(control_dir.join("journal")).with_context(|| {
        format!(
            "failed to create bootstrap journal directory at {}",
            control_dir.join("journal").display()
        )
    })?;
    fs::create_dir_all(control_dir.join("refs")).with_context(|| {
        format!(
            "failed to create bootstrap refs directory at {}",
            control_dir.join("refs").display()
        )
    })?;
    fs::create_dir_all(control_dir.join(KEYRING_DIR)).with_context(|| {
        format!(
            "failed to create bootstrap keyring directory at {}",
            control_dir.join(KEYRING_DIR).display()
        )
    })?;

    let mut credential = invite.bootstrap_credential.clone();
    credential.label = local_device_label.to_string();
    let mut keyring_state = invite.bootstrap_keyring_state.clone();
    if let Some(device) = keyring_state.devices.iter_mut().find(|device| {
        device.device_id == credential.device_id && device.actor_id == credential.actor_id
    }) {
        device.label = credential.label.clone();
        device.device_pubkey_hex = credential.public_key_hex.clone();
    }
    let keyring_pointer = invite.bootstrap_keyring_pointer.clone();

    atomic_write_bytes(
        control_dir.join(LAYOUT_ROOT_FILE),
        &hex::decode(&invite.layout_root_bytes_hex).context("invalid invite layout root bytes")?,
    )?;
    atomic_write_bytes(
        control_dir.join(DEFAULT_REF_FILE),
        &hex::decode(&invite.default_ref_bytes_hex).context("invalid invite default ref bytes")?,
    )?;
    write_keyring_generation_and_pointer(
        &control_dir,
        &keyring_pointer.current,
        &keyring_state,
        &keyring_pointer,
    )?;
    write_local_device_credential(&control_dir, &credential)?;
    let secrets = unlock_repo_secrets_with_local_device(&control_dir)?;
    cache_unlocked_secrets(&control_dir, &secrets);
    Ok(())
}

fn open_object_store(control_dir: &Path) -> Result<DirectLayoutObjectStore> {
    validate_layout_root(control_dir)?;
    let secrets = open_repo_secrets(control_dir)?;
    Ok(open_object_store_with_secrets(control_dir, secrets))
}

fn open_or_unlock_repo_secrets_with_local_device(control_dir: &Path) -> Result<RepoSecrets> {
    match open_repo_secrets(control_dir) {
        Ok(secrets) => Ok(secrets),
        Err(_) => {
            let secrets = unlock_repo_secrets_with_local_device(control_dir)?;
            cache_unlocked_secrets(control_dir, &secrets);
            Ok(secrets)
        }
    }
}

fn open_object_store_with_secrets(
    control_dir: &Path,
    secrets: RepoSecrets,
) -> DirectLayoutObjectStore {
    DirectLayoutObjectStore::new(control_dir, secrets)
}

fn validate_layout_root(control_dir: &Path) -> Result<LayoutRoot> {
    let layout_root: LayoutRoot = read_json(control_dir.join(LAYOUT_ROOT_FILE))?;
    validate_layout_root_value(&layout_root)?;
    Ok(layout_root)
}

pub fn validate_layout_root_value(layout_root: &LayoutRoot) -> Result<()> {
    ensure!(
        layout_root.schema_version == REPO_FORMAT_VERSION,
        "unsupported layout root schema version {}",
        layout_root.schema_version
    );
    ensure!(
        layout_root.layout_id == DIRECT_LAYOUT_ID,
        "unsupported layout id {}",
        layout_root.layout_id
    );
    ensure!(
        layout_root.mapping_policy == DIRECT_MAPPING_POLICY,
        "unsupported layout mapping policy {}",
        layout_root.mapping_policy
    );
    Ok(())
}

fn write_default_ref(control_dir: &Path, secrets: &RepoSecrets, value: &RefRecord) -> Result<()> {
    let plaintext = postcard_to_vec(value).context("failed to encode ref record")?;
    let bytes = encrypt_control_record(secrets, DEFAULT_REF_TOKEN, "ref", &plaintext)?;
    let path = control_dir.join(DEFAULT_REF_FILE);
    atomic_write_bytes(path, &bytes)
}

fn write_branch_ref(control_dir: &Path, secrets: &RepoSecrets, value: &RefRecord) -> Result<()> {
    fs::create_dir_all(control_dir.join(BRANCH_REFS_DIR)).with_context(|| {
        format!(
            "failed to create branch refs directory at {}",
            control_dir.join(BRANCH_REFS_DIR).display()
        )
    })?;
    let plaintext = postcard_to_vec(value).context("failed to encode ref record")?;
    let stable_name = branch_ref_stable_name(&value.ref_token_hex);
    let bytes = encrypt_control_record(secrets, &stable_name, "ref", &plaintext)?;
    let path = branch_ref_path(control_dir, &value.ref_token_hex)?;
    atomic_write_bytes(path, &bytes)
}

fn read_default_ref(control_dir: &Path, secrets: &RepoSecrets) -> Result<RefRecord> {
    let path = control_dir.join(DEFAULT_REF_FILE);
    let bytes =
        std::fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let plaintext = decrypt_control_record(secrets, DEFAULT_REF_TOKEN, "ref", &bytes)?;
    postcard_from_bytes(&plaintext).context("failed to decode ref record")
}

pub(crate) fn decode_default_ref_bytes(control_dir: &Path, bytes: &[u8]) -> Result<RefRecord> {
    let secrets = open_repo_secrets(control_dir)?;
    let plaintext = decrypt_control_record(&secrets, DEFAULT_REF_TOKEN, "ref", bytes)?;
    postcard_from_bytes(&plaintext).context("failed to decode ref record")
}

pub(crate) fn verify_snapshot_with_secrets_for_sync(
    repo_root: impl AsRef<Path>,
    secrets: RepoSecrets,
    snapshot_id: &str,
) -> Result<()> {
    let repo_root = repo_root.as_ref().to_path_buf();
    let control_dir = repo_root.join(CONTROL_DIR);
    let _layout_root = validate_layout_root(&control_dir)?;
    let secrets_were_cached = open_repo_secrets(&control_dir).is_ok();
    if !secrets_were_cached {
        cache_unlocked_secrets(&control_dir, &secrets);
    }
    let object_store = open_object_store_with_secrets(&control_dir, secrets);
    let manifest_store = LocalManifestStore::new(&repo_root);
    let result = verify_snapshot_graph(&manifest_store, &object_store, snapshot_id);
    if !secrets_were_cached {
        crate::keyring::clear_unlocked_keyring_cache(&control_dir);
    }
    result
}

fn read_current_ref(control_dir: &Path) -> Result<RefRecord> {
    let secrets = open_repo_secrets(control_dir)?;
    read_default_ref(control_dir, &secrets)
}

fn branch_fast_forward_allowed(
    repo_root: &Path,
    previous_head_snapshot_id: Option<&str>,
    next_head_snapshot_id: Option<&str>,
) -> Result<bool> {
    let Some(next_head_snapshot_id) = next_head_snapshot_id else {
        return Ok(previous_head_snapshot_id.is_none());
    };
    let Some(previous_head_snapshot_id) = previous_head_snapshot_id else {
        return Ok(true);
    };
    if previous_head_snapshot_id == next_head_snapshot_id {
        return Ok(true);
    }

    let manifest_store = LocalManifestStore::new(repo_root);
    let mut next_parent = Some(next_head_snapshot_id.to_string());
    while let Some(snapshot_id) = next_parent {
        if snapshot_id == previous_head_snapshot_id {
            return Ok(true);
        }
        let snapshot = manifest_store.get_snapshot(&snapshot_id)?;
        next_parent = snapshot.parent_snapshot_id;
    }
    Ok(false)
}

fn read_branch_ref_if_exists(
    control_dir: &Path,
    secrets: &RepoSecrets,
    ref_token_hex: &str,
) -> Result<Option<RefRecord>> {
    let path = branch_ref_path(control_dir, ref_token_hex)?;
    if !path.is_file() {
        return Ok(None);
    }
    let bytes = fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let plaintext = decrypt_control_record(
        secrets,
        &branch_ref_stable_name(ref_token_hex),
        "ref",
        &bytes,
    )?;
    let record: RefRecord =
        postcard_from_bytes(&plaintext).context("failed to decode branch ref record")?;
    ensure!(
        record.ref_token_hex == ref_token_hex,
        "branch ref token mismatch"
    );
    Ok(Some(record))
}

fn read_all_branch_refs(control_dir: &Path, secrets: &RepoSecrets) -> Result<Vec<RefRecord>> {
    let refs_dir = control_dir.join(BRANCH_REFS_DIR);
    if !refs_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut refs = Vec::new();
    for entry in fs::read_dir(&refs_dir).with_context(|| {
        format!(
            "failed to read branch refs directory {}",
            refs_dir.display()
        )
    })? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Some(token_hex) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        if let Some(branch_ref) = read_branch_ref_if_exists(control_dir, secrets, token_hex)? {
            refs.push(branch_ref);
        }
    }
    Ok(refs)
}

fn branch_ref_path(control_dir: &Path, ref_token_hex: &str) -> Result<PathBuf> {
    validate_ref_token_value(ref_token_hex)?;
    ensure!(
        Path::new(ref_token_hex).components().count() == 1,
        "branch token must be a single path segment"
    );
    Ok(control_dir
        .join(BRANCH_REFS_DIR)
        .join(format!("{ref_token_hex}.json")))
}

fn branch_ref_stable_name(ref_token_hex: &str) -> String {
    format!("branch-ref:{ref_token_hex}")
}

pub(crate) fn encrypt_control_record(
    secrets: &RepoSecrets,
    stable_name: &str,
    object_type: &str,
    plaintext: &[u8],
) -> Result<Vec<u8>> {
    let object_id = derive_control_object_id(secrets, stable_name, object_type, plaintext);
    let nonce = derive_control_nonce(secrets, &object_id, object_type);
    let epoch_keys = secrets.active_epoch_keys()?;
    let cipher = XChaCha20Poly1305::new((&epoch_keys.manifest_enc_key).into());
    let mut ciphertext = plaintext.to_vec();
    let associated_data =
        control_record_associated_data(secrets, &object_id, object_type, secrets.active_epoch);
    let auth_tag = cipher
        .encrypt_in_place_detached(
            XNonce::from_slice(&nonce),
            &associated_data,
            &mut ciphertext,
        )
        .map_err(|_| anyhow::anyhow!("failed to encrypt local ref"))?;

    let envelope = EncryptedControlRecord {
        magic: *CONTROL_REF_MAGIC,
        format_version: CONTROL_REF_FORMAT_VERSION,
        object_type: object_type.to_string(),
        crypto_suite: "xchacha20poly1305".to_string(),
        key_epoch: secrets.active_epoch,
        object_id,
        nonce: nonce.to_vec(),
        ciphertext,
        auth_tag: auth_tag.to_vec(),
    };

    postcard_to_vec(&envelope).context("failed to encode encrypted ref")
}

pub(crate) fn decrypt_control_record(
    secrets: &RepoSecrets,
    stable_name: &str,
    object_type: &str,
    bytes: &[u8],
) -> Result<Vec<u8>> {
    let envelope: EncryptedControlRecord =
        postcard_from_bytes(bytes).context("failed to decode encrypted ref")?;
    ensure!(
        envelope.magic == *CONTROL_REF_MAGIC,
        "ref authentication failed"
    );
    ensure!(
        envelope.format_version == CONTROL_REF_FORMAT_VERSION,
        "unsupported ref format version"
    );
    ensure!(
        envelope.object_type == object_type,
        "ref authentication failed"
    );
    ensure!(
        envelope.crypto_suite == "xchacha20poly1305",
        "unsupported ref crypto suite"
    );
    ensure!(envelope.nonce.len() == 24, "ref authentication failed");
    ensure!(envelope.auth_tag.len() == 16, "ref authentication failed");

    let epoch_keys = secrets.epoch_keys(envelope.key_epoch)?;
    let cipher = XChaCha20Poly1305::new((&epoch_keys.manifest_enc_key).into());
    let mut plaintext = envelope.ciphertext.clone();
    let associated_data = control_record_associated_data(
        secrets,
        &envelope.object_id,
        object_type,
        envelope.key_epoch,
    );

    cipher
        .decrypt_in_place_detached(
            XNonce::from_slice(&envelope.nonce),
            &associated_data,
            &mut plaintext,
            Tag::from_slice(&envelope.auth_tag),
        )
        .map_err(|_| anyhow::anyhow!("ref authentication failed"))?;

    let expected_object_id =
        derive_control_object_id(secrets, stable_name, object_type, &plaintext);
    ensure!(
        expected_object_id == envelope.object_id,
        "ref authentication failed"
    );

    Ok(plaintext)
}

fn derive_control_object_id(
    secrets: &RepoSecrets,
    stable_name: &str,
    object_type: &str,
    plaintext: &[u8],
) -> String {
    let mut input = Vec::new();
    input.extend_from_slice(stable_name.as_bytes());
    input.extend_from_slice(object_type.as_bytes());
    input.extend_from_slice(&(plaintext.len() as u64).to_le_bytes());
    input.extend_from_slice(plaintext);
    hex::encode(blake3::keyed_hash(&secrets.repo_dedup_key, &input).as_bytes())
}

fn derive_control_nonce(secrets: &RepoSecrets, object_id: &str, object_type: &str) -> [u8; 24] {
    let epoch_keys = secrets
        .active_epoch_keys()
        .expect("active epoch keys must be present");
    let mut hasher = Hasher::new_keyed(&epoch_keys.nonce_key);
    hasher.update(object_id.as_bytes());
    hasher.update(object_type.as_bytes());
    hasher.update(b"control-record");
    let hash = hasher.finalize();
    let mut nonce = [0u8; 24];
    nonce.copy_from_slice(&hash.as_bytes()[..24]);
    nonce
}

fn control_record_associated_data(
    secrets: &RepoSecrets,
    object_id: &str,
    object_type: &str,
    key_epoch: u32,
) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(CONTROL_REF_MAGIC);
    data.extend_from_slice(&CONTROL_REF_FORMAT_VERSION.to_le_bytes());
    data.extend_from_slice(secrets.repo_id.as_bytes());
    data.extend_from_slice(object_type.as_bytes());
    data.extend_from_slice(object_id.as_bytes());
    data.extend_from_slice(b"xchacha20poly1305");
    data.extend_from_slice(&key_epoch.to_le_bytes());
    data
}

fn build_tree_object(
    object_store: &DirectLayoutObjectStore,
    working_tree: &WorkingTree,
    current_dir: &Path,
    committed_files: &mut usize,
    new_bytes: &mut u64,
    reused_bytes: &mut u64,
    warnings: &mut Vec<String>,
) -> Result<String> {
    let scanned_entries = working_tree.scan_dir(current_dir, true)?;
    let mut tree_entries = Vec::new();

    for entry in scanned_entries {
        if entry.is_dir {
            let tree_id = build_tree_object(
                object_store,
                working_tree,
                &entry.path,
                committed_files,
                new_bytes,
                reused_bytes,
                warnings,
            )?;
            tree_entries.push(TreeEntry {
                name: entry.name,
                kind: "tree".to_string(),
                object_id: tree_id,
            });
        } else {
            let maybe_tree_entry = build_file_tree_entry_with(
                object_store,
                &entry,
                committed_files,
                new_bytes,
                reused_bytes,
                warnings,
                |path| {
                    let bytes = working_tree.open_stable_file(path)?;
                    let modified_unix_ms = std::fs::metadata(path)
                        .with_context(|| {
                            format!("failed to stat working tree file {}", path.display())
                        })?
                        .modified()
                        .with_context(|| {
                            format!("failed to read modified time for {}", path.display())
                        })?
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis()
                        .min(u128::from(u64::MAX))
                        as u64;
                    Ok((bytes, modified_unix_ms))
                },
            )?;
            if let Some(tree_entry) = maybe_tree_entry {
                tree_entries.push(tree_entry);
            }
        }
    }

    tree_entries.sort_by(|left, right| left.name.cmp(&right.name));
    if tree_entries.len() <= MAX_TREE_ENTRIES_PER_OBJECT {
        return write_object(
            object_store,
            "tree",
            &TreeObject {
                schema_version: REPO_FORMAT_VERSION,
                entries: tree_entries,
            },
        );
    }

    build_directory_root_object(object_store, tree_entries)
}

fn build_file_tree_entry_with<F>(
    object_store: &DirectLayoutObjectStore,
    entry: &WorkingTreeEntry,
    committed_files: &mut usize,
    new_bytes: &mut u64,
    reused_bytes: &mut u64,
    warnings: &mut Vec<String>,
    mut read_file: F,
) -> Result<Option<TreeEntry>>
where
    F: FnMut(&PathBuf) -> Result<(Vec<u8>, u64)>,
{
    let chunker = FastCdcChunker;
    let (file_bytes, modified_unix_ms) = match read_file(&entry.path) {
        Ok(values) => values,
        Err(error) => {
            if error.to_string().contains("unstable input") {
                warnings.push(format!(
                    "skipped unstable input {}: {}",
                    entry.path.display(),
                    error
                ));
            } else {
                warnings.push(format!(
                    "skipped unreadable file {}: {}",
                    entry.path.display(),
                    error
                ));
            }
            return Ok(None);
        }
    };
    let chunk_spans = chunker.split_spans(&file_bytes)?;
    let mut chunk_ids = Vec::with_capacity(chunk_spans.len());
    let mut chunk_lengths = Vec::with_capacity(chunk_spans.len());

    for span in chunk_spans {
        let piece_bytes = file_bytes[span.offset..span.end()].to_vec();
        let piece_len = piece_bytes.len();
        let chunk = ChunkObject {
            plaintext_length: piece_len,
            data: piece_bytes,
        };
        let chunk_bytes = postcard_to_vec(&chunk).context("failed to encode chunk")?;
        let chunk_id = object_store.preview_object_id("chunk", &chunk_bytes);
        let already_exists = object_store.exists_object(&chunk_id);
        let chunk_id = object_store.put_object("chunk", &chunk_bytes)?;
        if already_exists {
            *reused_bytes += piece_len as u64;
        } else {
            *new_bytes += piece_len as u64;
        }
        chunk_ids.push(chunk_id);
        chunk_lengths.push(piece_len as u64);
    }

    let total_chunk_count = chunk_ids.len() as u64;
    let (inline_chunk_ids, inline_chunk_lengths, shard_ids, shard_byte_lengths) =
        if chunk_ids.len() > max_file_chunks_per_object() {
            let mut shard_ids = Vec::new();
            let mut shard_byte_lengths = Vec::new();
            let mut start = 0usize;
            while start < chunk_ids.len() {
                let end = (start + max_file_chunks_per_object()).min(chunk_ids.len());
                let shard = FileShardObject {
                    schema_version: REPO_FORMAT_VERSION,
                    chunks: chunk_ids[start..end].to_vec(),
                    chunk_lengths: chunk_lengths[start..end].to_vec(),
                };
                shard_byte_lengths.push(chunk_lengths[start..end].iter().sum());
                shard_ids.push(write_object(object_store, "file_shard", &shard)?);
                start = end;
            }
            (Vec::new(), Vec::new(), shard_ids, shard_byte_lengths)
        } else {
            (chunk_ids, chunk_lengths, Vec::new(), Vec::new())
        };

    let file_object = FileObject {
        schema_version: REPO_FORMAT_VERSION,
        entry_name: entry.name.clone(),
        file_size: file_bytes.len() as u64,
        modified_unix_ms,
        chunker_id: chunker.id().to_string(),
        chunker_config_id: chunker.config_fingerprint().to_string(),
        chunk_count: total_chunk_count,
        chunks: inline_chunk_ids,
        chunk_lengths: inline_chunk_lengths,
        shard_ids,
        shard_byte_lengths,
    };
    let file_id = write_object(object_store, "file", &file_object)?;
    *committed_files += 1;

    Ok(Some(TreeEntry {
        name: entry.name.clone(),
        kind: "file".to_string(),
        object_id: file_id,
    }))
}

fn resolve_tree_for_path(
    manifest_store: &LocalManifestStore,
    root_tree_id: &str,
    path: &str,
) -> Result<String> {
    let normalized = normalize_snapshot_path(path);
    if normalized.is_empty() {
        return Ok(root_tree_id.to_string());
    }

    let mut current_tree_id = root_tree_id.to_string();
    for segment in normalized.split('/') {
        ensure!(!segment.is_empty(), "invalid snapshot path: {path}");
        ensure!(
            segment != "." && segment != "..",
            "invalid snapshot path: {path}"
        );
        let tree = manifest_store.get_tree_node(&current_tree_id)?;
        let next = tree
            .entries
            .into_iter()
            .find(|entry| entry.name == segment && entry.kind == "tree")
            .with_context(|| format!("directory not found in snapshot: {path}"))?;
        current_tree_id = next.object_id;
    }

    Ok(current_tree_id)
}

fn split_parent_and_name(path: &str) -> Result<(String, String)> {
    let normalized_string = normalize_snapshot_path(path);
    let normalized = normalized_string.as_str();
    ensure!(!normalized.is_empty(), "snapshot path must not be empty");
    match normalized.rsplit_once('/') {
        Some((parent, name)) => Ok((parent.to_string(), name.to_string())),
        None => Ok((String::new(), normalized.to_string())),
    }
}

fn normalize_snapshot_path(path: &str) -> String {
    path.trim_matches('/')
        .split('/')
        .map(|component| component.nfc().collect::<String>())
        .collect::<Vec<_>>()
        .join("/")
}

fn collect_checkout_file_paths(
    read_service: &ReadService,
    snapshot: &SnapshotHandle,
    snapshot_path: &str,
) -> Result<Vec<(String, FileHandle)>> {
    let entries = read_service.read_dir(snapshot, snapshot_path)?;
    let mut files = Vec::new();
    let path_validator = WorkingTree::new("D:\\manifest-path-validator");

    for entry in entries {
        let child_snapshot_path = join_snapshot_path(snapshot_path, &entry.name);
        path_validator.path_jail_validate(&child_snapshot_path)?;

        if entry.kind == "tree" {
            files.extend(collect_checkout_file_paths(
                read_service,
                snapshot,
                &child_snapshot_path,
            )?);
            continue;
        }

        let file = read_service.open_file(snapshot, &child_snapshot_path)?;
        files.push((child_snapshot_path, file));
    }

    Ok(files)
}

fn join_snapshot_path(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

fn write_object<T: Serialize>(
    object_store: &DirectLayoutObjectStore,
    object_type: &str,
    plaintext: &T,
) -> Result<String> {
    let plaintext_bytes =
        postcard_to_vec(plaintext).context("failed to encode object plaintext")?;
    object_store.put_object(object_type, &plaintext_bytes)
}

fn read_stored_object<T: for<'de> Deserialize<'de>>(
    object_store: &DirectLayoutObjectStore,
    object_id: &str,
    expected_type: &str,
) -> Result<T> {
    let plaintext = object_store.get_object(object_id, expected_type)?;
    postcard_from_bytes(&plaintext).context("failed to decode object plaintext")
}

fn read_snapshot_object(
    object_store: &DirectLayoutObjectStore,
    object_id: &str,
) -> Result<SnapshotObject> {
    let snapshot: SnapshotObject = read_stored_object(object_store, object_id, "snapshot")?;
    validate_manifest_schema_version_local("snapshot", snapshot.schema_version)?;
    Ok(snapshot)
}

#[allow(dead_code)]
#[cfg(test)]
fn read_tree_object(object_store: &DirectLayoutObjectStore, object_id: &str) -> Result<TreeObject> {
    let tree: TreeObject = read_stored_object(object_store, object_id, "tree")?;
    validate_manifest_schema_version("tree", tree.schema_version)?;
    Ok(tree)
}

fn read_file_object(object_store: &DirectLayoutObjectStore, object_id: &str) -> Result<FileObject> {
    let file: FileObject = read_stored_object(object_store, object_id, "file")?;
    validate_manifest_schema_version_local("file", file.schema_version)?;
    Ok(file)
}

struct ChunkReadContext<'a> {
    output: &'a mut Vec<u8>,
    object_store: &'a DirectLayoutObjectStore,
    repo_root: &'a Path,
    file: &'a FileHandle,
    offset: usize,
    end: usize,
    chunk_start: &'a mut usize,
}

fn append_requested_bytes_from_chunks(
    context: &mut ChunkReadContext<'_>,
    chunk_ids: &[String],
    chunk_lengths: &[u64],
) -> Result<()> {
    for (chunk_id, chunk_length) in chunk_ids.iter().zip(chunk_lengths.iter()) {
        if context.output.len() >= context.end - context.offset {
            break;
        }

        let chunk_len: usize = (*chunk_length)
            .try_into()
            .map_err(|_| anyhow::anyhow!("chunk length does not fit in usize"))?;
        let chunk_end = context.chunk_start.saturating_add(chunk_len);
        if chunk_end <= context.offset {
            *context.chunk_start = chunk_end;
            continue;
        }
        if *context.chunk_start >= context.end {
            break;
        }

        let chunk = if context.object_store.exists_object(chunk_id) {
            read_chunk_object(context.object_store, chunk_id)?
        } else {
            let fallback_bytes =
                crate::sync_support::read_cached_pack_object_bytes(context.repo_root, chunk_id)
                    .map_err(|_| {
                        anyhow::anyhow!(
                            "stale-layout fallback unavailable for missing chunk {chunk_id} at layout generation {}",
                            context.file.layout_generation
                        )
                    })?;
            let fallback_plaintext = crate::sync_support::decode_object_bytes_for_sync(
                context.repo_root,
                chunk_id,
                "chunk",
                &fallback_bytes,
            )
            .map_err(|_| {
                anyhow::anyhow!(
                    "stale-layout fallback unavailable for missing chunk {chunk_id} at layout generation {}",
                    context.file.layout_generation
                )
            })?;
            let chunk: ChunkObject = postcard_from_bytes(&fallback_plaintext)
                .context("failed to decode object plaintext")?;
            chunk
        };
        ensure!(
            chunk.data.len() == chunk_len,
            "file chunk metadata does not match chunk payload length"
        );
        let slice_start = context.offset.saturating_sub(*context.chunk_start);
        let slice_end = (context.end - *context.chunk_start).min(chunk_len);
        context
            .output
            .extend_from_slice(&chunk.data[slice_start..slice_end]);
        *context.chunk_start = chunk_end;
    }
    Ok(())
}

#[cfg(test)]
fn read_directory_root_object(
    object_store: &DirectLayoutObjectStore,
    object_id: &str,
) -> Result<DirectoryRootObject> {
    let directory_root: DirectoryRootObject =
        read_stored_object(object_store, object_id, "directory_root")?;
    validate_manifest_schema_version("directory_root", directory_root.schema_version)?;
    Ok(directory_root)
}

#[allow(dead_code)]
fn reserved_manifest_types() -> &'static [&'static str] {
    RESERVED_MANIFEST_TYPES
}

fn read_chunk_object(
    object_store: &DirectLayoutObjectStore,
    object_id: &str,
) -> Result<ChunkObject> {
    read_stored_object(object_store, object_id, "chunk")
}

fn verify_snapshot_graph(
    manifest_store: &LocalManifestStore,
    object_store: &DirectLayoutObjectStore,
    snapshot_id: &str,
) -> Result<()> {
    let mut visited_snapshots = std::collections::BTreeSet::new();
    let mut next_snapshot_id = Some(snapshot_id.to_string());
    while let Some(current_snapshot_id) = next_snapshot_id {
        ensure!(
            visited_snapshots.insert(current_snapshot_id.clone()),
            "verify snapshot failed: snapshot parent cycle detected at {current_snapshot_id}"
        );
        let snapshot = manifest_store.get_snapshot(&current_snapshot_id)?;
        verify_tree_graph(manifest_store, object_store, &snapshot.root_tree_id)?;
        next_snapshot_id = snapshot.parent_snapshot_id;
    }
    Ok(())
}

fn verify_tree_graph(
    manifest_store: &LocalManifestStore,
    object_store: &DirectLayoutObjectStore,
    tree_id: &str,
) -> Result<()> {
    let tree = manifest_store.get_tree_node(tree_id)?;
    for entry in tree.entries {
        match entry.kind.as_str() {
            "tree" => verify_tree_graph(manifest_store, object_store, &entry.object_id)?,
            "file" => verify_file_graph(manifest_store, object_store, &entry.object_id)?,
            other => anyhow::bail!("verify snapshot failed: unknown tree entry kind {other}"),
        }
    }
    Ok(())
}

fn build_directory_root_object(
    object_store: &DirectLayoutObjectStore,
    tree_entries: Vec<TreeEntry>,
) -> Result<String> {
    let mut shard_ids = Vec::new();
    let mut start = 0usize;

    while start < tree_entries.len() {
        let end = (start + MAX_TREE_ENTRIES_PER_OBJECT).min(tree_entries.len());
        let shard_entries = tree_entries[start..end].to_vec();
        let range_start = shard_entries
            .first()
            .map(|entry| entry.name.clone())
            .unwrap_or_default();
        let range_end = shard_entries
            .last()
            .map(|entry| entry.name.clone())
            .unwrap_or_default();

        let shard = TreeShardObject {
            schema_version: REPO_FORMAT_VERSION,
            range_start,
            range_end,
            entries: shard_entries,
        };
        shard_ids.push(write_object(object_store, "tree_shard", &shard)?);
        start = end;
    }

    write_object(
        object_store,
        "directory_root",
        &DirectoryRootObject {
            schema_version: REPO_FORMAT_VERSION,
            fanout: shard_ids.len() as u32,
            shards: shard_ids,
        },
    )
}

fn verify_file_graph(
    manifest_store: &LocalManifestStore,
    object_store: &DirectLayoutObjectStore,
    file_id: &str,
) -> Result<()> {
    let file = manifest_store.get_file(file_id)?;
    for chunk_id in file.chunks {
        let _ = read_chunk_object(object_store, &chunk_id)?;
    }
    for shard_id in file.shard_ids {
        let shard: FileShardObject = read_stored_object(object_store, &shard_id, "file_shard")?;
        validate_manifest_schema_version_local("file_shard", shard.schema_version)?;
        ensure!(
            shard.chunks.len() == shard.chunk_lengths.len(),
            "file shard metadata is inconsistent"
        );
        for chunk_id in shard.chunks {
            let _ = read_chunk_object(object_store, &chunk_id)?;
        }
    }
    Ok(())
}

fn validate_manifest_schema_version_local(object_type: &str, schema_version: u32) -> Result<()> {
    ensure!(
        schema_version == REPO_FORMAT_VERSION,
        "unsupported manifest schema version for {object_type}: {schema_version}"
    );
    Ok(())
}

#[cfg(test)]
fn validate_manifest_schema_version(object_type: &str, schema_version: u32) -> Result<()> {
    ensure!(
        schema_version == REPO_FORMAT_VERSION,
        "unsupported manifest schema version for {object_type}: {schema_version}"
    );
    Ok(())
}

fn atomic_write_bytes(path: PathBuf, bytes: &[u8]) -> Result<()> {
    let temp_path = path.with_extension(format!(
        "{}.tmp",
        path.extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("tmp")
    ));
    fs::write(&temp_path, bytes)
        .with_context(|| format!("failed to write {}", temp_path.display()))?;
    sync_path(&temp_path)?;
    let read_back = fs::read(&temp_path)
        .with_context(|| format!("failed to read back {}", temp_path.display()))?;
    ensure!(
        read_back == bytes,
        "read-back validation failed for {}",
        temp_path.display()
    );
    fs::rename(&temp_path, &path)
        .with_context(|| format!("failed to publish {}", path.display()))?;
    sync_path(&path)?;
    if let Some(parent) = path.parent() {
        sync_path(parent)?;
    }
    Ok(())
}

fn sync_path(path: &Path) -> Result<()> {
    #[cfg(windows)]
    let file = {
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x02000000;
        let metadata = std::fs::metadata(path)
            .with_context(|| format!("failed to stat {} for sync", path.display()))?;
        let mut options = std::fs::OpenOptions::new();
        if metadata.is_dir() {
            options
                .access_mode(0)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS);
        } else {
            options.read(true).write(true);
        }
        let file = options
            .open(path)
            .with_context(|| format!("failed to open {} for sync", path.display()))?;
        if metadata.is_dir() {
            match file.sync_all() {
                Ok(()) => return Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return Ok(()),
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to sync {}", path.display()));
                }
            }
        }
        file
    };
    #[cfg(not(windows))]
    let file = std::fs::OpenOptions::new()
        .read(true)
        .open(path)
        .with_context(|| format!("failed to open {} for sync", path.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to sync {}", path.display()))?;
    Ok(())
}

fn atomic_write_json<T: Serialize>(path: PathBuf, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value).context("failed to encode json")?;
    atomic_write_bytes(path, &bytes)
}

fn write_keyring_generation_and_pointer(
    control_dir: &Path,
    generation_file_name: &str,
    keyring_state: &KeyringState,
    keyring_pointer: &KeyringPointer,
) -> Result<()> {
    let journal_path = control_dir.join(JOURNAL_DIR).join("keyring-update.json");
    atomic_write_json(
        journal_path.clone(),
        &serde_json::json!({
            "generation": keyring_state.generation,
            "current": keyring_pointer.current,
            "stage": "writing_generation",
        }),
    )?;
    let result = with_keyring_lock(&control_dir.join(KEYRING_DIR), || {
        atomic_write_json(
            control_dir.join(KEYRING_DIR).join(generation_file_name),
            keyring_state,
        )?;
        atomic_write_json(
            journal_path.clone(),
            &serde_json::json!({
                "generation": keyring_state.generation,
                "current": keyring_pointer.current,
                "stage": "writing_pointer",
            }),
        )?;
        atomic_write_json(control_dir.join(KEYRING_CURRENT_FILE), keyring_pointer)?;
        Ok(())
    });
    match result {
        Ok(()) => {
            let _ = fs::remove_file(&journal_path);
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn with_keyring_lock<T, F>(keyring_dir: &Path, f: F) -> Result<T>
where
    F: FnOnce() -> Result<T>,
{
    let lock_path = keyring_dir.join("keyring.lock");
    ensure!(
        !lock_path.exists(),
        "keyring update blocked by local lock {}",
        lock_path.display()
    );
    fs::write(&lock_path, b"locked")
        .with_context(|| format!("failed to create keyring lock {}", lock_path.display()))?;
    let result = f();
    let cleanup_result = fs::remove_file(&lock_path)
        .with_context(|| format!("failed to remove keyring lock {}", lock_path.display()));
    match (result, cleanup_result) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup_error)) => Err(cleanup_error),
        (Err(error), Err(_cleanup_error)) => Err(error),
    }
}

fn read_json<T: for<'de> Deserialize<'de>>(path: PathBuf) -> Result<T> {
    let bytes = fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("failed to decode {}", path.display()))
}

#[cfg(test)]
mod facade_tests {
    use super::*;
    use crate::keyring::unlock_repo_secrets_uncached;
    use crate::working_tree::WorkingTreeEntry;
    use std::cell::RefCell;
    use tempfile::tempdir;

    fn init_repo(temp_name: &str) -> (PathBuf, DirectLayoutObjectStore) {
        let temp = tempdir().unwrap();
        let repo_root = temp.path().join(temp_name);
        fs::create_dir_all(&repo_root).unwrap();
        let facade = RepositoryFacade::new();
        facade
            .init(InitOptions {
                repo_root: repo_root.clone(),
                password: "correct horse battery staple".to_string(),
                branch_name: "main".to_string(),
            })
            .unwrap();
        let control_dir = repo_root.join(CONTROL_DIR);
        let store = open_object_store(&control_dir).unwrap();
        (repo_root, store)
    }

    #[test]
    fn read_file_object_rejects_wrong_manifest_schema_version() {
        let (_repo_root, store) = init_repo("repo");
        let file_id = write_object(
            &store,
            "file",
            &FileObject {
                schema_version: REPO_FORMAT_VERSION + 1,
                entry_name: "hello.txt".to_string(),
                file_size: 5,
                modified_unix_ms: 1,
                chunker_id: "fastcdc".to_string(),
                chunker_config_id: "fastcdc-64k-1m-8m".to_string(),
                chunk_count: 0,
                chunks: vec![],
                chunk_lengths: vec![],
                shard_ids: vec![],
                shard_byte_lengths: vec![],
            },
        )
        .unwrap();

        let error = read_file_object(&store, &file_id).unwrap_err();
        assert!(
            error.to_string().contains("schema version"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn reserved_directory_shard_manifest_types_exist_in_schema_boundary() {
        assert!(reserved_manifest_types().contains(&"directory_root"));
        assert!(reserved_manifest_types().contains(&"tree_shard"));
        let reserved = DirectoryRootObject {
            schema_version: REPO_FORMAT_VERSION,
            fanout: 16,
            shards: vec!["shard-1".to_string()],
        };
        let shard = TreeShardObject {
            schema_version: REPO_FORMAT_VERSION,
            range_start: "a".to_string(),
            range_end: "c".to_string(),
            entries: vec![],
        };

        let reserved_bytes = postcard_to_vec(&reserved).unwrap();
        let shard_bytes = postcard_to_vec(&shard).unwrap();

        assert!(!reserved_bytes.is_empty());
        assert!(!shard_bytes.is_empty());
    }

    #[test]
    fn read_directory_root_object_round_trips_when_supported() {
        let (_repo_root, store) = init_repo("repo");
        let object_id = write_object(
            &store,
            "directory_root",
            &DirectoryRootObject {
                schema_version: REPO_FORMAT_VERSION,
                fanout: 16,
                shards: vec!["shard-1".to_string()],
            },
        )
        .unwrap();

        let object = read_directory_root_object(&store, &object_id).unwrap();
        assert_eq!(object.fanout, 16);
    }

    #[test]
    fn build_tree_object_records_warning_and_skips_failed_file_reads() {
        let (_repo_root, store) = init_repo("repo");
        let failing_path = PathBuf::from("C:\\virtual\\bad.txt");
        let ok_path = PathBuf::from("C:\\virtual\\good.txt");
        let entries = [
            WorkingTreeEntry {
                name: "bad.txt".to_string(),
                path: failing_path.clone(),
                is_dir: false,
            },
            WorkingTreeEntry {
                name: "good.txt".to_string(),
                path: ok_path.clone(),
                is_dir: false,
            },
        ];
        let mut warnings: Vec<String> = Vec::new();
        let mut committed_files = 0usize;
        let mut new_bytes = 0u64;
        let mut reused_bytes = 0u64;

        let tree_entry = build_file_tree_entry_with(
            &store,
            &entries[0],
            &mut committed_files,
            &mut new_bytes,
            &mut reused_bytes,
            &mut warnings,
            |path| {
                if path == &failing_path {
                    Err(anyhow::anyhow!("simulated read failure"))
                } else {
                    Ok((b"good".to_vec(), 1))
                }
            },
        )
        .unwrap();

        assert!(tree_entry.is_none());
        let ok_entry = build_file_tree_entry_with(
            &store,
            &entries[1],
            &mut committed_files,
            &mut new_bytes,
            &mut reused_bytes,
            &mut warnings,
            |_path| Ok((b"good".to_vec(), 1)),
        )
        .unwrap();

        assert_eq!(committed_files, 1);
        assert_eq!(new_bytes, 4);
        assert_eq!(reused_bytes, 0);
        assert_eq!(ok_entry.unwrap().name, "good.txt");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("bad.txt"));
    }

    #[test]
    fn build_tree_object_classifies_unstable_input_warnings() {
        let (_repo_root, store) = init_repo("repo");
        let unstable_path = PathBuf::from("C:\\virtual\\volatile.db");
        let entry = WorkingTreeEntry {
            name: "volatile.db".to_string(),
            path: unstable_path.clone(),
            is_dir: false,
        };
        let mut warnings: Vec<String> = Vec::new();
        let mut committed_files = 0usize;
        let mut new_bytes = 0u64;
        let mut reused_bytes = 0u64;

        let tree_entry = build_file_tree_entry_with(
            &store,
            &entry,
            &mut committed_files,
            &mut new_bytes,
            &mut reused_bytes,
            &mut warnings,
            |_path| {
                Err(anyhow::anyhow!(
                    "unstable input: volatile source retry budget exhausted"
                ))
            },
        )
        .unwrap();

        assert!(tree_entry.is_none());
        assert_eq!(committed_files, 0);
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].contains("unstable input"),
            "unexpected warning: {}",
            warnings[0]
        );
        assert!(
            !warnings[0].contains("unreadable"),
            "unexpected warning: {}",
            warnings[0]
        );
    }

    #[test]
    fn verify_snapshot_with_explicit_secrets_does_not_require_cached_unlock_state() {
        let temp = tempdir().unwrap();
        let repo_root = temp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();

        let facade = RepositoryFacade::new();
        facade
            .init(InitOptions {
                repo_root: repo_root.clone(),
                password: "correct horse battery staple".to_string(),
                branch_name: "main".to_string(),
            })
            .unwrap();
        fs::write(repo_root.join("hello.txt"), b"hello").unwrap();
        let commit = facade
            .commit(CommitOptions {
                repo_root: repo_root.clone(),
                message: "seed".to_string(),
            })
            .unwrap();

        let control_dir = repo_root.join(CONTROL_DIR);
        crate::keyring::clear_unlocked_keyring_cache(&control_dir);
        let secrets =
            unlock_repo_secrets_uncached(&control_dir, "correct horse battery staple").unwrap();
        crate::keyring::clear_unlocked_keyring_cache(&control_dir);

        verify_snapshot_with_secrets_for_sync(&repo_root, secrets, &commit.snapshot_id).unwrap();
        assert!(open_repo_secrets(&control_dir).is_err());
    }

    #[test]
    fn branch_token_depends_on_repo_ref_key() {
        let repo_ref_key_a = [7u8; 32];
        let repo_ref_key_b = [8u8; 32];

        let left = derive_branch_token(&repo_ref_key_a, "main");
        let right = derive_branch_token(&repo_ref_key_b, "main");

        assert_ne!(left, right);
    }

    #[test]
    fn open_repo_secrets_follows_current_keyring_pointer() {
        let temp = tempdir().unwrap();
        let repo_root = temp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();
        let facade = RepositoryFacade::new();
        facade
            .init(InitOptions {
                repo_root: repo_root.clone(),
                password: "correct horse battery staple".to_string(),
                branch_name: "main".to_string(),
            })
            .unwrap();
        let control_dir = repo_root.join(CONTROL_DIR);
        let current_pointer_path = control_dir.join(KEYRING_CURRENT_FILE);
        let generation_two_path = control_dir.join("keyring").join("keyring.2");
        let generation_one_path = control_dir.join(KEYRING_DIR).join("keyring.1");

        let secrets_two = RepoSecrets {
            repo_id: derive_repo_id(&repo_root),
            active_epoch: DEFAULT_ACTIVE_EPOCH,
            repo_dedup_key: [1u8; 32],
            repo_ref_key: [9u8; 32],
            repo_manifest_enc_key: [2u8; 32],
            repo_nonce_key: [3u8; 32],
            repo_path_index_key: [4u8; 32],
            epoch_keys: std::collections::BTreeMap::from([(
                DEFAULT_ACTIVE_EPOCH,
                EpochSecrets {
                    manifest_enc_key: [2u8; 32],
                    nonce_key: [3u8; 32],
                },
            )]),
        };
        let mut keyring_one: KeyringState = read_json(generation_one_path.clone()).unwrap();
        keyring_one.generation = 2;
        keyring_one.envelopes = vec![seal_repo_secrets(
            &secrets_two.repo_id,
            DEFAULT_ACTIVE_EPOCH,
            "correct horse battery staple",
            &secrets_two,
            "len:28".to_string(),
        )
        .unwrap()];
        atomic_write_json(generation_two_path, &keyring_one).unwrap();
        atomic_write_json(
            current_pointer_path,
            &KeyringPointer {
                generation: 2,
                current: "keyring.2".to_string(),
            },
        )
        .unwrap();
        let secrets =
            unlock_repo_secrets_uncached(&control_dir, "correct horse battery staple").unwrap();

        assert_eq!(secrets.repo_ref_key, [9u8; 32]);
    }

    #[test]
    fn open_repo_secrets_rejects_pointer_generation_mismatch() {
        let temp = tempdir().unwrap();
        let repo_root = temp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();
        let facade = RepositoryFacade::new();
        facade
            .init(InitOptions {
                repo_root: repo_root.clone(),
                password: "correct horse battery staple".to_string(),
                branch_name: "main".to_string(),
            })
            .unwrap();
        let control_dir = repo_root.join(CONTROL_DIR);
        let generation_two_path = control_dir.join("keyring").join("keyring.2");
        let current_pointer_path = control_dir.join(KEYRING_CURRENT_FILE);
        let mut keyring_one: KeyringState =
            read_json(control_dir.join(KEYRING_DIR).join("keyring.1")).unwrap();
        keyring_one.generation = 1;
        atomic_write_json(generation_two_path, &keyring_one).unwrap();
        atomic_write_json(
            current_pointer_path,
            &KeyringPointer {
                generation: 2,
                current: "keyring.2".to_string(),
            },
        )
        .unwrap();

        let error =
            unlock_repo_secrets_uncached(&control_dir, "correct horse battery staple").unwrap_err();

        assert!(
            error.to_string().contains("generation") || error.to_string().contains("mismatch"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn keyring_generation_updates_retain_previous_generation_file() {
        let temp = tempdir().unwrap();
        let repo_root = temp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();
        let facade = RepositoryFacade::new();
        facade
            .init(InitOptions {
                repo_root: repo_root.clone(),
                password: "correct horse battery staple".to_string(),
                branch_name: "main".to_string(),
            })
            .unwrap();
        let control_dir = repo_root.join(CONTROL_DIR);
        let generation_one_path = control_dir.join(KEYRING_DIR).join("keyring.1");
        let generation_two_path = control_dir.join("keyring").join("keyring.2");
        let keyring_pointer_path = control_dir.join(KEYRING_CURRENT_FILE);
        let keyring_one: KeyringState = read_json(generation_one_path.clone()).unwrap();

        let secrets_two = RepoSecrets {
            repo_id: derive_repo_id(&repo_root),
            active_epoch: DEFAULT_ACTIVE_EPOCH,
            repo_dedup_key: [1u8; 32],
            repo_ref_key: [9u8; 32],
            repo_manifest_enc_key: [2u8; 32],
            repo_nonce_key: [3u8; 32],
            repo_path_index_key: [4u8; 32],
            epoch_keys: std::collections::BTreeMap::from([(
                DEFAULT_ACTIVE_EPOCH,
                EpochSecrets {
                    manifest_enc_key: [2u8; 32],
                    nonce_key: [3u8; 32],
                },
            )]),
        };
        let keyring_two = KeyringState {
            generation: 2,
            envelopes: vec![seal_repo_secrets(
                &secrets_two.repo_id,
                DEFAULT_ACTIVE_EPOCH,
                "correct horse battery staple",
                &secrets_two,
                "len:28".to_string(),
            )
            .unwrap()],
            ..keyring_one.clone()
        };

        atomic_write_json(generation_two_path.clone(), &keyring_two).unwrap();
        atomic_write_json(
            keyring_pointer_path,
            &KeyringPointer {
                generation: 2,
                current: "keyring.2".to_string(),
            },
        )
        .unwrap();

        assert!(generation_one_path.is_file());
        assert!(generation_two_path.is_file());
        let original: KeyringState = read_json(generation_one_path).unwrap();
        assert_eq!(original.generation, 1);
    }

    #[test]
    fn keyring_generation_update_rejects_existing_lock_file() {
        let temp = tempdir().unwrap();
        let repo_root = temp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();
        let facade = RepositoryFacade::new();
        facade
            .init(InitOptions {
                repo_root: repo_root.clone(),
                password: "correct horse battery staple".to_string(),
                branch_name: "main".to_string(),
            })
            .unwrap();
        let control_dir = repo_root.join(CONTROL_DIR);
        let generation_one_path = control_dir.join(KEYRING_DIR).join("keyring.1");
        let generation_two_path = control_dir.join("keyring").join("keyring.2");
        let keyring_pointer_path = control_dir.join(KEYRING_CURRENT_FILE);
        let keyring_one: KeyringState = read_json(generation_one_path).unwrap();
        let lock_path = control_dir.join("keyring").join("keyring.lock");
        fs::write(&lock_path, b"locked").unwrap();

        let error = write_keyring_generation_and_pointer(
            &control_dir,
            "keyring.2",
            &KeyringState {
                generation: 2,
                ..keyring_one.clone()
            },
            &KeyringPointer {
                generation: 2,
                current: "keyring.2".to_string(),
            },
        )
        .unwrap_err();

        assert!(
            error.to_string().contains("lock"),
            "unexpected error: {error:#}"
        );
        assert!(!generation_two_path.exists());
        let current: KeyringPointer = read_json(keyring_pointer_path).unwrap();
        assert_eq!(current.current, "keyring.1");
    }

    #[test]
    fn keyring_generation_update_leaves_no_journal_after_success() {
        let temp = tempdir().unwrap();
        let repo_root = temp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();
        let facade = RepositoryFacade::new();
        facade
            .init(InitOptions {
                repo_root: repo_root.clone(),
                password: "correct horse battery staple".to_string(),
                branch_name: "main".to_string(),
            })
            .unwrap();
        let control_dir = repo_root.join(CONTROL_DIR);
        let generation_one_path = control_dir.join(KEYRING_DIR).join("keyring.1");
        let generation_two_path = control_dir.join(KEYRING_DIR).join("keyring.2");
        let keyring_pointer_path = control_dir.join(KEYRING_CURRENT_FILE);
        let journal_path = control_dir.join(JOURNAL_DIR).join("keyring-update.json");
        let keyring_one: KeyringState = read_json(generation_one_path).unwrap();

        write_keyring_generation_and_pointer(
            &control_dir,
            "keyring.2",
            &KeyringState {
                generation: 2,
                ..keyring_one
            },
            &KeyringPointer {
                generation: 2,
                current: "keyring.2".to_string(),
            },
        )
        .unwrap();

        assert!(generation_two_path.is_file());
        assert!(keyring_pointer_path.is_file());
        assert!(!journal_path.exists());
    }

    #[test]
    fn keyring_generation_update_retains_journal_on_write_failure_and_releases_lock() {
        let temp = tempdir().unwrap();
        let repo_root = temp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();
        let facade = RepositoryFacade::new();
        facade
            .init(InitOptions {
                repo_root: repo_root.clone(),
                password: "correct horse battery staple".to_string(),
                branch_name: "main".to_string(),
            })
            .unwrap();
        let control_dir = repo_root.join(CONTROL_DIR);
        let generation_one_path = control_dir.join(KEYRING_DIR).join("keyring.1");
        let generation_two_path = control_dir.join(KEYRING_DIR).join("keyring.2");
        let keyring_pointer_path = control_dir.join(KEYRING_CURRENT_FILE);
        let journal_path = control_dir.join(JOURNAL_DIR).join("keyring-update.json");
        let lock_path = control_dir.join(KEYRING_DIR).join("keyring.lock");
        let keyring_one: KeyringState = read_json(generation_one_path).unwrap();
        fs::create_dir_all(&generation_two_path).unwrap();

        let error = write_keyring_generation_and_pointer(
            &control_dir,
            "keyring.2",
            &KeyringState {
                generation: 2,
                ..keyring_one
            },
            &KeyringPointer {
                generation: 2,
                current: "keyring.2".to_string(),
            },
        )
        .unwrap_err();

        assert!(
            error.to_string().contains("failed") || error.to_string().contains("directory"),
            "unexpected error: {error:#}"
        );
        assert!(journal_path.exists());
        assert!(!lock_path.exists());
        let journal: serde_json::Value = read_json(journal_path).unwrap();
        assert_eq!(journal["generation"].as_u64(), Some(2));
        assert_eq!(journal["current"].as_str(), Some("keyring.2"));
        assert!(journal["stage"].is_string());
        let current: KeyringPointer = read_json(keyring_pointer_path).unwrap();
        assert_eq!(current.current, "keyring.1");
    }

    #[test]
    fn keyring_generation_update_records_pointer_stage_when_pointer_publish_fails() {
        let temp = tempdir().unwrap();
        let repo_root = temp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();
        let facade = RepositoryFacade::new();
        facade
            .init(InitOptions {
                repo_root: repo_root.clone(),
                password: "correct horse battery staple".to_string(),
                branch_name: "main".to_string(),
            })
            .unwrap();
        let control_dir = repo_root.join(CONTROL_DIR);
        let generation_one_path = control_dir.join(KEYRING_DIR).join("keyring.1");
        let generation_two_path = control_dir.join(KEYRING_DIR).join("keyring.2");
        let keyring_pointer_path = control_dir.join(KEYRING_CURRENT_FILE);
        let journal_path = control_dir.join(JOURNAL_DIR).join("keyring-update.json");
        let lock_path = control_dir.join(KEYRING_DIR).join("keyring.lock");
        let keyring_one: KeyringState = read_json(generation_one_path).unwrap();
        fs::remove_file(&keyring_pointer_path).unwrap();
        fs::create_dir_all(&keyring_pointer_path).unwrap();

        let error = write_keyring_generation_and_pointer(
            &control_dir,
            "keyring.2",
            &KeyringState {
                generation: 2,
                ..keyring_one
            },
            &KeyringPointer {
                generation: 2,
                current: "keyring.2".to_string(),
            },
        )
        .unwrap_err();

        assert!(
            error.to_string().contains("failed") || error.to_string().contains("directory"),
            "unexpected error: {error:#}"
        );
        assert!(generation_two_path.is_file());
        assert!(journal_path.exists());
        assert!(!lock_path.exists());
        let journal: serde_json::Value = read_json(journal_path).unwrap();
        assert_eq!(journal["stage"].as_str(), Some("writing_pointer"));
    }

    #[test]
    fn with_keyring_lock_creates_lock_during_critical_section_and_removes_it_afterward() {
        let temp = tempdir().unwrap();
        let keyring_dir = temp.path().join("keyring");
        fs::create_dir_all(&keyring_dir).unwrap();
        let lock_path = keyring_dir.join("keyring.lock");

        with_keyring_lock(&keyring_dir, || {
            assert!(lock_path.exists());
            Ok(())
        })
        .unwrap();

        assert!(!lock_path.exists());
    }

    #[test]
    fn checkout_revalidates_manifest_paths_before_writing() {
        let temp = tempdir().unwrap();
        let repo_root = temp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();
        let facade = RepositoryFacade::new();
        facade
            .init(InitOptions {
                repo_root: repo_root.clone(),
                password: "correct horse battery staple".to_string(),
                branch_name: "main".to_string(),
            })
            .unwrap();
        let control_dir = repo_root.join(CONTROL_DIR);
        let store = open_object_store(&control_dir).unwrap();

        let chunk_id = write_object(
            &store,
            "chunk",
            &ChunkObject {
                plaintext_length: 5,
                data: b"hello".to_vec(),
            },
        )
        .unwrap();
        let file_id = write_object(
            &store,
            "file",
            &FileObject {
                schema_version: REPO_FORMAT_VERSION,
                entry_name: "../escape.txt".to_string(),
                file_size: 5,
                modified_unix_ms: 1,
                chunker_id: "fastcdc".to_string(),
                chunker_config_id: "fastcdc-64k-1m-8m".to_string(),
                chunk_count: 1,
                chunks: vec![chunk_id],
                chunk_lengths: vec![5],
                shard_ids: vec![],
                shard_byte_lengths: vec![],
            },
        )
        .unwrap();
        let tree_id = write_object(
            &store,
            "tree",
            &TreeObject {
                schema_version: REPO_FORMAT_VERSION,
                entries: vec![TreeEntry {
                    name: "../escape.txt".to_string(),
                    kind: "file".to_string(),
                    object_id: file_id,
                }],
            },
        )
        .unwrap();
        let snapshot_id = write_object(
            &store,
            "snapshot",
            &SnapshotObject {
                schema_version: REPO_FORMAT_VERSION,
                message: "malicious".to_string(),
                root_tree_id: tree_id,
                parent_snapshot_id: None,
            },
        )
        .unwrap();

        let checkout_target = temp.path().join("checkout");
        fs::create_dir_all(&checkout_target).unwrap();
        let escaped_path = temp.path().join("escape.txt");

        let error = facade
            .checkout(CheckoutOptions {
                repo_root: repo_root.clone(),
                snapshot_id,
                target_dir: checkout_target.clone(),
            })
            .unwrap_err();

        assert!(
            error.to_string().contains("path policy"),
            "unexpected error: {error:#}"
        );
        assert!(!escaped_path.exists());
        assert!(!checkout_target.join("escape.txt").exists());
    }

    #[test]
    fn atomic_write_bytes_publishes_final_content() {
        let temp = tempdir().unwrap();
        let file_path = temp.path().join("control.json");

        atomic_write_bytes(file_path.clone(), br#"{"hello":"world"}"#).unwrap();

        let written = fs::read(&file_path).unwrap();
        assert_eq!(written, br#"{"hello":"world"}"#);
    }

    #[test]
    fn atomic_write_bytes_leaves_no_temp_file_after_publish() {
        let temp = tempdir().unwrap();
        let file_path = temp.path().join("control.json");

        atomic_write_bytes(file_path.clone(), br#"{"hello":"world"}"#).unwrap();

        let leftover_temps = fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .filter(|name| name.ends_with(".tmp"))
            .collect::<Vec<_>>();
        assert!(
            leftover_temps.is_empty(),
            "expected no leftover temp files, found {leftover_temps:?}"
        );
    }

    #[test]
    fn read_default_ref_rejects_tampered_crypto_suite_header() {
        let temp = tempdir().unwrap();
        let repo_root = temp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();
        let facade = RepositoryFacade::new();
        facade
            .init(InitOptions {
                repo_root: repo_root.clone(),
                password: "correct horse battery staple".to_string(),
                branch_name: "main".to_string(),
            })
            .unwrap();
        let control_dir = repo_root.join(CONTROL_DIR);
        let secrets =
            unlock_repo_secrets_uncached(&control_dir, "correct horse battery staple").unwrap();
        let ref_path = control_dir.join(DEFAULT_REF_FILE);
        let bytes = fs::read(&ref_path).unwrap();
        let mut envelope: EncryptedControlRecord = postcard_from_bytes(&bytes).unwrap();
        envelope.crypto_suite = "not-a-real-suite".to_string();
        fs::write(&ref_path, postcard_to_vec(&envelope).unwrap()).unwrap();

        let error = read_default_ref(&control_dir, &secrets).unwrap_err();

        assert!(
            error.to_string().contains("crypto suite")
                || error.to_string().contains("authentication"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn read_default_ref_rejects_tampered_format_version() {
        let temp = tempdir().unwrap();
        let repo_root = temp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();
        let facade = RepositoryFacade::new();
        facade
            .init(InitOptions {
                repo_root: repo_root.clone(),
                password: "correct horse battery staple".to_string(),
                branch_name: "main".to_string(),
            })
            .unwrap();
        let control_dir = repo_root.join(CONTROL_DIR);
        let secrets =
            unlock_repo_secrets_uncached(&control_dir, "correct horse battery staple").unwrap();
        let ref_path = control_dir.join(DEFAULT_REF_FILE);
        let bytes = fs::read(&ref_path).unwrap();
        let mut envelope: EncryptedControlRecord = postcard_from_bytes(&bytes).unwrap();
        envelope.format_version += 1;
        fs::write(&ref_path, postcard_to_vec(&envelope).unwrap()).unwrap();

        let error = read_default_ref(&control_dir, &secrets).unwrap_err();

        assert!(
            error.to_string().contains("format version")
                || error.to_string().contains("authentication"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn read_default_ref_rejects_malformed_truncated_headers_without_panicking() {
        let temp = tempdir().unwrap();
        let repo_root = temp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();
        let facade = RepositoryFacade::new();
        facade
            .init(InitOptions {
                repo_root: repo_root.clone(),
                password: "correct horse battery staple".to_string(),
                branch_name: "main".to_string(),
            })
            .unwrap();
        let control_dir = repo_root.join(CONTROL_DIR);
        let secrets =
            unlock_repo_secrets_uncached(&control_dir, "correct horse battery staple").unwrap();
        let ref_path = control_dir.join(DEFAULT_REF_FILE);
        let mut bytes = fs::read(&ref_path).unwrap();
        bytes.truncate(bytes.len() - 5);
        fs::write(&ref_path, &bytes).unwrap();

        let result = std::panic::catch_unwind(|| read_default_ref(&control_dir, &secrets));

        match result {
            Ok(Err(error)) => assert!(
                error.to_string().contains("decode")
                    || error.to_string().contains("authentication")
                    || error.to_string().contains("format"),
                "unexpected error: {error:#}"
            ),
            Ok(Ok(_)) => panic!("expected malformed ref to be rejected"),
            Err(_) => panic!("malformed ref should not panic"),
        }
    }

    #[test]
    fn read_default_ref_rejects_tampered_object_type_header() {
        let temp = tempdir().unwrap();
        let repo_root = temp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();
        let facade = RepositoryFacade::new();
        facade
            .init(InitOptions {
                repo_root: repo_root.clone(),
                password: "correct horse battery staple".to_string(),
                branch_name: "main".to_string(),
            })
            .unwrap();
        let control_dir = repo_root.join(CONTROL_DIR);
        let secrets =
            unlock_repo_secrets_uncached(&control_dir, "correct horse battery staple").unwrap();
        let ref_path = control_dir.join(DEFAULT_REF_FILE);
        let bytes = fs::read(&ref_path).unwrap();
        let mut envelope: EncryptedControlRecord = postcard_from_bytes(&bytes).unwrap();
        envelope.object_type = "tree".to_string();
        fs::write(&ref_path, postcard_to_vec(&envelope).unwrap()).unwrap();

        let error = read_default_ref(&control_dir, &secrets).unwrap_err();

        assert!(
            error.to_string().contains("authentication"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn read_default_ref_rejects_tampered_nonce_length() {
        let temp = tempdir().unwrap();
        let repo_root = temp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();
        let facade = RepositoryFacade::new();
        facade
            .init(InitOptions {
                repo_root: repo_root.clone(),
                password: "correct horse battery staple".to_string(),
                branch_name: "main".to_string(),
            })
            .unwrap();
        let control_dir = repo_root.join(CONTROL_DIR);
        let secrets =
            unlock_repo_secrets_uncached(&control_dir, "correct horse battery staple").unwrap();
        let ref_path = control_dir.join(DEFAULT_REF_FILE);
        let bytes = fs::read(&ref_path).unwrap();
        let mut envelope: EncryptedControlRecord = postcard_from_bytes(&bytes).unwrap();
        envelope.nonce.pop();
        fs::write(&ref_path, postcard_to_vec(&envelope).unwrap()).unwrap();

        let error = read_default_ref(&control_dir, &secrets).unwrap_err();

        assert!(
            error.to_string().contains("authentication"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn sync_path_accepts_regular_files() {
        let temp = tempdir().unwrap();
        let file_path = temp.path().join("control.json");
        fs::write(&file_path, br#"{"hello":"world"}"#).unwrap();

        sync_path(&file_path).unwrap();
    }

    #[test]
    fn sync_path_accepts_directories() {
        let temp = tempdir().unwrap();

        sync_path(temp.path()).unwrap();
    }

    #[test]
    fn checkout_file_to_temp_requests_bounded_read_ranges() {
        let requested_lengths = RefCell::new(Vec::new());
        let file_size = (CHECKOUT_STREAM_CHUNK_BYTES as u64) * 2 + 17;
        let read_service = |offset: usize, length: usize| -> Result<Vec<u8>> {
            requested_lengths.borrow_mut().push((offset, length));
            ensure!(
                length <= CHECKOUT_STREAM_CHUNK_BYTES,
                "requested oversized checkout read: {length}"
            );
            let remaining = (file_size as usize).saturating_sub(offset);
            Ok(vec![b'x'; remaining.min(length)])
        };
        let temp = tempdir().unwrap();
        let final_path = temp.path().join("large.bin");
        let file = FileHandle {
            snapshot_id: "snapshot".to_string(),
            file_object_id: "file".to_string(),
            file_size,
            chunk_count: 0,
            chunk_ids: Vec::new(),
            chunk_lengths: Vec::new(),
            shard_ids: Vec::new(),
            shard_byte_lengths: Vec::new(),
            layout_generation: 1,
            crypto_suite: "xchacha20poly1305".to_string(),
            key_epoch: DEFAULT_ACTIVE_EPOCH,
            chunker_id: "fastcdc".to_string(),
        };

        let temp_path = checkout_file_to_temp(
            &WorkingTree::new(temp.path()),
            &file,
            &final_path,
            read_service,
        )
        .unwrap();

        assert_eq!(
            requested_lengths.borrow().as_slice(),
            &[
                (0, CHECKOUT_STREAM_CHUNK_BYTES),
                (CHECKOUT_STREAM_CHUNK_BYTES, CHECKOUT_STREAM_CHUNK_BYTES),
                (CHECKOUT_STREAM_CHUNK_BYTES * 2, 17),
            ]
        );
        assert_eq!(fs::read(temp_path).unwrap().len(), file_size as usize);
    }
}
