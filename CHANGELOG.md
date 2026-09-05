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

### Added

- Add an explicitly versioned, pure Flash 2 language foundation with qualified
  modules and compiled operations, nominal records and variants, bounded
  generics and patterns, structured outcomes, typed owned streams, shared
  formatter/checker/help/editor semantics, deterministic analysis and execution
  budgets, and a standalone read-only Flash 1 source migration analyzer. Later
  effects, authority grants, projects, actions, tasks, controlled planning, and
  FlashOS runtime qualification remain explicitly unavailable boundaries.

### Changed

- License Flash under the Mozilla Public License 2.0. Versions distributed
  before this change retain their then-applicable licenses.

## [1.0.0] - 2026-08-24

### Changed

- Freeze the Flash v1 grammar and public runtime contract, remove pre-v1
  config and history fallback paths, and retain only compatibility machinery
  with an explicit v1-or-later owner.
- Align the public guides with the implemented Flash v1 contract, document the
  first-supported-baseline migration policy, config and history locations,
  embedding and startup-recovery boundaries, and both shipped target binaries,
  and remove stale analysis and protocol claims.
- Audit every unsafe block and C ABI declaration, document its local pointer,
  lifetime, ownership, signal, and pre-exec invariants as applicable, and make
  undocumented or implicit future unsafe operations fail workspace linting.
- Harden security-sensitive boundaries: NUL-bearing exports and inherited
  environment snapshots now fail before spawn, complex background jobs retain
  the complete host-representable capture limit, startup module operations are
  explicit restricted-config failures, and persistent history uses validated
  no-follow, close-on-exec directory-relative opens.
- Make portable raw-terminal reads nonblocking and bypass lossy Redox PTY
  readiness notifications with bounded direct reads, so independent editor
  actions cannot remain indefinitely unread on the target console.
- Renamed FlashShell to Flash across the source tree while preserving the
  `fsh` executable, `/usr/bin/fsh`, `.fsh` scripts, and prompt protocol.

### Added

- Add a machine-validated exhaustive Flash v1 exercise inventory, assembled
  host runner with retained exact-source evidence, documentation-example and
  pre-v1 compatibility ownership, and expanded exact-image QEMU cases for the
  language, modules, intrinsics, frontends, and language server.
- Add bounded, replayable host scheduling campaigns for multi-member pipeline
  cancellation, terminal restoration, concurrent completion notices, and live-
  job exit cleanup, with retained manifests and complete failure output.
- Add bounded lexer, parser, and ordinary-word expander fuzz campaigns
  with a fast smoke runner, retained sustained-campaign corpora and artifacts,
  and a documented failure-to-regression workflow.
- Add a versioned FlashOS target-capability matrix that assigns every
  advertised classified operation to an owning target case and exercises the
  required startup, language, session, editor, supported-job, and clean-exit
  surfaces through QEMU. The identical ordered contract renders for
  operator-observed targets while keeping `Signals`, physical hardware, and
  release qualification out of scope.
- Add a versioned FlashOS capability report and reusable target-runtime smoke
  fixtures. The report matches the selected adapter's bounded advertised set,
  keeps `Signals` withheld, records explicit limitations, and shares exact QEMU
  and manually run real-system interactions without claiming exhaustive or
  physical qualification.
- Complete non-executing assignment checks and shared built-in argument
  contracts. `fsh check`, planning, runtime validation, help, completion, hover,
  and signature help now agree on positional arity and kinds, option arity and
  conflicts, `--`, and dynamic-tail handling for every standard built-in.
- Add `fsh plan [--] SOURCE` for deterministic inspection of one exact command
  pipeline through shared parsing, analysis, planning, PATH resolution, and
  structural preflight. Inspection renders escaped native plan data and source
  spans without substitution, mutation, redirection opening, process, config,
  history, or terminal access.
- Connect interactive startup configuration to live session state. Config can
  transactionally set `pipefail`, the command-capture byte limit, completion,
  history, and both prompt strings through config-only typed bindings; safe
  mode keeps its fixed marker and CLI bypasses retain clean defaults.
  Completion now refreshes at every prompt from live scope, cwd, child `PATH`,
  and bounded UTF-8 host snapshots without filesystem work in the keypress
  callback.
- Complete the portable interactive editor contract with grapheme-aware and
  display-cell-correct editing, whole-submission multiline movement, live
  resize, completion, highlighting, hints, persistent history, configurable
  prompts, and editor-owned background notices that preserve an active buffer.
  Host Reedline and forced-portable PTY coverage now exercise the shared
  behavior while FlashOS runtime qualification remains separate.
- Add explicit `glob(String | Path) -> List[Path]` filesystem matching through
  the portable directory-read capability. Component wildcards, character
  classes, and recursive `**` preserve native paths, sort deterministically,
  require explicit hidden-entry intent, avoid directory-symlink traversal, and
  remain cancellation- and resource-bounded without changing ordinary command
  word cardinality.
- Ratify the Flash v1 built-in namespace as an exact 30-command core with no
  current aliases or reserved names, validated core/alias/reserved lifecycle
  metadata, and a language-major compatibility policy that prevents silent
  capture or release of external names. Shared classification now drives
  resolution, planning, execution identity, background routing, static
  `CMD001`/`CMD002` migration diagnostics, help, completion, and `which`; the
  latter reports ordered internal, alias, reserved, external, and missing
  records with canonical target and executable path fields.
- Ratify the v1 grammar, operator precedence, closure and backgrounding
  boundaries, and add the normative grammar-family corpus.
- Ratify the v1 lexical surface and add the normative complete, incomplete,
  and invalid lexical corpus.
- Add `fsh script.fsh` execution for foreground external commands, byte
  pipelines, conditional status chains, and source-ordered redirections, with
  source diagnostics and process-status exit mapping.
- Add a deterministic CI-facing status and channel contract: exact completed
  codes, bounded signal mapping, silent ordinary nonzero completion, distinct
  status-1 shell failure and status-2 launcher misuse, stdout-only program
  bytes, stderr-only diagnostics and background reports, checked writes and
  flushes, and non-recursive diagnostic-stream failure. Interactive fatal exits
  hang up owned jobs before returning status 1 while recoverable diagnostics
  preserve the session.
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
- Stream explicit byte boundaries across any number of alternating external
  and internal segments without capture, in scripts and interactive sessions.
  The mixed executor preserves source-ordered stage statuses and `pipefail`,
  stops pulling an internal producer when an external consumer closes early,
  and keeps lazy structured streams session-owned rather than moving them
  across threads.
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

[← Back to Flash Overview](README.md) · [Flash documentation](docs/README.md) · [FlashOS changelog →](../../CHANGELOG.md)
