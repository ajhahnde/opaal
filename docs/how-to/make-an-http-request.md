# Make an HTTP request

Start with the four-file [first project](../learn/first-project.md) and a trusted service that answers `GET /` on loopback port 8787. Add this endpoint to `opaal.toml`:

```toml
[endpoints.local]
url = "http://127.0.0.1:8787/"
methods = ["GET"]
secret_headers = []
tls = false
```

Replace `tasks.opaal` with a task that names the endpoint identity and its effect:

```opaal
import project::endpoints as endpoints
import std::http as http

action fetch() -> http::HttpResponse
effects {
    network.http(endpoints::local);
}
{
    return http::request(endpoints::local, "GET", {}, null, null, 1048576)
}
task fetch = fetch
```

Replace `rules = []` in `authority.toml` with this row, keeping the schema, project, and environment fields:

```toml
[[rules]]
decision = "grant"
effect = "network.http"
scope = "endpoint.local"
required_enforcement = "enforced"
```

Check, plan, and inspect `fetch` before sending a request:

```sh
opaal check --project opaal.toml --task fetch --environment local
opaal plan --project opaal.toml --task fetch --environment local --expires-in 900s --out fetch.plan.json
opaal plan inspect fetch.plan.json
opaal execute --plan fetch.plan.json --accept sha256:EXACT_PLAN_DIGEST --journal fetch.run.jsonl
opaal audit --project opaal.toml --journal fetch.run.jsonl --out fetch.audit.json
```

Run `execute` only when the local service is ready, replacing the placeholder with the complete reviewed digest. A successful run writes a journal with a `network.http` boundary; audit validates the record. Connection refusal or a response above the declared limit leaves a failed run, not a successful fetch.

The controlled `std::http::request` operation accepts the endpoint identity, method, ordinary headers, optional secret-header handle, optional byte body, and response-byte limit. It returns status, headers, and body. Redirects are not followed, requests are not retried, and 3xx or non-2xx responses remain ordinary response data.

TLS uses only the endpoint's CA bytes and declared server name. The URL, server name, and CA are bound into the plan and rechecked before execution. A request can fail if those inputs change after review. The adapter limits request count, headers, body, and duration; see [HTTP reference](../reference/std/http.md).

If the request needs a secret header, follow [use secrets](use-secrets.md). Do not put a secret into an ordinary header value.
