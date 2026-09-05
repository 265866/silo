use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::sync::{Semaphore, mpsc, watch};

use crate::app::{
    AppEvent, ClipboardCopyResult, Command, OptimisticTransfer, ProfileDeleteResult,
    ProfileOpenedPayload, SendPersistResult, SettingChange, SetupResult, UnlockResult,
    WalletTextField,
};
use crate::db::{IntentTransitionOutcome, Storage};
use crate::price::{
    COINGECKO_BACKOFF_SECS, PriceCache, SolPrice, fetch_price, fetch_price_backoff_aware,
};
use crate::solana::reconcile::{
    BLOCKHASH_EXPIRED, Decision, ON_CHAIN_ERROR, RecheckOutcome, decide, recheck_outcome,
    reconcile_boot,
};
use crate::solana::rpc::Rpc;
use crate::sync::MutexExt;
use crate::types::{AuditEvent, IntentStatus, TerminalStatus, TransferOutcome};

const PRICE_POLL_BASE: Duration = Duration::from_secs(60);
const PRICE_POLL_JITTER_MS: u64 = 10_000;

const CONFIRM_POLL_INTERVAL: Duration = Duration::from_secs(2);
const CONFIRM_POLL_ATTEMPTS: usize = 45;
const CONFIRM_MAX_ROUNDS: usize = 3;
const REBROADCAST_INTERVAL: Duration = Duration::from_secs(12);

async fn publish_price(
    db: &Storage,
    price: Arc<PriceCache>,
    evt: &mpsc::Sender<AppEvent>,
    generation: Arc<AtomicU64>,
    event_generation: u64,
    p: SolPrice,
) -> anyhow::Result<bool> {
    let accepted = db
        .call_current(
            generation,
            event_generation,
            move |d| -> anyhow::Result<bool> {
                if current_currency(d)? != p.currency {
                    return Ok(false);
                }
                // Serialize acceptance with settings writes and database replacement.
                // A failed persistence must leave the live cache untouched.
                d.set_meta("last_price", &p.to_meta_json())?;
                price.set(p);
                Ok(true)
            },
        )
        .await
        .unwrap_or(Ok(false))?;
    if accepted {
        let _ = evt
            .send(AppEvent::Price {
                price: p,
                generation: event_generation,
            })
            .await;
    }
    Ok(accepted)
}

fn current_currency(db: &crate::db::Db) -> anyhow::Result<crate::types::Currency> {
    Ok(db
        .get_meta("currency")?
        .and_then(|s| crate::types::Currency::from_code(&s))
        .unwrap_or(crate::types::Currency::Usd))
}

async fn send_error(evt: &mpsc::Sender<AppEvent>, generation: u64, message: impl Into<String>) {
    let _ = evt
        .send(AppEvent::Error {
            message: message.into(),
            generation,
        })
        .await;
}

fn definitive_rejection_reason(e: &crate::solana::rpc::RpcError) -> Option<String> {
    use crate::solana::rpc::RpcError;
    match e {
        RpcError::JsonRpc { message, .. } => {
            Some(format!("transfer rejected by network: {message}"))
        }
        RpcError::NonRetryHttp { status, body, .. } => {
            let body = body.trim();
            if body.is_empty() {
                Some(format!("transfer rejected by network: HTTP {status}"))
            } else {
                Some(format!(
                    "transfer rejected by network: HTTP {status}: {body}"
                ))
            }
        }
        _ => None,
    }
}

pub fn build_client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("silo/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| anyhow::anyhow!("failed to build HTTP client: {e}"))
}

fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

