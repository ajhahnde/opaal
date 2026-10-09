#![forbid(unsafe_code)]

use opaal_runtime::workflow::{
    AuditArtifact, CheckArtifact, JOURNAL_TERMINAL_RESERVE_BYTES, JournalChain, MAX_JOURNAL_BYTES,
    PlanArtifact, audit_journal, digest_bytes, digest_value, native_path as encode_native_path,
    timestamp_from_unix_nanos,
};
use serde_json::{Value, json};
use std::path::Path;

fn digest(byte: char) -> String {
    format!("sha256:{}", byte.to_string().repeat(64))
}

fn native_path() -> Value {
    json!({"encoding":"base64url-nopad","platform":"unix","value":"Lw"})
}

fn outcome(class: &str, code: &str, message: impl Into<String>) -> Value {
    json!({
        "class": class,
        "code": code,
        "message": message.into(),
        "status": null,
        "value_digest": null,
        "partial": class == "partial"
    })
}

fn project() -> Value {
    json!({
        "name":"demo",
        "root":native_path(),
        "manifest_path":native_path(),
        "manifest_digest":digest('1'),
        "environment":"ci",
        "environment_digest":digest('2'),
        "tool_lock_digest":digest('3'),
        "child_environment_digest":digest('4'),
        "tls":[]
    })
}

fn task() -> Value {
    json!({
        "id":"demo::ready",
        "action_id":"/project/tasks.opaal::ready",
        "contract_digest":digest('5')
    })
}

fn authority() -> Value {
    json!({
        "path":native_path(),
        "digest":digest('6'),
        "rules":[],
        "requests":[]
    })
}

fn check_document() -> Value {
    json!({
        "schema":"opaal.check.v2",
        "schema_version":2,
        "toolchain":{"version":"1.0.0-alpha.1"},
        "project":project(),
        "task":task(),
        "inputs":[],
        "secrets":[],
        "sources":[],
        "authority":authority(),
        "tools":[],
        "findings":[],
        "outcome":outcome("success", "CHECK000", "check succeeded")
    })
}

fn plan_document() -> Value {
    let contract = digest('5');
    json!({
        "schema":"opaal.plan.v2",
        "schema_version":2,
        "created_at":"2026-09-09T08:00:00.000000000Z",
        "expires_at":"2026-09-09T08:15:00.000000000Z",
        "toolchain":{"version":"1.0.0-alpha.1"},
        "platform":{"triple":"aarch64-apple-darwin"},
        "project":project(),
        "task":task(),
        "inputs":[],
        "secrets":[],
        "sources":[],
        "authority":authority(),
        "tools":[],
        "observations":[
            {
                "kind":"manifest",
                "id":"demo",
                "path":native_path(),
                "digest":digest('1'),
                "size":0,
                "observed_at":null
            },
            {
                "kind":"authority",
                "id":"ci",
                "path":native_path(),
                "digest":digest('6'),
                "size":0,
                "observed_at":null
            },
            {
                "kind":"tool-lock",
                "id":"ci",
                "path":native_path(),
                "digest":digest('3'),
                "size":0,
                "observed_at":null
            },
            {
                "kind":"child-environment",
                "id":"ci",
                "path":null,
                "digest":digest('4'),
                "size":null,
                "observed_at":null
            },
            {
                "kind":"wall-clock",
                "id":"created-at",
                "path":null,
                "digest":null,
                "size":null,
                "observed_at":"2026-09-09T08:00:00.000000000Z"
            }
        ],
        "actions":[{
            "id":format!("{contract}#000000"),
            "ordinal":0,
            "action_id":"/project/tasks.opaal::ready",
            "contract_digest":contract,
            "requests":[],
            "dependencies":[],
            "outcome":outcome("success", "PLAN000", "action is executable")
        }],
        "outcome":outcome("success", "PLAN000", "plan is executable")
    })
}

fn header(plan_digest: &str) -> Value {
    json!({
        "plan_digest":plan_digest,
        "accepted_plan_digest":plan_digest,
        "authority_digest":digest('6'),
        "project_digest":digest('7'),
        "environment_digest":digest('2'),
        "tool_lock_digest":digest('3'),
        "child_environment_digest":digest('4'),
        "started_at":"2026-09-09T08:01:00.000000000Z"
    })
}

fn clock_operation(evidence_digest: Option<String>) -> Value {
    json!({
        "kind":"clock",
        "operation":"std::time::wall_now",
        "clock":"wall",
        "evidence_digest":evidence_digest
    })
}

fn process_operation(
    status: Option<u64>,
    stdout_bytes: Option<u64>,
    stderr_bytes: Option<u64>,
    evidence_digest: Option<String>,
) -> Value {
    json!({
        "kind":"process",
        "operation":"std::process::run",
        "tool":"git",
        "argv_count":1,
        "argv_bytes":4,
        "argv_digest":digest('7'),
        "argv":[{"kind":"redacted","bytes":4,"digest":digest('6')}],
        "status":status,
        "stdout_bytes":stdout_bytes,
        "stderr_bytes":stderr_bytes,
        "evidence_digest":evidence_digest
    })
}

fn effect_before(effect: &str, scope: Value, operation: Value) -> Value {
    json!({
        "action_node_id":format!("{}#000000", digest('5')),
        "effect":effect,
        "scope":scope,
        "operation":operation,
        "verdict":"granted-enforced",
        "attempt":0
    })
}

#[test]
fn check_and_plan_are_canonical_digest_bound_closed_artifacts() {
    let check = CheckArtifact::seal(check_document()).unwrap();
    assert_eq!(CheckArtifact::parse(check.bytes()).unwrap(), check);
    assert!(check.bytes().starts_with(b"{\"authority\":"));
    assert!(check.digest().starts_with("sha256:"));
    assert!(check.is_executable());

    let plan = PlanArtifact::seal(plan_document()).unwrap();
    assert_eq!(PlanArtifact::parse(plan.bytes()).unwrap(), plan);
    assert!(plan.is_executable());
    assert_eq!(plan.created_at(), "2026-09-09T08:00:00.000000000Z");
    assert_eq!(plan.expires_at(), "2026-09-09T08:15:00.000000000Z");

    let mut spaced = plan.bytes().to_vec();
    spaced.insert(1, b' ');
    assert_eq!(
        PlanArtifact::parse(&spaced).unwrap_err().code(),
        "ARTIFACT004"
    );

    let mut unknown = plan_document();
    unknown["extra"] = Value::Null;
    assert_eq!(
        PlanArtifact::seal(unknown).unwrap_err().code(),
        "ARTIFACT007"
    );
}

