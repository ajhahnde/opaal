# Developing OPAAL

Use the repository root for all commands. The pinned Rust toolchain is declared
in `rust-toolchain.toml`; Cargo uses the checked-in lockfile. Python validators
use only the standard library.

## Build and source gates

```sh
cargo fmt --all -- --check
cargo build --workspace --locked
cargo test --workspace --locked --no-fail-fast
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo doc --workspace --no-deps --locked
cargo deny check
python3 -m unittest discover -s ci/tests -p 'test_*.py'
python3 ci/check_product.py source
python3 ci/check_public_boundary.py
python3 ci/check_docs.py --cli target/debug/opaal
python3 ci/check_source_formatting.py --cli target/debug/opaal
git diff --check
```

The workspace has exactly six packages sharing the workspace version. A new
dependency, feature, crate, binary, environment protocol, source extension,
diagnostic namespace, or source identity is a public contract change.

## Focused checks

```sh
cargo test --locked -p opaal-syntax
cargo test --locked -p opaal-runtime
cargo test --locked -p opaal-cli
cargo test --locked -p opaal-lsp
```

Syntax tests own grammar, lexer, formatter, property, and source-diagnostic
behavior. Runtime tests own modules, types, operations, outcomes, streams,
resources, cancellation, and refusal. CLI tests exercise binaries, silent
success, status channels, no-write checking, atomic formatting, and input
negatives. LSP tests exercise framing, lifecycle, snapshots, queries,
cancellation, stale-result behavior, explicit project selection, disk-backed
module loading, editor-local overlays, and standalone fallbacks.

The workspace crates are unpublished source components. In 1.6, project-model
Rust callers use `ToolDeclaration::contract()` and `LockedTool::contract()` in
place of their former `adapter()` and `version()` getters. Match
`ToolContract::Maintained { adapter, version }` or `ToolContract::Native`, or use
the contract's optional adapter/version accessors; native tools have no version.
The downstream metadata/outcome defaults protected by
`embedding_api_compatibility` remain unchanged. Existing `OperationalAdapter`
implementations inherit snapshot refusal unless they explicitly implement it.

## Documentation example

```sh
target/debug/opaal format --check examples/language-foundation.opaal
target/debug/opaal check examples/language-foundation.opaal
target/debug/opaal examples/language-foundation.opaal
```

Pure non-interactive evaluation does not print its final value. Examples must not depend on authority that
the pure foundation does not grant.

## Pure policy tests

Export ordinary non-generic zero-argument Bool functions whose names start with
`test_`. List their signatures without initialization, or run them sequentially
in fresh module contexts:

```sh
target/debug/opaal test --list examples/image-check/policy-tests.opaal --format json
target/debug/opaal test examples/image-check/policy-tests.opaal
target/debug/opaal test examples/image-check/policy-tests.opaal --case test_negative_revision
```

Source checking and formatting use the ordinary language rules. Test admission
also requires proven-pure initializers and reachable calls; unresolved dynamic
calls refuse. Tests have no environment snapshot, process, filesystem, I/O or
random grant. A false Bool or uncaught ordinary Error fails a case and continues;
uncaught resource errors, refusal and infrastructure stop the suite. Caught
language Errors retain their existing semantics and consumed counters.
`opaal test --help` owns signature, view, budget and exit details.

## Source formatting

Use one canonical style for named functions and actions: the signature precedes
the body brace on its own line, and actions place `effects {` on a separate line
with semicolon-terminated requests. Control blocks, records and closures retain
their existing layout. Old same-line declarations remain executable; formatting
is an explicit check or rewrite, rather than an execution requirement.

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

The [source inventory](tests/golden/source-formatting/inventory.json) classifies
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
committed archive qualification. On a clean committed checkout, both host CI
jobs package the separate fixture bundle with `ci/package_source_formatting.py`
and replay it with the exact binary archive, retaining reports in `dist/`.
The fixture project requires OPAAL 1.2 or later. Release qualification uses
its committed version requirement without development adaptation.

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
server. See [Qualifying the operational core](RELEASING.md) for the
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

