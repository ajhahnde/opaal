# Security policy

## Supported versions

OPAAL 1.0 is the documented product line. Published releases identify the
versions maintained for security fixes; a development checkout does not itself
constitute a supported release. Corrections developed on `main` do not imply
publication.

## Reporting a vulnerability

Use GitHub's private vulnerability-reporting form for this repository. Do not
open a public issue for a suspected vulnerability or include secrets,
credentials, private paths, or unrelated system data in a report.

Describe the affected revision, the smallest reproduction, the observed
impact, and any known preconditions. Maintainers will acknowledge the report,
assess the boundary, and coordinate disclosure after a correction is available.

## Current security boundary

The OPAAL 1.1 source contract permits standalone and interactive clients to
start an explicit foreground `^program`. The child inherits the session's
native environment, including possible credentials, and can perform any effect
its OS user permits. OPAAL does not sandbox or audit that child. Unrelated host
effects remain refused. Declaring an action or importing a project identity
grants no additional filesystem, process, network, secret, or clock authority. An
explicitly selected project task can execute only after check, review of an
identity-bound plan, and
request-local acceptance of its exact digest under matching authority and host
enforcement. Crossing either boundary unexpectedly is security relevant even
without memory corruption.

Embedding clients may call the maintained bounded operational APIs only through
an explicit exact authority context. HTTPS uses only the endpoint's supplied
CA, secret bytes can enter only the one-use `SecretHeader` sink, and maintained
children receive an explicit environment with no ambient inheritance. Process
internals are reported as unenforced and require explicit acknowledgement.

Repository automation uses read-only tokens for ordinary checks. Crate
publication is disabled. The release workflow stores binary archives as
workflow artifacts but does not create a tag or GitHub release; the maintainer
publishes the exact checked files.
