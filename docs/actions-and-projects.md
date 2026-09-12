# Actions and explicit projects

OPAAL actions are typed callables with a closed, statically declared effect
set. A project exports selected actions as tasks and binds their effect scopes
to named project metadata. The explicit project lifecycle formats, analyzes,
inspects, checks, plans, accepts, executes, journals, and audits those same
contracts. Check and plan remain non-executing; only execution of one exact
accepted plan receives an operational host.

## Actions and tasks

An action requires typed parameters, an explicit result, an `effects` block,
and a body:

```opaal
import project::context as project
import project::tools as tools

action prepare(candidate: String) -> String
effects {
    filesystem.read(project::root);
    process.run(tools::git);
}
{
    return candidate
}

task release = prepare
```

The supported capabilities are `filesystem.read`, `filesystem.write`,
`process.run`, `network.http`, `secret.reveal`, `clock.wall`, and
`clock.monotonic`. Effect arguments are static literals or qualified project
identities; an exported task's scoped effects require explicitly imported
project identities. Duplicate non-secret requests collapse to one semantic
request while repeated secret reveals remain visible to the unsupported-
cardinality check; formatting preserves source order. Actions can call
functions and statically named actions. The action
graph must be acyclic, no deeper than 64 calls, and contain no more than 1,024
actions; every caller must declare each request made by its callees. Functions
cannot call actions, and actions are not first-class values.

A `task` is legal only in the manifest's root module. It exports exactly one
action and derives that action's signature and effects without overrides.
Standalone source cannot use `project::*` imports or task declarations.

## Manifest

Every project operation requires the exact path to a file named `opaal.toml`.
OPAAL never searches parents or activates a project implicitly. Schema 1 has a
closed top level:

```toml
schema_version = 1

[project]
name = "demo"
root_module = "tasks.opaal"
required_opaal = ">=1.0.0-alpha.1,<2.0.0"

[paths]
root = "."
evidence = "evidence.json"

[tools.git]
adapter = "git"
version = ">=2.39.0,<3.0.0"

[environments.ci]
authority = "authority-ci.toml"
tool_lock = "tools-host.toml"
```

Tool adapters are limited to `git` and `cargo`. Endpoint declarations name a
valid HTTP or HTTPS URL, a nonempty unique list of uppercase methods, an
ordered unique list of lowercase secret-header names, and `tls`. TLS endpoints
also require a lowercase DNS `tls_server_name` and project-local `ca` file.
Non-TLS endpoints must use a literal IPv4 or IPv6 loopback address. Secrets use
only `kind = "injected"`. Environments name one project-local authority file
and tool lock.

All manifest paths are relative, lexically contained by the project root, and
opened from retained directory descriptors one component at a time with
no-follow semantics. A symlink in the manifest path, root, source closure,
authority path, or tool-lock path is never followed.

## Authority and tool locks

An authority document contains only its schema version, matching project and
environment names, and exact grant or deny rows:

```toml
schema_version = 1
project = "demo"
environment = "ci"

[[rules]]
decision = "grant"
effect = "filesystem.read"
scope = "project.root"
required_enforcement = "enforced"

[[rules]]
decision = "grant"
effect = "process.run"
scope = "tool.git"
required_enforcement = "acknowledge-unenforced"
```

A deny row has no enforcement field. Rules admit no wildcard, prefix,
inheritance, or default. Duplicate rows and mismatched effect/scope families
refuse the complete document. Maintained process execution is the only scope
that may acknowledge an unenforced child boundary.

The host lock contains a matching identity and platform, one closed
`[child_environment]` table with `inherit = []`, optional explicit variables,
and exactly one `[[tools]]` row for every declared tool. Native paths and values
use canonical unpadded base64url tagged with `platform = "unix"`; tool versions
are canonical SemVer and digests are `sha256:` plus 64 lowercase hexadecimal
digits. Tool paths must decode to absolute Unix paths without NUL, and child
environment values cannot contain NUL. The checker does not read an executable
or run a version probe.

Each declarative file is UTF-8 TOML no larger than 1 MiB, nests no deeper than
16 aggregate tables/arrays, and accepts at most 256 collection entries. A task
check accepts at most 64 unique inputs across `--input` and `--input-file`.
`--input name=value` parses only the declared lexical value and performs no
filesystem access; a `Path` supplied this way need not exist. Scalar values use
UTF-8 text for `String`, native path bytes for `Path`, `true` or `false` for
`Bool`, decimal text for `Int` and finite `Float`, `null`, canonical unpadded
base64url for `Bytes`, and integer `ns` or `b` suffixes for `Duration` and
`ByteSize`. Types without an
exact `name=value` representation cannot be exported as task parameters and are
refused while the project is loaded. `--input-file name=PATH` is available only
for a declared `Path` parameter: it opens one existing bounded regular file
under the project root without following symlinks and records its native path,
size, and content digest for execution-time stale revalidation.

One check, plan, execute, or audit invocation also shares a 192 MiB aggregate
control-read budget across every control input it reads, including the
manifest, source closure, file-snapshot inputs, authority and tool-lock files, TLS
CA material, tool executables, and accepted or audited artifacts. The exact
limit is admitted; the first byte beyond it refuses the invocation. The
smaller per-file and per-family limits still apply independently.

## CLI boundary

Inspect a task without selecting authority:

```sh
opaal task inspect --project opaal.toml release
```

Check the complete selected declaration set without execution:

```sh
opaal check --project opaal.toml --task release --environment ci \
  --input-file candidate=artifact.tar
```

