use anyhow::{Result, anyhow};
use ed25519_dalek::{Signer, SigningKey};

pub const SYSTEM_PROGRAM_ID: [u8; 32] = [0u8; 32];

pub const COMPUTE_BUDGET_PROGRAM_ID_B58: &str = "ComputeBudget111111111111111111111111111111";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PriorityFee {
    pub unit_limit: u32,
    pub micro_lamports_per_cu: u64,
}

fn write_compact_u16(out: &mut Vec<u8>, mut v: u16) {
    loop {
        let mut b = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            b |= 0x80;
        }
        out.push(b);
        if v == 0 {
            break;
        }
    }
}

fn compute_budget_program_id() -> [u8; 32] {
    address_to_bytes(COMPUTE_BUDGET_PROGRAM_ID_B58).expect("valid compute-budget program id")
}

pub fn build_transfer_message(
    from: &[u8; 32],
    to: &[u8; 32],
    lamports: u64,
    recent_blockhash: &[u8; 32],
    priority: Option<PriorityFee>,
) -> Vec<u8> {
    match priority {
        None => build_bare_transfer(from, to, lamports, recent_blockhash),
        Some(p) => build_priority_transfer(from, to, lamports, recent_blockhash, p),
    }
}

fn build_bare_transfer(
    from: &[u8; 32],
    to: &[u8; 32],
    lamports: u64,
    recent_blockhash: &[u8; 32],
) -> Vec<u8> {
    let mut m = Vec::with_capacity(160);
    m.push(1);
    m.push(0);
    m.push(1);
    write_compact_u16(&mut m, 3);
    m.extend_from_slice(from);
    m.extend_from_slice(to);
    m.extend_from_slice(&SYSTEM_PROGRAM_ID);
    m.extend_from_slice(recent_blockhash);
    write_compact_u16(&mut m, 1);
    m.push(2);
    write_compact_u16(&mut m, 2);
    m.push(0);
    m.push(1);
    write_compact_u16(&mut m, 12);
    m.extend_from_slice(&2u32.to_le_bytes());
    m.extend_from_slice(&lamports.to_le_bytes());
    m
}

fn build_priority_transfer(
    from: &[u8; 32],
    to: &[u8; 32],
    lamports: u64,
    recent_blockhash: &[u8; 32],
    p: PriorityFee,
) -> Vec<u8> {
    let cb = compute_budget_program_id();
    let mut m = Vec::with_capacity(256);
    m.push(1);
    m.push(0);
    m.push(2);
    write_compact_u16(&mut m, 4);
    m.extend_from_slice(from);
    m.extend_from_slice(to);
    m.extend_from_slice(&SYSTEM_PROGRAM_ID);
    m.extend_from_slice(&cb);
    m.extend_from_slice(recent_blockhash);

    write_compact_u16(&mut m, 3);

    m.push(3);
    write_compact_u16(&mut m, 0);
    write_compact_u16(&mut m, 5);
    m.push(0x02);
    m.extend_from_slice(&p.unit_limit.to_le_bytes());

    m.push(3);
    write_compact_u16(&mut m, 0);
    write_compact_u16(&mut m, 9);
    m.push(0x03);
    m.extend_from_slice(&p.micro_lamports_per_cu.to_le_bytes());

    m.push(2);
    write_compact_u16(&mut m, 2);
    m.push(0);
    m.push(1);
    write_compact_u16(&mut m, 12);
    m.extend_from_slice(&2u32.to_le_bytes());
    m.extend_from_slice(&lamports.to_le_bytes());
    m
}

pub fn assemble_tx(message: &[u8], sig_bytes: &[u8; 64]) -> Vec<u8> {
    let mut tx = Vec::with_capacity(1 + 64 + message.len());
    write_compact_u16(&mut tx, 1);
    tx.extend_from_slice(sig_bytes);
    tx.extend_from_slice(message);
    tx
}

pub fn sign_and_serialize(message: &[u8], sk: &SigningKey) -> (Vec<u8>, [u8; 64]) {
    let sig_bytes = sk.sign(message).to_bytes();
    let tx = assemble_tx(message, &sig_bytes);
    (tx, sig_bytes)
}

