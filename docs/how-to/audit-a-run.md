# Audit a run

Name the explicit project, the journal written by one admitted execute request, and a new audit target:

```sh
opaal audit --project opaal.toml --journal welcome.run.jsonl --out welcome.audit.json
opaal audit inspect welcome.audit.json
```

Audit checks every complete canonical journal line: schema, run ID, contiguous sequence, digest, previous link, paired action and effect boundaries, cleanup, and terminal ordering. It then publishes an exclusive, redacted `opaal.audit.v2` artifact. It does not call an adapter, resume the task, or grant authority.

A complete valid journal produces a `complete` audit. If all complete lines are valid but no terminal record exists, audit writes an inspectable `incomplete` artifact and exits 1. It may ignore one truncated final line only after validating the preceding complete lines. Earlier corruption refuses without publishing success. Inspection validates the audit artifact itself; it does not reread the journal.

An audit records observed work and partial effects. It does not attest an external peer or undo effects. See [journal](../reference/formats/journal-artifact.md) and [audit artifact](../reference/formats/audit-artifact.md) for fields and limits.
