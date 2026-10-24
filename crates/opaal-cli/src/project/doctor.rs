use super::*;
use opaal_runtime::module::AnalysisControl;
use opaal_runtime::project::load_project_program_controlled;

/// One bounded safe diagnosis and its command exit status.
#[derive(Debug)]
pub struct DoctorProjectRun {
    pub output: Vec<u8>,
    pub exit_code: u8,
}

fn finding(
    code: &'static str,
    category: &'static str,
    severity: &'static str,
    message: &'static str,
    input: Option<&str>,
) -> Value {
    let input = input.filter(|name| safe_name(name));
    json!({"code":code,"category":category,"severity":severity,"input_name":input,"message":message,"source":null,"span":null})
}

fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphabetic() || byte == b'_' || (index > 0 && byte.is_ascii_digit())
        })
}

fn invalid_document() -> Value {
    finding(
        "DOCTOR001",
        "document",
        "error",
        "Selected control or source is unavailable, malformed or unsupported.",
        None,
    )
}

fn unknown_checks() -> Value {
    finding(
        "DOCTOR002",
        "task",
        "unknown",
        "Dependent task checks could not be established.",
        None,
    )
}

fn render(
    request: &CheckProjectRequest,
    project: Option<&str>,
    task: Option<&str>,
    mut findings: Vec<Value>,
    exit_code: u8,
) -> Result<DoctorProjectRun, ProjectFrontendError> {
    if findings.len() > 128 || project.map_or(0, str::len) + task.map_or(0, str::len) > 32 * 1024 {
        return Err(execute_error("DOCTOR003", "diagnosis view limit exceeded"));
    }
    findings.sort_by_key(|finding| {
        (
            finding["category"].as_str().unwrap().to_owned(),
            finding["code"].as_str().unwrap().to_owned(),
            finding["input_name"].as_str().unwrap_or("").to_owned(),
        )
    });
    let readiness = if findings
        .iter()
        .any(|finding| finding["severity"] == "error")
    {
        "blocked"
    } else if findings
        .iter()
        .any(|finding| finding["severity"] == "unknown")
    {
        "unknown"
    } else {
        "ready"
    };
    let live = if request.environment == "ci" {
        "unknown"
    } else {
        "not-applicable"
    };
    let document = json!({"schema":"opaal.project-doctor.v1","schema_version":1,"project_id":project,"task_id":task,"environment":request.environment,"local_readiness":readiness,"live_readiness":live,"findings":findings});
    let mut output = if request.format_json {
        serde_json::to_vec(&document).expect("constructed doctor view")
    } else {
        let mut text = format!(
            "project: {}\ntask: {}\nenvironment: {}\nlocal readiness: {readiness}\nlive readiness: {live}",
            project.unwrap_or("unknown"),
            task.unwrap_or("unknown"),
            request.environment
        );
        for finding in &findings {
            text.push_str(&format!(
                "\n{} [{}]{}: {}",
                finding["code"].as_str().expect("code"),
                finding["severity"].as_str().expect("severity"),
                finding["input_name"]
                    .as_str()
                    .map_or_else(String::new, |name| format!(" input={name}")),
                finding["message"].as_str().expect("fixed message")
            ));
        }
        text.into_bytes()
    };
    output.push(b'\n');
    if output.len() > 64 * 1024 {
        return Err(execute_error("DOCTOR003", "diagnosis view limit exceeded"));
    }
    Ok(DoctorProjectRun { output, exit_code })
}

