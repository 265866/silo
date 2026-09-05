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
mod update;
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

    if let Some(arg) = std::env::args().nth(1) {
        match arg.as_str() {
            "--version" | "-V" => {
                println!("silo {}", crate::update::CURRENT_VERSION);
                return Ok(());
            }
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            other => {
                eprintln!("silo: unknown argument '{other}' (try --help)");
                std::process::exit(2);
            }
        }
    }

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
    let update_latest_seen = db.get_meta("update_latest_seen")?;
    let update_check_due = update_check_due(&db)?;
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
    app.init_update_check(update_latest_seen, update_check_due);

    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        disable_bracketed_paste(&mut std::io::stdout());
        prev_hook(info);
    }));

    let terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);
    run(terminal, app, evt_rx, workers).await
}

fn disable_bracketed_paste(w: &mut impl std::io::Write) {
    let _ = execute!(w, DisableBracketedPaste);
}

fn update_check_due(db: &Db) -> Result<bool> {
    let last = db
        .get_meta("update_last_check")?
        .and_then(|s| s.parse::<u64>().ok());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Ok(match last {
        Some(ts) => now.saturating_sub(ts) >= crate::update::CHECK_INTERVAL_SECS,
        None => true,
    })
}

fn print_help() {
    println!(
        "silo {} — SOL-only Solana wallet manager",
        update::CURRENT_VERSION
    );
    println!();
    println!("USAGE:");
    println!("    silo             launch the wallet (requires a TTY)");
    println!("    silo --version   print the version and exit");
    println!("    silo --help      show this help and exit");
    println!();
    println!("On launch silo checks GitHub for a newer release and shows an in-app");
    println!("banner with how to upgrade. Toggle the check in Settings.");
}

struct Shutdown {
    #[cfg(unix)]
    term: Option<tokio::signal::unix::Signal>,
    #[cfg(unix)]
    hup: Option<tokio::signal::unix::Signal>,
}

