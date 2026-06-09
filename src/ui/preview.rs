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

fn empty_app() -> App {
    let db = Db::open_memory().unwrap();
    let db = Storage::new(db);
    let price = Arc::new(PriceCache::new());
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
    app
}

fn cell_fg_present(app: &mut App, glyph: &str, fg: ratatui::style::Color) -> bool {
    let backend = TestBackend::new(W, H);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, app)).unwrap();
    let buf = terminal.backend().buffer().clone();
    let area = buf.area;
    (0..area.height).any(|y| {
        (0..area.width).any(|x| {
            buf.cell((x, y))
                .is_some_and(|c| c.symbol() == glyph && c.fg == fg)
        })
    })
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
            d.mark_terminal(i.id, crate::types::TerminalStatus::Confirmed, None)
                .unwrap();
        });
    }
    app.refresh_detail_intents_blocking();
    app.route = Route::History;
    app.history_state.select(Some(0));
    banner("HISTORY — TX column (c copies the selected transaction ID)");
    print!("{}", render(&mut app));

    app.input.prompt_text =
        "Day-trading hot wallet — keep under 20 SOL.\nSweep profits weekly.".into();
    app.modal = Some(Modal::Prompt {
        kind: PromptKind::Note(app.wallets[2].id),
        title: "Set note".into(),
    });
    banner("NOTE EDITOR — multi-line (^S save · enter newline · esc cancel)");
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
        out.contains("archived (1)"),
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

const VALID_EXTERNAL: &str = "HAgk14JpMQLgt6rVgv7cBQFJWFto5Dqxi472uT3DKpqk";

fn send_from_master(app: &mut App) -> i64 {
    let id = app
        .wallets
        .iter()
        .find(|w| w.account_index == 0)
        .map(|w| w.id)
        .unwrap();
    app.focused_wallet = Some(id);
    app.route = Route::Send;
    app.input.send_to = VALID_EXTERNAL.into();
    id
}

#[test]
fn send_amount_label_tracks_denomination() {
    let mut app = fuzz_app();
    send_from_master(&mut app);
    app.input.send_amount = "2.5".into();
    app.input.focus = 1;

    app.input.send_in_fiat = false;
    let sol = render(&mut app);
    assert!(
        sol.contains("amount"),
        "amount field keeps a plain label:\n{sol}"
    );
    assert!(
        sol.contains("[SOL]") && sol.contains("c to switch"),
        "switch indicator must mark the active unit and the toggle key:\n{sol}"
    );

    app.input.send_in_fiat = true;
    let fiat = render(&mut app);
    assert!(
        fiat.contains("[USD]"),
        "switch indicator must mark the active fiat unit:\n{fiat}"
    );
}

#[test]
fn send_warns_when_amount_trips_source_floor() {
    let mut app = fuzz_app();
    let id = send_from_master(&mut app);
    let fee = app.send_fee();
    let min = app.rent_exempt_min;
    let amount = 1_000_000_000u64;
    if let Some(w) = app.wallets.iter_mut().find(|w| w.id == id) {
        w.balance_lamports = Some(amount + fee + min / 2);
    }
    app.input.send_amount = "1".into();
    let out = render(&mut app);
    assert!(
        out.contains("below the minimum balance"),
        "must warn the send would drop the wallet below the minimum:\n{out}"
    );
    assert!(
        !out.contains("rent-exempt"),
        "must not expose the rent-exempt jargon:\n{out}"
    );
}

#[test]
fn send_warns_on_underfunded_first_deposit() {
    let mut app = fuzz_app();
    send_from_master(&mut app);
    app.input.send_in_fiat = false;
    app.input.send_amount = "0.0001".into();
    let out = render(&mut app);
    assert!(
        out.contains("first deposit to a new address must be at least"),
        "must warn that a first deposit must clear the minimum:\n{out}"
    );
}

#[test]
fn send_floor_note_stays_quiet_at_zero_amount() {
    let mut app = fuzz_app();
    send_from_master(&mut app);
    app.input.send_in_fiat = false;
    app.input.send_amount = "0".into();
    let out = render(&mut app);
    assert!(
        !out.contains("first deposit"),
        "zero amount must not surface the first-deposit floor note:\n{out}"
    );
    assert!(
        !out.contains('⚠'),
        "zero amount must not surface any floor warning:\n{out}"
    );
}

