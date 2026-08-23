//! Secure, transactional startup configuration for the interactive client.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use flash_runtime::eval::{
    CancelReason, Cancellation, CancellationToken, Completion, EvalLimits, Instant, ResourceBudget,
    RestrictedCapability, RuntimeError, RuntimeErrorKind, SystemClock, evaluate_in_environment,
};
use flash_runtime::plan::SessionOptions;
use flash_runtime::{BindingMutability, Environment, ScopeStack, Value};
use flash_syntax::{
    Diagnostic, IncompleteInput, LabelStyle, ParseOutcome, Severity, SourceFile, SourceId, Span,
    parse, render_diagnostic,
};

use crate::editor::EditorPrompt;

/// Maximum bytes read from one automatic startup source.
pub const DEFAULT_CONFIG_SOURCE_LIMIT: usize = 256 * 1024;
/// Maximum evaluator charges allowed to one automatic startup source.
pub const DEFAULT_CONFIG_STEP_LIMIT: u64 = 100_000;
/// Host evaluation deadline measured from construction of the limits.
pub const DEFAULT_CONFIG_DEADLINE_NANOS: u64 = 250_000_000;

const PIPEFAIL_SETTING: &str = "pipefail";
const CAPTURE_LIMIT_SETTING: &str = "capture_limit";
const COMPLETION_SETTING: &str = "completion";
const HISTORY_SETTING: &str = "history";
const PROMPT_SETTING: &str = "prompt";
const CONTINUATION_PROMPT_SETTING: &str = "continuation_prompt";

/// Invocation families considered before any config discovery.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigInvocation {
    Interactive,
    Script,
    Command,
    BatchStdin,
    Check,
    Format,
    Help,
    Version,
}

/// Native user-config path convention.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigPlatform {
    Linux,
    MacOs,
    FlashOs,
}

impl ConfigPlatform {
    /// Path convention for the current supported host.
    #[must_use]
    pub const fn current() -> Self {
        #[cfg(target_os = "linux")]
        {
            Self::Linux
        }
        #[cfg(target_os = "macos")]
        {
            Self::MacOs
        }
        #[cfg(target_os = "redox")]
        {
            Self::FlashOs
        }
    }
}

/// Immutable startup request after CLI mode classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConfigRequest {
    invocation: ConfigInvocation,
    no_config: bool,
    platform: ConfigPlatform,
}

impl ConfigRequest {
    #[must_use]
    pub const fn new(
        invocation: ConfigInvocation,
        no_config: bool,
        platform: ConfigPlatform,
    ) -> Self {
        Self {
            invocation,
            no_config,
            platform,
        }
    }
}

/// Environment lookup seam used before config file access.
pub trait ConfigEnvironment {
    fn value(&self, name: &OsStr) -> Option<OsString>;
}

/// Process environment adapter for final interactive CLI wiring.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessConfigEnvironment;

impl ConfigEnvironment for ProcessConfigEnvironment {
    fn value(&self, name: &OsStr) -> Option<OsString> {
        std::env::var_os(name)
    }
}

/// Exact result of loading the one selected file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfigFile {
    Absent,
    Source(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConfigFileErrorKind {
    Trust,
    Read,
    Budget,
}

/// A host-file failure before parsing or evaluation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigFileError {
    kind: ConfigFileErrorKind,
    detail: String,
}

impl ConfigFileError {
    #[must_use]
    pub fn trust(detail: impl Into<String>) -> Self {
        Self {
            kind: ConfigFileErrorKind::Trust,
            detail: detail.into(),
        }
    }

    #[must_use]
    pub fn read(detail: impl Into<String>) -> Self {
        Self {
            kind: ConfigFileErrorKind::Read,
            detail: detail.into(),
        }
    }

    #[must_use]
    pub fn budget(detail: impl Into<String>) -> Self {
        Self {
            kind: ConfigFileErrorKind::Budget,
            detail: detail.into(),
        }
    }

    #[must_use]
    pub const fn is_trust_failure(&self) -> bool {
        matches!(self.kind, ConfigFileErrorKind::Trust)
    }

    #[must_use]
    pub const fn is_read_failure(&self) -> bool {
        matches!(self.kind, ConfigFileErrorKind::Read)
    }

