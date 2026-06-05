use anyhow::Result;
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde_json::json;

use super::{Db, append_audit, now_ms};
use crate::types::{AuditEvent, Intent, IntentStatus};

#[derive(Debug, thiserror::Error)]
pub enum CreateIntentError {
    #[error("this wallet already has a transfer in progress")]
    WalletHasOpenIntent,
    #[error("database error: {0}")]
    Db(String),
}

const INTENT_COLS: &str = "id, from_wallet, to_address, lamports, fee_lamports, status, signature, \
    recent_blockhash, last_valid_block_height, signed_tx, note, error, created_at, updated_at";

fn row_to_intent(r: &rusqlite::Row) -> rusqlite::Result<Intent> {
    let status_str: String = r.get(5)?;
    Ok(Intent {
        id: r.get(0)?,
        from_wallet: r.get(1)?,
        to_address: r.get(2)?,
        lamports: r.get::<_, i64>(3)? as u64,
        fee_lamports: r.get::<_, Option<i64>>(4)?.map(|v| v as u64),
        status: IntentStatus::from_db_str(&status_str).unwrap_or(IntentStatus::Failed),
        signature: r.get(6)?,
        recent_blockhash: r.get(7)?,
        last_valid_block_height: r.get::<_, Option<i64>>(8)?.map(|v| v as u64),
        signed_tx: r.get(9)?,
        note: r.get(10)?,
        error: r.get(11)?,
        created_at: r.get(12)?,
        updated_at: r.get(13)?,
    })
}

impl Db {
    pub fn create_intent(
        &mut self,
        from_wallet: i64,
        to_address: &str,
        lamports: u64,
        note: Option<&str>,
    ) -> Result<Intent, CreateIntentError> {
        let now = now_ms();
        let key = self
            .require_audit_key()
            .map_err(|e| CreateIntentError::Db(e.to_string()))?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| CreateIntentError::Db(e.to_string()))?;

