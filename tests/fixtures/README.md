# Test fixtures

These small Rust child programs are shared by the POSIX adapter and runtime
acceptance tests. They invoke no shell or host utility and contain no unsafe
code.

- `process_observer.rs` writes a length-prefixed binary report containing cwd,
  selected native environment values, descriptor visibility, and exact native
  argv. `FLASH_PROBE_REPORT` selects the report path, `FLASH_PROBE_VALUE` supplies
  the inspected environment value, and `FLASH_PROBE_FD` selects a descriptor.
- `status.rs` accepts `exit CODE` for ordinary completion or `signal` for a real
  `SIGABRT` termination.
- `stream.rs` provides deterministic `source`, `relay`, `sink`, `both`, and
  `both-closed` modes for stream, status, merged-output, and descriptor tests.
