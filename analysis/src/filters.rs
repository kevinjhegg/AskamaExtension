//! Filters: Askama's built-in ones, the crate's own from its `filters` module, and the
//! filters a template uses.

use std::ops::Range;
use std::path::PathBuf;

use crate::tags::{Tag, TagKind, literal_end};

/// A filter Askama 0.16 provides, as a template writes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Builtin {
    pub name: &'static str,
    /// How a template calls it, such as `truncate(length)`.
    pub usage: &'static str,
    pub description: &'static str,
}

/// Askama 0.16's built-in filters: the names `askama_derive` 0.16 dispatches on, with
/// descriptions taken from the documentation of `askama::filters`.
pub const BUILTINS: &[Builtin] = &[
    Builtin { name: "assigned_or", usage: "assigned_or(fallback)", description: "Renders the value if it is not its type's default value, otherwise `fallback`." },
    Builtin { name: "capitalize", usage: "capitalize", description: "Capitalizes the value: the first character uppercase, all others lowercase." },
    Builtin { name: "center", usage: "center(width)", description: "Centers the value in a field of the given width." },
    Builtin { name: "default", usage: "default(fallback) or default(fallback, true)", description: "Renders `fallback` when the variable is not defined. With `true`, also when the value is its type's default value." },
    Builtin { name: "defined_or", usage: "defined_or(fallback)", description: "Renders the variable if it is defined, otherwise `fallback`." },
    Builtin { name: "deref", usage: "deref", description: "Dereferences the value, as `*value`." },
    Builtin { name: "e", usage: "e or e(escaper)", description: "Alias for `escape`." },
    Builtin { name: "escape", usage: "escape or escape(escaper)", description: "Escapes the value for the template's escape mode, or for the escaper named, such as `\"html\"` or `\"txt\"`." },
    Builtin { name: "filesizeformat", usage: "filesizeformat", description: "Formats a number of bytes in a readable unit (KB, MB, …)." },
    Builtin { name: "fmt", usage: "fmt(format)", description: "Formats the value with a format string, as `format!(format, value)`, such as `value|fmt(\"{:?}\")`." },
    Builtin { name: "format", usage: "format(args…)", description: "Formats the arguments with the value as the format string, such as `\"{} {}\"|format(a, b)`." },
    Builtin { name: "indent", usage: "indent(width) or indent(prefix)", description: "Indents every line after the first with spaces or a prefix." },
    Builtin { name: "join", usage: "join(separator)", description: "Joins the items of an iterable into a string, with the separator between them." },
    Builtin { name: "json", usage: "json or json(indent)", description: "Serializes the value to JSON, prettified when an indent is given. Needs Askama's `serde_json` feature." },
    Builtin { name: "linebreaks", usage: "linebreaks", description: "Replaces line breaks in plain text with HTML: `<br>` for a single one, `<p>` for a blank line." },
    Builtin { name: "linebreaksbr", usage: "linebreaksbr", description: "Converts every newline in plain text to `<br>`." },
    Builtin { name: "lower", usage: "lower", description: "Converts the value to lowercase." },
    Builtin { name: "lowercase", usage: "lowercase", description: "Alias for `lower`." },
    Builtin { name: "paragraphbreaks", usage: "paragraphbreaks", description: "Replaces only paragraph breaks (blank lines) in plain text with `<p>`." },
    Builtin { name: "pluralize", usage: "pluralize, pluralize(singular, plural)", description: "For a count of ±1 renders `singular` (by default `\"\"`), otherwise `plural` (by default `\"s\"`)." },
    Builtin { name: "ref", usage: "ref", description: "Takes a reference to the value, as `&value`." },
    Builtin { name: "reject", usage: "reject(value)", description: "Returns an iterator without the items equal to `value`." },
    Builtin { name: "safe", usage: "safe", description: "Marks the value as safe, so it is not escaped." },
    Builtin { name: "title", usage: "title", description: "Title-cases the value: each word starts uppercase, the rest lowercase." },
    Builtin { name: "titlecase", usage: "titlecase", description: "Alias for `title`." },
    Builtin { name: "tojson", usage: "tojson or tojson(indent)", description: "Alias for `json`." },
    Builtin { name: "trim", usage: "trim", description: "Strips leading and trailing whitespace." },
    Builtin { name: "truncate", usage: "truncate(length)", description: "Limits the value to `length` characters, appending `...` if it was cut." },
    Builtin { name: "unique", usage: "unique", description: "Returns an iterator with duplicate items removed." },
    Builtin { name: "upper", usage: "upper", description: "Converts the value to uppercase." },
    Builtin { name: "uppercase", usage: "uppercase", description: "Alias for `upper`." },
    Builtin { name: "urlencode", usage: "urlencode", description: "Percent-encodes the value for use in a URL. Does not encode `/`." },
    Builtin { name: "urlencode_strict", usage: "urlencode_strict", description: "Percent-encodes the value for use in a URL, `/` included." },
    Builtin { name: "value", usage: "value::<Type>", description: "Looks up a runtime value passed to `render_with_values`, the value being its key." },
    Builtin { name: "wordcount", usage: "wordcount", description: "Counts the words in the value." },
];

