use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use tokio::sync::mpsc;

use crate::app::{AppEvent, Command};
use crate::db::Db;
use crate::price::{
    COINGECKO_BACKOFF_SECS, PriceCache, SolPrice, fetch_price, fetch_price_backoff_aware,
};
use crate::solana::reconcile::{EXPIRY_SLACK, reconcile_boot};
use crate::solana::rpc::Rpc;
use crate::sync::MutexExt;
use crate::types::{AuditEvent, IntentStatus, NetStatus, TransferOutcome};

const PRICE_POLL_BASE: Duration = Duration::from_secs(60);
const PRICE_POLL_JITTER_MS: u64 = 10_000;

const CONFIRM_POLL_INTERVAL: Duration = Duration::from_secs(2);
const CONFIRM_POLL_ATTEMPTS: usize = 45;

fn persist_last_price(db: &Arc<Mutex<Db>>, p: &SolPrice) {
    if let Ok(d) = db.lock() {
        let _ = d.set_meta("last_price", &p.to_meta_json());
    }
}

fn current_currency(db: &Arc<Mutex<Db>>) -> crate::types::Currency {
    db.lock()
        .ok()
        .and_then(|d| d.get_meta("currency").ok().flatten())
        .and_then(|s| crate::types::Currency::from_code(&s))
        .unwrap_or(crate::types::Currency::Usd)
}

pub fn build_client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("silo/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| anyhow::anyhow!("failed to build HTTP client: {e}"))
}

pub fn spawn_workers(
    mut cmd_rx: mpsc::Receiver<(u64, Command)>,
    evt_tx: mpsc::Sender<AppEvent>,
    db: Arc<Mutex<Db>>,
    rpc: Arc<Mutex<Rpc>>,
    price: Arc<PriceCache>,
    client: reqwest::Client,
    generation: Arc<AtomicU64>,
) -> tokio::task::JoinHandle<()> {
    {
        let price = price.clone();
        let evt = evt_tx.clone();
        let client = client.clone();
        let db = db.clone();
        tokio::spawn(async move {
            use std::time::Instant;
            let mut cg_backoff_until: Option<Instant> = None;
            loop {
                let jitter = {
                    let mut b = [0u8; 2];
                    crate::crypto::random_bytes(&mut b);
                    (u16::from_le_bytes(b) as u64) % PRICE_POLL_JITTER_MS
                };
                tokio::time::sleep(PRICE_POLL_BASE + Duration::from_millis(jitter)).await;

                let currency = current_currency(&db);
                let skip_cg = cg_backoff_until.is_some_and(|u| Instant::now() < u);
                let (result, rate_limited) =
                    fetch_price_backoff_aware(&client, currency, skip_cg).await;
                if rate_limited {
                    cg_backoff_until =
                        Some(Instant::now() + Duration::from_secs(COINGECKO_BACKOFF_SECS));
                } else if !skip_cg {
                    cg_backoff_until = None;
                }
                if let Ok(p) = result {
                    price.set(p);
                    persist_last_price(&db, &p);
                    let _ = evt.send(AppEvent::Price(p)).await;
                }
            }
        });
    }

    tokio::spawn(async move {
        while let Some((cmd_gen, cmd)) = cmd_rx.recv().await {
            let db = db.clone();
            let rpc = rpc.clone();
            let evt = evt_tx.clone();
            let price = price.clone();
            let client = client.clone();
            let generation = generation.clone();
            tokio::spawn(async move {
                handle_command(cmd_gen, cmd, db, rpc, evt, price, client, generation).await;
            });
        }
    })
}