#[test]
fn send_avail_fee_line_never_leads_with_separator_when_loading() {
    let mut app = fuzz_app();
    let id = send_from_master(&mut app);
    if let Some(w) = app.wallets.iter_mut().find(|w| w.id == id) {
        w.balance_lamports = None;
    }
    app.input.send_amount.clear();
    let out = render(&mut app);
    assert!(
        out.contains("available … · fee ≈"),
        "loading balance must render an ellipsis, not a bare separator:\n{out}"
    );
    for line in out.lines() {
        let t = line.trim_start();
        assert!(
            !t.starts_with('·'),
            "no rendered line may lead with the separator:\n{out}"
        );
    }
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

#[test]
fn confirm_counter_counts_non_empty_slots() {
    let mut app = test_app();
    app.toasts.clear();
    app.route = Route::Setup;
    app.setup.stage = SetupStage::ConfirmMnemonic;
    app.setup.creating = true;
    app.setup.mnemonic_words = vec!["abandon".into(); 12];
    app.setup.confirm_words = vec![String::new(); 12];
    app.setup.confirm_words[0] = "abandon".into();
    app.setup.confirm_words[1] = "ab".into();
    app.setup.confirm_words[2] = "ability".into();
    app.setup.confirm_focus = 3;

    let out = render(&mut app);
    assert!(
        out.contains("3/12 entered"),
        "counter must count every non-empty slot, not just valid words:\n{out}"
    );
}

#[test]
fn confirm_mismatch_renders_message_and_danger() {
    let mut app = test_app();
    app.toasts.clear();
    app.route = Route::Setup;
    app.setup.stage = SetupStage::ConfirmMnemonic;
    app.setup.creating = true;
    app.setup.mnemonic_words = vec!["abandon".into(); 12];
    app.setup.confirm_words = vec!["abandon".into(); 12];
    app.setup.confirm_words[4] = "zoo".into();
    app.setup.confirm_mismatch = Some(4);
    app.setup.confirm_focus = 4;

    let out = render(&mut app);
    assert!(
        out.contains("word 5 doesn't match"),
        "mismatch message must name the first differing slot:\n{out}"
    );
}

#[test]
fn passphrase_shows_live_match_indicator() {
    let mut app = test_app();
    app.toasts.clear();
    app.route = Route::Setup;
    app.setup.stage = SetupStage::SetPassphrase;
    app.input.passphrase = zeroize::Zeroizing::new("hunter22".into());

    app.input.passphrase2 = zeroize::Zeroizing::new("hunter22".into());
    let ok = render(&mut app);
    assert!(
        ok.contains("✓ match"),
        "matching passphrases show a tick:\n{ok}"
    );
    assert!(
        ok.contains("8+ characters recommended"),
        "passphrase hint must be present:\n{ok}"
    );
    assert!(
        !ok.contains("at least 8 characters"),
        "hint must not claim a hard minimum that isn't enforced:\n{ok}"
    );

    app.input.passphrase2 = zeroize::Zeroizing::new("hunter23".into());
    let bad = render(&mut app);
    assert!(
        bad.contains("✗ no match"),
        "differing passphrases show no-match:\n{bad}"
    );
}

#[test]
fn import_shows_paste_hint_and_word_counter() {
    let mut app = test_app();
    app.toasts.clear();
    app.route = Route::Setup;
    app.setup.stage = SetupStage::ImportEntry;
    app.input.import_phrase = zeroize::Zeroizing::new("abandon ability able about".into());

    let out = render(&mut app);
    assert!(out.contains("^V"), "import must hint at paste:\n{out}");
    assert!(
        out.contains("words: 4/12"),
        "import must show a live word counter:\n{out}"
    );
}

#[test]
fn import_colors_valid_and_impossible_words() {
    let mut app = test_app();
    app.toasts.clear();
    app.route = Route::Setup;
    app.setup.stage = SetupStage::ImportEntry;
    app.input.import_phrase = zeroize::Zeroizing::new("abandon zzzz".into());

    let ok = app.theme.usd;
    let bad = app.theme.danger;
    assert!(
        cell_fg_present(&mut app, "b", ok),
        "a complete valid word must render in the ok color"
    );
    assert!(
        cell_fg_present(&mut app, "z", bad),
        "an impossible word must render in the danger color"
    );
    assert!(
        !cell_fg_present(&mut app, "z", ok),
        "an impossible word must not borrow the ok color"
    );
}

#[test]
fn import_leaves_valid_prefix_neutral() {
    let mut app = test_app();
    app.toasts.clear();
    app.route = Route::Setup;
    app.setup.stage = SetupStage::ImportEntry;
    app.input.import_phrase = zeroize::Zeroizing::new("aban about".into());

    let neutral = app.theme.text;
    let bad = app.theme.danger;
    assert!(
        cell_fg_present(&mut app, "b", neutral),
        "a valid prefix still being typed must stay neutral"
    );
    assert!(
        !cell_fg_present(&mut app, "b", bad),
        "a valid prefix must not be flagged as impossible while typing"
    );
    assert!(
        !cell_fg_present(&mut app, "n", bad),
        "no glyph of a prefix-only phrase may render in the danger color"
    );
}

fn setup_stage_words() -> Vec<String> {
    "legal winner thank year wave sausage worth useful legal winner thank yellow"
        .split_whitespace()
        .map(String::from)
        .collect()
}

#[test]
fn setup_footer_is_stage_specific() {
    let cases = [
        (SetupStage::Choose, "c new wallet · i import · esc quit"),
        (SetupStage::ShowMnemonic, "enter continue · esc back"),
        (SetupStage::ImportEntry, "enter continue · esc back"),
        (
            SetupStage::SetPassphrase,
            "tab switch field · enter create · esc back",
        ),
    ];
    for (stage, expected) in cases {
        let mut app = test_app();
        app.toasts.clear();
        app.latest_version = None;
        app.route = Route::Setup;
        app.setup.stage = stage;
        app.setup.mnemonic_words = setup_stage_words();
        let out = render(&mut app);
        assert!(
            out.contains(expected),
            "stage {stage:?} footer must read '{expected}':\n{out}"
        );
    }
}

#[test]
fn setpassphrase_footer_never_says_vault() {
    let mut app = test_app();
    app.toasts.clear();
    app.latest_version = None;
    app.route = Route::Setup;
    app.setup.stage = SetupStage::SetPassphrase;
    let out = render(&mut app);
    assert!(
        out.contains("enter create") && !out.contains("create vault"),
        "passphrase footer must say 'enter create', not 'create vault':\n{out}"
    );
}

#[test]
fn no_setup_screen_surfaces_the_word_vault() {
    let stages = [
        SetupStage::Choose,
        SetupStage::ShowMnemonic,
        SetupStage::ConfirmMnemonic,
        SetupStage::ImportEntry,
        SetupStage::SetPassphrase,
    ];
    for stage in stages {
        let mut app = test_app();
        app.toasts.clear();
        app.route = Route::Setup;
        app.setup.stage = stage;
        app.setup.mnemonic_words = setup_stage_words();
        if stage == SetupStage::ConfirmMnemonic {
            let n = app.setup.mnemonic_words.len();
            app.setup.begin_confirm(n);
        }
        let out = render(&mut app);
        assert!(
            !out.to_lowercase().contains("vault"),
            "setup stage {stage:?} must not surface the word 'vault':\n{out}"
        );
    }
}

#[test]
fn confirm_mnemonic_height_fits_12_words_without_hollow_box() {
    let mut app = test_app();
    app.toasts.clear();
    app.route = Route::Setup;
    app.setup.stage = SetupStage::ConfirmMnemonic;
    app.setup.mnemonic_words = vec!["abandon".into(); 12];
    app.setup.begin_confirm(12);

    let out = render(&mut app);
    let inner: Vec<&str> = out.lines().filter(|l| l.contains('│')).collect();
    let is_blank = |l: &str| l.chars().all(|c| c == '│' || c == ' ');
    let trailing_blanks = inner.iter().rev().take_while(|l| is_blank(l)).count();
    assert!(
        trailing_blanks == 0,
        "12-word confirm panel must end on its hint, not a run of empty rows:\n{out}"
    );
    let last_meaningful = inner.iter().rev().find(|l| !is_blank(l)).copied();
    assert!(
        last_meaningful.is_some_and(|l| l.contains("enter confirm")),
        "the bottom inner row should be the in-panel hint, proving the panel hugs content:\n{out}"
    );
}

#[test]
fn setup_panels_share_one_width_across_stages() {
    let stages = [
        SetupStage::Choose,
        SetupStage::ShowMnemonic,
        SetupStage::ConfirmMnemonic,
        SetupStage::ImportEntry,
        SetupStage::SetPassphrase,
    ];
    let mut widths = Vec::new();
    for stage in stages {
        let mut app = test_app();
        app.toasts.clear();
        app.route = Route::Setup;
        app.setup.stage = stage;
        app.setup.mnemonic_words = setup_stage_words();
        if stage == SetupStage::ConfirmMnemonic {
            let n = app.setup.mnemonic_words.len();
            app.setup.begin_confirm(n);
        }
        let out = render(&mut app);
        let w = out
            .lines()
            .find(|l| l.contains('╭') && l.contains('╮'))
            .map(|l| {
                let chars: Vec<char> = l.chars().collect();
                let start = chars.iter().position(|c| *c == '╭').unwrap();
                let end = chars.iter().rposition(|c| *c == '╮').unwrap();
                end - start + 1
            })
            .unwrap_or(0);
        widths.push((stage, w));
    }
    let first = widths[0].1;
    for (stage, w) in &widths {
        assert_eq!(
            *w, first,
            "every setup stage must share one border width; stage {stage:?} differs"
        );
    }
}

fn focus_wallet(app: &mut App, account_index: u32) {
    let id = app
        .wallets
        .iter()
        .find(|w| w.account_index == account_index)
        .map(|w| w.id)
        .unwrap();
    app.focused_wallet = Some(id);
    app.refresh_detail_intents_blocking();
}

#[test]
fn wallet_detail_footer_advertises_back_and_lock() {
    let mut app = test_app();
    app.toasts.clear();
    app.latest_version = None;
    app.route = Route::WalletDetail;
    focus_wallet(&mut app, 0);
    let out = render(&mut app);
    assert!(
        out.contains("q back"),
        "wallet-detail footer must advertise q back:\n{out}"
    );
    assert!(
        out.contains("^L lock"),
        "wallet-detail footer must advertise ^L lock:\n{out}"
    );
}

#[test]
fn wallet_footers_name_the_copy_object() {
    let mut app = test_app();
    app.toasts.clear();
    app.latest_version = None;

    app.route = Route::WalletList;
    let list = render(&mut app);
    assert!(
        list.contains("c copy address"),
        "wallet-list footer must name what c copies:\n{list}"
    );

    app.route = Route::WalletDetail;
    focus_wallet(&mut app, 0);
    let detail = render(&mut app);
    assert!(
        detail.contains("c copy address"),
        "wallet-detail footer must name what c copies:\n{detail}"
    );
}

#[test]
fn wallet_list_title_spells_out_subwallet() {
    let mut app = test_app();
    app.toasts.clear();
    app.route = Route::WalletList;
    let out = render(&mut app);
    assert!(
        out.contains("subwallet)"),
        "panel title must spell out subwallet, not abbreviate to sub:\n{out}"
    );
}

fn render_buffer(app: &mut App, w: u16, h: u16) -> Buffer {
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, app)).unwrap();
    terminal.backend().buffer().clone()
}

