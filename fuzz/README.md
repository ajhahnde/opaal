# Fuzz targets

The separate unpublished `opaal-fuzz` package owns four targets:

- `lexer` checks lossless tokenization and progress;
- `parser` checks bounded parsing and syntax-tree traversal;
- `expander` checks pure word expansion; and
- `resources` varies analysis and evaluation ceilings and cancellation.

Invalid UTF-8 is rejected through the normal source boundary. Targets launch no
process and perform no platform I/O.

Install cargo-fuzz and a nightly toolchain, then run every target from the
repository root:

```sh
fuzz/run-smoke.sh
fuzz/run-smoke.sh 10000
```

The smoke script uses 1,000 executions per target by default. Inputs are seeded
from current grammar, lexical, module, operation, outcome, rest/spread, and type
corpora. Writable corpora live in a temporary directory; checked-in sources are
never modified.

For a sustained campaign, choose seconds per target and optionally a new result
directory:

```sh
fuzz/run-campaign.sh 600
fuzz/run-campaign.sh 3600 /path/to/new-results
```

An explicit result directory must not already exist. Default campaigns use an
ignored directory under `fuzz/campaigns/`. Each input is limited to 4,096 bytes,
ten seconds, and 2,048 MiB resident memory.

Review, reproduce, and minimize every failure before retaining a regression.
Bounded completion is evidence for the exercised target and corpus, not proof
that defects are absent.

[← OPAAL documentation](../docs/README.md)