/// Observe explicit local inputs through the shared loaders/checker, without execution.
pub fn doctor_project(
    request: &CheckProjectRequest,
    control: &AnalysisControl,
) -> Result<DoctorProjectRun, ProjectFrontendError> {
    if !safe_name(&request.task)
        || !safe_name(&request.environment)
        || request
            .inputs
            .iter()
            .any(|binding| !safe_name(binding.name()))
        || request
            .inputs
            .iter()
            .map(ProjectInputBinding::name)
            .collect::<BTreeSet<_>>()
            .len()
            != request.inputs.len()
    {
        return Err(execute_error(
            "DOCTOR004",
            "invalid diagnosis selection or input names",
        ));
    }
    if request.inputs.len() > opaal_runtime::project::MAX_PROJECT_INPUTS {
        return Err(execute_error("DOCTOR003", "diagnosis input limit exceeded"));
    }
    if control.is_cancelled() {
        return Ok(DoctorProjectRun {
            output: Vec::new(),
            exit_code: 130,
        });
    }
    let (manifest, filesystem, _) = match load_explicit_manifest_root_with_budget(
        &request.manifest,
        ControlReadBudget::default(),
    ) {
        Ok(loaded) => loaded,
        Err(_) => {
            return render(
                request,
                None,
                None,
                vec![invalid_document(), unknown_checks()],
                2,
            );
        }
    };
    let project_name = manifest.name().to_owned();
    let mut findings = Vec::new();
    let native = manifest
        .tools()
        .values()
        .any(|tool| tool.contract().adapter().is_none());
    if native {
        findings.push(finding(
            "DOCTOR017",
            "tool",
            "unknown",
            "Native tool version and behavior are unverified; child effects are unenforced.",
            None,
        ));
    }
    let mut exit_code = 0;
    let selected = manifest.environment(&request.environment);
    let mut authority = None;
    let mut tools = None;
    match selected {
        Ok(environment) => {
            match filesystem
                .read_existing_bounded(environment.authority(), MAX_PROJECT_DOCUMENT_BYTES)
                .and_then(|bytes| parse_authority_document(&manifest, &request.environment, &bytes))
            {
                Ok(parsed) => authority = Some(parsed),
                Err(_) => {
                    findings.push(invalid_document());
                    exit_code = 2;
                }
            }
            match filesystem
                .read_existing_bounded(environment.tool_lock(), MAX_PROJECT_DOCUMENT_BYTES)
                .and_then(|bytes| parse_tool_lock(&manifest, &request.environment, &bytes))
            {
                Ok(parsed) => tools = Some(parsed),
                Err(_) => {
                    findings.push(invalid_document());
                    exit_code = 2;
                }
            }
        }
        Err(_) => findings.push(finding(
            "DOCTOR005",
            "document",
            "error",
            "Selected environment is not declared.",
            None,
        )),
    }
    if let Some(tools) = &tools {
        if tools.platform() != host_platform_triple() {
            findings.push(finding(
                "DOCTOR006",
                "host",
                "error",
                "Tool lock does not match the current host.",
                None,
            ));
        }
        for tool in tools.tools().values() {
            let native = tool.contract().adapter().is_none();
            let path = PathBuf::from(OsString::from_vec(tool.path().bytes().to_vec()));
            let observed = open_absolute_file_with_parent_nofollow(&path).and_then(|(_, file)| {
                let mode = file
                    .metadata()
                    .map_err(|_| ProjectError::new("PROJECT024", "tool metadata unavailable"))?
                    .mode();
                if mode & 0o111 == 0 || (native && mode & 0o6000 != 0) {
                    return Err(ProjectError::new("PROJECT024", "tool is not executable"));
                }
                let bytes =
                    filesystem.read_file_bounded(file, process::MAX_TOOL_EXECUTABLE_BYTES)?;
                if native {
                    process::validate_native_image(&bytes, &|| {
                        if control.is_cancelled() {
                            Err(opaal_runtime::operational::ModuleError::Cancelled(
                                opaal_runtime::eval::CancelReason::Requested,
                            ))
                        } else {
                            Ok(())
                        }
                    })
                    .map_err(|_| ProjectError::new("PROJECT024", "unsupported native image"))?;
                }
                Ok(bytes)
            });
            if !observed
                .as_ref()
                .is_ok_and(|bytes| digest_bytes(bytes) == tool.digest())
            {
                findings.push(finding(
                    "DOCTOR007",
                    "tool",
                    "error",
                    "Locked executable is unavailable or changed.",
                    None,
                ));
            }
        }
    }
    if !manifest.targets().is_empty() {
        if !matches!(
            host_platform_triple(),
            "x86_64-unknown-linux-gnu" | "aarch64-unknown-linux-gnu"
        ) {
            findings.push(finding(
                "DOCTOR008",
                "host",
                "error",
                "Live Docker inspection requires the qualified Linux host.",
                None,
            ));
        }
        for target in manifest.targets().values() {
            if docker::bind_target(&manifest, target, &PosixOperationalAdapter::new()).is_err() {
                findings.push(finding(
                    "DOCTOR009",
                    "tool",
                    "error",
                    "Local socket or empty directory identity is unavailable or invalid.",
                    None,
                ));
            }
        }
    }
    let project = match load_project_program_controlled(manifest, &filesystem, &filesystem, control)
    {
        Ok(project) => project,
        Err(error) => {
            match error.code() {
                "PROJECT022" => findings.push(finding(
                    "DOCTOR010",
                    "version",
                    "error",
                    "Project requires an incompatible OPAAL version.",
                    None,
                )),
                "PROJECT029" => {
                    return Ok(DoctorProjectRun {
                        output: Vec::new(),
                        exit_code: 130,
                    });
                }
                "PROJECT030" => {
                    return Err(execute_error(
                        "DOCTOR003",
                        "diagnosis analysis limit exceeded",
                    ));
                }
                _ => {
                    findings.push(invalid_document());
                    exit_code = 2;
                }
            }
            findings.push(unknown_checks());
            return render(
                request,
                Some(&project_name),
                None,
                findings,
                exit_code.max(1),
            );
        }
    };
    if filesystem
        .validate_output_path(project.manifest().evidence())
        .is_err()
        || build_tls_bindings(project.manifest(), &filesystem).is_err()
    {
        findings.push(invalid_document());
        exit_code = 2;
    }
    let task = match project.task(&request.task) {
        Ok(task) => task,
        Err(_) => {
            findings.push(finding(
                "DOCTOR011",
                "task",
                "error",
                "Selected task is not declared or has an unsupported signature.",
                None,
            ));
            findings.push(unknown_checks());
            return render(
                request,
                Some(&project_name),
                None,
                findings,
                exit_code.max(1),
            );
        }
    };
    let task_id = task.id().qualified_name();
    match build_secret_requirements(&project, task.action()) {
        Ok(secrets) if secrets.len() > 1 => findings.push(finding(
            "DOCTOR016",
            "authority",
            "error",
            "Task secret requirements exceed the supported cardinality.",
            None,
        )),
        Err(_) => findings.push(unknown_checks()),
        _ => {}
    }
    let parameters = task.action().callable().parameters();
    for parameter in parameters {
        let binding = request
            .inputs
            .iter()
            .find(|binding| binding.name() == parameter.name());
        let valid = binding.is_some_and(|binding| {
            validate_input_binding_modes(&project, &request.task, std::slice::from_ref(binding))
                .is_ok()
                && runtime_input_text(&project, &request.task, std::slice::from_ref(binding))
                    .is_ok_and(|inputs| {
                        opaal_runtime::project::validate_input_text(
                            parameter.value_type(),
                            &inputs[0].1,
                        )
                        .is_ok()
                    })
        });
        if !valid {
            findings.push(finding(
                "DOCTOR014",
                "input",
                "error",
                "Required input is missing or incompatible with its declared type.",
                Some(parameter.name()),
            ));
        }
    }
    for binding in &request.inputs {
        if !parameters
            .iter()
            .any(|parameter| parameter.name() == binding.name())
        {
            findings.push(finding(
                "DOCTOR015",
                "input",
                "error",
                "Input is not declared by the selected task.",
                Some(binding.name()),
            ));
        }
    }
    if let Some(authority) = &authority {
        for _ in opaal_runtime::project::check_task_authority(task, authority) {
            findings.push(finding(
                "DOCTOR012",
                "authority",
                "error",
                "Task effect has no compatible exact grant.",
                None,
            ));
        }
        if let Some(tools) = &tools {
            match reachable_action_closure(&project, task.action()) {
                Ok(actions) => {
                    for action in actions {
                        match project.action_effects(action) {
                            Ok(effects) => {
                                for effect in effects {
                                    if !matches!(
                                        host_platform_triple(),
                                        "x86_64-unknown-linux-gnu" | "aarch64-unknown-linux-gnu"
                                    )
                                        && effect.capability() == "process.run"
                                        && effect
                                            .scope()
                                            .strip_prefix("tool.")
                                            .and_then(|id| project.manifest().tools().get(id))
                                            .is_some_and(|tool| tool.contract().adapter().is_none())
                                        && !findings
                                            .iter()
                                            .any(|finding| finding["code"] == "DOCTOR006")
                                    {
                                        findings.push(finding(
                                            "DOCTOR006",
                                            "host",
                                            "error",
                                            "Native execution requires the qualified Linux host.",
                                            None,
                                        ));
                                    }
                                    if !matches!(
                                        static_verdict(
                                            project.manifest(),
                                            authority,
                                            &effect,
                                            tools.platform()
                                        ),
                                        "granted-enforced" | "granted-unenforced"
                                    ) {
                                        findings.push(finding(
                                            "DOCTOR012",
                                            "authority",
                                            "error",
                                            "Task effect has no compatible exact grant.",
                                            None,
                                        ));
                                    }
                                }
                            }
                            Err(_) => findings.push(unknown_checks()),
                        }
                    }
                }
                Err(_) => findings.push(unknown_checks()),
            }
        }
    }
    if let (Some(authority), Some(tools)) = (&authority, &tools) {
        let checked = validate_input_binding_modes(&project, &request.task, &request.inputs)
            .and_then(|()| runtime_input_text(&project, &request.task, &request.inputs))
            .and_then(|inputs| {
                check_project(
                    &project,
                    &request.task,
                    &request.environment,
                    authority,
                    tools,
                    inputs,
                )
                .map_err(frontend_contract)
            });
        match checked {
            Ok(check) => {
                if build_input_records(&check, &filesystem, &request.inputs).is_err() {
                    findings.push(finding(
                        "DOCTOR013",
                        "input",
                        "error",
                        "Declared input file is unavailable or exceeds its read limit.",
                        None,
                    ));
                }
            }
            Err(_) => {
                if !findings
                    .iter()
                    .any(|finding| finding["category"] == "input")
                {
                    findings.push(finding(
                    "DOCTOR014",
                    "input",
                    "error",
                    "Task inputs are missing, unknown or incompatible with their declared types.",
                    None,
                    ));
                }
                findings.push(unknown_checks());
            }
        }
    } else {
        findings.push(unknown_checks());
    }
    if control.is_cancelled() {
        return Ok(DoctorProjectRun {
            output: Vec::new(),
            exit_code: 130,
        });
    }
    findings.sort_by_key(|finding| {
        (
            finding["category"].as_str().unwrap().to_owned(),
            finding["code"].as_str().unwrap().to_owned(),
        )
    });
    let exit_code = if findings.is_empty() {
        0
    } else {
        exit_code.max(1)
    };
    render(
        request,
        Some(&project_name),
        Some(&task_id),
        findings,
        exit_code,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_limits_reject_first_excess_without_truncating_successful_json() {
        let request = CheckProjectRequest::new(
            PathBuf::from("opaal.toml"),
            "task".to_owned(),
            "offline".to_owned(),
            Vec::new(),
        )
        .with_json();
        let findings = vec![unknown_checks(); 128];
        let view = render(&request, None, None, findings.clone(), 1).unwrap();
        assert!(view.output.len() <= 64 * 1024);
        assert!(serde_json::from_slice::<Value>(&view.output).is_ok());
        let mut excessive = findings;
        excessive.push(unknown_checks());
        assert!(render(&request, None, None, excessive, 1).is_err());
        assert!(render(&request, Some(&"x".repeat(64 * 1024)), None, Vec::new(), 0).is_err());
    }
}
