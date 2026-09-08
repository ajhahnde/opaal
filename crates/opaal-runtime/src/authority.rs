//! Explicit, fail-closed authority contracts for runtime embedders.
//!
//! Source declarations may eventually request these effects, but never grant
//! them. The current pure-source evaluator does not construct this context or
//! cross an operational adapter boundary.

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};

use opaal_platform::{
    AuthorityEffect, AuthorityEnforcement, AuthorityQuery, AuthorityScope, Platform,
};

/// Maximum exact grant/deny rows in one evaluation authority context.
pub const MAX_AUTHORITY_RULES: usize = 256;

/// One stable embedding-provided evaluation identity.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct EvaluationContextId(u64);

impl EvaluationContextId {
    /// Build a nonzero evaluation identity.
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        if value == 0 { None } else { Some(Self(value)) }
    }

    /// The nonzero numeric identity.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// One exact typed authority scope.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum CapabilityScope {
    /// One native project path.
    ProjectPath(PathBuf),
    /// One maintained tool identity.
    Tool(String),
    /// One endpoint identity and canonical method.
    Endpoint {
        /// Project endpoint identity.
        endpoint: String,
        /// Canonical HTTP method.
        method: String,
    },
    /// One secret-to-endpoint header sink.
    SecretSink {
        /// Project secret identity.
        secret: String,
        /// Project endpoint identity.
        endpoint: String,
        /// Canonical header name.
        header: String,
    },
    /// The current evaluation's clock.
    Evaluation,
}

/// An invalid public capability request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapabilityRequestError {
    /// A required path was empty.
    EmptyPath,
    /// A required semantic identity was empty.
    EmptyIdentity,
    /// An HTTP method was not canonical uppercase ASCII.
    InvalidMethod,
    /// A header name was not canonical lowercase ASCII.
    InvalidHeader,
}

impl fmt::Display for CapabilityRequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::EmptyPath => "an authority path scope cannot be empty",
            Self::EmptyIdentity => "an authority identity cannot be empty",
            Self::InvalidMethod => "an authority HTTP method must be uppercase ASCII",
            Self::InvalidHeader => "an authority header name must be lowercase ASCII",
        })
    }
}

impl std::error::Error for CapabilityRequestError {}

/// One exact operational authority request.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CapabilityRequest {
    effect: AuthorityEffect,
    scope: CapabilityScope,
}

impl CapabilityRequest {
    /// Request bounded reading from one exact project path.
    pub fn filesystem_read(path: impl Into<PathBuf>) -> Result<Self, CapabilityRequestError> {
        Self::project_path(AuthorityEffect::FilesystemRead, path.into())
    }

    /// Request bounded writing to one exact project path.
    pub fn filesystem_write(path: impl Into<PathBuf>) -> Result<Self, CapabilityRequestError> {
        Self::project_path(AuthorityEffect::FilesystemWrite, path.into())
    }

    /// Request execution of one exact maintained tool.
    pub fn process_run(tool: impl Into<String>) -> Result<Self, CapabilityRequestError> {
        let tool = nonempty(tool.into())?;
        Ok(Self {
            effect: AuthorityEffect::ProcessRun,
            scope: CapabilityScope::Tool(tool),
        })
    }

