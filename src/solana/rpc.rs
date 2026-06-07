use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;

use super::tx::base64_encode;
use crate::types::Commitment;

const MAX_RETRIES: u32 = 4;
const GMA_CHUNK: usize = 100;
const SIGSTATUS_CHUNK: usize = 256;
const RETRY_AFTER_CAP_SECS: u64 = 60;

#[derive(Clone, Debug, Deserialize)]
pub struct SignatureStatus {
    pub slot: u64,
    #[serde(default)]
    pub confirmations: Option<u64>,
    #[serde(default)]
    pub err: Option<serde_json::Value>,
    #[serde(rename = "confirmationStatus", default)]
    pub confirmation_status: Option<String>,
}

impl SignatureStatus {
    pub fn is_error(&self) -> bool {
        self.err.is_some()
    }
    pub fn is_confirmed(&self) -> bool {
        matches!(self.confirmation_status.as_deref(), Some("confirmed"))
    }
    pub fn is_finalized(&self) -> bool {
        matches!(self.confirmation_status.as_deref(), Some("finalized"))
    }
    pub fn is_confirmed_or_finalized(&self) -> bool {
        self.is_confirmed() || self.is_finalized()
    }
}

#[derive(Clone)]
pub struct Rpc {
    client: reqwest::Client,
    url: String,
    commitment: &'static str,
}

#[derive(Debug, thiserror::Error)]
pub enum RpcUrlError {
    #[error("enter an RPC URL")]
    Empty,
    #[error("URL contains control characters")]
    Control,
    #[error("URL is not valid")]
    Invalid,
    #[error("URL must use http or https")]
    Scheme,
    #[error("URL must include a host")]
    Host,
    #[error("URL must not include username or password")]
    UserInfo,
}

pub fn validate_rpc_url(raw: &str) -> Result<String, RpcUrlError> {
    if raw.chars().any(char::is_control) {
        return Err(RpcUrlError::Control);
    }
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(RpcUrlError::Empty);
    }
    let url = reqwest::Url::parse(trimmed).map_err(|_| RpcUrlError::Invalid)?;
    match url.scheme() {
        "http" | "https" => {}
        _ => return Err(RpcUrlError::Scheme),
    }
    if url.host_str().is_none() {
        return Err(RpcUrlError::Host);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(RpcUrlError::UserInfo);
    }
    Ok(trimmed.to_string())
}

pub fn redact_rpc_url(raw: &str) -> String {
    match reqwest::Url::parse(raw) {
        Ok(url) => {
            let mut out = format!("{}://{}", url.scheme(), url.host_str().unwrap_or(""));
            if let Some(port) = url.port() {
                out.push_str(&format!(":{port}"));
            }
            if url.path() != "/" && !url.path().is_empty() {
                out.push_str("/…");
            }
            if url.query().is_some() {
                out.push_str("?…");
            }
            out
        }
        Err(_) => "<invalid rpc url>".to_string(),
    }
}

#[derive(Deserialize)]
struct RpcEnvelope<T> {
    result: Option<T>,
    error: Option<RpcErr>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
}

type RpcErr = JsonRpcError;

#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("RPC {method} request failed: {source}")]
    Transport {
        method: &'static str,
        #[source]
        source: reqwest::Error,
    },
    #[error("RPC {method} failed after retries: HTTP {status}")]
    RetryExhaustedHttp {
        method: &'static str,
        status: reqwest::StatusCode,
    },
    #[error("RPC {method} HTTP {status}")]
    NonRetryHttp {
        method: &'static str,
        status: reqwest::StatusCode,
        body: String,
    },
    #[error("RPC {method} error {code}: {message}")]
    JsonRpc {
        method: &'static str,
        code: i64,
        message: String,
    },
    #[error("RPC {method}: empty result")]
    MissingResult { method: &'static str },
    #[error("RPC {method} returned {actual} result(s) for {expected} request(s)")]
    LengthMismatch {
        method: &'static str,
        expected: usize,
        actual: usize,
    },
    #[error("RPC {method} response decode failed: {source}")]
    Decode {
        method: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("RPC {method} response body read failed: {source}")]
    Body {
        method: &'static str,
        #[source]
        source: reqwest::Error,
    },
}

#[derive(Deserialize)]
struct Ctx<T> {
    value: T,
}

