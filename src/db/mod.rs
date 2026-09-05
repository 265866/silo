mod intents;
mod wallets;

pub use intents::{CreateIntentError, IntentTransitionOutcome};

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use zeroize::Zeroizing;

use crate::types::{AuditEntry, AuditEvent, Network};

const SCHEMA_VERSION: u32 = 2;

struct Migration {
    to: u32,
    sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[Migration {
    to: 2,
    sql: "DROP INDEX IF EXISTS ix_tx_intents_from_open;
          CREATE UNIQUE INDEX ux_tx_intents_one_open_per_wallet
              ON tx_intents(from_wallet)
              WHERE status IN ('created','signed','submitted');",
}];

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
    ("index", "ux_tx_intents_one_open_per_wallet"),
    ("index", "ix_tx_intents_from"),
    ("index", "ix_audit_ts"),
];

fn verify_durability_pragmas(
    path: &Path,
    journal_mode: &str,
    synchronous: i64,
    foreign_keys: i64,
) -> Result<()> {
    if journal_mode.to_lowercase() != "wal" {
        bail!(
            "silo requires a filesystem that supports SQLite WAL for crash-safe \
             money operations; got journal_mode='{journal_mode}' at {}",
            path.display()
        );
    }
    if synchronous != 2 {
        bail!(
            "PRAGMA synchronous=FULL did not take (got {synchronous}) at {}",
            path.display()
        );
    }
    if foreign_keys != 1 {
        bail!("PRAGMA foreign_keys=ON did not take at {}", path.display());
    }
    Ok(())
}

pub struct Db {
    conn: Connection,
    // Keep the derived key at a stable address and wipe it before deallocation.
    audit_key: Option<Box<Zeroizing<[u8; 32]>>>,
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
        conn.execute_batch(
            "PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; PRAGMA wal_autocheckpoint=1000;",
        )?;
        let sync: i64 = conn.query_row("PRAGMA synchronous", [], |r| r.get(0))?;
        let fk: i64 = conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?;
        verify_durability_pragmas(path, &mode, sync, fk)?;

        let mut db = Db {
            conn,
            audit_key: None,
        };

