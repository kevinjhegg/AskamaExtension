//! Folding ranges and selection ranges, from a template's tags and regions.

use std::ops::Range;

use crate::lines::LineIndex;
use crate::outline::Region;
use crate::tags::{Tag, TagKind};

/// What a fold folds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoldKind {
    /// A branch of a region.
    Region,
    /// A comment.
    Comment,
}

/// A foldable span of lines: `start_line` stays visible, the lines after it up to and including `end_line` fold away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fold {
    /// The line that stays visible.
    pub start_line: u32,
    /// The last line that folds away.
    pub end_line: u32,
    /// What it folds.
    pub kind: FoldKind,
}

/// One fold per branch of every region that spans lines, ending on the line before the next branch or the closing tag so that tag stays
/// visible, and one per multi-line comment.
pub fn folds(source: &str, lines: &LineIndex, tags: &[Tag], regions: &[Region]) -> Vec<Fold> {
    let line = |offset: usize| lines.position(source, offset).line;
    let mut folds = Vec::new();

    for region in Region::walk(regions) {
        let ends = region.branches.iter().map(|branch| branch.start).chain(region.closer.as_ref().map(|closer| closer.start));
        let starts = std::iter::once(region.opener.start).chain(region.branches.iter().map(|branch| branch.start));
        for (start, end) in starts.zip(ends.map(Some).chain(std::iter::once(None))) {
            let start_line = line(start);
            let end_line = match end {
                Some(end) => line(end).saturating_sub(1),
                None => line(region.range.end),
            };
            if end_line > start_line {
                folds.push(Fold { start_line, end_line, kind: FoldKind::Region });
            }
        }
    }

    for tag in tags.iter().filter(|tag| tag.kind == TagKind::Comment) {
        let (start_line, end_line) = (line(tag.range.start), line(tag.range.end.saturating_sub(1)));
        if end_line > start_line {
            folds.push(Fold { start_line, end_line, kind: FoldKind::Comment });
        }
    }
    folds
}

/// The ranges **Expand Selection** steps through from `offset`, smallest first: the word, the tag's content, the whole tag, then for each
/// enclosing region its body and the region itself, and last the whole text. Each range strictly contains the one before it.
pub fn selection(source: &str, tags: &[Tag], regions: &[Region], offset: usize) -> Vec<Range<usize>> {
    let mut chain = Vec::new();
    let mut push = |range: Range<usize>| {
        let grows = chain.last().is_none_or(|last: &Range<usize>| {
            range.start <= last.start && last.end <= range.end && range != *last
        });
        if grows && !range.is_empty() {
            chain.push(range);
        }
    };

    push(word(source, offset));
    if let Some(tag) = tags.iter().find(|tag| tag.range.start < offset && offset < tag.range.end) {
        push(tag.inner.clone());
        push(tag.range.clone());
    }
    let mut enclosing: Vec<&Region> = Region::walk(regions)
        .filter(|region| region.range.start <= offset && offset <= region.range.end)
        .collect();
    enclosing.sort_by_key(|region| region.range.len());
    for region in enclosing {
        if let Some(closer) = &region.closer {
            push(region.opener.end..closer.start);
        }
        push(region.range.clone());
    }
    push(0..source.len());
    chain
}

/// The identifier around `offset`, or an empty range at `offset` if there is none.
fn word(source: &str, offset: usize) -> Range<usize> {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let offset = offset.min(source.len());
    if !source.is_char_boundary(offset) {
        return offset..offset;
    }
    let start = source[..offset].rfind(|c: char| !is_word(c)).map_or(0, |at| crate::tags::next_char(source, at));
    let end = source[offset..].find(|c: char| !is_word(c)).map_or(source.len(), |at| offset + at);
    start..end
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_syntax;
    use crate::outline::regions;
    use crate::tags::scan;

    fn folds_of(source: &str) -> Vec<(u32, u32, FoldKind)> {
        let tags = scan(source, &default_syntax());
        let lines = LineIndex::new(source);
        folds(source, &lines, &tags, &regions(source, &tags))
            .into_iter()
            .map(|fold| (fold.start_line, fold.end_line, fold.kind))
            .collect()
    }

    #[test]
    fn folds_regions_up_to_their_closing_tag() {
        let source = "{% for x in xs %}\n  a\n  b\n{% endfor %}\n";
        assert_eq!(folds_of(source), [(0, 2, FoldKind::Region)]);
    }

    #[test]
    fn folds_each_branch() {
        let source = "{% if a %}\n  1\n{% else %}\n  2\n{% endif %}";
        assert_eq!(folds_of(source), [(0, 1, FoldKind::Region), (2, 3, FoldKind::Region)]);
    }

    #[test]
    fn folds_multi_line_comments_only() {
        let source = "{# one #}\n{# two\nlines\n#}";
        assert_eq!(folds_of(source), [(1, 3, FoldKind::Comment)]);
    }

    #[test]
    fn folds_an_unclosed_region_to_the_end() {
        assert_eq!(folds_of("{% block a %}\nx\ny"), [(0, 2, FoldKind::Region)]);
    }

    #[test]
    fn selection_grows_from_word_to_document() {
        let source = "<p>\n{% for x in xs %}\n  {{ x.name }}\n{% endfor %}\n</p>";
        let tags = scan(source, &default_syntax());
        let regions = regions(source, &tags);
        let offset = source.find("name").unwrap() + 1;
        let chain: Vec<&str> = selection(source, &tags, &regions, offset).into_iter().map(|range| &source[range]).collect();
        assert_eq!(
            chain,
            [
                "name",
                "x.name",
                "{{ x.name }}",
                "\n  {{ x.name }}\n",
                "{% for x in xs %}\n  {{ x.name }}\n{% endfor %}",
                source,
            ]
        );
    }

    #[test]
    fn word_finds_identifiers() {
        assert_eq!(word("ab.cd", 1), 0..2);
        assert_eq!(word("ab.cd", 4), 3..5);
        assert_eq!(word("é_x y", 2), 0..4);
        assert_eq!(word("a  b", 2), 2..2);
    }
}
