use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use zeroize::{Zeroize, Zeroizing};

use crate::app::{App, Command, ConfirmAction, Modal, PromptKind, Route, SetupStage};
use crate::clipboard::validate_solana_pubkey;
use crate::crypto;
use crate::solana::tx;
use crate::types::{Role, RouteError, WalletRow};

pub(crate) const BLOCKHASH_REFRESH_AFTER: std::time::Duration = std::time::Duration::from_secs(45);

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
            if edit_text(&mut app.input.passphrase, &key) {
                app.unlock_failed = false;
            }
        }
    }
}

fn try_unlock(app: &mut App) {
    if app.blocking_input {
        app.toast_info("Unlock already in progress");
        return;
    }
    let passphrase = std::mem::replace(&mut app.input.passphrase, Zeroizing::new(String::new()));
    if app.send_cmd(Command::UnlockVault {
        vault_path: app.vault_path.clone(),
        passphrase,
    }) {
        app.blocking_input = true;
        app.toast_info("Unlocking…");
    }
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
        SetupStage::ImportEntry if ctrl(&key, 'v') => {
            app.send_cmd(Command::ClipboardPaste {
                target: crate::app::PasteTarget::ImportPhrase,
            });
        }
        SetupStage::ImportEntry => match key.code {
            KeyCode::Enter => match crypto::parse_mnemonic(&app.input.import_phrase) {
                Ok(_) => {
                    app.setup.stage = SetupStage::SetPassphrase;
                    app.input.focus = 0;
                }
                Err(_) => app.toast_err(import_phrase_error(&app.input.import_phrase)),
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
            } else if let Some(i) = app.setup.first_confirm_mismatch(&app.setup.mnemonic_words) {
                app.setup.confirm_mismatch = Some(i);
                app.setup.confirm_focus = i;
                app.toast_err(format!("word {} doesn't match", i + 1));
            } else {
                app.toast_err("Recovery phrase doesn't match — check your backup");
            }
        }
        KeyCode::Esc => {
            app.setup.scrub_confirm();
            app.setup.stage = SetupStage::ShowMnemonic;
        }
        KeyCode::Char(c) if c.is_ascii_alphabetic() => {
            let i = app.setup.confirm_focus;
            app.setup.confirm_mismatch = None;
            app.setup.confirm_words[i].push(c.to_ascii_lowercase());
            let prefix = app.setup.confirm_words[i].clone();
            let sugg = crypto::word_suggestions(&prefix);
            if sugg.len() == 1 {
                app.setup.confirm_words[i] = sugg[0].to_string();
                advance_confirm_slot(app);
            }
        }
        KeyCode::Char(' ') | KeyCode::Tab => commit_confirm_slot(app),
        KeyCode::Left => {
            app.setup.confirm_focus = app.setup.confirm_focus.saturating_sub(1);
        }
        KeyCode::Right => {
            advance_confirm_slot(app);
        }
        KeyCode::Backspace => {
            let i = app.setup.confirm_focus;
            app.setup.confirm_mismatch = None;
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
    app.setup.confirm_mismatch = None;
    let cur = app.setup.confirm_words[i].clone();
    if crypto::word_is_valid(&cur) {
        advance_confirm_slot(app);
        return;
    }
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
            let phrase = Zeroizing::new(m.to_string());
            app.setup.mnemonic_words = phrase.split_whitespace().map(String::from).collect();
            app.setup.stage = SetupStage::ShowMnemonic;
        }
        Err(e) => app.toast_err(format!("Couldn't generate recovery phrase: {e}")),
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
                   access to this computer's files could read it. Press y to continue without a \
                   passphrase, or Enter/Esc to go back and set one."
                .into(),
            action: crate::app::ConfirmAction::CreateWithEmptyPassphrase,
        });
        return;
    }
    create_vault_and_finish(app);
}

fn create_vault_and_finish(app: &mut App) {
    if app.blocking_input {
        app.toast_info("Setup already in progress");
        return;
    }
    let phrase = if app.setup.creating {
        Zeroizing::new(app.setup.mnemonic_words.join(" "))
    } else {
        Zeroizing::new(app.input.import_phrase.to_string())
    };
    if let Err(e) = crypto::parse_mnemonic(&phrase) {
        app.toast_err(format!("Invalid recovery phrase: {e}"));
        return;
    }
    let passphrase = std::mem::replace(&mut app.input.passphrase, Zeroizing::new(String::new()));
    app.input.passphrase2.zeroize();
    app.input.import_phrase.zeroize();
    if app.send_cmd(Command::FinishSetup {
        vault_path: app.vault_path.clone(),
        config_dir: app.config_dir.clone(),
        current_profile: app.current_profile.clone(),
        creating: app.setup.creating,
        phrase,
        passphrase,
    }) {
        app.blocking_input = true;
        app.toast_info(if app.setup.creating {
            "Creating wallet…"
        } else {
            "Importing wallet…"
        });
    }
}

const VALID_WORD_COUNTS: [usize; 5] = [12, 15, 18, 21, 24];

fn import_phrase_error(phrase: &str) -> String {
    let words: Vec<&str> = phrase.split_whitespace().collect();
    let count = words.len();
    if count == 0 {
        return "Enter your recovery phrase".into();
    }
    if !VALID_WORD_COUNTS.contains(&count) {
        return format!("Recovery phrase has {count} words — expected 12 or 24");
    }
    if let Some(pos) = words
        .iter()
        .position(|w| !crypto::word_is_valid(&w.to_ascii_lowercase()))
    {
        return format!("Word {} ('{}') is not a valid word", pos + 1, words[pos]);
    }
    "Checksum failed — re-check the word order".into()
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
        KeyCode::Char('U') => copy_upgrade_command(app),
        KeyCode::Char('*') => app.celebrate_center(),
        _ => {}
    }
}

fn move_selection(app: &mut App, delta: i32) {
    let n = app.wallet_list_len() as i32;
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
    let seed = seed.clone();
    if app.send_cmd(Command::DeriveSubwallet { seed }) {
        app.toast_info("Deriving subwallet…");
    }
}

fn copy_selected_address(app: &mut App) {
    if let Some(w) = app.selected_wallet() {
        let addr = w.pubkey.clone();
        copy_addr(app, &addr);
    }
}

fn copy_text(app: &mut App, text: &str, ok_label: &str, arm_hot_refresh: bool) -> bool {
    app.send_cmd(Command::ClipboardCopy {
        text: text.to_string(),
        ok_label: ok_label.to_string(),
        arm_hot_refresh,
    })
}

fn copy_addr(app: &mut App, addr: &str) {
    copy_text(app, addr, "Copied address", true);
}

fn copy_upgrade_command(app: &mut App) {
    match app.update_available() {
        Some(_) => {
            let hint = app.install_method.upgrade_hint();
            let cmd = hint.strip_prefix("Run: ").unwrap_or(hint).to_string();
            copy_text(app, &cmd, "Upgrade command copied", false);
        }
        None => app.toast_info("You're on the latest version"),
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
            copy_text(app, &sig, "Copied transaction ID", false);
        }
        None => app.toast_info("No transaction ID yet (not signed)"),
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
    if app.send_cmd(Command::ArchiveWallet { id, want }) {
        app.toast_info(if want {
            "Archiving…"
        } else {
            "Unarchiving…"
        });
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
        KeyCode::Char('U') => copy_upgrade_command(app),
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
        KeyCode::Esc => {
            app.preparing_send = false;
            app.route = Route::WalletList;
        }
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
                let amt = crate::money::format_lamports(max);
                app.input.send_amount = amt.clone();
                app.toast_info(format!("Filled max: {amt} SOL (keeps wallet open)"));
            }
            None => app.toast_err("Not enough SOL — accounts must keep a small minimum balance"),
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
                let amt = crate::money::format_lamports(max);
                app.input.send_amount = amt.clone();
                app.toast_info(format!("Filled all: {amt} SOL (empties wallet)"));
            }
            None => app.toast_err("Not enough SOL to cover the fee"),
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
            app.input.send_amount = format!("{fiat:.*}", p.currency.decimals());
        }
        (false, Some(l), _) => {
            app.input.send_amount = crate::money::format_lamports(l);
        }
        _ => {}
    }
}

