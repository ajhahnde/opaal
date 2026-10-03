# Performance benchmarks

The OPAAL performance suite measures eleven host-only surfaces from an optimized
candidate. It contains no external-command, pipeline, operating-system image,
or alternate-runtime validation path.

## Contract

[`contract-v1.toml`](contract-v1.toml) owns the exact case and repetition set:

| Case | Boundary | Statistic |
| --- | --- | --- |
| `host-startup-cold` | First optimized `opaal` process running a minimal pure source | maximum elapsed ns |
| `host-startup-warm` | New optimized processes after discarded warmups | p95 elapsed ns |
| `host-first-prompt-cold` | First prompt from a fresh PTY process | maximum elapsed ns |
| `host-first-prompt-warm` | Fresh PTY processes after discarded warmups | p95 elapsed ns |
| `host-structured-stream-memory-warm` | Peak RSS while a fixture lazily pulls typed values | maximum bytes |
| `host-completion-cold` | First completion snapshot/query over isolated candidates | maximum elapsed ns |
| `host-completion-warm` | Completion snapshots/queries after discarded warmups | p95 elapsed ns |
| `operational-task-inspect-warm` | Release-readiness task inspection | p95 and maximum elapsed ns |
| `operational-project-check-warm` | Release-readiness project check | p95 and maximum elapsed ns |
| `operational-plan-build-render-warm` | Build and inspect an exact 1 MiB, 1,024-action plan | p95 and maximum elapsed ns |
| `operational-journal-audit-render-warm` | Audit and inspect an exact 16 MiB journal | p95 and maximum elapsed ns; maximum peak RSS |

Cold is the first observation in a fresh benchmark workspace and process
sequence; it does not claim flushed system caches or power-on state. The
qualification profile discards three warmups and retains fifteen samples for
warm cases; cold cases retain one sample.

Startup uses a minimal directive-free `.opaal` file. Structured-stream
measurement calls the pure carrier fixture. Completion uses an explicit
`^ben` external head, sees only the temporary candidate directory through
`PATH`, and never executes a candidate. The maximum journal remains below the
independent 100,000-line ceiling, proves refusal at the first excess byte, and
retains separate exact-limit and first-excess line properties.

## Run and validate

```sh
python3 benchmarks/run.py --profile smoke
python3 benchmarks/run.py --profile qualification \
  --budget-environment host-darwin-arm64 \
  --output benchmarks/evidence/host-darwin-arm64-candidate.json
python3 -m unittest discover -s ci/tests -p 'test_check_benchmarks.py'
python3 ci/check_benchmarks.py --contract-only
python3 ci/check_benchmarks.py \
  --result benchmarks/evidence/host-darwin-arm64-candidate.json \
  --environment host-darwin-arm64
```

The runner builds the optimized CLI and benchmark fixture, creates isolated
temporary inputs, retains raw integer samples, records a versioned JSON result,
and invokes the checker before success. A result is compared with a retained
host budget only when `--budget-environment` explicitly selects it; an
unselected run validates the complete profile and result contract without
claiming equivalence to that retained host.

The standard-library checker validates schemas, exact case coverage, unique
measurements, units, sample counts, raw summaries, contract and binary digests,
budget coverage and arithmetic, environment matching, and regression direction.
[`budgets-v1.toml`](budgets-v1.toml) derives every ceiling from the retained
candidate evidence.

## Evidence boundary

Checked-in evidence is one bounded macOS-arm64 observation under recorded host
load. It is not a universal product guarantee and does not establish external
packaging, another OS or architecture, emulation, or physical hardware.

## Report and callable comparison

`report-contract-v1.json` defines a separate 21-case before/after comparison:
reports with 0, 1, 100, 1,000 and 10,000 jobs; empty startup; unused definitions
and repeated closure construction; restoration; cold and warmed calls; empty
and repeated callback preparation; and 50,000 retained originals or clones.
Cold runs create fresh callable families and include their first invocation.
Only the explicitly warmed-call case performs an initial call before its loop.

Build each version from a fresh public source export, with the same declared
Rust toolchain and release profile. The export contains no Git metadata or
account state. The builder refuses the checkout and retains source, compiler,
command, helper-source and binary identities. It builds the CLI and raw API
fixture, then temporarily replaces semantic tests in the export with the phase
fixture. Counters, race scheduling, weak-reference observers and allocator
diagnostics are removed from measured sources. The original exported source is
restored after that build; the retained phase sources describe the measured
executable. No runtime option or alternate evaluator path is introduced.

```sh
python3 ci/build_report_performance.py --source BEFORE_EXPORT --output BEFORE_BUILD
python3 ci/build_report_performance.py --source AFTER_EXPORT --output AFTER_BUILD
python3 ci/measure_report_performance.py \
  --before BEFORE_CLI --after AFTER_CLI \
  --before-raw BEFORE_RAW --after-raw AFTER_RAW \
  --before-phases BEFORE_PHASES --after-phases AFTER_PHASES \
  --output comparison.json
```

The build manifests identify the three executable paths for each version.
Use the same host for a pair and qualify Linux x86_64 and macOS arm64 separately.
The runner retains three discarded process warmups and seven alternating pairs,
output bytes, wall time, user/system CPU and peak RSS. Every large-report pair
must improve wall and CPU by more than the larger of 5% of the baseline median
or its full observed spread. All other cases allow median regression of at
most the larger of 5% or 2 ms. Every case permits peak RSS growth of at most the
larger of 5% or 1 MiB. A report gain cannot waive a construction or memory failure.
Invalid JSON diagnostics and valid JSON/text reports must also match exactly.
Each attempt needs a new output path; failed attempts remain evidence.

These checks qualify the named workloads and exact binaries. They do not imply
uniform speed gains for other programs or acceptable overhead on every host.

[← OPAAL documentation](https://opaal-lang.org/docs/)
