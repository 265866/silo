use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Network {
    MainnetBeta,
}
impl Network {
    pub fn as_str(&self) -> &'static str {
        "mainnet-beta"
    }
    pub fn default_rpc_url(&self) -> &'static str {
        "https://api.mainnet-beta.solana.com"
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Commitment {
    Confirmed,
}
impl Commitment {
    pub fn as_str(&self) -> &'static str {
        "confirmed"
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Currency {
    Usd,
    Eur,
    Gbp,
    Jpy,
    Cad,
    Aud,
    Chf,
    Cny,
}

impl Currency {
    pub const ALL: [Currency; 8] = [
        Currency::Usd,
        Currency::Eur,
        Currency::Gbp,
        Currency::Jpy,
        Currency::Cad,
        Currency::Aud,
        Currency::Chf,
        Currency::Cny,
    ];

    pub fn code(&self) -> &'static str {
        match self {
            Currency::Usd => "usd",
            Currency::Eur => "eur",
            Currency::Gbp => "gbp",
            Currency::Jpy => "jpy",
            Currency::Cad => "cad",
            Currency::Aud => "aud",
            Currency::Chf => "chf",
            Currency::Cny => "cny",
        }
    }

    pub fn symbol(&self) -> &'static str {
        match self {
            Currency::Usd => "$",
            Currency::Eur => "€",
            Currency::Gbp => "£",
            Currency::Jpy => "¥",
            Currency::Cad => "CA$",
            Currency::Aud => "A$",
            Currency::Chf => "CHF ",
            Currency::Cny => "CN¥",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Currency::Usd => "USD",
            Currency::Eur => "EUR",
            Currency::Gbp => "GBP",
            Currency::Jpy => "JPY",
            Currency::Cad => "CAD",
            Currency::Aud => "AUD",
            Currency::Chf => "CHF",
            Currency::Cny => "CNY",
        }
    }

    pub fn decimals(&self) -> usize {
        match self {
            Currency::Jpy | Currency::Cny => 0,
            _ => 2,
        }
    }

    pub fn from_code(s: &str) -> Option<Self> {
        Currency::ALL.into_iter().find(|c| c.code() == s)
    }

    pub fn next(&self) -> Currency {
        let i = Currency::ALL.iter().position(|c| c == self).unwrap_or(0);
        Currency::ALL[(i + 1) % Currency::ALL.len()]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    Master,
    Sub,
}
impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Master => "master",
            Role::Sub => "sub",
        }
    }
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "master" => Some(Role::Master),
            "sub" => Some(Role::Sub),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct WalletRow {
    pub id: i64,
    pub account_index: u32,
    pub role: Role,
    pub pubkey: String,
    pub label: Option<String>,
    pub note: Option<String>,
    pub archived: bool,
    pub created_at: i64,
    pub balance_lamports: Option<u64>,
    pub has_open_intent: bool,
}

