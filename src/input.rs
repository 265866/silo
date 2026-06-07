use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use zeroize::{Zeroize, Zeroizing};

use crate::app::{App, Command, ConfirmAction, Modal, PromptKind, Route, SetupStage};
use crate::clipboard::validate_solana_pubkey;
use crate::crypto;
use crate::solana::tx;
use crate::types::{AuditEvent, Role, RouteError, WalletRow};

const BLOCKHASH_REFRESH_AFTER: std::time::Duration = std::time::Duration::from_secs(45);

pub fn handle_event(app: &mut App, ev: Event) {
    match ev {
        Event::Key(k) if k.kind == KeyEventKind::Press => handle_key(app, k),
        Event::Paste(s) => handle_paste(app, &s),
        _ => {}
    }
}

fn ctrl(k: &KeyEvent, c: char) -> bool {
    k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char(c)
}

fn handle_key(app: &mut App, key: KeyEvent) {
    if ctrl(&key, 'c') {
        app.running = false;
        return;
    }
    if ctrl(&key, 'l') && app.seed.is_some() {
        app.lock();
        app.toast_info("Locked");
        return;
    }
    if app.modal.is_some() {
        modal_keys(app, key);
        return;
    }
    match app.route {
        Route::ProfileSelect => profile_select_keys(app, key),
        Route::Unlock => unlock_keys(app, key),
        Route::Setup => setup_keys(app, key),
        Route::WalletList => wallet_list_keys(app, key),
        Route::WalletDetail => wallet_detail_keys(app, key),
        Route::Send => send_keys(app, key),
        Route::History => back_or_scroll(app, key),
        Route::AuditLog => back_or_scroll(app, key),
        Route::Settings => settings_keys(app, key),
    }
}

fn edit_text(buf: &mut String, key: &KeyEvent) -> bool {
    match key.code {
        KeyCode::Char(c) => {
            buf.push(c);
            true
        }
        KeyCode::Backspace => {
            buf.pop();
            true
        }
        _ => false,
    }
}

fn unlock_keys(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Enter => try_unlock(app),
        _ => {
            edit_text(&mut app.input.passphrase, &key);
        }
    }
}

fn try_unlock(app: &mut App) {
    let result = crate::vault::unlock_vault_keyed(&app.vault_path, &app.input.passphrase);
    app.input.passphrase.zeroize();
    match result {
        Ok((mnemonic, vault_key)) => {
            let seed = crypto::seed_from_mnemonic(&mnemonic);
            drop(mnemonic);
            let key_ok = match app.db.lock() {
                Ok(mut d) => d.unlock_audit_key(vault_key.as_bytes()).is_ok(),
                Err(_) => false,
            };
            drop(vault_key);
            if !key_ok {
                app.modal = Some(Modal::Error {
                    title: "Cannot open audit log".into(),
                    body: "The audit key could not be derived (database/vault inconsistent). \
                           Refusing to operate."
                        .into(),
                });
                return;
            }
            if !consistency_ok(app, &seed) {
                if let Ok(mut d) = app.db.lock() {
                    let _ = d.audit(AuditEvent::IntegrityCheckFailed, &serde_json::json!({}));
                }
                app.modal = Some(Modal::Error {
                    title: "Database / vault mismatch".into(),
                    body: "A stored wallet address does not match this recovery phrase. \
                           Refusing to operate to avoid misrouting funds."
                        .into(),
                });
                return;
            }
            let audit_tampered = match app.db.lock() {
                Ok(d) => d.verify_audit_chain().map(|ok| !ok).unwrap_or(false),
                Err(_) => false,
            };
            if audit_tampered {
                app.toast_err("⚠ audit log integrity check failed — history may be tampered");
            }
            app.seed = Some(seed);
            if let Ok(mut d) = app.db.lock() {
                let _ = d.audit(AuditEvent::VaultUnlocked, &serde_json::json!({}));
            }
            app.route = Route::WalletList;
            app.note_activity();
            app.reload_wallets();
            app.send_cmd(Command::Reconcile);
            app.request_balance_refresh();
            app.toast_ok("Unlocked");
        }
        Err(_) => {
            if let Ok(mut d) = app.db.lock() {
                let _ = d.audit(AuditEvent::VaultUnlockFailed, &serde_json::json!({}));
            }
            app.toast_err("Wrong passphrase");
        }
    }
}

fn consistency_ok(app: &App, seed: &crypto::Seed) -> bool {
    let wallets = {
        match app.db.lock() {
            Ok(d) => d.list_wallets().unwrap_or_default(),
            Err(_) => return false,
        }
    };
    wallets
        .iter()
        .all(|w| crypto::derive_address(seed, w.account_index) == w.pubkey)
}

