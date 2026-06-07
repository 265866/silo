use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::types::Currency;

const SOL_MINT: &str = "So11111111111111111111111111111111111111112";
pub const STALE_AFTER_SECS: u64 = 5 * 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PriceSource {
    CoinGecko,
    Jupiter,
}
impl PriceSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            PriceSource::CoinGecko => "CoinGecko",
            PriceSource::Jupiter => "Jupiter",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SolPrice {
    pub value: f64,
    pub currency: Currency,
    pub fetched_at: u64,
    pub source: PriceSource,
}

impl SolPrice {
    pub fn age_secs(&self) -> u64 {
        now_secs().saturating_sub(self.fetched_at)
    }
    pub fn is_stale(&self) -> bool {
        self.age_secs() > STALE_AFTER_SECS
    }

    pub fn to_meta_json(&self) -> String {
        serde_json::json!({
            "value": self.value,
            "currency": self.currency.code(),
            "fetched_at": self.fetched_at,
            "source": match self.source {
                PriceSource::CoinGecko => "coingecko",
                PriceSource::Jupiter => "jupiter",
            },
        })
        .to_string()
    }

    pub fn from_meta_json(s: &str) -> Option<SolPrice> {
        let v: serde_json::Value = serde_json::from_str(s).ok()?;
        Some(SolPrice {
            value: v.get("value")?.as_f64()?,
            currency: Currency::from_code(v.get("currency")?.as_str()?)?,
            fetched_at: v.get("fetched_at")?.as_u64()?,
            source: match v.get("source")?.as_str()? {
                "jupiter" => PriceSource::Jupiter,
                _ => PriceSource::CoinGecko,
            },
        })
    }
}

pub const COINGECKO_BACKOFF_SECS: u64 = 300;

#[derive(Debug, thiserror::Error)]
pub enum PriceError {
    #[error("http error: {0}")]
    Http(String),
    #[error("rate limited")]
    RateLimited,
    #[error("price payload was not valid")]
    Parse,
    #[error("price value out of range")]
    BadValue,
    #[error("primary price provider failed ({primary}); fallback failed ({fallback})")]
    Fallback {
        primary: Box<PriceError>,
        fallback: Box<PriceError>,
    },
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn validate(value: f64, currency: Currency, source: PriceSource) -> Result<SolPrice, PriceError> {
    if value.is_finite() && value > 0.0 {
        Ok(SolPrice {
            value,
            currency,
            fetched_at: now_secs(),
            source,
        })
    } else {
        Err(PriceError::BadValue)
    }
}

pub async fn fetch_price(
    client: &reqwest::Client,
    currency: Currency,
) -> Result<SolPrice, PriceError> {
    let primary = match fetch_coingecko(client, currency).await {
        Ok(p) => return Ok(p),
        Err(e) => e,
    };
    fetch_price_fallback_only(client, currency)
        .await
        .map_err(|fallback| PriceError::Fallback {
            primary: Box::new(primary),
            fallback: Box::new(fallback),
        })
}

pub async fn fetch_price_fallback_only(
    client: &reqwest::Client,
    currency: Currency,
) -> Result<SolPrice, PriceError> {
    let usd = fetch_jupiter(client).await?;
    if currency == Currency::Usd {
        return Ok(usd);
    }
    let rate = fetch_fx_rate(client, currency).await?;
    validate(usd.value * rate, currency, PriceSource::Jupiter)
}

pub async fn fetch_price_backoff_aware(
    client: &reqwest::Client,
    currency: Currency,
    skip_coingecko: bool,
) -> (Result<SolPrice, PriceError>, bool) {
    if !skip_coingecko {
        match fetch_coingecko(client, currency).await {
            Ok(p) => return (Ok(p), false),
            Err(PriceError::RateLimited) => {
                return (fetch_price_fallback_only(client, currency).await, true);
            }
            Err(_) => {}
        }
    }
    (fetch_price_fallback_only(client, currency).await, false)
}

async fn fetch_fx_rate(client: &reqwest::Client, currency: Currency) -> Result<f64, PriceError> {
    let code = currency.code().to_ascii_uppercase();
    let url = format!("https://api.frankfurter.dev/v1/latest?base=USD&symbols={code}");
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| PriceError::Http(e.to_string()))?
        .error_for_status()
        .map_err(|e| PriceError::Http(e.to_string()))?;
    #[derive(serde::Deserialize)]
    struct FxResp {
        rates: HashMap<String, f64>,
    }
    let body: FxResp = resp.json().await.map_err(|_| PriceError::Parse)?;
    let rate = body.rates.get(&code).copied().ok_or(PriceError::Parse)?;
    if rate.is_finite() && rate > 0.0 {
        Ok(rate)
    } else {
        Err(PriceError::BadValue)
    }
}

async fn fetch_coingecko(
    client: &reqwest::Client,
    currency: Currency,
) -> Result<SolPrice, PriceError> {
    let url = format!(
        "https://api.coingecko.com/api/v3/simple/price?ids=solana&vs_currencies={}",
        currency.code()
    );
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| PriceError::Http(e.to_string()))?;
    if resp.status().as_u16() == 429 {
        return Err(PriceError::RateLimited);
    }
    let resp = resp
        .error_for_status()
        .map_err(|e| PriceError::Http(e.to_string()))?;
    let map: HashMap<String, HashMap<String, f64>> =
        resp.json().await.map_err(|_| PriceError::Parse)?;
    let value = map
        .get("solana")
        .and_then(|m| m.get(currency.code()))
        .copied()
        .ok_or(PriceError::Parse)?;
    validate(value, currency, PriceSource::CoinGecko)
}

