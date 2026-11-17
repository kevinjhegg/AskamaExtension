//! Conversion between byte offsets, which Askama's parser reports, and line and column
//! positions, which editors use. Columns count UTF-16 code units, the LSP default encoding.

/// A position in a text: a zero-based line and a zero-based column in UTF-16 code units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

/// Where each line of a text starts, for converting offsets in either direction.
#[derive(Debug, Clone)]
pub struct LineIndex {
    line_starts: Vec<usize>,
}

impl LineIndex {
    pub fn new(text: &str) -> Self {
        let line_starts = std::iter::once(0)
            .chain(text.match_indices('\n').map(|(at, _)| at + 1))
            .collect();
        Self { line_starts }
    }

    /// The position of a byte offset. An offset past the end, or inside a character, is moved
    /// back to the nearest character boundary.
    pub fn position(&self, text: &str, offset: usize) -> Position {
        let offset = floor_char_boundary(text, offset);
        let line = self.line_starts.partition_point(|&start| start <= offset) - 1;
        let start = self.line_starts[line];
        let character = text[start..offset].encode_utf16().count();
        Position { line: to_u32(line), character: to_u32(character) }
    }

    /// The byte offset of a position. A line past the end gives the end of the text, and a
    /// column past the end of its line gives the end of that line.
    pub fn offset(&self, text: &str, position: Position) -> usize {
        let Some(&start) = self.line_starts.get(position.line as usize) else {
            return text.len();
        };
        let end = self
            .line_starts
            .get(position.line as usize + 1)
            .map_or(text.len(), |&next| next - 1);
        let mut units = 0;
        for (at, ch) in text[start..end].char_indices() {
            if units >= position.character as usize {
                return start + at;
            }
            units += ch.len_utf16();
        }
        end
    }
}

fn floor_char_boundary(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

fn to_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    #[test]
    fn positions_count_lines_and_columns() {
        let text = "ab\ncd\n\nef";
        let index = LineIndex::new(text);
        assert_eq!(index.position(text, 0), at(0, 0));
        assert_eq!(index.position(text, 2), at(0, 2));
        assert_eq!(index.position(text, 3), at(1, 0));
        assert_eq!(index.position(text, 6), at(2, 0));
        assert_eq!(index.position(text, 9), at(3, 2));
        assert_eq!(index.position(text, 99), at(3, 2));
    }

    #[test]
    fn columns_are_utf16_code_units() {
        // "é" is two bytes and one UTF-16 unit, "😀" four bytes and two units.
        let text = "é😀x";
        let index = LineIndex::new(text);
        assert_eq!(index.position(text, 2), at(0, 1));
        assert_eq!(index.position(text, 6), at(0, 3));
        // An offset inside the emoji moves back to its start.
        assert_eq!(index.position(text, 4), at(0, 1));
        assert_eq!(index.offset(text, at(0, 3)), 6);
    }

    #[test]
    fn offsets_round_trip() {
        let text = "{% if x %}\n  é {{ y }}\r\n{% endif %}";
        let index = LineIndex::new(text);
        for (offset, _) in text.char_indices() {
            assert_eq!(index.offset(text, index.position(text, offset)), offset, "offset {offset}");
        }
    }

    #[test]
    fn offsets_clamp_past_the_end() {
        let text = "ab\ncd";
        let index = LineIndex::new(text);
        assert_eq!(index.offset(text, at(0, 99)), 2);
        assert_eq!(index.offset(text, at(9, 0)), 5);
    }
}
