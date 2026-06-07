pub const LAMPORTS_PER_SOL: u64 = 1_000_000_000;

pub const BASE_FEE_PER_SIG: u64 = 5_000;

pub const RENT_EXEMPT_MIN_0_DATA_FALLBACK: u64 = 890_880;

pub const MICRO_LAMPORTS_PER_LAMPORT: u64 = 1_000_000;

pub const COMPUTE_UNIT_LIMIT: u32 = 450;

pub const PRIORITY_PRESETS: [(&str, u64); 4] = [
    ("off", 0),
    ("normal", 50_000),
    ("high", 250_000),
    ("turbo", 1_000_000),
];

pub const DEFAULT_PRIORITY_FEE_MICRO: u64 = 50_000;

pub fn priority_fee_lamports(micro_price_per_cu: u64) -> u64 {
    (COMPUTE_UNIT_LIMIT as u64)
        .saturating_mul(micro_price_per_cu)
        .div_ceil(MICRO_LAMPORTS_PER_LAMPORT)
}

pub fn total_fee(micro_price_per_cu: u64) -> u64 {
    BASE_FEE_PER_SIG.saturating_add(priority_fee_lamports(micro_price_per_cu))
}

pub fn priority_label(micro_price_per_cu: u64) -> &'static str {
    PRIORITY_PRESETS
        .iter()
        .find(|(_, v)| *v == micro_price_per_cu)
        .map(|(name, _)| *name)
        .unwrap_or("custom")
}

pub fn next_priority_preset(micro_price_per_cu: u64) -> u64 {
    let idx = PRIORITY_PRESETS
        .iter()
        .position(|(_, v)| *v == micro_price_per_cu)
        .unwrap_or(0);
    PRIORITY_PRESETS[(idx + 1) % PRIORITY_PRESETS.len()].1
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AmountError {
    Empty,
    NotANumber,
    TooManyDecimals,
    Overflow,
}

impl std::fmt::Display for AmountError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            AmountError::Empty => "enter an amount",
            AmountError::NotANumber => "not a valid number",
            AmountError::TooManyDecimals => "too many decimals (max 9)",
            AmountError::Overflow => "amount is too large",
        };
        f.write_str(s)
    }
}

#[inline]
pub fn lamports_to_sol(l: u64) -> f64 {
    l as f64 / LAMPORTS_PER_SOL as f64
}

pub fn parse_sol_to_lamports(s: &str) -> Result<u64, AmountError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(AmountError::Empty);
    }
    let (int_part, frac_part) = match s.split_once('.') {
        Some((i, f)) => (i, f),
        None => (s, ""),
    };
    if frac_part.len() > 9 {
        return Err(AmountError::TooManyDecimals);
    }
    if !int_part.chars().all(|c| c.is_ascii_digit())
        || !frac_part.chars().all(|c| c.is_ascii_digit())
        || (int_part.is_empty() && frac_part.is_empty())
    {
        return Err(AmountError::NotANumber);
    }
    let int_val: u64 = if int_part.is_empty() {
        0
    } else {
        int_part.parse().map_err(|_| AmountError::Overflow)?
    };
    let int_lamports = int_val
        .checked_mul(LAMPORTS_PER_SOL)
        .ok_or(AmountError::Overflow)?;

    let mut frac_padded = String::from(frac_part);
    while frac_padded.len() < 9 {
        frac_padded.push('0');
    }
    let frac_lamports: u64 = frac_padded.parse().map_err(|_| AmountError::Overflow)?;

    int_lamports
        .checked_add(frac_lamports)
        .ok_or(AmountError::Overflow)
}

pub fn fiat_to_lamports(fiat: &str, price_per_sol: f64) -> Result<u64, AmountError> {
    let amount = parse_decimal_scaled(fiat, 9)?;
    let price = price_to_scaled(price_per_sol)?;
    let numerator = amount
        .checked_mul(LAMPORTS_PER_SOL as u128)
        .ok_or(AmountError::Overflow)?;
    let rounded = numerator
        .checked_add(price / 2)
        .ok_or(AmountError::Overflow)?
        / price;
    u64::try_from(rounded).map_err(|_| AmountError::Overflow)
}

fn price_to_scaled(price_per_sol: f64) -> Result<u128, AmountError> {
    if !(price_per_sol.is_finite() && price_per_sol > 0.0) {
        return Err(AmountError::NotANumber);
    }
    let price = parse_decimal_scaled(&format!("{price_per_sol:.9}"), 9)?;
    if price == 0 {
        return Err(AmountError::NotANumber);
    }
    Ok(price)
}