fn setup_keys(app: &mut App, key: KeyEvent) {
    match app.setup.stage {
        SetupStage::Choose => match key.code {
            KeyCode::Char('c') | KeyCode::Char('C') => start_create(app),
            KeyCode::Char('i') | KeyCode::Char('I') => {
                app.setup.creating = false;
                app.setup.stage = SetupStage::ImportEntry;
            }
            KeyCode::Esc => app.running = false,
            _ => {}
        },
        SetupStage::ShowMnemonic => match key.code {
            KeyCode::Enter => {
                let n = app.setup.mnemonic_words.len();
                app.setup.begin_confirm(n);
                app.setup.stage = SetupStage::ConfirmMnemonic;
            }
            KeyCode::Esc => leave_setup_substage(app),
            _ => {}
        },
        SetupStage::ConfirmMnemonic => confirm_mnemonic_keys(app, key),
        SetupStage::ImportEntry => match key.code {
            KeyCode::Enter => match crypto::parse_mnemonic(&app.input.import_phrase) {
                Ok(_) => {
                    app.setup.stage = SetupStage::SetPassphrase;
                    app.input.focus = 0;
                }
                Err(_) => app.toast_err("Invalid recovery phrase"),
            },
            KeyCode::Esc => leave_setup_substage(app),
            _ => {
                edit_text(&mut app.input.import_phrase, &key);
            }
        },
        SetupStage::SetPassphrase => match key.code {
            KeyCode::Tab => app.input.focus ^= 1,
            KeyCode::Enter => finish_setup(app),
            KeyCode::Esc => leave_setup_substage(app),
            _ => {
                if app.input.focus == 0 {
                    edit_text(&mut app.input.passphrase, &key);
                } else {
                    edit_text(&mut app.input.passphrase2, &key);
                }
            }
        },
    }
}

fn leave_setup_substage(app: &mut App) {
    app.input.import_phrase.zeroize();
    app.input.passphrase.zeroize();
    app.input.passphrase2.zeroize();
    app.setup
        .mnemonic_words
        .iter_mut()
        .for_each(|w| w.zeroize());
    app.setup.mnemonic_words.clear();
    app.setup.scrub_confirm();
    app.input.focus = 0;
    app.setup.stage = SetupStage::Choose;
}

fn confirm_mnemonic_keys(app: &mut App, key: KeyEvent) {
    let n = app.setup.confirm_words.len();
    if n == 0 {
        return;
    }
    match key.code {
        KeyCode::Enter => {
            let typed = Zeroizing::new(app.setup.confirm_words.join(" "));
            let expected = Zeroizing::new(app.setup.mnemonic_words.join(" "));
            if *typed == *expected {
                app.setup.scrub_confirm();
                app.setup.stage = SetupStage::SetPassphrase;
                app.input.focus = 0;
            } else {
                app.toast_err("Phrase doesn't match — check your backup");
            }
        }
        KeyCode::Esc => {
            app.setup.scrub_confirm();
            app.setup.stage = SetupStage::ShowMnemonic;
        }
        KeyCode::Char(c) if c.is_ascii_alphabetic() => {
            let i = app.setup.confirm_focus;
            app.setup.confirm_words[i].push(c.to_ascii_lowercase());
            let prefix = app.setup.confirm_words[i].clone();
            let sugg = crypto::word_suggestions(&prefix);
            if sugg.len() == 1 {
                app.setup.confirm_words[i] = sugg[0].to_string();
                advance_confirm_slot(app);
            }
        }
        KeyCode::Char(' ') | KeyCode::Tab | KeyCode::Right => commit_confirm_slot(app),
        KeyCode::Left => {
            app.setup.confirm_focus = app.setup.confirm_focus.saturating_sub(1);
        }
        KeyCode::Backspace => {
            let i = app.setup.confirm_focus;
            if app.setup.confirm_words[i].is_empty() {
                app.setup.confirm_focus = i.saturating_sub(1);
            } else {
                app.setup.confirm_words[i].pop();
            }
        }
        _ => {}
    }
}

fn commit_confirm_slot(app: &mut App) {
    let i = app.setup.confirm_focus;
    let cur = app.setup.confirm_words[i].clone();
    if !cur.is_empty()
        && let Some(w) = crypto::word_suggestions(&cur).first()
    {
        app.setup.confirm_words[i] = (*w).to_string();
    }
    advance_confirm_slot(app);
}

fn advance_confirm_slot(app: &mut App) {
    let n = app.setup.confirm_words.len();
    if app.setup.confirm_focus + 1 < n {
        app.setup.confirm_focus += 1;
    }
}

fn start_create(app: &mut App) {
    app.setup
        .mnemonic_words
        .iter_mut()
        .for_each(|w| w.zeroize());
    app.setup.mnemonic_words.clear();
    app.input.import_phrase.zeroize();
    match crypto::generate_mnemonic(crypto::WordCount::Twelve) {
        Ok(m) => {
            app.setup.creating = true;
            app.setup.mnemonic_words = m.to_string().split_whitespace().map(String::from).collect();
            app.setup.stage = SetupStage::ShowMnemonic;
        }
        Err(e) => app.toast_err(format!("could not generate phrase: {e}")),
    }
}

fn finish_setup(app: &mut App) {
    if app.input.passphrase.as_str() != app.input.passphrase2.as_str() {
        app.toast_err("Passphrases do not match");
        return;
    }
    if app.input.passphrase.is_empty() {
        app.modal = Some(Modal::Confirm {
            title: "No passphrase".into(),
            body: "Your recovery phrase will be saved with an EMPTY passphrase — anyone with \
                   access to this computer's files could read it. Continue without a passphrase?"
                .into(),
            action: crate::app::ConfirmAction::CreateWithEmptyPassphrase,
        });
        return;
    }
    create_vault_and_finish(app);
}

