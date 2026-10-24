// Test-only persistence faults after an injected primary; no evaluator or child execution.
#[test]
fn native_frozen_finish_evidence_cases() {
    use opaal_platform::operational::FakeOperationalAdapter;
    use opaal_runtime::lifetime::{ResourceOwnerId, ResourceScope};
    use opaal_runtime::security::SecretStore;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let inventory =
        include_bytes!("../../../../tests/golden/controlled-external-tools/evidence-cases.json");
    let cases: Value = serde_json::from_slice(inventory).unwrap();
    let host = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        _ => return,
    };
    let digest = format!("sha256:{}", "1".repeat(64));
    let node = format!("{digest}#000000");
    let emit = |name: &str, observed: Value| {
        let row = cases["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == name)
            .unwrap();
        assert_eq!(observed, row["expected"], "{name}");
        println!(
            "OPAAL_EVIDENCE_CASE {}",
            json!({"name":name, "observed":observed,
            "host":host, "inventory_digest":digest_bytes(inventory)})
        );
    };
    for primary_kind in [
        "success",
        "error",
        "requested-cancellation",
        "deadline-cancellation",
    ] {
        let faults: &[&str] = if primary_kind == "deadline-cancellation" {
            &["none"]
        } else {
            &[
                "none",
                "before-event",
                "after-event",
                "action-end",
                "terminal",
                "receipt",
                "audit",
                "summary",
                "cleanup",
            ]
        };
        for &fault in faults {
            let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
                "opaal-finish-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            let filesystem = HostProjectFilesystem::new(
                root.clone(),
                File::open(&root).unwrap(),
                ControlReadBudget::default(),
            );
            let path = root.join("run.jsonl");
            let header = json!({"plan_digest":digest, "accepted_plan_digest":digest, "authority_digest":digest,
                "project_digest":digest, "environment_digest":digest, "tool_lock_digest":digest,
                "child_environment_digest":digest, "started_at":"1970-01-01T00:00:00.000000000Z",
                "result_policy":"require-true"});
            let mut journal = filesystem
                .create_journal(&path, &"1".repeat(32), header, 5)
                .unwrap();
            let prefix = fs::read(&path).unwrap();
            let class = match primary_kind {
                "success" => "success",
                "error" => "error",
                _ => "cancelled",
            };
            let reason = match primary_kind {
                "requested-cancellation" => Some(CancelReason::Requested),
                "deadline-cancellation" => Some(CancelReason::Timeout),
                _ => None,
            };
            journal.cancel_reason = reason;
            journal.assessment = (class == "success").then_some(true);
            let mut primary = outcome_value(
                class,
                "EXECUTE_TEST",
                "primary-payload-canary",
                None,
                None,
                true,
            );
            primary["cancel_reason"] = cancel_reason_value(reason, &primary);
            journal.primary = Some(primary.clone());
            journal
                .append(
                    "action-start",
                    json!({"action_node_id":node,
                "action_id":"/project/tasks.opaal::check", "contract_digest":digest}),
                )
                .unwrap();
            let operation = json!({"kind":"process", "operation":"std::process::run", "tool":"checker",
                "argv_count":1, "argv_bytes":1, "argv":[], "argv_digest":null,
                "status":null, "stdout_bytes":null, "stderr_bytes":null, "evidence_digest":null});
            let mut before = json!({"action_node_id":node, "effect":"process.run",
                "scope":{"kind":"tool", "tool":"checker"}, "attempt":0,
                "verdict":"granted-unenforced", "operation":operation});
            let mut after = before.clone();
            after.as_object_mut().unwrap().remove("verdict");
            after["outcome"] = success_outcome("EFFECT000", "effect-payload-canary");
            after["evidence_digest"] = Value::Null;
            for field in ["status", "stdout_bytes", "stderr_bytes"] {
                after["operation"][field] = json!(0);
            }
            let mut event_failed = false;
            let mut action_outcome = primary.clone();
            action_outcome
                .as_object_mut()
                .unwrap()
                .remove("cancel_reason");
            for (stage, kind, payload) in [
                ("before-event", "effect-before", std::mem::take(&mut before)),
                ("after-event", "effect-after", after),
                (
                    "action-end",
                    "action-end",
                    json!({"action_node_id":node, "outcome":action_outcome}),
                ),
            ] {
                if stage == fault {
                    journal.file = File::open(&path).unwrap();
                }
                if let Err(error) = journal.append(kind, payload) {
                    assert_eq!(stage, fault, "{}", error.rendered());
                    assert!(error.rendered().contains("JOURNAL005"));
                    event_failed = true;
                    break;
                }
            }
            let cleanup = if fault == "cleanup" {
                let mut resources = ResourceScope::new(ResourceOwnerId::new(
                    evaluation_id(&"1".repeat(32)).unwrap(),
                ));
                resources
                    .register(|| Err("cleanup-payload-canary".to_owned()))
                    .unwrap();
                append_cleanup_events(&mut journal, &resources.close(&SecretStore::new())).unwrap()
            } else {
                Vec::new()
            };
            if fault == "terminal" {
                journal.file = File::open(&path).unwrap();
            }
            let result = if event_failed {
                Err(journal_failure(
                    &primary,
                    execute_error("JOURNAL005", "sink-payload-canary"),
                    true,
                ))
            } else {
                finish_execution_journal(
                    &mut journal,
                    primary.clone(),
                    cleanup,
                    &FakeOperationalAdapter::new(),
                )
            };
            let receipt_path = root.join("receipt.json");
            let mut receipt_sink = filesystem.create_receipt(&receipt_path).unwrap();
            if fault == "receipt" {
                receipt_sink.file = File::open(&receipt_path).unwrap();
            }
            let execution = Ok(finish_receipt(
                receipt_sink,
                &digest,
                &mut journal,
                result,
                true,
            ));
            let audit_path = root.join("audit.json");
            let mut sink = filesystem.create_receipt(&audit_path).unwrap();
            if fault == "audit" {
                sink.file = File::open(&audit_path).unwrap();
            }
            let mut audit = RunAudit {
                path: audit_path.clone(),
                sink: Some(sink),
                artifact: None,
                persisted: false,
                signals: opaal_platform_posix::standard_host::SignalLease::capture().unwrap(),
                control: opaal_runtime::module::AnalysisControl::never(),
            };
            audit.finish(&filesystem, &path, &journal.file);
            assert_eq!(audit.persisted, fault != "audit");
            let receipt = execution.as_ref().unwrap().receipt().unwrap();
            assert_eq!(receipt["primary"]["class"], class);
            assert_eq!(receipt["primary"]["code"], "EXECUTE_TEST");
            assert_eq!(
                receipt["primary"]["cancel_reason"],
                primary["cancel_reason"]
            );
            assert_eq!(receipt["primary"]["partial"], true);
            let request = ExecuteProjectRequest::new(
                root.join("plan.json"),
                digest.clone(),
                None,
                None,
                path.clone(),
            )
            .with_require_true(true);
            let mut summary = FaultSink {
                fault: if fault == "summary" {
                    "partial"
                } else {
                    "none"
                },
                bytes: Rc::new(RefCell::new(Vec::new())),
                calls: Rc::new(RefCell::new(Vec::new())),
            };
            let run = finish_task_run(&request, &execution, &audit, &mut summary);
            assert_eq!(
                run.exit_code,
                if primary_kind == "requested-cancellation" {
                    130
                } else if fault == "none" && class == "success" {
                    0
                } else {
                    2
                }
            );
            assert_eq!(
                summary
                    .calls
                    .borrow()
                    .iter()
                    .filter(|call| **call == "write")
                    .count(),
                if fault == "summary" { 2 } else { 1 }
            );
            let retained = fs::read(&path).unwrap();
            assert!(retained.starts_with(&prefix));
            for artifact in [&path, &receipt_path, &audit_path] {
                assert!(artifact.exists());
                assert!(!fs::read_to_string(artifact).unwrap().contains("canary"));
            }
            if fault == "none" {
                assert!(audit.artifact.as_ref().unwrap().is_complete());
                let conclusion =
                    explanation(receipt, audit.artifact.as_ref(), false).unwrap()["conclusion"]
                        .clone();
                if primary_kind == "requested-cancellation" {
                    emit(
                        "requested-cancellation",
                        json!({"exit":run.exit_code,"conclusion":conclusion}),
                    );
                } else if primary_kind == "deadline-cancellation" {
                    emit(
                        "deadline-cancellation",
                        json!({"exit":run.exit_code,"passed":run.exit_code == 0}),
                    );
                }
            } else {
                let mut observed = json!({"exit":run.exit_code,"passed":run.exit_code == 0,
                    "partial_files_retained":retained.starts_with(&prefix)});
                if class != "success" {
                    observed["primary_preserved"] = json!(
                        receipt["primary"]["class"] == class
                            && receipt["primary"]["code"] == "EXECUTE_TEST"
                            && receipt["primary"]["cancel_reason"] == primary["cancel_reason"]
                    );
                }
                emit(&format!("{primary_kind}-{fault}-failure"), observed);
            }
            fs::remove_dir_all(&root).unwrap();
        }
    }
}
