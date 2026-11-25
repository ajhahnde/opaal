# Native tool qualification

Run the commands from the repository root. Start with the
[CI and local qualification guide](README.md) for the platform jobs and
the [repository check example](../examples/repository-check/README.md) for
the user-facing workflow.

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

On Linux x86_64 and GNU/Linux ARM64, runtime first checks original execute admission and rejects
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

The [repository reference](../examples/repository-check/README.md) provides first
use, failure routes, pure tests and a separate Python comparison. Its independent
corpus freezes exact safe reports, error classes and case inventories before
execution. Run the working candidate outside the checkout:

```sh
python3 ci/qualify_controlled_native_tools.py --cli target/debug/opaal --output /tmp/opaal-repository-reference
```

For a clean committed candidate, package the programs with
`ci/package_data_processing.py --platform macos-arm64`, `linux-x86_64` or
`linux-arm64` and
the reference with `ci/package_controlled_native_tools.py`. Replay the exact pair:

```sh
candidate_version=$(python3 -c 'from ci.package_release import VERSION; print(VERSION)')
python3 ci/qualify_controlled_native_tools.py \
  --archive "dist/opaal-v${candidate_version}-macos-arm64.tar.gz" \
  --fixtures "dist/opaal-v${candidate_version}-controlled-native-tools.tar.gz" \
  --expected-source "$(git rev-parse HEAD)" --platform macos-arm64 \
  --output /tmp/opaal-repository-package
```

Use the matching `linux-x86_64` or `linux-arm64` value in both platform
positions on Linux. Archive replay verifies
the shared source identity, version, platform, complete fixture inventory,
member hashes and modes before running the packaged CLI outside the checkout.
All three candidate CI jobs retain the archives and replay evidence. This saved
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
Generated projects require OPAAL1.9 or later and below2.0 in the existing compatibility field.
Existing controls are neither rewritten nor upgraded. The old setup script is
retained only in `tests/fixtures/image-check-legacy/` for comparison.

`project init --example repository-check --destination PATH --environment offline`
prepares the fixed saved policy without tools on all qualified hosts. Its
Linux x86_64 and Linux ARM64 `ci` forms require exactly one absolute `--tool` ELF executable;
Docker flags and `--qualified-version` refuse. It hashes and structurally
inspects the original executable without launching it or creating a snapshot.
Generated repository controls require >=1.9.0,<2.0.0 and declare only LC_ALL=C
with `inherit=[]`. Generated image-check controls require OPAAL1.9 or later and below2.0.

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
evidence. The [job-check reference](../tests/golden/standard-input-output/ci-job-check/README.md)
uses the native command sequence and keeps Python only as the external input
producer and independent observer. OPAAL 1.3.0 rejects the new option safely.
