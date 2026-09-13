# Audit artifact

`opaal audit --project opaal.toml --journal RUN.jsonl --out AUDIT.json` validates a journal and exclusively publishes one canonical `opaal.audit.v2` object beneath the selected project root. It does not grant authority, invoke an action, or resume work. `opaal audit inspect PATH` validates the audit and prints its bounded redacted view.

The exact top-level fields are `schema`, `schema_version`, `run_id`, `plan_digest`, `accepted_plan_digest`, `authority_digest`, `started_at`, `finished_at`, `journal_header_digest`, `validated_prefix_digest`, `validated_line_count`, `events`, `primary`, `cleanup`, `journal_terminal_digest`, `completeness`, and `digest`. Version 2 binds the validated header and final complete line. For an incomplete prefix, finish time and terminal digest are null.

Audit checks every complete line's closed schema, contiguous sequence, digest, previous link, run identity, and legal event nesting. A valid complete-line prefix with no terminal record produces `completeness = "incomplete"` and exits 1; a truncated final line is ignored only after all earlier complete lines validate. Earlier parse, schema, chain, or sequence corruption refuses without publishing success. Content after a terminal record is corruption.

Inspection verifies the audit's own closed schema and digest. Only audit generation revalidates the source journal bytes. See [run journal](journal-artifact.md) and [audit a run](../../how-to/audit-a-run.md).
