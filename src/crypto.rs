use anyhow::{Context, Result};
use bip39::Mnemonic;
use ed25519_dalek::SigningKey;
use slip10_ed25519::derive_ed25519_private_key;
use zeroize::{Zeroize, Zeroizing};

const PURPOSE: u32 = 44;
const SOLANA_COIN_TYPE: u32 = 501;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WordCount {
    Twelve,
    TwentyFour,
}

impl WordCount {
    fn words(self) -> usize {
        match self {
            WordCount::Twelve => 12,
            WordCount::TwentyFour => 24,
        }
    }
}

pub struct Seed(Zeroizing<[u8; 64]>);

impl Seed {
    fn as_bytes(&self) -> &[u8; 64] {
        &self.0
    }

    #[cfg(test)]
    pub(crate) fn seed_bytes(&self) -> &[u8; 64] {
        &self.0
    }
}

impl std::fmt::Debug for Seed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Seed([REDACTED])")
    }
}

pub fn generate_mnemonic(word_count: WordCount) -> Result<Mnemonic> {
    Mnemonic::generate(word_count.words()).context("failed to generate mnemonic")
}

pub fn parse_mnemonic(phrase: &str) -> Result<Mnemonic> {
    Mnemonic::parse(phrase.trim()).context("invalid recovery phrase")
}

pub fn random_bytes(buf: &mut [u8]) {
    use chacha20poly1305::aead::{OsRng, rand_core::RngCore};
    let mut rng = OsRng;
    rng.fill_bytes(buf);
}

pub fn hkdf_sha256(ikm: &[u8], salt: &[u8], info: &[u8], okm: &mut [u8]) -> Result<()> {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    type H = Hmac<Sha256>;

    if okm.len() > 255 * 32 {
        anyhow::bail!("HKDF-SHA256 output is too long");
    }

    let mut ext = H::new_from_slice(salt).expect("HMAC accepts any key length");
    ext.update(ikm);
    let prk = ext.finalize().into_bytes();

    let mut prev: Vec<u8> = Vec::new();
    let mut filled = 0usize;
    let mut counter: u8 = 1;
    while filled < okm.len() {
        let mut h = H::new_from_slice(&prk).expect("HMAC accepts any key length");
        h.update(&prev);
        h.update(info);
        h.update(&[counter]);
        let block = h.finalize().into_bytes();
        let take = (okm.len() - filled).min(block.len());
        okm[filled..filled + take].copy_from_slice(&block[..take]);
        prev = block.to_vec();
        filled += take;
        counter = counter.saturating_add(1);
    }
    prev.zeroize();
    Ok(())
}

pub fn word_suggestions(prefix: &str) -> Vec<&'static str> {
    bip39::Language::English.words_by_prefix(prefix).to_vec()
}

pub fn word_is_valid(w: &str) -> bool {
    !w.is_empty() && bip39::Language::English.words_by_prefix(w).contains(&w)
}

pub fn seed_from_mnemonic(mnemonic: &Mnemonic) -> Seed {
    Seed(Zeroizing::new(mnemonic.to_seed("")))
}

fn path(account_index: u32) -> [u32; 4] {
    [PURPOSE, SOLANA_COIN_TYPE, account_index, 0]
}

pub fn derive_signing_key(seed: &Seed, account_index: u32) -> SigningKey {
    let mut secret = derive_ed25519_private_key(seed.as_bytes(), &path(account_index));
    let key = SigningKey::from_bytes(&secret);
    secret.zeroize();
    key
}