Use `linux-x86_64` for Linux qualification. Working mode records dirty-source
identity and never claims committed archive evidence. The exact committed
both-host candidate jobs package `tests/golden/random-values` with
`ci/package_random_values.py` and pass both the fixture and program archives to
the qualifier. It checks real source domains, retained interactive cells and
help, pure/domain refusal, controlled progress and metadata-only inspection,
and stale-source refusal outside the checkout. No version or publication is
performed. The [Random fixture guide](tests/golden/random-values/README.md)
explains the first-release floor and the independent Python reference.

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

Use `linux-x86_64` on Linux. Working mode records an exact dirty-source snapshot
and binary hashes; it supplies no committed-package claim. Both candidate jobs
package the [self-contained references](tests/golden/standard-input-output/README.md)
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

The [image check reference](examples/image-check/README.md) shares one pure
policy and single-manifest decoder between saved data and controlled Linux
inspection. Saved-input use works on Linux x86_64 and macOS arm64 without Docker.
Live inspection requires the exact client/server profile, explicit local socket,
locked executable and new empty reference directories. Account configuration,
remote transport and live macOS execution are excluded. The reference documents
authority limits, stale bindings, escaped reports and manual reruns.

Replay the working binary outside the checkout and retain exact case artifacts:

```sh
python3 ci/qualify_docker_image_checks.py --offline --binary-directory target/debug --expected-source "$(git rev-parse HEAD)" --platform macos-arm64 --report dist/docker-offline-working.json
```

Use `--platform linux-x86_64` on Linux. Working reports record dirty source
snapshots and cannot substitute for committed candidate evidence. After commit,
`ci/package_docker_image_checks.py` packages the exact reference; replay it with
`--archive` and `--fixtures` instead of `--binary-directory`. Example copies must
match the golden owner byte-for-byte. The offline qualifier asserts saved task
exits, exact reports, receipt/audit agreement, standalone parity, independent
policy modification and exclusive paths. It does not qualify a live daemon.

`docker_image_limits` fuzzes hostile saved JSON and compares successful decoder
results under tight work/retention budgets. Controlled acquisition fakes verify
the fixed vectors, sanitized environment, failed-status gating and target checks.

## Native tool contracts and reference qualification

A schema1 project tool may declare `adapter = "native"` and must omit
`version`. Its schema1 lock row contains exactly `id`, `adapter`, `path` and
`digest`; paths use the existing lossless Unix encoding. No probe, shell,
interpreter, installation, PATH discovery or version field is accepted. Every
declaration still needs one matching lock row and a complete explicit child
environment. Native-only locks can omit maintained-tool variables; mixed
Git/Cargo and native locks retain those requirements. Docker mixed projects
remain refused under the dedicated profile.

`std::process::run` consumes the same project ToolIdentity and literal
`List[String]`, returning Status and captured stdout/stderr Bytes. It requires
the exact tool's `process.run` grant with `acknowledge-unenforced`. A nonzero
or signalled child returns Status data; admission and execution faults throw.
Native argv includes the locked path as argv0, with at most4096 entries,
32KiB per entry and128KiB total including terminators. Capture remains8MiB per
stream, child stdin is null, cwd is project root, and one evaluation shares at
most8 attempts. No native version probe or retry runs before the task.

On Linux x86_64, runtime first checks original execute admission and rejects
unsupported ELF images, privilege-bearing files and unavailable execution
policy. It copies at most64MiB under the current deadline into a private
executable memfd, verifies immutability seals, then hashes and structurally
validates the sealed bytes before descriptor execution. The snapshot descriptor
is close-on-exec and does not leak into the child. Copy, hash and launch share
the preflight/execution deadline. Failure refuses without pathname or disk
fallback. macOS live controlled execution remains unsupported.

The digest identifies executable bytes, with no version, behavior, dependency
closure, confinement or rollback promise. Libraries, loaders, helpers and child
filesystem/network effects remain unenforced. Snapshot execution can change
`/proc/self/exe` and `$ORIGIN`. OPAAL validates its bound file reads; a child
reopening a pathname can observe replacement-and-restore races. The reference
documents this limit and never calls it a child-input snapshot.

