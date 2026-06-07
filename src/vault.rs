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
    write_atomic_exclusive(path, &json).context("writing vault file")?;
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
    if salt.len() != SALT_LEN {
        return Err(anyhow!("corrupt salt: expected {SALT_LEN} bytes"));
    }
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
        std::str::from_utf8(&plaintext)
            .context("decrypted data is not valid UTF-8")?
            .to_owned(),
    );
    let mnemonic = Mnemonic::parse(phrase.as_str()).context("decrypted mnemonic is invalid")?;
    Ok((mnemonic, VaultKey(key)))
}

pub(crate) fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    write_atomic_inner(path, data, false)
}

fn write_atomic_exclusive(path: &Path, data: &[u8]) -> Result<()> {
    write_atomic_inner(path, data, true)
}

fn write_atomic_inner(path: &Path, data: &[u8], exclusive: bool) -> Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("vault path has no file name"))?
        .to_string_lossy();
    let tmp_path = unique_tmp_path(&dir, &file_name)?;

    let result = (|| -> Result<()> {
        {
            let mut tmp = private_create_new(&tmp_path)
                .with_context(|| format!("creating temp file {}", tmp_path.display()))?;
            tmp.write_all(data)?;
            tmp.sync_all()?;
        }

        if exclusive {
            fs::hard_link(&tmp_path, path).with_context(|| {
                format!(
                    "publishing {} -> {} without overwrite",
                    tmp_path.display(),
                    path.display()
                )
            })?;
            fs::remove_file(&tmp_path).ok();
        } else {
            replace_atomic(&tmp_path, path).with_context(|| {
                format!("renaming {} -> {}", tmp_path.display(), path.display())
            })?;
        }

        if let Ok(dir_file) = OpenOptions::new().read(true).open(&dir) {
            let _ = dir_file.sync_all();
        }
        Ok(())
    })();

    if result.is_err() {
        fs::remove_file(&tmp_path).ok();
    }
    result
}

fn unique_tmp_path(dir: &Path, file_name: &str) -> Result<PathBuf> {
    for _ in 0..16 {
        let mut b = [0u8; 8];
        crate::crypto::random_bytes(&mut b);
        let suffix: String = b.iter().map(|x| format!("{x:02x}")).collect();
        let tmp = dir.join(format!(".{file_name}.{suffix}.tmp"));
        if !tmp.exists() {
            return Ok(tmp);
        }
    }
    Err(anyhow!(
        "could not choose a unique temp file for {file_name}"
    ))
}

fn private_create_new(path: &Path) -> Result<File> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    Ok(opts.open(path)?)
}

#[cfg(not(windows))]
fn replace_atomic(tmp_path: &Path, path: &Path) -> Result<()> {
    Ok(fs::rename(tmp_path, path)?)
}

