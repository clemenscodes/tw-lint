use lsp_types::TextEdit;

/// Apply LSP text edits to a document. Edits are sorted by start position
/// descending so applying one never shifts the offsets of another. All edits
/// are computed against `source` (the original document), so they must be
/// applied together in one pass rather than re-read between edits.
pub fn apply_text_edits(source: &str, edits: &[TextEdit]) -> String {
    let line_starts = line_start_offsets(source);
    let mut ordered: Vec<&TextEdit> = edits.iter().collect();
    ordered.sort_by(|a, b| {
        (b.range.start.line, b.range.start.character)
            .cmp(&(a.range.start.line, a.range.start.character))
    });

    let mut buffer = source.to_string();
    for edit in ordered {
        let start = offset_of(
            &line_starts,
            edit.range.start.line,
            edit.range.start.character,
        );
        let end = offset_of(&line_starts, edit.range.end.line, edit.range.end.character);
        buffer.replace_range(start..end, &edit.new_text);
    }
    buffer
}

fn line_start_offsets(source: &str) -> Vec<usize> {
    let mut offsets = vec![0usize];
    for (index, byte) in source.bytes().enumerate() {
        if byte == b'\n' {
            offsets.push(index + 1);
        }
    }
    offsets
}

fn offset_of(line_starts: &[usize], line: u32, character: u32) -> usize {
    let line_start = line_starts[line as usize];
    // LSP characters are UTF-16 code units; class names here are ASCII so this
    // byte-based mapping is exact. Replace with a UTF-16-aware walk if
    // non-ASCII ever appears inside a class candidate.
    line_start + character as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{Position, Range, TextEdit};

    fn edit(l1: u32, c1: u32, l2: u32, c2: u32, text: &str) -> TextEdit {
        TextEdit {
            range: Range::new(Position::new(l1, c1), Position::new(l2, c2)),
            new_text: text.to_string(),
        }
    }

    #[test]
    fn applies_two_edits_on_one_line_without_offset_drift() {
        let source = "let x = tw![\"w-[100%]\", \"h-[100%]\"];\n";
        // w-[100%] spans cols 13..21, h-[100%] spans cols 25..33.
        let edits = vec![edit(0, 13, 0, 21, "w-full"), edit(0, 25, 0, 33, "h-full")];
        let result = apply_text_edits(source, &edits);
        assert_eq!(result, "let x = tw![\"w-full\", \"h-full\"];\n");
    }
}
