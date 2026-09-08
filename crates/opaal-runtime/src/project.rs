//! Strict, non-executing project, authority, and host-lock contracts.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::{Component, Path, PathBuf};

use opaal_platform::operational::{OperationalAdapter, ReadFileRequest};
use semver::{Version, VersionReq};
use sha2::{Digest, Sha256};
use toml::{Table, Value};

use crate::module::{
    ActionSignature, ModuleCanonicalizer, ModuleId, ModuleOrigin, ModuleProgram,
    ModuleProgramLoadError, ModuleProgramLoader, ModuleSourceError, ModuleSourceLoader, ValueType,
};

/// Maximum encoded bytes accepted for each declarative document.
pub const MAX_PROJECT_DOCUMENT_BYTES: usize = 1_048_576;
/// Maximum entries accepted in one document collection.
pub const MAX_PROJECT_ENTRIES: usize = 256;
/// Maximum aggregate nesting accepted in one declarative document.
pub const MAX_PROJECT_DOCUMENT_DEPTH: usize = 16;
/// Maximum explicit values accepted for one task check.
pub const MAX_PROJECT_INPUTS: usize = 64;

/// Stable identity of one explicitly named project.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProjectId {
    name: String,
    manifest_path: PathBuf,
    root: PathBuf,
    manifest_digest: String,
}

impl ProjectId {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn manifest_path(&self) -> &Path {
        &self.manifest_path
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn manifest_digest(&self) -> &str {
        &self.manifest_digest
    }
}

/// Stable identity of one tool declaration within a project.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ToolId {
    project: ProjectId,
    name: String,
}

impl ToolId {
    #[must_use]
    pub const fn project(&self) -> &ProjectId {
        &self.project
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn qualified_name(&self) -> String {
        format!("{}::tool::{}", self.project.name, self.name)
    }
}

/// Stable identity of one declared environment within a project.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct EnvironmentId {
    project: ProjectId,
    name: String,
}

