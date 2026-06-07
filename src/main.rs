#![allow(dead_code, unused_imports)]

use std::sync::{Arc, Mutex};

mod app;
mod clipboard;
mod crypto;
mod db;
mod input;
mod money;
mod platform;
mod price;
mod profiles;
mod solana;
mod sync;
mod types;
mod ui;
mod vault;
mod worker;

use anyhow::Result;
use futures_util::StreamExt;
use ratatui::crossterm::event::{DisableBracketedPaste, EnableBracketedPaste, EventStream};
use ratatui::crossterm::execute;
use tokio::sync::mpsc;

use crate::app::{App, AppEvent, Command};
use crate::db::{Db, Storage};
use crate::price::{PriceCache, SolPrice};
use crate::solana::rpc::Rpc;
use crate::types::{Currency, Network};

#[tokio::main]
async fn main() -> Result<()> {
    clipboard::maybe_run_clip_daemon();

    let dir = crate::platform::config_dir();
    crate::profiles::ensure_private_dir(&dir)?;

    let _instance_lock = crate::platform::acquire_single_instance(&dir)?;

    let profiles = crate::profiles::load(&dir)?;
    crate::profiles::cleanup_orphans(&dir, &profiles);
    let first_run = profiles.is_empty();
    let active_id = if first_run {
        crate::profiles::new_id()
    } else {
        profiles[0].id.clone()
    };
    let profile_dir = crate::profiles::dir_for(&dir, &active_id)?;
    crate::profiles::ensure_private_dir(&profile_dir)?;

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
        .unwrap_or(crate::money::DEFAULT_PRIORITY_FEE_MICRO);
    let last_price = db
        .get_meta("last_price")?
        .map(|s| SolPrice::from_meta_json(&s))
        .transpose()?
        .filter(|p| p.currency == currency && !p.is_stale());
    let auto_lock_mins = db
        .get_meta("auto_lock_minutes")?
        .and_then(|s| s.parse::<u64>().ok())
        .map(|m| {
            m.clamp(
                crate::app::AUTO_LOCK_MIN_MINUTES,
                crate::app::AUTO_LOCK_MAX_MINUTES,
            )
        });
    let vault_path = profile_dir.join("vault.json");

    let db = Storage::new(db);
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
    drop(cmd_tx);
    app.restore_startup_state(
        currency,
        priority_micro,
        auto_lock_mins,
        profiles,
        active_id,
        first_run,
    );

    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        disable_bracketed_paste(&mut std::io::stdout());
        prev_hook(info);
    }));

    let mut terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);
    let result = run(&mut terminal, app, evt_rx, workers).await;
    disable_bracketed_paste(&mut std::io::stdout());
    ratatui::restore();
    result
}

fn disable_bracketed_paste(w: &mut impl std::io::Write) {
    let _ = execute!(w, DisableBracketedPaste);
}

async fn run(
    terminal: &mut ratatui::DefaultTerminal,
    mut app: App,
    mut evt_rx: mpsc::Receiver<AppEvent>,
    mut workers: tokio::task::JoinHandle<()>,
) -> Result<()> {
    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(50));
    let mut worker_done = false;

    let loop_result = loop {
        if let Err(e) = terminal.draw(|f| ui::render(f, &mut app)) {
            break Err(e.into());
        }
        tokio::select! {
            maybe_ev = events.next() => match maybe_ev {
                Some(Ok(ev)) => {
                    app.note_activity();
                    input::handle_event(&mut app, ev);
                }
                _ => app.stop(),
            },
            maybe_app_ev = evt_rx.recv() => match maybe_app_ev {
                Some(app_ev) => app.apply_app_event(app_ev),
                None => app.stop(),
            },
            worker_result = &mut workers => {
                worker_done = true;
                if let Err(e) = worker_result {
                    break Err(anyhow::anyhow!("background worker task failed: {e}"));
                }
                app.stop();
            },
            _ = ticker.tick() => {
                app.tick();
                app.maybe_auto_lock();
                app.maybe_auto_refresh();
            }
        }
        if !app.is_running() {
            break Ok(());
        }
    };

    app.scrub_for_exit();
    drop(app);
    if !worker_done && let Err(e) = workers.await {
        return Err(anyhow::anyhow!("background worker task failed: {e}"));
    }
    loop_result
}

#[cfg(test)]
mod tests {
    use super::disable_bracketed_paste;

    #[test]
    fn teardown_emits_disable_bracketed_paste() {
        let mut buf: Vec<u8> = Vec::new();
        disable_bracketed_paste(&mut buf);
        assert_eq!(buf, b"\x1b[?2004l");
    }
}