#[test]
fn v1_future_and_unbound_secret_artifacts_are_rejected() {
    let mut old_check = check_document();
    old_check["schema"] = Value::String("opaal.check.v1".to_owned());
    old_check["schema_version"] = Value::from(1_u64);
    assert_eq!(
        CheckArtifact::seal(old_check).unwrap_err().code(),
        "ARTIFACT006"
    );

    let mut old_plan = plan_document();
    old_plan["schema"] = Value::String("opaal.plan.v1".to_owned());
    old_plan["schema_version"] = Value::from(1_u64);
    assert_eq!(
        PlanArtifact::seal(old_plan).unwrap_err().code(),
        "ARTIFACT006"
    );

    let mut future_plan = plan_document();
    future_plan["schema"] = Value::String("opaal.plan.v4".to_owned());
    future_plan["schema_version"] = Value::from(4_u64);
    assert_eq!(
        PlanArtifact::seal(future_plan).unwrap_err().code(),
        "ARTIFACT006"
    );

    let (chain, journal) =
        JournalChain::begin("00000000000000000000000000000009", header(&digest('5'))).unwrap();
    assert!(!chain.is_terminal());
    for unsupported in ["opaal.run-journal.v1", "opaal.run-journal.v4"] {
        let bytes = String::from_utf8(journal.clone())
            .unwrap()
            .replace("opaal.run-journal.v2", unsupported)
            .into_bytes();
        assert_eq!(audit_journal(&bytes).unwrap_err().code(), "JOURNAL003");
    }

    let audit = audit_journal(&journal).unwrap();
    for unsupported in ["opaal.audit.v1", "opaal.audit.v4"] {
        let bytes = String::from_utf8(audit.bytes().to_vec())
            .unwrap()
            .replace("opaal.audit.v2", unsupported)
            .into_bytes();
        assert_eq!(
            AuditArtifact::parse(&bytes).unwrap_err().code(),
            "ARTIFACT006"
        );
    }

    let mut unbound = check_document();
    unbound["secrets"] = json!([{
        "id":"token",
        "endpoint":"service",
        "header":"authorization"
    }]);
    assert_eq!(
        CheckArtifact::seal(unbound).unwrap_err().code(),
        "ARTIFACT007"
    );
}

#[test]
fn canonical_json_orders_object_names_by_utf16_code_units() {
    let value = json!({
        "\u{e000}": 1,
        "\u{10000}": 0
    });
    let expected = "{\"\u{10000}\":0,\"\u{e000}\":1}".to_owned();
    assert_eq!(
        digest_value(&value).unwrap(),
        digest_bytes(expected.as_bytes())
    );
}

#[test]
fn macos_process_requests_are_valid_only_as_unsupported_refusals() {
    let mut check = check_document();
    let scope = json!({"kind":"tool","tool":"git"});
    check["authority"] = json!({
        "path":native_path(),
        "digest":digest('6'),
        "rules":[{
            "decision":"grant",
            "effect":"process.run",
            "scope":scope,
            "required_enforcement":"acknowledged-unenforced"
        }],
        "requests":[{
            "effect":"process.run",
            "scope":scope,
            "verdict":"unsupported"
        }]
    });
    check["tools"] = json!([{
        "id":"git",
        "adapter":"git",
        "path":native_path(),
        "required_version":">=2.39.0,<3.0.0",
        "locked_version":"2.50.0",
        "digest":digest('8'),
        "platform":"aarch64-apple-darwin",
        "child_environment_digest":digest('4'),
        "version_verified":false
    }]);
    check["findings"] = json!([{
        "severity":"error",
        "code":"CHECK008",
        "message":"maintained process execution is unsupported on this platform",
        "source":null,
        "path":null,
        "start_byte":null,
        "end_byte":null
    }]);
    check["outcome"] = outcome("refused", "CHECK005", "authority check refused execution");

    let sealed = CheckArtifact::seal(check.clone()).unwrap();
    assert!(!sealed.is_executable());

    check["authority"]["requests"][0]["verdict"] = Value::String("granted-unenforced".into());
    assert_eq!(
        CheckArtifact::seal(check).unwrap_err().code(),
        "ARTIFACT007"
    );
}

#[test]
fn unknown_is_an_admitted_but_non_executable_authority_verdict() {
    let scope = json!({"kind":"tool","tool":"git"});
    let rule = json!({
        "decision":"grant",
        "effect":"process.run",
        "scope":scope,
        "required_enforcement":"acknowledged-unenforced"
    });
    let request = json!({
        "effect":"process.run",
        "scope":scope,
        "verdict":"unknown"
    });
    let mut check = check_document();
    check["authority"]["rules"] = json!([rule]);
    check["authority"]["requests"] = json!([request]);
    check["tools"] = json!([{
        "id":"git",
        "adapter":"git",
        "path":native_path(),
        "required_version":">=2.39.0,<3.0.0",
        "locked_version":"2.50.0",
        "digest":digest('8'),
        "platform":"future-unknown-platform",
        "child_environment_digest":digest('4'),
        "version_verified":false
    }]);
    check["findings"] = json!([{
        "severity":"error",
        "code":"CHECK009",
        "message":"maintained process enforcement is unknown on this platform",
        "source":null,
        "path":null,
        "start_byte":null,
        "end_byte":null
    }]);
    check["outcome"] = outcome("refused", "CHECK005", "authority enforcement is unknown");

    let sealed = CheckArtifact::seal(check.clone()).unwrap();
    assert!(!sealed.is_executable());

    check["outcome"] = outcome("success", "CHECK000", "project task check succeeded");
    assert_eq!(
        CheckArtifact::seal(check).unwrap_err().code(),
        "ARTIFACT007"
    );
}

#[test]
fn artifact_identities_require_nfc_and_action_nodes_do_not_depend_on_plan_digest() {
    let mut check = check_document();
    check["project"]["name"] = Value::String("e\u{301}".to_owned());
    assert_eq!(
        CheckArtifact::seal(check).unwrap_err().code(),
        "ARTIFACT007"
    );

    let mut plan = plan_document();
    plan["actions"][0]["id"] = Value::String(format!("{}#000001", digest('5')));
    assert_eq!(PlanArtifact::seal(plan).unwrap_err().code(), "ARTIFACT007");
}

