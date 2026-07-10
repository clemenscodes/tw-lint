use crate::cli::{ClassRegex, LintConfig};
use serde_json::{json, Value};

/// The `tailwindCSS` configuration object the server pulls via
/// `workspace/configuration`. Keys verified against the server in the Task 3
/// spike (see `docs/lsp-findings.md`); adjust here if the spike shows
/// different names.
pub fn tailwind_settings(config: &LintConfig) -> Value {
    let class_regex: Vec<Value> = config
        .class_regexes
        .iter()
        .map(|regex| match regex {
            ClassRegex::Simple(pattern) => Value::String(pattern.clone()),
            ClassRegex::Container { container, class } => json!([container, class]),
        })
        .collect();

    json!({
        "includeLanguages": config.include_languages,
        "experimental": {
            "classRegex": class_regex,
            "configFile": config.css.to_string_lossy(),
        },
        "lint": {
            "cssConflict": "warning",
            "invalidApply": "error",
            "invalidScreen": "error",
            "invalidVariant": "error",
            "invalidConfigPath": "error",
            "invalidTailwindDirective": "error",
            "recommendedVariantOrder": "warning"
        },
        "validate": true
    })
}