pub fn builtin(name: &str) -> Option<&'static Builtin> {
    BUILTINS.iter().find(|builtin| builtin.name == name)
}

/// A filter function in the crate's `filters` module, read by `crate::rust`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomFilter {
    pub name: String,
    /// The function's signature as written, from `fn` on, with whitespace collapsed.
    pub signature: String,
    pub docs: String,
    pub file: PathBuf,
    /// The function name's range in `file`.
    pub range: Range<usize>,
}

/// The custom filters of one crate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CrateFilters {
    pub filters: Vec<CustomFilter>,
    /// Whether a `filters` module brings in names this reading cannot follow (a `use`), so
    /// an unknown name may still be a real filter and must not be flagged.
    pub incomplete: bool,
}

impl CrateFilters {
    pub fn find(&self, name: &str) -> Option<&CustomFilter> {
        self.filters.iter().find(|filter| filter.name == name)
    }
}

/// A filter a template applies, `|name` or `{% filter name %}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterUse {
    pub name: String,
    pub range: Range<usize>,
    /// Whether the filter is named by a path (`module::name`), which Askama resolves as Rust
    /// does rather than in the `filters` module.
    pub qualified: bool,
}

/// Every filter applied in a template's expression and block tags.
pub fn filter_uses(source: &str, tags: &[Tag]) -> Vec<FilterUse> {
    let mut uses = Vec::new();
    for tag in tags.iter().filter(|tag| matches!(tag.kind, TagKind::Expr | TagKind::Block)) {
        let inner = tag.inner.clone();
        if tag.kind == TagKind::Block && tag.keyword(source) == "filter" {
            let start = inner.start + "filter".len();
            if let Some(found) = filter_name(source, start, inner.end) {
                uses.push(found);
            }
        }
        let bytes = source.as_bytes();
        let mut at = inner.start;
        while at < inner.end {
            if let Some(end) = literal_end(source, at) {
                at = end;
                continue;
            }
            if bytes[at] == b'|' {
                if bytes.get(at + 1) == Some(&b'|') {
                    at += 2;
                    continue;
                }
                if let Some(found) = filter_name(source, at + 1, inner.end) {
                    at = found.range.end;
                    uses.push(found);
                    continue;
                }
            }
            at += source[at..].chars().next().map_or(1, char::len_utf8);
        }
    }
    uses
}

