# Docker image checks

These references require OPAAL 1.4.0 or later. Saved-input checks need no
Docker executable, daemon, account configuration, or network connection.
`policy.opaal` is the shared pure policy for saved and live data.

In a fresh writable copy, select the lock for your host and the project manifest:

```sh
cp offline-tools-macos.toml offline-tools.toml
cp offline.toml opaal.toml
```

On Linux x86_64, use `offline-tools-linux.toml` instead. The CLI requires the
selected manifest to be named `opaal.toml`. Keep the fixture's normal
`>=1.4.0,<2.0.0` version requirement.

Run the check with explicit expectations and exclusive evidence paths:

```sh
opaal task inspect --project opaal.toml saved
accepted=$(opaal plan --project opaal.toml --task saved --environment offline --input expected_id=sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa --input expected_platform=linux/amd64 --input expected_revision=abc123 --expires-in 900s --out saved.plan.json --format digest)
opaal execute --plan saved.plan.json --accept "$accepted" --journal saved.run.jsonl --receipt-out saved.receipt.json --require-true < fixtures/image.json
opaal audit --project opaal.toml --journal saved.run.jsonl --out saved.audit.json
```

The policy compares the full observed image ID, platform and revision exactly.
It prints a JSON report and returns Bool. Exit 0 means the assessment passed
and execution, cleanup and evidence succeeded. Exit 1 means a completed negative
assessment; operational failures exit 2 and requested cancellation exits 130.
Use a fresh plan and evidence paths for each run.

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

Run the complete saved-input CI driver in a fresh copy:

```sh
sh run-saved.sh sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa linux/amd64 abc123 < fixtures/image.json
```

Change the last argument to `wrong` for exit 1. Supply `fixtures/invalid.json`
for exit 2, with no report or assessment. Missing revision differs from an empty
revision in the report's `missing` field. Every run needs fresh exclusive plan,
journal, receipt and audit paths. Existing evidence is never overwritten.

An independent Python implementation runs through the same shell interface:

```sh
sh run-comparison.sh sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa linux/amd64 abc123 < fixtures/image.json
```

It uses Python's standard library and independently validates the single-image
Descriptor, selected fields, duplicate keys and byte/depth limits before applying
the same three comparisons. Saved-input qualification asserts its report and
0/1/2 exits against all named OPAAL cases. It emits fixed input-failure diagnostics.
Its report has the same fields, including missing revision and escaped Unicode.
It has no accepted plan, journal, audit, receipt or owned-resource contract;
those native OPAAL features remain additional work for a plain script. Neither
implementation establishes image provenance or daemon confinement. This saved
comparison does not qualify live process cancellation or cleanup.

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

The fixed `docker-cli-profile.json` requires Docker client and engine 29.9.0,
API 1.48, Linux x86_64, and descriptor-bearing single-image responses. A version
string alone does not qualify a binary or server. Independently verify the exact
installed executable, source/profile and server before using this reference.
Use one already available image; inspection never pulls, builds or runs it.

Start from a fresh writable copy with no selected `opaal.toml`. Provision
`/usr/bin/docker` and the local `/run/docker.sock` independently. Supply the
verified CLI version as one LF-terminated line in `qualified-docker-version.txt`.
Supply `expected-image-ref.txt`, `expected-image-id.txt`, `expected-platform.txt`
and `expected-revision.txt`, each containing one explicit expectation line.
The ID must be the actual full observed Docker ID of the intended image;
repository references and config digests are independent values.

```sh
python3 setup-live.py
opaal task list --project opaal.toml
opaal task inspect --project opaal.toml image --format json
sh run-live.sh
opaal audit inspect image.audit.json
```

For an independently provisioned isolated daemon, supply its exact absolute
paths with `python3 setup-live.py --docker /absolute/path/docker --socket
/absolute/path/docker.sock` on one command line. The same profile and no-symlink
checks apply.

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
