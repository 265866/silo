use anyhow::{Result, bail};
use rusqlite::types::Type;
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde_json::json;

use super::{Db, append_audit, now_ms};
use crate::types::{AuditEvent, Role, WalletRow};

fn row_to_wallet(r: &rusqlite::Row) -> rusqlite::Result<WalletRow> {
    let role_str: String = r.get(2)?;
    let archived_i: i64 = r.get(6)?;
    let has_open: i64 = r.get(8)?;
    let role = Role::from_db_str(&role_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            2,
            Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown wallet role '{role_str}'"),
            )),
        )
    })?;
    Ok(WalletRow {
        id: r.get(0)?,
        account_index: read_u32(r, 1, "account_index")?,
        role,
        pubkey: r.get(3)?,
        label: r.get(4)?,
        note: r.get(5)?,
        archived: archived_i != 0,
        created_at: r.get(7)?,
        balance_lamports: None,
        has_open_intent: has_open != 0,
    })
}

fn read_u32(r: &rusqlite::Row, column: usize, field: &'static str) -> rusqlite::Result<u32> {
    let value = r.get::<_, i64>(column)?;
    u32::try_from(value).map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            Type::Integer,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{field} is outside u32 range: {value}"),
            )),
        )
    })
}

fn read_next_account_index(max: i64) -> Result<u32> {
    let max = u32::try_from(max).map_err(|_| anyhow::anyhow!("account_index is corrupt"))?;
    max.checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("account_index is exhausted"))
}

fn reject_unchanged_wallet_update(
    tx: &rusqlite::Transaction<'_>,
    changed: usize,
    id: i64,
    field: &'static str,
) -> Result<()> {
    if changed > 0 {
        return Ok(());
    }
    let exists = tx
        .query_row("SELECT 1 FROM wallets WHERE id=?1", params![id], |_| Ok(()))
        .optional()?
        .is_some();
    if exists {
        bail!("{field} is unchanged");
    }
    bail!("wallet not found")
}

const SELECT_WALLET: &str = "
SELECT w.id, w.account_index, w.role, w.pubkey, w.label, w.note, w.archived, w.created_at,
  EXISTS(SELECT 1 FROM tx_intents t
         WHERE t.from_wallet = w.id AND t.status IN ('created','signed','submitted')) AS has_open
FROM wallets w";