    #[must_use]
    pub const fn is_budget_failure(&self) -> bool {
        matches!(self.kind, ConfigFileErrorKind::Budget)
    }

    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

/// Source loading seam; implementations must inspect and read the same object.
pub trait ConfigSource {
    fn load(&self, path: &Path, source_limit: usize) -> Result<ConfigFile, ConfigFileError>;
}

/// Same-handle Unix config loader.
#[derive(Clone, Copy, Debug, Default)]
pub struct HostConfigSource;

impl ConfigSource for HostConfigSource {
    fn load(&self, path: &Path, source_limit: usize) -> Result<ConfigFile, ConfigFileError> {
        let descriptor = match rustix::fs::open(
            path,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        ) {
            Ok(descriptor) => descriptor,
            Err(error) if error == rustix::io::Errno::NOENT => return Ok(ConfigFile::Absent),
            Err(error) => {
                return Err(ConfigFileError::read(format!(
                    "cannot open {}: {error}",
                    path.display()
                )));
            }
        };
        let mut file = File::from(descriptor);
        verify_opened_file(&file, path)?;

        let read_limit = u64::try_from(source_limit.saturating_add(1)).unwrap_or(u64::MAX);
        let mut bytes = Vec::with_capacity(source_limit.min(8 * 1024));
        (&mut file)
            .take(read_limit)
            .read_to_end(&mut bytes)
            .map_err(|error| {
                ConfigFileError::read(format!("cannot read {}: {error}", path.display()))
            })?;
        if bytes.len() > source_limit {
            return Err(ConfigFileError::budget(format!(
                "config source exceeds the {source_limit}-byte limit"
            )));
        }
        String::from_utf8(bytes)
            .map(ConfigFile::Source)
            .map_err(|error| {
                ConfigFileError::read(format!(
                    "config source is not UTF-8 at byte {}",
                    error.utf8_error().valid_up_to()
                ))
            })
    }
}

fn verify_opened_file(file: &File, path: &Path) -> Result<(), ConfigFileError> {
    let metadata = file.metadata().map_err(|error| {
        ConfigFileError::trust(format!("cannot inspect {}: {error}", path.display()))
    })?;
    if !metadata.is_file() {
        return Err(ConfigFileError::trust(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    if metadata.uid() != rustix::process::geteuid().as_raw() {
        return Err(ConfigFileError::trust(format!(
            "{} is not owned by the effective user",
            path.display()
        )));
    }
    if metadata.mode() & 0o022 != 0 {
        return Err(ConfigFileError::trust(format!(
            "{} is writable by group or other users",
            path.display()
        )));
    }
    Ok(())
}

/// Deterministic clean session state used as the transaction base.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigDefaults {
    scope: ScopeStack,
    environment: Environment,
}

impl ConfigDefaults {
    #[must_use]
    pub const fn new(scope: ScopeStack, environment: Environment) -> Self {
        Self { scope, environment }
    }
}

impl Default for ConfigDefaults {
    fn default() -> Self {
        Self::new(ScopeStack::new(), Environment::new())
    }
}

/// Source, step, and cancellation limits for one startup evaluation.
#[derive(Clone, Debug)]
pub struct ConfigLimits {
    source_limit: usize,
    budget: ResourceBudget,
    cancellation: CancellationToken,
}

impl ConfigLimits {
    #[must_use]
    pub const fn new(
        source_limit: usize,
        budget: ResourceBudget,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            source_limit,
            budget,
            cancellation,
        }
    }

    /// Deterministic built-in limits without a wall clock, for host-free tests.
    #[must_use]
    pub fn test_default() -> Self {
        Self::new(
            DEFAULT_CONFIG_SOURCE_LIMIT,
            ResourceBudget::steps(DEFAULT_CONFIG_STEP_LIMIT),
            CancellationToken::never(),
        )
    }
}

impl Default for ConfigLimits {
    fn default() -> Self {
        let clock = SystemClock::new();
        let cancellation =
            CancellationToken::deadline(clock, Instant::from_nanos(DEFAULT_CONFIG_DEADLINE_NANOS));
        Self::new(
            DEFAULT_CONFIG_SOURCE_LIMIT,
            ResourceBudget::steps(DEFAULT_CONFIG_STEP_LIMIT),
            cancellation,
        )
    }
}

/// Stable high-level config failure category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigFailureKind {
    ConfigPathUnavailable,
    ConfigTrust,
    ConfigRead,
    ConfigParse,
    RestrictedStartup,
    ConfigEvaluation,
    ConfigBudget,
}

impl ConfigFailureKind {
    const fn name(self) -> &'static str {
        match self {
            Self::ConfigPathUnavailable => "ConfigPathUnavailable",
            Self::ConfigTrust => "ConfigTrust",
            Self::ConfigRead => "ConfigRead",
            Self::ConfigParse => "ConfigParse",
            Self::RestrictedStartup => "RestrictedStartup",
            Self::ConfigEvaluation => "ConfigEvaluation",
            Self::ConfigBudget => "ConfigBudget",
        }
    }
}

