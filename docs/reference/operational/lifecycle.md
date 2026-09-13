# Project lifecycle

The user-visible sequence is:

```text
task inspect → check → plan → plan inspect and human review
             → execute with the exact digest accepted for that request
             → journal → audit → audit inspect
```

`task inspect` reports the exported action without authority. `check` validates the selected source closure, inputs, manifest, authority, tool lock, request set, and host capability without invoking the task, reading a secret, probing a tool, or calling an adapter. JSON output is a canonical `opaal.check.v2` artifact; silent success is the default.

`plan` reads bounded identities and one wall-clock instant, writes an expiring `opaal.plan.v2` file exclusively under the project root, and prints its digest. It does not invoke the action, read a secret, probe a tool, or contact an endpoint. A process-bearing task on macOS can produce a refused, non-executable plan. `plan inspect` validates and renders the artifact for review.

`execute` requires `--accept` to equal the complete plan digest byte for byte. Acceptance belongs to that request; there is no separate persisted accepted state. Before the task receives a controlled host, OPAAL checks expiry and revalidates project, source, input, authority, lock, environment, CA, executable, platform, and toolchain identities. It then creates one exclusive, synced, hash-chained `opaal.run-journal.v2` journal. It does not retry an effect implicitly.

`audit` validates and projects journal evidence into `opaal.audit.v2` without granting authority or resuming work. A valid complete-line prefix without a terminal record becomes an inspectable incomplete audit and exits 1. Earlier corruption refuses without publishing success. `audit inspect` validates the audit artifact; only audit generation re-reads the journal.

The journal is evidence, not a transaction or rollback guarantee. There is no journal-inspection CLI command and no execution resumption. The [plan](../formats/plan-artifact.md), [journal](../formats/journal-artifact.md), and [audit](../formats/audit-artifact.md) references give file rules.
