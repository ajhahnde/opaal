# Scheduling stress

[FlashOS](../../../README.md) › [Flash](../README.md) › Scheduling Stress

These host stress cases run the real `fsh` executable through a pseudoterminal.
Nonzero seeds vary process and job actions while keeping wait times limited and
checking child cleanup and terminal ownership.

The ordinary workspace test gate runs four fixed regression seeds across these
scenarios:

- foreground pipeline cancellation with two to four process-group members,
  zero to two stop/resume cycles, optional job-table observations, and
  foreground, background, or stopped cancellation placement;
- repeated foreground/background job-control cycles;
- concurrent background completions released in different orders; and
- interactive exit while running and stopped jobs remain live.

The host kernel still schedules threads and processes. Replaying a seed repeats
the generated choices and assertions, but not necessarily the same wall-clock
timing.

## Run a campaign

After building Flash, run 64 generated seeds per scenario from the repository
root with the selected candidate runtime:

```sh
components/flash/target/debug/fsh components/flash/scheduling/run-campaign.fsh
```

The first argument selects a positive case count up to 4,096. The optional
second argument selects a new result directory, and the optional third argument
sets a nonzero decimal or `0x`-prefixed campaign seed:

```sh
components/flash/target/debug/fsh components/flash/scheduling/run-campaign.fsh \
  256 /path/to/new-results 0x4f3c2b1a098765ef
```

When no seed is supplied, the runner reads one from the host random source. It
prints the chosen seed before starting. A result directory must not already
exist, preventing separate runs from silently sharing evidence.

The default result path is a unique, ignored directory under
`scheduling/campaigns/`. Every result retains:

- `manifest.txt`, with the campaign seed, bounds, host and tool versions,
  replay command, timestamps, and result; and
- `output.log`, with every exact scenario seed and the complete test output.

## Replay a failure

Replay the complete campaign with the command recorded in its manifest. To run
only one exact seed reported immediately before a failure:

```sh
FLASH_PTY_STRESS_SEEDS=0x0123456789abcdef \
  cargo test -p flash-cli --test pty stress_ -- \
  --nocapture --test-threads=1
```

Replace `stress_` with the complete failing test name to isolate one scenario.
Retain the failed campaign directory until the result has been reproduced and
reduced to a fixed regression seed or a narrower deterministic test.

## Evidence boundary

The PTY harness is supported on Linux and macOS hosts. It exercises real host
process groups, signal delivery, terminal ownership, pipelines, cancellation,
and job notices. It does not establish equivalent FlashOS target behavior.
Redox target builds prove compilation separately, while image/QEMU or hardware
qualification owns target runtime claims. Campaign completion is bounded
evidence for the exercised seeds and scenarios, not proof that scheduling
defects are absent.

---

[← Flash documentation](../docs/README.md)
