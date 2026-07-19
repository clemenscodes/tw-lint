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
        // Scan a comment-masked copy so a quoted string or a stray bracket that
        // only appears in a Rust comment inside the block is never mistaken for a
        // class or for the macro's closing `]`. The mask keeps byte length, so
        // every offset below still points into the original text.
        let masked = mask_comments(text);
        let mut groups = Vec::new();
        for opener in self.opener.find_iter(&masked) {
            // The opener regex ends at the macro's `[`; scan for its match.
            let content_start = opener.end();
            let content_end = match matching_close(&masked, content_start) {
                Some(end) => end,
                None => continue,
            };
            let inner = &masked[content_start..content_end];
            let classes: Vec<String> = self
                .item
                .captures_iter(inner)
                .filter_map(|capture| capture.get(1).map(|item| item.as_str().to_string()))
                .collect();
            if classes.is_empty() {
                continue;
            }
            let line = masked[..opener.start()]
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

/// Return a copy of `text` with the bytes of every Rust comment replaced by
/// spaces, keeping newlines and total length so byte offsets stay valid. String
/// literals are left untouched, a `//` or `/*` inside a string does not open a
/// comment, and block comments nest the way Rust's do. Non-ASCII bytes inside a
/// comment become spaces one byte at a time, which stays valid UTF-8 because a
/// space is one byte.
fn mask_comments(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = text.as_bytes().to_vec();
    let mut index = 0;
    let mut in_string = false;
    let mut escaped = false;
    let mut block_depth: usize = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if block_depth > 0 {
            if byte == b'/' && bytes.get(index + 1) == Some(&b'*') {
                out[index] = b' ';
                out[index + 1] = b' ';
                block_depth += 1;
                index += 2;
                continue;
            }
            if byte == b'*' && bytes.get(index + 1) == Some(&b'/') {
                out[index] = b' ';
                out[index + 1] = b' ';
                block_depth -= 1;
                index += 2;
                continue;
            }
            if byte != b'\n' {
                out[index] = b' ';
            }
            index += 1;
            continue;
        }
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if byte == b'"' {
            in_string = true;
            index += 1;
            continue;
        }
        if byte == b'/' && bytes.get(index + 1) == Some(&b'/') {
            let mut scan = index;
            while scan < bytes.len() && bytes[scan] != b'\n' {
                out[scan] = b' ';
                scan += 1;
            }
            index = scan;
            continue;
        }
        if byte == b'/' && bytes.get(index + 1) == Some(&b'*') {
            out[index] = b' ';
            out[index + 1] = b' ';
            block_depth = 1;
            index += 2;
            continue;
        }
        index += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| text.to_string())
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
    fn ignores_quoted_strings_in_a_line_comment() {
        // A `//` comment inside the block mentions "Backspace" and "Num7" in
        // quotes. Those are prose, not classes, and must not be extracted — the
        // bug that promoted them to real classes on --fix.
        let text = concat!(
            "let _ = tw![\n",
            "    \"mobile:pb-6\",\n",
            "    // labels run to \"Backspace\" and \"Num7\", so size off those\n",
            "    \"mobile:[--key-slot:13.7cqi]\",\n",
            "];"
        );
        let groups = matcher().extract(Path::new("a.rs"), text);
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].classes,
            vec!["mobile:pb-6", "mobile:[--key-slot:13.7cqi]"]
        );
    }

    #[test]
    fn ignores_brackets_and_quotes_in_a_block_comment() {
        // A `/* */` comment carrying an unbalanced `]` and a lone `"` must not
        // close the macro early or open a phantom string.
        let text = concat!(
            "let _ = tw![\n",
            "    \"flex\",\n",
            "    /* danger: a ] and a lone \" live here */\n",
            "    \"gap-2\",\n",
            "];"
        );
        let groups = matcher().extract(Path::new("a.rs"), text);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].classes, vec!["flex", "gap-2"]);
    }

    #[test]
    fn a_commented_out_opener_starts_no_block() {
        let text = concat!("// let _ = tw![\"ghost\"];\n", "let _ = tw![\"flex\"];");
        let groups = matcher().extract(Path::new("a.rs"), text);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].classes, vec!["flex"]);
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