fn paste_into_focused_send(app: &mut App) {
    let target = if app.input.focus == 0 {
        crate::app::PasteTarget::SendTo
    } else {
        crate::app::PasteTarget::SendAmount
    };
    app.send_cmd(Command::ClipboardPaste { target });
}

fn prepare_send(app: &mut App) {
    if app.preparing_send
        || app.pending_send.is_some()
        || matches!(app.modal, Some(Modal::ConfirmSend))
    {
        return;
    }
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
        app.toast_err("Still syncing transfers — sends are disabled");
        return;
    }
    if app.send_cmd(Command::PrepareSend {
        from_id: from.id,
        to,
        lamports,
        priority_micro: app.priority_micro,
    }) {
        app.preparing_send = true;
        app.toast_info("Preparing transfer…");
    }
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
    if let Ok(bytes) = tx::address_to_bytes(to)
        && tx::is_instruction_program_address(&bytes)
    {
        return Err(RouteError::ProgramAddress);
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
        if app.send_cmd(Command::PrepareSend {
            from_id: ps.from_id,
            to: ps.to.clone(),
            lamports: ps.lamports,
            priority_micro: ps.priority_micro,
        }) {
            app.toast_info("Blockhash expired — refreshing…");
        }
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
            app.toast_err("Couldn't decode addresses for signing");
            return;
        }
    };

    let priority = (ps.priority_micro > 0).then_some(tx::PriorityFee {
        unit_limit: crate::money::COMPUTE_UNIT_LIMIT,
        micro_lamports_per_cu: ps.priority_micro,
    });
    let message = match tx::build_transfer_message(
        &from_bytes,
        &to_bytes,
        ps.lamports,
        &bh_bytes,
        priority,
    ) {
        Ok(m) => m,
        Err(e) => {
            app.toast_err(format!("Cannot build transfer: {e}"));
            return;
        }
    };
    let sig = match app.sign_for(from.account_index, &message) {
        Ok(s) => s.to_bytes(),
        Err(e) => {
            app.toast_err(format!("Signing failed: {e}"));
            return;
        }
    };
    let wire = tx::assemble_tx(&message, &sig);
    let sig_b58 = tx::signature_to_base58(&sig);

    if app.send_cmd(Command::PersistSignedSend {
        pending: ps,
        from,
        wire,
        sig_b58,
    }) {
        app.blocking_input = true;
        app.toast_info("Persisting signed transfer…");
    }
}

fn run_confirm_action(app: &mut App, action: ConfirmAction) {
    match action {
        ConfirmAction::CreateWithEmptyPassphrase => create_vault_and_finish(app),
    }
}

fn delete_profile(app: &mut App, id: &str) {
    if app.blocking_input {
        app.toast_info("Profile operation already in progress");
        return;
    }
    if app.send_cmd(Command::DeleteProfile {
        config_dir: app.config_dir.clone(),
        id: id.to_string(),
    }) {
        app.blocking_input = true;
        app.toast_info("Deleting profile…");
    }
}

