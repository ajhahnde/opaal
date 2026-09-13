# Operational context

Rust embedders can construct an `OperationalContext` that combines one exact `AuthorityContext`, adapter enforcement, a cancellation scope, monotonic deadline, injected-secret redaction, and owned cleanup. This is an explicit client-owned object. Constructing it does not grant an effect to ordinary `.opaal` source.

An authority context belongs to one nonzero evaluation context ID and accepts at most 256 exact rows. Missing and duplicate rows refuse. The adapter reports enforced, unenforced, unsupported, or unknown for the requested boundary; the context combines that report with the exact grant or denial. A declared effect outside the current call's set is unknown even with a matching row.

Cancellation is sticky: the first observed reason remains. A later phase may narrow a deadline but cannot extend it; reaching the exact deadline is a timeout. An embedder registers cleanup before exposing a resource handle. Finishing or dropping the context runs each cleanup exactly once in reverse registration order, keeps the primary outcome, and attaches cleanup failures in observation order.

Secrets enter through `insert_secret`, never ambient credentials or caches. A secret cannot be cloned, displayed, serialized, compared, or read through a payload accessor. The context accepts at most eight identities of at most 64 KiB each. A secret can be materialized only through the one-use `SecretHeader` sink after both exact network and reveal grants pass. The context retains raw and encoded redaction patterns after consumption until closure.

The current POSIX and fake adapters implement this contract. The [compatibility page](compatibility.md) states the Rust API promise; the [source lifecycle](../operational/lifecycle.md) is a separate controlled execution route.
