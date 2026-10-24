# Docker image checks

These references require OPAAL 1.5.0 or later. Saved-input checks need no
Docker executable, daemon, account configuration, or network connection.
`policy.opaal` is the shared pure policy for saved and live data.

Create an explicit new offline project with the host-matched lock:

```sh
opaal project init --example image-check --destination image-check --environment offline
cd image-check
opaal project doctor --project opaal.toml --task saved --environment offline --input expected_id=sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa --input expected_platform=linux/amd64 --input expected_revision=abc123 --format json
```

The parent must exist and have no symlink components. Init embeds the maintained
assets, creates directories mode0700 and files mode0600, and never overwrites or
merges an existing destination, including a dangling symlink. Failed setup leaves
partial files for inspection; use a fresh destination for the next attempt.
Creation is not crash-atomic. Native paths retain their bytes; filesystems that
reject those bytes refuse setup. Generated controls require `>=1.5.0,<2.0.0`.
Existing project copies keep their original version requirement.

Doctor reads only explicit local controls, source, declared input files and
locked executable identities. It never evaluates initializers, reads task stdin
or secrets, launches a process or contacts Docker. Local readiness is static,
not plan acceptance or daemon health. Exit0 means locally ready, 1 means findings,
2 means invalid invocation/controls or I/O, and requested cancellation is130.
Text is the default; `--format json` writes a closed
`opaal.project-doctor.v1` schema_version1 view followed by LF. Its fields are
schema, schema_version, project_id, task_id, environment, local_readiness,
live_readiness and findings. Findings contain fixed code/category/severity/message,
a nullable validated input_name, source and span. Blocked dependent checks remain
unknown, and input values, source snippets and tool bytes are omitted. Every ci
view reports live_readiness=unknown. Views are bounded to128 findings and64KiB;
consumers must validate the complete view and exit, and reject unknown schemas.

Check and test the actual shared policy before reviewing a new execution plan:

```sh
opaal check policy-tests.opaal
opaal format --check policy.opaal saved-tasks.opaal policy-tests.opaal
opaal test --list policy-tests.opaal --format json
opaal test policy-tests.opaal
opaal test policy-tests.opaal --case test_negative_revision --format json
```

Cases are ordinary exported non-generic zero-argument Bool functions named
`test_`; imported modules are dependencies, not additional test inventories.
`--list` checks the complete root inventory without initializing it. `--case`
selects one exact exported name while validating every discovered signature.
Tests admit only proven-pure reachable calls and initializers, then initialize
fresh module contexts and invoke each ordinary function with no environment
snapshot or host bindings. Unused effectful definitions may remain; reachable
effects or unresolved dynamic calls refuse before the first case runs.

true passes, false fails, and an uncaught ordinary Error fails with no message
or payload in the result. These failures continue in name order. Ordinary
try/catch can assert an expected Error; resource catches retain consumed counters.
Uncaught resource Errors, invalid returns, refusal and infrastructure stop the
suite and retain remaining cases as unrun. Exit is0 for all passed/listed,
1 for a completed suite with ordinary failures, 2 for invalid/refused/resource/
infrastructure/output failure, and130 for requested cancellation.

The closed `opaal.test-results.v1` JSON schema_version1 ends with LF. Root keys
are schema, schema_version, source, mode, selected, completeness, cases, counts
and failure. Each case has name, callable_id (native module path and declared
function name), state and a nullable diagnostic. Diagnostics expose only fixed
code/kind and nullable native source/UTF8 byte span, with exclusive end. Counts
are discovered, selected, passed, failed, error and unrun. Listing uses state
listed and zero execution counts. Consumers must reject duplicate/extra fields,
unknown versions and incomplete output. Text renders the same fixed facts.
Limits are128 cases, one million steps per case including initialization,
ten million suite steps, depth256, one million collection items and16MiB retained
bytes per case, and64KiB output. Contexts are released between sequential cases.

To verify that cases observe the real policy, change `revision_ok` in
`policy.opaal` to `true`. `test_negative_revision` must fail with exit1. Restore
the original expression, check and format the source, and verify all cases pass.
Changed source invalidates existing plans; create and review a fresh plan.

Run the check with explicit expectations and exclusive evidence paths:

```sh
opaal task inspect --project opaal.toml saved
accepted=$(opaal plan --project opaal.toml --task saved --environment offline --input expected_id=sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa --input expected_platform=linux/amd64 --input expected_revision=abc123 --expires-in 900s --out saved.plan.json --format digest)
opaal plan inspect saved.plan.json
opaal task run --plan saved.plan.json --accept "$accepted" --journal saved.run.jsonl --receipt-out saved.receipt.json --audit-out saved.audit.json --require-true < fixtures/image.json
opaal task explain --receipt saved.receipt.json --audit saved.audit.json --format json
```

