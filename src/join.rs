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
            "\n{bold}{total} issue(s){reset}: {warn}{fixable} canonical{reset}, \
             {err}{conflicts} conflict(s){reset} — all cleared by --fix \
             (conflicts resolved by keeping the last class; review the diff)",
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
    let palette = Palette::detect();
    let total_blocks = corpus.groups.len();
    let mut client = Client::launch(config)?;
    let mut out = std::io::stdout().lock();

    // Per file: (list byte-span, replacement source) for every changed block.
    let mut rewrites: BTreeMap<PathBuf, Vec<(std::ops::Range<usize>, String)>> = BTreeMap::new();
    let mut fixed_blocks = 0;
    let mut conflicts = 0;
    let mut scanned = 0;

    for (chunk_index, chunk) in corpus.groups.chunks(CHUNK_SIZE).enumerate() {
        let document = build_document(chunk);
        let uri = chunk_uri(config, chunk_index)?;
        let diagnostics = diagnose(&mut client, &uri, document)?;

        // Canonical edits per block (in class-value coordinates).
        let mut edits_by_block: BTreeMap<usize, Vec<ValueEdit>> = BTreeMap::new();
        let mut conflicts_by_block: BTreeMap<usize, Vec<ConflictHit>> = BTreeMap::new();
        for diagnostic in &diagnostics {
            let local = diagnostic.range.start.line as usize;
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
            if is_canonical(diagnostic) {
                if let Some(replacement) = canonical_replacement(&diagnostic.message) {
                    let edit = ValueEdit {
                        start,
                        end,
                        replacement,
                    };
                    edits_by_block.entry(local).or_default().push(edit);
                }
            } else if is_fatal(diagnostic) {
                if let Some(hit) = ConflictHit::parse(&diagnostic.message, start, end) {
                    conflicts_by_block.entry(local).or_default().push(hit);
                }
            }
        }

        for (local, group) in chunk.iter().enumerate() {
            let mut edits = edits_by_block.remove(&local).unwrap_or_default();
            let block_conflicts = conflicts_by_block.remove(&local).unwrap_or_default();
            let deletions = conflict_deletions(&block_conflicts);
            conflicts += deletions.len();
            merge_conflict_deletions(&mut edits, deletions);
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
                fixed_blocks += 1;
            }
        }
        scanned += chunk.len();
        let _ = write!(
            out,
            "\r{}scanning {scanned}/{total_blocks} blocks — {fixed_blocks} fixed{}",
            palette.dim, palette.reset
        );
        let _ = out.flush();
    }
    client.shutdown()?;
    let _ = writeln!(out);

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
        "{bold}✓ updated {fixed_blocks} block(s){reset} \
         ({conflicts} conflict(s) resolved by keeping the last conflicting class)",
        bold = palette.bold,
        reset = palette.reset,
    );
    Ok(())
}

/// One `cssConflict` diagnostic: the subject class (with its span in the class
/// value) and the other classes it collides with.
struct ConflictHit {
    name: String,
    start: usize,
    end: usize,
    others: Vec<String>,
}

impl ConflictHit {
    /// Parse ``'A' applies the same CSS properties as 'B'[ and 'C'…].``
    fn parse(message: &str, start: usize, end: usize) -> Option<Self> {
        let quoted = Regex::new(r"'([^']+)'").expect("valid regex");
        let mut names = quoted
            .captures_iter(message)
            .filter_map(|capture| capture.get(1).map(|m| m.as_str().to_string()));
        let name = names.next()?;
        let others: Vec<String> = names.collect();
        if others.is_empty() {
            return None;
        }
        Some(Self {
            name,
            start,
            end,
            others,
        })
    }
}

