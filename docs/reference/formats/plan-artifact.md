# Plan artifact

`opaal plan --project … --expires-in 900s --out PATH` exclusively writes a canonical `opaal.plan.v2` file beneath the project root and prints its exact digest. It refuses an existing target, symlink, non-regular target, or path outside that root. `opaal plan inspect PATH` validates and renders a bounded redacted view.

The top-level fields are exactly `schema`, `schema_version`, `created_at`, `expires_at`, `toolchain`, `platform`, `project`, `task`, `inputs`, `secrets`, `sources`, `authority`, `tools`, `observations`, `actions`, `outcome`, and `digest`. Version 2 uses canonical UTC timestamps. Expiry must be later than creation and no more than 900 seconds away.

The plan binds the check's project and source closure, typed inputs, authority and tool lock, complete child environment, platform and toolchain, TLS CA bytes, exact tool executables, and creation/expiry instants. Planning makes bounded reads and one wall-clock observation. It does not invoke the action, probe a tool, read a secret, or contact an endpoint. A process-bearing task on macOS can yield a valid `refused` plan; that artifact is inspectable but not executable.

`execute --accept` requires the complete plan digest byte for byte and revalidates every bound identity. Acceptance is request-local and is not stored as a separate artifact. A stale, expired, tampered, unknown-schema, or refused plan cannot run. See [lifecycle](../operational/lifecycle.md).
