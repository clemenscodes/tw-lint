use crate::cli::LintConfig;
use crate::groups::{ClassGroup, GroupMatcher};
use crate::lsp::client::Client;
use anyhow::{Context, Result};
use lsp_types::{Diagnostic, DiagnosticSeverity, Url};
use regex::Regex;
use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

/// Blocks per synthetic document. Small enough that the language server never
/// holds the whole corpus (it OOMs otherwise), large enough to stay fast.
const CHUNK_SIZE: usize = 120;

struct Palette {
    warn: &'static str,
    error: &'static str,
    dim: &'static str,
    bold: &'static str,
    reset: &'static str,
}

impl Palette {
    fn detect() -> Self {
        let colored = std::env::var_os("NO_COLOR").is_none()
            && (std::env::var_os("FORCE_COLOR").is_some()
                || std::env::var_os("CLICOLOR_FORCE").is_some()
                || std::io::stdout().is_terminal());
        if colored {
            Self {
                warn: "\x1b[33m",
                error: "\x1b[31m",
                dim: "\x1b[2m",
                bold: "\x1b[1m",
                reset: "\x1b[0m",
            }
        } else {
            Self {
                warn: "",
                error: "",
                dim: "",
                bold: "",
                reset: "",
            }
        }
    }
}

fn relative<'a>(path: &'a Path, root: &Path) -> &'a Path {
    path.strip_prefix(root).unwrap_or(path)
}

/// Column where the class value starts in `<div class="…">`.
const CLASS_VALUE_COLUMN: u32 = 12;

struct Corpus {
    groups: Vec<ClassGroup>,
    file_texts: BTreeMap<PathBuf, String>,
}

fn collect_corpus(config: &LintConfig, matcher: &GroupMatcher) -> Result<Corpus> {
    let root = std::fs::canonicalize(&config.root)?;
    let mut groups = Vec::new();
    let mut file_texts = BTreeMap::new();
    for source_glob in &config.sources {
        let pattern = root.join(source_glob).to_string_lossy().into_owned();
        for entry in glob::glob(&pattern).context("invalid --source glob")? {
            let path = entry?;
            let text = std::fs::read_to_string(&path)?;
            let mut extracted = matcher.extract(&path, &text);
            if !extracted.is_empty() {
                file_texts.insert(path.clone(), text);
                groups.append(&mut extracted);
            }
        }
    }
    Ok(Corpus { groups, file_texts })
}

/// Refuse to report "clean" when nothing was extracted — a zero-block corpus
/// almost always means the `--class-container` regex does not match the macro
/// (e.g. it was written to match the whole block instead of just the opener),
/// and silently passing is the false-green that must never happen.
fn ensure_blocks_matched(corpus: &Corpus) -> Result<()> {
    if corpus.groups.is_empty() {
        anyhow::bail!(
            "no class blocks matched --class-container: nothing was linted. \
             The regex must match only the macro opener (e.g. `tw!\\s*\\[`); \
             tw-lint scans for the closing bracket itself."
        );
    }
    Ok(())
}

/// One synthetic HTML document for a chunk: block `i` becomes a `class="…"` on
/// line `i`, so a diagnostic's line maps straight back to a block in the chunk.
fn build_document(chunk: &[ClassGroup]) -> String {
    chunk
        .iter()
        .map(|group| format!("<div class=\"{}\"></div>", group.classes.join(" ")))
        .collect::<Vec<_>>()
        .join("\n")
}

fn chunk_uri(config: &LintConfig, index: usize) -> Result<Url> {
    let root = std::fs::canonicalize(&config.root)?;
    let path = root.join(format!("__twlint_synthetic_{index}.html"));
    Url::from_file_path(&path).map_err(|_| anyhow::anyhow!("root is not absolute"))
}

fn diagnose(client: &mut Client, uri: &Url, document: String) -> Result<Vec<Diagnostic>> {
    client.notify(
        "textDocument/didOpen",
        serde_json::json!({ "textDocument": {
            "uri": uri, "languageId": "html", "version": 1, "text": document } }),
    )?;
    // Block until the server pushes final diagnostics for this chunk, then
    // close it so the server never holds the whole corpus.
    let diagnostics = client.collect_diagnostics_for(uri)?;
    client.close_document(uri)?;
    Ok(diagnostics)
}

fn is_fatal(diagnostic: &Diagnostic) -> bool {
    match diagnostic.severity {
        Some(severity) => severity <= DiagnosticSeverity::WARNING,
        None => true,
    }
}

fn is_canonical(diagnostic: &Diagnostic) -> bool {
    matches!(
        &diagnostic.code,
        Some(lsp_types::NumberOrString::String(code)) if code == "suggestCanonicalClasses"
    )
}

/// The replacement text from a canonical-suggestion message
/// (``The class `X` can be written as `Y` `` → `Y`).
fn canonical_replacement(message: &str) -> Option<String> {
    let pattern = Regex::new(r"can be written as `(.+)`").expect("valid regex");
    pattern
        .captures(message)
        .and_then(|captures| captures.get(1))
        .map(|value| value.as_str().to_string())
}