        let is_existing = db.has_meta_table()?;
        if is_existing {
            db.integrity_check()?;
            db.foreign_key_check()?;
        }
        db.migrate()?;
        db.ensure_audit_salt()?;
        if !is_existing {
            db.integrity_check()?;
            db.foreign_key_check()?;
        }
        Ok(db)
    }

    fn has_meta_table(&self) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='meta'",
                [],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false))
    }

    fn integrity_check(&self) -> Result<()> {
        let ic: String = self
            .conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        if ic != "ok" {
            bail!("database integrity check failed: {ic}");
        }
        Ok(())
    }

    fn foreign_key_check(&self) -> Result<()> {
        let mut stmt = self.conn.prepare("PRAGMA foreign_key_check")?;
        let mut rows = stmt.query([])?;
        if let Some(row) = rows.next()? {
            let table: String = row.get(0)?;
            let rowid: Option<i64> = row.get(1)?;
            let parent: String = row.get(2)?;
            bail!(
                "database foreign-key check failed: {table} row {rowid:?} references a missing row in {parent}"
            );
        }
        Ok(())
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
        db.audit_key = Some(Box::new(Zeroizing::new([0x42u8; 32])));
        Ok(db)
    }

    fn migrate(&mut self) -> Result<()> {
        let has_meta = self.has_meta_table()?;

        if !has_meta {
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute_batch(SCHEMA_SQL)?;
            tx.execute(
                "INSERT INTO meta (key,value) VALUES ('schema_version','1')",
                [],
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
        self.run_migrations()?;
        self.validate_schema()
    }

    fn current_schema_version(&self) -> Result<u32> {
        let raw = self
            .get_meta("schema_version")?
            .context("database schema_version is missing")?;
        match raw.parse::<u32>() {
            Ok(v) => Ok(v),
            Err(_) => bail!("database schema_version {raw} is not a valid integer"),
        }
    }

    fn run_migrations(&mut self) -> Result<()> {
        let mut version = self.current_schema_version()?;
        if version > SCHEMA_VERSION {
            bail!(
                "database schema_version {version} is newer than this build supports \
                 (max {SCHEMA_VERSION}); upgrade silo to open it"
            );
        }
        for migration in MIGRATIONS {
            if migration.to <= version {
                continue;
            }
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute_batch(migration.sql)?;
            tx.execute(
                "UPDATE meta SET value=?1 WHERE key='schema_version'",
                params![migration.to.to_string()],
            )?;
            tx.commit()?;
            version = migration.to;
        }
        if version != SCHEMA_VERSION {
            bail!("database reached schema_version {version}, expected {SCHEMA_VERSION}");
        }
        Ok(())
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
        let version = self.current_schema_version()?;
        if version != SCHEMA_VERSION {
            bail!("unsupported database schema_version {version}");
        }
        Ok(())
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
        let mut k = Box::new(Zeroizing::new([0u8; 32]));
        crate::crypto::hkdf_sha256(vault_key, &salt, b"silo-audit-key-v1", &mut **k)?;
        self.audit_key = Some(k);
        Ok(())
    }

    pub fn lock_audit_key(&mut self) {
        self.audit_key = None;
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
        let audit_key = require_audit_key(&self.audit_key)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO meta (key,value) VALUES (?1,?2)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )?;
        append_audit(&tx, audit_key, event, details)?;
        tx.commit()?;
        Ok(())
    }

    pub fn audit(&mut self, event: AuditEvent, details: &serde_json::Value) -> Result<()> {
        let Some(key) = self.audit_key.as_deref() else {
            return Ok(());
        };
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        append_audit(&tx, key, event, details)?;
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
        let key = require_audit_key(&self.audit_key)?;
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
            let computed = hmac_hex(key, &canonical, prev_hash.as_deref());
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

fn require_audit_key(key: &Option<Box<Zeroizing<[u8; 32]>>>) -> Result<&[u8; 32]> {
    key.as_deref()
        .map(|key| &**key)
        .context("audit key unavailable (vault locked)")
}

type Job = Box<dyn FnOnce(&mut Db) + Send>;

#[derive(Clone)]
pub struct Storage {
    tx: std::sync::mpsc::Sender<Job>,
}

impl Storage {
    pub fn new(db: Db) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("silo-db".to_string())
            .spawn(move || {
                let mut db = db;
                while let Ok(job) = rx.recv() {
                    job(&mut db);
                }
            })
            .expect("spawn silo-db thread");
        Storage { tx }
    }

    pub async fn call<R, F>(&self, f: F) -> R
    where
        F: FnOnce(&mut Db) -> R + Send + 'static,
        R: Send + 'static,
    {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let job: Job = Box::new(move |db| {
            let _ = reply_tx.send(f(db));
        });
        self.tx.send(job).expect("silo-db thread alive");
        reply_rx.await.expect("silo-db thread replied")
    }

    pub fn call_blocking<R, F>(&self, f: F) -> R
    where
        F: FnOnce(&mut Db) -> R + Send + 'static,
        R: Send + 'static,
    {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        let job: Job = Box::new(move |db| {
            let _ = reply_tx.send(f(db));
        });
        self.tx.send(job).expect("silo-db thread alive");
        reply_rx.recv().expect("silo-db thread replied")
    }

    pub async fn call_current<R, F>(
        &self,
        generation: Arc<AtomicU64>,
        cmd_gen: u64,
        f: F,
    ) -> Option<R>
    where
        F: FnOnce(&mut Db) -> R + Send + 'static,
        R: Send + 'static,
    {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let job: Job = Box::new(move |db| {
            let reply = if generation.load(Ordering::SeqCst) == cmd_gen {
                Some(f(db))
            } else {
                None
            };
            let _ = reply_tx.send(reply);
        });
        self.tx.send(job).expect("silo-db thread alive");
        reply_rx.await.expect("silo-db thread replied")
    }

    pub fn replace(&self, db: Db) {
        self.call_blocking(move |slot| {
            *slot = db;
        });
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
    use hmac::{Hmac, KeyInit, Mac};
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
    fn corrupt_existing_db_is_rejected_before_any_write() {
        use std::io::{Seek, SeekFrom, Write};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("silo.db");
        drop(Db::open(&path).unwrap());

        {
            let conn = Connection::open(&path).unwrap();
            let mut stmt = conn
                .prepare(
                    "INSERT INTO wallets (account_index, role, pubkey, label, note, archived, created_at)
                     VALUES (?1, 'sub', ?2, NULL, NULL, 0, 0)",
                )
                .unwrap();
            for i in 1..400i64 {
                stmt.execute(params![i, format!("SubPubkey{i:0>40}")])
                    .unwrap();
            }
            drop(stmt);
            conn.execute("DELETE FROM meta WHERE key='audit_key_salt'", [])
                .unwrap();
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
                .unwrap();
        }

        let len = std::fs::metadata(&path).unwrap().len();
        assert!(
            len > 8192,
            "expected a multi-page database so the corrupted page is not the header"
        );
        {
            let mut f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            f.seek(SeekFrom::Start(len - 1024)).unwrap();
            f.write_all(&[0x55u8; 512]).unwrap();
            f.sync_all().unwrap();
        }

        let before = std::fs::read(&path).unwrap();
        assert!(
            Db::open(&path).is_err(),
            "a corrupt existing database must be rejected"
        );
        let after = std::fs::read(&path).unwrap();
        assert_eq!(
            before, after,
            "Db::open must check integrity before writing: a corrupt file with a missing \
             audit_key_salt must be left untouched, not have the salt re-inserted first"
        );
    }

    #[test]
    fn dangling_foreign_key_is_rejected_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("silo.db");
        drop(Db::open(&path).unwrap());

        let wallet_id = {
            let conn = Connection::open(&path).unwrap();
            conn.execute(
                "INSERT INTO wallets (account_index, role, pubkey, archived, created_at)
                 VALUES (1, 'sub', 'SubPubkey', 0, 0)",
                [],
            )
            .unwrap();
            let id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO tx_intents (from_wallet, to_address, lamports, status, created_at, updated_at)
                 VALUES (?1, 'Recipient', 1000, 'created', 0, 0)",
                params![id],
            )
            .unwrap();
            id
        };

        Db::open(&path).expect("a consistent wallet+intent database must open cleanly");

        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
            conn.execute("DELETE FROM wallets WHERE id=?1", params![wallet_id])
                .unwrap();
        }

        assert!(
            Db::open(&path).is_err(),
            "a tx_intent referencing a deleted wallet must be rejected at open"
        );
    }

    #[test]
    fn migration_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("silo.db");
        let d1 = Db::open(&path).unwrap();
        assert_eq!(d1.get_meta("schema_version").unwrap().as_deref(), Some("2"));
        drop(d1);
        let d2 = Db::open(&path).unwrap();
        assert_eq!(d2.get_meta("schema_version").unwrap().as_deref(), Some("2"));
    }

    fn create_v1_database(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get::<_, String>(0))
            .unwrap();
        conn.execute_batch(SCHEMA_SQL).unwrap();
        conn.execute_batch(
            "INSERT INTO meta (key,value) VALUES ('schema_version','1');
             INSERT INTO meta (key,value) VALUES ('network','mainnet-beta');
             INSERT INTO meta (key,value) VALUES ('rpc_url','https://api.mainnet-beta.solana.com');
             INSERT INTO meta (key,value) VALUES ('commitment','confirmed');",
        )
        .unwrap();
    }

    fn index_exists(conn: &Connection, name: &str) -> bool {
        conn.query_row(
            "SELECT 1 FROM sqlite_master WHERE type='index' AND name=?1",
            params![name],
            |_| Ok(true),
        )
        .optional()
        .unwrap()
        .unwrap_or(false)
    }

    #[test]
    fn open_migrates_v1_database_and_swaps_in_the_one_open_intent_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("silo.db");
        create_v1_database(&path);
        {
            let conn = Connection::open(&path).unwrap();
            assert!(index_exists(&conn, "ix_tx_intents_from_open"));
            assert!(!index_exists(&conn, "ux_tx_intents_one_open_per_wallet"));
        }

        let db = Db::open(&path).unwrap();
        assert_eq!(db.get_meta("schema_version").unwrap().as_deref(), Some("2"));
        drop(db);

        let conn = Connection::open(&path).unwrap();
        assert!(
            index_exists(&conn, "ux_tx_intents_one_open_per_wallet"),
            "migration must create the one-open-intent unique index"
        );
        assert!(
            !index_exists(&conn, "ix_tx_intents_from_open"),
            "migration must drop the redundant non-unique open index"
        );
    }

    #[test]
    fn database_newer_than_build_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("silo.db");
        drop(Db::open(&path).unwrap());
        let conn = Connection::open(&path).unwrap();
        conn.execute("UPDATE meta SET value='999' WHERE key='schema_version'", [])
            .unwrap();
        drop(conn);
        assert!(
            Db::open(&path).is_err(),
            "a database from a newer build must be refused, not silently downgraded"
        );
    }

    #[test]
    fn one_open_intent_per_wallet_is_enforced_by_a_unique_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("silo.db");
        let db = Db::open(&path).unwrap();
        db.conn
            .execute(
                "INSERT INTO wallets (account_index, role, pubkey, created_at)
                 VALUES (0,'master','PK',0)",
                [],
            )
            .unwrap();
        let wid: i64 = db
            .conn
            .query_row("SELECT id FROM wallets", [], |r| r.get(0))
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO tx_intents (from_wallet,to_address,lamports,status,created_at,updated_at)
                 VALUES (?1,'A',1,'submitted',0,0)",
                params![wid],
            )
            .unwrap();
        let second = db.conn.execute(
            "INSERT INTO tx_intents (from_wallet,to_address,lamports,status,created_at,updated_at)
             VALUES (?1,'B',1,'signed',0,0)",
            params![wid],
        );
        assert!(
            second.is_err(),
            "a second open intent for the same wallet must violate the unique index"
        );
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
    fn durability_guard_accepts_valid_pragmas() {
        let path = Path::new("/tmp/silo-test.db");
        assert!(verify_durability_pragmas(path, "wal", 2, 1).is_ok());
        assert!(
            verify_durability_pragmas(path, "WAL", 2, 1).is_ok(),
            "the WAL comparison must be case-insensitive"
        );
    }

    #[test]
    fn durability_guard_rejects_non_wal() {
        let path = Path::new("/tmp/silo-test.db");
        let err = verify_durability_pragmas(path, "delete", 2, 1).unwrap_err();
        assert!(
            err.to_string().contains("WAL"),
            "non-WAL journal mode must be refused with a WAL/crash-safe message, got: {err}"
        );
    }

    #[test]
    fn durability_guard_rejects_non_full_synchronous() {
        let path = Path::new("/tmp/silo-test.db");
        let err = verify_durability_pragmas(path, "wal", 1, 1).unwrap_err();
        assert!(
            err.to_string().contains("synchronous"),
            "synchronous != FULL must be refused, got: {err}"
        );
    }

    #[test]
    fn durability_guard_rejects_disabled_foreign_keys() {
        let path = Path::new("/tmp/silo-test.db");
        let err = verify_durability_pragmas(path, "wal", 2, 0).unwrap_err();
        assert!(
            err.to_string().contains("foreign_keys"),
            "foreign_keys OFF must be refused, got: {err}"
        );
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
        d.insert_wallet(
            0,
            Role::Master,
            "M111111111111111111111111111111111111111111A",
            None,
        )
        .unwrap();
        assert!(d.verify_audit_chain().unwrap());

        d.unlock_audit_key(&vk).unwrap();
        assert!(d.verify_audit_chain().unwrap());

        d.unlock_audit_key(&[1u8; 32]).unwrap();
        assert!(!d.verify_audit_chain().unwrap());

        // Do not append with the wrong key: returning to the original key must
        // validate the untouched chain and allow another authenticated write.
        d.unlock_audit_key(&vk).unwrap();
        assert!(d.verify_audit_chain().unwrap());
        d.audit(AuditEvent::SettingsChanged, &serde_json::json!({}))
            .unwrap();
        assert_eq!(d.list_audit(10).unwrap().len(), 2);
        assert!(d.verify_audit_chain().unwrap());
    }

    #[test]
    fn locked_db_refuses_audited_writes() {
        let mut d = db();
        let vk = [9u8; 32];
        d.unlock_audit_key(&vk).unwrap();
        let wallet = d
            .insert_wallet(0, Role::Master, "Master", Some("original"))
            .unwrap();
        d.set_note(wallet.id, Some("original note")).unwrap();
        d.set_meta_audited(
            "currency",
            "usd",
            AuditEvent::SettingsChanged,
            &serde_json::json!({"currency":"usd"}),
        )
        .unwrap();
        let audit_count = d.list_audit(100).unwrap().len();
        let audit_head = d.get_meta("audit_head_hash").unwrap();

        for _ in 0..2 {
            d.lock_audit_key();
            assert!(require_audit_key(&d.audit_key).is_err());
            assert!(d.verify_audit_chain().is_err());
            assert!(d.insert_wallet(1, Role::Sub, "Sub", None).is_err());
            assert!(d.set_label(wallet.id, Some("changed")).is_err());
            assert!(d.set_note(wallet.id, Some("changed")).is_err());
            assert!(d.set_archived(wallet.id, true).is_err());
            assert!(
                d.set_meta_audited(
                    "currency",
                    "eur",
                    AuditEvent::SettingsChanged,
                    &serde_json::json!({"currency":"eur"}),
                )
                .is_err()
            );
            assert_eq!(d.list_wallets().unwrap().len(), 1);
            let unchanged = d.get_wallet(wallet.id).unwrap().unwrap();
            assert_eq!(unchanged.label.as_deref(), Some("original"));
            assert_eq!(unchanged.note.as_deref(), Some("original note"));
            assert!(!unchanged.archived);
            assert_eq!(d.get_meta("currency").unwrap().as_deref(), Some("usd"));
            // Preserve the bare audit call's locked no-op policy for this change.
            d.audit(
                AuditEvent::SettingsChanged,
                &serde_json::json!({"locked":true}),
            )
            .unwrap();
            assert_eq!(d.list_audit(100).unwrap().len(), audit_count);
            assert_eq!(d.get_meta("audit_head_hash").unwrap(), audit_head);
        }

        d.unlock_audit_key(&vk).unwrap();
        assert!(d.verify_audit_chain().unwrap());
        d.set_label(wallet.id, Some("unlocked")).unwrap();
        assert_eq!(d.list_audit(100).unwrap().len(), audit_count + 1);
        assert!(d.verify_audit_chain().unwrap());
    }

    #[test]
    fn invalid_audit_salt_preserves_current_key() {
        let mut d = db();
        d.unlock_audit_key(&[9u8; 32]).unwrap();
        d.audit(AuditEvent::SettingsChanged, &serde_json::json!({}))
            .unwrap();
        let salt = d.get_meta("audit_key_salt").unwrap().unwrap();
        d.set_meta("audit_key_salt", "invalid").unwrap();
        let err = d.unlock_audit_key(&[1u8; 32]).unwrap_err();
        assert_eq!(err.to_string(), "corrupt audit_key_salt");
        assert!(d.verify_audit_chain().unwrap());
        d.audit(AuditEvent::SettingsChanged, &serde_json::json!({}))
            .unwrap();
        d.set_meta("audit_key_salt", &salt).unwrap();
        d.unlock_audit_key(&[9u8; 32]).unwrap();
        assert_eq!(d.list_audit(10).unwrap().len(), 2);
        assert!(d.verify_audit_chain().unwrap());
    }

    #[test]
    fn unlocked_db_drop_reopens_locked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("silo.db");
        let vk = [9u8; 32];
        let mut d = Db::open(&path).unwrap();
        d.unlock_audit_key(&vk).unwrap();
        let wallet = d.insert_wallet(0, Role::Master, "Master", None).unwrap();
        assert!(d.verify_audit_chain().unwrap());
        drop(d); // No explicit lock before dropping the live key.

        let mut d = Db::open(&path).unwrap();
        assert!(require_audit_key(&d.audit_key).is_err());
        assert!(d.verify_audit_chain().is_err());
        assert!(d.set_label(wallet.id, Some("locked")).is_err());
        assert_eq!(d.list_audit(10).unwrap().len(), 1);
        assert!(d.get_wallet(wallet.id).unwrap().unwrap().label.is_none());
        d.unlock_audit_key(&vk).unwrap();
        assert!(d.verify_audit_chain().unwrap());
        d.set_label(wallet.id, Some("unlocked")).unwrap();
        assert_eq!(d.list_audit(10).unwrap().len(), 2);
        assert!(d.verify_audit_chain().unwrap());
    }

    #[test]
    fn storage_replacement_uses_only_the_new_databases_key() {
        let dir = tempfile::tempdir().unwrap();
        let path_a = dir.path().join("a.db");
        let path_b = dir.path().join("b.db");
        let locked_path = dir.path().join("locked.db");
        let mut a = Db::open(&path_a).unwrap();
        a.unlock_audit_key(&[9u8; 32]).unwrap();
        a.set_meta_audited(
            "profile",
            "A",
            AuditEvent::SettingsChanged,
            &serde_json::json!({"profile":"A"}),
        )
        .unwrap();
        assert!(a.verify_audit_chain().unwrap());
        let mut b = Db::open(&path_b).unwrap();
        b.unlock_audit_key(&[1u8; 32]).unwrap();
        b.set_meta_audited(
            "profile",
            "B",
            AuditEvent::SettingsChanged,
            &serde_json::json!({"profile":"B"}),
        )
        .unwrap();
        assert!(b.verify_audit_chain().unwrap());

        let storage = Storage::new(a);
        storage.call_blocking(|d| {
            assert_eq!(d.get_meta("profile").unwrap().as_deref(), Some("A"));
            assert!(d.verify_audit_chain().unwrap());
        });
        storage.replace(b);
        storage.call_blocking(|d| {
            assert_eq!(d.get_meta("profile").unwrap().as_deref(), Some("B"));
            assert_eq!(d.list_audit(10).unwrap().len(), 1);
            assert!(d.verify_audit_chain().unwrap());
            d.audit(
                AuditEvent::SettingsChanged,
                &serde_json::json!({"append":"B"}),
            )
            .unwrap();
            assert_eq!(d.list_audit(10).unwrap().len(), 2);
            assert!(d.verify_audit_chain().unwrap());
        });

        storage.replace(Db::open(&locked_path).unwrap());
        storage.call_blocking(|d| {
            assert!(require_audit_key(&d.audit_key).is_err());
            assert!(d.verify_audit_chain().is_err());
            assert!(d.get_meta("profile").unwrap().is_none());
            assert!(d.insert_wallet(0, Role::Master, "Locked", None).is_err());
            assert!(
                d.set_meta_audited(
                    "profile",
                    "inherited",
                    AuditEvent::SettingsChanged,
                    &serde_json::json!({}),
                )
                .is_err()
            );
            assert!(d.get_meta("profile").unwrap().is_none());
            assert!(d.list_wallets().unwrap().is_empty());
            assert!(d.list_audit(10).unwrap().is_empty());
            assert!(d.get_meta("audit_head_hash").unwrap().is_none());
        });

        // replace returns after dropping the displaced Db, unlike drop(Storage),
        // which does not join the actor. Move the final file-backed Db out too.
        storage.replace(Db::open_memory().unwrap());
        for (path, vk, profile, count) in
            [(&path_a, [9u8; 32], "A", 1), (&path_b, [1u8; 32], "B", 2)]
        {
            let mut d = Db::open(path).unwrap();
            assert!(d.verify_audit_chain().is_err());
            assert_eq!(d.get_meta("profile").unwrap().as_deref(), Some(profile));
            assert_eq!(d.list_audit(10).unwrap().len(), count);
            d.unlock_audit_key(&vk).unwrap();
            assert!(d.verify_audit_chain().unwrap());
        }
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

    #[test]
    fn actor_serializes_blocking_writes_and_async_reads() {
        let storage = Storage::new(Db::open_memory().unwrap());
        storage.call_blocking(|d| {
            d.insert_wallet(
                0,
                Role::Master,
                "M1111111111111111111111111111111111111111111",
                None,
            )
            .unwrap();
        });
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let count = rt.block_on(storage.call(|d| d.list_wallets().unwrap().len()));
        assert_eq!(
            count, 1,
            "an async read must observe a prior blocking write"
        );
    }

    #[test]
    fn actor_call_current_skips_stale_generation() {
        let storage = Storage::new(Db::open_memory().unwrap());
        let generation = Arc::new(AtomicU64::new(5));
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let ran = rt.block_on(storage.call_current(generation.clone(), 5, |_d| 42));
        assert_eq!(ran, Some(42), "matching generation must run the job");
        let skipped = rt.block_on(storage.call_current(generation.clone(), 4, |_d| 99));
        assert_eq!(
            skipped, None,
            "a stale generation must skip the job entirely"
        );
    }

    #[test]
    fn actor_long_job_does_not_block_the_executor() {
        let storage = Storage::new(Db::open_memory().unwrap());
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_time()
            .build()
            .unwrap();
        rt.block_on(async move {
            let ticks = Arc::new(AtomicU64::new(0));
            let ticker_ticks = ticks.clone();
            let ticker = tokio::spawn(async move {
                for _ in 0..40 {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                    ticker_ticks.fetch_add(1, Ordering::SeqCst);
                }
            });
            storage
                .call(|_d| std::thread::sleep(Duration::from_millis(60)))
                .await;
            assert!(
                ticks.load(Ordering::SeqCst) >= 3,
                "the single executor thread must keep advancing other tasks while a DB \
                 job runs on the actor thread"
            );
            ticker.await.unwrap();
        });
    }
}
