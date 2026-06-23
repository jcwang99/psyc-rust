use anyhow::{Context, Result, ensure};
use blake3::Hasher;
use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{Tag, XChaCha20Poly1305, XNonce};
use getrandom::fill as getrandom_fill;
use std::path::{Path, PathBuf};

use crate::local_backend::LocalFolderBackend;

const ENVELOPE_MAGIC: &[u8; 4] = b"E2V0";
const ENVELOPE_FORMAT_VERSION: u32 = 1;
const CRYPTO_SUITE: &str = "xchacha20poly1305";
const PADDING_POLICY_NONE: &str = "none";
const PADDING_POLICY_RANDOMIZED_MANIFEST: &str = "randomized-manifest-padding";
const NONCE_SIZE: usize = 24;
const TAG_SIZE: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoSecrets {
    pub repo_id: String,
    pub active_epoch: u32,
    pub repo_dedup_key: [u8; 32],
    pub repo_ref_key: [u8; 32],
    pub repo_manifest_enc_key: [u8; 32],
    pub repo_nonce_key: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhysicalObjectRef {
    pub layout_id: String,
    pub container_id: String,
    pub offset: Option<u64>,
    pub length: u64,
}

pub trait LogicalObjectStore {
    fn put_object(&self, object_type: &str, plaintext: &[u8]) -> Result<String>;
    fn get_object(&self, object_id: &str, expected_type: &str) -> Result<Vec<u8>>;
    fn get_object_range(
        &self,
        object_id: &str,
        expected_type: &str,
        offset: usize,
        length: usize,
    ) -> Result<Vec<u8>>;
    fn exists_object(&self, object_id: &str) -> bool;
    fn resolve_object(&self, object_id: &str) -> Result<PhysicalObjectRef>;
}

#[derive(Debug, Clone)]
pub struct DirectLayoutObjectStore {
    control_dir: PathBuf,
    backend: LocalFolderBackend,
    secrets: RepoSecrets,
}

impl DirectLayoutObjectStore {
    pub fn new(control_dir: impl AsRef<Path>, secrets: RepoSecrets) -> Self {
        Self {
            control_dir: control_dir.as_ref().to_path_buf(),
            backend: LocalFolderBackend::new(control_dir.as_ref()),
            secrets,
        }
    }

    pub fn preview_object_id(&self, object_type: &str, plaintext: &[u8]) -> String {
        self.derive_object_id(object_type, plaintext)
    }

    pub fn put_object(&self, object_type: &str, plaintext: &[u8]) -> Result<String> {
        let object_id = self.derive_object_id(object_type, plaintext);
        let padding_policy = self.padding_policy_for(object_type);
        let padded_plaintext = self.apply_padding_policy(object_type, plaintext, padding_policy)?;
        let nonce = self.nonce_for(object_type, &object_id, padding_policy)?;
        let (ciphertext, auth_tag) = self.encrypt_object(
            &object_id,
            object_type,
            padding_policy,
            &nonce,
            &padded_plaintext,
        )?;
        let envelope = EncryptedObjectEnvelope {
            format_version: ENVELOPE_FORMAT_VERSION,
            object_type: object_type.to_string(),
            crypto_suite: CRYPTO_SUITE.to_string(),
            key_epoch: self.secrets.active_epoch,
            padding_policy: padding_policy.to_string(),
            object_id: object_id.clone(),
            nonce,
            ciphertext,
            auth_tag,
        };

        self.backend
            .put_object(&self.relative_object_path(&object_id), &envelope.encode())
            .with_context(|| format!("failed to write object {}", object_id))?;

        Ok(object_id)
    }

    pub fn get_object(&self, object_id: &str, expected_type: &str) -> Result<Vec<u8>> {
        let bytes = self
            .backend
            .get_object(&self.relative_object_path(object_id))
            .with_context(|| format!("failed to read object {}", object_id))?;
        let envelope = EncryptedObjectEnvelope::decode(&bytes)?;

        ensure!(
            envelope.object_type == expected_type,
            "object type mismatch: expected {expected_type}, got {}",
            envelope.object_type
        );
        ensure!(
            envelope.crypto_suite == CRYPTO_SUITE,
            "unsupported crypto suite: {}",
            envelope.crypto_suite
        );
        ensure!(
            envelope.object_id == object_id,
            "object id mismatch in stored envelope"
        );

        let padded_plaintext = self.decrypt_object(&envelope)?;
        let plaintext = self.remove_padding_policy(&envelope.padding_policy, &padded_plaintext)?;
        let recomputed_id = self.derive_object_id(expected_type, &plaintext);
        ensure!(
            recomputed_id == object_id,
            "object authentication failed: object id mismatch"
        );

        Ok(plaintext)
    }

    pub fn get_object_range(
        &self,
        object_id: &str,
        expected_type: &str,
        offset: usize,
        length: usize,
    ) -> Result<Vec<u8>> {
        let bytes = self.get_object(object_id, expected_type)?;
        ensure!(offset <= bytes.len(), "range offset out of bounds");
        let end = offset.saturating_add(length).min(bytes.len());
        Ok(bytes[offset..end].to_vec())
    }

    pub fn exists_object(&self, object_id: &str) -> bool {
        self.backend
            .exists_object(&self.relative_object_path(object_id))
    }

    pub fn object_path(&self, object_id: &str) -> PathBuf {
        self.control_dir
            .join("objects")
            .join(format!("{object_id}.json"))
    }

    pub fn resolve_object(&self, object_id: &str) -> Result<PhysicalObjectRef> {
        let relative_path = self.relative_object_path(object_id);
        let bytes = self.backend.get_object(&relative_path)?;
        Ok(PhysicalObjectRef {
            layout_id: "direct".to_string(),
            container_id: relative_path,
            offset: None,
            length: bytes.len() as u64,
        })
    }

    fn relative_object_path(&self, object_id: &str) -> String {
        format!("objects/{object_id}.json")
    }

    fn derive_object_id(&self, object_type: &str, plaintext: &[u8]) -> String {
        let mut input = Vec::with_capacity(object_type.len() + 8 + plaintext.len());
        input.extend_from_slice(object_type.as_bytes());
        input.extend_from_slice(&(plaintext.len() as u64).to_le_bytes());
        input.extend_from_slice(plaintext);

        let hash = blake3::keyed_hash(&self.secrets.repo_dedup_key, &input);
        hex::encode(hash.as_bytes())
    }

    fn derive_nonce(
        &self,
        object_id: &str,
        object_type: &str,
        padding_policy: &str,
    ) -> [u8; NONCE_SIZE] {
        let mut hasher = Hasher::new_keyed(&self.secrets.repo_nonce_key);
        hasher.update(object_id.as_bytes());
        hasher.update(object_type.as_bytes());
        hasher.update(&ENVELOPE_FORMAT_VERSION.to_le_bytes());
        hasher.update(padding_policy.as_bytes());
        let hash = hasher.finalize();

        let mut nonce = [0u8; NONCE_SIZE];
        nonce.copy_from_slice(&hash.as_bytes()[..NONCE_SIZE]);
        nonce
    }

    fn nonce_for(
        &self,
        object_type: &str,
        object_id: &str,
        padding_policy: &str,
    ) -> Result<[u8; NONCE_SIZE]> {
        if object_type == "chunk" || padding_policy == PADDING_POLICY_NONE {
            return Ok(self.derive_nonce(object_id, object_type, padding_policy));
        }

        let mut nonce = [0u8; NONCE_SIZE];
        getrandom_fill(&mut nonce)
            .map_err(|_| anyhow::anyhow!("failed to obtain random object nonce"))?;
        Ok(nonce)
    }

    fn encrypt_object(
        &self,
        object_id: &str,
        object_type: &str,
        padding_policy: &str,
        nonce: &[u8; NONCE_SIZE],
        plaintext: &[u8],
    ) -> Result<(Vec<u8>, [u8; TAG_SIZE])> {
        let cipher = XChaCha20Poly1305::new((&self.secrets.repo_manifest_enc_key).into());
        let associated_data = self.associated_data(object_id, object_type, padding_policy);
        let mut buffer = plaintext.to_vec();
        let tag = cipher
            .encrypt_in_place_detached(XNonce::from_slice(nonce), &associated_data, &mut buffer)
            .map_err(|_| anyhow::anyhow!("failed to encrypt object"))?;

        let mut auth_tag = [0u8; TAG_SIZE];
        auth_tag.copy_from_slice(tag.as_slice());
        Ok((buffer, auth_tag))
    }

    fn decrypt_object(&self, envelope: &EncryptedObjectEnvelope) -> Result<Vec<u8>> {
        let cipher = XChaCha20Poly1305::new((&self.secrets.repo_manifest_enc_key).into());
        let associated_data = self.associated_data(
            &envelope.object_id,
            &envelope.object_type,
            &envelope.padding_policy,
        );
        let mut buffer = envelope.ciphertext.clone();

        cipher
            .decrypt_in_place_detached(
                XNonce::from_slice(&envelope.nonce),
                &associated_data,
                &mut buffer,
                Tag::from_slice(&envelope.auth_tag),
            )
            .map_err(|_| anyhow::anyhow!("object authentication failed"))?;

        Ok(buffer)
    }

    fn associated_data(&self, object_id: &str, object_type: &str, padding_policy: &str) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(ENVELOPE_MAGIC);
        data.extend_from_slice(&ENVELOPE_FORMAT_VERSION.to_le_bytes());
        data.extend_from_slice(self.secrets.repo_id.as_bytes());
        data.extend_from_slice(object_type.as_bytes());
        data.extend_from_slice(object_id.as_bytes());
        data.extend_from_slice(CRYPTO_SUITE.as_bytes());
        data.extend_from_slice(&self.secrets.active_epoch.to_le_bytes());
        data.extend_from_slice(padding_policy.as_bytes());
        data
    }

    fn padding_policy_for(&self, object_type: &str) -> &'static str {
        if object_type == "chunk" {
            PADDING_POLICY_NONE
        } else {
            PADDING_POLICY_RANDOMIZED_MANIFEST
        }
    }

    fn apply_padding_policy(
        &self,
        object_type: &str,
        plaintext: &[u8],
        padding_policy: &str,
    ) -> Result<Vec<u8>> {
        if object_type == "chunk" || padding_policy == PADDING_POLICY_NONE {
            return Ok(plaintext.to_vec());
        }

        let mut seed = [0u8; 1];
        getrandom_fill(&mut seed)
            .map_err(|_| anyhow::anyhow!("failed to obtain random padding seed"))?;
        let pad_len = (seed[0] as usize % 32) + 1;
        let mut bytes = Vec::with_capacity(4 + plaintext.len() + pad_len);
        bytes.extend_from_slice(&(pad_len as u32).to_le_bytes());
        bytes.extend_from_slice(plaintext);
        bytes.extend(std::iter::repeat_n(0u8, pad_len));
        Ok(bytes)
    }

    fn remove_padding_policy(&self, padding_policy: &str, plaintext: &[u8]) -> Result<Vec<u8>> {
        if padding_policy == PADDING_POLICY_NONE {
            return Ok(plaintext.to_vec());
        }

        ensure!(plaintext.len() >= 4, "object authentication failed");
        let mut pad_len_bytes = [0u8; 4];
        pad_len_bytes.copy_from_slice(&plaintext[..4]);
        let pad_len = u32::from_le_bytes(pad_len_bytes) as usize;
        ensure!(
            plaintext.len() >= 4 + pad_len,
            "object authentication failed"
        );
        let end = plaintext.len() - pad_len;
        Ok(plaintext[4..end].to_vec())
    }
}

