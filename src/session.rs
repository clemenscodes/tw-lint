use crate::cli::LintConfig;
use crate::lsp::client::Client;
use anyhow::{Context, Result};
use lsp_types::{Diagnostic, Url};
use std::path::{Path, PathBuf};

pub struct FileDiagnostics {
    pub path: PathBuf,
    pub diagnostics: Vec<Diagnostic>,
}

struct OpenedDocument {
    uri: Url,
    text: String,
    diagnostics: Vec<Diagnostic>,
}

/// Open a document and collect its diagnostics. Prefers pull diagnostics
/// (deterministic); falls back to a push barrier when the server has no
/// diagnostic provider.
fn open_and_diagnose(
    client: &mut Client,
    config: &LintConfig,
    path: &Path,
) -> Result<OpenedDocument> {
    let uri = Url::from_file_path(path)
        .map_err(|_| anyhow::anyhow!("non-absolute path {}", path.display()))?;
    let text = std::fs::read_to_string(path)?;
    let language_id = language_id_for(config, path);
    client.notify(
        "textDocument/didOpen",
        serde_json::json!({ "textDocument": {
            "uri": uri, "languageId": language_id, "version": 1, "text": text } }),
    )?;

    let diagnostics = if client.supports_pull_diagnostics() {
        client.pull_diagnostics(&uri)?
    } else {
        // Fallback: a barrier request the server answers only after processing
        // the didOpen, then drain whatever it published for this document.
        let _ = client.request(
            "textDocument/documentColor",
            serde_json::json!({ "textDocument": { "uri": uri } }),
        );
        client
            .take_diagnostics()
            .into_iter()
            .filter(|params| params.uri == uri)
            .flat_map(|params| params.diagnostics)
            .collect()
    };

    Ok(OpenedDocument {
        uri,
        text,
        diagnostics,
    })
}

fn each_source<F>(config: &LintConfig, mut visit: F) -> Result<()>
where
    F: FnMut(PathBuf) -> Result<()>,
{
    let root = std::fs::canonicalize(&config.root)?;
    for source_glob in &config.sources {
        let pattern = root.join(source_glob).to_string_lossy().into_owned();
        for entry in glob::glob(&pattern).context("invalid --source glob")? {
            visit(entry?)?;
        }
    }
    Ok(())
}

pub fn run_session(config: &LintConfig) -> Result<Vec<FileDiagnostics>> {
    let mut client = Client::launch(config)?;
    let mut results = Vec::new();
    each_source(config, |path| {
        let opened = open_and_diagnose(&mut client, config, &path)?;
        client.close_document(&opened.uri)?;
        results.push(FileDiagnostics {
            path,
            diagnostics: opened.diagnostics,
        });
        Ok(())
    })?;
    client.shutdown()?;
    Ok(results)
}

pub fn run_fix(config: &LintConfig) -> Result<()> {
    let mut client = Client::launch(config)?;
    each_source(config, |path| {
        let opened = open_and_diagnose(&mut client, config, &path)?;

        // Gather every edit targeting this file from all code actions, then
        // apply them in one batch (edits are all in original coordinates).
        let mut file_edits: Vec<lsp_types::TextEdit> = Vec::new();
        for diagnostic in &opened.diagnostics {
            for action in client.code_actions(&opened.uri, diagnostic)? {
                if let lsp_types::CodeActionOrCommand::CodeAction(code_action) = action {
                    if let Some(edit) = code_action.edit {
                        if let Some(changes) = edit.changes {
                            if let Some(edits) = changes.get(&opened.uri) {
                                file_edits.extend(edits.iter().cloned());
                            }
                        }
                    }
                }
            }
        }
        if !file_edits.is_empty() {
            let updated = crate::edits::apply_text_edits(&opened.text, &file_edits);
            std::fs::write(&path, updated)?;
        }
        client.close_document(&opened.uri)?;
        Ok(())
    })?;
    client.shutdown()?;
    Ok(())
}

pub(crate) fn language_id_for(config: &LintConfig, path: &Path) -> String {
    let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    match extension {
        "rs" if config.include_languages.contains_key("rust") => "rust".to_string(),
        other => other.to_string(),
    }
}
