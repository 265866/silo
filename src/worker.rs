use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::sync::{Semaphore, mpsc, watch};

use crate::app::{AppEvent, Command};
use crate::db::{IntentTransitionOutcome, Storage};
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
const REBROADCAST_INTERVAL: Duration = Duration::from_secs(12);

fn persist_last_price(db: &Storage, p: &SolPrice) {
    db.with(|d| {
        let _ = d.set_meta("last_price", &p.to_meta_json());
    });
}

fn current_currency(db: &Storage) -> crate::types::Currency {
    db.with(|d| d.get_meta("currency").ok().flatten())
        .and_then(|s| crate::types::Currency::from_code(&s))
        .unwrap_or(crate::types::Currency::Usd)
}

async fn send_error(evt: &mpsc::Sender<AppEvent>, generation: u64, message: impl Into<String>) {
    let _ = evt
        .send(AppEvent::Error {
            message: message.into(),
            generation,
        })
        .await;
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
    db: Storage,
    rpc: Arc<Mutex<Rpc>>,
    price: Arc<PriceCache>,
    client: reqwest::Client,
    generation: Arc<AtomicU64>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let price_handle = {
            let price = price.clone();
            let evt = evt_tx.clone();
            let client = client.clone();
            let db = db.clone();
            let generation = generation.clone();
            let mut shutdown = shutdown_rx.clone();
            tokio::spawn(async move {
                let mut cg_backoff_until: Option<Instant> = None;
                loop {
                    let jitter = {
                        let mut b = [0u8; 2];
                        crate::crypto::random_bytes(&mut b);
                        (u16::from_le_bytes(b) as u64) % PRICE_POLL_JITTER_MS
                    };
                    tokio::select! {
                        _ = shutdown.changed() => break,
                        _ = tokio::time::sleep(PRICE_POLL_BASE + Duration::from_millis(jitter)) => {}
                    }

                    let event_generation = generation.load(Ordering::SeqCst);
                    let currency = current_currency(&db);
                    let skip_cg = cg_backoff_until.is_some_and(|u| Instant::now() < u);
                    let (result, rate_limited) =
                        fetch_price_backoff_aware(&client, currency, skip_cg).await;
                    if generation.load(Ordering::SeqCst) != event_generation {
                        continue;
                    }
                    if rate_limited {
                        cg_backoff_until =
                            Some(Instant::now() + Duration::from_secs(COINGECKO_BACKOFF_SECS));
                    } else if !skip_cg {
                        cg_backoff_until = None;
                    }
                    if let Ok(p) = result {
                        price.set(p);
                        persist_last_price(&db, &p);
                        let _ = evt
                            .send(AppEvent::Price {
                                price: p,
                                generation: event_generation,
                            })
                            .await;
                    }
                }
            })
        };

        let (ordered_tx, mut ordered_rx) = mpsc::channel::<(u64, Command)>(64);
        let ordered_handle = {
            let db = db.clone();
            let rpc = rpc.clone();
            let evt = evt_tx.clone();
            let price = price.clone();
            let client = client.clone();
            let generation = generation.clone();
            let mut shutdown = shutdown_rx.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = shutdown.changed() => break,
                        maybe = ordered_rx.recv() => {
                            let Some((cmd_gen, cmd)) = maybe else {
                                break;
                            };
                            handle_command(
                                cmd_gen,
                                cmd,
                                db.clone(),
                                rpc.clone(),
                                evt.clone(),
                                price.clone(),
                                client.clone(),
                                generation.clone(),
                            )
                            .await;
                        }
                    }
                }
            })
        };

        let unordered_limit = Arc::new(Semaphore::new(4));
        let mut unordered = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                joined = unordered.join_next(), if !unordered.is_empty() => {
                    let _ = joined;
                }
                maybe = cmd_rx.recv() => {
                    let Some((cmd_gen, cmd)) = maybe else {
                        break;
                    };
                    if cmd.ordered() {
                        if ordered_tx.send((cmd_gen, cmd)).await.is_err() {
                            break;
                        }
                        continue;
                    }
                    let Ok(permit) = unordered_limit.clone().acquire_owned().await else {
                        break;
                    };
                    let db = db.clone();
                    let rpc = rpc.clone();
                    let evt = evt_tx.clone();
                    let price = price.clone();
                    let client = client.clone();
                    let generation = generation.clone();
                    unordered.spawn(async move {
                        let _permit = permit;
                        handle_command(cmd_gen, cmd, db, rpc, evt, price, client, generation).await;
                    });
                }
            }
        }

        let _ = shutdown_tx.send(true);
        drop(ordered_tx);
        while unordered.join_next().await.is_some() {}
        let _ = ordered_handle.await;
        let _ = price_handle.await;
    })
}

