use crate::session::FileDiagnostics;
use lsp_types::DiagnosticSeverity;
use std::fmt::Write;

pub fn fatal_count(results: &[FileDiagnostics]) -> usize {
    results
        .iter()
        .flat_map(|f| f.diagnostics.iter())
        .filter(|d| match d.severity {
            Some(severity) => severity <= DiagnosticSeverity::WARNING,
            None => true,
        })
        .count()
}

pub fn render(results: &[FileDiagnostics]) -> String {
    let mut out = String::new();
    for file in results {
        for diagnostic in &file.diagnostics {
            let line = diagnostic.range.start.line + 1;
            let column = diagnostic.range.start.character + 1;
            let _ = writeln!(
                out,
                "{}:{}:{}  {}",
                file.path.display(),
                line,
                column,
                diagnostic.message
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::FileDiagnostics;
    use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};
    use std::path::PathBuf;

    fn diag(line: u32, sev: DiagnosticSeverity, msg: &str) -> Diagnostic {
        Diagnostic {
            range: Range::new(Position::new(line, 4), Position::new(line, 10)),
            severity: Some(sev),
            message: msg.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn counts_warning_and_error_as_fatal() {
        let results = vec![FileDiagnostics {
            path: PathBuf::from("src/a.rs"),
            diagnostics: vec![
                diag(1, DiagnosticSeverity::ERROR, "bad"),
                diag(2, DiagnosticSeverity::WARNING, "meh"),
                diag(3, DiagnosticSeverity::HINT, "fyi"),
            ],
        }];
        assert_eq!(fatal_count(&results), 2);
    }

    #[test]
    fn render_includes_path_and_line() {
        let results = vec![FileDiagnostics {
            path: PathBuf::from("src/a.rs"),
            diagnostics: vec![diag(0, DiagnosticSeverity::WARNING, "conflict")],
        }];
        let text = render(&results);
        assert!(text.contains("src/a.rs:1:5"));
        assert!(text.contains("conflict"));
    }
}