fn first_glyph_x(buf: &Buffer, y: u16, glyph: &str) -> Option<u16> {
    let area = buf.area;
    (0..area.width).find(|&x| buf.cell((x, y)).is_some_and(|c| c.symbol() == glyph))
}

fn row_text(buf: &Buffer, y: u16) -> String {
    let area = buf.area;
    (0..area.width)
        .filter_map(|x| buf.cell((x, y)).map(|c| c.symbol().to_string()))
        .collect()
}

fn find_label_x(buf: &Buffer, label: &str) -> Option<(u16, u16)> {
    let area = buf.area;
    for y in 0..area.height {
        let line = row_text(buf, y);
        if let Some(byte_off) = line.find(label) {
            let col = line[..byte_off].chars().count() as u16;
            return Some((col, y));
        }
    }
    None
}

#[test]
fn wallet_list_name_origin_shared_across_roles() {
    let mut app = test_app();
    app.toasts.clear();
    app.route = Route::WalletList;
    let buf = render_buffer(&mut app, W, H);

    let (master_name_x, _) =
        find_label_x(&buf, "Treasury").expect("master name Treasury must render");
    let (sub_name_x, _) =
        find_label_x(&buf, "Cold storage").expect("sub name Cold storage must render");

    assert_eq!(
        master_name_x, sub_name_x,
        "master and sub NAME columns must share a left edge: Treasury at x={master_name_x}, Cold storage at x={sub_name_x}"
    );
}