#[test]
fn artifact_cross_fields_refuse_inconsistent_outcomes_and_observations() {
    let mut check = check_document();
    check["findings"] = json!([{
        "severity":"error",
        "code":"CHECK005",
        "message":"request is denied",
        "source":null,
        "path":null,
        "start_byte":null,
        "end_byte":null
    }]);
    assert_eq!(
        CheckArtifact::seal(check).unwrap_err().code(),
        "ARTIFACT007"
    );

    let mut plan = plan_document();
    plan["observations"][2]["digest"] = Value::String(digest('9'));
    assert_eq!(PlanArtifact::seal(plan).unwrap_err().code(), "ARTIFACT007");

    let mut plan = plan_document();
    plan["actions"][0]["outcome"] = outcome("refused", "PLAN001", "not executable");
    assert_eq!(PlanArtifact::seal(plan).unwrap_err().code(), "ARTIFACT007");

    let mut check = check_document();
    check["inputs"] = json!([{
        "name":"count",
        "type":"Int",
        "binding":"value",
        "value":"1",
        "path":native_path(),
        "digest":digest('1'),
        "size":1
    }]);
    assert_eq!(
        CheckArtifact::seal(check).unwrap_err().code(),
        "ARTIFACT007"
    );

    let mut plan = plan_document();
    plan["observations"][0]["size"] = Value::Null;
    assert_eq!(PlanArtifact::seal(plan).unwrap_err().code(), "ARTIFACT007");
}

#[test]
fn journal_descriptors_are_scope_bound_and_redaction_closed() {
    let mut mismatched_header = header(&digest('5'));
    mismatched_header["accepted_plan_digest"] = Value::String(digest('6'));
    assert_eq!(
        JournalChain::begin("00000000000000000000000000000008", mismatched_header)
            .unwrap_err()
            .code(),
        "JOURNAL003"
    );

    let new_chain = || {
        JournalChain::begin("00000000000000000000000000000008", header(&digest('5')))
            .unwrap()
            .0
    };

    let mut wrong_tool = process_operation(None, None, None, None);
    wrong_tool["tool"] = Value::String("cargo".to_owned());
    assert_eq!(
        new_chain()
            .append(
                "effect-before",
                effect_before(
                    "process.run",
                    json!({"kind":"tool","tool":"git"}),
                    wrong_tool,
                ),
            )
            .unwrap_err()
            .code(),
        "JOURNAL003"
    );

    let mut exposed_source_argument = process_operation(None, None, None, None);
    exposed_source_argument["argv"] = json!([{"kind":"public","value":"canary"}]);
    exposed_source_argument["argv_bytes"] = Value::from(6_u64);
    assert_eq!(
        new_chain()
            .append(
                "effect-before",
                effect_before(
                    "process.run",
                    json!({"kind":"tool","tool":"git"}),
                    exposed_source_argument,
                ),
            )
            .unwrap_err()
            .code(),
        "JOURNAL003"
    );

    let hostile_probe = json!({
        "kind":"process",
        "operation":"std::process::probe",
        "tool":"git",
        "argv_count":2,
        "argv_bytes":10,
        "argv_digest":digest('7'),
        "argv":[
            {"kind":"redacted","bytes":4,"digest":digest('6')},
            {"kind":"public","value":"canary"}
        ],
        "status":null,
        "stdout_bytes":null,
        "stderr_bytes":null,
        "evidence_digest":null
    });
    assert_eq!(
        new_chain()
            .append(
                "effect-before",
                effect_before(
                    "process.run",
                    json!({"kind":"tool","tool":"git"}),
                    hostile_probe,
                ),
            )
            .unwrap_err()
            .code(),
        "JOURNAL003"
    );

    let wrong_http_endpoint = json!({
        "kind":"http",
        "operation":"std::http::request",
        "endpoint":"other",
        "method":"GET",
        "status":null,
        "body_bytes":null,
        "evidence_digest":null
    });
    assert_eq!(
        new_chain()
            .append(
                "effect-before",
                effect_before(
                    "network.http",
                    json!({"kind":"endpoint","endpoint":"service","method":"GET"}),
                    wrong_http_endpoint,
                ),
            )
            .unwrap_err()
            .code(),
        "JOURNAL003"
    );

    let wrong_secret_header = json!({
        "kind":"secret-reveal",
        "operation":"std::http::secret_reveal",
        "endpoint":"service",
        "header":"x-other",
        "evidence_digest":null
    });
    assert_eq!(
        new_chain()
            .append(
                "effect-before",
                effect_before(
                    "secret.reveal",
                    json!({
                        "kind":"secret-sink",
                        "secret":"token",
                        "endpoint":"service",
                        "header":"authorization"
                    }),
                    wrong_secret_header,
                ),
            )
            .unwrap_err()
            .code(),
        "JOURNAL003"
    );
}

#[test]
fn journal_after_requires_matching_identity_evidence_and_success_metadata() {
    let new_chain = || {
        JournalChain::begin("0000000000000000000000000000000a", header(&digest('5')))
            .unwrap()
            .0
    };
    let scope = json!({"kind":"tool","tool":"git"});
    let before = process_operation(None, None, None, None);
    let assert_after_refused = |after: Value, evidence_digest: String| {
        let mut chain = new_chain();
        chain
            .append(
                "effect-before",
                effect_before("process.run", scope.clone(), before.clone()),
            )
            .unwrap();
        let error = chain
            .append(
                "effect-after",
                json!({
                    "action_node_id":format!("{}#000000", digest('5')),
                    "effect":"process.run",
                    "scope":scope,
                    "operation":after,
                    "attempt":0,
                    "outcome":outcome("success", "EFFECT000", "effect completed"),
                    "evidence_digest":evidence_digest
                }),
            )
            .unwrap_err();
        assert_eq!(error.code(), "JOURNAL003");
    };

    let mut mismatched_identity = process_operation(Some(0), Some(12), Some(0), Some(digest('8')));
    mismatched_identity["argv_digest"] = Value::String(digest('9'));
    assert_after_refused(mismatched_identity, digest('8'));
    assert_after_refused(
        process_operation(Some(0), None, Some(0), Some(digest('8'))),
        digest('8'),
    );
    assert_after_refused(
        process_operation(Some(0), Some(12), Some(0), Some(digest('9'))),
        digest('8'),
    );
}

