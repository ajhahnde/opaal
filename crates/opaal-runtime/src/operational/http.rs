//! Normalized bounded HTTP with a single opaque secret-header sink.

use std::fmt;
use std::time::Duration;

use opaal_platform::Platform;
use opaal_platform::operational::{
    HttpHeader, HttpRequest, HttpResponse, MAX_HTTP_HEADER_BYTES, MAX_HTTP_HEADERS,
    MaterializedSecretHeader, OperationalAdapter,
};

use crate::authority::{CapabilityRequest, EffectSet};
use crate::context::OperationalContext;
use crate::project::EndpointDeclaration;
use crate::security::{REDACTED, SecretId};

use super::url::{Scheme, Url};
use super::{MAX_HTTP_BODY_BYTES, ModuleError, adapter_error, authorize};

pub const MAX_HTTP_REQUESTS: usize = 8;
pub const MAX_HTTP_DURATION: Duration = opaal_platform::operational::MAX_HTTP_DURATION;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HttpBudget {
    requests: usize,
}

impl HttpBudget {
    #[must_use]
    pub const fn requests(&self) -> usize {
        self.requests
    }
    fn admit(&mut self) -> Result<(), ModuleError> {
        if self.requests == MAX_HTTP_REQUESTS {
            return Err(ModuleError::invalid(
                "HTTP001",
                "HTTP requests exceed eight",
            ));
        }
        self.requests += 1;
        Ok(())
    }
}

/// A non-cloneable, non-serializable reference to the only secret-aware sink.
pub struct SecretHeader {
    name: String,
    secret: SecretId,
    endpoint: String,
}

impl fmt::Debug for SecretHeader {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SecretHeader")
            .field("name", &self.name)
            .field("secret", &self.secret)
            .field("endpoint", &self.endpoint)
            .field("value", &REDACTED)
            .finish()
    }
}

pub fn secret_header(
    endpoint: &EndpointDeclaration,
    name: &str,
    secret: SecretId,
) -> Result<SecretHeader, ModuleError> {
    if !endpoint
        .secret_headers()
        .iter()
        .any(|allowed| allowed == name)
    {
        return Err(ModuleError::invalid(
            "HTTP002",
            "header is not an allowed secret sink for this endpoint",
        ));
    }
    CapabilityRequest::secret_reveal(secret.as_str(), endpoint.id(), name)
        .map_err(|error| ModuleError::invalid("HTTP003", error.to_string()))?;
    Ok(SecretHeader {
        name: name.to_owned(),
        secret,
        endpoint: endpoint.id().to_owned(),
    })
}