Any native declaration selects closed check/plan/journal/audit v5 and native
project inspection v2, even for a pure selected task. Native versions are null.
The existing receipt v2 and explanation v1 remain in use. Maintained-only
artifacts retain their existing forms; old readers reject native forms. Native
project evidence is always metadata-only, including mixed-tool projects: raw
arguments, payloads, error messages and result digests are omitted. Deliberate
program stdout remains separate. Changes to bound source, inputs, lock or
authority require a fresh reviewed plan and explicit acceptance.

The [repository reference](examples/repository-check/README.md) provides first
use, failure routes, pure tests and a separate Python comparison. Its independent
corpus freezes exact safe reports, error classes and case inventories before
execution. Run the working candidate outside the checkout:

```sh
python3 ci/qualify_controlled_native_tools.py --cli target/debug/opaal --output /tmp/opaal-repository-reference
```

For a clean committed candidate, package the programs with
`ci/package_data_processing.py --platform macos-arm64` (or `linux-x86_64`) and
the reference with `ci/package_controlled_native_tools.py`. Replay the exact pair:

```sh
candidate_version=$(python3 -c 'from ci.package_release import VERSION; print(VERSION)')
python3 ci/qualify_controlled_native_tools.py \
  --archive "dist/opaal-v${candidate_version}-macos-arm64.tar.gz" \
  --fixtures "dist/opaal-v${candidate_version}-controlled-native-tools.tar.gz" \
  --expected-source "$(git rev-parse HEAD)" --platform macos-arm64 \
  --output /tmp/opaal-repository-package
```

Use `linux-x86_64` in both platform positions on Linux. Archive replay verifies
the shared source identity, version, platform, complete fixture inventory,
member hashes and modes before running the packaged CLI outside the checkout.
Both candidate CI jobs retain the archives and replay evidence. This saved
reference replay leaves native fault cases and real-tool qualification separate.

On Linux, add an explicit `--checker /absolute/path/to/shellcheck` for real
checker comparison. Compile `tests/golden/controlled-external-tools/checker.c`
with the host C compiler and pass its absolute path through `--fault-checker`
for synthetic status/JSON/TOML cases. The synthetic fixture never impersonates
ShellCheck. Both modes use fresh accepted plans and explanation, assert every
selected case and compare reports to frozen independent bytes. Five measured
repetitions follow one warmup; OPAAL timing includes plan/run/explain. RSS is a
cumulative child high-water observation, not an isolated per-run peak.

