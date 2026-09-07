# Flash v1 exercise contract

[FlashOS](../../../README.md) › [Flash](../README.md) › Flash v1 Exercises

This directory contains the executable inventory for the complete Flash v1
user contract. [`v1.toml`](v1.toml) lists every user-reachable
language, intrinsic, built-in, frontend, configuration, language-server,
editor, process, platform, and active-documentation feature. Each entry points
to a host exercise, an expected rejection case, or the FlashOS test that covers
it.

[`run.fsh`](run.fsh) builds or reuses the product binaries and records the
actions, inputs, expectations, observations, environment, and results. The
case order and coverage map live in
[`host-cases-v1.json`](host-cases-v1.json). From the repository root, prepare
the independent bootstrap and run the CI profile with the selected candidate
runtime:

```sh
make flash-bootstrap
FLASH_V1_BOOTSTRAP_FSH="$PWD/build/flash-bootstrap/134635a5e1282b5d8455a4b2aeb754be5a3a77c1/fsh" \
  components/flash/target/debug/fsh components/flash/exercises/run.fsh \
  --profile ci
```

Add `--no-build` only after the workspace binaries and test fixtures have
already been built. The smoke profile runs the direct assembled-script cases,
while the CI and full profiles also run executable-boundary acceptance owners
for PTY, configuration, frontend, language-server, platform, and documentation
paths.

The native runner records its driving `fsh 1.0.0` rather than a Python version
and refuses source paths containing newlines because frozen-v1 Flash cannot
iterate those paths without ambiguity. Flash 1.0 also lacks guaranteed
scope-exit cleanup; an interruption or runtime-adapter failure may leave the
runner's uniquely owned temporary directory for inspection.

[`evidence/host-v1.json`](evidence/host-v1.json) is the retained host execution
for the source digest recorded in that file. CI validates that the report still
matches the current source inventory and reruns the complete CI profile. A host
pass establishes only the recorded macOS or Linux environment; it is not
FlashOS evidence.

FlashOS execution is owned by the versioned
[`flashos-x86_64-target-matrix-v1.toml`](../platforms/flashos-x86_64-target-matrix-v1.toml)
and its QEMU consumer. The matrix exercises the installed `fsh` and
`flash-language-server` inside the exact candidate image. Signals remain an
explicitly withheld capability. Physical-device qualification remains separate
and requires exact-device read-only identification plus operator approval
before any write.

The documentation inventory covers every fenced block in the active component
overview and four canonical guides. Runnable language and scripting examples
are assigned to assembled executable owners; output, protocol descriptions,
architecture models, and operational commands are classified under their
stronger validation or qualification gates. Adding or removing a block without
updating the inventory fails CI.

The compatibility inventory is deliberately separate from the v1 contract.
Pre-v1-only executable routes have been removed; retained internal machinery
has an explicit v1-or-later owner. An unclassified production legacy marker
fails validation.

This suite supports a bounded claim: the exact recorded candidate passed every
listed action in each named available environment. It does not promise that
arbitrary hosts, unlisted platform behavior, physical hardware, or future
revisions work, and it is not release qualification by itself.

The [Flash 1.0.0 release record](../release/v1.toml) binds this inventory to the
component version, frozen conformance contract, public claims, and exact
candidate workflow without expanding the suite's host, target, or hardware
claims.

---

[← Flash documentation](../docs/README.md)
