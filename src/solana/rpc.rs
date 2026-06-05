use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use futures_util::stream::{self, StreamExt};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;

use super::tx::base64_encode;
use crate::types::Commitment;

const MAX_RETRIES: u32 = 4;
const GMA_CHUNK: usize = 100;
const SIGSTATUS_CHUNK: usize = 256;
const FANOUT: usize = 3;

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
    pub fn is_confirmed_or_finalized(&self) -> bool {
        matches!(
            self.confirmation_status.as_deref(),
            Some("confirmed") | Some("finalized")
        )
    }
}

#[derive(Clone)]
pub struct Rpc {
    client: reqwest::Client,
    url: String,
    commitment: &'static str,
}

#[derive(Deserialize)]
struct RpcEnvelope<T> {
    result: Option<T>,
    error: Option<RpcErr>,
}

#[derive(Deserialize)]
struct RpcErr {
    code: i64,
    message: String,
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
        method: &str,
        params: serde_json::Value,
    ) -> Result<T> {
        let body = json!({"jsonrpc":"2.0","id":1,"method":method,"params":params});
        let mut attempt = 0u32;
        loop {
            match self.client.post(&self.url).json(&body).send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.as_u16() == 429 || status.as_u16() == 408 || status.is_server_error()
                    {
                        if attempt >= MAX_RETRIES {
                            bail!("RPC {method} failed after retries: HTTP {status}");
                        }
                        let wait = retry_after(&resp).unwrap_or_else(|| backoff(attempt));
                        attempt += 1;
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    let env: RpcEnvelope<T> = resp.error_for_status()?.json().await?;
                    if let Some(e) = env.error {
                        bail!("RPC {method} error {}: {}", e.code, e.message);
                    }
                    return env
                        .result
                        .ok_or_else(|| anyhow!("RPC {method}: empty result"));
                }
                Err(e) => {
                    if attempt >= MAX_RETRIES {
                        return Err(anyhow!("RPC {method} request failed: {e}"));
                    }
                    let wait = backoff(attempt);
                    attempt += 1;
                    tokio::time::sleep(wait).await;
                }
            }
        }
    }

    pub async fn get_balances(&self, pubkeys: &[String]) -> Result<Vec<u64>> {
        if pubkeys.is_empty() {
            return Ok(vec![]);
        }
        let chunks: Vec<(usize, Vec<String>)> = pubkeys
            .chunks(GMA_CHUNK)
            .enumerate()
            .map(|(i, c)| (i, c.to_vec()))
            .collect();

        let this = self;
        let mut results: Vec<(usize, Vec<u64>)> = stream::iter(chunks)
            .map(|(i, chunk)| async move {
                let params = json!([
                    chunk,
                    {"commitment": this.commitment, "encoding": "base64",
                     "dataSlice": {"offset": 0, "length": 0}}
                ]);
                let ctx: Ctx<Vec<Option<AccountInfo>>> =
                    this.call("getMultipleAccounts", params).await?;
                let balances = ctx
                    .value
                    .into_iter()
                    .map(|o| o.map(|a| a.lamports).unwrap_or(0))
                    .collect::<Vec<u64>>();
                Ok::<_, anyhow::Error>((i, balances))
            })
            .buffer_unordered(FANOUT)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()?;

        results.sort_by_key(|(i, _)| *i);
        Ok(results.into_iter().flat_map(|(_, v)| v).collect())
    }

    pub async fn get_balance(&self, pubkey: &str) -> Result<u64> {
        let v = self
            .get_balances(std::slice::from_ref(&pubkey.to_string()))
            .await?;
        Ok(v.first().copied().unwrap_or(0))
    }

    pub async fn get_latest_blockhash(&self) -> Result<(String, u64)> {
        let params = json!([{"commitment": self.commitment}]);
        let ctx: Ctx<BlockhashValue> = self.call("getLatestBlockhash", params).await?;
        Ok((ctx.value.blockhash, ctx.value.last_valid_block_height))
    }

    pub async fn send_transaction(&self, wire: &[u8]) -> Result<String> {
        let params = json!([
            base64_encode(wire),
            {"encoding": "base64", "skipPreflight": false,
             "preflightCommitment": self.commitment, "maxRetries": 0}
        ]);
        self.call("sendTransaction", params).await
    }

    pub async fn get_signature_statuses(
        &self,
        signatures: &[String],
        search_history: bool,
    ) -> Result<Vec<Option<SignatureStatus>>> {
        if signatures.is_empty() {
            return Ok(vec![]);
        }
        let chunks: Vec<(usize, Vec<String>)> = signatures
            .chunks(SIGSTATUS_CHUNK)
            .enumerate()
            .map(|(i, c)| (i, c.to_vec()))
            .collect();

        let this = self;
        let mut results: Vec<(usize, Vec<Option<SignatureStatus>>)> = stream::iter(chunks)
            .map(|(i, chunk)| async move {
                let params = json!([chunk, {"searchTransactionHistory": search_history}]);
                let ctx: Ctx<Vec<Option<SignatureStatus>>> =
                    this.call("getSignatureStatuses", params).await?;
                Ok::<_, anyhow::Error>((i, ctx.value))
            })
            .buffer_unordered(FANOUT)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()?;

        results.sort_by_key(|(i, _)| *i);
        Ok(results.into_iter().flat_map(|(_, v)| v).collect())
    }

    pub async fn get_block_height(&self) -> Result<u64> {
        self.call("getBlockHeight", json!([{"commitment": self.commitment}]))
            .await
    }

    pub async fn get_min_balance_for_rent_exemption(&self, data_len: usize) -> Result<u64> {
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
    let secs = raw.trim().parse::<u64>().ok()?;
    Some(Duration::from_secs(secs.min(60)))
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