#[allow(clippy::too_many_arguments)]
pub fn request(
    context: &mut OperationalContext,
    effects: &EffectSet,
    platform: &dyn Platform,
    adapter: &dyn OperationalAdapter,
    endpoint: &EndpointDeclaration,
    method: &str,
    headers: &[HttpHeader],
    secret: Option<SecretHeader>,
    body: &[u8],
    max_response_bytes: usize,
    ca_pem: Option<&[u8]>,
    timeout: Duration,
    budget: &mut HttpBudget,
) -> Result<HttpResponse, ModuleError> {
    if body.len() > MAX_HTTP_BODY_BYTES
        || max_response_bytes > MAX_HTTP_BODY_BYTES
        || timeout > MAX_HTTP_DURATION
    {
        return Err(ModuleError::invalid(
            "HTTP004",
            "HTTP body or deadline exceeds its fixed ceiling",
        ));
    }
    let total_headers = headers
        .len()
        .checked_add(usize::from(secret.is_some()))
        .ok_or_else(|| ModuleError::invalid("HTTP005", "HTTP header count overflow"))?;
    if total_headers > MAX_HTTP_HEADERS {
        return Err(ModuleError::invalid(
            "HTTP005",
            "HTTP header count exceeds 128",
        ));
    }
    let header_bytes = headers
        .iter()
        .try_fold(0usize, |sum, header| {
            sum.checked_add(header.name().len() + header.value().len() + 4)
        })
        .ok_or_else(|| ModuleError::invalid("HTTP006", "HTTP header size overflow"))?;
    let header_bytes = secret.as_ref().map_or(Ok(header_bytes), |secret| {
        header_bytes
            .checked_add(secret.name.len() + 4)
            .ok_or_else(|| ModuleError::invalid("HTTP006", "HTTP header size overflow"))
    })?;
    if header_bytes > MAX_HTTP_HEADER_BYTES {
        return Err(ModuleError::invalid(
            "HTTP006",
            "HTTP headers exceed 64 KiB",
        ));
    }
    if !endpoint.methods().iter().any(|allowed| allowed == method) {
        return Err(ModuleError::invalid(
            "HTTP007",
            "HTTP method is not allowed by the endpoint",
        ));
    }
    if headers.iter().any(|header| {
        endpoint
            .secret_headers()
            .iter()
            .any(|secret_name| secret_name == header.name())
    }) {
        return Err(ModuleError::invalid(
            "HTTP008",
            "a secret header name cannot use the ordinary header route",
        ));
    }
    let url = Url::parse(endpoint.url())
        .map_err(|error| ModuleError::invalid("HTTP009", error.to_string()))?;
    match (
        url.scheme(),
        endpoint.tls(),
        endpoint.tls_server_name(),
        ca_pem,
    ) {
        (Scheme::Https, true, Some(_), Some(ca))
            if !ca.is_empty() && ca.len() <= MAX_HTTP_BODY_BYTES => {}
        (Scheme::Http, false, None, None) if url.is_literal_loopback() => {}
        _ => {
            return Err(ModuleError::invalid(
                "HTTP010",
                "endpoint TLS/CA policy is incomplete or unsafe",
            ));
        }
    }
    if let Some(secret) = secret.as_ref()
        && secret.endpoint != endpoint.id()
    {
        return Err(ModuleError::invalid(
            "HTTP011",
            "secret header belongs to another endpoint",
        ));
    }
    let network = CapabilityRequest::network_http(endpoint.id(), method)
        .map_err(|error| ModuleError::invalid("HTTP012", error.to_string()))?;
    authorize(context, effects, &network, platform)?;
    if let Some(secret) = secret.as_ref() {
        let reveal =
            CapabilityRequest::secret_reveal(secret.secret.as_str(), endpoint.id(), &secret.name)
                .map_err(|error| ModuleError::invalid("HTTP013", error.to_string()))?;
        authorize(context, effects, &reveal, platform)?;
        if !context.contains_secret(&secret.secret) {
            return Err(ModuleError::invalid(
                "HTTP014",
                "secret is missing or has already been consumed",
            ));
        }
    }
    if let Some(reason) = context.poll_cancellation() {
        return Err(ModuleError::Cancelled(reason));
    }
    budget.admit()?;
    match secret {
        Some(secret) => {
            let result =
                context.consume_secret_with_cancellation(&secret.secret, |bytes, cancellation| {
                    let response = adapter.http_request(
                        HttpRequest {
                            connect_host: url.host(),
                            port: url.port(),
                            tls_server_name: endpoint.tls_server_name(),
                            ca_pem,
                            method,
                            path_and_query: url.path_and_query(),
                            headers,
                            secret_header: Some(MaterializedSecretHeader {
                                name: &secret.name,
                                value: bytes,
                            }),
                            body,
                            max_response_bytes,
                            timeout,
                        },
                        &|| cancellation.poll().is_some(),
                    )?;
                    if cancellation.poll().is_some() {
                        return Err(opaal_platform::operational::OperationalError::new(
                            opaal_platform::operational::OperationalErrorKind::Cancelled,
                            "HTTP request was cancelled",
                        ));
                    }
                    Ok(response)
                });
            let response = result
                .map_err(|error| adapter_error(context, error))?
                .ok_or_else(|| {
                    ModuleError::invalid(
                        "HTTP014",
                        "secret is missing or has already been consumed",
                    )
                })?;
            redact_response(context, response, max_response_bytes)
                .map_err(|error| adapter_error(context, error))
        }
        None => {
            let response = adapter
                .http_request(
                    HttpRequest {
                        connect_host: url.host(),
                        port: url.port(),
                        tls_server_name: endpoint.tls_server_name(),
                        ca_pem,
                        method,
                        path_and_query: url.path_and_query(),
                        headers,
                        secret_header: None,
                        body,
                        max_response_bytes,
                        timeout,
                    },
                    &|| context.poll_cancellation().is_some(),
                )
                .map_err(|error| adapter_error(context, error))?;
            if let Some(reason) = context.poll_cancellation() {
                return Err(ModuleError::Cancelled(reason));
            }
            redact_response(context, response, max_response_bytes)
                .map_err(|error| adapter_error(context, error))
        }
    }
}

fn redact_response(
    context: &OperationalContext,
    response: HttpResponse,
    max_response_bytes: usize,
) -> Result<HttpResponse, opaal_platform::operational::OperationalError> {
    let body = context.redact_bytes(response.body());
    if body.len() > max_response_bytes {
        return Err(opaal_platform::operational::OperationalError::new(
            opaal_platform::operational::OperationalErrorKind::LimitExceeded,
            "redacted HTTP response exceeds its byte limit",
        ));
    }
    let mut header_bytes = 0usize;
    let mut headers = Vec::with_capacity(response.headers().len());
    for header in response.headers() {
        if context.redact_bytes(header.name().as_bytes()) != header.name().as_bytes() {
            return Err(opaal_platform::operational::OperationalError::new(
                opaal_platform::operational::OperationalErrorKind::Protocol,
                "HTTP response header name contains secret data",
            ));
        }
        let value = context.redact_bytes(header.value());
        header_bytes = header_bytes
            .checked_add(header.name().len() + value.len() + 4)
            .ok_or_else(|| {
                opaal_platform::operational::OperationalError::new(
                    opaal_platform::operational::OperationalErrorKind::LimitExceeded,
                    "redacted HTTP headers exceed their byte limit",
                )
            })?;
        headers.push(HttpHeader::new(header.name(), value)?);
    }
    if header_bytes > MAX_HTTP_HEADER_BYTES {
        return Err(opaal_platform::operational::OperationalError::new(
            opaal_platform::operational::OperationalErrorKind::LimitExceeded,
            "redacted HTTP headers exceed their byte limit",
        ));
    }
    Ok(HttpResponse::new(response.status(), headers, body))
}