impl Db {
    pub fn master_exists(&self) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM wallets WHERE role='master' LIMIT 1",
                [],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false))
    }

    pub fn next_account_index(&self) -> Result<u32> {
        let max: Option<i64> = self
            .conn
            .query_row("SELECT MAX(account_index) FROM wallets", [], |r| {
                r.get::<_, Option<i64>>(0)
            })
            .optional()?
            .flatten();
        Ok(match max {
            Some(m) => read_next_account_index(m)?,
            None => 0,
        })
    }

    pub fn insert_wallet(
        &mut self,
        account_index: u32,
        role: Role,
        pubkey: &str,
        label: Option<&str>,
    ) -> Result<WalletRow> {
        let now = now_ms();
        let key = self.require_audit_key()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO wallets (account_index, role, pubkey, label, note, archived, created_at)
             VALUES (?1, ?2, ?3, ?4, NULL, 0, ?5)",
            params![account_index as i64, role.as_str(), pubkey, label, now],
        )?;
        let id = tx.last_insert_rowid();
        append_audit(
            &tx,
            &key,
            AuditEvent::WalletDerived,
            &json!({"id": id, "account_index": account_index, "role": role.as_str(), "pubkey": pubkey}),
        )?;
        tx.commit()?;
        Ok(WalletRow {
            id,
            account_index,
            role,
            pubkey: pubkey.to_string(),
            label: label.map(String::from),
            note: None,
            archived: false,
            created_at: now,
            balance_lamports: None,
            has_open_intent: false,
        })
    }

    pub fn list_wallets(&self) -> Result<Vec<WalletRow>> {
        let sql = format!("{SELECT_WALLET} ORDER BY w.account_index ASC");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], row_to_wallet)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    #[allow(dead_code)]
    pub fn get_wallet(&self, id: i64) -> Result<Option<WalletRow>> {
        let sql = format!("{SELECT_WALLET} WHERE w.id = ?1");
        Ok(self
            .conn
            .query_row(&sql, params![id], row_to_wallet)
            .optional()?)
    }

    pub fn set_label(&mut self, id: i64, label: Option<&str>) -> Result<()> {
        let key = self.require_audit_key()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let n = tx.execute(
            "UPDATE wallets SET label=?1 WHERE id=?2 AND label IS NOT ?1",
            params![label, id],
        )?;
        reject_unchanged_wallet_update(&tx, n, id, "wallet label")?;
        append_audit(&tx, &key, AuditEvent::WalletLabeled, &json!({"id": id})).map(|_| ())?;
        tx.commit()?;
        Ok(())
    }

    pub fn set_note(&mut self, id: i64, note: Option<&str>) -> Result<()> {
        let key = self.require_audit_key()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let n = tx.execute(
            "UPDATE wallets SET note=?1 WHERE id=?2 AND note IS NOT ?1",
            params![note, id],
        )?;
        reject_unchanged_wallet_update(&tx, n, id, "wallet note")?;
        append_audit(&tx, &key, AuditEvent::WalletNoted, &json!({"id": id}))?;
        tx.commit()?;
        Ok(())
    }

    pub fn set_archived(&mut self, id: i64, archived: bool) -> Result<()> {
        if archived && self.has_open_intent(id)? {
            bail!("cannot archive a wallet with a transfer in progress");
        }
        let key = self.require_audit_key()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let n = tx.execute(
            "UPDATE wallets SET archived=?1 WHERE id=?2 AND archived<>?1",
            params![archived as i64, id],
        )?;
        reject_unchanged_wallet_update(&tx, n, id, "wallet archived state")?;
        append_audit(
            &tx,
            &key,
            AuditEvent::WalletArchived,
            &json!({"id": id, "archived": archived}),
        )?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        Db::open_memory().unwrap()
    }

    const PK0: &str = "Master11111111111111111111111111111111111111";
    const PK1: &str = "Sub1111111111111111111111111111111111111111A";
    const PK2: &str = "Sub2222222222222222222222222222222222222222B";

    #[test]
    fn insert_and_list() {
        let mut d = db();
        assert_eq!(d.next_account_index().unwrap(), 0);
        assert!(!d.master_exists().unwrap());

        let m = d
            .insert_wallet(0, Role::Master, PK0, Some("Treasury"))
            .unwrap();
        assert_eq!(m.account_index, 0);
        assert!(d.master_exists().unwrap());
        assert_eq!(d.next_account_index().unwrap(), 1);

        d.insert_wallet(1, Role::Sub, PK1, None).unwrap();
        let all = d.list_wallets().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].role, Role::Master);
        assert_eq!(all[1].account_index, 1);
    }

    #[test]
    fn invalid_role_is_a_read_error() {
        let d = db();
        d.conn
            .execute_batch("PRAGMA ignore_check_constraints=ON;")
            .unwrap();
        d.conn
            .execute(
                "INSERT INTO wallets (account_index, role, pubkey, label, note, archived, created_at)
                 VALUES (1, 'mystery', ?1, NULL, NULL, 0, 1)",
                params![PK1],
            )
            .unwrap();
        assert!(d.list_wallets().is_err());
    }

    #[test]
    fn single_master_enforced() {
        let mut d = db();
        d.insert_wallet(0, Role::Master, PK0, None).unwrap();
        assert!(d.insert_wallet(5, Role::Master, PK1, None).is_err());
    }

    #[test]
    fn master_must_be_index_zero() {
        let mut d = db();
        assert!(d.insert_wallet(3, Role::Master, PK0, None).is_err());
        assert!(d.insert_wallet(0, Role::Sub, PK1, None).is_err());
    }

    #[test]
    fn duplicate_index_or_pubkey_rejected() {
        let mut d = db();
        d.insert_wallet(0, Role::Master, PK0, None).unwrap();
        d.insert_wallet(1, Role::Sub, PK1, None).unwrap();
        assert!(
            d.insert_wallet(1, Role::Sub, PK2, None).is_err(),
            "dup index"
        );
        assert!(
            d.insert_wallet(2, Role::Sub, PK1, None).is_err(),
            "dup pubkey"
        );
    }

    #[test]
    fn unchanged_wallet_updates_are_rejected_without_audit() {
        let mut d = db();
        let m = d.insert_wallet(0, Role::Master, PK0, Some("Cold")).unwrap();
        let before = d.list_audit(50).unwrap().len();
        assert!(d.set_label(m.id, Some("Cold")).is_err());
        assert!(d.set_note(m.id, None).is_err());
        assert!(d.set_archived(m.id, false).is_err());
        assert_eq!(d.list_audit(50).unwrap().len(), before);
    }

    #[test]
    fn label_and_note_update() {
        let mut d = db();
        let m = d.insert_wallet(0, Role::Master, PK0, None).unwrap();
        d.set_label(m.id, Some("Cold")).unwrap();
        d.set_note(m.id, Some("hardware-backed")).unwrap();
        let w = d.get_wallet(m.id).unwrap().unwrap();
        assert_eq!(w.label.as_deref(), Some("Cold"));
        assert_eq!(w.note.as_deref(), Some("hardware-backed"));
        assert!(d.verify_audit_chain().unwrap());
    }
}
