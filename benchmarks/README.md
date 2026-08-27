# Performance benchmarks

[FlashOS](../../../README.md) › [Flash](../README.md) › Performance Benchmarks

Flash's versioned performance contract measures the real optimized `fsh`
process where an executable boundary exists and uses a host-only fixture for
the in-process structured-stream and completion boundaries. It retains raw
integer samples, derives reviewable budgets from qualification evidence, and
keeps host and FlashOS target results separate.

## Measured surfaces

[`contract-v1.toml`](contract-v1.toml) is the case and repetition source of
truth. The suite owns these measurements:

| Surface | Host boundary | FlashOS target boundary |
|:--|:--|:--|
| Startup | Optimized `fsh` executing an empty script | Included in the login-shell first-prompt observation; no separate guest process timer is claimed |
| First prompt | Fresh PTY process with config and history disabled | Host-monotonic interval from the login success marker to the first target prompt |
| Command overhead | Per-command time for a repeated external `true` script | Serial-observed latency of a source/output-distinct external `printf` probe |
| Pipeline throughput | A fixed file through two `cat` stages and `wc` | One MiB through `yes`, `head`, `wc`, and a marker-transforming `tr` stage |
| Structured-stream memory | Peak RSS of five million lazily pulled `Value::Int` items | Not measured: the current target exposes no qualified per-process peak-RSS telemetry |
| Completion latency | Prompt-boundary host snapshot plus grammar-aware query over fixed command/path fixtures | Tab-to-accepted-completion latency through the portable editor and emulated UART |

The target pipeline's bounded `yes` producer reaches an expected broken pipe
after `head` has consumed the requested byte count. The transformed count
marker, rather than the typed source or diagnostic, closes the timed interval.

## Cold and warm semantics

"Cold" means the first observation in a newly created benchmark workspace and
process sequence. The runner does not flush kernel caches, request privileges,
or claim a power-on storage state. This definition is reproducible without
mutating the host and is recorded in every result.

"Warm" means observations after the cold case and the profile's discarded
warmups. Each measured operation still uses the process boundary named by its
case. Warm startup and first-prompt samples therefore start new `fsh` processes;
they do not reuse a live shell.

The qualification profile uses three discarded host warmups and fifteen raw
host samples. The exact-image target consumer uses one discarded warmup and
five raw samples for each warm target case. Cold cases retain one sample rather
than pretending that repeated first observations are independent.

## Run the host suite

From the repository root, acquire the independent Flash automation runtime and
the pinned host tools described in
[Public Automation](../../../docs/automation.md), then select that runtime:

```sh
make flash-bootstrap flash-automation-tools
export FLASH_AUTOMATION_RUNTIME="$PWD/build/flash-bootstrap/134635a5e1282b5d8455a4b2aeb754be5a3a77c1/fsh"
python3 components/flash/benchmarks/run.py --profile smoke
python3 components/flash/benchmarks/run.py --profile qualification
```

The runner builds optimized `fsh` and `flash-benchmark-fixture` binaries unless
`--no-build` is supplied. It uses an isolated temporary home, disables config
and history, fixes the locale to `C`, limits completion discovery to the fixed
fixture directory, creates deterministic completion and pipeline fixtures,
discards warmups, and writes a unique ignored JSON result under
`benchmarks/results/` by default.

Evaluate a qualification run only against a matching environment budget:

```sh
python3 components/flash/benchmarks/run.py \
  --profile qualification \
  --budget-environment host-darwin-arm64
```

The ordinary CI job runs and schema-validates the smoke profile to catch broken
probes and missing coverage. It does not compare an Ubuntu hosted runner with
the tracked macOS baseline. Cross-environment absolute comparisons are invalid.

## Run the FlashOS target suite

The exact-image QEMU consumer accepts a result path:

```sh
python3 ci/qemu_smoke.py \
  --image build/x86_64/flashos/harddrive.img \
  --disk-interface nvme \
  --log build/x86_64/flashos/qemu-benchmark.log \
  --benchmark-output build/x86_64/flashos/qemu-performance.json
```

Target measurements run after the existing runtime fixtures and exhaustive
capability matrix. They use one `core2duo` TCG vCPU, 1024 MiB of guest memory,
an NVMe snapshot, the host monotonic clock, bounded UART interactions, and the
exact image named in the raw result by SHA-256. The QEMU consumer evaluates the
result against the matching target budget before reporting success. Hosted
candidate runs upload their JSON as short-lived workflow evidence.

## Evidence and budget derivation

Qualification evidence under [`evidence/`](evidence/) is immutable input to
[`budgets-v1.toml`](budgets-v1.toml). The budget file binds the contract and
each evidence file by SHA-256. [`ci/flash_benchmarks.fsh`](../../../ci/flash_benchmarks.fsh)
checks schema versions, exact case coverage, raw summaries, environment
identity, evidence hashes, derivation arithmetic, and budget coverage.

Maximum latency and memory budgets multiply the owning evidence statistic.
Minimum throughput budgets divide it. Cold latency uses the retained maximum;
warm latency uses p95; structured-stream RSS uses the maximum; and throughput
uses the median. Host warm cases use 3× tolerance. Host cold cases retain only
one first observation, so they use 4× tolerance for scheduler and cache-state
variance that repetition cannot smooth. The TCG/serial target uses 3×
tolerance; its cold first-prompt interval is already dominated by the
controlled emulator and UART boundary. These factors are declared policy;
every absolute limit is mechanically derived from the bound evidence.

Validate the tracked contract or evaluate another matching result with:

```sh
"$FLASH_AUTOMATION_RUNTIME" ci/flash_benchmarks.fsh
"$FLASH_AUTOMATION_RUNTIME" ci/flash_benchmarks.fsh \
  --evaluate path/to/result.json \
  --environment flashos-qemu-tcg-core2duo
```

A regression is a selected latency or memory statistic above its maximum, or a
selected throughput statistic below its minimum. Investigate the result,
reproduce it on the same environment, and fix or explain the change before
replacing evidence. Never raise a budget merely to make an unexplained result
green. A new operating system, architecture, CPU/emulator identity, or
measurement contract requires separate evidence and a separately keyed budget.

## Evidence boundary

These results are bounded observations, not universal product guarantees. Host
samples reflect one OS/architecture class and current system load. TCG target
samples include serial transport and emulation overhead and are not physical
hardware performance. The suite does not flush caches, pin CPU frequency,
disable other host work, measure energy, establish whole-OS boot budgets, or
claim target RSS without telemetry. Medians, p95 values, repetition, warmups,
and generous evidence-derived tolerances reduce noise; they do not eliminate
it.

Physical hardware, release qualification, long-duration behavior, workloads
outside the fixed fixtures, and performance on a different host or QEMU
configuration remain separate evidence.

---

[← Previous: Development](../docs/development.md) · [Flash documentation](../docs/README.md) · [Next: Scheduling stress →](../scheduling/README.md)