    /// Request one exact endpoint and canonical HTTP method.
    pub fn network_http(
        endpoint: impl Into<String>,
        method: impl Into<String>,
    ) -> Result<Self, CapabilityRequestError> {
        let endpoint = nonempty(endpoint.into())?;
        let method = method.into();
        if method.is_empty()
            || !method
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte == b'-')
        {
            return Err(CapabilityRequestError::InvalidMethod);
        }
        Ok(Self {
            effect: AuthorityEffect::NetworkHttp,
            scope: CapabilityScope::Endpoint { endpoint, method },
        })
    }

    /// Request one exact secret-to-endpoint header sink.
    pub fn secret_reveal(
        secret: impl Into<String>,
        endpoint: impl Into<String>,
        header: impl Into<String>,
    ) -> Result<Self, CapabilityRequestError> {
        let secret = nonempty(secret.into())?;
        let endpoint = nonempty(endpoint.into())?;
        let header = header.into();
        if header.is_empty()
            || !header
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte == b'-')
        {
            return Err(CapabilityRequestError::InvalidHeader);
        }
        Ok(Self {
            effect: AuthorityEffect::SecretReveal,
            scope: CapabilityScope::SecretSink {
                secret,
                endpoint,
                header,
            },
        })
    }

    /// Request wall-clock observation for this evaluation.
    #[must_use]
    pub const fn clock_wall() -> Self {
        Self {
            effect: AuthorityEffect::ClockWall,
            scope: CapabilityScope::Evaluation,
        }
    }

    /// Request monotonic-clock observation for this evaluation.
    #[must_use]
    pub const fn clock_monotonic() -> Self {
        Self {
            effect: AuthorityEffect::ClockMonotonic,
            scope: CapabilityScope::Evaluation,
        }
    }

    /// The requested effect.
    #[must_use]
    pub const fn effect(&self) -> AuthorityEffect {
        self.effect
    }

    /// The exact requested scope.
    #[must_use]
    pub const fn scope(&self) -> &CapabilityScope {
        &self.scope
    }

    pub(crate) fn platform_query(&self) -> AuthorityQuery<'_> {
        let scope = match &self.scope {
            CapabilityScope::ProjectPath(path) => AuthorityScope::ProjectPath(path),
            CapabilityScope::Tool(tool) => AuthorityScope::Tool(tool),
            CapabilityScope::Endpoint { endpoint, method } => {
                AuthorityScope::Endpoint { endpoint, method }
            }
            CapabilityScope::SecretSink {
                secret,
                endpoint,
                header,
            } => AuthorityScope::SecretSink {
                secret,
                endpoint,
                header,
            },
            CapabilityScope::Evaluation => AuthorityScope::Evaluation,
        };
        AuthorityQuery::new(self.effect, scope)
    }

    fn project_path(
        effect: AuthorityEffect,
        path: PathBuf,
    ) -> Result<Self, CapabilityRequestError> {
        if path == Path::new("") {
            return Err(CapabilityRequestError::EmptyPath);
        }
        Ok(Self {
            effect,
            scope: CapabilityScope::ProjectPath(path),
        })
    }
}

fn nonempty(value: String) -> Result<String, CapabilityRequestError> {
    if value.is_empty() {
        Err(CapabilityRequestError::EmptyIdentity)
    } else {
        Ok(value)
    }
}

/// A canonical set of effect requests. An empty set grants nothing.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EffectSet {
    requests: BTreeSet<CapabilityRequest>,
}

impl EffectSet {
    /// Build a deduplicated canonical set.
    #[must_use]
    pub fn new(requests: impl IntoIterator<Item = CapabilityRequest>) -> Self {
        Self {
            requests: requests.into_iter().collect(),
        }
    }

    /// Whether the set contains no request.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.requests.is_empty()
    }

    /// Whether this set contains `request` exactly.
    #[must_use]
    pub fn contains(&self, request: &CapabilityRequest) -> bool {
        self.requests.contains(request)
    }

    /// Requests in canonical effect/scope order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &CapabilityRequest> {
        self.requests.iter()
    }
}

/// The minimum enforcement an explicit grant accepts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequiredEnforcement {
    /// The complete boundary must be enforced.
    Enforced,
    /// Parent enforcement is required and opaque child behavior is explicitly
    /// acknowledged.
    AcknowledgeUnenforced,
}

/// One exact deny or grant row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorityRule {
    request: CapabilityRequest,
    required: Option<RequiredEnforcement>,
}

impl AuthorityRule {
    /// Deny one exact request.
    #[must_use]
    pub fn deny(request: CapabilityRequest) -> Self {
        Self {
            request,
            required: None,
        }
    }

    /// Grant one exact request subject to `required` adapter enforcement.
    #[must_use]
    pub fn grant(request: CapabilityRequest, required: RequiredEnforcement) -> Self {
        Self {
            request,
            required: Some(required),
        }
    }

    /// The request this row matches exactly.
    #[must_use]
    pub const fn request(&self) -> &CapabilityRequest {
        &self.request
    }

    /// `None` for a deny, otherwise the grant's minimum enforcement.
    #[must_use]
    pub const fn required_enforcement(&self) -> Option<RequiredEnforcement> {
        self.required
    }
}

/// The five public verdicts produced by authority and adapter agreement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthorityVerdict {
    /// No matching explicit grant, or an explicit deny.
    Denied,
    /// The request or boundary cannot be proven.
    Unknown,
    /// The selected platform does not provide the required contract.
    Unsupported,
    /// Permission exists and the complete scope is enforced.
    GrantedEnforced,
    /// Permission exists and the exact unenforced boundary was acknowledged.
    GrantedUnenforced,
}

