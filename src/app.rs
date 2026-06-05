use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::mpsc;
use zeroize::{Zeroize, Zeroizing};

use crate::clipboard::ClipboardManager;
use crate::crypto::Seed;
use crate::db::Db;
use crate::price::{PriceCache, SolPrice};
use crate::sync::MutexExt;
use crate::types::{Intent, NetStatus, TransferOutcome, WalletRow};
use crate::ui::theme::Theme;

use ratatui::widgets::TableState;

#[cfg(target_os = "linux")]
fn boottime_now() -> Option<Duration> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) };
    if rc == 0 && ts.tv_sec >= 0 {
        Some(Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32))
    } else {
        None
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Route {
    ProfileSelect,
    Unlock,
    Setup,
    WalletList,
    WalletDetail,
    Send,
    History,
    AuditLog,
    Settings,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SetupStage {
    Choose,
    ShowMnemonic,
    ConfirmMnemonic,
    ImportEntry,
    SetPassphrase,
}

#[derive(Debug)]
pub enum Command {
    Reconcile,
    FetchRentExempt,
    FetchPrice,
    RefreshBalances {
        include_archived: bool,
    },
    PrepareSend {
        from_id: i64,
        to: String,
        lamports: u64,
        priority_micro: u64,
    },
    Broadcast {
        intent_id: i64,
    },
    ChangeRpc {
        url: String,
    },
}

#[derive(Debug)]
pub enum AppEvent {
    ReconcileComplete {
        resolved: usize,
        generation: u64,
    },
    ReconcileFailedOffline,
    Balances(Vec<(i64, u64)>),
    BalancesFailed {
        reason: String,
    },
    Price(SolPrice),
    RentExempt(u64),
    SendPrepared {
        from_id: i64,
        to: String,
        lamports: u64,
        blockhash: String,
        lvbh: u64,
        fee: u64,
        dest_balance: u64,
        priority_micro: u64,
    },
    TransferResult {
        intent_id: i64,
        outcome: TransferOutcome,
        generation: u64,
    },
    NetStatus(NetStatus),
    Error(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Error,
}

pub struct Toast {
    pub text: String,
    pub kind: ToastKind,
    pub created: Instant,
}

pub struct Confetto {
    pub x: f32,
    pub y: f32,
    pub vx: f32,
    pub vy: f32,
    pub life: f32,
    pub glyph: char,
    pub color: ratatui::style::Color,
}

#[derive(Clone)]
pub enum PromptKind {
    Label(i64),
    Note(i64),
    TxNote(i64),
    RpcUrl,
    ProfileName(String),
}

impl PromptKind {
    pub fn multiline(&self) -> bool {
        matches!(self, PromptKind::Note(_) | PromptKind::TxNote(_))
    }
}

#[derive(Clone)]
pub enum ConfirmAction {
    CreateWithEmptyPassphrase,
    DeleteProfile(String),
}

pub enum Modal {
    ConfirmSend,
    Confirm {
        title: String,
        body: String,
        action: ConfirmAction,
    },
    Error {
        title: String,
        body: String,
    },
    Prompt {
        kind: PromptKind,
        title: String,
    },
}

pub struct PendingSend {
    pub from_id: i64,
    pub to: String,
    pub lamports: u64,
    pub blockhash: String,
    pub lvbh: u64,
    pub fee: u64,
    pub dest_balance: u64,
    pub priority_micro: u64,
    pub prepared_at: Instant,
}

#[derive(Default)]
pub struct InputState {
    pub passphrase: Zeroizing<String>,
    pub passphrase2: Zeroizing<String>,
    pub import_phrase: Zeroizing<String>,
    pub send_to: String,
    pub send_amount: String,
    pub prompt_text: String,
    pub focus: usize,
    pub send_in_fiat: bool,
}

impl InputState {
    pub fn zeroize_secrets(&mut self) {
        self.passphrase.zeroize();
        self.passphrase2.zeroize();
        self.import_phrase.zeroize();
        self.focus = 0;
    }
}

pub struct SetupState {
    pub stage: SetupStage,
    pub creating: bool,
    pub mnemonic_words: Vec<String>,
    pub confirm_words: Vec<String>,
    pub confirm_focus: usize,
}

impl Default for SetupState {
    fn default() -> Self {
        SetupState {
            stage: SetupStage::Choose,
            creating: true,
            mnemonic_words: Vec::new(),
            confirm_words: Vec::new(),
            confirm_focus: 0,
        }
    }
}

impl SetupState {
    pub fn begin_confirm(&mut self, n: usize) {
        self.scrub_confirm();
        self.confirm_words = vec![String::new(); n];
        self.confirm_focus = 0;
    }

    pub fn scrub_confirm(&mut self) {
        for w in &mut self.confirm_words {
            w.zeroize();
        }
        self.confirm_words.clear();
        self.confirm_focus = 0;
    }
}

pub const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

const BASELINE_REFRESH: Duration = Duration::from_secs(60);
const HOT_REFRESH_MIN: Duration = Duration::from_secs(4);
const HOT_REFRESH_MAX: Duration = Duration::from_secs(30);
const HOT_WINDOW: Duration = Duration::from_secs(5 * 60);
const HOT_DECAY: f32 = 1.6;

pub const TOAST_TTL: Duration = Duration::from_millis(2500);
const MAX_TOASTS: usize = 4;

pub const DEFAULT_AUTO_LOCK_MINUTES: u64 = 10;
pub const AUTO_LOCK_MIN_MINUTES: u64 = 1;
pub const AUTO_LOCK_MAX_MINUTES: u64 = 120;

const LARGE_SEND_NUM: u64 = 9;
const LARGE_SEND_DEN: u64 = 10;

const PRICE_FLASH_DECAY: f32 = 0.07;
const BALANCE_EASE: f64 = 0.28;
const CONFETTI_GRAVITY: f32 = 0.10;
const CONFETTI_DRAG: f32 = 0.96;
const CONFETTI_FADE: f32 = 0.022;
const CONFETTI_BURST: usize = 60;
const CONFETTI_MAX: usize = 400;
const CONFETTI_FIELD_H: f32 = 400.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WalletListRow {
    Wallet(usize),
    ArchivedHeader,
}

pub struct App {
    pub running: bool,
    pub route: Route,
    pub db: Arc<Mutex<Db>>,
    pub price: Arc<PriceCache>,
    pub cmd_tx: mpsc::Sender<(u64, Command)>,
    pub generation: Arc<AtomicU64>,
    pub clip: ClipboardManager,
    pub theme: Theme,

    pub seed: Option<Seed>,
    pub reconcile_done: bool,
    pub net_status: NetStatus,
    pub rent_exempt_min: u64,
    pub priority_micro: u64,

    pub wallets: Vec<WalletRow>,
    pub list_state: TableState,
    pub archived_expanded: bool,
    pub history_state: TableState,
    pub audit_state: TableState,
    pub focused_wallet: Option<i64>,

    pub setup: SetupState,
    pub input: InputState,

    pub modal: Option<Modal>,
    pub toasts: Vec<Toast>,
    pub spinner_frame: usize,
    pub inflight: u32,

    pub detail_intents: Vec<Intent>,
    pub audit: Vec<crate::types::AuditEntry>,
    pub pending_send: Option<PendingSend>,
    pub send_confirm_armed: bool,

    pub last_activity: Instant,
    pub last_wall: SystemTime,
    #[cfg(target_os = "linux")]
    pub last_boottime: Option<Duration>,
    pub auto_lock_after: Duration,
    pub last_balance_refresh: Instant,
    pub hot_until: Option<Instant>,
    pub hot_interval: Duration,
    pub rpc_url: String,
    pub vault_path: std::path::PathBuf,
    pub currency: crate::types::Currency,
    pub anim_balance: HashMap<i64, f64>,
    pub config_dir: std::path::PathBuf,
    pub rpc: Arc<Mutex<crate::solana::rpc::Rpc>>,
    pub client: reqwest::Client,
    pub profiles: Vec<crate::profiles::ProfileMeta>,
    pub profile_sel: usize,
    pub current_profile: Option<String>,

    pub frame: u64,
    pub confetti: Vec<Confetto>,
    confetti_rng: u64,
    pub last_area: ratatui::layout::Rect,
    pub price_flash: f32,
    pub price_up: bool,
    last_price: Option<f64>,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: Arc<Mutex<Db>>,
        price: Arc<PriceCache>,
        cmd_tx: mpsc::Sender<(u64, Command)>,
        generation: Arc<AtomicU64>,
        rpc: Arc<Mutex<crate::solana::rpc::Rpc>>,
        client: reqwest::Client,
        config_dir: std::path::PathBuf,
        rpc_url: String,
        vault_path: std::path::PathBuf,
    ) -> Self {
        let mut list_state = TableState::default();
        list_state.select(Some(0));
        App {
            running: true,
            route: Route::ProfileSelect,
            db,
            price,
            cmd_tx,
            generation,
            clip: ClipboardManager::new(),
            theme: Theme::dark(),
            seed: None,
            reconcile_done: false,
            net_status: NetStatus::Syncing,
            rent_exempt_min: crate::money::RENT_EXEMPT_MIN_0_DATA_FALLBACK,
            priority_micro: crate::money::DEFAULT_PRIORITY_FEE_MICRO,
            wallets: Vec::new(),
            list_state,
            archived_expanded: false,
            history_state: TableState::default(),
            audit_state: TableState::default(),
            focused_wallet: None,
            setup: SetupState::default(),
            input: InputState::default(),
            modal: None,
            toasts: Vec::new(),
            spinner_frame: 0,
            inflight: 0,
            detail_intents: Vec::new(),
            audit: Vec::new(),
            pending_send: None,
            send_confirm_armed: false,
            last_activity: Instant::now(),
            last_wall: SystemTime::now(),
            #[cfg(target_os = "linux")]
            last_boottime: boottime_now(),
            auto_lock_after: Duration::from_secs(DEFAULT_AUTO_LOCK_MINUTES * 60),
            last_balance_refresh: Instant::now(),
            hot_until: None,
            hot_interval: HOT_REFRESH_MIN,
            rpc_url,
            vault_path,
            currency: crate::types::Currency::Usd,
            anim_balance: HashMap::new(),
            config_dir,
            rpc,
            client,
            profiles: Vec::new(),
            profile_sel: 0,
            current_profile: None,
            frame: 0,
            confetti: Vec::new(),
            confetti_rng: 0x2545_F491_4F6C_DD1D,
            last_area: ratatui::layout::Rect::new(0, 0, 80, 24),
            price_flash: 0.0,
            price_up: true,
            last_price: None,
        }
    }

    pub fn send_cmd(&self, cmd: Command) {
        let g = self.generation.load(Ordering::SeqCst);
        let _ = self.cmd_tx.try_send((g, cmd));
    }

    fn bump_generation(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    pub fn switch_to_profile(&mut self, id: &str) -> anyhow::Result<()> {
        self.bump_generation();
        let db_path = crate::profiles::db_path(&self.config_dir, id);
        let new_db = Db::open(&db_path)?;
        *self.db.lock_recover() = new_db;
        self.price.clear();

        let url = self
            .db
            .lock_recover()
            .get_meta("rpc_url")
            .ok()
            .flatten()
            .unwrap_or_else(|| {
                crate::types::Network::MainnetBeta
                    .default_rpc_url()
                    .to_string()
            });
        *self.rpc.lock_recover() = crate::solana::rpc::Rpc::new(self.client.clone(), url.clone());
        self.rpc_url = url;
        self.currency = self
            .db
            .lock_recover()
            .get_meta("currency")
            .ok()
            .flatten()
            .and_then(|s| crate::types::Currency::from_code(&s))
            .unwrap_or(crate::types::Currency::Usd);
        self.priority_micro = self
            .db
            .lock_recover()
            .get_meta("priority_fee_micro")
            .ok()
            .flatten()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(crate::money::DEFAULT_PRIORITY_FEE_MICRO);
        if let Some(m) = self
            .db
            .lock_recover()
            .get_meta("auto_lock_minutes")
            .ok()
            .flatten()
            .and_then(|s| s.parse::<u64>().ok())
        {
            self.auto_lock_after =
                Duration::from_secs(m.clamp(AUTO_LOCK_MIN_MINUTES, AUTO_LOCK_MAX_MINUTES) * 60);
        }
        if let Some(p) = self
            .db
            .lock_recover()
            .get_meta("last_price")
            .ok()
            .flatten()
            .and_then(|s| crate::price::SolPrice::from_meta_json(&s))
            .filter(|p| p.currency == self.currency)
        {
            self.price.seed(p);
        }

        self.vault_path = crate::profiles::vault_path(&self.config_dir, id);
        self.current_profile = Some(id.to_string());
        self.seed = None;
        self.wallets.clear();
        self.anim_balance.clear();
        self.detail_intents.clear();
        self.focused_wallet = None;
        self.reconcile_done = false;
        self.net_status = NetStatus::Syncing;
        self.hot_until = None;
        self.reload_wallets();

        self.send_cmd(Command::FetchRentExempt);
        self.send_cmd(Command::FetchPrice);
        self.request_balance_refresh();
        Ok(())
    }

    pub fn begin_new_profile(&mut self) {
        self.bump_generation();
        self.input.zeroize_secrets();
        self.setup
            .mnemonic_words
            .iter_mut()
            .for_each(|w| w.zeroize());
        self.setup.scrub_confirm();

        let id = crate::profiles::new_id();
        let dir = crate::profiles::dir_for(&self.config_dir, &id);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.toast_err(format!("could not create profile dir: {e}"));
            return;
        }
        match Db::open(&dir.join("silo.db")) {
            Ok(d) => *self.db.lock_recover() = d,
            Err(e) => {
                self.toast_err(format!("could not open profile DB: {e}"));
                return;
            }
        }
        self.price.clear();
        self.current_profile = Some(id);
        self.vault_path = dir.join("vault.json");
        self.seed = None;
        self.wallets.clear();
        self.anim_balance.clear();
        self.focused_wallet = None;
        self.setup = SetupState::default();
        self.route = Route::Setup;
    }

    pub fn reload_profiles(&mut self) {
        self.profiles = crate::profiles::load(&self.config_dir);
        if self.profile_sel >= self.profiles.len() {
            self.profile_sel = self.profiles.len().saturating_sub(1);
        }
    }

    pub fn note_activity(&mut self) {
        self.last_activity = Instant::now();
        self.last_wall = SystemTime::now();
        #[cfg(target_os = "linux")]
        {
            self.last_boottime = boottime_now();
        }
    }

    fn idle_elapsed(&self) -> Duration {
        #[cfg(target_os = "linux")]
        {
            if let (Some(prev), Some(now)) = (self.last_boottime, boottime_now()) {
                return now.saturating_sub(prev);
            }
        }
        let mono = self.last_activity.elapsed();
        match self.last_wall.elapsed() {
            Ok(wall) => mono.max(wall),
            Err(_) => self.auto_lock_after,
        }
    }

    pub fn maybe_auto_lock(&mut self) {
        if self.seed.is_some() && self.idle_elapsed() >= self.auto_lock_after {
            self.lock();
            self.toast_info("Auto-locked after inactivity");
        }
    }

    pub fn request_balance_refresh(&mut self) {
        self.last_balance_refresh = Instant::now();
        self.inflight += 1;
        self.send_cmd(Command::RefreshBalances {
            include_archived: self.archived_expanded,
        });
    }

    pub fn arm_hot_refresh(&mut self) {
        self.hot_until = Some(Instant::now() + HOT_WINDOW);
        self.hot_interval = HOT_REFRESH_MIN;
    }

    pub fn maybe_auto_refresh(&mut self) {
        if self.seed.is_none() {
            return;
        }
        if !matches!(
            self.route,
            Route::WalletList | Route::WalletDetail | Route::History
        ) {
            return;
        }
        if self.inflight > 0 {
            return;
        }

        let now = Instant::now();
        let interval = match self.hot_until {
            Some(until) if now < until => self.hot_interval,
            Some(_) => {
                self.hot_until = None;
                BASELINE_REFRESH
            }
            None => BASELINE_REFRESH,
        };
        if now.duration_since(self.last_balance_refresh) < interval {
            return;
        }
        if self.hot_until.is_some() {
            self.hot_interval = self.hot_interval.mul_f32(HOT_DECAY).min(HOT_REFRESH_MAX);
        }
        self.request_balance_refresh();
    }

    pub fn lock(&mut self) {
        self.seed = None;
        self.input.zeroize_secrets();
        self.modal = None;
        self.pending_send = None;
        self.setup
            .mnemonic_words
            .iter_mut()
            .for_each(|w| w.zeroize());
        self.setup.mnemonic_words.clear();
        self.setup.scrub_confirm();
        self.anim_balance.clear();
        self.hot_until = None;
        self.route = Route::Unlock;
        if let Ok(mut d) = self.db.lock() {
            let _ = d.audit(crate::types::AuditEvent::Locked, &serde_json::json!({}));
            d.lock_audit_key();
        }
    }

    pub fn toast(&mut self, text: impl Into<String>, kind: ToastKind) {
        self.toasts.push(Toast {
            text: text.into(),
            kind,
            created: Instant::now(),
        });
        if self.toasts.len() > MAX_TOASTS {
            self.toasts.remove(0);
        }
    }
    pub fn toast_info(&mut self, t: impl Into<String>) {
        self.toast(t, ToastKind::Info);
    }
    pub fn toast_ok(&mut self, t: impl Into<String>) {
        self.toast(t, ToastKind::Success);
    }
    pub fn toast_err(&mut self, t: impl Into<String>) {
        self.toast(t, ToastKind::Error);
    }

    pub fn tick(&mut self) {
        self.spinner_frame = (self.spinner_frame + 1) % SPINNER.len();
        self.frame = self.frame.wrapping_add(1);
        let now = Instant::now();
        self.toasts
            .retain(|t| now.duration_since(t.created) < TOAST_TTL);
        self.animate_balances();
        self.advance_confetti();
        if self.price_flash > 0.0 {
            self.price_flash = (self.price_flash - PRICE_FLASH_DECAY).max(0.0);
        }
    }

    pub fn send_fee(&self) -> u64 {
        crate::money::total_fee(self.priority_micro)
    }

    fn rng_f32(&mut self) -> f32 {
        self.confetti_rng = self.confetti_rng.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.confetti_rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        ((z >> 40) as f32) / ((1u32 << 24) as f32)
    }

    pub fn celebrate_center(&mut self) {
        let a = self.last_area;
        let cx = a.x as f32 + a.width as f32 / 2.0;
        let cy = a.y as f32 + a.height as f32 * 0.42;
        self.celebrate(cx, cy);
    }

    pub fn celebrate(&mut self, cx: f32, cy: f32) {
        const GLYPHS: [char; 10] = ['✦', '✧', '★', '*', '•', '◆', '◇', '❄', '✺', '＋'];
        let palette = [
            self.theme.accent,
            self.theme.usd,
            self.theme.warn,
            self.theme.master,
            self.theme.border_focus,
            self.theme.danger,
            ratatui::style::Color::Rgb(255, 255, 255),
        ];
        for _ in 0..CONFETTI_BURST {
            let ang = self.rng_f32() * std::f32::consts::TAU;
            let speed = 0.6 + self.rng_f32() * 1.9;
            let glyph = GLYPHS[(self.rng_f32() * GLYPHS.len() as f32) as usize % GLYPHS.len()];
            let color = palette[(self.rng_f32() * palette.len() as f32) as usize % palette.len()];
            self.confetti.push(Confetto {
                x: cx,
                y: cy,
                vx: ang.cos() * speed * 1.9,
                vy: ang.sin() * speed - 0.5,
                life: 1.0,
                glyph,
                color,
            });
        }
        if self.confetti.len() > CONFETTI_MAX {
            let drop = self.confetti.len() - CONFETTI_MAX;
            self.confetti.drain(0..drop);
        }
    }

    fn advance_confetti(&mut self) {
        if self.confetti.is_empty() {
            return;
        }
        for p in &mut self.confetti {
            p.vy += CONFETTI_GRAVITY;
            p.vx *= CONFETTI_DRAG;
            p.x += p.vx;
            p.y += p.vy;
            p.life -= CONFETTI_FADE;
        }
        self.confetti
            .retain(|p| p.life > 0.0 && p.y < CONFETTI_FIELD_H);
    }

    fn animate_balances(&mut self) {
        for w in &self.wallets {
            if let Some(target) = w.balance_lamports {
                let cur = self.anim_balance.entry(w.id).or_insert(0.0);
                let t = target as f64;
                let diff = t - *cur;
                if diff.abs() < 1.0 {
                    *cur = t;
                } else {
                    *cur += diff * BALANCE_EASE;
                }
            }
        }
    }

    pub fn shown_balance(&self, w: &WalletRow) -> Option<u64> {
        let actual = w.balance_lamports?;
        Some(
            self.anim_balance
                .get(&w.id)
                .map(|c| c.round() as u64)
                .unwrap_or(actual),
        )
    }

    pub fn spinner(&self) -> char {
        SPINNER[self.spinner_frame]
    }

    pub fn wallet_list_rows(&self) -> Vec<WalletListRow> {
        let mut rows = Vec::with_capacity(self.wallets.len() + 1);
        for (i, w) in self.wallets.iter().enumerate() {
            if !w.archived {
                rows.push(WalletListRow::Wallet(i));
            }
        }
        if self.wallets.iter().any(|w| w.archived) {
            rows.push(WalletListRow::ArchivedHeader);
            if self.archived_expanded {
                for (i, w) in self.wallets.iter().enumerate() {
                    if w.archived {
                        rows.push(WalletListRow::Wallet(i));
                    }
                }
            }
        }
        rows
    }

    pub fn selected_wallet(&self) -> Option<&WalletRow> {
        let sel = self.list_state.selected()?;
        match self.wallet_list_rows().get(sel)? {
            WalletListRow::Wallet(i) => self.wallets.get(*i),
            WalletListRow::ArchivedHeader => None,
        }
    }

    pub fn selected_is_archived_header(&self) -> bool {
        self.list_state
            .selected()
            .and_then(|s| self.wallet_list_rows().get(s).copied())
            == Some(WalletListRow::ArchivedHeader)
    }

    pub fn toggle_archived_expanded(&mut self) {
        self.archived_expanded = !self.archived_expanded;
        self.clamp_list_selection();
        if self.archived_expanded {
            self.request_balance_refresh();
        }
    }

    pub fn clamp_list_selection(&mut self) {
        let n = self.wallet_list_rows().len();
        if n == 0 {
            self.list_state.select(None);
        } else {
            let sel = self.list_state.selected().unwrap_or(0).min(n - 1);
            self.list_state.select(Some(sel));
        }
    }

    pub fn select_wallet_by_id(&mut self, id: i64) {
        let pos = self.wallet_list_rows().iter().position(|r| match r {
            WalletListRow::Wallet(i) => self.wallets[*i].id == id,
            WalletListRow::ArchivedHeader => false,
        });
        if let Some(p) = pos {
            self.list_state.select(Some(p));
        }
    }

    pub fn focused_wallet(&self) -> Option<&WalletRow> {
        let id = self.focused_wallet?;
        self.wallets.iter().find(|w| w.id == id)
    }

    pub fn reload_wallets(&mut self) {
        if let Ok(d) = self.db.lock()
            && let Ok(mut rows) = d.list_wallets()
        {
            for r in &mut rows {
                if let Some(old) = self.wallets.iter().find(|w| w.id == r.id) {
                    r.balance_lamports = old.balance_lamports;
                }
            }
            self.wallets = rows;
        }
        self.clamp_list_selection();
    }

    pub fn sign_for(
        &self,
        account_index: u32,
        message: &[u8],
    ) -> anyhow::Result<ed25519_dalek::Signature> {
        use ed25519_dalek::Signer;
        let seed = self
            .seed
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("wallet is locked"))?;
        let key = crate::crypto::derive_signing_key(seed, account_index);
        Ok(key.sign(message))
    }

    pub fn price_now(&self) -> Option<SolPrice> {
        self.price.get()
    }

    pub fn reset_price_baseline(&mut self) {
        self.last_price = None;
        self.price_flash = 0.0;
    }

    pub fn compose_lamports(&self) -> Result<u64, String> {
        if self.input.send_in_fiat {
            let price = self
                .price_now()
                .ok_or("no price yet — press c to enter SOL")?;
            crate::money::fiat_to_lamports(&self.input.send_amount, price.value)
                .map_err(|e| e.to_string())
        } else {
            crate::money::parse_sol_to_lamports(&self.input.send_amount).map_err(|e| e.to_string())
        }
    }

    pub fn apply_app_event(&mut self, ev: AppEvent) {
        match ev {
            AppEvent::ReconcileComplete {
                resolved,
                generation,
            } => {
                if generation != self.generation.load(Ordering::SeqCst) {
                    return;
                }
                self.reconcile_done = true;
                if self.net_status == NetStatus::Syncing {
                    self.net_status = NetStatus::Online;
                }
                if resolved > 0 {
                    self.toast_info(format!("Reconciled {resolved} in-flight transfer(s)"));
                }
                self.request_balance_refresh();
            }
            AppEvent::ReconcileFailedOffline => {
                self.net_status = NetStatus::Offline;
                self.toast_err("Offline — reconcile pending, sends disabled");
            }
            AppEvent::Balances(list) => {
                if self.inflight > 0 {
                    self.inflight -= 1;
                }
                if self.net_status == NetStatus::Offline {
                    self.net_status = NetStatus::Online;
                    if !self.reconcile_done && self.seed.is_some() {
                        self.send_cmd(Command::Reconcile);
                    }
                }
                let mut deposit = false;
                for (id, lamports) in list {
                    if let Some(w) = self.wallets.iter_mut().find(|w| w.id == id) {
                        if w.balance_lamports.is_some_and(|prev| lamports > prev) {
                            deposit = true;
                        }
                        w.balance_lamports = Some(lamports);
                    }
                }
                if deposit {
                    self.arm_hot_refresh();
                }
            }
            AppEvent::BalancesFailed { reason } => {
                if self.inflight > 0 {
                    self.inflight -= 1;
                }
                if self.net_status != NetStatus::Offline {
                    self.toast_err(format!("Balance refresh failed: {reason}"));
                }
                self.net_status = NetStatus::Offline;
            }
            AppEvent::Price(p) => {
                if let Some(prev) = self.last_price
                    && (p.value - prev).abs() > f64::EPSILON
                {
                    self.price_up = p.value > prev;
                    self.price_flash = 1.0;
                }
                self.last_price = Some(p.value);
            }
            AppEvent::RentExempt(v) => self.rent_exempt_min = v,
            AppEvent::SendPrepared {
                from_id,
                to,
                lamports,
                blockhash,
                lvbh,
                fee,
                dest_balance,
                priority_micro,
            } => {
                self.pending_send = Some(PendingSend {
                    from_id,
                    to,
                    lamports,
                    blockhash,
                    lvbh,
                    fee,
                    dest_balance,
                    priority_micro,
                    prepared_at: Instant::now(),
                });
                self.on_send_prepared();
            }
            AppEvent::TransferResult {
                intent_id,
                outcome,
                generation,
            } => {
                if generation != self.generation.load(Ordering::SeqCst) {
                    return;
                }
                match &outcome {
                    TransferOutcome::Submitted { signature } => {
                        self.toast_info(format!(
                            "Submitted {}",
                            crate::ui::format::elide_addr(signature)
                        ));
                    }
                    TransferOutcome::Confirmed { .. } => {
                        self.toast_ok("Transfer confirmed ✓");
                        self.celebrate_center();
                        self.apply_optimistic_transfer(intent_id);
                        self.request_balance_refresh();
                    }
                    TransferOutcome::Failed { reason } => {
                        self.toast_err(format!("Transfer failed: {reason}"));
                    }
                    TransferOutcome::Expired => {
                        self.toast_err("Transfer expired (blockhash) — safe to retry");
                    }
                    TransferOutcome::StillPending { signature } => {
                        self.toast_info(format!(
                            "Still pending {} — will reconcile on next unlock",
                            crate::ui::format::elide_addr(signature)
                        ));
                    }
                }
                self.refresh_detail_intents();
            }
            AppEvent::NetStatus(s) => self.net_status = s,
            AppEvent::Error(e) => {
                self.toast_err(e);
            }
        }
    }

    fn on_send_prepared(&mut self) {
        match self.evaluate_pending_send() {
            Ok(()) => {
                self.send_confirm_armed = false;
                self.modal = Some(Modal::ConfirmSend);
            }
            Err(msg) => {
                self.pending_send = None;
                self.toast_err(msg);
            }
        }
    }

    pub fn pending_send_is_large(&self) -> bool {
        let Some(ps) = self.pending_send.as_ref() else {
            return false;
        };
        let Some(bal) = self
            .wallets
            .iter()
            .find(|w| w.id == ps.from_id)
            .and_then(|w| w.balance_lamports)
        else {
            return false;
        };
        let debit = ps.lamports.saturating_add(ps.fee);
        bal > 0 && debit.saturating_mul(LARGE_SEND_DEN) >= bal.saturating_mul(LARGE_SEND_NUM)
    }

    pub fn evaluate_pending_send(&self) -> Result<(), String> {
        use crate::types::SendGuardError as G;
        let ps = self.pending_send.as_ref().ok_or("no pending send")?;
        if !self.reconcile_done {
            return Err(G::Reconciling.to_string());
        }
        let from = self
            .wallets
            .iter()
            .find(|w| w.id == ps.from_id)
            .ok_or("source wallet missing")?;
        if let Err(e) = crate::input::classify_route(&self.wallets, from, &ps.to) {
            return Err(e.to_string());
        }
        if from.has_open_intent {
            return Err(G::WalletHasOpenIntent.to_string());
        }
        let bal = from.balance_lamports.unwrap_or(0);
        if ps.lamports == 0 {
            return Err(G::ZeroAmount.to_string());
        }
        let need = ps.lamports.saturating_add(ps.fee);
        if bal < need {
            return Err(G::InsufficientFunds { need, have: bal }.to_string());
        }
        let after = bal.saturating_sub(need);
        if after > 0 && after < self.rent_exempt_min {
            return Err(G::SenderRentFloor.to_string());
        }
        if ps.dest_balance.saturating_add(ps.lamports) < self.rent_exempt_min {
            return Err(G::RecipientRentFloor {
                min_first_deposit: self.rent_exempt_min,
            }
            .to_string());
        }
        Ok(())
    }

    fn apply_optimistic_transfer(&mut self, intent_id: i64) {
        let intent = self
            .db
            .lock()
            .ok()
            .and_then(|d| d.get_intent(intent_id).ok().flatten());
        let Some(i) = intent else {
            return;
        };
        let spent = i
            .lamports
            .saturating_add(i.fee_lamports.unwrap_or_else(|| self.send_fee()));
        if let Some(w) = self.wallets.iter_mut().find(|w| w.id == i.from_wallet)
            && let Some(bal) = w.balance_lamports
        {
            w.balance_lamports = Some(bal.saturating_sub(spent));
        }
        if let Some(w) = self.wallets.iter_mut().find(|w| w.pubkey == i.to_address)
            && let Some(bal) = w.balance_lamports
        {
            w.balance_lamports = Some(bal.saturating_add(i.lamports));
        }
    }

    pub fn refresh_detail_intents(&mut self) {
        if let Some(id) = self.focused_wallet
            && let Ok(d) = self.db.lock()
            && let Ok(v) = d.list_intents_for_wallet(id, 50)
        {
            self.detail_intents = v;
        }
        self.history_state.select(None);
        self.reload_wallets();
    }

    pub fn refresh_audit(&mut self) {
        if let Ok(d) = self.db.lock()
            && let Ok(v) = d.list_audit(200)
        {
            self.audit = v;
        }
        self.audit_state.select(None);
    }
}
