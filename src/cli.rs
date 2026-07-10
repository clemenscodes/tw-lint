use anyhow::{bail, Context, Result};
use clap::Parser;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "tw-lint", about = "Tailwind LSP-driven linter/fixer")]
pub struct CliArgs {
    #[arg(long)]
    pub config: Option<PathBuf>,
    #[arg(long)]
    pub root: Option<PathBuf>,
    #[arg(long)]
    pub css: Option<PathBuf>,
    #[arg(long = "source")]
    pub source: Vec<String>,
    #[arg(long = "include-lang")]
    pub include_lang: Vec<String>,
    #[arg(long = "class-regex")]
    pub class_regex: Vec<String>,
    #[arg(long)]
    pub server: Option<String>,
    #[arg(long)]
    pub node: Option<PathBuf>,
    #[arg(long)]
    pub fix: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClassRegex {
    Simple(String),
    Container { container: String, class: String },
}

#[derive(Debug, Clone)]
pub struct LintConfig {
    pub root: PathBuf,
    pub css: PathBuf,
    pub sources: Vec<String>,
    pub include_languages: BTreeMap<String, String>,
    pub class_regexes: Vec<ClassRegex>,
    pub server_command: String,
    pub node: Option<PathBuf>,
    pub fix: bool,
}

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    root: Option<PathBuf>,
    css: Option<PathBuf>,
    #[serde(default)]
    source: Vec<String>,
    #[serde(default)]
    include_lang: Vec<String>,
    #[serde(default)]
    class_regex: Vec<String>,
    server: Option<String>,
    node: Option<PathBuf>,
}

impl LintConfig {
    pub fn resolve(args: CliArgs) -> Result<Self> {
        let file = match &args.config {
            Some(path) => {
                let text = std::fs::read_to_string(path)
                    .with_context(|| format!("reading config {}", path.display()))?;
                toml::from_str(&text).context("parsing config toml")?
            }
            None => FileConfig::default(),
        };

        let root = args
            .root
            .or(file.root)
            .unwrap_or_else(|| PathBuf::from("."));
        let css = args
            .css
            .or(file.css)
            .context("--css (or css in config) is required")?;

        let sources = if args.source.is_empty() {
            file.source
        } else {
            args.source
        };
        if sources.is_empty() {
            bail!("at least one --source glob (or config source) is required");
        }

        let include_raw = if args.include_lang.is_empty() {
            file.include_lang
        } else {
            args.include_lang
        };
        let mut include_languages = BTreeMap::new();
        for entry in include_raw {
            let (id, served) = entry
                .split_once('=')
                .with_context(|| format!("--include-lang must be id=served, got `{entry}`"))?;
            include_languages.insert(id.to_string(), served.to_string());
        }

        let regex_raw = if args.class_regex.is_empty() {
            file.class_regex
        } else {
            args.class_regex
        };
        let class_regexes = regex_raw.into_iter().map(ClassRegex::Simple).collect();

        let server_command = args
            .server
            .or(file.server)
            .or_else(|| std::env::var("TW_LINT_SERVER").ok())
            .unwrap_or_else(|| "tailwindcss-language-server".to_string());
        let node = args.node.or(file.node);

        Ok(Self {
            root,
            css,
            sources,
            include_languages,
            class_regexes,
            server_command,
            node,
            fix: args.fix,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn args() -> CliArgs {
        CliArgs {
            config: None,
            root: Some(PathBuf::from("/proj")),
            css: Some(PathBuf::from("tw.css")),
            source: vec!["src/**/*.rs".into()],
            include_lang: vec!["rust=html".into()],
            class_regex: vec![r#"tw!\s*\[([^\]]*)\]"#.into(), r#""([^"]*)""#.into()],
            server: None,
            node: None,
            fix: false,
        }
    }

    #[test]
    fn flags_only_resolve_without_a_config_file() {
        let resolved = LintConfig::resolve(args()).unwrap();
        assert_eq!(resolved.css, PathBuf::from("tw.css"));
        assert_eq!(resolved.sources, vec!["src/**/*.rs".to_string()]);
        assert_eq!(resolved.include_languages.get("rust").unwrap(), "html");
        assert_eq!(resolved.class_regexes.len(), 2);
        assert!(!resolved.fix);
    }

    #[test]
    fn server_defaults_but_flags_override_node_and_server() {
        std::env::remove_var("TW_LINT_SERVER");
        let default = LintConfig::resolve(args()).unwrap();
        assert_eq!(default.server_command, "tailwindcss-language-server");
        assert!(default.node.is_none());

        let mut a = args();
        a.server = Some("/opt/my-tw-ls/server.js".into());
        a.node = Some(PathBuf::from("/opt/node20/bin/node"));
        let overridden = LintConfig::resolve(a).unwrap();
        assert_eq!(overridden.server_command, "/opt/my-tw-ls/server.js");
        assert_eq!(
            overridden.node.unwrap(),
            PathBuf::from("/opt/node20/bin/node")
        );
    }

    #[test]
    fn include_lang_without_equation_is_an_error() {
        let mut a = args();
        a.include_lang = vec!["rust".into()];
        assert!(LintConfig::resolve(a).is_err());
    }
}
