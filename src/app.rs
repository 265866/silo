use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::mpsc;
use zeroize::{Zeroize, Zeroizing};

use crate::clipboard::ClipboardManager;
use crate::crypto::Seed;
use crate::db::{Db, Storage};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingChange {
    Currency(crate::types::Currency),
    Priority(u64),
    AutoLock(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalletTextField {
    Label,
    Note,
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
    PersistSignedSend {
        pending: PendingSend,
        from: WalletRow,
        wire: Vec<u8>,
        sig_b58: String,
    },
    Broadcast {
        intent_id: i64,
    },
    ChangeRpc {
        url: String,
    },
    UnlockVault {
        vault_path: std::path::PathBuf,
        passphrase: Zeroizing<String>,
    },
    FinishSetup {
        vault_path: std::path::PathBuf,
        config_dir: std::path::PathBuf,
        current_profile: Option<String>,
        creating: bool,
        phrase: Zeroizing<String>,
        passphrase: Zeroizing<String>,
    },
    DeleteProfile {
        config_dir: std::path::PathBuf,
        id: String,
    },
    ClipboardCopy {
        text: String,
        ok_label: String,
        arm_hot_refresh: bool,
    },
    ClipboardPaste {
        target: PasteTarget,
    },
    ArchiveWallet {
        id: i64,
        want: bool,
    },
    DeriveSubwallet {
        seed: Seed,
    },
    PersistSetting {
        change: SettingChange,
    },
    SetWalletText {
        id: i64,
        field: WalletTextField,
        value: Option<String>,
    },
    SetIntentNote {
        wallet_id: i64,
        id: i64,
        value: Option<String>,
    },
    RenameProfile {
        config_dir: std::path::PathBuf,
        id: String,
        name: String,
    },
    OpenProfile {
        config_dir: std::path::PathBuf,
        id: String,
    },
    CreateProfile {
        config_dir: std::path::PathBuf,
        id: String,
    },
}

impl Command {
    pub(crate) fn ordered(&self) -> bool {
        matches!(
            self,
            Command::Reconcile
                | Command::PrepareSend { .. }
                | Command::PersistSignedSend { .. }
                | Command::Broadcast { .. }
                | Command::ChangeRpc { .. }
                | Command::UnlockVault { .. }
                | Command::FinishSetup { .. }
                | Command::DeleteProfile { .. }
                | Command::ArchiveWallet { .. }
                | Command::DeriveSubwallet { .. }
                | Command::PersistSetting { .. }
                | Command::SetWalletText { .. }
                | Command::SetIntentNote { .. }
                | Command::RenameProfile { .. }
                | Command::OpenProfile { .. }
                | Command::CreateProfile { .. }
        )
    }
}

pub struct ProfileOpenedPayload {
    pub db: crate::db::Db,
    pub id: String,
    pub created: bool,
}

impl std::fmt::Debug for ProfileOpenedPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProfileOpenedPayload")
            .field("id", &self.id)
            .field("created", &self.created)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub enum AppEvent {
    ReconcileComplete {
        resolved: usize,
        generation: u64,
    },
    ReconcileFailedOffline {
        generation: u64,
    },
    Balances {
        list: Vec<(i64, u64)>,
        generation: u64,
    },
    BalancesFailed {
        reason: String,
        generation: u64,
    },
    Price {
        price: SolPrice,
        generation: u64,
    },
    RentExempt {
        lamports: u64,
        generation: u64,
    },
    SendPrepared {
        from_id: i64,
        to: String,
        lamports: u64,
        blockhash: String,
        lvbh: u64,
        fee: u64,
        dest_balance: u64,
        priority_micro: u64,
        generation: u64,
    },
    TransferResult {
        intent_id: i64,
        outcome: TransferOutcome,
        generation: u64,
    },
    SendPersisted {
        result: SendPersistResult,
        generation: u64,
    },
    UnlockComplete {
        result: UnlockResult,
        generation: u64,
    },
    SetupComplete {
        result: SetupResult,
        generation: u64,
    },
    ProfileDeleted {
        result: ProfileDeleteResult,
        generation: u64,
    },
    ProfileOpened {
        result: Result<ProfileOpenedPayload, String>,
        generation: u64,
    },
    ClipboardCopied {
        result: ClipboardCopyResult,
        generation: u64,
    },
    ClipboardPasted {
        target: PasteTarget,
        result: Result<String, String>,
        generation: u64,
    },
    WalletArchived {
        id: i64,
        want: bool,
        result: Result<Vec<WalletRow>, String>,
        generation: u64,
    },
    SubwalletDerived {
        result: Result<(u32, Vec<WalletRow>), String>,
        generation: u64,
    },
    SettingPersisted {
        change: SettingChange,
        result: Result<(), String>,
        generation: u64,
    },
    WalletTextSet {
        field: WalletTextField,
        result: Result<Vec<WalletRow>, String>,
        generation: u64,
    },
    IntentNoteSet {
        result: Result<Vec<Intent>, String>,
        generation: u64,
    },
    ProfileRenamed {
        result: Result<Vec<crate::profiles::ProfileMeta>, String>,
        generation: u64,
    },
    RpcChanged {
        url: String,
        generation: u64,
    },
    NetStatus {
        status: NetStatus,
        generation: u64,
    },
    Error {
        message: String,
        generation: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasteTarget {
    SendTo,
    SendAmount,
}

#[derive(Debug)]
pub enum UnlockResult {
    Unlocked { seed: Seed, wallets: Vec<WalletRow> },
    WrongPassphrase,
    AuditKey,
    WalletMismatch,
    WalletRead(String),
    AuditChainFailed,
    AuditChainRead(String),
}

#[derive(Debug)]
pub enum SetupResult {
    Finished {
        seed: Seed,
        wallets: Vec<WalletRow>,
        profiles: Vec<crate::profiles::ProfileMeta>,
    },
    Failed(String),
}

#[derive(Debug)]
pub enum SendPersistResult {
    Signed { intent_id: i64 },
    Failed(String),
}

#[derive(Debug)]
pub enum ProfileDeleteResult {
    Deleted {
        profiles: Vec<crate::profiles::ProfileMeta>,
    },
    Failed(String),
}

#[derive(Debug)]
pub struct ClipboardCopyResult {
    pub outcome: Result<crate::clipboard::CopyOutcome, String>,
    pub ok_label: String,
    pub arm_hot_refresh: bool,
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
    DeleteProfile { id: String, challenge: String },
}

impl PromptKind {
    pub fn multiline(&self) -> bool {
        matches!(self, PromptKind::Note(_) | PromptKind::TxNote(_))
    }
}

#[derive(Clone)]
pub enum ConfirmAction {
    CreateWithEmptyPassphrase,
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

#[derive(Clone, Debug)]
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
const CONFETTI_GLYPHS: [char; 10] = ['✦', '✧', '★', '*', '•', '◆', '◇', '❄', '✺', '+'];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WalletListRow {
    Wallet(usize),
    ArchivedHeader,
}

pub struct App {
    pub(crate) running: bool,
    pub(crate) route: Route,
    pub(crate) db: Storage,
    pub(crate) price: Arc<PriceCache>,
    pub(crate) cmd_tx: mpsc::Sender<(u64, Command)>,
    pub(crate) generation: Arc<AtomicU64>,
    pub(crate) clip: ClipboardManager,
    pub(crate) theme: Theme,

    pub(crate) seed: Option<Seed>,
    pub(crate) reconcile_done: bool,
    pub(crate) net_status: NetStatus,
    pub(crate) rent_exempt_min: u64,
    pub(crate) priority_micro: u64,

    pub(crate) wallets: Vec<WalletRow>,
    pub(crate) list_state: TableState,
    pub(crate) archived_expanded: bool,
    pub(crate) history_state: TableState,
    pub(crate) audit_state: TableState,
    pub(crate) focused_wallet: Option<i64>,

    pub(crate) setup: SetupState,
    pub(crate) input: InputState,

    pub(crate) modal: Option<Modal>,
    pub(crate) toasts: Vec<Toast>,
    pub(crate) spinner_frame: usize,
    pub(crate) inflight: u32,

    pub(crate) detail_intents: Vec<Intent>,
    pub(crate) audit: Vec<crate::types::AuditEntry>,
    pub(crate) pending_send: Option<PendingSend>,
    pub(crate) preparing_send: bool,
    pub(crate) send_confirm_armed: bool,
    pub(crate) blocking_input: bool,

    pub(crate) last_activity: Instant,
    pub(crate) last_wall: SystemTime,
    #[cfg(target_os = "linux")]
    pub(crate) last_boottime: Option<Duration>,
    pub(crate) auto_lock_after: Duration,
    pub(crate) last_balance_refresh: Instant,
    pub(crate) hot_until: Option<Instant>,
    pub(crate) hot_interval: Duration,
    pub(crate) rpc_url: String,
    pub(crate) vault_path: std::path::PathBuf,
    pub(crate) currency: crate::types::Currency,
    pub(crate) anim_balance: HashMap<i64, f64>,
    pub(crate) config_dir: std::path::PathBuf,
    pub(crate) rpc: Arc<Mutex<crate::solana::rpc::Rpc>>,
    pub(crate) client: reqwest::Client,
    pub(crate) profiles: Vec<crate::profiles::ProfileMeta>,
    pub(crate) profile_sel: usize,
    pub(crate) current_profile: Option<String>,
    pub(crate) pending_profile_open: Option<usize>,

    pub(crate) frame: u64,
    pub(crate) confetti: Vec<Confetto>,
    confetti_rng: u64,
    pub(crate) last_area: ratatui::layout::Rect,
    pub(crate) price_flash: f32,
    pub(crate) price_up: bool,
    last_price: Option<f64>,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: Storage,
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
            preparing_send: false,
            send_confirm_armed: false,
            blocking_input: false,
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
            pending_profile_open: None,
            frame: 0,
            confetti: Vec::new(),
            confetti_rng: 0x2545_F491_4F6C_DD1D,
            last_area: ratatui::layout::Rect::new(0, 0, 80, 24),
            price_flash: 0.0,
            price_up: true,
            last_price: None,
        }
    }

    pub fn restore_startup_state(
        &mut self,
        currency: crate::types::Currency,
        priority_micro: u64,
        auto_lock_mins: Option<u64>,
        profiles: Vec<crate::profiles::ProfileMeta>,
        active_id: String,
        first_run: bool,
    ) {
        self.currency = currency;
        self.priority_micro = priority_micro;
        if let Some(m) = auto_lock_mins {
            self.auto_lock_after = Duration::from_secs(m * 60);
        }
        self.profiles = profiles;
        self.current_profile = Some(active_id);
        self.reload_wallets();
        if first_run {
            self.route = Route::Setup;
            self.setup = SetupState::default();
        } else {
            self.route = Route::ProfileSelect;
        }
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    pub fn stop(&mut self) {
        self.running = false;
    }

    pub fn scrub_for_exit(&mut self) {
        if self.seed.is_some() {
            self.lock();
        }
        self.input.zeroize_secrets();
        self.setup.mnemonic_words.zeroize();
    }

    pub fn send_cmd(&mut self, cmd: Command) -> bool {
        let g = self.generation.load(Ordering::SeqCst);
        match self.cmd_tx.try_send((g, cmd)) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.toast_err("Command queue is full — try again");
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.toast_err("Background worker stopped");
                false
            }
        }
    }

    pub fn send_rpc_change_cmd(&mut self, url: String) -> bool {
        let old = self.generation.load(Ordering::SeqCst);
        let new = old + 1;
        self.generation.store(new, Ordering::SeqCst);
        match self.cmd_tx.try_send((new, Command::ChangeRpc { url })) {
            Ok(()) => {
                self.inflight = 0;
                true
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                let _ =
                    self.generation
                        .compare_exchange(new, old, Ordering::SeqCst, Ordering::SeqCst);
                self.toast_err("Command queue is full — try again");
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                let _ =
                    self.generation
                        .compare_exchange(new, old, Ordering::SeqCst, Ordering::SeqCst);
                self.toast_err("Background worker stopped");
                false
            }
        }
    }

    fn bump_generation(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    fn reset_profile_scoped_state(&mut self) {
        let url = crate::types::Network::MainnetBeta
            .default_rpc_url()
            .to_string();
        *self.rpc.lock_recover() = crate::solana::rpc::Rpc::new(self.client.clone(), url.clone());
        self.rpc_url = url;
        self.currency = crate::types::Currency::Usd;
        self.priority_micro = crate::money::DEFAULT_PRIORITY_FEE_MICRO;
        self.auto_lock_after = Duration::from_secs(DEFAULT_AUTO_LOCK_MINUTES * 60);
        self.price.clear();
        self.reset_price_baseline();
    }

    pub(crate) fn validate_profile_scoped_state(db: &Db) -> anyhow::Result<()> {
        db.get_meta("rpc_url")?;
        db.get_meta("currency")?;
        db.get_meta("priority_fee_micro")?;
        db.get_meta("auto_lock_minutes")?;
        db.get_meta("last_price")?;
        Ok(())
    }

    fn load_profile_scoped_state(&mut self) -> anyhow::Result<()> {
        let url = self.db.with(|d| -> anyhow::Result<_> {
            Ok(d.get_meta("rpc_url")?.unwrap_or_else(|| {
                crate::types::Network::MainnetBeta
                    .default_rpc_url()
                    .to_string()
            }))
        })?;
        *self.rpc.lock_recover() = crate::solana::rpc::Rpc::new(self.client.clone(), url.clone());
        self.rpc_url = url;
        self.currency = self
            .db
            .with(|d| d.get_meta("currency"))?
            .and_then(|s| crate::types::Currency::from_code(&s))
            .unwrap_or(crate::types::Currency::Usd);
        self.priority_micro = self
            .db
            .with(|d| d.get_meta("priority_fee_micro"))?
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(crate::money::DEFAULT_PRIORITY_FEE_MICRO);
        self.auto_lock_after = self
            .db
            .with(|d| d.get_meta("auto_lock_minutes"))?
            .and_then(|s| s.parse::<u64>().ok())
            .map(|m| m.clamp(AUTO_LOCK_MIN_MINUTES, AUTO_LOCK_MAX_MINUTES))
            .map(|m| Duration::from_secs(m * 60))
            .unwrap_or_else(|| Duration::from_secs(DEFAULT_AUTO_LOCK_MINUTES * 60));
        self.price.clear();
        self.reset_price_baseline();
        if let Some(s) = self.db.with(|d| d.get_meta("last_price"))? {
            match crate::price::SolPrice::from_meta_json(&s) {
                Ok(p) if p.currency == self.currency && !p.is_stale() => self.price.seed(p),
                Ok(_) => {}
                Err(e) => self.toast_err(format!("Invalid cached price: {e}")),
            }
        }
        Ok(())
    }

    pub fn switch_to_profile(&mut self, id: &str) -> anyhow::Result<()> {
        crate::profiles::validate_id(id)?;
        let old = self.generation.load(Ordering::SeqCst);
        self.bump_generation();
        if !self.send_cmd(Command::OpenProfile {
            config_dir: self.config_dir.clone(),
            id: id.to_string(),
        }) {
            let _ =
                self.generation
                    .compare_exchange(old + 1, old, Ordering::SeqCst, Ordering::SeqCst);
            anyhow::bail!("could not queue profile open");
        }
        self.blocking_input = true;
        Ok(())
    }

    pub fn begin_new_profile(&mut self) {
        let old = self.generation.load(Ordering::SeqCst);
        self.bump_generation();
        self.input.zeroize_secrets();
        self.setup
            .mnemonic_words
            .iter_mut()
            .for_each(|w| w.zeroize());
        self.setup.scrub_confirm();

        let id = crate::profiles::new_id();
        if !self.send_cmd(Command::CreateProfile {
            config_dir: self.config_dir.clone(),
            id,
        }) {
            let _ =
                self.generation
                    .compare_exchange(old + 1, old, Ordering::SeqCst, Ordering::SeqCst);
            self.toast_err("could not queue new profile creation");
            return;
        }
        self.blocking_input = true;
    }

    fn try_next_profile_fallback(&mut self, last_err: &str) {
        let next = match self.pending_profile_open {
            Some(idx) => idx + 1,
            None => return,
        };
        if next < self.profiles.len() {
            self.pending_profile_open = Some(next);
            let id = self.profiles[next].id.clone();
            if let Err(e) = self.switch_to_profile(&id) {
                self.try_next_profile_fallback(&e.to_string());
            }
        } else {
            self.pending_profile_open = None;
            self.toast_err(format!(
                "Profile deleted, but no remaining profile could be opened: {last_err}"
            ));
            self.begin_new_profile();
        }
    }

    pub fn reload_profiles(&mut self) {
        match crate::profiles::load(&self.config_dir) {
            Ok(profiles) => self.profiles = profiles,
            Err(e) => {
                self.toast_err(format!("Could not load profiles: {e}"));
                return;
            }
        }
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
        if self.send_cmd(Command::RefreshBalances {
            include_archived: self.archived_expanded,
        }) {
            self.inflight += 1;
        }
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
        self.db.with_mut(|d| {
            let _ = d.audit(crate::types::AuditEvent::Locked, &serde_json::json!({}));
            d.lock_audit_key();
        });
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
        const GLYPHS: [char; 10] = CONFETTI_GLYPHS;
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

    pub fn wallet_list_len(&self) -> usize {
        self.wallets.iter().filter(|w| !w.archived).count()
            + usize::from(self.wallets.iter().any(|w| w.archived))
            + if self.archived_expanded {
                self.wallets.iter().filter(|w| w.archived).count()
            } else {
                0
            }
    }

    fn wallet_list_row_at(&self, mut pos: usize) -> Option<WalletListRow> {
        for (i, w) in self.wallets.iter().enumerate() {
            if !w.archived {
                if pos == 0 {
                    return Some(WalletListRow::Wallet(i));
                }
                pos -= 1;
            }
        }
        if self.wallets.iter().any(|w| w.archived) {
            if pos == 0 {
                return Some(WalletListRow::ArchivedHeader);
            }
            pos -= 1;
            if self.archived_expanded {
                for (i, w) in self.wallets.iter().enumerate() {
                    if w.archived {
                        if pos == 0 {
                            return Some(WalletListRow::Wallet(i));
                        }
                        pos -= 1;
                    }
                }
            }
        }
        None
    }

    pub fn selected_wallet(&self) -> Option<&WalletRow> {
        let sel = self.list_state.selected()?;
        match self.wallet_list_row_at(sel)? {
            WalletListRow::Wallet(i) => self.wallets.get(i),
            WalletListRow::ArchivedHeader => None,
        }
    }

    pub fn selected_is_archived_header(&self) -> bool {
        self.list_state
            .selected()
            .and_then(|s| self.wallet_list_row_at(s))
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
        let n = self.wallet_list_len();
        if n == 0 {
            self.list_state.select(None);
        } else {
            let sel = self.list_state.selected().unwrap_or(0).min(n - 1);
            self.list_state.select(Some(sel));
        }
    }

    pub fn select_wallet_by_id(&mut self, id: i64) {
        for pos in 0..self.wallet_list_len() {
            if let Some(WalletListRow::Wallet(i)) = self.wallet_list_row_at(pos)
                && self.wallets[i].id == id
            {
                self.list_state.select(Some(pos));
                return;
            }
        }
    }

    pub fn focused_wallet(&self) -> Option<&WalletRow> {
        let id = self.focused_wallet?;
        self.wallets.iter().find(|w| w.id == id)
    }

    pub fn try_reload_wallets(&mut self) -> anyhow::Result<()> {
        let rows = self.db.with(|d| d.list_wallets())?;
        self.apply_reloaded_wallets(rows);
        Ok(())
    }

    pub fn apply_reloaded_wallets(&mut self, mut rows: Vec<WalletRow>) {
        for r in &mut rows {
            if let Some(old) = self.wallets.iter().find(|w| w.id == r.id) {
                r.balance_lamports = old.balance_lamports;
            }
        }
        self.wallets = rows;
        self.clamp_list_selection();
    }

    pub fn reload_wallets(&mut self) {
        if let Err(e) = self.try_reload_wallets() {
            self.wallets.clear();
            self.anim_balance.clear();
            self.clamp_list_selection();
            self.toast_err(format!("Could not load wallets: {e}"));
        }
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
        self.price.get().filter(|p| !p.is_stale())
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
            AppEvent::ReconcileFailedOffline { generation } => {
                if generation != self.generation.load(Ordering::SeqCst) {
                    return;
                }
                self.net_status = NetStatus::Offline;
                self.toast_err("Offline — reconcile pending, sends disabled");
            }
            AppEvent::Balances { list, generation } => {
                if generation != self.generation.load(Ordering::SeqCst) {
                    return;
                }
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
            AppEvent::BalancesFailed { reason, generation } => {
                if generation != self.generation.load(Ordering::SeqCst) {
                    return;
                }
                if self.inflight > 0 {
                    self.inflight -= 1;
                }
                if self.net_status != NetStatus::Offline {
                    self.toast_err(format!("Balance refresh failed: {reason}"));
                }
                self.net_status = NetStatus::Offline;
            }
            AppEvent::Price {
                price: p,
                generation,
            } => {
                if generation != self.generation.load(Ordering::SeqCst) {
                    return;
                }
                if let Some(prev) = self.last_price
                    && (p.value - prev).abs() > f64::EPSILON
                {
                    self.price_up = p.value > prev;
                    self.price_flash = 1.0;
                }
                self.last_price = Some(p.value);
            }
            AppEvent::RentExempt {
                lamports,
                generation,
            } => {
                if generation != self.generation.load(Ordering::SeqCst) {
                    return;
                }
                self.rent_exempt_min = lamports;
            }
            AppEvent::SendPrepared {
                from_id,
                to,
                lamports,
                blockhash,
                lvbh,
                fee,
                dest_balance,
                priority_micro,
                generation,
            } => {
                self.preparing_send = false;
                if generation != self.generation.load(Ordering::SeqCst) || self.route != Route::Send
                {
                    return;
                }
                let duplicate = matches!(self.modal, Some(Modal::ConfirmSend))
                    && self.pending_send.as_ref().is_some_and(|cur| {
                        cur.from_id == from_id && cur.to == to && cur.lamports == lamports
                    });
                if duplicate {
                    return;
                }
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
                            "Still pending {} — continuing to poll",
                            crate::ui::format::elide_addr(signature)
                        ));
                    }
                }
                self.refresh_detail_intents();
            }
            AppEvent::SendPersisted { result, generation } => {
                if generation != self.generation.load(Ordering::SeqCst) {
                    return;
                }
                self.blocking_input = false;
                match result {
                    SendPersistResult::Signed { intent_id } => {
                        if self.send_cmd(Command::Broadcast { intent_id }) {
                            self.route = Route::WalletDetail;
                            self.refresh_detail_intents();
                            self.toast_info("Signing & broadcasting…");
                        }
                    }
                    SendPersistResult::Failed(reason) => self.toast_err(reason),
                }
            }
            AppEvent::UnlockComplete { result, generation } => {
                if generation != self.generation.load(Ordering::SeqCst) {
                    return;
                }
                self.blocking_input = false;
                match result {
                    UnlockResult::Unlocked { seed, wallets } => {
                        self.seed = Some(seed);
                        self.wallets = wallets;
                        self.clamp_list_selection();
                        self.route = Route::WalletList;
                        self.note_activity();
                        self.send_cmd(Command::Reconcile);
                        self.request_balance_refresh();
                        self.toast_ok("Unlocked");
                    }
                    UnlockResult::WrongPassphrase => self.toast_err("Wrong passphrase"),
                    UnlockResult::AuditKey => {
                        self.modal = Some(Modal::Error {
                            title: "Cannot open audit log".into(),
                            body: "The audit key could not be derived (database/vault inconsistent). Refusing to operate.".into(),
                        });
                    }
                    UnlockResult::WalletMismatch => {
                        self.modal = Some(Modal::Error {
                            title: "Database / vault mismatch".into(),
                            body: "A stored wallet address does not match this recovery phrase. Refusing to operate to avoid misrouting funds.".into(),
                        });
                    }
                    UnlockResult::WalletRead(e) => {
                        self.modal = Some(Modal::Error {
                            title: "Cannot verify wallet database".into(),
                            body: format!(
                                "Wallet metadata could not be read: {e}. Refusing to operate."
                            ),
                        });
                    }
                    UnlockResult::AuditChainFailed => {
                        self.modal = Some(Modal::Error {
                            title: "Audit log integrity check failed".into(),
                            body: "The audit log does not match its stored integrity chain. Refusing to operate.".into(),
                        });
                    }
                    UnlockResult::AuditChainRead(e) => {
                        self.modal = Some(Modal::Error {
                            title: "Cannot verify audit log".into(),
                            body: format!(
                                "Audit chain verification failed: {e}. Refusing to operate."
                            ),
                        });
                    }
                }
            }
            AppEvent::SetupComplete { result, generation } => {
                if generation != self.generation.load(Ordering::SeqCst) {
                    return;
                }
                self.blocking_input = false;
                match result {
                    SetupResult::Finished {
                        seed,
                        wallets,
                        profiles,
                    } => {
                        self.seed = Some(seed);
                        self.profiles = profiles;
                        self.wallets = wallets;
                        self.clamp_list_selection();
                        self.setup
                            .mnemonic_words
                            .iter_mut()
                            .for_each(|w| w.zeroize());
                        self.setup.mnemonic_words.clear();
                        self.setup.scrub_confirm();
                        self.input.zeroize_secrets();
                        self.route = Route::WalletList;
                        self.note_activity();
                        self.send_cmd(Command::FetchRentExempt);
                        self.send_cmd(Command::FetchPrice);
                        self.send_cmd(Command::Reconcile);
                        self.request_balance_refresh();
                        self.toast_ok("Vault created");
                        self.celebrate_center();
                    }
                    SetupResult::Failed(reason) => self.toast_err(reason),
                }
            }
            AppEvent::ProfileDeleted { result, generation } => {
                if generation != self.generation.load(Ordering::SeqCst) {
                    return;
                }
                self.blocking_input = false;
                match result {
                    ProfileDeleteResult::Deleted { profiles } => {
                        self.profiles = profiles;
                        if self.profiles.is_empty() {
                            self.pending_profile_open = None;
                            self.begin_new_profile();
                            self.toast_info("Profile deleted — set up a new wallet");
                        } else {
                            self.pending_profile_open = Some(0);
                            let id = self.profiles[0].id.clone();
                            if let Err(e) = self.switch_to_profile(&id) {
                                self.try_next_profile_fallback(&e.to_string());
                            }
                        }
                    }
                    ProfileDeleteResult::Failed(reason) => self.toast_err(reason),
                }
            }
            AppEvent::ProfileOpened { result, generation } => {
                if generation != self.generation.load(Ordering::SeqCst) {
                    return;
                }
                self.blocking_input = false;
                let payload = match result {
                    Ok(p) => p,
                    Err(e) => {
                        if self.pending_profile_open.is_some() {
                            self.try_next_profile_fallback(&e);
                        } else {
                            self.toast_err(format!("Could not open profile: {e}"));
                        }
                        return;
                    }
                };
                let vault_path = match crate::profiles::vault_path(&self.config_dir, &payload.id) {
                    Ok(p) => p,
                    Err(e) => {
                        self.toast_err(format!("could not resolve profile path: {e}"));
                        return;
                    }
                };
                self.db.replace(payload.db);
                self.vault_path = vault_path;
                self.current_profile = Some(payload.id);
                self.seed = None;
                self.wallets.clear();
                self.anim_balance.clear();
                self.focused_wallet = None;
                self.inflight = 0;
                if payload.created {
                    self.pending_profile_open = None;
                    self.reset_profile_scoped_state();
                    self.setup = SetupState::default();
                    self.route = Route::Setup;
                } else {
                    if let Err(e) = self.load_profile_scoped_state() {
                        self.toast_err(format!("could not load profile state: {e}"));
                    }
                    self.detail_intents.clear();
                    self.reconcile_done = false;
                    self.net_status = NetStatus::Syncing;
                    self.hot_until = None;
                    self.reload_wallets();
                    self.send_cmd(Command::FetchRentExempt);
                    self.send_cmd(Command::FetchPrice);
                    self.request_balance_refresh();
                    if let Some(idx) = self.pending_profile_open.take() {
                        self.profile_sel = idx;
                        self.route = Route::ProfileSelect;
                        self.toast_ok("Profile deleted");
                    } else {
                        self.route = if crate::vault::vault_exists(&self.vault_path) {
                            Route::Unlock
                        } else {
                            Route::Setup
                        };
                    }
                }
            }
            AppEvent::ClipboardCopied { result, generation } => {
                if generation != self.generation.load(Ordering::SeqCst) {
                    return;
                }
                match result.outcome {
                    Ok(crate::clipboard::CopyOutcome::Persistent) => {
                        self.toast_ok(result.ok_label);
                        if result.arm_hot_refresh {
                            self.arm_hot_refresh();
                        }
                    }
                    Ok(crate::clipboard::CopyOutcome::NonPersistent) => {
                        self.toast_info("Copied (won't persist after exit on this compositor)");
                        if result.arm_hot_refresh {
                            self.arm_hot_refresh();
                        }
                    }
                    Ok(crate::clipboard::CopyOutcome::PersistenceUnknown) => {
                        self.toast_info("Copied (persistence not confirmed)");
                        if result.arm_hot_refresh {
                            self.arm_hot_refresh();
                        }
                    }
                    Err(e) => self.toast_err(format!("Copy failed: {e}")),
                }
            }
            AppEvent::ClipboardPasted {
                target,
                result,
                generation,
            } => {
                if generation != self.generation.load(Ordering::SeqCst) || self.route != Route::Send
                {
                    return;
                }
                match result {
                    Ok(text) => match target {
                        PasteTarget::SendTo => self.input.send_to = text.trim().to_string(),
                        PasteTarget::SendAmount => self.input.send_amount = text.trim().to_string(),
                    },
                    Err(e) => self.toast_err(format!("Paste failed: {e}")),
                }
            }
            AppEvent::WalletArchived {
                id,
                want,
                result,
                generation,
            } => {
                if generation != self.generation.load(Ordering::SeqCst) {
                    return;
                }
                match result {
                    Ok(wallets) => {
                        self.apply_reloaded_wallets(wallets);
                        if want {
                            let sel = self.list_state.selected().unwrap_or(0);
                            self.list_state.select(Some(sel.saturating_sub(1)));
                            self.clamp_list_selection();
                        } else {
                            self.select_wallet_by_id(id);
                        }
                        self.toast_ok(if want { "Archived" } else { "Unarchived" });
                    }
                    Err(e) => self.toast_err(e),
                }
            }
            AppEvent::SubwalletDerived { result, generation } => {
                if generation != self.generation.load(Ordering::SeqCst) {
                    return;
                }
                match result {
                    Ok((idx, wallets)) => {
                        self.apply_reloaded_wallets(wallets);
                        self.request_balance_refresh();
                        self.toast_ok(format!("Derived subwallet #{idx}"));
                    }
                    Err(_) => self.toast_err("Could not derive subwallet"),
                }
            }
            AppEvent::SettingPersisted {
                change,
                result,
                generation,
            } => {
                if generation != self.generation.load(Ordering::SeqCst) {
                    return;
                }
                match (change, result) {
                    (SettingChange::Currency(c), Ok(())) => {
                        self.currency = c;
                        self.price.clear();
                        self.reset_price_baseline();
                        self.send_cmd(Command::FetchPrice);
                        self.toast_info(format!("Currency: {}", self.currency.label()));
                    }
                    (SettingChange::Priority(p), Ok(())) => {
                        self.priority_micro = p;
                        self.toast_info(format!(
                            "Priority fee: {} (≈ {} SOL)",
                            crate::money::priority_label(self.priority_micro),
                            crate::money::format_lamports(crate::money::priority_fee_lamports(
                                self.priority_micro
                            ))
                        ));
                    }
                    (SettingChange::AutoLock(m), Ok(())) => {
                        self.auto_lock_after = Duration::from_secs(m * 60);
                        self.toast_info(format!("Auto-lock after {m} min"));
                    }
                    (SettingChange::Currency(_), Err(e)) => {
                        self.toast_err(format!("Could not save currency: {e}"))
                    }
                    (SettingChange::Priority(_), Err(e)) => {
                        self.toast_err(format!("Could not save priority fee: {e}"))
                    }
                    (SettingChange::AutoLock(_), Err(e)) => {
                        self.toast_err(format!("Could not save auto-lock: {e}"))
                    }
                }
            }
            AppEvent::WalletTextSet {
                field,
                result,
                generation,
            } => {
                if generation != self.generation.load(Ordering::SeqCst) {
                    return;
                }
                match result {
                    Ok(wallets) => {
                        self.apply_reloaded_wallets(wallets);
                        self.toast_ok(match field {
                            WalletTextField::Label => "Label updated",
                            WalletTextField::Note => "Note updated",
                        });
                    }
                    Err(e) => self.toast_err(match field {
                        WalletTextField::Label => format!("Could not save label: {e}"),
                        WalletTextField::Note => format!("Could not save note: {e}"),
                    }),
                }
            }
            AppEvent::IntentNoteSet { result, generation } => {
                if generation != self.generation.load(Ordering::SeqCst) {
                    return;
                }
                match result {
                    Ok(intents) => {
                        self.detail_intents = intents;
                        self.history_state.select(None);
                        self.toast_ok("Transfer note updated");
                    }
                    Err(e) => self.toast_err(format!("Could not save transfer note: {e}")),
                }
            }
            AppEvent::ProfileRenamed { result, generation } => {
                if generation != self.generation.load(Ordering::SeqCst) {
                    return;
                }
                match result {
                    Ok(profiles) => {
                        self.profiles = profiles;
                        if self.profile_sel >= self.profiles.len() {
                            self.profile_sel = self.profiles.len().saturating_sub(1);
                        }
                        self.toast_ok("Renamed");
                    }
                    Err(e) => self.toast_err(e),
                }
            }
            AppEvent::RpcChanged { url, generation } => {
                if generation != self.generation.load(Ordering::SeqCst) {
                    return;
                }
                self.rpc_url = url;
                self.net_status = NetStatus::Syncing;
                self.reconcile_done = false;
                self.toast_info("RPC updated — reconciling");
            }
            AppEvent::NetStatus { status, generation } => {
                if generation != self.generation.load(Ordering::SeqCst) {
                    return;
                }
                self.net_status = status;
            }
            AppEvent::Error {
                message,
                generation,
            } => {
                self.preparing_send = false;
                if generation != self.generation.load(Ordering::SeqCst) {
                    return;
                }
                self.toast_err(message);
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
        let intent = self.db.with(|d| d.get_intent(intent_id).ok().flatten());
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
        match self.focused_wallet {
            Some(id) => match self.db.with(|d| d.list_intents_for_wallet(id, 50)) {
                Ok(v) => self.detail_intents = v,
                Err(e) => {
                    self.detail_intents.clear();
                    self.toast_err(format!("Could not load transfer history: {e}"));
                }
            },
            None => self.detail_intents.clear(),
        }
        self.history_state.select(None);
        self.reload_wallets();
    }

    pub fn refresh_audit(&mut self) {
        match self.db.with(|d| d.list_audit(200)) {
            Ok(v) => self.audit = v,
            Err(e) => {
                self.audit.clear();
                self.toast_err(format!("Could not load audit log: {e}"));
            }
        }
        self.audit_state.select(None);
    }
}

#[cfg(test)]
mod tests {
    use super::CONFETTI_GLYPHS;

    #[test]
    fn confetti_glyphs_are_all_single_width() {
        for g in CONFETTI_GLYPHS {
            let cp = g as u32;
            assert_ne!(
                cp, 0xFF0B,
                "fullwidth plus sign re-added to confetti glyphs"
            );
            let fullwidth = (0x1100..=0x115F).contains(&cp)
                || (0x2E80..=0xA4CF).contains(&cp)
                || (0xAC00..=0xD7A3).contains(&cp)
                || (0xF900..=0xFAFF).contains(&cp)
                || (0xFE30..=0xFE4F).contains(&cp)
                || (0xFF00..=0xFF60).contains(&cp)
                || (0xFFE0..=0xFFE6).contains(&cp);
            assert!(
                !fullwidth,
                "glyph U+{cp:04X} is in an East-Asian wide range and would overflow one cell"
            );
        }
    }
}