fn create_vault_and_finish(app: &mut App) {
    let mnemonic = if app.setup.creating {
        crypto::parse_mnemonic(&Zeroizing::new(app.setup.mnemonic_words.join(" ")))
    } else {
        crypto::parse_mnemonic(&app.input.import_phrase)
    };
    let mnemonic = match mnemonic {
        Ok(m) => m,
        Err(e) => {
            app.toast_err(format!("invalid phrase: {e}"));
            return;
        }
    };

    let seed = crypto::seed_from_mnemonic(&mnemonic);
    if !consistency_ok(app, &seed) {
        if let Ok(mut d) = app.db.lock() {
            let _ = d.audit(AuditEvent::IntegrityCheckFailed, &serde_json::json!({}));
        }
        app.modal = Some(Modal::Error {
            title: "Database / phrase mismatch".into(),
            body: "Existing wallet records don't match this recovery phrase. Refusing to proceed."
                .into(),
        });
        return;
    }

    let vault_key =
        match crate::vault::create_vault(&app.vault_path, &mnemonic, &app.input.passphrase) {
            Ok(k) => k,
            Err(e) => {
                app.toast_err(format!("could not create vault: {e}"));
                return;
            }
        };
    drop(mnemonic);

    let master_ok = {
        let master_addr = crypto::derive_address(&seed, 0);
        match app.db.lock() {
            Ok(mut d) => {
                let key_ok = d.unlock_audit_key(vault_key.as_bytes()).is_ok();
                let _ = d.audit(AuditEvent::VaultCreated, &serde_json::json!({}));
                key_ok
                    && match d.master_exists() {
                        Ok(true) => true,
                        Ok(false) => d.insert_wallet(0, Role::Master, &master_addr, None).is_ok(),
                        Err(_) => false,
                    }
            }
            Err(_) => false,
        }
    };
    drop(vault_key);
    if !master_ok {
        app.toast_err("Could not initialize the master wallet — please retry");
        return;
    }

    app.seed = Some(seed);
    app.setup
        .mnemonic_words
        .iter_mut()
        .for_each(|w| w.zeroize());
    app.setup.mnemonic_words.clear();
    app.setup.scrub_confirm();
    app.input.zeroize_secrets();

    if let Some(id) = app.current_profile.clone() {
        let name = next_wallet_name(&app.profiles);
        let _ = crate::profiles::register(
            &app.config_dir,
            crate::profiles::ProfileMeta {
                id,
                name,
                created_at: crate::db::now_ms(),
            },
        );
        app.reload_profiles();
    }

    app.route = Route::WalletList;
    app.note_activity();
    app.reload_wallets();
    app.send_cmd(Command::FetchRentExempt);
    app.send_cmd(Command::FetchPrice);
    app.send_cmd(Command::Reconcile);
    app.request_balance_refresh();
    app.toast_ok("Vault created");
    app.celebrate_center();
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

fn wallet_list_keys(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Char('q') => app.running = false,
        KeyCode::Down | KeyCode::Char('j') => move_selection(app, 1),
        KeyCode::Up | KeyCode::Char('k') => move_selection(app, -1),
        KeyCode::Enter => {
            if app.selected_is_archived_header() {
                app.toggle_archived_expanded();
            } else if let Some(w) = app.selected_wallet() {
                app.focused_wallet = Some(w.id);
                app.route = Route::WalletDetail;
                app.refresh_detail_intents();
                app.arm_hot_refresh();
            }
        }
        KeyCode::Char('n') => derive_subwallet(app),
        KeyCode::Char('s') => start_send_from_selected(app),
        KeyCode::Char('M') => {
            if let Some(w) = app.selected_wallet().cloned() {
                quick_to_master(app, &w);
            }
        }
        KeyCode::Char('F') => {
            if let Some(w) = app.selected_wallet().cloned() {
                quick_fund(app, &w);
            }
        }
        KeyCode::Char('c') => copy_selected_address(app),
        KeyCode::Char('r') => {
            app.request_balance_refresh();
            app.toast_info("Refreshing balances…");
        }
        KeyCode::Char('h') => {
            if let Some(w) = app.selected_wallet() {
                app.focused_wallet = Some(w.id);
                app.route = Route::History;
                app.refresh_detail_intents();
            }
        }
        KeyCode::Char('l') => prompt_for_selected(app, true),
        KeyCode::Char('t') => prompt_for_selected(app, false),
        KeyCode::Char('x') => archive_selected(app),
        KeyCode::Char('a') => {
            app.route = Route::AuditLog;
            app.refresh_audit();
        }
        KeyCode::Char('g') => app.route = Route::Settings,
        KeyCode::Char('*') => app.celebrate_center(),
        _ => {}
    }
}

fn move_selection(app: &mut App, delta: i32) {
    let n = app.wallet_list_rows().len() as i32;
    if n == 0 {
        return;
    }
    let cur = app.list_state.selected().unwrap_or(0) as i32;
    let next = (cur + delta).rem_euclid(n);
    app.list_state.select(Some(next as usize));
}

fn derive_subwallet(app: &mut App) {
    let Some(seed) = app.seed.as_ref() else {
        app.toast_err("Wallet is locked");
        return;
    };
    let idx = {
        match app.db.lock() {
            Ok(d) => d.next_account_index().unwrap_or(1).max(1),
            Err(_) => return,
        }
    };
    let addr = crypto::derive_address(seed, idx);
    let res = {
        app.db
            .lock()
            .ok()
            .map(|mut d| d.insert_wallet(idx, Role::Sub, &addr, None))
    };
    match res {
        Some(Ok(_)) => {
            app.reload_wallets();
            app.request_balance_refresh();
            app.toast_ok(format!("Derived subwallet #{idx}"));
        }
        _ => app.toast_err("Could not derive subwallet"),
    }
}

