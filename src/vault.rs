use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use argon2::{Algorithm, Argon2, Params, Version};
use bip39::Mnemonic;
use chacha20poly1305::{
    KeyInit, XChaCha20Poly1305, XNonce,
    aead::{Aead, Generate},
};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

const MAGIC: &str = "silo-vault";
const VERSION: u32 = 1;
const SALT_LEN: usize = 16;
const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 24;

const ARGON2_M_COST: u32 = 65_536;
const ARGON2_T_COST: u32 = 3;
const ARGON2_P_COST: u32 = 1;

const ARGON2_M_COST_MAX: u32 = 1 << 21;
const ARGON2_T_COST_MAX: u32 = 64;
const ARGON2_P_COST_MAX: u32 = 16;

#[derive(Debug, thiserror::Error)]
pub enum UnlockError {
    #[error("wrong passphrase or corrupted vault")]
    Authentication,
    #[error(transparent)]
    Read(#[from] anyhow::Error),
}

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

    let mut salt = [0u8; SALT_LEN];
    crate::crypto::random_bytes(&mut salt);

    let key = derive_key(
        passphrase,
        &salt,
        ARGON2_M_COST,
        ARGON2_T_COST,
        ARGON2_P_COST,
    )?;
    let cipher =
        XChaCha20Poly1305::new_from_slice(&*key).map_err(|e| anyhow!("cipher init failed: {e}"))?;

    let nonce = XNonce::try_generate().map_err(|e| anyhow!("failed to generate nonce: {e}"))?;

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

#[cfg(test)]
pub fn unlock_vault(path: &Path, passphrase: &str) -> Result<Mnemonic> {
    Ok(unlock_vault_keyed(path, passphrase)?.0)
}

pub fn unlock_vault_keyed(
    path: &Path,
    passphrase: &str,
) -> std::result::Result<(Mnemonic, VaultKey), UnlockError> {
    let bytes = fs::read(path).with_context(|| format!("reading vault at {}", path.display()))?;
    let vault: VaultFile = serde_json::from_slice(&bytes).context("vault file is not valid")?;

    if vault.magic != MAGIC {
        return Err(anyhow!("not a silo vault file").into());
    }
    if vault.version != VERSION {
        return Err(anyhow!("unsupported vault version {}", vault.version).into());
    }
    if vault.kdf != "argon2id" {
        return Err(anyhow!("unsupported vault KDF {}", vault.kdf).into());
    }

    if vault.m_cost > ARGON2_M_COST_MAX
        || vault.t_cost > ARGON2_T_COST_MAX
        || vault.p_cost > ARGON2_P_COST_MAX
    {
        return Err(anyhow!("vault KDF parameters are out of bounds").into());
    }

    let salt = bs58::decode(&vault.salt_b58)
        .into_vec()
        .context("corrupt salt")?;
    if salt.len() != SALT_LEN {
        return Err(anyhow!("corrupt salt: expected {SALT_LEN} bytes").into());
    }
    let nonce_bytes = bs58::decode(&vault.nonce_b58)
        .into_vec()
        .context("corrupt nonce")?;
    let nonce = XNonce::try_from(nonce_bytes.as_slice())
        .map_err(|_| anyhow!("corrupt nonce: expected {NONCE_LEN} bytes"))?;
    let ciphertext = bs58::decode(&vault.ciphertext_b58)
        .into_vec()
        .context("corrupt ciphertext")?;
    if ciphertext.len() < 16 {
        return Err(anyhow!("corrupt ciphertext: missing authentication tag").into());
    }

    let key = derive_key(passphrase, &salt, vault.m_cost, vault.t_cost, vault.p_cost)?;
    let cipher =
        XChaCha20Poly1305::new_from_slice(&*key).map_err(|e| anyhow!("cipher init failed: {e}"))?;

    let plaintext = Zeroizing::new(
        cipher
            .decrypt(&nonce, ciphertext.as_ref())
            .map_err(|_| UnlockError::Authentication)?,
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
        "Couldn't choose a unique temp file for {file_name}"
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
    use crate::crypto::generate_mnemonic;
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
        let mnemonic = generate_mnemonic().unwrap();

        create_vault(&path, &mnemonic, "correct horse battery staple").unwrap();
        assert!(vault_exists(&path));

        let recovered = unlock_vault(&path, "correct horse battery staple").unwrap();
        assert_eq!(recovered.to_string(), mnemonic.to_string());
    }

    #[test]
    fn create_time_params_meet_cold_storage_floor() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.json");
        let mnemonic = generate_mnemonic().unwrap();
        create_vault(&path, &mnemonic, "pw").unwrap();

        let vault: VaultFile = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(
            vault.m_cost >= 65_536,
            "cold-storage vault must seal with at least 64 MiB of memory, got {}",
            vault.m_cost
        );
        assert!(
            vault.t_cost >= 2,
            "cold-storage vault must seal with at least 2 passes, got {}",
            vault.t_cost
        );
        assert!(vault.m_cost <= ARGON2_M_COST_MAX);
        assert!(vault.t_cost <= ARGON2_T_COST_MAX);
        assert!(vault.p_cost <= ARGON2_P_COST_MAX);
    }