/// Structured cause retained by safe-mode metadata.
#[derive(Clone, Debug, PartialEq)]
pub struct ConfigFailure {
    kind: ConfigFailureKind,
    detail: String,
    capability: Option<&'static str>,
    span: Option<Span>,
    cause: ConfigFailureCause,
}

/// Typed nested cause retained instead of flattening startup failures to text.
#[derive(Clone, Debug, PartialEq)]
pub enum ConfigFailureCause {
    PathUnavailable,
    File(ConfigFileError),
    Parse(Vec<Diagnostic>),
    Runtime(RuntimeError),
    Cancellation(Cancellation),
    Settings(String),
}

impl ConfigFailure {
    #[must_use]
    pub const fn kind(&self) -> ConfigFailureKind {
        self.kind
    }

    #[must_use]
    pub const fn capability(&self) -> Option<&'static str> {
        self.capability
    }

    #[must_use]
    pub const fn span(&self) -> Option<Span> {
        self.span
    }

    #[must_use]
    pub const fn cause(&self) -> &ConfigFailureCause {
        &self.cause
    }
}

/// Config consideration outcome recorded for the interactive session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigStatus {
    Ineligible,
    Disabled,
    Absent,
    Loaded,
    SafeMode,
}

/// Read-only config metadata retained after startup.
#[derive(Clone, Debug, PartialEq)]
pub struct ConfigMetadata {
    status: ConfigStatus,
    selected_path: Option<PathBuf>,
    failure: Option<ConfigFailure>,
}

impl ConfigMetadata {
    #[must_use]
    pub const fn status(&self) -> ConfigStatus {
        self.status
    }

    #[must_use]
    pub fn selected_path(&self) -> Option<&Path> {
        self.selected_path.as_deref()
    }

    #[must_use]
    pub const fn failure(&self) -> Option<&ConfigFailure> {
        self.failure.as_ref()
    }

    #[must_use]
    pub const fn config_safe_mode(&self) -> bool {
        matches!(self.status, ConfigStatus::SafeMode)
    }
}

/// Transaction result consumed later by final interactive wiring.
#[derive(Clone, Debug, PartialEq)]
pub struct ConfigStartup {
    scope: ScopeStack,
    environment: Environment,
    session_options: SessionOptions,
    interactive_settings: InteractiveSettings,
    metadata: ConfigMetadata,
    prompt: EditorPrompt,
    diagnostic: Option<String>,
}

impl ConfigStartup {
    #[must_use]
    pub const fn scope(&self) -> &ScopeStack {
        &self.scope
    }

    #[must_use]
    pub const fn environment(&self) -> &Environment {
        &self.environment
    }

    #[must_use]
    pub const fn session_options(&self) -> SessionOptions {
        self.session_options
    }

    #[must_use]
    pub const fn interactive_settings(&self) -> InteractiveSettings {
        self.interactive_settings
    }

    #[must_use]
    pub const fn metadata(&self) -> &ConfigMetadata {
        &self.metadata
    }

    #[must_use]
    pub const fn prompt(&self) -> &EditorPrompt {
        &self.prompt
    }

    #[must_use]
    pub fn diagnostic(&self) -> Option<&str> {
        self.diagnostic.as_deref()
    }
}

/// Interactive-only editor policy committed by startup configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InteractiveSettings {
    completion: bool,
    history: bool,
}

impl InteractiveSettings {
    #[must_use]
    pub const fn completion(self) -> bool {
        self.completion
    }

    #[must_use]
    pub const fn history(self) -> bool {
        self.history
    }
}

impl Default for InteractiveSettings {
    fn default() -> Self {
        Self {
            completion: true,
            history: true,
        }
    }
}

/// Fatal startup outcomes that must not degrade into config safe mode.
#[derive(Clone, Debug)]
pub enum ConfigFatalError {
    Cancelled(Cancellation),
    InvalidDefaults(String),
}

