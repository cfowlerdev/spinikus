//! Envelope encryption for secrets.
//!
//! Secret values are encrypted with a data key. Data keys live in the `data_keys` table,
//! encrypted ("wrapped") with the master key, which never touches the database. Every
//! sealed value records the data key that encrypted it, so the master key can be rotated
//! by rewrapping the data keys, without re-encrypting any values.
//!
//! Both layers use XChaCha20-Poly1305 with random 192-bit nonces, and store
//! `nonce || ciphertext || tag`.

use std::collections::HashMap;

use base64::{Engine, engine::general_purpose::STANDARD};
use chacha20poly1305::{
    KeyInit, XChaCha20Poly1305, XNonce,
    aead::{Aead, Generate, Payload}
};
use sha2::{Digest, Sha256};
use spinikus_core::id::DataKeyId;
use sqlx::SqlitePool;
use zeroize::Zeroizing;

use crate::{StoreError, error::parse_column};

const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 24;

#[derive(Debug, thiserror::Error)]
pub enum SecretsError {
    #[error("the master key must be {KEY_LEN} bytes, base64-encoded")]
    InvalidMasterKey,
    /// The database was set up with a different master key than the one configured.
    #[error("data keys in the database were wrapped by master key {stored}, but the configured master key is {configured}")]
    WrongMasterKey { stored: String, configured: String },
    #[error("data key `{0}` does not exist")]
    UnknownDataKey(DataKeyId),
    /// Authentication failed: the wrong key or context, or a tampered ciphertext.
    #[error("failed to decrypt a value sealed with data key `{0}`")]
    Decrypt(DataKeyId)
}

/// The key that wraps every data key. It comes from `spinikus.toml`, an environment variable
/// or a key file, never from the database.
pub struct MasterKey {
    key: Zeroizing<[u8; KEY_LEN]>,
    id: String
}

impl MasterKey {
    pub fn generate() -> Self {
        Self::from_bytes(Zeroizing::new(<[u8; KEY_LEN]>::generate()))
    }

    pub fn from_base64(encoded: &str) -> Result<Self, SecretsError> {
        let decoded = Zeroizing::new(STANDARD.decode(encoded).map_err(|_| SecretsError::InvalidMasterKey)?);
        let key: [u8; KEY_LEN] = decoded.as_slice().try_into().map_err(|_| SecretsError::InvalidMasterKey)?;
        Ok(Self::from_bytes(Zeroizing::new(key)))
    }

    fn from_bytes(key: Zeroizing<[u8; KEY_LEN]>) -> Self {
        let digest = Sha256::new().chain_update(b"spinikus master key id\0").chain_update(key.as_slice()).finalize();
        let id = digest[..8].iter().map(|byte| format!("{byte:02x}")).collect();
        Self { key, id }
    }

    /// The key as written to a key file.
    pub fn to_base64(&self) -> Zeroizing<String> {
        Zeroizing::new(STANDARD.encode(self.key.as_slice()))
    }

    /// A fingerprint that identifies the key without revealing it, safe to log.
    pub fn id(&self) -> &str {
        &self.id
    }

    fn cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new_from_slice(self.key.as_slice()).expect("the master key has the cipher's key length")
    }
}

impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MasterKey").field("id", &self.id).finish_non_exhaustive()
    }
}

/// A value encrypted with a data key, as stored next to the entity it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sealed {
    pub key_id: DataKeyId,
    pub ciphertext: Vec<u8>
}

/// The unwrapped data keys, held in memory for the life of the process.
pub struct Keyring {
    /// The newest data key, used to seal new values.
    active: DataKeyId,
    keys: HashMap<DataKeyId, XChaCha20Poly1305>
}