    #[test]
    fn argon2_053_vault_fixtures_still_unlock() {
        // Generated with argon2 0.5.3 and chacha20poly1305 0.11.0, not the
        // library under test: Argon2id v19, 32-byte output, password "legacy-pw",
        // salt [7; 16], nonce [9; 24], and the mnemonic below. Cover both the
        // historical (19456, 2, 1) and current (65536, 3, 1) stored costs.
        let mnemonic = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
        let fixtures = [
            (
                r#"{"magic":"silo-vault","version":1,"kdf":"argon2id","m_cost":19456,"t_cost":2,"p_cost":1,"salt_b58":"sLDDSz4DCdAn1W1xf36N2","nonce_b58":"pmuWZgV4nJVHaioT79ABVXxCGYGnWrUG","ciphertext_b58":"88i8dL9V5w7iyvVwNndKf7HzTbmA6qfS5C5GV8Sd1cZP4Y9pNYN52dWtiuShtnkcStkj9YaTbkgtKcRpBErBhFJd7pubG3Ya19PjUZoEJf76fV9CNtMyP9MdLBDRFzpigYQb4E6NBrFSYEvb6bPdJ"}"#,
                "AztT36fz59WsWN9D7pqragcaZaCE2EFGYJ6UNSbbSjyf",
            ),
            (
                r#"{"magic":"silo-vault","version":1,"kdf":"argon2id","m_cost":65536,"t_cost":3,"p_cost":1,"salt_b58":"sLDDSz4DCdAn1W1xf36N2","nonce_b58":"pmuWZgV4nJVHaioT79ABVXxCGYGnWrUG","ciphertext_b58":"S5VeKM82sFoW8TKX9WC94S1H8tdXeA79BfmVunhbXXiZymjKVSQhqyWfw9RM9qmRCVvaTR7f5y52bqrx7uDWgzdKDsas4WkHmfWhw1trcaZ5AzjjmtnYWoPAiE4DobM3eNE1ibb9EyJoFJzM3Gt1m"}"#,
                "AxMyCTc6xHFG8n8Tgq7XaZZKSDUdTHrrUPHKJtc3fpAE",
            ),
        ];
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.json");
        for (json, expected_key) in fixtures {
            fs::write(&path, json).unwrap();
            let (recovered, key) = unlock_vault_keyed(&path, "legacy-pw").unwrap();
            assert_eq!(recovered.to_string(), mnemonic);
            assert_eq!(bs58::encode(key.as_bytes()).into_string(), expected_key);
            assert!(unlock_vault_keyed(&path, "wrong-pw").is_err());
            assert_eq!(fs::read_to_string(&path).unwrap(), json);
        }
    }

    #[test]
    fn wrong_passphrase_fails() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.json");
        let mnemonic = generate_mnemonic().unwrap();

