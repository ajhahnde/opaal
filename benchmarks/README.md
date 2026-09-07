# Performance benchmarks

The OPAAL performance suite measures seven host-only surfaces from an optimized
candidate. It contains no external-command, pipeline, operating-system image,
or language-runtime validation path.

## Contract

[`contract-v1.toml`](contract-v1.toml) owns the exact case and repetition set:

| Case | Boundary | Statistic |
| --- | --- | --- |
| `host-startup-cold` | First optimized `opaal` process running a minimal pure `.opaal` source | maximum elapsed ns |
| `host-startup-warm` | New optimized processes after discarded warmups | p95 elapsed ns |
| `host-first-prompt-cold` | First prompt from a fresh PTY process | maximum elapsed ns |
| `host-first-prompt-warm` | Fresh PTY processes after discarded warmups | p95 elapsed ns |
| `host-structured-stream-memory-warm` | Peak RSS while a direct fixture lazily pulls typed values | maximum bytes |
| `host-completion-cold` | First completion snapshot/query over isolated injected candidates | maximum elapsed ns |
| `host-completion-warm` | Completion snapshots/queries after discarded warmups | p95 elapsed ns |

Cold means the first observation in a fresh benchmark workspace and process
sequence. The runner does not flush system caches or claim power-on state. Warm
startup and prompt samples still create new processes. The qualification
profile discards three warmups and retains fifteen samples; cold cases retain
one sample.

Startup uses `language 1` in a minimal `.opaal` file. Interactive mode
preselects OPAAL language 1. Structured-stream measurement calls the direct
pure carrier fixture. Completion sees only the temporary candidate directory
through `PATH` and never executes a candidate.

## Run

From the standalone repository root:

```sh
python3 benchmarks/run.py --profile smoke
python3 benchmarks/run.py --profile qualification
```

The runner resolves the pinned Rust tools, builds `target/release/opaal` and
`target/release/opaal-benchmark-fixture`, creates an isolated temporary home and
completion set, retains raw integer samples, writes a JSON result under the
ignored `benchmarks/results/` directory by default, and invokes
`python3 ci/check_benchmarks.py --result RESULT` before success.

The reviewed macOS-arm64 evidence command is:

```sh
python3 benchmarks/run.py --profile qualification \
  --output benchmarks/evidence/host-darwin-arm64-opaal-v1.json
```

The result uses schema `opaal-performance-result-v1` and binds the current
contract and candidate binary by SHA-256.

## Validate

```sh
python3 -m unittest discover -s ci/tests -p 'test_check_benchmarks.py'
python3 ci/check_benchmarks.py --contract-only
python3 ci/check_benchmarks.py \
  --result benchmarks/evidence/host-darwin-arm64-opaal-v1.json \
  --environment host-darwin-arm64
```

The standard-library checker validates contract/result schemas, exact case
coverage, unique measurements, units, profile sample counts, raw summaries,
contract and binary digests, budget coverage and arithmetic, environment
matching, and regression direction.

[`budgets-v1.toml`](budgets-v1.toml) derives each absolute ceiling from the
candidate OPAAL evidence statistic. Cold latency uses 4× the observed maximum;
warm latency and structured-stream memory use 3× p95 or maximum as declared.
These factors absorb ordinary host scheduling and cache variance. They do not
justify increasing a limit to hide an unexplained regression.

## Evidence boundary

The checked-in evidence is one bounded macOS-arm64 observation under recorded
host load. It is not a universal product guarantee and does not establish
another operating system's package, image, target, emulation, or physical
hardware performance. A different OS, architecture, or measurement contract
requires separately keyed evidence and budget policy.

[← OPAAL documentation](../docs/README.md)
