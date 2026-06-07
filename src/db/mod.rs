mod intents;
mod wallets;

pub use intents::{CreateIntentError, IntentTransitionError, IntentTransitionOutcome};

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::sync::MutexExt;
use crate::types::{AuditEntry, AuditEvent, Network};

const SCHEMA_VERSION: &str = "1";

const SCHEMA_SQL: &str = r#"
CREATE TABLE meta (
    key   TEXT NOT NULL PRIMARY KEY,
    value TEXT
) STRICT;

CREATE TABLE wallets (
    id            INTEGER PRIMARY KEY,
    account_index INTEGER NOT NULL UNIQUE,
    role          TEXT    NOT NULL CHECK (role IN ('master','sub')),
    pubkey        TEXT    NOT NULL UNIQUE,
    label         TEXT,
    note          TEXT,
    archived      INTEGER NOT NULL DEFAULT 0 CHECK (archived IN (0,1)),
    created_at    INTEGER NOT NULL,
    CHECK ( (role = 'master') = (account_index = 0) )
) STRICT;
CREATE UNIQUE INDEX ux_wallets_single_master ON wallets(role) WHERE role = 'master';

CREATE TABLE tx_intents (
    id                      INTEGER PRIMARY KEY,
    from_wallet             INTEGER NOT NULL REFERENCES wallets(id),
    to_address              TEXT    NOT NULL,
    lamports                INTEGER NOT NULL CHECK (lamports > 0),
    fee_lamports            INTEGER CHECK (fee_lamports IS NULL OR fee_lamports >= 0),
    status                  TEXT    NOT NULL CHECK (status IN
                                ('created','signed','submitted','confirmed','failed','expired')),
    signature               TEXT,
    recent_blockhash        TEXT,
    last_valid_block_height INTEGER CHECK (last_valid_block_height IS NULL OR last_valid_block_height >= 0),
    signed_tx               BLOB,
    note                    TEXT,
    error                   TEXT,
    created_at              INTEGER NOT NULL,
    updated_at              INTEGER NOT NULL
) STRICT;
CREATE UNIQUE INDEX ux_tx_intents_signature ON tx_intents(signature) WHERE signature IS NOT NULL;
CREATE INDEX ix_tx_intents_open ON tx_intents(status)
    WHERE status IN ('created','signed','submitted');
CREATE INDEX ix_tx_intents_from_open ON tx_intents(from_wallet)
    WHERE status IN ('created','signed','submitted');
CREATE INDEX ix_tx_intents_from ON tx_intents(from_wallet, created_at DESC);

CREATE TABLE audit_log (
    id         INTEGER PRIMARY KEY,
    ts         INTEGER NOT NULL,
    event_type TEXT    NOT NULL,
    details    TEXT    NOT NULL DEFAULT '{}',
    prev_hash  TEXT,
    row_hash   TEXT NOT NULL
) STRICT;
CREATE INDEX ix_audit_ts ON audit_log(ts);
"#;

const REQUIRED_SCHEMA_OBJECTS: &[(&str, &str)] = &[
    ("table", "meta"),
    ("table", "wallets"),
    ("table", "tx_intents"),
    ("table", "audit_log"),
    ("index", "ux_wallets_single_master"),
    ("index", "ux_tx_intents_signature"),
    ("index", "ix_tx_intents_open"),
    ("index", "ix_tx_intents_from_open"),
    ("index", "ix_tx_intents_from"),
    ("index", "ix_audit_ts"),
];

pub struct Db {
    conn: Connection,
    audit_key: Option<[u8; 32]>,
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        use rusqlite::OpenFlags;
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening database at {}", path.display()))?;

        conn.busy_timeout(Duration::from_millis(5000))?;

        let mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        if mode.to_lowercase() != "wal" {
            bail!(
                "silo requires a filesystem that supports SQLite WAL for crash-safe \
                 money operations; got journal_mode='{mode}' at {}",
                path.display()
            );
        }
        conn.execute_batch(
            "PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; PRAGMA wal_autocheckpoint=1000;",
        )?;

        let sync: i64 = conn.query_row("PRAGMA synchronous", [], |r| r.get(0))?;
        if sync != 2 {
            bail!(
                "PRAGMA synchronous=FULL did not take (got {sync}) at {}",
                path.display()
            );
        }
        let fk: i64 = conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?;
        if fk != 1 {
            bail!("PRAGMA foreign_keys=ON did not take at {}", path.display());
        }