impl Keyring {
    /// Unwraps every data key with `master`, creating the first one on a new database.
    pub(crate) async fn load(pool: &SqlitePool, master: &MasterKey) -> Result<Self, StoreError> {
        // Immediate, so two processes starting on a new database cannot both create a first key.
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        let rows: Vec<(String, String, Vec<u8>)> =
            sqlx::query_as("SELECT id, master_key_id, wrapped_key FROM data_keys ORDER BY id").fetch_all(&mut *tx).await?;

        let wrapping = master.cipher();
        let mut keys = HashMap::with_capacity(rows.len().max(1));
        for (id, master_key_id, wrapped_key) in rows {
            if master_key_id != master.id() {
                return Err(SecretsError::WrongMasterKey { stored: master_key_id, configured: master.id().to_string() }.into());
            }
            let id: DataKeyId = parse_column("data key", "id", &id)?;
            let key = open_with(&wrapping, &wrapped_key, id.to_string().as_bytes()).ok_or(SecretsError::Decrypt(id))?;
            keys.insert(id, cipher_from(&key));
        }

        let active = match keys.keys().max() {
            Some(id) => *id,
            None => {
                let id = DataKeyId::new();
                let key = Zeroizing::new(<[u8; KEY_LEN]>::generate());
                sqlx::query("INSERT INTO data_keys (id, master_key_id, wrapped_key) VALUES (?, ?, ?)")
                    .bind(id.to_string())
                    .bind(master.id())
                    .bind(seal_with(&wrapping, key.as_slice(), id.to_string().as_bytes()))
                    .execute(&mut *tx)
                    .await?;
                tracing::info!(data_key = %id, master_key = master.id(), "created the first data key");
                keys.insert(id, cipher_from(key.as_slice()));
                id
            }
        };

        tx.commit().await?;
        Ok(Self { active, keys })
    }

    /// Encrypts `plaintext` with the active data key. `context` is authenticated but not stored:
    /// passing what the value belongs to (such as its row ID) stops a ciphertext from being
    /// copied to another row, since opening it there fails.
    pub fn seal(&self, plaintext: &[u8], context: &[u8]) -> Sealed {
        let cipher = &self.keys[&self.active];
        Sealed { key_id: self.active, ciphertext: seal_with(cipher, plaintext, context) }
    }

    /// Decrypts a sealed value, given the same `context` it was sealed with.
    pub fn open(&self, sealed: &Sealed, context: &[u8]) -> Result<Zeroizing<Vec<u8>>, SecretsError> {
        let cipher = self.keys.get(&sealed.key_id).ok_or(SecretsError::UnknownDataKey(sealed.key_id))?;
        open_with(cipher, &sealed.ciphertext, context).ok_or(SecretsError::Decrypt(sealed.key_id))
    }
}

fn cipher_from(key: &[u8]) -> XChaCha20Poly1305 {
    XChaCha20Poly1305::new_from_slice(key).expect("data keys have the cipher's key length")
}

fn seal_with(cipher: &XChaCha20Poly1305, plaintext: &[u8], context: &[u8]) -> Vec<u8> {
    let nonce = XNonce::generate();
    let ciphertext = cipher
        .encrypt(&nonce, Payload { msg: plaintext, aad: context })
        .expect("XChaCha20-Poly1305 encrypts any message that fits in memory");

    let mut sealed = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    sealed.extend_from_slice(&nonce);
    sealed.extend_from_slice(&ciphertext);
    sealed
}