/// Discover, load, and transactionally evaluate the one eligible config.
pub fn initialize_config(
    request: ConfigRequest,
    environment: &dyn ConfigEnvironment,
    source: &dyn ConfigSource,
    defaults: &ConfigDefaults,
    limits: &ConfigLimits,
) -> Result<ConfigStartup, ConfigFatalError> {
    if request.invocation != ConfigInvocation::Interactive {
        return Ok(clean_startup(ConfigStatus::Ineligible, None, defaults));
    }
    if request.no_config {
        return Ok(clean_startup(ConfigStatus::Disabled, None, defaults));
    }

    let paths = match config_paths(request.platform, environment) {
        Some(paths) => paths,
        None => {
            return Ok(safe_startup(
                None,
                ConfigFailure {
                    kind: ConfigFailureKind::ConfigPathUnavailable,
                    detail: "no absolute config root or home is available".to_owned(),
                    capability: None,
                    span: None,
                    cause: ConfigFailureCause::PathUnavailable,
                },
                defaults,
            ));
        }
    };

    let (path, text) = match source.load(&paths.primary, limits.source_limit) {
        Ok(ConfigFile::Absent) => match source.load(&paths.legacy, limits.source_limit) {
            Ok(ConfigFile::Absent) => {
                return Ok(clean_startup(
                    ConfigStatus::Absent,
                    Some(paths.primary),
                    defaults,
                ));
            }
            Ok(ConfigFile::Source(text)) => (paths.legacy, text),
            Err(error) => {
                return Ok(safe_startup(
                    Some(paths.legacy),
                    file_failure(error),
                    defaults,
                ));
            }
        },
        Ok(ConfigFile::Source(text)) => (paths.primary, text),
        Err(error) => {
            return Ok(safe_startup(
                Some(paths.primary),
                file_failure(error),
                defaults,
            ));
        }
    };

    if text.len() > limits.source_limit {
        let error = ConfigFileError::budget(format!(
            "config source exceeds the {}-byte limit",
            limits.source_limit
        ));
        return Ok(safe_startup(
            Some(path),
            ConfigFailure {
                kind: ConfigFailureKind::ConfigBudget,
                detail: error.detail.clone(),
                capability: None,
                span: None,
                cause: ConfigFailureCause::File(error),
            },
            defaults,
        ));
    }

    let source_file = SourceFile::new(SourceId::new(0), path.to_string_lossy(), text);
    let script = match parse(&source_file) {
        ParseOutcome::Complete(script) => script,
        ParseOutcome::Incomplete(input) => {
            let failure = incomplete_failure(&source_file, input);
            return Ok(safe_startup(Some(path), failure, defaults));
        }
        ParseOutcome::Invalid(diagnostics) => {
            let failure = parse_failure(&source_file, &diagnostics);
            return Ok(safe_startup(Some(path), failure, defaults));
        }
    };

    let mut scope = defaults.scope.clone();
    install_config_settings(&mut scope).map_err(ConfigFatalError::InvalidDefaults)?;
    let mut startup_environment = defaults.environment.clone();
    let eval_limits = EvalLimits::startup(limits.cancellation.clone(), limits.budget);
    match evaluate_in_environment(
        &script,
        &source_file,
        &mut scope,
        &mut startup_environment,
        &eval_limits,
    ) {
        Ok(Completion::Value(_)) => match extract_config_settings(&mut scope) {
            Ok((session_options, interactive_settings, prompt)) => Ok(ConfigStartup {
                scope,
                environment: startup_environment,
                session_options,
                interactive_settings,
                metadata: ConfigMetadata {
                    status: ConfigStatus::Loaded,
                    selected_path: Some(path),
                    failure: None,
                },
                prompt,
                diagnostic: None,
            }),
            Err(detail) => Ok(safe_startup(
                Some(path),
                ConfigFailure {
                    kind: ConfigFailureKind::ConfigEvaluation,
                    detail: detail.clone(),
                    capability: None,
                    span: Some(script.span()),
                    cause: ConfigFailureCause::Settings(detail),
                },
                defaults,
            )),
        },
        Ok(Completion::Cancelled(cancellation))
            if cancellation.reason() == CancelReason::Requested =>
        {
            Err(ConfigFatalError::Cancelled(cancellation))
        }
        Ok(Completion::Cancelled(cancellation)) => Ok(safe_startup(
            Some(path),
            ConfigFailure {
                kind: ConfigFailureKind::ConfigBudget,
                detail: format!(
                    "startup evaluation was cancelled: {:?}",
                    cancellation.reason()
                ),
                capability: None,
                span: Some(cancellation.span()),
                cause: ConfigFailureCause::Cancellation(cancellation),
            },
            defaults,
        )),
        Err(error) => Ok(safe_startup(
            Some(path),
            runtime_failure(&source_file, &error),
            defaults,
        )),
    }
}

