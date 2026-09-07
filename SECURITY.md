# Security policy

## Supported versions

OPAAL `1.0.0-alpha.1` is an unreleased development version. There is no
published release line with a security-support promise. Security corrections
for the active foundation are developed on `main` and do not imply a release.

## Reporting a vulnerability

Use GitHub's private vulnerability-reporting form for this repository. Do not
open a public issue for a suspected vulnerability and do not include secrets,
credentials, private paths, or unrelated system data in a report.

Describe the affected revision, the smallest reproduction, the observed
impact, and any known preconditions. Maintainers will acknowledge the report,
assess the boundary, and coordinate disclosure after a correction is available.

## Current security boundary

The language 1 foundation is intentionally pure. It grants no filesystem,
process, environment, network, terminal, project, action, task, package, or
controlled-workflow authority. The Flash 1 migration analyzer is bounded and
read-only. A behavior that crosses either boundary unexpectedly is security
relevant even if it does not expose memory corruption.

Repository automation uses read-only tokens for ordinary checks. Publishing
workflows and package publication are disabled; a successful workflow does not
create a tag, package, artifact, or release.
