# Outcomes and errors

A completed evaluation may retain a value or carrier and a normal status. A catchable language error is different from cancellation, classified refusal, or a fatal host or reporting failure. OPAAL preserves completed stages and partial effects as evidence when a later stage fails. Cleanup failures are attached in observation order instead of replacing the primary outcome.

`throw` produces a language error; `check` can raise one when an upstream stage
did not succeed. A refusal means a required authority, host guarantee, or
supported route was unavailable. It does not mean a task ran and returned a
failing status. In 1.1, an explicit foreground caret stage in a CLI session
can reach the direct process host; unrelated ordinary effects remain refused
before adapter access. An accepted project task may reach a controlled host if
its exact grants and enforcement allow it.

The compiled [`std::outcome` module](../std/outcome.md) exports `Result` and `Option` for values in source. Those variants are not substitutes for the executor's refusal, fatal failure, or journal evidence classes. See [diagnostics](../diagnostics.md) for reported codes and [resources and lifetimes](../../concepts/resources-and-lifetimes.md) for cleanup behavior.
