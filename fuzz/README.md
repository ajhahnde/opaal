# Fuzz targets

[FlashOS](../../../README.md) › [Flash](../README.md) › Fuzz Targets

The `lexer`, `parser`, `expander`, `migration`, and `resources` targets accept arbitrary
bytes. Valid UTF-8 inputs are passed through the public syntax,
ordinary-word expansion, and read-only migration APIs; invalid UTF-8 inputs
exercise source-file loading and are then rejected normally. The migration
target uses a single explicit in-memory root, rejects import reads outside it,
varies every migration resource ceiling on a dedicated input route, and parses
every emitted JSON report. The resource target independently varies every v2
analysis ceiling, evaluator steps, call depth, retained collection items and
bytes, and cancellation schedule while exposing no import or host capability. The
expander, migration, and resource targets never launch
processes or perform platform I/O.

After building Flash, run a short campaign for all targets from the repository
root:

```sh
components/flash/target/debug/fsh components/flash/fuzz/run-smoke.fsh
```

Pass a run count to change the default 1,000 executions per target:

```sh
components/flash/target/debug/fsh components/flash/fuzz/run-smoke.fsh 10000
```

The runner uses every `.fsh` file below the v1 grammar/lexical corpora and the
v2 language-foundation grammar, lexical, module, operation, and type corpora as
a seed. It
puts libFuzzer's writable corpus in a temporary directory, so fuzzing never
modifies the golden sources. Each generated input is limited to 4,096 bytes,
ten seconds of execution, and 2,048 MiB of resident memory.

Run a sustained campaign for ten minutes per target:

```sh
components/flash/target/debug/fsh components/flash/fuzz/run-campaign.fsh
```

The first argument changes the duration per target in seconds. The optional
second argument selects a new result directory:

```sh
components/flash/target/debug/fsh components/flash/fuzz/run-campaign.fsh \
  3600 /path/to/results
```

The selected result directory must not already exist, preventing separate
campaign evidence from being mixed silently.

By default, writable corpora and failure artifacts are retained under a unique,
ignored `fuzz/campaigns/` directory. Review every artifact before removing a
campaign directory. Reproduce and minimize each failure, then retain its input
in a focused regression test or the appropriate golden corpus together with
the implementation fix.

The fuzz package is a separate workspace because cargo-fuzz requires nightly
compiler instrumentation. Install `cargo-fuzz` and a nightly Rust toolchain
before running it. Campaign completion is bounded evidence for the exercised
targets and inputs, not proof that defects are absent.

---

[← Flash documentation](../docs/README.md)
