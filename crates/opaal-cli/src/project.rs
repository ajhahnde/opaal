//! Explicit-path host frontend for non-executing project inspection and checks.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use opaal_platform::AuthorityEffect;
use opaal_platform::operational::{OperationalAdapter as _, supports_exact_process_execution};
use opaal_platform_posix::PosixPlatform;
use opaal_platform_posix::operational::PosixOperationalAdapter;
use opaal_runtime::Environment;
use opaal_runtime::authority::{
    AuthorityContext, AuthorityRule as RuntimeAuthorityRule, AuthorityVerdict, CapabilityRequest,
    CapabilityScope, EffectSet, EvaluationContextId, RequiredEnforcement,
};
use opaal_runtime::builtin::standard_registry;
use opaal_runtime::context::OperationalContext;
use opaal_runtime::eval::{CancellationToken, Instant, SystemClock};
use opaal_runtime::lifetime::{CleanupStatus, Deadline};
use opaal_runtime::module::{
    ActionId, ActionSignature, ModuleCanonicalizer, ModuleId, ModuleOrigin, ModulePathError,
    ModuleSourceError, ModuleSourceLoader, ValueType,
};
use opaal_runtime::operational::source::{
    ControlledSourceOperations, SourceActionOutcome, SourceEffectEvent, SourceEffectJournal,
    SourceEffectOutcome,
};
use opaal_runtime::operational::{data, process};
use opaal_runtime::outcome::{OutcomeEvidence, PrimaryOutcome};
use opaal_runtime::plan::SessionOptions;
use opaal_runtime::project::{
    AuthorityDecision, AuthorityDocument, DocumentEnforcement, MAX_PROJECT_DOCUMENT_BYTES,
    ProjectCheck, ProjectEffect, ProjectError, ProjectManifest, ProjectProgram, ToolLock,
    check_project, load_project_program, parse_authority_document, parse_project_manifest,
    parse_tool_lock,
};
use opaal_runtime::resolve::ExecutableProbe;
use opaal_runtime::script::execute_project_task_outcome;
use opaal_runtime::workflow::{
    CheckArtifact, JournalChain, digest_bytes, digest_value, native_path, path_from_native_value,
    unix_nanos_from_timestamp,
};
use opaal_runtime::workflow::{
    MAX_AUDIT_BYTES, MAX_JOURNAL_BYTES, PlanArtifact, audit_journal, timestamp_from_unix_nanos,
};
use serde_json::{Value, json};

const MAX_CONTROL_READ_BYTES: usize = 192 * 1024 * 1024;
type BuiltTlsBindings = (Vec<Value>, BTreeMap<String, Vec<u8>>);

#[derive(Clone, Debug, Default)]
struct ControlReadBudget {
    used: Arc<Mutex<usize>>,
}