#[test]
fn journal_chain_projects_complete_redacted_evidence() {
    let plan = PlanArtifact::seal(plan_document()).unwrap();
    let run_id = "00000000000000000000000000000001";
    let (mut chain, first) = JournalChain::begin(run_id, header(plan.digest())).unwrap();
    let action_node = format!("{}#000000", digest('5'));
    let scope = json!({"kind":"clock","clock":"wall"});
    let mut journal = first;
    journal.extend(
        chain
            .append(
                "action-start",
                json!({
                    "action_node_id":action_node,
                    "action_id":"/project/tasks.opaal::ready",
                    "contract_digest":digest('5')
                }),
            )
            .unwrap(),
    );
    journal.extend(
        chain
            .append(
                "effect-before",
                json!({
                    "action_node_id":action_node,
                    "effect":"clock.wall",
                    "scope":scope,
                    "operation":clock_operation(None),
                    "verdict":"granted-enforced",
                    "attempt":0
                }),
            )
            .unwrap(),
    );
    journal.extend(
        chain
            .append(
                "effect-after",
                json!({
                    "action_node_id":action_node,
                    "effect":"clock.wall",
                    "scope":scope,
                    "operation":clock_operation(Some(digest('8'))),
                    "attempt":0,
                    "outcome":outcome("success", "EFFECT000", "effect completed"),
                    "evidence_digest":digest('8')
                }),
            )
            .unwrap(),
    );
    journal.extend(
        chain
            .append(
                "action-end",
                json!({
                    "action_node_id":action_node,
                    "outcome":outcome("success", "ACTION000", "action completed")
                }),
            )
            .unwrap(),
    );
    journal.extend(
        chain
            .append(
                "terminal",
                json!({
                    "finished_at":"2026-09-09T08:02:00.000000000Z",
                    "primary":outcome("success", "RUN000", "run completed"),
                    "cleanup":[],
                    "complete":true
                }),
            )
            .unwrap(),
    );

    assert!(chain.is_terminal());
    let audit = audit_journal(&journal).unwrap();
    assert!(audit.is_complete());
    assert_eq!(AuditArtifact::parse(audit.bytes()).unwrap(), audit);
    assert_eq!(audit.value()["run_id"], run_id);
    assert_eq!(audit.value()["events"].as_array().unwrap().len(), 5);
    assert_eq!(audit.value()["primary"]["code"], "RUN000");
    assert_eq!(audit.value()["schema"], "opaal.audit.v2");
    assert_eq!(
        audit.value()["started_at"],
        "2026-09-09T08:01:00.000000000Z"
    );
    assert_eq!(
        audit.value()["finished_at"],
        "2026-09-09T08:02:00.000000000Z"
    );
    assert_eq!(audit.value()["validated_line_count"], 6);
    assert_eq!(
        audit.value()["journal_terminal_digest"],
        audit.value()["validated_prefix_digest"]
    );
}

#[test]
fn truncated_last_line_is_incomplete_but_earlier_corruption_refuses() {
    let plan = PlanArtifact::seal(plan_document()).unwrap();
    let (chain, mut journal) =
        JournalChain::begin("00000000000000000000000000000002", header(plan.digest())).unwrap();
    assert!(!chain.is_terminal());
    journal.extend_from_slice(b"{\"truncated\":");
    let audit = audit_journal(&journal).unwrap();
    assert!(!audit.is_complete());
    assert!(audit.value()["events"].as_array().unwrap().is_empty());

    let digest_position = journal[..journal.len() - 13]
        .windows(7)
        .position(|window| window == b"sha256:")
        .unwrap()
        + 7;
    journal[digest_position] = if journal[digest_position] == b'f' {
        b'e'
    } else {
        b'f'
    };
    assert_eq!(audit_journal(&journal).unwrap_err().code(), "JOURNAL003");

    let (_, mut journal) =
        JournalChain::begin("00000000000000000000000000000005", header(plan.digest())).unwrap();
    journal.extend_from_slice(b"\n");
    assert_eq!(audit_journal(&journal).unwrap_err().code(), "JOURNAL003");
}

#[test]
fn nonterminal_events_cannot_consume_terminal_reserve() {
    let plan = PlanArtifact::seal(plan_document()).unwrap();
    let (mut chain, _) =
        JournalChain::begin("00000000000000000000000000000003", header(plan.digest())).unwrap();
    let oversized = "x".repeat(MAX_JOURNAL_BYTES - JOURNAL_TERMINAL_RESERVE_BYTES);
    let error = chain
        .append(
            "action-end",
            json!({
                "action_node_id":format!("{}#000000", digest('5')),
                "outcome":outcome("error", "ACTION001", oversized)
            }),
        )
        .unwrap_err();
    assert_eq!(error.code(), "JOURNAL001");
    assert_eq!(chain.lines(), 1);
}

