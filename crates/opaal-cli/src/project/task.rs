use super::*;
use opaal_runtime::workflow::{AuditArtifact, parse_execution_receipt};

/// The exit selected after execution, owned finish, audit and summary persistence.
#[derive(Debug)]
pub struct TaskRun {
    pub exit_code: u8,
}

pub(super) struct RunAudit {
    path: PathBuf,
    sink: Option<ReceiptSink>,
    artifact: Option<AuditArtifact>,
    persisted: bool,
    signals: opaal_platform_posix::standard_host::SignalLease,
    pub(super) control: opaal_runtime::module::AnalysisControl,
}

impl RunAudit {
    pub(super) fn cancellation(&self) -> Box<dyn Fn() -> bool + Send + Sync> {
        self.signals.cancellation()
    }
    pub(super) fn reserve(
        &mut self,
        filesystem: &HostProjectFilesystem,
        request: &ExecuteProjectRequest,
        journal: &Path,
    ) -> Result<(), ProjectFrontendError> {
        let receipt = request
            .receipt_out
            .as_ref()
            .ok_or_else(|| execute_error("TASK001", "task run requires a receipt destination"))?;
        let paths = [
            journal.to_path_buf(),
            filesystem
                .resolve_output_path(receipt)
                .map_err(frontend_contract)?,
            filesystem
                .resolve_output_path(&self.path)
                .map_err(frontend_contract)?,
        ];
        if paths.iter().collect::<BTreeSet<_>>().len() != paths.len() {
            return Err(execute_error(
                "TASK001",
                "task evidence destinations must differ",
            ));
        }
        // Check every destination before reserving any sink or starting task effects.
        for path in &paths {
            let relative = path
                .strip_prefix(&filesystem.root)
                .expect("validated output root");
            let (parent, name) =
                open_relative_output_parent(&filesystem.root_directory, relative, path)
                    .map_err(frontend_contract)?;
            require_absent_output(&parent, &name, path).map_err(frontend_contract)?;
        }
        self.sink = Some(filesystem.create_receipt(&paths[2])?);
        Ok(())
    }

    pub(super) fn finish(
        &mut self,
        filesystem: &HostProjectFilesystem,
        journal: &Path,
        identity: &File,
    ) {
        let Ok(artifact) = generate_audit(filesystem, journal, Some(identity)) else {
            return;
        };
        if let Some(sink) = self.sink.take() {
            self.persisted = persist_audit(sink, artifact.bytes()).is_ok();
        }
        self.artifact = Some(artifact);
    }
}

fn persist_audit(mut sink: ReceiptSink<impl ReceiptFile>, bytes: &[u8]) -> std::io::Result<()> {
    sink.file.write_all(bytes)?;
    sink.file.flush()?;
    sink.file.sync_all()?;
    sink.parent.sync_all()
}

/// Execute exactly one accepted plan and persist its separate safe stderr summary.
pub fn run_accepted_task(
    request: &ExecuteProjectRequest,
    audit_out: &Path,
    secret_input_is_terminal: bool,
    secret_input: &mut dyn Read,
    summary: &mut dyn Write,
    signals: opaal_platform_posix::standard_host::SignalLease,
) -> TaskRun {
    let control = opaal_runtime::module::AnalysisControl::cooperative(signals.cancellation());
    let mut audit = RunAudit {
        path: audit_out.to_path_buf(),
        sink: None,
        artifact: None,
        persisted: false,
        signals,
        control,
    };
    let execution = execute_plan(
        request,
        secret_input_is_terminal,
        secret_input,
        Some(&mut audit),
    );
    finish_task_run(request, &execution, &audit, summary)
}

