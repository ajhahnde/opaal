# CI job check

This self-contained offline example uses native standard-stream APIs.
The controlled CI route requires a build supporting `execute --require-true`;
OPAAL 1.3.0 rejects that option safely. Set up the candidate programs as described in the
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
opaal task list --project opaal.toml
opaal task inspect --project opaal.toml sample --format json
sh project-ci.sh
opaal audit inspect ci.audit.json
```

The shell acquires input before any task work, discovers/checks/plans the task,
accepts the persisted digest, and runs `execute --require-true` with mandatory
receipt evidence. It suppresses successful administrative output and leaves
stdout/stderr for the program. Use a fresh directory or fresh evidence paths
for each run; existing evidence is never overwritten.

The action prints the same JSON and returns `result.passed` as Bool. The CLI
observes that value after owned cleanup and evidence persistence: true exits 0,
false exits 1. A negative check still has primary `success` and a false
assessment. Invalid input, refusal, timeout, output/cleanup or evidence failure
exits 2. Requested cancellation retains 130 even if later evidence fails.
Producer exit 7 maps to outer exit 2 with `acquisition failed`, and no task
journal or receipt. Printed bytes never decide the result. Ordinary execute
without the selector still exits 0 for a completed false Bool.

Journal/audit v4 and receipt v2 record `result_policy`, nullable Bool assessment,
and typed primary `cancel_reason`. An assessment alone cannot prove success;
require complete matching evidence and no secondary failures or omissions.
Incomplete audits have null assessment; a cancelled action-end without terminal
may have unknown null reason. Complete cancellation evidence requires requested
or timeout. The three stream roles retain separate grants and byte counts.
Recoverable Errors explicitly caught by ordinary tasks may still return Bool;
this reference propagates every acquisition/decode Error. Failed output remains
non-pass even when caught. Legacy artifacts retain their original meanings;
new toolchains emit the new versions without rewriting old evidence.