#[test]
fn journal_lifecycle_refuses_bad_attempts_nodes_and_open_boundaries() {
    let plan = PlanArtifact::seal(plan_document()).unwrap();
    let (mut chain, _) =
        JournalChain::begin("00000000000000000000000000000004", header(plan.digest())).unwrap();
    let action_node = format!("{}#000000", digest('5'));
    let scope = json!({"kind":"tool","tool":"git"});

    let bad_node = chain
        .append(
            "action-start",
            json!({
                "action_node_id":format!("{}#0", digest('5')),
                "action_id":"/project/tasks.opaal::ready",
                "contract_digest":digest('5')
            }),
        )
        .unwrap_err();
    assert_eq!(bad_node.code(), "JOURNAL003");
    assert_eq!(chain.lines(), 1);

    let bad_attempt = chain
        .append(
            "effect-before",
            json!({
                "action_node_id":action_node,
                "effect":"process.run",
                "scope":scope,
                "operation":process_operation(None, None, None, None),
                "verdict":"granted-unenforced",
                "attempt":1
            }),
        )
        .unwrap_err();
    assert_eq!(bad_attempt.code(), "JOURNAL003");
    assert_eq!(chain.lines(), 1);

    let bad_scope = chain
        .append(
            "effect-before",
            json!({
                "action_node_id":action_node,
                "effect":"clock.wall",
                "scope":scope,
                "operation":process_operation(None, None, None, None),
                "verdict":"granted-enforced",
                "attempt":0
            }),
        )
        .unwrap_err();
    assert_eq!(bad_scope.code(), "JOURNAL003");
    assert_eq!(chain.lines(), 1);

    let refused_attempt = chain
        .append(
            "effect-before",
            json!({
                "action_node_id":action_node,
                "effect":"process.run",
                "scope":scope,
                "operation":process_operation(None, None, None, None),
                "verdict":"denied",
                "attempt":0
            }),
        )
        .unwrap_err();
    assert_eq!(refused_attempt.code(), "JOURNAL003");
    assert_eq!(chain.lines(), 1);

    chain
        .append(
            "effect-before",
            json!({
                "action_node_id":action_node,
                "effect":"process.run",
                "scope":scope,
                "operation":process_operation(None, None, None, None),
                "verdict":"granted-unenforced",
                "attempt":0
            }),
        )
        .unwrap();
    assert_eq!(
        chain
            .append(
                "action-start",
                json!({
                    "action_node_id":action_node,
                    "action_id":"/project/tasks.opaal::ready",
                    "contract_digest":digest('5')
                }),
            )
            .unwrap_err()
            .code(),
        "JOURNAL003"
    );
    chain
        .append(
            "effect-after",
            json!({
                "action_node_id":action_node,
                "effect":"process.run",
                "scope":scope,
                "operation":process_operation(
                    Some(0),
                    Some(12),
                    Some(0),
                    Some(digest('8')),
                ),
                "attempt":0,
                "outcome":outcome("success", "EFFECT000", "probe completed"),
                "evidence_digest":digest('8')
            }),
        )
        .unwrap();
    chain
        .append(
            "action-start",
            json!({
                "action_node_id":action_node,
                "action_id":"/project/tasks.opaal::ready",
                "contract_digest":digest('5')
            }),
        )
        .unwrap();
    assert_eq!(
        chain
            .append(
                "terminal",
                json!({
                    "finished_at":"2026-09-09T08:02:00.000000000Z",
                    "primary":outcome("success", "RUN000", "run completed"),
                    "cleanup":[],
                    "complete":true
                }),
            )
            .unwrap_err()
            .code(),
        "JOURNAL003"
    );
}

#[test]
fn native_paths_and_wall_time_use_the_exact_persisted_spelling() {
    assert_eq!(
        encode_native_path(Path::new("/")),
        json!({"encoding":"base64url-nopad","platform":"unix","value":"Lw"})
    );
    assert_eq!(
        timestamp_from_unix_nanos(0).unwrap(),
        "1970-01-01T00:00:00.000000000Z"
    );
    assert_eq!(
        timestamp_from_unix_nanos(1_709_164_800_123_456_789).unwrap(),
        "2024-02-29T00:00:00.123456789Z"
    );
}

fn standard_host() -> Value {
    json!({
        "evidence_policy":"metadata-only", "roles":["entropy.system"],
        "max_call_bytes":1048576, "max_host_bytes":8388608,
        "max_integer_candidates":128, "operation_timeout_ms":30000,
        "poll_interval_ms":25, "term_grace_ms":100, "max_chunk_bytes":65536
    })
}

fn random_document(mut document: Value, schema: &str) -> Value {
    document["schema"] = json!(schema);
    document["schema_version"] = json!(3);
    document["standard_host"] = standard_host();
    let request = json!({"effect":"entropy.system", "scope":{"kind":"evaluation"}, "verdict":"granted-enforced"});
    document["authority"]["rules"] = json!([{
        "effect":"entropy.system", "scope":{"kind":"evaluation"},
        "decision":"grant", "required_enforcement":"enforced"
    }]);
    document["authority"]["requests"] = json!([request]);
    if let Some(actions) = document.get_mut("actions").and_then(Value::as_array_mut) {
        actions[0]["requests"] = json!([request]);
    }
    document
}

fn metadata_outcome(class: &str) -> Value {
    outcome(
        class,
        "EFFECT000",
        opaal_runtime::workflow::METADATA_ONLY_MESSAGE,
    )
}

fn entropy_operation(requested: u64, admitted: u64, progress: Option<(u64, u64)>) -> Value {
    json!({
        "kind":"entropy", "operation":"std::random::bytes", "effect":"entropy.system",
        "requested_bytes":requested, "admitted_bytes":admitted,
        "confirmed_bytes":progress.map(|counts| counts.0),
        "uncertain_bytes_upper_bound":progress.map(|counts| counts.1), "eof":null
    })
}

fn metadata_chain(policy: Value) -> (JournalChain, Vec<u8>) {
    let mut payload = header(&digest('5'));
    payload["standard_host"] = policy;
    let (mut chain, mut bytes) =
        JournalChain::begin("00000000000000000000000000000011", payload).unwrap();
    bytes.extend(
        chain
            .append(
                "action-start",
                json!({
                    "action_node_id":format!("{}#000000", digest('5')),
                    "action_id":"/project/tasks.opaal::ready", "contract_digest":digest('5')
                }),
            )
            .unwrap(),
    );
    (chain, bytes)
}

fn entropy_after(operation: Value, class: &str) -> Value {
    json!({
        "action_node_id":format!("{}#000000", digest('5')), "effect":"entropy.system",
        "scope":{"kind":"evaluation"}, "operation":operation, "attempt":0,
        "outcome":metadata_outcome(class), "evidence_digest":null
    })
}

