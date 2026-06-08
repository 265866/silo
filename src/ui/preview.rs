use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use tokio::sync::mpsc;

use crate::app::{
    App, AppEvent, Command, Modal, OptimisticTransfer, PendingSend, PromptKind, Route, SetupStage,
};
use crate::db::{Db, Storage};
use crate::price::{PriceCache, PriceSource, SolPrice};
use crate::profiles::ProfileMeta;
use crate::types::{Currency, NetStatus, Role, TransferOutcome};

const W: u16 = 96;
const H: u16 = 28;

fn buffer_to_string(buf: &Buffer) -> String {
    let area = buf.area;
    let mut out = String::with_capacity((area.width as usize + 1) * area.height as usize);
    for y in 0..area.height {
        for x in 0..area.width {
            if let Some(c) = buf.cell((x, y)) {
                out.push_str(c.symbol());
            }
        }
        out.push('\n');
    }
    out
}

fn test_app() -> App {
    let mut db = Db::open_memory().unwrap();
    db.insert_wallet(
        0,
        Role::Master,
        "7EdMxnq2k8vT3aLp9wR4cJ6sB1dF5gH7yZ2nQ8mK9kQ",
        Some("Treasury"),
    )
    .unwrap();
    db.insert_wallet(
        1,
        Role::Sub,
        "9aFh2mRcoldStorageXXXXXXXXXXXXXXXXXXXXXXXXXX",
        Some("Cold storage"),
    )
    .unwrap();
    db.insert_wallet(
        2,
        Role::Sub,
        "Bk7q3w9rZ4xN8sP1vT6cL2tradingYYYYYYYYYYYYpV1",
        Some("Trading"),
    )
    .unwrap();
    db.insert_wallet(
        3,
        Role::Sub,
        "Cz2payrollHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHH8nH",
        None,
    )
    .unwrap();

    let db = Storage::new(db);
    let price = Arc::new(PriceCache::new());
    price.set(SolPrice {
        value: 146.20,
        currency: Currency::Usd,
        fetched_at: (crate::db::now_ms() / 1000) as u64,
        source: PriceSource::CoinGecko,
    });
    let (tx, _rx) = mpsc::channel::<(u64, Command)>(16);
    let client = reqwest::Client::new();
    let rpc = Arc::new(Mutex::new(crate::solana::rpc::Rpc::new(
        client.clone(),
        "https://api.mainnet-beta.solana.com",
    )));
    let generation = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let mut app = App::new(
        db,
        price,
        tx,
        generation,
        rpc,
        client,
        PathBuf::from("/tmp/silo-preview"),
        "https://api.mainnet-beta.solana.com".into(),
        PathBuf::from("/tmp/silo-preview/vault.json"),
    );
    app.reconcile_done = true;
    app.net_status = NetStatus::Online;
    app.reload_wallets();
    let balances = [124_500_000_000u64, 0, 12_345_000_000, 0];
    for (w, b) in app.wallets.iter_mut().zip(balances) {
        w.balance_lamports = Some(b);
    }
    if let Some(w) = app.wallets.iter_mut().find(|w| w.account_index == 2) {
        w.has_open_intent = true;
    }
    let trading_id = app
        .wallets
        .iter()
        .find(|w| w.account_index == 2)
        .map(|w| w.id);
    if let Some(id) = trading_id {
        app.db.call_blocking(move |d| {
            let _ = d.set_note(
                id,
                Some(
                    "Day-trading hot wallet — keep under 20 SOL.\nSweep profits to cold storage \
                     weekly; never route sub→sub.",
                ),
            );
        });
    }
    app.reload_wallets();
    app.profiles = vec![
        ProfileMeta {
            id: "aaaa".into(),
            name: "Treasury".into(),
            created_at: 0,
        },
        ProfileMeta {
            id: "bbbb".into(),
            name: "Trading desk".into(),
            created_at: 0,
        },
    ];
    app
}