impl LogicalObjectStore for DirectLayoutObjectStore {
    fn put_object(&self, object_type: &str, plaintext: &[u8]) -> Result<String> {
        DirectLayoutObjectStore::put_object(self, object_type, plaintext)
    }

    fn get_object(&self, object_id: &str, expected_type: &str) -> Result<Vec<u8>> {
        DirectLayoutObjectStore::get_object(self, object_id, expected_type)
    }

    fn get_object_range(
        &self,
        object_id: &str,
        expected_type: &str,
        offset: usize,
        length: usize,
    ) -> Result<Vec<u8>> {
        DirectLayoutObjectStore::get_object_range(self, object_id, expected_type, offset, length)
    }

    fn exists_object(&self, object_id: &str) -> bool {
        DirectLayoutObjectStore::exists_object(self, object_id)
    }

    fn resolve_object(&self, object_id: &str) -> Result<PhysicalObjectRef> {
        DirectLayoutObjectStore::resolve_object(self, object_id)
    }
}

#[derive(Debug, Clone)]
struct EncryptedObjectEnvelope {
    format_version: u32,
    object_type: String,
    crypto_suite: String,
    key_epoch: u32,
    padding_policy: String,
    object_id: String,
    nonce: [u8; NONCE_SIZE],
    ciphertext: Vec<u8>,
    auth_tag: [u8; TAG_SIZE],
}

