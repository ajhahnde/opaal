# CI job report

These fixtures use synthetic jobs and need no account or network access. The
ordinary scripts require `cat`. `report.opaal` owns the transformation policy:
normalize names, validate job states, count failed and pending jobs, stably sort
failures by name, and retain the first ten. JSON output has no trailing newline.
The human report escapes job names as JSON strings before printing them.

From this directory, run a binary containing the data processing operations:

```sh
opaal check json-report.opaal
opaal format --check report.opaal json-report.opaal text-report.opaal policy-operations.opaal tasks.opaal
opaal json-report.opaal > actual-report.json
cmp actual-report.json expected-report.json
opaal text-report.opaal
python3 reference.py jobs.json
```

A successful report can contain failures or pending jobs. Exit zero means report
production succeeded. Acquire live exports in a separate successful step before
processing them; a default pipeline's final status does not prove its producer
succeeded. For example, with caller-supplied repository and run variables:

```sh
gh run view "$RUN_ID" --repo "$REPOSITORY" --json jobs > jobs.json && opaal json-report.opaal > actual-report.json
```

Discard output after any unsuccessful command. The export can be partial, and
an older report file can remain after failed acquisition.

`valid/` contains inputs and exact JSON/text outputs for empty and pending jobs,
all supported completed conclusions, stable ordering and truncation, escaped
Unicode/control characters, and a full producer-shaped document with extra
fields. `invalid/` contains malformed JSON, duplicate fields, invalid UTF-8 and
unsupported job shapes/states. Required job fields must be Strings; pending
jobs may omit `conclusion`, while null is rejected. Extra fields are ignored.
The independent Python reference rejects invalid JSON and job data too.

`tasks.opaal`, `opaal.toml` and `authority.toml` describe the filesystem-only
project route. Copy `tools-linux.toml` or `tools-macos.toml` to `tools.toml` in a
fresh example directory. The project requires OPAAL 1.2.0. Automated tests of
an unreleased binary adjust only the copied manifest's version floor to that
binary's actual version; they do not change the published fixture.

Project execution binds the source graph and `--input-file` content to an
accepted plan, records the read/write effects, and writes `report.json`.
Check and plan do not validate JSON data. Failed execution may retain completed
reads or writes; inspect its journal and audit. No automatic rollback is claimed.