fn render(app: &mut App) -> String {
    let backend = TestBackend::new(W, H);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, app)).unwrap();
    buffer_to_string(terminal.backend().buffer())
}

fn banner(title: &str) {
    println!("\n╔══════════════════════════════════════════════════════════════════════════╗");
    println!("  {title}");
    println!("╚══════════════════════════════════════════════════════════════════════════╝");
}

#[test]
fn preview_all_screens() {
    let mut app = test_app();

    app.route = Route::ProfileSelect;
    app.profile_sel = 1;
    banner("PROFILE SELECT");
    print!("{}", render(&mut app));

    app.route = Route::WalletList;
    banner("WALLET LIST");
    print!("{}", render(&mut app));

    app.focused_wallet = Some(app.wallets[2].id);
    app.refresh_detail_intents_blocking();
    app.route = Route::WalletDetail;
    banner("WALLET DETAIL (Trading) — multi-line note");
    print!("{}", render(&mut app));

    {
        let from = app.wallets[2].id;
        let to = app.wallets[0].pubkey.clone();
        app.db.call_blocking(move |d| {
            let i = d.create_intent(from, &to, 1_500_000_000, None).unwrap();
            d.mark_signed(
                i.id,
                "5Hx9c4kQ2mWnTr8sV1pLb3JfYx7aZ2nQ8mK9kQpVtRf2dGhEjKpLmNoPqRsTuVwXy",
                "BhAsH",
                1000,
                5023,
                b"wire",
            )
            .unwrap();
            d.mark_terminal(i.id, crate::types::IntentStatus::Confirmed, None)
                .unwrap();
        });
    }
    app.refresh_detail_intents_blocking();
    app.route = Route::History;
    app.history_state.select(Some(0));
    banner("HISTORY — TX column (c copies the selected transaction id)");
    print!("{}", render(&mut app));

    app.input.prompt_text =
        "Day-trading hot wallet — keep under 20 SOL.\nSweep profits weekly.".into();
    app.modal = Some(Modal::Prompt {
        kind: PromptKind::Note(app.wallets[2].id),
        title: "Set note".into(),
    });
    banner("NOTE EDITOR — multi-line (ctrl+s save · enter newline · esc cancel)");
    print!("{}", render(&mut app));
    app.modal = None;
    app.input.prompt_text.clear();

    app.route = Route::Send;
    app.input.send_to = "9aFh2mRcoldStorageXXXXXXXXXXXXXXXXXXXXXXXXXX".into();
    app.input.send_amount = "2.5".into();
    app.input.focus = 1;
    banner("SEND (compose) — note sub→sub is allowed here only because dest is shown valid");
    print!("{}", render(&mut app));

    app.input.send_amount = "365.50".into();
    app.input.send_in_fiat = true;
    banner("SEND (compose) — amount in fiat (c toggles SOL/fiat)");
    print!("{}", render(&mut app));
    app.input.send_in_fiat = false;
    app.input.send_amount = "2.5".into();

    app.pending_send = Some(PendingSend {
        from_id: app.wallets[2].id,
        to: "9aFh2mRcoldStorageXXXXXXXXXXXXXXXXXXXXXXXXXX".into(),
        lamports: 2_500_000_000,
        blockhash: "BhAsHxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx".into(),
        lvbh: 1000,
        fee: 5000,
        dest_balance: 1_000_000_000,
        priority_micro: 0,
        prepared_at: std::time::Instant::now(),
    });
    app.modal = Some(Modal::ConfirmSend);
    banner("CONFIRM SEND modal");
    print!("{}", render(&mut app));
    app.modal = None;
    app.pending_send = None;

    app.route = Route::Unlock;
    app.input.passphrase = zeroize::Zeroizing::new("hunter2".to_string());
    banner("UNLOCK");
    print!("{}", render(&mut app));

    app.route = Route::Setup;
    app.setup.stage = SetupStage::Choose;
    banner("SETUP — choose");
    print!("{}", render(&mut app));

    app.setup.stage = SetupStage::ShowMnemonic;
    app.setup.mnemonic_words =
        "legal winner thank year wave sausage worth useful legal winner thank yellow"
            .split_whitespace()
            .map(String::from)
            .collect();
    banner("SETUP — show recovery phrase");
    print!("{}", render(&mut app));

    app.setup.stage = SetupStage::ConfirmMnemonic;
    let n = app.setup.mnemonic_words.len();
    app.setup.begin_confirm(n);
    for (i, wd) in ["legal", "winner", "thank"].iter().enumerate() {
        app.setup.confirm_words[i] = (*wd).to_string();
    }
    app.setup.confirm_words[3] = "ye".to_string();
    app.setup.confirm_focus = 3;
    banner("SETUP — confirm phrase (12 boxes, box 4 mid-type)");
    print!("{}", render(&mut app));

    app.route = Route::AuditLog;
    app.refresh_audit_blocking();
    banner("AUDIT LOG");
    print!("{}", render(&mut app));

    app.route = Route::Settings;
    banner("SETTINGS");
    print!("{}", render(&mut app));
}