/// Stream diagnostics to stdout as each chunk completes; return the fatal count.
pub fn run_join_check(config: &LintConfig) -> Result<usize> {
    let matcher = GroupMatcher::from_config(config)?;
    let corpus = collect_corpus(config, &matcher)?;
    ensure_blocks_matched(&corpus)?;
    let root = std::fs::canonicalize(&config.root)?;
    let palette = Palette::detect();
    let mut client = Client::launch(config)?;

    let mut fixable = 0;
    let mut conflicts = 0;
    let mut last_file: Option<PathBuf> = None;
    let mut out = std::io::stdout().lock();

    for (chunk_index, chunk) in corpus.groups.chunks(CHUNK_SIZE).enumerate() {
        let document = build_document(chunk);
        let uri = chunk_uri(config, chunk_index)?;
        let diagnostics = diagnose(&mut client, &uri, document)?;
        for diagnostic in diagnostics {
            let local = diagnostic.range.start.line as usize;
            let group = match chunk.get(local) {
                Some(group) => group,
                None => continue,
            };
            if last_file.as_deref() != Some(group.file.as_path()) {
                let rel = relative(&group.file, &root);
                let _ = writeln!(out, "\n{}{}{}", palette.bold, rel.display(), palette.reset);
                last_file = Some(group.file.clone());
            }
            let (color, mark) = if is_canonical(&diagnostic) {
                fixable += 1;
                (palette.warn, "fix ")
            } else {
                conflicts += 1;
                (palette.error, "warn")
            };
            let _ = writeln!(
                out,
                "  {color}{mark}{reset} {dim}{}:{}{reset}  {}",
                group.line,
                diagnostic
                    .range
                    .start
                    .character
                    .saturating_sub(CLASS_VALUE_COLUMN),
                diagnostic.message,
                color = color,
                reset = palette.reset,
                dim = palette.dim,
            );
        }
        let _ = out.flush();
    }
    client.shutdown()?;

    let total = fixable + conflicts;
    if total == 0 {
        let _ = writeln!(
            out,
            "\n{}✓ no Tailwind issues{}",
            palette.bold, palette.reset
        );
    } else {
        let _ = writeln!(
            out,
            "\n{bold}{total} issue(s){reset}: {warn}{fixable} auto-fixable{reset} \
             (canonical + duplicates; run --fix), {err}{conflicts} conflict(s){reset} \
             to resolve by hand — the tool never deletes a class to settle a conflict",
            bold = palette.bold,
            reset = palette.reset,
            warn = palette.warn,
            err = palette.error,
        );
    }
    Ok(fixable + conflicts)
}

struct ValueEdit {
    start: usize,
    end: usize,
    replacement: String,
}

