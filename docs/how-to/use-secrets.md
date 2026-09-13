# Pass an injected secret

Start with the loopback task in [make an HTTP request](make-an-http-request.md). Change that endpoint's `secret_headers` to `["authorization"]` and add a secret identity to `opaal.toml`:

```toml
[secrets.token]
kind = "injected"
```

Replace `tasks.opaal` with the one-sink form:

```opaal
import project::endpoints as endpoints
import project::secrets as secrets
import std::http as http

action fetch_private() -> http::HttpResponse
effects {
    network.http(endpoints::local);
    secret.reveal(secrets::token, endpoints::local);
}
{
    let header = http::secret_header("authorization", secrets::token)
    return http::request(endpoints::local, "GET", {}, header, null, 1048576)
}
task fetch_private = fetch_private
```

Keep the existing `network.http` grant and add this authority row:

```toml
[[rules]]
decision = "grant"
effect = "secret.reveal"
scope = "secret.token@endpoint.local"
required_enforcement = "enforced"
```

Run `opaal check --project opaal.toml --task fetch_private --environment local`, then plan that same task to `fetch.plan.json` and inspect it. Once the local service is ready, pass the complete reviewed digest to `opaal execute --plan fetch.plan.json --accept sha256:EXACT_PLAN_DIGEST --secret-stdin token --journal fetch.run.jsonl`, with the secret bytes on non-terminal standard input. A successful request uses the header once; the journal records the sink identity and result without the secret bytes. For a remote endpoint, use the manifest's HTTPS, server-name, and project-local CA fields.

A project plan contains secret identities and sink scopes, never the bytes. One eligible plan may require exactly one secret ID and one sink. Multiple IDs, repeated reveal, or multiple sinks produce a valid but refused check and plan. Review the plan before providing input.

For the one-secret case, supply `--secret-stdin ID` on `execute` and send bytes through non-terminal standard input. A secret-free plan must omit the option and reads no stdin. OPAAL first revalidates static identities and maintained tool probes; only then does it read the secret. Denial or stale preflight leaves it unconsumed. A consumed identity cannot be reinjected.

The one-use handle cannot be cloned, displayed, serialized, or converted to an ordinary header. The adapter clears request buffers, overwrites the owned secret, and retains redaction patterns through later diagnostics and evidence. Journal payloads record only identity and sink scope. [HTTP](../reference/std/http.md), [embedding context](../reference/embedding/operational-context.md), and [limits](../reference/limits.md) give the bounds.