async fn fetch_jupiter(client: &reqwest::Client) -> Result<SolPrice, PriceError> {
    #[derive(serde::Deserialize)]
    struct Entry {
        #[serde(rename = "usdPrice")]
        usd_price: f64,
    }
    let url = format!("https://lite-api.jup.ag/price/v3?ids={SOL_MINT}");
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| PriceError::Http(e.to_string()))?
        .error_for_status()
        .map_err(|e| PriceError::Http(e.to_string()))?;
    let map: HashMap<String, Entry> = resp.json().await.map_err(|_| PriceError::Parse)?;
    let entry = map.get(SOL_MINT).ok_or(PriceError::Parse)?;
    validate(entry.usd_price, Currency::Usd, PriceSource::Jupiter)
}

#[derive(Default)]
pub struct PriceCache {
    inner: RwLock<Option<SolPrice>>,
}
impl PriceCache {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn get(&self) -> Option<SolPrice> {
        *self.inner.read().unwrap()
    }
    pub fn set(&self, p: SolPrice) {
        *self.inner.write().unwrap() = Some(p);
    }
    pub fn clear(&self) {
        *self.inner.write().unwrap() = None;
    }
    pub fn seed(&self, p: SolPrice) {
        let mut g = self.inner.write().unwrap();
        if g.is_none() {
            *g = Some(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_coingecko_shape() {
        let raw = r#"{"solana":{"eur":132.4}}"#;
        let map: HashMap<String, HashMap<String, f64>> = serde_json::from_str(raw).unwrap();
        assert_eq!(*map.get("solana").unwrap().get("eur").unwrap(), 132.4);
    }

    #[test]
    fn decode_jupiter_shape_tolerates_extra_fields() {
        let raw = format!(
            r#"{{"{SOL_MINT}":{{"usdPrice":146.31,"blockId":1,"decimals":9,"priceChange24h":-1.2}}}}"#
        );
        #[derive(serde::Deserialize)]
        struct Entry {
            #[serde(rename = "usdPrice")]
            usd_price: f64,
        }
        let map: HashMap<String, Entry> = serde_json::from_str(&raw).unwrap();
        assert_eq!(map.get(SOL_MINT).unwrap().usd_price, 146.31);
    }

    #[test]
    fn decode_frankfurter_shape() {
        let raw = r#"{"amount":1.0,"base":"USD","date":"2024-05-01","rates":{"EUR":0.859}}"#;
        #[derive(serde::Deserialize)]
        struct FxResp {
            rates: HashMap<String, f64>,
        }
        let body: FxResp = serde_json::from_str(raw).unwrap();
        assert_eq!(*body.rates.get("EUR").unwrap(), 0.859);
    }

    #[test]
    fn meta_json_roundtrips() {
        let p = SolPrice {
            value: 146.31,
            currency: Currency::Gbp,
            fetched_at: 1_700_000_000,
            source: PriceSource::Jupiter,
        };
        let back = SolPrice::from_meta_json(&p.to_meta_json()).unwrap();
        assert_eq!(back.value, p.value);
        assert_eq!(back.currency, p.currency);
        assert_eq!(back.fetched_at, p.fetched_at);
        assert_eq!(back.source, p.source);
    }

    #[test]
    fn validate_rejects_bad_values() {
        let c = Currency::Usd;
        assert!(validate(f64::NAN, c, PriceSource::CoinGecko).is_err());
        assert!(validate(0.0, c, PriceSource::CoinGecko).is_err());
        assert!(validate(-5.0, c, PriceSource::CoinGecko).is_err());
        assert!(validate(f64::INFINITY, c, PriceSource::CoinGecko).is_err());
        assert!(validate(146.2, c, PriceSource::CoinGecko).is_ok());
    }

    #[test]
    fn fallback_error_reports_both_providers() {
        let e = PriceError::Fallback {
            primary: Box::new(PriceError::RateLimited),
            fallback: Box::new(PriceError::Http("offline".into())),
        };
        let text = e.to_string();
        assert!(text.contains("rate limited"));
        assert!(text.contains("offline"));
    }

    #[test]
    fn cache_roundtrip_and_staleness() {
        let c = PriceCache::new();
        assert!(c.get().is_none());
        c.set(SolPrice {
            value: 100.0,
            currency: Currency::Usd,
            fetched_at: now_secs(),
            source: PriceSource::CoinGecko,
        });
        let p = c.get().unwrap();
        assert_eq!(p.value, 100.0);
        assert!(!p.is_stale());

        let old = SolPrice {
            value: 50.0,
            currency: Currency::Eur,
            fetched_at: now_secs().saturating_sub(STALE_AFTER_SECS + 10),
            source: PriceSource::Jupiter,
        };
        assert!(old.is_stale());
    }

    #[tokio::test]
    #[ignore = "hits live price APIs"]
    async fn live_fetch_price() {
        let client = reqwest::Client::new();
        let p = fetch_price(&client, Currency::Eur).await.unwrap();
        assert!(p.value > 0.0);
    }
}
