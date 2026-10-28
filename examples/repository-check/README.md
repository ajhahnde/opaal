# Repository checks with a pinned native tool

The action sources use one outer body with a first static effects prologue and
comma-separated requests. They require OPAAL 1.9.0 or later; when upgrading
from detached effects blocks, migrate the source and regenerate and explicitly
accept plans before executing it. Migrated projects require `>=1.9.0,<2.0.0`.

Requires OPAAL >=1.9.0,<2.0.0. The saved route works on macOS arm64, Linux
x86_64 and Linux AArch64; live native execution requires Linux x86_64 or AArch64. Supply an independently
qualified ShellCheck executable for the live route. Setup never installs,
discovers, downloads or launches a checker.

`policy.opaal` combines bounded ShellCheck JSON findings with TOML repository
policy. A clean script alone cannot pass an unacceptable repository policy.
All routes share this module. Ordinary pure tests need no tool or grant.
`source.opaal` reads saved input; `saved-tasks.opaal` exposes the same policy
through an accepted task. `tasks.opaal` acquires live findings.

The single JSON report contains only `passed`, `findings` and `manifest_ok`.
It omits raw messages, source, paths and payload. Expected stdout has no LF:

| Input fixture | Stdout | Standalone exit | Accepted task exit |
| --- | --- | --- | --- |
| pass.json | `{"findings":0,"manifest_ok":true,"passed":true}` | 0 | 0 |
| findings.json | `{"findings":1,"manifest_ok":true,"passed":false}` | 1 | 1 |
| negative-manifest.json | `{"findings":0,"manifest_ok":false,"passed":false}` | 1 | 1 |
| invalid.json / acquisition.json | empty | 1 | 2 |

Saved invalid/acquisition inputs throw `Saved repository input failed.`
The source uses fixed errors for acquisition, invalid checker output, invalid
TOML and invalid policy input. Accepted runs retain metadata-only error codes
and safe summaries; they omit thrown messages. Requested cancellation returns130.
A complete audit does not imply policy success. Report stdout and retained
receipt/audit evidence are separate.

Init requires an existing parent without symlink components and an absent
destination. Files are mode0600, directories mode0700 and writes exclusive.
Partial failures remain for inspection; retry with a fresh destination.
Offline rejects --tool. Live requires one absolute --tool and rejects Docker
and --qualified-version options. LC_ALL=C is explicit, with no inherited
environment. The lock pins executable bytes only: version and behavior remain
unverified. Doctor reports DOCTOR017 and readiness unknown without execution.

Native child effects are unenforced: a child may open other files, run helpers,
load libraries, read credentials, write files or use the network. The pin does
not identify loaders, libraries, helpers or executable-relative assets.
Sealed snapshot execution can change /proc/self/exe and $ORIGIN behavior.
There is no pathname fallback, confinement, hermeticity or rollback guarantee.
The original file must be executable and admitted by the host. Setup/header
inspection cannot prove runtime memfd, seal or exec availability.

OPAAL binds its input reads and compares script bytes before/after acquisition.
ShellCheck reopens its path: a replacement-and-restore race can escape detection.
This is not a child-input snapshot. Persistent changes refuse. Review a new
plan after changing pinned input, tool or authority. Native plans/evidence use
v5, inspection v2 and null versions; old readers refuse those forms. Existing
projects are not rewritten.

## Offline first use

Run from an existing parent directory. The accepted task declares stdin access;
its unseen streamed input bytes are not approved by the plan.

```sh
opaal project init --example repository-check --destination repository-offline --environment offline
cd repository-offline
opaal project doctor --project opaal.toml --task saved --environment offline --format json
opaal task list --project opaal.toml --format json
opaal test --list policy-tests.opaal --format json
opaal test policy-tests.opaal
opaal source.opaal < fixtures/pass.json
opaal source.opaal < fixtures/findings.json
opaal source.opaal < fixtures/negative-manifest.json
opaal source.opaal < fixtures/invalid.json
opaal source.opaal < fixtures/acquisition.json
accepted=$(opaal plan --project opaal.toml --task saved --environment offline --expires-in 900s --out saved.plan.json --format digest)
opaal plan inspect saved.plan.json
opaal task run --plan saved.plan.json --accept "$accepted" --journal saved.run.jsonl --receipt-out saved.receipt.json --audit-out saved.audit.json --require-true < fixtures/pass.json
opaal task explain --receipt saved.receipt.json --audit saved.audit.json --format json
```