/// The filter name (or path) starting at `from` after any whitespace.
fn filter_name(source: &str, from: usize, limit: usize) -> Option<FilterUse> {
    let text = source.get(from..limit)?;
    let start = from + (text.len() - text.trim_start().len());
    let rest = &source[start..limit];
    if !rest.starts_with(|c: char| c.is_alphabetic() || c == '_') {
        return None;
    }
    let length = rest.find(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':')).unwrap_or(rest.len());
    let full = rest[..length].trim_end_matches(':');
    let qualified = full.contains("::");
    // For a path the warning and navigation concern its last segment.
    let last = full.rsplit("::").next().unwrap_or(full);
    let name_start = start + full.len() - last.len();
    Some(FilterUse { name: last.to_owned(), range: name_start..name_start + last.len(), qualified })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::SystemTime;

    use crate::config::default_syntax;
    use crate::rust::RustIndex;
    use crate::tags::scan;
    use crate::testing::TempDir;

    fn crate_filters(index: &mut RustIndex, root: &Path) -> CrateFilters {
        index.crate_facts(root).filters
    }

    fn uses(source: &str) -> Vec<(String, &str, bool)> {
        filter_uses(source, &scan(source, &default_syntax()))
            .into_iter()
            .map(|found| (found.name, &source[found.range], found.qualified))
            .collect()
    }

    #[test]
    fn finds_filters_in_expressions_and_filter_blocks() {
        let source = "{{ name|upper }} {{ x | truncate(3)|safe }}{% filter lower|trim %}a{% endfilter %}";
        let names: Vec<String> = uses(source).into_iter().map(|found| found.0).collect();
        assert_eq!(names, ["upper", "truncate", "safe", "lower", "trim"]);
    }

    #[test]
    fn ignores_or_and_pipes_in_strings() {
        assert!(uses("{% if a || b %}{% endif %}{{ \"a|b\" }}{{ '|' }}").is_empty());
    }

    #[test]
    fn marks_qualified_filters() {
        assert_eq!(uses("{{ x|crate::text::shout }}"), [("shout".to_owned(), "shout", true)]);
    }

    #[test]
    fn knows_the_builtins() {
        assert!(builtin("truncate").is_some());
        assert!(builtin("dollars").is_none());
        let mut names: Vec<&str> = BUILTINS.iter().map(|builtin| builtin.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), BUILTINS.len(), "no filter is listed twice");
    }

    #[test]
    fn reads_custom_filters_from_inline_and_file_modules() {
        let dir = TempDir::new();
        dir.write(
            "src/main.rs",
            "mod filters {\n    /// Shouts.\n    /// Loudly.\n    #[askama::filter_fn]\n    pub fn shout(s: &str, _: &dyn askama::Values) -> askama::Result<String> {\n        Ok(s.to_uppercase())\n    }\n}\nfn main() {}\n",
        );
        dir.write("src/web/filters.rs", "pub fn whisper<T>(s: T) -> String where T: ToString { s.to_string() }\n");
        let filters = crate_filters(&mut RustIndex::default(), &dir.path(""));
        assert!(!filters.incomplete);
        let shout = filters.find("shout").unwrap();
        assert_eq!(shout.signature, "fn shout(s: &str, _: &dyn askama::Values) -> askama::Result<String>");
        assert_eq!(shout.docs, "Shouts.\nLoudly.");
        let source = std::fs::read_to_string(&shout.file).unwrap();
        assert_eq!(&source[shout.range.clone()], "shout");
        assert_eq!(filters.find("whisper").unwrap().signature, "fn whisper<T>(s: T) -> String where T: ToString");
    }

    #[test]
    fn notices_filters_it_cannot_follow() {
        let dir = TempDir::new();
        dir.write("src/lib.rs", "mod filters { pub use other::*; }");
        assert!(crate_filters(&mut RustIndex::default(), &dir.path("")).incomplete);
    }

    #[test]
    fn rereads_a_changed_file() {
        let dir = TempDir::new();
        dir.write("src/lib.rs", "mod filters { pub fn a() {} }");
        let mut index = RustIndex::default();
        assert!(crate_filters(&mut index, &dir.path("")).find("a").is_some());
        dir.write("src/lib.rs", "mod filters { pub fn b() {} }");
        // Make sure the modification time differs on file systems with coarse timestamps.
        let later = SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options().write(true).open(dir.path("src/lib.rs")).unwrap().set_modified(later).unwrap();
        let filters = crate_filters(&mut index, &dir.path(""));
        assert!(filters.find("a").is_none());
        assert!(filters.find("b").is_some());
    }

    #[test]
    fn reads_the_fixture_filter() {
        let root = PathBuf::from(format!("{}/../fixtures/sample-site", env!("CARGO_MANIFEST_DIR")));
        let filters = crate_filters(&mut RustIndex::default(), &root);
        let dollars = filters.find("dollars").unwrap();
        assert_eq!(dollars.docs, "Formats an amount in cents as dollars, for example `1250` as `$12.50`.");
    }
}
