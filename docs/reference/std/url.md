# `std::url`

`std::url` exports the nominal `Url` type, `parse(input: String) -> Url`, and `render(input: Url) -> String`. Parsing normalizes HTTP and HTTPS URLs and rejects credentials and fragments.

A project endpoint declares a fixed URL and method set. Plain HTTP endpoints must use a literal IPv4 or IPv6 loopback address. A TLS endpoint requires a separate lowercase DNS server name and project-local CA material. Its declared URL, TLS name, and CA bytes are bound into a plan and revalidated before execution.

A parsed URL does not grant network access. `std::http::request` requires the exact endpoint identity, declared method, authority grant, and enforcing adapter. See [HTTP](http.md) and [endpoint bindings](../operational/inputs-tools-endpoints-and-secrets.md).