/// Apply canonical suggestions (auto) and duplicate removal to every block,
/// streaming progress; rewrite each changed block in place.
pub fn run_join_fix(config: &LintConfig) -> Result<()> {
    let matcher = GroupMatcher::from_config(config)?;
    let corpus = collect_corpus(config, &matcher)?;
    ensure_blocks_matched(&corpus)?;
    let root = std::fs::canonicalize(&config.root)?;
    let palette = Palette::detect();
    let mut client = Client::launch(config)?;
    let mut out = std::io::stdout().lock();

    // Per file: (list byte-span, replacement source) for every changed block.
    let mut rewrites: BTreeMap<PathBuf, Vec<(std::ops::Range<usize>, String)>> = BTreeMap::new();
    let mut applied = 0;
    let mut reformatted = 0;
    let mut conflicts = 0;
    let mut last_file: Option<PathBuf> = None;

    for (chunk_index, chunk) in corpus.groups.chunks(CHUNK_SIZE).enumerate() {
        let document = build_document(chunk);
        let uri = chunk_uri(config, chunk_index)?;
        let mut diagnostics = diagnose(&mut client, &uri, document)?;
        // Stream in source order (block, then column within a block).
        diagnostics.sort_by_key(|diagnostic| {
            (
                diagnostic.range.start.line,
                diagnostic.range.start.character,
            )
        });

        // Only the guaranteed-safe fixes are applied: canonical rewrites and
        // exact-duplicate removal. Conflicts (two DIFFERENT classes fighting
        // over one property) are counted and reported, NEVER auto-resolved —
        // picking a winner is a design decision the tool must not guess.
        let mut edits_by_block: BTreeMap<usize, Vec<ValueEdit>> = BTreeMap::new();
        for diagnostic in &diagnostics {
            let local = diagnostic.range.start.line as usize;
            let group = match chunk.get(local) {
                Some(group) => group,
                None => continue,
            };
            if is_canonical(diagnostic) {
                let start = diagnostic
                    .range
                    .start
                    .character
                    .saturating_sub(CLASS_VALUE_COLUMN) as usize;
                let end = diagnostic
                    .range
                    .end
                    .character
                    .saturating_sub(CLASS_VALUE_COLUMN) as usize;
                if let Some(replacement) = canonical_replacement(&diagnostic.message) {
                    let edit = ValueEdit {
                        start,
                        end,
                        replacement,
                    };
                    edits_by_block.entry(local).or_default().push(edit);
                    // Stream the fix as it is applied.
                    if last_file.as_deref() != Some(group.file.as_path()) {
                        let rel = relative(&group.file, &root);
                        let _ =
                            writeln!(out, "\n{}{}{}", palette.bold, rel.display(), palette.reset);
                        last_file = Some(group.file.clone());
                    }
                    let _ = writeln!(
                        out,
                        "  {}fix{} {}{}{}  {}",
                        palette.warn,
                        palette.reset,
                        palette.dim,
                        group.line,
                        palette.reset,
                        diagnostic.message,
                    );
                    applied += 1;
                }
            } else if is_fatal(diagnostic) {
                conflicts += 1;
            }
        }

        for (local, group) in chunk.iter().enumerate() {
            let edits = edits_by_block.remove(&local).unwrap_or_default();
            let had_class_fix = !edits.is_empty();
            let resolved = resolve_classes(group, &edits);
            let text = corpus.file_texts.get(&group.file);
            let layout = text
                .map(|text| BlockLayout::at(text, group.list_span.start))
                .unwrap_or_default();
            let replacement = format_class_list(&resolved, &layout);
            // Rewrite when the class list changed OR the block is not already in
            // canonical layout (re-wrapping collapsed blocks rustfmt won't touch).
            let current = text
                .and_then(|text| text.get(group.list_span.clone()))
                .unwrap_or("");
            if replacement != current {
                rewrites
                    .entry(group.file.clone())
                    .or_default()
                    .push((group.list_span.clone(), replacement));
                if !had_class_fix {
                    reformatted += 1;
                }
            }
        }
        let _ = out.flush();
    }
    client.shutdown()?;

    for (path, mut spans) in rewrites {
        let mut text = corpus
            .file_texts
            .get(&path)
            .cloned()
            .context("file text missing")?;
        spans.sort_by_key(|(span, _)| std::cmp::Reverse(span.start));
        for (span, replacement) in spans {
            text.replace_range(span, &replacement);
        }
        std::fs::write(&path, text)?;
    }
    let _ = writeln!(
        out,
        "\n{bold}✓ {applied} class fix(es) applied{reset}, {reformatted} block(s) reformatted; \
         {err}{conflicts} conflict(s){reset} reported, NOT changed (resolve by hand — never guessed)",
        bold = palette.bold,
        reset = palette.reset,
        err = palette.error,
    );
    Ok(())
}

fn leading_whitespace(line: &str) -> String {
    line.chars()
        .take_while(|character| *character == ' ' || *character == '\t')
        .collect()
}

/// The indentation of the block's line and how many columns precede its class
/// list (`    base: tw![`), so a rewrite can match the surrounding layout.
#[derive(Default)]
struct BlockLayout {
    indent: String,
}

impl BlockLayout {
    fn at(text: &str, list_start: usize) -> Self {
        let line_start = text[..list_start]
            .rfind('\n')
            .map(|index| index + 1)
            .unwrap_or(0);
        Self {
            indent: leading_whitespace(&text[line_start..list_start]),
        }
    }
}

/// Render the class list back into the block with EVERY class on its own line,
/// indented one level past the block. rustfmt does not re-wrap custom-macro
/// bodies, so tw-lint emits this shape itself.
fn format_class_list(classes: &[String], layout: &BlockLayout) -> String {
    let item_indent = format!("{}    ", layout.indent);
    let mut rendered = String::new();
    for class in classes {
        rendered.push('\n');
        rendered.push_str(&item_indent);
        rendered.push('"');
        rendered.push_str(class);
        rendered.push_str("\",");
    }
    rendered.push('\n');
    rendered.push_str(&layout.indent);
    rendered
}

/// Compute a block's resolved class list: apply canonical value-edits and
/// conflict deletions, then drop exact duplicates (order-preserving).
fn resolve_classes(group: &ClassGroup, edits: &[ValueEdit]) -> Vec<String> {
    let value = group.classes.join(" ");
    let mut ordered: Vec<&ValueEdit> = edits.iter().collect();
    ordered.sort_by_key(|edit| std::cmp::Reverse(edit.start));
    let mut buffer: Vec<char> = value.chars().collect();
    for edit in ordered {
        let start = edit.start.min(buffer.len());
        let end = edit.end.min(buffer.len()).max(start);
        let replacement: Vec<char> = edit.replacement.chars().collect();
        buffer.splice(start..end, replacement);
    }
    let fixed: String = buffer.into_iter().collect();

    let mut seen = std::collections::HashSet::new();
    fixed
        .split_whitespace()
        .filter(|class| seen.insert(class.to_string()))
        .map(str::to_string)
        .collect()
}