pub fn base64_encode(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

pub fn address_to_bytes(addr: &str) -> Result<[u8; 32]> {
    let v = bs58::decode(addr.trim())
        .into_vec()
        .map_err(|_| anyhow!("address is not valid base58"))?;
    v.try_into().map_err(|_| anyhow!("address is not 32 bytes"))
}

pub fn signature_to_base58(sig: &[u8; 64]) -> String {
    bs58::encode(sig).into_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{derive_signing_key, parse_mnemonic, seed_from_mnemonic};
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use proptest::prelude::*;

    const PROPTEST_CASES: u32 = 96;

    fn fast_config() -> ProptestConfig {
        ProptestConfig {
            cases: PROPTEST_CASES,
            ..ProptestConfig::default()
        }
    }

    const TEST_MNEMONIC: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    #[test]
    fn compact_u16_boundaries() {
        let cases: &[(u16, &[u8])] = &[
            (0, &[0x00]),
            (1, &[0x01]),
            (0x7f, &[0x7f]),
            (0x80, &[0x80, 0x01]),
            (0x3fff, &[0xff, 0x7f]),
            (0x4000, &[0x80, 0x80, 0x01]),
            (0xffff, &[0xff, 0xff, 0x03]),
        ];
        for (v, expected) in cases {
            let mut out = Vec::new();
            write_compact_u16(&mut out, *v);
            assert_eq!(&out, expected, "compact-u16 of {v}");
        }
    }

    #[test]
    fn base64_rfc4648_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn signature_is_deterministic() {
        let seed = seed_from_mnemonic(&parse_mnemonic(TEST_MNEMONIC).unwrap());
        let key = derive_signing_key(&seed, 0);
        let msg = build_transfer_message(&[1u8; 32], &[2u8; 32], 1000, &[3u8; 32], None);
        let (tx1, sig1) = sign_and_serialize(&msg, &key);
        let (tx2, sig2) = sign_and_serialize(&msg, &key);
        assert_eq!(sig1, sig2, "ed25519 signatures must be deterministic");
        assert_eq!(tx1, tx2);
        assert_eq!(tx1[0], 1);
        assert_eq!(&tx1[1..65], &sig1[..]);
        assert_eq!(&tx1[65..], &msg[..]);
    }

    #[test]
    fn signer_pubkey_matches_displayed_address() {
        let seed = seed_from_mnemonic(&parse_mnemonic(TEST_MNEMONIC).unwrap());
        let key = derive_signing_key(&seed, 3);
        let from = key.verifying_key().to_bytes();
        let displayed = crate::crypto::derive_address(&seed, 3);
        assert_eq!(address_to_bytes(&displayed).unwrap(), from);
    }

    #[test]
    fn message_bytes_match_official_solana() {
        use solana_address::Address;
        use solana_hash::Hash;
        use solana_message::legacy::Message;
        use solana_system_interface::instruction::transfer;

        let seed = seed_from_mnemonic(&parse_mnemonic(TEST_MNEMONIC).unwrap());
        let from = derive_signing_key(&seed, 0).verifying_key().to_bytes();
        let to = derive_signing_key(&seed, 1).verifying_key().to_bytes();
        let bh = [42u8; 32];

        for lamports in [1u64, 5_000, 1_000_000_000, 9_223_372_036_854_775_807] {
            let ours = build_transfer_message(&from, &to, lamports, &bh, None);

            let ix = transfer(
                &Address::new_from_array(from),
                &Address::new_from_array(to),
                lamports,
            );
            let msg = Message::new_with_blockhash(
                &[ix],
                Some(&Address::new_from_array(from)),
                &Hash::new_from_array(bh),
            );
            let theirs = msg.serialize();

            assert_eq!(ours, theirs, "message mismatch at lamports={lamports}");
        }
    }

    #[test]
    fn priority_message_bytes_match_official_solana() {
        use solana_address::Address;
        use solana_compute_budget_interface::ComputeBudgetInstruction;
        use solana_hash::Hash;
        use solana_message::legacy::Message;
        use solana_system_interface::instruction::transfer;

        let seed = seed_from_mnemonic(&parse_mnemonic(TEST_MNEMONIC).unwrap());
        let from = derive_signing_key(&seed, 0).verifying_key().to_bytes();
        let to = derive_signing_key(&seed, 1).verifying_key().to_bytes();
        let bh = [42u8; 32];

        for (limit, price, lamports) in [(450u32, 50_000u64, 1u64), (300, 1_000_000, 1_000_000_000)]
        {
            let ours = build_transfer_message(
                &from,
                &to,
                lamports,
                &bh,
                Some(PriorityFee {
                    unit_limit: limit,
                    micro_lamports_per_cu: price,
                }),
            );

            let ixs = [
                ComputeBudgetInstruction::set_compute_unit_limit(limit),
                ComputeBudgetInstruction::set_compute_unit_price(price),
                transfer(
                    &Address::new_from_array(from),
                    &Address::new_from_array(to),
                    lamports,
                ),
            ];
            let msg = Message::new_with_blockhash(
                &ixs,
                Some(&Address::new_from_array(from)),
                &Hash::new_from_array(bh),
            );
            let theirs = msg.serialize();

            assert_eq!(
                ours, theirs,
                "priority message mismatch at limit={limit} price={price} lamports={lamports}"
            );
        }
    }

    #[test]
    fn signature_matches_official_solana_keypair() {
        use ed25519_dalek::Signer;
        use solana_derivation_path::DerivationPath;
        use solana_keypair::seed_derivable::keypair_from_seed_and_derivation_path;

        let seed = seed_from_mnemonic(&parse_mnemonic(TEST_MNEMONIC).unwrap());
        let our_key = derive_signing_key(&seed, 0);
        let from = our_key.verifying_key().to_bytes();
        let to = derive_signing_key(&seed, 5).verifying_key().to_bytes();
        let bh = [11u8; 32];
        let msg = build_transfer_message(&from, &to, 7_000_000, &bh, None);

        let (_tx, our_sig) = sign_and_serialize(&msg, &our_key);

        let kp = keypair_from_seed_and_derivation_path(
            seed.seed_bytes(),
            Some(DerivationPath::new_bip44(Some(0), Some(0))),
        )
        .unwrap();
        let kp_bytes = kp.to_bytes();
        let solana_secret =
            ed25519_dalek::SigningKey::from_bytes(kp_bytes[..32].try_into().unwrap());
        let solana_sig = solana_secret.sign(&msg).to_bytes();

        assert_eq!(our_sig, solana_sig, "signature must match Solana's keypair");
    }

    proptest! {
        #![proptest_config(fast_config())]

        #[test]
        fn compact_u16_roundtrips_against_decoder(v in any::<u16>()) {
            let mut out = Vec::new();
            write_compact_u16(&mut out, v);
            let mut decoded = 0u32;
            let mut shift = 0;
            for (idx, byte) in out.iter().copied().enumerate() {
                decoded |= ((byte & 0x7f) as u32) << shift;
                if byte & 0x80 == 0 {
                    prop_assert_eq!(idx + 1, out.len());
                }
                shift += 7;
            }
            prop_assert_eq!(decoded, v as u32);
            prop_assert!(out.len() <= 3);
            prop_assert_eq!(out.last().map(|b| b & 0x80), Some(0));
        }

        #[test]
        fn base64_matches_reference(bytes in prop::collection::vec(any::<u8>(), 0..192)) {
            prop_assert_eq!(base64_encode(&bytes), STANDARD.encode(&bytes));
        }

        #[test]
        fn transfer_message_matches_official_solana(from in any::<[u8; 32]>(), to in any::<[u8; 32]>(), bh in any::<[u8; 32]>(), lamports in any::<u64>()) {
            use solana_address::Address;
            use solana_hash::Hash;
            use solana_message::legacy::Message;
            use solana_system_interface::instruction::transfer;

            let ours = build_transfer_message(&from, &to, lamports, &bh, None);
            let ix = transfer(
                &Address::new_from_array(from),
                &Address::new_from_array(to),
                lamports,
            );
            let theirs = Message::new_with_blockhash(
                &[ix],
                Some(&Address::new_from_array(from)),
                &Hash::new_from_array(bh),
            )
            .serialize();
            prop_assert_eq!(ours, theirs);
        }
    }
}