fn copy_selected_address(app: &mut App) {
    if let Some(w) = app.selected_wallet() {
        let addr = w.pubkey.clone();
        copy_addr(app, &addr);
    }
}

fn copy_text(app: &mut App, text: &str, ok_label: &str) -> bool {
    match app.clip.copy(text) {
        Ok(crate::clipboard::CopyOutcome::Persistent) => {
            app.toast_ok(ok_label.to_string());
            true
        }
        Ok(crate::clipboard::CopyOutcome::NonPersistent) => {
            app.toast_info("Copied (won't persist after exit on this compositor)");
            true
        }
        Err(e) => {
            app.toast_err(format!("Copy failed: {e}"));
            false
        }
    }
}

fn copy_addr(app: &mut App, addr: &str) {
    if copy_text(app, addr, "Copied address") {
        app.arm_hot_refresh();
    }
}

fn copy_selected_txid(app: &mut App) {
    let Some(sel) = app.history_state.selected() else {
        app.toast_info("Select a transfer first (↑/↓)");
        return;
    };
    let Some(intent) = app.detail_intents.get(sel) else {
        return;
    };
    match intent.signature.clone() {
        Some(sig) => {
            copy_text(app, &sig, "Copied transaction id");
        }
        None => app.toast_info("No transaction id yet (not signed)"),
    }
}

fn archive_selected(app: &mut App) {
    let Some(w) = app.selected_wallet() else {
        return;
    };
    if w.role == Role::Master {
        app.toast_err("The master wallet can't be archived");
        return;
    }
    let (id, want) = (w.id, !w.archived);
    let sel = app.list_state.selected().unwrap_or(0);
    let res = {
        match app.db.lock() {
            Ok(mut d) => Some(d.set_archived(id, want)),
            Err(_) => None,
        }
    };
    match res {
        Some(Ok(())) => {
            app.reload_wallets();
            if want {
                app.list_state.select(Some(sel.saturating_sub(1)));
                app.clamp_list_selection();
            } else {
                app.select_wallet_by_id(id);
            }
            app.toast_ok(if want { "Archived" } else { "Unarchived" });
        }
        Some(Err(e)) => app.toast_err(e.to_string()),
        None => app.toast_err("Database busy"),
    }
}

fn prompt_for_selected(app: &mut App, label: bool) {
    if let Some(w) = app.selected_wallet() {
        let id = w.id;
        app.input.prompt_text = if label {
            w.label.clone().unwrap_or_default()
        } else {
            w.note.clone().unwrap_or_default()
        };
        app.modal = Some(Modal::Prompt {
            kind: if label {
                PromptKind::Label(id)
            } else {
                PromptKind::Note(id)
            },
            title: if label {
                "Set label".into()
            } else {
                "Set note".into()
            },
        });
    }
}

fn wallet_detail_keys(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => app.route = Route::WalletList,
        KeyCode::Char('c') => {
            if let Some(w) = app.focused_wallet() {
                let a = w.pubkey.clone();
                copy_addr(app, &a);
            }
        }
        KeyCode::Char('s') => {
            if let Some(w) = app.focused_wallet() {
                let id = w.id;
                begin_send(app, id);
            }
        }
        KeyCode::Char('M') => {
            if let Some(w) = app.focused_wallet().cloned() {
                quick_to_master(app, &w);
            }
        }
        KeyCode::Char('F') => {
            if let Some(w) = app.focused_wallet().cloned() {
                quick_fund(app, &w);
            }
        }
        KeyCode::Char('h') => {
            app.route = Route::History;
            app.refresh_detail_intents();
        }
        _ => {}
    }
}

fn start_send_from_selected(app: &mut App) {
    if let Some(w) = app.selected_wallet() {
        let id = w.id;
        begin_send(app, id);
    }
}

fn begin_send(app: &mut App, from_id: i64) {
    app.focused_wallet = Some(from_id);
    app.route = Route::Send;
    app.input.send_to.clear();
    app.input.send_amount.clear();
    app.input.send_in_fiat = false;
    app.input.focus = 0;
}

fn quick_send(app: &mut App, from_id: i64, to: String) {
    app.focused_wallet = Some(from_id);
    app.route = Route::Send;
    app.input.send_to = to;
    app.input.send_amount.clear();
    app.input.send_in_fiat = false;
    app.input.focus = 1;
}

fn master_of(app: &App) -> Option<&WalletRow> {
    app.wallets.iter().find(|w| w.role == Role::Master)
}

fn quick_to_master(app: &mut App, sub: &WalletRow) {
    if sub.role == Role::Master {
        app.toast_info("Already the master wallet");
        return;
    }
    match master_of(app).map(|m| m.pubkey.clone()) {
        Some(m) => quick_send(app, sub.id, m),
        None => app.toast_err("No master wallet"),
    }
}

fn quick_fund(app: &mut App, sub: &WalletRow) {
    if sub.role == Role::Master {
        app.toast_info("Select a subwallet to fund");
        return;
    }
    match master_of(app).map(|m| m.id) {
        Some(mid) => quick_send(app, mid, sub.pubkey.clone()),
        None => app.toast_err("No master wallet"),
    }
}

