use anyhow::Result;
use serde_json::json;

use super::rpc::{Rpc, SignatureStatus};
use crate::db::{IntentTransitionOutcome, Storage};
use crate::types::{AuditEvent, IntentStatus};

pub const EXPIRY_SLACK: u64 = 150;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Decision {
    FinalizeSuccess,
    WaitFinality,
    Fail(String),
    Expire,
    Rebroadcast,
}

fn decide(status: Option<&SignatureStatus>, current_height: u64, lvbh: u64) -> Decision {
    if let Some(st) = status {
        if st.is_error() {
            return Decision::Fail("on-chain error".to_string());
        }
        if st.is_finalized() {
            return Decision::FinalizeSuccess;
        }
        if st.is_confirmed() {
            return Decision::WaitFinality;
        }
        if current_height > lvbh + EXPIRY_SLACK {
            return Decision::Expire;
        }
        return Decision::Rebroadcast;
    }
    if current_height > lvbh + EXPIRY_SLACK {
        Decision::Expire
    } else {
        Decision::Rebroadcast
    }
}

fn applied(outcome: IntentTransitionOutcome) -> usize {
    usize::from(matches!(outcome, IntentTransitionOutcome::Applied))
}

pub async fn reconcile_boot(
    db: &Storage,
    rpc: &Rpc,
    generation: &std::sync::atomic::AtomicU64,
    cmd_gen: u64,
) -> Result<usize> {
    use std::sync::atomic::Ordering;
    if generation.load(Ordering::SeqCst) != cmd_gen {
        return Ok(0);
    }

    let open = match db.with_current(generation, cmd_gen, |d| -> Result<_> {
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
    let mut deferred = 0usize;

    macro_rules! guarded {
        ($f:expr) => {
            match db.with_current(generation, cmd_gen, $f) {
                Some(r) => r,
                None => return Ok(resolved),
            }
        };
    }

    macro_rules! terminal {
        ($id:expr, $status:expr, $error:expr $(,)?) => {{
            resolved += applied(guarded!(|d| d.mark_terminal($id, $status, $error))?);
        }};
    }

    for intent in open {
        if intent.status == IntentStatus::Created {
            terminal!(
                intent.id,
                IntentStatus::Failed,
                Some("abandoned before signing"),
            );
            continue;
        }

        let Some(sig) = intent.signature else {
            terminal!(
                intent.id,
                IntentStatus::Failed,
                Some("signed intent missing signature"),
            );
            continue;
        };
        let lvbh = intent.last_valid_block_height.unwrap_or(0);

        let status = match rpc.get_signature_statuses(&[sig.as_str()], true).await {
            Ok(statuses) => statuses.into_iter().next().flatten(),
            Err(_) => {
                deferred += 1;
                continue;
            }
        };
        let height = match rpc.get_block_height().await {
            Ok(height) => height,
            Err(_) => {
                deferred += 1;
                continue;
            }
        };

        match decide(status.as_ref(), height, lvbh) {
            Decision::FinalizeSuccess => {
                terminal!(intent.id, IntentStatus::Confirmed, None);
            }
            Decision::WaitFinality => {}
            Decision::Fail(reason) => {
                terminal!(intent.id, IntentStatus::Failed, Some(&reason));
            }
            Decision::Expire => {
                let recheck = match rpc.get_signature_statuses(&[sig.as_str()], true).await {
                    Ok(statuses) => statuses.into_iter().next().flatten(),
                    Err(_) => {
                        deferred += 1;
                        continue;
                    }
                };
                if let Some(s2) = recheck {
                    if s2.is_error() {
                        terminal!(intent.id, IntentStatus::Failed, Some("on-chain error"));
                        continue;
                    }
                    if s2.is_finalized() {
                        terminal!(intent.id, IntentStatus::Confirmed, None);
                        continue;
                    }
                    if s2.is_confirmed() {
                        continue;
                    }
                }
                terminal!(
                    intent.id,
                    IntentStatus::Expired,
                    Some("blockhash expired before confirmation"),
                );
            }
            Decision::Rebroadcast => {
                let Some(bytes) = intent.signed_tx else {
                    terminal!(
                        intent.id,
                        IntentStatus::Failed,
                        Some("signed intent missing wire bytes"),
                    );
                    continue;
                };
                match rpc.send_transaction(&bytes).await {
                    Ok(returned) if returned != sig => {
                        let outcome = guarded!(|d| -> Result<_> {
                            d.audit(
                                AuditEvent::IntegrityCheckFailed,
                                &json!({"intent": intent.id, "expected_sig": sig, "got": returned}),
                            )?;
                            Ok(d.mark_terminal(
                                intent.id,
                                IntentStatus::Failed,
                                Some("rpc returned mismatched signature"),
                            )?)
                        })?;
                        resolved += applied(outcome);
                    }
                    Ok(_) => {
                        if intent.status == IntentStatus::Signed {
                            resolved += applied(guarded!(|d| d.mark_submitted(intent.id))?);
                        }
                    }
                    Err(_) => {}
                }
            }
        }
    }

    if let Some(r) = db.with_current(generation, cmd_gen, |d| {
        d.audit(
            AuditEvent::ReconcileResolved,
            &json!({"resolved": resolved, "deferred": deferred}),
        )
    }) {
        r?;
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::types::Role;
    use serde_json::Value;
    use std::collections::VecDeque;
    use std::io::{Read as _, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::AtomicU64;
    use std::sync::{Arc, Mutex};

    struct MockServer {
        url: String,
        requests: Arc<Mutex<Vec<Value>>>,
        _worker: std::thread::JoinHandle<()>,
    }

    impl MockServer {
        fn new(results: Vec<Value>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let requests = Arc::new(Mutex::new(Vec::new()));
            let requests_for_thread = requests.clone();
            let responses = Arc::new(Mutex::new(VecDeque::from(results)));
            let responses_for_thread = responses.clone();
            let worker = std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let mut stream = stream.unwrap();
                    let request = read_request(&mut stream);
                    let body = request.split("\r\n\r\n").nth(1).unwrap_or("");
                    requests_for_thread
                        .lock()
                        .unwrap()
                        .push(serde_json::from_str(body).unwrap());
                    let result = responses_for_thread
                        .lock()
                        .unwrap()
                        .pop_front()
                        .unwrap_or_else(|| json!({"unexpected": true}));
                    let done = responses_for_thread.lock().unwrap().is_empty();
                    let envelope = if result.get("jsonrpc").is_some() {
                        result.to_string()
                    } else {
                        json!({"jsonrpc":"2.0","id":1,"result":result}).to_string()
                    };
                    write_response(&mut stream, &envelope);
                    if done {
                        break;
                    }
                }
            });
            MockServer {
                url,
                requests,
                _worker: worker,
            }
        }

        fn rpc(&self) -> Rpc {
            Rpc::new(reqwest::Client::new(), self.url.clone())
        }

        fn methods(&self) -> Vec<String> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .map(|v| v["method"].as_str().unwrap().to_string())
                .collect()
        }
    }

    fn read_request(stream: &mut TcpStream) -> String {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        loop {
            let n = stream.read(&mut tmp).unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&buf[..end]);
                let len = content_length(&headers);
                if buf.len().saturating_sub(end + 4) >= len {
                    break;
                }
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    fn content_length(headers: &str) -> usize {
        headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0)
    }

    fn write_response(stream: &mut TcpStream, body: &str) {
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(body.as_bytes()).unwrap();
    }

    fn db_with_intent() -> (Storage, i64) {
        let db = Storage::new(Db::open_memory().unwrap());
        let id = db.with_mut(|d| {
            let w = d
                .insert_wallet(
                    0,
                    Role::Master,
                    "M1111111111111111111111111111111111111111111",
                    None,
                )
                .unwrap();
            d.create_intent(
                w.id,
                "Dest1111111111111111111111111111111111111111",
                1000,
                None,
            )
            .unwrap()
            .id
        });
        (db, id)
    }

    fn db_with_two_intents() -> (Storage, i64, i64) {
        let db = Storage::new(Db::open_memory().unwrap());
        let (id1, id2) = db.with_mut(|d| {
            let m = d
                .insert_wallet(
                    0,
                    Role::Master,
                    "M1111111111111111111111111111111111111111111",
                    None,
                )
                .unwrap();
            let s = d
                .insert_wallet(
                    1,
                    Role::Sub,
                    "S2222222222222222222222222222222222222222222",
                    None,
                )
                .unwrap();
            let i1 = d
                .create_intent(
                    m.id,
                    "Dest1111111111111111111111111111111111111111",
                    1000,
                    None,
                )
                .unwrap()
                .id;
            let i2 = d
                .create_intent(
                    s.id,
                    "Dest2222222222222222222222222222222222222222",
                    1000,
                    None,
                )
                .unwrap()
                .id;
            (i1, i2)
        });
        (db, id1, id2)
    }

    fn rpc_error() -> Value {
        json!({"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"boom"}})
    }

    fn signed(db: &Storage, id: i64, sig: &str, lvbh: u64) {
        db.with_mut(|d| d.mark_signed(id, sig, "bh", lvbh, 5000, b"wire").unwrap());
    }

    async fn run(db: &Storage, rpc: &Rpc) -> usize {
        let generation = AtomicU64::new(1);
        reconcile_boot(db, rpc, &generation, 1).await.unwrap()
    }

    fn intent(db: &Storage, id: i64) -> crate::types::Intent {
        db.with(|d| d.get_intent(id).unwrap().unwrap())
    }

    fn audit_events(db: &Storage) -> Vec<String> {
        db.with(|d| {
            assert!(d.verify_audit_chain().unwrap());
            d.list_audit(50)
                .unwrap()
                .into_iter()
                .rev()
                .map(|a| a.event_type)
                .collect()
        })
    }

    fn status_value(err: Value, confirmation_status: &str) -> Value {
        json!({"slot":1,"confirmations":null,"err":err,"confirmationStatus":confirmation_status})
    }

    #[tokio::test]
    async fn reconcile_skips_when_generation_changed() {
        let (db, id) = db_with_intent();
        signed(&db, id, "Sig", 100);
        let rpc = Rpc::new(reqwest::Client::new(), "http://127.0.0.1:0");
        let generation = AtomicU64::new(7);
        let resolved = reconcile_boot(&db, &rpc, &generation, 6).await.unwrap();
        assert_eq!(resolved, 0);
        assert_eq!(db.with(|d| d.get_open_intents().unwrap().len()), 1);
    }

    #[tokio::test]
    async fn created_intent_is_abandoned_without_rpc_calls() {
        let (db, id) = db_with_intent();
        let rpc = Rpc::new(reqwest::Client::new(), "http://127.0.0.1:0");
        assert_eq!(run(&db, &rpc).await, 1);
        let got = intent(&db, id);
        assert_eq!(got.status, IntentStatus::Failed);
        assert_eq!(got.error.as_deref(), Some("abandoned before signing"));
        assert!(audit_events(&db).contains(&"reconcile_resolved".to_string()));
    }

    #[tokio::test]
    async fn signed_intent_missing_wire_bytes_fails_after_status_probe() {
        let (db, id) = db_with_intent();
        signed(&db, id, "Sig", 1000);
        db.with_mut(|d| d.clear_signed_tx_for_test(id).unwrap());
        let server = MockServer::new(vec![
            json!({"context":{"slot":1},"value":[null]}),
            json!(1000),
        ]);
        assert_eq!(run(&db, &server.rpc()).await, 1);
        let got = intent(&db, id);
        assert_eq!(got.status, IntentStatus::Failed);
        assert_eq!(
            got.error.as_deref(),
            Some("signed intent missing wire bytes")
        );
        assert_eq!(
            server.methods(),
            vec!["getSignatureStatuses", "getBlockHeight"]
        );
    }

    #[tokio::test]
    async fn signed_unknown_unexpired_rebroadcast_success_marks_submitted() {
        let (db, id) = db_with_intent();
        signed(&db, id, "Sig", 1000);
        let server = MockServer::new(vec![
            json!({"context":{"slot":1},"value":[null]}),
            json!(1000),
            json!("Sig"),
        ]);
        assert_eq!(run(&db, &server.rpc()).await, 1);
        assert_eq!(intent(&db, id).status, IntentStatus::Submitted);
        assert_eq!(
            server.methods(),
            vec!["getSignatureStatuses", "getBlockHeight", "sendTransaction"]
        );
    }

    #[tokio::test]
    async fn submitted_confirmed_waits_for_finality() {
        let (db, id) = db_with_intent();
        signed(&db, id, "Sig", 1000);
        db.with_mut(|d| d.mark_submitted(id).unwrap());
        let server = MockServer::new(vec![
            json!({"context":{"slot":1},"value":[status_value(Value::Null, "confirmed")]}),
            json!(1000),
        ]);
        assert_eq!(run(&db, &server.rpc()).await, 0);
        assert_eq!(intent(&db, id).status, IntentStatus::Submitted);
    }

    #[tokio::test]
    async fn submitted_finalized_marks_confirmed() {
        let (db, id) = db_with_intent();
        signed(&db, id, "Sig", 1000);
        db.with_mut(|d| d.mark_submitted(id).unwrap());
        let server = MockServer::new(vec![
            json!({"context":{"slot":1},"value":[status_value(Value::Null, "finalized")]}),
            json!(1000),
        ]);
        assert_eq!(run(&db, &server.rpc()).await, 1);
        assert_eq!(intent(&db, id).status, IntentStatus::Confirmed);
    }

    #[tokio::test]
    async fn unknown_expired_after_recheck_marks_expired() {
        let (db, id) = db_with_intent();
        signed(&db, id, "Sig", 1000);
        let server = MockServer::new(vec![
            json!({"context":{"slot":1},"value":[null]}),
            json!(1151),
            json!({"context":{"slot":1},"value":[null]}),
        ]);
        assert_eq!(run(&db, &server.rpc()).await, 1);
        let got = intent(&db, id);
        assert_eq!(got.status, IntentStatus::Expired);
        assert_eq!(
            got.error.as_deref(),
            Some("blockhash expired before confirmation")
        );
        assert_eq!(
            server.methods(),
            vec![
                "getSignatureStatuses",
                "getBlockHeight",
                "getSignatureStatuses"
            ]
        );
    }

    #[tokio::test]
    async fn on_chain_error_fails_intent() {
        let (db, id) = db_with_intent();
        signed(&db, id, "Sig", 1000);
        let server = MockServer::new(vec![
            json!({"context":{"slot":1},"value":[status_value(json!({"InstructionError":[0,"Custom"]}), "confirmed")]}),
            json!(1000),
        ]);
        assert_eq!(run(&db, &server.rpc()).await, 1);
        let got = intent(&db, id);
        assert_eq!(got.status, IntentStatus::Failed);
        assert_eq!(got.error.as_deref(), Some("on-chain error"));
    }

    #[tokio::test]
    async fn rebroadcast_signature_mismatch_fails_and_audits_integrity_check() {
        let (db, id) = db_with_intent();
        signed(&db, id, "Sig", 1000);
        let server = MockServer::new(vec![
            json!({"context":{"slot":1},"value":[null]}),
            json!(1000),
            json!("OtherSig"),
        ]);
        assert_eq!(run(&db, &server.rpc()).await, 1);
        let got = intent(&db, id);
        assert_eq!(got.status, IntentStatus::Failed);
        assert_eq!(
            got.error.as_deref(),
            Some("rpc returned mismatched signature")
        );
        let events = audit_events(&db);
        assert!(events.contains(&"integrity_check_failed".to_string()));
        assert!(events.contains(&"intent_failed".to_string()));
    }

    #[tokio::test]
    async fn one_intent_status_probe_error_does_not_abort_the_batch() {
        let (db, id1, id2) = db_with_two_intents();
        signed(&db, id1, "Sig1", 1000);
        signed(&db, id2, "Sig2", 1000);
        let server = MockServer::new(vec![
            json!({"context":{"slot":1},"value":[status_value(Value::Null, "finalized")]}),
            json!(1000),
            rpc_error(),
        ]);
        assert_eq!(run(&db, &server.rpc()).await, 1);
        assert_eq!(intent(&db, id1).status, IntentStatus::Confirmed);
        assert_eq!(intent(&db, id2).status, IntentStatus::Signed);
        assert!(audit_events(&db).contains(&"reconcile_resolved".to_string()));
    }

    #[tokio::test]
    async fn one_intent_block_height_error_does_not_abort_the_batch() {
        let (db, id1, id2) = db_with_two_intents();
        signed(&db, id1, "Sig1", 1000);
        signed(&db, id2, "Sig2", 1000);
        let server = MockServer::new(vec![
            json!({"context":{"slot":1},"value":[null]}),
            rpc_error(),
            json!({"context":{"slot":1},"value":[status_value(Value::Null, "finalized")]}),
            json!(1000),
        ]);
        assert_eq!(run(&db, &server.rpc()).await, 1);
        assert_eq!(intent(&db, id1).status, IntentStatus::Signed);
        assert_eq!(intent(&db, id2).status, IntentStatus::Confirmed);
        assert!(audit_events(&db).contains(&"reconcile_resolved".to_string()));
    }

    #[tokio::test]
    async fn expire_recheck_error_does_not_abort_the_batch() {
        let (db, id1, id2) = db_with_two_intents();
        signed(&db, id1, "Sig1", 1000);
        signed(&db, id2, "Sig2", 1000);
        let server = MockServer::new(vec![
            json!({"context":{"slot":1},"value":[null]}),
            json!(1151),
            rpc_error(),
            json!({"context":{"slot":1},"value":[status_value(Value::Null, "finalized")]}),
            json!(1000),
        ]);
        assert_eq!(run(&db, &server.rpc()).await, 1);
        assert_eq!(intent(&db, id1).status, IntentStatus::Signed);
        assert_eq!(intent(&db, id2).status, IntentStatus::Confirmed);
        assert!(audit_events(&db).contains(&"reconcile_resolved".to_string()));
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
    fn confirmed_status_waits_and_finalized_confirms() {
        assert_eq!(
            decide(Some(&status(false, Some("confirmed"))), 0, 0),
            Decision::WaitFinality
        );
        assert_eq!(
            decide(Some(&status(false, Some("finalized"))), 0, 0),
            Decision::FinalizeSuccess
        );
    }

    #[test]
    fn on_chain_error_decision_fails() {
        match decide(Some(&status(true, Some("confirmed"))), 0, 0) {
            Decision::Fail(_) => {}
            d => panic!("expected Fail, got {d:?}"),
        }
    }

    #[test]
    fn processed_is_in_flight_until_expiry() {
        assert_eq!(
            decide(Some(&status(false, Some("processed"))), 0, 0),
            Decision::Rebroadcast
        );
        assert_eq!(
            decide(
                Some(&status(false, Some("processed"))),
                1000 + EXPIRY_SLACK + 1,
                1000,
            ),
            Decision::Expire
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
