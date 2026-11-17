//! What the analysis knows from a crate's Rust code, read with `syn`: its custom filters, and
//! the structs that derive `Template`, with the template each names and its fields and
//! methods.
//!
//! Only the syntax is read, so field types are as written and nothing is resolved. Each file
//! is reread only when its modification time changes. Files are read from disk, so a change
//! counts once it is saved.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use quote::ToTokens;
use syn::spanned::Spanned;

use crate::filters::{CrateFilters, CustomFilter};
use crate::lines::{LineIndex, Position};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberKind {
    Field,
    Method,
}

/// A field of a template struct, or a method of one with a `self` receiver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub name: String,
    pub kind: MemberKind,
    /// A field's type, or a method's signature, as written with whitespace collapsed.
    pub detail: String,
    pub docs: String,
    pub file: PathBuf,
    /// The name's range in `file`.
    pub range: Range<usize>,
}

/// A struct that derives `Template` from a template file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateStruct {
    pub name: String,
    /// The template's name from `#[template(path = "...")]`.
    pub template: String,
    /// The range of that name, inside the quotes, in `file`.
    pub template_range: Range<usize>,
    pub file: PathBuf,
    /// The struct name's range in `file`.
    pub range: Range<usize>,
    /// Its fields, then the methods of its inherent `impl` blocks anywhere in the crate.
    pub members: Vec<Member>,
    /// An `impl` header for it with its generics, such as `impl<'a> Page<'a>`, for a probe.
    pub impl_header: String,
    /// Where the struct's item ends in `file`: a probe goes right after it, in its module.
    pub item_end: usize,
}

impl TemplateStruct {
    pub fn member(&self, name: &str) -> Option<&Member> {
        self.members.iter().find(|member| member.name == name)
    }
}

/// Everything read from one crate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CrateFacts {
    pub filters: CrateFilters,
    pub structs: Vec<TemplateStruct>,
}

#[derive(Debug, Default)]
struct FileFacts {
    filters: CrateFilters,
    structs: Vec<TemplateStruct>,
    /// Methods with the name of the type their `impl` block is for.
    methods: Vec<(String, Member)>,
}

/// The facts of each Rust file read so far, by path.
#[derive(Debug, Default)]
pub struct RustIndex {
    files: HashMap<PathBuf, (Option<SystemTime>, Arc<FileFacts>)>,
}

impl RustIndex {
    /// The facts of every Rust file under the crate's `src`.
    ///
    /// A custom filter is any function inside a module named `filters`, inline
    /// (`mod filters { … }`) or in its own file (`filters.rs`, `filters/mod.rs`). Askama looks
    /// filters up in the `filters` module in scope of the template's struct; this reading does
    /// not check which struct can see which module.
    pub fn crate_facts(&mut self, crate_root: &Path) -> CrateFacts {
        let files = rust_files(&crate_root.join("src"));
        self.files.retain(|path, _| !path.starts_with(crate_root) || files.contains(path));
        let mut facts = CrateFacts::default();
        let mut methods = Vec::new();
        for file in files {
            let modified = std::fs::metadata(&file).and_then(|metadata| metadata.modified()).ok();
            let cached = self.files.get(&file).filter(|(when, _)| *when == modified && modified.is_some());
            let read = match cached {
                Some((_, read)) => Arc::clone(read),
                None => {
                    let read = Arc::new(read_file(&file));
                    self.files.insert(file.clone(), (modified, Arc::clone(&read)));
                    read
                }
            };
            facts.filters.filters.extend(read.filters.filters.iter().cloned());
            facts.filters.incomplete |= read.filters.incomplete;
            facts.structs.extend(read.structs.iter().cloned());
            methods.extend(read.methods.iter().cloned());
        }
        for (owner, method) in methods {
            for template_struct in facts.structs.iter_mut().filter(|template_struct| template_struct.name == owner) {
                template_struct.members.push(method.clone());
            }
        }
        facts
    }
}

fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return files;
    };
    let mut entries: Vec<PathBuf> = entries.filter_map(|entry| entry.ok().map(|entry| entry.path())).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            files.extend(rust_files(&path));
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
    files
}

