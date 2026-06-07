use crate::money::{LAMPORTS_PER_SOL, lamports_to_sol};
use crate::price::SolPrice;

pub fn fmt_sol(lamports: u64) -> String {
    let whole = lamports / LAMPORTS_PER_SOL;
    let frac4 = (lamports % LAMPORTS_PER_SOL) / 100_000;
    format!("{}.{:04}", with_commas(whole), frac4)
}

pub fn fmt_sol_exact(lamports: u64) -> String {
    crate::money::format_lamports(lamports)
}

pub fn fmt_usd(price: Option<SolPrice>, lamports: u64) -> String {
    match price {
        Some(p) => {
            let value = lamports_to_sol(lamports) * p.value;
            format!(
                "{}{}",
                p.currency.symbol(),
                with_commas_decimals(value, p.currency.decimals())
            )
        }
        None => "—".to_string(),
    }
}

pub fn fmt_price(price: Option<SolPrice>) -> String {
    match price {
        Some(p) if p.is_stale() => format!(
            "SOL {}{} (stale {}m)",
            p.currency.symbol(),
            with_commas_decimals(p.value, p.currency.decimals()),
            p.age_secs() / 60
        ),
        Some(p) => format!(
            "SOL {}{}",
            p.currency.symbol(),
            with_commas_decimals(p.value, p.currency.decimals())
        ),
        None => "SOL —".to_string(),
    }
}

pub fn elide_addr(addr: &str) -> String {
    let chars: Vec<char> = addr.chars().collect();
    if chars.len() <= 10 {
        addr.to_string()
    } else {
        let head: String = chars[..4].iter().collect();
        let tail: String = chars[chars.len() - 4..].iter().collect();
        format!("{head}…{tail}")
    }
}

pub fn input_tail(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n <= width {
        return s.to_string();
    }
    if width <= 1 {
        return "…".to_string();
    }
    let tail: String = s.chars().skip(n - (width - 1)).collect();
    format!("…{tail}")
}

pub fn elide_middle(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    if max <= 1 {
        return "…".to_string();
    }
    let keep = max - 1;
    let head = keep.div_ceil(2);
    let tail = keep - head;
    let chars: Vec<char> = s.chars().collect();
    let head_s: String = chars[..head].iter().collect();
    let tail_s: String = chars[n - tail..].iter().collect();
    format!("{head_s}…{tail_s}")
}

pub fn truncate_end(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    if max <= 1 {
        return "…".to_string();
    }
    let head: String = s.chars().take(max - 1).collect();
    format!("{head}…")
}