fn parse_decimal_scaled(s: &str, scale_digits: usize) -> Result<u128, AmountError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(AmountError::Empty);
    }
    let (int_part, frac_part) = match s.split_once('.') {
        Some((i, f)) => {
            if f.contains('.') {
                return Err(AmountError::NotANumber);
            }
            (i, f)
        }
        None => (s, ""),
    };
    if frac_part.len() > scale_digits {
        return Err(AmountError::TooManyDecimals);
    }
    if !int_part.chars().all(|c| c.is_ascii_digit())
        || !frac_part.chars().all(|c| c.is_ascii_digit())
        || (int_part.is_empty() && frac_part.is_empty())
    {
        return Err(AmountError::NotANumber);
    }
    let int_val: u128 = if int_part.is_empty() {
        0
    } else {
        int_part.parse().map_err(|_| AmountError::Overflow)?
    };
    let scale = 10u128
        .checked_pow(scale_digits as u32)
        .ok_or(AmountError::Overflow)?;
    let int_scaled = int_val.checked_mul(scale).ok_or(AmountError::Overflow)?;
    let mut frac_padded = String::from(frac_part);
    while frac_padded.len() < scale_digits {
        frac_padded.push('0');
    }
    let frac_scaled: u128 = if frac_padded.is_empty() {
        0
    } else {
        frac_padded.parse().map_err(|_| AmountError::Overflow)?
    };
    int_scaled
        .checked_add(frac_scaled)
        .ok_or(AmountError::Overflow)
}

pub fn format_lamports(l: u64) -> String {
    let whole = l / LAMPORTS_PER_SOL;
    let frac = l % LAMPORTS_PER_SOL;
    if frac == 0 {
        format!("{whole}")
    } else {
        let f = format!("{frac:09}");
        format!("{whole}.{}", f.trim_end_matches('0'))
    }
}

pub const SEND_MAX_FEE_HEADROOM: u64 = 10 * BASE_FEE_PER_SIG;

pub fn max_send_keep_alive(src_balance: u64, fee: u64, rent_exempt_min: u64) -> Option<u64> {
    src_balance
        .checked_sub(fee)?
        .checked_sub(rent_exempt_min)?
        .checked_sub(SEND_MAX_FEE_HEADROOM)
}

