use std::collections::BTreeMap;
use std::path::PathBuf;

use tw_lint::cli::{ClassRegex, LintConfig};
use tw_lint::lsp::client::Client;

fn fixture_config() -> LintConfig {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/project");
    let mut include = BTreeMap::new();
    include.insert("rust".to_string(), "html".to_string());
    LintConfig {
        css: PathBuf::from("tailwind.input.css"),
        sources: vec!["src/**/*.rs".to_string()],
        include_languages: include,
        class_regexes: vec![
            ClassRegex::Simple(r#"tw!\s*\[([^\]]*)\]"#.to_string()),
            ClassRegex::Simple(r#""([^"]*)""#.to_string()),
        ],
        // In `nix develop` the bundled binary is on PATH; override with
        // --server/--node in real use.
        server_command: "tailwindcss-language-server".to_string(),
        node: None,
        fix: false,
        resolve: false,
        root,
    }
}

#[test]
#[ignore = "requires tailwindcss-language-server on PATH (run under `nix develop`)"]
fn noncanonical_class_produces_a_canonical_suggestion_diagnostic() {
    let config = fixture_config();
    let mut client = Client::launch(&config).unwrap();

    let sample = config.root.join("src/sample.rs");
    let uri = lsp_types::Url::from_file_path(&sample).unwrap();
    let text = std::fs::read_to_string(&sample).unwrap();
    client
        .notify(
            "textDocument/didOpen",
            serde_json::json!({ "textDocument": {
                "uri": uri, "languageId": "rust", "version": 1, "text": text } }),
        )
        .unwrap();

    // Barrier: a request the server answers only after processing the didOpen,
    // by which point publishDiagnostics for this document has already been sent.
    let _ = client.request(
        "textDocument/documentColor",
        serde_json::json!({ "textDocument": { "uri": uri } }),
    );

    let diagnostics = client.take_diagnostics();
    let messages: Vec<String> = diagnostics
        .iter()
        .flat_map(|d| d.diagnostics.iter().map(|x| x.message.clone()))
        .collect();
    // Canonical suggestion is delivered as a diagnostic (see docs/lsp-findings.md):
    // `w-[100%]` -> `w-full`.
    assert!(
        messages.iter().any(|m| m.contains("w-full")),
        "expected a canonical-suggestion diagnostic (w-[100%] -> w-full), got {messages:?}"
    );
    client.shutdown().unwrap();
}
