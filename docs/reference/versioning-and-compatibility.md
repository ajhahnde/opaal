# Versioning and compatibility

OPAAL 1.0 source has one implicit language identity. A `.opaal` file starts with ordinary declarations or expressions; no version directive or parser selector is needed. Project manifests use a `required_opaal` SemVer range and an explicitly selected environment.

Project control TOML uses schema version 1. Workflow artifacts use the closed identities `opaal.check.v2`, `opaal.plan.v2`, `opaal.run-journal.v2`, and `opaal.audit.v2`. Those suffixes describe file formats, not language versions. Old, future, unknown-field, or tampered artifacts are rejected; rerun check, plan, execute, or audit with a compatible toolchain rather than editing a persisted artifact.

A source program and a Rust embedding client have different compatibility questions: [embedding compatibility](embedding/compatibility.md) records that Rust API shape is not promised stable for OPAAL 1.0.

This page describes the 1.0 contract. A build's version string and any release artifact remain separate evidence of what has actually been published.
