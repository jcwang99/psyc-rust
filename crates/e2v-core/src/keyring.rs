use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result, ensure};
use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{Tag, XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};
use x25519_dalek::{PublicKey, StaticSecret};

use e2v_store::{EpochSecrets, RepoSecrets};

pub const KEYRING_DIR: &str = "keyring";
pub const KEYRING_CURRENT_FILE: &str = "keyring/keyring.current";
pub const LOCAL_DEVICE_FILE: &str = "device/local-device.json";
const KEYRING_PASSWORD_MAGIC: &[u8; 4] = b"E2KP";
const KEYRING_PASSWORD_FORMAT_VERSION: u32 = 1;
const KEYRING_PASSWORD_OBJECT_TYPE: &str = "keyring-password-envelope";
const KEYRING_DEVICE_MAGIC: &[u8; 4] = b"E2KD";
const KEYRING_DEVICE_FORMAT_VERSION: u32 = 1;
const KEYRING_DEVICE_OBJECT_TYPE: &str = "keyring-device-envelope";

static UNLOCKED_KEYRINGS: OnceLock<Mutex<HashMap<PathBuf, RepoSecrets>>> = OnceLock::new();
static UNLOCKED_PASSWORDS: OnceLock<Mutex<HashMap<PathBuf, String>>> = OnceLock::new();