#[test]
fn wallet_list_star_lives_left_of_name_origin() {
    let mut app = test_app();
    app.toasts.clear();
    app.route = Route::WalletList;
    let buf = render_buffer(&mut app, W, H);

    let (name_x, master_y) =
        find_label_x(&buf, "Treasury").expect("master name Treasury must render");
    let star_x = first_glyph_x(&buf, master_y, "★").expect("master row must carry the star");

    assert!(
        star_x < name_x,
        "the master star must sit in its own column left of the NAME origin: star x={star_x}, name x={name_x}"
    );
    assert!(
        name_x - star_x >= 3,
        "the star must occupy its own width-2 column with column spacing before NAME, not sit inline in the NAME cell: star x={star_x}, name x={name_x}"
    );
    let (sub_name_x, _) =
        find_label_x(&buf, "Cold storage").expect("sub name Cold storage must render");
    assert!(
        first_glyph_x(&buf, master_y, "★") < Some(sub_name_x),
        "the star column lives left of the shared NAME origin: star x={star_x}, name origin x={sub_name_x}"
    );
}

#[test]
fn wallet_detail_master_is_explained_and_gold() {
    let mut app = test_app();
    app.toasts.clear();
    app.route = Route::WalletDetail;
    focus_wallet(&mut app, 0);
    let out = render(&mut app);
    assert!(
        out.contains("★ master"),
        "master type line must carry the gold star cue:\n{out}"
    );
    assert!(
        out.contains("funds subwallets") && out.contains("cannot be archived"),
        "master type line must explain what master means:\n{out}"
    );
    let master_fg = app.theme.master;
    assert!(
        cell_fg_present(&mut app, "m", master_fg),
        "the master label must render in the master color"
    );
}