impl ControlReadBudget {
    fn charge(&self, bytes: usize) -> Result<(), ProjectError> {
        let mut used = self
            .used
            .lock()
            .map_err(|_| ProjectError::new("PROJECT027", "control-read budget is unavailable"))?;
        let proposed = used
            .checked_add(bytes)
            .ok_or_else(|| ProjectError::new("PROJECT027", "control-read byte total overflow"))?;
        if proposed > MAX_CONTROL_READ_BYTES {
            return Err(ProjectError::new(
                "PROJECT027",
                "named control reads exceed 192 MiB",
            ));
        }
        *used = proposed;
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InspectProjectRequest {
    manifest: PathBuf,
    task: String,
}

impl InspectProjectRequest {
    #[must_use]
    pub fn new(manifest: PathBuf, task: String) -> Self {
        Self { manifest, task }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckProjectRequest {
    manifest: PathBuf,
    task: String,
    environment: String,
    inputs: Vec<ProjectInputBinding>,
    format_json: bool,
}

/// One explicit lexical-value or regular-file project input binding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProjectInputBinding {
    Value { name: String, value: OsString },
    File { name: String, path: PathBuf },
}

impl ProjectInputBinding {
    #[must_use]
    pub fn value(name: String, value: String) -> Self {
        Self::native_value(name, OsString::from(value))
    }

    #[must_use]
    pub fn native_value(name: String, value: OsString) -> Self {
        Self::Value { name, value }
    }

    #[must_use]
    pub fn file(name: String, path: PathBuf) -> Self {
        Self::File { name, path }
    }

    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Value { name, .. } | Self::File { name, .. } => name,
        }
    }
}

impl From<(String, String)> for ProjectInputBinding {
    fn from((name, value): (String, String)) -> Self {
        Self::value(name, value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanProjectRequest {
    manifest: PathBuf,
    task: String,
    environment: String,
    inputs: Vec<ProjectInputBinding>,
    expires_in_seconds: u64,
    out: PathBuf,
}

impl PlanProjectRequest {
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        manifest: PathBuf,
        task: String,
        environment: String,
        inputs: Vec<(String, String)>,
        expires_in_seconds: u64,
        out: PathBuf,
    ) -> Self {
        Self::with_bindings(
            manifest,
            task,
            environment,
            inputs.into_iter().map(Into::into).collect(),
            expires_in_seconds,
            out,
        )
    }

    #[must_use]
    pub fn with_bindings(
        manifest: PathBuf,
        task: String,
        environment: String,
        inputs: Vec<ProjectInputBinding>,
        expires_in_seconds: u64,
        out: PathBuf,
    ) -> Self {
        Self {
            manifest,
            task,
            environment,
            inputs,
            expires_in_seconds,
            out,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecuteProjectRequest {
    plan: PathBuf,
    accept: String,
    run_id: Option<String>,
    secret_stdin: Option<String>,
    journal: PathBuf,
}

impl ExecuteProjectRequest {
    #[must_use]
    pub fn new(
        plan: PathBuf,
        accept: String,
        run_id: Option<String>,
        secret_stdin: Option<String>,
        journal: PathBuf,
    ) -> Self {
        Self {
            plan,
            accept,
            run_id,
            secret_stdin,
            journal,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecuteFrontendRun {
    output: Vec<u8>,
    successful: bool,
}

impl ExecuteFrontendRun {
    #[must_use]
    pub fn output(&self) -> &[u8] {
        &self.output
    }

    #[must_use]
    pub const fn is_successful(&self) -> bool {
        self.successful
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuditRequest {
    project: PathBuf,
    journal: PathBuf,
    out: PathBuf,
}

impl AuditRequest {
    #[must_use]
    pub fn new(project: PathBuf, journal: PathBuf, out: PathBuf) -> Self {
        Self {
            project,
            journal,
            out,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuditFrontendRun {
    output: Vec<u8>,
    complete: bool,
}

impl AuditFrontendRun {
    #[must_use]
    pub fn output(&self) -> &[u8] {
        &self.output
    }

    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.complete
    }
}

impl CheckProjectRequest {
    #[must_use]
    pub fn new(
        manifest: PathBuf,
        task: String,
        environment: String,
        inputs: Vec<(String, String)>,
    ) -> Self {
        Self::with_bindings(
            manifest,
            task,
            environment,
            inputs.into_iter().map(Into::into).collect(),
        )
    }

    #[must_use]
    pub fn with_bindings(
        manifest: PathBuf,
        task: String,
        environment: String,
        inputs: Vec<ProjectInputBinding>,
    ) -> Self {
        Self {
            manifest,
            task,
            environment,
            inputs,
            format_json: false,
        }
    }

    /// Request the canonical `opaal.check.v2` representation on stdout.
    #[must_use]
    pub const fn with_json(mut self) -> Self {
        self.format_json = true;
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectFrontendRun {
    output: Vec<u8>,
    diagnostic: Vec<u8>,
    successful: bool,
}

impl ProjectFrontendRun {
    #[must_use]
    pub fn output(&self) -> &[u8] {
        &self.output
    }

    #[must_use]
    pub fn diagnostic(&self) -> &[u8] {
        &self.diagnostic
    }

    #[must_use]
    pub const fn is_successful(&self) -> bool {
        self.successful
    }
}

#[derive(Debug)]
pub struct ProjectFrontendError {
    rendered: String,
}

impl ProjectFrontendError {
    #[must_use]
    pub fn rendered(&self) -> &str {
        &self.rendered
    }
}

impl std::fmt::Display for ProjectFrontendError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.rendered)
    }
}

impl std::error::Error for ProjectFrontendError {}

pub fn inspect_project(
    request: &InspectProjectRequest,
) -> Result<ProjectFrontendRun, ProjectFrontendError> {
    let (project, _, _) = load_explicit_project(&request.manifest)?;
    let task = project.task(&request.task).map_err(frontend_contract)?;
    let callable = task.action().callable();
    let mut output = format!(
        "task {}\naction {}\nsignature {}(",
        task.id().qualified_name(),
        task.action().id().qualified_name(),
        callable.name(),
    );
    for (index, parameter) in callable.parameters().iter().enumerate() {
        if index != 0 {
            output.push_str(", ");
        }
        output.push_str(parameter.name());
        output.push_str(": ");
        output.push_str(&parameter.value_type().to_string());
    }
    output.push_str(") -> ");
    output.push_str(&callable.result().to_string());
    output.push('\n');
    output.push_str("contract sha256:");
    output.push_str(task.action().id().contract_digest());
    output.push('\n');
    if let Some(documentation) = callable.documentation()
        && !documentation.is_empty()
    {
        output.push_str("documentation\n");
        for line in documentation.text().lines() {
            output.push_str("  ");
            output.push_str(line);
            output.push('\n');
        }
    }
    output.push_str("effects\n");
    for effect in task.effects() {
        output.push_str("  ");
        output.push_str(effect.capability());
        output.push(' ');
        output.push_str(effect.scope());
        output.push('\n');
    }
    output.push_str("tools\n");
    for tool in project.manifest().tools().values() {
        output.push_str("  ");
        output.push_str(&tool.id().qualified_name());
        output.push(' ');
        output.push_str(tool.adapter().name());
        output.push(' ');
        output.push_str(&tool.version().to_string());
        output.push('\n');
    }
    output.push_str("environments\n");
    for environment in project.manifest().environments().values() {
        output.push_str("  ");
        output.push_str(&environment.id().qualified_name());
        output.push('\n');
    }
    Ok(ProjectFrontendRun {
        output: output.into_bytes(),
        diagnostic: Vec::new(),
        successful: true,
    })
}

pub fn check_explicit_project(
    request: &CheckProjectRequest,
) -> Result<ProjectFrontendRun, ProjectFrontendError> {
    let (project, filesystem, _) = load_explicit_project(&request.manifest)?;
    let environment = project
        .manifest()
        .environment(&request.environment)
        .map_err(frontend_contract)?;
    let authority_path = environment.authority().to_path_buf();
    let tools_path = environment.tool_lock().to_path_buf();
    let authority_bytes = filesystem
        .read_existing_bounded(&authority_path, MAX_PROJECT_DOCUMENT_BYTES)
        .map_err(frontend_contract)?;
    let tools_bytes = filesystem
        .read_existing_bounded(&tools_path, MAX_PROJECT_DOCUMENT_BYTES)
        .map_err(frontend_contract)?;
    let authority =
        parse_authority_document(project.manifest(), &request.environment, &authority_bytes)
            .map_err(frontend_contract)?;
    let tools = parse_tool_lock(project.manifest(), &request.environment, &tools_bytes)
        .map_err(frontend_contract)?;
    validate_input_binding_modes(&project, &request.task, &request.inputs)?;
    let runtime_inputs = runtime_input_text(&project, &request.task, &request.inputs)?;
    let check = check_project(
        &project,
        &request.task,
        &request.environment,
        &authority,
        &tools,
        runtime_inputs,
    )
    .map_err(frontend_contract)?;
    let artifact = build_check_artifact(
        &project,
        &filesystem,
        &check,
        &request.environment,
        &authority_path,
        &authority_bytes,
        &authority,
        &tools_bytes,
        &tools,
        &request.inputs,
    )?;
    let executable = artifact.artifact.is_executable();
    let output = if request.format_json {
        artifact.artifact.bytes().to_vec()
    } else {
        Vec::new()
    };
    let diagnostic = if executable || request.format_json {
        Vec::new()
    } else {
        render_check_artifact_findings(&artifact.artifact)
    };
    Ok(ProjectFrontendRun {
        output,
        diagnostic,
        successful: executable,
    })
}

/// Build and exclusively publish one identity-bound, non-executing project plan.
pub fn plan_explicit_project(
    request: &PlanProjectRequest,
) -> Result<ProjectFrontendRun, ProjectFrontendError> {
    if request.expires_in_seconds == 0 || request.expires_in_seconds > 900 {
        return Err(frontend_contract(ProjectError::new(
            "PLAN005",
            "plan expiry must be from 1 through 900 seconds",
        )));
    }
    let (project, filesystem, manifest_bytes) = load_explicit_project(&request.manifest)?;
    let environment = project
        .manifest()
        .environment(&request.environment)
        .map_err(frontend_contract)?;
    let authority_path = environment.authority().to_path_buf();
    let tools_path = environment.tool_lock().to_path_buf();
    let authority_bytes = filesystem
        .read_existing_bounded(&authority_path, MAX_PROJECT_DOCUMENT_BYTES)
        .map_err(frontend_contract)?;
    let tools_bytes = filesystem
        .read_existing_bounded(&tools_path, MAX_PROJECT_DOCUMENT_BYTES)
        .map_err(frontend_contract)?;
    let authority =
        parse_authority_document(project.manifest(), &request.environment, &authority_bytes)
            .map_err(frontend_contract)?;
    let tools = parse_tool_lock(project.manifest(), &request.environment, &tools_bytes)
        .map_err(frontend_contract)?;
    validate_input_binding_modes(&project, &request.task, &request.inputs)?;
    let runtime_inputs = runtime_input_text(&project, &request.task, &request.inputs)?;
    let check = check_project(
        &project,
        &request.task,
        &request.environment,
        &authority,
        &tools,
        runtime_inputs,
    )
    .map_err(frontend_contract)?;
    if tools.platform() != host_platform_triple() {
        return Err(frontend_contract(ProjectError::new(
            "PLAN006",
            format!(
                "tool lock platform `{}` differs from `{}`",
                tools.platform(),
                host_platform_triple()
            ),
        )));
    }
    let check_artifact = build_check_artifact(
        &project,
        &filesystem,
        &check,
        &request.environment,
        &authority_path,
        &authority_bytes,
        &authority,
        &tools_bytes,
        &tools,
        &request.inputs,
    )?;
    let mut executable_observations = Vec::new();
    let mut total_executable_bytes = 0usize;
    for (name, tool) in tools.tools() {
        let path = PathBuf::from(OsString::from_vec(tool.path().bytes().to_vec()));
        let (_, file) =
            open_absolute_file_with_parent_nofollow(&path).map_err(frontend_contract)?;
        let bytes = filesystem
            .read_file_bounded(file, 64 * 1024 * 1024)
            .map_err(frontend_contract)?;
        total_executable_bytes =
            total_executable_bytes
                .checked_add(bytes.len())
                .ok_or_else(|| {
                    frontend_contract(ProjectError::new("PLAN007", "tool byte total overflow"))
                })?;
        if total_executable_bytes > 128 * 1024 * 1024 {
            return Err(frontend_contract(ProjectError::new(
                "PLAN007",
                "tool executable bytes exceed 128 MiB",
            )));
        }
        let observed = digest_bytes(&bytes);
        if observed != tool.digest() {
            return Err(frontend_contract(ProjectError::new(
                "PLAN008",
                format!("locked tool `{name}` digest is stale"),
            )));
        }
        executable_observations.push(json!({
            "kind":"tool-executable",
            "id":name,
            "path":native_path(&path),
            "digest":observed,
            "size":bytes.len(),
            "observed_at":null
        }));
    }
    let wall_nanos = PosixOperationalAdapter::new()
        .wall_time_unix_nanos()
        .map_err(|error| frontend_contract(ProjectError::new("PLAN009", error.to_string())))?;
    let created_at = timestamp_from_unix_nanos(wall_nanos).map_err(workflow_contract)?;
    let expires_nanos = wall_nanos
        .checked_add(i128::from(request.expires_in_seconds) * 1_000_000_000)
        .ok_or_else(|| frontend_contract(ProjectError::new("PLAN005", "plan expiry overflow")))?;
    let expires_at = timestamp_from_unix_nanos(expires_nanos).map_err(workflow_contract)?;
    let check_value = check_artifact.artifact.value();
    let mut observations = Vec::new();
    observations.push(observation(
        "manifest",
        project.manifest().name(),
        Some(project.manifest().manifest_path()),
        Some(digest_bytes(&manifest_bytes)),
        Some(manifest_bytes.len()),
        None,
    ));
    for source in check_value["sources"]
        .as_array()
        .expect("validated check sources are an array")
    {
        observations.push(json!({
            "kind":"source",
            "id":source["module"],
            "path":source["path"],
            "digest":source["digest"],
            "size":source["size"],
            "observed_at":null
        }));
    }
    observations.push(observation(
        "authority",
        &request.environment,
        Some(&authority_path),
        Some(digest_bytes(&authority_bytes)),
        Some(authority_bytes.len()),
        None,
    ));
    observations.push(observation(
        "tool-lock",
        &request.environment,
        Some(&tools_path),
        Some(digest_bytes(&tools_bytes)),
        Some(tools_bytes.len()),
        None,
    ));
    for input in check_value["inputs"]
        .as_array()
        .expect("validated check inputs are an array")
        .iter()
        .filter(|input| input["binding"] == "file")
    {
        observations.push(json!({
            "kind":"input",
            "id":input["name"],
            "path":input["path"],
            "digest":input["digest"],
            "size":input["size"],
            "observed_at":null
        }));
    }
    for tls in check_value["project"]["tls"]
        .as_array()
        .expect("validated TLS bindings are an array")
    {
        let endpoint = required_plan_string(tls, "endpoint")?;
        let ca = check_artifact
            .tls_ca
            .get(endpoint)
            .expect("the check retains every validated TLS CA");
        observations.push(json!({
            "kind":"tls-ca",
            "id":tls["endpoint"],
            "path":tls["ca_path"],
            "digest":tls["ca_digest"],
            "size":ca.len(),
            "observed_at":null
        }));
    }
    observations.extend(executable_observations);
    observations.push(json!({
        "kind":"child-environment",
        "id":request.environment,
        "path":null,
        "digest":check_value["project"]["child_environment_digest"],
        "size":null,
        "observed_at":null
    }));
    observations.push(observation(
        "wall-clock",
        "created-at",
        None,
        None,
        None,
        Some(&created_at),
    ));
    let task = &check_value["task"];
    let actions = build_action_records(
        &project,
        &authority,
        check.task().action(),
        tools.platform(),
    )?;
    let executable = check_artifact.artifact.is_executable();
    let plan_outcome = if executable {
        success_outcome("PLAN000", "plan is executable")
    } else {
        refused_outcome("PLAN001", "static project check refused execution")
    };
    let document = json!({
        "schema":"opaal.plan.v2",
        "schema_version":2,
        "created_at":created_at,
        "expires_at":expires_at,
        "toolchain":check_value["toolchain"],
        "platform":{"triple":tools.platform()},
        "project":check_value["project"],
        "task":task,
        "inputs":check_value["inputs"],
        "secrets":check_value["secrets"],
        "sources":check_value["sources"],
        "authority":check_value["authority"],
        "tools":check_value["tools"],
        "observations":observations,
        "actions":actions,
        "outcome":plan_outcome
    });
    let plan = PlanArtifact::seal(document).map_err(workflow_contract)?;
    filesystem.write_exclusive_atomic(&request.out, plan.bytes())?;
    Ok(ProjectFrontendRun {
        output: format!(
            "plan {}{}\n",
            plan.digest(),
            if executable { "" } else { " refused" }
        )
        .into_bytes(),
        diagnostic: if executable {
            Vec::new()
        } else {
            render_check_artifact_findings(&check_artifact.artifact)
        },
        successful: executable,
    })
}

fn render_check_artifact_findings(artifact: &CheckArtifact) -> Vec<u8> {
    artifact.value()["findings"]
        .as_array()
        .expect("validated check findings are an array")
        .iter()
        .map(|finding| {
            format!(
                "opaal: {}: {}\n",
                finding["code"]
                    .as_str()
                    .expect("validated finding code is text"),
                finding["message"]
                    .as_str()
                    .expect("validated finding message is text")
            )
        })
        .collect::<String>()
        .into_bytes()
}

fn reachable_action_closure<'a>(
    project: &'a ProjectProgram,
    root: &'a ActionSignature,
) -> Result<Vec<&'a ActionSignature>, ProjectFrontendError> {
    fn visit<'a>(
        project: &'a ProjectProgram,
        action: &'a ActionSignature,
        seen: &mut std::collections::BTreeSet<ActionId>,
        ordered: &mut Vec<&'a ActionSignature>,
    ) -> Result<(), ProjectFrontendError> {
        if !seen.insert(action.id().clone()) {
            return Ok(());
        }
        ordered.push(action);
        for dependency in action.dependencies() {
            let resolved = project
                .modules()
                .actions()
                .action(dependency.module(), dependency.name())
                .filter(|candidate| candidate.id() == dependency)
                .ok_or_else(|| {
                    frontend_contract(ProjectError::new(
                        "PLAN011",
                        "an analyzed action dependency cannot be resolved",
                    ))
                })?;
            visit(project, resolved, seen, ordered)?;
        }
        Ok(())
    }

    let mut seen = std::collections::BTreeSet::new();
    let mut ordered = Vec::new();
    visit(project, root, &mut seen, &mut ordered)?;
    Ok(ordered)
}

fn build_action_records(
    project: &ProjectProgram,
    authority: &AuthorityDocument,
    root: &ActionSignature,
    platform: &str,
) -> Result<Vec<Value>, ProjectFrontendError> {
    let reachable = reachable_action_closure(project, root)?;
    let node_ids = reachable
        .iter()
        .enumerate()
        .map(|(ordinal, action)| {
            (
                action.id().clone(),
                format!("sha256:{}#{ordinal:06}", action.id().contract_digest()),
            )
        })
        .collect::<BTreeMap<_, _>>();
    reachable
        .iter()
        .enumerate()
        .map(|(ordinal, action)| {
            let mut requests = project
                .action_effects(action)
                .map_err(frontend_contract)?
                .iter()
                .map(|effect| artifact_requests(project.manifest(), authority, effect, platform))
                .collect::<Result<Vec<_>, ProjectFrontendError>>()?
                .into_iter()
                .flatten()
                .collect::<Vec<_>>();
            sort_values_canonical(&mut requests);
            requests.dedup();
            let mut dependencies = action
                .dependencies()
                .iter()
                .map(|dependency| {
                    node_ids.get(dependency).cloned().ok_or_else(|| {
                        frontend_contract(ProjectError::new(
                            "PLAN011",
                            "reachable action dependency is absent from the plan closure",
                        ))
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            dependencies.sort();
            let executable = requests.iter().all(|request| {
                matches!(
                    request["verdict"].as_str(),
                    Some("granted-enforced" | "granted-unenforced")
                )
            });
            Ok(json!({
                "id":node_ids[action.id()],
                "ordinal":ordinal,
                "action_id":action.id().qualified_name(),
                "contract_digest":qualified_digest(action.id().contract_digest()),
                "requests":requests,
                "dependencies":dependencies,
                "outcome":if executable {
                    success_outcome("PLAN000", "action is executable")
                } else {
                    refused_outcome("PLAN001", "action has a refused authority request")
                }
            }))
        })
        .collect()
}

fn sort_values_canonical(values: &mut [Value]) {
    values.sort_by(|left, right| {
        serde_json::to_vec(left)
            .expect("JSON values are serializable")
            .cmp(&serde_json::to_vec(right).expect("JSON values are serializable"))
    });
}

/// Revalidate and execute one exact accepted project plan.
pub fn execute_explicit_plan(
    request: &ExecuteProjectRequest,
    secret_input_is_terminal: bool,
    secret_input: &mut dyn Read,
) -> Result<ExecuteFrontendRun, ProjectFrontendError> {
    let control_reads = ControlReadBudget::default();
    let plan_path = absolute_lexical(&request.plan).map_err(frontend_contract)?;
    let (_, plan_file) =
        open_absolute_file_with_parent_nofollow(&plan_path).map_err(frontend_contract)?;
    let plan_bytes = read_bounded_control(
        plan_file
            .try_clone()
            .map_err(|error| execute_error("EXECUTE_STALE", error.to_string()))?,
        opaal_runtime::workflow::MAX_ARTIFACT_BYTES,
        &control_reads,
    )
    .map_err(frontend_contract)?;
    let plan = PlanArtifact::parse(&plan_bytes).map_err(workflow_contract)?;
    if request.accept != plan.digest() {
        return Err(execute_error(
            "EXECUTE002",
            "--accept does not equal the exact plan digest",
        ));
    }
    if !plan.is_executable() {
        return Err(execute_error("EXECUTE003", "a refused plan cannot execute"));
    }

    let plan_value = plan.value();
    let project_value = &plan_value["project"];
    let planned_root = path_from_native_value(&project_value["root"]).map_err(workflow_contract)?;
    let manifest_path =
        path_from_native_value(&project_value["manifest_path"]).map_err(workflow_contract)?;
    let (project, filesystem, manifest_bytes) =
        load_explicit_project_with_budget(&manifest_path, control_reads)?;
    if filesystem.root != planned_root {
        return Err(stale("project root changed"));
    }
    let resolved_plan = filesystem
        .resolve_existing_file(&plan_path)
        .map_err(frontend_contract)?;
    if resolved_plan != plan_path {
        return Err(stale("plan path identity changed"));
    }
    let bound_plan_file = filesystem
        .open_existing_file(&plan_path)
        .map_err(frontend_contract)?;
    if !same_file_identity(&plan_file, &bound_plan_file).map_err(frontend_contract)? {
        return Err(stale("plan file changed while binding its project root"));
    }
    let journal_path = filesystem
        .resolve_output_path(&request.journal)
        .map_err(frontend_contract)?;
    if journal_path == plan_path {
        return Err(execute_error(
            "EXECUTE004",
            "journal path must differ from the accepted plan",
        ));
    }

    let environment_name = required_plan_string(project_value, "environment")?;
    let task_name = required_plan_string(&plan_value["task"], "id")?
        .rsplit_once("::")
        .map_or_else(
            || required_plan_string(&plan_value["task"], "id").map(str::to_owned),
            |(_, name)| Ok(name.to_owned()),
        )?;
    let planned_authority_path =
        path_from_native_value(&plan_value["authority"]["path"]).map_err(workflow_contract)?;
    let authority_path = filesystem
        .resolve_existing_file(&planned_authority_path)
        .map_err(frontend_contract)?;
    let tool_lock_observation = plan_value["observations"]
        .as_array()
        .and_then(|values| values.iter().find(|value| value["kind"] == "tool-lock"))
        .ok_or_else(|| stale("plan has no tool-lock observation"))?;
    let tool_lock_path =
        path_from_native_value(&tool_lock_observation["path"]).map_err(workflow_contract)?;
    let tool_lock_path = filesystem
        .resolve_existing_file(&tool_lock_path)
        .map_err(frontend_contract)?;
    let environment = project
        .manifest()
        .environment(environment_name)
        .map_err(frontend_contract)?;
    if authority_path != environment.authority() || tool_lock_path != environment.tool_lock() {
        return Err(stale(
            "authority or tool-lock path differs from the selected environment",
        ));
    }

    let authority_bytes = filesystem
        .read_existing_bounded(&authority_path, MAX_PROJECT_DOCUMENT_BYTES)
        .map_err(frontend_contract)?;
    let tools_bytes = filesystem
        .read_existing_bounded(&tool_lock_path, MAX_PROJECT_DOCUMENT_BYTES)
        .map_err(frontend_contract)?;
    let authority =
        parse_authority_document(project.manifest(), environment_name, &authority_bytes)
            .map_err(frontend_contract)?;
    let tools = parse_tool_lock(project.manifest(), environment_name, &tools_bytes)
        .map_err(frontend_contract)?;
    let input_bindings = plan_input_bindings(plan_value)?;
    let runtime_inputs = runtime_input_text(&project, &task_name, &input_bindings)?;
    let check = check_project(
        &project,
        &task_name,
        environment_name,
        &authority,
        &tools,
        runtime_inputs,
    )
    .map_err(frontend_contract)?;
    let current_check = build_check_artifact(
        &project,
        &filesystem,
        &check,
        environment_name,
        &authority_path,
        &authority_bytes,
        &authority,
        &tools_bytes,
        &tools,
        &input_bindings,
    )?;
    let executable_files = verify_plan_identity(
        &plan,
        current_check.artifact.value(),
        &manifest_bytes,
        &authority_path,
        &authority_bytes,
        &tool_lock_path,
        &tools_bytes,
        &current_check.tls_ca,
        &filesystem.control_reads,
    )?;

    let adapter = PosixOperationalAdapter::new();
    let now_nanos = adapter
        .wall_time_unix_nanos()
        .map_err(|error| execute_error("EXECUTE005", error.to_string()))?;
    let created_nanos = unix_nanos_from_timestamp(plan.created_at()).map_err(workflow_contract)?;
    let expires_nanos = unix_nanos_from_timestamp(plan.expires_at()).map_err(workflow_contract)?;
    if expires_nanos <= created_nanos
        || expires_nanos - created_nanos > 900_000_000_000
        || now_nanos >= expires_nanos
    {
        return Err(execute_error(
            "EXECUTE006",
            "accepted plan is expired or has an invalid expiry interval",
        ));
    }
    if plan_value["toolchain"]["version"] != opaal_runtime::version()
        || plan_value["platform"]["triple"] != host_platform_triple()
    {
        return Err(stale("toolchain or platform changed"));
    }
    validate_secret_binding(
        plan_value,
        request.secret_stdin.as_deref(),
        secret_input_is_terminal,
    )?;
    let tls_ca = current_check.tls_ca;
    let expected_actions = build_action_records(
        &project,
        &authority,
        check.task().action(),
        tools.platform(),
    )?;
    if plan_value["actions"] != Value::Array(expected_actions) {
        return Err(stale("plan action closure changed"));
    }
    let reachable = reachable_action_closure(&project, check.task().action())?;
    let planned_actions = plan_value["actions"]
        .as_array()
        .expect("validated plan actions are an array");
    let action_nodes = reachable
        .iter()
        .zip(planned_actions)
        .map(|(action, node)| {
            Ok((
                action.id().clone(),
                required_plan_string(node, "id")?.to_owned(),
            ))
        })
        .collect::<Result<BTreeMap<_, _>, ProjectFrontendError>>()?;
    let action_node_id = required_plan_string(&planned_actions[0], "id")?;

    let run_id = request.run_id.clone().map_or_else(generate_run_id, Ok)?;
    evaluation_id(&run_id)?;
    let project_digest = digest_value(project_value).map_err(workflow_contract)?;
    let started_nanos = adapter
        .wall_time_unix_nanos()
        .map_err(|error| execute_error("EXECUTE005", error.to_string()))?;
    if started_nanos >= expires_nanos {
        return Err(execute_error(
            "EXECUTE006",
            "accepted plan expired during identity verification",
        ));
    }
    let started_at = timestamp_from_unix_nanos(started_nanos).map_err(workflow_contract)?;
    let header = json!({
        "plan_digest":plan.digest(),
        "accepted_plan_digest":request.accept,
        "authority_digest":digest_bytes(&authority_bytes),
        "project_digest":project_digest,
        "environment_digest":project_value["environment_digest"],
        "tool_lock_digest":digest_bytes(&tools_bytes),
        "child_environment_digest":project_value["child_environment_digest"],
        "started_at":started_at
    });
    let mut journal = filesystem.create_journal(&journal_path, &run_id, header)?;
    let mut runtime_rules = Vec::new();
    for rule in authority.rules() {
        runtime_rules.extend(runtime_authority_rules(project.manifest(), rule)?);
    }
    let evaluation = evaluation_id(&run_id)?;
    let execution_clock = SystemClock::new();
    let execution_deadline = Instant::from_nanos(10 * 60 * 1_000_000_000);
    let cancellation = CancellationToken::deadline(execution_clock.clone(), execution_deadline);
    let operational_clock = Arc::new(execution_clock.clone());
    let mut operational = OperationalContext::new(
        AuthorityContext::new(evaluation, runtime_rules)
            .map_err(|error| execute_error("EXECUTE007", error.to_string()))?,
        cancellation.clone(),
        operational_clock,
        Some(Deadline::at(execution_deadline)),
    );
    let effects = EffectSet::new(
        check
            .task()
            .effects()
            .iter()
            .map(|effect| runtime_requests(project.manifest(), effect.capability(), effect.scope()))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten(),
    );
    let platform = PosixPlatform;
    let required_tools = check
        .task()
        .effects()
        .iter()
        .filter(|effect| effect.capability() == "process.run")
        .filter_map(|effect| effect.scope().strip_prefix("tool."))
        .collect::<BTreeSet<_>>();
    let mut process_budget = process::ProcessBudget::default();
    for tool_id in tools
        .tools()
        .keys()
        .filter(|tool_id| required_tools.contains(tool_id.as_str()))
    {
        let attempt = process_budget.attempts();
        let scope = json!({"kind":"tool","tool":tool_id});
        journal.append(
            "effect-before",
            json!({
                "action_node_id":action_node_id,
                "effect":"process.run",
                "scope":scope,
                "verdict":"granted-unenforced",
                "attempt":attempt
            }),
        )?;
        let before = process_budget.attempts();
        let executable_file = executable_files
            .get(tool_id)
            .ok_or_else(|| stale(format!("tool executable `{tool_id}` is absent")))?;
        match process::probe_retained(
            &operational,
            &effects,
            &platform,
            &adapter,
            &tools,
            tool_id,
            project.manifest().root(),
            &mut process_budget,
            executable_file,
        ) {
            Ok(result) => {
                let evidence = digest_value(&json!({
                    "stdout":digest_bytes(result.stdout()),
                    "stderr":digest_bytes(result.stderr())
                }))
                .map_err(workflow_contract)?;
                journal.append(
                    "effect-after",
                    json!({
                        "action_node_id":action_node_id,
                        "effect":"process.run",
                        "scope":scope,
                        "attempt":attempt,
                        "outcome":success_outcome("EXECUTE_PROBE", "maintained tool probe succeeded"),
                        "evidence_digest":evidence
                    }),
                )?;
            }
            Err(error) => {
                let partial = process_budget.attempts() > before;
                let outcome = outcome_value(
                    "refused",
                    "EXECUTE_STALE",
                    &operational.redact_text(&error.to_string()),
                    None,
                    None,
                    partial,
                );
                journal.append(
                    "effect-after",
                    json!({
                        "action_node_id":action_node_id,
                        "effect":"process.run",
                        "scope":scope,
                        "attempt":attempt,
                        "outcome":outcome,
                        "evidence_digest":null
                    }),
                )?;
                return finish_execution_journal(&mut journal, outcome, Vec::new(), &adapter);
            }
        }
    }

    let arguments = execution_argument_values(&check, plan_value)?;
    let planned_inputs = plan_value["inputs"]
        .as_array()
        .expect("validated plan inputs are an array")
        .iter()
        .filter(|input| input["binding"] == "file")
        .map(|input| {
            Ok((
                path_from_native_value(&input["path"]).map_err(workflow_contract)?,
                required_plan_string(input, "digest")?.to_owned(),
            ))
        })
        .collect::<Result<BTreeMap<_, _>, ProjectFrontendError>>()?;
    let mut environment = Environment::new();
    let mut action_output = Vec::new();
    let outcome = {
        let mut effect_journal = RuntimeEffectJournal {
            journal: &mut journal,
        };
        let secret_binding = request
            .secret_stdin
            .clone()
            .map(|id| (id, &mut *secret_input as &mut dyn Read));
        let mut operations = ControlledSourceOperations::new(
            &mut operational,
            &effects,
            project.manifest(),
            &tools,
            &executable_files,
            &platform,
            &adapter,
            &tls_ca,
            &mut effect_journal,
            action_nodes,
            planned_inputs,
            process_budget,
            secret_binding,
        );
        execute_project_task_outcome(
            &project,
            check.task(),
            arguments,
            project.manifest().root(),
            &mut environment,
            &standard_registry(),
            &NoExecutableProbe,
            &SessionOptions::default(),
            &platform,
            Arc::new(execution_clock),
            cancellation,
            &mut operations,
            &mut action_output,
        )
    };
    let partial = match outcome.primary() {
        PrimaryOutcome::Completed(_) => outcome
            .evidence()
            .iter()
            .any(|evidence| matches!(evidence, OutcomeEvidence::PartialEffect(_))),
        _ => !outcome.evidence().is_empty(),
    };
    let primary_value = script_outcome_value(outcome.primary(), &operational, partial);
    let (primary, evidence, _) = outcome.into_parts();
    let finished = operational.finish(primary, evidence);
    let cleanup = append_cleanup_events(&mut journal, finished.downstream().cleanup())?;
    finish_execution_journal(&mut journal, primary_value, cleanup, &adapter)
}

fn verify_plan_check_identity(plan: &Value, check: &Value) -> Result<(), ProjectFrontendError> {
    for field in [
        "toolchain",
        "project",
        "task",
        "inputs",
        "secrets",
        "sources",
        "authority",
        "tools",
    ] {
        if plan[field] != check[field] {
            return Err(stale(format!("bound {field} identity changed")));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn verify_plan_identity(
    plan: &PlanArtifact,
    check: &Value,
    manifest_bytes: &[u8],
    authority_path: &Path,
    authority_bytes: &[u8],
    tool_lock_path: &Path,
    tools_bytes: &[u8],
    tls_ca: &BTreeMap<String, Vec<u8>>,
    control_reads: &ControlReadBudget,
) -> Result<BTreeMap<String, File>, ProjectFrontendError> {
    let value = plan.value();
    verify_plan_check_identity(value, check)?;
    let project = &check["project"];
    let mut expected = vec![observation(
        "manifest",
        required_plan_string(project, "name")?,
        Some(&path_from_native_value(&project["manifest_path"]).map_err(workflow_contract)?),
        Some(digest_bytes(manifest_bytes)),
        Some(manifest_bytes.len()),
        None,
    )];
    for source in check["sources"]
        .as_array()
        .expect("validated check sources are an array")
    {
        expected.push(json!({
            "kind":"source",
            "id":source["module"],
            "path":source["path"],
            "digest":source["digest"],
            "size":source["size"],
            "observed_at":null
        }));
    }
    expected.push(observation(
        "authority",
        required_plan_string(project, "environment")?,
        Some(authority_path),
        Some(digest_bytes(authority_bytes)),
        Some(authority_bytes.len()),
        None,
    ));
    expected.push(observation(
        "tool-lock",
        required_plan_string(project, "environment")?,
        Some(tool_lock_path),
        Some(digest_bytes(tools_bytes)),
        Some(tools_bytes.len()),
        None,
    ));
    for input in check["inputs"]
        .as_array()
        .expect("validated check inputs are an array")
        .iter()
        .filter(|input| input["binding"] == "file")
    {
        expected.push(json!({
            "kind":"input",
            "id":input["name"],
            "path":input["path"],
            "digest":input["digest"],
            "size":input["size"],
            "observed_at":null
        }));
    }
    for tls in project["tls"]
        .as_array()
        .expect("validated project TLS bindings are an array")
    {
        let endpoint = required_plan_string(tls, "endpoint")?;
        let ca = tls_ca
            .get(endpoint)
            .ok_or_else(|| stale(format!("TLS CA `{endpoint}` is absent")))?;
        expected.push(json!({
            "kind":"tls-ca",
            "id":tls["endpoint"],
            "path":tls["ca_path"],
            "digest":digest_bytes(ca),
            "size":ca.len(),
            "observed_at":null
        }));
    }
    let mut executable_files = BTreeMap::new();
    for tool in check["tools"]
        .as_array()
        .expect("validated check tools are an array")
    {
        let path = path_from_native_value(&tool["path"]).map_err(workflow_contract)?;
        let (_, file) =
            open_absolute_file_with_parent_nofollow(&path).map_err(frontend_contract)?;
        let bytes = read_bounded_control(
            file.try_clone()
                .map_err(|error| execute_error("EXECUTE_STALE", error.to_string()))?,
            process::MAX_TOOL_EXECUTABLE_BYTES,
            control_reads,
        )
        .map_err(frontend_contract)?;
        if digest_bytes(&bytes) != tool["digest"] {
            return Err(stale(format!(
                "tool executable `{}` changed",
                required_plan_string(tool, "id")?
            )));
        }
        expected.push(json!({
            "kind":"tool-executable",
            "id":tool["id"],
            "path":tool["path"],
            "digest":digest_bytes(&bytes),
            "size":bytes.len(),
            "observed_at":null
        }));
        executable_files.insert(required_plan_string(tool, "id")?.to_owned(), file);
    }
    expected.push(json!({
        "kind":"child-environment",
        "id":project["environment"],
        "path":null,
        "digest":project["child_environment_digest"],
        "size":null,
        "observed_at":null
    }));
    expected.push(observation(
        "wall-clock",
        "created-at",
        None,
        None,
        None,
        Some(plan.created_at()),
    ));
    if value["observations"] != Value::Array(expected) {
        return Err(stale(
            "plan observations disagree with their bound identities",
        ));
    }
    Ok(executable_files)
}

fn plan_input_bindings(plan: &Value) -> Result<Vec<ProjectInputBinding>, ProjectFrontendError> {
    plan["inputs"]
        .as_array()
        .expect("validated plan inputs are an array")
        .iter()
        .map(|input| {
            let name = required_plan_string(input, "name")?.to_owned();
            match required_plan_string(input, "binding")? {
                "value" if input["path"].is_object() => {
                    let path = path_from_native_value(&input["path"]).map_err(workflow_contract)?;
                    Ok(ProjectInputBinding::native_value(
                        name,
                        path.into_os_string(),
                    ))
                }
                "value" => Ok(ProjectInputBinding::value(
                    name,
                    required_plan_string(input, "value")?.to_owned(),
                )),
                "file" => Ok(ProjectInputBinding::file(
                    name,
                    path_from_native_value(&input["path"]).map_err(workflow_contract)?,
                )),
                _ => Err(stale("plan input has an unsupported binding mode")),
            }
        })
        .collect()
}

fn execution_argument_values(
    check: &ProjectCheck,
    plan: &Value,
) -> Result<Vec<opaal_runtime::Value>, ProjectFrontendError> {
    let mut arguments = check.argument_values().map_err(frontend_contract)?;
    let inputs = plan["inputs"]
        .as_array()
        .expect("validated plan inputs are an array");
    for (index, parameter) in check
        .task()
        .action()
        .callable()
        .parameters()
        .iter()
        .enumerate()
    {
        if parameter.value_type() != &ValueType::Path {
            continue;
        }
        let input = inputs
            .iter()
            .find(|input| input["name"] == parameter.name())
            .expect("validated plan inputs match task parameters");
        let path = path_from_native_value(&input["path"]).map_err(workflow_contract)?;
        arguments[index] =
            opaal_runtime::Value::from(opaal_runtime::NativePath::new(path.into_os_string()));
    }
    Ok(arguments)
}

fn runtime_authority_rules(
    manifest: &ProjectManifest,
    rule: &opaal_runtime::project::AuthorityDocumentRule,
) -> Result<Vec<RuntimeAuthorityRule>, ProjectFrontendError> {
    runtime_requests(manifest, rule.effect(), rule.scope())?
        .into_iter()
        .map(|request| {
            Ok(match (rule.decision(), rule.required_enforcement()) {
                (AuthorityDecision::Deny, _) => RuntimeAuthorityRule::deny(request),
                (AuthorityDecision::Grant, Some(DocumentEnforcement::Enforced)) => {
                    RuntimeAuthorityRule::grant(request, RequiredEnforcement::Enforced)
                }
                (AuthorityDecision::Grant, Some(DocumentEnforcement::AcknowledgeUnenforced)) => {
                    RuntimeAuthorityRule::grant(request, RequiredEnforcement::AcknowledgeUnenforced)
                }
                (AuthorityDecision::Grant, None) => {
                    return Err(execute_error(
                        "EXECUTE007",
                        "grant rule has no required enforcement",
                    ));
                }
            })
        })
        .collect()
}

fn runtime_requests(
    manifest: &ProjectManifest,
    effect: &str,
    scope: &str,
) -> Result<Vec<CapabilityRequest>, ProjectFrontendError> {
    let requests = match (effect, scope) {
        ("filesystem.read", "project.root") => {
            vec![CapabilityRequest::filesystem_read(manifest.root())]
        }
        ("filesystem.write", "project.evidence") => {
            vec![CapabilityRequest::filesystem_write(manifest.evidence())]
        }
        ("process.run", scope) => vec![CapabilityRequest::process_run(
            scope
                .strip_prefix("tool.")
                .ok_or_else(|| execute_error("EXECUTE007", "invalid tool authority scope"))?,
        )],
        ("network.http", scope) => {
            let endpoint_id = scope
                .strip_prefix("endpoint.")
                .ok_or_else(|| execute_error("EXECUTE007", "invalid endpoint authority scope"))?;
            let endpoint = manifest
                .endpoints()
                .get(endpoint_id)
                .ok_or_else(|| execute_error("EXECUTE007", "unknown endpoint authority scope"))?;
            endpoint
                .methods()
                .iter()
                .map(|method| CapabilityRequest::network_http(endpoint_id, method))
                .collect()
        }
        ("secret.reveal", scope) => {
            let binding = scope
                .strip_prefix("secret.")
                .ok_or_else(|| execute_error("EXECUTE007", "invalid secret authority scope"))?;
            let (secret, endpoint_id) = binding
                .split_once("@endpoint.")
                .ok_or_else(|| execute_error("EXECUTE007", "invalid secret authority scope"))?;
            let endpoint = manifest.endpoints().get(endpoint_id).ok_or_else(|| {
                execute_error("EXECUTE007", "unknown secret endpoint authority scope")
            })?;
            endpoint
                .secret_headers()
                .iter()
                .map(|header| CapabilityRequest::secret_reveal(secret, endpoint_id, header))
                .collect()
        }
        ("clock.wall", "evaluation") => vec![Ok(CapabilityRequest::clock_wall())],
        ("clock.monotonic", "evaluation") => vec![Ok(CapabilityRequest::clock_monotonic())],
        _ => {
            return Err(execute_error(
                "EXECUTE007",
                format!("unsupported runtime request `{effect}` `{scope}`"),
            ));
        }
    };
    requests
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| execute_error("EXECUTE007", error.to_string()))
}

fn validate_secret_binding(
    plan: &Value,
    supplied: Option<&str>,
    secret_input_is_terminal: bool,
) -> Result<(), ProjectFrontendError> {
    let required = plan["secrets"]
        .as_array()
        .expect("validated plan secrets are an array");
    match (required.as_slice(), supplied) {
        ([], None) => Ok(()),
        ([], Some(_)) => Err(execute_error(
            "EXECUTE008",
            "--secret-stdin is not accepted for a secret-free plan",
        )),
        ([requirement], Some(supplied)) if requirement["id"].as_str() != Some(supplied) => {
            Err(execute_error(
                "EXECUTE008",
                "--secret-stdin must name the plan's one injected secret",
            ))
        }
        ([_], Some(_)) if secret_input_is_terminal => Err(execute_error(
            "EXECUTE001",
            "secret stdin must not be a terminal",
        )),
        ([_], Some(_)) => Ok(()),
        ([_], _) => Err(execute_error(
            "EXECUTE008",
            "--secret-stdin must name the plan's one injected secret",
        )),
        _ => Err(execute_error(
            "EXECUTE008",
            "a plan with unsupported secret cardinality cannot execute",
        )),
    }
}

fn generate_run_id() -> Result<String, ProjectFrontendError> {
    let mut random = File::open("/dev/urandom").map_err(run_id_generation_error)?;
    generate_run_id_from(&mut random)
}

fn generate_run_id_from(random: &mut dyn Read) -> Result<String, ProjectFrontendError> {
    let mut bytes = [0_u8; 16];
    random
        .read_exact(&mut bytes)
        .map_err(run_id_generation_error)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn run_id_generation_error(error: std::io::Error) -> ProjectFrontendError {
    execute_error("EXECUTE010", format!("run id generation failed: {error}"))
}

fn evaluation_id(run_id: &str) -> Result<EvaluationContextId, ProjectFrontendError> {
    if run_id.len() != 32
        || !run_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(execute_error(
            "EXECUTE010",
            "run id must be 32 lowercase hexadecimal digits",
        ));
    }
    let identity = digest_bytes(run_id.as_bytes());
    let numeric = u64::from_str_radix(&identity["sha256:".len().."sha256:".len() + 16], 16)
        .map_err(|error| execute_error("EXECUTE010", error.to_string()))?
        .max(1);
    Ok(EvaluationContextId::new(numeric).expect("the evaluation id is nonzero"))
}

fn script_outcome_value(
    primary: &PrimaryOutcome<
        opaal_runtime::script::ScriptCompletion,
        opaal_runtime::script::ScriptError,
    >,
    operational: &OperationalContext,
    partial: bool,
) -> Value {
    match primary {
        PrimaryOutcome::Completed(completion) => {
            if let Some(status) = completion.status()
                && !status.is_ok()
            {
                return outcome_value(
                    "status",
                    "EXECUTE_STATUS",
                    "task policy returned an unsuccessful status",
                    status.code().and_then(|code| u64::try_from(code).ok()),
                    None,
                    partial,
                );
            }
            let value_digest = data::json_encode(completion.value())
                .ok()
                .map(|bytes| digest_bytes(&bytes));
            outcome_value(
                "success",
                "EXECUTE000",
                "project task completed successfully",
                None,
                value_digest,
                partial,
            )
        }
        PrimaryOutcome::Error(error) => outcome_value(
            "error",
            "EXECUTE_LANGUAGE",
            &operational.redact_text(error.render()),
            None,
            None,
            partial,
        ),
        PrimaryOutcome::Cancelled(cancellation) => outcome_value(
            "cancelled",
            "EXECUTE_CANCELLED",
            &format!("task was cancelled: {:?}", cancellation.reason()),
            None,
            None,
            partial,
        ),
        PrimaryOutcome::Refused(refusal) => outcome_value(
            "refused",
            "EXECUTE_REFUSED",
            &refusal.to_string(),
            None,
            None,
            partial,
        ),
        PrimaryOutcome::FatalHostFailure(failure) => outcome_value(
            "error",
            "EXECUTE_HOST",
            &operational.redact_text(&failure.to_string()),
            None,
            None,
            partial,
        ),
    }
}

fn outcome_value(
    class: &str,
    code: &str,
    message: &str,
    status: Option<u64>,
    value_digest: Option<String>,
    partial: bool,
) -> Value {
    let message = bounded_public_message(message);
    json!({
        "class":class,
        "code":code,
        "message":message,
        "status":status,
        "value_digest":value_digest,
        "partial":partial
    })
}

fn finish_execution_journal(
    journal: &mut SyncedJournal,
    primary: Value,
    cleanup: Vec<Value>,
    adapter: &dyn opaal_platform::operational::OperationalAdapter,
) -> Result<ExecuteFrontendRun, ProjectFrontendError> {
    let class = required_plan_string(&primary, "class")?.to_owned();
    let cleanup_failed = cleanup
        .iter()
        .any(|outcome| outcome["class"] == "cleanup-failed");
    let finished_at = adapter
        .wall_time_unix_nanos()
        .map_err(|error| execute_error("EXECUTE005", error.to_string()))?;
    journal.append(
        "terminal",
        json!({
            "finished_at":timestamp_from_unix_nanos(finished_at).map_err(workflow_contract)?,
            "primary":primary,
            "cleanup":cleanup,
            "complete":true
        }),
    )?;
    Ok(ExecuteFrontendRun {
        output: format!("run {} {class}\n", journal.run_id).into_bytes(),
        successful: class == "success" && !cleanup_failed,
    })
}

fn append_cleanup_events(
    journal: &mut SyncedJournal,
    outcomes: &[opaal_runtime::lifetime::CleanupOutcome],
) -> Result<Vec<Value>, ProjectFrontendError> {
    let mut values = Vec::with_capacity(outcomes.len());
    for cleanup in outcomes {
        let resource = cleanup.resource();
        let outcome = match cleanup.status() {
            CleanupStatus::Succeeded => success_outcome("CLEANUP000", "resource cleanup succeeded"),
            CleanupStatus::Failed(message) => {
                outcome_value("cleanup-failed", "CLEANUP001", message, None, None, false)
            }
        };
        journal.append(
            "cleanup",
            json!({
                "action_node_id":null,
                "resource_id":format!(
                    "resource-{:016x}-{:06}",
                    resource.owner().evaluation().get(),
                    resource.ordinal()
                ),
                "ordinal":resource.ordinal(),
                "outcome":outcome
            }),
        )?;
        values.push(outcome);
    }
    Ok(values)
}

fn bounded_public_message(message: &str) -> String {
    const MAX_BYTES: usize = 4 * 1024;
    if message.len() <= MAX_BYTES {
        return message.to_owned();
    }
    let mut end = MAX_BYTES;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    let mut bounded = message[..end].to_owned();
    bounded.push('…');
    bounded
}

fn required_plan_string<'a>(value: &'a Value, name: &str) -> Result<&'a str, ProjectFrontendError> {
    value
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| execute_error("EXECUTE011", format!("plan field `{name}` is not a string")))
}

fn execute_error(code: &'static str, message: impl Into<String>) -> ProjectFrontendError {
    ProjectFrontendError {
        rendered: format!("opaal: {code}: {}\n", message.into()),
    }
}

fn stale(message: impl Into<String>) -> ProjectFrontendError {
    execute_error("EXECUTE_STALE", message)
}

struct NoExecutableProbe;

impl ExecutableProbe for NoExecutableProbe {
    fn is_executable(&self, _path: &OsStr) -> bool {
        false
    }
}

struct SyncedJournal {
    run_id: String,
    file: File,
    chain: JournalChain,
    failed: bool,
}

struct RuntimeEffectJournal<'journal> {
    journal: &'journal mut SyncedJournal,
}

impl SourceEffectJournal for RuntimeEffectJournal<'_> {
    fn action_start(&mut self, action_node_id: &str, action: &ActionId) -> Result<(), String> {
        self.journal
            .append(
                "action-start",
                json!({
                    "action_node_id":action_node_id,
                    "action_id":action.qualified_name(),
                    "contract_digest":qualified_digest(action.contract_digest())
                }),
            )
            .map_err(|error| error.rendered)
    }

    fn action_end(
        &mut self,
        action_node_id: &str,
        outcome: &SourceActionOutcome,
    ) -> Result<(), String> {
        let outcome = outcome.outcome();
        self.journal
            .append(
                "action-end",
                json!({
                    "action_node_id":action_node_id,
                    "outcome":outcome_value(
                        outcome.class(),
                        outcome.code(),
                        outcome.message(),
                        outcome.status().and_then(|status| u64::try_from(status).ok()),
                        outcome.value_digest().map(str::to_owned),
                        outcome.partial(),
                    )
                }),
            )
            .map_err(|error| error.rendered)
    }

    fn before(
        &mut self,
        event: &SourceEffectEvent,
        verdict: AuthorityVerdict,
    ) -> Result<(), String> {
        self.journal
            .append(
                "effect-before",
                json!({
                    "action_node_id":event.action_node_id(),
                    "effect":authority_effect_name(event.request().effect()),
                    "scope":runtime_scope_value(event.request()),
                    "verdict":authority_verdict_name(verdict),
                    "attempt":event.attempt()
                }),
            )
            .map_err(|error| error.rendered)
    }

    fn after(
        &mut self,
        event: &SourceEffectEvent,
        outcome: &SourceEffectOutcome,
    ) -> Result<(), String> {
        self.journal
            .append(
                "effect-after",
                json!({
                    "action_node_id":event.action_node_id(),
                    "effect":authority_effect_name(event.request().effect()),
                    "scope":runtime_scope_value(event.request()),
                    "attempt":event.attempt(),
                    "outcome":outcome_value(
                        outcome.class(),
                        outcome.code(),
                        outcome.message(),
                        outcome.status().and_then(|status| u64::try_from(status).ok()),
                        outcome.value_digest().map(str::to_owned),
                        outcome.partial(),
                    ),
                    "evidence_digest":outcome.value_digest()
                }),
            )
            .map_err(|error| error.rendered)
    }
}

fn authority_effect_name(effect: AuthorityEffect) -> &'static str {
    match effect {
        AuthorityEffect::FilesystemRead => "filesystem.read",
        AuthorityEffect::FilesystemWrite => "filesystem.write",
        AuthorityEffect::ProcessRun => "process.run",
        AuthorityEffect::NetworkHttp => "network.http",
        AuthorityEffect::SecretReveal => "secret.reveal",
        AuthorityEffect::ClockWall => "clock.wall",
        AuthorityEffect::ClockMonotonic => "clock.monotonic",
    }
}

fn authority_verdict_name(verdict: AuthorityVerdict) -> &'static str {
    match verdict {
        AuthorityVerdict::Denied => "denied",
        AuthorityVerdict::Unknown => "unknown",
        AuthorityVerdict::Unsupported => "unsupported",
        AuthorityVerdict::GrantedEnforced => "granted-enforced",
        AuthorityVerdict::GrantedUnenforced => "granted-unenforced",
    }
}

fn runtime_scope_value(request: &CapabilityRequest) -> Value {
    match request.scope() {
        CapabilityScope::ProjectPath(path) => {
            json!({"kind":"project-path","path":native_path(path)})
        }
        CapabilityScope::Tool(tool) => json!({"kind":"tool","tool":tool}),
        CapabilityScope::Endpoint { endpoint, method } => {
            json!({"kind":"endpoint","endpoint":endpoint,"method":method})
        }
        CapabilityScope::SecretSink {
            secret,
            endpoint,
            header,
        } => json!({
            "kind":"secret-sink",
            "secret":secret,
            "endpoint":endpoint,
            "header":header
        }),
        CapabilityScope::Evaluation => json!({
            "kind":"clock",
            "clock":match request.effect() {
                AuthorityEffect::ClockWall => "wall",
                AuthorityEffect::ClockMonotonic => "monotonic",
                _ => "evaluation",
            }
        }),
    }
}

impl SyncedJournal {
    fn append(&mut self, kind: &str, payload: Value) -> Result<(), ProjectFrontendError> {
        if self.failed {
            return Err(execute_error(
                "JOURNAL005",
                "journal is unavailable after a persistence failure",
            ));
        }
        let mut next = self.chain.clone();
        let line = next.append(kind, payload).map_err(workflow_contract)?;
        if let Err(error) = self
            .file
            .write_all(&line)
            .and_then(|()| self.file.sync_all())
        {
            self.failed = true;
            return Err(execute_error("JOURNAL005", error.to_string()));
        }
        self.chain = next;
        Ok(())
    }
}

/// Verify one project-contained journal and exclusively publish its audit.
pub fn audit_explicit_journal(
    request: &AuditRequest,
) -> Result<AuditFrontendRun, ProjectFrontendError> {
    let control_reads = ControlReadBudget::default();
    let (_, filesystem, _) =
        load_explicit_manifest_root_with_budget(&request.project, control_reads)?;
    let journal_path = filesystem
        .resolve_existing_file(&request.journal)
        .map_err(frontend_contract)?;
    let out_path = filesystem
        .resolve_output_path(&request.out)
        .map_err(frontend_contract)?;
    if journal_path == out_path {
        return Err(frontend_contract(ProjectError::new(
            "AUDIT001",
            "--out must differ from the explicit journal",
        )));
    }
    let bytes = filesystem
        .read_existing_bounded(&journal_path, MAX_JOURNAL_BYTES)
        .map_err(frontend_contract)?;
    let audit = audit_journal(&bytes).map_err(workflow_contract)?;
    if audit.bytes().len() > MAX_AUDIT_BYTES {
        return Err(frontend_contract(ProjectError::new(
            "AUDIT002",
            "audit output exceeds its byte limit",
        )));
    }
    filesystem.write_exclusive_atomic_kind(&out_path, audit.bytes(), "audit")?;
    Ok(AuditFrontendRun {
        output: format!(
            "audit {} {}\n",
            audit.digest(),
            if audit.is_complete() {
                "complete"
            } else {
                "incomplete"
            }
        )
        .into_bytes(),
        complete: audit.is_complete(),
    })
}

fn observation(
    kind: &str,
    id: &str,
    path: Option<&Path>,
    digest: Option<String>,
    size: Option<usize>,
    observed_at: Option<&str>,
) -> Value {
    json!({
        "kind":kind,
        "id":id,
        "path":path.map_or(Value::Null, native_path),
        "digest":digest.map_or(Value::Null, Value::String),
        "size":size.map_or(Value::Null, Value::from),
        "observed_at":observed_at.map_or(Value::Null, |value| Value::String(value.to_owned()))
    })
}

fn host_platform_triple() -> &'static str {
    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    {
        "aarch64-apple-darwin"
    }
    #[cfg(all(target_arch = "x86_64", target_os = "macos"))]
    {
        "x86_64-apple-darwin"
    }
    #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
    {
        "x86_64-unknown-linux-gnu"
    }
    #[cfg(all(target_arch = "aarch64", target_os = "linux"))]
    {
        "aarch64-unknown-linux-gnu"
    }
    #[cfg(not(any(
        all(target_arch = "aarch64", target_os = "macos"),
        all(target_arch = "x86_64", target_os = "macos"),
        all(target_arch = "x86_64", target_os = "linux"),
        all(target_arch = "aarch64", target_os = "linux")
    )))]
    {
        "unsupported"
    }
}

struct BuiltCheckArtifact {
    artifact: CheckArtifact,
    tls_ca: BTreeMap<String, Vec<u8>>,
}

#[allow(clippy::too_many_arguments)]
fn build_check_artifact(
    project: &ProjectProgram,
    filesystem: &HostProjectFilesystem,
    check: &ProjectCheck,
    environment_name: &str,
    authority_path: &Path,
    authority_bytes: &[u8],
    authority: &AuthorityDocument,
    tools_bytes: &[u8],
    tools: &ToolLock,
    input_bindings: &[ProjectInputBinding],
) -> Result<BuiltCheckArtifact, ProjectFrontendError> {
    let manifest = project.manifest();
    let child_environment = tools
        .variables()
        .iter()
        .map(|variable| {
            json!({
                "name":variable.name(),
                "value":{
                    "encoding":"base64url-nopad",
                    "platform":variable.value().platform(),
                    "value":variable.value().encoded()
                }
            })
        })
        .collect::<Vec<_>>();
    let child_environment_digest =
        digest_value(&Value::Array(child_environment)).map_err(workflow_contract)?;
    let selected_environment = manifest
        .environment(environment_name)
        .map_err(frontend_contract)?;
    let environment_digest = digest_value(&json!({
        "authority":native_path(selected_environment.authority()),
        "tool_lock":native_path(selected_environment.tool_lock())
    }))
    .map_err(workflow_contract)?;
    let (tls, tls_ca) = build_tls_bindings(manifest, filesystem)?;
    let project_value = json!({
        "name":manifest.name(),
        "root":native_path(manifest.root()),
        "manifest_path":native_path(manifest.manifest_path()),
        "manifest_digest":qualified_digest(manifest.id().manifest_digest()),
        "environment":environment_name,
        "environment_digest":environment_digest,
        "tool_lock_digest":digest_bytes(tools_bytes),
        "child_environment_digest":child_environment_digest,
        "tls":tls
    });
    let task = check.task();
    let task_value = json!({
        "id":task.id().qualified_name(),
        "action_id":task.action().id().qualified_name(),
        "contract_digest":qualified_digest(task.action().id().contract_digest())
    });
    let inputs = build_input_records(check, filesystem, input_bindings)?;
    let secrets = build_secret_requirements(project, task.action())?;
    let mut sources = project
        .modules()
        .sources()
        .entries()
        .filter(|entry| matches!(entry.module().origin(), ModuleOrigin::Local))
        .map(|entry| {
            json!({
                "module":entry.module().path().to_string_lossy(),
                "path":native_path(entry.module().path()),
                "digest":digest_bytes(entry.source().text().as_bytes()),
                "size":entry.source().len()
            })
        })
        .collect::<Vec<_>>();
    sources.sort_by(|left, right| left["module"].as_str().cmp(&right["module"].as_str()));
    let mut requests = reachable_action_closure(project, task.action())?
        .into_iter()
        .map(|action| project.action_effects(action).map_err(frontend_contract))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .map(|effect| artifact_requests(manifest, authority, &effect, tools.platform()))
        .collect::<Result<Vec<_>, ProjectFrontendError>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    sort_values_canonical(&mut requests);
    requests.dedup();
    let requests_executable = requests.iter().all(|request| {
        matches!(
            request["verdict"].as_str(),
            Some("granted-enforced" | "granted-unenforced")
        )
    });
    let mut rules = authority
        .rules()
        .iter()
        .map(|rule| {
            let enforcement = match rule.required_enforcement() {
                None => Value::Null,
                Some(DocumentEnforcement::Enforced) => Value::String("enforced".to_owned()),
                Some(DocumentEnforcement::AcknowledgeUnenforced) => {
                    Value::String("acknowledged-unenforced".to_owned())
                }
            };
            Ok(scope_values(manifest, rule.effect(), rule.scope())?
                .into_iter()
                .map(|scope| json!({
                    "decision":match rule.decision() { AuthorityDecision::Deny => "deny", AuthorityDecision::Grant => "grant" },
                    "effect":rule.effect(),
                    "scope":scope,
                    "required_enforcement":enforcement.clone()
                }))
                .collect::<Vec<_>>())
        })
        .collect::<Result<Vec<_>, ProjectFrontendError>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    sort_values_canonical(&mut rules);
    let authority_value = json!({
        "path":native_path(authority_path),
        "digest":digest_bytes(authority_bytes),
        "rules":rules,
        "requests":requests
    });
    let tool_values = build_tool_records(manifest, tools, &child_environment_digest)?;
    let mut findings = check
        .findings()
        .iter()
        .map(|finding| {
            json!({
                "severity":"error",
                "code":finding.code(),
                "message":finding.message(),
                "source":null,
                "path":null,
                "start_byte":null,
                "end_byte":null
            })
        })
        .collect::<Vec<_>>();
    if secrets.len() > 1 {
        findings.push(json!({
            "severity":"error",
            "code":"CHECK010",
            "message":"task requires unsupported secret cardinality; zero or one exact secret sink is supported",
            "source":null,
            "path":null,
            "start_byte":null,
            "end_byte":null
        }));
    }
    if task
        .effects()
        .iter()
        .any(|effect| effect.capability() == "process.run")
    {
        let (code, message) = match tools.platform() {
            "aarch64-apple-darwin" | "x86_64-apple-darwin" => (
                "CHECK008",
                "maintained process execution is unsupported on this platform",
            ),
            platform if !supports_exact_process_execution(platform) => (
                "CHECK009",
                "maintained process enforcement is unknown on this platform",
            ),
            _ => ("", ""),
        };
        if !code.is_empty() {
            findings.push(json!({
                "severity":"error",
                "code":code,
                "message":message,
                "source":null,
                "path":null,
                "start_byte":null,
                "end_byte":null
            }));
        }
    }
    findings.sort_by_key(|finding| {
        serde_json::to_vec(&json!([
            finding["severity"],
            finding["code"],
            finding["source"],
            finding["path"],
            finding["start_byte"],
            finding["end_byte"],
            finding["message"]
        ]))
        .expect("finding values are serializable")
    });
    let outcome = if findings.is_empty() && requests_executable {
        success_outcome("CHECK000", "project task check succeeded")
    } else {
        refused_outcome("CHECK005", "project task authority check refused execution")
    };
    let document = json!({
        "schema":"opaal.check.v2",
        "schema_version":2,
        "toolchain":{"version":opaal_runtime::version()},
        "project":project_value,
        "task":task_value,
        "inputs":inputs,
        "secrets":secrets,
        "sources":sources,
        "authority":authority_value,
        "tools":tool_values,
        "findings":findings,
        "outcome":outcome
    });
    let artifact = CheckArtifact::seal(document).map_err(workflow_contract)?;
    Ok(BuiltCheckArtifact { artifact, tls_ca })
}

fn build_input_records(
    check: &ProjectCheck,
    filesystem: &HostProjectFilesystem,
    bindings: &[ProjectInputBinding],
) -> Result<Vec<Value>, ProjectFrontendError> {
    let parameters = check.task().action().callable().parameters();
    let mut total_path_bytes = 0usize;
    let mut records = Vec::with_capacity(bindings.len());
    for binding in bindings {
        let name = binding.name();
        let parameter = parameters
            .iter()
            .find(|parameter| parameter.name() == name)
            .expect("project checking validated every input name");
        if let ProjectInputBinding::File { path: value, .. } = binding {
            if parameter.value_type() != &ValueType::Path {
                return Err(frontend_contract(ProjectError::new(
                    "CHECK009",
                    format!("--input-file `{name}` requires a Path task parameter"),
                )));
            }
            let path = filesystem
                .resolve_existing_file(value)
                .map_err(frontend_contract)?;
            let bytes = filesystem
                .read_existing_bounded(&path, opaal_runtime::operational::MAX_FILE_BYTES)
                .map_err(frontend_contract)?;
            total_path_bytes = total_path_bytes.checked_add(bytes.len()).ok_or_else(|| {
                frontend_contract(ProjectError::new("CHECK007", "input byte total overflow"))
            })?;
            if total_path_bytes > 32 * 1024 * 1024 {
                return Err(frontend_contract(ProjectError::new(
                    "CHECK007",
                    "named input bytes exceed 32 MiB",
                )));
            }
            records.push(json!({
                "name":name,
                "type":parameter.value_type().to_string(),
                "binding":"file",
                "value":null,
                "path":native_path(&path),
                "digest":digest_bytes(&bytes),
                "size":bytes.len()
            }));
            continue;
        }
        let ProjectInputBinding::Value { value, .. } = binding else {
            unreachable!("file bindings were handled above")
        };
        let (value_field, path_field) = if parameter.value_type() == &ValueType::Path {
            (Value::Null, native_path(Path::new(value)))
        } else {
            let value = value.to_str().ok_or_else(|| {
                frontend_contract(ProjectError::new(
                    "CHECK008",
                    format!("input `{name}` must be valid UTF-8 for its declared type"),
                ))
            })?;
            (Value::String(value.to_owned()), Value::Null)
        };
        records.push(json!({
            "name":name,
            "type":parameter.value_type().to_string(),
            "binding":"value",
            "value":value_field,
            "path":path_field,
            "digest":null,
            "size":null
        }));
    }
    records.sort_by(|left, right| left["name"].as_str().cmp(&right["name"].as_str()));
    Ok(records)
}

fn runtime_input_text(
    project: &ProjectProgram,
    task_name: &str,
    bindings: &[ProjectInputBinding],
) -> Result<Vec<(String, String)>, ProjectFrontendError> {
    let task = project.task(task_name).map_err(frontend_contract)?;
    bindings
        .iter()
        .map(|binding| {
            let name = binding.name();
            let parameter_type = task
                .action()
                .callable()
                .parameters()
                .iter()
                .find(|parameter| parameter.name() == name)
                .map(|parameter| parameter.value_type());
            match binding {
                ProjectInputBinding::Value { value, .. }
                    if parameter_type == Some(&ValueType::Path) =>
                {
                    if value.as_bytes().contains(&0) {
                        return Err(frontend_contract(ProjectError::new(
                            "CHECK008",
                            format!("input `{name}` is not an exact Path value: path contains NUL"),
                        )));
                    }
                    Ok((name.to_owned(), value.to_string_lossy().into_owned()))
                }
                ProjectInputBinding::Value { value, .. } => value
                    .to_str()
                    .map(|value| (name.to_owned(), value.to_owned()))
                    .ok_or_else(|| {
                        frontend_contract(ProjectError::new(
                            "CHECK008",
                            format!("input `{name}` must be valid UTF-8 for its declared type"),
                        ))
                    }),
                ProjectInputBinding::File { path, .. } => {
                    Ok((name.to_owned(), path.to_string_lossy().into_owned()))
                }
            }
        })
        .collect()
}

fn validate_input_binding_modes(
    project: &ProjectProgram,
    task_name: &str,
    bindings: &[ProjectInputBinding],
) -> Result<(), ProjectFrontendError> {
    let task = project.task(task_name).map_err(frontend_contract)?;
    for binding in bindings {
        let ProjectInputBinding::File { name, .. } = binding else {
            continue;
        };
        if let Some(parameter) = task
            .action()
            .callable()
            .parameters()
            .iter()
            .find(|parameter| parameter.name() == name)
            && parameter.value_type() != &ValueType::Path
        {
            return Err(frontend_contract(ProjectError::new(
                "CHECK009",
                format!("--input-file `{name}` requires a Path task parameter"),
            )));
        }
    }
    Ok(())
}

fn build_secret_requirements(
    project: &ProjectProgram,
    root: &ActionSignature,
) -> Result<Vec<Value>, ProjectFrontendError> {
    let mut requirements = Vec::new();
    let mut seen = BTreeSet::new();
    for (index, action) in reachable_action_closure(project, root)?
        .into_iter()
        .enumerate()
    {
        let mut seen_in_action = BTreeSet::new();
        for effect in project.action_effects(action).map_err(frontend_contract)? {
            if effect.capability() != "secret.reveal" {
                continue;
            }
            let binding = effect.scope().strip_prefix("secret.").ok_or_else(|| {
                frontend_contract(ProjectError::new("CHECK010", "invalid secret requirement"))
            })?;
            let (secret, endpoint) = binding.split_once("@endpoint.").ok_or_else(|| {
                frontend_contract(ProjectError::new("CHECK010", "invalid secret requirement"))
            })?;
            let endpoint = project
                .manifest()
                .endpoints()
                .get(endpoint)
                .ok_or_else(|| {
                    frontend_contract(ProjectError::new("CHECK010", "unknown secret endpoint"))
                })?;
            for header in endpoint.secret_headers() {
                let requirement = json!({"id":secret,"endpoint":endpoint.id(),"header":header});
                let key = serde_json::to_vec(&requirement)
                    .expect("secret requirement values are serializable");
                let repeated_in_action = !seen_in_action.insert(key.clone());
                if seen.insert(key) || index == 0 || repeated_in_action {
                    requirements.push(requirement);
                }
            }
        }
    }
    sort_values_canonical(&mut requirements);
    Ok(requirements)
}

fn build_tls_bindings(
    manifest: &ProjectManifest,
    filesystem: &HostProjectFilesystem,
) -> Result<BuiltTlsBindings, ProjectFrontendError> {
    let mut bindings = Vec::new();
    let mut retained = BTreeMap::new();
    let mut total_ca_bytes = 0usize;
    for endpoint in manifest
        .endpoints()
        .values()
        .filter(|endpoint| endpoint.tls())
    {
        let ca_path = endpoint.ca().ok_or_else(|| {
            frontend_contract(ProjectError::new(
                "PROJECT017",
                "TLS endpoint has no CA identity",
            ))
        })?;
        let ca = filesystem
            .read_existing_bounded(ca_path, 8 * 1024 * 1024)
            .map_err(frontend_contract)?;
        total_ca_bytes = total_ca_bytes.checked_add(ca.len()).ok_or_else(|| {
            frontend_contract(ProjectError::new("CHECK008", "TLS CA byte total overflow"))
        })?;
        if total_ca_bytes > 8 * 1024 * 1024 {
            return Err(frontend_contract(ProjectError::new(
                "CHECK008",
                "TLS CA bytes exceed 8 MiB",
            )));
        }
        let mut methods = endpoint.methods().to_vec();
        methods.sort();
        let mut headers = endpoint.secret_headers().to_vec();
        headers.sort();
        bindings.push(json!({
            "endpoint":endpoint.id(),
            "origin":endpoint.url(),
            "methods":methods,
            "secret_headers":headers,
            "ca_path":native_path(ca_path),
            "ca_digest":digest_bytes(&ca),
            "server_name":endpoint.tls_server_name().unwrap_or("")
        }));
        retained.insert(endpoint.id().to_owned(), ca);
    }
    Ok((bindings, retained))
}

fn build_tool_records(
    manifest: &ProjectManifest,
    tools: &ToolLock,
    child_environment_digest: &str,
) -> Result<Vec<Value>, ProjectFrontendError> {
    tools
        .tools()
        .iter()
        .map(|(name, locked)| {
            let declaration = manifest.tools().get(name).ok_or_else(|| {
                frontend_contract(ProjectError::new(
                    "PROJECT015",
                    format!("tool lock contains unknown tool `{name}`"),
                ))
            })?;
            let path = PathBuf::from(OsString::from_vec(locked.path().bytes().to_vec()));
            Ok(json!({
                "id":name,
                "adapter":locked.adapter().name(),
                "path":native_path(&path),
                "required_version":declaration.version().to_string(),
                "locked_version":locked.version().to_string(),
                "digest":locked.digest(),
                "platform":tools.platform(),
                "child_environment_digest":child_environment_digest,
                "version_verified":false
            }))
        })
        .collect()
}

fn static_verdict(
    authority: &AuthorityDocument,
    effect: &ProjectEffect,
    platform: &str,
) -> &'static str {
    authority
        .rules()
        .iter()
        .find(|rule| rule.effect() == effect.capability() && rule.scope() == effect.scope())
        .map_or("denied", |rule| {
            match (rule.decision(), rule.required_enforcement()) {
                (AuthorityDecision::Deny, _) => "denied",
                (AuthorityDecision::Grant, Some(DocumentEnforcement::AcknowledgeUnenforced))
                    if effect.capability() == "process.run" =>
                {
                    if supports_exact_process_execution(platform) {
                        "granted-unenforced"
                    } else if matches!(platform, "aarch64-apple-darwin" | "x86_64-apple-darwin") {
                        "unsupported"
                    } else {
                        "unknown"
                    }
                }
                (AuthorityDecision::Grant, Some(DocumentEnforcement::Enforced))
                    if effect.capability() != "process.run" =>
                {
                    "granted-enforced"
                }
                (AuthorityDecision::Grant, _) => "denied",
            }
        })
}

fn artifact_requests(
    manifest: &ProjectManifest,
    authority: &AuthorityDocument,
    effect: &ProjectEffect,
    platform: &str,
) -> Result<Vec<Value>, ProjectFrontendError> {
    Ok(scope_values(manifest, effect.capability(), effect.scope())?
        .into_iter()
        .map(|scope| {
            json!({
                "effect":effect.capability(),
                "scope":scope,
                "verdict":static_verdict(authority, effect, platform)
            })
        })
        .collect())
}

fn scope_values(
    manifest: &ProjectManifest,
    effect: &str,
    scope: &str,
) -> Result<Vec<Value>, ProjectFrontendError> {
    if scope == "project.root" {
        return Ok(vec![
            json!({"kind":"project-path","path":native_path(manifest.root())}),
        ]);
    }
    if scope == "project.evidence" {
        return Ok(vec![
            json!({"kind":"project-path","path":native_path(manifest.evidence())}),
        ]);
    }
    if let Some(tool) = scope.strip_prefix("tool.") {
        return Ok(vec![json!({"kind":"tool","tool":tool})]);
    }
    if let Some(endpoint_id) = scope.strip_prefix("endpoint.") {
        let endpoint = manifest.endpoints().get(endpoint_id).ok_or_else(|| {
            frontend_contract(ProjectError::new("ACT005", "unknown endpoint scope"))
        })?;
        return Ok(endpoint
            .methods()
            .iter()
            .map(|method| json!({"kind":"endpoint","endpoint":endpoint.id(),"method":method}))
            .collect());
    }
    if let Some(binding) = scope.strip_prefix("secret.") {
        let (secret, endpoint_id) = binding.split_once("@endpoint.").ok_or_else(|| {
            frontend_contract(ProjectError::new("ACT005", "invalid secret sink scope"))
        })?;
        let endpoint = manifest.endpoints().get(endpoint_id).ok_or_else(|| {
            frontend_contract(ProjectError::new("ACT005", "unknown secret endpoint"))
        })?;
        return Ok(endpoint
            .secret_headers()
            .iter()
            .map(|header| {
                json!({"kind":"secret-sink","secret":secret,"endpoint":endpoint_id,"header":header})
            })
            .collect());
    }
    if scope == "evaluation" && effect == "clock.wall" {
        return Ok(vec![json!({"kind":"clock","clock":"wall"})]);
    }
    if scope == "evaluation" && effect == "clock.monotonic" {
        return Ok(vec![json!({"kind":"clock","clock":"monotonic"})]);
    }
    Err(frontend_contract(ProjectError::new(
        "ACT005",
        format!("unsupported workflow scope `{scope}` for `{effect}`"),
    )))
}

fn success_outcome(code: &str, message: &str) -> Value {
    json!({
        "class":"success",
        "code":code,
        "message":message,
        "status":null,
        "value_digest":null,
        "partial":false
    })
}

fn refused_outcome(code: &str, message: &str) -> Value {
    json!({
        "class":"refused",
        "code":code,
        "message":message,
        "status":null,
        "value_digest":null,
        "partial":false
    })
}

fn qualified_digest(value: &str) -> String {
    if value.starts_with("sha256:") {
        value.to_owned()
    } else {
        format!("sha256:{value}")
    }
}

fn workflow_contract(
    error: opaal_runtime::workflow::WorkflowArtifactError,
) -> ProjectFrontendError {
    ProjectFrontendError {
        rendered: format!("opaal: {error}\n"),
    }
}

fn load_explicit_project(
    requested: &Path,
) -> Result<(ProjectProgram, HostProjectFilesystem, Vec<u8>), ProjectFrontendError> {
    load_explicit_project_with_budget(requested, ControlReadBudget::default())
}

fn load_explicit_project_with_budget(
    requested: &Path,
    control_reads: ControlReadBudget,
) -> Result<(ProjectProgram, HostProjectFilesystem, Vec<u8>), ProjectFrontendError> {
    let (manifest, filesystem, manifest_bytes) =
        load_explicit_manifest_root_with_budget(requested, control_reads)?;
    filesystem
        .resolve_existing_file(manifest.root_module())
        .map_err(frontend_contract)?;
    filesystem
        .validate_output_path(manifest.evidence())
        .map_err(frontend_contract)?;
    for endpoint in manifest.endpoints().values() {
        if let Some(ca) = endpoint.ca() {
            filesystem
                .resolve_existing_file(ca)
                .map_err(frontend_contract)?;
        }
    }
    let program = load_project_program(manifest, &filesystem, &filesystem).map_err(|error| {
        let rendered = match error {
            opaal_runtime::project::ProjectProgramError::Module(error) => error.render().to_owned(),
            opaal_runtime::project::ProjectProgramError::Contract(error) => {
                format!("opaal: {error}\n")
            }
        };
        ProjectFrontendError { rendered }
    })?;
    Ok((program, filesystem, manifest_bytes))
}

fn load_explicit_manifest_root_with_budget(
    requested: &Path,
    control_reads: ControlReadBudget,
) -> Result<(ProjectManifest, HostProjectFilesystem, Vec<u8>), ProjectFrontendError> {
    if requested.file_name().and_then(|name| name.to_str()) != Some("opaal.toml") {
        return Err(frontend_contract(ProjectError::new(
            "PROJECT023",
            "--project must name an explicit opaal.toml file",
        )));
    }
    let manifest_path = absolute_lexical(requested).map_err(frontend_contract)?;
    let (manifest_parent, manifest_file) =
        open_absolute_file_with_parent_nofollow(&manifest_path).map_err(frontend_contract)?;
    let manifest_bytes =
        read_bounded_control(manifest_file, MAX_PROJECT_DOCUMENT_BYTES, &control_reads)
            .map_err(frontend_contract)?;
    let manifest =
        parse_project_manifest(&manifest_path, &manifest_bytes).map_err(frontend_contract)?;
    let manifest_directory = manifest_path
        .parent()
        .expect("an absolute manifest path has a parent");
    let root_relative = manifest
        .root()
        .strip_prefix(manifest_directory)
        .map_err(|_| {
            frontend_contract(ProjectError::new(
                "PROJECT006",
                "project root escapes the retained manifest directory",
            ))
        })?;
    let root_directory =
        open_relative_directory_nofollow(manifest_parent, root_relative, manifest.root())
            .map_err(frontend_contract)?;
    let filesystem =
        HostProjectFilesystem::new(manifest.root().to_path_buf(), root_directory, control_reads);
    Ok((manifest, filesystem, manifest_bytes))
}

#[derive(Debug)]
struct HostProjectFilesystem {
    root: PathBuf,
    root_directory: File,
    control_reads: ControlReadBudget,
}

impl HostProjectFilesystem {
    fn new(root: PathBuf, root_directory: File, control_reads: ControlReadBudget) -> Self {
        Self {
            root,
            root_directory,
            control_reads,
        }
    }