fn lock_or_recover<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyringEnvelope {
    pub kind: String,
    #[serde(default)]
    pub envelope_id: String,
    #[serde(default)]
    pub actor_id: String,
    #[serde(default)]
    pub device_id: String,
    #[serde(default)]
    pub recipient_pubkey_hex: String,
    #[serde(default)]
    pub record_hex: String,
    pub password_hint: String,
    pub salt_hex: String,
    pub nonce_hex: String,
    pub ciphertext_hex: String,
    pub auth_tag_hex: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActorRecord {
    pub actor_id: String,
    pub display_name: String,
    pub role: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRecord {
    pub device_id: String,
    pub actor_id: String,
    pub label: String,
    pub device_pubkey_hex: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EpochDescriptor {
    pub epoch: u32,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyringState {
    pub format_version: u32,
    pub generation: u64,
    pub repo_id: String,
    pub active_epoch: u32,
    pub crypto_suite: String,
    pub kdf: String,
    #[serde(default)]
    pub actors: Vec<ActorRecord>,
    #[serde(default)]
    pub devices: Vec<DeviceRecord>,
    #[serde(default)]
    pub epochs: Vec<EpochDescriptor>,
    pub envelopes: Vec<KeyringEnvelope>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyringPointer {
    pub generation: u64,
    pub current: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedRepoSecrets {
    pub repo_dedup_key_hex: String,
    pub repo_ref_key_hex: String,
    pub repo_manifest_enc_key_hex: String,
    pub repo_nonce_key_hex: String,
    pub repo_path_index_key_hex: String,
    #[serde(default)]
    pub epoch_manifest_enc_keys_hex: BTreeMap<u32, String>,
    #[serde(default)]
    pub epoch_nonce_keys_hex: BTreeMap<u32, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PasswordEnvelopeRecord {
    magic: [u8; 4],
    format_version: u32,
    object_type: String,
    crypto_suite: String,
    nonce: Vec<u8>,
    ciphertext: Vec<u8>,
    auth_tag: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalDeviceCredential {
    pub actor_id: String,
    pub device_id: String,
    pub label: String,
    pub public_key_hex: String,
    pub private_key_hex: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct DeviceEnvelopeRecord {
    magic: [u8; 4],
    format_version: u32,
    object_type: String,
    crypto_suite: String,
    ephemeral_public_key: [u8; 32],
    nonce: Vec<u8>,
    ciphertext: Vec<u8>,
    auth_tag: Vec<u8>,
}

pub fn cache_unlocked_secrets(control_dir: &Path, secrets: &RepoSecrets) {
    let cache = UNLOCKED_KEYRINGS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = lock_or_recover(cache);
    cache.insert(control_dir.to_path_buf(), secrets.clone());
}

pub fn cache_unlocked_password(control_dir: &Path, password: &str) {
    let cache = UNLOCKED_PASSWORDS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = lock_or_recover(cache);
    cache.insert(control_dir.to_path_buf(), password.to_string());
}

pub fn clear_unlocked_keyring_cache(control_dir: &Path) {
    if let Some(cache) = UNLOCKED_KEYRINGS.get() {
        lock_or_recover(cache).remove(control_dir);
    }
    if let Some(cache) = UNLOCKED_PASSWORDS.get() {
        lock_or_recover(cache).remove(control_dir);
    }
}

pub(crate) fn clear_unlocked_keyring_cache_for_test(control_dir: &Path) {
    clear_unlocked_keyring_cache(control_dir);
}

pub fn unlock_repo_secrets(control_dir: &Path, password: &str) -> Result<RepoSecrets> {
    let secrets = unlock_repo_secrets_uncached(control_dir, password)?;
    cache_unlocked_secrets(control_dir, &secrets);
    cache_unlocked_password(control_dir, password);
    Ok(secrets)
}

pub fn unlock_repo_secrets_with_local_device(control_dir: &Path) -> Result<RepoSecrets> {
    let keyring = read_current_keyring_state(control_dir)?;
    let credential = read_local_device_credential(control_dir)?;
    let secrets = unlock_repo_secrets_from_state_with_local_device(&keyring, &credential)?;
    cache_unlocked_secrets(control_dir, &secrets);
    Ok(secrets)
}

pub fn unlock_repo_secrets_uncached(control_dir: &Path, password: &str) -> Result<RepoSecrets> {
    let keyring = read_current_keyring_state(control_dir)?;
    unlock_repo_secrets_from_state(&keyring, password)
}

pub fn unlock_repo_secrets_from_generation_file(
    control_dir: &Path,
    keyring_file_name: &str,
    password: &str,
) -> Result<RepoSecrets> {
    let keyring: KeyringState = read_json(control_dir.join(KEYRING_DIR).join(keyring_file_name))?;
    unlock_repo_secrets_from_state(&keyring, password)
}

pub fn unlock_repo_secrets_from_keyring_bytes(
    keyring_bytes: &[u8],
    password: &str,
) -> Result<RepoSecrets> {
    let keyring: KeyringState =
        serde_json::from_slice(keyring_bytes).context("failed to decode keyring state")?;
    unlock_repo_secrets_from_state(&keyring, password)
}

pub fn unlock_repo_secrets_from_keyring_bytes_with_local_device(
    control_dir: &Path,
    keyring_bytes: &[u8],
) -> Result<RepoSecrets> {
    let keyring: KeyringState =
        serde_json::from_slice(keyring_bytes).context("failed to decode keyring state")?;
    let credential = read_local_device_credential(control_dir)?;
    let secrets = unlock_repo_secrets_from_state_with_local_device(&keyring, &credential)?;
    cache_unlocked_secrets(control_dir, &secrets);
    Ok(secrets)
}

fn unlock_repo_secrets_from_state(keyring: &KeyringState, password: &str) -> Result<RepoSecrets> {
    let password_envelope = keyring
        .envelopes
        .iter()
        .find(|envelope| envelope.kind == "password")
        .context("keyring has no password envelope")?;
    let sealed = decrypt_password_envelope(keyring, password_envelope, password)?;
    let secrets = RepoSecrets {
        repo_id: keyring.repo_id.clone(),
        active_epoch: keyring.active_epoch,
        repo_dedup_key: decode_hex_key(&sealed.repo_dedup_key_hex)?,
        repo_ref_key: decode_hex_key(&sealed.repo_ref_key_hex)?,
        repo_manifest_enc_key: decode_hex_key(&sealed.repo_manifest_enc_key_hex)?,
        repo_nonce_key: decode_hex_key(&sealed.repo_nonce_key_hex)?,
        repo_path_index_key: decode_hex_key(&sealed.repo_path_index_key_hex)?,
        epoch_keys: decode_epoch_keys(&sealed, keyring.active_epoch)?,
    };
    Ok(secrets)
}

fn unlock_repo_secrets_from_state_with_local_device(
    keyring: &KeyringState,
    credential: &LocalDeviceCredential,
) -> Result<RepoSecrets> {
    let device_envelope = keyring
        .envelopes
        .iter()
        .find(|envelope| {
            envelope.kind == "device"
                && !envelope.device_id.is_empty()
                && envelope.device_id == credential.device_id
        })
        .context("keyring has no matching local device envelope")?;
    let sealed = decrypt_device_envelope(keyring, device_envelope, credential)?;
    Ok(RepoSecrets {
        repo_id: keyring.repo_id.clone(),
        active_epoch: keyring.active_epoch,
        repo_dedup_key: decode_hex_key(&sealed.repo_dedup_key_hex)?,
        repo_ref_key: decode_hex_key(&sealed.repo_ref_key_hex)?,
        repo_manifest_enc_key: decode_hex_key(&sealed.repo_manifest_enc_key_hex)?,
        repo_nonce_key: decode_hex_key(&sealed.repo_nonce_key_hex)?,
        repo_path_index_key: decode_hex_key(&sealed.repo_path_index_key_hex)?,
        epoch_keys: decode_epoch_keys(&sealed, keyring.active_epoch)?,
    })
}

pub fn open_repo_secrets(control_dir: &Path) -> Result<RepoSecrets> {
    let cache = UNLOCKED_KEYRINGS.get_or_init(|| Mutex::new(HashMap::new()));
    let cache = lock_or_recover(cache);
    cache
        .get(control_dir)
        .cloned()
        .context("repository keyring is locked; unlock with a password first")
}

pub fn seal_repo_secrets(
    repo_id: &str,
    active_epoch: u32,
    password: &str,
    secrets: &RepoSecrets,
    password_hint: String,
) -> Result<KeyringEnvelope> {
    let salt = derive_password_salt(repo_id, active_epoch);
    let key = derive_password_key(password, &salt)?;
    let sealed = SealedRepoSecrets {
        repo_dedup_key_hex: hex::encode(secrets.repo_dedup_key),
        repo_ref_key_hex: hex::encode(secrets.repo_ref_key),
        repo_manifest_enc_key_hex: hex::encode(secrets.repo_manifest_enc_key),
        repo_nonce_key_hex: hex::encode(secrets.repo_nonce_key),
        repo_path_index_key_hex: hex::encode(secrets.repo_path_index_key),
        epoch_manifest_enc_keys_hex: secrets
            .epoch_keys
            .iter()
            .map(|(epoch, keys)| (*epoch, hex::encode(keys.manifest_enc_key)))
            .collect(),
        epoch_nonce_keys_hex: secrets
            .epoch_keys
            .iter()
            .map(|(epoch, keys)| (*epoch, hex::encode(keys.nonce_key)))
            .collect(),
    };
    let plaintext = postcard::to_stdvec(&sealed).context("failed to encode sealed repo secrets")?;
    let mut nonce = [0u8; 24];
    getrandom::fill(&mut nonce).map_err(|_| anyhow::anyhow!("failed to obtain keyring nonce"))?;
    let cipher = XChaCha20Poly1305::new((&key).into());
    let mut ciphertext = plaintext.clone();
    let associated_data = password_envelope_associated_data(repo_id, active_epoch);
    let auth_tag = cipher
        .encrypt_in_place_detached(
            XNonce::from_slice(&nonce),
            &associated_data,
            &mut ciphertext,
        )
        .map_err(|_| anyhow::anyhow!("failed to encrypt keyring envelope"))?;
    let record = PasswordEnvelopeRecord {
        magic: *KEYRING_PASSWORD_MAGIC,
        format_version: KEYRING_PASSWORD_FORMAT_VERSION,
        object_type: KEYRING_PASSWORD_OBJECT_TYPE.to_string(),
        crypto_suite: "xchacha20poly1305".to_string(),
        nonce: nonce.to_vec(),
        ciphertext,
        auth_tag: auth_tag.to_vec(),
    };
    Ok(KeyringEnvelope {
        kind: "password".to_string(),
        envelope_id: String::new(),
        actor_id: String::new(),
        device_id: String::new(),
        recipient_pubkey_hex: String::new(),
        record_hex: String::new(),
        password_hint,
        salt_hex: hex::encode(salt),
        nonce_hex: hex::encode(nonce),
        ciphertext_hex: hex::encode(
            postcard::to_stdvec(&record).context("failed to encode keyring password envelope")?,
        ),
        auth_tag_hex: hex::encode(auth_tag),
    })
}

pub fn seal_repo_secrets_for_device(
    repo_id: &str,
    active_epoch: u32,
    recipient_public_key_hex: &str,
    secrets: &RepoSecrets,
    actor_id: &str,
    device_id: &str,
) -> Result<KeyringEnvelope> {
    let recipient_public_key = decode_x25519_public_key(recipient_public_key_hex)?;
    let mut ephemeral_secret_bytes = [0u8; 32];
    getrandom::fill(&mut ephemeral_secret_bytes)
        .map_err(|_| anyhow::anyhow!("failed to obtain device envelope key material"))?;
    let ephemeral_secret = StaticSecret::from(ephemeral_secret_bytes);
    let ephemeral_public_key = PublicKey::from(&ephemeral_secret);
    let shared_secret = ephemeral_secret.diffie_hellman(&recipient_public_key);
    let key = derive_device_key(
        shared_secret.as_bytes(),
        repo_id,
        active_epoch,
        actor_id,
        device_id,
    );
    let sealed = SealedRepoSecrets {
        repo_dedup_key_hex: hex::encode(secrets.repo_dedup_key),
        repo_ref_key_hex: hex::encode(secrets.repo_ref_key),
        repo_manifest_enc_key_hex: hex::encode(secrets.repo_manifest_enc_key),
        repo_nonce_key_hex: hex::encode(secrets.repo_nonce_key),
        repo_path_index_key_hex: hex::encode(secrets.repo_path_index_key),
        epoch_manifest_enc_keys_hex: secrets
            .epoch_keys
            .iter()
            .map(|(epoch, keys)| (*epoch, hex::encode(keys.manifest_enc_key)))
            .collect(),
        epoch_nonce_keys_hex: secrets
            .epoch_keys
            .iter()
            .map(|(epoch, keys)| (*epoch, hex::encode(keys.nonce_key)))
            .collect(),
    };
    let plaintext =
        postcard::to_stdvec(&sealed).context("failed to encode device sealed repo secrets")?;
    let mut nonce = [0u8; 24];
    getrandom::fill(&mut nonce)
        .map_err(|_| anyhow::anyhow!("failed to obtain device envelope nonce"))?;
    let cipher = XChaCha20Poly1305::new((&key).into());
    let mut ciphertext = plaintext;
    let associated_data =
        device_envelope_associated_data(repo_id, active_epoch, actor_id, device_id);
    let auth_tag = cipher
        .encrypt_in_place_detached(
            XNonce::from_slice(&nonce),
            &associated_data,
            &mut ciphertext,
        )
        .map_err(|_| anyhow::anyhow!("failed to encrypt keyring device envelope"))?;
    let record = DeviceEnvelopeRecord {
        magic: *KEYRING_DEVICE_MAGIC,
        format_version: KEYRING_DEVICE_FORMAT_VERSION,
        object_type: KEYRING_DEVICE_OBJECT_TYPE.to_string(),
        crypto_suite: "x25519-xchacha20poly1305".to_string(),
        ephemeral_public_key: ephemeral_public_key.to_bytes(),
        nonce: nonce.to_vec(),
        ciphertext,
        auth_tag: auth_tag.to_vec(),
    };
    Ok(KeyringEnvelope {
        kind: "device".to_string(),
        envelope_id: format!("device:{device_id}"),
        actor_id: actor_id.to_string(),
        device_id: device_id.to_string(),
        recipient_pubkey_hex: recipient_public_key_hex.to_string(),
        record_hex: hex::encode(
            postcard::to_stdvec(&record).context("failed to encode keyring device envelope")?,
        ),
        password_hint: String::new(),
        salt_hex: String::new(),
        nonce_hex: String::new(),
        ciphertext_hex: String::new(),
        auth_tag_hex: String::new(),
    })
}

pub fn read_current_keyring_state(control_dir: &Path) -> Result<KeyringState> {
    let keyring_pointer: KeyringPointer = read_json(control_dir.join(KEYRING_CURRENT_FILE))?;
    let keyring: KeyringState =
        read_json(control_dir.join(KEYRING_DIR).join(&keyring_pointer.current))?;
    ensure!(
        keyring_pointer.generation == keyring.generation,
        "keyring pointer generation mismatch"
    );
    Ok(keyring)
}

fn decrypt_password_envelope(
    keyring: &KeyringState,
    envelope: &KeyringEnvelope,
    password: &str,
) -> Result<SealedRepoSecrets> {
    let salt = hex::decode(&envelope.salt_hex).context("invalid keyring password envelope salt")?;
    let key = derive_password_key(password, &salt)?;
    let record_bytes = hex::decode(&envelope.ciphertext_hex)
        .context("invalid keyring password envelope ciphertext")?;
    let record: PasswordEnvelopeRecord = postcard::from_bytes(&record_bytes)
        .context("failed to decode keyring password envelope")?;
    ensure!(
        record.magic == *KEYRING_PASSWORD_MAGIC,
        "keyring unlock failed"
    );
    ensure!(
        record.format_version == KEYRING_PASSWORD_FORMAT_VERSION,
        "unsupported keyring password envelope version"
    );
    ensure!(
        record.object_type == KEYRING_PASSWORD_OBJECT_TYPE,
        "keyring unlock failed"
    );
    ensure!(record.nonce.len() == 24, "keyring unlock failed");
    ensure!(record.auth_tag.len() == 16, "keyring unlock failed");

    let cipher = XChaCha20Poly1305::new((&key).into());
    let mut plaintext = record.ciphertext.clone();
    let associated_data = password_envelope_associated_data(&keyring.repo_id, keyring.active_epoch);
    cipher
        .decrypt_in_place_detached(
            XNonce::from_slice(&record.nonce),
            &associated_data,
            &mut plaintext,
            Tag::from_slice(&record.auth_tag),
        )
        .map_err(|_| anyhow::anyhow!("keyring unlock failed: wrong password"))?;

    postcard::from_bytes(&plaintext).context("failed to decode sealed repo secrets")
}

fn decrypt_device_envelope(
    keyring: &KeyringState,
    envelope: &KeyringEnvelope,
    credential: &LocalDeviceCredential,
) -> Result<SealedRepoSecrets> {
    ensure!(
        !envelope.record_hex.is_empty(),
        "invalid keyring device envelope record"
    );
    let record_bytes =
        hex::decode(&envelope.record_hex).context("invalid keyring device envelope record")?;
    let record: DeviceEnvelopeRecord =
        postcard::from_bytes(&record_bytes).context("failed to decode keyring device envelope")?;
    ensure!(
        record.magic == *KEYRING_DEVICE_MAGIC,
        "keyring device unlock failed"
    );
    ensure!(
        record.format_version == KEYRING_DEVICE_FORMAT_VERSION,
        "unsupported keyring device envelope version"
    );
    ensure!(
        record.object_type == KEYRING_DEVICE_OBJECT_TYPE,
        "keyring device unlock failed"
    );
    ensure!(
        record.crypto_suite == "x25519-xchacha20poly1305",
        "unsupported keyring device envelope crypto suite"
    );
    ensure!(record.nonce.len() == 24, "keyring device unlock failed");
    ensure!(record.auth_tag.len() == 16, "keyring device unlock failed");

    let private_key = decode_x25519_secret_key(&credential.private_key_hex)?;
    let shared_secret = private_key.diffie_hellman(&PublicKey::from(record.ephemeral_public_key));
    let key = derive_device_key(
        shared_secret.as_bytes(),
        &keyring.repo_id,
        keyring.active_epoch,
        &credential.actor_id,
        &credential.device_id,
    );
    let cipher = XChaCha20Poly1305::new((&key).into());
    let mut plaintext = record.ciphertext.clone();
    let associated_data = device_envelope_associated_data(
        &keyring.repo_id,
        keyring.active_epoch,
        &credential.actor_id,
        &credential.device_id,
    );
    cipher
        .decrypt_in_place_detached(
            XNonce::from_slice(&record.nonce),
            &associated_data,
            &mut plaintext,
            Tag::from_slice(&record.auth_tag),
        )
        .map_err(|_| anyhow::anyhow!("keyring device unlock failed"))?;

    postcard::from_bytes(&plaintext).context("failed to decode sealed repo secrets")
}

fn password_envelope_associated_data(repo_id: &str, active_epoch: u32) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(KEYRING_PASSWORD_MAGIC);
    data.extend_from_slice(&KEYRING_PASSWORD_FORMAT_VERSION.to_le_bytes());
    data.extend_from_slice(repo_id.as_bytes());
    data.extend_from_slice(&active_epoch.to_le_bytes());
    data
}

fn derive_password_salt(repo_id: &str, active_epoch: u32) -> [u8; 16] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(repo_id.as_bytes());
    hasher.update(&active_epoch.to_le_bytes());
    hasher.update(b"keyring-password-salt");
    let hash = hasher.finalize();
    let mut salt = [0u8; 16];
    salt.copy_from_slice(&hash.as_bytes()[..16]);
    salt
}

fn derive_password_key(password: &str, salt: &[u8]) -> Result<[u8; 32]> {
    let params = Params::new(19_456, 2, 1, Some(32))
        .map_err(|_| anyhow::anyhow!("invalid argon2 params"))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = [0u8; 32];
    argon2
        .hash_password_into(password.as_bytes(), salt, &mut key)
        .map_err(|_| anyhow::anyhow!("failed to derive keyring password key"))?;
    Ok(key)
}

pub fn generate_local_device_credential(
    actor_id: String,
    device_id: String,
    label: String,
) -> Result<LocalDeviceCredential> {
    let mut private_key_bytes = [0u8; 32];
    getrandom::fill(&mut private_key_bytes)
        .map_err(|_| anyhow::anyhow!("failed to obtain local device key material"))?;
    let private_key = StaticSecret::from(private_key_bytes);
    let public_key = PublicKey::from(&private_key);
    Ok(LocalDeviceCredential {
        actor_id,
        device_id,
        label,
        public_key_hex: hex::encode(public_key.to_bytes()),
        private_key_hex: hex::encode(private_key.to_bytes()),
    })
}

pub fn write_local_device_credential(
    control_dir: &Path,
    credential: &LocalDeviceCredential,
) -> Result<()> {
    let path = control_dir.join(LOCAL_DEVICE_FILE);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let bytes =
        serde_json::to_vec(credential).context("failed to encode local device credential")?;
    atomic_write_bytes(&path, &bytes)
}

pub fn read_local_device_credential(control_dir: &Path) -> Result<LocalDeviceCredential> {
    read_json(control_dir.join(LOCAL_DEVICE_FILE))
}

pub fn decode_hex_key(value: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(value).context("invalid key hex")?;
    ensure!(bytes.len() == 32, "invalid key length");
    let mut array = [0u8; 32];
    array.copy_from_slice(&bytes);
    Ok(array)
}

fn decode_epoch_keys(
    sealed: &SealedRepoSecrets,
    active_epoch: u32,
) -> Result<BTreeMap<u32, EpochSecrets>> {
    ensure!(
        !sealed.epoch_manifest_enc_keys_hex.is_empty(),
        "missing epoch manifest encryption keys"
    );
    ensure!(
        !sealed.epoch_nonce_keys_hex.is_empty(),
        "missing epoch nonce keys"
    );
    let mut epoch_keys = BTreeMap::new();
    for (epoch, manifest_enc_key_hex) in &sealed.epoch_manifest_enc_keys_hex {
        let nonce_key_hex = sealed
            .epoch_nonce_keys_hex
            .get(epoch)
            .with_context(|| format!("missing nonce key for epoch {epoch}"))?;
        epoch_keys.insert(
            *epoch,
            EpochSecrets {
                manifest_enc_key: decode_hex_key(manifest_enc_key_hex)?,
                nonce_key: decode_hex_key(nonce_key_hex)?,
            },
        );
    }

    ensure!(
        epoch_keys.contains_key(&active_epoch),
        "missing epoch keys for active epoch {active_epoch}"
    );
    Ok(epoch_keys)
}

fn decode_x25519_public_key(value: &str) -> Result<PublicKey> {
    let bytes = decode_hex_key(value)?;
    Ok(PublicKey::from(bytes))
}

fn decode_x25519_secret_key(value: &str) -> Result<StaticSecret> {
    let bytes = decode_hex_key(value)?;
    Ok(StaticSecret::from(bytes))
}

fn derive_device_key(
    shared_secret: &[u8; 32],
    repo_id: &str,
    active_epoch: u32,
    actor_id: &str,
    device_id: &str,
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(shared_secret);
    hasher.update(repo_id.as_bytes());
    hasher.update(&active_epoch.to_le_bytes());
    hasher.update(actor_id.as_bytes());
    hasher.update(device_id.as_bytes());
    hasher.update(b"keyring-device-envelope");
    let hash = hasher.finalize();
    let mut key = [0u8; 32];
    key.copy_from_slice(hash.as_bytes());
    key
}

fn device_envelope_associated_data(
    repo_id: &str,
    active_epoch: u32,
    actor_id: &str,
    device_id: &str,
) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(KEYRING_DEVICE_MAGIC);
    data.extend_from_slice(&KEYRING_DEVICE_FORMAT_VERSION.to_le_bytes());
    data.extend_from_slice(repo_id.as_bytes());
    data.extend_from_slice(&active_epoch.to_le_bytes());
    data.extend_from_slice(actor_id.as_bytes());
    data.extend_from_slice(device_id.as_bytes());
    data
}

fn read_json<T: for<'de> Deserialize<'de>>(path: PathBuf) -> Result<T> {
    let bytes =
        std::fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("failed to decode {}", path.display()))
}

fn atomic_write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let temp_path = path.with_extension(format!(
        "{}.tmp",
        path.extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("tmp")
    ));
    std::fs::write(&temp_path, bytes)
        .with_context(|| format!("failed to write {}", temp_path.display()))?;
    std::fs::rename(&temp_path, path)
        .with_context(|| format!("failed to publish {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::panic::{self, AssertUnwindSafe};
    use std::path::PathBuf;

    use tempfile::tempdir;

    use super::*;

    fn hex_key(byte: u8) -> String {
        hex::encode([byte; 32])
    }

    fn sample_sealed_repo_secrets() -> SealedRepoSecrets {
        SealedRepoSecrets {
            repo_dedup_key_hex: hex_key(1),
            repo_ref_key_hex: hex_key(2),
            repo_manifest_enc_key_hex: hex_key(3),
            repo_nonce_key_hex: hex_key(4),
            repo_path_index_key_hex: hex_key(5),
            epoch_manifest_enc_keys_hex: BTreeMap::from([(1, hex_key(6)), (2, hex_key(7))]),
            epoch_nonce_keys_hex: BTreeMap::from([(1, hex_key(8)), (2, hex_key(9))]),
        }
    }

    #[test]
    fn decode_epoch_keys_requires_latest_epoch_maps() {
        let mut sealed = sample_sealed_repo_secrets();
        sealed.epoch_manifest_enc_keys_hex.clear();

        let error = decode_epoch_keys(&sealed, 2).unwrap_err();

        assert!(
            error.to_string().contains("epoch manifest")
                || error.to_string().contains("missing epoch"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn decode_epoch_keys_reads_latest_epoch_maps() {
        let sealed = sample_sealed_repo_secrets();

        let decoded = decode_epoch_keys(&sealed, 2).unwrap();

        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[&1].manifest_enc_key, [6u8; 32]);
        assert_eq!(decoded[&1].nonce_key, [8u8; 32]);
        assert_eq!(decoded[&2].manifest_enc_key, [7u8; 32]);
        assert_eq!(decoded[&2].nonce_key, [9u8; 32]);
    }

    #[test]
    fn open_repo_secrets_recovers_from_poisoned_unlocked_keyring_cache() {
        let temp = tempdir().unwrap();
        let control_dir = temp.path().join("control");
        let expected = RepoSecrets {
            repo_id: "repo".to_string(),
            active_epoch: 1,
            repo_dedup_key: [1u8; 32],
            repo_ref_key: [2u8; 32],
            repo_manifest_enc_key: [3u8; 32],
            repo_nonce_key: [4u8; 32],
            repo_path_index_key: [5u8; 32],
            epoch_keys: BTreeMap::from([(
                1,
                EpochSecrets {
                    manifest_enc_key: [6u8; 32],
                    nonce_key: [7u8; 32],
                },
            )]),
        };
        cache_unlocked_secrets(&control_dir, &expected);

        let cache = UNLOCKED_KEYRINGS.get_or_init(|| Mutex::new(HashMap::new()));
        let poisoned = cache;
        let _ = panic::catch_unwind(AssertUnwindSafe(move || {
            let _guard = poisoned.lock().unwrap();
            panic!("poison unlocked keyring cache");
        }));

        let result = panic::catch_unwind(AssertUnwindSafe(|| open_repo_secrets(&control_dir)));

        assert!(result.is_ok(), "open_repo_secrets should not panic");
        assert_eq!(result.unwrap().unwrap(), expected);
    }

    #[test]
    fn clear_unlocked_keyring_cache_recovers_from_poisoned_password_cache() {
        let control_dir = PathBuf::from("repo-control");
        cache_unlocked_password(&control_dir, "password");

        let cache = UNLOCKED_PASSWORDS.get_or_init(|| Mutex::new(HashMap::new()));
        let poisoned = cache;
        let _ = panic::catch_unwind(AssertUnwindSafe(move || {
            let _guard = poisoned.lock().unwrap();
            panic!("poison unlocked password cache");
        }));

        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            clear_unlocked_keyring_cache(&control_dir);
        }));

        assert!(
            result.is_ok(),
            "clear_unlocked_keyring_cache should not panic when the password cache is poisoned"
        );
    }
}
