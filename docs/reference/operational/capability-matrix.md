# Capability matrix

Check distinguishes a declared request, an authority row, and what the selected host can enforce. A grant cannot turn an unsupported adapter into an executable one.

| Request | Maintained POSIX boundary | Required authority |
| --- | --- | --- |
| Project file read or write | Enforced with retained no-follow directory descriptors | Exact `filesystem.read` or `filesystem.write` grant |
| Endpoint HTTP/TLS | Enforced for the declared endpoint, method, and TLS identity | Exact `network.http` grant |
| Secret header | Enforced one-use sink tied to endpoint and header | Exact `secret.reveal` and network grants |
| Wall or monotonic clock | Enforced evaluation-local observation | Exact clock grant |
| Maintained process on Linux | Parent identity, argv, environment, output, and deadline bound; child internals unenforced | Exact `process.run` grant with `acknowledge-unenforced` |
| Maintained process on macOS | Unsupported descriptor-backed start | No grant makes it executable |
| Unrecognized target | Unknown | Refused until an adapter establishes the boundary |

On macOS a process-bearing task receives a `CHECK008` finding and an `unsupported` request verdict. Plan can preserve that finding in a refused artifact, but execute never starts a pathname process. An unrecognized target receives `CHECK009` and an `unknown` verdict. Artifact readers preserve unknown as non-executable rather than calling it malformed.

The same host can still inspect, check, and audit a process-bearing task. Qualified secret-free accepted execution is available on macOS. [Platform support](../platform-support.md) states the evidence claim; [authority](authority.md) describes verdict composition.
