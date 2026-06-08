use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::sync::{Semaphore, mpsc, watch};

use crate::app::{
    AppEvent, ClipboardCopyResult, Command, OptimisticTransfer, PasteTarget, ProfileDeleteResult,
    ProfileOpenedPayload, SendPersistResult, SettingChange, SetupResult, UnlockResult,
    WalletTextField,
};
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
const CONFIRM_MAX_ROUNDS: usize = 3;
const REBROADCAST_INTERVAL: Duration = Duration::from_secs(12);

async fn persist_last_price(db: &Storage, p: &SolPrice) {
    let json = p.to_meta_json();
    db.call(move |d| {
        let _ = d.set_meta("last_price", &json);
    })
    .await;
}

async fn current_currency(db: &Storage) -> crate::types::Currency {
    db.call(|d| d.get_meta("currency").ok().flatten())
        .await
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
                    let currency = current_currency(&db).await;
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
                        persist_last_price(&db, &p).await;
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
                                    if let Some(ctx) = broadcast_submit(
                                        intent_id,
                                        &db,
                                        &rpc,
                                        &evt,
                                        &generation,
                                        cmd_gen,
                                    )
                                    .await
                                    {
                                        let db = db.clone();
                                        let rpc = rpc.clone();
                                        let evt = evt.clone();
                                        let generation = generation.clone();
                                        confirm_tasks.spawn(async move {
                                            poll_confirmation(ctx, db, rpc, evt, generation, cmd_gen)
                                                .await;
                                        });
                                    }
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

fn wallet_consistency(
    db: &Storage,
    seed: &crate::crypto::Seed,
) -> Result<Vec<crate::types::WalletRow>, String> {
    let wallets = db
        .call_blocking(|d| d.list_wallets())
        .map_err(|e| e.to_string())?;
    if wallets
        .iter()
        .all(|w| crate::crypto::derive_address(seed, w.account_index) == w.pubkey)
    {
        Ok(wallets)
    } else {
        Err("wallet mismatch".to_string())
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
    let wallets = match wallet_consistency(&db, &seed) {
        Ok(wallets) => wallets,
        Err(e) if e == "wallet mismatch" => {
            db.call_blocking(|d| {
                let _ = d.audit(AuditEvent::IntegrityCheckFailed, &serde_json::json!({}));
            });
            return UnlockResult::WalletMismatch;
        }
        Err(e) => return UnlockResult::WalletRead(e),
    };
    match db.call_blocking(|d| d.verify_audit_chain()) {
        Ok(true) => {
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
        Err(e) if e == "wallet mismatch" => {
            db.call_blocking(|d| {
                let _ = d.audit(AuditEvent::IntegrityCheckFailed, &serde_json::json!({}));
            });
            return SetupResult::Failed(
                "Existing wallet records don't match this recovery phrase. Refusing to proceed."
                    .to_string(),
            );
        }
        Err(e) => {
            return SetupResult::Failed(format!(
                "Wallet metadata could not be read: {e}. Refusing to proceed."
            ));
        }
    }

    let vault_key = if crate::vault::vault_exists(&vault_path) {
        match crate::vault::unlock_vault_keyed(&vault_path, &passphrase) {
            Ok((existing, key)) if existing.to_string() == mnemonic.to_string() => key,
            Ok(_) => {
                return SetupResult::Failed(
                    "Existing vault does not match this recovery phrase".to_string(),
                );
            }
            Err(e) => return SetupResult::Failed(format!("could not reopen existing vault: {e}")),
        }
    } else {
        match crate::vault::create_vault(&vault_path, &mnemonic, &passphrase) {
            Ok(k) => k,
            Err(e) => return SetupResult::Failed(format!("could not create vault: {e}")),
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
            "Could not initialize the master wallet — please retry".to_string(),
        );
    }

    if let Some(id) = current_profile {
        let profiles = match crate::profiles::load(&config_dir) {
            Ok(profiles) => profiles,
            Err(e) => return SetupResult::Failed(format!("could not load profiles: {e}")),
        };
        let name = next_wallet_name(&profiles);
        if let Err(e) = crate::profiles::register(
            &config_dir,
            crate::profiles::ProfileMeta {
                id,
                name,
                created_at: crate::db::now_ms(),
            },
        ) {
            return SetupResult::Failed(format!("could not register profile: {e}"));
        }
    }

    let wallets = match db.call_blocking(|d| d.list_wallets()) {
        Ok(wallets) => wallets,
        Err(e) => return SetupResult::Failed(format!("Could not load wallets: {e}")),
    };
    let profiles = match crate::profiles::load(&config_dir) {
        Ok(profiles) => profiles,
        Err(e) => return SetupResult::Failed(format!("Could not load profiles: {e}")),
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
        Err(e) => return SendPersistResult::Failed(format!("Could not record transfer: {e}")),
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
                    IntentStatus::Failed,
                    Some("could not persist signed transfer"),
                )
            });
            match cleaned {
                Ok(_) => {
                    SendPersistResult::Failed(format!("Could not persist signed transfer: {e}"))
                }
                Err(_) => SendPersistResult::Failed(format!(
                    "Could not persist signed transfer, and could not clean up the pending \
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
                        ProfileDeleteResult::Failed(format!("Could not delete profile: {e}"))
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
                    .map_err(|e| format!("Could not rename profile: {e}"))
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
            let currency = current_currency(&db).await;
            match fetch_price(&client, currency).await {
                Ok(p) => {
                    if generation.load(Ordering::SeqCst) != cmd_gen {
                        return;
                    }
                    price.set(p);
                    persist_last_price(&db, &p).await;
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
            let wallets: Vec<(i64, String)> = match db.call(|d| d.list_wallets()).await {
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

        Command::LoadWallets => match db.call(|d| d.list_wallets()).await {
            Ok(wallets) => {
                let _ = evt
                    .send(AppEvent::WalletsLoaded {
                        wallets,
                        generation: cmd_gen,
                    })
                    .await;
            }
            Err(e) => send_error(&evt, cmd_gen, format!("could not load wallets: {e}")).await,
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
                        format!("could not load transfer history: {e}"),
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
            Err(e) => send_error(&evt, cmd_gen, format!("could not load audit log: {e}")).await,
        },
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
        Ok(IntentTransitionOutcome::Applied) => status,
        Err(e) => {
            send_error(
                evt,
                cmd_gen,
                format!(
                    "transfer {} on-chain but could not be recorded locally — will reconcile on restart: {e}",
                    status.as_str()
                ),
            )
            .await;
            status
        }
        Ok(_) => match db
            .call_current(generation.clone(), cmd_gen, move |d| {
                d.get_intent(intent_id).ok().flatten().map(|i| i.status)
            })
            .await
        {
            Some(Some(s)) => s,
            Some(None) => status,
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
                format!("could not record submitted transfer: {e}"),
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
                IntentStatus::Failed,
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
                    IntentStatus::Failed,
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
        finalize(
            &db,
            &evt,
            intent_id,
            &sig,
            IntentStatus::Expired,
            Some("confirmation timed out before the network confirmed or rejected it"),
            &generation,
            cmd_gen,
        )
        .await;
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

    struct RawMockServer {
        url: String,
        requests: Arc<Mutex<Vec<serde_json::Value>>>,
        _worker: std::thread::JoinHandle<()>,
    }

    impl RawMockServer {
        fn new(bodies: Vec<String>) -> Self {
            use std::collections::VecDeque;
            use std::io::{Read, Write};
            use std::net::TcpListener;
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let requests = Arc::new(Mutex::new(Vec::new()));
            let requests_t = requests.clone();
            let mut responses = VecDeque::from(bodies);
            let worker = std::thread::spawn(move || {
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
                    let req = String::from_utf8_lossy(&buf);
                    let body = req.split("\r\n\r\n").nth(1).unwrap_or("");
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
                        requests_t.lock().unwrap().push(v);
                    }
                    let resp_body = responses.pop_front().unwrap_or_else(|| "{}".to_string());
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        resp_body.len()
                    );
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(resp_body.as_bytes());
                    if responses.is_empty() {
                        break;
                    }
                }
            });
            RawMockServer {
                url,
                requests,
                _worker: worker,
            }
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
            IntentStatus::Confirmed,
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
                    assert!(message.contains("could not be recorded locally"));
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
