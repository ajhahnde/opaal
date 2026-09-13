# Embedding compatibility

OPAAL's current Rust crates expose callable syntax, runtime, platform, and operational entry points. Embedders can build an explicit authority and resource context, use maintained bounded operations, and observe structured outcomes under the current crate versions. Pure source parsing and evaluation do not acquire host authority because an embedding client holds such a context.

OPAAL 1.0 does not promise a stable Rust API *shape*. Public `pub` items and a compiled embedding test establish present behavior, not a guarantee that Rust names, types, or signatures remain source-compatible throughout 1.x. A later stable Rust API commitment needs its own reviewed product and release decision. Do not infer that commitment from the OPAAL source-language version or artifact schema suffix.

For source programs, [the 1.0 specification](../../specification/1.0.md) and [versioning](../versioning-and-compatibility.md) state the separate language and artifact contracts. For current embedding behavior, see [operational context](operational-context.md).