pub fn derive_address(seed: &Seed, account_index: u32) -> String {
    let key = derive_signing_key(seed, account_index);
    bs58::encode(key.verifying_key().to_bytes()).into_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_MNEMONIC: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    #[test]
    fn hkdf_matches_rfc5869_test_case_1() {
        let ikm = [0x0bu8; 22];
        let salt: Vec<u8> = (0u8..=12).collect();
        let info: Vec<u8> = (0xf0u8..=0xf9).collect();
        let mut okm = [0u8; 42];
        hkdf_sha256(&ikm, &salt, &info, &mut okm).unwrap();
        let expected =
            "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865";
        let got: String = okm.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn hkdf_rejects_overlong_output() {
        let mut okm = vec![0u8; 255 * 32];
        hkdf_sha256(b"ikm", b"salt", b"info", &mut okm).unwrap();
        let mut too_long = vec![0u8; 255 * 32 + 1];
        assert!(hkdf_sha256(b"ikm", b"salt", b"info", &mut too_long).is_err());
    }

    #[test]
    fn autocomplete_invariants() {
        assert_eq!(word_suggestions("aban"), vec!["abandon"]);
        assert!(word_suggestions("ab").len() > 1);
        assert!(word_is_valid("abandon"));
        assert!(word_is_valid("zoo"));
        assert!(!word_is_valid("aban"));
        assert!(!word_is_valid(""));
        assert!(!word_is_valid("notaword"));
        assert!(word_suggestions("zoo").contains(&"zoo"));
    }

    #[test]
    fn generate_and_parse_roundtrip() {
        let m = generate_mnemonic(WordCount::Twelve).unwrap();
        assert_eq!(m.to_string().split_whitespace().count(), 12);
        let reparsed = parse_mnemonic(&m.to_string()).unwrap();
        assert_eq!(m.to_string(), reparsed.to_string());

        let m24 = generate_mnemonic(WordCount::TwentyFour).unwrap();
        assert_eq!(m24.to_string().split_whitespace().count(), 24);
    }

    #[test]
    fn rejects_invalid_mnemonic() {
        assert!(parse_mnemonic("not a real recovery phrase at all").is_err());
        assert!(parse_mnemonic(&"abandon ".repeat(12)).is_err());
    }

    #[test]
    fn seed_is_64_bytes_and_deterministic() {
        let m = parse_mnemonic(TEST_MNEMONIC).unwrap();
        let s1 = seed_from_mnemonic(&m);
        let s2 = seed_from_mnemonic(&m);
        assert_eq!(s1.as_bytes(), s2.as_bytes());
        assert_eq!(s1.as_bytes().len(), 64);
    }

    #[test]
    fn derivation_is_deterministic_and_index_distinct() {
        let seed = seed_from_mnemonic(&parse_mnemonic(TEST_MNEMONIC).unwrap());
        let master_a = derive_address(&seed, 0);
        let master_b = derive_address(&seed, 0);
        let sub1 = derive_address(&seed, 1);
        let sub2 = derive_address(&seed, 2);

        assert_eq!(master_a, master_b);
        assert_ne!(master_a, sub1);
        assert_ne!(sub1, sub2);

        let decoded = bs58::decode(&master_a).into_vec().unwrap();
        assert_eq!(decoded.len(), 32);
    }

    #[test]
    fn known_mnemonic_address_is_pinned() {
        let seed = seed_from_mnemonic(&parse_mnemonic(TEST_MNEMONIC).unwrap());
        let addr = derive_address(&seed, 0);
        assert_eq!(addr, "HAgk14JpMQLgt6rVgv7cBQFJWFto5Dqxi472uT3DKpqk");
    }

    #[test]
    fn matches_official_solana_derivation() {
        use solana_derivation_path::DerivationPath;
        use solana_keypair::seed_derivable::keypair_from_seed_and_derivation_path;

        let seed = seed_from_mnemonic(&parse_mnemonic(TEST_MNEMONIC).unwrap());

        for index in [0u32, 1, 2, 5, 17, 100, 1000] {
            let ours = derive_address(&seed, index);

            let path = DerivationPath::new_bip44(Some(index), Some(0));
            let official =
                keypair_from_seed_and_derivation_path(seed.as_bytes(), Some(path)).unwrap();
            let official_addr = bs58::encode(&official.to_bytes()[32..64]).into_string();

            assert_eq!(ours, official_addr, "mismatch at account index {index}");
        }
    }
}