#[test]
fn v3_entropy_progress_cannot_raise_admission_or_exceed_one_fill() {
    for (name, requested, before_admitted, confirmed, uncertain, class) in [
        ("bytes", 8, 0, 8, 0, "success"),
        ("bytes", 512, 512, 0, 512, "cancelled"),
        ("int", 1024, 1024, 1, 0, "cancelled"),
        ("int", 1024, 1024, 0, 16, "cancelled"),
        ("float", 8, 8, 1, 7, "cancelled"),
    ] {
        let (mut chain, mut bytes) = metadata_chain(standard_host());
        let mut before = entropy_operation(requested, before_admitted, None);
        before["operation"] = json!(format!("std::random::{name}"));
        bytes.extend(
            chain
                .append(
                    "effect-before",
                    effect_before(
                        "entropy.system",
                        json!({"kind":"evaluation"}),
                        before.clone(),
                    ),
                )
                .unwrap(),
        );
        let mut operation = before;
        operation["admitted_bytes"] = json!(confirmed + uncertain);
        operation["confirmed_bytes"] = json!(confirmed);
        operation["uncertain_bytes_upper_bound"] = json!(uncertain);
        let mut after = entropy_after(operation, class);
        after["outcome"]["partial"] = json!(class != "success");
        let checkpoint = chain.clone();
        let result = chain.append("effect-after", after.clone());
        assert!(
            result.is_err(),
            "{name}: {confirmed} confirmed, {uncertain} uncertain"
        );
        assert_eq!(chain, checkpoint);

        let last: Value = serde_json::from_slice(
            bytes
                .split(|byte| *byte == b'\n')
                .rfind(|line| !line.is_empty())
                .unwrap(),
        )
        .unwrap();
        let mut forged = json!({"schema":"opaal.run-journal.v3", "schema_version":3,
            "run_id":last["run_id"], "seq":last["seq"].as_u64().unwrap()+1,
            "kind":"effect-after", "previous":last["digest"], "payload":after});
        forged["digest"] = json!(digest_value(&forged).unwrap());
        bytes.extend(serde_json::to_vec(&forged).unwrap());
        bytes.push(b'\n');
        assert!(audit_journal(&bytes).is_err());
    }
}

#[test]
fn v3_static_artifacts_bind_exact_reachable_roles_and_policy() {
    let check = CheckArtifact::seal(random_document(check_document(), "opaal.check.v3")).unwrap();
    assert_eq!(CheckArtifact::parse(check.bytes()).unwrap(), check);
    let document = random_document(plan_document(), "opaal.plan.v3");
    let plan = PlanArtifact::seal(document.clone()).unwrap();
    assert_eq!(PlanArtifact::parse(plan.bytes()).unwrap(), plan);
    assert!(plan.is_executable());
    let mut lower = document.clone();
    lower["standard_host"]["max_host_bytes"] = json!(0);
    assert_ne!(PlanArtifact::seal(lower).unwrap().digest(), plan.digest());

    for field in ["schema", "schema_version", "standard_host"] {
        let mut missing = document.clone();
        missing.as_object_mut().unwrap().remove(field);
        assert!(PlanArtifact::seal(missing).is_err(), "missing {field}");
    }
    for roles in [
        json!([]),
        json!(["stdout.write"]),
        json!(["entropy.system", "entropy.system"]),
        json!(["unknown"]),
    ] {
        let mut invalid = document.clone();
        invalid["standard_host"]["roles"] = roles;
        assert!(PlanArtifact::seal(invalid).is_err());
    }
    for field in [
        "max_call_bytes",
        "max_host_bytes",
        "max_integer_candidates",
        "operation_timeout_ms",
        "poll_interval_ms",
        "term_grace_ms",
        "max_chunk_bytes",
    ] {
        let mut raised = document.clone();
        raised["standard_host"][field] =
            json!(document["standard_host"][field].as_u64().unwrap() + 1);
        assert!(PlanArtifact::seal(raised).is_err(), "raised {field}");
        let mut negative = document.clone();
        negative["standard_host"][field] = json!(-1);
        assert!(PlanArtifact::seal(negative).is_err(), "negative {field}");
    }
    for (field, value) in [
        ("evidence_policy", json!("full")),
        ("payload", json!("sentinel")),
    ] {
        let mut invalid = document.clone();
        invalid["standard_host"][field] = value;
        assert!(PlanArtifact::seal(invalid).is_err());
    }
    let mut v2 = document.clone();
    v2["schema"] = json!("opaal.plan.v2");
    v2["schema_version"] = json!(2);
    v2.as_object_mut().unwrap().remove("standard_host");
    assert!(PlanArtifact::seal(v2).is_err());
    let mut wrong_scope = document.clone();
    wrong_scope["authority"]["rules"][0]["scope"] = json!({"kind":"clock", "clock":"wall"});
    assert!(PlanArtifact::seal(wrong_scope).is_err());
    let mut unenforced = document;
    unenforced["authority"]["rules"][0]["required_enforcement"] = json!("acknowledged-unenforced");
    assert!(PlanArtifact::seal(unenforced).is_err());
    for outcome_path in ["outcome", "actions"] {
        let mut invalid = random_document(plan_document(), "opaal.plan.v3");
        if outcome_path == "outcome" {
            invalid["outcome"]["value_digest"] = json!(digest('a'));
        } else {
            invalid["actions"][0]["outcome"]["value_digest"] = json!(digest('a'));
        }
        assert!(PlanArtifact::seal(invalid).is_err());
    }
}

#[test]
fn v3_journal_preserves_safe_progress_and_policy_through_audit() {
    let (mut chain, mut bytes) = metadata_chain(standard_host());
    bytes.extend(
        chain
            .append(
                "effect-before",
                effect_before(
                    "entropy.system",
                    json!({"kind":"evaluation"}),
                    entropy_operation(16, 16, None),
                ),
            )
            .unwrap(),
    );
    let mut after = entropy_after(entropy_operation(16, 16, Some((8, 8))), "cancelled");
    after["outcome"]["partial"] = json!(true);
    bytes.extend(chain.append("effect-after", after).unwrap());
    let mut primary = metadata_outcome("cancelled");
    primary["partial"] = json!(true);
    bytes.extend(
        chain
            .append(
                "action-end",
                json!({"action_node_id":format!("{}#000000", digest('5')), "outcome":primary}),
            )
            .unwrap(),
    );
    bytes.extend(
        chain
            .append(
                "terminal",
                json!({
                    "finished_at":"2026-09-09T08:02:00.000000000Z", "primary":primary,
                    "cleanup":[], "complete":true
                }),
            )
            .unwrap(),
    );
    let audit = audit_journal(&bytes).unwrap();
    assert_eq!(audit.value()["schema"], "opaal.audit.v3");
    assert_eq!(audit.value()["standard_host"], standard_host());
    assert_eq!(
        audit.value()["events"][2]["payload"]["operation"]["uncertain_bytes_upper_bound"],
        8
    );
    assert_eq!(AuditArtifact::parse(audit.bytes()).unwrap(), audit);
    assert!(audit.is_complete());
    // V2 bytes retain their version; changing only an identity is never an upgrade.
    let mixed = String::from_utf8(bytes).unwrap().replacen(
        "opaal.run-journal.v3",
        "opaal.run-journal.v2",
        1,
    );
    assert!(audit_journal(mixed.as_bytes()).is_err());
}