        let mut db = Db {
            conn,
            audit_key: None,
        };
        db.migrate()?;
        db.ensure_audit_salt()?;

        let ic: String = db
            .conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        if ic != "ok" {
            bail!("database integrity check failed: {ic}");
        }
        Ok(db)
    }

    #[cfg(test)]
    pub fn open_memory() -> Result<Db> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        let mut db = Db {
            conn,
            audit_key: None,
        };
        db.migrate()?;
        db.ensure_audit_salt()?;
        db.audit_key = Some([0x42u8; 32]);
        Ok(db)
    }

    fn migrate(&mut self) -> Result<()> {
        let has_meta: bool = self
            .conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='meta'",
                [],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);

        if !has_meta {
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute_batch(SCHEMA_SQL)?;
            tx.execute(
                "INSERT INTO meta (key,value) VALUES ('schema_version',?1)",
                params![SCHEMA_VERSION],
            )?;
            tx.execute(
                "INSERT INTO meta (key,value) VALUES ('network','mainnet-beta')",
                [],
            )?;
            tx.execute(
                "INSERT INTO meta (key,value) VALUES ('rpc_url',?1)",
                params![Network::MainnetBeta.default_rpc_url()],
            )?;
            tx.execute(
                "INSERT INTO meta (key,value) VALUES ('commitment','confirmed')",
                [],
            )?;
            tx.commit()?;
        }
        self.validate_schema()
    }

    fn validate_schema(&self) -> Result<()> {
        for (kind, name) in REQUIRED_SCHEMA_OBJECTS {
            let exists = self
                .conn
                .query_row(
                    "SELECT 1 FROM sqlite_master WHERE type=?1 AND name=?2",
                    params![kind, name],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if !exists {
                bail!("database schema is missing required {kind} {name}");
            }
        }
        let version = self.get_meta("schema_version")?;
        match version.as_deref() {
            Some(SCHEMA_VERSION) => Ok(()),
            Some(v) => bail!("unsupported database schema_version {v}"),
            None => bail!("database schema_version is missing"),
        }
    }

    fn ensure_audit_salt(&mut self) -> Result<()> {
        if self.get_meta("audit_key_salt")?.is_none() {
            let mut salt = [0u8; 32];
            crate::crypto::random_bytes(&mut salt);
            self.set_meta("audit_key_salt", &to_hex(&salt))?;
        }
        Ok(())
    }

    pub fn unlock_audit_key(&mut self, vault_key: &[u8; 32]) -> Result<()> {
        let salt_hex = self
            .get_meta("audit_key_salt")?
            .context("missing audit_key_salt")?;
        let salt = from_hex32(&salt_hex).context("corrupt audit_key_salt")?;
        let mut k = [0u8; 32];
        crate::crypto::hkdf_sha256(vault_key, &salt, b"silo-audit-key-v1", &mut k)?;
        self.audit_key = Some(k);
        Ok(())
    }

    fn require_audit_key(&self) -> Result<[u8; 32]> {
        self.audit_key
            .context("audit key unavailable (vault locked)")
    }

    pub fn audit_unlocked(&self) -> bool {
        self.audit_key.is_some()
    }

    pub fn lock_audit_key(&mut self) {
        use zeroize::Zeroize;
        if let Some(mut k) = self.audit_key.take() {
            k.zeroize();
        }
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key=?1", params![key], |r| {
                r.get::<_, Option<String>>(0)
            })
            .optional()?
            .flatten())
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key,value) VALUES (?1,?2)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn set_meta_audited(
        &mut self,
        key: &str,
        value: &str,
        event: AuditEvent,
        details: &serde_json::Value,
    ) -> Result<()> {
        let audit_key = self.require_audit_key()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO meta (key,value) VALUES (?1,?2)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )?;
        append_audit(&tx, &audit_key, event, details)?;
        tx.commit()?;
        Ok(())
    }

    pub fn audit(&mut self, event: AuditEvent, details: &serde_json::Value) -> Result<()> {
        let Some(key) = self.audit_key else {
            return Ok(());
        };
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        append_audit(&tx, &key, event, details)?;
        tx.commit()?;
        Ok(())
    }

    pub fn list_audit(&self, limit: usize) -> Result<Vec<AuditEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, ts, event_type, details, prev_hash, row_hash
             FROM audit_log ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            let details_text: String = r.get(3)?;
            Ok(AuditEntry {
                id: r.get(0)?,
                ts: r.get(1)?,
                event_type: r.get(2)?,
                details: serde_json::from_str(&details_text).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        3,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?,
                prev_hash: r.get(4)?,
                row_hash: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn verify_audit_chain(&self) -> Result<bool> {
        let key = self.require_audit_key()?;
        let mut stmt = self
            .conn
            .prepare("SELECT id, ts, event_type, details, prev_hash, row_hash FROM audit_log ORDER BY id ASC")?;
        let mut rows = stmt.query([])?;
        let mut expected_prev: Option<String> = None;
        let mut last_hash: Option<String> = None;
        while let Some(r) = rows.next()? {
            let id: i64 = r.get(0)?;
            let ts: i64 = r.get(1)?;
            let event_type: String = r.get(2)?;
            let details: String = r.get(3)?;
            let prev_hash: Option<String> = r.get(4)?;
            let row_hash: String = r.get(5)?;
            if prev_hash != expected_prev {
                return Ok(false);
            }
            let canonical = canonical_bytes(id, ts, &event_type, &details);
            let computed = hmac_hex(&key, &canonical, prev_hash.as_deref());
            if computed != row_hash {
                return Ok(false);
            }
            expected_prev = Some(row_hash.clone());
            last_hash = Some(row_hash);
        }
        let head = self.get_meta("audit_head_hash")?;
        if head.is_some() != last_hash.is_some() {
            return Ok(false);
        }
        if last_hash.is_none() && self.master_exists()? {
            return Ok(false);
        }
        Ok(head == last_hash)
    }
}

#[derive(Clone)]
pub struct Storage {
    inner: Arc<Mutex<Db>>,
}

impl Storage {
    pub fn new(db: Db) -> Self {
        Storage {
            inner: Arc::new(Mutex::new(db)),
        }
    }

    pub fn with<R>(&self, f: impl FnOnce(&Db) -> R) -> R {
        let guard = self.inner.lock_recover();
        f(&guard)
    }

    pub fn with_mut<R>(&self, f: impl FnOnce(&mut Db) -> R) -> R {
        let mut guard = self.inner.lock_recover();
        f(&mut guard)
    }

    pub fn replace(&self, db: Db) {
        *self.inner.lock_recover() = db;
    }

    pub fn with_current<R>(
        &self,
        generation: &AtomicU64,
        cmd_gen: u64,
        f: impl FnOnce(&mut Db) -> R,
    ) -> Option<R> {
        let mut guard = self.inner.lock_recover();
        if generation.load(Ordering::SeqCst) != cmd_gen {
            return None;
        }
        Some(f(&mut guard))
    }
}

pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub(in crate::db) fn append_audit(
    tx: &rusqlite::Transaction,
    audit_key: &[u8; 32],
    event: AuditEvent,
    details: &serde_json::Value,
) -> rusqlite::Result<()> {
    let ts = now_ms();
    let details_text = details.to_string();
    let prev: Option<String> = tx
        .query_row(
            "SELECT value FROM meta WHERE key='audit_head_hash'",
            [],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();

    tx.execute(
        "INSERT INTO audit_log (ts, event_type, details, prev_hash, row_hash)
         VALUES (?1,?2,?3,?4,'')",
        params![ts, event.as_str(), details_text, prev],
    )?;
    let id = tx.last_insert_rowid();
    let canonical = canonical_bytes(id, ts, event.as_str(), &details_text);
    let row_hash = hmac_hex(audit_key, &canonical, prev.as_deref());
    tx.execute(
        "UPDATE audit_log SET row_hash=?1 WHERE id=?2",
        params![row_hash, id],
    )?;
    tx.execute(
        "INSERT INTO meta (key, value) VALUES ('audit_head_hash', ?1)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![row_hash],
    )?;
    Ok(())
}

fn canonical_bytes(id: i64, ts: i64, event_type: &str, details: &str) -> Vec<u8> {
    let mut b = Vec::with_capacity(24 + event_type.len() + details.len());
    b.extend_from_slice(&id.to_be_bytes());
    b.extend_from_slice(&ts.to_be_bytes());
    b.extend_from_slice(&(event_type.len() as u32).to_be_bytes());
    b.extend_from_slice(event_type.as_bytes());
    b.extend_from_slice(&(details.len() as u32).to_be_bytes());
    b.extend_from_slice(details.as_bytes());
    b
}

fn hmac_hex(key: &[u8; 32], canonical: &[u8], prev: Option<&str>) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(canonical);
    if let Some(p) = prev {
        mac.update(p.as_bytes());
    }
    to_hex(&mac.finalize().into_bytes())
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

fn from_hex32(s: &str) -> Result<[u8; 32]> {
    if s.len() != 64 {
        bail!("expected 64 hex chars, got {}", s.len());
    }
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hi = (chunk[0] as char)
            .to_digit(16)
            .context("invalid hex digit")?;
        let lo = (chunk[1] as char)
            .to_digit(16)
            .context("invalid hex digit")?;
        out[i] = ((hi << 4) | lo) as u8;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Role;

    fn db() -> Db {
        Db::open_memory().unwrap()
    }

    #[test]
    fn schema_version_is_required() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("silo.db");
        drop(Db::open(&path).unwrap());
        let conn = Connection::open(&path).unwrap();
        conn.execute("DELETE FROM meta WHERE key='schema_version'", [])
            .unwrap();
        drop(conn);
        assert!(Db::open(&path).is_err());
    }

    #[test]
    fn required_schema_objects_are_validated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("silo.db");
        drop(Db::open(&path).unwrap());
        let conn = Connection::open(&path).unwrap();
        conn.execute("DROP INDEX ix_audit_ts", []).unwrap();
        drop(conn);
        assert!(Db::open(&path).is_err());
    }

    #[test]
    fn migration_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("silo.db");
        let d1 = Db::open(&path).unwrap();
        assert_eq!(d1.get_meta("schema_version").unwrap().as_deref(), Some("1"));
        drop(d1);
        let d2 = Db::open(&path).unwrap();
        assert_eq!(d2.get_meta("schema_version").unwrap().as_deref(), Some("1"));
    }

    #[test]
    fn durability_pragmas_are_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("silo.db");
        let d = Db::open(&path).unwrap();
        let jm: String = d
            .conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(jm.to_lowercase(), "wal");
        let sync: i64 = d
            .conn
            .query_row("PRAGMA synchronous", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sync, 2, "synchronous=FULL is 2");
        let fk: i64 = d
            .conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fk, 1, "foreign_keys ON");
    }

    #[test]
    fn audit_chain_verifies_and_breaks_on_edit() {
        let mut d = db();
        let m = d
            .insert_wallet(
                0,
                Role::Master,
                "MasterPubkey1111111111111111111111111111111",
                None,
            )
            .unwrap();
        d.set_label(m.id, Some("Treasury")).unwrap();
        assert!(d.verify_audit_chain().unwrap(), "fresh chain must verify");

        d.conn
            .execute(
                "UPDATE audit_log SET details='{\"evil\":true}' WHERE id=1",
                [],
            )
            .unwrap();
        assert!(
            !d.verify_audit_chain().unwrap(),
            "edited row must break the chain"
        );
    }

    #[test]
    fn audit_chain_detects_tail_truncation() {
        let mut d = db();
        let m = d
            .insert_wallet(
                0,
                Role::Master,
                "MasterPubkey1111111111111111111111111111111",
                None,
            )
            .unwrap();
        d.set_label(m.id, Some("one")).unwrap();
        d.set_label(m.id, Some("two")).unwrap();
        assert!(d.verify_audit_chain().unwrap());

        let max: i64 = d
            .conn
            .query_row("SELECT MAX(id) FROM audit_log", [], |r| r.get(0))
            .unwrap();
        d.conn
            .execute("DELETE FROM audit_log WHERE id=?1", params![max])
            .unwrap();
        assert!(
            !d.verify_audit_chain().unwrap(),
            "tail truncation must be detected via the head anchor"
        );
    }

    #[test]
    fn audit_chain_fails_on_full_wipe() {
        let mut d = db();
        let m = d
            .insert_wallet(
                0,
                Role::Master,
                "MasterPubkey1111111111111111111111111111111",
                None,
            )
            .unwrap();
        d.set_label(m.id, Some("one")).unwrap();
        assert!(d.verify_audit_chain().unwrap());

        d.conn.execute("DELETE FROM audit_log", []).unwrap();
        d.conn
            .execute("DELETE FROM meta WHERE key='audit_head_hash'", [])
            .unwrap();
        assert!(
            !d.verify_audit_chain().unwrap(),
            "a full wipe of an initialized vault must not verify as intact"
        );
    }

    #[test]
    fn audit_chain_fails_when_rows_gone_but_head_remains() {
        let mut d = db();
        let m = d
            .insert_wallet(
                0,
                Role::Master,
                "MasterPubkey1111111111111111111111111111111",
                None,
            )
            .unwrap();
        d.set_label(m.id, Some("one")).unwrap();
        assert!(d.verify_audit_chain().unwrap());

        d.conn.execute("DELETE FROM audit_log", []).unwrap();
        assert!(
            !d.verify_audit_chain().unwrap(),
            "empty log with a surviving head anchor must fail"
        );
    }

    #[test]
    fn audit_chain_fails_when_head_gone_but_rows_remain() {
        let mut d = db();
        let m = d
            .insert_wallet(
                0,
                Role::Master,
                "MasterPubkey1111111111111111111111111111111",
                None,
            )
            .unwrap();
        d.set_label(m.id, Some("one")).unwrap();
        assert!(d.verify_audit_chain().unwrap());

        d.conn
            .execute("DELETE FROM meta WHERE key='audit_head_hash'", [])
            .unwrap();
        assert!(
            !d.verify_audit_chain().unwrap(),
            "surviving rows with no head anchor must fail"
        );
    }

    #[test]
    fn audit_chain_empty_uninitialized_vault_verifies() {
        let d = db();
        assert!(
            d.verify_audit_chain().unwrap(),
            "a fresh vault with no master and no audit log is intact"
        );
    }

    #[test]
    fn audit_key_hkdf_is_deterministic_and_keyed() {
        let mut d = db();
        let vk = [9u8; 32];
        d.unlock_audit_key(&vk).unwrap();
        let derived = d.audit_key;
        d.insert_wallet(
            0,
            Role::Master,
            "M111111111111111111111111111111111111111111A",
            None,
        )
        .unwrap();
        assert!(d.verify_audit_chain().unwrap());

        d.unlock_audit_key(&vk).unwrap();
        assert_eq!(derived, d.audit_key);
        assert!(d.verify_audit_chain().unwrap());

        d.unlock_audit_key(&[1u8; 32]).unwrap();
        assert_ne!(derived, d.audit_key);
        assert!(!d.verify_audit_chain().unwrap());
    }

    #[test]
    fn locked_db_refuses_audited_writes() {
        let mut d = Db::open_memory().unwrap();
        d.audit_key = None;
        assert!(
            d.insert_wallet(
                0,
                Role::Master,
                "M111111111111111111111111111111111111111111B",
                None,
            )
            .is_err()
        );
    }

    #[test]
    fn audited_meta_write_commits_meta_and_audit_together() {
        let mut d = db();
        d.set_meta_audited(
            "currency",
            "eur",
            AuditEvent::SettingsChanged,
            &serde_json::json!({"currency":"eur"}),
        )
        .unwrap();
        assert_eq!(d.get_meta("currency").unwrap().as_deref(), Some("eur"));
        assert!(d.verify_audit_chain().unwrap());
        assert_eq!(d.list_audit(1).unwrap()[0].event_type, "settings_changed");
    }

    #[test]
    fn audited_meta_write_rolls_back_when_audit_fails() {
        let mut d = db();
        d.conn
            .execute_batch(
                "CREATE TRIGGER abort_audit BEFORE INSERT ON audit_log
                 BEGIN SELECT RAISE(ABORT, 'audit disabled'); END;",
            )
            .unwrap();
        assert!(
            d.set_meta_audited(
                "currency",
                "eur",
                AuditEvent::SettingsChanged,
                &serde_json::json!({"currency":"eur"}),
            )
            .is_err()
        );
        assert!(d.get_meta("currency").unwrap().is_none());
    }

    #[test]
    fn malformed_audit_details_are_read_errors() {
        let d = db();
        d.conn
            .execute(
                "INSERT INTO audit_log (ts,event_type,details,prev_hash,row_hash)
                 VALUES (1,'settings_changed','not-json',NULL,'x')",
                [],
            )
            .unwrap();
        assert!(d.list_audit(1).is_err());
    }

    #[test]
    fn hex_roundtrip() {
        let mut k = [0u8; 32];
        crate::crypto::random_bytes(&mut k);
        assert_eq!(from_hex32(&to_hex(&k)).unwrap(), k);
    }
}