The policy compares the full observed image ID, platform and revision exactly.
It prints a JSON report and returns Bool. Exit 0 means the assessment passed
and execution, cleanup and evidence succeeded. Exit 1 means a completed negative
assessment; operational failures exit 2 and requested cancellation exits 130.
Use a fresh reviewed plan and evidence paths for each run. Acceptance is scoped to
this request; run neither generates a plan nor overrides its inputs. All evidence
paths must be distinct and absent, including hardlinks, symlinks and native path
aliases. The audit destination is reserved mode0600 before task effects. Failed
reservations and partial evidence remain for inspection; nothing is overwritten
or automatically removed. Owned task/stream/child finish precedes the bounded
audit. Audit or stderr summary persistence failure exits2 unless requested
cancellation retains130. Without `--require-true`, a completed false Bool is
ordinary data and exits0 after successful finish and evidence.

Task stdout remains the policy report; the native administrative summary uses
stderr and never interprets report bytes. `task explain` reads only bounded
explicit receipt/audit files, without reopening the journal, source or Docker.
It returns0 for a valid view even when the recorded task failed; invalid,
contradictory or unwritable views return2. A complete audit records a complete
execution, including negative and failed checks. Missing audit, incomplete
journal or legacy unknown assessment/cancellation facts cannot prove a pass.
Conclusion precedence is cancelled, then failed primary/cleanup/secondaries,
then unproven evidence, then passed/negative or execution-complete. Partial and
four-role confirmed/uncertain byte counts remain independent recorded facts.

Text and `opaal.task-explanation.v1` JSON schema_version1 expose run_id,
plan_digest, nullable result_policy/assessment/primary_code/cancel_reason,
execution, partial, progress, cleanup, journal, receipt, audit, evidence,
conclusion, safe secondary categories/codes and omitted_secondary_count. Unknown
codes are null; messages, exception payloads, secret references and report data
are omitted. Views end with LF and are bounded to64KiB. They report recorded
facts, without authenticating the artifact author or proving the original
writer's fsync or command exit. Existing execute/audit commands and persisted
schemas retain their meanings. To recover a valid journal prefix, use the old
`audit` command with a fresh output path; it does not rerun the task.

`std::docker::decode_image` requires a single-manifest `Descriptor` with a
supported OCI or Docker image media type and a full lowercase SHA-256 digest
equal to `Id`. Indexes, manifest lists and descriptorless responses fail before
policy evaluation. The ID is the observed Docker identity; it carries no
config-digest guarantee. Repository references remain separate data.

The decoder returns only the ID, OS, architecture, variant, optional revision,
and repository references. Missing revision is distinct from an empty String;
both produce a negative revision assessment. The whole document is bounded to
1 MiB of compact JSON, including ignored metadata; saved input also has a raw
1 MiB read limit. JSON depth, work and retained-value budgets still apply.
Selected revision/report fields are ordinary data and may contain sensitive
text. The reference emits them only as escaped JSON; other labels and raw
metadata are omitted.

Malformed saved JSON produces a fixed diagnostic so duplicate key names cannot
expose ignored metadata. The general `std::data` JSON API retains its ordinary
parser diagnostics.

`decode/cases.json` names selected-field, shape, UTF-8, depth and exact-limit
cases shared by runtime tests and saved-input qualification. `operations/cases.json`
names fixed client/server/API and failed-status cases for the controlled fake
adapter. These deterministic cases complement live host qualification.

To change the policy, edit `policy.opaal`, check the source, and create a new
plan. Printed bytes never determine the native assessment.

Change `expected_revision` to `wrong` for exit1. Supply `fixtures/invalid.json`
for exit2, with no report or assessment. Every run needs a fresh plan and exclusive
journal, receipt and audit paths. Existing evidence is never overwritten.

Repository engineering qualification retains original lifecycle drivers under
test-only fixtures and uses `comparison.py` and `run-comparison.sh` as independent
comparison inputs. They are not required
by generated projects. The Python comparison independently checks the same
Descriptor, selected fields and byte/depth limits and asserts0/1/2 report behavior;
it has no accepted-plan, journal, receipt, audit or owned-resource contract.
Neither implementation establishes image provenance or daemon confinement.

`source.opaal` performs the same check as an ordinary standalone script:

```sh
opaal check source.opaal
opaal format --check policy.opaal saved-tasks.opaal tasks.opaal source.opaal
opaal source.opaal < fixtures/image.json
```

A successful ordinary script exits 0, a completed negative assessment exits 1,
and an uncaught script error uses the ordinary script failure exit. Controlled
`--require-true` uses the separate 0/1/2/130 contract above. The policy is also
usable interactively after importing `std::docker` and `./policy.opaal`: pass a
saved Record to `docker::decode_image`, then pass the Image to `policy::assess`.
Live inspection requires controlled task execution.