#[derive(Deserialize)]
struct AccountInfo {
    lamports: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BlockhashValue {
    blockhash: String,
    last_valid_block_height: u64,
}

impl Rpc {
    pub fn new(client: reqwest::Client, url: impl Into<String>) -> Self {
        Rpc {
            client,
            url: url.into(),
            commitment: Commitment::Confirmed.as_str(),
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    async fn call<T: DeserializeOwned>(
        &self,
        method: &'static str,
        params: serde_json::Value,
    ) -> std::result::Result<T, RpcError> {
        let body = json!({"jsonrpc":"2.0","id":1,"method":method,"params":params});
        let mut attempt = 0u32;
        loop {
            match self.client.post(&self.url).json(&body).send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.as_u16() == 429 || status.as_u16() == 408 || status.is_server_error()
                    {
                        if attempt >= MAX_RETRIES {
                            return Err(RpcError::RetryExhaustedHttp { method, status });
                        }
                        let wait = retry_after(&resp).unwrap_or_else(|| backoff(attempt));
                        attempt += 1;
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    if !status.is_success() {
                        let body = resp
                            .text()
                            .await
                            .map_err(|source| RpcError::Body { method, source })?;
                        return Err(RpcError::NonRetryHttp {
                            method,
                            status,
                            body,
                        });
                    }
                    let body = resp
                        .text()
                        .await
                        .map_err(|source| RpcError::Body { method, source })?;
                    let env: RpcEnvelope<T> = serde_json::from_str(&body)
                        .map_err(|source| RpcError::Decode { method, source })?;
                    if let Some(e) = env.error {
                        return Err(RpcError::JsonRpc {
                            method,
                            code: e.code,
                            message: e.message,
                        });
                    }
                    return env.result.ok_or(RpcError::MissingResult { method });
                }
                Err(e) => {
                    if attempt >= MAX_RETRIES {
                        return Err(RpcError::Transport { method, source: e });
                    }
                    let wait = backoff(attempt);
                    attempt += 1;
                    tokio::time::sleep(wait).await;
                }
            }
        }
    }

    pub async fn get_balances(&self, pubkeys: &[&str]) -> std::result::Result<Vec<u64>, RpcError> {
        if pubkeys.is_empty() {
            return Ok(vec![]);
        }

        let mut out = Vec::with_capacity(pubkeys.len());
        for chunk in pubkeys.chunks(GMA_CHUNK) {
            let params = json!([
                chunk,
                {"commitment": self.commitment, "encoding": "base64",
                 "dataSlice": {"offset": 0, "length": 0}}
            ]);
            let ctx: Ctx<Vec<Option<AccountInfo>>> =
                self.call("getMultipleAccounts", params).await?;
            let actual = ctx.value.len();
            if actual != chunk.len() {
                return Err(RpcError::LengthMismatch {
                    method: "getMultipleAccounts",
                    expected: chunk.len(),
                    actual,
                });
            }
            out.extend(
                ctx.value
                    .into_iter()
                    .map(|o| o.map(|a| a.lamports).unwrap_or(0)),
            );
        }
        Ok(out)
    }

    pub async fn get_balance(&self, pubkey: &str) -> std::result::Result<u64, RpcError> {
        let v = self.get_balances(&[pubkey]).await?;
        Ok(v.first().copied().unwrap_or(0))
    }

    pub async fn get_latest_blockhash(&self) -> std::result::Result<(String, u64), RpcError> {
        let params = json!([{"commitment": self.commitment}]);
        let ctx: Ctx<BlockhashValue> = self.call("getLatestBlockhash", params).await?;
        Ok((ctx.value.blockhash, ctx.value.last_valid_block_height))
    }

    pub async fn send_transaction(&self, wire: &[u8]) -> std::result::Result<String, RpcError> {
        let params = json!([
            base64_encode(wire),
            {"encoding": "base64", "skipPreflight": false,
             "preflightCommitment": self.commitment, "maxRetries": 0}
        ]);
        self.call("sendTransaction", params).await
    }

    pub async fn get_signature_statuses(
        &self,
        signatures: &[&str],
        search_history: bool,
    ) -> std::result::Result<Vec<Option<SignatureStatus>>, RpcError> {
        if signatures.is_empty() {
            return Ok(vec![]);
        }

        let mut out = Vec::with_capacity(signatures.len());
        for chunk in signatures.chunks(SIGSTATUS_CHUNK) {
            let params = json!([chunk, {"searchTransactionHistory": search_history}]);
            let ctx: Ctx<Vec<Option<SignatureStatus>>> =
                self.call("getSignatureStatuses", params).await?;
            let actual = ctx.value.len();
            if actual != chunk.len() {
                return Err(RpcError::LengthMismatch {
                    method: "getSignatureStatuses",
                    expected: chunk.len(),
                    actual,
                });
            }
            out.extend(ctx.value);
        }
        Ok(out)
    }

    pub async fn get_block_height(&self) -> std::result::Result<u64, RpcError> {
        self.call("getBlockHeight", json!([{"commitment": self.commitment}]))
            .await
    }

    pub async fn get_min_balance_for_rent_exemption(
        &self,
        data_len: usize,
    ) -> std::result::Result<u64, RpcError> {
        self.call("getMinimumBalanceForRentExemption", json!([data_len]))
            .await
    }
}

fn retry_after(r: &reqwest::Response) -> Option<Duration> {
    let raw = r
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    retry_after_delay(raw, now)
}

fn retry_after_delay(raw: &str, now_unix: u64) -> Option<Duration> {
    let raw = raw.trim();
    if let Ok(secs) = raw.parse::<u64>() {
        return Some(Duration::from_secs(secs.min(RETRY_AFTER_CAP_SECS)));
    }
    let target = parse_http_date(raw)?;
    let delay = target.saturating_sub(now_unix);
    Some(Duration::from_secs(delay.min(RETRY_AFTER_CAP_SECS)))
}

fn parse_http_date(s: &str) -> Option<u64> {
    let (_weekday, rest) = s.trim().split_once(", ")?;
    let mut fields = rest.split(' ');
    let day: u32 = fields.next()?.parse().ok()?;
    let month = match fields.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i64 = fields.next()?.parse().ok()?;
    let mut time = fields.next()?.split(':');
    let hour: u64 = time.next()?.parse().ok()?;
    let minute: u64 = time.next()?.parse().ok()?;
    let second: u64 = time.next()?.parse().ok()?;
    if time.next().is_some() {
        return None;
    }
    if fields.next()? != "GMT" || fields.next().is_some() {
        return None;
    }
    if !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    if days < 0 {
        return None;
    }
    Some(days as u64 * 86_400 + hour * 3_600 + minute * 60 + second)
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let month = month as i64;
    let doy = (153 * if month > 2 { month - 3 } else { month + 9 } + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn jitter_ms() -> u64 {
    let mut b = [0u8; 1];
    crate::crypto::random_bytes(&mut b);
    (b[0] as u64) % 100
}

fn backoff(attempt: u32) -> Duration {
    let base = 250u64;
    let exp = base.saturating_mul(1u64 << attempt.min(5));
    Duration::from_millis(exp.min(8_000) + jitter_ms())
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

        fn header(mut self, name: &'static str, value: &'static str) -> Self {
            self.headers.push((name, value));
            self
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

        fn url(&self) -> &str {
            &self.url
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
    fn rpc_url_validation_accepts_http_https() {
        assert_eq!(
            validate_rpc_url(" https://rpc.example.com/path?key=abc ").unwrap(),
            "https://rpc.example.com/path?key=abc"
        );
        assert!(validate_rpc_url("http://127.0.0.1:8899").is_ok());
    }

    #[test]
    fn rpc_url_validation_rejects_unsafe_urls() {
        assert!(matches!(validate_rpc_url(""), Err(RpcUrlError::Empty)));
        assert!(matches!(
            validate_rpc_url("ftp://rpc.example.com"),
            Err(RpcUrlError::Scheme)
        ));
        assert!(matches!(
            validate_rpc_url("https://user:pw@rpc.example.com"),
            Err(RpcUrlError::UserInfo)
        ));
        assert!(matches!(
            validate_rpc_url("https://rpc.example.com/\n"),
            Err(RpcUrlError::Control)
        ));
    }

    #[test]
    fn rpc_url_redaction_hides_path_and_query() {
        assert_eq!(
            redact_rpc_url("https://rpc.example.com/v2/secret?api_key=abc"),
            "https://rpc.example.com/…?…"
        );
        assert_eq!(
            redact_rpc_url("http://127.0.0.1:8899"),
            "http://127.0.0.1:8899"
        );
    }

    #[test]
    fn decode_get_multiple_accounts() {
        let raw = r#"{"context":{"slot":1},"value":[{"lamports":124500000000,"owner":"x","data":["","base64"],"executable":false,"rentEpoch":0},null]}"#;
        let ctx: Ctx<Vec<Option<AccountInfo>>> = serde_json::from_str(raw).unwrap();
        let balances: Vec<u64> = ctx
            .value
            .into_iter()
            .map(|o| o.map(|a| a.lamports).unwrap_or(0))
            .collect();
        assert_eq!(balances, vec![124_500_000_000, 0]);
    }

    #[test]
    fn decode_latest_blockhash() {
        let raw = r#"{"context":{"slot":50},"value":{"blockhash":"9xQ...","lastValidBlockHeight":12345}}"#;
        let ctx: Ctx<BlockhashValue> = serde_json::from_str(raw).unwrap();
        assert_eq!(ctx.value.blockhash, "9xQ...");
        assert_eq!(ctx.value.last_valid_block_height, 12345);
    }

    #[test]
    fn decode_signature_statuses_distinguishes_unknown_vs_processed() {
        let raw = r#"{"context":{"slot":99},"value":[
            null,
            {"slot":10,"confirmations":1,"err":null,"confirmationStatus":"processed"},
            {"slot":8,"confirmations":null,"err":null,"confirmationStatus":"confirmed"},
            {"slot":7,"confirmations":null,"err":{"InstructionError":[0,"Custom"]},"confirmationStatus":"confirmed"}
        ]}"#;
        let ctx: Ctx<Vec<Option<SignatureStatus>>> = serde_json::from_str(raw).unwrap();
        let v = ctx.value;
        assert!(v[0].is_none(), "null element = unknown / maybe never sent");
        let processed = v[1].as_ref().unwrap();
        assert!(
            !processed.is_confirmed_or_finalized(),
            "processed is below our commitment"
        );
        assert!(!processed.is_error());
        let confirmed = v[2].as_ref().unwrap();
        assert!(confirmed.is_confirmed_or_finalized());
        assert!(!confirmed.is_error());
        let failed = v[3].as_ref().unwrap();
        assert!(failed.is_error(), "err present = on-chain failure");
    }

    #[test]
    fn decode_rpc_error_envelope() {
        let raw =
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32002,"message":"blockhash not found"}}"#;
        let env: RpcEnvelope<String> = serde_json::from_str(raw).unwrap();
        assert!(env.result.is_none());
        assert_eq!(env.error.unwrap().code, -32002);
    }

    #[test]
    fn backoff_is_bounded_and_increasing() {
        let b0 = backoff(0).as_millis();
        let b3 = backoff(3).as_millis();
        assert!((250..350).contains(&b0));
        assert!(b3 > b0);
        assert!(backoff(20).as_millis() <= 8_100);
    }

    #[test]
    fn parse_http_date_matches_known_imf_fixdate() {
        assert_eq!(
            parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784_111_777)
        );
        assert_eq!(parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
    }

    #[test]
    fn parse_http_date_rejects_malformed() {
        assert_eq!(parse_http_date("not-a-date"), None);
        assert_eq!(parse_http_date("Sun, 06 Foo 1994 08:49:37 GMT"), None);
        assert_eq!(parse_http_date("Sun, 06 Nov 1994 08:49:37 PST"), None);
        assert_eq!(parse_http_date("Sun, 06 Nov 1994 25:49:37 GMT"), None);
        assert_eq!(parse_http_date("Sun, 06 Nov 1994 08:49 GMT"), None);
    }

    #[test]
    fn retry_after_delay_honors_integer_form() {
        assert_eq!(retry_after_delay("3", 1_000), Some(Duration::from_secs(3)));
        assert_eq!(retry_after_delay("0", 1_000), Some(Duration::ZERO));
        assert_eq!(
            retry_after_delay("9000", 1_000),
            Some(Duration::from_secs(60))
        );
    }

    #[test]
    fn retry_after_delay_honors_future_http_date() {
        let now = 784_111_777;
        let wait = retry_after_delay("Sun, 06 Nov 1994 08:49:47 GMT", now).unwrap();
        assert_eq!(wait, Duration::from_secs(10));
    }

    #[test]
    fn retry_after_delay_clamps_past_http_date_to_zero() {
        let now = 784_111_877;
        let wait = retry_after_delay("Sun, 06 Nov 1994 08:49:37 GMT", now).unwrap();
        assert_eq!(wait, Duration::ZERO);
    }

    #[test]
    fn retry_after_delay_caps_far_future_http_date() {
        let now = 784_111_777;
        let wait = retry_after_delay("Sun, 06 Nov 1994 09:49:37 GMT", now).unwrap();
        assert_eq!(wait, Duration::from_secs(60));
    }

    #[test]
    fn retry_after_delay_returns_none_for_garbage() {
        assert_eq!(retry_after_delay("not-a-date", 1_000), None);
    }

    #[tokio::test]
    async fn retries_transient_http_statuses() {
        let server = MockServer::new(vec![
            MockResponse::new(408, "").header("Retry-After", "0"),
            MockResponse::new(429, "").header("Retry-After", "0"),
            MockResponse::new(500, "").header("Retry-After", "0"),
            MockResponse::new(200, r#"{"jsonrpc":"2.0","id":1,"result":123}"#),
        ]);
        let rpc = Rpc::new(reqwest::Client::new(), server.url());
        assert_eq!(rpc.get_block_height().await.unwrap(), 123);
        let requests = server.requests();
        assert_eq!(requests.len(), 4);
        assert!(
            requests
                .iter()
                .all(|request| request.contains(r#""method":"getBlockHeight""#))
        );
    }

    #[tokio::test]
    async fn rpc_error_envelope_is_reported_through_call_path() {
        let server = MockServer::new(vec![MockResponse::new(
            200,
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32002,"message":"blockhash not found"}}"#,
        )]);
        let rpc = Rpc::new(reqwest::Client::new(), server.url());
        assert!(matches!(
            rpc.get_block_height().await.unwrap_err(),
            RpcError::JsonRpc {
                method: "getBlockHeight",
                code: -32002,
                message
            } if message == "blockhash not found"
        ));
    }

    #[tokio::test]
    async fn rpc_typed_http_errors_are_distinct() {
        let server = MockServer::new(vec![MockResponse::new(401, "nope")]);
        let rpc = Rpc::new(reqwest::Client::new(), server.url());
        assert!(matches!(
            rpc.get_block_height().await.unwrap_err(),
            RpcError::NonRetryHttp {
                method: "getBlockHeight",
                status,
                body
            } if status.as_u16() == 401 && body == "nope"
        ));

        let server = MockServer::new(vec![
            MockResponse::new(500, "").header("Retry-After", "0"),
            MockResponse::new(500, "").header("Retry-After", "0"),
            MockResponse::new(500, "").header("Retry-After", "0"),
            MockResponse::new(500, "").header("Retry-After", "0"),
            MockResponse::new(500, "").header("Retry-After", "0"),
        ]);
        let rpc = Rpc::new(reqwest::Client::new(), server.url());
        assert!(matches!(
            rpc.get_block_height().await.unwrap_err(),
            RpcError::RetryExhaustedHttp {
                method: "getBlockHeight",
                status
            } if status.as_u16() == 500
        ));
    }

    #[tokio::test]
    async fn rpc_rejects_mismatched_batch_lengths() {
        let server = MockServer::new(vec![MockResponse::new(
            200,
            r#"{"jsonrpc":"2.0","id":1,"result":{"context":{"slot":1},"value":[null]}}"#,
        )]);
        let rpc = Rpc::new(reqwest::Client::new(), server.url());
        assert!(matches!(
            rpc.get_balances(&["A", "B"]).await.unwrap_err(),
            RpcError::LengthMismatch {
                method: "getMultipleAccounts",
                expected: 2,
                actual: 1,
            }
        ));
    }

    #[tokio::test]
    async fn rpc_typed_transport_error_is_distinct() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let rpc = Rpc::new(reqwest::Client::new(), url);
        assert!(matches!(
            rpc.get_block_height().await.unwrap_err(),
            RpcError::Transport {
                method: "getBlockHeight",
                ..
            }
        ));
    }

    #[tokio::test]
    async fn rpc_typed_body_shape_errors_are_distinct() {
        let server = MockServer::new(vec![MockResponse::new(200, "not json")]);
        let rpc = Rpc::new(reqwest::Client::new(), server.url());
        assert!(matches!(
            rpc.get_block_height().await.unwrap_err(),
            RpcError::Decode {
                method: "getBlockHeight",
                ..
            }
        ));

        let server = MockServer::new(vec![MockResponse::new(200, r#"{"jsonrpc":"2.0","id":1}"#)]);
        let rpc = Rpc::new(reqwest::Client::new(), server.url());
        assert!(matches!(
            rpc.get_block_height().await.unwrap_err(),
            RpcError::MissingResult {
                method: "getBlockHeight"
            }
        ));
    }

    #[tokio::test]
    #[ignore = "hits live devnet RPC"]
    async fn devnet_blockhash_and_height() {
        let rpc = Rpc::new(reqwest::Client::new(), "https://api.devnet.solana.com");
        let (bh, lvbh) = rpc.get_latest_blockhash().await.unwrap();
        assert!(!bh.is_empty());
        let h = rpc.get_block_height().await.unwrap();
        assert!(h > 0 && lvbh >= h);
        let rent = rpc.get_min_balance_for_rent_exemption(0).await.unwrap();
        assert!(rent > 0);
    }
}
