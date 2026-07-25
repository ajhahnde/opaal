# Fuzz targets

The `lexer` and `parser` targets accept arbitrary bytes. Valid UTF-8 inputs are
passed through the public syntax APIs; invalid UTF-8 inputs exercise source-file
loading and are then rejected normally.

Run a bounded smoke campaign for both targets from the repository root:

```sh
./fuzz/run-smoke.sh
```

Pass a run count to change the default 1,000 executions per target:

```sh
./fuzz/run-smoke.sh 10000
```

The runner supplies every `.fsh` file below `tests/golden/grammar` and
`tests/golden/lexical` as canonical seeds. It puts libFuzzer's writable corpus
in a temporary directory, so fuzzing never modifies the golden sources.

The fuzz package is a separate workspace because cargo-fuzz requires nightly
compiler instrumentation. Install `cargo-fuzz` and a nightly Rust toolchain
before running it.
