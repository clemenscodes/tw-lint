use crate::cli::{ClassRegex, LintConfig};
use serde_json::{json, Value};

/// The `tailwindCSS` configuration object the server pulls via
/// `workspace/configuration`. Keys verified against the server in the Task 3
/// spike (see `docs/lsp-findings.md`); adjust here if the spike shows
/// different names.
pub fn tailwind_settings(config: &LintConfig) -> Value {
    // Join mode lints a synthetic HTML document with real `class="…"`
    // attributes, so no custom extraction is needed — omit classRegex and
    // includeLanguages (which would otherwise also match the synthetic markup).
    let experimental = if config.uses_container() {
        json!({ "configFile": config.css.to_string_lossy() })
    } else {
        let class_regex: Vec<Value> = config
            .class_regexes
            .iter()
            .map(|regex| match regex {
                ClassRegex::Simple(pattern) => Value::String(pattern.clone()),
                ClassRegex::Container { container, class } => json!([container, class]),
            })
            .collect();
        json!({
            "classRegex": class_regex,
            "configFile": config.css.to_string_lossy(),
        })
    };

    let include_languages = if config.uses_container() {
        json!({})
    } else {
        json!(config.include_languages)
    };

    json!({
        "includeLanguages": include_languages,
        "experimental": experimental,
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
