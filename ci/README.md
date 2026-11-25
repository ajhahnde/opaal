# CI and local qualification

Run the commands from the repository root. The [development guide](../DEVELOPMENT.md)
covers building the workspace and choosing everyday checks.

| Check | Where to start |
| --- | --- |
| Product, repository and dependency policy | [Policy checks](#policy-checks) |
| Candidate program and fixture packaging | [Candidate archive replay](#replaying-a-candidate-archive) |
| Formatter, CLI and LSP behavior | [Source formatting](#source-formatting) |
| Host performance | [Benchmarks](#benchmarks) |
| Project execution and authority | [Operational core](#operational-core-qualification) |
| Random and stream behavior | [Random](#random-qualification) and [standard streams](#standard-stream-qualification) |
| Saved and live image checks | [Docker image checks](#docker-image-check-reference) |
| Native tools, project preparation and evidence | [Native tool qualification](controlled-native-tools.md) |
| Tag archives | [Release archives](../RELEASING.md#release-archives) |

## Policy checks

```sh
python3 ci/check_product.py source
python3 ci/check_public_boundary.py
```

The Security workflow also runs dependency checks with cargo-deny. With the
tool installed locally, run `cargo deny check`.

## Replaying a candidate archive

From a clean committed checkout, build the release workspace with
`cargo build --release --workspace --locked`. On the matching native host,
run the shared replay with its platform identifier and report directory.

```sh
bash ci/qualify_candidate_archives.sh linux-arm64 dist
```

Use `linux-x86_64`, `linux-arm64` or `macos-arm64`. Apply the macOS signing
step from the workflow before packaging on macOS.

## Source formatting

Use one canonical style for named functions and actions: the signature precedes
the body brace on its own line. Actions require a first static `effects { ... }`
prologue inside that body, with comma-separated requests and an optional final
comma. Formatting uses one request per line with a trailing comma. Empty effects
remain explicit. Control blocks, records and closures retain their existing
layout. Complete same-line declarations remain executable; formatting is an
explicit check or rewrite, rather than an execution requirement.

Give the formatter an explicit list of owned project sources:

```sh
target/debug/opaal format --check -- examples/numeric-report/report.opaal examples/numeric-report/tasks.opaal
target/debug/opaal format --write -- examples/numeric-report/report.opaal examples/numeric-report/tasks.opaal
```

Check is read-only and silent on success; noncanonical source reports FMT001.
Write preflights all operands and replaces each changed file atomically,
preserving mode bits. A later replacement failure can leave earlier files
rewritten: inspect and recheck the list. A rewrite changes content digests, so
regenerate project checks and plans before accepting and executing them.

The [source inventory](../tests/golden/source-formatting/inventory.json) classifies
every public source, Markdown fence, embedded source owner and fuzz seed.
Positive sources are canonical. Invalid, incomplete and deliberately legacy
fixtures retain their exact bytes and diagnostic assertions; semantic and
runtime negatives retain their owning corpus expectations. Do not run a blanket
write across fixtures. The inventory validator fails on unclassified or missing
sources and requires renewed classification when embedded owners change.

Editors can enable their own format-on-save using LSP document formatting. The
server emits the same bytes as CLI write, independently of tab/space options;
invalid and incomplete buffers receive no edit. Terminal and piped interactive
input continue incomplete signatures/bodies without rewriting the entered text.
Published 1.1 binaries cannot parse the new function layout; upgrade before
migrating source and keep source backups when downgrading.

Replay the formatter, framed language server and fresh/stale project workflows
outside the checkout using built working programs (select the current host):

```sh
python3 ci/qualify_source_formatting.py --binary-directory target/debug \
  --expected-source "$(git rev-parse HEAD)" --platform macos-arm64 \
  --report dist/source-formatting-working.json
```

This records program, fixture and working-source digests without claiming
committed archive qualification. The archive jobs on Linux x86_64, Linux ARM64
and macOS ARM64 run `ci/qualify_candidate_archives.sh` against the exact PR
head, or the event commit for other runs. They package the source-formatting
fixtures and replay them with the exact CLI/LSP archive, retaining both
archives and the reports. The operational core jobs test the PR merge commit.
The migrated fixture project requires OPAAL 1.10.0 or later and below
2.0.0. Release qualification uses its committed version requirement without
development adaptation.

## Benchmarks

```sh
python3 -m unittest discover -s ci/tests -p 'test_check_benchmarks.py'
python3 ci/check_benchmarks.py --contract-only
python3 benchmarks/run.py --profile smoke
python3 benchmarks/run.py --profile qualification \
  --budget-environment host-darwin-arm64 \
  --output benchmarks/evidence/host-darwin-arm64-candidate.json
```

The runner measures exactly eleven host cases, including the four operational
artifact cases, and invokes the checker before reporting success. Evidence is
bound to the candidate binary and matching host. Budget comparison is explicit
because a different host, including a shared CI runner, may validate the full
profile without claiming equivalence to the retained evidence host.

## Operational-core qualification

```sh
python3 ci/qualify_operational_core.py --profile qualification
```

This creates an isolated temporary project and proves the complete
non-publishing check, plan, accepted-execution, journal, and audit path on Linux.
On macOS arm64 it proves secret-free execution/audit plus the explicit refused
process boundary. The harness uses only synthetic inputs and a TLS loopback
server. See [Qualifying the operational core](../RELEASING.md) for the
scenario and evidence boundary.

## Random qualification

For macOS builds, apply the native image policy before running or packaging
the CLI and its host fixture. This uses an ad-hoc signature and no signing key:

```sh
cargo test --workspace --locked --no-run
codesign --force --sign - --options kill target/debug/opaal target/debug/opaal-standard-host-fixture
for binary in target/debug/deps/opaal-* target/debug/deps/opaal_standard_host_fixture-* target/debug/deps/random_native-* target/debug/deps/io_receipt_native-*; do
    [ -f "$binary" ] && [ -x "$binary" ] || continue
    codesign --force --sign - --options kill "$binary"
done
```

Apply the same option to `target/release/opaal` after release-mode builds. The
test compilation must finish before signing. Cargo restores visible programs
from the hashed copies even on a cached run. Compiling a different selection
of packages can replace those copies, so sign again after rebuilding. The
worker requires valid code with invalidation termination enabled; an ordinary
linker signature does not guarantee that policy on every supported macOS host.

```sh
cargo test -p opaal-platform-posix --test standard_host_worker --locked
cargo test -p opaal-runtime --test random_sampling --test random_authority --test random_limits --test random_controlled --test random_observers --test random_golden --locked
cargo test -p opaal-cli --test random_golden --test terminal_editor_pty --locked
python3 ci/qualify_random_values.py --binary-directory target/debug --expected-source "$(git rev-parse HEAD)" --platform macos-arm64 --report dist/random-working.json
python3 ci/qualify_random_failures.py --binary target/debug/opaal --output dist/random-failures
```

Use `linux-x86_64` or `linux-arm64` on the matching native Linux host. Working
mode records dirty-source identity and never claims committed archive evidence.
The committed host jobs package `tests/golden/random-values` with
`ci/package_random_values.py` and pass both the fixture and program archives to
the qualifier. It checks real source domains, retained interactive cells and
help, pure/domain refusal, controlled progress and metadata-only inspection,
and stale-source refusal outside the checkout. No version or publication is
performed. The [Random fixture guide](../tests/golden/random-values/README.md)
explains the current syntax floor and the independent Python reference.

A manual CI run on the candidate branch also runs `random-native-linux`.
It exercises the native host and CLI tests, real worker and maintained-probe
failures with failed journal persistence, and records maximum byte-fill timing
and process resources. Its artifact retains logs even after failure. This lane
complements the regular workspace, policy, fuzz and archive jobs; it does not
infer performance guarantees or replace missing failure scenarios.

## Standard-stream qualification

Use the same macOS image signing policy as Random after compiling the CLI and
native fixture. Exercise the processor, controlled receipt and shared native
protocol routes:

```sh
cargo test -p opaal-runtime --test stdio_values --test stdio_failure --test stdio_authority --test stdio_lifetime --test stdio_controlled --test stdio_observers --test stdio_golden --locked
cargo test -p opaal-platform-posix --test standard_host_worker --locked
cargo test -p opaal-cli --test io_golden --test io_receipt_native --test data_processing_golden --test terminal_editor_pty --locked
python3 ci/qualify_standard_input_output.py --binary-directory target/debug --expected-source "$(git rev-parse HEAD)" --platform macos-arm64 --report dist/stdio-working.json
python3 ci/qualify_standard_io_failures.py --binary target/debug/opaal --output dist/stdio-failures
```

Use `linux-x86_64` or `linux-arm64` on the matching Linux host. Working mode records an exact dirty-source snapshot
and binary hashes; it supplies no committed-package claim. All three candidate jobs
package the [self-contained references](../tests/golden/standard-input-output/README.md)
with `ci/package_standard_input_output.py` and replay those exact fixtures and
program archives outside the checkout. Copies of the maintained report core
and input corpus must match byte-for-byte before packaging. Qualification runs
the documented source and project CI drivers, all report policy/error fixtures,
and a complete producer output followed by exit 7 with zero processor launches.

The native fault harness retains receipts/journals/audits and validates blocked
read cancellation, worker death, exact reap and real file write limits. A file
size limit proves that tested write failure, without establishing disk-full or
quota support. Injected syscall/receipt stages are reported separately from
actual kernel failures. Hosted configuration alone is no passing evidence.
The `io_receipt_native` fixture independently limits the receipt's regular-file
write after the journal completes. File/parent sync tests substitute owned pipe
descriptors after the final receipt write and exercise the kernel sync error;
they do not establish regular-filesystem EIO or power-loss durability. Each
success/Error primary retains exact earlier output counts, empty administrative
streams and owned worker reap, while receipt failure makes the run unsuccessful.

## Docker image check reference

The [image check reference](../examples/image-check/README.md) shares one pure
policy and single-manifest decoder between saved data and controlled Linux
inspection. Saved-input use works on Linux x86_64, Linux ARM64 and macOS ARM64
without Docker.
Live inspection requires the exact client/server profile, explicit local socket,
locked executable and new empty reference directories. Account configuration,
remote transport and live macOS execution are excluded. The reference documents
authority limits, stale bindings, escaped reports and manual reruns.

Replay the working binary outside the checkout and retain exact case artifacts:

```sh
python3 ci/qualify_docker_image_checks.py --offline --binary-directory target/debug --expected-source "$(git rev-parse HEAD)" --platform macos-arm64 --report dist/docker-offline-working.json
```

Use `--platform linux-x86_64` or `--platform linux-arm64` on the matching Linux host. Working reports record dirty source
snapshots and cannot substitute for committed candidate evidence. After commit,
`ci/package_docker_image_checks.py` packages the exact reference; replay it with
`--archive` and `--fixtures` instead of `--binary-directory`. Example copies must
match the golden owner byte-for-byte. The offline qualifier asserts saved task
exits, exact reports, receipt/audit agreement, standalone parity, independent
policy modification and exclusive paths. It does not qualify a live daemon.

`docker_image_limits` fuzzes hostile saved JSON and compares successful decoder
results under tight work/retention budgets. Controlled acquisition fakes verify
the fixed vectors, sanitized environment, failed-status gating and target checks.

## Fuzzing

See the [fuzz targets and campaign commands](../fuzz/README.md).