fn send_keys(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => app.route = Route::WalletList,
        KeyCode::Tab => app.input.focus ^= 1,
        KeyCode::Enter => prepare_send(app),
        _ if ctrl(&key, 'v') => paste_into_focused_send(app),
        KeyCode::Char('m') if app.input.focus == 1 => fill_max_amount(app),
        KeyCode::Char('a') if app.input.focus == 1 => fill_drain_amount(app),
        KeyCode::Char('c') if app.input.focus == 1 => toggle_send_denom(app),
        _ => {
            if app.input.focus == 0 {
                edit_text(&mut app.input.send_to, &key);
            } else {
                edit_text(&mut app.input.send_amount, &key);
            }
        }
    }
}

fn fill_max_amount(app: &mut App) {
    if let Some(from) = app.focused_wallet()
        && let Some(bal) = from.balance_lamports
    {
        match crate::money::max_send_keep_alive(bal, app.send_fee(), app.rent_exempt_min) {
            Some(max) => {
                app.input.send_in_fiat = false;
                app.input.send_amount = crate::money::format_lamports(max);
            }
            None => app.toast_err("Balance too low to send while staying rent-exempt"),
        }
    }
}

fn fill_drain_amount(app: &mut App) {
    if let Some(from) = app.focused_wallet()
        && let Some(bal) = from.balance_lamports
    {
        match crate::money::max_send_drain(bal, app.send_fee()) {
            Some(max) => {
                app.input.send_in_fiat = false;
                app.input.send_amount = crate::money::format_lamports(max);
            }
            None => app.toast_err("Balance too low to cover the fee"),
        }
    }
}

fn toggle_send_denom(app: &mut App) {
    if !app.input.send_in_fiat && app.price_now().is_none() {
        app.toast_err("No price yet — can't switch to fiat");
        return;
    }
    let lamports = app.compose_lamports().ok();
    app.input.send_in_fiat = !app.input.send_in_fiat;
    match (app.input.send_in_fiat, lamports, app.price_now()) {
        (true, Some(l), Some(p)) => {
            let fiat = crate::money::lamports_to_sol(l) * p.value;
            app.input.send_amount = format!("{fiat:.2}");
        }
        (false, Some(l), _) => {
            app.input.send_amount = crate::money::format_lamports(l);
        }
        _ => {}
    }
}

fn paste_into_focused_send(app: &mut App) {
    match app.clip.paste() {
        Ok(text) => {
            let t = text.trim().to_string();
            if app.input.focus == 0 {
                app.input.send_to = t;
            } else {
                app.input.send_amount = t;
            }
        }
        Err(e) => app.toast_err(format!("Paste failed: {e}")),
    }
}

fn prepare_send(app: &mut App) {
    let Some(from) = app.focused_wallet().cloned() else {
        app.toast_err("No source wallet");
        return;
    };
    let to = match validate_solana_pubkey(&app.input.send_to) {
        Ok(t) => t,
        Err(_) => {
            app.toast_err("Recipient is not a valid Solana address");
            return;
        }
    };
    if let Err(e) = classify_route(&app.wallets, &from, &to) {
        app.toast_err(e.to_string());
        return;
    }
    let lamports = match app.compose_lamports() {
        Ok(0) => {
            app.toast_err("Amount must be greater than zero");
            return;
        }
        Ok(l) => l,
        Err(e) => {
            app.toast_err(format!("Amount: {e}"));
            return;
        }
    };
    if !app.reconcile_done {
        app.toast_err("Still reconciling — sends are disabled");
        return;
    }
    app.toast_info("Preparing transfer…");
    app.send_cmd(Command::PrepareSend {
        from_id: from.id,
        to,
        lamports,
        priority_micro: app.priority_micro,
    });
}

pub fn classify_route(
    wallets: &[WalletRow],
    from: &WalletRow,
    to_addr: &str,
) -> Result<(), RouteError> {
    let to = to_addr.trim();
    if to == from.pubkey {
        return Err(RouteError::SelfSend);
    }
    let dest_role = wallets.iter().find(|w| w.pubkey == to).map(|w| w.role);
    match (from.role, dest_role) {
        (Role::Sub, Some(Role::Sub)) => Err(RouteError::SubToSubForbidden),
        _ => Ok(()),
    }
}