#[test]
fn wallet_detail_subwallet_type_stays_plain() {
    let mut app = test_app();
    app.toasts.clear();
    app.route = Route::WalletDetail;
    focus_wallet(&mut app, 1);
    let out = render(&mut app);
    assert!(
        out.contains("subwallet"),
        "subwallet type line must label the role:\n{out}"
    );
    assert!(
        !out.contains("funds subwallets"),
        "the master qualifier must not appear on a subwallet:\n{out}"
    );
}

#[test]
fn wallet_list_footer_carries_pending_legend() {
    let mut app = test_app();
    app.toasts.clear();
    app.latest_version = None;
    app.route = Route::WalletList;

    for w in &mut app.wallets {
        w.has_open_intent = false;
    }
    let out = render_sized(&mut app, 130, 24);
    assert!(
        !out.contains("transfer in progress"),
        "wallet-list footer must omit the pending legend with no open intent:\n{out}"
    );

    app.wallets[0].has_open_intent = true;
    let out = render_sized(&mut app, 130, 24);
    assert!(
        out.contains('⏳') && out.contains("transfer in progress"),
        "wallet-list footer must legend the pending glyph when an intent is open:\n{out}"
    );
}

#[test]
fn wallet_list_empty_state_prompts_for_a_subwallet() {
    let mut app = empty_app();
    app.toasts.clear();
    app.route = Route::WalletList;
    assert!(app.wallets.is_empty(), "fixture must start with no wallets");
    let out = render(&mut app);
    assert!(
        out.contains("No wallets yet") && out.contains("press n to add a subwallet"),
        "empty wallet list must prompt the user to add a subwallet:\n{out}"
    );
}

#[test]
fn audit_log_empty_state_shows_no_events_yet() {
    let mut app = test_app();
    app.toasts.clear();
    app.route = Route::AuditLog;
    app.audit.clear();
    let out = render(&mut app);
    assert!(
        out.contains("no events yet"),
        "empty audit log must show 'no events yet':\n{out}"
    );
    assert!(
        out.contains("Audit log"),
        "empty audit log must still show the panel title:\n{out}"
    );
}

#[test]
fn audit_log_title_and_subtitle() {
    let mut app = test_app();
    app.toasts.clear();
    app.route = Route::AuditLog;
    app.refresh_audit_blocking();
    let out = render(&mut app);
    assert!(
        out.contains("Audit log"),
        "audit log panel title must read 'Audit log':\n{out}"
    );
    assert!(
        !out.contains("append-only") && !out.contains("hash-chained"),
        "implementation jargon must not appear in the panel title:\n{out}"
    );
    assert!(
        out.contains("Tamper-evident record of every action"),
        "tamper-evidence subtitle must appear inside the panel:\n{out}"
    );
}

#[test]
fn history_footer_shows_scroll_key_hints() {
    let mut app = test_app();
    app.toasts.clear();
    app.latest_version = None;
    app.route = Route::History;
    let out = render_sized(&mut app, 96, 24);
    assert!(
        out.contains("PgUp/PgDn"),
        "history footer must show PgUp/PgDn:\n{out}"
    );
    assert!(
        out.contains("Home/End"),
        "history footer must show Home/End:\n{out}"
    );
}

#[test]
fn audit_log_footer_shows_scroll_key_hints() {
    let mut app = test_app();
    app.toasts.clear();
    app.latest_version = None;
    app.route = Route::AuditLog;
    let out = render_sized(&mut app, 96, 24);
    assert!(
        out.contains("PgUp/PgDn"),
        "audit log footer must show PgUp/PgDn:\n{out}"
    );
    assert!(
        out.contains("Home/End"),
        "audit log footer must show Home/End:\n{out}"
    );
}