pub fn wrap_lines(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out: Vec<String> = Vec::new();
    for segment in text.split('\n') {
        let mut cur = String::new();
        let mut cur_w = 0usize;
        for word in segment.split(' ') {
            let wlen = word.chars().count();
            if wlen > width {
                if cur_w > 0 {
                    out.push(std::mem::take(&mut cur));
                    cur_w = 0;
                }
                for ch in word.chars() {
                    if cur_w == width {
                        out.push(std::mem::take(&mut cur));
                        cur_w = 0;
                    }
                    cur.push(ch);
                    cur_w += 1;
                }
                continue;
            }
            let need = if cur_w == 0 { wlen } else { cur_w + 1 + wlen };
            if need > width {
                out.push(std::mem::take(&mut cur));
                cur.push_str(word);
                cur_w = wlen;
            } else {
                if cur_w > 0 {
                    cur.push(' ');
                    cur_w += 1;
                }
                cur.push_str(word);
                cur_w += wlen;
            }
        }
        out.push(cur);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

pub fn fmt_relative_time(ts_ms: i64) -> String {
    let now = crate::db::now_ms();
    let secs = (now - ts_ms).max(0) / 1000;
    if secs < 5 {
        "just now".to_string()
    } else if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

fn with_commas(n: u64) -> String {
    let s = n.to_string();
    let len = s.len();
    let mut out = String::with_capacity(len + len / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn with_commas_decimals(v: f64, decimals: usize) -> String {
    let v = if v < 0.0 { 0.0 } else { v };
    if decimals == 0 {
        return with_commas(v.round() as u64);
    }
    let scale = 10u128.pow(decimals as u32);
    let scaled = (v * scale as f64).round() as u128;
    let whole = (scaled / scale) as u64;
    let frac = (scaled % scale) as u64;
    format!("{}.{:0width$}", with_commas(whole), frac, width = decimals)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commas() {
        assert_eq!(with_commas(0), "0");
        assert_eq!(with_commas(12), "12");
        assert_eq!(with_commas(123), "123");
        assert_eq!(with_commas(1234), "1,234");
        assert_eq!(with_commas(1234567), "1,234,567");
        assert_eq!(with_commas(1000000), "1,000,000");
    }

    #[test]
    fn sol_format() {
        assert_eq!(fmt_sol(0), "0.0000");
        assert_eq!(fmt_sol(LAMPORTS_PER_SOL), "1.0000");
        assert_eq!(fmt_sol(124_500_000_000), "124.5000");
        assert_eq!(fmt_sol(1_234_500_000_000), "1,234.5000");
    }

    #[test]
    fn usd_format() {
        let p = SolPrice {
            value: 146.2,
            currency: crate::types::Currency::Usd,
            fetched_at: crate::db::now_ms() as u64 / 1000,
            source: crate::price::PriceSource::CoinGecko,
        };
        assert_eq!(fmt_usd(Some(p), 124_500_000_000), "$18,201.90");
        assert_eq!(fmt_usd(None, 124_500_000_000), "—");

        let eur = SolPrice {
            value: 100.0,
            currency: crate::types::Currency::Eur,
            fetched_at: crate::db::now_ms() as u64 / 1000,
            source: crate::price::PriceSource::CoinGecko,
        };
        assert_eq!(fmt_usd(Some(eur), LAMPORTS_PER_SOL), "€100.00");
    }

    #[test]
    fn zero_decimal_currencies_render_without_minor_units() {
        let now = crate::db::now_ms() as u64 / 1000;
        let jpy = SolPrice {
            value: 21000.0,
            currency: crate::types::Currency::Jpy,
            fetched_at: now,
            source: crate::price::PriceSource::CoinGecko,
        };
        assert_eq!(fmt_usd(Some(jpy), 124_500_000_000), "¥2,614,500");
        assert_eq!(fmt_price(Some(jpy)), "SOL ¥21,000");

        let cny = SolPrice {
            value: 7000.0,
            currency: crate::types::Currency::Cny,
            fetched_at: now,
            source: crate::price::PriceSource::CoinGecko,
        };
        assert_eq!(fmt_usd(Some(cny), 124_500_000_000), "CN¥871,500");
        assert_eq!(fmt_price(Some(cny)), "SOL CN¥7,000");
    }

    #[test]
    fn fmt_price_two_decimal_currencies_keep_minor_units() {
        let now = crate::db::now_ms() as u64 / 1000;
        let usd = SolPrice {
            value: 21000.0,
            currency: crate::types::Currency::Usd,
            fetched_at: now,
            source: crate::price::PriceSource::CoinGecko,
        };
        assert_eq!(fmt_price(Some(usd)), "SOL $21,000.00");
    }

    #[test]
    fn elision() {
        assert_eq!(
            elide_addr("HAgk14JpMQLgt6rVgv7cBQFJWFto5Dqxi472uT3DKpqk"),
            "HAgk…Kpqk"
        );
        assert_eq!(elide_addr("short"), "short");
    }

    #[test]
    fn input_tail_keeps_caret_visible() {
        assert_eq!(input_tail("hello", 10), "hello");
        assert_eq!(input_tail("hello", 5), "hello");
        assert_eq!(input_tail("abcdefgh", 4), "…fgh");
        assert!(input_tail("a-very-long-rpc-url.example.com", 12).ends_with(".com"));
    }

    #[test]
    fn middle_and_end_elision() {
        assert_eq!(elide_middle("short", 10), "short");
        let m = elide_middle("https://api.mainnet-beta.solana.com", 20);
        assert_eq!(m.chars().count(), 20);
        assert!(m.contains('…'));
        assert!(m.starts_with("https"));
        assert!(m.ends_with(".com"));

        assert_eq!(truncate_end("Wallet 1", 20), "Wallet 1");
        assert_eq!(truncate_end("A very long profile name", 10), "A very lo…");
        assert_eq!(
            truncate_end("A very long profile name", 10).chars().count(),
            10
        );
    }

    #[test]
    fn wrap_lines_basics() {
        assert_eq!(wrap_lines("", 10), vec![""]);
        assert_eq!(wrap_lines("hello world", 20), vec!["hello world"]);
        let lines = wrap_lines("the quick brown fox jumps", 10);
        assert!(lines.iter().all(|l| l.chars().count() <= 10));
        assert_eq!(lines, vec!["the quick", "brown fox", "jumps"]);
        assert_eq!(wrap_lines("a\n\nb", 10), vec!["a", "", "b"]);
        let hard = wrap_lines("abcdefghij", 4);
        assert_eq!(hard, vec!["abcd", "efgh", "ij"]);
        assert!(hard.iter().all(|l| l.chars().count() <= 4));
    }
}