    fn read_file_bounded(&self, file: File, maximum: usize) -> Result<Vec<u8>, ProjectError> {
        read_bounded_control(file, maximum, &self.control_reads)
    }

    fn read_existing_bounded(
        &self,
        candidate: &Path,
        maximum: usize,
    ) -> Result<Vec<u8>, ProjectError> {
        self.read_file_bounded(self.open_existing_file(candidate)?, maximum)
    }

    fn resolve_existing_file(&self, candidate: &Path) -> Result<PathBuf, ProjectError> {
        let candidate = if candidate.is_absolute() {
            lexical_normalize(candidate)?
        } else {
            lexical_normalize(&self.root.join(candidate))?
        };
        if !candidate.starts_with(&self.root) {
            return Err(ProjectError::new(
                "PROJECT006",
                "path escapes the explicit project root",
            ));
        }
        self.open_existing_file(&candidate)?;
        Ok(candidate)
    }

    fn open_existing_file(&self, candidate: &Path) -> Result<File, ProjectError> {
        let candidate = if candidate.is_absolute() {
            lexical_normalize(candidate)?
        } else {
            lexical_normalize(&self.root.join(candidate))?
        };
        let relative = candidate.strip_prefix(&self.root).map_err(|_| {
            ProjectError::new("PROJECT006", "path escapes the explicit project root")
        })?;
        open_relative_file_nofollow(&self.root_directory, relative, &candidate)
    }