fn profile_select_keys(app: &mut App, key: KeyEvent) {
    if app.blocking_input {
        return;
    }
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
                    app.toast_err(format!("Couldn't open profile: {e}"));
                }
            }
        }
        KeyCode::Char('n') => app.begin_new_profile(),
        KeyCode::Char('d') => {
            if let Some(p) = app.profiles.get(app.profile_sel) {
                let challenge = format!("delete {}", p.id);
                app.input.prompt_text.clear();
                app.modal = Some(Modal::Prompt {
                    kind: PromptKind::DeleteProfile {
                        id: p.id.clone(),
                        challenge: challenge.clone(),
                    },
                    title: format!("Type {challenge:?} to delete profile"),
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
            let currency = app.currency.next();
            app.send_cmd(Command::PersistSetting {
                change: crate::app::SettingChange::Currency(currency),
            });
        }
        KeyCode::Char('p') => {
            let priority = crate::money::next_priority_preset(app.priority_micro);
            app.send_cmd(Command::PersistSetting {
                change: crate::app::SettingChange::Priority(priority),
            });
        }
        KeyCode::Char('+') | KeyCode::Char('=') => {
            let m = app.auto_lock_after.as_secs() / 60;
            set_auto_lock_minutes(app, (m + 1).min(crate::app::AUTO_LOCK_MAX_MINUTES));
        }
        KeyCode::Char('-') => {
            let m = app.auto_lock_after.as_secs() / 60;
            set_auto_lock_minutes(
                app,
                m.saturating_sub(1).max(crate::app::AUTO_LOCK_MIN_MINUTES),
            );
        }
        _ => {}
    }
}

fn set_auto_lock_minutes(app: &mut App, mins: u64) {
    app.send_cmd(Command::PersistSetting {
        change: crate::app::SettingChange::AutoLock(mins),
    });
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
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                app.modal = None;
                run_confirm_action(app, action);
            }
            KeyCode::Enter | KeyCode::Esc => {
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
                    app.send_confirm_armed_at = Some(std::time::Instant::now());
                    app.toast_info("Large send (≈90%+ of balance) — press Enter again to confirm");
                } else if app.large_send_confirm_ready() {
                    execute_send(app);
                }
            }
            KeyCode::Esc => {
                app.modal = None;
                app.pending_send = None;
                app.send_confirm_armed = false;
                app.send_confirm_armed_at = None;
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
            app.send_cmd(Command::SetWalletText {
                id,
                field: crate::app::WalletTextField::Label,
                value,
            });
        }
        Some(PromptKind::Note(id)) => {
            app.send_cmd(Command::SetWalletText {
                id,
                field: crate::app::WalletTextField::Note,
                value,
            });
        }
        Some(PromptKind::TxNote(id)) => {
            if let Some(wallet_id) = app.focused_wallet {
                app.send_cmd(Command::SetIntentNote {
                    wallet_id,
                    id,
                    value,
                });
            } else {
                app.toast_err("No wallet selected");
            }
        }
        Some(PromptKind::RpcUrl) => {
            if let Some(url) = value {
                let url = match crate::solana::rpc::validate_rpc_url(&url) {
                    Ok(url) => url,
                    Err(e) => {
                        app.toast_err(format!("Invalid RPC URL: {e}"));
                        return;
                    }
                };
                if app.send_rpc_change_cmd(url) {
                    app.toast_info("Saving RPC URL…");
                }
            }
        }
        Some(PromptKind::ProfileName(id)) => {
            let name = value.unwrap_or_else(|| "Wallet".to_string());
            app.send_cmd(Command::RenameProfile {
                config_dir: app.config_dir.clone(),
                id,
                name,
            });
        }
        Some(PromptKind::DeleteProfile { id, challenge }) => {
            if text != challenge {
                app.toast_err("Profile deletion challenge did not match");
                return;
            }
            delete_profile(app, &id);
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use tokio::sync::mpsc;

    use super::*;
    use crate::app::{AppEvent, PendingSend, ProfileDeleteResult, ProfileOpenedPayload};
    use crate::db::{Db, Storage};
    use crate::price::{PriceCache, PriceSource, SolPrice};
    use crate::profiles::ProfileMeta;
    use crate::solana::rpc::Rpc;

    const TEST_MNEMONIC: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    struct Harness {
        app: App,
        rx: mpsc::Receiver<(u64, Command)>,
    }

    fn harness(seed: bool) -> Harness {
        let db = Db::open_memory().unwrap();
        let storage = Storage::new(db);
        let mnemonic = crypto::parse_mnemonic(TEST_MNEMONIC).unwrap();
        let seed_value = crypto::seed_from_mnemonic(&mnemonic);
        let from_pubkey = crypto::derive_address(&seed_value, 0);
        let wallet = storage
            .call_blocking(move |d| d.insert_wallet(0, Role::Master, &from_pubkey, None))
            .unwrap();
        let (tx, rx) = mpsc::channel::<(u64, Command)>(8);
        let config_dir = std::env::temp_dir().join(format!("silo-test-{}", std::process::id()));
        let mut app = App::new(
            storage,
            Arc::new(PriceCache::default()),
            tx,
            Arc::new(AtomicU64::new(0)),
            Arc::new(std::sync::Mutex::new(Rpc::new(
                reqwest::Client::new(),
                "http://127.0.0.1:8899".to_string(),
            ))),
            reqwest::Client::new(),
            config_dir.clone(),
            "http://127.0.0.1:8899".to_string(),
            config_dir.join("vault"),
        );
        app.wallets = vec![wallet];
        app.route = Route::Send;
        app.modal = Some(Modal::ConfirmSend);
        if seed {
            app.seed = Some(seed_value);
        }
        Harness { app, rx }
    }

    fn pending(from_id: i64, priority_micro: u64, prepared_at: Instant) -> PendingSend {
        PendingSend {
            from_id,
            to: crypto::derive_address(
                &crypto::seed_from_mnemonic(&crypto::parse_mnemonic(TEST_MNEMONIC).unwrap()),
                1,
            ),
            lamports: 1_234_567,
            blockhash: bs58::encode([3u8; 32]).into_string(),
            lvbh: 99_999,
            fee: 7_500,
            dest_balance: 0,
            priority_micro,
            prepared_at,
        }
    }

    fn intents(app: &App) -> Vec<crate::types::Intent> {
        let wallet_id = app.wallets[0].id;
        app.db
            .call_blocking(move |d| d.list_intents_for_wallet(wallet_id, 10))
            .unwrap()
    }

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
    fn failed_unlock_sets_flag_empties_buffer_and_keystroke_clears_it() {
        let mut h = harness(true);
        h.app.route = Route::Unlock;
        h.app.input.passphrase = Zeroizing::new("typed-while-waiting".to_string());

        h.app.apply_app_event(AppEvent::UnlockComplete {
            result: crate::app::UnlockResult::WrongPassphrase,
            generation: h.app.generation.load(Ordering::SeqCst),
        });
        assert!(h.app.unlock_failed, "failed unlock must set the flag");
        assert!(
            h.app.input.passphrase.is_empty(),
            "failed unlock must scrub the passphrase buffer"
        );

        unlock_keys(
            &mut h.app,
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
        );
        assert!(
            !h.app.unlock_failed,
            "the next keystroke must clear the failed-unlock flag"
        );
        assert_eq!(h.app.input.passphrase.as_str(), "x");
    }

    #[test]
    fn successful_unlock_clears_failed_flag() {
        let mut h = harness(true);
        h.app.route = Route::Unlock;
        h.app.unlock_failed = true;
        let seed = crypto::seed_from_mnemonic(&crypto::parse_mnemonic(TEST_MNEMONIC).unwrap());

        h.app.apply_app_event(AppEvent::UnlockComplete {
            result: crate::app::UnlockResult::Unlocked {
                seed,
                wallets: vec![],
            },
            generation: h.app.generation.load(Ordering::SeqCst),
        });
        assert!(
            !h.app.unlock_failed,
            "a successful unlock must clear the failed-unlock flag"
        );
    }

    #[test]
    fn stale_fiat_price_cannot_compose_send_amount() {
        let mut h = harness(true);
        h.app.input.send_in_fiat = true;
        h.app.input.send_amount = "10".into();
        h.app.price.set(SolPrice {
            value: 100.0,
            currency: crate::types::Currency::Usd,
            fetched_at: 0,
            source: PriceSource::CoinGecko,
        });
        assert!(h.app.compose_lamports().is_err());
    }

    #[tokio::test]
    async fn rpc_prompt_invalidates_generation_before_worker_success() {
        let mut h = harness(true);
        h.app.modal = Some(Modal::Prompt {
            kind: PromptKind::RpcUrl,
            title: "RPC URL".into(),
        });
        h.app.input.prompt_text = "https://rpc.example.com".into();

        apply_prompt(&mut h.app);

        assert_eq!(
            h.app.generation.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(h.app.rpc_url, "http://127.0.0.1:8899");
        let (generation, cmd) = h.rx.recv().await.unwrap();
        assert_eq!(generation, 1);
        assert!(matches!(
            cmd,
            Command::ChangeRpc { url } if url == "https://rpc.example.com"
        ));
    }

    #[tokio::test]
    async fn execute_send_queues_signed_persistence_without_inline_storage() {
        let mut h = harness(true);
        let from_id = h.app.wallets[0].id;
        h.app.pending_send = Some(pending(from_id, 0, Instant::now()));

        execute_send(&mut h.app);

        let (_, cmd) = h.rx.recv().await.unwrap();
        match cmd {
            Command::PersistSignedSend {
                pending,
                from,
                wire,
                sig_b58,
            } => {
                assert_eq!(pending.from_id, from_id);
                assert_eq!(from.id, from_id);
                assert!(!sig_b58.is_empty());
                assert!(wire.len() > 64);
                assert!(intents(&h.app).is_empty());
            }
            other => panic!("unexpected command: {other:?}"),
        }
        assert!(h.rx.try_recv().is_err());
        assert_eq!(h.app.route, Route::Send);
    }

    #[tokio::test]
    async fn execute_send_stale_pending_send_refreshes_without_intent() {
        let mut h = harness(true);
        let from_id = h.app.wallets[0].id;
        h.app.pending_send = Some(pending(
            from_id,
            42,
            Instant::now() - Duration::from_secs(46),
        ));

        execute_send(&mut h.app);

        let (_, cmd) = h.rx.recv().await.unwrap();
        match cmd {
            Command::PrepareSend {
                from_id: cmd_from,
                lamports,
                priority_micro,
                ..
            } => {
                assert_eq!(cmd_from, from_id);
                assert_eq!(lamports, 1_234_567);
                assert_eq!(priority_micro, 42);
            }
            other => panic!("unexpected command: {other:?}"),
        }
        assert!(intents(&h.app).is_empty());
    }

    #[tokio::test]
    async fn execute_send_locked_does_not_create_intent_or_broadcast() {
        let mut h = harness(false);
        let from_id = h.app.wallets[0].id;
        h.app.pending_send = Some(pending(from_id, 0, Instant::now()));

        execute_send(&mut h.app);

        assert!(intents(&h.app).is_empty());
        assert!(h.rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn execute_send_preserves_priority_fee_in_persist_command() {
        let mut h = harness(true);
        let from_id = h.app.wallets[0].id;
        let priority_micro = 12_345;
        h.app.pending_send = Some(pending(from_id, priority_micro, Instant::now()));

        execute_send(&mut h.app);

        let (_, cmd) = h.rx.recv().await.unwrap();
        match cmd {
            Command::PersistSignedSend { pending, wire, .. } => {
                assert_eq!(pending.fee, 7_500);
                assert!(wire.windows(8).any(|w| w == priority_micro.to_le_bytes()));
                assert!(
                    wire.windows(4)
                        .any(|w| w == crate::money::COMPUTE_UNIT_LIMIT.to_le_bytes())
                );
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn execute_send_closed_persistence_channel_reports_error_without_storage() {
        let mut h = harness(true);
        drop(h.rx);
        let from_id = h.app.wallets[0].id;
        h.app.pending_send = Some(pending(from_id, 0, Instant::now()));

        execute_send(&mut h.app);

        assert!(intents(&h.app).is_empty());
        assert_eq!(h.app.route, Route::Send);
        assert!(
            h.app
                .toasts
                .iter()
                .any(|t| t.text == "Background worker stopped")
        );
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

    fn sub_address() -> String {
        crypto::derive_address(
            &crypto::seed_from_mnemonic(&crypto::parse_mnemonic(TEST_MNEMONIC).unwrap()),
            1,
        )
    }

    #[tokio::test]
    async fn derive_subwallet_queues_command_without_inline_insert() {
        let mut h = harness(true);
        let before = h.app.db.call_blocking(|d| d.list_wallets()).unwrap().len();
        derive_subwallet(&mut h.app);
        assert!(matches!(
            h.rx.try_recv().unwrap().1,
            Command::DeriveSubwallet { .. }
        ));
        assert_eq!(
            h.app.db.call_blocking(|d| d.list_wallets()).unwrap().len(),
            before
        );
    }

    #[tokio::test]
    async fn derive_subwallet_locked_does_not_queue() {
        let mut h = harness(false);
        derive_subwallet(&mut h.app);
        assert!(h.rx.try_recv().is_err());
    }

    fn meta(id: &str) -> ProfileMeta {
        ProfileMeta {
            id: id.to_string(),
            name: id.to_string(),
            created_at: 0,
        }
    }

    fn fresh_db(wallet_count: usize) -> Db {
        let mut db = Db::open_memory().unwrap();
        let seed = crypto::seed_from_mnemonic(&crypto::parse_mnemonic(TEST_MNEMONIC).unwrap());
        for i in 0..wallet_count {
            let role = if i == 0 { Role::Master } else { Role::Sub };
            db.insert_wallet(
                i as u32,
                role,
                &crypto::derive_address(&seed, i as u32),
                None,
            )
            .unwrap();
        }
        db
    }

    #[tokio::test]
    async fn switch_to_profile_enqueues_open_without_inline_db_swap() {
        let mut h = harness(true);
        let before = h.app.db.call_blocking(|d| d.list_wallets()).unwrap().len();
        h.app.switch_to_profile("00000000000000a1").unwrap();
        let (g, cmd) = h.rx.try_recv().unwrap();
        assert_eq!(g, 1);
        assert!(matches!(cmd, Command::OpenProfile { id, .. } if id == "00000000000000a1"));
        assert_eq!(
            h.app.db.call_blocking(|d| d.list_wallets()).unwrap().len(),
            before
        );
        assert!(h.app.blocking_input);
    }

    #[tokio::test]
    async fn switch_to_profile_rejects_bad_id_synchronously() {
        let mut h = harness(true);
        assert!(h.app.switch_to_profile("../etc/passwd").is_err());
        assert!(h.rx.try_recv().is_err());
        assert_eq!(h.app.generation.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn begin_new_profile_enqueues_create_and_scrubs_secrets() {
        let mut h = harness(true);
        h.app.input.passphrase = Zeroizing::new("secret".to_string());
        h.app.setup.mnemonic_words = vec!["abandon".to_string(), "ability".to_string()];
        h.app.begin_new_profile();
        assert!(h.app.input.passphrase.is_empty());
        assert!(h.app.setup.mnemonic_words.iter().all(|w| w.is_empty()));
        assert!(matches!(
            h.rx.try_recv().unwrap().1,
            Command::CreateProfile { .. }
        ));
        assert!(h.app.blocking_input);
    }

    #[tokio::test]
    async fn profile_opened_switch_installs_db_and_reloads() {
        let mut h = harness(true);
        h.app.generation.store(1, Ordering::SeqCst);
        h.app.blocking_input = true;
        let payload = ProfileOpenedPayload {
            db: fresh_db(2),
            id: "00000000000000ab".to_string(),
            created: false,
        };
        h.app.apply_app_event(AppEvent::ProfileOpened {
            result: Ok(payload),
            generation: 1,
        });
        assert_eq!(h.app.current_profile.as_deref(), Some("00000000000000ab"));
        assert_eq!(
            h.app.db.call_blocking(|d| d.list_wallets()).unwrap().len(),
            2
        );
        assert_eq!(h.app.wallets.len(), 2);
        assert!(!h.app.blocking_input);
        assert_eq!(h.app.route, Route::Setup);
    }

    #[tokio::test]
    async fn profile_opened_created_routes_to_setup() {
        let mut h = harness(true);
        h.app.generation.store(1, Ordering::SeqCst);
        h.app.blocking_input = true;
        h.app.route = Route::ProfileSelect;
        let payload = ProfileOpenedPayload {
            db: fresh_db(0),
            id: "00000000000000cd".to_string(),
            created: true,
        };
        h.app.apply_app_event(AppEvent::ProfileOpened {
            result: Ok(payload),
            generation: 1,
        });
        assert_eq!(h.app.route, Route::Setup);
        assert_eq!(h.app.current_profile.as_deref(), Some("00000000000000cd"));
        assert!(h.app.wallets.is_empty());
        assert!(!h.app.blocking_input);
    }

    #[tokio::test]
    async fn profile_opened_stale_generation_is_ignored() {
        let mut h = harness(true);
        h.app.generation.store(2, Ordering::SeqCst);
        h.app.blocking_input = true;
        let before_profile = h.app.current_profile.clone();
        let before_wallets = h.app.db.call_blocking(|d| d.list_wallets()).unwrap().len();
        let payload = ProfileOpenedPayload {
            db: fresh_db(2),
            id: "00000000000000ef".to_string(),
            created: false,
        };
        h.app.apply_app_event(AppEvent::ProfileOpened {
            result: Ok(payload),
            generation: 1,
        });
        assert_eq!(h.app.current_profile, before_profile);
        assert_eq!(
            h.app.db.call_blocking(|d| d.list_wallets()).unwrap().len(),
            before_wallets
        );
        assert!(h.app.blocking_input);
    }

    #[tokio::test]
    async fn profile_opened_error_toasts_without_swap() {
        let mut h = harness(true);
        h.app.generation.store(1, Ordering::SeqCst);
        h.app.blocking_input = true;
        h.app.pending_profile_open = None;
        let before_profile = h.app.current_profile.clone();
        h.app.apply_app_event(AppEvent::ProfileOpened {
            result: Err("boom".to_string()),
            generation: 1,
        });
        assert_eq!(h.app.current_profile, before_profile);
        assert!(!h.app.blocking_input);
        assert!(h.app.toasts.iter().any(|t| t.text.contains("boom")));
    }

    #[tokio::test]
    async fn profile_deleted_nonempty_enqueues_first_open() {
        let mut h = harness(true);
        h.app.pending_profile_open = None;
        let cur_gen = h.app.generation.load(Ordering::SeqCst);
        h.app.apply_app_event(AppEvent::ProfileDeleted {
            result: ProfileDeleteResult::Deleted {
                profiles: vec![meta("00000000000000a1"), meta("00000000000000a2")],
            },
            generation: cur_gen,
        });
        assert_eq!(h.app.pending_profile_open, Some(0));
        assert!(matches!(
            h.rx.try_recv().unwrap().1,
            Command::OpenProfile { id, .. } if id == "00000000000000a1"
        ));
    }

    #[tokio::test]
    async fn profile_deleted_empty_enqueues_create() {
        let mut h = harness(true);
        let cur_gen = h.app.generation.load(Ordering::SeqCst);
        h.app.apply_app_event(AppEvent::ProfileDeleted {
            result: ProfileDeleteResult::Deleted { profiles: vec![] },
            generation: cur_gen,
        });
        assert!(matches!(
            h.rx.try_recv().unwrap().1,
            Command::CreateProfile { .. }
        ));
        assert_eq!(h.app.pending_profile_open, None);
    }

    #[tokio::test]
    async fn delete_fallback_advances_on_open_error() {
        let mut h = harness(true);
        h.app.profiles = vec![meta("00000000000000b1"), meta("00000000000000b2")];
        h.app.pending_profile_open = Some(0);
        h.app.generation.store(5, Ordering::SeqCst);
        h.app.apply_app_event(AppEvent::ProfileOpened {
            result: Err("io".to_string()),
            generation: 5,
        });
        assert_eq!(h.app.pending_profile_open, Some(1));
        assert!(matches!(
            h.rx.try_recv().unwrap().1,
            Command::OpenProfile { id, .. } if id == "00000000000000b2"
        ));
    }

    #[tokio::test]
    async fn delete_fallback_exhausted_creates_new() {
        let mut h = harness(true);
        h.app.profiles = vec![meta("00000000000000c1")];
        h.app.pending_profile_open = Some(0);
        h.app.generation.store(3, Ordering::SeqCst);
        h.app.apply_app_event(AppEvent::ProfileOpened {
            result: Err("io".to_string()),
            generation: 3,
        });
        assert_eq!(h.app.pending_profile_open, None);
        assert!(matches!(
            h.rx.try_recv().unwrap().1,
            Command::CreateProfile { .. }
        ));
    }

    #[tokio::test]
    async fn switch_to_profile_rolls_back_generation_when_enqueue_fails() {
        let mut h = harness(true);
        drop(h.rx);
        assert!(h.app.switch_to_profile("00000000000000a1").is_err());
        assert_eq!(h.app.generation.load(Ordering::SeqCst), 0);
        assert!(!h.app.blocking_input);
    }

    #[tokio::test]
    async fn begin_new_profile_rolls_back_generation_when_enqueue_fails() {
        let mut h = harness(true);
        drop(h.rx);
        h.app.begin_new_profile();
        assert_eq!(h.app.generation.load(Ordering::SeqCst), 0);
        assert!(!h.app.blocking_input);
    }

    #[tokio::test]
    async fn profile_opened_switch_routes_to_unlock_when_vault_exists() {
        let mut h = harness(true);
        let dir = std::env::temp_dir().join(format!("silo-unlock-test-{}", std::process::id()));
        h.app.config_dir = dir.clone();
        let id = "00000000000000d1";
        let vault_path = crate::profiles::vault_path(&dir, id).unwrap();
        std::fs::create_dir_all(vault_path.parent().unwrap()).unwrap();
        std::fs::write(&vault_path, b"{}").unwrap();
        h.app.generation.store(1, Ordering::SeqCst);
        h.app.blocking_input = true;
        h.app.pending_profile_open = None;
        h.app.apply_app_event(AppEvent::ProfileOpened {
            result: Ok(ProfileOpenedPayload {
                db: fresh_db(1),
                id: id.to_string(),
                created: false,
            }),
            generation: 1,
        });
        assert_eq!(h.app.route, Route::Unlock);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn profile_opened_switch_from_delete_returns_to_profile_select() {
        let mut h = harness(true);
        h.app.generation.store(1, Ordering::SeqCst);
        h.app.blocking_input = true;
        h.app.pending_profile_open = Some(1);
        h.app.apply_app_event(AppEvent::ProfileOpened {
            result: Ok(ProfileOpenedPayload {
                db: fresh_db(1),
                id: "00000000000000a3".to_string(),
                created: false,
            }),
            generation: 1,
        });
        assert_eq!(h.app.route, Route::ProfileSelect);
        assert_eq!(h.app.profile_sel, 1);
        assert_eq!(h.app.pending_profile_open, None);
        assert!(
            h.app
                .toasts
                .iter()
                .any(|t| t.text.contains("Profile deleted"))
        );
    }

    #[tokio::test]
    async fn profile_select_keys_ignored_while_blocking_input() {
        let mut h = harness(true);
        h.app.route = Route::ProfileSelect;
        h.app.profiles = vec![meta("00000000000000e1")];
        h.app.blocking_input = true;
        profile_select_keys(
            &mut h.app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(h.rx.try_recv().is_err());
        assert_eq!(h.app.generation.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn archive_selected_queues_command_without_inline_write() {
        let mut h = harness(true);
        let sub = h
            .app
            .db
            .call_blocking(|d| d.insert_wallet(1, Role::Sub, &sub_address(), None))
            .unwrap();
        h.app.wallets.push(sub.clone());
        h.app.route = Route::WalletList;
        h.app.list_state.select(Some(1));

        archive_selected(&mut h.app);

        match h.rx.try_recv().unwrap().1 {
            Command::ArchiveWallet { id, want } => {
                assert_eq!(id, sub.id);
                assert!(want);
            }
            other => panic!("unexpected command: {other:?}"),
        }
        assert!(
            !h.app
                .db
                .call_blocking(|d| d.list_wallets())
                .unwrap()
                .iter()
                .find(|w| w.id == sub.id)
                .unwrap()
                .archived
        );
    }

    #[test]
    fn set_auto_lock_minutes_queues_persist_setting_without_inline_write() {
        let mut h = harness(true);
        set_auto_lock_minutes(&mut h.app, 7);
        match h.rx.try_recv().unwrap().1 {
            Command::PersistSetting { change } => {
                assert_eq!(change, crate::app::SettingChange::AutoLock(7));
            }
            other => panic!("unexpected command: {other:?}"),
        }
        assert_eq!(
            h.app
                .db
                .call_blocking(|d| d.get_meta("auto_lock_minutes"))
                .unwrap(),
            None
        );
    }

    #[test]
    fn settings_currency_key_queues_persist_setting() {
        let mut h = harness(true);
        h.app.route = Route::Settings;
        let next = h.app.currency.next();
        settings_keys(
            &mut h.app,
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE),
        );
        match h.rx.try_recv().unwrap().1 {
            Command::PersistSetting { change } => {
                assert_eq!(change, crate::app::SettingChange::Currency(next));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn settings_plain_l_does_not_lock_but_ctrl_l_does() {
        let mut h = harness(true);
        h.app.modal = None;
        h.app.route = Route::Settings;
        assert!(h.app.seed.is_some(), "fixture must start unlocked");

        handle_key(
            &mut h.app,
            KeyEvent::new(KeyCode::Char('L'), KeyModifiers::NONE),
        );
        handle_key(
            &mut h.app,
            KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
        );
        assert!(
            h.app.seed.is_some(),
            "plain L/l on settings must not lock; ^L is the only lock key"
        );

        handle_key(
            &mut h.app,
            KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL),
        );
        assert!(h.app.seed.is_none(), "^L must lock from settings");
    }

    #[test]
    fn apply_prompt_label_queues_set_wallet_text_and_closes_modal() {
        let mut h = harness(true);
        let id = h.app.wallets[0].id;
        h.app.modal = Some(Modal::Prompt {
            kind: PromptKind::Label(id),
            title: "Set label".into(),
        });
        h.app.input.prompt_text = "cold".into();

        apply_prompt(&mut h.app);

        match h.rx.try_recv().unwrap().1 {
            Command::SetWalletText {
                id: cid,
                field,
                value,
            } => {
                assert_eq!(cid, id);
                assert_eq!(field, crate::app::WalletTextField::Label);
                assert_eq!(value.as_deref(), Some("cold"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
        assert!(h.app.modal.is_none());
        assert_eq!(
            h.app
                .db
                .call_blocking(|d| d.list_wallets())
                .unwrap()
                .iter()
                .find(|w| w.id == id)
                .unwrap()
                .label,
            None
        );
    }

    #[tokio::test]
    async fn apply_prompt_tx_note_queues_set_intent_note() {
        let mut h = harness(true);
        let wallet_id = h.app.wallets[0].id;
        h.app.focused_wallet = Some(wallet_id);
        h.app.modal = Some(Modal::Prompt {
            kind: PromptKind::TxNote(42),
            title: "Transfer note".into(),
        });
        h.app.input.prompt_text = "memo".into();

        apply_prompt(&mut h.app);

        match h.rx.try_recv().unwrap().1 {
            Command::SetIntentNote {
                wallet_id: wid,
                id,
                value,
            } => {
                assert_eq!(wid, wallet_id);
                assert_eq!(id, 42);
                assert_eq!(value.as_deref(), Some("memo"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn try_unlock_twice_queues_single_command() {
        let mut h = harness(true);
        h.app.route = Route::Unlock;
        h.app.blocking_input = false;
        h.app.input.passphrase = Zeroizing::new("pw".to_string());

        try_unlock(&mut h.app);
        assert!(matches!(
            h.rx.try_recv().unwrap().1,
            Command::UnlockVault { .. }
        ));
        assert!(h.app.blocking_input);

        h.app.input.passphrase = Zeroizing::new("pw".to_string());
        try_unlock(&mut h.app);
        assert!(h.rx.try_recv().is_err(), "duplicate unlock must not queue");
        assert!(
            h.app
                .toasts
                .iter()
                .any(|t| t.text == "Unlock already in progress")
        );
    }

    #[test]
    fn create_vault_and_finish_twice_queues_single_command() {
        let mut h = harness(false);
        h.app.route = Route::Setup;
        h.app.setup.creating = true;
        h.app.setup.mnemonic_words = TEST_MNEMONIC.split_whitespace().map(String::from).collect();
        h.app.input.passphrase = Zeroizing::new("pw".to_string());

        create_vault_and_finish(&mut h.app);
        assert!(matches!(
            h.rx.try_recv().unwrap().1,
            Command::FinishSetup { .. }
        ));
        assert!(h.app.blocking_input);

        create_vault_and_finish(&mut h.app);
        assert!(h.rx.try_recv().is_err(), "duplicate setup must not queue");
        assert!(
            h.app
                .toasts
                .iter()
                .any(|t| t.text == "Setup already in progress")
        );
    }

    #[test]
    fn copy_text_queues_clipboard_copy() {
        let mut h = harness(true);
        assert!(copy_text(&mut h.app, "ADDR", "Copied address", true));
        match h.rx.try_recv().unwrap().1 {
            Command::ClipboardCopy {
                text,
                ok_label,
                arm_hot_refresh,
            } => {
                assert_eq!(text, "ADDR");
                assert_eq!(ok_label, "Copied address");
                assert!(arm_hot_refresh);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn paste_into_focused_send_queues_clipboard_paste() {
        let mut h = harness(true);
        h.app.route = Route::Send;
        h.app.input.focus = 0;
        paste_into_focused_send(&mut h.app);
        match h.rx.try_recv().unwrap().1 {
            Command::ClipboardPaste { target } => {
                assert_eq!(target, crate::app::PasteTarget::SendTo);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn stale_wallet_text_event_is_ignored() {
        let mut h = harness(true);
        let original_len = h.app.wallets.len();
        h.app
            .generation
            .store(5, std::sync::atomic::Ordering::SeqCst);
        h.app.apply_app_event(crate::app::AppEvent::WalletTextSet {
            field: crate::app::WalletTextField::Label,
            result: Ok(vec![w(99, 9, Role::Sub, "GHOST")]),
            generation: 0,
        });
        assert_eq!(h.app.wallets.len(), original_len);
        assert!(h.app.wallets.iter().all(|x| x.pubkey != "GHOST"));
    }

    #[test]
    fn current_setting_persisted_event_applies() {
        let mut h = harness(true);
        h.app
            .generation
            .store(3, std::sync::atomic::Ordering::SeqCst);
        h.app
            .apply_app_event(crate::app::AppEvent::SettingPersisted {
                change: crate::app::SettingChange::AutoLock(7),
                result: Ok(()),
                generation: 3,
            });
        assert_eq!(
            h.app.auto_lock_after,
            std::time::Duration::from_secs(7 * 60)
        );
    }

    #[test]
    fn rpc_change_resets_inflight_counter() {
        let mut h = harness(true);
        h.app.request_balance_refresh();
        assert_eq!(
            h.app.inflight, 1,
            "the queued refresh must be counted in flight"
        );
        let _ = h.rx.try_recv();

        assert!(
            h.app
                .send_rpc_change_cmd("http://127.0.0.1:9999".to_string())
        );
        assert_eq!(
            h.app.inflight, 0,
            "bumping the generation on RPC change must clear the in-flight counter"
        );
    }

    #[test]
    fn stale_balances_after_rpc_change_are_dropped_and_keep_inflight_clear() {
        let mut h = harness(true);
        let wallet_id = h.app.wallets[0].id;
        h.app.request_balance_refresh();
        let _ = h.rx.try_recv();
        assert!(
            h.app
                .send_rpc_change_cmd("http://127.0.0.1:9999".to_string())
        );
        assert_eq!(h.app.inflight, 0);

        h.app.apply_app_event(AppEvent::Balances {
            list: vec![(wallet_id, 5_000)],
            generation: 0,
        });
        assert_eq!(
            h.app.wallets[0].balance_lamports, None,
            "a balances reply from before the RPC change must be ignored"
        );
        assert_eq!(
            h.app.inflight, 0,
            "a stale balances reply must not strand the counter"
        );

        h.app.apply_app_event(AppEvent::BalancesFailed {
            reason: "old generation".to_string(),
            generation: 0,
        });
        assert_eq!(
            h.app.inflight, 0,
            "a stale balances failure must not strand the counter"
        );
    }

    #[test]
    fn auto_refresh_queues_again_after_rpc_change() {
        let mut h = harness(true);
        h.app.route = Route::WalletList;
        h.app.request_balance_refresh();
        let _ = h.rx.try_recv();
        assert!(
            h.app
                .send_rpc_change_cmd("http://127.0.0.1:9999".to_string())
        );
        let _ = h.rx.try_recv();
        assert_eq!(h.app.inflight, 0);

        h.app.last_balance_refresh = Instant::now() - Duration::from_secs(120);
        h.app.maybe_auto_refresh();
        assert_eq!(
            h.app.inflight, 1,
            "with the gate cleared, auto-refresh must queue a fresh balances request"
        );
        assert!(
            matches!(h.rx.try_recv(), Ok((_, Command::RefreshBalances { .. }))),
            "auto-refresh must enqueue a RefreshBalances command"
        );
    }

    #[test]
    fn send_prepared_off_send_route_is_dropped() {
        let mut h = harness(true);
        h.app.route = Route::WalletList;
        h.app.modal = None;
        h.app.pending_send = None;
        let generation = h.app.generation.load(Ordering::SeqCst);
        let from_id = h.app.wallets[0].id;
        h.app.apply_app_event(AppEvent::SendPrepared {
            from_id,
            to: crypto::derive_address(
                &crypto::seed_from_mnemonic(&crypto::parse_mnemonic(TEST_MNEMONIC).unwrap()),
                1,
            ),
            lamports: 1_234_567,
            blockhash: bs58::encode([3u8; 32]).into_string(),
            lvbh: 99_999,
            fee: 7_500,
            dest_balance: 0,
            priority_micro: 0,
            generation,
        });
        assert!(
            h.app.modal.is_none(),
            "a confirm modal must not open once the user has left the Send screen"
        );
        assert!(
            h.app.pending_send.is_none(),
            "a stale SendPrepared off the Send route must not populate pending_send"
        );
    }

    #[test]
    fn send_prepared_on_send_route_opens_confirm_modal() {
        let mut h = harness(true);
        h.app.route = Route::Send;
        h.app.modal = None;
        h.app.pending_send = None;
        h.app.reconcile_done = true;
        h.app.wallets[0].balance_lamports = Some(1_234_567 + 7_500);
        let generation = h.app.generation.load(Ordering::SeqCst);
        let from_id = h.app.wallets[0].id;
        h.app.apply_app_event(AppEvent::SendPrepared {
            from_id,
            to: crypto::derive_address(
                &crypto::seed_from_mnemonic(&crypto::parse_mnemonic(TEST_MNEMONIC).unwrap()),
                1,
            ),
            lamports: 1_234_567,
            blockhash: bs58::encode([3u8; 32]).into_string(),
            lvbh: 99_999,
            fee: 7_500,
            dest_balance: 0,
            priority_micro: 0,
            generation,
        });
        assert!(
            matches!(h.app.modal, Some(Modal::ConfirmSend)),
            "a SendPrepared on the Send route must open the confirm modal"
        );
        assert!(
            h.app.pending_send.is_some(),
            "a SendPrepared on the Send route must populate pending_send"
        );
    }

    #[test]
    fn prepare_send_is_idempotent_while_a_prepare_is_outstanding() {
        let mut h = harness(true);
        h.app.route = Route::Send;
        h.app.modal = None;
        h.app.reconcile_done = true;
        h.app.focused_wallet = Some(h.app.wallets[0].id);
        h.app.input.send_to = crypto::derive_address(
            &crypto::seed_from_mnemonic(&crypto::parse_mnemonic(TEST_MNEMONIC).unwrap()),
            1,
        );
        h.app.input.send_amount = "0.001".to_string();

        prepare_send(&mut h.app);
        assert!(
            h.app.preparing_send,
            "the first prepare must mark a prepare in flight"
        );
        prepare_send(&mut h.app);

        assert!(
            matches!(h.rx.try_recv(), Ok((_, Command::PrepareSend { .. }))),
            "the first prepare must enqueue exactly one PrepareSend"
        );
        assert!(
            h.rx.try_recv().is_err(),
            "a second prepare while one is outstanding must not enqueue another"
        );
    }

    #[test]
    fn prepare_send_blocked_while_confirm_modal_open() {
        let mut h = harness(true);
        h.app.route = Route::Send;
        h.app.reconcile_done = true;
        h.app.focused_wallet = Some(h.app.wallets[0].id);
        h.app.input.send_to = crypto::derive_address(
            &crypto::seed_from_mnemonic(&crypto::parse_mnemonic(TEST_MNEMONIC).unwrap()),
            1,
        );
        h.app.input.send_amount = "0.001".to_string();
        h.app.modal = Some(Modal::ConfirmSend);

        prepare_send(&mut h.app);

        assert!(
            h.rx.try_recv().is_err(),
            "no new prepare may be queued while a confirm modal is already open"
        );
    }

    #[test]
    fn duplicate_send_prepared_does_not_disarm_large_send() {
        let mut h = harness(true);
        h.app.route = Route::Send;
        let from_id = h.app.wallets[0].id;
        let to = crypto::derive_address(
            &crypto::seed_from_mnemonic(&crypto::parse_mnemonic(TEST_MNEMONIC).unwrap()),
            1,
        );
        h.app.wallets[0].balance_lamports = Some(1_300_000);
        h.app.pending_send = Some(crate::app::PendingSend {
            from_id,
            to: to.clone(),
            lamports: 1_234_567,
            blockhash: bs58::encode([3u8; 32]).into_string(),
            lvbh: 99_999,
            fee: 7_500,
            dest_balance: 0,
            priority_micro: 0,
            prepared_at: Instant::now(),
        });
        h.app.modal = Some(Modal::ConfirmSend);
        h.app.send_confirm_armed = true;
        assert!(
            h.app.pending_send_is_large(),
            "test precondition: the pending send must be a large send"
        );
        let generation = h.app.generation.load(Ordering::SeqCst);

        h.app.apply_app_event(AppEvent::SendPrepared {
            from_id,
            to,
            lamports: 1_234_567,
            blockhash: bs58::encode([9u8; 32]).into_string(),
            lvbh: 100_000,
            fee: 7_500,
            dest_balance: 0,
            priority_micro: 0,
            generation,
        });

        assert!(
            h.app.send_confirm_armed,
            "a duplicate SendPrepared reply must not disarm an already-armed large send"
        );
        assert!(matches!(h.app.modal, Some(Modal::ConfirmSend)));
    }

    fn large_pending_modal(h: &mut Harness) {
        let from_id = h.app.wallets[0].id;
        let to = crypto::derive_address(
            &crypto::seed_from_mnemonic(&crypto::parse_mnemonic(TEST_MNEMONIC).unwrap()),
            1,
        );
        h.app.route = Route::Send;
        h.app.wallets[0].balance_lamports = Some(1_300_000);
        h.app.pending_send = Some(crate::app::PendingSend {
            from_id,
            to,
            lamports: 1_234_567,
            blockhash: bs58::encode([3u8; 32]).into_string(),
            lvbh: 99_999,
            fee: 7_500,
            dest_balance: 0,
            priority_micro: 0,
            prepared_at: Instant::now(),
        });
        h.app.modal = Some(Modal::ConfirmSend);
        assert!(
            h.app.pending_send_is_large(),
            "test precondition: the pending send must be a large send"
        );
    }

    #[test]
    fn held_enter_does_not_execute_large_send_before_debounce() {
        let mut h = harness(true);
        large_pending_modal(&mut h);

        modal_keys(
            &mut h.app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(
            h.app.send_confirm_armed,
            "the first Enter arms the large send"
        );
        assert!(
            h.app.send_confirm_armed_at.is_some(),
            "arming must record a timestamp"
        );

        modal_keys(
            &mut h.app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(
            h.rx.try_recv().is_err(),
            "an immediate second Enter (key autorepeat) must not execute the send"
        );
        assert!(
            h.app.pending_send.is_some(),
            "the send must stay pending and merely armed"
        );
        assert!(h.app.send_confirm_armed);
        assert!(matches!(h.app.modal, Some(Modal::ConfirmSend)));
    }

    #[test]
    fn large_send_executes_after_debounce_elapses() {
        let mut h = harness(true);
        large_pending_modal(&mut h);
        h.app.send_confirm_armed = true;
        h.app.send_confirm_armed_at = Some(Instant::now() - Duration::from_millis(600));

        modal_keys(
            &mut h.app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );

        assert!(
            matches!(h.rx.try_recv(), Ok((_, Command::PersistSignedSend { .. }))),
            "a deliberate second Enter after the debounce must execute the send"
        );
        assert!(h.app.pending_send.is_none());
    }

    #[test]
    fn small_send_executes_on_first_enter_without_debounce() {
        let mut h = harness(true);
        let from_id = h.app.wallets[0].id;
        let to = crypto::derive_address(
            &crypto::seed_from_mnemonic(&crypto::parse_mnemonic(TEST_MNEMONIC).unwrap()),
            1,
        );
        h.app.route = Route::Send;
        h.app.wallets[0].balance_lamports = Some(1_000_000_000);
        h.app.pending_send = Some(crate::app::PendingSend {
            from_id,
            to,
            lamports: 1_000,
            blockhash: bs58::encode([3u8; 32]).into_string(),
            lvbh: 99_999,
            fee: 7_500,
            dest_balance: 0,
            priority_micro: 0,
            prepared_at: Instant::now(),
        });
        h.app.modal = Some(Modal::ConfirmSend);
        assert!(
            !h.app.pending_send_is_large(),
            "test precondition: this must not be a large send"
        );

        modal_keys(
            &mut h.app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );

        assert!(
            matches!(h.rx.try_recv(), Ok((_, Command::PersistSignedSend { .. }))),
            "a normal send must execute on the first Enter with no debounce"
        );
    }

    #[test]
    fn classify_route_rejects_program_addresses() {
        let seed = crypto::seed_from_mnemonic(&crypto::parse_mnemonic(TEST_MNEMONIC).unwrap());
        let from = w(1, 0, Role::Master, &crypto::derive_address(&seed, 0));
        let wallets = vec![from.clone()];

        assert_eq!(
            classify_route(&wallets, &from, "11111111111111111111111111111111"),
            Err(RouteError::ProgramAddress),
            "system program address must be rejected as a recipient"
        );
        assert_eq!(
            classify_route(&wallets, &from, tx::COMPUTE_BUDGET_PROGRAM_ID_B58),
            Err(RouteError::ProgramAddress),
            "compute-budget program address must be rejected as a recipient"
        );

        let recipient = crypto::derive_address(&seed, 1);
        assert_eq!(
            classify_route(&wallets, &from, &recipient),
            Ok(()),
            "an ordinary wallet recipient must still be accepted"
        );
    }

    fn setup_confirm_harness(words: &[&str]) -> Harness {
        let mut h = harness(false);
        h.app.modal = None;
        h.app.route = Route::Setup;
        h.app.setup.stage = SetupStage::ConfirmMnemonic;
        h.app.setup.creating = true;
        h.app.setup.mnemonic_words = words.iter().map(|w| w.to_string()).collect();
        h.app.setup.begin_confirm(words.len());
        h
    }

    #[test]
    fn confirm_mismatch_points_at_first_differing_slot() {
        let words = ["abandon", "ability", "able", "about"];
        let mut h = setup_confirm_harness(&words);
        h.app.setup.confirm_words = vec![
            "abandon".into(),
            "ability".into(),
            "zoo".into(),
            "about".into(),
        ];
        h.app.setup.confirm_focus = 3;

        confirm_mnemonic_keys(
            &mut h.app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );

        assert_eq!(h.app.setup.confirm_mismatch, Some(2));
        assert_eq!(h.app.setup.confirm_focus, 2);
        assert!(
            h.app
                .toasts
                .iter()
                .any(|t| t.text == "word 3 doesn't match"),
            "must name the first mismatched word"
        );
    }

    #[test]
    fn confirm_mismatch_clears_on_edit() {
        let words = ["abandon", "ability"];
        let mut h = setup_confirm_harness(&words);
        h.app.setup.confirm_mismatch = Some(1);
        h.app.setup.confirm_focus = 1;

        confirm_mnemonic_keys(
            &mut h.app,
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        );
        assert_eq!(h.app.setup.confirm_mismatch, None);
    }

    #[test]
    fn confirm_right_arrow_does_not_commit_word() {
        let words = ["abandon", "ability"];
        let mut h = setup_confirm_harness(&words);
        h.app.setup.confirm_words = vec!["aban".into(), String::new()];
        h.app.setup.confirm_focus = 0;

        confirm_mnemonic_keys(
            &mut h.app,
            KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
        );

        assert_eq!(
            h.app.setup.confirm_words[0], "aban",
            "Right must not expand or commit the current slot"
        );
        assert_eq!(h.app.setup.confirm_focus, 1, "Right moves focus forward");
    }

    #[test]
    fn confirm_space_commits_exact_valid_word() {
        let words = ["add", "ability"];
        let mut h = setup_confirm_harness(&words);
        h.app.setup.confirm_words = vec!["add".into(), String::new()];
        h.app.setup.confirm_focus = 0;

        confirm_mnemonic_keys(
            &mut h.app,
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
        );

        assert_eq!(
            h.app.setup.confirm_words[0], "add",
            "an exact valid word commits as typed, not expanded to a longer prefix match"
        );
        assert_eq!(h.app.setup.confirm_focus, 1);
    }

    #[test]
    fn empty_passphrase_confirm_defaults_to_safe() {
        let mut h = harness(false);
        h.app.route = Route::Setup;
        h.app.setup.stage = SetupStage::SetPassphrase;
        h.app.setup.creating = true;
        h.app.setup.mnemonic_words = TEST_MNEMONIC.split_whitespace().map(String::from).collect();
        h.app.input.passphrase = Zeroizing::new(String::new());
        h.app.input.passphrase2 = Zeroizing::new(String::new());

        finish_setup(&mut h.app);
        assert!(
            matches!(h.app.modal, Some(Modal::Confirm { .. })),
            "empty passphrase must raise a confirm modal"
        );

        modal_keys(
            &mut h.app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(h.app.modal.is_none(), "Enter dismisses the modal");
        assert!(
            h.rx.try_recv().is_err(),
            "Enter must NOT proceed with an empty passphrase"
        );

        finish_setup(&mut h.app);
        modal_keys(
            &mut h.app,
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
        );
        assert!(
            matches!(h.rx.try_recv(), Ok((_, Command::FinishSetup { .. }))),
            "explicit y proceeds with the empty passphrase"
        );
    }

    #[test]
    fn import_error_names_the_failing_word() {
        assert_eq!(
            import_phrase_error("abandon abandon abandon"),
            "Recovery phrase has 3 words — expected 12 or 24"
        );
        let mut twelve: Vec<&str> = vec!["abandon"; 12];
        twelve[4] = "zzzzz";
        let phrase = twelve.join(" ");
        assert_eq!(
            import_phrase_error(&phrase),
            "Word 5 ('zzzzz') is not a valid word"
        );
        let bad_checksum = ["abandon"; 12].join(" ");
        assert_eq!(
            import_phrase_error(&bad_checksum),
            "Checksum failed — re-check the word order"
        );
    }
}
