#![forbid(unsafe_code)]
use opaal_runtime::workflow::{
    AuditArtifact, JournalChain, audit_journal, parse_execution_receipt,
};
use serde_json::{Value, json};

fn header(policy: Option<&str>) -> Value {
    let digest = format!("sha256:{}", "1".repeat(64));
    let mut header = json!({"plan_digest":digest,"accepted_plan_digest":digest,"authority_digest":digest,
        "project_digest":digest,"environment_digest":digest,"tool_lock_digest":digest,
        "child_environment_digest":digest,"started_at":"2026-10-11T00:00:00.000000000Z"});
    if let Some(policy) = policy {
        header["result_policy"] = json!(policy);
    }
    header
}

fn primary(class: &str, reason: Value) -> Value {
    json!({"class":class,"code":"EXECUTE_TEST","message":"metadata-only outcome",
        "status":null,"value_digest":null,"partial":false,"cancel_reason":reason})
}

fn terminal(primary: Value, assessment: Value) -> Value {
    json!({"finished_at":"2026-10-11T00:00:01.000000000Z","primary":primary,
        "cleanup":[],"complete":true,"assessment":assessment})
}

#[test]
fn current_complete_and_incomplete_evidence_preserves_independent_assessment() {
    for policy in ["execution", "require-true"] {
        for value in [true, false] {
            let (mut chain, mut bytes) =
                JournalChain::begin(&"1".repeat(32), header(Some(policy))).unwrap();
            let incomplete = audit_journal(&bytes).unwrap();
            assert_eq!(incomplete.value()["schema"], "opaal.audit.v4");
            assert_eq!(incomplete.value()["result_policy"], policy);
            assert_eq!(incomplete.value()["assessment"], Value::Null);
            assert!(!incomplete.is_complete());
            let assessment = if policy == "require-true" {
                json!({"kind":"bool","value":value})
            } else {
                Value::Null
            };
            bytes.extend(
                chain
                    .append(
                        "terminal",
                        terminal(primary("success", Value::Null), assessment.clone()),
                    )
                    .unwrap(),
            );
            let audit = audit_journal(&bytes).unwrap();
            assert!(audit.is_complete());
            assert_eq!(audit.value()["primary"]["class"], "success");
            assert_eq!(audit.value()["assessment"], assessment);
            assert_eq!(AuditArtifact::parse(audit.bytes()).unwrap(), audit);
            let mut forged = audit.value().clone();
            forged.as_object_mut().unwrap().remove("digest");
            forged["assessment"] = json!({"kind":"bool","value":!value});
            forged["digest"] = json!(opaal_runtime::workflow::digest_value(&forged).unwrap());
            assert!(AuditArtifact::parse(&serde_json::to_vec(&forged).unwrap()).is_err());
        }
    }
}

#[test]
fn cancellation_reason_is_unknown_only_in_incomplete_audit_without_terminal() {
    for reason in ["requested", "timeout"] {
        let (mut chain, mut bytes) =
            JournalChain::begin(&"2".repeat(32), header(Some("require-true"))).unwrap();
        bytes.extend(chain.append("action-start", json!({"action_node_id":format!("sha256:{}#000000", "2".repeat(64)),"action_id":"test::run","contract_digest":format!("sha256:{}","2".repeat(64))})).unwrap());
        let mut outcome = primary("cancelled", Value::Null);
        outcome.as_object_mut().unwrap().remove("cancel_reason");
        bytes.extend(
            chain
                .append(
                    "action-end",
                    json!({"action_node_id":format!("sha256:{}#000000", "2".repeat(64)),"outcome":outcome}),
                )
                .unwrap(),
        );
        let incomplete = audit_journal(&bytes).unwrap();
        assert_eq!(incomplete.value()["primary"]["class"], "cancelled");
        assert_eq!(incomplete.value()["primary"]["cancel_reason"], Value::Null);
        assert_eq!(incomplete.value()["assessment"], Value::Null);
        assert!(
            chain
                .append(
                    "terminal",
                    terminal(primary("cancelled", Value::Null), Value::Null)
                )
                .is_err()
        );
        bytes.extend(
            chain
                .append(
                    "terminal",
                    terminal(primary("cancelled", json!(reason)), Value::Null),
                )
                .unwrap(),
        );
        let complete = audit_journal(&bytes).unwrap();
        assert_eq!(complete.value()["primary"]["cancel_reason"], reason);
        let truncated = audit_journal(&bytes[..bytes.len() - 1]).unwrap();
        assert!(!truncated.is_complete());
        assert_eq!(truncated.value()["primary"]["class"], "cancelled");
        assert_eq!(truncated.value()["primary"]["cancel_reason"], Value::Null);
        assert_eq!(truncated.value()["assessment"], Value::Null);
    }
}

