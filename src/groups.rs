use crate::cli::{ClassRegex, LintConfig};
use anyhow::{anyhow, Result};
use regex::Regex;
use std::ops::Range;
use std::path::{Path, PathBuf};

/// One `tw![…]`-style block: the classes it contains and the byte span of the
/// argument list (capture group 1 of the container regex) within the file, so a
/// fix can rewrite exactly that region.
pub struct ClassGroup {
    pub file: PathBuf,
    pub line: u32,
    pub list_span: Range<usize>,
    pub classes: Vec<String>,
}

/// The compiled container + item regexes join mode extracts blocks with.
pub struct GroupMatcher {
    container: Regex,
    item: Regex,
}

impl GroupMatcher {
    pub fn from_config(config: &LintConfig) -> Result<Self> {
        let container_source = config
            .class_regexes
            .iter()
            .find_map(|regex| match regex {
                ClassRegex::Container { container, class } => Some((container, class)),
                ClassRegex::Simple(_) => None,
            })
            .ok_or_else(|| anyhow!("--join requires --class-container with a --class-regex"))?;
        let container = Regex::new(container_source.0)
            .map_err(|error| anyhow!("invalid --class-container regex: {error}"))?;
        let item = Regex::new(container_source.1)
            .map_err(|error| anyhow!("invalid --class-regex: {error}"))?;
        Ok(Self { container, item })
    }

    pub fn extract(&self, file: &Path, text: &str) -> Vec<ClassGroup> {
        let mut groups = Vec::new();
        for captures in self.container.captures_iter(text) {
            let list = match captures.get(1) {
                Some(list) => list,
                None => continue,
            };
            let inner = list.as_str();
            let classes: Vec<String> = self
                .item
                .captures_iter(inner)
                .filter_map(|capture| capture.get(1).map(|item| item.as_str().to_string()))
                .collect();
            if classes.is_empty() {
                continue;
            }
            let line = text[..list.start()]
                .bytes()
                .filter(|&byte| byte == b'\n')
                .count()
                + 1;
            let group = ClassGroup {
                file: file.to_path_buf(),
                line: u32::try_from(line).unwrap_or(u32::MAX),
                list_span: list.start()..list.end(),
                classes,
            };
            groups.push(group);
        }
        groups
    }
}
