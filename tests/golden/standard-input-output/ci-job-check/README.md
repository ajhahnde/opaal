# CI job check

This self-contained offline example requires the native standard-stream APIs
in OPAAL 1.3.0. Set up the candidate programs as described in the
[standard-stream guide](../README.md), then enter this directory. No account,
network, or repository checkout is needed.

```sh
opaal check source.opaal
opaal format --check assess.opaal report.opaal source.opaal tasks.opaal
python3 ci.py
```

For the included `jobs.json`, stdout is exactly the following JSON, with no LF:

```json
{"passed":true,"report":{"failed":0,"failures":[],"pending":0,"total":1}}
```

Exit zero means acquisition, processing and domain assessment all passed.
`assess.opaal` adds `passed` to the unchanged report: it is true exactly when
both failed and pending counts are zero. Empty jobs pass. Queued/in-progress
jobs fail; completed success/skipped/neutral jobs pass; failure/cancelled/
timed_out/action_required/stale/startup_failure jobs fail. The report still
normalizes names, sorts failures stably, retains the first ten and counts all.
`report.opaal` and its `valid/`/`invalid/` fixtures are exact copies of the
maintained [report reference](../../data-processing/README.md); packaging and
tests enforce equality. The original report-only scripts still exit zero when
report production succeeds, including a report with failures or pending jobs.

Replace `jobs.json` with `{"jobs":[{"name":"build","status":"queued"}]}`.
`python3 ci.py` emits `passed:false`, pending 1, total 1 and exits 1 with empty
stderr. `{"jobs":[]}` emits `passed:true`, all counts zero, and exits zero.
Malformed JSON, duplicate fields, invalid UTF-8, wrong shapes/types and unknown
states/conclusions emit no report and exit 1 with a diagnostic. The report-shaped
JSON distinguishes a negative assessment from invalid input; exit 1 alone does
not. The source reads through EOF with a 65536-byte cap and writes exact JSON.

`ci.py` runs `producer.py` to completion before launching OPAAL. Change the
producer's exit from 0 to 7 without changing its complete output: CI exits 7,
emits only `acquisition failed` on stderr and launches no processor. It never
reads a prior input/report artifact. Keep this acquisition gate when replacing
the offline producer with a live exporter. Default pipeline status does not
prove producer success. Overflow, Broken Pipe and cancellation can consume
input or emit incomplete output; discard unsuccessful reports and do not replay
uncertain transfers automatically.

## Controlled CI route

Copy the matching `tools-linux.toml` or `tools-macos.toml` to `tools.toml`.
In a fresh directory, restore the producer's successful exit and run:

```sh
python3 project-ci.py
opaal audit --project opaal.toml --journal ci.run.jsonl --out ci.audit.json
opaal audit inspect ci.audit.json
```

The driver acquires input, checks and plans the declared task, accepts its exact
digest, and writes a separate journal and receipt. Use a fresh directory or new
run paths for each execution; existing evidence is never overwritten.
For queued jobs the action succeeds as domain data: execution exit zero,
successful receipt, `passed:false` output. Only after validating execution and
receipt identity, complete journal state and absence of secondary failures does
CI map that Bool to exit 1. Invalid input is an action Error, produces no report
and CI exits 1. The driver uses fixed diagnostics for failed evidence and keeps
administrative text out of the language's streams. The three declared roles
retain distinct grants and counts, including zero stderr transfers. Receipt
values/digests remain suppressed; inspect safe outcome/progress metadata.