#[test]
fn current_policy_primary_and_assessment_are_closed_and_consistent() {
    assert!(JournalChain::begin(&"3".repeat(32), header(Some("truthy"))).is_err());
    for (policy, primary, assessment) in [
        (
            "execution",
            primary("success", Value::Null),
            json!({"kind":"bool","value":true}),
        ),
        ("require-true", primary("success", Value::Null), Value::Null),
        (
            "require-true",
            primary("error", Value::Null),
            json!({"kind":"bool","value":true}),
        ),
        (
            "require-true",
            primary("success", json!("requested")),
            json!({"kind":"bool","value":true}),
        ),
        (
            "require-true",
            primary("cancelled", json!("future")),
            Value::Null,
        ),
        (
            "require-true",
            primary("success", Value::Null),
            json!({"kind":"bool","value":"true"}),
        ),
        (
            "require-true",
            primary("success", Value::Null),
            json!({"kind":"bool","value":true,"extra":0}),
        ),
    ] {
        let (mut chain, _) = JournalChain::begin(&"3".repeat(32), header(Some(policy))).unwrap();
        assert!(
            chain
                .append("terminal", terminal(primary, assessment))
                .is_err()
        );
    }
}

fn receipt(version: u64) -> Vec<u8> {
    match version {
        1 => include_bytes!(
            "../../../tests/golden/native-ci-outcomes/format-versions/receipt-v1.json"
        )
        .to_vec(),
        2 => include_bytes!(
            "../../../tests/golden/native-ci-outcomes/format-versions/receipt-v2.json"
        )
        .to_vec(),
        _ => unreachable!("fixture versions are explicit"),
    }
}

#[test]
fn legacy_and_current_receipts_are_explicit_and_hostile_forms_refuse() {
    for (version, bytes) in [
        (
            2,
            include_bytes!(
                "../../../tests/golden/native-ci-outcomes/format-versions/journal-v2.jsonl"
            )
            .as_slice(),
        ),
        (
            3,
            include_bytes!(
                "../../../tests/golden/native-ci-outcomes/format-versions/journal-v3.jsonl"
            )
            .as_slice(),
        ),
        (
            4,
            include_bytes!(
                "../../../tests/golden/native-ci-outcomes/format-versions/journal-v4.jsonl"
            )
            .as_slice(),
        ),
    ] {
        let audit = audit_journal(bytes).unwrap();
        assert_eq!(audit.value()["schema_version"], version);
        assert!(audit.is_complete());
        assert_eq!(AuditArtifact::parse(audit.bytes()).unwrap(), audit);
    }
    for version in [1, 2] {
        let bytes = receipt(version);
        assert_eq!(
            parse_execution_receipt(&bytes).unwrap()["schema_version"],
            version
        );
        assert!(parse_execution_receipt(&bytes[..bytes.len() - 1]).is_err());
        let duplicate =
            String::from_utf8(bytes.clone())
                .unwrap()
                .replacen('{', "{\"schema_version\":0,", 1);
        assert!(parse_execution_receipt(duplicate.as_bytes()).is_err());
    }
    for (path, value) in [
        ("schema_version", json!(3)),
        ("result_policy", json!("truthy")),
        ("assessment", Value::Null),
        ("extra", json!(true)),
    ] {
        let mut document: Value = serde_json::from_slice(&receipt(2)).unwrap();
        document[path] = value;
        let mut bytes = serde_json::to_vec(&document).unwrap();
        bytes.push(b'\n');
        assert!(parse_execution_receipt(&bytes).is_err());
    }
    let mut missing_class: Value = serde_json::from_slice(&receipt(2)).unwrap();
    missing_class["primary"]
        .as_object_mut()
        .unwrap()
        .remove("class");
    let mut bytes = serde_json::to_vec(&missing_class).unwrap();
    bytes.push(b'\n');
    assert!(parse_execution_receipt(&bytes).is_err());
    for reason in [
        json!("requested"),
        json!("timeout"),
        Value::Null,
        json!("future"),
    ] {
        let mut cancelled: Value = serde_json::from_slice(&receipt(2)).unwrap();
        cancelled["primary"]["class"] = json!("cancelled");
        cancelled["primary"]["cancel_reason"] = reason.clone();
        cancelled["assessment"] = Value::Null;
        let mut bytes = serde_json::to_vec(&cancelled).unwrap();
        bytes.push(b'\n');
        assert_eq!(
            parse_execution_receipt(&bytes).is_ok(),
            reason == "requested" || reason == "timeout"
        );
    }
    let (mut chain, mut bytes) = JournalChain::begin(&"4".repeat(32), header(None)).unwrap();
    let mut payload = terminal(primary("success", Value::Null), Value::Null);
    payload.as_object_mut().unwrap().remove("assessment");
    payload["primary"]
        .as_object_mut()
        .unwrap()
        .remove("cancel_reason");
    bytes.extend(chain.append("terminal", payload).unwrap());
    assert_eq!(
        audit_journal(&bytes).unwrap().value()["schema"],
        "opaal.audit.v2"
    );
    let future = String::from_utf8(bytes)
        .unwrap()
        .replace("opaal.run-journal.v2", "opaal.run-journal.v5");
    assert!(audit_journal(future.as_bytes()).is_err());
}