#[allow(clippy::too_many_arguments)]
async fn handle_command(
    cmd_gen: u64,
    cmd: Command,
    db: Storage,
    rpc: Arc<Mutex<Rpc>>,
    evt: mpsc::Sender<AppEvent>,
    price: Arc<PriceCache>,
    client: reqwest::Client,
    generation: Arc<AtomicU64>,
) {
    let rpc_now = { rpc.lock_recover().clone() };

    match cmd {
        Command::Reconcile => match reconcile_boot(&db, &rpc_now, &generation, cmd_gen).await {
            Ok(resolved) => {
                let _ = evt
                    .send(AppEvent::ReconcileComplete {
                        resolved,
                        generation: cmd_gen,
                    })
                    .await;
            }
            Err(_) => {
                let _ = evt
                    .send(AppEvent::ReconcileFailedOffline {
                        generation: cmd_gen,
                    })
                    .await;
            }
        },

        Command::FetchRentExempt => match rpc_now.get_min_balance_for_rent_exemption(0).await {
            Ok(v) => {
                let _ = db.with_current(&generation, cmd_gen, |d| {
                    let _ = d.set_meta("rent_exempt_min_0", &v.to_string());
                });
                let _ = evt
                    .send(AppEvent::RentExempt {
                        lamports: v,
                        generation: cmd_gen,
                    })
                    .await;
            }
            Err(e) => {
                send_error(&evt, cmd_gen, format!("rent lookup failed: {e}")).await;
            }
        },

        Command::FetchPrice => {
            let currency = current_currency(&db);
            match fetch_price(&client, currency).await {
                Ok(p) => {
                    if generation.load(Ordering::SeqCst) != cmd_gen {
                        return;
                    }
                    price.set(p);
                    persist_last_price(&db, &p);
                    let _ = evt
                        .send(AppEvent::Price {
                            price: p,
                            generation: cmd_gen,
                        })
                        .await;
                }
                Err(e) => {
                    send_error(&evt, cmd_gen, format!("price fetch failed: {e}")).await;
                }
            }
        }

        Command::RefreshBalances { include_archived } => {
            let wallets: Vec<(i64, String)> = match db.with(|d| d.list_wallets()) {
                Ok(ws) => ws
                    .into_iter()
                    .filter(|w| include_archived || !w.archived)
                    .map(|w| (w.id, w.pubkey))
                    .collect(),
                Err(e) => {
                    let _ = evt
                        .send(AppEvent::BalancesFailed {
                            reason: format!("could not load wallets: {e}"),
                            generation: cmd_gen,
                        })
                        .await;
                    return;
                }
            };
            if wallets.is_empty() {
                let _ = evt
                    .send(AppEvent::Balances {
                        list: Vec::new(),
                        generation: cmd_gen,
                    })
                    .await;
                return;
            }
            let pubkeys: Vec<&str> = wallets.iter().map(|(_, p)| p.as_str()).collect();
            match rpc_now.get_balances(&pubkeys).await {
                Ok(bals) => {
                    let list: Vec<(i64, u64)> =
                        wallets.iter().map(|(id, _)| *id).zip(bals).collect();
                    let _ = evt
                        .send(AppEvent::Balances {
                            list,
                            generation: cmd_gen,
                        })
                        .await;
                    let _ = evt
                        .send(AppEvent::NetStatus {
                            status: NetStatus::Online,
                            generation: cmd_gen,
                        })
                        .await;
                }
                Err(e) => {
                    let _ = evt
                        .send(AppEvent::BalancesFailed {
                            reason: e.to_string(),
                            generation: cmd_gen,
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
                    send_error(&evt, cmd_gen, format!("could not fetch blockhash: {e}")).await;
                    return;
                }
            };
            let dest_balance = match rpc_now.get_balance(&to).await {
                Ok(b) => b,
                Err(e) => {
                    send_error(
                        &evt,
                        cmd_gen,
                        format!("could not fetch recipient balance: {e}"),
                    )
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
                    generation: cmd_gen,
                })
                .await;
        }

        Command::Broadcast { intent_id } => {
            broadcast_and_poll(intent_id, db, rpc, evt, generation, cmd_gen).await;
        }

        Command::ChangeRpc { url } => {
            if generation.load(Ordering::SeqCst) != cmd_gen {
                return;
            }
            let url = match crate::solana::rpc::validate_rpc_url(&url) {
                Ok(url) => url,
                Err(e) => {
                    send_error(&evt, cmd_gen, format!("invalid RPC URL: {e}")).await;
                    return;
                }
            };
            let redacted = crate::solana::rpc::redact_rpc_url(&url);
            let wrote = db.with_current(&generation, cmd_gen, |d| {
                d.set_meta_audited(
                    "rpc_url",
                    &url,
                    AuditEvent::RpcChanged,
                    &json!({ "url": redacted }),
                )
            });
            match wrote {
                Some(Ok(())) => {}
                Some(Err(e)) => {
                    send_error(&evt, cmd_gen, format!("could not save RPC URL: {e}")).await;
                    return;
                }
                None => return,
            }
            {
                let mut g = rpc.lock_recover();
                *g = Rpc::new(client.clone(), url.clone());
            }
            let _ = evt
                .send(AppEvent::RpcChanged {
                    url,
                    generation: cmd_gen,
                })
                .await;
            let new_rpc = { rpc.lock_recover().clone() };
            match reconcile_boot(&db, &new_rpc, &generation, cmd_gen).await {
                Ok(resolved) => {
                    let _ = evt
                        .send(AppEvent::ReconcileComplete {
                            resolved,
                            generation: cmd_gen,
                        })
                        .await;
                }
                Err(_) => {
                    let _ = evt
                        .send(AppEvent::ReconcileFailedOffline {
                            generation: cmd_gen,
                        })
                        .await;
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn finalize(
    db: &Storage,
    evt: &mpsc::Sender<AppEvent>,
    intent_id: i64,
    sig: &str,
    status: IntentStatus,
    error: Option<&str>,
    generation: &AtomicU64,
    cmd_gen: u64,
) {
    let Some(outcome) = db.with_current(generation, cmd_gen, |d| {
        d.mark_terminal(intent_id, status, error)
    }) else {
        return;
    };
    let final_status = if matches!(outcome, Ok(IntentTransitionOutcome::Applied)) {
        status
    } else {
        match db.with_current(generation, cmd_gen, |d| {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PollDecision {
    Continue,
    WaitFinality,
    Confirm,
    Fail,
    ExpireCandidate,
}

fn poll_decision(
    status: Option<&crate::solana::rpc::SignatureStatus>,
    height: Option<u64>,
    lvbh: u64,
) -> PollDecision {
    if let Some(st) = status {
        if st.is_error() {
            return PollDecision::Fail;
        }
        if st.is_finalized() {
            return PollDecision::Confirm;
        }
        if st.is_confirmed() {
            return PollDecision::WaitFinality;
        }
        if height.is_some_and(|h| h > lvbh + EXPIRY_SLACK) {
            return PollDecision::ExpireCandidate;
        }
        return PollDecision::Continue;
    }
    if height.is_some_and(|h| h > lvbh + EXPIRY_SLACK) {
        PollDecision::ExpireCandidate
    } else {
        PollDecision::Continue
    }
}

async fn rebroadcast_if_due(
    rpc: &Rpc,
    bytes: &[u8],
    sig: &str,
    lvbh: u64,
    last_rebroadcast: &mut Option<Instant>,
) -> Result<(), String> {
    if last_rebroadcast.is_some_and(|last| last.elapsed() < REBROADCAST_INTERVAL) {
        return Ok(());
    }
    let Ok(height) = rpc.get_block_height().await else {
        return Ok(());
    };
    if height > lvbh {
        return Ok(());
    }
    *last_rebroadcast = Some(Instant::now());
    match rpc.send_transaction(bytes).await {
        Ok(returned) if returned != sig => Err(returned),
        _ => Ok(()),
    }
}

async fn broadcast_and_poll(
    intent_id: i64,
    db: Storage,
    rpc_arc: Arc<Mutex<Rpc>>,
    evt: mpsc::Sender<AppEvent>,
    generation: Arc<AtomicU64>,
    cmd_gen: u64,
) {
    use std::sync::atomic::Ordering;
    let current = || generation.load(Ordering::SeqCst) == cmd_gen;
    let intent = match db.with_current(&generation, cmd_gen, |d| {
        d.get_intent(intent_id).ok().flatten()
    }) {
        None => return,
        Some(intent) => intent,
    };
    let Some(intent) = intent else {
        send_error(&evt, cmd_gen, "transfer record vanished").await;
        return;
    };
    let (Some(bytes), Some(sig)) = (intent.signed_tx, intent.signature) else {
        send_error(&evt, cmd_gen, "transfer was not signed").await;
        return;
    };
    let lvbh = intent.last_valid_block_height.unwrap_or(0);

    match db.with_current(&generation, cmd_gen, |d| d.mark_submitted(intent_id)) {
        Some(Ok(IntentTransitionOutcome::Applied)) => {}
        Some(Ok(IntentTransitionOutcome::WrongState(_) | IntentTransitionOutcome::NotFound)) => {
            send_error(
                &evt,
                cmd_gen,
                "transfer was not in signed state; not broadcasting",
            )
            .await;
            return;
        }
        Some(Err(e)) => {
            send_error(
                &evt,
                cmd_gen,
                format!("could not record submitted transfer: {e}"),
            )
            .await;
            return;
        }
        None => return,
    }

    let mut last_rebroadcast = None;
    let rpc = { rpc_arc.lock_recover().clone() };
    match rpc.send_transaction(&bytes).await {
        Ok(returned) if returned != sig => {
            let _ = db.with_current(&generation, cmd_gen, |d| {
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
            last_rebroadcast = Some(Instant::now());
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
            send_error(
                &evt,
                cmd_gen,
                format!("broadcast uncertain — polling signed transfer: {e}"),
            )
            .await;
        }
    }

    let mut reported_pending = false;
    loop {
        for _ in 0..CONFIRM_POLL_ATTEMPTS {
            tokio::time::sleep(CONFIRM_POLL_INTERVAL).await;
            if !current() {
                return;
            }
            let rpc = { rpc_arc.lock_recover().clone() };

            let status = sig_status(&rpc, &sig).await;
            let height = if status
                .as_ref()
                .is_none_or(|st| !st.is_error() && !st.is_confirmed() && !st.is_finalized())
            {
                rpc.get_block_height().await.ok()
            } else {
                None
            };
            match poll_decision(status.as_ref(), height, lvbh) {
                PollDecision::Fail => {
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
                PollDecision::Confirm => {
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
                PollDecision::Continue => {
                    if let Err(returned) =
                        rebroadcast_if_due(&rpc, &bytes, &sig, lvbh, &mut last_rebroadcast).await
                    {
                        let _ = db.with_current(&generation, cmd_gen, |d| {
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
                }
                PollDecision::WaitFinality => {}
                PollDecision::ExpireCandidate => {
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
                        if s2.is_finalized() {
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
        }

        if !current() {
            return;
        }
        if !reported_pending {
            let _ = evt
                .send(AppEvent::TransferResult {
                    intent_id,
                    outcome: TransferOutcome::StillPending {
                        signature: sig.clone(),
                    },
                    generation: cmd_gen,
                })
                .await;
            reported_pending = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn status(err: bool, confirmation_status: Option<&str>) -> crate::solana::rpc::SignatureStatus {
        crate::solana::rpc::SignatureStatus {
            slot: 1,
            confirmations: None,
            err: err.then_some(json!("err")),
            confirmation_status: confirmation_status.map(String::from),
        }
    }

    #[test]
    fn ordered_commands_cover_money_state_changes() {
        assert!(Command::Reconcile.ordered());
        assert!(
            Command::PrepareSend {
                from_id: 1,
                to: "to".into(),
                lamports: 1,
                priority_micro: 0,
            }
            .ordered()
        );
        assert!(Command::Broadcast { intent_id: 1 }.ordered());
        assert!(
            Command::ChangeRpc {
                url: "https://rpc.example.com".into(),
            }
            .ordered()
        );
        assert!(!Command::FetchPrice.ordered());
        assert!(!Command::FetchRentExempt.ordered());
        assert!(
            !Command::RefreshBalances {
                include_archived: false,
            }
            .ordered()
        );
    }

    #[test]
    fn poll_decision_covers_terminal_and_pending_states() {
        assert_eq!(
            poll_decision(Some(&status(false, Some("confirmed"))), None, 100),
            PollDecision::WaitFinality
        );
        assert_eq!(
            poll_decision(Some(&status(false, Some("finalized"))), None, 100),
            PollDecision::Confirm
        );
        assert_eq!(
            poll_decision(Some(&status(true, Some("confirmed"))), None, 100),
            PollDecision::Fail
        );
        assert_eq!(
            poll_decision(Some(&status(false, Some("processed"))), None, 100),
            PollDecision::Continue
        );
        assert_eq!(
            poll_decision(
                Some(&status(false, Some("processed"))),
                Some(100 + EXPIRY_SLACK + 1),
                100,
            ),
            PollDecision::ExpireCandidate
        );
        assert_eq!(
            poll_decision(None, Some(100 + EXPIRY_SLACK), 100),
            PollDecision::Continue
        );
        assert_eq!(
            poll_decision(None, Some(100 + EXPIRY_SLACK + 1), 100),
            PollDecision::ExpireCandidate
        );
    }
}
