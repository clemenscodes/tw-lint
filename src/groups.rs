use crate::cli::{ClassRegex, LintConfig};
use anyhow::{anyhow, Result};
use regex::Regex;
use std::ops::Range;
use std::path::{Path, PathBuf};

/// One `tw![…]`-style block: the classes it contains and the byte span of the
/// argument list (between the macro's `[` and its matching `]`), so a fix can
/// rewrite exactly that region.
pub struct ClassGroup {
    pub file: PathBuf,
    pub line: u32,
    pub list_span: Range<usize>,
    pub classes: Vec<String>,
}

/// Finds class blocks: an `opener` regex locates each macro's opening bracket
/// (e.g. `tw!\s*\[`), a bracket scanner finds its matching close, and `item`
/// extracts each class from the content in between.
pub struct GroupMatcher {
    opener: Regex,
    item: Regex,
}

impl GroupMatcher {
    pub fn from_config(config: &LintConfig) -> Result<Self> {
        let source = config
            .class_regexes
            .iter()
            .find_map(|regex| match regex {
                ClassRegex::Container { container, class } => Some((container, class)),
                ClassRegex::Simple(_) => None,
            })
            .ok_or_else(|| anyhow!("--class-container is required (with a --class-regex)"))?;
        let opener = Regex::new(source.0)
            .map_err(|error| anyhow!("invalid --class-container regex: {error}"))?;
        let item =
            Regex::new(source.1).map_err(|error| anyhow!("invalid --class-regex: {error}"))?;
        Ok(Self { opener, item })
    }

    pub fn extract(&self, file: &Path, text: &str) -> Vec<ClassGroup> {
        let mut groups = Vec::new();
        for opener in self.opener.find_iter(text) {
            // The opener regex ends at the macro's `[`; scan for its match.
            let content_start = opener.end();
            let content_end = match matching_close(text, content_start) {
                Some(end) => end,
                None => continue,
            };
            let inner = &text[content_start..content_end];
            let classes: Vec<String> = self
                .item
                .captures_iter(inner)
                .filter_map(|capture| capture.get(1).map(|item| item.as_str().to_string()))
                .collect();
            if classes.is_empty() {
                continue;
            }
            let line = text[..opener.start()]
                .bytes()
                .filter(|&byte| byte == b'\n')
                .count()
                + 1;
            let group = ClassGroup {
                file: file.to_path_buf(),
                line: u32::try_from(line).unwrap_or(u32::MAX),
                list_span: content_start..content_end,
                classes,
            };
            groups.push(group);
        }
        groups
    }
}

/// Given the byte offset just past an opening `[`, return the offset of the `]`
/// that closes it. Brackets inside string literals are ignored (so class values
/// like `"[[role=button]]"` cannot be mistaken for the macro's close), and
/// nesting is balanced by depth — something a regex cannot do.
fn matching_close(text: &str, content_start: usize) -> Option<usize> {
    let mut depth: usize = 1;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, character) in text[content_start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        match character {
            '"' => in_string = true,
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(content_start + offset);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::LintConfig;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn matcher() -> GroupMatcher {
        let config = LintConfig {
            root: PathBuf::from("."),
            css: PathBuf::from("x.css"),
            sources: vec![],
            include_languages: BTreeMap::new(),
            class_regexes: vec![ClassRegex::Container {
                container: r"tw!\s*\[".into(),
                class: r#""([^"]*)""#.into(),
            }],
            server_command: "x".into(),
            node: None,
            fix: false,
            resolve: false,
        };
        GroupMatcher::from_config(&config).unwrap()
    }

    #[test]
    fn extracts_a_simple_block() {
        let text = "let _ = tw![\"flex\", \"gap-2\"];";
        let groups = matcher().extract(Path::new("a.rs"), text);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].classes, vec!["flex", "gap-2"]);
    }

    #[test]
    fn handles_nested_brackets_inside_a_class() {
        // The class contains `[[role=button]]` and other bracketed values — the
        // scanner must not close the macro on a bracket inside a string.
        let text = concat!(
            "let _ = tw![\n",
            "    \"mobile:**:[[role=button]]:touch-manipulation\",\n",
            "    \"w-[26cqi]\",\n",
            "];"
        );
        let groups = matcher().extract(Path::new("a.rs"), text);
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].classes,
            vec!["mobile:**:[[role=button]]:touch-manipulation", "w-[26cqi]"]
        );
        // The span must cover the whole list, ending at the real closing `]`.
        assert!(text[groups[0].list_span.clone()].contains("w-[26cqi]"));
    }

    #[test]
    fn finds_multiple_blocks() {
        let text = "a: tw![\"flex\"], b: tw![\"grid\", \"[&_x]:block\"]";
        let groups = matcher().extract(Path::new("a.rs"), text);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].classes, vec!["flex"]);
        assert_eq!(groups[1].classes, vec!["grid", "[&_x]:block"]);
    }
}
