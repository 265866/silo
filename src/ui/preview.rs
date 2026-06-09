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

fn render_sized(app: &mut App, w: u16, h: u16) -> String {
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, app)).unwrap();
    buffer_to_string(terminal.backend().buffer())
}

const ALL_ROUTES: [Route; 9] = [
    Route::ProfileSelect,
    Route::Unlock,
    Route::Setup,
    Route::WalletList,
    Route::WalletDetail,
    Route::Send,
    Route::History,
    Route::AuditLog,
    Route::Settings,
];

fn fuzz_app() -> App {
    let mut app = test_app();
    app.latest_version = Some("9.9.9".into());
    app.focused_wallet = Some(app.wallets[2].id);
    app.refresh_detail_intents_blocking();
    app.list_state.select(Some(0));
    app.input.send_to = "9aFh2mRcoldStorageXXXXXXXXXXXXXXXXXXXXXXXXXX".into();
    app.input.send_amount = "2.5".into();
    app.refresh_audit_blocking();
    app.toast_info("a toast that should not blank live rows");
    app
}

#[test]
fn renders_every_size_without_panicking() {
    let setups = [
        SetupStage::Choose,
        SetupStage::ShowMnemonic,
        SetupStage::ConfirmMnemonic,
    ];
    for w in (20u16..=120).step_by(7) {
        for h in (6u16..=30).step_by(3) {
            for route in ALL_ROUTES {
                let mut app = fuzz_app();
                app.route = route;
                if route == Route::Setup {
                    for stage in setups {
                        app.setup.stage = stage;
                        app.setup.mnemonic_words =
                            "legal winner thank year wave sausage worth useful legal winner \
                             thank yellow"
                                .split_whitespace()
                                .map(String::from)
                                .collect();
                        if stage == SetupStage::ConfirmMnemonic {
                            let n = app.setup.mnemonic_words.len();
                            app.setup.begin_confirm(n);
                        }
                        let _ = render_sized(&mut app, w, h);
                    }
                }
                let _ = render_sized(&mut app, w, h);
            }

            let mut app = fuzz_app();
            app.route = Route::WalletList;
            app.modal = Some(Modal::Error {
                title: "Send failed".into(),
                body: "the network rejected this transaction for a long winded reason that wraps"
                    .into(),
            });
            let _ = render_sized(&mut app, w, h);

            app.modal = Some(Modal::Confirm {
                title: "Empty passphrase".into(),
                body: "create the vault with no passphrase?".into(),
                action: crate::app::ConfirmAction::CreateWithEmptyPassphrase,
            });
            let _ = render_sized(&mut app, w, h);

            app.modal = Some(Modal::Prompt {
                kind: PromptKind::Label(app.wallets[0].id),
                title: "Rename".into(),
            });
            let _ = render_sized(&mut app, w, h);

            app.input.prompt_text = "a multi line note\nsecond line here".into();
            app.modal = Some(Modal::Prompt {
                kind: PromptKind::Note(app.wallets[0].id),
                title: "Set note".into(),
            });
            let _ = render_sized(&mut app, w, h);

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
            let _ = render_sized(&mut app, w, h);

            app.pending_send = Some(PendingSend {
                from_id: app.wallets[2].id,
                to: "ZzExternalWalletNotOursXXXXXXXXXXXXXXXXXXXXXX".into(),
                lamports: 12_000_000_000,
                blockhash: "BhAsHxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx".into(),
                lvbh: 1000,
                fee: 5000,
                dest_balance: 0,
                priority_micro: 0,
                prepared_at: std::time::Instant::now(),
            });
            app.send_confirm_armed = true;
            let _ = render_sized(&mut app, w, h);
            app.send_confirm_armed = false;
        }
    }
}