pub fn max_send_drain(src_balance: u64, fee: u64) -> Option<u64> {
    src_balance.checked_sub(fee)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const PROPTEST_CASES: u32 = 128;

    fn fast_config() -> ProptestConfig {
        ProptestConfig {
            cases: PROPTEST_CASES,
            ..ProptestConfig::default()
        }
    }

    #[test]
    fn parse_basic() {
        assert_eq!(parse_sol_to_lamports("1").unwrap(), 1_000_000_000);
        assert_eq!(parse_sol_to_lamports("0.1").unwrap(), 100_000_000);
        assert_eq!(parse_sol_to_lamports("1.005").unwrap(), 1_005_000_000);
        assert_eq!(parse_sol_to_lamports("0.000000001").unwrap(), 1);
        assert_eq!(parse_sol_to_lamports(".5").unwrap(), 500_000_000);
        assert_eq!(parse_sol_to_lamports("12").unwrap(), 12_000_000_000);
        assert_eq!(parse_sol_to_lamports("  2.5  ").unwrap(), 2_500_000_000);
    }

    #[test]
    fn parse_rejects() {
        assert_eq!(parse_sol_to_lamports(""), Err(AmountError::Empty));
        assert_eq!(parse_sol_to_lamports("   "), Err(AmountError::Empty));
        assert_eq!(parse_sol_to_lamports("."), Err(AmountError::NotANumber));
        assert_eq!(parse_sol_to_lamports("abc"), Err(AmountError::NotANumber));
        assert_eq!(parse_sol_to_lamports("-1"), Err(AmountError::NotANumber));
        assert_eq!(parse_sol_to_lamports("1.2.3"), Err(AmountError::NotANumber));
        assert_eq!(
            parse_sol_to_lamports("1.0000000001"),
            Err(AmountError::TooManyDecimals)
        );
        assert_eq!(
            parse_sol_to_lamports("20000000000"),
            Err(AmountError::Overflow)
        );
    }

    #[test]
    fn format_roundtrips() {
        assert_eq!(format_lamports(1_000_000_000), "1");
        assert_eq!(format_lamports(0), "0");
        assert_eq!(format_lamports(100_000_000), "0.1");
        assert_eq!(format_lamports(1_005_000_000), "1.005");
        assert_eq!(format_lamports(1), "0.000000001");
        for &l in &[0u64, 1, 5_000, 100_000_000, 1_005_000_000, u64::MAX] {
            let s = format_lamports(l);
            assert_eq!(parse_sol_to_lamports(&s).unwrap(), l, "roundtrip {l}");
        }
    }

    #[test]
    fn fiat_conversion() {
        assert_eq!(fiat_to_lamports("250", 100.0).unwrap(), 2_500_000_000);
        assert_eq!(fiat_to_lamports("100", 100.0).unwrap(), 1_000_000_000);
        assert_eq!(fiat_to_lamports("1", 100.0).unwrap(), 10_000_000);
        assert_eq!(fiat_to_lamports("0.000000001", 1.0).unwrap(), 1);
        assert_eq!(
            fiat_to_lamports("18446744073.709551615", 1.0).unwrap(),
            u64::MAX
        );
        assert_eq!(
            fiat_to_lamports("0.0000000001", 1.0),
            Err(AmountError::TooManyDecimals)
        );
        assert_eq!(fiat_to_lamports("1", 3.0).unwrap(), 333_333_333);
        assert_eq!(fiat_to_lamports("2", 3.0).unwrap(), 666_666_667);
        assert!(fiat_to_lamports("abc", 100.0).is_err());
        assert!(fiat_to_lamports("10", 0.0).is_err());
        assert!(fiat_to_lamports("10", f64::NAN).is_err());
        assert_eq!(fiat_to_lamports("", 100.0), Err(AmountError::Empty));
    }

    #[test]
    fn priority_fee_math() {
        assert_eq!(priority_fee_lamports(0), 0);
        assert_eq!(total_fee(0), BASE_FEE_PER_SIG);
        assert_eq!(priority_fee_lamports(50_000), 23);
        assert_eq!(total_fee(50_000), BASE_FEE_PER_SIG + 23);
        assert_eq!(priority_fee_lamports(1_000_000), 450);
        assert_eq!(priority_label(0), "off");
        assert_eq!(priority_label(50_000), "normal");
        assert_eq!(priority_label(999), "custom");
        assert_eq!(next_priority_preset(0), 50_000);
        assert_eq!(next_priority_preset(1_000_000), 0);
    }

    #[test]
    fn max_send_math() {
        assert_eq!(
            max_send_keep_alive(1_000_000, 5_000, 890_880),
            Some(104_120 - SEND_MAX_FEE_HEADROOM)
        );
        assert_eq!(max_send_keep_alive(800_000, 5_000, 890_880), None);
        assert_eq!(
            max_send_keep_alive(890_880 + 5_000 + 1, 5_000, 890_880),
            None
        );
        assert_eq!(max_send_drain(1_000_000, 5_000), Some(995_000));
        assert_eq!(max_send_drain(4_000, 5_000), None);
    }

    proptest! {
        #![proptest_config(fast_config())]

        #[test]
        fn formatted_lamports_roundtrip(lamports in any::<u64>()) {
            let formatted = format_lamports(lamports);
            prop_assert_eq!(parse_sol_to_lamports(&formatted), Ok(lamports));
            prop_assert!(!formatted.ends_with('.'));
            prop_assert!(!formatted.contains(".000000000"));
        }

        #[test]
        fn exact_decimal_lamports_parse(whole in 0u64..=18_446_744_073, frac in 0u64..LAMPORTS_PER_SOL) {
            let Some(expected) = whole
                .checked_mul(LAMPORTS_PER_SOL)
                .and_then(|v| v.checked_add(frac)) else {
                    return Ok(());
                };
            let amount = if frac == 0 {
                whole.to_string()
            } else {
                let frac = format!("{frac:09}");
                format!("{whole}.{}", frac.trim_end_matches('0'))
            };
            prop_assert_eq!(parse_sol_to_lamports(&amount), Ok(expected));
        }

        #[test]
        fn fiat_conversion_matches_integer_rounding(cents in 0u64..=1_000_000_000, price_cents in 1u64..=1_000_000_000) {
            let fiat = format!("{}.{:02}", cents / 100, cents % 100);
            let price = price_cents as f64 / 100.0;
            let expected = ((cents as u128) * (LAMPORTS_PER_SOL as u128) + (price_cents as u128 / 2)) / price_cents as u128;
            prop_assert_eq!(fiat_to_lamports(&fiat, price), u64::try_from(expected).map_err(|_| AmountError::Overflow));
        }

        #[test]
        fn priority_fee_is_ceil_saturating_product(price in any::<u64>()) {
            let expected = (COMPUTE_UNIT_LIMIT as u64)
                .saturating_mul(price)
                .div_ceil(MICRO_LAMPORTS_PER_LAMPORT);
            prop_assert_eq!(priority_fee_lamports(price), expected);
            prop_assert_eq!(total_fee(price), BASE_FEE_PER_SIG.saturating_add(expected));
        }
    }
}
