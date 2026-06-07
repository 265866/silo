use anyhow::{Result, bail};
use rusqlite::types::Type;
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde_json::json;

use super::{Db, append_audit, now_ms};
use crate::types::{AuditEvent, Intent, IntentStatus};

#[derive(Debug, thiserror::Error)]
pub enum CreateIntentError {
    #[error("audit key unavailable (vault locked)")]
    AuditKeyLocked(#[source] anyhow::Error),
    #[error("this wallet already has a transfer in progress")]
    WalletHasOpenIntent,
    #[error("{field} is too large to store")]
    ValueOutOfRange { field: &'static str },
    #[error("database error while {context}")]
    Db {
        context: &'static str,
        #[source]
        source: rusqlite::Error,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntentTransitionOutcome {
    Applied,
    NotFound,
    WrongState(IntentStatus),
}

#[derive(Debug, thiserror::Error)]
pub enum IntentTransitionError {
    #[error("audit key unavailable (vault locked)")]
    AuditKeyLocked(#[source] anyhow::Error),
    #[error("{field} is too large to store")]
    ValueOutOfRange { field: &'static str },
    #[error("database error while {context}")]
    Db {
        context: &'static str,
        #[source]
        source: rusqlite::Error,
    },
}

const INTENT_COLS: &str = "id, from_wallet, to_address, lamports, fee_lamports, status, signature, \
    recent_blockhash, last_valid_block_height, signed_tx, note, error, created_at, updated_at";

fn row_to_intent(r: &rusqlite::Row) -> rusqlite::Result<Intent> {
    let status_str: String = r.get(5)?;
    let status = IntentStatus::from_db_str(&status_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            5,
            Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown intent status '{status_str}'"),
            )),
        )
    })?;
    Ok(Intent {
        id: r.get(0)?,
        from_wallet: r.get(1)?,
        to_address: r.get(2)?,
        lamports: read_u64(r, 3, "lamports")?,
        fee_lamports: read_optional_u64(r, 4, "fee_lamports")?,
        status,
        signature: r.get(6)?,
        recent_blockhash: r.get(7)?,
        last_valid_block_height: read_optional_u64(r, 8, "last_valid_block_height")?,
        signed_tx: r.get(9)?,
        note: r.get(10)?,
        error: r.get(11)?,
        created_at: r.get(12)?,
        updated_at: r.get(13)?,
    })
}

fn read_u64(r: &rusqlite::Row, column: usize, field: &'static str) -> rusqlite::Result<u64> {
    let value = r.get::<_, i64>(column)?;
    u64_from_i64(value, column, field)
}

fn read_optional_u64(
    r: &rusqlite::Row,
    column: usize,
    field: &'static str,
) -> rusqlite::Result<Option<u64>> {
    r.get::<_, Option<i64>>(column)?
        .map(|value| u64_from_i64(value, column, field))
        .transpose()
}

fn u64_from_i64(value: i64, column: usize, field: &'static str) -> rusqlite::Result<u64> {
    if value < 0 {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            column,
            Type::Integer,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{field} cannot be negative: {value}"),
            )),
        ));
    }
    Ok(value as u64)
}

