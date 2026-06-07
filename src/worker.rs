use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::sync::{Semaphore, mpsc, watch};

use crate::app::{
    AppEvent, ClipboardCopyResult, Command, PasteTarget, ProfileDeleteResult, ProfileOpenedPayload,
    SendPersistResult, SettingChange, SetupResult, UnlockResult, WalletTextField,
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
    let wallets = db.with(|d| d.list_wallets()).map_err(|e| e.to_string())?;
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
            db.with_mut(|d| {
                let _ = d.audit(AuditEvent::VaultUnlockFailed, &serde_json::json!({}));
            });
            return UnlockResult::WrongPassphrase;
        }
    };
    let seed = crate::crypto::seed_from_mnemonic(&mnemonic);
    drop(mnemonic);
    let key_ok = db.with_mut(|d| d.unlock_audit_key(vault_key.as_bytes()).is_ok());
    drop(vault_key);
    if !key_ok {
        return UnlockResult::AuditKey;
    }
    let wallets = match wallet_consistency(&db, &seed) {
        Ok(wallets) => wallets,
        Err(e) if e == "wallet mismatch" => {
            db.with_mut(|d| {
                let _ = d.audit(AuditEvent::IntegrityCheckFailed, &serde_json::json!({}));
            });
            return UnlockResult::WalletMismatch;
        }
        Err(e) => return UnlockResult::WalletRead(e),
    };
    match db.with(|d| d.verify_audit_chain()) {
        Ok(true) => {
            db.with_mut(|d| {
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
            db.with_mut(|d| {
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
        db.with_mut(|d| {
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
    drop(vault_key);
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

    let wallets = match db.with(|d| d.list_wallets()) {
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
    let created = db.with_mut(|d| d.create_intent(from.id, &pending.to, pending.lamports, None));
    let intent = match created {
        Ok(intent) => intent,
        Err(crate::db::CreateIntentError::WalletHasOpenIntent) => {
            return SendPersistResult::Failed(
                "This wallet already has a transfer in progress".to_string(),
            );
        }
        Err(e) => return SendPersistResult::Failed(format!("Could not record transfer: {e}")),
    };
    let signed = db.with_mut(|d| {
        d.mark_signed(
            intent.id,
            &sig_b58,
            &pending.blockhash,
            pending.lvbh,
            pending.fee,
            &wire,
        )
    });
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
            db.with_mut(|d| {
                let _ = d.mark_terminal(
                    intent.id,
                    IntentStatus::Failed,
                    Some("could not persist signed transfer"),
                );
            });
            SendPersistResult::Failed(format!("Could not persist signed transfer: {e}"))
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
            let db = db.clone();
            let generation = generation.clone();
            let outcome = tokio::task::spawn_blocking(move || {
                db.with_current(&generation, cmd_gen, |d| -> anyhow::Result<_> {
                    d.set_archived(id, want)?;
                    d.list_wallets()
                })
            })
            .await
            .unwrap_or_else(|e| Some(Err(anyhow::anyhow!("archive task failed: {e}"))));
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
            let db = db.clone();
            let generation = generation.clone();
            let outcome = tokio::task::spawn_blocking(move || {
                db.with_current(&generation, cmd_gen, |d| -> anyhow::Result<_> {
                    let idx = d.next_account_index().unwrap_or(1).max(1);
                    let addr = crate::crypto::derive_address(&seed, idx);
                    d.insert_wallet(idx, crate::types::Role::Sub, &addr, None)?;
                    Ok((idx, d.list_wallets()?))
                })
            })
            .await
            .unwrap_or_else(|e| Some(Err(anyhow::anyhow!("derive task failed: {e}"))));
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
            let db = db.clone();
            let generation = generation.clone();
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
            let outcome = tokio::task::spawn_blocking(move || {
                db.with_current(&generation, cmd_gen, |d| {
                    d.set_meta_audited(key, &value, AuditEvent::SettingsChanged, &details)
                })
            })
            .await
            .unwrap_or_else(|e| Some(Err(anyhow::anyhow!("setting task failed: {e}"))));
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
            let db = db.clone();
            let generation = generation.clone();
            let outcome = tokio::task::spawn_blocking(move || {
                db.with_current(&generation, cmd_gen, |d| -> anyhow::Result<_> {
                    match field {
                        WalletTextField::Label => d.set_label(id, value.as_deref())?,
                        WalletTextField::Note => d.set_note(id, value.as_deref())?,
                    }
                    d.list_wallets()
                })
            })
            .await
            .unwrap_or_else(|e| Some(Err(anyhow::anyhow!("wallet text task failed: {e}"))));
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
            let db = db.clone();
            let generation = generation.clone();
            let outcome = tokio::task::spawn_blocking(move || {
                db.with_current(&generation, cmd_gen, |d| -> anyhow::Result<_> {
                    d.set_intent_note(id, value.as_deref())?;
                    d.list_intents_for_wallet(wallet_id, 50)
                })
            })
            .await
            .unwrap_or_else(|e| Some(Err(anyhow::anyhow!("intent note task failed: {e}"))));
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
        Ok(_) => match db.with_current(generation, cmd_gen, |d| {
            d.get_intent(intent_id).ok().flatten().map(|i| i.status)
        }) {
            Some(Some(s)) => s,
            Some(None) => status,
            None => return,
        },
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
        let shared_before = db.with(|d| d.list_wallets().unwrap().len());

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
        assert_eq!(db.with(|d| d.list_wallets().unwrap().len()), shared_before);
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
        let shared_before = db.with(|d| d.list_wallets().unwrap().len());

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
        assert_eq!(db.with(|d| d.list_wallets().unwrap().len()), shared_before);
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
            db.with(|d| d.list_wallets())
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
            !db.with(|d| d.list_wallets())
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
        let before = db.with(|d| d.list_wallets()).unwrap().len();
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
            db.with(|d| d.get_meta("auto_lock_minutes")).unwrap(),
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
            .with_mut(|d| d.create_intent(sub_id, &to, 1_000, None))
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
            .with_mut(|d| d.create_intent(sub_id, &to, 1_000, None))
            .unwrap();
        db.with_mut(|d| d.lock_audit_key());
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