#[test]
fn unlock_shows_inline_error_and_clears_field_on_failure() {
    let mut app = test_app();
    app.toasts.clear();
    app.current_profile = Some("aaaa".into());
    app.route = Route::Unlock;
    app.input.passphrase = zeroize::Zeroizing::new(String::new());
    app.unlock_failed = true;

    let out = render(&mut app);
    assert!(
        out.contains("Incorrect passphrase"),
        "failed unlock must render a persistent inline error:\n{out}"
    );
    assert!(
        !out.contains('•'),
        "the masked field must be cleared after a failed attempt:\n{out}"
    );
    let danger = app.theme.danger;
    assert!(
        cell_fg_present(&mut app, "I", danger),
        "the error line must render in the danger color"
    );
}

#[test]
fn unlock_names_the_selected_profile() {
    let mut app = test_app();
    app.toasts.clear();
    app.current_profile = Some("bbbb".into());
    app.route = Route::Unlock;
    let out = render(&mut app);
    assert!(
        out.contains("Trading desk"),
        "unlock panel must name the profile being unlocked:\n{out}"
    );
    assert!(
        out.contains("only the recovery phrase can"),
        "unlock panel must warn that a lost passphrase is unrecoverable:\n{out}"
    );
}

#[test]
fn unlock_reserves_error_row_so_height_is_stable() {
    let panel_geom = |app: &mut App| -> (usize, usize) {
        let out = render(app);
        let border_rows = out.lines().filter(|l| l.contains('╭') || l.contains('╰'));
        let bordered = out.lines().filter(|l| l.contains('│')).count();
        (border_rows.count(), bordered)
    };

    let mut clean = test_app();
    clean.toasts.clear();
    clean.current_profile = Some("aaaa".into());
    clean.route = Route::Unlock;
    clean.unlock_failed = false;
    let clean_geom = panel_geom(&mut clean);

    let mut failed = test_app();
    failed.toasts.clear();
    failed.current_profile = Some("aaaa".into());
    failed.route = Route::Unlock;
    failed.unlock_failed = true;
    let failed_geom = panel_geom(&mut failed);

    assert_eq!(
        clean_geom, failed_geom,
        "unlock panel must reserve the error row so its height never jumps between attempts"
    );
}

#[test]
fn profile_select_uses_profile_term_with_gloss_and_footer() {
    let mut app = test_app();
    app.toasts.clear();
    app.latest_version = None;
    app.route = Route::ProfileSelect;
    let out = render(&mut app);
    assert!(
        out.contains("Choose a profile"),
        "profile select body must use the profile term:\n{out}"
    );
    assert!(
        out.contains("silo — profiles"),
        "profile select title must use the profile term, not 'wallets':\n{out}"
    );
    assert!(
        !out.contains("wallet profile"),
        "title and body must not clash on profile vs wallet:\n{out}"
    );
    assert!(
        out.contains("Each profile is a separate wallet with its own recovery phrase"),
        "profile select must gloss what a profile is:\n{out}"
    );
    assert!(
        out.contains("n new profile"),
        "footer must label n as creating a new profile, matching what it does:\n{out}"
    );
    assert!(
        !out.to_lowercase().contains("vault"),
        "profile select must not surface the word 'vault':\n{out}"
    );
}

#[test]
fn status_style_signed_is_past_participle_not_gerund() {
    let mut app = test_app();
    app.toasts.clear();
    app.route = Route::History;

    let from = app.wallets[0].id;
    let to = app.wallets[1].pubkey.clone();
    app.db.call_blocking(move |d| {
        let i = d.create_intent(from, &to, 500_000_000, None).unwrap();
        d.mark_signed(
            i.id,
            "5Hx9c4kQ2mWnTr8sV1pLb3JfYx7aZ2nQ8mK9kQpVtRf2dGhEjKpLmNoPqRsTuVwXy",
            "BhAsH",
            1000,
            5023,
            b"wire",
        )
        .unwrap();
    });
    app.focused_wallet = Some(app.wallets[0].id);
    app.refresh_detail_intents_blocking();

    let out = render(&mut app);
    assert!(
        out.contains("signed"),
        "signed status must appear in the history table:\n{out}"
    );
    assert!(
        !out.contains("signing"),
        "gerund 'signing' must not appear — only past-participle 'signed':\n{out}"
    );
}

#[test]
fn authenticated_footers_advertise_ctrl_l_lock() {
    let cases = [
        (Route::Send, "send"),
        (Route::History, "history"),
        (Route::AuditLog, "audit log"),
        (Route::Settings, "settings"),
    ];
    for (route, name) in cases {
        let mut app = test_app();
        app.toasts.clear();
        app.latest_version = None;
        app.route = route;
        let out = render_sized(&mut app, 110, 24);
        assert!(
            out.contains("^L lock"),
            "{name} footer must advertise ^L lock:\n{out}"
        );
    }
}

