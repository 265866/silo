use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::sync::RwLockExt;
use crate::types::Currency;

const SOL_MINT: &str = "So11111111111111111111111111111111111111112";
pub const STALE_AFTER_SECS: u64 = 5 * 60;

const MIN_SOL_PRICE: f64 = 0.01;
const MAX_SOL_PRICE: f64 = 100_000.0;
const MIN_FX_RATE: f64 = 0.0001;
const MAX_FX_RATE: f64 = 10_000.0;

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

#[derive(Debug, thiserror::Error)]
pub enum PriceMetaError {
    #[error("price cache JSON was not valid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("price cache field is missing or invalid: {0}")]
    Field(&'static str),
    #[error("unknown price source: {0}")]
    Source(String),
}

impl SolPrice {
    pub fn age_secs(&self) -> u64 {
        now_secs().saturating_sub(self.fetched_at)
    }
    pub fn is_stale(&self) -> bool {
        self.age_secs() > STALE_AFTER_SECS
    }

    pub fn to_meta_json(self) -> String {
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

    pub fn from_meta_json(s: &str) -> Result<SolPrice, PriceMetaError> {
        let v: serde_json::Value = serde_json::from_str(s)?;
        let value = v
            .get("value")
            .and_then(serde_json::Value::as_f64)
            .filter(|v| v.is_finite() && *v > 0.0)
            .ok_or(PriceMetaError::Field("value"))?;
        let currency = v
            .get("currency")
            .and_then(serde_json::Value::as_str)
            .and_then(Currency::from_code)
            .ok_or(PriceMetaError::Field("currency"))?;
        let fetched_at = v
            .get("fetched_at")
            .and_then(serde_json::Value::as_u64)
            .ok_or(PriceMetaError::Field("fetched_at"))?;
        let source_raw = v
            .get("source")
            .and_then(serde_json::Value::as_str)
            .ok_or(PriceMetaError::Field("source"))?;
        let source = match source_raw {
            "coingecko" => PriceSource::CoinGecko,
            "jupiter" => PriceSource::Jupiter,
            other => return Err(PriceMetaError::Source(other.to_string())),
        };
        Ok(SolPrice {
            value,
            currency,
            fetched_at,
            source,
        })
    }
}

pub const COINGECKO_BACKOFF_SECS: u64 = 300;

#[derive(Clone)]
struct PriceEndpoints {
    coingecko: String,
    jupiter: String,
    fx: String,
}

impl PriceEndpoints {
    fn live() -> Self {
        PriceEndpoints {
            coingecko: "https://api.coingecko.com/api/v3/simple/price".to_string(),
            jupiter: "https://lite-api.jup.ag/price/v3".to_string(),
            fx: "https://api.frankfurter.dev/v1/latest".to_string(),
        }
    }

    fn coingecko_url(&self, currency: Currency) -> String {
        format!(
            "{}?ids=solana&vs_currencies={}",
            self.coingecko,
            currency.code()
        )
    }

    fn jupiter_url(&self) -> String {
        format!("{}?ids={SOL_MINT}", self.jupiter)
    }