impl AuthorityVerdict {
    /// Whether this verdict permits an adapter call.
    #[must_use]
    pub const fn is_executable(self) -> bool {
        matches!(self, Self::GrantedEnforced | Self::GrantedUnenforced)
    }
}

/// An invalid authority context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthorityContextError {
    /// One evaluation cannot carry more than the fixed rule ceiling.
    TooManyRules {
        /// Number of supplied rows.
        count: usize,
        /// Fixed supported maximum.
        max: usize,
    },
    /// More than one row names the same normalized request.
    DuplicateRule(CapabilityRequest),
    /// Only an opaque maintained-process boundary may be acknowledged as
    /// unenforced.
    InvalidUnenforcedAcknowledgement(CapabilityRequest),
}

impl fmt::Display for AuthorityContextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyRules { count, max } => {
                write!(
                    formatter,
                    "authority context has {count} rules; maximum is {max}"
                )
            }
            Self::DuplicateRule(request) => {
                write!(formatter, "duplicate authority rule for {request:?}")
            }
            Self::InvalidUnenforcedAcknowledgement(request) => write!(
                formatter,
                "unenforced acknowledgement is invalid for {request:?}"
            ),
        }
    }
}

impl std::error::Error for AuthorityContextError {}

/// One immutable per-evaluation authority context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorityContext {
    evaluation: EvaluationContextId,
    rules: Vec<AuthorityRule>,
}

impl AuthorityContext {
    /// Validate and construct one explicit context.
    pub fn new(
        evaluation: EvaluationContextId,
        rules: impl IntoIterator<Item = AuthorityRule>,
    ) -> Result<Self, AuthorityContextError> {
        let mut rules = rules.into_iter().collect::<Vec<_>>();
        if rules.len() > MAX_AUTHORITY_RULES {
            return Err(AuthorityContextError::TooManyRules {
                count: rules.len(),
                max: MAX_AUTHORITY_RULES,
            });
        }
        for rule in &rules {
            if rule.required == Some(RequiredEnforcement::AcknowledgeUnenforced)
                && rule.request.effect() != AuthorityEffect::ProcessRun
            {
                return Err(AuthorityContextError::InvalidUnenforcedAcknowledgement(
                    rule.request.clone(),
                ));
            }
        }
        rules.sort_by(|left, right| left.request.cmp(&right.request));
        for pair in rules.windows(2) {
            if pair[0].request == pair[1].request {
                return Err(AuthorityContextError::DuplicateRule(
                    pair[0].request.clone(),
                ));
            }
        }
        Ok(Self { evaluation, rules })
    }

    /// A deny-by-default context with no authority rows.
    #[must_use]
    pub fn empty(evaluation: EvaluationContextId) -> Self {
        Self {
            evaluation,
            rules: Vec::new(),
        }
    }

    /// The evaluation this authority belongs to.
    #[must_use]
    pub const fn evaluation(&self) -> EvaluationContextId {
        self.evaluation
    }

    /// Exact rows in canonical request order.
    #[must_use]
    pub fn rules(&self) -> &[AuthorityRule] {
        &self.rules
    }

    /// Resolve one request by exact rule matching and adapter-owned
    /// enforcement reporting.
    #[must_use]
    pub fn verdict(
        &self,
        request: &CapabilityRequest,
        platform: &dyn Platform,
    ) -> AuthorityVerdict {
        let Ok(index) = self
            .rules
            .binary_search_by(|rule| rule.request.cmp(request))
        else {
            return AuthorityVerdict::Denied;
        };
        let Some(required) = self.rules[index].required else {
            return AuthorityVerdict::Denied;
        };

        match (
            platform.authority_enforcement(request.platform_query()),
            required,
        ) {
            (AuthorityEnforcement::Enforced, _) => AuthorityVerdict::GrantedEnforced,
            (AuthorityEnforcement::Unenforced, RequiredEnforcement::AcknowledgeUnenforced) => {
                AuthorityVerdict::GrantedUnenforced
            }
            (AuthorityEnforcement::Unenforced, RequiredEnforcement::Enforced)
            | (AuthorityEnforcement::Unsupported, _) => AuthorityVerdict::Unsupported,
            (AuthorityEnforcement::Unknown, _) => AuthorityVerdict::Unknown,
        }
    }
}