fn execute_send(app: &mut App) {
    let Some(ps) = app.pending_send.take() else {
        return;
    };
    app.modal = None;

    if ps.prepared_at.elapsed() > BLOCKHASH_REFRESH_AFTER {
        app.send_cmd(Command::PrepareSend {
            from_id: ps.from_id,
            to: ps.to.clone(),
            lamports: ps.lamports,
            priority_micro: ps.priority_micro,
        });
        app.toast_info("Blockhash expired — refreshing…");
        return;
    }

    let Some(from) = app.wallets.iter().find(|w| w.id == ps.from_id).cloned() else {
        app.toast_err("Source wallet missing");
        return;
    };

    let (from_bytes, to_bytes, bh_bytes) = match (
        tx::address_to_bytes(&from.pubkey),
        tx::address_to_bytes(&ps.to),
        tx::address_to_bytes(&ps.blockhash),
    ) {
        (Ok(f), Ok(t), Ok(b)) => (f, t, b),
        _ => {
            app.toast_err("Could not decode addresses for signing");
            return;
        }
    };

    let created = match app.db.lock() {
        Ok(mut d) => Some(d.create_intent(from.id, &ps.to, ps.lamports, None)),
        Err(_) => None,
    };
    let intent = match created {
        Some(Ok(i)) => i,
        Some(Err(crate::db::CreateIntentError::WalletHasOpenIntent)) => {
            app.toast_err("This wallet already has a transfer in progress");
            return;
        }
        Some(Err(crate::db::CreateIntentError::Db(e))) => {
            app.toast_err(format!("Could not record transfer: {e}"));
            return;
        }
        None => {
            app.toast_err("Database busy");
            return;
        }
    };

    let priority = (ps.priority_micro > 0).then_some(tx::PriorityFee {
        unit_limit: crate::money::COMPUTE_UNIT_LIMIT,
        micro_lamports_per_cu: ps.priority_micro,
    });
    let message =
        tx::build_transfer_message(&from_bytes, &to_bytes, ps.lamports, &bh_bytes, priority);
    let sig = match app.sign_for(from.account_index, &message) {
        Ok(s) => s.to_bytes(),
        Err(e) => {
            fail_intent(app, intent.id, "signing failed");
            app.toast_err(format!("Signing failed: {e}"));
            return;
        }
    };
    let wire = tx::assemble_tx(&message, &sig);
    let sig_b58 = tx::signature_to_base58(&sig);

    let signed = match app.db.lock() {
        Ok(mut d) => {
            Some(d.mark_signed(intent.id, &sig_b58, &ps.blockhash, ps.lvbh, ps.fee, &wire))
        }
        Err(_) => None,
    };
    match signed {
        Some(Ok(())) => {}
        other => {
            fail_intent(app, intent.id, "could not persist signed transfer");
            let msg = match other {
                Some(Err(e)) => format!("Could not persist signed transfer: {e}"),
                _ => "Database busy".to_string(),
            };
            app.toast_err(msg);
            return;
        }
    }

    app.send_cmd(Command::Broadcast {
        intent_id: intent.id,
    });
    app.route = Route::WalletDetail;
    app.refresh_detail_intents();
    app.toast_info("Signing & broadcasting…");
}

fn fail_intent(app: &App, intent_id: i64, reason: &str) {
    if let Ok(mut d) = app.db.lock() {
        let _ = d.mark_terminal(intent_id, crate::types::IntentStatus::Failed, Some(reason));
    }
}

fn run_confirm_action(app: &mut App, action: ConfirmAction) {
    match action {
        ConfirmAction::CreateWithEmptyPassphrase => create_vault_and_finish(app),
        ConfirmAction::DeleteProfile(id) => delete_profile(app, &id),
    }
}

fn delete_profile(app: &mut App, id: &str) {
    if let Err(e) = crate::profiles::remove(&app.config_dir, id) {
        app.toast_err(format!("Could not delete profile: {e}"));
        return;
    }
    app.reload_profiles();
    if app.profiles.is_empty() {
        app.begin_new_profile();
        app.toast_info("Profile deleted — set up a new wallet");
    } else {
        let first = app.profiles[0].id.clone();
        let _ = app.switch_to_profile(&first);
        app.profile_sel = 0;
        app.route = Route::ProfileSelect;
        app.toast_ok("Profile deleted");
    }
}

fn profile_select_keys(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => app.running = false,
        KeyCode::Down | KeyCode::Char('j') => {
            if !app.profiles.is_empty() {
                app.profile_sel = (app.profile_sel + 1) % app.profiles.len();
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if !app.profiles.is_empty() {
                let n = app.profiles.len();
                app.profile_sel = (app.profile_sel + n - 1) % n;
            }
        }
        KeyCode::Enter => {
            if let Some(p) = app.profiles.get(app.profile_sel) {
                let id = p.id.clone();
                if let Err(e) = app.switch_to_profile(&id) {
                    app.toast_err(format!("Could not open profile: {e}"));
                    return;
                }
                app.route = if crate::vault::vault_exists(&app.vault_path) {
                    Route::Unlock
                } else {
                    Route::Setup
                };
            }
        }
        KeyCode::Char('n') => app.begin_new_profile(),
        KeyCode::Char('d') => {
            if let Some(p) = app.profiles.get(app.profile_sel) {
                app.modal = Some(Modal::Confirm {
                    title: "Delete profile".into(),
                    body: format!(
                        "Permanently delete \"{}\" — its wallet DB and encrypted recovery \
                         phrase. This cannot be undone (recover only from your written phrase).",
                        p.name
                    ),
                    action: ConfirmAction::DeleteProfile(p.id.clone()),
                });
            }
        }
        KeyCode::Char('r') => {
            if let Some(p) = app.profiles.get(app.profile_sel) {
                app.input.prompt_text = p.name.clone();
                app.modal = Some(Modal::Prompt {
                    kind: PromptKind::ProfileName(p.id.clone()),
                    title: "Rename profile".into(),
                });
            }
        }
        _ => {}
    }
}