fn finish_task_run(
    request: &ExecuteProjectRequest,
    execution: &Result<ExecuteFrontendRun, ProjectFrontendError>,
    audit: &RunAudit,
    summary: &mut dyn Write,
) -> TaskRun {
    let receipt = execution
        .as_ref()
        .ok()
        .and_then(ExecuteFrontendRun::receipt);
    let requested = audit.control.is_cancelled()
        || receipt.is_some_and(|receipt| {
            receipt["primary"]["class"] == "cancelled"
                && receipt["primary"]["cancel_reason"] == "requested"
        });
    let mut exit_code = if requested {
        130
    } else if let (Ok(run), Some(receipt)) = (execution, receipt) {
        if !audit.persisted
            || !audit
                .artifact
                .as_ref()
                .is_some_and(AuditArtifact::is_complete)
        {
            2
        } else if request.require_true {
            selected_exit(receipt, run.is_successful())
        } else if run.is_successful() {
            0
        } else {
            2
        }
    } else {
        2
    };
    let view = receipt.and_then(|receipt| {
        explanation(
            receipt,
            audit.artifact.as_ref().filter(|_| audit.persisted),
            false,
        )
        .ok()
    });
    let output = view
        .as_ref()
        .and_then(|view| render_explanation(view, false).ok());
    if output.is_none() && !requested {
        exit_code = 2;
    }
    let output = output
        .as_deref()
        .unwrap_or(b"TASK001: execution evidence unavailable; result unproven\n");
    if summary
        .write_all(output)
        .and_then(|()| summary.flush())
        .is_err()
        && !requested
    {
        exit_code = 2;
    }
    if audit.control.is_cancelled() {
        exit_code = 130;
    }
    TaskRun { exit_code }
}

/// Read only explicit recorded artifacts; a valid view does not prove the original command exit.
pub fn explain_task(
    receipt: &Path,
    audit: Option<&Path>,
    format_json: bool,
) -> Result<Vec<u8>, ProjectFrontendError> {
    let receipt = read_inspection_artifact(receipt, opaal_runtime::workflow::MAX_ARTIFACT_BYTES)
        .and_then(|bytes| parse_execution_receipt(&bytes).map_err(workflow_contract))
        .map_err(|_| invalid_evidence())?;
    let audit = audit
        .map(|path| {
            // A missing optional audit remains unavailable; present malformed evidence is refused.
            match std::fs::symlink_metadata(path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                _ => read_inspection_artifact(path, MAX_AUDIT_BYTES)
                    .and_then(|bytes| AuditArtifact::parse(&bytes).map_err(workflow_contract))
                    .map(Some)
                    .map_err(|_| invalid_evidence()),
            }
        })
        .transpose()?
        .flatten();
    render_explanation(&explanation(&receipt, audit.as_ref(), true)?, format_json)
}

fn invalid_evidence() -> ProjectFrontendError {
    execute_error(
        "TASK002",
        "task evidence is invalid, unavailable or inconsistent",
    )
}

fn safe_code(value: &Value) -> Value {
    match value.as_str() {
        Some(
            code @ ("EXECUTE000"
            | "EXECUTE_STATUS"
            | "EXECUTE_LANGUAGE"
            | "EXECUTE_CANCELLED"
            | "EXECUTE_REFUSED"
            | "EXECUTE_HOST"
            | "EXECUTE_TYPE"
            | "EXECUTE_OUTPUT"
            | "EXECUTE_EVIDENCE_OVERFLOW"
            | "JOURNAL005"
            | "NATIVE001"
            | "NATIVE002"
            | "NATIVE003"
            | "NATIVE004"
            | "CLEANUP001"
            | "OPERATION004"),
        ) => json!(code),
        _ => Value::Null,
    }
}