The selected environment determines the exact authority and tool-lock files;
they cannot be restated at the CLI. Success is silent by default; adding
`--format json` writes one canonical `opaal.check.v2` artifact to standard
output. It records each input's explicit `value` or `file` binding and the
statically reachable secret requirements. A missing or
explicitly denied request, malformed document, mismatched identity, invalid
input, unknown task, or unsupported target capability exits nonzero. A task
declaring `process.run` against a macOS tool lock receives a `CHECK008` finding
and an `unsupported` request verdict because exact executable identity cannot
be preserved through process creation. An unrecognized target receives
`CHECK009` and an `unknown` verdict. Project check never invokes the task, reads
an executable, probes a tool, reads a secret, or calls an adapter.

Create an expiring review artifact for the same checked task:

```sh
opaal plan --project opaal.toml --task release --environment ci \
  --input-file candidate=artifact.tar --expires-in 900s \
  --out release.plan.json
opaal plan inspect release.plan.json
```

The plan binds the canonical check, project and source closure, typed inputs,
authority and tool lock, child environment, platform and toolchain, TLS CA
material, tool executables, and its creation and expiry instants. Planning
performs bounded reads and one wall-clock observation, but does not probe a
tool, invoke the action, read a secret, or contact an endpoint. It writes the
canonical `opaal.plan.v2` file only when the project-contained destination does
not already exist, then prints its exact digest. A process-bearing plan on
macOS is written with a `refused` outcome and cannot be executed.

The artifact schema also retains `unknown` for an adapter that cannot establish
its enforcement verdict. Such a request always makes the action and plan
non-executable. On the currently supported Linux and macOS hosts, the
maintained POSIX project adapters report only the granted, denied, or
unsupported verdicts. An unrecognized target remains `unknown`; artifact
readers preserve and fail closed on that verdict rather than treating it as
malformed or executable.

After reviewing that artifact, copy its complete digest into one execution
request:

```sh
opaal execute --plan release.plan.json \
  --accept sha256:EXACT_PLAN_DIGEST \
  --secret-stdin readiness_token \
  --journal release.run.jsonl < secret-input
```

`--accept` must equal the plan's digest byte for byte and applies only to this
request. Execution derives the environment, authority, tool lock, and input
identity from that validated plan; attempts to restate them are CLI misuse.
`--run-id` is optional. When omitted, OPAAL generates 128 bits with the operating
system CSPRNG and renders 32 lowercase hexadecimal digits. A supplied or
generated run ID is a correlation label local to the exact journal target. Exclusive journal creation
prevents reuse at the same target, while a different journal may use the same
label; correlate artifacts by journal identity plus run ID. Secret input must
come from non-terminal standard input and name the plan's exact one required
secret. Secret-free plans reject `--secret-stdin` and read no stdin. Tasks with
multiple secret IDs, repeated reveal, or multiple endpoint/header sinks produce
a valid but refused check and plan before acceptance; they never read stdin.
One-secret input is read only after static identities and maintained tool probes
have been revalidated. Execution rejects an expired plan or any drift in its
project root, manifest, source, input, authority, tool lock, child environment,
TLS material, executable, platform, or toolchain identities.

Accepted execution of a process-bearing task is currently Linux-only. On
macOS, refusal occurs during check and planning, before journal creation,
secret consumption, a tool probe, or a pathname process spawn. Inspection,
check artifacts, refused plans, and audit remain available there.

Version-1 check and plan artifacts, future schemas, and unknown fields are
rejected without execution or conversion. Regenerate them with the current
toolchain; OPAAL never rewrites a persisted development artifact in place.

The journal destination is project-contained, mode `0600`, and never
overwritten. Its initial header is written and synced in an exclusive sibling,
atomically published to the exact absent target without replacement, and
followed by a parent-directory sync. Later canonical `opaal.run-journal.v2`
lines are synced in hash-chain order: action boundaries, paired effect
boundaries, cleanup, and one terminal record. A secret-bearing HTTP request
nests its own paired `secret.reveal` records inside the surrounding
`network.http` records; journal payloads contain only the secret identity and
sink scope, never secret bytes.
Every paired effect also carries one closed operation descriptor. Filesystem
paths are project-root-relative; source-derived process arguments are lengths
and SHA-256 digests; HTTP bodies, header values, secrets, environment values,
observed clock values, and absolute project roots are never descriptors.
Completed and partial external facts remain evidence even when the action later
fails, and no effect is retried implicitly.

Validate and project that evidence without granting authority or resuming work:

```sh
opaal audit --project opaal.toml --journal release.run.jsonl \
  --out release.audit.json
opaal audit inspect release.audit.json
```

Audit checks the closed journal schema, contiguous sequence, digests, hash
chain, and terminal state, then exclusively publishes `opaal.audit.v2` under
the same explicit project root. A valid complete-line prefix with no terminal
record becomes an `incomplete` audit and exits 1; earlier parse, schema,
sequence, or hash corruption refuses without publishing success.
The audit records the journal header digest, last validated complete-line
digest, inclusive validated line count, start time, nullable finish time, and
nullable terminal digest. Inspection validates the audit and its own digest;
only audit generation revalidates the source journal bytes.

All plan, journal, and audit outputs refuse pre-existing files, symlinks,
non-regular paths, and paths outside the explicit project root. Ordinary
`opaal SCRIPT`, interactive evaluation, and `opaal plan SOURCE` still refuse
effectful source before host access.

[← Documentation index](README.md) · [Editor and project analysis](editor.md) · [Language foundation](language-foundation.md) · [Authority and resources](authority-and-resources.md) · [Bounded operational modules](operational-modules.md)