#[test]
fn v3_progress_refuses_forged_counts_results_and_payload_extensions() {
    let (chain, _) = metadata_chain(standard_host());
    for (field, value) in [
        ("admitted_bytes", json!(9)),
        ("confirmed_bytes", json!(0)),
        ("eof", json!(true)),
        ("bound", json!(123)),
        ("payload_digest", json!(digest('a'))),
        ("operation", json!("std::random::unknown")),
        ("effect", json!("stdout.write")),
    ] {
        let mut operation = entropy_operation(8, 8, None);
        operation[field] = value;
        assert!(
            chain
                .clone()
                .append(
                    "effect-before",
                    effect_before("entropy.system", json!({"kind":"evaluation"}), operation)
                )
                .is_err(),
            "before {field}"
        );
    }
    let mut admitted = chain.clone();
    admitted
        .append(
            "effect-before",
            effect_before(
                "entropy.system",
                json!({"kind":"evaluation"}),
                entropy_operation(8, 8, None),
            ),
        )
        .unwrap();
    for (confirmed, uncertain, class, partial) in [
        (9, 0, "success", false),
        (8, 1, "success", false),
        (7, 1, "success", false),
        (8, 0, "success", true),
        (0, 8, "cancelled", false),
        (8, 0, "refused", true),
    ] {
        let mut after = entropy_after(entropy_operation(8, 8, Some((confirmed, uncertain))), class);
        after["outcome"]["partial"] = json!(partial);
        assert!(
            admitted.clone().append("effect-after", after).is_err(),
            "{confirmed} {uncertain} {class}"
        );
    }
    let mut unknown = entropy_after(entropy_operation(8, 8, None), "cancelled");
    unknown["outcome"]["partial"] = json!(true);
    assert!(admitted.append("effect-after", unknown).is_err());

    let mut narrow = standard_host();
    narrow["max_call_bytes"] = json!(7);
    let (mut narrow, _) = metadata_chain(narrow);
    assert!(
        narrow
            .append(
                "effect-before",
                effect_before(
                    "entropy.system",
                    json!({"kind":"evaluation"}),
                    entropy_operation(8, 8, None)
                )
            )
            .is_err()
    );
    let mut absent = standard_host();
    absent["roles"] = json!(["stdout.write"]);
    let (mut absent, _) = metadata_chain(absent);
    assert!(
        absent
            .append(
                "effect-before",
                effect_before(
                    "entropy.system",
                    json!({"kind":"evaluation"}),
                    entropy_operation(0, 0, None)
                )
            )
            .is_err()
    );
}

#[test]
fn v3_http_scope_cannot_retain_the_dynamic_method() {
    let (mut chain, _) = metadata_chain(standard_host());
    let event = effect_before(
        "network.http",
        json!({"kind":"endpoint", "endpoint":"api", "method":"METHOD-SENTINEL"}),
        json!({"kind":"http", "operation":"std::http::request", "endpoint":"api", "method":null, "status":null, "body_bytes":null, "evidence_digest":null}),
    );
    assert!(chain.append("effect-before", event).is_err());
}

#[test]
fn v3_metadata_policy_covers_ordinary_sibling_actions_and_secondaries() {
    let (chain, _) = metadata_chain(standard_host());
    for class in ["success", "error", "cancelled", "refused", "cleanup-failed"] {
        for (field, value) in [
            ("message", json!("payload-sentinel")),
            ("value_digest", json!(digest('b'))),
        ] {
            let mut outcome = metadata_outcome(class);
            outcome[field] = value;
            assert!(chain.clone().append("action-end", json!({"action_node_id":format!("{}#000000", digest('5')), "outcome":outcome})).is_err());
        }
    }
    // An untaken entropy branch still requires metadata-only ordinary effects.
    for (effect, scope, safe, field, unsafe_value) in [
        (
            "clock.wall",
            json!({"kind":"clock", "clock":"wall"}),
            clock_operation(None),
            "evidence_digest",
            json!(digest('c')),
        ),
        (
            "filesystem.read",
            json!({"kind":"project-path", "path":native_path()}),
            json!({"kind":"filesystem", "operation":"std::filesystem::read", "relative_path":null, "bytes":null, "evidence_digest":null}),
            "relative_path",
            encode_native_path(Path::new("payload-sentinel")),
        ),
        (
            "process.run",
            json!({"kind":"tool", "tool":"rust"}),
            json!({"kind":"process", "operation":"std::process::run", "tool":"rust", "argv_count":1, "argv_bytes":16, "argv_digest":null, "argv":[], "status":null, "stdout_bytes":null, "stderr_bytes":null, "evidence_digest":null}),
            "argv_digest",
            json!(digest('d')),
        ),
        (
            "network.http",
            json!({"kind":"endpoint", "endpoint":"api", "method":null}),
            json!({"kind":"http", "operation":"std::http::request", "endpoint":"api", "method":null, "status":null, "body_bytes":null, "evidence_digest":null}),
            "method",
            json!("payload-sentinel"),
        ),
    ] {
        chain
            .clone()
            .append(
                "effect-before",
                effect_before(effect, scope.clone(), safe.clone()),
            )
            .unwrap();
        let mut unsafe_operation = safe;
        unsafe_operation[field] = unsafe_value;
        assert!(
            chain
                .clone()
                .append(
                    "effect-before",
                    effect_before(effect, scope, unsafe_operation)
                )
                .is_err(),
            "{field}"
        );
    }
    let mut ended = chain;
    ended.append("action-end", json!({"action_node_id":format!("{}#000000", digest('5')), "outcome":metadata_outcome("success")})).unwrap();
    let mut secondary = metadata_outcome("cleanup-failed");
    secondary["message"] = json!("secondary-sentinel");
    assert!(ended.append("cleanup", json!({"action_node_id":null, "resource_id":"worker", "ordinal":0, "outcome":secondary})).is_err());
}

