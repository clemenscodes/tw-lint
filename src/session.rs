use crate::cli::LintConfig;
use crate::lsp::client::Client;
use anyhow::{Context, Result};
use lsp_types::{Diagnostic, Url};
use std::path::{Path, PathBuf};

pub struct FileDiagnostics {
    pub path: PathBuf,
    pub diagnostics: Vec<Diagnostic>,
}

pub fn run_session(config: &LintConfig) -> Result<Vec<FileDiagnostics>> {
    let mut client = Client::launch(config)?;
    let root = std::fs::canonicalize(&config.root)?;
    let mut results = Vec::new();

    for source_glob in &config.sources {
        let pattern = root.join(source_glob);
        let pattern = pattern.to_string_lossy().into_owned();
        for entry in glob::glob(&pattern).context("invalid --source glob")? {
            let path = entry?;
            let uri = Url::from_file_path(&path)
                .map_err(|_| anyhow::anyhow!("non-absolute path {}", path.display()))?;
            let text = std::fs::read_to_string(&path)?;
            let language_id = language_id_for(config, &path);

            client.notify(
                "textDocument/didOpen",
                serde_json::json!({ "textDocument": {
                    "uri": uri, "languageId": language_id, "version": 1, "text": text } }),
            )?;
            // Barrier request; ignore its result, keep the diagnostics it flushed.
            let _ = client.request(
                "textDocument/documentColor",
                serde_json::json!({ "textDocument": { "uri": uri } }),
            );
            let mut collected = client.take_diagnostics();
            let diagnostics = collected
                .drain(..)
                .filter(|params| params.uri == uri)
                .flat_map(|params| params.diagnostics)
                .collect();
            results.push(FileDiagnostics { path, diagnostics });
        }
    }
    client.shutdown()?;
    Ok(results)
}

pub(crate) fn language_id_for(config: &LintConfig, path: &Path) -> String {
    let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    match extension {
        "rs" if config.include_languages.contains_key("rust") => "rust".to_string(),
        other => other.to_string(),
    }
}