#[test]
fn preview_confetti_overlay() {
    let mut app = test_app();
    app.route = Route::WalletList;
    app.celebrate_center();
    for _ in 0..7 {
        app.tick();
    }
    assert!(!app.confetti.is_empty(), "confetti should be alive");
    let out = render(&mut app);
    banner("WALLET LIST + CONFETTI (7 ticks in)");
    print!("{out}");
    let any = ['✦', '✧', '★', '*', '•', '◆', '◇', '❄', '✺', '＋']
        .iter()
        .any(|g| out.contains(*g));
    assert!(any, "expected at least one confetti glyph in the buffer");
}

#[test]
fn preview_archived_dropdown() {
    let mut app = test_app();
    app.route = Route::WalletList;
    if let Some(w) = app.wallets.iter_mut().find(|w| w.account_index == 3) {
        w.archived = true;
    }
    app.clamp_list_selection();
    banner("WALLET LIST — archived section collapsed");
    print!("{}", render(&mut app));

    app.archived_expanded = true;
    app.clamp_list_selection();
    banner("WALLET LIST — archived section expanded");
    let out = render(&mut app);
    print!("{out}");
    assert!(
        out.contains("Archived (1)"),
        "archived header should render"
    );
    assert!(
        out.contains("Subwallet 3"),
        "archived wallet row should show under the header"
    );
}

#[test]
fn optimistic_balance_bump_on_confirm() {
    let mut app = test_app();
    app.route = Route::WalletList;
    let from_id = app.wallets[0].id;
    let to_addr = app.wallets[1].pubkey.clone();
    let from_before = app.wallets[0].balance_lamports.unwrap();
    let to_before = app.wallets[1].balance_lamports.unwrap();

    let amount = 5_000_000_000u64;
    let to_for_intent = to_addr.clone();
    let intent_id = app.db.call_blocking(move |d| {
        d.create_intent(from_id, &to_for_intent, amount, None)
            .unwrap()
            .id
    });

    app.apply_app_event(AppEvent::TransferResult {
        intent_id,
        outcome: TransferOutcome::Confirmed {
            signature: "Sig11111111111111111111111111111111111111111".into(),
        },
        transfer: Some(OptimisticTransfer {
            from_wallet: from_id,
            to_address: to_addr,
            lamports: amount,
            fee_lamports: None,
        }),
        generation: app.generation.load(std::sync::atomic::Ordering::SeqCst),
    });

    let fee = app.send_fee();
    let from_after = app.wallets[0].balance_lamports.unwrap();
    let to_after = app.wallets[1].balance_lamports.unwrap();
    assert_eq!(
        from_after,
        from_before - amount - fee,
        "sender debited amount + fee immediately"
    );
    assert_eq!(
        to_after,
        to_before + amount,
        "tracked recipient credited the amount immediately"
    );
}
