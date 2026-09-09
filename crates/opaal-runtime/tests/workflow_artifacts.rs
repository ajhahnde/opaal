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
        "schema":"opaal.check.v1",
        "schema_version":1,
        "toolchain":{"version":"1.0.0-alpha.1"},
        "project":project(),
        "task":task(),
        "inputs":[],
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
        "schema":"opaal.plan.v1",
        "schema_version":1,
        "created_at":"2026-09-09T08:00:00.000000000Z",
        "expires_at":"2026-09-09T08:15:00.000000000Z",
        "toolchain":{"version":"1.0.0-alpha.1"},
        "platform":{"triple":"aarch64-apple-darwin"},
        "project":project(),
        "task":task(),
        "inputs":[],
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
    journal[digest_position] = b'f';
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
