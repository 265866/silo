use std::sync::Mutex;

use anyhow::Result;
use serde_json::json;

use super::rpc::{Rpc, SignatureStatus};
use crate::db::Db;
use crate::types::{AuditEvent, IntentStatus};

pub const EXPIRY_SLACK: u64 = 150;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Decision {
    Confirm,
    Fail(String),
    Expire,
    Rebroadcast,
}

fn decide(status: Option<&SignatureStatus>, current_height: u64, lvbh: u64) -> Decision {
    if let Some(st) = status {
        if st.is_error() {
            return Decision::Fail("on-chain error".to_string());
        }
        if st.is_confirmed_or_finalized() {
            return Decision::Confirm;
        }
        return Decision::Rebroadcast;
    }
    if current_height > lvbh + EXPIRY_SLACK {
        Decision::Expire
    } else {
        Decision::Rebroadcast
    }
}

pub async fn reconcile_boot(
    db: &Mutex<Db>,
    rpc: &Rpc,
    generation: &std::sync::atomic::AtomicU64,
    cmd_gen: u64,
) -> Result<usize> {
    use crate::db::with_current_db;
    use std::sync::atomic::Ordering;
    if generation.load(Ordering::SeqCst) != cmd_gen {
        return Ok(0);
    }

    let open = match with_current_db(db, generation, cmd_gen, |d| -> Result<_> {
        let open = d.get_open_intents()?;
        d.audit(
            AuditEvent::ReconcileStarted,
            &json!({"open_count": open.len()}),
        )?;
        Ok(open)
    }) {
        Some(r) => r?,
        None => return Ok(0),
    };

    let mut resolved = 0usize;

    macro_rules! guarded {
        ($f:expr) => {
            match with_current_db(db, generation, cmd_gen, $f) {
                Some(r) => r,
                None => return Ok(resolved),
            }
        };
    }

    for intent in open {
        if intent.status == IntentStatus::Created {
            guarded!(|d| d.mark_terminal(
                intent.id,
                IntentStatus::Failed,
                Some("abandoned before signing"),
            ))?;
            resolved += 1;
            continue;
        }

        let Some(sig) = intent.signature.clone() else {
            guarded!(|d| d.mark_terminal(
                intent.id,
                IntentStatus::Failed,
                Some("signed intent missing signature"),
            ))?;
            resolved += 1;
            continue;
        };
        let lvbh = intent.last_valid_block_height.unwrap_or(0);

        let status = rpc
            .get_signature_statuses(std::slice::from_ref(&sig), true)
            .await?
            .into_iter()
            .next()
            .flatten();
        let height = rpc.get_block_height().await?;

        match decide(status.as_ref(), height, lvbh) {
            Decision::Confirm => {
                guarded!(|d| d.mark_terminal(intent.id, IntentStatus::Confirmed, None))?;
                resolved += 1;
            }
            Decision::Fail(reason) => {
                guarded!(|d| d.mark_terminal(intent.id, IntentStatus::Failed, Some(&reason)))?;
                resolved += 1;
            }
            Decision::Expire => {
                let recheck = rpc
                    .get_signature_statuses(std::slice::from_ref(&sig), true)
                    .await?
                    .into_iter()
                    .next()
                    .flatten();
                if let Some(s2) = recheck {
                    if s2.is_error() {
                        guarded!(|d| d.mark_terminal(
                            intent.id,
                            IntentStatus::Failed,
                            Some("on-chain error"),
                        ))?;
                        resolved += 1;
                        continue;
                    }
                    if s2.is_confirmed_or_finalized() {
                        guarded!(|d| d.mark_terminal(intent.id, IntentStatus::Confirmed, None))?;
                        resolved += 1;
                        continue;
                    }
                }
                guarded!(|d| d.mark_terminal(
                    intent.id,
                    IntentStatus::Expired,
                    Some("blockhash expired before confirmation"),
                ))?;
                resolved += 1;
            }
            Decision::Rebroadcast => {
                let Some(bytes) = intent.signed_tx.clone() else {
                    guarded!(|d| d.mark_terminal(
                        intent.id,
                        IntentStatus::Failed,
                        Some("signed intent missing wire bytes"),
                    ))?;
                    resolved += 1;
                    continue;
                };
                match rpc.send_transaction(&bytes).await {
                    Ok(returned) if returned != sig => {
                        guarded!(|d| -> Result<()> {
                            d.audit(
                                AuditEvent::IntegrityCheckFailed,
                                &json!({"intent": intent.id, "expected_sig": sig, "got": returned}),
                            )?;
                            d.mark_terminal(
                                intent.id,
                                IntentStatus::Failed,
                                Some("rpc returned mismatched signature"),
                            )?;
                            Ok(())
                        })?;
                        resolved += 1;
                    }
                    Ok(_) => {
                        if intent.status == IntentStatus::Signed {
                            guarded!(|d| d.mark_submitted(intent.id))?;
                            resolved += 1;
                        }
                    }
                    Err(_) => {}
                }
            }
        }
    }

    if let Some(r) = with_current_db(db, generation, cmd_gen, |d| {
        d.audit(
            AuditEvent::ReconcileResolved,
            &json!({"resolved": resolved}),
        )
    }) {
        r?;
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    #[tokio::test]
    async fn reconcile_skips_when_generation_changed() {
        let db = Mutex::new(Db::open_memory().unwrap());
        {
            let mut d = db.lock().unwrap();
            d.insert_wallet(
                0,
                crate::types::Role::Master,
                "M1111111111111111111111111111111111111111111",
                None,
            )
            .unwrap();
            let i = d
                .create_intent(
                    1,
                    "Dest1111111111111111111111111111111111111111",
                    1000,
                    None,
                )
                .unwrap();
            d.mark_signed(i.id, "Sig", "bh", 100, 5000, b"x").unwrap();
        }
        let rpc = Rpc::new(reqwest::Client::new(), "http://127.0.0.1:0");
        let generation = AtomicU64::new(7);
        let resolved = reconcile_boot(&db, &rpc, &generation, 6).await.unwrap();
        assert_eq!(resolved, 0);
        assert_eq!(db.lock().unwrap().get_open_intents().unwrap().len(), 1);
    }

    fn status(err: bool, conf: Option<&str>) -> SignatureStatus {
        SignatureStatus {
            slot: 1,
            confirmations: None,
            err: if err { Some(json!("boom")) } else { None },
            confirmation_status: conf.map(String::from),
        }
    }

    #[test]
    fn confirmed_status_confirms() {
        assert_eq!(
            decide(Some(&status(false, Some("confirmed"))), 0, 0),
            Decision::Confirm
        );
        assert_eq!(
            decide(Some(&status(false, Some("finalized"))), 0, 0),
            Decision::Confirm
        );
    }

    #[test]
    fn on_chain_error_fails() {
        match decide(Some(&status(true, Some("confirmed"))), 0, 0) {
            Decision::Fail(_) => {}
            d => panic!("expected Fail, got {d:?}"),
        }
    }

    #[test]
    fn processed_is_in_flight_rebroadcast() {
        assert_eq!(
            decide(Some(&status(false, Some("processed"))), 0, 0),
            Decision::Rebroadcast
        );
    }

    #[test]
    fn unknown_within_window_rebroadcasts() {
        assert_eq!(decide(None, 1000, 1000), Decision::Rebroadcast);
        assert_eq!(
            decide(None, 1000 + EXPIRY_SLACK, 1000),
            Decision::Rebroadcast
        );
    }

    #[test]
    fn unknown_past_window_is_expire_candidate() {
        assert_eq!(
            decide(None, 1000 + EXPIRY_SLACK + 1, 1000),
            Decision::Expire
        );
    }
}