struct ConfigPaths {
    primary: PathBuf,
    legacy: PathBuf,
}

fn config_paths(
    platform: ConfigPlatform,
    environment: &dyn ConfigEnvironment,
) -> Option<ConfigPaths> {
    let root = environment
        .value(OsStr::new("XDG_CONFIG_HOME"))
        .filter(|value| !value.is_empty() && Path::new(value).is_absolute())
        .map(PathBuf::from)
        .or_else(|| {
            environment
                .value(OsStr::new("HOME"))
                .filter(|value| !value.is_empty() && Path::new(value).is_absolute())
                .map(PathBuf::from)
                .map(|home| match platform {
                    ConfigPlatform::Linux | ConfigPlatform::FlashOs => home.join(".config"),
                    ConfigPlatform::MacOs => home.join("Library/Application Support"),
                })
        })?;
    Some(ConfigPaths {
        primary: root.join("flash/config.fsh"),
        legacy: root.join("flashshell/config.fsh"),
    })
}

fn file_failure(error: ConfigFileError) -> ConfigFailure {
    let kind = match error.kind {
        ConfigFileErrorKind::Trust => ConfigFailureKind::ConfigTrust,
        ConfigFileErrorKind::Read => ConfigFailureKind::ConfigRead,
        ConfigFileErrorKind::Budget => ConfigFailureKind::ConfigBudget,
    };
    ConfigFailure {
        kind,
        detail: error.detail.clone(),
        capability: None,
        span: None,
        cause: ConfigFailureCause::File(error),
    }
}

fn install_config_settings(scope: &mut ScopeStack) -> Result<(), String> {
    for (name, value) in [
        (
            PIPEFAIL_SETTING,
            Value::Bool(SessionOptions::default().pipefail()),
        ),
        (
            CAPTURE_LIMIT_SETTING,
            Value::Int(
                i64::try_from(SessionOptions::DEFAULT_CAPTURE_LIMIT)
                    .expect("the built-in capture limit fits in a Flash Int"),
            ),
        ),
        (
            COMPLETION_SETTING,
            Value::Bool(InteractiveSettings::default().completion()),
        ),
        (
            HISTORY_SETTING,
            Value::Bool(InteractiveSettings::default().history()),
        ),
        (
            PROMPT_SETTING,
            Value::string(EditorPrompt::default().primary()),
        ),
        (
            CONTINUATION_PROMPT_SETTING,
            Value::string(EditorPrompt::default().continuation()),
        ),
    ] {
        scope
            .declare(name, BindingMutability::Mutable, value)
            .map_err(|error| format!("config setting binding {name:?} is reserved: {error}"))?;
    }
    Ok(())
}

fn extract_config_settings(
    scope: &mut ScopeStack,
) -> Result<(SessionOptions, InteractiveSettings, EditorPrompt), String> {
    let pipefail = take_bool_setting(scope, PIPEFAIL_SETTING)?;
    let capture_limit = match take_setting(scope, CAPTURE_LIMIT_SETTING)? {
        Value::Int(value) => usize::try_from(value).map_err(|_| {
            format!(
                "config setting `${CAPTURE_LIMIT_SETTING}` requires a nonnegative Int representable as usize"
            )
        })?,
        value => return Err(setting_type_error(CAPTURE_LIMIT_SETTING, "Int", &value)),
    };
    let completion = take_bool_setting(scope, COMPLETION_SETTING)?;
    let history = take_bool_setting(scope, HISTORY_SETTING)?;
    let prompt = take_prompt_setting(scope, PROMPT_SETTING)?;
    let continuation_prompt = take_prompt_setting(scope, CONTINUATION_PROMPT_SETTING)?;
    Ok((
        SessionOptions::default()
            .with_pipefail(pipefail)
            .with_capture_limit(capture_limit),
        InteractiveSettings {
            completion,
            history,
        },
        EditorPrompt::new(prompt, continuation_prompt),
    ))
}

fn take_prompt_setting(scope: &mut ScopeStack, name: &str) -> Result<String, String> {
    let value = match take_setting(scope, name)? {
        Value::String(value) => value,
        value => return Err(setting_type_error(name, "String", &value)),
    };
    if value.chars().any(char::is_control) {
        return Err(format!(
            "config setting `${name}` may not contain control characters"
        ));
    }
    Ok(value.to_string())
}