## Live Linux inspection

The fixed `docker-cli-profile.json` and `docker-cli-profile-aarch64.json` require
Docker client and engine 29.9.0, API 1.48, and descriptor-bearing single-image
responses on Linux x86_64 and AArch64 respectively. Client and server architecture
must match the host. A version string alone does not qualify a binary or server. Independently verify the exact
installed executable, source/profile and server before using this reference.
Use one already available image; inspection never pulls, builds or runs it.

The saved ARM64 live fixture is `live/arm64-image.tar`. Load it with
`docker load --input live/arm64-image.tar`; it creates
`opaal-qualification/arm64:fixture` with manifest ID
`sha256:2a654a940aa4d12f5f291f5d600ac3b9a6166bb3c3a0f391f6626f049161328e`
and revision label `opaal-arm64-live-fixture-v1`. The native qualification
also uses an already-loaded, digest-pinned multi-platform reference to verify
that index responses are refused. These are test inputs; OPAAL does not pull,
build, or push images.

Use independently qualified installed Docker and a local Unix socket. Supply
both absolute paths and the profile's exact version as an explicit caller claim:

```sh
opaal project init --example image-check --destination live-check --environment ci --docker /usr/bin/docker --socket /run/docker.sock --qualified-version 29.9.0
cd live-check
opaal project doctor --project opaal.toml --task image --environment ci --input image=example:checked --input expected_id=sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa --input expected_platform=linux/amd64 --input expected_revision=abc123
opaal task inspect --project opaal.toml image --format json
accepted=$(opaal plan --project opaal.toml --task image --environment ci --input image=example:checked --input expected_id=sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa --input expected_platform=linux/amd64 --input expected_revision=abc123 --expires-in 900s --out image.plan.json --format digest)
opaal plan inspect image.plan.json
opaal task run --plan image.plan.json --accept "$accepted" --journal image.run.jsonl --receipt-out image.receipt.json --audit-out image.audit.json --require-true
opaal task explain --receipt image.receipt.json --audit image.audit.json --format json
```

Replace the image reference, full observed ID, platform and revision with the
intended image's actual facts. Repository references and config digests are
independent values. Custom absolute executable/socket paths use the same fixed
profile and no-symlink rules. Setup neither provisions Docker nor probes its
version; an independently supplied version string alone never qualifies live use.
macOS live setup refuses before creating files or starting a process.

Setup records executable bytes and creates four new empty reference directories
and the host-bound manifest/lock. It does not connect to Docker or qualify it.
Never point those directories at account state. The live task declares the exact
Docker tool effect with an `acknowledge-unenforced` grant. Daemon access can grant
broad host power; these command constraints provide no daemon sandbox or
read-only socket permission guarantee.

Check and plan observe local files without launching a tool or contacting the
daemon. Execution retains the locked executable and runs four fixed children:
client version, server/API version, one image inspection, server/API version.
Each uses explicit `--config` and `--host`, closed stdin, captured output, and
exactly `LC_ALL=C`, `TZ=UTC`, `DOCKER_API_VERSION=1.48`, plus `PATH`, `HOME` and
`TMPDIR` pointing at the bound empty reference directories. Nothing is inherited.
No account configuration, credential helpers, plugins, context discovery or
remote transport is selected. Each child has at most 30 seconds and the whole
acquisition at most 60 seconds, narrowed by the caller deadline and existing
output/process/work/retention limits.

Socket, directory, tool, source, lock, authority and input changes invalidate the
accepted plan. The socket must have a normalized bounded absolute ASCII path
with no symlink components. The four directories must remain empty and retain
their bound local identities before and after children. These observations do
not authenticate a daemon or exclude replacement-back races. macOS live
execution refuses; use saved-input checks there.

A failed child status rejects even complete-looking stdout. Missing image,
unavailable daemon, changed profile, malformed metadata, index/list or
missing Descriptor produce no Image, report or domain assessment. The reference
leaves acquisition errors uncaught. In a different ordinary task, an explicitly
caught recoverable Error followed by a valid Bool can complete; `--require-true`
does not make every handled Error permanently fatal. Cancellation, cleanup and
evidence failure retain their native precedence.

Journals store safe operation identity, status, byte counts and evidence digests,
not raw image responses, unknown labels or child diagnostics. Selected report
fields remain ordinary potentially sensitive data. To diagnose a failed run,
inspect its receipt and audit; an incomplete audit cannot prove success. Correct
the input and create a new plan and exclusive evidence paths for a manual rerun.
There is no replay, automatic retry, rollback or image mutation.
