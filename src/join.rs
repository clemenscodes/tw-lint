use crate::cli::LintConfig;
use crate::groups::{ClassGroup, GroupMatcher};
use crate::lsp::client::Client;
use crate::session::FileDiagnostics;
use anyhow::{Context, Result};
use lsp_types::{Diagnostic, Position, Range, Url};
use regex::Regex;
use std::collections::BTreeMap;
use std::path::PathBuf;

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

/// One synthetic HTML document: block `i` becomes a `class="…"` on line `i`,
/// so a diagnostic's line number maps straight back to a block.
fn build_document(groups: &[ClassGroup]) -> String {
    groups
        .iter()
        .map(|group| format!("<div class=\"{}\"></div>", group.classes.join(" ")))
        .collect::<Vec<_>>()
        .join("\n")
}

fn synthetic_uri(config: &LintConfig) -> Result<Url> {
    let root = std::fs::canonicalize(&config.root)?;
    let path = root.join("__twlint_synthetic.html");
    Url::from_file_path(&path).map_err(|_| anyhow::anyhow!("root is not absolute"))
}

fn diagnose(client: &mut Client, uri: &Url, document: String) -> Result<Vec<Diagnostic>> {
    client.notify(
        "textDocument/didOpen",
        serde_json::json!({ "textDocument": {
            "uri": uri, "languageId": "html", "version": 1, "text": document } }),
    )?;
    if client.supports_pull_diagnostics() {
        client.pull_diagnostics(uri)
    } else {
        let _ = client.request(
            "textDocument/documentColor",
            serde_json::json!({ "textDocument": { "uri": uri } }),
        );
        let collected = client
            .take_diagnostics()
            .into_iter()
            .filter(|params| &params.uri == uri)
            .flat_map(|params| params.diagnostics)
            .collect();
        Ok(collected)
    }
}

fn is_canonical(diagnostic: &Diagnostic) -> bool {
    matches!(
        &diagnostic.code,
        Some(lsp_types::NumberOrString::String(code)) if code == "suggestCanonicalClasses"
    )
}

/// Apply edits to a single line in isolation. Edit ranges are clamped to this
/// line (an end on a later line means "to end of line"), and applied
/// right-to-left so earlier edits never shift later offsets.
fn apply_line_edits(line: &str, line_index: u32, edits: &[lsp_types::TextEdit]) -> String {
    let mut ordered: Vec<&lsp_types::TextEdit> = edits.iter().collect();
    ordered.sort_by_key(|edit| std::cmp::Reverse(edit.range.start.character));
    let mut buffer: Vec<char> = line.chars().collect();
    for edit in ordered {
        if edit.range.start.line != line_index {
            continue;
        }
        let start = (edit.range.start.character as usize).min(buffer.len());
        let end = if edit.range.end.line == line_index {
            (edit.range.end.character as usize).min(buffer.len())
        } else {
            buffer.len()
        }
        .max(start);
        let replacement: Vec<char> = edit.new_text.chars().collect();
        buffer.splice(start..end, replacement);
    }
    buffer.into_iter().collect()
}

pub fn run_join_check(config: &LintConfig) -> Result<Vec<FileDiagnostics>> {
    let matcher = GroupMatcher::from_config(config)?;
    let corpus = collect_corpus(config, &matcher)?;
    let document = build_document(&corpus.groups);
    let uri = synthetic_uri(config)?;

    let mut client = Client::launch(config)?;
    let diagnostics = diagnose(&mut client, &uri, document)?;
    client.shutdown()?;

    // Each diagnostic's line is the block index. Re-anchor it to the real
    // `tw![…]` location so the report points at source.
    let mut per_file: BTreeMap<PathBuf, Vec<Diagnostic>> = BTreeMap::new();
    for diagnostic in diagnostics {
        let index = usize::try_from(diagnostic.range.start.line).unwrap_or(usize::MAX);
        let group = match corpus.groups.get(index) {
            Some(group) => group,
            None => continue,
        };
        let anchor = group.line.saturating_sub(1);
        let mut anchored = diagnostic;
        anchored.range = Range::new(Position::new(anchor, 0), Position::new(anchor, 0));
        per_file
            .entry(group.file.clone())
            .or_default()
            .push(anchored);
    }
    let results = per_file
        .into_iter()
        .map(|(path, diagnostics)| FileDiagnostics { path, diagnostics })
        .collect();
    Ok(results)
}

