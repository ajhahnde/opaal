# Test fixtures

These small Rust programs provide host-only observation boundaries for adapter,
runtime, CLI, terminal, and benchmark tests. They invoke no shell and contain no
unreviewed unsafe code.

- `process_observer.rs` reports cwd, selected native environment values,
  descriptor visibility, process-group state, signals, and exact native argv.
- `status.rs` returns an explicit exit code or a real signal termination.
- `stream.rs` supplies deterministic source, relay, sink, and closed-endpoint
  behavior for stream/status tests.
- `terminal_editor.rs` drives isolated prompt, completion, history, restoration,
  and external-notice observations.
- `benchmark.rs` exposes direct completion, lazy structured-stream, and pure
  OPAAL analysis/evaluation probes.

Fixture binaries and their `OPAAL_*` environment protocol names are test-only.
They are not installed language tools and do not expand source authority.

[← OPAAL documentation](../../docs/README.md)