fn read_file(file: &Path) -> FileFacts {
    let Ok(source) = std::fs::read_to_string(file) else {
        return FileFacts::default();
    };
    let Ok(parsed) = syn::parse_file(&source) else {
        return FileFacts::default();
    };
    let lines = LineIndex::new(&source);
    let whole_file_is_filters = file.file_stem().is_some_and(|stem| stem == "filters")
        || (file.file_stem().is_some_and(|stem| stem == "mod") && file.parent().and_then(Path::file_name).is_some_and(|dir| dir == "filters"));
    let mut facts = FileFacts::default();
    collect(&parsed.items, whole_file_is_filters, &Reader { file, source: &source, lines: &lines }, &mut facts);
    facts
}

struct Reader<'a> {
    file: &'a Path,
    source: &'a str,
    lines: &'a LineIndex,
}

impl Reader<'_> {
    /// The byte offset of a span's start or end. `proc-macro2`'s columns count characters.
    fn offset(&self, location: proc_macro2::LineColumn) -> usize {
        let line = u32::try_from(location.line.saturating_sub(1)).unwrap_or(u32::MAX);
        let line_start = self.lines.offset(self.source, Position { line, character: 0 });
        self.source[line_start..].char_indices().nth(location.column).map_or(self.source.len(), |(at, _)| line_start + at)
    }

    fn range(&self, span: proc_macro2::Span) -> Range<usize> {
        self.offset(span.start())..self.offset(span.end())
    }

    /// The source text of a span, with whitespace collapsed.
    fn text(&self, span: proc_macro2::Span) -> String {
        self.source[self.range(span)].split_whitespace().collect::<Vec<_>>().join(" ")
    }
}