        create_vault(&path, &mnemonic, "right-passphrase").unwrap();
        assert!(matches!(
            unlock_vault_keyed(&path, "wrong-passphrase"),
            Err(UnlockError::Authentication)
        ));
    }

    #[test]
    fn unlock_classifies_file_and_structure_errors_as_read_failures() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.json");
        let mnemonic = generate_mnemonic().unwrap();
        create_vault(&path, &mnemonic, "pw").unwrap();
        let original: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        for (field, value, diagnostic) in [
            ("magic", serde_json::json!("other"), "not a silo vault"),
            (
                "version",
                serde_json::json!(VERSION + 1),
                "unsupported vault version",
            ),
            ("kdf", serde_json::json!("other"), "unsupported vault KDF"),
            (
                "m_cost",
                serde_json::json!(ARGON2_M_COST_MAX + 1),
                "out of bounds",
            ),
            (
                "t_cost",
                serde_json::json!(ARGON2_T_COST_MAX + 1),
                "out of bounds",
            ),
            (
                "p_cost",
                serde_json::json!(ARGON2_P_COST_MAX + 1),
                "out of bounds",
            ),
            ("m_cost", serde_json::json!(0), "invalid Argon2 params"),
            ("t_cost", serde_json::json!(0), "invalid Argon2 params"),
            ("p_cost", serde_json::json!(0), "invalid Argon2 params"),
            ("salt_b58", serde_json::json!("0"), "corrupt salt"),
            ("salt_b58", serde_json::json!("1"), "corrupt salt"),
            ("nonce_b58", serde_json::json!("0"), "corrupt nonce"),
            ("nonce_b58", serde_json::json!("1"), "corrupt nonce"),
            (
                "ciphertext_b58",
                serde_json::json!("0"),
                "corrupt ciphertext",
            ),
            (
                "ciphertext_b58",
                serde_json::json!("1"),
                "missing authentication tag",
            ),
        ] {
            let mut changed = original.clone();
            changed[field] = value;
            fs::write(&path, serde_json::to_vec(&changed).unwrap()).unwrap();
            let err = unlock_vault_keyed(&path, "pw").unwrap_err();
            assert!(matches!(&err, UnlockError::Read(_)), "{field}: {err}");
            assert!(err.to_string().contains(diagnostic), "{field}: {err}");
        }

        fs::write(&path, b"{\"magic\":").unwrap();
        let err = unlock_vault_keyed(&path, "pw").unwrap_err();
        assert!(matches!(&err, UnlockError::Read(_)));
        assert!(err.to_string().contains("vault file is not valid"));

        fs::remove_file(&path).unwrap();
        let err = unlock_vault_keyed(&path, "pw").unwrap_err();
        assert!(matches!(&err, UnlockError::Read(_)));
        assert!(err.to_string().contains("reading vault at"));

        // A directory is an actual unreadable vault path on both Windows and Unix.
        fs::create_dir(&path).unwrap();
        assert!(matches!(
            unlock_vault_keyed(&path, "pw"),
            Err(UnlockError::Read(_))
        ));
    }

    #[test]
    fn refuses_to_overwrite_existing_vault() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.json");
        let m1 = generate_mnemonic().unwrap();
        let m2 = generate_mnemonic().unwrap();

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
        let mnemonic = generate_mnemonic().unwrap();
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
        let mnemonic = generate_mnemonic().unwrap();
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
        let mnemonic = generate_mnemonic().unwrap();
        create_vault(&path, &mnemonic, "pw").unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn corrupt_nonce_length_errors_not_panics() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.json");
        let mnemonic = generate_mnemonic().unwrap();
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
        let mnemonic = generate_mnemonic().unwrap();
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
        let mnemonic = generate_mnemonic().unwrap();
        create_vault(&path, &mnemonic, "pw").unwrap();

        let mut vault: VaultFile = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let mut ct = bs58::decode(&vault.ciphertext_b58).into_vec().unwrap();
        ct[0] ^= 0xFF;
        vault.ciphertext_b58 = bs58::encode(ct).into_string();
        fs::write(&path, serde_json::to_vec(&vault).unwrap()).unwrap();

        assert!(matches!(
            unlock_vault_keyed(&path, "pw"),
            Err(UnlockError::Authentication)
        ));
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
        let nonce = <&XNonce>::from(&nonce_bytes);
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
