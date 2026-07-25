# Grammar golden corpus

`manifest.tsv` is the normative inventory for the v0.1 parser grammar. Each
non-comment row records a completeness class, grammar family, source path, and
expected classification reason.

`complete` sources must parse as full scripts. `incomplete` sources must request
more input at end of file. `invalid` sources are structurally closed but cannot
participate in any ratified production. Parser tests consume this manifest
directly.
