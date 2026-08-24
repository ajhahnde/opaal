# Flash v1 exercise contract

[FlashOS](../../../README.md) › [Flash](../README.md) › Flash v1 Exercises

This directory owns the executable inventory used to qualify the complete
Flash v1 user contract. [`v1.toml`](v1.toml) lists every user-reachable
language, intrinsic, built-in, frontend, configuration, language-server,
editor, process, platform, and active-documentation surface. Each surface has
an assembled host exercise, an intentional-negative owner where rejection is
contractual, and an exact FlashOS evidence owner.

[`run.py`](run.py) builds or reuses the product binaries and records each exact
action, input, expectation, observation, environment, and result. Run
`python3 exercises/run.py --profile ci` from `components/flash`; add
`--no-build` only after the workspace binaries and test fixtures have already
been built. The smoke profile runs the direct assembled-script cases, while the
CI and full profiles also run executable-boundary acceptance owners for PTY,
configuration, frontend, language-server, platform, and documentation paths.

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

[← Previous: Performance benchmarks](../benchmarks/README.md) · [Flash documentation](../docs/README.md) · [Next: Scheduling stress →](../scheduling/README.md)
