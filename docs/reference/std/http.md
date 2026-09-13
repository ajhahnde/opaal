# `std::http`

The compiled module exports opaque `SecretHeader`, `HttpResponse`, `secret_header(name: String, secret: secrets::SecretIdentity) -> SecretHeader`, and `request(endpoint: endpoints::EndpointIdentity, method: String, headers: Record, secret: SecretHeader, body: Bytes, max_response_bytes: Int) -> HttpResponse`. The secret and body slots may be `null` when absent. A response has `status`, `headers`, and `body`.

A request uses a declared endpoint and method. It never follows redirects or retries; 3xx and non-2xx statuses are returned as data. One evaluation admits at most eight requests, 128 headers, 64 KiB of header bytes, 8 MiB per body, and 30 seconds per request. TLS trusts only the endpoint's CA bytes, validates certificate time and the declared server name, and does not add platform roots or proxy settings. The POSIX adapter connects to a literal IP so DNS cannot outlive the request deadline.

An ordinary header cannot use a declared secret-header name. `SecretHeader` is a one-use opaque sink; both `network.http` and `secret.reveal` authority are checked before secret bytes are borrowed. The journal records the request and reveal boundaries without the payload. [Secrets](../operational/inputs-tools-endpoints-and-secrets.md) and [authority](../operational/authority.md) describe the exact grants.
