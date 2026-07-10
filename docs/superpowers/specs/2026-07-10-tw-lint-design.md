# tw-lint — design

**Status:** approved design, pending implementation plan
**Date:** 2026-07-10

## Purpose

A standalone Rust CLI that drives the real Tailwind CSS language server as a
subprocess (a "mini LSP client"), collects its diagnostics against a project's
source files, and either **fails** (for CI / lint pipelines) or **auto-fixes**
(for format pipelines) — so a codebase can enforce that every Tailwind class it
uses is the one the LSP would suggest: no class conflicts, no invalid/unknown
classes, no deprecated utilities, and canonical (shortest-equivalent) class
names.

The tool is **domain-agnostic**. It knows nothing about any particular project's
macros or file layout; all of that is supplied as configuration. The first
consumer is `warcraft-hotkey-editor`, whose classes live inside Rust
`tw![…]` / `classes!` macros, but nothing in the tool is specific to that.

## Why an LSP client (not a reimplementation)

The question "is this class canonical / valid / conflicting?" is answered by
Tailwind v4's design system, which is a JavaScript module derived from the
project's own `@theme`. Reimplementing it in Rust would mean reimplementing
Tailwind's candidate parser + theme engine, and would drift from what an editor
actually shows. So the intelligence stays in the official
`tailwindcss-language-server`; `tw-lint` is a client that feeds it files and
reports what it says. This guarantees the CLI's verdict matches the editor's
verdict by construction.

`tw-lint` also does **not** extract classes itself. The language server already
knows how to find classes in arbitrary syntax via its `includeLanguages` +
`experimental.classRegex` settings. `tw-lint` passes those settings through and
lets the server's own extractor do the work — which is precisely what makes the
tool generic across languages and macro syntaxes.

## Distribution

Packaged as a Nix flake. The flake output wraps the Rust binary with a **pinned
`tailwindcss-language-server`** (and its Node runtime) on `PATH`, so a consumer
needs no per-repo Node/npm setup. The pinned server version is independent of the
consumer's own `tailwindcss` version: the server still loads the *consumer's*
`@theme` / design system for canonicalization, but the diagnostic feature set
comes from the pinned server. Consumers add one flake input and invoke
`tw-lint` from their existing task runner.

**The bundled versions are a default, not a lock.** A user is never limited to
the flake's node/server: `--server <path-or-command>` selects the
language-server executable (default: `tailwindcss-language-server` on `PATH`,
i.e. the bundled one) and `--node <path>` runs it with a chosen Node
(`<node> <server> --stdio`, for when the server is a `.js` entry or a specific
runtime is required). This lets a consumer track a newer/older
`tailwindcss-language-server` — and, through the server's own module
resolution, a specific `tailwindcss` — without rebuilding tw-lint.

## Configuration

Two equivalent, overlapping sources — a config file is **optional**:

- `--config <path>` points at a `tw-lint.toml`.
- Individual flags supply the same values so a consumer can invoke the bundled
  binary with flags only and no file on disk:
  - `--source <glob>` (repeatable) — files to lint.
  - `--css <path>` — the Tailwind v4 entry CSS (e.g. `tailwind.input.css`),
    the design-system source of truth.
  - `--include-lang <id=served-as>` (repeatable) — e.g. `rust=html`, mirrors
    `tailwindCSS.includeLanguages`.
  - `--class-regex <regex>` (repeatable) — mirrors
    `tailwindCSS.experimental.classRegex`; supports the two-level
    `[containerRegex, classRegex]` form for macro bodies.
  - `--server <path-or-command>` — language-server executable to launch
    (default: `tailwindcss-language-server`, i.e. the flake-bundled one; env
    `TW_LINT_SERVER` overrides the default).
  - `--node <path>` — optional Node runtime; when set, the server is launched
    as `<node> <server> --stdio`.
  - `--fix` — apply fixes in place instead of only reporting.

Flags override file values where both are given. Precedence and the full flag
list are finalized in the implementation plan.

Example invocation from `warcraft-hotkey-editor` (flags only):

