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

## Nuance: conflict detection needs the two-level container regex

With two **flat** `classRegex` patterns (`tw!\[…\]` then `"…"`), the server
treats each quoted string as its own single-class context, so cross-class lints
like `cssConflict` (`block` vs `flex`) do **not** fire. Per-class lints
(canonical suggestions, invalid classes) work fine.

To get conflict detection, use the **two-level container form** so all classes
in one `tw![…]` are grouped:

```
classRegex: [ [ "tw!\\s*\\[([^\\]]*)\\]", "\"([^\"]*)\"" ] ]
```

i.e. an outer container regex capturing the macro body, and an inner regex
extracting each class within it. This maps to `ClassRegex::Container` in the
config. The flags-only path currently emits flat `ClassRegex::Simple` patterns;
consumers wanting conflict detection should supply the container form via a
config file (a `--class-regex-container` flag is a possible future addition,
out of scope for v1).
