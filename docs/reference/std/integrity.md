# `std::integrity`

`std::integrity` exports `sha256(input: Bytes) -> String`. It hashes the exact byte sequence and renders a lowercase SHA-256 digest with the `sha256:` prefix.

An artifact's `digest` field binds its canonical JSON content with that field omitted; it is not the hash of the entire persisted file. Plans also bind source, control documents, tool executables, and relevant inputs by exact identities. If one of those changes before execution, the plan is stale even if its human-readable task name is unchanged.

Hashing is an integrity check, not proof that the data came from a trusted party. The [plan format](../formats/plan-artifact.md) describes what is bound; [reproducibility and stale plans](../../concepts/reproducibility-and-stale-plans.md) explains the practical consequence.