#[test]
fn error_modal_hint_exposes_esc() {
    let mut app = test_app();
    app.toasts.clear();
    app.route = Route::WalletList;
    app.modal = Some(Modal::Error {
        title: "Send failed".into(),
        body: "the network rejected this transaction".into(),
    });
    let out = render(&mut app);
    assert!(
        out.contains("enter / esc dismiss"),
        "error modal must let esc dismiss, not only enter:\n{out}"
    );
    assert!(
        !out.contains("press Enter"),
        "error modal must drop the old enter-only phrasing:\n{out}"
    );
}

#[test]
fn footers_use_slash_arrows_not_bare_pair() {
    let mut app = test_app();
    app.toasts.clear();
    app.latest_version = None;
    app.route = Route::History;
    let out = render_sized(&mut app, 110, 24);
    assert!(
        out.contains("↑/↓"),
        "scroll hint must use the slash arrow form:\n{out}"
    );
    assert!(
        !out.contains("↑↓"),
        "no footer may carry the bare arrow pair:\n{out}"
    );
}

#[test]
fn scroll_and_move_footers_surface_vim_jk() {
    let mut app = test_app();
    app.toasts.clear();
    app.latest_version = None;

    app.route = Route::History;
    let history = render_sized(&mut app, 110, 24);
    assert!(
        history.contains("↑/↓ jk scroll"),
        "history scroll hint must surface vim jk:\n{history}"
    );

    app.route = Route::ProfileSelect;
    let profiles = render_sized(&mut app, 110, 24);
    assert!(
        profiles.contains("↑/↓ jk move"),
        "profile-select move hint must surface vim jk:\n{profiles}"
    );
}

#[test]
fn settings_drops_redundant_in_screen_hint_line() {
    let mut app = test_app();
    app.toasts.clear();
    app.latest_version = None;
    app.route = Route::Settings;
    let out = render_sized(&mut app, 110, 24);
    assert!(
        !out.contains("lock now"),
        "the redundant in-screen settings hint line must be gone:\n{out}"
    );
    assert!(
        out.contains("(u to cycle)") && out.contains("(+/- to adjust)"),
        "per-row inline hints must stay:\n{out}"
    );
    assert!(
        out.contains("^L lock"),
        "settings footer still advertises the lock key:\n{out}"
    );
}

#[test]
fn authenticated_footers_wrap_to_at_most_two_lines_at_width_80() {
    let routes = [
        Route::WalletList,
        Route::WalletDetail,
        Route::Send,
        Route::History,
        Route::AuditLog,
        Route::Settings,
    ];
    for route in routes {
        let mut app = test_app();
        app.latest_version = Some("9.9.9".into());
        app.focused_wallet = Some(app.wallets[0].id);
        app.input.focus = 1;
        app.route = route;
        let hints = super::footer_hints(&app);
        let lines = super::hint_height(&hints, 80);
        assert!(
            lines <= 2,
            "{route:?} hints must wrap to at most 2 lines at width 80, got {lines}: {hints}"
        );
    }
}

#[test]
fn history_title_uses_transfers_noun() {
    let mut app = test_app();
    app.toasts.clear();
    app.latest_version = None;
    app.route = Route::History;

    let from = app.wallets[0].id;
    let to = app.wallets[1].pubkey.clone();
    app.db.call_blocking(move |d| {
        let i = d.create_intent(from, &to, 500_000_000, None).unwrap();
        d.mark_signed(
            i.id,
            "5Hx9c4kQ2mWnTr8sV1pLb3JfYx7aZ2nQ8mK9kQpVtRf2dGhEjKpLmNoPqRsTuVwXy",
            "BhAsH",
            1000,
            5023,
            b"wire",
        )
        .unwrap();
    });
    app.focused_wallet = Some(app.wallets[0].id);
    app.refresh_detail_intents_blocking();

    let out = render(&mut app);
    assert!(
        out.contains("Transfers —"),
        "history title must use the transfers noun:\n{out}"
    );
    assert!(
        !out.contains("History —"),
        "history title must drop the 'History —' label:\n{out}"
    );
}

#[test]
fn empty_history_uses_capitalized_transfers_copy() {
    let mut app = test_app();
    app.toasts.clear();
    app.latest_version = None;
    app.focused_wallet = Some(app.wallets[0].id);
    app.detail_intents.clear();
    app.route = Route::History;

    let out = render(&mut app);
    assert!(
        out.contains("No transfers yet"),
        "empty history must use the capitalized transfers copy:\n{out}"
    );
    assert!(
        !out.contains("no transfers yet"),
        "empty history must not use the lowercase copy:\n{out}"
    );
}