    fn fx_url(&self, currency: Currency) -> String {
        format!(
            "{}?base=USD&symbols={}",
            self.fx,
            currency.code().to_ascii_uppercase()
        )
    }
}

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
    if value.is_finite() && (MIN_SOL_PRICE..=MAX_SOL_PRICE).contains(&value) {
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
    let endpoints = PriceEndpoints::live();
    fetch_price_with_endpoints(client, currency, &endpoints).await
}

async fn fetch_price_with_endpoints(
    client: &reqwest::Client,
    currency: Currency,
    endpoints: &PriceEndpoints,
) -> Result<SolPrice, PriceError> {
    let primary = match fetch_coingecko(client, currency, endpoints).await {
        Ok(p) => return Ok(p),
        Err(e) => e,
    };
    fetch_price_fallback_only_with_endpoints(client, currency, endpoints)
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
    let endpoints = PriceEndpoints::live();
    fetch_price_fallback_only_with_endpoints(client, currency, &endpoints).await
}

async fn fetch_price_fallback_only_with_endpoints(
    client: &reqwest::Client,
    currency: Currency,
    endpoints: &PriceEndpoints,
) -> Result<SolPrice, PriceError> {
    let usd = fetch_jupiter(client, endpoints).await?;
    if currency == Currency::Usd {
        return Ok(usd);
    }
    let rate = fetch_fx_rate(client, currency, endpoints).await?;
    validate(usd.value * rate, currency, PriceSource::Jupiter)
}

pub async fn fetch_price_backoff_aware(
    client: &reqwest::Client,
    currency: Currency,
    skip_coingecko: bool,
) -> (Result<SolPrice, PriceError>, bool) {
    let endpoints = PriceEndpoints::live();
    fetch_price_backoff_aware_with_endpoints(client, currency, skip_coingecko, &endpoints).await
}

async fn fetch_price_backoff_aware_with_endpoints(
    client: &reqwest::Client,
    currency: Currency,
    skip_coingecko: bool,
    endpoints: &PriceEndpoints,
) -> (Result<SolPrice, PriceError>, bool) {
    if !skip_coingecko {
        match fetch_coingecko(client, currency, endpoints).await {
            Ok(p) => return (Ok(p), false),
            Err(PriceError::RateLimited) => {
                return (
                    fetch_price_fallback_only_with_endpoints(client, currency, endpoints).await,
                    true,
                );
            }
            Err(_) => {}
        }
    }
    (
        fetch_price_fallback_only_with_endpoints(client, currency, endpoints).await,
        false,
    )
}

async fn fetch_fx_rate(
    client: &reqwest::Client,
    currency: Currency,
    endpoints: &PriceEndpoints,
) -> Result<f64, PriceError> {
    let code = currency.code().to_ascii_uppercase();
    let resp = client
        .get(endpoints.fx_url(currency))
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
    if rate.is_finite() && (MIN_FX_RATE..=MAX_FX_RATE).contains(&rate) {
        Ok(rate)
    } else {
        Err(PriceError::BadValue)
    }
}

async fn fetch_coingecko(
    client: &reqwest::Client,
    currency: Currency,
    endpoints: &PriceEndpoints,
) -> Result<SolPrice, PriceError> {
    let resp = client
        .get(endpoints.coingecko_url(currency))
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

async fn fetch_jupiter(
    client: &reqwest::Client,
    endpoints: &PriceEndpoints,
) -> Result<SolPrice, PriceError> {
    #[derive(serde::Deserialize)]
    struct Entry {
        #[serde(rename = "usdPrice")]
        usd_price: f64,
    }
    let resp = client
        .get(endpoints.jupiter_url())
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
        *self.inner.read_recover()
    }
    pub fn set(&self, p: SolPrice) {
        *self.inner.write_recover() = Some(p);
    }
    pub fn clear(&self) {
        *self.inner.write_recover() = None;
    }
    pub fn seed(&self, p: SolPrice) {
        let mut g = self.inner.write_recover();
        if g.is_none() {
            *g = Some(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io::{Read as _, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};

    struct MockResponse {
        status: u16,
        headers: Vec<(&'static str, &'static str)>,
        body: String,
    }

    impl MockResponse {
        fn new(status: u16, body: impl Into<String>) -> Self {
            MockResponse {
                status,
                headers: Vec::new(),
                body: body.into(),
            }
        }
    }

    struct MockServer {
        url: String,
        requests: Arc<Mutex<Vec<String>>>,
        _worker: std::thread::JoinHandle<()>,
    }

    impl MockServer {
        fn new(responses: Vec<MockResponse>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let requests = Arc::new(Mutex::new(Vec::new()));
            let requests_for_thread = requests.clone();
            let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
            let responses_for_thread = responses.clone();
            let worker = std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let mut stream = stream.unwrap();
                    let request = read_request(&mut stream);
                    requests_for_thread.lock().unwrap().push(request);
                    let response = responses_for_thread
                        .lock()
                        .unwrap()
                        .pop_front()
                        .unwrap_or_else(|| MockResponse::new(500, "unexpected request"));
                    let done = responses_for_thread.lock().unwrap().is_empty();
                    write_response(&mut stream, response);
                    if done {
                        break;
                    }
                }
            });
            MockServer {
                url,
                requests,
                _worker: worker,
            }
        }

        fn endpoints(&self) -> PriceEndpoints {
            PriceEndpoints {
                coingecko: format!("{}/coingecko", self.url),
                jupiter: format!("{}/jupiter", self.url),
                fx: format!("{}/fx", self.url),
            }
        }

        fn requests(&self) -> Vec<String> {
            self.requests.lock().unwrap().clone()
        }
    }

    fn read_request(stream: &mut TcpStream) -> String {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        loop {
            let n = stream.read(&mut tmp).unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&buf[..end]);
                let len = content_length(&headers);
                if buf.len().saturating_sub(end + 4) >= len {
                    break;
                }
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    fn content_length(headers: &str) -> usize {
        headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0)
    }

    fn write_response(stream: &mut TcpStream, response: MockResponse) {
        let reason = match response.status {
            200 => "OK",
            408 => "Request Timeout",
            429 => "Too Many Requests",
            500 => "Internal Server Error",
            _ => "OK",
        };
        let mut head = format!(
            "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
            response.status,
            reason,
            response.body.len()
        );
        for (name, value) in response.headers {
            head.push_str(name);
            head.push_str(": ");
            head.push_str(value);
            head.push_str("\r\n");
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(response.body.as_bytes()).unwrap();
    }

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
    fn meta_json_rejects_unknown_source() {
        let err = SolPrice::from_meta_json(
            r#"{"value":146.31,"currency":"usd","fetched_at":1700000000,"source":"mystery"}"#,
        )
        .unwrap_err();
        assert!(matches!(err, PriceMetaError::Source(_)));
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
    fn validate_rejects_out_of_band_prices() {
        let c = Currency::Usd;
        assert!(matches!(
            validate(0.0001, c, PriceSource::CoinGecko),
            Err(PriceError::BadValue)
        ));
        assert!(matches!(
            validate(1_000_000.0, c, PriceSource::Jupiter),
            Err(PriceError::BadValue)
        ));
        assert!(validate(150.0, c, PriceSource::CoinGecko).is_ok());
        assert!(validate(MIN_SOL_PRICE, c, PriceSource::CoinGecko).is_ok());
        assert!(validate(MAX_SOL_PRICE, c, PriceSource::CoinGecko).is_ok());
    }

    #[tokio::test]
    async fn fx_rate_out_of_band_is_rejected() {
        let server = MockServer::new(vec![MockResponse::new(
            200,
            r#"{"amount":1.0,"base":"USD","date":"2024-05-01","rates":{"EUR":100000.0}}"#,
        )]);
        let endpoints = server.endpoints();
        let client = reqwest::Client::new();
        let err = fetch_fx_rate(&client, Currency::Eur, &endpoints)
            .await
            .unwrap_err();
        assert!(matches!(err, PriceError::BadValue));
    }

    #[tokio::test]
    async fn fx_rate_realistic_is_accepted() {
        let server = MockServer::new(vec![MockResponse::new(
            200,
            r#"{"amount":1.0,"base":"USD","date":"2024-05-01","rates":{"EUR":0.92}}"#,
        )]);
        let endpoints = server.endpoints();
        let client = reqwest::Client::new();
        let rate = fetch_fx_rate(&client, Currency::Eur, &endpoints)
            .await
            .unwrap();
        assert_eq!(rate, 0.92);
    }

    #[tokio::test]
    async fn fallback_rejects_out_of_band_product() {
        let jupiter = format!(r#"{{"{SOL_MINT}":{{"usdPrice":150.0}}}}"#);
        let server = MockServer::new(vec![
            MockResponse::new(200, jupiter),
            MockResponse::new(
                200,
                r#"{"amount":1.0,"base":"USD","date":"2024-05-01","rates":{"EUR":5000.0}}"#,
            ),
        ]);
        let endpoints = server.endpoints();
        let client = reqwest::Client::new();
        let err = fetch_price_fallback_only_with_endpoints(&client, Currency::Eur, &endpoints)
            .await
            .unwrap_err();
        assert!(matches!(err, PriceError::BadValue));
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

    #[test]
    fn cache_recovers_from_poison() {
        let c = Arc::new(PriceCache::new());
        let c2 = c.clone();
        let _ = std::thread::spawn(move || {
            let mut g = c2.inner.write().unwrap();
            *g = Some(SolPrice {
                value: 10.0,
                currency: Currency::Usd,
                fetched_at: now_secs(),
                source: PriceSource::CoinGecko,
            });
            panic!("poison it");
        })
        .join();
        assert_eq!(c.get().unwrap().value, 10.0);
        c.clear();
        assert!(c.get().is_none());
    }

    #[tokio::test]
    async fn coingecko_429_falls_back_to_jupiter() {
        let body = format!(r#"{{"{SOL_MINT}":{{"usdPrice":146.31}}}}"#);
        let server = MockServer::new(vec![
            MockResponse::new(429, ""),
            MockResponse::new(200, body),
        ]);
        let endpoints = server.endpoints();
        let client = reqwest::Client::new();
        let (result, backed_off) =
            fetch_price_backoff_aware_with_endpoints(&client, Currency::Usd, false, &endpoints)
                .await;
        let p = result.unwrap();
        assert!(backed_off);
        assert_eq!(p.source, PriceSource::Jupiter);
        assert_eq!(p.value, 146.31);
        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].contains("/coingecko?ids=solana&vs_currencies=usd"));
        assert!(requests[1].contains("/jupiter?ids="));
    }

    #[tokio::test]
    async fn malformed_provider_payloads_return_parse_errors() {
        let server = MockServer::new(vec![
            MockResponse::new(200, r#"{"solana":{}}"#),
            MockResponse::new(200, r#"{}"#),
        ]);
        let endpoints = server.endpoints();
        let client = reqwest::Client::new();
        let err = fetch_price_with_endpoints(&client, Currency::Usd, &endpoints)
            .await
            .unwrap_err();
        match err {
            PriceError::Fallback { primary, fallback } => {
                assert!(matches!(*primary, PriceError::Parse));
                assert!(matches!(*fallback, PriceError::Parse));
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[tokio::test]
    #[ignore = "hits live price APIs"]
    async fn live_fetch_price() {
        let client = reqwest::Client::new();
        let p = fetch_price(&client, Currency::Eur).await.unwrap();
        assert!(p.value > 0.0);
    }
}