impl WalletRow {
    pub fn display_name(&self) -> String {
        match &self.label {
            Some(l) if !l.is_empty() => l.clone(),
            _ => match self.role {
                Role::Master => "Master".to_string(),
                Role::Sub => format!("Subwallet {}", self.account_index),
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntentStatus {
    Created,
    Signed,
    Submitted,
    Confirmed,
    Failed,
    Expired,
}
impl IntentStatus {
    pub fn as_str(&self) -> &'static str {
        use IntentStatus::*;
        match self {
            Created => "created",
            Signed => "signed",
            Submitted => "submitted",
            Confirmed => "confirmed",
            Failed => "failed",
            Expired => "expired",
        }
    }
    pub fn from_db_str(s: &str) -> Option<Self> {
        use IntentStatus::*;
        Some(match s {
            "created" => Created,
            "signed" => Signed,
            "submitted" => Submitted,
            "confirmed" => Confirmed,
            "failed" => Failed,
            "expired" => Expired,
            _ => return None,
        })
    }
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            IntentStatus::Confirmed | IntentStatus::Failed | IntentStatus::Expired
        )
    }
}

#[derive(Clone, Debug)]
pub struct Intent {
    pub id: i64,
    pub from_wallet: i64,
    pub to_address: String,
    pub lamports: u64,
    pub fee_lamports: Option<u64>,
    pub status: IntentStatus,
    pub signature: Option<String>,
    pub recent_blockhash: Option<String>,
    pub last_valid_block_height: Option<u64>,
    pub signed_tx: Option<Vec<u8>>,
    pub note: Option<String>,
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug)]
pub enum TransferOutcome {
    Submitted { signature: String },
    Confirmed { signature: String },
    Failed { reason: String },
    Expired,
    StillPending { signature: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetStatus {
    Online,
    Syncing,
    Offline,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuditEvent {
    VaultCreated,
    VaultUnlocked,
    VaultUnlockFailed,
    WalletDerived,
    WalletLabeled,
    WalletNoted,
    WalletArchived,
    IntentCreated,
    IntentSigned,
    IntentSubmitted,
    IntentConfirmed,
    IntentFailed,
    IntentExpired,
    IntentNoted,
    ReconcileStarted,
    ReconcileResolved,
    RpcChanged,
    SettingsChanged,
    Locked,
    IntegrityCheckFailed,
}
impl AuditEvent {
    pub fn as_str(&self) -> &'static str {
        use AuditEvent::*;
        match self {
            VaultCreated => "vault_created",
            VaultUnlocked => "vault_unlocked",
            VaultUnlockFailed => "vault_unlock_failed",
            WalletDerived => "wallet_derived",
            WalletLabeled => "wallet_labeled",
            WalletNoted => "wallet_noted",
            WalletArchived => "wallet_archived",
            IntentCreated => "intent_created",
            IntentSigned => "intent_signed",
            IntentSubmitted => "intent_submitted",
            IntentConfirmed => "intent_confirmed",
            IntentFailed => "intent_failed",
            IntentExpired => "intent_expired",
            IntentNoted => "intent_noted",
            ReconcileStarted => "reconcile_started",
            ReconcileResolved => "reconcile_resolved",
            RpcChanged => "rpc_changed",
            SettingsChanged => "settings_changed",
            Locked => "locked",
            IntegrityCheckFailed => "integrity_check_failed",
        }
    }
}

#[derive(Clone, Debug)]
pub struct AuditEntry {
    pub id: i64,
    pub ts: i64,
    pub event_type: String,
    pub details: serde_json::Value,
    pub prev_hash: Option<String>,
    pub row_hash: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteError {
    SubToSubForbidden,
    SelfSend,
    UnknownDestination,
    ProgramAddress,
}
impl std::fmt::Display for RouteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            RouteError::SubToSubForbidden => "subwallet → subwallet is blocked",
            RouteError::SelfSend => "cannot send to the same wallet",
            RouteError::UnknownDestination => "not a valid Solana address",
            RouteError::ProgramAddress => "cannot send to a program address",
        };
        f.write_str(s)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendGuardError {
    InsufficientFunds { need: u64, have: u64 },
    RecipientRentFloor { min_first_deposit: u64 },
    SenderRentFloor,
    ZeroAmount,
    WalletHasOpenIntent,
    Reconciling,
}
impl std::fmt::Display for SendGuardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use crate::money::format_lamports;
        match self {
            SendGuardError::InsufficientFunds { need, have } => write!(
                f,
                "insufficient funds: need {} SOL, have {} SOL",
                format_lamports(*need),
                format_lamports(*have)
            ),
            SendGuardError::RecipientRentFloor { min_first_deposit } => write!(
                f,
                "first deposit to a new address must be ≥ {} SOL (rent-exempt minimum)",
                format_lamports(*min_first_deposit)
            ),
            SendGuardError::SenderRentFloor => {
                f.write_str("this would drop the source below the rent-exempt minimum")
            }
            SendGuardError::ZeroAmount => f.write_str("amount must be greater than zero"),
            SendGuardError::WalletHasOpenIntent => {
                f.write_str("this wallet already has a transfer in progress")
            }
            SendGuardError::Reconciling => {
                f.write_str("still reconciling in-flight transfers — sends are disabled")
            }
        }
    }
}