Results list observed, pending and other-host cases separately. To include the
structural image API observer, pass `--image-test-binary PATH`; for private
preparation faults and shared image-source checks, pass `--preparation-test-binary PATH`. Build these library
test executables with `cargo test -p opaal-runtime --lib --locked --offline
--no-run --message-format=json` and the same command for `opaal-cli`, respectively;
use the library compiler artifact's `executable` path. Without them private
preparation faults and macOS format/source checks remain pending; Linux still checks
formats through CLI preparation. The CLI library observer checks exact/first-excess
64MiB reads and symlink/FIFO/directory refusal through the shared source helpers;
these observations request no execution. These test executables are not package contents.
For closed check-artifact readers, receipt byte limits and maintained v4
roundtrips, pass `--evidence-test-binary PATH`. Build it with `cargo test -p
opaal-runtime --test workflow_artifacts --locked --offline --no-run
--message-format=json` and use that test target's `executable` path. Missing this
binary leaves its seven explicitly identified cases pending. CLI journal
mutations rebuild valid digest links before exercising lifecycle refusals;
a complete synthetic process journal is the positive control, not a launch
observation. Metadata descriptor counts and forbidden argument, digest and
outcome fields also use this journal control with freshly sealed mutations.
Stale acceptance and passive checks use a directly tested launch
canary with an unused native tool; they do not measure execution syscalls.
Audit native-version mutations reject forbidden root version fields: audit v5
has no root tool collection. These cases check its closed shape, not check/plan
tool-version validation.
The library executable supplied by `--preparation-test-binary` also observes
twenty-six evidence rows after injected success, error and cancellation primaries.
Reserved files become unwritable at each event, terminal, receipt or audit
boundary; a failing resource callback and partial summary writer cover finish
selection. These tests run the persistence and exit code, without evaluating a
task or launching a child; they do not establish cancellation timing.
The runtime `controlled_native_tools` suite separately evaluates native actions
with a fake adapter and clock. It covers requested cancellation and deadline
expiry before acquisition, during acquisition, at launch and after effect
journaling, combined with journal and resource-cleanup faults. A successful
effect retains completed evidence before checking cancellation again; an earlier
journal failure keeps precedence. These evaluator tests also check the eighth
and ninth process calls and preserve nonzero child statuses as data. They do not
measure OS process timing or reaping.
The runtime library executable supplied by `--image-test-binary` also observes
three journal-bound rows. It injects counters to isolate the 100000-line ceiling
and terminal line/byte reserves; real v5 journals reach the byte ceiling first.
For the passive LSP row, pass `--lsp-test-binary PATH`, built with `cargo test -p
opaal-lsp --test server_e2e --locked --offline --no-run --message-format=json`.
The framed candidate server handles diagnostics, hover, completion, signature
and definition requests with directly tested executable marker controls.
Missing these executables leaves their evidence rows pending.
For accepted CLI redaction and post-acceptance bound-input reads, pass
`--cli-test-binary PATH`. Build it with `cargo test -p opaal-cli --test
controlled_native_tools --locked --offline --no-run --message-format=json`.
The executable emits thirteen Linux or six macOS inventory records; its CLI
image digest must match `--cli`. Missing it leaves these cases pending.
The native redaction observer uses a synthetic compiled C tool; mixed-tool
cases include a maintained version-probe double. Input mutations wait for an
accepted task's ready marker before changing bytes, replacing the file, or
substituting a symlink. Same-byte replacement is allowed by the byte-only
contract. These observations do not measure native syscalls or confinement.
For native request limits, authority, attempt counting and the remaining child
deadline, pass `--process-test-binary PATH`. Build it with `cargo test -p
opaal-runtime --test controlled_native_tools --locked --offline --no-run
--message-format=json`. Its thirty-two Linux or fifteen macOS image-inventory
records exercise request bounds, authority, attempts and the shared deadline.
The ordinary request cases use fake snapshot/process adapters. Pure calls,
module initializers and default embedding refuse without an operational host.
Eleven Linux acquisition cases use real POSIX snapshots: requested cancellation
and fake-clock deadline expiry during copy, sealed hashing, ELF validation and
before launch, plus stale originals, preseal mutation and sealed-read failure.
The fixed two-chunk, one-program-header image selects these poll boundaries;
Linux requires `strace` on PATH to verify the actual sealed read/seal census
and absence of child execution. Parent snapshot descriptors close on refusal.
One maintained probe consumes the same eight-attempt budget as native calls.
Two separate Linux observations use the real POSIX adapter for a missing working
directory and a missing ELF loader; syscall traces check failed descriptor launch
and absence of pathname fallback. The exit records map runtime cancellation/refusal
to task statuses; they do not inject CLI cancellation. Missing the binary leaves
these cases pending. It is not a package member.
For sealed Linux execution, source mutation, descriptor cleanup, original mode
refusals, image growth and real output/lifetime boundaries, pass
`--posix-test-binary PATH`. Build it with `cargo test -p opaal-platform-posix
--test controlled_native_tools --locked --offline --no-run --message-format=json`.
It emits thirty-three Linux or one macOS inventory records. Linux additionally
requires `strace` on PATH: the qualifier retains its execution trace, checks all
seventeen descriptor launches and rejects pathname fallback attempts. The
compiled C observers check actual output, argv/environment/cwd, inherited
snapshot descriptors and owned child processes. Their exact compiled image
digests are retained after comparing snapshot bytes with the original image.
Requested/timeout records here observe adapter errors mapped to task exit
statuses; they do not inject CLI
cancellation. macOS checks snapshot refusal without launching a child. Missing
the binary leaves these cases pending; it is not packaged. Nine snapshot fault
cases attach `strace` after test startup and verify the sole test thread is traced,
then inject one effective-access, capability-query, snapshot-create/mode,
copy-read/write or seal fault. Each checks the exact injected syscall, zero
process creation/execution attempts and no remaining snapshot handle. The Linux
host must permit same-user tracing of these test processes. Two Linux CLI finish
cases compile `finish.c`, inject EIO into the supervisor's first owned-group
existence check, and verify real descriptor execution, child reaping, partial
evidence and error/requested-cancellation precedence. The cancellation child
requests SIGINT with `tgkill`, leaving `kill` exclusive to the supervisor's
injected fault. These working-corpus observers do not replace complete finish
and evidence qualification. Three original
permission cases require `setfacl`, `setcap`, `unshare` and `mount`: ACL denial
runs as the ordinary user; capability metadata uses an isolated user namespace;
noexec uses a private tmpfs mount in isolated user/mount namespaces. They create
no host mount and require no system privileges. Positive snapshot controls
before/after removing the ACL/capability distinguish those refusals from an
unavailable snapshot route.
Linux qualification requires a host C compiler for its fixed observers. Child
input cases demonstrate persistent-change refusal and a successful change-and-
restore counterexample: matching before/after bytes do not prove child input.