## Live first use

Run separately on Linux x86_64 or AArch64 with the explicit independently qualified image.
The sample path is supplied by the operator, never created by OPAAL.

```sh
opaal project init --example repository-check --destination repository-live --environment ci --tool /opt/check-tools/shellcheck
cd repository-live
opaal project doctor --project opaal.toml --task repository --environment ci --input-file script=fixtures/check.sh --input-file manifest=fixtures/repository.toml --format json
opaal task inspect --project opaal.toml repository --format json
accepted=$(opaal plan --project opaal.toml --task repository --environment ci --input-file script=fixtures/check.sh --input-file manifest=fixtures/repository.toml --expires-in 900s --out repository.plan.json --format digest)
opaal plan inspect repository.plan.json
opaal task run --plan repository.plan.json --accept "$accepted" --journal repository.run.jsonl --receipt-out repository.receipt.json --audit-out repository.audit.json --require-true
opaal task explain --receipt repository.receipt.json --audit repository.audit.json --format json
```

Use fresh plan/evidence paths on every rerun. A second plan at an occupied path
refuses. Changed bound input or invalid acceptance returns2 before launching a
child; pre-admission refusal may have no receipt/audit.

```sh
printf '\n# changed after acceptance\n' >> fixtures/check.sh
opaal task run --plan repository.plan.json --accept "$accepted" --journal stale.run.jsonl --receipt-out stale.receipt.json --audit-out stale.audit.json --require-true
opaal task run --plan repository.plan.json --accept sha256:0000000000000000000000000000000000000000000000000000000000000000 --journal rejected.run.jsonl --receipt-out rejected.receipt.json --audit-out rejected.audit.json --require-true
```

## Interactive policy

Start `opaal` in the generated directory and use ordinary import/call/try/catch:

```text
import './policy.opaal' as policy
policy::checked(0, { comments: [] }, { repository: { license: "MIT", private: false } })
policy::checked(1, { comments: [{ code: 2086, level: "info" }] }, { repository: { license: "MIT", private: false } })
try { policy::checked(2, { comments: [] }, { repository: { license: "MIT", private: false } }) } catch error { error.message }
```

## Python comparison

The source reference bundle also includes `comparison.py` (Python >=3.11).
It is an independent comparison tool, separate from the generated project:

```sh
python3 comparison.py < fixtures/pass.json
python3 comparison.py --tool /opt/check-tools/shellcheck --script fixtures/check.sh --manifest fixtures/repository.toml
```

Live comparison uses the same explicit cwd, argv and environment, reads at most
64KiB per file and captures at most8MiB per stream under a600s deadline. It
combines findings with repository policy and emits the same bounded report.
Valid negative reports exit1; malformed input or failed acquisition exits2;
cancellation exits130. Its fixed errors never echo checker stderr or payloads.
It does not implement byte pinning, sealed snapshots, accepted plans or retained
OPAAL evidence. The source bundle's synthetic observer is a test fixture, not
ShellCheck or a substitute for qualifying the supplied executable.

OPAAL adds setup, explicit plan review/acceptance and fresh evidence paths on
each rerun. The Python route needs only its program and explicit tool/input
paths. OPAAL's additional work produces a bound plan and correlated execution,
assessment and cleanup evidence. Both routes must validate data and maintain
the repository policy. Neither establishes external adoption or a speed benefit.

To accept Apache-2.0 as well as MIT, change `manifest_ok` in `policy.opaal` and
add a pure named Apache case in `policy-tests.opaal`. Saved, interactive and
live callers continue importing that one policy; task and evidence orchestration
need no edit. Change the equivalent license comparison in Python separately.
Rerun pure tests and review a fresh plan after changing the policy.

For diagnosis, a valid negative report identifies findings or manifest policy.
Invalid saved input has no report; inspect the input separately through pure
policy calls. For accepted errors, use `task explain` and inspect the selected
input, lock and source separately. A complete journal or audit proves neither
policy success nor the content of a redacted error message. Stale bound input
requires a new reviewed plan, never automatic reacceptance.
