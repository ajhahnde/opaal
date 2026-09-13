# Authority

An authority file records exact grant or deny rows for one project and environment. A missing row denies the request. There are no wildcard, prefix, inherited, combined, or ambient grants. Duplicate rows, including a grant/deny conflict, refuse the entire document. An action's declared effect is necessary for a request to be considered, but it is not itself permission.

The typed families are project-path `filesystem.read` and `filesystem.write`, maintained-tool `process.run`, endpoint-and-method `network.http`, secret-to-endpoint/header `secret.reveal`, and evaluation-local `clock.wall` and `clock.monotonic`. A row's scope must match its family and its named project identity. Direct filesystem, HTTP, secret-sink, and clock operations require enforced boundaries. Maintained process execution may instead use `acknowledge-unenforced` for the child's internal behavior.

OPAAL combines the row with the adapter's enforcement report into five verdicts: `Denied`, `Unknown`, `Unsupported`, `GrantedEnforced`, and `GrantedUnenforced`. An effect outside the declared set is `Unknown` even if a matching row exists. An unenforced process request runs only when its exact grant acknowledges that status. A target that cannot establish enforcement remains unknown or unsupported, never silently granted.

Check reports the static request verdicts without calling an adapter. Plan binds the authority bytes and the host verdict into one expiring artifact. Execute revalidates them before the accepted action. See the [authority TOML format](../formats/authority-toml.md), [capability matrix](capability-matrix.md), and [security model](../../concepts/security-model.md).