fn open_with(cipher: &XChaCha20Poly1305, sealed: &[u8], context: &[u8]) -> Option<Zeroizing<Vec<u8>>> {
    let (nonce, ciphertext) = sealed.split_at_checked(NONCE_LEN)?;
    let nonce = XNonce::try_from(nonce).ok()?;
    cipher.decrypt(&nonce, Payload { msg: ciphertext, aad: context }).ok().map(Zeroizing::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::migrated_pool;

    #[test]
    fn master_key_round_trips_through_base64() {
        let key = MasterKey::generate();

        let parsed = MasterKey::from_base64(&key.to_base64()).unwrap();

        assert_eq!(parsed.id(), key.id());
        assert_eq!(key.id().len(), 16);
        assert_ne!(MasterKey::generate().id(), key.id());
    }

    #[test]
    fn rejects_malformed_master_keys() {
        assert!(matches!(MasterKey::from_base64("not base64!"), Err(SecretsError::InvalidMasterKey)));
        assert!(matches!(MasterKey::from_base64(&STANDARD.encode([0u8; 16])), Err(SecretsError::InvalidMasterKey)));
    }

    #[test]
    fn debug_does_not_show_the_key() {
        let key = MasterKey::generate();

        let debug = format!("{key:?}");

        assert!(debug.contains(key.id()));
        assert!(!debug.contains(key.to_base64().as_str()));
    }

    #[tokio::test]
    async fn values_survive_a_restart() {
        let (_dir, pool) = migrated_pool().await;
        let master = MasterKey::generate();
        let first = Keyring::load(&pool, &master).await.unwrap();
        let sealed = first.seal(b"hunter2", b"var_1");

        let second = Keyring::load(&pool, &master).await.unwrap();

        assert_eq!(second.active, first.active, "an existing data key is reused");
        assert_eq!(second.open(&sealed, b"var_1").unwrap().as_slice(), b"hunter2");
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM data_keys").fetch_one(&pool).await.unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn sealing_uses_a_fresh_nonce_each_time() {
        let (_dir, pool) = migrated_pool().await;
        let keyring = Keyring::load(&pool, &MasterKey::generate()).await.unwrap();

        let one = keyring.seal(b"hunter2", b"");
        let two = keyring.seal(b"hunter2", b"");

        assert_ne!(one.ciphertext, two.ciphertext);
        assert_eq!(one.ciphertext.len(), NONCE_LEN + b"hunter2".len() + 16);
    }

    #[tokio::test]
    async fn rejects_the_wrong_context_and_tampering() {
        let (_dir, pool) = migrated_pool().await;
        let keyring = Keyring::load(&pool, &MasterKey::generate()).await.unwrap();
        let sealed = keyring.seal(b"hunter2", b"var_1");
        let mut tampered = sealed.clone();
        *tampered.ciphertext.last_mut().unwrap() ^= 1;
        let truncated = Sealed { key_id: sealed.key_id, ciphertext: sealed.ciphertext[..10].to_vec() };

        assert!(matches!(keyring.open(&sealed, b"var_2"), Err(SecretsError::Decrypt(_))));
        assert!(matches!(keyring.open(&tampered, b"var_1"), Err(SecretsError::Decrypt(_))));
        assert!(matches!(keyring.open(&truncated, b"var_1"), Err(SecretsError::Decrypt(_))));
    }

    #[tokio::test]
    async fn rejects_unknown_data_keys() {
        let (_dir, pool) = migrated_pool().await;
        let keyring = Keyring::load(&pool, &MasterKey::generate()).await.unwrap();
        let sealed = Sealed { key_id: DataKeyId::new(), ..keyring.seal(b"hunter2", b"") };

        assert!(matches!(keyring.open(&sealed, b""), Err(SecretsError::UnknownDataKey(_))));
    }

    #[tokio::test]
    async fn refuses_to_start_with_another_master_key() {
        let (_dir, pool) = migrated_pool().await;
        let original = MasterKey::generate();
        Keyring::load(&pool, &original).await.unwrap();

        let result = Keyring::load(&pool, &MasterKey::generate()).await;

        assert!(
            matches!(&result, Err(StoreError::Secrets(SecretsError::WrongMasterKey { stored, .. })) if stored == original.id()),
            "{:?}",
            result.err()
        );
    }

    #[tokio::test]
    async fn data_keys_are_stored_wrapped() {
        let (_dir, pool) = migrated_pool().await;
        let master = MasterKey::generate();
        let keyring = Keyring::load(&pool, &master).await.unwrap();
        let wrapped: Vec<u8> = sqlx::query_scalar("SELECT wrapped_key FROM data_keys").fetch_one(&pool).await.unwrap();

        let unwrapped = open_with(&master.cipher(), &wrapped, keyring.active.to_string().as_bytes()).unwrap();

        assert_eq!(unwrapped.len(), KEY_LEN);
        assert!(!wrapped.windows(KEY_LEN).any(|window| window == unwrapped.as_slice()));
    }
}