impl EncryptedObjectEnvelope {
    fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(ENVELOPE_MAGIC);
        bytes.extend_from_slice(&self.format_version.to_le_bytes());
        push_string(&mut bytes, &self.object_type);
        push_string(&mut bytes, &self.crypto_suite);
        bytes.extend_from_slice(&self.key_epoch.to_le_bytes());
        push_string(&mut bytes, &self.padding_policy);
        push_string(&mut bytes, &self.object_id);
        bytes.push(NONCE_SIZE as u8);
        bytes.extend_from_slice(&self.nonce);
        bytes.extend_from_slice(&(self.ciphertext.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&self.ciphertext);
        bytes.extend_from_slice(&self.auth_tag);
        bytes
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut cursor = 0usize;
        ensure!(
            bytes.len() >= ENVELOPE_MAGIC.len(),
            "object authentication failed"
        );

        let magic = take_exact(bytes, &mut cursor, ENVELOPE_MAGIC.len())?;
        ensure!(magic == ENVELOPE_MAGIC, "object authentication failed");

        let format_version = u32::from_le_bytes(take_array(bytes, &mut cursor)?);
        ensure!(
            format_version == ENVELOPE_FORMAT_VERSION,
            "unsupported object format version"
        );

        let object_type = take_string(bytes, &mut cursor)?;
        let crypto_suite = take_string(bytes, &mut cursor)?;
        let key_epoch = u32::from_le_bytes(take_array(bytes, &mut cursor)?);
        let padding_policy = take_string(bytes, &mut cursor)?;
        let object_id = take_string(bytes, &mut cursor)?;
        let nonce_len = take_u8(bytes, &mut cursor)? as usize;
        ensure!(nonce_len == NONCE_SIZE, "object authentication failed");

        let nonce_bytes = take_exact(bytes, &mut cursor, nonce_len)?;
        let mut nonce = [0u8; NONCE_SIZE];
        nonce.copy_from_slice(nonce_bytes);

        let ciphertext_len = u64::from_le_bytes(take_array(bytes, &mut cursor)?) as usize;
        let ciphertext = take_exact(bytes, &mut cursor, ciphertext_len)?.to_vec();
        let auth_tag_bytes = take_exact(bytes, &mut cursor, TAG_SIZE)?;
        let mut auth_tag = [0u8; TAG_SIZE];
        auth_tag.copy_from_slice(auth_tag_bytes);
        ensure!(cursor == bytes.len(), "object authentication failed");

        Ok(Self {
            format_version,
            object_type,
            crypto_suite,
            key_epoch,
            padding_policy,
            object_id,
            nonce,
            ciphertext,
            auth_tag,
        })
    }
}