#[allow(clippy::too_many_arguments)]
async fn handle_command(
    cmd_gen: u64,
    cmd: Command,
    db: Arc<Mutex<Db>>,
    rpc: Arc<Mutex<Rpc>>,
    evt: mpsc::Sender<AppEvent>,
    price: Arc<PriceCache>,
    client: reqwest::Client,
    generation: Arc<AtomicU64>,
) {
    let rpc_now = { rpc.lock_recover().clone() };

    match cmd {
        Command::Reconcile => {
            match reconcile_boot(db.as_ref(), &rpc_now, &generation, cmd_gen).await {
                Ok(resolved) => {
                    let _ = evt
                        .send(AppEvent::ReconcileComplete {
                            resolved,
                            generation: cmd_gen,
                        })
                        .await;
                }
                Err(_) => {
                    let _ = evt.send(AppEvent::ReconcileFailedOffline).await;
                }
            }
        }

        Command::FetchRentExempt => match rpc_now.get_min_balance_for_rent_exemption(0).await {
            Ok(v) => {
                let _ = crate::db::with_current_db(&db, &generation, cmd_gen, |d| {
                    let _ = d.set_meta("rent_exempt_min_0", &v.to_string());
                });
                let _ = evt.send(AppEvent::RentExempt(v)).await;
            }
            Err(e) => {
                let _ = evt
                    .send(AppEvent::Error(format!("rent lookup failed: {e}")))
                    .await;
            }
        },

        Command::FetchPrice => {
            let currency = current_currency(&db);
            match fetch_price(&client, currency).await {
                Ok(p) => {
                    price.set(p);
                    persist_last_price(&db, &p);
                    let _ = evt.send(AppEvent::Price(p)).await;
                }
                Err(e) => {
                    let _ = evt
                        .send(AppEvent::Error(format!("price fetch failed: {e}")))
                        .await;
                }
            }
        }

        Command::RefreshBalances { include_archived } => {
            let wallets: Vec<(i64, String)> = {
                let d = db.lock().unwrap();
                d.list_wallets()
                    .map(|ws| {
                        ws.into_iter()
                            .filter(|w| include_archived || !w.archived)
                            .map(|w| (w.id, w.pubkey))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            if wallets.is_empty() {
                return;
            }
            let pubkeys: Vec<&str> = wallets.iter().map(|(_, p)| p.as_str()).collect();
            match rpc_now.get_balances(&pubkeys).await {
                Ok(bals) => {
                    let list: Vec<(i64, u64)> =
                        wallets.iter().map(|(id, _)| *id).zip(bals).collect();
                    let _ = evt.send(AppEvent::Balances(list)).await;
                    let _ = evt.send(AppEvent::NetStatus(NetStatus::Online)).await;
                }
                Err(e) => {
                    let _ = evt
                        .send(AppEvent::BalancesFailed {
                            reason: e.to_string(),
                        })
                        .await;
                }
            }
        }

        Command::PrepareSend {
            from_id,
            to,
            lamports,
            priority_micro,
        } => {
            let (blockhash, lvbh) = match rpc_now.get_latest_blockhash().await {
                Ok(x) => x,
                Err(e) => {
                    let _ = evt
                        .send(AppEvent::Error(format!("could not fetch blockhash: {e}")))
                        .await;
                    return;
                }
            };
            let dest_balance = match rpc_now.get_balance(&to).await {
                Ok(b) => b,
                Err(e) => {
                    let _ = evt
                        .send(AppEvent::Error(format!(
                            "could not fetch recipient balance: {e}"
                        )))
                        .await;
                    return;
                }
            };
            let _ = evt
                .send(AppEvent::SendPrepared {
                    from_id,
                    to,
                    lamports,
                    blockhash,
                    lvbh,
                    fee: crate::money::total_fee(priority_micro),
                    dest_balance,
                    priority_micro,
                })
                .await;
        }

        Command::Broadcast { intent_id } => {
            broadcast_and_poll(intent_id, db, rpc, evt, generation, cmd_gen).await;
        }

        Command::ChangeRpc { url } => {
            if generation.load(std::sync::atomic::Ordering::SeqCst) != cmd_gen {
                return;
            }
            {
                let mut g = rpc.lock_recover();
                *g = Rpc::new(client.clone(), url.clone());
            }
            let wrote = crate::db::with_current_db(&db, &generation, cmd_gen, |d| {
                let _ = d.set_meta("rpc_url", &url);
                let _ = d.audit(AuditEvent::RpcChanged, &json!({ "url": url }));
            });
            if wrote.is_none() {
                return;
            }
            let new_rpc = { rpc.lock_recover().clone() };
            match reconcile_boot(db.as_ref(), &new_rpc, &generation, cmd_gen).await {
                Ok(resolved) => {
                    let _ = evt
                        .send(AppEvent::ReconcileComplete {
                            resolved,
                            generation: cmd_gen,
                        })
                        .await;
                }
                Err(_) => {
                    let _ = evt.send(AppEvent::ReconcileFailedOffline).await;
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn finalize(
    db: &Arc<Mutex<Db>>,
    evt: &mpsc::Sender<AppEvent>,
    intent_id: i64,
    sig: &str,
    status: IntentStatus,
    error: Option<&str>,
    generation: &AtomicU64,
    cmd_gen: u64,
) {
    use crate::db::with_current_db;
    let Some(changed) = with_current_db(db, generation, cmd_gen, |d| {
        d.mark_terminal(intent_id, status, error).unwrap_or(false)
    }) else {
        return;
    };
    let final_status = if changed {
        status
    } else {
        match with_current_db(db, generation, cmd_gen, |d| {
            d.get_intent(intent_id).ok().flatten().map(|i| i.status)
        }) {
            Some(Some(s)) => s,
            Some(None) => status,
            None => return,
        }
    };
    let outcome = match final_status {
        IntentStatus::Confirmed => TransferOutcome::Confirmed {
            signature: sig.to_string(),
        },
        IntentStatus::Failed => TransferOutcome::Failed {
            reason: error.unwrap_or("failed").to_string(),
        },
        IntentStatus::Expired => TransferOutcome::Expired,
        _ => return,
    };
    let _ = evt
        .send(AppEvent::TransferResult {
            intent_id,
            outcome,
            generation: cmd_gen,
        })
        .await;
}

async fn sig_status(rpc: &Rpc, sig: &str) -> Option<crate::solana::rpc::SignatureStatus> {
    rpc.get_signature_statuses(&[sig], true)
        .await
        .ok()
        .and_then(|v| v.into_iter().next().flatten())
}

async fn broadcast_and_poll(
    intent_id: i64,
    db: Arc<Mutex<Db>>,
    rpc_arc: Arc<Mutex<Rpc>>,
    evt: mpsc::Sender<AppEvent>,
    generation: Arc<AtomicU64>,
    cmd_gen: u64,
) {
    use crate::db::with_current_db;
    use std::sync::atomic::Ordering;
    let current = || generation.load(Ordering::SeqCst) == cmd_gen;
    let intent = match with_current_db(&db, &generation, cmd_gen, |d| {
        d.get_intent(intent_id).ok().flatten()
    }) {
        None => return,
        Some(intent) => intent,
    };
    let Some(intent) = intent else {
        let _ = evt
            .send(AppEvent::Error("transfer record vanished".into()))
            .await;
        return;
    };
    let signed_tx = intent.signed_tx;
    let signature = intent.signature;
    let lvbh = intent.last_valid_block_height.unwrap_or(0);
    let (Some(bytes), Some(sig)) = (signed_tx, signature) else {
        let _ = evt
            .send(AppEvent::Error("transfer was not signed".into()))
            .await;
        return;
    };

    if with_current_db(&db, &generation, cmd_gen, |d| {
        let _ = d.mark_submitted(intent_id);
    })
    .is_none()
    {
        return;
    }

    let rpc = { rpc_arc.lock_recover().clone() };
    match rpc.send_transaction(&bytes).await {
        Ok(returned) if returned != sig => {
            let _ = with_current_db(&db, &generation, cmd_gen, |d| {
                let _ = d.audit(
                    AuditEvent::IntegrityCheckFailed,
                    &json!({"intent": intent_id, "expected": sig, "got": returned}),
                );
            });
            finalize(
                &db,
                &evt,
                intent_id,
                &sig,
                IntentStatus::Failed,
                Some("rpc returned mismatched signature"),
                &generation,
                cmd_gen,
            )
            .await;
            return;
        }
        Ok(_) => {
            let _ = evt
                .send(AppEvent::TransferResult {
                    intent_id,
                    outcome: TransferOutcome::Submitted {
                        signature: sig.clone(),
                    },
                    generation: cmd_gen,
                })
                .await;
        }
        Err(e) => {
            let _ = evt
                .send(AppEvent::Error(format!(
                    "broadcast failed (will retry): {e}"
                )))
                .await;
            return;
        }
    }

    for _ in 0..CONFIRM_POLL_ATTEMPTS {
        tokio::time::sleep(CONFIRM_POLL_INTERVAL).await;
        if !current() {
            return;
        }
        let rpc = { rpc_arc.lock_recover().clone() };

        if let Some(st) = sig_status(&rpc, &sig).await {
            if st.is_error() {
                finalize(
                    &db,
                    &evt,
                    intent_id,
                    &sig,
                    IntentStatus::Failed,
                    Some("on-chain error"),
                    &generation,
                    cmd_gen,
                )
                .await;
                return;
            }
            if st.is_confirmed_or_finalized() {
                finalize(
                    &db,
                    &evt,
                    intent_id,
                    &sig,
                    IntentStatus::Confirmed,
                    None,
                    &generation,
                    cmd_gen,
                )
                .await;
                return;
            }
        } else if let Ok(h) = rpc.get_block_height().await
            && h > lvbh + EXPIRY_SLACK
        {
            if let Some(s2) = sig_status(&rpc, &sig).await {
                if s2.is_error() {
                    finalize(
                        &db,
                        &evt,
                        intent_id,
                        &sig,
                        IntentStatus::Failed,
                        Some("on-chain error"),
                        &generation,
                        cmd_gen,
                    )
                    .await;
                    return;
                }
                if s2.is_confirmed_or_finalized() {
                    finalize(
                        &db,
                        &evt,
                        intent_id,
                        &sig,
                        IntentStatus::Confirmed,
                        None,
                        &generation,
                        cmd_gen,
                    )
                    .await;
                    return;
                }
            }
            finalize(
                &db,
                &evt,
                intent_id,
                &sig,
                IntentStatus::Expired,
                Some("blockhash expired before confirmation"),
                &generation,
                cmd_gen,
            )
            .await;
            return;
        }
    }

    if current() {
        let _ = evt
            .send(AppEvent::TransferResult {
                intent_id,
                outcome: TransferOutcome::StillPending {
                    signature: sig.clone(),
                },
                generation: cmd_gen,
            })
            .await;
    }
}
