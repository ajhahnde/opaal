# Embedding reference

The current Rust crates expose an explicit [operational context](operational-context.md) for authority, cancellation, injected secrets, and owned cleanup. The embedding client constructs this context. Its existence does not make ordinary source effectful.

[Compatibility](compatibility.md) describes current behavior and the OPAAL 1.0 Rust API stability boundary. For the source-facing task route, use the [operational lifecycle](../operational/lifecycle.md).