fn take_bool_setting(scope: &mut ScopeStack, name: &str) -> Result<bool, String> {
    match take_setting(scope, name)? {
        Value::Bool(value) => Ok(value),
        value => Err(setting_type_error(name, "Bool", &value)),
    }
}

fn take_setting(scope: &mut ScopeStack, name: &str) -> Result<Value, String> {
    scope
        .remove(name)
        .ok_or_else(|| format!("config setting `${name}` is unavailable"))
}

fn setting_type_error(name: &str, expected: &str, actual: &Value) -> String {
    format!(
        "config setting `${name}` requires {expected}, found {}",
        actual.family_name()
    )
}

fn clean_startup(
    status: ConfigStatus,
    selected_path: Option<PathBuf>,
    defaults: &ConfigDefaults,
) -> ConfigStartup {
    ConfigStartup {
        scope: defaults.scope.clone(),
        environment: defaults.environment.clone(),
        session_options: SessionOptions::default(),
        interactive_settings: InteractiveSettings::default(),
        metadata: ConfigMetadata {
            status,
            selected_path,
            failure: None,
        },
        prompt: EditorPrompt::default(),
        diagnostic: None,
    }
}

fn safe_startup(
    selected_path: Option<PathBuf>,
    failure: ConfigFailure,
    defaults: &ConfigDefaults,
) -> ConfigStartup {
    let location = selected_path.as_deref().map_or_else(
        || "config discovery".to_owned(),
        |path| path.display().to_string(),
    );
    let diagnostic = format!(
        "fsh: {}: {location}: {}\n",
        failure.kind.name(),
        failure.detail
    );
    ConfigStartup {
        scope: defaults.scope.clone(),
        environment: defaults.environment.clone(),
        session_options: SessionOptions::default(),
        interactive_settings: InteractiveSettings::default(),
        metadata: ConfigMetadata {
            status: ConfigStatus::SafeMode,
            selected_path,
            failure: Some(failure),
        },
        prompt: EditorPrompt::safe_mode(),
        diagnostic: Some(diagnostic),
    }
}

fn incomplete_failure(source: &SourceFile, input: IncompleteInput) -> ConfigFailure {
    let diagnostic = Diagnostic::new(
        Severity::Error,
        "CFG001",
        format!("incomplete config input: {}", input.reason()),
    )
    .with_primary(
        input.span(),
        "config ends before this construct is complete",
    );
    let detail = render_diagnostic(source, &diagnostic)
        .expect("config diagnostic spans address their source");
    ConfigFailure {
        kind: ConfigFailureKind::ConfigParse,
        detail,
        capability: None,
        span: Some(input.span()),
        cause: ConfigFailureCause::Parse(vec![diagnostic]),
    }
}

fn parse_failure(source: &SourceFile, diagnostics: &[Diagnostic]) -> ConfigFailure {
    let span = diagnostics.iter().find_map(|diagnostic| {
        diagnostic
            .labels()
            .iter()
            .find(|label| label.style() == LabelStyle::Primary)
            .map(|label| label.span())
    });
    let detail = diagnostics
        .iter()
        .map(|diagnostic| {
            render_diagnostic(source, diagnostic)
                .expect("parser diagnostics always address their source")
        })
        .collect::<String>();
    ConfigFailure {
        kind: ConfigFailureKind::ConfigParse,
        detail,
        capability: None,
        span,
        cause: ConfigFailureCause::Parse(diagnostics.to_vec()),
    }
}

fn runtime_failure(source: &SourceFile, error: &RuntimeError) -> ConfigFailure {
    let (kind, capability) = match error.kind() {
        RuntimeErrorKind::RestrictedStartup { capability } => (
            ConfigFailureKind::RestrictedStartup,
            Some(match capability {
                RestrictedCapability::ProcessExecution => "process execution",
                RestrictedCapability::CommandSubstitution => "command substitution",
                RestrictedCapability::FilesystemRead => "filesystem reads",
                RestrictedCapability::ModuleLoad => "module loading",
            }),
        ),
        RuntimeErrorKind::ResourceBudgetExceeded => (ConfigFailureKind::ConfigBudget, None),
        _ => (ConfigFailureKind::ConfigEvaluation, None),
    };
    let diagnostic = Diagnostic::new(Severity::Error, "CFG002", error.to_string())
        .with_primary(error.span(), "config evaluation failed");
    ConfigFailure {
        kind,
        detail: render_diagnostic(source, &diagnostic)
            .expect("runtime error spans address their source"),
        capability,
        span: Some(error.span()),
        cause: ConfigFailureCause::Runtime(error.clone()),
    }
}