```
tw-lint \
  --css crates/hotkey-editor/tailwind.input.css \
  --source 'crates/**/*.rs' \
  --include-lang rust=html \
  --class-regex 'tw!\s*\[([^\]]*)\]' \
  --class-regex '"([^"]*)"'
```

(Exact regexes are an implementation detail, validated against real
`tw![…]` / `classes!` call sites during the build.)

## Architecture

Small, independently-testable units:

- **`cli`** — parse flags/config into a single resolved `LintConfig`. Pure;
  no I/O beyond reading the config file.
- **`lsp_client`** — own the server subprocess and the JSON-RPC transport:
  spawn, `initialize`/`initialized` handshake (sending the workspace root,
  the Tailwind settings, and client capabilities), request/response and
  notification plumbing, shutdown. Knows the protocol, not the lint policy.
- **`session`** — the lint workflow over `lsp_client`: for each source file,
  `didOpen` it, await `publishDiagnostics`, collect them; in `--fix` mode,
  request `textDocument/codeAction` for each diagnostic range and gather the
  returned `WorkspaceEdit`s.
- **`edits`** — apply a set of `WorkspaceEdit`s to files on disk, correctly
  (reverse-sorted ranges per file so offsets stay valid), idempotently.
- **`report`** — render collected diagnostics as `file:line:col` with the rule
  and message; compute the process exit code.

## Data flow

```
LintConfig ──▶ lsp_client (spawn + initialize with TW settings)
           ──▶ session: for each file → didOpen → publishDiagnostics
                                  └─(--fix)─▶ codeAction → WorkspaceEdit
           ──▶ report (─▶ exit 1 on any fatal diagnostic)
                  and/or edits (apply fixes in place)
```

## Modes

- `--check` (default): exit `1` if any diagnostic at or above the fatal
  severity threshold; exit `0` otherwise. Prints each diagnostic. This is what
  runs in CI and in a repo's `lint` step.
- `--fix`: apply all fixable diagnostics' edits to the source files, then
  report anything left unfixable. This is what runs in a repo's `format` step.

Default fatal threshold: **warning and above** (i.e. any diagnostic the LSP
emits fails `--check`). A future `--allow <rule>` / severity-filter mechanism is
explicitly out of scope for v1 (YAGNI) but the report layer is structured so it
can be added without rework.

## Open item (resolved as first implementation task)

The full diagnostic set — class conflicts (`cssConflict`), invalid classes,
deprecated utilities — is delivered via `publishDiagnostics` and is reliable.
It is **not yet confirmed** whether the *canonical-suggestion* ("this class has
a shorter equivalent") surfaces as a `publishDiagnostics` entry or only as an
on-request `codeAction` in current server versions.

**Task 1 is a spike** against the pinned server that determines this. The
`session` unit is designed to handle either outcome: it always collects
`publishDiagnostics`, and — if the spike shows canonicalization is code-action
only — additionally sweeps `codeAction` over the extracted class ranges to
detect it in `--check` mode. Either way the architecture above is unchanged;
only the `session` collection loop's exact steps depend on the spike.

## Non-goals (v1)

- No pure-Rust reimplementation of Tailwind's design system.
- No editor/LSP-server behavior — this is a client only, run to completion.
- No per-rule severity configuration or allow-lists.
- No support for extraction the language server itself cannot express via
  `includeLanguages` + `classRegex`.

## Success criteria

- Running `tw-lint --check …` against `warcraft-hotkey-editor` exits non-zero
  when a `.rs` file contains a non-canonical / conflicting / invalid Tailwind
  class inside a `tw![…]` / `classes!` macro, and exits zero when all are clean.
- `tw-lint --fix …` rewrites those classes in place to the LSP-suggested form,
  leaving the files otherwise byte-identical, and a subsequent `--check` passes.
- The verdict matches what the Tailwind LSP shows in an editor for the same
  files and the same `@theme`.
- Consumed purely as a flake input + a flags-only invocation; no Node/npm setup
  in the consumer repo.