pub fn run_join_fix(config: &LintConfig) -> Result<()> {
    let matcher = GroupMatcher::from_config(config)?;
    let corpus = collect_corpus(config, &matcher)?;
    let document = build_document(&corpus.groups);
    let uri = synthetic_uri(config)?;

    let mut client = Client::launch(config)?;
    let diagnostics = diagnose(&mut client, &uri, document.clone())?;

    // Collect code-action edits PER synthetic line. Only canonical suggestions
    // are safely auto-fixable — a conflict's resolution (which class to drop) is
    // a human decision, so those are reported, never rewritten. Edits are kept
    // line-local because some LSP edit ranges end at the next line's column 0;
    // applied globally they would merge lines and corrupt every later block.
    let mut edits_by_line: BTreeMap<u32, Vec<lsp_types::TextEdit>> = BTreeMap::new();
    for diagnostic in &diagnostics {
        if !is_canonical(diagnostic) {
            continue;
        }
        let line = diagnostic.range.start.line;
        for action in client.code_actions(&uri, diagnostic)? {
            if let lsp_types::CodeActionOrCommand::CodeAction(code_action) = action {
                if let Some(edit) = code_action.edit {
                    if let Some(changes) = edit.changes {
                        if let Some(text_edits) = changes.get(&uri) {
                            edits_by_line
                                .entry(line)
                                .or_default()
                                .extend(text_edits.iter().cloned());
                        }
                    }
                }
            }
        }
    }
    client.shutdown()?;

    if edits_by_line.is_empty() {
        return Ok(());
    }

    // Rebuild each affected line by applying its own edits in isolation.
    let class_attribute = Regex::new(r#"class="([^"]*)""#).expect("valid regex");
    let mut fixed_lines: Vec<String> = document.split('\n').map(str::to_string).collect();
    for (line, edits) in &edits_by_line {
        let index = *line as usize;
        if let Some(text) = fixed_lines.get(index) {
            fixed_lines[index] = apply_line_edits(text, *line, edits);
        }
    }
    let fixed_lines: Vec<&str> = fixed_lines.iter().map(String::as_str).collect();

    // Collect per-file rewrites: (list_span, replacement) for changed blocks.
    let mut rewrites: BTreeMap<PathBuf, Vec<(std::ops::Range<usize>, String)>> = BTreeMap::new();
    for (index, group) in corpus.groups.iter().enumerate() {
        let line = match fixed_lines.get(index) {
            Some(line) => *line,
            None => continue,
        };
        let value = match class_attribute.captures(line).and_then(|c| c.get(1)) {
            Some(value) => value.as_str(),
            None => continue,
        };
        let new_classes: Vec<&str> = value.split_whitespace().collect();
        let original: Vec<&str> = group.classes.iter().map(String::as_str).collect();
        if new_classes == original {
            continue;
        }
        let replacement = new_classes
            .iter()
            .map(|class| format!("\"{class}\""))
            .collect::<Vec<_>>()
            .join(", ");
        rewrites
            .entry(group.file.clone())
            .or_default()
            .push((group.list_span.clone(), replacement));
    }

    for (path, mut spans) in rewrites {
        let mut text = corpus
            .file_texts
            .get(&path)
            .cloned()
            .context("file text missing")?;
        // Apply end-to-start so earlier edits don't shift later spans.
        spans.sort_by_key(|(span, _)| std::cmp::Reverse(span.start));
        for (span, replacement) in spans {
            text.replace_range(span, &replacement);
        }
        std::fs::write(&path, text)?;
    }
    Ok(())
}
