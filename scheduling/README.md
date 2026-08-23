# Scheduling stress

[FlashOS](../../../README.md) › [Flash](../README.md) › Scheduling Stress

Flash's host scheduling stress cases exercise the real `fsh` executable over a
pseudoterminal. They use deterministic nonzero seeds to vary process and job
actions while retaining bounded waits, exact child-reaping checks, and terminal
ownership assertions.

The ordinary workspace test gate runs four fixed regression seeds across these
scenarios:

- foreground pipeline cancellation with two to four process-group members,
  zero to two stop/resume cycles, optional job-table observations, and
  foreground, background, or stopped cancellation placement;
- repeated foreground/background job-control cycles;
- concurrent background completions released in different orders; and
- interactive exit while running and stopped jobs remain live.

These schedules vary actions and release order. The host kernel still owns
thread and process scheduling, so replay preserves the generated choices and
asserted boundaries rather than promising identical wall-clock timing.

## Run a campaign

From the Flash workspace, run 64 generated seeds per scenario:

```sh
./scheduling/run-campaign.sh
```

The first argument selects a positive case count up to 4,096. The optional
second argument selects a new result directory, and the optional third argument
sets a nonzero decimal or `0x`-prefixed campaign seed:

```sh
./scheduling/run-campaign.sh 256 /path/to/new-results 0x4f3c2b1a098765ef
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

[← Previous: Development](../docs/development.md) · [Flash documentation](../docs/README.md) · [Next: Fuzz targets →](../fuzz/README.md)