fn explanation(
    receipt: &Value,
    audit: Option<&AuditArtifact>,
    compare: bool,
) -> Result<Value, ProjectFrontendError> {
    let primary = &receipt["primary"];
    let secondary = receipt["secondary"]
        .as_array()
        .expect("validated receipt secondaries");
    let receipt_cleanup_failed = secondary.iter().any(|item| item["category"] == "cleanup");
    if let Some(audit) = audit.filter(|_| compare) {
        let value = audit.value();
        if value["run_id"] != receipt["run_id"]
            || value["plan_digest"] != receipt["plan_digest"]
            || (value.get("result_policy").is_some()
                && receipt.get("result_policy").is_some()
                && value["result_policy"] != receipt["result_policy"])
        {
            return Err(invalid_evidence());
        }
        if audit.is_complete() {
            let mut audit_primary = value["primary"].clone();
            project_metadata_outcome(&mut audit_primary);
            for name in ["class", "code", "status", "value_digest", "partial"] {
                if audit_primary[name] != primary[name] {
                    return Err(invalid_evidence());
                }
            }
            if value.get("result_policy").is_some()
                && receipt.get("result_policy").is_some()
                && (value["assessment"] != receipt["assessment"]
                    || value["primary"]["cancel_reason"] != primary["cancel_reason"])
            {
                return Err(invalid_evidence());
            }
            let failed = value["cleanup"]
                .as_array()
                .expect("validated cleanup")
                .iter()
                .any(|outcome| outcome["class"] == "cleanup-failed");
            if failed != receipt_cleanup_failed {
                return Err(invalid_evidence());
            }
            if value["schema_version"] == 5 || value.get("standard_host").is_some() {
                let mut progress = json!({});
                for role in [
                    "entropy.system",
                    "stdin.read",
                    "stdout.write",
                    "stderr.write",
                ] {
                    let mut confirmed = 0_u64;
                    let mut uncertain = 0_u64;
                    for event in value["events"].as_array().expect("validated events") {
                        let payload = &event["payload"];
                        if event["kind"] == "effect-after" && payload["effect"] == role {
                            let operation = &payload["operation"];
                            confirmed = confirmed
                                .checked_add(
                                    operation["confirmed_bytes"]
                                        .as_u64()
                                        .ok_or_else(invalid_evidence)?,
                                )
                                .ok_or_else(invalid_evidence)?;
                            uncertain = uncertain
                                .checked_add(
                                    operation["uncertain_bytes_upper_bound"]
                                        .as_u64()
                                        .ok_or_else(invalid_evidence)?,
                                )
                                .ok_or_else(invalid_evidence)?;
                        }
                    }
                    progress[role] = json!({"confirmed_bytes":confirmed,"uncertain_bytes_upper_bound":uncertain});
                }
                if progress != receipt["progress"] {
                    return Err(invalid_evidence());
                }
            }
        }
    }
    let cleanup = if receipt_cleanup_failed {
        "failed"
    } else if audit.is_some_and(AuditArtifact::is_complete) {
        if audit.unwrap().value()["cleanup"]
            .as_array()
            .expect("validated cleanup")
            .iter()
            .any(|outcome| outcome["class"] == "cleanup-failed")
        {
            "failed"
        } else {
            "complete"
        }
    } else {
        "unknown"
    };
    let audit_state = audit.map_or("unavailable", |audit| {
        if audit.is_complete() {
            "complete"
        } else {
            "incomplete"
        }
    });
    let journal = receipt["journal_state"]
        .as_str()
        .expect("validated journal state");
    let evidence = if journal == "complete" && audit_state == "complete" {
        "complete"
    } else if journal == "unavailable" || audit_state == "unavailable" {
        "unavailable"
    } else {
        "incomplete"
    };
    let current = receipt.get("result_policy").is_some()
        && receipt.get("assessment").is_some()
        && audit.is_some_and(|audit| audit.value().get("result_policy").is_some());
    let conclusion = if primary["class"] == "cancelled" {
        "cancelled"
    } else if primary["class"] != "success"
        || cleanup == "failed"
        || !secondary.is_empty()
        || receipt["omitted_secondary_count"] != 0
    {
        "failed"
    } else if evidence != "complete" || cleanup != "complete" || !current {
        "unproven"
    } else if receipt["result_policy"] == "execution" {
        "execution-complete"
    } else {
        match receipt["assessment"]["value"].as_bool() {
            Some(true) => "passed",
            Some(false) => "negative",
            None => "unproven",
        }
    };
    Ok(json!({
        "schema":"opaal.task-explanation.v1", "schema_version":1,
        "run_id":receipt["run_id"], "plan_digest":receipt["plan_digest"],
        "result_policy":receipt.get("result_policy"), "execution":primary["class"],
        "assessment":receipt.get("assessment"), "primary_code":safe_code(&primary["code"]),
        "partial":primary["partial"], "progress":receipt["progress"],
        "cancel_reason":primary.get("cancel_reason"), "cleanup":cleanup,
        "journal":journal, "receipt":"valid", "audit":audit_state, "evidence":evidence,
        "conclusion":conclusion, "secondary":secondary.iter().map(|item| {
            json!({"category":item["category"],"code":safe_code(&item["code"])})
        }).collect::<Vec<_>>(), "omitted_secondary_count":receipt["omitted_secondary_count"]
    }))
}

