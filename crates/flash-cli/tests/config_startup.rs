#![forbid(unsafe_code)]
#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use flash_cli::config::{
    ConfigDefaults, ConfigEnvironment, ConfigFailureCause, ConfigFailureKind, ConfigFatalError,
    ConfigFile, ConfigFileError, ConfigInvocation, ConfigLimits, ConfigPlatform, ConfigRequest,
    ConfigSource, ConfigStatus, initialize_config,
};
use flash_cli::editor::EditorPrompt;
use flash_runtime::eval::{CancellationToken, FakeClock, Instant, ResourceBudget};
use flash_runtime::{BindingMutability, Environment, ScopeStack, Value};

struct Home;

impl ConfigEnvironment for Home {
    fn value(&self, name: &OsStr) -> Option<OsString> {
        (name == "HOME").then(|| OsString::from("/users/test"))
    }
}

#[derive(Clone)]
struct Source(Result<ConfigFile, ConfigFileError>);

impl Source {
    fn text(source: &str) -> Self {
        Self(Ok(ConfigFile::Source(source.to_owned())))
    }
}

impl ConfigSource for Source {
    fn load(&self, _path: &Path, _source_limit: usize) -> Result<ConfigFile, ConfigFileError> {
        self.0.clone()
    }
}

fn defaults() -> ConfigDefaults {
    let mut scope = ScopeStack::new();
    scope
        .declare("base", BindingMutability::Immutable, Value::Int(1))
        .expect("base binding should be unique");
    let mut environment = Environment::new();
    environment.set("MODE", "base");
    ConfigDefaults::new(scope, environment)
}

fn request() -> ConfigRequest {
    ConfigRequest::new(ConfigInvocation::Interactive, false, ConfigPlatform::Linux)
}

#[test]
fn successful_config_commits_bindings_functions_and_exports_atomically() {
    let source = Source::text(
        "let configured = 7\nexport MODE = 'configured'\ndef dormant() {\n    ^echo no\n}\n",
    );
    let startup = initialize_config(
        request(),
        &Home,
        &source,
        &defaults(),
        &ConfigLimits::test_default(),
    )
    .expect("dormant prohibited work must not reject a definition");

    assert_eq!(startup.metadata().status(), ConfigStatus::Loaded);
    assert!(matches!(
        startup.scope().get("configured"),
        Some(Value::Int(7))
    ));
    assert!(matches!(
        startup.scope().get("dormant"),
        Some(Value::Callable(_))
    ));
    assert_eq!(
        startup.environment().get("MODE"),
        Some(OsStr::new("configured"))
    );
    assert!(startup.diagnostic().is_none());
}

#[test]
fn successful_config_commits_typed_session_and_editor_settings_without_leaking_bindings() {
    let source = Source::text(
        "$pipefail = true\n$capture_limit = 0\n$completion = false\n$history = false\n$prompt = 'flash> '\n$continuation_prompt = 'more> '\n",
    );
    let startup = initialize_config(
        request(),
        &Home,
        &source,
        &defaults(),
        &ConfigLimits::test_default(),
    )
    .expect("valid settings should commit");

    assert_eq!(startup.metadata().status(), ConfigStatus::Loaded);
    assert!(startup.session_options().pipefail());
    assert_eq!(startup.session_options().capture_limit(), 0);
    assert!(!startup.interactive_settings().completion());
    assert!(!startup.interactive_settings().history());
    assert_eq!(startup.prompt().primary(), "flash> ");
    assert_eq!(startup.prompt().continuation(), "more> ");
    for name in [
        "pipefail",
        "capture_limit",
        "completion",
        "history",
        "prompt",
        "continuation_prompt",
    ] {
        assert!(
            startup.scope().get(name).is_none(),
            "config-only binding {name:?} must not enter the live scope"
        );
    }
}

#[test]
fn a_reserved_setting_in_host_defaults_is_a_typed_fatal_error() {
    let mut scope = ScopeStack::new();
    scope
        .declare("pipefail", BindingMutability::Immutable, Value::Bool(false))
        .unwrap();
    let defaults = ConfigDefaults::new(scope, Environment::new());

    let result = initialize_config(
        request(),
        &Home,
        &Source::text("$pipefail = true\n"),
        &defaults,
        &ConfigLimits::test_default(),
    );

    assert!(matches!(result, Err(ConfigFatalError::InvalidDefaults(_))));
}

#[test]
fn invalid_settings_roll_back_scope_environment_options_and_editor_state() {
    for source in [
        "$pipefail = 1\n",
        "$capture_limit = -1\n",
        "$completion = 'yes'\n",
        "$history = null\n",
        "$prompt = false\n",
        "$continuation_prompt = 1\n",
    ] {
        let startup = initialize_config(
            request(),
            &Home,
            &Source::text(&format!(
                "let partial = 2\nexport MODE = 'partial'\n{source}"
            )),
            &defaults(),
            &ConfigLimits::test_default(),
        )
        .expect("a config setting failure should enter safe mode");

        assert_eq!(startup.metadata().status(), ConfigStatus::SafeMode);
        assert_eq!(
            startup.metadata().failure().map(|failure| failure.kind()),
            Some(ConfigFailureKind::ConfigEvaluation)
        );
        assert!(startup.metadata().failure().unwrap().span().is_some());
        assert!(startup.scope().get("partial").is_none());
        assert_eq!(startup.environment().get("MODE"), Some(OsStr::new("base")));
        assert_eq!(
            startup.session_options(),
            flash_runtime::plan::SessionOptions::default()
        );
        assert!(startup.interactive_settings().completion());
        assert!(startup.interactive_settings().history());
        assert_eq!(startup.prompt(), &EditorPrompt::safe_mode());
        for name in [
            "pipefail",
            "capture_limit",
            "completion",
            "history",
            "prompt",
            "continuation_prompt",
        ] {
            assert!(startup.scope().get(name).is_none());
        }
    }
}

