# Run journal

An admitted `execute` request exclusively creates a project-contained, mode-`0600` `opaal.run-journal.v2` JSON Lines file. OPAAL writes and syncs the initial header in a private sibling, publishes it atomically to the exact absent target, and syncs the parent directory. Later canonical lines are synced in hash-chain order.

Each line has the closed fields `schema`, `schema_version`, `run_id`, `seq`, `previous`, `kind`, `payload`, and `digest`. Sequence starts at zero with one header. Each subsequent line names the preceding digest. Events include paired action and effect boundaries, cleanup, and one terminal record. The terminal record follows closed boundaries and records the primary outcome and cleanup. A completed effect remains evidence if a later action fails.

A secret-bearing HTTP call records `network.http` boundaries around nested `secret.reveal` boundaries. Payloads retain the secret identity and sink scope, not secret bytes. Filesystem descriptors use project-root-relative paths; process arguments use lengths and digests. HTTP bodies, header values, environment values, observed clock values, and absolute project roots are not journal descriptors.

The journal has a 16 MiB and 100,000-line ceiling. No effect is retried implicitly. A journal is evidence of observed boundaries, not an attestation, transaction, rollback mechanism, or resumable execution log. Use [audit](audit-artifact.md) to validate it; there is no journal-inspection CLI command.
