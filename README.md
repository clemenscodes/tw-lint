# tw-lint

Fail CI / auto-fix on any Tailwind LSP diagnostic, by driving the real
`tailwindcss-language-server` as a subprocess. All Tailwind intelligence lives
in the server; tw-lint only transports files and reports. Config-file-optional;
flags-only works.

`--check` (default) prints diagnostics and exits non-zero on any at warning
severity or above. `--fix` applies the server's quick-fix edits in place.

## Consume as a flake input

```nix
inputs.tw-lint.url = "github:clemenscodes/tw-lint";
```

Then invoke the bundled binary (the pinned server is on `PATH` automatically):

```bash
tw-lint \
  --css crates/hotkey-editor/tailwind.input.css \
  --source 'crates/**/*.rs' \
  --include-lang rust=html \
  --class-regex 'tw!\s*\[([^\]]*)\]' \
  --class-regex '"([^"]*)"'
```

Add `--fix` to rewrite in place (format step); omit it to fail on diagnostics
(CI / lint step).

## Using your own node / language server

The bundled versions are defaults, not a lock:

```bash
tw-lint --server /path/to/tailwindcss-language-server ...     # your server
tw-lint --node /path/to/node --server /path/to/server.js ...  # your node + server
```

`--server` also reads from `TW_LINT_SERVER`. The server resolves the design
system from your project's own `tailwindcss`, so this is how you track a
specific Tailwind version too.

## Notes

- **Extraction is delegated to the server** via `--include-lang` +
  `--class-regex` (mirroring `tailwindCSS.includeLanguages` and
  `experimental.classRegex`); tw-lint never parses classes itself.
- **Cross-class conflict detection** (`cssConflict`) needs the two-level
  container class-regex so classes in one container are grouped; two flat
  regexes only yield per-class diagnostics (canonical suggestions, invalid
  classes). See `docs/lsp-findings.md`.

## Develop

```bash
nix develop
cargo test            # unit tests
cargo test --tests -- --ignored   # e2e tests (need the server on PATH)
```
