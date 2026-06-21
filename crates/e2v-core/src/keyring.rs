use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result, ensure};
use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{Tag, XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};

use e2v_store::RepoSecrets;

pub const KEYRING_DIR: &str = "keyring";
pub const KEYRING_CURRENT_FILE: &str = "keyring/keyring.current";
const KEYRING_PASSWORD_MAGIC: &[u8; 4] = b"E2KP";
const KEYRING_PASSWORD_FORMAT_VERSION: u32 = 1;
const KEYRING_PASSWORD_OBJECT_TYPE: &str = "keyring-password-envelope";

static UNLOCKED_KEYRINGS: OnceLock<Mutex<HashMap<PathBuf, RepoSecrets>>> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyringEnvelope {
    pub kind: String,
    pub password_hint: String,
    pub salt_hex: String,
    pub nonce_hex: String,
    pub ciphertext_hex: String,
    pub auth_tag_hex: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyringState {
    pub format_version: u32,
    pub generation: u64,
    pub repo_id: String,
    pub active_epoch: u32,
    pub crypto_suite: String,
    pub kdf: String,
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

pub fn cache_unlocked_secrets(control_dir: &Path, secrets: &RepoSecrets) {
    let cache = UNLOCKED_KEYRINGS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache.lock().unwrap();
    cache.insert(control_dir.to_path_buf(), secrets.clone());
}

pub fn clear_unlocked_keyring_cache_for_test(control_dir: &Path) {
    if let Some(cache) = UNLOCKED_KEYRINGS.get() {
        cache.lock().unwrap().remove(control_dir);
    }
}

pub fn unlock_repo_secrets(control_dir: &Path, password: &str) -> Result<RepoSecrets> {
    let secrets = unlock_repo_secrets_uncached(control_dir, password)?;
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

fn unlock_repo_secrets_from_state(keyring: &KeyringState, password: &str) -> Result<RepoSecrets> {
    let password_envelope = keyring
        .envelopes
        .iter()
        .find(|envelope| envelope.kind == "password")
        .context("keyring has no password envelope")?;
    let sealed = decrypt_password_envelope(&keyring, password_envelope, password)?;
    let secrets = RepoSecrets {
        repo_id: keyring.repo_id.clone(),
        active_epoch: keyring.active_epoch,
        repo_dedup_key: decode_hex_key(&sealed.repo_dedup_key_hex)?,
        repo_ref_key: decode_hex_key(&sealed.repo_ref_key_hex)?,
        repo_manifest_enc_key: decode_hex_key(&sealed.repo_manifest_enc_key_hex)?,
        repo_nonce_key: decode_hex_key(&sealed.repo_nonce_key_hex)?,
    };
    Ok(secrets)
}

pub fn open_repo_secrets(control_dir: &Path) -> Result<RepoSecrets> {
    let cache = UNLOCKED_KEYRINGS.get_or_init(|| Mutex::new(HashMap::new()));
    let cache = cache.lock().unwrap();
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
    };
    let plaintext = postcard::to_stdvec(&sealed).context("failed to encode sealed repo secrets")?;
    let mut nonce = [0u8; 24];
    getrandom::fill(&mut nonce).map_err(|_| anyhow::anyhow!("failed to obtain keyring nonce"))?;
    let cipher = XChaCha20Poly1305::new((&key).into());
    let mut ciphertext = plaintext.clone();
    let associated_data = password_envelope_associated_data(repo_id, active_epoch);
    let auth_tag = cipher
        .encrypt_in_place_detached(XNonce::from_slice(&nonce), &associated_data, &mut ciphertext)
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
        password_hint,
        salt_hex: hex::encode(salt),
        nonce_hex: hex::encode(nonce),
        ciphertext_hex: hex::encode(
            postcard::to_stdvec(&record).context("failed to encode keyring password envelope")?,
        ),
        auth_tag_hex: hex::encode(auth_tag),
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
    let record_bytes =
        hex::decode(&envelope.ciphertext_hex).context("invalid keyring password envelope ciphertext")?;
    let record: PasswordEnvelopeRecord =
        postcard::from_bytes(&record_bytes).context("failed to decode keyring password envelope")?;
    ensure!(record.magic == *KEYRING_PASSWORD_MAGIC, "keyring unlock failed");
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

pub fn decode_hex_key(value: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(value).context("invalid key hex")?;
    ensure!(bytes.len() == 32, "invalid key length");
    let mut array = [0u8; 32];
    array.copy_from_slice(&bytes);
    Ok(array)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: PathBuf) -> Result<T> {
    let bytes = std::fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("failed to decode {}", path.display()))
}