fn transition_miss(
    tx: &rusqlite::Transaction<'_>,
    id: i64,
) -> Result<IntentTransitionOutcome, IntentTransitionError> {
    let status = tx
        .query_row(
            "SELECT status FROM tx_intents WHERE id=?1",
            params![id],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .map_err(|source| IntentTransitionError::Db {
            context: "checking intent transition state",
            source,
        })?;
    match status {
        None => Ok(IntentTransitionOutcome::NotFound),
        Some(status) => {
            let status =
                IntentStatus::from_db_str(&status).ok_or_else(|| IntentTransitionError::Db {
                    context: "decoding intent transition state",
                    source: rusqlite::Error::FromSqlConversionFailure(
                        0,
                        Type::Text,
                        Box::new(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("unknown intent status '{status}'"),
                        )),
                    ),
                })?;
            Ok(IntentTransitionOutcome::WrongState(status))
        }
    }
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
        let lamports_i64 = i64::try_from(lamports)
            .map_err(|_| CreateIntentError::ValueOutOfRange { field: "lamports" })?;
        let key = self
            .require_audit_key()
            .map_err(CreateIntentError::AuditKeyLocked)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| CreateIntentError::Db {
                context: "starting intent transaction",
                source,
            })?;

        let open: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM tx_intents
                 WHERE from_wallet=?1 AND status IN ('created','signed','submitted') LIMIT 1",
                params![from_wallet],
                |_| Ok(1),
            )
            .optional()
            .map_err(|source| CreateIntentError::Db {
                context: "checking open intent",
                source,
            })?;
        if open.is_some() {
            return Err(CreateIntentError::WalletHasOpenIntent);
        }

        tx.execute(
            "INSERT INTO tx_intents (from_wallet, to_address, lamports, status, note, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'created', ?4, ?5, ?5)",
            params![from_wallet, to_address, lamports_i64, note, now],
        )
        .map_err(|source| CreateIntentError::Db {
            context: "inserting intent",
            source,
        })?;
        let id = tx.last_insert_rowid();
        append_audit(
            &tx,
            &key,
            AuditEvent::IntentCreated,
            &json!({"id": id, "from_wallet": from_wallet, "to": to_address, "lamports": lamports}),
        )
        .map_err(|source| CreateIntentError::Db {
            context: "appending intent audit",
            source,
        })?;
        tx.commit().map_err(|source| CreateIntentError::Db {
            context: "committing intent transaction",
            source,
        })?;

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
    ) -> Result<IntentTransitionOutcome, IntentTransitionError> {
        let now = now_ms();
        let last_valid_block_height = i64::try_from(last_valid_block_height).map_err(|_| {
            IntentTransitionError::ValueOutOfRange {
                field: "last_valid_block_height",
            }
        })?;
        let fee_lamports =
            i64::try_from(fee_lamports).map_err(|_| IntentTransitionError::ValueOutOfRange {
                field: "fee_lamports",
            })?;
        let key = self
            .require_audit_key()
            .map_err(IntentTransitionError::AuditKeyLocked)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| IntentTransitionError::Db {
                context: "starting signed intent transition",
                source,
            })?;
        let n = tx
            .execute(
                "UPDATE tx_intents SET status='signed', signature=?1, recent_blockhash=?2,
             last_valid_block_height=?3, fee_lamports=?4, signed_tx=?5, updated_at=?6
             WHERE id=?7 AND status='created'",
                params![
                    signature,
                    recent_blockhash,
                    last_valid_block_height,
                    fee_lamports,
                    signed_tx,
                    now,
                    id
                ],
            )
            .map_err(|source| IntentTransitionError::Db {
                context: "marking intent signed",
                source,
            })?;
        if n == 0 {
            return transition_miss(&tx, id);
        }
        append_audit(
            &tx,
            &key,
            AuditEvent::IntentSigned,
            &json!({"id": id, "signature": signature}),
        )
        .map_err(|source| IntentTransitionError::Db {
            context: "appending signed intent audit",
            source,
        })?;
        tx.commit().map_err(|source| IntentTransitionError::Db {
            context: "committing signed intent transition",
            source,
        })?;
        Ok(IntentTransitionOutcome::Applied)
    }

    #[cfg(test)]
    pub(crate) fn clear_signed_tx_for_test(&mut self, id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE tx_intents SET signed_tx=NULL WHERE id=?1",
            params![id],
        )?;
        Ok(())
    }

    pub fn mark_submitted(
        &mut self,
        id: i64,
    ) -> Result<IntentTransitionOutcome, IntentTransitionError> {
        let now = now_ms();
        let key = self
            .require_audit_key()
            .map_err(IntentTransitionError::AuditKeyLocked)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| IntentTransitionError::Db {
                context: "starting intent transition",
                source,
            })?;
        let n = tx
            .execute(
                "UPDATE tx_intents SET status='submitted', updated_at=?1
             WHERE id=?2 AND status='signed'",
                params![now, id],
            )
            .map_err(|source| IntentTransitionError::Db {
                context: "marking intent submitted",
                source,
            })?;
        if n == 0 {
            return transition_miss(&tx, id);
        }
        append_audit(&tx, &key, AuditEvent::IntentSubmitted, &json!({"id": id})).map_err(
            |source| IntentTransitionError::Db {
                context: "appending transition audit",
                source,
            },
        )?;
        tx.commit().map_err(|source| IntentTransitionError::Db {
            context: "committing intent transition",
            source,
        })?;
        Ok(IntentTransitionOutcome::Applied)
    }

    pub fn mark_terminal(
        &mut self,
        id: i64,
        status: IntentStatus,
        error: Option<&str>,
    ) -> Result<IntentTransitionOutcome, IntentTransitionError> {
        debug_assert!(status.is_terminal());
        let event = match status {
            IntentStatus::Confirmed => AuditEvent::IntentConfirmed,
            IntentStatus::Failed => AuditEvent::IntentFailed,
            IntentStatus::Expired => AuditEvent::IntentExpired,
            _ => AuditEvent::IntentFailed,
        };
        let now = now_ms();
        let key = self
            .require_audit_key()
            .map_err(IntentTransitionError::AuditKeyLocked)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| IntentTransitionError::Db {
                context: "starting intent transition",
                source,
            })?;
        let n = tx
            .execute(
                "UPDATE tx_intents SET status=?1, error=?2, updated_at=?3
             WHERE id=?4 AND status NOT IN ('confirmed','failed','expired')",
                params![status.as_str(), error, now, id],
            )
            .map_err(|source| IntentTransitionError::Db {
                context: "marking intent terminal",
                source,
            })?;
        if n == 0 {
            return transition_miss(&tx, id);
        }
        append_audit(
            &tx,
            &key,
            event,
            &json!({"id": id, "status": status.as_str(), "error": error}),
        )
        .map_err(|source| IntentTransitionError::Db {
            context: "appending transition audit",
            source,
        })?;
        tx.commit().map_err(|source| IntentTransitionError::Db {
            context: "committing intent transition",
            source,
        })?;
        Ok(IntentTransitionOutcome::Applied)
    }

    pub fn set_intent_note(&mut self, id: i64, note: Option<&str>) -> Result<()> {
        let key = self.require_audit_key()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let n = tx.execute(
            "UPDATE tx_intents SET note=?1, updated_at=?2 WHERE id=?3 AND note IS NOT ?1",
            params![note, now_ms(), id],
        )?;
        if n == 0 {
            let exists = tx
                .query_row("SELECT 1 FROM tx_intents WHERE id=?1", params![id], |_| {
                    Ok(())
                })
                .optional()?
                .is_some();
            if exists {
                bail!("transfer note is unchanged");
            }
            bail!("transfer not found");
        }
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
    fn invalid_status_is_a_read_error() {
        let (d, m, _s) = db_with_two_wallets();
        d.conn
            .execute_batch("PRAGMA ignore_check_constraints=ON;")
            .unwrap();
        d.conn
            .execute(
                "INSERT INTO tx_intents
                 (from_wallet, to_address, lamports, status, created_at, updated_at)
                 VALUES (?1, 'Dest11111111111111111111111111111111111111A', 1000, 'mystery', 1, 1)",
                params![m],
            )
            .unwrap();
        let id = d.conn.last_insert_rowid();
        assert!(d.get_intent(id).is_err());
    }

    #[test]
    fn zero_amount_rejected_by_check() {
        let (mut d, m, _s) = db_with_two_wallets();
        assert!(matches!(
            d.create_intent(m, "Dest", 0, None),
            Err(CreateIntentError::Db {
                context: "inserting intent",
                ..
            })
        ));
    }

    #[test]
    fn locked_audit_key_is_typed_create_intent_error() {
        let (mut d, m, _s) = db_with_two_wallets();
        d.lock_audit_key();
        assert!(matches!(
            d.create_intent(m, "Dest11111111111111111111111111111111111111A", 1000, None),
            Err(CreateIntentError::AuditKeyLocked(_))
        ));
    }

    #[test]
    fn nonexistent_transition_is_typed() {
        let (mut d, _m, _s) = db_with_two_wallets();
        assert_eq!(
            d.mark_submitted(999).unwrap(),
            IntentTransitionOutcome::NotFound
        );
        assert_eq!(
            d.mark_terminal(999, IntentStatus::Failed, Some("missing"))
                .unwrap(),
            IntentTransitionOutcome::NotFound
        );
    }

    #[test]
    fn mark_signed_only_from_created() {
        let (mut d, m, _s) = db_with_two_wallets();
        assert_eq!(
            d.mark_signed(999, "Sig", "bh", 100, 5000, b"x").unwrap(),
            IntentTransitionOutcome::NotFound
        );
        let i = d
            .create_intent(m, "Dest11111111111111111111111111111111111111A", 1000, None)
            .unwrap();
        assert_eq!(
            d.mark_signed(i.id, "Sig", "bh", 100, 5000, b"x").unwrap(),
            IntentTransitionOutcome::Applied
        );
        assert_eq!(
            d.mark_signed(i.id, "Sig2", "bh", 100, 5000, b"x").unwrap(),
            IntentTransitionOutcome::WrongState(IntentStatus::Signed)
        );
    }

    #[test]
    fn unchanged_intent_note_is_rejected_without_audit() {
        let (mut d, m, _s) = db_with_two_wallets();
        let i = d
            .create_intent(
                m,
                "Dest11111111111111111111111111111111111111A",
                1000,
                Some("rent"),
            )
            .unwrap();
        let before = d.list_audit(50).unwrap().len();
        assert!(d.set_intent_note(i.id, Some("rent")).is_err());
        assert_eq!(
            d.get_intent(i.id).unwrap().unwrap().note.as_deref(),
            Some("rent")
        );
        assert_eq!(d.list_audit(50).unwrap().len(), before);
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
        assert_eq!(
            d.mark_submitted(i.id).unwrap(),
            IntentTransitionOutcome::Applied
        );
        assert_eq!(
            d.mark_terminal(i.id, IntentStatus::Confirmed, None)
                .unwrap(),
            IntentTransitionOutcome::Applied
        );

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
        assert_eq!(
            d.mark_terminal(i.id, IntentStatus::Confirmed, None)
                .unwrap(),
            IntentTransitionOutcome::Applied
        );
        assert_eq!(
            d.mark_terminal(i.id, IntentStatus::Expired, Some("late"))
                .unwrap(),
            IntentTransitionOutcome::WrongState(IntentStatus::Confirmed)
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
        assert_eq!(
            d.mark_submitted(i.id).unwrap(),
            IntentTransitionOutcome::WrongState(IntentStatus::Created)
        );
        d.mark_signed(i.id, "Sig", "bh", 100, 5000, b"x").unwrap();
        assert_eq!(
            d.mark_submitted(i.id).unwrap(),
            IntentTransitionOutcome::Applied
        );
        assert_eq!(
            d.mark_submitted(i.id).unwrap(),
            IntentTransitionOutcome::WrongState(IntentStatus::Submitted)
        );
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
