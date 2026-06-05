use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use ratatui::crossterm::event::{DisableBracketedPaste, EnableBracketedPaste, EventStream};
use ratatui::crossterm::execute;
use tokio::sync::mpsc;

use silo::app::{App, AppEvent, Command};
use silo::db::Db;
use silo::price::{PriceCache, SolPrice};
use silo::solana::rpc::Rpc;
use silo::types::{Currency, Network};
use silo::{clipboard, input, ui, worker};

#[tokio::main]
async fn main() -> Result<()> {
    clipboard::maybe_run_clip_daemon();

    let dir = config_dir();
    std::fs::create_dir_all(&dir)?;

    let _instance_lock = acquire_single_instance(&dir)?;

    let profiles = silo::profiles::load(&dir);
    silo::profiles::cleanup_orphans(&dir, &profiles);
    let first_run = profiles.is_empty();
    let active_id = if first_run {
        silo::profiles::new_id()
    } else {
        profiles[0].id.clone()
    };
    let profile_dir = silo::profiles::dir_for(&dir, &active_id);
    std::fs::create_dir_all(&profile_dir)?;

    let db = Db::open(&profile_dir.join("silo.db"))?;
    let rpc_url = db
        .get_meta("rpc_url")?
        .unwrap_or_else(|| Network::MainnetBeta.default_rpc_url().to_string());
    let currency = db
        .get_meta("currency")?
        .and_then(|s| Currency::from_code(&s))
        .unwrap_or(Currency::Usd);
    let priority_micro = db
        .get_meta("priority_fee_micro")?
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(silo::money::DEFAULT_PRIORITY_FEE_MICRO);
    let last_price = db
        .get_meta("last_price")?
        .and_then(|s| SolPrice::from_meta_json(&s))
        .filter(|p| p.currency == currency);
    let auto_lock_mins = db
        .get_meta("auto_lock_minutes")?
        .and_then(|s| s.parse::<u64>().ok())
        .map(|m| {
            m.clamp(
                silo::app::AUTO_LOCK_MIN_MINUTES,
                silo::app::AUTO_LOCK_MAX_MINUTES,
            )
        });
    let vault_path = profile_dir.join("vault.json");

    let db = Arc::new(Mutex::new(db));
    let client = worker::build_client()?;
    let rpc = Arc::new(Mutex::new(Rpc::new(client.clone(), rpc_url.clone())));
    let price = Arc::new(PriceCache::new());
    if let Some(p) = last_price {
        price.seed(p);
    }
    let generation = Arc::new(std::sync::atomic::AtomicU64::new(0));

    let (cmd_tx, cmd_rx) = mpsc::channel::<(u64, Command)>(64);
    let (evt_tx, evt_rx) = mpsc::channel::<AppEvent>(256);
    let workers = worker::spawn_workers(
        cmd_rx,
        evt_tx,
        db.clone(),
        rpc.clone(),
        price.clone(),
        client.clone(),
        generation.clone(),
    );

    let mut app = App::new(
        db.clone(),
        price.clone(),
        cmd_tx.clone(),
        generation.clone(),
        rpc.clone(),
        client.clone(),
        dir.clone(),
        rpc_url,
        vault_path,
    );
    app.currency = currency;
    app.priority_micro = priority_micro;
    if let Some(m) = auto_lock_mins {
        app.auto_lock_after = std::time::Duration::from_secs(m * 60);
    }
    app.profiles = profiles;
    app.current_profile = Some(active_id);
    app.reload_wallets();

    if first_run {
        app.route = silo::app::Route::Setup;
        app.setup = silo::app::SetupState::default();
    } else {
        app.route = silo::app::Route::ProfileSelect;
    }

    let mut terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);
    let result = run(&mut terminal, app, evt_rx, workers).await;
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    result
}

async fn run(
    terminal: &mut ratatui::DefaultTerminal,
    mut app: App,
    mut evt_rx: mpsc::Receiver<AppEvent>,
    mut workers: tokio::task::JoinHandle<()>,
) -> Result<()> {
    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(50));

    while app.running {
        terminal.draw(|f| ui::render(f, &mut app))?;
        tokio::select! {
            maybe_ev = events.next() => match maybe_ev {
                Some(Ok(ev)) => {
                    app.note_activity();
                    input::handle_event(&mut app, ev);
                }
                _ => app.running = false,
            },
            maybe_app_ev = evt_rx.recv() => match maybe_app_ev {
                Some(app_ev) => app.apply_app_event(app_ev),
                None => app.running = false,
            },
            _ = &mut workers => app.running = false,
            _ = ticker.tick() => {
                app.tick();
                app.maybe_auto_lock();
                app.maybe_auto_refresh();
            }
        }
    }

    use zeroize::Zeroize;
    if app.seed.is_some() {
        app.lock();
    }
    app.input.zeroize_secrets();
    app.setup.mnemonic_words.zeroize();
    Ok(())
}

fn acquire_single_instance(dir: &Path) -> Result<std::fs::File> {
    let path = dir.join("silo.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => {
            bail!("another silo instance is already running")
        }
        Err(std::fs::TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("acquiring single-instance lock at {}", path.display()))
        }
    }
}

fn config_dir() -> PathBuf {
    if let Ok(x) = std::env::var("SILO_CONFIG_DIR") {
        return PathBuf::from(x);
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(h) = std::env::var("HOME") {
            return PathBuf::from(h).join("Library/Application Support/silo");
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Ok(a) = std::env::var("APPDATA") {
            return PathBuf::from(a).join("silo");
        }
    }
    if let Ok(x) = std::env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(x).join("silo");
    }
    if let Ok(h) = std::env::var("HOME") {
        return PathBuf::from(h).join(".config/silo");
    }
    PathBuf::from(".silo")
}
