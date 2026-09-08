# Actions and explicit projects

OPAAL actions are typed callables with a closed, statically declared effect
set. A project exports selected actions as tasks and binds their effect scopes
to named project metadata. The current surface formats, analyzes, inspects, and
checks these contracts; it does not execute an effect, probe a tool, contact a
network endpoint, read ambient environment, or materialize a secret.

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
project identities. Duplicate requests collapse to one semantic request while
formatting preserves source order. Actions can call functions and statically named actions. The action
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
check accepts at most 64 unique inputs. Scalar values use type-directed text:
plain text for `String` and `Path`, `true` or `false` for `Bool`, decimal text
for `Int` and finite `Float`, `null`, canonical unpadded base64url for `Bytes`,
and integer `ns` or `b` suffixes for `Duration` and `ByteSize`. Types without an
exact `name=value` representation cannot be exported as task parameters and are
refused while the project is loaded.

## CLI boundary

Inspect a task without selecting authority:

```sh
opaal task inspect --project opaal.toml release
```

Check the complete selected declaration set without execution:

```sh
opaal check --project opaal.toml --task release --environment ci \
  --authority authority-ci.toml --tools tools-host.toml \
  --input candidate=artifact.tar
```

The supplied authority and tool paths must be the selected environment's exact
normalized files. Success is silent. The complete `opaal.check.v1` machine
artifact belongs to the later reviewable-planning lifecycle and is not emitted
by this check-only surface. A missing or explicitly denied request, malformed
document, mismatched identity, invalid input, or unknown task exits nonzero.
Project check never invokes the task or an adapter. Maintained bounded adapters
now exist for embedders and direct host/fake verification, but effectful source
execution remains unavailable.

[← Documentation index](README.md) · [Language foundation](language-foundation.md) · [Authority and resources](authority-and-resources.md) · [Bounded operational modules](operational-modules.md)