#[test]
fn parse_and_evaluation_failures_discard_the_complete_overlay() {
    for (source, expected) in [
        ("let unfinished =", ConfigFailureKind::ConfigParse),
        (
            "let partial = 2\nexport MODE = 'partial'\nlet broken = $missing\n",
            ConfigFailureKind::ConfigEvaluation,
        ),
    ] {
        let startup = initialize_config(
            request(),
            &Home,
            &Source::text(source),
            &defaults(),
            &ConfigLimits::test_default(),
        )
        .expect("config-origin failure should enter safe mode");

        assert_eq!(startup.metadata().status(), ConfigStatus::SafeMode);
        assert_eq!(
            startup.metadata().failure().map(|failure| failure.kind()),
            Some(expected)
        );
        assert!(startup.scope().get("partial").is_none());
        assert_eq!(startup.environment().get("MODE"), Some(OsStr::new("base")));
        assert_eq!(startup.prompt().primary(), "[SAFE] >> ");
        assert!(startup.diagnostic().is_some());
    }
}

#[test]
fn reached_commands_and_substitutions_name_the_restricted_capability() {
    for (source, capability) in [
        ("^echo no\n", "process execution"),
        ("let value = $(^echo no)\n", "command substitution"),
        ("import './dependency.fsh'\n", "module loading"),
        ("let value = 1\nexport { value }\n", "module loading"),
        (
            "def reached() {\n    ^echo no\n}\nlet value = reached()\n",
            "process execution",
        ),
    ] {
        let startup = initialize_config(
            request(),
            &Home,
            &Source::text(source),
            &defaults(),
            &ConfigLimits::test_default(),
        )
        .expect("restricted config operation should enter safe mode");
        let failure = startup
            .metadata()
            .failure()
            .expect("safe mode should retain its structured failure");
        assert_eq!(failure.kind(), ConfigFailureKind::RestrictedStartup);
        assert_eq!(failure.capability(), Some(capability));
        assert!(failure.span().is_some());
        assert!(matches!(failure.cause(), ConfigFailureCause::Runtime(_)));
    }
}

#[test]
fn requested_cancellation_is_fatal_while_budgets_enter_safe_mode() {
    let cancelled = initialize_config(
        request(),
        &Home,
        &Source::text("while true {\n}\n"),
        &defaults(),
        &ConfigLimits::new(
            1024,
            ResourceBudget::steps(10_000),
            CancellationToken::from_fn(|| true),
        ),
    );
    assert!(matches!(cancelled, Err(ConfigFatalError::Cancelled(_))));

    let budgeted = initialize_config(
        request(),
        &Home,
        &Source::text("while true {\n}\n"),
        &defaults(),
        &ConfigLimits::new(1024, ResourceBudget::steps(8), CancellationToken::never()),
    )
    .expect("step exhaustion should enter safe mode");
    assert_eq!(
        budgeted.metadata().failure().map(|failure| failure.kind()),
        Some(ConfigFailureKind::ConfigBudget)
    );

    let timeout = initialize_config(
        request(),
        &Home,
        &Source::text("while true {\n}\n"),
        &defaults(),
        &ConfigLimits::new(
            1024,
            ResourceBudget::steps(10_000),
            CancellationToken::deadline(FakeClock::at(10), Instant::from_nanos(10)),
        ),
    )
    .expect("deadline cancellation should enter budget safe mode");
    assert!(matches!(
        timeout
            .metadata()
            .failure()
            .expect("timeout should retain a failure")
            .cause(),
        ConfigFailureCause::Cancellation(_)
    ));

    let oversized = initialize_config(
        request(),
        &Home,
        &Source::text("12345"),
        &defaults(),
        &ConfigLimits::new(4, ResourceBudget::steps(10_000), CancellationToken::never()),
    )
    .expect("source limit should enter safe mode even for an injected source");
    assert_eq!(
        oversized.metadata().failure().map(|failure| failure.kind()),
        Some(ConfigFailureKind::ConfigBudget)
    );
}

#[test]
fn path_and_source_failures_are_structured_safe_mode_with_clean_defaults() {
    struct NoHome;
    impl ConfigEnvironment for NoHome {
        fn value(&self, _name: &OsStr) -> Option<OsString> {
            None
        }
    }

    let unavailable = initialize_config(
        request(),
        &NoHome,
        &Source(Ok(ConfigFile::Absent)),
        &defaults(),
        &ConfigLimits::test_default(),
    )
    .expect("path failure should enter safe mode");
    assert_eq!(
        unavailable
            .metadata()
            .failure()
            .map(|failure| failure.kind()),
        Some(ConfigFailureKind::ConfigPathUnavailable)
    );
    assert_eq!(unavailable.metadata().selected_path(), None);

    let path = PathBuf::from("/users/test/.config/flash/config.fsh");
    let untrusted = initialize_config(
        request(),
        &Home,
        &Source(Err(ConfigFileError::trust("wrong owner"))),
        &defaults(),
        &ConfigLimits::test_default(),
    )
    .expect("trust failure should enter safe mode");
    assert_eq!(untrusted.metadata().selected_path(), Some(path.as_path()));
    assert_eq!(
        untrusted.metadata().failure().map(|failure| failure.kind()),
        Some(ConfigFailureKind::ConfigTrust)
    );
}