fn push_string(bytes: &mut Vec<u8>, value: &str) {
    bytes.push(value.len() as u8);
    bytes.extend_from_slice(value.as_bytes());
}

fn take_u8(bytes: &[u8], cursor: &mut usize) -> Result<u8> {
    let slice = take_exact(bytes, cursor, 1)?;
    Ok(slice[0])
}

fn take_string(bytes: &[u8], cursor: &mut usize) -> Result<String> {
    let len = take_u8(bytes, cursor)? as usize;
    let slice = take_exact(bytes, cursor, len)?;
    String::from_utf8(slice.to_vec()).context("object authentication failed")
}

fn take_exact<'a>(bytes: &'a [u8], cursor: &mut usize, len: usize) -> Result<&'a [u8]> {
    ensure!(
        bytes.len().saturating_sub(*cursor) >= len,
        "object authentication failed"
    );
    let slice = &bytes[*cursor..*cursor + len];
    *cursor += len;
    Ok(slice)
}

fn take_array<const N: usize>(bytes: &[u8], cursor: &mut usize) -> Result<[u8; N]> {
    let slice = take_exact(bytes, cursor, N)?;
    let mut array = [0u8; N];
    array.copy_from_slice(slice);
    Ok(array)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use tempfile::tempdir;

    use super::{
        DirectLayoutObjectStore, EncryptedObjectEnvelope, LogicalObjectStore, RepoSecrets,
    };

    fn secrets(repo_id: &str) -> RepoSecrets {
        RepoSecrets {
            repo_id: repo_id.to_string(),
            active_epoch: 1,
            repo_dedup_key: [1u8; 32],
            repo_ref_key: [4u8; 32],
            repo_manifest_enc_key: [2u8; 32],
            repo_nonce_key: [3u8; 32],
        }
    }

    #[test]
    fn put_object_makes_exists_object_true() {
        let temp = tempdir().unwrap();
        let store = DirectLayoutObjectStore::new(temp.path(), secrets("repo-a"));

        let object_id = store.put_object("chunk", b"hello world").unwrap();

        assert!(store.exists_object(&object_id));
    }

    #[test]
    fn get_object_range_reads_authenticated_slice() {
        let temp = tempdir().unwrap();
        let store = DirectLayoutObjectStore::new(temp.path(), secrets("repo-a"));

        let object_id = store.put_object("chunk", b"hello world").unwrap();
        let slice = store.get_object_range(&object_id, "chunk", 6, 5).unwrap();

        assert_eq!(slice, b"world");
    }

    #[test]
    fn object_path_uses_loose_object_location() {
        let temp = tempdir().unwrap();
        let store = DirectLayoutObjectStore::new(temp.path(), secrets("repo-a"));

        let path = store.object_path("abc123");

        assert_eq!(path, temp.path().join("objects").join("abc123.json"));
    }

    #[test]
    fn resolve_object_returns_direct_layout_physical_reference() {
        let temp = tempdir().unwrap();
        let store = DirectLayoutObjectStore::new(temp.path(), secrets("repo-a"));

        let object_id = store.put_object("chunk", b"hello world").unwrap();
        let reference = store.resolve_object(&object_id).unwrap();

        assert_eq!(reference.layout_id, "direct");
        assert_eq!(reference.container_id, format!("objects/{object_id}.json"));
        assert_eq!(reference.offset, None);
        assert!(reference.length > 0);
    }

    #[test]
    fn trait_object_store_contract_supports_round_trip() {
        let temp = tempdir().unwrap();
        let store = DirectLayoutObjectStore::new(temp.path(), secrets("repo-a"));
        let object_store: &dyn LogicalObjectStore = &store;

        let object_id = object_store.put_object("chunk", b"hello world").unwrap();
        let bytes = object_store.get_object(&object_id, "chunk").unwrap();

        assert_eq!(bytes, b"hello world");
    }

    #[test]
    fn chunk_plaintext_and_tree_plaintext_produce_distinct_object_ids() {
        let temp = tempdir().unwrap();
        let store = DirectLayoutObjectStore::new(temp.path(), secrets("repo-a"));

        let chunk_id = store.put_object("chunk", b"same-plaintext").unwrap();
        let tree_id = store.put_object("tree", b"same-plaintext").unwrap();

        assert_ne!(chunk_id, tree_id);
    }

    #[test]
    fn public_object_bytes_cannot_be_reused_across_repositories() {
        let temp = tempdir().unwrap();
        let repo_a = temp.path().join("repo-a");
        let repo_b = temp.path().join("repo-b");
        let store_a = DirectLayoutObjectStore::new(&repo_a, secrets("repo-a"));
        let store_b = DirectLayoutObjectStore::new(&repo_b, secrets("repo-b"));

        let object_id = store_a.put_object("chunk", b"same-plaintext").unwrap();
        let bytes = fs::read(store_a.object_path(&object_id)).unwrap();
        fs::create_dir_all(repo_b.join("objects")).unwrap();
        fs::write(store_b.object_path(&object_id), bytes).unwrap();

        let error = store_b.get_object(&object_id, "chunk").unwrap_err();

        assert!(
            error.to_string().contains("authentication") || error.to_string().contains("mismatch"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn reading_chunk_object_as_tree_is_rejected() {
        let temp = tempdir().unwrap();
        let store = DirectLayoutObjectStore::new(temp.path(), secrets("repo-a"));

        let chunk_id = store.put_object("chunk", b"same-plaintext").unwrap();
        let error = store.get_object(&chunk_id, "tree").unwrap_err();

        assert!(
            error.to_string().contains("type mismatch")
                || error.to_string().contains("authentication"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn tampering_envelope_object_type_is_detected_by_authentication() {
        let temp = tempdir().unwrap();
        let store = DirectLayoutObjectStore::new(temp.path(), secrets("repo-a"));

        let chunk_id = store.put_object("chunk", b"same-plaintext").unwrap();
        let object_path = store.object_path(&chunk_id);
        let bytes = fs::read(&object_path).unwrap();
        let mut envelope = EncryptedObjectEnvelope::decode(&bytes).unwrap();
        envelope.object_type = "tree!".to_string();
        fs::write(&object_path, envelope.encode()).unwrap();

        let error = store.get_object(&chunk_id, "tree!").unwrap_err();

        assert!(
            error.to_string().contains("authentication"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn tampering_envelope_crypto_suite_is_rejected() {
        let temp = tempdir().unwrap();
        let store = DirectLayoutObjectStore::new(temp.path(), secrets("repo-a"));

        let chunk_id = store.put_object("chunk", b"same-plaintext").unwrap();
        let object_path = store.object_path(&chunk_id);
        let bytes = fs::read(&object_path).unwrap();
        let mut envelope = EncryptedObjectEnvelope::decode(&bytes).unwrap();
        envelope.crypto_suite = "not-a-real-suite".to_string();
        fs::write(&object_path, envelope.encode()).unwrap();

        let error = store.get_object(&chunk_id, "chunk").unwrap_err();

        assert!(
            error.to_string().contains("crypto suite")
                || error.to_string().contains("authentication"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn tampering_manifest_padding_length_is_rejected() {
        let temp = tempdir().unwrap();
        let store = DirectLayoutObjectStore::new(temp.path(), secrets("repo-a"));

        let object_id = store.put_object("tree", b"manifest-bytes").unwrap();
        let object_path = store.object_path(&object_id);
        let bytes = fs::read(&object_path).unwrap();
        let mut envelope = EncryptedObjectEnvelope::decode(&bytes).unwrap();
        let mut padded_plaintext = store.decrypt_object(&envelope).unwrap();
        padded_plaintext[..4].copy_from_slice(&(u32::MAX).to_le_bytes());
        let (ciphertext, auth_tag) = store
            .encrypt_object(
                &envelope.object_id,
                &envelope.object_type,
                &envelope.padding_policy,
                &envelope.nonce,
                &padded_plaintext,
            )
            .unwrap();
        envelope.ciphertext = ciphertext;
        envelope.auth_tag = auth_tag;
        fs::write(&object_path, envelope.encode()).unwrap();

        let error = store.get_object(&object_id, "tree").unwrap_err();

        assert!(
            error.to_string().contains("authentication"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn tampering_envelope_format_version_is_rejected() {
        let temp = tempdir().unwrap();
        let store = DirectLayoutObjectStore::new(temp.path(), secrets("repo-a"));

        let object_id = store.put_object("chunk", b"same-plaintext").unwrap();
        let object_path = store.object_path(&object_id);
        let bytes = fs::read(&object_path).unwrap();
        let mut envelope = EncryptedObjectEnvelope::decode(&bytes).unwrap();
        envelope.format_version += 1;
        fs::write(&object_path, envelope.encode()).unwrap();

        let error = store.get_object(&object_id, "chunk").unwrap_err();

        assert!(
            error.to_string().contains("format version")
                || error.to_string().contains("authentication"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn tampering_envelope_nonce_length_is_rejected() {
        let temp = tempdir().unwrap();
        let store = DirectLayoutObjectStore::new(temp.path(), secrets("repo-a"));

        let object_id = store.put_object("chunk", b"same-plaintext").unwrap();
        let object_path = store.object_path(&object_id);
        let mut bytes = fs::read(&object_path).unwrap();

        let object_type_len = bytes[8] as usize;
        let crypto_suite_len = bytes[9 + object_type_len] as usize;
        let nonce_len_index =
            9 + object_type_len + 1 + crypto_suite_len + 4 + 1 + "none".len() + 1 + 64;
        bytes[nonce_len_index] = 0;
        fs::write(&object_path, &bytes).unwrap();

        let error = store.get_object(&object_id, "chunk").unwrap_err();

        assert!(
            error.to_string().contains("authentication"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn malformed_envelope_length_headers_return_error_without_panic() {
        let temp = tempdir().unwrap();
        let store = DirectLayoutObjectStore::new(temp.path(), secrets("repo-a"));

        let object_id = store.put_object("chunk", b"same-plaintext").unwrap();
        let object_path = store.object_path(&object_id);
        let mut bytes = fs::read(&object_path).unwrap();
        bytes.truncate(bytes.len() - 5);
        fs::write(&object_path, &bytes).unwrap();

        let result = std::panic::catch_unwind(|| store.get_object(&object_id, "chunk"));

        match result {
            Ok(Err(error)) => assert!(
                error.to_string().contains("authentication"),
                "unexpected error: {error:#}"
            ),
            Ok(Ok(_)) => panic!("expected malformed envelope to be rejected"),
            Err(_) => panic!("malformed envelope should not panic"),
        }
    }

    #[test]
    fn chunk_objects_use_none_padding_policy() {
        let temp = tempdir().unwrap();
        let store = DirectLayoutObjectStore::new(temp.path(), secrets("repo-a"));

        let object_id = store.put_object("chunk", b"hello world").unwrap();
        let bytes = fs::read(store.object_path(&object_id)).unwrap();

        assert!(bytes.windows("none".len()).any(|window| window == b"none"));
    }

    #[test]
    fn manifest_objects_use_randomized_manifest_padding_policy() {
        let temp = tempdir().unwrap();
        let store = DirectLayoutObjectStore::new(temp.path(), secrets("repo-a"));

        let object_id = store.put_object("tree", b"manifest-bytes").unwrap();
        let bytes = fs::read(store.object_path(&object_id)).unwrap();

        assert!(
            bytes
                .windows("randomized-manifest-padding".len())
                .any(|window| window == b"randomized-manifest-padding")
        );
    }

    #[test]
    fn randomized_manifest_padding_changes_nonce_but_not_object_id() {
        let temp = tempdir().unwrap();
        let store = DirectLayoutObjectStore::new(temp.path(), secrets("repo-a"));

        let object_id = store.put_object("tree", b"manifest-bytes").unwrap();
        let first_bytes = fs::read(store.object_path(&object_id)).unwrap();
        let same_object_id = store.put_object("tree", b"manifest-bytes").unwrap();
        let second_bytes = fs::read(store.object_path(&same_object_id)).unwrap();
        let first_envelope = EncryptedObjectEnvelope::decode(&first_bytes).unwrap();
        let second_envelope = EncryptedObjectEnvelope::decode(&second_bytes).unwrap();

        assert_eq!(object_id, same_object_id);
        assert_ne!(first_envelope.nonce, second_envelope.nonce);
    }
}