#[test]
fn syncing_status_avoids_reconciling_jargon() {
    let mut app = test_app();
    app.toasts.clear();
    app.latest_version = None;
    app.reconcile_done = false;
    app.net_status = NetStatus::Syncing;
    app.route = Route::WalletList;
    let mnemonic = crate::crypto::parse_mnemonic(
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon \
         about",
    )
    .unwrap();
    app.seed = Some(crate::crypto::seed_from_mnemonic(&mnemonic));

    let out = render(&mut app);
    assert!(
        out.contains("syncing transfers"),
        "status bar must read 'syncing transfers':\n{out}"
    );
    assert!(
        !out.to_lowercase().contains("reconciling"),
        "no live status string may expose 'reconciling':\n{out}"
    );
}

#[test]
fn update_notice_renders_in_footer_right_on_all_routes() {
    let mut app = test_app();
    app.toasts.clear();
    app.route = Route::Settings;
    app.latest_version = Some("9.9.9".into());

    let hints = super::footer_hints(&app);
    assert!(
        !hints.to_lowercase().contains("upgrade"),
        "footer hints must no longer carry an upgrade suffix: {hints}"
    );

    for route in [Route::Settings, Route::History] {
        let mut app = test_app();
        app.toasts.clear();
        app.route = route;
        app.latest_version = Some("9.9.9".into());
        let out = render(&mut app);
        assert!(
            out.contains("v9.9.9 · "),
            "{route:?} footer must show the versioned upgrade line:\n{out}"
        );

        let mut app = test_app();
        app.toasts.clear();
        app.route = route;
        app.latest_version = None;
        let out = render(&mut app);
        assert!(
            !out.contains("v9.9.9"),
            "{route:?} footer must hide the notice with no update:\n{out}"
        );
    }
}

#[test]
fn status_dot_shows_rpc_host_and_offline_word() {
    let mut app = test_app();
    app.toasts.clear();
    app.latest_version = None;
    app.route = Route::WalletList;
    app.net_status = NetStatus::Online;

    let host = crate::solana::rpc::rpc_host_label(&app.rpc_url);
    let out = render(&mut app);
    assert!(
        out.contains(&host),
        "status bar must show the RPC host label {host:?}:\n{out}"
    );
    assert!(
        !out.contains("● syncing"),
        "status bar must drop the bare '● syncing' label:\n{out}"
    );

    app.net_status = NetStatus::Offline;
    let out = render(&mut app);
    assert!(
        out.contains("offline"),
        "offline status bar must surface the 'offline' word:\n{out}"
    );
}

#[test]
fn post_send_toasts_carry_transfer_noun() {
    let mut app = test_app();
    let cur_gen = app.generation.load(std::sync::atomic::Ordering::SeqCst);

    app.apply_app_event(AppEvent::TransferResult {
        intent_id: 1,
        outcome: TransferOutcome::Submitted {
            signature: "5Hx9c4kQ2mWnTr8sV1pLb3JfYx7aZ2nQ8mK9kQpVtRf".into(),
        },
        transfer: None,
        generation: cur_gen,
    });
    assert!(
        app.toasts
            .iter()
            .any(|t| t.text.contains("Transfer submitted")),
        "submitted toast must label the transfer noun: {:?}",
        app.toasts.iter().map(|t| &t.text).collect::<Vec<_>>()
    );

    app.toasts.clear();
    app.apply_app_event(AppEvent::TransferResult {
        intent_id: 1,
        outcome: TransferOutcome::StillPending {
            signature: "5Hx9c4kQ2mWnTr8sV1pLb3JfYx7aZ2nQ8mK9kQpVtRf".into(),
        },
        transfer: None,
        generation: cur_gen,
    });
    assert!(
        app.toasts
            .iter()
            .any(|t| t.text.contains("Transfer still pending")),
        "still-pending toast must label the transfer noun: {:?}",
        app.toasts.iter().map(|t| &t.text).collect::<Vec<_>>()
    );
}

#[test]
fn settings_confirmation_row_avoids_commitment_jargon() {
    let mut app = test_app();
    app.toasts.clear();
    app.latest_version = None;
    app.route = Route::Settings;

    let out = render(&mut app);
    assert!(
        out.contains("confirmations"),
        "settings must label the row 'confirmations':\n{out}"
    );
    assert!(
        out.contains("standard"),
        "settings must show the plain 'standard' value:\n{out}"
    );
    assert!(
        !out.contains("commitment"),
        "settings must drop the 'commitment' jargon:\n{out}"
    );
}