impl Shutdown {
    fn new() -> Self {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Shutdown {
                term: signal(SignalKind::terminate()).ok(),
                hup: signal(SignalKind::hangup()).ok(),
            }
        }
        #[cfg(not(unix))]
        {
            Shutdown {}
        }
    }

    async fn recv(&mut self) {
        #[cfg(unix)]
        {
            match (self.term.as_mut(), self.hup.as_mut()) {
                (Some(t), Some(h)) => {
                    tokio::select! {
                        _ = t.recv() => {}
                        _ = h.recv() => {}
                    }
                }
                (Some(t), None) => {
                    let _ = t.recv().await;
                }
                (None, Some(h)) => {
                    let _ = h.recv().await;
                }
                (None, None) => std::future::pending::<()>().await,
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

async fn run(
    mut terminal: ratatui::DefaultTerminal,
    mut app: App,
    mut evt_rx: mpsc::Receiver<AppEvent>,
    mut workers: tokio::task::JoinHandle<()>,
) -> Result<()> {
    let mut events = EventStream::new();
    let active_tick = std::time::Duration::from_millis(50);
    let ambient_tick = std::time::Duration::from_millis(100);
    let tick = tokio::time::sleep(active_tick);
    tokio::pin!(tick);
    let mut shutdown = Shutdown::new();
    let mut worker_done = false;

    let loop_result = loop {
        if app.take_redraw()
            && let Err(e) = terminal.draw(|f| ui::render(f, &mut app))
        {
            break Err(e.into());
        }
        tokio::select! {
            maybe_ev = events.next() => {
                match maybe_ev {
                    Some(Ok(ev)) => {
                        app.note_activity();
                        input::handle_event(&mut app, ev);
                    }
                    _ => app.stop(),
                }
                app.request_redraw();
            }
            maybe_app_ev = evt_rx.recv() => {
                match maybe_app_ev {
                    Some(app_ev) => app.apply_app_event(app_ev),
                    None => app.stop(),
                }
                app.request_redraw();
            }
            worker_result = &mut workers => {
                worker_done = true;
                if let Err(e) = worker_result {
                    break Err(anyhow::anyhow!("background worker task failed: {e}"));
                }
                app.stop();
            },
            _ = &mut tick => {
                app.tick();
                app.maybe_auto_lock();
                app.maybe_auto_refresh();
                app.request_redraw();
                let period = if app.animations_active() { active_tick } else { ambient_tick };
                tick.as_mut().reset(tokio::time::Instant::now() + period);
            }
            _ = shutdown.recv() => {
                app.stop();
            }
        }
        if app.animations_active() {
            let soon = tokio::time::Instant::now() + active_tick;
            if soon < tick.deadline() {
                tick.as_mut().reset(soon);
            }
        }
        if !app.is_running() {
            break Ok(());
        }
    };

    // Release terminal ownership before waiting for in-flight work. Workers still
    // drain normally so transaction persistence is not interrupted.
    drop(events);
    drop(terminal);
    disable_bracketed_paste(&mut std::io::stdout());
    ratatui::restore();

    app.scrub_for_exit();
    drop(app);
    if !worker_done && let Err(e) = workers.await {
        return Err(anyhow::anyhow!("background worker task failed: {e}"));
    }
    loop_result
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    use super::Shutdown;
    use super::disable_bracketed_paste;

    #[test]
    fn teardown_emits_disable_bracketed_paste() {
        let mut buf: Vec<u8> = Vec::new();
        disable_bracketed_paste(&mut buf);
        assert_eq!(buf, b"\x1b[?2004l");
    }

    fn console_test_app(config_dir: &std::path::Path) -> super::App {
        let db = super::Storage::new(super::Db::open_memory().unwrap());
        let client = super::worker::build_client().unwrap();
        let rpc_url = "http://127.0.0.1:9".to_string();
        let rpc = std::sync::Arc::new(std::sync::Mutex::new(super::Rpc::new(
            client.clone(),
            rpc_url.clone(),
        )));
        let (cmd_tx, _) = super::mpsc::channel(1);
        super::App::new(
            db,
            std::sync::Arc::new(super::PriceCache::new()),
            cmd_tx,
            std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            rpc,
            client,
            config_dir.to_path_buf(),
            rpc_url,
            config_dir.join("vault.json"),
        )
    }

    #[tokio::test]
    #[ignore = "requires an exclusive real console; run with --ignored --exact --nocapture"]
    async fn teardown_restores_terminal_before_delayed_worker_completion() {
        use ratatui::crossterm::terminal::is_raw_mode_enabled;

        let dir = tempfile::tempdir().unwrap();
        let app = console_test_app(dir.path());
        let db = app.db.clone();
        let worker_db = db.clone();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let workers = tokio::spawn(async move {
            if release_rx.await.is_ok() {
                worker_db
                    .call(|d| d.set_meta("teardown_worker_finished", "yes"))
                    .await
                    .unwrap();
            }
        });
        let (evt_tx, evt_rx) = super::mpsc::channel(1);
        // End the real event loop without waiting for keyboard input or a timer.
        drop(evt_tx);

        println!("ORIGINAL SCREEN: before terminal initialization");
        let mut terminal = ratatui::init();
        let _ = super::execute!(std::io::stdout(), super::EnableBracketedPaste);
        terminal
            .draw(|frame| {
                frame.render_widget(
                    ratatui::widgets::Paragraph::new("ALTERNATE SCREEN: terminal owned by silo"),
                    frame.area(),
                );
            })
            .unwrap();
        assert!(is_raw_mode_enabled().unwrap());
        // These pauses only make the real-console recording readable. Ordering
        // assertions below depend on the held channel, not elapsed wall time.
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let run = super::run(terminal, app, evt_rx, workers);
        tokio::pin!(run);
        let first_poll = std::future::poll_fn(|cx| {
            std::task::Poll::Ready(std::future::Future::poll(run.as_mut(), cx))
        })
        .await;
        assert!(first_poll.is_pending());
        let raw_mode = is_raw_mode_enabled().unwrap();
        if raw_mode {
            // Keep the console usable even when reproducing the old ordering.
            disable_bracketed_paste(&mut std::io::stdout());
        }
        assert!(
            !raw_mode,
            "terminal must be restored while the worker is still held"
        );
        assert!(
            db.call(|d| d.get_meta("teardown_worker_finished"))
                .await
                .unwrap()
                .is_none()
        );
        println!("RESTORED SCREEN: raw mode disabled; worker still held");
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        release_tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), &mut run)
            .await
            .expect("released worker must drain")
            .unwrap();
        assert_eq!(
            db.call(|d| d.get_meta("teardown_worker_finished"))
                .await
                .unwrap()
                .as_deref(),
            Some("yes")
        );
        println!("WORKER DRAINED: persistence completed after restoration");
    }

    #[tokio::test]
    #[ignore = "requires an exclusive real console; run with --ignored --exact --nocapture"]
    async fn teardown_restores_terminal_after_worker_error() {
        let dir = tempfile::tempdir().unwrap();
        let app = console_test_app(dir.path());
        let (_evt_tx, evt_rx) = super::mpsc::channel(1);
        let workers = tokio::spawn(std::future::pending::<()>());
        workers.abort();
        let terminal = ratatui::init();
        let _ = super::execute!(std::io::stdout(), super::EnableBracketedPaste);
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            super::run(terminal, app, evt_rx, workers),
        )
        .await
        .expect("worker failure must end the event loop");
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("background worker task failed")
        );
        assert!(!ratatui::crossterm::terminal::is_raw_mode_enabled().unwrap());
        println!("RESTORED SCREEN: worker error returned with raw mode disabled");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn shutdown_recv_wakes_on_sigterm() {
        let mut shutdown = Shutdown::new();
        unsafe {
            libc::raise(libc::SIGTERM);
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), shutdown.recv())
            .await
            .expect("SIGTERM must wake the shutdown future so the run loop can stop");
    }
}