fn collect(items: &[syn::Item], in_filters: bool, reader: &Reader<'_>, facts: &mut FileFacts) {
    for item in items {
        match item {
            syn::Item::Fn(function) if in_filters => facts.filters.filters.push(custom_filter(function, reader)),
            syn::Item::Use(_) if in_filters => facts.filters.incomplete = true,
            syn::Item::Mod(module) => {
                if let Some((_, items)) = &module.content {
                    collect(items, module.ident == "filters", reader, facts);
                }
            }
            syn::Item::Struct(item) => {
                if let Some(template_struct) = template_struct(item, reader) {
                    facts.structs.push(template_struct);
                }
            }
            syn::Item::Impl(item) if item.trait_.is_none() => {
                if let syn::Type::Path(owner) = &*item.self_ty
                    && let Some(owner) = owner.path.segments.last()
                {
                    for method in &item.items {
                        if let syn::ImplItem::Fn(method) = method
                            && method.sig.receiver().is_some()
                        {
                            let member = Member {
                                name: method.sig.ident.to_string(),
                                kind: MemberKind::Method,
                                detail: reader.text(method.sig.span()),
                                docs: docs(&method.attrs),
                                file: reader.file.to_path_buf(),
                                range: reader.range(method.sig.ident.span()),
                            };
                            facts.methods.push((owner.ident.to_string(), member));
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

fn custom_filter(function: &syn::ItemFn, reader: &Reader<'_>) -> CustomFilter {
    CustomFilter {
        name: function.sig.ident.to_string(),
        signature: reader.text(function.sig.span()),
        docs: docs(&function.attrs),
        file: reader.file.to_path_buf(),
        range: reader.range(function.sig.ident.span()),
    }
}

/// A struct that derives `Template` and names a template file. One with an inline
/// `source = "..."` template, or none, is not one.
fn template_struct(item: &syn::ItemStruct, reader: &Reader<'_>) -> Option<TemplateStruct> {
    let derives_template = item.attrs.iter().filter(|attribute| attribute.path().is_ident("derive")).any(|attribute| {
        attribute
            .parse_args_with(syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated)
            .is_ok_and(|paths| paths.iter().any(|path| path.segments.last().is_some_and(|segment| segment.ident == "Template")))
    });
    if !derives_template {
        return None;
    }
    let mut template = None;
    for attribute in item.attrs.iter().filter(|attribute| attribute.path().is_ident("template")) {
        let _ = attribute.parse_nested_meta(|meta| {
            if meta.path.is_ident("path") {
                let path: syn::LitStr = meta.value()?.parse()?;
                let quoted = reader.range(path.span());
                template = Some((path.value(), quoted.start + 1..quoted.end.saturating_sub(1)));
            } else if meta.input.peek(syn::Token![=]) {
                let _: syn::Expr = meta.value()?.parse()?;
            }
            Ok(())
        });
    }
    let (template, template_range) = template?;
    let members = match &item.fields {
        syn::Fields::Named(fields) => fields
            .named
            .iter()
            .filter_map(|field| {
                let name = field.ident.as_ref()?;
                Some(Member {
                    name: name.to_string(),
                    kind: MemberKind::Field,
                    detail: reader.text(field.ty.span()),
                    docs: docs(&field.attrs),
                    file: reader.file.to_path_buf(),
                    range: reader.range(name.span()),
                })
            })
            .collect(),
        syn::Fields::Unnamed(_) | syn::Fields::Unit => Vec::new(),
    };
    let (impl_generics, type_generics, where_clause) = item.generics.split_for_impl();
    let impl_header = format!(
        "impl{} {}{} {}",
        impl_generics.to_token_stream(),
        item.ident,
        type_generics.to_token_stream(),
        where_clause.to_token_stream()
    );
    Some(TemplateStruct {
        name: item.ident.to_string(),
        template,
        template_range,
        file: reader.file.to_path_buf(),
        range: reader.range(item.ident.span()),
        members,
        impl_header: impl_header.trim_end().to_owned(),
        item_end: reader.offset(item.span().end()),
    })
}

/// The text of `///` doc comments, one line each.
fn docs(attributes: &[syn::Attribute]) -> String {
    attributes
        .iter()
        .filter(|attribute| attribute.path().is_ident("doc"))
        .filter_map(|attribute| match &attribute.meta {
            syn::Meta::NameValue(syn::MetaNameValue { value: syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(text), .. }), .. }) => Some(text.value()),
            _ => None,
        })
        .map(|line| line.strip_prefix(' ').map(str::to_owned).unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    const MAIN: &str = r#"use askama::Template;

#[derive(Template, Debug)]
#[template(path = "order.html", escape = "html")]
pub struct OrderPage<'a> {
    /// The page title.
    pub title: &'a str,
    pub items: Vec<
        Item
    >,
}

impl OrderPage<'_> {
    /// How many items.
    pub fn count(&self) -> usize { self.items.len() }
    pub fn new() -> Self { todo!() }
}

#[derive(askama::Template)]
#[template(source = "{{ x }}", ext = "txt")]
struct Inline { x: u8 }

#[derive(Debug)]
struct NotATemplate { y: u8 }
"#;

    #[test]
    fn reads_template_structs_with_fields_and_methods() {
        let dir = TempDir::new();
        dir.write("src/main.rs", MAIN);
        dir.write("src/more.rs", "impl crate::OrderPage<'_> { fn label(&self) -> String { String::new() } }");
        let facts = RustIndex::default().crate_facts(&dir.path(""));
        assert_eq!(facts.structs.len(), 1);
        let page = &facts.structs[0];
        assert_eq!((page.name.as_str(), page.template.as_str()), ("OrderPage", "order.html"));
        assert_eq!(&MAIN[page.template_range.clone()], "order.html");
        assert_eq!(&MAIN[page.range.clone()], "OrderPage");

        let members: Vec<(&str, MemberKind, &str, &str)> =
            page.members.iter().map(|member| (member.name.as_str(), member.kind, member.detail.as_str(), member.docs.as_str())).collect();
        assert_eq!(
            members,
            [
                ("title", MemberKind::Field, "&'a str", "The page title."),
                ("items", MemberKind::Field, "Vec< Item >", ""),
                ("count", MemberKind::Method, "fn count(&self) -> usize", "How many items."),
                ("label", MemberKind::Method, "fn label(&self) -> String", ""),
            ]
        );
        assert_eq!(&MAIN[page.member("title").unwrap().range.clone()], "title");
        assert_eq!(page.impl_header, "impl< 'a > OrderPage< 'a >");
        assert!(MAIN[..page.item_end].ends_with(">,\n}"));
    }

    #[test]
    fn reads_the_fixture() {
        let root = PathBuf::from(format!("{}/../fixtures/sample-site", env!("CARGO_MANIFEST_DIR")));
        let facts = RustIndex::default().crate_facts(&root);
        let page = &facts.structs[0];
        assert_eq!((page.name.as_str(), page.template.as_str()), ("OrderPage", "order.html"));
        let names: Vec<&str> = page.members.iter().map(|member| member.name.as_str()).collect();
        assert_eq!(names, ["title", "order"]);
        assert!(facts.filters.find("dollars").is_some());
    }
}