impl EnvironmentId {
    #[must_use]
    pub const fn project(&self) -> &ProjectId {
        &self.project
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn qualified_name(&self) -> String {
        format!("{}::environment::{}", self.project.name, self.name)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectManifest {
    manifest_path: PathBuf,
    id: ProjectId,
    root_module: PathBuf,
    required_opaal: VersionReq,
    root: PathBuf,
    evidence: PathBuf,
    tools: BTreeMap<String, ToolDeclaration>,
    endpoints: BTreeMap<String, EndpointDeclaration>,
    secrets: BTreeMap<String, SecretDeclaration>,
    environments: BTreeMap<String, EnvironmentDeclaration>,
}

impl ProjectManifest {
    #[must_use]
    pub fn manifest_path(&self) -> &Path {
        &self.manifest_path
    }

    #[must_use]
    pub fn name(&self) -> &str {
        self.id.name()
    }

    #[must_use]
    pub const fn id(&self) -> &ProjectId {
        &self.id
    }

    #[must_use]
    pub fn root_module(&self) -> &Path {
        &self.root_module
    }

    #[must_use]
    pub const fn required_opaal(&self) -> &VersionReq {
        &self.required_opaal
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn evidence(&self) -> &Path {
        &self.evidence
    }

    #[must_use]
    pub const fn tools(&self) -> &BTreeMap<String, ToolDeclaration> {
        &self.tools
    }

    #[must_use]
    pub const fn endpoints(&self) -> &BTreeMap<String, EndpointDeclaration> {
        &self.endpoints
    }

    #[must_use]
    pub const fn secrets(&self) -> &BTreeMap<String, SecretDeclaration> {
        &self.secrets
    }

    #[must_use]
    pub const fn environments(&self) -> &BTreeMap<String, EnvironmentDeclaration> {
        &self.environments
    }

    pub fn environment(&self, id: &str) -> Result<&EnvironmentDeclaration, ProjectError> {
        self.environments
            .get(id)
            .ok_or_else(|| ProjectError::new("PROJECT021", format!("unknown environment `{id}`")))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolDeclaration {
    id: ToolId,
    adapter: MaintainedAdapter,
    version: VersionReq,
}

impl ToolDeclaration {
    #[must_use]
    pub const fn id(&self) -> &ToolId {
        &self.id
    }

    #[must_use]
    pub const fn adapter(&self) -> MaintainedAdapter {
        self.adapter
    }

    #[must_use]
    pub const fn version(&self) -> &VersionReq {
        &self.version
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MaintainedAdapter {
    Git,
    Cargo,
}

impl MaintainedAdapter {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Cargo => "cargo",
        }
    }

    fn parse(value: &str) -> Result<Self, ProjectError> {
        match value {
            "git" => Ok(Self::Git),
            "cargo" => Ok(Self::Cargo),
            _ => Err(ProjectError::new(
                "PROJECT012",
                format!("unknown maintained adapter `{value}`"),
            )),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EndpointDeclaration {
    id: String,
    url: String,
    methods: Vec<String>,
    secret_headers: Vec<String>,
    tls: bool,
    tls_server_name: Option<String>,
    ca: Option<PathBuf>,
}

impl EndpointDeclaration {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    #[must_use]
    pub fn methods(&self) -> &[String] {
        &self.methods
    }

    #[must_use]
    pub fn secret_headers(&self) -> &[String] {
        &self.secret_headers
    }

    #[must_use]
    pub const fn tls(&self) -> bool {
        self.tls
    }

    #[must_use]
    pub fn tls_server_name(&self) -> Option<&str> {
        self.tls_server_name.as_deref()
    }

    #[must_use]
    pub fn ca(&self) -> Option<&Path> {
        self.ca.as_deref()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SecretDeclaration;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnvironmentDeclaration {
    id: EnvironmentId,
    authority: PathBuf,
    tool_lock: PathBuf,
}

impl EnvironmentDeclaration {
    #[must_use]
    pub const fn id(&self) -> &EnvironmentId {
        &self.id
    }

    #[must_use]
    pub fn authority(&self) -> &Path {
        &self.authority
    }

    #[must_use]
    pub fn tool_lock(&self) -> &Path {
        &self.tool_lock
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorityDocument {
    project: String,
    environment: String,
    rules: Vec<AuthorityDocumentRule>,
}

impl AuthorityDocument {
    #[must_use]
    pub fn project(&self) -> &str {
        &self.project
    }

    #[must_use]
    pub fn environment(&self) -> &str {
        &self.environment
    }

    #[must_use]
    pub fn rules(&self) -> &[AuthorityDocumentRule] {
        &self.rules
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthorityDecision {
    Deny,
    Grant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DocumentEnforcement {
    Enforced,
    AcknowledgeUnenforced,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorityDocumentRule {
    decision: AuthorityDecision,
    effect: String,
    scope: String,
    required_enforcement: Option<DocumentEnforcement>,
}

impl AuthorityDocumentRule {
    #[must_use]
    pub const fn decision(&self) -> AuthorityDecision {
        self.decision
    }

    #[must_use]
    pub fn effect(&self) -> &str {
        &self.effect
    }

    #[must_use]
    pub fn scope(&self) -> &str {
        &self.scope
    }

    #[must_use]
    pub const fn required_enforcement(&self) -> Option<DocumentEnforcement> {
        self.required_enforcement
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolLock {
    project: String,
    environment: String,
    platform: String,
    variables: Vec<ChildEnvironmentVariable>,
    tools: BTreeMap<String, LockedTool>,
}

impl ToolLock {
    #[must_use]
    pub fn project(&self) -> &str {
        &self.project
    }

    #[must_use]
    pub fn environment(&self) -> &str {
        &self.environment
    }

    #[must_use]
    pub fn platform(&self) -> &str {
        &self.platform
    }

    #[must_use]
    pub fn variables(&self) -> &[ChildEnvironmentVariable] {
        &self.variables
    }

    #[must_use]
    pub const fn tools(&self) -> &BTreeMap<String, LockedTool> {
        &self.tools
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChildEnvironmentVariable {
    name: String,
    value: NativeBytes,
}

impl ChildEnvironmentVariable {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub const fn value(&self) -> &NativeBytes {
        &self.value
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeBytes {
    platform: String,
    base64url_nopad: String,
    bytes: Vec<u8>,
}

impl NativeBytes {
    #[must_use]
    pub fn platform(&self) -> &str {
        &self.platform
    }

    #[must_use]
    pub fn encoded(&self) -> &str {
        &self.base64url_nopad
    }

    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LockedTool {
    adapter: MaintainedAdapter,
    path: NativeBytes,
    version: Version,
    digest: String,
}

impl LockedTool {
    #[must_use]
    pub const fn adapter(&self) -> MaintainedAdapter {
        self.adapter
    }

    #[must_use]
    pub const fn path(&self) -> &NativeBytes {
        &self.path
    }

    #[must_use]
    pub const fn version(&self) -> &Version {
        &self.version
    }

    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectError {
    code: &'static str,
    message: String,
}

impl ProjectError {
    #[must_use]
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn code(&self) -> &'static str {
        self.code
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ProjectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ProjectError {}

pub fn parse_project_manifest(
    manifest_path: &Path,
    bytes: &[u8],
) -> Result<ProjectManifest, ProjectError> {
    if manifest_path.file_name().and_then(|name| name.to_str()) != Some("opaal.toml")
        || !manifest_path.is_absolute()
        || manifest_path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(ProjectError::new(
            "PROJECT023",
            "manifest identity must be an absolute normalized opaal.toml path",
        ));
    }
    let document = parse_document(bytes, "project manifest")?;
    exact_keys(
        &document,
        &[
            "schema_version",
            "project",
            "paths",
            "tools",
            "endpoints",
            "secrets",
            "environments",
        ],
        "project manifest",
    )?;
    schema_one(&document)?;
    let project = required_table(&document, "project")?;
    exact_keys(
        project,
        &["name", "root_module", "required_opaal"],
        "project",
    )?;
    let name = required_string(project, "name")?.to_owned();
    if !is_opaal_identifier(&name) {
        return Err(ProjectError::new(
            "PROJECT004",
            "project name must be an OPAAL identifier",
        ));
    }
    let required_opaal =
        VersionReq::parse(required_string(project, "required_opaal")?).map_err(|error| {
            ProjectError::new("PROJECT005", format!("invalid required_opaal: {error}"))
        })?;
    let paths = required_table(&document, "paths")?;
    exact_keys(paths, &["root", "evidence"], "paths")?;
    let base = manifest_path
        .parent()
        .ok_or_else(|| ProjectError::new("PROJECT006", "manifest path has no parent"))?;
    let root = resolve_contained(base, required_string(paths, "root")?, base, "paths.root")?;
    let root_module = resolve_contained(
        &root,
        required_string(project, "root_module")?,
        &root,
        "project.root_module",
    )?;
    let evidence = resolve_contained(
        &root,
        required_string(paths, "evidence")?,
        &root,
        "paths.evidence",
    )?;
    let project_id = ProjectId {
        name,
        manifest_path: manifest_path.to_path_buf(),
        root: root.clone(),
        manifest_digest: sha256_digest(bytes),
    };
    let total = ["tools", "endpoints", "secrets", "environments"]
        .into_iter()
        .try_fold(0usize, |total, key| {
            total
                .checked_add(optional_named_table_len(&document, key)?)
                .ok_or_else(|| ProjectError::new("PROJECT019", "manifest entry count overflow"))
        })?;
    if total > MAX_PROJECT_ENTRIES {
        return Err(ProjectError::new(
            "PROJECT019",
            format!("project manifest has {total} entries; maximum is {MAX_PROJECT_ENTRIES}"),
        ));
    }
    let tools = parse_named_tables(&document, "tools", |id, table| {
        exact_keys(table, &["adapter", "version"], &format!("tools.{id}"))?;
        Ok(ToolDeclaration {
            id: ToolId {
                project: project_id.clone(),
                name: id.to_owned(),
            },
            adapter: MaintainedAdapter::parse(required_string(table, "adapter")?)?,
            version: VersionReq::parse(required_string(table, "version")?).map_err(|error| {
                ProjectError::new("PROJECT013", format!("invalid tools.{id}.version: {error}"))
            })?,
        })
    })?;
    let endpoints = parse_named_tables(&document, "endpoints", |id, table| {
        exact_keys(
            table,
            &[
                "url",
                "methods",
                "secret_headers",
                "tls",
                "tls_server_name",
                "ca",
            ],
            &format!("endpoints.{id}"),
        )?;
        let url = required_string(table, "url")?.to_owned();
        let methods = required_unique_strings(table, "methods")?;
        if methods.is_empty() || methods.iter().any(|method| !is_http_method(method)) {
            return Err(ProjectError::new(
                "PROJECT014",
                format!("endpoints.{id}.methods must contain canonical uppercase methods"),
            ));
        }
        let secret_headers = required_unique_strings(table, "secret_headers")?;
        if secret_headers.iter().any(|header| !is_header_name(header)) {
            return Err(ProjectError::new(
                "PROJECT015",
                format!("endpoints.{id}.secret_headers contains an invalid header"),
            ));
        }
        let tls = required_bool(table, "tls")?;
        let tls_server_name = optional_string(table, "tls_server_name")?.map(str::to_owned);
        let ca = optional_string(table, "ca")?
            .map(|path| resolve_contained(&root, path, &root, "endpoint CA"))
            .transpose()?;
        if tls && (tls_server_name.is_none() || ca.is_none()) {
            return Err(ProjectError::new(
                "PROJECT016",
                format!("TLS endpoint `{id}` requires tls_server_name and ca"),
            ));
        }
        if tls
            && (parse_http_url(&url).is_none_or(|parsed| parsed.scheme != "https")
                || tls_server_name
                    .as_deref()
                    .is_none_or(|name| !is_dns_name(name)))
        {
            return Err(ProjectError::new(
                "PROJECT016",
                format!("TLS endpoint `{id}` requires a valid HTTPS URL and DNS server name"),
            ));
        }
        if !tls && (!is_literal_loopback(&url) || tls_server_name.is_some() || ca.is_some()) {
            return Err(ProjectError::new(
                "PROJECT017",
                format!("non-TLS endpoint `{id}` must be literal loopback without TLS fields"),
            ));
        }
        Ok(EndpointDeclaration {
            id: id.to_owned(),
            url,
            methods,
            secret_headers,
            tls,
            tls_server_name,
            ca,
        })
    })?;
    let secrets = parse_named_tables(&document, "secrets", |id, table| {
        exact_keys(table, &["kind"], &format!("secrets.{id}"))?;
        if required_string(table, "kind")? != "injected" {
            return Err(ProjectError::new(
                "PROJECT018",
                format!("secret `{id}` must use kind = \"injected\""),
            ));
        }
        Ok(SecretDeclaration)
    })?;
    let environments = parse_named_tables(&document, "environments", |id, table| {
        exact_keys(
            table,
            &["authority", "tool_lock"],
            &format!("environments.{id}"),
        )?;
        Ok(EnvironmentDeclaration {
            id: EnvironmentId {
                project: project_id.clone(),
                name: id.to_owned(),
            },
            authority: resolve_contained(
                &root,
                required_string(table, "authority")?,
                &root,
                "environment authority",
            )?,
            tool_lock: resolve_contained(
                &root,
                required_string(table, "tool_lock")?,
                &root,
                "environment tool_lock",
            )?,
        })
    })?;
    Ok(ProjectManifest {
        manifest_path: manifest_path.to_path_buf(),
        id: project_id,
        root_module,
        required_opaal,
        root,
        evidence,
        tools,
        endpoints,
        secrets,
        environments,
    })
}

pub fn parse_authority_document(
    manifest: &ProjectManifest,
    environment: &str,
    bytes: &[u8],
) -> Result<AuthorityDocument, ProjectError> {
    let document = parse_document(bytes, "authority document")?;
    exact_keys(
        &document,
        &["schema_version", "project", "environment", "rules"],
        "authority document",
    )?;
    schema_one(&document)?;
    let project = required_string(&document, "project")?.to_owned();
    let selected = required_string(&document, "environment")?.to_owned();
    if project != manifest.id.name || selected != environment {
        return Err(ProjectError::new(
            "AUTH001",
            "authority project/environment does not match the selected project",
        ));
    }
    let rows = document
        .get("rules")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            ProjectError::new("AUTH002", "authority rules must be an array of tables")
        })?;
    if rows.len() > MAX_PROJECT_ENTRIES {
        return Err(ProjectError::new(
            "AUTH003",
            "authority has more than 256 rules",
        ));
    }
    let mut rules = Vec::new();
    let mut seen = BTreeSet::new();
    for row in rows {
        let table = row
            .as_table()
            .ok_or_else(|| ProjectError::new("AUTH002", "authority rule is not a table"))?;
        let decision_text = required_string(table, "decision")?;
        let decision = match decision_text {
            "deny" => AuthorityDecision::Deny,
            "grant" => AuthorityDecision::Grant,
            _ => {
                return Err(ProjectError::new(
                    "AUTH004",
                    "decision must be grant or deny",
                ));
            }
        };
        let allowed: &[&str] = if decision == AuthorityDecision::Grant {
            &["decision", "effect", "scope", "required_enforcement"]
        } else {
            &["decision", "effect", "scope"]
        };
        exact_keys(table, allowed, "authority rule")?;
        let effect = required_string(table, "effect")?.to_owned();
        let scope = required_string(table, "scope")?.to_owned();
        validate_authority_scope(manifest, &effect, &scope)?;
        if !seen.insert((effect.clone(), scope.clone())) {
            return Err(ProjectError::new(
                "AUTH005",
                "duplicate normalized authority request",
            ));
        }
        let required_enforcement = if decision == AuthorityDecision::Grant {
            let enforcement = match required_string(table, "required_enforcement")? {
                "enforced" => DocumentEnforcement::Enforced,
                "acknowledge-unenforced" => DocumentEnforcement::AcknowledgeUnenforced,
                _ => return Err(ProjectError::new("AUTH006", "invalid required_enforcement")),
            };
            if effect == "process.run" && enforcement != DocumentEnforcement::AcknowledgeUnenforced
            {
                return Err(ProjectError::new(
                    "AUTH007",
                    "process.run grants require acknowledge-unenforced",
                ));
            }
            if effect != "process.run" && enforcement != DocumentEnforcement::Enforced {
                return Err(ProjectError::new(
                    "AUTH008",
                    "only process.run may acknowledge an unenforced boundary",
                ));
            }
            Some(enforcement)
        } else {
            None
        };
        rules.push(AuthorityDocumentRule {
            decision,
            effect,
            scope,
            required_enforcement,
        });
    }
    Ok(AuthorityDocument {
        project,
        environment: selected,
        rules,
    })
}

pub fn parse_tool_lock(
    manifest: &ProjectManifest,
    environment: &str,
    bytes: &[u8],
) -> Result<ToolLock, ProjectError> {
    let document = parse_document(bytes, "tool lock")?;
    exact_keys(
        &document,
        &[
            "schema_version",
            "project",
            "environment",
            "platform",
            "child_environment",
            "tools",
        ],
        "tool lock",
    )?;
    schema_one(&document)?;
    let project = required_string(&document, "project")?.to_owned();
    let selected = required_string(&document, "environment")?.to_owned();
    if project != manifest.id.name || selected != environment {
        return Err(ProjectError::new(
            "LOCK001",
            "tool lock project/environment does not match the selected project",
        ));
    }
    let platform = required_string(&document, "platform")?.to_owned();
    if platform.is_empty() {
        return Err(ProjectError::new(
            "LOCK002",
            "tool lock platform cannot be empty",
        ));
    }
    let child = required_table(&document, "child_environment")?;
    exact_keys(child, &["inherit", "variables"], "child_environment")?;
    let inherit = child
        .get("inherit")
        .and_then(Value::as_array)
        .ok_or_else(|| ProjectError::new("LOCK003", "child_environment.inherit must be []"))?;
    if !inherit.is_empty() {
        return Err(ProjectError::new(
            "LOCK003",
            "ambient environment inheritance is forbidden",
        ));
    }
    let variable_rows = child
        .get("variables")
        .map(|value| {
            value
                .as_array()
                .ok_or_else(|| ProjectError::new("LOCK004", "variables must be an array of tables"))
        })
        .transpose()?
        .map_or(&[][..], Vec::as_slice);
    if variable_rows.len() > MAX_PROJECT_ENTRIES {
        return Err(ProjectError::new(
            "LOCK005",
            "too many child environment variables",
        ));
    }
    let tool_rows = document
        .get("tools")
        .and_then(Value::as_array)
        .ok_or_else(|| ProjectError::new("LOCK008", "tools must be an array of tables"))?;
    let total_entries = variable_rows
        .len()
        .checked_add(tool_rows.len())
        .ok_or_else(|| ProjectError::new("LOCK005", "tool lock entry count overflow"))?;
    if total_entries > MAX_PROJECT_ENTRIES {
        return Err(ProjectError::new(
            "LOCK005",
            "tool lock has more than 256 variable and tool entries",
        ));
    }
    let mut variables = Vec::new();
    let mut names = BTreeSet::new();
    let mut encoded_bytes = 0usize;
    for row in variable_rows {
        let table = row
            .as_table()
            .ok_or_else(|| ProjectError::new("LOCK004", "variable is not a table"))?;
        exact_keys(table, &["name", "value"], "child_environment variable")?;
        let name = required_string(table, "name")?.to_owned();
        if !is_portable_identifier(&name) || !names.insert(name.clone()) {
            return Err(ProjectError::new(
                "LOCK006",
                "invalid or duplicate environment variable name",
            ));
        }
        let value = parse_native_bytes(required_table(table, "value")?)?;
        if value.bytes().contains(&0) {
            return Err(ProjectError::new(
                "LOCK019",
                "child environment values cannot contain NUL bytes",
            ));
        }
        encoded_bytes = encoded_bytes
            .checked_add(name.len() + value.encoded().len())
            .ok_or_else(|| ProjectError::new("LOCK007", "child environment size overflow"))?;
        if encoded_bytes > 65_536 {
            return Err(ProjectError::new(
                "LOCK007",
                "child environment exceeds 64 KiB",
            ));
        }
        variables.push(ChildEnvironmentVariable { name, value });
    }
    let mut tools = BTreeMap::new();
    for row in tool_rows {
        let table = row
            .as_table()
            .ok_or_else(|| ProjectError::new("LOCK008", "tool row is not a table"))?;
        exact_keys(
            table,
            &["id", "adapter", "path", "version", "digest"],
            "locked tool",
        )?;
        let id = required_string(table, "id")?.to_owned();
        let declaration = manifest.tools.get(&id).ok_or_else(|| {
            ProjectError::new("LOCK009", format!("lock contains undeclared tool `{id}`"))
        })?;
        let adapter = MaintainedAdapter::parse(required_string(table, "adapter")?)?;
        if adapter != declaration.adapter {
            return Err(ProjectError::new(
                "LOCK010",
                format!("adapter mismatch for tool `{id}`"),
            ));
        }
        let version_text = required_string(table, "version")?;
        let version = Version::parse(version_text).map_err(|error| {
            ProjectError::new("LOCK011", format!("invalid tool version: {error}"))
        })?;
        if version.to_string() != version_text {
            return Err(ProjectError::new(
                "LOCK011",
                format!("tool `{id}` version is not canonical SemVer"),
            ));
        }
        if !declaration.version.matches(&version) {
            return Err(ProjectError::new(
                "LOCK012",
                format!("tool `{id}` version is outside its declared range"),
            ));
        }
        let digest = required_string(table, "digest")?.to_owned();
        if !is_sha256(&digest) {
            return Err(ProjectError::new(
                "LOCK013",
                format!("tool `{id}` has an invalid SHA-256 digest"),
            ));
        }
        let path = parse_native_bytes(required_table(table, "path")?)?;
        if !is_normalized_absolute_native_path(path.bytes()) {
            return Err(ProjectError::new(
                "LOCK020",
                format!("tool `{id}` path must be a normalized absolute Unix file path"),
            ));
        }
        let locked = LockedTool {
            adapter,
            path,
            version,
            digest,
        };
        if tools.insert(id.clone(), locked).is_some() {
            return Err(ProjectError::new(
                "LOCK014",
                format!("duplicate locked tool `{id}`"),
            ));
        }
    }
    if tools.len() != manifest.tools.len()
        || manifest.tools.keys().any(|id| !tools.contains_key(id))
    {
        return Err(ProjectError::new(
            "LOCK015",
            "tool lock must contain every declared tool exactly once",
        ));
    }
    Ok(ToolLock {
        project,
        environment: selected,
        platform,
        variables,
        tools,
    })
}

/// Read one explicitly named project-contained tool lock without following a
/// symlink, then apply the same closed schema used by in-memory callers.
pub fn read_tool_lock(
    adapter: &dyn OperationalAdapter,
    manifest: &ProjectManifest,
    environment: &str,
    path: &Path,
) -> Result<ToolLock, ProjectError> {
    let path = crate::operational::path::contained(manifest.root(), path).map_err(|_| {
        ProjectError::new(
            "LOCK021",
            "tool-lock path must be absolute and lexically project-contained",
        )
    })?;
    if path == manifest.root() {
        return Err(ProjectError::new(
            "LOCK021",
            "tool-lock path must be absolute and lexically project-contained",
        ));
    }
    let expected = manifest.environment(environment)?.tool_lock();
    if path != expected {
        return Err(ProjectError::new(
            "LOCK021",
            "tool-lock path differs from the selected environment binding",
        ));
    }
    let bytes = adapter
        .read_file(ReadFileRequest {
            root: manifest.root(),
            path: &path,
            max_bytes: MAX_PROJECT_DOCUMENT_BYTES,
        })
        .map_err(|error| ProjectError::new("LOCK022", format!("cannot read tool lock: {error}")))?;
    parse_tool_lock(manifest, environment, &bytes)
}

fn is_normalized_absolute_native_path(bytes: &[u8]) -> bool {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    if bytes.contains(&0) {
        return false;
    }
    let path = PathBuf::from(std::ffi::OsString::from_vec(bytes.to_vec()));
    if !path.is_absolute() || path == Path::new("/") {
        return false;
    }
    let mut normalized = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => normalized.push(name),
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => return false,
        }
    }
    normalized.as_os_str().as_bytes() == bytes
}

fn parse_document(bytes: &[u8], role: &str) -> Result<Table, ProjectError> {
    if bytes.len() > MAX_PROJECT_DOCUMENT_BYTES {
        return Err(ProjectError::new(
            "PROJECT002",
            format!("{role} exceeds 1 MiB"),
        ));
    }
    let text = std::str::from_utf8(bytes).map_err(|error| {
        ProjectError::new("PROJECT003", format!("{role} is not UTF-8: {error}"))
    })?;
    let document: Table = toml::from_str(text)
        .map_err(|error| ProjectError::new("PROJECT003", format!("invalid {role}: {error}")))?;
    validate_document_depth(&document, role)?;
    Ok(document)
}

fn validate_document_depth(document: &Table, role: &str) -> Result<(), ProjectError> {
    let mut pending = document
        .values()
        .map(|value| (value, 2usize))
        .collect::<Vec<_>>();
    while let Some((value, depth)) = pending.pop() {
        match value {
            Value::Array(values) => {
                if depth > MAX_PROJECT_DOCUMENT_DEPTH {
                    return Err(ProjectError::new(
                        "PROJECT026",
                        format!("{role} nesting exceeds {MAX_PROJECT_DOCUMENT_DEPTH}"),
                    ));
                }
                pending.extend(values.iter().map(|value| (value, depth + 1)));
            }
            Value::Table(table) => {
                if depth > MAX_PROJECT_DOCUMENT_DEPTH {
                    return Err(ProjectError::new(
                        "PROJECT026",
                        format!("{role} nesting exceeds {MAX_PROJECT_DOCUMENT_DEPTH}"),
                    ));
                }
                pending.extend(table.values().map(|value| (value, depth + 1)));
            }
            _ => {}
        }
    }
    Ok(())
}

fn schema_one(table: &Table) -> Result<(), ProjectError> {
    if table.get("schema_version").and_then(Value::as_integer) == Some(1) {
        Ok(())
    } else {
        Err(ProjectError::new(
            "PROJECT001",
            "schema_version must be exactly 1",
        ))
    }
}

fn exact_keys(table: &Table, allowed: &[&str], role: &str) -> Result<(), ProjectError> {
    if let Some(key) = table.keys().find(|key| !allowed.contains(&key.as_str())) {
        Err(ProjectError::new(
            "PROJECT007",
            format!("unknown field `{key}` in {role}"),
        ))
    } else {
        Ok(())
    }
}

fn required_table<'a>(table: &'a Table, key: &str) -> Result<&'a Table, ProjectError> {
    table
        .get(key)
        .and_then(Value::as_table)
        .ok_or_else(|| ProjectError::new("PROJECT008", format!("`{key}` must be a table")))
}

fn required_string<'a>(table: &'a Table, key: &str) -> Result<&'a str, ProjectError> {
    table
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ProjectError::new("PROJECT009", format!("`{key}` must be a string")))
}

fn optional_string<'a>(table: &'a Table, key: &str) -> Result<Option<&'a str>, ProjectError> {
    table
        .get(key)
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| ProjectError::new("PROJECT009", format!("`{key}` must be a string")))
        })
        .transpose()
}

fn required_bool(table: &Table, key: &str) -> Result<bool, ProjectError> {
    table
        .get(key)
        .and_then(Value::as_bool)
        .ok_or_else(|| ProjectError::new("PROJECT010", format!("`{key}` must be a boolean")))
}

fn required_unique_strings(table: &Table, key: &str) -> Result<Vec<String>, ProjectError> {
    let values = table
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| ProjectError::new("PROJECT011", format!("`{key}` must be an array")))?;
    if values.len() > MAX_PROJECT_ENTRIES {
        return Err(ProjectError::new(
            "PROJECT019",
            format!("`{key}` has more than {MAX_PROJECT_ENTRIES} entries"),
        ));
    }
    let strings = values
        .iter()
        .map(|value| {
            value.as_str().map(str::to_owned).ok_or_else(|| {
                ProjectError::new("PROJECT011", format!("`{key}` must contain strings"))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let unique = strings.iter().collect::<BTreeSet<_>>();
    if unique.len() != strings.len() {
        return Err(ProjectError::new(
            "PROJECT011",
            format!("`{key}` contains duplicates"),
        ));
    }
    Ok(strings)
}

fn parse_named_tables<T>(
    document: &Table,
    key: &str,
    mut parse: impl FnMut(&str, &Table) -> Result<T, ProjectError>,
) -> Result<BTreeMap<String, T>, ProjectError> {
    let Some(value) = document.get(key) else {
        return Ok(BTreeMap::new());
    };
    let tables = value
        .as_table()
        .ok_or_else(|| ProjectError::new("PROJECT008", format!("`{key}` must be a table")))?;
    if tables.len() > MAX_PROJECT_ENTRIES {
        return Err(ProjectError::new(
            "PROJECT019",
            format!("`{key}` has more than {MAX_PROJECT_ENTRIES} entries"),
        ));
    }
    let mut output = BTreeMap::new();
    for (id, value) in tables {
        if !is_opaal_identifier(id) {
            return Err(ProjectError::new(
                "PROJECT020",
                format!("invalid {key} identifier `{id}`"),
            ));
        }
        let table = value.as_table().ok_or_else(|| {
            ProjectError::new("PROJECT008", format!("`{key}.{id}` must be a table"))
        })?;
        output.insert(id.clone(), parse(id, table)?);
    }
    Ok(output)
}

fn optional_named_table_len(document: &Table, key: &str) -> Result<usize, ProjectError> {
    document
        .get(key)
        .map(|value| {
            value
                .as_table()
                .map(Table::len)
                .ok_or_else(|| ProjectError::new("PROJECT008", format!("`{key}` must be a table")))
        })
        .transpose()
        .map(Option::unwrap_or_default)
}

fn resolve_contained(
    base: &Path,
    raw: &str,
    root: &Path,
    role: &str,
) -> Result<PathBuf, ProjectError> {
    let relative = Path::new(raw);
    if relative.as_os_str().is_empty() || relative.is_absolute() {
        return Err(ProjectError::new(
            "PROJECT006",
            format!("{role} must be a nonempty relative path"),
        ));
    }
    let mut normalized = PathBuf::from(base);
    for component in relative.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(value) => normalized.push(value),
            Component::ParentDir => {
                if !normalized.pop() || !normalized.starts_with(root) {
                    return Err(ProjectError::new(
                        "PROJECT006",
                        format!("{role} escapes the project root"),
                    ));
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(ProjectError::new(
                    "PROJECT006",
                    format!("{role} is not project-relative"),
                ));
            }
        }
    }
    if !normalized.starts_with(root) {
        return Err(ProjectError::new(
            "PROJECT006",
            format!("{role} escapes the project root"),
        ));
    }
    Ok(normalized)
}

fn validate_authority_scope(
    manifest: &ProjectManifest,
    effect: &str,
    scope: &str,
) -> Result<(), ProjectError> {
    let valid = match effect {
        "filesystem.read" => scope == "project.root",
        "filesystem.write" => scope == "project.evidence",
        "process.run" => scope
            .strip_prefix("tool.")
            .is_some_and(|id| manifest.tools.contains_key(id)),
        "network.http" => scope
            .strip_prefix("endpoint.")
            .is_some_and(|id| manifest.endpoints.contains_key(id)),
        "secret.reveal" => scope
            .split_once("@endpoint.")
            .is_some_and(|(secret, endpoint)| {
                secret
                    .strip_prefix("secret.")
                    .is_some_and(|id| manifest.secrets.contains_key(id))
                    && manifest.endpoints.contains_key(endpoint)
            }),
        "clock.wall" | "clock.monotonic" => scope == "evaluation",
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(ProjectError::new(
            "AUTH009",
            format!("effect `{effect}` and scope `{scope}` do not name one exact project request"),
        ))
    }
}

fn parse_native_bytes(table: &Table) -> Result<NativeBytes, ProjectError> {
    exact_keys(table, &["encoding", "platform", "value"], "native bytes")?;
    if required_string(table, "encoding")? != "base64url-nopad" {
        return Err(ProjectError::new(
            "LOCK016",
            "native bytes require base64url-nopad encoding",
        ));
    }
    let platform = required_string(table, "platform")?.to_owned();
    if platform != "unix" {
        return Err(ProjectError::new(
            "LOCK017",
            "OPAAL 1.0 host locks require platform = \"unix\"",
        ));
    }
    let value = required_string(table, "value")?.to_owned();
    if !is_base64url_nopad(&value) {
        return Err(ProjectError::new(
            "LOCK018",
            "invalid unpadded base64url bytes",
        ));
    }
    let bytes = decode_base64url_nopad(&value)
        .ok_or_else(|| ProjectError::new("LOCK018", "invalid unpadded base64url bytes"))?;
    Ok(NativeBytes {
        platform,
        base64url_nopad: value,
        bytes,
    })
}

fn decode_base64url_nopad(value: &str) -> Option<Vec<u8>> {
    let digits = value
        .bytes()
        .map(base64url_digit)
        .collect::<Option<Vec<_>>>()?;
    let mut output = Vec::with_capacity(digits.len().saturating_mul(3) / 4);
    for chunk in digits.chunks(4) {
        let first = *chunk.first()?;
        let second = *chunk.get(1)?;
        output.push((first << 2) | (second >> 4));
        if let Some(third) = chunk.get(2) {
            output.push((second << 4) | (third >> 2));
            if let Some(fourth) = chunk.get(3) {
                output.push((third << 6) | fourth);
            }
        }
    }
    Some(output)
}

fn sha256_digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn is_opaal_identifier(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn is_portable_identifier(value: &str) -> bool {
    is_opaal_identifier(value)
}

fn is_http_method(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().any(|byte| byte.is_ascii_uppercase())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte == b'-')
}

fn is_header_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn is_literal_loopback(url: &str) -> bool {
    parse_http_url(url).is_some_and(|parsed| {
        parsed.scheme == "http"
            && (parsed
                .host
                .parse::<Ipv4Addr>()
                .is_ok_and(|address| address.is_loopback())
                || parsed
                    .host
                    .parse::<Ipv6Addr>()
                    .is_ok_and(|address| address.is_loopback()))
    })
}

struct ParsedHttpUrl<'a> {
    scheme: &'a str,
    host: &'a str,
}

fn parse_http_url(url: &str) -> Option<ParsedHttpUrl<'_>> {
    if url
        .bytes()
        .any(|byte| byte.is_ascii_control() || byte == b' ')
        || url.contains('#')
    {
        return None;
    }
    let (scheme, remainder) = url.split_once("://")?;
    if !matches!(scheme, "http" | "https") {
        return None;
    }
    if !valid_url_remainder(remainder) {
        return None;
    }
    let authority = remainder.split(['/', '?']).next()?;
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let (host, suffix) = bracketed.split_once(']')?;
        if host.parse::<Ipv6Addr>().is_err() {
            return None;
        }
        let port = if suffix.is_empty() {
            None
        } else {
            Some(suffix.strip_prefix(':')?)
        };
        (host, port)
    } else if let Some((host, port)) = authority.rsplit_once(':') {
        if host.contains(':') {
            return None;
        }
        (host, Some(port))
    } else {
        (authority, None)
    };
    if host.is_empty() || !valid_url_host(host) || port.is_some_and(|port| !valid_port(port)) {
        return None;
    }
    Some(ParsedHttpUrl { scheme, host })
}

fn valid_url_remainder(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if !byte.is_ascii()
            || byte.is_ascii_control()
            || matches!(byte, b' ' | b'\\' | b'"' | b'<' | b'>' | b'`')
        {
            return false;
        }
        if byte == b'%' {
            let Some(high) = bytes.get(index + 1) else {
                return false;
            };
            let Some(low) = bytes.get(index + 2) else {
                return false;
            };
            if !high.is_ascii_hexdigit()
                || !low.is_ascii_hexdigit()
                || high.is_ascii_lowercase()
                || low.is_ascii_lowercase()
            {
                return false;
            }
            let decoded = (ascii_hex_value(*high) << 4) | ascii_hex_value(*low);
            if decoded < 0x20
                || decoded == 0x7f
                || matches!(
                    decoded,
                    0x20 | 0x22 | 0x3c | 0x3e | 0x5c | 0x5e | 0x60 | 0x7b | 0x7c | 0x7d
                )
            {
                return false;
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    true
}

fn ascii_hex_value(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'A'..=b'F' => byte - b'A' + 10,
        _ => 0,
    }
}

fn valid_url_host(host: &str) -> bool {
    host.parse::<Ipv4Addr>().is_ok() || host.parse::<Ipv6Addr>().is_ok() || is_dns_name(host)
}

fn valid_port(port: &str) -> bool {
    !port.is_empty()
        && port.bytes().all(|byte| byte.is_ascii_digit())
        && port.parse::<u16>().is_ok_and(|port| port != 0)
}

fn is_dns_name(name: &str) -> bool {
    !name.is_empty()
        && !name
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
        && name.len() <= 253
        && !name.ends_with('.')
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'.')
        })
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
}

fn is_sha256(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    })
}

fn is_base64url_nopad(value: &str) -> bool {
    let Some(last) = value
        .bytes()
        .map(base64url_digit)
        .collect::<Option<Vec<_>>>()
    else {
        return false;
    };
    match last.len() % 4 {
        0 => true,
        2 => last.last().is_some_and(|digit| digit & 0b00_1111 == 0),
        3 => last.last().is_some_and(|digit| digit & 0b00_0011 == 0),
        _ => false,
    }
}

fn base64url_digit(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'-' => Some(62),
        b'_' => Some(63),
        _ => None,
    }
}

/// One explicit project's completely analyzed check-only authoring surface.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectProgram {
    manifest: ProjectManifest,
    modules: ModuleProgram,
    tasks: BTreeMap<String, TaskSignature>,
}

impl ProjectProgram {
    #[must_use]
    pub const fn manifest(&self) -> &ProjectManifest {
        &self.manifest
    }

    #[must_use]
    pub const fn modules(&self) -> &ModuleProgram {
        &self.modules
    }

    #[must_use]
    pub const fn tasks(&self) -> &BTreeMap<String, TaskSignature> {
        &self.tasks
    }

    pub fn task(&self, name: &str) -> Result<&TaskSignature, ProjectError> {
        self.tasks
            .get(name)
            .ok_or_else(|| ProjectError::new("TASK004", format!("unknown task `{name}`")))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskId {
    project: ProjectId,
    name: String,
}

impl TaskId {
    #[must_use]
    pub const fn project_id(&self) -> &ProjectId {
        &self.project
    }

    #[must_use]
    pub fn project(&self) -> &str {
        self.project.name()
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn qualified_name(&self) -> String {
        format!("{}::{}", self.project.name, self.name)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskSignature {
    id: TaskId,
    action: ActionSignature,
    effects: Vec<ProjectEffect>,
}

impl TaskSignature {
    #[must_use]
    pub const fn id(&self) -> &TaskId {
        &self.id
    }

    #[must_use]
    pub const fn action(&self) -> &ActionSignature {
        &self.action
    }

    #[must_use]
    pub fn effects(&self) -> &[ProjectEffect] {
        &self.effects
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ProjectEffect {
    capability: String,
    scope: String,
}

impl ProjectEffect {
    #[must_use]
    pub fn capability(&self) -> &str {
        &self.capability
    }

    #[must_use]
    pub fn scope(&self) -> &str {
        &self.scope
    }
}

#[derive(Debug)]
pub enum ProjectProgramError {
    Module(ModuleProgramLoadError),
    Contract(ProjectError),
}

impl fmt::Display for ProjectProgramError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Module(error) => error.fmt(formatter),
            Self::Contract(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ProjectProgramError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Module(error) => Some(error),
            Self::Contract(error) => Some(error),
        }
    }
}

struct ProjectModuleSources<'a> {
    source_loader: &'a dyn ModuleSourceLoader,
    generated: BTreeMap<String, Vec<u8>>,
}

impl ModuleSourceLoader for ProjectModuleSources<'_> {
    fn load(&self, module: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        self.load_bounded(module, usize::MAX)
    }

    fn load_bounded(
        &self,
        module: &ModuleId,
        maximum: usize,
    ) -> Result<Vec<u8>, ModuleSourceError> {
        if let ModuleOrigin::Standard { namespace, module } = module.origin()
            && namespace == "project"
        {
            let bytes = self.generated.get(module).ok_or_else(|| {
                ModuleSourceError::new(format!("unknown declarative project module `{module}`"))
            })?;
            return Ok(bytes[..bytes.len().min(maximum)].to_vec());
        }
        self.source_loader.load_bounded(module, maximum)
    }
}

/// Loads one explicitly selected project without executing source or adapters.
pub fn load_project_program(
    manifest: ProjectManifest,
    canonicalizer: &dyn ModuleCanonicalizer,
    source_loader: &dyn ModuleSourceLoader,
) -> Result<ProjectProgram, ProjectProgramError> {
    let current =
        Version::parse(crate::version()).expect("the package version is canonical SemVer");
    if !manifest.required_opaal.matches(&current) {
        return Err(ProjectProgramError::Contract(ProjectError::new(
            "PROJECT022",
            format!(
                "project requires OPAAL `{}`, current version is `{current}`",
                manifest.required_opaal
            ),
        )));
    }
    let sources = ProjectModuleSources {
        source_loader,
        generated: generated_project_modules(&manifest),
    };
    let modules = ModuleProgramLoader::for_project(canonicalizer, &sources)
        .load_for_frontend(&manifest.root_module)
        .map_err(ProjectProgramError::Module)?;
    reject_project_value_references(&modules).map_err(ProjectProgramError::Contract)?;
    let tasks = collect_tasks(&manifest, &modules).map_err(ProjectProgramError::Contract)?;
    Ok(ProjectProgram {
        manifest,
        modules,
        tasks,
    })
}

fn reject_project_value_references(modules: &ModuleProgram) -> Result<(), ProjectError> {
    for entry in modules.sources().entries() {
        for reference in modules.names().references(entry.module()) {
            if let crate::module::ModuleReferenceTarget::Imported { target_module, .. } =
                reference.target()
                && matches!(
                    target_module.origin(),
                    ModuleOrigin::Standard { namespace, .. } if namespace == "project"
                )
            {
                return Err(ProjectError::new(
                    "PROJECT027",
                    format!(
                        "declarative project identity `{}` cannot be used as an ordinary value",
                        reference.name()
                    ),
                ));
            }
        }
    }
    Ok(())
}

fn generated_project_modules(manifest: &ProjectManifest) -> BTreeMap<String, Vec<u8>> {
    BTreeMap::from([
        (
            "context".to_owned(),
            generated_binding_module(["root", "evidence"]),
        ),
        (
            "tools".to_owned(),
            generated_binding_module(manifest.tools.keys().map(String::as_str)),
        ),
        (
            "endpoints".to_owned(),
            generated_binding_module(manifest.endpoints.keys().map(String::as_str)),
        ),
        (
            "secrets".to_owned(),
            generated_binding_module(manifest.secrets.keys().map(String::as_str)),
        ),
    ])
}

fn generated_binding_module<'a>(names: impl IntoIterator<Item = &'a str>) -> Vec<u8> {
    let names = names.into_iter().collect::<Vec<_>>();
    let mut source = String::new();
    if !names.is_empty() {
        source.push_str("export { ");
        source.push_str(&names.join(", "));
        source.push_str(" }\n");
    }
    for name in names {
        source.push_str("let ");
        source.push_str(name);
        source.push_str(": String = \"\"\n");
    }
    source.into_bytes()
}

fn collect_tasks(
    manifest: &ProjectManifest,
    modules: &ModuleProgram,
) -> Result<BTreeMap<String, TaskSignature>, ProjectError> {
    let root = modules.graph().root();
    let mut tasks = BTreeMap::new();
    for entry in modules.sources().entries() {
        for statement in entry.script().statements() {
            let opaal_syntax::StatementKind::Task(task) = statement.kind() else {
                continue;
            };
            if entry.module() != root {
                return Err(ProjectError::new(
                    "TASK001",
                    "task declarations are allowed only in the project root module",
                ));
            }
            let name = entry
                .source()
                .slice(task.name.span())
                .expect("task name belongs to root source")
                .to_owned();
            let action = resolve_task_action(entry.module(), entry.source(), task, modules)?;
            if let Some(parameter) = action
                .callable()
                .parameters()
                .iter()
                .find(|parameter| !is_task_input_type(parameter.value_type()))
            {
                return Err(ProjectError::new(
                    "TASK005",
                    format!(
                        "task `{name}` parameter `{}` has type `{}`, which has no exact name=value representation",
                        parameter.name(),
                        parameter.value_type()
                    ),
                ));
            }
            let effects = action
                .effects()
                .iter()
                .map(|effect| bind_project_effect(manifest, effect))
                .collect::<Result<Vec<_>, _>>()?;
            let signature = TaskSignature {
                id: TaskId {
                    project: manifest.id.clone(),
                    name: name.clone(),
                },
                action: action.clone(),
                effects,
            };
            if tasks.insert(name.clone(), signature).is_some() {
                return Err(ProjectError::new(
                    "TASK002",
                    format!("duplicate task `{name}`"),
                ));
            }
        }
    }
    Ok(tasks)
}

fn is_task_input_type(value_type: &ValueType) -> bool {
    matches!(
        value_type,
        ValueType::Null
            | ValueType::Bool
            | ValueType::Int
            | ValueType::Float
            | ValueType::String
            | ValueType::Bytes
            | ValueType::Path
            | ValueType::Duration
            | ValueType::ByteSize
    )
}

fn resolve_task_action<'a>(
    root: &ModuleId,
    source: &opaal_syntax::SourceFile,
    task: &opaal_syntax::TaskDefinition,
    modules: &'a ModuleProgram,
) -> Result<&'a ActionSignature, ProjectError> {
    let segments = task
        .action
        .segments
        .iter()
        .map(|segment| {
            source
                .slice(segment.span())
                .expect("task action name belongs to root source")
        })
        .collect::<Vec<_>>();
    let (name, qualifiers) = segments
        .split_last()
        .expect("qualified action has at least one segment");
    let owner = if qualifiers.is_empty() {
        root
    } else {
        modules.aliases().resolve(root, qualifiers).ok_or_else(|| {
            ProjectError::new(
                "TASK003",
                format!("unknown action qualifier `{}`", qualifiers.join("::")),
            )
        })?
    };
    if !qualifiers.is_empty() && modules.names().export(owner, name).is_none() {
        return Err(ProjectError::new(
            "TASK003",
            format!("action `{}` is not exported", segments.join("::")),
        ));
    }
    modules.actions().action(owner, name).ok_or_else(|| {
        ProjectError::new(
            "TASK003",
            format!("task target `{}` is not an action", segments.join("::")),
        )
    })
}

fn bind_project_effect(
    manifest: &ProjectManifest,
    effect: &crate::module::DeclaredEffect,
) -> Result<ProjectEffect, ProjectError> {
    let references = effect
        .arguments()
        .iter()
        .enumerate()
        .map(|(index, argument)| {
            effect.project_binding(index).ok_or_else(|| {
                ProjectError::new(
                    "ACT012",
                    format!(
                        "effect argument `{argument}` is not an explicitly imported project identity"
                    ),
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let scope = match effect.capability() {
        "filesystem.read" if references.as_slice() == [("context", "root")] => {
            "project.root".to_owned()
        }
        "filesystem.write" if references.as_slice() == [("context", "evidence")] => {
            "project.evidence".to_owned()
        }
        "process.run" => {
            let [(module, id)] = references.as_slice() else {
                return Err(ProjectError::new("ACT005", "invalid process tool scope"));
            };
            if *module != "tools" || !manifest.tools.contains_key(*id) {
                return Err(ProjectError::new("ACT005", "unknown process tool scope"));
            }
            format!("tool.{id}")
        }
        "network.http" => {
            let [(module, id)] = references.as_slice() else {
                return Err(ProjectError::new(
                    "ACT005",
                    "invalid network endpoint scope",
                ));
            };
            if *module != "endpoints" || !manifest.endpoints.contains_key(*id) {
                return Err(ProjectError::new(
                    "ACT005",
                    "unknown network endpoint scope",
                ));
            }
            format!("endpoint.{id}")
        }
        "secret.reveal" => {
            let [(secret_module, secret), (endpoint_module, endpoint)] = references.as_slice()
            else {
                return Err(ProjectError::new("ACT005", "invalid secret sink scope"));
            };
            if *secret_module != "secrets"
                || *endpoint_module != "endpoints"
                || !manifest.secrets.contains_key(*secret)
                || !manifest.endpoints.contains_key(*endpoint)
            {
                return Err(ProjectError::new("ACT005", "unknown secret sink scope"));
            }
            format!("secret.{secret}@endpoint.{endpoint}")
        }
        "clock.wall" | "clock.monotonic" if references.is_empty() => "evaluation".to_owned(),
        _ => {
            return Err(ProjectError::new(
                "ACT005",
                format!("invalid static scope for `{}`", effect.capability()),
            ));
        }
    };
    Ok(ProjectEffect {
        capability: effect.capability().to_owned(),
        scope,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectCheck {
    task: TaskSignature,
    inputs: BTreeMap<String, String>,
    downstream: crate::seam::DownstreamCallMetadata,
}

impl ProjectCheck {
    #[must_use]
    pub const fn task(&self) -> &TaskSignature {
        &self.task
    }

    #[must_use]
    pub const fn inputs(&self) -> &BTreeMap<String, String> {
        &self.inputs
    }

    #[must_use]
    pub const fn downstream(&self) -> &crate::seam::DownstreamCallMetadata {
        &self.downstream
    }
}

/// Validates one task selection and all declarative grants without host observation.
pub fn check_project(
    project: &ProjectProgram,
    task: &str,
    environment: &str,
    authority: &AuthorityDocument,
    tools: &ToolLock,
    inputs: impl IntoIterator<Item = (String, String)>,
) -> Result<ProjectCheck, ProjectError> {
    let selected_environment = project.manifest.environment(environment)?;
    if authority.project != project.manifest.id.name
        || authority.environment != environment
        || tools.project != project.manifest.id.name
        || tools.environment != environment
    {
        return Err(ProjectError::new(
            "CHECK001",
            "authority or tool lock identity differs from the selected project/environment",
        ));
    }
    let task = project.task(task)?;
    let mut supplied = BTreeMap::new();
    for (name, value) in inputs {
        if supplied.contains_key(&name) {
            return Err(ProjectError::new(
                "CHECK002",
                format!("duplicate input `{name}`"),
            ));
        }
        if supplied.len() == MAX_PROJECT_INPUTS {
            return Err(ProjectError::new(
                "CHECK007",
                format!("task inputs exceed the maximum of {MAX_PROJECT_INPUTS}"),
            ));
        }
        supplied.insert(name, value);
    }
    let expected = task
        .action
        .callable()
        .parameters()
        .iter()
        .map(|parameter| parameter.name())
        .collect::<BTreeSet<_>>();
    if let Some(name) = supplied
        .keys()
        .find(|name| !expected.contains(name.as_str()))
    {
        return Err(ProjectError::new(
            "CHECK003",
            format!("unknown input `{name}`"),
        ));
    }
    if let Some(name) = expected.iter().find(|name| !supplied.contains_key(**name)) {
        return Err(ProjectError::new(
            "CHECK004",
            format!("missing required input `{name}`"),
        ));
    }
    for parameter in task.action.callable().parameters() {
        let value = supplied
            .get(parameter.name())
            .expect("unknown and missing task inputs were rejected");
        validate_input_text(parameter.value_type(), value).map_err(|reason| {
            ProjectError::new(
                "CHECK008",
                format!(
                    "input `{}` is not an exact {} value: {reason}",
                    parameter.name(),
                    parameter.value_type()
                ),
            )
        })?;
    }
    for effect in &task.effects {
        let matching = authority
            .rules
            .iter()
            .find(|rule| rule.effect == effect.capability && rule.scope == effect.scope);
        match matching {
            Some(AuthorityDocumentRule {
                decision: AuthorityDecision::Grant,
                ..
            }) => {}
            Some(_) => {
                return Err(ProjectError::new(
                    "CHECK005",
                    format!(
                        "request `{}` `{}` is explicitly denied",
                        effect.capability, effect.scope
                    ),
                ));
            }
            None => {
                return Err(ProjectError::new(
                    "CHECK005",
                    format!(
                        "request `{}` `{}` has no exact grant",
                        effect.capability, effect.scope
                    ),
                ));
            }
        }
    }
    Ok(ProjectCheck {
        task: task.clone(),
        inputs: supplied,
        downstream: crate::seam::DownstreamCallMetadata::foundation()
            .with_action(task.action.id().clone())
            .with_project_task(
                project.manifest.id.clone(),
                task.id.clone(),
                selected_environment.id.clone(),
            ),
    })
}

fn validate_input_text(value_type: &ValueType, value: &str) -> Result<(), &'static str> {
    match value_type {
        ValueType::Null if value == "null" => Ok(()),
        ValueType::Bool if matches!(value, "true" | "false") => Ok(()),
        ValueType::Int if value.parse::<i64>().is_ok() => Ok(()),
        ValueType::Float if value.parse::<f64>().is_ok_and(|number| number.is_finite()) => Ok(()),
        ValueType::String => Ok(()),
        ValueType::Bytes if is_base64url_nopad(value) => Ok(()),
        ValueType::Path if !value.contains('\0') => Ok(()),
        ValueType::Duration
            if value
                .strip_suffix("ns")
                .is_some_and(|number| number.parse::<u128>().is_ok()) =>
        {
            Ok(())
        }
        ValueType::ByteSize
            if value
                .strip_suffix('b')
                .is_some_and(|number| number.parse::<u64>().is_ok()) =>
        {
            Ok(())
        }
        ValueType::Null
        | ValueType::Bool
        | ValueType::Int
        | ValueType::Float
        | ValueType::Bytes
        | ValueType::Path
        | ValueType::Duration
        | ValueType::ByteSize => Err("value has invalid canonical text"),
        ValueType::Any
        | ValueType::List(_)
        | ValueType::Record
        | ValueType::Table
        | ValueType::Range
        | ValueType::Status
        | ValueType::Error
        | ValueType::Function
        | ValueType::Closure
        | ValueType::TypeParameter(_)
        | ValueType::Nominal { .. } => Err("type has no exact name=value representation"),
    }
}