fn settings_keys(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => app.route = Route::WalletList,
        KeyCode::Char('e') => {
            app.input.prompt_text = app.rpc_url.clone();
            app.modal = Some(Modal::Prompt {
                kind: PromptKind::RpcUrl,
                title: "RPC URL".into(),
            });
        }
        KeyCode::Char('u') => {
            app.currency = app.currency.next();
            if let Ok(mut d) = app.db.lock() {
                let _ = d.set_meta("currency", app.currency.code());
                let _ = d.audit(
                    AuditEvent::SettingsChanged,
                    &serde_json::json!({"currency": app.currency.code()}),
                );
            }
            app.price.clear();
            app.reset_price_baseline();
            app.send_cmd(Command::FetchPrice);
            app.toast_info(format!("Currency: {}", app.currency.label()));
        }
        KeyCode::Char('L') => {
            app.lock();
            app.toast_info("Locked");
        }
        KeyCode::Char('p') => {
            app.priority_micro = crate::money::next_priority_preset(app.priority_micro);
            if let Ok(mut d) = app.db.lock() {
                let _ = d.set_meta("priority_fee_micro", &app.priority_micro.to_string());
                let _ = d.audit(
                    AuditEvent::SettingsChanged,
                    &serde_json::json!({"priority_fee_micro": app.priority_micro}),
                );
            }
            app.toast_info(format!(
                "Priority fee: {} (≈ {} SOL)",
                crate::money::priority_label(app.priority_micro),
                crate::money::format_lamports(crate::money::priority_fee_lamports(
                    app.priority_micro
                ))
            ));
        }
        KeyCode::Char('+') | KeyCode::Char('=') => {
            let m = app.auto_lock_after.as_secs() / 60;
            app.auto_lock_after =
                std::time::Duration::from_secs((m + 1).min(crate::app::AUTO_LOCK_MAX_MINUTES) * 60);
            persist_auto_lock(app);
        }
        KeyCode::Char('-') => {
            let m = app.auto_lock_after.as_secs() / 60;
            app.auto_lock_after = std::time::Duration::from_secs(
                m.saturating_sub(1).max(crate::app::AUTO_LOCK_MIN_MINUTES) * 60,
            );
            persist_auto_lock(app);
        }
        _ => {}
    }
}

fn persist_auto_lock(app: &mut App) {
    let mins = app.auto_lock_after.as_secs() / 60;
    if let Ok(mut d) = app.db.lock() {
        let _ = d.set_meta("auto_lock_minutes", &mins.to_string());
        let _ = d.audit(
            AuditEvent::SettingsChanged,
            &serde_json::json!({"auto_lock_minutes": mins}),
        );
    }
    app.toast_info(format!("Auto-lock after {mins} min"));
}

fn back_or_scroll(app: &mut App, key: KeyEvent) {
    if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
        app.route = Route::WalletList;
        return;
    }
    if app.route == Route::History && matches!(key.code, KeyCode::Char('t')) {
        prompt_for_tx_note(app);
        return;
    }
    if app.route == Route::History && matches!(key.code, KeyCode::Char('c')) {
        copy_selected_txid(app);
        return;
    }
    let delta = match key.code {
        KeyCode::Down | KeyCode::Char('j') => 1,
        KeyCode::Up | KeyCode::Char('k') => -1,
        KeyCode::PageDown => 10,
        KeyCode::PageUp => -10,
        KeyCode::Home => i32::MIN,
        KeyCode::End => i32::MAX,
        _ => return,
    };
    let (len, state) = match app.route {
        Route::History => (app.detail_intents.len(), &mut app.history_state),
        Route::AuditLog => (app.audit.len(), &mut app.audit_state),
        _ => return,
    };
    move_table_selection(state, len, delta);
}

fn prompt_for_tx_note(app: &mut App) {
    let Some(sel) = app.history_state.selected() else {
        app.toast_info("Select a transfer first (↑/↓)");
        return;
    };
    let Some(intent) = app.detail_intents.get(sel) else {
        return;
    };
    let id = intent.id;
    app.input.prompt_text = intent.note.clone().unwrap_or_default();
    app.modal = Some(Modal::Prompt {
        kind: PromptKind::TxNote(id),
        title: "Transfer note".into(),
    });
}

fn move_table_selection(state: &mut ratatui::widgets::TableState, len: usize, delta: i32) {
    if len == 0 {
        state.select(None);
        return;
    }
    let last = len as i64 - 1;
    let next = match state.selected() {
        None => {
            if delta >= 0 {
                0
            } else {
                last
            }
        }
        Some(c) => (c as i64 + delta as i64).clamp(0, last),
    };
    state.select(Some(next as usize));
}