`ci/package_controlled_native_tools.py` uses `package_examples` to package exact
reference/corpus files from a clean committed checkout. Copy assertions and
archive roundtrips preserve all input bytes. Working-corpus results do not
substitute for complete native fault, package, host or candidate qualification.

## Project preparation and diagnosis

`project init --example image-check --destination PATH --environment offline`
uses the compiled reference assets and host lock without installation/cwd lookup.
Live Linux setup additionally requires explicit `--docker`, `--socket` and
`--qualified-version 29.9.0`; these are independently qualified caller facts.
Generated projects require OPAAL1.5 or later in the existing compatibility field.
Existing controls are neither rewritten nor upgraded. The old setup script is
retained only in `tests/fixtures/image-check-legacy/` for comparison.

`project init --example repository-check --destination PATH --environment offline`
prepares the fixed saved policy without tools on both qualified hosts. Its
Linux x86_64 `ci` form requires exactly one absolute `--tool` ELF executable;
Docker flags and `--qualified-version` refuse. It hashes and structurally
inspects the original executable without launching it or creating a snapshot.
Generated repository controls require >=1.6.0,<2.0.0 and declare only LC_ALL=C
with `inherit=[]`. Existing image-check controls retain their1.5 minimum.

Setup validates controls/facts before creating an absent directory, then uses
descriptor-relative exclusive no-follow writes and syncs. Its fixed inventory
is bounded to64 files/16MiB. Injected write/sync and concurrent-collision tests
assert retained partial files and safe refusal on retry; no cleanup path unlinks
them. Existing directories, files, FIFOs and dangling symlinks remain unchanged.

`project doctor` requires explicit project, task and environment selections and
the existing input binding forms. It uses the shared cancellable source analysis,
scalar validation and declarative authority checks plus bounded local identity
reads. No execution host, initializer, secret, stdin or daemon is consulted.
Text and closed schema_version1 JSON are bounded to128 findings/64KiB. Root keys
are `schema`, `schema_version`, `project_id`, `task_id`, `environment`,
`local_readiness`, `live_readiness`, `findings`. The schema name is
`opaal.project-doctor.v1`. Local readiness is ready/blocked/unknown; live readiness
is unknown for ci and not-applicable otherwise. Every finding has `code`,
`category`, `severity`, `input_name`, `message`, `source`, `span`. Severity is
error/unknown; category is version/document/task/input/authority/tool/host/resource.
Source and span are currently null; input_name is a validated identifier or null.
No raw diagnostics, source lines, input values or executable bytes are rendered.

| Code | Fixed meaning |
| --- | --- |
| DOCTOR001 | Unavailable, malformed or unsupported selected document/source |
| DOCTOR002 | Dependent task checks unknown |
| DOCTOR003 | Analysis/input/view resource refusal (diagnostic, exit2) |
| DOCTOR004 | Invalid selection/input names (diagnostic, exit2) |
| DOCTOR005 | Environment not declared |
| DOCTOR006 | Tool lock host mismatch |
| DOCTOR007 | Executable unavailable or changed |
| DOCTOR008 | Unsupported live host |
| DOCTOR009 | Unavailable/invalid socket or empty directories |
| DOCTOR010 | Incompatible OPAAL version |
| DOCTOR011 | Task unavailable or unsupported |
| DOCTOR012 | Missing or incompatible exact authority grant |
| DOCTOR013 | Input file unavailable or beyond read bound |
| DOCTOR014 | Missing or incompatible typed inputs |
| DOCTOR015 | Undeclared input name |
| DOCTOR016 | Unsupported secret cardinality |
| DOCTOR017 | Native bytes observed; version, behavior and live readiness unknown |