/// Resolve conflicts by keeping, in each set of mutually-conflicting classes,
/// the one that appears LAST in source order and deleting the earlier ones.
/// Returns deletion edits (empty replacement) for the losers.
fn conflict_deletions(hits: &[ConflictHit]) -> Vec<ValueEdit> {
    use std::collections::{HashMap, HashSet};
    let mut span: HashMap<&str, (usize, usize)> = HashMap::new();
    let mut adjacency: HashMap<&str, HashSet<&str>> = HashMap::new();
    for hit in hits {
        span.insert(&hit.name, (hit.start, hit.end));
        for other in &hit.others {
            adjacency.entry(&hit.name).or_default().insert(other);
            adjacency.entry(other).or_default().insert(&hit.name);
        }
    }

    let mut visited: HashSet<&str> = HashSet::new();
    let mut deletions = Vec::new();
    for hit in hits {
        let root: &str = &hit.name;
        if visited.contains(root) {
            continue;
        }
        // Collect the connected component (all classes fighting the same property).
        let mut component = Vec::new();
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            if !visited.insert(node) {
                continue;
            }
            component.push(node);
            if let Some(neighbours) = adjacency.get(node) {
                for neighbour in neighbours {
                    if !visited.contains(neighbour) {
                        stack.push(neighbour);
                    }
                }
            }
        }
        // Keep the class that appears last (largest start); delete the rest.
        let keeper = component
            .iter()
            .filter_map(|name| span.get(name).map(|(start, _)| (*start, *name)))
            .max();
        let keeper = match keeper {
            Some((_, name)) => name,
            None => continue,
        };
        for name in component {
            if name == keeper {
                continue;
            }
            if let Some((start, end)) = span.get(name) {
                deletions.push(ValueEdit {
                    start: *start,
                    end: *end,
                    replacement: String::new(),
                });
            }
        }
    }
    deletions
}

/// Merge conflict deletions into the canonical edits, dropping any canonical
/// edit whose span overlaps a deletion (the class is being removed anyway).
fn merge_conflict_deletions(edits: &mut Vec<ValueEdit>, deletions: Vec<ValueEdit>) {
    edits.retain(|edit| {
        !deletions
            .iter()
            .any(|deletion| edit.start < deletion.end && deletion.start < edit.end)
    });
    edits.extend(deletions);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_conflict_message() {
        let hit = ConflictHit::parse(
            "'text-transparent' applies the same CSS properties as 'text-[0]'.",
            5,
            21,
        )
        .unwrap();
        assert_eq!(hit.name, "text-transparent");
        assert_eq!(hit.others, vec!["text-[0]".to_string()]);
    }

    #[test]
    fn keeps_the_last_conflicting_class() {
        // "a" at column 0 conflicts with "b" at column 10; keep b (later), delete a.
        let hits = vec![
            ConflictHit {
                name: "a".into(),
                start: 0,
                end: 1,
                others: vec!["b".into()],
            },
            ConflictHit {
                name: "b".into(),
                start: 10,
                end: 11,
                others: vec!["a".into()],
            },
        ];
        let deletions = conflict_deletions(&hits);
        assert_eq!(deletions.len(), 1);
        assert_eq!(deletions[0].start, 0);
        assert_eq!(deletions[0].end, 1);
        assert!(deletions[0].replacement.is_empty());
    }

    #[test]
    fn three_way_conflict_keeps_only_the_last() {
        let hits = vec![
            ConflictHit {
                name: "a".into(),
                start: 0,
                end: 1,
                others: vec!["b".into(), "c".into()],
            },
            ConflictHit {
                name: "b".into(),
                start: 5,
                end: 6,
                others: vec!["a".into(), "c".into()],
            },
            ConflictHit {
                name: "c".into(),
                start: 9,
                end: 10,
                others: vec!["a".into(), "b".into()],
            },
        ];
        let mut deletions = conflict_deletions(&hits);
        deletions.sort_by_key(|edit| edit.start);
        assert_eq!(deletions.len(), 2);
        assert_eq!(deletions[0].start, 0);
        assert_eq!(deletions[1].start, 5);
    }
}
