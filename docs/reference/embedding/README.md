# Embedding reference

The current Rust crates expose an explicit [operational context](operational-context.md)
for authority, cancellation, injected secrets, and owned cleanup. Existing
embedding constructors remain deny by default for direct external programs;
the CLI's 1.1 foreground grant does not flow into them.

[Compatibility](compatibility.md) describes current behavior and the OPAAL 1.0 Rust API stability boundary. For the source-facing task route, use the [operational lifecycle](../operational/lifecycle.md).
