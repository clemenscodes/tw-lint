# Language-server behavior (Task 3 spike)

Server: `tailwindcss-language-server` 0.14.29 (nixpkgs), launched with `--stdio`.
Verified against `tests/fixtures/project` (a `@import "tailwindcss";` v4 entry +
a `.rs` file with classes inside `tw![…]`).

## Configuration is pulled, and it works

The server does not read `initializationOptions`; it **pulls** config by sending
`workspace/configuration` requests with `[{ "section": "tailwindCSS" }, …]`. The
client answers with the `tailwind_settings(...)` object. Confirmed honored:

- `includeLanguages` (`rust` → `html`) — the server analyzed a `.rs` document.
- `experimental.classRegex` — it extracted `w-[100%]` from inside `tw![ "…" ]`.
- `experimental.configFile` — pointed at the fixture's `tailwind.input.css`.

## Open item RESOLVED: canonical suggestions are diagnostics

The canonical ("write it shorter") suggestion arrives as a normal
`textDocument/publishDiagnostics` entry — **not** a code-action-only feature:

```
severity: Warning
code:     "suggestCanonicalClasses"
message:  "The class `w-[100%]` can be written as `w-full`"
range:    the class span
```

Consequence: `--check` catches canonicalization with the diagnostic stream
alone; no code-action sweep is needed for detection. Code actions are still
requested in `--fix` mode to obtain the replacement edit.

## Whole-list diagnostics need JOINED classes (the big one)

Tailwind's diagnostics operate on a whole class **list**, not per class. Most of
`suggestCanonicalClasses` fires on MULTIPLE classes together (`pt-4 pr-4 pb-4
pl-4` → `p-4`), and `cssConflict` needs the conflicting classes in one list.
A macro like `tw!["a","b","c"]` puts each class in its own string, so
per-string extraction (even the two-level container classRegex, which the LSP
does NOT merge) reports almost nothing — on warcraft it found 0 where 161 exist.

Fix: tw-lint JOINS every class in a container block into one synthetic
`<div class="…">` and lints that. `--fix` derives each canonical rewrite from
the diagnostic's own range + message (`can be written as \`Y\``) and writes the
corrected list back into the block — no `textDocument/codeAction` round-trips
(161 of them OOM-crash the server on a large corpus). Exact-duplicate classes
are dropped; semantic conflicts (two different classes) are report-only.

## Chunking + push-wait (streaming AND correctness)

The synthetic corpus is linted in CHUNKS of ~120 blocks, each its own document,
`didClose`d after — so the server never holds the whole corpus and results
stream per chunk. Crucially, per chunk you must **wait for the server's
`publishDiagnostics` push** for that document (`collect_diagnostics_for`): a
`textDocument/diagnostic` PULL right after `didOpen` races the server's
debounced analysis and returns empty for all but the first chunk (161 → 9). The
push notification is the only deterministic "analysis complete" signal.

## Reliability at scale: pull diagnostics + didClose (both required)

Two bugs surfaced running against a 508-file tree:

1. **Push diagnostics are async/debounced.** Collecting them per-file with a
   "barrier request then drain, filter by uri" silently drops diagnostics that
   arrive out of step — the run was non-deterministic (0 found / transient
   error). Fix: **pull diagnostics** (`textDocument/diagnostic`) when the server
   advertises `diagnosticProvider` — a synchronous request whose response holds
   the full set. Deterministic. The push+barrier path remains only as a fallback.
2. **Opening every file without closing OOM-crashes the Node server**
   (`Broken pipe`, V8 `StringDecoder` trace). The server retains every open
   document. Fix: `textDocument/didClose` after diagnosing each file, so at most
   one document is open at a time. With both fixes: 4/4 deterministic clean runs.

## What actually fires (empirically, server 0.14.29)

- `suggestCanonicalClasses` (canonical suggestion) — fires per class. ✓
- `cssConflict` — fires ONLY for classes **space-separated in one string**
  (`tw!["block flex"]`), or across separate strings **when grouped by a
  container regex** (below). ✓
- Invalid/unknown class (e.g. a typo `flexx`) — **NOT linted**. Tailwind
  IntelliSense does not flag unrecognized classes.

## Conflict detection + scoping need the two-level container regex

With two **flat** `classRegex` patterns (`tw!\[…\]` then `"…"`), the server
treats each quoted string as its own single-class context, so cross-class lints
like `cssConflict` (`block` vs `flex`) do **not** fire. Per-class lints
(canonical suggestions, invalid classes) work fine.

A **flat** `--class-regex "\"([^\"]*)\""` lints EVERY string literal in the
file — SVG path data, error messages, doc comments — not just Tailwind classes.
It happens to report little only because random prose rarely looks like a
conflicting/non-canonical utility; a stray `"grid table"` in an error string
would fail CI spuriously. So flat is wrong for scoping.

Use the **two-level container form** (`--class-container` + `--class-regex`) so
extraction is scoped to `tw![…]` and classes within one block are grouped:

```
--class-container 'tw!\s*\[((?:[^\[\]]|\[[^\]]*\])*)\]'  --class-regex '"([^"]*)"'
```

The container regex must be **bracket-aware and escape `]`**: a naive
`[^]]` is parsed ambiguously by the JS engine (matches nothing), and a plain
`[^\]]*` truncates at the first `]` inside an arbitrary value like `w-[26cqi]`.
The `(?:[^\[\]]|\[[^\]]*\])*` form traverses one level of `[...]` so it captures
the whole (possibly multi-line) `tw![…]` block. Verified: it flags a conflict
and a canonical suggestion inside `tw![…]`, and does NOT flag an identical prose
string outside it. This is the correct invocation for the warcraft consumer.
