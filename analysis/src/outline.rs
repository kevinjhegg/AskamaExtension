//! A template's structure: blocks, loops, conditions and the other tags that open a region,
//! paired with the tags that close them. Outline, folding and selection ranges all come from
//! it.
//!
//! Pairing is forgiving, so a template being edited still has a structure: a closing tag
//! with no matching opener is ignored, a closing tag closes any regions left open inside the
//! one it matches, and a region never closed runs to the end of the text.

use std::ops::Range;

use crate::tags::{Tag, TagKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionKind {
    Block,
    For,
    If,
    Match,
    Macro,
    Call,
    Filter,
    /// `let` or `set` in its block form, `{% let x %}…{% endlet %}`.
    Let,
    Raw,
    /// `extends`, a single tag.
    Extends,
    /// `import`, a single tag.
    Import,
    /// `include`, a single tag.
    Include,
}

impl RegionKind {
    fn opened_by(keyword: &str, rest: &str) -> Option<Self> {
        Some(match keyword {
            "block" => Self::Block,
            "for" => Self::For,
            "if" => Self::If,
            "match" => Self::Match,
            "macro" => Self::Macro,
            "call" => Self::Call,
            "filter" => Self::Filter,
            "raw" => Self::Raw,
            "let" | "set" if !has_assignment(rest) => Self::Let,
            _ => return None,
        })
    }

    /// The tag that closes a region of this kind, or `None` for a single tag.
    pub fn closer(self) -> Option<&'static str> {
        Some(match self {
            Self::Block => "endblock",
            Self::For => "endfor",
            Self::If => "endif",
            Self::Match => "endmatch",
            Self::Macro => "endmacro",
            Self::Call => "endcall",
            Self::Filter => "endfilter",
            Self::Let => "endlet",
            Self::Raw => "endraw",
            Self::Extends | Self::Import | Self::Include => return None,
        })
    }

    fn closed_by(keyword: &str) -> Option<Self> {
        Some(match keyword {
            "endblock" => Self::Block,
            "endfor" => Self::For,
            "endif" => Self::If,
            "endmatch" => Self::Match,
            "endmacro" => Self::Macro,
            "endcall" => Self::Call,
            "endfilter" => Self::Filter,
            "endraw" => Self::Raw,
            "endlet" | "endset" => Self::Let,
            _ => return None,
        })
    }

    fn single(keyword: &str) -> Option<Self> {
        Some(match keyword {
            "extends" => Self::Extends,
            "import" => Self::Import,
            "include" => Self::Include,
            _ => return None,
        })
    }

    /// Whether a tag with this keyword starts a new branch of the region: `else` and `elif`
    /// in an `if`, `else` in a `for`, `when` and `else` in a `match`.
    fn branches_at(self, keyword: &str) -> bool {
        matches!(
            (self, keyword),
            (Self::If, "else" | "elif") | (Self::For, "else") | (Self::Match, "when" | "else")
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Region {
    pub kind: RegionKind,
    /// The opening tag's text with its whitespace collapsed, such as `for item in order.items`.
    pub label: String,
    /// From the start of the opening tag to the end of the closing one.
    pub range: Range<usize>,
    /// The opening tag.
    pub opener: Range<usize>,
    /// The tags that start each later branch (`else`, `elif`, `when`), in order.
    pub branches: Vec<Range<usize>>,
    /// The closing tag, if there is one.
    pub closer: Option<Range<usize>>,
    pub children: Vec<Region>,
}

impl Region {
    /// Every region in a tree, depth first.
    pub fn walk<'r>(regions: &'r [Region]) -> Box<dyn Iterator<Item = &'r Region> + 'r> {
        Box::new(regions.iter().flat_map(|region| std::iter::once(region).chain(Self::walk(&region.children))))
    }
}

/// The regions of a template, as a tree in document order.
pub fn regions(source: &str, tags: &[Tag]) -> Vec<Region> {
    let mut roots = Vec::new();
    let mut open: Vec<Region> = Vec::new();

    for tag in tags.iter().filter(|tag| tag.kind == TagKind::Block) {
        let keyword = tag.keyword(source);
        let rest = tag.rest(source);
        if let Some(kind) = RegionKind::opened_by(keyword, rest) {
            open.push(Region {
                kind,
                label: label(source, tag),
                range: tag.range.clone(),
                opener: tag.range.clone(),
                branches: Vec::new(),
                closer: None,
                children: Vec::new(),
            });
        } else if let Some(kind) = RegionKind::closed_by(keyword) {
            let Some(index) = open.iter().rposition(|region| region.kind == kind) else {
                continue;
            };
            while open.len() > index + 1 {
                let mut unclosed = open.pop().unwrap_or_else(|| unreachable!());
                unclosed.range.end = tag.range.start;
                attach(&mut open, &mut roots, unclosed);
            }
            if let Some(mut region) = open.pop() {
                region.range.end = tag.range.end;
                region.closer = Some(tag.range.clone());
                attach(&mut open, &mut roots, region);
            }
        } else if let Some(kind) = RegionKind::single(keyword) {
            let region = Region {
                kind,
                label: label(source, tag),
                range: tag.range.clone(),
                opener: tag.range.clone(),
                branches: Vec::new(),
                closer: None,
                children: Vec::new(),
            };
            attach(&mut open, &mut roots, region);
        } else if let Some(region) = open.last_mut()
            && region.kind.branches_at(keyword)
        {
            region.branches.push(tag.range.clone());
        }
    }

    while let Some(mut unclosed) = open.pop() {
        unclosed.range.end = source.len();
        attach(&mut open, &mut roots, unclosed);
    }
    roots
}

fn attach(open: &mut [Region], roots: &mut Vec<Region>, region: Region) {
    match open.last_mut() {
        Some(parent) => parent.children.push(region),
        None => roots.push(region),
    }
}

/// A tag's text with whitespace collapsed, cut at 80 characters.
fn label(source: &str, tag: &Tag) -> String {
    const LIMIT: usize = 80;
    let words: Vec<&str> = source[tag.inner.clone()].split_whitespace().collect();
    let text = words.join(" ");
    if text.chars().count() > LIMIT {
        let cut: String = text.chars().take(LIMIT - 1).collect();
        format!("{cut}…")
    } else {
        text
    }
}

/// Whether `let` or `set` assigns a value (`= …`), which makes it a single tag rather than
/// the opening of a block. Comparison operators do not count.
fn has_assignment(rest: &str) -> bool {
    let bytes = rest.as_bytes();
    bytes.iter().enumerate().any(|(at, &byte)| {
        byte == b'='
            && !matches!(at.checked_sub(1).map(|before| bytes[before]), Some(b'=' | b'!' | b'<' | b'>'))
            && bytes.get(at + 1) != Some(&b'=')
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_syntax;
    use crate::tags::scan;

    fn tree(source: &str) -> Vec<Region> {
        regions(source, &scan(source, &default_syntax()))
    }

    /// Each region as an indented line: its label and the text it spans.
    fn sketch(source: &str) -> Vec<String> {
        fn walk(source: &str, regions: &[Region], depth: usize, out: &mut Vec<String>) {
            for region in regions {
                out.push(format!("{}{} [{}]", "  ".repeat(depth), region.label, &source[region.range.clone()]));
                walk(source, &region.children, depth + 1, out);
            }
        }
        let mut out = Vec::new();
        walk(source, &tree(source), 0, &mut out);
        out
    }

    #[test]
    fn nests_regions() {
        let source = "{% block a %}{% for x in xs %}{% if x %}y{% endif %}{% endfor %}{% endblock %}";
        assert_eq!(
            sketch(source),
            [
                format!("block a [{source}]"),
                "  for x in xs [{% for x in xs %}{% if x %}y{% endif %}{% endfor %}]".to_owned(),
                "    if x [{% if x %}y{% endif %}]".to_owned(),
            ]
        );
    }

    #[test]
    fn lists_single_tags_in_place() {
        let source = "{% extends \"base.html\" %}{% import \"m.html\" as m %}{% block b %}{% include \"p.html\" %}{% endblock %}";
        let regions = tree(source);
        let kinds: Vec<RegionKind> = regions.iter().map(|region| region.kind).collect();
        assert_eq!(kinds, [RegionKind::Extends, RegionKind::Import, RegionKind::Block]);
        assert_eq!(regions[2].children[0].kind, RegionKind::Include);
        assert_eq!(regions[0].label, "extends \"base.html\"");
    }

    #[test]
    fn records_branches() {
        let source = "{% if a %}1{% elif b %}2{% else %}3{% endif %}{% match m %}{% when Some(x) %}{% when None %}{% endmatch %}";
        let regions = tree(source);
        let branches: Vec<&str> = regions[0].branches.iter().map(|range| &source[range.clone()]).collect();
        assert_eq!(branches, ["{% elif b %}", "{% else %}"]);
        assert_eq!(regions[1].branches.len(), 2);
    }

    #[test]
    fn tells_block_let_from_assignment() {
        let source = "{% let a = 1 %}{% let b %}x{% endlet %}";
        let regions = tree(source);
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].label, "let b");
        assert!(has_assignment("x = a == b"));
        assert!(!has_assignment("x: Option<u8>"));
        assert!(!has_assignment("a == b && c != d && e <= f && g >= h"));
    }

    #[test]
    fn survives_unbalanced_tags() {
        // An `endfor` closes the `if` left open inside it, a stray `endmacro` is ignored, and
        // the unclosed `block` runs to the end.
        let source = "{% block a %}{% for x in xs %}{% if x %}{% endfor %}{% endmacro %}tail";
        let regions = tree(source);
        assert_eq!(regions[0].range, 0..source.len());
        assert_eq!(regions[0].closer, None);
        let for_loop = &regions[0].children[0];
        assert!(for_loop.closer.is_some());
        assert_eq!(&source[for_loop.children[0].range.clone()], "{% if x %}");
    }

    #[test]
    fn outlines_the_fixture() {
        let path = format!("{}/../fixtures/sample-site/templates/order.html", env!("CARGO_MANIFEST_DIR"));
        let source = std::fs::read_to_string(path).unwrap();
        let labels: Vec<String> = Region::walk(&tree(&source)).map(|region| region.label.clone()).collect();
        assert_eq!(
            labels,
            [
                "extends \"base.html\"",
                "import \"macros.html\" as m",
                "block title",
                "block content",
                "for item in order.items",
                "call m::money_cell(item.total_cents())",
            ]
        );
    }

    #[test]
    fn shortens_long_labels() {
        let condition = "x".repeat(100);
        let source = format!("{{% if {condition} %}}{{% endif %}}");
        let label = &tree(&source)[0].label;
        assert_eq!(label.chars().count(), 80);
        assert!(label.ends_with('…'));
    }
}
