use crate::cli::LintConfig;
use crate::groups::{ClassGroup, GroupMatcher};
use crate::lsp::client::Client;
use anyhow::{Context, Result};
use lsp_types::{Diagnostic, DiagnosticSeverity, Url};
use regex::Regex;
use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

/// Blocks per synthetic document. Only ever one document is open at a time (it
/// is reused via `didChange`), so this bounds the *per-validation* document
/// size, not total memory. The server debounces diagnostics 500ms per change,
/// so every chunk costs a fixed 500ms of latency; too-small chunks make that
/// debounce dominate (120 blocks → hundreds of chunks → tens of seconds of pure
/// waiting). Per-document validation cost, meanwhile, grows superlinearly with
/// block count and starts to bite past a few thousand blocks. ~2000 is the
/// empirical sweet spot that minimises `chunks × (500ms + validate(chunk))`
/// across both class-light and class-heavy corpora.
const CHUNK_SIZE: usize = 2000;

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

/// One reused synthetic document. Opening a single URI and updating it with
/// `didChange` per chunk (instead of a fresh URI each time) is what keeps the
/// server's memory bounded no matter how large the corpus is.
struct Synthetic {
    uri: Url,
    version: i32,
}

impl Synthetic {
    fn open(client: &mut Client, root: &Path) -> Result<Self> {
        let path = root.join("__twlint_synthetic.html");
        let uri =
            Url::from_file_path(&path).map_err(|_| anyhow::anyhow!("root is not absolute"))?;
        client.open_document(&uri, "html", "")?;
        Ok(Self { uri, version: 1 })
    }

    /// Push one chunk into the document and return its diagnostics. The server
    /// only ever holds this one document, so its footprint is independent of how
    /// many chunks (i.e. how many classes) the corpus has.
    fn diagnose(&mut self, client: &mut Client, chunk: &[ClassGroup]) -> Result<Vec<Diagnostic>> {
        let document = build_document(chunk);
        self.version += 1;
        client.change_document(&self.uri, &document, self.version)?;
        client.collect_diagnostics_for(&self.uri)
    }

    fn close(self, client: &mut Client) -> Result<()> {
        client.close_document(&self.uri)
    }
}