fn render_explanation(view: &Value, format_json: bool) -> Result<Vec<u8>, ProjectFrontendError> {
    let mut bytes = if format_json {
        serde_json::to_vec(view).expect("constructed explanation")
    } else {
        let mut text = String::new();
        for (key, value) in view.as_object().expect("constructed explanation") {
            let value = value
                .as_str()
                .map_or_else(|| value.to_string(), escaped_text);
            text.push_str(&format!("{key}: {value}\n"));
        }
        text.pop();
        text.into_bytes()
    };
    bytes.push(b'\n');
    if bytes.len() > 64 * 1024 {
        return Err(execute_error(
            "TASK003",
            "task explanation exceeds its view limit",
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    include!("finish_observers.rs");

    #[test]
    fn native_explanation_codes_are_fixed_and_payload_free() {
        for code in ["NATIVE001", "NATIVE002", "NATIVE003", "NATIVE004"] {
            assert_eq!(safe_code(&json!(code)), json!(code));
        }
        assert_eq!(safe_code(&json!("NATIVE005")), Value::Null);
        assert_eq!(safe_code(&json!("NATIVE001: payload-canary")), Value::Null);
    }

    struct FaultSink {
        fault: &'static str,
        bytes: Rc<RefCell<Vec<u8>>>,
        calls: Rc<RefCell<Vec<&'static str>>>,
    }

    impl Write for FaultSink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.calls.borrow_mut().push("write");
            if self.fault == "write" {
                return Err(std::io::Error::other("payload-canary"));
            }
            if self.fault == "partial" {
                if !self.bytes.borrow().is_empty() {
                    return Err(std::io::Error::other("payload-canary"));
                }
                self.bytes.borrow_mut().extend_from_slice(&bytes[..1]);
                return Ok(1);
            }
            self.bytes.borrow_mut().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.calls.borrow_mut().push("flush");
            if self.fault == "flush" {
                Err(std::io::Error::other("payload-canary"))
            } else {
                Ok(())
            }
        }
    }

    impl ReceiptFile for FaultSink {
        fn sync_all(&self) -> std::io::Result<()> {
            self.calls.borrow_mut().push("sync");
            if self.fault == "sync" {
                Err(std::io::Error::other("payload-canary"))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn reserved_audit_faults_retain_partial_bytes_and_stop_persistence() {
        for (fault, expected) in [
            ("write", vec!["write"]),
            ("partial", vec!["write", "write"]),
            ("flush", vec!["write", "flush"]),
            ("sync", vec!["write", "flush", "sync"]),
            ("parent", vec!["write", "flush", "sync"]),
        ] {
            let bytes = Rc::new(RefCell::new(Vec::new()));
            let calls = Rc::new(RefCell::new(Vec::new()));
            let sink = ReceiptSink {
                file: FaultSink {
                    fault,
                    bytes: bytes.clone(),
                    calls: calls.clone(),
                },
                parent: File::open(if fault == "parent" {
                    Path::new("/dev/null")
                } else {
                    Path::new(env!("CARGO_MANIFEST_DIR"))
                })
                .unwrap(),
            };
            assert!(persist_audit(sink, b"canonical audit").is_err());
            assert_eq!(*calls.borrow(), expected);
            if fault == "partial" {
                assert_eq!(&*bytes.borrow(), b"c");
            }
            if fault == "write" {
                assert!(bytes.borrow().is_empty());
            }
        }
    }
}