Both source and packaged qualification assert native setup/doctor outside the
checkout. Linux live setup canaries assert no executable launch/socket connection;
macOS live setup refuses. Real Docker qualification retains its separate fixed
profile gate. Local readiness confers no plan acceptance or health guarantee.

## Static task discovery and plan digests

Inspect the checked-in job-check project with the built client:

```sh
target/debug/opaal task list --project tests/golden/standard-input-output/ci-job-check/opaal.toml --format json
target/debug/opaal task inspect --project tests/golden/standard-input-output/ci-job-check/opaal.toml sample --format json
```

Maintained-only projects use the closed `opaal.project-inspection.v1` schema,
version1. Any declared native tool selects `opaal.project-inspection.v2`, version2,
with a null native version, including when a selected pure task uses no tool.
Both emit one JSON object followed by LF. Listing sorts tasks by qualified ID;
inspection selects one task from the same view. Parameters retain declaration
order and show exact types and supported `value`/`file` bindings. An unsupported
parameter has an empty binding list and remains refused by project checking.
Effects, tool requirements, and environments come from static project metadata;
discovery never executes initializers or tools. The manifest path uses the
lossless native-path encoding. Omit `--format json` for escaped human text.

For project planning, `--format digest` prints exactly `sha256:HEX` followed by
LF after successfully persisting an executable plan to its exclusive output
path. Refused plans and persistence failures return nonzero and emit no digest.
Supply that digest explicitly to `execute --accept`; displaying it grants no
authority. Other plan output retains its human form.

New check and plan artifacts use v4. The project record includes a sorted
`targets` array (empty for Docker-free projects). Docker target records bind
the compiled profile digest, lossless native socket/config paths and the
device, inode, mode and owner of the socket and four empty safe directories.
Planning observes local files without connecting to Docker. Execution checks
these facts again before and after each fixed child; it does not authenticate
a daemon or exclude replacement-back races. Legacy v2/v3 check/plan readers
retain their closed schemas and reject target fields.

Native CI selection uses `execute --require-true` with mandatory `--receipt-out`.
The task must statically and dynamically return Bool. After owned cleanup and
evidence persistence, true exits 0, false exits 1, operational/type/evidence
failure exits 2, requested cancellation exits 130, and timeout exits 2.
Ordinary execute keeps its previous exit policy. Printed bytes never select an
assessment. Journal/audit v4 and receipt v2 record policy, nullable assessment,
and typed cancellation reason separately from primary execution; incomplete
audits cannot establish a pass. Legacy v2/v3 artifacts and receipt v1 remain
explicit read formats, with no automatic rewriting or replay.

```sh
cargo test -p opaal-cli --test native_ci_outcomes --locked
cargo test -p opaal-runtime --test workflow_contract --test workflow_artifacts --locked
```

The existing receipt fault tests cover selected candidates and cancellation
through write/partial-write/flush/sync failures. The native I/O fault qualifier
now checks selected exits, including cancellation with unavailable journal
evidence. The [job-check reference](tests/golden/standard-input-output/ci-job-check/README.md)
uses the native command sequence and keeps Python only as the external input
producer and independent observer. OPAAL 1.3.0 rejects the new option safely.

## Fuzzing

Install cargo-fuzz and a nightly Rust toolchain, then run:

```sh
fuzz/run-smoke.sh
fuzz/run-smoke.sh 10000
fuzz/run-campaign.sh 600
```

The scripts seed lexer, parser, expander, and resource targets from current
OPAAL corpora. Writable corpora and artifacts stay in temporary or ignored
directories. Reproduce and minimize failures, then retain a focused regression.

## Change discipline

Keep source, fixtures, binaries, diagnostics, docs, and protocol names coherent.
Do not use a host pass to claim external image, target, Redox, or hardware
qualification.

Every package is non-publishable. The manual release-policy workflow performs
only read-only validation. Maintainer pull requests must pass the stable `required` and
`security-required` aggregates; third-party actions are pinned to full commit
identifiers.

[← Repository overview](README.md) · [Product documentation](https://opaal-lang.org/docs/)