/// Refuse to report "clean" when nothing was extracted — a zero-block corpus
/// almost always means the `--class-container` regex does not match the macro
/// (e.g. it was written to match the whole block instead of just the opener),
/// and silently passing is the false-green that must never happen.
fn ensure_blocks_matched(total_blocks: usize) -> Result<()> {
    if total_blocks == 0 {
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

/// The two class names in a cssConflict message
/// (`'A' applies the same CSS properties as 'B'.` → `("A", "B")`).
fn conflict_pair(message: &str) -> Option<(String, String)> {
    let quoted = Regex::new(r"'([^']+)'").expect("valid regex");
    let mut names = quoted
        .captures_iter(message)
        .filter_map(|capture| capture.get(1).map(|name| name.as_str().to_string()));
    let first = names.next()?;
    let second = names.next()?;
    Some((first, second))
}

/// Print the file's path as a header the first time a diagnostic in it is shown.
fn print_file_header(
    out: &mut impl Write,
    palette: &Palette,
    root: &Path,
    group: &ClassGroup,
    last_file: &mut Option<PathBuf>,
) {
    if last_file.as_deref() != Some(group.file.as_path()) {
        let rel = relative(&group.file, root);
        let _ = writeln!(out, "\n{}{}{}", palette.bold, rel.display(), palette.reset);
        *last_file = Some(group.file.clone());
    }
}

/// Walk the configured source globs one file at a time, extracting its class
/// blocks, and hand each file to `visit`. Never holds more than the current
/// file's text and blocks in memory, so memory stays bounded no matter how large
/// the corpus is. Returns the total number of blocks seen (for the false-green
/// guard).
fn each_source_file<Visit>(
    config: &LintConfig,
    matcher: &GroupMatcher,
    root: &Path,
    mut visit: Visit,
) -> Result<usize>
where
    Visit: FnMut(PathBuf, String, Vec<ClassGroup>) -> Result<()>,
{
    let mut total_blocks = 0;
    for source_glob in &config.sources {
        let pattern = root.join(source_glob).to_string_lossy().into_owned();
        for entry in glob::glob(&pattern).context("invalid --source glob")? {
            let path = entry?;
            let text = std::fs::read_to_string(&path)?;
            let blocks = matcher.extract(&path, &text);
            if !blocks.is_empty() {
                total_blocks += blocks.len();
                visit(path, text, blocks)?;
            }
        }
    }
    Ok(total_blocks)
}

/// Report one chunk's diagnostics to stdout, updating the running tallies. The
/// conflict dedup is scoped to the chunk (each block lives in exactly one chunk),
/// so it stays bounded rather than accumulating across the whole corpus.
#[allow(clippy::too_many_arguments)]
fn report_check_chunk(
    client: &mut Client,
    document: &mut Synthetic,
    chunk: &[ClassGroup],
    root: &Path,
    palette: &Palette,
    out: &mut impl Write,
    last_file: &mut Option<PathBuf>,
    fixable: &mut usize,
    conflicts: &mut usize,
) -> Result<()> {
    let mut diagnostics = document.diagnose(client, chunk)?;
    diagnostics.sort_by_key(|diagnostic| {
        (
            diagnostic.range.start.line,
            diagnostic.range.start.character,
        )
    });
    // The LSP reports each conflict twice (A vs B and B vs A); collapse them
    // within this chunk (bounded — never grows with the corpus).
    let mut seen_conflicts: std::collections::HashSet<(PathBuf, u32, String)> =
        std::collections::HashSet::new();
    for diagnostic in &diagnostics {
        let local = diagnostic.range.start.line as usize;
        let group = match chunk.get(local) {
            Some(group) => group,
            None => continue,
        };

        if is_canonical(diagnostic) {
            print_file_header(out, palette, root, group, last_file);
            let _ = writeln!(
                out,
                "  {warn}fix{reset}  {dim}{}{reset}  {}",
                group.line,
                diagnostic.message,
                warn = palette.warn,
                reset = palette.reset,
                dim = palette.dim,
            );
            *fixable += 1;
        } else if is_fatal(diagnostic) {
            let (a, b) = match conflict_pair(&diagnostic.message) {
                Some(pair) => pair,
                None => continue,
            };
            let mut ordered = [a.clone(), b.clone()];
            ordered.sort();
            let key = (group.file.clone(), group.line, ordered.join("\u{0}"));
            if !seen_conflicts.insert(key) {
                continue;
            }
            print_file_header(out, palette, root, group, last_file);
            let _ = writeln!(
                out,
                "  {err}conflict{reset}  {dim}{}{reset}  {a}  ⟷  {b}",
                group.line,
                err = palette.error,
                reset = palette.reset,
                dim = palette.dim,
            );
            *conflicts += 1;
        }
    }
    let _ = out.flush();
    Ok(())
}

/// Stream diagnostics to stdout as each chunk completes; return the fatal count.
///
/// Memory is bounded regardless of corpus size: files are read one at a time,
/// blocks are batched into a rolling buffer that never exceeds one chunk, and a
/// single synthetic document is reused for every chunk so the language server
/// holds one document at most. A corpus of any size only takes *longer*, never
/// more memory.
pub fn run_join_check(config: &LintConfig) -> Result<usize> {
    let matcher = GroupMatcher::from_config(config)?;
    let root = std::fs::canonicalize(&config.root)?;
    let palette = Palette::detect();
    let mut client = Client::launch(config)?;
    let mut document = Synthetic::open(&mut client, &root)?;
    let mut out = std::io::stdout().lock();

    let mut fixable = 0;
    let mut conflicts = 0;
    let mut last_file: Option<PathBuf> = None;
    let mut buffer: Vec<ClassGroup> = Vec::new();

    let total_blocks = each_source_file(config, &matcher, &root, |_path, _text, mut blocks| {
        buffer.append(&mut blocks);
        while buffer.len() >= CHUNK_SIZE {
            let rest = buffer.split_off(CHUNK_SIZE);
            report_check_chunk(
                &mut client,
                &mut document,
                &buffer,
                &root,
                &palette,
                &mut out,
                &mut last_file,
                &mut fixable,
                &mut conflicts,
            )?;
            buffer = rest;
        }
        Ok(())
    })?;
    if !buffer.is_empty() {
        report_check_chunk(
            &mut client,
            &mut document,
            &buffer,
            &root,
            &palette,
            &mut out,
            &mut last_file,
            &mut fixable,
            &mut conflicts,
        )?;
    }

    document.close(&mut client)?;
    client.shutdown()?;
    ensure_blocks_matched(total_blocks)?;

    let total = fixable + conflicts;
    if total == 0 {
        let _ = writeln!(out, "\n{}✓ clean{}", palette.bold, palette.reset);
    } else {
        let _ = writeln!(
            out,
            "\n{bold}{total} issues{reset}  {warn}{fixable} fixable{reset}  {err}{conflicts} conflicts{reset}",
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

/// A block's identity across the two fix passes: its file plus the byte offset
/// where its class list starts (unique within a file), so pass 2 can look up the
/// edits pass 1 computed without holding every block in memory.
type BlockKey = (PathBuf, usize);

/// Diagnose one cross-file chunk, recording canonical value-edits keyed by block
/// and streaming a `fix` line for each. Conflicts are only counted here (they are
/// resolved by `--resolve`, not `--fix`).
#[allow(clippy::too_many_arguments)]
fn diagnose_fix_chunk(
    client: &mut Client,
    document: &mut Synthetic,
    chunk: &[ClassGroup],
    root: &Path,
    palette: &Palette,
    out: &mut impl Write,
    last_file: &mut Option<PathBuf>,
    edits_by_block: &mut BTreeMap<BlockKey, Vec<ValueEdit>>,
    applied: &mut usize,
    conflicts: &mut usize,
) -> Result<()> {
    let mut diagnostics = document.diagnose(client, chunk)?;
    diagnostics.sort_by_key(|diagnostic| {
        (
            diagnostic.range.start.line,
            diagnostic.range.start.character,
        )
    });
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
                edits_by_block
                    .entry((group.file.clone(), group.list_span.start))
                    .or_default()
                    .push(ValueEdit {
                        start,
                        end,
                        replacement,
                    });
                print_file_header(out, palette, root, group, last_file);
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
                *applied += 1;
            }
        } else if is_fatal(diagnostic) {
            *conflicts += 1;
        }
    }
    let _ = out.flush();
    Ok(())
}

/// Apply canonical suggestions (auto) and duplicate removal to every block,
/// streaming progress; rewrite each changed block in place.
///
/// Two passes, both streaming files one at a time. Pass 1 diagnoses blocks in
/// cross-file chunks — a *single* `didChange` per chunk, never per file — so the
/// server only ever sees a handful of document versions; diagnosing per file
/// (one version each) both pays the 500ms debounce per file and leaks server
/// memory until it OOMs on a large tree. Pass 2 re-reads each file and rewrites
/// it from the edits pass 1 recorded. Only the (sparse) edit map lives between
/// passes, so memory stays bounded no matter how large the corpus is.
pub fn run_join_fix(config: &LintConfig) -> Result<()> {
    let matcher = GroupMatcher::from_config(config)?;
    let root = std::fs::canonicalize(&config.root)?;
    let palette = Palette::detect();
    let mut client = Client::launch(config)?;
    let mut document = Synthetic::open(&mut client, &root)?;
    let mut out = std::io::stdout().lock();

    let mut applied = 0;
    let mut reformatted = 0;
    let mut conflicts = 0;
    let mut last_file: Option<PathBuf> = None;

    // Pass 1: diagnose, batching blocks across files into full chunks.
    let mut edits_by_block: BTreeMap<BlockKey, Vec<ValueEdit>> = BTreeMap::new();
    let mut buffer: Vec<ClassGroup> = Vec::new();
    let total_blocks = each_source_file(config, &matcher, &root, |_path, _text, mut blocks| {
        buffer.append(&mut blocks);
        while buffer.len() >= CHUNK_SIZE {
            let rest = buffer.split_off(CHUNK_SIZE);
            diagnose_fix_chunk(
                &mut client,
                &mut document,
                &buffer,
                &root,
                &palette,
                &mut out,
                &mut last_file,
                &mut edits_by_block,
                &mut applied,
                &mut conflicts,
            )?;
            buffer = rest;
        }
        Ok(())
    })?;
    if !buffer.is_empty() {
        diagnose_fix_chunk(
            &mut client,
            &mut document,
            &buffer,
            &root,
            &palette,
            &mut out,
            &mut last_file,
            &mut edits_by_block,
            &mut applied,
            &mut conflicts,
        )?;
    }

    document.close(&mut client)?;
    client.shutdown()?;
    ensure_blocks_matched(total_blocks)?;

    // Pass 2: re-read each file and rewrite it from the recorded edits. The files
    // are untouched until here, so re-extraction yields the same blocks (and the
    // same `list_span.start` keys) pass 1 saw.
    each_source_file(config, &matcher, &root, |path, text, blocks| {
        let mut rewrites: Vec<(std::ops::Range<usize>, String)> = Vec::new();
        for group in &blocks {
            let edits = edits_by_block
                .remove(&(path.clone(), group.list_span.start))
                .unwrap_or_default();
            let had_class_fix = !edits.is_empty();
            let resolved = resolve_classes(group, &edits);
            let layout = BlockLayout::at(&text, group.list_span.start);
            let replacement = format_class_list(&resolved, &layout);
            let current = text.get(group.list_span.clone()).unwrap_or("");
            if replacement != current {
                rewrites.push((group.list_span.clone(), replacement));
                if !had_class_fix {
                    reformatted += 1;
                }
            }
        }
        if !rewrites.is_empty() {
            let mut updated = text.clone();
            rewrites.sort_by_key(|(span, _)| std::cmp::Reverse(span.start));
            for (span, replacement) in rewrites {
                updated.replace_range(span, &replacement);
            }
            std::fs::write(&path, updated)?;
        }
        Ok(())
    })?;

    let _ = writeln!(
        out,
        "\n{bold}✓ {applied} fixed  {reformatted} reformatted  {err}{conflicts} conflicts left{reset}",
        bold = palette.bold,
        reset = palette.reset,
        err = palette.error,
    );
    Ok(())
}

/// Diagnose one cross-file chunk, recording deduped conflict pairs keyed by
/// block. The LSP reports each conflict twice (A vs B and B vs A); `seen`
/// collapses them.
fn diagnose_resolve_chunk(
    client: &mut Client,
    document: &mut Synthetic,
    chunk: &[ClassGroup],
    conflicts_by_block: &mut BTreeMap<BlockKey, Vec<(String, String)>>,
    seen: &mut std::collections::HashSet<(PathBuf, usize, String)>,
) -> Result<()> {
    let diagnostics = document.diagnose(client, chunk)?;
    for diagnostic in &diagnostics {
        if is_canonical(diagnostic) || !is_fatal(diagnostic) {
            continue;
        }
        let (a, b) = match conflict_pair(&diagnostic.message) {
            Some(pair) => pair,
            None => continue,
        };
        let local = diagnostic.range.start.line as usize;
        let group = match chunk.get(local) {
            Some(group) => group,
            None => continue,
        };
        let mut ordered = [a, b];
        ordered.sort();
        let key = (group.file.clone(), group.list_span.start);
        if seen.insert((key.0.clone(), key.1, ordered.join("\u{0}"))) {
            let [first, second] = ordered;
            conflicts_by_block
                .entry(key)
                .or_default()
                .push((first, second));
        }
    }
    Ok(())
}

/// Interactively resolve conflicts: show each one and let the user pick the
/// class to keep. The tool applies the choice and never guesses.
///
/// Two passes. Pass 1 diagnoses every block in cross-file chunks (one
/// `didChange` per chunk, never per file) and records the conflicts, then shuts
/// the server down — diagnosing per file would pay the 500ms debounce per file
/// and leak server memory until it OOMs, and it would also pin the server open
/// across the whole interactive session. Pass 2 re-reads each file and prompts
/// only for the blocks that have conflicts. Only the (sparse) conflict map lives
/// between passes, so memory stays bounded regardless of corpus size.
pub fn run_join_resolve(config: &LintConfig) -> Result<()> {
    let matcher = GroupMatcher::from_config(config)?;
    let root = std::fs::canonicalize(&config.root)?;
    let palette = Palette::detect();
    let mut client = Client::launch(config)?;
    let mut document = Synthetic::open(&mut client, &root)?;
    let stdin = std::io::stdin();

    let mut resolved = 0;
    let mut quit = false;

    // Pass 1: diagnose, batching blocks across files into full chunks.
    let mut conflicts_by_block: BTreeMap<BlockKey, Vec<(String, String)>> = BTreeMap::new();
    let mut seen: std::collections::HashSet<(PathBuf, usize, String)> =
        std::collections::HashSet::new();
    let mut buffer: Vec<ClassGroup> = Vec::new();
    let total_blocks = each_source_file(config, &matcher, &root, |_path, _text, mut blocks| {
        buffer.append(&mut blocks);
        while buffer.len() >= CHUNK_SIZE {
            let rest = buffer.split_off(CHUNK_SIZE);
            diagnose_resolve_chunk(
                &mut client,
                &mut document,
                &buffer,
                &mut conflicts_by_block,
                &mut seen,
            )?;
            buffer = rest;
        }
        Ok(())
    })?;
    if !buffer.is_empty() {
        diagnose_resolve_chunk(
            &mut client,
            &mut document,
            &buffer,
            &mut conflicts_by_block,
            &mut seen,
        )?;
    }

    document.close(&mut client)?;
    client.shutdown()?;
    ensure_blocks_matched(total_blocks)?;

    // Pass 2: re-read each file and prompt for its conflicting blocks. The files
    // are untouched until here, so re-extraction yields the same blocks (and the
    // same `list_span.start` keys) pass 1 recorded.
    each_source_file(config, &matcher, &root, |path, text, blocks| {
        if quit {
            return Ok(());
        }
        let mut rewrites: Vec<(std::ops::Range<usize>, String)> = Vec::new();
        for group in &blocks {
            if quit {
                break;
            }
            let pairs = match conflicts_by_block.remove(&(path.clone(), group.list_span.start)) {
                Some(pairs) => pairs,
                None => continue,
            };
            let mut remove: std::collections::HashSet<String> = std::collections::HashSet::new();
            for (a, b) in &pairs {
                if remove.contains(a) || remove.contains(b) {
                    continue;
                }
                let rel = relative(&group.file, &root);
                println!(
                    "{bold}{}:{}{reset}",
                    rel.display(),
                    group.line,
                    reset = palette.reset,
                    bold = palette.bold,
                );
                println!(
                    "    {warn}1{reset}  {a}",
                    warn = palette.warn,
                    reset = palette.reset
                );
                println!(
                    "    {warn}2{reset}  {b}",
                    warn = palette.warn,
                    reset = palette.reset
                );
                loop {
                    print!("  keep [1/2/s/q]: ");
                    let _ = std::io::stdout().flush();
                    let mut line = String::new();
                    if stdin.read_line(&mut line)? == 0 {
                        quit = true;
                        break;
                    }
                    match line.trim() {
                        "1" => {
                            remove.insert(b.clone());
                            break;
                        }
                        "2" => {
                            remove.insert(a.clone());
                            break;
                        }
                        "s" | "" => break,
                        "q" => {
                            quit = true;
                            break;
                        }
                        _ => continue,
                    }
                }
                println!();
                if quit {
                    break;
                }
            }
            if !remove.is_empty() {
                let kept: Vec<String> = group
                    .classes
                    .iter()
                    .filter(|class| !remove.contains(*class))
                    .cloned()
                    .collect();
                let layout = BlockLayout::at(&text, group.list_span.start);
                let replacement = format_class_list(&kept, &layout);
                rewrites.push((group.list_span.clone(), replacement));
                resolved += remove.len();
            }
        }

        if !rewrites.is_empty() {
            let mut updated = text.clone();
            rewrites.sort_by_key(|(span, _)| std::cmp::Reverse(span.start));
            for (span, replacement) in rewrites {
                updated.replace_range(span, &replacement);
            }
            std::fs::write(&path, updated)?;
        }
        Ok(())
    })?;

    if resolved == 0 {
        println!("{}✓ no conflicts resolved{}", palette.bold, palette.reset);
    } else {
        println!(
            "\n{bold}✓ removed {resolved} class(es){reset} across your choices",
            bold = palette.bold,
            reset = palette.reset,
        );
    }
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
