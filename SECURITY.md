# Security policy

## Supported versions

OPAAL `1.0.0-alpha.1` is an unreleased development version. There is no
published release line with a security-support promise. Corrections developed
on `main` do not imply a release.

## Reporting a vulnerability

Use GitHub's private vulnerability-reporting form for this repository. Do not
open a public issue for a suspected vulnerability or include secrets,
credentials, private paths, or unrelated system data in a report.

Describe the affected revision, the smallest reproduction, the observed
impact, and any known preconditions. Maintainers will acknowledge the report,
assess the boundary, and coordinate disclosure after a correction is available.

## Current security boundary

OPAAL source is intentionally pure. It grants no filesystem, process,
environment, network, terminal, project, action, task, package, or controlled
workflow authority. Crossing that boundary unexpectedly is security relevant
even without memory corruption.

Repository automation uses read-only tokens for ordinary checks. Package
publication is disabled, and successful workflows create no tag, package,
artifact, or release.