        let open: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM tx_intents
                 WHERE from_wallet=?1 AND status IN ('created','signed','submitted') LIMIT 1",
                params![from_wallet],
                |_| Ok(1),
            )
            .optional()
            .map_err(|e| CreateIntentError::Db(e.to_string()))?;
        if open.is_some() {
            return Err(CreateIntentError::WalletHasOpenIntent);
        }

        tx.execute(
            "INSERT INTO tx_intents (from_wallet, to_address, lamports, status, note, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'created', ?4, ?5, ?5)",
            params![from_wallet, to_address, lamports as i64, note, now],
        )
        .map_err(|e| CreateIntentError::Db(e.to_string()))?;
        let id = tx.last_insert_rowid();
        append_audit(
            &tx,
            &key,
            AuditEvent::IntentCreated,
            &json!({"id": id, "from_wallet": from_wallet, "to": to_address, "lamports": lamports}),
        )
        .map_err(|e| CreateIntentError::Db(e.to_string()))?;
        tx.commit()
            .map_err(|e| CreateIntentError::Db(e.to_string()))?;

        Ok(Intent {
            id,
            from_wallet,
            to_address: to_address.to_string(),
            lamports,
            fee_lamports: None,
            status: IntentStatus::Created,
            signature: None,
            recent_blockhash: None,
            last_valid_block_height: None,
            signed_tx: None,
            note: note.map(String::from),
            error: None,
            created_at: now,
            updated_at: now,
        })
    }

    pub fn mark_signed(
        &mut self,
        id: i64,
        signature: &str,
        recent_blockhash: &str,
        last_valid_block_height: u64,
        fee_lamports: u64,
        signed_tx: &[u8],
    ) -> Result<()> {
        let now = now_ms();
        let key = self.require_audit_key()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE tx_intents SET status='signed', signature=?1, recent_blockhash=?2,
             last_valid_block_height=?3, fee_lamports=?4, signed_tx=?5, updated_at=?6 WHERE id=?7",
            params![
                signature,
                recent_blockhash,
                last_valid_block_height as i64,
                fee_lamports as i64,
                signed_tx,
                now,
                id
            ],
        )?;
        append_audit(
            &tx,
            &key,
            AuditEvent::IntentSigned,
            &json!({"id": id, "signature": signature}),
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn mark_submitted(&mut self, id: i64) -> Result<bool> {
        let now = now_ms();
        let key = self.require_audit_key()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let n = tx.execute(
            "UPDATE tx_intents SET status='submitted', updated_at=?1
             WHERE id=?2 AND status='signed'",
            params![now, id],
        )?;
        if n == 0 {
            return Ok(false);
        }
        append_audit(&tx, &key, AuditEvent::IntentSubmitted, &json!({"id": id}))?;
        tx.commit()?;
        Ok(true)
    }

    pub fn mark_terminal(
        &mut self,
        id: i64,
        status: IntentStatus,
        error: Option<&str>,
    ) -> Result<bool> {
        debug_assert!(status.is_terminal());
        let event = match status {
            IntentStatus::Confirmed => AuditEvent::IntentConfirmed,
            IntentStatus::Failed => AuditEvent::IntentFailed,
            IntentStatus::Expired => AuditEvent::IntentExpired,
            _ => AuditEvent::IntentFailed,
        };
        let now = now_ms();
        let key = self.require_audit_key()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let n = tx.execute(
            "UPDATE tx_intents SET status=?1, error=?2, updated_at=?3
             WHERE id=?4 AND status NOT IN ('confirmed','failed','expired')",
            params![status.as_str(), error, now, id],
        )?;
        if n == 0 {
            return Ok(false);
        }
        append_audit(
            &tx,
            &key,
            event,
            &json!({"id": id, "status": status.as_str(), "error": error}),
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub fn set_intent_note(&mut self, id: i64, note: Option<&str>) -> Result<()> {
        let key = self.require_audit_key()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE tx_intents SET note=?1, updated_at=?2 WHERE id=?3",
            params![note, now_ms(), id],
        )?;
        append_audit(&tx, &key, AuditEvent::IntentNoted, &json!({"id": id}))?;
        tx.commit()?;
        Ok(())
    }

    pub fn get_intent(&self, id: i64) -> Result<Option<Intent>> {
        let sql = format!("SELECT {INTENT_COLS} FROM tx_intents WHERE id=?1");
        Ok(self
            .conn
            .query_row(&sql, params![id], row_to_intent)
            .optional()?)
    }

    pub fn get_open_intents(&self) -> Result<Vec<Intent>> {
        let sql = format!(
            "SELECT {INTENT_COLS} FROM tx_intents
             WHERE status IN ('created','signed','submitted') ORDER BY id ASC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], row_to_intent)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn list_intents_for_wallet(&self, wallet_id: i64, limit: usize) -> Result<Vec<Intent>> {
        let sql = format!(
            "SELECT {INTENT_COLS} FROM tx_intents
             WHERE from_wallet=?1 ORDER BY created_at DESC, id DESC LIMIT ?2"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![wallet_id, limit as i64], row_to_intent)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn has_open_intent(&self, wallet_id: i64) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM tx_intents
                 WHERE from_wallet=?1 AND status IN ('created','signed','submitted') LIMIT 1",
                params![wallet_id],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Role;

    fn db_with_two_wallets() -> (Db, i64, i64) {
        let mut d = Db::open_memory().unwrap();
        let m = d
            .insert_wallet(
                0,
                Role::Master,
                "Master11111111111111111111111111111111111111",
                None,
            )
            .unwrap();
        let s = d
            .insert_wallet(
                1,
                Role::Sub,
                "Sub1111111111111111111111111111111111111111A",
                None,
            )
            .unwrap();
        (d, m.id, s.id)
    }

    #[test]
    fn one_open_intent_per_wallet() {
        let (mut d, m, _s) = db_with_two_wallets();
        let i1 = d
            .create_intent(m, "DestAddr11111111111111111111111111111111111", 1000, None)
            .unwrap();
        match d.create_intent(m, "DestAddr22222222222222222222222222222222222", 2000, None) {
            Err(CreateIntentError::WalletHasOpenIntent) => {}
            other => panic!("expected WalletHasOpenIntent, got {other:?}"),
        }
        d.mark_terminal(i1.id, IntentStatus::Failed, Some("test"))
            .unwrap();
        assert!(
            d.create_intent(m, "DestAddr33333333333333333333333333333333333", 3000, None)
                .is_ok()
        );
    }

    #[test]
    fn set_intent_note_persists_and_audits() {
        let (mut d, m, _s) = db_with_two_wallets();
        let i = d
            .create_intent(m, "Dest11111111111111111111111111111111111111A", 1000, None)
            .unwrap();
        d.set_intent_note(i.id, Some("rent for May")).unwrap();
        assert_eq!(
            d.get_intent(i.id).unwrap().unwrap().note.as_deref(),
            Some("rent for May")
        );
        assert!(d.verify_audit_chain().unwrap());
        d.set_intent_note(i.id, None).unwrap();
        assert!(d.get_intent(i.id).unwrap().unwrap().note.is_none());
    }

    #[test]
    fn zero_amount_rejected_by_check() {
        let (mut d, m, _s) = db_with_two_wallets();
        assert!(d.create_intent(m, "Dest", 0, None).is_err());
    }

    #[test]
    fn signature_is_unique_when_present() {
        let (mut d, m, s) = db_with_two_wallets();
        let im = d
            .create_intent(m, "DestA1111111111111111111111111111111111111A", 1000, None)
            .unwrap();
        d.mark_signed(im.id, "SharedSig", "bhash", 100, 5000, b"bytesA")
            .unwrap();
        let is = d
            .create_intent(s, "DestB2222222222222222222222222222222222222B", 1000, None)
            .unwrap();
        assert!(
            d.mark_signed(is.id, "SharedSig", "bhash", 100, 5000, b"bytesB")
                .is_err()
        );
    }

    #[test]
    fn full_lifecycle_and_audit_intact() {
        let (mut d, m, _s) = db_with_two_wallets();
        let i = d
            .create_intent(
                m,
                "Dest11111111111111111111111111111111111111A",
                500_000,
                None,
            )
            .unwrap();
        assert_eq!(i.status, IntentStatus::Created);
        d.mark_signed(i.id, "Sig123", "BlockHashXYZ", 12345, 5000, b"\x01\x02\x03")
            .unwrap();
        d.mark_submitted(i.id).unwrap();
        d.mark_terminal(i.id, IntentStatus::Confirmed, None)
            .unwrap();

        let got = d.get_intent(i.id).unwrap().unwrap();
        assert_eq!(got.status, IntentStatus::Confirmed);
        assert_eq!(got.signature.as_deref(), Some("Sig123"));
        assert_eq!(got.fee_lamports, Some(5000));
        assert_eq!(got.signed_tx.as_deref(), Some(&b"\x01\x02\x03"[..]));
        assert!(d.get_open_intents().unwrap().is_empty());
        assert!(
            d.verify_audit_chain().unwrap(),
            "all transitions audited atomically"
        );
    }

    #[test]
    fn terminal_transition_is_compare_and_swap() {
        let (mut d, m, _s) = db_with_two_wallets();
        let i = d
            .create_intent(m, "Dest11111111111111111111111111111111111111A", 1000, None)
            .unwrap();
        d.mark_signed(i.id, "Sig", "bh", 100, 5000, b"x").unwrap();
        assert!(
            d.mark_terminal(i.id, IntentStatus::Confirmed, None)
                .unwrap()
        );
        assert!(
            !d.mark_terminal(i.id, IntentStatus::Expired, Some("late"))
                .unwrap()
        );
        let got = d.get_intent(i.id).unwrap().unwrap();
        assert_eq!(
            got.status,
            IntentStatus::Confirmed,
            "confirmed must not be overwritten by expired"
        );
        assert!(got.error.is_none());
    }

    #[test]
    fn mark_submitted_only_from_signed() {
        let (mut d, m, _s) = db_with_two_wallets();
        let i = d
            .create_intent(m, "Dest11111111111111111111111111111111111111A", 1000, None)
            .unwrap();
        assert!(!d.mark_submitted(i.id).unwrap());
        d.mark_signed(i.id, "Sig", "bh", 100, 5000, b"x").unwrap();
        assert!(d.mark_submitted(i.id).unwrap());
        assert!(!d.mark_submitted(i.id).unwrap());
    }

    #[test]
    fn open_intents_listing() {
        let (mut d, m, s) = db_with_two_wallets();
        let im = d
            .create_intent(m, "DestA1111111111111111111111111111111111111A", 1000, None)
            .unwrap();
        d.create_intent(s, "DestB2222222222222222222222222222222222222B", 2000, None)
            .unwrap();
        assert_eq!(d.get_open_intents().unwrap().len(), 2);
        d.mark_terminal(im.id, IntentStatus::Confirmed, None)
            .unwrap();
        let open = d.get_open_intents().unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].from_wallet, s);
    }
}