async fn fetch_latest_release(client: &reqwest::Client) -> anyhow::Result<String> {
    let resp = client
        .get(crate::update::releases_api_url())
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .timeout(Duration::from_secs(8))
        .send()
        .await?
        .error_for_status()?;
    let json: serde_json::Value = resp.json().await?;
    let tag = json
        .get("tag_name")
        .and_then(|t| t.as_str())
        .ok_or_else(|| anyhow::anyhow!("release response missing tag_name"))?;
    Ok(tag.trim_start_matches('v').to_string())
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
                    let currency = match db.call(|d| current_currency(d)).await {
                        Ok(currency) => currency,
                        Err(e) => {
                            send_error(
                                &evt,
                                event_generation,
                                format!("price currency lookup failed: {e:#}"),
                            )
                            .await;
                            continue;
                        }
                    };
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
                    if let Ok(p) = result
                        && let Err(e) = publish_price(
                            &db,
                            price.clone(),
                            &evt,
                            generation.clone(),
                            event_generation,
                            p,
                        )
                        .await
                    {
                        send_error(
                            &evt,
                            event_generation,
                            format!("price publication failed: {e:#}"),
                        )
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
                let mut confirm_tasks = tokio::task::JoinSet::new();
                loop {
                    tokio::select! {
                        _ = shutdown.changed() => break,
                        joined = confirm_tasks.join_next(), if !confirm_tasks.is_empty() => {
                            let _ = joined;
                        }
                        maybe = ordered_rx.recv() => {
                            let Some((cmd_gen, cmd)) = maybe else {
                                break;
                            };
                            match cmd {
                                Command::Broadcast { intent_id } => {
                                    let db = db.clone();
                                    let rpc = rpc.clone();
                                    let evt = evt.clone();
                                    let generation = generation.clone();
                                    confirm_tasks.spawn(async move {
                                        broadcast_and_poll(
                                            intent_id, db, rpc, evt, generation, cmd_gen,
                                        )
                                        .await;
                                    });
                                }
                                other => {
                                    handle_command(
                                        cmd_gen,
                                        other,
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
                        match ordered_tx.try_send((cmd_gen, cmd)) {
                            Ok(()) => {}
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                send_error(
                                    &evt_tx,
                                    cmd_gen,
                                    "system busy — command dropped, please retry",
                                )
                                .await;
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => break,
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

fn next_wallet_name(profiles: &[crate::profiles::ProfileMeta]) -> String {
    let max = profiles
        .iter()
        .filter_map(|p| p.name.strip_prefix("Wallet "))
        .filter_map(|n| n.trim().parse::<u32>().ok())
        .max()
        .unwrap_or(0);
    format!("Wallet {}", max + 1)
}

enum WalletCheckError {
    Mismatch,
    Read(String),
}

fn wallet_consistency(
    db: &Storage,
    seed: &crate::crypto::Seed,
) -> Result<Vec<crate::types::WalletRow>, WalletCheckError> {
    let wallets = db
        .call_blocking(|d| d.list_wallets())
        .map_err(|e| WalletCheckError::Read(e.to_string()))?;
    if wallets
        .iter()
        .all(|w| crate::crypto::derive_address(seed, w.account_index) == w.pubkey)
    {
        Ok(wallets)
    } else {
        Err(WalletCheckError::Mismatch)
    }
}

fn unlock_vault_blocking(
    db: Storage,
    vault_path: std::path::PathBuf,
    passphrase: zeroize::Zeroizing<String>,
) -> UnlockResult {
    let unlocked = crate::vault::unlock_vault_keyed(&vault_path, &passphrase);
    drop(passphrase);
    let (mnemonic, vault_key) = match unlocked {
        Ok(v) => v,
        Err(_) => {
            db.call_blocking(|d| {
                let _ = d.audit(AuditEvent::VaultUnlockFailed, &serde_json::json!({}));
            });
            return UnlockResult::WrongPassphrase;
        }
    };
    let seed = crate::crypto::seed_from_mnemonic(&mnemonic);
    drop(mnemonic);
    let key_ok = db.call_blocking(move |d| d.unlock_audit_key(vault_key.as_bytes()).is_ok());
    if !key_ok {
        return UnlockResult::AuditKey;
    }
    let mut wallets = match wallet_consistency(&db, &seed) {
        Ok(wallets) => wallets,
        Err(WalletCheckError::Mismatch) => {
            db.call_blocking(|d| {
                let _ = d.audit(AuditEvent::IntegrityCheckFailed, &serde_json::json!({}));
            });
            return UnlockResult::WalletMismatch;
        }
        Err(WalletCheckError::Read(e)) => return UnlockResult::WalletRead(e),
    };
    match db.call_blocking(|d| d.verify_audit_chain()) {
        Ok(true) => {
            if wallets.is_empty() {
                let address = crate::crypto::derive_address(&seed, 0);
                match db.call_blocking(move |d| {
                    d.insert_wallet(0, crate::types::Role::Master, &address, None)
                }) {
                    Ok(master) => wallets.push(master),
                    Err(e) => {
                        return UnlockResult::WalletRead(format!(
                            "initializing recovered master wallet: {e}"
                        ));
                    }
                }
            }
            db.call_blocking(|d| {
                let _ = d.audit(AuditEvent::VaultUnlocked, &serde_json::json!({}));
            });
            UnlockResult::Unlocked { seed, wallets }
        }
        Ok(false) => UnlockResult::AuditChainFailed,
        Err(e) => UnlockResult::AuditChainRead(e.to_string()),
    }
}

fn finish_setup_blocking(
    db: Storage,
    vault_path: std::path::PathBuf,
    config_dir: std::path::PathBuf,
    current_profile: Option<String>,
    creating: bool,
    phrase: zeroize::Zeroizing<String>,
    passphrase: zeroize::Zeroizing<String>,
) -> SetupResult {
    let mnemonic = match crate::crypto::parse_mnemonic(&phrase) {
        Ok(m) => m,
        Err(e) => return SetupResult::Failed(format!("invalid phrase: {e}")),
    };
    drop(phrase);
    let seed = crate::crypto::seed_from_mnemonic(&mnemonic);
    match wallet_consistency(&db, &seed) {
        Ok(_) => {}
        Err(WalletCheckError::Mismatch) => {
            db.call_blocking(|d| {
                let _ = d.audit(AuditEvent::IntegrityCheckFailed, &serde_json::json!({}));
            });
            return SetupResult::Failed(
                "Existing wallet records don't match this recovery phrase. Refusing to proceed."
                    .to_string(),
            );
        }
        Err(WalletCheckError::Read(e)) => {
            return SetupResult::Failed(format!(
                "Wallet metadata couldn't be read: {e}. Refusing to proceed."
            ));
        }
    }

    let profile_meta = if let Some(id) = current_profile {
        let profiles = match crate::profiles::load(&config_dir) {
            Ok(profiles) => profiles,
            Err(e) => return SetupResult::Failed(format!("Couldn't load profiles: {e}")),
        };
        Some(
            profiles
                .iter()
                .find(|profile| profile.id == id)
                .cloned()
                .unwrap_or_else(|| crate::profiles::ProfileMeta {
                    id,
                    name: next_wallet_name(&profiles),
                    created_at: crate::db::now_ms(),
                }),
        )
    } else {
        None
    };

    let vault_key = if crate::vault::vault_exists(&vault_path) {
        match crate::vault::unlock_vault_keyed(&vault_path, &passphrase) {
            Ok((existing, key)) if existing == mnemonic => key,
            Ok(_) => {
                return SetupResult::Failed(
                    "This recovery phrase doesn't match the existing wallet".to_string(),
                );
            }
            Err(e) => {
                return SetupResult::Failed(format!("Couldn't reopen the existing wallet: {e}"));
            }
        }
    } else {
        match crate::vault::create_vault(&vault_path, &mnemonic, &passphrase) {
            Ok(k) => k,
            Err(e) => return SetupResult::Failed(format!("Couldn't create wallet: {e}")),
        }
    };
    drop(passphrase);
    drop(mnemonic);

    let master_ok = {
        let master_addr = crate::crypto::derive_address(&seed, 0);
        db.call_blocking(move |d| {
            let key_ok = d.unlock_audit_key(vault_key.as_bytes()).is_ok();
            let _ = d.audit(AuditEvent::VaultCreated, &serde_json::json!({}));
            key_ok
                && match d.master_exists() {
                    Ok(true) => true,
                    Ok(false) => d
                        .insert_wallet(0, crate::types::Role::Master, &master_addr, None)
                        .is_ok(),
                    Err(_) => false,
                }
        })
    };
    if !master_ok {
        return SetupResult::Failed(
            "Couldn't initialize the master wallet — please retry".to_string(),
        );
    }

    if let Some(meta) = profile_meta
        && let Err(e) = crate::profiles::register(&config_dir, meta)
    {
        return SetupResult::Failed(format!("Couldn't register profile: {e}"));
    }

    let wallets = match db.call_blocking(|d| d.list_wallets()) {
        Ok(wallets) => wallets,
        Err(e) => return SetupResult::Failed(format!("Couldn't load wallets: {e}")),
    };
    let profiles = match crate::profiles::load(&config_dir) {
        Ok(profiles) => profiles,
        Err(e) => return SetupResult::Failed(format!("Couldn't load profiles: {e}")),
    };
    let _ = creating;
    SetupResult::Finished {
        seed,
        wallets,
        profiles,
    }
}

fn persist_signed_send_blocking(
    db: Storage,
    pending: crate::app::PendingSend,
    from: crate::types::WalletRow,
    wire: Vec<u8>,
    sig_b58: String,
) -> SendPersistResult {
    let from_id = from.id;
    let to = pending.to.clone();
    let lamports = pending.lamports;
    let created = db.call_blocking(move |d| d.create_intent(from_id, &to, lamports, None));
    let intent = match created {
        Ok(intent) => intent,
        Err(crate::db::CreateIntentError::WalletHasOpenIntent) => {
            return SendPersistResult::Failed(
                "This wallet already has a transfer in progress".to_string(),
            );
        }
        Err(e) => return SendPersistResult::Failed(format!("Couldn't record transfer: {e}")),
    };
    let intent_id = intent.id;
    let blockhash = pending.blockhash.clone();
    let lvbh = pending.lvbh;
    let fee = pending.fee;
    let signed =
        db.call_blocking(move |d| d.mark_signed(intent_id, &sig_b58, &blockhash, lvbh, fee, &wire));
    match signed {
        Ok(IntentTransitionOutcome::Applied) => SendPersistResult::Signed {
            intent_id: intent.id,
        },
        Ok(IntentTransitionOutcome::NotFound) => {
            SendPersistResult::Failed("Transfer record vanished before signing".to_string())
        }
        Ok(IntentTransitionOutcome::WrongState(status)) => {
            SendPersistResult::Failed(format!("Transfer was already {}", status.as_str()))
        }
        Err(e) => {
            let cleaned = db.call_blocking(move |d| {
                d.mark_terminal(
                    intent_id,
                    TerminalStatus::Failed,
                    Some("couldn't persist signed transfer"),
                )
            });
            match cleaned {
                Ok(_) => {
                    SendPersistResult::Failed(format!("Couldn't persist signed transfer: {e}"))
                }
                Err(_) => SendPersistResult::Failed(format!(
                    "Couldn't persist signed transfer, and couldn't clean up the pending \
                     record: {e} — restart silo to reconcile before sending from this wallet again"
                )),
            }
        }
    }
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
        Command::UnlockVault {
            vault_path,
            passphrase,
        } => {
            let db = db.clone();
            let result = tokio::task::spawn_blocking(move || {
                unlock_vault_blocking(db, vault_path, passphrase)
            })
            .await
            .unwrap_or_else(|e| UnlockResult::AuditChainRead(e.to_string()));
            let _ = evt
                .send(AppEvent::UnlockComplete {
                    result,
                    generation: cmd_gen,
                })
                .await;
        }

        Command::FinishSetup {
            vault_path,
            config_dir,
            current_profile,
            creating,
            phrase,
            passphrase,
        } => {
            let db = db.clone();
            let result = tokio::task::spawn_blocking(move || {
                finish_setup_blocking(
                    db,
                    vault_path,
                    config_dir,
                    current_profile,
                    creating,
                    phrase,
                    passphrase,
                )
            })
            .await
            .unwrap_or_else(|e| SetupResult::Failed(format!("setup task failed: {e}")));
            let _ = evt
                .send(AppEvent::SetupComplete {
                    result,
                    generation: cmd_gen,
                })
                .await;
        }

        Command::PersistSignedSend {
            pending,
            from,
            wire,
            sig_b58,
        } => {
            let db = db.clone();
            let result = tokio::task::spawn_blocking(move || {
                persist_signed_send_blocking(db, pending, from, wire, sig_b58)
            })
            .await
            .unwrap_or_else(|e| {
                SendPersistResult::Failed(format!("send persistence task failed: {e}"))
            });
            let _ = evt
                .send(AppEvent::SendPersisted {
                    result,
                    generation: cmd_gen,
                })
                .await;
        }

        Command::DeleteProfile { config_dir, id } => {
            let result = tokio::task::spawn_blocking(move || {
                crate::profiles::remove(&config_dir, &id)
                    .and_then(|_| crate::profiles::load(&config_dir))
                    .map(|profiles| ProfileDeleteResult::Deleted { profiles })
                    .unwrap_or_else(|e| {
                        ProfileDeleteResult::Failed(format!("Couldn't delete profile: {e}"))
                    })
            })
            .await
            .unwrap_or_else(|e| {
                ProfileDeleteResult::Failed(format!("delete profile task failed: {e}"))
            });
            let _ = evt
                .send(AppEvent::ProfileDeleted {
                    result,
                    generation: cmd_gen,
                })
                .await;
        }

        Command::OpenProfile { config_dir, id } => {
            let result = tokio::task::spawn_blocking(move || {
                let path = crate::profiles::db_path(&config_dir, &id).map_err(|e| e.to_string())?;
                let opened = crate::db::Db::open(&path).map_err(|e| e.to_string())?;
                crate::app::App::validate_profile_scoped_state(&opened)
                    .map_err(|e| e.to_string())?;
                Ok(ProfileOpenedPayload {
                    db: opened,
                    id,
                    created: false,
                })
            })
            .await
            .unwrap_or_else(|e| Err(format!("open profile task failed: {e}")));
            let _ = evt
                .send(AppEvent::ProfileOpened {
                    result,
                    generation: cmd_gen,
                })
                .await;
        }

        Command::CreateProfile { config_dir, id } => {
            let result = tokio::task::spawn_blocking(move || {
                let dir = crate::profiles::dir_for(&config_dir, &id).map_err(|e| e.to_string())?;
                crate::profiles::ensure_private_dir(&dir).map_err(|e| e.to_string())?;
                let opened =
                    crate::db::Db::open(&dir.join("silo.db")).map_err(|e| e.to_string())?;
                Ok(ProfileOpenedPayload {
                    db: opened,
                    id,
                    created: true,
                })
            })
            .await
            .unwrap_or_else(|e| Err(format!("create profile task failed: {e}")));
            let _ = evt
                .send(AppEvent::ProfileOpened {
                    result,
                    generation: cmd_gen,
                })
                .await;
        }

        Command::ClipboardCopy {
            text,
            ok_label,
            arm_hot_refresh,
        } => {
            let result = tokio::task::spawn_blocking(move || ClipboardCopyResult {
                outcome: crate::clipboard::ClipboardManager::new()
                    .copy(&text)
                    .map_err(|e| e.to_string()),
                ok_label,
                arm_hot_refresh,
            })
            .await
            .unwrap_or_else(|e| ClipboardCopyResult {
                outcome: Err(e.to_string()),
                ok_label: "Copied".to_string(),
                arm_hot_refresh: false,
            });
            let _ = evt
                .send(AppEvent::ClipboardCopied {
                    result,
                    generation: cmd_gen,
                })
                .await;
        }

        Command::ClipboardPaste { target } => {
            let result = tokio::task::spawn_blocking(move || {
                crate::clipboard::ClipboardManager::new()
                    .paste()
                    .map_err(|e| e.to_string())
            })
            .await
            .unwrap_or_else(|e| Err(e.to_string()));
            let _ = evt
                .send(AppEvent::ClipboardPasted {
                    target,
                    result,
                    generation: cmd_gen,
                })
                .await;
        }

        Command::ArchiveWallet { id, want } => {
            let outcome = db
                .call_current(generation.clone(), cmd_gen, move |d| -> anyhow::Result<_> {
                    d.set_archived(id, want)?;
                    d.list_wallets()
                })
                .await;
            if let Some(res) = outcome {
                let _ = evt
                    .send(AppEvent::WalletArchived {
                        id,
                        want,
                        result: res.map_err(|e| e.to_string()),
                        generation: cmd_gen,
                    })
                    .await;
            }
        }

        Command::DeriveSubwallet { seed } => {
            let outcome = db
                .call_current(generation.clone(), cmd_gen, move |d| -> anyhow::Result<_> {
                    let idx = d.next_account_index().unwrap_or(1).max(1);
                    let addr = crate::crypto::derive_address(&seed, idx);
                    d.insert_wallet(idx, crate::types::Role::Sub, &addr, None)?;
                    Ok((idx, d.list_wallets()?))
                })
                .await;
            if let Some(res) = outcome {
                let _ = evt
                    .send(AppEvent::SubwalletDerived {
                        result: res.map_err(|e| e.to_string()),
                        generation: cmd_gen,
                    })
                    .await;
            }
        }

        Command::PersistSetting { change } => {
            let (key, value, details) = match change {
                SettingChange::Currency(c) => (
                    "currency",
                    c.code().to_string(),
                    json!({ "currency": c.code() }),
                ),
                SettingChange::Priority(p) => (
                    "priority_fee_micro",
                    p.to_string(),
                    json!({ "priority_fee_micro": p }),
                ),
                SettingChange::AutoLock(m) => (
                    "auto_lock_minutes",
                    m.to_string(),
                    json!({ "auto_lock_minutes": m }),
                ),
            };
            let outcome = db
                .call_current(generation.clone(), cmd_gen, move |d| {
                    d.set_meta_audited(key, &value, AuditEvent::SettingsChanged, &details)
                })
                .await;
            if let Some(res) = outcome {
                let _ = evt
                    .send(AppEvent::SettingPersisted {
                        change,
                        result: res.map_err(|e| e.to_string()),
                        generation: cmd_gen,
                    })
                    .await;
            }
        }

        Command::SetWalletText { id, field, value } => {
            let outcome = db
                .call_current(generation.clone(), cmd_gen, move |d| -> anyhow::Result<_> {
                    match field {
                        WalletTextField::Label => d.set_label(id, value.as_deref())?,
                        WalletTextField::Note => d.set_note(id, value.as_deref())?,
                    }
                    d.list_wallets()
                })
                .await;
            if let Some(res) = outcome {
                let _ = evt
                    .send(AppEvent::WalletTextSet {
                        field,
                        result: res.map_err(|e| e.to_string()),
                        generation: cmd_gen,
                    })
                    .await;
            }
        }

        Command::SetIntentNote {
            wallet_id,
            id,
            value,
        } => {
            let outcome = db
                .call_current(generation.clone(), cmd_gen, move |d| -> anyhow::Result<_> {
                    d.set_intent_note(id, value.as_deref())?;
                    d.list_intents_for_wallet(wallet_id, 50)
                })
                .await;
            if let Some(res) = outcome {
                let _ = evt
                    .send(AppEvent::IntentNoteSet {
                        result: res.map_err(|e| e.to_string()),
                        generation: cmd_gen,
                    })
                    .await;
            }
        }

        Command::RenameProfile {
            config_dir,
            id,
            name,
        } => {
            let result = tokio::task::spawn_blocking(move || {
                crate::profiles::rename(&config_dir, &id, &name)
                    .and_then(|_| crate::profiles::load(&config_dir))
                    .map_err(|e| format!("Couldn't rename profile: {e}"))
            })
            .await
            .unwrap_or_else(|e| Err(format!("rename profile task failed: {e}")));
            let _ = evt
                .send(AppEvent::ProfileRenamed {
                    result,
                    generation: cmd_gen,
                })
                .await;
        }

        Command::Reconcile => match reconcile_boot(&db, &rpc_now, &generation, cmd_gen).await {
            Ok(outcome) if outcome.deferred == 0 => {
                let _ = evt
                    .send(AppEvent::ReconcileComplete {
                        resolved: outcome.resolved,
                        generation: cmd_gen,
                    })
                    .await;
            }
            Ok(_) | Err(_) => {
                let _ = evt
                    .send(AppEvent::ReconcileFailedOffline {
                        generation: cmd_gen,
                    })
                    .await;
            }
        },

        Command::FetchRentExempt => match rpc_now.get_min_balance_for_rent_exemption(0).await {
            Ok(v) => {
                let _ = db
                    .call_current(generation.clone(), cmd_gen, move |d| {
                        let _ = d.set_meta("rent_exempt_min_0", &v.to_string());
                    })
                    .await;
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
            let currency = match db.call(|d| current_currency(d)).await {
                Ok(currency) => currency,
                Err(e) => {
                    send_error(
                        &evt,
                        cmd_gen,
                        format!("price currency lookup failed: {e:#}"),
                    )
                    .await;
                    return;
                }
            };
            match fetch_price(&client, currency).await {
                Ok(p) => {
                    if let Err(e) = publish_price(&db, price, &evt, generation, cmd_gen, p).await {
                        send_error(&evt, cmd_gen, format!("price publication failed: {e:#}")).await;
                    }
                }
                Err(e) => {
                    send_error(&evt, cmd_gen, format!("price fetch failed: {e}")).await;
                }
            }
        }

        Command::CheckForUpdate => {
            if let Ok(latest) = fetch_latest_release(&client).await {
                let now = unix_now_secs();
                let seen = latest.clone();
                db.call(move |d| {
                    let _ = d.set_meta("update_last_check", &now.to_string());
                    let _ = d.set_meta("update_latest_seen", &seen);
                })
                .await;
                let _ = evt.send(AppEvent::UpdateStatus { latest }).await;
            }
        }

        Command::RefreshBalances { include_archived } => {
            let wallets: Vec<(i64, String)> = match db.call(|d| d.list_wallets()).await {
                Ok(ws) => ws
                    .into_iter()
                    .filter(|w| include_archived || !w.archived)
                    .map(|w| (w.id, w.pubkey))
                    .collect(),
                Err(e) => {
                    let _ = evt
                        .send(AppEvent::BalancesFailed {
                            reason: format!("Couldn't load wallets: {e}"),
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
                    send_error(&evt, cmd_gen, format!("Couldn't fetch blockhash: {e}")).await;
                    return;
                }
            };
            let dest_balance = match rpc_now.get_balance(&to).await {
                Ok(b) => b,
                Err(e) => {
                    send_error(
                        &evt,
                        cmd_gen,
                        format!("Couldn't fetch recipient balance: {e}"),
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

        Command::Broadcast { .. } => unreachable!("Broadcast is an ordered command"),

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
            let url_for_db = url.clone();
            let wrote = db
                .call_current(generation.clone(), cmd_gen, move |d| {
                    d.set_meta_audited(
                        "rpc_url",
                        &url_for_db,
                        AuditEvent::RpcChanged,
                        &json!({ "url": redacted }),
                    )
                })
                .await;
            match wrote {
                Some(Ok(())) => {}
                Some(Err(e)) => {
                    send_error(&evt, cmd_gen, format!("Couldn't save RPC URL: {e}")).await;
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
                Ok(outcome) if outcome.deferred == 0 => {
                    let _ = evt
                        .send(AppEvent::ReconcileComplete {
                            resolved: outcome.resolved,
                            generation: cmd_gen,
                        })
                        .await;
                }
                Ok(_) | Err(_) => {
                    let _ = evt
                        .send(AppEvent::ReconcileFailedOffline {
                            generation: cmd_gen,
                        })
                        .await;
                }
            }
        }

        Command::LoadWallets => match db.call(|d| d.list_wallets()).await {
            Ok(wallets) => {
                let _ = evt
                    .send(AppEvent::WalletsLoaded {
                        wallets,
                        generation: cmd_gen,
                    })
                    .await;
            }
            Err(e) => send_error(&evt, cmd_gen, format!("Couldn't load wallets: {e}")).await,
        },

        Command::LoadDetail { wallet_id } => {
            let loaded = db
                .call(move |d| {
                    let intents = d.list_intents_for_wallet(wallet_id, 50)?;
                    let wallets = d.list_wallets()?;
                    Ok::<_, anyhow::Error>((intents, wallets))
                })
                .await;
            match loaded {
                Ok((intents, wallets)) => {
                    let _ = evt
                        .send(AppEvent::DetailLoaded {
                            intents,
                            wallets,
                            generation: cmd_gen,
                        })
                        .await;
                }
                Err(e) => {
                    send_error(
                        &evt,
                        cmd_gen,
                        format!("Couldn't load transfer history: {e}"),
                    )
                    .await
                }
            }
        }

        Command::LoadAudit => match db.call(|d| d.list_audit(200)).await {
            Ok(audit) => {
                let _ = evt
                    .send(AppEvent::AuditLoaded {
                        audit,
                        generation: cmd_gen,
                    })
                    .await;
            }
            Err(e) => send_error(&evt, cmd_gen, format!("Couldn't load audit log: {e}")).await,
        },
    }
}

#[allow(clippy::too_many_arguments)]
async fn finalize(
    db: &Storage,
    evt: &mpsc::Sender<AppEvent>,
    intent_id: i64,
    sig: &str,
    status: TerminalStatus,
    error: Option<&str>,
    generation: &Arc<AtomicU64>,
    cmd_gen: u64,
) {
    let error_owned = error.map(|s| s.to_string());
    let Some(outcome) = db
        .call_current(generation.clone(), cmd_gen, move |d| {
            d.mark_terminal(intent_id, status, error_owned.as_deref())
        })
        .await
    else {
        return;
    };
    let final_status = match outcome {
        Ok(IntentTransitionOutcome::Applied) => status.to_intent_status(),
        Err(e) => {
            send_error(
                evt,
                cmd_gen,
                format!(
                    "transfer {} on-chain but couldn't be recorded locally — will reconcile on restart: {e}",
                    status.as_str()
                ),
            )
            .await;
            status.to_intent_status()
        }
        Ok(_) => match db
            .call_current(generation.clone(), cmd_gen, move |d| {
                d.get_intent(intent_id).ok().flatten().map(|i| i.status)
            })
            .await
        {
            Some(Some(s)) => s,
            Some(None) => status.to_intent_status(),
            None => return,
        },
    };
    let (outcome, transfer) = match final_status {
        IntentStatus::Confirmed => {
            let transfer = db
                .call_current(generation.clone(), cmd_gen, move |d| {
                    d.get_intent(intent_id)
                        .ok()
                        .flatten()
                        .map(|i| OptimisticTransfer {
                            from_wallet: i.from_wallet,
                            to_address: i.to_address,
                            lamports: i.lamports,
                            fee_lamports: i.fee_lamports,
                        })
                })
                .await
                .flatten();
            (
                TransferOutcome::Confirmed {
                    signature: sig.to_string(),
                },
                transfer,
            )
        }
        IntentStatus::Failed => (
            TransferOutcome::Failed {
                reason: error.unwrap_or("failed").to_string(),
            },
            None,
        ),
        IntentStatus::Expired => (TransferOutcome::Expired, None),
        _ => return,
    };
    let _ = evt
        .send(AppEvent::TransferResult {
            intent_id,
            outcome,
            transfer,
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

struct PollContext {
    intent_id: i64,
    sig: String,
    bytes: Vec<u8>,
    lvbh: u64,
    last_rebroadcast: Option<Instant>,
}

async fn broadcast_submit(
    intent_id: i64,
    db: &Storage,
    rpc_arc: &Arc<Mutex<Rpc>>,
    evt: &mpsc::Sender<AppEvent>,
    generation: &Arc<AtomicU64>,
    cmd_gen: u64,
) -> Option<PollContext> {
    let intent = match db
        .call_current(generation.clone(), cmd_gen, move |d| {
            d.get_intent(intent_id).ok().flatten()
        })
        .await
    {
        None => return None,
        Some(intent) => intent,
    };
    let Some(intent) = intent else {
        send_error(evt, cmd_gen, "transfer record vanished").await;
        return None;
    };
    let (Some(bytes), Some(sig)) = (intent.signed_tx, intent.signature) else {
        send_error(evt, cmd_gen, "transfer was not signed").await;
        return None;
    };
    let lvbh = intent.last_valid_block_height.unwrap_or(0);

    match db
        .call_current(generation.clone(), cmd_gen, move |d| {
            d.mark_submitted(intent_id)
        })
        .await
    {
        Some(Ok(IntentTransitionOutcome::Applied)) => {}
        Some(Ok(IntentTransitionOutcome::WrongState(_) | IntentTransitionOutcome::NotFound)) => {
            send_error(
                evt,
                cmd_gen,
                "transfer was not in signed state; not broadcasting",
            )
            .await;
            return None;
        }
        Some(Err(e)) => {
            send_error(
                evt,
                cmd_gen,
                format!("Couldn't record submitted transfer: {e}"),
            )
            .await;
            return None;
        }
        None => return None,
    }

    let mut last_rebroadcast = None;
    let rpc = { rpc_arc.lock_recover().clone() };
    match rpc.send_transaction(&bytes).await {
        Ok(returned) if returned != sig => {
            let sig_for_audit = sig.clone();
            let _ = db
                .call_current(generation.clone(), cmd_gen, move |d| {
                    let _ = d.audit(
                        AuditEvent::IntegrityCheckFailed,
                        &json!({"intent": intent_id, "expected": sig_for_audit, "got": returned}),
                    );
                })
                .await;
            finalize(
                db,
                evt,
                intent_id,
                &sig,
                TerminalStatus::Failed,
                Some("rpc returned mismatched signature"),
                generation,
                cmd_gen,
            )
            .await;
            return None;
        }
        Ok(_) => {
            last_rebroadcast = Some(Instant::now());
            let _ = evt
                .send(AppEvent::TransferResult {
                    intent_id,
                    outcome: TransferOutcome::Submitted {
                        signature: sig.clone(),
                    },
                    transfer: None,
                    generation: cmd_gen,
                })
                .await;
        }
        Err(e) => {
            if let Some(reason) = definitive_rejection_reason(&e) {
                finalize(
                    db,
                    evt,
                    intent_id,
                    &sig,
                    TerminalStatus::Failed,
                    Some(&reason),
                    generation,
                    cmd_gen,
                )
                .await;
                return None;
            }
            send_error(
                evt,
                cmd_gen,
                format!("broadcast uncertain — polling signed transfer: {e}"),
            )
            .await;
        }
    }

    Some(PollContext {
        intent_id,
        sig,
        bytes,
        lvbh,
        last_rebroadcast,
    })
}

async fn poll_confirmation(
    ctx: PollContext,
    db: Storage,
    rpc_arc: Arc<Mutex<Rpc>>,
    evt: mpsc::Sender<AppEvent>,
    generation: Arc<AtomicU64>,
    cmd_gen: u64,
) {
    let current = || generation.load(Ordering::SeqCst) == cmd_gen;
    let PollContext {
        intent_id,
        sig,
        bytes,
        lvbh,
        mut last_rebroadcast,
    } = ctx;

    let mut reported_pending = false;
    let mut rounds = 0;
    while current() && rounds < CONFIRM_MAX_ROUNDS {
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
            match decide(status.as_ref(), height, lvbh) {
                Decision::Fail => {
                    finalize(
                        &db,
                        &evt,
                        intent_id,
                        &sig,
                        TerminalStatus::Failed,
                        Some(ON_CHAIN_ERROR),
                        &generation,
                        cmd_gen,
                    )
                    .await;
                    return;
                }
                Decision::FinalizeSuccess => {
                    finalize(
                        &db,
                        &evt,
                        intent_id,
                        &sig,
                        TerminalStatus::Confirmed,
                        None,
                        &generation,
                        cmd_gen,
                    )
                    .await;
                    return;
                }
                Decision::Rebroadcast => {
                    if let Err(returned) =
                        rebroadcast_if_due(&rpc, &bytes, &sig, lvbh, &mut last_rebroadcast).await
                    {
                        let sig_for_audit = sig.clone();
                        let _ = db
                            .call_current(generation.clone(), cmd_gen, move |d| {
                                let _ = d.audit(
                                    AuditEvent::IntegrityCheckFailed,
                                    &json!({"intent": intent_id, "expected": sig_for_audit, "got": returned}),
                                );
                            })
                            .await;
                        finalize(
                            &db,
                            &evt,
                            intent_id,
                            &sig,
                            TerminalStatus::Failed,
                            Some("rpc returned mismatched signature"),
                            &generation,
                            cmd_gen,
                        )
                        .await;
                        return;
                    }
                }
                Decision::WaitFinality => {}
                Decision::Expire => match recheck_outcome(sig_status(&rpc, &sig).await.as_ref()) {
                    RecheckOutcome::Fail => {
                        finalize(
                            &db,
                            &evt,
                            intent_id,
                            &sig,
                            TerminalStatus::Failed,
                            Some(ON_CHAIN_ERROR),
                            &generation,
                            cmd_gen,
                        )
                        .await;
                        return;
                    }
                    RecheckOutcome::Confirmed => {
                        finalize(
                            &db,
                            &evt,
                            intent_id,
                            &sig,
                            TerminalStatus::Confirmed,
                            None,
                            &generation,
                            cmd_gen,
                        )
                        .await;
                        return;
                    }
                    RecheckOutcome::KeepOpen => continue,
                    RecheckOutcome::Expire => {
                        finalize(
                            &db,
                            &evt,
                            intent_id,
                            &sig,
                            TerminalStatus::Expired,
                            Some(BLOCKHASH_EXPIRED),
                            &generation,
                            cmd_gen,
                        )
                        .await;
                        return;
                    }
                },
            }
        }

        if !current() {
            return;
        }
        rounds += 1;
        if !reported_pending {
            let _ = evt
                .send(AppEvent::TransferResult {
                    intent_id,
                    outcome: TransferOutcome::StillPending {
                        signature: sig.clone(),
                    },
                    transfer: None,
                    generation: cmd_gen,
                })
                .await;
            reported_pending = true;
        }
    }

    if current() {
        let rpc = { rpc_arc.lock_recover().clone() };
        match sig_status(&rpc, &sig).await {
            Some(s) if s.is_error() => {
                finalize(
                    &db,
                    &evt,
                    intent_id,
                    &sig,
                    TerminalStatus::Failed,
                    Some("on-chain error"),
                    &generation,
                    cmd_gen,
                )
                .await;
            }
            Some(s) if s.is_finalized() => {
                finalize(
                    &db,
                    &evt,
                    intent_id,
                    &sig,
                    TerminalStatus::Confirmed,
                    None,
                    &generation,
                    cmd_gen,
                )
                .await;
            }
            Some(s) if s.is_confirmed() => {}
            _ => {
                finalize(
                    &db,
                    &evt,
                    intent_id,
                    &sig,
                    TerminalStatus::Expired,
                    Some("confirmation timed out before the network confirmed or rejected it"),
                    &generation,
                    cmd_gen,
                )
                .await;
            }
        }
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
    if let Some(ctx) = broadcast_submit(intent_id, &db, &rpc_arc, &evt, &generation, cmd_gen).await
    {
        poll_confirmation(ctx, db, rpc_arc, evt, generation, cmd_gen).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn status(err: bool, confirmation_status: Option<&str>) -> crate::solana::rpc::SignatureStatus {
        crate::solana::rpc::SignatureStatus {
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
        assert!(Command::ArchiveWallet { id: 1, want: true }.ordered());
        assert!(Command::DeriveSubwallet { seed: test_seed() }.ordered());
        assert!(
            Command::PersistSetting {
                change: SettingChange::AutoLock(5),
            }
            .ordered()
        );
        assert!(
            Command::SetWalletText {
                id: 1,
                field: WalletTextField::Label,
                value: None,
            }
            .ordered()
        );
        assert!(
            Command::SetIntentNote {
                wallet_id: 1,
                id: 1,
                value: None,
            }
            .ordered()
        );
        assert!(
            Command::RenameProfile {
                config_dir: std::path::PathBuf::from("/tmp"),
                id: "p".into(),
                name: "n".into(),
            }
            .ordered()
        );
        assert!(
            Command::OpenProfile {
                config_dir: std::path::PathBuf::from("/tmp"),
                id: "p".into(),
            }
            .ordered()
        );
        assert!(
            Command::CreateProfile {
                config_dir: std::path::PathBuf::from("/tmp"),
                id: "p".into(),
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

    const TEST_MNEMONIC: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    #[test]
    fn recovered_vault_initializes_master_only_after_unlock() {
        for audit_created in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let id = "0000000000000018";
            let profile = crate::profiles::dir_for(dir.path(), id).unwrap();
            crate::profiles::ensure_private_dir(&profile).unwrap();
            crate::profiles::save(dir.path(), &[]).unwrap();
            let vault_path = profile.join("vault.json");
            let mnemonic = crate::crypto::parse_mnemonic(TEST_MNEMONIC).unwrap();
            let key =
                crate::vault::create_vault(&vault_path, &mnemonic, "test passphrase").unwrap();
            assert_eq!(crate::profiles::load(dir.path()).unwrap()[0].id, id);
            let db = Storage::new(crate::db::Db::open(&profile.join("silo.db")).unwrap());
            if audit_created {
                db.call_blocking(move |d| {
                    d.unlock_audit_key(key.as_bytes()).unwrap();
                    d.audit(AuditEvent::VaultCreated, &json!({})).unwrap();
                    d.lock_audit_key();
                });
            }

            assert!(matches!(
                unlock_vault_blocking(
                    db.clone(),
                    vault_path.clone(),
                    zeroize::Zeroizing::new("wrong".into())
                ),
                UnlockResult::WrongPassphrase
            ));
            assert!(db.call_blocking(|d| d.list_wallets()).unwrap().is_empty());
            for _ in 0..2 {
                let result = unlock_vault_blocking(
                    db.clone(),
                    vault_path.clone(),
                    zeroize::Zeroizing::new("test passphrase".into()),
                );
                let UnlockResult::Unlocked { seed, wallets } = result else {
                    panic!("recovered vault did not unlock");
                };
                assert_eq!(wallets.len(), 1);
                assert_eq!(wallets[0].account_index, 0);
                assert_eq!(wallets[0].role, crate::types::Role::Master);
                assert_eq!(wallets[0].pubkey, crate::crypto::derive_address(&seed, 0));
                assert!(db.call_blocking(|d| d.verify_audit_chain()).unwrap());
                db.call_blocking(|d| d.lock_audit_key());
            }
        }
    }

    #[test]
    fn recovered_vault_rejects_tampered_audit_before_master_creation() {
        let dir = tempfile::tempdir().unwrap();
        let vault_path = dir.path().join("vault.json");
        let db_path = dir.path().join("silo.db");
        let mnemonic = crate::crypto::parse_mnemonic(TEST_MNEMONIC).unwrap();
        let key = crate::vault::create_vault(&vault_path, &mnemonic, "test passphrase").unwrap();
        let mut database = crate::db::Db::open(&db_path).unwrap();
        database.unlock_audit_key(key.as_bytes()).unwrap();
        database
            .audit(AuditEvent::VaultCreated, &json!({}))
            .unwrap();
        database.lock_audit_key();
        let db = Storage::new(database);
        rusqlite::Connection::open(&db_path)
            .unwrap()
            .execute("UPDATE audit_log SET details='tampered'", [])
            .unwrap();

        let result = unlock_vault_blocking(
            db.clone(),
            vault_path,
            zeroize::Zeroizing::new("test passphrase".into()),
        );
        assert!(matches!(result, UnlockResult::AuditChainFailed));
        assert!(db.call_blocking(|d| d.list_wallets()).unwrap().is_empty());
    }

    #[test]
    fn setup_preserves_profile_metadata_on_creation_and_retry() {
        for registered in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let id = "0000000000000017";
            let profile = crate::profiles::dir_for(dir.path(), id).unwrap();
            crate::profiles::ensure_private_dir(&profile).unwrap();
            let initial = if registered {
                vec![crate::profiles::ProfileMeta {
                    id: id.into(),
                    name: "Treasury".into(),
                    created_at: 42,
                }]
            } else {
                Vec::new()
            };
            crate::profiles::save(dir.path(), &initial).unwrap();
            let db = Storage::new(crate::db::Db::open(&profile.join("silo.db")).unwrap());
            let mut first_created_at = None;
            for _ in 0..2 {
                let result = finish_setup_blocking(
                    db.clone(),
                    profile.join("vault.json"),
                    dir.path().to_path_buf(),
                    Some(id.into()),
                    true,
                    zeroize::Zeroizing::new(TEST_MNEMONIC.into()),
                    zeroize::Zeroizing::new("test passphrase".into()),
                );
                let SetupResult::Finished { profiles, .. } = result else {
                    panic!("setup failed");
                };
                assert_eq!(profiles.len(), 1);
                assert_eq!(
                    profiles[0].name,
                    if registered { "Treasury" } else { "Wallet 1" }
                );
                if registered {
                    assert_eq!(profiles[0].created_at, 42);
                }
                assert_eq!(
                    *first_created_at.get_or_insert(profiles[0].created_at),
                    profiles[0].created_at
                );
            }
        }
    }

    #[test]
    fn setup_existing_vault_accepts_matching_phrase_with_empty_wallets() {
        for phrase in [
            TEST_MNEMONIC.to_string(),
            format!(" \t{}\n", TEST_MNEMONIC.replace(' ', "  \t")),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let vault_path = dir.path().join("vault.json");
            let mnemonic = crate::crypto::parse_mnemonic(TEST_MNEMONIC).unwrap();
            crate::vault::create_vault(&vault_path, &mnemonic, "test passphrase").unwrap();
            let original_vault = std::fs::read(&vault_path).unwrap();
            let db = Storage::new(crate::db::Db::open(&dir.path().join("silo.db")).unwrap());
            assert!(db.call_blocking(|d| d.list_wallets()).unwrap().is_empty());

            let result = finish_setup_blocking(
                db.clone(),
                vault_path.clone(),
                dir.path().to_path_buf(),
                None,
                false,
                zeroize::Zeroizing::new(phrase),
                zeroize::Zeroizing::new("test passphrase".into()),
            );
            let SetupResult::Finished { seed, wallets, .. } = result else {
                panic!("matching existing vault did not finish setup");
            };
            let expected_address = crate::crypto::derive_address(&test_seed(), 0);
            assert_eq!(crate::crypto::derive_address(&seed, 0), expected_address);
            assert_eq!(wallets.len(), 1);
            assert_eq!(wallets[0].account_index, 0);
            assert_eq!(wallets[0].role, crate::types::Role::Master);
            assert_eq!(wallets[0].pubkey, expected_address);
            let stored = db.call_blocking(|d| d.list_wallets()).unwrap();
            assert_eq!(stored.len(), 1);
            assert_eq!(stored[0].pubkey, expected_address);
            assert_eq!(std::fs::read(&vault_path).unwrap(), original_vault);
        }
    }

    #[test]
    fn setup_existing_vault_rejects_different_phrase_with_empty_wallets() {
        let dir = tempfile::tempdir().unwrap();
        let vault_path = dir.path().join("vault.json");
        let mnemonic = crate::crypto::parse_mnemonic(TEST_MNEMONIC).unwrap();
        let different = bip39::Mnemonic::from_entropy(&[1u8; 16]).unwrap();
        assert_ne!(mnemonic, different);
        crate::vault::create_vault(&vault_path, &mnemonic, "test passphrase").unwrap();
        let original_vault = std::fs::read(&vault_path).unwrap();
        let db = Storage::new(crate::db::Db::open(&dir.path().join("silo.db")).unwrap());
        assert!(db.call_blocking(|d| d.list_wallets()).unwrap().is_empty());

        let result = finish_setup_blocking(
            db.clone(),
            vault_path.clone(),
            dir.path().to_path_buf(),
            None,
            false,
            zeroize::Zeroizing::new(different.to_string()),
            zeroize::Zeroizing::new("test passphrase".into()),
        );
        let SetupResult::Failed(message) = result else {
            panic!("different phrase was accepted for an existing vault");
        };
        assert_eq!(
            message,
            "This recovery phrase doesn't match the existing wallet"
        );
        assert!(db.call_blocking(|d| d.list_wallets()).unwrap().is_empty());
        assert_eq!(std::fs::read(&vault_path).unwrap(), original_vault);
    }

    #[test]
    fn setup_existing_vault_rejects_wrong_passphrase_with_empty_wallets() {
        let dir = tempfile::tempdir().unwrap();
        let vault_path = dir.path().join("vault.json");
        let mnemonic = crate::crypto::parse_mnemonic(TEST_MNEMONIC).unwrap();
        crate::vault::create_vault(&vault_path, &mnemonic, "test passphrase").unwrap();
        let original_vault = std::fs::read(&vault_path).unwrap();
        let db = Storage::new(crate::db::Db::open(&dir.path().join("silo.db")).unwrap());
        assert!(db.call_blocking(|d| d.list_wallets()).unwrap().is_empty());

        let result = finish_setup_blocking(
            db.clone(),
            vault_path.clone(),
            dir.path().to_path_buf(),
            None,
            false,
            zeroize::Zeroizing::new(TEST_MNEMONIC.into()),
            zeroize::Zeroizing::new("wrong passphrase".into()),
        );
        let SetupResult::Failed(message) = result else {
            panic!("wrong passphrase was accepted for an existing vault");
        };
        assert_eq!(
            message,
            "Couldn't reopen the existing wallet: wrong passphrase or corrupted vault"
        );
        assert!(db.call_blocking(|d| d.list_wallets()).unwrap().is_empty());
        assert_eq!(std::fs::read(&vault_path).unwrap(), original_vault);
    }

    fn test_seed() -> crate::crypto::Seed {
        crate::crypto::seed_from_mnemonic(&crate::crypto::parse_mnemonic(TEST_MNEMONIC).unwrap())
    }

    fn storage_with_wallets() -> (Storage, i64) {
        let s = test_seed();
        let mut db = crate::db::Db::open_memory().unwrap();
        db.insert_wallet(
            0,
            crate::types::Role::Master,
            &crate::crypto::derive_address(&s, 0),
            None,
        )
        .unwrap();
        let sub = db
            .insert_wallet(
                1,
                crate::types::Role::Sub,
                &crate::crypto::derive_address(&s, 1),
                None,
            )
            .unwrap();
        (Storage::new(db), sub.id)
    }

    #[test]
    fn persist_signed_send_reports_wedged_wallet_when_cleanup_also_fails() {
        let s = test_seed();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("silo.db");
        let mut db = crate::db::Db::open(&path).unwrap();
        db.unlock_audit_key(&[7u8; 32]).unwrap();
        db.insert_wallet(
            0,
            crate::types::Role::Master,
            &crate::crypto::derive_address(&s, 0),
            None,
        )
        .unwrap();
        let sub = db
            .insert_wallet(
                1,
                crate::types::Role::Sub,
                &crate::crypto::derive_address(&s, 1),
                None,
            )
            .unwrap();
        let sub_id = sub.id;

        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TRIGGER block_intent_updates BEFORE UPDATE ON tx_intents \
                 BEGIN SELECT RAISE(ABORT, 'tx_intents writes blocked for test'); END;",
            )
            .unwrap();
        }

        let storage = Storage::new(db);
        let pending = crate::app::PendingSend {
            from_id: sub_id,
            to: crate::crypto::derive_address(&s, 2),
            lamports: 1_000,
            blockhash: "11111111111111111111111111111111".to_string(),
            lvbh: 100,
            fee: 5_000,
            dest_balance: 0,
            priority_micro: 0,
            prepared_at: std::time::Instant::now(),
        };

        let result = persist_signed_send_blocking(
            storage.clone(),
            pending,
            sub,
            vec![1, 2, 3, 4],
            "sig11111111111111111111111111111111".to_string(),
        );

        let SendPersistResult::Failed(msg) = result else {
            panic!("expected SendPersistResult::Failed, got {result:?}");
        };
        assert!(
            msg.contains("restart silo to reconcile"),
            "a wedged wallet (both writes failed) must tell the user to restart to reconcile, got: {msg}"
        );

        let blocked_to = crate::crypto::derive_address(&s, 2);
        let blocked =
            storage.call_blocking(move |d| d.create_intent(sub_id, &blocked_to, 1_000, None));
        assert!(
            matches!(
                blocked,
                Err(crate::db::CreateIntentError::WalletHasOpenIntent)
            ),
            "the orphaned 'created' intent must keep blocking new sends from the wallet"
        );
    }

    fn worker_deps() -> (Arc<Mutex<Rpc>>, Arc<PriceCache>, reqwest::Client) {
        let client = reqwest::Client::new();
        (
            Arc::new(Mutex::new(Rpc::new(
                client.clone(),
                "http://127.0.0.1:8899".to_string(),
            ))),
            Arc::new(PriceCache::default()),
            client,
        )
    }

    // Drive the real input and persistence paths, but leave their completion unapplied.
    async fn persisted_send_completion(
        existing_intent: bool,
        lock_before_persistence: bool,
    ) -> (crate::app::App, mpsc::Receiver<(u64, Command)>, AppEvent) {
        use crate::app::{App, Modal, PendingSend, Route};
        use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

        let (db, from_id) = storage_with_wallets();
        db.call_blocking(|d| d.unlock_audit_key(&[7u8; 32]))
            .unwrap();
        let (rpc, price, client) = worker_deps();
        let (cmd_tx, mut cmd_rx) = mpsc::channel(8);
        let generation = Arc::new(AtomicU64::new(7));
        let config_dir = std::env::temp_dir().join("silo-send-persistence-test");
        let mut app = App::new(
            db.clone(),
            price.clone(),
            cmd_tx,
            generation.clone(),
            rpc.clone(),
            client.clone(),
            config_dir.clone(),
            "http://127.0.0.1:8899".into(),
            config_dir.join("vault.json"),
        );
        app.wallets = db.call_blocking(|d| d.list_wallets()).unwrap();
        app.seed = Some(test_seed());
        app.focused_wallet = Some(from_id);
        app.route = Route::Send;
        app.modal = Some(Modal::ConfirmSend);
        app.pending_send = Some(PendingSend {
            from_id,
            to: crate::crypto::derive_address(&test_seed(), 2),
            lamports: 1_000,
            blockhash: bs58::encode([3u8; 32]).into_string(),
            lvbh: 100,
            fee: 5_000,
            dest_balance: 0,
            priority_micro: 0,
            prepared_at: std::time::Instant::now(),
        });
        if existing_intent {
            let to = app.pending_send.as_ref().unwrap().to.clone();
            db.call_blocking(move |d| d.create_intent(from_id, &to, 1_000, None))
                .unwrap();
        }
        crate::input::handle_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        assert!(app.blocking_input);
        let (cmd_gen, cmd) = cmd_rx.try_recv().unwrap();
        assert_eq!(cmd_gen, 7);
        assert!(matches!(cmd, Command::PersistSignedSend { .. }));
        if lock_before_persistence {
            app.lock();
        }
        let (evt_tx, mut evt_rx) = mpsc::channel(8);
        handle_command(cmd_gen, cmd, db, rpc, evt_tx, price, client, generation).await;
        let completion = evt_rx.try_recv().unwrap();
        assert!(matches!(
            completion,
            AppEvent::SendPersisted { generation: 7, .. }
        ));
        assert!(evt_rx.try_recv().is_err());
        assert!(cmd_rx.try_recv().is_err());
        (app, cmd_rx, completion)
    }

    #[tokio::test]
    async fn send_persistence_after_lock_before_write_fails_without_intent_or_broadcast() {
        // Existing policy: locking removes the audit key required to create an intent.
        let (mut app, mut rx, completion) = persisted_send_completion(false, true).await;
        let reason = match &completion {
            AppEvent::SendPersisted {
                result: SendPersistResult::Failed(reason),
                generation: 7,
            } => reason.clone(),
            other => panic!("unexpected completion: {other:?}"),
        };
        assert_eq!(
            reason,
            "Couldn't record transfer: audit key unavailable (vault locked)"
        );

        app.apply_app_event(completion);

        assert_eq!(app.route, crate::app::Route::Unlock);
        assert!(app.seed.is_none());
        assert!(app.modal.is_none());
        assert!(app.pending_send.is_none());
        assert!(!app.blocking_input);
        assert!(!app.preparing_send);
        assert_eq!(app.generation.load(Ordering::SeqCst), 7);
        assert!(
            rx.try_recv().is_err(),
            "failed persistence must not queue Broadcast"
        );
        let wallet_id = app.focused_wallet.unwrap();
        assert!(
            app.db
                .call_blocking(move |d| d.list_intents_for_wallet(wallet_id, 10))
                .unwrap()
                .is_empty()
        );
        assert!(app.toasts.iter().any(|t| t.text == reason));
    }

    #[tokio::test]
    async fn send_persistence_completion_after_lock_preserves_signed_intent_and_broadcast() {
        use crate::app::Route;
        let (mut app, mut rx, completion) = persisted_send_completion(false, false).await;
        let intent_id = match &completion {
            AppEvent::SendPersisted {
                result: SendPersistResult::Signed { intent_id },
                ..
            } => *intent_id,
            other => panic!("unexpected completion: {other:?}"),
        };
        let before = app
            .db
            .call_blocking(move |d| d.get_intent(intent_id))
            .unwrap()
            .unwrap();
        assert_eq!(before.status, crate::types::IntentStatus::Signed);
        assert!(before.signature.as_ref().is_some_and(|s| !s.is_empty()));
        assert!(
            before
                .signed_tx
                .as_ref()
                .is_some_and(|wire| !wire.is_empty())
        );

        app.lock();
        app.apply_app_event(completion);

        assert_eq!(app.route, Route::Unlock);
        assert!(app.seed.is_none());
        assert!(app.modal.is_none());
        assert!(app.pending_send.is_none());
        assert!(!app.blocking_input);
        assert!(!app.preparing_send);
        assert_eq!(app.generation.load(Ordering::SeqCst), 7);
        assert!(
            matches!(rx.try_recv().unwrap(), (7, Command::Broadcast { intent_id: id }) if id == intent_id)
        );
        assert!(
            rx.try_recv().is_err(),
            "locked completion must not refresh unlocked details"
        );
        let after = app
            .db
            .call_blocking(move |d| d.get_intent(intent_id))
            .unwrap()
            .unwrap();
        assert_eq!(after.status, crate::types::IntentStatus::Signed);
        assert_eq!(after.signature, before.signature);
        assert_eq!(after.signed_tx, before.signed_tx);
    }

    #[tokio::test]
    async fn send_persistence_completion_unlocked_routes_and_broadcasts() {
        let (mut app, mut rx, completion) = persisted_send_completion(false, false).await;
        let intent_id = match &completion {
            AppEvent::SendPersisted {
                result: SendPersistResult::Signed { intent_id },
                ..
            } => *intent_id,
            other => panic!("unexpected completion: {other:?}"),
        };
        app.apply_app_event(completion);
        assert_eq!(app.route, crate::app::Route::WalletDetail);
        assert!(app.seed.is_some());
        assert!(!app.blocking_input);
        assert_eq!(app.generation.load(Ordering::SeqCst), 7);
        assert!(
            matches!(rx.try_recv().unwrap(), (7, Command::Broadcast { intent_id: id }) if id == intent_id)
        );
        assert!(
            matches!(rx.try_recv().unwrap(), (7, Command::LoadDetail { wallet_id }) if Some(wallet_id) == app.focused_wallet)
        );
        assert!(rx.try_recv().is_err());
        assert!(
            app.toasts
                .iter()
                .any(|t| t.text == "Signing & broadcasting…")
        );
    }

    #[tokio::test]
    async fn send_persistence_failure_after_lock_does_not_broadcast() {
        let (mut app, mut rx, completion) = persisted_send_completion(true, false).await;
        let reason = match &completion {
            AppEvent::SendPersisted {
                result: SendPersistResult::Failed(reason),
                ..
            } => reason.clone(),
            other => panic!("unexpected completion: {other:?}"),
        };
        assert_eq!(reason, "This wallet already has a transfer in progress");
        app.lock();
        app.apply_app_event(completion);
        assert_eq!(app.route, crate::app::Route::Unlock);
        assert!(app.seed.is_none());
        assert!(app.modal.is_none());
        assert!(app.pending_send.is_none());
        assert!(!app.blocking_input);
        assert!(!app.preparing_send);
        assert_eq!(app.generation.load(Ordering::SeqCst), 7);
        assert!(rx.try_recv().is_err());
        assert!(app.toasts.iter().any(|t| t.text == reason));
    }

    async fn assert_send_completion_preserves_pending_unlock(existing_intent: bool) {
        use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

        let (mut app, mut rx, completion) = persisted_send_completion(existing_intent, false).await;
        let signed_intent = match &completion {
            AppEvent::SendPersisted {
                result: SendPersistResult::Signed { intent_id },
                ..
            } => {
                assert!(!existing_intent);
                Some(*intent_id)
            }
            AppEvent::SendPersisted {
                result: SendPersistResult::Failed(reason),
                ..
            } => {
                assert!(existing_intent);
                assert_eq!(reason, "This wallet already has a transfer in progress");
                None
            }
            other => panic!("unexpected completion: {other:?}"),
        };
        app.lock();
        assert!(!app.blocking_input);
        crate::input::handle_event(&mut app, Event::Paste("test passphrase".into()));
        crate::input::handle_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        assert!(app.blocking_input);
        assert!(matches!(
            rx.try_recv().unwrap(),
            (7, Command::UnlockVault { vault_path, passphrase })
                if vault_path == app.vault_path && passphrase.as_str() == "test passphrase"
        ));
        assert!(app.input.passphrase.is_empty());
        assert!(rx.try_recv().is_err());

        app.apply_app_event(completion);

        assert!(
            app.blocking_input,
            "send completion must not clear a pending unlock"
        );
        assert_eq!(app.route, crate::app::Route::Unlock);
        assert!(app.seed.is_none());
        assert!(app.modal.is_none());
        assert!(app.pending_send.is_none());
        assert!(!app.preparing_send);
        assert_eq!(app.generation.load(Ordering::SeqCst), 7);
        if let Some(intent_id) = signed_intent {
            assert!(
                matches!(rx.try_recv().unwrap(), (7, Command::Broadcast { intent_id: id }) if id == intent_id)
            );
        }
        assert!(rx.try_recv().is_err());
        crate::input::handle_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        assert!(app.blocking_input);
        assert!(
            rx.try_recv().is_err(),
            "second Enter must not queue another unlock"
        );
        assert!(
            app.toasts
                .iter()
                .any(|t| t.text == "Unlock already in progress")
        );
    }

    #[tokio::test]
    async fn send_persistence_success_preserves_pending_unlock() {
        assert_send_completion_preserves_pending_unlock(false).await;
    }

    #[tokio::test]
    async fn send_persistence_failure_preserves_pending_unlock() {
        assert_send_completion_preserves_pending_unlock(true).await;
    }

    #[tokio::test]
    async fn send_persistence_completion_queue_errors_preserve_route() {
        for locked in [false, true] {
            for closed in [false, true] {
                let (mut app, mut rx, completion) = persisted_send_completion(false, false).await;
                if locked {
                    app.lock();
                }
                let route = app.route;
                if closed {
                    rx.close();
                } else {
                    for _ in 0..8 {
                        assert!(app.send_cmd(Command::LoadWallets));
                    }
                }
                app.apply_app_event(completion);
                assert_eq!(app.route, route);
                assert_eq!(app.seed.is_none(), locked);
                assert!(app.modal.is_none());
                assert!(app.pending_send.is_none());
                assert!(!app.blocking_input);
                assert!(!app.preparing_send);
                assert_eq!(app.generation.load(Ordering::SeqCst), 7);
                let expected = if closed {
                    "Background worker stopped"
                } else {
                    "Command queue is full — try again"
                };
                assert!(app.toasts.iter().any(|t| t.text == expected));
                if !closed {
                    for _ in 0..8 {
                        assert!(matches!(rx.try_recv().unwrap(), (7, Command::LoadWallets)));
                    }
                }
                assert!(rx.try_recv().is_err());
            }
        }
    }

    fn publication_price(currency: crate::types::Currency, value: f64) -> SolPrice {
        SolPrice {
            value,
            currency,
            fetched_at: unix_now_secs(),
            source: crate::price::PriceSource::CoinGecko,
        }
    }

    #[tokio::test]
    async fn price_publication_accepts_matching_and_default_currency() {
        use crate::types::Currency;
        for stored in [
            None,
            Some("unknown"),
            Some(Currency::Usd.code()),
            Some(Currency::Jpy.code()),
        ] {
            let db = Storage::new(crate::db::Db::open_memory().unwrap());
            if let Some(code) = stored {
                db.call(move |d| d.set_meta("currency", code))
                    .await
                    .unwrap();
            }
            let currency = if stored == Some(Currency::Jpy.code()) {
                Currency::Jpy
            } else {
                Currency::Usd
            };
            let p = publication_price(currency, 150.0);
            let price = Arc::new(PriceCache::default());
            let (tx, mut rx) = mpsc::channel(8);
            assert!(
                publish_price(&db, price.clone(), &tx, Arc::new(AtomicU64::new(7)), 7, p)
                    .await
                    .unwrap()
            );
            assert_eq!(price.get().unwrap().to_meta_json(), p.to_meta_json());
            assert_eq!(
                db.call(|d| d.get_meta("last_price")).await.unwrap(),
                Some(p.to_meta_json())
            );
            match rx.try_recv().unwrap() {
                AppEvent::Price { price, generation } => {
                    assert_eq!(price.to_meta_json(), p.to_meta_json());
                    assert_eq!(generation, 7);
                }
                other => panic!("unexpected event: {other:?}"),
            }
            assert!(rx.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn price_publication_rejects_late_usd_after_persist_setting_jpy() {
        use crate::types::Currency;
        let (db, _) = storage_with_wallets();
        db.call(|d| d.set_meta("currency", Currency::Usd.code()))
            .await
            .unwrap();
        let (rpc, price, client) = worker_deps();
        let (tx, mut rx) = mpsc::channel(8);
        let generation = Arc::new(AtomicU64::new(0));
        let usd = publication_price(Currency::Usd, 150.0);
        assert!(
            publish_price(&db, price.clone(), &tx, generation.clone(), 0, usd)
                .await
                .unwrap()
        );
        assert!(matches!(rx.try_recv().unwrap(), AppEvent::Price { .. }));
        handle_command(
            0,
            Command::PersistSetting {
                change: SettingChange::Currency(Currency::Jpy),
            },
            db.clone(),
            rpc,
            tx.clone(),
            price.clone(),
            client,
            generation.clone(),
        )
        .await;
        match rx.try_recv().unwrap() {
            AppEvent::SettingPersisted {
                change,
                result,
                generation,
            } => {
                assert_eq!(change, SettingChange::Currency(Currency::Jpy));
                result.unwrap();
                assert_eq!(generation, 0);
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert_eq!(generation.load(Ordering::SeqCst), 0);
        assert_eq!(
            db.call(|d| d.get_meta("currency"))
                .await
                .unwrap()
                .as_deref(),
            Some(Currency::Jpy.code())
        );
        let jpy = publication_price(Currency::Jpy, 22_500.0);
        assert!(
            publish_price(&db, price.clone(), &tx, generation.clone(), 0, jpy)
                .await
                .unwrap()
        );
        assert!(
            matches!(rx.try_recv().unwrap(), AppEvent::Price { price, generation: 0 } if price.currency == Currency::Jpy)
        );
        let accepted = publish_price(&db, price.clone(), &tx, generation.clone(), 0, usd)
            .await
            .unwrap();
        assert_eq!(
            price.get().unwrap().to_meta_json(),
            jpy.to_meta_json(),
            "late USD must not replace JPY cache"
        );
        assert_eq!(
            db.call(|d| d.get_meta("last_price")).await.unwrap(),
            Some(jpy.to_meta_json())
        );
        assert!(!accepted);
        assert!(rx.try_recv().is_err(), "rejected USD must not emit");
        assert_eq!(generation.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn price_publication_checks_generation_at_actor_execution_after_replace() {
        use crate::types::Currency;
        let db = Storage::new(crate::db::Db::open_memory().unwrap());
        let replacement = crate::db::Db::open_memory().unwrap();
        replacement
            .set_meta("currency", Currency::Usd.code())
            .unwrap();
        let previous = publication_price(Currency::Usd, 100.0);
        replacement
            .set_meta("last_price", &previous.to_meta_json())
            .unwrap();
        db.replace(replacement);
        let price = Arc::new(PriceCache::default());
        price.set(previous);
        let generation = Arc::new(AtomicU64::new(0));
        let (tx, mut rx) = mpsc::channel(8);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocked_db = db.clone();
        let blocker = std::thread::spawn(move || {
            blocked_db.call_blocking(move |_| {
                ready_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            })
        });
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let publication = publish_price(
            &db,
            price.clone(),
            &tx,
            generation.clone(),
            0,
            publication_price(Currency::Usd, 150.0),
        );
        tokio::pin!(publication);
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(publication.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        generation.store(1, Ordering::SeqCst);
        release_tx.send(()).unwrap();
        let accepted = publication.await.unwrap();
        blocker.join().unwrap();
        assert_eq!(price.get().unwrap().to_meta_json(), previous.to_meta_json());
        assert_eq!(
            db.call(|d| d.get_meta("last_price")).await.unwrap(),
            Some(previous.to_meta_json())
        );
        assert!(!accepted);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn price_publication_sqlite_read_failure_does_not_publish() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("silo.db");
        let db = Storage::new(crate::db::Db::open(&path).unwrap());
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("ALTER TABLE meta RENAME TO unavailable_meta;")
            .unwrap();
        let price = Arc::new(PriceCache::default());
        let (tx, mut rx) = mpsc::channel(8);
        let result = publish_price(
            &db,
            price.clone(),
            &tx,
            Arc::new(AtomicU64::new(0)),
            0,
            publication_price(crate::types::Currency::Usd, 150.0),
        )
        .await;
        assert!(
            result.is_err(),
            "SQLite read errors must not become default USD"
        );
        assert!(price.get().is_none());
        assert!(rx.try_recv().is_err());
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM unavailable_meta WHERE key='last_price'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
        let expected_error = db.call(|d| d.get_meta("currency")).await.unwrap_err();
        let (rpc, _, client) = worker_deps();
        handle_command(
            0,
            Command::FetchPrice,
            db,
            rpc,
            tx,
            price.clone(),
            client,
            Arc::new(AtomicU64::new(0)),
        )
        .await;
        match rx.try_recv().unwrap() {
            AppEvent::Error {
                message,
                generation: 0,
            } => {
                assert_eq!(
                    message,
                    format!("price currency lookup failed: {expected_error:#}")
                );
            }
            other => panic!("expected database error, got {other:?}"),
        }
        assert!(price.get().is_none());
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn price_publication_sqlite_write_failure_preserves_previous_price() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("silo.db");
        let db = Storage::new(crate::db::Db::open(&path).unwrap());
        let previous = publication_price(crate::types::Currency::Usd, 100.0);
        db.call(move |d| d.set_meta("last_price", &previous.to_meta_json()))
            .await
            .unwrap();
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TRIGGER block_price BEFORE INSERT ON meta WHEN NEW.key='last_price' BEGIN SELECT RAISE(ABORT, 'price writes blocked'); END;").unwrap();
        let price = Arc::new(PriceCache::default());
        price.set(previous);
        let (tx, mut rx) = mpsc::channel(8);
        let result = publish_price(
            &db,
            price.clone(),
            &tx,
            Arc::new(AtomicU64::new(0)),
            0,
            publication_price(crate::types::Currency::Usd, 150.0),
        )
        .await;
        assert!(result.is_err(), "SQLite write failure must propagate");
        assert_eq!(price.get().unwrap().to_meta_json(), previous.to_meta_json());
        assert_eq!(
            db.call(|d| d.get_meta("last_price")).await.unwrap(),
            Some(previous.to_meta_json())
        );
        assert!(rx.try_recv().is_err());
    }

    struct RawMockServer {
        url: String,
        requests: Arc<Mutex<Vec<serde_json::Value>>>,
        _worker: std::thread::JoinHandle<()>,
    }

    impl RawMockServer {
        fn serve(
            listener: std::net::TcpListener,
            requests: Arc<Mutex<Vec<serde_json::Value>>>,
            mut responder: impl FnMut(&str) -> String + Send + 'static,
        ) -> std::thread::JoinHandle<()> {
            use std::io::{Read, Write};
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { break };
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 1024];
                    loop {
                        let n = stream.read(&mut tmp).unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            let headers = String::from_utf8_lossy(&buf[..end]);
                            let len = headers
                                .lines()
                                .find_map(|l| {
                                    let (n, v) = l.split_once(':')?;
                                    n.eq_ignore_ascii_case("content-length")
                                        .then(|| v.trim().parse::<usize>().ok())
                                        .flatten()
                                })
                                .unwrap_or(0);
                            if buf.len().saturating_sub(end + 4) >= len {
                                break;
                            }
                        }
                    }
                    if buf.is_empty() {
                        break;
                    }
                    let req = String::from_utf8_lossy(&buf);
                    let body = req.split("\r\n\r\n").nth(1).unwrap_or("");
                    let method = match serde_json::from_str::<serde_json::Value>(body) {
                        Ok(v) => {
                            let m = v["method"].as_str().unwrap_or("").to_string();
                            requests.lock().unwrap().push(v);
                            m
                        }
                        Err(_) => String::new(),
                    };
                    let resp_body = responder(&method);
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        resp_body.len()
                    );
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(resp_body.as_bytes());
                }
            })
        }

        fn new(bodies: Vec<String>) -> Self {
            use std::collections::VecDeque;
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let requests = Arc::new(Mutex::new(Vec::new()));
            let mut responses = VecDeque::from(bodies);
            let worker = Self::serve(listener, requests.clone(), move |_method| {
                responses.pop_front().unwrap_or_else(|| "{}".to_string())
            });
            RawMockServer {
                url,
                requests,
                _worker: worker,
            }
        }

        fn routed(routes: Vec<(&'static str, String)>) -> Self {
            use std::collections::HashMap;
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let requests = Arc::new(Mutex::new(Vec::new()));
            let table: HashMap<String, String> = routes
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect();
            let worker = Self::serve(listener, requests.clone(), move |method| {
                table
                    .get(method)
                    .cloned()
                    .unwrap_or_else(|| "{}".to_string())
            });
            RawMockServer {
                url,
                requests,
                _worker: worker,
            }
        }

        fn finish(self) {
            let stream =
                std::net::TcpStream::connect(self.url.trim_start_matches("http://")).unwrap();
            stream.shutdown(std::net::Shutdown::Both).unwrap();
            self._worker.join().unwrap();
        }

        fn methods(&self) -> Vec<String> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .map(|v| v["method"].as_str().unwrap_or("").to_string())
                .collect()
        }
    }

    #[tokio::test]
    async fn reconcile_rpc_failure_emits_offline_not_complete() {
        let (db, sub_id) = storage_with_wallets();
        db.call_blocking(move |d| {
            let intent = d.create_intent(sub_id, "destination", 1000, None).unwrap();
            d.mark_signed(intent.id, "Sig", "bh", 1000, 5000, b"wire")
                .unwrap();
        });
        let server = RawMockServer::routed(vec![(
            "getSignatureStatuses",
            json!({"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"offline"}}).to_string(),
        )]);
        let (rpc, price, client) = worker_deps();
        *rpc.lock_recover() = Rpc::new(client.clone(), server.url.clone());
        let (tx, mut rx) = mpsc::channel(8);
        handle_command(
            1,
            Command::Reconcile,
            db,
            rpc,
            tx,
            price,
            client,
            Arc::new(AtomicU64::new(1)),
        )
        .await;
        let event = rx.try_recv().unwrap();
        server.finish();
        assert!(
            matches!(event, AppEvent::ReconcileFailedOffline { generation: 1 }),
            "{event:?}"
        );
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn reconcile_and_change_rpc_report_clean_partial_and_database_failure() {
        for change_rpc in [false, true] {
            for case in ["clean", "partial", "database_read", "database_write"] {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("silo.db");
                let mut db = crate::db::Db::open(&path).unwrap();
                db.unlock_audit_key(&[7; 32]).unwrap();
                let mut ids = Vec::new();
                for index in 0..2 {
                    let wallet = db
                        .insert_wallet(
                            index,
                            if index == 0 {
                                crate::types::Role::Master
                            } else {
                                crate::types::Role::Sub
                            },
                            &format!("Wallet{index}"),
                            None,
                        )
                        .unwrap();
                    let intent = db
                        .create_intent(wallet.id, "destination", 1000, None)
                        .unwrap();
                    db.mark_signed(intent.id, &format!("Sig{index}"), "bh", 1000, 5000, b"wire")
                        .unwrap();
                    ids.push(intent.id);
                }
                let conn = rusqlite::Connection::open(&path).unwrap();
                match case {
                    "database_read" => conn.execute_batch("DROP TABLE tx_intents;").unwrap(),
                    "database_write" => conn.execute_batch("CREATE TRIGGER block_updates BEFORE UPDATE ON tx_intents BEGIN SELECT RAISE(ABORT, 'blocked intent update'); END;").unwrap(),
                    _ => {}
                }
                drop(conn);
                let db = Storage::new(db);
                let finalized = json!({"jsonrpc":"2.0","id":1,"result":{"context":{"slot":1},"value":[{"err":null,"confirmationStatus":"finalized"}]}}).to_string();
                let height = json!({"jsonrpc":"2.0","id":1,"result":1000}).to_string();
                let responses = if case == "partial" {
                    vec![
                        json!({"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"offline"}})
                            .to_string(),
                        finalized,
                        height,
                    ]
                } else {
                    vec![finalized.clone(), height.clone(), finalized, height]
                };
                let server = RawMockServer::new(responses);
                let (rpc, price, client) = worker_deps();
                if !change_rpc {
                    *rpc.lock_recover() = Rpc::new(client.clone(), server.url.clone());
                }
                let command = if change_rpc {
                    Command::ChangeRpc {
                        url: server.url.clone(),
                    }
                } else {
                    Command::Reconcile
                };
                let (tx, mut rx) = mpsc::channel(8);
                handle_command(
                    1,
                    command,
                    db.clone(),
                    rpc,
                    tx,
                    price,
                    client,
                    Arc::new(AtomicU64::new(1)),
                )
                .await;
                let methods = server.methods();
                let url = server.url.clone();
                server.finish();
                if change_rpc {
                    assert!(
                        matches!(rx.try_recv().unwrap(), AppEvent::RpcChanged { url: got, generation: 1 } if got == url),
                        "ack must precede reconciliation: {case}"
                    );
                    assert_eq!(db.call(|d| d.get_meta("rpc_url")).await.unwrap(), Some(url));
                }
                let event = rx.try_recv().unwrap();
                if case == "clean" {
                    assert!(
                        matches!(
                            event,
                            AppEvent::ReconcileComplete {
                                resolved: 2,
                                generation: 1
                            }
                        ),
                        "{event:?}"
                    );
                } else {
                    assert!(
                        matches!(event, AppEvent::ReconcileFailedOffline { generation: 1 }),
                        "{case}: {event:?}"
                    );
                }
                assert!(rx.try_recv().is_err());
                if case == "database_read" {
                    assert!(methods.is_empty());
                } else {
                    let states = db
                        .call(move |d| {
                            ids.into_iter()
                                .map(|id| d.get_intent(id).unwrap().unwrap().status)
                                .collect::<Vec<_>>()
                        })
                        .await;
                    assert_eq!(
                        states,
                        match case {
                            "clean" => vec![IntentStatus::Confirmed, IntentStatus::Confirmed],
                            "partial" => vec![IntentStatus::Signed, IntentStatus::Confirmed],
                            _ => vec![IntentStatus::Signed, IntentStatus::Signed],
                        }
                    );
                }
                assert!(db.call(|d| d.verify_audit_chain()).await.unwrap());
            }
        }
    }

    #[tokio::test]
    async fn refresh_balances_emits_only_balances_on_success() {
        let (db, _) = storage_with_wallets();
        let server = RawMockServer::routed(vec![("getMultipleAccounts", json!({"jsonrpc":"2.0","id":1,"result":{"context":{"slot":1},"value":[{"lamports":1000},{"lamports":2000}]}}).to_string())]);
        let (rpc, price, client) = worker_deps();
        *rpc.lock_recover() = Rpc::new(client.clone(), server.url.clone());
        let wallets = db.call(|d| d.list_wallets()).await.unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        handle_command(
            1,
            Command::RefreshBalances {
                include_archived: false,
            },
            db,
            rpc,
            tx,
            price,
            client,
            Arc::new(AtomicU64::new(1)),
        )
        .await;
        server.finish();
        match rx.try_recv().unwrap() {
            AppEvent::Balances {
                list,
                generation: 1,
            } => assert_eq!(list, vec![(wallets[0].id, 1000), (wallets[1].id, 2000)]),
            other => panic!("{other:?}"),
        }
        assert!(
            rx.try_recv().is_err(),
            "no trailing Online event may overwrite a reconciliation failure"
        );
    }

    #[tokio::test]
    async fn open_profile_command_opens_off_thread_without_touching_shared_db() {
        let (db, _sub) = storage_with_wallets();
        let (rpc, price, client) = worker_deps();
        let (evt_tx, mut evt_rx) = mpsc::channel(8);
        let generation = Arc::new(AtomicU64::new(1));
        let id = crate::profiles::new_id();
        let config_dir =
            std::env::temp_dir().join(format!("silo-open-test-{}-{id}", std::process::id()));
        let dir = crate::profiles::dir_for(&config_dir, &id).unwrap();
        crate::profiles::ensure_private_dir(&dir).unwrap();
        let shared_before = db.call_blocking(|d| d.list_wallets().unwrap().len());

        handle_command(
            1,
            Command::OpenProfile {
                config_dir: config_dir.clone(),
                id: id.clone(),
            },
            db.clone(),
            rpc,
            evt_tx,
            price,
            client,
            generation,
        )
        .await;

        match evt_rx.try_recv().unwrap() {
            AppEvent::ProfileOpened { result, generation } => {
                assert_eq!(generation, 1);
                let payload = result.unwrap();
                assert_eq!(payload.id, id);
                assert!(!payload.created);
                assert_eq!(payload.db.list_wallets().unwrap().len(), 0);
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert_eq!(
            db.call_blocking(|d| d.list_wallets().unwrap().len()),
            shared_before
        );
        let _ = std::fs::remove_dir_all(&config_dir);
    }

    #[tokio::test]
    async fn create_profile_command_creates_dir_and_fresh_db_off_thread() {
        let (db, _sub) = storage_with_wallets();
        let (rpc, price, client) = worker_deps();
        let (evt_tx, mut evt_rx) = mpsc::channel(8);
        let generation = Arc::new(AtomicU64::new(2));
        let id = crate::profiles::new_id();
        let config_dir =
            std::env::temp_dir().join(format!("silo-create-test-{}-{id}", std::process::id()));
        let dir = crate::profiles::dir_for(&config_dir, &id).unwrap();
        let shared_before = db.call_blocking(|d| d.list_wallets().unwrap().len());

        handle_command(
            2,
            Command::CreateProfile {
                config_dir: config_dir.clone(),
                id: id.clone(),
            },
            db.clone(),
            rpc,
            evt_tx,
            price,
            client,
            generation,
        )
        .await;

        match evt_rx.try_recv().unwrap() {
            AppEvent::ProfileOpened { result, generation } => {
                assert_eq!(generation, 2);
                let payload = result.unwrap();
                assert_eq!(payload.id, id);
                assert!(payload.created);
                assert_eq!(payload.db.list_wallets().unwrap().len(), 0);
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(dir.join("silo.db").exists());
        assert_eq!(
            db.call_blocking(|d| d.list_wallets().unwrap().len()),
            shared_before
        );
        let _ = std::fs::remove_dir_all(&config_dir);
    }

    #[tokio::test]
    async fn archive_wallet_command_persists_and_emits_reloaded_wallets() {
        let (db, sub_id) = storage_with_wallets();
        let (rpc, price, client) = worker_deps();
        let (evt_tx, mut evt_rx) = mpsc::channel(8);
        let generation = Arc::new(AtomicU64::new(0));
        handle_command(
            0,
            Command::ArchiveWallet {
                id: sub_id,
                want: true,
            },
            db.clone(),
            rpc,
            evt_tx,
            price,
            client,
            generation,
        )
        .await;
        match evt_rx.try_recv().unwrap() {
            AppEvent::WalletArchived {
                id, want, result, ..
            } => {
                assert_eq!(id, sub_id);
                assert!(want);
                let wallets = result.unwrap();
                assert!(wallets.iter().find(|w| w.id == sub_id).unwrap().archived);
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(
            db.call_blocking(|d| d.list_wallets())
                .unwrap()
                .iter()
                .find(|w| w.id == sub_id)
                .unwrap()
                .archived
        );
    }

    #[tokio::test]
    async fn archive_wallet_command_stale_generation_skips_write_and_event() {
        let (db, sub_id) = storage_with_wallets();
        let (rpc, price, client) = worker_deps();
        let (evt_tx, mut evt_rx) = mpsc::channel(8);
        let generation = Arc::new(AtomicU64::new(5));
        handle_command(
            0,
            Command::ArchiveWallet {
                id: sub_id,
                want: true,
            },
            db.clone(),
            rpc,
            evt_tx,
            price,
            client,
            generation,
        )
        .await;
        assert!(evt_rx.try_recv().is_err(), "stale command must not emit");
        assert!(
            !db.call_blocking(|d| d.list_wallets())
                .unwrap()
                .iter()
                .find(|w| w.id == sub_id)
                .unwrap()
                .archived,
            "stale command must not write"
        );
    }

    #[tokio::test]
    async fn derive_subwallet_command_inserts_next_index() {
        let (db, _sub_id) = storage_with_wallets();
        let before = db.call_blocking(|d| d.list_wallets()).unwrap().len();
        let (rpc, price, client) = worker_deps();
        let (evt_tx, mut evt_rx) = mpsc::channel(8);
        let generation = Arc::new(AtomicU64::new(0));
        handle_command(
            0,
            Command::DeriveSubwallet { seed: test_seed() },
            db.clone(),
            rpc,
            evt_tx,
            price,
            client,
            generation,
        )
        .await;
        match evt_rx.try_recv().unwrap() {
            AppEvent::SubwalletDerived { result, .. } => {
                let (idx, wallets) = result.unwrap();
                assert_eq!(idx, 2);
                assert_eq!(wallets.len(), before + 1);
                assert!(
                    wallets
                        .iter()
                        .any(|w| w.account_index == 2 && w.role == crate::types::Role::Sub)
                );
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn persist_setting_command_writes_audited_meta() {
        let (db, _sub_id) = storage_with_wallets();
        let (rpc, price, client) = worker_deps();
        let (evt_tx, mut evt_rx) = mpsc::channel(8);
        let generation = Arc::new(AtomicU64::new(0));
        handle_command(
            0,
            Command::PersistSetting {
                change: SettingChange::AutoLock(9),
            },
            db.clone(),
            rpc,
            evt_tx,
            price,
            client,
            generation,
        )
        .await;
        match evt_rx.try_recv().unwrap() {
            AppEvent::SettingPersisted { change, result, .. } => {
                assert_eq!(change, SettingChange::AutoLock(9));
                result.unwrap();
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert_eq!(
            db.call_blocking(|d| d.get_meta("auto_lock_minutes"))
                .unwrap(),
            Some("9".to_string())
        );
    }

    #[tokio::test]
    async fn set_wallet_text_command_persists_label() {
        let (db, sub_id) = storage_with_wallets();
        let (rpc, price, client) = worker_deps();
        let (evt_tx, mut evt_rx) = mpsc::channel(8);
        let generation = Arc::new(AtomicU64::new(0));
        handle_command(
            0,
            Command::SetWalletText {
                id: sub_id,
                field: WalletTextField::Label,
                value: Some("hot".into()),
            },
            db.clone(),
            rpc,
            evt_tx,
            price,
            client,
            generation,
        )
        .await;
        match evt_rx.try_recv().unwrap() {
            AppEvent::WalletTextSet { field, result, .. } => {
                assert_eq!(field, WalletTextField::Label);
                let wallets = result.unwrap();
                assert_eq!(
                    wallets
                        .iter()
                        .find(|w| w.id == sub_id)
                        .unwrap()
                        .label
                        .as_deref(),
                    Some("hot")
                );
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn set_intent_note_command_persists_note() {
        let (db, sub_id) = storage_with_wallets();
        let to = crate::crypto::derive_address(&test_seed(), 0);
        let intent = db
            .call_blocking(move |d| d.create_intent(sub_id, &to, 1_000, None))
            .unwrap();
        let (rpc, price, client) = worker_deps();
        let (evt_tx, mut evt_rx) = mpsc::channel(8);
        let generation = Arc::new(AtomicU64::new(0));
        handle_command(
            0,
            Command::SetIntentNote {
                wallet_id: sub_id,
                id: intent.id,
                value: Some("memo".into()),
            },
            db.clone(),
            rpc,
            evt_tx,
            price,
            client,
            generation,
        )
        .await;
        match evt_rx.try_recv().unwrap() {
            AppEvent::IntentNoteSet { result, .. } => {
                let intents = result.unwrap();
                assert_eq!(
                    intents
                        .iter()
                        .find(|i| i.id == intent.id)
                        .unwrap()
                        .note
                        .as_deref(),
                    Some("memo")
                );
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn finalize_surfaces_db_write_failure_instead_of_dropping_result() {
        let (db, sub_id) = storage_with_wallets();
        let to = crate::crypto::derive_address(&test_seed(), 0);
        let intent = db
            .call_blocking(move |d| d.create_intent(sub_id, &to, 1_000, None))
            .unwrap();
        db.call_blocking(|d| d.lock_audit_key());
        let (evt_tx, mut evt_rx) = mpsc::channel(8);
        let generation = Arc::new(AtomicU64::new(0));

        finalize(
            &db,
            &evt_tx,
            intent.id,
            "sig-finalize-err",
            TerminalStatus::Confirmed,
            None,
            &generation,
            0,
        )
        .await;

        let mut saw_error = false;
        let mut saw_result = false;
        while let Ok(ev) = evt_rx.try_recv() {
            match ev {
                AppEvent::Error { message, .. } => {
                    assert!(message.contains("couldn't be recorded locally"));
                    saw_error = true;
                }
                AppEvent::TransferResult {
                    intent_id, outcome, ..
                } => {
                    assert_eq!(intent_id, intent.id);
                    assert!(matches!(outcome, TransferOutcome::Confirmed { .. }));
                    saw_result = true;
                }
                other => panic!("unexpected event: {other:?}"),
            }
        }
        assert!(
            saw_error,
            "a DB write failure while finalizing must be surfaced as an error event"
        );
        assert!(
            saw_result,
            "the on-chain confirmation must still reach the UI, not be silently dropped"
        );
    }

    #[test]
    fn definitive_rejections_are_classified_failed_uncertain_errors_are_not() {
        use crate::solana::rpc::RpcError;

        let jsonrpc = RpcError::JsonRpc {
            method: "sendTransaction",
            code: -32002,
            message: "Transaction simulation failed: insufficient funds".into(),
        };
        let reason = definitive_rejection_reason(&jsonrpc).expect("JsonRpc is definitive");
        assert!(reason.contains("transfer rejected by network"));
        assert!(reason.contains("insufficient funds"));

        let http4xx = RpcError::NonRetryHttp {
            method: "sendTransaction",
            status: reqwest::StatusCode::BAD_REQUEST,
            body: "bad request".into(),
        };
        assert!(definitive_rejection_reason(&http4xx).is_some());

        assert!(
            definitive_rejection_reason(&RpcError::RetryExhaustedHttp {
                method: "sendTransaction",
                status: reqwest::StatusCode::SERVICE_UNAVAILABLE,
            })
            .is_none()
        );
        assert!(
            definitive_rejection_reason(&RpcError::MissingResult {
                method: "sendTransaction"
            })
            .is_none()
        );
        assert!(
            definitive_rejection_reason(&RpcError::LengthMismatch {
                method: "sendTransaction",
                expected: 1,
                actual: 0,
            })
            .is_none()
        );
        let decode = serde_json::from_str::<serde_json::Value>("not json").unwrap_err();
        assert!(
            definitive_rejection_reason(&RpcError::Decode {
                method: "sendTransaction",
                source: decode,
            })
            .is_none()
        );
    }

    #[tokio::test]
    async fn broadcast_finalizes_failed_on_preflight_rejection_without_polling() {
        let (db, sub_id) = storage_with_wallets();
        let to = crate::crypto::derive_address(&test_seed(), 0);
        let intent = db
            .call_blocking(move |d| d.create_intent(sub_id, &to, 1_000, None))
            .unwrap();
        let sig = "Sig1111111111111111111111111111111111111111111";
        db.call_blocking(move |d| {
            d.mark_signed(intent.id, sig, "bh", 1000, 5000, b"wire")
                .unwrap()
        });
        assert!(db.call_blocking(move |d| d.has_open_intent(sub_id).unwrap()));

        let server = RawMockServer::new(vec![
            json!({"jsonrpc":"2.0","id":1,"error":{"code":-32002,"message":"Transaction simulation failed: insufficient funds"}})
                .to_string(),
        ]);
        let rpc = Arc::new(Mutex::new(Rpc::new(
            reqwest::Client::new(),
            server.url.clone(),
        )));
        let (evt_tx, mut evt_rx) = mpsc::channel(8);
        let generation = Arc::new(AtomicU64::new(0));

        broadcast_and_poll(intent.id, db.clone(), rpc, evt_tx, generation, 0).await;

        let mut failed_reason = None;
        while let Ok(ev) = evt_rx.try_recv() {
            if let AppEvent::TransferResult {
                outcome: TransferOutcome::Failed { reason },
                ..
            } = ev
            {
                failed_reason = Some(reason);
            }
        }
        let reason = failed_reason.expect("a Failed TransferResult must be emitted");
        assert!(reason.contains("transfer rejected by network"), "{reason}");
        assert!(reason.contains("insufficient funds"), "{reason}");

        let got = db.call_blocking(move |d| d.get_intent(intent.id).unwrap().unwrap());
        assert_eq!(got.status, IntentStatus::Failed);
        assert!(
            !db.call_blocking(move |d| d.has_open_intent(sub_id).unwrap()),
            "the source wallet's open-intent guard must be released"
        );
        assert_eq!(
            server.methods(),
            vec!["sendTransaction".to_string()],
            "a definitively-rejected transfer must not enter the poll loop"
        );
    }

    #[tokio::test]
    async fn broadcast_confirmation_poll_does_not_block_later_ordered_commands() {
        let (db, sub_id) = storage_with_wallets();
        let to = crate::crypto::derive_address(&test_seed(), 0);
        let intent = db
            .call_blocking(move |d| d.create_intent(sub_id, &to, 1_000, None))
            .unwrap();
        let sig = "Sig1111111111111111111111111111111111111111111";
        db.call_blocking(move |d| {
            d.mark_signed(intent.id, sig, "bh", 1000, 5000, b"wire")
                .unwrap()
        });

        let server = RawMockServer::new(vec![
            json!({"jsonrpc":"2.0","id":1,"result": sig}).to_string(),
        ]);
        let rpc = Arc::new(Mutex::new(Rpc::new(
            reqwest::Client::new(),
            server.url.clone(),
        )));
        let (_, price, client) = worker_deps();
        let (cmd_tx, cmd_rx) = mpsc::channel::<(u64, Command)>(64);
        let (evt_tx, mut evt_rx) = mpsc::channel(64);
        let generation = Arc::new(AtomicU64::new(0));
        let handle = spawn_workers(cmd_rx, evt_tx, db.clone(), rpc, price, client, generation);

        cmd_tx
            .send((
                0,
                Command::Broadcast {
                    intent_id: intent.id,
                },
            ))
            .await
            .unwrap();
        cmd_tx
            .send((
                0,
                Command::PersistSetting {
                    change: SettingChange::AutoLock(9),
                },
            ))
            .await
            .unwrap();

        let persisted = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match evt_rx.recv().await {
                    Some(AppEvent::SettingPersisted { change, result, .. }) => {
                        break (change, result);
                    }
                    Some(_) => continue,
                    None => panic!("worker event channel closed unexpectedly"),
                }
            }
        })
        .await
        .expect(
            "PersistSetting must be serviced while the broadcast confirmation poll runs concurrently — a poll still on the ordered task would stall every later command for minutes",
        );

        assert_eq!(persisted.0, SettingChange::AutoLock(9));
        persisted.1.unwrap();

        drop(cmd_tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    #[tokio::test(start_paused = true)]
    async fn broadcast_confirmation_poll_terminates_and_expires_after_bounded_rounds() {
        let (db, sub_id) = storage_with_wallets();
        let to = crate::crypto::derive_address(&test_seed(), 0);
        let intent = db
            .call_blocking(move |d| d.create_intent(sub_id, &to, 1_000, None))
            .unwrap();
        let sig = "Sig1111111111111111111111111111111111111111111";
        db.call_blocking(move |d| {
            d.mark_signed(intent.id, sig, "bh", 1000, 5000, b"wire")
                .unwrap()
        });
        assert!(db.call_blocking(move |d| d.has_open_intent(sub_id).unwrap()));

        let server = RawMockServer::new(vec![
            json!({"jsonrpc":"2.0","id":1,"result": sig}).to_string(),
        ]);
        let rpc = Arc::new(Mutex::new(Rpc::new(
            reqwest::Client::new(),
            server.url.clone(),
        )));
        let (evt_tx, mut evt_rx) = mpsc::channel(64);
        let generation = Arc::new(AtomicU64::new(0));

        broadcast_and_poll(intent.id, db.clone(), rpc, evt_tx, generation, 0).await;

        let mut outcomes = Vec::new();
        while let Ok(ev) = evt_rx.try_recv() {
            if let AppEvent::TransferResult { outcome, .. } = ev {
                outcomes.push(outcome);
            }
        }
        assert!(
            matches!(outcomes.first(), Some(TransferOutcome::Submitted { .. })),
            "the first outcome should be Submitted, got {outcomes:?}"
        );
        assert!(
            outcomes
                .iter()
                .any(|o| matches!(o, TransferOutcome::StillPending { .. })),
            "a StillPending heartbeat should be emitted while polling, got {outcomes:?}"
        );
        assert!(
            matches!(outcomes.last(), Some(TransferOutcome::Expired)),
            "the poll must finalize Expired once the round bound is hit, got {outcomes:?}"
        );

        let got = db.call_blocking(move |d| d.get_intent(intent.id).unwrap().unwrap());
        assert_eq!(got.status, IntentStatus::Expired);
        assert!(
            !db.call_blocking(move |d| d.has_open_intent(sub_id).unwrap()),
            "a bounded-out transfer must release the source wallet's open-intent guard"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn broadcast_confirmation_poll_keeps_confirmed_unfinalized_transfer_open() {
        let (db, sub_id) = storage_with_wallets();
        let to = crate::crypto::derive_address(&test_seed(), 0);
        let intent = db
            .call_blocking(move |d| d.create_intent(sub_id, &to, 1_000, None))
            .unwrap();
        let sig = "Sig1111111111111111111111111111111111111111111";
        db.call_blocking(move |d| {
            d.mark_signed(intent.id, sig, "bh", 1000, 5000, b"wire")
                .unwrap()
        });

        let server = RawMockServer::routed(vec![
            (
                "sendTransaction",
                json!({"jsonrpc":"2.0","id":1,"result": sig}).to_string(),
            ),
            (
                "getSignatureStatuses",
                json!({"jsonrpc":"2.0","id":1,"result":{"context":{"slot":1},"value":[
                    {"slot":1,"confirmations":null,"err":null,"confirmationStatus":"confirmed"}
                ]}})
                .to_string(),
            ),
        ]);
        let rpc = Arc::new(Mutex::new(Rpc::new(
            reqwest::Client::new(),
            server.url.clone(),
        )));
        let (evt_tx, mut evt_rx) = mpsc::channel(64);
        let generation = Arc::new(AtomicU64::new(0));

        broadcast_and_poll(intent.id, db.clone(), rpc, evt_tx, generation, 0).await;

        let mut outcomes = Vec::new();
        while let Ok(ev) = evt_rx.try_recv() {
            if let AppEvent::TransferResult { outcome, .. } = ev {
                outcomes.push(outcome);
            }
        }
        assert!(
            !outcomes
                .iter()
                .any(|o| matches!(o, TransferOutcome::Expired)),
            "a confirmed transfer must never be reported Expired (safe to retry), got {outcomes:?}"
        );
        assert!(
            !outcomes
                .iter()
                .any(|o| matches!(o, TransferOutcome::Confirmed { .. })),
            "a transfer that only reached confirmed must not be reported Confirmed, got {outcomes:?}"
        );

        let got = db.call_blocking(move |d| d.get_intent(intent.id).unwrap().unwrap());
        assert_eq!(
            got.status,
            IntentStatus::Submitted,
            "a confirmed-but-unfinalized transfer must stay submitted for reconcile to finalize"
        );
        assert!(
            db.call_blocking(move |d| d.has_open_intent(sub_id).unwrap()),
            "the source wallet's open-intent guard must stay held so the user cannot double-send"
        );
    }

    #[tokio::test]
    async fn rename_profile_command_reports_failure_for_missing_profile() {
        let (db, _sub_id) = storage_with_wallets();
        let (rpc, price, client) = worker_deps();
        let (evt_tx, mut evt_rx) = mpsc::channel(8);
        let generation = Arc::new(AtomicU64::new(0));
        let dir = std::env::temp_dir().join(format!("silo-test-rename-{}", std::process::id()));
        handle_command(
            0,
            Command::RenameProfile {
                config_dir: dir,
                id: "does-not-exist".into(),
                name: "x".into(),
            },
            db,
            rpc,
            evt_tx,
            price,
            client,
            generation,
        )
        .await;
        match evt_rx.try_recv().unwrap() {
            AppEvent::ProfileRenamed { result, .. } => assert!(result.is_err()),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn decide_covers_terminal_and_pending_states() {
        use crate::solana::reconcile::EXPIRY_SLACK;
        assert_eq!(
            decide(Some(&status(false, Some("confirmed"))), None, 100),
            Decision::WaitFinality
        );
        assert_eq!(
            decide(Some(&status(false, Some("finalized"))), None, 100),
            Decision::FinalizeSuccess
        );
        assert_eq!(
            decide(Some(&status(true, Some("confirmed"))), None, 100),
            Decision::Fail
        );
        assert_eq!(
            decide(Some(&status(false, Some("processed"))), None, 100),
            Decision::Rebroadcast
        );
        assert_eq!(
            decide(
                Some(&status(false, Some("processed"))),
                Some(100 + EXPIRY_SLACK + 1),
                100,
            ),
            Decision::Expire
        );
        assert_eq!(
            decide(None, Some(100 + EXPIRY_SLACK), 100),
            Decision::Rebroadcast
        );
        assert_eq!(
            decide(None, Some(100 + EXPIRY_SLACK + 1), 100),
            Decision::Expire
        );
    }
}
