# Security model

OPAAL separates source meaning from permission. A standalone or interactive
CLI session in 1.1 admits explicit foreground `^program` execution and its
named context controls. The session starts from a native cwd and bounded native
environment snapshot. The child receives the current session context, which
`cd`, `export`, and `unset` can change; its environment may contain credentials.
The child can perform opaque
filesystem, network, and device effects under the OS user's authority. OPAAL
does not sandbox, audit, roll back, or redact the child's output. Other host
effects remain refused before access. Naming a module or declaring an effect
grants nothing. Static tools do not probe or spawn, and embeddings remain
deny by default without an explicit host-enabled context.

The controlled route begins with an explicitly selected project task, exact
environment, static check, reviewed plan, and request-local digest acceptance.

Permission is narrow. Authority rows name exact typed requests; missing rows deny. The adapter independently reports whether the selected host can enforce the boundary. Unsupported and unknown results do not become grants. A process child is deliberately classified as unenforced internally even when the Linux parent binds its executable, argv, environment, output, and lifetime. macOS refuses that process route.

File adapters retain directory descriptors and refuse symlink traversal. TLS uses only a declared endpoint CA and server name. Secret bytes are injected, one-use, redacted in several encodings, and omitted from journal payloads. Project control reads, output, and runtime work are bounded. A refused plan remains inspectable evidence of why execution did not start.

These mechanisms limit what OPAAL itself admits. They do not claim sandboxing of an admitted child, rollback of external changes, or attestation of an HTTP peer. See [authority](../reference/operational/authority.md), [capabilities](../reference/operational/capability-matrix.md), and [journal format](../reference/formats/journal-artifact.md).
