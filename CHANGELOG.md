<div align="center">

<h1>Flash</h1>

<p>
  <a href="README.md"><b>README</b></a> ·
  <b>Changelog</b> ·
  <a href="LICENSE"><b>License</b></a>
</p>

</div>

---

All notable changes to Flash will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project intends to follow [Semantic Versioning](https://semver.org/).

## [Unreleased]
### Changed
- Renamed FlashShell to Flash across the codebase.

- Ratify the v0.1 grammar, operator precedence, closure and backgrounding
  boundaries, and add the normative grammar-family corpus.
- Ratify the v0.1 lexical surface and add the normative complete, incomplete,
  and invalid lexical corpus.
- Add `fsh script.fsh` execution for foreground external commands, byte
  pipelines, conditional status chains, and source-ordered redirections, with
  source diagnostics and process-status exit mapping.
- Add shared shell-free Rust process fixtures and end-to-end coverage for native
  arguments and paths, large streams, broken pipes, resolution failures, and
  redirection setup failures.
- Add an interactive shell for the no-argument invocation, driving the same
  language at a terminal: primary and continuation prompts with parser-driven
  multiline input, syntax highlighting, context-aware completion, history
  autosuggestions, and persistent searchable history. A session retains scope,
  environment, working directory, and status across submissions, recovers from
  parse and runtime errors, and honors `cd` and `exit`. Startup loads a trusted
  user configuration transactionally and enters a visible safe mode on failure.
  New options `--no-config` and `--no-history` opt out of each; Ctrl-C cancels
  the line and Ctrl-D on an empty line exits.
- Add live structured internal pipelines with lazy carrier-preserving execution
  for `ls`, `first`, `last`, `collect`, `length`, `lines`, `select`, `get`, and
  `sort`, plus closure-driven `each`, `where`, and `update`. Structured edges
  are never rendered or serialized between stages; commands that must drain a
  stream enforce a documented item ceiling, and a lazy failure rolls back
  pending session state and closure environment changes.
- Add live explicit byte boundaries: `decode` and `encode` support strict UTF-8,
  lossy UTF-8 decoding, and byte-preserving values; `from` and `to` support JSON
  and line-oriented text; and lazy `open` composes with byte-preserving `save`.
  JSON documents and unterminated text lines enforce documented byte ceilings,
  while byte streams retain producer failures and cancellation without implicit
  decoding, rendering, or serialization.
- Stream explicit byte boundaries across external and internal stages without
  capture, in scripts and interactive sessions. Mixed pipelines preserve
  source-ordered stage statuses and `pipefail`, and stop pulling an internal
  producer when an external consumer closes early.
- Add the checked lifecycle foundation for process-backed jobs: stable shell
  job identities, an all-members startup barrier, foreground/background and
  stopped states, per-process completion observations, prompt-safe notice
  retention, and explicit acknowledged record removal.
- Hand the terminal to a foreground job for exactly the interval it runs, and
  take it back before the next prompt. Ownership is released again if execution
  fails or panics, so a job can no longer leave the terminal owned by a process
  that has exited. A redirected session and a platform without terminal
  ownership are unaffected.
- Start every external stage of one pipeline in a single process group, so a
  pipeline can later be signalled, stopped, and continued as one job instead of
  as unrelated processes. The group is established before each child executes
  and covers the external stages on both sides of an internal pipeline stage. A
  platform that does not provide process groups keeps running pipelines in the
  shell's own group.

---