fn modal_keys(app: &mut App, key: KeyEvent) {
    if let Some(Modal::Confirm { action, .. }) = &app.modal {
        let action = action.clone();
        match key.code {
            KeyCode::Enter => {
                app.modal = None;
                run_confirm_action(app, action);
            }
            KeyCode::Esc => {
                app.modal = None;
                app.toast_info("Cancelled");
            }
            _ => {}
        }
        return;
    }
    let kind = match &app.modal {
        Some(Modal::ConfirmSend) => 0,
        Some(Modal::Error { .. }) => 1,
        Some(Modal::Prompt { .. }) => 2,
        Some(Modal::Confirm { .. }) => return,
        None => return,
    };
    match kind {
        0 => match key.code {
            KeyCode::Enter => {
                if app.pending_send_is_large() && !app.send_confirm_armed {
                    app.send_confirm_armed = true;
                    app.toast_info("Large send (≈90%+ of balance) — press Enter again to confirm");
                } else {
                    execute_send(app);
                }
            }
            KeyCode::Esc => {
                app.modal = None;
                app.pending_send = None;
                app.send_confirm_armed = false;
                app.toast_info("Send cancelled");
            }
            _ => {}
        },
        1 => {
            if matches!(key.code, KeyCode::Enter | KeyCode::Esc) {
                app.modal = None;
            }
        }
        2 => {
            let multiline = matches!(
                &app.modal,
                Some(Modal::Prompt { kind, .. }) if kind.multiline()
            );
            if ctrl(&key, 's') {
                apply_prompt(app);
            } else {
                match key.code {
                    KeyCode::Enter if multiline => app.input.prompt_text.push('\n'),
                    KeyCode::Enter => apply_prompt(app),
                    KeyCode::Esc => {
                        app.modal = None;
                        app.input.prompt_text.clear();
                    }
                    _ => {
                        edit_text(&mut app.input.prompt_text, &key);
                    }
                }
            }
        }
        _ => {}
    }
}

fn apply_prompt(app: &mut App) {
    let text = app.input.prompt_text.trim().to_string();
    let value = if text.is_empty() {
        None
    } else {
        Some(text.clone())
    };
    let kind = match &app.modal {
        Some(Modal::Prompt { kind, .. }) => Some(kind.clone()),
        _ => None,
    };
    match kind {
        Some(PromptKind::Label(id)) => {
            if let Ok(mut d) = app.db.lock() {
                let _ = d.set_label(id, value.as_deref());
            }
            app.reload_wallets();
            app.toast_ok("Label updated");
        }
        Some(PromptKind::Note(id)) => {
            if let Ok(mut d) = app.db.lock() {
                let _ = d.set_note(id, value.as_deref());
            }
            app.reload_wallets();
            app.toast_ok("Note updated");
        }
        Some(PromptKind::TxNote(id)) => {
            if let Ok(mut d) = app.db.lock() {
                let _ = d.set_intent_note(id, value.as_deref());
            }
            app.refresh_detail_intents();
            app.toast_ok("Transfer note updated");
        }
        Some(PromptKind::RpcUrl) => {
            if let Some(url) = value {
                let url = match crate::solana::rpc::validate_rpc_url(&url) {
                    Ok(url) => url,
                    Err(e) => {
                        app.toast_err(format!("Invalid RPC URL: {e}"));
                        app.modal = None;
                        app.input.prompt_text.clear();
                        return;
                    }
                };
                app.rpc_url = url.clone();
                app.net_status = crate::types::NetStatus::Syncing;
                app.reconcile_done = false;
                app.send_cmd(Command::ChangeRpc { url });
                app.toast_info("RPC updated — reconciling");
            }
        }
        Some(PromptKind::ProfileName(id)) => {
            let name = value.unwrap_or_else(|| "Wallet".to_string());
            let _ = crate::profiles::rename(&app.config_dir, &id, &name);
            app.reload_profiles();
            app.toast_ok("Renamed");
        }
        None => {}
    }
    app.modal = None;
    app.input.prompt_text.clear();
}

fn handle_paste(app: &mut App, text: &str) {
    let t = text.trim();
    if app.modal.is_some() {
        if let Some(Modal::Prompt { .. }) = &app.modal {
            app.input.prompt_text.push_str(t);
        }
        return;
    }
    match app.route {
        Route::Send => {
            if app.input.focus == 0 {
                app.input.send_to.push_str(t);
            } else {
                app.input.send_amount.push_str(t);
            }
        }
        Route::Setup if app.setup.stage == SetupStage::ImportEntry => {
            app.input.import_phrase.push_str(t);
        }
        Route::Unlock => app.input.passphrase.push_str(t),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(id: i64, idx: u32, role: Role, pk: &str) -> WalletRow {
        WalletRow {
            id,
            account_index: idx,
            role,
            pubkey: pk.to_string(),
            label: None,
            note: None,
            archived: false,
            created_at: 0,
            balance_lamports: None,
            has_open_intent: false,
        }
    }

    #[test]
    fn routing_truth_table() {
        let master = w(1, 0, Role::Master, "MASTER");
        let sub1 = w(2, 1, Role::Sub, "SUB1");
        let sub2 = w(3, 2, Role::Sub, "SUB2");
        let wallets = vec![master.clone(), sub1.clone(), sub2.clone()];

        assert!(
            classify_route(&wallets, &master, "SUB1").is_ok(),
            "master->sub"
        );
        assert!(
            classify_route(&wallets, &sub1, "MASTER").is_ok(),
            "sub->master"
        );
        assert!(
            classify_route(&wallets, &sub1, "EXTERNAL_ADDRESS").is_ok(),
            "sub->external"
        );
        assert!(
            classify_route(&wallets, &master, "EXTERNAL_ADDRESS").is_ok(),
            "master->external"
        );

        assert_eq!(
            classify_route(&wallets, &sub1, "SUB2"),
            Err(RouteError::SubToSubForbidden),
            "sub->sub must be blocked"
        );
        assert_eq!(
            classify_route(&wallets, &sub1, "SUB1"),
            Err(RouteError::SelfSend),
            "self-send must be blocked"
        );
        assert_eq!(
            classify_route(&wallets, &master, "MASTER"),
            Err(RouteError::SelfSend),
            "master self-send must be blocked"
        );
    }
}