    fn validate_output_path(&self, candidate: &Path) -> Result<(), ProjectError> {
        self.resolve_output_path(candidate).map(|_| ())
    }

    fn resolve_output_path(&self, candidate: &Path) -> Result<PathBuf, ProjectError> {
        let candidate = if candidate.is_absolute() {
            lexical_normalize(candidate)?
        } else {
            lexical_normalize(&self.root.join(candidate))?
        };
        let relative = candidate.strip_prefix(&self.root).map_err(|_| {
            ProjectError::new("PROJECT006", "path escapes the explicit project root")
        })?;
        validate_relative_output_path(&self.root_directory, relative, &candidate)?;
        Ok(candidate)
    }

    fn write_exclusive_atomic(
        &self,
        candidate: &Path,
        bytes: &[u8],
    ) -> Result<(), ProjectFrontendError> {
        self.write_exclusive_atomic_kind(candidate, bytes, "plan")
    }

    fn write_exclusive_atomic_kind(
        &self,
        candidate: &Path,
        bytes: &[u8],
        kind: &str,
    ) -> Result<(), ProjectFrontendError> {
        let maximum = if kind == "audit" {
            MAX_AUDIT_BYTES
        } else {
            opaal_runtime::workflow::MAX_ARTIFACT_BYTES
        };
        if bytes.len() > maximum {
            return Err(frontend_contract(ProjectError::new(
                "PLAN010",
                format!("{kind} output exceeds its byte limit"),
            )));
        }
        let candidate = if candidate.is_absolute() {
            lexical_normalize(candidate)
        } else {
            lexical_normalize(&self.root.join(candidate))
        }
        .map_err(frontend_contract)?;
        let relative = candidate.strip_prefix(&self.root).map_err(|_| {
            frontend_contract(ProjectError::new(
                "PROJECT006",
                "output path escapes the explicit project root",
            ))
        })?;
        let (parent, name) =
            open_relative_output_parent(&self.root_directory, relative, &candidate)
                .map_err(frontend_contract)?;
        require_absent_output(&parent, &name, &candidate).map_err(frontend_contract)?;
        let temporary = OsString::from(format!(
            ".opaal-{kind}-{}-{:016x}",
            std::process::id(),
            NEXT_CONTROL_TEMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let descriptor = rustix::fs::openat(
            &parent,
            &temporary,
            rustix::fs::OFlags::WRONLY
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::EXCL
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .map_err(|error| frontend_contract(ProjectError::new("PLAN010", error.to_string())))?;
        let mut file = File::from(descriptor);
        let outcome = (|| {
            file.write_all(bytes).map_err(|error| {
                frontend_contract(ProjectError::new("PLAN010", error.to_string()))
            })?;
            file.sync_all().map_err(|error| {
                frontend_contract(ProjectError::new("PLAN010", error.to_string()))
            })?;
            rustix::fs::renameat_with(
                &parent,
                &temporary,
                &parent,
                &name,
                rustix::fs::RenameFlags::NOREPLACE,
            )
            .map_err(|error| frontend_contract(ProjectError::new("PLAN010", error.to_string())))?;
            parent.sync_all().map_err(|error| {
                frontend_contract(ProjectError::new("PLAN010", error.to_string()))
            })?;
            Ok(())
        })();
        if outcome.is_err() {
            let _ = rustix::fs::unlinkat(&parent, &temporary, rustix::fs::AtFlags::empty());
        }
        outcome
    }

    fn create_journal(
        &self,
        candidate: &Path,
        run_id: &str,
        header: Value,
    ) -> Result<SyncedJournal, ProjectFrontendError> {
        let candidate = self
            .resolve_output_path(candidate)
            .map_err(frontend_contract)?;
        let relative = candidate.strip_prefix(&self.root).map_err(|_| {
            frontend_contract(ProjectError::new(
                "PROJECT006",
                "journal path escapes the explicit project root",
            ))
        })?;
        let (parent, name) =
            open_relative_output_parent(&self.root_directory, relative, &candidate)
                .map_err(frontend_contract)?;
        require_absent_output(&parent, &name, &candidate).map_err(frontend_contract)?;
        let (chain, header_line) =
            JournalChain::begin(run_id, header).map_err(workflow_contract)?;
        let temporary = OsString::from(format!(
            ".opaal-journal-{}-{:016x}",
            std::process::id(),
            NEXT_CONTROL_TEMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let descriptor = rustix::fs::openat(
            &parent,
            &temporary,
            rustix::fs::OFlags::WRONLY
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::EXCL
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .map_err(|error| execute_error("JOURNAL005", error.to_string()))?;
        let mut file = File::from(descriptor);
        if let Err(error) = file.write_all(&header_line).and_then(|()| file.sync_all()) {
            let _ = rustix::fs::unlinkat(&parent, &temporary, rustix::fs::AtFlags::empty());
            return Err(execute_error("JOURNAL005", error.to_string()));
        }
        if let Err(error) = rustix::fs::renameat_with(
            &parent,
            &temporary,
            &parent,
            &name,
            rustix::fs::RenameFlags::NOREPLACE,
        ) {
            let _ = rustix::fs::unlinkat(&parent, &temporary, rustix::fs::AtFlags::empty());
            return Err(execute_error("JOURNAL005", error.to_string()));
        }
        parent
            .sync_all()
            .map_err(|error| execute_error("JOURNAL005", error.to_string()))?;
        Ok(SyncedJournal {
            run_id: run_id.to_owned(),
            file,
            chain,
            failed: false,
        })
    }
}

static NEXT_CONTROL_TEMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl ModuleCanonicalizer for HostProjectFilesystem {
    fn canonicalize(&self, candidate: &Path) -> Result<PathBuf, ModulePathError> {
        self.resolve_existing_file(candidate)
            .map_err(|error| ModulePathError::new(error.to_string()))
    }
}

impl ModuleSourceLoader for HostProjectFilesystem {
    fn load(&self, module: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        self.load_bounded(module, usize::MAX)
    }

    fn load_bounded(
        &self,
        module: &ModuleId,
        maximum: usize,
    ) -> Result<Vec<u8>, ModuleSourceError> {
        let file = self
            .open_existing_file(module.path())
            .map_err(|error| ModuleSourceError::new(error.to_string()))?;
        let mut bytes = Vec::new();
        file.take(maximum as u64)
            .read_to_end(&mut bytes)
            .map_err(|error| ModuleSourceError::new(error.to_string()))?;
        self.control_reads
            .charge(bytes.len())
            .map_err(|error| ModuleSourceError::new(error.to_string()))?;
        Ok(bytes)
    }
}

fn read_bounded_control(
    file: File,
    maximum: usize,
    control_reads: &ControlReadBudget,
) -> Result<Vec<u8>, ProjectError> {
    let bytes = read_bounded(file, maximum)?;
    control_reads.charge(bytes.len())?;
    Ok(bytes)
}

fn read_bounded(file: File, maximum: usize) -> Result<Vec<u8>, ProjectError> {
    let metadata = file
        .metadata()
        .map_err(|error| ProjectError::new("PROJECT024", error.to_string()))?;
    if !metadata.file_type().is_file() {
        return Err(ProjectError::new(
            "PROJECT024",
            "path is not a regular file",
        ));
    }
    let mut bytes = Vec::new();
    file.take(maximum.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| ProjectError::new("PROJECT024", error.to_string()))?;
    if bytes.len() > maximum {
        return Err(ProjectError::new(
            "PROJECT002",
            format!("control file exceeds its {maximum}-byte read limit"),
        ));
    }
    Ok(bytes)
}

fn same_file_identity(first: &File, second: &File) -> Result<bool, ProjectError> {
    let first = first
        .metadata()
        .map_err(|error| ProjectError::new("PROJECT024", error.to_string()))?;
    let second = second
        .metadata()
        .map_err(|error| ProjectError::new("PROJECT024", error.to_string()))?;
    Ok(first.dev() == second.dev() && first.ino() == second.ino())
}

fn open_absolute_file_with_parent_nofollow(path: &Path) -> Result<(File, File), ProjectError> {
    let parent = path
        .parent()
        .ok_or_else(|| ProjectError::new("PROJECT024", "path has no parent directory"))?;
    let name = path
        .file_name()
        .ok_or_else(|| ProjectError::new("PROJECT024", "path has no file name"))?;
    let directory = open_absolute_directory_nofollow(parent)?;
    let file = open_file_at_nofollow(&directory, name, path)?;
    Ok((directory, file))
}

fn open_relative_directory_nofollow(
    mut directory: File,
    relative: &Path,
    display_path: &Path,
) -> Result<File, ProjectError> {
    let mut traversed = PathBuf::new();
    for component in relative.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(name) => {
                traversed.push(name);
                directory = open_directory_at_nofollow(&directory, name, &traversed)?;
            }
            Component::RootDir | Component::ParentDir | Component::Prefix(_) => {
                return Err(ProjectError::new(
                    "PROJECT024",
                    format!("{} is not relative to the manifest", display_path.display()),
                ));
            }
        }
    }
    Ok(directory)
}

fn open_absolute_directory_nofollow(path: &Path) -> Result<File, ProjectError> {
    if !path.is_absolute() {
        return Err(ProjectError::new(
            "PROJECT024",
            "project directory path is not absolute",
        ));
    }
    let descriptor = rustix::fs::open(
        Path::new("/"),
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|error| project_open_error(error, Path::new("/")))?;
    let mut directory = File::from(descriptor);
    let mut traversed = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => {
                traversed.push(name);
                directory = open_directory_at_nofollow(&directory, name, &traversed)?;
            }
            Component::ParentDir | Component::Prefix(_) => {
                return Err(ProjectError::new(
                    "PROJECT024",
                    "project directory path is not lexically normalized",
                ));
            }
        }
    }
    Ok(directory)
}

fn open_relative_file_nofollow(
    root: &File,
    relative: &Path,
    display_path: &Path,
) -> Result<File, ProjectError> {
    let components = relative.components().collect::<Vec<_>>();
    let Some((final_component, parent_components)) = components.split_last() else {
        return Err(ProjectError::new(
            "PROJECT024",
            "path is not a regular file",
        ));
    };
    let Component::Normal(file_name) = final_component else {
        return Err(ProjectError::new(
            "PROJECT024",
            "project file path is not lexically normalized",
        ));
    };
    let mut directory = None;
    let mut traversed = PathBuf::new();
    for component in parent_components {
        let Component::Normal(name) = component else {
            return Err(ProjectError::new(
                "PROJECT024",
                "project file path is not lexically normalized",
            ));
        };
        traversed.push(name);
        let parent = directory.as_ref().unwrap_or(root);
        directory = Some(open_directory_at_nofollow(parent, name, &traversed)?);
    }
    open_file_at_nofollow(directory.as_ref().unwrap_or(root), file_name, display_path)
}

fn validate_relative_output_path(
    root: &File,
    relative: &Path,
    display_path: &Path,
) -> Result<(), ProjectError> {
    let components = relative.components().collect::<Vec<_>>();
    let Some((final_component, parent_components)) = components.split_last() else {
        return Err(ProjectError::new(
            "PROJECT024",
            "output path has no file name",
        ));
    };
    let Component::Normal(file_name) = final_component else {
        return Err(ProjectError::new(
            "PROJECT024",
            "output path is not lexically normalized",
        ));
    };
    let mut directory = None;
    let mut traversed = PathBuf::new();
    for component in parent_components {
        let Component::Normal(name) = component else {
            return Err(ProjectError::new(
                "PROJECT024",
                "output path is not lexically normalized",
            ));
        };
        traversed.push(name);
        let parent = directory.as_ref().unwrap_or(root);
        directory = Some(open_directory_at_nofollow(parent, name, &traversed)?);
    }
    let parent = directory.as_ref().unwrap_or(root);
    let metadata =
        match rustix::fs::statat(parent, *file_name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
            Ok(metadata) => metadata,
            Err(rustix::io::Errno::NOENT) => return Ok(()),
            Err(error) => return Err(project_open_error(error, display_path)),
        };
    match rustix::fs::FileType::from_raw_mode(metadata.st_mode) {
        rustix::fs::FileType::RegularFile => Ok(()),
        rustix::fs::FileType::Symlink => Err(ProjectError::new(
            "PROJECT025",
            format!(
                "project path contains symbolic link `{}`",
                display_path.display()
            ),
        )),
        _ => Err(ProjectError::new(
            "PROJECT024",
            format!("{} is not a regular output file", display_path.display()),
        )),
    }
}

fn open_relative_output_parent(
    root: &File,
    relative: &Path,
    display_path: &Path,
) -> Result<(File, OsString), ProjectError> {
    let components = relative.components().collect::<Vec<_>>();
    let Some((final_component, parent_components)) = components.split_last() else {
        return Err(ProjectError::new(
            "PROJECT024",
            "output path has no file name",
        ));
    };
    let Component::Normal(file_name) = final_component else {
        return Err(ProjectError::new(
            "PROJECT024",
            "output path is not lexically normalized",
        ));
    };
    let mut parent = root
        .try_clone()
        .map_err(|error| ProjectError::new("PROJECT024", error.to_string()))?;
    let mut traversed = PathBuf::new();
    for component in parent_components {
        let Component::Normal(name) = component else {
            return Err(ProjectError::new(
                "PROJECT024",
                "output path is not lexically normalized",
            ));
        };
        traversed.push(name);
        parent = open_directory_at_nofollow(&parent, name, &traversed)?;
    }
    let _ = display_path;
    Ok((parent, file_name.to_os_string()))
}

fn require_absent_output(
    parent: &File,
    name: &OsStr,
    display_path: &Path,
) -> Result<(), ProjectError> {
    match rustix::fs::statat(parent, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
        Err(rustix::io::Errno::NOENT) => Ok(()),
        Ok(_) => Err(ProjectError::new(
            "PLAN010",
            format!("output `{}` already exists", display_path.display()),
        )),
        Err(error) => Err(project_open_error(error, display_path)),
    }
}

fn open_directory_at_nofollow(
    parent: &File,
    name: &OsStr,
    display_path: &Path,
) -> Result<File, ProjectError> {
    let descriptor = rustix::fs::openat(
        parent,
        name,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|error| project_open_error(error, display_path))?;
    let directory = File::from(descriptor);
    if !directory
        .metadata()
        .map_err(|error| ProjectError::new("PROJECT024", error.to_string()))?
        .is_dir()
    {
        return Err(ProjectError::new(
            "PROJECT024",
            format!("{} is not a directory", display_path.display()),
        ));
    }
    Ok(directory)
}

fn open_file_at_nofollow(
    parent: &File,
    name: &OsStr,
    display_path: &Path,
) -> Result<File, ProjectError> {
    let descriptor = rustix::fs::openat(
        parent,
        name,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|error| project_open_error(error, display_path))?;
    let file = File::from(descriptor);
    if !file
        .metadata()
        .map_err(|error| ProjectError::new("PROJECT024", error.to_string()))?
        .is_file()
    {
        return Err(ProjectError::new(
            "PROJECT024",
            format!("{} is not a regular file", display_path.display()),
        ));
    }
    Ok(file)
}

fn project_open_error(error: rustix::io::Errno, path: &Path) -> ProjectError {
    if error == rustix::io::Errno::LOOP {
        ProjectError::new(
            "PROJECT025",
            format!("project path contains symbolic link `{}`", path.display()),
        )
    } else {
        ProjectError::new(
            "PROJECT024",
            format!("cannot open `{}`: {error}", path.display()),
        )
    }
}

fn absolute_lexical(path: &Path) -> Result<PathBuf, ProjectError> {
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| ProjectError::new("PROJECT024", error.to_string()))?
            .join(path)
    };
    lexical_normalize(&candidate)
}

fn lexical_normalize(path: &Path) -> Result<PathBuf, ProjectError> {
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => output.push(prefix.as_os_str()),
            Component::RootDir => output.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                if !output.pop() {
                    return Err(ProjectError::new(
                        "PROJECT006",
                        "path escapes its lexical root",
                    ));
                }
            }
            Component::Normal(value) => output.push(value),
        }
    }
    Ok(output)
}