#[test]
fn below_floor_shows_resize_notice_and_hides_screen() {
    for (w, h) in [(50u16, 10u16), (59, 20), (80, 12)] {
        let mut app = fuzz_app();
        app.route = Route::WalletList;
        let out = render_sized(&mut app, w, h);
        assert!(
            out.contains("please resize"),
            "expected resize notice at {w}x{h}, got:\n{out}"
        );
        assert!(
            !out.contains("Wallets ("),
            "wallet-list panel must not render below the floor at {w}x{h}"
        );
    }
}

#[test]
fn at_floor_renders_normal_screen_not_resize_notice() {
    let mut app = fuzz_app();
    app.route = Route::WalletList;
    let out = render_sized(&mut app, 60, 16);
    assert!(
        !out.contains("please resize"),
        "60x16 is at the floor and should render the screen"
    );
    assert!(
        out.contains("Wallets ("),
        "wallet-list panel should render at the floor"
    );
}

#[test]
fn confirm_send_flags_external_recipient_and_shows_total() {
    let mut app = test_app();
    app.route = Route::Send;
    app.pending_send = Some(PendingSend {
        from_id: app.wallets[2].id,
        to: "ZzExternalWalletNotOursXXXXXXXXXXXXXXXXXXXXXX".into(),
        lamports: 2_500_000_000,
        blockhash: "BhAsHxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx".into(),
        lvbh: 1000,
        fee: 5000,
        dest_balance: 0,
        priority_micro: 0,
        prepared_at: std::time::Instant::now(),
    });
    app.modal = Some(Modal::ConfirmSend);
    let out = render(&mut app);
    assert!(
        out.contains("leaving your wallets"),
        "external send must warn that funds leave your wallets:\n{out}"
    );
    assert!(
        out.contains("(external)"),
        "external send must mark the recipient external:\n{out}"
    );
    assert!(
        out.contains("total") && out.contains("2.500005 SOL"),
        "total row must show amount + fee:\n{out}"
    );
    assert!(
        out.contains("Send now"),
        "normal send shows the plain send verb:\n{out}"
    );
    assert!(
        !out.contains("blockhash") && !out.contains("re-fetched"),
        "blockhash jargon must not appear:\n{out}"
    );
}

#[test]
fn confirm_send_shows_large_send_banner_when_armed() {
    let mut app = test_app();
    app.route = Route::Send;
    app.pending_send = Some(PendingSend {
        from_id: app.wallets[2].id,
        to: "9aFh2mRcoldStorageXXXXXXXXXXXXXXXXXXXXXXXXXX".into(),
        lamports: 12_000_000_000,
        blockhash: "BhAsHxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx".into(),
        lvbh: 1000,
        fee: 5000,
        dest_balance: 0,
        priority_micro: 0,
        prepared_at: std::time::Instant::now(),
    });
    app.send_confirm_armed = true;
    app.modal = Some(Modal::ConfirmSend);
    assert!(
        app.pending_send_is_large(),
        "fixture must be a large send for the banner to apply"
    );
    let out = render(&mut app);
    assert!(
        out.contains("% of this wallet's balance"),
        "armed large send must show the percent-of-balance banner:\n{out}"
    );
    assert!(
        out.contains("confirm large send"),
        "armed large send must relabel the action button:\n{out}"
    );
}

#[test]
fn wallet_list_footer_keeps_lock_and_quit() {
    for w in [80u16, 60] {
        let mut app = fuzz_app();
        app.latest_version = None;
        app.toasts.clear();
        app.route = Route::WalletList;
        let out = render_sized(&mut app, w, 24);
        assert!(out.contains("lock"), "footer must keep lock at width {w}");
        assert!(out.contains("quit"), "footer must keep quit at width {w}");
    }
}

#[test]
fn wallet_list_drops_fiat_column_when_narrow() {
    let mut app = fuzz_app();
    app.route = Route::WalletList;

    let narrow = render_sized(&mut app, 70, 24);
    assert!(
        !narrow.contains("USD"),
        "USD column should be dropped at width 70:\n{narrow}"
    );

    let wide = render_sized(&mut app, 90, 24);
    assert!(
        wide.contains("USD"),
        "USD column should be present at width 90:\n{wide}"
    );
}
