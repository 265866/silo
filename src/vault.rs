use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use argon2::{Algorithm, Argon2, Params, Version};
use bip39::Mnemonic;
use chacha20poly1305::{
    KeyInit, XChaCha20Poly1305, XNonce,
    aead::{Aead, AeadCore, OsRng, rand_core::RngCore},
};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

const MAGIC: &str = "silo-vault";
const VERSION: u32 = 1;
const SALT_LEN: usize = 16;
const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 24;

const ARGON2_M_COST: u32 = 19_456;
const ARGON2_T_COST: u32 = 2;
const ARGON2_P_COST: u32 = 1;

const ARGON2_M_COST_MAX: u32 = 1 << 21;
const ARGON2_T_COST_MAX: u32 = 64;
const ARGON2_P_COST_MAX: u32 = 16;

pub struct VaultKey(Zeroizing<[u8; KEY_LEN]>);

impl VaultKey {
    pub fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }
}

impl std::fmt::Debug for VaultKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("VaultKey([REDACTED])")
    }
}

#[derive(Serialize, Deserialize)]
struct VaultFile {
    magic: String,
    version: u32,
    kdf: String,
    m_cost: u32,
    t_cost: u32,
    p_cost: u32,
    salt_b58: String,
    nonce_b58: String,
    ciphertext_b58: String,
}

fn derive_key(
    passphrase: &str,
    salt: &[u8],
    m_cost: u32,
    t_cost: u32,
    p_cost: u32,
) -> Result<Zeroizing<[u8; KEY_LEN]>> {
    let params = Params::new(m_cost, t_cost, p_cost, Some(KEY_LEN))
        .map_err(|e| anyhow!("invalid Argon2 params: {e}"))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = Zeroizing::new([0u8; KEY_LEN]);
    argon2
        .hash_password_into(passphrase.as_bytes(), salt, key.as_mut_slice())
        .map_err(|e| anyhow!("key derivation failed: {e}"))?;
    Ok(key)
}

pub fn vault_exists(path: &Path) -> bool {
    path.exists()
}

pub fn create_vault(path: &Path, mnemonic: &Mnemonic, passphrase: &str) -> Result<VaultKey> {
    if vault_exists(path) {
        return Err(anyhow!(
            "a vault already exists at {} — refusing to overwrite",
            path.display()
        ));
    }

    let mut rng = OsRng;

    let mut salt = [0u8; SALT_LEN];
    rng.fill_bytes(&mut salt);

    let key = derive_key(
        passphrase,
        &salt,
        ARGON2_M_COST,
        ARGON2_T_COST,
        ARGON2_P_COST,
    )?;
    let cipher =
        XChaCha20Poly1305::new_from_slice(&*key).map_err(|e| anyhow!("cipher init failed: {e}"))?;

    let nonce = XChaCha20Poly1305::generate_nonce(&mut rng);

    let plaintext = Zeroizing::new(mnemonic.to_string());
    let ciphertext = cipher
        .encrypt(&nonce, plaintext.as_bytes())
        .map_err(|_| anyhow!("encryption failed"))?;

    let vault = VaultFile {
        magic: MAGIC.to_string(),
        version: VERSION,
        kdf: "argon2id".to_string(),
        m_cost: ARGON2_M_COST,
        t_cost: ARGON2_T_COST,
        p_cost: ARGON2_P_COST,
        salt_b58: bs58::encode(salt).into_string(),
        nonce_b58: bs58::encode(nonce).into_string(),
        ciphertext_b58: bs58::encode(ciphertext).into_string(),
    };

    let json = serde_json::to_vec_pretty(&vault).context("serializing vault")?;
    write_atomic(path, &json).context("writing vault file")?;
    Ok(VaultKey(key))
}

pub fn unlock_vault(path: &Path, passphrase: &str) -> Result<Mnemonic> {
    Ok(unlock_vault_keyed(path, passphrase)?.0)
}