fn frontend_contract(error: ProjectError) -> ProjectFrontendError {
    ProjectFrontendError {
        rendered: format!("opaal: {error}\n"),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{self, Cursor};
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct TempDirectory(PathBuf);

    impl TempDirectory {
        fn new() -> Self {
            let path = absolute_lexical(
                &Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../target")
                    .join(format!(
                        "opaal-retained-project-{}-{}",
                        std::process::id(),
                        NEXT.fetch_add(1, Ordering::Relaxed)
                    )),
            )
            .unwrap();
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn retained_manifest_parent_anchors_later_source_reads() {
        let temporary = TempDirectory::new();
        let project_path = temporary.0.join("project");
        let moved_path = temporary.0.join("moved");
        fs::create_dir(&project_path).unwrap();
        let manifest_path = project_path.join("opaal.toml");
        fs::write(&manifest_path, "manifest").unwrap();
        fs::write(project_path.join("tasks.opaal"), "original").unwrap();

        let (manifest_parent, manifest_file) =
            open_absolute_file_with_parent_nofollow(&manifest_path).unwrap();
        drop(manifest_file);
        fs::rename(&project_path, &moved_path).unwrap();
        fs::create_dir(&project_path).unwrap();
        fs::write(project_path.join("tasks.opaal"), "replacement").unwrap();

        let root = open_relative_directory_nofollow(manifest_parent, Path::new("."), &project_path)
            .unwrap();
        let filesystem =
            HostProjectFilesystem::new(project_path.clone(), root, ControlReadBudget::default());
        let mut source = String::new();
        filesystem
            .open_existing_file(&project_path.join("tasks.opaal"))
            .unwrap()
            .read_to_string(&mut source)
            .unwrap();
        assert_eq!(source, "original");
    }

    #[test]
    fn aggregate_control_reads_accept_the_exact_limit_and_refuse_the_first_excess() {
        let budget = ControlReadBudget::default();
        budget.charge(MAX_CONTROL_READ_BYTES).unwrap();
        let error = budget.charge(1).unwrap_err();
        assert_eq!(error.code(), "PROJECT027");
    }

    #[test]
    fn plan_identity_includes_secret_requirements() {
        let plan = json!({
            "toolchain":{},
            "project":{},
            "task":{},
            "inputs":[],
            "secrets":[{"id":"planned","endpoint":"sink","header":"authorization"}],
            "sources":[],
            "authority":{},
            "tools":[]
        });
        let mut check = plan.clone();
        check["secrets"][0]["id"] = Value::String("current".to_owned());

        let error = verify_plan_check_identity(&plan, &check).unwrap_err();
        assert!(error.rendered().contains("bound secrets identity changed"));
    }

    #[test]
    fn wrong_secret_identity_precedes_terminal_validation() {
        let plan = json!({
            "secrets":[{"id":"token","endpoint":"sink","header":"authorization"}]
        });

        let wrong = validate_secret_binding(&plan, Some("other"), true).unwrap_err();
        assert!(wrong.rendered().contains("EXECUTE008"));
        let matching = validate_secret_binding(&plan, Some("token"), true).unwrap_err();
        assert!(matching.rendered().contains("EXECUTE001"));
    }

    #[test]
    fn injected_run_id_entropy_is_exact_and_fail_closed() {
        struct FailedEntropy;

        impl Read for FailedEntropy {
            fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("entropy unavailable"))
            }
        }

        let mut complete = Cursor::new((0_u8..16).collect::<Vec<_>>());
        assert_eq!(
            generate_run_id_from(&mut complete).unwrap(),
            "000102030405060708090a0b0c0d0e0f"
        );

        let mut short = Cursor::new(vec![0_u8; 15]);
        let short_error = generate_run_id_from(&mut short).unwrap_err();
        assert!(short_error.rendered().contains("EXECUTE010"));

        let failure = generate_run_id_from(&mut FailedEntropy).unwrap_err();
        assert!(failure.rendered().contains("EXECUTE010"));
    }
}
