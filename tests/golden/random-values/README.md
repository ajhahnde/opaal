# Bounded Random fixtures

These fixtures require the Random API planned for OPAAL 1.3.0. Released 1.2.0
binaries do not provide it. Qualification of an unreleased build adjusts only
the copied project manifest to that build's version.

`source.opaal` samples a shard in [0,4), a finite 53-bit fraction in [0,1), a
16-byte identifier and empty Bytes, catches an invalid interval and prints the
numeric results. It requires `printf` on PATH. `tasks.opaal` performs the same
sampling in a typed action under a selected `entropy.system` grant. Imports and
effect declarations do not grant authority. `int(7,8)` and `bytes(0)` still need
a grant and a qualified host, although they draw no entropy.

`int(min,max)` includes min and excludes max, including extreme Int bounds.
`float()` returns k/2^53 for an integer k from zero through 2^53-1.
`bytes(count)` returns exactly count bytes; negative counts and counts over
1,048,576 fail with `RANDOM001`. Empty intervals also fail with `RANDOM001`.
The evaluation admits at most 8 MiB of entropy, intersects the caller's work
and retained-byte limits, and tries at most 128 integer candidates. Native
fills admit at most 256 bytes under one original operation deadline of at most
30 seconds. Errors, resource exhaustion and cancellation return no partial
value or fallback. Entropy is seedless; no generator state or distribution API
is provided, and this example makes no cryptographic key-generation promise.

`interval.opaal`, `count.opaal` and `excess.opaal` are expected runtime failures.
`pure.opaal` and `callback.opaal` are expected refusals, including their no-draw
calls. Imported initializers and default native embeddings also refuse entropy.
Checker, help, plan and LSP observations do not sample values.

`reference.py` is the independent Python/system-entropy workaround. It also
checks the exact scripted rejection example: words 0 and 5 produce int(0,3)=2;
the next word 2^64-1 produces 1-2^-53. Native values are checked by interval,
lattice and length, without statistical uniqueness assertions.

With a qualified binary, copy this directory into a fresh working directory and
select the matching `tools-linux.toml` or `tools-macos.toml` as `tools.toml`:

```sh
opaal check source.opaal
opaal format --check source.opaal tasks.opaal
opaal source.opaal
python3 reference.py
opaal check --project opaal.toml --task sample --environment ci --format json
opaal plan --project opaal.toml --task sample --environment ci --expires-in 900s --out sample.plan.json
```

Pass the exact `digest` from `sample.plan.json` to `execute --accept`, choosing
an unused journal path. Execute returns the existing plain run receipt; the
sampled Record is available to an explicitly receiving runtime caller. Routine
v3 evidence omits sampled values and their digests, dynamic bounds, derived
paths, process arguments, HTTP methods/payloads and error text for the whole run.
Counts and branch outcomes remain observable: metadata-only evidence is not a
secrecy or noninterference guarantee. Existing tasks without entropy remain v2.

```sh
opaal execute --plan sample.plan.json --accept "$PLAN_DIGEST" --journal sample.run.jsonl
opaal audit --project opaal.toml --journal sample.run.jsonl --out sample.audit.json
opaal audit inspect sample.audit.json
```

The candidate qualifier runs these programs, domain/refusal fixtures, retained
interactive cells, help, project check/plan/execute/audit and stale-source
refusal outside the checkout. Archive mode requires identity-bound program and
fixture archives from a clean committed checkout. Working mode reports its
source snapshot and program/fixture hashes without an archive qualification
claim. Linux x86_64 and macOS arm64 are separate gates; other hosts are not
qualified by these fixtures.

[OPAAL documentation](https://opaal-lang.org/docs/)
