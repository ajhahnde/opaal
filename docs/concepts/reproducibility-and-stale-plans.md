# Reproducibility and stale plans

A task name is not enough to identify work. Source may change, an input file may be replaced, authority may be edited, an endpoint CA may rotate, or an executable at the same path may become a different binary. A plan binds those identities before execution so the accepted digest has a precise meaning.

Check is static and does not probe tools. Plan makes bounded reads, captures the tool and TLS material, and fixes its creation and expiry times. Execute revalidates every bound identity before the task receives a host. A changed file or tool makes the plan stale; OPAAL does not silently re-plan under the old acceptance. Generate and review a new plan for the new state.

This provides a controlled comparison between review and execution, not a promise that every external fact remains constant afterward. A child process can have its own unconfined behavior; a network peer can return different data. The journal records admitted operations and results, with limits and redaction, rather than claiming an attestation of the outside world.

The [plan format](../reference/formats/plan-artifact.md) lists bound fields. The [tool lock](../reference/formats/tool-lock.md) and [platform support](../reference/platform-support.md) describe executable identity.
