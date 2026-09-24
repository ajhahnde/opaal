# Resources and lifetimes

A lazy value stream can be consumed once. Pulling it may reach a limit, cancellation, or a terminal failure after earlier values have been delivered. OPAAL keeps those events distinct and runs the stream's cleanup path rather than turning a partial stream into an ordinary completed list.

A controlled evaluation owns its cancellation scope, deadline, secrets, and adapter handles. Cancellation keeps the first observed reason. A later phase may shorten a deadline but not extend it; equality with the deadline is timeout. An embedder registers cleanup before exposing a handle. Finishing or dropping the context invokes every cleanup once in reverse registration order, even if an earlier cleanup fails.

In the 1.1 direct CLI route, a foreground pipeline owns one process group.
OPAAL waits for its members on normal completion and terminates and reaps the
owned group after startup or wait failure or a supported `SIGINT`, `SIGTERM`,
or `SIGHUP`. The client restores foreground terminal ownership before the
prompt resumes. A child can still create effects that cleanup cannot undo;
forced host death and descendants that leave the group are outside the
cleanup guarantee.

A secret is also a lifetime-bound resource. Its bytes enter only by injection, pass only through one authorized header sink, and are overwritten when consumed. Redaction patterns remain until the evaluation closes so a later diagnostic cannot reveal a consumed secret. These rules explain why a second reveal or reinjection fails instead of quietly reusing a value.

[Outcomes](../reference/language/outcomes-and-errors.md), [embedding context](../reference/embedding/operational-context.md), and [limits](../reference/limits.md) give the exact contracts.
