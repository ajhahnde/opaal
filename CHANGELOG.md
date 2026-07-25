<div align="center">

<h1>FlashShell</h1>

<p>
  <a href="README.md"><b>README</b></a> ·
  <b>Changelog</b> ·
  <a href="LICENSE"><b>License</b></a>
</p>

</div>

---

All notable changes to FlashShell will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project intends to follow [Semantic Versioning](https://semver.org/).

## Unreleased

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

---