pub fn unlock_vault_keyed(path: &Path, passphrase: &str) -> Result<(Mnemonic, VaultKey)> {
    let bytes = fs::read(path).with_context(|| format!("reading vault at {}", path.display()))?;
    let vault: VaultFile = serde_json::from_slice(&bytes).context("vault file is not valid")?;

    if vault.magic != MAGIC {
        return Err(anyhow!("not a silo vault file"));
    }
    if vault.version != VERSION {
        return Err(anyhow!("unsupported vault version {}", vault.version));
    }

    if vault.m_cost > ARGON2_M_COST_MAX
        || vault.t_cost > ARGON2_T_COST_MAX
        || vault.p_cost > ARGON2_P_COST_MAX
    {
        return Err(anyhow!("vault KDF parameters are out of bounds"));
    }

    let salt = bs58::decode(&vault.salt_b58)
        .into_vec()
        .context("corrupt salt")?;
    let nonce_bytes = bs58::decode(&vault.nonce_b58)
        .into_vec()
        .context("corrupt nonce")?;
    if nonce_bytes.len() != NONCE_LEN {
        return Err(anyhow!("corrupt nonce: expected {NONCE_LEN} bytes"));
    }
    let ciphertext = bs58::decode(&vault.ciphertext_b58)
        .into_vec()
        .context("corrupt ciphertext")?;

    let key = derive_key(passphrase, &salt, vault.m_cost, vault.t_cost, vault.p_cost)?;
    let cipher =
        XChaCha20Poly1305::new_from_slice(&*key).map_err(|e| anyhow!("cipher init failed: {e}"))?;

    let nonce = XNonce::from_slice(&nonce_bytes);
    let plaintext = Zeroizing::new(
        cipher
            .decrypt(nonce, ciphertext.as_ref())
            .map_err(|_| anyhow!("wrong passphrase or corrupted vault"))?,
    );

    let phrase = Zeroizing::new(
        String::from_utf8(plaintext.to_vec()).context("decrypted data is not valid UTF-8")?,
    );
    let mnemonic = Mnemonic::parse(phrase.as_str()).context("decrypted mnemonic is invalid")?;
    Ok((mnemonic, VaultKey(key)))
}

pub(crate) fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("vault path has no file name"))?
        .to_string_lossy();
    let tmp_path = dir.join(format!(".{file_name}.tmp"));

    {
        let mut tmp = File::create(&tmp_path)
            .with_context(|| format!("creating temp file {}", tmp_path.display()))?;
        tmp.write_all(data)?;
        tmp.sync_all()?;
    }

    fs::rename(&tmp_path, path)
        .with_context(|| format!("renaming {} -> {}", tmp_path.display(), path.display()))?;

    if let Ok(dir_file) = OpenOptions::new().read(true).open(&dir) {
        let _ = dir_file.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{WordCount, generate_mnemonic};
    use tempfile::tempdir;

    #[test]
    fn create_then_unlock_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.json");
        let mnemonic = generate_mnemonic(WordCount::Twelve).unwrap();

        create_vault(&path, &mnemonic, "correct horse battery staple").unwrap();
        assert!(vault_exists(&path));

        let recovered = unlock_vault(&path, "correct horse battery staple").unwrap();
        assert_eq!(recovered.to_string(), mnemonic.to_string());
    }

    #[test]
    fn wrong_passphrase_fails() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.json");
        let mnemonic = generate_mnemonic(WordCount::Twelve).unwrap();

        create_vault(&path, &mnemonic, "right-passphrase").unwrap();
        assert!(unlock_vault(&path, "wrong-passphrase").is_err());
    }

    #[test]
    fn refuses_to_overwrite_existing_vault() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.json");
        let m1 = generate_mnemonic(WordCount::Twelve).unwrap();
        let m2 = generate_mnemonic(WordCount::Twelve).unwrap();

        create_vault(&path, &m1, "pw").unwrap();
        assert!(create_vault(&path, &m2, "pw").is_err());
        assert_eq!(
            unlock_vault(&path, "pw").unwrap().to_string(),
            m1.to_string()
        );
    }

    #[test]
    fn corrupt_nonce_length_errors_not_panics() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.json");
        let mnemonic = generate_mnemonic(WordCount::Twelve).unwrap();
        create_vault(&path, &mnemonic, "pw").unwrap();

        let mut vault: VaultFile = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let mut nonce = bs58::decode(&vault.nonce_b58).into_vec().unwrap();
        nonce.truncate(20);
        vault.nonce_b58 = bs58::encode(nonce).into_string();
        fs::write(&path, serde_json::to_vec(&vault).unwrap()).unwrap();

        assert!(unlock_vault(&path, "pw").is_err());
    }

    #[test]
    fn out_of_bounds_kdf_params_rejected() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.json");
        let mnemonic = generate_mnemonic(WordCount::Twelve).unwrap();
        create_vault(&path, &mnemonic, "pw").unwrap();

        let mut vault: VaultFile = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        vault.m_cost = u32::MAX;
        fs::write(&path, serde_json::to_vec(&vault).unwrap()).unwrap();

        assert!(unlock_vault(&path, "pw").is_err());
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.json");
        let mnemonic = generate_mnemonic(WordCount::Twelve).unwrap();
        create_vault(&path, &mnemonic, "pw").unwrap();

        let mut vault: VaultFile = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let mut ct = bs58::decode(&vault.ciphertext_b58).into_vec().unwrap();
        ct[0] ^= 0xFF;
        vault.ciphertext_b58 = bs58::encode(ct).into_string();
        fs::write(&path, serde_json::to_vec(&vault).unwrap()).unwrap();

        assert!(unlock_vault(&path, "pw").is_err());
    }
}