#[cfg(windows)]
fn replace_atomic(tmp_path: &Path, path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(fs::rename(tmp_path, path)?);
    }
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;

    fn wide(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    let replaced = wide(path.as_os_str());
    let replacement = wide(tmp_path.as_os_str());
    let ok = unsafe {
        ReplaceFileW(
            replaced.as_ptr(),
            replacement.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error()).context("replacing destination file");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{WordCount, generate_mnemonic};
    use proptest::prelude::*;
    use tempfile::tempdir;

    const PROPTEST_CASES: u32 = 48;

    fn fast_config() -> ProptestConfig {
        ProptestConfig {
            cases: PROPTEST_CASES,
            ..ProptestConfig::default()
        }
    }

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
    fn corrupt_salt_length_errors_not_panics() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.json");
        let mnemonic = generate_mnemonic(WordCount::Twelve).unwrap();
        create_vault(&path, &mnemonic, "pw").unwrap();

        let mut vault: VaultFile = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let mut salt = bs58::decode(&vault.salt_b58).into_vec().unwrap();
        salt.truncate(8);
        vault.salt_b58 = bs58::encode(salt).into_string();
        fs::write(&path, serde_json::to_vec(&vault).unwrap()).unwrap();

        let err = unlock_vault(&path, "pw").unwrap_err().to_string();
        assert!(err.contains("corrupt salt"));
    }

    #[test]
    fn oversized_salt_errors_not_panics() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.json");
        let mnemonic = generate_mnemonic(WordCount::Twelve).unwrap();
        create_vault(&path, &mnemonic, "pw").unwrap();

        let mut vault: VaultFile = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let mut salt = bs58::decode(&vault.salt_b58).into_vec().unwrap();
        salt.push(1);
        vault.salt_b58 = bs58::encode(salt).into_string();
        fs::write(&path, serde_json::to_vec(&vault).unwrap()).unwrap();

        let err = unlock_vault(&path, "pw").unwrap_err().to_string();
        assert!(err.contains("corrupt salt"));
    }

    #[cfg(unix)]
    #[test]
    fn vault_file_mode_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.json");
        let mnemonic = generate_mnemonic(WordCount::Twelve).unwrap();
        create_vault(&path, &mnemonic, "pw").unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
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

    #[test]
    fn non_utf8_plaintext_errors_not_panics() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.json");

        let salt = [7u8; SALT_LEN];
        let m_cost = 8;
        let t_cost = 1;
        let p_cost = 1;
        let key = derive_key("pw", &salt, m_cost, t_cost, p_cost).unwrap();
        let cipher = XChaCha20Poly1305::new_from_slice(&*key).unwrap();
        let nonce_bytes = [3u8; NONCE_LEN];
        let nonce = XNonce::from_slice(&nonce_bytes);
        let ciphertext = cipher
            .encrypt(nonce, [0xff, 0xfe, 0x80, 0x00].as_ref())
            .unwrap();

        let vault = VaultFile {
            magic: MAGIC.to_string(),
            version: VERSION,
            kdf: "argon2id".to_string(),
            m_cost,
            t_cost,
            p_cost,
            salt_b58: bs58::encode(salt).into_string(),
            nonce_b58: bs58::encode(nonce_bytes).into_string(),
            ciphertext_b58: bs58::encode(&ciphertext).into_string(),
        };
        fs::write(&path, serde_json::to_vec(&vault).unwrap()).unwrap();

        let err = unlock_vault(&path, "pw").unwrap_err().to_string();
        assert!(err.contains("not valid UTF-8"), "unexpected error: {err}");
    }

    proptest! {
        #![proptest_config(fast_config())]

        #[test]
        fn malformed_vault_decoding_never_unlocks(
            salt in prop::collection::vec(any::<u8>(), 0..40),
            nonce in prop::collection::vec(any::<u8>(), 0..48),
            ciphertext in prop::collection::vec(any::<u8>(), 0..64),
            mutation in 0u8..7,
        ) {
            prop_assume!(salt.len() != SALT_LEN || nonce.len() != NONCE_LEN || mutation != 0);
            let dir = tempdir().unwrap();
            let path = dir.path().join("vault.json");
            let mut vault = VaultFile {
                magic: MAGIC.to_string(),
                version: VERSION,
                kdf: "argon2id".to_string(),
                m_cost: 8,
                t_cost: 1,
                p_cost: 1,
                salt_b58: bs58::encode(&salt).into_string(),
                nonce_b58: bs58::encode(&nonce).into_string(),
                ciphertext_b58: bs58::encode(&ciphertext).into_string(),
            };
            match mutation {
                1 => vault.magic = "not-silo".to_string(),
                2 => vault.version = VERSION + 1,
                3 => vault.m_cost = ARGON2_M_COST_MAX + 1,
                4 => vault.t_cost = ARGON2_T_COST_MAX + 1,
                5 => vault.salt_b58 = "0".to_string(),
                6 => vault.nonce_b58 = "0".to_string(),
                _ => {}
            }
            fs::write(&path, serde_json::to_vec(&vault).unwrap()).unwrap();
            prop_assert!(unlock_vault(&path, "pw").is_err());
        }
    }
}
