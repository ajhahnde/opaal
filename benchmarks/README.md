# Performance benchmarks

The OPAAL performance suite measures seven host-only surfaces from an optimized
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

Cold is the first observation in a fresh benchmark workspace and process
sequence; it does not claim flushed system caches or power-on state. The
qualification profile discards three warmups and retains fifteen samples for
warm cases; cold cases retain one sample.

Startup uses a minimal directive-free `.opaal` file. Structured-stream
measurement calls the pure carrier fixture. Completion uses an explicit
`^ben` external head, sees only the temporary candidate directory through
`PATH`, and never executes a candidate.

## Run and validate

```sh
python3 benchmarks/run.py --profile smoke
python3 benchmarks/run.py --profile qualification \
  --output benchmarks/evidence/host-darwin-arm64-candidate.json
python3 -m unittest discover -s ci/tests -p 'test_check_benchmarks.py'
python3 ci/check_benchmarks.py --contract-only
python3 ci/check_benchmarks.py \
  --result benchmarks/evidence/host-darwin-arm64-candidate.json \
  --environment host-darwin-arm64
```

The runner builds the optimized CLI and benchmark fixture, creates isolated
temporary inputs, retains raw integer samples, records a versioned JSON result,
and invokes the checker before success.

The standard-library checker validates schemas, exact case coverage, unique
measurements, units, sample counts, raw summaries, contract and binary digests,
budget coverage and arithmetic, environment matching, and regression direction.
[`budgets-v1.toml`](budgets-v1.toml) derives every ceiling from the retained
candidate evidence.

## Evidence boundary

Checked-in evidence is one bounded macOS-arm64 observation under recorded host
load. It is not a universal product guarantee and does not establish external
packaging, another OS or architecture, emulation, or physical hardware.

[← OPAAL documentation](../docs/README.md)