#[test]
fn v3_admission_settlement_and_cumulative_limits_survive_audit() {
    let mut policy = standard_host();
    policy["max_host_bytes"] = json!(8);
    let (mut chain, mut bytes) = metadata_chain(policy);
    bytes.extend(
        chain
            .append(
                "effect-before",
                effect_before(
                    "entropy.system",
                    json!({"kind":"evaluation"}),
                    entropy_operation(8, 8, None),
                ),
            )
            .unwrap(),
    );
    bytes.extend(
        chain
            .append(
                "effect-after",
                entropy_after(entropy_operation(8, 8, Some((8, 0))), "success"),
            )
            .unwrap(),
    );
    let mut second_before = effect_before(
        "entropy.system",
        json!({"kind":"evaluation"}),
        entropy_operation(1, 0, None),
    );
    second_before["attempt"] = json!(1);
    bytes.extend(chain.append("effect-before", second_before).unwrap());
    let mut second_after = entropy_after(entropy_operation(1, 1, Some((1, 0))), "success");
    second_after["attempt"] = json!(1);
    let checkpoint = chain.clone();
    assert!(chain.append("effect-after", second_after.clone()).is_err());
    assert_eq!(chain, checkpoint);
    // A correctly hashed hostile suffix must also fail the reader's budget gate.
    let last: Value = serde_json::from_slice(
        bytes
            .split(|byte| *byte == b'\n')
            .rfind(|line| !line.is_empty())
            .unwrap(),
    )
    .unwrap();
    let mut forged = json!({"schema":"opaal.run-journal.v3", "schema_version":3,
        "run_id":last["run_id"], "seq":last["seq"].as_u64().unwrap()+1,
        "kind":"effect-after", "previous":last["digest"], "payload":second_after});
    forged["digest"] = json!(digest_value(&forged).unwrap());
    bytes.extend(serde_json::to_vec(&forged).unwrap());
    bytes.push(b'\n');
    assert!(audit_journal(&bytes).is_err());
}

#[test]
fn v3_open_admissions_remain_within_the_cumulative_host_limit() {
    let mut policy = standard_host();
    policy["max_host_bytes"] = json!(8);
    let (mut chain, mut bytes) = metadata_chain(policy);
    bytes.extend(
        chain
            .append(
                "effect-before",
                effect_before(
                    "entropy.system",
                    json!({"kind":"evaluation"}),
                    entropy_operation(4, 4, None),
                ),
            )
            .unwrap(),
    );
    bytes.extend(
        chain
            .append(
                "effect-after",
                entropy_after(entropy_operation(4, 4, Some((4, 0))), "success"),
            )
            .unwrap(),
    );
    let mut before = effect_before(
        "entropy.system",
        json!({"kind":"evaluation"}),
        entropy_operation(5, 5, None),
    );
    before["attempt"] = json!(1);
    let checkpoint = chain.clone();
    assert!(chain.append("effect-before", before.clone()).is_err());
    assert_eq!(chain, checkpoint);

    let last: Value = serde_json::from_slice(
        bytes
            .split(|byte| *byte == b'\n')
            .rfind(|line| !line.is_empty())
            .unwrap(),
    )
    .unwrap();
    let mut forged = json!({"schema":"opaal.run-journal.v3", "schema_version":3,
        "run_id":last["run_id"], "seq":last["seq"].as_u64().unwrap()+1,
        "kind":"effect-before", "previous":last["digest"], "payload":before});
    forged["digest"] = json!(digest_value(&forged).unwrap());
    let mut hostile = bytes.clone();
    hostile.extend(serde_json::to_vec(&forged).unwrap());
    hostile.push(b'\n');
    assert!(audit_journal(&hostile).is_err());

    before["operation"]["requested_bytes"] = json!(4);
    before["operation"]["admitted_bytes"] = json!(4);
    bytes.extend(chain.append("effect-before", before).unwrap());
    let audit = audit_journal(&bytes).unwrap();
    assert!(!audit.is_complete());
    let mut forged = audit.value().clone();
    forged["standard_host"]["max_host_bytes"] = json!(7);
    forged.as_object_mut().unwrap().remove("digest");
    forged["digest"] = json!(digest_value(&forged).unwrap());
    assert!(AuditArtifact::parse(&serde_json::to_vec(&forged).unwrap()).is_err());
}

#[test]
fn v3_scalar_and_stream_descriptors_have_closed_roles_and_count_rules() {
    for (operation_name, effect, requested, confirmed, eof) in [
        ("std::random::int", "entropy.system", 1024, 8, Value::Null),
        ("std::random::int", "entropy.system", 0, 0, Value::Null),
        ("std::random::float", "entropy.system", 8, 8, Value::Null),
        ("std::io::read_stdin", "stdin.read", 9, 8, json!(true)),
        ("std::io::println", "stdout.write", 1, 1, Value::Null),
        ("std::io::eprintln", "stderr.write", 1, 1, Value::Null),
        ("std::io::print", "stdout.write", 0, 0, Value::Null),
        ("std::io::eprint", "stderr.write", 0, 0, Value::Null),
        ("std::io::write_stdout", "stdout.write", 8, 8, Value::Null),
        ("std::io::write_stderr", "stderr.write", 8, 8, Value::Null),
    ] {
        let mut policy = standard_host();
        policy["roles"] = json!([effect]);
        let (mut chain, mut bytes) = metadata_chain(policy);
        let mut before = entropy_operation(requested, requested, None);
        before["operation"] = json!(operation_name);
        before["effect"] = json!(effect);
        before["kind"] = json!(if effect == "entropy.system" {
            "entropy"
        } else {
            "standard-stream"
        });
        bytes.extend(
            chain
                .append(
                    "effect-before",
                    effect_before(effect, json!({"kind":"evaluation"}), before.clone()),
                )
                .unwrap(),
        );
        let mut after = before;
        after["admitted_bytes"] = json!(confirmed);
        after["confirmed_bytes"] = json!(confirmed);
        after["uncertain_bytes_upper_bound"] = json!(0);
        after["eof"] = eof;
        let mut payload = entropy_after(after, "success");
        payload["effect"] = json!(effect);
        let mut invalid = payload.clone();
        invalid["operation"]["confirmed_bytes"] = json!(u64::MAX);
        invalid["operation"]["uncertain_bytes_upper_bound"] = json!(1);
        assert!(chain.clone().append("effect-after", invalid).is_err());
        bytes.extend(chain.append("effect-after", payload).unwrap());
        audit_journal(&bytes).unwrap();
    }
}
